//! Signal syscalls: the kill family, sigaction, signal masks, sigsuspend, sigaltstack.

use std::rc::{Rc, Weak};
use std::time::Instant;

use super::{SysResult, get, put, read_timespec};
use crate::cell::RefCell;
use crate::cred::Cred;
use crate::errno::Errno;
use crate::fd::FileKind;
use crate::kernel::{Cx, INIT, Tid, wake};
use crate::process::Signal;
use crate::pseudo::PseudoNode;
use crate::signal::{self, SigAction, SigInfo, UNBLOCKABLE, code};
use crate::vfs::Node;

const SIG_BLOCK: u32 = 0;
const SIG_UNBLOCK: u32 = 1;
const SIG_SETMASK: u32 = 2;
/// Size of the guest's sigset_t in the kernel ABI (64 signals).
const SIGSET_SIZE: usize = 8;

pub(super) fn kill(cx: &mut Cx<'_>, pid: i32, sig: i32) -> SysResult {
    let sig = signal_arg(sig)?;
    let me = cx.tgid();
    let (my_pgid, my_sid) = {
        let g = cx.t.group.borrow();
        (g.pgid, g.sid)
    };
    let cred = cx.cred();
    // The built-in init ignores other signals; without permission to signal it, the result is
    // EPERM as on Linux. When the host started it from inittab, SIGTERM, SIGUSR1 and SIGUSR2 are
    // requests to reboot, halt and power off, as with busybox's init (busybox's reboot, halt,
    // poweroff): record the request and wake the waiting host
    if pid == INIT {
        if !cred.may_signal(&crate::cred::Cred::root()) {
            return Err(Errno::EPERM);
        }
        let k = cx.k();
        if let Some(power) = sig.and_then(crate::inittab::Power::from_signal)
            && k.init_handlers
        {
            k.power.get_or_insert(power);
            crate::kernel::request_wake_on(crate::kernel::wake::WATCHED);
        }
        return Ok(0);
    }
    // Live target thread groups: pid > 0 is one process (a thread's id names its process, as in
    // Linux's kill_pid_info), 0 is this process group, -1 is every process except init and
    // ourselves (those visible in our pid namespace with ids above 1), < -1 is process group -pid
    let process = {
        let k = cx.kr();
        match k.tasks.get(&pid) {
            Some(t) if !k.groups.contains_key(&pid) => t.tgid,
            _ => pid,
        }
    };
    let ns = cx.pid_ns();
    let targets: Vec<Tid> = cx
        .kr()
        .groups
        .values()
        .filter_map(|g| {
            let g = g.borrow();
            let hit = match pid {
                p if p > 0 => g.tgid == process,
                0 => g.pgid == my_pgid,
                -1 => g.tgid != me && ns.vnr(g.tgid).is_some_and(|v| v > 1),
                p => g.pgid == -p,
            };
            (hit && g.tgid != INIT).then_some(g.tgid)
        })
        .collect();
    if targets.is_empty() {
        return Err(Errno::ESRCH);
    }
    // Targets without permission are skipped; if none can be signalled, EPERM (except for pid -1)
    let allowed: Vec<Tid> = targets
        .into_iter()
        .filter(|&t| may_kill(cx, &cred, my_sid, t, sig))
        .collect();
    if allowed.is_empty() && pid != -1 {
        return Err(Errno::EPERM);
    }
    if let Some(sig) = sig {
        for tgid in allowed {
            let info = SigInfo::kill(code::SI_USER, cx.pid_out(me), cred.uid_out(cred.ruid))
                .sent_by(&cred)
                .from(me);
            signal::send_to_group(cx.kr(), tgid, sig, info);
        }
    }
    Ok(0)
}

pub(super) fn tkill(cx: &mut Cx<'_>, tid: i32, sig: i32) -> SysResult {
    kill_thread(cx, None, tid, sig)
}

pub(super) fn tgkill(cx: &mut Cx<'_>, tgid: i32, tid: i32, sig: i32) -> SysResult {
    if tgid <= 0 {
        return Err(Errno::EINVAL);
    }
    kill_thread(cx, Some(tgid), tid, sig)
}

/// Sends a signal to one thread; with `tgid` None, does not check which thread group it belongs
/// to (tkill).
fn kill_thread(cx: &mut Cx<'_>, tgid: Option<i32>, tid: i32, sig: i32) -> SysResult {
    if tid <= 0 {
        return Err(Errno::EINVAL);
    }
    let sig = signal_arg(sig)?;
    let task = cx.kr().tasks.get(&tid).cloned().ok_or(Errno::ESRCH)?;
    if tgid.is_some_and(|g| task.tgid != g) {
        return Err(Errno::ESRCH);
    }
    let cred = cx.cred();
    let sid = cx.t.group.borrow().sid;
    if !may_kill(cx, &cred, sid, tid, sig) {
        return Err(Errno::EPERM);
    }
    if let Some(sig) = sig {
        // tkill's si_code is not SI_USER: when a real-time signal cannot be queued, EAGAIN as
        // with sigqueue
        if !signal::can_queue(cx.kr(), task.tgid, sig) {
            return Err(Errno::EAGAIN);
        }
        let me = cx.tgid();
        let info = SigInfo::kill(code::SI_TKILL, cx.pid_out(me), cred.uid_out(cred.ruid))
            .sent_by(&cred)
            .from(me);
        signal::send_to_task(cx.kr(), &task, sig, info);
    }
    Ok(0)
}

/// pidfd_send_signal: the pidfd may also be an open `/proc/<pid>` directory. With `uinfo` null it
/// is the same as kill, otherwise the same as rt_sigqueueinfo (si_signo must equal `sig`).
pub(super) fn pidfd_send_signal(
    cx: &mut Cx<'_>,
    pidfd: i32,
    sig: i32,
    uinfo: u64,
    flags: u32,
) -> SysResult {
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let file = cx.fds().get(pidfd)?;
    let tgid = match &file.borrow().kind {
        FileKind::Pidfd(p) => p.group(cx.kr()).map(|_| p.tgid).ok_or(Errno::ESRCH)?,
        // The process that was opened has been reaped: the pid may already belong to a new one
        FileKind::Directory {
            node: Node::Pseudo(_, PseudoNode::Pid(pid, None)),
            process,
            ..
        } => {
            let same = |g: &Rc<RefCell<crate::process::ThreadGroup>>| {
                process
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .is_some_and(|p| Rc::ptr_eq(&p, g))
            };
            // /proc/<tid> of any thread names its process
            match crate::pseudo::group_of(cx.kr(), *pid) {
                Some(g) if same(g) => g.borrow().tgid,
                _ => return Err(Errno::ESRCH),
            }
        }
        _ => return Err(Errno::EBADF),
    };
    let sig = signal_arg(sig)?;
    let me = cx.tgid();
    let cred = cx.cred();
    let info = if uinfo != 0 {
        let b: [u8; 128] = get(cx, uinfo)?;
        if i32::from_le_bytes(b[..4].try_into().unwrap()) != sig.map_or(0, Signal::number) {
            return Err(Errno::EINVAL);
        }
        let info = queue_info(&b).sent_by(&cred);
        if (info.code >= 0 || info.code == code::SI_TKILL) && tgid != me {
            return Err(Errno::EPERM);
        }
        info
    } else {
        SigInfo::kill(code::SI_USER, cx.pid_out(me), cred.uid_out(cred.ruid)).sent_by(&cred)
    }
    .from(me);
    if !cx.kr().groups.contains_key(&tgid) {
        return Err(Errno::ESRCH);
    }
    let sid = cx.t.group.borrow().sid;
    if !may_kill(cx, &cred, sid, tgid, sig) {
        return Err(Errno::EPERM);
    }
    let Some(sig) = sig else {
        return Ok(0);
    };
    if uinfo != 0 && info.code != code::SI_USER && !signal::can_queue(cx.kr(), tgid, sig) {
        return Err(Errno::EAGAIN);
    }
    // The built-in init ignores all signals
    if tgid != INIT {
        signal::send_to_group(cx.kr(), tgid, sig, info);
    }
    Ok(0)
}

/// rt_sigqueueinfo and rt_tgsigqueueinfo (with `tid` None, sent to the whole process): the
/// siginfo is given by the caller. It may not impersonate signals sent by the kernel or by kill
/// (si_code not below 0, or SI_TKILL), unless sent to oneself.
pub(super) fn sigqueueinfo(
    cx: &mut Cx<'_>,
    tgid: i32,
    tid: Option<i32>,
    sig: i32,
    uinfo: u64,
) -> SysResult {
    let b: [u8; 128] = get(cx, uinfo)?;
    let info = queue_info(&b).sent_by(&cx.cred()).from(cx.tgid());
    let code = info.code;
    let target = tid.unwrap_or(tgid);
    if (code >= 0 || code == code::SI_TKILL) && target != cx.tid() && tgid != cx.tgid() {
        return Err(Errno::EPERM);
    }
    let sig = signal_arg(sig)?;
    let cred = cx.cred();
    let sid = cx.t.group.borrow().sid;
    // rt_sigqueueinfo's pid may also be the tid of any thread in the process
    let tgid = match tid {
        None => cx.kr().tasks.get(&tgid).map_or(tgid, |t| t.tgid),
        Some(_) => tgid,
    };
    // A real-time signal that cannot be queued returns EAGAIN, rather than dropping the siginfo
    // as kill does
    let full = |cx: &Cx<'_>| {
        sig.is_some_and(|s| code != code::SI_USER && !signal::can_queue(cx.kr(), tgid, s))
    };
    match tid {
        Some(tid) => {
            if tgid <= 0 || tid <= 0 {
                return Err(Errno::EINVAL);
            }
            let task = cx.kr().tasks.get(&tid).cloned().ok_or(Errno::ESRCH)?;
            if task.tgid != tgid {
                return Err(Errno::ESRCH);
            }
            if !may_kill(cx, &cred, sid, tid, sig) {
                return Err(Errno::EPERM);
            }
            if full(cx) {
                return Err(Errno::EAGAIN);
            }
            if let Some(sig) = sig {
                signal::send_to_task(cx.kr(), &task, sig, info);
            }
        }
        None => {
            // A zombie that has not been reaped is still there, as for kill (send_to_group then
            // does nothing)
            if tgid == INIT || !cx.kr().groups.contains_key(&tgid) {
                return Err(Errno::ESRCH);
            }
            if !may_kill(cx, &cred, sid, tgid, sig) {
                return Err(Errno::EPERM);
            }
            if full(cx) {
                return Err(Errno::EAGAIN);
            }
            if let Some(sig) = sig {
                signal::send_to_group(cx.kr(), tgid, sig, info);
            }
        }
    }
    Ok(0)
}

/// A signal number argument: 0 only checks that the target exists.
fn signal_arg(sig: i32) -> Result<Option<Signal>, Errno> {
    if sig == 0 {
        return Ok(None);
    }
    Signal::new(sig).map(Some).ok_or(Errno::EINVAL)
}

/// The siginfo given by the caller (rt_sigqueueinfo, pidfd_send_signal): delivered as given, as
/// far as Linux's `kernel_siginfo` reaches (48 bytes; si_signo is the signal's).
fn queue_info(b: &[u8; 128]) -> SigInfo {
    let word = |off: usize| i32::from_le_bytes(b[off..off + 4].try_into().unwrap());
    SigInfo {
        code: word(8),
        fields: signal::InfoFields::User {
            errno: word(4),
            pid: word(16),
            uid: word(20) as u32,
            rest: b[24..48].try_into().unwrap(),
        },
        ..SigInfo::default()
    }
}

/// Whether thread `target` (a thread ID, or a pid when signalling the process, which goes by its
/// main thread's credentials) may be sent signal `sig` (`check_kill_permission`): its credentials
/// allow it, or it is in the same process, or it is SIGCONT sent within the same session.
fn may_kill(cx: &Cx<'_>, cred: &Cred, sid: Tid, target: Tid, sig: Option<Signal>) -> bool {
    let k = cx.kr();
    let Some(group) = crate::pseudo::group_of(k, target) else {
        return false;
    };
    let g = group.borrow();
    g.tgid == cx.tgid()
        || k.cred_of(target).is_some_and(|t| cred.may_signal(&t))
        || (sig == Some(Signal::CONT) && g.sid == sid)
}

pub(super) fn rt_sigaction(
    cx: &mut Cx<'_>,
    sig: i32,
    act: u64,
    oact: u64,
    size: usize,
) -> SysResult {
    if size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let sig = Signal::new(sig).ok_or(Errno::EINVAL)?;
    if act != 0 && (sig == Signal::KILL || sig == Signal::STOP) {
        return Err(Errno::EINVAL);
    }
    let new = if act != 0 {
        let b: [u8; 32] = get(cx, act)?;
        let word = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        // Unknown flag bits are cleared (Linux 5.11+'s UAPI_SA_FLAGS), so programs probing with
        // SA_UNSUPPORTED see which flags this kernel knows: SA_NOCLDSTOP, SA_NOCLDWAIT,
        // SA_SIGINFO, SA_EXPOSE_TAGBITS, SA_RESTORER, SA_ONSTACK, SA_RESTART, SA_NODEFER and
        // SA_RESETHAND
        const UAPI_SA_FLAGS: u64 = 0x1
            | 0x2
            | 0x4
            | 0x800
            | 0x0400_0000
            | 0x0800_0000
            | 0x1000_0000
            | 0x4000_0000
            | 0x8000_0000;
        Some(SigAction {
            handler: word(0),
            flags: word(1) & UAPI_SA_FLAGS,
            restorer: word(2),
            mask: word(3) & !UNBLOCKABLE,
            immutable: false,
        })
    } else {
        None
    };
    let sighand = cx.t.group.borrow().sighand.clone();
    // The default action forced when seccomp kills the process cannot be changed again (Linux's
    // SA_IMMUTABLE)
    if sighand.borrow().get(sig).immutable {
        return Err(Errno::EINVAL);
    }
    if oact != 0 {
        let old = sighand.borrow().get(sig);
        let mut b = [0u8; 32];
        for (i, w) in [old.handler, old.flags, old.restorer, old.mask]
            .into_iter()
            .enumerate()
        {
            b[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        put(cx, oact, b)?;
    }
    if let Some(new) = new {
        sighand.borrow_mut().set(sig, new);
        // Once changed to ignored, pending instances of the same signal are discarded (on the
        // thread group and on every thread)
        if sighand.borrow().ignores(sig) {
            let threads = {
                let mut g = cx.t.group.borrow_mut();
                g.pending.discard(sig.bit());
                g.threads.clone()
            };
            for tid in threads {
                if let Some(t) = cx.kr().tasks.get(&tid) {
                    t.pending.borrow_mut().discard(sig.bit());
                }
            }
        }
    }
    Ok(0)
}

pub(super) fn rt_sigprocmask(
    cx: &mut Cx<'_>,
    how: u32,
    set: u64,
    oset: u64,
    size: usize,
) -> SysResult {
    if size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let cur = cx.task.sigmask.get();
    let new = if set != 0 {
        let s = u64::from_le_bytes(get(cx, set)?);
        Some(match how {
            SIG_BLOCK => cur | s,
            SIG_UNBLOCK => cur & !s,
            SIG_SETMASK => s,
            _ => return Err(Errno::EINVAL),
        })
    } else {
        None
    };
    if oset != 0 {
        put(cx, oset, cur.to_le_bytes())?;
    }
    if let Some(mask) = new {
        cx.task.sigmask.set(mask & !UNBLOCKABLE);
    }
    Ok(0)
}

pub(super) fn rt_sigpending(cx: &mut Cx<'_>, set: u64, size: usize) -> SysResult {
    if size > SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let pending = cx.task.pending.borrow().set() | cx.t.group.borrow().pending.set();
    let pending = pending & cx.task.sigmask.get();
    cx.mm().write(
        crate::uaccess::UserSlice::new(set, size),
        &pending.to_le_bytes()[..size],
    )?;
    Ok(0)
}

pub(super) fn rt_sigsuspend(cx: &mut Cx<'_>, set: u64, size: usize) -> SysResult {
    if size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let mask = u64::from_le_bytes(get(cx, set)?);
    // On delivery the frame saves the original mask, restored when the handler returns
    // (signal::deliver)
    cx.task.saved_sigmask.set(Some(cx.task.sigmask.get()));
    cx.task.sigmask.set(mask & !UNBLOCKABLE);
    while !cx.interrupted() {
        if let Err(e) = cx.wait(None, &mut []) {
            if let Some(saved) = cx.task.saved_sigmask.take() {
                cx.task.sigmask.set(saved);
            }
            return Err(e);
        }
    }
    Err(Errno::ERESTARTNOHAND)
}

/// rt_sigtimedwait (sigwait, sigwaitinfo, sigtimedwait): waits for a signal in `set` to become
/// pending and takes it directly, without a handler. Signals in `set` are usually blocked by the
/// caller; when another signal interrupts the wait it returns EINTR, and as on Linux this syscall
/// is never restarted.
pub(super) fn rt_sigtimedwait(
    cx: &mut Cx<'_>,
    set: u64,
    info: u64,
    timeout: u64,
    size: usize,
) -> SysResult {
    if size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let want = u64::from_le_bytes(get(cx, set)?) & !UNBLOCKABLE;
    let deadline = match timeout {
        0 => None,
        t => Some(Instant::now() + read_timespec(cx, t)?),
    };
    loop {
        if let Some((sig, si)) = signal::take(cx, want) {
            if info != 0 {
                put(cx, info, si.encode(sig))?;
            }
            return Ok(sig.number() as u64);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::EINTR);
        }
        // A blocked signal becoming pending requests wake::SIGNAL; an unblocked one wakes this
        // thread directly
        cx.wait_on(deadline, &mut [], wake::SIGNAL)?;
    }
}

/// signalfd4: creates a new one when `fd` is -1, otherwise changes the mask of an existing
/// signalfd.
pub(super) fn signalfd4(cx: &mut Cx<'_>, fd: i32, mask: u64, size: usize, flags: u32) -> SysResult {
    use crate::fd::{FileKind, OpenFile, SignalFd, oflags};
    if size != SIGSET_SIZE || flags & !(oflags::O_CLOEXEC | oflags::O_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    let set = u64::from_le_bytes(get(cx, mask)?) & !UNBLOCKABLE;
    if fd != -1 {
        let file = cx.fds().get(fd)?;
        let FileKind::SignalFd(s) = &file.borrow().kind else {
            return Err(Errno::EINVAL);
        };
        s.borrow_mut().mask = set;
        // Threads waiting on this signalfd recheck with the new mask (poll registered it)
        crate::kernel::request_wake_object(crate::kernel::wake::of(s));
        return Ok(fd as u64);
    }
    let s = SignalFd {
        mask: set,
        group: std::rc::Rc::downgrade(&cx.t.group),
    };
    // Read-write, as Linux's anon_inode_getfd gives it (write is EINVAL, F_GETFL shows O_RDWR)
    let file = OpenFile::new(
        FileKind::SignalFd(std::rc::Rc::new(crate::cell::RefCell::new(s))),
        oflags::O_RDWR | (flags & oflags::O_NONBLOCK),
    );
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

/// Reads a signalfd: takes the caller's pending signals that are in the mask, each encoded as a
/// 128-byte `struct signalfd_siginfo`. Blocks when there are none (EAGAIN when nonblocking).
pub(super) fn signalfd_read(
    cx: &mut Cx<'_>,
    s: &std::rc::Rc<crate::cell::RefCell<crate::fd::SignalFd>>,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    if buf.len() < 128 {
        return Err(Errno::EINVAL);
    }
    let mut n = 0;
    loop {
        let mask = s.borrow().mask;
        while n + 128 <= buf.len() {
            let Some((sig, info)) = signal::take(cx, mask) else {
                break;
            };
            buf[n..n + 128].copy_from_slice(&encode_signalfd_siginfo(sig, &info));
            n += 128;
        }
        if n > 0 {
            return Ok(n);
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        // A blocked signal becoming pending requests wake::SIGNAL; changing the mask requests
        // this signalfd
        cx.wait_objects(None, &mut [], wake::FILE | wake::SIGNAL, &[wake::of(s)])?;
    }
}

/// `struct signalfd_siginfo` (128 bytes).
fn encode_signalfd_siginfo(sig: Signal, info: &SigInfo) -> [u8; 128] {
    use signal::InfoFields;
    let mut b = [0u8; 128];
    let mut put32 = |off: usize, v: u32| b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    put32(0, sig.number() as u32);
    put32(8, info.code as u32);
    let mut value = None;
    match info.fields {
        InfoFields::None => {}
        InfoFields::Kill { pid, uid } => {
            put32(12, pid as u32);
            put32(16, uid);
        }
        InfoFields::Child { pid, uid, status } => {
            put32(12, pid as u32);
            put32(16, uid);
            put32(40, status as u32);
        }
        InfoFields::Queue { pid, uid, value: v } => {
            put32(12, pid as u32);
            put32(16, uid);
            value = Some(v);
        }
        InfoFields::Timer {
            id,
            overrun,
            value: v,
        } => {
            put32(24, id as u32);
            put32(32, overrun);
            value = Some(v);
        }
        InfoFields::Fault { addr } => b[72..80].copy_from_slice(&addr.to_le_bytes()),
        InfoFields::Poll { band, fd } => {
            put32(20, fd as u32);
            put32(28, band as u32);
        }
        InfoFields::Sys {
            errno,
            call_addr,
            syscall,
            arch,
        } => {
            put32(4, errno as u32);
            put32(84, syscall as u32);
            put32(96, arch);
            b[88..96].copy_from_slice(&call_addr.to_le_bytes());
        }
        InfoFields::User {
            errno,
            pid,
            uid,
            rest,
        } => {
            put32(4, errno as u32);
            put32(12, pid as u32);
            put32(16, uid);
            value = Some(u64::from_le_bytes(rest[..8].try_into().unwrap()));
        }
        // Given by a tracer: most si_codes start with the sender's pid and uid
        InfoFields::Raw(rest) => {
            put32(12, u32::from_le_bytes(rest[..4].try_into().unwrap()));
            put32(16, u32::from_le_bytes(rest[4..8].try_into().unwrap()));
        }
    }
    if let Some(v) = value {
        b[44..48].copy_from_slice(&(v as u32).to_le_bytes());
        b[48..56].copy_from_slice(&v.to_le_bytes());
    }
    b
}

pub(super) fn sigaltstack(cx: &mut Cx<'_>, sp: u64, uss: u64, uoss: u64) -> SysResult {
    let old = cx.t.altstack;
    if uss != 0 {
        let b: [u8; 24] = get(cx, uss)?;
        let ss_sp = u64::from_le_bytes(b[..8].try_into().unwrap());
        let flags = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let size = u64::from_le_bytes(b[16..].try_into().unwrap());
        let mut alt = old;
        alt.change(sp, ss_sp, flags, size)?;
        cx.t.altstack = alt;
    }
    if uoss != 0 {
        let mut b = [0u8; 24];
        b[..8].copy_from_slice(&old.sp.to_le_bytes());
        b[8..12].copy_from_slice(&old.reported_flags(sp).to_le_bytes());
        b[16..].copy_from_slice(&old.size.to_le_bytes());
        put(cx, uoss, b)?;
    }
    Ok(0)
}
