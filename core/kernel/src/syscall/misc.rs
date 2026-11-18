//! Miscellaneous syscalls: futex, time, resource limits and so on (the kernel/process map; uname,
//! syslog, adjtimex, clock setting and restart_syscall: the kernel/signal map; futexes and robust
//! lists: the kernel/futex map; clocks, sleeps and interval timers: the kernel/time map;
//! reboot: the kernel/boot map; time-namespace offsets: the kernel/ns map).

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::{IO_CHUNK, SysResult, get, put, read_timespec, write_pair};
use crate::cred::{cap, secbit};
use crate::errno::Errno;
use crate::futex::{Enqueued, Key};
use crate::itimer::{CpuTimer, Expiry, ITimer};
use crate::kernel::{Cx, Tid, wake};
use crate::klog;
use crate::process::{RLIM_NLIMITS, RLIMIT_NOFILE, Rusage, Signal};
use crate::uaccess::UserSlice;

const FUTEX_WAIT: u32 = 0;
const FUTEX_WAKE: u32 = 1;
const FUTEX_REQUEUE: u32 = 3;
const FUTEX_CMP_REQUEUE: u32 = 4;
const FUTEX_WAKE_OP: u32 = 5;
const FUTEX_LOCK_PI: u32 = 6;
const FUTEX_UNLOCK_PI: u32 = 7;
const FUTEX_TRYLOCK_PI: u32 = 8;
const FUTEX_WAIT_BITSET: u32 = 9;
const FUTEX_WAKE_BITSET: u32 = 10;
const FUTEX_WAIT_REQUEUE_PI: u32 = 11;
const FUTEX_LOCK_PI2: u32 = 13;
const FUTEX_CLOCK_REALTIME: u32 = 256;
/// The command: everything but `FUTEX_PRIVATE_FLAG` and `FUTEX_CLOCK_REALTIME`, so unknown high
/// bits make an unknown command (Linux's `FUTEX_CMD_MASK`).
const FUTEX_CMD_MASK: u32 = !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);

const TIMER_ABSTIME: u32 = 1;
const ITIMER_REAL: i32 = 0;
const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;

const PR_SET_PDEATHSIG: i32 = 1;
const PR_GET_PDEATHSIG: i32 = 2;
const PR_GET_DUMPABLE: i32 = 3;
const PR_SET_DUMPABLE: i32 = 4;
const PR_GET_KEEPCAPS: i32 = 7;
const PR_SET_KEEPCAPS: i32 = 8;
const PR_SET_NAME: i32 = 15;
const PR_GET_NAME: i32 = 16;
const PR_GET_SECCOMP: i32 = 21;
const PR_CAPBSET_READ: i32 = 23;
const PR_CAPBSET_DROP: i32 = 24;
const PR_GET_SECUREBITS: i32 = 27;
const PR_SET_SECUREBITS: i32 = 28;
const PR_GET_TIMING: i32 = 13;
const PR_SET_TIMING: i32 = 14;
const PR_TIMING_STATISTICAL: u64 = 0;
const PR_SET_SECCOMP: i32 = 22;
const PR_SET_THP_DISABLE: i32 = 41;
const PR_GET_THP_DISABLE: i32 = 42;
const PR_CAP_AMBIENT: i32 = 47;
const PR_CAP_AMBIENT_IS_SET: u64 = 1;
const PR_CAP_AMBIENT_RAISE: u64 = 2;
const PR_CAP_AMBIENT_LOWER: u64 = 3;
const PR_CAP_AMBIENT_CLEAR_ALL: u64 = 4;
const PR_GET_SPECULATION_CTRL: i32 = 52;
const PR_SPEC_NOT_AFFECTED: u64 = 0;
const PR_SET_TIMERSLACK: i32 = 29;
const PR_GET_TIMERSLACK: i32 = 30;
const PR_SET_CHILD_SUBREAPER: i32 = 36;
const PR_GET_CHILD_SUBREAPER: i32 = 37;
const PR_SET_NO_NEW_PRIVS: i32 = 38;
const PR_GET_NO_NEW_PRIVS: i32 = 39;
/// The highest capability number (`CAP_CHECKPOINT_RESTORE`).
const CAP_LAST_CAP: u64 = 40;

const GRND_NONBLOCK: u32 = 1;
const GRND_RANDOM: u32 = 2;
const GRND_INSECURE: u32 = 4;

/// The kernel version `uname -r` reports, that is, the ABI contract (the kernel/signal map).
const RELEASE: &[u8] = b"6.1.0-tuxport";

pub(super) fn futex(
    cx: &mut Cx<'_>,
    uaddr: u64,
    op: u32,
    val: u32,
    timeout: u64,
    uaddr2: u64,
    val3: u32,
) -> SysResult {
    let cmd = op & FUTEX_CMD_MASK;
    let private = op & FUTEX_PRIVATE_FLAG != 0;
    // A wait the no-BKL path (syscall::fast) began and a signal cut short hands over the deadline
    // it computed: its timeout was read and checked then, and is not read again, as Linux's
    // restart never reads it again (the memory may be gone by now)
    let early = cx.t.fast_sleep.take();
    // sys_futex reads and checks the timeout of the commands that take one before do_futex
    // allows FUTEX_CLOCK_REALTIME only for the absolute waits (ENOSYS)
    if timeout != 0
        && early.is_none()
        && matches!(
            cmd,
            FUTEX_WAIT | FUTEX_LOCK_PI | FUTEX_LOCK_PI2 | FUTEX_WAIT_BITSET | FUTEX_WAIT_REQUEUE_PI
        )
    {
        read_timespec(cx, timeout)?;
    }
    if op & FUTEX_CLOCK_REALTIME != 0
        && !matches!(
            cmd,
            FUTEX_WAIT_BITSET | FUTEX_WAIT_REQUEUE_PI | FUTEX_LOCK_PI2
        )
    {
        return Err(Errno::ENOSYS);
    }
    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let bitset = if cmd == FUTEX_WAIT { u32::MAX } else { val3 };
            if bitset == 0 {
                return Err(Errno::EINVAL);
            }
            // FUTEX_WAIT's timeout is relative (CLOCK_MONOTONIC), WAIT_BITSET's is absolute
            let deadline = match (timeout, early) {
                (0, _) => None,
                (_, Some(early)) => Some(early),
                (addr, None) => {
                    let t = read_timespec(cx, addr)?;
                    Some(if cmd == FUTEX_WAIT {
                        Expiry::after(t)
                    } else {
                        let clock = if op & FUTEX_CLOCK_REALTIME != 0 {
                            CLOCK_REALTIME
                        } else {
                            CLOCK_MONOTONIC
                        };
                        expiry_in_ns(cx, clock, t)?
                    })
                }
            };
            futex_wait(cx, (uaddr, private, val, bitset), deadline)
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let bitset = if cmd == FUTEX_WAKE { u32::MAX } else { val3 };
            if bitset == 0 {
                return Err(Errno::EINVAL);
            }
            let key = futex_key(cx, uaddr, private)?;
            Ok(cx.shared.futexes().wake(key, wake_count(val), bitset) as u64)
        }
        // The fourth argument is the second wake count
        FUTEX_WAKE_OP => futex_wake_op(cx, (uaddr, uaddr2), private, (val, timeout as u32), val3),
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            // The wake and requeue counts are ints; negative values are EINVAL
            if (val as i32) < 0 || (timeout as u32 as i32) < 0 {
                return Err(Errno::EINVAL);
            }
            let key = futex_key(cx, uaddr, private)?;
            let key2 = futex_key(cx, uaddr2, private)?;
            let mut futexes = cx.shared.futexes();
            if cmd == FUTEX_CMP_REQUEUE && u32::from_le_bytes(get(cx, uaddr)?) != val3 {
                return Err(Errno::EAGAIN);
            }
            // In REQUEUE the fourth argument is the number to requeue, not a timeout
            let n = futexes.requeue(key, val, key2, timeout as u32);
            Ok(n as u64)
        }
        // LOCK_PI's timeout is absolute on CLOCK_REALTIME, LOCK_PI2's on CLOCK_MONOTONIC unless
        // FUTEX_CLOCK_REALTIME; TRYLOCK_PI takes none
        FUTEX_LOCK_PI | FUTEX_LOCK_PI2 | FUTEX_TRYLOCK_PI => {
            let deadline = match timeout {
                0 => None,
                _ if cmd == FUTEX_TRYLOCK_PI => None,
                addr => {
                    let realtime = cmd == FUTEX_LOCK_PI || op & FUTEX_CLOCK_REALTIME != 0;
                    let clock = if realtime {
                        CLOCK_REALTIME
                    } else {
                        CLOCK_MONOTONIC
                    };
                    Some(expiry_in_ns(cx, clock, read_timespec(cx, addr)?)?)
                }
            };
            futex_lock_pi(cx, (uaddr, private), cmd == FUTEX_TRYLOCK_PI, deadline)
        }
        FUTEX_UNLOCK_PI => futex_unlock_pi(cx, uaddr, private),
        _ => {
            crate::stub::hit(cx, crate::stub::Stub::Futex(cmd));
            Err(Errno::ENOSYS)
        }
    }
}

/// How many waiters a wake count wakes at most: Linux counts with an int and stops once the count
/// is reached after waking one, so 0 and negative counts wake one.
pub(super) fn wake_count(n: u32) -> u32 {
    if (n as i32) <= 0 { 1 } else { n }
}

/// `FUTEX_WAKE_OP` (Linux's `futex_wake_op`): applies the operation `encoded` holds to the word at
/// `uaddr2` atomically (set, add, or, and-not or xor with its argument, which with
/// `FUTEX_OP_OPARG_SHIFT` is a shift count; another operation ENOSYS before anything changes),
/// wakes up to `nr` waiters on `uaddr`, and when the old value compares with the comparison
/// argument as asked, up to `nr2` on `uaddr2` (an unknown comparison is ENOSYS after the word has
/// changed). Returns how many were woken.
fn futex_wake_op(
    cx: &mut Cx<'_>,
    (uaddr, uaddr2): (u64, u64),
    private: bool,
    (nr, nr2): (u32, u32),
    encoded: u32,
) -> SysResult {
    const FUTEX_OP_OPARG_SHIFT: u32 = 8;
    let key = futex_key(cx, uaddr, private)?;
    let key2 = futex_key(cx, uaddr2, private)?;
    // 12-bit fields, sign-extended
    let field = |v: u32| ((v & 0xfff) << 20) as i32 >> 20;
    let op = (encoded >> 28) & 7;
    let cmp = (encoded >> 24) & 15;
    let mut oparg = field(encoded >> 12);
    let cmparg = field(encoded);
    if encoded >> 28 & FUTEX_OP_OPARG_SHIFT != 0 {
        oparg = 1 << (oparg & 31);
    }
    let mut futexes = cx.shared.futexes();
    let mm = cx.mm();
    let word = crate::futex::writable_word(&mm, uaddr2).ok_or(Errno::EFAULT)?;
    // FUTEX_OP_SET, ADD, OR, ANDN, XOR
    let apply = |old: i32| match op {
        0 => Some(oparg),
        1 => Some(old.wrapping_add(oparg)),
        2 => Some(old | oparg),
        3 => Some(old & !oparg),
        4 => Some(old ^ oparg),
        _ => None,
    };
    let old = word
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
            apply(v as i32).map(|n| n as u32)
        })
        .map_err(|_| Errno::ENOSYS)? as i32;
    // FUTEX_OP_CMP_EQ, NE, LT, LE, GT, GE
    let matched = match cmp {
        0 => old == cmparg,
        1 => old != cmparg,
        2 => old < cmparg,
        3 => old <= cmparg,
        4 => old > cmparg,
        5 => old >= cmparg,
        _ => return Err(Errno::ENOSYS),
    };
    let mut woken = futexes.wake(key, wake_count(nr), u32::MAX);
    if matched {
        woken += futexes.wake(key2, wake_count(nr2), u32::MAX);
    }
    Ok(woken as u64)
}

/// A futex wait (Linux's `futex_wait`): while `*uaddr` is `val`, waits until woken, `deadline`, or
/// interrupted. When interrupted without a timeout the result is ERESTARTSYS; with a timeout it
/// records the deadline and returns ERESTART_RESTARTBLOCK: without a handler, restart_syscall
/// goes on waiting until the original deadline (after a stop and continue, or a cgroup freeze and
/// thaw, the wait does not start over), and with a handler it is EINTR (even with SA_RESTART).
fn futex_wait(
    cx: &mut Cx<'_>,
    (uaddr, private, val, bitset): (u64, bool, u32, u32),
    deadline: Option<Expiry>,
) -> SysResult {
    let key = futex_key(cx, uaddr, private)?;
    let shared = cx.shared;
    // No-BKL wakes (syscall::fast) also take the futex lock, so nothing is missed between
    // checking the value and registering
    let id = {
        let mut futexes = shared.futexes();
        if u32::from_le_bytes(get(cx, uaddr)?) != val {
            return Err(Errno::EAGAIN);
        }
        futexes.enqueue(key, bitset, cx.task.event.clone())
    };
    let waiting = Enqueued::new(shared, [id]);
    crate::bug::trap(crate::bug::Trap::FutexWait, cx.tgid());
    loop {
        if !shared.futexes().is_queued(id) {
            waiting.leave();
            return Ok(0);
        }
        let err = if deadline.is_some_and(Expiry::passed) {
            Some(Errno::ETIMEDOUT)
        } else if cx.interrupted() {
            Some(match deadline {
                Some(_) => Errno::ERESTART_RESTARTBLOCK,
                None => Errno::ERESTARTSYS,
            })
        } else {
            cx.wait_alone(deadline.map(Expiry::instant)).err()
        };
        if let Some(e) = err {
            // It used to be removed only after releasing the futex lock: if a no-BKL wake
            // (syscall::fast) took it in between, the wake was lost while it returned an error
            return match waiting.leave() {
                Some(_) => Ok(0),
                None => {
                    if let (Errno::ERESTART_RESTARTBLOCK, Some(deadline)) = (e, deadline) {
                        cx.t.restart = Some(Restart::Futex {
                            uaddr,
                            private,
                            val,
                            bitset,
                            deadline,
                        });
                    }
                    Err(e)
                }
            };
        }
    }
}

/// `FUTEX_LOCK_PI`, `LOCK_PI2` and `TRYLOCK_PI` (Linux 6.1's `futex_lock_pi`, without priority
/// inheritance: the host schedules the threads): a free word (no owner) is taken at once, keeping
/// `FUTEX_OWNER_DIED`; one the caller owns is EDEADLK; otherwise `FUTEX_WAITERS` is set, the owner
/// must exist (ESRCH), a trylock gives up (EAGAIN), and a lock waits until its owner hands the word
/// over ([`futex_unlock_pi`], or [`crate::futex::pi_owners_exited`] when the owner exits). A
/// signal restarts the call (ERESTARTNOINTR); the deadline ends it with ETIMEDOUT. Tids in the word
/// are as each thread's own pid namespace sees them.
fn futex_lock_pi(
    cx: &mut Cx<'_>,
    (uaddr, private): (u64, bool),
    trylock: bool,
    deadline: Option<Expiry>,
) -> SysResult {
    use crate::futex::{FUTEX_OWNER_DIED, FUTEX_TID_MASK, FUTEX_WAITERS, PiWaiter};
    let key = futex_key(cx, uaddr, private)?;
    let me = cx.pid_ns().vnr(cx.tid()).unwrap_or(0) as u32;
    let shared = cx.shared;
    let id = {
        let mut futexes = shared.futexes();
        let mm = cx.mm();
        let word = crate::futex::writable_word(&mm, uaddr).ok_or(Errno::EFAULT)?;
        let mut cur = word.load(Ordering::SeqCst);
        loop {
            let owner = cur & FUTEX_TID_MASK;
            if owner == me {
                return Err(Errno::EDEADLK);
            }
            if owner == 0 {
                let mut new = (cur & FUTEX_OWNER_DIED) | me;
                if futexes.has_pi(key, None) {
                    new |= FUTEX_WAITERS;
                }
                match word.compare_exchange(cur, new, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => return Ok(0),
                    Err(v) => cur = v,
                }
                continue;
            }
            // The first waiter sets the bit before looking at the owner, so its unlock comes to
            // the kernel (futex_lock_pi_atomic)
            if cur & FUTEX_WAITERS == 0 {
                let new = cur | FUTEX_WAITERS;
                if let Err(v) = word.compare_exchange(cur, new, Ordering::SeqCst, Ordering::SeqCst)
                {
                    cur = v;
                    continue;
                }
            }
            break;
        }
        let owner = cx.pid_ns().find((cur & FUTEX_TID_MASK) as Tid);
        if !owner.is_some_and(|g| cx.kr().tasks.contains_key(&g)) {
            return Err(Errno::ESRCH);
        }
        if trylock {
            return Err(Errno::EAGAIN);
        }
        let waiter = PiWaiter {
            tid: cx.tid(),
            addr: uaddr,
        };
        futexes.enqueue_pi(key, cx.task.event.clone(), waiter)
    };
    let waiting = Enqueued::new(shared, [id]);
    crate::bug::trap(crate::bug::Trap::FutexWait, cx.tgid());
    loop {
        // Dequeued: the lock was handed over
        if !shared.futexes().is_queued(id) {
            waiting.leave();
            return Ok(0);
        }
        let err = if deadline.is_some_and(Expiry::passed) {
            Some(Errno::ETIMEDOUT)
        } else if cx.interrupted() {
            Some(Errno::ERESTARTNOINTR)
        } else {
            cx.wait_alone(deadline.map(Expiry::instant)).err()
        };
        if let Some(e) = err {
            return match waiting.leave() {
                Some(_) => Ok(0),
                None => Err(e),
            };
        }
    }
}

/// `FUTEX_UNLOCK_PI` (Linux 6.1's `futex_unlock_pi`): the caller must own the word (EPERM); the
/// first waiter gets it (its tid with `FUTEX_WAITERS`, `wake_futex_pi`) and is woken, and without
/// waiters it becomes 0.
fn futex_unlock_pi(cx: &mut Cx<'_>, uaddr: u64, private: bool) -> SysResult {
    use crate::futex::{FUTEX_TID_MASK, FUTEX_WAITERS};
    let key = futex_key(cx, uaddr, private)?;
    let me = cx.pid_ns().vnr(cx.tid()).unwrap_or(0) as u32;
    let mut futexes = cx.shared.futexes();
    let mm = cx.mm();
    let word = crate::futex::writable_word(&mm, uaddr).ok_or(Errno::EFAULT)?;
    let next = futexes.first_pi(key).and_then(|(id, w)| {
        let tgid = cx.kr().tasks.get(&w.tid)?.tgid;
        let tid = cx.kr().pid_ns_of(tgid).vnr(w.tid)?;
        Some((id, FUTEX_WAITERS | tid as u32))
    });
    let mut cur = word.load(Ordering::SeqCst);
    loop {
        if cur & FUTEX_TID_MASK != me {
            return Err(Errno::EPERM);
        }
        let new = next.map_or(0, |(_, v)| v);
        match word.compare_exchange(cur, new, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(v) => cur = v,
        }
    }
    if let Some((id, _)) = next {
        futexes.hand_over(id);
    }
    Ok(0)
}

/// Flags of `struct futex_waitv`: a 32-bit futex, and valid only within this process (we do not
/// tell them apart).
const FUTEX_32: u32 = 2;
const FUTEX_PRIVATE_FLAG: u32 = 128;
/// The most futexes futex_waitv waits on at once.
const FUTEX_WAITV_MAX: u32 = 128;

/// futex_waitv(2): waits on several futexes at once and returns the index of whichever is woken.
/// The timeout is an absolute time on `clock`.
pub(super) fn futex_waitv(
    cx: &mut Cx<'_>,
    waiters: u64,
    nr: u32,
    flags: u32,
    timeout: u64,
    clock: i32,
) -> SysResult {
    if flags != 0 || waiters == 0 || nr == 0 || nr > FUTEX_WAITV_MAX {
        return Err(Errno::EINVAL);
    }
    let deadline = match timeout {
        0 => None,
        _ if clock != CLOCK_MONOTONIC && clock != CLOCK_REALTIME => return Err(Errno::EINVAL),
        addr => Some(expiry_in_ns(cx, clock, read_timespec(cx, addr)?)?),
    };
    // struct futex_waitv { u64 val; u64 uaddr; u32 flags; u32 __reserved; }
    let mut keys = Vec::with_capacity(nr as usize);
    for i in 0..nr as u64 {
        let w: [u8; 24] = get(cx, waiters + i * 24)?;
        let val = u64::from_le_bytes(w[..8].try_into().unwrap());
        let uaddr = u64::from_le_bytes(w[8..16].try_into().unwrap());
        let wflags = u32::from_le_bytes(w[16..20].try_into().unwrap());
        let reserved = u32::from_le_bytes(w[20..].try_into().unwrap());
        if wflags & !(FUTEX_32 | FUTEX_PRIVATE_FLAG) != 0 || wflags & FUTEX_32 == 0 || reserved != 0
        {
            return Err(Errno::EINVAL);
        }
        keys.push((
            futex_key(cx, uaddr, wflags & FUTEX_PRIVATE_FLAG != 0)?,
            uaddr,
            val,
        ));
    }
    // Holding the futex lock, so nobody can wake between checking the values and registering
    let shared = cx.shared;
    let ids: Vec<u64> = {
        let mut futexes = shared.futexes();
        for &(_, uaddr, val) in &keys {
            if u64::from(u32::from_le_bytes(get(cx, uaddr)?)) != val {
                return Err(Errno::EAGAIN);
            }
        }
        let event = &cx.task.event;
        keys.iter()
            .map(|&(key, ..)| futexes.enqueue(key, u32::MAX, event.clone()))
            .collect()
    };
    let waiting = Enqueued::new(shared, ids);
    crate::bug::trap(crate::bug::Trap::FutexWait, cx.tgid());
    let r = loop {
        let woken = waiting
            .ids()
            .iter()
            .position(|&id| !shared.futexes().is_queued(id));
        if let Some(i) = woken {
            break Ok(i as u64);
        }
        let err = if deadline.is_some_and(Expiry::passed) {
            Some(Errno::ETIMEDOUT)
        } else if cx.interrupted() {
            Some(Errno::ERESTARTSYS)
        } else {
            cx.wait_alone(deadline.map(Expiry::instant)).err()
        };
        if let Some(e) = err {
            break Err(e);
        }
    };
    // As with Linux's futex_unqueue_multiple, a wake that lands just as the wait times out or is
    // interrupted by a signal counts as a wake
    match (waiting.leave(), r) {
        (Some(i), Err(_)) => Ok(i as u64),
        (_, r) => r,
    }
}

/// The futex key ([`crate::mm::Mm::futex_key`]); `private` is an operation with
/// `FUTEX_PRIVATE_FLAG`.
fn futex_key(cx: &Cx<'_>, uaddr: u64, private: bool) -> Result<Key, Errno> {
    if !uaddr.is_multiple_of(4) {
        return Err(Errno::EINVAL);
    }
    cx.mm().futex_key(uaddr, private).ok_or(Errno::EFAULT)
}

/// nanosleep(2). A sleep the no-BKL path began hands over its deadline ([`futex`]'s `early`), and
/// its time is not read again.
pub(super) fn nanosleep(cx: &mut Cx<'_>, req: u64, rem: u64) -> SysResult {
    let deadline = match cx.t.fast_sleep.take() {
        Some(early) => early,
        None => Expiry::after(read_timespec(cx, req)?),
    };
    sleep_rest(cx, deadline, rem)
}

pub(super) fn clock_nanosleep(
    cx: &mut Cx<'_>,
    clock: i32,
    flags: u32,
    req: u64,
    rem: u64,
) -> SysResult {
    let early = cx.t.fast_sleep.take();
    match clock {
        // Thread CPU clocks, the raw monotonic clock and the coarse clocks cannot be slept on
        CLOCK_THREAD_CPUTIME_ID | 4..=6 => return Err(Errno::EOPNOTSUPP),
        c if c < 0 && c & 4 != 0 => return Err(Errno::EINVAL),
        _ => {}
    }
    // A sleep the no-BKL path began hands over its deadline (as for nanosleep), relative or
    // absolute, and its time is not read again
    if let Some(early) = early {
        if flags & TIMER_ABSTIME == 0 {
            return sleep_rest(cx, early, rem);
        }
        return sleep_until(cx, early).map_or(Err(Errno::ERESTARTNOHAND), |()| Ok(0));
    }
    let t = read_timespec(cx, req)?;
    // Process CPU clocks: CPU time converted to approximate wall time (checked every 10 ms)
    if let Some(cpu) = cpu_clock(cx, clock) {
        let now = cpu?;
        let target = if flags & TIMER_ABSTIME != 0 {
            t.as_nanos() as u64
        } else {
            now + t.as_nanos() as u64
        };
        while cpu_clock(cx, clock).unwrap()? < target {
            if sleep_until(cx, Expiry::after(Duration::from_millis(10))).is_err() {
                return Err(Errno::ERESTARTNOHAND);
            }
        }
        return Ok(0);
    }
    // A relative sleep counts on the clock's base: CLOCK_BOOTTIME's goes on while the device
    // sleeps, CLOCK_REALTIME's moves to the monotonic base (Expiry::new)
    if flags & TIMER_ABSTIME == 0 {
        tuxport_hal::clock_gettime(clock).map_err(Errno)?;
        return sleep_rest(cx, Expiry::new(clock, t, false)?, rem);
    }
    // An absolute time: re-executing with the same arguments simply continues the sleep
    match sleep_until(cx, expiry_in_ns(cx, clock, t)?) {
        Ok(()) => Ok(0),
        Err(_) => Err(Errno::ERESTARTNOHAND),
    }
}

/// Sleeps until `deadline`. When interrupted by a signal, writes the remaining time to `rem` (if
/// not 0) and records the deadline: when no handler interrupts (for example a stop followed by a
/// continue), restart_syscall goes on sleeping instead of sleeping the original duration again.
fn sleep_rest(cx: &mut Cx<'_>, deadline: Expiry, rem: u64) -> SysResult {
    match sleep_until(cx, deadline) {
        Ok(()) => Ok(0),
        Err(left) => {
            if rem != 0 {
                write_pair(cx, rem, left.as_secs() as i64, left.subsec_nanos() as i64)?;
            }
            cx.t.restart = Some(Restart::Sleep { deadline, rem });
            Err(Errno::ERESTART_RESTARTBLOCK)
        }
    }
}

/// Sleeps until `deadline`; returns the remaining time when interrupted by a signal. Each pass
/// turns the deadline into an instant again, so a jump of its clock (which wakes every waiter)
/// moves the wait with it.
fn sleep_until(cx: &mut Cx<'_>, deadline: Expiry) -> Result<(), Duration> {
    loop {
        if deadline.passed() {
            return Ok(());
        }
        if cx.interrupted() || cx.wait(Some(deadline.instant()), &mut []).is_err() {
            return Err(deadline.remaining());
        }
    }
}

/// What restart_syscall continues after an interruption without a handler (Linux's
/// `restart_block`).
pub(crate) enum Restart {
    /// nanosleep, clock_nanosleep: the deadline and the address of `rem`.
    Sleep { deadline: Expiry, rem: u64 },
    /// A futex wait with a timeout (`futex_wait_restart`): goes on waiting with the original
    /// arguments until the original deadline.
    Futex {
        uaddr: u64,
        private: bool,
        val: u32,
        bitset: u32,
        deadline: Expiry,
    },
}

/// restart_syscall: continues an interrupted nanosleep or futex wait. As on Linux, returns EINTR
/// when there is nothing to continue.
pub(super) fn restart_syscall(cx: &mut Cx<'_>) -> SysResult {
    match cx.t.restart.take() {
        Some(Restart::Sleep { deadline, rem }) => sleep_rest(cx, deadline, rem),
        Some(Restart::Futex {
            uaddr,
            private,
            val,
            bitset,
            deadline,
        }) => futex_wait(cx, (uaddr, private, val, bitset), Some(deadline)),
        None => Err(Errno::EINTR),
    }
}

const ITIMER_VIRTUAL: i32 = 1;
const ITIMER_PROF: i32 = 2;

pub(super) fn getitimer(cx: &mut Cx<'_>, which: i32, curr: u64) -> SysResult {
    check_itimer(which)?;
    let t = itimer_value(cx, which);
    write_itimerval(cx, curr, t).map(|()| 0)
}

pub(super) fn setitimer(cx: &mut Cx<'_>, which: i32, new: u64, old: u64) -> SysResult {
    check_itimer(which)?;
    // As on Linux, a null new value means disarm
    let (interval, value) = if new == 0 {
        (Duration::ZERO, Duration::ZERO)
    } else {
        read_itimerval(cx, new)?
    };
    if old != 0 {
        let prev = itimer_value(cx, which);
        write_itimerval(cx, old, prev)?;
    }
    if which == ITIMER_REAL {
        cx.t.group.borrow_mut().itimer = (!value.is_zero()).then(|| ITimer {
            deadline: Instant::now() + value,
            interval,
        });
    } else {
        let now = cx.t.group.borrow().cpu_ns(cx.kr());
        cx.t.group.borrow_mut().cpu_itimers[(which - 1) as usize] =
            (!value.is_zero()).then(|| CpuTimer {
                deadline: now + value.as_nanos() as u64,
                interval: interval.as_nanos() as u64,
            });
    }
    // Make the timer thread recompute its wait. Wake only it: this used to wake all waiters
    crate::kernel::signal_later(&cx.shared.timers);
    Ok(0)
}

fn check_itimer(which: i32) -> Result<(), Errno> {
    match which {
        ITIMER_REAL | ITIMER_VIRTUAL | ITIMER_PROF => Ok(()),
        _ => Err(Errno::EINVAL),
    }
}

/// Reads a `struct itimerval`: (interval, time until the first expiry).
fn read_itimerval(cx: &Cx<'_>, addr: u64) -> Result<(Duration, Duration), Errno> {
    let b: [u8; 32] = get(cx, addr)?;
    let tv = |o: usize| {
        let sec = i64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let usec = i64::from_le_bytes(b[o + 8..o + 16].try_into().unwrap());
        if !(0..1_000_000).contains(&usec) {
            return Err(Errno::EINVAL);
        }
        super::timespec(sec, usec * 1000).ok_or(Errno::EINVAL)
    };
    Ok((tv(0)?, tv(16)?))
}

/// The timer's current value: (interval, time left until the next expiry), or None when it is not
/// armed.
fn itimer_value(cx: &Cx<'_>, which: i32) -> Option<(Duration, Duration)> {
    let g = cx.t.group.borrow();
    if which == ITIMER_REAL {
        let t = g.itimer?;
        return Some((
            t.interval,
            t.deadline.saturating_duration_since(Instant::now()),
        ));
    }
    let t = g.cpu_itimers[(which - 1) as usize]?;
    let left = t.deadline.saturating_sub(g.cpu_ns(cx.kr()));
    Some((Duration::from_nanos(t.interval), Duration::from_nanos(left)))
}

/// Writes a `struct itimerval`: the interval and the time left until the next expiry.
fn write_itimerval(cx: &Cx<'_>, addr: u64, t: Option<(Duration, Duration)>) -> Result<(), Errno> {
    let (interval, left) = match t {
        // As on Linux, a timer that has not expired yet reports at least 1 microsecond
        Some((interval, left)) => (interval, left.max(Duration::from_micros(1))),
        None => (Duration::ZERO, Duration::ZERO),
    };
    let mut b = [0u8; 32];
    for (o, d) in [(0, interval), (16, left)] {
        b[o..o + 8].copy_from_slice(&(d.as_secs() as i64).to_le_bytes());
        b[o + 8..o + 16].copy_from_slice(&(d.subsec_micros() as i64).to_le_bytes());
    }
    put(cx, addr, b)
}

const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;

pub(super) fn clock_gettime(cx: &mut Cx<'_>, clock: i32, tp: u64) -> SysResult {
    let (sec, nsec) = match cpu_clock(cx, clock) {
        Some(ns) => {
            let ns = ns?;
            ((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64)
        }
        // Monotonic and boottime clocks get the time namespace offset added
        None => add_offset(
            tuxport_hal::clock_gettime(clock).map_err(Errno)?,
            cx.time_offset(clock),
        ),
    };
    write_pair(cx, tp, sec, nsec).map(|()| 0)
}

pub(super) fn clock_getres(cx: &mut Cx<'_>, clock: i32, res: u64) -> SysResult {
    let (sec, nsec) = match cpu_clock(cx, clock) {
        Some(ns) => ns.map(|_| (0, 1))?,
        None => tuxport_hal::clock_getres(clock).map_err(Errno)?,
    };
    if res != 0 {
        write_pair(cx, res, sec, nsec)?;
    }
    Ok(0)
}

/// Nanoseconds used on the CPU clock of a guest process or thread; None when `clock` is not a CPU
/// clock. Negative clock ids are encoded by `clock_getcpuclockid`/`pthread_getcpuclockid`
/// (Linux's `MAKE_PROCESS_CPUCLOCK`): `(~pid << 3) | type`, where bit 2 means a thread and pid 0
/// means ourselves; the pid is as the caller's pid namespace numbers it.
fn cpu_clock(cx: &Cx<'_>, clock: i32) -> Option<Result<u64, Errno>> {
    let (pid, thread) = match clock {
        CLOCK_PROCESS_CPUTIME_ID => (0, false),
        CLOCK_THREAD_CPUTIME_ID => (0, true),
        // Type 3 is not a CPU clock (low 3 bits of 3 are a dynamic clock encoded from an fd)
        c if c < 0 && c & 3 == 3 => return Some(Err(Errno::EINVAL)),
        c if c < 0 => (cx.pid_in(!(c >> 3)), c & 4 != 0),
        _ => return None,
    };
    let k = cx.kr();
    Some(if thread {
        let task = if pid == 0 {
            Some(cx.task.clone())
        } else {
            k.tasks.get(&pid).cloned()
        };
        task.map(|t| t.cpu_ns()).ok_or(Errno::EINVAL)
    } else {
        let group = if pid == 0 {
            Some(cx.t.group.clone())
        } else {
            k.groups.get(&pid).cloned()
        };
        group.map(|g| g.borrow().cpu_ns(k)).ok_or(Errno::EINVAL)
    })
}

/// clock_settime: the guest cannot change the host's time. The settable clock (`CLOCK_REALTIME`)
/// returns EPERM, as without `CAP_SYS_TIME`; the other clocks cannot be set in any case.
pub(super) fn clock_settime(cx: &mut Cx<'_>, clock: i32, tp: u64) -> SysResult {
    read_timespec(cx, tp)?;
    match clock {
        CLOCK_REALTIME => Err(Errno::EPERM),
        _ => Err(Errno::EINVAL),
    }
}

/// adjtimex (`clock` None) and clock_adjtime: only queries are answered, with the host clock's
/// NTP state from [`tuxport_hal::adjtimex_query`]. Anything that would change the clock or NTP
/// parameters is EPERM, treated as lacking `CAP_SYS_TIME` as with clock_settime
/// (`timekeeping_validate_timex`). adjtimex writes the structure back whether it succeeds or
/// fails, clock_adjtime only on success, as on Linux.
pub(super) fn adjtimex(cx: &mut Cx<'_>, clock: Option<i32>, buf: u64) -> SysResult {
    const ADJ_FREQUENCY: u32 = 0x0002;
    const ADJ_SETOFFSET: u32 = 0x0100;
    // The definition inside the kernel (the user-space header's ADJ_OFFSET_SINGLESHOT is 0x8001,
    // with ADJ_ADJTIME)
    const ADJ_OFFSET_SINGLESHOT: u32 = 0x0001;
    const ADJ_OFFSET_READONLY: u32 = 0x2000;
    const ADJ_ADJTIME: u32 = 0x8000;
    /// `PPM_SCALE`: NSEC_PER_USEC << (NTP_SCALE_SHIFT - SHIFT_USEC).
    const PPM_SCALE: i64 = 1000 << 16;
    const CLOCKFD: i32 = 3;
    const CLOCK_SGI_CYCLE: i32 = 10;
    const CLOCK_TAI: i32 = 11;

    let mut tx: [u8; tuxport_hal::TIMEX_SIZE] = get(cx, buf)?;
    let modes = u32::from_le_bytes(tx[..4].try_into().unwrap());
    let freq = i64::from_le_bytes(tx[16..24].try_into().unwrap());
    let result = (|| {
        // do_clock_adjtime: only CLOCK_REALTIME can be adjusted; an fd-encoded dynamic clock
        // would need the fd of a device such as PTP, which Tuxport does not have
        match clock {
            None | Some(CLOCK_REALTIME) => {}
            Some(c) if c < 0 && c & 7 == CLOCKFD => return Err(Errno::EINVAL),
            Some(c) if c < 0 => return Err(Errno::EOPNOTSUPP),
            Some(CLOCK_SGI_CYCLE) => return Err(Errno::EINVAL),
            Some(c) if c > CLOCK_TAI => return Err(Errno::EINVAL),
            Some(_) => return Err(Errno::EOPNOTSUPP),
        }
        // ADJ_ADJTIME must come with ADJ_OFFSET_SINGLESHOT, and with ADJ_OFFSET_READONLY as well
        // (adjtime(3) querying the remaining adjustment) it is a query; without ADJ_ADJTIME, any
        // nonzero modes changes something
        let query_adjtime = modes & ADJ_ADJTIME != 0;
        if query_adjtime {
            if modes & ADJ_OFFSET_SINGLESHOT == 0 {
                return Err(Errno::EINVAL);
            }
            if modes & ADJ_OFFSET_READONLY == 0 {
                return Err(Errno::EPERM);
            }
        } else if modes != 0 {
            return Err(Errno::EPERM);
        }
        if modes & ADJ_SETOFFSET != 0 {
            return Err(Errno::EPERM);
        }
        if modes & ADJ_FREQUENCY != 0
            && !(i64::MIN / PPM_SCALE..=i64::MAX / PPM_SCALE).contains(&freq)
        {
            return Err(Errno::EINVAL);
        }
        let (state, host) = tuxport_hal::adjtimex_query(query_adjtime).map_err(Errno)?;
        // __do_adjtimex fills every named field, leaving modes and the padding alone
        for range in [8..44, 48..116, 120..164] {
            tx[range.clone()].copy_from_slice(&host[range]);
        }
        Ok(state as u64)
    })();
    if result.is_ok() || clock.is_none() {
        cx.mm().write(UserSlice::new(buf, tx.len()), &tx)?;
    }
    result
}

pub(crate) mod syslog_action {
    pub const CLOSE: i32 = 0;
    pub const OPEN: i32 = 1;
    pub const READ: i32 = 2;
    pub const READ_ALL: i32 = 3;
    pub const READ_CLEAR: i32 = 4;
    pub const CLEAR: i32 = 5;
    pub const SIZE_UNREAD: i32 = 9;
    pub const SIZE_BUFFER: i32 = 10;
}

/// syslog(2) (klogctl); for the contents see [`crate::klog`].
pub(super) fn syslog(cx: &mut Cx<'_>, kind: i32, buf: u64, len: i32) -> SysResult {
    do_syslog(cx, kind, buf, len, false)
}

/// The actions of syslog(2) and `/proc/kmsg` (`from_proc`) (`do_syslog`): read (2) waits until
/// there is a record and takes it (sharing the position with `/proc/kmsg`); read-all (3) and
/// read-clear (4) do not change the read position; clear (5) affects only read-all; the unread
/// byte count (9; the record count when `/proc/kmsg` uses it); the buffer size (10). Open and close
/// have no effect. Console on/off and the console level (6, 7, 8) are recorded as usual
/// (`/proc/sys/kernel/printk`), there is just no console to print to.
pub(crate) fn do_syslog(
    cx: &mut Cx<'_>,
    kind: i32,
    buf: u64,
    len: i32,
    from_proc: bool,
) -> SysResult {
    use syslog_action::*;
    /// arm64's `TASK_SIZE_MAX` (48-bit addresses): `access_ok` looks only at the address range.
    const TASK_SIZE_MAX: u64 = 1 << 48;

    check_syslog_permissions(cx, kind, from_proc)?;
    let check_buf = || -> Result<bool, Errno> {
        if buf == 0 || len < 0 {
            return Err(Errno::EINVAL);
        }
        if len > 0
            && buf
                .checked_add(len as u64)
                .is_none_or(|end| end > TASK_SIZE_MAX)
        {
            return Err(Errno::EFAULT);
        }
        Ok(len > 0)
    };
    let log = cx.kr().klog.clone();
    match kind {
        CLOSE | OPEN => Ok(0),
        READ => {
            if !check_buf()? {
                return Ok(0);
            }
            while !log.borrow().syslog_readable() {
                if cx.interrupted() {
                    return Err(Errno::ERESTARTSYS);
                }
                cx.wait_object(wake::of(&*log))?;
            }
            let text = log.borrow_mut().syslog_read(len as usize);
            cx.mm().write(UserSlice::new(buf, text.len()), &text)?;
            Ok(text.len() as u64)
        }
        READ_ALL | READ_CLEAR => {
            if !check_buf()? {
                return Ok(0);
            }
            let text = log.borrow_mut().read_all(len as usize, kind == READ_CLEAR);
            cx.mm().write(UserSlice::new(buf, text.len()), &text)?;
            Ok(text.len() as u64)
        }
        CLEAR => {
            log.borrow_mut().clear();
            Ok(0)
        }
        // Console off: save the current level (if not saved yet) and switch to the minimum level
        6 => {
            let k = cx.k();
            let level = k.printk[0];
            k.saved_console_loglevel.get_or_insert(level);
            k.printk[0] = k.printk[2];
            Ok(0)
        }
        7 => {
            let k = cx.k();
            if let Some(level) = k.saved_console_loglevel.take() {
                k.printk[0] = level;
            }
            Ok(0)
        }
        // Set the console level (1–8, not below the minimum level), which also turns the console
        // back on
        8 => {
            if !(1..=8).contains(&len) {
                return Err(Errno::EINVAL);
            }
            let k = cx.k();
            k.printk[0] = len.max(k.printk[2]);
            k.saved_console_loglevel = None;
            Ok(0)
        }
        SIZE_UNREAD => Ok(log.borrow_mut().size_unread(from_proc)),
        SIZE_BUFFER => Ok(klog::LOG_BUF_LEN as u64),
        _ => Err(Errno::EINVAL),
    }
}

/// Permissions (`check_syslog_permissions`), checked before the arguments: with `dmesg_restrict`
/// 0 anyone may read all and ask for the buffer size; other actions (and every action with
/// `dmesg_restrict` 1) need `CAP_SYSLOG` in the boot namespace; `CAP_SYS_ADMIN` alone also works,
/// but the first time a warning is logged. `/proc/kmsg` was checked when opened, so later reads
/// are not checked again (`from_proc`). Opening `/dev/kmsg` for reading is checked as read-all.
pub(crate) fn check_syslog_permissions(cx: &Cx<'_>, kind: i32, from_proc: bool) -> SysResult {
    use syslog_action::*;
    if from_proc && kind != OPEN {
        return Ok(0);
    }
    let restricted = cx.kr().dmesg_restrict || (kind != READ_ALL && kind != SIZE_BUFFER);
    if !restricted {
        return Ok(0);
    }
    let cred = cx.cred();
    if cred.capable(cap::SYSLOG) {
        return Ok(0);
    }
    if !cred.capable(cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    let log = cx.kr().klog.clone();
    if !std::mem::replace(&mut log.borrow_mut().sys_admin_warned, true) {
        let comm = crate::pseudo::comm_str(Some(cx.task), cx.tgid());
        klog::printk(
            &log,
            klog::level::WARNING,
            format_args!(
                "printk: {comm} ({}): Attempt to access syslog with CAP_SYS_ADMIN but no \
                 CAP_SYSLOG (deprecated).",
                cx.tid()
            ),
        );
    }
    Ok(0)
}

pub(super) fn gettimeofday(cx: &mut Cx<'_>, tv: u64, tz: u64) -> SysResult {
    if tv != 0 {
        let (sec, nsec) = tuxport_hal::clock_gettime(CLOCK_REALTIME).map_err(Errno)?;
        write_pair(cx, tv, sec, nsec / 1000)?;
    }
    if tz != 0 {
        put(cx, tz, timezone())?;
    }
    Ok(0)
}

/// The `struct timezone` gettimeofday reports: the host kernel's, as the host's vDSO reports it
/// (the kernel/signal map; the guest cannot set it, settimeofday being refused).
pub(super) fn timezone() -> [u8; 8] {
    let (west, dst) = tuxport_hal::timezone();
    let mut tz = [0u8; 8];
    tz[..4].copy_from_slice(&west.to_le_bytes());
    tz[4..].copy_from_slice(&dst.to_le_bytes());
    tz
}

pub(super) fn times(cx: &mut Cx<'_>, buf: u64) -> SysResult {
    let ticks = crate::loader::CLOCK_TICKS;
    // struct tms: user and system CPU time of the process itself and of its reaped children
    if buf != 0 {
        let to_ticks = |ns: u64| ns / (1_000_000_000 / ticks);
        let g = cx.t.group.borrow();
        let (me, children) = (g.cpu(cx.kr()), g.child_cpu);
        drop(g);
        let mut b = [0u8; 32];
        let fields = [me.user_ns(), me.sys_ns, children.user_ns(), children.sys_ns];
        for (i, ns) in fields.into_iter().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&to_ticks(ns).to_le_bytes());
        }
        put(cx, buf, b)?;
    }
    // The return value is the number of clock ticks since boot
    let (sec, nsec) = tuxport_hal::clock_gettime(tuxport_hal::CLOCK_BOOTTIME).map_err(Errno)?;
    Ok(sec as u64 * ticks + nsec as u64 / (1_000_000_000 / ticks))
}

pub(super) fn getrusage(cx: &mut Cx<'_>, who: i32, usage: u64) -> SysResult {
    const RUSAGE_SELF: i32 = 0;
    const RUSAGE_CHILDREN: i32 = -1;
    const RUSAGE_THREAD: i32 = 1;
    if !matches!(who, RUSAGE_SELF | RUSAGE_CHILDREN | RUSAGE_THREAD) {
        return Err(Errno::EINVAL);
    }
    let group = cx.t.group.clone();
    let mut g = group.borrow_mut();
    // As on Linux, ru_maxrss for RUSAGE_THREAD is also the whole process's
    if who != RUSAGE_CHILDREN {
        g.sample_rss_throttled(&cx.mm());
    }
    let r = match who {
        RUSAGE_SELF => Rusage {
            cpu: g.cpu(cx.kr()),
            maxrss_kb: g.maxrss_kb,
        },
        RUSAGE_CHILDREN => Rusage {
            cpu: g.child_cpu,
            maxrss_kb: g.child_maxrss_kb,
        },
        _ => Rusage {
            cpu: cx.task.cpu_self(),
            maxrss_kb: g.maxrss_kb,
        },
    };
    drop(g);
    put(cx, usage, r.encode()).map(|()| 0)
}

pub(super) fn uname(cx: &mut Cx<'_>, buf: u64) -> SysResult {
    let uts = cx.task.ns.borrow().uts.clone();
    let fields: [&[u8]; 6] = [
        b"Linux",
        &uts.hostname.borrow(),
        RELEASE,
        b"#1 SMP PREEMPT tuxport",
        b"aarch64",
        &uts.domainname.borrow(),
    ];
    let mut out = [0u8; 6 * 65];
    for (i, f) in fields.iter().enumerate() {
        out[i * 65..i * 65 + f.len()].copy_from_slice(f);
    }
    cx.mm()
        .write(UserSlice::new(buf, out.len()), &out)
        .map(|()| 0)
}

/// reboot: the host cannot be rebooted; only the Ctrl-Alt-Del switch is accepted, and the other
/// valid commands are EPERM.
pub(super) fn reboot(cx: &mut Cx<'_>, magic1: u32, magic2: u32, cmd: u32) -> SysResult {
    const CAP_SYS_BOOT: u32 = 22;
    const LINUX_REBOOT_MAGIC1: u32 = 0xfee1_dead;
    const MAGIC2: [u32; 4] = [672_274_793, 85_072_278, 369_367_448, 537_993_216];
    const CMD_CAD_ON: u32 = 0x89ab_cdef;
    const CMD_CAD_OFF: u32 = 0;
    const OTHER_CMDS: [u32; 6] = [
        0x0123_4567, // RESTART
        0xcdef_0123, // HALT
        0x4321_fedc, // POWER_OFF
        0xa1b2_c3d4, // RESTART2
        0xd000_fce2, // SW_SUSPEND
        0x4558_4543, // KEXEC
    ];
    if !cx.cred().capable(CAP_SYS_BOOT) {
        return Err(Errno::EPERM);
    }
    if magic1 != LINUX_REBOOT_MAGIC1 || !MAGIC2.contains(&magic2) {
        return Err(Errno::EINVAL);
    }
    match cmd {
        CMD_CAD_ON | CMD_CAD_OFF => Ok(0),
        c if OTHER_CMDS.contains(&c) => Err(Errno::EPERM),
        _ => Err(Errno::EINVAL),
    }
}

/// sethostname, setdomainname: change the names of the UTS namespace the caller is in, which
/// requires `CAP_SYS_ADMIN` in its owner.
pub(super) fn set_uts_name(cx: &mut Cx<'_>, name: u64, len: i64, domain: bool) -> SysResult {
    let uts = cx.task.ns.borrow().uts.clone();
    if !cx.cred().ns_capable(&uts.owner, cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    let len = usize::try_from(len)
        .ok()
        .filter(|&n| n <= crate::kernel::UTS_LEN)
        .ok_or(Errno::EINVAL)?;
    let mut value = vec![0u8; len];
    cx.mm().read(UserSlice::new(name, len), &mut value)?;
    *if domain {
        uts.domainname.borrow_mut()
    } else {
        uts.hostname.borrow_mut()
    } = value;
    Ok(0)
}

pub(super) fn prlimit(cx: &mut Cx<'_>, pid: i32, resource: u32, new: u64, old: u64) -> SysResult {
    let cred = cx.cred();
    let group = if pid == 0 || pid == cx.tgid() {
        cx.t.group.clone()
    } else {
        let g = cx.kr().groups.get(&pid).cloned().ok_or(Errno::ESRCH)?;
        // Another user's process: its user and group IDs must all equal the caller's real IDs
        let t = cx.kr().cred_of(pid).ok_or(Errno::ESRCH)?;
        let same = [t.ruid, t.euid, t.suid].iter().all(|&u| u == cred.ruid)
            && [t.rgid, t.egid, t.sgid].iter().all(|&g| g == cred.rgid);
        if !same && !cred.capable(cap::SYS_RESOURCE) {
            return Err(Errno::EPERM);
        }
        g
    };
    if resource as usize >= RLIM_NLIMITS {
        return Err(Errno::EINVAL);
    }
    let slot = group.borrow().rlimits[resource as usize];
    let new = if new != 0 {
        let b: [u8; 16] = get(cx, new)?;
        let cur = u64::from_le_bytes(b[..8].try_into().unwrap());
        let max = u64::from_le_bytes(b[8..].try_into().unwrap());
        if cur > max {
            return Err(Errno::EINVAL);
        }
        // The fd table's capacity is fixed; raising the hard limit requires CAP_SYS_RESOURCE
        if resource == RLIMIT_NOFILE && max > cx.kr().nr_open as u64
            || max > slot.1 && !cred.capable(cap::SYS_RESOURCE)
        {
            return Err(Errno::EPERM);
        }
        Some((cur, max))
    } else {
        None
    };
    if old != 0 {
        write_pair(cx, old, slot.0 as i64, slot.1 as i64)?;
    }
    if let Some(v) = new {
        group.borrow_mut().rlimits[resource as usize] = v;
        // The fd table allocates new fds by the soft limit of RLIMIT_NOFILE
        if resource == RLIMIT_NOFILE {
            let k = cx.kr();
            for tid in &group.borrow().threads {
                if let Some(t) = k.tasks.get(tid) {
                    t.res.borrow().fds.borrow_mut().limit = v.0 as usize;
                }
            }
        }
    }
    Ok(0)
}

pub(super) fn umask(cx: &mut Cx<'_>, mask: u32) -> SysResult {
    let mut fs = cx.t.fs.borrow_mut();
    let old = fs.umask;
    fs.umask = mask & 0o777;
    Ok(old as u64)
}

pub(super) fn prctl(cx: &mut Cx<'_>, option: i32, args: [u64; 4]) -> SysResult {
    let arg = args[0];
    // As on Linux, unused arguments must be 0
    let unused = |from: usize| {
        if args[from..].iter().all(|&a| a == 0) {
            Ok(())
        } else {
            Err(Errno::EINVAL)
        }
    };
    match option {
        PR_SET_NAME => {
            // The name need not be NUL-terminated; at most 15 bytes are taken
            let raw = match cx.mm().read_cstr(arg, 15) {
                Ok(name) => name,
                Err(e) if e == Errno::ENAMETOOLONG => get::<15>(cx, arg)?.to_vec(),
                Err(e) => return Err(e),
            };
            let mut comm = [0u8; 16];
            comm[..raw.len()].copy_from_slice(&raw);
            cx.task.comm.set(comm);
            Ok(0)
        }
        PR_GET_NAME => put(cx, arg, cx.task.comm.get()).map(|()| 0),
        PR_SET_PDEATHSIG => {
            if arg != 0 && Signal::new(arg as i32).is_none() {
                return Err(Errno::EINVAL);
            }
            cx.task.pdeathsig.set(arg as i32);
            Ok(0)
        }
        PR_GET_PDEATHSIG => {
            let sig = cx.task.pdeathsig.get();
            put(cx, arg, sig.to_le_bytes()).map(|()| 0)
        }
        // As on Linux the other arguments are not checked; any nonzero value counts as set
        PR_SET_CHILD_SUBREAPER => {
            cx.t.group.borrow_mut().child_subreaper = arg != 0;
            Ok(0)
        }
        PR_GET_CHILD_SUBREAPER => {
            let on = cx.t.group.borrow().child_subreaper as i32;
            put(cx, arg, on.to_le_bytes()).map(|()| 0)
        }
        PR_GET_DUMPABLE => Ok(cx.t.group.borrow().dumpable as u64),
        PR_SET_DUMPABLE if arg <= 1 => {
            cx.t.group.borrow_mut().dumpable = arg == 1;
            Ok(0)
        }
        PR_SET_NO_NEW_PRIVS if arg == 1 => {
            unused(1)?;
            cx.t.group.borrow_mut().no_new_privs = true;
            Ok(0)
        }
        PR_GET_NO_NEW_PRIVS => {
            unused(0)?;
            Ok(cx.t.group.borrow().no_new_privs as u64)
        }
        PR_GET_KEEPCAPS => Ok(cx.cred().keep_caps() as u64),
        PR_SET_KEEPCAPS if arg <= 1 => {
            let mut c = cx.cred();
            if c.securebits & secbit::KEEP_CAPS_LOCKED != 0 {
                return Err(Errno::EPERM);
            }
            c.securebits = (c.securebits & !secbit::KEEP_CAPS) | (arg as u32 * secbit::KEEP_CAPS);
            cx.set_cred(c);
            Ok(0)
        }
        PR_GET_SECUREBITS => Ok(cx.cred().securebits as u64),
        PR_SET_SECUREBITS => {
            let bits = u32::try_from(arg).map_err(|_| Errno::EPERM)?;
            let mut c = cx.cred();
            c.set_securebits(bits)?;
            cx.set_cred(c);
            Ok(0)
        }
        PR_GET_SECCOMP => Ok(u64::from(cx.task.seccomp.borrow().mode)),
        PR_SET_SECCOMP => crate::seccomp::prctl_set(cx, arg, args[1]),
        PR_CAPBSET_READ if arg <= CAP_LAST_CAP => Ok((cx.cred().cap_bounding >> arg) & 1),
        PR_CAPBSET_DROP if arg <= CAP_LAST_CAP => {
            let mut c = cx.cred();
            if !c.capable_here(cap::SETPCAP) {
                return Err(Errno::EPERM);
            }
            c.cap_bounding &= !(1 << arg);
            cx.set_cred(c);
            Ok(0)
        }
        // Ambient capabilities (capabilities(7))
        PR_CAP_AMBIENT => match arg {
            PR_CAP_AMBIENT_CLEAR_ALL => {
                unused(1)?;
                let mut c = cx.cred();
                c.cap_ambient = 0;
                cx.set_cred(c);
                Ok(0)
            }
            PR_CAP_AMBIENT_IS_SET | PR_CAP_AMBIENT_RAISE | PR_CAP_AMBIENT_LOWER => {
                unused(2)?;
                if args[1] > CAP_LAST_CAP {
                    return Err(Errno::EINVAL);
                }
                let mut c = cx.cred();
                match arg {
                    PR_CAP_AMBIENT_IS_SET => return Ok((c.cap_ambient >> args[1]) & 1),
                    PR_CAP_AMBIENT_RAISE => c.raise_ambient(args[1] as u32)?,
                    _ => c.cap_ambient &= !(1 << args[1]),
                }
                cx.set_cred(c);
                Ok(0)
            }
            _ => Err(Errno::EINVAL),
        },
        // Only recorded; 0 means restore the default
        PR_SET_TIMERSLACK => {
            let (_, default) = cx.task.timerslack.get();
            cx.task
                .timerslack
                .set((if arg == 0 { default } else { arg }, default));
            Ok(0)
        }
        PR_GET_TIMERSLACK => Ok(cx.task.timerslack.get().0),
        // Only statistical timing
        PR_SET_TIMING if arg == PR_TIMING_STATISTICAL => Ok(0),
        PR_GET_TIMING => Ok(PR_TIMING_STATISTICAL),
        PR_SET_THP_DISABLE => {
            unused(1)?;
            cx.t.group.borrow_mut().thp_disable = arg != 0;
            Ok(0)
        }
        PR_GET_THP_DISABLE => {
            unused(0)?;
            Ok(cx.t.group.borrow().thp_disable as u64)
        }
        // The guest sees no speculative-execution mitigations
        PR_GET_SPECULATION_CTRL => {
            unused(1)?;
            match arg {
                0..=2 => Ok(PR_SPEC_NOT_AFFECTED),
                _ => Err(Errno::ENODEV),
            }
        }
        // Options handled above whose arguments were refused
        PR_SET_DUMPABLE | PR_SET_NO_NEW_PRIVS | PR_SET_KEEPCAPS | PR_CAPBSET_READ
        | PR_CAPBSET_DROP | PR_SET_TIMING => Err(Errno::EINVAL),
        _ => {
            crate::stub::hit(cx, crate::stub::Stub::Prctl(option));
            Err(Errno::EINVAL)
        }
    }
}

/// personality(2): 0xffffffff only queries.
pub(super) fn personality(cx: &mut Cx<'_>, persona: u32) -> SysResult {
    let mut g = cx.t.group.borrow_mut();
    let old = g.personality;
    if persona != u32::MAX {
        g.personality = persona;
    }
    Ok(old as u64)
}

/// getcpu(2): reports the first CPU in the affinity mask (threads are scheduled by the host, so
/// the CPU they actually run on is unknown).
pub(super) fn getcpu(cx: &mut Cx<'_>, cpu: u64, node: u64) -> SysResult {
    let first = cx.task.affinity.get().trailing_zeros().min(63);
    if cpu != 0 {
        put(cx, cpu, first.to_le_bytes())?;
    }
    if node != 0 {
        put(cx, node, 0u32.to_le_bytes())?;
    }
    Ok(0)
}

pub(super) fn set_robust_list(cx: &mut Cx<'_>, head: u64, len: u64) -> SysResult {
    // Size of struct robust_list_head
    if len != 24 {
        return Err(Errno::EINVAL);
    }
    cx.task.robust_list.set((head, len));
    Ok(0)
}

pub(super) fn get_robust_list(cx: &mut Cx<'_>, pid: i32, head: u64, len: u64) -> SysResult {
    let (h, l) = if pid == 0 {
        cx.task.robust_list.get()
    } else if pid == crate::kernel::INIT {
        // The built-in init belongs to root and has no robust list
        if cx.cred().ruid != 0 && !cx.cred().capable(cap::SYS_PTRACE) {
            return Err(Errno::EPERM);
        }
        (0, 0)
    } else {
        let task = cx.kr().tasks.get(&pid).cloned().ok_or(Errno::ESRCH)?;
        // As with ptrace: only threads of the same user can be inspected
        let target = task.cred.borrow().clone();
        let cred = cx.cred();
        if cred.ruid != target.ruid && !cred.ns_capable(&target.user_ns, cap::SYS_PTRACE) {
            return Err(Errno::EPERM);
        }
        task.robust_list.get()
    };
    put(cx, head, h.to_le_bytes())?;
    put(cx, len, l.to_le_bytes())?;
    Ok(0)
}

pub(super) fn sysinfo(cx: &mut Cx<'_>, buf: u64) -> SysResult {
    // As with Linux's do_sysinfo, a fraction of a second rounds up to a whole second
    let (secs, ns) = tuxport_hal::clock_gettime(tuxport_hal::CLOCK_BOOTTIME).map_err(Errno)?;
    // Add the time namespace's boottime offset (Linux's timens_add_boottime)
    let (secs, ns) = add_offset((secs, ns), cx.time_offset(tuxport_hal::CLOCK_BOOTTIME));
    let uptime = secs + i64::from(ns != 0);
    let m = tuxport_hal::host_meminfo();
    let procs = cx.kr().tasks.len().min(u16::MAX as usize) as u16;
    // struct sysinfo (64-bit): uptime, loads[3], totalram, freeram, sharedram, bufferram,
    // totalswap, freeswap, procs, totalhigh, freehigh, mem_unit; copied from the host as
    // /proc/meminfo is
    let mut b = [0u8; 112];
    b[..8].copy_from_slice(&uptime.to_le_bytes());
    for (i, load) in crate::loadavg::sysinfo_loads(cx.kr()).iter().enumerate() {
        b[8 + i * 8..16 + i * 8].copy_from_slice(&load.to_le_bytes());
    }
    for (i, v) in [
        m.total,
        m.free,
        m.shmem,
        m.buffers,
        m.swap_total,
        m.swap_free,
    ]
    .into_iter()
    .enumerate()
    {
        b[32 + i * 8..40 + i * 8].copy_from_slice(&v.to_le_bytes());
    }
    b[80..82].copy_from_slice(&procs.to_le_bytes());
    b[104..108].copy_from_slice(&1u32.to_le_bytes());
    put(cx, buf, b).map(|()| 0)
}

/// membarrier(2). Kicking the other threads that share the address space out of guest code amounts
/// to a full memory barrier and context synchronization on them (V8 uses the `SYNC_CORE` form
/// after rewriting code). Global commands also act only on this process: other processes share
/// only shared mappings with it, and all of Tuxport's guest threads are in one host process, so a
/// host memory barrier is added here as well.
pub(super) fn membarrier(cx: &mut Cx<'_>, cmd: i32, flags: u32) -> SysResult {
    const QUERY: i32 = 0;
    const GLOBAL: i32 = 1;
    const GLOBAL_EXPEDITED: i32 = 2;
    const REGISTER_GLOBAL_EXPEDITED: i32 = 4;
    const PRIVATE_EXPEDITED: i32 = 8;
    const REGISTER_PRIVATE_EXPEDITED: i32 = 16;
    const PRIVATE_EXPEDITED_SYNC_CORE: i32 = 32;
    const REGISTER_PRIVATE_EXPEDITED_SYNC_CORE: i32 = 64;
    const SUPPORTED: i32 = GLOBAL
        | GLOBAL_EXPEDITED
        | REGISTER_GLOBAL_EXPEDITED
        | PRIVATE_EXPEDITED
        | REGISTER_PRIVATE_EXPEDITED
        | PRIVATE_EXPEDITED_SYNC_CORE
        | REGISTER_PRIVATE_EXPEDITED_SYNC_CORE;
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let registered = cx.mm().membarrier;
    match cmd {
        QUERY => Ok(SUPPORTED as u64),
        REGISTER_GLOBAL_EXPEDITED
        | REGISTER_PRIVATE_EXPEDITED
        | REGISTER_PRIVATE_EXPEDITED_SYNC_CORE => {
            cx.mm_mut().membarrier |= cmd;
            Ok(0)
        }
        GLOBAL | GLOBAL_EXPEDITED | PRIVATE_EXPEDITED | PRIVATE_EXPEDITED_SYNC_CORE => {
            // Private commands require registering first, as on Linux
            if (cmd == PRIVATE_EXPEDITED && registered & REGISTER_PRIVATE_EXPEDITED == 0)
                || (cmd == PRIVATE_EXPEDITED_SYNC_CORE
                    && registered & REGISTER_PRIVATE_EXPEDITED_SYNC_CORE == 0)
            {
                return Err(Errno::EPERM);
            }
            std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
            let sync = cx.task.sync.clone();
            cx.mm().quiesce(&sync);
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn getrandom(cx: &mut Cx<'_>, buf: u64, len: usize, flags: u32) -> SysResult {
    if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0
        || flags & (GRND_RANDOM | GRND_INSECURE) == GRND_RANDOM | GRND_INSECURE
    {
        return Err(Errno::EINVAL);
    }
    let mut chunk = vec![0u8; len.min(IO_CHUNK)];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(IO_CHUNK);
        tuxport_hal::fill_random(&mut chunk[..n]).map_err(Errno)?;
        if let Err(e) = cx.mm().write(
            UserSlice::new(buf.wrapping_add(done as u64), n),
            &chunk[..n],
        ) {
            if done == 0 {
                return Err(e);
            }
            break;
        }
        done += n;
    }
    Ok(done as u64)
}

// ---- Time namespace offsets ----

/// The expiry of the absolute time `abs` on clock `clock` in the time namespace: first subtracts
/// the offset (Linux's `timens_ktime_to_host`; times before the offset count as already past),
/// then puts it on the clock's base ([`Expiry::new`]).
pub(super) fn expiry_in_ns(cx: &Cx<'_>, clock: i32, abs: Duration) -> Result<Expiry, Errno> {
    Expiry::new(clock, to_host_clock(cx.time_offset(clock), abs), true)
}

/// What time `abs`, which includes offset `offset`, is on the host clock: the offset subtracted,
/// not before 0.
pub(super) fn to_host_clock(offset: i64, abs: Duration) -> Duration {
    let ns = abs.as_nanos() as i128 - offset as i128;
    Duration::from_nanos(ns.clamp(0, u64::MAX as i128) as u64)
}

/// A host clock reading (sec, nsec) plus the time namespace offset `offset` (in nanoseconds).
pub(super) fn add_offset((sec, nsec): (i64, i64), offset: i64) -> (i64, i64) {
    if offset == 0 {
        return (sec, nsec);
    }
    let ns = sec as i128 * 1_000_000_000 + nsec as i128 + offset as i128;
    (
        ns.div_euclid(1_000_000_000) as i64,
        ns.rem_euclid(1_000_000_000) as i64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Time namespace conversions: readings plus offsets (borrows, negative offsets), absolute
    /// times minus offsets, not before 0.
    #[test]
    fn time_namespace_conversions() {
        assert_eq!(
            add_offset((10, 900_000_000), 200_000_000),
            (11, 100_000_000)
        );
        assert_eq!(
            add_offset((10, 100_000_000), -200_000_000),
            (9, 900_000_000)
        );
        assert_eq!(add_offset((10, 5), 0), (10, 5));
        let s = Duration::from_secs;
        assert_eq!(to_host_clock(86_400_000_000_000, s(86_410)), s(10));
        assert_eq!(to_host_clock(86_400_000_000_000, s(5)), Duration::ZERO);
        assert_eq!(to_host_clock(-1_000_000_000, s(5)), s(6));
    }
}
