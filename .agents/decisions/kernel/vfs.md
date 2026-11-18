# VFS (kernel/vfs) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/vfs"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b). D1–D14 were seeded by hand from the retired design
> doc §8.2–§8.3 and §11.1 and the round records in `docs/rounds/`, so their sources cite rounds;
> later entries cite commits.

## Constraint index (compressed active decisions)

> Full text of D1, D4, D5, D7, D9–D11, D14, D16–D21, D23, D24 and D26–D29: `git show 248d79f637c8f05dadc6ef9fdb0733c568c6a468:.claude/decisions/kernel/vfs.md`
>
> Full text of D25, D30, D31, D33–D38 and D41: `git show 260512d530d136e70f10564e248f7db876f07062:.claude/decisions/kernel/vfs.md`

- **D1** — Nodes carry the mount they were reached through (`Node::Store(mount, ino)` and `Node::Pseudo(mount, node)`, Linux's `struct path`), so one directory mounted twice is two nodes, `..`, getcwd and `/proc/<pid>/{cwd,root,fd}` climb back through their own mount points, and mount ids are never reused (rounds 105, 116)
- **D4** — `/dev` (tmpfs with standard nodes), `/dev/pts`, `/dev/shm` (executable and unlimited for JIT double mappings), `/dev/mqueue` and `/run` are mounted at boot with systemd's options so OpenRC leaves them alone, `/tmp` stays a store directory, and the store's own `/dev` keeps the same nodes underneath (rounds 05, 184)
- **D5** — Opening any character-device node selects its in-process implementation by device number wherever the node is (unknown numbers are `ENXIO`), and nodev mounts and the cgroup BPF device filter must be checked before dispatch (rounds 04, 05, 184; a26c8b4)
- **D7** — Of the new mount API only open_tree, move_mount and mount_setattr exist, following 6.1's `fs/namespace.c` because crun sends every bind through them, while the fsopen family stays `ENOSYS` (callers fall back to mount(2)), idmapped mounts are `EINVAL` and an unattached clone's anonymous namespace is torn down only at the next collection (round 168)
- **D9** — openat2's `RESOLVE_*` flags follow 6.1's `fs/namei.c` inside Tuxport's own walker because Android's app seccomp blocks the host's openat2, `RESOLVE_CACHED` only rejects create, truncate and tmpfile opens, and `exe` counts as a magic link here although it is followed by content elsewhere (round 144)
- **D10** — Mounting over `/proc/<pid>/fd` entries is `ENOENT` as in Linux 6.12, one of the deliberate exceptions to the 6.1 contract to count when deciding on a contract upgrade (round 105)
- **D11** — Regular-file reads and writes release the BKL for host `pread`/`pwrite` behind an `f_pos_lock` emulation (one position-based operation per open file, released between 64 KiB chunks), `O_APPEND` writes stay under the BKL, and position-lock waiters listen on the lock wake channel only (rounds 24, 53, 89)
- **D14** — `protected_symlinks` and `protected_hardlinks` default to 1 and are enforced as 6.1's `may_follow_link` and `may_linkat` do (`protected_regular` and `protected_fifos` are not implemented), and the credential-less no-BKL `Cached` walk must keep handing such links to the normal walk (round 185; 12c4afa)
- **D16** — timerfd parses timespecs through the shared `timespec()` helper (kernel/process D15) and the lease-break deadline clamps at KTIME_MAX, so neither can overflow `Instant` (96cd8bd)
- **D17** — Open keeps `O_DSYNC` and `__O_SYNC` (`KEPT_AT_OPEN`), so `F_GETFL` shows them, `F_SETFL` cannot clear them and `write_sync` syncs after each write (sendfile, copy_file_range and splice still do not), and `CLOSE_RANGE_UNSHARE` copies the fd table only above two references, like `unshare(CLONE_FILES)` (68cafce)
- **D18** — A chown that would clear the set-id bits of a store non-directory needs the owner or `CAP_FOWNER` (`EPERM` otherwise), following 6.2's `setattr_should_drop_sgid` as an exception to the 6.1 contract, while writes still clear setgid only with group exec as 6.1 does (ce7d0d8)
- **D19** — `exe_file` and `open_pseudo` find a pid node's process with `pseudo::group_of`, so `/proc/<tid>` works like its thread group's, and code treating a pid node's id as a thread-group id misses threads (kernel/procfs D12) (173da76, 45be9fae) (20137e4c; d053be24, 39e55b4b)
- **D20** — The VFS's Rust files read in the order their code runs (syscall/fs.rs from opening to its helpers, `impl Vfs` top-down), and code appended at the end of a file drifts from that order (866b1cf)
- **D21** — Loop devices in `Kernel::loops` forward I/O through the backing open file at the device offset with 6.1's loop and block ioctl errors (no page cache, partitions, discard, mmap or mounting), differential cases must wait for native kernels after 6.1 to unbind an autoclear device, and a loop over a loop keeps the lower device open until the upper one is unbound (a26c8b4)
- **D23** — A timerfd keeps an `Expiry` on its clock (kernel/process D19), and with `TFD_TIMER_CANCEL_ON_SET` a later realtime clock jump counts as one expiration and makes the next read return `ECANCELED` once while the timer stays armed (63b9e59)
- **D24** — read_file and readv copy out through `Cx::write_user_partial`, growing a stack below the buffer first, and return the bytes copied (rewinding a regular file's position past the uncopied part) instead of `EFAULT`, though pipe and terminal data past the fault is still consumed (e03c0cc)
- **D25** — F_SETOWN follows Linux's `f_setown` on global numbers (`INT_MIN` `EINVAL`, an unknown number `ESRCH`, 0 removes the owner, the setter's uids kept), owners are translated once by the dispatcher's `pid_args`, and lock ranges use an inclusive last byte in `flock64_to_posix_lock`'s order; syscalls listed in `pid_args` must not translate again (09188ec, 5656ada, f6a82cc)
- **D26** — The vfs ioctl dispatch passes `file.readable()` to `tty::ioctl` so TIOCSCTTY can require read access (kernel/tty D14) (838b5b4)
- **D27** — memfd_create passes the caller's fsuid and fsgid (kernel ids) to the store's `create_anonymous` (kernel/store D14) (907b82b)
- **D28** — linkat follows 6.1: `AT_EMPTY_PATH` without `CAP_DAC_READ_SEARCH` is `ENOENT` (6.10's exception for the opener is not followed), a deleted file reached through `/proc/<pid>/fd/N` links as its store node (memfds stay `EXDEV`), `protected_hardlinks` reads the database row, and the store refuses a new name for an nlink-0 inode (kernel/store D15) (c108be0)
- **D29** — sendfile returns `EINVAL` for an `O_APPEND` output that is not a pipe, after the input checks, as Linux's `do_splice_direct` does (298cadb)
- **D30** — `fs.mount-max` (`Kernel::mount_max`) refuses with `ENOSPC` a mount, a bind with its copies or an attach that would pass it, and whole-table walks index the table once per operation; a loop looking each mount up in the list is quadratic again, and `reach` holds a shared borrow of the table while it calls the store and `Pseudo` (0a4712e)
- **D31** — splice, tee and vmsplice follow 6.1's argument order and nonblocking rules (`SPLICE_F_NONBLOCK` or either end's `O_NONBLOCK`), and a splice into a pipe waits for whole free pages and pushes what it read whole, so a pipe with less than a page free is full to them (5530624)
- **D33** — truncate, ftruncate and `O_TRUNC` unmap a shrinking file's dropped tail in every address space (`Mm::file_truncated`, kernel/mm D5) before the host file shrinks; if the host truncation then fails, accesses past the requested length get `SIGBUS` until remapped (400817d)
- **D34** — Open modes follow `build_open_how` and `OPEN_FMODE`: an `O_PATH` fd keeps only `O_PATH`, access mode 3 neither reads nor writes (a FIFO `EINVAL`), `O_TRUNC` on a directory is `EISDIR` and on an `O_RDONLY` regular file truncates, and a synthetic file opened with `O_TRUNC` needs write permission (5d23297, f6a82cc)
- **D35** — A read-only remount or mount_setattr is `EBUSY` while regular or synthetic files on that mount (without `MS_BIND`, on any mount of the same device) are open for writing or back a loop device; files held only by mappings or in-flight `SCM_RIGHTS` messages do not count (b43ff37)
- **D36** — File syscalls follow 6.1's argument checks and error order (getdents64, copy_file_range with `EXDEV` between devices, `O_PATH` refusals, truncate, `O_DIRECT` only on store regular files outside tmpfs and on loop devices, `O_TMPFILE` validation, renameat2, directory seeks per filesystem), so `O_DIRECT` opens of devices or tmpfs files and copies between tmpfs and the store fail (dcacfb6, 5d7b936)
- **D37** — fallocate, `SEEK_DATA`/`SEEK_HOLE` and `st_blocks` use the host object's holes and allocation (kernel/store D16), falling back to writing zeros where the host lacks a mode, and mode 0 really reserves storage (5d7b936)
- **D38** — `security.capability` changes need `CAP_SETFCAP` over the file's mapped owner and group, setxattr stores the value converted for the writer's namespace and getxattr returns the reader's view (kernel/cred D12); an owner unmapped in the caller's namespace refuses changes with `EPERM` (2aca260)
- **D41** — timerfds and signalfds open `O_RDWR` as `anon_inode_getfd` does, and writes to them are kept out by the write path's `EINVAL`, not by `writable()` (e6bfdf8)

## Active (full detail)

### D2: One global mount table, with the mount namespace kept in the fs context
- **What**: Mount records are stored globally by id and each records its namespace; a task's mount namespace lives in `FsContext` next to root, cwd and umask rather than in nsproxy.
- **Why**: Mount ids are global anyway, and path resolution needed no change because copied mounts have their own ids. `CLONE_NEWNS` cannot be combined with `CLONE_FS`, unshare implies it and setns requires an unshared fs, so tasks sharing an fs are always in the same mount namespace; this also avoided building all of nsproxy first.
- **Tradeoffs**: Unused namespaces, detached mounts and open_tree clones are collected by a scan of every task's directories and fd table, run at `umount2`, at task exit and when a thread lets go of a detached mount's last use (D48).
- **Watch out**: Operations on mounts must check that they belong to the caller's namespace (Linux's `check_mnt`).
- **Source**: round 118 (`118-mount-namespaces`); 922e015

### D3: tmpfs is a detached tree in the store, accounted by the pages its objects take
- **What**: Each tmpfs mount is a new instance backed by a store directory outside the root tree, with its own device number; usage is counted per instance in inodes and in the pages its host objects take, a write or reservation checks room only for the pages it would newly allocate, recounting the instance once first when its usage changed since the last count (`Tmpfs::changed`, `Vfs::tmpfs_recount`), and space is freed at the last unlink.
- **Why**: bubblewrap builds its new root on a tmpfs. A separate in-memory filesystem would need every path and fd syscall, mmap, file locks, inotify and AF_UNIX paths to handle another node kind, and its contents would consume the app's memory (the reason `/tmp` was put in the store); counting file sizes as if files had no holes made sparse files reach `ENOSPC` early, and charges for writes to deleted open files, never given back, made an instance refuse writes it had room for.
- **Tradeoffs**: tmpfs is as slow as the store and writes flash; room checks near a full instance cost host lseeks and count reserved-page slack per file rather than per range; concurrent unlocked writes, writes through shared mappings and truncating a deleted open file can miscount until the next recount, the last letting writes pass the limit.
- **Watch out**: statfs, remount and a refusal after any charge recount the whole tree, which also corrects drift from shared mappings, so a full tmpfs walks the instance once after each successful charge; a host whose `SEEK_DATA` treats reserved space differently could let a write past a full tmpfs or refuse one.
- **Source**: rounds 117 (`117-tmpfs`), 131 (`131-tmpfs-usage`); 5d7b936, 9b99931

### D6: Mount propagation follows `fs/pnode.c`: types, peer groups and mount events
- **What**: Mounts carry Linux's propagation (peer group, master group, unbindable), changed as `change_mnt_propagation` and `do_make_slave` do and inherited by binds, clones and namespace copies; a tree attached under a shared mount is copied to every peer and slave that sees the place (planned by `plan_attach` before attaching, locked in a namespace with another owner), and an unmount takes down the matching mounts of the propagation tree unless locked, `EBUSY` when one is in use.
- **Why**: Ignored types left dockerd's data root mounted until reboot, let unbindable mounts be bound and skipped the shared-mount checks of `MS_MOVE` and pivot_root, and with types alone peers and slaves never saw each other's mounts (19 suite tests skipped).
- **Tradeoffs**: Masters are kept per group rather than per mount, propagated copies are appended in target order so mountinfo lists them differently from Linux, and a move whose targets lie inside the moved tree is not special-cased.
- **Watch out**: Every operation over many mounts must use `Index` and `GroupAlloc`, since large peer groups made a scan per mount hold the BKL past the test limit, and the `at` of a mount on another mount's root lies in the parent mount, which new place-matching code must allow for.
- **Source**: 970cf6a, 66b765c

### D8: Mount flags and cross-mount rules follow Linux 6.1's check order
- **What**: ro is checked per mount and per filesystem instance (`Vfs::readonly`, D53), and noexec, nosuid, nodev and nosymfollow per mount, at the same points and in the same order as Linux; rename and link across mounts are `EXDEV` even between binds of one filesystem; removing or renaming a mount point is `EBUSY` only in the caller's namespace, and mounts on it elsewhere are detached.
- **Why**: Sandboxes (bubblewrap's `--ro-bind`, `--remount-ro`) and LTP's read-only filesystem tests depend on exact errors and ordering (for example `mkdir -p` must still see `EEXIST` on a read-only mount). Treating mount points in other namespaces as busy broke `podman run --rm`.
- **Tradeoffs**: Atime flags are recorded and reported only.
- **Watch out**: New syscalls that create or modify files must call the read-only check (`Vfs::readonly`, not the mount's own flags) at the matching point.
- **Source**: rounds 116 (`116-store-binds`), 168 (`168-open-tree`); f52bde6, 9b99931

### D12: Locks and leases are in-process, keyed by store inode or by overlay or FUSE node
- **What**: `flock`, POSIX record locks (owned by the fd table, released by any close of the file) and OFD locks (owned by the open file) split and merge ranges and detect POSIX deadlocks; leases follow Linux's rules; locks are keyed by the store inode (bind mounts share them), by the overlay node for a file opened through an overlay and by the FUSE node for a FUSE file (`OpenFile::lock_ino`), and leases by the store inode behind the file; nothing reaches host locks.
- **Why**: All guest processes run in one kernel layer, so in-kernel tables give exact Linux semantics; wake-ups go to a dedicated lock channel so unlocking does not wake every poller.
- **Tradeoffs**: Locks are not visible to another kernel or to the Files app sharing the store.
- **Watch out**: Closing a file with locks or leases must request the lock channel.
- **Source**: rounds 08 (`08-m6`), 19 (`19-deferred`), 53 (`53-locks-host-sockets`); 2a244d2, c008749

### D13: inotify is implemented entirely in the kernel layer
- **What**: Syscalls report events after changing the store, at the positions and in the order of Linux's fsnotify hooks; watches are keyed by store inode or by the key of an overlay, FUSE or cgroup2 node (D46, D47, D51); per-fd events use the directory and name the file was opened through; Files app changes are replayed under the target kernel's BKL.
- **Why**: Every filesystem change passes through the kernel layer, so no host notification is needed; Java's WatchService and Node.js's `fs.watch` needed inotify. The early approximations (files opened before a watch, renamed open files, immediate `IN_DELETE_SELF`, unenforced limits) were removed to match Linux.
- **Tradeoffs**: Synthetic filesystems other than cgroup2 accept watches but never report; no fanotify; the provider only reports creates, removes, moves, write opens and writes.
- **Watch out**: Keep the no-watch cost at one emptiness check per event.
- **Source**: rounds 11 (`11-m8-jvm`), 19 (`19-deferred`), 201 (`201-documents-notify`); d7076fc, affb1e2

### D15: kernel/vfs owns resolution, metadata and tmpfs; open files belong to kernel/files
- **What**: kernel/vfs keeps path resolution, mount-flag checks, tmpfs instances and accounting, metadata and namespace calls, getdents64 and device nodes, while kernel/files owns fd tables, opening, data I/O, truncation and syncing alongside the already separate filesystem modules.
- **Why**: The fully refreshed VFS map reached 61.5 KB, beyond one read, and open files form a coherent subsystem apart from resolution and metadata.
- **Tradeoffs**: Boundaries run inside shared files, opens and writes need both maps for mount flags and tmpfs duties, and pre-split decisions retain their original tags and IDs here; device checks and cgroup mkdir/rmdir policy belong to kernel/cgroup while synthetic node adapters stay in kernel/procfs.
- **Watch out**: The confirmed reopen and file-position bugs moved to kernel/files, where 01bfd882 fixes reopens and write-position checks but leaves the lseek race; tmpfs accounting and remaining VFS fidelity gaps stay in this module’s map.
- **Source**: c5236a1, a536142, cc160c1, 5f327de (scope since extended by 3f3db11 and 2a244d2 and narrowed by d2b1dc3; bug list since updated by 3f3db11, 4e64e74, 70c1bc6, ce8a9ce, 2aca260, d9e1f0c, 9b99931 and a36860b); ce193253; 01bfd882; 20137e4c; cf029f9a; d053be24; 39e55b4b

### D22: Vectored I/O on pipes, terminals, sockets and host stdio streams is one operation
- **What**: Following Linux's split between `read_iter`/`write_iter` and plain operations (checked against 6.1), sockets go to kernel/net's `rw` (D13 there), pipe, PTY and eventfd reads do one 64 KiB read scattered over the iovecs, pipe and PTY writes go in 64 KiB pieces across iovecs until one is short, `stream_vectored` sends `tuxport-run`'s host stdio down the same pipe path unless `host_regular` finds a regular file on the host, and positional files and such regular files keep the per-iovec loop.
- **Why**: readv and writev called the file's read or write once per iovec, so message boundaries, pipe readv waits and pipe write atomicity differed from Linux, and on host stdio a filled first iovec waited for more input and a small writev (musl flushes stdout with two iovecs) was not one write.
- **Tradeoffs**: A pipe, terminal or host stdio readv still reads at most 64 KiB per call, and every readv or writev on host stdio costs one host fstat.
- **Watch out**: Borrows of the open file must end before any socket or pipe call that can wait, a file kind whose Linux operations change between plain and iter needs its arm moved, and a host regular file that behaves like a stream would keep the per-iovec loop.
- **Source**: b295644, a36860b

### D32: Inherited mounts are locked, and proc, sysfs and cgroup2 check their namespace's owner
- **What**: Copying a mount namespace into one owned by another user namespace locks every copy (`Mount::locks`: the mount itself and its ro, nodev, nosuid, noexec and atime flags), so umount2, moves and pivot_root refuse a locked mount (`EINVAL`), remount and mount_setattr refuse to clear a locked flag (`EPERM`), and a non-recursive bind or clone of a tree with locked mounts below is `EINVAL`; lazy unmounts and namespace collection keep a locked mount on a parent detached with it, as 6.1's `disconnect_mount` does; proc, sysfs and cgroup2 need `CAP_SYS_ADMIN` over the owner of their pid, network or cgroup namespace, and outside the initial user namespace proc and sysfs also need a fully visible instance, whose locked flags the new mount takes over.
- **Why**: Root of a nested user namespace inside a sandbox could unmount the mounts hiding paths, clear ro/nosuid/nodev/noexec with a bind remount, bind a tree without its submounts or mount a fresh unmasked proc or sysfs, and once mounts were locked a lazy unmount still dropped them from their parent, so an fd kept in the detached tree reached the covered directories.
- **Tradeoffs**: Mounts detached because their mount point was deleted in another namespace still disconnect unlocked children (Linux keeps them connected), every lookup also scans the detached list, and `/sys/fs/cgroup` is the only directory counted as an empty mount point.
- **Watch out**: A detached mount may still have a mount point; rootless container setups that mount sysfs depend on `/sys/fs/cgroup` counting as empty, and `unshare -Urm mount -t proc` without a new pid namespace is `EPERM`, as on Linux.
- **Source**: 6d05aa9, 135cb1e

### D39: overlayfs is a third node kind with instances of its own
- **What**: `Node::Overlay(MountId, OvlId)` and `Fs::Overlay` in `vfs/overlay.rs` keep overlay instances and one node per directory and name (Linux's overlay dentry); reads go to the topmost store inode, open files record their overlay node in `OpenFile::ovl` so `node()`, fd-based changes and `/proc/<pid>/fd` never reach a layer's inode, and an overlay without an upper layer is read-only as a superblock (`sb_readonly`).
- **Why**: Container engines on Tuxport ran on the vfs driver and native snapshotter, which copy every layer and every container's tree, while on Linux they use overlayfs, whose base is read-only multi-layer mounts.
- **Tradeoffs**: Every exhaustive match on `Node` handles a third variant (a fourth since D47), and overlays cannot be layers.
- **Watch out**: `let Node::Store(..) = .. else` sites treat overlay nodes like synthetic ones and may return another errno than Linux; overlay lock keys set the top bit and must never collide with store inode numbers; code that turns an open file's store inode back into a `Node::Store` writes into a layer behind the overlay's back.
- **Source**: 2a244d2 (tradeoffs since changed by d7076fc, 0d82807 and 9590431)

### D40: Overlay copy-up runs in one store transaction per node
- **What**: A change copies its node up in one store transaction (`Overlays::upper_of`), parents first, keeping mode, owner, attributes, times and holes; creations replace a whiteout in the same transaction and make a directory there opaque; `unlink_overlay` and `rename_overlay` follow Linux's check order (merged-directory renames are `EXDEV`); permission is checked on what the overlay shows before copying up; `store_dir` refuses overlay directories so no store path can change a layer; read-only fds move to the upper object on their next read; an instance pins the tmpfs instances its layers are in; origins are kept in memory only.
- **Why**: Container engines need writable overlays (copy-up on change, whiteouts on removal, opaque directories, `EXDEV` for merged-directory renames), and podman's probe (a premade whiteout, then `rm -rf` through the overlay) must pass.
- **Tradeoffs**: The work directory is created but never used; a copied-up file shares the lower one's object only when no host writer or writable mapping requires an immediate copy (kernel/overlay D2); copied-up files change inode number at the next mount, and links made through the overlay show the upper inode's number; writes through an overlay are not counted against a tmpfs upper layer's limits; parents are copied up before a creation's permission check.
- **Watch out**: Code that builds a `Node::Store` from an overlay file's store inode or calls store mutations on a layer bypasses copy-up and whiteouts.
- **Source**: 4b4af75 (since changed by df6ee70 and 0d82807); 01bfd882 (kernel/overlay D2); 20137e4c; d053be24; 39e55b4b

### D42: Pending AIO polls complete outside io_getevents, and io_submit follows 6.1
- **What**: io_getevents waits on its pending polls' files as ppoll would, plus the context's wake object (kernel/poll D16); io_cancel first completes ready polls; the timer thread watches the contexts with eventfd-signalling polls (`Kernel::aio_polls`, kernel/process D11); the resfd eventfd is taken at submission, and releasing a context signals its cancelled polls' eventfds; writes take `append_offset` and `write_sync` as pwritev2 does; io_submit follows `io_submit_one` and the per-command checks, with a `read_iter`/`write_iter` table (`has_iter`) and `has_fsync`; events whose copy faults go back to the queue.
- **Why**: Pending `IOCB_CMD_POLL`s completed only inside io_getevents on their context, so a resfd eventfd watched with epoll never fired and polls on host sockets or timerfds were seen late or never; AIO writes also ignored `O_APPEND` and the `RWF_*` sync flags, a faulting copy lost events, and io_submit queued requests 6.1 refuses.
- **Tradeoffs**: An eventfd-signalling poll completes when the timer thread next runs rather than inside the readying write, and synthetic files are all taken to have `read_iter` and `write_iter`, which `/proc/<pid>/mem` and pagemap do not.
- **Watch out**: io_getevents sleeps on the context object, so a path that queues an event without requesting it leaves waiters asleep; `has_iter` must follow new file kinds, and its `/dev/kmsg`, loop-control, seccomp and bpf entries were not checked natively.
- **Source**: fffe1c1

### D43: Lock listings come from the lock tables
- **What**: `FlockHolder` and `Lease` record their taker's tgid, and `locks.rs` formats `/proc/locks` and fdinfo `lock:` lines as `lock_get_status` does, with fstat's device and inode (an overlay node's shown inode) and inclusive ranges, numbering `/proc/locks` by position without invisible pids and fdinfo per open file.
- **Why**: Both listings need each lock's taker, file and range, which belong with the lock tables as in Linux's `fs/locks.c`.
- **Tradeoffs**: Entries come by kind and inode instead of newest first, `F_SETLKW` waiters are not listed, and a POSIX lock shows under the open file of its table's lowest fd on the file.
- **Watch out**: Listing scans fd tables for POSIX locks, which is cheap only while few locks are held.
- **Source**: 39ea702

### D44: FIONREAD answers for regular files before the per-kind dispatch, and misses are counted
- **What**: FIONREAD on a regular, procfs or message-queue file whose stat is `S_IFREG` returns the size less the position as an int before the per-kind ioctl dispatch, host fds pass it to the host, and names the walk does not find, the fcntl and ioctl default arms and devices without a driver are counted as unimplemented paths (kernel/signal).
- **Why**: FIONREAD on a regular file was `ENOTTY` where Linux's `do_vfs_ioctl` answers it, and misses under `/proc`, `/sys` and `/dev` and driverless devices had nowhere to be counted.
- **Tradeoffs**: FIONREAD on a regular file costs a stat, FIOQSIZE stays unimplemented, and an `O_CREAT` walk that misses at the root of `/dev` counts the name it then creates.
- **Watch out**: Stat sizes that differ from Linux now show in FIONREAD too (sysfs reports 4096).
- **Source**: 99b4216

### D45: Clones share store objects until a file is written
- **What**: FICLONE, FICLONERANGE and whole-file copy_file_range share the store's object through `try_share` (copying instead for memfds, overlay and tmpfs files, deleted sources, sources with a writer not marked `cow` or a writable shared mapping, mapped, sealed or unsharing destinations, and objects out for host writing by the Files app); every change calls `prepare_write` or `make_private` first, copying with the BKL released, and `rehome` moves every description to the new object, also when a Files app open makes a held file private (`host_unshared`).
- **Why**: The guest's clone calls must share, every change must unshare first, every description must follow its file, and `try_share` had checked only the kernel's own writers.
- **Tradeoffs**: Private mappings, and after a Files app unshare shared mappings too, stay on the old object; FICLONE succeeds on a filesystem reporting ext4's magic, FIDEDUPERANGE stays `EOPNOTSUPP`, copy_file_range can return more than `MAX_RW_COUNT`, and a source with a writer is copied.
- **Watch out**: Any new path that changes a regular file must call `prepare_write` first with no borrow of the file held, and a description borrowed during `rehome` keeps its old handle (logged).
- **Source**: df6ee70, 1f99a1c

### D46: inotify watches overlay nodes by their keys and reports through the overlay
- **What**: Watches on overlay nodes key by `overlay::key`, hooks take a node's or an open file's key (`key_of`, `file_key`), opens, creations, links, removals, renames, attribute changes and truncation report to overlay keys, creations also report `IN_CREATE` to the upper directory, and fdinfo shows the inode and device stat shows.
- **Why**: Watches on overlay nodes never reported, so LTP inotify07 and inotify08 failed and programs watching container roots on overlayfs saw nothing.
- **Tradeoffs**: Watches on a layer's own files see only the creations the overlay makes there, and a watched file's last unlink after a hard link made through the overlay reports `IN_DELETE_SELF` where Linux's overlay never does.
- **Watch out**: A hook still given the store inode behind an overlay node reports to the layer, where no overlay watch sees it, and every overlay open allocates an `Opened` record.
- **Source**: d7076fc

### D47: FUSE nodes are a fourth node kind with FUSE branches in the VFS
- **What**: `Node::Fuse(MountId, FuseId)` and `Fs::Fuse` give FUSE mounts instances of their own, the walk returns `FUSE_RETRY` when it needs the daemon, `/dev/fuse` is `FileKind::FuseDev` with whole-message reads, writes and splices, FUSE files are `FileKind::Fuse`, `collect` aborts instances without mounts, FUSE keys take the top two bits for locks and inotify, and `Vfs::mount_nodes` and `Registry::watched` let kernel/fuse's sweep see every mount's nodes and watch.
- **Why**: FUSE nodes have no store inode and their content comes from a daemon, which libfuse drives with read, writev and splice on `/dev/fuse`.
- **Tradeoffs**: Every exhaustive match on `Node` and `FileKind` grows, a spliced reply's length rewrite accepts what Linux rejects when the pipe holds less, path-based attribute changes on FUSE nodes report no inotify events, and each sweep enumerates the holders.
- **Watch out**: Code that swallows walk errors or changes state before a walk breaks the retry, store, overlay and FUSE keys must never collide, and a new long-lived holder of `Node` values must join the sweep's scan.
- **Source**: c008749, 3957a03

### D48: Detached mounts go when their last use does
- **What**: Each thread records the mounts of the open files, cwds and roots it lets go of (`vfs::released`, up to 16 and then "any", from an open file's drop and `FsContext`'s `set_cwd`, `set_root` and drop), and the dispatcher's `collect_released` and thread exit's `collect_unused` collect when one of them is detached, scanning in-use mounts with `try_borrow` and retrying later.
- **Why**: Detached mounts were collected only by `umount2` and dying namespaces, so a lazily unmounted FUSE mount kept its daemon, and unused open_tree clones and tmpfs trees lingered.
- **Tradeoffs**: With no reference counts each collection scans every task's directories and fd table, a syscall closing a file in a still-used detached mount pays that scan, and releases on non-guest threads wait for another collection.
- **Watch out**: A new holder counted by `in_use` that does not report its release leaves its mount behind until the next collection, as holders that release `ROOT_MOUNT` instead of their node's mount, never-attached open_tree clones and mounts detached by removing their mount point already do (kernel/mount's map), and the per-syscall check must stay a thread-local test when nothing was released.
- **Source**: 922e015 (Watch out since corrected after 49bb4f6's kernel/mount Decision)

### D49: Removed overlay nodes are swept once nothing can use them
- **What**: Removed overlay nodes become orphans stamped with the syscall count, and once 1024 have gathered (then twice those left) the end of a syscall frees each orphan whose subtree no process root or cwd, mount, inotify watch or `Hold` (taken by open files and running programs) names and that no syscall another thread began before its removal may still use, unless that syscall waits only on files it holds (`waits_on_files_only`).
- **Why**: Overlay nodes lived until the instance's last mount was collected, so memory grew by one node per name ever removed in a long-running container.
- **Tradeoffs**: Nodes of names that still exist stay while mounted, orphans wait for long path-based syscalls in other threads, `Hold` takes a global mutex at each overlay open and close, and a node number held in another kernel of the process keeps this kernel's node of that number.
- **Watch out**: A new holder of overlay nodes that neither takes a `Hold` nor joins the sweep's scan, or a node-holding wait added to `waits_on_files_only`, lets a node go while used and makes its next lookup panic.
- **Source**: 0d82807, 9590431

### D50: `system.*` xattrs follow Linux's POSIX ACL rules without enforcing them
- **What**: Setting `system.posix_acl_*` follows `posix_acl_xattr_set` and `set_posix_acl` (other `system.*` names `EOPNOTSUPP`, owner or `CAP_FOWNER`, validated, default ACLs only on directories), a base-entries access ACL becomes a chmod, other ACLs are stored with kernel IDs and shown in the reader's namespace, and the walker asks a synthetic view for its search check (`Pseudo::searched`).
- **Why**: `system.*` was stored raw with no permission check, so anyone who could resolve a file could set or remove its ACLs or invent other `system.*` names.
- **Tradeoffs**: Stored ACLs with named entries or a mask grant and restrict nothing, the mode does not take the mask, and new files do not inherit default ACLs.
- **Watch out**: Programs that set named ACL entries believe access changed, and ACLs stored raw by older versions are read back through the conversion as they are.
- **Source**: f5c09f7

### D51: inotify keys give each node kind its own top bits, and cgroup2 files report
- **What**: Store inode numbers stay below 2^62, overlay keys take top bits 10, FUSE keys 11 and cgroup2 nodes 01 (`cgroup::key`), every `of_key`-style check tests both bits, cgroup2 nodes are never taken as unlinked, and `mounts_changed` bumps the mount event counter and wakes its pollers.
- **Why**: inotify keys had no room for synthetic files, and `overlay::of_key` took FUSE keys for overlay nodes.
- **Tradeoffs**: Store inode numbers must stay below 2^62.
- **Watch out**: A new key kind must take a free bit pattern, and every check must keep testing both bits.
- **Source**: affb1e2

### D52: Exec seals, pivot_root lookups and deleted mount roots follow Linux
- **What**: chmod refuses a change of execute bits on a memfd sealed with `F_SEAL_EXEC` as 6.3 does (memfd_create refuses the MFD exec flags, and the exec seal on an executable memfd adds the write seals), pivot_root looks each path up as a directory in turn, and removing the name a mount's root is marks the mount (`roots_removed`) so mountinfo shows that root with `//deleted`.
- **Why**: A sealed memfd could be made executable again, pivot_root answered `ENOENT` for a file new_root with a missing put_old, and mountinfo showed `/` as the root of a bind whose source was removed.
- **Tradeoffs**: The exec seal stays a 6.3 feature on a 6.1 contract, a deleted root's path comes from its former parent, and a root reached without a recorded name is marked only once it has no links left.
- **Watch out**: A new way of removing a name that skips `detach_points` leaves its mounts' roots looking alive in mountinfo.
- **Source**: 5760479

### D53: Filesystem instances have superblock flags of their own
- **What**: `Mounts::sb_flags` keeps read-only, sync, dirsync, mand and lazytime per instance by device number, set when an instance is created (`new_superblock`) and by a remount without `MS_BIND` (all but dirsync), so `readonly` covers every mount of a read-only instance (an overlay without an upper layer included) and write opens check it instead of the mount's own flags, mountinfo, `/proc/<pid>/mounts` and statfs report them, and a read-only remount checks writers on every mount of the instance only when the instance itself turns read-only.
- **Why**: Read-only was a flag of one mount, so a read-only tmpfs turned writable through a bind remount, binds did not inherit a read-only instance, and mountinfo always said rw; write opens still read the mount's flags afterwards, so a rw bind over an instance remounted read-only could write and truncate.
- **Tradeoffs**: sysfs, cgroup2 and mqueue mounts reuse an instance and ignore the flags they are mounted with, and sync, dirsync, mand and lazytime are only recorded and reported.
- **Watch out**: A read-only remount of `/` without `MS_BIND` makes the whole store read-only for every mount of it, as on Linux, a new way of creating an instance must call `new_superblock`, and a write check reading a mount's own flags instead of `Vfs::readonly` misses read-only instances.
- **Source**: f52bde6, 9b99931

### D54: A splice out of a pipe holds off the pipe's other consumers until its write ends
- **What**: A `Splicing` guard sets `Pipe::splicing` around the write of a splice out of a pipe and clears it with a wake, and pipe_read, pipe_to_pipe's source and other splices out of that pipe wait for it (`wait_splice_done`) whatever their `O_NONBLOCK`, as Linux's pipe lock makes them, while the drain after the write is clamped behind a debug assertion.
- **Why**: A splice from a pipe into a file, socket or FUSE file wrote first, possibly releasing the BKL, and drained the bytes only afterwards, so a concurrent reader of the pipe made it remove newer data or panic, killing the process.
- **Tradeoffs**: A nonblocking reader blocks while a splice writes, as on Linux, but closing an end does not wait for it, where Linux's close takes the pipe lock.
- **Watch out**: A new path taking data out of a pipe must call `wait_splice_done` first or the late drain removes bytes it never wrote, and a FUSE daemon reading the pipe that a splice into `/dev/fuse` comes from deadlocks, as on Linux.
- **Source**: 9b99931

### D55: An fd can be reserved before its open description is installed

- **What**: `FdTable::reserve`, `fill` and `unreserve` keep allocations away from pending slots, make explicit replacement return EBUSY, hide reservations from other operations and discard them on clone.
- **Why**: accept4 needs Linux’s separation between choosing an fd and installing it after a blocking accept.
- **Tradeoffs**: Allocations scan a normally tiny reservation list, and `fill` ignores a subsequently lowered limit.
- **Watch out**: Every replacement must use `install_at`; musl dup2 can retry EBUSY indefinitely, and this pre-split decision now concerns kernel/files.
- **Source**: 4e01f78b; 20137e4c; d053be24; 39e55b4b

### D56: Bind-root traversal and tmpfs owner options respect their boundaries

- **What**: `dotdot` returns ENOENT outside a bind root, and tmpfs mount/remount maps uid/gid options through the caller’s user namespace, rejecting unmapped IDs.
- **Why**: Renaming directories out of a bind subtree escaped its walk boundary, and tmpfs options used host-side IDs.
- **Tradeoffs**: Each parent traversal under a non-root bind walks the store’s parent chain.
- **Watch out**: Deep bind trees pay for the walk, and previously accepted unmapped owner options now fail with EINVAL.
- **Source**: 01bfd882; 20137e4c; d053be24; 39e55b4b

## Superseded

- ~~There are no block devices or loop devices (D5's tradeoff, rounds 04, 05, 184)~~ → replaced by **D21** in a26c8b4 (2026-11-06)
- ~~F_SETOWN/F_GETOWN owners translated by the owner helpers (09188ec)~~ → replaced by **D25** in 5656ada (2026-11-07)
- ~~An unknown F_SETOWN pid is accepted where Linux returns `ESRCH` (D25's tradeoff, 09188ec, 5656ada)~~ → replaced by **D25** in f6a82cc (2026-11-08)
- ~~tmpfs counts usage by file size, treating files as having no holes (D3's What and tradeoff, rounds 117, 131)~~ → replaced by **D3** in 5d7b936 (2026-11-08)
- ~~Mount propagation types are validated and then ignored, so every mount behaves as private (no `shared:N`, `MOVE_MOUNT_SET_GROUP` is `EINVAL`), and remount changes only the one mount's flags except tmpfs size options (D6, rounds 105, 116, 168)~~ → replaced by **D6** in 970cf6a and 66b765c (2026-11-09); the remount clause by **D53** in f52bde6 (2026-11-10, no SUPERSEDES declared)
- ~~Propagation types are recorded but mount events do not propagate (970cf6a)~~ → replaced by **D6** in 66b765c (2026-11-09)
- ~~Unused namespaces and detached mounts are collected only at `umount2` and task exit (D2's tradeoff, round 118)~~ → replaced by **D48** in 922e015 (2026-11-09, no SUPERSEDES declared)
- ~~Synthetic filesystems never report inotify events (D13's tradeoff, rounds 11, 19, 201)~~ → narrowed by **D51** in affb1e2 (2026-11-09, no SUPERSEDES declared): cgroup2 files report
- ~~Overlay nodes are kept until the instance's last mount is collected (D39's tradeoff, 2a244d2), and memory grows with the overlay nodes looked up while mounted (D40's watch-out, 4b4af75)~~ → replaced by **D49** in 0d82807 (2026-11-09)
- ~~inotify on overlay nodes never reports (D39's tradeoff, 2a244d2)~~ → replaced by **D46** in d7076fc (2026-11-09, no SUPERSEDES declared)
- ~~`/proc/<pid>/exe` of a program in an overlay is followed by path with no `ETXTBSY` for its writers (D39's tradeoff, 2a244d2)~~ → changed by 9590431's kernel/procfs and kernel/process Decisions (2026-11-09, no SUPERSEDES declared)
- ~~Copy-up copies data with the BKL held, so large copy-ups stall every process (D40, 4b4af75)~~ → replaced by **D45** in df6ee70 (2026-11-08, no SUPERSEDES declared)
- ~~Mounts, fds, file syscalls, pipes, locks, devices and inotify are one kernel/vfs module (D15, c5236a1)~~ → replaced by **D15** in d2b1dc3 (2026-11-10)
- ~~Host stdio fds stay per-iovec (D22's tradeoff, b295644)~~ → replaced by **D22** in a36860b (2026-11-10, no SUPERSEDES declared)
- ~~kernel/vfs also covers inotify, native AIO, loop devices and timerfd (D15, c5236a1, a536142)~~ → replaced by **D15** in cc160c1 (2026-11-11)
- ~~kernel/vfs also covers overlayfs, locks and leases, pipes and xattrs, overlayfs having stayed when inotify, AIO and loop devices left (D15; cc160c1)~~ → replaced by **D15** in 5f327de (2026-11-11)
- ~~D15’s ownership of fd tables, open files and file I/O~~ → replaced by **D15** in ce193253 (2026-11-12)
- ~~D40’s unconditional sharing of a regular file’s lower object after copy-up (df6ee70)~~ → replaced by **kernel/overlay D2** in 01bfd882 (2026-11-12, cross-module SUPERSEDES)
