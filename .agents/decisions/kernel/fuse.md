# FUSE (kernel/fuse) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/fuse"`
>
> Last distilled: 2026-11-12 (HEAD = 0553229)
>
> The VFS side of FUSE (the `Node::Fuse` kind, `/dev/fuse` as a file kind, key bits and the sweep's view of mounts) is kernel/vfs's D47 and D51; FUSE becoming a module of its own is knowledge-loop D7.

## Active

### D1: FUSE follows Linux 6.1's `fs/fuse`, with walks reading a node cache that a syscall retry fills

- **What**: Connections in `vfs/fuse.rs` follow Linux's queue, interrupt and abort rules; walks read names, attributes and links from a node cache (`Fuses`) and return `FUSE_RETRY` when they need the daemon, after which `serve()` answers one query and the syscall runs again with a per-syscall token keeping what the retries fetched; file syscalls in `syscall/fuse.rs` send change requests with the BKL released after all of a handler's walks; programs are read into memfds; INIT offers ATOMIC_O_TRUNC, EXPORT_SUPPORT, BIG_WRITES, DONT_MASK, NO_OPEN(DIR)_SUPPORT, PARALLEL_DIROPS, HANDLE_KILLPRIV and MAX_PAGES but not ASYNC_READ; FLUSH is forced; one `O_APPEND` write per node runs at a time; and parents are kept free of loops (`ELOOP`).
- **Why**: sshfs, rclone mount and other FUSE daemons could not run, and Starnix's FUSE depends on Zircon, so Linux 6.1's `fs/fuse` is the model.
- **Tradeoffs**: There is no page cache, so shared mappings never follow writes and shared writable mappings are refused (SQLite's WAL); locks stay local; there is no fuseblk, fusectl, readdirplus or writeback cache; FIFOs, sockets and devices on FUSE cannot be opened; FUSE programs never use set-id bits; and an uncached path of n components takes n retries.
- **Watch out**: A handler that changes state before a walk that can retry repeats the change, code swallowing walk errors can turn `FUSE_RETRY` into "not found", and daemons relying on Linux serializing creations and removals within a directory see them in parallel.
- **Source**: c008749

### D2: Mapped pages are read on demand, and unused nodes are forgotten by a sweep between syscalls

- **What**: mmap of a FUSE file makes a `Fill` (kernel/mm D40) that reads pages through the mapped file's handle as they are touched, before read and write syscalls for their buffers (`prefault`) and in full with `MAP_POPULATE` (shared mappings of `FOPEN_DIRECT_IO` files are `ENODEV`); nodes count lookups and connections count open handles per node, and above 16384 nodes or on `drop_caches` 2 or 3 a sweep at a syscall's end keeps every node that tasks' roots and cwds, fds (`O_PATH` too), running programs, mounts, inotify watches, open handles and requests in flight use, with their directories, and forgets the oldest leaves (down to 12288, or all with `drop_caches`) through `FUSE_FORGET` or `FUSE_BATCH_FORGET` interleaved with requests as 6.1 does; nodes `drop_caches` finds kept only by a `RELEASE` or `RELEASEDIR` in flight lose their names and wait in `Conn::doomed`, and the sweep after that release's answer forgets those nothing keeps any more that are leaves.
- **Why**: Without forgetting, the node cache and the daemon's node table grew with every name looked up until unmount, a mapping read its whole range at once, and a closed file's node kept at `drop_caches` only by its release was forgotten late or never, where Linux evicts it once the release lets go.
- **Tradeoffs**: An unlinked file is not forgotten when it stops being used (Linux forgets it at once), each sweep scans every task and fd table, and the mapped file stays open while mapped, so `FUSE_RELEASE` comes after munmap as on Linux; a doomed node with children waits for them, and a path lookup before its release is answered asks the daemon again, as on Linux once the dentry is gone.
- **Watch out**: Any new place keeping a `Node::Fuse` across syscalls must join `in_use`, or its node can be forgotten (after `drop_caches` too, through `doomed`) and answer `ENOTCONN`; an `O_PATH` fd in flight over `SCM_RIGHTS` is not seen; and above the limit with everything in use, every new entry makes the next syscall's end rescan.
- **Source**: 3957a03, dbfce1d

### D3: A lazily unmounted connection aborts when the VFS collects its last mount

- **What**: The instance aborts in `Vfs::collect` once no attached or detached mount of it remains, which now also happens when the last open file, cwd or root of a lazily unmounted mount goes (kernel/vfs D48), rather than at `MNT_DETACH` or when the last FUSE handle closes.
- **Why**: `fusermount -uz` and `umount -l` of a busy sshfs or rclone mount are common, and the daemon kept running until reboot or an unrelated umount.
- **Tradeoffs**: A running program's executable on the mount does not keep it, where Linux's does.
- **Watch out**: The detached mounts kernel/mount's map lists as outliving their last use (never-attached open_tree clones, mounts whose mount point was removed, holders that release the root mount) keep their daemon running until an unrelated collection.
- **Source**: 922e015

### D4: Releases that find the connection borrowed are queued per thread until the syscall ends

- **What**: An open file's last reference sends its `Released` record (connection, node, release) at once, or, when the connection is borrowed, queues it on the thread for `flush_released` at the end of every syscall and at thread exit, and a queue left when the thread ends is leaked rather than dropped without the BKL.
- **Why**: A release dropped while the connection was borrowed was lost, leaking the daemon's handle and the node's open count.
- **Tradeoffs**: A deferred release reaches the daemon at the end of the syscall rather than at the drop.
- **Watch out**: Drops on threads that never end a syscall or exit through the guest path (none known) would keep their releases queued.
- **Source**: 6d9a9e8

## Superseded

- ~~Mappings are copied whole into memfds, so mmap reads the whole mapped range at once, and nodes are never forgotten (D1's tradeoffs, c008749)~~ → replaced by **D2** in 3957a03 (2026-11-09, no SUPERSEDES declared)
- ~~`drop_caches` leaves nodes kept only by requests in flight to a later sweep (D2 in part, 3957a03)~~ → replaced by **D2** in dbfce1d (2026-11-11)
