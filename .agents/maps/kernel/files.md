# kernel/files Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/files.md` for the paired decision history (not yet created; decisions from before the split are D5, D11, D17, D22, D24, D26, D27, D29, D33, D34, D41, D44, D45, the owner half of D25, the `open_pseudo` half of D19 and the file I/O halves of D36 and D37 in `.agents/decisions/kernel/vfs.md`).
>
> Verified: 2026-11-12 (split out of the kernel/vfs full mapping of 2026-11-12; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-14 (targeted verification only since the full mapping; latest change: cgroup and device BPF references follow the split into kernel/cgroup)

## Responsibilities

`kernel/files` is what open files are and what is done through them:

- fd tables and open file descriptions, open flags, `fcntl` and the generic ioctls;
- opening a path (the checks and the file each node kind gives), magic-link reopens, file handles
  and `memfd_create`, and each open file's inotify `Opened` record;
- data I/O: regular files through host `pread`/`pwrite` without the BKL, the per-kind read and
  write paths, readv/writev, `sendfile`/`copy_file_range`;
- clones sharing the store's objects, and giving a shared file an object of its own before it
  changes;
- truncate, ftruncate, fallocate, lseek, cache hints and syncing;
- opening the character devices by device number, and their reads, writes and ioctls.

Root: `core/kernel/src/fd.rs` (its `EventFd` and `TimerFd` are described by `kernel/poll`), and in
`syscall/fs.rs` the section Opening, closing, duplicating (except `open_overlay`, `flock`, the fcntl
lock commands, `pipe2`, `open_fifo` and the pipe-size fcntl commands), the section Reading and
writing (except `follow_copy_up`, `copy_up`, the pipe reads and writes and the splice block),
`lseek`, the sections Cache hints and syncing and truncate and fallocate, and the shared helpers
`executing`, `open_for_write`, `check_seals`, `remove_privs` and `truncating`.

Outside the module: path resolution, mount-flag checks, tmpfs accounting, metadata and namespace
syscalls, `getdents64`, `/dev` population and the `Device` numbers (`kernel/vfs`); the mount table
(`kernel/mount`); overlay opens and copy-up (`kernel/overlay`); locks and leases (`kernel/locks`);
pipes, FIFOs and splice (`kernel/pipe`); FUSE opens and I/O (`kernel/fuse`); store objects
(`kernel/store`); socket I/O (`kernel/net`, `kernel/unix`); poll bookkeeping, eventfd and timerfd
(`kernel/poll`); inotify (`kernel/inotify`); native AIO (`kernel/aio`); loop devices
(`kernel/loop`); terminals (`kernel/tty`); synthetic file content (`kernel/procfs`).

## Key types

- `FdTable` — slots (`FileRef` and `cloexec`), reserved fds, the limit (1024 by default; the soft
  `RLIMIT_NOFILE`), `holds_locks`.
- `OpenFile` (`FileRef = Rc<RefCell<OpenFile>>`) — kind, mode and status flags, position and
  `pos_busy` (0 free, 1 taken, 2 with waiters), display path, dentry, mount (the root mount unless
  the open set one), `F_SETOWN*` owner with the setter's uids, `F_SETSIG` signal, poll bookkeeping
  (`kernel/poll`), the inotify `Opened` record, lock ownership, open_tree namespace (`tree`),
  `pseudo` (a memfd: not a writer; seals in `Kernel::seals`), `ovl` and `ovl_holds` (the overlay
  node behind a file opened through an overlay, and its `Hold`s), and old host handles kept until
  close: `ovl_lower` (after a copy-up) and `retired` (after `rehome`).
- `FileKind` — Host, Regular, Directory, Path, Pipe, Device, Pty, Socket, Epoll, EventFd, TimerFd,
  SignalFd, Inotify, Pidfd, MessageQueue, SeccompNotify, Bpf, Proc, Fuse and FuseDev (a FUSE
  directory is a Directory with the daemon's handle). Regular carries `cow`: writable, but the
  handle only reads because the file shares its object, so its first change gives the file an
  object of its own; set at open, by `rehome` and by `follow_copy_up`, cleared by `rehome`.
- `ProcEvent` — change counters given at open to mounts, mountinfo, cgroup2 and sysfs files.
- `IoPath` — the route of one I/O call, chosen so no borrow of the open file is held during I/O
  (`Local`: regular files through `unlocked_io`; other local files, and `O_APPEND` writes to
  regular files, while borrowed under the BKL).
- `Kernel::unsharing` — inodes `make_private` is copying with the BKL released.
- `StoreOpen` — how `open_store` reached the inode: found by path (every check), created by this
  open (none), or a nameless file reopened through a magic link (every check, through its own host
  handle; a memfd never read-only).

## Public entry points

- **fds and I/O**: `openat`, `openat2`, `name_to_handle_at`, `open_by_handle_at`, `close`,
  `close_range`, `dup`/`dup3`, `fcntl`, `lseek`, `read`/`write` and the vector and positional
  forms, `preadv2`/`pwritev2`, `sendfile`, `copy_file_range`, `truncate`, `ftruncate`,
  `fallocate`, `memfd_create`; ioctls `FIOCLEX`/`FIONCLEX`/`FIONBIO`/`FIOASYNC`/`FIONREAD`/
  `FICLONE`/`FICLONERANGE`/`FIDEDUPERANGE`, random ioctls, and the dispatch of other ioctls by kind.
- **Cache hints and syncing**: `fadvise64`, `sync_file_range`, `readahead`, `syncfs`, `cachestat`
  (a 6.5 call), `fsync`/`fdatasync`, `sync`.
- **Kernel-internal**: `create_exclusive`, `unlink_path`, `write_at` (core dumps), `close_replaced`
  (seccomp's ADDFD),
  `FdTable::with_host_stdio` (boot), `FdTable::reserve`/`fill`/`unreserve` (accept4);
  `prepare_write`, `make_private`; `host_unshared` (from `kernel/inotify`'s `host_change`);
  `read_file`, `write_file`, `read_bytes`, `write_bytes`, `write_sync`, `vectored`, `fsync`,
  `has_fsync`, `seekable`, `append_offset` (`kernel/aio`, `kernel/loop`); `check_fsize`,
  `fsize_room` (`kernel/fuse`).

## Data flow / lifecycle

### fd tables and open files

- Allocation takes the lowest slot at or above the minimum that is neither used nor reserved, below
  the table's limit (`EMFILE`; `F_DUPFD` with min negative or at or over the limit `EINVAL`; dup3
  targets at or over it `EBADF`). getrlimit, setrlimit and prlimit share one path, which refuses an
  `RLIMIT_NOFILE` hard limit over `nr_open` (`EPERM`) and sets the soft limit on the tables of all
  the target's threads, so it changes for every process sharing one (Linux checks each process's
  own rlimit). `reserve` takes an fd for a file made later (accept4 while it waits, Linux's
  `get_unused_fd_flags`), and `fill` or `unreserve` ends it: new fds skip a reserved one, dup3 onto
  it (and seccomp's `SECCOMP_ADDFD_FLAG_SETFD`) is `EBUSY`, everything else sees it closed, and
  fork does not copy it. arm64 has no dup2.
- dup and fork share one `OpenFile` (offset and status flags shared). Clone without `CLONE_FILES`
  copies the slots and the limit and resets `holds_locks`. Every thread holds two references to its
  table: `unshare(CLONE_FILES)`, execve (after the other threads are gone) and
  `CLOSE_RANGE_UNSHARE` copy only when there are more; a copy is a new owner of POSIX locks, so an
  unshared table keeps its locks. `unshare(CLONE_FILES)` is also `EMFILE` when the highest fd,
  rounded up to 64, passes `nr_open`. execve then applies close-on-exec.
- Closing (close, dup3 over an fd, close_range, close-on-exec) removes the slot, runs
  `locks::closed`, `mqueue_closed`, flushes a FUSE file (close returns the flush's error, the
  others ignore it) and `unix::released`; the last reference's `Drop` tells `vfs::released` its
  mount (and a pinned PTY's devpts mount), wakes host-socket (not AF_UNIX) waiters, wakes lock
  waiters when the file held locks, and drops the `Opened` record (`IN_CLOSE_*` when any watch
  exists). A table's drop wakes lock waiters when it held locks. A file seccomp's
  `SECCOMP_ADDFD_FLAG_SETFD` replaces in another process's table is closed as that process's (its
  locks, message-queue notifications and FUSE flush; `close_replaced`).
- Open keeps only the access mode, `O_APPEND`, `O_NONBLOCK`, `O_PATH`, `O_DSYNC` and `__O_SYNC`
  (`kept`; with `O_PATH` only `O_PATH`), and every other open adds `O_LARGEFILE`. An `O_PATH` fd
  takes only `F_DUPFD`, `F_DUPFD_CLOEXEC`, `F_GETFD`, `F_SETFD` and `F_GETFL` (else `EBADF`), and
  flock on it is `EBADF`. `F_SETFL` changes `O_APPEND|O_NONBLOCK|O_ASYNC|O_DIRECT|O_NOATIME`
  (registering fasync when `O_ASYNC` turns on for pipes, sockets, PTYs, inotify and host fds; no
  `O_NOATIME` ownership or `O_DIRECT` check). Access mode 3 needs read and write permission but
  makes the file neither readable nor writable (`OPEN_FMODE`). Unknown fcntl commands are `EINVAL`,
  counted as unimplemented paths.
- Owners: `F_SETOWN`/`F_GETOWN` (translated by the dispatcher's `pid_args` in a non-initial pid
  namespace: the argument, -1 included as process group 1, and the result), `F_SETOWN_EX`/
  `F_GETOWN_EX` and, on sockets only, `FIOSETOWN`/`SIOCSPGRP`, `FIOGETOWN`/`SIOCGPGRP` (translating
  themselves). Owners are global ids with the setter's real and effective uid for the delivery
  check (`kernel/poll`); an unknown or invisible number is `ESRCH` (`find_vpid`), 0 removes the
  owner, an owner outside the caller's namespace reads as 0, `F_SETOWN(INT_MIN)` is `EINVAL`.
  fcntl also handles `F_SETSIG`, pipe size and seals.
- memfd_create takes 6.3's `MFD_NOEXEC_SEAL` (mode 0666, sealed with `F_SEAL_EXEC`, sealing
  allowed) or `MFD_EXEC` (0777), not both (`EINVAL`); `MFD_HUGETLB` and names of 249 bytes or more
  are `EINVAL`; the path is `/memfd:<name> (deleted)`. `F_ADD_SEALS` with `F_SEAL_EXEC` on a memfd
  with an execute bit also adds the shrink, grow, write and future-write seals
  (`memfd_add_seals`); `F_SEAL_WRITE` with a writable shared mapping is `EBUSY`.
- Boot: with host stdio, fds 0–2 wrap the host's fds (`FileKind::Host`); with a console, one PTY
  slave description on all three.
- ioctl: `O_PATH` is `EBADF`; then `FIOCLEX`/`FIONCLEX`/`FIONBIO`/`FIOASYNC`, the clone and dedupe
  requests, socket owners and `FIONREAD` on regular inodes (store, tmpfs, procfs, sysfs and nsfs
  files and message queues whose stat is a regular file: the size less the position, as an int,
  negative past the end; not FUSE files) come before dispatch by kind (nsfs, `/dev/fuse`, seccomp
  listeners, terminals, sockets, loop devices, pipe and inotify `FIONREAD`, the random device);
  host fds pass only 15 termios, termios2, window-size and `FIONREAD` requests to the host.
  Anything else is `ENOTTY`, counted as an unimplemented path unless Linux refuses it there too
  (terminal requests on other files, FUSE `FIONREAD` included).

### Opening

- `open_path_with` (openat, openat2, `create_exclusive`): resolve through `kernel/vfs` (openat2's
  rules and `O_CREAT`'s creation loop are in its map); on create, `may_create` (search, `EEXIST`,
  `EROFS`, write+search) and create with the mode `new_owner` makes of the requested one and the
  umask (`kernel/cred`: setgid stripped before the umask, setgid directories); a just-created file
  skips permission checks and truncation. `O_PATH` gives a `FileKind::Path` (`O_DIRECTORY` on a
  non-directory `ENOTDIR`). Only store, overlay and FUSE nodes set the file's mount; others keep
  the root mount.
- Store nodes (`open_store`), unless just created: symlink `ELOOP`; a directory with write access,
  `O_CREAT` or `O_TRUNC` `EISDIR`; a regular file with `O_TRUNC` `EROFS` on ro, else it adds
  `MAY_WRITE`; devices on nodev `EACCES`; permission; the cgroup BPF device filter (not for device
  number 0); `O_NOATIME` by a non-owner without `CAP_FOWNER` `EPERM`; a regular file opened for
  writing on ro `EROFS`. Then always: the `O_DIRECT` rule; `EISDIR` for directories and `ENOTDIR`
  for `O_DIRECTORY` (after the checks above, where `do_open` checks before `may_open`); unless
  just created, writing an executing file `ETXTBSY` and lease breaks; `O_TRUNC` removes set-id bits
  and truncates, with `O_RDONLY` too (through a writable handle of its own), a held file first
  getting an empty object of its own; any other write open of a held file gets a read-only handle
  on the shared object, marked `cow`. Then by type: regular files open the host object, character
  devices by device number (Character devices below), block devices as loop devices (major 7,
  `kernel/loop`; others `ENXIO`), FIFOs the shared pipe (access mode 3 `EINVAL`), sockets `ENXIO`.
- FUSE nodes are opened and created by `kernel/fuse` (the daemon's open and create); a file
  created that way is not checked again.
- Pseudo nodes: nsfs writes `EPERM` first; permission from the synthetic stat, except `/proc/sys`
  (the mode's bits for euid 0, group 0 or others; `net.*` owner bits for `CAP_NET_ADMIN` over the
  network namespace's owner; `user.max_*` writable only with `CAP_SYS_RESOURCE`); `O_TRUNC` asks
  for write; the ptrace read check for `mem`, `auxv`, `environ`, `maps`, `smaps`, `smaps_rollup`
  and `pagemap` (the process found through `pseudo::group_of`, so `/proc/<tid>` works; `mem` and
  `pagemap` of a gone thread `ESRCH`); `setgroups` writes also need `CAP_SYS_ADMIN` over the
  target's user namespace; nodev and the device filter for PTYs; `O_DIRECT` `EINVAL`;
  `EISDIR`/`ENOTDIR`/`EROFS`; `EACCES` for writes the filesystem does not allow except read-only
  procfs files outside `/proc/sys`, which root opens and whose writes fail (`kernel/procfs`).
  Content is generated at open, and the opener's credentials are kept for regeneration and id-map
  writes.
- Magic links to nameless targets — `/proc/<pid>/fd/N` of a deleted or memfd file, a pipe or a host
  fd (`OpenFd`), `/proc/<pid>/exe` of a deleted program (`OpenExe`) — skip the synthetic stat and
  go to `reopen`/`reopen_exe`: a regular file checks seals for `O_TRUNC` with a write mode, then
  gets `open_store`'s checks on the mount it was opened or run through (`StoreOpen::Reopened`,
  through a duplicate of its host handle, or a writable one opened through the host's
  `/proc/self/fd`); pipes reopen a new end and host fds the same fd, with no check. The new
  description takes the original's mount and display path (`/memfd:<name> (deleted)`,
  `pipe:[<n>]`) and records an `Opened`; unlike the memfd it reopens it is not `pseudo`, so it
  counts as a writer (exec `ETXTBSY`) as on Linux. Linked targets resolve to store or overlay
  nodes and get the normal checks.
- File handles: `name_to_handle_at` (`AT_SYMLINK_FOLLOW`, `AT_EMPTY_PATH`, and 6.5's
  `AT_HANDLE_FID` ignored) encodes store nodes only (else `EOPNOTSUPP`) as 8 bytes of type 1 holding
  the inode number, and returns the mount id; over 128 bytes is `EINVAL`, too small `EOVERFLOW`.
  `open_by_handle_at` needs `CAP_DAC_READ_SEARCH` in the initial user namespace (`EPERM`), a size
  of 1 to 128 (`EINVAL`) and a mount fd on a store node; a wrong handle or a missing or unlinked
  inode is `ESTALE`. It opens the inode on that mount (any store mount, with no same-filesystem
  check) through `open_store` with every check, or as an `O_PATH` fd, without open events, display
  path or validation of other open flags.
- Opens through a path of a regular file, device, directory (store, overlay, FUSE, cgroup2) or FUSE
  file record an inotify `Opened` (`kernel/inotify`), watches or not. `O_TMPFILE` is not
  implemented: invalid combinations are `EINVAL` as on Linux (with `O_CREAT`, or read-only), a
  valid one reaches the directory branch (`EISDIR`). `O_DIRECT` (6.1's `direct_IO` check, before
  the file's own open) passes regular files on mounts other than tmpfs, block devices and sockets;
  directories, tmpfs files, character devices, FIFOs and synthetic files are `EINVAL`.

### Data I/O

- read and pread send sockets to `kernel/net` first (even for 0 bytes); otherwise they read in
  pieces of 64 KiB (8 MiB for FUSE files and `/dev/fuse`), looping until the buffer is full only
  for regular files, synthetic files other than `/proc/kmsg`, `/dev/zero`, `/dev/full`, random
  and loop devices; pipes, terminals, host fds and FUSE files return after one piece. write and
  pwrite check mode first, then copy and write a piece at a time, stopping at a short write.
- Without an explicit offset, regular files take `pos_busy` per 64 KiB piece in `unlocked_io`,
  emulating `f_pos_lock`: wait while it is set, take it, snapshot the position, run host
  `pread`/`pwrite` with the BKL released, advance, release. The BKL is kept between pieces, so
  another position-based I/O cannot slip between the pieces of one call (only between the iovecs
  of a synchronous writev). FUSE files take it per 8 MiB piece in `kernel/fuse`, and
  `copy_regular` for its host copy. lseek, `O_APPEND` writes, sendfile's and `copy_data`'s
  rollbacks, copy_file_range's shared path, `read_file`'s fault fix-up, loop devices and synthetic
  files move the position without it.
- write and writev check `RLIMIT_FSIZE` once for the whole call (cut, or `SIGXFSZ` and `EFBIG` at
  or past it), so a piece or iovec ending at the limit only shortens the call. A write at the
  position of a regular file (not `O_APPEND`) takes `pos_busy` before its checks and holds it to
  the end (`write_local`); each piece checks `RLIMIT_FSIZE`, tmpfs room and seals at its position,
  calls `prepare_write` and `remove_privs`, writes, and charges tmpfs. Writes and truncation call
  `remove_privs` (`CAP_FSETID` checked in the initial namespace), which clears setgid only with
  group exec (6.1).
- `O_APPEND` writes run under the BKL at the current host length; `pwrite` on an `O_APPEND` file
  appends too unless `RWF_NOAPPEND` (6.9's flag, honoured only with an explicit offset; FUSE files
  ignore it). preadv2/pwritev2 refuse unknown flags, `RWF_NOWAIT` and `RWF_APPEND` included
  (`EOPNOTSUPP`, before the fd lookup). After each write call `write_sync` syncs a regular store
  file's object (BKL released) for `O_DSYNC` (fdatasync), `O_SYNC` (fsync) or
  `RWF_DSYNC`/`RWF_SYNC`, so writev syncs once per iovec and pwritev2 once more at the end; FUSE
  files, host fds, sendfile, copy_file_range, splice, core dumps and loop writes are not synced.
- A zero-length `read` or `pread` of anything but a socket returns 0 after the fd lookup, before
  the readable, `ESPIPE` and directory checks (zero-length readv and write check first).
- Host stdio fds (`FileKind::Host`): reads poll the host fd (`EAGAIN` when nonblocking,
  interruptible), then read without the BKL; writes run without the BKL, ignore guest
  `O_NONBLOCK`, retry `EINTR` and turn `EPIPE` into `SIGPIPE`; positional I/O is `ESPIPE` even on
  a regular host file, while lseek reaches the host.
- Holes are the host's: `SEEK_DATA`/`SEEK_HOLE` ask the host object (outside the file `ENXIO`; a
  host that cannot tell has data everywhere). lseek on a directory takes `SEEK_SET` and `SEEK_CUR`
  everywhere; `SEEK_END`, `SEEK_DATA` and `SEEK_HOLE` count from the end as the filesystem's
  `llseek` does: store directories end at the largest position (ext4's hashed directories),
  procfs, sysfs and cgroup2 at 0, every other filesystem refuses them (`EINVAL`).
- readv and writev follow Linux's `read_iter`/`write_iter` split: pipes, terminals and sockets
  (and eventfd reads) do one read or write over all iovecs (one read of up to 64 KiB scattered in
  order; writes in 64 KiB pieces across iovecs until one is short; sockets through `kernel/net`),
  so a pipe readv returns what was there and small pipe writes stay atomic; `/dev/fuse` does the
  same in pieces of up to 8 MiB, since a FUSE message moves whole, and a `/dev/kmsg` writev is one
  record. Host stdio fds that are not regular files on the host take the pipe path. Everything else
  goes iovec by iovec and stops at an incomplete one, as Linux does for files with only
  `read`/`write`. Over 1024 iovecs or a total over `isize::MAX` is `EINVAL`; a total of 0 returns
  0 after the offset and mode checks.
- A copy to or from the guest buffer that faults grows a grows-down mapping below it
  (`kernel/mm`) and retries (not for vmsplice's write side or a `/dev/kmsg` writev). A read whose
  buffer faults partway returns the bytes before the faulting page (`EFAULT` when none): regular
  and FUSE files move the position back past what was not copied (`give_back`: never further than
  the read moved it, so FUSE stream files keep theirs), other files keep what they
  consumed (pipe and terminal data, synthetic and loop positions). Writes copy one piece at a time,
  all or nothing, so a fault fails with `EFAULT` only in the first piece (a pipe writev also writes
  the iovecs before the faulting one); earlier pieces stay written and count.
- `sendfile` (input a regular file, a seekable FUSE file or a device that can be spliced from, so
  not `/dev/null`, else `EINVAL`; an output other than a pipe that is `O_APPEND` or cannot be
  spliced into, such as `/dev/full` or an eventfd, `EINVAL` with the input position unchanged,
  as `do_splice_direct`; both ends judged by `kernel/pipe`'s `splice_reads`/`splice_writes`) and
  `copy_file_range` between distinct regular store files use the
  host's sendfile without the BKL (`copy_regular`, capped at `MAX_RW_COUNT`), falling back to
  chunked copies for an `O_APPEND` output, the same inode, a file without a host handle (devices;
  FUSE files, which copy_file_range takes as regular and copies block by block, as 6.1 falls back
  without `FUSE_COPY_FILE_RANGE`), a sealed or tmpfs output, or host
  `EINVAL`/`ENOSYS`/`EOPNOTSUPP`/`EXDEV`. The fallbacks move the input position back over what was
  not written, never further than the read moved it (devices keep their positions), and an error
  after some bytes were copied returns those bytes. `copy_file_range` checks fds, offsets, then
  flags (`EINVAL`), then in `generic_copy_file_checks` order per file (a non-regular input before a
  directory output): directories `EISDIR`, other non-regular files `EINVAL`, modes and `O_APPEND`
  `EBADF`, different devices `EXDEV` (the mounts' device numbers; a memfd counts as the root
  filesystem), wrapping ranges `EOVERFLOW`, then the length cut to the input's end, `RLIMIT_FSIZE`
  and the largest file size (`EFBIG`), overlap in one file `EINVAL`, negative positions `EINVAL`.

### Size changes, cache hints and syncing

- `fallocate` checks offset, length and `EFBIG` before the fd, then `prepare_write`, writability
  (`EBADF`), directories `EISDIR`, pipes and sockets `ESPIPE`, other non-regular files `ENODEV`,
  then the mode: 0, `KEEP_SIZE`, `PUNCH_HOLE` (with `KEEP_SIZE`) and, outside tmpfs, `ZERO_RANGE`
  go to the host, others `EOPNOTSUPP`; then `RLIMIT_FSIZE`, seals and tmpfs room. A host without
  the mode falls back to writing zeros (with the BKL held) and lengthening.
- truncate: `EISDIR`, `EINVAL` for non-regular, `EROFS`, write permission (a synthetic regular file
  then succeeds unchanged), `ETXTBSY`, `RLIMIT_FSIZE`, then leases (6.1 breaks leases before
  `inode_newsize_ok`), seals, `make_private`, unmapping the dropped tail (busy address spaces
  skipped) and the host truncation. A deleted file or memfd reached through `/proc/<pid>/fd/N` is
  truncated itself: its store inode on the mount it was opened through, through the description's
  host handle when the store has no object (a memfd's), a memfd never read-only; ftruncate breaks
  leases for a writable regular file, then `prepare_write`, then `EINVAL` unless writable and
  regular (`EBADF` on `O_PATH`), `RLIMIT_FSIZE` and seals, with no `ETXTBSY` check.
- Cache hints and syncing: fadvise64 is `ESPIPE` on pipes and sockets and `EINVAL` for a negative
  length or unknown advice, else a no-op; sync_file_range passes regular files with flags to the
  host, directories succeed, other files are `ESPIPE`; readahead accepts readable regular files;
  cachestat asks the host for regular files and reports zeros for others. fsync and fdatasync
  (regular files, directories, host fds, FUSE files and loop devices, else `EINVAL`) sync the host
  object with the BKL released (FUSE through the daemon, loop devices their backing file, host fds
  not at all), then commit the store and wait for its WAL; sync and syncfs also sync the whole
  host filesystem holding the store.

### Clones and unsharing

- Before a regular file changes it gets an object of its own if it shares one: a write or host copy
  through a `cow` description, ftruncate, fallocate and a `MAP_SHARED` mmap of a read-write
  description (before mmap's other checks) call `prepare_write`, which acts on `cow` descriptions;
  truncate and `O_TRUNC` of a held file call `make_private` directly; both run before set-id bits
  are removed. `make_private` waits on the lock channel while the inode is in `Kernel::unsharing`,
  asks `kernel/store` to unshare, and when a copy is needed makes it (or what a truncation keeps)
  with the BKL released while the inode is in that set, retrying when the object changed
  meanwhile. Then, when the file was held or a description of it is still `cow`, `rehome` gives
  every description of the file it can borrow (fd tables, AF_UNIX
  messages in flight, loop backing files) a new handle on its current object, read-only and `cow`
  while still shared, retires the old handles and forgets the cached executable; a busy
  description is logged and skipped. Only after a copy, shared mappings move to the new object
  (`move_mappings`, through `kernel/mm`'s `file_moving`/`file_moved`), or all stay when some address
  space is busy. A Files app open that unshares a file only rehomes (`host_unshared`).
- Clones: copy_file_range from offset 0 to offset 0 of a whole non-empty input into an output no
  longer than it, and FICLONE or FICLONERANGE doing the same, share the input's object
  (`try_share`) unless either is not a regular store file, a memfd, opened through an overlay or on
  tmpfs, both are one inode, the input is deleted, open for writing in this kernel by a description
  not marked `cow`, or mapped shared where it may become writable, either is open for writing by
  the Files app (`host_writing`), or the output is mapped (privately too), sealed or being
  unshared; then they copy. The output's set-id bits go first and its descriptions move to the
  shared object (`rehome`); copy_file_range returns the whole length at once and advances the
  positions. FICLONE/FICLONERANGE check in 6.1's order: different mounts (memfds on their own)
  `EXDEV`, directories `EISDIR`, other non-regular files `EINVAL`, modes and `O_APPEND` `EBADF`,
  memfds, overlay and tmpfs files `EOPNOTSUPP` (no remap_file_range), negative or wrapping ranges
  `EINVAL`; length 0 reaches the input's end (nothing at the end, `EINVAL` past it); unaligned
  offsets, an input offset at or past the end, a length cut by the end or `RLIMIT_FSIZE` (at or
  past it `SIGXFSZ` and `EFBIG`) or unaligned short of the end, and overlapping ranges in one file
  are `EINVAL`. Other ranges are copied in `MAX_RW_COUNT` pieces. `FIDEDUPERANGE` checks its
  reserved fields (`EINVAL`), directories (`EISDIR`) and other non-regular files (`EINVAL`), then
  is `EOPNOTSUPP`.
- An overlay copy-up shares the lower file's object too, through `kernel/overlay`'s `copy_up`,
  which copies the data instead when a writable shared mapping or the Files app could write the
  lower object in place, and rehomes the lower file's descriptions open for writing.

### Character devices

- Opening dispatches by device number for store and overlay nodes anywhere and handle opens (device
  nodes in FUSE filesystems are `ENXIO`), after nodev (`EACCES`), permission and the cgroup BPF
  device filter: 5:0 and 5:1 the controlling terminal, 5:2 the ptmx of the devpts beside the node
  (`ENOENT` without a `pts` there, `ENODEV` when it is not devpts or the node's directory is not a
  store directory; the boot devpts when no directory was recorded), major 136 a boot devpts slave,
  1:3/5/7/8/9/11 `kernel/vfs`'s `Device` (`/dev/kmsg`: `EPERM` when devkmsg is off, syslog
  permission to read), 10:237 loop-control and block 7:N (`kernel/loop`), 10:229 `/dev/fuse`
  (`kernel/fuse`); other character devices and block majors `ENXIO`, counted as unimplemented
  paths.
- `/dev/zero`, `/dev/full` and the random devices fill the whole buffer per read; randomness comes
  from the host's `getrandom(2)` with no flags (entropy count fixed at 256; the ioctls follow
  `random_ioctl`, changing ones needing `CAP_SYS_ADMIN` in the initial namespace); writes to the
  random devices are discarded, to `/dev/full` `ENOSPC`.

## Dependencies (inbound / outbound)

### Inbound

- The syscall dispatcher (with `pid_args` for `F_SETOWN`/`F_GETOWN`).
- `kernel/process`: fd tables at clone, prlimit; `kernel/ns`: fd-table copies at unshare;
  `kernel/exec`: execve's fd-table handling.
- `kernel/vfs`: open files and fd tables behind fd-based metadata calls, dirfds and getdents64;
  the open check before releasing an unlinked file's object.
- `kernel/mount`: the fd tables its busy and writer checks scan.
- `kernel/overlay`: `open_store`, `prepare_write`; `kernel/locks`: fd tables, open files,
  `lock_ino`; `kernel/pipe`: file checks and the read and write paths of a splice's other end.
- `kernel/mm`: `prepare_write` for shared mappings.
- `kernel/procfs`: fd links and fdinfo; `kernel/net`: accept4's reserved fds.
- `kernel/poll`: poll/epoll reading open files; eventfd and timerfd reads through the read path.
- `kernel/inotify`: `host_unshared`; `kernel/aio` and `kernel/loop`: the read, write and fsync
  paths; `kernel/fuse`: the fd-table and file-size helpers.
- `kernel/ipc` (queue fds), seccomp (added fds), core dumps, and the machine at boot (host stdio).

### Outbound

- `kernel/vfs`: path resolution and creation (`start_dir`, `lookup_at`, `may_create`), the
  read-only, nodev and other mount-flag checks, the tmpfs hooks, `vfs::released`,
  `display_path`, `Device` and the device numbers opens dispatch on, `file_stat` for `FIONREAD`.
- `kernel/store`: objects, sharing and unsharing, commits and syncs.
- `kernel/inotify` (the hooks, `opened`), `kernel/overlay` (opens through an overlay,
  `follow_copy_up`), `kernel/locks` (fcntl lock commands, `locks::closed`, `break_leases`),
  `kernel/pipe` (pipes, FIFOs, pipe-size fcntl, which files splice at either end for sendfile),
  `kernel/loop`, and `kernel/fuse` (opens and I/O on
  FUSE files, flushes).
- `kernel/cgroup` (device BPF checks, cgroup file-change observation and keys).
- `kernel/procfs` (synthetic opens and content), `kernel/tty` (terminal opens and ioctls),
  `kernel/ipc` (mqueue objects), `kernel/poll` (eventfd and timerfd reads, fasync), `kernel/net`
  (socket I/O and ioctls), `kernel/unix` (files in flight).
- `kernel/mm` for clones, unsharing and truncation: `writably_maps`, `maps_file`, `file_moving`,
  `file_moved`, `file_truncated`.
- Credentials (`permission`, `owns`, `capable_wrt`, `new_owner`, `FileCaps`),
  `pidns`; signals (`SIGPIPE`, `SIGIO`, `SIGXFSZ`, `F_SETSIG`);
  `kernel/signal`'s `stub` for unimplemented fcntl commands and ioctls.
- `tuxport_hal` (pread, pwrite, poll, lseek, fsync, `copy_file`, fallocate, `sync_file_range`,
  `cachestat`, `fill_random`); the kernel's wake objects and `Cx::unlocked`.

## Invariants and gotchas

- Opening walks through `kernel/vfs`'s resolver, which may fail with the internal `FUSE_RETRY`
  (see its map): nothing that changes state may come before a walk that can need it.
- Every file operation keeps `kernel/overlay`'s rules for overlay nodes: no `Node::Store` is ever
  kept for an overlay file's store inode (changes go through a copy-up), every open file naming an
  overlay node holds it, and a syscall that waits while keeping a looked-up node stays out of
  `waits_on_files_only`.
- Every open records an `Opened` (`kernel/inotify`), and file operations added here call its hooks
  where Linux's fsnotify does, passing an overlay node's key (`key_of`).
- A path that changes a store regular file's size or pages must call `kernel/vfs`'s tmpfs hooks
  (`tmpfs_size`/`tmpfs_resized`, the write-room check), or usage drifts until the next full count.
- `kernel/aio`'s `has_iter` and `has_fsync` must follow new `FileKind`s and devices.
- `open_store` skips its checks only for a file this open created (`StoreOpen::Created`); reopens
  through `/proc/<pid>/fd` and `exe` get them, so a check added there must also hold for a nameless
  file opened through its host handle (a memfd has no store object, no mount of the store).
- A held file's object is shared and must never be written in place: every path that changes a
  regular file calls `prepare_write` (or `make_private`) first, with no borrow of the open file
  held, and takes the host fd only afterwards. A `cow` description's handle is read-only, so a
  path that forgets fails with `EBADF` instead of writing other files' data; a description made
  writable before its file became shared is not `cow` and must be rehomed when it does.
- A description's host handle can be replaced (`rehome`, `follow_copy_up`) while I/O without the
  BKL still uses the old fd number: old handles are kept until the description closes.
- `rehome` skips descriptions borrowed at that moment, and a skipped read-only one would keep
  reading the old object, so no code may hold a borrow of an open file across a BKL release or a
  wait (checking builds assert it).
- Unlocked I/O snapshots the position before releasing the BKL and writes it back afterwards, so
  other code that moves the position must take `pos_busy` (`take_pos`/`release_pos`) or accept
  being overwritten; a rollback moves it back only as far as the read moved it (`give_back`).

## Confirmed bugs / technical debt

- **Writes keep a setgid bit chown would clear:** `remove_privs` clears setgid only with group
  exec (6.1), while chown follows 6.2; since 6.2 Linux's writes match its chown.
- **File position races:** lseek and the other paths that move the position without `pos_busy` can
  be overwritten by a piece finishing without the BKL (Linux's lseek takes `f_pos_lock`), and a
  synchronous writev releases the position between iovecs.
- **Fidelity gaps**, one per line:
  - no `O_TMPFILE`;
  - a write open on a read-only instance is `EROFS` only after permission (6.1's `sb_permission`
    comes first; overlay opens already do);
  - `F_GETFL` never shows `O_DIRECT`, `O_NOATIME`, `O_ASYNC`, `O_DIRECTORY` or `O_NOFOLLOW` given at
    open (`F_SETFL` can set the first three);
  - open_by_handle_at and memfds report no `IN_OPEN`, and cgroup2 files no `IN_CLOSE_*`; handles
    open a store inode through any store mount;
  - `RLIMIT_NOFILE` from prlimit applies to the whole shared fd table, and `CLOSE_RANGE_UNSHARE`
    skips `unshare`'s `nr_open` check;
  - sendfile, copy_file_range and splice into an `O_SYNC`/`O_DSYNC` file do not sync it, nor do
    FUSE writes;
  - preadv2/pwritev2 refuse `RWF_NOWAIT` and `RWF_APPEND` (6.1 supports both);
  - read, write, their vector forms and the chunked copies are not capped at `MAX_RW_COUNT`;
  - host stdio: positional I/O is `ESPIPE` on a regular host file, and a blocked write cannot be
    interrupted;
  - FIONREAD on a FUSE regular file is `ENOTTY`;
  - fadvise64 on an `O_PATH` fd succeeds, and sync_file_range on a loop device is `ESPIPE`.
- **Error order in I/O calls:** a zero-length read or pread skips the readable (`EBADF`, `EINVAL`
  for epoll), `ESPIPE` and `EISDIR` checks; pread and pwrite look up the fd before refusing a
  negative offset; fallocate checks its arguments before the fd, its mode after the fd's type, and
  on a socket is `ESPIPE` where 6.1 says `ENODEV`.
- **writev to a synchronous file syncs per iovec** (slower, same durability), and a sync failing
  on a later iovec returns the earlier total, leaving out bytes already written.
- **Private mappings do not follow a file to its own object:** a private mapping made while the
  file shared an object keeps the shared data in pages it has not copied after a write through
  another descriptor. Shared mappings follow, unless an address space was busy then or the Files
  app unshared the file.
- **Clone fidelity:** FICLONE and FICLONERANGE work on a store that reports ext4's magic (ext4
  says `EOPNOTSUPP`); partial ranges are copied, not shared; `FIDEDUPERANGE` is `EOPNOTSUPP`; a
  whole-file copy_file_range returns more than `MAX_RW_COUNT` in one call.

## Open questions

- Should `O_TMPFILE` be implemented?

## To verify

- None.
