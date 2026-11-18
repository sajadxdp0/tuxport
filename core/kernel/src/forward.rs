//! Connections from outside the host to published ports (docs/rounds/173-port-forwarding.md).
//!
//! When the initial namespace has PREROUTING DNAT rules (ports published by podman or docker), a
//! connection from outside Tuxport (another device, a browser on the phone) to that port on the
//! host reaches the socket the guest is listening on in the host (the one podman holds the port
//! with and never accepts on). Such a socket is duplicated and handed to the host thread here: it
//! accepts connections, connects to the DNAT destination (the container's loopback address on the
//! host) and copies data between the two, acting as a docker-proxy inside the kernel layer. UDP is
//! forwarded through one host socket per source, closed after two minutes idle.
//!
//! When the outside client is IPv4 and not a loopback address (another device), the forwarded
//! connection first binds a 127.254.x.y address and the client's port before connecting, and
//! records it ([`outside_peer`]): accept and getpeername in the container see the client's real
//! address, as with DNAT on Linux (docs/rounds/176-real-client-address.md). Connections from
//! loopback addresses (other apps on the phone, a browser opening localhost) still bind the
//! gateway's address on the host: on Linux, when a published port is reached from the same
//! machine, netavark masquerades and docker goes through docker-proxy, so the container sees the
//! gateway too.
//!
//! The thread only touches host fds and never takes the BKL. Which ports to forward and where to
//! is computed from the rules by `syscall::net` and handed over ([`Forwarder::add`]); dropping the
//! [`Handle`] when the socket closes or the rules change stops the forwarding.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;
const POLLERR: i16 = 0x8;
const POLLHUP: i16 = 0x10;
const EINTR: i32 = 4;
const EAGAIN: i32 = 11;
const EINPROGRESS: i32 = 115;
const ECONNRESET: i32 = 104;
const SOL_SOCKET: i32 = 1;
const SO_ERROR: i32 = 4;
const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;
const SHUT_WR: i32 = 1;
/// Most bytes buffered per direction (when full, reading pauses).
const BUFFER: usize = 64 * 1024;
/// How long a UDP forwarding socket may stay idle before it is closed (docker-proxy: 90 seconds).
const UDP_IDLE: Duration = Duration::from_secs(120);
/// How long a listening socket that is readable while accept fails (out of fds, shut down) is left
/// out of the poll, so the thread does not spin on it.
const ACCEPT_BACKOFF: Duration = Duration::from_secs(1);
/// How long the thread waits after poll itself fails (too many fds, out of memory) before trying
/// again.
const POLL_BACKOFF: Duration = Duration::from_millis(100);

// ---- Forwarding thread ----

/// A port to forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub udp: bool,
    /// Destination on the host (sockaddr).
    pub addr: Vec<u8>,
    /// Host address bound (port 0) before connecting: the source the container sees (the initial
    /// namespace's address facing it).
    pub source: Option<Vec<u8>>,
}

/// The forwarding thread and the ports handed to it.
#[derive(Default)]
pub(crate) struct Forwarder {
    shared: Arc<Shared>,
}

impl Forwarder {
    /// Forwards `fd`, listening on the host (a TCP listening socket, a bound UDP socket), to
    /// `target`.
    pub(crate) fn add(&self, fd: i32, target: Target) -> Result<Handle, i32> {
        let event = match self.shared.event.get() {
            Some(e) => e,
            None => {
                let e = tuxport_hal::Event::new()?;
                let _ = self.shared.event.set(e);
                self.shared.event.get().unwrap()
            }
        };
        let dup = tuxport_hal::dup(fd)?;
        let mut st = self.shared.state.lock().unwrap();
        st.next += 1;
        let id = st.next;
        st.ports.insert(id, (dup, target.clone()));
        if !st.running {
            let shared = self.shared.clone();
            let spawned = std::thread::Builder::new()
                .name("forward".into())
                .spawn(move || run(&shared));
            if spawned.is_err() {
                st.ports.remove(&id);
                tuxport_hal::net::close(dup);
                return Err(11);
            }
            st.running = true;
        }
        drop(st);
        event.signal();
        Ok(Handle {
            shared: self.shared.clone(),
            id,
            target,
        })
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().stop = true;
        if let Some(e) = self.shared.event.get() {
            e.signal();
        }
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    event: std::sync::OnceLock<tuxport_hal::Event>,
}

#[derive(Default)]
struct State {
    next: u64,
    /// Ports to forward: the duplicated listening fd and the destination.
    ports: HashMap<u64, (i32, Target)>,
    /// Duplicated fds of ports no longer forwarded: the thread closes them (it may be polling
    /// them).
    closing: Vec<i32>,
    running: bool,
    /// The kernel is gone ([`Forwarder`] was dropped): the thread closes the fds it holds and
    /// exits.
    stop: bool,
}

/// One forwarded port: forwarding stops when it is dropped.
pub(crate) struct Handle {
    shared: Arc<Shared>,
    id: u64,
    pub target: Target,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle")
            .field("id", &self.id)
            .field("target", &self.target)
            .finish()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let mut st = self.shared.state.lock().unwrap();
        if let Some((fd, _)) = st.ports.remove(&self.id) {
            st.closing.push(fd);
        }
        drop(st);
        if let Some(e) = self.shared.event.get() {
            e.signal();
        }
    }
}

/// One forwarded TCP connection.
struct Conn {
    client: i32,
    upstream: i32,
    /// The bound 127.254.x.y ([`outside_peer`]); its record is removed when the connection ends.
    outside: Option<[u8; 4]>,
    /// The connection to the destination has completed.
    ready: bool,
    to_up: Vec<u8>,
    to_client: Vec<u8>,
    client_eof: bool,
    up_eof: bool,
    /// shutdown(SHUT_WR) already done toward the other side.
    up_shut: bool,
    client_shut: bool,
}

impl Conn {
    fn done(&self) -> bool {
        self.client_eof && self.up_eof && self.to_up.is_empty() && self.to_client.is_empty()
    }

    fn close(&self) {
        tuxport_hal::net::close(self.client);
        tuxport_hal::net::close(self.upstream);
        if let Some(a) = self.outside {
            forget_outside(a);
        }
    }
}

/// UDP: the forwarding socket of one source.
struct Relay {
    fd: i32,
    port: u64,
    client: Vec<u8>,
    last: Instant,
}

/// The forwarding thread.
fn run(shared: &Shared) {
    let event = shared.event.get().expect("event before thread");
    let mut ports: HashMap<u64, (i32, Target)> = HashMap::new();
    let mut conns: Vec<Conn> = Vec::new();
    let mut relays: Vec<Relay> = Vec::new();
    // Ports whose accept failed while they polled readable, and until when they are left out
    let mut stalled: HashMap<u64, Instant> = HashMap::new();
    loop {
        // Take the ports handed over; close those no longer forwarded, and their UDP
        // forwarding sockets too
        {
            event.clear();
            let mut st = shared.state.lock().unwrap();
            for fd in st.closing.drain(..) {
                ports.retain(|_, (f, _)| *f != fd);
                tuxport_hal::net::close(fd);
            }
            ports = st.ports.clone();
            if st.stop {
                for (fd, _) in st.ports.values() {
                    tuxport_hal::net::close(*fd);
                }
                st.ports.clear();
                st.running = false;
                drop(st);
                for c in &conns {
                    c.close();
                }
                for r in &relays {
                    tuxport_hal::net::close(r.fd);
                }
                return;
            }
        }
        relays.retain(|r| {
            let keep = ports.contains_key(&r.port) && r.last.elapsed() < UDP_IDLE;
            if !keep {
                tuxport_hal::net::close(r.fd);
            }
            keep
        });
        let now = Instant::now();
        stalled.retain(|id, until| ports.contains_key(id) && *until > now);
        // What to wait for: the event, every port (but stalled ones), both sides of every
        // connection, every UDP forwarding socket
        let mut fds: Vec<(i32, i16, i16)> = vec![(event.fd(), POLLIN, 0)];
        let order: Vec<(u64, i32, bool)> = ports
            .iter()
            .filter(|(id, _)| !stalled.contains_key(id))
            .map(|(id, (fd, t))| (*id, *fd, t.udp))
            .collect();
        fds.extend(order.iter().map(|&(_, fd, _)| (fd, POLLIN, 0)));
        for c in &conns {
            let mut ce = 0;
            if !c.client_eof && c.to_up.len() < BUFFER {
                ce |= POLLIN;
            }
            if !c.to_client.is_empty() {
                ce |= POLLOUT;
            }
            let mut ue = 0;
            if !c.ready {
                ue |= POLLOUT;
            } else {
                if !c.up_eof && c.to_client.len() < BUFFER {
                    ue |= POLLIN;
                }
                if !c.to_up.is_empty() {
                    ue |= POLLOUT;
                }
            }
            fds.push((c.client, ce, 0));
            fds.push((c.upstream, ue, 0));
        }
        fds.extend(relays.iter().map(|r| (r.fd, POLLIN, 0)));
        // Connections accepted and forwarding sockets opened below are not in this round
        let (polled_conns, polled_relays) = (conns.len(), relays.len());
        let idle = (!relays.is_empty()).then_some(Duration::from_secs(5));
        let resume = stalled
            .values()
            .min()
            .map(|t| t.saturating_duration_since(now));
        let timeout = match (idle, resume) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match tuxport_hal::poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(EINTR) => continue,
            Err(_) => {
                std::thread::sleep(POLL_BACKOFF);
                continue;
            }
        }
        let mut at = 1;
        // Ports: accept connections, receive datagrams
        for &(id, fd, udp) in &order {
            let ready = fds[at].2 != 0;
            at += 1;
            if !ready {
                continue;
            }
            let target = &ports[&id].1;
            if udp {
                datagrams(id, fd, target, &mut relays);
                continue;
            }
            // Accept at most 64 at a time so the other connections do not starve
            for n in 0..64 {
                let (client, from) = match tuxport_hal::net::accept(fd) {
                    Ok(c) => c,
                    // Readable but nothing taken: out of fds, or a listener shut down (readable
                    // for good); leave it out of the poll for a while
                    Err(e) if n == 0 && e != EAGAIN => {
                        stalled.insert(id, Instant::now() + ACCEPT_BACKOFF);
                        break;
                    }
                    Err(_) => break,
                };
                match open(target, outside_client(&from)) {
                    Ok((upstream, ready, outside)) => conns.push(Conn {
                        client,
                        upstream,
                        outside,
                        ready,
                        to_up: Vec::new(),
                        to_client: Vec::new(),
                        client_eof: false,
                        up_eof: false,
                        up_shut: false,
                        client_shut: false,
                    }),
                    // The destination cannot be reached: the client sees the connection closed
                    Err(_) => tuxport_hal::net::close(client),
                }
            }
        }
        // Connections
        let mut broken = vec![false; conns.len()];
        for (k, c) in conns.iter_mut().enumerate().take(polled_conns) {
            let (cr, ur) = (fds[at + 2 * k].2, fds[at + 2 * k + 1].2);
            let r = step(c, cr, ur);
            broken[k] = r.is_err() || c.done();
        }
        at += 2 * polled_conns;
        let mut i = 0;
        conns.retain(|c| {
            let gone = broken[i];
            i += 1;
            if gone {
                c.close();
            }
            !gone
        });
        // Send UDP replies back to the source
        for (k, r) in relays.iter_mut().enumerate().take(polled_relays) {
            if fds[at + k].2 == 0 {
                continue;
            }
            let Some((port_fd, _)) = ports.get(&r.port) else {
                continue;
            };
            let mut buf = vec![0u8; 65536];
            while let Ok(m) = tuxport_hal::net::recvmsg(r.fd, &mut buf, 0, 0) {
                let _ = tuxport_hal::net::sendmsg(
                    *port_fd,
                    &buf[..m.len.min(buf.len())],
                    &r.client,
                    &[],
                    0,
                );
                r.last = Instant::now();
            }
        }
    }
}

/// Events ready on one connection (`cr` on the client side, `ur` on the destination side).
fn step(c: &mut Conn, cr: i16, ur: i16) -> Result<(), i32> {
    if !c.ready {
        if ur == 0 {
            return Ok(());
        }
        let err = tuxport_hal::net::getsockopt(c.upstream, SOL_SOCKET, SO_ERROR, 4)?;
        match i32::from_le_bytes(err[..4].try_into().unwrap()) {
            0 => c.ready = true,
            e => return Err(e),
        }
    }
    // A side that reports an error, or hangs up after its end of file while it may still be written
    // to, was reset: nothing more goes through it. Neither direction is polled for input after end
    // of file, so a reset of a half-closed side would otherwise show at every poll
    for (fd, r, eof, shut) in [
        (c.client, cr, c.client_eof, c.client_shut),
        (c.upstream, ur, c.up_eof, c.up_shut),
    ] {
        if r & POLLERR != 0 {
            let err = tuxport_hal::net::getsockopt(fd, SOL_SOCKET, SO_ERROR, 4)?;
            match i32::from_le_bytes(err[..4].try_into().unwrap()) {
                0 => {}
                e => return Err(e),
            }
        }
        if r & POLLHUP != 0 && eof && !shut {
            return Err(ECONNRESET);
        }
    }
    if cr & (POLLIN | POLLHUP | POLLERR) != 0 && !c.client_eof {
        c.client_eof = !fill(c.client, &mut c.to_up)?;
    }
    if ur & (POLLIN | POLLHUP | POLLERR) != 0 && !c.up_eof {
        c.up_eof = !fill(c.upstream, &mut c.to_client)?;
    }
    drain(c.upstream, &mut c.to_up)?;
    drain(c.client, &mut c.to_client)?;
    // One side reached its end and all its data went across: half-close toward the other side
    if c.client_eof && c.to_up.is_empty() && !c.up_shut {
        let _ = tuxport_hal::net::shutdown(c.upstream, SHUT_WR);
        c.up_shut = true;
    }
    if c.up_eof && c.to_client.is_empty() && !c.client_shut {
        let _ = tuxport_hal::net::shutdown(c.client, SHUT_WR);
        c.client_shut = true;
    }
    Ok(())
}

/// Reads from `fd` into `buf` (up to [`BUFFER`]): end of file is Ok(false).
fn fill(fd: i32, buf: &mut Vec<u8>) -> Result<bool, i32> {
    let mut chunk = vec![0u8; BUFFER - buf.len()];
    match tuxport_hal::read(fd, &mut chunk) {
        Ok(0) => Ok(false),
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            Ok(true)
        }
        Err(EAGAIN) => Ok(true),
        Err(e) => Err(e),
    }
}

/// Writes `buf` to `fd` (whatever cannot be written stays).
fn drain(fd: i32, buf: &mut Vec<u8>) -> Result<(), i32> {
    while !buf.is_empty() {
        match tuxport_hal::net::sendmsg(fd, buf, &[], &[], 0) {
            Ok(n) => {
                buf.drain(..n);
            }
            Err(EAGAIN) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Datagrams received on a UDP port: finds (or opens) the forwarding socket of the source and
/// sends them to the destination.
fn datagrams(id: u64, fd: i32, target: &Target, relays: &mut Vec<Relay>) {
    let mut buf = vec![0u8; 65536];
    for _ in 0..64 {
        let Ok(m) = tuxport_hal::net::recvmsg(fd, &mut buf, 0, 0) else {
            break;
        };
        let data = &buf[..m.len.min(buf.len())];
        let r = match relays
            .iter()
            .position(|r| r.port == id && r.client == m.addr)
        {
            Some(i) => &mut relays[i],
            None => {
                // UDP still binds the gateway's address: datagrams the container sends back to
                // the real address must go back through the forwarding socket, and the address
                // mapping has no entry for that direction
                let Ok((rfd, _, _)) = open(target, None) else {
                    continue;
                };
                relays.push(Relay {
                    fd: rfd,
                    port: id,
                    client: m.addr.clone(),
                    last: Instant::now(),
                });
                relays.last_mut().unwrap()
            }
        };
        r.last = Instant::now();
        let _ = tuxport_hal::net::sendmsg(r.fd, data, &[], &[], 0);
    }
}

/// Creates a host socket connecting to `t` (nonblocking; the connection may still be in
/// progress) and returns (fd, connected, bound 127.254.x.y). When `client` is an outside client's
/// IPv4 address and port, first binds a 127.254.x.y address and that port (letting the host choose
/// if the port cannot be bound) and records it; otherwise binds `t.source`.
fn open(t: &Target, client: Option<([u8; 4], u16)>) -> Result<(i32, bool, Option<[u8; 4]>), i32> {
    let fd = tuxport_hal::net::socket(
        family(&t.addr),
        if t.udp { SOCK_DGRAM } else { SOCK_STREAM },
        0,
    )?;
    let mut outside = None;
    let bound = match client.filter(|_| family(&t.addr) == 2) {
        Some((ip, port)) => {
            let host = take_outside();
            let at = |p: u16| tuxport_hal::net::sockaddr(std::net::SocketAddr::from((host, p)));
            let r = tuxport_hal::net::bind(fd, &at(port))
                .or_else(|_| tuxport_hal::net::bind(fd, &at(0)));
            if r.is_ok() {
                let hport = tuxport_hal::net::sockname(fd, false)
                    .ok()
                    .map_or(0, |a| u16::from_be_bytes([a[2], a[3]]));
                record_outside(host, ip, port, hport);
                outside = Some(host);
            }
            r
        }
        None => t
            .source
            .as_ref()
            .map_or(Ok(()), |src| tuxport_hal::net::bind(fd, src)),
    };
    if let Err(e) = bound {
        tuxport_hal::net::close(fd);
        return Err(e);
    }
    match tuxport_hal::net::connect(fd, &t.addr) {
        Ok(()) => Ok((fd, true, outside)),
        Err(EINPROGRESS) => Ok((fd, false, outside)),
        Err(e) => {
            tuxport_hal::net::close(fd);
            if let Some(a) = outside {
                forget_outside(a);
            }
            Err(e)
        }
    }
}

// ---- Real client address ----

/// 127.254.0.0/16 on the host is kept for connections forwarded in from outside (blocks of new
/// network namespaces never take it).
pub(crate) const OUTSIDE_BLOCK: u8 = 254;

/// Host address bound by a connection forwarded in from outside → (client address, client port,
/// bound host port). One per process: addresses come from a process-wide counter, so they never
/// repeat.
static OUTSIDE: Mutex<Option<Outside>> = Mutex::new(None);

#[derive(Default)]
struct Outside {
    next: u16,
    peers: HashMap<[u8; 4], ([u8; 4], u16, u16)>,
}

/// The IPv4 address and port in an accepted client address: for an IPv4-mapped IPv6 address, the
/// IPv4 part; None for loopback addresses and pure IPv6.
fn outside_client(addr: &[u8]) -> Option<([u8; 4], u16)> {
    const MAPPED: [u8; 12] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff];
    let port = u16::from_be_bytes([*addr.get(2)?, *addr.get(3)?]);
    let ip: [u8; 4] = match family(addr) {
        2 => addr.get(4..8)?.try_into().ok()?,
        10 if addr.get(8..20)? == MAPPED => addr.get(20..24)?.try_into().ok()?,
        _ => return None,
    };
    (ip[0] != 127).then_some((ip, port))
}

/// Gives a connection from outside a 127.254.x.y address (skipping .0.0).
fn take_outside() -> [u8; 4] {
    let mut g = OUTSIDE.lock().unwrap();
    let o = g.get_or_insert_with(Default::default);
    loop {
        o.next = o.next.wrapping_add(1);
        let [a, b] = o.next.to_be_bytes();
        let addr = [127, OUTSIDE_BLOCK, a, b];
        if o.next != 0 && !o.peers.contains_key(&addr) {
            return addr;
        }
    }
}

fn record_outside(host: [u8; 4], client: [u8; 4], cport: u16, hport: u16) {
    let mut g = OUTSIDE.lock().unwrap();
    g.get_or_insert_with(Default::default)
        .peers
        .insert(host, (client, cport, hport));
}

/// Records a connection forwarded in from outside, in tests ([`outside_peer`]).
#[cfg(test)]
pub(crate) fn record_outside_for_test(host: [u8; 4], client: [u8; 4], cport: u16, hport: u16) {
    record_outside(host, client, cport, hport);
}

/// The client's real address and port, when host address `host` (127.254.x.y) and port `port`
/// belong to a connection forwarded in from outside.
pub(crate) fn outside_peer(host: [u8; 4], port: u16) -> Option<([u8; 4], u16)> {
    let g = OUTSIDE.lock().unwrap();
    let &(ip, cport, hport) = g.as_ref()?.peers.get(&host)?;
    (hport == port).then_some((ip, cport))
}

fn forget_outside(host: [u8; 4]) {
    if let Some(o) = OUTSIDE.lock().unwrap().as_mut() {
        o.peers.remove(&host);
    }
}

fn family(addr: &[u8]) -> i32 {
    i32::from(u16::from_le_bytes([addr[0], addr[1]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AF_INET: i32 = 2;

    fn loopback(port: u16) -> Vec<u8> {
        tuxport_hal::net::sockaddr(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
    }

    fn port(fd: i32) -> u16 {
        let a = tuxport_hal::net::sockname(fd, false).unwrap();
        u16::from_be_bytes([a[2], a[3]])
    }

    /// Waits, blocking, until the fd is ready (used in tests).
    fn wait(fd: i32, ev: i16) {
        let mut f = [(fd, ev, 0)];
        tuxport_hal::poll(&mut f, Some(Duration::from_secs(5))).unwrap();
        assert_ne!(f[0].2, 0, "timed out");
    }

    /// TCP: an outside connection to the listening socket gets its data to the destination and
    /// the reply back, and both sides end after closing.
    #[test]
    fn tcp_is_forwarded() {
        let server = tuxport_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        tuxport_hal::net::bind(server, &loopback(0)).unwrap();
        tuxport_hal::net::listen(server, 8).unwrap();
        let held = tuxport_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        tuxport_hal::net::bind(held, &loopback(0)).unwrap();
        tuxport_hal::net::listen(held, 8).unwrap();
        let f = Forwarder::default();
        let h = f
            .add(
                held,
                Target {
                    udp: false,
                    addr: loopback(port(server)),
                    source: None,
                },
            )
            .unwrap();
        // The "outside" client
        let c = tuxport_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        let _ = tuxport_hal::net::connect(c, &loopback(port(held)));
        wait(server, POLLIN);
        let (s, _) = tuxport_hal::net::accept(server).unwrap();
        wait(c, POLLOUT);
        tuxport_hal::net::sendmsg(c, b"ping", &[], &[], 0).unwrap();
        wait(s, POLLIN);
        let mut buf = [0u8; 16];
        assert_eq!(tuxport_hal::read(s, &mut buf), Ok(4));
        assert_eq!(&buf[..4], b"ping");
        tuxport_hal::net::sendmsg(s, b"pong!", &[], &[], 0).unwrap();
        tuxport_hal::net::close(s);
        wait(c, POLLIN);
        assert_eq!(tuxport_hal::read(c, &mut buf), Ok(5));
        assert_eq!(&buf[..5], b"pong!");
        // The destination closed: the client reads end of file
        wait(c, POLLIN);
        assert_eq!(tuxport_hal::read(c, &mut buf), Ok(0));
        // No longer forwarded: incoming connections are not accepted by anyone
        drop(h);
        std::thread::sleep(Duration::from_millis(100));
        let c2 = tuxport_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        let _ = tuxport_hal::net::connect(c2, &loopback(port(held)));
        let mut f2 = [(server, POLLIN, 0)];
        tuxport_hal::poll(&mut f2, Some(Duration::from_millis(300))).unwrap();
        assert_eq!(f2[0].2, 0);
        for fd in [server, held, c, c2] {
            tuxport_hal::net::close(fd);
        }
    }

    /// An outside client (not a loopback address): the forwarded connection binds a 127.254.x.y
    /// address and the client's port and records the client's real address; the record is
    /// removed when the connection ends.
    #[test]
    fn outside_clients_keep_their_address() {
        // Use a non-loopback IPv4 address of the host as the outside client; skip the test if
        // there is none
        let Some(ip) = tuxport_hal::net::interfaces().iter().find_map(|i| {
            i.addrs
                .iter()
                .find(|(f, a, _)| *f == AF_INET && a.len() == 4 && a[0] != 127)
                .map(|(_, a, _)| <[u8; 4]>::try_from(&a[..]).unwrap())
        }) else {
            return;
        };
        let at =
            |ip: [u8; 4], p: u16| tuxport_hal::net::sockaddr(std::net::SocketAddr::from((ip, p)));
        let server = tuxport_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        tuxport_hal::net::bind(server, &loopback(0)).unwrap();
        tuxport_hal::net::listen(server, 8).unwrap();
        let held = tuxport_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        tuxport_hal::net::bind(held, &at([0; 4], 0)).unwrap();
        tuxport_hal::net::listen(held, 8).unwrap();
        let f = Forwarder::default();
        let _h = f
            .add(
                held,
                Target {
                    udp: false,
                    addr: loopback(port(server)),
                    source: None,
                },
            )
            .unwrap();
        let c = tuxport_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        tuxport_hal::net::bind(c, &at(ip, 0)).unwrap();
        let _ = tuxport_hal::net::connect(c, &at(ip, port(held)));
        wait(server, POLLIN);
        let (s, peer) = tuxport_hal::net::accept(server).unwrap();
        let host: [u8; 4] = peer[4..8].try_into().unwrap();
        let hport = u16::from_be_bytes([peer[2], peer[3]]);
        assert_eq!(host[..2], [127, OUTSIDE_BLOCK]);
        // What is bound is the client's port
        assert_eq!(hport, port(c));
        assert_eq!(outside_peer(host, hport), Some((ip, port(c))));
        assert_eq!(outside_peer(host, hport.wrapping_add(1)), None);
        // After both sides close, the record is gone
        tuxport_hal::net::close(c);
        tuxport_hal::net::close(s);
        for _ in 0..50 {
            if outside_peer(host, hport).is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(outside_peer(host, hport), None);
        // A connection from a loopback address is not an outside client
        assert_eq!(outside_client(&loopback(4000)), None);
        tuxport_hal::net::close(server);
        tuxport_hal::net::close(held);
    }

    /// CPU time (clock ticks) of the forwarding threads of this process, on Linux hosts.
    fn forward_ticks() -> Option<u64> {
        let mut total = 0;
        for task in std::fs::read_dir("/proc/self/task").ok()? {
            let stat = std::fs::read_to_string(task.ok()?.path().join("stat")).ok()?;
            let (name, rest) = stat.split_once(") ")?;
            if name.ends_with("(forward") {
                let f: Vec<&str> = rest.split(' ').collect();
                total += f[11].parse::<u64>().ok()? + f[12].parse::<u64>().ok()?;
            }
        }
        Some(total)
    }

    /// A connection whose client half-closed and then reset is closed toward the destination at
    /// once, and a listening socket that stays readable while accept fails (shut down) is left
    /// out of the poll for a while: neither makes the thread spin.
    #[test]
    fn resets_and_dead_listeners_do_not_spin() {
        let server = tuxport_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        tuxport_hal::net::bind(server, &loopback(0)).unwrap();
        tuxport_hal::net::listen(server, 8).unwrap();
        let held = tuxport_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        tuxport_hal::net::bind(held, &loopback(0)).unwrap();
        tuxport_hal::net::listen(held, 8).unwrap();
        let f = Forwarder::default();
        let _h = f
            .add(
                held,
                Target {
                    udp: false,
                    addr: loopback(port(server)),
                    source: None,
                },
            )
            .unwrap();
        let c = tuxport_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        let _ = tuxport_hal::net::connect(c, &loopback(port(held)));
        wait(server, POLLIN);
        let (s, _) = tuxport_hal::net::accept(server).unwrap();
        wait(c, POLLOUT);
        tuxport_hal::net::sendmsg(c, b"x", &[], &[], 0).unwrap();
        tuxport_hal::net::shutdown(c, SHUT_WR).unwrap();
        let mut buf = [0u8; 16];
        wait(s, POLLIN);
        assert_eq!(tuxport_hal::read(s, &mut buf), Ok(1));
        wait(s, POLLIN);
        assert_eq!(tuxport_hal::read(s, &mut buf), Ok(0));
        let idle = |what: &str| {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(before) = forward_ticks() {
                std::thread::sleep(Duration::from_millis(500));
                let used = forward_ticks().unwrap() - before;
                assert!(
                    used < 10,
                    "{what}: the forwarding threads used {used} ticks in 0.5 s"
                );
            }
        };
        // The client resets (SO_LINGER with a zero timeout) after its half-close
        let linger = [1i32.to_le_bytes(), 0i32.to_le_bytes()].concat();
        tuxport_hal::net::setsockopt(c, SOL_SOCKET, 13, &linger).unwrap();
        tuxport_hal::net::close(c);
        idle("reset");
        // A listener shut down for reading polls readable for good while accept fails
        tuxport_hal::net::shutdown(held, 0).unwrap();
        idle("shut-down listener");
        for fd in [server, held, s] {
            tuxport_hal::net::close(fd);
        }
    }

    /// UDP: datagrams go to the destination, and replies go back to the source from the
    /// listening socket.
    #[test]
    fn udp_is_forwarded() {
        let server = tuxport_hal::net::socket(AF_INET, SOCK_DGRAM, 0).unwrap();
        tuxport_hal::net::bind(server, &loopback(0)).unwrap();
        let held = tuxport_hal::net::socket(AF_INET, SOCK_DGRAM, 0).unwrap();
        tuxport_hal::net::bind(held, &loopback(0)).unwrap();
        let f = Forwarder::default();
        let _h = f
            .add(
                held,
                Target {
                    udp: true,
                    addr: loopback(port(server)),
                    source: None,
                },
            )
            .unwrap();
        let c = tuxport_hal::net::socket(AF_INET, SOCK_DGRAM, 0).unwrap();
        tuxport_hal::net::sendmsg(c, b"query", &loopback(port(held)), &[], 0).unwrap();
        wait(server, POLLIN);
        let mut buf = [0u8; 64];
        let m = tuxport_hal::net::recvmsg(server, &mut buf, 0, 0).unwrap();
        assert_eq!(&buf[..m.len], b"query");
        tuxport_hal::net::sendmsg(server, b"answer", &m.addr, &[], 0).unwrap();
        wait(c, POLLIN);
        let r = tuxport_hal::net::recvmsg(c, &mut buf, 0, 0).unwrap();
        assert_eq!(&buf[..r.len], b"answer");
        // The reply comes from the port the client sent to
        assert_eq!(r.addr[2..8], loopback(port(held))[2..8]);
        for fd in [server, held, c] {
            tuxport_hal::net::close(fd);
        }
    }
}
