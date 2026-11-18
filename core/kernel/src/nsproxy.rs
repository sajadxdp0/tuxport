//! The UTS, IPC, network, cgroup and time namespaces a thread is in (the kernel/ns map; these kinds
//! of Linux's `struct nsproxy`; the mount namespace is in `FsContext`, and for the pid namespace
//! see [`crate::pidns`]). As on Linux they are recorded per thread: unshare and setns change only
//! the calling thread, and threads and processes created by clone keep the parent thread's.
//!
//! A UTS namespace has its own host name and NIS domain name. An IPC namespace has its own System
//! V IPC objects, POSIX message queues and their limits; the contents are stored by number in
//! [`crate::kernel::Kernel::ipcs`] and dropped once no thread uses the namespace and no namespace
//! file keeps it ([`NsRef`]). For network namespaces see [`NetNs`].

use std::cell::Cell;
use std::rc::Rc;

use crate::cell::RefCell;
use crate::cred::UserNs;

/// The number of the boot UTS namespace (`/proc/<pid>/ns/uts`).
pub(crate) const INIT_UTS_NS: u64 = 4_026_531_838;
/// The number of the boot IPC namespace.
pub(crate) const INIT_IPC_NS: u64 = 4_026_531_839;
/// The number of the boot network namespace.
pub(crate) const INIT_NET_NS: u64 = 4_026_531_840;
/// The number of the boot cgroup namespace.
pub(crate) const INIT_CGROUP_NS: u64 = 4_026_531_835;
/// The number of the boot time namespace.
pub(crate) const INIT_TIME_NS: u64 = 4_026_531_834;

/// A time namespace (Linux's `struct time_namespace`, since 5.6): processes inside see
/// CLOCK_MONOTONIC (including RAW and COARSE) and CLOCK_BOOTTIME (including ALARM) plus their
/// offsets; absolute-time waits and timers subtract the offsets instead; `/proc/uptime`, sysinfo
/// and process start times use the boottime offset. unshare(`CLONE_NEWTIME`) creates only the one
/// for children (copying the old offsets), which children forked afterwards enter; a thread
/// entering by setns switches both. Offsets are set through `/proc/<pid>/timens_offsets`, and can
/// no longer be changed once a process has entered.
#[derive(Debug)]
pub(crate) struct TimeNs {
    pub id: u64,
    /// The user namespace it was created in: `CAP_SYS_TIME` here is needed to set the offsets, and
    /// `CAP_SYS_ADMIN` to enter it by setns.
    pub owner: Rc<UserNs>,
    /// Offsets of CLOCK_MONOTONIC and CLOCK_BOOTTIME, each (seconds, nanoseconds), with
    /// nanoseconds in [0, 10⁹).
    pub offsets: Cell<[(i64, i64); 2]>,
    /// A process has entered, so the offsets can no longer change (Linux's `frozen_offsets`).
    /// Always so for the boot namespace.
    pub frozen: Cell<bool>,
}

impl TimeNs {
    /// The boot time namespace (one per kernel).
    pub fn init() -> Rc<TimeNs> {
        Rc::new(TimeNs {
            id: INIT_TIME_NS,
            owner: UserNs::init(),
            offsets: Cell::new([(0, 0); 2]),
            frozen: Cell::new(true),
        })
    }

    /// Creates a time namespace for children, owned by user namespace `owner`, copying the offsets
    /// of `self` (Linux's `clone_time_ns`).
    pub fn copy(&self, owner: Rc<UserNs>) -> Rc<TimeNs> {
        Rc::new(TimeNs {
            id: crate::vfs::new_ns_id(),
            owner,
            offsets: Cell::new(self.offsets.get()),
            frozen: Cell::new(false),
        })
    }

    /// The offset of clock `clock` (nanoseconds): only the monotonic clocks and boottime have one.
    pub fn offset(&self, clock: i32) -> i64 {
        let [mono, boot] = self.offsets_ns();
        match clock {
            // CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW, CLOCK_MONOTONIC_COARSE
            1 | 4 | 6 => mono,
            // CLOCK_BOOTTIME, CLOCK_BOOTTIME_ALARM
            7 | 9 => boot,
            _ => 0,
        }
    }

    /// The two offsets (nanoseconds): the monotonic clock's and boottime's.
    pub fn offsets_ns(&self) -> [i64; 2] {
        self.offsets
            .get()
            .map(|(sec, nsec)| sec.saturating_mul(1_000_000_000).saturating_add(nsec))
    }
}

/// A UTS namespace.
#[derive(Debug)]
pub(crate) struct UtsNs {
    pub id: u64,
    /// The user namespace it was created in: `CAP_SYS_ADMIN` here is needed to change the names
    /// and to enter it by setns.
    pub owner: Rc<UserNs>,
    /// Host name and NIS domain name (uname, sethostname, `/proc/sys/kernel/hostname`), at most
    /// [`crate::kernel::UTS_LEN`] bytes.
    pub hostname: RefCell<Vec<u8>>,
    pub domainname: RefCell<Vec<u8>>,
}

impl UtsNs {
    /// Copies the names, owned by user namespace `owner` (Linux's `copy_utsname`).
    pub fn copy(&self, owner: Rc<UserNs>) -> Rc<UtsNs> {
        Rc::new(UtsNs {
            id: crate::vfs::new_ns_id(),
            owner,
            hostname: RefCell::new(self.hostname.borrow().clone()),
            domainname: RefCell::new(self.domainname.borrow().clone()),
        })
    }
}

/// An IPC namespace. Its objects are in [`crate::kernel::Kernel::ipcs`].
#[derive(Debug)]
pub(crate) struct IpcNs {
    pub id: u64,
    /// The user namespace it was created in (for permission checks).
    pub owner: Rc<UserNs>,
}

/// A cgroup namespace: paths in `/proc/<pid>/cgroup` and the root of cgroup2 mounted inside are
/// relative to its root cgroup ([`crate::cgroup`]).
#[derive(Debug)]
pub(crate) struct CgroupNs {
    pub id: u64,
    /// Root: the cgroup its creating process was in at the time (a [`crate::cgroup`] number).
    pub root: u64,
    /// The user namespace it was created in (for setns and cgroup2 mount permission checks).
    pub owner: Rc<UserNs>,
}

/// A network namespace. The boot one is the host's network (sockets use the host's directly). A
/// new one starts with only the loopback interface lo: sockets are still host sockets, with
/// addresses changed to the host loopback range 127.a.b.0/24 given to this namespace
/// ([`crate::netns`]), and no other address can be reached. Both kinds can have virtual devices
/// ([`crate::netdev`]).
#[derive(Debug)]
pub(crate) struct NetNs {
    pub id: u64,
    /// The user namespace it was created in: `CAP_NET_ADMIN` here is needed to configure lo.
    pub owner: Rc<UserNs>,
    /// a and b of the host loopback range 127.a.b.0/24; None for the boot namespace.
    pub host_net: Option<[u8; 2]>,
    /// The state of lo.
    pub lo: RefCell<Loopback>,
    /// Virtual devices, routes through them, and sysctls ([`crate::netdev`]).
    pub devs: RefCell<crate::netdev::Devices>,
    /// The nftables rule set ([`crate::nft`]).
    pub nft: RefCell<crate::nft::Nft>,
    /// TCP sockets listening and UDP sockets bound in a new network namespace
    /// ([`crate::netns::listening`]).
    pub listeners: RefCell<Vec<Listener>>,
}

/// A TCP socket listening or a UDP socket bound in a new network namespace: on the host they are
/// all on this namespace's loopback addresses, and incoming connections and datagrams are checked
/// against these for whether anyone is there and whether the bound address matches (on Linux TCP
/// gets ECONNREFUSED and UDP is dropped).
#[derive(Debug, Clone)]
pub(crate) struct Listener {
    /// The number used to remove it ([`crate::net::InetSocket::listener`]).
    pub id: u64,
    /// The guest port.
    pub port: u16,
    /// The bound guest address (4 or 16 bytes); None is the wildcard address.
    pub addr: Option<Vec<u8>>,
    /// An AF_INET6 socket, and its `IPV6_V6ONLY`.
    pub v6: bool,
    pub v6only: bool,
    /// UDP (bound), otherwise TCP (listening).
    pub udp: bool,
}

/// The state of lo in a new network namespace: initially down and without addresses (Linux's
/// `loopback_net_init`).
#[derive(Debug, Default, Clone)]
pub(crate) struct Loopback {
    /// `IFF_UP`.
    pub up: bool,
    /// IPv4 addresses and prefix lengths.
    pub v4: Vec<([u8; 4], u8)>,
    /// Has ::1 (added automatically when lo comes up, Linux's addrconf).
    pub v6: bool,
    /// The MTU set (`SIOCSIFMTU`), 65536 by default.
    pub mtu: Option<u32>,
    /// UDP ports bound on the host at 127.a.b.254: sockets sending to their own virtual addresses
    /// are bound there ([`crate::netns`]), and datagrams sent to these ports of the virtual
    /// addresses (replies to them) must be delivered there.
    pub self_udp: Vec<u16>,
}

/// A namespace (Linux's `struct ns_common`). Open namespace files and namespace files bind-mounted
/// elsewhere (nsfs) hold it: the namespace survives after every process in it has exited and can
/// still be entered by setns (that is how `ip netns add` keeps a network namespace).
#[derive(Clone, Debug)]
pub(crate) enum NsRef {
    Mnt(Rc<crate::vfs::MntNs>),
    User(Rc<UserNs>),
    /// Both the `pid` and the `pid_for_children` files are this.
    Pid(Rc<crate::pidns::PidNs>),
    Uts(Rc<UtsNs>),
    Ipc(Rc<IpcNs>),
    Net(Rc<NetNs>),
    Cgroup(Rc<CgroupNs>),
    /// Both the `time` and the `time_for_children` files are this.
    Time(Rc<TimeNs>),
}

impl NsRef {
    /// The number (the inode number of the namespace file).
    pub fn id(&self) -> u64 {
        match self {
            NsRef::Mnt(n) => n.id,
            NsRef::User(n) => n.id,
            NsRef::Pid(n) => n.id,
            NsRef::Uts(n) => n.id,
            NsRef::Ipc(n) => n.id,
            NsRef::Net(n) => n.id,
            NsRef::Cgroup(n) => n.id,
            NsRef::Time(n) => n.id,
        }
    }

    /// The owner (Linux's `ns->ops->owner`): the user namespace it was created in; for a user
    /// namespace, its parent; the boot user namespace has none.
    pub fn owner(&self) -> Option<Rc<UserNs>> {
        Some(match self {
            NsRef::Mnt(n) => n.owner.clone(),
            NsRef::User(n) => return n.parent.clone(),
            NsRef::Pid(n) => n.owner.clone(),
            NsRef::Uts(n) => n.owner.clone(),
            NsRef::Ipc(n) => n.owner.clone(),
            NsRef::Net(n) => n.owner.clone(),
            NsRef::Cgroup(n) => n.owner.clone(),
            NsRef::Time(n) => n.owner.clone(),
        })
    }
}

/// A thread's UTS, IPC, network, cgroup and time namespaces.
#[derive(Clone, Debug)]
pub(crate) struct NsProxy {
    pub uts: Rc<UtsNs>,
    pub ipc: Rc<IpcNs>,
    pub net: Rc<NetNs>,
    pub cgroup: Rc<CgroupNs>,
    /// The time namespace it is in, and the one children created later enter (different after an
    /// unshare of `CLONE_NEWTIME`).
    pub time: Rc<TimeNs>,
    pub time_for_children: Rc<TimeNs>,
}

impl NsProxy {
    /// The boot namespaces (one per kernel, shared by all processes from boot).
    pub fn init() -> NsProxy {
        let time = TimeNs::init();
        NsProxy {
            uts: Rc::new(UtsNs {
                id: INIT_UTS_NS,
                owner: UserNs::init(),
                hostname: RefCell::new(b"localhost".to_vec()),
                domainname: RefCell::new(b"(none)".to_vec()),
            }),
            ipc: Rc::new(IpcNs {
                id: INIT_IPC_NS,
                owner: UserNs::init(),
            }),
            net: Rc::new(NetNs {
                id: INIT_NET_NS,
                owner: UserNs::init(),
                host_net: None,
                lo: RefCell::default(),
                devs: RefCell::default(),
                nft: Default::default(),
                listeners: Default::default(),
            }),
            cgroup: Rc::new(CgroupNs {
                id: INIT_CGROUP_NS,
                root: crate::cgroup::ROOT,
                owner: UserNs::init(),
            }),
            time: time.clone(),
            time_for_children: time,
        }
    }
}

/// The kinds of namespaces whose number `/proc/sys/user/max_*_namespaces` limits (Linux's
/// `UCOUNT_*_NAMESPACES`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Counted {
    User,
    Pid,
    Uts,
    Ipc,
    Net,
    Mnt,
    Cgroup,
    Time,
}

impl Counted {
    /// The kind `/proc/sys/user/<name>` limits.
    pub(crate) fn from_sysctl(name: &[u8]) -> Option<Counted> {
        Some(match name {
            b"max_user_namespaces" => Counted::User,
            b"max_pid_namespaces" => Counted::Pid,
            b"max_uts_namespaces" => Counted::Uts,
            b"max_ipc_namespaces" => Counted::Ipc,
            b"max_net_namespaces" => Counted::Net,
            b"max_mnt_namespaces" => Counted::Mnt,
            b"max_cgroup_namespaces" => Counted::Cgroup,
            b"max_time_namespaces" => Counted::Time,
            _ => return None,
        })
    }
}

/// How many namespaces each user has created, per user namespace (Linux's ucounts and
/// `inc_ucount`): a namespace of some kind created by effective user u in user namespace n (for a
/// new user namespace, n is its parent) counts for (n, u), then for (n's parent, n's owner), and
/// so on up to the initial user namespace, and each count must stay below that user namespace's
/// limit for the kind (`/proc/sys/user/max_*_namespaces`, kept per user namespace: half of
/// threads-max in the initial one, `INT_MAX` in the others), else creating it is ENOSPC. So a limit
/// written in a user namespace bounds everything created below it, which is how bubblewrap's
/// `--disable-userns` works. A namespace counts while it is alive; those gone are taken off at the
/// next check.
#[derive(Default)]
pub(crate) struct Ucounts {
    live: Vec<Charged>,
    counts: std::collections::HashMap<(u64, u32), [u32; 8]>,
    max: std::collections::HashMap<u64, [Option<u32>; 8]>,
}

/// A namespace counted in [`Ucounts`].
struct Charged {
    ns: std::rc::Weak<dyn std::any::Any>,
    kind: Counted,
    /// The (user namespace, user) pairs it counts for.
    keys: Vec<(u64, u32)>,
    /// A user namespace's own number: its limits go with it.
    user_ns: Option<u64>,
}

impl Ucounts {
    /// The limit on namespaces of `kind` in user namespace `ns`; `init` is the initial user
    /// namespace's default.
    pub(crate) fn limit(&self, ns: u64, kind: Counted, init: u32) -> u32 {
        let default = if ns == crate::cred::INIT_USER_NS {
            init
        } else {
            i32::MAX as u32
        };
        self.max
            .get(&ns)
            .and_then(|m| m[kind as usize])
            .unwrap_or(default)
    }

    pub(crate) fn set_limit(&mut self, ns: u64, kind: Counted, n: u32) {
        self.max.entry(ns).or_default()[kind as usize] = Some(n);
    }

    /// Whether one more namespace of `kind` created by effective user `euid` in user namespace
    /// `owner` stays within every limit: ENOSPC if not.
    pub(crate) fn check(
        &mut self,
        kind: Counted,
        owner: &UserNs,
        euid: u32,
        init: u32,
    ) -> Result<(), crate::errno::Errno> {
        self.purge();
        for key in keys(owner, euid) {
            let n = self.counts.get(&key).map_or(0, |c| c[kind as usize]);
            if n >= self.limit(key.0, kind, init) {
                return Err(crate::errno::Errno::ENOSPC);
            }
        }
        Ok(())
    }

    /// Counts namespace `ns`, created after [`Ucounts::check`] allowed it, until it goes away.
    pub(crate) fn add(
        &mut self,
        kind: Counted,
        owner: &UserNs,
        euid: u32,
        ns: Rc<dyn std::any::Any>,
        user_ns: Option<u64>,
    ) {
        let keys = keys(owner, euid);
        for key in &keys {
            self.counts.entry(*key).or_default()[kind as usize] += 1;
        }
        self.live.push(Charged {
            ns: Rc::downgrade(&ns),
            kind,
            keys,
            user_ns,
        });
    }

    /// Takes the namespaces that have gone off the counts.
    fn purge(&mut self) {
        let (dead, live): (Vec<Charged>, Vec<Charged>) = std::mem::take(&mut self.live)
            .into_iter()
            .partition(|c| c.ns.strong_count() == 0);
        self.live = live;
        for c in dead {
            for key in &c.keys {
                if let Some(n) = self.counts.get_mut(key) {
                    n[c.kind as usize] -= 1;
                    if n.iter().all(|&x| x == 0) {
                        self.counts.remove(key);
                    }
                }
            }
            if let Some(id) = c.user_ns {
                self.max.remove(&id);
            }
        }
    }
}

/// The initial user namespace's default limit on namespaces of each kind: half of threads-max
/// (Linux's fork_init).
pub(crate) fn default_ns_limit(threads_max: usize) -> u32 {
    (threads_max / 2).min(i32::MAX as usize) as u32
}

/// The (user namespace, user) pairs a namespace created by `euid` in user namespace `owner`
/// counts for: its own, then each user namespace above with the owner of the one below.
fn keys(owner: &UserNs, euid: u32) -> Vec<(u64, u32)> {
    let mut out = vec![(owner.id, euid)];
    let mut ns = owner;
    while let Some(parent) = &ns.parent {
        out.push((parent.id, ns.owner));
        ns = parent;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Offsets are per clock: the three monotonic clocks and the two boottime clocks, none for the
    /// others; a new namespace copies them and is not frozen.
    #[test]
    fn time_namespace_offsets() {
        let init = TimeNs::init();
        assert!(init.frozen.get());
        let ns = init.copy(UserNs::init());
        assert_ne!(ns.id, init.id);
        assert!(!ns.frozen.get());
        ns.offsets.set([(86_400, 0), (-1, 500_000_000)]);
        for clock in [1, 4, 6] {
            assert_eq!(ns.offset(clock), 86_400_000_000_000);
        }
        for clock in [7, 9] {
            assert_eq!(ns.offset(clock), -500_000_000);
        }
        for clock in [0, 2, 3, 5, 8, 11] {
            assert_eq!(ns.offset(clock), 0);
        }
        assert_eq!(ns.copy(UserNs::init()).offsets.get(), ns.offsets.get());
    }
}
