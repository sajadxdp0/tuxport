# Advisory locks and leases (kernel/locks) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/locks"`
>
> Last distilled: 2026-11-11 (HEAD = c1a37da)
>
> Lock and lease decisions made before kernel/locks was split from kernel/vfs keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D12, D43 and the lock-range half of D25.

## Active

### D1: kernel/locks is split out of kernel/vfs
- **What**: kernel/locks owns `locks.rs`, `syscall/lease.rs`, `flock` and the fcntl lock commands (`record_lock`): flock, POSIX and OFD locks, leases and their breaks, and the `/proc/locks` and fdinfo listings.
- **Why**: Locks and leases are their own files and Linux's `fs/locks.c`, with their own listings, and the vfs map could no longer be read in one Read.
- **Tradeoffs**: Close paths and lease breaks are called from kernel/vfs, and lock keys of overlay and FUSE nodes come from there.
- **Watch out**: Bugs open in the map are leases lost across a copy-up, no-op locks on inode-less files and the listing order.
- **Source**: 5f327de
