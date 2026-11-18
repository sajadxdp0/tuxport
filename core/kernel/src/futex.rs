//! Futex wait queues (the kernel/futex map). Keys follow Linux's `get_futex_key`: private
//! memory is keyed by address space and address, so a waiter is still found after the memory at
//! its address has been replaced (a `MAP_FIXED` mmap, or munmap and mmap again); shared anonymous
//! memory (shared mappings, System V shared memory) by its host address, so processes that share
//! one piece of host memory after fork can wake each other; shared file mappings by host file and
//! offset, because processes that each map the same file (the `/dev/shm` file of `sem_open`) see
//! different host addresses. With `FUTEX_PRIVATE_FLAG` every futex is keyed as private memory.
//!
//! A waiter registers in the queue and then waits; a woken waiter is removed from the queue, and
//! when it re-checks and finds itself no longer in the queue it knows it was woken by FUTEX_WAKE.
//! A wake notifies only the threads it wakes (their events were recorded when they registered),
//! and futex waiters listen on no wake channel or object ([`crate::kernel::Cx::wait_alone`]), so
//! no change elsewhere wakes them: the JVM and the Go runtime have many threads parked on locks
//! and condition variables, and waking all of them on every change elsewhere would be costly.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tuxport_engine::prot;
use tuxport_hal::Event;

use crate::kernel::{Shared, Tid};
use crate::mm::Mm;
use crate::uaccess::UserSlice;

/// A futex key ([`crate::mm::Mm::futex_key`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    /// Private memory, or any memory with `FUTEX_PRIVATE_FLAG`: the address space
    /// ([`crate::mm::Mm::id`]) and the address. `shared` is an operation without the flag
    /// (Linux's `FUT_OFF_MMSHARED`): private and shared operations on one address never meet.
    Private { mm: u64, addr: u64, shared: bool },
    /// Shared anonymous memory: its host address.
    Host(usize),
    /// Offset in a host file (device number, inode).
    File { dev: u64, ino: u64, offset: u64 },
}

struct Entry {
    id: u64,
    key: Key,
    bitset: u32,
    /// The waiting thread's event ([`crate::task::Task::event`]).
    event: Arc<Event>,
    /// A `FUTEX_LOCK_PI` waiter: its thread, which the lock is handed to ([`Futexes::first_pi`]).
    /// Plain wakes and requeues leave such waiters alone.
    pi: Option<PiWaiter>,
}

/// A thread blocked in `FUTEX_LOCK_PI`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PiWaiter {
    /// Its global tid.
    pub tid: Tid,
    /// The address of the lock word in its own address space.
    pub addr: u64,
}

/// All futex waiters, in registration order (first come, first served on wake).
#[derive(Default)]
pub(crate) struct Futexes {
    next_id: u64,
    waiting: Vec<Entry>,
}

impl Futexes {
    /// Registers a waiter and returns its id. `event` is the event to signal when it is woken; the
    /// waiter must wait with [`crate::kernel::Cx::wait_alone`].
    pub fn enqueue(&mut self, key: Key, bitset: u32, event: Arc<Event>) -> u64 {
        self.push(key, bitset, event, None)
    }

    /// Registers a `FUTEX_LOCK_PI` waiter ([`Futexes::enqueue`]), which gets the lock when it is
    /// woken ([`Futexes::first_pi`], [`Futexes::hand_over`]).
    pub fn enqueue_pi(&mut self, key: Key, event: Arc<Event>, waiter: PiWaiter) -> u64 {
        self.push(key, u32::MAX, event, Some(waiter))
    }

    fn push(&mut self, key: Key, bitset: u32, event: Arc<Event>, pi: Option<PiWaiter>) -> u64 {
        self.next_id += 1;
        self.waiting.push(Entry {
            id: self.next_id,
            key,
            bitset,
            event,
            pi,
        });
        self.next_id
    }

    /// The first `FUTEX_LOCK_PI` waiter on `key` (Linux's top waiter; without priorities, the one
    /// that came first): its id and thread.
    pub fn first_pi(&self, key: Key) -> Option<(u64, PiWaiter)> {
        self.waiting
            .iter()
            .find(|e| e.key == key && e.pi.is_some())
            .and_then(|e| e.pi.map(|w| (e.id, w)))
    }

    /// Whether `FUTEX_LOCK_PI` waiters other than `except` are queued on `key`.
    pub fn has_pi(&self, key: Key, except: Option<u64>) -> bool {
        self.waiting
            .iter()
            .any(|e| e.key == key && e.pi.is_some() && Some(e.id) != except)
    }

    /// The first `FUTEX_LOCK_PI` waiter of each key that has some (for owners that exit): its id
    /// and thread.
    pub fn pi_heads(&self) -> Vec<(u64, PiWaiter)> {
        let mut keys: Vec<Key> = Vec::new();
        let mut heads = Vec::new();
        for e in &self.waiting {
            if let Some(w) = e.pi
                && !keys.contains(&e.key)
            {
                keys.push(e.key);
                heads.push((e.id, w));
            }
        }
        heads
    }

    /// Wakes waiter `id`, the new owner of the lock it waited for.
    pub fn hand_over(&mut self, id: u64) {
        if let Some(i) = self.waiting.iter().position(|e| e.id == id) {
            let e = self.waiting.remove(i);
            crate::kernel::signal_later(&e.event);
        }
    }

    /// FUTEX_REQUEUE: wakes at most `n_wake` waiters on `key`, then moves at most `n_requeue` more
    /// to `key2`. Returns the total woken and moved.
    pub fn requeue(&mut self, key: Key, n_wake: u32, key2: Key, n_requeue: u32) -> u32 {
        let woken = self.wake(key, n_wake, u32::MAX);
        let mut moved = 0;
        for e in self
            .waiting
            .iter_mut()
            .filter(|e| e.key == key && e.pi.is_none())
        {
            if moved == n_requeue {
                break;
            }
            e.key = key2;
            moved += 1;
        }
        woken + moved
    }

    /// Wakes at most `n` waiters on `key` with an intersecting bitset, and returns how many were
    /// woken.
    pub fn wake(&mut self, key: Key, n: u32, bitset: u32) -> u32 {
        let mut woken = 0;
        self.waiting.retain(|e| {
            if woken < n && e.key == key && e.bitset & bitset != 0 && e.pi.is_none() {
                woken += 1;
                crate::kernel::signal_later(&e.event);
                false
            } else {
                true
            }
        });
        woken
    }

    /// Whether the waiter is still queued (if not, it has already been woken).
    pub fn is_queued(&self, id: u64) -> bool {
        self.waiting.iter().any(|e| e.id == id)
    }

    /// Removes a waiter (timed out, interrupted by a signal).
    pub fn remove(&mut self, id: u64) {
        self.waiting.retain(|e| e.id != id);
    }
}

/// Waiters registered in the queue ([`Futexes::enqueue`]); when done waiting they leave with
/// [`Enqueued::leave`]. On a panic midway (a kernel-layer bug, [`crate::bug`]) the destructor
/// removes those still queued: a leftover entry has nobody waiting on it, a later FUTEX_WAKE
/// would count against it, and the real waiter would not be woken; when the key is a host
/// address, that memory may later be given to another process.
pub(crate) struct Enqueued<'s, I: AsRef<[u64]>> {
    shared: &'s Shared,
    ids: I,
    left: bool,
}

impl<'s, I: AsRef<[u64]>> Enqueued<'s, I> {
    pub fn new(shared: &'s Shared, ids: I) -> Self {
        Enqueued {
            shared,
            ids,
            left: false,
        }
    }

    /// The registered ids.
    pub fn ids(&self) -> &[u64] {
        self.ids.as_ref()
    }

    /// Leaves the queue: in the same lock hold, removes those still queued and returns the index of
    /// the first one no longer queued (woken). A wake that coincides with a timeout or a signal
    /// interruption counts as a wake, as in Linux's `futex_unqueue`, so that wake is not lost.
    pub fn leave(mut self) -> Option<usize> {
        self.left = true;
        let mut futexes = self.shared.futexes();
        let mut woken = None;
        for (i, &id) in self.ids.as_ref().iter().enumerate() {
            if futexes.is_queued(id) {
                futexes.remove(id);
            } else if woken.is_none() {
                woken = Some(i);
            }
        }
        woken
    }
}

impl<I: AsRef<[u64]>> Drop for Enqueued<'_, I> {
    fn drop(&mut self) {
        if !self.left {
            let mut futexes = self.shared.futexes();
            for &id in self.ids.as_ref() {
                futexes.remove(id);
            }
        }
    }
}

pub(crate) const FUTEX_WAITERS: u32 = 0x8000_0000;
pub(crate) const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
pub(crate) const FUTEX_TID_MASK: u32 = 0x3fff_ffff;
/// The most list entries processed (`ROBUST_LIST_LIMIT`), against a cycle built by the guest.
const ROBUST_LIST_LIMIT: usize = 2048;

/// Processes a thread's robust futex list (set_robust_list) when it exits or calls execve, the
/// same as Linux's `exit_robust_list`: locks it still holds are marked `FUTEX_OWNER_DIED`, and if
/// there are waiters one is woken, which thereby gets EOWNERDEAD. `head` is the address of the
/// `struct robust_list_head`.
pub(crate) fn exit_robust_list(futexes: &mut Futexes, mm: &Mm, head: u64, tid: u32) {
    if head == 0 {
        return;
    }
    let word = |addr: u64| {
        let mut b = [0u8; 8];
        mm.read(UserSlice::new(addr, 8), &mut b)
            .ok()
            .map(|()| u64::from_le_bytes(b))
    };
    // The lowest bit of a list entry marks a PI futex
    let (Some(mut entry), Some(offset), Some(pending)) =
        (word(head), word(head + 8), word(head + 16))
    else {
        return;
    };
    for _ in 0..ROBUST_LIST_LIMIT {
        if entry & !1 == head {
            break;
        }
        let Some(next) = word(entry & !1) else {
            return;
        };
        // The entry being locked or unlocked is handled separately at the end
        if entry & !1 != pending & !1 {
            owner_died(
                futexes,
                mm,
                (entry & !1).wrapping_add(offset),
                tid,
                entry & 1 != 0,
                false,
            );
        }
        entry = next;
    }
    if pending & !1 != 0 {
        let addr = (pending & !1).wrapping_add(offset);
        owner_died(futexes, mm, addr, tid, pending & 1 != 0, true);
    }
}

/// Linux's `handle_futex_death`: if the lock at `addr` is held by `tid`, marks it
/// `FUTEX_OWNER_DIED` (keeping `FUTEX_WAITERS`), and wakes one waiter if there are any. Other
/// threads may be changing it with atomic operations in guest code, so a compare-and-swap is used.
fn owner_died(futexes: &mut Futexes, mm: &Mm, addr: u64, tid: u32, pi: bool, pending: bool) {
    if !addr.is_multiple_of(4) {
        return;
    }
    let Some(lock) = writable_word(mm, addr) else {
        return;
    };
    let mut cur = lock.load(Ordering::SeqCst);
    // As Linux's handle_futex_death, a shared wake (glibc's robust mutexes use shared operations
    // for this reason)
    let wake = |futexes: &mut Futexes| {
        if let Some(key) = mm.futex_key(addr, false) {
            futexes.wake(key, 1, u32::MAX);
        }
    };
    // Exited after unlocking but before waking the waiters: the lock is already 0, so wake one
    // directly, lest they wait forever
    if pending && !pi && cur == 0 {
        wake(futexes);
        return;
    }
    loop {
        if cur & FUTEX_TID_MASK != tid {
            return;
        }
        let new = (cur & FUTEX_WAITERS) | FUTEX_OWNER_DIED;
        match lock.compare_exchange(cur, new, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(v) => cur = v,
        }
    }
    if !pi && cur & FUTEX_WAITERS != 0 {
        wake(futexes);
    }
}

/// A thread has exited: each PI lock with waiters whose owner no longer exists goes to its first
/// waiter (Linux's `exit_pi_state_list` handing the rt_mutex over, and the new owner's
/// `fixup_pi_state_owner`). The word becomes the waiter's tid with `FUTEX_WAITERS` and
/// `FUTEX_OWNER_DIED`, whether or not the lock was on the owner's robust list.
pub(crate) fn pi_owners_exited(k: &crate::kernel::Kernel, futexes: &mut Futexes) {
    for (id, w) in futexes.pi_heads() {
        let Some(task) = k.tasks.get(&w.tid) else {
            continue;
        };
        let ns = k.pid_ns_of(task.tgid);
        let Ok(res) = task.res.try_borrow() else {
            continue;
        };
        let Ok(mm) = res.mm.try_borrow() else {
            continue;
        };
        let (Some(word), Some(me)) = (writable_word(&mm, w.addr), ns.vnr(w.tid)) else {
            continue;
        };
        let cur = word.load(Ordering::SeqCst);
        let owner = cur & FUTEX_TID_MASK;
        if owner != 0
            && ns
                .find(owner as Tid)
                .is_some_and(|g| k.tasks.contains_key(&g))
        {
            continue;
        }
        let new = FUTEX_OWNER_DIED | me as u32 | FUTEX_WAITERS;
        if word
            .compare_exchange(cur, new, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            futexes.hand_over(id);
        }
    }
}

/// The futex word at `addr` (4-byte aligned) for the kernel to change atomically while guest
/// threads may be changing it too, or None when it cannot be written. A copy-on-write page not
/// written since fork, or one without an entry yet, is made writable first, as for any kernel
/// write (`Mm::write`), and as Linux faults it in.
pub(crate) fn writable_word(mm: &Mm, addr: u64) -> Option<&AtomicU32> {
    let need = prot::READ | prot::WRITE;
    let host = match mm.translate(addr, need) {
        Some(host) => host,
        None if mm.cow_break(addr) || mm.fault_in(addr, need) => mm.translate(addr, need)?,
        None => return None,
    };
    // SAFETY: 4-byte-aligned, writable guest memory; the host memory is held by the address space,
    // which the caller borrows for as long as it uses the word
    Some(unsafe { AtomicU32::from_ptr(host.as_ptr().cast()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> Arc<Event> {
        Arc::new(Event::new().unwrap())
    }

    fn signaled(e: &Event) -> bool {
        crate::kernel::flush_signals();
        let mut p = [(e.fd(), 1, 0)];
        tuxport_hal::poll(&mut p, Some(std::time::Duration::ZERO)).unwrap();
        p[0].2 & 1 != 0
    }

    #[test]
    fn wake_and_requeue_in_order() {
        let (k1, k2) = (
            Key::Host(0x1000),
            Key::File {
                dev: 1,
                ino: 2,
                offset: 0,
            },
        );
        let mut f = Futexes::default();
        let (ea, eb) = (event(), event());
        let a = f.enqueue(k1, u32::MAX, ea.clone());
        let b = f.enqueue(k1, 0b10, eb.clone());
        let c = f.enqueue(k1, u32::MAX, event());
        let d = f.enqueue(k2, u32::MAX, event());
        assert_eq!(f.wake(k1, 1, 0b01), 1);
        assert!(!f.is_queued(a) && f.is_queued(b));
        // Only the woken thread is notified
        assert!(signaled(&ea) && !signaled(&eb));
        assert_eq!(f.requeue(k1, 1, k2, 5), 2);
        assert!(!f.is_queued(b) && f.is_queued(c));
        assert_eq!(f.wake(k2, 10, u32::MAX), 2);
        assert!(!f.is_queued(c) && !f.is_queued(d));
    }
}
