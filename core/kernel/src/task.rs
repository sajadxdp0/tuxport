//! Guest threads (Linux's tasks) and their run loop (the kernel/process map).
//!
//! Each guest thread corresponds to one host thread (1:1). The run loop executes guest code
//! without holding the BKL, and takes the BKL to enter the kernel on a syscall, a synchronous
//! exception or a kick; a few syscalls need not take the BKL ([`crate::syscall::fast`]).

use std::cell::Cell;
use std::panic::AssertUnwindSafe;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tuxport_engine::{
    Access, Cpu, CpuState, Dbt, DbtStats, Exit, FaultKind, Kicker, PAGE_SIZE, prot,
};
use tuxport_hal::Event;

use crate::bug::Trap;
use crate::cell::RefCell;
use crate::errno::Errno;
use crate::fd::FdTable;
use crate::kernel::{Cx, Doing, Entry, Kernel, Shared, Tid, request_wake_on, wake};
use crate::mm::{CodeSpace, Generations, Mm};
use crate::process::{CpuTime, ExitStatus, Signal, ThreadGroup, Tier};
use crate::signal::{self, AltStack, Pending, SigInfo, code};
use crate::syscall::fast::Fast;
use crate::syscall::{self, Flow};
use crate::uaccess::UserSlice;
use crate::vfs::{Node, Vfs};

/// State shared without locks between the run loop and the rest of the kernel.
pub(crate) struct TaskSync {
    /// Whether the thread is running guest code (a TLB shootdown waits for it to leave).
    pub in_guest: AtomicBool,
    /// Kicks the thread's Cpu.
    pub kicker: Kicker,
    /// Why the thread is in the kernel, for `/proc/<pid>/syscall`.
    pub entry: KernelEntry,
}

/// What a thread entered the kernel with and has not yet gone back to guest code from, as Linux's
/// `task_current_syscall` sees it: the syscall number (-1 for a fault, a kick or another
/// exception), x0 to x5 and sp and pc at the entry. Published with atomics, since the no-BKL path
/// enters syscalls too, and only while the thread is in the kernel.
#[derive(Default)]
pub(crate) struct KernelEntry {
    inside: AtomicBool,
    regs: [AtomicU64; 9],
}

impl KernelEntry {
    /// The thread enters the kernel with the registers `state`, for a syscall when `svc`.
    fn enter(&self, state: &CpuState, svc: bool) {
        let x = &state.x;
        let nr = if svc { x[8] } else { u64::MAX };
        let regs = [nr, x[0], x[1], x[2], x[3], x[4], x[5], state.sp, state.pc];
        for (r, v) in self.regs.iter().zip(regs) {
            r.store(v, Ordering::Relaxed);
        }
        self.inside.store(true, Ordering::Release);
    }

    /// Linux's `forget_syscall`, which signal delivery does first: the thread is still in the
    /// kernel but no longer in a syscall, and `state`'s pc is where it goes back to (the svc
    /// again when the syscall restarts). A thread stopped by a signal shows that.
    pub fn forget_syscall(&self, state: &CpuState) {
        self.enter(state, false);
    }

    /// The thread runs guest code again.
    fn leave(&self) {
        if self.inside.load(Ordering::Relaxed) {
            self.inside.store(false, Ordering::Relaxed);
        }
    }

    /// The syscall number (`u64::MAX` outside a syscall), x0 to x5, sp and pc the thread entered
    /// the kernel with, or None while it runs guest code.
    pub fn get(&self) -> Option<[u64; 9]> {
        if !self.inside.load(Ordering::Acquire) {
            return None;
        }
        Some(std::array::from_fn(|i| {
            self.regs[i].load(Ordering::Relaxed)
        }))
    }
}

/// The root directory, current directory and file-creation mask (the part `CLONE_FS` shares),
/// plus the mount namespace. On Linux the mount namespace is in the nsproxy, but `CLONE_NEWNS`
/// cannot be combined with `CLONE_FS` and setns requires the fs not to be shared, so tasks sharing
/// one fs are always in the same mount namespace and keeping them together is equivalent.
pub(crate) struct FsContext {
    /// Root directory (chroot).
    pub root: Node,
    pub cwd: Node,
    pub umask: u32,
    pub mnt_ns: Rc<crate::vfs::MntNs>,
}

impl FsContext {
    /// Changes the current directory. The old one may have kept a detached mount in use
    /// ([`crate::vfs::released`]).
    pub fn set_cwd(&mut self, node: Node) {
        crate::vfs::released(self.cwd.mnt());
        self.cwd = node;
    }

    /// Changes the root directory, as [`FsContext::set_cwd`] does the current one.
    pub fn set_root(&mut self, node: Node) {
        crate::vfs::released(self.root.mnt());
        self.root = node;
    }
}

impl Drop for FsContext {
    /// The last thread using it exited or took another: its directories may have kept a detached
    /// mount in use.
    fn drop(&mut self) {
        crate::vfs::released(self.root.mnt());
        crate::vfs::released(self.cwd.mnt());
    }
}

/// The address space, fd table and current directory the thread is using. They really live in
/// [`TaskState`]; this is a copy other threads can read too (for procfs), kept in sync by
/// [`Cx::set_mm`](crate::kernel::Cx::set_mm) and the like when they are replaced.
pub(crate) struct TaskRes {
    pub mm: Rc<RefCell<Mm>>,
    pub fds: Rc<RefCell<FdTable>>,
    pub fs: Rc<RefCell<FsContext>>,
}

/// A guest thread. Apart from [`Task::state`], the fields are also accessed by other threads
/// (sending signals, ending the thread group), always only while holding the BKL.
pub(crate) struct Task {
    /// tid. It changes, because a non-leader thread that calls execve takes over the leader's tid.
    pub tid: Cell<Tid>,
    /// The thread's serial number, never repeated: after a tid is reused by a new thread it tells
    /// the old and new thread apart (a POSIX timer on the old one's CPU clock, `crate::itimer`).
    pub serial: u64,
    /// The thread group it belongs to.
    pub tgid: Tid,
    /// Signals sent to this thread and not yet handled.
    pub pending: RefCell<Pending>,
    /// Signal mask.
    pub sigmask: Cell<u64>,
    /// The pid namespace that new child processes go into, None meaning the same as its own
    /// (Linux's `nsproxy->pid_ns_for_children`; changed by `CLONE_NEWPID` in unshare and setns).
    /// It is here rather than in TaskState because `/proc/<pid>/ns/pid_for_children` reads it.
    pub pid_ns_children: RefCell<Option<Rc<crate::pidns::PidNs>>>,
    /// The UTS, IPC, network, cgroup and time namespaces it is in ([`crate::nsproxy`]).
    pub ns: RefCell<crate::nsproxy::NsProxy>,
    /// The original mask while sigsuspend or ppoll temporarily replaces it (Linux's
    /// `saved_sigmask` plus `TIF_RESTORE_SIGMASK`): saved in the handler's frame, or restored when
    /// there is no handler. It is here rather than in TaskState because the tracer reads and writes
    /// it (PTRACE_GETSIGMASK, SETSIGMASK).
    pub saved_sigmask: Cell<Option<u64>>,
    /// The thread has been asked to exit (whole-group exit, execve ending the other threads).
    pub killed: Cell<bool>,
    /// The thread stopped before signal delivery because its cgroup is frozen
    /// ([`crate::signal::deliver`]), or waits for its vfork child (which Linux counts as frozen
    /// too): the frozen field of `cgroup.events` looks at it.
    pub frozen: Cell<bool>,
    /// `PR_SET_PDEATHSIG`: the signal sent to this process when the thread in the parent that
    /// created this process exits, 0 meaning none ([`crate::process::pdeath_signals`]). As on
    /// Linux it is kept per thread; new threads and processes start at 0, and execve keeps it.
    pub pdeathsig: Cell<i32>,
    /// The thread's credentials, kept per thread as on Linux: libc's set*id functions change every
    /// thread's, but setfsuid and raw system calls only the caller's. A new thread or process
    /// starts with its creator's; the leader's are also the process's ([`ThreadGroup::cred`]),
    /// both changed through [`Cx::set_cred`].
    pub cred: RefCell<crate::cred::Cred>,
    /// The thread is waiting in the kernel ([`Task::asleep`], load average). Set before the BKL is
    /// released and cleared after it is taken again; waits without the BKL (`syscall::fast`) set
    /// it too.
    pub sleeping: AtomicBool,
    /// Thread name (`prctl(PR_SET_NAME)`).
    pub comm: Cell<[u8; 16]>,
    /// nice value (-20–19): as on Linux it belongs to the thread, and a new thread inherits its
    /// creator's. The host thread is set to match (`syscall::sched`).
    pub nice: Cell<i32>,
    /// Scheduling policy (including `SCHED_RESET_ON_FORK`) and real-time priority: only recorded;
    /// the host does the scheduling.
    pub sched: Cell<(i32, i32)>,
    /// I/O priority (the raw value set by ioprio_set, Linux's `io_context->ioprio`). None means no
    /// io_context yet (ioprio_get reads 0); a thread created with `CLONE_IO` shares one with its
    /// creator. The host thread is set to match (`syscall::sched`).
    pub ioprio: RefCell<Option<Rc<Cell<u16>>>>,
    /// (runtime, deadline, period) of `SCHED_DEADLINE`, in nanoseconds.
    pub deadline: Cell<(u64, u64, u64)>,
    /// The utilization clamps requested with sched_setattr (min, max, out of 1024): only recorded.
    pub uclamp: Cell<(u32, u32)>,
    /// The timer slack of `PR_SET_TIMERSLACK` and its default (nanoseconds): only recorded. For a
    /// new thread both are the parent thread's current slack; setting it to 0 restores the default.
    pub timerslack: Cell<(u64, u64)>,
    /// CPU affinity (at most 64 CPUs). The host thread is set to match (`syscall::sched`).
    pub affinity: Cell<u64>,
    /// The robust futex list head and length set by set_robust_list; a new thread starts empty.
    /// The list is processed when the thread leaves its address space at exit or execve
    /// ([`release_mm`], [`crate::futex::exit_robust_list`]).
    pub robust_list: Cell<(u64, u64)>,
    pub sync: Arc<TaskSync>,
    /// Wakes this thread while it waits.
    pub event: Arc<Event>,
    /// A shared copy of the address space and other resources (for procfs).
    pub res: RefCell<TaskRes>,
    /// The host thread's CPU-time clock.
    pub clock: Cell<Option<i32>>,
    /// The CPU time, and the kernel-mode time within it, the host thread had already used when
    /// the thread started (nanoseconds): the host thread may be left over from the pool and have
    /// run other guest threads ([`pool`]).
    pub cpu_base: Cell<(u64, u64)>,
    /// The CPU time last reported ([`Task::cpu`]): neither user nor kernel time goes below it.
    cpu_prev: Cell<CpuTime>,
    /// The host thread's voluntary and involuntary context-switch counts when the thread started
    /// (host threads are reused): `/proc/<pid>/status` reports those since then.
    pub ctxt_base: Cell<(u64, u64)>,
    /// The host thread's tid (to read the kernel-mode time the host records).
    pub host_tid: Cell<i32>,
    /// ptrace state ([`crate::ptrace`]).
    pub trace: RefCell<crate::ptrace::Trace>,
    /// The tracer resumed it with PTRACE_SYSCALL: all syscalls go to the BKL-holding path so that
    /// they can stop at entry and exit (`syscall::fast` reads it without the BKL).
    pub syscall_traced: AtomicBool,
    /// The system call the thread is in on the BKL path, as (its number in [`SYSCALLS`], its
    /// system call number); (0, 0) outside one. Read under the BKL by other threads: an overlay
    /// node removed after a thread's system call began may still be used by it
    /// (the overlay sweep in `syscall::mount`).
    pub syscall: Cell<(u64, u64)>,
    /// How the thread ended itself: the exit code of exit(2), or seccomp having it end with a
    /// signal (reported to the tracer when a traced thread exits).
    pub exit_status: Cell<ExitStatus>,
    /// Has entered the exit path (may still be stopped at PTRACE_EVENT_EXIT).
    exit_started: Cell<bool>,
    /// A vfork child whose parent still waits for it to exec or exit (`TaskState::vfork_done`
    /// pending), readable by other threads: the OOM killer passes it over while it shares its
    /// parent's address space (Linux's `in_vfork`, [`crate::oom`]).
    pub vfork: Cell<bool>,
    /// seccomp state ([`crate::seccomp`]), accessed only while holding the BKL.
    pub seccomp: RefCell<crate::seccomp::Seccomp>,
    /// The thread has seccomp: syscalls must be checked first (`syscall::fast` reads it without
    /// the BKL). Updated along with `seccomp` by [`crate::seccomp::publish`].
    pub seccomp_on: AtomicBool,
    /// The numbers for which the filter always returns ALLOW ([`crate::seccomp::always_allowed`]):
    /// these syscalls take the no-BKL path as usual. Updated along with `seccomp` by
    /// [`crate::seccomp::publish`].
    pub seccomp_allow: [AtomicU64; crate::seccomp::CACHE_WORDS],
    /// The tracer resumed it with PTRACE_SINGLESTEP: the run loop has the interpreter execute one
    /// instruction at a time ([`crate::ptrace::step_done`]); read without the BKL.
    pub stepping: AtomicBool,
    /// An svc was single-stepped: the next single step executes no instruction and reports again
    /// in place ([`crate::ptrace::syscall_stepped`]).
    pub step_again: AtomicBool,
    /// State accessed only by the thread itself.
    pub state: std::cell::RefCell<TaskState>,
}

/// System calls entered on the BKL path so far, across all kernels: each one takes the next
/// number ([`Task::syscall`]), so numbers order system calls against what happens under the BKL.
pub(crate) static SYSCALLS: AtomicU64 = AtomicU64::new(0);

/// A thread's private state, and the resources it shares with other threads.
pub(crate) struct TaskState {
    /// Address space (shared by `CLONE_VM`).
    pub mm: Rc<RefCell<Mm>>,
    /// fd table (shared by `CLONE_FILES`).
    pub fds: Rc<RefCell<FdTable>>,
    /// Current directory, umask (shared by `CLONE_FS`).
    pub fs: Rc<RefCell<FsContext>>,
    /// The thread group it belongs to.
    pub group: Rc<RefCell<ThreadGroup>>,
    /// Root filesystem.
    pub vfs: Option<Rc<Vfs>>,
    /// Alternate signal stack.
    pub altstack: AltStack,
    /// The address zeroed and futex-woken when the thread exits (`set_tid_address`,
    /// `CLONE_CHILD_CLEARTID`).
    pub clear_child_tid: u64,
    /// A vfork child: set on execve or exit, and wakes the parent directly (its event) so that it
    /// continues.
    pub vfork_done: Option<(Rc<Cell<bool>>, Arc<Event>)>,
    /// What remains of a nanosleep or timed futex wait interrupted by a signal (`restart_syscall`).
    pub restart: Option<crate::syscall::Restart>,
    /// The deadline of a no-BKL sleep or timed futex wait (`syscall::fast`) that was woken before
    /// it passed and handed over to the locked path, which goes on waiting until it without
    /// reading the time argument again, and computes what remains from it when a signal
    /// interrupts. Cleared once the locked path has dispatched that call.
    pub fast_sleep: Option<crate::itimer::Expiry>,
}

impl Task {
    /// Creates a thread for `cpu`, with its thread group's credentials.
    pub fn new(
        tid: Tid,
        tgid: Tid,
        cpu: &Cpu,
        state: TaskState,
        ns: crate::nsproxy::NsProxy,
        sigmask: u64,
        comm: [u8; 16],
    ) -> Result<Rc<Task>, Errno> {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let event = Event::new().map_err(|_| Errno::EAGAIN)?;
        let vfork = state.vfork_done.is_some();
        let cred = state.group.borrow().cred.clone();
        let res = TaskRes {
            mm: state.mm.clone(),
            fds: state.fds.clone(),
            fs: state.fs.clone(),
        };
        Ok(Rc::new(Task {
            tid: Cell::new(tid),
            serial: SERIAL.fetch_add(1, Ordering::Relaxed),
            tgid,
            pending: RefCell::default(),
            sigmask: Cell::new(sigmask),
            pid_ns_children: RefCell::default(),
            ns: RefCell::new(ns),
            saved_sigmask: Cell::new(None),
            killed: Cell::new(false),
            frozen: Cell::new(false),
            pdeathsig: Cell::new(0),
            cred: RefCell::new(cred),
            sleeping: AtomicBool::new(false),
            comm: Cell::new(comm),
            nice: Cell::new(0),
            sched: Cell::new((0, 0)),
            ioprio: RefCell::new(None),
            deadline: Cell::new((0, 0, 0)),
            uclamp: Cell::new((0, 1024)),
            timerslack: Cell::new((50_000, 50_000)),
            affinity: Cell::new(u64::MAX),
            robust_list: Cell::new((0, 0)),
            sync: Arc::new(TaskSync {
                in_guest: AtomicBool::new(false),
                kicker: cpu.kicker(),
                entry: KernelEntry::default(),
            }),
            event: Arc::new(event),
            res: RefCell::new(res),
            clock: Cell::new(None),
            cpu_base: Cell::new((0, 0)),
            cpu_prev: Cell::new(CpuTime::default()),
            ctxt_base: Cell::new((0, 0)),
            host_tid: Cell::new(0),
            trace: RefCell::default(),
            syscall_traced: AtomicBool::new(false),
            syscall: Cell::new((0, 0)),
            exit_status: Cell::new(ExitStatus::Exited(0)),
            exit_started: Cell::new(false),
            vfork: Cell::new(vfork),
            seccomp: RefCell::default(),
            seccomp_on: AtomicBool::new(false),
            seccomp_allow: [const { AtomicU64::new(0) }; crate::seccomp::CACHE_WORDS],
            stepping: AtomicBool::new(false),
            step_again: AtomicBool::new(false),
            state: std::cell::RefCell::new(state),
        }))
    }

    /// Linux's synchronize_group_exit: the last thread to enter the exit path decides the
    /// process's exit status, without overriding a status already set by exit_group or a fatal
    /// signal. It must be done before PTRACE_EVENT_EXIT, and cannot follow the order in which
    /// resources are released after the tracer lets the threads go.
    pub(crate) fn begin_exit(&self, k: &Kernel, group: &RefCell<ThreadGroup>, status: ExitStatus) {
        if self.exit_started.replace(true) {
            return;
        }
        self.exit_status.set(status);
        let mut g = group.borrow_mut();
        if g.threads
            .iter()
            .all(|tid| k.tasks.get(tid).is_none_or(|t| t.exit_started.get()))
        {
            g.exiting.get_or_insert(status);
        }
    }

    /// The thread is waiting in the kernel and has not been woken yet (state S in
    /// `/proc/<pid>/stat`). After being woken and before taking the BKL again, [`Task::sleeping`]
    /// is still set but its event has already fired: like Linux's `try_to_wake_up` making the
    /// state runnable on the spot, this counts as R. LTP synchronises on it: for example,
    /// shmctl01 signals a child stopped in pause() and, once its state is back to S, takes it
    /// that the child has done the next step.
    pub fn asleep(&self) -> bool {
        self.sleeping.load(Ordering::Relaxed) && !self.event.is_signaled()
    }

    /// [`Task::cpu`] called by the thread itself: the kernel-mode time comes from getrusage, with
    /// no need to read /proc (which used to take a fifth of thread exit).
    pub fn cpu_self(&self) -> CpuTime {
        let total_ns = self.cpu_ns();
        let sys_ns = tuxport_hal::thread_system_ns_self().saturating_sub(self.cpu_base.get().1);
        self.cpu_adjust(total_ns, sys_ns)
    }

    /// The CPU time the thread has used, with kernel mode split out: the time the host kernel
    /// spends working for this host thread (forwarded I/O, page faults and so on). What the kernel
    /// layer itself does in host user mode is not counted, so it is an underestimate.
    pub fn cpu(&self) -> CpuTime {
        let total_ns = self.cpu_ns();
        let sys_ns = tuxport_hal::thread_system_ns(self.host_tid.get())
            .unwrap_or(0)
            .saturating_sub(self.cpu_base.get().1);
        self.cpu_adjust(total_ns, sys_ns)
    }

    /// The CPU time the thread has used (nanoseconds), counted by the host thread's CPU time.
    pub fn cpu_ns(&self) -> u64 {
        self.host_cpu_ns().saturating_sub(self.cpu_base.get().0)
    }

    /// The CPU time the host thread has used in total (nanoseconds).
    fn host_cpu_ns(&self) -> u64 {
        self.clock
            .get()
            .and_then(|c| tuxport_hal::clock_gettime(c).ok())
            .map_or(0, |(s, ns)| s as u64 * 1_000_000_000 + ns as u64)
    }

    /// Keeps user and kernel time each from going backwards (Linux's `cputime_adjust`). User time
    /// is the total minus kernel time, and the two come from different sources: the total is the
    /// host thread's CPU clock, accurate to the nanosecond; kernel time, when read for another
    /// thread, is only the value in `/proc` rounded to clock ticks, and the getrusage the thread
    /// reads for itself is not taken at the same moment as the total either. When kernel time
    /// suddenly gains a tick, user time would go backwards (LTP's times03).
    fn cpu_adjust(&self, total_ns: u64, sys_ns: u64) -> CpuTime {
        let prev = self.cpu_prev.get();
        if total_ns <= prev.total_ns {
            return prev;
        }
        // The previous kernel time does not exceed the previous total, so here it does not
        // exceed total_ns; if user time is not to go backwards either, kernel time grows only by
        // the increase in the total
        let mut sys_ns = sys_ns.min(total_ns).max(prev.sys_ns);
        if total_ns - sys_ns < prev.user_ns() {
            sys_ns = total_ns - prev.user_ns();
        }
        let now = CpuTime { total_ns, sys_ns };
        self.cpu_prev.set(now);
        now
    }
}

/// Starts a host thread for `task` and registers it in the kernel, the thread group and the
/// address space. The caller holds the BKL; the thread starts running only after the caller
/// releases the BKL.
pub(crate) fn start(
    shared: &Arc<Shared>,
    k: &mut Kernel,
    task: Rc<Task>,
    cpu: Cpu,
) -> Result<(), Errno> {
    let tid = task.tid.get();
    if crate::bug::fails(crate::bug::Trap::ThreadStart, crate::bug::HOST) {
        return Err(Errno::EAGAIN);
    }
    let thread_shared = shared.clone();
    // No JoinHandle is kept: at the end of a session what is waited for is kernel.tasks becoming
    // empty, and after that the thread only has its own Cpu and translation cache left to free
    pool::spawn(Box::new(move |reused| {
        tuxport_hal::set_thread_name(&format!("guest-{tid}"));
        thread_main(thread_shared, tid, cpu, reused)
    }))
    .map_err(|_| Errno::EAGAIN)?;
    {
        let st = task.state.borrow();
        let mut group = st.group.borrow_mut();
        group.threads.push(tid);
        k.cgroups.task_changed(group.cgroup);
        st.mm.borrow_mut().add_user(task.sync.clone());
    }
    k.tasks.insert(tid, task);
    Ok(())
}

/// Host thread pool: after a guest thread ends, its host thread stays for a while for the next
/// guest thread to use. A quarter of fork's time used to go into creating host threads (kernel
/// stack, thread stack, TLS), and they also had to be torn down when the thread ended.
pub(crate) mod pool {
    use std::collections::VecDeque;
    use std::sync::{Condvar, Mutex, PoisonError};
    use std::time::{Duration, Instant};

    /// The argument is whether this host thread has run other guest threads (host-thread settings
    /// such as affinity must be set again).
    type Job = Box<dyn FnOnce(bool) + Send>;

    struct Pool {
        jobs: VecDeque<Job>,
        idle: usize,
    }

    static POOL: Mutex<Pool> = Mutex::new(Pool {
        jobs: VecDeque::new(),
        idle: 0,
    });
    static READY: Condvar = Condvar::new();

    thread_local! {
        /// The number of jobs handed to idle threads while holding the BKL: they are woken after
        /// the BKL is released ([`notify_pending`]), otherwise an idle thread would wake only to
        /// go straight back to sleep waiting for the BKL.
        #[allow(clippy::missing_const_for_thread_local)] // already const; Android clippy misfires
        static PENDING: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// The most idle host threads kept.
    const MAX_IDLE: usize = 8;
    /// How long an idle host thread is kept.
    const LINGER: Duration = Duration::from_secs(5);

    /// Runs `job` on a host thread: hands it to an idle one if there is one, otherwise creates a
    /// new one. `job` must not panic.
    pub fn spawn(job: Job) -> std::io::Result<()> {
        let mut p = POOL.lock().unwrap_or_else(PoisonError::into_inner);
        if p.idle > p.jobs.len() {
            p.jobs.push_back(job);
            if crate::kernel::bkl_held() {
                PENDING.set(PENDING.get() + 1);
            } else {
                READY.notify_one();
            }
            return Ok(());
        }
        drop(p);
        std::thread::Builder::new()
            .name("guest".into())
            .spawn(move || worker(job))
            .map(drop)
    }

    /// Wakes the idle threads that were handed jobs while the BKL was held. Called after releasing
    /// the BKL.
    pub(crate) fn notify_pending() {
        for _ in 0..PENDING.replace(0) {
            READY.notify_one();
        }
    }

    fn worker(mut job: Job) {
        let mut reused = false;
        loop {
            job(reused);
            let mut p = POOL.lock().unwrap_or_else(PoisonError::into_inner);
            if p.idle >= MAX_IDLE {
                return;
            }
            p.idle += 1;
            let deadline = Instant::now() + LINGER;
            job = loop {
                if let Some(j) = p.jobs.pop_front() {
                    p.idle -= 1;
                    break j;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    p.idle -= 1;
                    return;
                }
                p = READY
                    .wait_timeout(p, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            };
            reused = true;
        }
    }
}

/// A guest thread and its thread group (weak references).
type Current = (Weak<Task>, Weak<RefCell<ThreadGroup>>);

thread_local! {
    /// The guest thread this host thread is running, and its thread group ([`current`]). Set and
    /// cleared only while holding the BKL ([`exit`]).
    #[allow(clippy::missing_const_for_thread_local)] // already const; Android clippy misfires
    static CURRENT: std::cell::RefCell<Option<Current>> = const { std::cell::RefCell::new(None) };
}

/// The caller: the guest thread this host thread is running, and its thread group. None when not
/// on a guest thread (the timer thread, a thread on which the host calls Machine). Inside a
/// syscall [`Task::state`] is already borrowed and the thread group cannot be reached, so it is
/// recorded when the thread starts.
pub(crate) fn current() -> Option<(Rc<Task>, Rc<RefCell<ThreadGroup>>)> {
    // upgrade changes the counts of the objects the weak references point at
    debug_assert!(crate::kernel::bkl_held(), "task::current without the BKL");
    CURRENT.with_borrow(|c| {
        let (task, group) = c.as_ref()?;
        Some((task.upgrade()?, group.upgrade()?))
    })
}

fn thread_main(shared: Arc<Shared>, tid: Tid, mut cpu: Cpu, reused: bool) {
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| run(&shared, tid, &mut cpu, reused)));
    crate::kernel::done();
    // CURRENT has already been cleared by exit while holding the BKL
    if r.is_err() {
        // A kernel-layer bug, and not in a place that contains it (crate::bug): the thread cannot
        // find itself, or the cleanup after an error failed in turn. State protected by the BKL
        // may already be inconsistent, so other threads must not go on
        eprintln!("tuxport: internal error in guest thread {tid}, aborting");
        std::process::abort();
    }
}

/// `reused`: the host thread has run other guest threads ([`pool`]).
fn run(shared: &Arc<Shared>, tid: Tid, cpu: &mut Cpu, reused: bool) {
    // What this thread holds the BKL for, for the watchdog (crate::watchdog)
    let entry = |task: Option<&Task>, doing, pc| Entry {
        tid,
        tgid: task.map_or(0, |t| t.tgid),
        comm: task.map_or([0; 16], |t| t.comm.get()),
        doing,
        pc,
    };
    crate::kernel::doing(entry(None, Doing::Start, cpu.state().pc));
    let k = shared.lock();
    let task = k.tasks[&tid].clone();
    let group = Rc::downgrade(&task.state.borrow().group);
    CURRENT.set(Some((Rc::downgrade(&task), group)));
    // A bug while the thread starts ends only this process, like one on entering the kernel
    // (crate::bug)
    let started = crate::bug::catch(|| {
        crate::bug::trap(Trap::Start, task.tgid);
        task.clock.set(tuxport_hal::thread_cpu_clock());
        task.cpu_base
            .set((task.host_cpu_ns(), tuxport_hal::thread_system_ns_self()));
        task.host_tid.set(tuxport_hal::gettid());
        task.ctxt_base
            .set(tuxport_hal::thread_ctxt_switches_self().unwrap_or((0, 0)));
        crate::syscall::host_started(&task, reused);
        task.state.borrow().mm.borrow().generations()
    });
    let Some(mut generations) = started else {
        let what = format_args!("kernel panic while starting the thread");
        crate::bug::kill(&k, &task, cpu.state().pc, what);
        drop(k);
        crate::kernel::doing(entry(Some(&task), Doing::Exit, cpu.state().pc));
        return exit(shared, task, cpu.state(), None);
    };
    let (sync, tier) = (task.sync.clone(), k.tier);
    // Tier 1 translations are shared by the threads in this address space, and may also be
    // shared by several address spaces of the same program (crate::mm::CodeGroup). Mm is accessed
    // only while holding the BKL; attaching (clearing the jump cache and so on) needs no BKL
    let space_of = |k: &Kernel| match tier {
        Tier::Interpreter => None,
        Tier::Dbt => task
            .state
            .borrow()
            .mm
            .borrow_mut()
            .code_space(&k.code_groups),
    };
    let attach = |space: Option<Arc<CodeSpace>>| {
        space.and_then(|s| Dbt::attach(&s.space, s.tag).map(|d| (d, s)))
    };
    let space = space_of(&k);
    let mut fast = Fast::new(shared, &task, &task.state.borrow());
    drop(k);
    let mut dbt = attach(space);
    let load = |g: &Generations| {
        let tlb = g.tlb.load(Ordering::SeqCst);
        (
            tlb,
            g.code.load(Ordering::SeqCst),
            g.space.load(Ordering::SeqCst),
        )
    };
    let mut seen = load(&generations);
    loop {
        // When every thread is waiting, the timer thread's periodic checks stop (crate::oom,
        // crate::loadavg)
        shared.resume_checks();
        // Mark first, then read the generations, pairing with Mm::sync's "change the generation
        // first, then look at the flags"
        sync.in_guest.store(true, Ordering::SeqCst);
        sync.entry.leave();
        let now = load(&generations);
        // The address space has left its shared-translation group (crate::mm::CodeGroup): switch
        // to its own translations. Leaving did a shootdown, so this thread will not run the
        // group's translations again
        if now.2 != seen.2 {
            sync.in_guest.store(false, Ordering::SeqCst);
            if dbt.take().is_some() {
                let space = space_of(&shared.lock());
                dbt = attach(space);
            }
            seen.2 = now.2;
            continue;
        }
        if now.0 != seen.0 {
            cpu.tlb_flush();
        }
        // Translations need invalidating only when code changed: invalidate page by page over the
        // missed ranges, or flush everything when the record is incomplete. Translations are
        // shared, so it is enough for the first thread that sees the new generation to do it once
        if now.1 != seen.1
            && let Some((d, space)) = &mut dbt
        {
            let mut done = space.seen.lock().unwrap();
            if *done < now.1 {
                match generations.log.lock().unwrap().since(*done, now.1) {
                    Some(ranges) => ranges
                        .iter()
                        .for_each(|&(start, len)| d.invalidate(cpu, start, len)),
                    None => d.flush(),
                }
                *done = now.1;
            }
        }
        seen = now;
        // PTRACE_SINGLESTEP: the interpreter executes only one instruction. When it is done (ic
        // ivau counts too) enter the kernel to report; an svc reports after the syscall
        // completes, and memory faults and brk deliver their own signals (crate::ptrace)
        let (exit, stepped) = if task.stepping.load(Ordering::Relaxed) {
            // This step right after single-stepping an svc executes no instruction
            // (crate::ptrace::syscall_stepped)
            let r = if task.step_again.swap(false, Ordering::Relaxed) {
                None
            } else {
                cpu.step()
            };
            match r {
                None => (Exit::Kick, true),
                Some(e @ Exit::InvalidateCode { .. }) => (e, true),
                Some(e) => (e, false),
            }
        } else {
            // With hardware breakpoints (set by ptrace) only the interpreter checks them. For
            // watchpoints, Tier 1 hands accesses to the watched pages to the interpreter
            // (CpuState::set_hw_debug)
            let exit = match &mut dbt {
                Some((d, _)) if !cpu.state().hw_breakpoints_active() => d.run(cpu),
                _ => cpu.run(),
            };
            (exit, false)
        };
        // Syscalls that can complete without the BKL (syscall::fast). While it accesses guest
        // memory it is still marked as running guest code. A kernel-layer bug ends only this
        // process, as on the locked path (crate::bug): the frames unwound through here hold no
        // Rc; it used to unwind all the way to thread_main and terminate the whole app
        let svc = matches!(exit, Exit::Svc { .. });
        sync.entry.enter(cpu.state(), svc);
        let done = if svc {
            crate::bug::catch(|| fast.syscall(shared, &task, cpu))
        } else {
            Some(false)
        };
        sync.in_guest.store(false, Ordering::SeqCst);
        match done {
            Some(true) => continue,
            Some(false) => {}
            None => {
                task.sleeping.store(false, Ordering::SeqCst);
                let nr = cpu.state().x[8];
                crate::kernel::doing(entry(Some(&task), Doing::Syscall(nr), cpu.state().pc));
                let what = format_args!("kernel panic in syscall {nr} without the BKL");
                crate::bug::kill(&shared.lock(), &task, cpu.state().pc, what);
                break;
            }
        }

        // A kernel-layer bug ends only this process (crate::bug)
        let pc = cpu.state().pc;
        let doing = match exit {
            Exit::Svc { .. } => Doing::Syscall(cpu.state().x[8]),
            Exit::Fault { .. } => Doing::Fault,
            Exit::Kick => Doing::Kick,
            _ => Doing::Exception,
        };
        crate::kernel::doing(entry(Some(&task), doing, pc));
        let entered = crate::bug::catch(|| {
            let k = shared.lock();
            let mut state = task.state.borrow_mut();
            let mut cx = Cx::new(shared, k, &task, &mut state);
            if stepped {
                crate::ptrace::step_done(&mut cx, cpu);
            }
            let flow = handle(&mut cx, cpu, exit);
            if let Flow::Exec = flow {
                generations = cx.mm().generations();
            }
            if let Flow::Exit = flow {
                crate::syscall::flush_at_exit(&mut cx);
            }
            fast.refresh(shared, cx.task, cx.t);
            flow
        });
        let flow = entered.unwrap_or_else(|| {
            task.sleeping.store(false, Ordering::SeqCst);
            let what = match exit {
                Exit::Svc { .. } => format!("kernel panic in syscall {}", cpu.state().x[8]),
                _ => format!("kernel panic while handling {exit:?}"),
            };
            crate::bug::kill(&shared.lock(), &task, pc, format_args!("{what}"));
            Flow::Exit
        });
        match flow {
            Flow::Continue => {}
            Flow::Exec => {
                seen = load(&generations);
                // The address space changed, so switch to its translations. Release the old ones
                // first: nobody uses the old address space any more, its translations go back to
                // the pool, and the new address space most likely gets them. The next exec is
                // most likely the same program (busybox in a shell script), and the kept
                // translations are checked and reused
                if dbt.take().is_some() {
                    let space = space_of(&shared.lock());
                    dbt = attach(space);
                }
            }
            Flow::Exit => break,
        }
    }
    crate::kernel::doing(entry(Some(&task), Doing::Exit, cpu.state().pc));
    exit(shared, task, cpu.state(), dbt.map(|(d, _)| d.stats()));
}

// ESR fields for synthesising esr_context (ARM ARM D17.2.37)
const ESR_IL: u64 = 1 << 25;
const ESR_WNR: u64 = 1 << 6;
const EC_IABT_LOW: u64 = 0x20;
const EC_PC_ALIGN: u64 = 0x22;
const EC_DABT_LOW: u64 = 0x24;
const EC_SP_ALIGN: u64 = 0x26;
/// Level 3 translation fault (page not mapped).
const FSC_TRANSLATION: u64 = 0x07;
/// Level 3 permission fault.
const FSC_PERMISSION: u64 = 0x0f;
/// Alignment fault.
const FSC_ALIGNMENT: u64 = 0x21;

/// Handles one guest exit.
fn handle(cx: &mut Cx<'_>, cpu: &mut Cpu, exit: Exit) -> Flow {
    if cx.task.killed.get() {
        return Flow::Exit;
    }
    // Tier 1 records ic ivau per page and invalidates the recorded page itself at dsb or isb; a
    // page still recorded when it exits for another reason is invalidated here. A thread running
    // only Tier 0 (no translator, hardware breakpoints, single-stepping) exits with
    // Exit::InvalidateCode at every ic ivau
    let ivau = std::mem::take(&mut cpu.state_mut().ivau_page);
    let pc = cpu.state().pc;
    // The signal, its siginfo, and the ESR and name of the exception in the kernel log's report of
    // it (signal::show_unhandled): Linux reports undefined instructions and debug exceptions
    // without an ESR
    let (sig, info, esr, what) = match exit {
        Exit::InvalidateCode { addr } => {
            invalidate_code(cx, &[addr & !(PAGE_SIZE - 1), ivau]);
            return signal::deliver(cx, cpu, false);
        }
        _ if ivau != 0 => {
            invalidate_code(cx, &[ivau]);
            return handle(cx, cpu, exit);
        }
        Exit::Svc { .. } => {
            let flow = syscall::dispatch(cx, cpu);
            // A deadline the no-BKL path handed over (TaskState::fast_sleep) is for this call's
            // handler only: one it never took (the call skipped or renumbered by seccomp or a
            // tracer, or failing first) must not end the thread's next timed wait early
            cx.t.fast_sleep = None;
            return flow;
        }
        Exit::Kick => return signal::deliver(cx, cpu, false),
        Exit::Undefined { insn } => {
            crate::debug::log(format_args!(
                "undefined instruction {insn:#010x} at {pc:#x}"
            ));
            let info = SigInfo::fault(code::ILL_ILLOPC, pc, ESR_IL);
            (Signal::ILL, info, 0, signal::trap::UNDEFINED)
        }
        // Linux's SIGTRAP for a breakpoint has no esr_context in the signal frame
        Exit::Breakpoint { .. } => {
            let info = SigInfo::fault(code::TRAP_BRKPT, pc, 0);
            (Signal::TRAP, info, 0, signal::trap::DEBUG)
        }
        // An engine bug (crate::bug)
        Exit::HostFault {
            signal,
            code,
            addr,
            pc: host_pc,
        } => {
            crate::bug::kill(
                cx.kr(),
                cx.task,
                pc,
                format_args!(
                    "host signal {signal} (code {code}) in the engine at {host_pc:#x}, \
                     address {addr:#x}"
                ),
            );
            return Flow::Exit;
        }
        Exit::Fault {
            addr,
            access,
            kind: FaultKind::Page,
        } => {
            // A copy-on-write page after fork: make it writable and re-execute
            if access == Access::Write && cx.mm().cow_break(addr) {
                return signal::deliver(cx, cpu, false);
            }
            // The page table may already have been fixed while waiting for the BKL (a borrower
            // returned a copy-on-write page, another thread's mprotect); re-execute as on Linux.
            // The engine reports the faulting page (the later page when an access crosses pages)
            let need = match access {
                Access::Exec => prot::EXEC,
                Access::Read => prot::READ,
                Access::Write => prot::WRITE,
            };
            if cx.mm().mmu().translate(addr, need).is_some() {
                return signal::deliver(cx, cpu, false);
            }
            // Large mappings fill entries on demand: fill them in on first access and re-execute
            if cx.mm().fault_in(addr, need) {
                return signal::deliver(cx, cpu, false);
            }
            let (ec, wnr) = match access {
                Access::Exec => (EC_IABT_LOW, 0),
                Access::Read => (EC_DABT_LOW, 0),
                Access::Write => (EC_DABT_LOW, ESR_WNR),
            };
            // A FUSE file's mapping reads its pages as they are touched (crate::mm::Fill): read
            // this one with the BKL released and re-execute. A signal arriving before the daemon
            // took the read is delivered first, and the access faults again after it; a page that
            // cannot be read is SIGBUS, as on Linux
            let wanted = cx.mm().fill_wanted(addr, need);
            let unreadable = match wanted {
                Some(w) => match crate::syscall::fill(cx, w) {
                    Ok(()) | Err(Errno::EINTR) => return signal::deliver(cx, cpu, false),
                    Err(_) => true,
                },
                None => false,
            };
            let past_eof = if unreadable {
                Some(true)
            } else {
                cx.mm_mut().fault_past_eof(addr)
            };
            if past_eof == Some(false) {
                // The file of a shared mapping grew and the new contents are mapped in: re-execute
                return signal::deliver(cx, cpu, false);
            }
            if past_eof == Some(true) {
                let esr = ec << 26 | ESR_IL | wnr | FSC_TRANSLATION;
                let info = SigInfo::fault(code::BUS_ADRERR, addr, esr);
                signal::show_unhandled(cx, cpu, Signal::BUS, esr, signal::abort_name(esr));
                signal::force(cx, Signal::BUS, info);
                return signal::deliver(cx, cpu, false);
            }
            // A grows-down mapping (the main thread's stack, MAP_GROWSDOWN) grows to here:
            // re-execute
            if cx.grow_stack(addr) {
                return signal::deliver(cx, cpu, false);
            }
            let mapped = cx.mm().is_mapped(addr);
            let (code, fsc) = if mapped {
                (code::SEGV_ACCERR, FSC_PERMISSION)
            } else {
                (code::SEGV_MAPERR, FSC_TRANSLATION)
            };
            let esr = ec << 26 | ESR_IL | wnr | fsc;
            let info = SigInfo::fault(code, addr, esr);
            (Signal::SEGV, info, esr, signal::abort_name(esr))
        }
        Exit::Fault {
            addr,
            access,
            kind: FaultKind::Alignment,
        } => {
            let (esr, what) = match access {
                Access::Exec => (EC_PC_ALIGN << 26 | ESR_IL, signal::trap::SP_PC_ALIGNMENT),
                Access::Read => {
                    let esr = EC_DABT_LOW << 26 | ESR_IL | FSC_ALIGNMENT;
                    (esr, signal::abort_name(esr))
                }
                Access::Write => {
                    let esr = EC_DABT_LOW << 26 | ESR_IL | ESR_WNR | FSC_ALIGNMENT;
                    (esr, signal::abort_name(esr))
                }
            };
            (
                Signal::BUS,
                SigInfo::fault(code::BUS_ADRALN, addr, esr),
                esr,
                what,
            )
        }
        Exit::Fault {
            addr,
            kind: FaultKind::StackAlignment,
            ..
        } => {
            let esr = EC_SP_ALIGN << 26 | ESR_IL;
            let info = SigInfo::fault(code::BUS_ADRALN, addr, esr);
            (Signal::BUS, info, esr, signal::trap::SP_PC_ALIGNMENT)
        }
        // Hardware watchpoint (the start of the access) or breakpoint (pc): the instruction has
        // not executed yet, and as in Linux's ptrace_hbptriggered there is no esr_context
        Exit::Fault {
            addr,
            kind: FaultKind::Watchpoint | FaultKind::HwBreakpoint,
            ..
        } => {
            let info = SigInfo::fault(code::TRAP_HWBKPT, addr, 0);
            (Signal::TRAP, info, 0, signal::trap::HW_BREAKPOINT)
        }
    };
    signal::show_unhandled(cx, cpu, sig, esr, what);
    signal::force(cx, sig, info);
    signal::deliver(cx, cpu, false)
}

/// Invalidates the translations of each page in `pages` (0 is skipped), and lets the other
/// threads sharing the address space know.
fn invalidate_code(cx: &mut Cx<'_>, pages: &[u64]) {
    let sync = cx.task.sync.clone();
    let mut mm = cx.mm_mut();
    for &page in pages.iter().filter(|&&p| p != 0) {
        mm.invalidate_code(page, PAGE_SIZE);
    }
    mm.sync(&sync);
}

/// Thread exit: releases its resources; when it is the last thread of the thread group, ends the
/// whole process. `pc` is the thread's last guest pc.
///
/// Each step contains kernel-layer bugs separately (crate::bug): on an error this process is
/// ended and the later steps are still done, so the thread is always deregistered from the kernel
/// and the process always ends, and its waiting parent and the host do not get stuck.
fn exit(shared: &Arc<Shared>, task: Rc<Task>, state: &CpuState, stats: Option<DbtStats>) {
    let pc = state.pc;
    let mut k = shared.lock();
    let (tid, tgid) = (task.tid.get(), task.tgid);
    let group = task.state.borrow().group.clone();
    let vfs = task.state.borrow().vfs.clone();
    task.begin_exit(&k, &group, task.exit_status.get());
    let released = crate::bug::catch(|| {
        crate::bug::trap(Trap::Exit, tid);
        // The process is writing a core file: leave the registers (crate::coredump)
        if let Some(core) = group.borrow_mut().core.as_mut() {
            core.push(crate::coredump::ThreadCore::of(&task, state));
        }
        if let Some(s) = stats {
            let total = k.dbt_stats.get_or_insert_with(DbtStats::default);
            add_stats(total, &s);
        }
        let cpu = task.cpu_self();
        k.cpu += cpu;
        group.borrow_mut().cpu += cpu;
        // The main thread: procfs goes on showing it as a zombie thread (ThreadGroup::leader_exit)
        if tid == tgid {
            group.borrow_mut().leader_exit = Some((task.comm.get(), cpu));
        }
        let mut st = task.state.borrow_mut();
        // The process's last thread: sample the peak resident memory once before exiting
        if group.borrow().threads.len() == 1 {
            group.borrow_mut().sample_rss(&st.mm.borrow());
        }
        release_mm(shared, &task, &mut st);
        // A traced thread exited: report it to the tracer (crate::ptrace)
        let status = task.exit_status.get();
        crate::ptrace::tracee_exited(&mut k, &task, &mut group.borrow_mut(), status);
        crate::seccomp::exited(&task, &mut group.borrow_mut());
    });
    if released.is_none() {
        // The stage stopped part way: what it may not have reached must still happen, or a later
        // shootdown kicks this thread's freed CPU state and a vfork parent waits for good (both
        // are idempotent)
        let left = crate::bug::catch(|| {
            let mut st = task.state.borrow_mut();
            if let Some((done, parent)) = st.vfork_done.take() {
                done.set(true);
                task.vfork.set(false);
                crate::kernel::signal_later(&parent);
            }
            st.mm.borrow_mut().remove_user(&task.sync);
        });
        if left.is_none() {
            crate::bug::report(format_args!(
                "kernel panic while thread {tid} was leaving its address space"
            ));
        }
        let what = format_args!("kernel panic while the thread was exiting");
        crate::bug::kill(&k, &task, pc, what);
    }
    k.tasks.remove(&tid);
    k.release_pid(tid);
    // PI locks this thread held with waiters go to their first waiter (exit_pi_state_list)
    if crate::bug::catch(|| crate::futex::pi_owners_exited(&k, &mut shared.futexes())).is_none() {
        crate::bug::report(format_args!(
            "kernel panic while thread {tid} handed over its PI futexes"
        ));
    }
    // Its address space's attachments no longer count: a segment marked for removal goes with
    // its last one (Linux's exit_mmap closes them)
    crate::ipc::reap_shm(&mut k);
    let last = {
        let mut g = group.borrow_mut();
        g.threads.retain(|&t| t != tid);
        k.cgroups.task_changed(g.cgroup);
        g.threads.is_empty()
    };
    // The child processes this thread created go to a live thread of the process (Linux's
    // forget_original_parent); the last thread's are reparented by finish_group
    if !last {
        let to = crate::process::alive_thread(&k, &group.borrow());
        let mine: Vec<Tid> = group
            .borrow()
            .children
            .iter()
            .copied()
            .filter(|c| {
                k.groups
                    .get(c)
                    .is_some_and(|g| g.borrow().parent_thread == tid)
            })
            .collect();
        let signals = crate::process::pdeath_signals(&k, &mine);
        crate::process::hand_thread_children(&k, &group, tid, to);
        let ruid = task.cred.borrow().ruid;
        crate::process::send_pdeath(&k, signals, tgid, ruid);
        // Signals sent to the process may have been meant for this thread: wake one that takes
        // them (Linux's exit_signals)
        let pending = group.borrow().pending.set();
        crate::signal::retarget(&k, &group, None, pending);
    }
    // When a later step fails: if other threads remain, end this process; otherwise it has
    // already ended, so only report
    let failed = |k: &Kernel, step: &str| {
        if last {
            crate::bug::report(format_args!(
                "kernel panic while pid {tgid} was exiting ({step})"
            ));
        } else {
            let what = format_args!("kernel panic while thread {tid} was exiting ({step})");
            crate::bug::kill_process(k, tgid, what);
        }
    };
    // finish_group contains bugs step by step itself; only the few lines reading process state
    // are left for this catch
    if last && crate::bug::catch(|| crate::process::finish_group(&mut k, &group)).is_none() {
        failed(&k, "finishing the process");
    }
    // The fd table goes with its last thread (each holds two references): its message-queue
    // registrations end as its descriptors close (Linux's mqueue_flush_file at exit)
    let mq_flush = || {
        let fds = task.res.borrow().fds.clone();
        if Rc::strong_count(&fds) <= 3 {
            let t = fds.borrow();
            let files: Vec<_> = t
                .open_fds()
                .into_iter()
                .filter_map(|fd| t.get(fd).ok())
                .collect();
            crate::syscall::mqueue_closed(tgid, &files);
        }
    };
    if crate::bug::catch(mq_flush).is_none() {
        failed(&k, "closing message queues");
    }
    // Release the thread's resources while holding the BKL: the address space, the fd table
    // (closing pipes wakes the other end)... The thread object may still be held elsewhere, so
    // release the namespaces first
    *task.ns.borrow_mut() = k.init_ns.clone();
    if crate::bug::catch(|| drop((group, task))).is_none() {
        failed(&k, "releasing its resources");
    }
    // FUSE releases that found their connection borrowed: sent while the BKL is held
    if crate::bug::catch(crate::vfs::fuse::flush_released).is_none() {
        failed(&k, "releasing FUSE files");
    }
    // After the fd table's unix sockets are closed, only in-flight cycles may remain
    if last
        && !k.unix_inflight.is_empty()
        && crate::bug::catch(|| crate::unix::collect(&mut k)).is_none()
    {
        failed(&k, "collecting unix sockets");
    }
    // The last thread using an IPC namespace exited: drop the objects in it
    k.collect_ipc();
    // The last thread using a mount namespace exited: take down its mounts (the tmpfs in them
    // are deleted with them); detached mounts whose last use was its files or directories go too
    if let Some(vfs) = &vfs
        && crate::bug::catch(|| crate::syscall::collect_unused(&k, vfs)).is_none()
    {
        failed(&k, "collecting mount namespaces");
    }
    // A cgroup may have lost its last process (cgroup.events' populated)
    if crate::bug::catch(|| crate::cgroup::notify_changes(&mut k)).is_none() {
        failed(&k, "notifying cgroup changes");
    }
    // Child state changes are requested separately by finish_group (wake::CHILD), the vfork
    // parent is woken by release_mm, and closed files make their own requests; here only those
    // waiting for threads to end are woken (execve, shutdown)
    request_wake_on(wake::EXITED);
    // The two weak references in the thread-local must also be dropped while holding the BKL:
    // Rc counts are not atomic, so dropping them after the release would race with a thread
    // holding the BKL at the same time (a thread just starting in the same process doing
    // Rc::downgrade) on the same count; a lost update frees it early, and later accesses all
    // land on freed memory
    CURRENT.set(None);
}

fn add_stats(total: &mut DbtStats, s: &DbtStats) {
    total.blocks += s.blocks;
    total.insns += s.insns;
    total.insns_copied += s.insns_copied;
    total.insns_remapped += s.insns_remapped;
    total.insns_memory += s.insns_memory;
    total.insns_fallback += s.insns_fallback;
    total.exits_chain += s.exits_chain;
    total.exits_lookup += s.exits_lookup;
    total.exits_interp += s.exits_interp;
    total.exits_svc += s.exits_svc;
    total.exits_kick += s.exits_kick;
    total.flushes += s.flushes;
    total.invalidated += s.invalidated;
    total.blocks_shared += s.blocks_shared;
    total.insns_grouped += s.insns_grouped;
    total.ungrouped += s.ungrouped;
}

/// The thread leaves its address space (exit, execve): processes the robust futex list, zeroes
/// `clear_child_tid` and wakes the threads waiting for it (pthread_join), lets the vfork parent
/// continue, and deregisters it from the users.
pub(crate) fn release_mm(shared: &Shared, task: &Task, st: &mut TaskState) {
    let (head, _) = task.robust_list.replace((0, 0));
    // The lock records the tid the thread itself sees (in its pid namespace)
    let tid = st.group.borrow().pid_ns.vnr(task.tid.get()).unwrap_or(0);
    let mut futexes = shared.futexes();
    crate::futex::exit_robust_list(&mut futexes, &st.mm.borrow(), head, tid as u32);
    let addr = std::mem::take(&mut st.clear_child_tid);
    {
        let mm = st.mm.borrow();
        // As in Linux's mm_release, no notification is needed when nobody else uses the address
        // space
        if addr != 0
            && mm.shared_with_others()
            && mm.write(UserSlice::new(addr, 4), &[0; 4]).is_ok()
            && let Some(key) = mm.futex_key(addr, false)
        {
            futexes.wake(key, 1, u32::MAX);
        }
    }
    drop(futexes);
    // The parent waits only for the direct notification: this used to wake every waiter, so each
    // posix_spawn (musl and glibc both use CLONE_VFORK) woke every thread reading a pipe and
    // parked in poll
    if let Some((done, parent)) = st.vfork_done.take() {
        done.set(true);
        task.vfork.set(false);
        crate::kernel::signal_later(&parent);
    }
    st.mm.borrow_mut().remove_user(&task.sync);
}
