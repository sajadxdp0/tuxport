//! Bugs of the kernel layer and the engine themselves (the kernel/process map, design doc §12
//! item 5).
//!
//! When a guest program hits such a bug, only that guest process is ended, and the other sessions
//! in the app keep running:
//!
//! - A kernel-layer panic: caught with [`catch`], after which taking the BKL ignores its poison
//!   flag ([`crate::kernel::Shared::lock`]). Where it is caught, and what is ended on an error:
//!   - every entry of a guest thread into the kernel, thread start, and each step of thread exit:
//!     the process the thread belongs to;
//!   - each step of process finish (after the last thread exits,
//!     [`crate::process::finish_group`]): the process has already ended, so it is only reported
//!     and the next step goes on;
//!   - the timer thread handling one process's timers: that process; the memory-limit check and
//!     signal-driven I/O cannot tell which process they are for, so on an error that feature is
//!     turned off;
//!   - the host calling [`crate::Machine`] methods and the library's other public functions
//!     ([`host_call`]): the call returns an error, and when the failing call is on a console, that
//!     console is hung up.
//! - The engine makes the host receive SIGSEGV, SIGBUS or SIGILL: the engine returns
//!   [`Exit::HostFault`](tuxport_engine::Exit).
//!
//! In both cases the process (here the whole thread group) is ended with SIGSEGV, as Linux's oops
//! ends the current task, and a report is written to the host's stderr (logcat on Android); the
//! panic's message and location are printed first by the panic hook, and also go into the report
//! ([`install_panic_hook`]). The state the BKL protects may be only half changed, but with the
//! faulty process ended, most inconsistencies go away with it.
//!
//! Debug builds still terminate the host process directly by default (abort on panic; host
//! faults produce a core dump by the default action), for debugging on the spot during
//! development; the environment variable `TUXPORT_ON_BUG=kill` or `abort` overrides the default.

use std::fmt;
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::OnceLock;
#[cfg(any(debug_assertions, feature = "test-hooks"))]
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

#[cfg(any(debug_assertions, feature = "test-hooks"))]
use tuxport_engine::Cpu;

#[cfg(any(debug_assertions, feature = "test-hooks"))]
use crate::errno::Errno;
#[cfg(any(debug_assertions, feature = "test-hooks"))]
use crate::kernel::Cx;
use crate::kernel::{Kernel, Tid};
use crate::process::{ExitStatus, Signal};
use crate::task::Task;

// ---- Catching panics ----

thread_local! {
    /// This thread's latest panic message and location ([`install_panic_hook`]), not yet written
    /// into a report.
    #[allow(clippy::missing_const_for_thread_local)] // already const; a false positive on Android
    static LAST_PANIC: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The most bytes of a panic message written into a report: an `assert_eq!` message carries the
/// values of both sides and can be long.
const PANIC_MESSAGE_MAX: usize = 1000;

/// Installs the panic hook (once per host process, still calling the original hook): records each
/// thread's latest panic message and location, which go into the report when a process is ended
/// or a bug reported ([`kill`], [`report`]). The report goes into the kernel log and to the app, so
/// the user can see where the error happened in `dmesg` and in the report copied for developers;
/// before, these were only printed to stderr by the default hook (logcat on Android, out of the
/// user's reach).
pub(crate) fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let payload = info.payload();
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("(not a string)");
            let mut end = message.len().min(PANIC_MESSAGE_MAX);
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            let mut text = message[..end].to_owned();
            if let Some(at) = info.location() {
                text.push_str(&format!(" at {}:{}", at.file(), at.line()));
            }
            // Do not record when the thread is exiting and its thread-locals are already gone
            let _ = LAST_PANIC.try_with(|p| {
                if let Ok(mut p) = p.try_borrow_mut() {
                    *p = Some(text);
                }
            });
            previous(info);
        }));
    });
}

/// Runs the kernel code in host call `what` (a [`crate::Machine`] method or another public
/// function of this library): on a kernel-layer panic it reports and returns None, and the caller
/// returns an error.
pub(crate) fn host_call<R>(what: &str, f: impl FnOnce() -> R) -> Option<R> {
    let r = catch(|| {
        trap(Trap::HostCall, HOST);
        f()
    });
    if r.is_none() {
        report(format_args!("kernel panic in {what}"));
    }
    r
}

/// Runs kernel code `f`, catching panics in it: returns None, leaving the cleanup to the caller
/// (ending the faulty process, etc.). When not containing ([`contain`]) it terminates the host
/// process directly. The panic's message and location have already been printed by the panic hook.
pub(crate) fn catch<R>(f: impl FnOnce() -> R) -> Option<R> {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => Some(r),
        Err(_) if contain() => None,
        Err(_) => {
            eprintln!("tuxport: internal error, aborting");
            std::process::abort()
        }
    }
}

/// Hitting a bug ends only the guest process, instead of terminating the host process.
pub(crate) fn contain() -> bool {
    static CONTAIN: OnceLock<bool> = OnceLock::new();
    *CONTAIN.get_or_init(|| match std::env::var("TUXPORT_ON_BUG").as_deref() {
        Ok("kill") => true,
        Ok("abort") => false,
        _ => !cfg!(debug_assertions),
    })
}

// ---- Ending processes and kill reports ----

/// The kernel layer ended a guest process (the app uses this to explain why in its tab; the
/// android/service map, design doc §12 item 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillReport {
    /// Process ID (in the pid namespace created at boot).
    pub pid: i32,
    /// The process's name (comm).
    pub comm: String,
    /// When the controlling terminal is one of the app's consoles, its number (as returned by
    /// `Machine::open_console`).
    pub console: Option<u32>,
    pub reason: KillReason,
}

/// Why it was ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillReason {
    /// A kernel-layer or engine bug: `report` is the report line (also written to the kernel log),
    /// and `log` is the few records before it in the kernel log plus itself (as `dmesg` writes
    /// them, one per line): whether there were other bugs, OOMs or guest-written logs before the
    /// error can then be seen in the report copied for developers. `stubs` is the paths the kernel
    /// layer does not implement that the guest reached most often since boot, one per line
    /// (`prctl option 0x59616d61: 2 calls, first by gdb`, [`crate::stub`]).
    Bug {
        report: String,
        log: Vec<String>,
        stubs: Vec<String>,
    },
    /// OOM killer: the app's memory limit (`cgroup` is None) or some cgroup's `memory.max` was not
    /// enough; usage and limit are in bytes.
    OutOfMemory {
        cgroup: Option<String>,
        used: u64,
        limit: u64,
    },
}

/// The most reports kept; the oldest is dropped when the app has not taken them in time.
const MAX_KILLS: usize = 32;

/// The report handed to the app carries this many records before it in the kernel log.
const LOG_BEFORE: u64 = 10;

/// Ends the process of the thread that hit a bug: reports `what` and ends the whole thread group
/// with SIGSEGV. `pc` is the guest pc at this thread's last entry into the kernel.
pub(crate) fn kill(k: &Kernel, task: &Task, pc: u64, what: fmt::Arguments<'_>) {
    let comm = crate::pseudo::comm_str(Some(task), task.tgid);
    let report = format!(
        "tuxport: internal error in pid {} ({comm}), thread {}, last guest pc {pc:#x}: {}; \
         killed the process with SIGSEGV",
        task.tgid,
        task.tid.get(),
        with_panic(what),
    );
    eprintln!("{report}");
    let log = log(k, &report);
    let stubs = crate::stub::top(k);
    record_kill(k, task.tgid, comm, KillReason::Bug { report, log, stubs });
    task.killed.set(true);
    segv(k, task.tgid);
}

/// Ends process `tgid`: the bug was not hit on one of its own threads (the timer thread).
pub(crate) fn kill_process(k: &Kernel, tgid: Tid, what: fmt::Arguments<'_>) {
    let task = k.groups.get(&tgid).and_then(|g| {
        let first = g.borrow().threads.first().copied();
        first.and_then(|t| k.tasks.get(&t).cloned())
    });
    let comm = crate::pseudo::comm_str(task.as_deref(), tgid);
    let report = format!(
        "tuxport: internal error in pid {tgid} ({comm}): {}; killed the process with SIGSEGV",
        with_panic(what),
    );
    eprintln!("{report}");
    let log = log(k, &report);
    let stubs = crate::stub::top(k);
    record_kill(k, tgid, comm, KillReason::Bug { report, log, stubs });
    segv(k, tgid);
}

/// Reports a bug that cannot be attributed to a guest process.
pub(crate) fn report(what: fmt::Arguments<'_>) {
    eprintln!("tuxport: internal error: {}", with_panic(what));
}

/// `what` followed by the message and location of the panic this thread just had (if any).
fn with_panic(what: fmt::Arguments<'_>) -> String {
    match take_panic() {
        Some(panic) => format!("{what}: {panic}"),
        None => what.to_string(),
    }
}

/// Takes this thread's latest panic message and location ([`install_panic_hook`]).
fn take_panic() -> Option<String> {
    LAST_PANIC
        .try_with(|p| p.try_borrow_mut().ok()?.take())
        .ok()
        .flatten()
}

/// The report of ending a process also goes into the kernel log ([`crate::klog`]), so `dmesg` in
/// the guest shows why the process ended with SIGSEGV. Linux's oops uses KERN_EMERG, since the
/// whole kernel went wrong; here only one process is ended, so KERN_ERR is used, as when the OOM
/// killer ends a process. Newlines in the report (a panic message may have some) each start a line
/// of their own.
///
/// Returns the [`LOG_BEFORE`] records before the report in the log plus the report itself
/// ([`crate::klog::Klog::lines_from`]): the report's timestamp sits with those of the records
/// before it, showing how much time passed. Empty when the log is borrowed (an error in the log's
/// own code).
fn log(k: &Kernel, report: &str) -> Vec<String> {
    let start = k.klog.try_borrow().map(|l| l.upcoming_seq()).ok();
    for line in report.lines() {
        crate::klog::printk(&k.klog, crate::klog::level::ERR, format_args!("{line}"));
    }
    start
        .and_then(|start| {
            let l = k.klog.try_borrow().ok()?;
            Some(l.lines_from(start.saturating_sub(LOG_BEFORE)))
        })
        .unwrap_or_default()
}

/// Records the report of ending process `tgid`, for the app to take.
pub(crate) fn record_kill(k: &Kernel, tgid: Tid, comm: String, reason: KillReason) {
    // On hitting a bug the thread group may be borrowed: if it cannot be borrowed, take it to have
    // no controlling terminal, and record the report all the same
    let console = k.groups.get(&tgid).and_then(|g| {
        let tty = g.try_borrow().ok()?.ctty.clone()?;
        k.consoles
            .iter()
            .find(|(_, c)| Rc::ptr_eq(&c.master.tty, &tty))
            .map(|(id, _)| *id)
    });
    let Ok(mut kills) = k.kills.try_borrow_mut() else {
        return;
    };
    if kills.len() >= MAX_KILLS {
        kills.remove(0);
    }
    kills.push(KillReport {
        pid: tgid,
        comm,
        console,
        reason,
    });
}

fn segv(k: &Kernel, tgid: Tid) {
    if let Some(group) = k.groups.get(&tgid) {
        let status = ExitStatus::Signaled {
            signal: Signal::SEGV,
            pc: None,
            fault_addr: None,
            core_dumped: false,
        };
        crate::signal::group_exit(k, group, status);
    }
}

// ---- Test hooks ----

/// Places where the test hooks can plant bugs ([`trap`]).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Trap {
    /// When a thread starts running: planted for a process (pid), whose next thread to start
    /// panics.
    Start,
    /// When a thread exits: planted for a thread (tid).
    Exit,
    /// When the timer thread handles the timers of a process (pid).
    Timers,
    /// The next time the host calls a [`crate::Machine`] method (except those that only read a
    /// table) or another public function of this library.
    HostCall,
    /// A step of process finish ([`crate::process::finish_group`]): planted for a process (pid),
    /// with the step recorded in `FINISH_STEP`.
    Finish,
    /// The thread's next no-BKL system call (`syscall::fast`): planted for a thread (tid).
    Fast,
    /// Waiting on a futex (after registering as a waiter): planted for a process (pid).
    FutexWait,
    /// Creating the host thread of a guest thread ([`crate::task::start`]): planted for
    /// [`HOST`], it fails ([`fails`]) instead of panicking, as when the host has no thread to
    /// give, whichever process the thread is for.
    ThreadStart,
}

/// Bugs planted by the test hooks: the tid or pid planted for on each [`Trap`], 0 for none.
#[cfg(any(debug_assertions, feature = "test-hooks"))]
static ARMED: [AtomicI32; 8] = [const { AtomicI32::new(0) }; 8];

/// The step [`Trap::Finish`] is planted on (counting from 1).
#[cfg(any(debug_assertions, feature = "test-hooks"))]
static FINISH_STEP: AtomicU32 = AtomicU32::new(0);

/// Host calls belong to no guest process; this number is used when planting for them.
pub(crate) const HOST: Tid = -1;

/// Step `step` of process `tgid`'s finish: panics, once only, when a test hook has planted a bug on
/// this step for it.
#[inline]
pub(crate) fn trap_finish(tgid: Tid, step: u32) {
    #[cfg(any(debug_assertions, feature = "test-hooks"))]
    if FINISH_STEP.load(Ordering::Relaxed) != step {
        return;
    }
    #[cfg(not(any(debug_assertions, feature = "test-hooks")))]
    let _ = step;
    trap(Trap::Finish, tgid);
}

/// Panics, once only, when a test hook has planted a bug on `at` for `id` (a tid, a pid or
/// [`HOST`]). Does nothing in builds without test hooks.
#[inline]
pub(crate) fn trap(at: Trap, id: Tid) {
    if fails(at, id) {
        panic!("panic injected by the test hook at {at:?}");
    }
}

/// Whether a test hook has planted a failure on `at` for `id`, true once only: for places that
/// fail with an error rather than a panic ([`Trap::ThreadStart`]). Always false in builds without
/// test hooks.
#[inline]
pub(crate) fn fails(at: Trap, id: Tid) -> bool {
    #[cfg(any(debug_assertions, feature = "test-hooks"))]
    {
        let armed = &ARMED[at as usize];
        armed.load(Ordering::Relaxed) == id
            && armed
                .compare_exchange(id, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
    }
    #[cfg(not(any(debug_assertions, feature = "test-hooks")))]
    {
        let _ = (at, id);
        false
    }
}

/// Test hook 14 is acting as a host that refuses TCP filters.
#[cfg(any(debug_assertions, feature = "test-hooks"))]
static REFUSE_TCP_FILTERS: AtomicBool = AtomicBool::new(false);

/// Whether a test hook makes the host refuse `SO_ATTACH_FILTER` on TCP sockets, as a host with
/// Linux 5d39580f68e6 does (`syscall::net`). Always false in builds without test hooks.
#[inline]
pub(crate) fn host_refuses_tcp_filters() -> bool {
    #[cfg(any(debug_assertions, feature = "test-hooks"))]
    {
        REFUSE_TCP_FILTERS.load(Ordering::Relaxed)
    }
    #[cfg(not(any(debug_assertions, feature = "test-hooks")))]
    false
}

/// The test hook's syscall number, far outside Linux's numbers. Exists only in debug builds and in
/// builds with the `test-hooks` feature (the Android debug app), and requires `TUXPORT_TEST_HOOKS=1`
/// to be set. End-to-end tests use it to simulate kernel-layer and engine bugs.
#[cfg(any(debug_assertions, feature = "test-hooks"))]
pub(crate) const TEST_SYSCALL: u64 = 0xa991;

/// Test hooks; `what` is:
///
/// 1. panic in the kernel layer at once;
/// 2. make the page holding `addr` point in the page table at an inaccessible host page, so the
///    next guest access to it makes the engine fault on the host, as if a memory access
///    instruction were mistranslated;
/// 3. panic when the calling thread exits;
/// 4. the next thread of this process to start running panics;
/// 5. panic the next time the timer thread handles this process's timers;
/// 6. panic the next time the host calls a Machine method;
/// 7. panic at step `addr` when this process finishes ([`crate::process::finish_group`]);
/// 8. panic right after releasing the BKL ([`Cx::unlocked`]). By the time unwinding gets here
///    the BKL must have been taken again, otherwise the host process is terminated: an `Rc`
///    dropped by the outer frames would change its count at the same time as other threads;
/// 9. the calling thread's next no-BKL system call (`syscall::fast`) panics;
/// 10. panic the next time this process waits on a futex, after registering as a waiter;
/// 11. act as if the host's wall clock had jumped ([`crate::itimer::clock_jumped`]: every waiting
///     thread is woken, `TFD_TIMER_CANCEL_ON_SET` timerfds are cancelled), which a test cannot
///     cause by setting the host's clock;
/// 12. hold the BKL for `addr` milliseconds, as a kernel operation that takes too long, for the
///     BKL watchdog ([`crate::watchdog`]);
/// 13. the next host thread created for a guest thread, in any process or for the host's
///     `Machine::spawn`, cannot be created ([`Trap::ThreadStart`]): the clone or spawn fails with
///     EAGAIN;
/// 14. from now on (`addr` 1) or no longer (`addr` 0), act as a host with Linux 5d39580f68e6, which
///     refuses `SO_ATTACH_FILTER` on TCP sockets to the app ([`host_refuses_tcp_filters`]), since
///     whether the host refuses depends on its kernel.
#[cfg(any(debug_assertions, feature = "test-hooks"))]
pub(crate) fn test_hook(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    what: u64,
    addr: u64,
) -> Result<u64, Errno> {
    if std::env::var_os("TUXPORT_TEST_HOOKS").is_none_or(|v| v != "1") {
        return Err(Errno::ENOSYS);
    }
    let arm = |at: Trap, id: Tid| ARMED[at as usize].store(id, Ordering::SeqCst);
    match what {
        1 => panic!("panic injected by the test hook"),
        2 => {
            cx.mm_mut().break_page(addr)?;
            cpu.tlb_flush();
        }
        3 => arm(Trap::Exit, cx.tid()),
        4 => arm(Trap::Start, cx.tgid()),
        5 => {
            arm(Trap::Timers, cx.tgid());
            cx.shared.timers.signal();
        }
        6 => {
            arm(Trap::HostCall, HOST);
            // Make host threads waiting in Machine methods check again
            crate::kernel::request_wake();
        }
        7 if addr != 0 => {
            FINISH_STEP.store(addr as u32, Ordering::SeqCst);
            arm(Trap::Finish, cx.tgid());
        }
        8 => {
            struct MustHoldBkl;
            impl Drop for MustHoldBkl {
                fn drop(&mut self) {
                    if !crate::kernel::bkl_held() {
                        eprintln!("tuxport: unwound out of Cx::unlocked without the BKL");
                        std::process::abort();
                    }
                }
            }
            let _check = MustHoldBkl;
            cx.unlocked(|| panic!("panic injected by the test hook while the BKL is released"));
        }
        9 => arm(Trap::Fast, cx.tid()),
        10 => arm(Trap::FutexWait, cx.tgid()),
        11 => crate::itimer::clock_jumped(cx.kr()),
        12 => std::thread::sleep(std::time::Duration::from_millis(addr)),
        13 => arm(Trap::ThreadStart, HOST),
        14 => REFUSE_TCP_FILTERS.store(addr != 0, Ordering::SeqCst),
        _ => return Err(Errno::EINVAL),
    }
    Ok(0)
}
