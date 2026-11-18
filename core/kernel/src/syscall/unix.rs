//! AF_UNIX socket syscalls (the data structures are in [`crate::unix`]).
//!
//! [`super::net`] passes calls here when it finds the socket is AF_UNIX. Blocking operations use
//! [`Cx::wait_objects`] to wait for other threads to change a channel's state (they all request a
//! wake of that channel, see [`UnixSocket::wake_objects`]); they can be interrupted by signals and
//! time out according to `SO_RCVTIMEO`/`SO_SNDTIMEO`.

use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use super::fs::{AT_FDCWD, create_parent_at, lookup_at};
use super::net::{Message, SocketRef, epipe, timeval};
use crate::cell::RefCell;
use crate::cred::cap;
use crate::errno::Errno;
use crate::fd::FileRef;
use crate::kernel::{Cx, request_wake_object, wake};
use crate::net::{AF_UNIX, SOCK_DGRAM, SOCK_RAW, SOCK_STREAM, Socket};
use crate::unix::{
    CAPACITY, ChannelRef, Link, Name, Packet, Received, SOCK_SEQPACKET, SOCKADDR_UN_LEN, Ucred,
    UnixSocket, unnamed,
};
use crate::vfs::Node;
use crate::vfs::store::mode;

const SOL_SOCKET: i32 = 1;
const SO_TYPE: i32 = 3;
const SO_ERROR: i32 = 4;
const SO_PASSCRED: i32 = 16;
const SO_PEERCRED: i32 = 17;
const SO_RCVTIMEO: i32 = 20;
const SO_SNDTIMEO: i32 = 21;
const SO_ACCEPTCONN: i32 = 30;
const SO_PROTOCOL: i32 = 38;
const SO_DOMAIN: i32 = 39;

const SCM_RIGHTS: i32 = 1;
const SCM_CREDENTIALS: i32 = 2;
/// The most open files one message passes (Linux's `SCM_MAX_FD`).
const SCM_MAX_FD: usize = 253;

const MSG_PEEK: u32 = 0x2;
const MSG_CTRUNC: u32 = 0x8;
const MSG_TRUNC: u32 = 0x20;
const MSG_DONTWAIT: u32 = 0x40;
const MSG_WAITALL: u32 = 0x100;
const MSG_CMSG_CLOEXEC: u32 = 0x4000_0000;

/// The length of a `cmsghdr` (64-bit).
const CMSG_HDR: usize = 16;
/// The upper limit of the listen backlog (`SOMAXCONN`).
const SOMAXCONN: usize = 4096;

// ---- Creating sockets ----

/// Creates an AF_UNIX socket.
pub(super) fn socket(ty: i32, protocol: i32) -> Result<Socket, Errno> {
    Ok(Socket::Unix(UnixSocket::new(socket_type(ty, protocol)?)))
}

/// socketpair(2): two sockets connected to each other.
pub(super) fn pair(cx: &Cx<'_>, ty: i32, protocol: i32) -> Result<(Socket, Socket), Errno> {
    let (mut a, mut b) = UnixSocket::pair(socket_type(ty, protocol)?, cx.ucred());
    let qlen = max_dgram_qlen(cx);
    a.set_max_qlen(qlen);
    b.set_max_qlen(qlen);
    let net = cx.task.ns.borrow().net.clone();
    if net.host_net.is_some() {
        a.net = Some(net.clone());
        b.net = Some(net);
    }
    Ok((Socket::Unix(a), Socket::Unix(b)))
}

/// `net.unix.max_dgram_qlen` of the caller's network namespace, which a datagram socket takes when
/// created (Linux's `unix_create1`).
pub(super) fn max_dgram_qlen(cx: &Cx<'_>) -> i64 {
    let net = cx.task.ns.borrow().net.clone();
    let conf = &net.devs.borrow().conf;
    crate::netconf::sysctl_read(conf, "unix/max_dgram_qlen")
        .trim()
        .parse()
        .unwrap_or(crate::unix::MAX_DGRAM_QLEN as i64)
}

/// Checks the socket type and protocol and returns the actual type.
fn socket_type(ty: i32, protocol: i32) -> Result<i32, Errno> {
    if protocol != 0 && protocol != AF_UNIX {
        return Err(Errno::EPROTONOSUPPORT);
    }
    match ty {
        SOCK_STREAM | SOCK_DGRAM | SOCK_SEQPACKET => Ok(ty),
        // As on Linux, SOCK_RAW is treated as datagram
        SOCK_RAW => Ok(SOCK_DGRAM),
        _ => Err(Errno::ESOCKTNOSUPPORT),
    }
}

// ---- Names and connections ----

pub(super) fn bind(cx: &mut Cx<'_>, sock: &SocketRef, addr: &[u8]) -> Result<(), Errno> {
    if with(sock, |u| !u.addr.is_empty()) {
        return Err(Errno::EINVAL);
    }
    let mut bound = unnamed();
    let (net, ty) = with(sock, |u| (u.net_id(), u.ty));
    match parse(addr)? {
        Addr::Unnamed => {
            let path = cx
                .k()
                .unix
                .autobind(net, ty, sock)
                .ok_or(Errno::EADDRINUSE)?;
            bound.extend_from_slice(&path);
        }
        Addr::Abstract(p) => {
            if !cx
                .k()
                .unix
                .insert(Name::Abstract(net, ty, p.to_vec()), sock)
            {
                return Err(Errno::EADDRINUSE);
            }
            bound.extend_from_slice(p);
        }
        Addr::Path(p) => {
            // As on Linux, create a socket node in the file system; fail when it already
            // exists. When the path is / or ends with ., it refers to an existing directory
            let in_use = |e: Errno| {
                if e == Errno::EEXIST {
                    Errno::EADDRINUSE
                } else {
                    e
                }
            };
            let (mnt, dir, name, ovl) = create_parent_at(cx, AT_FDCWD, p).map_err(in_use)?;
            if name == b"." || name == b".." {
                return Err(Errno::EADDRINUSE);
            }
            let vfs = cx.vfs()?;
            let cred = cx.cred();
            let dir_st = super::fs::may_create(&vfs, &cred, (mnt, dir), name).map_err(in_use)?;
            let (uid, gid, perm) = cred.new_owner(&dir_st, 0o777, cx.umask(), false);
            let tmpfs = super::fs::tmpfs_reserve(&vfs, (mnt, dir), 0)?;
            let ino = super::fs::create_in(
                &vfs,
                (mnt, dir),
                name,
                || {
                    vfs.store()
                        .mknod(dir, name, mode::S_IFSOCK | perm, 0, uid, gid)
                },
                |&ino| ino,
            )
            .map_err(in_use)?;
            super::fs::tmpfs_created(&vfs, tmpfs, 0);
            super::fs::created_events(cx.kr(), (dir, ovl), name, 0);
            cx.k().unix.insert(Name::Path(ino), sock);
            bound.extend_from_slice(p);
            bound.push(0);
        }
    }
    with(sock, |u| u.addr = bound);
    Ok(())
}

pub(super) fn listen(u: &mut UnixSocket, backlog: i32, cred: Ucred) -> Result<(), Errno> {
    let backlog = usize::try_from(backlog).map_or(SOMAXCONN, |b| b.min(SOMAXCONN));
    if u.ty == SOCK_DGRAM {
        return Err(Errno::EOPNOTSUPP);
    }
    if u.addr.is_empty() {
        return Err(Errno::EINVAL);
    }
    match &mut u.link {
        Link::None => {
            u.link = Link::Listening {
                backlog,
                pending: Default::default(),
                cred,
            };
            Ok(())
        }
        // unix_listen: every listen records the credentials again, and a larger backlog wakes the
        // connectors waiting for room
        Link::Listening {
            backlog: b,
            cred: c,
            ..
        } => {
            if backlog > *b {
                request_wake_object(wake::of(&u.rx));
            }
            *b = backlog;
            *c = cred;
            Ok(())
        }
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn accept(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
) -> Result<(SocketRef, Vec<u8>), Errno> {
    let (timeout, queue) = with(sock, |u| (u.rcvtimeo, u.rx.clone()));
    let deadline = deadline(timeout);
    loop {
        let new = with(sock, |u| match &mut u.link {
            Link::Listening { pending, .. } => Ok(pending.pop_front()),
            _ => Err(Errno::EINVAL),
        })?;
        if let Some(new) = new {
            with(&new, |u| u.embryo = false);
            // The listener's SO_PASSCRED carries over (unix_sock_inherit_flags)
            if with(sock, |u| u.passcred) {
                with(&new, |u| u.passcred = true);
            }
            // Connects waiting for room in the backlog can go on now
            request_wake_object(wake::of(&queue));
            let peer = with(&new, |u| u.peer_name()).unwrap_or_else(unnamed);
            return Ok((new, peer));
        }
        block(cx, nonblock, deadline, &queue)?;
    }
}

pub(super) fn connect(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
    addr: &[u8],
) -> Result<(), Errno> {
    let (ty, timeout) = with(sock, |u| (u.ty, u.sndtimeo));
    if ty == SOCK_DGRAM {
        return connect_dgram(cx, sock, addr);
    }
    let cred = cx.ucred();
    let deadline = deadline(timeout);
    loop {
        with(sock, |u| match u.link {
            Link::None => Ok(()),
            Link::Listening { .. } => Err(Errno::EINVAL),
            _ => Err(Errno::EISCONN),
        })?;
        let target = find(cx, sock, addr)?;
        if Rc::ptr_eq(&target, sock) {
            return Err(Errno::ECONNREFUSED);
        }
        let queue = {
            let mut t = target.borrow_mut();
            let Socket::Unix(server) = &mut *t else {
                return Err(Errno::ECONNREFUSED);
            };
            if server.ty != ty {
                return Err(Errno::EPROTOTYPE);
            }
            let Link::Listening {
                backlog,
                pending,
                cred: server_cred,
            } = &mut server.link
            else {
                return Err(Errno::ECONNREFUSED);
            };
            // As on Linux, at most backlog + 1 connections wait for accept
            if pending.len() <= *backlog {
                let (end, own) = with(sock, |u| {
                    (
                        u.connect_stream(&server.addr, *server_cred, cred),
                        wake::of(&u.rx),
                    )
                });
                pending.push_back(Rc::new(RefCell::new(Socket::Unix(end))));
                server.connections += 1;
                request_wake_object(wake::of(&server.rx));
                // Connected: threads polling this socket registered its own receive queue
                request_wake_object(own);
                return Ok(());
            }
            server.rx.clone()
        };
        block(cx, nonblock, deadline, &queue)?;
    }
}

fn connect_dgram(cx: &mut Cx<'_>, sock: &SocketRef, addr: &[u8]) -> Result<(), Errno> {
    // AF_UNSPEC: disconnect
    if addr.len() >= 2 && addr[..2] == [0, 0] {
        with(sock, |u| u.set_dgram_link(Link::None));
        return Ok(());
    }
    let target = find(cx, sock, addr)?;
    let own_rx = with(sock, |u| Rc::downgrade(&u.rx));
    let link = match &*target.borrow() {
        // unix_may_send: a peer connected to another socket takes nothing from this one
        Socket::Unix(p) if p.ty == SOCK_DGRAM => match &p.link {
            Link::Dgram { tx, .. } if !Weak::ptr_eq(tx, &own_rx) => return Err(Errno::EPERM),
            _ => Link::Dgram {
                tx: Rc::downgrade(&p.rx),
                peer_addr: p.name(),
            },
        },
        Socket::Unix(_) => return Err(Errno::EPROTOTYPE),
        _ => return Err(Errno::ECONNREFUSED),
    };
    with(sock, |u| u.set_dgram_link(link));
    Ok(())
}

// ---- Sending and receiving ----

pub(super) fn send(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
    data: &[u8],
    addr: &[u8],
    control: &[u8],
    flags: u32,
) -> Result<usize, Errno> {
    let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
    let (mut files, declared) = rights(cx, control)?;
    // Without a declaration it is the sender itself (Linux's maybe_add_creds uses the real user ID
    // and group ID)
    let cred = declared.unwrap_or_else(|| cx.scm_cred());
    let (ty, timeout, from) = with(sock, |u| (u.ty, u.sndtimeo, u.addr.clone()));
    let deadline = deadline(timeout);
    if ty == SOCK_DGRAM {
        return send_dgram(cx, sock, nonblock, deadline, data, addr, files, cred);
    }
    // A stream socket takes no destination (unix_stream_sendmsg); seqpacket ignores it
    if ty == SOCK_STREAM && !addr.is_empty() {
        let connected = with(sock, |u| matches!(u.link, Link::Stream { .. }));
        return Err(if connected {
            Errno::EISCONN
        } else {
            Errno::EOPNOTSUPP
        });
    }
    let mut done = 0;
    loop {
        let tx = with(sock, |u| match &u.link {
            Link::Stream { tx, .. } => Ok(tx.clone()),
            _ => Err(Errno::ENOTCONN),
        })?;
        {
            let mut c = tx.borrow_mut();
            if c.eof || c.closed {
                drop(c);
                let e = epipe(cx, flags);
                return if done > 0 { Ok(done) } else { Err(e) };
            }
            if ty == SOCK_SEQPACKET {
                if data.len() > CAPACITY {
                    return Err(Errno::EMSGSIZE);
                }
                if c.space() >= data.len() {
                    let inflight = !files.is_empty();
                    c.push(packet(data, files, cred, from));
                    if inflight {
                        crate::unix::note_inflight(cx.k(), &tx);
                    }
                    return Ok(data.len());
                }
            } else {
                if data.is_empty() && files.is_empty() {
                    return Ok(0);
                }
                // A stream socket may queue only part of it; open files go with the first part
                let n = c.space().min(data.len() - done);
                if n > 0 || data.is_empty() {
                    if !files.is_empty() {
                        crate::unix::note_inflight(cx.k(), &tx);
                    }
                    // The receiver's recvfrom gets the sender's address, as bound when sent
                    c.push(packet(
                        &data[done..done + n],
                        std::mem::take(&mut files),
                        cred,
                        from.clone(),
                    ));
                    done += n;
                    if done == data.len() {
                        return Ok(done);
                    }
                    continue;
                }
            }
        }
        if let Err(e) = block(cx, nonblock, deadline, &tx) {
            return if done > 0 { Ok(done) } else { Err(e) };
        }
    }
}

/// Parses sendmsg's control messages, taking out the open files `SCM_RIGHTS` passes and the
/// credentials `SCM_CREDENTIALS` declares (kernel IDs, global pid).
fn rights(cx: &Cx<'_>, control: &[u8]) -> Result<(Vec<FileRef>, Option<Ucred>), Errno> {
    let mut files = Vec::new();
    let mut declared = None;
    let mut off = 0;
    while off + CMSG_HDR <= control.len() {
        let h = &control[off..];
        let len = u64::from_le_bytes(h[..8].try_into().unwrap()) as usize;
        let level = i32::from_le_bytes(h[8..12].try_into().unwrap());
        let ty = i32::from_le_bytes(h[12..16].try_into().unwrap());
        if len < CMSG_HDR || len > h.len() {
            return Err(Errno::EINVAL);
        }
        match (level, ty) {
            (SOL_SOCKET, SCM_RIGHTS) => {
                let n = (len - CMSG_HDR) / 4;
                if files.len() + n > SCM_MAX_FD {
                    return Err(Errno::EINVAL);
                }
                for fd in h[CMSG_HDR..CMSG_HDR + n * 4].chunks_exact(4) {
                    files.push(cx.fds().get(i32::from_le_bytes(fd.try_into().unwrap()))?);
                }
            }
            (SOL_SOCKET, SCM_CREDENTIALS) => {
                if len != CMSG_HDR + 12 {
                    return Err(Errno::EINVAL);
                }
                let word = |i: usize| {
                    u32::from_le_bytes(
                        h[CMSG_HDR + i * 4..CMSG_HDR + i * 4 + 4]
                            .try_into()
                            .unwrap(),
                    )
                };
                declared = Some(check_creds(cx, word(0) as i32, word(1), word(2))?);
            }
            _ => return Err(Errno::EINVAL),
        }
        off += len.next_multiple_of(8);
    }
    Ok((files, declared))
}

/// The credentials the sender declares in `SCM_CREDENTIALS` (Linux's `scm_check_creds` and
/// `__scm_send`): the IDs must have a mapping in the sender's own namespace (EINVAL); the pid must
/// be the sender's own, or the sender must have CAP_SYS_ADMIN over the owner of its pid namespace;
/// the user ID must be one of its real, effective and saved ones, or it must have CAP_SETUID, and
/// likewise for the group ID (CAP_SETGID), otherwise EPERM; a pid that does not exist is ESRCH.
fn check_creds(cx: &Cx<'_>, pid: i32, uid: u32, gid: u32) -> Result<Ucred, Errno> {
    let c = cx.cred();
    let (Some(kuid), Some(kgid)) = (c.uid_in(uid), c.gid_in(gid)) else {
        return Err(Errno::EINVAL);
    };
    let pid_ok = pid == cx.pid_out(cx.tgid()) || c.ns_capable(&cx.pid_ns().owner, cap::SYS_ADMIN);
    let uid_ok = [c.ruid, c.euid, c.suid].contains(&kuid) || c.capable_here(cap::SETUID);
    let gid_ok = [c.rgid, c.egid, c.sgid].contains(&kgid) || c.capable_here(cap::SETGID);
    if !(pid_ok && uid_ok && gid_ok) {
        return Err(Errno::EPERM);
    }
    let global = cx.pid_in(pid);
    if pid <= 0 || !cx.kr().tasks.contains_key(&global) {
        return Err(Errno::ESRCH);
    }
    Ok(Ucred::of(global, kuid, kgid))
}

#[allow(clippy::too_many_arguments)]
fn send_dgram(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
    deadline: Option<Instant>,
    data: &[u8],
    addr: &[u8],
    files: Vec<FileRef>,
    cred: Ucred,
) -> Result<usize, Errno> {
    let (own_rx, from, connected, shut_wr) = with(sock, |u| {
        let connected = match &u.link {
            Link::Dgram { tx, .. } => Some(tx.clone()),
            _ => None,
        };
        (Rc::downgrade(&u.rx), u.addr.clone(), connected, u.shut_wr)
    });
    // Datagram sends never raise SIGPIPE (only unix_stream_sendmsg sends it)
    if shut_wr {
        return Err(Errno::EPIPE);
    }
    // unix_dgram_sendmsg: sending to a dead connected peer disconnects the socket, ECONNREFUSED
    // this time and ENOTCONN from then on
    let disconnect = |tx: &Weak<_>| {
        with(sock, |u| {
            if matches!(&u.link, Link::Dgram { tx: t, .. } if Weak::ptr_eq(t, tx)) {
                u.set_dgram_link(Link::None);
            }
        });
        Errno::ECONNREFUSED
    };
    let rx: ChannelRef = if let Some(tx) = connected.as_ref().filter(|_| addr.is_empty()) {
        tx.upgrade().ok_or_else(|| disconnect(tx))?
    } else if addr.is_empty() {
        return Err(Errno::ENOTCONN);
    } else {
        let target = find(cx, sock, addr)?;
        let t = target.borrow();
        let Socket::Unix(p) = &*t else {
            return Err(Errno::ECONNREFUSED);
        };
        if p.ty != SOCK_DGRAM {
            return Err(Errno::EPROTOTYPE);
        }
        // When the peer is connected to another socket it receives only datagrams from that socket
        if let Link::Dgram { tx, .. } = &p.link
            && !Weak::ptr_eq(tx, &own_rx)
        {
            return Err(Errno::EPERM);
        }
        p.rx.clone()
    };
    if data.len() > CAPACITY {
        return Err(Errno::EMSGSIZE);
    }
    let mut files = Some(files);
    loop {
        {
            let mut c = rx.borrow_mut();
            if c.closed {
                drop(c);
                return Err(match connected.as_ref().filter(|_| addr.is_empty()) {
                    Some(tx) => disconnect(tx),
                    None => Errno::ECONNREFUSED,
                });
            }
            // The receiver has shut down reading (RCV_SHUTDOWN): EPIPE, without SIGPIPE
            if c.eof {
                return Err(Errno::EPIPE);
            }
            // Room for the bytes, and unless the receiver is connected back to this socket, for
            // one more datagram (unix_dgram_sendmsg's unix_recvq_full wait)
            if c.space() >= data.len() && !c.holds_back(&own_rx) {
                let files = files.take().unwrap_or_default();
                if !files.is_empty() {
                    crate::unix::note_inflight(cx.k(), &rx);
                }
                c.push(packet(data, files, cred, from));
                return Ok(data.len());
            }
        }
        block(cx, nonblock, deadline, &rx)?;
    }
}

fn packet(data: &[u8], fds: Vec<FileRef>, cred: Ucred, from: Vec<u8>) -> Packet {
    Packet {
        data: data.to_vec(),
        off: 0,
        fds,
        cred,
        from,
    }
}

pub(super) fn recv(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
    max: usize,
    control_len: usize,
    flags: u32,
) -> Result<Message, Errno> {
    let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
    let peek = flags & MSG_PEEK != 0;
    let (ty, timeout, passcred) = with(sock, |u| (u.ty, u.rcvtimeo, u.passcred));
    let deadline = deadline(timeout);
    let wait_all = flags & MSG_WAITALL != 0 && ty == SOCK_STREAM && !peek;
    // The part already read under MSG_WAITALL
    let mut part: Option<Received> = None;
    loop {
        let rx = with(sock, |u| match &u.link {
            Link::Stream { rx, .. } => Ok(rx.clone()),
            Link::Listening { .. } => Err(Errno::EINVAL),
            Link::None if u.ty != SOCK_DGRAM => Err(Errno::ENOTCONN),
            _ => Ok(u.rx.clone()),
        })?;
        let (got, eof, reset) = {
            let mut c = rx.borrow_mut();
            // A connection reset by the peer (Linux's sk_err): a seqpacket socket reports it
            // before what is queued (__skb_try_recv_datagram), a stream socket once it has read
            // everything (unix_stream_read_generic)
            if ty == SOCK_SEQPACKET && c.take_reset() {
                return Err(Errno::ECONNRESET);
            }
            let room = max - part.as_ref().map_or(0, |p| p.data.len());
            let got = if ty == SOCK_STREAM {
                // A read of nothing returns 0 at once when something is queued, taking the first
                // message's files (Linux's unix_stream_read_generic with size 0)
                let queued = !c.packets.is_empty();
                Some(c.read_stream(room, peek))
                    .filter(|r| !r.data.is_empty() || !r.fds.is_empty() || (room == 0 && queued))
            } else {
                c.read_packet(room, peek)
            };
            let reset = got.is_none() && c.take_reset();
            (got, c.eof, reset)
        };
        if let Some(r) = got {
            let r = match part.take() {
                Some(mut p) => {
                    p.data.extend_from_slice(&r.data);
                    p.len = p.data.len();
                    p.fds.extend(r.fds);
                    p
                }
                None => r,
            };
            if !wait_all || r.data.len() >= max || !r.fds.is_empty() {
                return Ok(deliver(cx, r, passcred, control_len, flags));
            }
            part = Some(r);
            continue;
        }
        if reset {
            return match part.take() {
                Some(r) => Ok(deliver(cx, r, passcred, control_len, flags)),
                None => Err(Errno::ECONNRESET),
            };
        }
        if eof {
            return Ok(match part.take() {
                Some(r) => deliver(cx, r, passcred, control_len, flags),
                None => Message {
                    data: Vec::new(),
                    len: 0,
                    addr: Vec::new(),
                    control: Vec::new(),
                    flags: 0,
                },
            });
        }
        if let Err(e) = block(cx, nonblock, deadline, &rx) {
            return match part.take() {
                Some(r) => Ok(deliver(cx, r, passcred, control_len, flags)),
                None => Err(e),
            };
        }
    }
}

/// Hands the data read to the guest: attaches the credentials and installs the passed open files
/// into the current process's fd table, as Linux's `scm_recv` fills a control buffer of
/// `control_len` bytes: the credentials cut to what fits (`put_cmsg`), then as many files as fit
/// after a header (`scm_detach_fds`); what does not fit is dropped and `MSG_CTRUNC` is set.
fn deliver(
    cx: &mut Cx<'_>,
    r: Received,
    passcred: bool,
    control_len: usize,
    flags: u32,
) -> Message {
    let mut control = Vec::new();
    let mut room = control_len;
    let mut out_flags = if r.len > r.data.len() { MSG_TRUNC } else { 0 };
    if passcred {
        // pid and IDs as the receiver sees them (Linux's scm_recv uses pid_vnr, from_kuid_munged)
        let me = cx.cred();
        let cred = Ucred {
            pid: cx.pid_out(r.cred.pid),
            uid: me.uid_out(r.cred.uid),
            gid: me.gid_out(r.cred.gid),
        };
        let data = cred.encode();
        super::net::put_cmsg(
            &mut control,
            &mut room,
            &mut out_flags,
            SOL_SOCKET,
            SCM_CREDENTIALS,
            &data,
        );
    }
    if !r.fds.is_empty() {
        let fit = room.saturating_sub(CMSG_HDR) / 4;
        let cloexec = flags & MSG_CMSG_CLOEXEC != 0;
        let mut fds = Vec::new();
        let total = r.fds.len();
        for file in r.fds.into_iter().take(fit) {
            match cx.fds_mut().install(file, cloexec, 0) {
                Ok(fd) => fds.extend_from_slice(&fd.to_le_bytes()),
                Err(_) => break,
            }
        }
        if fds.len() / 4 < total {
            out_flags |= MSG_CTRUNC;
        }
        if !fds.is_empty() {
            super::net::put_cmsg(
                &mut control,
                &mut room,
                &mut out_flags,
                SOL_SOCKET,
                SCM_RIGHTS,
                &fds,
            );
        }
    }
    Message {
        data: r.data,
        len: r.len,
        addr: r.from,
        control,
        flags: out_flags,
    }
}

/// The control buffer that would carry `control` cannot be written: the files passed in it,
/// already installed, are closed again (Linux installs a file only once its number is written,
/// `receive_fd_user`).
pub(super) fn drop_passed_files(cx: &mut Cx<'_>, control: &[u8]) {
    let mut off = 0;
    while let Some(h) = control.get(off..off + CMSG_HDR) {
        let len = u64::from_le_bytes(h[..8].try_into().unwrap()) as usize;
        let level = i32::from_le_bytes(h[8..12].try_into().unwrap());
        let ty = i32::from_le_bytes(h[12..16].try_into().unwrap());
        if len < CMSG_HDR {
            break;
        }
        if (level, ty) == (SOL_SOCKET, SCM_RIGHTS) {
            let data = control.get(off + CMSG_HDR..off + len).unwrap_or_default();
            for fd in data.chunks_exact(4) {
                let _ = cx
                    .fds_mut()
                    .close(i32::from_le_bytes(fd.try_into().unwrap()));
            }
        }
        off += len.next_multiple_of(8);
    }
}

// ---- Options and shutdown ----

/// `privileged` is (`CAP_NET_ADMIN` in the initial user namespace, whether a priority outside
/// 0..=6 is allowed), for [`SockOpts::set`](crate::net::SockOpts::set).
pub(super) fn setsockopt(
    u: &mut UnixSocket,
    level: i32,
    name: i32,
    value: &[u8],
    (admin, net_admin): (bool, bool),
) -> Result<(), Errno> {
    if level != SOL_SOCKET {
        return Err(Errno::EOPNOTSUPP);
    }
    match name {
        SO_PASSCRED => u.passcred = value.get(..4).ok_or(Errno::EINVAL)? != [0; 4],
        SO_RCVTIMEO => u.rcvtimeo = timeval(value)?,
        SO_SNDTIMEO => u.sndtimeo = timeval(value)?,
        // Buffer sizes, lingering and the like are only recorded; other options are accepted
        // without effect
        _ => {
            if let Some(r) = u.opts.set(name, value, admin, net_admin) {
                r?;
            }
        }
    }
    Ok(())
}

/// `pid_ns` and `cred` are the caller's pid namespace and credentials: `SO_PEERCRED`'s pid and IDs
/// are translated by them (Linux's `cred_to_ucred`).
pub(super) fn getsockopt(
    u: &UnixSocket,
    level: i32,
    name: i32,
    pid_ns: &crate::pidns::PidNs,
    cred: &crate::cred::Cred,
) -> Result<Vec<u8>, Errno> {
    if level != SOL_SOCKET {
        return Err(Errno::EOPNOTSUPP);
    }
    let int = |v: i32| v.to_le_bytes().to_vec();
    Ok(match name {
        SO_TYPE => int(u.ty),
        SO_PROTOCOL => int(0),
        // A connection reset by the peer, reported once (sock_error)
        SO_ERROR => match &u.link {
            Link::Stream { rx, .. } if rx.borrow_mut().take_reset() => int(Errno::ECONNRESET.0),
            _ => int(0),
        },
        SO_DOMAIN => int(AF_UNIX),
        SO_PASSCRED => int(u.passcred as i32),
        SO_ACCEPTCONN => int(matches!(u.link, Link::Listening { .. }) as i32),
        SO_RCVTIMEO => encode_timeval(u.rcvtimeo),
        SO_SNDTIMEO => encode_timeval(u.sndtimeo),
        // A listener's are its own, recorded at listen; without a peer, as Linux's cred_to_ucred
        // without credentials: pid 0, uid and gid -1
        SO_PEERCRED => match &u.link {
            Link::Listening { cred, .. } => Some(*cred),
            _ => u.peer_cred,
        }
        .map(|c| Ucred {
            pid: pid_ns.vnr(c.pid).unwrap_or(0),
            uid: cred.uid_out(c.uid),
            gid: cred.gid_out(c.gid),
        })
        .unwrap_or(Ucred {
            pid: 0,
            uid: u32::MAX,
            gid: u32::MAX,
        })
        .encode()
        .to_vec(),
        _ => return u.opts.get(name).ok_or(Errno::ENOPROTOOPT),
    })
}

fn encode_timeval(t: Option<Duration>) -> Vec<u8> {
    let t = t.unwrap_or_default();
    let mut b = (t.as_secs() as i64).to_le_bytes().to_vec();
    b.extend_from_slice(&(t.subsec_micros() as i64).to_le_bytes());
    b
}

pub(super) fn sockname(u: &UnixSocket, peer: bool) -> Result<Vec<u8>, Errno> {
    if peer {
        u.peer_name().ok_or(Errno::ENOTCONN)
    } else {
        Ok(u.name())
    }
}

pub(super) fn shutdown(u: &mut UnixSocket, how: i32) -> Result<(), Errno> {
    // Linux 6.1's unix_shutdown records the flags on any socket, connected or not
    if !(0..=2).contains(&how) {
        return Err(Errno::EINVAL);
    }
    u.shutdown(how);
    Ok(())
}

// ---- Addresses and waiting ----

/// An address given by the guest.
enum Addr<'a> {
    /// Only the address family: bind autobinds.
    Unnamed,
    /// A path (without the trailing NUL).
    Path(&'a [u8]),
    /// An abstract address: all bytes of `sun_path`.
    Abstract(&'a [u8]),
}

/// Finds the socket bound at an address (abstract addresses are looked up in the network
/// namespace of `sock`).
fn find(cx: &Cx<'_>, sock: &SocketRef, addr: &[u8]) -> Result<SocketRef, Errno> {
    let name = match parse(addr)? {
        Addr::Unnamed => return Err(Errno::EINVAL),
        Addr::Abstract(p) => {
            let (net, ty) = with(sock, |u| (u.net_id(), u.ty));
            Name::Abstract(net, ty, p.to_vec())
        }
        Addr::Path(p) => {
            let node = lookup_at(cx, AT_FDCWD, p, true)?;
            let vfs = cx.vfs()?;
            match node {
                Node::Store(..) | Node::Overlay(..)
                    if vfs.kind(&cx.view(&vfs), &node)? == mode::S_IFSOCK =>
                {
                    // The socket file behind an overlay's node (in the upper layer when it was
                    // bound through the overlay)
                    let ino = match node {
                        Node::Overlay(_, o) => vfs.overlay().node(o).real(),
                        _ => node.ino().unwrap_or(0),
                    };
                    // Connecting to a path address requires write permission on the socket file
                    cx.cred()
                        .permission(&vfs.store().stat(ino)?, crate::cred::MAY_WRITE)?;
                    Name::Path(ino)
                }
                _ => return Err(Errno::ECONNREFUSED),
            }
        }
    };
    cx.kr().unix.lookup(&name).ok_or(Errno::ECONNREFUSED)
}

fn parse(addr: &[u8]) -> Result<Addr<'_>, Errno> {
    if addr.len() < 2 || addr.len() > SOCKADDR_UN_LEN {
        return Err(Errno::EINVAL);
    }
    if u16::from_le_bytes([addr[0], addr[1]]) as i32 != AF_UNIX {
        return Err(Errno::EINVAL);
    }
    let path = &addr[2..];
    Ok(match path.first() {
        None => Addr::Unnamed,
        Some(0) => Addr::Abstract(path),
        Some(_) => Addr::Path(&path[..path.iter().position(|&b| b == 0).unwrap_or(path.len())]),
    })
}

/// Borrows the internal state of an AF_UNIX socket.
fn with<R>(sock: &SocketRef, f: impl FnOnce(&mut UnixSocket) -> R) -> R {
    match &mut *sock.borrow_mut() {
        Socket::Unix(u) => f(u),
        _ => unreachable!("not an AF_UNIX socket"),
    }
}

fn deadline(timeout: Option<Duration>) -> Option<Instant> {
    timeout.map(|t| Instant::now() + t)
}

/// The common part of blocking operations: EAGAIN when nonblocking or timed out; on a signal,
/// EINTR when `SO_RCVTIMEO`/`SO_SNDTIMEO` set a deadline and ERESTARTSYS otherwise (Linux's
/// `sock_intr_errno`), so a timed wait is never restarted with its whole timeout again; otherwise
/// wait until some thread changes the state of channel `on` (a listening socket's connection
/// queue is recorded on its receive queue).
fn block(
    cx: &mut Cx<'_>,
    nonblock: bool,
    deadline: Option<Instant>,
    on: &ChannelRef,
) -> Result<(), Errno> {
    if nonblock || deadline.is_some_and(|d| Instant::now() >= d) {
        return Err(Errno::EAGAIN);
    }
    if cx.interrupted() {
        return Err(if deadline.is_some() {
            Errno::EINTR
        } else {
            Errno::ERESTARTSYS
        });
    }
    cx.wait_objects(deadline, &mut [], wake::FILE, &[wake::of(on)])
}
