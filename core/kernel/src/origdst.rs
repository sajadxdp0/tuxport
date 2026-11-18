//! `SO_ORIGINAL_DST` (docs/rounds/180-original-dst.md): transparent proxies (iptables REDIRECT
//! paired with `sslocal --protocol redir`, redsocks, clash's redir-port and the like) ask it, on an
//! accepted connection, where the connection originally wanted to go. Linux answers it from
//! connection tracking (`getorigdst` and `ipv6_getorigdst` in `nf_conntrack_proto.c`).
//!
//! Tuxport has no connection tracking: nat is done by the socket layer at connect time
//! ([`crate::nft`], `syscall::net::route`), and the connection on the host goes to the address
//! after nat. So at connect time, TCP connections whose destination nat rewrote are recorded here
//! by their 4-tuple on the host ([`Table::register`]); at accept time the accepting end claims, by
//! the same 4-tuple, the original destination seen in its own namespace ([`Table::claim`]), which
//! is then kept on the socket ([`crate::net::Track::orig_dst`]). It can still be claimed when the
//! client closed before the accept (Linux's connection-tracking entry is still there too).

use std::collections::HashMap;
use std::time::{Duration, Instant};

const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;

/// One end on the host: an IPv6 address (IPv4 ones are written IPv4-mapped, so the two kinds of
/// socket still match up when connected to each other) and a port.
pub(crate) type End = ([u8; 16], u16);

/// Converts a host sockaddr into an [`End`]; None for anything other than IPv4 or IPv6.
pub(crate) fn end_of(addr: &[u8]) -> Option<End> {
    let family = u16::from_le_bytes([*addr.first()?, *addr.get(1)?]);
    let port = u16::from_be_bytes([*addr.get(2)?, *addr.get(3)?]);
    let mut ip = [0u8; 16];
    match family {
        AF_INET => {
            ip[10..12].copy_from_slice(&[0xff, 0xff]);
            ip[12..].copy_from_slice(addr.get(4..8)?);
        }
        AF_INET6 => ip.copy_from_slice(addr.get(8..24)?),
        _ => return None,
    }
    Some((ip, port))
}

/// Records not yet claimed are kept at most this long (Linux connection tracking's timeouts for
/// SYN_SENT and TIME_WAIT are both 120 seconds), and at most this many (when full the oldest is
/// dropped). Connections to listeners outside Tuxport (other apps on the phone) are never claimed.
const KEEP: Duration = Duration::from_secs(120);
const MAX: usize = 4096;

/// TCP connections that went through nat, keyed by (the client's local end on the host, the
/// destination connected to on the host).
#[derive(Default)]
pub(crate) struct Table {
    entries: HashMap<(End, End), Entry>,
    /// When expired records were last pruned: at most once a second.
    pruned: Option<Instant>,
}

struct Entry {
    /// The original destination (a guest sockaddr) seen in each network namespace
    /// ([`crate::nsproxy::NetNs::id`]): in the namespace that made the connection it is the one
    /// before OUTPUT nat, in other namespaces it entered the one before their PREROUTING nat.
    origs: Vec<(u64, Vec<u8>)>,
    born: Instant,
}

impl Table {
    /// No records: connect and accept need not fetch the addresses on the host.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// connect made a connection (the 4-tuple `key` on the host): `origs` are the namespaces where
    /// nat rewrote the destination and the original destination seen there; empty means no nat, in
    /// which case records earlier left for the same 4-tuple are removed.
    pub(crate) fn register(&mut self, key: (End, End), origs: Vec<(u64, Vec<u8>)>) {
        if origs.is_empty() {
            if !self.entries.is_empty() {
                self.entries.remove(&key);
            }
            return;
        }
        let now = Instant::now();
        if self
            .pruned
            .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1))
        {
            self.entries
                .retain(|_, e| now.duration_since(e.born) < KEEP);
            self.pruned = Some(now);
        }
        if self.entries.len() >= MAX
            && !self.entries.contains_key(&key)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.born)
                .map(|(k, _)| *k)
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(key, Entry { origs, born: now });
    }

    /// A connection from accept (whose peer and local end on the host are `key`) in namespace
    /// `ns`: claims the original destination seen there. The whole record is removed (a connection
    /// has only one accepting end).
    pub(crate) fn claim(&mut self, key: (End, End), ns: u64) -> Option<Vec<u8>> {
        if self.entries.is_empty() {
            return None;
        }
        let e = self.entries.remove(&key)?;
        if Instant::now().duration_since(e.born) >= KEEP {
            return None;
        }
        e.origs.into_iter().find(|(n, _)| *n == ns).map(|(_, a)| a)
    }
}

/// The `getsockopt` answer: the guest sockaddr `addr` written as a `sockaddr_in` (`SOL_IP`, 16
/// bytes, the IPv4 address taken out of an IPv4-mapped one) or a `sockaddr_in6` (`SOL_IPV6`, 28
/// bytes; flow label and scope id are 0: Tuxport does not support setting flow labels, and
/// link-local addresses cannot reach other namespaces).
pub(crate) fn encode(addr: &[u8], v6: bool) -> Option<Vec<u8>> {
    let (ip, port) = end_of(addr)?;
    let port = port.to_be_bytes();
    if v6 {
        let mut out = vec![0u8; 28];
        out[..2].copy_from_slice(&AF_INET6.to_le_bytes());
        out[2..4].copy_from_slice(&port);
        out[8..24].copy_from_slice(&ip);
        Some(out)
    } else {
        if ip[..12] != [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff] {
            return None;
        }
        let mut out = vec![0u8; 16];
        out[..2].copy_from_slice(&AF_INET.to_le_bytes());
        out[2..4].copy_from_slice(&port);
        out[4..8].copy_from_slice(&ip[12..]);
        Some(out)
    }
}

/// Whether the guest sockaddr is an IPv6 connection (neither IPv4 nor an IPv4-mapped address):
/// connection tracking keeps packets apart by protocol family.
pub(crate) fn is_v6(addr: &[u8]) -> bool {
    end_of(addr).is_some_and(|(ip, _)| ip[..12] != [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(ip: [u8; 4], port: u16) -> Vec<u8> {
        let mut a = vec![0u8; 16];
        a[..2].copy_from_slice(&AF_INET.to_le_bytes());
        a[2..4].copy_from_slice(&port.to_be_bytes());
        a[4..8].copy_from_slice(&ip);
        a
    }

    fn v6(ip: [u8; 16], port: u16) -> Vec<u8> {
        let mut a = vec![0u8; 28];
        a[..2].copy_from_slice(&AF_INET6.to_le_bytes());
        a[2..4].copy_from_slice(&port.to_be_bytes());
        a[8..24].copy_from_slice(&ip);
        a
    }

    fn mapped(ip: [u8; 4]) -> [u8; 16] {
        let mut m = [0u8; 16];
        m[10..12].copy_from_slice(&[0xff, 0xff]);
        m[12..].copy_from_slice(&ip);
        m
    }

    #[test]
    fn ipv4_and_mapped_ends_match() {
        assert_eq!(
            end_of(&v4([127, 0, 0, 1], 1081)),
            end_of(&v6(mapped([127, 0, 0, 1]), 1081))
        );
        assert!(!is_v6(&v6(mapped([127, 0, 0, 1]), 1)));
        let mut lo6 = [0u8; 16];
        lo6[15] = 1;
        assert!(is_v6(&v6(lo6, 1)));
        assert_eq!(end_of(&[1, 0, 0, 0]), None);
    }

    #[test]
    fn claimed_once_by_the_right_namespace() {
        let mut t = Table::default();
        let key = (
            end_of(&v4([127, 0, 0, 1], 40000)).unwrap(),
            end_of(&v4([127, 0, 0, 1], 1081)).unwrap(),
        );
        let orig = v4([127, 0, 0, 2], 2290);
        t.register(key, vec![(1, orig.clone()), (7, v4([10, 0, 0, 2], 80))]);
        // Other 4-tuples cannot claim it
        let other = (key.1, key.0);
        assert_eq!(t.claim(other, 1), None);
        assert_eq!(t.claim(key, 1), Some(orig.clone()));
        assert_eq!(t.claim(key, 1), None);
        // Not the recorded namespace: nothing to claim, and the record is removed too
        t.register(key, vec![(1, orig.clone())]);
        assert_eq!(t.claim(key, 2), None);
        assert_eq!(t.claim(key, 1), None);
        // A later connection on the same 4-tuple without nat replaces the old record
        t.register(key, vec![(1, orig)]);
        t.register(key, Vec::new());
        assert_eq!(t.claim(key, 1), None);
    }

    #[test]
    fn bounded() {
        let mut t = Table::default();
        for p in 0..(MAX as u16 + 10) {
            let key = (
                end_of(&v4([127, 0, 0, 1], p)).unwrap(),
                end_of(&v4([127, 0, 0, 1], 1)).unwrap(),
            );
            t.register(key, vec![(1, v4([1, 1, 1, 1], p))]);
        }
        assert_eq!(t.entries.len(), MAX);
    }

    #[test]
    fn encodes_like_linux() {
        let a = encode(&v6(mapped([127, 0, 0, 2]), 2290), false).unwrap();
        assert_eq!(a, v4([127, 0, 0, 2], 2290));
        let mut lo6 = [0u8; 16];
        lo6[15] = 1;
        assert_eq!(encode(&v6(lo6, 80), false), None);
        assert_eq!(encode(&v6(lo6, 80), true).unwrap(), v6(lo6, 80));
    }
}
