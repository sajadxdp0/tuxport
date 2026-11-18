//! Sockets (the kernel/net map; its netlink sections are the kernel/netlink map).
//!
//! - TCP and UDP passthrough: a guest socket holds a nonblocking host socket, and addresses and
//!   options are passed through byte for byte. The guest's `SOCK_RAW + IPPROTO_ICMP` becomes a
//!   host ping socket.
//! - DNS stub: the guest's `/etc/resolv.conf` points at `127.0.0.53`. UDP datagrams sent to
//!   `127.0.0.53:53` do not go through the host network but to the host's resolver
//!   ([`tuxport_hal::net::dns_query`]), and the answer is put into this socket's own queue, as if
//!   received from `127.0.0.53:53`.
//! - `NETLINK_ROUTE`: queries for interfaces, addresses, routes and rules are answered in process
//!   (getifaddrs, `ip addr`, `ip route`, `ip rule`; routes are in [`crate::route`]); the neighbour
//!   table is always empty.
//! - AF_UNIX: emulated entirely in process ([`crate::unix`]).

use std::collections::{HashSet, VecDeque};
use std::fmt::Write as _;
use std::rc::Rc;
use std::time::Duration;

use tuxport_hal::net::{Interface, Route};

use crate::traffic::IfStats;

/// Address families.
pub const AF_UNIX: i32 = 1;
pub const AF_INET: i32 = 2;
pub const AF_INET6: i32 = 10;
pub const AF_NETLINK: i32 = 16;

/// Socket types (without `SOCK_NONBLOCK`, `SOCK_CLOEXEC`).
pub const SOCK_STREAM: i32 = 1;
pub const SOCK_DGRAM: i32 = 2;
pub const SOCK_RAW: i32 = 3;

pub(crate) const IPPROTO_ICMP: i32 = 1;
pub(crate) const IPPROTO_TCP: i32 = 6;
pub(crate) const IPPROTO_UDP: i32 = 17;
/// The protocol number of MPTCP (the third argument of `socket(2)`, reported by `SO_PROTOCOL`).
pub(crate) const IPPROTO_MPTCP: i32 = 262;
const IPPROTO_ICMPV6: i32 = 58;

/// A guest socket.
pub(crate) enum Socket {
    Inet(InetSocket),
    Netlink(NetlinkSocket),
    Unix(crate::unix::UnixSocket),
}

/// TCP, UDP and ping sockets implemented with host sockets.
pub(crate) struct InetSocket {
    /// The host fd (nonblocking), closed together with this object.
    pub fd: i32,
    pub family: i32,
    /// The type of the host socket.
    pub ty: i32,
    /// The protocol number: the one the guest gave when creating the socket, or the default TCP or
    /// UDP for the type when it gave 0.
    pub proto: i32,
    /// The guest created a `SOCK_RAW`: raw ICMP is a ping socket on the host, and `SO_TYPE` and
    /// `/proc/net/raw` report what the guest created.
    pub raw: bool,
    /// The peer a datagram socket connected to with connect. Ping and raw sockets have no peer
    /// port, and the host's getpeername does not give the peer (Linux's inet_getname), so
    /// `/proc/net` needs this.
    pub peer: Option<Vec<u8>>,
    /// Connected to the DNS stub with connect (the host socket itself is not connected).
    pub stub_peer: bool,
    /// DNS stub answers waiting for the guest to read them.
    pub answers: VecDeque<Vec<u8>>,
    /// The number of answers put into `answers` (`EPOLLET` triggers on each).
    pub answered: u64,
    /// `SO_RCVTIMEO`, `SO_SNDTIMEO`: we emulate blocking, so we also handle the timeouts.
    pub rcvtimeo: Option<Duration>,
    pub sndtimeo: Option<Duration>,
    /// Which interface the traffic counts on ([`crate::traffic`]), only once the local address is
    /// settled.
    pub route: Option<crate::traffic::Route>,
    /// A TCP connection this socket initiated has been counted in ActiveOpens and its failure not
    /// yet counted ([`crate::traffic::Protocols`]).
    pub opening: bool,
    /// Created in a new network namespace: addresses need translating ([`crate::netns`]); None is
    /// the host's network.
    pub net: Option<Rc<crate::nsproxy::NetNs>>,
    /// In a new network namespace: the host socket is already bound (by the guest, or on its
    /// behalf before connect, sendto or listen).
    pub bound: bool,
    /// In a new network namespace: the local end is a wildcard address and not yet connected
    /// (getsockname reports the wildcard address).
    pub any: bool,
    /// In a new network namespace: bound to its own virtual address (on the host it is 127.a.b.1,
    /// the same as the wildcard address; getsockname reports it, [`crate::netns`]).
    pub vaddr: Option<[u8; 4]>,
    /// The peer given to connect (a guest address): getpeername reports it.
    pub guest_peer: Option<Vec<u8>>,
    /// In a new network namespace the host socket is bound to the wildcard address (a datagram
    /// socket that explicitly bound the wildcard address with port 0, or that the host bound
    /// automatically on its first send to the host network): it can reach the outside. When
    /// sending to a mapped destination, `IP_PKTINFO` sets the source address to this namespace's
    /// loopback address ([`crate::netns`]).
    pub wide: bool,
    /// A `wide` datagram socket connected to a mapped destination (an address on the host): the
    /// host socket is not connected (connecting would fix the local end at the 127.0.0.1 the host
    /// chooses, and datagrams coming back to this namespace's loopback address would not match);
    /// sends without an address go to it, and only datagrams from it are received (as with a
    /// connected UDP socket on Linux).
    pub vconn: Option<Vec<u8>>,
    /// What nftables nat and filtering need to record ([`Track`]), allocated only when used.
    pub track: Option<Box<Track>>,
    /// A port listening in the boot namespace that has a DNAT in PREROUTING: connections from
    /// outside the host are passed to the destination by the forwarding thread
    /// ([`crate::forward`]).
    pub fwd: Option<Box<crate::forward::Handle>>,
    /// The guest called listen (`SO_ACCEPTCONN` is not available on MPTCP host sockets).
    pub listening: bool,
    /// The guest shut the socket down for reading (`SHUT_RD` or `SHUT_RDWR`; recorded also when
    /// the call fails with `ENOTCONN`, as Linux's `inet_shutdown` sets `sk_shutdown` then too): a
    /// blocking receive on a datagram socket with nothing queued returns 0.
    pub rd_shut: bool,
    /// The number under which a listen in a new network namespace is registered in the namespace
    /// ([`crate::nsproxy::Listener`]), removed on close.
    pub listener: Option<std::num::NonZeroU64>,
    /// UDP: bound on the host to 127.a.b.254 (for datagrams sent to its own virtual address), with
    /// the port registered in the namespace's [`crate::nsproxy::Loopback::self_udp`], removed on
    /// close.
    pub self_port: Option<u16>,
    /// `IPV6_V6ONLY` in a new network namespace: the host socket must use IPv4-mapped addresses,
    /// so it is not set there.
    pub v6only: bool,
    /// The guest created a raw ICMP socket: received messages go through the aggregation point
    /// and are reshaped as on a raw socket ([`crate::rawicmp`]).
    pub icmp: Option<crate::rawicmp::RawIcmp>,
    /// UDP: the guest set `IP_TTL`, `IPV6_UNICAST_HOPS` (traceroute's probes), enabled
    /// `IP_RECVERR`, `IPV6_RECVERR` itself, and the aggregation point watching its errors
    /// ([`crate::rawicmp::before_io`]).
    pub ttl_set: bool,
    pub guest_recverr: bool,
    pub udp_tap: Option<Rc<crate::cell::RefCell<crate::rawicmp::Tap>>>,
    /// The ICMP error that the aggregation point took off the host on this socket's behalf and that
    /// Linux's rules say to report to the guest (an errno, 0 for none): the next send or receive,
    /// or `SO_ERROR`, takes it (Linux's `sk_err`).
    pub icmp_err: std::cell::Cell<i32>,
    /// Bound to a mapped low port ([`crate::portmap`]): the lease, and (guest port, host port).
    /// Connections from accept have only the latter (the local end is the listening port).
    pub lease: Option<crate::portmap::Lease>,
    pub local_map: Option<(u16, u16)>,
    /// Connected to a mapped low port on the local machine: (guest port, host port).
    pub peer_map: Option<(u16, u16)>,
    /// A classic BPF program attached with `SO_ATTACH_FILTER` that the host refused (a host with
    /// Linux 5d39580f68e6 lets only `CAP_NET_ADMIN` attach one to a TCP socket, and the app never
    /// has it), kept here as the guest gave it so that `SO_GET_FILTER`, `SO_DETACH_FILTER` and
    /// accepted sockets see it as on Linux. It filters nothing.
    pub filter: Option<Vec<u8>>,
}

impl InetSocket {
    pub fn new(fd: i32, family: i32, ty: i32) -> InetSocket {
        InetSocket {
            fd,
            family,
            raw: ty == SOCK_RAW,
            ty,
            proto: match ty {
                SOCK_STREAM => IPPROTO_TCP,
                SOCK_DGRAM => IPPROTO_UDP,
                _ => 0,
            },
            peer: None,
            stub_peer: false,
            answers: VecDeque::new(),
            answered: 0,
            rcvtimeo: None,
            sndtimeo: None,
            route: None,
            opening: false,
            net: None,
            bound: false,
            any: false,
            vaddr: None,
            guest_peer: None,
            wide: false,
            vconn: None,
            track: None,
            fwd: None,
            listening: false,
            rd_shut: false,
            listener: None,
            self_port: None,
            v6only: false,
            icmp: None,
            ttl_set: false,
            guest_recverr: false,
            udp_tap: None,
            icmp_err: std::cell::Cell::new(0),
            lease: None,
            local_map: None,
            peer_map: None,
            filter: None,
        }
    }
}

impl InetSocket {
    /// An ICMPv6 socket (a ping socket, or the one emulating a raw socket): IPv4-mapped addresses
    /// cannot be used in a new network namespace ([`crate::netns::to_host`]).
    pub fn icmp6(&self) -> bool {
        self.family == AF_INET6 && self.proto == IPPROTO_ICMPV6
    }
}

/// What Linux keeps in connection tracking for a socket ([`crate::nft`],
/// docs/rounds/175-filter.md).
#[derive(Debug, Default)]
pub(crate) struct Track {
    /// Destinations nat rewrote: (rewritten to, given by the guest), both guest sockaddrs.
    /// Datagrams received from the former are reported as coming from the latter.
    pub nat_back: Vec<(Vec<u8>, Vec<u8>)>,
    /// UDP: peers that datagrams were received from (guest sockaddrs). Sending back to them is
    /// ESTABLISHED, and `ct state established` rules in the filter chains recognize them.
    pub seen: Vec<Vec<u8>>,
    /// Connected UDP: the destination connected to was dropped by a filter chain. Sends without an
    /// address are not sent: 0 means silently dropped, anything else is an error number (a drop
    /// on OUTPUT is EPERM).
    pub blocked: Option<i32>,
    /// A TCP connection from accept: when nat rewrote the destination of the initiating end, the
    /// original destination seen in this namespace (a guest sockaddr, reported by
    /// `SO_ORIGINAL_DST`, [`crate::origdst`]).
    pub orig_dst: Option<Vec<u8>>,
    /// `SO_MARK`: only recorded here, not set on the host socket (the app has no permission for it
    /// on the host, and Android's netd also uses marks on the host to choose networks). Packets
    /// sent by this machine carry it in nftables ([`crate::nft::Packet::mark`]).
    pub mark: u32,
}

impl InetSocket {
    pub(crate) fn track_mut(&mut self) -> &mut Track {
        self.track.get_or_insert_with(Default::default)
    }

    /// `SO_MARK` ([`Track::mark`]).
    pub(crate) fn mark(&self) -> u32 {
        self.track.as_ref().map_or(0, |t| t.mark)
    }
}

impl Drop for InetSocket {
    fn drop(&mut self) {
        tuxport_hal::net::close(self.fd);
        if let (Some(p), Some(n)) = (self.self_port, &self.net)
            && let Ok(mut lo) = n.lo.try_borrow_mut()
            && let Some(i) = lo.self_udp.iter().position(|&x| x == p)
        {
            lo.self_udp.swap_remove(i);
        }
        if let (Some(id), Some(n)) = (self.listener, &self.net)
            && let Ok(mut l) = n.listeners.try_borrow_mut()
        {
            l.retain(|x| x.id != id.get());
        }
    }
}

// ---- DNS stub ----

/// The IPv4 address of the DNS stub, 127.0.0.53.
const STUB_V4: [u8; 4] = [127, 0, 0, 53];

/// Whether an address is that of the DNS stub (`127.0.0.53:53`, or its IPv4-mapped IPv6 address).
pub fn is_stub(addr: &[u8]) -> bool {
    if addr.len() < 8 || addr[2..4] != 53u16.to_be_bytes() {
        return false;
    }
    match u16::from_le_bytes([addr[0], addr[1]]) as i32 {
        AF_INET => addr[4..8] == STUB_V4,
        AF_INET6 if addr.len() >= 24 => {
            addr[8..18] == [0; 10] && addr[18..20] == [0xff, 0xff] && addr[20..24] == STUB_V4
        }
        _ => false,
    }
}

/// The DNS stub's sockaddr, encoded for the socket's address family.
pub fn stub_addr(family: i32) -> Vec<u8> {
    use std::net::{Ipv4Addr, SocketAddr};
    let v4 = Ipv4Addr::from(STUB_V4);
    let addr = if family == AF_INET6 {
        SocketAddr::new(v4.to_ipv6_mapped().into(), 53)
    } else {
        SocketAddr::new(v4.into(), 53)
    };
    tuxport_hal::net::sockaddr(addr)
}

/// The answer given to the guest when resolution fails: the query brought back as is, with the
/// QR and RA bits set and RCODE SERVFAIL, so the resolver fails at once instead of waiting for a
/// timeout.
pub fn servfail(query: &[u8]) -> Vec<u8> {
    let mut r = query.to_vec();
    if r.len() >= 4 {
        r[2] |= 0x80;
        r[3] = (r[3] & 0x70) | 0x80 | 2;
    }
    r
}

// ---- netlink (the kernel/netlink map, through NETLINK_SOCK_DIAG) ----

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_MULTI: u16 = 2;
const NLM_F_ACK: u16 = 4;
const NLM_F_DUMP: u16 = 0x300;
const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_GETADDR: u16 = 22;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
const RTM_GETROUTE: u16 = 26;
const RTM_GETNEIGH: u16 = 30;
const RTM_NEWRULE: u16 = 32;
const RTM_GETRULE: u16 = 34;
const RTM_NEWNSID: u16 = 88;
const RTM_GETNSID: u16 = 90;
const NETNSA_NSID: u16 = 1;
/// RTM_GETNSID looks up a network namespace by process ([`Host::peer_net`]).
pub(crate) const NETNSA_PID: u16 = 2;
/// RTM_GETNSID looks up a network namespace by namespace file.
pub(crate) const NETNSA_FD: u16 = 3;
const NETNSA_TARGET_NSID: u16 = 4;
const IFLA_ADDRESS: u16 = 1;
const IFLA_BROADCAST: u16 = 2;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_QDISC: u16 = 6;
const IFLA_STATS: u16 = 7;
const IFLA_TXQLEN: u16 = 13;
const IFLA_OPERSTATE: u16 = 16;
const IFLA_LINKMODE: u16 = 17;
const IFLA_STATS64: u16 = 23;
const IFLA_GROUP: u16 = 27;
const IFLA_PROMISCUITY: u16 = 30;
const IFLA_NUM_TX_QUEUES: u16 = 31;
const IFLA_NUM_RX_QUEUES: u16 = 32;
const IFLA_CARRIER: u16 = 33;
const IFLA_PROTO_DOWN: u16 = 39;
const IFLA_MASTER: u16 = 10;
const IFLA_LINKINFO: u16 = 18;
/// The largest datagram of a dump (larger dumps are split): Linux's `NLMSG_GOODSIZE` on 4 KiB
/// pages, `SKB_WITH_OVERHEAD(PAGE_SIZE)`, which bounds a dump's first datagram and every later one
/// for a reader whose buffer is a page. Go's `syscall.NetlinkRIB` (`net.Interfaces`,
/// `net.InterfaceAddrs`) reads into one page and fails on a truncated datagram without checking
/// `MSG_TRUNC`, so a dump datagram must fit in 4 KiB.
const DUMP_CHUNK: usize = 3776;
/// `IF_OPER_*` (RFC 2863 interface states).
const IF_OPER_UNKNOWN: u8 = 0;
const IF_OPER_DOWN: u8 = 2;
const IF_OPER_UP: u8 = 6;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const IFA_BROADCAST: u16 = 4;
const IFA_CACHEINFO: u16 = 6;
const IFA_FLAGS: u16 = 8;
const IFA_PROTO: u16 = 11;
/// Addresses added by the kernel: the loopback interface's `::1` and autoconfigured link-local
/// addresses (`IFAPROT_KERNEL_*`).
const IFAPROT_KERNEL_LO: u8 = 1;
const IFAPROT_KERNEL_LL: u8 = 3;
const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;
const IFF_UP: u32 = 0x1;
const IFF_BROADCAST: u32 = 0x2;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_RUNNING: u32 = 0x40;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RT_SCOPE_HOST: u8 = 254;
/// The address never expires (not a temporary address from DHCP or SLAAC).
const IFA_F_PERMANENT: u8 = 0x80;
/// The protocol number of NETLINK_ROUTE.
pub const NETLINK_ROUTE: i32 = 0;
/// The protocol number of NETLINK_KOBJECT_UEVENT: hotplug events sent by the kernel.
pub const NETLINK_KOBJECT_UEVENT: i32 = 15;
/// The protocol number of NETLINK_GENERIC.
pub const NETLINK_GENERIC: i32 = 16;
/// The protocol number of NETLINK_NETFILTER (nftables, [`crate::nft`]).
pub const NETLINK_NETFILTER: i32 = 12;
/// The protocol number of NETLINK_SOCK_DIAG (`NETLINK_INET_DIAG`): socket queries, answered as by
/// a kernel without its diag modules ([`sock_diag_request`]).
pub const NETLINK_SOCK_DIAG: i32 = 4;

/// A netlink socket emulated in process. `NETLINK_ROUTE` answers queries, `NETLINK_GENERIC` only
/// for nlctrl, `NETLINK_NETFILTER` for nftables and `NETLINK_SOCK_DIAG` only with errors; a
/// `NETLINK_KOBJECT_UEVENT` socket never receives anything (the uevents mdev listens for), and
/// other protocols cannot be created (`syscall::net::socket`).
pub(crate) struct NetlinkSocket {
    pub protocol: i32,
    /// `SOCK_RAW` or `SOCK_DGRAM`, as created (`SO_TYPE`).
    pub ty: i32,
    /// `nl_pid`: assigned on bind, or by the first connect or send (Linux's autobind); 0 until
    /// then.
    pub pid: u32,
    /// Answers and notifications waiting for the guest to read them, one datagram per item,
    /// with the multicast group a notification went to (0 for a unicast answer: the source
    /// address's `nl_groups` and `NETLINK_PKTINFO` report it).
    pub queue: VecDeque<(Vec<u8>, u32)>,
    /// The number of datagrams put into `queue` (`EPOLLET` triggers on each).
    pub arrivals: u64,
    /// `SO_RCVTIMEO`: how long to wait at most when there is no answer to read.
    pub rcvtimeo: Option<Duration>,
    /// The new network namespace it was created in, if any (its lo is answered and configured).
    pub net: Option<Rc<crate::nsproxy::NetNs>>,
    /// `NETLINK_EXT_ACK`: error acks carry the extack message (iproute2 prints it as
    /// `Error: ….`).
    pub ext_ack: bool,
    /// `NETLINK_CAP_ACK`: error acks carry only the request's message header.
    pub cap_ack: bool,
    /// The multicast groups joined by bind or `NETLINK_ADD_MEMBERSHIP` (bit `n - 1` for group
    /// `n`), None until the first one (Linux allocates the set then, and
    /// `NETLINK_LIST_MEMBERSHIPS` reports nothing before). On `NETLINK_ROUTE` the link, address
    /// and route groups receive notifications ([`Snapshot::changes`]).
    pub groups: Option<u64>,
    /// A notification was dropped for a full queue: the next receive or `SO_ERROR` takes
    /// `ENOBUFS` (`netlink_overrun`).
    pub overrun: std::cell::Cell<bool>,
    /// The `SOL_NETLINK` flags that are only recorded and read back (`NETLINK_PKTINFO`,
    /// `NETLINK_BROADCAST_ERROR`, `NETLINK_NO_ENOBUFS`, `NETLINK_LISTEN_ALL_NSID`,
    /// `NETLINK_GET_STRICT_CHK`): bit `n` for option `n`. With `NETLINK_PKTINFO` every message
    /// received carries a pktinfo control message (group 0: answers are unicast).
    pub flags: u32,
    /// `SO_PASSCRED`: every message received carries the kernel's credentials.
    pub passcred: bool,
    /// `SO_SNDTIMEO`: recorded only (sending to the kernel never waits).
    pub sndtimeo: Option<Duration>,
    /// The generic `SOL_SOCKET` options it only records.
    pub opts: SockOpts,
}

impl NetlinkSocket {
    pub fn new(protocol: i32, ty: i32) -> NetlinkSocket {
        NetlinkSocket {
            protocol,
            ty,
            pid: 0,
            queue: VecDeque::new(),
            arrivals: 0,
            rcvtimeo: None,
            net: None,
            ext_ack: false,
            cap_ack: false,
            groups: None,
            flags: 0,
            passcred: false,
            sndtimeo: None,
            opts: SockOpts::default(),
            overrun: Default::default(),
        }
    }

    /// It is an rtnetlink socket that joined a group notifications go to.
    pub fn subscribed(&self) -> bool {
        self.protocol == NETLINK_ROUTE && self.groups.is_some_and(|g| g & RTNL_NOTIFIED != 0)
    }

    /// Queues notification `msg` of multicast group `group` when the socket joined it; when the
    /// queue is full it is dropped, and unless `NETLINK_NO_ENOBUFS` the next receive reports the
    /// overrun. Returns whether it concerned the socket.
    pub fn notify(&mut self, group: u32, msg: &[u8]) -> bool {
        const NETLINK_NO_ENOBUFS: u32 = 5;
        if self.protocol != NETLINK_ROUTE || self.groups.is_none_or(|g| g & (1 << (group - 1)) == 0)
        {
            return false;
        }
        if self.queue.len() >= NOTE_QUEUE {
            if self.flags & (1 << NETLINK_NO_ENOBUFS) == 0 {
                self.overrun.set(true);
            }
            return true;
        }
        self.queue.push_back((msg.to_vec(), group));
        self.arrivals += 1;
        true
    }

    /// How many multicast groups the protocol has (Linux's `nl_table[].groups`: at least 32,
    /// and `RTNLGRP_MAX` for `NETLINK_ROUTE` on 6.1).
    pub fn ngroups(&self) -> u32 {
        if self.protocol == NETLINK_ROUTE {
            36
        } else {
            32
        }
    }

    /// Handles one datagram from the guest (which may hold several requests) and queues the
    /// answers.
    pub fn handle(&mut self, data: &[u8], host: &dyn Host) {
        // An nftables batch spans several messages, so the whole datagram is handled together
        if self.protocol == NETLINK_NETFILTER {
            let peer = crate::nft::Peer {
                pid: self.pid,
                ext_ack: self.ext_ack,
                cap_ack: self.cap_ack,
                proc_pid: 0,
                comm: b"",
            };
            for d in host.netfilter(data, &peer) {
                self.queue.push_back((d, 0));
                self.arrivals += 1;
            }
            return;
        }
        if !matches!(
            self.protocol,
            NETLINK_ROUTE | NETLINK_GENERIC | NETLINK_SOCK_DIAG
        ) {
            return;
        }
        let mut off = 0;
        while off + 16 <= data.len() {
            let h = &data[off..];
            let len = u32::from_le_bytes(h[..4].try_into().unwrap()) as usize;
            if len < 16 || off + len > data.len() {
                break;
            }
            let answers = match self.protocol {
                NETLINK_GENERIC => generic_request(&h[..len], self.pid),
                NETLINK_SOCK_DIAG => sock_diag_request(&h[..len], self.pid, self.cap_ack),
                _ => self.route_request(&h[..len], host),
            };
            for d in answers {
                self.queue.push_back((d, 0));
                self.arrivals += 1;
            }
            off += len.next_multiple_of(4);
        }
    }

    /// Answers one request `req` (the whole message) on `NETLINK_ROUTE`, returning the datagrams.
    fn route_request(&self, req: &[u8], host: &dyn Host) -> Vec<Vec<u8>> {
        let ty = u16::from_le_bytes([req[4], req[5]]);
        let flags = u16::from_le_bytes([req[6], req[7]]);
        let seq = u32::from_le_bytes(req[8..12].try_into().unwrap());
        let family = req.get(16).copied().unwrap_or(0) as i32;
        let reply = |ty, flags, payload: &[u8]| message(ty, flags, seq, self.pid, payload);
        let done = || reply(NLMSG_DONE, NLM_F_MULTI, &0i32.to_le_bytes());
        let dump = flags & NLM_F_DUMP != 0;
        if let Some(r) = host.configure(req) {
            return match r {
                Err(f) => vec![ack_ext(
                    req,
                    f.errno,
                    self.pid,
                    f.msg.filter(|_| self.ext_ack),
                    self.cap_ack,
                )],
                Ok(()) if flags & NLM_F_ACK != 0 => vec![ack(req, 0, self.pid)],
                Ok(()) => Vec::new(),
            };
        }
        let one = |l: &Link| match l {
            Link::Host(i) => link_message(i, host.stats(&i.name), host.tx_queue_len(&i.name)),
            Link::Virt(v, nsid) => crate::netdev::link_message(v, *nsid),
        };
        match ty {
            // As on Linux, the interfaces are one datagram (split into several when too large,
            // `pack`); the addresses one per address family (IPv4 first for AF_UNSPEC, each split
            // the same way), and NLMSG_DONE another one. When filtering by IFLA_MASTER or
            // IFLA_LINKINFO's kind, the messages carry NLM_F_DUMP_FILTERED (Linux's
            // rtnl_dump_ifinfo)
            RTM_GETLINK if dump => {
                const NLM_F_DUMP_FILTERED: u16 = 0x20;
                let attrs = crate::route::attrs(req.get(32..).unwrap_or_default());
                let master = attrs
                    .iter()
                    .find(|(t, d)| *t == IFLA_MASTER && d.len() >= 4)
                    .map(|(_, d)| u32::from_le_bytes(d[..4].try_into().unwrap()))
                    .filter(|&m| m != 0);
                let kind = attrs
                    .iter()
                    .find(|(t, _)| *t == IFLA_LINKINFO)
                    .and_then(|(_, d)| {
                        crate::route::attrs(d)
                            .into_iter()
                            .find(|(t, _)| *t == 1)
                            .map(|(_, k)| k.split(|&c| c == 0).next().unwrap_or_default().to_vec())
                    })
                    .filter(|k| matches!(&k[..], b"bridge" | b"veth" | b"dummy"));
                let filtered = master.is_some() || kind.is_some();
                let mflags = NLM_F_MULTI | if filtered { NLM_F_DUMP_FILTERED } else { 0 };
                let links = host.links();
                let kept = links.iter().filter(|l| match l {
                    Link::Host(_) => master.is_none() && kind.is_none(),
                    Link::Virt(v, _) => {
                        master.is_none_or(|m| v.master == Some(m))
                            && kind
                                .as_deref()
                                .is_none_or(|k| k == v.kind.name().as_bytes())
                    }
                });
                let mut out = pack(kept.map(|l| reply(RTM_NEWLINK, mflags, &one(l))));
                out.push(done());
                out
            }
            RTM_GETADDR if dump => {
                let links = host.links();
                let mut out = Vec::new();
                for f in [AF_INET, AF_INET6] {
                    if family != 0 && family != f {
                        continue;
                    }
                    let messages = links
                        .iter()
                        .flat_map(|l| match l {
                            Link::Host(i) => addr_messages(i, f as u8),
                            Link::Virt(v, _) => crate::netdev::addr_messages(v, f),
                        })
                        .map(|m| reply(RTM_NEWADDR, NLM_F_MULTI, &m));
                    out.extend(pack(messages));
                }
                out.push(done());
                out
            }
            // `ip link show <interface>`: looked up by ifi_index or IFLA_IFNAME (Linux's
            // rtnl_getlink)
            RTM_GETLINK => {
                const ENODEV: i32 = 19;
                let index = req
                    .get(20..24)
                    .map_or(0, |b| i32::from_le_bytes(b.try_into().unwrap()));
                let name = crate::route::attrs(req.get(32..).unwrap_or_default())
                    .into_iter()
                    .find(|(t, _)| *t == IFLA_IFNAME)
                    .map(|(_, d)| d.split(|&c| c == 0).next().unwrap_or_default().to_vec());
                if index <= 0 && name.is_none() {
                    return vec![ack(req, EINVAL, self.pid)];
                }
                let found = host.links().into_iter().find(|l| {
                    if index > 0 {
                        l.index() as i32 == index
                    } else {
                        name.as_deref() == Some(l.name())
                    }
                });
                let Some(l) = found else {
                    return vec![ack(req, ENODEV, self.pid)];
                };
                let mut out = vec![reply(RTM_NEWLINK, 0, &one(&l))];
                if flags & NLM_F_ACK != 0 {
                    out.push(ack(req, 0, self.pid));
                }
                out
            }
            // As on Linux, the routes of each address family are one datagram (split when too
            // large), and NLMSG_DONE another one
            RTM_GETROUTE if dump => {
                let entries = host.tables();
                let mut out = Vec::new();
                for f in [AF_INET, AF_INET6] {
                    let messages = entries
                        .iter()
                        .filter(|e| e.family == f && (family == 0 || family == f))
                        .map(|e| reply(RTM_NEWROUTE, NLM_F_MULTI, &crate::route::route_message(e)));
                    out.extend(pack(messages));
                }
                out.push(done());
                out
            }
            RTM_GETROUTE => {
                let ifaces = host.all_interfaces();
                let entries = host.tables();
                match crate::route::get(req, &entries, &ifaces, host.uid(), host.init_net()) {
                    Ok(p) => {
                        let mut out = vec![reply(RTM_NEWROUTE, 0, &p)];
                        if flags & NLM_F_ACK != 0 {
                            out.push(ack(req, 0, self.pid));
                        }
                        out
                    }
                    Err(e) => vec![ack(req, e, self.pid)],
                }
            }
            // The rules fit in one datagram, and NLMSG_DONE is another one
            RTM_GETRULE if dump => {
                const EAFNOSUPPORT: i32 = 97;
                let rules = crate::route::rule_messages(family);
                if rules.is_empty() {
                    return vec![ack(req, EAFNOSUPPORT, self.pid)];
                }
                let d = rules
                    .iter()
                    .flat_map(|m| reply(RTM_NEWRULE, NLM_F_MULTI, m))
                    .collect();
                vec![d, done()]
            }
            // Network namespace numbers (`ip netns list`): assigned to the namespace of the peer
            // only when dumping veths (IFLA_LINK_NETNSID, Linux's peernet2id_alloc), and
            // RTM_NEWNSID is EOPNOTSUPP; when the other namespace is found the assigned number is
            // answered, -1 when unassigned (NETNSA_NSID_NOT_ASSIGNED), and lookups by number never
            // find anything. The order is the same as Linux's rtnl_net_getid
            RTM_GETNSID => {
                let attrs = crate::route::attrs(req.get(20..).unwrap_or_default());
                let get = |t| {
                    attrs
                        .iter()
                        .find(|(a, d)| *a == t && d.len() >= 4)
                        .map(|(_, d)| u32::from_le_bytes(d[..4].try_into().unwrap()))
                };
                let peer = if let Some(pid) = get(NETNSA_PID) {
                    host.peer_net(NETNSA_PID, pid)
                } else if let Some(fd) = get(NETNSA_FD) {
                    host.peer_net(NETNSA_FD, fd)
                } else if get(NETNSA_NSID).is_some() {
                    Err(ENOENT)
                } else {
                    Err(EINVAL)
                };
                let r = peer.and_then(|id| match get(NETNSA_TARGET_NSID) {
                    Some(_) => Err(EINVAL),
                    None => Ok(id),
                });
                match r {
                    Ok(id) => {
                        let mut p = vec![0u8; 4];
                        attr(&mut p, NETNSA_NSID, &id.unwrap_or(-1).to_le_bytes());
                        let mut out = vec![reply(RTM_NEWNSID, 0, &p)];
                        if flags & NLM_F_ACK != 0 {
                            out.push(ack(req, 0, self.pid));
                        }
                        out
                    }
                    Err(e) => vec![ack(req, e, self.pid)],
                }
            }
            // The neighbour table (ARP, NDP): the app on Android cannot see the host's, so it is
            // always empty
            RTM_GETNEIGH if dump => vec![done()],
            RTM_GETNEIGH => vec![ack(req, ENOENT, self.pid)],
            // Unsupported requests: EOPNOTSUPP, after rtnetlink_rcv_msg's checks (a type it knows
            // that changes something needs CAP_NET_ADMIN over the namespace owner: EPERM);
            // control messages are acked only when an ack is asked for
            _ if flags & NLM_F_REQUEST != 0 => {
                if ty < NLMSG_MIN_TYPE {
                    if flags & NLM_F_ACK != 0 {
                        vec![ack(req, 0, self.pid)]
                    } else {
                        Vec::new()
                    }
                } else if (RTM_BASE..=RTM_MAX).contains(&ty)
                    && (ty - RTM_BASE) & 3 != 2
                    && !host.net_admin()
                {
                    vec![ack(req, EPERM, self.pid)]
                } else {
                    vec![ack(req, EOPNOTSUPP, self.pid)]
                }
            }
            _ => Vec::new(),
        }
    }

    /// This socket's `sockaddr_nl`.
    pub fn sockaddr(&self) -> Vec<u8> {
        let mut b = vec![0u8; 12];
        b[..2].copy_from_slice(&(AF_NETLINK as u16).to_le_bytes());
        b[4..8].copy_from_slice(&self.pid.to_le_bytes());
        // nl_groups: the first 32 groups
        b[8..12].copy_from_slice(&(self.groups.unwrap_or(0) as u32).to_le_bytes());
        b
    }
}

/// Packs a dump's messages into datagrams of at most `DUMP_CHUNK` bytes, as Linux's `netlink_dump`
/// fills one buffer after another (a message larger than that goes alone); no messages give no
/// datagram.
fn pack(messages: impl IntoIterator<Item = Vec<u8>>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for m in messages {
        if !cur.is_empty() && cur.len() + m.len() > DUMP_CHUNK {
            out.push(std::mem::take(&mut cur));
        }
        cur.extend(m);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Host information that netlink answers need (replaced with fixed values in tests).
pub(crate) trait Host {
    /// The host's network interfaces.
    fn interfaces(&self) -> Vec<Interface>;
    /// The traffic counters of an interface ([`crate::traffic`]).
    fn stats(&self, name: &str) -> IfStats;
    /// The routes of the host's main table; when None they are synthesized from the interface
    /// addresses ([`crate::route`]).
    fn routes(&self) -> Option<Vec<Route>>;
    /// The interface's transmit queue length (`IFLA_TXQLEN`), None when unavailable.
    fn tx_queue_len(&self, name: &str) -> Option<u32>;
    /// The user ID of the requesting process (RTA_UID in the `ip route get` answer).
    fn uid(&self) -> u32;
    /// Requests that change interfaces, addresses or routes ([`crate::netdev::request`]): None
    /// means not handled, Some holds the result.
    fn configure(&self, _req: &[u8]) -> Option<Result<(), crate::netdev::Fail>> {
        None
    }
    /// The network namespace RTM_GETNSID looks for: `attr` is [`NETNSA_PID`] (a process) or
    /// [`NETNSA_FD`] (a namespace file), and `value` its value. When found, the number this
    /// namespace assigned to it (None when unassigned); otherwise an error number.
    fn peer_net(&self, _attr: u16, _value: u32) -> Result<Option<i32>, i32> {
        Ok(None)
    }
    /// All interfaces: the host's (or lo) and virtual devices, by index.
    fn links(&self) -> Vec<Link> {
        self.interfaces().into_iter().map(Link::Host).collect()
    }
    /// The routing tables ([`crate::route::tables`], including those on virtual devices).
    fn tables(&self) -> Vec<Route2> {
        crate::route::tables(&self.interfaces(), self.routes())
    }
    /// The addresses of all interfaces (virtual devices included), for `ip route get`.
    fn all_interfaces(&self) -> Vec<Interface> {
        self.interfaces()
    }
    /// This is the boot namespace: `ip route get` may let the host choose the source address.
    fn init_net(&self) -> bool {
        true
    }
    /// Whether the requester has `CAP_NET_ADMIN` over the namespace's owner (requests that
    /// change something).
    fn net_admin(&self) -> bool {
        true
    }
    /// One datagram on `NETLINK_NETFILTER` ([`crate::nft::handle`]); returns the answer
    /// datagrams.
    fn netfilter(&self, _data: &[u8], _peer: &crate::nft::Peer<'_>) -> Vec<Vec<u8>> {
        Vec::new()
    }
}

type Route2 = crate::route::Entry;

/// An interface in a network namespace (for netlink dumps): the host's (or lo in a new namespace),
/// or a virtual device ([`crate::netdev`], with the number of the namespace its peer is in).
pub(crate) enum Link {
    Host(Interface),
    Virt(crate::netdev::View, Option<i32>),
}

impl Link {
    fn index(&self) -> u32 {
        match self {
            Link::Host(i) => i.index,
            Link::Virt(v, _) => v.index,
        }
    }

    fn name(&self) -> &[u8] {
        match self {
            Link::Host(i) => i.name.as_bytes(),
            Link::Virt(v, _) => &v.name,
        }
    }
}

/// Requests in the boot network namespace that change a host interface (`RTM_NEWLINK`,
/// `RTM_SETLINK`; Linux's `rtnl_setlink`, `__rtnl_newlink`). The app cannot change the host's
/// interfaces, so only requests that change nothing are accepted: after the flags are combined by
/// `rtnl_dev_combine_flags`, the bits `__dev_change_flags` can change are the same as now, and the
/// name and MTU carried are also the same as now. That is what dockerd does when it brings up the
/// already-up lo for a container's default network sandbox (`loopbackUp`). Anything that really
/// changes something is still EOPNOTSUPP, and so is creating an interface (`NLM_F_CREATE`, veth,
/// bridge and the like). `net_admin`: whether there is `CAP_NET_ADMIN` (EPERM without it; Linux
/// checks before parsing). None: neither of these two requests.
pub(crate) fn configure_host_link(
    ifaces: &[Interface],
    net_admin: bool,
    req: &[u8],
) -> Option<i32> {
    const RTM_SETLINK: u16 = 19;
    const NLM_F_REPLACE: u16 = 0x100;
    const NLM_F_EXCL: u16 = 0x200;
    const NLM_F_CREATE: u16 = 0x400;
    const ENODEV: i32 = 19;
    // The bits __dev_change_flags changes on request (IFF_UP, IFF_PROMISC, IFF_ALLMULTI are
    // handled separately, and count too)
    const CHANGEABLE: u32 =
        0x1 | 0x4 | 0x20 | 0x80 | 0x100 | 0x200 | 0x1000 | 0x2000 | 0x4000 | 0x8000;
    let ty = u16::from_le_bytes([req[4], req[5]]);
    if ty != RTM_NEWLINK && ty != RTM_SETLINK {
        return None;
    }
    if !net_admin {
        return Some(EPERM);
    }
    let flags = u16::from_le_bytes([req[6], req[7]]);
    // struct ifinfomsg: address family, type, index, flags, flags to change
    let Some(m) = req.get(16..32) else {
        return Some(EINVAL);
    };
    let index = i32::from_le_bytes(m[4..8].try_into().unwrap());
    let ifi_flags = u32::from_le_bytes(m[8..12].try_into().unwrap());
    let change = u32::from_le_bytes(m[12..16].try_into().unwrap());
    let attrs = crate::route::attrs(&req[32..]);
    let name = attrs
        .iter()
        .find(|(t, _)| *t == IFLA_IFNAME)
        .map(|(_, d)| d.split(|&c| c == 0).next().unwrap_or_default());
    let newlink = ty == RTM_NEWLINK;
    if newlink && index < 0 {
        return Some(EINVAL);
    }
    // Look up by index, or by name when the index is not positive; when neither is given
    // RTM_SETLINK is EINVAL and RTM_NEWLINK counts as not found
    if !newlink && index <= 0 && name.is_none() {
        return Some(EINVAL);
    }
    let found = ifaces.iter().find(|i| {
        if index > 0 {
            i.index as i32 == index
        } else {
            name.is_some() && name == Some(i.name.as_bytes())
        }
    });
    let Some(dev) = found else {
        // Tuxport cannot create one when asked to (Linux also gives EOPNOTSUPP when it has no
        // driver for that kind of interface)
        return Some(if newlink && flags & NLM_F_CREATE != 0 {
            EOPNOTSUPP
        } else {
            ENODEV
        });
    };
    if newlink && flags & NLM_F_EXCL != 0 {
        return Some(EEXIST);
    }
    if newlink && flags & NLM_F_REPLACE != 0 {
        return Some(EOPNOTSUPP);
    }
    let same = attrs.iter().all(|&(t, d)| match t {
        IFLA_IFNAME => d.split(|&c| c == 0).next() == Some(dev.name.as_bytes()),
        IFLA_MTU => d.get(..4) == Some(&dev.mtu.to_le_bytes()[..]),
        _ => false,
    });
    // When ifi_change is 0 the whole ifi_flags counts as wanted (Linux's "bugwards
    // compatibility"); when both are 0 the flags are not changed
    let wanted = if change != 0 {
        (ifi_flags & change) | (dev.flags & !change)
    } else {
        ifi_flags
    };
    let flags_same = (ifi_flags == 0 && change == 0) || (wanted ^ dev.flags) & CHANGEABLE == 0;
    Some(if same && flags_same { 0 } else { EOPNOTSUPP })
}

/// One interface's `RTM_NEWLINK` (Linux's `rtnl_fill_ifinfo`, attributes in the same order): the
/// items `ip link` uses and programs commonly read; device parameters such as `IFLA_AF_SPEC` are
/// missing. The transmit queue length `txqlen` comes from the host, or Linux's default 1000 when
/// unavailable; guest packets are handed to host sockets and the interface has no queueing
/// discipline of its own, so qdisc is always noqueue.
fn link_message(i: &Interface, st: IfStats, txqlen: Option<u32>) -> Vec<u8> {
    let loopback = i.flags & IFF_LOOPBACK != 0;
    // struct ifinfomsg: family, pad, type, index, flags, change
    let mut p = vec![0u8, 0];
    let ty = if loopback {
        ARPHRD_LOOPBACK
    } else {
        ARPHRD_ETHER
    };
    p.extend_from_slice(&ty.to_le_bytes());
    p.extend_from_slice(&(i.index as i32).to_le_bytes());
    p.extend_from_slice(&i.flags.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    let mut name = i.name.clone().into_bytes();
    name.push(0);
    attr(&mut p, IFLA_IFNAME, &name);
    attr(&mut p, IFLA_TXQLEN, &txqlen.unwrap_or(1000).to_le_bytes());
    attr(&mut p, IFLA_OPERSTATE, &[operstate(i)]);
    attr(&mut p, IFLA_LINKMODE, &[0]);
    attr(&mut p, IFLA_MTU, &i.mtu.to_le_bytes());
    attr(&mut p, IFLA_GROUP, &0u32.to_le_bytes());
    attr(&mut p, IFLA_PROMISCUITY, &0u32.to_le_bytes());
    attr(&mut p, IFLA_NUM_TX_QUEUES, &1u32.to_le_bytes());
    attr(&mut p, IFLA_NUM_RX_QUEUES, &1u32.to_le_bytes());
    attr(
        &mut p,
        IFLA_CARRIER,
        &[u8::from(i.flags & IFF_RUNNING != 0)],
    );
    // lo in a new network namespace that is not up yet has no qdisc yet (crate::netns)
    let qdisc: &[u8] = if i.flags & (IFF_LOOPBACK | IFF_UP) == IFF_LOOPBACK {
        b"noop\0"
    } else {
        b"noqueue\0"
    };
    attr(&mut p, IFLA_QDISC, qdisc);
    attr(&mut p, IFLA_PROTO_DOWN, &[0]);
    let mac = i.mac.unwrap_or([0; 6]);
    attr(&mut p, IFLA_ADDRESS, &mac);
    let bcast = if loopback { [0; 6] } else { [0xff; 6] };
    attr(&mut p, IFLA_BROADCAST, &bcast);
    // Traffic counters (crate::traffic): struct rtnl_link_stats64 is 25 u64s in Linux 6.1, and
    // rtnl_link_stats 24 u32s; the first four are packets in, packets out, bytes in and bytes
    // out, and everything else is 0
    let head = [st.rx_packets, st.tx_packets, st.rx_bytes, st.tx_bytes];
    let field = |n: usize| head.get(n).copied().unwrap_or(0);
    let stats64: Vec<u8> = (0..25).flat_map(|n| field(n).to_le_bytes()).collect();
    attr(&mut p, IFLA_STATS64, &stats64);
    let stats: Vec<u8> = (0..24)
        .flat_map(|n| (field(n) as u32).to_le_bytes())
        .collect();
    attr(&mut p, IFLA_STATS, &stats);
    p
}

/// The interface's state (`IFLA_OPERSTATE`): as with `/sys/class/net/<interface>/operstate`, the
/// loopback interface is unknown (Linux's loopback driver does not set it), one that is up and
/// connected is up, and the others are down.
pub(crate) fn operstate(i: &Interface) -> u8 {
    if i.flags & IFF_LOOPBACK != 0 && i.flags & IFF_UP != 0 {
        IF_OPER_UNKNOWN
    } else if i.flags & IFF_UP != 0 && i.flags & IFF_RUNNING != 0 {
        IF_OPER_UP
    } else {
        IF_OPER_DOWN
    }
}

/// The `RTM_NEWADDR`s on one interface (Linux's `inet_fill_ifaddr`, `inet6_fill_ifaddr`,
/// attributes in the same order). Addresses are all taken as never expiring (`IFA_F_PERMANENT`,
/// `ip addr` shows forever); their creation and update times are not known and are 0.
fn addr_messages(i: &Interface, family: u8) -> Vec<Vec<u8>> {
    // struct ifa_cacheinfo: preferred, valid (all ones is permanent), cstamp, tstamp
    let mut cacheinfo = [0xffu8; 16];
    cacheinfo[8..].fill(0);
    i.addrs
        .iter()
        .filter(|(f, ..)| family == 0 || *f as u8 == family)
        .map(|(f, addr, prefix)| {
            let link_local = *f == AF_INET6 && addr[0] == 0xfe && addr[1] & 0xc0 == 0x80;
            let scope = if i.flags & IFF_LOOPBACK != 0 {
                RT_SCOPE_HOST
            } else if link_local {
                RT_SCOPE_LINK
            } else {
                RT_SCOPE_UNIVERSE
            };
            // struct ifaddrmsg: family, prefixlen, flags, scope, index
            let mut p = vec![*f as u8, *prefix, IFA_F_PERMANENT, scope];
            p.extend_from_slice(&i.index.to_le_bytes());
            let flags = u32::from(IFA_F_PERMANENT).to_le_bytes();
            attr(&mut p, IFA_ADDRESS, addr);
            if *f == AF_INET {
                attr(&mut p, IFA_LOCAL, addr);
                if let Some(b) = broadcast_of(i, addr, *prefix) {
                    attr(&mut p, IFA_BROADCAST, &b);
                }
                let mut name = i.name.clone().into_bytes();
                name.push(0);
                attr(&mut p, IFA_LABEL, &name);
                attr(&mut p, IFA_FLAGS, &flags);
                attr(&mut p, IFA_CACHEINFO, &cacheinfo);
            } else {
                attr(&mut p, IFA_CACHEINFO, &cacheinfo);
                attr(&mut p, IFA_FLAGS, &flags);
                if i.flags & IFF_LOOPBACK != 0 {
                    attr(&mut p, IFA_PROTO, &[IFAPROT_KERNEL_LO]);
                } else if link_local {
                    attr(&mut p, IFA_PROTO, &[IFAPROT_KERNEL_LL]);
                }
            }
            p
        })
        .collect()
}

/// The broadcast address of IPv4 address `addr`/`prefix`: when the interface supports broadcast
/// and the subnet has more than two addresses, the subnet's last address (what DHCP and
/// `ip addr add ... brd +` set), consistent with the broadcast route in the local table.
fn broadcast_of(i: &Interface, addr: &[u8], prefix: u8) -> Option<[u8; 4]> {
    if i.flags & IFF_BROADCAST == 0 || prefix >= 31 {
        return None;
    }
    let ip = u32::from_be_bytes(addr.try_into().ok()?);
    Some((ip | u32::MAX >> prefix).to_be_bytes())
}

// ---- rtnetlink notifications ----

/// The rtnetlink groups notifications go to (`RTNLGRP_*`).
const RTNLGRP_LINK: u32 = 1;
const RTNLGRP_IPV4_IFADDR: u32 = 5;
const RTNLGRP_IPV4_ROUTE: u32 = 7;
const RTNLGRP_IPV6_IFADDR: u32 = 9;
const RTNLGRP_IPV6_ROUTE: u32 = 11;
/// Those groups as bits of [`NetlinkSocket::groups`].
const RTNL_NOTIFIED: u64 = 1 << (RTNLGRP_LINK - 1)
    | 1 << (RTNLGRP_IPV4_IFADDR - 1)
    | 1 << (RTNLGRP_IPV4_ROUTE - 1)
    | 1 << (RTNLGRP_IPV6_IFADDR - 1)
    | 1 << (RTNLGRP_IPV6_ROUTE - 1);
/// The most datagrams a socket holds before notifications are dropped (about its 212992-byte
/// receive buffer at a typical notification's size).
const NOTE_QUEUE: usize = 1024;

/// A namespace's links, addresses and routes as rtnetlink dumps encode them (message bodies, in
/// dump order). Taken before and after a change request, the difference is what Linux notifies
/// as it makes each change ([`Snapshot::changes`]).
pub(crate) struct Snapshot {
    links: Vec<Vec<u8>>,
    addrs: Vec<Vec<u8>>,
    routes: Vec<Vec<u8>>,
}

impl Snapshot {
    /// The namespace `host` serves.
    pub(crate) fn take(host: &dyn Host) -> Snapshot {
        let links = host.links();
        let mut addrs = Vec::new();
        for f in [AF_INET, AF_INET6] {
            for l in &links {
                addrs.extend(match l {
                    Link::Host(i) => addr_messages(i, f as u8),
                    Link::Virt(v, _) => crate::netdev::addr_messages(v, f),
                });
            }
        }
        let routes = host
            .tables()
            .iter()
            .filter(|e| e.family == AF_INET || e.family == AF_INET6)
            .map(crate::route::route_message)
            .collect();
        let links = links
            .iter()
            .map(|l| match l {
                Link::Host(i) => link_message(i, host.stats(&i.name), host.tx_queue_len(&i.name)),
                Link::Virt(v, nsid) => crate::netdev::link_message(v, *nsid),
            })
            .collect();
        Snapshot {
            links,
            addrs,
            routes,
        }
    }

    /// The notifications (group, message) for the change from `before` to this one, in the order
    /// Linux makes them: links that appeared or changed (`RTM_NEWLINK`, statistics aside),
    /// addresses that went and came, routes that went and came, links that went
    /// (`RTM_DELLINK`). An address or route that changed in place ([`addr_identity`], [`route_identity`]) is only notified
    /// as new. They carry no sequence number, port or flags.
    pub(crate) fn changes(&self, before: &Snapshot) -> Vec<(u32, Vec<u8>)> {
        let index = |m: &[u8]| {
            m.get(4..8)
                .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
        };
        let find = |set: &[Vec<u8>], i| set.iter().find(|m| index(m) == i).cloned();
        let note = |group, ty, body: &[u8]| (group, message(ty, 0, 0, 0, body));
        let mut out = Vec::new();
        for m in &self.links {
            if find(&before.links, index(m))
                .is_none_or(|old| without_stats(&old) != without_stats(m))
            {
                out.push(note(RTNLGRP_LINK, RTM_NEWLINK, m));
            }
        }
        let groups = [
            (
                &before.addrs,
                &self.addrs,
                addr_identity as Identity,
                RTM_DELADDR,
                RTM_NEWADDR,
                RTNLGRP_IPV4_IFADDR,
                RTNLGRP_IPV6_IFADDR,
            ),
            (
                &before.routes,
                &self.routes,
                route_identity,
                RTM_DELROUTE,
                RTM_NEWROUTE,
                RTNLGRP_IPV4_ROUTE,
                RTNLGRP_IPV6_ROUTE,
            ),
        ];
        for (old, new, identity, del, add, v4, v6) in groups {
            let group = |m: &[u8]| match m.first().map(|&f| f as i32) {
                Some(AF_INET) => v4,
                _ => v6,
            };
            // Hashed: a namespace can hold thousands of addresses and routes
            let kept: HashSet<_> = new.iter().map(|m| identity(m)).collect();
            let was: HashSet<&Vec<u8>> = old.iter().collect();
            let gone = |m: &&Vec<u8>| !kept.contains(&identity(m));
            out.extend(old.iter().filter(gone).map(|m| note(group(m), del, m)));
            out.extend(
                new.iter()
                    .filter(|m| !was.contains(m))
                    .map(|m| note(group(m), add, m)),
            );
        }
        for m in &before.links {
            if find(&self.links, index(m)).is_none() {
                out.push(note(RTNLGRP_LINK, RTM_DELLINK, m));
            }
        }
        out
    }
}

/// The identity of an address or route message (header bytes, attributes).
type Identity = for<'a> fn(&'a [u8]) -> (Vec<u8>, Vec<(u16, &'a [u8])>);

/// What identifies an address: family, prefix length, interface (`struct ifaddrmsg` without
/// flags and scope), address and local address.
fn addr_identity(m: &[u8]) -> (Vec<u8>, Vec<(u16, &[u8])>) {
    let head = [&m[..2.min(m.len())], m.get(4..8).unwrap_or_default()].concat();
    (head, picked(m.get(8..), &[IFA_ADDRESS, IFA_LOCAL]))
}

/// What identifies a route: family, destination length, TOS and table (of `struct rtmsg`), then
/// destination, priority and table attributes.
fn route_identity(m: &[u8]) -> (Vec<u8>, Vec<(u16, &[u8])>) {
    const RTA_DST: u16 = 1;
    const RTA_PRIORITY: u16 = 6;
    const RTA_TABLE: u16 = 15;
    let head = [0, 1, 3, 4]
        .iter()
        .filter_map(|&i| m.get(i).copied())
        .collect();
    (
        head,
        picked(m.get(12..), &[RTA_DST, RTA_PRIORITY, RTA_TABLE]),
    )
}

/// The attributes of types `types` in `attrs`.
fn picked<'a>(attrs: Option<&'a [u8]>, types: &[u16]) -> Vec<(u16, &'a [u8])> {
    crate::route::attrs(attrs.unwrap_or_default())
        .into_iter()
        .filter(|(t, _)| types.contains(t))
        .collect()
}

/// A link message's header and attributes without its statistics, which change with traffic.
fn without_stats(m: &[u8]) -> (&[u8], Vec<(u16, &[u8])>) {
    let (head, attrs) = m.split_at(m.len().min(16));
    let attrs = crate::route::attrs(attrs)
        .into_iter()
        .filter(|&(t, _)| t != IFLA_STATS && t != IFLA_STATS64)
        .collect();
    (head, attrs)
}

// ---- NETLINK_GENERIC ----
//
// Only the nlctrl family that Linux always has (GENL_ID_CTRL). Subsystems such as nl80211,
// ethtool, wireguard and TASKSTATS are absent, and querying them gives ENOENT, as on a Linux
// without them built in: iw, ethtool, wg and iotop report it or fall back to other approaches
// (before, socket itself gave EPROTONOSUPPORT, which some programs do not expect). The answers'
// content, error numbers and the requests attached to error messages are byte-identical to Linux
// 6.8 (differential test).

const GENL_ID_CTRL: u16 = 0x10;
/// Message types below this are netlink's own control messages (`NLMSG_NOOP` and so on).
const NLMSG_MIN_TYPE: u16 = 0x10;
/// The rtnetlink message types Linux 6.1 knows (`RTM_BASE` to `RTM_MAX`): their lowest two bits
/// tell new, delete, get and set apart.
const RTM_BASE: u16 = 16;
const RTM_MAX: u16 = 123;
/// The ack carries only the request's message header.
const NLM_F_CAPPED: u16 = 0x100;
const CTRL_CMD_NEWFAMILY: u8 = 1;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_CMD_GETPOLICY: u8 = 10;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const CTRL_ATTR_VERSION: u16 = 3;
const CTRL_ATTR_HDRSIZE: u16 = 4;
const CTRL_ATTR_MAXATTR: u16 = 5;
const CTRL_ATTR_OPS: u16 = 6;
const CTRL_ATTR_MCAST_GROUPS: u16 = 7;
const CTRL_ATTR_OP_ID: u16 = 1;
const CTRL_ATTR_OP_FLAGS: u16 = 2;
const CTRL_ATTR_MCAST_GRP_NAME: u16 = 1;
const CTRL_ATTR_MCAST_GRP_ID: u16 = 2;
const GENL_CMD_CAP_DO: u32 = 2;
const GENL_CMD_CAP_DUMP: u32 = 4;
const GENL_CMD_CAP_HASPOL: u32 = 8;
const NLA_TYPE_MASK: u16 = 0x3fff;
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EEXIST: i32 = 17;
const EINVAL: i32 = 22;
const EOPNOTSUPP: i32 = 95;

/// Answers one request `req` (the whole message) on `NETLINK_GENERIC`; `pid` is this socket's
/// `nl_pid`. Returns the datagrams: as on Linux, the answer and the ack are one each
/// (`genlmsg_reply`, `netlink_ack`), so a program reading once gets only the answer.
fn generic_request(req: &[u8], pid: u32) -> Vec<Vec<u8>> {
    let ty = u16::from_le_bytes([req[4], req[5]]);
    let flags = u16::from_le_bytes([req[6], req[7]]);
    let seq = u32::from_le_bytes(req[8..12].try_into().unwrap());
    let reply = |ty, flags, payload: &[u8]| message(ty, flags, seq, pid, payload);
    let dump = flags & NLM_F_DUMP == NLM_F_DUMP;
    // Linux's netlink_rcv_skb: non-requests and control messages are acked only when an ack is
    // asked for
    let r = if flags & NLM_F_REQUEST == 0 || ty < NLMSG_MIN_TYPE {
        Ok(None)
    } else if ty != GENL_ID_CTRL {
        Err(ENOENT)
    } else if req.len() < 20 {
        Err(EINVAL)
    } else {
        match req[16] {
            // The dump is small, and as on Linux NLMSG_DONE follows in the same datagram
            CTRL_CMD_GETFAMILY if dump => {
                let mut out = reply(GENL_ID_CTRL, NLM_F_MULTI, &nlctrl_family());
                out.extend(reply(NLMSG_DONE, NLM_F_MULTI, &0i32.to_le_bytes()));
                Ok(Some(out))
            }
            CTRL_CMD_GETFAMILY => {
                names_nlctrl(req).map(|()| Some(reply(GENL_ID_CTRL, 0, &nlctrl_family())))
            }
            // Policy dumps are not implemented: it can only tell that the requested family does not
            // exist
            CTRL_CMD_GETPOLICY if dump => names_nlctrl(req).and(Err(EOPNOTSUPP)),
            _ => Err(EOPNOTSUPP),
        }
    };
    match r {
        // No separate ack after a dump (Linux's netlink_dump_start makes netlink_rcv_skb skip the
        // ack)
        Ok(answer) => {
            let mut out: Vec<Vec<u8>> = answer.into_iter().collect();
            if flags & NLM_F_ACK != 0 && (!dump || out.is_empty()) {
                out.push(ack(req, 0, pid));
            }
            out
        }
        Err(e) => vec![ack(req, e, pid)],
    }
}

/// nlctrl's `CTRL_CMD_NEWFAMILY`: the genlmsghdr and attributes, the same as Linux 6.8's
/// `ctrl_fill_info` (since 6.2 each nlctrl command has its own policy, and the family's maxattr
/// is 0).
fn nlctrl_family() -> Vec<u8> {
    let mut p = vec![CTRL_CMD_NEWFAMILY, 2, 0, 0];
    attr(&mut p, CTRL_ATTR_FAMILY_NAME, b"nlctrl\0");
    attr(&mut p, CTRL_ATTR_FAMILY_ID, &GENL_ID_CTRL.to_le_bytes());
    attr(&mut p, CTRL_ATTR_VERSION, &2u32.to_le_bytes());
    attr(&mut p, CTRL_ATTR_HDRSIZE, &0u32.to_le_bytes());
    attr(&mut p, CTRL_ATTR_MAXATTR, &0u32.to_le_bytes());
    let caps = [
        (
            CTRL_CMD_GETFAMILY,
            GENL_CMD_CAP_DO | GENL_CMD_CAP_DUMP | GENL_CMD_CAP_HASPOL,
        ),
        (CTRL_CMD_GETPOLICY, GENL_CMD_CAP_DUMP | GENL_CMD_CAP_HASPOL),
    ];
    let mut ops = Vec::new();
    for (n, (cmd, flags)) in caps.into_iter().enumerate() {
        let mut op = Vec::new();
        attr(&mut op, CTRL_ATTR_OP_ID, &u32::from(cmd).to_le_bytes());
        attr(&mut op, CTRL_ATTR_OP_FLAGS, &flags.to_le_bytes());
        attr(&mut ops, n as u16 + 1, &op);
    }
    attr(&mut p, CTRL_ATTR_OPS, &ops);
    let mut group = Vec::new();
    attr(
        &mut group,
        CTRL_ATTR_MCAST_GRP_ID,
        &u32::from(GENL_ID_CTRL).to_le_bytes(),
    );
    attr(&mut group, CTRL_ATTR_MCAST_GRP_NAME, b"notify\0");
    let mut groups = Vec::new();
    attr(&mut groups, 1, &group);
    attr(&mut p, CTRL_ATTR_MCAST_GROUPS, &groups);
    p
}

/// Whether request `req` (genlmsghdr and attributes after the message header) refers to the
/// nlctrl family: looked up by `CTRL_ATTR_FAMILY_ID` or `CTRL_ATTR_FAMILY_NAME` (Linux's
/// `ctrl_getfamily`); EINVAL when neither is present, ENOENT when not found.
fn names_nlctrl(req: &[u8]) -> Result<(), i32> {
    let (mut id, mut name) = (None, None);
    let mut b = req.get(20..).unwrap_or_default();
    while b.len() >= 4 {
        let len = u16::from_le_bytes([b[0], b[1]]) as usize;
        if len < 4 || len > b.len() {
            break;
        }
        let data = &b[4..len];
        match u16::from_le_bytes([b[2], b[3]]) & NLA_TYPE_MASK {
            CTRL_ATTR_FAMILY_ID if data.len() >= 2 => {
                id = Some(u16::from_le_bytes([data[0], data[1]]))
            }
            CTRL_ATTR_FAMILY_NAME => {
                name = Some(data.split(|&c| c == 0).next().unwrap_or_default());
            }
            _ => {}
        }
        b = &b[len.next_multiple_of(4).min(b.len())..];
    }
    match (id, name) {
        (Some(GENL_ID_CTRL), _) | (_, Some(b"nlctrl")) => Ok(()),
        (None, None) => Err(EINVAL),
        _ => Err(ENOENT),
    }
}

// ---- Netlink messages ----

pub(crate) fn attr(b: &mut Vec<u8>, ty: u16, data: &[u8]) {
    b.extend_from_slice(&((4 + data.len()) as u16).to_le_bytes());
    b.extend_from_slice(&ty.to_le_bytes());
    b.extend_from_slice(data);
    align4(b);
}

/// A netlink message: the message header plus the payload.
pub(crate) fn message(ty: u16, flags: u16, seq: u32, pid: u32, payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(16 + payload.len());
    b.extend_from_slice(&((16 + payload.len()) as u32).to_le_bytes());
    b.extend_from_slice(&ty.to_le_bytes());
    b.extend_from_slice(&flags.to_le_bytes());
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&pid.to_le_bytes());
    b.extend_from_slice(payload);
    align4(&mut b);
    b
}

// ---- NETLINK_SOCK_DIAG ----
//
// Answered as by a Linux 6.1 kernel built with sock_diag but none of its family modules (inet,
// unix, netlink, packet diag): every query is refused, as `sock_diag_rcv_msg` and
// `__sock_diag_cmd` refuse it without a handler. iproute2's ss takes ENOENT and EOPNOTSUPP on this
// protocol quietly as "not supported" and reads /proc/net instead; before, the socket itself was
// EPROTONOSUPPORT, and ss printed "Cannot open netlink socket" for every table it listed.

/// `TCPDIAG_GETSOCK` and `DCCPDIAG_GETSOCK`: the old inet_diag requests.
const TCPDIAG_GETSOCK: u16 = 18;
const DCCPDIAG_GETSOCK: u16 = 19;
/// `SOCK_DIAG_BY_FAMILY` and `SOCK_DESTROY`, which start with a `sock_diag_req` (family, protocol).
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const SOCK_DESTROY: u16 = 21;
/// Linux 6.1's `AF_MAX`.
const AF_MAX: u8 = 46;

/// Answers one request `req` (the whole message) on `NETLINK_SOCK_DIAG` (Linux's
/// `sock_diag_rcv_msg` under `netlink_rcv_skb`): the old inet_diag requests `EOPNOTSUPP`, family
/// requests with a short body or a family at or past `AF_MAX` `EINVAL` and otherwise `ENOENT` (no
/// family handler), other types `EINVAL`; non-requests and control messages are acked only when
/// asked. `cap` is `NETLINK_CAP_ACK`.
fn sock_diag_request(req: &[u8], pid: u32, cap: bool) -> Vec<Vec<u8>> {
    let ty = u16::from_le_bytes([req[4], req[5]]);
    let flags = u16::from_le_bytes([req[6], req[7]]);
    let err = if flags & NLM_F_REQUEST == 0 || ty < NLMSG_MIN_TYPE {
        0
    } else {
        match ty {
            TCPDIAG_GETSOCK | DCCPDIAG_GETSOCK => EOPNOTSUPP,
            SOCK_DIAG_BY_FAMILY | SOCK_DESTROY if req.len() < 18 || req[16] >= AF_MAX => EINVAL,
            SOCK_DIAG_BY_FAMILY | SOCK_DESTROY => ENOENT,
            _ => EINVAL,
        }
    };
    if err == 0 && flags & NLM_F_ACK == 0 {
        return Vec::new();
    }
    vec![ack_ext(req, err, pid, None, cap)]
}

/// The ack or error for request `req` (the whole message) (Linux's `netlink_ack`): on success only
/// the request's message header is attached, with `NLM_F_CAPPED`; on error the whole request is
/// attached (`NETLINK_CAP_ACK` not set).
fn ack(req: &[u8], err: i32, pid: u32) -> Vec<u8> {
    ack_ext(req, err, pid, None, false)
}

/// Like [`ack`], also attaching the extack message per `NETLINK_EXT_ACK` (`NLMSGERR_ATTR_MSG`,
/// with `NLM_F_ACK_TLVS`); `cap` means `NETLINK_CAP_ACK` is set.
pub(crate) fn ack_ext(req: &[u8], err: i32, pid: u32, msg: Option<&str>, cap: bool) -> Vec<u8> {
    const NLM_F_ACK_TLVS: u16 = 0x200;
    const NLMSGERR_ATTR_MSG: u16 = 1;
    let seq = u32::from_le_bytes(req[8..12].try_into().unwrap());
    let mut p = (-err).to_le_bytes().to_vec();
    let mut flags = 0;
    if err == 0 || cap {
        p.extend_from_slice(&req[..16]);
        flags |= NLM_F_CAPPED;
    } else {
        p.extend_from_slice(req);
    }
    if let Some(m) = msg {
        align4(&mut p);
        let mut s = m.as_bytes().to_vec();
        s.push(0);
        attr(&mut p, NLMSGERR_ATTR_MSG, &s);
        flags |= NLM_F_ACK_TLVS;
    }
    message(NLMSG_ERROR, flags, seq, pid, &p)
}

fn align4(b: &mut Vec<u8>) {
    b.resize(b.len().next_multiple_of(4), 0);
}

// ---- /proc/net ----

const SOL_SOCKET: i32 = 1;
const SO_PROTOCOL: i32 = 38;
const TCP_INFO: i32 = 11;
/// The st column of `/proc/net` (Linux's TCP states; UDP borrows two of them too).
const TCP_ESTABLISHED: u8 = 1;
const TCP_CLOSE: u8 = 7;
const TCP_LISTEN: u8 = 10;

/// The tables under `/proc/net` that list sockets.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Table {
    Tcp,
    Udp,
    /// Ping sockets (`icmp`, `icmp6`).
    Icmp,
    /// Raw sockets (`raw`, `raw6`).
    Raw,
}

/// `/proc/net/{tcp,udp,icmp,raw}` and the matching IPv6 files: this kind of socket of `family`.
/// Addresses and TCP state are queried from the host socket; queue lengths and timers are all 0.
/// As on Linux, the peer port of ping and raw sockets is always 0 and the local port of raw
/// sockets is the protocol number; IPv4 tcp, udp and icmp lines are padded with spaces to a fixed
/// width, raw and IPv6 lines are not.
pub(crate) fn proc_listing(
    sockets: &[(crate::syscall::SocketRef, u32)],
    family: i32,
    table: Table,
    portmap: &crate::cell::RefCell<crate::portmap::PortMap>,
    w: &crate::netns::World,
) -> String {
    let (v6, tcp) = (family == AF_INET6, table == Table::Tcp);
    let width = match table {
        _ if v6 => 0,
        Table::Tcp => 149,
        Table::Udp | Table::Icmp => 127,
        Table::Raw => 0,
    };
    let address = if v6 {
        "local_address                         remote_address                        "
    } else {
        "local_address rem_address   "
    };
    let mut lines = vec![format!(
        "{}  sl  {address}st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode{}",
        if table == Table::Udp && !v6 { " " } else { "" },
        if tcp { "" } else { " ref pointer drops" },
    )];
    let int = |b: Result<Vec<u8>, i32>| {
        b.ok()
            .and_then(|b| Some(i32::from_le_bytes(b.get(..4)?.try_into().ok()?)))
    };
    let icmp = if v6 { IPPROTO_ICMPV6 } else { IPPROTO_ICMP };
    for (sock, uid) in sockets {
        let b = sock.borrow();
        let Socket::Inet(inet) = &*b else {
            continue;
        };
        let fd = inet.fd;
        let proto = int(tuxport_hal::net::getsockopt(fd, SOL_SOCKET, SO_PROTOCOL, 4));
        let listed = inet.family == family
            && match table {
                // MPTCP sockets are listed here too (the first subflow is a TCP socket; Go 1.24
                // and later listen with it by default)
                Table::Tcp => {
                    inet.ty == SOCK_STREAM && matches!(proto, Some(IPPROTO_TCP | IPPROTO_MPTCP))
                }
                Table::Udp => inet.ty == SOCK_DGRAM && proto == Some(IPPROTO_UDP),
                Table::Icmp => !inet.raw && inet.ty == SOCK_DGRAM && proto == Some(icmp),
                Table::Raw => inet.raw,
            };
        if !listed {
            continue;
        }
        // Addresses as the guest sees them (crate::netns), and mapped low ports reported as the
        // guest's (crate::portmap)
        let me = inet.net.clone().unwrap_or_else(|| w.all[0].clone());
        let unmap =
            |a: Vec<u8>, local: bool| crate::syscall::unmap_addr(portmap, w, inet, a, local);
        let host_peer = tuxport_hal::net::sockname(fd, true).ok();
        let local = tuxport_hal::net::sockname(fd, false).ok().map(|a| {
            let role = crate::netns::Role::Local {
                any: inet.any,
                vaddr: inet.vaddr,
                peer: host_peer.as_deref(),
            };
            crate::netns::from_host(w, &me, &unmap(a, true), role, inet.icmp6())
        });
        let peer = host_peer
            .clone()
            .map(|a| {
                let role = crate::netns::Role::Peer;
                crate::netns::from_host(w, &me, &unmap(a, false), role, inet.icmp6())
            })
            .or_else(|| inet.peer.clone());
        let state = if tcp {
            tuxport_hal::net::getsockopt(fd, IPPROTO_TCP, TCP_INFO, 8)
                .ok()
                .and_then(|b| b.first().copied())
                .unwrap_or(TCP_CLOSE)
        } else if peer.is_some() {
            TCP_ESTABLISHED
        } else {
            TCP_CLOSE
        };
        let ino = crate::pseudo::socket_ino(sock);
        let tail = if !tcp {
            "2 0000000000000000 0".to_owned()
        } else if state == TCP_LISTEN {
            "1 0000000000000000 100 0 0 10 0".to_owned()
        } else {
            "2 0000000000000000 20 4 30 10 -1".to_owned()
        };
        let (local_port, peer_port) = match table {
            Table::Raw => (Some(proto.unwrap_or(0) as u16), Some(0)),
            Table::Icmp => (None, Some(0)),
            _ => (None, None),
        };
        let sl = lines.len() - 1;
        lines.push(format!(
            "{sl:>w$}: {} {} {state:02X} 00000000:00000000 00:00000000 00000000 {uid:5} {:8} {ino} {tail}",
            proc_addr(local.as_deref(), v6, local_port),
            proc_addr(peer.as_deref(), v6, peer_port),
            0,
            w = if tcp || table == Table::Raw && !v6 { 4 } else { 5 },
        ));
    }
    lines.iter().map(|l| format!("{l:<width$}\n")).collect()
}

/// An address in `/proc/net` format: IPv4 is one 32-bit word and IPv6 four, printed as their
/// in-memory bytes read as little-endian (the same as Linux on arm64); the port as a number.
fn proc_addr(addr: Option<&[u8]>, v6: bool, port: Option<u16>) -> String {
    let addr = addr.unwrap_or_default();
    let port = port.unwrap_or_else(|| {
        addr.get(2..4)
            .map_or(0, |p| u16::from_be_bytes([p[0], p[1]]))
    });
    let (start, words) = if v6 { (8, 4) } else { (4, 1) };
    let mut s = String::new();
    for w in 0..words {
        let at = start + w * 4;
        let v = addr
            .get(at..at + 4)
            .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
        write!(s, "{v:08X}").unwrap();
    }
    write!(s, ":{port:04X}").unwrap();
    s
}

/// `/proc/net/sockstat`, `sockstat6` (`v6`): how many guest sockets of each kind there are
/// (Linux's `sockstat_seq_show`). TCP inuse counts those listening or connected (not CLOSE),
/// alloc counts all TCP sockets (both address families together; on Linux it is system-wide);
/// UDP counts those with a bound port, raw counts all, and ping sockets are not shown.
/// Connections already closed in the host kernel but still in TIME_WAIT or orphaned are not seen
/// here, and the number of memory pages used is not known either; both are 0.
pub(crate) fn sockstat(sockets: &[(crate::syscall::SocketRef, u32)], v6: bool) -> String {
    let int = |b: Result<Vec<u8>, i32>| {
        b.ok()
            .and_then(|b| Some(i32::from_le_bytes(b.get(..4)?.try_into().ok()?)))
    };
    let family = if v6 { AF_INET6 } else { AF_INET };
    let (mut tcp, mut alloc, mut udp, mut raw) = (0, 0, 0, 0);
    for (sock, _) in sockets {
        let b = sock.borrow();
        let Socket::Inet(inet) = &*b else {
            continue;
        };
        let proto = int(tuxport_hal::net::getsockopt(
            inet.fd,
            SOL_SOCKET,
            SO_PROTOCOL,
            4,
        ));
        if inet.ty == SOCK_STREAM && proto == Some(IPPROTO_TCP) {
            alloc += 1;
            let state = tuxport_hal::net::getsockopt(inet.fd, IPPROTO_TCP, TCP_INFO, 8)
                .ok()
                .and_then(|b| b.first().copied());
            if inet.family == family && state.is_some_and(|s| s != TCP_CLOSE) {
                tcp += 1;
            }
            continue;
        }
        if inet.family != family {
            continue;
        }
        if inet.raw {
            raw += 1;
        } else if inet.ty == SOCK_DGRAM && proto == Some(IPPROTO_UDP) {
            let bound = tuxport_hal::net::sockname(inet.fd, false)
                .is_ok_and(|a| a.get(2..4).is_some_and(|p| p != [0, 0]));
            if bound || inet.stub_peer {
                udp += 1;
            }
        }
    }
    if v6 {
        format!(
            "TCP6: inuse {tcp}\nUDP6: inuse {udp}\nUDPLITE6: inuse 0\nRAW6: inuse {raw}\n\
             FRAG6: inuse 0 memory 0\n"
        )
    } else {
        format!(
            "sockets: used {}\nTCP: inuse {tcp} orphan 0 tw 0 alloc {alloc} mem 0\n\
             UDP: inuse {udp} mem 0\nUDPLITE: inuse 0\nRAW: inuse {raw}\nFRAG: inuse 0 memory 0\n",
            sockets.len()
        )
    }
}

/// The TCP fields in `/proc/net/snmp` (Linux 6.1's `snmp4_tcp_list`).
const SNMP_TCP: [&str; 15] = [
    "RtoAlgorithm",
    "RtoMin",
    "RtoMax",
    "MaxConn",
    "ActiveOpens",
    "PassiveOpens",
    "AttemptFails",
    "EstabResets",
    "CurrEstab",
    "InSegs",
    "OutSegs",
    "RetransSegs",
    "InErrs",
    "OutRsts",
    "InCsumErrors",
];

/// The UDP and UDP-Lite fields (`snmp4_udp_list`).
const SNMP_UDP: [&str; 9] = [
    "InDatagrams",
    "NoPorts",
    "InErrors",
    "OutDatagrams",
    "RcvbufErrors",
    "SndbufErrors",
    "InCsumErrors",
    "IgnoredMulti",
    "MemErrors",
];

/// The IP fields (`snmp4_ipstats_list`), preceded by Forwarding and DefaultTTL.
const SNMP_IP: [&str; 17] = [
    "InReceives",
    "InHdrErrors",
    "InAddrErrors",
    "ForwDatagrams",
    "InUnknownProtos",
    "InDiscards",
    "InDelivers",
    "OutRequests",
    "OutDiscards",
    "OutNoRoutes",
    "ReasmTimeout",
    "ReasmReqds",
    "ReasmOKs",
    "ReasmFails",
    "FragOKs",
    "FragFails",
    "FragCreates",
];

/// The per-type ICMP fields and type numbers (`icmpmibmap`), one set each for in and out.
const SNMP_ICMP: [(&str, u8); 11] = [
    ("DestUnreachs", 3),
    ("TimeExcds", 11),
    ("ParmProbs", 12),
    ("SrcQuenchs", 4),
    ("Redirects", 5),
    ("Echos", 8),
    ("EchoReps", 0),
    ("Timestamps", 13),
    ("TimestampReps", 14),
    ("AddrMasks", 17),
    ("AddrMaskReps", 18),
];
const ICMP_ECHOREPLY: u8 = 0;
const ICMP_ECHO: u8 = 8;

/// `/proc/net/snmp` (Linux 6.1's `snmp_seq_show`): IPv4 counters for IP and ICMP, and for TCP and
/// UDP, each group as one line of field names and one line of values. The counters come from
/// [`crate::traffic::Protocols`], and CurrEstab is the number of TCP sockets now ESTABLISHED; no
/// forwarding (there is no `ip_forward`), the TTL is the default 64, the RTO items are Linux's
/// initial values, and errors, retransmissions, resets and fragments the kernel layer cannot see
/// are all 0.
pub(crate) fn proc_snmp(
    sockets: &[(crate::syscall::SocketRef, u32)],
    p: &crate::traffic::Protocols,
) -> String {
    let established = sockets
        .iter()
        .filter(|(sock, _)| {
            let b = sock.borrow();
            let Socket::Inet(inet) = &*b else {
                return false;
            };
            (inet.ty, inet.proto) == (SOCK_STREAM, IPPROTO_TCP)
                && tuxport_hal::net::getsockopt(inet.fd, IPPROTO_TCP, TCP_INFO, 8)
                    .is_ok_and(|b| b.first() == Some(&TCP_ESTABLISHED))
        })
        .count() as u64;
    let line = |group: &str, names: &[&str], values: &[String]| {
        format!(
            "{group}: {}\n{group}: {}\n",
            names.join(" "),
            values.join(" ")
        )
    };
    let nums = |v: &[u64]| v.iter().map(u64::to_string).collect::<Vec<_>>();
    let mut out = String::new();

    let mut ip = vec!["2".to_owned(), "64".to_owned()];
    let mut counts = [0u64; SNMP_IP.len()];
    counts[0] = p.ip_in_packets; // InReceives
    counts[6] = p.ip_in_packets; // InDelivers
    counts[7] = p.ip_out_packets; // OutRequests
    ip.extend(nums(&counts));
    let names: Vec<&str> = ["Forwarding", "DefaultTTL"]
        .into_iter()
        .chain(SNMP_IP)
        .collect();
    out += &line("Ip", &names, &ip);

    let by_type = |ty: u8, rx: bool| match (ty, rx) {
        (ICMP_ECHOREPLY, true) => p.icmp_in_echo_replies,
        (ICMP_ECHO, false) => p.icmp_out_echoes,
        _ => 0,
    };
    let mut names = vec![
        "InMsgs".to_owned(),
        "InErrors".into(),
        "InCsumErrors".into(),
    ];
    let mut icmp = vec![p.icmp_in_echo_replies, 0, 0];
    for (name, ty) in SNMP_ICMP {
        names.push(format!("In{name}"));
        icmp.push(by_type(ty, true));
    }
    names.extend(["OutMsgs".to_owned(), "OutErrors".into()]);
    icmp.extend([p.icmp_out_echoes, 0]);
    for (name, ty) in SNMP_ICMP {
        names.push(format!("Out{name}"));
        icmp.push(by_type(ty, false));
    }
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    out += &line("Icmp", &names, &nums(&icmp));
    // List only types that are not 0: in before out, each by type number
    let msgs: Vec<(String, u64)> = [(ICMP_ECHOREPLY, true), (ICMP_ECHO, false)]
        .into_iter()
        .map(|(ty, rx)| {
            let dir = if rx { "In" } else { "Out" };
            (format!("{dir}Type{ty}"), by_type(ty, rx))
        })
        .filter(|(_, n)| *n != 0)
        .collect();
    if !msgs.is_empty() {
        let names: Vec<&str> = msgs.iter().map(|(n, _)| n.as_str()).collect();
        let values: Vec<u64> = msgs.iter().map(|(_, v)| *v).collect();
        out += &line("IcmpMsg", &names, &nums(&values));
    }

    let mut tcp = vec!["1".to_owned(), "200".into(), "120000".into(), "-1".into()];
    tcp.extend(nums(&[
        p.tcp_active_opens,
        p.tcp_passive_opens,
        p.tcp_attempt_fails,
        0,
        established,
        p.tcp_in_segs,
        p.tcp_out_segs,
        0,
        0,
        0,
        0,
    ]));
    out += &line("Tcp", &SNMP_TCP, &tcp);
    let mut udp = [0u64; SNMP_UDP.len()];
    udp[0] = p.udp_in_datagrams;
    udp[3] = p.udp_out_datagrams;
    out += &line("Udp", &SNMP_UDP, &nums(&udp));
    out += &line("UdpLite", &SNMP_UDP, &nums(&[0; SNMP_UDP.len()]));
    out
}

/// The TcpExt fields in `/proc/net/netstat` (Linux 6.1's `snmp4_net_list`).
const NETSTAT_TCP: &str = "SyncookiesSent SyncookiesRecv SyncookiesFailed EmbryonicRsts \
PruneCalled RcvPruned OfoPruned OutOfWindowIcmps LockDroppedIcmps ArpFilter TW TWRecycled \
TWKilled PAWSActive PAWSEstab DelayedACKs DelayedACKLocked DelayedACKLost ListenOverflows \
ListenDrops TCPHPHits TCPPureAcks TCPHPAcks TCPRenoRecovery TCPSackRecovery TCPSACKReneging \
TCPSACKReorder TCPRenoReorder TCPTSReorder TCPFullUndo TCPPartialUndo TCPDSACKUndo \
TCPLossUndo TCPLostRetransmit TCPRenoFailures TCPSackFailures TCPLossFailures \
TCPFastRetrans TCPSlowStartRetrans TCPTimeouts TCPLossProbes TCPLossProbeRecovery \
TCPRenoRecoveryFail TCPSackRecoveryFail TCPRcvCollapsed TCPBacklogCoalesce TCPDSACKOldSent \
TCPDSACKOfoSent TCPDSACKRecv TCPDSACKOfoRecv TCPAbortOnData TCPAbortOnClose \
TCPAbortOnMemory TCPAbortOnTimeout TCPAbortOnLinger TCPAbortFailed TCPMemoryPressures \
TCPMemoryPressuresChrono TCPSACKDiscard TCPDSACKIgnoredOld TCPDSACKIgnoredNoUndo \
TCPSpuriousRTOs TCPMD5NotFound TCPMD5Unexpected TCPMD5Failure TCPSackShifted TCPSackMerged \
TCPSackShiftFallback TCPBacklogDrop PFMemallocDrop TCPMinTTLDrop TCPDeferAcceptDrop \
IPReversePathFilter TCPTimeWaitOverflow TCPReqQFullDoCookies TCPReqQFullDrop \
TCPRetransFail TCPRcvCoalesce TCPOFOQueue TCPOFODrop TCPOFOMerge TCPChallengeACK \
TCPSYNChallenge TCPFastOpenActive TCPFastOpenActiveFail TCPFastOpenPassive \
TCPFastOpenPassiveFail TCPFastOpenListenOverflow TCPFastOpenCookieReqd \
TCPFastOpenBlackhole TCPSpuriousRtxHostQueues BusyPollRxPackets TCPAutoCorking \
TCPFromZeroWindowAdv TCPToZeroWindowAdv TCPWantZeroWindowAdv TCPSynRetrans TCPOrigDataSent \
TCPHystartTrainDetect TCPHystartTrainCwnd TCPHystartDelayDetect TCPHystartDelayCwnd \
TCPACKSkippedSynRecv TCPACKSkippedPAWS TCPACKSkippedSeq TCPACKSkippedFinWait2 \
TCPACKSkippedTimeWait TCPACKSkippedChallenge TCPWinProbe TCPKeepAlive TCPMTUPFail \
TCPMTUPSuccess TCPDelivered TCPDeliveredCE TCPAckCompressed TCPZeroWindowDrop TCPRcvQDrop \
TCPWqueueTooBig TCPFastOpenPassiveAltKey TcpTimeoutRehash TcpDuplicateDataRehash \
TCPDSACKRecvSegs TCPDSACKIgnoredDubious TCPMigrateReqSuccess TCPMigrateReqFailure";

/// The IpExt fields (`snmp4_ipextstats_list`).
const NETSTAT_IP: &str = "InNoRoutes InTruncatedPkts InMcastPkts OutMcastPkts InBcastPkts \
OutBcastPkts InOctets OutOctets InMcastOctets OutMcastOctets InBcastOctets OutBcastOctets \
InCsumErrors InNoECTPkts InECT1Pkts InECT0Pkts InCEPkts ReasmOverlaps";

/// The MPTcpExt fields (`mptcp_snmp_list`).
const NETSTAT_MPTCP: &str = "MPCapableSYNRX MPCapableSYNTX MPCapableSYNACKRX MPCapableACKRX \
MPCapableFallbackACK MPCapableFallbackSYNACK MPFallbackTokenInit MPTCPRetrans \
MPJoinNoTokenFound MPJoinSynRx MPJoinSynAckRx MPJoinSynAckHMacFailure MPJoinAckRx \
MPJoinAckHMacFailure DSSNotMatching InfiniteMapTx InfiniteMapRx DSSNoMatchTCP DataCsumErr \
OFOQueueTail OFOQueue OFOMerge NoDSSInWindow DuplicateData AddAddr EchoAdd PortAdd \
AddAddrDrop MPJoinPortSynRx MPJoinPortSynAckRx MPJoinPortAckRx MismatchPortSynRx \
MismatchPortAckRx RmAddr RmAddrDrop RmSubflow MPPrioTx MPPrioRx MPFailTx MPFailRx \
MPFastcloseTx MPFastcloseRx MPRstTx MPRstRx RcvPruned SubflowStale SubflowRecover \
SndWndShared RcvWndShared RcvWndConflictUpdate RcvWndConflict";

/// `/proc/net/netstat` (`netstat_seq_show`): the kernel layer sees none of the TCP and MPTCP
/// extended counters, which are 0; IpExt's byte counts in and out are IPv4 payload (Linux's
/// include the IP header), and every received packet is taken as having no ECN mark.
pub(crate) fn proc_netstat(p: &crate::traffic::Protocols) -> String {
    let group = |name: &str, fields: &str, values: &dyn Fn(&str) -> u64| {
        let values: Vec<String> = fields.split(' ').map(|f| values(f).to_string()).collect();
        format!("{name}: {fields}\n{name}: {}\n", values.join(" "))
    };
    group("TcpExt", NETSTAT_TCP, &|_| 0)
        + &group("IpExt", NETSTAT_IP, &|f| match f {
            "InOctets" => p.ip_in_octets,
            "OutOctets" => p.ip_out_octets,
            "InNoECTPkts" => p.ip_in_packets,
            _ => 0,
        })
        + &group("MPTcpExt", NETSTAT_MPTCP, &|_| 0)
}

/// `/proc/net/arp`: the neighbour table is always empty (as with `RTM_GETNEIGH`), only the header.
pub(crate) const PROC_ARP: &str =
    "IP address       HW type     Flags       HW address            Mask     Device\n";

#[cfg(test)]
mod tests {
    use super::*;

    /// What a change request notifies: links whose statistics alone changed are left out, an
    /// address changed in place is only new, and a full queue drops and flags the overrun.
    #[test]
    fn snapshot_changes_are_notified_as_on_linux() {
        let link = |index: i32, mtu: u32, rx: u64| {
            let mut p = vec![0u8; 16];
            p[4..8].copy_from_slice(&index.to_le_bytes());
            attr(&mut p, IFLA_MTU, &mtu.to_le_bytes());
            attr(&mut p, IFLA_STATS64, &rx.to_le_bytes());
            p
        };
        let addr = |a: u8, flags: u8| {
            let mut p = vec![AF_INET as u8, 24, flags, 0, 2, 0, 0, 0];
            attr(&mut p, IFA_ADDRESS, &[10, 0, 0, a]);
            p
        };
        let route = |dst: u8, proto: u8| {
            let mut p = vec![AF_INET6 as u8, 64, 0, 0, 254, proto, 0, 1, 0, 0, 0, 0];
            let mut to = [0u8; 16];
            to[..2].copy_from_slice(&[0xfd, dst]);
            attr(&mut p, 1, &to); // RTA_DST
            p
        };
        let before = Snapshot {
            links: vec![link(1, 1500, 10), link(2, 1500, 0)],
            addrs: vec![addr(1, 0), addr(2, 0)],
            routes: vec![route(1, 2), route(2, 2)],
        };
        let after = Snapshot {
            links: vec![link(1, 1500, 99), link(3, 1400, 0)],
            addrs: vec![addr(1, 0x80), addr(3, 0)],
            routes: vec![route(1, 4)],
        };
        let got: Vec<_> = after
            .changes(&before)
            .into_iter()
            .map(|(g, m)| (g, u16::from_le_bytes([m[4], m[5]]), m[16..].to_vec()))
            .collect();
        assert_eq!(
            got,
            [
                (RTNLGRP_LINK, RTM_NEWLINK, link(3, 1400, 0)),
                (RTNLGRP_IPV4_IFADDR, RTM_DELADDR, addr(2, 0)),
                (RTNLGRP_IPV4_IFADDR, RTM_NEWADDR, addr(1, 0x80)),
                (RTNLGRP_IPV4_IFADDR, RTM_NEWADDR, addr(3, 0)),
                (RTNLGRP_IPV6_ROUTE, RTM_DELROUTE, route(2, 2)),
                (RTNLGRP_IPV6_ROUTE, RTM_NEWROUTE, route(1, 4)),
                (RTNLGRP_LINK, RTM_DELLINK, link(2, 1500, 0)),
            ]
        );

        let mut s = NetlinkSocket::new(NETLINK_ROUTE, SOCK_RAW);
        assert!(!s.subscribed() && !s.notify(RTNLGRP_LINK, b"x"));
        s.groups = Some(1 << (RTNLGRP_IPV4_IFADDR - 1));
        assert!(s.subscribed() && !s.notify(RTNLGRP_LINK, b"x"));
        for _ in 0..=NOTE_QUEUE {
            assert!(s.notify(RTNLGRP_IPV4_IFADDR, b"y"));
        }
        assert_eq!(s.queue.len(), NOTE_QUEUE);
        assert_eq!(s.queue[0], (b"y".to_vec(), RTNLGRP_IPV4_IFADDR));
        assert!(s.overrun.get());
    }

    fn lo() -> Interface {
        Interface {
            name: "lo".into(),
            index: 1,
            flags: 0x49,
            mtu: 65536,
            mac: None,
            addrs: vec![
                (AF_INET, vec![127, 0, 0, 1], 8),
                (AF_INET6, vec![0; 16], 128),
            ],
        }
    }

    /// The attributes in a message: type → payload.
    fn attributes(mut b: &[u8]) -> std::collections::BTreeMap<u16, Vec<u8>> {
        let mut out = std::collections::BTreeMap::new();
        while b.len() >= 4 {
            let len = u16::from_le_bytes([b[0], b[1]]) as usize;
            let ty = u16::from_le_bytes([b[2], b[3]]);
            out.insert(ty, b[4..len].to_vec());
            b = &b[len.next_multiple_of(4).min(b.len())..];
        }
        out
    }

    /// A fixed host: only lo, `stats` is its traffic counters, and the routes the host gives are
    /// `routes`.
    struct Fake {
        stats: IfStats,
        routes: Option<Vec<Route>>,
    }

    impl Host for Fake {
        fn interfaces(&self) -> Vec<Interface> {
            vec![lo()]
        }
        fn stats(&self, name: &str) -> IfStats {
            if name == "lo" {
                self.stats
            } else {
                IfStats::default()
            }
        }
        fn routes(&self) -> Option<Vec<Route>> {
            self.routes.clone()
        }
        fn tx_queue_len(&self, _: &str) -> Option<u32> {
            None
        }
        fn uid(&self) -> u32 {
            0
        }
    }

    const NOTHING: Fake = Fake {
        stats: IfStats {
            rx_bytes: 0,
            rx_packets: 0,
            tx_bytes: 0,
            tx_packets: 0,
        },
        routes: Some(Vec::new()),
    };

    /// Parses an answer as musl's `__rtnetlink_enumerate` does: returns (message type, message
    /// payload) up to DONE.
    fn parse(datagram: &[u8]) -> Vec<(u16, Vec<u8>)> {
        let mut out = Vec::new();
        let mut off = 0;
        while off + 16 <= datagram.len() {
            let len = u32::from_le_bytes(datagram[off..off + 4].try_into().unwrap()) as usize;
            let ty = u16::from_le_bytes([datagram[off + 4], datagram[off + 5]]);
            out.push((ty, datagram[off + 16..off + len].to_vec()));
            off += len.next_multiple_of(4);
        }
        out
    }

    #[test]
    fn dumps_links_and_addresses() {
        let mut s = NetlinkSocket::new(NETLINK_ROUTE, SOCK_RAW);
        s.pid = 42;
        let mut req = message(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 7, 0, &[0, 0, 0, 0]);
        req.extend(message(
            RTM_GETADDR,
            NLM_F_REQUEST | NLM_F_DUMP,
            8,
            0,
            &[AF_INET as u8, 0, 0, 0],
        ));
        let st = IfStats {
            rx_bytes: 1000,
            rx_packets: 3,
            tx_bytes: 0x1_0000_0002,
            tx_packets: 4,
        };
        s.handle(
            &req,
            &Fake {
                stats: st,
                routes: None,
            },
        );
        // Interfaces, NLMSG_DONE, IPv4 addresses, NLMSG_DONE are one datagram each
        let datagrams: Vec<_> = s.queue.drain(..).map(|(d, _)| parse(&d)).collect();
        let types: Vec<Vec<u16>> = datagrams
            .iter()
            .map(|d| d.iter().map(|m| m.0).collect())
            .collect();
        assert_eq!(
            types,
            [
                vec![RTM_NEWLINK],
                vec![NLMSG_DONE],
                vec![RTM_NEWADDR],
                vec![NLMSG_DONE]
            ]
        );
        let links = &datagrams[0];
        assert!(links[0].1.windows(3).any(|w| w == b"lo\0"));
        assert!(links[0].1.windows(3).any(|w| w == b"lo\0"));
        // IFLA_STATS is truncated to 32 bits, IFLA_STATS64 is complete
        let attrs = attributes(&links[0].1[16..]);
        let u32s = |n: usize| -> Vec<u32> {
            attrs[&IFLA_STATS][..n * 4]
                .chunks(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .collect()
        };
        assert_eq!(attrs[&IFLA_STATS].len(), 96);
        assert_eq!(u32s(5), [3, 4, 1000, 2, 0]);
        assert_eq!(attrs[&IFLA_STATS64].len(), 200);
        assert_eq!(attrs[&IFLA_STATS64][24..32], 0x1_0000_0002u64.to_le_bytes());
        assert_eq!(attrs[&IFLA_OPERSTATE], [IF_OPER_UNKNOWN]);
        assert_eq!(attrs[&IFLA_QDISC], b"noqueue\0");
        assert_eq!(attrs[&IFLA_TXQLEN], 1000u32.to_le_bytes());
        // Only IPv4 addresses: the loopback interface has no broadcast address, and the address
        // is permanent
        let addrs = &datagrams[2];
        assert_eq!(
            addrs[0].1[..4],
            [AF_INET as u8, 8, IFA_F_PERMANENT, RT_SCOPE_HOST]
        );
        let a = attributes(&addrs[0].1[8..]);
        assert_eq!(a[&IFA_LOCAL], [127, 0, 0, 1]);
        assert!(!a.contains_key(&IFA_BROADCAST));
        assert_eq!(a[&IFA_FLAGS], u32::from(IFA_F_PERMANENT).to_le_bytes());
        assert_eq!(a[&IFA_CACHEINFO][..8], [0xff; 8]);
        // Single queries: by index, by name; ENODEV when not found, EINVAL without criteria
        let get = |index: i32, name: Option<&[u8]>| {
            let mut p = vec![0u8, 0, 0, 0];
            p.extend_from_slice(&index.to_le_bytes());
            p.extend_from_slice(&[0; 8]);
            if let Some(n) = name {
                attr(&mut p, IFLA_IFNAME, n);
            }
            message(RTM_GETLINK, NLM_F_REQUEST, 9, 0, &p)
        };
        let mut ask = |req: Vec<u8>| -> (u16, Vec<u8>) {
            s.handle(&req, &NOTHING);
            let d = parse(&s.queue.pop_front().unwrap().0);
            assert!(s.queue.is_empty());
            d[0].clone()
        };
        assert_eq!(ask(get(1, None)).0, RTM_NEWLINK);
        assert_eq!(ask(get(0, Some(b"lo\0"))).0, RTM_NEWLINK);
        let errno = |m: (u16, Vec<u8>)| i32::from_le_bytes(m.1[..4].try_into().unwrap());
        assert_eq!(errno(ask(get(0, Some(b"eth9\0")))), -19);
        assert_eq!(errno(ask(get(7, None))), -19);
        assert_eq!(errno(ask(get(0, None))), -22);
    }

    /// A host with many interfaces, each with an IPv4 and an IPv6 address.
    struct Many(u32);

    impl Host for Many {
        fn interfaces(&self) -> Vec<Interface> {
            (1..=self.0)
                .map(|n| Interface {
                    name: format!("eth{n}"),
                    index: n,
                    flags: 0x43,
                    mtu: 1500,
                    mac: Some([2, 0, 0, 0, 0, n as u8]),
                    addrs: vec![
                        (AF_INET, vec![10, 0, n as u8, 1], 24),
                        (AF_INET6, [&[0xfd, 0][..], &[n as u8; 14]].concat(), 64),
                    ],
                })
                .collect()
        }
        fn stats(&self, _: &str) -> IfStats {
            IfStats::default()
        }
        fn routes(&self) -> Option<Vec<Route>> {
            None
        }
        fn tx_queue_len(&self, _: &str) -> Option<u32> {
            None
        }
        fn uid(&self) -> u32 {
            0
        }
    }

    /// Large dumps come in datagrams that fit a page-sized buffer (Go's `NetlinkRIB` reads one
    /// page and fails on a truncated datagram), with every message kept in order: links in one
    /// run, addresses per family, then NLMSG_DONE alone.
    #[test]
    fn splits_large_dumps_into_page_sized_datagrams() {
        let mut s = NetlinkSocket::new(NETLINK_ROUTE, SOCK_RAW);
        let host = Many(64);
        let mut dump = |ty: u16| -> Vec<Vec<(u16, Vec<u8>)>> {
            s.handle(
                &message(ty, NLM_F_REQUEST | NLM_F_DUMP, 1, 0, &[0; 16]),
                &host,
            );
            s.queue
                .drain(..)
                .map(|(d, _)| {
                    assert!(d.len() <= 4096, "a {}-byte datagram", d.len());
                    parse(&d)
                })
                .collect()
        };
        let links = dump(RTM_GETLINK);
        assert!(links.len() > 2);
        assert_eq!(links.last().unwrap()[0].0, NLMSG_DONE);
        let names: Vec<Vec<u8>> = links[..links.len() - 1]
            .iter()
            .flatten()
            .map(|(ty, p)| {
                assert_eq!(*ty, RTM_NEWLINK);
                attributes(&p[16..])[&IFLA_IFNAME].clone()
            })
            .collect();
        let want: Vec<Vec<u8>> = (1..=64).map(|n| format!("eth{n}\0").into()).collect();
        assert_eq!(names, want);
        let addrs = dump(RTM_GETADDR);
        assert_eq!(addrs.last().unwrap()[0].0, NLMSG_DONE);
        // A datagram never mixes the families, and the IPv6 ones are split too
        let families: Vec<Vec<u8>> = addrs[..addrs.len() - 1]
            .iter()
            .map(|d| d.iter().map(|(_, p)| p[0]).collect())
            .collect();
        assert!(families.iter().all(|f| f.iter().all(|&x| x == f[0])));
        let v6 = families.iter().filter(|f| f[0] == AF_INET6 as u8).count();
        assert!(v6 > 1);
        assert_eq!(families.concat().len(), 128);
    }

    /// Route and rule dumps: one datagram per address family, and NLMSG_DONE another one; the
    /// neighbour table is empty; errors for unsupported requests attach the whole request, and
    /// acks attach only the message header.
    #[test]
    fn dumps_routes_rules_and_neighbours() {
        let mut s = NetlinkSocket::new(NETLINK_ROUTE, SOCK_RAW);
        s.pid = 42;
        let mut ask = |req: &[u8]| -> Vec<Vec<(u16, Vec<u8>)>> {
            s.handle(req, &NOTHING);
            s.queue.drain(..).map(|(d, _)| parse(&d)).collect()
        };
        let dump = |ty, family: i32| {
            message(
                ty,
                NLM_F_REQUEST | NLM_F_DUMP,
                7,
                0,
                &[family as u8, 0, 0, 0],
            )
        };
        // On lo: three IPv4 local-table entries, and one for IPv6's ::1
        let r = ask(&dump(RTM_GETROUTE, 0));
        let counts: Vec<(usize, u16)> = r.iter().map(|d| (d.len(), d[0].0)).collect();
        assert_eq!(
            counts,
            [(3, RTM_NEWROUTE), (1, RTM_NEWROUTE), (1, NLMSG_DONE)]
        );
        let r = ask(&dump(RTM_GETROUTE, AF_INET6));
        assert_eq!(r.len(), 2);
        let r = ask(&dump(RTM_GETRULE, AF_INET));
        assert_eq!((r.len(), r[0].len(), r[1][0].0), (2, 3, NLMSG_DONE));
        let r = ask(&dump(RTM_GETNEIGH, 0));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0][0].0, NLMSG_DONE);
        // An unsupported request
        let req = message(
            RTM_NEWROUTE,
            NLM_F_REQUEST,
            9,
            0,
            &[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        );
        let r = ask(&req);
        assert_eq!(r[0][0].0, NLMSG_ERROR);
        assert_eq!(r[0][0].1[..4], (-95i32).to_le_bytes());
        assert_eq!(&r[0][0].1[4..], &req[..]);
        // NLMSG_NOOP asking for an ack
        let r = ask(&message(1, NLM_F_REQUEST | NLM_F_ACK, 9, 0, &[]));
        assert_eq!(r[0][0].1.len(), 4 + 16);
        assert_eq!(r[0][0].1[..4], 0i32.to_le_bytes());
    }

    #[test]
    fn netns_ids_are_never_assigned() {
        let mut s = NetlinkSocket::new(NETLINK_ROUTE, SOCK_RAW);
        s.pid = 42;
        let mut ask = |attrs: &[(u16, &[u8])]| -> Vec<(u16, Vec<u8>)> {
            let mut p = vec![0u8; 4];
            for (t, d) in attrs {
                attr(&mut p, *t, d);
            }
            s.handle(&message(RTM_GETNSID, NLM_F_REQUEST, 5, 0, &p), &NOTHING);
            s.queue.drain(..).flat_map(|(d, _)| parse(&d)).collect()
        };
        // The other namespace is found: the answer is -1 (no number assigned), the same as what
        // `ip netns list` sees on Linux
        let r = ask(&[(NETNSA_FD, &3u32.to_le_bytes())]);
        let mut want = vec![0u8; 4];
        attr(&mut want, NETNSA_NSID, &(-1i32).to_le_bytes());
        assert_eq!(r, [(RTM_NEWNSID, want)]);
        assert_eq!(ask(&[(NETNSA_PID, &1u32.to_le_bytes())]).len(), 1);
        // Lookups by number never find anything; giving nothing is EINVAL; so is asking for the
        // number translated into another namespace
        let errno = |r: Vec<(u16, Vec<u8>)>| i32::from_le_bytes(r[0].1[..4].try_into().unwrap());
        assert_eq!(errno(ask(&[(NETNSA_NSID, &0i32.to_le_bytes())])), -ENOENT);
        assert_eq!(errno(ask(&[])), -EINVAL);
        let target = [
            (NETNSA_FD, &3u32.to_le_bytes()[..]),
            (NETNSA_TARGET_NSID, &0i32.to_le_bytes()[..]),
        ];
        assert_eq!(errno(ask(&target)), -EINVAL);
    }

    /// A NETLINK_GENERIC request: attributes after the genlmsghdr.
    fn genl(ty: u16, flags: u16, cmd: u8, attrs: &[(u16, &[u8])]) -> Vec<u8> {
        let mut p = vec![cmd, 1, 0, 0];
        for (t, d) in attrs {
            attr(&mut p, *t, d);
        }
        message(ty, NLM_F_REQUEST | flags, 9, 0, &p)
    }

    #[test]
    fn generic_netlink_has_only_nlctrl() {
        let mut s = NetlinkSocket::new(NETLINK_GENERIC, SOCK_RAW);
        s.pid = 42;
        // The answer's datagrams, each parsed into messages
        let mut ask = |req: &[u8]| -> Vec<Vec<(u16, Vec<u8>)>> {
            s.handle(req, &NOTHING);
            s.queue.drain(..).map(|(d, _)| parse(&d)).collect()
        };
        // Query nlctrl by name, asking for an ack: the answer and the ack are two datagrams, and
        // the ack attaches only the request's message header
        let name: &[u8] = b"nlctrl\0";
        let r = ask(&genl(
            GENL_ID_CTRL,
            NLM_F_ACK,
            CTRL_CMD_GETFAMILY,
            &[(CTRL_ATTR_FAMILY_NAME, name)],
        ));
        assert_eq!(r.len(), 2);
        let (ty, reply) = &r[0][0];
        assert_eq!(
            (*ty, &reply[..2]),
            (GENL_ID_CTRL, &[CTRL_CMD_NEWFAMILY, 2][..])
        );
        let a = attributes(&reply[4..]);
        assert_eq!(a[&CTRL_ATTR_FAMILY_ID], GENL_ID_CTRL.to_le_bytes());
        assert_eq!(a[&CTRL_ATTR_FAMILY_NAME], name);
        let (ty, ack) = &r[1][0];
        assert_eq!(*ty, NLMSG_ERROR);
        assert_eq!(ack.len(), 4 + 16);
        assert_eq!(ack[..4], 0i32.to_le_bytes());
        // The same by number
        let id = GENL_ID_CTRL.to_le_bytes();
        let r = ask(&genl(
            GENL_ID_CTRL,
            0,
            CTRL_CMD_GETFAMILY,
            &[(CTRL_ATTR_FAMILY_ID, &id)],
        ));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0][0].0, GENL_ID_CTRL);
        // On error the whole request is attached
        let unknown: &[u8] = b"nl80211\0";
        let cases: [(Vec<u8>, i32); 5] = [
            (
                genl(
                    GENL_ID_CTRL,
                    0,
                    CTRL_CMD_GETFAMILY,
                    &[(CTRL_ATTR_FAMILY_NAME, unknown)],
                ),
                -2,
            ),
            (genl(0x7ff0, 0, 1, &[]), -2),
            (genl(GENL_ID_CTRL, 0, CTRL_CMD_GETFAMILY, &[]), -22),
            (genl(GENL_ID_CTRL, 0, 99, &[]), -95),
            (
                genl(
                    GENL_ID_CTRL,
                    NLM_F_DUMP,
                    CTRL_CMD_GETPOLICY,
                    &[(CTRL_ATTR_FAMILY_NAME, unknown)],
                ),
                -2,
            ),
        ];
        for (req, err) in cases {
            let r = ask(&req);
            assert_eq!(r.len(), 1);
            let (ty, e) = &r[0][0];
            assert_eq!(*ty, NLMSG_ERROR);
            assert_eq!(e[..4], err.to_le_bytes());
            assert_eq!(&e[4..], &req[..]);
        }
        // The dump has only nlctrl, NLMSG_DONE is in the same datagram, and no separate ack
        // follows
        let r = ask(&genl(
            GENL_ID_CTRL,
            NLM_F_DUMP | NLM_F_ACK,
            CTRL_CMD_GETFAMILY,
            &[],
        ));
        assert_eq!(r.len(), 1);
        let types: Vec<u16> = r[0].iter().map(|m| m.0).collect();
        assert_eq!(types, [GENL_ID_CTRL, NLMSG_DONE]);
    }

    #[test]
    fn sock_diag_refuses_every_query() {
        let mut s = NetlinkSocket::new(NETLINK_SOCK_DIAG, SOCK_RAW);
        s.pid = 42;
        let mut ask = |req: &[u8]| -> Vec<Vec<(u16, Vec<u8>)>> {
            s.handle(req, &NOTHING);
            s.queue.drain(..).map(|(d, _)| parse(&d)).collect()
        };
        let dump = NLM_F_REQUEST | NLM_F_DUMP;
        // ss's inet_diag_req_v2 for TCP over IPv4, a unix_diag_req, the old TCPDIAG_GETSOCK, a
        // short family request, a family past AF_MAX and an unknown type: each one error with the
        // whole request attached
        let mut inet = vec![AF_INET as u8, IPPROTO_TCP as u8, 0, 0];
        inet.extend_from_slice(&u32::MAX.to_le_bytes());
        inet.resize(56, 0);
        let cases = [
            (message(SOCK_DIAG_BY_FAMILY, dump, 1, 0, &inet), -ENOENT),
            (
                message(SOCK_DIAG_BY_FAMILY, dump, 2, 0, &[AF_UNIX as u8; 24]),
                -ENOENT,
            ),
            (message(TCPDIAG_GETSOCK, dump, 3, 0, &[0; 72]), -EOPNOTSUPP),
            (message(SOCK_DIAG_BY_FAMILY, dump, 4, 0, &[2]), -EINVAL),
            (
                message(SOCK_DESTROY, NLM_F_REQUEST, 5, 0, &[AF_MAX, 0]),
                -EINVAL,
            ),
            (message(30, NLM_F_REQUEST, 6, 0, &[]), -EINVAL),
        ];
        for (req, err) in cases {
            let r = ask(&req);
            assert_eq!(r.len(), 1);
            let (ty, e) = &r[0][0];
            assert_eq!(*ty, NLMSG_ERROR);
            assert_eq!(e[..4], err.to_le_bytes());
            // The request as long as its header says, without the padding after it
            let len = u32::from_le_bytes(req[..4].try_into().unwrap()) as usize;
            assert_eq!(&e[4..], &req[..len]);
        }
        // Control messages and non-requests are acked only when asked
        assert!(ask(&message(NLMSG_DONE, NLM_F_REQUEST, 7, 0, &[])).is_empty());
        let r = ask(&message(NLMSG_DONE, NLM_F_ACK, 8, 0, &[]));
        assert_eq!(r[0][0].1[..4], 0i32.to_le_bytes());
    }

    #[test]
    fn recognizes_the_dns_stub() {
        assert!(is_stub(&stub_addr(AF_INET)));
        assert!(is_stub(&stub_addr(AF_INET6)));
        let other = tuxport_hal::net::sockaddr("127.0.0.53:54".parse().unwrap());
        assert!(!is_stub(&other));
        let fail = servfail(&[0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(fail[..4], [0x12, 0x34, 0x81, 0x82]);
    }

    /// Changing host interfaces in the boot namespace: requests that change nothing succeed, ones
    /// that change something are EOPNOTSUPP, and the order of error numbers is as in Linux 6.1's
    /// rtnl_setlink, __rtnl_newlink.
    #[test]
    fn host_links_accept_requests_that_change_nothing() {
        const SET: u16 = 19;
        const UP: u32 = 0x1;
        // The message: header, ifinfomsg (index, flags, flags to change), then attributes
        let msg = |ty: u16, flags: u16, index: i32, ifi: (u32, u32), attrs: &[(u16, &[u8])]| {
            let mut m = vec![0u8; 32];
            m[4..6].copy_from_slice(&ty.to_le_bytes());
            m[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_ACK | flags).to_le_bytes());
            m[20..24].copy_from_slice(&index.to_le_bytes());
            m[24..28].copy_from_slice(&ifi.0.to_le_bytes());
            m[28..32].copy_from_slice(&ifi.1.to_le_bytes());
            for (t, d) in attrs {
                attr(&mut m, *t, d);
            }
            let len = m.len() as u32;
            m[..4].copy_from_slice(&len.to_le_bytes());
            m
        };
        let ifaces = [lo()];
        let ask = |m: Vec<u8>| configure_host_link(&ifaces, true, &m);
        let lo_name: &[u8] = b"lo\0";
        let mtu = 65536u32.to_le_bytes();
        // lo, already up: by index, by name, both messages, flags all 0, the whole ifi_flags
        // (ifi_change 0)
        assert_eq!(ask(msg(RTM_NEWLINK, 0, 1, (UP, UP), &[])), Some(0));
        assert_eq!(ask(msg(SET, 0, 1, (UP, UP), &[])), Some(0));
        assert_eq!(
            ask(msg(RTM_NEWLINK, 0, 0, (UP, UP), &[(IFLA_IFNAME, lo_name)])),
            Some(0)
        );
        assert_eq!(ask(msg(RTM_NEWLINK, 0, 1, (0, 0), &[])), Some(0));
        assert_eq!(ask(msg(RTM_NEWLINK, 0, 1, (UP | 0x48, 0), &[])), Some(0));
        assert_eq!(
            ask(msg(RTM_NEWLINK, 0x400, 1, (UP, UP), &[(IFLA_MTU, &mtu)])),
            Some(0)
        );
        // Ones that change something: down, promiscuous mode, another MTU, rename, other
        // attributes
        assert_eq!(ask(msg(RTM_NEWLINK, 0, 1, (0, UP), &[])), Some(EOPNOTSUPP));
        assert_eq!(
            ask(msg(RTM_NEWLINK, 0, 1, (0x100, 0x100), &[])),
            Some(EOPNOTSUPP)
        );
        assert_eq!(
            ask(msg(
                RTM_NEWLINK,
                0,
                1,
                (0, 0),
                &[(IFLA_MTU, &1500u32.to_le_bytes())]
            )),
            Some(EOPNOTSUPP)
        );
        assert_eq!(
            ask(msg(SET, 0, 1, (0, 0), &[(IFLA_IFNAME, b"lo2\0")])),
            Some(EOPNOTSUPP)
        );
        assert_eq!(
            ask(msg(SET, 0, 1, (0, 0), &[(IFLA_TXQLEN, &[0; 4])])),
            Some(EOPNOTSUPP)
        );
        // Not found, create, EXCL, REPLACE, neither index nor name given
        assert_eq!(ask(msg(RTM_NEWLINK, 0, 9, (UP, UP), &[])), Some(19));
        assert_eq!(
            ask(msg(SET, 0, 0, (UP, UP), &[(IFLA_IFNAME, b"eth9\0")])),
            Some(19)
        );
        assert_eq!(
            ask(msg(
                RTM_NEWLINK,
                0x400,
                0,
                (0, 0),
                &[(IFLA_IFNAME, b"eth9\0")]
            )),
            Some(EOPNOTSUPP)
        );
        assert_eq!(ask(msg(RTM_NEWLINK, 0x200, 1, (UP, UP), &[])), Some(EEXIST));
        assert_eq!(
            ask(msg(RTM_NEWLINK, 0x100, 1, (UP, UP), &[])),
            Some(EOPNOTSUPP)
        );
        assert_eq!(ask(msg(RTM_NEWLINK, 0, 0, (UP, UP), &[])), Some(19));
        assert_eq!(ask(msg(SET, 0, 0, (UP, UP), &[])), Some(EINVAL));
        assert_eq!(ask(msg(RTM_NEWLINK, 0, -1, (UP, UP), &[])), Some(EINVAL));
        // Without CAP_NET_ADMIN it is EPERM first; other messages are not its business
        assert_eq!(
            configure_host_link(&ifaces, false, &msg(RTM_NEWLINK, 0, 9, (0, 0), &[])),
            Some(EPERM)
        );
        assert_eq!(ask(msg(RTM_GETLINK, 0, 1, (0, 0), &[])), None);
    }
}

// ---- Generic socket options ----

/// `SO_SNDBUF` and `SO_RCVBUF` before any change and at most without the `FORCE` forms: Linux's
/// `net.core.wmem_default`, `rmem_default`, `wmem_max` and `rmem_max` (which can be written, but
/// keep these values here).
const BUF_DEFAULT: i32 = 212992;
/// The smallest buffer sizes Linux 6.1 keeps on arm64 (`SOCK_MIN_SNDBUF`, `SOCK_MIN_RCVBUF`).
const MIN_SNDBUF: i32 = 4608;
const MIN_RCVBUF: i32 = 2304;

/// The `SOL_SOCKET` options every Linux socket keeps (`sk_setsockopt`, `sk_getsockopt`) that
/// netlink and AF_UNIX sockets only record: they are set and read back as Linux does, without
/// effect (buffer sizes do not limit what is queued in process; lingering, keepalives, routing
/// and priorities mean nothing there).
#[derive(Clone, Debug)]
pub(crate) struct SockOpts {
    debug: bool,
    reuseaddr: bool,
    reuseport: bool,
    dontroute: bool,
    broadcast: bool,
    keepalive: bool,
    oobinline: bool,
    no_check: bool,
    sndbuf: i32,
    rcvbuf: i32,
    priority: i32,
    rcvlowat: i32,
    /// `SO_LINGER`: whether it is on, and the seconds (kept when it is turned off, as Linux keeps
    /// `sk_lingertime`).
    linger: (bool, i32),
}

impl Default for SockOpts {
    fn default() -> SockOpts {
        SockOpts {
            debug: false,
            reuseaddr: false,
            reuseport: false,
            dontroute: false,
            broadcast: false,
            keepalive: false,
            oobinline: false,
            no_check: false,
            sndbuf: BUF_DEFAULT,
            rcvbuf: BUF_DEFAULT,
            priority: 0,
            rcvlowat: 1,
            linger: (false, 0),
        }
    }
}

impl SockOpts {
    const DEBUG: i32 = 1;
    const REUSEADDR: i32 = 2;
    const TYPE: i32 = 3;
    const ERROR: i32 = 4;
    const DONTROUTE: i32 = 5;
    const BROADCAST: i32 = 6;
    const SNDBUF: i32 = 7;
    const RCVBUF: i32 = 8;
    const KEEPALIVE: i32 = 9;
    const OOBINLINE: i32 = 10;
    const NO_CHECK: i32 = 11;
    const PRIORITY: i32 = 12;
    const LINGER: i32 = 13;
    const BSDCOMPAT: i32 = 14;
    const REUSEPORT: i32 = 15;
    const RCVLOWAT: i32 = 18;
    const SNDLOWAT: i32 = 19;
    const SNDBUFFORCE: i32 = 32;
    const RCVBUFFORCE: i32 = 33;
    const PROTOCOL: i32 = 38;
    const DOMAIN: i32 = 39;

    /// setsockopt at `SOL_SOCKET`; None when `name` is not one of these options. `admin` is
    /// `CAP_NET_ADMIN` in the initial user namespace (`SO_DEBUG`, the `FORCE` forms), `net_admin`
    /// `CAP_NET_RAW` or `CAP_NET_ADMIN` over the owner of the socket's network namespace
    /// (priorities above 6).
    pub fn set(
        &mut self,
        name: i32,
        value: &[u8],
        admin: bool,
        net_admin: bool,
    ) -> Option<Result<(), crate::errno::Errno>> {
        use crate::errno::Errno;
        let known = matches!(
            name,
            Self::DEBUG
                | Self::REUSEADDR
                | Self::TYPE
                | Self::ERROR
                | Self::DONTROUTE
                | Self::BROADCAST
                | Self::SNDBUF
                | Self::RCVBUF
                | Self::KEEPALIVE
                | Self::OOBINLINE
                | Self::NO_CHECK
                | Self::PRIORITY
                | Self::LINGER
                | Self::BSDCOMPAT
                | Self::REUSEPORT
                | Self::RCVLOWAT
                | Self::SNDBUFFORCE
                | Self::RCVBUFFORCE
                | Self::PROTOCOL
                | Self::DOMAIN
        );
        if !known {
            return None;
        }
        // Each of them takes an int
        let Some(b) = value.get(..4) else {
            return Some(Err(Errno::EINVAL));
        };
        let val = i32::from_le_bytes(b.try_into().unwrap());
        let on = val != 0;
        // sock_set_sndbuf, __sock_set_rcvbuf: at most half of INT_MAX, then doubled
        let sndbuf = |v: i32| v.min(i32::MAX / 2).saturating_mul(2).max(MIN_SNDBUF);
        let rcvbuf = |v: i32| v.min(i32::MAX / 2).saturating_mul(2).max(MIN_RCVBUF);
        // The plain forms are capped at the maximum, comparing as unsigned (a negative size is
        // the maximum)
        let capped = |v: i32| (v as u32).min(BUF_DEFAULT as u32) as i32;
        match name {
            Self::DEBUG if on && !admin => return Some(Err(Errno::EACCES)),
            Self::DEBUG => self.debug = on,
            Self::REUSEADDR => self.reuseaddr = on,
            Self::REUSEPORT => self.reuseport = on,
            Self::DONTROUTE => self.dontroute = on,
            Self::BROADCAST => self.broadcast = on,
            Self::KEEPALIVE => self.keepalive = on,
            Self::OOBINLINE => self.oobinline = on,
            Self::NO_CHECK => self.no_check = on,
            Self::SNDBUF => self.sndbuf = sndbuf(capped(val)),
            Self::RCVBUF => self.rcvbuf = rcvbuf(capped(val)),
            Self::SNDBUFFORCE | Self::RCVBUFFORCE if !admin => return Some(Err(Errno::EPERM)),
            Self::SNDBUFFORCE => self.sndbuf = sndbuf(val.max(0)),
            Self::RCVBUFFORCE => self.rcvbuf = rcvbuf(val.max(0)),
            Self::PRIORITY if !(0..=6).contains(&val) && !net_admin => {
                return Some(Err(Errno::EPERM));
            }
            Self::PRIORITY => self.priority = val,
            Self::LINGER => match value.get(4..8) {
                None => return Some(Err(Errno::EINVAL)),
                Some(_) if !on => self.linger.0 = false,
                Some(secs) => self.linger = (true, i32::from_le_bytes(secs.try_into().unwrap())),
            },
            Self::RCVLOWAT => self.rcvlowat = if val < 0 { i32::MAX } else { val.max(1) },
            Self::BSDCOMPAT => {}
            // Read-only
            _ => return Some(Err(Errno::ENOPROTOOPT)),
        }
        Some(Ok(()))
    }

    /// getsockopt at `SOL_SOCKET`; None when `name` is not one of these options.
    pub fn get(&self, name: i32) -> Option<Vec<u8>> {
        let int = |v: i32| Some(v.to_le_bytes().to_vec());
        match name {
            Self::DEBUG => int(self.debug as i32),
            Self::REUSEADDR => int(self.reuseaddr as i32),
            Self::REUSEPORT => int(self.reuseport as i32),
            Self::DONTROUTE => int(self.dontroute as i32),
            Self::BROADCAST => int(self.broadcast as i32),
            Self::KEEPALIVE => int(self.keepalive as i32),
            Self::OOBINLINE => int(self.oobinline as i32),
            Self::NO_CHECK => int(self.no_check as i32),
            Self::SNDBUF => int(self.sndbuf),
            Self::RCVBUF => int(self.rcvbuf),
            Self::PRIORITY => int(self.priority),
            Self::RCVLOWAT => int(self.rcvlowat),
            Self::SNDLOWAT => int(1),
            Self::BSDCOMPAT => int(0),
            Self::LINGER => {
                let mut b = (self.linger.0 as i32).to_le_bytes().to_vec();
                b.extend_from_slice(&self.linger.1.to_le_bytes());
                Some(b)
            }
            _ => None,
        }
    }
}
