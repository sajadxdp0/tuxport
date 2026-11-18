//! The tracee side of ptrace (the kernel/ptrace map): stopping in a ptrace-stop, waiting for the
//! tracer to read and write its registers and memory, then resuming. The tracer side's requests
//! are in `syscall::ptrace`.
//!
//! Tracees are threads, and the tracer is recorded as a process (tgid). Linux accepts requests
//! only from the tracer thread itself, while any thread in the thread group may wait; Tuxport does
//! not record which thread forked (the tracer of PTRACE_TRACEME is the parent process), so this is
//! relaxed to any thread in the tracer process.
//!
//! A tracee stops at these points, and [`stop`] notifies the tracer:
//!
//! - before a signal is delivered (signal-delivery stop, [`signal_stop`]): the tracer can replace
//!   the signal, change its siginfo, or not deliver it;
//! - when the thread group is stopped by a stop signal (group-stop, [`group_stop`]). A tracee does
//!   not stay stopped in the group-stop: when the tracer resumes it, it runs even though the
//!   thread group is still stopped, as on Linux;
//! - after PTRACE_SYSCALL resumes it, at syscall entry and exit ([`syscall_entry`],
//!   [`syscall_exit`]);
//! - `PTRACE_EVENT_*` ([`event`]): fork, vfork, clone, exec, vfork done, exit, plus the
//!   `PTRACE_EVENT_STOP` of PTRACE_INTERRUPT and of new threads auto-attached under PTRACE_SEIZE
//!   ([`trap`]).
//!
//! On a stop the registers are saved in [`Trace::regs`]; the tracer reads and writes that copy,
//! and it is written back to the Cpu on resume. While stopped, no signals are handled; only
//! SIGKILL makes it leave (Linux's TASK_TRACED).

use std::rc::Rc;
use std::sync::atomic::Ordering;

use tuxport_engine::{Cpu, CpuState};

use crate::cell::RefCell;
use crate::kernel::{Cx, Kernel, Tid, request_wake_on, wake};
use crate::process::{ExitStatus, Signal, ThreadGroup};
use crate::signal::{self, SigAction, SigInfo, code, sa};
use crate::task::Task;

/// `PTRACE_EVENT_*`.
pub(crate) mod event {
    pub const FORK: u32 = 1;
    pub const VFORK: u32 = 2;
    pub const CLONE: u32 = 3;
    pub const EXEC: u32 = 4;
    pub const VFORK_DONE: u32 = 5;
    pub const EXIT: u32 = 6;
    pub const SECCOMP: u32 = 7;
    pub const STOP: u32 = 128;
}

/// `PTRACE_O_*`. The option for `PTRACE_EVENT_x` is `1 << x`.
pub(crate) mod opt {
    pub const TRACESYSGOOD: u32 = 1;
    pub const EXITKILL: u32 = 1 << 20;
    pub const SUSPEND_SECCOMP: u32 = 1 << 21;
    /// Known options (Linux's `PTRACE_O_MASK`).
    pub const MASK: u32 = 0xff | EXITKILL | SUSPEND_SECCOMP;
}

/// Why the thread stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    /// Before a signal is delivered.
    Signal(Signal),
    /// A group-stop (not traced with PTRACE_SEIZE; under SEIZE it is reported as
    /// `PTRACE_EVENT_STOP`).
    Group(Signal),
    /// Syscall entry (true) or exit.
    Syscall(bool),
    /// `PTRACE_EVENT_*` and WSTOPSIG: for `PTRACE_EVENT_STOP` SIGTRAP or the stop signal, for the
    /// rest SIGTRAP.
    Event(u32, Signal),
}

/// How the tracer resumes the thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    /// PTRACE_CONT: run until the next stop.
    #[default]
    Cont,
    /// PTRACE_SYSCALL: stop at syscall entry and exit.
    Syscall,
    /// PTRACE_SINGLESTEP: stop after executing one instruction ([`step_done`]).
    Step,
}

/// The registers at a stop. The tracer reads and writes this copy, which is written back to the
/// Cpu on resume.
#[derive(Clone)]
pub(crate) struct Regs {
    pub x: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    /// NZCV and DIT (bit 24), [`pstate_of`].
    pub pstate: u64,
    pub fpcr: u64,
    pub fpsr: u64,
    pub tpidr: u64,
    pub v: [[u64; 2]; 32],
    /// The syscall's original x0 (the first argument PTRACE_GET_SYSCALL_INFO reports), read-only.
    pub orig_x0: u64,
}

impl Regs {
    pub(crate) fn save(s: &CpuState) -> Regs {
        Regs {
            x: s.x,
            sp: s.sp,
            pc: s.pc,
            pstate: pstate_of(s),
            fpcr: s.fpcr,
            fpsr: s.fpsr,
            tpidr: s.tpidr_el0,
            v: s.v,
            orig_x0: s.orig_x0,
        }
    }

    fn restore(&self, s: &mut CpuState) {
        s.x = self.x;
        s.sp = self.sp;
        s.pc = self.pc;
        set_pstate(s, self.pstate);
        s.fpcr = self.fpcr;
        s.fpsr = self.fpsr;
        s.tpidr_el0 = self.tpidr;
        s.v = self.v;
    }
}

/// NZCV in pstate.
const PSTATE_NZCV: u64 = 0xf000_0000;
/// DIT in pstate.
const PSTATE_DIT: u64 = 1 << 24;

/// The pstate the guest sees (signal frames, ptrace register sets, core dumps): NZCV and DIT, with
/// all other bits always 0 (EL0t, AArch64).
pub(crate) fn pstate_of(s: &CpuState) -> u64 {
    (s.nzcv & PSTATE_NZCV) | (u64::from(s.dit) << 24)
}

/// Restores NZCV and DIT from a pstate ([`user_pstate`]).
pub(crate) fn set_pstate(s: &mut CpuState, pstate: u64) {
    let p = user_pstate(pstate);
    s.nzcv = p & PSTATE_NZCV;
    s.dit = u32::from(p & PSTATE_DIT != 0);
}

/// The bits of a pstate from a signal frame or from the tracer that the guest may change: NZCV,
/// plus DIT when DIT is advertised (Linux's `valid_native_regs` only clears the RES0 bits and
/// restores DIT as given; on hardware without DIT the bit has no effect).
pub(crate) fn user_pstate(pstate: u64) -> u64 {
    let dit = tuxport_engine::guest_hwcap() & tuxport_engine::hwcap::DIT != 0;
    pstate & (PSTATE_NZCV | if dit { PSTATE_DIT } else { 0 })
}

/// A thread's ptrace state ([`Task::trace`]), accessed only while holding the BKL.
#[derive(Default)]
pub(crate) struct Trace {
    /// The tracer process; None means not traced.
    pub tracer: Option<Tid>,
    /// The tracer's credentials when tracing began (Linux's `ptracer_cred`), judged by
    /// [`Trace::tracer_capable`].
    pub cred: Option<crate::cred::Cred>,
    /// Traced with PTRACE_SEIZE: group-stops and PTRACE_INTERRUPT are reported as
    /// `PTRACE_EVENT_STOP`, and so is the start of an auto-attached new thread; execve does not
    /// send it SIGTRAP.
    pub seized: bool,
    /// The options of PTRACE_SETOPTIONS.
    pub options: u32,
    /// How to run after resuming.
    pub mode: Mode,
    /// Why it stopped; None means running.
    pub stop: Option<Stop>,
    /// This stop has already been taken by the tracer's wait.
    pub reported: bool,
    /// The tracer resumed it: the signal to inject (0 means none).
    pub resume: Option<i32>,
    /// PTRACE_LISTEN: still stopped, but reported once more when the group-stop ends or a
    /// PTRACE_INTERRUPT arrives.
    pub listening: bool,
    /// Stop once at `PTRACE_EVENT_STOP` (PTRACE_INTERRUPT, a new thread auto-attached under
    /// SEIZE).
    pub trap: bool,
    /// The stop signal of the group-stop to join: set when the thread group is stopped, cleared
    /// once the thread has stopped.
    pub group_stop: Option<Signal>,
    /// The registers at the stop.
    pub regs: Option<Box<Regs>>,
    /// The siginfo of the stop (Linux's `last_siginfo`), encoded as `siginfo_t`; a group-stop not
    /// under SEIZE has none, and PTRACE_GETSIGINFO is EINVAL.
    pub siginfo: Option<[u8; 128]>,
    /// PTRACE_GETEVENTMSG.
    pub message: u64,
    /// The syscall number: the tracer may change it at an entry stop (`NT_ARM_SYSTEM_CALL`); -1
    /// skips the syscall.
    pub syscallno: i32,
    /// Hardware breakpoints and watchpoints set by the tracer; when `hw_changed`, handed to the
    /// CPU on resume.
    pub hw: HwDebug,
    pub hw_changed: bool,
}

impl Trace {
    /// The status reported to the tracer on a stop (Linux's `exit_code`): the wait4 status word is
    /// it shifted left by 8 and ORed with 0x7f, and waitid's `si_status` is it.
    pub fn exit_code(&self) -> Option<i32> {
        let trap = Signal::TRAP.number();
        Some(match self.stop? {
            Stop::Signal(s) | Stop::Group(s) => s.number(),
            Stop::Syscall(_) if self.options & opt::TRACESYSGOOD != 0 => trap | 0x80,
            Stop::Syscall(_) => trap,
            Stop::Event(e, s) => s.number() | (e as i32) << 8,
        })
    }

    /// The tracer has enabled event `e`.
    pub fn enabled(&self, e: u32) -> bool {
        self.tracer.is_some() && self.options & (1 << e) != 0
    }

    /// In a ptrace-stop and not in PTRACE_LISTEN: the tracer can send it requests.
    pub fn stopped(&self) -> bool {
        self.stop.is_some() && !self.listening
    }

    /// Whether the tracer had `CAP_SYS_PTRACE` over user namespace `ns` when tracing began
    /// (Linux's `ptracer_capable`; true when untraced). Without it, an execve of a setuid program
    /// or one with file capabilities gains no new privileges, and the memory of a tracee that is
    /// not dumpable cannot be read or written with `PTRACE_PEEK*`/`POKE*`, even if the tracer has
    /// gained the capability since.
    pub fn tracer_capable(&self, ns: &crate::cred::UserNs) -> bool {
        self.cred
            .as_ref()
            .is_none_or(|c| c.ns_capable(ns, crate::cred::cap::SYS_PTRACE))
    }
}

/// The thread is traced.
pub(crate) fn traced(task: &Task) -> bool {
    task.trace.borrow().tracer.is_some()
}

/// Number of hardware breakpoint and watchpoint slots, the same as common Cortex-A cores (BRPs
/// and WRPs of `ID_AA64DFR0_EL1`).
pub(crate) const BRK_SLOTS: usize = 6;
pub(crate) const WATCH_SLOTS: usize = 4;

/// Hardware breakpoints and watchpoints (the 16 sets of address and control word of
/// `NT_ARM_HW_BREAK` and `NT_ARM_HW_WATCH`, of which the first [`BRK_SLOTS`] and [`WATCH_SLOTS`]
/// are used). Enabled ones are already normalized as Linux does: watchpoint addresses aligned to
/// 8 bytes, with the offset moved into the byte select (BAS), and privilege level EL0
/// (`syscall::ptrace`). As on Linux they belong to the thread: threads created by fork and clone
/// have none, execve clears them, PTRACE_DETACH does not. Unlike Linux, [`detach`] resets this
/// copy while the CPU keeps the slots armed until execve, so a later tracer reads empty slots
/// that still trap.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HwDebug {
    pub brk: [(u64, u32); 16],
    pub watch: [(u64, u32); 16],
}

impl HwDebug {
    /// Hands them to the engine (`CpuState::set_hw_debug`): the addresses of enabled breakpoints,
    /// and the [start, end) and kind of enabled watchpoints (the control word's LSC bits are the
    /// engine's `WATCH_LOAD` and `WATCH_STORE`).
    pub fn apply(&self, s: &mut CpuState) {
        s.set_hw_debug(&self.breakpoints(), &self.watchpoints());
    }

    fn breakpoints(&self) -> Vec<u64> {
        self.brk[..BRK_SLOTS]
            .iter()
            .filter(|(_, ctrl)| ctrl & 1 != 0)
            .map(|&(addr, _)| addr)
            .collect()
    }

    fn watchpoints(&self) -> Vec<(u64, u64, u32)> {
        self.watch[..WATCH_SLOTS]
            .iter()
            .filter(|(_, ctrl)| ctrl & 1 != 0)
            .map(|&(addr, ctrl)| {
                let bas = ctrl >> 5 & 0xff;
                let lo = addr + u64::from(bas.trailing_zeros());
                (lo, lo + u64::from(bas.count_ones()), ctrl >> 3 & 3)
            })
            .collect()
    }
}

// ---- Signal-delivery stops and group-stops ----

/// Joins a group-stop (the thread group was stopped by a stop signal): stops in a ptrace-stop and
/// reports to the tracer. Does nothing when there is no group-stop to join. Returns false when
/// the thread is killed.
pub(crate) fn group_stop(cx: &mut Cx<'_>, cpu: &mut Cpu) -> bool {
    let (sig, seized) = {
        let mut t = cx.task.trace.borrow_mut();
        match t.group_stop.take() {
            Some(sig) if t.tracer.is_some() => (sig, t.seized),
            _ => return true,
        }
    };
    let (why, info) = if seized {
        let si_code = sig.number() | (event::STOP as i32) << 8;
        let info = notify_info(
            sig,
            si_code,
            cx.pid_out(cx.tid()),
            cx.cred().uid_out(cx.cred().ruid),
        );
        (Stop::Event(event::STOP, sig), Some(info))
    } else {
        (Stop::Group(sig), None)
    };
    stop(cx, cpu, why, info, 0).is_some()
}

/// PTRACE_INTERRUPT, or a new thread auto-attached under SEIZE: stops once at
/// `PTRACE_EVENT_STOP`. Does nothing when no stop is due. Returns false when the thread is
/// killed.
pub(crate) fn trap(cx: &mut Cx<'_>, cpu: &mut Cpu) -> bool {
    {
        let mut t = cx.task.trace.borrow_mut();
        if !std::mem::take(&mut t.trap) || t.tracer.is_none() {
            return true;
        }
    }
    let sig = Signal::TRAP;
    let si_code = sig.number() | (event::STOP as i32) << 8;
    let info = notify_info(
        sig,
        si_code,
        cx.pid_out(cx.tid()),
        cx.cred().uid_out(cx.cred().ruid),
    );
    stop(cx, cpu, Stop::Event(event::STOP, sig), Some(info), 0).is_some()
}

/// Before signal `sig` is delivered (signal-delivery stop). Returns the signal the tracer lets
/// through and its siginfo, `Ok(None)` when the tracer suppresses delivery, and `Err(())` when the
/// thread is killed.
pub(crate) fn signal_stop(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    sig: Signal,
    info: SigInfo,
) -> Result<Option<(Signal, SigInfo)>, ()> {
    if !traced(cx.task) {
        return Ok(Some((sig, info)));
    }
    let raw = info.encode(sig);
    let data = stop(cx, cpu, Stop::Signal(sig), Some(raw), 0).ok_or(())?;
    let Some(new) = Signal::new(data) else {
        return Ok(None);
    };
    // The siginfo as changed by PTRACE_SETSIGINFO (Linux's ptrace_signal)
    let now = cx.task.trace.borrow_mut().siginfo.take().unwrap_or(raw);
    let signo = i32::from_le_bytes(now[..4].try_into().unwrap());
    let info = if new.number() != signo {
        // The signal was replaced without a siginfo: treat it as sent by the tracer with kill
        let tracer = cx.task.trace.borrow().tracer.unwrap_or(0);
        let uid = cx.kr().cred_of(tracer).map_or(0, |c| c.ruid);
        // Linux's ptrace_signal: the tracer's real user ID as seen in the tracee's namespace
        SigInfo::kill(code::SI_USER, cx.pid_out(tracer), cx.cred().uid_out(uid))
    } else if now == raw {
        info
    } else {
        SigInfo::from_raw(&now)
    };
    // The replacement signal is blocked: queue it again, to be delivered once unblocked
    if cx.task.sigmask.get() & new.bit() != 0 {
        signal::send_to_task(cx.kr(), cx.task, new, info);
        return Ok(None);
    }
    Ok(Some((new, info)))
}

/// While single-stepping, a signal handler has just been set up (its first instruction not yet
/// executed): stops once, as Linux's `ptrace_notify(SIGTRAP)` does; the tracer sees a siginfo of
/// SIGTRAP whose `si_code` is also SIGTRAP and whose sender is the thread itself. The signal of
/// this stop is not delivered. Returns false when the thread is killed.
pub(crate) fn handler_entered(cx: &mut Cx<'_>, cpu: &mut Cpu) -> bool {
    if !traced(cx.task) {
        return true;
    }
    let sig = Signal::TRAP;
    let info = notify_info(
        sig,
        sig.number(),
        cx.pid_out(cx.tid()),
        cx.cred().uid_out(cx.cred().ruid),
    );
    stop(cx, cpu, Stop::Event(0, sig), Some(info), 0).is_some()
}

// ---- Single-step ----

/// PTRACE_SINGLESTEP made the thread execute one instruction (not an svc): as with arm64 Linux's
/// single-step exception, delivers SIGTRAP (`TRAP_TRACE`, with the address of the next
/// instruction) and reports to the tracer in a signal-delivery stop.
pub(crate) fn step_done(cx: &mut Cx<'_>, cpu: &Cpu) {
    let info = SigInfo::fault(code::TRAP_TRACE, cpu.state().pc, 0);
    signal::force(cx, Signal::TRAP, info);
}

/// The single-stepped instruction was an svc: reports after the syscall completes. As on arm64
/// Linux, this SIGTRAP is `SI_USER` with no sender; the hardware single-step state is still
/// pending, so the next PTRACE_SINGLESTEP executes no instruction and reports `TRAP_TRACE` again
/// in place (`Task::step_again`).
pub(crate) fn syscall_stepped(cx: &mut Cx<'_>) {
    if cx.task.stepping.load(Ordering::Relaxed) {
        signal::force(cx, Signal::TRAP, SigInfo::kill(code::SI_USER, 0, 0));
        cx.task.step_again.store(true, Ordering::Relaxed);
    }
}

// ---- Syscall stops ----

/// The values of PTRACE_GETEVENTMSG at syscall entry and exit stops.
pub(crate) const EVENTMSG_SYSCALL_ENTRY: u64 = 1;
pub(crate) const EVENTMSG_SYSCALL_EXIT: u64 = 2;

/// The entry of a syscall (number `nr`). Returns None when there is no tracer, or when it is a
/// vDSO clock stub (on Linux these do not enter the kernel, so tracers do not see them).
/// Otherwise records the number, as Linux's `regs->syscallno` does: when resumed other than by
/// PTRACE_SYSCALL, event stops along the way (`PTRACE_EVENT_FORK` and so on), exit stops after a
/// later switch to PTRACE_SYSCALL, and `NT_ARM_SYSTEM_CALL` also read this syscall's number;
/// returns whether PTRACE_SYSCALL stops at the entry. Whether to stop at the exit is decided
/// again by [`syscall_exit`] from the state at that time.
pub(crate) fn syscall_start(cx: &Cx<'_>, cpu: &Cpu, nr: u64) -> Option<bool> {
    if !traced(cx.task) || crate::vdso::clock_stub(cx, cpu, nr) {
        return None;
    }
    let mut t = cx.task.trace.borrow_mut();
    t.syscallno = nr as i32;
    Some(t.mode == Mode::Syscall)
}

/// Stops at syscall entry (the number was recorded by [`syscall_start`]). Returns the number as
/// changed by the tracer (-1 skips this syscall); None when the thread is killed.
pub(crate) fn syscall_entry(cx: &mut Cx<'_>, cpu: &mut Cpu) -> Option<u64> {
    syscall_stop(cx, cpu, true).then(|| cx.task.trace.borrow().syscallno as i64 as u64)
}

/// A seccomp filter returned TRACE (with data `data`): stops once at `PTRACE_EVENT_SECCOMP`, and
/// the tracer may change the number (`NT_ARM_SYSTEM_CALL`, -1 skips) and the registers. Returns
/// the resulting number; None when the thread is killed.
pub(crate) fn seccomp_stop(cx: &mut Cx<'_>, cpu: &mut Cpu, nr: u64, data: u32) -> Option<u64> {
    cx.task.trace.borrow_mut().syscallno = nr as i32;
    event(cx, cpu, event::SECCOMP, u64::from(data))
        .then(|| cx.task.trace.borrow().syscallno as i64 as u64)
}

/// Stops at syscall exit (if the thread has by then been resumed with PTRACE_SYSCALL; rechecked
/// as arm64 Linux's `syscall_trace_exit` does, whether or not it stopped at entry). Returns false
/// when the thread is killed.
pub(crate) fn syscall_exit(cx: &mut Cx<'_>, cpu: &mut Cpu) -> bool {
    let nr = cx.task.trace.borrow().syscallno as i64 as u64;
    !syscall_traced(cx, cpu, nr) || syscall_stop(cx, cpu, false)
}

/// Whether this syscall (number `nr`) stops at entry and exit after PTRACE_SYSCALL resumed the
/// thread. vDSO clock stubs do not count: on Linux they do not enter the kernel, so tracers do
/// not see them.
pub(crate) fn syscall_traced(cx: &Cx<'_>, cpu: &Cpu, nr: u64) -> bool {
    {
        let t = cx.task.trace.borrow();
        if t.tracer.is_none() || t.mode != Mode::Syscall {
            return false;
        }
    }
    !crate::vdso::clock_stub(cx, cpu, nr)
}

/// A syscall stop: as on arm64 Linux, x7 is 0 (entry) or 1 (exit) during the stop, and the tracer
/// can neither see nor change its original value. Returns false when the thread is killed.
fn syscall_stop(cx: &mut Cx<'_>, cpu: &mut Cpu, entry: bool) -> bool {
    let options = cx.task.trace.borrow().options;
    let sig = Signal::TRAP;
    let si_code = sig.number()
        | if options & opt::TRACESYSGOOD != 0 {
            0x80
        } else {
            0
        };
    let info = notify_info(
        sig,
        si_code,
        cx.pid_out(cx.tid()),
        cx.cred().uid_out(cx.cred().ruid),
    );
    let message = if entry {
        EVENTMSG_SYSCALL_ENTRY
    } else {
        EVENTMSG_SYSCALL_EXIT
    };
    let saved = std::mem::replace(&mut cpu.state_mut().x[7], u64::from(!entry));
    let data = stop(cx, cpu, Stop::Syscall(entry), Some(info), message);
    cpu.state_mut().x[7] = saved;
    match data {
        Some(data) => {
            inject(cx, data);
            true
        }
        None => false,
    }
}

/// A signal the tracer injects at a syscall stop: treated, as on Linux, as a new signal the
/// kernel sends to the thread itself.
fn inject(cx: &Cx<'_>, data: i32) {
    if let Some(sig) = Signal::new(data) {
        signal::send_to_task(cx.kr(), cx.task, sig, SigInfo::kernel());
    }
}

// ---- Event stops ----

/// Whether a thread `child` created by clone (`flags` and `exit_signal` are clone's arguments) is
/// traced: when the parent thread is traced, the matching event is enabled (or `CLONE_PTRACE` was
/// used) and there is no `CLONE_UNTRACED`, the new thread is traced by the same tracer and stops
/// before executing its first instruction (with a pending SIGSTOP when not under SEIZE). Returns
/// the event the parent thread reports.
pub(crate) fn clone(parent: &Task, child: &Task, flags: u64, exit_signal: i32) -> Option<u32> {
    const CLONE_VFORK: u64 = 0x4000;
    const CLONE_PTRACE: u64 = 0x2000;
    const CLONE_UNTRACED: u64 = 0x0080_0000;
    let p = parent.trace.borrow();
    if p.tracer.is_none() || flags & CLONE_UNTRACED != 0 {
        return None;
    }
    let e = if flags & CLONE_VFORK != 0 {
        event::VFORK
    } else if exit_signal != Signal::CHLD.number() {
        event::CLONE
    } else {
        event::FORK
    };
    let report = p.enabled(e).then_some(e);
    if report.is_none() && flags & CLONE_PTRACE == 0 {
        return None;
    }
    {
        let mut c = child.trace.borrow_mut();
        c.tracer = p.tracer;
        c.cred = p.cred.clone();
        c.seized = p.seized;
        c.options = p.options;
        if p.seized {
            c.trap = true;
        } else {
            let stop = SigInfo::default();
            child.pending.borrow_mut().add(Signal::STOP, stop, false);
        }
    }
    // SAFETY: the new thread's Cpu still exists, and the caller holds the BKL
    unsafe { child.sync.kicker.kick() };
    report
}

/// execve: clears hardware breakpoints and watchpoints, as Linux's `flush_ptrace_hw_breakpoint`
/// does.
pub(crate) fn exec_hw_flush(cx: &Cx<'_>, cpu: &mut Cpu) {
    let mut t = cx.task.trace.borrow_mut();
    if t.hw != HwDebug::default() {
        t.hw = HwDebug::default();
        t.hw_changed = false;
        cpu.state_mut().set_hw_debug(&[], &[]);
    }
}

/// After a successful execve: `PTRACE_EVENT_EXEC` (the message is the tid before the execve); if
/// the event is not enabled and the tracee is not under SEIZE, sends itself SIGTRAP. Returns false
/// when the thread is killed.
pub(crate) fn exec(cx: &mut Cx<'_>, cpu: &mut Cpu, old_tid: Tid) -> bool {
    let (traced, seized, enabled) = {
        let t = cx.task.trace.borrow();
        (t.tracer.is_some(), t.seized, t.enabled(event::EXEC))
    };
    if enabled {
        let old = tracer_pid_of(cx, old_tid);
        return event(cx, cpu, event::EXEC, old as u64);
    }
    if traced && !seized {
        let cred = cx.cred();
        let info = SigInfo::kill(
            code::SI_USER,
            cx.pid_out(cx.tgid()),
            cred.uid_out(cred.ruid),
        );
        signal::send_to_task(cx.kr(), cx.task, Signal::TRAP, info);
    }
    true
}

/// The id of thread `tid` as seen by the process tracing it (`ptrace_event_pid`), or as seen by
/// itself when there is no tracer.
pub(crate) fn tracer_pid_of(cx: &Cx<'_>, tid: Tid) -> Tid {
    match cx.task.trace.borrow().tracer {
        Some(tracer) => cx.kr().pid_seen_by(tracer, tid),
        None => cx.pid_out(tid),
    }
}

/// The thread is about to exit (exit, exit_group, a fatal signal's default action):
/// `PTRACE_EVENT_EXIT`, with the wait status word as the message. exit_group has already asked
/// every thread to exit, including the caller; the caller still stops and reports, and stops
/// waiting if killed by SIGKILL during the stop.
pub(crate) fn exit(cx: &mut Cx<'_>, cpu: &mut Cpu, status: ExitStatus) {
    cx.task.begin_exit(cx.kr(), &cx.t.group, status);
    if !cx.task.trace.borrow().enabled(event::EXIT) {
        return;
    }
    let killed = cx.task.killed.replace(false);
    cx.kr().cgroups.task_changed(cx.t.group.borrow().cgroup);
    if event(cx, cpu, event::EXIT, status.wait_status() as u64) {
        cx.task.killed.set(killed);
        cx.kr().cgroups.task_changed(cx.t.group.borrow().cgroup);
    }
}

/// Stops at a `PTRACE_EVENT_*` (when the tracer has enabled the event). Returns false when the
/// thread is killed.
pub(crate) fn event(cx: &mut Cx<'_>, cpu: &mut Cpu, e: u32, message: u64) -> bool {
    if !cx.task.trace.borrow().enabled(e) {
        return true;
    }
    let sig = Signal::TRAP;
    let si_code = sig.number() | (e as i32) << 8;
    let info = notify_info(
        sig,
        si_code,
        cx.pid_out(cx.tid()),
        cx.cred().uid_out(cx.cred().ruid),
    );
    stop(cx, cpu, Stop::Event(e, sig), Some(info), message).is_some()
}

// ---- Stopping and notifying the tracer ----

/// Stops in a ptrace-stop: notifies the tracer and waits for it to resume the thread. Returns the
/// signal the tracer injects (0 means none); None when the thread is killed (SIGKILL). `info` is
/// the siginfo of this stop and `message` the value of PTRACE_GETEVENTMSG.
pub(crate) fn stop(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    why: Stop,
    info: Option<[u8; 128]>,
    message: u64,
) -> Option<i32> {
    {
        let mut t = cx.task.trace.borrow_mut();
        if t.tracer.is_none() {
            return Some(0);
        }
        t.stop = Some(why);
        t.reported = false;
        t.resume = None;
        t.listening = false;
        t.regs = Some(Box::new(Regs::save(cpu.state())));
        t.siginfo = info;
        t.message = message;
    }
    notify(cx.kr(), cx.task);
    let (tid, uid) = (cx.pid_out(cx.tid()), cx.cred().uid_out(cx.cred().ruid));
    let resumed = loop {
        if cx.task.killed.get() {
            break None;
        }
        let group_stopped = cx.t.group.borrow().stopped;
        let retrap = {
            let mut t = cx.task.trace.borrow_mut();
            if let Some(sig) = t.resume.take() {
                break Some(sig);
            }
            // The tracer exited without taking this stop: the signal to be delivered is delivered
            // as usual ([`detach`])
            if t.tracer.is_none() {
                break Some(match t.stop {
                    Some(Stop::Signal(sig)) => sig.number(),
                    _ => 0,
                });
            }
            // After PTRACE_LISTEN the group-stop ended (SIGCONT), or a PTRACE_INTERRUPT arrived:
            // report PTRACE_EVENT_STOP once more
            if t.listening && (t.trap || !group_stopped) {
                let sig = match t.stop {
                    Some(Stop::Event(_, sig)) if group_stopped => sig,
                    _ => Signal::TRAP,
                };
                t.listening = false;
                t.trap = false;
                t.stop = Some(Stop::Event(event::STOP, sig));
                t.reported = false;
                let si_code = sig.number() | (event::STOP as i32) << 8;
                t.siginfo = Some(notify_info(sig, si_code, tid, uid));
                true
            } else {
                false
            }
        };
        if retrap {
            notify(cx.kr(), cx.task);
        }
        let _ = cx.wait_alone(None);
    };
    let regs = {
        let mut t = cx.task.trace.borrow_mut();
        t.stop = None;
        t.listening = false;
        t.regs.take()
    };
    if let Some(regs) = regs {
        regs.restore(cpu.state_mut());
    }
    // The tracer changed hardware breakpoints or watchpoints (PTRACE_SETREGSET)
    let hw = {
        let mut t = cx.task.trace.borrow_mut();
        std::mem::take(&mut t.hw_changed).then_some(t.hw)
    };
    if let Some(hw) = hw {
        hw.apply(cpu.state_mut());
    }
    resumed
}

/// Notifies the tracer that the thread stopped: wakes its wait, and sends SIGCHLD (`CLD_TRAPPED`,
/// or `CLD_STOPPED` for a group-stop) unless it ignores SIGCHLD or has set SA_NOCLDSTOP.
fn notify(k: &Kernel, task: &Task) {
    let (tracer, why, exit_code) = {
        let t = task.trace.borrow();
        match (t.tracer, t.stop, t.exit_code()) {
            (Some(tracer), Some(why), Some(c)) => (tracer, why, c),
            _ => return,
        }
    };
    request_wake_on(wake::CHILD);
    let Some(g) = k.groups.get(&tracer) else {
        return;
    };
    let act = g.borrow().sighand.borrow().get(Signal::CHLD);
    if act.handler == SigAction::IGNORE || act.flags & sa::NOCLDSTOP != 0 {
        return;
    }
    let why = match why {
        Stop::Group(_) | Stop::Event(event::STOP, _) => code::CLD_STOPPED,
        _ => code::CLD_TRAPPED,
    };
    let pid = k.pid_seen_by(tracer, task.tid.get());
    if !k.groups.contains_key(&task.tgid) {
        return;
    }
    let info = signal::child_info(&task.cred.borrow(), &g.borrow().cred, |uid| {
        SigInfo::child_event(pid, uid, why, exit_code & 0x7f)
    });
    signal::send_to_group(k, tracer, Signal::CHLD, info);
}

/// The siginfo sent by the tracer: carries a sender as a kill would (what Linux's
/// `ptrace_do_notify` fills in is the tracee thread itself).
fn notify_info(signo: Signal, si_code: i32, pid: Tid, uid: u32) -> [u8; 128] {
    SigInfo::kill(si_code, pid, uid).encode(signo)
}

// ---- Detaching ----

/// The tracer stops tracing `task` (PTRACE_DETACH, tracer exit): a stopped thread is resumed, and
/// the registers the tracer changed are written back. `sig` is the signal PTRACE_DETACH delivers;
/// it is None when the tracer exited: if this stop has already been taken by wait, the signal of a
/// signal-delivery stop is dropped (Linux's `wait_task_stopped` clears `exit_code` to 0 when
/// taking it; the delivery as usual that ptrace(2) describes holds only while it is not yet
/// taken); if not taken, it is delivered as usual ([`stop`]). When delve exits after attaching
/// without detaching, the process stays stopped by the attach's SIGSTOP. When the thread group is
/// stopped, the thread returns to the group-stop. Hardware breakpoints and watchpoints stay set,
/// as on arm64 Linux (its `ptrace_disable` leaves them; only execve flushes them), so a later
/// tracer reads the slots that still trap.
pub(crate) fn detach(task: &Task, sig: Option<i32>) {
    {
        let mut t = task.trace.borrow_mut();
        let (stop, regs, reported, syscallno) = (t.stop, t.regs.take(), t.reported, t.syscallno);
        let (hw, hw_changed) = (t.hw, t.hw_changed);
        *t = Trace::default();
        t.hw = hw;
        t.hw_changed = hw_changed;
        if stop.is_some() {
            t.stop = stop;
            t.regs = regs;
            // A thread detached at a syscall-entry or seccomp stop goes on with its syscall,
            // whose number the stop returns (without this it became 0, io_setup)
            t.syscallno = syscallno;
            t.resume = sig.or(reported.then_some(0));
        }
    }
    task.syscall_traced.store(false, Ordering::Relaxed);
    task.stepping.store(false, Ordering::Relaxed);
    signal::wake(task);
}

/// Tracer `tgid` has exited: tracees that set `PTRACE_O_EXITKILL` are killed, the rest are no
/// longer traced.
pub(crate) fn tracer_exited(k: &mut Kernel, tgid: Tid) {
    let tracees: Vec<_> = k
        .tasks
        .values()
        .filter(|t| t.trace.borrow().tracer == Some(tgid))
        .cloned()
        .collect();
    for task in tracees {
        if task.trace.borrow().options & opt::EXITKILL != 0 {
            signal::send_to_task(k, &task, Signal::KILL, SigInfo::kernel());
        }
        detach(&task, None);
    }
    // Ends not yet taken: the traced processes are handed to their parents (Linux's
    // `__ptrace_detach`)
    let (gone, kept) = std::mem::take(&mut k.trace_exits)
        .into_iter()
        .partition(|e| e.tracer == tgid);
    k.trace_exits = kept;
    for e in gone {
        released(k, &e);
    }
    for g in k.groups.values() {
        let mut g = g.borrow_mut();
        if g.leader_tracer == Some(tgid) {
            g.leader_tracer = None;
        }
    }
}

// ---- Exits of traced threads ----

/// A traced thread has exited, waiting for the tracer's wait to take it ([`Kernel::trace_exits`]).
pub(crate) struct TraceExit {
    pub tracer: Tid,
    pub pid: Tid,
    /// The pid as the tracer sees it (computed when reported: by the time wait takes it, the
    /// thread's id may no longer exist).
    pub seen: Tid,
    /// The process group at exit (wait can wait by process group).
    pub pgid: Tid,
    pub status: ExitStatus,
    /// The end of the whole process: the process goes to the parent only after the tracer takes
    /// it ([`group_finished`]).
    pub process: bool,
    /// The credentials at exit: the si_uid of SIGCHLD and waitid is their real user ID.
    pub cred: crate::cred::Cred,
}

/// Traced thread `task` has exited (already removed from `kernel.tasks`): a thread that is not the
/// main thread is reported to the tracer on its own; the main thread records the tracer and is
/// reported when the whole process ends ([`group_finished`]). `status` is how the thread itself
/// ended.
pub(crate) fn tracee_exited(
    k: &mut Kernel,
    task: &Task,
    group: &mut ThreadGroup,
    status: ExitStatus,
) {
    let Some(tracer) = task.trace.borrow().tracer else {
        return;
    };
    let tid = task.tid.get();
    if tid == group.tgid {
        group.leader_tracer = Some(tracer);
        return;
    }
    let status = group.exiting.unwrap_or(status);
    let exit = TraceExit {
        tracer,
        pid: tid,
        seen: 0,
        pgid: group.pgid,
        status,
        process: false,
        cred: task.cred.borrow().clone(),
    };
    report_exit(k, exit);
}

/// Process `group` has ended: when its main thread is traced by a process other than its parent,
/// as on Linux it is first reported only to the tracer (with SIGCHLD), and handed to the parent
/// only after the tracer takes it in wait ([`released`]) or exits itself. Returns true when
/// deferred this way. When the parent itself is the tracer, the ordinary wait reports it.
pub(crate) fn group_finished(
    k: &mut Kernel,
    group: &Rc<RefCell<ThreadGroup>>,
    status: ExitStatus,
) -> bool {
    let exit = {
        let g = group.borrow();
        match g.leader_tracer {
            Some(tracer) if tracer != g.ppid && k.groups.contains_key(&tracer) => TraceExit {
                tracer,
                pid: g.tgid,
                seen: 0,
                pgid: g.pgid,
                status,
                process: true,
                cred: g.cred.clone(),
            },
            _ => return false,
        }
    };
    {
        let mut g = group.borrow_mut();
        g.zombie = Some(status);
        g.exit_tracer = Some(exit.tracer);
    }
    report_exit(k, exit);
    true
}

fn report_exit(k: &mut Kernel, mut exit: TraceExit) {
    if !k.groups.contains_key(&exit.tracer) {
        return;
    }
    exit.seen = k.pid_seen_by(exit.tracer, exit.pid);
    let tracer = exit.tracer;
    let info = signal::exit_info(
        &exit.cred,
        &k.groups[&tracer].borrow().cred,
        exit.seen,
        exit.status,
    );
    k.trace_exits.push(exit);
    request_wake_on(wake::CHILD);
    signal::send_to_group(k, tracer, Signal::CHLD, info);
}

/// The tracer has taken the end `e` of a traced thread (or exited without taking it): the end of
/// the whole process is handed to the parent.
pub(crate) fn released(k: &mut Kernel, e: &TraceExit) {
    if !e.process {
        return;
    }
    if let Some(group) = k.groups.get(&e.pid).cloned()
        && group.borrow().exit_tracer == Some(e.tracer)
    {
        crate::process::tracer_released(k, &group);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hw_debug_ranges_come_from_the_byte_selects() {
        let mut hw = HwDebug::default();
        // An 8-byte-aligned address, BAS selecting bytes 4–7, both reads and writes; disabled
        // ones and ones beyond the slot count do not count
        hw.watch[0] = (0x1000, 0xf0 << 5 | 3 << 3 | 4 | 1);
        hw.watch[1] = (0x2000, 0x01 << 5 | 2 << 3 | 4);
        hw.watch[WATCH_SLOTS] = (0x3000, 0x01 << 5 | 2 << 3 | 4 | 1);
        hw.brk[0] = (0x4000, 0xf << 5 | 4 | 1);
        assert_eq!(hw.watchpoints(), vec![(0x1004, 0x1008, 3)]);
        assert_eq!(hw.breakpoints(), vec![0x4000]);
    }
}
