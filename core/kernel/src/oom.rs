//! The guest's own OOM killer (the kernel/oom map, design doc §12 item 4).
//!
//! Android's LMK sees only the app process: under memory pressure it kills the whole app, and all
//! sessions are lost together. So the app process's anonymous memory gets a limit (by default half
//! of physical memory), and when it is exceeded a guest process is picked and ended with SIGKILL,
//! as Linux's OOM killer does. What is measured is the anonymous memory of the whole host process
//! ([`app_memory`]): that is what the LMK looks at, and besides the guest's private and shared
//! memory it includes the translation cache and metadata cache the kernel layer spends on the
//! guest; the host's file pages (files in the store, mapped programs) can be reclaimed and do not
//! count.
//!
//! The host kernel handles the guest touching new pages directly, and the kernel layer does not
//! see those page faults, so it can only check periodically: while some guest thread is not waiting
//! in the kernel, the timer thread measures every 20 milliseconds (one read of `/proc/self/status`
//! takes about 2 microseconds), and every 5 milliseconds once usage passes 3/4 of the limit; when
//! all are waiting, memory cannot grow any more, so it stops, and is woken again when some thread
//! is about to go on running guest code ([`Shared::resume_checks`]). A program allocating memory
//! nonstop can use tens of MB more before it is noticed (under Tier 1 it can touch several GB of
//! new pages per second), so the limit must leave headroom. After ending a process, it waits until
//! its threads have all exited and its memory has gone back to the host before deciding whether to
//! end the next one.
//!
//! cgroups with `memory.max` set ([`crate::cgroup`]) are checked along with it: the resident memory
//! of the processes in the subtree is measured (mincore: a process of tens of MB takes one or two
//! hundred microseconds, and one with many mappings like node at 200 MB takes over a millisecond,
//! so only cgroups with a limit are measured, and the interval is at least 20 times the time
//! taken), and when it is over, as with Linux's memcg OOM, a process below the cgroup is picked and
//! ended, together with that group under `memory.oom.group`.

use std::cell::Cell;
use std::rc::{Rc, Weak};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tuxport_engine::PAGE_SIZE;

use crate::cell::RefCell;
use crate::cgroup::ROOT;
use crate::kernel::{INIT, Kernel, Shared};
use crate::process::{ExitStatus, Signal, ThreadGroup};

/// Interval between checks while some thread is running.
const INTERVAL: Duration = Duration::from_millis(20);
/// Interval between checks once usage passes 3/4 of the limit.
const NEAR_INTERVAL: Duration = Duration::from_millis(5);
/// The check interval is at least this many times the time one check takes (measuring a cgroup's
/// memory needs mincore; see [`check`]).
const MEASURE_SHARE: u32 = 20;
/// When an ended process has not finished exiting after this long (stuck in host I/O), it is no
/// longer waited for; about the time Linux's oom_reaper takes to give up on a process.
const VICTIM_TIMEOUT: Duration = Duration::from_secs(1);
/// The lower bound of `oom_score_adj`: such processes are never picked.
const OOM_SCORE_ADJ_MIN: i32 = -1000;
/// How often the pages the code cache has written in the writable view are torn down
/// ([`unmap_code_views`]).
const CODE_VIEW_INTERVAL: Duration = Duration::from_secs(1);

/// The OOM killer's state, in [`Kernel`].
pub(crate) struct Oom {
    /// The limit (bytes); `u64::MAX` means no limit.
    pub limit: u64,
    /// The highest usage measured (`/sys/fs/cgroup/memory.peak`).
    pub peak: Cell<u64>,
    /// When the next check is due; None means all threads are waiting and checks have stopped.
    next: Option<Instant>,
    /// The process ended most recently, and when it was ended.
    victim: Option<(Weak<RefCell<ThreadGroup>>, Instant)>,
    /// Victims still not done exiting after [`VICTIM_TIMEOUT`] (stuck in host I/O): never waited
    /// for or picked again.
    stuck: Vec<Weak<RefCell<ThreadGroup>>>,
    /// When the code cache's writable view is next torn down; None means all threads are waiting
    /// and it has stopped.
    code_views: Option<Instant>,
}

impl Default for Oom {
    fn default() -> Oom {
        Oom {
            limit: u64::MAX,
            peak: Cell::new(0),
            next: None,
            victim: None,
            stuck: Vec::new(),
            code_views: None,
        }
    }
}

impl Oom {
    /// The app's current memory usage (0 when it cannot be measured), noting the peak on the way.
    pub fn measure(&self) -> u64 {
        let used = footprint().unwrap_or(0);
        self.note(used);
        used
    }

    fn note(&self, used: u64) {
        self.peak.set(self.peak.get().max(used));
    }

    /// A cgroup's `memory.max` changed: check at the next wakeup (the new limit may already be
    /// exceeded).
    pub fn recheck(&mut self) {
        self.next = None;
    }
}

/// The counts in `memory.events`: how often usage was measured over the limit and so a process
/// had to be picked and ended (not counted while the one picked last time has not finished
/// exiting), how many processes were ended, and how often a whole group was ended because of
/// `memory.oom.group`. Recorded on the cgroup ([`crate::cgroup::memory_event`]); those for the
/// app's limit are recorded on the root.
#[derive(Clone, Copy, Default)]
pub(crate) struct Events {
    pub max: u64,
    pub oom: u64,
    pub oom_kill: u64,
    pub oom_group_kill: u64,
}

/// The default limit: half of the host's physical memory.
pub(crate) fn default_limit() -> u64 {
    match tuxport_hal::memory_info().0 {
        0 => u64::MAX,
        total => total / 2,
    }
}

/// Called each time the timer thread wakes up: when a check is due, measures the app's memory and
/// the memory of the cgroups with `memory.max` set, and when a limit is exceeded ends a guest
/// process (for a cgroup, picked from inside it). Returns when the next check is due.
pub(crate) fn check(k: &mut Kernel, shared: &Shared) -> Option<Instant> {
    let limits = crate::cgroup::memory_limits(k);
    if k.oom.limit == u64::MAX && limits.is_empty() {
        return None;
    }
    let now = Instant::now();
    if let Some(next) = k.oom.next
        && now < next
    {
        return Some(next);
    }
    if shared.stop_checks(|| all_sleeping(k)) {
        k.oom.next = None;
        return None;
    }
    // The scope that is over (the root being the whole app), usage, limit
    let mut over = None;
    let mut near = false;
    if k.oom.limit != u64::MAX {
        match footprint() {
            Some(used) => {
                k.oom.note(used);
                near = used > k.oom.limit / 4 * 3;
                if used > k.oom.limit {
                    over = Some((ROOT, used, k.oom.limit));
                }
            }
            None => {
                eprintln!("tuxport: cannot measure the memory use, the memory limit is off");
                k.oom.limit = u64::MAX;
            }
        }
    }
    for (id, max) in limits {
        if over.is_some() {
            break;
        }
        let used = crate::cgroup::measure_memory(k, id);
        near |= used > max / 4 * 3;
        if used > max {
            over = Some((id, used, max));
        }
    }
    // Measuring cgroups holds the BKL, and takes a millisecond or two with many processes and large
    // mappings: the interval is at least MEASURE_SHARE times this check's duration, taking no more
    // than 1/MEASURE_SHARE of the BKL's time
    let interval = if near { NEAR_INTERVAL } else { INTERVAL };
    k.oom.next = Some(now + interval.max(now.elapsed() * MEASURE_SHARE));
    // The memory of a process that just exited goes back to the host only after it releases the
    // BKL (mm::release_unmapped)
    let Some((domain, used, limit)) = over else {
        return k.oom.next;
    };
    // The process picked last time is still exiting: its memory will be back soon, so this does
    // not count as hitting the limit again
    if crate::mm::UNMAPPING.load(Ordering::Acquire) != 0 || waiting_for_victim(k, now) {
        return k.oom.next;
    }
    crate::cgroup::memory_event(k, domain, |e| e.max += 1);
    out_of_memory(k, (domain, used, limit));
    k.oom.next
}

/// The app's memory usage (bytes): the sum of the items of [`app_memory`]. Returns None when it
/// cannot be read.
fn footprint() -> Option<u64> {
    app_memory().map(|u| u.anon + u.shmem + u.swap)
}

/// The app's memory usage, item by item ([`tuxport_hal::memory_usage`]). Pages Tier 1's code cache
/// has written in the writable view and that are not torn down yet ([`unmap_code_views`]) count
/// twice in the host's RssShmem, so one count is subtracted.
pub(crate) fn app_memory() -> Option<tuxport_hal::MemoryUsage> {
    let mut u = tuxport_hal::memory_usage()?;
    u.shmem = u.shmem.saturating_sub(tuxport_engine::written_code_bytes());
    Some(u)
}

/// Usage `used` of OOM scope `domain` (a cgroup, the root being the whole app) exceeds limit
/// `limit`: picks a process below it and ends it (Linux's `out_of_memory`, `oom_kill_process`),
/// together with that group under `memory.oom.group`. While the process picked last time has not
/// finished exiting it waits for it (its memory will be back soon); one still not done after the
/// timeout (stuck in host I/O) is no longer waited for, nor picked again, however many others are
/// picked after it.
pub(crate) fn out_of_memory(k: &mut Kernel, (domain, used, limit): (u64, u64, u64)) {
    let now = Instant::now();
    if waiting_for_victim(k, now) {
        return;
    }
    let alive = |w: &Weak<RefCell<ThreadGroup>>| {
        w.upgrade().is_some_and(|g| !g.borrow().threads.is_empty())
    };
    if let Some((v, _)) = k.oom.victim.take()
        && alive(&v)
    {
        k.oom.stuck.push(v);
    }
    k.oom.stuck.retain(alive);
    let stuck: Vec<_> = k.oom.stuck.iter().filter_map(Weak::upgrade).collect();
    let Some((victim, rss)) = select(k, &stuck, (domain, limit)) else {
        return;
    };
    crate::cgroup::memory_event(k, domain, |e| e.oom += 1);
    k.oom.victim = Some((Rc::downgrade(&victim), now));
    // One already exiting (exit_group, a fatal signal) need not be ended; as on Linux, wait for it
    // to finish exiting
    if victim.borrow().exiting.is_some() {
        return;
    }
    {
        let g = victim.borrow();
        let task = g.threads.first().and_then(|t| k.tasks.get(t));
        eprintln!(
            "tuxport: out of memory: killed process {} ({}) rss:{}kB oom_score_adj:{}; \
             {} memory {}kB, limit {}kB",
            g.tgid,
            crate::pseudo::comm_str(task.map(|t| &**t), g.tgid),
            rss * PAGE_SIZE / 1024,
            g.oom_score_adj,
            if domain == ROOT {
                "App".to_owned()
            } else {
                format!("cgroup {}", k.cgroups.path(domain, ROOT))
            },
            used / 1024,
            limit / 1024,
        );
        log_kill(k, &g, task.map(|t| &**t), (domain, used, limit));
        let comm = crate::pseudo::comm_str(task.map(|t| &**t), g.tgid);
        crate::bug::record_kill(k, g.tgid, comm, oom_reason(k, domain, used, limit));
    }
    let group = crate::cgroup::oom_group(k, victim.borrow().cgroup, domain);
    kill(k, &victim);
    // memory.oom.group: the processes in that cgroup are ended along with it (Linux's
    // mem_cgroup_scan_tasks, counting the picked one again too), except those whose oom_score_adj
    // is the lower bound
    if let Some(group) = group {
        crate::cgroup::memory_event(k, group, |e| e.oom_group_kill += 1);
        let members: Vec<_> = k
            .groups
            .values()
            .filter(|g| {
                let g = g.borrow();
                g.tgid != INIT
                    && !g.threads.is_empty()
                    && g.oom_score_adj != OOM_SCORE_ADJ_MIN
                    && k.cgroups.is_within(g.cgroup, group)
            })
            .cloned()
            .collect();
        for g in members {
            {
                let gb = g.borrow();
                let task = gb.threads.first().and_then(|t| k.tasks.get(t));
                log_killed(k, &gb, task.map(|t| &**t));
                // The picked one was already reported above
                if !Rc::ptr_eq(&g, &victim) {
                    let comm = crate::pseudo::comm_str(task.map(|t| &**t), gb.tgid);
                    let reason = oom_reason(k, domain, used, limit);
                    crate::bug::record_kill(k, gb.tgid, comm, reason);
                }
            }
            kill(k, &g);
        }
    }
}

/// Picks the process to end (skipping those in `skip`): the OOM scope is cgroup `domain` (the root
/// being the whole app), and the pick is among the processes below it, by Linux 6.1's oom_badness:
/// resident pages plus `oom_score_adj` times a thousandth of the limit `limit` in pages (for the
/// root, the larger of physical memory and the limit; 1000 equals everything, so such a process is
/// always picked first), unclamped; on equal scores the later process wins, as
/// `oom_evaluate_task` keeps the last one not below the best. Returns the process and its resident
/// 4K page count.
fn select(
    k: &Kernel,
    skip: &[Rc<RefCell<ThreadGroup>>],
    (domain, limit): (u64, u64),
) -> Option<(Rc<RefCell<ThreadGroup>>, u64)> {
    let total = if domain == ROOT {
        tuxport_hal::memory_info().0.max(limit)
    } else {
        limit
    };
    let total = (total / PAGE_SIZE) as i64;
    let mut best: Option<(Rc<RefCell<ThreadGroup>>, i64, u64)> = None;
    for group in k.groups.values() {
        if skip.iter().any(|s| Rc::ptr_eq(s, group)) {
            continue;
        }
        let g = group.borrow();
        if !k.cgroups.is_within(g.cgroup, domain) {
            continue;
        }
        let Some((points, rss)) = badness(k, &g, total) else {
            continue;
        };
        if best.as_ref().is_none_or(|b| points >= b.1) {
            best = Some((group.clone(), points, rss));
        }
    }
    best.map(|(g, _, rss)| (g, rss))
}

/// Linux's `oom_badness` for process `g` against `total` pages: its resident pages plus its
/// `oom_score_adj` times a thousandth of `total`, and the resident pages; None for a process never
/// picked (the built-in init, `oom_score_adj` -1000, no live thread, a vfork child still sharing
/// its parent's address space). Unlike Linux, swap and page tables are not counted.
fn badness(k: &Kernel, g: &ThreadGroup, total: i64) -> Option<(i64, u64)> {
    if g.tgid == INIT || g.oom_score_adj == OOM_SCORE_ADJ_MIN {
        return None;
    }
    let task = g.threads.first().and_then(|t| k.tasks.get(t))?;
    let mm = task.res.borrow().mm.clone();
    // Linux's in_vfork: ending it frees nothing, and with the same memory and score it would win
    // over its parent as the later process (posix_spawn's child before it execs)
    let parent = k.tasks.get(&g.parent_thread);
    if task.vfork.get() && parent.is_some_and(|p| Rc::ptr_eq(&p.res.borrow().mm, &mm)) {
        return None;
    }
    let rss = mm.borrow().resident_pages();
    Some((rss as i64 + g.oom_score_adj as i64 * (total / 1000), rss))
}

/// `/proc/<pid>/oom_score` (Linux's `proc_oom_score`): the badness against the host's RAM plus swap,
/// scaled into 0 to 2000 (1000 + badness × 1000 / total, times 2/3), and 0 for a process never
/// picked.
pub(crate) fn proc_score(k: &Kernel, g: &ThreadGroup) -> u64 {
    let total = (tuxport_hal::ram_and_swap() / PAGE_SIZE).max(1) as i64;
    badness(k, g, total).map_or(0, |(b, _)| {
        ((1000 + b * 1000 / total) * 2 / 3).max(0) as u64
    })
}

/// Writes to the kernel log when ending a process ([`crate::klog`]): to the guest, the app's limit
/// is the `memory.max` of the cgroup it is in, so this writes as Linux's memcg OOM does (the first
/// line of `mem_cgroup_print_oom_meminfo`, `dump_oom_summary`, `__oom_kill_process`), and `dmesg`
/// shows who was ended and why. The call stack, the table of processes and the like are left out.
/// Resident pages are split by kind of mapping (counted by host memory, as smaps does), and there
/// is no page-table count (`VmPTE` is 0 too).
fn log_kill(
    k: &Kernel,
    g: &ThreadGroup,
    task: Option<&crate::task::Task>,
    (domain, used, limit): (u64, u64, u64),
) {
    use crate::klog::{level, printk};
    let comm = crate::pseudo::comm_str(task, g.tgid);
    let (pid, uid) = (g.tgid, g.cred.ruid);
    let failcnt = k.cgroups.get(domain).map_or(0, |c| c.mem_events.max);
    let log = &k.klog;
    printk(
        log,
        level::INFO,
        format_args!(
            "memory: usage {}kB, limit {}kB, failcnt {failcnt}",
            used / 1024,
            limit / 1024,
        ),
    );
    printk(
        log,
        level::INFO,
        format_args!(
            "oom-kill:constraint=CONSTRAINT_MEMCG,nodemask=(null),cpuset=/,mems_allowed=0,\
             oom_memcg={},task_memcg={},task={comm},pid={pid},uid={uid}",
            k.cgroups.path(domain, ROOT),
            k.cgroups.path(g.cgroup, ROOT),
        ),
    );
    log_killed(k, g, task);
}

/// The line about ending one process (`__oom_kill_process`). Resident pages are split by kind of
/// mapping (counted by host memory, as smaps does), and there is no page-table count.
fn log_killed(k: &Kernel, g: &ThreadGroup, task: Option<&crate::task::Task>) {
    let comm = crate::pseudo::comm_str(task, g.tgid);
    let (pid, uid) = (g.tgid, g.cred.ruid);
    let (mut vm, mut anon, mut file, mut shmem) = (0, 0, 0, 0);
    if let Some(t) = task {
        let res = t.res.borrow();
        let mm = res.mm.borrow();
        vm = mm.size() / 1024;
        for r in mm.regions() {
            let pages = mm.resident_in(r.start, r.end - r.start);
            match (r.file, r.shared) {
                (Some(_), _) => file += pages * 4,
                (None, true) => shmem += pages * 4,
                (None, false) => anon += pages * 4,
            }
        }
    }
    crate::klog::printk(
        &k.klog,
        crate::klog::level::ERR,
        format_args!(
            "Memory cgroup out of memory: Killed process {pid} ({comm}) total-vm:{vm}kB, \
             anon-rss:{anon}kB, file-rss:{file}kB, shmem-rss:{shmem}kB, UID:{uid} \
             pgtables:0kB oom_score_adj:{}",
            g.oom_score_adj
        ),
    );
}

/// The report for the app ([`crate::bug::KillReport`]): the app's limit, or which cgroup's
/// `memory.max`.
fn oom_reason(k: &Kernel, domain: u64, used: u64, limit: u64) -> crate::bug::KillReason {
    crate::bug::KillReason::OutOfMemory {
        cgroup: (domain != ROOT).then(|| k.cgroups.path(domain, ROOT)),
        used,
        limit,
    }
}

/// Ends process `g` with SIGKILL and counts it in the `oom_kill` of its cgroup (one already exiting
/// counts too, as on Linux).
fn kill(k: &mut Kernel, g: &Rc<RefCell<ThreadGroup>>) {
    let cgroup = g.borrow().cgroup;
    if g.borrow().exiting.is_none() {
        let status = ExitStatus::Signaled {
            signal: Signal::KILL,
            pc: None,
            fault_addr: None,
            core_dumped: false,
        };
        crate::signal::group_exit(k, g, status);
    }
    crate::cgroup::memory_event(k, cgroup, |e| e.oom_kill += 1);
}

/// The process picked last time has not finished exiting, and [`VICTIM_TIMEOUT`] has not passed
/// yet.
fn waiting_for_victim(k: &Kernel, now: Instant) -> bool {
    matches!(&k.oom.victim, Some((victim, at))
        if now < *at + VICTIM_TIMEOUT
            && victim.upgrade().is_some_and(|g| !g.borrow().threads.is_empty()))
}

/// Called each time the timer thread wakes up: while some guest thread is running, tears down the
/// pages Tier 1's code cache has written in the writable view once a second, and once more after
/// all threads are waiting, then stops ([`tuxport_engine::unmap_written_code`]). The code cache is
/// mapped twice for W^X, and the host's RSS counts pages per mapping, so a page touched in both
/// views counts twice: translations count double in the app usage measured above ([`app_memory`]
/// subtracts the ones not torn down yet) and in the RSS the LMK looks at. Tearing down by time
/// rather than by how much was written: JIT workloads backfill links and invalidate blocks in
/// thousands of different pages per second, and after a teardown every write to those pages takes
/// a page fault again. Returns when the next one is due.
pub(crate) fn unmap_code_views(k: &mut Kernel, shared: &Shared) -> Option<Instant> {
    let now = Instant::now();
    if let Some(next) = k.oom.code_views
        && now < next
    {
        return Some(next);
    }
    let idle = all_sleeping(k);
    // Woken by something else after stopping: no thread ran meanwhile, so there are no newly
    // written pages
    if !idle || k.oom.code_views.is_some() {
        tuxport_engine::unmap_written_code();
    }
    if idle && shared.stop_checks(|| all_sleeping(k)) {
        k.oom.code_views = None;
        return None;
    }
    k.oom.code_views = Some(now + CODE_VIEW_INTERVAL);
    k.oom.code_views
}

/// All guest threads are waiting in the kernel: memory will not grow any more, and the code cache
/// will not be written either.
fn all_sleeping(k: &Kernel) -> bool {
    k.tasks.values().all(|t| t.sleeping.load(Ordering::SeqCst))
}
