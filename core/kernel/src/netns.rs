//! New network namespaces ([`NetNs`]): each starts with only the loopback interface lo (virtual
//! devices are in [`crate::netdev`]).
//!
//! Sockets stay host sockets, and addresses are translated at the syscall boundary: the guest's
//! 127.0.0.x is the host's 127.a.b.x given to this namespace, and binding the wildcard address
//! binds 127.a.b.1; AF_INET6 ::1 and :: are the IPv4-mapped ::ffff:127.a.b.1 (host sockets do not
//! set `IPV6_V6ONLY`). As seen on Linux, 127.0.0.x can be bound even while lo is down or has no
//! address, but is reachable only while lo is up with an address covering it. Virtual addresses
//! and routed destinations are translated as [`to_host`] describes; other addresses cannot be
//! bound (EADDRNOTAVAIL) or reached (ENETUNREACH). A socket that connects, sends or listens
//! without being bound is first bound to 127.a.b.1, so that the host does not bind it to all
//! interfaces instead.
//!
//! lo's configuration is read and written through netlink (`RTM_NEWADDR`, `RTM_DELADDR`,
//! `RTM_NEWLINK`) and interface ioctls, which is how bubblewrap brings lo up.

use std::rc::Rc;

use crate::cred::{Cred, cap};
use crate::errno::Errno;
use crate::nsproxy::{Loopback, NetNs};
use tuxport_hal::net::Interface;

const AF_INET: i32 = 2;
const AF_INET6: i32 = 10;
const IFF_UP: u32 = 0x1;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_RUNNING: u32 = 0x40;
const IFF_LOWER_UP: u32 = 0x10000;
/// Interface index of lo in a new namespace.
pub(crate) const LO_INDEX: u32 = 1;
const LOOPBACK6: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

/// All network namespaces (address translation looks at virtual addresses in other
/// namespaces): the first is the initial one.
pub(crate) struct World {
    pub all: Vec<Rc<NetNs>>,
}

/// Host loopback block for the virtual addresses of the initial namespace: 127.255.0.0/16 (the
/// blocks 127.a.b.0/24 of new namespaces never use 255 for a).
pub(crate) const INIT_BLOCK: u8 = 255;

/// 127.a.b.254, which a new namespace binds as the source when connecting to its own virtual
/// address: translated back, it is this namespace's virtual address (so the guest's 127.0.0.254
/// is reported as the virtual address).
const SELF_VIA_VADDR: u8 = 254;

impl World {
    pub(crate) fn init(&self) -> &Rc<NetNs> {
        &self.all[0]
    }

    /// Which new namespace the host loopback block 127.a.b.0/24 belongs to.
    fn by_block(&self, a: u8, b: u8) -> Option<&Rc<NetNs>> {
        self.all.iter().find(|n| n.host_net == Some([a, b]))
    }

    /// Which namespace (other than `me`) has virtual address `ip`: when several do, the one in a
    /// subnet `me` has a directly connected route to.
    fn owner(&self, me: &NetNs, ip: [u8; 4]) -> Option<&Rc<NetNs>> {
        let mut found = self.all.iter().filter(|n| {
            !std::ptr::eq(n.as_ref(), me)
                && crate::netdev::local_v4(n).iter().any(|(a, _)| *a == ip)
        });
        let first = found.next()?;
        let mut rest = std::iter::once(first).chain(found);
        let direct = connected(me);
        Some(
            rest.find(|_| direct.iter().any(|&(net, p)| covers(net, p, ip)))
                .unwrap_or(first),
        )
    }
}

// ---- Translation to the host ----

/// What a guest address is used for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Use {
    /// bind: must be a local address (EADDRNOTAVAIL otherwise); the wildcard address also works.
    Bind,
    /// connect, sendto: must be reachable (ENETUNREACH otherwise).
    Reach,
}

/// A guest address translated to the host's, the result of [`to_host`].
pub(crate) struct Mapped {
    pub addr: Vec<u8>,
    /// The network namespace of the destination (low-port mapping goes by it); None means it is
    /// handed to the host network (going out through the default route, or not a virtual address
    /// in the initial namespace).
    pub target: Option<Rc<NetNs>>,
    /// Host address (port 0) an unbound socket is bound to first: in a new namespace, its
    /// 127.a.b.1; in the initial namespace connecting to another namespace, the host address of
    /// the virtual address facing the other side.
    pub source: Option<Vec<u8>>,
    /// Bound to a virtual address in a new namespace (127.a.b.1 on the host; getsockname reports
    /// the virtual address).
    pub vaddr: Option<[u8; 4]>,
}

impl Mapped {
    fn plain(addr: &[u8]) -> Mapped {
        Mapped {
            addr: addr.to_vec(),
            target: None,
            source: None,
            vaddr: None,
        }
    }
}

/// Translates an address given by the guest to the host's (bind, connect, sendto). Addresses
/// that are neither IPv4 nor IPv6 (AF_UNSPEC disconnects) are returned unchanged.
///
/// A new network namespace (127.a.b.0/24):
/// - 127.0.0.x is 127.a.b.x, the wildcard address is allowed only for bind, and 127.0.0.x is
///   unreachable while lo is down or has no address ([`Loopback::has_v4`]); ::1 and :: are the
///   IPv4-mapped 127.a.b.1. `icmp6` means an ICMPv6 socket: it cannot send to IPv4-mapped
///   addresses, so ::1 becomes the host's real ::1 (echo is answered by the host kernel), and the
///   wildcard address is bound to ::1 as well.
/// - Addresses on its own virtual devices are 127.a.b.1 too (the same as the wildcard address, so
///   a service listening on the wildcard address is reachable through any of them).
/// - Another namespace's virtual address: needs a route through a virtual device (ENETUNREACH
///   without one) and becomes that namespace's 127.c.d.1; those of the initial namespace become
///   its 127.255.x.y.
/// - An address that is nobody's virtual address: reachable through a route via a gateway (the
///   default route), it is handed to the host network unchanged; missing from a directly
///   connected subnet, EHOSTUNREACH; with no route, ENETUNREACH.
/// - Other addresses cannot be bound (EADDRNOTAVAIL).
///
/// The initial namespace: its own virtual addresses become 127.255.x.y; another namespace's
/// virtual addresses reachable through a route over a virtual device are handled as above; the
/// rest are unchanged.
pub(crate) fn to_host(
    w: &World,
    me: &Rc<NetNs>,
    addr: &[u8],
    usage: Use,
    icmp6: bool,
) -> Result<Mapped, Errno> {
    let Some([a, b]) = me.host_net else {
        return init_to_host(w, me, addr, usage);
    };
    let lo = me.lo.borrow();
    let fail = || match usage {
        Use::Bind => Errno::EADDRNOTAVAIL,
        Use::Reach => Errno::ENETUNREACH,
    };
    let block = |x: u8| [127, a, b, x];
    let src = || implicit_bind(me, family(addr).unwrap_or(AF_INET), icmp6);
    let mapped_to = |ip: [u8; 4], target: &Rc<NetNs>, vaddr| Mapped {
        addr: with_ipv4(addr, ip),
        target: Some(target.clone()),
        source: src(),
        vaddr,
    };
    match family(addr) {
        Some(AF_INET6) if addr.len() >= 24 && icmp6 => {
            let ip = &addr[8..24];
            if ip == LOOPBACK6 && !(lo.up && lo.v6) {
                return Err(Errno::EADDRNOTAVAIL);
            }
            if !(ip == LOOPBACK6 || ip == [0; 16] && usage == Use::Bind) {
                return Err(fail());
            }
            let mut out = addr.to_vec();
            out[8..24].copy_from_slice(&LOOPBACK6);
            if let Some(s) = out.get_mut(24..28) {
                s.fill(0);
            }
            Ok(Mapped {
                target: Some(me.clone()),
                source: src(),
                ..Mapped::plain(&out)
            })
        }
        Some(AF_INET6) if addr.len() >= 24 && mapped(&addr[8..24]).is_none() => {
            let ip: [u8; 16] = addr[8..24].try_into().unwrap();
            if ip == [0; 16] && usage == Use::Bind {
                return Ok(mapped_to(block(1), me, None));
            }
            if ip == LOOPBACK6 {
                // Without ::1 a connection cannot choose a source address
                if !(lo.up && lo.v6) {
                    return Err(Errno::EADDRNOTAVAIL);
                }
                return Ok(mapped_to(block(1), me, None));
            }
            // Other IPv6 addresses: those going out through a route over a virtual device are
            // handed to the host network
            if usage == Use::Reach && crate::netdev::routes_v6(me, &ip) {
                return Ok(Mapped::plain(addr));
            }
            Err(fail())
        }
        Some(AF_INET | AF_INET6) if ipv4_of(addr).is_some() => {
            let ip = ipv4_of(addr).unwrap();
            if ip == [0; 4] {
                return match usage {
                    Use::Bind => Ok(mapped_to(block(1), me, None)),
                    Use::Reach => Err(fail()),
                };
            }
            if ip[0] == 127 {
                return if ip[..3] == [127, 0, 0] && lo.has_v4(ip, usage) {
                    Ok(mapped_to(block(ip[3]), me, None))
                } else {
                    Err(fail())
                };
            }
            drop(lo);
            if crate::netdev::local_v4(me).iter().any(|(x, _)| *x == ip) {
                if usage == Use::Bind {
                    return Ok(mapped_to(block(1), me, Some(ip)));
                }
                // Connecting to its own virtual address: the source is bound to 127.a.b.254, and
                // both ends translate back to the virtual address
                let mut m = mapped_to(block(1), me, None);
                m.source = m.source.map(|s| with_ipv4(&s, block(SELF_VIA_VADDR)));
                return Ok(m);
            }
            if usage == Use::Bind {
                return Err(fail());
            }
            let route = crate::netdev::route_v4(me, ip).ok_or_else(fail)?;
            match w.owner(me, ip) {
                Some(m) => Ok(mapped_to(host_of(w, m, ip), m, None)),
                // Handed to the host network (going out through the gateway): no loopback address
                // is bound first, or the traffic could not get out
                None if route.gateway.is_some() => Ok(Mapped::plain(addr)),
                None => Err(Errno::EHOSTUNREACH),
            }
        }
        Some(AF_INET | AF_INET6) => Err(Errno::EINVAL),
        _ => Ok(Mapped::plain(addr)),
    }
}

/// [`to_host`] in the initial namespace.
fn init_to_host(w: &World, me: &Rc<NetNs>, addr: &[u8], usage: Use) -> Result<Mapped, Errno> {
    let Some(ip) = ipv4_of(addr).filter(|ip| ip[0] != 127 && *ip != [0; 4]) else {
        return Ok(Mapped::plain(addr));
    };
    if crate::netdev::local_v4(me).iter().any(|(x, _)| *x == ip) {
        return Ok(Mapped {
            addr: with_ipv4(addr, init_host(me, ip)),
            target: Some(me.clone()),
            source: None,
            vaddr: None,
        });
    }
    if usage == Use::Bind {
        return Ok(Mapped::plain(addr));
    }
    // Only routes through virtual devices count: the host's subnets belong to the host
    let Some(route) = crate::netdev::route_v4(me, ip) else {
        return Ok(Mapped::plain(addr));
    };
    match w.owner(me, ip) {
        Some(m) => {
            let src = source(me, ip).map(|s| with_ipv4(&any_like(addr), init_host(me, s)));
            Ok(Mapped {
                addr: with_ipv4(addr, host_of(w, m, ip)),
                target: Some(m.clone()),
                source: src,
                vaddr: None,
            })
        }
        None if route.gateway.is_none() => Err(Errno::EHOSTUNREACH),
        None => Ok(Mapped::plain(addr)),
    }
}

/// The wildcard address of the same family as `addr`, with port 0.
pub(crate) fn any_like(addr: &[u8]) -> Vec<u8> {
    let mut s = vec![
        0u8;
        if family(addr) == Some(AF_INET6) {
            28
        } else {
            16
        }
    ];
    s[..2].copy_from_slice(&addr[..2]);
    s
}

/// The host address (port 0) an unbound socket is bound to first.
pub(crate) fn implicit_bind(ns: &NetNs, family: i32, icmp6: bool) -> Option<Vec<u8>> {
    let [a, b] = ns.host_net?;
    Some(if icmp6 {
        let mut s = vec![0u8; 28];
        s[..2].copy_from_slice(&(AF_INET6 as u16).to_le_bytes());
        s[8..24].copy_from_slice(&LOOPBACK6);
        s
    } else if family == AF_INET6 {
        let mut s = vec![0u8; 28];
        s[..2].copy_from_slice(&(AF_INET6 as u16).to_le_bytes());
        s[18..20].copy_from_slice(&[0xff, 0xff]);
        s[20..24].copy_from_slice(&[127, a, b, 1]);
        s
    } else {
        let mut s = vec![0u8; 16];
        s[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        s[4..8].copy_from_slice(&[127, a, b, 1]);
        s
    })
}

impl Loopback {
    /// Whether IPv4 loopback address `ip` may be used this way: as seen on Linux, 127.0.0.0/8 can
    /// always be bound (even while lo is down or has no address), and is reachable only when lo
    /// is up and the address's subnet is configured on it.
    fn has_v4(&self, ip: [u8; 4], usage: Use) -> bool {
        let covered = |&(a, len): &([u8; 4], u8)| {
            let mask = u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0);
            u32::from_be_bytes(a) & mask == u32::from_be_bytes(ip) & mask
        };
        match usage {
            Use::Bind => true,
            Use::Reach => self.up && self.v4.iter().any(covered),
        }
    }
}

/// The host address of virtual address `ip` in another namespace `m`: 127.c.d.1 for a new
/// namespace, 127.255.x.y for the initial one.
fn host_of(w: &World, m: &NetNs, ip: [u8; 4]) -> [u8; 4] {
    match m.host_net {
        Some([c, d]) => [127, c, d, 1],
        None => init_host(w.init(), ip),
    }
}

/// The host loopback address (127.255.x.y) of virtual address `v` in the initial namespace,
/// allocated if there is none.
pub(crate) fn init_host(init: &NetNs, v: [u8; 4]) -> [u8; 4] {
    let mut d = init.devs.borrow_mut();
    if let Some((_, h)) = d.hostmap.iter().find(|(g, _)| *g == v) {
        return *h;
    }
    let n = (1..0xffffu16)
        .find(|n| {
            let h = [127, INIT_BLOCK, (n >> 8) as u8, *n as u8];
            !d.hostmap.iter().any(|(_, x)| *x == h)
        })
        .unwrap_or(1);
    let h = [127, INIT_BLOCK, (n >> 8) as u8, n as u8];
    d.hostmap.push((v, h));
    h
}

// ---- Translation back to the guest ----

/// The role of a host address ([`from_host`]).
pub(crate) enum Role<'a> {
    /// The peer.
    Peer,
    /// The local end: whether the guest bound the wildcard address and is not connected yet
    /// (reporting the wildcard address), the virtual address it is bound to (127.a.b.1 on the
    /// host), and the peer's host address (the local address facing it).
    Local {
        any: bool,
        vaddr: Option<[u8; 4]>,
        peer: Option<&'a [u8]>,
    },
}

/// Translates an address given by the host (local or peer) to what processes in `me` see: its own
/// block 127.a.b.x is 127.0.0.x (127.a.b.1 may also be the IPv4 127.0.0.1 or the ::1 that was
/// connected to, decided by the address family when it cannot be told apart), another
/// namespace's 127.c.d.1 is that namespace's virtual address facing `me`, and 127.255.x.y is a
/// virtual address of the initial namespace. For a connection in a new namespace going out
/// through the host network, the local host address becomes the source address this namespace
/// uses toward the peer. For `icmp6` see [`to_host`].
pub(crate) fn from_host(
    w: &World,
    me: &NetNs,
    addr: &[u8],
    role: Role<'_>,
    icmp6: bool,
) -> Vec<u8> {
    let any = matches!(role, Role::Local { any: true, .. });
    let mut out = addr.to_vec();
    if any && matches!(family(addr), Some(AF_INET | AF_INET6)) {
        let range = if family(addr) == Some(AF_INET) {
            4..8
        } else {
            8..24
        };
        if let Some(s) = out.get_mut(range) {
            s.fill(0);
        }
        return out;
    }
    if icmp6 && family(addr) == Some(AF_INET6) {
        return out;
    }
    let Some(x) = ipv4_of(addr) else {
        return out;
    };
    if x == [0; 4] {
        return out;
    }
    // A connection forwarded in from outside (crate::forward): the client's real address and port
    if x[..2] == [127, crate::forward::OUTSIDE_BLOCK]
        && let Some((ip, port)) =
            crate::forward::outside_peer(x, u16::from_be_bytes([addr[2], addr[3]]))
    {
        out = with_ipv4(addr, ip);
        out[2..4].copy_from_slice(&port.to_be_bytes());
        return out;
    }
    let guest_of_peer = |p: &[u8]| from_host(w, me, p, Role::Peer, icmp6);
    match me.host_net {
        Some([a, b]) => {
            if x[..3] == [127, a, b] {
                let first = || crate::netdev::local_v4(me).first().map(|(v, _)| *v);
                if x[3] == SELF_VIA_VADDR
                    && let Some(v) = first()
                {
                    return with_ipv4(addr, v);
                }
                if let Role::Local { vaddr, peer, .. } = &role {
                    if let Some(v) = vaddr.filter(|_| x[3] == 1) {
                        return with_ipv4(addr, v);
                    }
                    // Connected to its own virtual address: the local end is the virtual address
                    // too
                    if peer.and_then(ipv4_of) == Some([127, a, b, SELF_VIA_VADDR])
                        && let Some(v) = first()
                    {
                        return with_ipv4(addr, v);
                    }
                    // Connected from another namespace: the local end is the address facing the
                    // other side
                    let remote = peer
                        .and_then(ipv4_of)
                        .filter(|p| p[0] != 127 || p[1..3] != [a, b]);
                    if remote.is_some()
                        && let Some(g) = peer.map(guest_of_peer).as_deref().and_then(ipv4_of)
                        && let Some(s) = source(me, g)
                    {
                        return with_ipv4(addr, s);
                    }
                }
                if family(addr) == Some(AF_INET) {
                    out[4..8].copy_from_slice(&[127, 0, 0, x[3]]);
                } else if x[3] == 1 {
                    // 127.a.b.1 is the guest's ::1 (or possibly the IPv4 127.0.0.1 that was
                    // connected to)
                    out[8..24].copy_from_slice(&LOOPBACK6);
                } else {
                    out[20..24].copy_from_slice(&[127, 0, 0, x[3]]);
                }
                return out;
            }
            if let Some(g) = remote_guest(w, me, x) {
                return with_ipv4(addr, g);
            }
            // A connection going out through the host network: the local end is this
            // namespace's source address
            if x[0] != 127
                && let Role::Local { peer, .. } = &role
                && let Some(s) = peer
                    .and_then(ipv4_of)
                    .and_then(|p| source(me, p))
                    .or_else(|| crate::netdev::local_v4(me).first().map(|(a, _)| *a))
            {
                return with_ipv4(addr, s);
            }
            out
        }
        None => remote_guest(w, me, x).map_or(out, |g| with_ipv4(addr, g)),
    }
}

/// The virtual address seen in `me` for another namespace's loopback address on the host
/// (127.c.d.1, 127.255.x.y).
fn remote_guest(w: &World, me: &NetNs, x: [u8; 4]) -> Option<[u8; 4]> {
    if x[0] != 127 {
        return None;
    }
    if x[1] == INIT_BLOCK {
        return init_guest(w.init(), x);
    }
    let m = w.by_block(x[1], x[2]).filter(|_| x[3] == 1)?;
    facing(m, me)
}

/// Which virtual address in the initial namespace the host's 127.255.x.y stands for.
fn init_guest(init: &NetNs, h: [u8; 4]) -> Option<[u8; 4]> {
    init.devs
        .borrow()
        .hostmap
        .iter()
        .find(|(_, x)| *x == h)
        .map(|(g, _)| *g)
}

/// The virtual address of `m` that faces `me`: one in a subnet `me` has a directly connected
/// route to, otherwise the first.
fn facing(m: &NetNs, me: &NetNs) -> Option<[u8; 4]> {
    let mine = crate::netdev::local_v4(m);
    let direct = connected(me);
    mine.iter()
        .find(|(a, _)| direct.iter().any(|&(net, p)| covers(net, p, *a)))
        .or(mine.first())
        .map(|(a, _)| *a)
}

/// The local end in `me` when sending to guest address `peer`: `local` (the local end on the
/// host, whose port is kept) with the source address used toward the peer.
pub(crate) fn local_toward(me: &NetNs, local: &[u8], peer: &[u8]) -> Option<Vec<u8>> {
    let ip = ipv4_of(peer)?;
    let src = source(me, ip)?;
    Some(with_ipv4(local, src))
}

// ---- Listeners ----

/// What [`add_listener`] records: the guest port, whether the wildcard address is bound, the
/// virtual address bound, the local end on the host, the address family and `IPV6_V6ONLY`, and
/// whether it is UDP.
#[derive(Clone, Copy)]
pub(crate) struct ListenAt<'a> {
    pub port: u16,
    pub any: bool,
    pub vaddr: Option<[u8; 4]>,
    pub host: &'a [u8],
    pub v6: bool,
    pub v6only: bool,
    pub udp: bool,
}

/// A TCP socket in a new network namespace starts listening, or a UDP socket binds: records it
/// ([`listening`]).
pub(crate) fn add_listener(me: &NetNs, l: &ListenAt<'_>) -> std::num::NonZeroU64 {
    let ListenAt {
        port,
        any,
        vaddr,
        host,
        v6,
        v6only,
        udp,
    } = *l;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // The guest address bound: the virtual address; 127.a.b.x is 127.0.0.x, and AF_INET6
    // 127.a.b.1 is ::1
    let addr = if any {
        None
    } else if let Some(v) = vaddr {
        Some(v.to_vec())
    } else {
        match ipv4_of(host) {
            Some([_, _, _, 1]) if v6 && mapped(&host[8..24]).is_some() => Some(LOOPBACK6.to_vec()),
            // 127.a.b.254, bound when sending to its own virtual address: it receives what is
            // sent to the virtual address
            Some([_, _, _, SELF_VIA_VADDR]) => {
                crate::netdev::local_v4(me).first().map(|(a, _)| a.to_vec())
            }
            Some([_, _, _, x]) => Some(vec![127, 0, 0, x]),
            None => None,
        }
    };
    me.listeners.borrow_mut().push(crate::nsproxy::Listener {
        id,
        port,
        addr,
        v6,
        v6only,
        udp,
    });
    std::num::NonZeroU64::new(id).expect("starts at 1")
}

/// Whether a TCP socket (with `udp`, a bound UDP socket; [`crate::nsproxy::Listener`]) in new
/// network namespace `t` is listening on guest address `dst` (a sockaddr, as seen in `t`): a
/// wildcard listener accepts IPv4 and IPv6 (an AF_INET one only IPv4, one with `IPV6_V6ONLY` set
/// only IPv6), and one bound to an address accepts only what is sent to it.
pub(crate) fn listening(t: &NetNs, dst: &[u8], udp: bool) -> bool {
    let Some(port) = dst.get(2..4).map(|p| u16::from_be_bytes([p[0], p[1]])) else {
        return true;
    };
    let ip: Vec<u8> = match (family(dst), ipv4_of(dst)) {
        (_, Some(v4)) => v4.to_vec(),
        (Some(AF_INET6), None) if dst.len() >= 24 => dst[8..24].to_vec(),
        _ => return true,
    };
    t.listeners.borrow().iter().any(|l| {
        l.port == port
            && l.udp == udp
            && match &l.addr {
                None if ip.len() == 4 => !l.v6 || !l.v6only,
                None => l.v6,
                Some(a) => *a == ip,
            }
    })
}

// ---- Sending to its own virtual address ----

/// Whether host address `host` is `me`'s 127.a.b.254 (the source bound when sending to its own
/// virtual address).
pub(crate) fn is_self_source(me: &NetNs, host: &[u8]) -> bool {
    matches!((me.host_net, ipv4_of(host)), (Some([a, b]), Some(x)) if x == [127, a, b, SELF_VIA_VADDR])
}

/// A datagram in `me` sent to its own virtual address `dst` (the guest's): when the port is that
/// of a socket bound to 127.a.b.254 ([`crate::nsproxy::Loopback::self_udp`], a client that sent to
/// its own virtual address, so this is the reply to it), the host destination `host` becomes
/// 127.a.b.254.
pub(crate) fn toward_self(me: &NetNs, dst: &[u8], host: Vec<u8>) -> Vec<u8> {
    let (Some([a, b]), Some(ip)) = (me.host_net, ipv4_of(dst)) else {
        return host;
    };
    let port = u16::from_be_bytes([dst[2], dst[3]]);
    if me.lo.borrow().self_udp.contains(&port)
        && crate::netdev::local_v4(me).iter().any(|(x, _)| *x == ip)
    {
        return with_ipv4(&host, [127, a, b, SELF_VIA_VADDR]);
    }
    host
}

/// A socket bound to 127.a.b.254 receives a datagram from 127.a.b.1 (`host` is the source): the
/// other side sent it to its own virtual address, so the source is that address too
/// ([`toward_self`]).
pub(crate) fn from_self(me: &NetNs, host: &[u8]) -> Option<Vec<u8>> {
    let ([a, b], x) = (me.host_net?, ipv4_of(host)?);
    if x != [127, a, b, 1] {
        return None;
    }
    let v = crate::netdev::local_v4(me).first()?.0;
    Some(with_ipv4(host, v))
}

// ---- OUTPUT and PREROUTING nat ----

/// nftables OUTPUT nat ([`crate::nft::Nft::nat`]): a connection or datagram in `me` with protocol
/// `proto` (TCP, UDP) to guest address `dst` (a sockaddr). Returns the new destination when
/// DNATed, and EPERM when a rule drops it. `vaddr` is the virtual address the socket is bound to
/// (the source; chosen by route when absent), `sport` is the local port, and `mark` is the
/// socket's `SO_MARK`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn output_nat(
    me: &NetNs,
    proto: u8,
    vaddr: Option<[u8; 4]>,
    sport: impl FnOnce() -> u16,
    dst: &[u8],
    uid: u32,
    gid: u32,
    mark: u32,
) -> Result<Option<Vec<u8>>, Errno> {
    if !me.nft.borrow().has_nat(crate::nft::HOOK_LOCAL_OUT) {
        return Ok(None);
    }
    let Some((ip, _)) = nat_ip(dst) else {
        return Ok(None);
    };
    let src = match vaddr {
        Some(v) if ip.len() == 4 => v.to_vec(),
        _ if is_loopback(&ip) => ip.clone(),
        _ if ip.len() == 4 => source(me, ip[..4].try_into().unwrap())
            .unwrap_or_default()
            .to_vec(),
        _ => vec![0; 16],
    };
    let packet = NatPacket {
        hook: crate::nft::HOOK_LOCAL_OUT,
        proto,
        src,
        sport: sport(),
        uid,
        gid,
        iif: None,
        ct_state: crate::nft::CT_NEW,
        mark,
    };
    hook_nat(me, &packet, dst)
}

/// The other namespace that a connection or datagram in `me` to guest address `dst` (already
/// translated to the host's `m`) enters, whose PREROUTING must run ([`prerouting_nat`]):
/// - the destination is in another namespace (a virtual address);
/// - going out through the host network from a new namespace enters the initial namespace when
///   the gateway is its address (a container reaching the outside through the outer bridge), or
///   the destination is one of its local addresses (a host address: a container connecting to a
///   port published on an outer address). With REDIRECT and DNAT in the outer PREROUTING for
///   traffic from the bridge, a container's outside connections and DNS can be transparently
///   proxied or hijacked. When the gateway is in another namespace (a router-like "container"),
///   nobody's PREROUTING is passed.
pub(crate) fn entering(w: &World, me: &Rc<NetNs>, m: &Mapped, dst: &[u8]) -> Option<Rc<NetNs>> {
    match &m.target {
        Some(t) if !Rc::ptr_eq(t, me) => Some(t.clone()),
        Some(_) => None,
        None => {
            let init = w.init();
            if me.host_net.is_none() || !init.nft.borrow().has_nat(crate::nft::HOOK_PRE_ROUTING) {
                return None;
            }
            let ip = ipv4_of(dst)?;
            let via_init = crate::netdev::route_v4(me, ip)
                .and_then(|e| <[u8; 4]>::try_from(e.gateway?.as_slice()).ok())
                .and_then(|gw| w.owner(me, gw))
                .is_some_and(|o| Rc::ptr_eq(o, init));
            (via_init || init_local(init, ip)).then(|| init.clone())
        }
    }
}

/// A connection or datagram entering `t` from another namespace `from` (to an address of `t`,
/// guest sockaddr `dst`; virtual addresses are the same for everyone) passes `t`'s PREROUTING nat:
/// returns the new destination as seen in `t` when DNATed. The source address is the one `from`
/// chooses toward it, and the input interface is the interface in `t` facing that source.
pub(crate) fn prerouting_nat(
    from: &NetNs,
    t: &NetNs,
    proto: u8,
    sport: u16,
    dst: &[u8],
) -> Result<Option<Vec<u8>>, Errno> {
    if !t.nft.borrow().has_nat(crate::nft::HOOK_PRE_ROUTING) {
        return Ok(None);
    }
    let Some((ip, true)) = nat_ip(dst) else {
        return Ok(None);
    };
    let src = source(from, ip[..4].try_into().unwrap()).unwrap_or_default();
    let iif = crate::netdev::route_v4(t, src).and_then(|e| {
        let i = crate::netdev::interfaces(t)
            .into_iter()
            .find(|i| i.index == e.index)?;
        Some(crate::nft::Iif {
            index: i.index,
            name: i.name.into_bytes(),
            addr: crate::netdev::first_v4(t, e.index)
                .map(|a| a.to_vec())
                .unwrap_or_default(),
        })
    });
    let packet = NatPacket {
        hook: crate::nft::HOOK_PRE_ROUTING,
        proto,
        src: src.to_vec(),
        sport,
        uid: 0,
        gid: 0,
        iif: Some(iif.unwrap_or_default()),
        ct_state: crate::nft::CT_NEW,
        mark: 0,
    };
    hook_nat(t, &packet, dst)
}

// ---- Filter chains along the path ----

/// One namespace a packet enters in turn in [`path_filter`]: the hook, the input interface and
/// the output interface (on FORWARD).
struct Step<'a> {
    ns: &'a Rc<NetNs>,
    hook: u32,
    iif: crate::nft::Iif,
    oif: Option<Vec<u8>>,
}

impl<'a> Step<'a> {
    fn new(ns: &'a Rc<NetNs>, hook: u32, iif: crate::nft::Iif, oif: Option<Vec<u8>>) -> Step<'a> {
        Step { ns, hook, iif, oif }
    }
}

/// Arguments of [`path_filter`]: what the packet looks like.
pub(crate) struct Flow<'a> {
    pub proto: u8,
    /// The virtual address the socket is bound to (the source; chosen by route when absent).
    pub vaddr: Option<[u8; 4]>,
    pub sport: u16,
    /// The destination as seen in `me` after OUTPUT nat.
    pub dst: &'a [u8],
    /// As seen on arrival in the destination's namespace (after PREROUTING nat).
    pub there: &'a [u8],
    pub uid: u32,
    pub gid: u32,
    /// UDP replying to a peer it has received from (conntrack ESTABLISHED).
    pub established: bool,
    /// The socket's `SO_MARK`: packets on OUTPUT and those coming back to their own namespace
    /// through lo carry it; it is cleared when entering another namespace through a veth.
    pub mark: u32,
}

/// The filter chains a connection or datagram sent by a socket in `me` passes
/// (docs/rounds/175-filter.md), IPv4 only:
///
/// 1. `me`'s OUTPUT.
/// 2. From a new namespace, through the initial namespace: going out to the host network (`t` is
///    None) takes its FORWARD, with a host interface as the output; going to it takes INPUT; going
///    to another namespace whose destination is not in `me`'s directly connected subnets (through
///    a gateway) takes FORWARD.
/// 3. On arrival in the destination's namespace `t`: INPUT (for the initial namespace see above).
///
/// On entering a namespace, the filter chains on its PREROUTING (the raw table and the like) come
/// first. Returns the result and whether it was dropped or rejected on OUTPUT.
pub(crate) fn path_filter(
    w: &World,
    me: &Rc<NetNs>,
    t: Option<&Rc<NetNs>>,
    f: &Flow<'_>,
) -> (crate::nft::Outcome, bool) {
    use crate::nft::{HOOK_LOCAL_OUT, HOOK_PRE_ROUTING, Outcome};
    const HOOK_LOCAL_IN: u32 = 1;
    const HOOK_FORWARD: u32 = 2;
    let ct_state = if f.established {
        crate::nft::CT_ESTABLISHED
    } else {
        crate::nft::CT_NEW
    };
    let (Some((ip, true)), Some((final_ip, true))) = (nat_ip(f.dst), nat_ip(f.there)) else {
        return (Outcome::Accept, false);
    };
    let ip: [u8; 4] = ip[..].try_into().unwrap();
    let final_ip: [u8; 4] = final_ip[..].try_into().unwrap();
    let src = match f.vaddr {
        Some(v) => v,
        None if ip[0] == 127 => ip,
        None => source(me, ip).unwrap_or([0; 4]),
    };
    let packet = |hook: u32, iif: Option<crate::nft::Iif>, ns: &NetNs| NatPacket {
        hook,
        proto: f.proto,
        src: src.to_vec(),
        sport: f.sport,
        uid: f.uid,
        gid: f.gid,
        iif,
        ct_state,
        mark: if std::ptr::eq(ns, me.as_ref()) {
            f.mark
        } else {
            0
        },
    };
    let pass = |o: &Outcome| matches!(o, Outcome::Accept | Outcome::Dnat(..));
    // 1. OUTPUT
    if me.nft.borrow().has_filter(HOOK_LOCAL_OUT) {
        let o = hook_eval(me, &packet(HOOK_LOCAL_OUT, None, me), f.dst, None, true);
        if !pass(&o) {
            return (o, true);
        }
    }
    // The namespaces entered in turn: (namespace, hook, input interface, output interface)
    let init = w.init();
    let lo = crate::nft::Iif {
        index: 1,
        name: b"lo".to_vec(),
        addr: vec![127, 0, 0, 1],
    };
    let mut steps: Vec<Step<'_>> = Vec::new();
    let new_ns = me.host_net.is_some();
    match t {
        None if new_ns => {
            let oif = outer_iface(init).map(|(i, _)| i.name).unwrap_or_default();
            steps.push(Step::new(
                init,
                HOOK_FORWARD,
                iif_facing(init, src),
                Some(oif),
            ));
        }
        None => {}
        Some(x) if Rc::ptr_eq(x, me) => steps.push(Step::new(me, HOOK_LOCAL_IN, lo, None)),
        Some(x) if Rc::ptr_eq(x, init) => {
            steps.push(Step::new(init, HOOK_LOCAL_IN, iif_facing(init, src), None))
        }
        Some(x) => {
            // Not through the initial namespace: sent to x's address in a directly connected
            // subnet (x's PREROUTING may DNAT it again afterwards). Sent to an address behind the
            // gateway or to a local address of the outer namespace, and DNATed to x in the outer
            // PREROUTING (a container connecting to a published port, or going outside and DNATed
            // to another container): all of these pass the outer FORWARD
            let direct = crate::netdev::route_v4(me, ip).is_some_and(|e| e.gateway.is_none())
                && !init_local(init, ip);
            if new_ns && !direct {
                let oif = iif_facing(init, final_ip).name;
                steps.push(Step::new(
                    init,
                    HOOK_FORWARD,
                    iif_facing(init, src),
                    Some(oif),
                ));
            }
            steps.push(Step::new(x, HOOK_LOCAL_IN, iif_facing(x, src), None));
        }
    }
    for Step { ns, hook, iif, oif } in steps {
        let (pre, this) = {
            let nft = ns.nft.borrow();
            (nft.has_filter(HOOK_PRE_ROUTING), nft.has_filter(hook))
        };
        // What a namespace sends to itself over loopback also passes PREROUTING, as on Linux
        if pre {
            let o = hook_eval(
                ns,
                &packet(HOOK_PRE_ROUTING, Some(iif.clone()), ns),
                f.there,
                None,
                true,
            );
            if !pass(&o) {
                return (o, false);
            }
        }
        if this {
            let o = hook_eval(ns, &packet(hook, Some(iif), ns), f.there, oif, true);
            if !pass(&o) {
                return (o, false);
            }
        }
    }
    (Outcome::Accept, false)
}

/// Local addresses in the initial namespace `init` (on host interfaces, lo and virtual devices).
fn init_local(init: &NetNs, ip: [u8; 4]) -> bool {
    crate::netdev::interfaces(init)
        .iter()
        .any(|i| i.addrs.iter().any(|(_, a, _)| a[..] == ip[..]))
}

// ---- Forwarding from outside ----

/// Source address of packets from outside (TEST-NET-1).
const EXTERNAL: [u8; 4] = [192, 0, 2, 1];

/// The PREROUTING nat result for a connection from outside the host (another device, another app
/// on the phone) to `dst` listening in the initial namespace `init` (a guest sockaddr, with the
/// wildcard address replaced by one of the host's addresses): the source is taken to be 192.0.2.1
/// (TEST-NET-1), and the input interface is the host's first non-loopback interface with an IPv4
/// address.
pub(crate) fn external_nat(init: &NetNs, proto: u8, dst: &[u8]) -> Option<Vec<u8>> {
    if !init.nft.borrow().has_nat(crate::nft::HOOK_PRE_ROUTING) {
        return None;
    }
    let outer = outer_iface(init);
    let mut dst = dst.to_vec();
    if ipv4_of(&dst) == Some([0; 4]) {
        dst = with_ipv4(&dst, outer.as_ref().map_or([127, 0, 0, 1], |(_, a)| *a));
    }
    let packet = NatPacket {
        hook: crate::nft::HOOK_PRE_ROUTING,
        proto,
        src: EXTERNAL.to_vec(),
        sport: 40000,
        uid: 0,
        gid: 0,
        iif: Some(outer.map(|(i, _)| i).unwrap_or_default()),
        ct_state: crate::nft::CT_NEW,
        mark: 0,
    };
    hook_nat(init, &packet, &dst).ok().flatten()
}

/// Forwarding from outside the host through the initial namespace `init` to the DNAT
/// destination `there` (a guest address in namespace `t`): passes `init`'s FORWARD (entering on
/// the host's interface) and the filter chains on `t`'s PREROUTING and INPUT; what is dropped or
/// rejected is not forwarded.
pub(crate) fn external_filter(init: &NetNs, t: Option<&NetNs>, proto: u8, there: &[u8]) -> bool {
    use crate::nft::{HOOK_PRE_ROUTING, Outcome};
    const HOOK_LOCAL_IN: u32 = 1;
    const HOOK_FORWARD: u32 = 2;
    let Some((ip, true)) = nat_ip(there) else {
        return true;
    };
    let ip: [u8; 4] = ip[..].try_into().unwrap();
    let packet = |hook: u32, iif: crate::nft::Iif| NatPacket {
        hook,
        proto,
        src: EXTERNAL.to_vec(),
        sport: 40000,
        uid: 0,
        gid: 0,
        iif: Some(iif),
        ct_state: crate::nft::CT_NEW,
        mark: 0,
    };
    let pass = |o: Outcome| matches!(o, Outcome::Accept | Outcome::Dnat(..));
    let outer = outer_iface(init).map(|(i, _)| i).unwrap_or_default();
    if let Some(t) = t.filter(|t| !std::ptr::eq(*t, init)) {
        if init.nft.borrow().has_filter(HOOK_FORWARD) {
            let oif = iif_facing(init, ip).name;
            if !pass(hook_eval(
                init,
                &packet(HOOK_FORWARD, outer.clone()),
                there,
                Some(oif),
                true,
            )) {
                return false;
            }
        }
        let iif = iif_facing(t, EXTERNAL);
        for hook in [HOOK_PRE_ROUTING, HOOK_LOCAL_IN] {
            if t.nft.borrow().has_filter(hook)
                && !pass(hook_eval(t, &packet(hook, iif.clone()), there, None, true))
            {
                return false;
            }
        }
    } else if init.nft.borrow().has_filter(HOOK_LOCAL_IN)
        && !pass(hook_eval(
            init,
            &packet(HOOK_LOCAL_IN, outer),
            there,
            None,
            true,
        ))
    {
        return false;
    }
    true
}

// ---- Synthetic packets ----

/// The parts of a synthetic packet besides the socket ([`hook_nat`]).
struct NatPacket {
    hook: u32,
    proto: u8,
    src: Vec<u8>,
    sport: u16,
    uid: u32,
    gid: u32,
    iif: Option<crate::nft::Iif>,
    ct_state: u32,
    /// The packet's initial mark ([`crate::nft::Packet::mark`]).
    mark: u32,
}

/// nat on hook `p.hook` of `ns` ([`crate::nft::Nft::nat`]): for a packet to guest address `dst`,
/// returns the new destination when DNATed (keeping the port when it is not changed), and EPERM
/// when a rule drops it.
fn hook_nat(ns: &NetNs, p: &NatPacket, dst: &[u8]) -> Result<Option<Vec<u8>>, Errno> {
    let Some((_, v4)) = nat_ip(dst) else {
        return Ok(None);
    };
    match hook_eval(ns, p, dst, None, false) {
        crate::nft::Outcome::Accept => Ok(None),
        crate::nft::Outcome::Drop | crate::nft::Outcome::Reject(_) => Err(Errno::EPERM),
        crate::nft::Outcome::Dnat(a, port) => {
            let mut out = if v4 {
                with_ipv4(dst, a[..4].try_into().map_err(|_| Errno::EINVAL)?)
            } else {
                let mut o = dst.to_vec();
                o[8..24].copy_from_slice(&a);
                o
            };
            if let Some(port) = port {
                out[2..4].copy_from_slice(&port.to_be_bytes());
            }
            Ok(Some(out))
        }
    }
}

/// Builds a packet to `dst` from `p` and runs the filter chains (`filter`) or the nat chains on
/// hook `p.hook` of `ns`. `oif` is the name of a given output interface (on FORWARD); with None,
/// it is chosen by route.
fn hook_eval(
    ns: &NetNs,
    p: &NatPacket,
    dst: &[u8],
    oif: Option<Vec<u8>>,
    filter: bool,
) -> crate::nft::Outcome {
    let Some((ip, v4)) = nat_ip(dst) else {
        return crate::nft::Outcome::Accept;
    };
    let local = |a: &[u8]| {
        is_loopback(a)
            || crate::netdev::interfaces(ns)
                .iter()
                .any(|i| i.addrs.iter().any(|(_, x, _)| x == a))
    };
    let src = if p.src.len() == ip.len() {
        p.src.clone()
    } else {
        vec![0; ip.len()]
    };
    let addr_type = |a: &[u8]| crate::nft::addr_type(a, local);
    // Index and name of the output interface: by name when one is given (on FORWARD), lo for
    // local addresses, otherwise by route
    let oif = || -> (u32, Vec<u8>) {
        let found = |f: &dyn Fn(&tuxport_hal::net::Interface) -> bool| {
            crate::netdev::interfaces(ns)
                .into_iter()
                .find(|i| f(i))
                .map(|i| (i.index, i.name.into_bytes()))
        };
        if let Some(o) = &oif {
            return found(&|i| i.name.as_bytes() == &o[..]).unwrap_or((0, o.clone()));
        }
        if local(&ip) {
            return found(&|i| i.name == "lo").unwrap_or((1, b"lo".to_vec()));
        }
        let index = v4
            .then(|| crate::netdev::route_v4(ns, ip[..4].try_into().unwrap()))
            .flatten()
            .map(|e| e.index);
        found(&|i| Some(i.index) == index).unwrap_or_default()
    };
    let packet = crate::nft::Packet {
        src: &src,
        dst: &ip,
        proto: p.proto,
        sport: p.sport,
        dport: u16::from_be_bytes([dst[2], dst[3]]),
        uid: p.uid,
        gid: p.gid,
        addr_type: &addr_type,
        oif: &oif,
        hook: p.hook,
        iif: p.iif.clone(),
        ct_state: p.ct_state,
        mark: p.mark,
    };
    let nft = ns.nft.borrow();
    if filter {
        nft.filter(&packet)
    } else {
        nft.nat(&packet)
    }
}

/// Which kind of packet a sockaddr is judged as: (address, is IPv4). IPv4-mapped IPv6 addresses
/// count as IPv4 packets.
fn nat_ip(dst: &[u8]) -> Option<(Vec<u8>, bool)> {
    match (family(dst), ipv4_of(dst)) {
        (Some(AF_INET | AF_INET6), Some(x)) => Some((x.to_vec(), true)),
        (Some(AF_INET6), None) if dst.len() >= 24 => Some((dst[8..24].to_vec(), false)),
        _ => None,
    }
}

fn is_loopback(a: &[u8]) -> bool {
    (a.len() == 4 && a[0] == 127) || a == std::net::Ipv6Addr::LOCALHOST.octets()
}

/// The interface in `t` facing IPv4 address `src` (the one packets from `src` come in on): the
/// one with the directly connected subnet, otherwise the default route's output interface.
fn iif_facing(t: &NetNs, src: [u8; 4]) -> crate::nft::Iif {
    let Some(e) = crate::netdev::route_v4(t, src) else {
        return Default::default();
    };
    let name = crate::netdev::interfaces(t)
        .into_iter()
        .find(|i| i.index == e.index)
        .map(|i| i.name.into_bytes())
        .unwrap_or_default();
    crate::nft::Iif {
        index: e.index,
        name,
        addr: crate::netdev::first_v4(t, e.index)
            .map(|a| a.to_vec())
            .unwrap_or_default(),
    }
}

/// The host's first non-loopback interface with an IPv4 address: the input interface of packets
/// from outside and the output interface of packets going outside.
fn outer_iface(init: &NetNs) -> Option<(crate::nft::Iif, [u8; 4])> {
    crate::netdev::interfaces(init).into_iter().find_map(|i| {
        let a = i
            .addrs
            .iter()
            .find(|(f, a, _)| *f == AF_INET && a.len() == 4 && a[0] != 127)?;
        let addr: [u8; 4] = a.1[..].try_into().ok()?;
        Some((
            crate::nft::Iif {
                index: i.index,
                name: i.name.clone().into_bytes(),
                addr: addr.to_vec(),
            },
            addr,
        ))
    })
}

// ---- Address helpers ----

/// The namespace a host address belongs to (the low-port mapping registry goes by it,
/// [`crate::portmap`]): another namespace's block gives that namespace, 127.255.x.y the initial
/// one; anything else None (the socket's own namespace is used).
pub(crate) fn owner_of_host(w: &World, addr: &[u8]) -> Option<Rc<NetNs>> {
    let x = ipv4_of(addr).filter(|x| x[0] == 127)?;
    if x[1] == INIT_BLOCK {
        return Some(w.init().clone());
    }
    w.by_block(x[1], x[2]).cloned()
}

/// Whether it is an IPv4 or IPv6 address.
pub(crate) fn is_inet(addr: &[u8]) -> bool {
    matches!(family(addr), Some(AF_INET | AF_INET6))
}

/// Whether the guest bound a wildcard address (0.0.0.0, ::, ::ffff:0.0.0.0).
pub(crate) fn is_any(addr: &[u8]) -> bool {
    match family(addr) {
        Some(AF_INET) => addr.get(4..8) == Some(&[0; 4]),
        Some(AF_INET6) => addr
            .get(8..24)
            .is_some_and(|ip| ip == [0; 16] || mapped(ip) == Some([0; 4])),
        _ => false,
    }
}

/// The source address when `me` sends to `ip` (`inet_select_addr`): the route's preferred
/// source, otherwise an address on the outgoing interface, otherwise `me`'s first virtual
/// address.
fn source(me: &NetNs, ip: [u8; 4]) -> Option<[u8; 4]> {
    let r = crate::netdev::route_v4(me, ip);
    r.as_ref()
        .and_then(|r| r.prefsrc.as_deref())
        .and_then(|p| p.try_into().ok())
        .or_else(|| r.and_then(|r| crate::netdev::first_v4(me, r.index)))
        .or_else(|| crate::netdev::local_v4(me).first().map(|(a, _)| *a))
}

/// The directly connected subnets in `me` (routes on virtual devices with no gateway, not /32).
fn connected(me: &NetNs) -> Vec<([u8; 4], u8)> {
    crate::netdev::entries(me)
        .into_iter()
        .filter(|e| e.family == AF_INET && e.table == 254 && e.gateway.is_none() && e.prefix < 32)
        .filter_map(|e| Some((<[u8; 4]>::try_from(&e.dst[..]).ok()?, e.prefix)))
        .collect()
}

fn covers(net: [u8; 4], prefix: u8, ip: [u8; 4]) -> bool {
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    u32::from_be_bytes(net) & mask == u32::from_be_bytes(ip) & mask
}

/// The IPv4 address in a sockaddr (for an IPv4-mapped IPv6 address, the IPv4 part).
fn ipv4_of(addr: &[u8]) -> Option<[u8; 4]> {
    match family(addr) {
        Some(AF_INET) if addr.len() >= 8 => addr[4..8].try_into().ok(),
        Some(AF_INET6) if addr.len() >= 24 => mapped(&addr[8..24]),
        _ => None,
    }
}

/// Replaces the IPv4 address in a sockaddr with `ip` (written IPv4-mapped for AF_INET6, with the
/// scope id cleared).
fn with_ipv4(addr: &[u8], ip: [u8; 4]) -> Vec<u8> {
    let mut out = addr.to_vec();
    if family(addr) == Some(AF_INET) {
        out[4..8].copy_from_slice(&ip);
    } else {
        out[8..18].fill(0);
        out[18..20].copy_from_slice(&[0xff, 0xff]);
        out[20..24].copy_from_slice(&ip);
        if let Some(s) = out.get_mut(24..28) {
            s.fill(0);
        }
    }
    out
}

fn family(addr: &[u8]) -> Option<i32> {
    addr.get(..2)
        .map(|b| i32::from(u16::from_le_bytes([b[0], b[1]])))
}

fn mapped(ip: &[u8]) -> Option<[u8; 4]> {
    (ip[..10] == [0; 10] && ip[10..12] == [0xff, 0xff]).then(|| ip[12..16].try_into().unwrap())
}

// ---- lo configuration ----

/// A netlink request that modifies lo (the whole message): `RTM_NEWADDR`, `RTM_DELADDR`,
/// `RTM_NEWLINK` (`RTM_SETLINK`). None means it is none of these; Some holds the error code (0 is
/// success). Needs `CAP_NET_ADMIN` in the namespace's owner (EPERM; Linux's
/// `netlink_net_capable`).
pub(crate) fn configure(ns: &NetNs, cred: &Cred, req: &[u8]) -> Option<i32> {
    const RTM_NEWLINK: u16 = 16;
    const RTM_SETLINK: u16 = 19;
    const RTM_NEWADDR: u16 = 20;
    const RTM_DELADDR: u16 = 21;
    const NLM_F_EXCL: u16 = 0x200;
    const IFA_ADDRESS: u16 = 1;
    const IFA_LOCAL: u16 = 2;
    let ty = u16::from_le_bytes([req[4], req[5]]);
    if !matches!(ty, RTM_NEWLINK | RTM_SETLINK | RTM_NEWADDR | RTM_DELADDR) {
        return None;
    }
    if !cred.ns_capable(&ns.owner, cap::NET_ADMIN) {
        return Some(Errno::EPERM.0);
    }
    let body = &req[16..];
    let mut lo = ns.lo.borrow_mut();
    Some(match ty {
        // struct ifinfomsg: address family, type, index, flags, flags to change
        RTM_NEWLINK | RTM_SETLINK => {
            let Some(b) = body.get(..16) else {
                return Some(Errno::EINVAL.0);
            };
            let index = i32::from_le_bytes(b[4..8].try_into().unwrap());
            let flags = u32::from_le_bytes(b[8..12].try_into().unwrap());
            let change = u32::from_le_bytes(b[12..16].try_into().unwrap());
            if index != LO_INDEX as i32 {
                return Some(Errno::ENODEV.0);
            }
            if change & IFF_UP != 0 || change == 0 {
                set_up(&mut lo, flags & IFF_UP != 0);
            }
            0
        }
        // struct ifaddrmsg: address family, prefix length, flags, scope, index, followed by
        // IFA_LOCAL, IFA_ADDRESS
        _ => {
            let Some(b) = body.get(..8) else {
                return Some(Errno::EINVAL.0);
            };
            let (family, prefix) = (i32::from(b[0]), b[1]);
            let index = u32::from_le_bytes(b[4..8].try_into().unwrap());
            if index != LO_INDEX {
                return Some(Errno::ENODEV.0);
            }
            let attrs = crate::route::attrs(&body[8..]);
            let addr = attrs
                .iter()
                .find(|(t, _)| *t == IFA_LOCAL)
                .or_else(|| attrs.iter().find(|(t, _)| *t == IFA_ADDRESS))
                .map(|(_, d)| d.to_vec());
            let excl = u16::from_le_bytes([req[6], req[7]]) & NLM_F_EXCL != 0;
            match (family, addr) {
                (AF_INET, Some(a)) if a.len() == 4 && prefix <= 32 => {
                    let a: [u8; 4] = a.try_into().unwrap();
                    let at = lo.v4.iter().position(|(x, _)| *x == a);
                    match (ty, at) {
                        (RTM_NEWADDR, Some(_)) if excl => Errno::EEXIST.0,
                        (RTM_NEWADDR, Some(i)) => {
                            lo.v4[i].1 = prefix;
                            0
                        }
                        (RTM_NEWADDR, None) => {
                            lo.v4.push((a, prefix));
                            0
                        }
                        (_, Some(i)) => {
                            lo.v4.remove(i);
                            0
                        }
                        (_, None) => Errno::EADDRNOTAVAIL.0,
                    }
                }
                (AF_INET6, Some(a)) if a == LOOPBACK6 => match (ty, lo.v6) {
                    (RTM_NEWADDR, true) if excl => Errno::EEXIST.0,
                    (RTM_NEWADDR, _) => {
                        lo.v6 = true;
                        0
                    }
                    (_, true) => {
                        lo.v6 = false;
                        0
                    }
                    (_, false) => Errno::EADDRNOTAVAIL.0,
                },
                // Other IPv6 addresses cannot reach the host, so they cannot be configured
                (AF_INET6, Some(_)) => Errno::EOPNOTSUPP.0,
                (AF_INET | AF_INET6, _) => Errno::EINVAL.0,
                _ => Errno::EAFNOSUPPORT.0,
            }
        }
    })
}

/// Interface ioctls (`struct ifreq`, 40 bytes, the first 16 the name): a new namespace has only
/// lo. None means it is not handled here.
pub(crate) fn interface_ioctl(
    ns: &NetNs,
    cred: &Cred,
    request: u32,
    ifr: &mut [u8; 40],
) -> Option<Result<(), Errno>> {
    const SIOCGIFNAME: u32 = 0x8910;
    const SIOCGIFFLAGS: u32 = 0x8913;
    const SIOCSIFFLAGS: u32 = 0x8914;
    const SIOCGIFADDR: u32 = 0x8915;
    const SIOCGIFNETMASK: u32 = 0x891b;
    const SIOCGIFMTU: u32 = 0x8921;
    const SIOCSIFMTU: u32 = 0x8922;
    const SIOCGIFHWADDR: u32 = 0x8927;
    const SIOCGIFINDEX: u32 = 0x8933;
    const SIOCGIFTXQLEN: u32 = 0x8942;
    const ARPHRD_LOOPBACK: u16 = 772;
    let name = ifr[..16].split(|&c| c == 0).next().unwrap_or_default();
    if request == SIOCGIFNAME {
        let index = i32::from_le_bytes(ifr[16..20].try_into().unwrap());
        if index != LO_INDEX as i32 {
            return Some(Err(Errno::ENODEV));
        }
        ifr[..16].fill(0);
        ifr[..2].copy_from_slice(b"lo");
        return Some(Ok(()));
    }
    let known = [
        SIOCGIFFLAGS,
        SIOCSIFFLAGS,
        SIOCSIFMTU,
        SIOCGIFADDR,
        SIOCGIFNETMASK,
        SIOCGIFMTU,
        SIOCGIFHWADDR,
        SIOCGIFINDEX,
        SIOCGIFTXQLEN,
    ];
    if !known.contains(&request) {
        return None;
    }
    if name != b"lo" {
        return Some(Err(Errno::ENODEV));
    }
    let lo_if = lo_interface(ns);
    let data = &mut ifr[16..];
    if matches!(request, SIOCSIFFLAGS | SIOCSIFMTU) {
        if !cred.ns_capable(&ns.owner, cap::NET_ADMIN) {
            return Some(Err(Errno::EPERM));
        }
        let mut lo = ns.lo.borrow_mut();
        if request == SIOCSIFFLAGS {
            let flags = u16::from_le_bytes([data[0], data[1]]);
            set_up(&mut lo, u32::from(flags) & IFF_UP != 0);
        } else {
            // As with Linux's dev_set_mtu, not below the IPv4 minimum of 68 (loopback has no
            // upper bound)
            let mtu = i32::from_le_bytes(data[..4].try_into().unwrap());
            if mtu < 68 {
                return Some(Err(Errno::EINVAL));
            }
            lo.mtu = Some(mtu as u32);
        }
        return Some(Ok(()));
    }
    data.fill(0);
    let sockaddr = |data: &mut [u8], a: [u8; 4]| {
        data[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        data[4..8].copy_from_slice(&a);
    };
    match request {
        SIOCGIFFLAGS => data[..2].copy_from_slice(&(lo_if.flags as u16).to_le_bytes()),
        SIOCGIFADDR | SIOCGIFNETMASK => {
            let Some(&(a, len)) = ns.lo.borrow().v4.first() else {
                return Some(Err(Errno::EADDRNOTAVAIL));
            };
            let mask = u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0);
            let value = if request == SIOCGIFADDR {
                a
            } else {
                mask.to_be_bytes()
            };
            sockaddr(data, value);
        }
        SIOCGIFMTU => data[..4].copy_from_slice(&(lo_if.mtu as i32).to_le_bytes()),
        SIOCGIFHWADDR => data[..2].copy_from_slice(&ARPHRD_LOOPBACK.to_le_bytes()),
        SIOCGIFINDEX => data[..4].copy_from_slice(&(LO_INDEX as i32).to_le_bytes()),
        _ => data[..4].copy_from_slice(&1000i32.to_le_bytes()),
    }
    Some(Ok(()))
}

/// The lo interface (for netlink and procfs answers).
pub(crate) fn lo_interface(ns: &NetNs) -> Interface {
    let lo = ns.lo.borrow();
    let mut addrs: Vec<(i32, Vec<u8>, u8)> = lo
        .v4
        .iter()
        .map(|(a, len)| (AF_INET, a.to_vec(), *len))
        .collect();
    if lo.v6 {
        addrs.push((AF_INET6, LOOPBACK6.to_vec(), 128));
    }
    Interface {
        name: "lo".into(),
        index: LO_INDEX,
        flags: if lo.up {
            IFF_UP | IFF_LOOPBACK | IFF_RUNNING | IFF_LOWER_UP
        } else {
            IFF_LOOPBACK
        },
        mtu: lo.mtu.unwrap_or(65536),
        mac: None,
        addrs,
    }
}

/// Brings lo up or down (`RTM_NEWLINK`, `SIOCSIFFLAGS`): bringing it up adds 127.0.0.1/8 when
/// it has no IPv4 address (Linux's `inetdev_event`), and adds ::1 (addrconf); bringing it down
/// removes ::1 with it.
pub(crate) fn set_up(lo: &mut Loopback, up: bool) {
    if up && !lo.up {
        if lo.v4.is_empty() {
            lo.v4.push(([127, 0, 0, 1], 8));
        }
        lo.v6 = true;
    } else if !up {
        lo.v6 = false;
    }
    lo.up = up;
}

/// `SIOCGIFCONF`: lo while it is up with an IPv4 address (Linux's `inet_gifconf` lists only IPv4
/// addresses).
pub(crate) fn ifconf(ns: &NetNs) -> Vec<u8> {
    let lo = ns.lo.borrow();
    let mut out = Vec::new();
    for (a, _) in lo.v4.iter().filter(|_| lo.up) {
        let mut r = [0u8; 40];
        r[..2].copy_from_slice(b"lo");
        r[16..18].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        r[20..24].copy_from_slice(a);
        out.extend_from_slice(&r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::RefCell;

    fn ns() -> NetNs {
        NetNs {
            id: 1,
            owner: crate::cred::UserNs::init(),
            host_net: Some([1, 2]),
            lo: Default::default(),
            devs: Default::default(),
            nft: Default::default(),
            listeners: Default::default(),
        }
    }

    fn v4(ip: [u8; 4], port: u16) -> Vec<u8> {
        let mut s = vec![0u8; 16];
        s[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        s[2..4].copy_from_slice(&port.to_be_bytes());
        s[4..8].copy_from_slice(&ip);
        s
    }

    fn v6(ip: [u8; 16], port: u16) -> Vec<u8> {
        let mut s = vec![0u8; 28];
        s[..2].copy_from_slice(&(AF_INET6 as u16).to_le_bytes());
        s[2..4].copy_from_slice(&port.to_be_bytes());
        s[8..24].copy_from_slice(&ip);
        s
    }

    fn up(ns: &NetNs) {
        set_up(&mut ns.lo.borrow_mut(), true);
    }

    /// Connections forwarded in from outside (127.254.x.y, crate::forward): the peer is reported
    /// as the client's real address and port; unchanged when the host port does not match.
    #[test]
    fn outside_peers_show_the_real_client() {
        let n = Rc::new(ns());
        let w = World {
            all: vec![crate::nsproxy::NsProxy::init().net, n.clone()],
        };
        crate::forward::record_outside_for_test(
            [127, 254, 200, 7],
            [192, 168, 1, 50],
            51000,
            51000,
        );
        let got = from_host(&w, &n, &v4([127, 254, 200, 7], 51000), Role::Peer, false);
        assert_eq!(got, v4([192, 168, 1, 50], 51000));
        let other = from_host(&w, &n, &v4([127, 254, 200, 7], 51001), Role::Peer, false);
        assert_ne!(other[4..8], [192, 168, 1, 50]);
    }

    /// Listener records: wildcard addresses go by address family and `IPV6_V6ONLY`; those bound
    /// to an address accept only what is sent to it.
    #[test]
    fn listeners_match_like_linux() {
        let n = ns();
        let host = |x: u8| v4([127, 1, 2, x], 0);
        let mapped = |x: u8| {
            let mut a = [0u8; 16];
            a[10..12].copy_from_slice(&[0xff, 0xff]);
            a[12..].copy_from_slice(&[127, 1, 2, x]);
            v6(a, 0)
        };
        let eth0 = [10, 9, 0, 2];
        let a = add_listener(
            &n,
            &ListenAt {
                port: 7000,
                any: true,
                vaddr: None,
                host: &host(1),
                v6: false,
                v6only: false,
                udp: false,
            },
        );
        add_listener(
            &n,
            &ListenAt {
                port: 7001,
                any: true,
                vaddr: None,
                host: &mapped(1),
                v6: true,
                v6only: true,
                udp: false,
            },
        );
        add_listener(
            &n,
            &ListenAt {
                port: 7002,
                any: false,
                vaddr: None,
                host: &host(1),
                v6: false,
                v6only: false,
                udp: false,
            },
        );
        add_listener(
            &n,
            &ListenAt {
                port: 7003,
                any: false,
                vaddr: Some(eth0),
                host: &host(1),
                v6: false,
                v6only: false,
                udp: false,
            },
        );
        add_listener(
            &n,
            &ListenAt {
                port: 7004,
                any: false,
                vaddr: None,
                host: &mapped(1),
                v6: true,
                v6only: false,
                udp: false,
            },
        );
        // 0.0.0.0: any IPv4 works, pure IPv6 does not
        assert!(listening(&n, &v4(eth0, 7000), false));
        assert!(listening(&n, &v4([127, 0, 0, 1], 7000), false));
        assert!(!listening(&n, &v6(LOOPBACK6, 7000), false));
        // [::] with IPV6_V6ONLY: only IPv6
        assert!(listening(&n, &v6(LOOPBACK6, 7001), false));
        assert!(!listening(&n, &v4(eth0, 7001), false));
        // 127.0.0.1, eth0's address and ::1 accept only what is sent to them
        assert!(listening(&n, &v4([127, 0, 0, 1], 7002), false));
        assert!(!listening(&n, &v4(eth0, 7002), false));
        assert!(listening(&n, &v4(eth0, 7003), false));
        assert!(!listening(&n, &v4([127, 0, 0, 1], 7003), false));
        assert!(listening(&n, &v6(LOOPBACK6, 7004), false));
        assert!(!listening(&n, &v4([127, 0, 0, 1], 7004), false));
        // UDP and TCP are registered separately
        add_listener(
            &n,
            &ListenAt {
                port: 7005,
                any: true,
                vaddr: None,
                host: &host(1),
                v6: false,
                v6only: false,
                udp: true,
            },
        );
        assert!(listening(&n, &v4(eth0, 7005), true));
        assert!(!listening(&n, &v4(eth0, 7005), false));
        assert!(!listening(&n, &v4(eth0, 7000), true));
        // A port nobody listens on; and none after removal either
        assert!(!listening(&n, &v4(eth0, 7999), false));
        n.listeners.borrow_mut().retain(|l| l.id != a.get());
        assert!(!listening(&n, &v4(eth0, 7000), false));
    }

    /// Translation in a world of only the initial namespace and `ns` (addresses only).
    fn th(ns: &Rc<NetNs>, addr: &[u8], usage: Use, icmp6: bool) -> Result<Vec<u8>, Errno> {
        let w = World {
            all: vec![crate::nsproxy::NsProxy::init().net, ns.clone()],
        };
        to_host(&w, ns, addr, usage, icmp6).map(|m| m.addr)
    }

    fn fh(ns: &Rc<NetNs>, addr: &[u8], any: bool, icmp6: bool) -> Vec<u8> {
        let w = World {
            all: vec![crate::nsproxy::NsProxy::init().net, ns.clone()],
        };
        let role = if any {
            Role::Local {
                any: true,
                vaddr: None,
                peer: None,
            }
        } else {
            Role::Peer
        };
        from_host(&w, ns, addr, role, icmp6)
    }

    #[test]
    fn loopback_addresses_map_to_the_namespace_range() {
        let ns = Rc::new(ns());
        // lo still down: the wildcard address and 127.0.0.1 can be bound, but neither 127.0.0.1
        // nor ::1 can be reached; outside addresses are never reachable
        assert_eq!(
            th(&ns, &v4([0; 4], 80), Use::Bind, false).unwrap(),
            v4([127, 1, 2, 1], 80)
        );
        assert_eq!(
            th(&ns, &v4([127, 0, 0, 1], 80), Use::Bind, false).unwrap(),
            v4([127, 1, 2, 1], 80)
        );
        assert_eq!(
            th(&ns, &v4([127, 0, 0, 1], 80), Use::Reach, false),
            Err(Errno::ENETUNREACH)
        );
        assert_eq!(
            th(&ns, &v6(LOOPBACK6, 80), Use::Reach, false),
            Err(Errno::EADDRNOTAVAIL)
        );
        up(&ns);
        assert_eq!(ns.lo.borrow().v4, [([127, 0, 0, 1], 8)]);
        assert_eq!(
            th(&ns, &v4([127, 0, 0, 1], 80), Use::Reach, false).unwrap(),
            v4([127, 1, 2, 1], 80)
        );
        assert_eq!(
            th(&ns, &v4([127, 0, 0, 9], 80), Use::Bind, false).unwrap(),
            v4([127, 1, 2, 9], 80)
        );
        assert_eq!(
            th(&ns, &v4([8, 8, 8, 8], 53), Use::Reach, false),
            Err(Errno::ENETUNREACH)
        );
        assert_eq!(
            th(&ns, &v4([127, 1, 0, 1], 80), Use::Bind, false),
            Err(Errno::EADDRNOTAVAIL)
        );
        // ::1 and :: are the IPv4-mapped 127.a.b.1, and back again ::1
        let mapped = {
            let mut ip = [0u8; 16];
            ip[10..12].copy_from_slice(&[0xff, 0xff]);
            ip[12..].copy_from_slice(&[127, 1, 2, 1]);
            ip
        };
        assert_eq!(
            th(&ns, &v6(LOOPBACK6, 80), Use::Reach, false).unwrap(),
            v6(mapped, 80)
        );
        assert_eq!(
            th(&ns, &v6([0; 16], 80), Use::Bind, false).unwrap(),
            v6(mapped, 80)
        );
        assert_eq!(fh(&ns, &v6(mapped, 80), false, false), v6(LOOPBACK6, 80));
        assert_eq!(fh(&ns, &v6(mapped, 80), true, false), v6([0; 16], 80));
        assert_eq!(
            fh(&ns, &v4([127, 1, 2, 7], 9), false, false),
            v4([127, 0, 0, 7], 9)
        );
        assert_eq!(fh(&ns, &v4([127, 1, 2, 1], 9), true, false), v4([0; 4], 9));
        let mut doc = [0u8; 16];
        doc[0] = 0x20;
        assert_eq!(
            th(&ns, &v6(doc, 80), Use::Reach, false),
            Err(Errno::ENETUNREACH)
        );
        // ICMPv6 sockets: ::1 is the host's ::1, the wildcard address is bound to it too, and
        // others are still unreachable
        assert_eq!(
            th(&ns, &v6(LOOPBACK6, 0), Use::Reach, true).unwrap(),
            v6(LOOPBACK6, 0)
        );
        assert_eq!(
            th(&ns, &v6([0; 16], 0), Use::Bind, true).unwrap(),
            v6(LOOPBACK6, 0)
        );
        assert_eq!(
            th(&ns, &v6(doc, 0), Use::Reach, true),
            Err(Errno::ENETUNREACH)
        );
        assert_eq!(fh(&ns, &v6(LOOPBACK6, 0), false, true), v6(LOOPBACK6, 0));
        assert_eq!(fh(&ns, &v6(LOOPBACK6, 0), true, true), v6([0; 16], 0));
        // The initial namespace does not translate
        let init = crate::nsproxy::NsProxy::init().net;
        assert_eq!(
            th(&init, &v4([8, 8, 8, 8], 53), Use::Reach, false).unwrap(),
            v4([8, 8, 8, 8], 53)
        );
        let _ = Rc::clone(&init);
    }

    #[test]
    fn virtual_addresses_map_across_namespaces() {
        let init = crate::nsproxy::NsProxy::init().net;
        let net = |b: u8| {
            Rc::new(NetNs {
                id: u64::from(b),
                owner: crate::cred::UserNs::init(),
                host_net: Some([7, b]),
                lo: Default::default(),
                devs: RefCell::new(crate::netdev::Devices::for_new_ns(&Default::default())),
                nft: Default::default(),
                listeners: Default::default(),
            })
        };
        let (a, b) = (net(1), net(2));
        let gw = [10, 50, 0, 1];
        crate::netdev::test_dev(&init, "br0", gw, 24, None);
        crate::netdev::test_dev(&a, "eth0", [10, 50, 0, 2], 24, Some(gw));
        crate::netdev::test_dev(&b, "eth0", [10, 50, 0, 3], 24, Some(gw));
        let w = World {
            all: vec![init.clone(), a.clone(), b.clone()],
        };
        let reach = |me: &Rc<NetNs>, ip| to_host(&w, me, &v4(ip, 80), Use::Reach, false);
        let target = |m: &Mapped| m.target.as_ref().map(|t| t.id);
        // Another container, the gateway, outside, and an address missing from a directly
        // connected subnet
        let m = reach(&a, [10, 50, 0, 3]).unwrap();
        assert_eq!(
            (m.addr.clone(), target(&m)),
            (v4([127, 7, 2, 1], 80), Some(2))
        );
        assert_eq!(m.source, Some(v4([127, 7, 1, 1], 0)));
        let m = reach(&a, gw).unwrap();
        assert_eq!(m.addr, v4([127, INIT_BLOCK, 0, 1], 80));
        let m = reach(&a, [8, 8, 8, 8]).unwrap();
        assert_eq!(
            (m.addr.clone(), m.target.is_none(), m.source),
            (v4([8, 8, 8, 8], 80), true, None)
        );
        assert_eq!(reach(&a, [10, 50, 0, 9]).err(), Some(Errno::EHOSTUNREACH));
        // Binding its own address: 127.a.b.1 on the host, recording the virtual address; others'
        // cannot be bound
        let m = to_host(&w, &a, &v4([10, 50, 0, 2], 80), Use::Bind, false).unwrap();
        assert_eq!(
            (m.addr, m.vaddr),
            (v4([127, 7, 1, 1], 80), Some([10, 50, 0, 2]))
        );
        assert_eq!(
            to_host(&w, &a, &v4([10, 50, 0, 3], 80), Use::Bind, false).err(),
            Some(Errno::EADDRNOTAVAIL)
        );
        // The initial namespace: connecting to a container first binds the host counterpart of
        // the gateway address, and binding its own address binds to that too
        let m = reach(&init, [10, 50, 0, 2]).unwrap();
        assert_eq!(m.addr, v4([127, 7, 1, 1], 80));
        assert_eq!(m.source, Some(v4([127, INIT_BLOCK, 0, 1], 0)));
        let m = to_host(&w, &init, &v4(gw, 53), Use::Bind, false).unwrap();
        assert_eq!(m.addr, v4([127, INIT_BLOCK, 0, 1], 53));
        assert_eq!(
            reach(&init, [8, 8, 8, 8]).unwrap().addr,
            v4([8, 8, 8, 8], 80)
        );
        // The other way: another namespace's block and 127.255.x.y translate back to virtual
        // addresses; the local end faces the other side
        let peer = |me: &NetNs, x| from_host(&w, me, &v4(x, 9), Role::Peer, false);
        assert_eq!(peer(&b, [127, 7, 1, 1]), v4([10, 50, 0, 2], 9));
        assert_eq!(peer(&a, [127, INIT_BLOCK, 0, 1]), v4(gw, 9));
        assert_eq!(peer(&init, [127, 7, 2, 1]), v4([10, 50, 0, 3], 9));
        assert_eq!(peer(&a, [8, 8, 4, 4]), v4([8, 8, 4, 4], 9));
        let local = |me: &NetNs, x, p| {
            let p = v4(p, 1);
            let role = Role::Local {
                any: false,
                vaddr: None,
                peer: Some(&p),
            };
            from_host(&w, me, &v4(x, 9), role, false)
        };
        assert_eq!(
            local(&a, [127, 7, 1, 1], [127, 7, 2, 1]),
            v4([10, 50, 0, 2], 9)
        );
        assert_eq!(
            local(&a, [192, 168, 1, 5], [8, 8, 8, 8]),
            v4([10, 50, 0, 2], 9)
        );
        // Without a default route, outside is unreachable
        let mut r = vec![AF_INET as u8, 0, 0, 0, 254, 0, 255, 0, 0, 0, 0, 0];
        crate::net::attr(&mut r, 5, &gw);
        let del = crate::net::message(25, 1, 1, 0, &r);
        let none = |_: u16, _: u32| -> Result<Rc<NetNs>, Errno> { Err(Errno::ESRCH) };
        crate::netdev::request(&a, &Cred::root(), &del, &none)
            .unwrap()
            .unwrap();
        assert_eq!(reach(&a, [8, 8, 8, 8]).err(), Some(Errno::ENETUNREACH));
    }

    #[test]
    fn netlink_configures_lo() {
        let ns = ns();
        let root = Cred::root();
        let msg = |ty: u16, flags: u16, body: &[u8]| {
            let mut m = crate::net::message(ty, flags | 1, 1, 0, body);
            m.truncate(16 + body.len());
            m
        };
        // bwrap's loopback_setup: add 127.0.0.1/8 first, then bring lo up
        let mut addr = vec![AF_INET as u8, 8, 0x80, 254, 1, 0, 0, 0];
        crate::net::attr(&mut addr, 2, &[127, 0, 0, 1]);
        crate::net::attr(&mut addr, 1, &[127, 0, 0, 1]);
        assert_eq!(configure(&ns, &root, &msg(20, 0x600, &addr)), Some(0));
        assert_eq!(
            configure(&ns, &root, &msg(20, 0x600, &addr)),
            Some(Errno::EEXIST.0)
        );
        let mut link = vec![0u8; 16];
        link[4..8].copy_from_slice(&1i32.to_le_bytes());
        link[8..12].copy_from_slice(&IFF_UP.to_le_bytes());
        link[12..16].copy_from_slice(&IFF_UP.to_le_bytes());
        assert_eq!(configure(&ns, &root, &msg(16, 0, &link)), Some(0));
        let lo = lo_interface(&ns);
        assert_eq!(lo.flags & IFF_UP, IFF_UP);
        assert_eq!(lo.addrs.len(), 2);
        // There are no other interfaces; without CAP_NET_ADMIN nothing can be configured
        link[4..8].copy_from_slice(&2i32.to_le_bytes());
        assert_eq!(
            configure(&ns, &root, &msg(16, 0, &link)),
            Some(Errno::ENODEV.0)
        );
        let mut nobody = Cred::root();
        nobody.cap_effective = 0;
        assert_eq!(
            configure(&ns, &nobody, &msg(16, 0, &link)),
            Some(Errno::EPERM.0)
        );
        assert_eq!(configure(&ns, &root, &msg(18, 0, &link)), None);
    }
}
