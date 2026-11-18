//! The kernel layer's global state, the big kernel lock and the unified wait mechanism (the
//! kernel/process map).
//!
//! All guest threads share one [`Kernel`], protected by one big kernel lock (BKL): held while
//! handling syscalls and delivering signals, not held while running guest code, and released
//! before waiting. Kernel objects are shared among themselves through `Rc`/`RefCell` and are
//! accessed only while holding the BKL; the only exception is a thread's own [`TaskState`], which
//! only the thread it belongs to accesses, and which stays borrowed even while waiting. Neither
//! `Rc` counts nor `RefCell` borrow flags are atomic, so without the BKL not even clone or drop is
//! allowed: the only things that release the BKL in the middle of a syscall are the waits
//! ([`Cx::wait_on`] and the like) and [`Cx::unlocked`], whose closure must be `Send` and so cannot
//! touch kernel objects; on a panic in either place the BKL is also taken again first, so that
//! the `Rc`s dropped by the outer frames during unwinding do not change counts at the same time
//! as other threads. Kernel objects must not still be borrowed when the BKL is released either;
//! debug builds and builds with the `borrow-check` feature check this through [`crate::cell`].
//!
//! Every wait goes through the host `poll()`: each thread has an eventfd, and while waiting polls
//! it together with the host fds it waits on. Every change that might make someone else's wait
//! condition true requests a wake-up ([`request_wake_on`], [`request_wake_object`],
//! [`request_wake`]), and when the BKL is released the matching waiting threads are woken and each
//! re-checks its own condition. Changes are distinguished by channel ([`wake`]) or by object, and
//! only the waiters interested in that channel or object are woken: changes such as closing files
//! or reading and writing pipes are very frequent, and need not wake the shell waiting for
//! children in wait4 and the timer thread every time. Futexes are the exception: many threads park
//! on locks and condition variables; they wait with [`Cx::wait_alone`] and are notified directly,
//! only by FUTEX_WAKE, signals and the ending of the thread group.

use std::collections::{BTreeMap, HashMap};
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use tuxport_engine::DbtStats;
use tuxport_hal::Event;

use crate::cell::{Ref, RefCell, RefMut};
use crate::errno::Errno;
use crate::fd::{FdTable, FileRef};
use crate::futex::Futexes;
use crate::mm::Mm;
use crate::process::{CpuTime, ExitStatus, ThreadGroup, Tier};
use crate::task::{Task, TaskState};
use crate::tty::Tty;
use crate::uaccess::UserSlice;
use crate::vfs::store::Ino;
use crate::vfs::{Node, Vfs};

/// Thread ID; the thread group ID (pid) is the tid of the group's first thread.
pub(crate) type Tid = i32;

/// The kernel layer's built-in init (the kernel/boot map): it only adopts orphans and reaps
/// zombies.
pub(crate) const INIT: Tid = 1;

/// The largest tid, the same as the upper bound of `pid_max` on 64-bit Linux.
pub(crate) const PID_MAX: Tid = 4 << 20;

/// Maximum length of the host name and domain name (`__NEW_UTS_LEN`).
pub(crate) const UTS_LEN: usize = 64;

/// Wake-up channels: waiters register which ones they are interested in ([`Cx::wait_on`]), and a
/// change wakes only the waiters interested in it.
pub(crate) mod wake {
    /// Changes on open files (pipes, sockets, PTYs, message queues...). These changes are all
    /// requested per object ([`super::request_wake_object`]): a waiter listening only on this
    /// channel is woken by changes on the objects it registered (one that registered no objects,
    /// by changes on any object) and by changes that request all channels.
    pub const FILE: u32 = 1;
    /// Everything else: setting timers, signals and so on.
    pub const OTHER: u32 = 2;
    /// Child status changes (exit, stop, continue): wait4 and waitid listen only to this.
    pub const CHILD: u32 = 4;
    /// A process the host is waiting for (started by [`crate::Machine::spawn`]) has ended:
    /// `Machine::wait` listens only to this.
    pub const WATCHED: u32 = 8;
    /// A thread has ended (execve waits for the other threads to end, shutdown waits for all
    /// threads to end).
    pub const EXITED: u32 = 16;
    /// File locks (flock, fcntl record locks and OFD locks), leases and open-file position locks
    /// (f_pos_lock) were released or changed: only the threads waiting for them listen. These
    /// used to go through the file channel, and every unlock woke every thread parked in poll or
    /// epoll.
    pub const LOCK: u32 = 32;
    /// A blocked signal became pending (one sent to the process and blocked by every thread, or
    /// one sent to a thread and blocked by it): only the threads waiting for them listen
    /// (sigtimedwait, signalfd reads, poll and epoll watching a signalfd). This used to wake every
    /// waiter: musl's fork blocks all signals around clone, and a child exiting at that moment has
    /// its SIGCHLD blocked by every thread, which a shell setting up a pipeline often runs into.
    pub const SIGNAL: u32 = 64;
    pub const ALL: u32 = FILE | OTHER | CHILD | WATCHED | EXITED | LOCK | SIGNAL;
    /// There are changes that affect only some objects ([`super::request_wake_object`]); not a
    /// channel waiters are interested in.
    pub(super) const OBJECTS: u32 = 1 << 31;

    /// An object a waiter waits on (pipe, eventfd, PTY, AF_UNIX channel...): the object's address.
    pub type Object = usize;

    /// The wake object of object `c` in a `RefCell`: the address of the value inside, the same as
    /// [`of_value`] inside the object's own methods.
    pub fn of<T>(c: &crate::cell::RefCell<T>) -> Object {
        c.as_ptr() as Object
    }

    /// Inside the object's own methods (with only `&self`): the same address as [`of`].
    pub fn of_value<T>(v: &T) -> Object {
        v as *const T as Object
    }
}

/// Wake-ups requested without the BKL (there should be none; kept as a fallback): handed to the
/// next thread that releases any BKL. Normal requests are recorded per thread ([`PENDING`]).
static WAKE: AtomicU32 = AtomicU32::new(0);

/// Objects requested to be woken without the BKL (the fallback of [`WAKE`]).
static WAKE_OBJECTS: Mutex<Vec<wake::Object>> = Mutex::new(Vec::new());

thread_local! {
    /// Events to signal, recorded while holding the BKL ([`signal_later`]).
    #[allow(clippy::missing_const_for_thread_local)] // already const; Android clippy misfires
    static SIGNALS: std::cell::RefCell<Vec<Arc<Event>>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The BKL this thread holds ([`Shared::id`]), 0 meaning none.
    #[allow(clippy::missing_const_for_thread_local)] // already const; Android clippy misfires
    static HELD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// What the guest thread on this host thread entered the kernel for ([`doing`]), copied into
    /// [`Hold`] each time this thread takes a BKL.
    #[allow(clippy::missing_const_for_thread_local)] // already const; Android clippy misfires
    static DOING: std::cell::Cell<Entry> = const { std::cell::Cell::new(Entry::NONE) };
    /// This host thread's tid, read at its first BKL ([`Hold::host_tid`]).
    #[allow(clippy::missing_const_for_thread_local)] // already const; Android clippy misfires
    static HOST_TID: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    /// Wake-ups requested while this thread holds the BKL: channels ([`wake`]) and objects,
    /// processed by the same thread when it releases the BKL ([`Kernel::flush_wakeups`]).
    /// Recorded per thread rather than globally: one host process can contain several kernels
    /// (tests, the app switching kernels, [`crate::machine::store_changed`] taking another
    /// kernel's BKL when the host changed the store), each with its own BKL and waiters; kept
    /// globally, the requests would be taken and cleared when another kernel releases its lock,
    /// and the waiters here would sleep forever.
    #[allow(clippy::missing_const_for_thread_local)] // already const; Android clippy misfires
    static PENDING: std::cell::RefCell<(u32, Vec<wake::Object>)> =
        const { std::cell::RefCell::new((0, Vec::new())) };
}

// ---- Global state ----

/// A waiting thread: its event, the channels it is interested in ([`wake`]) and the objects it
/// waits on (sorted; with None, a change on any object wakes it). One epoll may watch thousands
/// of files, and the list is searched every time an object changes, hence binary search.
type Waiter = (Arc<Event>, u32, Option<Box<[wake::Object]>>);
/// Pid 1 of a pid namespace waiting for the processes in the namespace to be reaped: the
/// namespace, the process, and the process groups that may become orphaned.
type HeldReaper = (Rc<crate::pidns::PidNs>, Rc<RefCell<ThreadGroup>>, Vec<Tid>);

/// An open file holding a flock(2) lock (a weak reference; the lock is released with it when it is
/// closed), whether the lock is exclusive, and the process that took it (`/proc/locks`).
pub(crate) type FlockHolder = (std::rc::Weak<RefCell<crate::fd::OpenFile>>, bool, Tid);

/// The kernel layer's global state.
pub(crate) struct Kernel {
    /// All live threads.
    pub tasks: BTreeMap<Tid, Rc<Task>>,
    /// All thread groups, including zombies and init.
    pub groups: BTreeMap<Tid, Rc<RefCell<ThreadGroup>>>,
    /// For threads and processes in a new pid namespace, the namespace they are in
    /// ([`crate::pidns`]); removed from each level's tables when the number is no longer used.
    pid_nss: std::collections::HashMap<Tid, Rc<crate::pidns::PidNs>>,
    /// Pid 1 processes of pid namespaces that have exited and are waiting for the other processes
    /// in the namespace to be reaped.
    held_reapers: Vec<HeldReaper>,
    /// Execution engine.
    pub tier: Tier,
    /// Processes whose end the host is waiting for (started by [`crate::Machine::spawn`]): how each
    /// ended is recorded once it ends.
    pub watched: BTreeMap<Tid, Option<ExitStatus>>,
    /// Root filesystem; without it the guest sees no filesystem at all.
    pub vfs: Option<Rc<crate::vfs::Vfs>>,
    /// The Machine is shutting down: the timer thread exits.
    pub shutting_down: bool,
    /// The sum of exited threads' Tier 1 statistics.
    pub dbt_stats: Option<DbtStats>,
    /// The total CPU time used by exited threads (`/proc/stat`).
    pub cpu: CpuTime,
    /// The number of tasks created, threads included (processes in `/proc/stat`).
    pub forks: u64,
    /// `/proc/sys/kernel/random/boot_id` of this run.
    pub boot_id: [u8; 16],
    /// All PTYs, by their devpts instance (device number) and their number within the instance; an
    /// entry becomes stale once both master and slave are closed.
    pub ptys: BTreeMap<(u64, u32), Weak<RefCell<Tty>>>,
    /// PTYs whose master the host holds ([`crate::terminal`]), by PTY number.
    pub consoles: BTreeMap<u32, crate::terminal::Console>,
    /// flock(2) locks: for each inode, the open files holding a lock and whether it is exclusive.
    /// Once an open file is closed its weak reference dies and the lock is released with it.
    pub flocks: BTreeMap<Ino, Vec<FlockHolder>>,
    /// fcntl(2) record locks.
    pub locks: crate::locks::Locks,
    /// The boot-time UTS, IPC, network, cgroup and time namespaces ([`crate::nsproxy`]).
    pub init_ns: crate::nsproxy::NsProxy,
    /// The System V IPC objects and POSIX message queues of each IPC namespace, by namespace id;
    /// namespaces that no thread uses any more are dropped ([`Kernel::collect_ipc`]).
    pub ipcs: BTreeMap<u64, (Weak<crate::nsproxy::IpcNs>, crate::ipc::Ipc)>,
    /// The cgroup tree ([`crate::cgroup`]).
    pub cgroups: crate::cgroup::Cgroups,
    /// Ids of loaded eBPF programs ([`crate::bpf`]).
    pub bpf: crate::bpf::Registry,
    /// Newly created network namespaces ([`crate::nsproxy::NetNs`]): host loopback addresses still
    /// in use are avoided when allocating.
    net_nss: Vec<Weak<crate::nsproxy::NetNs>>,
    /// Addresses bound by AF_UNIX sockets.
    pub unix: crate::unix::Names,
    /// AF_UNIX channels with open files in their messages (for `unix::collect`).
    pub unix_inflight: Vec<Weak<RefCell<crate::unix::Channel>>>,
    /// Named pipes (FIFOs) currently open: all opens of one inode share one pipe object.
    pub fifos: BTreeMap<Ino, Weak<RefCell<crate::pipe::Pipe>>>,
    /// The loop devices (`syscall::loopdev`).
    pub loops: crate::syscall::Loops,
    /// memfd seals (F_ADD_SEALS), keyed by inode; files not in here cannot be sealed.
    pub seals: BTreeMap<Ino, u32>,
    /// Held files whose data is being copied to an object of their own with the BKL released
    /// (`syscall::fs::make_private`): others wanting to change them wait
    /// ([`wake::LOCK`]).
    pub unsharing: std::collections::BTreeSet<Ino>,
    /// `/proc/sys/kernel/core_pattern`, `core_uses_pid` (`crate::coredump`).
    pub core_pattern: Vec<u8>,
    pub core_uses_pid: bool,
    /// Kernel log (`syscall::misc::syslog`): the four levels of `/proc/sys/kernel/printk`
    /// (console, default message, minimum console, default console), the console level saved by
    /// `SYSLOG_ACTION_CONSOLE_OFF`, and `/proc/sys/kernel/dmesg_restrict`.
    pub printk: [i32; 4],
    pub saved_console_loglevel: Option<i32>,
    pub dmesg_restrict: bool,
    /// `/proc/sys/kernel/panic`: only recorded; Tuxport's kernel layer has no panic (a kernel bug
    /// ends only the process that hits it).
    pub panic: i32,
    /// `/proc/sys/fs/protected_symlinks`, `protected_hardlinks` (Linux's `may_follow_link`,
    /// `may_linkat`), on by default, as systemd and Android set them.
    pub protected_symlinks: bool,
    pub protected_hardlinks: bool,
    /// `/proc/sys/kernel/unprivileged_bpf_disabled`: at 0, processes without capabilities may also
    /// use some bpf(2) commands; at 1 it cannot be changed back; at 2 (the default,
    /// `BPF_UNPRIV_DEFAULT_OFF`) root can change it to 0.
    pub unprivileged_bpf_disabled: u8,
    /// The host has booted from inittab ([`crate::Machine::boot`]): the built-in init then handles
    /// SIGTERM, SIGUSR1 and SIGUSR2 like busybox's ([`crate::Power`]); what it receives is recorded
    /// in `power` and taken by the host ([`crate::Machine::take_power`]) to shut down.
    pub init_handlers: bool,
    pub power: Option<crate::Power>,
    /// The host has started shutting down and ending all processes ([`crate::Machine::shutdown`],
    /// `kill_all`): a [`crate::Machine::boot`] still booting runs no further commands.
    pub stopping: bool,
    /// The kernel log records ([`crate::klog`]): open files of `/dev/kmsg` hold it too.
    pub klog: Rc<RefCell<crate::klog::Klog>>,
    /// `/proc/sys/fs/pipe-max-size`: the largest pipe capacity without `CAP_SYS_RESOURCE`.
    pub pipe_max_size: usize,
    /// `/proc/sys/fs/nr_open`: the fd ceiling (`RLIMIT_NOFILE` cannot exceed it).
    pub nr_open: usize,
    /// `/proc/sys/fs/mount-max`: the most mounts one mount namespace may hold (Linux's
    /// `sysctl_mount_max`); adding more is `ENOSPC`.
    pub mount_max: usize,
    /// `/proc/sys/kernel/threads-max`: the upper bound on the number of threads existing at once;
    /// it applies to root too.
    pub threads_max: usize,
    /// The app's memory limit and the OOM killer ([`crate::oom`]).
    pub oom: crate::oom::Oom,
    /// Guest processes the kernel layer ended (kernel bugs, host faults in the engine, the OOM
    /// killer), waiting for the app to take them ([`crate::Machine::take_kills`]).
    pub kills: RefCell<Vec<crate::bug::KillReport>>,
    /// The paths the kernel layer does not implement that the guest has reached ([`crate::stub`]);
    /// in a cell, since procfs lookups record them through a shared view.
    pub stubs: RefCell<crate::stub::Stubs>,
    /// Groups in which address spaces of the same program share Tier 1 translations
    /// ([`crate::mm::CodeGroup`]).
    pub code_groups: crate::mm::CodeGroups,
    /// Load average ([`crate::loadavg`]).
    pub load: crate::loadavg::LoadAvg,
    /// Exits of traced threads not yet taken by the tracer's wait ([`crate::ptrace`]).
    pub trace_exits: Vec<crate::ptrace::TraceExit>,
    next_tid: Tid,
    /// Waiting threads.
    waiters: Vec<Waiter>,
    /// Waiting epoll_waits that, because of edge triggering, leave some events out of what they
    /// wait for (`syscall::poll`).
    pub edge_waiters: Vec<Arc<Event>>,
    /// Threads waiting on files watched with `EPOLLEXCLUSIVE` (the exclusive gates of
    /// `syscall::poll`), checked by the timer thread.
    pub ex_gates: Vec<crate::syscall::ExGate>,
    /// Open files with `O_ASYNC` set (the fasync of `syscall::poll`).
    pub fasync: Vec<crate::syscall::Fasync>,
    /// AIO contexts with a pending poll that signals an eventfd, completed by the timer thread
    /// (`syscall::aio`).
    pub aio_polls: Vec<Weak<RefCell<crate::syscall::AioContext>>>,
    /// The ICMP taps of the network namespaces ([`crate::rawicmp`]), watched by the timer thread.
    pub icmp_taps: Vec<std::rc::Weak<crate::cell::RefCell<crate::rawicmp::Tap>>>,
    /// The `NETLINK_ROUTE` sockets, which link, address and route notifications go to
    /// (`syscall::net`, [`crate::net::Snapshot`]).
    pub rtnl_sockets: Vec<std::rc::Weak<crate::cell::RefCell<crate::net::Socket>>>,
    /// DNS stub queries being resolved by host threads: the socket each answer goes to
    /// ([`Shared::dns_answers`], `syscall::net`).
    pub dns_queries: HashMap<u64, std::rc::Weak<crate::cell::RefCell<crate::net::Socket>>>,
    /// The number of the last DNS stub query sent.
    pub dns_last: u64,
    /// File leases, by inode (`syscall::lease`).
    pub leases: BTreeMap<Ino, Vec<crate::syscall::Lease>>,
    /// `/proc/sys/fs/lease-break-time`: the most seconds to wait when breaking a lease.
    pub lease_break_time: u64,
    /// `/proc/sys/kernel/seccomp/actions_logged`: which seccomp actions are logged
    /// (`seccomp::logged`).
    pub seccomp_logged: u32,
    /// Network interface traffic counters ([`crate::traffic`]).
    pub traffic: crate::traffic::Traffic,
    /// The mapping of ports below 1024 and the guest's listeners on non-loopback addresses
    /// ([`crate::portmap`]). Socket leases also refer to it, so it is shared through Rc.
    pub portmap: std::rc::Rc<crate::cell::RefCell<crate::portmap::PortMap>>,
    /// Forwarding of connections from outside the host to published ports ([`crate::forward`]).
    pub forward: crate::forward::Forwarder,
    /// The host address that TCP connections dropped by the filter chain connect to
    /// (`syscall::net::blackhole`), created on first use.
    pub blackhole: std::cell::OnceCell<Option<Vec<u8>>>,
    /// The original destination addresses of TCP connections whose destination was rewritten by
    /// nat and that have not been accepted yet ([`crate::origdst`], `SO_ORIGINAL_DST`).
    pub orig_dst: crate::origdst::Table,
    /// Main-table routes handed over by the host ([`crate::Machine::set_routes`], taken from
    /// `LinkProperties` on Android); when None, the host's are read ([`crate::route`]).
    pub host_routes: Option<Vec<tuxport_hal::net::Route>>,
    /// The events taken by AIO contexts (`/proc/sys/fs/aio-nr`) and the limit (`aio-max-nr`).
    pub aio_nr: u64,
    pub aio_max_nr: u64,
    /// The namespaces each user has created per user namespace, and the limits on them
    /// (`/proc/sys/user/max_*_namespaces`).
    pub ucounts: crate::nsproxy::Ucounts,
    /// The written values of the sysctls procfs only records (`crate::pseudo::RECORDED`, by path);
    /// a missing one still has its default.
    pub sysctls: BTreeMap<&'static [u8], i64>,
    /// `/proc/sys/kernel/modprobe` (only recorded: there are no modules).
    pub modprobe: Vec<u8>,
    /// `/proc/sys/kernel/tainted`: the taint flags written by privileged processes (Tuxport itself
    /// taints nothing).
    pub tainted: u64,
    /// The ratelimit of unhandled-exception reports, kernel-wide as in arm64 Linux's
    /// `arm64_show_signal` (crate::signal).
    pub unhandled_rs: crate::klog::Ratelimit,
    /// All inotify watches. Open files must report `IN_CLOSE_*` when destroyed, so it is shared
    /// through Rc.
    pub inotify: Rc<RefCell<crate::syscall::Registry>>,
    /// Read-only file mappings shared by all address spaces ([`crate::mm::FileMaps`]).
    pub file_maps: crate::mm::FileMaps,
}

// SAFETY: Kernel and the Rc/RefCell in it are accessed only while holding the BKL (the
// Shared::kernel lock), see the module docs
unsafe impl Send for Kernel {}

impl Kernel {
    pub fn new(tier: Tier) -> Kernel {
        // Record the host's CPUs before any guest thread changes the host affinity
        tuxport_hal::host_cpus();
        let init_ns = crate::nsproxy::NsProxy::init();
        Kernel {
            tasks: BTreeMap::new(),
            groups: BTreeMap::new(),
            pid_nss: Default::default(),
            held_reapers: Vec::new(),
            tier,
            watched: BTreeMap::new(),
            vfs: None,
            shutting_down: false,
            dbt_stats: None,
            cpu: CpuTime::default(),
            forks: 0,
            boot_id: {
                let mut id = [0u8; 16];
                let _ = tuxport_hal::fill_random(&mut id);
                id[6] = (id[6] & 0x0f) | 0x40;
                id[8] = (id[8] & 0x3f) | 0x80;
                id
            },
            ptys: BTreeMap::new(),
            consoles: BTreeMap::new(),
            flocks: BTreeMap::new(),
            locks: crate::locks::Locks::default(),
            init_ns: init_ns.clone(),
            net_nss: Vec::new(),
            cgroups: Default::default(),
            bpf: Default::default(),
            ipcs: BTreeMap::from([(
                init_ns.ipc.id,
                (Rc::downgrade(&init_ns.ipc), crate::ipc::Ipc::default()),
            )]),
            unix: Default::default(),
            unix_inflight: Vec::new(),
            fifos: BTreeMap::new(),
            loops: crate::syscall::Loops::new(),
            seals: BTreeMap::new(),
            unsharing: Default::default(),
            core_pattern: b"core".to_vec(),
            core_uses_pid: false,
            // The default configuration of CONSOLE_LOGLEVEL_DEFAULT, MESSAGE_LOGLEVEL_DEFAULT and
            // CONSOLE_LOGLEVEL_MIN; no CONFIG_SECURITY_DMESG_RESTRICT
            printk: [7, 4, 1, 7],
            saved_console_loglevel: None,
            dmesg_restrict: false,
            panic: 0,
            protected_symlinks: true,
            protected_hardlinks: true,
            unprivileged_bpf_disabled: 2,
            init_handlers: false,
            power: None,
            stopping: false,
            klog: Default::default(),
            kills: Default::default(),
            stubs: Default::default(),
            pipe_max_size: crate::pipe::PIPE_MAX_SIZE,
            nr_open: crate::fd::MAX_FDS,
            mount_max: 100_000,
            threads_max: crate::process::default_threads_max(),
            oom: Default::default(),
            code_groups: Default::default(),
            load: crate::loadavg::LoadAvg::new(Instant::now()),
            trace_exits: Vec::new(),
            next_tid: INIT + 1,
            waiters: Vec::new(),
            edge_waiters: Vec::new(),
            ex_gates: Vec::new(),
            fasync: Vec::new(),
            aio_polls: Vec::new(),
            icmp_taps: Vec::new(),
            rtnl_sockets: Vec::new(),
            dns_queries: HashMap::new(),
            dns_last: 0,
            leases: BTreeMap::new(),
            lease_break_time: 45,
            seccomp_logged: crate::seccomp::logged::DEFAULT,
            traffic: Default::default(),
            portmap: Default::default(),
            forward: Default::default(),
            blackhole: Default::default(),
            orig_dst: Default::default(),
            host_routes: None,
            aio_nr: 0,
            aio_max_nr: 65536,
            ucounts: Default::default(),
            sysctls: BTreeMap::new(),
            modprobe: b"/sbin/modprobe".to_vec(),
            tainted: 0,
            unhandled_rs: Default::default(),
            inotify: Rc::default(),
            file_maps: Default::default(),
        }
    }

    /// Chooses the global number for a new thread or process in pid namespace `ns` (the checking
    /// part of Linux's `alloc_pid`). `set_tid` holds the numbers clone3 specified for each level,
    /// from the level of `ns` outwards: there may not be more levels than `ns` has; each number
    /// must be between 1 and pid_max, and must be 1 while the namespace has no process yet
    /// (EINVAL); `cred` must have `CAP_SYS_ADMIN` or `CAP_CHECKPOINT_RESTORE` in that level's
    /// owner (EPERM); a number already in use is EEXIST. If the global number is specified it is
    /// used, otherwise one is allocated.
    pub fn choose_pid(
        &mut self,
        ns: &Rc<crate::pidns::PidNs>,
        cred: &crate::cred::Cred,
        set_tid: &[Tid],
    ) -> Result<Tid, Errno> {
        use crate::cred::cap;
        let levels = ns.levels_from(0);
        if set_tid.len() > levels.len() {
            return Err(Errno::EINVAL);
        }
        for (n, &tid) in levels.iter().rev().zip(set_tid) {
            if !(1..PID_MAX).contains(&tid) || tid != 1 && n.level > 0 && n.reaper.get().is_none() {
                return Err(Errno::EINVAL);
            }
            if !cred.ns_capable(&n.owner, cap::SYS_ADMIN)
                && !cred.ns_capable(&n.owner, cap::CHECKPOINT_RESTORE)
            {
                return Err(Errno::EPERM);
            }
            let taken = if n.level == 0 {
                self.tasks.contains_key(&tid) || self.groups.contains_key(&tid)
            } else {
                n.find(tid).is_some()
            };
            if taken {
                return Err(Errno::EEXIST);
            }
        }
        match set_tid.get(ns.level as usize) {
            Some(&tid) => Ok(tid),
            None => self.alloc_tid(),
        }
    }

    /// Allocates a tid not taken by a thread or a thread group (zombies included): the numbers
    /// after the last one below `PID_MAX`, then from 300 again (Linux's `RESERVED_PIDS` in
    /// `alloc_pid`).
    pub fn alloc_tid(&mut self) -> Result<Tid, Errno> {
        for _ in 0..PID_MAX {
            let tid = if self.next_tid >= PID_MAX {
                crate::pidns::RESERVED_PIDS
            } else {
                self.next_tid
            };
            self.next_tid = tid + 1;
            if !self.tasks.contains_key(&tid) && !self.groups.contains_key(&tid) {
                return Ok(tid);
            }
        }
        Err(Errno::EAGAIN)
    }

    /// Creates an empty IPC namespace owned by user namespace `owner` (Linux's `create_ipc_ns`:
    /// all limits at their defaults). `creator` is the fsuid and fsgid of the process creating it:
    /// the root directory of the mqueue filesystem belongs to them (`mqueue_get_inode` in
    /// `mq_init_ns` takes `current_fsuid()`).
    pub fn new_ipc_ns(
        &mut self,
        owner: Rc<crate::cred::UserNs>,
        creator: (u32, u32),
    ) -> Rc<crate::nsproxy::IpcNs> {
        let ns = Rc::new(crate::nsproxy::IpcNs {
            id: crate::vfs::new_ns_id(),
            owner,
        });
        let ipc = crate::ipc::Ipc {
            mq_dir: crate::cell::RefCell::new(crate::syscall::MqDir::new(creator.0, creator.1)),
            ..Default::default()
        };
        let objects = (Rc::downgrade(&ns), ipc);
        self.ipcs.insert(ns.id, objects);
        ns
    }

    /// Creates a network namespace owned by user namespace `owner`: it contains only a down lo;
    /// it takes a host loopback range 127.a.b.0/24 that no namespace uses yet (a and b not both
    /// 0); the IPv4 sysctls are copied from the boot-time namespace's current ones
    /// ([`crate::netconf`]).
    pub fn new_net_ns(
        &mut self,
        owner: Rc<crate::cred::UserNs>,
    ) -> Result<Rc<crate::nsproxy::NetNs>, Errno> {
        self.net_nss.retain(|w| w.strong_count() > 0);
        let used: Vec<[u8; 2]> = self
            .net_nss
            .iter()
            .filter_map(|w| w.upgrade().and_then(|n| n.host_net))
            .collect();
        // 127.255.0.0/16 is reserved for virtual addresses in the boot-time namespace
        // (crate::netns::INIT_BLOCK), 127.254.0.0/16 for connections forwarded in from outside
        // (crate::forward::OUTSIDE_BLOCK)
        let net = (1..=u16::MAX)
            .map(u16::to_be_bytes)
            .find(|n| {
                n[0] != crate::netns::INIT_BLOCK
                    && n[0] != crate::forward::OUTSIDE_BLOCK
                    && !used.contains(n)
            })
            .ok_or(Errno::ENOMEM)?;
        let devs = crate::netdev::Devices::for_new_ns(&self.init_ns.net.devs.borrow().conf);
        let ns = Rc::new(crate::nsproxy::NetNs {
            id: crate::vfs::new_ns_id(),
            owner,
            host_net: Some(net),
            lo: Default::default(),
            devs: crate::cell::RefCell::new(devs),
            nft: Default::default(),
            listeners: Default::default(),
        });
        self.net_nss.push(Rc::downgrade(&ns));
        Ok(ns)
    }

    /// All live network namespaces, the boot-time one first (for address translation,
    /// [`crate::netns::World`]).
    pub fn world(&self) -> crate::netns::World {
        let mut all = vec![self.init_ns.net.clone()];
        all.extend(self.net_nss.iter().filter_map(Weak::upgrade));
        crate::netns::World { all }
    }

    /// The objects in IPC namespace `id`.
    pub fn ipc(&self, id: u64) -> &crate::ipc::Ipc {
        &self.ipcs[&id].1
    }

    pub fn ipc_mut(&mut self, id: u64) -> &mut crate::ipc::Ipc {
        &mut self.ipcs.get_mut(&id).expect("IPC namespace in use").1
    }

    /// IPC namespaces that no thread uses any more: drops the objects in them (Linux's
    /// `free_ipc_ns`; for shared memory segments still mapped, the mappings keep the memory).
    pub fn collect_ipc(&mut self) {
        self.ipcs.retain(|_, (ns, _)| ns.strong_count() > 0);
    }

    /// New thread or process `tid` is in pid namespace `ns`: allocates its number at each level
    /// ([`crate::pidns`]); for `set_tid` see [`Kernel::choose_pid`].
    pub fn register_pid(
        &mut self,
        ns: &Rc<crate::pidns::PidNs>,
        tid: Tid,
        set_tid: &[Tid],
    ) -> Result<(), Errno> {
        if ns.level == 0 {
            return Ok(());
        }
        crate::pidns::PidNs::register(ns, tid, set_tid)?;
        self.pid_nss.insert(tid, ns.clone());
        Ok(())
    }

    /// After a thread exits or a zombie is reaped: when number `tid` is used by neither a thread
    /// nor a process any more, removes it from each level's tables. A pid 1 waiting for all
    /// processes in its namespace to be reaped ([`Kernel::hold_reaper`]) finishes at this point
    /// if its wait is over.
    pub fn release_pid(&mut self, tid: Tid) {
        if !self.tasks.contains_key(&tid)
            && !self.groups.contains_key(&tid)
            && let Some(ns) = self.pid_nss.remove(&tid)
        {
            crate::pidns::PidNs::unregister(&ns, tid);
            while let Some(i) = self.held_reapers.iter().position(|(ns, g, _)| {
                let tgid = g.borrow().tgid;
                ns.members().iter().all(|&m| m == tgid)
            }) {
                let (_, group, pgrps) = self.held_reapers.swap_remove(i);
                crate::process::finish_reported(self, &group, pgrps);
            }
        }
    }

    /// Pid 1 `group` of pid namespace `ns` has exited while other processes remain in the
    /// namespace: reports to the parent only once they have all been reaped (`pgrps` are the
    /// process groups [`crate::process::finish_group`] computed).
    pub fn hold_reaper(
        &mut self,
        ns: Rc<crate::pidns::PidNs>,
        group: Rc<RefCell<ThreadGroup>>,
        pgrps: Vec<Tid>,
    ) {
        self.held_reapers.push((ns, group, pgrps));
    }

    /// The number of global number `g` in the pid namespace of process `viewer`, 0 when it cannot
    /// be seen.
    pub fn pid_seen_by(&self, viewer: Tid, g: Tid) -> Tid {
        self.pid_ns_of(viewer).vnr(g).unwrap_or(0)
    }

    /// The credentials of thread `tid` (a thread ID, or a pid standing for its main thread): its
    /// own while it runs, otherwise its process's, which keep the main thread's after it ended
    /// ([`ThreadGroup::cred`]). None for a thread that does not exist.
    pub fn cred_of(&self, tid: Tid) -> Option<crate::cred::Cred> {
        match self.tasks.get(&tid) {
            Some(t) => Some(t.cred.borrow().clone()),
            None => self.groups.get(&tid).map(|g| g.borrow().cred.clone()),
        }
    }

    /// The pid namespace that process `tgid` is in.
    pub fn pid_ns_of(&self, tgid: Tid) -> Rc<crate::pidns::PidNs> {
        self.groups
            .get(&tgid)
            .map_or_else(crate::pidns::PidNs::init, |g| g.borrow().pid_ns.clone())
    }

    /// The last allocated tid (`/proc/sys/kernel/ns_last_pid`).
    pub fn last_pid(&self) -> Tid {
        self.next_tid - 1
    }

    /// Writes `ns_last_pid`: allocation then starts from `pid + 1` (skipping those taken), or
    /// from 300 after the largest number.
    pub fn set_last_pid(&mut self, pid: Tid) {
        self.next_tid = (pid.saturating_add(1)).clamp(INIT + 1, PID_MAX);
    }

    /// The open files in all fd tables. An open file in several fd tables appears that many times.
    pub(crate) fn open_files(&self) -> Vec<FileRef> {
        let mut tables: Vec<Rc<RefCell<FdTable>>> = Vec::new();
        let mut files = Vec::new();
        for t in self.tasks.values() {
            let fds = t.res.borrow().fds.clone();
            if tables.iter().any(|p| Rc::ptr_eq(p, &fds)) {
                continue;
            }
            {
                let table = fds.borrow();
                files.extend(
                    table
                        .open_fds()
                        .into_iter()
                        .filter_map(|fd| table.get(fd).ok()),
                );
            }
            tables.push(fds);
        }
        files
    }

    fn flush_wakeups(&self) {
        let (mut channels, mut objects) = PENDING.with_borrow_mut(std::mem::take);
        let stray = WAKE.swap(0, Ordering::AcqRel);
        if stray != 0 {
            channels |= stray;
            let mut more = WAKE_OBJECTS.lock().unwrap_or_else(PoisonError::into_inner);
            objects.append(&mut more);
        }
        if channels == 0 {
            return;
        }
        for (event, interest, wanted) in &self.waiters {
            let hit = interest & channels & wake::ALL != 0
                || interest & wake::FILE != 0
                    && !objects.is_empty()
                    && wanted
                        .as_ref()
                        .is_none_or(|w| objects.iter().any(|o| w.binary_search(o).is_ok()));
            if hit {
                signal_later(event);
            }
        }
    }
}

// ---- The BKL ----

/// How long at most to spin when taking the BKL ([`Shared::lock`]). It used to spin 100 times
/// (under 1 µs), and with four processes starting processes at once the total throughput was lower
/// than with one: as soon as the holder released the lock, the waiters had all gone to sleep, and
/// the lock stayed free while they were being woken. Anything from 10 to 60 µs is about the same;
/// with longer spins the spinning threads take the holder's core instead.
const BKL_SPIN: std::time::Duration = std::time::Duration::from_micros(30);

/// DNS stub answers resolved by host threads: (query number, answer message)
/// ([`Shared::dns_answers`]).
pub(crate) type DnsAnswers = Mutex<Vec<(u64, Vec<u8>)>>;

/// Kernel objects shared by all guest threads.
pub(crate) struct Shared {
    kernel: Mutex<Kernel>,
    /// Whether some thread holds the BKL, and which ([`Hold`]).
    hold: Hold,
    /// The latest hold of the BKL that the watchdog found too long ([`crate::watchdog`]).
    pub stall: Mutex<Option<crate::watchdog::Stall>>,
    /// The number of threads spinning for the BKL: at most [`max_spinners`]; the others go
    /// straight to sleep rather than wasting cores.
    spinning: AtomicU32,
    /// Wakes the timer thread.
    pub timers: Arc<Event>,
    /// DNS stub answers the host threads resolved, by query number ([`Kernel::dns_queries`]), for
    /// the timer thread to queue on their sockets. The host threads hold only this and
    /// [`Shared::timers`], never the kernel.
    pub dns_answers: Arc<DnsAnswers>,
    /// The timer thread's periodic checks (memory limit [`crate::oom`], load average
    /// [`crate::loadavg`]) have stopped: all threads are waiting.
    pub checks_idle: AtomicBool,
    /// Incremented whenever the state threads recorded for no-BKL syscalls may be stale
    /// ([`Shared::fast_changed`]).
    fast: AtomicU64,
    /// The futex wait queue. No-BKL futexes (`syscall::fast`) take only this lock; it may also be
    /// taken while holding the BKL, but not the other way round. Checking the futex value and
    /// registering as a waiter happen in one hold of it, and waking takes it too, so no wake-up
    /// is lost between the two.
    futexes: Mutex<Futexes>,
}

impl Shared {
    pub fn new(kernel: Kernel) -> Shared {
        Shared {
            kernel: Mutex::new(kernel),
            hold: Hold::default(),
            stall: Mutex::new(None),
            spinning: AtomicU32::new(0),
            timers: Arc::new(Event::new().expect("cannot create an eventfd for the timers")),
            dns_answers: Arc::new(Mutex::new(Vec::new())),
            checks_idle: AtomicBool::new(false),
            fast: AtomicU64::new(0),
            futexes: Mutex::new(Futexes::default()),
        }
    }

    /// Takes the BKL. A kernel-layer panic poisons the lock: the offending process has already
    /// been ended ([`crate::bug`]), and the rest go on using it as usual.
    ///
    /// The lock is usually held for only a few microseconds (one lstat), and long holds (execve,
    /// fork, process exit) last from about ten to several tens of microseconds, so first spin for
    /// at most [`BKL_SPIN`]: going to sleep and being woken takes tens of microseconds, the
    /// releasing side needs one more futex wake, and the lock stays free meanwhile. Parallel
    /// lstat in `git status` used to be slower than single-threaded. Spinning reads only
    /// [`Hold::held`] and tries to take the lock only when it looks free; when [`max_spinners`]
    /// threads are already spinning, go straight to sleep.
    pub fn lock(&self) -> Bkl<'_> {
        if let Ok(k) = self.kernel.try_lock() {
            return Bkl::new(k, &self.hold);
        }
        if self.spinning.fetch_add(1, Ordering::Acquire) < max_spinners() {
            if let Some(k) = self.spin() {
                return k;
            }
        } else {
            self.spinning.fetch_sub(1, Ordering::Release);
        }
        Bkl::new(
            self.kernel.lock().unwrap_or_else(PoisonError::into_inner),
            &self.hold,
        )
    }

    /// Spins for the BKL, for at most [`BKL_SPIN`]; the caller has already incremented
    /// [`Shared::spinning`], and it is decremented here.
    fn spin(&self) -> Option<Bkl<'_>> {
        let mut deadline = None;
        for i in 0u32.. {
            if !self.hold.held.load(Ordering::Relaxed) {
                let k = match self.kernel.try_lock() {
                    Ok(k) => Some(k),
                    Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
                    Err(std::sync::TryLockError::WouldBlock) => None,
                };
                if let Some(k) = k {
                    self.spinning.fetch_sub(1, Ordering::Release);
                    return Some(Bkl::new(k, &self.hold));
                }
            }
            std::hint::spin_loop();
            // Reading the clock costs much more than one spin iteration, so look only every so
            // often
            if i % 64 == 63 {
                let now = Instant::now();
                if now >= *deadline.get_or_insert(now + BKL_SPIN) {
                    break;
                }
            }
        }
        self.spinning.fetch_sub(1, Ordering::Release);
        None
    }

    /// Distinguishes the kernels in one host process (in tests, when the app switches kernels):
    /// [`bkl_holder`] uses it to tell which BKL is held. It is the address of [`Hold::held`],
    /// which does not change while Shared lives and is never 0.
    pub fn id(&self) -> usize {
        &self.hold.held as *const AtomicBool as usize
    }

    /// Who holds the BKL, read without it ([`crate::watchdog`]).
    pub fn hold(&self) -> &Hold {
        &self.hold
    }

    /// The state threads recorded for no-BKL syscalls (`syscall::fast`) may be stale: some
    /// thread's root directory, current directory or credentials changed. Called while holding
    /// the BKL.
    pub fn fast_changed(&self) {
        self.fast.fetch_add(1, Ordering::Release);
    }

    /// The number of [`Shared::fast_changed`] calls.
    pub fn fast_epoch(&self) -> u64 {
        self.fast.load(Ordering::Acquire)
    }

    /// The futex wait queue ([`Shared::futexes`]).
    pub fn futexes(&self) -> MutexGuard<'_, Futexes> {
        self.futexes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The thread is about to go on running guest code: wakes the timer thread if the periodic
    /// checks are stopped. Does not need the BKL. The thread has already cleared its `sleeping`
    /// flag (SeqCst) before this SeqCst read, pairing with [`stop_checks`](Self::stop_checks).
    pub fn resume_checks(&self) {
        if self.checks_idle.load(Ordering::SeqCst) && self.checks_idle.swap(false, Ordering::SeqCst)
        {
            self.timers.signal();
        }
    }

    /// The timer thread saw every thread waiting and stops its periodic checks: sets
    /// `checks_idle`, then looks at the threads again with `idle` and clears it when one has woken
    /// meanwhile. Returns whether the checks stopped. With every `sleeping` store and load and both
    /// sides' `checks_idle` accesses SeqCst, either a waking thread reads the flag set and wakes
    /// the timer thread, or this second look sees it running; one look alone could miss a thread
    /// that woke between the look and the store, stopping the OOM and load checks until something
    /// else woke the timer thread.
    pub fn stop_checks(&self, idle: impl Fn() -> bool) -> bool {
        self.checks_idle.store(true, Ordering::SeqCst);
        if idle() {
            return true;
        }
        self.checks_idle.store(false, Ordering::SeqCst);
        false
    }
}

/// Proof of holding the BKL; on release it wakes the waiting threads, then returns to the host
/// the host memory released while it was held ([`crate::mm::release_unmapped`]).
pub(crate) struct Bkl<'a>(ManuallyDrop<MutexGuard<'a, Kernel>>, &'a Hold);

impl<'a> Bkl<'a> {
    fn new(guard: MutexGuard<'a, Kernel>, hold: &'a Hold) -> Bkl<'a> {
        hold.held.store(true, Ordering::Relaxed);
        hold.record();
        HELD.set(&hold.held as *const AtomicBool as usize);
        crate::mm::defer_unmaps();
        Bkl(ManuallyDrop::new(guard), hold)
    }
}

impl Deref for Bkl<'_> {
    type Target = Kernel;
    fn deref(&self) -> &Kernel {
        &self.0
    }
}

impl DerefMut for Bkl<'_> {
    fn deref_mut(&mut self) -> &mut Kernel {
        &mut self.0
    }
}

impl Drop for Bkl<'_> {
    fn drop(&mut self) {
        // A terminal whose last master was closed while the lock was held must be hung up, and
        // hanging up sends signals and wakes waiters
        let k: &mut Kernel = &mut self.0;
        if crate::bug::catch(|| crate::syscall::hang_up_closed(k)).is_none() {
            crate::bug::report(format_args!("kernel panic while hanging up terminals"));
        }
        self.0.flush_wakeups();
        self.1.held.store(false, Ordering::Relaxed);
        // SAFETY: self.0 is not used after this
        unsafe { ManuallyDrop::drop(&mut self.0) };
        HELD.set(0);
        crate::task::pool::notify_pending();
        flush_signals();
        crate::mm::release_unmapped();
        crate::mm::spare::refill();
    }
}

/// The most threads waiting for the BKL that spin at once ([`Shared::lock`]); the rest go straight
/// to sleep: half the host CPUs, 1 to 4. It used to let only one spin; with 8 processes starting
/// processes at once on 8 cores the lock almost always had waiters, and as soon as the spinning one
/// got the lock, the next handover could only wake a sleeper, with the lock free while it woke
/// (30% of the time). Letting 4 spin makes concurrent exec 7–10% faster and pipelines 13% faster,
/// using 18% more CPU; go build and Maven are unchanged. More is not faster; the spinning threads
/// would take the holder's core instead.
fn max_spinners() -> u32 {
    static N: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *N.get_or_init(|| (tuxport_hal::host_cpus().len() / 2).clamp(1, 4) as u32)
}

/// Whether some thread holds the BKL, and which, for the watchdog ([`crate::watchdog`]) that reads
/// it without the lock. Each holder writes it when it takes the lock ([`Hold::record`]). It sits
/// on one cache line with `held`, which the holder writes then anyway, so the other stores add
/// almost nothing to taking the lock; it reads no clock.
#[derive(Default)]
#[repr(C, align(64))]
pub(crate) struct Hold {
    /// Some thread holds the BKL (only a hint): waiters read only this while spinning and try to
    /// take the lock only when it looks free, so that the lock's cache line does not bounce
    /// between cores.
    held: AtomicBool,
    /// How many times the lock was taken (wrapping): a different count is a different hold.
    count: AtomicU32,
    /// The holder's host tid.
    host_tid: AtomicI32,
    /// The guest thread on the holder's host thread and its process ([`Entry`]); 0 when the
    /// holder runs none (the timer thread, the host calling [`crate::Machine`]).
    tid: AtomicI32,
    tgid: AtomicI32,
    /// That thread's name, 16 bytes in two words.
    comm: [AtomicU64; 2],
    /// What that guest thread entered the kernel for ([`Doing::bits`]).
    doing: AtomicU64,
    /// The guest pc at that entry.
    pc: AtomicU64,
}

/// One look at [`Hold`] while the BKL was held: the same values seen again mean the same hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HoldSample {
    pub count: u32,
    pub host_tid: i32,
    pub entry: Entry,
}

impl Hold {
    /// Records the calling thread as the holder; called just after taking the lock. Only the
    /// holder writes, so the count needs no read-modify-write.
    fn record(&self) {
        let host_tid = match HOST_TID.get() {
            0 => {
                let tid = tuxport_hal::gettid();
                HOST_TID.set(tid);
                tid
            }
            tid => tid,
        };
        let e = DOING.get();
        self.host_tid.store(host_tid, Ordering::Relaxed);
        self.tid.store(e.tid, Ordering::Relaxed);
        self.tgid.store(e.tgid, Ordering::Relaxed);
        for (word, bytes) in self.comm.iter().zip(e.comm.chunks_exact(8)) {
            word.store(
                u64::from_ne_bytes(bytes.try_into().unwrap()),
                Ordering::Relaxed,
            );
        }
        self.doing.store(e.doing.bits(), Ordering::Relaxed);
        self.pc.store(e.pc, Ordering::Relaxed);
        let count = self.count.load(Ordering::Relaxed).wrapping_add(1);
        self.count.store(count, Ordering::Relaxed);
    }

    /// The current holder, None when the lock looks free. The fields are read one by one without
    /// the lock: a sample taken while a new holder writes them can mix two holders, and differs
    /// from the samples before and after it, so it only looks like another hold.
    pub fn sample(&self) -> Option<HoldSample> {
        if !self.held.load(Ordering::Relaxed) {
            return None;
        }
        let mut comm = [0; 16];
        for (bytes, word) in comm.chunks_exact_mut(8).zip(&self.comm) {
            bytes.copy_from_slice(&word.load(Ordering::Relaxed).to_ne_bytes());
        }
        Some(HoldSample {
            count: self.count.load(Ordering::Relaxed),
            host_tid: self.host_tid.load(Ordering::Relaxed),
            entry: Entry {
                tid: self.tid.load(Ordering::Relaxed),
                tgid: self.tgid.load(Ordering::Relaxed),
                comm,
                doing: Doing::from_bits(self.doing.load(Ordering::Relaxed)),
                pc: self.pc.load(Ordering::Relaxed),
            },
        })
    }
}

/// Why a guest thread is in the kernel ([`doing`]): what a long hold of the BKL is reported with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    pub tid: Tid,
    /// 0 before the thread has found its process (while it starts).
    pub tgid: Tid,
    /// The thread's name at the entry (`Task::comm`), copied so that a report needs neither the
    /// BKL nor the thread still being there; zeros while it starts.
    pub comm: [u8; 16],
    pub doing: Doing,
    /// The guest pc at the entry.
    pub pc: u64,
}

impl Entry {
    const NONE: Entry = Entry {
        tid: 0,
        tgid: 0,
        comm: [0; 16],
        doing: Doing::Nothing,
        pc: 0,
    };
}

/// What a guest thread entered the kernel for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Doing {
    /// Not on a guest thread.
    Nothing,
    /// Starting the thread.
    Start,
    /// A syscall: its number, the low 56 bits of x8.
    Syscall(u64),
    /// A memory fault.
    Fault,
    /// A kick: signals, stops, a TLB shootdown.
    Kick,
    /// Any other exception (undefined instruction, breakpoint, code invalidation, a host fault).
    Exception,
    /// Ending the thread.
    Exit,
}

impl Doing {
    /// The kind in the top byte, a syscall number below it.
    const fn bits(self) -> u64 {
        match self {
            Doing::Nothing => 0,
            Doing::Start => 1 << 56,
            Doing::Syscall(nr) => 2 << 56 | nr & ((1 << 56) - 1),
            Doing::Fault => 3 << 56,
            Doing::Kick => 4 << 56,
            Doing::Exception => 5 << 56,
            Doing::Exit => 6 << 56,
        }
    }

    const fn from_bits(bits: u64) -> Doing {
        match bits >> 56 {
            1 => Doing::Start,
            2 => Doing::Syscall(bits & ((1 << 56) - 1)),
            3 => Doing::Fault,
            4 => Doing::Kick,
            5 => Doing::Exception,
            6 => Doing::Exit,
            _ => Doing::Nothing,
        }
    }
}

impl std::fmt::Display for Doing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Doing::Nothing => f.write_str("outside the guest"),
            Doing::Start => f.write_str("starting the thread"),
            Doing::Syscall(nr) => write!(f, "in syscall {nr}"),
            Doing::Fault => f.write_str("handling a memory fault"),
            Doing::Kick => f.write_str("handling a kick"),
            Doing::Exception => f.write_str("handling an exception"),
            Doing::Exit => f.write_str("ending the thread"),
        }
    }
}

/// The guest thread on this host thread enters the kernel for `entry`: every BKL it takes until
/// the next call (waits inside a syscall take it again) records this in [`Hold`].
pub(crate) fn doing(entry: Entry) {
    DOING.set(entry);
}

/// This host thread no longer runs a guest thread.
pub(crate) fn done() {
    DOING.set(Entry::NONE);
}

/// Signals the events collected by [`signal_later`].
pub(crate) fn flush_signals() {
    SIGNALS.with_borrow_mut(|s| s.drain(..).for_each(|e| e.signal()));
}

/// Whether the current thread holds the BKL.
pub(crate) fn bkl_held() -> bool {
    HELD.get() != 0
}

/// Which kernel's BKL the current thread holds ([`Shared::id`]), 0 meaning none. Used only by
/// checking builds ([`crate::vfs::store::Store::guard_with_bkl`]).
#[cfg(any(debug_assertions, feature = "borrow-check"))]
pub(crate) fn bkl_holder() -> usize {
    HELD.get()
}

// ---- Syscall context ----

/// The context of one entry into the kernel (syscall, signal delivery): the proof of holding the
/// BKL, and the current thread.
pub(crate) struct Cx<'a> {
    pub shared: &'a Arc<Shared>,
    k: Option<Bkl<'a>>,
    /// The part of the current thread that other threads also access.
    pub task: &'a Rc<Task>,
    /// The current thread's private state.
    pub t: &'a mut TaskState,
}

impl<'a> Cx<'a> {
    pub fn new(
        shared: &'a Arc<Shared>,
        k: Bkl<'a>,
        task: &'a Rc<Task>,
        t: &'a mut TaskState,
    ) -> Cx<'a> {
        Cx {
            shared,
            k: Some(k),
            task,
            t,
        }
    }

    /// Global state.
    pub fn k(&mut self) -> &mut Kernel {
        self.k.as_mut().expect("BKL is held")
    }

    /// Read-only global state.
    pub fn kr(&self) -> &Kernel {
        self.k.as_ref().expect("BKL is held")
    }

    /// The current thread's tid.
    pub fn tid(&self) -> Tid {
        self.task.tid.get()
    }

    /// The current process's pid (thread group ID).
    pub fn tgid(&self) -> Tid {
        self.task.tgid
    }

    /// Address space.
    pub fn mm(&self) -> Ref<'_, Mm> {
        self.t.mm.borrow()
    }

    /// Mutable address space.
    pub fn mm_mut(&self) -> RefMut<'_, Mm> {
        self.t.mm.borrow_mut()
    }

    /// Grows the grows-down mapping (the main thread's stack, `MAP_GROWSDOWN`) right above `addr`
    /// to reach the page containing it, to a size of at most the current `RLIMIT_STACK`
    /// ([`Mm::grow_down`]).
    pub fn grow_stack(&self, addr: u64) -> bool {
        let limit = self.stack_limit();
        self.mm_mut().grow_down(addr, limit)
    }

    /// Reads guest memory as the kernel does (`copy_from_user`, [`Mm::read`]); an unmapped address
    /// below a grows-down mapping grows it first, as Linux's fault handler does for kernel accesses
    /// ([`Mm::grow_for`]).
    pub fn read_user(&self, src: UserSlice, dst: &mut [u8]) -> Result<(), Errno> {
        let r = self.mm().read(src, dst);
        if r.is_err() && self.grow_for(src) {
            return self.mm().read(src, dst);
        }
        r
    }

    /// Writes guest memory as the kernel does (`copy_to_user`, [`Mm::write`]), growing grows-down
    /// mappings like [`Cx::read_user`].
    pub fn write_user(&self, dst: UserSlice, data: &[u8]) -> Result<(), Errno> {
        let r = self.mm().write(dst, data);
        if r.is_err() && self.grow_for(dst) {
            return self.mm().write(dst, data);
        }
        r
    }

    /// Writes `data` to guest memory at `addr` and returns how much was written: like Linux's
    /// copy_to_user, it stops at the first page that cannot be written ([`Cx::write_user`]), and
    /// what came before it stays written.
    pub fn write_user_partial(&self, addr: u64, data: &[u8]) -> usize {
        if self
            .write_user(UserSlice::new(addr, data.len()), data)
            .is_ok()
        {
            return data.len();
        }
        let mm = self.mm();
        let mut done = 0;
        while done < data.len() {
            let at = addr.wrapping_add(done as u64);
            let n = ((tuxport_engine::PAGE_SIZE - at % tuxport_engine::PAGE_SIZE) as usize)
                .min(data.len() - done);
            if mm
                .write(UserSlice::new(at, n), &data[done..done + n])
                .is_err()
            {
                break;
            }
            done += n;
        }
        done
    }

    /// The current `RLIMIT_STACK`, which bounds every growth of a grows-down mapping.
    pub fn stack_limit(&self) -> u64 {
        self.t.group.borrow().rlimits[crate::process::RLIMIT_STACK as usize].0
    }

    fn grow_for(&self, range: UserSlice) -> bool {
        let limit = self.stack_limit();
        self.mm_mut().grow_for(range.addr, range.len, limit)
    }

    /// Syscall entry: when SP is already below the bottom of the main thread's stack (the program
    /// allocated a large stack frame but has not touched it yet), first grow the stack to SP. The
    /// on-stack buffers syscalls use are all above SP (arm64 has no red zone), so the kernel
    /// accessing them does not get EFAULT, as on Linux. When SP is within the stack (the common
    /// case) this is a single comparison.
    pub fn grow_stack_to(&self, sp: u64) {
        if sp >= self.mm().stack_low() {
            return;
        }
        let limit = self.stack_limit();
        // Other threads' stacks are in the mmap area, much farther from the main thread's stack:
        // no need to check
        if crate::loader::STACK_TOP - sp <= limit {
            self.mm_mut().grow_down(sp, limit);
        }
    }

    /// fd table.
    pub fn fds(&self) -> Ref<'_, FdTable> {
        self.t.fds.borrow()
    }

    /// Mutable fd table.
    pub fn fds_mut(&self) -> RefMut<'_, FdTable> {
        self.t.fds.borrow_mut()
    }

    /// Root filesystem; without a rootfs the guest sees no files.
    pub fn vfs(&self) -> Result<Rc<Vfs>, Errno> {
        self.t.vfs.clone().ok_or(Errno::ENOENT)
    }

    /// Current directory.
    pub fn cwd(&self) -> Node {
        self.t.fs.borrow().cwd.clone()
    }

    /// Root directory (chroot).
    pub fn root(&self) -> Node {
        self.t.fs.borrow().root.clone()
    }

    /// The mount namespace's id.
    pub fn mnt_ns(&self) -> u64 {
        self.t.fs.borrow().mnt_ns.id
    }

    /// Replaces the address space (execve), also updating the copy procfs sees.
    pub fn set_mm(&mut self, mm: Rc<RefCell<Mm>>) {
        self.task.res.borrow_mut().mm = mm.clone();
        self.t.mm = mm;
    }

    /// Replaces the fd table (when execve or close_range stops sharing it), also updating the
    /// copy procfs sees.
    pub fn set_fds(&mut self, fds: Rc<RefCell<FdTable>>) {
        self.task.res.borrow_mut().fds = fds.clone();
        self.t.fds = fds;
    }

    /// Replaces the root directory, current directory and umask (unshare), also updating the copy
    /// procfs sees.
    pub fn set_fs(&mut self, fs: Rc<RefCell<crate::task::FsContext>>) {
        self.task.res.borrow_mut().fs = fs.clone();
        self.t.fs = fs;
        self.shared.fast_changed();
    }

    /// The procfs and devpts view (for path resolution).
    pub fn view<'v>(&'v self, vfs: &'v Vfs) -> crate::pseudo::View<'v> {
        crate::pseudo::View {
            k: self.kr(),
            vfs,
            tid: self.tid(),
            tgid: self.tgid(),
            cred: None,
        }
    }

    /// The offset (nanoseconds) of clock `clock` in the thread's time namespace.
    pub fn time_offset(&self, clock: i32) -> i64 {
        self.task.ns.borrow().time.offset(clock)
    }

    /// The UTS, IPC, network, cgroup and time namespaces it is in ([`Task::ns`]).
    pub fn ns(&self) -> crate::nsproxy::NsProxy {
        self.task.ns.borrow().clone()
    }

    /// The objects in its IPC namespace.
    pub fn ipc(&self) -> &crate::ipc::Ipc {
        let id = self.task.ns.borrow().ipc.id;
        self.kr().ipc(id)
    }

    pub fn ipc_mut(&mut self) -> &mut crate::ipc::Ipc {
        let id = self.task.ns.borrow().ipc.id;
        self.k().ipc_mut(id)
    }

    /// The pid namespace it is in.
    pub fn pid_ns(&self) -> Rc<crate::pidns::PidNs> {
        self.t.group.borrow().pid_ns.clone()
    }

    /// The pid namespace new child processes go into.
    pub fn pid_ns_children(&self) -> Rc<crate::pidns::PidNs> {
        let ns = self.task.pid_ns_children.borrow().clone();
        ns.unwrap_or_else(|| self.pid_ns())
    }

    /// The number of global number `g` in its own pid namespace, 0 when it cannot be seen (the pid
    /// returned to the guest).
    pub fn pid_out(&self, g: Tid) -> Tid {
        self.pid_ns().vnr(g).unwrap_or(0)
    }

    /// The global number for a pid given by the guest (a number in its own pid namespace); one
    /// that cannot be seen becomes a number that does not exist; 0 and negatives are unchanged
    /// (Linux's `find_vpid`).
    pub fn pid_in(&self, l: Tid) -> Tid {
        if l <= 0 {
            return l;
        }
        self.pid_ns().find(l).unwrap_or(crate::syscall::NO_PID)
    }

    /// The current thread's credentials.
    pub fn cred(&self) -> crate::cred::Cred {
        self.task.cred.borrow().clone()
    }

    /// Replaces the current thread's credentials (without the side effects of Linux's
    /// `commit_creds`, which `syscall::cred` adds): the main thread's are also the process's
    /// ([`ThreadGroup::cred`]). The no-BKL path records them ([`Shared::fast_changed`]).
    pub fn set_cred(&self, cred: crate::cred::Cred) {
        if self.tid() == self.tgid() {
            self.t.group.borrow_mut().cred = cred.clone();
        }
        *self.task.cred.borrow_mut() = cred;
        self.shared.fast_changed();
    }

    /// AF_UNIX `SO_PEERCRED`: the pid and the current thread's effective user and group IDs
    /// (kernel IDs, Linux's `cred_to_ucred`).
    pub fn ucred(&self) -> crate::unix::Ucred {
        let c = self.task.cred.borrow();
        crate::unix::Ucred::of(self.tgid(), c.euid, c.egid)
    }

    /// The `SCM_CREDENTIALS` attached when none is declared: the pid and the current thread's real
    /// user and group IDs (Linux's `maybe_add_creds` uses `current_uid_gid`).
    pub fn scm_cred(&self) -> crate::unix::Ucred {
        let c = self.task.cred.borrow();
        crate::unix::Ucred::of(self.tgid(), c.ruid, c.rgid)
    }

    /// File-creation mask.
    pub fn umask(&self) -> u32 {
        self.t.fs.borrow().umask
    }

    /// Whether there is a deliverable signal, or the thread has been asked to exit.
    pub fn interrupted(&self) -> bool {
        crate::signal::interrupted(self)
    }

    /// Releases the BKL and waits, see [`block`]. No shared kernel object may be borrowed while
    /// waiting.
    pub fn wait(
        &mut self,
        deadline: Option<Instant>,
        host: &mut [(i32, i16, i16)],
    ) -> Result<(), Errno> {
        self.wait_on(deadline, host, wake::ALL)
    }

    /// Waits only for a direct notification (futex wake, signal, being ended) or a timeout;
    /// [`request_wake`] does not wake it. The caller must make sure that every place that makes
    /// its condition true notifies `self.task.event` directly.
    pub fn wait_alone(&mut self, deadline: Option<Instant>) -> Result<(), Errno> {
        self.wait_on(deadline, &mut [], 0)
    }

    /// The same as [`Cx::wait`], but woken only by the channels in `interest` ([`wake`]). The
    /// caller must make sure that the changes that make its condition true all request these
    /// channels.
    pub fn wait_on(
        &mut self,
        deadline: Option<Instant>,
        host: &mut [(i32, i16, i16)],
        interest: u32,
    ) -> Result<(), Errno> {
        self.wait_for(deadline, host, interest, None)
    }

    /// Waits, with no deadline, for a change on object `object` (a pipe, the kernel log)
    /// ([`request_wake_object`]), a change that requests [`wake::FILE`] or all channels, or a
    /// direct notification.
    pub fn wait_object(&mut self, object: wake::Object) -> Result<(), Errno> {
        self.wait_for(None, &mut [], wake::FILE, Some(&[object]))
    }

    /// The same as [`Cx::wait`], but changes that affect only objects ([`request_wake_object`])
    /// wake it only when the object is one in `objects`. The caller must make sure that each change
    /// that makes its condition true either requests one of these objects or requests some
    /// channel in `interest` ([`request_wake_on`], [`request_wake`]).
    pub fn wait_objects(
        &mut self,
        deadline: Option<Instant>,
        host: &mut [(i32, i16, i16)],
        interest: u32,
        objects: &[wake::Object],
    ) -> Result<(), Errno> {
        self.wait_for(deadline, host, interest, Some(objects))
    }

    fn wait_for(
        &mut self,
        deadline: Option<Instant>,
        host: &mut [(i32, i16, i16)],
        interest: u32,
        objects: Option<&[wake::Object]>,
    ) -> Result<(), Errno> {
        let k = self.k.take().expect("BKL is held");
        self.task.sleeping.store(true, Ordering::SeqCst);
        let relock = Relock(self);
        let cx = &mut *relock.0;
        let event = &cx.task.event;
        let (k, r) = block_for(cx.shared, k, event, deadline, host, interest, objects);
        cx.k = Some(k);
        r
    }

    /// Runs `f` (host I/O that may block) with the BKL released. `f` must not access any kernel
    /// object: it is `Send`, so it cannot capture `Rc`, `RefCell` or references to them.
    pub fn unlocked<R>(&mut self, f: impl FnOnce() -> R + Send) -> R {
        crate::cell::assert_none_held();
        self.task.sleeping.store(true, Ordering::SeqCst);
        drop(self.k.take());
        let _relock = Relock(self);
        f()
    }
}

/// A [`Cx`] that has released the BKL ([`Cx::wait_on`] and the like, [`Cx::unlocked`]): on drop,
/// takes the BKL if it has not been taken again yet. On a normal return that is just re-taking
/// the lock; the same happens on a panic while the lock is released (a kernel-layer bug,
/// [`crate::bug`]): the `Rc`s (vfs, open files...) dropped by the outer frames the unwinding
/// passes through must be dropped while holding the BKL, because their counts are not atomic and
/// would be changed together with a thread holding the BKL at the same time; a lost update frees
/// them early.
struct Relock<'c, 'a>(&'c mut Cx<'a>);

impl Drop for Relock<'_, '_> {
    fn drop(&mut self) {
        if self.0.k.is_none() {
            self.0.k = Some(self.0.shared.lock());
        }
        self.0.task.sleeping.store(false, Ordering::SeqCst);
    }
}

// ---- Waiting and waking ----

/// Releases the BKL and waits: re-takes the BKL and returns when the event is signalled
/// ([`request_wake`], signals), a host fd in `host` becomes ready, or `deadline` is reached. The
/// caller re-checks its own condition. `interest` is the channels of interest ([`wake`]); when it
/// is 0 the waiter is not put on the list [`request_wake`] wakes, and wakes only when `event` is
/// signalled directly (futex).
pub(crate) fn block<'a>(
    shared: &'a Shared,
    k: Bkl<'a>,
    event: &Arc<Event>,
    deadline: Option<Instant>,
    host: &mut [(i32, i16, i16)],
    interest: u32,
) -> (Bkl<'a>, Result<(), Errno>) {
    block_for(shared, k, event, deadline, host, interest, None)
}

/// [`block`]; when `objects` is not None, changes that affect only other objects
/// ([`request_wake_object`]) do not wake it.
pub(crate) fn block_for<'a>(
    shared: &'a Shared,
    mut k: Bkl<'a>,
    event: &Arc<Event>,
    deadline: Option<Instant>,
    host: &mut [(i32, i16, i16)],
    interest: u32,
    objects: Option<&[wake::Object]>,
) -> (Bkl<'a>, Result<(), Errno>) {
    const POLLIN: i16 = 1;
    crate::cell::assert_none_held();
    let listed = interest != 0;
    if listed {
        let objects = objects.map(|o| {
            let mut o = Box::<[wake::Object]>::from(o);
            o.sort_unstable();
            o
        });
        k.waiters.push((event.clone(), interest, objects));
    }
    drop(k);
    let timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
    let mut fds: Vec<(i32, i16, i16)> = host.iter().map(|&(fd, ev, _)| (fd, ev, 0)).collect();
    fds.push((event.fd(), POLLIN, 0));
    let r = tuxport_hal::poll(&mut fds, timeout);
    let mut k = shared.lock();
    event.clear();
    if listed {
        k.waiters.retain(|(e, ..)| !Arc::ptr_eq(e, event));
    }
    for (h, f) in host.iter_mut().zip(&fds) {
        h.2 = f.2;
    }
    let r = match r {
        Ok(_) => Ok(()),
        // A host signal interrupted poll (a debugger, say): treat it as an ordinary wake-up
        Err(e) if e == Errno::EINTR.0 => Ok(()),
        Err(e) => Err(Errno(e)),
    };
    (k, r)
}

/// Requests that all waiting threads be woken when the BKL is released. The caller holds the BKL.
pub(crate) fn request_wake() {
    note_wake(wake::ALL, None);
}

/// A file lock, lease or open-file position lock was released or changed: wakes only the waiters
/// interested in [`wake::LOCK`]. The caller holds the BKL.
pub(crate) fn request_wake_locks() {
    request_wake_on(wake::LOCK);
}

/// Requests that, when the BKL is released, the waiters interested in any channel in `channels`
/// ([`wake`]) be woken. The caller holds the BKL.
pub(crate) fn request_wake_on(channels: u32) {
    note_wake(channels, None);
}

/// A change that affects only object `object` (data in a pipe, an eventfd's count, a PTY's input
/// and output, messages in an AF_UNIX channel...): wakes only the waiters that registered this
/// object ([`Cx::wait_object`], [`Cx::wait_objects`]), plus [`wake::FILE`] waiters that named no
/// objects. Every pipe read or write of a shell used to wake the processes reading other pipes
/// (the shell outside `$(...)`, `tail`), and poll and epoll waiters were also woken by every such
/// change. The caller holds the BKL.
pub(crate) fn request_wake_object(object: wake::Object) {
    note_wake(wake::OBJECTS, Some(object));
}

/// Records a wake request ([`request_wake`] and the like): on this thread while holding the BKL,
/// otherwise (which should not happen) in the global fallback.
fn note_wake(channels: u32, object: Option<wake::Object>) {
    if HELD.get() != 0 {
        PENDING.with_borrow_mut(|(c, objects)| {
            *c |= channels;
            if let Some(o) = object
                && !objects.contains(&o)
            {
                objects.push(o);
            }
        });
        return;
    }
    if let Some(o) = object {
        let mut objects = WAKE_OBJECTS.lock().unwrap_or_else(PoisonError::into_inner);
        if !objects.contains(&o) {
            objects.push(o);
        }
    }
    WAKE.fetch_or(channels, Ordering::Relaxed);
}

/// Signals `event` only after the BKL is released. The woken thread will take the BKL right away:
/// woken before the release, it would only go back to sleep waiting for the lock, and would need
/// waking once more when this side releases it (every time a shell waits for a child). The caller
/// holds the BKL.
pub(crate) fn signal_later(event: &Arc<Event>) {
    SIGNALS.with_borrow_mut(|s| {
        if !s.iter().any(|e| Arc::ptr_eq(e, event)) {
            s.push(event.clone());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::Tier;

    /// Tids run up to `PID_MAX - 1`, never `PID_MAX`, and then start again from 300, as after a
    /// write of `ns_last_pid`.
    #[test]
    fn tids_wrap_to_300_below_pid_max() {
        let mut k = Kernel::new(Tier::Interpreter);
        k.set_last_pid(PID_MAX - 2);
        assert_eq!(k.alloc_tid(), Ok(PID_MAX - 1));
        assert_eq!(k.last_pid(), PID_MAX - 1);
        assert_eq!(k.alloc_tid(), Ok(crate::pidns::RESERVED_PIDS));
        assert_eq!(k.last_pid(), crate::pidns::RESERVED_PIDS);
        // Nothing took 300 (no task was created): after the largest number it is free again
        k.set_last_pid(PID_MAX);
        assert_eq!(k.alloc_tid(), Ok(crate::pidns::RESERVED_PIDS));
    }

    /// Two kernels in one host process: wake-ups requested while A holds its lock are not taken by
    /// B releasing its lock in the meantime ([`PENDING`]). They used to be global; B's release
    /// cleared them, and A's waiters slept forever.
    #[test]
    fn wakeups_stay_with_their_kernel() {
        let a = Shared::new(Kernel::new(Tier::Interpreter));
        let b = Shared::new(Kernel::new(Tier::Interpreter));
        let event = Arc::new(Event::new().unwrap());
        a.lock().waiters.push((event.clone(), wake::LOCK, None));
        // Releasing the lock may also have processed requests other tests made without the BKL
        // (the global fallback), so count from here
        event.clear();
        let k = a.lock();
        request_wake_on(wake::LOCK);
        std::thread::scope(|s| {
            s.spawn(|| drop(b.lock())).join().unwrap();
        });
        assert!(!event.is_signaled());
        drop(k);
        assert!(event.is_signaled());
    }
}
