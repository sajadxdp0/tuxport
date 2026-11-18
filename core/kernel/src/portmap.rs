//! Port mapping below 1024 (the kernel/net map).
//!
//! Apps on Android have no `CAP_NET_BIND_SERVICE` and generally cannot bind ports below 1024 on
//! the host; root in the guest still wants to bind 22, 80 and the like (the default configuration
//! of sshd and nginx). When a guest socket binds such a port and the host lets it bind the original
//! port, it binds the original port: Google Play system updates since May 2026 (Android 13 and
//! later, kernel 5.15 and later) let apps bind TCP 20–23, 80, 443, 445, 515, 631 and UDP 319, 320,
//! 443, and development hosts running as root or in a container can bind them too. When the host
//! refuses (EACCES) or another app on the phone holds the port (EADDRINUSE), the host socket binds
//! another port, and the guest side cannot tell: getsockname, getpeername, accept, recvfrom and
//! `/proc/net` all report the original port, and connecting to this port on the local machine is
//! switched to the host's port. Other devices and other apps on the phone have to connect to the
//! port on the host; the first time the app sees a listen on a non-loopback address it notifies
//! the user ([`PortMap::take_listens`]).
//!
//! After the original port it tries `port + offset`, `port + 2 × offset`, `port + 3 × offset` in
//! turn (up to 65535), and when all are taken lets the host assign one. The last three digits are
//! always the original port (with offset 10000, 22 becomes 10022, 20022, 30022), recognizable at a
//! glance and never taking another port's place. As long as any socket binds a (network namespace,
//! protocol, port), it keeps the same host port (`SO_REUSEPORT`, IPv4 and IPv6 bound separately),
//! and the guest's own conflicts are EADDRINUSE as usual; TCP and UDP on the same port use the same
//! host port when possible (DNS's 53). A bind within [`STICKY`] after all of them closed still
//! tries the original host port first and is EADDRINUSE when that fails: after a TCP connection
//! closes its host port is in TIME_WAIT, and on Linux binding again without `SO_REUSEADDR` is
//! EADDRINUSE too, so it must not be taken as held by another program and moved to another port.

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::cell::RefCell;
use crate::errno::Errno;

/// The default offset.
pub const DEFAULT_OFFSET: u16 = 10000;

/// The most listen events kept that the app has not taken yet (tuxport-run does not take them).
const MAX_EVENTS: usize = 32;

/// How long after a mapping is fully released a new bind still uses the original host port:
/// longer than TIME_WAIT (60 seconds on both Linux and Android).
pub const STICKY: Duration = Duration::from_secs(120);

/// How long after a mapping is fully released the host port is still translated back to the guest
/// port. Linux translates when a packet arrives, here it happens only when the other side calls
/// recvfrom or accept: when a service closes its socket right after answering a datagram (as
/// `srv.py` does), the answer is still in the other side's receive queue, and when the other side
/// reads it the source is still the guest's port. The chance that the host gives this port to
/// another, unmapped socket in the same namespace during that time is small (the host picks
/// ephemeral ports at random).
pub const LINGER: Duration = Duration::from_secs(10);

/// The key of the registry: network namespace (0 for the boot one, otherwise the `NetNs`'s
/// address), protocol, guest port, and whether the binding takes only IPv6 ([`bind_any`]).
type Key = (usize, i32, u16, bool);

/// A host port: network namespace, protocol, port on the host.
type HostKey = (usize, i32, u16);

struct Entry {
    host: u16,
    refs: u32,
}

/// The guest listens on a port on a non-loopback address for the first time (design doc §12
/// item 7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenEvent {
    /// The name (comm) of the listening process.
    pub comm: String,
    /// The port the guest sees.
    pub port: u16,
    /// The port on the host: other devices connect to it (the same as `port` without a mapping).
    pub host_port: u16,
}

/// The port-mapping registry, one for the whole kernel ([`crate::kernel::Kernel::portmap`]).
pub struct PortMap {
    /// 0 means no mapping: binding low ports is left to the host as before (mostly EACCES on
    /// Android).
    pub offset: u16,
    /// Map even when the host allows the original port
    /// ([`crate::MachineOptions::always_map_low_ports`]): to test mapping on hosts that can bind
    /// low ports.
    pub always_map: bool,
    entries: HashMap<Key, Entry>,
    /// Host port → guest port (the same key, with the port on the host); fully released ones carry
    /// the release time ([`LINGER`]).
    by_host: HashMap<HostKey, (u16, Option<Instant>)>,
    /// Fully released mappings: the host port and the release time ([`STICKY`]).
    released: HashMap<Key, (u16, Instant)>,
    /// (guest port, host port) pairs already reported.
    reported: HashSet<(u16, u16)>,
    events: VecDeque<ListenEvent>,
}

impl PortMap {
    pub fn new() -> PortMap {
        PortMap {
            offset: DEFAULT_OFFSET,
            always_map: false,
            entries: HashMap::new(),
            by_host: HashMap::new(),
            released: HashMap::new(),
            reported: HashSet::new(),
            events: VecDeque::new(),
        }
    }

    /// Whether this port is mapped.
    pub fn maps(&self, port: u16) -> bool {
        self.offset != 0 && (1..1024).contains(&port)
    }

    /// Which host port an already mapped guest port is (connecting to this port on the local
    /// machine is switched to it). `v6`: the destination is an IPv6 address (not IPv4-mapped),
    /// which a binding that takes only IPv6 receives first, then a dual-stack one; an IPv4
    /// destination never goes to an IPv6-only binding.
    pub fn host_port(&self, ns: usize, proto: i32, port: u16, v6: bool) -> Option<u16> {
        let get = |v6only| self.entries.get(&(ns, proto, port, v6only)).map(|e| e.host);
        if v6 {
            get(true).or_else(|| get(false))
        } else {
            get(false)
        }
    }

    /// When a host port comes from a mapping (including just released ones, [`LINGER`]), the port
    /// the guest sees.
    pub fn guest_port(&self, ns: usize, proto: i32, host: u16) -> Option<u16> {
        self.by_host
            .get(&(ns, proto, host))
            .filter(|(_, t)| t.is_none_or(|t| t.elapsed() < LINGER))
            .map(|&(g, _)| g)
    }

    /// Records a listen on a non-loopback address: queues an event the first time a (guest port,
    /// host port) appears. Only a queued one counts as reported, so a listen dropped by a full
    /// queue is reported when it happens again.
    pub fn listened(&mut self, comm: String, port: u16, host_port: u16) {
        if self.events.len() < MAX_EVENTS && self.reported.insert((port, host_port)) {
            self.events.push_back(ListenEvent {
                comm,
                port,
                host_port,
            });
        }
    }

    /// Takes the queued listen events.
    pub fn take_listens(&mut self) -> Vec<ListenEvent> {
        self.events.drain(..).collect()
    }
}

impl Default for PortMap {
    fn default() -> Self {
        PortMap::new()
    }
}

/// A mapping a socket holds bound: returned on drop (the last return removes the registration).
pub struct Lease {
    map: Rc<RefCell<PortMap>>,
    key: Key,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut m = self.map.borrow_mut();
        let Some(e) = m.entries.get_mut(&self.key) else {
            return;
        };
        e.refs -= 1;
        if e.refs == 0 {
            let host = e.host;
            m.entries.remove(&self.key);
            let (ns, proto, port, _) = self.key;
            let now = Instant::now();
            m.by_host
                .retain(|_, (_, t)| t.is_none_or(|t| now.duration_since(t) < LINGER));
            if let Some(v) = m.by_host.get_mut(&(ns, proto, host))
                && v.0 == port
            {
                v.1 = Some(now);
            }
            m.released
                .retain(|_, (_, t)| now.duration_since(*t) < STICKY);
            m.released.insert(self.key, (host, now));
        }
    }
}

/// Binds the guest port `port` on the host: `bind` binds the given host port (0 lets the host
/// assign one) and returns the host errno; `bound_port` reads the port the host actually bound.
/// Returns the host port (`port` itself when the original port could be bound) and the lease, and
/// whether this mapping is new (new ones whose port changed write a kernel log line). Binding
/// the original port is registered too: binding the same port again, or just after release
/// (TIME_WAIT), still uses it, and conflicts are EADDRINUSE as usual rather than being taken as
/// the host refusing and moved to another port.
pub fn bind(
    map: &Rc<RefCell<PortMap>>,
    ns: usize,
    proto: i32,
    port: u16,
    mut bind: impl FnMut(u16) -> Result<(), i32>,
    bound_port: impl Fn() -> Option<u16>,
) -> Result<(u16, Lease, bool), Errno> {
    let key = (ns, proto, port, false);
    let existing = map.borrow().host_port(ns, proto, port, false).or_else(|| {
        // Just released: still the original host port (STICKY)
        let m = map.borrow();
        m.released
            .get(&key)
            .filter(|(_, t)| t.elapsed() < STICKY)
            .map(|&(h, _)| h)
    });
    let lease = |map: &Rc<RefCell<PortMap>>, host: u16| {
        let mut m = map.borrow_mut();
        m.released.remove(&key);
        m.entries.entry(key).or_insert(Entry { host, refs: 0 }).refs += 1;
        m.by_host.insert((ns, proto, host), (port, None));
        Lease {
            map: map.clone(),
            key,
        }
    };
    // A socket already binds this port, or it was just released: use the same host port, and a
    // conflict is the guest's own
    if let Some(host) = existing {
        bind(host).map_err(Errno)?;
        return Ok((host, lease(map, host), false));
    }
    let (offset, always_map) = {
        let m = map.borrow();
        (u32::from(m.offset), m.always_map)
    };
    let mut candidates = Vec::new();
    // When the host allows the original port, bind the original port
    if !always_map {
        candidates.push(port);
    }
    // The other protocol on the same port is already mapped: try its host port next
    let other = if proto == crate::net::IPPROTO_TCP {
        crate::net::IPPROTO_UDP
    } else {
        crate::net::IPPROTO_TCP
    };
    if let Some(h) = map.borrow().host_port(ns, other, port, false)
        && !candidates.contains(&h)
    {
        candidates.push(h);
    }
    for k in 1..=3u32 {
        let h = u32::from(port) + k * offset;
        if h <= u32::from(u16::MAX) && !candidates.contains(&(h as u16)) {
            candidates.push(h as u16);
        }
    }
    candidates.push(0);
    for h in candidates {
        match bind(h) {
            Ok(()) => {
                let host = if h == 0 {
                    bound_port().ok_or(Errno::EADDRINUSE)?
                } else {
                    h
                };
                return Ok((host, lease(map, host), true));
            }
            // Held by a program outside the guest (or the guest bound this high port directly):
            // try the next one
            Err(e) if e == Errno::EADDRINUSE.0 && h != 0 => {}
            // The host refuses the original port (on Android, not one of the opened few): try
            // another
            Err(e) if e == Errno::EACCES.0 && h == port => {}
            Err(e) => return Err(Errno(e)),
        }
    }
    Err(Errno::EADDRINUSE)
}

/// A port bound explicitly in a new network namespace ([`bind`] is for low ports in the boot
/// namespace): the host always picks another one (host-assigned), so this port on the host is not
/// taken, and the boot namespace and other apps on the phone can still bind it (when someone else
/// on the host binds the wildcard address, binding this namespace's loopback address would conflict
/// anyway). When a socket in the same namespace already binds this port, the same host port is
/// used, and a conflict is the guest's own (EADDRINUSE, `SO_REUSEPORT`). No [`STICKY`]: nobody
/// else connects to the host port, so just take another.
///
/// `v6only`: an AF_INET6 socket with `IPV6_V6ONLY` binds, which gets a host port of its own. The
/// host socket is dual-stack on the namespace's IPv4-mapped loopback address, so sharing a host
/// port with IPv4 binds of the same port would conflict, where Linux lets `0.0.0.0:P` and
/// `[::]:P` with `IPV6_V6ONLY` coexist (nginx's `listen 80; listen [::]:80`); connections pick
/// the binding by destination ([`PortMap::host_port`]).
pub fn bind_any(
    map: &Rc<RefCell<PortMap>>,
    ns: usize,
    proto: i32,
    port: u16,
    v6only: bool,
    mut bind: impl FnMut(u16) -> Result<(), i32>,
    bound_port: impl Fn() -> Option<u16>,
) -> Result<(u16, Lease), Errno> {
    let key = (ns, proto, port, v6only);
    let lease = |map: &Rc<RefCell<PortMap>>, host: u16| {
        let mut m = map.borrow_mut();
        m.entries.entry(key).or_insert(Entry { host, refs: 0 }).refs += 1;
        m.by_host.insert((ns, proto, host), (port, None));
        Lease {
            map: map.clone(),
            key,
        }
    };
    let existing = map.borrow().entries.get(&key).map(|e| e.host);
    if let Some(host) = existing {
        bind(host).map_err(Errno)?;
        return Ok((host, lease(map, host)));
    }
    bind(0).map_err(Errno)?;
    let host = bound_port().ok_or(Errno::EADDRINUSE)?;
    Ok((host, lease(map, host)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: i32 = crate::net::IPPROTO_TCP;
    const UDP: i32 = crate::net::IPPROTO_UDP;

    fn map() -> Rc<RefCell<PortMap>> {
        Rc::new(RefCell::new(PortMap::new()))
    }

    /// A host that refuses low ports (on Android, ports not among the opened few): those in `busy`
    /// are held by others.
    fn refusing(busy: &[u16]) -> impl FnMut(u16) -> Result<(), i32> + '_ {
        move |p| {
            if (1..1024).contains(&p) {
                Err(Errno::EACCES.0)
            } else if busy.contains(&p) {
                Err(Errno::EADDRINUSE.0)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn tries_the_offsets_then_lets_the_host_choose() {
        let m = map();
        let mut tried = Vec::new();
        let (h, _a, new) = bind(
            &m,
            0,
            TCP,
            22,
            |p| {
                tried.push(p);
                refusing(&[10022, 20022])(p)
            },
            || None,
        )
        .unwrap();
        assert_eq!((h, new), (30022, true));
        assert_eq!(tried, [22, 10022, 20022, 30022]);
        let (h, _b, _) = bind(&m, 0, TCP, 80, refusing(&[30080, 10080, 20080]), || {
            Some(45678)
        })
        .unwrap();
        assert_eq!(h, 45678);
        assert_eq!(m.borrow().guest_port(0, TCP, 45678), Some(80));
        // Other errors stop the search: when a moved-to port is also EACCES, no further move
        assert_eq!(
            bind(&m, 0, TCP, 23, |_| Err(Errno::EACCES.0), || None).err(),
            Some(Errno::EACCES)
        );
        assert_eq!(
            bind(&m, 0, TCP, 24, |_| Err(Errno::EADDRNOTAVAIL.0), || None).err(),
            Some(Errno::EADDRNOTAVAIL)
        );
    }

    /// The host allows the original port (opened, or running as root): bind the original, register
    /// it too, and use it again on rebinding and just after release; move only when another
    /// program holds it or only one protocol is opened.
    #[test]
    fn the_real_port_when_the_host_allows_it() {
        let m = map();
        let (h, a, new) = bind(&m, 0, TCP, 22, |_| Ok(()), || None).unwrap();
        assert_eq!((h, new), (22, true));
        assert_eq!(m.borrow().host_port(0, TCP, 22, false), Some(22));
        assert_eq!(m.borrow().guest_port(0, TCP, 22), Some(22));
        // A second socket on the same port: the guest's own conflict is reported as usual, no move
        assert_eq!(
            bind(&m, 0, TCP, 22, |_| Err(Errno::EADDRINUSE.0), || None).err(),
            Some(Errno::EADDRINUSE)
        );
        let (h, b, new) = bind(&m, 0, TCP, 22, |_| Ok(()), || None).unwrap();
        assert_eq!((h, new), (22, false));
        // After closing, the host port is in TIME_WAIT: EADDRINUSE as on Linux, not taken as held
        // by someone else
        drop((a, b));
        assert_eq!(
            bind(&m, 0, TCP, 22, |_| Err(Errno::EADDRINUSE.0), || None).err(),
            Some(Errno::EADDRINUSE)
        );
        // A port opened only for TCP: UDP may bind neither the original nor TCP's port (the same
        // one), so it moves to another
        let (_, _c, _) = bind(&m, 0, TCP, 22, |_| Ok(()), || None).unwrap();
        let mut tried = Vec::new();
        let (h, _d, _) = bind(
            &m,
            0,
            UDP,
            22,
            |p| {
                tried.push(p);
                refusing(&[])(p)
            },
            || None,
        )
        .unwrap();
        assert_eq!((h, tried), (10022, vec![22, 10022]));
        // Another app on the phone holds the original port: move to another
        let (h, _e, _) = bind(
            &m,
            0,
            TCP,
            80,
            |p| {
                if p == 80 {
                    Err(Errno::EADDRINUSE.0)
                } else {
                    Ok(())
                }
            },
            || None,
        )
        .unwrap();
        assert_eq!(h, 10080);
        // A refused low port (DNS's 53): UDP tries the original first, then the port TCP moved to
        let (_, _f, _) = bind(&m, 0, TCP, 53, refusing(&[10053]), || None).unwrap();
        let mut tried = Vec::new();
        let (h, _g, _) = bind(
            &m,
            0,
            UDP,
            53,
            |p| {
                tried.push(p);
                refusing(&[])(p)
            },
            || None,
        )
        .unwrap();
        assert_eq!((h, tried), (20053, vec![53, 20053]));
        // Always map: move even when the host allows the port
        m.borrow_mut().always_map = true;
        let (h, _h, _) = bind(&m, 0, TCP, 443, |_| Ok(()), || None).unwrap();
        assert_eq!(h, 10443);
    }

    /// Ports in a new namespace: the host assigns them; binding again in the same namespace uses
    /// the same host port (conflicts left to the host), the same port in another namespace gets
    /// its own, and binding after full release need not reuse the original.
    #[test]
    fn namespace_ports_let_the_host_choose() {
        let m = map();
        let mut tried = Vec::new();
        let (h, a) = bind_any(
            &m,
            7,
            TCP,
            8080,
            false,
            |p| {
                tried.push(p);
                Ok(())
            },
            || Some(41000),
        )
        .unwrap();
        assert_eq!((h, tried), (41000, vec![0]));
        assert_eq!(m.borrow().host_port(7, TCP, 8080, false), Some(41000));
        assert_eq!(m.borrow().guest_port(7, TCP, 41000), Some(8080));
        // Same namespace: the same host port, and when the host reports a conflict it is
        // EADDRINUSE
        let r = bind_any(
            &m,
            7,
            TCP,
            8080,
            false,
            |p| {
                assert_eq!(p, 41000);
                Err(Errno::EADDRINUSE.0)
            },
            || None,
        );
        assert_eq!(r.err(), Some(Errno::EADDRINUSE));
        let (h2, b) = bind_any(&m, 7, TCP, 8080, false, |_| Ok(()), || None).unwrap();
        assert_eq!(h2, 41000);
        // The same port in another namespace and in the boot namespace are unrelated
        let (h3, _c) = bind_any(
            &m,
            9,
            TCP,
            8080,
            false,
            |p| {
                assert_eq!(p, 0);
                Ok(())
            },
            || Some(42000),
        )
        .unwrap();
        assert_eq!(h3, 42000);
        assert_eq!(m.borrow().host_port(0, TCP, 8080, false), None);
        // After the last lease is returned the registration is gone, and binding again assigns
        // anew
        drop((a, b));
        assert_eq!(m.borrow().host_port(7, TCP, 8080, false), None);
        assert_eq!(m.borrow().guest_port(7, TCP, 41000), Some(8080));
        let (h4, _d) = bind_any(
            &m,
            7,
            TCP,
            8080,
            false,
            |p| {
                assert_eq!(p, 0);
                Ok(())
            },
            || Some(43000),
        )
        .unwrap();
        assert_eq!(h4, 43000);
        // The host gave the just released port to another mapping: switch to the new one
        let (_, _e) = bind_any(&m, 7, TCP, 9090, false, |_| Ok(()), || Some(41000)).unwrap();
        assert_eq!(m.borrow().guest_port(7, TCP, 41000), Some(9090));
    }

    /// In a new namespace an AF_INET6 socket with `IPV6_V6ONLY` gets a host port of its own, so it
    /// can bind next to an IPv4 socket on the same port (nginx's `listen 80; listen [::]:80`);
    /// IPv6 destinations go to it first, IPv4 ones never do, and a dual-stack binding still takes
    /// IPv6 when there is no IPv6-only one.
    #[test]
    fn ipv6_only_binds_get_their_own_host_port() {
        let m = map();
        let (v4, _a) = bind_any(&m, 7, TCP, 80, false, |_| Ok(()), || Some(41000)).unwrap();
        let (v6, b) = bind_any(
            &m,
            7,
            TCP,
            80,
            true,
            |p| {
                assert_eq!(p, 0, "a new host port, not the IPv4 binding's");
                Ok(())
            },
            || Some(42000),
        )
        .unwrap();
        assert_eq!((v4, v6), (41000, 42000));
        assert_eq!(m.borrow().host_port(7, TCP, 80, false), Some(41000));
        assert_eq!(m.borrow().host_port(7, TCP, 80, true), Some(42000));
        assert_eq!(m.borrow().guest_port(7, TCP, 42000), Some(80));
        drop(b);
        assert_eq!(m.borrow().host_port(7, TCP, 80, true), Some(41000));
        let (_, _c) = bind_any(&m, 7, UDP, 53, true, |_| Ok(()), || Some(43000)).unwrap();
        assert_eq!(m.borrow().host_port(7, UDP, 53, false), None);
    }

    #[test]
    fn a_bound_port_keeps_its_host_port_until_the_last_lease() {
        let m = map();
        m.borrow_mut().always_map = true;
        let (h, a, _) = bind(&m, 0, TCP, 22, |_| Ok(()), || None).unwrap();
        assert_eq!(h, 10022);
        // A second socket (SO_REUSEPORT, the other address family) uses the same host port, and
        // conflicts are reported as usual
        let (h2, b, new) = bind(
            &m,
            0,
            TCP,
            22,
            |p| if p == 10022 { Ok(()) } else { Err(1) },
            || None,
        )
        .unwrap();
        assert_eq!((h2, new), (10022, false));
        assert_eq!(
            bind(&m, 0, TCP, 22, |_| Err(Errno::EADDRINUSE.0), || None).err(),
            Some(Errno::EADDRINUSE)
        );
        drop(a);
        assert_eq!(m.borrow().host_port(0, TCP, 22, false), Some(10022));
        drop(b);
        assert_eq!(m.borrow().host_port(0, TCP, 22, false), None);
        // The host port still translates back to the guest port for a while (answers the other
        // side has not read yet)
        assert_eq!(m.borrow().guest_port(0, TCP, 10022), Some(22));
        m.borrow_mut().by_host.get_mut(&(0, TCP, 10022)).unwrap().1 = Some(Instant::now() - LINGER);
        assert_eq!(m.borrow().guest_port(0, TCP, 10022), None);
        // Rebinding a just released port still uses the original host port; when it is held
        // (TIME_WAIT) it is EADDRINUSE, no move
        assert_eq!(
            bind(&m, 0, TCP, 22, |_| Err(Errno::EADDRINUSE.0), || None).err(),
            Some(Errno::EADDRINUSE)
        );
        let (h, again, _) = bind(
            &m,
            0,
            TCP,
            22,
            |p| if p == 10022 { Ok(()) } else { Err(1) },
            || None,
        )
        .unwrap();
        assert_eq!(h, 10022);
        drop(again);
        // Namespaces and protocols are independent; UDP tries TCP's host port first
        let (_, _c, _) = bind(
            &m,
            0,
            TCP,
            53,
            |p| {
                if p == 10053 {
                    Err(Errno::EADDRINUSE.0)
                } else {
                    Ok(())
                }
            },
            || None,
        )
        .unwrap();
        let (h, _d, _) = bind(&m, 0, UDP, 53, |_| Ok(()), || None).unwrap();
        assert_eq!(h, 20053);
        let (h, _e, _) = bind(&m, 1, TCP, 53, |_| Ok(()), || None).unwrap();
        assert_eq!(h, 10053);
    }

    #[test]
    fn listens_are_reported_once() {
        let mut m = PortMap::new();
        m.listened("sshd".into(), 22, 10022);
        m.listened("sshd".into(), 22, 10022);
        m.listened("sshd".into(), 22, 20022);
        let e = m.take_listens();
        assert_eq!(e.len(), 2);
        assert_eq!(e[1].host_port, 20022);
        assert!(m.take_listens().is_empty());
        // A listen the full queue dropped is reported the next time
        for p in 0..MAX_EVENTS as u16 {
            m.listened("x".into(), 1000 + p, 1000 + p);
        }
        m.listened("late".into(), 80, 10080);
        assert_eq!(m.take_listens().len(), MAX_EVENTS);
        m.listened("late".into(), 80, 10080);
        assert_eq!(m.take_listens()[0].comm, "late");
        assert!(m.maps(22) && !m.maps(0) && !m.maps(1024));
        m.offset = 0;
        assert!(!m.maps(22));
    }
}
