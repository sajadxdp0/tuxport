//! AF_UNIX sockets (the kernel/unix map): emulated entirely in process, never going through the
//! host.
//!
//! - A stream (`SOCK_STREAM`) or `SOCK_SEQPACKET` connection is a pair of one-way [`Channel`]s;
//!   each datagram socket has a receive queue, and senders put messages straight into the peer's
//!   queue.
//! - Addresses: a path address creates a socket node in the inode store and is looked up by inode;
//!   abstract addresses (`sun_path` starting with NUL) exist only in [`Names`].
//! - `SCM_RIGHTS` passes the open files themselves, installed into the receiving process's fd
//!   table on receipt. `SO_PEERCRED` gives the peer's pid and effective user ID and group ID;
//!   `SCM_CREDENTIALS` gives the sender's pid and real user ID and group ID, unless the sender
//!   declared others.
//! - Sockets that refer to each other only through in-flight `SCM_RIGHTS` messages form `Rc`
//!   cycles (open file → socket → receive queue → message → open file) that the program can no
//!   longer reach. As with Linux's unix_gc, such sockets are found after a unix socket is closed
//!   and after a process exits, and their receive queues are emptied ([`collect`]).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::{Rc, Weak};
use std::time::Duration;

use crate::cell::RefCell;
use crate::fd::{FileKind, FileRef, OpenFile};
use crate::kernel::{Kernel, request_wake_object, wake};
use crate::net::{AF_UNIX, SOCK_DGRAM, Socket};
use crate::vfs::store::Ino;

pub const SOCK_SEQPACKET: i32 = 5;

/// The most bytes buffered per direction, the same as Linux's default `SO_SNDBUF` (208 KiB).
pub const CAPACITY: usize = 212_992;

/// The default `net.unix.max_dgram_qlen`: datagrams a socket's queue holds before senders other
/// than its peer wait.
pub(crate) const MAX_DGRAM_QLEN: usize = 10;

/// The length of a `sockaddr_un`.
pub const SOCKADDR_UN_LEN: usize = 110;

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;
const POLLRDNORM: i16 = 0x040;
const POLLWRNORM: i16 = 0x100;
const POLLRDHUP: i16 = 0x2000;

/// `struct ucred`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ucred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

impl Ucred {
    /// The process with pid `pid` (global) and user ID and group ID `uid` and `gid` (kernel IDs:
    /// the effective ones for `SO_PEERCRED`, the real or declared ones for `SCM_CREDENTIALS`).
    pub fn of(pid: i32, uid: u32, gid: u32) -> Ucred {
        Ucred { pid, uid, gid }
    }

    pub fn encode(&self) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[..4].copy_from_slice(&self.pid.to_le_bytes());
        b[4..8].copy_from_slice(&self.uid.to_le_bytes());
        b[8..].copy_from_slice(&self.gid.to_le_bytes());
        b
    }
}

// ---- Sockets ----

/// Connection state.
pub(crate) enum Link {
    /// Not connected yet.
    None,
    /// Listening: the server-side sockets already connected and waiting for accept.
    Listening {
        backlog: usize,
        pending: VecDeque<Rc<RefCell<Socket>>>,
        /// The process that called listen; it is the `SO_PEERCRED` connecting sides see.
        cred: Ucred,
    },
    /// The two directions of a stream connection.
    Stream {
        rx: ChannelRef,
        tx: ChannelRef,
        peer_addr: Vec<u8>,
    },
    /// A datagram socket connected to the peer's receive queue.
    Dgram {
        tx: Weak<RefCell<Channel>>,
        peer_addr: Vec<u8>,
    },
}

/// An AF_UNIX socket.
pub(crate) struct UnixSocket {
    pub ty: i32,
    /// The bound address (`sockaddr_un` bytes), empty when unbound.
    pub addr: Vec<u8>,
    pub link: Link,
    /// The datagram socket's own receive queue.
    pub rx: ChannelRef,
    /// The peer's credentials (`SO_PEERCRED`): recorded at socketpair and stream connect.
    pub peer_cred: Option<Ucred>,
    /// `SO_PASSCRED`: every receive attaches the sender's credentials.
    pub passcred: bool,
    pub rcvtimeo: Option<Duration>,
    pub sndtimeo: Option<Duration>,
    /// The datagram socket did `SHUT_WR`.
    pub shut_wr: bool,
    /// The number of connections a listening socket has received (`EPOLLET` triggers on each).
    pub connections: u64,
    /// The server end of a connection not accepted yet (Linux's embryo): closing it with its
    /// listener resets the connection.
    pub embryo: bool,
    /// The new network namespace it was created in, if any (abstract addresses are kept apart by
    /// it, and interface ioctls look at its lo).
    pub net: Option<Rc<crate::nsproxy::NetNs>>,
    /// The generic `SOL_SOCKET` options it only records (buffer sizes among them: the capacity
    /// stays [`CAPACITY`]).
    pub opts: crate::net::SockOpts,
}

impl UnixSocket {
    /// A new socket; a datagram socket's queue takes Linux's default limit
    /// ([`UnixSocket::set_max_qlen`] applies the namespace's).
    pub fn new(ty: i32) -> UnixSocket {
        let rx = ChannelRef::default();
        if ty == SOCK_DGRAM {
            rx.borrow_mut().max_qlen = Some(MAX_DGRAM_QLEN);
        }
        UnixSocket {
            ty,
            addr: Vec::new(),
            link: Link::None,
            rx,
            peer_cred: None,
            passcred: false,
            rcvtimeo: None,
            sndtimeo: None,
            shut_wr: false,
            connections: 0,
            embryo: false,
            net: None,
            opts: crate::net::SockOpts::default(),
        }
    }

    /// A datagram socket's queue limit (`net.unix.max_dgram_qlen` of the namespace it is created
    /// in, as an unsigned number like `sk_max_ack_backlog`).
    pub fn set_max_qlen(&mut self, qlen: i64) {
        if self.ty == SOCK_DGRAM {
            self.rx.borrow_mut().max_qlen = Some(qlen as u32 as usize);
        }
    }

    /// Connects a datagram socket (`Link::Dgram`) or disconnects it (`Link::None`), noting the
    /// peer in its own queue for senders ([`Channel::holds_back`]).
    pub fn set_dgram_link(&mut self, link: Link) {
        self.rx.borrow_mut().peer = match &link {
            Link::Dgram { tx, .. } => tx.clone(),
            _ => Weak::new(),
        };
        self.link = link;
    }

    /// socketpair: two sockets connected to each other.
    pub fn pair(ty: i32, cred: Ucred) -> (UnixSocket, UnixSocket) {
        let mut a = UnixSocket::new(ty);
        let mut b = UnixSocket::new(ty);
        if ty == SOCK_DGRAM {
            a.set_dgram_link(Link::Dgram {
                tx: Rc::downgrade(&b.rx),
                peer_addr: unnamed(),
            });
            b.set_dgram_link(Link::Dgram {
                tx: Rc::downgrade(&a.rx),
                peer_addr: unnamed(),
            });
        } else {
            let (ab, ba) = (ChannelRef::default(), ChannelRef::default());
            a.link = Link::Stream {
                rx: ba.clone(),
                tx: ab.clone(),
                peer_addr: unnamed(),
            };
            b.link = Link::Stream {
                rx: ab,
                tx: ba,
                peer_addr: unnamed(),
            };
        }
        a.peer_cred = Some(cred);
        b.peer_cred = Some(cred);
        (a, b)
    }

    /// Stream connect to the listening socket with address `server_addr`: returns the server end,
    /// which the caller puts into the listening socket's wait queue.
    pub fn connect_stream(
        &mut self,
        server_addr: &[u8],
        server_cred: Ucred,
        cred: Ucred,
    ) -> UnixSocket {
        let (up, down) = (ChannelRef::default(), ChannelRef::default());
        let mut server = UnixSocket::new(self.ty);
        // As in Linux's unix_stream_connect, the new end is created in the connecting side's
        // network namespace
        server.net = self.net.clone();
        server.addr = server_addr.to_vec();
        server.embryo = true;
        server.link = Link::Stream {
            rx: up.clone(),
            tx: down.clone(),
            peer_addr: self.name(),
        };
        server.peer_cred = Some(cred);
        self.link = Link::Stream {
            rx: down,
            tx: up,
            peer_addr: server_addr.to_vec(),
        };
        self.peer_cred = Some(server_cred);
        server
    }

    /// FIONREAD: for streams and seqpacket connections the buffered bytes, for datagrams the length
    /// of the next datagram; None on a listener.
    pub fn unread(&self) -> Option<usize> {
        // unix_inq_len: none on a listener (EINVAL), every byte queued on a stream or
        // seqpacket connection, the next datagram's length
        match &self.link {
            Link::Listening { .. } => None,
            Link::Stream { rx, .. } => Some(rx.borrow().bytes),
            _ => Some(self.rx.borrow().packets.front().map_or(0, |p| p.data.len())),
        }
    }

    /// The events ppoll and epoll report, by the same rules as Linux's `unix_poll` and
    /// `unix_dgram_poll`.
    pub fn readiness(&self) -> i16 {
        match &self.link {
            Link::Listening { pending, .. } => {
                if pending.is_empty() {
                    0
                } else {
                    POLLIN | POLLRDNORM
                }
            }
            Link::Stream { rx, tx, .. } => {
                let (r, t) = (rx.borrow(), tx.borrow());
                let mut ev = 0;
                if !r.packets.is_empty() {
                    ev |= POLLIN | POLLRDNORM;
                }
                // The peer closed with data it had not read (unix_poll's sk_err)
                if r.reset {
                    ev |= POLLERR;
                }
                if r.eof {
                    ev |= POLLIN | POLLRDNORM | POLLRDHUP;
                    if t.eof || t.closed {
                        ev |= POLLHUP;
                    }
                }
                // Report writable also when the peer has closed, so the writer can get EPIPE
                if t.writable() || t.closed {
                    ev |= POLLOUT | POLLWRNORM;
                }
                ev
            }
            Link::None if self.ty != SOCK_DGRAM => POLLOUT | POLLWRNORM | POLLHUP,
            Link::None | Link::Dgram { .. } => {
                let r = self.rx.borrow();
                let mut ev = 0;
                if !r.packets.is_empty() {
                    ev |= POLLIN | POLLRDNORM;
                }
                if r.eof {
                    ev |= POLLIN | POLLRDNORM | POLLRDHUP;
                    // Both directions shut down (unix_dgram_poll's SHUTDOWN_MASK)
                    if self.shut_wr {
                        ev |= POLLHUP;
                    }
                }
                // unix_dgram_poll: also not writable while a peer that is not connected back to
                // this socket holds more datagrams than its limit
                let peer_full = match &self.link {
                    Link::Dgram { tx, .. } => tx.upgrade().is_some_and(|c| {
                        let c = c.borrow();
                        !c.writable() || c.holds_back(&Rc::downgrade(&self.rx))
                    }),
                    _ => false,
                };
                if !peer_full {
                    ev |= POLLOUT | POLLWRNORM;
                }
                ev
            }
        }
    }

    /// The wake objects poll and epoll register when waiting on it: its own receive queue
    /// (messages of a datagram socket; while listening, an incoming connection and accept taking
    /// one also request a wake of it), both directions of a stream connection, and for a connected
    /// datagram socket the peer's receive queue (whether it has room).
    pub fn wake_objects(&self, out: &mut Vec<wake::Object>) {
        out.push(wake::of(&self.rx));
        match &self.link {
            Link::Stream { rx, tx, .. } => {
                out.push(wake::of(rx));
                out.push(wake::of(tx));
            }
            Link::Dgram { tx, .. } => {
                if let Some(c) = tx.upgrade() {
                    out.push(wake::of(&c));
                }
            }
            Link::None | Link::Listening { .. } => {}
        }
    }

    /// How many events have arrived in each of the read and write directions (`syscall::poll`'s
    /// data generations).
    pub fn arrivals(&self) -> [u64; 2] {
        match &self.link {
            Link::Listening { .. } => [self.connections, 0],
            Link::Stream { rx, tx, .. } => [rx.borrow().arrivals, tx.borrow().taken],
            Link::Dgram { tx, .. } => [
                self.rx.borrow().arrivals,
                tx.upgrade().map_or(0, |c| c.borrow().taken),
            ],
            Link::None => [self.rx.borrow().arrivals, 0],
        }
    }

    /// shutdown(2): `how` is SHUT_RD(0), SHUT_WR(1) or SHUT_RDWR(2).
    pub fn shutdown(&mut self, how: i32) {
        let (rd, wr) = (how != 1, how != 0);
        match &self.link {
            Link::Stream { rx, tx, .. } => {
                if rd {
                    let mut r = rx.borrow_mut();
                    r.eof = true;
                    r.closed = true;
                }
                if wr {
                    tx.borrow_mut().eof = true;
                }
            }
            _ => {
                if rd {
                    self.rx.borrow_mut().eof = true;
                }
                self.shut_wr |= wr;
            }
        }
        self.wake();
    }

    /// Wakes the threads waiting on this socket ([`UnixSocket::wake_objects`]).
    fn wake(&self) {
        let mut objects = Vec::new();
        self.wake_objects(&mut objects);
        objects.into_iter().for_each(request_wake_object);
    }

    /// The number of the network namespace it is in.
    pub fn net_id(&self) -> u64 {
        self.net
            .as_ref()
            .map_or(crate::nsproxy::INIT_NET_NS, |n| n.id)
    }

    /// Its own address; only the address family when unbound.
    pub fn name(&self) -> Vec<u8> {
        if self.addr.is_empty() {
            unnamed()
        } else {
            self.addr.clone()
        }
    }

    /// The peer's address, None when not connected.
    pub fn peer_name(&self) -> Option<Vec<u8>> {
        match &self.link {
            Link::Stream { peer_addr, .. } | Link::Dgram { peer_addr, .. } => {
                Some(peer_addr.clone())
            }
            Link::None | Link::Listening { .. } => None,
        }
    }
}

impl Drop for UnixSocket {
    /// The peer reads EOF and its writes fail, connections waiting for accept close with it, and
    /// connects waiting for room in the backlog check again ([`UnixSocket::wake_objects`] includes
    /// its own receive queue). A connection closed with data this end had not read, or never
    /// accepted, is reset: the peer's next read once its data is read, `SO_ERROR` and `POLLERR`
    /// report `ECONNRESET` (Linux's `unix_release_sock`).
    fn drop(&mut self) {
        self.wake();
        let stale = match &self.link {
            Link::Stream { rx, tx, .. } => {
                let mut t = tx.borrow_mut();
                t.eof = true;
                t.reset |= self.embryo || !rx.borrow().packets.is_empty();
                drop(t);
                rx.borrow_mut().discard()
            }
            _ => self.rx.borrow_mut().discard(),
        };
        drop(stale);
    }
}

/// The `sockaddr_un` of a socket with no bound address: only the address family.
pub fn unnamed() -> Vec<u8> {
    (AF_UNIX as u16).to_le_bytes().to_vec()
}

// ---- Names ----

/// A name in the address table.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Name {
    /// A path address: the inode of the socket node in the store.
    Path(Ino),
    /// An abstract address: the number of its network namespace, the socket type and all bytes
    /// of `sun_path` (starting with NUL). As on Linux, abstract addresses of different network
    /// namespaces or socket types are unrelated (`unix_abstract_hash` mixes the type in), while
    /// path addresses are not kept apart.
    Abstract(u64, i32, Vec<u8>),
}

/// Bound addresses (kernel-wide). After a socket closes its weak reference dies, and the address
/// is released with it.
#[derive(Default)]
pub(crate) struct Names {
    map: BTreeMap<Name, Weak<RefCell<Socket>>>,
    next_auto: u32,
}

impl Names {
    /// Autobind: as on Linux, allocates in network namespace `net` an abstract address of `\0`
    /// followed by 5 hex digits, free among sockets of type `ty`.
    pub fn autobind(&mut self, net: u64, ty: i32, sock: &Rc<RefCell<Socket>>) -> Option<Vec<u8>> {
        for _ in 0..0x100000 {
            self.next_auto = (self.next_auto + 1) & 0xfffff;
            let mut path = vec![0u8];
            path.extend_from_slice(format!("{:05x}", self.next_auto).as_bytes());
            if self.insert(Name::Abstract(net, ty, path.clone()), sock) {
                return Some(path);
            }
        }
        None
    }

    /// Registers an address; returns false when a live socket already holds it.
    pub fn insert(&mut self, name: Name, sock: &Rc<RefCell<Socket>>) -> bool {
        self.map.retain(|_, w| w.strong_count() > 0);
        if self.lookup(&name).is_some() {
            return false;
        }
        self.map.insert(name, Rc::downgrade(sock));
        true
    }

    pub fn lookup(&self, name: &Name) -> Option<Rc<RefCell<Socket>>> {
        self.map.get(name).and_then(Weak::upgrade)
    }
}

// ---- Channels ----

/// One message: one write on a stream socket, or one datagram.
pub(crate) struct Packet {
    pub data: Vec<u8>,
    /// The bytes of a stream socket already read.
    pub off: usize,
    /// The open files `SCM_RIGHTS` brought.
    pub fds: Vec<FileRef>,
    /// The sender's credentials.
    pub cred: Ucred,
    /// The sender's address (recvfrom of datagrams).
    pub from: Vec<u8>,
}

impl Packet {
    fn remaining(&self) -> usize {
        self.data.len() - self.off
    }
}

/// Data read.
pub(crate) struct Received {
    pub data: Vec<u8>,
    /// The full length of the message (greater than `data` when a datagram was truncated).
    pub len: usize,
    pub fds: Vec<FileRef>,
    pub cred: Ucred,
    pub from: Vec<u8>,
}

/// The messages buffered in one direction.
#[derive(Default)]
pub(crate) struct Channel {
    pub packets: VecDeque<Packet>,
    /// Bytes not yet read.
    pub bytes: usize,
    /// No new data will come (the sender closed or did `SHUT_WR`, or the receiver did `SHUT_RD`):
    /// EOF once everything is read.
    pub eof: bool,
    /// The receiver no longer reads (closed or `SHUT_RD`): writes fail.
    pub closed: bool,
    /// The number of messages put in: as on Linux, every arrival triggers the receiver's `EPOLLET`
    /// once more.
    pub arrivals: u64,
    /// The number of times it was still writable after the receiver took data: Linux wakes the
    /// sender on every read (`unix_write_space`), and the sender's `EPOLLET` triggers once more.
    pub taken: u64,
    /// A datagram socket's receive queue: the messages it holds before senders wait (Linux's
    /// `sk_max_ack_backlog`, from `net.unix.max_dgram_qlen` when the socket was created), except
    /// its own peer ([`Channel::holds_back`]); None for stream channels.
    pub max_qlen: Option<usize>,
    /// The receive queue of the socket this one's datagram socket is connected to (Linux's
    /// `unix_peer`).
    pub peer: Weak<RefCell<Channel>>,
    /// The sending end of a connection closed without reading everything sent to it, or before
    /// it was accepted: the receiver's pending `ECONNRESET` (its `sk_err`), cleared once reported
    /// ([`Channel::take_reset`]).
    pub reset: bool,
}

pub(crate) type ChannelRef = Rc<RefCell<Channel>>;

impl Channel {
    /// The bytes that still fit.
    pub fn space(&self) -> usize {
        CAPACITY.saturating_sub(self.bytes)
    }

    /// Reports the pending `ECONNRESET` once (Linux's `sock_error`).
    pub fn take_reset(&mut self) -> bool {
        std::mem::take(&mut self.reset)
    }

    pub fn push(&mut self, p: Packet) {
        self.bytes += p.remaining();
        self.packets.push_back(p);
        self.arrivals += 1;
        request_wake_object(wake::of_value(self));
    }

    /// Stream read: takes up to `max` bytes in order. As on Linux, it stops after a message
    /// carrying open files is read, and data from different senders is not merged into one read.
    pub fn read_stream(&mut self, max: usize, peek: bool) -> Received {
        let mut data = Vec::new();
        let mut fds = Vec::new();
        let mut cred = None;
        // The sender's address, from the first message read (unix_stream_read_generic)
        let mut from = None;
        let mut i = 0;
        while let Some(p) = self.packets.get_mut(i) {
            if from.is_none() {
                from = Some(p.from.clone());
            }
            let room = max - data.len();
            // A read of nothing still takes the first message's files and leaves its data, as
            // Linux detaches them before looking at the length
            if max == 0 && i == 0 && cred.is_none() {
                cred = Some(p.cred);
                if peek {
                    fds.extend(p.fds.iter().cloned());
                } else {
                    fds.append(&mut p.fds);
                    if p.remaining() == 0 {
                        self.packets.pop_front();
                    }
                }
                break;
            }
            // A message with only open files and no data is read out even when there is no room
            if (room == 0 && p.remaining() > 0) || cred.is_some_and(|c| c != p.cred) {
                break;
            }
            cred = Some(p.cred);
            let n = room.min(p.remaining());
            data.extend_from_slice(&p.data[p.off..p.off + n]);
            let had_fds = !p.fds.is_empty();
            if peek {
                fds.extend(p.fds.iter().cloned());
                i += 1;
            } else {
                p.off += n;
                self.bytes -= n;
                fds.append(&mut p.fds);
                if p.remaining() == 0 {
                    self.packets.pop_front();
                }
            }
            if had_fds || data.len() >= max {
                break;
            }
        }
        if !peek && !data.is_empty() {
            if self.writable() {
                self.taken += 1;
            }
            request_wake_object(wake::of_value(self));
        }
        Received {
            len: data.len(),
            data,
            fds,
            cred: cred.unwrap_or_default(),
            from: from.unwrap_or_default(),
        }
    }

    /// Takes one whole message (datagram, `SOCK_SEQPACKET`), dropping what exceeds `max`.
    pub fn read_packet(&mut self, max: usize, peek: bool) -> Option<Received> {
        let (data, len, fds, cred, from) = if peek {
            let p = self.packets.front()?;
            let n = p.data.len().min(max);
            (
                p.data[..n].to_vec(),
                p.data.len(),
                p.fds.clone(),
                p.cred,
                p.from.clone(),
            )
        } else {
            let mut p = self.packets.pop_front()?;
            self.bytes -= p.data.len();
            let len = p.data.len();
            p.data.truncate(max);
            if self.writable() {
                self.taken += 1;
            }
            request_wake_object(wake::of_value(self));
            (p.data, len, std::mem::take(&mut p.fds), p.cred, p.from)
        };
        Some(Received {
            data,
            len,
            fds,
            cred,
            from,
        })
    }

    /// As on Linux: writable is reported only when the buffered data is at most a quarter of the
    /// capacity.
    pub fn writable(&self) -> bool {
        self.bytes * 4 <= CAPACITY
    }

    /// Whether a datagram from the socket receiving into `sender` must wait (Linux's
    /// `unix_recvq_full` for a receiver whose peer is not that socket): this queue holds more than
    /// its limit.
    pub fn holds_back(&self, sender: &Weak<RefCell<Channel>>) -> bool {
        self.max_qlen.is_some_and(|m| self.packets.len() > m) && !Weak::ptr_eq(&self.peer, sender)
    }

    /// The receiver closed: discard the buffered messages. The messages are handed to the caller
    /// to drop after its borrows are released, because the open files in them may need other
    /// channels when dropped.
    fn discard(&mut self) -> VecDeque<Packet> {
        self.closed = true;
        self.bytes = 0;
        std::mem::take(&mut self.packets)
    }
}

// ---- /proc/net/unix ----

/// In-flight sockets (those only in `SCM_RIGHTS` messages): `/proc/net/unix` lists them too.
pub(crate) fn inflight_sockets(k: &Kernel) -> Vec<Rc<RefCell<Socket>>> {
    let mut out = Vec::new();
    for c in k.unix_inflight.iter().filter_map(Weak::upgrade) {
        for p in &c.borrow().packets {
            out.extend(p.fds.iter().filter_map(unix_socket));
        }
    }
    out
}

/// The open files carried by messages in flight (each as many times as it is carried).
pub(crate) fn inflight_files(k: &Kernel) -> Vec<FileRef> {
    let mut out = Vec::new();
    for c in k.unix_inflight.iter().filter_map(Weak::upgrade) {
        for p in &c.borrow().packets {
            out.extend(p.fds.iter().cloned());
        }
    }
    out
}

/// `/proc/net/unix`: all AF_UNIX sockets. Abstract addresses start with `@`, and the NULs in them
/// are shown as `@` too.
pub(crate) fn proc_listing(sockets: &[(crate::syscall::SocketRef, u32)]) -> String {
    let mut s = String::from("Num       RefCount Protocol Flags    Type St Inode Path\n");
    for (sock, _) in sockets {
        let b = sock.borrow();
        let Socket::Unix(u) = &*b else {
            continue;
        };
        // __SO_ACCEPTCON; SS_UNCONNECTED, SS_CONNECTED
        let (flags, state) = match &u.link {
            Link::Listening { .. } => (0x10000, 1),
            Link::Stream { .. } | Link::Dgram { .. } => (0, 3),
            Link::None => (0, 1),
        };
        let ino = crate::pseudo::socket_ino(sock);
        line(&mut s, flags, u.ty, state, ino, &u.addr);
        // Connections not yet accepted: as on Linux they have no inode, the address is the
        // listening address and the state is SS_CONNECTING (there is no socket object yet)
        if let Link::Listening { pending, .. } = &u.link {
            for _ in pending {
                line(&mut s, 0, u.ty, 2, 0, &u.addr);
            }
        }
    }
    s
}

/// One line of `/proc/net/unix`.
fn line(s: &mut String, flags: u32, ty: i32, state: u32, ino: u64, addr: &[u8]) {
    use std::fmt::Write as _;
    write!(
        s,
        "0000000000000000: 00000002 00000000 {flags:08X} {ty:04X} {state:02X} {ino:5}"
    )
    .unwrap();
    match addr.get(2..) {
        Some([0, name @ ..]) => {
            let name: Vec<u8> = name
                .iter()
                .map(|&c| if c == 0 { b'@' } else { c })
                .collect();
            write!(s, " @{}", String::from_utf8_lossy(&name)).unwrap();
        }
        Some(path) if !path.is_empty() => {
            let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
            write!(s, " {}", String::from_utf8_lossy(&path[..end])).unwrap();
        }
        _ => {}
    }
    s.push('\n');
}

// ---- Garbage collection ----

/// Records a channel whose messages carry open files; [`collect`] finds in-flight sockets here.
pub(crate) fn note_inflight(k: &mut Kernel, c: &ChannelRef) {
    let weak = Rc::downgrade(c);
    if !k.unix_inflight.iter().any(|w| w.ptr_eq(&weak)) {
        k.unix_inflight.push(weak);
    }
}

/// Some open files were closed (close, dup2 over an fd, execve and so on): releases them, and
/// collects in-flight cycles when any of them is a unix socket.
pub(crate) fn released(k: &mut Kernel, files: Vec<FileRef>) {
    let unix = files.iter().any(|f| unix_socket(f).is_some());
    drop(files);
    if unix {
        collect(k);
    }
}

/// Collects in-flight sockets the program cannot reach (Linux's unix_gc). Candidates are unix
/// sockets all of whose references come from in-flight messages; references starting from the
/// candidates' receive queues do not count, candidates with references left are reachable from
/// outside, and so are the candidates in their receive queues. The remaining candidates have their
/// receive queues emptied, which breaks the cycles. A socket with an extra reference temporarily
/// held elsewhere (such as by a thread blocked on it) is not a candidate, so this can only collect
/// too little, never wrongly.
pub(crate) fn collect(k: &mut Kernel) {
    type Key = *const RefCell<OpenFile>;
    k.unix_inflight.retain(|w| {
        w.upgrade()
            .is_some_and(|c| c.borrow().packets.iter().any(|p| !p.fds.is_empty()))
    });
    // In-flight open files and how many times each is referenced by in-flight messages
    let mut inflight: HashMap<Key, (FileRef, usize)> = HashMap::new();
    for c in k.unix_inflight.iter().filter_map(Weak::upgrade) {
        for p in &c.borrow().packets {
            for f in &p.fds {
                inflight
                    .entry(Rc::as_ptr(f))
                    .or_insert_with(|| (f.clone(), 0))
                    .1 += 1;
            }
        }
    }
    // The reference counts also include our own one held above
    let mut candidates: HashMap<Key, usize> = inflight
        .iter()
        .filter(|(_, (f, n))| Rc::strong_count(f) == n + 1 && unix_socket(f).is_some())
        .map(|(&key, &(_, n))| (key, n))
        .collect();
    if candidates.is_empty() {
        return;
    }
    let children = |key: Key| -> Vec<Key> {
        let Some(s) = unix_socket(&inflight[&key].0) else {
            return Vec::new();
        };
        let Socket::Unix(u) = &*s.borrow() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for c in inboxes(u) {
            for p in &c.borrow().packets {
                out.extend(p.fds.iter().map(Rc::as_ptr));
            }
        }
        out
    };
    let keys: Vec<Key> = candidates.keys().copied().collect();
    for &key in &keys {
        for child in children(key) {
            if let Some(n) = candidates.get_mut(&child) {
                *n -= 1;
            }
        }
    }
    let mut work: Vec<Key> = keys.iter().copied().filter(|k| candidates[k] > 0).collect();
    let mut reachable: HashSet<Key> = work.iter().copied().collect();
    while let Some(key) = work.pop() {
        for child in children(key) {
            if candidates.contains_key(&child) && reachable.insert(child) {
                work.push(child);
            }
        }
    }
    // Drop the messages after all borrows are released: the open files in them need other
    // channels when dropped
    let mut garbage = Vec::new();
    for key in keys.into_iter().filter(|k| !reachable.contains(k)) {
        let Some(s) = unix_socket(&inflight[&key].0) else {
            continue;
        };
        if let Socket::Unix(u) = &*s.borrow() {
            for c in inboxes(u) {
                let mut c = c.borrow_mut();
                c.bytes = 0;
                garbage.extend(std::mem::take(&mut c.packets));
            }
        }
    }
    drop(inflight);
    drop(garbage);
}

/// The channels holding messages sent to `u`: a datagram socket's receive queue, the receive
/// direction of a stream connection, and for a listening socket also those of the connections
/// waiting for accept (Linux's GC also counts them as received by the listening socket).
fn inboxes(u: &UnixSocket) -> Vec<ChannelRef> {
    let mut out = vec![u.rx.clone()];
    match &u.link {
        Link::Stream { rx, .. } => out.push(rx.clone()),
        Link::Listening { pending, .. } => {
            for s in pending {
                if let Socket::Unix(e) = &*s.borrow()
                    && let Link::Stream { rx, .. } = &e.link
                {
                    out.push(rx.clone());
                }
            }
        }
        _ => {}
    }
    out
}

/// Returns the socket of `f` when it is an AF_UNIX socket.
fn unix_socket(f: &FileRef) -> Option<Rc<RefCell<Socket>>> {
    match &f.borrow().kind {
        FileKind::Socket(s) if matches!(&*s.borrow(), Socket::Unix(_)) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fd::{FileKind, OpenFile, oflags};
    use crate::net::SOCK_STREAM;

    fn packet(data: &[u8], pid: i32) -> Packet {
        Packet {
            data: data.to_vec(),
            off: 0,
            fds: Vec::new(),
            cred: Ucred::of(pid, 0, 0),
            from: Vec::new(),
        }
    }

    /// The open files of a pair of connected stream sockets, and the channel from `a` to `b`.
    fn pair() -> (FileRef, FileRef, ChannelRef) {
        let (a, b) = UnixSocket::pair(SOCK_STREAM, Ucred::of(1, 0, 0));
        let Link::Stream { tx, .. } = &a.link else {
            unreachable!()
        };
        let tx = tx.clone();
        let file = |u| {
            let s = Rc::new(RefCell::new(Socket::Unix(u)));
            OpenFile::new(FileKind::Socket(s), oflags::O_RDWR)
        };
        (file(a), file(b), tx)
    }

    fn send(k: &mut Kernel, c: &ChannelRef, f: &FileRef) {
        let mut p = packet(b"x", 1);
        p.fds.push(f.clone());
        c.borrow_mut().push(p);
        note_inflight(k, c);
    }

    #[test]
    fn collects_sockets_only_reachable_from_their_own_queue() {
        let mut k = Kernel::new(crate::process::Tier::Interpreter);
        // b is in its own receive queue
        let (a, b, to_b) = pair();
        send(&mut k, &to_b, &b);
        let gone = Rc::downgrade(&b);
        drop((a, b, to_b));
        // In-flight c has d in its receive queue, and c itself is in the queue of a socket still
        // open: both are reachable
        let (keep, keep_peer, to_keep_peer) = pair();
        let (c, c_peer, to_c_peer) = pair();
        let (d, d_peer, _) = pair();
        send(&mut k, &to_c_peer, &d_peer);
        send(&mut k, &to_keep_peer, &c_peer);
        let (kept_c, kept_d) = (Rc::downgrade(&c_peer), Rc::downgrade(&d_peer));
        drop((c, c_peer, to_c_peer, d, d_peer, to_keep_peer));
        collect(&mut k);
        assert!(gone.upgrade().is_none());
        assert!(kept_c.upgrade().is_some() && kept_d.upgrade().is_some());
        assert_eq!(inflight_sockets(&k).len(), 2);
        // Once the outermost is closed too, the whole chain is collected
        drop((keep, keep_peer));
        collect(&mut k);
        assert!(kept_c.upgrade().is_none() && kept_d.upgrade().is_none());
        assert!(inflight_sockets(&k).is_empty());
    }

    #[test]
    fn stream_reads_merge_writes_but_stop_at_passed_fds() {
        let mut c = Channel::default();
        c.push(packet(b"ab", 1));
        let mut with_fd = packet(b"cd", 1);
        with_fd
            .fds
            .push(OpenFile::new(FileKind::Host(1), oflags::O_WRONLY));
        c.push(with_fd);
        c.push(packet(b"ef", 1));
        c.push(packet(b"gh", 2));
        assert_eq!(c.bytes, 8);

        let peek = c.read_stream(100, true);
        assert_eq!(peek.data, b"abcd");
        assert_eq!(peek.fds.len(), 1);
        assert_eq!(c.bytes, 8);

        let r = c.read_stream(3, false);
        assert_eq!((r.data.as_slice(), r.fds.len()), (&b"abc"[..], 1));
        // The rest of a message with open files is read as usual, but the open files are
        // delivered only once
        let r = c.read_stream(100, false);
        assert_eq!((r.data.as_slice(), r.fds.len()), (&b"def"[..], 0));
        // Data from different senders is not merged
        let r = c.read_stream(100, false);
        assert_eq!((r.data.as_slice(), r.cred.pid), (&b"gh"[..], 2));
        assert_eq!(c.bytes, 0);
    }

    #[test]
    fn datagrams_keep_boundaries_and_truncate() {
        let mut c = Channel::default();
        c.push(packet(b"hello", 1));
        c.push(packet(b"x", 1));
        let r = c.read_packet(3, false).unwrap();
        assert_eq!((r.data.as_slice(), r.len), (&b"hel"[..], 5));
        let r = c.read_packet(3, false).unwrap();
        assert_eq!(r.data, b"x");
        assert!(c.read_packet(3, false).is_none());
        assert_eq!(c.bytes, 0);
    }

    #[test]
    fn closing_one_end_shows_eof_and_hangup_on_the_other() {
        let (a, b) = UnixSocket::pair(SOCK_STREAM, Ucred::of(7, 0, 0));
        assert_eq!(b.peer_cred, Some(Ucred::of(7, 0, 0)));
        if let Link::Stream { tx, .. } = &a.link {
            tx.borrow_mut().push(packet(b"bye", 7));
        }
        assert_eq!(b.readiness() & (POLLIN | POLLHUP), POLLIN);
        drop(a);
        let ev = b.readiness();
        assert_eq!(
            ev & (POLLIN | POLLHUP | POLLRDHUP | POLLOUT),
            POLLIN | POLLHUP | POLLRDHUP | POLLOUT
        );
        if let Link::Stream { rx, tx, .. } = &b.link {
            assert_eq!(rx.borrow_mut().read_stream(10, false).data, b"bye");
            assert!(tx.borrow().closed);
        }
    }

    #[test]
    fn abstract_names_are_released_with_the_socket() {
        let mut names = Names::default();
        let s = Rc::new(RefCell::new(Socket::Unix(UnixSocket::new(SOCK_STREAM))));
        let name = Name::Abstract(0, SOCK_STREAM, b"\0test".to_vec());
        assert!(names.insert(name.clone(), &s));
        assert!(!names.insert(name.clone(), &s));
        let auto = names.autobind(0, SOCK_STREAM, &s).unwrap();
        assert_eq!(auto, b"\x0000001");
        drop(s);
        assert!(names.lookup(&name).is_none());
        let t = Rc::new(RefCell::new(Socket::Unix(UnixSocket::new(SOCK_STREAM))));
        assert!(names.insert(name, &t));
    }
}
