//! The guest's raw ICMP sockets (the kernel/net map). Apps on Android have no `CAP_NET_RAW`, so a
//! guest raw ICMP socket is a ping socket on the host (which needs no privilege), and the
//! differences between the two are made up for here:
//!
//! - **Receiving every ICMP message that arrives**: a raw ICMP socket on Linux receives every ICMP
//!   message sent to the machine, and traceroute sends probes on one socket and receives the
//!   time-exceeded and unreachable messages on another raw socket; the host's ping socket receives
//!   only its own echo replies. So there is an aggregation point ([`Tap`]) per network namespace:
//!   the ping sockets behind raw sockets, and UDP sockets that set a TTL, connected or not
//!   (traceroute's probes, [`before_io`]), have `IP_RECVERR` enabled on the host. The messages
//!   they receive and the ICMP errors in their error queues are taken off by the aggregation point
//!   ([`dispatch`]); errors are reassembled into whole ICMP error messages (quoting the original IP
//!   header and the start of the message) and given to the queue of every raw socket of the same
//!   namespace and protocol family. A raw socket takes them off once itself when it receives, and
//!   for waiters in poll and epoll the timer thread watches the aggregation points
//!   ([`check_taps`]).
//! - **The echo id**: a ping socket replaces the id with its own number when sending an echo
//!   request, and the replies carry that number too; a raw socket sends it unchanged, and programs
//!   (busybox's ping uses the pid) recognize replies by their own id. The id the guest sent last
//!   is recorded, and received echo replies and the echo requests quoted in error messages get it
//!   back, with the checksum updated along with it.
//! - **The IPv4 IP header**: data received on a raw socket includes the IP header, so a 20-byte
//!   header is assembled here ([`present`]): TTL and destination address come from the host's
//!   `IP_TTL` and `IP_PKTINFO` control messages (always enabled on the host, and not passed to the
//!   guest when it has not enabled them itself); TOS, the IP id and the flags are all 0. IPv6 raw
//!   sockets carry no IP header anyway.
//! - **Options only raw sockets have**: `ICMP_FILTER` and `ICMPV6_FILTER` filter at dispatch;
//!   `IPV6_CHECKSUM` (2 by default for ICMPv6) is only recorded, since the host's ping socket
//!   always computes the checksum.
//!
//! The `IP_RECVERR` enabled on the host for the guest has a side effect: the host records received
//! ICMP errors on the socket, so the next send or receive fails because of them. These sockets
//! take the errors off before sending and receiving, and when a call fails because of one, take it
//! off too and try again.
//!
//! [`present`]: RawIcmp::present

use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use tuxport_hal::Watch;

use crate::cell::RefCell;
use crate::errno::Errno;
use crate::kernel::{Kernel, request_wake_object, wake};
use crate::net::{AF_INET, AF_INET6, IPPROTO_ICMP, IPPROTO_UDP, InetSocket, SOCK_DGRAM, Socket};
use crate::nsproxy::NetNs;

const SOL_SOCKET: i32 = 1;
const SOL_IP: i32 = 0;
const SOL_IPV6: i32 = 41;
const SOL_ICMPV6: i32 = 58;
const SOL_RAW: i32 = 255;
const IP_TTL: i32 = 2;
const IP_PKTINFO: i32 = 8;
const IP_RECVERR: i32 = 11;
const IP_RECVTTL: i32 = 12;
const IPV6_UNICAST_HOPS: i32 = 16;
const IPV6_RECVERR: i32 = 25;
const IPV6_HOPLIMIT: i32 = 52;
/// `ICMP_FILTER` (SOL_RAW), `ICMPV6_FILTER` (SOL_ICMPV6).
const ICMP_FILTER: i32 = 1;
const IPV6_CHECKSUM: i32 = 7;
const ICMP_ECHOREPLY: u8 = 0;
const ICMP_DEST_UNREACH: u8 = 3;
const ICMP_ECHO: u8 = 8;
const ICMP_PARAMETERPROB: u8 = 12;
const ICMP_FRAG_NEEDED: u8 = 4;
const ICMPV6_PKT_TOOBIG: u8 = 2;
const ICMPV6_PARAMPROB: u8 = 4;
const ICMPV6_ECHO_REQUEST: u8 = 128;
const ICMPV6_ECHO_REPLY: u8 = 129;
const NEXTHDR_ICMPV6: u8 = 58;
const SO_EE_ORIGIN_ICMP: u8 = 2;
const SO_EE_ORIGIN_ICMP6: u8 = 3;
const MSG_DONTWAIT: i32 = 0x40;
const MSG_ERRQUEUE: i32 = 0x2000;
const MSG_CTRUNC: u32 = 0x08;
/// The length of the host's `struct cmsghdr` (arm64 Linux: `size_t cmsg_len` plus two ints).
const CMSG_HDR: usize = 16;
/// The longest an ICMP message can be (the IP total length is 16 bits).
const PACKET_MAX: usize = 1 << 16;
/// The most bytes queued per raw socket (Linux's default `rmem_default`); what does not fit is
/// dropped.
const QUEUE_MAX: usize = 212_992;
/// Room for control messages the host may leave when sending and receiving.
const CONTROL_ROOM: usize = 512;

/// The guest raw ICMP socket's own state: options, the id of the last echo sent, and its
/// aggregation point.
pub(crate) struct RawIcmp {
    family: i32,
    /// The id of the last echo request the guest sent (network byte order).
    id: Option<[u8; 2]>,
    /// The filter; types whose bit is set are dropped (Linux stores it as a u32 array, and IPv4
    /// uses only the first).
    filter: [u8; 32],
    /// The guest itself enabled `IP_RECVTTL`, `IP_PKTINFO` (IPv4).
    recvttl: bool,
    pktinfo: bool,
    /// `IP_RECVERR`, `IPV6_RECVERR` as set by the guest itself (always enabled on the host): when
    /// set, ICMP errors also go into the socket's error queue and every one is reported
    /// ([`dispatch`]).
    recverr: bool,
    /// The value of `IPV6_CHECKSUM`; -1 means not computed.
    checksum: i32,
    /// Every received message goes through it and is queued there.
    pub tap: Rc<RefCell<Tap>>,
}

impl RawIcmp {
    pub fn new(family: i32, tap: Rc<RefCell<Tap>>) -> RawIcmp {
        RawIcmp {
            family,
            id: None,
            filter: [0; 32],
            recvttl: false,
            pktinfo: false,
            recverr: false,
            checksum: 2,
            tap,
        }
    }

    /// The options to enable on the host's ping socket for the guest.
    pub fn host_options(family: i32) -> &'static [(i32, i32)] {
        match family {
            AF_INET => &[
                (SOL_IP, IP_RECVTTL),
                (SOL_IP, IP_PKTINFO),
                (SOL_IP, IP_RECVERR),
            ],
            _ => &[(SOL_IPV6, IPV6_RECVERR)],
        }
    }

    /// The raw socket's own options. Returns None for those not handled here (left to the host).
    pub fn setsockopt(&mut self, level: i32, name: i32, v: &[u8]) -> Option<Result<(), i32>> {
        const EINVAL: i32 = 22;
        const ENOPROTOOPT: i32 = 92;
        let int = || match v.len() {
            0 => 0,
            1..4 => v[0] as i32,
            _ => i32::from_le_bytes(v[..4].try_into().unwrap()),
        };
        Some(match (self.family, level, name) {
            // Linux's raw_seticmpfilter, rawv6_seticmpfilter: copy at most the filter's length,
            // and a short value covers only the front part
            (AF_INET, SOL_RAW, ICMP_FILTER) | (AF_INET6, SOL_ICMPV6, ICMP_FILTER) => {
                let n = v.len().min(if self.family == AF_INET { 4 } else { 32 });
                self.filter[..n].copy_from_slice(&v[..n]);
                Ok(())
            }
            (AF_INET, SOL_RAW, _) => Err(ENOPROTOOPT),
            (AF_INET, SOL_IP, IP_RECVTTL) => {
                self.recvttl = int() != 0;
                Ok(())
            }
            (AF_INET, SOL_IP, IP_PKTINFO) => {
                self.pktinfo = int() != 0;
                Ok(())
            }
            (AF_INET, SOL_IP, IP_RECVERR) | (AF_INET6, SOL_IPV6, IPV6_RECVERR) => {
                self.recverr = int() != 0;
                Ok(())
            }
            // RFC 3542 forbids setting it for ICMPv6 at the IPPROTO_IPV6 level (Linux's
            // do_rawv6_setsockopt)
            (AF_INET6, SOL_IPV6, IPV6_CHECKSUM) => Err(EINVAL),
            (AF_INET6, SOL_RAW, IPV6_CHECKSUM) => {
                if v.len() < 4 {
                    return Some(Err(EINVAL));
                }
                match int() {
                    n if n > 0 && n & 1 != 0 => Err(EINVAL),
                    n => {
                        self.checksum = n.max(-1);
                        Ok(())
                    }
                }
            }
            (AF_INET6, SOL_RAW, _) => Err(ENOPROTOOPT),
            _ => return None,
        })
    }

    /// The values of the raw socket's own options. Returns None for those not handled here.
    pub fn getsockopt(&self, level: i32, name: i32) -> Option<Result<Vec<u8>, i32>> {
        const ENOPROTOOPT: i32 = 92;
        let int = |b: bool| (b as i32).to_le_bytes().to_vec();
        Some(Ok(match (self.family, level, name) {
            (AF_INET, SOL_RAW, ICMP_FILTER) => self.filter[..4].to_vec(),
            (AF_INET6, SOL_ICMPV6, ICMP_FILTER) => self.filter.to_vec(),
            (AF_INET, SOL_RAW, _) => return Some(Err(ENOPROTOOPT)),
            (AF_INET, SOL_IP, IP_RECVTTL) => int(self.recvttl),
            (AF_INET, SOL_IP, IP_PKTINFO) => int(self.pktinfo),
            (AF_INET, SOL_IP, IP_RECVERR) | (AF_INET6, SOL_IPV6, IPV6_RECVERR) => int(self.recverr),
            (AF_INET6, SOL_RAW | SOL_IPV6, IPV6_CHECKSUM) => self.checksum.to_le_bytes().to_vec(),
            (AF_INET6, SOL_RAW, _) => return Some(Err(ENOPROTOOPT)),
            _ => return None,
        }))
    }

    /// The guest sends a message: when it is an echo request, record its id.
    pub fn sent(&mut self, data: &[u8]) {
        if data.len() >= 8 && data[0] == self.echo().0 {
            self.id = Some([data[4], data[5]]);
        }
    }

    fn echo(&self) -> (u8, u8) {
        echo_types(self.family)
    }

    /// Whether a message of type `ty` passes the filter.
    fn passes(&self, ty: u8) -> bool {
        let word = |i: usize| u32::from_le_bytes(self.filter[4 * i..4 * i + 4].try_into().unwrap());
        !match self.family {
            AF_INET => ty < 32 && word(0) & (1 << ty) != 0,
            _ => word(ty as usize >> 5) & (1 << (ty & 31)) != 0,
        }
    }

    /// What the queued message `it` looks like when given to this socket (`key` is its wake
    /// object): for IPv4 an IP header is prepended, and control messages follow its own options.
    /// `src` is the peer address already translated for the guest, and `local` maps a local
    /// address on the host to the guest's. Returns the data and the control messages.
    pub fn present(
        &self,
        it: &Item,
        key: usize,
        src: &[u8],
        local: impl Fn([u8; 4]) -> [u8; 4],
    ) -> (Vec<u8>, Vec<u8>) {
        // Control messages on a message another socket received follow that socket's options and
        // are not given to this one
        let other: &[u8] = if it.origin == key { &it.other } else { &[] };
        if self.family != AF_INET {
            return (it.icmp.clone(), other.to_vec());
        }
        let ttl = it.ttl.unwrap_or(64);
        // struct in_pktinfo: ipi_addr is the destination address in the IP header. A ping
        // socket does not fill in ipi_spec_dst; a raw socket fills in the local address used for
        // replies (Linux's fib_compute_spec_dst: for messages to a local unicast address it is
        // that address)
        let pktinfo = it.pktinfo.map(|p| {
            let addr = local(p[8..12].try_into().unwrap());
            let spec = match p[4..8] {
                [0, 0, 0, 0] => addr,
                _ => local(p[4..8].try_into().unwrap()),
            };
            let mut out = p;
            out[4..8].copy_from_slice(&spec);
            out[8..12].copy_from_slice(&addr);
            out
        });
        // Linux's order: socket-level ones such as timestamps first, then ip_cmsg_recv's PKTINFO
        // and TTL, then other IP-level ones
        let mut control = Vec::new();
        for (level, _, whole, _) in cmsgs(other) {
            if level == SOL_SOCKET {
                control.extend_from_slice(whole);
            }
        }
        if let Some(p) = pktinfo.filter(|_| self.pktinfo) {
            control.extend(cmsg(SOL_IP, IP_PKTINFO, &p));
        }
        if self.recvttl && it.ttl.is_some() {
            control.extend(cmsg(SOL_IP, IP_TTL, &i32::from(ttl).to_le_bytes()));
        }
        for (level, _, whole, _) in cmsgs(other) {
            if level != SOL_SOCKET {
                control.extend_from_slice(whole);
            }
        }
        let total = 20 + it.icmp.len();
        let mut h = vec![0u8; 20];
        h[0] = 0x45;
        h[2..4].copy_from_slice(&(total.min(0xffff) as u16).to_be_bytes());
        h[8] = ttl;
        h[9] = IPPROTO_ICMP as u8;
        if let Some(a) = src.get(4..8) {
            h[12..16].copy_from_slice(a);
        }
        if let Some(p) = pktinfo {
            h[16..20].copy_from_slice(&p[8..12]);
        }
        let sum = checksum(&h);
        h[10..12].copy_from_slice(&sum);
        h.extend_from_slice(&it.icmp);
        (h, control)
    }
}

impl RawIcmp {
    /// The control messages for an error-queue entry given to the guest: `IP_TTL` and
    /// `IP_PKTINFO` enabled on the host for it are removed when it has not enabled them itself.
    pub fn err_control(&self, e: &ErrItem) -> Vec<u8> {
        let mut out = Vec::new();
        for (level, ty, whole, _) in cmsgs(&e.control) {
            let drop = match (self.family, level, ty) {
                (AF_INET, SOL_IP, IP_TTL) => !self.recvttl,
                (AF_INET, SOL_IP, IP_PKTINFO) => !self.pktinfo,
                _ => false,
            };
            if !drop {
                out.extend_from_slice(whole);
            }
        }
        out
    }
}

// ---- Aggregation points ----

/// The ICMP aggregation point of one network namespace (see the module docs).
pub(crate) struct Tap {
    /// The host fds of the sources: readable when there are messages or errors.
    watch: Watch,
    /// Its network namespace; None is the boot one.
    net: Option<Rc<NetNs>>,
    sources: Vec<Source>,
    receivers: Vec<Receiver>,
}

/// A host socket the aggregation point takes messages from.
struct Source {
    fd: i32,
    sock: Weak<RefCell<Socket>>,
    family: i32,
    /// A UDP socket: only its error queue is taken.
    udp: bool,
    /// Its wake object (so raw sockets can tell which messages they received themselves).
    key: usize,
}

/// The queue of one raw socket.
struct Receiver {
    key: usize,
    sock: Weak<RefCell<Socket>>,
    family: i32,
    queue: VecDeque<Item>,
    bytes: usize,
    /// The number of messages put in the queue (`EPOLLET` triggers on each).
    arrivals: u64,
    /// Its error queue (`MSG_ERRQUEUE`) when the guest enabled `IP_RECVERR`.
    errors: VecDeque<ErrItem>,
}

/// One entry in a raw socket's error queue: the same as what Linux's `ip_recv_error` hands out,
/// the data is the original message (with the guest's id restored), the peer is the original
/// destination address, and the control messages include `IP_RECVERR`.
pub(crate) struct ErrItem {
    pub data: Vec<u8>,
    /// The address on the host.
    pub addr: Vec<u8>,
    /// The control messages the host gave.
    control: Vec<u8>,
    errno: i32,
}

/// One received ICMP message (without the IP header for IPv4).
#[derive(Clone)]
pub(crate) struct Item {
    pub icmp: Vec<u8>,
    /// The peer (host) address: the source of a message, the reporter of an error.
    pub src: Vec<u8>,
    /// The TTL in the IP header (hop limit for IPv6), if the host gave it.
    ttl: Option<u8>,
    /// The IPv4 `struct in_pktinfo`, if the host gave it.
    pktinfo: Option<[u8; 12]>,
    /// Other control messages the host gave (wanted by the source socket's options).
    other: Vec<u8>,
    /// The wake object of the source socket.
    origin: usize,
}

/// A message the aggregation point took from the ping socket behind a raw socket: its traffic is
/// counted on that socket ([`account`]).
pub(crate) struct Arrival {
    sock: Rc<RefCell<Socket>>,
    bytes: usize,
    peer: Vec<u8>,
}

impl Tap {
    /// The aggregation point's host fd: readable when a source has something to take.
    pub fn fd(&self) -> i32 {
        self.watch.fd()
    }

    fn add_source(&mut self, sock: &Rc<RefCell<Socket>>, fd: i32, family: i32, udp: bool) {
        // Watch only the UDP socket's errors (the error queue), not the data it receives
        if self.watch.add(fd, !udp).is_ok() {
            self.sources.push(Source {
                fd,
                sock: Rc::downgrade(sock),
                family,
                udp,
                key: wake::of(sock),
            });
        }
    }

    fn remove_source(&mut self, fd: i32) {
        self.watch.remove(fd);
        self.sources.retain(|s| s.fd != fd);
    }

    fn has_receivers(&mut self, family: i32) -> bool {
        self.receivers.retain(|r| r.sock.strong_count() > 0);
        self.receivers.iter().any(|r| r.family == family)
    }
}

/// The aggregation point of network namespace `net`, created if there is none yet.
pub(crate) fn tap_for(k: &mut Kernel, net: &Option<Rc<NetNs>>) -> Result<Rc<RefCell<Tap>>, Errno> {
    k.icmp_taps.retain(|t| t.strong_count() > 0);
    if let Some(t) = find_tap(k, net) {
        return Ok(t);
    }
    let tap = Rc::new(RefCell::new(Tap {
        watch: Watch::new().map_err(Errno)?,
        net: net.clone(),
        sources: Vec::new(),
        receivers: Vec::new(),
    }));
    k.icmp_taps.push(Rc::downgrade(&tap));
    Ok(tap)
}

fn find_tap(k: &Kernel, net: &Option<Rc<NetNs>>) -> Option<Rc<RefCell<Tap>>> {
    k.icmp_taps
        .iter()
        .filter_map(Weak::upgrade)
        .find(|t| match (&t.borrow().net, net) {
            (None, None) => true,
            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
            _ => false,
        })
}

/// Registers a new raw socket `sock` (host fd `fd`) with its aggregation point: its ping socket is
/// a source, and it has a queue of its own.
pub(crate) fn register(tap: &Rc<RefCell<Tap>>, sock: &Rc<RefCell<Socket>>, fd: i32, family: i32) {
    let mut t = tap.borrow_mut();
    t.add_source(sock, fd, family, false);
    t.receivers.push(Receiver {
        key: wake::of(sock),
        sock: Rc::downgrade(sock),
        family,
        queue: VecDeque::new(),
        bytes: 0,
        arrivals: 0,
        errors: VecDeque::new(),
    });
}

// ---- UDP sockets that set a TTL ----

/// UDP socket options: records that the guest set a TTL, or enabled `IP_RECVERR` itself. When
/// Some is returned the option is not passed to the host (while the aggregation point watches the
/// socket, `IP_RECVERR` on the host is always on), and the value is the result the guest wants.
pub(crate) fn udp_setsockopt(s: &mut InetSocket, level: i32, name: i32, v: &[u8]) -> Option<bool> {
    if s.ty != SOCK_DGRAM || s.proto != IPPROTO_UDP {
        return None;
    }
    let on = v
        .first_chunk::<4>()
        .map_or(v.first().is_some_and(|&b| b != 0), |b| {
            i32::from_le_bytes(*b) != 0
        });
    match (level, name) {
        (SOL_IP, IP_TTL) | (SOL_IPV6, IPV6_UNICAST_HOPS) => {
            s.ttl_set = true;
            None
        }
        (SOL_IP, IP_RECVERR) | (SOL_IPV6, IPV6_RECVERR) => {
            s.guest_recverr = on;
            // The guest turns it off while watched: only recorded; turning it on: no longer
            // watched afterwards ([`untap`])
            s.udp_tap.as_ref().map(|_| on)
        }
        _ => None,
    }
}

/// UDP socket option values: while the aggregation point watches it, `IP_RECVERR` reports what the
/// guest set.
pub(crate) fn udp_getsockopt(s: &InetSocket, level: i32, name: i32) -> Option<Vec<u8>> {
    match (level, name) {
        (SOL_IP, IP_RECVERR) | (SOL_IPV6, IPV6_RECVERR) if s.udp_tap.is_some() => {
            Some(i32::from(s.guest_recverr).to_le_bytes().to_vec())
        }
        _ => None,
    }
}

// ---- Sending and receiving ----

/// Host socket `sock` is about to send or receive: a raw socket returns its aggregation point; a
/// UDP socket that set a TTL, whose guest has not enabled `IP_RECVERR` itself, in a namespace that
/// also has a raw socket of the same protocol family, is handed to the aggregation point to watch
/// (enabling `IP_RECVERR` on the host), and the aggregation point is returned. When Some is
/// returned, the errors on this socket must be taken off before the send or receive ([`dispatch`]
/// taking only from it).
pub(crate) fn before_io(k: &Kernel, sock: &Rc<RefCell<Socket>>) -> Option<Rc<RefCell<Tap>>> {
    let (fd, family, tapped, wanted) = match &*sock.borrow() {
        Socket::Inet(InetSocket { icmp: Some(r), .. }) => return Some(r.tap.clone()),
        Socket::Inet(s) if s.ty == SOCK_DGRAM && s.proto == IPPROTO_UDP => (
            s.fd,
            s.family,
            s.udp_tap.clone(),
            s.ttl_set && !s.guest_recverr,
        ),
        _ => return None,
    };
    if let Some(t) = tapped {
        if t.borrow_mut().has_receivers(family) {
            return Some(t);
        }
        // All raw sockets are closed
        untap(sock);
        return None;
    }
    if !wanted {
        return None;
    }
    let net = match &*sock.borrow() {
        Socket::Inet(s) => s.net.clone(),
        _ => None,
    };
    let tap = find_tap(k, &net).filter(|t| t.borrow_mut().has_receivers(family))?;
    let (level, name) = recverr_option(family);
    tuxport_hal::net::setsockopt(fd, level, name, &1i32.to_le_bytes()).ok()?;
    tap.borrow_mut().add_source(sock, fd, family, true);
    if let Socket::Inet(s) = &mut *sock.borrow_mut() {
        s.udp_tap = Some(tap.clone());
    }
    Some(tap)
}

/// The aggregation point stops watching UDP socket `sock` (the guest wants `IP_RECVERR` itself,
/// or all raw sockets are closed): errors already arrived are dispatched first, and `IP_RECVERR`
/// on the host is restored to what the guest set. The caller must not be borrowing any socket.
pub(crate) fn untap(sock: &Rc<RefCell<Socket>>) {
    let (tap, fd, family, guest) = match &*sock.borrow() {
        Socket::Inet(s) => match &s.udp_tap {
            Some(t) => (t.clone(), s.fd, s.family, s.guest_recverr),
            None => return,
        },
        _ => return,
    };
    let _ = dispatch(&tap, Some(fd));
    tap.borrow_mut().remove_source(fd);
    let (level, name) = recverr_option(family);
    let _ = tuxport_hal::net::setsockopt(fd, level, name, &i32::from(guest).to_le_bytes());
    if let Socket::Inet(s) = &mut *sock.borrow_mut() {
        s.udp_tap = None;
    }
}

fn recverr_option(family: i32) -> (i32, i32) {
    match family {
        AF_INET => (SOL_IP, IP_RECVERR),
        _ => (SOL_IPV6, IPV6_RECVERR),
    }
}

/// The ICMP error the aggregation point recorded for `sock` to report to the guest: takes it
/// (Linux's `sock_error`).
pub(crate) fn take_error(sock: &RefCell<Socket>) -> Option<Errno> {
    match &*sock.borrow() {
        Socket::Inet(s) => Some(Errno(s.icmp_err.replace(0))).filter(|e| e.0 != 0),
        _ => None,
    }
}

/// The errors that sends and receives get because of the `IP_RECVERR` enabled on the host for the
/// guest (the results of `icmp_err_convert`, `icmpv6_err_convert`): on getting one, take the
/// errors off first and try again.
pub(crate) fn from_icmp(e: i32) -> bool {
    const EHOSTDOWN: i32 = 112;
    const ENONET: i32 = 64;
    const EPROTO: i32 = 71;
    [
        Errno::ENETUNREACH,
        Errno::EHOSTUNREACH,
        Errno::ENOPROTOOPT,
        Errno::ECONNREFUSED,
        Errno::EMSGSIZE,
        Errno::EOPNOTSUPP,
        Errno::EACCES,
    ]
    .iter()
    .any(|x| x.0 == e)
        || [EHOSTDOWN, ENONET, EPROTO].contains(&e)
}

/// Takes one entry from the error queue of raw socket `sock`; the error to report becomes the
/// next entry's (Linux's `sock_dequeue_err_skb`), or 0 when there is none left.
pub(crate) fn take_err(tap: &Rc<RefCell<Tap>>, sock: &RefCell<Socket>) -> Option<ErrItem> {
    let key = wake::of(sock);
    let mut t = tap.borrow_mut();
    let r = t.receivers.iter_mut().find(|r| r.key == key)?;
    let e = r.errors.pop_front()?;
    if let Socket::Inet(s) = &*sock.borrow() {
        s.icmp_err.set(r.errors.front().map_or(0, |n| n.errno));
    }
    Some(e)
}

/// Takes a message from the queue of a raw socket (wake object `key`), without removing it when
/// `peek`.
pub(crate) fn take(tap: &Rc<RefCell<Tap>>, key: usize, peek: bool) -> Option<Item> {
    let mut t = tap.borrow_mut();
    let r = t.receivers.iter_mut().find(|r| r.key == key)?;
    if peek {
        return r.queue.front().cloned();
    }
    let it = r.queue.pop_front()?;
    r.bytes -= it.icmp.len();
    Some(it)
}

/// Fits control messages into the `room` bytes the guest gave (Linux's `put_cmsg`): the one that
/// does not fit is cut short (`cmsg_len` written as the remaining length), the ones after it are
/// left out, and the returned flags include `MSG_CTRUNC`.
pub(crate) fn fit_control(control: &[u8], room: usize) -> (Vec<u8>, u32) {
    let mut out = Vec::new();
    let mut flags = 0;
    for (_, _, whole, _) in cmsgs(control) {
        let left = room - out.len();
        let len = u64::from_le_bytes(whole[..8].try_into().unwrap()) as usize;
        if left < CMSG_HDR {
            flags |= MSG_CTRUNC;
            break;
        }
        if left < len {
            flags |= MSG_CTRUNC;
            let start = out.len();
            out.extend_from_slice(&whole[..left]);
            out[start..start + 8].copy_from_slice(&(left as u64).to_le_bytes());
            break;
        }
        out.extend_from_slice(&whole[..whole.len().min(left)]);
    }
    (out, flags)
}

// ---- Readiness ----

/// Whether the error queue of a raw socket (wake object `key`) has anything in it.
pub(crate) fn has_errors(tap: &RefCell<Tap>, key: usize) -> bool {
    let t = tap.borrow();
    t.receivers
        .iter()
        .any(|r| r.key == key && !r.errors.is_empty())
}

/// The queue of a raw socket (wake object `key`): (whether it has messages, the number of messages
/// ever put in).
pub(crate) fn queued(tap: &RefCell<Tap>, key: usize) -> (bool, u64) {
    let t = tap.borrow();
    t.receivers
        .iter()
        .find(|r| r.key == key)
        .map_or((false, 0), |r| (!r.queue.is_empty(), r.arrivals))
}

/// The timer thread: takes what has arrived at each aggregation point on behalf of waiters in poll
/// and epoll. Returns the host fds to wait on along with the rest.
pub(crate) fn check_taps(k: &mut Kernel) -> Vec<(i32, i16, i16)> {
    const POLLIN: i16 = 1;
    k.icmp_taps.retain(|t| t.strong_count() > 0);
    let taps: Vec<_> = k.icmp_taps.iter().filter_map(Weak::upgrade).collect();
    let mut host = Vec::new();
    for tap in taps {
        let arrivals = dispatch(&tap, None);
        account(k, arrivals);
        host.push((tap.borrow().fd(), POLLIN, 0));
    }
    host
}

/// Records the messages the aggregation point took from ping sockets in the traffic counters
/// ([`crate::traffic`]).
pub(crate) fn account(k: &mut Kernel, arrivals: Vec<Arrival>) {
    for a in arrivals {
        if let Socket::Inet(s) = &mut *a.sock.borrow_mut()
            && s.net.is_none()
        {
            k.traffic.account(s, true, a.bytes, &a.peer);
        }
    }
}

// ---- Dispatch ----

/// Takes the messages and errors already arrived at the sources and gives them to the queues of
/// the raw sockets. `only` takes only from this one host fd (clearing its errors before a send or
/// receive). The caller must not be borrowing any socket. Returns the messages taken from ping
/// sockets, whose traffic is to be recorded ([`account`]).
pub(crate) fn dispatch(tap: &Rc<RefCell<Tap>>, only: Option<i32>) -> Vec<Arrival> {
    let sources: Vec<_> = {
        let mut t = tap.borrow_mut();
        t.sources.retain(|s| s.sock.strong_count() > 0);
        t.sources
            .iter()
            .filter(|s| only.is_none_or(|fd| fd == s.fd))
            .filter_map(|s| Some((s.fd, s.sock.upgrade()?, s.family, s.udp, s.key)))
            .collect()
    };
    let ids = guest_ids(tap);
    let mut items = Vec::new();
    let mut arrivals = Vec::new();
    let mut buf = vec![0u8; PACKET_MAX];
    for (fd, sock, family, udp, key) in sources {
        // On sockets without IP_RECVERR, Linux reports only hard errors and only to connected
        // ones (harderr in __udp4_lib_err, raw_err, ping_err), and the error must match the
        // connection: a UDP socket's host connection may have been changed to receive errors
        // (syscall::net::send), so the comparison here is against the peer the guest connected to.
        // A raw socket whose guest enabled IP_RECVERR itself: errors also go into its error queue,
        // and every one is reported (raw_err)
        let (peer, recverr) = match &*sock.borrow() {
            Socket::Inet(s) => {
                let recverr = s.icmp.as_ref().is_some_and(|r| r.recverr);
                (
                    s.peer.clone().filter(|_| !recverr && !s.guest_recverr),
                    recverr,
                )
            }
            _ => (None, false),
        };
        let net = tap.borrow().net.clone();
        let flags = MSG_ERRQUEUE | MSG_DONTWAIT;
        while let Ok(r) = tuxport_hal::net::recvmsg(fd, &mut buf, CONTROL_ROOM, flags) {
            let data = &buf[..r.len.min(buf.len())];
            let it = match family {
                AF_INET => error_v4(fd, udp, &ids, key, data, &r),
                _ => error_v6(fd, udp, &ids, key, data, &r),
            };
            let Some((it, errno, original)) = it else {
                continue;
            };
            // Only for comparing with the connected peer: addresses of other namespaces are not
            // translated (the kernel is not available here), only its own loopback range
            let dst = match &net {
                Some(ns) => {
                    let w = crate::netns::World {
                        all: vec![ns.clone()],
                    };
                    let role = crate::netns::Role::Peer;
                    crate::netns::from_host(&w, ns, &r.addr, role, family == AF_INET6 && !udp)
                }
                None => r.addr.clone(),
            };
            let to_peer = peer.as_ref().is_some_and(|p| same_peer(p, &dst, udp));
            if recverr {
                let e = ErrItem {
                    data: original,
                    addr: r.addr.clone(),
                    control: r.control.clone(),
                    errno,
                };
                let mut t = tap.borrow_mut();
                if let Some(rx) = t.receivers.iter_mut().find(|rx| rx.key == key)
                    && rx.errors.len() < 64
                {
                    rx.errors.push_back(e);
                }
            }
            if recverr || to_peer && hard(family, it.icmp[0], it.icmp[1]) {
                if let Socket::Inet(s) = &*sock.borrow() {
                    s.icmp_err.set(errno);
                }
                request_wake_object(wake::of(&sock));
            }
            items.push((family, it));
        }
        if udp {
            continue;
        }
        while let Ok(r) = tuxport_hal::net::recvmsg(fd, &mut buf, CONTROL_ROOM, MSG_DONTWAIT) {
            let mut icmp = buf[..r.len.min(buf.len())].to_vec();
            if icmp.first() == Some(&echo_types(family).1) {
                restore_id(&mut icmp, &ids);
            }
            arrivals.push(Arrival {
                sock: sock.clone(),
                bytes: r.len,
                peer: r.addr.clone(),
            });
            if !icmp.is_empty() {
                items.push((family, data_item(family, icmp, r.addr, &r.control, key)));
            }
        }
    }
    deliver(tap, items);
    arrivals
}

/// The numbers (echo ids) of the ping sockets of the raw sockets on the host, and the ids the guest
/// sent last. Replies and errors do not necessarily reach the ping socket that sent the request:
/// the guest may bind the receiving socket to the sending socket's address (busybox's
/// traceroute), and the host finds the socket by number, which finds the one bound first.
fn guest_ids(tap: &Rc<RefCell<Tap>>) -> Vec<([u8; 2], [u8; 2])> {
    let socks: Vec<_> = tap
        .borrow()
        .sources
        .iter()
        .filter(|s| !s.udp)
        .filter_map(|s| s.sock.upgrade())
        .collect();
    socks
        .iter()
        .filter_map(|sock| {
            let Socket::Inet(InetSocket {
                icmp: Some(r), fd, ..
            }) = &*sock.borrow()
            else {
                return None;
            };
            let guest = r.id?;
            let name = tuxport_hal::net::sockname(*fd, false).ok()?;
            Some((name.get(2..4)?.try_into().ok()?, guest))
        })
        .collect()
}

/// Reassembles an IPv4 error-queue entry into an ICMP error message: type, code, the attached
/// value (the MTU when fragmentation is needed, the position of a parameter problem), quoting the
/// original IP header (the local end is the source address, the address the error queue gives is
/// the destination, TTL recorded as 1) and the start of the original message. The reporter is the
/// peer address.
fn error_v4(
    fd: i32,
    udp: bool,
    ids: &[([u8; 2], [u8; 2])],
    key: usize,
    data: &[u8],
    r: &tuxport_hal::net::Received,
) -> Option<(Item, i32, Vec<u8>)> {
    let (ee, offender) = extended_err(&r.control, SOL_IP, IP_RECVERR, SO_EE_ORIGIN_ICMP)?;
    let (ty, code) = (ee[5], ee[6]);
    let info = u32::from_le_bytes(ee[8..12].try_into().unwrap());
    let mut it = data_item(
        AF_INET,
        Vec::new(),
        offender.get(..16)?.to_vec(),
        &r.control,
        key,
    );
    it.other.clear();
    let local = tuxport_hal::net::sockname(fd, false).unwrap_or_default();
    // The source address of the original message: the destination address of the error message
    // (IP_PKTINFO), or the local address; when the local end is a wildcard address (an unconnected
    // UDP socket) it is taken from routing
    let ours: [u8; 4] = match it.pktinfo {
        Some(p) => p[8..12].try_into().unwrap(),
        None => local.get(4..8).map_or([0; 4], |a| a.try_into().unwrap()),
    };
    let ours = match (ours, r.addr.get(4..8)) {
        ([0, 0, 0, 0], Some(dst)) => tuxport_hal::net::source_for(dst)
            .ok()
            .and_then(|a| a.try_into().ok())
            .unwrap_or(ours),
        _ => ours,
    };
    let (sport, dport) = ports(&local, &r.addr);
    let inner = quoted(udp, ids, ICMP_ECHO, data, &sport, &dport);
    let original = quoted(false, ids, ICMP_ECHO, data, &[], &[]);
    let mut ip = vec![0u8; 20];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&((20 + inner.len()).min(0xffff) as u16).to_be_bytes());
    ip[8] = 1;
    ip[9] = if udp {
        IPPROTO_UDP as u8
    } else {
        IPPROTO_ICMP as u8
    };
    ip[12..16].copy_from_slice(&ours);
    ip[16..20].copy_from_slice(r.addr.get(4..8)?);
    let sum = checksum(&ip);
    ip[10..12].copy_from_slice(&sum);
    let mut icmp = vec![ty, code, 0, 0, 0, 0, 0, 0];
    match (ty, code) {
        (ICMP_DEST_UNREACH, ICMP_FRAG_NEEDED) => {
            icmp[6..8].copy_from_slice(&(info as u16).to_be_bytes())
        }
        (ICMP_PARAMETERPROB, _) => icmp[4] = info as u8,
        _ => {}
    }
    icmp.extend_from_slice(&ip);
    icmp.extend_from_slice(&inner);
    let sum = checksum(&icmp);
    icmp[2..4].copy_from_slice(&sum);
    it.icmp = icmp;
    Some((
        it,
        i32::from_le_bytes(ee[..4].try_into().unwrap()),
        original,
    ))
}

/// Reassembles an IPv6 error-queue entry into an ICMPv6 error message (the same as [`error_v4`],
/// quoting an IPv6 header; the checksum includes the pseudo-header: reporter to local end).
fn error_v6(
    fd: i32,
    udp: bool,
    ids: &[([u8; 2], [u8; 2])],
    key: usize,
    data: &[u8],
    r: &tuxport_hal::net::Received,
) -> Option<(Item, i32, Vec<u8>)> {
    let (ee, offender) = extended_err(&r.control, SOL_IPV6, IPV6_RECVERR, SO_EE_ORIGIN_ICMP6)?;
    let (ty, code) = (ee[5], ee[6]);
    let info = u32::from_le_bytes(ee[8..12].try_into().unwrap());
    let offender = offender.get(..28)?.to_vec();
    let local = tuxport_hal::net::sockname(fd, false).unwrap_or_default();
    let mut ours: [u8; 16] = local.get(8..24).map_or([0; 16], |a| a.try_into().unwrap());
    if ours == [0; 16]
        && let Some(src) = r
            .addr
            .get(8..24)
            .and_then(|d| tuxport_hal::net::source_for(d).ok())
        && let Ok(a) = src.try_into()
    {
        ours = a;
    }
    let ours = &ours[..];
    let (sport, dport) = ports(&local, &r.addr);
    let inner = quoted(udp, ids, ICMPV6_ECHO_REQUEST, data, &sport, &dport);
    let original = quoted(false, ids, ICMPV6_ECHO_REQUEST, data, &[], &[]);
    let mut icmp = vec![ty, code, 0, 0, 0, 0, 0, 0];
    if matches!(ty, ICMPV6_PKT_TOOBIG | ICMPV6_PARAMPROB) {
        icmp[4..8].copy_from_slice(&info.to_be_bytes());
    }
    icmp.extend_from_slice(&[0x60, 0, 0, 0]);
    icmp.extend_from_slice(&(inner.len().min(0xffff) as u16).to_be_bytes());
    icmp.push(if udp {
        IPPROTO_UDP as u8
    } else {
        NEXTHDR_ICMPV6
    });
    icmp.push(1);
    icmp.extend_from_slice(ours);
    icmp.extend_from_slice(r.addr.get(8..24)?);
    icmp.extend_from_slice(&inner);
    let mut pseudo = offender[8..24].to_vec();
    pseudo.extend_from_slice(ours);
    pseudo.extend_from_slice(&(icmp.len() as u32).to_be_bytes());
    pseudo.extend_from_slice(&[0, 0, 0, NEXTHDR_ICMPV6]);
    pseudo.extend_from_slice(&icmp);
    let sum = checksum(&pseudo);
    icmp[2..4].copy_from_slice(&sum);
    let ttl = cmsgs(&r.control)
        .find(|&(l, t, _, _)| (l, t) == (SOL_IPV6, IPV6_HOPLIMIT))
        .and_then(|(_, _, _, d)| int_cmsg(d));
    let errno = i32::from_le_bytes(ee[..4].try_into().unwrap());
    Some((
        Item {
            icmp,
            src: offender,
            ttl,
            pktinfo: None,
            other: Vec::new(),
            origin: key,
        },
        errno,
        original,
    ))
}

/// One error-queue entry (`struct sock_extended_err` and the reporter's address); None when it was
/// not reported by ICMP.
fn extended_err(control: &[u8], level: i32, name: i32, origin: u8) -> Option<(&[u8], &[u8])> {
    let (_, _, _, data) = cmsgs(control).find(|&(l, t, _, _)| (l, t) == (level, name))?;
    (data.len() >= 16 && data[4] == origin).then(|| (&data[..16], &data[16..]))
}

/// The ports (network byte order) in the local address and the original destination address, 0
/// when unavailable.
fn ports(local: &[u8], dst: &[u8]) -> ([u8; 2], [u8; 2]) {
    let port = |a: &[u8]| a.get(2..4).map_or([0; 2], |p| [p[0], p[1]]);
    (port(local), port(dst))
}

/// The quoted original message: an ICMP echo request gets the guest's id back; for UDP a UDP
/// header is added (the error queue gives only the payload; the ports come from the local address
/// and the original destination address).
fn quoted(
    udp: bool,
    ids: &[([u8; 2], [u8; 2])],
    echo: u8,
    data: &[u8],
    sport: &[u8],
    dport: &[u8],
) -> Vec<u8> {
    if udp {
        let mut u = Vec::with_capacity(8 + data.len());
        u.extend_from_slice(sport);
        u.extend_from_slice(dport);
        u.extend_from_slice(&((8 + data.len()).min(0xffff) as u16).to_be_bytes());
        u.extend_from_slice(&[0, 0]);
        u.extend_from_slice(data);
        u
    } else {
        let mut d = data.to_vec();
        if d.first() == Some(&echo) {
            restore_id(&mut d, ids);
        }
        d
    }
}

/// The destination address `dst` of the failed message is the peer `peer` the socket is connected
/// to: same protocol family and address, and for UDP also the same port (raw sockets have no
/// ports).
fn same_peer(peer: &[u8], dst: &[u8], udp: bool) -> bool {
    let range = match peer
        .get(..2)
        .map(|f| i32::from(u16::from_le_bytes([f[0], f[1]])))
    {
        Some(AF_INET) => 4..8,
        Some(AF_INET6) => 8..24,
        _ => return false,
    };
    peer.get(..2) == dst.get(..2)
        && peer.get(range.clone()).is_some()
        && peer.get(range.clone()) == dst.get(range)
        && (!udp || peer.get(2..4) == dst.get(2..4))
}

/// The ICMP errors Linux reports to connected sockets without `IP_RECVERR` (the fatal ones in
/// `icmp_err_convert`, `icmpv6_err_convert`, plus fragmentation needed and parameter problems);
/// soft errors such as time exceeded are not reported.
fn hard(family: i32, ty: u8, code: u8) -> bool {
    match family {
        AF_INET => match ty {
            ICMP_DEST_UNREACH => matches!(code, 2..=4 | 6..=10 | 13..=15),
            ICMP_PARAMETERPROB => true,
            _ => false,
        },
        _ => match ty {
            1 => matches!(code, 1 | 4..=6),
            ICMPV6_PKT_TOOBIG | ICMPV6_PARAMPROB => true,
            _ => false,
        },
    }
}

fn echo_types(family: i32) -> (u8, u8) {
    match family {
        AF_INET => (ICMP_ECHO, ICMP_ECHOREPLY),
        _ => (ICMPV6_ECHO_REQUEST, ICMPV6_ECHO_REPLY),
    }
}

/// When the id of an echo message is the number of a ping socket on the host, replaces it with the
/// id the guest sent from that socket ([`guest_ids`]), updating the checksum incrementally.
fn restore_id(icmp: &mut [u8], ids: &[([u8; 2], [u8; 2])]) {
    if icmp.len() >= 6
        && let Some(&(_, id)) = ids.iter().find(|(host, _)| icmp[4..6] == *host)
    {
        let sum = csum_replace([icmp[2], icmp[3]], [icmp[4], icmp[5]], id);
        icmp[2..4].copy_from_slice(&sum);
        icmp[4..6].copy_from_slice(&id);
    }
}

/// The checksum after one 16-bit word it covers changes from `old` to `new` (equation 3 of RFC
/// 1624): the rest of the content (ICMPv6's pseudo-header) need not be known.
fn csum_replace(sum: [u8; 2], old: [u8; 2], new: [u8; 2]) -> [u8; 2] {
    let mut acc = !u16::from_be_bytes(sum) as u32
        + !u16::from_be_bytes(old) as u32
        + u16::from_be_bytes(new) as u32;
    while acc > 0xffff {
        acc = (acc & 0xffff) + (acc >> 16);
    }
    (!(acc as u16)).to_be_bytes()
}

/// A message received on a ping socket: for IPv4, takes out the `IP_TTL` and `IP_PKTINFO` the host
/// enabled for the guest, to be given again by the guest's own options when handed to it
/// ([`RawIcmp::present`]).
fn data_item(family: i32, icmp: Vec<u8>, src: Vec<u8>, control: &[u8], key: usize) -> Item {
    let mut it = Item {
        icmp,
        src,
        ttl: None,
        pktinfo: None,
        other: Vec::new(),
        origin: key,
    };
    for (level, ty, whole, data) in cmsgs(control) {
        match (family, level, ty) {
            (AF_INET, SOL_IP, IP_TTL) => it.ttl = int_cmsg(data),
            (AF_INET, SOL_IP, IP_PKTINFO) => it.pktinfo = data.first_chunk::<12>().copied(),
            _ => it.other.extend_from_slice(whole),
        }
    }
    it
}

fn int_cmsg(data: &[u8]) -> Option<u8> {
    data.first_chunk::<4>()
        .map(|b| i32::from_le_bytes(*b) as u8)
}

/// Puts messages into the queues of raw sockets of the same protocol family whose filters let them
/// through, and wakes the threads waiting on them.
fn deliver(tap: &Rc<RefCell<Tap>>, items: Vec<(i32, Item)>) {
    if items.is_empty() {
        return;
    }
    let mut woken = Vec::new();
    let mut t = tap.borrow_mut();
    t.receivers.retain(|r| r.sock.strong_count() > 0);
    for r in &mut t.receivers {
        let Some(sock) = r.sock.upgrade() else {
            continue;
        };
        let s = sock.borrow();
        let Socket::Inet(InetSocket {
            icmp: Some(raw), ..
        }) = &*s
        else {
            continue;
        };
        let mut got = false;
        for (family, it) in &items {
            if *family != r.family || !raw.passes(it.icmp[0]) || r.bytes + it.icmp.len() > QUEUE_MAX
            {
                continue;
            }
            r.bytes += it.icmp.len();
            r.arrivals += 1;
            r.queue.push_back(it.clone());
            got = true;
        }
        drop(s);
        if got {
            woken.push(sock);
        }
    }
    drop(t);
    for s in woken {
        request_wake_object(wake::of(&s));
    }
}

// ---- Checksums and control messages ----

/// The internet checksum (RFC 1071), in network byte order.
fn checksum(b: &[u8]) -> [u8; 2] {
    let mut acc: u32 = b
        .chunks(2)
        .map(|w| u16::from_be_bytes([w[0], *w.get(1).unwrap_or(&0)]) as u32)
        .sum();
    while acc > 0xffff {
        acc = (acc & 0xffff) + (acc >> 16);
    }
    (!(acc as u16)).to_be_bytes()
}

/// One control message (with its alignment padding).
fn cmsg(level: i32, ty: i32, data: &[u8]) -> Vec<u8> {
    let mut c = ((CMSG_HDR + data.len()) as u64).to_le_bytes().to_vec();
    c.extend_from_slice(&level.to_le_bytes());
    c.extend_from_slice(&ty.to_le_bytes());
    c.extend_from_slice(data);
    c.resize(c.len().next_multiple_of(8), 0);
    c
}

/// Iterates over the host's control messages: (level, type, the whole message with its alignment
/// padding, data).
fn cmsgs(control: &[u8]) -> impl Iterator<Item = (i32, i32, &[u8], &[u8])> {
    let mut off = 0;
    std::iter::from_fn(move || {
        let h = control.get(off..off + CMSG_HDR)?;
        let len = u64::from_le_bytes(h[..8].try_into().unwrap()) as usize;
        if len < CMSG_HDR || off + len > control.len() {
            return None;
        }
        let level = i32::from_le_bytes(h[8..12].try_into().unwrap());
        let ty = i32::from_le_bytes(h[12..16].try_into().unwrap());
        let space = (len + 7) & !7;
        let whole = &control[off..(off + space).min(control.len())];
        let data = &control[off + CMSG_HDR..off + len];
        off += space;
        Some((level, ty, whole, data))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn received(addr: Vec<u8>, control: Vec<u8>) -> tuxport_hal::net::Received {
        tuxport_hal::net::Received {
            len: 0,
            addr,
            control,
            flags: 0,
        }
    }

    fn sockaddr_in(ip: [u8; 4], port: u16) -> Vec<u8> {
        let mut a = vec![0u8; 16];
        a[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        a[2..4].copy_from_slice(&port.to_be_bytes());
        a[4..8].copy_from_slice(&ip);
        a
    }

    /// An echo message: type, id (the host's number 0x0597), sequence 7, 56 bytes of payload.
    fn echo(ty: u8) -> Vec<u8> {
        let mut e = vec![ty, 0, 0, 0, 0x05, 0x97, 0, 7];
        e.extend(8u8..64);
        let sum = checksum(&e);
        e[2..4].copy_from_slice(&sum);
        e
    }

    const IDS: [([u8; 2], [u8; 2]); 1] = [([0x05, 0x97], [0x12, 0x34])];

    #[test]
    fn replies_get_the_guest_id_and_an_ip_header() {
        let mut reply = echo(ICMP_ECHOREPLY);
        restore_id(&mut reply, &IDS);
        assert_eq!(reply[4..6], [0x12, 0x34]);
        assert_eq!(
            checksum(&reply),
            [0, 0],
            "the checksum is updated along with it"
        );
        let mut other = echo(ICMP_ECHOREPLY);
        restore_id(&mut other, &[([9, 9], [1, 1])]);
        assert_eq!(
            other,
            echo(ICMP_ECHOREPLY),
            "ids that are not the number of a raw socket on the host are not replaced"
        );

        let mut control = cmsg(SOL_IP, IP_PKTINFO, &[1, 0, 0, 0, 0, 0, 0, 0, 10, 0, 0, 2]);
        control.extend(cmsg(SOL_IP, IP_TTL, &57i32.to_le_bytes()));
        let src = sockaddr_in([10, 0, 0, 1], 0);
        let it = data_item(AF_INET, reply.clone(), src.clone(), &control, 7);
        let tap = Rc::new(RefCell::new(Tap {
            watch: Watch::new().unwrap(),
            net: None,
            sources: Vec::new(),
            receivers: Vec::new(),
        }));
        let mut r = RawIcmp::new(AF_INET, tap);
        r.setsockopt(SOL_IP, IP_PKTINFO, &[1]).unwrap().unwrap();
        let (packet, kept) = r.present(&it, 7, &src, |a| a);
        assert_eq!(packet.len(), 84);
        assert_eq!(packet[..4], [0x45, 0, 0, 84]);
        assert_eq!((packet[8], packet[9]), (57, 1));
        assert_eq!(packet[12..20], [10, 0, 0, 1, 10, 0, 0, 2]);
        assert_eq!(checksum(&packet[..20]), [0, 0]);
        assert_eq!(packet[20..], reply);
        // IP_RECVTTL not enabled: only PKTINFO, with ipi_spec_dst filled in with the destination
        // address
        assert_eq!(kept.len(), 32);
        assert_eq!(
            kept[CMSG_HDR + 4..CMSG_HDR + 12],
            [10, 0, 0, 2, 10, 0, 0, 2]
        );

        // The filter blocks echo replies
        assert!(r.passes(ICMP_ECHOREPLY));
        r.setsockopt(SOL_RAW, ICMP_FILTER, &1u32.to_le_bytes())
            .unwrap()
            .unwrap();
        assert!(!r.passes(ICMP_ECHOREPLY));
        assert!(r.passes(ICMP_ECHO));
    }

    #[test]
    fn errors_become_whole_icmp_messages() {
        // 10.0.0.254 reports time exceeded, quoting the echo request the guest sent to 1.1.1.1
        let mut ee = 113u32.to_le_bytes().to_vec();
        ee.extend([SO_EE_ORIGIN_ICMP, 11, 0, 0]);
        ee.extend([0; 8]);
        ee.extend(sockaddr_in([10, 0, 0, 254], 0));
        let mut control = cmsg(SOL_IP, IP_PKTINFO, &[1, 0, 0, 0, 0, 0, 0, 0, 172, 17, 0, 2]);
        control.extend(cmsg(SOL_IP, IP_TTL, &63i32.to_le_bytes()));
        control.extend(cmsg(SOL_IP, IP_RECVERR, &ee));
        let r = received(sockaddr_in([1, 1, 1, 1], 0), control);
        let request = echo(ICMP_ECHO);
        let (it, errno, original) = error_v4(-1, false, &IDS, 3, &request, &r).unwrap();
        assert_eq!(errno, 113);
        assert_eq!(original[4..6], [0x12, 0x34]);
        assert_eq!(it.src, sockaddr_in([10, 0, 0, 254], 0));
        assert_eq!(it.ttl, Some(63));
        assert!(it.other.is_empty());
        let icmp = &it.icmp;
        assert_eq!(icmp[..2], [11, 0]);
        assert_eq!(checksum(icmp), [0, 0]);
        let inner = &icmp[8..28];
        assert_eq!((inner[0], inner[9]), (0x45, IPPROTO_ICMP as u8));
        assert_eq!(inner[12..20], [172, 17, 0, 2, 1, 1, 1, 1]);
        assert_eq!(checksum(inner), [0, 0]);
        assert_eq!(icmp[28..], original);
        // Time exceeded is a soft error, port unreachable a hard one
        assert!(!hard(AF_INET, 11, 0));
        assert!(hard(AF_INET, ICMP_DEST_UNREACH, 3));
        assert!(!hard(AF_INET, ICMP_DEST_UNREACH, 1));
        assert!(hard(AF_INET6, 1, 4));
        assert!(!hard(AF_INET6, 3, 0));

        // UDP: the error queue holds only the payload, and a UDP header is added (the destination
        // port comes from the original destination address)
        let r = received(sockaddr_in([127, 0, 0, 1], 33434), r.control.clone());
        let (it, ..) = error_v4(-1, true, &[], 3, b"abcd", &r).unwrap();
        let udp = &it.icmp[28..];
        assert_eq!(it.icmp[8 + 9], IPPROTO_UDP as u8);
        assert_eq!(udp[2..6], [0x82, 0x9a, 0, 12]);
        assert_eq!(&udp[8..], b"abcd");

        // Errors not reported by ICMP (local ones) are skipped
        let mut local = ee.clone();
        local[4] = 1;
        let r = received(
            sockaddr_in([1, 1, 1, 1], 0),
            cmsg(SOL_IP, IP_RECVERR, &local),
        );
        assert!(error_v4(-1, false, &[], 3, &request, &r).is_none());
    }

    #[test]
    fn control_messages_are_cut_like_put_cmsg() {
        let mut control = cmsg(SOL_IP, IP_PKTINFO, &[0; 12]);
        control.extend(cmsg(SOL_IP, IP_TTL, &64i32.to_le_bytes()));
        // Does not fit: the first whole, the second cut to the remaining length; the last one's
        // alignment padding may be left out
        let (fit, flags) = fit_control(&control, 32 + 18);
        assert_eq!(flags, MSG_CTRUNC);
        assert_eq!(fit.len(), 50);
        assert_eq!(fit[32..40], 18u64.to_le_bytes());
        assert_eq!(
            fit_control(&control, 32 + 12),
            (control[..32].to_vec(), MSG_CTRUNC)
        );
        assert_eq!(fit_control(&control, 32 + 20), (control[..52].to_vec(), 0));
        assert_eq!(fit_control(&control, 8), (Vec::new(), MSG_CTRUNC));
        assert_eq!(fit_control(&control, 64), (control.clone(), 0));
    }
}
