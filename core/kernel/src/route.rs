//! The routing table (the kernel/net map): netlink's `RTM_GETROUTE` (`ip route`, `ip route get`)
//! and `RTM_GETRULE` (`ip rule`), `/proc/net/route` and `/proc/net/ipv6_route`.
//!
//! Since Android 10 apps cannot read the host's `/proc/net`, and with targetSdk ≥ 30 they cannot
//! bind `NETLINK_ROUTE` either, so the host's routing table cannot be passed to the guest directly.
//! Instead two tables are assembled here the way Linux has them without custom rules:
//!
//! - The main table: the host's routes. On Android the app takes them from the default network's
//!   `LinkProperties` and hands them to [`crate::Machine::set_routes`]; on other hosts the host's
//!   own are read ([`tuxport_hal::net::routes`]). When neither exists they are synthesized from
//!   the interface addresses: one per subnet of each address, plus a default route without a
//!   gateway through the default interface (the interface of the local address the host chooses
//!   for sending to the internet). Routes without a gateway that are exactly the subnet of an
//!   address on the interface count as added by the kernel, as on Linux (proto kernel, with a
//!   preferred source address for IPv4); the others count as boot.
//! - The local table: synthesized from the interface addresses, the same as what Linux's
//!   `fib_add_ifaddr` and addrconf add: for IPv4, one local entry per address and one broadcast
//!   entry for the subnet (on the loopback interface the subnet itself is local too); for IPv6, one
//!   local entry per address, and one `ff00::/8` multicast entry per non-loopback interface with an
//!   IPv6 address.
//!
//! Entries are in the same order as Linux's dumps: IPv4 by destination address ascending, longer
//! prefixes first for the same address (the leaves of the fib trie); IPv6 in the post-order of the
//! fib6 tree (0 first at a fork, a subnet before the subnet containing it); the main table before
//! the local table. For `ip route get` the host chooses the source address
//! ([`tuxport_hal::net::source_for`]), the output interface is the interface of that address, and
//! the gateway comes from the longest prefix match in the main table; when the destination is a
//! local address it is local, through the loopback interface. The rules are only Linux's default
//! ones.

use std::cmp::Ordering;
use std::fmt::Write as _;

use tuxport_hal::net::{Interface, Route};

use crate::net::{AF_INET, AF_INET6};

pub(crate) const RT_TABLE_DEFAULT: u8 = 253;
pub(crate) const RT_TABLE_MAIN: u8 = 254;
pub(crate) const RT_TABLE_LOCAL: u8 = 255;
const RTN_UNICAST: u8 = 1;
const RTN_LOCAL: u8 = 2;
const RTN_BROADCAST: u8 = 3;
const RTN_MULTICAST: u8 = 5;
const RTPROT_KERNEL: u8 = 2;
const RTPROT_BOOT: u8 = 3;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RT_SCOPE_HOST: u8 = 254;
const RTA_DST: u16 = 1;
const RTA_SRC: u16 = 2;
const RTA_IIF: u16 = 3;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PRIORITY: u16 = 6;
const RTA_PREFSRC: u16 = 7;
const RTA_CACHEINFO: u16 = 12;
const RTA_TABLE: u16 = 15;
const RTA_PREF: u16 = 20;
const RTA_UID: u16 = 25;
/// rtm_flags: `ip route get`'s answer is an entry in the route cache; the matched route itself is
/// what is asked for.
const RTM_F_CLONED: u32 = 0x200;
const RTM_F_FIB_MATCH: u32 = 0x2000;
/// rt_flags of route-cache entries (`ip route get`'s `cache <local>` and so on).
const RTCF_BROADCAST: u32 = 0x1000_0000;
const RTCF_MULTICAST: u32 = 0x2000_0000;
const RTCF_LOCAL: u32 = 0x8000_0000;
const IFF_UP: u32 = 0x1;
const IFF_LOOPBACK: u32 = 0x8;
/// Default metrics of IPv6 routes: subnets the kernel adds for addresses, and routes without a
/// specified metric (Linux's `IP6_RT_PRIO_ADDRCONF`, `IP6_RT_PRIO_USER`).
const IP6_METRIC_ADDRCONF: u32 = 256;
const IP6_METRIC_USER: u32 = 1024;

/// One route in the routing table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub family: i32,
    pub table: u8,
    /// `RTN_*`.
    pub ty: u8,
    pub proto: u8,
    pub scope: u8,
    pub dst: Vec<u8>,
    pub prefix: u8,
    pub prefsrc: Option<Vec<u8>>,
    pub gateway: Option<Vec<u8>>,
    pub iface: String,
    pub index: u32,
    pub metric: u32,
    /// rtm_flags: `RTNH_F_LINKDOWN` when the next hop's interface has no carrier (routes on virtual
    /// devices).
    pub flags: u32,
    /// A host route (on the host's interfaces in the boot namespace): for `ip route get` the host
    /// chooses the source address. False for virtual devices ([`crate::netdev`]) and in new
    /// namespaces.
    pub host: bool,
}

/// The routes of the host's main table: the ones the host handed over
/// ([`crate::Machine::set_routes`]), otherwise the host's own read directly.
pub(crate) fn host_routes(k: &crate::kernel::Kernel) -> Option<Vec<Route>> {
    k.host_routes.clone().or_else(tuxport_hal::net::routes)
}

/// All routes of the main and local tables, in Linux's dump order: IPv4 main and local, then IPv6
/// main and local. `host` is the routes of the host's main table (module docs); when it is None
/// they are synthesized from the interface addresses.
pub(crate) fn tables(ifaces: &[Interface], host: Option<Vec<Route>>) -> Vec<Entry> {
    let entry = |family, table, ty, i: &Interface| Entry {
        family,
        table,
        ty,
        proto: RTPROT_KERNEL,
        scope: RT_SCOPE_UNIVERSE,
        dst: Vec::new(),
        prefix: 0,
        prefsrc: None,
        gateway: None,
        iface: i.name.clone(),
        index: i.index,
        metric: 0,
        flags: 0,
        host: true,
    };
    let mut main = Vec::new();
    match host {
        Some(routes) => {
            for r in routes {
                let i = ifaces.iter().find(|i| i.name == r.iface);
                // The subnet of an address on the interface: added by the kernel for the address
                let addr = i.filter(|_| r.gateway.is_none()).and_then(|i| {
                    i.addrs.iter().find(|(f, a, p)| {
                        *f == r.family && *p == r.prefix && network(a, *p) == r.dst
                    })
                });
                let v4 = r.family == AF_INET;
                main.push(Entry {
                    family: r.family,
                    table: RT_TABLE_MAIN,
                    ty: RTN_UNICAST,
                    proto: if addr.is_some() {
                        RTPROT_KERNEL
                    } else {
                        RTPROT_BOOT
                    },
                    scope: if v4 && r.gateway.is_none() {
                        RT_SCOPE_LINK
                    } else {
                        RT_SCOPE_UNIVERSE
                    },
                    dst: r.dst,
                    prefix: r.prefix,
                    prefsrc: addr.filter(|_| v4).map(|(_, a, _)| a.clone()),
                    gateway: r.gateway,
                    iface: r.iface,
                    index: i.map_or(0, |i| i.index),
                    metric: r.metric,
                    flags: 0,
                    host: true,
                });
            }
        }
        None => {
            for i in up(ifaces).filter(|i| i.flags & IFF_LOOPBACK == 0) {
                for (f, a, p) in &i.addrs {
                    let full = if *f == AF_INET { 32 } else { 128 };
                    if *p >= full {
                        continue;
                    }
                    let mut e = entry(*f, RT_TABLE_MAIN, RTN_UNICAST, i);
                    e.dst = network(a, *p);
                    e.prefix = *p;
                    if *f == AF_INET {
                        e.scope = RT_SCOPE_LINK;
                        e.prefsrc = Some(a.clone());
                    } else {
                        e.metric = IP6_METRIC_ADDRCONF;
                    }
                    main.push(e);
                }
            }
            for family in [AF_INET, AF_INET6] {
                if let Some(i) = default_iface(ifaces, family) {
                    let mut e = entry(family, RT_TABLE_MAIN, RTN_UNICAST, i);
                    e.proto = RTPROT_BOOT;
                    if family == AF_INET {
                        e.dst = vec![0; 4];
                        e.scope = RT_SCOPE_LINK;
                    } else {
                        e.dst = vec![0; 16];
                        e.metric = IP6_METRIC_USER;
                    }
                    main.push(e);
                }
            }
        }
    }
    let mut local = Vec::new();
    for i in up(ifaces) {
        let loopback = i.flags & IFF_LOOPBACK != 0;
        for (f, a, p) in &i.addrs {
            let mut e = entry(*f, RT_TABLE_LOCAL, RTN_LOCAL, i);
            e.dst = a.clone();
            if *f == AF_INET {
                e.prefix = 32;
                e.scope = RT_SCOPE_HOST;
                e.prefsrc = Some(a.clone());
                local.push(e.clone());
                if loopback && *p < 32 {
                    local.push(Entry {
                        dst: network(a, *p),
                        prefix: *p,
                        ..e.clone()
                    });
                }
                if *p < 31 {
                    local.push(Entry {
                        ty: RTN_BROADCAST,
                        scope: RT_SCOPE_LINK,
                        dst: broadcast(a, *p),
                        ..e
                    });
                }
            } else {
                e.prefix = 128;
                local.push(e);
            }
        }
        if !loopback && i.addrs.iter().any(|(f, ..)| *f == AF_INET6) {
            let mut e = entry(AF_INET6, RT_TABLE_LOCAL, RTN_MULTICAST, i);
            e.dst = network(&[0xff; 16], 8);
            e.prefix = 8;
            e.metric = IP6_METRIC_ADDRCONF;
            local.push(e);
        }
    }
    let mut all = Vec::new();
    for family in [AF_INET, AF_INET6] {
        for table in [&main, &local] {
            let mut part: Vec<Entry> = table
                .iter()
                .filter(|e| e.family == family)
                .cloned()
                .collect();
            part.sort_by(order);
            // Two addresses in the same subnet on the same interface: only one subnet and one
            // broadcast entry
            part.dedup_by(|a, b| {
                (a.ty, &a.dst, a.prefix, a.index) == (b.ty, &b.dst, b.prefix, b.index)
            });
            all.extend(part);
        }
    }
    all
}

fn up(ifaces: &[Interface]) -> impl Iterator<Item = &Interface> {
    ifaces.iter().filter(|i| i.flags & IFF_UP != 0)
}

/// The interface the host uses for sending to the internet ([`crate::traffic`]'s default
/// interface), or None when there is no route for this address family.
fn default_iface(ifaces: &[Interface], family: i32) -> Option<&Interface> {
    owner(ifaces, &crate::traffic::outgoing_source(family)?)
}

/// The broadcast address of an IPv4 subnet: all ones after the first `prefix` bits.
fn broadcast(addr: &[u8], prefix: u8) -> Vec<u8> {
    let mask = u32::MAX.checked_shr(prefix as u32).unwrap_or(0);
    (u32::from_be_bytes(addr.try_into().unwrap()) | mask)
        .to_be_bytes()
        .to_vec()
}

/// Merges two sets of routes (each in Linux's order): sorted by address family (IPv4 first),
/// table number and order within the table ([`order`]), keeping equal ones in their original
/// order (`a`'s before `b`'s).
pub(crate) fn merge(a: Vec<Entry>, b: Vec<Entry>) -> Vec<Entry> {
    let mut all = a;
    all.extend(b);
    all.sort_by(|x, y| {
        let fam = |e: &Entry| u8::from(e.family != AF_INET);
        fam(x)
            .cmp(&fam(y))
            .then(x.table.cmp(&y.table))
            .then_with(|| order(x, y))
    });
    all
}

/// The order of two routes of the same table in Linux's dumps (module docs).
fn order(a: &Entry, b: &Entry) -> Ordering {
    if a.family == AF_INET {
        let key = |e: &Entry| u32::from_be_bytes(e.dst[..4].try_into().unwrap());
        return key(a).cmp(&key(b)).then(b.prefix.cmp(&a.prefix));
    }
    let common = a.prefix.min(b.prefix) as usize;
    (0..common)
        .map(|n| bit(&a.dst, n).cmp(&bit(&b.dst, n)))
        .find(|o| o.is_ne())
        .unwrap_or_else(|| b.prefix.cmp(&a.prefix))
}

fn bit(addr: &[u8], n: usize) -> u8 {
    (addr[n / 8] >> (7 - n % 8)) & 1
}

// ---- netlink ----

/// One route in a dump (Linux's `fib_dump_info`, `rt6_fill_node`).
pub(crate) fn route_message(e: &Entry) -> Vec<u8> {
    let mut p = rtmsg(
        e.family, e.prefix, 0, e.table, e.proto, e.scope, e.ty, e.flags,
    );
    put(&mut p, RTA_TABLE, &u32::from(e.table).to_le_bytes());
    if e.prefix > 0 {
        put(&mut p, RTA_DST, &e.dst);
    }
    if e.family == AF_INET {
        if e.metric != 0 {
            put(&mut p, RTA_PRIORITY, &e.metric.to_le_bytes());
        }
        if let Some(src) = &e.prefsrc {
            put(&mut p, RTA_PREFSRC, src);
        }
    } else {
        put(&mut p, RTA_PRIORITY, &e.metric.to_le_bytes());
    }
    if let Some(gw) = &e.gateway {
        put(&mut p, RTA_GATEWAY, gw);
    }
    put(&mut p, RTA_OIF, &e.index.to_le_bytes());
    if e.family == AF_INET6 {
        put(&mut p, RTA_CACHEINFO, &cacheinfo(0));
        put(&mut p, RTA_PREF, &[0]);
    }
    p
}

/// `ip route get`: answers the single `RTM_GETROUTE` query `req` (the whole message, attributes
/// after the rtmsg); `uid` is the user ID of the requesting process. Returns the answer's content
/// (RTM_NEWROUTE) or an error number.
///
/// When the longest prefix match in the main table is a route on a virtual device
/// ([`crate::netdev`]), or this is not the boot namespace (`host_ok` is false), the choice follows
/// this namespace's table: the output is that route's interface, and the source address
/// is its preferred source, or when there is none an address on the output interface
/// (`inet_select_addr`).
pub(crate) fn get(
    req: &[u8],
    entries: &[Entry],
    ifaces: &[Interface],
    uid: u32,
    host_ok: bool,
) -> Result<Vec<u8>, i32> {
    const EINVAL: i32 = 22;
    const EOPNOTSUPP: i32 = 95;
    const ENETUNREACH: i32 = 101;
    let family = *req.get(16).ok_or(EINVAL)? as i32;
    let flags = u32::from_le_bytes(req.get(24..28).ok_or(EINVAL)?.try_into().unwrap());
    let len = match family {
        AF_INET => 4,
        AF_INET6 => 16,
        _ => return Err(EOPNOTSUPP),
    };
    let (mut dst, mut src) = (vec![0u8; len], None);
    for (ty, data) in attrs(&req[28..]) {
        match ty {
            RTA_DST if data.len() == len => dst = data.to_vec(),
            RTA_SRC if data.len() == len => src = Some(data.to_vec()),
            // Routing as for a received packet (`iif`) is not implemented
            RTA_IIF => return Err(EOPNOTSUPP),
            _ => {}
        }
    }
    let of_family = || entries.iter().filter(|e| e.family == family);
    let local_hit = longest_match(of_family().filter(|e| e.table == RT_TABLE_LOCAL), &dst)
        .filter(|e| e.ty != RTN_MULTICAST || family == AF_INET);
    let lo = ifaces.iter().find(|i| i.flags & IFF_LOOPBACK != 0);
    // The matched route, output interface, source address, and the cache entry's rt_flags
    let (route, oif, saddr, rt_flags) = match local_hit {
        Some(e) if e.ty == RTN_LOCAL => {
            // A local address: through the loopback interface, with the local route's preferred
            // source as the source (the address itself; for a secondary, the primary of the same
            // subnet; for other addresses in 127.0.0.0/8, the loopback one)
            let saddr = e
                .prefsrc
                .clone()
                .or_else(|| owner(ifaces, &dst).map(|_| dst.clone()));
            (
                e.clone(),
                lo.map_or(e.index, |i| i.index),
                saddr,
                RTCF_LOCAL,
            )
        }
        Some(e) if e.ty == RTN_BROADCAST => (
            e.clone(),
            e.index,
            e.prefsrc.clone(),
            RTCF_BROADCAST | RTCF_LOCAL,
        ),
        _ if !host_ok
            || longest_match(of_family().filter(|e| e.table == RT_TABLE_MAIN), &dst)
                .is_some_and(|e| !e.host) =>
        {
            let route = longest_match(of_family().filter(|e| e.table == RT_TABLE_MAIN), &dst)
                .ok_or(ENETUNREACH)?
                .clone();
            let saddr = match &src {
                Some(s) => s.clone(),
                None => route
                    .prefsrc
                    .clone()
                    .or_else(|| select_source(ifaces, &route, &dst))
                    .ok_or(ENETUNREACH)?,
            };
            let index = route.index;
            (route, index, Some(saddr), 0)
        }
        _ => {
            let saddr = match &src {
                Some(s) => s.clone(),
                None => tuxport_hal::net::source_for(&dst)?,
            };
            let out = owner(ifaces, &saddr);
            let matched = longest_match(
                of_family()
                    .filter(|e| e.table == RT_TABLE_MAIN && out.is_none_or(|i| i.index == e.index)),
                &dst,
            )
            .or_else(|| longest_match(of_family().filter(|e| e.table == RT_TABLE_MAIN), &dst));
            // If the host can choose a source address there is a way out: when the main table has
            // no matching route (Android's 464xlat: LinkProperties omits the stacked link's
            // routes), treat it as through that address's interface, without a gateway
            let route = match (matched, out) {
                (Some(r), _) => r.clone(),
                (None, Some(i)) => Entry {
                    family,
                    table: RT_TABLE_MAIN,
                    ty: RTN_UNICAST,
                    proto: RTPROT_BOOT,
                    scope: if family == AF_INET {
                        RT_SCOPE_LINK
                    } else {
                        RT_SCOPE_UNIVERSE
                    },
                    dst: vec![0; len],
                    prefix: 0,
                    prefsrc: None,
                    gateway: None,
                    iface: i.name.clone(),
                    index: i.index,
                    metric: 0,
                    flags: 0,
                    host: true,
                },
                (None, None) => return Err(ENETUNREACH),
            };
            // Multicast: this machine is always in the all-hosts group (224.0.0.1); for other
            // groups whether it joined cannot be told
            let rt_flags = match dst[..] {
                [224, 0, 0, 1] if family == AF_INET => RTCF_MULTICAST | RTCF_LOCAL,
                [b, ..] if family == AF_INET && b & 0xf0 == 0xe0 => RTCF_MULTICAST,
                _ => 0,
            };
            let index = out.map_or(route.index, |i| i.index);
            (route, index, Some(saddr), rt_flags)
        }
    };
    if flags & RTM_F_FIB_MATCH != 0 {
        return Ok(route_message(&route));
    }
    let mut p;
    if family == AF_INET {
        let ty = match rt_flags {
            f if f & RTCF_MULTICAST != 0 => RTN_MULTICAST,
            f if f & RTCF_BROADCAST != 0 => RTN_BROADCAST,
            RTCF_LOCAL => RTN_LOCAL,
            _ => RTN_UNICAST,
        };
        let src_len = if src.is_some() { 32 } else { 0 };
        // Without custom rules the local table is merged into the main table, so every route found
        // counts as the main table's
        p = rtmsg(
            family,
            32,
            src_len,
            RT_TABLE_MAIN,
            0,
            RT_SCOPE_UNIVERSE,
            ty,
            rt_flags | RTM_F_CLONED,
        );
        put(&mut p, RTA_TABLE, &u32::from(RT_TABLE_MAIN).to_le_bytes());
        put(&mut p, RTA_DST, &dst);
        if let Some(s) = &src {
            put(&mut p, RTA_SRC, s);
        }
        put(&mut p, RTA_OIF, &oif.to_le_bytes());
        if let Some(s) = saddr.as_ref().filter(|s| Some(*s) != src.as_ref()) {
            put(&mut p, RTA_PREFSRC, s);
        }
        if let Some(gw) = route.gateway.as_ref().filter(|_| rt_flags == 0) {
            put(&mut p, RTA_GATEWAY, gw);
        }
        put(&mut p, RTA_UID, &uid.to_le_bytes());
        put(&mut p, RTA_CACHEINFO, &cacheinfo(2));
    } else {
        p = rtmsg(
            family,
            128,
            128,
            route.table,
            route.proto,
            RT_SCOPE_UNIVERSE,
            route.ty,
            0,
        );
        put(&mut p, RTA_TABLE, &u32::from(route.table).to_le_bytes());
        put(&mut p, RTA_DST, &dst);
        put(&mut p, RTA_SRC, src.as_deref().unwrap_or(&[0; 16]));
        if let Some(s) = &saddr {
            put(&mut p, RTA_PREFSRC, s);
        }
        put(&mut p, RTA_PRIORITY, &route.metric.to_le_bytes());
        if let Some(gw) = &route.gateway {
            put(&mut p, RTA_GATEWAY, gw);
        }
        put(&mut p, RTA_OIF, &oif.to_le_bytes());
        put(&mut p, RTA_CACHEINFO, &cacheinfo(2));
        put(&mut p, RTA_PREF, &[0]);
    }
    Ok(p)
}

/// The attributes in a message: (type, payload).
pub(crate) fn attrs(mut b: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    while b.len() >= 4 {
        let len = u16::from_le_bytes([b[0], b[1]]) as usize;
        if len < 4 || len > b.len() {
            break;
        }
        out.push((u16::from_le_bytes([b[2], b[3]]) & 0x3fff, &b[4..len]));
        b = &b[len.next_multiple_of(4).min(b.len())..];
    }
    out
}

/// The route in `entries` with the longest prefix containing `ip`; among equally long ones the
/// lowest metric (among equal ones the first).
fn longest_match<'a>(entries: impl Iterator<Item = &'a Entry>, ip: &[u8]) -> Option<&'a Entry> {
    entries
        .filter(|e| e.dst.len() == ip.len() && network(ip, e.prefix) == e.dst)
        .min_by_key(|e| (std::cmp::Reverse(e.prefix), e.metric))
}

/// The source address when there is no preferred source (`inet_select_addr`, with IPv6 roughly
/// following `ipv6_dev_get_saddr`): an address on the output interface in the same subnet as the
/// gateway (the destination when there is none), then another address on it, then one on another
/// interface; link-local addresses are not taken unless they are the only ones on the output
/// interface.
fn select_source(ifaces: &[Interface], route: &Entry, dst: &[u8]) -> Option<Vec<u8>> {
    let toward = route.gateway.as_deref().unwrap_or(dst);
    let family = route.family;
    let usable = |a: &Vec<u8>| !(family == AF_INET6 && a[0] == 0xfe && a[1] & 0xc0 == 0x80);
    let of = |i: &Interface| -> Vec<(Vec<u8>, u8)> {
        i.addrs
            .iter()
            .filter(|(f, ..)| *f == family)
            .map(|(_, a, p)| (a.clone(), *p))
            .collect()
    };
    let out = ifaces.iter().find(|i| i.index == route.index);
    if let Some(o) = out {
        let mine = of(o);
        if let Some((a, _)) = mine
            .iter()
            .find(|(a, p)| usable(a) && network(a, *p) == network(toward, *p))
        {
            return Some(a.clone());
        }
        if let Some((a, _)) = mine.iter().find(|(a, _)| usable(a)) {
            return Some(a.clone());
        }
    }
    let others = ifaces
        .iter()
        .filter(|i| i.flags & IFF_LOOPBACK == 0)
        .flat_map(of)
        .find(|(a, _)| usable(a))
        .map(|(a, _)| a);
    others.or_else(|| out.and_then(|o| of(o).into_iter().next().map(|(a, _)| a)))
}

/// struct rtmsg: family, dst_len, src_len, tos, table, protocol, scope, type, flags.
#[allow(clippy::too_many_arguments)]
fn rtmsg(
    family: i32,
    dst_len: u8,
    src_len: u8,
    table: u8,
    proto: u8,
    scope: u8,
    ty: u8,
    flags: u32,
) -> Vec<u8> {
    let mut p = vec![family as u8, dst_len, src_len, 0, table, proto, scope, ty];
    p.extend_from_slice(&flags.to_le_bytes());
    p
}

/// struct rta_cacheinfo: clntref, lastuse, expires, error, used, id, ts, tsage. In the answer of
/// `ip route get` clntref is the cache entry's reference count, set to 2, the usual value on Linux.
fn cacheinfo(clntref: u32) -> [u8; 32] {
    let mut c = [0u8; 32];
    c[..4].copy_from_slice(&clntref.to_le_bytes());
    c
}

const FRA_PRIORITY: u16 = 6;
const FRA_SUPPRESS_PREFIXLEN: u16 = 14;
const FRA_TABLE: u16 = 15;
const FRA_PROTOCOL: u16 = 21;
const FR_ACT_TO_TBL: u8 = 1;
/// The address families of the multicast routing rules (`RTNL_FAMILY_IPMR`, `RTNL_FAMILY_IP6MR`).
const RTNL_FAMILY_IPMR: i32 = 128;
const RTNL_FAMILY_IP6MR: i32 = 129;

/// `ip rule`: the default rules of address family `family` (0 for all), the same as Linux without
/// custom rules.
pub(crate) fn rule_messages(family: i32) -> Vec<Vec<u8>> {
    let rules: [(i32, u8, u32); 7] = [
        (AF_INET, RT_TABLE_LOCAL, 0),
        (AF_INET, RT_TABLE_MAIN, 32766),
        (AF_INET, RT_TABLE_DEFAULT, 32767),
        (RTNL_FAMILY_IPMR, RT_TABLE_DEFAULT, 32767),
        (RTNL_FAMILY_IP6MR, RT_TABLE_MAIN, 32767),
        (AF_INET6, RT_TABLE_LOCAL, 0),
        (AF_INET6, RT_TABLE_MAIN, 32766),
    ];
    rules
        .iter()
        .filter(|(f, ..)| family == 0 || *f == family)
        .map(|&(f, table, pref)| {
            // struct fib_rule_hdr: family, dst_len, src_len, tos, table, res1, res2, action, flags
            let mut p = vec![f as u8, 0, 0, 0, table, 0, 0, FR_ACT_TO_TBL, 0, 0, 0, 0];
            put(&mut p, FRA_TABLE, &u32::from(table).to_le_bytes());
            put(&mut p, FRA_SUPPRESS_PREFIXLEN, &(-1i32).to_le_bytes());
            put(&mut p, FRA_PROTOCOL, &[RTPROT_KERNEL]);
            if pref != 0 {
                put(&mut p, FRA_PRIORITY, &pref.to_le_bytes());
            }
            p
        })
        .collect()
}

fn put(b: &mut Vec<u8>, ty: u16, data: &[u8]) {
    crate::net::attr(b, ty, data);
}

// ---- Shared helpers ----

/// The first `prefix` bits of `addr`, with the rest cleared.
pub(crate) fn network(addr: &[u8], prefix: u8) -> Vec<u8> {
    addr.iter()
        .enumerate()
        .map(|(i, &b)| {
            let keep = (prefix as usize).saturating_sub(i * 8).min(8);
            if keep == 0 {
                0
            } else {
                b & (0xff << (8 - keep))
            }
        })
        .collect()
}

/// The interface that has address `ip`.
fn owner<'a>(ifaces: &'a [Interface], ip: &[u8]) -> Option<&'a Interface> {
    ifaces
        .iter()
        .find(|i| i.addrs.iter().any(|(_, a, _)| a == ip))
}

// ---- /proc/net ----

/// `/proc/net/route`: the IPv4 unicast routes of the main table (Linux's `fib_route_seq_show`),
/// with addresses printed as their in-memory bytes read as little-endian, and each line padded
/// with spaces to 127 characters.
pub(crate) fn proc_route(entries: &[Entry]) -> String {
    const RTF_UP: u32 = 0x1;
    const RTF_GATEWAY: u32 = 0x2;
    const RTF_HOST: u32 = 0x4;
    let mut lines = vec![
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT"
            .to_owned(),
    ];
    let word = |b: &[u8]| u32::from_le_bytes(b.try_into().unwrap());
    for e in entries
        .iter()
        .filter(|e| e.family == AF_INET && e.table == RT_TABLE_MAIN && e.ty == RTN_UNICAST)
    {
        let mask = u32::MAX.checked_shl(32 - e.prefix as u32).unwrap_or(0);
        let mut flags = RTF_UP;
        if e.gateway.is_some() {
            flags |= RTF_GATEWAY;
        }
        if e.prefix == 32 {
            flags |= RTF_HOST;
        }
        lines.push(format!(
            "{}\t{:08X}\t{:08X}\t{flags:04X}\t0\t0\t{}\t{:08X}\t0\t0\t0",
            e.iface,
            word(&e.dst),
            e.gateway.as_deref().map_or(0, word),
            e.metric,
            word(&mask.to_be_bytes()),
        ));
    }
    lines.iter().map(|l| format!("{l:<127}\n")).collect()
}

/// `/proc/net/ipv6_route`: the IPv6 tables (Linux's `ipv6_route_native_seq_show`); each table ends
/// with the null route at the tree root (when there is no `::/0` route). Reference counts mean
/// nothing, so they are all 1.
pub(crate) fn proc_ipv6_route(entries: &[Entry]) -> String {
    const RTF_UP: u32 = 0x1;
    const RTF_GATEWAY: u32 = 0x2;
    const RTF_REJECT: u32 = 0x200;
    const RTF_NONEXTHOP: u32 = 0x20_0000;
    const RTF_LOCAL: u32 = 0x8000_0000;
    let hex = |b: &[u8]| -> String { b.iter().map(|x| format!("{x:02x}")).collect() };
    let zero = hex(&[0; 16]);
    let mut s = String::new();
    for table in [RT_TABLE_MAIN, RT_TABLE_LOCAL] {
        let part: Vec<&Entry> = entries
            .iter()
            .filter(|e| e.family == AF_INET6 && e.table == table)
            .collect();
        for e in &part {
            let flags = match e.ty {
                RTN_LOCAL => RTF_LOCAL | RTF_NONEXTHOP | RTF_UP,
                _ if e.gateway.is_some() => RTF_GATEWAY | RTF_UP,
                _ => RTF_UP,
            };
            writeln!(
                s,
                "{} {:02x} {zero} 00 {} {:08x} 00000001 00000000 {flags:08x} {:>8}",
                hex(&e.dst),
                e.prefix,
                e.gateway.as_deref().map_or_else(|| zero.clone(), hex),
                e.metric,
                e.iface,
            )
            .unwrap();
        }
        if !part.iter().any(|e| e.prefix == 0) {
            writeln!(
                s,
                "{zero} 00 {zero} 00 {zero} ffffffff 00000001 00000000 {:08x} {:>8}",
                RTF_NONEXTHOP | RTF_REJECT,
                "lo"
            )
            .unwrap();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, index: u32, flags: u32, addrs: Vec<(i32, Vec<u8>, u8)>) -> Interface {
        Interface {
            name: name.into(),
            index,
            flags,
            mtu: 1500,
            mac: None,
            addrs,
        }
    }

    /// The two interfaces in a docker container: lo and eth0 with 172.17.0.2/16, and eth0 also has
    /// a link-local address.
    fn container() -> Vec<Interface> {
        let mut one = vec![0; 16];
        one[15] = 1;
        let mut ll = vec![0xfe, 0x80, 0, 0, 0, 0, 0, 0];
        ll.extend_from_slice(&[0, 0x42, 0xac, 0xff, 0xfe, 0x11, 0, 2]);
        vec![
            iface(
                "lo",
                1,
                0x49,
                vec![(AF_INET, vec![127, 0, 0, 1], 8), (AF_INET6, one, 128)],
            ),
            iface(
                "eth0",
                2,
                0x1043,
                vec![(AF_INET, vec![172, 17, 0, 2], 16), (AF_INET6, ll, 64)],
            ),
        ]
    }

    fn host_routes() -> Vec<Route> {
        vec![
            Route {
                family: AF_INET,
                dst: vec![0; 4],
                prefix: 0,
                gateway: Some(vec![172, 17, 0, 1]),
                iface: "eth0".into(),
                metric: 0,
            },
            Route {
                family: AF_INET,
                dst: vec![172, 17, 0, 0],
                prefix: 16,
                gateway: None,
                iface: "eth0".into(),
                metric: 0,
            },
            Route {
                family: AF_INET6,
                dst: network(&[0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 64),
                prefix: 64,
                gateway: None,
                iface: "eth0".into(),
                metric: 256,
            },
        ]
    }

    fn summary(e: &Entry) -> String {
        let ip = |b: &[u8]| -> String {
            match b.len() {
                4 => std::net::Ipv4Addr::from(<[u8; 4]>::try_from(b).unwrap()).to_string(),
                _ => std::net::Ipv6Addr::from(<[u8; 16]>::try_from(b).unwrap()).to_string(),
            }
        };
        format!(
            "{} {}/{} t{} n{} p{} s{} src {} via {} {} m{}",
            e.family,
            ip(&e.dst),
            e.prefix,
            e.table,
            e.ty,
            e.proto,
            e.scope,
            e.prefsrc.as_deref().map_or("-".into(), ip),
            e.gateway.as_deref().map_or("-".into(), ip),
            e.iface,
            e.metric
        )
    }

    /// When the host provides the main table: the subnets added by the kernel carry the preferred
    /// source address, the local table is synthesized from the addresses, and the order is the
    /// same as a dump in a docker container.
    #[test]
    fn builds_both_tables_in_dump_order() {
        let t: Vec<String> = tables(&container(), Some(host_routes()))
            .iter()
            .map(summary)
            .collect();
        assert_eq!(
            t,
            [
                "2 0.0.0.0/0 t254 n1 p3 s0 src - via 172.17.0.1 eth0 m0",
                "2 172.17.0.0/16 t254 n1 p2 s253 src 172.17.0.2 via - eth0 m0",
                "2 127.0.0.0/8 t255 n2 p2 s254 src 127.0.0.1 via - lo m0",
                "2 127.0.0.1/32 t255 n2 p2 s254 src 127.0.0.1 via - lo m0",
                "2 127.255.255.255/32 t255 n3 p2 s253 src 127.0.0.1 via - lo m0",
                "2 172.17.0.2/32 t255 n2 p2 s254 src 172.17.0.2 via - eth0 m0",
                "2 172.17.255.255/32 t255 n3 p2 s253 src 172.17.0.2 via - eth0 m0",
                "10 fe80::/64 t254 n1 p2 s0 src - via - eth0 m256",
                "10 ::1/128 t255 n2 p2 s0 src - via - lo m0",
                "10 fe80::42:acff:fe11:2/128 t255 n2 p2 s0 src - via - eth0 m0",
                "10 ff00::/8 t255 n5 p2 s0 src - via - eth0 m256",
            ]
        );
    }

    /// When the host provides no routes, the subnets are synthesized from the interface addresses;
    /// the default route depends on the host's network and is not checked here. Each interface
    /// with an IPv6 address has its own multicast route.
    #[test]
    fn synthesizes_connected_routes() {
        let mut ifaces = container();
        let ll = vec![
            0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0xb0, 0xb8, 0xdb, 0xff, 0xfe, 0x0f, 0xb7, 2,
        ];
        ifaces.push(iface("dummy0", 3, 0xc3, vec![(AF_INET6, ll, 64)]));
        ifaces[1].addrs.push((AF_INET, vec![172, 17, 0, 3], 16));
        let t = tables(&ifaces, None);
        let multicast: Vec<&str> = t
            .iter()
            .filter(|e| e.ty == RTN_MULTICAST)
            .map(|e| &e.iface[..])
            .collect();
        assert_eq!(multicast, ["eth0", "dummy0"]);
        let broadcasts = t.iter().filter(|e| e.ty == RTN_BROADCAST).count();
        assert_eq!(broadcasts, 2);
        let main: Vec<String> = t
            .iter()
            .filter(|e| e.table == RT_TABLE_MAIN && e.prefix > 0)
            .map(summary)
            .collect();
        assert_eq!(
            main,
            [
                "2 172.17.0.0/16 t254 n1 p2 s253 src 172.17.0.2 via - eth0 m0",
                "10 fe80::/64 t254 n1 p2 s0 src - via - eth0 m256",
                "10 fe80::/64 t254 n1 p2 s0 src - via - dummy0 m256",
            ]
        );
    }

    #[test]
    fn ipv6_entries_follow_the_fib6_walk() {
        let e = |dst: &[u8], prefix| Entry {
            family: AF_INET6,
            table: RT_TABLE_MAIN,
            ty: RTN_UNICAST,
            proto: RTPROT_KERNEL,
            scope: 0,
            dst: network(dst, prefix),
            prefix,
            prefsrc: None,
            gateway: None,
            iface: "eth0".into(),
            index: 2,
            metric: 0,
            flags: 0,
            host: true,
        };
        let mut v = [
            e(&[0; 16], 0),
            e(&[0xff; 16], 8),
            e(&[0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 64),
            e(
                &[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                64,
            ),
            e(
                &[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                128,
            ),
        ];
        v.sort_by(order);
        let got: Vec<(u8, u8)> = v.iter().map(|e| (e.dst[0], e.prefix)).collect();
        assert_eq!(
            got,
            [(0x20, 128), (0x20, 64), (0xfe, 64), (0xff, 8), (0, 0)]
        );
    }

    /// The format of `/proc/net/route` and `/proc/net/ipv6_route` is the same as in a docker
    /// container.
    #[test]
    fn formats_proc_files() {
        let t = tables(&container(), Some(host_routes()));
        let route = proc_route(&t);
        let lines: Vec<&str> = route.lines().map(str::trim_end).collect();
        assert_eq!(
            lines,
            [
                "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT",
                "eth0\t00000000\t010011AC\t0003\t0\t0\t0\t00000000\t0\t0\t0",
                "eth0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0",
            ]
        );
        assert!(route.lines().all(|l| l.len() == 127));
        let v6 = proc_ipv6_route(&t);
        let lines: Vec<&str> = v6.lines().collect();
        assert_eq!(lines.len(), 6);
        assert!(lines[0].starts_with("fe800000000000000000000000000000 40 "));
        assert!(lines[1].ends_with(" ffffffff 00000001 00000000 00200200       lo"));
        assert!(lines[2].ends_with(" 00000000 00000001 00000000 80200001       lo"));
        assert!(lines[4].starts_with("ff000000000000000000000000000000 08 "));
        assert_eq!(lines[5], lines[1]);
    }

    /// A single `ip route get` query: RTA_DST after the rtmsg.
    fn request(family: i32, dst: &[u8], flags: u32) -> Vec<u8> {
        let mut p = rtmsg(family, dst.len() as u8 * 8, 0, 0, 0, 0, 0, flags);
        put(&mut p, RTA_DST, dst);
        crate::net::message(26, 1, 1, 0, &p)
    }

    #[test]
    fn answers_local_destinations_like_linux() {
        let ifaces = container();
        let t = tables(&ifaces, Some(host_routes()));
        // A local address: the table is main (merged), type local, through lo, flags RTCF_LOCAL |
        // RTM_F_CLONED
        let r = get(&request(AF_INET, &[172, 17, 0, 2], 0), &t, &ifaces, 0, true).unwrap();
        assert_eq!(r[..12], [2, 32, 0, 0, 254, 0, 0, 2, 0, 2, 0, 0x80]);
        let a: Vec<(u16, Vec<u8>)> = attrs(&r[12..])
            .iter()
            .map(|(t, d)| (*t, d.to_vec()))
            .collect();
        let types: Vec<u16> = a.iter().map(|(t, _)| *t).collect();
        assert_eq!(
            types,
            [
                RTA_TABLE,
                RTA_DST,
                RTA_OIF,
                RTA_PREFSRC,
                RTA_UID,
                RTA_CACHEINFO
            ]
        );
        assert_eq!(a[2].1, 1u32.to_le_bytes());
        assert_eq!(a[3].1, [172, 17, 0, 2]);
        // Another address in the loopback subnet: the source address is the loopback one
        let r = get(&request(AF_INET, &[127, 0, 0, 9], 0), &t, &ifaces, 0, true).unwrap();
        assert!(attrs(&r[12..]).contains(&(RTA_PREFSRC, &[127, 0, 0, 1][..])));
        // The subnet's broadcast
        let r = get(
            &request(AF_INET, &[172, 17, 255, 255], 0),
            &t,
            &ifaces,
            0,
            true,
        )
        .unwrap();
        assert_eq!(r[7], RTN_BROADCAST);
        // IPv6 ::1: the local table, with RTA_SRC ::, metric and priority
        let mut one = [0u8; 16];
        one[15] = 1;
        let r = get(&request(AF_INET6, &one, 0), &t, &ifaces, 0, true).unwrap();
        assert_eq!(r[..12], [10, 128, 128, 0, 255, 2, 0, 2, 0, 0, 0, 0]);
        let types: Vec<u16> = attrs(&r[12..]).iter().map(|(t, _)| *t).collect();
        assert_eq!(
            types,
            [
                RTA_TABLE,
                RTA_DST,
                RTA_SRC,
                RTA_PREFSRC,
                RTA_PRIORITY,
                RTA_OIF,
                RTA_CACHEINFO,
                RTA_PREF
            ]
        );
        // The matched route itself
        let r = get(
            &request(AF_INET, &[172, 17, 0, 2], RTM_F_FIB_MATCH),
            &t,
            &ifaces,
            0,
            true,
        )
        .unwrap();
        assert_eq!(r, route_message(&t[5]));
        // No route in the main table, and the host can choose a source address (given by the
        // request here): through the interface of the source address
        let mut req = request(AF_INET, &[1, 1, 1, 1], 0);
        let mut from = Vec::new();
        put(&mut from, RTA_SRC, &[172, 17, 0, 2]);
        req.extend_from_slice(&from);
        let n = req.len() as u32;
        req[..4].copy_from_slice(&n.to_le_bytes());
        let empty = tables(&ifaces, Some(Vec::new()));
        let r = get(&req, &empty, &ifaces, 0, true).unwrap();
        let a = attrs(&r[12..]);
        assert!(a.contains(&(RTA_OIF, &2u32.to_le_bytes()[..])));
        assert!(
            !a.iter()
                .any(|(t, _)| *t == RTA_GATEWAY || *t == RTA_PREFSRC)
        );
        // Routing as for a received packet is not implemented
        let mut req = request(AF_INET, &[1, 1, 1, 1], 0);
        let mut iif = Vec::new();
        put(&mut iif, RTA_IIF, &2u32.to_le_bytes());
        req.extend_from_slice(&iif);
        let n = req.len() as u32;
        req[..4].copy_from_slice(&n.to_le_bytes());
        assert_eq!(get(&req, &t, &ifaces, 0, true), Err(95));
    }

    #[test]
    fn default_rules() {
        let v4 = rule_messages(AF_INET);
        assert_eq!(v4.len(), 3);
        assert_eq!(v4[0][..12], [2, 0, 0, 0, 255, 0, 0, 1, 0, 0, 0, 0]);
        // Rules with priority 0 carry no FRA_PRIORITY
        assert_eq!(v4[0].len(), 12 + 8 + 8 + 8);
        assert_eq!(v4[1].len(), 12 + 8 + 8 + 8 + 8);
        assert_eq!(rule_messages(AF_INET6).len(), 2);
        let all: Vec<u8> = rule_messages(0).iter().map(|m| m[0]).collect();
        assert_eq!(all, [2, 2, 2, 128, 129, 10, 10]);
    }
}
