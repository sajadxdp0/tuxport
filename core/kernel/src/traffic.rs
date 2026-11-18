//! Traffic counters for network interfaces and protocols (the kernel/net map): `/proc/net/dev`,
//! `/sys/class/net/<interface>/statistics/` and netlink's `IFLA_STATS` and `IFLA_STATS64` come
//! from the interface counters, `/proc/net/snmp` and `netstat` from the protocol counters
//! ([`Protocols`]).
//!
//! On Android the app cannot read the host's counters (`/proc/net` and netlink's `RTM_GETLINK`
//! are both blocked), and the host's counters also include the traffic of the app itself and of
//! other apps. The guest's TCP, UDP and ping sockets all send and receive through the kernel
//! layer, so they are counted here: only the guest's traffic, just as a container sees the traffic
//! of its own network namespace.
//!
//! - Only payload is counted, without protocol headers, ACKs and retransmissions; packet counts of
//!   stream sockets are estimated from the interface's MTU minus the IPv4 and TCP headers and the
//!   timestamp option, and each datagram counts as one.
//! - The interface is identified by the socket's local address: that is the result of the host's
//!   routing, taken once at the first send or receive (getsockname), kept on the socket, and
//!   identified again on bind and connect. When the local end is a wildcard address (UDP without
//!   connect, ping) that is recorded too, and afterwards every call is identified by that call's
//!   peer: loopback addresses count on the loopback interface, others on the default interface.
//! - DNS stub queries and answers do not go through host sockets and are not counted; AF_UNIX is
//!   not traffic on a network interface.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::net::{
    AF_INET, AF_INET6, IPPROTO_ICMP, IPPROTO_TCP, IPPROTO_UDP, InetSocket, SOCK_DGRAM, SOCK_STREAM,
};

/// The counters of one interface.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct IfStats {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
}

/// Protocol counters (`/proc/net/snmp`, `netstat`, [`crate::net::proc_snmp`]). Like the interface
/// counters they count only the payload the guest sends and receives through the kernel layer;
/// packet counts (IP packets, TCP segments) are estimates, and handshakes, ACKs and
/// retransmissions are not counted. IP, UDP and ICMP count only IPv4 (IPv4-mapped addresses on
/// IPv6 sockets included), TCP does not distinguish address families, as on Linux. ICMP covers
/// only the ping sockets' own echo requests and replies; on loopback addresses it is the host that
/// answers the requests, and that is not counted.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Protocols {
    pub ip_in_packets: u64,
    pub ip_out_packets: u64,
    pub ip_in_octets: u64,
    pub ip_out_octets: u64,
    pub icmp_in_echo_replies: u64,
    pub icmp_out_echoes: u64,
    /// Connects for which the host sent a SYN (`syscall::net::count_connect`).
    pub tcp_active_opens: u64,
    /// Connections accepted. Linux counts them when the handshake completes; ones not yet accepted
    /// are not seen here.
    pub tcp_passive_opens: u64,
    pub tcp_attempt_fails: u64,
    pub tcp_in_segs: u64,
    pub tcp_out_segs: u64,
    pub udp_in_datagrams: u64,
    pub udp_out_datagrams: u64,
}

/// The interface a socket is identified with and the MSS for estimating packet counts, kept on
/// [`InetSocket`]. When `iface` is None the local end is a wildcard address and every call is
/// identified by its peer.
#[derive(Clone, Debug)]
pub(crate) struct Route {
    pub iface: Option<Rc<str>>,
    pub mss: u32,
    /// The local address is IPv4 (IPv4-mapped included). When the local end is the IPv6 wildcard
    /// address, every call is identified by its peer.
    pub v4: bool,
}

/// The address table is fetched again when it is used after this long: after a network switch,
/// new sockets recognize the new interfaces.
const TABLE_TTL: Duration = Duration::from_secs(5);
/// When a local address is not in the table (the network just changed) the table is fetched again,
/// but at least this long apart.
const MISS_REFRESH: Duration = Duration::from_secs(1);
/// The bytes of an MTU taken by the IPv4 and TCP headers and the timestamp option: the rest is the
/// MSS.
const TCP_OVERHEAD: u32 = 52;

const IFF_UP: u32 = 0x1;
const IFF_LOOPBACK: u32 = 0x8;

/// The address table of the host's interfaces.
struct Table {
    at: Instant,
    /// (address, interface name, MSS): 4 bytes for IPv4, 16 bytes for IPv6.
    addrs: Vec<(Vec<u8>, Rc<str>, u32)>,
    /// The loopback interface.
    lo: Rc<str>,
    /// The default interface: the interface of the local address the host chooses for sending to
    /// the internet.
    default: Rc<str>,
}

impl Table {
    fn lookup(&self, ip: &[u8]) -> Option<(Rc<str>, u32)> {
        self.addrs
            .iter()
            .find(|(a, ..)| a == ip)
            .map(|(_, n, mss)| (n.clone(), *mss))
    }

    /// The MSS on interface `name`; with no address on it, by Ethernet's MTU.
    fn mss_of(&self, name: &str) -> u32 {
        self.addrs
            .iter()
            .find(|(_, n, _)| &**n == name)
            .map_or(1500 - TCP_OVERHEAD, |(.., mss)| *mss)
    }
}

/// Per-interface counters, in [`crate::kernel::Kernel`], keyed by interface name.
#[derive(Default)]
pub(crate) struct Traffic {
    stats: BTreeMap<Rc<str>, IfStats>,
    table: Option<Table>,
    pub protocols: Protocols,
}

impl Traffic {
    /// Records `bytes` bytes received (`rx`) or sent by socket `s`. `peer` is this call's peer
    /// address (the sendto target or the recvfrom source, as sockaddr bytes), and may be empty.
    pub fn account(&mut self, s: &mut InetSocket, rx: bool, bytes: usize, peer: &[u8]) {
        if bytes == 0 && s.ty == SOCK_STREAM {
            return;
        }
        let (iface, mss) = self.route(s, peer);
        let packets = if s.ty == SOCK_STREAM {
            (bytes as u64).div_ceil(mss.max(1) as u64)
        } else {
            1
        };
        let st = self.stats.entry(iface).or_default();
        if rx {
            st.rx_bytes += bytes as u64;
            st.rx_packets += packets;
        } else {
            st.tx_bytes += bytes as u64;
            st.tx_packets += packets;
        }
        let v4 = s.family == AF_INET
            || s.route.as_ref().is_some_and(|r| r.v4)
            || ip_of(peer).is_some_and(|ip| ip.len() == 4);
        let p = &mut self.protocols;
        let (packets_in, packets_out) = if rx { (packets, 0) } else { (0, packets) };
        if v4 {
            p.ip_in_packets += packets_in;
            p.ip_out_packets += packets_out;
            if rx {
                p.ip_in_octets += bytes as u64;
            } else {
                p.ip_out_octets += bytes as u64;
            }
        }
        match (s.ty, s.proto) {
            (SOCK_STREAM, IPPROTO_TCP) => {
                p.tcp_in_segs += packets_in;
                p.tcp_out_segs += packets_out;
            }
            (SOCK_DGRAM, IPPROTO_UDP) if v4 => {
                p.udp_in_datagrams += packets_in;
                p.udp_out_datagrams += packets_out;
            }
            (SOCK_DGRAM, IPPROTO_ICMP) => {
                p.icmp_in_echo_replies += packets_in;
                p.icmp_out_echoes += packets_out;
            }
            _ => {}
        }
    }

    /// Which interface this call's traffic of socket `s` counts on, and the TCP MSS on that
    /// interface (module docs).
    fn route(&mut self, s: &mut InetSocket, peer: &[u8]) -> (Rc<str>, u32) {
        if s.route.is_none() {
            let local = tuxport_hal::net::sockname(s.fd, false)
                .ok()
                .and_then(|a| ip_of(&a));
            s.route = match local {
                Some(ip) if ip.iter().any(|&b| b != 0) => {
                    let (iface, mss) = self.iface_of(&ip);
                    Some(Route {
                        iface: Some(iface),
                        mss,
                        v4: ip.len() == 4,
                    })
                }
                Some(ip) => Some(Route {
                    iface: None,
                    mss: 0,
                    v4: ip.len() == 4,
                }),
                // The local address is unavailable: identify this call by its peer and try again
                // next time
                None => None,
            };
        }
        if let Some(Route {
            iface: Some(iface),
            mss,
            ..
        }) = &s.route
        {
            return (iface.clone(), *mss);
        }
        let t = self.table();
        if ip_of(peer).is_some_and(|ip| is_loopback(&ip)) {
            (t.lo.clone(), 0)
        } else {
            (t.default.clone(), 0)
        }
    }

    /// The interface of local address `ip` and the MSS there.
    fn iface_of(&mut self, ip: &[u8]) -> (Rc<str>, u32) {
        if let Some(n) = self.table().lookup(ip) {
            return n;
        }
        if self
            .table
            .as_ref()
            .is_some_and(|t| t.at.elapsed() >= MISS_REFRESH)
        {
            self.refresh();
        }
        let t = self.table();
        t.lookup(ip).unwrap_or_else(|| {
            let iface = if is_loopback(ip) { &t.lo } else { &t.default };
            (iface.clone(), t.mss_of(iface))
        })
    }

    /// The address table, fetched again first if it has expired.
    fn table(&mut self) -> &Table {
        if self
            .table
            .as_ref()
            .is_none_or(|t| t.at.elapsed() >= TABLE_TTL)
        {
            self.refresh();
        }
        self.table.as_ref().unwrap()
    }

    fn refresh(&mut self) {
        let list = tuxport_hal::net::interfaces();
        let mut addrs = Vec::new();
        for i in &list {
            let name: Rc<str> = i.name.as_str().into();
            let mss = i.mtu.saturating_sub(TCP_OVERHEAD);
            addrs.extend(
                i.addrs
                    .iter()
                    .map(|(_, a, _)| (a.clone(), name.clone(), mss)),
            );
        }
        let lo: Rc<str> = list
            .iter()
            .find(|i| i.flags & IFF_LOOPBACK != 0)
            .map_or("lo".into(), |i| i.name.as_str().into());
        let lookup = |ip: &[u8]| {
            addrs
                .iter()
                .find(|(a, ..)| a == ip)
                .map(|(_, n, _)| n.clone())
        };
        let default = [AF_INET, AF_INET6]
            .into_iter()
            .filter_map(outgoing_source)
            .find_map(|ip| lookup(&ip))
            .or_else(|| {
                list.iter()
                    .find(|i| i.flags & IFF_UP != 0 && i.flags & IFF_LOOPBACK == 0)
                    .map(|i| i.name.as_str().into())
            })
            .unwrap_or_else(|| lo.clone());
        self.table = Some(Table {
            at: Instant::now(),
            addrs,
            lo,
            default,
        });
    }

    /// The counters of interface `name`.
    pub fn get(&self, name: &str) -> IfStats {
        self.stats.get(name).copied().unwrap_or_default()
    }
}

/// The IP address in a sockaddr: 4 bytes for IPv4, 16 bytes for IPv6, and an IPv4-mapped IPv6
/// address as IPv4.
fn ip_of(addr: &[u8]) -> Option<Vec<u8>> {
    let family = u16::from_ne_bytes(addr.get(..2)?.try_into().ok()?) as i32;
    match family {
        AF_INET => Some(addr.get(4..8)?.to_vec()),
        AF_INET6 => {
            let ip = addr.get(8..24)?;
            Some(
                match ip.strip_prefix(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff][..]) {
                    Some(v4) => v4.to_vec(),
                    None => ip.to_vec(),
                },
            )
        }
        _ => None,
    }
}

fn is_loopback(ip: &[u8]) -> bool {
    match ip.len() {
        4 => ip[0] == 127,
        16 => ip[..15].iter().all(|&b| b == 0) && ip[15] == 1,
        _ => false,
    }
}

/// The local address the host chooses for sending to the internet
/// ([`tuxport_hal::net::source_for`]), or None when there is no route for this address family.
pub(crate) fn outgoing_source(family: i32) -> Option<Vec<u8>> {
    let dst: &[u8] = if family == AF_INET {
        &[8, 8, 8, 8]
    } else {
        &[
            0x20, 0x01, 0x48, 0x60, 0x48, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0x88, 0x88,
        ]
    };
    tuxport_hal::net::source_for(dst).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::SOCK_DGRAM;

    fn sockaddr(s: &str) -> Vec<u8> {
        tuxport_hal::net::sockaddr(s.parse().unwrap())
    }

    #[test]
    fn addresses_are_read_from_sockaddrs() {
        assert_eq!(ip_of(&sockaddr("127.0.0.1:80")), Some(vec![127, 0, 0, 1]));
        assert_eq!(
            ip_of(&sockaddr("[::ffff:10.0.0.2]:80")),
            Some(vec![10, 0, 0, 2])
        );
        let v6 = ip_of(&sockaddr("[::1]:80")).unwrap();
        assert_eq!(v6.len(), 16);
        assert!(is_loopback(&v6));
        assert!(is_loopback(&[127, 1, 2, 3]));
        assert!(!is_loopback(&[10, 0, 0, 1]));
        assert_eq!(ip_of(&[]), None);
        assert_eq!(ip_of(&[1, 0, b'/', 0]), None);
    }

    /// TCP connections on loopback: both ends count on the loopback interface, bytes are payload,
    /// and packet counts are estimated from the MSS.
    #[test]
    fn loopback_connections_count_on_the_loopback_interface() {
        use std::net::{TcpListener, TcpStream};
        use std::os::fd::IntoRawFd;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let mut a = InetSocket::new(client.into_raw_fd(), AF_INET, SOCK_STREAM);
        let mut b = InetSocket::new(server.into_raw_fd(), AF_INET, SOCK_STREAM);
        let mut t = Traffic::default();
        t.account(&mut a, false, 100_000, &[]);
        t.account(&mut b, true, 60_000, &[]);
        t.account(&mut b, true, 40_000, &[]);
        t.account(&mut b, true, 0, &[]);
        let lo = t.table().lo.clone();
        let route = a.route.clone().unwrap();
        let mss = route.mss as u64;
        assert_eq!(route.iface.as_deref(), Some(&*lo));
        // Loopback's MTU 65536 minus the protocol headers
        assert_eq!(mss, 65536 - 52);
        let st = t.get(&lo);
        assert_eq!((st.tx_bytes, st.rx_bytes), (100_000, 100_000));
        assert_eq!(st.tx_packets, 100_000u64.div_ceil(mss));
        assert_eq!(
            st.rx_packets,
            60_000u64.div_ceil(mss) + 40_000u64.div_ceil(mss)
        );
    }

    /// UDP without connect: the local end is a wildcard address; this is recorded and every call
    /// identifies the interface by its peer.
    #[test]
    fn unconnected_datagrams_count_by_peer() {
        let fd = tuxport_hal::net::socket(AF_INET, SOCK_DGRAM, 0).unwrap();
        let mut s = InetSocket::new(fd, AF_INET, SOCK_DGRAM);
        let mut t = Traffic::default();
        t.account(&mut s, false, 100, &sockaddr("127.0.0.1:9"));
        t.account(&mut s, false, 0, &sockaddr("127.0.0.1:9"));
        assert!(s.route.as_ref().is_some_and(|r| r.iface.is_none()));
        let lo = t.table().lo.clone();
        let st = t.get(&lo);
        assert_eq!((st.tx_bytes, st.tx_packets), (100, 2));
    }
}
