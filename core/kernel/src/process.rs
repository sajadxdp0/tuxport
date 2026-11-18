//! Guest processes (thread groups) (the kernel/process map). Processes started by the host are
//! all children of the built-in init (pid 1) ([`crate::Machine`]).

use std::fmt;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use tuxport_engine::DbtStats;

use crate::cell::RefCell;
use crate::cred::{Cred, MAY_EXEC};
use crate::errno::Errno;
use crate::kernel::{INIT, Kernel, Tid, request_wake_on, wake};
use crate::loader::{self, Executable, LoadError};
use crate::signal::{self, Pending, SigAction, SigHandlers, SigInfo, sa};
use crate::terminal::Terminal;
use crate::tty::Tty;
use crate::vfs::overlay::{Hold, OvlId};
use crate::vfs::store::{Ino, Stat, mode};
use crate::vfs::{Dentry, MountId, Node, Pseudo, Vfs};

/// A signal number (1–64).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signal(i32);

impl Signal {
    /// Hangup.
    pub const HUP: Signal = Signal(1);
    /// Terminal interrupt.
    pub const INT: Signal = Signal(2);
    /// Terminal quit (^\).
    pub const QUIT: Signal = Signal(3);
    /// Undefined instruction.
    pub const ILL: Signal = Signal(4);
    /// Breakpoint.
    pub const TRAP: Signal = Signal(5);
    /// abort.
    pub const ABRT: Signal = Signal(6);
    /// Bus error, unaligned access.
    pub const BUS: Signal = Signal(7);
    /// Forced termination.
    pub const KILL: Signal = Signal(9);
    /// User-defined signal 1.
    pub const USR1: Signal = Signal(10);
    /// User-defined signal 2.
    pub const USR2: Signal = Signal(12);
    /// Invalid memory access.
    pub const SEGV: Signal = Signal(11);
    /// Broken pipe.
    pub const PIPE: Signal = Signal(13);
    /// Timer expired.
    pub const ALRM: Signal = Signal(14);
    /// Termination.
    pub const TERM: Signal = Signal(15);
    /// Child process status changed.
    pub const CHLD: Signal = Signal(17);
    /// Continues a stopped process.
    pub const CONT: Signal = Signal(18);
    /// Stop (cannot be caught).
    pub const STOP: Signal = Signal(19);
    /// Terminal stop (^Z).
    pub const TSTP: Signal = Signal(20);
    /// A background process read the terminal.
    pub const TTIN: Signal = Signal(21);
    /// A background process wrote to the terminal or changed its settings.
    pub const TTOU: Signal = Signal(22);
    /// CPU time exceeded `RLIMIT_CPU`.
    pub const XCPU: Signal = Signal(24);
    /// File size exceeded `RLIMIT_FSIZE`.
    pub const XFSZ: Signal = Signal(25);
    /// `ITIMER_VIRTUAL` expired.
    pub const VTALRM: Signal = Signal(26);
    /// `ITIMER_PROF` expired.
    pub const PROF: Signal = Signal(27);
    /// Terminal window size changed.
    pub const WINCH: Signal = Signal(28);
    /// Bad system call (seccomp).
    pub const SYS: Signal = Signal(31);

    /// The largest signal number.
    pub const MAX: i32 = 64;

    /// Constructs from a number, which must be within 1–64.
    pub fn new(n: i32) -> Option<Signal> {
        (1..=Self::MAX).contains(&n).then_some(Signal(n))
    }

    /// The Linux signal number.
    pub fn number(self) -> i32 {
        self.0
    }

    /// The corresponding bit in a signal-set bitmap.
    pub(crate) fn bit(self) -> u64 {
        1 << (self.0 - 1)
    }

    /// The index in a table ordered by signal number.
    pub(crate) fn index(self) -> usize {
        self.0 as usize - 1
    }

    /// Whether this is a real-time signal (from 32; libc uses up the first few itself, so its
    /// SIGRTMIN is larger).
    pub(crate) fn is_realtime(self) -> bool {
        self.0 >= 32
    }

    /// Whether this is a stop signal (SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU).
    pub(crate) fn is_stop(self) -> bool {
        matches!(self.0, 19..=22)
    }

    /// Whether the default action is to ignore: SIGCHLD, SIGCONT (its "continue" already took
    /// effect when it was generated), SIGURG, SIGWINCH.
    pub(crate) fn ignored_by_default(self) -> bool {
        matches!(self.0, 17 | 18 | 23 | 28)
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const NAMES: [&str; 31] = [
            "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "BUS", "FPE", "KILL", "USR1", "SEGV",
            "USR2", "PIPE", "ALRM", "TERM", "STKFLT", "CHLD", "CONT", "STOP", "TSTP", "TTIN",
            "TTOU", "URG", "XCPU", "XFSZ", "VTALRM", "PROF", "WINCH", "IO", "PWR", "SYS",
        ];
        match self.0 {
            n @ 1..=31 => write!(f, "SIG{}", NAMES[n as usize - 1]),
            n => write!(f, "SIGRT{}", n - 32),
        }
    }
}

/// How a guest process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// Called `exit` or `exit_group`.
    Exited(i32),
    /// Terminated by a signal.
    Signaled {
        /// The signal.
        signal: Signal,
        /// For a synchronous exception (memory fault, undefined instruction, breakpoint), the
        /// address of the faulting instruction.
        pc: Option<u64>,
        /// For a memory fault, the guest address.
        fault_addr: Option<u64>,
        /// A core file was written (`WCOREDUMP`).
        core_dumped: bool,
    },
}

impl ExitStatus {
    /// The status word wait4 reports (the format decoded by `WIFEXITED` and the other macros).
    pub(crate) fn wait_status(self) -> i32 {
        match self {
            ExitStatus::Exited(code) => (code & 0xff) << 8,
            ExitStatus::Signaled {
                signal,
                core_dumped,
                ..
            } => signal.number() | if core_dumped { 0x80 } else { 0 },
        }
    }
}

/// Execution engine (the engine map).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    /// Tier 0: the interpreter.
    #[default]
    Interpreter,
    /// Tier 1: a same-ISA pass-through DBT (only on aarch64 Linux hosts).
    Dbt,
}

/// Run options.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Execution engine.
    pub tier: Tier,
    /// Host directory of the inode store. When set, program paths are resolved as guest paths in
    /// the store; otherwise they are read as host paths, and the guest sees no filesystem at all.
    pub rootfs: Option<PathBuf>,
    /// When set, the first process gets an in-process PTY as stdin/stdout/stderr and controlling
    /// terminal, with the PTY connected to these host fds; otherwise the guest uses the host's
    /// stdin/stdout/stderr directly.
    pub terminal: Option<Terminal>,
    /// Upper bound on the host process's anonymous memory, see
    /// [`crate::MachineOptions::memory_limit`].
    pub memory_limit: Option<u64>,
    /// Offset for low-port mapping, see [`crate::MachineOptions::low_port_offset`].
    pub low_port_offset: Option<u16>,
    /// See [`crate::MachineOptions::always_map_low_ports`].
    pub always_map_low_ports: bool,
    /// Boot from inittab first ([`crate::Machine::boot`]), and shut down
    /// ([`crate::Machine::shutdown`]) when the first process ends or the guest requests a
    /// shutdown, instead of ending all processes directly.
    pub init: bool,
    /// A host fd from which each byte read is a signal the host process was asked to stop with
    /// (`tuxport-run`'s handlers for SIGINT, SIGTERM and SIGHUP write them): the first is passed on
    /// to the first process, one at least a second later ends every process. Either way the run then ends as usual,
    /// with the shutdown, the final store sync and the host terminal restored.
    pub stop: Option<i32>,
}

/// The result of a run.
#[derive(Debug, Clone, Copy)]
pub struct RunReport {
    /// How the first guest process ended.
    pub status: ExitStatus,
    /// The sum of all threads' statistics when Tier 1 is used.
    pub dbt_stats: Option<DbtStats>,
}

/// Why the guest could not start running.
#[derive(Debug)]
pub enum RunError {
    /// Reading the program file on the host failed.
    Io(io::Error),
    /// Opening the inode store failed.
    Rootfs(Errno),
    /// The program could not be found or executed in the guest filesystem.
    Exec(Errno),
    /// Loading failed.
    Load(LoadError),
    /// The console does not exist, or already belongs to a session.
    Console(Errno),
    /// The current host does not support the chosen execution engine.
    TierUnavailable,
    /// A kernel-layer bug (panic), reported on the host's stderr (design doc §12 item 5).
    Internal,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Io(err) => err.fmt(f),
            RunError::Rootfs(errno) => write!(f, "cannot open the rootfs: {errno}"),
            RunError::Exec(errno) if *errno == Errno::ENOENT => {
                f.write_str("no such file or directory")
            }
            RunError::Exec(errno) if *errno == Errno::EACCES => f.write_str("permission denied"),
            RunError::Exec(errno) => write!(f, "cannot execute: {errno}"),
            RunError::Load(err) => err.fmt(f),
            RunError::Console(errno) => write!(f, "cannot use the console: {errno}"),
            RunError::TierUnavailable => {
                f.write_str("the selected tier is not supported on this host")
            }
            RunError::Internal => f.write_str("internal error"),
        }
    }
}

impl std::error::Error for RunError {}

/// personality(2)'s `READ_IMPLIES_EXEC`: readable mappings are made executable too (by mmap,
/// mprotect and brk), except those that may never execute (Linux's `VM_MAYEXEC`).
pub(crate) const READ_IMPLIES_EXEC: u32 = 0x040_0000;
/// The personality flags a privileged execve clears (Linux's `PER_CLEAR_ON_SETID`):
/// `READ_IMPLIES_EXEC`, `ADDR_NO_RANDOMIZE`, `ADDR_COMPAT_LAYOUT` and `MMAP_PAGE_ZERO`.
pub(crate) const PER_CLEAR_ON_SETID: u32 = READ_IMPLIES_EXEC | 0x004_0000 | 0x020_0000 | 0x010_0000;

/// `RLIMIT_CPU` (seconds).
pub(crate) const RLIMIT_CPU: u32 = 0;
/// `RLIMIT_FSIZE`.
pub(crate) const RLIMIT_FSIZE: u32 = 1;
/// `RLIMIT_STACK`.
pub(crate) const RLIMIT_STACK: u32 = 3;
/// `RLIMIT_CORE`.
pub(crate) const RLIMIT_CORE: u32 = 4;
/// `RLIMIT_NPROC`: the most threads one real user ID may have.
pub(crate) const RLIMIT_NPROC: u32 = 6;
/// `RLIMIT_NOFILE`.
pub(crate) const RLIMIT_NOFILE: u32 = 7;
/// `RLIMIT_MEMLOCK`.
pub(crate) const RLIMIT_MEMLOCK: u32 = 8;
/// `RLIMIT_SIGPENDING`.
pub(crate) const RLIMIT_SIGPENDING: u32 = 11;
/// Bytes of POSIX message queues a user may have (Linux's `MQ_BYTES_MAX`, 819200 by default).
pub(crate) const RLIMIT_MSGQUEUE: u32 = 12;
/// `RLIMIT_NICE`: the lowest nice value reachable without `CAP_SYS_NICE`, recorded as 20 - nice.
pub(crate) const RLIMIT_NICE: u32 = 13;
/// `RLIMIT_RTPRIO`.
const RLIMIT_RTPRIO: u32 = 14;
/// The number of resource kinds (`RLIM_NLIMITS`).
pub(crate) const RLIM_NLIMITS: usize = 16;

/// The smallest value `threads-max` accepts (Linux's `MIN_THREADS`).
pub(crate) const MIN_THREADS: usize = 20;

/// A guest process, that is, a thread group. After its threads exit, the thread group may remain
/// as a zombie process until the parent reaps it.
pub(crate) struct ThreadGroup {
    /// pid.
    pub tgid: Tid,
    /// Parent process.
    pub ppid: Tid,
    /// The thread in the parent that created it (Linux's `real_parent`; with `CLONE_PARENT`, the
    /// caller's parent thread): after that thread exits it becomes a live thread of the same
    /// process, and on reparenting a thread of the new parent.
    /// `/proc/<pid>/task/<tid>/children` lists a thread's children by it.
    pub parent_thread: Tid,
    /// Process group.
    pub pgid: Tid,
    /// Session.
    pub sid: Tid,
    /// Live threads, the first being the leader (if it is still alive).
    pub threads: Vec<Tid>,
    /// Child processes (zombies included).
    pub children: Vec<Tid>,
    /// The pid namespace it is in (all its threads are in it).
    pub pid_ns: Rc<crate::pidns::PidNs>,
    /// The cgroup it is in (a [`crate::cgroup`] number): follows the parent at fork, migrated
    /// through `cgroup.procs`.
    pub cgroup: u64,
    /// A synchronous signal with default handling has been force-delivered: the process ends as
    /// usual even if it is pid 1 of a pid namespace (Linux's `force_sig_info_to_task` clears
    /// `SIGNAL_UNKILLABLE`).
    pub killable: bool,
    /// Signals sent to the whole process and not yet handled.
    pub pending: Pending,
    /// Signal dispositions.
    pub sighand: Rc<RefCell<SigHandlers>>,
    /// Resource limits: (soft limit, hard limit).
    pub rlimits: [(u64, u64); RLIM_NLIMITS],
    /// How it ends while exiting as a whole (exit_group, fatal signal).
    pub exiting: Option<ExitStatus>,
    /// How the leader itself ended, a fallback for abnormal cleanup; on a normal exit the last
    /// thread to enter the exit path decides `exiting` (task::Task::begin_exit).
    pub leader_status: ExitStatus,
    /// How it ended, once all threads have exited and it waits for the parent to reap it.
    pub zombie: Option<ExitStatus>,
    /// The main thread has exited (while other threads run on, or with the whole process): its
    /// name and own CPU time, so that procfs shows it as a zombie thread until the process is
    /// reaped, as Linux keeps the leader as a zombie task. None while it runs, after a non-main
    /// thread's execve took its place, and for the virtual init, which never had one.
    pub leader_exit: Option<([u8; 16], CpuTime)>,
    /// The signal sent to the parent on exit (the low 8 bits of clone, usually SIGCHLD), 0 meaning
    /// none.
    pub exit_signal: i32,
    /// Controlling terminal.
    pub ctty: Option<Rc<RefCell<Tty>>>,
    /// The foreground process group when the controlling terminal hung up (Linux's
    /// `tty_old_pgrp`), recorded only on the session leader: when it exits it no longer has a
    /// controlling terminal, so SIGHUP and SIGCONT go to this process group instead
    /// ([`release_ctty`]). Cleared when it gets a controlling terminal again.
    pub tty_old_pgrp: Option<Tid>,
    /// The whole thread group has been stopped by a stop signal.
    pub stopped: bool,
    /// Its cgroup or an ancestor of it is frozen (`cgroup.freeze`): threads stop before returning
    /// to the guest; unlike a stop signal, the parent and wait do not see it.
    pub frozen: bool,
    /// The stop signal that stopped the thread group (Linux's `JOBCTL_STOP_SIGMASK`), meaningful
    /// while stopped: it is still remembered after the parent's wait has taken the stop event, and
    /// traced threads report group-stop with it while stopped.
    pub stop_signal: Signal,
    /// A "stopped" event not yet taken by the parent's wait.
    pub stop_report: Option<Signal>,
    /// A "continued" event not yet taken by the parent's wait.
    pub cont_report: bool,
    /// The program file being executed (`/proc/<pid>/exe`); None for one that has not executed a
    /// program yet (the built-in init).
    pub exe: Option<Rc<ExeFile>>,
    /// Creation time, in clock ticks since boot (`/proc/<pid>/stat`).
    pub start_ticks: u64,
    /// CPU time used by exited threads.
    pub cpu: CpuTime,
    /// CPU time used by reaped children.
    pub child_cpu: CpuTime,
    /// Peak resident memory (kB, `ru_maxrss`): sampled at getrusage, execve and exit, and kept
    /// across execve.
    pub maxrss_kb: u64,
    /// The largest `maxrss_kb` among reaped children (including those they reaped).
    pub child_maxrss_kb: u64,
    /// When getrusage last sampled ([`ThreadGroup::sample_rss_throttled`]).
    pub rss_sampled: Option<std::time::Instant>,
    /// `ITIMER_REAL`: not inherited by fork, kept by execve.
    pub itimer: Option<crate::itimer::ITimer>,
    /// `ITIMER_VIRTUAL`, `ITIMER_PROF`, timed by the CPU time the process uses.
    pub cpu_itimers: [Option<crate::itimer::CpuTimer>; 2],
    /// `PR_SET_CHILD_SUBREAPER`: orphaned descendants are reparented to the nearest such ancestor
    /// instead of init ([`finish_group`]); not inherited by fork, kept by execve.
    pub child_subreaper: bool,
    /// `PR_SET_DUMPABLE`, inherited by fork.
    pub dumpable: bool,
    /// The user namespace of the address space (Linux's `mm->user_ns`): the credentials' at
    /// execve, or the nearest ancestor where the owner and group of a program file (or
    /// interpreter) the caller could not read are mapped (`would_dump`); kept by fork. A process
    /// that is not dumpable can be traced only with `CAP_SYS_PTRACE` over it. None until the first
    /// execve: the credentials' namespace.
    pub mm_user_ns: Option<Rc<crate::cred::UserNs>>,
    /// `PR_SET_NO_NEW_PRIVS`, kept by both fork and execve.
    pub no_new_privs: bool,
    /// `/proc/<pid>/oom_score_adj` (-1000–1000), inherited by fork. Used when the OOM killer picks
    /// a process ([`crate::oom`]); -1000 means never pick it.
    pub oom_score_adj: i32,
    /// The lowest value `oom_score_adj` can be set to without `CAP_SYS_RESOURCE`: the value
    /// written by a writer with that capability, inherited by fork.
    pub oom_score_adj_min: i32,
    /// The main thread's credentials (each thread has its own, [`crate::task::Task::cred`]): what
    /// other processes go by (signal permission, ptrace access, `/proc/<pid>`, wait's `si_uid`).
    /// Changed along with the main thread's ([`crate::kernel::Cx::set_cred`]), kept after it ends,
    /// and set from the thread that calls execve, which takes its place.
    pub cred: Cred,
    /// The execution domain and flags of personality(2), kept by fork and by execve except that
    /// every execve clears [`READ_IMPLIES_EXEC`] (arm64's `SET_PERSONALITY`) and a privileged one
    /// [`PER_CLEAR_ON_SETID`]. Of the flags only `READ_IMPLIES_EXEC` has an effect
    /// (`syscall::mem`); the rest are recorded.
    pub personality: u32,
    /// Has called execve (the parent can no longer setpgid it); a forked process starts at false.
    pub execed: bool,
    /// `PR_SET_THP_DISABLE` (there are no transparent huge pages, only recorded), inherited by
    /// fork.
    pub thp_disable: bool,
    /// `/proc/<pid>/coredump_filter` (`mm::dump_filter`), inherited by fork.
    pub coredump_filter: u32,
    /// Writing a core file: other threads leave their registers here when they exit
    /// (`crate::coredump`).
    pub core: Option<Vec<crate::coredump::ThreadCore>>,
    /// POSIX timers (timer_create), by id; not inherited by fork, deleted on execve.
    pub posix_timers: std::collections::BTreeMap<i32, crate::itimer::PosixTimer>,
    /// AIO contexts, by id (`syscall::aio`).
    pub aio: std::collections::BTreeMap<u64, Rc<RefCell<crate::syscall::AioContext>>>,
    /// The process's serial number, never repeated: after a pid is reused by a new process it
    /// tells the old and new process apart (the inode of `/proc/<pid>`).
    pub serial: u64,
    /// The process tracing the leader when the leader exited: it is also reported to when the
    /// whole process ends ([`crate::ptrace`]).
    pub leader_tracer: Option<Tid>,
    /// A tracer that was tracing the leader at the end and is not the parent: until it takes this
    /// end with wait (or exits itself), only it can see this zombie process, and the parent can
    /// neither wait for it nor get notified (as with Linux's zombies that are still traced).
    pub exit_tracer: Option<Tid>,
    /// The leader's seccomp filter chain when it ended: as on Linux, released only when the
    /// process is reaped ([`crate::seccomp`]).
    pub leader_seccomp: Option<crate::seccomp::Chain>,
}

impl ThreadGroup {
    pub fn new(tgid: Tid, ppid: Tid, sighand: Rc<RefCell<SigHandlers>>) -> ThreadGroup {
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        ThreadGroup {
            serial: SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            tgid,
            ppid,
            parent_thread: ppid,
            pgid: tgid,
            sid: tgid,
            threads: Vec::new(),
            children: Vec::new(),
            pid_ns: crate::pidns::PidNs::init(),
            cgroup: crate::cgroup::ROOT,
            killable: false,
            pending: Pending::default(),
            sighand,
            rlimits: default_rlimits(),
            exiting: None,
            leader_status: ExitStatus::Exited(0),
            zombie: None,
            leader_exit: None,
            exit_signal: Signal::CHLD.number(),
            ctty: None,
            tty_old_pgrp: None,
            stopped: false,
            frozen: false,
            stop_signal: Signal::STOP,
            stop_report: None,
            cont_report: false,
            exe: None,
            start_ticks: crate::pseudo::uptime_ns() / (1_000_000_000 / loader::CLOCK_TICKS),
            cpu: CpuTime::default(),
            child_cpu: CpuTime::default(),
            maxrss_kb: 0,
            child_maxrss_kb: 0,
            rss_sampled: None,
            itimer: None,
            cpu_itimers: [None; 2],
            child_subreaper: false,
            dumpable: true,
            mm_user_ns: None,
            no_new_privs: false,
            oom_score_adj: 0,
            oom_score_adj_min: 0,
            cred: Cred::root(),
            personality: 0,
            thp_disable: false,
            coredump_filter: crate::mm::dump_filter::DEFAULT,
            core: None,
            execed: false,
            posix_timers: std::collections::BTreeMap::new(),
            aio: std::collections::BTreeMap::new(),
            leader_tracer: None,
            exit_tracer: None,
            leader_seccomp: None,
        }
    }
}

impl ThreadGroup {
    /// The total of [`ThreadGroup::cpu`].
    pub fn cpu_ns(&self, k: &Kernel) -> u64 {
        self.cpu(k).total_ns
    }

    /// CPU time used: exited threads plus live threads.
    pub fn cpu(&self, k: &Kernel) -> CpuTime {
        let threads = self.threads.iter().filter_map(|t| k.tasks.get(t));
        threads.fold(self.cpu, |sum, t| sum + t.cpu())
    }

    /// What decides access to a process that is not dumpable ([`crate::cred::Cred::may_ptrace`]):
    /// whether it is dumpable, and its address space's user namespace.
    pub fn dump_scope(&self) -> (bool, Option<&crate::cred::UserNs>) {
        (self.dumpable, self.mm_user_ns.as_deref())
    }

    /// The sample before execve replaces the address space: a process that was forked, has not
    /// called execve yet and was created no more than one clock tick ago does not measure its own
    /// anonymous memory ([`crate::mm::Mm::peak_resident_pages`]). With 4 processes starting
    /// processes at once, this sample took a good deal of time: mincore waits for the host's
    /// mmap_lock while holding the BKL, while other processes are doing munmap outside the lock.
    pub fn sample_rss_at_exec(&mut self, mm: &crate::mm::Mm) {
        let now = crate::pseudo::uptime_ns() / (1_000_000_000 / loader::CLOCK_TICKS);
        let fresh = !self.execed && now <= self.start_ticks + 1;
        self.maxrss_kb = self.maxrss_kb.max(mm.peak_resident_pages(fresh) * 4);
    }

    /// The getrusage sample: within 10 ms of the last one, the last peak is reused. Sampling scans
    /// the written mappings with mincore; the JVM calls getrusage often and has a large heap, and
    /// sampling every time took more than a tenth of Maven's time. The samples at execve and exit
    /// are not throttled.
    pub fn sample_rss_throttled(&mut self, mm: &crate::mm::Mm) {
        const INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
        let now = std::time::Instant::now();
        if self.rss_sampled.is_some_and(|t| now - t < INTERVAL) {
            return;
        }
        self.rss_sampled = Some(now);
        self.sample_rss(mm);
    }

    /// Samples resident memory once and updates the peak.
    pub fn sample_rss(&mut self, mm: &crate::mm::Mm) {
        self.maxrss_kb = self.maxrss_kb.max(mm.peak_resident_pages(false) * 4);
    }
}

fn default_rlimits() -> [(u64, u64); RLIM_NLIMITS] {
    let mut r = [(u64::MAX, u64::MAX); RLIM_NLIMITS];
    r[RLIMIT_STACK as usize] = (loader::STACK_SIZE, u64::MAX);
    r[RLIMIT_CORE as usize] = (0, u64::MAX);
    let (soft, hard) = crate::fd::DEFAULT_NOFILE;
    r[RLIMIT_NOFILE as usize] = (soft as u64, hard as u64);
    // The same as the default since Linux 5.16
    r[RLIMIT_MEMLOCK as usize] = (8 << 20, 8 << 20);
    // Half of threads-max, as on Linux; root is not limited by it
    let nproc = default_threads_max() as u64 / 2;
    r[RLIMIT_NPROC as usize] = (nproc, nproc);
    // Linux computes it from the memory size (max_threads / 2); this is the value for 4 GB of
    // memory
    r[RLIMIT_SIGPENDING as usize] = (16384, 16384);
    // MQ_BYTES_MAX, as on Linux (only reported: the limit is not enforced, see kernel/ipc)
    r[RLIMIT_MSGQUEUE as usize] = (819200, 819200);
    r[RLIMIT_NICE as usize] = (0, 0);
    r[RLIMIT_RTPRIO as usize] = (0, 0);
    r
}

/// Default for `/proc/sys/kernel/threads-max`: the upper bound on the number of threads existing
/// at once, which stops fork bombs and applies to root too (`RLIMIT_NPROC` cannot limit root).
/// Every guest thread is a host thread and also takes a host fd (the eventfd for waiting), so it
/// is one per MB of physical memory (Linux: one per 128 KB), and at most half the host fd limit,
/// leaving room for open files.
pub(crate) fn default_threads_max() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| {
        let by_memory = match tuxport_hal::memory_info().0 {
            0 => 4096,
            total => (total >> 20) as usize,
        };
        let by_fds = usize::try_from(tuxport_hal::fd_limit() / 2).unwrap_or(usize::MAX);
        by_memory.min(by_fds).max(MIN_THREADS)
    })
}

/// CPU time used, of which `sys_ns` is the kernel-mode part (not more than `total_ns`, see
/// [`Task::cpu`](crate::task::Task::cpu)).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CpuTime {
    pub total_ns: u64,
    pub sys_ns: u64,
}

impl CpuTime {
    pub fn user_ns(self) -> u64 {
        self.total_ns - self.sys_ns
    }
}

impl std::ops::Add for CpuTime {
    type Output = CpuTime;
    fn add(self, o: CpuTime) -> CpuTime {
        CpuTime {
            total_ns: self.total_ns + o.total_ns,
            sys_ns: self.sys_ns + o.sys_ns,
        }
    }
}

impl std::ops::AddAssign for CpuTime {
    fn add_assign(&mut self, o: CpuTime) {
        *self = *self + o;
    }
}

/// Resource usage reported by getrusage and wait4.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Rusage {
    pub cpu: CpuTime,
    /// Peak resident memory (kB).
    pub maxrss_kb: u64,
}

impl Rusage {
    /// `struct rusage`: user and kernel CPU time and `ru_maxrss`; the other fields are 0.
    pub fn encode(&self) -> [u8; 144] {
        let mut b = [0u8; 144];
        for (at, ns) in [(0, self.cpu.user_ns()), (16, self.cpu.sys_ns)] {
            b[at..at + 8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
            b[at + 8..at + 16].copy_from_slice(&(ns % 1_000_000_000 / 1000).to_le_bytes());
        }
        b[32..40].copy_from_slice(&self.maxrss_kb.to_le_bytes());
        b
    }
}

// ---- execve ----

/// The program file a process is executing (Linux's `mm->exe_file`): reading `/proc/<pid>/exe`
/// gives its path, and following it leads straight to this file without resolving the path
/// again. After the program file is renamed, deleted or replaced (dpkg upgrades replace running
/// programs with rename), what is opened or executed through exe is still the original. Shared
/// by fork, replaced by execve.
pub struct ExeFile {
    /// The absolute path at execution, with all symbolic links resolved: musl expands the main
    /// program's `$ORIGIN` from it, and programs started through a link such as /usr/bin/java rely
    /// on it to find their libraries.
    pub path: Rc<[u8]>,
    /// The mount passed through and the inode in the store; None for a host file used in
    /// development (no store), which is followed by path.
    pub node: Option<(MountId, Ino)>,
    /// The last directory passed through and the name: the path of a program executed through exe
    /// is computed from it ([`Pseudo::follow_dentry`]).
    pub dentry: Option<Dentry>,
    /// The open host file (the object in the store): after the program file is deleted the object
    /// is deleted too, and reopening or executing relies on it ([`open_exe_file`]).
    pub host: Option<Arc<File>>,
    /// A program found through an overlay (its `node` is then None, as an overlay file's store
    /// inode must not be reached directly): the mount and the overlay node, kept while recorded,
    /// so the link leads to the node and shows ` (deleted)` once its name is removed.
    pub ovl: Option<(MountId, OvlId, Hold)>,
}

impl ExeFile {
    /// The just-executed program `file` (inode `ino` in the store). `walked` is the node and the
    /// last directory entry obtained by walking the path at execution again ([`Vfs::walk`]); when
    /// that fails (executing a deleted file through an fd), the opened node `opened` is used
    /// (absent when a script was executed: the program is the interpreter). When a deleted program
    /// is executed through another process's exe ([`crate::pseudo::PseudoNode::OpenExe`]), the
    /// mount and inode are taken over from that process's `from`. `ovl` is the overlay node the
    /// program was found as ([`ExeFile::ovl`]).
    pub(crate) fn new(
        path: Rc<[u8]>,
        walked: Option<(Node, Option<Dentry>)>,
        opened: Option<&Node>,
        from: Option<&ExeFile>,
        ino: Option<Ino>,
        file: &Executable,
        ovl: Option<(MountId, OvlId)>,
    ) -> ExeFile {
        let (node, dentry) = match (walked, opened, from) {
            (Some((Node::Store(mnt, i), found)), ..) if Some(i) == ino => (Some((mnt, i)), found),
            (Some((Node::Pseudo(..), found)), _, Some(from)) => (from.node, found),
            (_, Some(&Node::Store(mnt, i)), _) if Some(i) == ino => (Some((mnt, i)), None),
            (_, Some(Node::Pseudo(..)), Some(from)) => (from.node, from.dentry.clone()),
            _ => (None, None),
        };
        ExeFile {
            path,
            node,
            dentry,
            host: file.host(),
            ovl: ovl.map(|(mnt, o)| (mnt, o, Hold::new(o))),
        }
    }
}

/// A deleted program file ([`crate::pseudo::PseudoNode::OpenExe`], obtained by following
/// `/proc/<pid>/exe`): opened through the recorded host file, with the same checks as
/// [`open_node`]. A program from an overlay whose name was removed is run the same way, from the
/// store inode behind its overlay node and with its overlay mount's flags.
pub(crate) fn open_exe_file(
    vfs: &Vfs,
    cred: &Cred,
    exe: &ExeFile,
) -> Result<(Executable, Stat, bool), Errno> {
    let (ino, node) = match (exe.node, &exe.ovl) {
        (Some((mnt, ino)), _) => (ino, Node::Store(mnt, ino)),
        (None, Some((mnt, o, _))) => {
            let ino = vfs.overlay().real_of(*o).ok_or(Errno::ENOENT)?;
            (ino, Node::Overlay(*mnt, *o))
        }
        (None, None) => return Err(Errno::ENOENT),
    };
    let Some(host) = &exe.host else {
        return Err(Errno::ENOENT);
    };
    let st = vfs.store().stat_open(ino, host)?;
    if st.kind() != mode::S_IFREG {
        return Err(Errno::EACCES);
    }
    cred.permission(&st, MAY_EXEC)?;
    let flags = vfs.mount_flags(&node).unwrap_or(0);
    if flags & crate::vfs::ms::NOEXEC != 0 {
        return Err(Errno::EACCES);
    }
    let nosuid = flags & crate::vfs::ms::NOSUID != 0;
    let file = host
        .try_clone()
        .map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))?;
    Ok((Executable::open(file)?, st, nosuid))
}

/// Executing through an open file (fexecve, execveat's `AT_EMPTY_PATH`, `/proc/<pid>/fd/<n>`):
/// uses its host file, because the objects of deleted files and of memfds are gone and cannot be
/// opened by inode. The checks are the same as [`open_node`], with noexec and nosuid taken from
/// the mount it was opened through (a memfd goes through no mount). A file that is not regular
/// is EACCES.
pub(crate) fn open_open_file(
    vfs: &Vfs,
    cred: &Cred,
    f: &crate::fd::OpenFile,
) -> Result<(Executable, Stat, bool), Errno> {
    let crate::fd::FileKind::Regular { ino, file, .. } = &f.kind else {
        return Err(Errno::EACCES);
    };
    let st = vfs.store().stat_open(*ino, file)?;
    if st.kind() != mode::S_IFREG {
        return Err(Errno::EACCES);
    }
    cred.permission(&st, MAY_EXEC)?;
    let flags = if f.pseudo {
        0
    } else {
        vfs.mount_flags(&Node::Store(f.mnt, *ino)).unwrap_or(0)
    };
    if flags & crate::vfs::ms::NOEXEC != 0 {
        return Err(Errno::EACCES);
    }
    let nosuid = flags & crate::vfs::ms::NOSUID != 0;
    let file = file
        .try_clone()
        .map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))?;
    Ok((Executable::open(file)?, st, nosuid))
}

/// [`open_executable`], keeping only the file itself.
pub(crate) fn read_executable(
    vfs: &Vfs,
    ps: &dyn Pseudo,
    cred: &Cred,
    root: &Node,
    cwd: &Node,
    path: &[u8],
) -> Result<Executable, Errno> {
    open_executable(vfs, ps, cred, root, cwd, path).map(|(file, ..)| file)
}

/// Opens a guest file by the rules of execve: it must be a regular file in the store, and a
/// process with credentials `cred` must have execute permission. Also returns the file's
/// metadata (setuid and setgid bits).
pub(crate) fn open_executable(
    vfs: &Vfs,
    ps: &dyn Pseudo,
    cred: &Cred,
    root: &Node,
    cwd: &Node,
    path: &[u8],
) -> Result<(Executable, Stat, bool), Errno> {
    open_node(vfs, cred, &vfs.resolve(ps, cred, root, cwd, path, true)?)
}

/// Opens the file `node` to execute (a path or fd execveat has already resolved). An unfollowed
/// symbolic link is ELOOP (`AT_SYMLINK_NOFOLLOW`), and one on a noexec mount is EACCES (Linux's
/// `path_noexec`). The third item is whether it is on a nosuid mount: setuid and setgid bits and
/// file capabilities then have no effect.
pub(crate) fn open_node(
    vfs: &Vfs,
    cred: &Cred,
    node: &Node,
) -> Result<(Executable, Stat, bool), Errno> {
    // A program in an overlay runs from the store inode behind it
    let ino = match *node {
        Node::Store(_, ino) => ino,
        Node::Overlay(_, o) => vfs.overlay().node(o).real(),
        Node::Fuse(_, f) => return open_fuse(vfs, cred, node, f),
        Node::Pseudo(..) => return Err(Errno::EACCES),
    };
    let st = vfs.store().stat(ino)?;
    match st.kind() {
        mode::S_IFREG => {}
        mode::S_IFLNK => return Err(Errno::ELOOP),
        _ => return Err(Errno::EACCES),
    }
    cred.permission(&st, MAY_EXEC)?;
    let flags = vfs.mount_flags(node).unwrap_or(0);
    if flags & crate::vfs::ms::NOEXEC != 0 {
        return Err(Errno::EACCES);
    }
    let nosuid = flags & crate::vfs::ms::NOSUID != 0;
    Ok((vfs.executable(ino)?, st, nosuid))
}

/// A program in a FUSE filesystem: the checks [`open_node`] makes, with execute permission as
/// [`Vfs::fuse_permission`] decides, then a copy of the file read in this syscall (a retry reads
/// it, [`crate::vfs::fuse::Query::Program`]). Its inode number is the node's key, which no store
/// file has (the checks for programs open for writing and being executed compare them), and set-id
/// bits and file capabilities never take effect, as on the nosuid mounts fusermount makes.
fn open_fuse(
    vfs: &Vfs,
    cred: &Cred,
    node: &Node,
    id: crate::vfs::fuse::FuseId,
) -> Result<(Executable, Stat, bool), Errno> {
    use crate::vfs::fuse;
    match vfs.fuse().get(id).ok_or(Errno::ENOTCONN)?.kind() {
        mode::S_IFREG => {}
        mode::S_IFLNK => return Err(Errno::ELOOP),
        _ => return Err(Errno::EACCES),
    }
    vfs.fuse_permission(cred, id, MAY_EXEC)?;
    if vfs.mount_flags(node).unwrap_or(0) & crate::vfs::ms::NOEXEC != 0 {
        return Err(Errno::EACCES);
    }
    let mut st = fuse::cached_attr(vfs, id)?.stat();
    st.ino = fuse::key(id);
    let file = fuse::program(id).ok_or_else(|| fuse::need(fuse::Query::Program(id)))?;
    Ok((Executable::open(file)?, st, true))
}

/// The last component of a program path, truncated to 15 bytes (`comm`).
pub(crate) fn comm_of(path: &[u8]) -> [u8; 16] {
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    let n = name.len().min(15);
    let mut comm = [0u8; 16];
    comm[..n].copy_from_slice(&name[..n]);
    comm
}

// ---- Exit and reaping ----

/// Thread `from` in process `parent` exited: the children it created get thread `to` of the same
/// process as their parent thread, and move to the end of the child list (Linux's
/// `forget_original_parent` does `list_splice_tail_init` when it finds a new parent thread in
/// the same process).
pub(crate) fn hand_thread_children(k: &Kernel, parent: &RefCell<ThreadGroup>, from: Tid, to: Tid) {
    let mut p = parent.borrow_mut();
    let (moved, kept): (Vec<Tid>, Vec<Tid>) = p.children.iter().partition(|c| {
        k.groups
            .get(c)
            .is_some_and(|g| g.borrow().parent_thread == from)
    });
    for c in &moved {
        k.groups[c].borrow_mut().parent_thread = to;
    }
    p.children = kept;
    p.children.extend(moved);
}

/// After the last thread of a thread group exits: hands the children to the nearest subreaper,
/// else to pid 1 of its pid namespace, else to the built-in init, and notifies the parent or reaps
/// directly.
///
/// Each step contains kernel-layer bugs separately ([`crate::bug`]): on an error it is reported
/// and the later steps are still done. What the failing step did is lost, but nothing else is
/// affected: the parent and the host can always wait for it to end, and the cleanup of its
/// children, its tracees, orphaned process groups and SysV IPC does not get stuck because an
/// earlier step failed.
pub(crate) fn finish_group(k: &mut Kernel, group: &Rc<RefCell<ThreadGroup>>) {
    let (tgid, pgid, sid) = {
        let g = group.borrow();
        (g.tgid, g.pgid, g.sid)
    };
    step(k, tgid, 1, "releasing AIO contexts", |k| {
        let contexts = std::mem::take(&mut group.borrow_mut().aio);
        crate::syscall::aio_released(k, contexts);
    });
    // Process groups that may become orphaned after this process exits (Linux's
    // kill_orphaned_pgrp): its own process group, when it connects another process group in the
    // same session with this process group; and those of its children that are in the same
    // session but in another process group
    let mut pgrps = Vec::new();
    let mut orphaned = Vec::new();
    // Pid 1 of a pid namespace exits: the processes in the namespace are all ended, no process
    // can be created there afterwards, and it counts as ended only once they have all been
    // reaped (Linux's zap_pid_ns_processes)
    let ns = group.borrow().pid_ns.clone();
    let zap = ns.level > 0 && ns.reaper.get() == Some(tgid);
    step(k, tgid, 2, "handing its children to init", |k| {
        if zap {
            ns.dead.set(true);
            for g in ns.members() {
                if g != tgid && k.groups.contains_key(&g) {
                    signal::send_to_group(k, g, Signal::KILL, SigInfo::kernel());
                }
            }
        }
        // Orphans go first to the nearest subreaper (Linux's find_new_reaper), otherwise to pid 1
        // of their pid namespace (find_child_reaper), and to init in the boot-time namespace or
        // once pid 1 has exited: its group stays registered until the namespace is empty
        // (Kernel::hold_reaper) but never waits again, and Linux's exiting pid 1 reaps them itself
        // (zap_pid_ns_processes), so the built-in init, which frees them at once, takes its place
        let subreaper = if zap {
            None
        } else {
            find_subreaper(k, &group.borrow())
        };
        let reaper = subreaper
            .or_else(|| {
                ns.reaper.get().filter(|&r| {
                    ns.level > 0 && !zap && !ns.dead.get() && k.groups.contains_key(&r)
                })
            })
            .unwrap_or(INIT);
        let reaper_thread = k
            .groups
            .get(&reaper)
            .map_or(reaper, |g| alive_thread(k, &g.borrow()));
        // The PR_SET_PDEATHSIG signals owed to the children created by the last thread (including
        // those handed to it when other threads exited)
        orphaned = pdeath_signals(k, &group.borrow().children);
        let children = std::mem::take(&mut group.borrow_mut().children);
        for c in children {
            let Some(child) = k.groups.get(&c).cloned() else {
                continue;
            };
            let zombie = {
                let mut cg = child.borrow_mut();
                cg.ppid = reaper;
                cg.parent_thread = reaper_thread;
                cg.exit_signal = Signal::CHLD.number();
                if cg.pgid != pgid && cg.sid == sid {
                    pgrps.push(cg.pgid);
                }
                // A zombie the tracer has not taken yet goes to the new parent; once the tracer
                // has taken it, the new parent reaps it
                cg.zombie.filter(|_| cg.exit_tracer.is_none())
            };
            if reaper != INIT {
                k.groups[&reaper].borrow_mut().children.push(c);
            }
            match zombie {
                Some(_) if reaper == INIT => {
                    k.groups.remove(&c);
                    k.release_pid(c);
                }
                // The new parent must reap it: notify once more, as Linux's reparent_leader does
                Some(status) => {
                    hand_to_parent(k, &child, (c, reaper, Signal::CHLD.number()), status)
                }
                None if reaper == INIT => k.groups[&INIT].borrow_mut().children.push(c),
                None => {}
            }
        }
    });
    // PR_SET_PDEATHSIG: notify the children that lost their parent
    let ruid = group.borrow().cred.ruid;
    step(k, tgid, 3, "sending parent-death signals", |k| {
        send_pdeath(k, orphaned, tgid, ruid)
    });
    step(k, tgid, 4, "releasing SysV IPC", |k| {
        crate::ipc::exit(k, tgid)
    });
    step(k, tgid, 5, "releasing the controlling terminal", |k| {
        release_ctty(k, group)
    });
    if zap && ns.members().iter().any(|&g| g != tgid) {
        k.hold_reaper(ns, group.clone(), pgrps);
    } else {
        finish_reported(k, group, pgrps);
    }
}

/// The subreaper to which the orphans of the exiting process `group` are reparented: going up
/// from the parent, the nearest process in the same pid-namespace level that has set
/// `PR_SET_CHILD_SUBREAPER` and still has live threads (Linux's find_new_reaper compares pid
/// levels and does not leave the namespace). Only a process with no threads left is skipped; one
/// whose group exit has begun can still be chosen while a thread has not finished exiting.
fn find_subreaper(k: &Kernel, group: &ThreadGroup) -> Option<Tid> {
    let level = group.pid_ns.level;
    let mut p = group.ppid;
    while p != INIT {
        let a = k.groups.get(&p)?.borrow();
        if a.pid_ns.level != level {
            return None;
        }
        if a.child_subreaper && !a.threads.is_empty() {
            return Some(p);
        }
        p = a.ppid;
    }
    None
}

/// The first thread of process `g` that is alive and not being ended (Linux's
/// `find_alive_thread`, leader first); the first thread when all are being ended, and `tgid`
/// when there are no threads. It becomes the parent thread when orphans are reparented to a
/// process.
pub(crate) fn alive_thread(k: &Kernel, g: &ThreadGroup) -> Tid {
    g.threads
        .iter()
        .copied()
        .find(|t| k.tasks.get(t).is_some_and(|t| !t.killed.get()))
        .or_else(|| g.threads.first().copied())
        .unwrap_or(g.tgid)
}

/// The signals that the threads with `PR_SET_PDEATHSIG` in the child processes `children` should
/// each receive because their parent thread exited (Linux's `forget_original_parent`: one per such
/// thread, sent to its process).
pub(crate) fn pdeath_signals(k: &Kernel, children: &[Tid]) -> Vec<(Tid, Signal)> {
    let mut out = Vec::new();
    for c in children {
        let Some(g) = k.groups.get(c) else {
            continue;
        };
        for t in &g.borrow().threads {
            if let Some(sig) = k.tasks.get(t).and_then(|t| Signal::new(t.pdeathsig.get())) {
                out.push((*c, sig));
            }
        }
    }
    out
}

/// Sends the [`pdeath_signals`] signals: as on Linux they are `SI_USER`, the sender is the parent
/// process `parent` (whose real user ID is `ruid`), its pid is 0 when the child's pid namespace
/// cannot see it, and they are handled as forced signals.
pub(crate) fn send_pdeath(k: &Kernel, signals: Vec<(Tid, Signal)>, parent: Tid, ruid: u32) {
    for (c, sig) in signals {
        let Some(g) = k.groups.get(&c) else {
            continue;
        };
        let uid = g.borrow().cred.uid_out(ruid);
        let info = SigInfo::kill(signal::code::SI_USER, k.pid_seen_by(c, parent), uid).from(parent);
        signal::send_to_group(k, c, sig, info);
    }
}

/// The session leader exits: the controlling terminal (always a PTY here) no longer belongs to this
/// session, and the foreground process group gets SIGHUP (Linux's `disassociate_ctty`). If the
/// controlling terminal has already hung up, SIGHUP and SIGCONT go to the foreground process group
/// at hangup:
/// when a tmux window is closed, a command started by `sh -c` is in the same process group as an
/// sh that does not exec (dash), and when the group leader sh gets SIGHUP and exits, they get
/// SIGHUP.
fn release_ctty(k: &Kernel, group: &Rc<RefCell<ThreadGroup>>) {
    let (tgid, sid, ctty, old_pgrp) = {
        let mut g = group.borrow_mut();
        (g.tgid, g.sid, g.ctty.clone(), g.tty_old_pgrp.take())
    };
    if sid != tgid {
        return;
    }
    let Some(tty) = ctty else {
        if let Some(pgrp) = old_pgrp {
            signal::send_to_pgrp(k, pgrp, Signal::HUP, SigInfo::kernel());
            signal::send_to_pgrp(k, pgrp, Signal::CONT, SigInfo::kernel());
        }
        return;
    };
    let fg = {
        let mut t = tty.borrow_mut();
        if t.session != Some(sid) {
            return;
        }
        t.session = None;
        t.fg.take()
    };
    for g in k.groups.values() {
        let mut g = g.borrow_mut();
        if g.sid == sid {
            g.ctty = None;
        }
    }
    // Only SIGHUP: Linux's disassociate_ctty sends SIGCONT too only when giving up the terminal
    // with TIOCNOTTY, or when it has already hung up (above)
    if let Some(fg) = fg {
        signal::send_to_pgrp(k, fg, Signal::HUP, SigInfo::kernel());
    }
}

/// The second half of [`finish_group`]: reporting to the tracer and the parent. Pid 1 of a pid
/// namespace does this half only once the other processes in the namespace have all been reaped
/// ([`Kernel::release_pid`]).
pub(crate) fn finish_reported(
    k: &mut Kernel,
    group: &Rc<RefCell<ThreadGroup>>,
    mut pgrps: Vec<Tid>,
) {
    let (tgid, ppid, pgid, sid, status, exit_signal) = {
        let g = group.borrow();
        let status = g.exiting.unwrap_or(g.leader_status);
        (g.tgid, g.ppid, g.pgid, g.sid, status, g.exit_signal)
    };
    if let Some(slot) = k.watched.get_mut(&tgid) {
        *slot = Some(status);
    }
    // When traced by a process other than the parent, report to the tracer first, and hand it to
    // the parent after the tracer has taken it
    let traced = step(k, tgid, 6, "reporting to its tracer", |k| {
        crate::ptrace::group_finished(k, group, status)
    });
    if traced != Some(true) {
        step(k, tgid, 7, "handing it to its parent", |k| {
            // Reporting to the tracer failed: stop waiting for the tracer to take it
            group.borrow_mut().exit_tracer = None;
            hand_to_parent(k, group, (tgid, ppid, exit_signal), status);
        });
    }
    // The processes it traces are no longer traced
    step(k, tgid, 8, "releasing its tracees", |k| {
        crate::ptrace::tracer_exited(k, tgid)
    });
    // POSIX: when this exit orphans a process group that has stopped processes in it, send
    // SIGHUP and then SIGCONT, otherwise they never get a chance to continue
    step(k, tgid, 9, "signalling orphaned process groups", |k| {
        if let Some(p) = k.groups.get(&ppid) {
            let p = p.borrow();
            if p.pgid != pgid && p.sid == sid {
                pgrps.push(pgid);
            }
        }
        pgrps.sort_unstable();
        pgrps.dedup();
        for pgid in pgrps {
            let stopped = k
                .groups
                .values()
                .any(|g| g.borrow().pgid == pgid && g.borrow().stopped);
            if stopped && signal::pgrp_orphaned(k, pgid) {
                signal::send_to_pgrp(k, pgid, Signal::HUP, SigInfo::kernel());
                signal::send_to_pgrp(k, pgid, Signal::CONT, SigInfo::kernel());
            }
        }
    });
    finished(k, tgid);
}

/// Process `tgid` has ended: wakes the threads waiting for children, and the host too if it is
/// waiting for it. The other waiters need not wake: the changes they care about (closing files,
/// semaphore undo, sending signals) each request their own wake-ups.
fn finished(k: &Kernel, tgid: Tid) {
    let watched = k.watched.contains_key(&tgid);
    request_wake_on(wake::CHILD | if watched { wake::WATCHED } else { 0 });
}

/// Step `n`, `what`, of [`finish_group`]: contains kernel-layer bugs, reports on an error and
/// returns None.
fn step<R>(
    k: &mut Kernel,
    tgid: Tid,
    n: u32,
    what: &str,
    f: impl FnOnce(&mut Kernel) -> R,
) -> Option<R> {
    let r = crate::bug::catch(|| {
        crate::bug::trap_finish(tgid, n);
        f(k)
    });
    if r.is_none() {
        crate::bug::report(format_args!(
            "kernel panic while pid {tgid} was exiting ({what})"
        ));
    }
    r
}

/// The tracer has taken the ended process `group` (or exited without taking it): hands it to the
/// parent, the same as what Linux's `wait_task_zombie` and `__ptrace_detach` do with zombies
/// ([`crate::ptrace`]).
pub(crate) fn tracer_released(k: &mut Kernel, group: &Rc<RefCell<ThreadGroup>>) {
    let (ids, status) = {
        let mut g = group.borrow_mut();
        g.exit_tracer = None;
        let Some(status) = g.zombie else {
            return;
        };
        ((g.tgid, g.ppid, g.exit_signal), status)
    };
    hand_to_parent(k, group, ids, status);
    request_wake_on(wake::CHILD);
}

/// Hands an exited process (`tgid`, `ppid`, `exit_signal`) to its parent: when the parent is
/// init, is gone, or ignores SIGCHLD (SIG_IGN, SA_NOCLDWAIT), the process is reaped directly;
/// otherwise it becomes a zombie and the parent is notified.
fn hand_to_parent(
    k: &mut Kernel,
    group: &Rc<RefCell<ThreadGroup>>,
    (tgid, ppid, exit_signal): (Tid, Tid, i32),
    status: ExitStatus,
) {
    let parent = k.groups.get(&ppid).cloned();
    let reap = ppid == INIT
        || parent.as_ref().is_none_or(|p| {
            let act = p.borrow().sighand.borrow().get(Signal::CHLD);
            act.handler == SigAction::IGNORE || act.flags & sa::NOCLDWAIT != 0
        });
    if reap {
        k.groups.remove(&tgid);
        k.release_pid(tgid);
        if let Some(p) = &parent {
            p.borrow_mut().children.retain(|&c| c != tgid);
        }
    } else {
        group.borrow_mut().zombie = Some(status);
        if let (Some(sig), Some(p)) = (Signal::new(exit_signal), &parent) {
            let pid = k.pid_seen_by(ppid, tgid);
            let info = signal::exit_info(&group.borrow().cred, &p.borrow().cred, pid, status);
            signal::send_to_group(k, ppid, sig, info);
        }
    }
}
