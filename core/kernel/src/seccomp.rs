//! seccomp (the kernel/cred map): at syscall entry a thread checks the syscall against its own
//! seccomp mode.
//!
//! - Strict mode (`SECCOMP_MODE_STRICT`) allows only read, write, exit and rt_sigreturn; any other
//!   syscall ends the thread with SIGKILL (Linux's `do_exit(SIGKILL)`, which ends only this
//!   thread).
//! - Filter mode (`SECCOMP_MODE_FILTER`) runs every cBPF program in the thread's filter chain on a
//!   `struct seccomp_data` (newest first) and takes the result with the strictest action
//!   ([`run_filters`]): kill the process or thread, SIGSYS (TRAP), return an error code, hand to
//!   the tracer (`PTRACE_EVENT_SECCOMP`), hand to the supervisor at the other end of the
//!   notification fd (USER_NOTIF), log and run, run.
//!
//! As on Linux, filters belong to threads: clone and fork inherit them, execve keeps them, and
//! `SECCOMP_FILTER_FLAG_TSYNC` syncs them to the other threads of the thread group. Processes
//! after fork share their ancestors' filters, so filters form a tree, and each thread sees one
//! chain going up from its own newest filter ([`Chain`]). The check comes after the PTRACE_SYSCALL
//! entry stop (arm64's `syscall_trace_enter`); the clock-reading stubs in the vDSO are not
//! checked: on Linux they do not enter the kernel.
//!
//! Programs are checked as on Linux: first by the cBPF rules (`bpf_check_classic`), then by
//! seccomp's (`seccomp_check_filter`: only aligned words of `seccomp_data` may be read, no packet
//! reads). Execution uses a simple interpreter with the same results as Linux's JIT: shifts are
//! taken modulo 32, and division by 0 makes the program return 0.
//!
//! As on Linux, results are cached by number (`seccomp_cache`): when a filter is installed, the
//! program is simulated knowing only the number and the architecture ([`const_allow`]); the
//! numbers it always returns ALLOW for are recorded in a bitmap, ANDed with the previous filter's
//! bitmap. These syscalls no longer run the programs on the chain. Threads keep an atomic copy of
//! the bitmap ([`publish`]), so the no-BKL fast path (`syscall::fast`) handles these numbers as
//! usual.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tuxport_engine::Cpu;
use tuxport_hal::Event;

use crate::cell::RefCell;
use crate::errno::Errno;
use crate::fd::{FileKind, OpenFile, oflags};
use crate::kernel::{Cx, Kernel, Tid, request_wake_object, signal_later, wake};
use crate::process::{ExitStatus, Signal};
use crate::signal::{self, InfoFields, SigInfo};
use crate::task::Task;
use crate::uaccess::UserSlice;

/// seccomp modes (`Seccomp` in `/proc/<pid>/status`, PR_GET_SECCOMP).
pub(crate) mod mode {
    pub const DISABLED: u32 = 0;
    pub const STRICT: u32 = 1;
    pub const FILTER: u32 = 2;
    /// After a filter asked for this thread to be killed (Linux's `SECCOMP_MODE_DEAD`): entering
    /// the kernel again ends it with SIGKILL.
    pub const DEAD: u32 = 3;
}

/// A filter's return value: the high 16 bits are the action, the low 16 bits the attached data.
/// Actions compare as signed numbers, and smaller is stricter.
mod ret {
    pub const KILL_PROCESS: u32 = 0x8000_0000;
    pub const KILL_THREAD: u32 = 0;
    pub const TRAP: u32 = 0x0003_0000;
    pub const ERRNO: u32 = 0x0005_0000;
    pub const USER_NOTIF: u32 = 0x7fc0_0000;
    pub const TRACE: u32 = 0x7ff0_0000;
    pub const LOG: u32 = 0x7ffc_0000;
    pub const ALLOW: u32 = 0x7fff_0000;
    pub const ACTION_FULL: u32 = 0xffff_0000;
    pub const DATA: u32 = 0xffff;
}

/// `SECCOMP_FILTER_FLAG_*`.
mod flag {
    pub const TSYNC: u32 = 1;
    pub const LOG: u32 = 2;
    /// Do not apply the speculative-execution mitigations (on the host, out of Tuxport's control):
    /// only checked.
    pub const SPEC_ALLOW: u32 = 4;
    pub const NEW_LISTENER: u32 = 8;
    pub const TSYNC_ESRCH: u32 = 16;
    pub const WAIT_KILLABLE_RECV: u32 = 32;
    pub const MASK: u32 =
        TSYNC | LOG | SPEC_ALLOW | NEW_LISTENER | TSYNC_ESRCH | WAIT_KILLABLE_RECV;
}

/// `seccomp_data.arch` and SIGSYS's `si_arch`.
const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
/// The most instructions one program may have (`BPF_MAXINSNS`).
const MAX_INSNS: usize = 4096;
/// The instruction count of a chain after conversion to eBPF, plus an overhead of 4 for each older
/// filter (Linux's `MAX_INSNS_PER_PATH`). arm64 Linux converts cBPF to eBPF first and then checks
/// this limit.
const MAX_INSNS_PER_PATH: usize = (1 << 18) / 8;
/// The size of `struct seccomp_data`.
const DATA_SIZE: usize = 64;
/// How many numbers the per-number cache bitmap covers (on Linux, the number of arm64 syscalls;
/// 512 here, which covers every number Tuxport implements), and how many words it has.
const CACHED_NRS: usize = 512;
pub(crate) const CACHE_WORDS: usize = CACHED_NRS / 64;
/// SIGSYS's `si_code`.
const SYS_SECCOMP: i32 = 1;
/// The data of ERRNO is at most `MAX_ERRNO`.
const MAX_ERRNO: u32 = 4095;
/// Syscalls strict mode allows: read, write, exit, rt_sigreturn.
const STRICT_ALLOWED: [u64; 4] = [63, 64, 93, 139];
/// `seccomp_notif_resp.flags`: let the syscall run as usual.
const USER_NOTIF_FLAG_CONTINUE: u32 = 1;

/// An installed filter (Linux's `struct seccomp_filter`), unchanged once installed (except for the
/// notification fd's end and the user count).
pub(crate) struct Filter {
    /// The checked and rewritten program.
    prog: Box<[Insn]>,
    /// The length after Linux's conversion to eBPF (for counting chain capacity).
    converted_len: usize,
    /// Per-number cache (Linux's `seccomp_cache`): the numbers this chain (this filter and those
    /// above it) always returns ALLOW for ([`cache`]).
    allow: [u64; CACHE_WORDS],
    /// The program as given (what PTRACE_SECCOMP_GET_FILTER reads).
    orig: Box<[u8]>,
    /// `SECCOMP_FILTER_FLAG_LOG`: log every action except ALLOW.
    log: bool,
    /// `SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV`: after the notification is received, waiting for
    /// the answer is interrupted only by fatal signals.
    wait_killable_recv: bool,
    /// The thread's previous filter when this one was installed.
    prev: Option<Rc<Filter>>,
    /// The threads using it directly, plus the filters with it as `prev` that still have users
    /// (Linux's `users`): once this drops to 0, the notification fd reports POLLHUP.
    users: Cell<usize>,
    /// The state of the notification fd's end with `SECCOMP_FILTER_FLAG_NEW_LISTENER`; None when
    /// there is no notification fd or it was closed, and then USER_NOTIF syscalls return ENOSYS.
    notif: RefCell<Option<Notif>>,
}

impl Filter {
    /// The wake object of the notification fd: a notification arrives, a notification is
    /// received, no users are left.
    fn object(&self) -> wake::Object {
        wake::of(&self.notif)
    }

    /// This chain always returns ALLOW for number `nr`.
    fn always_allows(&self, nr: u64) -> bool {
        cached(|i| self.allow[i], nr)
    }

    /// The chain going up from this filter.
    fn chain(&self) -> impl Iterator<Item = &Filter> {
        std::iter::successors(Some(self), |f| f.prev.as_deref())
    }
}

/// The filter chain a thread uses; [`Chain::filter`] is the newest one installed. Holding it
/// counts as one user of that filter: copying adds one, dropping subtracts one, and a filter that
/// drops to 0 is no longer a user of its `prev`, and so on upwards (Linux's
/// `__seccomp_filter_orphan`).
pub(crate) struct Chain(Option<Rc<Filter>>);

impl Chain {
    fn filter(&self) -> &Rc<Filter> {
        self.0
            .as_ref()
            .expect("a chain holds a filter until it is dropped")
    }

    /// Hands over the filter without decreasing its user count: it becomes the new filter's
    /// `prev`, and this user is transferred to the new filter.
    fn into_filter(mut self) -> Rc<Filter> {
        self.0
            .take()
            .expect("a chain holds a filter until it is dropped")
    }
}

impl Clone for Chain {
    fn clone(&self) -> Chain {
        let f = self.filter();
        f.users.set(f.users.get() + 1);
        Chain(Some(f.clone()))
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        let Some(top) = self.0.take() else {
            return;
        };
        for f in top.chain() {
            let users = f.users.get() - 1;
            f.users.set(users);
            if users != 0 {
                break;
            }
            if let Some(n) = f.notif.borrow_mut().as_mut() {
                n.arrivals[0] += 1;
                request_wake_object(f.object());
            }
        }
    }
}

/// A thread's seccomp state ([`Task::seccomp`]), accessed only while holding the BKL. Copied on
/// clone and fork.
#[derive(Clone, Default)]
pub(crate) struct Seccomp {
    /// [`mode`].
    pub mode: u32,
    /// The filter chain in filter mode.
    chain: Option<Chain>,
}

impl Seccomp {
    /// The number of filters on the chain (`Seccomp_filters` in `/proc/<pid>/status`).
    pub fn filters(&self) -> usize {
        self.chain
            .as_ref()
            .map_or(0, |c| c.filter().chain().count())
    }
}

/// The notification fd's end (Linux's `struct notification`).
struct Notif {
    next_id: u64,
    /// The number of notifications not yet taken by RECV (Linux's semaphore). It is not decreased
    /// when a sending thread is interrupted and withdraws its notification, so the next RECV may
    /// be ENOENT, as on Linux.
    requests: u64,
    /// Notifications waiting for an answer, in order.
    list: Vec<Rc<RefCell<Knotif>>>,
    /// How many events have arrived in the read and write directions (the data generation for
    /// `EPOLLET`): new notifications and running out of users count for reading, a notification
    /// being received counts for writing.
    arrivals: [u64; 2],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Not yet taken by RECV.
    Init,
    /// Received, waiting for the answer.
    Sent,
    /// Answered (or the notification fd was closed).
    Replied,
}

/// One notification (Linux's `struct seccomp_knotif`): the thread that sent it waits for it to be
/// answered.
struct Knotif {
    id: u64,
    tid: Tid,
    /// The sending thread's event: woken when answered.
    event: Arc<Event>,
    state: State,
    data: [u8; DATA_SIZE],
    error: i32,
    val: i64,
    flags: u32,
}

/// What the notification fd's open file holds ([`FileKind::SeccompNotify`]): when the last
/// reference is closed, the notifications waiting for answers all get ENOSYS, and later
/// USER_NOTIF syscalls are ENOSYS too (Linux's `seccomp_notify_detach`).
pub(crate) struct Listener(Rc<Filter>);

impl Listener {
    /// The filter the notifications belong to. ioctl takes its Rc, so waiting for notifications
    /// does not borrow the open file (`syscall::fs::ioctl`).
    pub(crate) fn filter(&self) -> Rc<Filter> {
        self.0.clone()
    }
}

impl Listener {
    /// Ready events: readable when a notification is waiting to be received, writable when one
    /// received is waiting for an answer, POLLHUP when no users are left.
    pub fn readiness(&self) -> i16 {
        // poll(2) event bits
        const POLLIN: i16 = 0x001;
        const POLLOUT: i16 = 0x004;
        const POLLHUP: i16 = 0x010;
        const POLLRDNORM: i16 = 0x040;
        const POLLWRNORM: i16 = 0x100;
        let mut ev = 0;
        if let Some(n) = self.0.notif.borrow().as_ref() {
            for k in &n.list {
                match k.borrow().state {
                    State::Init => ev |= POLLIN | POLLRDNORM,
                    State::Sent => ev |= POLLOUT | POLLWRNORM,
                    State::Replied => {}
                }
            }
        }
        if self.0.users.get() == 0 {
            ev |= POLLHUP;
        }
        ev
    }

    /// How many events have arrived in the read and write directions ([`Notif::arrivals`]).
    pub fn arrivals(&self) -> [u64; 2] {
        self.0
            .notif
            .borrow()
            .as_ref()
            .map_or([0, 0], |n| n.arrivals)
    }

    /// The wake object ([`Filter::object`]).
    pub fn object(&self) -> wake::Object {
        self.0.object()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let Some(n) = self.0.notif.borrow_mut().take() else {
            return;
        };
        for k in n.list {
            let mut k = k.borrow_mut();
            if k.state != State::Replied {
                k.state = State::Replied;
                k.error = -(Errno::ENOSYS.0);
                k.val = 0;
                signal_later(&k.event);
            }
        }
    }
}

// ---- Installing filters ----

/// seccomp(2) operations.
const SET_MODE_STRICT: u32 = 0;
const SET_MODE_FILTER: u32 = 1;
const GET_ACTION_AVAIL: u32 = 2;
const GET_NOTIF_SIZES: u32 = 3;

/// seccomp(2).
pub(crate) fn seccomp(cx: &mut Cx<'_>, op: u32, flags: u32, args: u64) -> Result<u64, Errno> {
    match op {
        SET_MODE_STRICT if flags != 0 || args != 0 => Err(Errno::EINVAL),
        SET_MODE_STRICT => set_strict(cx),
        SET_MODE_FILTER => set_filter(cx, flags, args),
        GET_ACTION_AVAIL if flags != 0 => Err(Errno::EINVAL),
        GET_ACTION_AVAIL => {
            let b: [u8; 4] = get(cx, args)?;
            match u32::from_le_bytes(b) {
                ret::KILL_PROCESS
                | ret::KILL_THREAD
                | ret::TRAP
                | ret::ERRNO
                | ret::USER_NOTIF
                | ret::TRACE
                | ret::LOG
                | ret::ALLOW => Ok(0),
                _ => Err(Errno::EOPNOTSUPP),
            }
        }
        GET_NOTIF_SIZES if flags != 0 => Err(Errno::EINVAL),
        GET_NOTIF_SIZES => {
            // Sizes of seccomp_notif, seccomp_notif_resp, seccomp_data
            let mut b = [0u8; 6];
            b[..2].copy_from_slice(&80u16.to_le_bytes());
            b[2..4].copy_from_slice(&24u16.to_le_bytes());
            b[4..].copy_from_slice(&(DATA_SIZE as u16).to_le_bytes());
            cx.mm().write(UserSlice::new(args, 6), &b)?;
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// prctl(PR_SET_SECCOMP): no flags; strict mode ignores `filter`.
pub(crate) fn prctl_set(cx: &mut Cx<'_>, mode: u64, filter: u64) -> Result<u64, Errno> {
    match mode {
        1 => set_strict(cx),
        2 => set_filter(cx, 0, filter),
        _ => Err(Errno::EINVAL),
    }
}

/// Enters strict mode. A thread already in filter mode cannot switch.
fn set_strict(cx: &mut Cx<'_>) -> Result<u64, Errno> {
    let mut s = cx.task.seccomp.borrow_mut();
    if s.mode != mode::DISABLED && s.mode != mode::STRICT {
        return Err(Errno::EINVAL);
    }
    s.mode = mode::STRICT;
    drop(s);
    publish(cx.task);
    Ok(0)
}

/// Installs a filter (Linux's `seccomp_set_mode_filter`): checks the flags and the program, and
/// requires `no_new_privs` or `CAP_SYS_ADMIN`; with `NEW_LISTENER` returns the notification fd,
/// EBUSY when the chain already has one; with `TSYNC` the other threads of the thread group must
/// all be able to sync, otherwise returns the tid of the first that cannot (ESRCH with
/// `TSYNC_ESRCH`).
fn set_filter(cx: &mut Cx<'_>, flags: u32, args: u64) -> Result<u64, Errno> {
    if flags & !flag::MASK != 0 {
        return Err(Errno::EINVAL);
    }
    // With both set, an fd returned on success and a tid returned on failure cannot be told
    // apart, unless the tid is not wanted
    if flags & flag::TSYNC != 0 && flags & flag::NEW_LISTENER != 0 && flags & flag::TSYNC_ESRCH == 0
    {
        return Err(Errno::EINVAL);
    }
    if flags & flag::WAIT_KILLABLE_RECV != 0 && flags & flag::NEW_LISTENER == 0 {
        return Err(Errno::EINVAL);
    }
    // struct sock_fprog: the length and the address of the instructions
    let b: [u8; 16] = get(cx, args)?;
    let len = u16::from_le_bytes([b[0], b[1]]) as usize;
    let addr = u64::from_le_bytes(b[8..].try_into().unwrap());
    if len == 0 || len > MAX_INSNS {
        return Err(Errno::EINVAL);
    }
    {
        // Without no_new_privs, CAP_SYS_ADMIN is needed in our own user namespace (Linux's
        // seccomp_prepare_filter uses ns_capable, so root in a user namespace is enough)
        let cred = cx.cred();
        if !cx.t.group.borrow().no_new_privs
            && !cred.ns_capable(&cred.user_ns, crate::cred::cap::SYS_ADMIN)
        {
            return Err(Errno::EACCES);
        }
    }
    if addr == 0 {
        return Err(Errno::EINVAL);
    }
    let mut orig = vec![0u8; len * 8];
    cx.mm().read(UserSlice::new(addr, orig.len()), &mut orig)?;
    let mut prog: Vec<Insn> = orig.chunks_exact(8).map(Insn::decode).collect();
    check_classic(&prog)?;
    check_seccomp(&mut prog)?;

    // Find the notification fd's slot first: nothing can fail once the filter is installed (Linux
    // does get_unused_fd_flags first)
    let listener = flags & flag::NEW_LISTENER != 0;
    let fd = if listener {
        Some(cx.fds().free_slot(0)?)
    } else {
        None
    };
    let (mode, prev) = {
        let s = cx.task.seccomp.borrow();
        (s.mode, s.chain.as_ref().map(|c| c.filter().clone()))
    };
    if mode != mode::DISABLED && mode != mode::FILTER {
        return Err(Errno::EINVAL);
    }
    if listener
        && prev
            .iter()
            .any(|p| p.chain().any(|f| f.notif.borrow().is_some()))
    {
        return Err(Errno::EBUSY);
    }
    let converted_len = converted_len(&prog);
    let total = converted_len
        + prev
            .iter()
            .flat_map(|p| p.chain())
            .map(|f| f.converted_len + 4)
            .sum::<usize>();
    if total > MAX_INSNS_PER_PATH {
        return Err(Errno::ENOMEM);
    }
    // TSYNC: every other thread either has no seccomp or uses a filter on our current chain
    let others: Vec<Rc<Task>> = {
        let g = cx.t.group.borrow();
        let me = cx.tid();
        g.threads
            .iter()
            .filter(|&&t| t != me)
            .filter_map(|t| cx.kr().tasks.get(t).cloned())
            .collect()
    };
    if flags & flag::TSYNC != 0 {
        let unsyncable = others.iter().find(|t| {
            let s = t.seccomp.borrow();
            match (s.mode, &s.chain) {
                (mode::DISABLED, _) => false,
                (mode::FILTER, Some(c)) => !prev
                    .as_ref()
                    .is_some_and(|p| p.chain().any(|f| std::ptr::eq(f, &**c.filter()))),
                _ => true,
            }
        });
        if let Some(t) = unsyncable {
            // The thread's number in the caller's pid namespace (ESRCH when it has none there)
            let tid = cx.pid_out(t.tid.get());
            if flags & flag::TSYNC_ESRCH != 0 || tid == 0 {
                return Err(Errno::ESRCH);
            }
            return Ok(tid as u64);
        }
    }
    let allow = cache(&prog, prev.as_ref().map(|p| &p.allow));
    let filter = Rc::new(Filter {
        converted_len,
        allow,
        prog: prog.into_boxed_slice(),
        orig: orig.into_boxed_slice(),
        log: flags & flag::LOG != 0,
        wait_killable_recv: flags & flag::WAIT_KILLABLE_RECV != 0,
        prev: cx
            .task
            .seccomp
            .borrow_mut()
            .chain
            .take()
            .map(Chain::into_filter),
        users: Cell::new(1),
        notif: RefCell::new(listener.then(|| Notif {
            next_id: random_id(),
            requests: 0,
            list: Vec::new(),
            arrivals: [0, 0],
        })),
    });
    let chain = Chain(Some(filter.clone()));
    if flags & flag::TSYNC != 0 {
        for t in &others {
            {
                let mut s = t.seccomp.borrow_mut();
                s.mode = mode::FILTER;
                // The replaced chain decreases its user count when it is dropped
                s.chain = Some(chain.clone());
            }
            publish(t);
        }
    }
    {
        let mut s = cx.task.seccomp.borrow_mut();
        s.mode = mode::FILTER;
        s.chain = Some(chain);
    }
    publish(cx.task);
    match fd {
        Some(fd) => {
            let file = OpenFile::new(FileKind::SeccompNotify(Listener(filter)), oflags::O_RDWR);
            cx.fds_mut().install_at(fd, file, true)?;
            Ok(fd as u64)
        }
        None => Ok(0),
    }
}

/// Notification ids start from a random number (as on Linux, so supervisors cannot guess other
/// notifications).
fn random_id() -> u64 {
    let mut b = [0u8; 8];
    let _ = tuxport_hal::fill_random(&mut b);
    u64::from_le_bytes(b)
}

// ---- Syscall entry ----

/// The result of the check at syscall entry.
pub(crate) enum Verdict {
    /// Run the syscall with this number as usual (the tracer may have changed the number).
    Run(u64),
    /// Do not run it; x0 already holds the return value (or SIGSYS is already pending).
    Skip,
    /// The thread has exited.
    Exit,
}

/// Syscall entry (after the PTRACE_SYSCALL entry stop): checks syscall number `nr` against the
/// thread's seccomp mode.
pub(crate) fn check(cx: &mut Cx<'_>, cpu: &mut Cpu, nr: u64) -> Verdict {
    let (mode, top) = {
        let s = cx.task.seccomp.borrow();
        (s.mode, s.chain.as_ref().map(|c| c.filter().clone()))
    };
    // PTRACE_O_SUSPEND_SECCOMP (used by checkpoint/restore); the clock-reading stubs in the vDSO
    // do not enter the kernel on Linux
    let suspended = {
        let t = cx.task.trace.borrow();
        t.tracer.is_some() && t.options & crate::ptrace::opt::SUSPEND_SECCOMP != 0
    };
    if mode == mode::DISABLED || suspended || crate::vdso::clock_stub(cx, cpu, nr) {
        return Verdict::Run(nr);
    }
    match (mode, top) {
        (mode::FILTER, Some(top)) => filter(cx, cpu, &top, nr, false),
        (mode::STRICT, _) if STRICT_ALLOWED.contains(&nr) => Verdict::Run(nr),
        (mode::STRICT, _) => {
            log(cx, nr, Signal::KILL.number(), ret::KILL_THREAD, true);
            cx.task.seccomp.borrow_mut().mode = mode::DEAD;
            publish(cx.task);
            exit_thread(cx, cpu, Signal::KILL)
        }
        // A thread already told to die entered the kernel again (Linux should not get here
        // either)
        _ => exit_thread(cx, cpu, Signal::KILL),
    }
}

/// Filter mode: runs the filters on the chain and carries out the resulting action (Linux's
/// `__seccomp_filter`). `recheck` is the re-check after the tracer has handled TRACE: this time
/// TRACE counts as allow.
fn filter(cx: &mut Cx<'_>, cpu: &mut Cpu, top: &Rc<Filter>, nr: u64, recheck: bool) -> Verdict {
    // Numbers the cache says are always ALLOW do not run the programs (Linux's
    // seccomp_cache_check_allow)
    if top.always_allows(nr) {
        return Verdict::Run(nr);
    }
    let data = {
        let s = cpu.state();
        let args = [s.orig_x0, s.x[1], s.x[2], s.x[3], s.x[4], s.x[5]];
        seccomp_data(nr as i32, s.pc, args)
    };
    let (r, matched) = run_filters(top, &data);
    let (action, value) = (r & ret::ACTION_FULL, r & ret::DATA);
    // The syscall is not run: logged only when the filter set SECCOMP_FILTER_FLAG_LOG
    let requested = matched.as_ref().is_some_and(|m| m.log);
    let skip = |cx: &Cx<'_>| {
        log(cx, nr, 0, action, requested);
        Verdict::Skip
    };
    match action {
        ret::ERRNO => {
            cpu.state_mut().x[0] = (-i64::from(value.min(MAX_ERRNO))) as u64;
            skip(cx)
        }
        ret::TRAP => {
            // The handler sees the original registers
            cpu.state_mut().x[0] = cpu.state().orig_x0;
            signal::force(cx, Signal::SYS, sigsys(cpu, nr, value));
            skip(cx)
        }
        ret::TRACE if recheck => Verdict::Run(nr),
        ret::TRACE => {
            if !cx
                .task
                .trace
                .borrow()
                .enabled(crate::ptrace::event::SECCOMP)
            {
                cpu.state_mut().x[0] = Errno::ENOSYS.to_syscall_return();
                return skip(cx);
            }
            // The tracer may change the number (-1 skips) and the arguments; check again after
            let Some(nr) = crate::ptrace::seccomp_stop(cx, cpu, nr, value) else {
                return Verdict::Exit;
            };
            if nr as i64 >= 0 {
                filter(cx, cpu, top, nr, true)
            } else {
                skip(cx)
            }
        }
        ret::USER_NOTIF => match matched {
            Some(m) => notify(cx, cpu, &m, data, nr),
            None => Verdict::Run(nr),
        },
        ret::LOG => {
            log(cx, nr, 0, action, true);
            Verdict::Run(nr)
        }
        ret::ALLOW => Verdict::Run(nr),
        // KILL_PROCESS, KILL_THREAD and unknown actions: terminate. With only this thread left it
        // is the same as killing the process: deliver a SIGSYS that cannot be handled or stopped
        // by a tracer, and write a core
        _ => {
            log(cx, nr, Signal::SYS.number(), action, true);
            cx.task.seccomp.borrow_mut().mode = mode::DEAD;
            publish(cx.task);
            if action != ret::KILL_THREAD || cx.t.group.borrow().threads.len() == 1 {
                cpu.state_mut().x[0] = cpu.state().orig_x0;
                signal::force_fatal(cx, Signal::SYS, sigsys(cpu, nr, value));
                Verdict::Skip
            } else {
                exit_thread(cx, cpu, Signal::SYS)
            }
        }
    }
}

/// `struct seccomp_data`: the number, the architecture, the pc after svc and the six arguments
/// (the first is the original x0).
fn seccomp_data(nr: i32, pc: u64, args: [u64; 6]) -> [u8; DATA_SIZE] {
    let mut d = [0u8; DATA_SIZE];
    d[..4].copy_from_slice(&nr.to_le_bytes());
    d[4..8].copy_from_slice(&AUDIT_ARCH_AARCH64.to_le_bytes());
    d[8..16].copy_from_slice(&pc.to_le_bytes());
    for (i, a) in args.iter().enumerate() {
        d[16 + i * 8..24 + i * 8].copy_from_slice(&a.to_le_bytes());
    }
    d
}

/// Runs every filter on the chain and takes the result with the strictest action (the smallest
/// as a signed number, the newest among equally strict ones), along with the filter that gave it
/// (Linux's `seccomp_run_filters`).
fn run_filters(top: &Rc<Filter>, data: &[u8; DATA_SIZE]) -> (u32, Option<Rc<Filter>>) {
    let action = |r: u32| (r & ret::ACTION_FULL) as i32;
    let mut best = (ret::ALLOW, None);
    let mut f = Some(top);
    while let Some(cur) = f {
        let r = run(&cur.prog, data);
        if action(r) < action(best.0) {
            best = (r, Some(cur.clone()));
        }
        f = cur.prev.as_ref();
    }
    best
}

/// SIGSYS's siginfo: `si_errno` is the data the filter gave, `si_call_addr` the pc after svc.
fn sigsys(cpu: &Cpu, nr: u64, data: u32) -> SigInfo {
    SigInfo {
        code: SYS_SECCOMP,
        fields: InfoFields::Sys {
            errno: data as i32,
            call_addr: cpu.state().pc,
            syscall: nr as i32,
            arch: AUDIT_ARCH_AARCH64,
        },
        ..SigInfo::default()
    }
}

/// USER_NOTIF: sends a notification to the notification fd's end and waits for its answer
/// (Linux's `seccomp_do_user_notification`). Returns ENOSYS without a notification fd. Returns
/// ERESTARTSYS when interrupted by a signal, and sends a new notification when restarted; with
/// `WAIT_KILLABLE_RECV`, once the notification is received only fatal signals interrupt.
fn notify(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    filter: &Rc<Filter>,
    data: [u8; DATA_SIZE],
    nr: u64,
) -> Verdict {
    let knotif = {
        let mut slot = filter.notif.borrow_mut();
        let Some(n) = slot.as_mut() else {
            cpu.state_mut().x[0] = Errno::ENOSYS.to_syscall_return();
            return Verdict::Skip;
        };
        let k = Rc::new(RefCell::new(Knotif {
            id: n.next_id,
            tid: cx.tid(),
            event: cx.task.event.clone(),
            state: State::Init,
            data,
            error: 0,
            val: 0,
            flags: 0,
        }));
        n.next_id = n.next_id.wrapping_add(1);
        n.list.push(k.clone());
        n.requests += 1;
        n.arrivals[0] += 1;
        k
    };
    request_wake_object(filter.object());
    let outcome = loop {
        let state = knotif.borrow().state;
        if state == State::Replied {
            break Ok(());
        }
        if cx.task.killed.get() {
            break Err(None);
        }
        let killable = filter.wait_killable_recv && state == State::Sent;
        if !killable && cx.interrupted() {
            break Err(Some(Errno::ERESTARTSYS));
        }
        let _ = cx.wait_alone(None);
    };
    if let Some(n) = filter.notif.borrow_mut().as_mut() {
        n.list.retain(|k| !Rc::ptr_eq(k, &knotif));
    }
    let k = knotif.borrow();
    match outcome {
        Err(None) => Verdict::Exit,
        Err(Some(e)) => {
            cpu.state_mut().x[0] = e.to_syscall_return();
            Verdict::Skip
        }
        Ok(()) if k.flags & USER_NOTIF_FLAG_CONTINUE != 0 => Verdict::Run(nr),
        Ok(()) => {
            let v = if k.error != 0 {
                i64::from(k.error)
            } else {
                k.val
            };
            cpu.state_mut().x[0] = v as u64;
            Verdict::Skip
        }
    }
}

/// Logs (Linux emits an audit record, here the debug log is written): kills and LOG are always
/// logged, TRAP, ERRNO, TRACE and USER_NOTIF only when the filter set `SECCOMP_FILTER_FLAG_LOG`,
/// and all of them must be in `actions_logged`.
fn log(cx: &Cx<'_>, nr: u64, sig: i32, action: u32, requested: bool) {
    let (bit, always) = match action {
        ret::ALLOW => return,
        ret::TRAP => (logged::TRAP, false),
        ret::ERRNO => (logged::ERRNO, false),
        ret::TRACE => (logged::TRACE, false),
        ret::USER_NOTIF => (logged::USER_NOTIF, false),
        ret::LOG => (logged::LOG, true),
        ret::KILL_THREAD => (logged::KILL_THREAD, true),
        _ => (logged::KILL_PROCESS, true),
    };
    if (always || requested) && cx.kr().seccomp_logged & bit != 0 {
        crate::debug::log(format_args!(
            "seccomp: pid {} tid {} syscall {nr} sig {sig} action {action:#x}",
            cx.tgid(),
            cx.tid(),
        ));
    }
}

/// Ends only this thread, as if terminated by signal `sig` (Linux's `do_exit(sig)`).
fn exit_thread(cx: &mut Cx<'_>, cpu: &mut Cpu, sig: Signal) -> Verdict {
    let status = ExitStatus::Signaled {
        signal: sig,
        pc: None,
        fault_addr: None,
        core_dumped: false,
    };
    cx.task.exit_status.set(status);
    if cx.tid() == cx.tgid() {
        cx.t.group.borrow_mut().leader_status = status;
    }
    crate::ptrace::exit(cx, cpu, status);
    Verdict::Exit
}

// ---- actions_avail and actions_logged sysctls ----

/// Bits of `/proc/sys/kernel/seccomp/actions_logged` (Linux's `SECCOMP_LOG_*`).
pub(crate) mod logged {
    pub const KILL_PROCESS: u32 = 1;
    pub const KILL_THREAD: u32 = 2;
    pub const TRAP: u32 = 4;
    pub const ERRNO: u32 = 8;
    pub const TRACE: u32 = 16;
    pub const LOG: u32 = 32;
    pub const ALLOW: u32 = 64;
    pub const USER_NOTIF: u32 = 128;
    /// By default every action except ALLOW is logged.
    pub const DEFAULT: u32 = KILL_PROCESS | KILL_THREAD | TRAP | ERRNO | USER_NOTIF | TRACE | LOG;
}

/// Action names, in `actions_avail` order.
const NAMES: [(&str, u32); 8] = [
    ("kill_process", logged::KILL_PROCESS),
    ("kill_thread", logged::KILL_THREAD),
    ("trap", logged::TRAP),
    ("errno", logged::ERRNO),
    ("user_notif", logged::USER_NOTIF),
    ("trace", logged::TRACE),
    ("log", logged::LOG),
    ("allow", logged::ALLOW),
];

/// `/proc/sys/kernel/seccomp/actions_avail`.
pub(crate) fn actions_avail() -> String {
    let names: Vec<_> = NAMES.iter().map(|(n, _)| *n).collect();
    format!("{}\n", names.join(" "))
}

/// `/proc/sys/kernel/seccomp/actions_logged`.
pub(crate) fn actions_logged(k: &Kernel) -> String {
    let names: Vec<_> = NAMES
        .iter()
        .filter(|(_, bit)| k.seccomp_logged & bit != 0)
        .map(|(n, _)| *n)
        .collect();
    format!("{}\n", names.join(" "))
}

/// Writes `actions_logged`: space-separated names (up to a newline or an empty name), allow is
/// not permitted, and `CAP_SYS_ADMIN` is required (Linux's `write_actions_logged`).
pub(crate) fn set_actions_logged(
    k: &mut Kernel,
    cred: &crate::cred::Cred,
    data: &[u8],
) -> Result<(), Errno> {
    if !cred.capable(crate::cred::cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    let line = data.split(|&c| c == b'\n' || c == 0).next().unwrap_or(&[]);
    let mut bits = 0;
    for name in line.split(|&c| c == b' ') {
        if name.is_empty() {
            break;
        }
        let (_, bit) = NAMES
            .iter()
            .find(|(n, _)| n.as_bytes() == name)
            .ok_or(Errno::EINVAL)?;
        bits |= bit;
    }
    if bits & logged::ALLOW != 0 {
        return Err(Errno::EINVAL);
    }
    k.seccomp_logged = bits;
    Ok(())
}

// ---- User notifications ----

/// ioctls of the notification fd.
const IOCTL_NOTIF_RECV: u32 = 0xc050_2100;
const IOCTL_NOTIF_SEND: u32 = 0xc018_2101;
const IOCTL_NOTIF_ID_VALID: u32 = 0x4008_2102;
/// ID_VALID as first defined, with the wrong direction; Linux still accepts it.
const IOCTL_NOTIF_ID_VALID_WRONG_DIR: u32 = 0x8008_2102;
/// ADDFD's size can vary (the struct may grow later), so the direction and size are masked off
/// when comparing.
const IOCTL_NOTIF_ADDFD: u32 = 0x4018_2103;
const IOC_DIR_SIZE_MASK: u32 = 0xc000_0000 | 0x3fff_0000;
/// The original size of `struct seccomp_notif_addfd`.
const ADDFD_SIZE_VER0: usize = 24;
const ADDFD_FLAG_SETFD: u32 = 1;
const ADDFD_FLAG_SEND: u32 = 2;

/// ioctls of the notification fd.
pub(crate) fn ioctl(cx: &mut Cx<'_>, f: &Filter, request: u32, arg: u64) -> Result<u64, Errno> {
    match request {
        IOCTL_NOTIF_RECV => recv(cx, f, arg),
        IOCTL_NOTIF_SEND => send(cx, f, arg),
        IOCTL_NOTIF_ID_VALID | IOCTL_NOTIF_ID_VALID_WRONG_DIR => {
            let id = u64::from_le_bytes(get(cx, arg)?);
            match find(f, id) {
                Some(k) if k.borrow().state == State::Sent => Ok(0),
                _ => Err(Errno::ENOENT),
            }
        }
        r if r & !IOC_DIR_SIZE_MASK == IOCTL_NOTIF_ADDFD & !IOC_DIR_SIZE_MASK => {
            addfd(cx, f, arg, (r >> 16 & 0x3fff) as usize)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// SECCOMP_IOCTL_NOTIF_RECV: waits for a notification not yet received and takes it (`struct
/// seccomp_notif` must be zeroed first). As in 6.1, interruption by a signal is EINTR, and it
/// waits even when the fd has O_NONBLOCK set.
fn recv(cx: &mut Cx<'_>, f: &Filter, arg: u64) -> Result<u64, Errno> {
    let mut b = [0u8; 80];
    cx.mm().read(UserSlice::new(arg, b.len()), &mut b)?;
    if b.iter().any(|&c| c != 0) {
        return Err(Errno::EINVAL);
    }
    let knotif = loop {
        let got = {
            let mut slot = f.notif.borrow_mut();
            let n = slot.as_mut().ok_or(Errno::ENOENT)?;
            if n.requests > 0 {
                n.requests -= 1;
                let k = n
                    .list
                    .iter()
                    .find(|k| k.borrow().state == State::Init)
                    .cloned();
                if k.is_some() {
                    n.arrivals[1] += 1;
                }
                Some(k)
            } else {
                None
            }
        };
        match got {
            // The thread that sent it was interrupted and withdrew it
            Some(None) => return Err(Errno::ENOENT),
            Some(Some(k)) => break k,
            None => {
                if cx.interrupted() {
                    return Err(Errno::EINTR);
                }
                cx.wait_objects(None, &mut [], wake::FILE, &[f.object()])?;
            }
        }
    };
    knotif.borrow_mut().state = State::Sent;
    request_wake_object(f.object());
    let (id, tid, data) = {
        let k = knotif.borrow();
        (k.id, k.tid, k.data)
    };
    // The thread as the receiver's pid namespace sees it, 0 when invisible (task_pid_vnr)
    let tid = cx.pid_out(tid);
    b[..8].copy_from_slice(&id.to_le_bytes());
    b[8..12].copy_from_slice(&tid.to_le_bytes());
    b[16..].copy_from_slice(&data);
    if cx.mm().write(UserSlice::new(arg, b.len()), &b).is_err() {
        // Cannot write it back: put the notification back so the thread that sent it does not
        // wait forever (one waiting only for fatal signals is woken to look again)
        let mut slot = f.notif.borrow_mut();
        if let Some(n) = slot.as_mut()
            && n.list.iter().any(|k| Rc::ptr_eq(k, &knotif))
        {
            let mut k = knotif.borrow_mut();
            if f.wait_killable_recv && k.state == State::Sent {
                signal_later(&k.event);
            }
            k.state = State::Init;
            n.requests += 1;
            n.arrivals[0] += 1;
            request_wake_object(f.object());
        }
        return Err(Errno::EFAULT);
    }
    Ok(0)
}

/// SECCOMP_IOCTL_NOTIF_SEND: answers a received notification (`struct seccomp_notif_resp`).
fn send(cx: &mut Cx<'_>, f: &Filter, arg: u64) -> Result<u64, Errno> {
    let b: [u8; 24] = get(cx, arg)?;
    let id = u64::from_le_bytes(b[..8].try_into().unwrap());
    let val = i64::from_le_bytes(b[8..16].try_into().unwrap());
    let error = i32::from_le_bytes(b[16..20].try_into().unwrap());
    let flags = u32::from_le_bytes(b[20..24].try_into().unwrap());
    if flags & !USER_NOTIF_FLAG_CONTINUE != 0
        || (flags & USER_NOTIF_FLAG_CONTINUE != 0 && (error != 0 || val != 0))
    {
        return Err(Errno::EINVAL);
    }
    let knotif = find(f, id).ok_or(Errno::ENOENT)?;
    let mut k = knotif.borrow_mut();
    // Only one answer
    if k.state != State::Sent {
        return Err(Errno::EINPROGRESS);
    }
    k.state = State::Replied;
    k.error = error;
    k.val = val;
    k.flags = flags;
    signal_later(&k.event);
    Ok(0)
}

/// SECCOMP_IOCTL_NOTIF_ADDFD: installs our fd `srcfd` into the fd table of the thread that sent
/// the notification (at `newfd` with `SETFD`, like dup2) and returns the fd on that side; with
/// `SEND` it also answers the notification with it. On Linux that thread installs it itself;
/// here it is installed directly while holding the BKL, with the same effect, and it cannot fail
/// at the moment that thread is interrupted (Linux returns ESRCH).
fn addfd(cx: &mut Cx<'_>, f: &Filter, arg: u64, size: usize) -> Result<u64, Errno> {
    if !(ADDFD_SIZE_VER0..tuxport_engine::PAGE_SIZE as usize).contains(&size) {
        return Err(Errno::EINVAL);
    }
    let mut b = vec![0u8; size];
    cx.mm().read(UserSlice::new(arg, size), &mut b)?;
    // The part beyond the current struct must be zero (copy_struct_from_user)
    if b[ADDFD_SIZE_VER0..].iter().any(|&c| c != 0) {
        return Err(Errno::E2BIG);
    }
    let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    let id = u64::from_le_bytes(b[..8].try_into().unwrap());
    let (flags, srcfd, newfd, newfd_flags) = (u32_at(8), u32_at(12), u32_at(16), u32_at(20));
    if newfd_flags & !oflags::O_CLOEXEC != 0
        || flags & !(ADDFD_FLAG_SETFD | ADDFD_FLAG_SEND) != 0
        || (newfd != 0 && flags & ADDFD_FLAG_SETFD == 0)
    {
        return Err(Errno::EINVAL);
    }
    let file = cx.fds().get(srcfd as i32)?;
    let knotif = find(f, id).ok_or(Errno::ENOENT)?;
    if knotif.borrow().state != State::Sent {
        return Err(Errno::EINPROGRESS);
    }
    let tid = knotif.borrow().tid;
    let task = cx.kr().tasks.get(&tid).cloned().ok_or(Errno::ENOENT)?;
    let fds = task.res.borrow().fds.clone();
    let cloexec = newfd_flags & oflags::O_CLOEXEC != 0;
    let installed = if flags & ADDFD_FLAG_SETFD != 0 {
        let replaced = fds.borrow_mut().install_at(newfd as i32, file, cloexec);
        match replaced {
            Ok(old) => {
                let old: Vec<_> = old.into_iter().collect();
                crate::syscall::close_replaced(cx, &fds, task.tgid, old);
                Ok(newfd as i32)
            }
            Err(e) => Err(e),
        }
    } else {
        fds.borrow_mut().install(file, cloexec, 0)
    };
    let fd = installed?;
    if flags & ADDFD_FLAG_SEND != 0 {
        let mut k = knotif.borrow_mut();
        k.state = State::Replied;
        k.flags = 0;
        k.error = 0;
        k.val = i64::from(fd);
        signal_later(&k.event);
    }
    Ok(fd as u64)
}

fn find(f: &Filter, id: u64) -> Option<Rc<RefCell<Knotif>>> {
    let n = f.notif.borrow();
    n.as_ref()?
        .list
        .iter()
        .find(|k| k.borrow().id == id)
        .cloned()
}

// ---- ptrace ----

/// PTRACE_SECCOMP_GET_FILTER: the original program of the tracee's filter number `index` (0 is
/// the first installed), written to `data` (when it is 0, only the instruction count is
/// returned). The tracer needs `CAP_SYS_ADMIN` and must have no seccomp itself.
pub(crate) fn get_filter(cx: &Cx<'_>, task: &Task, index: u64, data: u64) -> Result<u64, Errno> {
    let f = nth_filter(cx, task, index)?;
    if data != 0 {
        cx.mm().write(UserSlice::new(data, f.orig.len()), &f.orig)?;
    }
    Ok((f.orig.len() / 8) as u64)
}

/// PTRACE_SECCOMP_GET_METADATA: `data` is a `struct seccomp_metadata`; the tracer gives
/// `filter_off` and gets that filter's flags (only `SECCOMP_FILTER_FLAG_LOG`). Returns the number
/// of bytes written.
pub(crate) fn get_metadata(cx: &Cx<'_>, task: &Task, size: u64, data: u64) -> Result<u64, Errno> {
    privileged_tracer(cx)?;
    let size = size.min(16) as usize;
    if size < 8 {
        return Err(Errno::EINVAL);
    }
    let index = u64::from_le_bytes(get(cx, data)?);
    let f = nth_filter(cx, task, index)?;
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&index.to_le_bytes());
    if f.log {
        b[8..].copy_from_slice(&u64::from(flag::LOG).to_le_bytes());
    }
    cx.mm().write(UserSlice::new(data, size), &b[..size])?;
    Ok(size as u64)
}

fn nth_filter(cx: &Cx<'_>, task: &Task, index: u64) -> Result<Rc<Filter>, Errno> {
    privileged_tracer(cx)?;
    let s = task.seccomp.borrow();
    let top = match (s.mode, &s.chain) {
        (mode::FILTER, Some(c)) => c.filter(),
        _ => return Err(Errno::EINVAL),
    };
    let all: Vec<&Filter> = top.chain().collect();
    let i = usize::try_from(index)
        .ok()
        .filter(|&i| i < all.len())
        .ok_or(Errno::ENOENT)?;
    // The chain starts from the newest, indexes from the oldest
    let mut f = top.clone();
    for _ in 0..all.len() - 1 - i {
        let prev = f.prev.clone().expect("the chain is that long");
        f = prev;
    }
    Ok(f)
}

fn privileged_tracer(cx: &Cx<'_>) -> Result<(), Errno> {
    if !cx.cred().capable(crate::cred::cap::SYS_ADMIN)
        || cx.task.seccomp.borrow().mode != mode::DISABLED
    {
        return Err(Errno::EACCES);
    }
    Ok(())
}

// ---- Thread exit ----

/// A thread has exited: a non-main thread drops its filter chain at once; the main thread's is
/// handed to the thread group and dropped only when the process is reaped (Linux drops it in
/// `release_task`; a zombie is still a user, and the notification fd reports POLLHUP only then).
pub(crate) fn exited(task: &Task, group: &mut crate::process::ThreadGroup) {
    let chain = task.seccomp.borrow_mut().chain.take();
    if task.tid.get() == group.tgid {
        group.leader_seccomp = chain;
    }
}

// ---- The always-allow cache ----

/// The bitmap of a new filter (Linux's `seccomp_cache_prepare_bitmap`): the numbers in the
/// previous filter's bitmap (every number when there is none) that this program also always
/// returns ALLOW for.
fn cache(prog: &[Insn], prev: Option<&[u64; CACHE_WORDS]>) -> [u64; CACHE_WORDS] {
    let mut allow = prev.copied().unwrap_or([u64::MAX; CACHE_WORDS]);
    for nr in 0..CACHED_NRS {
        let bit = 1 << (nr % 64);
        if allow[nr / 64] & bit != 0 && !const_allow(prog, nr as u32) {
            allow[nr / 64] &= !bit;
        }
    }
    allow
}

/// Whether the program always returns ALLOW when only the number `nr` and the architecture are
/// known (Linux's `seccomp_is_const_allow`): it may read only the number and the architecture and
/// use only jumps comparing with constants, unconditional jumps and ANDs with constants. Reading
/// other data or using other instructions does not count, and the return value must be exactly
/// ALLOW (with data it does not count).
fn const_allow(prog: &[Insn], nr: u32) -> bool {
    use bpf::*;
    let mut a = 0u32;
    let mut pc = 0;
    while let Some(i) = prog.get(pc) {
        pc += 1;
        match i.code {
            LOAD_DATA => match i.k {
                0 => a = nr,
                4 => a = AUDIT_ARCH_AARCH64,
                _ => return false,
            },
            c if c == RET | K => return i.k == ret::ALLOW,
            c if c == JMP | JA => pc += i.k as usize,
            c if conditional(c) && c & X == 0 => {
                let taken = match c & 0xf0 {
                    JEQ => a == i.k,
                    JGT => a > i.k,
                    JGE => a >= i.k,
                    _ => a & i.k != 0,
                };
                pc += if taken { i.jt } else { i.jf } as usize;
            }
            c if c == ALU | AND | K => a &= i.k,
            _ => return false,
        }
    }
    false
}

/// A thread's seccomp changed (a filter installed, TSYNC, clone, being killed): updates
/// [`Task::seccomp_on`] and the allow bitmap [`Task::seccomp_allow`], which are read without the
/// BKL. Only filter mode has a bitmap.
pub(crate) fn publish(task: &Task) {
    let s = task.seccomp.borrow();
    let allow = match (s.mode, &s.chain) {
        (mode::FILTER, Some(c)) => c.filter().allow,
        _ => [0; CACHE_WORDS],
    };
    for (w, bits) in task.seccomp_allow.iter().zip(allow) {
        w.store(bits, Ordering::Relaxed);
    }
    task.seccomp_on
        .store(s.mode != mode::DISABLED, Ordering::Relaxed);
}

/// Without the BKL: the thread's filters always return ALLOW for number `nr` (the bitmap from
/// [`publish`]). Another thread doing TSYNC may be changing it; a stale bit read belongs to the
/// chain before the sync (the new chain can only be stricter), as if this syscall happened before
/// the sync.
pub(crate) fn always_allowed(task: &Task, nr: u64) -> bool {
    cached(|i| task.seccomp_allow[i].load(Ordering::Relaxed), nr)
}

/// Whether number `nr` is in the bitmap (each word given by `word`).
fn cached(word: impl Fn(usize) -> u64, nr: u64) -> bool {
    nr < CACHED_NRS as u64 && word(nr as usize / 64) >> (nr % 64) & 1 != 0
}

// ---- cBPF programs ----

/// cBPF opcodes.
mod bpf {
    pub const LD: u16 = 0x00;
    pub const LDX: u16 = 0x01;
    pub const ST: u16 = 0x02;
    pub const STX: u16 = 0x03;
    pub const ALU: u16 = 0x04;
    pub const JMP: u16 = 0x05;
    pub const RET: u16 = 0x06;
    pub const MISC: u16 = 0x07;

    pub const W: u16 = 0x00;
    pub const H: u16 = 0x08;
    pub const B: u16 = 0x10;

    pub const IMM: u16 = 0x00;
    pub const ABS: u16 = 0x20;
    pub const IND: u16 = 0x40;
    pub const MEM: u16 = 0x60;
    pub const LEN: u16 = 0x80;
    pub const MSH: u16 = 0xa0;

    pub const ADD: u16 = 0x00;
    pub const SUB: u16 = 0x10;
    pub const MUL: u16 = 0x20;
    pub const DIV: u16 = 0x30;
    pub const OR: u16 = 0x40;
    pub const AND: u16 = 0x50;
    pub const LSH: u16 = 0x60;
    pub const RSH: u16 = 0x70;
    pub const NEG: u16 = 0x80;
    pub const MOD: u16 = 0x90;
    pub const XOR: u16 = 0xa0;

    pub const JA: u16 = 0x00;
    pub const JEQ: u16 = 0x10;
    pub const JGT: u16 = 0x20;
    pub const JGE: u16 = 0x30;
    pub const JSET: u16 = 0x40;

    pub const K: u16 = 0x00;
    pub const X: u16 = 0x08;
    /// `BPF_RET | BPF_A`.
    pub const A: u16 = 0x10;

    pub const TAX: u16 = 0x00;
    pub const TXA: u16 = 0x80;

    /// Number of scratch memory slots.
    pub const MEMWORDS: u32 = 16;

    /// The opcode seccomp rewrites `LD | W | ABS` reads of `seccomp_data` into (Linux borrows it
    /// the same way).
    pub const LOAD_DATA: u16 = LDX | W | ABS;
}

/// One cBPF instruction (`struct sock_filter`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

impl Insn {
    fn decode(b: &[u8]) -> Insn {
        Insn {
            code: u16::from_le_bytes([b[0], b[1]]),
            jt: b[2],
            jf: b[3],
            k: u32::from_le_bytes(b[4..8].try_into().unwrap()),
        }
    }
}

/// Checks a program by the cBPF rules (Linux's `bpf_check_classic`): opcodes are recognized;
/// division by constant 0, shifts beyond 31, out-of-bounds scratch memory and jumps out of the
/// program are refused; the last instruction is a return; scratch memory is written before read.
fn check_classic(prog: &[Insn]) -> Result<(), Errno> {
    use bpf::*;
    let n = prog.len();
    for (pc, i) in prog.iter().enumerate() {
        if !classic_code(i.code) {
            return Err(Errno::EINVAL);
        }
        let bad = match i.code {
            c if c == ALU | DIV | K || c == ALU | MOD | K => i.k == 0,
            c if c == ALU | LSH | K || c == ALU | RSH | K => i.k >= 32,
            c if c == LD | MEM || c == LDX | MEM || c == ST || c == STX => i.k >= MEMWORDS,
            c if c == JMP | JA => i.k as u64 >= (n - pc - 1) as u64,
            c if conditional(c) => pc + i.jt as usize + 1 >= n || pc + i.jf as usize + 1 >= n,
            _ => false,
        };
        if bad {
            return Err(Errno::EINVAL);
        }
    }
    if !matches!(prog[n - 1].code, c if c == RET | K || c == RET | A) {
        return Err(Errno::EINVAL);
    }
    check_load_and_stores(prog)
}

/// Opcodes cBPF recognizes (Linux's `chk_code_allowed`).
fn classic_code(code: u16) -> bool {
    use bpf::*;
    matches!(
        code,
        0x04 | 0x0c // ADD
            | 0x14 | 0x1c // SUB
            | 0x24 | 0x2c // MUL
            | 0x34 | 0x3c // DIV
            | 0x94 | 0x9c // MOD
            | 0x54 | 0x5c // AND
            | 0x44 | 0x4c // OR
            | 0xa4 | 0xac // XOR
            | 0x64 | 0x6c // LSH
            | 0x74 | 0x7c // RSH
            | 0x84 // NEG
    ) || [
        LD | W | ABS,
        LD | H | ABS,
        LD | B | ABS,
        LD | W | LEN,
        LD | W | IND,
        LD | H | IND,
        LD | B | IND,
        LD | IMM,
        LD | MEM,
        LDX | W | LEN,
        LDX | B | MSH,
        LDX | IMM,
        LDX | MEM,
        ST,
        STX,
        MISC | TAX,
        MISC | TXA,
        RET | K,
        RET | A,
        JMP | JA,
    ]
    .contains(&code)
        || conditional(code)
}

/// Scratch memory is written before it is read on every path (Linux's `check_load_and_stores`):
/// a jump passes the slots already written on arrival to its target, and the instruction after a
/// jump can only be reached by jumping from elsewhere.
fn check_load_and_stores(prog: &[Insn]) -> Result<(), Errno> {
    use bpf::*;
    let mut masks = vec![u16::MAX; prog.len()];
    let mut valid = 0u16;
    for (pc, i) in prog.iter().enumerate() {
        valid &= masks[pc];
        match i.code {
            c if c == ST || c == STX => valid |= 1 << i.k,
            c if c == LD | MEM || c == LDX | MEM => {
                if valid & 1 << i.k == 0 {
                    return Err(Errno::EINVAL);
                }
            }
            c if c == JMP | JA => {
                masks[pc + 1 + i.k as usize] &= valid;
                valid = u16::MAX;
            }
            c if conditional(c) => {
                masks[pc + 1 + i.jt as usize] &= valid;
                masks[pc + 1 + i.jf as usize] &= valid;
                valid = u16::MAX;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Checks and rewrites a program by seccomp's rules (Linux's `seccomp_check_filter`): data reads
/// may only read 4-byte-aligned words of `seccomp_data`, length reads become the constant 64, and
/// other packet-reading instructions and modulo are refused.
fn check_seccomp(prog: &mut [Insn]) -> Result<(), Errno> {
    use bpf::*;
    for i in prog {
        match i.code {
            c if c == LD | W | ABS => {
                if i.k as usize >= DATA_SIZE || i.k & 3 != 0 {
                    return Err(Errno::EINVAL);
                }
                i.code = LOAD_DATA;
            }
            c if c == LD | W | LEN => {
                i.code = LD | IMM;
                i.k = DATA_SIZE as u32;
            }
            c if c == LDX | W | LEN => {
                i.code = LDX | IMM;
                i.k = DATA_SIZE as u32;
            }
            0x06 | 0x16 // RET K, RET A
            | 0x04 | 0x0c | 0x14 | 0x1c | 0x24 | 0x2c | 0x34 | 0x3c // ADD, SUB, MUL, DIV
            | 0x54 | 0x5c | 0x44 | 0x4c | 0xa4 | 0xac // AND, OR, XOR
            | 0x64 | 0x6c | 0x74 | 0x7c | 0x84 // LSH, RSH, NEG
            | 0x00 | 0x01 // LD IMM, LDX IMM
            | 0x07 | 0x87 // TAX, TXA
            | 0x60 | 0x61 | 0x02 | 0x03 // LD MEM, LDX MEM, ST, STX
            | 0x05 | 0x15 | 0x1d | 0x35 | 0x3d | 0x25 | 0x2d | 0x45 | 0x4d => {} // jumps
            _ => return Err(Errno::EINVAL),
        }
    }
    Ok(())
}

/// The length after Linux's `bpf_convert_filter`, used for the filter chain's capacity check.
/// cBPF is still interpreted here, and only the converted length is counted; seccomp has already
/// excluded packet access and modulo instructions.
fn converted_len(prog: &[Insn]) -> usize {
    use bpf::*;
    // Clear A and X and save the context pointer.
    3 + prog
        .iter()
        .map(|i| match i.code {
            c if c == RET | K => 2,
            c if c == ALU | DIV | X => 5,
            c if conditional(c) => {
                let immediate = usize::from(c & X == 0 && i.k > i32::MAX as u32);
                let extra_jump = usize::from(i.jf != 0 && (i.jt != 0 || c & 0xf0 == JSET));
                1 + immediate + extra_jump
            }
            _ => 1,
        })
        .sum::<usize>()
}

/// Conditional jumps.
fn conditional(code: u16) -> bool {
    use bpf::*;
    code & 0x07 == JMP && code & 0xf0 != JA && code & 0xf0 <= JSET
}

/// Runs a checked program on `data` (a `struct seccomp_data`) and returns its return value.
fn run(prog: &[Insn], data: &[u8; DATA_SIZE]) -> u32 {
    use bpf::*;
    let (mut a, mut x) = (0u32, 0u32);
    let mut mem = [0u32; MEMWORDS as usize];
    let mut pc = 0;
    loop {
        let i = prog[pc];
        pc += 1;
        let src = if i.code & X != 0 { x } else { i.k };
        match i.code {
            LOAD_DATA => {
                let k = i.k as usize;
                a = u32::from_le_bytes(data[k..k + 4].try_into().unwrap());
            }
            c if c == LD | IMM => a = i.k,
            c if c == LDX | IMM => x = i.k,
            c if c == LD | MEM => a = mem[i.k as usize],
            c if c == LDX | MEM => x = mem[i.k as usize],
            c if c == ST => mem[i.k as usize] = a,
            c if c == STX => mem[i.k as usize] = x,
            c if c == MISC | TAX => x = a,
            c if c == MISC | TXA => a = x,
            c if c == RET | K => return i.k,
            c if c == RET | A => return a,
            c if c == JMP | JA => pc += i.k as usize,
            c if c & 0x07 == ALU => match c & 0xf0 {
                ADD => a = a.wrapping_add(src),
                SUB => a = a.wrapping_sub(src),
                MUL => a = a.wrapping_mul(src),
                // On division by 0 (only possible with X) a cBPF program returns 0
                DIV if src == 0 => return 0,
                DIV => a /= src,
                OR => a |= src,
                AND => a &= src,
                XOR => a ^= src,
                LSH => a = a.wrapping_shl(src),
                RSH => a = a.wrapping_shr(src),
                NEG => a = a.wrapping_neg(),
                _ => unreachable!("modulo cannot pass the checks"),
            },
            c => {
                let taken = match c & 0xf0 {
                    JEQ => a == src,
                    JGT => a > src,
                    JGE => a >= src,
                    _ => a & src != 0,
                };
                pc += if taken { i.jt } else { i.jf } as usize;
            }
        }
    }
}

// ---- Shared helpers ----

fn get<const N: usize>(cx: &Cx<'_>, addr: u64) -> Result<[u8; N], Errno> {
    let mut b = [0u8; N];
    cx.mm().read(UserSlice::new(addr, N), &mut b)?;
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insn(code: u16, jt: u8, jf: u8, k: u32) -> Insn {
        Insn { code, jt, jf, k }
    }

    fn prepared(mut prog: Vec<Insn>) -> Result<Vec<Insn>, Errno> {
        check_classic(&prog)?;
        check_seccomp(&mut prog)?;
        Ok(prog)
    }

    #[test]
    fn programs_are_checked_like_linux() {
        use bpf::*;
        let ret = insn(RET | K, 0, 0, ret::ALLOW);
        assert!(prepared(vec![ret]).is_ok());
        // Reading the number: aligned to 4, not beyond seccomp_data
        assert!(prepared(vec![insn(LD | W | ABS, 0, 0, 0), ret]).is_ok());
        assert_eq!(
            prepared(vec![insn(LD | W | ABS, 0, 0, 2), ret]),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            prepared(vec![insn(LD | W | ABS, 0, 0, 64), ret]),
            Err(Errno::EINVAL)
        );
        // Half-word reads and modulo are recognized by cBPF but not by seccomp
        assert_eq!(
            prepared(vec![insn(LD | H | ABS, 0, 0, 0), ret]),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            prepared(vec![insn(ALU | MOD | K, 0, 0, 3), ret]),
            Err(Errno::EINVAL)
        );
        // Last instruction not a return, jumping out of the program, division by constant 0,
        // shifting too far
        assert_eq!(prepared(vec![insn(LD | IMM, 0, 0, 1)]), Err(Errno::EINVAL));
        assert_eq!(
            prepared(vec![insn(JMP | JA, 0, 0, 1), ret]),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            prepared(vec![insn(JMP | JEQ | K, 1, 0, 0), ret]),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            prepared(vec![insn(ALU | DIV | K, 0, 0, 0), ret]),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            prepared(vec![insn(ALU | LSH | K, 0, 0, 32), ret]),
            Err(Errno::EINVAL)
        );
        // Scratch memory written before read: a slot written on only one branch cannot be read
        // after the join
        let partial = vec![
            insn(JMP | JEQ | K, 0, 1, 0),
            insn(ST, 0, 0, 3),
            insn(LD | MEM, 0, 0, 3),
            ret,
        ];
        assert_eq!(prepared(partial), Err(Errno::EINVAL));
        let both = vec![
            insn(ST, 0, 0, 3),
            insn(JMP | JA, 0, 0, 0),
            insn(LD | MEM, 0, 0, 3),
            ret,
        ];
        assert!(prepared(both).is_ok());
        // The length becomes the size of seccomp_data
        let len = prepared(vec![insn(LD | W | LEN, 0, 0, 0), insn(RET | A, 0, 0, 0)]).unwrap();
        assert_eq!(run(&len, &[0; DATA_SIZE]), 64);
    }

    #[test]
    fn programs_run_like_the_jit() {
        use bpf::*;
        let data = seccomp_data(39, 0x1000, [7, 0, 0, 0, 0, 0]);
        // Returns ERRNO|3 when the number is 39, otherwise ALLOW
        let prog = prepared(vec![
            insn(LD | W | ABS, 0, 0, 0),
            insn(JMP | JEQ | K, 0, 1, 39),
            insn(RET | K, 0, 0, ret::ERRNO | 3),
            insn(RET | K, 0, 0, ret::ALLOW),
        ])
        .unwrap();
        assert_eq!(run(&prog, &data), ret::ERRNO | 3);
        // The low 32 bits of the first argument, the architecture
        let arg = prepared(vec![insn(LD | W | ABS, 0, 0, 16), insn(RET | A, 0, 0, 0)]).unwrap();
        assert_eq!(run(&arg, &data), 7);
        let arch = prepared(vec![insn(LD | W | ABS, 0, 0, 4), insn(RET | A, 0, 0, 0)]).unwrap();
        assert_eq!(run(&arch, &data), AUDIT_ARCH_AARCH64);
        // Division by X = 0 returns 0 (KILL_THREAD); shifts are taken modulo 32
        let div = prepared(vec![
            insn(LD | IMM, 0, 0, 10),
            insn(ALU | DIV | X, 0, 0, 0),
            insn(RET | K, 0, 0, ret::ALLOW),
        ])
        .unwrap();
        assert_eq!(run(&div, &data), 0);
        let shift = prepared(vec![
            insn(LD | IMM, 0, 0, 1),
            insn(LDX | IMM, 0, 0, 33),
            insn(ALU | LSH | X, 0, 0, 0),
            insn(RET | A, 0, 0, 0),
        ])
        .unwrap();
        assert_eq!(run(&shift, &data), 2);
    }

    #[test]
    fn the_cache_keeps_only_constant_allows() {
        use bpf::*;
        let allowed = |allow: &[u64; CACHE_WORDS], nr: u64| cached(|i| allow[i], nr);
        // Architecture first, then by number: 98 returns ERRNO, 400 and above TRAP, others ALLOW
        let by_nr = prepared(vec![
            insn(LD | W | ABS, 0, 0, 4),
            insn(JMP | JEQ | K, 1, 0, AUDIT_ARCH_AARCH64),
            insn(RET | K, 0, 0, ret::KILL_PROCESS),
            insn(LD | W | ABS, 0, 0, 0),
            insn(JMP | JEQ | K, 2, 0, 98),
            insn(JMP | JGE | K, 2, 0, 400),
            insn(RET | K, 0, 0, ret::ALLOW),
            insn(RET | K, 0, 0, ret::ERRNO | 1),
            insn(RET | K, 0, 0, ret::TRAP),
        ])
        .unwrap();
        let c = cache(&by_nr, None);
        assert!(allowed(&c, 0) && allowed(&c, 97) && allowed(&c, 399));
        assert!(!allowed(&c, 98) && !allowed(&c, 400) && !allowed(&c, 511));
        // Data other than the number (here the first argument) is not constant: 98 must run the
        // program, the others are still constant
        let by_arg = prepared(vec![
            insn(LD | W | ABS, 0, 0, 0),
            insn(JMP | JEQ | K, 0, 2, 98),
            insn(LD | W | ABS, 0, 0, 16),
            insn(JMP | JSET | K, 1, 0, 1),
            insn(RET | K, 0, 0, ret::ALLOW),
            insn(RET | K, 0, 0, ret::ERRNO | 1),
        ])
        .unwrap();
        let c = cache(&by_arg, None);
        assert!(!allowed(&c, 98) && allowed(&c, 99));
        // Comparing after an AND, unconditional jumps
        let masked = prepared(vec![
            insn(LD | W | ABS, 0, 0, 0),
            insn(ALU | AND | K, 0, 0, 0xf),
            insn(JMP | JEQ | K, 1, 0, 3),
            insn(JMP | JA, 0, 0, 1),
            insn(RET | K, 0, 0, ret::LOG),
            insn(RET | K, 0, 0, ret::ALLOW),
        ])
        .unwrap();
        let c = cache(&masked, None);
        assert!(!allowed(&c, 3) && !allowed(&c, 19) && allowed(&c, 4));
        // The return value must be exactly ALLOW; RET A and other instructions do not count
        let data = prepared(vec![insn(RET | K, 0, 0, ret::ALLOW | 1)]).unwrap();
        assert!(!allowed(&cache(&data, None), 0));
        let ret_a = prepared(vec![
            insn(LD | IMM, 0, 0, ret::ALLOW),
            insn(RET | A, 0, 0, 0),
        ])
        .unwrap();
        assert!(!allowed(&cache(&ret_a, None), 0));
        // The new filter always allows, yet does not open up what the previous one does not allow
        let all = prepared(vec![insn(RET | K, 0, 0, ret::ALLOW)]).unwrap();
        let c = cache(&all, Some(&cache(&by_nr, None)));
        assert!(!allowed(&c, 98) && allowed(&c, 97));
        // Numbers outside the bitmap and negative ones must run the program
        assert!(
            !allowed(&cache(&all, None), CACHED_NRS as u64)
                && !allowed(&cache(&all, None), u64::MAX)
        );
    }
}
