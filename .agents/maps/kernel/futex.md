# kernel/futex Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/futex.md` for the paired decision history (decisions from before the split are D26, D44 and the futex halves of D32, D33 and D37 in `.agents/decisions/kernel/process.md`).
>
> Verified: 2026-11-11 (split out of the kernel/process full mapping of 2026-11-10; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-14 (targeted verification only since the full mapping; latest change: references follow the kernel/time and kernel/boot split from kernel/process)

## Responsibilities

`kernel/futex` is futexes (futex(2), 6.1's `kernel/futex/`):

- futex keys for private, shared anonymous and shared file memory;
- the futex syscall's commands, argument checks and wake counts, `FUTEX_WAKE_OP`, and
  `futex_waitv`;
- PI locks without priority inheritance, and handing them on when their owner exits;
- robust lists and `clear_child_tid` wakes at thread exit and execve;
- the one waiter queue shared by the BKL path and the fast path.

Root: `core/kernel/src/futex.rs`; in `syscall/misc.rs` the futex handlers (`futex`,
`futex_wake_op`, `futex_wait`, `futex_lock_pi`, `futex_unlock_pi`, `futex_waitv`, `futex_key`) and
`set_robust_list`/`get_robust_list`; the futex call of `syscall/fast.rs`.

Outside the module: the BKL, the waiting primitives, restart blocks and the fast path's record and
deadline hand-over (`kernel/process`); thread exit and execve, which call the robust-list and PI
hand-overs (`kernel/process`, `kernel/exec`); translating guest addresses and breaking
copy-on-write (`kernel/mm`); counting unimplemented commands (`kernel/signal`).

## Key types

- `futex::Key` — `Private{mm, addr, shared}`, `Host(address)` or `File{dev, ino, offset}`;
  `Futexes` — one queue (in registration order) of (id, key, bitset, event, and for a
  `FUTEX_LOCK_PI` waiter a `PiWaiter`: its global tid and the lock's address in its address space)
  behind `Shared`'s futex lock; `Enqueued` — leaves the queue on the normal path (a wake racing it
  counts as a wake), and on drop only when unwinding.

## Public entry points

- **Syscalls**: `futex`, `futex_waitv`, `set_robust_list`/`get_robust_list`; `futex` on the fast
  path (`syscall::fast`).
- **Kernel-internal**: `exit_robust_list` (thread exit and execve), `pi_owners_exited` (thread
  exit), `Shared::futexes` (the lock and queue).

## Data flow / lifecycle

### Futexes

- Keys follow Linux's `get_futex_key`: private memory, and any memory for an operation with
  `FUTEX_PRIVATE_FLAG`, is keyed by the address space's id and the address (with whether the
  operation was shared, so private and shared operations never meet), whatever page is there now;
  without the flag, shared anonymous memory (shared mappings, SysV shm) uses its host address and
  a shared file mapping (even a read-only one) (device, inode, offset), so processes sharing them
  meet. Kernel wakes (robust list, `clear_child_tid`) are shared operations. An unaligned address
  is `EINVAL`, a shared operation on an unreadable page `EFAULT`. The single waiter list is
  scanned linearly.
- Supported: WAIT, WAKE, REQUEUE, CMP_REQUEUE, WAKE_OP, WAIT_BITSET, WAKE_BITSET, `futex_waitv`,
  LOCK_PI, LOCK_PI2, TRYLOCK_PI, UNLOCK_PI; WAIT_REQUEUE_PI, CMP_REQUEUE_PI and `FUTEX_FD` return
  `ENOSYS` (counted as unimplemented). As in 6.1's `sys_futex`/`do_futex`: the command is
  everything but `FUTEX_PRIVATE_FLAG` and `FUTEX_CLOCK_REALTIME`; the timeout of a command taking
  one is read and checked first (`EFAULT`, `EINVAL`), then `FUTEX_CLOCK_REALTIME` with anything but
  the absolute waits is `ENOSYS`; a zero bitset is `EINVAL`. Wake counts of WAKE, WAKE_BITSET and
  WAKE_OP stop after the first waiter, so 0 and negative counts wake one; REQUEUE and CMP_REQUEUE
  refuse negative counts and wake none for 0.
- WAKE_OP (`futex_wake_op`): the word at the second address must be writable (`EFAULT`; a page
  not written since fork or without an entry is made writable first) and is changed atomically
  (set, add, or, and-not, xor; `FUTEX_OP_OPARG_SHIFT` makes the argument a shift count; another
  operation `ENOSYS` before the change); up to the first count of waiters on the first address
  are woken and, when the old value compares as asked with the 12-bit argument (`EQ`, `NE`, `LT`,
  `LE`, `GT`, `GE`; another comparison `ENOSYS` after the change), up to the fourth argument's
  count on the second.
- Wait: compare and enqueue under the futex lock, then wait alone (no channels); dequeued means
  woken (also when it races a timeout or signal); timeouts are `ETIMEDOUT`; interruptions restart
  (with a restart block when timed).
- `futex_waitv`: no flags, 1 to 128 entries of `FUTEX_32` words with no other flags (each with its
  own private flag), an absolute timeout on `CLOCK_MONOTONIC` or `CLOCK_REALTIME`; it returns the
  index of the first entry woken. `set_robust_list` takes only the 24-byte head.
- Wake removes up to n matching waiters in FIFO order and signals their events after the BKL is
  released (or at once on the fast path). The robust list and `clear_child_tid` at thread exit and
  execve also wake waiters (`clear_child_tid` only when the address space is shared with another
  process); a robust lock word on a copy-on-write page (not written since fork) or a page without
  an entry is made writable first (`cow_break`, `fault_in`), as Linux faults it in.
- PI locks (6.1's `futex_lock_pi`, `futex_unlock_pi`, without priority inheritance: the host
  schedules the threads; BKL path only): the word holds the owner's tid as its own pid namespace
  sees it. LOCK_PI takes a word with no owner (keeping `FUTEX_OWNER_DIED`, adding
  `FUTEX_WAITERS` while PI waiters are queued); the caller's own word is `EDEADLK`; otherwise it
  sets `FUTEX_WAITERS`, then needs the owner to exist (`ESRCH`), and TRYLOCK_PI gives up
  (`EAGAIN`) while a lock queues as a PI waiter until the word is handed to it. A signal restarts
  it (`ERESTARTNOINTR`); the deadline (absolute, `CLOCK_REALTIME` for LOCK_PI, `CLOCK_MONOTONIC`
  for LOCK_PI2 unless `FUTEX_CLOCK_REALTIME`) ends it with `ETIMEDOUT`. UNLOCK_PI needs the caller
  to own the word (`EPERM`) and hands it to the first PI waiter (its tid with `FUTEX_WAITERS`,
  `wake_futex_pi`) or clears it. Plain wakes and requeues leave PI waiters alone.
- When a thread exits (after it leaves the task table), every PI lock with waiters whose owner no
  longer exists goes to its first waiter with `FUTEX_WAITERS` and `FUTEX_OWNER_DIED`
  (`exit_pi_state_list`, `fixup_pi_state_owner`), so a robust mutex reports `EOWNERDEAD`; a waiter
  whose word cannot be written then is skipped until a later exit.
- Lock order is BKL then futex lock; the fast path takes only the futex lock, so BKL and fast paths
  share one queue without lost wakeups.
- On the fast path (the conditions and the deadline hand-over are `kernel/process`'s fast path), a
  wait compares and enqueues under the futex lock and sleeps on its event; dequeued means woken,
  still queued after waking hands over to the BKL path.

## Dependencies (inbound / outbound)

### Inbound

- The syscall dispatcher and the fast path (`kernel/process`).
- Thread exit (`kernel/process`: robust list, `clear_child_tid`, `pi_owners_exited`) and execve
  (`kernel/exec`: robust list, `clear_child_tid`).

### Outbound

- `kernel/process`: `Shared`'s futex lock and queue, wait events, restart blocks and the task
  table for PI owners; `kernel/time`: `Expiry` deadlines and clock-jump handling.
- `kernel/mm`: `futex_key`, `translate`, `cow_break`; `kernel/mmu`: `fault_in`.
- `kernel/ns`: owner tids as their pid namespace sees them; `kernel/signal`: counting
  unimplemented commands (`stub`).

## Invariants and gotchas

- Lock order is BKL, then the futex lock; never the reverse.
- Every kernel write to a futex word (WAKE_OP, the PI commands, robust lists, PI hand-overs at
  exit) goes through `writable_word`, which breaks copy-on-write or faults the page in first,
  since a plain `translate` does neither.

## Confirmed bugs / technical debt

- **PI futexes are partial:** no priority is inherited (waiters are served first come, first
  served, and owners are not boosted); `FUTEX_WAIT_REQUEUE_PI` and `FUTEX_CMP_REQUEUE_PI` are
  `ENOSYS` (glibc since 2.25 and musl do not use them); an execve marks robust PI locks the thread
  holds owner-died with no owner but hands none to its waiters, which sleep until some thread exits
  or another locker takes and releases the word.

## Open questions

- None recorded.

## To verify

- None beyond the full mapping's: the Linux 6.1 comparisons here were checked from knowledge of the
  kernel, not against its source.
