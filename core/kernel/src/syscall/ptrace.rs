//! The tracer side of ptrace(2), plus process_vm_readv and process_vm_writev (the kernel/ptrace
//! map). The tracee side is in [`crate::ptrace`].

use std::rc::Rc;
use std::sync::atomic::Ordering;

use tuxport_engine::PAGE_SIZE;

use super::proc::may_ptrace;
use super::{SysResult, get, put};
use crate::errno::Errno;
use crate::kernel::{Cx, INIT};
use crate::process::Signal;
use crate::ptrace::{
    BRK_SLOTS, EVENTMSG_SYSCALL_ENTRY, EVENTMSG_SYSCALL_EXIT, HwDebug, Mode, Regs, Stop, Trace,
    WATCH_SLOTS, event, opt,
};
use crate::signal::{self, SigInfo};
use crate::task::Task;
use crate::uaccess::UserSlice;

const TRACEME: i64 = 0;
const PEEKTEXT: i64 = 1;
const PEEKDATA: i64 = 2;
const POKETEXT: i64 = 4;
const POKEDATA: i64 = 5;
const CONT: i64 = 7;
const KILL: i64 = 8;
const SINGLESTEP: i64 = 9;
const ATTACH: i64 = 16;
const DETACH: i64 = 17;
const SYSCALL: i64 = 24;
const SETOPTIONS: i64 = 0x4200;
const GETEVENTMSG: i64 = 0x4201;
const GETSIGINFO: i64 = 0x4202;
const SETSIGINFO: i64 = 0x4203;
const GETREGSET: i64 = 0x4204;
const SETREGSET: i64 = 0x4205;
const SEIZE: i64 = 0x4206;
const INTERRUPT: i64 = 0x4207;
const LISTEN: i64 = 0x4208;
const GETSIGMASK: i64 = 0x420a;
const SETSIGMASK: i64 = 0x420b;
const SECCOMP_GET_FILTER: i64 = 0x420c;
const SECCOMP_GET_METADATA: i64 = 0x420d;
const GET_SYSCALL_INFO: i64 = 0x420e;

/// Regset types (`NT_*`).
const NT_PRSTATUS: u64 = 1;
const NT_PRFPREG: u64 = 2;
const NT_ARM_TLS: u64 = 0x401;
const NT_ARM_HW_BREAK: u64 = 0x402;
const NT_ARM_HW_WATCH: u64 = 0x403;
const NT_ARM_SYSTEM_CALL: u64 = 0x404;

/// `AUDIT_ARCH_AARCH64` (the arch of PTRACE_GET_SYSCALL_INFO).
const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
/// The debug architecture (`ID_AA64DFR0_EL1.DebugVer`) is ARMv8.
const DEBUG_ARCH_V8: u32 = 6;

pub(super) fn ptrace(cx: &mut Cx<'_>, request: i64, pid: i32, addr: u64, data: u64) -> SysResult {
    match request {
        TRACEME => return traceme(cx),
        ATTACH | SEIZE => return attach(cx, pid, request == SEIZE, addr, data),
        _ => {}
    }
    let task = cx.kr().tasks.get(&pid).cloned().ok_or(Errno::ESRCH)?;
    // Requests may only be made to threads we trace; except for PTRACE_KILL and PTRACE_INTERRUPT,
    // the thread must also be in a ptrace-stop (Linux's ptrace_check_attach)
    {
        let t = task.trace.borrow();
        if t.tracer != Some(cx.tgid()) || !(matches!(request, KILL | INTERRUPT) || t.stopped()) {
            return Err(Errno::ESRCH);
        }
    }
    match request {
        PEEKTEXT | PEEKDATA => peek(cx, &task, addr, data),
        POKETEXT | POKEDATA => {
            if !may_access_vm(cx, &task) {
                return Err(Errno::EIO);
            }
            // As with Linux's FOLL_FORCE, private mappings without write permission can be
            // written: setting breakpoints
            let mm = task.res.borrow().mm.clone();
            mm.borrow_mut()
                .write_force(addr, &data.to_le_bytes(), &cx.task.sync)
                .map_err(|_| Errno::EIO)?;
            Ok(0)
        }
        CONT => resume(&task, Mode::Cont, data),
        SYSCALL => resume(&task, Mode::Syscall, data),
        SINGLESTEP => resume(&task, Mode::Step, data),
        DETACH => {
            if data > Signal::MAX as u64 {
                return Err(Errno::EIO);
            }
            crate::ptrace::detach(&task, Some(data as i32));
            Ok(0)
        }
        KILL => {
            signal::send_to_task(cx.kr(), &task, Signal::KILL, SigInfo::kernel());
            Ok(0)
        }
        SETOPTIONS => {
            if data & !(opt::MASK as u64) != 0 {
                return Err(Errno::EINVAL);
            }
            check_options(cx, data as u32)?;
            task.trace.borrow_mut().options = data as u32;
            Ok(0)
        }
        GETEVENTMSG => {
            let message = task.trace.borrow().message;
            put(cx, data, message.to_le_bytes()).map(|()| 0)
        }
        GETSIGINFO => {
            let info = task.trace.borrow().siginfo.ok_or(Errno::EINVAL)?;
            cx.mm().write(UserSlice::new(data, 128), &info)?;
            Ok(0)
        }
        SETSIGINFO => {
            let mut info = [0u8; 128];
            cx.mm().read(UserSlice::new(data, 128), &mut info)?;
            let mut t = task.trace.borrow_mut();
            let slot = t.siginfo.as_mut().ok_or(Errno::EINVAL)?;
            *slot = info;
            Ok(0)
        }
        GETREGSET | SETREGSET => regset(cx, &task, request == SETREGSET, addr, data),
        INTERRUPT => {
            let mut t = task.trace.borrow_mut();
            if !t.seized {
                return Err(Errno::EIO);
            }
            // A running thread enters the kernel at once, a waiting one leaves its syscall
            // (signal::interrupted), a stopped one stops after it resumes; one in PTRACE_LISTEN
            // reports again at once
            t.trap = true;
            drop(t);
            signal::wake(&task);
            Ok(0)
        }
        LISTEN => {
            let mut t = task.trace.borrow_mut();
            if !t.seized || !matches!(t.stop, Some(Stop::Event(event::STOP, _))) {
                return Err(Errno::EIO);
            }
            t.listening = true;
            drop(t);
            // If the group-stop has already ended since it stopped, report again at once
            crate::kernel::signal_later(&task.event);
            Ok(0)
        }
        GETSIGMASK | SETSIGMASK => {
            if addr != 8 {
                return Err(Errno::EINVAL);
            }
            // As on Linux: when sigsuspend or ppoll swapped the mask, the original one is read;
            // after a set the original is no longer restored (clear_tsk_restore_sigmask), so after
            // the handler returns the mask is the one the tracer set
            if request == GETSIGMASK {
                let mask = task.saved_sigmask.get().unwrap_or(task.sigmask.get());
                put(cx, data, mask.to_le_bytes()).map(|()| 0)
            } else {
                let mask = u64::from_le_bytes(get(cx, data)?);
                task.sigmask.set(mask & !signal::UNBLOCKABLE);
                task.saved_sigmask.set(None);
                Ok(0)
            }
        }
        GET_SYSCALL_INFO => syscall_info(cx, &task, addr, data),
        SECCOMP_GET_FILTER => crate::seccomp::get_filter(cx, &task, addr, data),
        SECCOMP_GET_METADATA => crate::seccomp::get_metadata(cx, &task, addr, data),
        // PEEKUSR, POKEUSR, GETREGS and the like never existed on arm64
        _ => Err(Errno::EIO),
    }
}

/// PTRACE_TRACEME: be traced by the parent. Nobody can trace it when the parent is the built-in
/// init. The credentials kept as the tracer's are the caller's own, as Linux records those of
/// whoever asks for the relationship (`ptrace_link`, since the fix for CVE-2019-13272): a parent
/// that is privileged now may later run code its child controls.
fn traceme(cx: &mut Cx<'_>) -> SysResult {
    let ppid = cx.t.group.borrow().ppid;
    if ppid == INIT || !cx.kr().groups.contains_key(&ppid) {
        return Err(Errno::EPERM);
    }
    let tracer = cx.cred();
    let mut t = cx.task.trace.borrow_mut();
    if t.tracer.is_some() {
        return Err(Errno::EPERM);
    }
    t.tracer = Some(ppid);
    t.cred = Some(tracer);
    Ok(0)
}

/// PTRACE_ATTACH and PTRACE_SEIZE (`options` in `data`). ATTACH sends the thread a SIGSTOP, SEIZE
/// leaves it undisturbed.
fn attach(cx: &mut Cx<'_>, pid: i32, seize: bool, addr: u64, data: u64) -> SysResult {
    let options = if seize {
        // As on Linux, options unknown to SEIZE are EIO, those unknown to SETOPTIONS EINVAL
        if addr != 0 || data & !(opt::MASK as u64) != 0 {
            return Err(Errno::EIO);
        }
        check_options(cx, data as u32)?;
        data as u32
    } else {
        0
    };
    let task = cx.kr().tasks.get(&pid).cloned().ok_or(Errno::ESRCH)?;
    let group = cx
        .kr()
        .groups
        .get(&task.tgid)
        .cloned()
        .ok_or(Errno::ESRCH)?;
    if task.tgid == cx.tgid() || !may_ptrace(cx, task.tid.get()) {
        return Err(Errno::EPERM);
    }
    if group.borrow().exiting.is_some() || task.killed.get() {
        return Err(Errno::EPERM);
    }
    {
        let mut t = task.trace.borrow_mut();
        if t.tracer.is_some() {
            return Err(Errno::EPERM);
        }
        // Hardware breakpoints and watchpoints an earlier tracer left armed stay (ptrace::detach)
        *t = Trace {
            tracer: Some(cx.tgid()),
            cred: Some(cx.cred()),
            seized: seize,
            options,
            hw: t.hw,
            hw_changed: t.hw_changed,
            ..Trace::default()
        };
        // The thread group is already stopped: stop and report at once as a group-stop (Linux's
        // JOBCTL_TRAP_STOP), reporting the signal that stopped it, even if the parent has
        // already taken the stop event
        let g = group.borrow();
        if g.stopped {
            t.group_stop = Some(g.stop_signal);
        }
    }
    if !seize {
        signal::send_to_task(cx.kr(), &task, Signal::STOP, SigInfo::kernel());
    }
    // A thread waiting in a group-stop must switch to waiting in a ptrace-stop
    signal::wake(&task);
    Ok(0)
}

/// `PTRACE_O_SUSPEND_SECCOMP` (for checkpoint/restore): requires `CAP_SYS_ADMIN`, and that the
/// tracer itself has no seccomp and is not traced this way (Linux's `check_ptrace_options`).
fn check_options(cx: &Cx<'_>, options: u32) -> Result<(), Errno> {
    if options & opt::SUSPEND_SECCOMP != 0 {
        let suspended = {
            let t = cx.task.trace.borrow();
            t.tracer.is_some() && t.options & opt::SUSPEND_SECCOMP != 0
        };
        if !cx.cred().capable(crate::cred::cap::SYS_ADMIN)
            || cx.task.seccomp.borrow().mode != crate::seccomp::mode::DISABLED
            || suspended
        {
            return Err(Errno::EPERM);
        }
    }
    Ok(())
}

/// PTRACE_CONT, PTRACE_SYSCALL, PTRACE_SINGLESTEP: resume a stopped thread, with `data` as the
/// signal to inject.
fn resume(task: &Task, mode: Mode, data: u64) -> SysResult {
    if data > Signal::MAX as u64 {
        return Err(Errno::EIO);
    }
    {
        let mut t = task.trace.borrow_mut();
        t.mode = mode;
        t.resume = Some(data as i32);
    }
    task.syscall_traced
        .store(mode == Mode::Syscall, Ordering::Relaxed);
    task.stepping.store(mode == Mode::Step, Ordering::Relaxed);
    if mode != Mode::Step {
        task.step_again.store(false, Ordering::Relaxed);
    }
    // It is stopped in the kernel waiting only for its own event, so no kick is needed
    crate::kernel::signal_later(&task.event);
    Ok(0)
}

/// PTRACE_PEEKTEXT, PTRACE_PEEKDATA: reads one word of the tracee and writes it to `data`.
fn peek(cx: &mut Cx<'_>, task: &Task, addr: u64, data: u64) -> SysResult {
    if !may_access_vm(cx, task) {
        return Err(Errno::EIO);
    }
    let mm = task.res.borrow().mm.clone();
    let mut word = [0u8; 8];
    mm.borrow()
        .read(UserSlice::new(addr, 8), &mut word)
        .map_err(|_| Errno::EIO)?;
    put(cx, data, word).map(|()| 0)
}

/// Whether the tracer may read and write the tracee's memory with `PTRACE_PEEK*`/`POKE*` (Linux's
/// `ptrace_access_vm`): the tracee is dumpable, or the tracer had `CAP_SYS_PTRACE` over the
/// tracee's user namespace when it attached ([`Trace::tracer_capable`]), so a process that made
/// itself non-dumpable after an unprivileged attach (ssh-agent does) keeps its memory to itself.
fn may_access_vm(cx: &Cx<'_>, task: &Task) -> bool {
    let Some(group) = cx.kr().groups.get(&task.tgid) else {
        return false;
    };
    let g = group.borrow();
    g.dumpable || task.trace.borrow().tracer_capable(&g.cred.user_ns)
}

/// PTRACE_GETREGSET, PTRACE_SETREGSET: `data` points at an iovec whose length may be shorter than
/// the regset (only the front part is read or written); the amount read or written is written
/// back as the iovec's length.
fn regset(cx: &mut Cx<'_>, task: &Task, set: bool, nt: u64, iov: u64) -> SysResult {
    let b: [u8; 16] = get(cx, iov)?;
    let base = u64::from_le_bytes(b[..8].try_into().unwrap());
    let len = u64::from_le_bytes(b[8..].try_into().unwrap());
    let (size, n) = regset_shape(nt)?;
    if !len.is_multiple_of(size as u64) {
        return Err(Errno::EINVAL);
    }
    let len = len.min((size * n) as u64) as usize;
    {
        let mut t = task.trace.borrow_mut();
        let Trace {
            regs,
            syscallno,
            hw,
            hw_changed,
            ..
        } = &mut *t;
        let regs = regs.as_mut().ok_or(Errno::ESRCH)?;
        let mut bytes = encode_regset(nt, regs, *syscallno, hw);
        if set {
            cx.mm().read(UserSlice::new(base, len), &mut bytes[..len])?;
            decode_regset(nt, &bytes, regs, syscallno, hw)?;
            *hw_changed |= nt == NT_ARM_HW_BREAK || nt == NT_ARM_HW_WATCH;
        } else {
            cx.mm().write(UserSlice::new(base, len), &bytes[..len])?;
        }
    }
    put(cx, iov + 8, (len as u64).to_le_bytes()).map(|()| 0)
}

/// A regset's (element size, element count).
fn regset_shape(nt: u64) -> Result<(usize, usize), Errno> {
    Ok(match nt {
        // user_pt_regs: x0–x30, sp, pc, pstate
        NT_PRSTATUS => (8, 34),
        // user_fpsimd_state: v0–v31, fpsr, fpcr, two reserved words
        NT_PRFPREG => (4, 132),
        NT_ARM_TLS => (8, 1),
        // user_hwdebug_state: dbg_info, a pad word, 16 sets of (address, control, pad)
        NT_ARM_HW_BREAK | NT_ARM_HW_WATCH => (4, 66),
        NT_ARM_SYSTEM_CALL => (4, 1),
        // No SVE, PAC, MTE, SME
        _ => return Err(Errno::EINVAL),
    })
}

/// A regset's contents.
pub(crate) fn encode_regset(nt: u64, r: &Regs, syscallno: i32, hw: &HwDebug) -> Vec<u8> {
    let mut b = Vec::new();
    match nt {
        NT_PRSTATUS => {
            for v in r.x.iter().chain(&[r.sp, r.pc, r.pstate]) {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        NT_PRFPREG => {
            for v in &r.v {
                b.extend_from_slice(&v[0].to_le_bytes());
                b.extend_from_slice(&v[1].to_le_bytes());
            }
            b.extend_from_slice(&(r.fpsr as u32).to_le_bytes());
            b.extend_from_slice(&(r.fpcr as u32).to_le_bytes());
            b.extend_from_slice(&[0; 8]);
        }
        NT_ARM_TLS => b.extend_from_slice(&r.tpidr.to_le_bytes()),
        NT_ARM_HW_BREAK | NT_ARM_HW_WATCH => {
            // dbg_info: the debug architecture and slot count, then 16 sets of (address, control
            // word, pad)
            let (slots, n) = if nt == NT_ARM_HW_BREAK {
                (&hw.brk, BRK_SLOTS)
            } else {
                (&hw.watch, WATCH_SLOTS)
            };
            b.extend_from_slice(&(DEBUG_ARCH_V8 << 8 | n as u32).to_le_bytes());
            b.extend_from_slice(&[0; 4]);
            for &(addr, ctrl) in slots {
                b.extend_from_slice(&addr.to_le_bytes());
                b.extend_from_slice(&ctrl.to_le_bytes());
                b.extend_from_slice(&[0; 4]);
            }
        }
        _ => b.extend_from_slice(&syscallno.to_le_bytes()),
    }
    b
}

/// Regset contents written by the tracer.
fn decode_regset(
    nt: u64,
    b: &[u8],
    r: &mut Regs,
    syscallno: &mut i32,
    hw: &mut HwDebug,
) -> Result<(), Errno> {
    let u64_at = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    let u32_at = |off: usize| u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
    match nt {
        NT_PRSTATUS => {
            for (i, x) in r.x.iter_mut().enumerate() {
                *x = u64_at(i);
            }
            r.sp = u64_at(31);
            r.pc = u64_at(32);
            r.pstate = crate::ptrace::user_pstate(u64_at(33));
        }
        NT_PRFPREG => {
            for (i, v) in r.v.iter_mut().enumerate() {
                *v = [u64_at(2 * i), u64_at(2 * i + 1)];
            }
            r.fpsr = u64::from(u32_at(512));
            r.fpcr = u64::from(u32_at(516));
        }
        NT_ARM_TLS => r.tpidr = u64_at(0),
        NT_ARM_HW_BREAK | NT_ARM_HW_WATCH => {
            let watch = nt == NT_ARM_HW_WATCH;
            let (mut slots, n) = if watch {
                (hw.watch, WATCH_SLOTS)
            } else {
                (hw.brk, BRK_SLOTS)
            };
            for (i, slot) in slots.iter_mut().enumerate() {
                let addr = u64_at(1 + 2 * i);
                let ctrl = u32_at(16 + 16 * i);
                // Beyond the slot count only zeros are allowed (Linux cannot create the
                // breakpoint, ENOSPC)
                *slot = if i >= n {
                    if (addr, ctrl) != (0, 0) {
                        return Err(Errno::ENOSPC);
                    }
                    (0, 0)
                } else {
                    hw_slot(watch, addr, ctrl)?
                };
            }
            if watch {
                hw.watch = slots;
            } else {
                hw.brk = slots;
            }
        }
        _ => *syscallno = u32_at(0) as i32,
    }
    Ok(())
}

/// The address and control word of one hardware breakpoint or watchpoint (`watch`), checked and
/// normalized as Linux does (`ptrace_hbp_fill_attr_ctrl`, `hw_breakpoint_arch_parse`): disabled
/// ones are recorded as they are; for enabled ones the byte select (BAS) must be one contiguous
/// run, a watchpoint's kind is read, write or both, and a breakpoint can only be 4 bytes; the start
/// is the address plus the BAS offset, aligned to 8 bytes (4 for breakpoints) with the offset moved
/// back into BAS, and the privilege level is recorded as EL0.
fn hw_slot(watch: bool, addr: u64, ctrl: u32) -> Result<(u64, u32), Errno> {
    const EL0: u32 = 2 << 1;
    if ctrl & 1 == 0 {
        return Ok((addr, ctrl));
    }
    let ty = ctrl >> 3 & 3;
    let bas = ctrl >> 5 & 0xff;
    let shift = bas.trailing_zeros();
    let run = bas.checked_shr(shift).unwrap_or(0);
    if bas == 0 || run & (run + 1) != 0 {
        return Err(Errno::EINVAL);
    }
    let len = run.count_ones() as u64;
    let start = addr.wrapping_add(u64::from(shift));
    if watch {
        let offset = start & 7;
        if ty == 0 || offset + len > 8 {
            return Err(Errno::EINVAL);
        }
        let bas = ((1u32 << len) - 1) << offset;
        Ok((start & !7, bas << 5 | ty << 3 | EL0 | 1))
    } else {
        if ty != 0 || len != 4 {
            return Err(Errno::EINVAL);
        }
        Ok((start & !3, 0xf << 5 | EL0 | 1))
    }
}

/// PTRACE_GET_SYSCALL_INFO: `size` is the buffer length. Syscall entry and exit (which need
/// `PTRACE_O_TRACESYSGOOD`) give the number and arguments, or the return value; other stops only
/// the common part at the front. Returns the full length.
fn syscall_info(cx: &mut Cx<'_>, task: &Task, size: u64, data: u64) -> SysResult {
    const OP_ENTRY: u8 = 1;
    const OP_EXIT: u8 = 2;
    const OP_SECCOMP: u8 = 3;
    let mut b = [0u8; 88];
    let actual = {
        let t = task.trace.borrow();
        let r = t.regs.as_ref().ok_or(Errno::ESRCH)?;
        b[4..8].copy_from_slice(&AUDIT_ARCH_AARCH64.to_le_bytes());
        b[8..16].copy_from_slice(&r.pc.to_le_bytes());
        b[16..24].copy_from_slice(&r.sp.to_le_bytes());
        let si_code = t
            .siginfo
            .map_or(0, |s| i32::from_le_bytes(s[8..12].try_into().unwrap()));
        let syscall = si_code == Signal::TRAP.number() | 0x80;
        let seccomp = si_code == Signal::TRAP.number() | (event::SECCOMP as i32) << 8;
        let entry = |b: &mut [u8; 88]| {
            b[24..32].copy_from_slice(&(t.syscallno as i64).to_le_bytes());
            let args = [r.orig_x0, r.x[1], r.x[2], r.x[3], r.x[4], r.x[5]];
            for (i, a) in args.iter().enumerate() {
                b[32 + i * 8..40 + i * 8].copy_from_slice(&a.to_le_bytes());
            }
        };
        match t.message {
            // seccomp's TRACE: the number and arguments, plus the data from the filter
            data if seccomp => {
                b[0] = OP_SECCOMP;
                entry(&mut b);
                b[80..84].copy_from_slice(&(data as u32).to_le_bytes());
                84
            }
            EVENTMSG_SYSCALL_ENTRY if syscall => {
                b[0] = OP_ENTRY;
                entry(&mut b);
                80
            }
            EVENTMSG_SYSCALL_EXIT if syscall => {
                b[0] = OP_EXIT;
                let rval = r.x[0] as i64;
                b[24..32].copy_from_slice(&rval.to_le_bytes());
                b[32] = (-4095..0).contains(&rval) as u8;
                33
            }
            _ => 24,
        }
    };
    let n = size.min(actual) as usize;
    cx.mm().write(UserSlice::new(data, n), &b[..n])?;
    Ok(actual)
}

/// process_vm_readv (process_vm_writev with `write`): copies between our own `local` and the
/// `remote` of process `pid`, with the same permissions as PTRACE_ATTACH. The remote side is
/// copied page by page, stopping at the first inaccessible page and returning how much was copied
/// (EFAULT if not a single byte was).
pub(super) fn process_vm(
    cx: &mut Cx<'_>,
    pid: i32,
    (local, local_count): (u64, u64),
    (remote, remote_count): (u64, u64),
    flags: u64,
    write: bool,
) -> SysResult {
    const IOV_MAX: u64 = 1024;
    if flags != 0 || local_count > IOV_MAX || remote_count > IOV_MAX {
        return Err(Errno::EINVAL);
    }
    let local = iovecs(cx, local, local_count)?;
    if local.iter().all(|&(_, len)| len == 0) {
        return Ok(0);
    }
    let remote = iovecs(cx, remote, remote_count)?;
    let task = cx.kr().tasks.get(&pid).cloned().ok_or(Errno::ESRCH)?;
    if !may_ptrace(cx, task.tid.get()) {
        return Err(Errno::EPERM);
    }
    let target: Rc<_> = task.res.borrow().mm.clone();
    let mut local = local.into_iter().filter(|&(_, len)| len > 0);
    let mut cur = (0, 0);
    let mut total = 0u64;
    let mut faulted = false;
    let mut buf = Vec::new();
    'remote: for (base, len) in remote {
        let mut done = 0;
        while done < len {
            if cur.1 == 0 {
                match local.next() {
                    Some(next) => cur = next,
                    None => break 'remote,
                }
            }
            let addr = base + done;
            let n = (len - done).min(cur.1).min(PAGE_SIZE - addr % PAGE_SIZE) as usize;
            buf.resize(n, 0);
            let ok = if write {
                cx.mm().read(UserSlice::new(cur.0, n), &mut buf).is_ok()
                    && target.borrow().write(UserSlice::new(addr, n), &buf).is_ok()
            } else {
                target
                    .borrow()
                    .read(UserSlice::new(addr, n), &mut buf)
                    .is_ok()
                    && cx.mm().write(UserSlice::new(cur.0, n), &buf).is_ok()
            };
            if !ok {
                faulted = true;
                break 'remote;
            }
            done += n as u64;
            total += n as u64;
            cur = (cur.0 + n as u64, cur.1 - n as u64);
        }
    }
    if faulted && total == 0 {
        return Err(Errno::EFAULT);
    }
    Ok(total)
}

/// Reads `count` iovecs.
fn iovecs(cx: &Cx<'_>, addr: u64, count: u64) -> Result<Vec<(u64, u64)>, Errno> {
    let mut b = vec![0u8; count as usize * 16];
    cx.mm().read(UserSlice::new(addr, b.len()), &mut b)?;
    let mut total = 0u64;
    let mut out = Vec::with_capacity(count as usize);
    for c in b.chunks_exact(16) {
        let base = u64::from_le_bytes(c[..8].try_into().unwrap());
        let len = u64::from_le_bytes(c[8..].try_into().unwrap());
        total = total
            .checked_add(len)
            .filter(|&t| t <= i64::MAX as u64)
            .ok_or(Errno::EINVAL)?;
        out.push((base, len));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hw_slots_are_checked_and_normalized_like_linux() {
        const EL0: u32 = 2 << 1;
        let watch = |bas: u32, ty: u32| bas << 5 | ty << 3 | EL0 | 1;
        // Disabled ones are recorded as they are, unchecked
        assert_eq!(hw_slot(true, 0x1234, 0x40), Ok((0x1234, 0x40)));
        // An offset in the address and an offset in BAS are the same thing: both are normalized
        // to an 8-aligned address
        assert_eq!(
            hw_slot(true, 0x1000, watch(0xf0, 2)),
            Ok((0x1000, watch(0xf0, 2)))
        );
        assert_eq!(
            hw_slot(true, 0x1004, watch(0x0f, 2)),
            Ok((0x1000, watch(0xf0, 2)))
        );
        assert_eq!(
            hw_slot(true, 0x1003, watch(0x01, 1)),
            Ok((0x1000, watch(0x08, 1)))
        );
        // Crossing 8 bytes, BAS not contiguous or zero, kind is execute: EINVAL
        assert_eq!(hw_slot(true, 0x1006, watch(0x0f, 2)), Err(Errno::EINVAL));
        assert_eq!(hw_slot(true, 0x1000, watch(0x05, 2)), Err(Errno::EINVAL));
        assert_eq!(hw_slot(true, 0x1000, watch(0, 2)), Err(Errno::EINVAL));
        assert_eq!(hw_slot(true, 0x1000, watch(0xff, 0)), Err(Errno::EINVAL));
        // Breakpoints can only be execute, 4 bytes, with the address aligned to 4
        assert_eq!(
            hw_slot(false, 0x2000, watch(0x0f, 0)),
            Ok((0x2000, watch(0x0f, 0)))
        );
        assert_eq!(
            hw_slot(false, 0x2002, watch(0x0f, 0)),
            Ok((0x2000, watch(0x0f, 0)))
        );
        assert_eq!(hw_slot(false, 0x2000, watch(0x03, 0)), Err(Errno::EINVAL));
        assert_eq!(hw_slot(false, 0x2000, watch(0x0f, 1)), Err(Errno::EINVAL));
    }
}
