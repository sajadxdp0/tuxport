//! Load average (`/proc/loadavg`, sysinfo): as in Linux's `kernel/sched/loadavg.c`, the number of
//! runnable tasks (guest threads not waiting in the kernel; those waiting for the BKL count too,
//! like Linux's R plus D) is sampled every 5 seconds and averaged with exponential decay over 1, 5
//! and 15 minutes.
//!
//! The timer thread does the sampling. It stops while every thread is waiting, so it does not wake
//! every 5 seconds for this; a thread about to go on running guest code wakes the timer thread
//! ([`Shared::resume_checks`]), and the samples missed while stopped are filled in as 0, as Linux's
//! NO_HZ does on an idle CPU.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::kernel::{Kernel, Shared};

/// The sampling interval (Linux's `LOAD_FREQ` is 5 seconds plus one tick).
const LOAD_FREQ: Duration = Duration::from_secs(5);
/// Fraction bits of the fixed-point numbers (`FSHIFT`).
const FSHIFT: u32 = 11;
const FIXED_1: u64 = 1 << FSHIFT;
/// Decay factors for 1, 5 and 15 minutes: `FIXED_1 / exp(5 s / period)`.
const EXP: [u64; 3] = [1884, 2014, 2037];
/// Fraction bits of sysinfo's `loads` (`SI_LOAD_SHIFT`).
const SI_LOAD_SHIFT: u32 = 16;

/// Load-average state, in [`Kernel`].
pub(crate) struct LoadAvg {
    /// Averages over 1, 5 and 15 minutes (fixed-point).
    avenrun: [u64; 3],
    /// When the next sample is due.
    due: Instant,
    /// Stopped: every thread was waiting at the last sample, and no next sample is scheduled.
    stopped: bool,
}

impl LoadAvg {
    pub fn new(now: Instant) -> LoadAvg {
        LoadAvg {
            avenrun: [0; 3],
            due: now + LOAD_FREQ,
            stopped: false,
        }
    }

    /// Called when the timer thread wakes, with `running` threads now runnable: samples if a sample
    /// is due. Returns when the timer thread should next be woken, or None when stopped.
    fn update(&mut self, now: Instant, running: u64) -> Option<Instant> {
        if now < self.due {
            // A thread ran again after the sampler stopped (it woke the timer thread)
            if self.stopped && running > 0 {
                self.stopped = false;
            }
        } else {
            let n = self.missed(now);
            // Every thread was waiting while stopped
            let active = if self.stopped { 0 } else { running };
            self.avenrun = fold(self.avenrun, active, n);
            self.due += LOAD_FREQ * n as u32;
            self.stopped = running == 0;
        }
        (!self.stopped).then_some(self.due)
    }

    /// The current averages. Samples that the timer thread has not yet filled in since stopping
    /// (the reading thread has just woken) are filled in here.
    fn current(&self, now: Instant) -> [u64; 3] {
        if self.stopped && now >= self.due {
            fold(self.avenrun, 0, self.missed(now))
        } else {
            self.avenrun
        }
    }

    /// The number of samples missed up to `now` (`now` is not earlier than `due`).
    fn missed(&self, now: Instant) -> u64 {
        let behind = (now - self.due).as_nanos() / LOAD_FREQ.as_nanos();
        1 + u64::try_from(behind)
            .unwrap_or(u64::MAX)
            .min(u32::MAX as u64 - 1)
    }
}

/// The averages after `n` samples of `active` tasks each (Linux's `calc_load_n`).
fn fold(avenrun: [u64; 3], active: u64, n: u64) -> [u64; 3] {
    let active = active.saturating_mul(FIXED_1);
    let mut out = avenrun;
    for (avg, exp) in out.iter_mut().zip(EXP) {
        *avg = calc_load(*avg, fixed_power(exp, n), active);
    }
    out
}

/// Linux's `calc_load`: `load * exp + active * (1 - exp)`, rounded towards active.
fn calc_load(load: u64, exp: u64, active: u64) -> u64 {
    let mut new = load * exp + active * (FIXED_1 - exp);
    if active >= load {
        new += FIXED_1 - 1;
    }
    new / FIXED_1
}

/// Fixed-point `x` to the power `n` (Linux's `fixed_power_int`).
fn fixed_power(mut x: u64, mut n: u64) -> u64 {
    let mut result = FIXED_1;
    while n != 0 {
        if n & 1 != 0 {
            result = (result * x + FIXED_1 / 2) >> FSHIFT;
        }
        n >>= 1;
        x = (x * x + FIXED_1 / 2) >> FSHIFT;
    }
    result
}

/// Called every time the timer thread wakes. Returns when the next sample is due; when stopped,
/// returns None and makes the next thread about to run guest code wake the timer thread.
pub(crate) fn sample(k: &mut Kernel, shared: &Shared) -> Option<Instant> {
    let now = Instant::now();
    // The timer thread also wakes for other work: no need to count threads before a sample is due
    if !k.load.stopped && now < k.load.due {
        return Some(k.load.due);
    }
    let running = running(k);
    let next = k.load.update(now, running);
    // A thread that woke since it was counted keeps the sampler going
    if next.is_none() && !shared.stop_checks(|| self::running(k) == 0) {
        k.load.stopped = false;
        return Some(k.load.due);
    }
    next
}

/// The contents of `/proc/loadavg`.
pub(crate) fn proc_loadavg(k: &Kernel, last_pid: crate::kernel::Tid) -> String {
    // As on Linux, add 0.005 and then truncate to two decimal places
    let avg = k.load.current(Instant::now()).map(|a| a + FIXED_1 / 200);
    let int = |a: u64| a >> FSHIFT;
    let frac = |a: u64| ((a & (FIXED_1 - 1)) * 100) >> FSHIFT;
    format!(
        "{}.{:02} {}.{:02} {}.{:02} {}/{} {}\n",
        int(avg[0]),
        frac(avg[0]),
        int(avg[1]),
        frac(avg[1]),
        int(avg[2]),
        frac(avg[2]),
        running(k).max(1),
        k.tasks.len(),
        last_pid,
    )
}

/// sysinfo's `loads`.
pub(crate) fn sysinfo_loads(k: &Kernel) -> [u64; 3] {
    k.load
        .current(Instant::now())
        .map(|a| a << (SI_LOAD_SHIFT - FSHIFT))
}

/// The number of runnable threads: guest threads not waiting in the kernel.
pub(crate) fn running(k: &Kernel) -> u64 {
    k.tasks
        .values()
        .filter(|t| !t.sleeping.load(Ordering::SeqCst))
        .count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenths(a: u64) -> u64 {
        (a * 100) >> FSHIFT
    }

    #[test]
    fn samples_every_five_seconds() {
        let t0 = Instant::now();
        let mut l = LoadAvg::new(t0);
        // No sample due yet
        assert_eq!(
            l.update(t0 + Duration::from_secs(1), 2),
            Some(t0 + LOAD_FREQ)
        );
        assert_eq!(l.avenrun, [0; 3]);
        // One task ran for a minute: the 1-minute average is about 1 - 1/e
        let mut now = t0;
        for _ in 0..12 {
            now += LOAD_FREQ;
            assert!(l.update(now, 1).is_some());
        }
        assert_eq!(tenths(l.avenrun[0]), 63);
        assert!(l.avenrun[1] < l.avenrun[0] && l.avenrun[2] < l.avenrun[1]);
    }

    #[test]
    fn stopped_samples_count_as_idle() {
        let t0 = Instant::now();
        let mut l = LoadAvg::new(t0);
        let mut now = t0;
        for _ in 0..24 {
            now += LOAD_FREQ;
            l.update(now, 4);
        }
        let busy = l.avenrun;
        // Every thread is waiting: the sampler stops
        now += LOAD_FREQ;
        assert_eq!(l.update(now, 0), None);
        let first_idle = l.avenrun;
        assert!(first_idle[0] < busy[0]);
        // A thread wakes after a minute: the 12 missed samples are filled in as 0
        now += Duration::from_secs(60) + Duration::from_millis(1);
        assert_eq!(l.current(now), fold(first_idle, 0, 12));
        assert!(l.update(now, 1).is_some());
        assert_eq!(l.avenrun, fold(first_idle, 0, 12));
        // The same as sampling one by one: each squaring in the power rounds, so they may differ
        // by a few thousandths
        let mut step = first_idle;
        for _ in 0..12 {
            step = fold(step, 0, 1);
        }
        for i in 0..3 {
            assert!(
                l.avenrun[i].abs_diff(step[i]) <= 16,
                "{:?} {:?}",
                l.avenrun,
                step
            );
        }
        // The next sample proceeds as usual, with the thread count at that time
        let due = l.due;
        assert!(l.update(due, 1).is_some());
        assert_eq!(l.avenrun, fold(fold(first_idle, 0, 12), 1, 1));
    }

    #[test]
    fn stopped_sampler_restarts_before_the_next_sample() {
        let t0 = Instant::now();
        let mut l = LoadAvg::new(t0);
        assert_eq!(l.update(t0 + LOAD_FREQ, 0), None);
        // A thread ran again after the sampler stopped and before the next sample
        let next = t0 + LOAD_FREQ * 2;
        assert_eq!(
            l.update(t0 + LOAD_FREQ + Duration::from_secs(1), 1),
            Some(next)
        );
        l.update(next, 1);
        assert!(l.avenrun[0] > 0);
    }
}
