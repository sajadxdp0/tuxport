//! Virtual network devices: bridge, veth and dummy (docs/rounds/170-netdev.md).
//!
//! Every network namespace (the initial one too) has a device table ([`Devices`], in
//! [`NetNs::devs`]). Container engines (dockerd, netavark) create bridges and veth pairs and
//! configure addresses and routes over netlink; this module records that state as Linux 6.1 does
//! and answers queries. No packets exist: container traffic is translated to host loopback
//! addresses in the socket layer ([`crate::netns`]).
//!
//! - In the initial namespace, virtual devices sit on top of the host's interfaces: indexes are
//!   allocated after the host's largest, and names must not repeat the host's; the host's
//!   interfaces still cannot be changed. In a new namespace they sit alongside lo.
//! - The two ends of a veth are each in their own namespace and find each other through `Weak`:
//!   deleting one end deletes the other, and when a namespace is no longer used (its device table
//!   is freed with it), the peers of its veths are deleted too (cleared on lookup,
//!   [`Devices::prune`]).
//! - Carrier and state as in 6.1: a veth has carrier only when both ends are up; a bridge only
//!   when it has a port in forwarding state (a port that is up and has carrier enters forwarding
//!   at once while STP is off); a dummy always. An up veth without carrier is LOWERLAYERDOWN
//!   (`default_operstate`: its iflink is not itself), a bridge is DOWN, and a dummy is always
//!   UNKNOWN.
//! - Addresses: IPv4 ones as in `__inet_insert_ifa` (a second one in the same subnet is
//!   secondary), and routes added per address as in `fib_add_ifaddr` (the local route always,
//!   subnet and broadcast routes while the interface is up); when an interface goes down, the
//!   routes through it are removed (except local routes: `fib_sync_down_dev` leaves next hops with
//!   scope nowhere alone). IPv6 is configured when the interface is up with carrier and IPv6 is
//!   not disabled (addrconf): a link-local address generated from the MAC (EUI-64), and routes for
//!   fe80::/64 and ff00::/8; there is no DAD, so addresses are usable at once. When an interface
//!   goes down, its IPv6 addresses and routes are all removed (`keep_addr_on_down` is 0).
//! - Routes: those added per address and those added over netlink are kept in the namespace in
//!   the order added, and dumps sort them together with the host's (or lo's) routes
//!   ([`crate::route`]). Only through virtual devices.
//! - Interface sysctls: see [`crate::netconf`].

use std::rc::{Rc, Weak};

use tuxport_hal::net::Interface;

use crate::cell::RefCell;
use crate::cred::{Cred, cap};
use crate::errno::Errno;
use crate::net::{AF_INET, AF_INET6, attr};
use crate::netconf::{Conf, NetConf};
use crate::nsproxy::NetNs;
use crate::route::Entry;

pub(crate) type DevRef = Rc<RefCell<Dev>>;

const IFF_UP: u32 = 0x1;
const IFF_BROADCAST: u32 = 0x2;
const IFF_RUNNING: u32 = 0x40;
const IFF_NOARP: u32 = 0x80;
const IFF_MULTICAST: u32 = 0x1000;
const IFF_LOWER_UP: u32 = 0x1_0000;
/// The bits `__dev_change_flags` changes as requested: DEBUG, NOTRAILERS, NOARP, DYNAMIC,
/// MULTICAST, PORTSEL and AUTOMEDIA, plus PROMISC and ALLMULTI as set by the user (gflags).
const CHANGEABLE: u32 = 0x4 | 0x20 | 0x80 | 0x8000 | 0x1000 | 0x2000 | 0x4000 | 0x100 | 0x200;
const IF_OPER_UNKNOWN: u8 = 0;
const IF_OPER_DOWN: u8 = 2;
const IF_OPER_LOWERLAYERDOWN: u8 = 3;
const IF_OPER_UP: u8 = 6;
const IFNAMSIZ: usize = 16;
const ETH_MIN_MTU: u32 = 68;
const ETH_MAX_MTU: u32 = 65535;
const ETH_DATA_LEN: u32 = 1500;

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_SETLINK: u16 = 19;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
const NLM_F_REPLACE: u16 = 0x100;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const NLM_F_APPEND: u16 = 0x800;

const IFLA_ADDRESS: u16 = 1;
const IFLA_BROADCAST: u16 = 2;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_LINK: u16 = 5;
const IFLA_QDISC: u16 = 6;
const IFLA_STATS: u16 = 7;
const IFLA_MASTER: u16 = 10;
const IFLA_TXQLEN: u16 = 13;
const IFLA_OPERSTATE: u16 = 16;
const IFLA_LINKMODE: u16 = 17;
const IFLA_LINKINFO: u16 = 18;
const IFLA_NET_NS_PID: u16 = 19;
const IFLA_IFALIAS: u16 = 20;
const IFLA_STATS64: u16 = 23;
const IFLA_GROUP: u16 = 27;
const IFLA_NET_NS_FD: u16 = 28;
const IFLA_PROMISCUITY: u16 = 30;
const IFLA_NUM_TX_QUEUES: u16 = 31;
const IFLA_NUM_RX_QUEUES: u16 = 32;
const IFLA_CARRIER: u16 = 33;
const IFLA_CARRIER_CHANGES: u16 = 35;
const IFLA_LINK_NETNSID: u16 = 37;
const IFLA_PROTO_DOWN: u16 = 39;
const IFLA_GSO_MAX_SEGS: u16 = 40;
const IFLA_GSO_MAX_SIZE: u16 = 41;
const IFLA_TARGET_NETNSID: u16 = 46;
const IFLA_CARRIER_UP_COUNT: u16 = 47;
const IFLA_CARRIER_DOWN_COUNT: u16 = 48;
const IFLA_MIN_MTU: u16 = 50;
const IFLA_MAX_MTU: u16 = 51;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const IFLA_INFO_SLAVE_KIND: u16 = 4;
const IFLA_INFO_SLAVE_DATA: u16 = 5;
const VETH_INFO_PEER: u16 = 1;

const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const IFA_BROADCAST: u16 = 4;
const IFA_CACHEINFO: u16 = 6;
const IFA_FLAGS: u16 = 8;
const IFA_RT_PRIORITY: u16 = 9;
const IFA_PROTO: u16 = 11;
const IFA_F_SECONDARY: u32 = 0x1;
const IFA_F_NODAD: u32 = 0x2;
const IFA_F_OPTIMISTIC: u32 = 0x4;
const IFA_F_HOMEADDRESS: u32 = 0x10;
const IFA_F_PERMANENT: u32 = 0x80;
const IFA_F_MANAGETEMPADDR: u32 = 0x100;
const IFA_F_NOPREFIXROUTE: u32 = 0x200;
const IFA_F_MCAUTOJOIN: u32 = 0x400;
/// A link-local address generated by the kernel (`IFAPROT_KERNEL_LL`).
const IFAPROT_KERNEL_LL: u8 = 3;

const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PRIORITY: u16 = 6;
const RTA_PREFSRC: u16 = 7;
const RTA_TABLE: u16 = 15;
const RT_TABLE_UNSPEC: u8 = 0;
const RT_TABLE_MAIN: u8 = 254;
const RT_TABLE_LOCAL: u8 = 255;
const RTN_UNICAST: u8 = 1;
const RTN_LOCAL: u8 = 2;
const RTN_BROADCAST: u8 = 3;
const RTN_MULTICAST: u8 = 5;
const RTPROT_KERNEL: u8 = 2;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RT_SCOPE_HOST: u8 = 254;
const RT_SCOPE_NOWHERE: u8 = 255;
/// rtm_flags: the gateway is taken as on the link without a route covering it (`ip route`'s
/// onlink), and the next hop's interface has no carrier (linkdown).
const RTNH_F_ONLINK: u32 = 4;
const RTNH_F_LINKDOWN: u32 = 0x10;
/// IPv6 route metrics: for routes added per address, and when none is given
/// (`IP6_RT_PRIO_ADDRCONF`, `IP6_RT_PRIO_USER`).
const IP6_PRIO_ADDRCONF: u32 = 256;
const IP6_PRIO_USER: u32 = 1024;

/// A failed netlink request: the error code and the extack message (the one Linux gives with
/// `NL_SET_ERR_MSG`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fail {
    pub errno: i32,
    pub msg: Option<&'static str>,
}

impl From<Errno> for Fail {
    fn from(e: Errno) -> Fail {
        Fail {
            errno: e.0,
            msg: None,
        }
    }
}

fn fail(e: Errno, msg: &'static str) -> Fail {
    Fail {
        errno: e.0,
        msg: Some(msg),
    }
}

/// Device kind (`IFLA_INFO_KIND`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Bridge,
    Veth,
    Dummy,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Bridge => "bridge",
            Kind::Veth => "veth",
            Kind::Dummy => "dummy",
        }
    }

    fn parse(b: &[u8]) -> Option<Kind> {
        match b {
            b"bridge" => Some(Kind::Bridge),
            b"veth" => Some(Kind::Veth),
            b"dummy" => Some(Kind::Dummy),
            _ => None,
        }
    }

    /// MTU bounds (`min_mtu`, `max_mtu`; 0 means unbounded): both 0 for dummy.
    fn mtu_range(self) -> (u32, u32) {
        match self {
            Kind::Dummy => (0, 0),
            _ => (ETH_MIN_MTU, ETH_MAX_MTU),
        }
    }

    /// Whether it can move to another namespace (bridges have `NETIF_F_NETNS_LOCAL`).
    fn movable(self) -> bool {
        self != Kind::Bridge
    }
}

/// One IPv4 address on an interface (`struct in_ifaddr`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Addr4 {
    pub local: [u8; 4],
    /// `IFA_ADDRESS`: the peer for point-to-point, otherwise the same as local.
    pub address: [u8; 4],
    pub prefix: u8,
    /// All zeros means none.
    pub broadcast: [u8; 4],
    pub label: Vec<u8>,
    pub flags: u32,
    pub scope: u8,
    pub metric: u32,
    /// `IFA_PROTO`; 0 means none.
    pub proto: u8,
}

impl Addr4 {
    fn mask(&self) -> u32 {
        u32::MAX
            .checked_shl(32 - u32::from(self.prefix))
            .unwrap_or(0)
    }

    /// The subnet (by `IFA_ADDRESS`).
    fn subnet(&self) -> u32 {
        u32::from_be_bytes(self.address) & self.mask()
    }

    fn contains(&self, ip: [u8; 4]) -> bool {
        u32::from_be_bytes(ip) & self.mask() == self.subnet()
    }
}

/// One IPv6 address on an interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Addr6 {
    pub addr: [u8; 16],
    pub prefix: u8,
    pub flags: u32,
    pub proto: u8,
    pub metric: u32,
}

/// One virtual device.
#[derive(Debug)]
pub(crate) struct Dev {
    pub ns: Weak<NetNs>,
    pub index: u32,
    pub name: Vec<u8>,
    pub kind: Kind,
    /// `IFF_UP`.
    pub up: bool,
    /// Has been up (a noqueue qdisc was attached, and it stays after going down).
    pub ever_up: bool,
    /// The bridge has computed its state from its ports (`br_port_state_selection`: after ports
    /// are added or removed). Before that its carrier is the "on" it was created with, and its
    /// state stays UNKNOWN (no carrier change event has happened).
    pub selected: bool,
    /// Other flags: those determined by the kind (BROADCAST, MULTICAST, NOARP) and those changed
    /// ([`CHANGEABLE`]).
    pub flags: u32,
    pub mtu: u32,
    /// The bridge's MTU was set by the user and no longer follows its ports
    /// (`BROPT_MTU_SET_BY_USER`).
    pub mtu_set: bool,
    pub mac: [u8; 6],
    /// The MAC was given by the user (`NET_ADDR_SET`): a bridge no longer takes the smallest of
    /// its ports'.
    pub mac_set: bool,
    pub txqlen: u32,
    pub group: u32,
    pub alias: Option<Vec<u8>>,
    /// The other end of a veth.
    pub peer: Weak<RefCell<Dev>>,
    /// The veth is connected to its other end (the end registered first at creation is not, until
    /// the other end registers).
    pub paired: bool,
    /// The bridge it is attached to.
    pub master: Weak<RefCell<Dev>>,
    pub v4: Vec<Addr4>,
    pub v6: Vec<Addr6>,
    /// IPv6 has been configured (when up with carrier, addrconf).
    pub v6_ready: bool,
    pub conf4: Conf,
    pub conf6: Conf,
    /// Number of carrier changes (`IFLA_CARRIER_CHANGES`), and the carrier last seen.
    pub carrier_changes: u32,
    pub carrier_was: bool,
}

/// A route added per address or over netlink (only through virtual devices).
#[derive(Debug, Clone)]
pub(crate) struct Rt {
    pub family: i32,
    pub table: u8,
    pub ty: u8,
    pub proto: u8,
    pub scope: u8,
    pub tos: u8,
    pub dst: Vec<u8>,
    pub prefix: u8,
    pub prefsrc: Option<Vec<u8>>,
    pub gateway: Option<Vec<u8>>,
    pub dev: Weak<RefCell<Dev>>,
    pub metric: u32,
    /// Added with `RTNH_F_ONLINK`.
    pub onlink: bool,
    /// The address it was added for (local for IPv4, the address for IPv6), or added during IPv6
    /// configuration (empty); None means added over netlink.
    pub owner: Option<Vec<u8>>,
}

impl Rt {
    fn on(&self, d: &DevRef) -> bool {
        self.dev.upgrade().is_some_and(|x| Rc::ptr_eq(&x, d))
    }

    /// The same route (for deduplication when adding).
    fn same(&self, o: &Rt) -> bool {
        (
            self.family,
            self.table,
            self.ty,
            self.tos,
            &self.dst,
            self.prefix,
            &self.prefsrc,
            &self.gateway,
            self.metric,
        ) == (
            o.family, o.table, o.ty, o.tos, &o.dst, o.prefix, &o.prefsrc, &o.gateway, o.metric,
        ) && Weak::ptr_eq(&self.dev, &o.dev)
    }
}

/// The virtual devices, routes and sysctls of one network namespace.
#[derive(Debug, Default)]
pub(crate) struct Devices {
    pub list: Vec<DevRef>,
    /// The last interface index allocated (Linux's `net->ifindex`).
    ifindex: u32,
    /// Routes through virtual devices, in the order added.
    pub routes: Vec<Rt>,
    pub conf: NetConf,
    /// Ids allocated to other namespaces (`IFLA_LINK_NETNSID`, `RTM_GETNSID`).
    pub nsids: Vec<(Weak<NetNs>, i32)>,
    /// In the initial namespace: virtual addresses and their host loopback addresses
    /// (127.255.x.y, allocated when used, [`crate::netns`]).
    pub hostmap: Vec<([u8; 4], [u8; 4])>,
}

impl Devices {
    /// For a new namespace: lo takes index 1, and sysctls follow `init`'s
    /// ([`NetConf::for_new_ns`]).
    pub fn for_new_ns(init: &NetConf) -> Devices {
        Devices {
            ifindex: 1,
            conf: NetConf::for_new_ns(init),
            ..Devices::default()
        }
    }

    /// Removes veths whose peer is gone (the peer's namespace was freed) and routes through
    /// devices that are gone.
    fn prune(&mut self) {
        let gone: Vec<DevRef> = self
            .list
            .iter()
            .filter(|d| {
                let d = d.borrow();
                d.kind == Kind::Veth && d.paired && d.peer.strong_count() == 0
            })
            .cloned()
            .collect();
        if !gone.is_empty() {
            self.list.retain(|d| !gone.iter().any(|g| Rc::ptr_eq(g, d)));
            for g in &gone {
                for d in &self.list {
                    let mut d = d.borrow_mut();
                    if d.master.upgrade().is_some_and(|m| Rc::ptr_eq(&m, g)) {
                        d.master = Weak::new();
                    }
                }
            }
        }
        self.routes.retain(|r| r.dev.strong_count() > 0);
    }

    fn by_index(&self, index: u32) -> Option<DevRef> {
        self.list
            .iter()
            .find(|d| d.borrow().index == index)
            .cloned()
    }

    fn by_name(&self, name: &[u8]) -> Option<DevRef> {
        self.list.iter().find(|d| d.borrow().name == name).cloned()
    }

    /// Ports: the devices attached to `br`.
    fn ports(&self, br: &DevRef) -> Vec<DevRef> {
        self.list
            .iter()
            .filter(|d| {
                d.borrow()
                    .master
                    .upgrade()
                    .is_some_and(|m| Rc::ptr_eq(&m, br))
            })
            .cloned()
            .collect()
    }
}

// ---- netlink change requests ----

/// Finds the network namespace that `IFLA_NET_NS_PID` or `IFLA_NET_NS_FD` refers to (ESRCH when
/// the process is gone, EINVAL when the fd is not a network namespace).
pub(crate) type Resolve<'a> = &'a dyn Fn(u16, u32) -> Result<Rc<NetNs>, Errno>;

/// A netlink request changing interfaces, addresses or routes (a whole message): `RTM_NEWLINK`,
/// `RTM_SETLINK`, `RTM_DELLINK`, `RTM_NEWADDR`, `RTM_DELADDR`, `RTM_NEWROUTE`, `RTM_DELROUTE`.
/// None: none of these. Needs `CAP_NET_ADMIN` in the namespace's owner (EPERM,
/// `netlink_net_capable`).
pub(crate) fn request(
    ns: &Rc<NetNs>,
    cred: &Cred,
    req: &[u8],
    resolve: Resolve<'_>,
) -> Option<Result<(), Fail>> {
    let ty = u16::from_le_bytes([req[4], req[5]]);
    if !matches!(
        ty,
        RTM_NEWLINK
            | RTM_DELLINK
            | RTM_SETLINK
            | RTM_NEWADDR
            | RTM_DELADDR
            | RTM_NEWROUTE
            | RTM_DELROUTE
    ) {
        return None;
    }
    if !cred.ns_capable(&ns.owner, cap::NET_ADMIN) {
        return Some(Err(Errno::EPERM.into()));
    }
    Some(match ty {
        RTM_NEWLINK | RTM_SETLINK => link_request(ns, cred, req, resolve),
        RTM_DELLINK => del_request(ns, req),
        RTM_NEWADDR | RTM_DELADDR => addr_request(ns, cred, req),
        _ => route_request(ns, req),
    })
}

fn link_request(ns: &Rc<NetNs>, cred: &Cred, req: &[u8], resolve: Resolve<'_>) -> Result<(), Fail> {
    let ty = u16::from_le_bytes([req[4], req[5]]);
    let nlflags = u16::from_le_bytes([req[6], req[7]]);
    let m = req.get(16..32).ok_or(Errno::EINVAL)?;
    let (index, ifi_flags, change) = ifinfo(m);
    let attrs = crate::route::attrs(&req[32..]);
    let name = ifname(&attrs)?;
    let newlink = ty == RTM_NEWLINK;
    if newlink && index < 0 {
        return Err(Errno::EINVAL.into());
    }
    if !newlink && index <= 0 && name.is_none() {
        return Err(Errno::EINVAL.into());
    }
    let virt = {
        let d = devs(ns);
        if index > 0 {
            d.by_index(index as u32)
        } else {
            name.and_then(|n| d.by_name(n))
        }
    };
    if let Some(dev) = virt {
        let info = linkinfo(&attrs);
        if newlink {
            if nlflags & NLM_F_EXCL != 0 {
                return Err(Errno::EEXIST.into());
            }
            if nlflags & NLM_F_REPLACE != 0 {
                return Err(Errno::EOPNOTSUPP.into());
            }
            // Kind arguments (changelink): only bridges have them; accepted but without effect
            if info.data.is_some() {
                let kind = info.kind.and_then(Kind::parse);
                if kind != Some(dev.borrow().kind) || kind != Some(Kind::Bridge) {
                    return Err(Errno::EOPNOTSUPP.into());
                }
            }
            if info.slave_data.is_some() && dev.borrow().master.strong_count() == 0 {
                return Err(Errno::EOPNOTSUPP.into());
            }
        }
        let r = setlink(
            ns, cred, &dev, index, ifi_flags, change, &attrs, name, resolve,
        );
        settle_around(&dev);
        return r;
    }
    let base_ifs = base(ns);
    let hit = base_ifs.iter().find(|i| {
        if index > 0 {
            i.index as i32 == index
        } else {
            name == Some(i.name.as_bytes())
        }
    });
    if let Some(i) = hit {
        if newlink && nlflags & NLM_F_EXCL != 0 {
            return Err(Errno::EEXIST.into());
        }
        return if ns.host_net.is_some() {
            lo_link(ns, cred, ifi_flags, change, &attrs, resolve)
        } else {
            let _ = i;
            match crate::net::configure_host_link(&base_ifs, true, req) {
                Some(0) | None => Ok(()),
                Some(e) => Err(Errno(e).into()),
            }
        };
    }
    if !newlink || nlflags & NLM_F_CREATE == 0 {
        return Err(Errno::ENODEV.into());
    }
    create(ns, cred, index, ifi_flags, change, &attrs, resolve)
}

/// Changes a virtual device (`do_setlink`): in order, moving to another namespace, MAC, MTU,
/// group, rename (when found by index), alias, flags, master, transmit queue length; earlier
/// changes stay when a later one fails.
#[allow(clippy::too_many_arguments)]
fn setlink(
    ns: &Rc<NetNs>,
    cred: &Cred,
    dev: &DevRef,
    index: i32,
    ifi_flags: u32,
    change: u32,
    attrs: &[(u16, &[u8])],
    name: Option<&[u8]>,
    resolve: Resolve<'_>,
) -> Result<(), Fail> {
    let mut ns = ns.clone();
    if let Some(to) = target_ns(attrs, cred, resolve)? {
        let pat = name.filter(|n| !n.is_empty());
        move_to(&ns, dev, &to, pat)?;
        ns = to;
    }
    if let Some(a) = get(attrs, IFLA_ADDRESS) {
        set_mac(&ns, dev, a)?;
    }
    if let Some(m) = get_u32(attrs, IFLA_MTU) {
        set_mtu(&ns, dev, m)?;
    }
    if let Some(g) = get_u32(attrs, IFLA_GROUP) {
        dev.borrow_mut().group = g;
    }
    if index > 0
        && let Some(n) = name
    {
        rename(&ns, dev, n)?;
    }
    if let Some(a) = get(attrs, IFLA_IFALIAS) {
        let a = a.split(|&c| c == 0).next().unwrap_or_default();
        dev.borrow_mut().alias = (!a.is_empty()).then(|| a.to_vec());
    }
    if ifi_flags != 0 || change != 0 {
        let cur = reported_flags(&ns, dev);
        change_flags(&ns, dev, combine(cur, ifi_flags, change));
    }
    if let Some(m) = get_u32(attrs, IFLA_MASTER) {
        set_master(&ns, dev, m)?;
    }
    if get(attrs, IFLA_CARRIER).is_some() && dev.borrow().kind != Kind::Dummy {
        return Err(Errno::EOPNOTSUPP.into());
    }
    if let Some(q) = get_u32(attrs, IFLA_TXQLEN) {
        dev.borrow_mut().txqlen = q;
    }
    Ok(())
}

/// lo in a new namespace: can only be brought up or down and have its MTU changed; attaching it
/// to a bridge is EINVAL (a loopback interface), moving it to another namespace is EINVAL
/// (`NETIF_F_NETNS_LOCAL`), and renaming is not supported.
fn lo_link(
    ns: &Rc<NetNs>,
    cred: &Cred,
    ifi_flags: u32,
    change: u32,
    attrs: &[(u16, &[u8])],
    resolve: Resolve<'_>,
) -> Result<(), Fail> {
    // Moving to any namespace (including the one it is in) is EINVAL: Linux checks
    // NETIF_F_NETNS_LOCAL first
    if target_ns(attrs, cred, resolve)?.is_some() {
        return Err(Errno::EINVAL.into());
    }
    if let Some(m) = get_u32(attrs, IFLA_MTU) {
        if m < ETH_MIN_MTU {
            return Err(fail(Errno::EINVAL, "mtu less than device minimum"));
        }
        ns.lo.borrow_mut().mtu = Some(m);
    }
    // Finding lo by index and giving another name is a rename, which is not supported
    if ifname(attrs)?.is_some_and(|n| n != b"lo") {
        return Err(Errno::EOPNOTSUPP.into());
    }
    if ifi_flags != 0 || change != 0 {
        let cur = crate::netns::lo_interface(ns).flags;
        crate::netns::set_up(
            &mut ns.lo.borrow_mut(),
            combine(cur, ifi_flags, change) & IFF_UP != 0,
        );
    }
    if let Some(m) = get_u32(attrs, IFLA_MASTER)
        && m != 0
    {
        let target = devs(ns).by_index(m);
        return Err(match target {
            Some(t) if t.borrow().kind != Kind::Bridge => Errno::EOPNOTSUPP,
            _ => Errno::EINVAL,
        }
        .into());
    }
    Ok(())
}

/// Creates a device (`rtnl_newlink_create`): a veth registers its peer first (in the namespace
/// given in `VETH_INFO_PEER`, or the requesting one when none is given), then itself; then flags
/// are changed per the ifinfomsg, and finally it is attached to its master (the new device is
/// deleted on failure).
fn create(
    ns: &Rc<NetNs>,
    cred: &Cred,
    index: i32,
    ifi_flags: u32,
    change: u32,
    attrs: &[(u16, &[u8])],
    resolve: Resolve<'_>,
) -> Result<(), Fail> {
    let info = linkinfo(attrs);
    let Some(kind) = info.kind.and_then(Kind::parse) else {
        return Err(fail(Errno::EOPNOTSUPP, "Unknown device type"));
    };
    validate(kind, attrs)?;
    let dest = target_ns(attrs, cred, resolve)?.unwrap_or_else(|| ns.clone());
    let default_name: &[u8] = match kind {
        Kind::Bridge => b"bridge%d",
        Kind::Veth => b"veth%d",
        Kind::Dummy => b"dummy%d",
    };
    let pat = ifname(attrs)?.unwrap_or(default_name);
    let dev = if kind == Kind::Veth {
        let peer = info
            .data
            .map(crate::route::attrs)
            .and_then(|a| get(&a, VETH_INFO_PEER));
        let (pm, pattrs) = match peer {
            Some(p) if p.len() >= 16 => (p[..16].to_vec(), crate::route::attrs(&p[16..])),
            Some(_) => return Err(Errno::EINVAL.into()),
            None => (vec![0u8; 16], Vec::new()),
        };
        validate(kind, &pattrs)?;
        let peer_ns = target_ns(&pattrs, cred, resolve)?.unwrap_or_else(|| ns.clone());
        let ppat = ifname(&pattrs)?.unwrap_or(b"veth%d");
        let (pindex, pflags, pchange) = ifinfo(&pm);
        let p = new_dev(&peer_ns, kind, ppat, pindex, &pattrs)?;
        let dev = match new_dev(&dest, kind, pat, index, attrs) {
            Ok(d) => d,
            Err(e) => {
                unlink(&peer_ns, &p);
                return Err(e);
            }
        };
        p.borrow_mut().peer = Rc::downgrade(&dev);
        p.borrow_mut().paired = true;
        dev.borrow_mut().peer = Rc::downgrade(&p);
        dev.borrow_mut().paired = true;
        if pflags != 0 || pchange != 0 {
            let cur = reported_flags(&peer_ns, &p);
            change_flags(&peer_ns, &p, combine(cur, pflags, pchange));
        }
        dev
    } else {
        new_dev(&dest, kind, pat, index, attrs)?
    };
    if ifi_flags != 0 || change != 0 {
        let cur = reported_flags(&dest, &dev);
        change_flags(&dest, &dev, combine(cur, ifi_flags, change));
    }
    if let Some(m) = get_u32(attrs, IFLA_MASTER)
        && let Err(e) = set_master(&dest, &dev, m)
    {
        delete(&dest, &dev);
        return Err(e);
    }
    settle_around(&dev);
    Ok(())
}

/// Kind checks (`validate` of veth, bridge and dummy): a MAC must be 6 bytes (EINVAL) and valid
/// (EADDRNOTAVAIL).
fn validate(kind: Kind, attrs: &[(u16, &[u8])]) -> Result<(), Fail> {
    if let Some(a) = get(attrs, IFLA_ADDRESS) {
        if a.len() != 6 {
            return Err(Errno::EINVAL.into());
        }
        if !valid_mac(a) {
            return Err(Errno::EADDRNOTAVAIL.into());
        }
    }
    if let Some(m) = get_u32(attrs, IFLA_MTU) {
        validate_mtu(kind, m)?;
    }
    Ok(())
}

/// Registers a new device in `ns` (`rtnl_create_link`, `register_netdevice`): the name from
/// `pat` (which may contain `%d`), index `index` (allocated when 0), and the MTU, MAC, transmit
/// queue length and group from the attributes.
fn new_dev(
    ns: &Rc<NetNs>,
    kind: Kind,
    pat: &[u8],
    index: i32,
    attrs: &[(u16, &[u8])],
) -> Result<DevRef, Fail> {
    let mut d = devs(ns);
    let name = choose_name(ns, &d, pat)?;
    let index = if index > 0 {
        if index_in_use(ns, &d, index as u32) {
            return Err(Errno::EBUSY.into());
        }
        index as u32
    } else {
        alloc_index(ns, &mut d)
    };
    let mac = get(attrs, IFLA_ADDRESS).map(|a| <[u8; 6]>::try_from(&a[..6]).unwrap());
    let flags = match kind {
        Kind::Dummy => IFF_BROADCAST | IFF_NOARP,
        _ => IFF_BROADCAST | IFF_MULTICAST,
    };
    let dev = Rc::new(RefCell::new(Dev {
        ns: Rc::downgrade(ns),
        index,
        name,
        kind,
        up: false,
        ever_up: false,
        // A bridge created with a MAC computes its state from its ports at once (br_dev_newlink
        // calls br_stp_change_bridge_id); it has no ports, so no carrier
        selected: kind == Kind::Bridge && mac.is_some(),
        flags,
        mtu: get_u32(attrs, IFLA_MTU).unwrap_or(ETH_DATA_LEN),
        mtu_set: kind == Kind::Bridge && get(attrs, IFLA_MTU).is_some(),
        mac: mac.unwrap_or_else(random_mac),
        mac_set: mac.is_some(),
        txqlen: get_u32(attrs, IFLA_TXQLEN).unwrap_or(1000),
        group: get_u32(attrs, IFLA_GROUP).unwrap_or(0),
        alias: None,
        peer: Weak::new(),
        paired: false,
        master: Weak::new(),
        v4: Vec::new(),
        v6: Vec::new(),
        v6_ready: false,
        conf4: d.conf.dflt4.inherit(),
        conf6: d.conf.dflt6.inherit(),
        carrier_changes: 0,
        carrier_was: false,
    }));
    d.list.push(dev.clone());
    Ok(dev)
}

fn random_mac() -> [u8; 6] {
    let mut m = [0u8; 6];
    let _ = tuxport_hal::fill_random(&mut m);
    // A unicast, locally administered address (eth_random_addr)
    m[0] = (m[0] & 0xfe) | 0x02;
    m
}

fn del_request(ns: &Rc<NetNs>, req: &[u8]) -> Result<(), Fail> {
    let m = req.get(16..32).ok_or(Errno::EINVAL)?;
    let (index, ..) = ifinfo(m);
    let attrs = crate::route::attrs(&req[32..]);
    let name = ifname(&attrs)?;
    if index <= 0 && name.is_none() {
        return Err(Errno::EINVAL.into());
    }
    let virt = {
        let d = devs(ns);
        if index > 0 {
            d.by_index(index as u32)
        } else {
            name.and_then(|n| d.by_name(n))
        }
    };
    if let Some(dev) = virt {
        delete(ns, &dev);
        return Ok(());
    }
    let known = base(ns).iter().any(|i| {
        if index > 0 {
            i.index as i32 == index
        } else {
            name == Some(i.name.as_bytes())
        }
    });
    // lo and host interfaces have no rtnl_link_ops, so they cannot be deleted
    Err(if known {
        Errno::EOPNOTSUPP
    } else {
        Errno::ENODEV
    }
    .into())
}

/// ifinfomsg's index, flags and flags to change.
fn ifinfo(m: &[u8]) -> (i32, u32, u32) {
    (
        i32::from_le_bytes(m[4..8].try_into().unwrap()),
        u32::from_le_bytes(m[8..12].try_into().unwrap()),
        u32::from_le_bytes(m[12..16].try_into().unwrap()),
    )
}

/// `IFLA_IFNAME` (without the terminating NUL): ERANGE from the policy check when longer than 15
/// bytes.
fn ifname<'a>(attrs: &[(u16, &'a [u8])]) -> Result<Option<&'a [u8]>, Fail> {
    let Some(d) = get(attrs, IFLA_IFNAME) else {
        return Ok(None);
    };
    let n = d.split(|&c| c == 0).next().unwrap_or_default();
    if n.len() >= IFNAMSIZ {
        return Err(fail(Errno::ERANGE, "Attribute failed policy validation"));
    }
    Ok(Some(n))
}

/// The kind and arguments in `IFLA_LINKINFO`.
#[derive(Default)]
struct LinkInfo<'a> {
    kind: Option<&'a [u8]>,
    data: Option<&'a [u8]>,
    slave_data: Option<&'a [u8]>,
}

fn linkinfo<'a>(attrs: &[(u16, &'a [u8])]) -> LinkInfo<'a> {
    let Some(li) = get(attrs, IFLA_LINKINFO) else {
        return LinkInfo::default();
    };
    let inner = crate::route::attrs(li);
    LinkInfo {
        kind: get(&inner, IFLA_INFO_KIND).map(|k| k.split(|&c| c == 0).next().unwrap_or_default()),
        data: get(&inner, IFLA_INFO_DATA),
        slave_data: get(&inner, IFLA_INFO_SLAVE_DATA),
    }
}

/// The namespace named in the request (`rtnl_link_get_net_capable`): needs `CAP_NET_ADMIN` in
/// its owner.
fn target_ns(
    attrs: &[(u16, &[u8])],
    cred: &Cred,
    resolve: Resolve<'_>,
) -> Result<Option<Rc<NetNs>>, Fail> {
    let ns = if let Some(pid) = get_u32(attrs, IFLA_NET_NS_PID) {
        resolve(IFLA_NET_NS_PID, pid)?
    } else if let Some(fd) = get_u32(attrs, IFLA_NET_NS_FD) {
        resolve(IFLA_NET_NS_FD, fd)?
    } else if get(attrs, IFLA_TARGET_NETNSID).is_some() {
        // Naming a namespace by id is not implemented
        return Err(Errno::EOPNOTSUPP.into());
    } else {
        return Ok(None);
    };
    if !cred.ns_capable(&ns.owner, cap::NET_ADMIN) {
        return Err(Errno::EPERM.into());
    }
    Ok(Some(ns))
}

/// Combined flags (`rtnl_dev_combine_flags`): when `change` is 0, all of `flags` counts as
/// wanted.
fn combine(cur: u32, flags: u32, change: u32) -> u32 {
    if change != 0 {
        (flags & change) | (cur & !change)
    } else {
        flags
    }
}

// ---- Device changes ----

/// Moves to namespace `to` (`__dev_change_net_namespace`): first brought down and detached, with
/// addresses and routes all removed, and sysctls start over from that namespace's `default`; the
/// index is reallocated if it is taken there. If the name is taken, it is renamed by `pat`, and
/// EEXIST when none is given.
fn move_to(ns: &NetNs, dev: &DevRef, to: &Rc<NetNs>, pat: Option<&[u8]>) -> Result<(), Fail> {
    if !dev.borrow().kind.movable() {
        return Err(Errno::EINVAL.into());
    }
    if std::ptr::eq(ns, Rc::as_ptr(to)) {
        return Ok(());
    }
    let name = dev.borrow().name.clone();
    let new_name = {
        let d = devs(to);
        if name_in_use(to, &d, &name) {
            match pat {
                Some(p) => choose_name(to, &d, p)?,
                None => return Err(Errno::EEXIST.into()),
            }
        } else {
            name.clone()
        }
    };
    set_up(ns, dev, false);
    unlink(ns, dev);
    {
        let mut x = dev.borrow_mut();
        x.v4.clear();
        x.v6.clear();
        x.v6_ready = false;
        x.name = new_name;
    }
    {
        let mut d = devs(to);
        let index = dev.borrow().index;
        let index = if index_in_use(to, &d, index) {
            alloc_index(to, &mut d)
        } else {
            index
        };
        let (c4, c6) = (d.conf.dflt4.inherit(), d.conf.dflt6.inherit());
        {
            let mut x = dev.borrow_mut();
            x.index = index;
            x.ns = Rc::downgrade(to);
            x.conf4 = c4;
            x.conf6 = c6;
        }
        d.list.push(dev.clone());
    }
    settle_around(dev);
    settle(ns, &[]);
    Ok(())
}

/// Changes the MAC (`dev_set_mac_address_user`): it must be a valid unicast address
/// (EADDRNOTAVAIL).
fn set_mac(ns: &NetNs, dev: &DevRef, mac: &[u8]) -> Result<(), Fail> {
    if mac.len() < 6 {
        return Err(Errno::EINVAL.into());
    }
    if !valid_mac(&mac[..6]) {
        return Err(Errno::EADDRNOTAVAIL.into());
    }
    {
        let mut x = dev.borrow_mut();
        x.mac.copy_from_slice(&mac[..6]);
        x.mac_set = true;
        // A bridge changing its MAC recomputes its state from its ports (br_set_mac_address →
        // br_stp_change_bridge_id)
        if x.kind == Kind::Bridge {
            x.selected = true;
        }
    }
    let br = dev.borrow().master.upgrade();
    if let Some(br) = br {
        bridge_adjust(ns, &br);
    }
    Ok(())
}

fn valid_mac(m: &[u8]) -> bool {
    m.len() == 6 && m[0] & 1 == 0 && m.iter().any(|&b| b != 0)
}

/// Changes the MTU (`dev_set_mtu_ext`): a bridge records that the user set it, and when a port
/// changes, its bridge adjusts; IPv6's `mtu` follows the interface's.
fn set_mtu(ns: &NetNs, dev: &DevRef, mtu: u32) -> Result<(), Fail> {
    let kind = dev.borrow().kind;
    validate_mtu(kind, mtu)?;
    {
        let mut x = dev.borrow_mut();
        x.mtu = mtu;
        x.conf6.mtu6 = None;
        if kind == Kind::Bridge {
            x.mtu_set = true;
        }
    }
    let br = dev.borrow().master.upgrade();
    if let Some(br) = br {
        bridge_adjust(ns, &br);
    }
    // IPv6 is turned off when the MTU is below 1280 (NETDEV_CHANGEMTU)
    if i64::from(mtu) < crate::netconf::IPV6_MIN_MTU {
        let mut d = devs(ns);
        v6_down(&mut d, dev);
    }
    Ok(())
}

/// MTU check (`dev_validate_mtu`).
fn validate_mtu(kind: Kind, mtu: u32) -> Result<(), Fail> {
    let (min, max) = kind.mtu_range();
    if (mtu as i32) < 0 || mtu < min {
        return Err(fail(Errno::EINVAL, "mtu less than device minimum"));
    }
    if max > 0 && mtu > max {
        return Err(fail(Errno::EINVAL, "mtu greater than device maximum"));
    }
    Ok(())
}

/// Renames (`dev_change_name`): in 6.1 an interface that is up cannot be renamed (EBUSY).
fn rename(ns: &NetNs, dev: &DevRef, name: &[u8]) -> Result<(), Fail> {
    if dev.borrow().name == name {
        return Ok(());
    }
    if dev.borrow().up {
        return Err(Errno::EBUSY.into());
    }
    let new = {
        let d = devs(ns);
        choose_name(ns, &d, name)?
    };
    let mut x = dev.borrow_mut();
    // Address labels are renamed along with it (`inetdev_changename`)
    let old = std::mem::replace(&mut x.name, new.clone());
    for a in &mut x.v4 {
        if a.label.starts_with(&old) {
            let mut l = new.clone();
            l.extend_from_slice(&a.label[old.len()..]);
            a.label = l;
        }
    }
    Ok(())
}

/// Reported flags (`dev_get_flags`).
fn reported_flags(ns: &NetNs, dev: &DevRef) -> u32 {
    let d = devs(ns);
    view(dev, &d, ns).flags
}

/// Changes flags (`dev_change_flags`): `flags` are the wanted flags after combining.
fn change_flags(ns: &NetNs, dev: &DevRef, flags: u32) {
    {
        let mut x = dev.borrow_mut();
        x.flags = (flags & CHANGEABLE) | (x.flags & !CHANGEABLE);
    }
    set_up(ns, dev, flags & IFF_UP != 0);
}

/// Brings a device up or down (`__dev_open`, `__dev_close`).
fn set_up(ns: &NetNs, dev: &DevRef, up: bool) {
    if dev.borrow().up == up {
        return;
    }
    {
        let mut x = dev.borrow_mut();
        x.up = up;
        x.ever_up |= up;
    }
    let mut d = devs(ns);
    if up {
        // NETDEV_UP: re-add the routes per address
        let addrs = dev.borrow().v4.clone();
        for a in &addrs {
            add_ifaddr4(&mut d, dev, a);
        }
    } else {
        drop_routes(&mut d, dev, Some(AF_INET), true);
        let keep = {
            let x = dev.borrow();
            x.conf6.get(true, "keep_addr_on_down") > 0
                || d.conf.all6.get(true, "keep_addr_on_down") > 0
        };
        if keep {
            dev.borrow_mut().v6_ready = false;
            drop_routes(&mut d, dev, Some(AF_INET6), false);
        } else {
            v6_down(&mut d, dev);
        }
    }
}

/// Attaches to or detaches from a bridge (`do_set_master`, `br_add_if`, `br_del_if`). `index` 0
/// detaches.
fn set_master(ns: &NetNs, dev: &DevRef, index: u32) -> Result<(), Fail> {
    let cur = dev.borrow().master.upgrade();
    if let Some(cur) = &cur {
        if cur.borrow().index == index {
            return Ok(());
        }
        dev.borrow_mut().master = Weak::new();
        bridge_adjust(ns, cur);
    }
    if index == 0 {
        return Ok(());
    }
    let br = devs(ns).by_index(index);
    let Some(br) = br else {
        // Host interfaces and lo cannot be masters (no ndo_add_slave); a nonexistent one is
        // EINVAL
        return Err(if index_in_use(ns, &devs(ns), index) {
            Errno::EOPNOTSUPP
        } else {
            Errno::EINVAL
        }
        .into());
    };
    if br.borrow().kind != Kind::Bridge {
        return Err(Errno::EOPNOTSUPP.into());
    }
    if dev.borrow().kind == Kind::Bridge {
        return Err(fail(Errno::ELOOP, "Can not enslave a bridge to a bridge"));
    }
    dev.borrow_mut().master = Rc::downgrade(&br);
    bridge_adjust(ns, &br);
    Ok(())
}

/// Deletes (`rtnl_dellink`): a veth together with its peer.
fn delete(ns: &NetNs, dev: &DevRef) {
    let peer = dev.borrow().peer.upgrade();
    unlink(ns, dev);
    dev.borrow_mut().ns = Weak::new();
    let mut touched = Vec::new();
    if let Some(p) = peer {
        if let Some(pns) = p.borrow().ns.upgrade() {
            touched.push(pns);
        }
        if let Some(pns) = touched.last() {
            unlink(pns, &p);
        }
        p.borrow_mut().ns = Weak::new();
    }
    for n in &touched {
        settle(n, &[]);
    }
    settle(ns, &[]);
}

/// Removes a device from the namespace's table (on deletion or moving away): detaches its ports
/// and removes the routes through it.
fn unlink(ns: &NetNs, dev: &DevRef) {
    let ports = {
        let mut d = devs(ns);
        d.list.retain(|x| !Rc::ptr_eq(x, dev));
        d.routes.retain(|r| !r.on(dev) && r.dev.strong_count() > 0);
        d.ports(dev)
    };
    for p in &ports {
        p.borrow_mut().master = Weak::new();
    }
    let br = dev.borrow().master.upgrade();
    dev.borrow_mut().master = Weak::new();
    if let Some(br) = br {
        bridge_adjust(ns, &br);
    }
}

/// A bridge's MTU and MAC follow its ports (`br_mtu_auto_adjust`,
/// `br_stp_recalculate_bridge_id`): unless set by the user, the MTU is the smallest of its ports'
/// (1500 with no ports), and the MAC the smallest of its ports' (all zeros with no ports).
fn bridge_adjust(ns: &NetNs, br: &DevRef) {
    let ports = devs(ns).ports(br);
    let mut b = br.borrow_mut();
    b.selected = true;
    if !b.mtu_set {
        b.mtu = ports
            .iter()
            .map(|p| p.borrow().mtu)
            .min()
            .unwrap_or(ETH_DATA_LEN);
    }
    if !b.mac_set {
        b.mac = ports.iter().map(|p| p.borrow().mac).min().unwrap_or([0; 6]);
    }
}

// ---- Names and indexes ----

/// Chooses a name (`dev_get_valid_name`): one with `%d` takes the lowest unused number; others
/// must be valid and unused (otherwise EINVAL, EEXIST).
fn choose_name(ns: &NetNs, d: &Devices, pattern: &[u8]) -> Result<Vec<u8>, Errno> {
    if !valid_name(pattern) {
        return Err(Errno::EINVAL);
    }
    let Some(at) = pattern.windows(2).position(|w| w == b"%d") else {
        if pattern.contains(&b'%') {
            return Err(Errno::EINVAL);
        }
        if name_in_use(ns, d, pattern) {
            return Err(Errno::EEXIST);
        }
        return Ok(pattern.to_vec());
    };
    if pattern[at + 2..].contains(&b'%') || pattern[..at].contains(&b'%') {
        return Err(Errno::EINVAL);
    }
    for n in 0..32768 {
        let mut name = pattern[..at].to_vec();
        name.extend_from_slice(n.to_string().as_bytes());
        name.extend_from_slice(&pattern[at + 2..]);
        if name.len() >= IFNAMSIZ {
            return Err(Errno::EINVAL);
        }
        if !name_in_use(ns, d, &name) {
            return Ok(name);
        }
    }
    Err(Errno::ENFILE)
}

/// Whether an interface name is valid (`dev_valid_name`).
fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() < IFNAMSIZ
        && name != b"."
        && name != b".."
        && !name
            .iter()
            .any(|&c| c == b'/' || c == b':' || c.is_ascii_whitespace())
}

fn name_in_use(ns: &NetNs, d: &Devices, name: &[u8]) -> bool {
    d.by_name(name).is_some() || base(ns).iter().any(|i| i.name.as_bytes() == name)
}

/// Allocates an interface index (`dev_new_index`): in the initial namespace, after the host's
/// largest.
fn alloc_index(ns: &NetNs, d: &mut Devices) -> u32 {
    let host = base(ns);
    if ns.host_net.is_none() {
        let top = host.iter().map(|i| i.index).max().unwrap_or(0);
        d.ifindex = d.ifindex.max(top);
    }
    loop {
        d.ifindex = d.ifindex.wrapping_add(1).max(1);
        let i = d.ifindex;
        if d.by_index(i).is_none() && !host.iter().any(|h| h.index == i) {
            return i;
        }
    }
}

fn index_in_use(ns: &NetNs, d: &Devices, index: u32) -> bool {
    d.by_index(index).is_some() || base(ns).iter().any(|h| h.index == index)
}

// ---- Addresses ----

fn addr_request(ns: &Rc<NetNs>, cred: &Cred, req: &[u8]) -> Result<(), Fail> {
    let ty = u16::from_le_bytes([req[4], req[5]]);
    let nlflags = u16::from_le_bytes([req[6], req[7]]);
    let b = req.get(16..24).ok_or(Errno::EINVAL)?;
    let (family, prefix, ifa_flags, scope) = (i32::from(b[0]), b[1], b[2], b[3]);
    let index = u32::from_le_bytes(b[4..8].try_into().unwrap());
    let attrs = crate::route::attrs(&req[24..]);
    if family != AF_INET && family != AF_INET6 {
        return Err(Errno::EOPNOTSUPP.into());
    }
    let dev = devs(ns).by_index(index);
    let Some(dev) = dev else {
        if ns.host_net.is_some() && index == crate::netns::LO_INDEX {
            return match crate::netns::configure(ns, cred, req) {
                Some(0) | None => Ok(()),
                Some(e) => Err(Errno(e).into()),
            };
        }
        // Host interfaces cannot be changed
        return Err(if base(ns).iter().any(|i| i.index == index) {
            Errno::EOPNOTSUPP
        } else {
            Errno::ENODEV
        }
        .into());
    };
    let new = ty == RTM_NEWADDR;
    let flags = get_u32(&attrs, IFA_FLAGS).unwrap_or(u32::from(ifa_flags));
    let r = if family == AF_INET {
        if new {
            add_addr4(ns, &dev, nlflags, prefix, flags, scope, &attrs)
        } else {
            del_addr4(ns, &dev, prefix, &attrs)
        }
    } else if new {
        add_addr6(ns, &dev, nlflags, prefix, flags, &attrs)
    } else {
        del_addr6(ns, &dev, prefix, &attrs)
    };
    settle_around(&dev);
    r
}

/// Adds an IPv4 address (`inet_rtm_newaddr`, `__inet_insert_ifa`).
fn add_addr4(
    ns: &Rc<NetNs>,
    dev: &DevRef,
    nlflags: u16,
    prefix: u8,
    flags: u32,
    scope: u8,
    attrs: &[(u16, &[u8])],
) -> Result<(), Fail> {
    let local = ip4(attrs, IFA_LOCAL).or_else(|| ip4(attrs, IFA_ADDRESS));
    let (Some(local), true) = (local, prefix <= 32) else {
        return Err(Errno::EINVAL.into());
    };
    let mut a = Addr4 {
        local,
        address: ip4(attrs, IFA_ADDRESS).unwrap_or(local),
        prefix,
        broadcast: ip4(attrs, IFA_BROADCAST).unwrap_or([0; 4]),
        label: get(attrs, IFA_LABEL)
            .map(|l| l.split(|&c| c == 0).next().unwrap_or_default().to_vec())
            .unwrap_or_else(|| dev.borrow().name.clone()),
        // Lifetimes are all treated as permanent (IFA_CACHEINFO is ignored)
        flags: (flags & !IFA_F_SECONDARY) | IFA_F_PERMANENT,
        scope,
        metric: get_u32(attrs, IFA_RT_PRIORITY).unwrap_or(0),
        proto: get(attrs, IFA_PROTO)
            .and_then(|p| p.first().copied())
            .unwrap_or(0),
    };
    let _ = IFA_CACHEINFO;
    let mut x = dev.borrow_mut();
    let same =
        x.v4.iter()
            .position(|b| b.prefix == a.prefix && b.contains(a.address) && b.local == a.local);
    if let Some(i) = same {
        if nlflags & NLM_F_EXCL != 0 || nlflags & NLM_F_REPLACE == 0 {
            return Err(Errno::EEXIST.into());
        }
        let old = &mut x.v4[i];
        old.flags = (a.flags & !IFA_F_SECONDARY) | (old.flags & IFA_F_SECONDARY);
        old.metric = a.metric;
        old.proto = a.proto;
        let local = old.local;
        drop(x);
        redo_ifaddr4(&mut devs(ns), dev, &[local]);
        return Ok(());
    }
    let mut last_primary = 0;
    for (i, b) in x.v4.iter().enumerate() {
        if b.flags & IFA_F_SECONDARY == 0 && a.scope <= b.scope {
            last_primary = i + 1;
        }
        if b.prefix == a.prefix && b.contains(a.address) {
            if b.scope != a.scope {
                return Err(Errno::EINVAL.into());
            }
            a.flags |= IFA_F_SECONDARY;
        }
    }
    if a.flags & IFA_F_SECONDARY != 0 {
        x.v4.push(a.clone());
    } else {
        x.v4.insert(last_primary, a.clone());
    }
    drop(x);
    add_ifaddr4(&mut devs(ns), dev, &a);
    Ok(())
}

/// Deletes an IPv4 address (`inet_rtm_deladdr`): the first one matching the given local, label,
/// address and prefix; deleting a primary also deletes the secondaries in the same subnet
/// (`promote_secondaries` is not implemented).
fn del_addr4(ns: &Rc<NetNs>, dev: &DevRef, prefix: u8, attrs: &[(u16, &[u8])]) -> Result<(), Fail> {
    let local = ip4(attrs, IFA_LOCAL);
    let address = ip4(attrs, IFA_ADDRESS);
    let label = get(attrs, IFA_LABEL).map(|l| l.split(|&c| c == 0).next().unwrap_or_default());
    let mut x = dev.borrow_mut();
    let i =
        x.v4.iter()
            .position(|a| {
                local.is_none_or(|l| a.local == l)
                    && label.is_none_or(|l| a.label == l)
                    && address.is_none_or(|ad| a.prefix == prefix && a.contains(ad))
            })
            .ok_or(Errno::EADDRNOTAVAIL)?;
    let a = x.v4.remove(i);
    let mut gone = vec![a.local];
    if a.flags & IFA_F_SECONDARY == 0 {
        x.v4.retain(|b| {
            let sec = b.flags & IFA_F_SECONDARY != 0 && b.prefix == a.prefix && a.contains(b.local);
            if sec {
                gone.push(b.local);
            }
            !sec
        });
    }
    drop(x);
    redo_ifaddr4(&mut devs(ns), dev, &gone);
    Ok(())
}

fn ip4(attrs: &[(u16, &[u8])], ty: u16) -> Option<[u8; 4]> {
    get(attrs, ty).and_then(|d| d.try_into().ok())
}

/// Adds an IPv6 address (`inet6_rtm_newaddr`, `inet6_addr_add`): EACCES when the interface has
/// IPv6 disabled; placed in the list by scope (larger first, and within a scope the newest
/// first).
fn add_addr6(
    ns: &Rc<NetNs>,
    dev: &DevRef,
    nlflags: u16,
    prefix: u8,
    flags: u32,
    attrs: &[(u16, &[u8])],
) -> Result<(), Fail> {
    let addr = ip6(attrs, IFA_LOCAL).or_else(|| ip6(attrs, IFA_ADDRESS));
    let (Some(addr), true) = (addr, prefix <= 128) else {
        return Err(Errno::EINVAL.into());
    };
    let allowed = IFA_F_NODAD
        | IFA_F_HOMEADDRESS
        | IFA_F_MANAGETEMPADDR
        | IFA_F_NOPREFIXROUTE
        | IFA_F_MCAUTOJOIN
        | IFA_F_OPTIMISTIC;
    let a = Addr6 {
        addr,
        prefix,
        flags: (flags & allowed) | IFA_F_PERMANENT,
        proto: get(attrs, IFA_PROTO)
            .and_then(|p| p.first().copied())
            .unwrap_or(0),
        metric: get_u32(attrs, IFA_RT_PRIORITY).unwrap_or(0),
    };
    let mut x = dev.borrow_mut();
    if let Some(i) = x.v6.iter().position(|b| b.addr == addr) {
        if nlflags & NLM_F_EXCL != 0 || nlflags & NLM_F_REPLACE == 0 {
            return Err(Errno::EEXIST.into());
        }
        x.v6[i].flags = a.flags;
        x.v6[i].metric = a.metric;
        return Ok(());
    }
    if v6_disabled(&x) {
        return Err(Errno::EACCES.into());
    }
    let s = src_scope6(&addr);
    let at =
        x.v6.iter()
            .position(|b| s >= src_scope6(&b.addr))
            .unwrap_or(x.v6.len());
    x.v6.insert(at, a.clone());
    drop(x);
    add_ifaddr6(&mut devs(ns), dev, &a);
    Ok(())
}

/// The scope of an IPv6 address for list ordering (`ipv6_addr_src_scope`): global 14, site 5,
/// link 2, loopback 1.
fn src_scope6(a: &[u8; 16]) -> u8 {
    if a[0] == 0xfe && a[1] & 0xc0 == 0x80 {
        2
    } else if a[0] == 0xfe && a[1] & 0xc0 == 0xc0 {
        5
    } else if *a == std::net::Ipv6Addr::LOCALHOST.octets() {
        1
    } else {
        14
    }
}

fn del_addr6(ns: &Rc<NetNs>, dev: &DevRef, prefix: u8, attrs: &[(u16, &[u8])]) -> Result<(), Fail> {
    let addr = ip6(attrs, IFA_LOCAL)
        .or_else(|| ip6(attrs, IFA_ADDRESS))
        .ok_or(Errno::EINVAL)?;
    let mut x = dev.borrow_mut();
    let i =
        x.v6.iter()
            .position(|a| a.addr == addr && a.prefix == prefix)
            .ok_or(Errno::EADDRNOTAVAIL)?;
    x.v6.remove(i);
    drop(x);
    devs(ns)
        .routes
        .retain(|r| !(r.family == AF_INET6 && r.owner.as_deref() == Some(&addr[..])));
    Ok(())
}

fn ip6(attrs: &[(u16, &[u8])], ty: u16) -> Option<[u8; 16]> {
    get(attrs, ty).and_then(|d| d.try_into().ok())
}

// ---- Routes ----

/// `RTM_NEWROUTE` and `RTM_DELROUTE` (`fib_table_insert`, `fib_table_delete`; for IPv6
/// `ip6_route_add`, `ip6_route_del`): unicast routes only; the table number must be at most 255.
fn route_request(ns: &Rc<NetNs>, req: &[u8]) -> Result<(), Fail> {
    let ty = u16::from_le_bytes([req[4], req[5]]);
    let nlflags = u16::from_le_bytes([req[6], req[7]]);
    let m = req.get(16..28).ok_or(Errno::EINVAL)?;
    let (family, dst_len, tos, table, proto, scope, rtype) =
        (i32::from(m[0]), m[1], m[3], m[4], m[5], m[6], m[7]);
    let onlink = u32::from_le_bytes(m[8..12].try_into().unwrap()) & RTNH_F_ONLINK != 0;
    let attrs = crate::route::attrs(&req[28..]);
    let table = match get_u32(&attrs, RTA_TABLE) {
        Some(t) if t > 255 => return Err(Errno::EOPNOTSUPP.into()),
        Some(t) => t as u8,
        None => table,
    };
    let table = if table == RT_TABLE_UNSPEC {
        RT_TABLE_MAIN
    } else {
        table
    };
    let len = match family {
        AF_INET => 4,
        AF_INET6 => 16,
        _ => return Err(Errno::EOPNOTSUPP.into()),
    };
    let v4 = family == AF_INET;
    if usize::from(dst_len) > len * 8 {
        return Err(fail(Errno::EINVAL, "Invalid prefix length"));
    }
    let addr = |t| -> Result<Option<Vec<u8>>, Fail> {
        match get(&attrs, t) {
            Some(d) if d.len() == len => Ok(Some(d.to_vec())),
            Some(_) => Err(Errno::EINVAL.into()),
            None => Ok(None),
        }
    };
    let mut dst = addr(RTA_DST)?.unwrap_or_else(|| vec![0; len]);
    let gw = addr(RTA_GATEWAY)?;
    let prefsrc = addr(RTA_PREFSRC)?;
    let oif = get_u32(&attrs, RTA_OIF);
    let metric = get_u32(&attrs, RTA_PRIORITY).unwrap_or(0);
    let masked = crate::route::network(&dst, dst_len);
    if ty == RTM_DELROUTE {
        if !v4 {
            dst = masked;
        }
        let mut d = devs(ns);
        let i = d
            .routes
            .iter()
            .position(|r| {
                let idx = r.dev.upgrade().map_or(0, |x| x.borrow().index);
                r.family == family
                    && r.table == table
                    && r.dst == dst
                    && r.prefix == dst_len
                    && (!v4 || r.tos == tos)
                    && (rtype == 0 || r.ty == rtype)
                    && (!v4 || scope == RT_SCOPE_NOWHERE || r.scope == scope)
                    && prefsrc
                        .as_ref()
                        .is_none_or(|p| r.prefsrc.as_ref() == Some(p))
                    && (proto == 0 || r.proto == proto)
                    && (metric == 0 || r.metric == metric)
                    && oif.is_none_or(|o| o == 0 || o == idx)
                    && gw.as_ref().is_none_or(|g| r.gateway.as_ref() == Some(g))
            })
            // Other routes in the initial namespace are the host's and cannot be deleted
            .ok_or(if ns.host_net.is_none() {
                Errno::EOPNOTSUPP
            } else {
                Errno::ESRCH
            })?;
        d.routes.remove(i);
        return Ok(());
    }
    if v4 && masked != dst {
        return Err(fail(
            Errno::EINVAL,
            "Invalid prefix for given prefix length",
        ));
    }
    dst = masked;
    if rtype != RTN_UNICAST {
        return Err(Errno::EOPNOTSUPP.into());
    }
    if v4 && scope > RT_SCOPE_HOST {
        return Err(fail(Errno::EINVAL, "Invalid scope"));
    }
    let dev = nexthop(ns, (family, scope), gw.as_deref(), oif, onlink)?;
    let onlink = onlink && gw.is_some();
    if let Some(src) = &prefsrc {
        let local = interfaces(ns)
            .iter()
            .any(|i| i.addrs.iter().any(|(f, a, _)| *f == family && a == src));
        if !local {
            return Err(fail(Errno::EINVAL, "Invalid prefsrc address"));
        }
    }
    let r = Rt {
        family,
        table,
        ty: RTN_UNICAST,
        proto,
        scope: if v4 { scope } else { RT_SCOPE_UNIVERSE },
        tos: if v4 { tos } else { 0 },
        dst,
        prefix: dst_len,
        prefsrc,
        gateway: gw,
        dev: Rc::downgrade(&dev),
        metric: if !v4 && metric == 0 {
            IP6_PRIO_USER
        } else {
            metric
        },
        onlink,
        owner: None,
    };
    let mut d = devs(ns);
    let same: Vec<usize> = d
        .routes
        .iter()
        .enumerate()
        .filter(|(_, x)| {
            (x.family, x.table, &x.dst, x.prefix, x.tos, x.metric)
                == (r.family, r.table, &r.dst, r.prefix, r.tos, r.metric)
        })
        .map(|(i, _)| i)
        .collect();
    if let (Some(&first), Some(&last)) = (same.first(), same.last()) {
        if nlflags & NLM_F_EXCL != 0 {
            return Err(Errno::EEXIST.into());
        }
        if nlflags & NLM_F_REPLACE != 0 {
            d.routes[first] = r;
            return Ok(());
        }
        let identical = same.iter().any(|&i| {
            let x = &d.routes[i];
            x.ty == r.ty
                && x.scope == r.scope
                && Weak::ptr_eq(&x.dev, &r.dev)
                && x.gateway == r.gateway
                && x.prefsrc == r.prefsrc
        });
        if identical {
            return Err(Errno::EEXIST.into());
        }
        if nlflags & NLM_F_APPEND != 0 {
            d.routes.insert(last + 1, r);
        } else {
            d.routes.insert(first, r);
        }
        return Ok(());
    }
    if nlflags & NLM_F_CREATE == 0 && (v4 || nlflags & NLM_F_REPLACE != 0) {
        return Err(Errno::ENOENT.into());
    }
    d.routes.push(r);
    Ok(())
}

/// A next hop (`fib_check_nh`, `fib6_nh_init`): the interface must be a virtual device (host
/// interfaces and lo are EOPNOTSUPP) and up. With a gateway, the subnet it is in is looked up in
/// this namespace's routes, unless `onlink` (`RTNH_F_ONLINK`, as CNI plugins and VPN scripts add
/// default routes): then the gateway is taken as on the given interface, which is required
/// (ENODEV) and must be up (ENETDOWN); for IPv4 the route's scope must be below link and the
/// gateway must not be a local, broadcast or multicast address (EINVAL, `fib_check_nh_v4_gw`).
fn nexthop(
    ns: &Rc<NetNs>,
    (family, scope): (i32, u8),
    gw: Option<&[u8]>,
    oif: Option<u32>,
    onlink: bool,
) -> Result<DevRef, Fail> {
    let v4 = family == AF_INET;
    let not_up = if v4 {
        "Device for nexthop is not up"
    } else {
        "Nexthop device is not up"
    };
    let oif = oif.filter(|&i| i != 0);
    let device = |i: Option<u32>, missing: Fail, not_up: &'static str| -> Result<DevRef, Fail> {
        let i = i.ok_or(missing)?;
        let dev = devs(ns).by_index(i);
        let Some(dev) = dev else {
            return Err(if base(ns).iter().any(|b| b.index == i) {
                Errno::EOPNOTSUPP
            } else {
                Errno::ENODEV
            }
            .into());
        };
        if !dev.borrow().up {
            return Err(fail(Errno::ENETDOWN, not_up));
        }
        Ok(dev)
    };
    let Some(gw) = gw else {
        return device(oif, Errno::ENODEV.into(), not_up);
    };
    if onlink {
        if v4 && scope >= RT_SCOPE_LINK {
            return Err(fail(Errno::EINVAL, "Nexthop has invalid scope"));
        }
        let dev = device(
            oif,
            fail(Errno::ENODEV, "Nexthop device required for onlink"),
            "Nexthop device is not up",
        )?;
        // inet_addr_type_dev_table: 0.x and 255.255.255.255 are broadcast, 224 to 239 multicast,
        // 127.x and the namespace's own addresses local
        let special = || {
            gw[0] == 0
                || gw == [255; 4]
                || (224..240).contains(&gw[0])
                || gw[0] == 127
                || interfaces(ns)
                    .iter()
                    .any(|i| i.addrs.iter().any(|(f, a, _)| *f == family && a == gw))
        };
        if v4 && special() {
            return Err(fail(Errno::EINVAL, "Nexthop has invalid gateway"));
        }
        return Ok(dev);
    }
    // The gateway's subnet: the longest prefix in the local and main tables through oif (when
    // given) with a scope not less than link
    let want = if v4 {
        scope.saturating_add(1).max(RT_SCOPE_LINK)
    } else {
        0
    };
    let found = entries(ns)
        .into_iter()
        .filter(|e| {
            e.family == family
                && (e.table == RT_TABLE_LOCAL || e.table == RT_TABLE_MAIN)
                && e.scope >= want
                && oif.is_none_or(|o| e.index == o)
                && crate::route::network(gw, e.prefix) == e.dst
        })
        .max_by_key(|e| e.prefix);
    let Some(e) = found else {
        let host = base(ns).iter().any(|i| {
            i.addrs.iter().any(|(f, a, p)| {
                *f == family && crate::route::network(a, *p) == crate::route::network(gw, *p)
            })
        });
        if host && ns.host_net.is_none() {
            return Err(Errno::EOPNOTSUPP.into());
        }
        return Err(if v4 {
            fail(Errno::ENETUNREACH, "Nexthop has invalid gateway")
        } else {
            Errno::ENETUNREACH.into()
        });
    };
    if e.ty != RTN_UNICAST && e.ty != RTN_LOCAL {
        return Err(fail(Errno::EINVAL, "Nexthop has invalid gateway"));
    }
    let dev = devs(ns).by_index(e.index).ok_or(Errno::ENODEV)?;
    if !dev.borrow().up {
        return Err(if v4 {
            Errno::ENETDOWN.into()
        } else {
            fail(Errno::ENETDOWN, not_up)
        });
    }
    Ok(dev)
}

// ---- netlink attributes ----

fn get<'a>(attrs: &[(u16, &'a [u8])], ty: u16) -> Option<&'a [u8]> {
    attrs.iter().find(|(t, _)| *t == ty).map(|(_, d)| *d)
}

fn get_u32(attrs: &[(u16, &[u8])], ty: u16) -> Option<u32> {
    get(attrs, ty)
        .and_then(|d| d.get(..4))
        .map(|d| u32::from_le_bytes(d.try_into().unwrap()))
}

// ---- netlink answers ----

/// `RTM_NEWLINK` for one virtual device (`rtnl_fill_ifinfo`, with the same attribute order).
/// `nsid`: when the peer is in another namespace, the id this namespace gives that one
/// (`IFLA_LINK_NETNSID`).
pub(crate) fn link_message(v: &View, nsid: Option<i32>) -> Vec<u8> {
    let mut p = vec![0u8, 0];
    p.extend_from_slice(&1u16.to_le_bytes());
    p.extend_from_slice(&(v.index as i32).to_le_bytes());
    p.extend_from_slice(&v.flags.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    let mut name = v.name.clone();
    name.push(0);
    attr(&mut p, IFLA_IFNAME, &name);
    attr(&mut p, IFLA_TXQLEN, &v.txqlen.to_le_bytes());
    attr(&mut p, IFLA_OPERSTATE, &[v.operstate]);
    attr(&mut p, IFLA_LINKMODE, &[0]);
    attr(&mut p, IFLA_MTU, &v.mtu.to_le_bytes());
    let (min, max) = v.kind.mtu_range();
    attr(&mut p, IFLA_MIN_MTU, &min.to_le_bytes());
    attr(&mut p, IFLA_MAX_MTU, &max.to_le_bytes());
    attr(&mut p, IFLA_GROUP, &v.group.to_le_bytes());
    attr(
        &mut p,
        IFLA_PROMISCUITY,
        &u32::from(v.port_state.is_some()).to_le_bytes(),
    );
    attr(&mut p, IFLA_NUM_TX_QUEUES, &1u32.to_le_bytes());
    attr(&mut p, IFLA_GSO_MAX_SEGS, &65535u32.to_le_bytes());
    attr(&mut p, IFLA_GSO_MAX_SIZE, &65536u32.to_le_bytes());
    attr(&mut p, IFLA_NUM_RX_QUEUES, &1u32.to_le_bytes());
    attr(&mut p, IFLA_CARRIER, &[u8::from(v.carrier)]);
    attr(
        &mut p,
        IFLA_CARRIER_CHANGES,
        &v.carrier_changes.to_le_bytes(),
    );
    attr(&mut p, IFLA_PROTO_DOWN, &[0]);
    let up = v.carrier_changes.div_ceil(2);
    attr(&mut p, IFLA_CARRIER_UP_COUNT, &up.to_le_bytes());
    attr(
        &mut p,
        IFLA_CARRIER_DOWN_COUNT,
        &(v.carrier_changes - up).to_le_bytes(),
    );
    if let Some((peer, _)) = &v.link
        && *peer != v.index
    {
        attr(&mut p, IFLA_LINK, &peer.to_le_bytes());
    }
    if let Some(m) = v.master {
        attr(&mut p, IFLA_MASTER, &m.to_le_bytes());
    }
    let qdisc: &[u8] = if v.ever_up { b"noqueue\0" } else { b"noop\0" };
    attr(&mut p, IFLA_QDISC, qdisc);
    if let Some(a) = &v.alias {
        let mut a = a.clone();
        a.push(0);
        attr(&mut p, IFLA_IFALIAS, &a);
    }
    attr(&mut p, IFLA_ADDRESS, &v.mac);
    attr(&mut p, IFLA_BROADCAST, &[0xff; 6]);
    attr(&mut p, IFLA_STATS64, &[0u8; 25 * 8]);
    attr(&mut p, IFLA_STATS, &[0u8; 24 * 4]);
    let mut li = Vec::new();
    let mut kind = v.kind.name().as_bytes().to_vec();
    kind.push(0);
    attr(&mut li, IFLA_INFO_KIND, &kind);
    if v.kind == Kind::Bridge {
        attr(&mut li, IFLA_INFO_DATA, &bridge_info(v));
    }
    if let Some(state) = v.port_state {
        attr(&mut li, IFLA_INFO_SLAVE_KIND, b"bridge\0");
        let mut sd = Vec::new();
        attr(&mut sd, 1, &[state]); // IFLA_BRPORT_STATE
        attr(&mut sd, 2, &32u16.to_le_bytes()); // PRIORITY
        attr(&mut sd, 3, &2u32.to_le_bytes()); // COST
        attr(&mut sd, 4, &[0]); // MODE (hairpin)
        attr(&mut sd, 8, &[1]); // LEARNING
        attr(&mut sd, 9, &[1]); // UNICAST_FLOOD
        attr(&mut li, IFLA_INFO_SLAVE_DATA, &sd);
    }
    attr(&mut p, IFLA_LINKINFO, &li);
    if let Some(id) = nsid {
        attr(&mut p, IFLA_LINK_NETNSID, &id.to_le_bytes());
    }
    p
}

/// Bridge parameters (part of `br_fill_info`): all Linux defaults, with STP off.
fn bridge_info(v: &View) -> Vec<u8> {
    let mut d = Vec::new();
    let mut id = vec![0x80, 0x00];
    id.extend_from_slice(&v.bridge_mac);
    attr(&mut d, 1, &1500u32.to_le_bytes()); // IFLA_BR_FORWARD_DELAY
    attr(&mut d, 2, &200u32.to_le_bytes()); // HELLO_TIME
    attr(&mut d, 3, &2000u32.to_le_bytes()); // MAX_AGE
    attr(&mut d, 4, &30000u32.to_le_bytes()); // AGEING_TIME
    attr(&mut d, 5, &0u32.to_le_bytes()); // STP_STATE
    attr(&mut d, 6, &32768u16.to_le_bytes()); // PRIORITY
    attr(&mut d, 7, &[0]); // VLAN_FILTERING
    attr(&mut d, 9, &0u16.to_le_bytes()); // GROUP_FWD_MASK
    attr(&mut d, 11, &id); // BRIDGE_ID
    attr(&mut d, 10, &id); // ROOT_ID
    attr(&mut d, 12, &0u16.to_le_bytes()); // ROOT_PORT
    attr(&mut d, 13, &0u32.to_le_bytes()); // ROOT_PATH_COST
    attr(&mut d, 14, &[0]); // TOPOLOGY_CHANGE
    attr(&mut d, 15, &[0]); // TOPOLOGY_CHANGE_DETECTED
    attr(&mut d, 20, &[0x01, 0x80, 0xc2, 0, 0, 0]); // GROUP_ADDR
    attr(&mut d, 8, &0x8100u16.to_be_bytes()); // VLAN_PROTOCOL
    attr(&mut d, 39, &1u16.to_le_bytes()); // VLAN_DEFAULT_PVID
    d
}

/// `RTM_NEWADDR` messages of one family on a virtual device (`inet_fill_ifaddr`,
/// `inet6_fill_ifaddr`).
pub(crate) fn addr_messages(v: &View, family: i32) -> Vec<Vec<u8>> {
    let mut cacheinfo = [0xffu8; 16];
    cacheinfo[8..].fill(0);
    let mut out = Vec::new();
    if family == AF_INET {
        for a in &v.v4 {
            let mut p = vec![AF_INET as u8, a.prefix, a.flags as u8, a.scope];
            p.extend_from_slice(&v.index.to_le_bytes());
            attr(&mut p, IFA_ADDRESS, &a.address);
            attr(&mut p, IFA_LOCAL, &a.local);
            if a.broadcast != [0; 4] {
                attr(&mut p, IFA_BROADCAST, &a.broadcast);
            }
            let mut l = a.label.clone();
            l.push(0);
            attr(&mut p, IFA_LABEL, &l);
            if a.proto != 0 {
                attr(&mut p, IFA_PROTO, &[a.proto]);
            }
            attr(&mut p, IFA_FLAGS, &a.flags.to_le_bytes());
            if a.metric != 0 {
                attr(&mut p, IFA_RT_PRIORITY, &a.metric.to_le_bytes());
            }
            attr(&mut p, IFA_CACHEINFO, &cacheinfo);
            out.push(p);
        }
    } else {
        for a in &v.v6 {
            let mut p = vec![AF_INET6 as u8, a.prefix, a.flags as u8, scope6(&a.addr)];
            p.extend_from_slice(&v.index.to_le_bytes());
            attr(&mut p, IFA_ADDRESS, &a.addr);
            if a.metric != 0 {
                attr(&mut p, IFA_RT_PRIORITY, &a.metric.to_le_bytes());
            }
            attr(&mut p, IFA_CACHEINFO, &cacheinfo);
            attr(&mut p, IFA_FLAGS, &a.flags.to_le_bytes());
            if a.proto != 0 {
                attr(&mut p, IFA_PROTO, &[a.proto]);
            }
            out.push(p);
        }
    }
    out
}

/// Address scope (`ipv6_addr_scope`): link for link-local, host for ::1, universe otherwise.
fn scope6(a: &[u8; 16]) -> u8 {
    if a[0] == 0xfe && a[1] & 0xc0 == 0x80 {
        RT_SCOPE_LINK
    } else if *a == std::net::Ipv6Addr::LOCALHOST.octets() {
        RT_SCOPE_HOST
    } else {
        RT_SCOPE_UNIVERSE
    }
}

// ---- Namespace ids ----

/// The id `ns` gives `peer`, allocating one if there is none (`peernet2id_alloc`: the lowest
/// unused from 0).
pub(crate) fn nsid_alloc(ns: &NetNs, peer: &Rc<NetNs>) -> i32 {
    let mut d = ns.devs.borrow_mut();
    d.nsids.retain(|(w, _)| w.strong_count() > 0);
    if let Some((_, id)) = d
        .nsids
        .iter()
        .find(|(w, _)| Weak::ptr_eq(w, &Rc::downgrade(peer)))
    {
        return *id;
    }
    let id = (0..)
        .find(|i| !d.nsids.iter().any(|(_, x)| x == i))
        .unwrap();
    d.nsids.push((Rc::downgrade(peer), id));
    id
}

/// The id `ns` gives `peer`; None when none is allocated.
pub(crate) fn nsid(ns: &NetNs, peer: &Rc<NetNs>) -> Option<i32> {
    let d = ns.devs.borrow();
    d.nsids
        .iter()
        .find(|(w, _)| w.strong_count() > 0 && Weak::ptr_eq(w, &Rc::downgrade(peer)))
        .map(|(_, id)| *id)
}

// ---- ioctl ----

/// Interface ioctls (`struct ifreq`, 40 bytes) for virtual devices: handled when a virtual
/// device is found by name (by index for `SIOCGIFNAME`); None means it is not handled here (lo,
/// host interfaces, unknown requests). Changes need `CAP_NET_ADMIN` in the namespace's owner.
pub(crate) fn ioctl(
    ns: &Rc<NetNs>,
    cred: &Cred,
    request: u32,
    ifr: &mut [u8; 40],
) -> Option<Result<(), Errno>> {
    const SIOCGIFNAME: u32 = 0x8910;
    const SIOCGIFFLAGS: u32 = 0x8913;
    const SIOCSIFFLAGS: u32 = 0x8914;
    const SIOCGIFADDR: u32 = 0x8915;
    const SIOCGIFDSTADDR: u32 = 0x8917;
    const SIOCGIFBRDADDR: u32 = 0x8919;
    const SIOCGIFNETMASK: u32 = 0x891b;
    const SIOCGIFMETRIC: u32 = 0x891d;
    const SIOCGIFMTU: u32 = 0x8921;
    const SIOCSIFMTU: u32 = 0x8922;
    const SIOCSIFNAME: u32 = 0x8923;
    const SIOCSIFHWADDR: u32 = 0x8924;
    const SIOCGIFHWADDR: u32 = 0x8927;
    const SIOCGIFINDEX: u32 = 0x8933;
    const SIOCGIFTXQLEN: u32 = 0x8942;
    const SIOCSIFTXQLEN: u32 = 0x8943;
    if request == SIOCGIFNAME {
        let index = i32::from_le_bytes(ifr[16..20].try_into().unwrap());
        let dev = devs(ns).by_index(u32::try_from(index).ok()?)?;
        ifr[..16].fill(0);
        let name = dev.borrow().name.clone();
        ifr[..name.len()].copy_from_slice(&name);
        return Some(Ok(()));
    }
    let known = [
        SIOCGIFFLAGS,
        SIOCSIFFLAGS,
        SIOCGIFADDR,
        SIOCGIFDSTADDR,
        SIOCGIFBRDADDR,
        SIOCGIFNETMASK,
        SIOCGIFMETRIC,
        SIOCGIFMTU,
        SIOCSIFMTU,
        SIOCSIFNAME,
        SIOCSIFHWADDR,
        SIOCGIFHWADDR,
        SIOCGIFINDEX,
        SIOCGIFTXQLEN,
        SIOCSIFTXQLEN,
    ];
    if !known.contains(&request) {
        return None;
    }
    let name = ifr[..16]
        .split(|&c| c == 0)
        .next()
        .unwrap_or_default()
        .to_vec();
    // Address requests look up by label (aliases like `eth0:1`), others by interface name
    let base_name = name.split(|&c| c == b':').next().unwrap_or_default();
    let dev = devs(ns).by_name(base_name)?;
    let setter = matches!(
        request,
        SIOCSIFFLAGS | SIOCSIFMTU | SIOCSIFNAME | SIOCSIFHWADDR | SIOCSIFTXQLEN
    );
    if setter && !cred.ns_capable(&ns.owner, cap::NET_ADMIN) {
        return Some(Err(Errno::EPERM));
    }
    let data = &mut ifr[16..];
    let i32_of = |d: &[u8]| i32::from_le_bytes(d[..4].try_into().unwrap());
    let r = match request {
        SIOCGIFFLAGS => {
            let f = reported_flags(ns, &dev) as u16;
            data[..2].copy_from_slice(&f.to_le_bytes());
            Ok(())
        }
        SIOCSIFFLAGS => {
            // ifr_flags is a short and is sign-extended (dev_change_flags takes an unsigned int)
            let f = i16::from_le_bytes([data[0], data[1]]) as i32 as u32;
            change_flags(ns, &dev, f);
            Ok(())
        }
        SIOCGIFMTU => {
            data[..4].copy_from_slice(&(dev.borrow().mtu as i32).to_le_bytes());
            Ok(())
        }
        SIOCSIFMTU => set_mtu(ns, &dev, i32_of(data) as u32).map_err(|f| Errno(f.errno)),
        SIOCSIFNAME => {
            let new = data[..16]
                .split(|&c| c == 0)
                .next()
                .unwrap_or_default()
                .to_vec();
            rename(ns, &dev, &new).map_err(|f| Errno(f.errno))
        }
        SIOCGIFHWADDR => {
            data[..16].fill(0);
            data[..2].copy_from_slice(&1u16.to_le_bytes());
            data[2..8].copy_from_slice(&dev.borrow().mac);
            Ok(())
        }
        SIOCSIFHWADDR => {
            if u16::from_le_bytes([data[0], data[1]]) != 1 {
                Err(Errno::EINVAL)
            } else {
                let mac = data[2..8].to_vec();
                set_mac(ns, &dev, &mac).map_err(|f| Errno(f.errno))
            }
        }
        SIOCGIFINDEX => {
            data[..4].copy_from_slice(&(dev.borrow().index as i32).to_le_bytes());
            Ok(())
        }
        SIOCGIFTXQLEN => {
            data[..4].copy_from_slice(&(dev.borrow().txqlen as i32).to_le_bytes());
            Ok(())
        }
        SIOCSIFTXQLEN => {
            let q = i32_of(data);
            if q < 0 {
                Err(Errno::EINVAL)
            } else {
                dev.borrow_mut().txqlen = q as u32;
                Ok(())
            }
        }
        SIOCGIFMETRIC => {
            data[..4].fill(0);
            Ok(())
        }
        _ => {
            let x = dev.borrow();
            match x.v4.iter().find(|a| a.label == name) {
                None => Err(Errno::EADDRNOTAVAIL),
                Some(a) => {
                    let value = match request {
                        SIOCGIFADDR => a.local,
                        SIOCGIFDSTADDR => a.address,
                        SIOCGIFBRDADDR => a.broadcast,
                        _ => a.mask().to_be_bytes(),
                    };
                    data[..16].fill(0);
                    data[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
                    data[4..8].copy_from_slice(&value);
                    Ok(())
                }
            }
        }
    };
    if setter {
        settle_around(&dev);
    }
    Some(r)
}

/// Entries for virtual devices in `SIOCGIFCONF`: one ifreq per IPv4 address (named by the
/// address's label).
pub(crate) fn ifconf(ns: &NetNs) -> Vec<u8> {
    let mut out = Vec::new();
    for v in views(ns) {
        for a in &v.v4 {
            let mut r = [0u8; 40];
            let n = a.label.len().min(15);
            r[..n].copy_from_slice(&a.label[..n]);
            r[16..18].copy_from_slice(&(AF_INET as u16).to_le_bytes());
            r[20..24].copy_from_slice(&a.local);
            out.extend_from_slice(&r);
        }
    }
    out
}

// ---- sysctls (crate::netconf) ----

/// A directory under `/proc/sys/net/ipv{4,6}/conf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ConfDir {
    All,
    Default,
    /// An interface, by index.
    Dev(u32),
}

/// The directories under `ipv4/conf` and `ipv6/conf`: `all`, `default` and every interface.
pub(crate) fn conf_dirs(ns: &NetNs) -> Vec<(Vec<u8>, ConfDir)> {
    let mut v = vec![
        (b"all".to_vec(), ConfDir::All),
        (b"default".to_vec(), ConfDir::Default),
    ];
    v.extend(
        interfaces(ns)
            .into_iter()
            .map(|i| (i.name.into_bytes(), ConfDir::Dev(i.index))),
    );
    v
}

/// A directory's name (None when the interface is gone).
pub(crate) fn conf_dir_name(ns: &NetNs, dir: ConfDir) -> Option<Vec<u8>> {
    conf_dirs(ns)
        .into_iter()
        .find(|(_, d)| *d == dir)
        .map(|(n, _)| n)
}

/// Reads one interface setting.
pub(crate) fn conf_read(ns: &NetNs, v6: bool, dir: ConfDir, name: &[u8]) -> Result<String, Errno> {
    let i = crate::netconf::position(v6, name).ok_or(Errno::ENOENT)?;
    let (c, mtu) = conf_of(ns, v6, dir)?;
    if v6 && name == b"stable_secret" {
        return crate::netconf::show_secret(c.secret);
    }
    if v6 && name == b"mtu" {
        return Ok(format!(
            "{}\n",
            c.mtu6.or(mtu.map(i64::from)).unwrap_or(c.vals[i])
        ));
    }
    Ok(format!("{}\n", c.vals[i]))
}

/// Writes one interface setting (the entries the module docs of [`crate::netconf`] mention take
/// effect).
pub(crate) fn conf_write(
    ns: &NetNs,
    v6: bool,
    dir: ConfDir,
    name: &[u8],
    data: &[u8],
) -> Result<(), Errno> {
    let i = crate::netconf::position(v6, name).ok_or(Errno::ENOENT)?;
    let key = std::str::from_utf8(name).unwrap_or_default();
    if v6 && name == b"stable_secret" {
        let s = crate::netconf::parse_secret(data)?;
        return conf_apply(ns, v6, dir, |c| c.secret = Some(s));
    }
    let v = crate::netconf::parse_int(data)?;
    crate::netconf::check(v6, key, v)?;
    if v6
        && name == b"mtu"
        && let ConfDir::Dev(_) = dir
    {
        let (_, mtu) = conf_of(ns, v6, dir)?;
        if v > i64::from(mtu.unwrap_or(0)) {
            return Err(Errno::EINVAL);
        }
        return conf_apply(ns, v6, dir, |c| c.mtu6 = Some(v));
    }
    let set = |c: &mut Conf| {
        c.vals[i] = v;
        c.set[i] = true;
    };
    match dir {
        ConfDir::Dev(_) => conf_apply(ns, v6, dir, set)?,
        ConfDir::Default => {
            conf_apply(ns, v6, dir, set)?;
            // IPv4 default is applied to the interfaces that have not set this entry
            // individually (devinet_copy_dflt_conf)
            if !v6 {
                for_each_conf(ns, false, |c| {
                    if !c.set[i] {
                        c.vals[i] = v;
                    }
                });
            }
        }
        ConfDir::All => {
            conf_apply(ns, v6, dir, set)?;
            // forwarding, and IPv6 disable_ipv6, written on all are applied to default and every
            // interface
            if name == b"forwarding" || (v6 && name == b"disable_ipv6") {
                conf_apply(ns, v6, ConfDir::Default, |c| c.vals[i] = v)?;
                for_each_conf(ns, v6, |c| c.vals[i] = v);
            }
        }
    }
    if v6 && name == b"disable_ipv6" {
        ipv6_switch(ns);
    }
    Ok(())
}

/// A set of settings and the interface's MTU (for IPv6's `mtu`).
fn conf_of(ns: &NetNs, v6: bool, dir: ConfDir) -> Result<(Conf, Option<u32>), Errno> {
    let mut d = devs(ns);
    let pick = |c4: &Conf, c6: &Conf| if v6 { c6.clone() } else { c4.clone() };
    Ok(match dir {
        ConfDir::All => (pick(&d.conf.all4, &d.conf.all6), None),
        ConfDir::Default => (pick(&d.conf.dflt4, &d.conf.dflt6), None),
        ConfDir::Dev(i) => {
            if let Some(x) = d.by_index(i) {
                let x = x.borrow();
                (pick(&x.conf4, &x.conf6), Some(x.mtu))
            } else {
                let b = base(ns)
                    .into_iter()
                    .find(|b| b.index == i)
                    .ok_or(Errno::ENODEV)?;
                let (c4, c6) = d.conf.other_mut(i).clone();
                (pick(&c4, &c6), Some(b.mtu))
            }
        }
    })
}

/// Changes one set of settings.
fn conf_apply(ns: &NetNs, v6: bool, dir: ConfDir, f: impl FnOnce(&mut Conf)) -> Result<(), Errno> {
    fn pick<'a>(v6: bool, c4: &'a mut Conf, c6: &'a mut Conf) -> &'a mut Conf {
        if v6 { c6 } else { c4 }
    }
    let mut d = devs(ns);
    match dir {
        ConfDir::All => {
            let NetConf { all4, all6, .. } = &mut d.conf;
            f(pick(v6, all4, all6));
        }
        ConfDir::Default => {
            let NetConf { dflt4, dflt6, .. } = &mut d.conf;
            f(pick(v6, dflt4, dflt6));
        }
        ConfDir::Dev(i) => {
            if let Some(x) = d.by_index(i) {
                let mut x = x.borrow_mut();
                let Dev { conf4, conf6, .. } = &mut *x;
                f(pick(v6, conf4, conf6));
            } else {
                if !base(ns).iter().any(|b| b.index == i) {
                    return Err(Errno::ENODEV);
                }
                let (c4, c6) = d.conf.other_mut(i);
                f(pick(v6, c4, c6));
            }
        }
    }
    Ok(())
}

/// Changes the settings of every interface (virtual devices', and lo's and host interfaces').
fn for_each_conf(ns: &NetNs, v6: bool, mut f: impl FnMut(&mut Conf)) {
    let mut d = devs(ns);
    for x in &d.list {
        let mut x = x.borrow_mut();
        f(if v6 { &mut x.conf6 } else { &mut x.conf4 });
    }
    for b in base(ns) {
        let (c4, c6) = d.conf.other_mut(b.index);
        f(if v6 { c6 } else { c4 });
    }
}

/// After `disable_ipv6` changes: interfaces with IPv6 turned off lose it, and those with IPv6
/// turned on that are up with carrier are configured again; in a new namespace lo's ::1
/// follows.
fn ipv6_switch(ns: &NetNs) {
    {
        let mut d = devs(ns);
        let list = d.list.clone();
        for x in &list {
            if v6_disabled(&x.borrow()) && x.borrow().v6_ready {
                v6_down(&mut d, x);
            } else if v6_disabled(&x.borrow()) {
                x.borrow_mut().v6.clear();
            }
        }
    }
    settle(ns, &[]);
    if ns.host_net.is_some() {
        let off = {
            let mut d = devs(ns);
            d.conf
                .other_mut(crate::netns::LO_INDEX)
                .1
                .get(true, "disable_ipv6")
                != 0
        };
        let mut lo = ns.lo.borrow_mut();
        lo.v6 = lo.up && !off;
    }
}

/// `ipv4/ip_forward` (an alias of `all/forwarding`).
pub(crate) fn ip_forward(ns: &NetNs) -> i64 {
    ns.devs.borrow().conf.all4.get(false, "forwarding")
}

/// For tests: creates an up dummy device in `ns` with address `addr`/`prefix`, and when a gateway
/// is given, also a default route through it.
#[cfg(test)]
pub(crate) fn test_dev(ns: &Rc<NetNs>, name: &str, addr: [u8; 4], prefix: u8, gw: Option<[u8; 4]>) {
    let none = |_: u16, _: u32| -> Result<Rc<NetNs>, Errno> { Err(Errno::ESRCH) };
    let root = Cred::root();
    let send = |ty: u16, flags: u16, body: Vec<u8>| {
        let req = crate::net::message(ty, flags | 1, 1, 0, &body);
        request(ns, &root, &req, &none).unwrap().unwrap();
    };
    let mut link = vec![0u8; 16];
    link[8..12].copy_from_slice(&IFF_UP.to_le_bytes());
    link[12..16].copy_from_slice(&IFF_UP.to_le_bytes());
    let mut n = name.as_bytes().to_vec();
    n.push(0);
    attr(&mut link, IFLA_IFNAME, &n);
    let mut li = Vec::new();
    attr(&mut li, IFLA_INFO_KIND, b"dummy");
    attr(&mut link, IFLA_LINKINFO, &li);
    send(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, link);
    let index = views(ns)
        .iter()
        .find(|v| v.name == name.as_bytes())
        .unwrap()
        .index;
    let mut a = vec![AF_INET as u8, prefix, 0, 0];
    a.extend_from_slice(&index.to_le_bytes());
    attr(&mut a, IFA_LOCAL, &addr);
    send(RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, a);
    if let Some(g) = gw {
        let mut r = vec![
            AF_INET as u8,
            0,
            0,
            0,
            RT_TABLE_MAIN,
            3,
            0,
            RTN_UNICAST,
            0,
            0,
            0,
            0,
        ];
        attr(&mut r, RTA_GATEWAY, &g);
        send(RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, r);
    }
}

// ---- Carrier changes and IPv6 configuration ----

/// Advances the namespaces of `dev` and of its peer.
fn settle_around(dev: &DevRef) {
    let first = related(dev);
    let mut nss: Vec<Rc<NetNs>> = Vec::new();
    for x in &first {
        if let Some(n) = x.borrow().ns.upgrade()
            && !nss.iter().any(|m| Rc::ptr_eq(m, &n))
        {
            nss.push(n);
        }
    }
    for n in &nss {
        settle(n, &first);
    }
}

/// `dev` and those connected to it: its peer and the bridge it is attached to (the order for
/// [`settle`]).
fn related(dev: &DevRef) -> Vec<DevRef> {
    let x = dev.borrow();
    let mut v = vec![dev.clone()];
    v.extend(x.peer.upgrade());
    v.extend(x.master.upgrade());
    v
}

/// Advances after a state change: counts carrier changes, and configures IPv6 on interfaces that
/// are up with carrier and IPv6 not disabled. Those in `first` are handled first (Linux goes by
/// the order of events: the changed one, its peer, the bridge it is attached to).
fn settle(ns: &NetNs, first: &[DevRef]) {
    let mut d = devs(ns);
    let mut order: Vec<DevRef> = Vec::new();
    for x in first.iter().chain(d.list.clone().iter()) {
        let mine = x
            .borrow()
            .ns
            .upgrade()
            .is_some_and(|n| std::ptr::eq(Rc::as_ptr(&n), ns));
        if mine && !order.iter().any(|o| Rc::ptr_eq(o, x)) {
            order.push(x.clone());
        }
    }
    for x in &order {
        let c = carrier(x, &d);
        let ready = {
            let mut y = x.borrow_mut();
            if c != y.carrier_was {
                y.carrier_was = c;
                y.carrier_changes += 1;
            }
            y.up && c && !y.v6_ready && !v6_disabled(&y)
        };
        if ready {
            v6_config(&mut d, x);
        }
    }
}

/// Whether the interface has IPv6 off: its own `disable_ipv6`, or an MTU below 1280.
fn v6_disabled(x: &Dev) -> bool {
    x.conf6.get(true, "disable_ipv6") != 0 || i64::from(x.mtu) < crate::netconf::IPV6_MIN_MTU
}

/// Configures an interface's IPv6 (addrconf's `addrconf_dev_config`): routes for ff00::/8 and
/// fe80::/64, a generated link-local address (none when `addr_gen_mode` is 1), and routes for
/// existing addresses.
fn v6_config(d: &mut Devices, dev: &DevRef) {
    let (mac, mode) = {
        let mut x = dev.borrow_mut();
        x.v6_ready = true;
        (x.mac, x.conf6.get(true, "addr_gen_mode"))
    };
    let r = |table, ty, dst: Vec<u8>, prefix| {
        kernel_route(
            AF_INET6,
            table,
            ty,
            RT_SCOPE_UNIVERSE,
            dst,
            prefix,
            None,
            dev,
            IP6_PRIO_ADDRCONF,
            Vec::new(),
        )
    };
    let mut mc = vec![0u8; 16];
    mc[0] = 0xff;
    add_route(d, r(RT_TABLE_LOCAL, RTN_MULTICAST, mc, 8));
    let mut ll = vec![0u8; 16];
    ll[..2].copy_from_slice(&[0xfe, 0x80]);
    add_route(d, r(RT_TABLE_MAIN, RTN_UNICAST, ll, 64));
    if mode != 1 {
        let a = Addr6 {
            addr: link_local(mac),
            prefix: 64,
            flags: IFA_F_PERMANENT,
            proto: IFAPROT_KERNEL_LL,
            metric: 0,
        };
        let mut x = dev.borrow_mut();
        if !x.v6.iter().any(|b| b.addr == a.addr) {
            x.v6.push(a);
        }
    }
    let addrs = dev.borrow().v6.clone();
    for a in &addrs {
        add_ifaddr6(d, dev, a);
    }
}

/// The link-local address generated from the MAC (EUI-64).
fn link_local(mac: [u8; 6]) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[0] = 0xfe;
    a[1] = 0x80;
    a[8] = mac[0] ^ 0x02;
    a[9] = mac[1];
    a[10] = mac[2];
    a[11] = 0xff;
    a[12] = 0xfe;
    a[13] = mac[3];
    a[14] = mac[4];
    a[15] = mac[5];
    a
}

/// Removes an interface's IPv6 (when going down, disabling IPv6 or moving away,
/// `addrconf_ifdown`): addresses and routes are all removed.
fn v6_down(d: &mut Devices, dev: &DevRef) {
    {
        let mut x = dev.borrow_mut();
        x.v6.clear();
        x.v6_ready = false;
    }
    drop_routes(d, dev, Some(AF_INET6), false);
}

// ---- Adding and removing routes (added per address) ----

/// Rebuilds the routes added per address after IPv4 addresses change: removes those of `gone`
/// (including those whose preferred source it is, `fib_sync_down_addr`), then adds them back for
/// the remaining addresses (`fib_del_ifaddr` keeps shared ones).
fn redo_ifaddr4(d: &mut Devices, dev: &DevRef, gone: &[[u8; 4]]) {
    d.routes.retain(|r| {
        !(r.family == AF_INET
            && gone.iter().any(|g| {
                r.owner.as_deref() == Some(&g[..]) || r.prefsrc.as_deref() == Some(&g[..])
            }))
    });
    let addrs = dev.borrow().v4.clone();
    for a in &addrs {
        add_ifaddr4(d, dev, a);
    }
}

/// The routes of IPv4 address `a` (`fib_add_ifaddr`): the local route is always added; an
/// explicit broadcast address, the subnet and the subnet broadcast only while the interface is
/// up; a secondary's preferred source is the primary in the same subnet.
fn add_ifaddr4(d: &mut Devices, dev: &DevRef, a: &Addr4) {
    let (up, prim) = {
        let x = dev.borrow();
        let prim = if a.flags & IFA_F_SECONDARY != 0 {
            x.v4.iter()
                .find(|p| {
                    p.flags & IFA_F_SECONDARY == 0 && p.prefix == a.prefix && p.contains(a.local)
                })
                .map_or(a.local, |p| p.local)
        } else {
            a.local
        };
        (x.up, prim)
    };
    let src = Some(prim.to_vec());
    let own = a.local.to_vec();
    let host = |ty, scope, dst: [u8; 4]| {
        kernel_route(
            AF_INET,
            RT_TABLE_LOCAL,
            ty,
            scope,
            dst.to_vec(),
            32,
            src.clone(),
            dev,
            0,
            own.clone(),
        )
    };
    add_route(d, host(RTN_LOCAL, RT_SCOPE_HOST, a.local));
    if !up {
        return;
    }
    if a.broadcast != [0; 4] && a.broadcast != [0xff; 4] {
        add_route(d, host(RTN_BROADCAST, RT_SCOPE_LINK, a.broadcast));
    }
    let prefix = a.subnet();
    if prefix != 0
        && a.flags & IFA_F_SECONDARY == 0
        && (prefix != u32::from_be_bytes(a.local) || a.prefix < 32)
    {
        if a.flags & IFA_F_NOPREFIXROUTE == 0 {
            add_route(
                d,
                kernel_route(
                    AF_INET,
                    RT_TABLE_MAIN,
                    RTN_UNICAST,
                    RT_SCOPE_LINK,
                    prefix.to_be_bytes().to_vec(),
                    a.prefix,
                    src.clone(),
                    dev,
                    a.metric,
                    own.clone(),
                ),
            );
        }
        if a.prefix < 31 {
            add_route(
                d,
                host(
                    RTN_BROADCAST,
                    RT_SCOPE_LINK,
                    (prefix | !a.mask()).to_be_bytes(),
                ),
            );
        }
    }
}

/// The routes of an IPv6 address: the subnet (none with `IFA_F_NOPREFIXROUTE`, nor for /128) and
/// the local route (while the interface is up).
fn add_ifaddr6(d: &mut Devices, dev: &DevRef, a: &Addr6) {
    let up = dev.borrow().up;
    let own = a.addr.to_vec();
    if a.flags & IFA_F_NOPREFIXROUTE == 0 && a.prefix < 128 {
        add_route(
            d,
            kernel_route(
                AF_INET6,
                RT_TABLE_MAIN,
                RTN_UNICAST,
                RT_SCOPE_UNIVERSE,
                crate::route::network(&a.addr, a.prefix),
                a.prefix,
                None,
                dev,
                if a.metric != 0 {
                    a.metric
                } else {
                    IP6_PRIO_ADDRCONF
                },
                own.clone(),
            ),
        );
    }
    if up {
        add_route(
            d,
            kernel_route(
                AF_INET6,
                RT_TABLE_LOCAL,
                RTN_LOCAL,
                RT_SCOPE_UNIVERSE,
                own.clone(),
                128,
                None,
                dev,
                0,
                own,
            ),
        );
    }
}

fn add_route(d: &mut Devices, r: Rt) {
    if !d.routes.iter().any(|x| x.same(&r)) {
        d.routes.push(r);
    }
}

/// A route added per address (by `fib_magic` or addrconf).
#[allow(clippy::too_many_arguments)]
fn kernel_route(
    family: i32,
    table: u8,
    ty: u8,
    scope: u8,
    dst: Vec<u8>,
    prefix: u8,
    prefsrc: Option<Vec<u8>>,
    dev: &DevRef,
    metric: u32,
    owner: Vec<u8>,
) -> Rt {
    Rt {
        family,
        table,
        ty,
        proto: RTPROT_KERNEL,
        scope,
        tos: 0,
        dst,
        prefix,
        prefsrc,
        gateway: None,
        dev: Rc::downgrade(dev),
        metric,
        onlink: false,
        owner: Some(owner),
    }
}

/// Removes the routes through `dev`: with `keep_local`, IPv4 local routes stay (when the
/// interface goes down).
fn drop_routes(d: &mut Devices, dev: &DevRef, family: Option<i32>, keep_local: bool) {
    d.routes.retain(|r| {
        !(r.on(dev)
            && family.is_none_or(|f| r.family == f)
            && !(keep_local && r.family == AF_INET && r.ty == RTN_LOCAL))
    });
}

// ---- Carrier and state ----

/// A snapshot of a device (for netlink answers, procfs and sysfs).
#[derive(Debug, Clone)]
pub(crate) struct View {
    pub index: u32,
    pub name: Vec<u8>,
    pub kind: Kind,
    /// Reported flags (`dev_get_flags`: including RUNNING and LOWER_UP).
    pub flags: u32,
    pub carrier: bool,
    pub operstate: u8,
    pub mtu: u32,
    pub mac: [u8; 6],
    pub txqlen: u32,
    pub group: u32,
    pub alias: Option<Vec<u8>>,
    /// `IFLA_LINK`: the index of the veth's peer (with its namespace when it is not in the same
    /// one).
    pub link: Option<(u32, Option<Rc<NetNs>>)>,
    pub master: Option<u32>,
    pub port_state: Option<u8>,
    pub carrier_changes: u32,
    pub v4: Vec<Addr4>,
    pub v6: Vec<Addr6>,
    /// The MAC used for the bridge id (`bridge_id`).
    pub bridge_mac: [u8; 6],
    /// Has been up (the qdisc is noqueue).
    pub ever_up: bool,
}

impl View {
    /// Viewed as an interface, the way `ip` and procfs use it.
    pub fn interface(&self) -> Interface {
        let mut addrs: Vec<(i32, Vec<u8>, u8)> = self
            .v4
            .iter()
            .map(|a| (AF_INET, a.local.to_vec(), a.prefix))
            .collect();
        addrs.extend(
            self.v6
                .iter()
                .map(|a| (AF_INET6, a.addr.to_vec(), a.prefix)),
        );
        Interface {
            name: String::from_utf8_lossy(&self.name).into_owned(),
            index: self.index,
            flags: self.flags,
            mtu: self.mtu,
            mac: Some(self.mac),
            addrs,
        }
    }
}

/// The virtual devices in namespace `ns`, by index.
pub(crate) fn views(ns: &NetNs) -> Vec<View> {
    let d = devs(ns);
    let mut v: Vec<View> = d.list.iter().map(|x| view(x, &d, ns)).collect();
    v.sort_by_key(|x| x.index);
    v
}

fn view(me: &DevRef, devs: &Devices, my_ns: &NetNs) -> View {
    let c = carrier(me, devs);
    let port = port_state(me, devs);
    let d = me.borrow();
    let link = match d.kind {
        Kind::Veth => Some(match d.peer.upgrade() {
            Some(p) => {
                let p = p.borrow();
                let other =
                    p.ns.upgrade()
                        .filter(|n| !std::ptr::eq(Rc::as_ptr(n), my_ns));
                (p.index, other)
            }
            None => (0, None),
        }),
        _ => None,
    };
    let iflink = link.as_ref().map_or(d.index, |l| l.0);
    let operstate = if !d.up {
        IF_OPER_DOWN
    } else if !c {
        if iflink != d.index {
            IF_OPER_LOWERLAYERDOWN
        } else {
            IF_OPER_DOWN
        }
    } else if d.kind == Kind::Dummy || (d.kind == Kind::Bridge && !d.selected) {
        IF_OPER_UNKNOWN
    } else {
        IF_OPER_UP
    };
    let mut flags = d.flags;
    if d.up {
        flags |= IFF_UP;
        if matches!(operstate, IF_OPER_UP | IF_OPER_UNKNOWN) {
            flags |= IFF_RUNNING;
        }
        if c {
            flags |= IFF_LOWER_UP;
        }
    }
    View {
        index: d.index,
        name: d.name.clone(),
        kind: d.kind,
        flags,
        carrier: c,
        operstate,
        mtu: d.mtu,
        mac: d.mac,
        txqlen: d.txqlen,
        group: d.group,
        alias: d.alias.clone(),
        link,
        master: d.master.upgrade().map(|m| m.borrow().index),
        port_state: port,
        carrier_changes: d.carrier_changes,
        v4: d.v4.clone(),
        v6: d.v6.clone(),
        bridge_mac: d.mac,
        ever_up: d.ever_up,
    }
}

/// A port's state (`BR_STATE_*`): forwarding when the bridge and the port are both up and the
/// port has carrier (STP off), disabled otherwise.
fn port_state(port: &DevRef, devs: &Devices) -> Option<u8> {
    let br = port.borrow().master.upgrade()?;
    let on = br.borrow().up && port.borrow().up && carrier(port, devs);
    Some(if on { 3 } else { 0 })
}

/// The IPv4 addresses and prefix lengths on virtual devices in namespace `ns` (for address
/// mapping, [`crate::netns`]).
pub(crate) fn local_v4(ns: &NetNs) -> Vec<([u8; 4], u8)> {
    let d = devs(ns);
    let mut v = Vec::new();
    for x in &d.list {
        v.extend(x.borrow().v4.iter().map(|a| (a.local, a.prefix)));
    }
    v
}

/// The route in namespace `ns` through virtual devices to IPv4 address `ip` (the longest prefix
/// in the main table; among equal lengths, the lowest metric).
pub(crate) fn route_v4(ns: &NetNs, ip: [u8; 4]) -> Option<Entry> {
    entries(ns)
        .into_iter()
        .filter(|e| {
            e.family == AF_INET
                && e.table == RT_TABLE_MAIN
                && crate::route::network(&ip, e.prefix) == e.dst
        })
        .min_by_key(|e| (std::cmp::Reverse(e.prefix), e.metric))
}

/// Whether namespace `ns` has an IPv6 route to `ip` through virtual devices.
pub(crate) fn routes_v6(ns: &NetNs, ip: &[u8; 16]) -> bool {
    entries(ns).iter().any(|e| {
        e.family == AF_INET6
            && e.table == RT_TABLE_MAIN
            && crate::route::network(ip, e.prefix) == e.dst
    })
}

/// The first primary IPv4 address on the virtual device with index `index` in namespace `ns`.
pub(crate) fn first_v4(ns: &NetNs, index: u32) -> Option<[u8; 4]> {
    let d = devs(ns);
    let x = d.by_index(index)?;
    let x = x.borrow();
    x.v4.iter()
        .find(|a| a.flags & IFA_F_SECONDARY == 0)
        .map(|a| a.local)
}

/// All interfaces in namespace `ns` (the host's or lo, plus virtual devices), by index.
pub(crate) fn interfaces(ns: &NetNs) -> Vec<Interface> {
    let mut all = base(ns);
    all.extend(views(ns).iter().map(View::interface));
    all.sort_by_key(|i| i.index);
    all
}

/// The two tables of namespace `ns` (the host's or lo's, plus those on virtual devices).
/// `host_routes` are the routes in the host's main table (the initial namespace,
/// [`crate::route::host_routes`]).
pub(crate) fn tables(ns: &NetNs, host_routes: Option<Vec<tuxport_hal::net::Route>>) -> Vec<Entry> {
    let base_ifaces = base(ns);
    let host = if ns.host_net.is_none() {
        crate::route::tables(&base_ifaces, host_routes)
    } else {
        // A new namespace: lo's local routes (there is no host main table)
        crate::route::tables(&base_ifaces, Some(Vec::new()))
            .into_iter()
            .map(|mut e| {
                e.host = false;
                e
            })
            .collect()
    };
    crate::route::merge(host, entries(ns))
}

/// Routes on virtual devices ([`Entry`] with `host` false), in the order added.
pub(crate) fn entries(ns: &NetNs) -> Vec<Entry> {
    let d = devs(ns);
    d.routes
        .iter()
        .filter_map(|r| {
            let dev = r.dev.upgrade()?;
            let linkdown = r.ty != RTN_LOCAL && !carrier(&dev, &d);
            let x = dev.borrow();
            Some(Entry {
                family: r.family,
                table: r.table,
                ty: r.ty,
                proto: r.proto,
                scope: r.scope,
                dst: r.dst.clone(),
                prefix: r.prefix,
                prefsrc: r.prefsrc.clone(),
                gateway: r.gateway.clone(),
                iface: String::from_utf8_lossy(&x.name).into_owned(),
                index: x.index,
                metric: r.metric,
                flags: if linkdown { RTNH_F_LINKDOWN } else { 0 }
                    | if r.onlink { RTNH_F_ONLINK } else { 0 },
                host: false,
            })
        })
        .collect()
}

/// Whether the device has carrier (`netif_carrier_ok`). `devs` is the device table of its
/// namespace (to find a bridge's ports).
fn carrier(me: &DevRef, devs: &Devices) -> bool {
    let d = me.borrow();
    match d.kind {
        Kind::Dummy => true,
        Kind::Veth => d.up && d.peer.upgrade().is_some_and(|p| p.borrow().up),
        Kind::Bridge if !d.selected => true,
        Kind::Bridge => {
            d.up && devs
                .ports(me)
                .iter()
                .any(|p| p.borrow().up && carrier(p, devs))
        }
    }
}

// ---- Device table ----

/// The device table of namespace `ns`, after first clearing veths whose peer is gone.
fn devs(ns: &NetNs) -> crate::cell::RefMut<'_, Devices> {
    let mut d = ns.devs.borrow_mut();
    d.prune();
    d
}

/// The interfaces in a namespace that are not virtual devices: the host's in the initial
/// namespace, lo in a new one.
pub(crate) fn base(ns: &NetNs) -> Vec<Interface> {
    if ns.host_net.is_none() {
        tuxport_hal::net::interfaces()
    } else {
        vec![crate::netns::lo_interface(ns)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::message;

    fn ns(b: u8) -> Rc<NetNs> {
        Rc::new(NetNs {
            id: u64::from(b),
            owner: crate::cred::UserNs::init(),
            host_net: Some([9, b]),
            lo: Default::default(),
            devs: RefCell::new(Devices::for_new_ns(&NetConf::default())),
            nft: Default::default(),
            listeners: Default::default(),
        })
    }

    fn nested(ty: u16, children: &[(u16, Vec<u8>)]) -> (u16, Vec<u8>) {
        let mut b = Vec::new();
        for (t, d) in children {
            attr(&mut b, *t, d);
        }
        (ty, b)
    }

    fn link(
        ty: u16,
        flags: u16,
        index: i32,
        up: Option<bool>,
        attrs: &[(u16, Vec<u8>)],
    ) -> Vec<u8> {
        let mut p = vec![0u8; 16];
        p[4..8].copy_from_slice(&index.to_le_bytes());
        if let Some(u) = up {
            p[8..12].copy_from_slice(&u32::from(u).to_le_bytes());
            p[12..16].copy_from_slice(&IFF_UP.to_le_bytes());
        }
        for (t, d) in attrs {
            attr(&mut p, *t, d);
        }
        message(ty, flags | 1, 1, 0, &p)
    }

    fn name(n: &str) -> (u16, Vec<u8>) {
        let mut v = n.as_bytes().to_vec();
        v.push(0);
        (IFLA_IFNAME, v)
    }

    fn kind(k: &str) -> (u16, Vec<u8>) {
        nested(IFLA_LINKINFO, &[(IFLA_INFO_KIND, k.as_bytes().to_vec())])
    }

    fn run(n: &Rc<NetNs>, req: &[u8], others: &[Rc<NetNs>]) -> Result<(), Fail> {
        let resolve = |_: u16, v: u32| -> Result<Rc<NetNs>, Errno> {
            others
                .iter()
                .find(|o| o.id == u64::from(v))
                .cloned()
                .ok_or(Errno::ESRCH)
        };
        request(n, &Cred::root(), req, &resolve).unwrap()
    }

    fn up(n: &Rc<NetNs>, dev: &str, on: bool) {
        run(n, &link(RTM_SETLINK, 0, 0, Some(on), &[name(dev)]), &[]).unwrap();
    }

    fn view_of(n: &NetNs, dev: &str) -> View {
        views(n)
            .into_iter()
            .find(|v| v.name == dev.as_bytes())
            .unwrap()
    }

    const CREATE: u16 = NLM_F_CREATE | NLM_F_EXCL;

    #[test]
    fn veth_peer_in_another_namespace() {
        let (a, b) = (ns(1), ns(2));
        let mut peer = vec![0u8; 16];
        let (t, d) = name("eth0");
        attr(&mut peer, t, &d);
        attr(&mut peer, IFLA_NET_NS_PID, &2u32.to_le_bytes());
        let info = nested(
            IFLA_LINKINFO,
            &[
                (IFLA_INFO_KIND, b"veth".to_vec()),
                (IFLA_INFO_DATA, {
                    let mut x = Vec::new();
                    attr(&mut x, VETH_INFO_PEER, &peer);
                    x
                }),
            ],
        );
        let req = link(RTM_NEWLINK, CREATE, 0, None, &[name("v0"), info]);
        run(&a, &req, std::slice::from_ref(&b)).unwrap();
        // The peer registers first: it is 2 in b (after lo), and this end is 2 in a as well
        let (va, vb) = (view_of(&a, "v0"), view_of(&b, "eth0"));
        assert_eq!((va.index, vb.index), (2, 2));
        assert_eq!(va.link.as_ref().unwrap().0, 2);
        assert!(va.link.as_ref().unwrap().1.is_some());
        // Carrier only when both ends are up; with only one end up, both ends have the same
        // index, so it is DOWN rather than LOWERLAYERDOWN
        up(&a, "v0", true);
        assert_eq!(view_of(&a, "v0").operstate, IF_OPER_DOWN);
        up(&b, "eth0", true);
        assert_eq!(view_of(&a, "v0").operstate, IF_OPER_UP);
        assert!(view_of(&b, "eth0").carrier);
        // IPv6 configured: there is a link-local address
        assert_eq!(view_of(&b, "eth0").v6.len(), 1);
        assert_eq!(nsid_alloc(&a, &b), 0);
        assert_eq!(nsid(&a, &b), Some(0));
        // One with the same name cannot be created; once b is gone, v0 is gone too (the snapshot
        // holds the peer's namespace, so drop it first)
        assert_eq!(
            run(&a, &req, std::slice::from_ref(&b)).unwrap_err().errno,
            Errno::EEXIST.0
        );
        drop((va, vb, b));
        assert!(views(&a).is_empty());
    }

    #[test]
    fn bridge_carrier_mtu_and_mac_follow_ports() {
        let n = ns(3);
        run(
            &n,
            &link(RTM_NEWLINK, CREATE, 0, None, &[name("br0"), kind("bridge")]),
            &[],
        )
        .unwrap();
        up(&n, "br0", true);
        // No ports yet: carrier, state UNKNOWN
        let v = view_of(&n, "br0");
        assert!(v.carrier && v.operstate == IF_OPER_UNKNOWN);
        let mut peer = vec![0u8; 16];
        let (t, d) = name("v1");
        attr(&mut peer, t, &d);
        attr(&mut peer, IFLA_MTU, &1400u32.to_le_bytes());
        let info = nested(
            IFLA_LINKINFO,
            &[
                (IFLA_INFO_KIND, b"veth".to_vec()),
                (IFLA_INFO_DATA, {
                    let mut x = Vec::new();
                    attr(&mut x, VETH_INFO_PEER, &peer);
                    x
                }),
            ],
        );
        let br = view_of(&n, "br0").index;
        let req = link(
            RTM_NEWLINK,
            CREATE,
            0,
            None,
            &[
                name("v0"),
                (IFLA_MTU, 1300u32.to_le_bytes().to_vec()),
                (IFLA_MASTER, br.to_le_bytes().to_vec()),
                info,
            ],
        );
        run(&n, &req, &[]).unwrap();
        let v = view_of(&n, "br0");
        assert!(!v.carrier && v.operstate == IF_OPER_DOWN);
        assert_eq!(v.mtu, 1300);
        assert_eq!(v.mac, view_of(&n, "v0").mac);
        assert_eq!(view_of(&n, "v0").port_state, Some(0));
        up(&n, "v0", true);
        up(&n, "v1", true);
        assert!(view_of(&n, "br0").carrier);
        assert_eq!(view_of(&n, "v0").port_state, Some(3));
        // A bridge cannot be attached to a bridge, nor moved away
        let me = link(
            RTM_SETLINK,
            0,
            0,
            None,
            &[name("br0"), (IFLA_MASTER, br.to_le_bytes().to_vec())],
        );
        assert_eq!(
            run(&n, &me, &[]).unwrap_err(),
            fail(Errno::ELOOP, "Can not enslave a bridge to a bridge")
        );
        // Deleting the veth (with its peer): the bridge has no ports left, so its MAC is all
        // zeros and it has no carrier
        run(&n, &link(RTM_DELLINK, 0, 0, None, &[name("v1")]), &[]).unwrap();
        let v = view_of(&n, "br0");
        assert_eq!((v.mac, v.carrier, v.mtu), ([0; 6], false, 1500));
        assert_eq!(views(&n).len(), 1);
        // An unknown kind fails; lo cannot be deleted
        let bad = link(RTM_NEWLINK, CREATE, 0, None, &[name("x"), kind("vxlan")]);
        assert_eq!(
            run(&n, &bad, &[]).unwrap_err(),
            fail(Errno::EOPNOTSUPP, "Unknown device type")
        );
        let lo = link(RTM_DELLINK, 0, 1, None, &[]);
        assert_eq!(run(&n, &lo, &[]).unwrap_err().errno, Errno::EOPNOTSUPP.0);
    }

    fn addr(
        ty: u16,
        flags: u16,
        family: i32,
        prefix: u8,
        index: u32,
        a: &[u8],
        brd: Option<[u8; 4]>,
    ) -> Vec<u8> {
        let mut p = vec![family as u8, prefix, 0, 0];
        p.extend_from_slice(&index.to_le_bytes());
        attr(&mut p, IFA_LOCAL, a);
        attr(&mut p, IFA_ADDRESS, a);
        if let Some(b) = brd {
            attr(&mut p, IFA_BROADCAST, &b);
        }
        message(ty, flags | 1, 1, 0, &p)
    }

    fn route(
        ty: u16,
        flags: u16,
        dst: [u8; 4],
        prefix: u8,
        gw: Option<[u8; 4]>,
        oif: Option<u32>,
    ) -> Vec<u8> {
        let mut p = vec![
            AF_INET as u8,
            prefix,
            0,
            0,
            RT_TABLE_MAIN,
            3,
            0,
            RTN_UNICAST,
            0,
            0,
            0,
            0,
        ];
        if ty == RTM_DELROUTE {
            p[6] = RT_SCOPE_NOWHERE;
        }
        attr(&mut p, RTA_DST, &dst);
        if let Some(g) = gw {
            attr(&mut p, RTA_GATEWAY, &g);
        }
        if let Some(o) = oif {
            attr(&mut p, RTA_OIF, &o.to_le_bytes());
        }
        message(ty, flags | 1, 1, 0, &p)
    }

    fn shown(n: &NetNs) -> Vec<String> {
        entries(n)
            .iter()
            .filter(|e| e.family == AF_INET)
            .map(|e| {
                format!(
                    "t{} n{} {:?}/{} src {:?} via {:?} {}{}",
                    e.table,
                    e.ty,
                    e.dst,
                    e.prefix,
                    e.prefsrc,
                    e.gateway,
                    e.iface,
                    if e.flags != 0 { " linkdown" } else { "" }
                )
            })
            .collect()
    }

    #[test]
    fn addresses_and_routes() {
        let n = ns(4);
        run(
            &n,
            &link(RTM_NEWLINK, CREATE, 0, None, &[name("d0"), kind("dummy")]),
            &[],
        )
        .unwrap();
        let i = view_of(&n, "d0").index;
        // While down: only the local route
        run(
            &n,
            &addr(RTM_NEWADDR, CREATE, AF_INET, 24, i, &[10, 0, 0, 1], None),
            &[],
        )
        .unwrap();
        assert_eq!(
            shown(&n),
            ["t255 n2 [10, 0, 0, 1]/32 src Some([10, 0, 0, 1]) via None d0"]
        );
        up(&n, "d0", true);
        run(
            &n,
            &addr(RTM_NEWADDR, CREATE, AF_INET, 24, i, &[10, 0, 0, 2], None),
            &[],
        )
        .unwrap();
        assert_eq!(
            run(
                &n,
                &addr(RTM_NEWADDR, CREATE, AF_INET, 24, i, &[10, 0, 0, 2], None),
                &[]
            )
            .unwrap_err()
            .errno,
            Errno::EEXIST.0
        );
        let v = view_of(&n, "d0");
        assert_eq!(v.v4[1].flags & IFA_F_SECONDARY, IFA_F_SECONDARY);
        assert_eq!(
            shown(&n),
            [
                "t255 n2 [10, 0, 0, 1]/32 src Some([10, 0, 0, 1]) via None d0",
                "t254 n1 [10, 0, 0, 0]/24 src Some([10, 0, 0, 1]) via None d0",
                "t255 n3 [10, 0, 0, 255]/32 src Some([10, 0, 0, 1]) via None d0",
                "t255 n2 [10, 0, 0, 2]/32 src Some([10, 0, 0, 1]) via None d0",
            ]
        );
        // The gateway must be in the subnet and the interface up; a duplicate is EEXIST, and
        // deleting a missing one ESRCH
        let gw = route(RTM_NEWROUTE, CREATE, [0; 4], 0, Some([10, 0, 0, 254]), None);
        run(&n, &gw, &[]).unwrap();
        assert_eq!(run(&n, &gw, &[]).unwrap_err().errno, Errno::EEXIST.0);
        let far = route(
            RTM_NEWROUTE,
            CREATE,
            [10, 9, 0, 0],
            16,
            Some([192, 168, 0, 1]),
            None,
        );
        assert_eq!(
            run(&n, &far, &[]).unwrap_err(),
            fail(Errno::ENETUNREACH, "Nexthop has invalid gateway")
        );
        let host_bits = route(RTM_NEWROUTE, CREATE, [10, 9, 0, 1], 16, None, Some(i));
        assert_eq!(run(&n, &host_bits, &[]).unwrap_err().errno, Errno::EINVAL.0);
        let del = route(RTM_DELROUTE, 0, [10, 8, 0, 0], 16, None, None);
        assert_eq!(run(&n, &del, &[]).unwrap_err().errno, Errno::ESRCH.0);
        // Down: the local route stays, others (including the default route through it) are
        // removed; bringing it up again restores only the routes added per address
        up(&n, "d0", false);
        assert_eq!(shown(&n).len(), 2);
        let down = route(RTM_NEWROUTE, CREATE, [10, 7, 0, 0], 16, None, Some(i));
        assert_eq!(
            run(&n, &down, &[]).unwrap_err(),
            fail(Errno::ENETDOWN, "Device for nexthop is not up")
        );
        up(&n, "d0", true);
        assert_eq!(shown(&n).len(), 4);
        // Deleting the primary: the secondaries in the same subnet go too, and all routes are
        // gone
        run(
            &n,
            &addr(RTM_DELADDR, 0, AF_INET, 24, i, &[10, 0, 0, 1], None),
            &[],
        )
        .unwrap();
        assert!(view_of(&n, "d0").v4.is_empty());
        assert!(shown(&n).is_empty());
    }

    #[test]
    fn sysctls_follow_default_all_and_disable_ipv6() {
        let n = ns(5);
        conf_write(&n, false, ConfDir::Default, b"rp_filter", b"2\n").unwrap();
        run(
            &n,
            &link(RTM_NEWLINK, CREATE, 0, None, &[name("d0"), kind("dummy")]),
            &[],
        )
        .unwrap();
        let i = view_of(&n, "d0").index;
        assert_eq!(
            conf_read(&n, false, ConfDir::Dev(i), b"rp_filter").unwrap(),
            "2\n"
        );
        // default is applied to the interfaces that have not set the entry individually; those
        // that have stay unchanged
        conf_write(&n, false, ConfDir::Dev(i), b"arp_notify", b"1").unwrap();
        conf_write(&n, false, ConfDir::Default, b"arp_notify", b"0").unwrap();
        conf_write(&n, false, ConfDir::Default, b"rp_filter", b"1").unwrap();
        assert_eq!(
            conf_read(&n, false, ConfDir::Dev(i), b"arp_notify").unwrap(),
            "1\n"
        );
        assert_eq!(
            conf_read(&n, false, ConfDir::Dev(i), b"rp_filter").unwrap(),
            "1\n"
        );
        // ip_forward is all/forwarding: applied to every interface
        conf_write(&n, false, ConfDir::All, b"forwarding", b"1").unwrap();
        assert_eq!(ip_forward(&n), 1);
        assert_eq!(
            conf_read(&n, false, ConfDir::Dev(i), b"forwarding").unwrap(),
            "1\n"
        );
        // IPv6: after bringing it up there is a link-local address; disabling IPv6 removes it, and
        // adding an address is EACCES
        up(&n, "d0", true);
        assert_eq!(view_of(&n, "d0").v6.len(), 1);
        assert_eq!(
            conf_read(&n, true, ConfDir::Dev(i), b"mtu").unwrap(),
            "1500\n"
        );
        assert_eq!(
            conf_read(&n, true, ConfDir::Dev(i), b"stable_secret"),
            Err(Errno::EIO)
        );
        conf_write(&n, true, ConfDir::Dev(i), b"disable_ipv6", b"1").unwrap();
        assert!(view_of(&n, "d0").v6.is_empty());
        let a6 = addr(RTM_NEWADDR, CREATE, AF_INET6, 64, i, &[0xfd; 16], None);
        assert_eq!(run(&n, &a6, &[]).unwrap_err().errno, Errno::EACCES.0);
        conf_write(&n, true, ConfDir::Dev(i), b"disable_ipv6", b"0").unwrap();
        assert_eq!(view_of(&n, "d0").v6.len(), 1);
        assert_eq!(
            conf_write(&n, true, ConfDir::Dev(i), b"mtu", b"9000"),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            conf_write(&n, true, ConfDir::Dev(i), b"hop_limit", b"x"),
            Err(Errno::EINVAL)
        );
        let dirs: Vec<Vec<u8>> = conf_dirs(&n).into_iter().map(|(d, _)| d).collect();
        assert_eq!(
            dirs,
            [
                b"all".to_vec(),
                b"default".to_vec(),
                b"lo".to_vec(),
                b"d0".to_vec()
            ]
        );
    }
}
