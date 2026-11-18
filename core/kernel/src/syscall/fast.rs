//! Syscalls without the BKL (the kernel/process map).
//!
//! Parallel lstat calls (`git status` preloading the index, build tools, language servers' thread
//! pools) used to queue for the BKL, getting slower with more threads. Only the common cases that
//! change no state and whose inputs can all be had without locks are handled here: credentials
//! need no search-permission checks, the path starts at the root or the current directory, and the
//! whole path is in the store's metadata cache ([`Cached`]). When that is not possible nothing is
//! changed and the call goes to [`dispatch`](super::dispatch) with the BKL held, which also fills
//! the cache with what is needed.
//!
//! Clock reads (`clock_gettime`, `gettimeofday`) are here too. The host's vDSO reads most clocks
//! without entering the kernel layer (the kernel/vdso map), but they come here from its svc
//! stubs (the built vDSO, a time namespace with offsets), from what the vDSO leaves to the kernel
//! (CPU-time clocks), and from programs making the syscall themselves; with stubs only, one go
//! build made 2.18 million of them, and the few threads queuing for the BKL waited 2.5 s in total.
//! The host clocks and the thread's own CPU clock need no kernel state; process CPU clocks and
//! other threads' go to the BKL path.
//!
//! Futex wait and wake (the kernel/futex map) and nanosleep are here too: the futex wait queue has
//! its own lock ([`Shared::futexes`]), and a waiting thread sleeps on its own event, woken by futex
//! wakes, signals and thread-group exit just as when waiting with the BKL held. Only private memory
//! is handled, keyed by address space and address (the page is writable in the thread's page table
//! and not in a writable shared mapping, [`Mm::futex_key`](crate::mm::Mm::futex_key)). When the
//! thread wakes neither by a futex wake nor by expiry (most likely a signal arrived), the call goes
//! to the BKL path to be redone, which handles the signal and the restart.
//!
//! sched_yield is here too: it only yields the host thread, which needs no kernel state.
//!
//! For a thread with seccomp only two kinds of syscalls come here: numbers for which the filter
//! always returns ALLOW (cached by number, [`crate::seccomp::always_allowed`]), and the vDSO stubs'
//! reads of clocks Linux's vDSO reads in user space (which are not checked, as they do not enter
//! the kernel on Linux). Others go to the BKL path to be checked.
//!
//! While holding the BKL a thread records its root directory, current directory and credentials
//! ([`Fast`]). Other threads may change them (a chdir shared through `CLONE_FS`, credentials shared
//! by the thread group); the changing side increments the count of [`Shared::fast_changed`], which
//! invalidates what was recorded.
//!
//! While accessing guest memory the thread is still marked as running guest code
//! (`TaskSync::in_guest`): a thread that unmaps memory waits for it to leave before releasing the
//! host memory ([`Mm::sync`](crate::mm::Mm::sync)), just as for guest code's own memory accesses.
//! It is not marked while sleeping, and after waking it no longer accesses guest memory.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use tuxport_engine::{Cpu, PAGE_SIZE, prot};

use super::fs::{
    AT_FDCWD, AT_SYMLINK_NOFOLLOW, ROOT_DEV, encode_stat, encode_statx, newfstatat_flags,
    statx_flags,
};
use super::{PATH_MAX, SysResult};
use crate::errno::Errno;
use crate::futex::{Enqueued, Key};
use crate::itimer::Expiry;
use crate::kernel::{Shared, Tid};
use crate::task::{Task, TaskState};
use crate::vfs::store::{Ino, Stat};
use crate::vfs::{Cached, Node, ROOT_MOUNT};

/// The state a thread records for syscalls without the BKL.
pub(crate) struct Fast {
    /// [`Shared::fast_epoch`] when recorded.
    epoch: u64,
    /// Syscalls are being traced (`TUXPORT_STRACE`): all go to the BKL path so they can be printed.
    traced: bool,
    /// None when paths cannot be resolved without the BKL (the credentials need search-permission
    /// checks, or the root is not in the store).
    paths: Option<Paths>,
    /// The thread's own tid (execve may take over the main thread's; [`Shared::fast_epoch`] changes
    /// then too).
    tid: Tid,
    /// The same tid as the thread's own pid namespace numbers it, which `pthread_getcpuclockid`
    /// puts into its clock id.
    ns_tid: Tid,
    /// The host thread's CPU clock and its reading when the thread started ([`Task::cpu_ns`]).
    cpu_clock: Option<(i32, u64)>,
    /// The address space's [`crate::mm::Mm::id`], for futex keys.
    mm: u64,
    /// The ranges of the writable shared mappings in the address space
    /// ([`crate::mm::Mm::shared_maps`]): futex keys there are not the address space's.
    shared_maps: Arc<RwLock<Vec<(u64, u64)>>>,
    /// The address of the vDSO mapping and whether its clock functions are svc stubs
    /// ([`crate::mm::Mm::vdso_svc`]; an execve that replaces the address space changes
    /// [`Shared::fast_epoch`], and so does entering a time namespace).
    vdso: (u64, bool),
    /// Offsets (nanoseconds) of the time namespace the thread is in: the monotonic clock's and
    /// boottime's. setns switching namespaces calls [`Shared::fast_changed`], and the offsets no
    /// longer change once a process has entered.
    time: [i64; 2],
}

struct Paths {
    vfs: Cached,
    root: Ino,
    /// None when the current directory is not in the boot root filesystem (it is in `/proc` or in a
    /// bind mount).
    cwd: Option<Ino>,
}

impl Fast {
    /// Records the thread's current state. Called with the BKL held.
    pub(crate) fn new(shared: &Shared, task: &Task, t: &TaskState) -> Fast {
        let epoch = shared.fast_epoch();
        let fs = t.fs.borrow();
        // Resolve only in the boot root filesystem: in other mounts `..` must go through the
        // mount point
        let paths = match (&t.vfs, &fs.root) {
            (Some(vfs), &Node::Store(ROOT_MOUNT, root)) if task.cred.borrow().searches_all() => {
                let vfs = vfs.cached();
                vfs.usable.then(|| Paths {
                    vfs,
                    root,
                    cwd: match fs.cwd {
                        Node::Store(ROOT_MOUNT, ino) => Some(ino),
                        _ => None,
                    },
                })
            }
            _ => None,
        };
        Fast {
            epoch,
            traced: crate::debug::tracing(),
            paths,
            tid: task.tid.get(),
            ns_tid: t.group.borrow().pid_ns.vnr(task.tid.get()).unwrap_or(0),
            cpu_clock: task.clock.get().map(|c| (c, task.cpu_base.get().0)),
            mm: t.mm.borrow().id,
            shared_maps: t.mm.borrow().shared_maps(),
            vdso: (t.mm.borrow().vdso, t.mm.borrow().vdso_svc),
            time: task.ns.borrow().time.offsets_ns(),
        }
    }

    /// Records the state again if what was recorded is stale. Called with the BKL held.
    pub(crate) fn refresh(&mut self, shared: &Shared, task: &Task, t: &TaskState) {
        if shared.fast_epoch() != self.epoch {
            *self = Fast::new(shared, task, t);
        }
    }

    /// Runs the syscall `cpu` stopped at without the BKL. Returns true when done (the result
    /// written to x0); on false the caller handles it normally with the BKL held, and nothing but
    /// [`TaskState::fast_sleep`] has been changed here.
    pub(crate) fn syscall(&self, shared: &Shared, task: &Task, cpu: &mut Cpu) -> bool {
        // With an ic ivau not yet invalidated, the thread must first enter the kernel to invalidate
        // it (task::handle); under PTRACE_SYSCALL it must stop at entry and exit, and when
        // single-stepping the finished syscall must be reported (crate::ptrace); with seccomp it
        // must be checked first (crate::seccomp)
        if self.traced
            || cpu.state().ivau_page != 0
            || task.syscall_traced.load(Ordering::Relaxed)
            || task.stepping.load(Ordering::Relaxed)
            || !self.seccomp_allows(task, cpu)
        {
            return false;
        }
        crate::bug::trap(crate::bug::Trap::Fast, self.tid);
        let Some(r) = self.dispatch(shared, task, cpu) else {
            return false;
        };
        cpu.state_mut().x[0] = match r {
            Ok(v) => v,
            Err(e) => e.to_syscall_return(),
        };
        true
    }

    /// What may run without checking under seccomp: the vDSO stubs' clock reads
    /// ([`crate::vdso::clock_stub_at`]; on Linux they do not enter the kernel), and numbers for
    /// which the filter always returns ALLOW ([`crate::seccomp::always_allowed`]).
    fn seccomp_allows(&self, task: &Task, cpu: &Cpu) -> bool {
        if !task.seccomp_on.load(Ordering::Relaxed) {
            return true;
        }
        let s = cpu.state();
        crate::vdso::clock_stub_at(self.vdso, s.pc, s.x[8], s.x[0])
            || crate::seccomp::always_allowed(task, s.x[8])
    }

    fn dispatch(&self, shared: &Shared, task: &Task, cpu: &Cpu) -> Option<SysResult> {
        let s = cpu.state();
        let a = [s.x[0], s.x[1], s.x[2], s.x[3], s.x[4], s.x[5]];
        match s.x[8] {
            98 if !self.in_shared_map(a[0]) => {
                futex(shared, (task, cpu, self.mm), &a, self.time[0])
            }
            101 => nanosleep(task, cpu, a[0]),
            115 => {
                let clock = a[0] as i32;
                clock_nanosleep(task, cpu, (clock, self.offset(clock)), a[1] as u32, a[2])
            }
            124 => Some(sched_yield(task)),
            113 => self.clock_gettime(cpu, a[0] as i32, a[1]),
            114 => clock_getres(cpu, a[0] as i32, a[1]),
            169 => gettimeofday(cpu, a[0], a[1]),
            79 | 291 => {
                let p = self.paths.as_ref()?;
                if shared.fast_epoch() != self.epoch {
                    return None;
                }
                match s.x[8] {
                    79 => p.newfstatat(cpu, a[0] as i32, a[1], a[2], a[3] as u32),
                    _ => p.statx(cpu, a[0] as i32, a[1], a[2] as u32, a[3] as u32, a[4]),
                }
            }
            _ => None,
        }
    }

    /// The host clocks and the thread's own CPU clock of `misc::clock_gettime`.
    fn clock_gettime(&self, cpu: &Cpu, clock: i32, tp: u64) -> Option<SysResult> {
        let (sec, nsec) = if self.own_cpu_clock(clock) {
            let (host, base) = self.cpu_clock?;
            let (sec, nsec) = tuxport_hal::clock_gettime(host).ok()?;
            let ns = (sec as u64 * 1_000_000_000 + nsec as u64).saturating_sub(base);
            ((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64)
        } else if host_clock(clock) {
            super::misc::add_offset(tuxport_hal::clock_gettime(clock).ok()?, self.offset(clock))
        } else {
            return None;
        };
        write(cpu, tp, &pair(sec, nsec))?;
        Some(Ok(0))
    }

    /// The offset of clock `clock` in the time namespace ([`crate::nsproxy::TimeNs::offset`]).
    fn offset(&self, clock: i32) -> i64 {
        match clock {
            1 | 4 | 6 => self.time[0],
            7 | 9 => self.time[1],
            _ => 0,
        }
    }

    /// `addr` is in a writable shared file mapping.
    fn in_shared_map(&self, addr: u64) -> bool {
        let ranges = self
            .shared_maps
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        ranges.iter().any(|&(s, e)| (s..e).contains(&addr))
    }

    /// `clock` is the thread's own CPU clock: `CLOCK_THREAD_CPUTIME_ID`, or its own as encoded by
    /// `pthread_getcpuclockid` (`misc::cpu_clock`).
    fn own_cpu_clock(&self, clock: i32) -> bool {
        clock == CLOCK_THREAD_CPUTIME_ID
            || clock < 0 && clock & 3 != 3 && clock & 4 != 0 && {
                let tid = !(clock >> 3);
                tid == 0 || tid == self.ns_tid
            }
    }
}

const CLOCK_REALTIME: i32 = 0;
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;

/// Use the host's clock directly (the clocks of `misc::clock_gettime` that are not CPU clocks).
/// Clocks the host does not know go to the BKL path, which reports the error.
fn host_clock(clock: i32) -> bool {
    clock >= 0 && clock != CLOCK_PROCESS_CPUTIME_ID && clock != CLOCK_THREAD_CPUTIME_ID
}

fn clock_getres(cpu: &Cpu, clock: i32, res: u64) -> Option<SysResult> {
    if !host_clock(clock) {
        return None;
    }
    let (sec, nsec) = tuxport_hal::clock_getres(clock).ok()?;
    if res != 0 {
        write(cpu, res, &pair(sec, nsec))?;
    }
    Some(Ok(0))
}

fn gettimeofday(cpu: &Cpu, tv: u64, tz: u64) -> Option<SysResult> {
    if tv != 0 {
        let (sec, nsec) = tuxport_hal::clock_gettime(CLOCK_REALTIME).ok()?;
        write(cpu, tv, &pair(sec, nsec / 1000))?;
    }
    if tz != 0 {
        write(cpu, tz, &super::misc::timezone())?;
    }
    Some(Ok(0))
}

const FUTEX_WAIT: u32 = 0;
const FUTEX_WAKE: u32 = 1;
const FUTEX_WAIT_BITSET: u32 = 9;
const FUTEX_WAKE_BITSET: u32 = 10;
const FUTEX_PRIVATE_FLAG: u32 = 128;
const FUTEX_CLOCK_REALTIME: u32 = 256;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_BOOTTIME: i32 = 7;
const TIMER_ABSTIME: u32 = 1;

/// Futex wait and wake of `misc::futex` on private memory, with `a` the syscall arguments and `mm`
/// the address space's [`crate::mm::Mm::id`]. The timeout is relative in FUTEX_WAIT and absolute
/// in WAIT_BITSET.
/// `monotonic` is the monotonic clock's offset in the time namespace (WAIT_BITSET's absolute
/// timeout is converted with it).
fn futex(
    shared: &Shared,
    (task, cpu, mm): (&Task, &Cpu, u64),
    a: &[u64; 6],
    monotonic: i64,
) -> Option<SysResult> {
    let (uaddr, op, val, timeout, val3) = (a[0], a[1] as u32, a[2] as u32, a[3], a[5] as u32);
    let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    // FUTEX_CLOCK_REALTIME with another command is ENOSYS, which the BKL path answers
    if op & FUTEX_CLOCK_REALTIME != 0 && cmd != FUTEX_WAIT_BITSET {
        return None;
    }
    let bitset = match cmd {
        FUTEX_WAIT | FUTEX_WAKE => u32::MAX,
        FUTEX_WAIT_BITSET | FUTEX_WAKE_BITSET if val3 != 0 => val3,
        _ => return None,
    };
    // The key of private memory is the address space and the address (Mm::futex_key; the caller
    // left shared mappings to the BKL path). Borrowed copy-on-write pages and read-only mappings
    // are rare places for a futex: they go to the BKL path too, which reads them
    if !uaddr.is_multiple_of(4) {
        return None;
    }
    let host = cpu.translate(uaddr, prot::WRITE)?.as_ptr().cast::<u32>();
    let key = Key::Private {
        mm,
        addr: uaddr,
        shared: op & FUTEX_PRIVATE_FLAG == 0,
    };
    if cmd == FUTEX_WAKE || cmd == FUTEX_WAKE_BITSET {
        let woken = shared
            .futexes()
            .wake(key, super::misc::wake_count(val), bitset);
        crate::kernel::flush_signals();
        return Some(Ok(woken as u64));
    }
    let deadline = match timeout {
        0 => None,
        addr => {
            let t = read_timespec(cpu, addr)?;
            Some(if cmd == FUTEX_WAIT {
                Expiry::after(t)
            } else {
                let (clock, offset) = if op & FUTEX_CLOCK_REALTIME != 0 {
                    (CLOCK_REALTIME, 0)
                } else {
                    (CLOCK_MONOTONIC, monotonic)
                };
                let t = super::misc::to_host_clock(offset, t);
                Expiry::new(clock, t, true).ok()?
            })
        }
    };
    let id = {
        let mut futexes = shared.futexes();
        // SAFETY: host points at 4-byte-aligned guest memory in this page; the thread is marked
        // as running guest code, so the page is not released meanwhile
        let now = unsafe { AtomicU32::from_ptr(host) }.load(Ordering::SeqCst);
        if now != val {
            return Some(Err(Errno::EAGAIN));
        }
        futexes.enqueue(key, bitset, task.event.clone())
    };
    let waiting = Enqueued::new(shared, [id]);
    crate::bug::trap(crate::bug::Trap::FutexWait, task.tgid);
    let woke = sleep(task, deadline);
    match waiting.leave() {
        Some(_) => Some(Ok(0)),
        // Most likely a signal arrived: the BKL path re-checks the value, enqueues again and
        // handles the signal, waiting until the deadline computed here without reading the
        // timeout again ([`TaskState::fast_sleep`])
        None if woke => {
            if deadline.is_some() {
                task.state.borrow_mut().fast_sleep = deadline;
            }
            None
        }
        None => Some(Err(Errno::ETIMEDOUT)),
    }
}

fn nanosleep(task: &Task, cpu: &Cpu, req: u64) -> Option<SysResult> {
    let deadline = Expiry::after(read_timespec(cpu, req)?);
    sleep_until(task, deadline)
}

/// The wall clock, the monotonic clock and the since-boot clock of `misc::clock_nanosleep`.
/// `offset` is this clock's offset in the time namespace (absolute times are converted with it).
fn clock_nanosleep(
    task: &Task,
    cpu: &Cpu,
    (clock, offset): (i32, i64),
    flags: u32,
    req: u64,
) -> Option<SysResult> {
    if !matches!(clock, CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME)
        || flags & !TIMER_ABSTIME != 0
    {
        return None;
    }
    let t = read_timespec(cpu, req)?;
    if flags & TIMER_ABSTIME == 0 {
        return sleep_until(task, Expiry::new(clock, t, false).ok()?);
    }
    let t = super::misc::to_host_clock(offset, t);
    let deadline = Expiry::new(clock, t, true).ok()?;
    sleep_until(task, deadline)
}

/// Sleeps until `deadline`. When woken (most likely by a signal) the BKL path goes on sleeping,
/// with the deadline recorded in [`TaskState::fast_sleep`]: it takes that instead of reading the
/// time again, so a relative sleep does not start over from now, the time remaining when a
/// signal interrupts comes from it, and memory unmapped meanwhile is never read (Linux's restart
/// keeps the deadline too).
fn sleep_until(task: &Task, deadline: Expiry) -> Option<SysResult> {
    if !sleep(task, Some(deadline)) {
        return Some(Ok(0));
    }
    task.state.borrow_mut().fast_sleep = Some(deadline);
    None
}

/// Sleeps without the BKL until `deadline` (None sleeps forever), or until the thread's event is
/// signalled (futex wake, signal, being ended). While sleeping it does not count as running guest
/// code: a thread that unmaps memory need not wait for it (Mm::sync), and it is marked again on
/// returning to the run loop. Returns true when the event was signalled (or a host signal
/// interrupted the wait); a jump of the deadline's clock signals it too ([`crate::itimer::clock_jumped`]),
/// and the locked path then looks at the deadline again.
fn sleep(task: &Task, deadline: Option<Expiry>) -> bool {
    const POLLIN: i16 = 1;
    task.sync.in_guest.store(false, Ordering::SeqCst);
    task.sleeping.store(true, Ordering::SeqCst);
    let woke = loop {
        let timeout = match deadline {
            Some(d) if d.passed() => break false,
            Some(d) => Some(d.remaining()),
            None => None,
        };
        let mut fds = [(task.event.fd(), POLLIN, 0)];
        match tuxport_hal::poll(&mut fds, timeout) {
            Ok(0) => {}
            _ => break true,
        }
    };
    task.sleeping.store(false, Ordering::SeqCst);
    if woke {
        task.event.clear();
    }
    woke
}

/// sched_yield: the host thread yields its CPU, as Linux's `do_sched_yield` yields the guest
/// thread's. Spinning threads wait with it (parking_lot's and crossbeam's backoff, Go's osyield,
/// HotSpot's GC termination); when it took the BKL and only returned, eight threads passing a
/// turn around with it went 230 times slower than natively, and spinners slowed every other
/// thread's syscalls. It touches no guest memory, so it does not count as running guest code.
fn sched_yield(task: &Task) -> SysResult {
    task.sync.in_guest.store(false, Ordering::SeqCst);
    std::thread::yield_now();
    Ok(0)
}

/// Reads a `struct timespec` from guest memory (`super::read_timespec`). Returns None when it
/// cannot be read or is invalid, so the BKL path reports the error.
fn read_timespec(cpu: &Cpu, addr: u64) -> Option<Duration> {
    let mut b = [0u8; 16];
    read(cpu, addr, &mut b)?;
    let sec = i64::from_le_bytes(b[..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(b[8..].try_into().unwrap());
    super::timespec(sec, nsec)
}

/// `struct timespec`, `struct timeval`.
fn pair(a: i64, b: i64) -> [u8; 16] {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&a.to_le_bytes());
    buf[8..].copy_from_slice(&b.to_le_bytes());
    buf
}

impl Paths {
    fn newfstatat(
        &self,
        cpu: &Cpu,
        dirfd: i32,
        path: u64,
        buf: u64,
        flags: u32,
    ) -> Option<SysResult> {
        newfstatat_flags(flags).ok()?;
        let st = match self.stat(cpu, dirfd, path, flags)? {
            Ok(st) => st,
            Err(e) => return Some(Err(e)),
        };
        write(cpu, buf, &encode_stat(&st, ROOT_DEV))?;
        Some(Ok(0))
    }

    fn statx(
        &self,
        cpu: &Cpu,
        dirfd: i32,
        path: u64,
        flags: u32,
        mask: u32,
        buf: u64,
    ) -> Option<SysResult> {
        let flags = statx_flags(flags, mask).ok()?;
        let st = match self.stat(cpu, dirfd, path, flags)? {
            Ok(st) => st,
            Err(e) => return Some(Err(e)),
        };
        write(cpu, buf, &encode_statx(&encode_stat(&st, ROOT_DEV)))?;
        Some(Ok(0))
    }

    /// The case of `fs::stat_at` where the path is not empty and starts at the root or the current
    /// directory.
    fn stat(&self, cpu: &Cpu, dirfd: i32, path: u64, flags: u32) -> Option<Result<Stat, Errno>> {
        let mut buf = [0u8; PATH_MAX];
        let path = read_path(cpu, path, &mut buf)?;
        let cwd = match path.first()? {
            b'/' => self.root,
            _ if dirfd == AT_FDCWD => self.cwd?,
            _ => return None,
        };
        self.vfs
            .stat(self.root, cwd, path, flags & AT_SYMLINK_NOFOLLOW == 0)
    }
}

/// Reads a NUL-terminated path from guest memory into `buf` (`syscall::read_path`). Returns None
/// when it cannot be read or is too long.
fn read_path<'b>(cpu: &Cpu, addr: u64, buf: &'b mut [u8; PATH_MAX]) -> Option<&'b [u8]> {
    let mut done = 0;
    while done < buf.len() {
        let cur = addr.checked_add(done as u64)?;
        let chunk = ((PAGE_SIZE - cur % PAGE_SIZE) as usize).min(buf.len() - done);
        let host = cpu.translate(cur, prot::READ)?;
        // SAFETY: the thread is marked as running guest code, so this page's host memory is not
        // released meanwhile
        let bytes = unsafe { std::slice::from_raw_parts(host.as_ptr(), chunk) };
        if let Some(n) = bytes.iter().position(|&b| b == 0) {
            buf[done..done + n].copy_from_slice(&bytes[..n]);
            return Some(&buf[..done + n]);
        }
        buf[done..done + chunk].copy_from_slice(bytes);
        done += chunk;
    }
    None
}

/// Reads guest memory, requiring every page to be readable; returns None when it cannot be read.
fn read(cpu: &Cpu, addr: u64, buf: &mut [u8]) -> Option<()> {
    let mut done = 0;
    while done < buf.len() {
        let cur = addr.checked_add(done as u64)?;
        let chunk = ((PAGE_SIZE - cur % PAGE_SIZE) as usize).min(buf.len() - done);
        let host = cpu.translate(cur, prot::READ)?;
        // SAFETY: as in read_path
        unsafe { std::ptr::copy_nonoverlapping(host.as_ptr(), buf[done..].as_mut_ptr(), chunk) };
        done += chunk;
    }
    Some(())
}

/// Writes guest memory, requiring every page to be writable; returns None when it cannot be
/// written (part may already be written; the normal path writes it again).
fn write(cpu: &Cpu, addr: u64, data: &[u8]) -> Option<()> {
    let mut done = 0;
    while done < data.len() {
        let cur = addr.checked_add(done as u64)?;
        let chunk = ((PAGE_SIZE - cur % PAGE_SIZE) as usize).min(data.len() - done);
        let host = cpu.translate(cur, prot::WRITE)?;
        // SAFETY: as in read_path
        unsafe { std::ptr::copy_nonoverlapping(data[done..].as_ptr(), host.as_ptr(), chunk) };
        done += chunk;
    }
    Some(())
}
