//! Scheduling syscalls: nice values (getpriority, setpriority) and `sched_*`.
//!
//! Guest threads are ordinary host threads scheduled by the host. As on Linux the nice value
//! belongs to the thread; it is applied to the host thread together with the CPU affinity
//! ([`apply_host`]), and the host allocates CPUs accordingly; `SCHED_IDLE` becomes the host's
//! lowest priority. Guest CPU `i` is the `i`th CPU the host allows this process to use
//! ([`tuxport_hal::host_cpus`]). Real-time policies and real-time priorities are only recorded
//! (visible in procfs and through the matching get calls). Permissions are checked by Linux's
//! rules: changing another's settings needs the same effective user ID, raising priority needs
//! `RLIMIT_NICE` to allow it or `CAP_SYS_NICE`, and real-time policies need `CAP_SYS_NICE`.
//! `SCHED_DEADLINE` parameters are checked by Linux's rules and recorded, with no bandwidth
//! admission control.

use std::cell::Cell;
use std::rc::Rc;

use super::{SysResult, get, put, write_pair};
use crate::cell::RefCell;
use crate::cred::{Cred, cap};
use crate::errno::Errno;
use crate::kernel::{Cx, INIT};
use crate::process::{RLIMIT_NICE, ThreadGroup};
use crate::task::Task;

const PRIO_PROCESS: i32 = 0;
const PRIO_PGRP: i32 = 1;
const PRIO_USER: i32 = 2;

const SCHED_OTHER: i32 = 0;
const SCHED_FIFO: i32 = 1;
const SCHED_RR: i32 = 2;
const SCHED_BATCH: i32 = 3;
const SCHED_IDLE: i32 = 5;
const SCHED_DEADLINE: i32 = 6;
pub(super) const SCHED_RESET_ON_FORK: i32 = 0x4000_0000;
const SCHED_FLAG_RESET_ON_FORK: u64 = 1;
/// Keep the current policy, or the current parameters (nice, priority, deadline), and set only the
/// rest (uclampset uses both).
const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
/// Set the minimum or maximum utilization clamp (Linux built with uclamp, as Android and
/// distribution kernels are): only recorded.
const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;
const SCHED_FLAG_UTIL_CLAMP_MAX: u64 = 0x40;
/// Every flag of Linux 6.1 (`SCHED_FLAG_ALL`), including the deadline ones `SCHED_FLAG_RECLAIM` and
/// `SCHED_FLAG_DL_OVERRUN`, accepted and not used.
const SCHED_FLAG_ALL: u64 = 0x7f;
/// The utilization scale (`SCHED_CAPACITY_SCALE`).
const UCLAMP_MAX: u32 = 1024;

/// Sizes of `struct sched_attr`: the first version, and the version supported here (with
/// util_min/util_max).
const SCHED_ATTR_SIZE_VER0: usize = 48;
const SCHED_ATTR_SIZE: usize = 56;

/// The `SCHED_RR` time slice (Linux's `RR_TIMESLICE`, 100 ms).
const RR_TIMESLICE_NS: i64 = 100_000_000;

// ---- Nice values ----

/// One thread that getpriority or setpriority acts on, and its process.
type Target = (Option<Rc<Task>>, Rc<RefCell<ThreadGroup>>);

pub(super) fn getpriority(cx: &mut Cx<'_>, which: i32, who: i32) -> SysResult {
    // As Linux's syscall does, return 20 - nice (1–40); libc converts it back
    let nice = targets(cx, which, who)?
        .iter()
        .map(|(t, _)| t.as_ref().map_or(0, |t| t.nice.get()))
        .min()
        .unwrap();
    Ok((20 - nice) as u64)
}

pub(super) fn setpriority(cx: &mut Cx<'_>, which: i32, who: i32, nice: i32) -> SysResult {
    let nice = nice.clamp(-20, 19);
    let cred = cx.cred();
    let mut result = Ok(0);
    for (t, g) in targets(cx, which, who)? {
        let g = g.borrow();
        let current = t.as_ref().map_or(0, |t| t.nice.get());
        // Another's process needs the same effective user ID or CAP_SYS_NICE over its user
        // namespace (set_one_prio); raising priority (lowering nice) must be within the target's
        // RLIMIT_NICE, or needs CAP_SYS_NICE (Linux's can_nice, in the initial namespace)
        if !owner_or_nice_over(&cred, &cred_of(t.as_ref())) {
            result = Err(Errno::EPERM);
        } else if nice < current
            && (20 - nice) as u64 > g.rlimits[RLIMIT_NICE as usize].0
            && !cred.capable(cap::SYS_NICE)
        {
            result = Err(Errno::EACCES);
        } else if let Some(t) = t {
            t.nice.set(nice);
            apply_host(&t);
        }
    }
    result
}

/// The threads getpriority and setpriority act on (zombies excluded). As on Linux, `PRIO_PROCESS`
/// is one thread, and `PRIO_PGRP` and `PRIO_USER` are every thread of those processes. The
/// built-in init has no threads but is a root process: its thread is None, its nice value is
/// always 0, and changing it has no effect.
fn targets(cx: &Cx<'_>, which: i32, who: i32) -> Result<Vec<Target>, Errno> {
    let k = cx.kr();
    let threads = |g: &Rc<RefCell<ThreadGroup>>| -> Vec<Target> {
        let gb = g.borrow();
        if gb.tgid == INIT && gb.threads.is_empty() {
            return vec![(None, g.clone())];
        }
        gb.threads
            .iter()
            .filter_map(|tid| k.tasks.get(tid))
            .map(|t| (Some(t.clone()), g.clone()))
            .collect()
    };
    let found: Vec<_> = match which {
        PRIO_PROCESS => match who {
            0 => vec![(Some(cx.task.clone()), cx.t.group.clone())],
            tid => match k.tasks.get(&tid) {
                Some(t) => k
                    .groups
                    .get(&t.tgid)
                    .map(|g| (Some(t.clone()), g.clone()))
                    .into_iter()
                    .collect(),
                None if tid == INIT => k.groups.get(&INIT).map(threads).unwrap_or_default(),
                None => Vec::new(),
            },
        },
        PRIO_PGRP => {
            let pgid = if who == 0 {
                cx.t.group.borrow().pgid
            } else {
                who
            };
            k.groups
                .values()
                .filter(|g| g.borrow().pgid == pgid)
                .flat_map(threads)
                .collect()
        }
        // `who` is a user ID in the caller's user namespace (0 for its own real user ID; one
        // that is not mapped matches nobody), and processes invisible in its pid namespace do
        // not count (Linux's make_kuid and task_pid_vnr)
        PRIO_USER => {
            let uid = if who == 0 {
                Some(cx.cred().ruid)
            } else {
                cx.cred().uid_in(who as u32)
            };
            // Each thread by its own real user ID (Linux's for_each_process_thread)
            k.groups
                .values()
                .filter(|g| cx.pid_out(g.borrow().tgid) != 0)
                .flat_map(threads)
                .filter(|(t, _)| Some(cred_of(t.as_ref()).ruid) == uid)
                .collect()
        }
        _ => return Err(Errno::EINVAL),
    };
    if found.is_empty() {
        return Err(Errno::ESRCH);
    }
    Ok(found)
}

// ---- I/O priority (Linux 6.1's block/ioprio.c, blk-ioc.c) ----

const IOPRIO_WHO_PROCESS: i32 = 1;
const IOPRIO_WHO_PGRP: i32 = 2;
const IOPRIO_WHO_USER: i32 = 3;

const IOPRIO_CLASS_SHIFT: u32 = 13;
const IOPRIO_CLASS_NONE: u16 = 0;
const IOPRIO_CLASS_RT: u16 = 1;
const IOPRIO_CLASS_BE: u16 = 2;
const IOPRIO_CLASS_IDLE: u16 = 3;
/// The number of RT and BE levels (0 is highest).
const IOPRIO_NR_LEVELS: u16 = 8;

pub(super) fn ioprio_set(cx: &mut Cx<'_>, which: i32, who: i32, ioprio: i32) -> SysResult {
    let cred = cx.cred();
    // io_context holds an unsigned short, so the high bits are dropped (the check looks only at
    // bits 0–15)
    let ioprio = ioprio as u16;
    ioprio_check(&cred, ioprio)?;
    // Linux uses make_kuid(current_user_ns(), who) here: who = 0 is root, not the caller
    let uid = (which == IOPRIO_WHO_USER)
        .then(|| cred.uid_in(who as u32))
        .flatten();
    for (t, _) in ioprio_targets(cx, which, who, uid)? {
        // set_task_ioprio: the target's real user ID must equal the caller's effective or real
        // user ID, or CAP_SYS_NICE is needed; for a process group or user the first failure
        // returns, with the earlier ones already changed
        let target = cred_of(t.as_ref()).ruid;
        if target != cred.euid && target != cred.ruid && !cred.capable(cap::SYS_NICE) {
            return Err(Errno::EPERM);
        }
        if let Some(t) = t {
            set_task_ioprio(cx, &t, ioprio);
        }
    }
    Ok(0)
}

/// Whether the ioprio_set value itself is valid and the caller may set it (`ioprio_check_cap`):
/// RT needs `CAP_SYS_ADMIN` or `CAP_SYS_NICE`, RT and BE levels are 0–7, the NONE level must be
/// 0, and the IDLE level is not checked.
fn ioprio_check(cred: &Cred, ioprio: u16) -> Result<(), Errno> {
    let data = ioprio & ((1 << IOPRIO_CLASS_SHIFT) - 1);
    match ioprio_class(ioprio) {
        IOPRIO_CLASS_RT | IOPRIO_CLASS_BE => {
            if ioprio_class(ioprio) == IOPRIO_CLASS_RT
                && !cred.capable(cap::SYS_ADMIN)
                && !cred.capable(cap::SYS_NICE)
            {
                return Err(Errno::EPERM);
            }
            if data >= IOPRIO_NR_LEVELS {
                return Err(Errno::EINVAL);
            }
            Ok(())
        }
        IOPRIO_CLASS_IDLE => Ok(()),
        IOPRIO_CLASS_NONE if data == 0 => Ok(()),
        _ => Err(Errno::EINVAL),
    }
}

/// Records thread `t`'s I/O priority (creating an io_context if it has none) and hands it to the
/// host thread. Threads sharing this one through `CLONE_IO` are handed to the host too.
fn set_task_ioprio(cx: &Cx<'_>, t: &Rc<Task>, ioprio: u16) {
    let shared = {
        let mut slot = t.ioprio.borrow_mut();
        let cell = slot.get_or_insert_with(|| Rc::new(Cell::new(0)));
        cell.set(ioprio);
        Rc::strong_count(cell) > 1
    };
    if shared {
        let mine = t.ioprio.borrow().clone();
        for other in cx.kr().tasks.values() {
            let same =
                matches!((&*other.ioprio.borrow(), &mine), (Some(a), Some(b)) if Rc::ptr_eq(a, b));
            if same {
                apply_host_ioprio(other, false);
            }
        }
    } else {
        apply_host_ioprio(t, false);
    }
}

pub(super) fn ioprio_get(cx: &mut Cx<'_>, which: i32, who: i32) -> SysResult {
    let cred = cx.cred();
    // Here who = 0 is the caller's real user (current_user())
    let uid = (which == IOPRIO_WHO_USER)
        .then(|| {
            if who == 0 {
                Some(cred.ruid)
            } else {
                cred.uid_in(who as u32)
            }
        })
        .flatten();
    let targets = ioprio_targets(cx, which, who, uid)?;
    // For one thread, the value it originally set (`get_task_raw_ioprio`); for a process group
    // or user, the highest of the effective priorities (the smallest value, `ioprio_best`)
    let prio = if which == IOPRIO_WHO_PROCESS {
        targets[0].0.as_deref().map_or(0, raw_ioprio)
    } else {
        targets
            .iter()
            .map(|(t, _)| effective_ioprio(t.as_deref()))
            .min()
            .unwrap()
    };
    Ok(prio as u64)
}

/// The thread's effective I/O priority (`__get_task_ioprio`): class NONE is derived from the
/// scheduling policy and the nice value, IDLE for `SCHED_IDLE`, RT for real-time policies and BE
/// otherwise, at level (nice + 20) / 5. The built-in init is BE 4.
fn effective_ioprio(t: Option<&Task>) -> u16 {
    let Some(t) = t else {
        return ioprio_value(IOPRIO_CLASS_BE, 4);
    };
    let raw = raw_ioprio(t);
    if ioprio_class(raw) != IOPRIO_CLASS_NONE {
        return raw;
    }
    let class = match t.sched.get().0 & !SCHED_RESET_ON_FORK {
        SCHED_IDLE => IOPRIO_CLASS_IDLE,
        SCHED_FIFO | SCHED_RR | SCHED_DEADLINE => IOPRIO_CLASS_RT,
        _ => IOPRIO_CLASS_BE,
    };
    ioprio_value(class, ((t.nice.get() + 20) / 5) as u16)
}

/// The threads ioprio_set and ioprio_get act on. As on Linux, `IOPRIO_WHO_PROCESS` is one thread,
/// `IOPRIO_WHO_PGRP` is every thread of every process in the process group, and
/// `IOPRIO_WHO_USER` is every thread whose real user ID is it (those in invisible pid namespaces
/// do not count). The built-in init has no threads and is None. Not found is ESRCH. `uid` is the
/// kernel user ID to look for with `IOPRIO_WHO_USER` (set and get interpret who = 0 differently,
/// so the caller computes it).
fn ioprio_targets(
    cx: &Cx<'_>,
    which: i32,
    who: i32,
    uid: Option<u32>,
) -> Result<Vec<Target>, Errno> {
    let k = cx.kr();
    let threads = |g: &Rc<RefCell<ThreadGroup>>| -> Vec<Target> {
        let gb = g.borrow();
        if gb.tgid == INIT && gb.threads.is_empty() {
            return vec![(None, g.clone())];
        }
        gb.threads
            .iter()
            .filter_map(|tid| k.tasks.get(tid))
            .map(|t| (Some(t.clone()), g.clone()))
            .collect()
    };
    let found: Vec<_> = match which {
        IOPRIO_WHO_PROCESS => match who {
            0 => vec![(Some(cx.task.clone()), cx.t.group.clone())],
            tid => match k.tasks.get(&tid) {
                Some(t) => k
                    .groups
                    .get(&t.tgid)
                    .map(|g| (Some(t.clone()), g.clone()))
                    .into_iter()
                    .collect(),
                None if tid == INIT => k.groups.get(&INIT).map(threads).unwrap_or_default(),
                None => Vec::new(),
            },
        },
        IOPRIO_WHO_PGRP => {
            let pgid = if who == 0 {
                cx.t.group.borrow().pgid
            } else {
                who
            };
            k.groups
                .values()
                .filter(|g| g.borrow().pgid == pgid)
                .flat_map(threads)
                .collect()
        }
        IOPRIO_WHO_USER => match uid {
            Some(uid) => k
                .groups
                .values()
                .filter(|g| cx.pid_out(g.borrow().tgid) != 0)
                .flat_map(threads)
                .filter(|(t, _)| cred_of(t.as_ref()).ruid == uid)
                .collect(),
            None => Vec::new(),
        },
        _ => return Err(Errno::EINVAL),
    };
    if found.is_empty() {
        return Err(Errno::ESRCH);
    }
    Ok(found)
}

// ---- sched_* ----

pub(super) fn sched_setscheduler(cx: &mut Cx<'_>, pid: i32, policy: i32, param: u64) -> SysResult {
    if pid < 0 {
        return Err(Errno::EINVAL);
    }
    let priority = read_priority(cx, param)?;
    let task = target(cx, pid)?;
    set_policy(cx, task.as_ref(), (policy, priority), true).map(|()| 0)
}

pub(super) fn sched_setparam(cx: &mut Cx<'_>, pid: i32, param: u64) -> SysResult {
    if pid < 0 {
        return Err(Errno::EINVAL);
    }
    let priority = read_priority(cx, param)?;
    let task = target(cx, pid)?;
    let policy = task.as_ref().map_or(SCHED_OTHER, |t| t.sched.get().0);
    set_policy(cx, task.as_ref(), (policy, priority), true).map(|()| 0)
}

/// Reads the priority of a `struct sched_param`.
fn read_priority(cx: &Cx<'_>, param: u64) -> Result<i32, Errno> {
    if param == 0 {
        return Err(Errno::EINVAL);
    }
    Ok(i32::from_le_bytes(get(cx, param)?))
}

pub(super) fn sched_getscheduler(cx: &mut Cx<'_>, pid: i32) -> SysResult {
    let task = target(cx, pid)?;
    Ok(task.map_or(SCHED_OTHER, |t| t.sched.get().0) as u64)
}

pub(super) fn sched_getparam(cx: &mut Cx<'_>, pid: i32, param: u64) -> SysResult {
    if param == 0 || pid < 0 {
        return Err(Errno::EINVAL);
    }
    let task = target(cx, pid)?;
    let priority = task.map_or(0, |t| t.sched.get().1);
    put(cx, param, priority.to_le_bytes()).map(|()| 0)
}

pub(super) fn sched_setattr(cx: &mut Cx<'_>, pid: i32, attr: u64, flags: u32) -> SysResult {
    if attr == 0 || pid < 0 || flags != 0 {
        return Err(Errno::EINVAL);
    }
    let size = match u32::from_le_bytes(get(cx, attr)?) as usize {
        0 => SCHED_ATTR_SIZE_VER0,
        n => n,
    };
    if !(SCHED_ATTR_SIZE_VER0..=4096).contains(&size) {
        put(cx, attr, (SCHED_ATTR_SIZE as u32).to_le_bytes())?;
        return Err(Errno::E2BIG);
    }
    // Fields past a shorter structure read as 0 (copy_struct_from_user): the first version ends
    // before util_min/util_max
    let mut b = vec![0u8; size.max(SCHED_ATTR_SIZE)];
    cx.mm()
        .read(crate::uaccess::UserSlice::new(attr, size), &mut b[..size])?;
    // When longer than the version known here, the extra part must be 0
    if b.iter().skip(SCHED_ATTR_SIZE).any(|&x| x != 0) {
        put(cx, attr, (SCHED_ATTR_SIZE as u32).to_le_bytes())?;
        return Err(Errno::E2BIG);
    }
    let word = |off: usize| u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
    let long = |off: usize| u64::from_le_bytes(b[off..off + 8].try_into().unwrap());
    let mut policy = word(4) as i32;
    let sched_flags = long(8);
    // sched_copy_attr: the clamps need the structure that has them; nice is cut to -20..19
    let clamps = sched_flags & (SCHED_FLAG_UTIL_CLAMP_MIN | SCHED_FLAG_UTIL_CLAMP_MAX);
    if clamps != 0 && size < SCHED_ATTR_SIZE {
        return Err(Errno::EINVAL);
    }
    let mut nice = (word(16) as i32).clamp(-20, 19);
    let mut priority = word(20) as i32;
    let mut dl = (long(24), long(32), long(40));
    if policy < 0 {
        return Err(Errno::EINVAL);
    }
    let task = target(cx, pid)?;
    // SCHED_FLAG_KEEP_POLICY keeps the policy and its reset-on-fork; SCHED_FLAG_KEEP_PARAMS takes
    // the current parameters (get_params)
    let (cur_policy, cur_priority) = task.as_ref().map_or((SCHED_OTHER, 0), |t| t.sched.get());
    let keep_policy = sched_flags & SCHED_FLAG_KEEP_POLICY != 0;
    let keep_params = sched_flags & SCHED_FLAG_KEEP_PARAMS != 0;
    if keep_policy {
        policy = cur_policy & !SCHED_RESET_ON_FORK;
    } else if policy & SCHED_RESET_ON_FORK != 0 {
        // Only sched_setscheduler takes it inside the policy; here it is not a valid policy
        return Err(Errno::EINVAL);
    }
    if keep_params && let Some(t) = &task {
        nice = t.nice.get();
        priority = cur_priority;
        dl = t.deadline.get();
    }
    if sched_flags & !SCHED_FLAG_ALL != 0 {
        return Err(Errno::EINVAL);
    }
    // uclamp_validate: each at most 1024, or -1 for the default; a minimum and a maximum (each the
    // one given, or the thread's current one) not crossing
    let (util_min, util_max) = (word(48), word(52));
    let given = |flag: u64, v: u32| (sched_flags & flag != 0 && v != u32::MAX).then_some(v);
    let (min, max) = (
        given(SCHED_FLAG_UTIL_CLAMP_MIN, util_min),
        given(SCHED_FLAG_UTIL_CLAMP_MAX, util_max),
    );
    let (cur_min, cur_max) = task.as_ref().map_or((0, UCLAMP_MAX), |t| t.uclamp.get());
    let effective = |flag: u64, v: Option<u32>, cur: u32| {
        if sched_flags & flag != 0 {
            v
        } else {
            Some(cur)
        }
    };
    if min.is_some_and(|v| v > UCLAMP_MAX)
        || max.is_some_and(|v| v > UCLAMP_MAX)
        || effective(SCHED_FLAG_UTIL_CLAMP_MIN, min, cur_min)
            .zip(effective(SCHED_FLAG_UTIL_CLAMP_MAX, max, cur_max))
            .is_some_and(|(lo, hi)| lo > hi)
    {
        return Err(Errno::EINVAL);
    }
    if policy == SCHED_DEADLINE {
        set_deadline(cx, task.as_ref(), priority, dl)?;
    } else {
        let reset = if keep_policy {
            cur_policy & SCHED_RESET_ON_FORK
        } else if sched_flags & SCHED_FLAG_RESET_ON_FORK != 0 {
            SCHED_RESET_ON_FORK
        } else {
            0
        };
        // With SCHED_FLAG_KEEP_PARAMS neither the parameters nor the policy change
        // (__setscheduler_params is skipped); only reset-on-fork and the clamps do
        set_policy(cx, task.as_ref(), (policy | reset, priority), !keep_params)?;
        if keep_params && let Some(t) = &task {
            t.sched
                .set((cur_policy & !SCHED_RESET_ON_FORK | reset, cur_priority));
        }
        // Only SCHED_OTHER and SCHED_BATCH take the nice value (fair_policy): SCHED_IDLE keeps the
        // one the thread had, which leaving it again is judged by
        if !keep_params
            && matches!(policy, SCHED_OTHER | SCHED_BATCH)
            && let Some(t) = &task
        {
            let tid = t.tid.get();
            // A nice value beyond RLIMIT_NICE is EPERM here (setpriority answers EACCES)
            setpriority(cx, PRIO_PROCESS, tid, nice).map_err(|e| match e {
                Errno::EACCES => Errno::EPERM,
                e => e,
            })?;
        }
    }
    // __setscheduler_uclamp: -1 goes back to the default
    if clamps != 0
        && let Some(t) = &task
    {
        let (mut lo, mut hi) = t.uclamp.get();
        if sched_flags & SCHED_FLAG_UTIL_CLAMP_MIN != 0 {
            lo = min.unwrap_or(0);
        }
        if sched_flags & SCHED_FLAG_UTIL_CLAMP_MAX != 0 {
            hi = max.unwrap_or(UCLAMP_MAX);
        }
        t.uclamp.set((lo, hi));
    }
    Ok(0)
}

/// Sets `SCHED_DEADLINE` (`__checkparam_dl`): runtime ≤ deadline ≤ period, runtime at least
/// 1024 ns, and a period of 0 equals the deadline.
fn set_deadline(
    cx: &Cx<'_>,
    task: Option<&Rc<Task>>,
    priority: i32,
    (runtime, deadline, period): (u64, u64, u64),
) -> SysResult {
    let period = if period == 0 { deadline } else { period };
    if priority != 0
        || deadline == 0
        || runtime < 1024
        || (deadline | period) & (1 << 63) != 0
        || period < deadline
        || deadline < runtime
    {
        return Err(Errno::EINVAL);
    }
    let cred = cx.cred();
    if !same_owner(&cred, &cred_of(task)) || !cred.capable(cap::SYS_NICE) {
        return Err(Errno::EPERM);
    }
    if let Some(t) = task {
        t.sched.set((SCHED_DEADLINE, 0));
        t.deadline.set((runtime, deadline, period));
    }
    Ok(0)
}

/// Sets the scheduling policy: checks the policy, priority and permissions.
fn set_policy(
    cx: &Cx<'_>,
    task: Option<&Rc<Task>>,
    (policy, priority): (i32, i32),
    apply: bool,
) -> Result<(), Errno> {
    let base = policy & !SCHED_RESET_ON_FORK;
    let rt = match base {
        SCHED_FIFO | SCHED_RR => true,
        SCHED_OTHER | SCHED_BATCH | SCHED_IDLE => false,
        _ => return Err(Errno::EINVAL),
    };
    if rt && !(1..=99).contains(&priority) || !rt && priority != 0 {
        return Err(Errno::EINVAL);
    }
    let cred = cx.cred();
    if !same_owner(&cred, &cred_of(task)) || rt && !cred.capable(cap::SYS_NICE) {
        return Err(Errno::EPERM);
    }
    // Without CAP_SYS_NICE (__sched_setscheduler): SCHED_IDLE counts as nice 20, so leaving it
    // needs the RLIMIT_NICE that would allow the current nice value; and reset-on-fork, once set,
    // cannot be cleared
    if let Some(t) = task
        && !cred.capable(cap::SYS_NICE)
    {
        let cur = t.sched.get().0;
        let rlimit_nice = cx
            .kr()
            .groups
            .get(&t.tgid)
            .map_or(u64::MAX, |g| g.borrow().rlimits[RLIMIT_NICE as usize].0);
        if cur & !SCHED_RESET_ON_FORK == SCHED_IDLE
            && base != SCHED_IDLE
            && (20 - t.nice.get()) as u64 > rlimit_nice
        {
            return Err(Errno::EPERM);
        }
        if cur & SCHED_RESET_ON_FORK != 0 && policy & SCHED_RESET_ON_FORK == 0 {
            return Err(Errno::EPERM);
        }
    }
    if let Some(t) = task.filter(|_| apply) {
        t.sched.set((policy, priority));
        apply_host(t);
    }
    Ok(())
}

pub(super) fn sched_getattr(
    cx: &mut Cx<'_>,
    pid: i32,
    attr: u64,
    size: u32,
    flags: u32,
) -> SysResult {
    let size = size as usize;
    if attr == 0 || pid < 0 || flags != 0 || !(SCHED_ATTR_SIZE_VER0..=4096).contains(&size) {
        return Err(Errno::EINVAL);
    }
    let task = target(cx, pid)?;
    let (policy, priority) = task.as_ref().map_or((SCHED_OTHER, 0), |t| t.sched.get());
    let nice = task.as_ref().map_or(0, |t| t.nice.get());
    let n = size.min(SCHED_ATTR_SIZE);
    let mut b = [0u8; SCHED_ATTR_SIZE];
    b[..4].copy_from_slice(&(n as u32).to_le_bytes());
    b[4..8].copy_from_slice(&(policy & !SCHED_RESET_ON_FORK).to_le_bytes());
    if policy & SCHED_RESET_ON_FORK != 0 {
        b[8..16].copy_from_slice(&SCHED_FLAG_RESET_ON_FORK.to_le_bytes());
    }
    b[16..20].copy_from_slice(&nice.to_le_bytes());
    b[20..24].copy_from_slice(&priority.to_le_bytes());
    if policy == SCHED_DEADLINE
        && let Some(t) = &task
    {
        let (runtime, deadline, period) = t.deadline.get();
        b[24..32].copy_from_slice(&runtime.to_le_bytes());
        b[32..40].copy_from_slice(&deadline.to_le_bytes());
        b[40..48].copy_from_slice(&period.to_le_bytes());
    }
    let (util_min, util_max) = task.as_ref().map_or((0, UCLAMP_MAX), |t| t.uclamp.get());
    b[48..52].copy_from_slice(&util_min.to_le_bytes());
    b[52..56].copy_from_slice(&util_max.to_le_bytes());
    cx.mm()
        .write(crate::uaccess::UserSlice::new(attr, n), &b[..n])
        .map(|()| 0)
}

pub(super) fn sched_setaffinity(cx: &mut Cx<'_>, pid: i32, len: usize, mask: u64) -> SysResult {
    // A mask shorter than the kernel's cpumask (8 bytes) leaves the remaining bits 0; any longer
    // part is ignored
    let mut b = [0u8; 8];
    let n = len.min(8);
    cx.mm()
        .read(crate::uaccess::UserSlice::new(mask, n), &mut b[..n])?;
    let task = affinity_target(cx, pid)?;
    let new = u64::from_le_bytes(b) & online_cpus();
    if !owner_or_nice_over(&cx.cred(), &cred_of(task.as_ref())) {
        return Err(Errno::EPERM);
    }
    if new == 0 {
        return Err(Errno::EINVAL);
    }
    if let Some(t) = task {
        t.affinity.set(new);
        if t.host_tid.get() != 0 {
            tuxport_hal::set_thread_affinity(t.host_tid.get(), new);
        }
    }
    Ok(0)
}

pub(super) fn sched_getaffinity(cx: &mut Cx<'_>, pid: i32, len: usize, mask: u64) -> SysResult {
    // The kernel's cpumask is aligned to unsigned long; the guest's buffer must at least hold it
    if len < 8 || !len.is_multiple_of(8) {
        return Err(Errno::EINVAL);
    }
    let task = affinity_target(cx, pid)?;
    let bits = task.map_or(u64::MAX, |t| t.affinity.get()) & online_cpus();
    put(cx, mask, bits.to_le_bytes())?;
    Ok(8)
}

/// The affinity target thread: as on Linux, a negative pid is a process not found (ESRCH), not
/// EINVAL.
fn affinity_target(cx: &Cx<'_>, pid: i32) -> Result<Option<Rc<Task>>, Errno> {
    if pid < 0 {
        return Err(Errno::ESRCH);
    }
    target(cx, pid)
}

/// sched_yield when the fast path cannot take it (tracing, a seccomp filter to run;
/// `syscall::fast`): the host thread yields as there, with the BKL released.
pub(super) fn sched_yield(cx: &mut Cx<'_>) -> SysResult {
    cx.unlocked(std::thread::yield_now);
    Ok(0)
}

pub(super) fn sched_priority_range(policy: i32, max: bool) -> SysResult {
    match policy {
        SCHED_FIFO | SCHED_RR => Ok(if max { 99 } else { 1 }),
        SCHED_OTHER | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE => Ok(0),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn sched_rr_get_interval(cx: &mut Cx<'_>, pid: i32, ts: u64) -> SysResult {
    let task = target(cx, pid)?;
    // Only SCHED_RR threads have a time slice
    let policy = task.map_or(SCHED_OTHER, |t| t.sched.get().0 & !SCHED_RESET_ON_FORK);
    let ns = if policy == SCHED_RR {
        RR_TIMESLICE_NS
    } else {
        0
    };
    write_pair(cx, ts, 0, ns).map(|()| 0)
}

// ---- New threads and their host threads ----

/// When the guest raises priority, the host thread's nice value goes no lower than this: never
/// above Android's display-related threads (-4 to -10), so guest busy loops do not slow the UI.
const HOST_NICE_FLOOR: i32 = -4;

/// The scheduling policy and nice value a new thread inherits: with `SCHED_RESET_ON_FORK`,
/// real-time policies go back to the normal policy and negative nice values back to 0.
pub(super) fn inherit((policy, priority): (i32, i32), nice: i32) -> ((i32, i32), i32) {
    if policy & SCHED_RESET_ON_FORK != 0 {
        let base = policy & !SCHED_RESET_ON_FORK;
        if matches!(base, SCHED_FIFO | SCHED_RR) {
            return ((SCHED_OTHER, 0), 0);
        }
        return ((base, priority), nice.max(0));
    }
    ((policy, priority), nice)
}

/// A new thread's I/O priority (`copy_io`): when the creator has an io_context, `CLONE_IO` shares
/// it, otherwise a copy is made if its class is not NONE; if the creator has none, neither does
/// the new thread.
pub(super) fn inherit_ioprio(parent: &Option<Rc<Cell<u16>>>, flags: u64) -> Option<Rc<Cell<u16>>> {
    const CLONE_IO: u64 = 0x8000_0000;
    let p = parent.as_ref()?;
    if flags & CLONE_IO != 0 {
        Some(p.clone())
    } else {
        (ioprio_class(p.get()) != IOPRIO_CLASS_NONE).then(|| Rc::new(Cell::new(p.get())))
    }
}

/// A host thread starts running guest thread `t`: it has inherited the priority, I/O priority and
/// affinity of the host thread that created it (of the previous guest thread when `reused`), and
/// switches to `t`'s own.
pub(crate) fn host_started(t: &Task, reused: bool) {
    apply_host(t);
    apply_host_ioprio(t, reused);
    let mask = t.affinity.get() & online_cpus();
    // A host thread reused from the pool still has the previous guest thread's affinity
    if mask != online_cpus() || reused {
        tuxport_hal::set_thread_affinity(t.host_tid.get(), mask);
    }
}

/// Sets a guest thread's host thread nice value from the guest thread's nice value and scheduling
/// policy. `SCHED_IDLE` uses the host's lowest priority. If the host thread has not started
/// running yet, it is set when it starts.
fn apply_host(t: &Task) {
    let tid = t.host_tid.get();
    if tid == 0 {
        return;
    }
    let nice = if t.sched.get().0 & !SCHED_RESET_ON_FORK == SCHED_IDLE {
        19
    } else {
        t.nice.get().max(HOST_NICE_FLOOR)
    };
    tuxport_hal::set_thread_nice(tid, nice);
}

/// Sets a guest thread's host thread I/O priority from the guest thread's: RT becomes BE at the
/// same level (as with nice values, the guest may not let the host thread's I/O override the UI),
/// and NONE lets the host derive it from the nice value. With `reset`, it is written once even if
/// never set (a host thread reused from the pool still has the previous guest thread's).
fn apply_host_ioprio(t: &Task, reset: bool) {
    let tid = t.host_tid.get();
    if tid == 0 || (!reset && t.ioprio.borrow().is_none()) {
        return;
    }
    let raw = raw_ioprio(t);
    let host = if ioprio_class(raw) == IOPRIO_CLASS_RT {
        ioprio_value(IOPRIO_CLASS_BE, raw & ((1 << IOPRIO_CLASS_SHIFT) - 1))
    } else {
        raw
    };
    tuxport_hal::set_thread_ioprio(tid, host);
}

// ---- Shared helpers ----

/// The target thread of `sched_*`: pid 0 is the caller. The built-in init has no threads and
/// returns None. Zombies (and processes whose leader has exited) are None too: on Linux the leader
/// can still be found until it is reaped, and is handled with the default scheduling parameters.
fn target(cx: &Cx<'_>, pid: i32) -> Result<Option<Rc<Task>>, Errno> {
    match pid {
        p if p < 0 => Err(Errno::EINVAL),
        0 => Ok(Some(cx.task.clone())),
        INIT => Ok(None),
        p => match cx.kr().tasks.get(&p) {
            Some(t) => Ok(Some(t.clone())),
            None if cx.kr().groups.contains_key(&p) => Ok(None),
            None => Err(Errno::ESRCH),
        },
    }
}

/// The credentials of a target thread; the built-in init, which has none, is root.
fn cred_of(task: Option<&Rc<Task>>) -> Cred {
    task.map_or_else(Cred::root, |t| t.cred.borrow().clone())
}

/// Whether the scheduling parameters of a process with credentials `target` may be changed
/// (`__sched_setscheduler`: `check_same_owner`, or `CAP_SYS_NICE` in the initial namespace).
fn same_owner(cred: &Cred, target: &Cred) -> bool {
    cred.euid == target.euid || cred.euid == target.ruid || cred.capable(cap::SYS_NICE)
}

/// Whether the nice value or CPU affinity of a process with credentials `target` may be changed
/// (`set_one_prio`, `sched_setaffinity`): the same owner, or `CAP_SYS_NICE` over the target's user
/// namespace, so root of a container may change its own processes'.
fn owner_or_nice_over(cred: &Cred, target: &Cred) -> bool {
    cred.euid == target.euid
        || cred.euid == target.ruid
        || cred.ns_capable(&target.user_ns, cap::SYS_NICE)
}

/// The mask for the host's CPU count (at most 64).
fn online_cpus() -> u64 {
    let cpus = tuxport_hal::host_cpus().len();
    if cpus == 64 {
        u64::MAX
    } else {
        (1u64 << cpus) - 1
    }
}

/// The thread's raw I/O priority: 0 if never set (`IOPRIO_DEFAULT`, class NONE).
fn raw_ioprio(t: &Task) -> u16 {
    t.ioprio.borrow().as_ref().map_or(0, |c| c.get())
}

fn ioprio_class(ioprio: u16) -> u16 {
    (ioprio >> IOPRIO_CLASS_SHIFT) & 7
}

fn ioprio_value(class: u16, data: u16) -> u16 {
    (class << IOPRIO_CLASS_SHIFT) | data
}
