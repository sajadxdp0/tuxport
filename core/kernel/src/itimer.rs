//! Interval timers (setitimer, getitimer; musl's alarm also goes through setitimer) and POSIX
//! timers (the timer_create family).
//!
//! Deadlines are recorded in the thread group and handled by the Machine's timer thread
//! ([`expire`]): it waits until the earliest deadline, signals the processes whose timers expired,
//! and re-arms those with an interval. Setting a timer requests a wake-up, and the timer thread
//! then recomputes how long to wait.
//!
//! - `ITIMER_REAL` counts by the host's monotonic clock and sends SIGALRM on expiry.
//! - `ITIMER_VIRTUAL` and `ITIMER_PROF` count by the CPU time the process has used (the host
//!   thread CPU clock cannot tell user mode from kernel mode, so both count total CPU time), and
//!   send SIGVTALRM and SIGPROF on expiry. While such a timer exists the timer thread checks every
//!   10 ms, so the precision is 10 ms.
//! - `RLIMIT_CPU`: when CPU time exceeds the soft limit, SIGXCPU is sent and the soft limit is
//!   pushed back by one second (as on Linux, so once a second afterwards); exceeding the hard limit
//!   sends SIGKILL. Checked every 10 ms, like the CPU-time timers.
//! - POSIX timers count by a wall clock ([`Expiry`]) or by process or thread CPU time, and send an
//!   `SI_TIMER` signal on expiry (to the given thread with `SIGEV_THREAD_ID`). While the previous
//!   signal has not been taken yet no new one is sent; only the overrun count is recorded in that
//!   pending signal.
//!
//! Wall-clock deadlines ([`Expiry`]) sit on the base Linux keeps their hrtimer on: the monotonic
//! clock, or the realtime or boottime clock, which jump against it when the host's wall clock is
//! set and run on while the device sleeps. The timer thread watches the host for those jumps and
//! then wakes every thread ([`clock_jumped`]), and every wait looks at its deadline again.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::bug::Trap;
use crate::cell::RefCell;
use crate::kernel::{Kernel, Tid};
use crate::process::{RLIMIT_CPU, Signal, ThreadGroup};
use crate::signal::{self, InfoFields, Pending, SigInfo, code};

const CLOCK_REALTIME: i32 = 0;
const CLOCK_BOOTTIME: i32 = 7;
const CLOCK_REALTIME_ALARM: i32 = 8;
const CLOCK_BOOTTIME_ALARM: i32 = 9;
const CLOCK_TAI: i32 = 11;

/// When a wall-clock wait or timer expires, on the base Linux keeps its hrtimer on
/// (`__hrtimer_init`, `clockid_to_hrtimer_base`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Expiry {
    /// An instant of the host's monotonic clock: `CLOCK_MONOTONIC`, and relative `CLOCK_REALTIME`
    /// times, which Linux moves to the monotonic base.
    Mono(Instant),
    /// A time on a host clock that can jump against the monotonic one: `CLOCK_REALTIME` (absolute
    /// times), `CLOCK_BOOTTIME` (which goes on counting while the device sleeps) and `CLOCK_TAI`.
    /// It is compared with that clock whenever it is looked at.
    On(i32, Duration),
}

impl Expiry {
    /// `d` from now on the monotonic clock.
    pub fn after(d: Duration) -> Expiry {
        Expiry::Mono(Instant::now() + d)
    }

    /// A timer or sleep on host clock `clock` (already out of any time namespace) for `value`,
    /// absolute or relative to now. Alarm clocks count as their ordinary counterparts (nothing
    /// wakes the device here).
    pub fn new(clock: i32, value: Duration, abs: bool) -> Result<Expiry, crate::errno::Errno> {
        let base = match clock {
            CLOCK_REALTIME | CLOCK_REALTIME_ALARM if abs => CLOCK_REALTIME,
            CLOCK_REALTIME | CLOCK_REALTIME_ALARM => return Ok(Expiry::after(value)),
            CLOCK_BOOTTIME | CLOCK_BOOTTIME_ALARM => CLOCK_BOOTTIME,
            CLOCK_TAI => CLOCK_TAI,
            _ if !abs => return Ok(Expiry::after(value)),
            // CLOCK_MONOTONIC and anything else counting with it
            c => {
                let now = clock_now(c).ok_or(crate::errno::Errno::EINVAL)?;
                return Ok(Expiry::Mono(shift(Instant::now(), value, now)));
            }
        };
        let now = clock_now(base).ok_or(crate::errno::Errno::EINVAL)?;
        Ok(Expiry::On(base, if abs { value } else { now + value }))
    }

    /// The instant of the monotonic clock it falls on as the clocks stand now: what a wait uses as
    /// its timeout. A time already past becomes an instant in the past.
    pub fn instant(self) -> Instant {
        match self {
            Expiry::Mono(i) => i,
            Expiry::On(clock, at) => match clock_now(clock) {
                Some(now) => shift(Instant::now(), at, now),
                None => Instant::now(),
            },
        }
    }

    /// How far it has passed; None while it is still ahead.
    pub fn late(self) -> Option<Duration> {
        match self {
            Expiry::Mono(i) => Instant::now().checked_duration_since(i),
            Expiry::On(clock, at) => clock_now(clock).and_then(|now| now.checked_sub(at)),
        }
    }

    /// Whether it has passed.
    pub fn passed(self) -> bool {
        self.late().is_some()
    }

    /// How long until it; 0 once passed.
    pub fn remaining(self) -> Duration {
        match self {
            Expiry::Mono(i) => i.saturating_duration_since(Instant::now()),
            Expiry::On(clock, at) => {
                clock_now(clock).map_or(Duration::ZERO, |now| at.saturating_sub(now))
            }
        }
    }

    /// `d` later, on the same base.
    pub fn add(self, d: Duration) -> Expiry {
        match self {
            Expiry::Mono(i) => Expiry::Mono(i + d),
            Expiry::On(clock, at) => Expiry::On(clock, at + d),
        }
    }
}

/// The current time of host clock `clock`.
fn clock_now(clock: i32) -> Option<Duration> {
    let (sec, nsec) = tuxport_hal::clock_gettime(clock).ok()?;
    Some(Duration::new(sec.max(0) as u64, nsec as u32))
}

/// The instant `at` falls on when the clock reads `now` at instant `base`; a time too early for an
/// instant to represent becomes `base`.
fn shift(base: Instant, at: Duration, now: Duration) -> Instant {
    match at.checked_sub(now) {
        Some(ahead) => base + ahead,
        None => base.checked_sub(now - at).unwrap_or(base),
    }
}

/// Counts the jumps of the host's wall clock seen so far ([`clock_jumped`]); `TFD_TIMER_CANCEL_ON_SET`
/// timerfds compare it with the count when they were set. One count for the whole host process:
/// a jump is the same for every kernel in it.
static CLOCK_EPOCH: AtomicU64 = AtomicU64::new(0);

/// The number of wall-clock jumps seen so far.
pub(crate) fn clock_epoch() -> u64 {
    CLOCK_EPOCH.load(Ordering::SeqCst)
}

/// The host's wall clock was set, or the device resumed from sleep (the timer thread's
/// [`tuxport_hal::ClockWatch`] saw it): the realtime and boottime clocks have jumped against the
/// monotonic one. Every waiting thread is woken so that it turns its deadline into an instant
/// again ([`Expiry::instant`]), including futex waiters and sleepers without the BKL, which wait
/// on nothing but their own event; `TFD_TIMER_CANCEL_ON_SET` timerfds now read `ECANCELED`.
pub(crate) fn clock_jumped(k: &Kernel) {
    CLOCK_EPOCH.fetch_add(1, Ordering::SeqCst);
    for t in k.tasks.values() {
        t.event.signal();
    }
}

/// A process's `ITIMER_REAL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ITimer {
    /// When it next expires.
    pub deadline: Instant,
    /// The interval for re-arming after expiry; 0 means it fires only once.
    pub interval: Duration,
}

impl ITimer {
    /// The next state after expiring at `now`: missed periods are not made up (as on Linux, they
    /// count once).
    fn next(self, now: Instant) -> Option<ITimer> {
        if self.interval.is_zero() {
            return None;
        }
        let late = now.saturating_duration_since(self.deadline).as_nanos();
        let periods = (late / self.interval.as_nanos()) as u32 + 1;
        Some(ITimer {
            deadline: self.deadline + self.interval * periods,
            interval: self.interval,
        })
    }
}

/// A timer counting CPU time: the deadline and interval are both CPU nanoseconds used by the
/// process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CpuTimer {
    pub deadline: u64,
    pub interval: u64,
}

impl CpuTimer {
    /// The next state after expiring when the process has used `now` nanoseconds; missed periods
    /// are not made up.
    fn next(self, now: u64) -> Option<CpuTimer> {
        if self.interval == 0 {
            return None;
        }
        let periods = (now - self.deadline) / self.interval + 1;
        Some(CpuTimer {
            deadline: self.deadline + self.interval * periods,
            interval: self.interval,
        })
    }
}

/// How often CPU-time timers are checked.
const CPU_TICK: Duration = Duration::from_millis(10);

/// What a POSIX timer counts by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Clock {
    /// A wall clock (`CLOCK_REALTIME`, `MONOTONIC`, `BOOTTIME`, `TAI`), with deadlines as
    /// [`Deadline::At`] on the clock's base ([`Expiry`]). Carries the matching host clock id.
    Wall(i32),
    /// CPU time used by the process, with deadlines as [`Deadline::Cpu`]. The serial
    /// ([`ThreadGroup::serial`]) keeps it on the process it was made for, as Linux's timer holds
    /// that process's `struct pid`, not a later process reusing the number.
    Process { tgid: Tid, serial: u64 },
    /// CPU time used by the thread ([`crate::task::Task::serial`], likewise).
    Thread { tid: Tid, serial: u64 },
}

/// A CPU clock's thread or process as it is now ([`PosixTimer::target`]).
enum Target {
    /// A wall clock: the expiry compares itself.
    Wall,
    /// The clock's CPU nanoseconds, and whether they can still grow (false for a process whose
    /// threads have all exited and that is not reaped yet).
    Cpu(u64, bool),
    /// The thread has exited, or the process has been reaped (Linux's `cpu_timer_task_rcu`
    /// finds no task).
    Gone,
}

/// A POSIX timer's next expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Deadline {
    At(Expiry),
    /// CPU nanoseconds.
    Cpu(u64),
}

/// A POSIX timer (timer_create).
#[derive(Clone, Debug)]
pub(crate) struct PosixTimer {
    pub clock: Clock,
    /// The signal sent on expiry, and the target thread of `SIGEV_THREAD_ID`; None is `SIGEV_NONE`.
    pub signal: Option<(Signal, Option<Tid>)>,
    /// `sigev_value`, put into siginfo.
    pub value: u64,
    pub deadline: Option<Deadline>,
    pub interval: Duration,
    /// The overrun count accumulated by the most recently delivered signal (timer_getoverrun,
    /// updated when the signal is taken).
    pub overrun: u32,
}

impl PosixTimer {
    /// How long until the next expiry; None when not armed (or its clock's thread or process is
    /// gone).
    pub fn remaining(&self, k: &Kernel) -> Option<Duration> {
        match (self.deadline?, self.target(k)) {
            (Deadline::At(d), _) => Some(d.remaining()),
            (Deadline::Cpu(d), Target::Cpu(now, _)) => {
                Some(Duration::from_nanos(d.saturating_sub(now)))
            }
            (Deadline::Cpu(_), _) => None,
        }
    }

    /// The thread or process of its CPU clock has exited or been reaped (never one that reused
    /// the number since): as on Linux, the timer then never fires, reads as all zeros
    /// (`posix_cpu_timer_get`) and cannot be set (`ESRCH`, `posix_cpu_timer_set`), though it can
    /// still be deleted.
    pub fn gone(&self, k: &Kernel) -> bool {
        matches!(self.target(k), Target::Gone)
    }

    /// Expires `value` from now (with `abs`, `value` is an absolute CPU-clock value; for a wall
    /// clock the caller gives the expiry as `at`). The caller has checked that the clock is not
    /// [`gone`](Self::gone).
    pub fn arm(&mut self, k: &Kernel, value: Duration, at: Option<Expiry>, abs: bool) {
        self.deadline = match self.target(k) {
            Target::Wall => Some(Deadline::At(at.unwrap_or_else(|| Expiry::after(value)))),
            Target::Cpu(now, _) => {
                let v = value.as_nanos() as u64;
                Some(Deadline::Cpu(if abs { v } else { now + v }))
            }
            Target::Gone => None,
        };
    }

    /// If expired, returns the number of missed periods (not counting this one, capped at INT_MAX
    /// as on Linux) and re-arms. A CPU-clock timer whose clock no longer advances never fires (on
    /// Linux an exit takes the target's timers off its queues), and one whose target is gone is
    /// disarmed, so it no longer keeps the timer thread ticking.
    fn fire(&mut self, k: &Kernel) -> Option<u32> {
        let (late, deadline) = match (self.deadline?, self.target(k)) {
            (Deadline::At(d), _) => (d.late()?, Deadline::At(d)),
            (Deadline::Cpu(d), Target::Cpu(cpu, true)) if d <= cpu => {
                (Duration::from_nanos(cpu - d), Deadline::Cpu(d))
            }
            (Deadline::Cpu(_), Target::Gone) => {
                self.deadline = None;
                return None;
            }
            _ => return None,
        };
        if self.interval.is_zero() {
            self.deadline = None;
            return Some(0);
        }
        // With a short interval and a start long ago the period count exceeds 32 bits: advance by
        // the real period count
        let periods = late.as_nanos() / self.interval.as_nanos();
        let step_ns = (self.interval.as_nanos() * (periods + 1)).min(u64::MAX as u128) as u64;
        self.deadline = Some(match deadline {
            Deadline::At(d) => Deadline::At(d.add(Duration::from_nanos(step_ns))),
            Deadline::Cpu(d) => Deadline::Cpu(d.saturating_add(step_ns)),
        });
        Some(periods.min(i32::MAX as u128) as u32)
    }

    /// Whether its clock is a CPU clock that still advances, which the timer thread must look at
    /// every [`CPU_TICK`] while the timer is armed.
    fn ticking(&self, k: &Kernel) -> bool {
        matches!(self.target(k), Target::Cpu(_, true))
    }

    /// The clock's thread or process as it is now: the one the timer was made for (an exited
    /// thread leaves the task table, a reaped process the group table).
    fn target(&self, k: &Kernel) -> Target {
        match self.clock {
            Clock::Wall(_) => Target::Wall,
            Clock::Process { tgid, serial } => match k.groups.get(&tgid) {
                Some(g) if g.borrow().serial == serial => {
                    let g = g.borrow();
                    Target::Cpu(g.cpu_ns(k), !g.threads.is_empty())
                }
                _ => Target::Gone,
            },
            Clock::Thread { tid, serial } => match k.tasks.get(&tid) {
                Some(t) if t.serial == serial => Target::Cpu(t.cpu_ns(), true),
                _ => Target::Gone,
            },
        }
    }
}

/// Signals the processes whose timers expired and returns when to check next.
///
/// If handling a process hits a kernel-layer bug ([`crate::bug`]): that process is ended and its
/// timers cleared, and the other processes are handled as usual.
pub(crate) fn expire(k: &Kernel) -> Option<Instant> {
    let now = Instant::now();
    let mut next: Option<Instant> = None;
    for (&tgid, group) in &k.groups {
        match crate::bug::catch(|| expire_group(k, tgid, group, now)) {
            Some(Some(n)) => next = Some(next.map_or(n, |m| m.min(n))),
            Some(None) => {}
            None => {
                disarm(&mut group.borrow_mut());
                crate::bug::kill_process(k, tgid, format_args!("kernel panic in its timers"));
            }
        }
    }
    next
}

/// Handles the timers of process `tgid` and returns when it should next be checked.
fn expire_group(
    k: &Kernel,
    tgid: Tid,
    group: &RefCell<ThreadGroup>,
    now: Instant,
) -> Option<Instant> {
    crate::bug::trap(Trap::Timers, tgid);
    let mut next: Option<Instant> = None;
    let mut fired = Vec::new();
    let mut timer_fired = Vec::new();
    if group.borrow().threads.is_empty() {
        disarm(&mut group.borrow_mut());
        return None;
    }
    let cpu_limit = group.borrow().rlimits[RLIMIT_CPU as usize];
    let cpu_armed =
        group.borrow().cpu_itimers.iter().any(Option::is_some) || cpu_limit.0 != u64::MAX;
    let cpu = if cpu_armed {
        group.borrow().cpu_ns(k)
    } else {
        0
    };
    let mut g = group.borrow_mut();
    if let Some(t) = g.itimer {
        if t.deadline <= now {
            fired.push(Signal::ALRM);
            g.itimer = t.next(now);
        }
        if let Some(t) = g.itimer {
            next = Some(next.map_or(t.deadline, |n| n.min(t.deadline)));
        }
    }
    for (slot, sig) in [(0, Signal::VTALRM), (1, Signal::PROF)] {
        if let Some(t) = g.cpu_itimers[slot]
            && t.deadline <= cpu
        {
            fired.push(sig);
            g.cpu_itimers[slot] = t.next(cpu);
        }
    }
    if cpu_limit.0 != u64::MAX {
        let secs = cpu / 1_000_000_000;
        if secs >= cpu_limit.1 {
            fired.push(Signal::KILL);
        } else if secs >= cpu_limit.0 {
            fired.push(Signal::XCPU);
            g.rlimits[RLIMIT_CPU as usize].0 = cpu_limit.0 + 1;
        }
    }
    if cpu_armed {
        let tick = now + CPU_TICK;
        next = Some(next.map_or(tick, |n| n.min(tick)));
    }
    let mut timers = std::mem::take(&mut g.posix_timers);
    drop(g);
    for (&id, t) in timers.iter_mut() {
        if let Some(missed) = t.fire(k)
            && let Some((sig, tid)) = t.signal
        {
            let info = SigInfo {
                code: code::SI_TIMER,
                fields: InfoFields::Timer {
                    id,
                    overrun: missed,
                    value: t.value,
                },
                ..SigInfo::default()
            };
            if !add_overrun(k, tgid, tid, sig, id, missed) {
                let cpu = matches!(t.clock, Clock::Process { .. });
                timer_fired.push((tid, sig, info, cpu));
            }
        }
        match t.deadline {
            Some(Deadline::At(d)) => {
                let d = d.instant();
                next = Some(next.map_or(d, |n| n.min(d)));
            }
            Some(Deadline::Cpu(_)) if t.ticking(k) => {
                let tick = now + CPU_TICK;
                next = Some(next.map_or(tick, |n| n.min(tick)));
            }
            _ => {}
        }
    }
    group.borrow_mut().posix_timers = timers;
    for sig in fired {
        if sig == Signal::VTALRM || sig == Signal::PROF {
            signal::send_cpu_timer(k, tgid, sig, SigInfo::kernel());
        } else {
            signal::send_to_group(k, tgid, sig, SigInfo::kernel());
        }
    }
    for (tid, sig, info, cpu) in timer_fired {
        match tid {
            // SIGEV_THREAD_ID: to that thread while it lives in this process; once it has exited
            // the signal is dropped, as Linux's posix_timer_event finds no task (never the whole
            // process, nor a thread that took over its number)
            Some(t) => {
                if let Some(task) = k.tasks.get(&t).filter(|task| task.tgid == tgid) {
                    signal::send_to_task(k, task, sig, info);
                }
            }
            None if cpu => signal::send_cpu_timer(k, tgid, sig, info),
            None => signal::send_to_group(k, tgid, sig, info),
        }
    }
    next
}

/// The last signal sent by timer `id` is still pending: adds the overrun count to that signal's
/// siginfo and returns true.
fn add_overrun(k: &Kernel, tgid: Tid, tid: Option<Tid>, sig: Signal, id: i32, missed: u32) -> bool {
    let bump = |pending: &mut Pending| {
        let info = pending.infos_mut(sig).find(|info| {
            info.code == code::SI_TIMER
                && matches!(info.fields, InfoFields::Timer { id: i, .. } if i == id)
        });
        match info.map(|info| &mut info.fields) {
            Some(InfoFields::Timer { overrun, .. }) => {
                *overrun = overrun.saturating_add(missed + 1).min(i32::MAX as u32);
                true
            }
            _ => false,
        }
    };
    match tid.and_then(|t| k.tasks.get(&t)) {
        Some(task) => bump(&mut task.pending.borrow_mut()),
        None => k
            .groups
            .get(&tgid)
            .is_some_and(|g| bump(&mut g.borrow_mut().pending)),
    }
}

/// Stops all of a process's timers.
fn disarm(g: &mut ThreadGroup) {
    g.itimer = None;
    g.cpu_itimers = [None; 2];
    g.posix_timers.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periodic_timers_skip_missed_periods() {
        let start = Instant::now();
        let ms = Duration::from_millis;
        let t = ITimer {
            deadline: start,
            interval: ms(10),
        };
        assert_eq!(t.next(start).unwrap().deadline, start + ms(10));
        assert_eq!(t.next(start + ms(35)).unwrap().deadline, start + ms(40));
        let once = ITimer {
            deadline: start,
            interval: Duration::ZERO,
        };
        assert_eq!(once.next(start), None);
    }
}
