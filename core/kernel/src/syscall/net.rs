//! Socket syscalls (the kernel/net map; the `netlink_*` functions, the netlink section and the
//! `Socket::Netlink` arms are the kernel/netlink map).
//!
//! Host sockets are nonblocking; on EAGAIN a blocking guest call waits for the host fd to become
//! ready through [`Cx::wait`], releasing the BKL meanwhile; it can be interrupted by signals and
//! times out according to `SO_RCVTIMEO`/`SO_SNDTIMEO`.

use std::rc::Rc;
use std::time::{Duration, Instant};

use super::{SysResult, get, put, unix};
use crate::cell::RefCell;
use crate::errno::Errno;
use crate::fd::{FileKind, FileRef, OpenFile, oflags};
use crate::kernel::{Cx, request_wake_object, wake};
use crate::net::{
    AF_INET, AF_INET6, AF_NETLINK, AF_UNIX, InetSocket, NETLINK_GENERIC, NETLINK_KOBJECT_UEVENT,
    NETLINK_NETFILTER, NETLINK_ROUTE, NETLINK_SOCK_DIAG, NetlinkSocket, SOCK_DGRAM, SOCK_RAW,
    SOCK_STREAM, Socket, is_stub, servfail, stub_addr,
};
use crate::process::Signal;
use crate::signal::{self, SigInfo};
use crate::uaccess::UserSlice;

/// A socket and the open file it is in.
pub(crate) type SocketRef = Rc<RefCell<Socket>>;

const SOCK_TYPE_MASK: u32 = 0xf;
const SOCK_NONBLOCK: u32 = oflags::O_NONBLOCK;
const SOCK_CLOEXEC: u32 = oflags::O_CLOEXEC;
const IPPROTO_ICMP: i32 = 1;
const IPPROTO_TCP: i32 = 6;
const IPPROTO_IPV6: i32 = 41;
const IPV6_V6ONLY: i32 = 26;
const IPPROTO_UDP: i32 = 17;
const IPPROTO_ICMPV6: i32 = 58;
const IPPROTO_UDPLITE: i32 = 136;
const IPPROTO_MPTCP: i32 = 262;

const SOL_SOCKET: i32 = 1;
/// `SOL_NETLINK` and its options.
const SOL_NETLINK: i32 = 270;
const NETLINK_ADD_MEMBERSHIP: i32 = 1;
const NETLINK_DROP_MEMBERSHIP: i32 = 2;
const NETLINK_PKTINFO: i32 = 3;
const NETLINK_BROADCAST_ERROR: i32 = 4;
const NETLINK_NO_ENOBUFS: i32 = 5;
const NETLINK_LISTEN_ALL_NSID: i32 = 8;
const NETLINK_LIST_MEMBERSHIPS: i32 = 9;
const NETLINK_CAP_ACK: i32 = 10;
const NETLINK_EXT_ACK: i32 = 11;
const NETLINK_GET_STRICT_CHK: i32 = 12;
const SCM_CREDENTIALS: i32 = 2;
const SO_TYPE: i32 = 3;
const SO_RCVTIMEO: i32 = 20;
const SO_SNDTIMEO: i32 = 21;
const SO_ERROR: i32 = 4;
const SO_MARK: i32 = 36;
const SO_ATTACH_FILTER: i32 = 26;
/// getsockopt's name for `SO_ATTACH_FILTER`.
const SO_GET_FILTER: i32 = SO_ATTACH_FILTER;
const SO_DETACH_FILTER: i32 = 27;
const SO_LOCK_FILTER: i32 = 44;
/// The most instructions of a classic BPF program (`BPF_MAXINSNS`).
const BPF_MAXINSNS: usize = 4096;

const MSG_PEEK: u32 = 0x2;
const MSG_CTRUNC: u32 = 0x8;
const MSG_TRUNC: u32 = 0x20;
const MSG_DONTWAIT: u32 = 0x40;
const MSG_ERRQUEUE: u32 = 0x2000;
const MSG_WAITALL: u32 = 0x100;
const MSG_NOSIGNAL: u32 = 0x4000;
const MSG_FASTOPEN: u32 = 0x2000_0000;

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;

/// The largest sockaddr length (`sockaddr_storage`).
const SOCKADDR_MAX: usize = 128;
/// The most bytes per send or receive.
const MSG_MAX: usize = 1 << 20;

// ---- Creating sockets ----

pub(super) fn socket(cx: &mut Cx<'_>, domain: i32, ty: u32, protocol: i32) -> SysResult {
    if ty & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let base = (ty & SOCK_TYPE_MASK) as i32;
    // A socket created in a new network namespace records it (crate::netns)
    let net = cx.task.ns.borrow().net.clone();
    let owner = net.owner.clone();
    let net = net.host_net.is_some().then_some(net);
    let sock = match domain {
        AF_INET | AF_INET6 => {
            // A raw socket needs CAP_NET_RAW over the network namespace's owner (Linux's
            // inet_create: a wrong protocol number is reported first, left to the host). The raw
            // ICMP emulated with a ping socket on the host needs no privilege, so without this
            // check ordinary users could create one too, and busybox's ping would not fall back
            // to a ping socket
            let icmp = matches!(
                (domain, protocol),
                (AF_INET, IPPROTO_ICMP) | (AF_INET6, IPPROTO_ICMPV6)
            );
            // Other raw sockets cannot be created, as for apps on Android: even when the host has
            // CAP_NET_RAW (a Linux host for development) they are not handed to it, since
            // otherwise the ICMP caused by the packets they send would not reach the emulated raw
            // ICMP sockets, and programs like mtr that send on a raw socket and receive on a raw
            // ICMP socket would not match up (on EPERM they fall back to ping sockets)
            if base == SOCK_RAW
                && (1..256).contains(&protocol)
                && (!icmp || !cx.cred().ns_capable(&owner, crate::cred::cap::NET_RAW))
            {
                return Err(Errno::EPERM);
            }
            // The guest's raw ICMP socket becomes an unprivileged ping socket
            let host_ty = if base == SOCK_RAW && icmp {
                SOCK_DGRAM
            } else {
                base
            };
            let fd = tuxport_hal::net::socket(domain, host_ty, protocol).map_err(Errno)?;
            let mut s = InetSocket::new(fd, domain, host_ty);
            s.raw = base == SOCK_RAW;
            if s.raw {
                // Enable IP_RECVERR and the like on the host for it (crate::rawicmp); everything
                // received goes through the aggregation point
                for &(level, name) in crate::rawicmp::RawIcmp::host_options(domain) {
                    let _ = tuxport_hal::net::setsockopt(fd, level, name, &1i32.to_le_bytes());
                }
                let tap = crate::rawicmp::tap_for(cx.k(), &net)?;
                s.icmp = Some(crate::rawicmp::RawIcmp::new(domain, tap));
                // The timer thread watches aggregation points for waiters in poll and epoll: this
                // one may be new, so wake it
                crate::kernel::signal_later(&cx.shared.timers);
            }
            if protocol != 0 {
                s.proto = protocol;
            }
            s.net = net;
            Socket::Inet(s)
        }
        AF_NETLINK if base == SOCK_RAW || base == SOCK_DGRAM => {
            // NETLINK_ROUTE answers queries for interfaces and addresses, NETLINK_GENERIC has
            // only the nlctrl family (which Linux always has), NETLINK_NETFILTER is a subset of
            // nftables (crate::nft), NETLINK_SOCK_DIAG refuses every query as without its diag
            // modules (ss then reads /proc/net); NETLINK_KOBJECT_UEVENT is only for listening to
            // hotplug events (mdev, udev), and there are none here. Other protocols are
            // EPROTONOSUPPORT, as for subsystems not built into a Linux kernel: before, they were
            // created anyway and never answered requests, so libaudit waited 40 seconds for an
            // ACK and failed, and PAM then refused logins (Debian's sshd, login)
            if !matches!(
                protocol,
                NETLINK_ROUTE
                    | NETLINK_GENERIC
                    | NETLINK_KOBJECT_UEVENT
                    | NETLINK_NETFILTER
                    | NETLINK_SOCK_DIAG
            ) {
                return Err(Errno::EPROTONOSUPPORT);
            }
            let mut s = NetlinkSocket::new(protocol, base);
            s.net = net;
            Socket::Netlink(s)
        }
        AF_NETLINK => return Err(Errno::ESOCKTNOSUPPORT),
        AF_UNIX => {
            let mut s = unix::socket(base, protocol)?;
            if let Socket::Unix(u) = &mut s {
                u.net = net;
                u.set_max_qlen(unix::max_dgram_qlen(cx));
            }
            s
        }
        _ => return Err(Errno::EAFNOSUPPORT),
    };
    let sock = Rc::new(RefCell::new(sock));
    if matches!(&*sock.borrow(), Socket::Netlink(s) if s.protocol == NETLINK_ROUTE) {
        let all = &mut cx.k().rtnl_sockets;
        all.retain(|w| w.strong_count() > 0);
        all.push(Rc::downgrade(&sock));
    }
    if let Socket::Inet(InetSocket {
        icmp: Some(r),
        fd,
        family,
        ..
    }) = &*sock.borrow()
    {
        crate::rawicmp::register(&r.tap, &sock, *fd, *family);
    }
    install(cx, sock, ty).map(|fd| fd as u64)
}

pub(super) fn socketpair(
    cx: &mut Cx<'_>,
    domain: i32,
    ty: u32,
    protocol: i32,
    sv: u64,
) -> SysResult {
    if ty & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    match domain {
        AF_UNIX => {}
        // As on Linux, first create the socket (__sock_create, inet_create: a type outside
        // SOCK_MAX is EINVAL, a type no inet protocol has ESOCKTNOSUPPORT, a protocol the type
        // does not have EPROTONOSUPPORT, a raw socket without CAP_NET_RAW EPERM), then report
        // that socketpair is not supported
        AF_INET | AF_INET6 => {
            const SOCK_MAX: i32 = 11;
            let base = (ty & SOCK_TYPE_MASK) as i32;
            let ok = match base {
                SOCK_STREAM => matches!(protocol, 0 | IPPROTO_TCP | IPPROTO_MPTCP),
                SOCK_DGRAM => matches!(protocol, 0 | IPPROTO_UDP | IPPROTO_ICMP | IPPROTO_UDPLITE),
                // Raw sockets cannot use protocol 0
                SOCK_RAW => protocol != 0,
                _ if base >= SOCK_MAX => return Err(Errno::EINVAL),
                _ => return Err(Errno::ESOCKTNOSUPPORT),
            };
            if !ok {
                return Err(Errno::EPROTONOSUPPORT);
            }
            let owner = cx.task.ns.borrow().net.owner.clone();
            if base == SOCK_RAW && !cx.cred().ns_capable(&owner, crate::cred::cap::NET_RAW) {
                return Err(Errno::EPERM);
            }
            return Err(Errno::EOPNOTSUPP);
        }
        AF_NETLINK => return Err(Errno::EOPNOTSUPP),
        _ => return Err(Errno::EAFNOSUPPORT),
    }
    let (a, b) = unix::pair(cx, (ty & SOCK_TYPE_MASK) as i32, protocol)?;
    let fa = install(cx, Rc::new(RefCell::new(a)), ty)?;
    let fds = install(cx, Rc::new(RefCell::new(b)), ty).and_then(|fb| {
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&fa.to_le_bytes());
        out[4..].copy_from_slice(&fb.to_le_bytes());
        put(cx, sv, out).inspect_err(|_| {
            let _ = cx.fds_mut().close(fb);
        })
    });
    if let Err(e) = fds {
        let _ = cx.fds_mut().close(fa);
        return Err(e);
    }
    Ok(0)
}

// ---- Binding, listening, connecting, accepting ----

/// Whether two host socket addresses name the same address (family and address, not the port).
fn same_host(a: &[u8], b: &[u8]) -> bool {
    match (a.get(..2), b.get(..2)) {
        (Some(fa), Some(fb)) if fa == fb => match u16::from_le_bytes([fa[0], fa[1]]) as i32 {
            AF_INET => a.get(4..8) == b.get(4..8),
            AF_INET6 => a.get(8..24) == b.get(8..24),
            _ => false,
        },
        _ => false,
    }
}

pub(super) fn bind(cx: &mut Cx<'_>, fd: i32, addr: u64, len: u32) -> SysResult {
    let (sock, _) = socket_of(cx, fd)?;
    let addr = read_addr(cx, addr, len)?;
    if is_unix(&sock) {
        return unix::bind(cx, &sock, &addr).map(|()| 0);
    }
    if let Socket::Netlink(s) = &mut *sock.borrow_mut() {
        return netlink_bind(cx, s, &addr).map(|()| 0);
    }
    let kind = match &*sock.borrow() {
        Socket::Inet(s) => Some((s.family, s.raw)),
        _ => None,
    };
    let addr = match kind {
        Some((family, raw)) => inet_bind_addr(family, raw, addr)?,
        None => addr,
    };
    // In a new network namespace, translate to the host address first (EADDRNOTAVAIL when it is
    // not a local address)
    let (net, icmp6, raw) = match &*sock.borrow() {
        Socket::Inet(s) => (s.net.clone(), s.icmp6(), s.icmp.is_some()),
        _ => (None, false, false),
    };
    // Raw sockets have no port (Linux's raw_bind ignores it). The host's ping socket, however,
    // takes it as the echo number: busybox's traceroute binds the receiving socket to the sending
    // socket's address (got from getsockname, with the port), so the two would share one number
    // and replies would arrive on the other one
    let mut addr = addr;
    if raw && addr.len() >= 4 {
        addr[2..4].fill(0);
    }
    let w = cx.kr().world();
    let me = net.clone().unwrap_or_else(|| cx.kr().init_ns.net.clone());
    let mapped = crate::netns::to_host(&w, &me, &addr, crate::netns::Use::Bind, icmp6)?;
    // In a new network namespace a datagram socket binding the wildcard address with port 0
    // (busybox's ping and Java's DatagramSocket do this): bind the wildcard address on the host
    // too, otherwise it cannot send to the outside (Socket::wide)
    let wide = net.is_some()
        && !icmp6
        && !raw
        && matches!(&*sock.borrow(), Socket::Inet(s) if s.ty == SOCK_DGRAM)
        && crate::netns::is_any(&addr)
        && port_of(&addr) == Some(0);
    let host_addr = if wide {
        crate::netns::any_like(&addr)
    } else {
        mapped.addr
    };
    // Ports below ip_unprivileged_port_start (1024 by default) require CAP_NET_BIND_SERVICE over
    // the network namespace's owner (Linux's inet_port_requires_bind_service)
    let family = addr
        .get(..2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]) as i32);
    if matches!(family, Some(AF_INET | AF_INET6)) && addr.len() >= 4 {
        let port = u16::from_be_bytes([addr[2], addr[3]]);
        let ns = net.clone().unwrap_or_else(|| cx.kr().init_ns.net.clone());
        let owner = ns.owner.clone();
        let prot_sock = ns.devs.borrow().conf.prot_sock;
        if port != 0
            && i64::from(port) < prot_sock
            && !cx
                .cred()
                .ns_capable(&owner, crate::cred::cap::NET_BIND_SERVICE)
        {
            return Err(Errno::EACCES);
        }
    }
    let map = cx.k().portmap.clone();
    // A low port newly mapped in a new namespace: forwarding to its destination address must be
    // recomputed (crate::forward)
    let mut remap = false;
    match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            // Identify again which interface the traffic counts on (connect does the same)
            s.route = None;
            // Ports below 1024 move to another one when the host does not let the original be
            // bound (crate::portmap); ports bound explicitly in a new network namespace always
            // move (so the port is not taken on the host, and the outer namespace and other
            // namespaces can still bind it)
            let own = s.net.is_some() && !wide;
            match port_of(&host_addr)
                .filter(|&p| mappable(s) && p != 0 && (own || map.borrow().maps(p)))
            {
                Some(port) if own => {
                    let fd = s.fd;
                    let try_bind = |h: u16| {
                        let mut a = host_addr.clone();
                        set_port(&mut a, h);
                        tuxport_hal::net::bind(fd, &a)
                    };
                    let bound = || {
                        tuxport_hal::net::sockname(fd, false)
                            .ok()
                            .and_then(|a| port_of(&a))
                    };
                    // An IPv6-only binding gets a host port of its own (crate::portmap::bind_any)
                    let v6only = s.family == AF_INET6 && s.v6only && ipv6_proper(&addr);
                    let (host, lease) = crate::portmap::bind_any(
                        &map,
                        ns_key(s),
                        map_proto(s),
                        port,
                        v6only,
                        try_bind,
                        bound,
                    )?;
                    s.lease = Some(lease);
                    s.local_map = Some((port, host));
                    remap = true;
                }
                Some(port) => {
                    let fd = s.fd;
                    let try_bind = |h: u16| {
                        let mut a = host_addr.clone();
                        set_port(&mut a, h);
                        tuxport_hal::net::bind(fd, &a)
                    };
                    let bound = || {
                        tuxport_hal::net::sockname(fd, false)
                            .ok()
                            .and_then(|a| port_of(&a))
                    };
                    let (host, lease, new) =
                        crate::portmap::bind(&map, ns_key(s), map_proto(s), port, try_bind, bound)?;
                    if new && host != port {
                        let proto = if map_proto(s) == IPPROTO_TCP {
                            "tcp"
                        } else {
                            "udp"
                        };
                        crate::klog::printk(
                            &cx.kr().klog,
                            crate::klog::level::INFO,
                            format_args!("tuxport: port {port}/{proto} is {host} on the host"),
                        );
                    }
                    s.lease = Some(lease);
                    s.local_map = Some((port, host));
                }
                // A raw socket may be bound again (raw_bind), but its host ping socket refuses a
                // second bind: binding the address it already has succeeds without one
                None if s.icmp.is_some()
                    && tuxport_hal::net::sockname(s.fd, false).is_ok_and(|cur| {
                        port_of(&cur) != Some(0) && same_host(&cur, &host_addr)
                    }) => {}
                None => tuxport_hal::net::bind(s.fd, &host_addr).map_err(Errno)?,
            }
            if s.net.is_some() {
                s.bound = true;
                s.any = crate::netns::is_any(&addr);
                s.vaddr = mapped.vaddr;
                s.wide = wide;
                register_udp(s);
            }
            // A UDP socket with a bound port in the boot namespace: when PREROUTING has a DNAT,
            // forward what comes from outside
            if s.ty == SOCK_DGRAM {
                forward_socket(cx.kr(), s);
            }
        }
        Socket::Netlink(_) | Socket::Unix(_) => unreachable!("handled above"),
    }
    if remap
        && cx
            .kr()
            .init_ns
            .net
            .nft
            .borrow()
            .has_nat(crate::nft::HOOK_PRE_ROUTING)
    {
        forward_all(cx.kr());
    }
    Ok(0)
}

/// bind on a netlink socket (Linux's `netlink_bind`): the address must be a whole
/// `sockaddr_nl`; groups need a protocol anyone may listen to or `CAP_NET_ADMIN` over the
/// namespace owner (rtnetlink's multicast-routing groups always), and only those the protocol
/// has count; a bound socket keeps its `nl_pid` (another one is EINVAL), an unbound one takes
/// the one given or, for 0, the caller's tgid (other sockets holding it are not looked for); the
/// groups given replace the first 32 joined.
fn netlink_bind(cx: &Cx<'_>, s: &mut NetlinkSocket, addr: &[u8]) -> Result<(), Errno> {
    const RTNLGRP_MROUTE_R: u64 = (1 << 29) | (1 << 30);
    if addr.len() < 12 || u16::from_le_bytes([addr[0], addr[1]]) != AF_NETLINK as u16 {
        return Err(Errno::EINVAL);
    }
    let pid = u32::from_le_bytes(addr[4..8].try_into().unwrap());
    let mut groups = u64::from(u32::from_le_bytes(addr[8..12].try_into().unwrap()));
    let admin = netlink_admin(cx, s);
    if groups != 0 {
        if !(admin || netlink_listen_unprivileged(s.protocol)) {
            return Err(Errno::EPERM);
        }
        s.groups.get_or_insert(0);
    }
    groups &= (1 << s.ngroups()) - 1;
    if s.pid != 0 && pid != s.pid {
        return Err(Errno::EINVAL);
    }
    if s.protocol == NETLINK_ROUTE && groups & RTNLGRP_MROUTE_R != 0 && !admin {
        return Err(Errno::EPERM);
    }
    if s.pid == 0 {
        s.pid = if pid == 0 {
            cx.pid_out(cx.tgid()) as u32
        } else {
            pid
        };
    }
    if let Some(g) = &mut s.groups {
        *g = (*g & !0xffff_ffff) | groups;
    }
    Ok(())
}

/// connect on a netlink socket (Linux's `netlink_connect`): `AF_UNSPEC` disconnects; otherwise
/// the address must be a whole `sockaddr_nl`, and naming a port or group needs `CAP_NET_ADMIN`
/// over the namespace owner (no protocol here lets anyone send to one). An unbound socket gets
/// its `nl_pid` ([`netlink_autobind`]). Only the kernel answers, so the peer is not recorded.
fn netlink_connect(cx: &Cx<'_>, s: &mut NetlinkSocket, addr: &[u8]) -> Result<(), Errno> {
    let family = addr
        .get(..2)
        .map(|f| u16::from_le_bytes([f[0], f[1]]))
        .ok_or(Errno::EINVAL)?;
    if family == 0 {
        return Ok(());
    }
    if family != AF_NETLINK as u16 || addr.len() < 12 {
        return Err(Errno::EINVAL);
    }
    if addr[4..12] != [0; 8] && !netlink_admin(cx, s) {
        return Err(Errno::EPERM);
    }
    netlink_autobind(cx, s);
    Ok(())
}

/// Gives an unbound netlink socket its `nl_pid` at connect and send, as bind with 0 does (Linux's
/// `netlink_autobind`, which takes another number when the tgid is held; other sockets are not
/// looked at here).
fn netlink_autobind(cx: &Cx<'_>, s: &mut NetlinkSocket) {
    if s.pid == 0 {
        s.pid = cx.pid_out(cx.tgid()) as u32;
    }
}

/// Whether the caller has `CAP_NET_ADMIN` over the owner of the netlink socket's network
/// namespace (Linux's `netlink_allowed` and `netlink_net_capable`).
fn netlink_admin(cx: &Cx<'_>, s: &NetlinkSocket) -> bool {
    let net = s.net.clone().unwrap_or_else(|| cx.kr().init_ns.net.clone());
    cx.cred()
        .ns_capable(&net.owner, crate::cred::cap::NET_ADMIN)
}

/// Whether anyone may join the protocol's multicast groups (`NL_CFG_F_NONROOT_RECV`: all but
/// `NETLINK_NETFILTER` here).
fn netlink_listen_unprivileged(protocol: i32) -> bool {
    protocol != NETLINK_NETFILTER
}

/// The address an inet socket binds, checked as Linux 6.1 checks it first (`inet_bind`,
/// `ping_check_bind_addr`, `raw_bind`, `inet6_bind`, `rawv6_bind`): shorter than the family's
/// sockaddr is EINVAL; an IPv4 socket takes AF_UNSPEC with the wildcard address as AF_INET (a
/// compatibility rule) and refuses other families with EAFNOSUPPORT, while a raw one ignores the
/// family; an IPv6 socket takes only AF_INET6 (a raw one answers EINVAL). The host never sees
/// another family: some Ubuntu kernels oops in AppArmor on an AF_UNSPEC bind, and the host thread
/// would die holding the BKL.
fn inet_bind_addr(family: i32, raw: bool, mut addr: Vec<u8>) -> Result<Vec<u8>, Errno> {
    const AF_UNSPEC: i32 = 0;
    let given = addr
        .get(..2)
        .map(|b| i32::from(u16::from_le_bytes([b[0], b[1]])));
    if family == AF_INET6 {
        if addr.len() < 24 {
            return Err(Errno::EINVAL);
        }
        if given != Some(AF_INET6) {
            return Err(if raw {
                Errno::EINVAL
            } else {
                Errno::EAFNOSUPPORT
            });
        }
        return Ok(addr);
    }
    if addr.len() < 16 {
        return Err(Errno::EINVAL);
    }
    match given {
        Some(AF_INET) => {}
        Some(AF_UNSPEC) if addr[4..8] == [0; 4] => {}
        _ if raw => {}
        _ => return Err(Errno::EAFNOSUPPORT),
    }
    addr[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    Ok(addr)
}

pub(super) fn listen(cx: &mut Cx<'_>, fd: i32, backlog: i32) -> SysResult {
    let (sock, _) = socket_of(cx, fd)?;
    match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            let src = s
                .net
                .as_ref()
                .and_then(|ns| crate::netns::implicit_bind(ns, s.family, s.icmp6()));
            ensure_bound(s, src.as_deref())?;
            tuxport_hal::net::listen(s.fd, backlog).map_err(Errno)?;
            s.listening = true;
            // TCP listens in a new network namespace are registered in the namespace: incoming
            // connections are judged by it (crate::netns)
            if let Some(net) = &s.net
                && s.listener.is_none()
                && map_proto(s) == IPPROTO_TCP
                && let Ok(local) = tuxport_hal::net::sockname(s.fd, false)
                && let Some(host) = port_of(&local)
            {
                let port = match s.local_map {
                    Some((g, h)) if h == host => g,
                    _ => host,
                };
                let v6 = s.family == AF_INET6;
                let at = crate::netns::ListenAt {
                    port,
                    any: s.any,
                    vaddr: s.vaddr,
                    host: &local,
                    v6,
                    v6only: s.v6only,
                    udp: false,
                };
                s.listener = Some(crate::netns::add_listener(net, &at));
            }
            // A listen in the boot namespace: when PREROUTING has a DNAT, forward connections from
            // outside (crate::forward)
            forward_socket(cx.kr(), s);
            // A listen other devices can reach (design doc §12 item 7): the app notifies the user
            // which port on the host to connect to
            if let Ok(local) = tuxport_hal::net::sockname(s.fd, false)
                && let (Some(ip), Some(host)) = (ip_of(&local), port_of(&local))
                && !ip.is_loopback()
            {
                let port = match s.local_map {
                    Some((g, h)) if h == host => g,
                    _ => host,
                };
                let comm = cx.task.comm.get();
                let len = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
                let comm = String::from_utf8_lossy(&comm[..len]).into_owned();
                cx.kr().portmap.borrow_mut().listened(comm, port, host);
            }
        }
        Socket::Netlink(_) => return Err(Errno::EOPNOTSUPP),
        Socket::Unix(u) => unix::listen(u, backlog, cx.ucred())?,
    }
    Ok(0)
}

pub(super) fn accept4(cx: &mut Cx<'_>, fd: i32, addr: u64, addrlen: u64, flags: u32) -> SysResult {
    // As Linux's __sys_accept4_file: a bad fd (EBADF), then the flags (EINVAL), then the new fd
    // is reserved (EMFILE) before a connection is taken, so no other thread can take it while
    // this one waits; then ENOTSOCK. The peer's address is written before the fd is installed,
    // so a bad address drops the connection (do_accept) and gives the fd back
    cx.fds().get(fd)?;
    if flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let newfd = cx.fds_mut().reserve(0)?;
    let accepted = (|| {
        let (sock, nonblock) = socket_of(cx, fd)?;
        let (new, peer) = if is_unix(&sock) {
            unix::accept(cx, &sock, nonblock)?
        } else {
            accept_host(cx, &sock, nonblock)?
        };
        write_addr(cx, addr, addrlen, &peer)?;
        Ok(new)
    })();
    match accepted {
        Ok(new) => {
            let file = socket_file(new, flags);
            cx.fds_mut().fill(newfd, file, flags & SOCK_CLOEXEC != 0);
            Ok(newfd as u64)
        }
        Err(e) => {
            cx.fds_mut().unreserve(newfd);
            Err(e)
        }
    }
}

/// Accepts a connection on the host socket: returns the new socket and the peer's address.
fn accept_host(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
) -> Result<(SocketRef, Vec<u8>), Errno> {
    let (host_fd, family, ty, proto, (timeout, sndtimeo), net, local_map, vaddr, inherited) =
        match &*sock.borrow() {
            Socket::Inet(s) => (
                s.fd,
                s.family,
                s.ty,
                s.proto,
                (s.rcvtimeo, s.sndtimeo),
                s.net.clone(),
                s.local_map,
                s.vaddr,
                (s.mark(), s.filter.clone(), s.v6only),
            ),
            Socket::Netlink(_) => return Err(Errno::EOPNOTSUPP),
            Socket::Unix(_) => unreachable!("handled by accept4"),
        };
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        match tuxport_hal::net::accept(host_fd) {
            Ok((new, peer)) => {
                let mut new = InetSocket::new(new, family, ty);
                new.proto = proto;
                // The listener's timeouts and the filter kept in Tuxport carry over (sk_clone_lock
                // copies the whole sock and shares sk_filter)
                (new.rcvtimeo, new.sndtimeo) = (timeout, sndtimeo);
                let (mark, filter, v6only) = &inherited;
                new.filter = filter.clone();
                // IPV6_V6ONLY, recorded in a new namespace (sk_clone_lock copies sk_ipv6only)
                new.v6only = *v6only;
                new.local_map = local_map;
                new.net = net.clone();
                // The accepted connection inherits the listener's SO_MARK (ir_mark in
                // inet_csk_clone_lock)
                if *mark != 0 {
                    new.track_mut().mark = *mark;
                }
                // When nat rewrote the destination of the initiating end: claim the original
                // destination seen in this namespace (SO_ORIGINAL_DST, crate::origdst)
                if ty == SOCK_STREAM
                    && !cx.kr().orig_dst.is_empty()
                    && let Ok(local) = tuxport_hal::net::sockname(new.fd, false)
                    && let (Some(p), Some(l)) = (
                        crate::origdst::end_of(&peer),
                        crate::origdst::end_of(&local),
                    )
                {
                    let ns = net.as_ref().map_or(cx.kr().init_ns.net.id, |n| n.id);
                    if let Some(o) = cx.k().orig_dst.claim((p, l), ns) {
                        new.track_mut().orig_dst = Some(o);
                    }
                }
                if proto == IPPROTO_TCP && net.is_none() {
                    cx.k().traffic.protocols.tcp_passive_opens += 1;
                }
                let w = cx.kr().world();
                let peer = unmap_addr(&cx.kr().portmap, &w, &new, peer, false);
                let me = net.clone().unwrap_or_else(|| cx.kr().init_ns.net.clone());
                let peer = crate::netns::from_host(&w, &me, &peer, crate::netns::Role::Peer, false);
                new.bound = net.is_some();
                new.vaddr = vaddr;
                new.net = net;
                return Ok((Rc::new(RefCell::new(Socket::Inet(new))), peer));
            }
            // A signal is EINTR when the wait has a timeout (inet_csk_wait_for_connect's
            // sock_intr_errno), as for AF_UNIX
            Err(e) if e == Errno::EAGAIN.0 => {
                wait_host(cx, sock, host_fd, POLLIN, nonblock, deadline, Errno::EAGAIN).map_err(
                    |e| match e {
                        Errno::ERESTARTSYS if timeout.is_some() => Errno::EINTR,
                        e => e,
                    },
                )?;
            }
            Err(e) => return Err(Errno(e)),
        }
    }
}

pub(super) fn connect(cx: &mut Cx<'_>, fd: i32, addr: u64, len: u32) -> SysResult {
    let (sock, nonblock) = socket_of(cx, fd)?;
    let addr = read_addr(cx, addr, len)?;
    if is_unix(&sock) {
        return unix::connect(cx, &sock, nonblock, &addr).map(|()| 0);
    }
    if let Socket::Netlink(s) = &mut *sock.borrow_mut() {
        return netlink_connect(cx, s, &addr).map(|()| 0);
    }
    let map = cx.k().portmap.clone();
    let w = cx.kr().world();
    let init = cx.kr().init_ns.net.clone();
    let cred = cx.cred();
    let (host_addr, tcp, host_fd, origs) = match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            // The local address may have changed: identify again which interface the traffic
            // counts on
            s.route = None;
            // UDP connecting to the DNS stub: record it, and later sends are handled by the stub
            // (it does not exist in new network namespaces). The host socket is connected there
            // too, though nothing is ever sent through it, so that it gets the local address and
            // port Linux's connect gives (127.0.0.1 and an autobound port) for getsockname and
            // /proc/net
            if stub_capable(s) {
                s.stub_peer = is_stub(&addr);
                if s.stub_peer {
                    let _ = tuxport_hal::net::connect(s.fd, &addr);
                    return Ok(0);
                }
            }
            // Translate the address to the host's (crate::netns), binding first if unbound.
            // nftables' OUTPUT nat (crate::nft) rewrites the destination first: the peer the
            // guest sees is still the one it connected to, and the source of received datagrams
            // is translated back
            let me = s.net.clone().unwrap_or_else(|| init.clone());
            let Routed {
                m,
                dst,
                there,
                actual,
                verdict,
                origs,
            } = route(&w, s, &me, &addr, &cred)?;
            // Filter chains: a rejected TCP connection fails at once, a dropped one connects to
            // a blackhole and waits for the timeout (a drop on OUTPUT also retransmits until
            // timeout on Linux); UDP connects as usual, and later sends without an address are
            // not sent
            let blocked = !matches!(
                verdict.0,
                crate::nft::Outcome::Accept | crate::nft::Outcome::Dnat(..)
            );
            if blocked && s.ty == SOCK_STREAM {
                if let crate::nft::Outcome::Reject(e) = verdict.0 {
                    return Err(Errno(e));
                }
                s.guest_peer = Some(connected_peer(s, &addr));
                let hole = blackhole(cx.kr()).ok_or(Errno::ETIMEDOUT)?;
                let hole = if s.family == AF_INET6 {
                    let mut a = vec![0u8; 28];
                    a[..2].copy_from_slice(&(AF_INET6 as u16).to_le_bytes());
                    a[2..4].copy_from_slice(&hole[2..4]);
                    a[18..20].copy_from_slice(&[0xff, 0xff]);
                    a[20..24].copy_from_slice(&hole[4..8]);
                    a
                } else {
                    hole
                };
                (hole, false, s.fd, None)
            } else {
                // Connecting into a new network namespace: ECONNREFUSED when there is no matching
                // listener there (a socket listening on the wildcard address in the boot namespace
                // on the host also covers this address, so it cannot be left to the host)
                if s.ty == SOCK_STREAM
                    && let Some(t) = &m.target
                    && t.host_net.is_some()
                    && !crate::netns::listening(t, &there, false)
                {
                    return Err(Errno::ECONNREFUSED);
                }
                let back: Vec<_> = actual.iter().map(|t| (t.clone(), addr.clone())).collect();
                if !back.is_empty() || s.track.is_some() {
                    let t = s.track_mut();
                    t.nat_back = back;
                    t.blocked = None;
                }
                let dst = &dst;
                if crate::netns::is_inet(&addr) {
                    ensure_bound(s, m.source.as_deref())?;
                }
                // getpeername reports the address connected to (right even when the host-side peer
                // cannot be translated back, such as when connecting to its own address)
                s.guest_peer = crate::netns::is_inet(&addr).then(|| connected_peer(s, &addr));
                // Mapped low ports on the local machine become the host's (crate::portmap), by the
                // namespace the destination is in
                let key = m.target.as_ref().map_or(0, key_of);
                let to = if s.ty == SOCK_DGRAM {
                    crate::netns::toward_self(&me, dst, m.addr)
                } else {
                    m.addr
                };
                let (host_addr, pm) = map_dest(&map, s, to, key, ipv6_proper(&there));
                s.peer_map = pm;
                // A datagram socket bound to the wildcard address on the host connects to a mapped
                // destination: the host socket is not connected and the peer is recorded here
                // (InetSocket::vconn); a previous host-side connection is undone first
                let was = s.vconn.take();
                if s.wide && s.ty == SOCK_DGRAM && m.target.is_some() {
                    // Disconnect only when really connected on the host; disconnecting releases
                    // the port the kernel assigned (when bound to port 0, udp_disconnect), so bind
                    // the original one back, keeping getsockname and the registered port unchanged
                    if was.is_none() && tuxport_hal::net::sockname(s.fd, true).is_ok() {
                        let old = tuxport_hal::net::sockname(s.fd, false).ok();
                        let _ = tuxport_hal::net::connect(s.fd, &[0u8; 16]);
                        if let Some(o) = old
                            && tuxport_hal::net::sockname(s.fd, false)
                                .ok()
                                .as_deref()
                                .and_then(port_of)
                                != port_of(&o)
                        {
                            let _ = tuxport_hal::net::bind(s.fd, &o);
                        }
                    }
                    s.vconn = Some(host_addr);
                    s.peer = Some(addr.clone());
                    s.any = false;
                    return Ok(0);
                }
                if was.is_some() && !crate::netns::is_inet(&addr) {
                    s.peer = None;
                    return Ok(0);
                }
                // The destination of a connected UDP socket is dropped by a filter chain: sends
                // get EPERM on OUTPUT and are silently dropped elsewhere
                if blocked {
                    s.track_mut().blocked = Some(if verdict.1 { Errno::EPERM.0 } else { 0 });
                }
                // Traffic in new network namespaces is not counted on the host's interfaces (nor
                // in /proc/net/snmp)
                let tcp = s.net.is_none() && s.ty == SOCK_STREAM && s.proto == IPPROTO_TCP;
                let stream = s.ty == SOCK_STREAM && nat_proto(s) == Some(IPPROTO_TCP as u8);
                (host_addr, tcp, s.fd, stream.then_some(origs))
            }
        }
        Socket::Netlink(_) | Socket::Unix(_) => unreachable!("handled above"),
    };
    let r = tuxport_hal::net::connect(host_fd, &host_addr);
    if tcp {
        count_connect(cx, &sock, r.err().map(Errno));
    }
    // TCP connections whose destination nat rewrote record the original destination by their
    // 4-tuple on the host, for the accepting end to claim at accept; ones without nat remove
    // earlier records for the same 4-tuple (crate::origdst). A blocking connect also records it
    // before releasing the BKL to wait, so the other side cannot accept first
    if let Some(origs) = origs
        && (r.is_ok() || r == Err(Errno::EINPROGRESS.0))
        && (!origs.is_empty() || !cx.kr().orig_dst.is_empty())
        && let Ok(local) = tuxport_hal::net::sockname(host_fd, false)
        && let (Some(a), Some(b)) = (
            crate::origdst::end_of(&local),
            crate::origdst::end_of(&host_addr),
        )
    {
        cx.k().orig_dst.register((a, b), origs);
    }
    let connected = addr.get(..2) != Some(&[0, 0]);
    if (r.is_ok() || r == Err(Errno::EINPROGRESS.0))
        && let Socket::Inet(s) = &mut *sock.borrow_mut()
    {
        s.any &= !connected;
    }
    match r {
        Ok(()) => {
            // A datagram socket records the peer; AF_UNSPEC disconnects
            if let Socket::Inet(s) = &mut *sock.borrow_mut()
                && s.ty != SOCK_STREAM
            {
                s.peer = connected.then(|| addr.clone());
            }
            return Ok(0);
        }
        Err(e) if e == Errno::EINPROGRESS.0 && !nonblock => {}
        // A blocking connect made again while the first is still in progress (restarted after a
        // signal, or from another thread) waits for it too (Linux's SS_CONNECTING case)
        Err(e) if e == Errno::EALREADY.0 && !nonblock => {}
        Err(e) => return Err(Errno(e)),
    }
    // Blocking connect: wait for the connection to complete, then fetch its result. As in Linux's
    // __inet_stream_connect, SO_SNDTIMEO bounds the wait (then the error is EINPROGRESS, or
    // EALREADY for a connect made again, and the connection goes on), and a signal restarts the
    // call (ERESTARTSYS), or ends it with EINTR when there is a timeout
    let timeout = match &*sock.borrow() {
        Socket::Inet(s) => s.sndtimeo,
        _ => None,
    };
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        let mut pfd = [(host_fd, POLLOUT, 0)];
        tuxport_hal::poll(&mut pfd, Some(Duration::ZERO)).map_err(Errno)?;
        if pfd[0].2 != 0 {
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Errno(r.unwrap_err()));
        }
        if cx.interrupted() {
            return Err(if timeout.is_some() {
                Errno::EINTR
            } else {
                Errno::ERESTARTSYS
            });
        }
        cx.wait_objects(deadline, &mut pfd, wake::FILE, &[wake::of(&sock)])?;
    }
    let err = tuxport_hal::net::getsockopt(host_fd, SOL_SOCKET, SO_ERROR, 4).map_err(Errno)?;
    match i32::from_le_bytes(err[..4].try_into().unwrap()) {
        // At the socket layer the host socket is still "connecting", and becomes "connected"
        // only after connecting once more: otherwise a later connect by the guest would succeed,
        // whereas on Linux connecting again after a blocking connect returned is EISCONN
        0 => {
            let _ = tuxport_hal::net::connect(host_fd, &host_addr);
            if let Socket::Inet(s) = &mut *sock.borrow_mut() {
                s.opening = false;
            }
            Ok(0)
        }
        // Here too the host socket is left "connecting" until connected again (which answers
        // ECONNABORTED and sends nothing), while Linux's __inet_stream_connect has already reset
        // the guest's: otherwise the guest's next connect on this socket would get that
        // ECONNABORTED instead of sending a new SYN
        e => {
            let _ = tuxport_hal::net::connect(host_fd, &host_addr);
            if tcp {
                count_connect(cx, &sock, Some(Errno(e)));
            }
            Err(Errno(e))
        }
    }
}

/// The host address that TCP connections dropped by filter chains connect to: a host listener
/// whose accept queue is already full and that never accepts, so the host silently drops further
/// SYNs, the connection fails after Linux's retransmission timeout (ETIMEDOUT), and timeouts the
/// guest set still take effect. One per kernel; None when unavailable (connect is then ETIMEDOUT
/// at once).
fn blackhole(k: &crate::kernel::Kernel) -> Option<Vec<u8>> {
    k.blackhole
        .get_or_init(|| {
            const POLLOUT: i16 = 4;
            let lo = tuxport_hal::net::sockaddr(std::net::SocketAddr::from(([127, 0, 0, 1], 0)));
            let l = tuxport_hal::net::socket(AF_INET, SOCK_STREAM, 0).ok()?;
            tuxport_hal::net::bind(l, &lo).ok()?;
            tuxport_hal::net::listen(l, 0).ok()?;
            let addr = tuxport_hal::net::sockname(l, false).ok()?;
            // Fill the accept queue: connect until one fails (even with backlog 0 one or two may
            // get in first)
            let connects = |wait: Duration| -> Option<(i32, bool)> {
                let c = tuxport_hal::net::socket(AF_INET, SOCK_STREAM, 0).ok()?;
                let _ = tuxport_hal::net::connect(c, &addr);
                let mut p = [(c, POLLOUT, 0)];
                let _ = tuxport_hal::poll(&mut p, Some(wait));
                let err = tuxport_hal::net::getsockopt(c, SOL_SOCKET, SO_ERROR, 4).ok()?;
                Some((c, p[0].2 & POLLOUT != 0 && err[..4] == [0; 4]))
            };
            for _ in 0..4 {
                let (c, done) = connects(Duration::from_millis(200))?;
                if !done {
                    tuxport_hal::net::close(c);
                    // The listener and the connections filled in stay forever
                    return Some(addr);
                }
            }
            None
        })
        .clone()
}

/// Before connect, sendto or listen, an unbound host socket is first bound to `source`
/// ([`crate::netns::Mapped::source`]): in a new network namespace this is its loopback address
/// (Linux's autobind binds the namespace's wildcard address, and binding the wildcard address on
/// the host would reach every interface); for traffic handed to the host network (`source` is
/// None) the host binds it; in the boot namespace, when connecting to another namespace, it is
/// the host-side address of the virtual address facing the other side, so the peer the other side
/// sees is right.
fn ensure_bound(s: &mut InetSocket, source: Option<&[u8]>) -> Result<(), Errno> {
    if let Some(net) = s.net.clone() {
        if !s.bound {
            match source {
                Some(addr) => {
                    tuxport_hal::net::bind(s.fd, addr).map_err(Errno)?;
                    s.any = true;
                    register_udp(s);
                    // A UDP socket sending to its own virtual address: register the port so that
                    // answers can be delivered (crate::netns)
                    if s.ty == SOCK_DGRAM
                        && crate::netns::is_self_source(&net, addr)
                        && let Some(p) = tuxport_hal::net::sockname(s.fd, false)
                            .ok()
                            .as_deref()
                            .and_then(port_of)
                    {
                        net.lo.borrow_mut().self_udp.push(p);
                        s.self_port = Some(p);
                    }
                }
                // Handed to the host network: the host binds it to the wildcard address
                None => s.wide = true,
            }
            s.bound = true;
        }
    } else if let Some(addr) = source
        && tuxport_hal::net::sockname(s.fd, false)
            .ok()
            .is_some_and(|a| port_of(&a) == Some(0) && crate::netns::is_any(&a))
        && tuxport_hal::net::bind(s.fd, addr).is_ok()
    {
        // To the guest it is an autobound wildcard address (getsockname reports it before
        // connecting)
        s.any = true;
    }
    Ok(())
}

/// UDP sockets bound in a new network namespace are registered in the namespace
/// ([`crate::netns::listening`]): datagrams sent there use it to decide whether anyone receives
/// them. Both explicit binds and binds made on its behalf before sending count.
fn register_udp(s: &mut InetSocket) {
    let Some(net) = s.net.clone() else {
        return;
    };
    if s.ty != SOCK_DGRAM || s.icmp.is_some() || s.listener.is_some() || map_proto(s) != IPPROTO_UDP
    {
        return;
    }
    let Ok(local) = tuxport_hal::net::sockname(s.fd, false) else {
        return;
    };
    let Some(host) = port_of(&local).filter(|&p| p != 0) else {
        return;
    };
    let port = match s.local_map {
        Some((g, h)) if h == host => g,
        _ => host,
    };
    let any = s.any || s.wide || crate::netns::is_any(&local);
    let at = crate::netns::ListenAt {
        port,
        any,
        vaddr: s.vaddr,
        host: &local,
        v6: s.family == AF_INET6,
        v6only: s.v6only,
        udp: true,
    };
    s.listener = Some(crate::netns::add_listener(&net, &at));
}

/// Counts active TCP connections (`/proc/net/snmp`'s ActiveOpens, AttemptFails,
/// [`crate::traffic`]): ActiveOpens is counted once when the host sent a SYN (connect succeeded,
/// is in progress, was refused or timed out), and AttemptFails once when this connection is later
/// seen to fail. `err` is the result of this connect or of the blocking wait. When a nonblocking
/// connection fails and the guest learns it only from poll and SO_ERROR, only an SO_ERROR of
/// ECONNREFUSED can be counted ([`getsockopt`]).
fn count_connect(cx: &mut Cx<'_>, sock: &SocketRef, err: Option<Errno>) {
    let Socket::Inet(s) = &mut *sock.borrow_mut() else {
        return;
    };
    let p = &mut cx.k().traffic.protocols;
    match err {
        None | Some(Errno::EINPROGRESS) => {
            if !s.opening {
                p.tcp_active_opens += 1;
                s.opening = true;
            }
        }
        // The SYN sent by an earlier connect failed (as found by connecting again when
        // nonblocking)
        Some(Errno::ECONNREFUSED | Errno::ETIMEDOUT | Errno::EHOSTUNREACH | Errno::ENETUNREACH)
            if s.opening =>
        {
            p.tcp_attempt_fails += 1;
            s.opening = false;
        }
        // The SYN sent this time failed at once
        Some(Errno::ECONNREFUSED | Errno::ETIMEDOUT) => {
            p.tcp_active_opens += 1;
            p.tcp_attempt_fails += 1;
        }
        Some(_) => {}
    }
}

// ---- getsockname and getpeername ----

pub(super) fn getsockname(cx: &mut Cx<'_>, fd: i32, addr: u64, addrlen: u64) -> SysResult {
    sockname(cx, fd, addr, addrlen, false)
}

pub(super) fn getpeername(cx: &mut Cx<'_>, fd: i32, addr: u64, addrlen: u64) -> SysResult {
    sockname(cx, fd, addr, addrlen, true)
}

fn sockname(cx: &mut Cx<'_>, fd: i32, addr: u64, addrlen: u64, peer: bool) -> SysResult {
    let (sock, _) = socket_of(cx, fd)?;
    let value = match &*sock.borrow() {
        Socket::Inet(s) if peer && s.stub_peer => stub_addr(s.family),
        // The host connection of a UDP socket the aggregation point watches may have been changed
        // ([`send`]), so the peer reported is the one the guest connected to
        Socket::Inet(InetSocket {
            udp_tap: Some(_),
            peer: Some(p),
            ..
        }) if peer => p.clone(),
        // The peer is recorded (InetSocket::vconn): the host socket is not connected
        Socket::Inet(InetSocket {
            vconn: Some(_),
            guest_peer: Some(p),
            ..
        }) if peer => p.clone(),
        Socket::Inet(s) => inet_name(cx, s, peer)?,
        Socket::Netlink(_) if peer => {
            let mut b = vec![0u8; 12];
            b[..2].copy_from_slice(&(AF_NETLINK as u16).to_le_bytes());
            b
        }
        Socket::Netlink(s) => s.sockaddr(),
        Socket::Unix(u) => unix::sockname(u, peer)?,
    };
    write_addr_to(cx, addr, addrlen, &value).map(|()| 0)
}

/// The peer getpeername reports after connecting inet socket `s` to `addr` (Linux's
/// `inet_getname` and `inet6_getname` after connect): the wildcard address stands for the local
/// machine (`ip_route_connect` and `tcp_v6_connect` take the loopback address, IPv4-mapped when
/// the IPv6 socket's local address is), an IPv4 address given to an IPv6 socket is reported
/// IPv4-mapped, and an IPv6 peer has no flow information and a scope only when link-local. An
/// IPv4 socket bound to an address of its own would connect to that one instead of the loopback;
/// this is not followed.
fn connected_peer(s: &InetSocket, addr: &[u8]) -> Vec<u8> {
    let family = s.family;
    // Shorter than its family's sockaddr: kept as given (the host refuses the connect)
    let fam = addr
        .get(..2)
        .map_or(0, |f| u16::from_le_bytes([f[0], f[1]]) as i32);
    let short = match fam {
        AF_INET => addr.len() < 16,
        AF_INET6 => addr.len() < 24,
        _ => true,
    };
    if short {
        return addr.to_vec();
    }
    let port = &addr[2..4];
    match fam {
        AF_INET => {
            let mut ip: [u8; 4] = addr[4..8].try_into().unwrap();
            if ip == [0; 4] {
                ip = [127, 0, 0, 1];
            }
            if family == AF_INET6 {
                let mut a = vec![0u8; 28];
                a[..2].copy_from_slice(&(AF_INET6 as u16).to_le_bytes());
                a[2..4].copy_from_slice(port);
                a[18..20].copy_from_slice(&[0xff, 0xff]);
                a[20..24].copy_from_slice(&ip);
                a
            } else {
                let mut a = vec![0u8; 16];
                a[..4].copy_from_slice(&addr[..4]);
                a[4..8].copy_from_slice(&ip);
                a
            }
        }
        AF_INET6 => {
            let mut a = addr.to_vec();
            a.resize(28, 0);
            let mapped = a[8..20] == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff];
            let mapped_local = tuxport_hal::net::sockname(s.fd, false)
                .is_ok_and(|l| l.get(8..20) == Some(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff]));
            if a[8..24] == [0; 16] && mapped_local {
                a[18..24].copy_from_slice(&[0xff, 0xff, 127, 0, 0, 1]);
            } else if a[8..24] == [0; 16] {
                a[23] = 1;
            } else if mapped && a[20..24] == [0; 4] {
                a[20..24].copy_from_slice(&[127, 0, 0, 1]);
            }
            a[4..8].fill(0);
            if !(a[8] == 0xfe && a[9] & 0xc0 == 0x80) {
                a[24..28].fill(0);
            }
            a
        }
        _ => addr.to_vec(),
    }
}

/// The host socket's local (or with `peer`, peer) address as the guest sees it (getsockname,
/// getpeername).
fn inet_name(cx: &Cx<'_>, s: &InetSocket, peer: bool) -> Result<Vec<u8>, Errno> {
    let mut a = tuxport_hal::net::sockname(s.fd, peer).map_err(Errno)?;
    // A raw socket's local "port" is the protocol number (inet_sport, set by Linux's
    // inet_create), while the host's ping socket reports its number
    if s.icmp.is_some() && !peer && a.len() >= 4 {
        a[2..4].copy_from_slice(&(s.proto as u16).to_be_bytes());
    }
    let w = cx.kr().world();
    let a = unmap_addr(&cx.kr().portmap, &w, s, a, !peer);
    let me = net_of(cx, s);
    let other = if peer {
        None
    } else {
        tuxport_hal::net::sockname(s.fd, true).ok()
    };
    if peer && let Some(g) = &s.guest_peer {
        return Ok(g.clone());
    }
    // The peer is recorded (InetSocket::vconn): the local end is the address facing the peer
    if !peer
        && s.vconn.is_some()
        && let Some(g) = s.guest_peer.as_deref()
        && let Some(local) = crate::netns::local_toward(&me, &a, g)
    {
        return Ok(local);
    }
    let role = if peer {
        crate::netns::Role::Peer
    } else {
        crate::netns::Role::Local {
            any: s.any,
            vaddr: s.vaddr,
            peer: other.as_deref(),
        }
    };
    Ok(crate::netns::from_host(&w, &me, &a, role, s.icmp6()))
}

// ---- Options ----

pub(super) fn setsockopt(
    cx: &mut Cx<'_>,
    fd: i32,
    level: i32,
    name: i32,
    value: u64,
    len: u32,
) -> SysResult {
    const SO_SNDBUF: i32 = 7;
    const SO_RCVBUF: i32 = 8;
    const SO_SNDBUFFORCE: i32 = 32;
    const SO_RCVBUFFORCE: i32 = 33;
    let (sock, _) = socket_of(cx, fd)?;
    let name = new_timeouts(level, name);
    let optlen = len;
    let len = (len as usize).min(256);
    let mut v = vec![0u8; len];
    cx.mm().read(UserSlice::new(value, len), &mut v)?;
    // Netlink and AF_UNIX sockets keep their options in process
    match &mut *sock.borrow_mut() {
        Socket::Netlink(s) => return netlink_setsockopt(cx, s, level, name, &v).map(|()| 0),
        Socket::Unix(u) => {
            let net = u.net.clone().unwrap_or_else(|| cx.kr().init_ns.net.clone());
            let privileged = (
                cx.cred().capable(crate::cred::cap::NET_ADMIN),
                priority_allowed(cx, &net),
            );
            return unix::setsockopt(u, level, name, &v, privileged).map(|()| 0);
        }
        Socket::Inet(_) => {}
    }
    // SO_*BUFFORCE, which ignore the limits, require CAP_NET_ADMIN, which the app does not have
    // on the host: guest processes with this capability fall back to the plain SO_*BUF (limited
    // by the host)
    let name = match (level, name) {
        (SOL_SOCKET, SO_SNDBUFFORCE | SO_RCVBUFFORCE)
            if !cx.cred().capable(crate::cred::cap::NET_ADMIN) =>
        {
            return Err(Errno::EPERM);
        }
        (SOL_SOCKET, SO_SNDBUFFORCE) => SO_SNDBUF,
        (SOL_SOCKET, SO_RCVBUFFORCE) => SO_RCVBUF,
        _ => name,
    };
    // Raw ICMP's own options (crate::rawicmp::RawIcmp)
    if let Socket::Inet(InetSocket { icmp: Some(r), .. }) = &mut *sock.borrow_mut()
        && let Some(res) = r.setsockopt(level, name, &v)
    {
        return res.map(|()| 0).map_err(Errno);
    }
    // UDP: record that a TTL was set and IP_RECVERR enabled by the guest itself (crate::rawicmp).
    // While the aggregation point watches the socket, IP_RECVERR on the host is always on: the
    // guest turning it off is only recorded, and after it turns it on the socket is no longer
    // watched and the host's setting is the guest's
    let udp = match &mut *sock.borrow_mut() {
        Socket::Inet(s) => crate::rawicmp::udp_setsockopt(s, level, name, &v),
        _ => None,
    };
    match udp {
        Some(true) => {
            crate::rawicmp::untap(&sock);
            return Ok(0);
        }
        Some(false) => return Ok(0),
        None => {}
    }
    // Classic BPF filters (SO_ATTACH_FILTER, SO_ATTACH_REUSEPORT_CBPF), checked in Linux 6.1.y's
    // order: an optlen under an int is EINVAL; SO_ATTACH_FILTER on a TCP socket needs
    // CAP_NET_ADMIN over the socket's network namespace (Linux 5d39580f68e6, EPERM); struct
    // sock_fprog must be exactly 16 bytes (EINVAL); a locked filter is EPERM; a count of 0 or above
    // BPF_MAXINSNS, or no program, is EINVAL. The pointer to the instructions is then replaced
    // with one on the host, which checks them. dockerd attaches a drop-everything filter to each
    // TCP port it publishes until its DNAT rules are in place, and a host with 5d39580f68e6
    // refuses TCP filters from the app, which never has CAP_NET_ADMIN: the program is then checked
    // on a scratch UDP socket and kept in InetSocket::filter, filtering nothing
    const SO_ATTACH_REUSEPORT_CBPF: i32 = 51;
    let inet = match &*sock.borrow() {
        Socket::Inet(s) => Some((
            s.fd,
            s.ty == SOCK_STREAM && s.proto == IPPROTO_TCP,
            net_of(cx, s).owner.clone(),
        )),
        _ => None,
    };
    if let Some((host_fd, tcp, owner)) = inet
        && level == SOL_SOCKET
        && matches!(name, SO_ATTACH_FILTER | SO_ATTACH_REUSEPORT_CBPF)
    {
        let tcp_filter = tcp && name == SO_ATTACH_FILTER;
        if optlen < 4 {
            return Err(Errno::EINVAL);
        }
        if tcp_filter && !cx.cred().ns_capable(&owner, crate::cred::cap::NET_ADMIN) {
            return Err(Errno::EPERM);
        }
        if optlen != 16 {
            return Err(Errno::EINVAL);
        }
        if filter_locked(host_fd) {
            return Err(Errno::EPERM);
        }
        let n = usize::from(u16::from_le_bytes([v[0], v[1]]));
        let ptr = u64::from_le_bytes(v[8..16].try_into().unwrap());
        if n == 0 || n > BPF_MAXINSNS || ptr == 0 {
            return Err(Errno::EINVAL);
        }
        let mut insns = vec![0u8; n * 8];
        cx.mm().read(UserSlice::new(ptr, n * 8), &mut insns)?;
        let mut host = [0u8; 16];
        host[..2].copy_from_slice(&(n as u16).to_le_bytes());
        host[8..16].copy_from_slice(&(insns.as_ptr() as u64).to_le_bytes());
        let attach = |fd: i32| tuxport_hal::net::setsockopt(fd, level, name, &host);
        let res = if tcp_filter && crate::bug::host_refuses_tcp_filters() {
            Err(Errno::EPERM.0)
        } else {
            attach(host_fd)
        };
        let kept = match res {
            Ok(()) => None,
            Err(e) if e == Errno::EPERM.0 && tcp_filter => {
                let scratch = tuxport_hal::net::socket(AF_INET, SOCK_DGRAM, 0).map_err(Errno)?;
                let checked = attach(scratch);
                tuxport_hal::net::close(scratch);
                checked.map_err(Errno)?;
                Some(insns)
            }
            Err(e) => return Err(Errno(e)),
        };
        if name == SO_ATTACH_FILTER
            && let Socket::Inet(s) = &mut *sock.borrow_mut()
        {
            s.filter = kept;
        }
        return Ok(0);
    }
    // SO_DETACH_FILTER of a program kept in Tuxport (Linux's sk_detach_filter: EPERM while the
    // filter is locked); without one the host answers (ENOENT when it has none either)
    if (level, name) == (SOL_SOCKET, SO_DETACH_FILTER)
        && let Socket::Inet(s) = &mut *sock.borrow_mut()
        && s.filter.is_some()
    {
        if optlen < 4 {
            return Err(Errno::EINVAL);
        }
        if filter_locked(s.fd) {
            return Err(Errno::EPERM);
        }
        s.filter = None;
        return Ok(0);
    }
    // SO_MARK: only recorded (InetSocket::mark); packets sent by this machine carry it in
    // nftables. Linux requires CAP_NET_RAW or CAP_NET_ADMIN over the owner of the socket's
    // network namespace (sk_setsockopt)
    if (level, name) == (SOL_SOCKET, SO_MARK)
        && let Socket::Inet(s) = &mut *sock.borrow_mut()
    {
        let mark = v.get(..4).ok_or(Errno::EINVAL)?;
        let owner = net_of(cx, s).owner.clone();
        let cred = cx.cred();
        if !cred.ns_capable(&owner, crate::cred::cap::NET_RAW)
            && !cred.ns_capable(&owner, crate::cred::cap::NET_ADMIN)
        {
            return Err(Errno::EPERM);
        }
        let mark = u32::from_le_bytes(mark.try_into().unwrap());
        if mark != 0 || s.track.is_some() {
            s.track_mut().mark = mark;
        }
        return Ok(0);
    }
    match &mut *sock.borrow_mut() {
        Socket::Inet(s) => match (level, name) {
            (SOL_SOCKET, SO_RCVTIMEO) => s.rcvtimeo = timeval(&v)?,
            (SOL_SOCKET, SO_SNDTIMEO) => s.sndtimeo = timeval(&v)?,
            // In a new network namespace the host socket must use IPv4-mapped addresses: only
            // record it
            (IPPROTO_IPV6, IPV6_V6ONLY) if s.net.is_some() && s.family == AF_INET6 => {
                let on = v.get(..4).ok_or(Errno::EINVAL)?;
                s.v6only = i32::from_le_bytes(on.try_into().unwrap()) != 0;
            }
            _ => tuxport_hal::net::setsockopt(s.fd, level, name, &v).map_err(Errno)?,
        },
        Socket::Netlink(_) | Socket::Unix(_) => unreachable!("handled above"),
    }
    Ok(0)
}

/// `SO_RCVTIMEO_NEW` and `SO_SNDTIMEO_NEW` are the old options on a 64-bit kernel: the same
/// `struct timeval` (`__kernel_sock_timeval` has two 64-bit fields too).
fn new_timeouts(level: i32, name: i32) -> i32 {
    const SO_RCVTIMEO_NEW: i32 = 66;
    const SO_SNDTIMEO_NEW: i32 = 67;
    match (level, name) {
        (SOL_SOCKET, SO_RCVTIMEO_NEW) => SO_RCVTIMEO,
        (SOL_SOCKET, SO_SNDTIMEO_NEW) => SO_SNDTIMEO,
        _ => name,
    }
}

/// Whether the host socket's filter is locked (`SO_LOCK_FILTER`, which goes to the host): Linux
/// then refuses to attach or detach one with EPERM. A host that cannot say counts as unlocked.
fn filter_locked(host_fd: i32) -> bool {
    tuxport_hal::net::getsockopt(host_fd, SOL_SOCKET, SO_LOCK_FILTER, 4)
        .is_ok_and(|v| v.iter().any(|&b| b != 0))
}

/// Whether the caller may give a socket in `net` a priority outside 0..=6 (`CAP_NET_RAW` or
/// `CAP_NET_ADMIN` over the namespace owner, `sk_setsockopt`).
fn priority_allowed(cx: &Cx<'_>, net: &crate::nsproxy::NetNs) -> bool {
    let cred = cx.cred();
    cred.ns_capable(&net.owner, crate::cred::cap::NET_RAW)
        || cred.ns_capable(&net.owner, crate::cred::cap::NET_ADMIN)
}

/// setsockopt on a netlink socket. `SOL_SOCKET`: the timeouts, `SO_PASSCRED` and the options
/// [`SockOpts`] records, others accepted without effect; `SOL_NETLINK` as Linux's
/// `netlink_setsockopt` (the value read only when it is a whole int): memberships and flags
/// recorded ([`NetlinkSocket`]), `NETLINK_CAP_ACK` and `NETLINK_EXT_ACK` shaping error acks;
/// other levels ENOPROTOOPT.
fn netlink_setsockopt(
    cx: &Cx<'_>,
    s: &mut NetlinkSocket,
    level: i32,
    name: i32,
    v: &[u8],
) -> Result<(), Errno> {
    const SO_PASSCRED: i32 = 16;
    const RTNLGRP_IPV4_MROUTE_R: u32 = 30;
    const RTNLGRP_IPV6_MROUTE_R: u32 = 31;
    let net = s.net.clone().unwrap_or_else(|| cx.kr().init_ns.net.clone());
    if level == SOL_SOCKET {
        match name {
            SO_RCVTIMEO => s.rcvtimeo = timeval(v)?,
            SO_SNDTIMEO => s.sndtimeo = timeval(v)?,
            SO_PASSCRED => {
                let on = v.get(..4).ok_or(Errno::EINVAL)?;
                s.passcred = on != [0; 4];
            }
            _ => {
                let admin = cx.cred().capable(crate::cred::cap::NET_ADMIN);
                if let Some(r) = s.opts.set(name, v, admin, priority_allowed(cx, &net)) {
                    r?;
                }
            }
        }
        return Ok(());
    }
    if level != SOL_NETLINK {
        return Err(Errno::ENOPROTOOPT);
    }
    let val = v
        .get(..4)
        .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
    match name {
        NETLINK_ADD_MEMBERSHIP | NETLINK_DROP_MEMBERSHIP => {
            let admin = netlink_admin(cx, s);
            if !(admin || netlink_listen_unprivileged(s.protocol)) {
                return Err(Errno::EPERM);
            }
            // The set is allocated before the group is checked
            s.groups.get_or_insert(0);
            if val == 0 || val > s.ngroups() {
                return Err(Errno::EINVAL);
            }
            // rtnetlink_bind
            if name == NETLINK_ADD_MEMBERSHIP
                && s.protocol == NETLINK_ROUTE
                && matches!(val, RTNLGRP_IPV4_MROUTE_R | RTNLGRP_IPV6_MROUTE_R)
                && !admin
            {
                return Err(Errno::EPERM);
            }
            let bit = 1u64 << (val - 1);
            let groups = s.groups.get_or_insert(0);
            if name == NETLINK_ADD_MEMBERSHIP {
                *groups |= bit;
            } else {
                *groups &= !bit;
            }
        }
        NETLINK_CAP_ACK => s.cap_ack = val != 0,
        NETLINK_EXT_ACK => s.ext_ack = val != 0,
        NETLINK_LISTEN_ALL_NSID
            if !cx
                .cred()
                .ns_capable(&net.owner, crate::cred::cap::NET_BROADCAST) =>
        {
            return Err(Errno::EPERM);
        }
        NETLINK_PKTINFO
        | NETLINK_BROADCAST_ERROR
        | NETLINK_NO_ENOBUFS
        | NETLINK_LISTEN_ALL_NSID
        | NETLINK_GET_STRICT_CHK => {
            if val != 0 {
                s.flags |= 1 << name;
            } else {
                s.flags &= !(1 << name);
            }
        }
        _ => return Err(Errno::ENOPROTOOPT),
    }
    Ok(())
}

/// getsockopt on a netlink socket: the data and the length to report when it differs from the
/// data's (`NETLINK_LIST_MEMBERSHIPS` reports the whole set's size whatever fits). `SOL_SOCKET`
/// as Linux's `sk_getsockopt` (no peer: pid 0, uid and gid -1), `SOL_NETLINK` as
/// `netlink_getsockopt` (a flag needs room for an int), other levels ENOPROTOOPT.
fn netlink_getsockopt(
    s: &NetlinkSocket,
    level: i32,
    name: i32,
    room: usize,
) -> Result<(Vec<u8>, Option<usize>), Errno> {
    const SO_PROTOCOL: i32 = 38;
    const SO_DOMAIN: i32 = 39;
    const SO_PASSCRED: i32 = 16;
    const SO_PEERCRED: i32 = 17;
    const SO_ACCEPTCONN: i32 = 30;
    let int = |v: i32| Ok((v.to_le_bytes().to_vec(), None));
    if level == SOL_SOCKET {
        return match name {
            SO_TYPE => int(s.ty),
            SO_PROTOCOL => int(s.protocol),
            SO_DOMAIN => int(AF_NETLINK),
            SO_ERROR => int(if s.overrun.take() {
                Errno::ENOBUFS.0
            } else {
                0
            }),
            SO_ACCEPTCONN => int(0),
            SO_PASSCRED => int(s.passcred as i32),
            SO_RCVTIMEO => Ok((encode_timeval(s.rcvtimeo), None)),
            SO_SNDTIMEO => Ok((encode_timeval(s.sndtimeo), None)),
            SO_PEERCRED => {
                let mut b = 0i32.to_le_bytes().to_vec();
                b.extend_from_slice(&(-1i32).to_le_bytes());
                b.extend_from_slice(&(-1i32).to_le_bytes());
                Ok((b, None))
            }
            _ => s
                .opts
                .get(name)
                .map(|b| (b, None))
                .ok_or(Errno::ENOPROTOOPT),
        };
    }
    if level != SOL_NETLINK {
        return Err(Errno::ENOPROTOOPT);
    }
    match name {
        NETLINK_LIST_MEMBERSHIPS => {
            let Some(groups) = s.groups else {
                return Ok((Vec::new(), Some(0)));
            };
            let size = (s.ngroups() as usize).div_ceil(8).next_multiple_of(4);
            let mut out = Vec::new();
            for word in 0..size / 4 {
                if room - out.len() < 4 {
                    break;
                }
                let w = groups.checked_shr(word as u32 * 32).unwrap_or(0) as u32;
                out.extend_from_slice(&w.to_le_bytes());
            }
            Ok((out, Some(size)))
        }
        NETLINK_PKTINFO
        | NETLINK_BROADCAST_ERROR
        | NETLINK_NO_ENOBUFS
        | NETLINK_LISTEN_ALL_NSID
        | NETLINK_CAP_ACK
        | NETLINK_EXT_ACK
        | NETLINK_GET_STRICT_CHK => {
            if room < 4 {
                return Err(Errno::EINVAL);
            }
            let on = match name {
                NETLINK_CAP_ACK => s.cap_ack,
                NETLINK_EXT_ACK => s.ext_ack,
                _ => s.flags & (1 << name) != 0,
            };
            int(on as i32)
        }
        _ => Err(Errno::ENOPROTOOPT),
    }
}

/// A timeout as `struct timeval` (0 for none).
fn encode_timeval(t: Option<Duration>) -> Vec<u8> {
    let t = t.unwrap_or_default();
    let mut b = (t.as_secs() as i64).to_le_bytes().to_vec();
    b.extend_from_slice(&(t.subsec_micros() as i64).to_le_bytes());
    b
}

/// `SO_RCVTIMEO`/`SO_SNDTIMEO` (Linux 6.1's `sock_set_timeout`): a short buffer is EINVAL and
/// microseconds outside `[0, 1000000)` EDOM; a negative time means not waiting at all (zero, so
/// reads and writes fail with EAGAIN at once); None means no timeout, as for 0 or a value at
/// least `KTIME_MAX` long (Linux treats such values as `MAX_SCHEDULE_TIMEOUT`).
pub(super) fn timeval(b: &[u8]) -> Result<Option<Duration>, Errno> {
    let (Some(sec), Some(usec)) = (b.get(..8), b.get(8..16)) else {
        return Err(Errno::EINVAL);
    };
    let sec = i64::from_le_bytes(sec.try_into().unwrap());
    let usec = i64::from_le_bytes(usec.try_into().unwrap());
    if !(0..1_000_000).contains(&usec) {
        return Err(Errno::EDOM);
    }
    if sec < 0 {
        return Ok(Some(Duration::ZERO));
    }
    let d = Duration::from_secs(sec as u64) + Duration::from_micros(usec as u64);
    Ok((!d.is_zero() && d < super::KTIME_MAX).then_some(d))
}

pub(super) fn getsockopt(
    cx: &mut Cx<'_>,
    fd: i32,
    level: i32,
    name: i32,
    value: u64,
    len: u64,
) -> SysResult {
    let (sock, _) = socket_of(cx, fd)?;
    let name = new_timeouts(level, name);
    let room = usize::try_from(i32::from_le_bytes(get(cx, len)?)).map_err(|_| Errno::EINVAL)?;
    let encode_tv = encode_timeval;
    if let Socket::Netlink(s) = &*sock.borrow() {
        let (out, size) = netlink_getsockopt(s, level, name, room)?;
        let n = room.min(out.len());
        cx.mm().write(UserSlice::new(value, n), &out[..n])?;
        let size = size.unwrap_or(n) as u32;
        return put(cx, len, size.to_le_bytes()).map(|()| 0);
    }
    let pid_ns = cx.pid_ns();
    // SO_ORIGINAL_DST: Linux writes only the sockaddr and writes optlen back unchanged
    if let Some(r) = original_dst(cx, &sock, level, name, room) {
        let out = r?;
        cx.mm().write(UserSlice::new(value, out.len()), &out)?;
        return Ok(0);
    }
    // SO_GET_FILTER: Linux's sk_get_filter takes optlen as a count of instructions (8 bytes
    // each), and optlen comes back as the program's count; with 0 only the count is asked, and
    // fewer than the program has is EINVAL. A program kept in Tuxport (InetSocket::filter) answers
    // itself; otherwise the host is asked with the count, never above BPF_MAXINSNS
    let inet = match &*sock.borrow() {
        Socket::Inet(s) => Some((s.fd, s.filter.clone())),
        _ => None,
    };
    if (level, name) == (SOL_SOCKET, SO_GET_FILTER)
        && let Some((host_fd, kept)) = inet
    {
        let (n, insns) = match kept {
            Some(insns) => {
                let n = insns.len() / 8;
                match room {
                    0 => (n, Vec::new()),
                    r if r < n => return Err(Errno::EINVAL),
                    _ => (n, insns),
                }
            }
            None => {
                let count = room.min(BPF_MAXINSNS) as u32;
                let (n, insns) = tuxport_hal::net::get_filter(host_fd, count).map_err(Errno)?;
                (n as usize, insns)
            }
        };
        cx.mm().write(UserSlice::new(value, insns.len()), &insns)?;
        return put(cx, len, (n as u32).to_le_bytes()).map(|()| 0);
    }
    // The ICMP errors the host recorded for raw ICMP sockets and for UDP sockets the aggregation
    // point watches must not be seen by the guest (crate::rawicmp)
    if (level, name) == (SOL_SOCKET, SO_ERROR)
        && let Some(t) = crate::rawicmp::before_io(cx.kr(), &sock)
    {
        let fd = match &*sock.borrow() {
            Socket::Inet(s) => s.fd,
            _ => unreachable!("raw ICMP and UDP sockets are inet"),
        };
        take_errors(cx, &t, fd);
    }
    // Raw ICMP's own options (crate::rawicmp::RawIcmp), IP_RECVERR of UDP sockets the
    // aggregation point watches, and the error to report that the aggregation point recorded for
    // them
    let raw = match &*sock.borrow() {
        Socket::Inet(s) if (level, name) == (SOL_SOCKET, SO_ERROR) && s.icmp_err.get() != 0 => {
            Some(Ok(s.icmp_err.replace(0).to_le_bytes().to_vec()))
        }
        Socket::Inet(InetSocket { icmp: Some(r), .. }) => r.getsockopt(level, name),
        Socket::Inet(s) => crate::rawicmp::udp_getsockopt(s, level, name).map(Ok),
        _ => None,
    };
    let out = if let Some(r) = raw {
        r.map_err(Errno)?
    } else {
        match &*sock.borrow() {
            Socket::Inet(s) => match (level, name) {
                (IPPROTO_IPV6, IPV6_V6ONLY) if s.net.is_some() && s.family == AF_INET6 => {
                    (s.v6only as i32).to_le_bytes().to_vec()
                }
                (SOL_SOCKET, SO_RCVTIMEO) => encode_tv(s.rcvtimeo),
                (SOL_SOCKET, SO_SNDTIMEO) => encode_tv(s.sndtimeo),
                // SO_MARK: the recorded one (the one on the host is set by Android's netd)
                (SOL_SOCKET, SO_MARK) => s.mark().to_le_bytes().to_vec(),
                // Raw ICMP is a ping socket on the host
                (SOL_SOCKET, SO_TYPE) if s.raw => SOCK_RAW.to_le_bytes().to_vec(),
                _ => {
                    tuxport_hal::net::getsockopt(s.fd, level, name, room.min(256)).map_err(Errno)?
                }
            },
            Socket::Netlink(_) => unreachable!("handled above"),
            Socket::Unix(u) => unix::getsockopt(u, level, name, &pid_ns, &cx.cred())?,
        }
    };
    // A nonblocking TCP connection was refused: the guest learns it here (the host clears it once
    // read, so it is never counted twice)
    if (level, name) == (SOL_SOCKET, SO_ERROR)
        && out.get(..4) == Some(&Errno::ECONNREFUSED.0.to_le_bytes())
    {
        let opening = matches!(&*sock.borrow(), Socket::Inet(s) if s.opening);
        if opening {
            count_connect(cx, &sock, Some(Errno::ECONNREFUSED));
        }
    }
    let n = room.min(out.len());
    cx.mm().write(UserSlice::new(value, n), &out[..n])?;
    put(cx, len, (n as u32).to_le_bytes()).map(|()| 0)
}

/// `SO_ORIGINAL_DST` (`SOL_IP`), `IP6T_SO_ORIGINAL_DST` (`SOL_IPV6`): as in Linux's `getorigdst`,
/// `ipv6_getorigdst` (`nf_conntrack_proto.c`, [`crate::origdst`]); `room` is the optlen the guest
/// gave. None means left to the host: other options, inet sockets that are neither TCP nor UDP,
/// and `SOL_IPV6` on an AF_INET socket (EOPNOTSUPP on Linux).
///
/// - Non-TCP is ENOPROTOOPT, an optlen too small (16, 28) is EINVAL, both before the lookup.
/// - Connection tracking keeps packets apart by protocol family: IPv4 connections (IPv4-mapped
///   addresses included) are found only with `SOL_IP`, IPv6 ones only with `SOL_IPV6`, and the
///   other one is ENOENT.
/// - The initiating end (one that called connect): the original destination of the original
///   direction is the one it connected to (the peer the guest sees), found only when the
///   namespace's packets go through connection tracking ([`crate::nft::Nft::tracks`]).
/// - The accepting end: when nat rewrote the initiating end's destination, the one claimed at
///   accept is reported; otherwise, when going through connection tracking, its own local address
///   (for a connection without nat, that is the original destination).
/// - Unconnected ones (listening, not yet connected): Linux looks up with an all-zero peer,
///   ENOENT.
fn original_dst(
    cx: &Cx<'_>,
    sock: &SocketRef,
    level: i32,
    name: i32,
    room: usize,
) -> Option<Result<Vec<u8>, Errno>> {
    const IPPROTO_IP: i32 = 0;
    const SO_ORIGINAL_DST: i32 = 80;
    let b = sock.borrow();
    let Socket::Inet(s) = &*b else {
        return None;
    };
    if name != SO_ORIGINAL_DST || !matches!(level, IPPROTO_IP | IPPROTO_IPV6) || s.icmp.is_some() {
        return None;
    }
    let v6 = level == IPPROTO_IPV6;
    if v6 && s.family != AF_INET6 {
        return None;
    }
    match nat_proto(s) {
        Some(p) if p == IPPROTO_TCP as u8 && s.ty == SOCK_STREAM => {}
        Some(_) => return Some(Err(Errno::ENOPROTOOPT)),
        None => return None,
    }
    if room < if v6 { 28 } else { 16 } {
        return Some(Err(Errno::EINVAL));
    }
    // The protocol family follows the connection's address: the initiating end looks at what it
    // connected to (in a new namespace the local end of an AF_INET6 socket is 127.a.b.1, which
    // translates back to ::1, so whether it connected to an IPv4-mapped address cannot be told),
    // the accepting end at its local end
    let found = (|| {
        let local = inet_name(cx, s, false).ok()?;
        let fam_of = s.guest_peer.as_deref().unwrap_or(&local);
        let tracks = net_of(cx, s)
            .nft
            .borrow()
            .tracks(crate::origdst::is_v6(fam_of));
        let orig = if let Some(p) = &s.guest_peer {
            tracks.then(|| p.clone())?
        } else {
            tuxport_hal::net::sockname(s.fd, true).ok()?;
            match s.track.as_ref().and_then(|t| t.orig_dst.clone()) {
                Some(o) => o,
                None if tracks => local,
                None => return None,
            }
        };
        crate::origdst::encode(&orig, v6).filter(|_| crate::origdst::is_v6(&orig) == v6)
    })();
    Some(found.ok_or(Errno::ENOENT))
}

// ---- Sending and receiving ----

/// One received message.
pub(crate) struct Message {
    pub data: Vec<u8>,
    /// The full length of the message (may exceed `data` with `MSG_TRUNC`).
    pub len: usize,
    pub addr: Vec<u8>,
    pub control: Vec<u8>,
    pub flags: u32,
}

/// Appends a control message the way Linux's `put_cmsg` writes it into a buffer with `room` bytes
/// left: with less than a header left nothing is written; with less than the whole message the
/// header and as much data as fits, its `cmsg_len` cut to what was written; `MSG_CTRUNC` in
/// either case. `room` shrinks by the message's aligned length (at most to 0).
pub(super) fn put_cmsg(
    control: &mut Vec<u8>,
    room: &mut usize,
    flags: &mut u32,
    level: i32,
    ty: i32,
    data: &[u8],
) {
    const MSG_CTRUNC: u32 = 0x8;
    const CMSG_HDR: usize = 16;
    if *room < CMSG_HDR {
        *flags |= MSG_CTRUNC;
        return;
    }
    let mut len = CMSG_HDR + data.len();
    if *room < len {
        *flags |= MSG_CTRUNC;
        len = *room;
    }
    control.extend_from_slice(&(len as u64).to_le_bytes());
    control.extend_from_slice(&level.to_le_bytes());
    control.extend_from_slice(&ty.to_le_bytes());
    control.extend_from_slice(&data[..len - CMSG_HDR]);
    let space = (CMSG_HDR + data.len()).next_multiple_of(8).min(*room);
    control.resize(control.len() + space - len, 0);
    *room -= space;
}

/// The guest's `struct msghdr`.
struct MsgHdr {
    name: u64,
    namelen: u32,
    /// (base, length).
    iovecs: Vec<(u64, usize)>,
    control: u64,
    controllen: usize,
}

/// The size of `struct mmsghdr`: a msghdr followed by msg_len.
const MMSGHDR_SIZE: u64 = 64;
/// The most messages sendmmsg and recvmmsg handle at once (`UIO_MAXIOV`).
const MMSG_MAX: u32 = 1024;
const MSG_WAITFORONE: u32 = 0x1_0000;

pub(super) fn sendto(
    cx: &mut Cx<'_>,
    fd: i32,
    buf: u64,
    len: usize,
    flags: u32,
    addr: u64,
    addrlen: u32,
) -> SysResult {
    let (sock, nonblock) = socket_of(cx, fd)?;
    let len = len.min(MSG_MAX);
    let mut data = vec![0u8; len];
    cx.mm().read(UserSlice::new(buf, len), &mut data)?;
    let addr = if addr != 0 {
        read_addr(cx, addr, addrlen)?
    } else {
        Vec::new()
    };
    send(cx, &sock, nonblock, &data, &addr, &[], flags).map(|n| n as u64)
}

pub(super) fn recvfrom(
    cx: &mut Cx<'_>,
    fd: i32,
    buf: u64,
    len: usize,
    flags: u32,
    addr: u64,
    addrlen: u64,
) -> SysResult {
    let (sock, nonblock) = socket_of(cx, fd)?;
    let m = recv(cx, &sock, nonblock, len, 0, flags)?;
    cx.mm().write(UserSlice::new(buf, m.data.len()), &m.data)?;
    let more = recv_rest(cx, &sock, nonblock, m.len, len, flags, |cx, off, d| {
        cx.mm().write(UserSlice::new(buf + off as u64, d.len()), d)
    });
    write_addr(cx, addr, addrlen, &m.addr)?;
    Ok((if flags & MSG_TRUNC != 0 {
        m.len
    } else {
        m.data.len()
    } + more) as u64)
}

/// recvfrom and recvmsg with `MSG_WAITALL` (without `MSG_PEEK`) on an internet stream socket: one
/// [`recv`] takes at most [`MSG_MAX`], so a longer request goes on piece by piece, each written out
/// by `put` at its offset, until it is complete or a piece comes up short (end of file, a signal,
/// the timeout, an error), as Linux's tcp_recvmsg returns what it copied. `first` is the length the
/// first piece gave (its `len`, as with `MSG_TRUNC` a stream piece carries no data); returns how
/// much more was received.
fn recv_rest(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
    first: usize,
    total: usize,
    flags: u32,
    mut put: impl FnMut(&mut Cx<'_>, usize, &[u8]) -> Result<(), Errno>,
) -> usize {
    let stream = matches!(&*sock.borrow(), Socket::Inet(s) if s.ty == SOCK_STREAM);
    if !stream || flags & (MSG_WAITALL | MSG_PEEK) != MSG_WAITALL {
        return 0;
    }
    let mut got = first;
    let mut last = first;
    while last == MSG_MAX && got < total {
        let Ok(m) = recv(cx, sock, nonblock, total - got, 0, flags) else {
            break;
        };
        if put(cx, got, &m.data).is_err() {
            break;
        }
        last = m.len;
        got += last;
    }
    got - first
}

/// sendmmsg: sendmsg one by one. Returns the error when the first one fails, otherwise the number
/// sent.
pub(super) fn sendmmsg(cx: &mut Cx<'_>, fd: i32, vec: u64, vlen: u32, flags: u32) -> SysResult {
    let mut done = 0;
    for i in 0..vlen.min(MMSG_MAX) as u64 {
        let hdr = vec + i * MMSGHDR_SIZE;
        match sendmsg(cx, fd, hdr, flags) {
            Ok(n) => put(cx, hdr + 56, (n as u32).to_le_bytes())?,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        }
        done += 1;
    }
    Ok(done)
}

pub(super) fn sendmsg(cx: &mut Cx<'_>, fd: i32, msg: u64, flags: u32) -> SysResult {
    let (sock, nonblock) = socket_of(cx, fd)?;
    let MsgHdr {
        name,
        namelen,
        iovecs,
        control,
        controllen,
    } = read_msghdr(cx, msg)?;
    let mut data = Vec::new();
    for (base, len) in iovecs {
        let len = len.min(MSG_MAX - data.len());
        let start = data.len();
        data.resize(start + len, 0);
        cx.mm()
            .read(UserSlice::new(base, len), &mut data[start..])?;
    }
    let addr = if name != 0 && namelen > 0 {
        read_addr(cx, name, namelen)?
    } else {
        Vec::new()
    };
    let mut cmsg = vec![0u8; controllen.min(4096)];
    if !cmsg.is_empty() {
        cx.mm()
            .read(UserSlice::new(control, cmsg.len()), &mut cmsg)?;
    }
    send(cx, &sock, nonblock, &data, &addr, &cmsg, flags).map(|n| n as u64)
}

/// recvmmsg: recvmsg one by one. With `MSG_WAITFORONE` it no longer waits after the first is
/// received; the timeout is checked only after each one received, as on Linux.
pub(super) fn recvmmsg(
    cx: &mut Cx<'_>,
    fd: i32,
    vec: u64,
    vlen: u32,
    flags: u32,
    timeout: u64,
) -> SysResult {
    let deadline = super::poll::deadline_ts(cx, timeout)?;
    let mut flags = flags;
    let mut done = 0;
    for i in 0..vlen.min(MMSG_MAX) as u64 {
        let hdr = vec + i * MMSGHDR_SIZE;
        match recvmsg(cx, fd, hdr, flags & !MSG_WAITFORONE) {
            Ok(n) => put(cx, hdr + 56, (n as u32).to_le_bytes())?,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        }
        done += 1;
        if flags & MSG_WAITFORONE != 0 {
            flags |= MSG_DONTWAIT;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
    }
    Ok(done)
}

pub(super) fn recvmsg(cx: &mut Cx<'_>, fd: i32, msg: u64, flags: u32) -> SysResult {
    let (sock, nonblock) = socket_of(cx, fd)?;
    let MsgHdr {
        name,
        namelen,
        iovecs,
        control,
        controllen,
    } = read_msghdr(cx, msg)?;
    let total: usize = iovecs.iter().map(|v| v.1).sum();
    // AF_UNIX data is taken off its queue: when the part of the iovecs that the queued data
    // would fill cannot take it, fail first, leaving the data queued (Linux keeps a stream's
    // message when copying it faults)
    let queued = match &*sock.borrow() {
        Socket::Unix(u) => u.unread().unwrap_or(0),
        _ => 0,
    };
    let mut need = total.min(queued);
    for &(base, len) in &iovecs {
        if need == 0 {
            break;
        }
        let n = len.min(need);
        if !cx.mm().can_write(base, n as u64) {
            return Err(Errno::EFAULT);
        }
        need -= n;
    }
    // A NULL control buffer holds nothing whatever its length (scm_recv): control messages are
    // cut (MSG_CTRUNC) and passed files dropped
    let room = if control == 0 {
        0
    } else {
        controllen.min(4096)
    };
    let mut m = recv(cx, &sock, nonblock, total, room, flags)?;
    scatter(cx, &iovecs, 0, &m.data)?;
    let more = recv_rest(cx, &sock, nonblock, m.len, total, flags, |cx, off, d| {
        scatter(cx, &iovecs, off, d)
    });
    // Write msg_namelen, msg_controllen, msg_flags back to the msghdr
    if name != 0 {
        let n = (namelen as usize).min(m.addr.len());
        cx.mm().write(UserSlice::new(name, n), &m.addr[..n])?;
        put(cx, msg + 8, (m.addr.len() as u32).to_le_bytes())?;
    }
    let mut n = controllen.min(m.control.len());
    // A control buffer that cannot be written loses its messages, not the data: the passed files
    // are not installed (receive_fd_user) and MSG_CTRUNC is set
    if n > 0
        && cx
            .mm()
            .write(UserSlice::new(control, n), &m.control[..n])
            .is_err()
    {
        unix::drop_passed_files(cx, &m.control);
        m.flags |= MSG_CTRUNC;
        n = 0;
    }
    put(cx, msg + 40, (n as u64).to_le_bytes())?;
    // As on Linux, msg_flags echoes the MSG_CMSG_CLOEXEC the caller gave
    const MSG_CMSG_CLOEXEC: u32 = 0x4000_0000;
    put(
        cx,
        msg + 48,
        (m.flags | flags & MSG_CMSG_CLOEXEC).to_le_bytes(),
    )?;
    Ok((if flags & MSG_TRUNC != 0 {
        m.len
    } else {
        m.data.len()
    } + more) as u64)
}

/// Writes `data` over the iovecs in order, starting `off` bytes into them.
fn scatter(cx: &mut Cx<'_>, iovecs: &[(u64, usize)], off: usize, data: &[u8]) -> Result<(), Errno> {
    let mut skip = off;
    let mut done = 0;
    for &(base, len) in iovecs {
        if skip >= len {
            skip -= len;
            continue;
        }
        let n = (len - skip).min(data.len() - done);
        if n == 0 {
            break;
        }
        cx.mm()
            .write(UserSlice::new(base + skip as u64, n), &data[done..done + n])?;
        done += n;
        skip = 0;
    }
    Ok(())
}

/// read, readv, write and writev on a socket (Linux's `sock_read_iter` and `sock_write_iter`): a
/// recvmsg or sendmsg without an address, control data or flags over all the iovecs, so a
/// datagram is one message whatever the iovecs or the buffer size. A stream socket is written
/// until everything has gone (a blocking write writes it all, as Linux's TCP and AF_UNIX streams
/// do), in pieces of at most [`MSG_MAX`]; a read of nothing returns 0 at once.
pub(super) fn rw(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
    iovecs: &[(u64, usize)],
    write: bool,
) -> SysResult {
    if !write {
        let total: usize = iovecs.iter().map(|v| v.1).sum();
        if total == 0 {
            return Ok(0);
        }
        let m = recv(cx, sock, nonblock, total, 0, 0)?;
        let mut off = 0;
        for &(base, len) in iovecs {
            let n = len.min(m.data.len() - off);
            if n == 0 {
                break;
            }
            cx.mm()
                .write(UserSlice::new(base, n), &m.data[off..off + n])?;
            off += n;
        }
        return Ok(m.data.len() as u64);
    }
    let stream = match &*sock.borrow() {
        Socket::Inet(s) => s.ty == SOCK_STREAM,
        Socket::Unix(u) => u.ty == SOCK_STREAM,
        Socket::Netlink(_) => false,
    };
    // The iovecs not yet gathered, and the bytes sent so far. A fault ends a stream's data there
    // and what came before it is still sent, as TCP's copy does; a message with a fault is not
    // sent at all (EFAULT)
    let mut rest = iovecs.iter().copied();
    let mut cur = None;
    let mut done = 0;
    loop {
        let mut data = Vec::new();
        let mut fault = None;
        while data.len() < MSG_MAX {
            let Some((base, len)) = cur.take().or_else(|| rest.next()) else {
                break;
            };
            let n = len.min(MSG_MAX - data.len());
            let start = data.len();
            data.resize(start + n, 0);
            if let Err(e) = cx.mm().read(UserSlice::new(base, n), &mut data[start..]) {
                data.truncate(start);
                fault = Some(e);
                break;
            }
            if n < len {
                cur = Some((base.wrapping_add(n as u64), len - n));
            }
        }
        if let Some(e) = fault.filter(|_| data.is_empty() || !stream) {
            return if done > 0 { Ok(done as u64) } else { Err(e) };
        }
        let last = fault.is_some() || (cur.is_none() && rest.len() == 0);
        // Once some data has gone, a broken connection ends the write with its count and no
        // SIGPIPE (the next call gets both)
        let flags = if done > 0 { MSG_NOSIGNAL } else { 0 };
        match send(cx, sock, nonblock, &data, &[], &[], flags) {
            Ok(n) => {
                done += n;
                if !stream || last || n < data.len() {
                    return Ok(done as u64);
                }
            }
            Err(_) if done > 0 => return Ok(done as u64),
            Err(e) => return Err(e),
        }
    }
}

pub(super) fn shutdown(cx: &mut Cx<'_>, fd: i32, how: i32) -> SysResult {
    let (sock, _) = socket_of(cx, fd)?;
    match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            const SHUT_RD: i32 = 0;
            const SHUT_RDWR: i32 = 2;
            let r = tuxport_hal::net::shutdown(s.fd, how).map_err(Errno);
            // The host records the shutdown even on an unconnected socket, failing with ENOTCONN
            // as Linux's inet_shutdown does, and recv needs to know about it (InetSocket::rd_shut)
            if matches!(r, Ok(()) | Err(Errno::ENOTCONN)) && matches!(how, SHUT_RD | SHUT_RDWR) {
                s.rd_shut = true;
            }
            r?
        }
        Socket::Netlink(_) => {}
        Socket::Unix(u) => unix::shutdown(u, how)?,
    }
    Ok(0)
}

fn read_msghdr(cx: &Cx<'_>, msg: u64) -> Result<MsgHdr, Errno> {
    let b: [u8; 56] = get(cx, msg)?;
    let word = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let name = word(0);
    let namelen = u32::from_le_bytes(b[8..12].try_into().unwrap());
    let iov = word(16);
    let iovlen = word(24) as usize;
    if iovlen > 1024 {
        return Err(Errno::EMSGSIZE);
    }
    // As Linux's import_iovec: a negative length (as ssize_t) anywhere is EINVAL; then each iovec
    // must lie below arm64's TASK_SIZE_MAX (access_ok, EFAULT), and the total is cut to
    // MAX_RW_COUNT by shortening the iovec that reaches it
    const MAX_RW_COUNT: u64 = 0x7fff_f000;
    const TASK_SIZE_MAX: u64 = 1 << 48;
    let mut iovecs = Vec::with_capacity(iovlen);
    for i in 0..iovlen as u64 {
        let v: [u8; 16] = get(cx, iov.wrapping_add(i * 16))?;
        let len = u64::from_le_bytes(v[8..].try_into().unwrap());
        if (len as i64) < 0 {
            return Err(Errno::EINVAL);
        }
        iovecs.push((u64::from_le_bytes(v[..8].try_into().unwrap()), len as usize));
    }
    let mut total = 0;
    for (base, len) in &mut iovecs {
        if *len as u64 > TASK_SIZE_MAX || *base > TASK_SIZE_MAX - *len as u64 {
            return Err(Errno::EFAULT);
        }
        *len = (*len as u64).min(MAX_RW_COUNT - total) as usize;
        total += *len as u64;
    }
    Ok(MsgHdr {
        name,
        namelen,
        iovecs,
        control: word(32),
        controllen: word(40) as usize,
    })
}

/// Sends one message (shared by send, sendto, sendmsg, write).
pub(crate) fn send(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
    data: &[u8],
    addr: &[u8],
    control: &[u8],
    flags: u32,
) -> Result<usize, Errno> {
    if is_unix(sock) {
        return unix::send(cx, sock, nonblock, data, addr, control, flags);
    }
    if matches!(&*sock.borrow(), Socket::Netlink(_)) {
        return netlink_send(cx, sock, data);
    }
    let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
    let (host_fd, ty, timeout, to_stub) = match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            if let Some(r) = &mut s.icmp {
                r.sent(data);
            }
            let to_stub = stub_capable(s) && (is_stub(addr) || (addr.is_empty() && s.stub_peer));
            (s.fd, s.ty, s.sndtimeo, to_stub)
        }
        Socket::Netlink(_) | Socket::Unix(_) => unreachable!("handled above"),
    };
    if to_stub {
        // DNS stub: a host thread resolves it while the send returns at once, as a UDP send does
        // on Linux; the answer arrives later like any datagram (deliver_dns), so nonblocking
        // sockets and event loops never wait here, and their own timeouts and parallel queries
        // keep working. Like a UDP send, it autobinds an unbound socket (inet_send_prepare), so
        // the host socket is bound to the wildcard address; past DNS_MAX_INFLIGHT queries
        // resolving at once, a query is dropped, as by a server too busy to answer
        if tuxport_hal::net::sockname(host_fd, false).is_ok_and(|a| port_of(&a) == Some(0)) {
            let family = match &*sock.borrow() {
                Socket::Inet(s) => s.family,
                _ => AF_INET,
            };
            let mut any = vec![0u8; if family == AF_INET6 { 28 } else { 16 }];
            any[..2].copy_from_slice(&(family as u16).to_le_bytes());
            let _ = tuxport_hal::net::bind(host_fd, &any);
        }
        if cx.kr().dns_queries.len() >= DNS_MAX_INFLIGHT {
            crate::debug::log(format_args!(
                "DNS stub: too many queries at once, one dropped"
            ));
            return Ok(data.len());
        }
        let query = data.to_vec();
        let id = {
            let k = cx.k();
            k.dns_last += 1;
            k.dns_queries.insert(k.dns_last, Rc::downgrade(sock));
            k.dns_last
        };
        let answers = cx.shared.dns_answers.clone();
        let timers = cx.shared.timers.clone();
        let started = std::thread::Builder::new()
            .name("dns-stub".into())
            .spawn(move || {
                let answer = tuxport_hal::net::dns_query(&query).unwrap_or_else(|e| {
                    crate::debug::log(format_args!("DNS stub: query failed: errno {e}"));
                    servfail(&query)
                });
                answers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((id, answer));
                timers.signal();
            });
        if started.is_err() {
            cx.k().dns_queries.remove(&id);
            return Err(Errno::EAGAIN);
        }
        return Ok(data.len());
    }
    // Translate the destination address to the host's (crate::netns: ENETUNREACH when
    // unreachable), binding first if unbound; record the namespace the destination is in (low
    // port mapping follows it)
    let w = cx.kr().world();
    let init = cx.kr().init_ns.net.clone();
    let cred = cx.cred();
    let mut key = None;
    // The destination as seen in its namespace is an IPv6 address (map_dest)
    let mut dest_v6 = false;
    // A socket bound to the host's wildcard address sending to a mapped destination: specify the
    // source address (Socket::wide)
    let mut pktinfo = None;
    // The destination of a connected UDP socket was dropped by a filter chain (recorded at
    // connect)
    if addr.is_empty()
        && let Socket::Inet(s) = &*sock.borrow()
        && s.ty != SOCK_STREAM
        && let Some(e) = s.track.as_ref().and_then(|k| k.blocked)
    {
        return if e == 0 {
            Ok(data.len())
        } else {
            Err(Errno(e))
        };
    }
    let host_addr = match &mut *sock.borrow_mut() {
        Socket::Inet(s) if ty != SOCK_STREAM && (s.net.is_some() || !addr.is_empty()) => {
            let me = s.net.clone().unwrap_or_else(|| init.clone());
            if let (true, Some(v)) = (addr.is_empty(), &s.vconn) {
                pktinfo = crate::netns::implicit_bind(&me, s.family, false);
                Some(v.clone())
            } else if addr.is_empty() {
                let src = crate::netns::implicit_bind(&me, s.family, s.icmp6());
                ensure_bound(s, src.as_deref())?;
                Some(Vec::new())
            } else {
                // nftables nat rewrote the destination: record it, so the source of datagrams
                // coming back is translated back
                let Routed {
                    m,
                    dst,
                    there,
                    actual,
                    verdict,
                    ..
                } = route(&w, s, &me, addr, &cred)?;
                // Dropped or rejected by a filter chain: EPERM on OUTPUT (Linux's ip_local_out),
                // silently dropped elsewhere
                match verdict {
                    (crate::nft::Outcome::Accept | crate::nft::Outcome::Dnat(..), _) => {}
                    (_, true) => return Err(Errno::EPERM),
                    (_, false) => return Ok(data.len()),
                }
                // Sending to a port nobody binds in a new network namespace (or bound only to
                // other addresses): dropped on Linux, and it cannot be left to the host (a UDP
                // socket bound to the wildcard address outside would receive it)
                if nat_proto(s) == Some(IPPROTO_UDP as u8)
                    && let Some(t) = &m.target
                    && t.host_net.is_some()
                    && !crate::netns::listening(t, &there, true)
                {
                    return Ok(data.len());
                }
                if let Some(t) = &actual
                    && !s
                        .track
                        .as_ref()
                        .is_some_and(|k| k.nat_back.iter().any(|(x, o)| x == t && o == addr))
                {
                    let back = &mut s.track_mut().nat_back;
                    if back.len() >= NAT_BACK_MAX {
                        back.remove(0);
                    }
                    back.push((t.clone(), addr.to_vec()));
                }
                let dst = &dst[..];
                if crate::netns::is_inet(addr) {
                    // An unbound UDP socket in a new namespace sending to an address in the
                    // namespace for the first time: bind the wildcard address on the host too
                    // (Socket::wide), with IP_PKTINFO specifying the source address below, the
                    // same as one that bound the wildcard address with port 0 first (musl's
                    // resolver, Node, Go and Java do so). Bound to the loopback address, it could
                    // not send to the outside later (the host gives EINVAL). Sockets sending to
                    // their own virtual address still bind 127.a.b.254: answers are delivered
                    // back there by the registered port (Loopback::self_udp)
                    let source = m.source.as_deref().filter(|src| {
                        s.net.is_none()
                            || nat_proto(s) != Some(IPPROTO_UDP as u8)
                            || crate::netns::is_self_source(&me, src)
                    });
                    ensure_bound(s, source)?;
                }
                key = Some(m.target.as_ref().map_or(0, key_of));
                dest_v6 = ipv6_proper(&there);
                if s.wide && m.target.is_some() {
                    pktinfo = crate::netns::implicit_bind(&me, s.family, false);
                }
                // Bound to the wildcard address in the boot namespace (a UDP server answering a
                // client in a new namespace): the source address the host chooses is 127.0.0.1,
                // and the other side must see the virtual address facing it (an unbound one was
                // bound by ensure_bound)
                if s.net.is_none()
                    && let Some(src) = &m.source
                    && tuxport_hal::net::sockname(s.fd, false)
                        .ok()
                        .is_some_and(|a| crate::netns::is_any(&a))
                {
                    pktinfo = Some(src.clone());
                }
                Some(crate::netns::toward_self(&me, dst, m.addr))
            }
        }
        _ => None,
    };
    let addr = host_addr.as_deref().unwrap_or(addr);
    // TCP ignores the given address (except with MSG_FASTOPEN, Linux's tcp_sendmsg_locked), so
    // it is not passed to the host here either: the host's LSM may check it by address family
    // (Ubuntu 24.04's AppArmor rejects AF_UNSPEC with EAFNOSUPPORT)
    let addr = if ty == SOCK_STREAM && flags & MSG_FASTOPEN == 0 {
        &[][..]
    } else {
        addr
    };
    // Sending to a mapped low port on the local machine: switch to the host's (crate::portmap)
    let mapped = match &*sock.borrow() {
        Socket::Inet(s) if !addr.is_empty() => {
            let k = key.unwrap_or_else(|| ns_key(s));
            let (a, pm) = map_dest(&cx.kr().portmap, s, addr.to_vec(), k, dest_v6);
            pm.map(|_| a)
        }
        _ => None,
    };
    let addr = mapped.as_deref().unwrap_or(addr);
    // Raw ICMP sockets and UDP sockets handed to the aggregation point (crate::rawicmp): ICMP
    // errors the host recorded for them would make the send fail, so take them off first; when
    // the send fails because of one, take it off too and try again
    let tap = crate::rawicmp::before_io(cx.kr(), sock);
    if let Some(t) = &tap {
        take_errors(cx, t, host_fd);
        if let Some(e) = crate::rawicmp::take_error(sock) {
            return Err(e);
        }
        // A connected UDP socket sending to another address (busybox's traceroute connects to
        // the destination and then changes the port for every probe): the host only hands it
        // errors caused by packets that match the socket's connection, so connect it to this
        // address first. The peer the guest sees is still the one it connected to (getpeername)
        let connected_udp = matches!(&*sock.borrow(),
            Socket::Inet(s) if s.icmp.is_none() && s.peer.is_some());
        if connected_udp && !addr.is_empty() {
            let current = tuxport_hal::net::sockname(host_fd, true).unwrap_or_default();
            if !same_endpoint(&current, addr) {
                let _ = tuxport_hal::net::connect(host_fd, addr);
            }
        }
    }
    let with_src = pktinfo.map(|src| with_pktinfo(control, &src));
    let control = with_src.as_deref().unwrap_or(control);
    let mut retried = false;
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut done = 0;
    let mut sent_datagram = false;
    let r = loop {
        let r = tuxport_hal::net::sendmsg(
            host_fd,
            &data[done..],
            addr,
            control,
            flags as i32 & !(MSG_DONTWAIT | MSG_NOSIGNAL) as i32,
        );
        match r {
            Ok(n) => {
                done += n;
                sent_datagram = ty != SOCK_STREAM;
                // A blocking send on a stream socket must send all the data
                if ty != SOCK_STREAM || done == data.len() || nonblock {
                    break Ok(done);
                }
            }
            Err(e) if e == Errno::EAGAIN.0 => {
                if let Err(err) = wait_host(
                    cx,
                    sock,
                    host_fd,
                    POLLOUT,
                    nonblock,
                    deadline,
                    Errno::EAGAIN,
                ) {
                    break if done > 0 { Ok(done) } else { Err(err) };
                }
            }
            // A stream that sent part of its data returns that count, and the error (with
            // SIGPIPE) comes with the next call, as tcp_sendmsg returns what it copied
            Err(e) if e == Errno::EPIPE.0 => {
                break if done > 0 {
                    Ok(done)
                } else {
                    Err(epipe(cx, flags))
                };
            }
            Err(e) if !retried && tap.is_some() && crate::rawicmp::from_icmp(e) => {
                if let Some(t) = &tap {
                    take_errors(cx, t, host_fd);
                }
                retried = true;
            }
            Err(e) => break if done > 0 { Ok(done) } else { Err(Errno(e)) },
        }
    };
    if done > 0 || sent_datagram {
        account(cx, sock, false, done, addr);
    }
    // Bound automatically by the host (on the first send to the outside in a new namespace): only
    // now is the port known, so register it
    if sent_datagram
        && let Socket::Inet(s) = &mut *sock.borrow_mut()
        && s.listener.is_none()
    {
        register_udp(s);
    }
    r
}

/// Appends to control messages `control` an `IP_PKTINFO` (`IPV6_PKTINFO`, for IPv4-mapped
/// addresses) specifying source address `src` (a sockaddr), unless the guest gave one itself.
fn with_pktinfo(control: &[u8], src: &[u8]) -> Vec<u8> {
    const IPPROTO_IP: i32 = 0;
    const IP_PKTINFO: i32 = 8;
    const IPV6_PKTINFO: i32 = 50;
    // struct cmsghdr: cmsg_len (size_t), cmsg_level, cmsg_type, with the data aligned to 8 bytes
    let mut off = 0;
    while off + 16 <= control.len() {
        let len = u64::from_le_bytes(control[off..off + 8].try_into().unwrap()) as usize;
        let level = i32::from_le_bytes(control[off + 8..off + 12].try_into().unwrap());
        let ty = i32::from_le_bytes(control[off + 12..off + 16].try_into().unwrap());
        if (level == IPPROTO_IP && ty == IP_PKTINFO)
            || (level == IPPROTO_IPV6 && ty == IPV6_PKTINFO)
        {
            return control.to_vec();
        }
        if len < 16 {
            break;
        }
        off += len.next_multiple_of(8);
    }
    let (level, ty, data) = if u16::from_le_bytes([src[0], src[1]]) as i32 == AF_INET6 {
        // struct in6_pktinfo: address, interface index
        let mut d = src[8..24].to_vec();
        d.extend_from_slice(&0i32.to_le_bytes());
        (IPPROTO_IPV6, IPV6_PKTINFO, d)
    } else {
        // struct in_pktinfo: interface index, ipi_spec_dst (the source address), ipi_addr
        let mut d = 0i32.to_le_bytes().to_vec();
        d.extend_from_slice(&src[4..8]);
        d.extend_from_slice(&[0; 4]);
        (IPPROTO_IP, IP_PKTINFO, d)
    };
    let mut out = control.to_vec();
    out.resize(out.len().next_multiple_of(8), 0);
    out.extend_from_slice(&((16 + data.len()) as u64).to_le_bytes());
    out.extend_from_slice(&level.to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&data);
    out.resize(out.len().next_multiple_of(8), 0);
    out
}

/// The other side has closed: sends SIGPIPE to the current thread unless `MSG_NOSIGNAL` is set.
pub(super) fn epipe(cx: &mut Cx<'_>, flags: u32) -> Errno {
    if flags & MSG_NOSIGNAL == 0 {
        let task = cx.task.clone();
        signal::send_to_task(cx.kr(), &task, Signal::PIPE, SigInfo::kernel());
    }
    Errno::EPIPE
}

/// Receives one message (shared by recv, recvfrom, recvmsg, read), at most `max` bytes.
pub(crate) fn recv(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
    max: usize,
    control_len: usize,
    flags: u32,
) -> Result<Message, Errno> {
    let max = max.min(MSG_MAX);
    if is_unix(sock) {
        return unix::recv(cx, sock, nonblock, max, control_len, flags);
    }
    // Reading the error queue (MSG_ERRQUEUE) never waits: EAGAIN when no errors are queued
    let nonblock = nonblock || flags & (MSG_DONTWAIT | MSG_ERRQUEUE) != 0;
    // Every message a raw ICMP socket receives goes through the aggregation point
    // (crate::rawicmp)
    let raw = match &*sock.borrow() {
        Socket::Inet(InetSocket { icmp: Some(r), .. }) => Some(r.tap.clone()),
        _ => None,
    };
    if let Some(tap) = raw {
        return raw_recv(cx, sock, &tap, nonblock, max, control_len, flags);
    }
    // Datagrams produced in process: DNS stub answers, netlink answers and notifications
    fn take<T: Clone>(q: &mut std::collections::VecDeque<T>, peek: bool) -> Option<T> {
        if peek {
            q.front().cloned()
        } else {
            q.pop_front()
        }
    }
    let peek = flags & MSG_PEEK != 0;
    let local = |d: Vec<u8>, addr: Vec<u8>| {
        let n = d.len().min(max);
        Message {
            data: d[..n].to_vec(),
            len: d.len(),
            addr,
            control: Vec::new(),
            flags: if d.len() > n { MSG_TRUNC } else { 0 },
        }
    };
    // The deadline for netlink answers (SO_RCVTIMEO), set the first time no answer can be read
    let mut netlink_deadline = None;
    let (host_fd, ty, timeout) = loop {
        let host = match &mut *sock.borrow_mut() {
            Socket::Inet(s) => {
                if let Some(d) = take(&mut s.answers, peek) {
                    return Ok(local(d, stub_addr(s.family)));
                }
                Some((s.fd, s.ty, s.rcvtimeo))
            }
            Socket::Netlink(s) => {
                // A dropped notification is reported before what is queued (sock_error)
                if s.overrun.take() {
                    return Err(Errno::ENOBUFS);
                }
                if let Some((d, group)) = take(&mut s.queue, peek) {
                    // From the kernel (pid 0), with the group a notification went to as a mask
                    let mut addr = vec![0u8; 12];
                    addr[..2].copy_from_slice(&(AF_NETLINK as u16).to_le_bytes());
                    if (1..=32).contains(&group) {
                        addr[8..].copy_from_slice(&(1u32 << (group - 1)).to_le_bytes());
                    }
                    let mut m = local(d, addr);
                    // netlink_recvmsg: the pktinfo (the group, 0 for an answer), then the
                    // credentials of the kernel, which sends as root (pid 0; uid and gid 0 as
                    // the receiver sees them, the overflow IDs when 0 is not mapped)
                    let mut room = control_len;
                    if s.flags & (1 << NETLINK_PKTINFO) != 0 {
                        let group = group.to_le_bytes();
                        put_cmsg(
                            &mut m.control,
                            &mut room,
                            &mut m.flags,
                            SOL_NETLINK,
                            NETLINK_PKTINFO,
                            &group,
                        );
                    }
                    if s.passcred {
                        let cred = cx.cred();
                        let mut ucred = 0i32.to_le_bytes().to_vec();
                        ucred.extend_from_slice(&cred.uid_out(0).to_le_bytes());
                        ucred.extend_from_slice(&cred.gid_out(0).to_le_bytes());
                        put_cmsg(
                            &mut m.control,
                            &mut room,
                            &mut m.flags,
                            SOL_SOCKET,
                            SCM_CREDENTIALS,
                            &ucred,
                        );
                    }
                    return Ok(m);
                }
                let timeo = s.rcvtimeo;
                netlink_deadline.get_or_insert_with(|| timeo.map(|t| Instant::now() + t));
                None
            }
            Socket::Unix(_) => unreachable!("handled above"),
        };
        match host {
            Some(h) => break h,
            // No netlink answer to read: wait until interrupted by a signal or until SO_RCVTIMEO
            // expires (before, it was ignored and the wait was forever)
            None => {
                let deadline = netlink_deadline.flatten();
                if nonblock || deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err(Errno::EAGAIN);
                }
                if cx.interrupted() {
                    return Err(Errno::ERESTARTSYS);
                }
                cx.wait_objects(deadline, &mut [], wake::FILE, &[wake::of(sock)])?;
            }
        }
    };
    // A UDP socket handed to the aggregation point: ICMP errors the host recorded for it would
    // make the receive fail, so take them off first; when the receive fails because of one, take
    // it off too and try again
    let tap = crate::rawicmp::before_io(cx.kr(), sock);
    if let Some(t) = &tap {
        take_errors(cx, t, host_fd);
        if flags & MSG_ERRQUEUE == 0
            && let Some(e) = crate::rawicmp::take_error(sock)
        {
            return Err(e);
        }
    }
    let mut retried = false;
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut buf = vec![0u8; max];
    let mut got = 0;
    // Peeked data will be read again later, and the error queue is not received traffic
    let counted = flags & (MSG_PEEK | MSG_ERRQUEUE) == 0;
    // MSG_WAITALL on a stream waits until `max` bytes are there, as Linux's tcp_recvmsg does: a
    // read takes them piece by piece, a peek looks at the head of the queue again each time
    let wait_all = flags & MSG_WAITALL != 0 && ty == SOCK_STREAM;
    let peek = flags & MSG_PEEK != 0;
    // The last wait for more data to peek ended because the host fd was ready
    let mut peek_ready = false;
    loop {
        let host_flags = flags & !(MSG_DONTWAIT | MSG_WAITALL);
        let into = if peek { &mut buf[..] } else { &mut buf[got..] };
        match tuxport_hal::net::recvmsg(host_fd, into, control_len, host_flags as i32) {
            Ok(r) => {
                // The peer is recorded (InetSocket::vconn): datagrams not from it are dropped
                // (peeked ones must be taken off too)
                let foreign = matches!(&*sock.borrow(),
                    Socket::Inet(s) if s.vconn.as_ref().is_some_and(|v| !same_endpoint(v, &r.addr)));
                if foreign {
                    if flags & MSG_PEEK != 0 {
                        let mut skip = vec![0u8; 1];
                        let _ = tuxport_hal::net::recvmsg(host_fd, &mut skip, 0, 0);
                    }
                    continue;
                }
                if peek {
                    let grew = r.len > got;
                    got = r.len.min(max);
                    // Wait for more unless the host fd was ready and nothing came (end of file,
                    // or a receive buffer too small to ever hold `max`): return what is there
                    // rather than spin
                    if wait_all
                        && r.len > 0
                        && got < max
                        && (grew || !peek_ready)
                        && let Ok(ready) = wait_peek(cx, sock, host_fd, max, nonblock, deadline)
                    {
                        peek_ready = ready;
                        continue;
                    }
                } else {
                    got += r.len.min(buf.len() - got);
                    if wait_all && r.len > 0 && got < max {
                        continue;
                    }
                }
                buf.truncate(got);
                let len = if ty == SOCK_STREAM { got } else { r.len };
                // A stream receive with MSG_TRUNC discards the data, copying nothing to the
                // buffer, and returns its length (tcp_recvmsg); the host wrote nothing into `buf`
                if ty == SOCK_STREAM && flags & MSG_TRUNC != 0 {
                    buf.clear();
                }
                if counted {
                    account(cx, sock, true, len, &r.addr);
                }
                let addr = match &*sock.borrow() {
                    Socket::Inet(s) if !r.addr.is_empty() => {
                        // From a mapped low port on the local machine: report the guest's port
                        // (crate::portmap)
                        let w = cx.kr().world();
                        let a = unmap_addr(&cx.kr().portmap, &w, s, r.addr, false);
                        let me = net_of(cx, s);
                        let a = match s.self_port.and_then(|_| crate::netns::from_self(&me, &a)) {
                            Some(v) => v,
                            None => crate::netns::from_host(
                                &w,
                                &me,
                                &a,
                                crate::netns::Role::Peer,
                                s.icmp6(),
                            ),
                        };
                        // From the destination DNAT rewrote to: report the one the guest sent to
                        match s
                            .track
                            .as_ref()
                            .and_then(|k| k.nat_back.iter().find(|(t, _)| same_endpoint(t, &a)))
                        {
                            Some((_, o)) => o.clone(),
                            None => a,
                        }
                    }
                    _ => r.addr,
                };
                // UDP: record the peers received from; sending back to them is ESTABLISHED
                // (filter chains)
                if let Socket::Inet(s) = &mut *sock.borrow_mut()
                    && s.ty == SOCK_DGRAM
                    && s.icmp.is_none()
                    && crate::netns::is_inet(&addr)
                    && flags & MSG_PEEK == 0
                    && !s
                        .track
                        .as_ref()
                        .is_some_and(|k| k.seen.iter().any(|p| same_endpoint(p, &addr)))
                {
                    let seen = &mut s.track_mut().seen;
                    if seen.len() >= NAT_BACK_MAX {
                        seen.remove(0);
                    }
                    seen.push(addr.clone());
                }
                return Ok(Message {
                    data: buf,
                    len,
                    addr,
                    control: r.control,
                    flags: r.flags as u32,
                });
            }
            Err(e) if e == Errno::EAGAIN.0 => {
                // Also wake up when DNS stub answers come in: they do not go through the host
                // socket
                if let Socket::Inet(s) = &mut *sock.borrow_mut()
                    && let Some(d) = take(&mut s.answers, peek)
                {
                    return Ok(local(d, stub_addr(s.family)));
                }
                // A datagram socket shut down for reading: a blocking receive with nothing queued
                // returns 0 (Linux's __skb_wait_for_more_packets checks RCV_SHUTDOWN before
                // waiting, timeouts and signals), while the nonblocking host socket answers EAGAIN
                // and polls readable, so waiting would spin. Programs wake their receiving threads
                // this way
                if !nonblock
                    && ty == SOCK_DGRAM
                    && matches!(&*sock.borrow(), Socket::Inet(s) if s.rd_shut)
                {
                    return Ok(Message {
                        data: Vec::new(),
                        len: 0,
                        addr: Vec::new(),
                        control: Vec::new(),
                        flags: 0,
                    });
                }
                // MSG_WAITALL with part of the data read: wait for the rest; nonblocking, a signal,
                // the timeout or an error ends it with what was read (Linux's tcp_recvmsg returns
                // what it copied and leaves the signal to be handled)
                if got == 0 {
                    wait_host(cx, sock, host_fd, POLLIN, nonblock, deadline, Errno::EAGAIN)?;
                } else if wait_host(cx, sock, host_fd, POLLIN, nonblock, deadline, Errno::EAGAIN)
                    .is_err()
                {
                    break;
                }
            }
            Err(e) if !retried && tap.is_some() && crate::rawicmp::from_icmp(e) => {
                if let Some(t) = &tap {
                    take_errors(cx, t, host_fd);
                }
                retried = true;
            }
            // An error after part of a MSG_WAITALL read: return the part (the host has taken the
            // error, so the next read sees end of file where Linux would report it)
            Err(_) if got > 0 => break,
            Err(e) => return Err(Errno(e)),
        }
    }
    // Part of a MSG_WAITALL read
    buf.truncate(got);
    if counted {
        account(cx, sock, true, got, &[]);
    }
    Ok(Message {
        len: got,
        data: buf,
        addr: Vec::new(),
        control: Vec::new(),
        flags: 0,
    })
}

/// Waits for a MSG_PEEK|MSG_WAITALL peek to have `want` bytes to look at: the host socket's
/// `SO_RCVLOWAT` is raised for the wait, as the host fd is readable as soon as anything is queued.
/// Returns whether the host fd was ready (rather than a wake-up for another reason).
fn wait_peek(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    fd: i32,
    want: usize,
    nonblock: bool,
    deadline: Option<Instant>,
) -> Result<bool, Errno> {
    const SO_RCVLOWAT: i32 = 18;
    if nonblock {
        return Err(Errno::EAGAIN);
    }
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return Err(Errno::EAGAIN);
    }
    if cx.interrupted() {
        return Err(Errno::ERESTARTSYS);
    }
    let old = tuxport_hal::net::getsockopt(fd, SOL_SOCKET, SO_RCVLOWAT, 4);
    let want = i32::try_from(want).unwrap_or(i32::MAX);
    let _ = tuxport_hal::net::setsockopt(fd, SOL_SOCKET, SO_RCVLOWAT, &want.to_le_bytes());
    let mut pfd = [(fd, POLLIN, 0)];
    let r = cx.wait_objects(deadline, &mut pfd, wake::FILE, &[wake::of(sock)]);
    if let Ok(old) = old {
        let _ = tuxport_hal::net::setsockopt(fd, SOL_SOCKET, SO_RCVLOWAT, &old);
    }
    r.map(|()| pfd[0].2 != 0)
}

/// Receiving on a raw ICMP socket: the messages are in its own queue in the aggregation point
/// ([`crate::rawicmp`]); first take off what has already arrived on the host, and when there is
/// nothing, wait together with the aggregation point's host fd.
fn raw_recv(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    tap: &Rc<RefCell<crate::rawicmp::Tap>>,
    nonblock: bool,
    max: usize,
    control_len: usize,
    flags: u32,
) -> Result<Message, Errno> {
    // The error queue: what the aggregation point put in when the guest enabled IP_RECVERR (never
    // waits)
    if flags & MSG_ERRQUEUE != 0 {
        let arrivals = crate::rawicmp::dispatch(tap, None);
        crate::rawicmp::account(cx.k(), arrivals);
        let e = crate::rawicmp::take_err(tap, sock).ok_or(Errno::EAGAIN)?;
        let s = sock.borrow();
        let Socket::Inet(
            s @ InetSocket {
                icmp: Some(raw), ..
            },
        ) = &*s
        else {
            unreachable!("raw ICMP socket");
        };
        let w = cx.kr().world();
        let me = net_of(cx, s);
        let addr = crate::netns::from_host(&w, &me, &e.addr, crate::netns::Role::Peer, s.icmp6());
        let (control, mut out_flags) =
            crate::rawicmp::fit_control(&raw.err_control(&e), control_len);
        let mut data = e.data;
        let len = data.len();
        if len > max {
            data.truncate(max);
            out_flags |= MSG_TRUNC;
        }
        return Ok(Message {
            data,
            len,
            addr,
            control,
            flags: out_flags | MSG_ERRQUEUE,
        });
    }
    let key = wake::of(sock);
    let timeout = match &*sock.borrow() {
        Socket::Inet(s) => s.rcvtimeo,
        _ => None,
    };
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        let arrivals = crate::rawicmp::dispatch(tap, None);
        crate::rawicmp::account(cx.k(), arrivals);
        if let Some(e) = crate::rawicmp::take_error(sock) {
            return Err(e);
        }
        if let Some(it) = crate::rawicmp::take(tap, key, flags & MSG_PEEK != 0) {
            let s = sock.borrow();
            let Socket::Inet(
                s @ InetSocket {
                    icmp: Some(raw), ..
                },
            ) = &*s
            else {
                unreachable!("raw ICMP socket");
            };
            let w = cx.kr().world();
            let me = net_of(cx, s);
            let src =
                crate::netns::from_host(&w, &me, &it.src, crate::netns::Role::Peer, s.icmp6());
            let (mut data, control) = raw.present(&it, key, &src, |a| local_addr(&w, &me, s, a));
            let len = data.len();
            let (control, mut out_flags) = crate::rawicmp::fit_control(&control, control_len);
            if len > max {
                data.truncate(max);
                out_flags |= MSG_TRUNC;
            }
            return Ok(Message {
                data,
                len,
                addr: src,
                control,
                flags: out_flags,
            });
        }
        if nonblock || deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        let fd = tap.borrow().fd();
        cx.wait_objects(
            deadline,
            &mut [(fd, POLLIN, 0)],
            wake::FILE,
            &[wake::of(sock)],
        )?;
    }
}

/// The local IPv4 address on the host as the guest sees it ([`crate::netns`]).
fn local_addr(
    w: &crate::netns::World,
    me: &crate::nsproxy::NetNs,
    s: &InetSocket,
    a: [u8; 4],
) -> [u8; 4] {
    let mut sa = vec![0u8; 16];
    sa[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    sa[4..8].copy_from_slice(&a);
    let role = crate::netns::Role::Local {
        any: false,
        vaddr: s.vaddr,
        peer: None,
    };
    let g = crate::netns::from_host(w, me, &sa, role, false);
    g.get(4..8).and_then(|b| b.try_into().ok()).unwrap_or(a)
}

/// Takes the ICMP errors the host recorded on `fd` (given to raw sockets,
/// [`crate::rawicmp::dispatch`]).
fn take_errors(cx: &mut Cx<'_>, tap: &Rc<RefCell<crate::rawicmp::Tap>>, fd: i32) {
    let arrivals = crate::rawicmp::dispatch(tap, Some(fd));
    crate::rawicmp::account(cx.k(), arrivals);
}

/// Records the traffic sent and received on a host socket ([`crate::traffic`]).
fn account(cx: &mut Cx<'_>, sock: &SocketRef, rx: bool, bytes: usize, peer: &[u8]) {
    // Traffic in new network namespaces is not counted on the host's interfaces
    if let Socket::Inet(s) = &mut *sock.borrow_mut()
        && s.net.is_none()
    {
        cx.k().traffic.account(s, rx, bytes, peer);
    }
}

// ---- netlink (the kernel/netlink map) ----

/// Sends a netlink request: the kernel answers it at once into the socket's queue. A change
/// request on `NETLINK_ROUTE` also notifies the sockets that joined the link, address or route
/// groups of a namespace it changed ([`rtnl_watched`], [`rtnl_notify`]).
fn netlink_send(cx: &mut Cx<'_>, sock: &SocketRef, data: &[u8]) -> Result<usize, Errno> {
    let notes = {
        let mut guard = sock.borrow_mut();
        let Socket::Netlink(s) = &mut *guard else {
            unreachable!("checked by the caller")
        };
        netlink_autobind(cx, s);
        let mut host = KernelHost::of(cx, s.net.clone());
        if s.protocol == NETLINK_ROUTE {
            host.watched = rtnl_watched(cx.kr(), Some((sock, s)));
        }
        // The nftables ruleset of the boot namespace changed: look again at which ports to
        // forward
        let init = &cx.kr().init_ns.net;
        let before = (s.net.is_none() && s.protocol == NETLINK_NETFILTER)
            .then(|| init.nft.borrow().generation);
        s.handle(data, &host);
        if before.is_some_and(|g| g != init.nft.borrow().generation) {
            forward_all(cx.kr());
        }
        host.notes.into_inner()
    };
    request_wake_object(wake::of(sock));
    rtnl_notify(cx.k(), notes);
    Ok(data.len())
}

/// The network namespaces with a socket in a notified rtnetlink group, a sending one `s` included
/// (it is borrowed: Linux notifies the sender too unless it asked for `NLM_F_ECHO`).
fn rtnl_watched(
    k: &crate::kernel::Kernel,
    sender: Option<(&SocketRef, &NetlinkSocket)>,
) -> Vec<Rc<crate::nsproxy::NetNs>> {
    let ns_of = |net: &Option<Rc<crate::nsproxy::NetNs>>| {
        net.clone().unwrap_or_else(|| k.init_ns.net.clone())
    };
    let mut out: Vec<Rc<crate::nsproxy::NetNs>> = Vec::new();
    for other in k.rtnl_sockets.iter().filter_map(|w| w.upgrade()) {
        let ns = if let Some((_, s)) = sender.filter(|(sock, _)| Rc::ptr_eq(&other, sock)) {
            s.subscribed().then(|| ns_of(&s.net))
        } else {
            match other.try_borrow().as_deref() {
                Ok(Socket::Netlink(o)) if o.subscribed() => Some(ns_of(&o.net)),
                _ => None,
            }
        };
        if let Some(ns) = ns.filter(|ns| !out.iter().any(|o| Rc::ptr_eq(o, ns))) {
            out.push(ns);
        }
    }
    out
}

/// Interface ioctls that change something (`SIOCSIF*` for flags, addresses, metric, MTU, name,
/// hardware address, transmit queue; `SIOCDIFADDR`; `SIOCADDRT`, `SIOCDELRT`): rtnetlink
/// listeners are told what they changed, as for change requests.
const CHANGING_IOCTLS: [u32; 15] = [
    0x890b, 0x890c, 0x8914, 0x8916, 0x8918, 0x891a, 0x891c, 0x891e, 0x8922, 0x8923, 0x8924, 0x8934,
    0x8936, 0x8937, 0x8943,
];

/// Queues the notifications of a change request on the sockets of each namespace that joined
/// their groups, and wakes them.
fn rtnl_notify(k: &mut crate::kernel::Kernel, notes: Vec<RtnlNotes>) {
    if notes.is_empty() {
        return;
    }
    k.rtnl_sockets.retain(|w| w.strong_count() > 0);
    for sock in k.rtnl_sockets.iter().filter_map(|w| w.upgrade()) {
        let Socket::Netlink(s) = &mut *sock.borrow_mut() else {
            continue;
        };
        let ns = s.net.clone().unwrap_or_else(|| k.init_ns.net.clone());
        let mut any = false;
        for (_, changes) in notes.iter().filter(|(n, _)| Rc::ptr_eq(n, &ns)) {
            for (group, msg) in changes {
                any |= s.notify(*group, msg);
            }
        }
        if any {
            request_wake_object(wake::of(&sock));
        }
    }
}

/// A namespace and the notifications (group, message) for what a request changed in it.
type RtnlNotes = (Rc<crate::nsproxy::NetNs>, Vec<(u32, Vec<u8>)>);

/// Host information for netlink answers: interfaces come from the host, traffic counters and the
/// routes the host handed over come from the kernel. In a new network namespace there is only its
/// lo ([`crate::netns`]), and traffic is not counted.
struct KernelHost<'a> {
    k: &'a crate::kernel::Kernel,
    cred: crate::cred::Cred,
    net: Option<Rc<crate::nsproxy::NetNs>>,
    /// The requesting process's pid namespace and fd table (RTM_GETNSID's NETNSA_PID,
    /// NETNSA_FD).
    pid_ns: Rc<crate::pidns::PidNs>,
    fds: Rc<crate::cell::RefCell<crate::fd::FdTable>>,
    /// The requesting process's number in its pid namespace and its name (NFT_MSG_NEWGEN's
    /// PROC_PID, PROC_NAME).
    proc_pid: u32,
    comm: [u8; 16],
    /// The namespaces with sockets listening for link, address and route changes
    /// ([`rtnl_watched`]), and the notifications for what this request changed in them.
    watched: Vec<Rc<crate::nsproxy::NetNs>>,
    notes: std::cell::RefCell<Vec<RtnlNotes>>,
}

impl<'a> KernelHost<'a> {
    /// The calling process looking at network namespace `net` (None: the boot one), watching
    /// none for notifications.
    fn of(cx: &'a Cx<'_>, net: Option<Rc<crate::nsproxy::NetNs>>) -> KernelHost<'a> {
        KernelHost {
            k: cx.kr(),
            cred: cx.cred(),
            net,
            pid_ns: cx.pid_ns(),
            fds: cx.t.fds.clone(),
            proc_pid: cx.pid_ns().vnr(cx.task.tgid).unwrap_or(0) as u32,
            comm: cx.task.comm.get(),
            watched: Vec::new(),
            notes: Default::default(),
        }
    }

    /// The watched namespaces as they are now ([`KernelHost::changed_since`]).
    fn snapshots(&self, watched: &[Rc<crate::nsproxy::NetNs>]) -> Vec<crate::net::Snapshot> {
        watched
            .iter()
            .map(|ns| crate::net::Snapshot::take(&self.at(ns)))
            .collect()
    }

    /// The notifications for what changed in the watched namespaces since `before` was taken.
    fn changed_since(
        &self,
        watched: &[Rc<crate::nsproxy::NetNs>],
        before: Vec<crate::net::Snapshot>,
    ) -> Vec<RtnlNotes> {
        let after = self.snapshots(watched);
        watched
            .iter()
            .zip(before)
            .zip(after)
            .map(|((ns, before), after)| (ns.clone(), after.changes(&before)))
            .filter(|(_, changes)| !changes.is_empty())
            .collect()
    }

    /// The same requester looking at namespace `ns`.
    fn at(&self, ns: &Rc<crate::nsproxy::NetNs>) -> KernelHost<'_> {
        KernelHost {
            k: self.k,
            cred: self.cred.clone(),
            net: (!Rc::ptr_eq(ns, &self.k.init_ns.net)).then(|| ns.clone()),
            pid_ns: self.pid_ns.clone(),
            fds: self.fds.clone(),
            proc_pid: self.proc_pid,
            comm: self.comm,
            watched: Vec::new(),
            notes: Default::default(),
        }
    }

    /// The network namespace this socket is in (the boot one is recorded as None).
    fn ns(&self) -> Rc<crate::nsproxy::NetNs> {
        self.net
            .clone()
            .unwrap_or_else(|| self.k.init_ns.net.clone())
    }

    /// Finds a network namespace by process (`by_pid`) or by namespace file: the process must not
    /// have exited yet (Linux's get_net_ns_by_pid, ESRCH), and the fd must be a network namespace
    /// file (get_net_ns_by_fd, EINVAL).
    fn net_of(&self, by_pid: bool, value: u32) -> Result<Rc<crate::nsproxy::NetNs>, Errno> {
        if by_pid {
            let tid = self.pid_ns.find(value as i32).ok_or(Errno::ESRCH)?;
            // The virtual process 1 (with no Task) is in the boot namespace
            if tid == crate::kernel::INIT && !self.k.tasks.contains_key(&tid) {
                return Ok(self.k.init_ns.net.clone());
            }
            let t = self.k.tasks.get(&tid).ok_or(Errno::ESRCH)?;
            return Ok(t.ns.borrow().net.clone());
        }
        let file = self.fds.borrow().get(value as i32)?;
        match &file.borrow().kind {
            FileKind::Proc {
                ns: Some(crate::nsproxy::NsRef::Net(n)),
                ..
            } => Ok(n.clone()),
            _ => Err(Errno::EINVAL),
        }
    }
}

impl crate::net::Host for KernelHost<'_> {
    fn interfaces(&self) -> Vec<tuxport_hal::net::Interface> {
        crate::netdev::base(&self.ns())
    }

    fn links(&self) -> Vec<crate::net::Link> {
        let ns = self.ns();
        let mut all: Vec<crate::net::Link> = crate::netdev::base(&ns)
            .into_iter()
            .map(crate::net::Link::Host)
            .collect();
        for v in crate::netdev::views(&ns) {
            let nsid = v
                .link
                .as_ref()
                .and_then(|(_, other)| other.as_ref())
                .map(|o| crate::netdev::nsid_alloc(&ns, o));
            all.push(crate::net::Link::Virt(v, nsid));
        }
        all.sort_by_key(|l| match l {
            crate::net::Link::Host(i) => i.index,
            crate::net::Link::Virt(v, _) => v.index,
        });
        all
    }

    fn tables(&self) -> Vec<crate::route::Entry> {
        crate::netdev::tables(&self.ns(), self.routes())
    }

    fn all_interfaces(&self) -> Vec<tuxport_hal::net::Interface> {
        crate::netdev::interfaces(&self.ns())
    }

    fn init_net(&self) -> bool {
        self.net.is_none()
    }

    fn net_admin(&self) -> bool {
        self.cred
            .ns_capable(&self.ns().owner, crate::cred::cap::NET_ADMIN)
    }

    fn stats(&self, name: &str) -> crate::traffic::IfStats {
        match &self.net {
            Some(_) => Default::default(),
            None => self.k.traffic.get(name),
        }
    }

    fn routes(&self) -> Option<Vec<tuxport_hal::net::Route>> {
        match &self.net {
            Some(_) => None,
            None => crate::route::host_routes(self.k),
        }
    }

    fn tx_queue_len(&self, name: &str) -> Option<u32> {
        match &self.net {
            Some(_) => Some(1000),
            None => tuxport_hal::net::tx_queue_len(name),
        }
    }

    fn uid(&self) -> u32 {
        self.cred.ruid
    }

    fn configure(&self, req: &[u8]) -> Option<Result<(), crate::netdev::Fail>> {
        // IFLA_NET_NS_PID (19) is a process, IFLA_NET_NS_FD (28) a namespace file
        let resolve = |attr: u16, value: u32| self.net_of(attr == 19, value);
        // What the namespaces being watched look like before and after a change request
        // (RTM_NEW, DEL and SETLINK, NEW and DELADDR, NEW and DELROUTE): it can change another
        // namespace too (moving a link, a veth's peer)
        let ty = req
            .get(4..6)
            .map_or(0, |t| u16::from_le_bytes([t[0], t[1]]));
        let watched = match ty {
            16 | 17 | 19 | 20 | 21 | 24 | 25 => &self.watched[..],
            _ => &[],
        };
        let before = self.snapshots(watched);
        let done = crate::netdev::request(&self.ns(), &self.cred, req, &resolve);
        let notes = self.changed_since(watched, before);
        self.notes.borrow_mut().extend(notes);
        done
    }

    fn peer_net(&self, attr: u16, value: u32) -> Result<Option<i32>, i32> {
        let peer = self
            .net_of(attr == crate::net::NETNSA_PID, value)
            .map_err(|e| e.0)?;
        Ok(crate::netdev::nsid(&self.ns(), &peer))
    }

    fn netfilter(&self, data: &[u8], peer: &crate::nft::Peer<'_>) -> Vec<Vec<u8>> {
        let comm = self.comm.split(|&c| c == 0).next().unwrap_or_default();
        let peer = crate::nft::Peer {
            proc_pid: self.proc_pid,
            comm,
            ..*peer
        };
        crate::nft::handle(&self.ns(), &self.cred, data, &peer)
    }
}

// ---- ioctls ----

/// Socket ioctls: FIONREAD, and interface information (`SIOCGIF*`).
pub(super) fn ioctl(cx: &mut Cx<'_>, sock: &SocketRef, request: u32, arg: u64) -> SysResult {
    // One that changes an interface, address or route: notify rtnetlink listeners of what it did
    let watched = if CHANGING_IOCTLS.contains(&request) {
        rtnl_watched(cx.kr(), None)
    } else {
        Vec::new()
    };
    if watched.is_empty() {
        return socket_ioctl(cx, sock, request, arg);
    }
    let before = KernelHost::of(cx, None).snapshots(&watched);
    let r = socket_ioctl(cx, sock, request, arg);
    let notes = KernelHost::of(cx, None).changed_since(&watched, before);
    rtnl_notify(cx.k(), notes);
    r
}

/// Socket ioctls, as [`ioctl`] without the notifications.
fn socket_ioctl(cx: &mut Cx<'_>, sock: &SocketRef, request: u32, arg: u64) -> SysResult {
    const FIONREAD: u32 = 0x541b;
    const SIOCATMARK: u32 = 0x8905;
    // Routes on an internet socket, in its namespace
    if matches!(request, SIOCADDRT | SIOCDELRT)
        && let Some((family, ns)) = match &*sock.borrow() {
            Socket::Inet(s) => Some((s.family, net_of(cx, s))),
            _ => None,
        }
    {
        return route_ioctl(cx, &ns, family, request, arg).map(|()| 0);
    }
    // A socket in a new network namespace: interface ioctls see only its lo (crate::netns)
    let net = match &*sock.borrow() {
        Socket::Inet(s) => s.net.clone(),
        Socket::Netlink(s) => s.net.clone(),
        Socket::Unix(u) => u.net.clone(),
    };
    if let Some(ns) = net
        && (0x8910..0x8960).contains(&request)
    {
        return netns_ioctl(cx, &ns, request, arg);
    }
    // Virtual devices in the boot namespace (crate::netdev): those recognized by name or index are
    // not handed to the host
    if (0x8910..0x8960).contains(&request)
        && request != 0x8912
        && let Ok(mut ifr) = get::<40>(cx, arg)
    {
        let init = cx.kr().init_ns.net.clone();
        if let Some(r) = crate::netdev::ioctl(&init, &cx.cred(), request, &mut ifr) {
            r?;
            return put(cx, arg, ifr).map(|()| 0);
        }
    }
    let host_fd = match &*sock.borrow() {
        // DNS stub answers are in our own queue and are read before what the host socket
        // received (recvmsg does the same): a datagram's FIONREAD is the length of the next
        // datagram. glibc's resolver sizes the buffer for the AAAA answer by it
        Socket::Inet(s) if request == FIONREAD && !s.answers.is_empty() => {
            let n = s.answers.front().map_or(0, Vec::len) as i32;
            return put(cx, arg, n.to_le_bytes()).map(|()| 0);
        }
        Socket::Inet(s) => Some(s.fd),
        Socket::Unix(u) if request == FIONREAD => {
            let n = u.unread().ok_or(Errno::EINVAL)? as i32;
            return put(cx, arg, n.to_le_bytes()).map(|()| 0);
        }
        Socket::Netlink(_) | Socket::Unix(_) => None,
    };
    match (request, host_fd) {
        // Ask the host first (EINVAL for UDP), then write back to the guest (EFAULT for a bad
        // address)
        (SIOCATMARK, Some(fd)) => {
            let mut b = [0u8; 4];
            tuxport_hal::ioctl(fd, request as u64, &mut b).map_err(Errno)?;
            put(cx, arg, b).map(|()| 0)
        }
        (FIONREAD, Some(fd)) => {
            let mut b = [0u8; 4];
            tuxport_hal::ioctl(fd, request as u64, &mut b).map_err(Errno)?;
            put(cx, arg, b).map(|()| 0)
        }
        (FIONREAD, None) => Err(Errno::ENOTTY),
        (_, Some(fd)) => interface_ioctl(cx, fd, request, arg),
        // As with Linux's dev_ioctl, interface ioctls do not care about the socket's address
        // family (musl's if_nametoindex uses an AF_UNIX socket): borrow a temporary host socket
        (_, None) => {
            let fd = tuxport_hal::net::socket(AF_INET, SOCK_DGRAM, 0).map_err(Errno)?;
            let r = interface_ioctl(cx, fd, request, arg);
            tuxport_hal::net::close(fd);
            r
        }
    }
}

/// `SIOCADDRT` and `SIOCDELRT` on an internet socket (Linux's `ip_rt_ioctl` with a `struct
/// rtentry`, `ipv6_route_ioctl` with a `struct in6_rtmsg` on an IPv6 socket): the structure is read
/// whole (EFAULT), `CAP_NET_ADMIN` over the namespace's owner is needed (EPERM), and it becomes the
/// `RTM_NEWROUTE` or `RTM_DELROUTE` request that `rtentry_to_fib_config` or `rtmsg_to_fib6_config`
/// describes, carried out by [`crate::netdev::request`] as for rtnetlink (so only routes through
/// virtual devices can change).
fn route_ioctl(
    cx: &mut Cx<'_>,
    ns: &Rc<crate::nsproxy::NetNs>,
    family: i32,
    request: u32,
    arg: u64,
) -> Result<(), Errno> {
    let add = request == SIOCADDRT;
    let req = if family == AF_INET6 {
        let m: [u8; 80] = get(cx, arg)?;
        if !cx.cred().ns_capable(&ns.owner, crate::cred::cap::NET_ADMIN) {
            return Err(Errno::EPERM);
        }
        in6_rtmsg_request(add, &m)?
    } else {
        let rt: [u8; 120] = get(cx, arg)?;
        if !cx.cred().ns_capable(&ns.owner, crate::cred::cap::NET_ADMIN) {
            return Err(Errno::EPERM);
        }
        rtentry_request(cx, ns, add, &rt)?
    };
    let resolve = |_: u16, _: u32| Err(Errno::EINVAL);
    match crate::netdev::request(ns, &cx.cred(), &req, &resolve) {
        Some(r) => r.map_err(|f| Errno(f.errno)),
        None => unreachable!("a route request"),
    }
}

const SIOCADDRT: u32 = 0x890b;
const SIOCDELRT: u32 = 0x890c;
const RTF_GATEWAY: u16 = 0x2;
const RTF_HOST: u16 = 0x4;
const RTF_REJECT: u16 = 0x200;
const RTN_UNICAST: u8 = 1;
const RTN_UNREACHABLE: u8 = 7;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RT_SCOPE_HOST: u8 = 254;
const RT_SCOPE_NOWHERE: u8 = 255;
const RTPROT_BOOT: u8 = 3;
const RT_TABLE_MAIN: u8 = 254;
const NLM_F_CREATE: u16 = 0x400;
const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PRIORITY: u16 = 6;
const RTA_PREFSRC: u16 = 7;

/// The rtnetlink request for an IPv4 `struct rtentry` (Linux's `rtentry_to_fib_config`): the
/// destination must be `AF_INET` (EAFNOSUPPORT); without `RTF_HOST` the mask must be contiguous,
/// clear the destination's host bits (EINVAL) and be `AF_INET` unless it and its family are 0
/// (EAFNOSUPPORT); a metric is one more than the route's priority; `RTF_REJECT` asks for an
/// unreachable route; `rt_dev` names the interface (ENODEV), and `name:label` also takes the source
/// of that address label; a gateway on a unicast address with `RTF_GATEWAY` makes the route
/// universe-scoped. An added route is `RTPROT_BOOT`, link-scoped otherwise, and `RTF_GATEWAY`
/// without a gateway is EINVAL. `RTF_MTU`, `RTF_WINDOW` and `RTF_IRTT` are not carried.
fn rtentry_request(
    cx: &Cx<'_>,
    ns: &crate::nsproxy::NetNs,
    add: bool,
    rt: &[u8; 120],
) -> Result<Vec<u8>, Errno> {
    let family = |off: usize| u16::from_le_bytes([rt[off], rt[off + 1]]);
    let addr = |off: usize| -> [u8; 4] { rt[off + 4..off + 8].try_into().unwrap() };
    let (dst_off, gw_off, mask_off) = (8, 24, 40);
    let flags = u16::from_le_bytes([rt[56], rt[57]]);
    let metric = i16::from_le_bytes([rt[80], rt[81]]);
    let dev = u64::from_le_bytes(rt[88..96].try_into().unwrap());
    if i32::from(family(dst_off)) != AF_INET {
        return Err(Errno::EAFNOSUPPORT);
    }
    let dst = addr(dst_off);
    let mut prefix = 32;
    if flags & RTF_HOST == 0 {
        let mask = u32::from_be_bytes(addr(mask_off));
        if i32::from(family(mask_off)) != AF_INET && (mask != 0 || family(mask_off) != 0) {
            return Err(Errno::EAFNOSUPPORT);
        }
        // bad_mask: host bits set in the address, or a mask that is not contiguous
        if u32::from_be_bytes(dst) & !mask != 0 || (!mask & (!mask).wrapping_add(1)) != 0 {
            return Err(Errno::EINVAL);
        }
        prefix = mask.count_ones() as u8;
    }
    let mut attrs: Vec<(u16, Vec<u8>)> = vec![(RTA_DST, dst.to_vec())];
    if metric != 0 {
        let priority = (i32::from(metric) - 1) as u32;
        attrs.push((RTA_PRIORITY, priority.to_le_bytes().to_vec()));
    }
    let (proto, nlflags) = if add {
        (RTPROT_BOOT, NLM_F_CREATE)
    } else {
        (0, 0)
    };
    let rtm = |scope: u8, ty: u8| [AF_INET as u8, prefix, 0, 0, RT_TABLE_MAIN, proto, scope, ty];
    if flags & RTF_REJECT != 0 {
        return Ok(route_message(
            add,
            nlflags,
            rtm(RT_SCOPE_HOST, RTN_UNREACHABLE),
            &attrs,
        ));
    }
    let mut scope = RT_SCOPE_NOWHERE;
    if dev != 0 {
        // The first IFNAMSIZ - 1 bytes, ending at a NUL, then at a colon for the device
        let mut name = [0u8; 15];
        cx.read_user(UserSlice::new(dev, name.len()), &mut name)?;
        let name = name.split(|&c| c == 0).next().unwrap_or_default();
        let base = name.split(|&c| c == b':').next().unwrap_or_default();
        let index = crate::netdev::interfaces(ns)
            .iter()
            .find(|i| i.name.as_bytes() == base)
            .ok_or(Errno::ENODEV)?
            .index;
        attrs.push((RTA_OIF, index.to_le_bytes().to_vec()));
        if base.len() < name.len() {
            let local = crate::netdev::views(ns)
                .into_iter()
                .find(|v| v.index == index)
                .and_then(|v| v.v4.into_iter().find(|a| a.label == name))
                .ok_or(Errno::ENODEV)?
                .local;
            attrs.push((RTA_PREFSRC, local.to_vec()));
        }
    }
    let gw = addr(gw_off);
    let has_gw = i32::from(family(gw_off)) == AF_INET && gw != [0; 4];
    if has_gw {
        attrs.push((RTA_GATEWAY, gw.to_vec()));
        if flags & RTF_GATEWAY != 0 && unicast_v4(ns, gw) {
            scope = RT_SCOPE_UNIVERSE;
        }
    }
    if add {
        if flags & RTF_GATEWAY != 0 && !has_gw {
            return Err(Errno::EINVAL);
        }
        if scope == RT_SCOPE_NOWHERE {
            scope = RT_SCOPE_LINK;
        }
    }
    Ok(route_message(add, nlflags, rtm(scope, RTN_UNICAST), &attrs))
}

/// Whether IPv4 address `a` is unicast in namespace `ns` (`inet_addr_type_table`): not on the
/// loopback network, not a local or broadcast address of an interface, not multicast or limited
/// broadcast.
fn unicast_v4(ns: &crate::nsproxy::NetNs, a: [u8; 4]) -> bool {
    let ip = u32::from_be_bytes(a);
    if a[0] == 127 || a[0] >= 224 || ip == 0 {
        return false;
    }
    !crate::netdev::interfaces(ns).iter().any(|i| {
        i.addrs.iter().any(|(f, addr, prefix)| {
            let Ok(local) = <[u8; 4]>::try_from(addr.as_slice()) else {
                return false;
            };
            let mask = u32::MAX.checked_shl(32 - u32::from(*prefix)).unwrap_or(0);
            *f == AF_INET
                && (local == a || (*prefix < 31 && ip == u32::from_be_bytes(local) | !mask))
        })
    })
}

/// The rtnetlink request for an IPv6 `struct in6_rtmsg` (Linux's `rtmsg_to_fib6_config`): the
/// destination and its length, the gateway when `RTF_GATEWAY` is set, the interface index and the
/// metric (an added route without one takes 1024 there); `RTF_REJECT` asks for an unreachable
/// route, and a source prefix (`rtmsg_src_len`) is EOPNOTSUPP, as source routes are not kept.
fn in6_rtmsg_request(add: bool, m: &[u8; 80]) -> Result<Vec<u8>, Errno> {
    let u32_at = |off: usize| u32::from_le_bytes(m[off..off + 4].try_into().unwrap());
    let dst_len = u16::from_le_bytes([m[52], m[53]]);
    let src_len = u16::from_le_bytes([m[54], m[55]]);
    let (metric, flags, ifindex) = (u32_at(56), u32_at(72), u32_at(76));
    if src_len != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let prefix = u8::try_from(dst_len).map_err(|_| Errno::EINVAL)?;
    let mut attrs: Vec<(u16, Vec<u8>)> = vec![(RTA_DST, m[..16].to_vec())];
    if flags & u32::from(RTF_GATEWAY) != 0 {
        attrs.push((RTA_GATEWAY, m[32..48].to_vec()));
    }
    if ifindex != 0 {
        attrs.push((RTA_OIF, ifindex.to_le_bytes().to_vec()));
    }
    if metric != 0 {
        attrs.push((RTA_PRIORITY, metric.to_le_bytes().to_vec()));
    }
    let ty = if flags & u32::from(RTF_REJECT) != 0 {
        RTN_UNREACHABLE
    } else {
        RTN_UNICAST
    };
    let (proto, nlflags) = if add {
        (RTPROT_BOOT, NLM_F_CREATE)
    } else {
        (0, 0)
    };
    let rtm = [
        AF_INET6 as u8,
        prefix,
        0,
        0,
        RT_TABLE_MAIN,
        proto,
        RT_SCOPE_UNIVERSE,
        ty,
    ];
    Ok(route_message(add, nlflags, rtm, &attrs))
}

/// A whole `RTM_NEWROUTE` (`add`) or `RTM_DELROUTE` request: `rtm` is `struct rtmsg` without its
/// flags (family, destination length, source length, tos, table, protocol, scope, type).
fn route_message(add: bool, nlflags: u16, rtm: [u8; 8], attrs: &[(u16, Vec<u8>)]) -> Vec<u8> {
    const RTM_NEWROUTE: u16 = 24;
    const RTM_DELROUTE: u16 = 25;
    const NLM_F_REQUEST: u16 = 1;
    let mut body = rtm.to_vec();
    body.extend_from_slice(&[0; 4]);
    for (ty, data) in attrs {
        crate::net::attr(&mut body, *ty, data);
    }
    let ty = if add { RTM_NEWROUTE } else { RTM_DELROUTE };
    crate::net::message(ty, NLM_F_REQUEST | nlflags, 0, 0, &body)
}

/// Interface ioctls in a new network namespace: virtual devices ([`crate::netdev`]) and lo.
fn netns_ioctl(
    cx: &mut Cx<'_>,
    ns: &Rc<crate::nsproxy::NetNs>,
    request: u32,
    arg: u64,
) -> SysResult {
    const SIOCGIFCONF: u32 = 0x8912;
    if request == SIOCGIFCONF {
        let conf: [u8; 16] = get(cx, arg)?;
        let len = i32::from_le_bytes(conf[..4].try_into().unwrap()).max(0) as usize;
        let buf = u64::from_le_bytes(conf[8..].try_into().unwrap());
        let mut all = crate::netns::ifconf(ns);
        all.extend(crate::netdev::ifconf(ns));
        // With an empty buffer only report how large it must be; otherwise write as many whole
        // entries as fit
        let n = if buf == 0 {
            all.len()
        } else {
            let n = (len / 40 * 40).min(all.len());
            cx.mm().write(UserSlice::new(buf, n), &all[..n])?;
            n
        };
        return put(cx, arg, (n as i32).to_le_bytes()).map(|()| 0);
    }
    let mut ifr: [u8; 40] = get(cx, arg)?;
    if let Some(r) = crate::netdev::ioctl(ns, &cx.cred(), request, &mut ifr) {
        r?;
        return put(cx, arg, ifr).map(|()| 0);
    }
    match crate::netns::interface_ioctl(ns, &cx.cred(), request, &mut ifr) {
        Some(r) => {
            r?;
            put(cx, arg, ifr).map(|()| 0)
        }
        // Other changes (address, MTU and so on) are not supported
        None if matches!(
            request,
            0x8916 | 0x8918 | 0x891a | 0x891c | 0x8923 | 0x8924 | 0x8943
        ) =>
        {
            Err(Errno::EPERM)
        }
        None => {
            crate::stub::ioctl(cx, request, "socket");
            Err(Errno::ENOTTY)
        }
    }
}

/// Interface information ioctls, passed through to host socket `host_fd`.
fn interface_ioctl(cx: &mut Cx<'_>, host_fd: i32, request: u32, arg: u64) -> SysResult {
    const SIOCGIFCONF: u32 = 0x8912;
    const SIOCGIFFLAGS: u32 = 0x8913;
    const SIOCGIFHWADDR: u32 = 0x8927;
    const IFF_LOOPBACK: u16 = 0x8;
    const ARPHRD_ETHER: u16 = 1;
    const ARPHRD_LOOPBACK: u16 = 772;
    // struct ifreq (40 bytes) in, ifreq out: SIOCGIFNAME, FLAGS, ADDR, DSTADDR, BRDADDR,
    // NETMASK, MTU, HWADDR, INDEX, TXQLEN
    const IFREQ_GETTERS: [u32; 10] = [
        0x8910, 0x8913, 0x8915, 0x8917, 0x8919, 0x891b, 0x8921, 0x8927, 0x8933, 0x8942,
    ];
    match request {
        r if IFREQ_GETTERS.contains(&r) => {
            let mut b: [u8; 40] = get(cx, arg)?;
            tuxport_hal::ioctl(host_fd, r as u64, &mut b).map_err(Errno)?;
            if r == SIOCGIFHWADDR && b[16..18] == [0, 0] {
                // Android does not let apps read hardware addresses, and the address family is 0
                // too: fill in according to whether the interface is loopback
                let mut flags = b;
                tuxport_hal::ioctl(host_fd, SIOCGIFFLAGS as u64, &mut flags).map_err(Errno)?;
                let loopback = u16::from_le_bytes([flags[16], flags[17]]) & IFF_LOOPBACK != 0;
                let ty = if loopback {
                    ARPHRD_LOOPBACK
                } else {
                    ARPHRD_ETHER
                };
                b[16..18].copy_from_slice(&ty.to_le_bytes());
            }
            put(cx, arg, b).map(|()| 0)
        }
        // Changing interfaces (SIOCSIFFLAGS, ADDR, MTU and so on): the app cannot change the
        // host's interfaces, so it is EPERM, as without CAP_NET_ADMIN, after reading the argument
        // first (EFAULT for a bad address)
        0x8914 | 0x8916 | 0x8918 | 0x891a | 0x891c | 0x8922 | 0x8923 | 0x8924 | 0x8943 => {
            let _: [u8; 40] = get(cx, arg)?;
            Err(Errno::EPERM)
        }
        SIOCGIFCONF => {
            // struct ifconf: the length and the pointer to the guest buffer, replaced with a host
            // buffer
            let conf: [u8; 16] = get(cx, arg)?;
            let len = i32::from_le_bytes(conf[..4].try_into().unwrap()).clamp(0, 1 << 16) as usize;
            let guest_buf = u64::from_le_bytes(conf[8..].try_into().unwrap());
            let mut host = vec![0u8; len];
            let mut req = [0u8; 16];
            req[..4].copy_from_slice(&(len as i32).to_le_bytes());
            let ptr = if guest_buf == 0 {
                0
            } else {
                host.as_mut_ptr() as u64
            };
            req[8..].copy_from_slice(&ptr.to_le_bytes());
            tuxport_hal::ioctl(host_fd, SIOCGIFCONF as u64, &mut req).map_err(Errno)?;
            let got = i32::from_le_bytes(req[..4].try_into().unwrap()).max(0) as usize;
            // Virtual devices in the boot namespace come after the host's
            let extra = crate::netdev::ifconf(&cx.kr().init_ns.net);
            if guest_buf == 0 {
                return put(cx, arg, ((got + extra.len()) as i32).to_le_bytes()).map(|()| 0);
            }
            let got = got.min(len);
            cx.mm()
                .write(UserSlice::new(guest_buf, got), &host[..got])?;
            let n = ((len - got) / 40 * 40).min(extra.len());
            cx.mm()
                .write(UserSlice::new(guest_buf + got as u64, n), &extra[..n])?;
            put(cx, arg, ((got + n) as i32).to_le_bytes()).map(|()| 0)
        }
        _ => {
            crate::stub::ioctl(cx, request, "socket");
            Err(Errno::ENOTTY)
        }
    }
}

// ---- Readiness and wakeups ----

/// The readiness for ppoll and epoll: (events already known, the fd to ask the host about).
pub(crate) fn readiness(sock: &Socket) -> (i16, Option<i32>) {
    const POLLRDNORM: i16 = 0x040;
    const POLLWRNORM: i16 = 0x100;
    match sock {
        // What a raw ICMP socket receives is in the aggregation point's queue (crate::rawicmp),
        // taken off by the timer thread
        Socket::Inet(s @ InetSocket { icmp: Some(r), .. }) => {
            let key = wake::of_value(sock);
            let ready = crate::rawicmp::queued(&r.tap, key).0;
            let read = if ready { POLLIN | POLLRDNORM } else { 0 };
            let errors = s.icmp_err.get() != 0 || crate::rawicmp::has_errors(&r.tap, key);
            let err = if errors { POLLERR } else { 0 };
            (read | err | POLLOUT | POLLWRNORM, None)
        }
        // A UDP socket the aggregation point watches: the ICMP errors the host recorded for it
        // make the host poll report POLLERR, so take them off first; the one to report to the
        // guest is kept separately
        Socket::Inet(s) if s.udp_tap.is_some() => {
            if let Some(t) = &s.udp_tap {
                let _ = crate::rawicmp::dispatch(t, Some(s.fd));
            }
            let err = if s.icmp_err.get() != 0 { POLLERR } else { 0 };
            let stub = if s.answers.is_empty() {
                0
            } else {
                POLLIN | POLLOUT
            };
            (err | stub, Some(s.fd))
        }
        Socket::Inet(s) if !s.answers.is_empty() => (POLLIN | POLLOUT, None),
        Socket::Inet(s) => (0, Some(s.fd)),
        Socket::Netlink(s) => {
            let err = if s.overrun.get() { POLLERR } else { 0 };
            (
                POLLOUT | err | if s.queue.is_empty() { 0 } else { POLLIN },
                None,
            )
        }
        Socket::Unix(u) => (u.readiness(), None),
    }
}

/// The wake objects ppoll and epoll register when waiting on a socket (`poll::wake_objects`): the
/// channels for AF_UNIX; the socket itself for host sockets and netlink. Data on host sockets is
/// waited for by the host poll; closing the socket (the host poll of threads waiting on it must
/// let go), DNS stub answers and netlink answers request a wake of it.
pub(crate) fn wake_objects(sock: &RefCell<Socket>, out: &mut Vec<wake::Object>) {
    match &*sock.borrow() {
        Socket::Unix(u) => u.wake_objects(out),
        Socket::Inet(_) | Socket::Netlink(_) => out.push(wake::of(sock)),
    }
}

/// Queues the DNS stub answers that host threads have resolved on the sockets the queries came
/// from, and wakes the sockets' waiters (the timer thread, holding the BKL). Answers for sockets
/// closed meanwhile are dropped.
pub(crate) fn deliver_dns(k: &mut crate::kernel::Kernel, answers: &crate::kernel::DnsAnswers) {
    let done = std::mem::take(
        &mut *answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for (id, answer) in done {
        let Some(sock) = k.dns_queries.remove(&id).and_then(|w| w.upgrade()) else {
            continue;
        };
        if let Socket::Inet(s) = &mut *sock.borrow_mut() {
            // A full receive queue drops the answer (__udp_enqueue_schedule_skb: what is queued
            // already exceeds SO_RCVBUF), counting answers by their length
            const SO_RCVBUF: i32 = 8;
            let rcvbuf = tuxport_hal::net::getsockopt(s.fd, SOL_SOCKET, SO_RCVBUF, 4)
                .ok()
                .and_then(|v| Some(i32::from_le_bytes(v.get(..4)?.try_into().ok()?)))
                .map_or(212_992, |v| v.max(0) as usize);
            if s.answers.iter().map(Vec::len).sum::<usize>() > rcvbuf {
                crate::debug::log(format_args!("DNS stub: receive queue full, answer dropped"));
                continue;
            }
            s.answers.push_back(answer);
            s.answered += 1;
        }
        request_wake_object(wake::of(&*sock));
    }
}

/// Most DNS stub queries resolving at once (host threads), across the kernel.
const DNS_MAX_INFLIGHT: usize = 128;

/// Whether a socket's sends and connects to `127.0.0.53:53` go to the DNS stub: a UDP socket in
/// the initial network namespace (not UDP-Lite, a ping socket or a raw ICMP socket, which are
/// datagram sockets on the host too).
fn stub_capable(s: &InetSocket) -> bool {
    s.net.is_none() && s.ty == SOCK_DGRAM && s.proto == IPPROTO_UDP && !s.raw && s.icmp.is_none()
}

/// How many times in-process data has arrived: [read direction, write direction]
/// (`syscall::poll`'s data generations). Data on host sockets does not go through the kernel
/// layer and is recorded by [`crate::fd::HostEdges`].
pub(crate) fn arrivals(sock: &Socket) -> [u64; 2] {
    match sock {
        Socket::Inet(InetSocket { icmp: Some(r), .. }) => {
            [crate::rawicmp::queued(&r.tap, wake::of_value(sock)).1, 0]
        }
        Socket::Inet(s) => [s.answered, 0],
        Socket::Netlink(s) => [s.arrivals, 0],
        Socket::Unix(u) => u.arrivals(),
    }
}

// ---- Routing: nat, namespaces and filter chains ----

/// The result of [`route`].
struct Routed {
    /// The destination on the host ([`crate::netns::to_host`]), with the source address chosen
    /// on `me`'s side.
    m: crate::netns::Mapped,
    /// The destination after OUTPUT nat (as seen in `me`).
    dst: Vec<u8>,
    /// How the final destination looks in its namespace (`m.target`) (after PREROUTING nat).
    there: Vec<u8>,
    /// When nat rewrote the destination, the actual destination as seen in `me`: datagrams coming
    /// back from it are reported as coming from the address the guest gave
    /// ([`crate::net::Track::nat_back`]).
    actual: Option<Vec<u8>>,
    /// The result of the filter chains passed, and whether it was on OUTPUT
    /// ([`crate::netns::path_filter`]).
    verdict: (crate::nft::Outcome, bool),
    /// The namespaces (by number) where nat rewrote the destination, and the original destination
    /// seen there: in `me` it is the one the guest gave, in other namespaces entered it is the one
    /// before their PREROUTING (`SO_ORIGINAL_DST`, [`crate::origdst`]).
    origs: Vec<(u64, Vec<u8>)>,
}

/// The most DNATs ([`crate::net::Track::nat_back`]) and peers received from
/// ([`crate::net::Track::seen`]) an unconnected datagram socket records, each.
const NAT_BACK_MAX: usize = 16;

/// The destination address `addr` (the guest's) of connect or sendto: it first goes through `me`'s
/// OUTPUT nat and is translated to the host's; when it enters another namespace
/// ([`crate::netns::entering`]) it also goes through that namespace's PREROUTING nat and is
/// translated to the host's by that namespace (containers connecting to published ports through
/// the gateway or the outer address).
fn route(
    w: &crate::netns::World,
    s: &InetSocket,
    me: &Rc<crate::nsproxy::NetNs>,
    addr: &[u8],
    cred: &crate::cred::Cred,
) -> Result<Routed, Errno> {
    let nat = nat_dest(s, me, addr, cred)?;
    let mut origs = Vec::new();
    if nat.is_some() {
        origs.push((me.id, addr.to_vec()));
    }
    let dst = nat.clone().unwrap_or_else(|| addr.to_vec());
    let mut m = crate::netns::to_host(w, me, &dst, crate::netns::Use::Reach, s.icmp6())?;
    let mut actual = nat;
    let mut there = dst.clone();
    if let Some(proto) = nat_proto(s)
        && let Some(t) = crate::netns::entering(w, me, &m, &dst)
        && let Some(d2) = crate::netns::prerouting_nat(me, &t, proto, local_port(s), &dst)?
    {
        let m2 = crate::netns::to_host(w, &t, &d2, crate::netns::Use::Reach, s.icmp6())?;
        origs.push((t.id, dst.clone()));
        there = d2;
        actual = Some(crate::netns::from_host(
            w,
            me,
            &m2.addr,
            crate::netns::Role::Peer,
            s.icmp6(),
        ));
        // Going through the host network originally, no source address was chosen: when DNAT
        // leads into a namespace, a new namespace binds its loopback address so the other side
        // recognizes the source; when DNAT leads to another address outside it still does not
        // bind, since bound it could not get out
        let source = m.source.take().or_else(|| {
            m2.target.as_ref()?;
            me.host_net
                .and_then(|_| crate::netns::implicit_bind(me, s.family, s.icmp6()))
        });
        m = crate::netns::Mapped {
            addr: m2.addr,
            target: m2.target,
            source,
            vaddr: m.vaddr,
        };
    }
    // The filter chains passed (crate::netns::path_filter): UDP sending back to peers it received
    // from is ESTABLISHED
    let verdict = match nat_proto(s) {
        Some(proto) => {
            let established = proto == IPPROTO_UDP as u8
                && s.track
                    .as_ref()
                    .is_some_and(|k| k.seen.iter().any(|p| same_endpoint(p, addr)));
            let flow = crate::netns::Flow {
                proto,
                vaddr: s.vaddr,
                sport: local_port(s),
                dst: &dst,
                there: &there,
                uid: cred.euid,
                gid: cred.egid,
                established,
                mark: s.mark(),
            };
            crate::netns::path_filter(w, me, m.target.as_ref(), &flow)
        }
        None => (crate::nft::Outcome::Accept, false),
    };
    Ok(Routed {
        m,
        dst,
        there,
        actual,
        verdict,
        origs,
    })
}

/// The destination address rewritten by nftables' OUTPUT nat ([`crate::netns::output_nat`]);
/// protocols other than TCP and UDP are ignored.
fn nat_dest(
    s: &InetSocket,
    me: &crate::nsproxy::NetNs,
    dst: &[u8],
    cred: &crate::cred::Cred,
) -> Result<Option<Vec<u8>>, Errno> {
    let Some(proto) = nat_proto(s) else {
        return Ok(None);
    };
    crate::netns::output_nat(
        me,
        proto,
        s.vaddr,
        || local_port(s),
        dst,
        cred.euid,
        cred.egid,
        s.mark(),
    )
}

/// The protocol of a socket that goes through nftables nat: TCP (MPTCP included), UDP.
fn nat_proto(s: &InetSocket) -> Option<u8> {
    match (s.ty, s.proto) {
        (SOCK_STREAM, 0 | IPPROTO_TCP | IPPROTO_MPTCP) => Some(IPPROTO_TCP as u8),
        (SOCK_DGRAM, 0 | IPPROTO_UDP) if s.icmp.is_none() => Some(IPPROTO_UDP as u8),
        _ => None,
    }
}

/// The host socket's local port (0 while unbound).
fn local_port(s: &InetSocket) -> u16 {
    tuxport_hal::net::sockname(s.fd, false)
        .ok()
        .and_then(|a| port_of(&a))
        .unwrap_or(0)
}

/// Whether two IPv4 or IPv6 socket addresses have the same protocol family, port and address
/// (for IPv6 also the flow info; the scope id is not compared).
fn same_endpoint(a: &[u8], b: &[u8]) -> bool {
    let n = match a.get(..2).map(|f| u16::from_le_bytes([f[0], f[1]]) as i32) {
        Some(AF_INET) => 8,
        Some(AF_INET6) => 24,
        _ => return false,
    };
    a.get(..n).is_some() && a.get(..n) == b.get(..n)
}

// ---- Forwarding ----

/// Decides by the current rules whether `s` is forwarded and where to ([`crate::forward`]).
fn forward_socket(k: &crate::kernel::Kernel, s: &mut InetSocket) {
    let want = forward_target(k, s);
    if want.as_ref() == s.fwd.as_ref().map(|h| &h.target) {
        return;
    }
    // Hand over the new one before dropping the old: no gap in between
    let new = want.and_then(|t| k.forward.add(s.fd, t).ok()).map(Box::new);
    s.fwd = new;
}

/// When a connection from outside the host reaches `s` in the boot namespace (a TCP listen, or a
/// UDP socket with a bound port), where PREROUTING nat sends it: the destination on the host (a
/// container's loopback address, with low ports switched to the mapped ones), and the source
/// address bound when connecting there (the boot namespace's address facing it).
fn forward_target(k: &crate::kernel::Kernel, s: &InetSocket) -> Option<crate::forward::Target> {
    if s.net.is_some() || s.raw || s.icmp.is_some() {
        return None;
    }
    let udp = match (s.ty, map_proto(s)) {
        (SOCK_STREAM, IPPROTO_TCP) => false,
        (SOCK_DGRAM, IPPROTO_UDP) => true,
        _ => return None,
    };
    let init = &k.init_ns.net;
    if !init.nft.borrow().has_nat(crate::nft::HOOK_PRE_ROUTING) {
        return None;
    }
    let mut local = tuxport_hal::net::sockname(s.fd, false).ok()?;
    let host_port = port_of(&local).filter(|&p| p != 0)?;
    if !udp && !s.listening {
        return None;
    }
    // The rules hold the guest's port
    if let Some((g, h)) = s.local_map
        && h == host_port
    {
        set_port(&mut local, g);
    }
    let proto = if udp { IPPROTO_UDP } else { IPPROTO_TCP };
    let dst = crate::netns::external_nat(init, proto as u8, &local)?;
    let w = k.world();
    let m = crate::netns::to_host(&w, init, &dst, crate::netns::Use::Reach, false).ok()?;
    // Not forwarded when dropped or rejected by filter chains (FORWARD and so on)
    if !crate::netns::external_filter(init, m.target.as_deref(), proto as u8, &dst) {
        return None;
    }
    let mut addr = m.addr;
    let key = m.target.as_ref().map_or(0, key_of);
    let map = k.portmap.borrow();
    if let Some(p) = port_of(&addr)
        && let Some(h) = map.host_port(key, proto, p, ipv6_proper(&dst))
    {
        set_port(&mut addr, h);
    }
    Some(crate::forward::Target {
        udp,
        addr,
        source: m.source,
    })
}

/// After the nftables ruleset of the boot namespace changes, looks at all sockets again.
fn forward_all(k: &crate::kernel::Kernel) {
    for (sock, _) in crate::pseudo::sockets(k) {
        if let Ok(mut b) = sock.try_borrow_mut()
            && let Socket::Inet(s) = &mut *b
            && s.net.is_none()
        {
            forward_socket(k, s);
        }
    }
}

// ---- Low ports ----

/// Connecting or sending to host address `addr`: a mapped low port on the local machine becomes
/// the host port ([`crate::portmap`]), by the registration of the network namespace the
/// destination is in (`key`, [`key_of`]); `v6` says whether the destination as seen in that
/// namespace is an IPv6 address, which picks an IPv6-only binding first
/// ([`crate::portmap::PortMap::host_port`]). Returns the rewritten address and (guest port, host
/// port). Not rewritten when no program in the guest binds this port: if another app on the
/// phone happens to hold the host port, connecting there would be wrong, and it should fail to
/// connect as before.
fn map_dest(
    map: &crate::cell::RefCell<crate::portmap::PortMap>,
    s: &InetSocket,
    mut addr: Vec<u8>,
    key: usize,
    v6: bool,
) -> (Vec<u8>, Option<(u16, u16)>) {
    let Some(port) = port_of(&addr) else {
        return (addr, None);
    };
    let host = {
        if !mappable(s) {
            return (addr, None);
        }
        // Only low ports and ports bound explicitly in new namespaces are registered; unregistered
        // ones are not rewritten
        map.borrow().host_port(key, map_proto(s), port, v6)
    };
    match host {
        Some(h) if is_local(&addr) => {
            set_port(&mut addr, h);
            (addr, Some((port, h)))
        }
        _ => (addr, None),
    }
}

/// In a local (`local`) or peer address given by the host, a mapped host port is translated back
/// to the guest's: the local end by the socket's bound mapping; the peer first by what was
/// recorded at connect, then by whether it was mapped on the local machine (the source of UDP, or
/// an accepted peer bound with a low port), by the network namespace the peer address is in
/// ([`crate::netns::owner_of_host`]).
pub(crate) fn unmap_addr(
    map: &crate::cell::RefCell<crate::portmap::PortMap>,
    w: &crate::netns::World,
    s: &InetSocket,
    mut addr: Vec<u8>,
    local: bool,
) -> Vec<u8> {
    let Some(port) = port_of(&addr) else {
        return addr;
    };
    let known = if local { s.local_map } else { s.peer_map };
    // The source of a connection forwarded in from outside is not a mapped port (crate::forward)
    let outside = ip_of(&addr).is_some_and(|ip| match ip {
        std::net::IpAddr::V4(v) => v.octets()[..2] == [127, crate::forward::OUTSIDE_BLOCK],
        std::net::IpAddr::V6(_) => false,
    });
    let guest = match known {
        Some((g, h)) if h == port => Some(g),
        _ if local || !mappable(s) || outside => None,
        _ => {
            let key =
                crate::netns::owner_of_host(w, &addr).map_or_else(|| ns_key(s), |n| key_of(&n));
            map.borrow()
                .guest_port(key, map_proto(s), port)
                .filter(|_| is_local(&addr))
        }
    };
    if let Some(g) = guest {
        set_port(&mut addr, g);
    }
    addr
}

/// Mapped low ports apply to this socket ([`crate::portmap`]): TCP, UDP, not raw sockets.
fn mappable(s: &InetSocket) -> bool {
    !s.raw && matches!(map_proto(s), IPPROTO_TCP | IPPROTO_UDP)
}

/// The protocol in the low-port mapping registry: MPTCP's ports are the same as TCP's (Go 1.24 and
/// later listen with MPTCP by default).
fn map_proto(s: &InetSocket) -> i32 {
    if s.proto == IPPROTO_MPTCP {
        IPPROTO_TCP
    } else {
        s.proto
    }
}

/// The network namespace in the registry: the boot one is 0.
fn ns_key(s: &InetSocket) -> usize {
    s.net.as_ref().map_or(0, |n| Rc::as_ptr(n) as usize)
}

/// The key of a network namespace in the low-port mapping registry ([`ns_key`]).
fn key_of(n: &Rc<crate::nsproxy::NetNs>) -> usize {
    if n.host_net.is_none() {
        0
    } else {
        Rc::as_ptr(n) as usize
    }
}

/// The host address is local: loopback, the wildcard address, or one on a host interface
/// (connecting to its own wlan0 address).
fn is_local(addr: &[u8]) -> bool {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let Some(ip) = ip_of(addr) else {
        return false;
    };
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    tuxport_hal::net::interfaces().iter().any(|i| {
        i.addrs.iter().any(|(_, a, _)| {
            let own = match a.len() {
                4 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(&a[..]).unwrap())),
                16 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&a[..]).unwrap())),
                _ => return false,
            };
            own == ip
        })
    })
}

// ---- Shared helpers ----

/// The socket of an fd; ENOTSOCK when it is not a socket.
fn socket_of(cx: &Cx<'_>, fd: i32) -> Result<(SocketRef, bool), Errno> {
    let file = cx.fds().get(fd)?;
    let f = file.borrow();
    match &f.kind {
        _ if f.flags & oflags::O_PATH != 0 => Err(Errno::EBADF),
        FileKind::Socket(s) => Ok((s.clone(), f.nonblocking())),
        _ => Err(Errno::ENOTSOCK),
    }
}

/// AF_UNIX operations that wait or need the socket itself are handed to [`unix`].
fn is_unix(sock: &SocketRef) -> bool {
    matches!(&*sock.borrow(), Socket::Unix(_))
}

/// Reads a sockaddr given by the guest.
fn read_addr(cx: &Cx<'_>, addr: u64, len: u32) -> Result<Vec<u8>, Errno> {
    let len = len as usize;
    if len > SOCKADDR_MAX {
        return Err(Errno::EINVAL);
    }
    let mut b = vec![0u8; len];
    cx.mm().read(UserSlice::new(addr, len), &mut b)?;
    Ok(b)
}

/// Writes an address back to the guest: `addrlen` points at the buffer length, and the actual
/// length is written back (truncated when the buffer is too small). accept's and recvfrom's
/// `addr` may be NULL, meaning no address is wanted; otherwise a NULL `addrlen` is EFAULT
/// (move_addr_to_user).
fn write_addr(cx: &Cx<'_>, addr: u64, addrlen: u64, value: &[u8]) -> Result<(), Errno> {
    if addr == 0 {
        return Ok(());
    }
    write_addr_to(cx, addr, addrlen, value)
}

/// getsockname, getpeername: `addrlen` must be valid, and a negative length is EINVAL.
fn write_addr_to(cx: &Cx<'_>, addr: u64, addrlen: u64, value: &[u8]) -> Result<(), Errno> {
    let room = usize::try_from(i32::from_le_bytes(get(cx, addrlen)?)).map_err(|_| Errno::EINVAL)?;
    let n = room.min(value.len());
    cx.mm().write(UserSlice::new(addr, n), &value[..n])?;
    put(cx, addrlen, (value.len() as u32).to_le_bytes())
}

/// Waits for host socket `sock` (host fd `fd`) to become ready: EAGAIN at once when nonblocking,
/// `timeout_err` on timeout. Only this socket is registered ([`wake_objects`]): changes on other
/// files do not wake it, whereas before, every pipe or PTY read and write in the system woke the
/// threads blocked in recv once each.
fn wait_host(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    fd: i32,
    events: i16,
    nonblock: bool,
    deadline: Option<Instant>,
    timeout_err: Errno,
) -> Result<(), Errno> {
    if nonblock {
        return Err(Errno::EAGAIN);
    }
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return Err(timeout_err);
    }
    if cx.interrupted() {
        return Err(Errno::ERESTARTSYS);
    }
    let mut pfd = [(fd, events, 0)];
    cx.wait_objects(deadline, &mut pfd, wake::FILE, &[wake::of(sock)])
}

/// The network namespace this socket is in (the boot one is recorded as None).
fn net_of(cx: &Cx<'_>, s: &InetSocket) -> Rc<crate::nsproxy::NetNs> {
    s.net.clone().unwrap_or_else(|| cx.kr().init_ns.net.clone())
}

/// Puts a socket into the fd table; `flags` may include `SOCK_NONBLOCK`, `SOCK_CLOEXEC`.
fn install(cx: &mut Cx<'_>, sock: SocketRef, flags: u32) -> Result<i32, Errno> {
    let file = socket_file(sock, flags);
    cx.fds_mut().install(file, flags & SOCK_CLOEXEC != 0, 0)
}

/// The open file of a socket; `flags` may include `SOCK_NONBLOCK`.
fn socket_file(sock: SocketRef, flags: u32) -> FileRef {
    OpenFile::new(
        FileKind::Socket(sock),
        oflags::O_RDWR | (flags & SOCK_NONBLOCK),
    )
}

/// The port in a sockaddr (AF_INET, AF_INET6).
fn port_of(addr: &[u8]) -> Option<u16> {
    let family = i32::from(u16::from_le_bytes([*addr.first()?, *addr.get(1)?]));
    match family {
        AF_INET | AF_INET6 if addr.len() >= 4 => Some(u16::from_be_bytes([addr[2], addr[3]])),
        _ => None,
    }
}

fn set_port(addr: &mut [u8], port: u16) {
    addr[2..4].copy_from_slice(&port.to_be_bytes());
}

/// Whether a sockaddr holds an IPv6 address proper (not an IPv4-mapped one).
fn ipv6_proper(addr: &[u8]) -> bool {
    matches!(ip_of(addr), Some(std::net::IpAddr::V6(_)))
}

/// The IP in a sockaddr (the IPv4 part of an IPv4-mapped IPv6 address).
fn ip_of(addr: &[u8]) -> Option<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let family = i32::from(u16::from_le_bytes([*addr.first()?, *addr.get(1)?]));
    match family {
        AF_INET => Some(IpAddr::V4(Ipv4Addr::from(
            <[u8; 4]>::try_from(addr.get(4..8)?).ok()?,
        ))),
        AF_INET6 => {
            let v6 = Ipv6Addr::from(<[u8; 16]>::try_from(addr.get(8..24)?).ok()?);
            Some(v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4))
        }
        _ => None,
    }
}
