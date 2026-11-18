//! POSIX timers: timer_create, timer_settime, timer_gettime, timer_getoverrun, timer_delete
//! (timing and expiry in [`crate::itimer`]).
//!
//! Timers belong to the process, and an id takes the lowest free value starting from 0; they are
//! not inherited across fork and are deleted on execve. musl implements `SIGEV_THREAD` in user
//! space; the kernel sees `SIGEV_THREAD_ID`.

use std::time::Duration;

use super::misc::expiry_in_ns;
use super::{SysResult, get, put};
use crate::errno::Errno;
use crate::itimer::{Clock, Expiry, PosixTimer};
use crate::kernel::Cx;
use crate::process::Signal;

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
const CLOCK_BOOTTIME: i32 = 7;
const CLOCK_REALTIME_ALARM: i32 = 8;
const CLOCK_BOOTTIME_ALARM: i32 = 9;
const CLOCK_TAI: i32 = 11;

const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD: i32 = 2;
const SIGEV_THREAD_ID: i32 = 4;

const TIMER_ABSTIME: u32 = 1;
/// The most timers one process may have.
const MAX_TIMERS: usize = 4096;
/// The capability the wake-alarm clocks require (`CAP_WAKE_ALARM`).
const CAP_WAKE_ALARM: u32 = 35;

pub(super) fn timer_create(cx: &mut Cx<'_>, clockid: i32, sevp: u64, idp: u64) -> SysResult {
    let clock = clock_of(cx, clockid)?;
    // Without a sigevent: SIGEV_SIGNAL, SIGALRM, and sigev_value is the timer id
    let (value, signo, notify, tid) = if sevp == 0 {
        (None, Signal::ALRM.number(), SIGEV_SIGNAL, 0)
    } else {
        let b: [u8; 64] = get(cx, sevp)?;
        let word = |off: usize| i32::from_le_bytes(b[off..off + 4].try_into().unwrap());
        (
            Some(u64::from_le_bytes(b[..8].try_into().unwrap())),
            word(8),
            word(12),
            word(16),
        )
    };
    let signal = match notify {
        SIGEV_NONE => None,
        SIGEV_SIGNAL | SIGEV_THREAD | SIGEV_THREAD_ID => {
            let sig = Signal::new(signo).ok_or(Errno::EINVAL)?;
            let target = if notify == SIGEV_THREAD_ID {
                // The target thread must be in the same process
                let tid = cx.pid_in(tid);
                match cx.kr().tasks.get(&tid) {
                    Some(t) if t.tgid == cx.tgid() => Some(tid),
                    _ => return Err(Errno::EINVAL),
                }
            } else {
                None
            };
            Some((sig, target))
        }
        _ => return Err(Errno::EINVAL),
    };
    let mut g = cx.t.group.borrow_mut();
    if g.posix_timers.len() >= MAX_TIMERS {
        return Err(Errno::EAGAIN);
    }
    let id = (0..).find(|i| !g.posix_timers.contains_key(i)).unwrap();
    g.posix_timers.insert(
        id,
        PosixTimer {
            clock,
            signal,
            value: value.unwrap_or(id as u64),
            deadline: None,
            interval: Duration::ZERO,
            overrun: 0,
        },
    );
    drop(g);
    if let Err(e) = put(cx, idp, id.to_le_bytes()) {
        cx.t.group.borrow_mut().posix_timers.remove(&id);
        return Err(e);
    }
    Ok(0)
}

/// The way of keeping time that a clock id stands for.
fn clock_of(cx: &Cx<'_>, clockid: i32) -> Result<Clock, Errno> {
    Ok(match clockid {
        CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME | CLOCK_TAI => Clock::Wall(clockid),
        // Wake alarms: the host app cannot wake the device, so they are timed as ordinary clocks
        CLOCK_REALTIME_ALARM | CLOCK_BOOTTIME_ALARM => {
            if !cx.cred().capable(CAP_WAKE_ALARM) {
                return Err(Errno::EPERM);
            }
            Clock::Wall(if clockid == CLOCK_REALTIME_ALARM {
                CLOCK_REALTIME
            } else {
                CLOCK_BOOTTIME
            })
        }
        CLOCK_PROCESS_CPUTIME_ID => Clock::Process {
            tgid: cx.tgid(),
            serial: cx.t.group.borrow().serial,
        },
        CLOCK_THREAD_CPUTIME_ID => Clock::Thread {
            tid: cx.tid(),
            serial: cx.task.serial,
        },
        // Clocks encoded by clock_getcpuclockid: (~pid << 3) | type, bit 2 meaning a thread, the
        // pid as the caller's pid namespace numbers it
        c if c < 0 && c & 3 != 3 => {
            let thread = c & 4 != 0;
            let pid = match !(c >> 3) {
                0 if thread => cx.tid(),
                0 => cx.tgid(),
                p => cx.pid_in(p),
            };
            let k = cx.kr();
            if thread {
                let serial = k.tasks.get(&pid).ok_or(Errno::EINVAL)?.serial;
                Clock::Thread { tid: pid, serial }
            } else {
                let serial = k.groups.get(&pid).ok_or(Errno::EINVAL)?.borrow().serial;
                Clock::Process { tgid: pid, serial }
            }
        }
        // CLOCK_MONOTONIC_RAW and the coarse clocks cannot be used for timers
        4..=6 => return Err(Errno::EOPNOTSUPP),
        _ => return Err(Errno::EINVAL),
    })
}

pub(super) fn timer_settime(cx: &mut Cx<'_>, id: i32, flags: u32, new: u64, old: u64) -> SysResult {
    if new == 0 {
        return Err(Errno::EINVAL);
    }
    let mut t = timer(cx, id)?;
    let b: [u8; 32] = get(cx, new)?;
    let ts = |off: usize| -> Result<Duration, Errno> {
        let sec = i64::from_le_bytes(b[off..off + 8].try_into().unwrap());
        let nsec = i64::from_le_bytes(b[off + 8..off + 16].try_into().unwrap());
        super::timespec(sec, nsec).ok_or(Errno::EINVAL)
    };
    let (interval, value) = (ts(0)?, ts(16)?);
    // A CPU clock whose thread or process is gone cannot be set, nor disarmed
    if t.gone(cx.kr()) {
        return Err(Errno::ESRCH);
    }
    if old != 0 {
        let left = t.remaining(cx.kr());
        put(cx, old, encode_itimerspec(&t, left))?;
    }
    let abs = flags & TIMER_ABSTIME != 0;
    if value.is_zero() {
        t.deadline = None;
    } else {
        let at = match t.clock {
            Clock::Wall(clock) if abs => Some(expiry_in_ns(cx, clock, value)?),
            Clock::Wall(clock) => Some(Expiry::new(clock, value, false)?),
            _ => None,
        };
        t.arm(cx.kr(), value, at, abs);
    }
    t.interval = interval;
    t.overrun = 0;
    cx.t.group.borrow_mut().posix_timers.insert(id, t);
    // Make the timer thread recompute how long to wait (waking only it)
    crate::kernel::signal_later(&cx.shared.timers);
    Ok(0)
}

pub(super) fn timer_gettime(cx: &mut Cx<'_>, id: i32, cur: u64) -> SysResult {
    let t = timer(cx, id)?;
    // A CPU clock whose thread or process is gone reads as all zeros, the interval too
    let spec = if t.gone(cx.kr()) {
        [0; 32]
    } else {
        encode_itimerspec(&t, t.remaining(cx.kr()))
    };
    put(cx, cur, spec).map(|()| 0)
}

pub(super) fn timer_getoverrun(cx: &mut Cx<'_>, id: i32) -> SysResult {
    Ok(timer(cx, id)?.overrun.min(i32::MAX as u32) as u64)
}

pub(super) fn timer_delete(cx: &mut Cx<'_>, id: i32) -> SysResult {
    cx.t.group
        .borrow_mut()
        .posix_timers
        .remove(&id)
        .map(|_| 0)
        .ok_or(Errno::EINVAL)
}

fn timer(cx: &Cx<'_>, id: i32) -> Result<PosixTimer, Errno> {
    cx.t.group
        .borrow()
        .posix_timers
        .get(&id)
        .cloned()
        .ok_or(Errno::EINVAL)
}

/// `struct itimerspec`: the interval and how long until the next expiry (both 0 when not armed).
fn encode_itimerspec(t: &PosixTimer, left: Option<Duration>) -> [u8; 32] {
    let left = left.unwrap_or(Duration::ZERO);
    // A timer that has not expired yet reports at least 1 nanosecond, as on Linux
    let left = if t.deadline.is_some() {
        left.max(Duration::from_nanos(1))
    } else {
        left
    };
    let mut b = [0u8; 32];
    for (o, d) in [(0, t.interval), (16, left)] {
        b[o..o + 8].copy_from_slice(&(d.as_secs() as i64).to_le_bytes());
        b[o + 8..o + 16].copy_from_slice(&(d.subsec_nanos() as i64).to_le_bytes());
    }
    b
}
