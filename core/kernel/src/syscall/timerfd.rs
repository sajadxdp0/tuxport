//! timerfd (the kernel/poll map): timerfd_create, timerfd_settime, timerfd_gettime, and reads from
//! it.
//!
//! Expiration times are kept on the clock's base ([`crate::itimer::Expiry`]: relative realtime and
//! monotonic times on the host's monotonic clock, absolute realtime times on its realtime clock,
//! boottime on its boottime clock), and reads and poll compute the number of expirations already
//! passed; waiting threads use the earliest expiration time as their timeout ([`super::poll`]).
//! `TFD_TIMER_CANCEL_ON_SET` on an absolute realtime timer counts a jump of the host's wall clock
//! (set, or the device resuming from sleep) as an expiration that reads as `ECANCELED`; the guest
//! itself cannot set the clock.

use std::rc::Rc;
use std::time::Duration;

use super::misc::expiry_in_ns;
use super::{SysResult, get, put};
use crate::cell::RefCell;
use crate::errno::Errno;
use crate::fd::{FileKind, OpenFile, TimerFd, oflags};
use crate::kernel::{Cx, request_wake_object, wake};

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_BOOTTIME: i32 = 7;
const CLOCK_REALTIME_ALARM: i32 = 8;
const CLOCK_BOOTTIME_ALARM: i32 = 9;

const TFD_TIMER_ABSTIME: u32 = 1;
const TFD_TIMER_CANCEL_ON_SET: u32 = 2;

pub(super) fn timerfd_create(cx: &mut Cx<'_>, clock: i32, flags: u32) -> SysResult {
    if !matches!(
        clock,
        CLOCK_REALTIME
            | CLOCK_MONOTONIC
            | CLOCK_BOOTTIME
            | CLOCK_REALTIME_ALARM
            | CLOCK_BOOTTIME_ALARM
    ) {
        return Err(Errno::EINVAL);
    }
    if flags & !(oflags::O_CLOEXEC | oflags::O_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    // The alarm clocks could wake the system, so they need CAP_WAKE_ALARM (in the initial
    // namespace, Linux's capable)
    if matches!(clock, CLOCK_REALTIME_ALARM | CLOCK_BOOTTIME_ALARM)
        && !cx.cred().capable(crate::cred::cap::WAKE_ALARM)
    {
        return Err(Errno::EPERM);
    }
    let t = TimerFd {
        clock,
        deadline: None,
        cancel: None,
        interval: Duration::ZERO,
        expirations: 0,
        fired: 0,
        settime_flags: 0,
    };
    // Read-write, as Linux's anon_inode_getfd gives it: write is then EINVAL (no write operation),
    // not EBADF, and F_GETFL shows O_RDWR
    let file = OpenFile::new(
        FileKind::TimerFd(Rc::new(RefCell::new(t))),
        oflags::O_RDWR | (flags & oflags::O_NONBLOCK),
    );
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

pub(super) fn timerfd_settime(
    cx: &mut Cx<'_>,
    fd: i32,
    flags: u32,
    new: u64,
    old: u64,
) -> SysResult {
    if flags & !(TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET) != 0 {
        return Err(Errno::EINVAL);
    }
    let timer = timer_of(cx, fd)?;
    let spec: [u8; 32] = get(cx, new)?;
    let interval = timespec(&spec[..16])?;
    let value = timespec(&spec[16..])?;
    // Setting an alarm timer needs CAP_WAKE_ALARM too, even one created by a process that had it
    let alarm = matches!(
        timer.borrow().clock,
        CLOCK_REALTIME_ALARM | CLOCK_BOOTTIME_ALARM
    );
    if alarm && !cx.cred().capable(crate::cred::cap::WAKE_ALARM) {
        return Err(Errno::EPERM);
    }
    let mut t = timer.borrow_mut();
    if old != 0 {
        put(cx, old, current(&mut t))?;
    }
    t.interval = interval;
    t.expirations = 0;
    t.settime_flags = flags;
    // An alarm clock reads the same as its ordinary counterpart
    let clock = match t.clock {
        CLOCK_REALTIME_ALARM => CLOCK_REALTIME,
        CLOCK_BOOTTIME_ALARM => CLOCK_BOOTTIME,
        c => c,
    };
    let abs = flags & TFD_TIMER_ABSTIME != 0;
    // timerfd_setup_cancel: only an absolute realtime timer can be cancelled by a clock jump
    t.cancel = (clock == CLOCK_REALTIME && abs && flags & TFD_TIMER_CANCEL_ON_SET != 0)
        .then(|| (crate::itimer::clock_epoch(), false));
    t.deadline = if value.is_zero() {
        None
    } else if abs {
        Some(expiry_in_ns(cx, clock, value)?)
    } else {
        Some(crate::itimer::Expiry::new(clock, value, false)?)
    };
    // Other threads may be waiting on it with the old expiration time (epoll_wait, read)
    request_wake_object(wake::of(&timer));
    Ok(0)
}

/// Reads a `struct timespec` (`super::timespec`): EINVAL if the seconds are negative or the
/// nanoseconds out of range.
fn timespec(b: &[u8]) -> Result<Duration, Errno> {
    let sec = i64::from_le_bytes(b[..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(b[8..16].try_into().unwrap());
    super::timespec(sec, nsec).ok_or(Errno::EINVAL)
}

pub(super) fn timerfd_gettime(cx: &mut Cx<'_>, fd: i32, cur: u64) -> SysResult {
    let timer = timer_of(cx, fd)?;
    let b = current(&mut timer.borrow_mut());
    put(cx, cur, b).map(|()| 0)
}

fn timer_of(cx: &Cx<'_>, fd: i32) -> Result<Rc<RefCell<TimerFd>>, Errno> {
    match &cx.fds().get(fd)?.borrow().kind {
        FileKind::TimerFd(t) => Ok(t.clone()),
        _ => Err(Errno::EINVAL),
    }
}

/// `struct itimerspec`: the interval and how long until the next expiration (both 0 when not
/// armed).
fn current(t: &mut TimerFd) -> [u8; 32] {
    let left = t.remaining();
    let mut b = [0u8; 32];
    for (o, d) in [(0, t.interval), (16, left)] {
        b[o..o + 8].copy_from_slice(&(d.as_secs() as i64).to_le_bytes());
        b[o + 8..o + 16].copy_from_slice(&(d.subsec_nanos() as i64).to_le_bytes());
    }
    b
}

/// Reads the number of expirations (8 bytes), waiting if none has happened yet; after a wall-clock
/// jump a `TFD_TIMER_CANCEL_ON_SET` timer reads `ECANCELED` once instead (and its count is
/// dropped).
pub(super) fn read(
    cx: &mut Cx<'_>,
    timer: &Rc<RefCell<TimerFd>>,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    if buf.len() < 8 {
        return Err(Errno::EINVAL);
    }
    loop {
        let deadline = {
            let mut t = timer.borrow_mut();
            t.update();
            if t.take_cancel() {
                t.expirations = 0;
                return Err(Errno::ECANCELED);
            }
            if t.expirations > 0 {
                buf[..8].copy_from_slice(&std::mem::take(&mut t.expirations).to_le_bytes());
                return Ok(8);
            }
            t.deadline
        };
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        let deadline = deadline.map(crate::itimer::Expiry::instant);
        cx.wait_objects(deadline, &mut [], wake::FILE, &[wake::of(timer)])?;
    }
}
