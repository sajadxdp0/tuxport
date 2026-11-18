//! The BKL watchdog (the kernel/process map): a host thread that never takes the BKL and reports
//! a hold of it that lasts too long. While one thread holds the BKL, every other guest thread
//! stops at its next kernel entry, so a hold that does not end (a loop in kernel code, a host call
//! that blocks with the lock held) freezes every guest process and the app's tabs, and the app's
//! own calls into the kernel wait with them. Linux's soft-lockup detector reports a CPU stuck in
//! the kernel the same way, and Starnix's lockup detector names the threads stuck in one syscall.
//!
//! Each holder records itself in [`Hold`](crate::kernel::Hold) when it takes the lock, without
//! reading a clock. The watchdog looks at it every fifth of the threshold; once it has seen the
//! same hold for the threshold (10 s; `TUXPORT_BKL_WATCHDOG` sets it in seconds, and 0 turns the
//! watchdog off) it reports:
//!
//! - at once, without the BKL: one line on the host's stderr (logcat on Android) naming the guest
//!   process and thread, what it entered the kernel for and its pc, or else the host thread, and
//!   what the holder's host thread is doing (running, or in which host syscall); the line is also
//!   kept for the host ([`crate::Machine::stall`]), and the app shows it in a notification with
//!   the bug report;
//! - when the hold ends: a KERN_ERR record in the kernel log naming the process and how long the
//!   hold lasted, also kept for the host. Writing the record takes the BKL, so a short-lived
//!   thread writes it: the next holder may keep the BKL just as long, and the watchdog must go on
//!   looking meanwhile.
//!
//! Durations count from the first look that saw the hold, so they are right to within one look
//! (2 s by default). Nothing is ended: a hold cannot be interrupted, and one that ends leaves the
//! kernel as consistent as any other.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::kernel::{HoldSample, Shared};

/// How long one hold of the BKL may last before it is reported.
const THRESHOLD: Duration = Duration::from_secs(10);

/// A hold of the BKL that lasted longer than the watchdog's threshold ([`crate::Machine::stall`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stall {
    /// Counts this kernel's stalls from 1, telling a new one from the last.
    pub id: u64,
    /// How long the hold has lasted so far, or in all once it ended, in whole seconds.
    pub secs: u64,
    /// The report made when the hold reached the threshold, the same as the line on stderr.
    pub report: String,
    /// Once the hold has ended: its record in the kernel log, with the process's name and how long
    /// the hold lasted.
    pub ended: Option<String>,
}

/// The running watchdog of one kernel.
pub(crate) struct Watchdog {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Watchdog {
    /// Starts watching `shared`'s BKL; None when the watchdog is turned off or its thread cannot
    /// start (the kernel runs without it).
    pub fn start(shared: &Arc<Shared>) -> Option<Watchdog> {
        let threshold = threshold()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_shared, thread_stop) = (shared.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("bkl-watchdog".into())
            .spawn(move || watch(&thread_shared, &thread_stop, threshold))
            .ok()?;
        Some(Watchdog { stop, thread })
    }

    /// Stops the thread and waits for it.
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.thread().unpark();
        let _ = self.thread.join();
    }
}

/// [`THRESHOLD`], or the seconds in `TUXPORT_BKL_WATCHDOG`; None when that is 0.
fn threshold() -> Option<Duration> {
    match std::env::var("TUXPORT_BKL_WATCHDOG").map(|v| v.trim().parse::<u64>()) {
        Ok(Ok(0)) => None,
        Ok(Ok(secs)) => Some(Duration::from_secs(secs)),
        _ => Some(THRESHOLD),
    }
}

/// The hold the watchdog is following.
struct Seen {
    sample: HoldSample,
    /// The first look that saw it.
    since: Instant,
    /// Once reported: the [`Stall::id`], and the holder's host thread name.
    reported: Option<(u64, String)>,
}

/// The watchdog's thread, until `stop` is set.
fn watch(shared: &Arc<Shared>, stop: &AtomicBool, threshold: Duration) {
    let look = threshold / 5;
    let mut seen: Option<Seen> = None;
    loop {
        // Waking early (unpark, or spuriously) only means one more look
        std::thread::park_timeout(look);
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let now = Instant::now();
        let sample = shared.hold().sample();
        if let Some(s) = &mut seen
            && sample == Some(s.sample)
        {
            let held = now - s.since;
            match &s.reported {
                Some((id, _)) => set(shared, *id, |stall| stall.secs = held.as_secs()),
                None if held >= threshold => s.reported = Some(report(shared, &s.sample, held)),
                None => {}
            }
            continue;
        }
        if let Some(s) = seen.take()
            && let Some((id, host)) = s.reported
        {
            ended(shared, id, &s.sample, &host, now - s.since);
        }
        seen = sample.map(|sample| Seen {
            sample,
            since: now,
            reported: None,
        });
    }
}

/// Reports a hold that has lasted `held`, without the BKL: returns its [`Stall::id`] and the
/// holder's host thread name.
fn report(shared: &Shared, s: &HoldSample, held: Duration) -> (u64, String) {
    let (name, activity) = tuxport_hal::thread_activity(s.host_tid)
        .unwrap_or_else(|| (String::from("?"), String::from("gone")));
    let secs = held.as_secs();
    let report = match holder(s) {
        Some(guest) => format!(
            "tuxport: {guest} has held the BKL for {secs} s {} at guest pc {:#x} (host thread \
             {name}, tid {}, {activity})",
            s.entry.doing, s.entry.pc, s.host_tid,
        ),
        None => format!(
            "tuxport: host thread {name} (tid {}, {activity}) has held the BKL for {secs} s",
            s.host_tid
        ),
    };
    eprintln!("{report}");
    let mut stall = shared.stall.lock().unwrap_or_else(PoisonError::into_inner);
    let id = stall.as_ref().map_or(1, |s| s.id + 1);
    *stall = Some(Stall {
        id,
        secs,
        report,
        ended: None,
    });
    (id, name)
}

/// The reported hold has ended after `held`: tells stderr and the host at once, and starts a
/// thread that records it in the kernel log once it gets the BKL.
fn ended(shared: &Arc<Shared>, id: u64, s: &HoldSample, host: &str, held: Duration) {
    let secs = held.as_secs();
    let line = match holder(s) {
        Some(guest) => format!(
            "tuxport: {guest} held the BKL for {secs} s {} at guest pc {:#x}",
            s.entry.doing, s.entry.pc,
        ),
        None => format!("tuxport: host thread {host} held the BKL for {secs} s"),
    };
    eprintln!("{line}");
    set(shared, id, |stall| {
        stall.secs = secs;
        stall.ended = Some(line.clone());
    });
    // When the thread cannot start, only the kernel log goes without the record
    let shared = shared.clone();
    let _ = std::thread::Builder::new()
        .name("bkl-watchdog-log".into())
        .spawn(move || {
            let k = shared.lock();
            let logged = crate::bug::catch(|| {
                crate::klog::printk(&k.klog, crate::klog::level::ERR, format_args!("{line}"));
            });
            drop(k);
            if logged.is_none() {
                crate::bug::report(format_args!("kernel panic in the BKL watchdog"));
            }
        });
}

/// The guest thread holding the BKL, as `pid 120 (git), thread 121`; None when the holder runs
/// no guest thread.
fn holder(s: &HoldSample) -> Option<String> {
    let e = &s.entry;
    if e.tid == 0 {
        return None;
    }
    let n = e.comm.iter().position(|&b| b == 0).unwrap_or(e.comm.len());
    let comm = String::from_utf8_lossy(&e.comm[..n]);
    Some(match (e.tgid, comm.is_empty()) {
        // Still starting: its process is not known yet
        (0, _) => format!("thread {}", e.tid),
        (tgid, true) => format!("pid {tgid}, thread {}", e.tid),
        (tgid, false) => format!("pid {tgid} ({comm}), thread {}", e.tid),
    })
}

/// Changes stall `id` if it is still the latest.
fn set(shared: &Shared, id: u64, f: impl FnOnce(&mut Stall)) {
    let mut stall = shared.stall.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(stall) = stall.as_mut().filter(|s| s.id == id) {
        f(stall);
    }
}
