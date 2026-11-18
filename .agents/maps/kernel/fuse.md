# kernel/fuse Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/fuse.md` for the paired decision history.
>
> Verified: 2026-11-09 (full mapping: 3 research concerns; 14 claims checked by 2 independent verifiers — 10 confirmed and 4 partial, all corrected below; the bugs the research and verification found (umount races, parent loops, program copies, interrupts, oversized requests, open ENOSYS, clone ioctl, subtypes, xattr rules, link order, appends, mmap errors) fixed before this line)
>
> Maintained: 2026-11-11 (targeted verification only since the full mapping; latest change: a node drop_caches keeps for a release in flight is forgotten when the release is answered)

## Responsibilities

`kernel/fuse` is FUSE as Linux 6.1's `fs/fuse` has it: filesystems a guest process (the daemon:
sshfs, rclone mount, fuse-overlayfs, any libfuse program) serves through `/dev/fuse`. It provides:

- the connection (`Conn`): mount options, `FUSE_INIT` negotiation, the request queue, requests in
  Linux's wire format matched with replies by `unique`, interrupts, notifications, and aborting;
- the `/dev/fuse` device file: attaching to a connection, read, write, readv/writev, splice, poll
  and the clone ioctl;
- a cache of what the daemon said about nodes, names, attributes and links (`Fuses`), which path
  resolution reads under the BKL, and the retry that fetches what it lacks;
- the file syscalls on FUSE nodes and open FUSE files: open and create, reads and writes,
  directory listing, namespace changes, attribute and xattr changes, statfs, access, fsync,
  fallocate, flushing and releasing, reading mapped pages as they are touched, and copies of
  programs for execve;
- forgetting nodes nothing uses (`FUSE_FORGET`).

Root: `core/kernel/src/vfs/fuse.rs` (connection, device, cache, retries, request helpers) and
`core/kernel/src/syscall/fuse.rs` (the file syscalls on FUSE nodes).

Outside the module: the walker, mount table, fd tables and the dispatch of every file syscall
(`kernel/vfs` and `kernel/files`, which hand FUSE nodes and files to this module), the syscall
dispatcher's retry loop (`kernel/process`), execve (`kernel/exec`), poll and epoll (`kernel/poll`),
the copy mappings (`kernel/mm`), mountinfo (`kernel/procfs`).

## Key types

- `Conn` (`Rc<RefCell<Conn>>`) — one connection (Linux's `fuse_conn` with its input queue): the
  instance's device number and `Options`, the mounter's user and pid namespaces, the INIT state
  (waiting, ready, refused), `aborted`, what INIT negotiated (`minor`, accepted `flags`,
  `max_write`, `max_read`), `no` (operations the daemon answered ENOSYS to), the number of attached
  device files, the next `unique` (even, from 2), the queues `pending` and `interrupts` plus
  `requests` by unique, the `forgets` to send with Linux's `forget_batch`, `opens`, the open
  handles of each node id, and `doomed`, the node ids `drop_caches` kept only for a release in
  flight.
- `Req` — the whole message, its state (`Pending` until read, `Sent` until answered, `Done` with
  the reply or error), whether nobody waits for it (`background`: INIT, RELEASE) and whether its
  caller was interrupted.
- `Options` — `fd`, `rootmode`, `user_id`/`group_id` (kernel IDs), `default_permissions`,
  `allow_other`, `max_read`, `subtype`.
- `DevFile` (`FileKind::FuseDev`) — the connection once attached (None before), the device node it
  was opened through, the opener's user namespace.
- `Fuses` (in `Vfs`) — connections by device number, `Node`s by `FuseId`, the node of each
  (device, daemon node id), and a `clock` counting entries.
- `FuseId` — a node's number, never reused; `Node::Fuse(MountId, FuseId)` in the VFS. `key(id)` is
  `3 << 62 | id`, its number in tables keyed by inode (locks, inotify watches, a program's inode).
- `Node` — device and daemon node id, the directory and name it was last found under (a root's
  parent is itself), the last attributes (`Attr`, owners as kernel IDs) with their validity and
  the syscall that fetched them, the directory's cached names (node, or None for a missing name,
  with validity and syscall), a symlink target read in this syscall, `nlookup` (entries the daemon
  returned for it) and `seen` (the clock when last entered).
- `Entry` — a `fuse_entry_out`: node id (0 for a missing name), attributes, both validities.
- `Query` — what a walk lacked: `Lookup`, `Getattr`, `Readlink`, `Program`.
- `Handle` (`FileKind::Fuse`, or a FUSE `Directory`'s `fuse`) — Linux's `fuse_file`: connection,
  node, node id, the daemon's file handle, the `FOPEN_*` flags, the open flags sent,
  directory or not, and `no_release` when no open was made. Every handle is counted in its
  connection's `opens` (`counted`) until dropped.
- `Fill` (`kernel/mm`) — what a mapping of a FUSE file reads its pages into; it holds the open file.
- `SWEEP`, `DROP` — atomics asking for a sweep of the node cache at the end of a syscall, and for
  dropping every unused node (`drop_caches`).
- `Io` (syscall side) — a copy of a handle's fields, taken so no borrow is held while waiting.
- `SetAttr` — the fields a `FUSE_SETATTR` sets; times as leave, now, or a value.
- `Errno::FUSE_RETRY` (600) — internal: a walk needs a reply; never returned to the guest.

## Public entry points

- Connection and device (`vfs::fuse`): `Options::parse`/`show`, `Conn::new`/`allows`/`abort`,
  `send_init`, `call`, `call_as`, `background`, `dev_read`, `dev_write`, `DevFile` (`new`,
  `attach`, `readiness`, `object`), `atomic_o_trunc`, `dont_mask`.
- Cache and retries: `Fuses` (`add`, `remove`, `root_of`, `conn`, `conn_of`, `get`, `node`,
  `cached_child`, `enter`, `enter_negative`, `forget_child`, `moved`, `is_within`, `set_attr`,
  `stale`, `link`, `set_link`, `evict`), `token`, `new_syscall`, `need`, `serve`, `program`,
  `lookup`, `getattr`, `cached_attr`, `target`, `key`, `id_of`, `drop_caches`.
- Request helpers: `open`, `read`, `write`, `readdir`, `statfs`, `access`, `getxattr`,
  `lock_owner`, `snapshot`.
- File syscalls (`syscall::fuse`): `open`, `create`, `make`, `remove`, `rename`, `rename_at`,
  `setattr`, `read`, `write`, `lseek`, `fill_dir`, `fsync`, `fallocate`, `flush`, `flush_all`,
  `flush_at_exit`, `stat`, `statfs`, `access`, `may_chdir`, `xattr_permission`, `set_xattr`,
  `parent`, `mkdir`, `mknod`, `symlink`, `link`, `unlink`, `truncate`, `ftruncate`, `file_io`,
  `io_of`, `map_fill`, `fill`, `prefault`, `sweep`, `dev_ioctl`, `seekable`,
  `checks_permissions`.
- VFS hooks this module relies on: `Vfs::new_fuse`, `add_fuse_mount`, `fuse`/`fuse_mut`,
  `fuse_child`, `fuse_permission`, `type_of`, `mount_nodes`.

## Data flow / lifecycle

### Mounting and the connection

- Opening the node with device number 10:229 (`/dev/fuse`, 0666 in the boot `/dev` and the
  store's) gives a `DevFile` with no connection: read and write are EPERM, poll only `POLLERR`,
  seeking ESPIPE.
- mount(2) of type `fuse` or `fuse.<subtype>` (6.1's `fuse_get_tree`, `fuse_fill_super`): an empty
  subtype, or a `subtype=` option as well, is EINVAL; options as `Options::parse` (the VFS's
  superblock words skipped, unknown options, bad numbers, a type Linux does not know as
  `rootmode`, unmapped IDs and `blksize` EINVAL). An `fd` that is not open is EINVAL. One already
  attached to a connection mounts that instance again while it exists (ENOTCONN otherwise),
  ignoring the values of the other options and needing none of them. Otherwise `rootmode`,
  `user_id` and `group_id` are required and the file must be a `/dev/fuse` opened in the mounter's
  user namespace (EINVAL).
- A new instance gets its own device number, a connection, and a root node (node id 1, the type of
  `rootmode`, attributes not fetched); the device file is attached and `FUSE_INIT` queued
  (7.37, 128 KiB readahead, the offered flags). The mount does not wait for INIT.
- Offered: `ATOMIC_O_TRUNC`, `EXPORT_SUPPORT`, `BIG_WRITES`, `DONT_MASK`, `NO_OPEN_SUPPORT`,
  `PARALLEL_DIROPS`, `HANDLE_KILLPRIV`, `MAX_PAGES`, `NO_OPENDIR_SUPPORT` (libfuse 3.17 ends the
  session when a filesystem wants an unoffered flag, and fuse-overlayfs always wants
  `DONT_MASK`). Not offered: `ASYNC_READ` (read(2) is not read ahead, and sshfs reads ahead
  itself only when it is not offered), writeback cache, readdirplus, POSIX and flock locks, cache
  invalidation, splice (libfuse decides splice by itself).
- The INIT reply: an error or another major version refuses the connection (every request is then
  ECONNREFUSED); otherwise accepted flags are those offered, pages per request 32 (or the reply's
  `max_pages` clamped to 1–256 with `MAX_PAGES`), `max_write` the reply's within one page and that
  many pages, `max_read` the option's within the same bounds. A short reply from an old daemon
  counts its missing fields as 0.
- mountinfo shows the type with its subtype and the options `user_id`, `group_id` (in the
  mounter's namespace), `default_permissions`, `allow_other`, `max_read` (at least 4096) when
  given. statfs reports `FUSE_SUPER_MAGIC`; `/proc/filesystems` lists `nodev fuse`.
- Aborting (6.1's `fuse_abort_conn`): the last attached device file closing, or `Vfs::collect`
  finding no attached or detached mount of the instance (umount, a dying mount namespace, or the
  last open file, cwd or root in a lazily unmounted one going, `kernel/mount`), which also forgets
  the instance's nodes. Requests not yet answered end with ECONNABORTED, new ones are
  ENOTCONN, the daemon's reads ENODEV and writes ENOENT, poll `POLLERR`.

### Requests and waiting

- `call` waits until INIT has been answered (only a kill stops that), queues the request with the
  caller's filesystem IDs mapped into the connection's user namespace (overflow IDs when unmapped)
  and its thread id in the connection's pid namespace (0 when not visible), wakes the readers and
  waits with the BKL released. `FUSE_ACCESS` carries the IDs of the credentials it is asked for
  (`call_as`): the real ones for access(2) without `AT_EACCESS`.
- Waiting (6.1's `request_wait_answer`): a signal marks the request interrupted; once the daemon
  holds it a `FUSE_INTERRUPT` is queued (at once, or when the daemon reads an already-interrupted
  request) and the wait goes on. A killed thread takes back a request not yet read (EINTR), except
  a forced one (`FUSE_FLUSH`, 6.1's `FR_FORCE`); one already read, or forced, is waited for until
  answered or aborted (after an interrupt), unless the system is stopping, when it is left to the
  daemon and the call is EINTR.
- One wake object per side: every reply wakes every waiter of the connection, every new request
  every reader.
- `background` queues RELEASE and RELEASEDIR with nobody waiting (an open file's last reference
  dropping), unless the connection is refused or aborted; their replies are dropped. When the
  connection is borrowed as the last reference goes, the release and the node's open count wait in
  a per-thread queue (`Released`) that `flush_released` empties at the end of the syscall or at
  thread exit, with the BKL held.

### The device

- read (6.1's `fuse_dev_do_read`): the buffer must hold 8 KiB and a whole write request (EINVAL,
  checked first); then ENODEV once aborted; interrupts for requests still held go first, then
  forgets (all of them while no request is pending, otherwise up to 16 before the next request and
  then 8 requests in a row: `FUSE_FORGET` for one, `FUSE_BATCH_FORGET` for several from a 7.16 or
  later daemon, neither answered, each taking a unique), then the oldest request, which becomes
  `Sent`. A request too large for the buffer ends with EIO (E2BIG
  for SETXATTR) and the next is taken. Nothing to read: EAGAIN when nonblocking, `ERESTARTSYS` on a
  signal, otherwise it waits.
- write (`fuse_dev_do_write`): shorter than a header or a length unlike the write's is EINVAL;
  ENOENT once aborted; unique 0 is a notification; an error must come alone and be a negative errno
  above -512 (EINVAL); then an interrupt's reply must be for a request still held (ENOENT) and a
  bare header (EINVAL), ENOSYS stopping interrupts, EAGAIN queuing it again, any other error
  accepted; a reply to anything but a held request is ENOENT. An ENOSYS reply marks the operation in `no`. The INIT reply is taken by the
  connection.
- Notifications: `INVAL_INODE` drops the node's attributes, `INVAL_ENTRY` and `DELETE` drop the
  name and the directory's attributes, `POLL` and `STORE` are accepted, `RETRIEVE` is ENOSYS.
- read, write, readv and writev of the device move one whole message (up to 8 MiB); splice into a
  pipe takes one whole request (the device's own `O_NONBLOCK`, EIO when the pipe lacks room, the
  request then lost as in Linux), and splice from a pipe takes a reply of the length asked, its
  header's length set to it (`kernel/pipe`).
- poll: always writable, readable while a request or an interrupt for a held request waits,
  `POLLERR` once aborted or unattached.
- `FUSE_DEV_IOC_CLONE` attaches an unattached device file to the connection of another, both
  opened in one user namespace; anything else, a bad fd included, is EINVAL.

### The node cache and retries

- Each syscall gets a new token (`new_syscall`, which also drops the program copies of the last
  one). A cached name, attribute or link is usable while its validity lasts or when fetched by the
  current syscall, however short its validity; a symlink target only within the syscall that read
  it.
- A walk in a FUSE directory reads the cache: `fuse_child` for a name, the cached type for
  `is_dir` and symlinks, the cached parent for `..` (no `..` lookup is ever sent), the cached link
  for a target. What it lacks becomes a `Query` (`need`) and the internal `FUSE_RETRY`; the
  dispatcher then calls `serve`, which asks the daemon with the BKL released and caches the answer,
  and runs the syscall again (at most 16384 times, then EIO). A lookup's ENOENT is cached as a
  missing name for this syscall (a node id of 0 for its validity); any other error of a query ends
  the syscall with it.
- `enter` caches a lookup's or creation's entry: the root's node id is EIO, a known node id keeps
  its node (a directory moves under its new parent, the root never moves), a known node id of
  another type gets a new node, and a directory that would move into itself or below is ELOOP, so
  parents never loop. `moved`
  records a directory renamed through the mount, unless that would loop.
- Changes made through the mount update the cache after the daemon's reply: creations stale the
  directory and enter the new name; removals forget the name and stale the directory and the
  node; renames forget both names, stale both directories and move a directory; writes,
  truncation, fallocate, xattr changes and links stale the node; SETATTR caches its reply.
- Every entry accepted counts a lookup on its node (`nlookup`), even one refused as a loop; the
  root's node id counts none.
- Forgetting (6.1 evicting unused inodes): when an entry takes the cache past 16384 nodes, or a
  write of 2 or 3 to `vm.drop_caches`, `SWEEP` is set and the dispatcher calls `sweep` at the end
  of a syscall. It keeps what `in_use` finds (every task's root and cwd, the node of every open
  file in every fd table, O_PATH ones included, every running program's key, every mount's root
  and point attached or detached, every inotify watch), each connection's roots, nodes with an
  open handle, and nodes a request in flight is about (its node, and a rename's other directory or
  a link's file), plus the directories all of these lie in; it gives up for now when something is
  borrowed. `evict` then forgets leaves (nodes no other node lies in), oldest entered first, round
  after round: all it may with `drop_caches`, otherwise down to 12288. A forgotten node leaves the
  table and the node-id index, the names naming it go with negative entries no longer valid, and
  its `nlookup` is queued as a forget and the readers woken. A node id looked up again is a new
  node.
- A node `drop_caches` keeps only for a release in flight (its file closed, the daemon not yet
  answering `FUSE_RELEASE`/`RELEASEDIR`) loses its names and is doomed: the daemon's answer to that
  release asks for a sweep, which forgets it unless something keeps it again or a lookup found it
  meanwhile (Linux evicts the inode when the release, holding its last reference, ends; its dentry
  is gone and FUSE keeps no unused inode).

### Permission

- `fuse_permission` (6.1's `fuse_permission`): EACCES, root included, unless the mount allows the
  process (with `allow_other`, anyone in the mounter's user namespace or one below; otherwise only
  a process whose real, effective and saved user and group IDs are the mounter's). Then with
  `default_permissions` the cached attributes decide (fetched again when out of date; callers able
  to search everything need none for a search); without it only executing a regular file is
  checked (some execute bit).
- access(2), chdir, fchdir and chroot: the mount's check, then the cached attributes with
  `default_permissions`, otherwise the daemon's answer to `FUSE_ACCESS` (a daemon without it
  allows).
- stat is EACCES for a process the mount does not allow; statfs gives it only the magic.

### Opening and creating

- Open (6.1's `may_open`, `fuse_open_common`): a symlink ELOOP; a directory with write access,
  `O_CREAT` or `O_TRUNC` EISDIR; `O_TRUNC` on a read-only mount or instance (`readonly`) EROFS; a
  device on a nodev mount EACCES; permission; `O_NOATIME` by a non-owner EPERM; a write open of a
  regular file on a read-only mount or instance EROFS;
  writing a program being run ETXTBSY; `O_DIRECTORY` on a non-directory ENOTDIR. Then
  `FUSE_OPENDIR` or `FUSE_OPEN` with the open flags less `O_CREAT`, `O_EXCL`, `O_NOCTTY` (and
  `O_TRUNC` without `ATOMIC_O_TRUNC`, which then is a SETATTR after the open). An ENOSYS answer
  means no open is needed, now and later (no handle, no release). Device nodes, FIFOs and
  sockets in a FUSE filesystem cannot be opened (ENXIO).
- Create (`O_CREAT` of a missing name, 6.1's `fuse_create_open`): EROFS, the directory's write and
  search permission, then `FUSE_CREATE` (mode with the umask applied unless `DONT_MASK`, which
  sends it beside), or from a daemon without it `FUSE_MKNOD` and an open. Without `O_EXCL`, an
  EEXIST (another process created the name since the walk; Linux holds the directory's lock across
  both) forgets the missing name and runs the syscall again, which then opens the file.

### Reading, writing and positions

- read and write go to the daemon in requests of the negotiated size, stopping at a short reply;
  errors after the first piece return what was done. One guest read or write moves up to 8 MiB.
- A file opened `FOPEN_STREAM` has no position (offset 0); with `FOPEN_NONSEEKABLE` or `STREAM`,
  pread, pwrite and lseek are ESPIPE.
- The position is taken as unlocked I/O takes it (`pos_busy`) and advanced past what moved; a
  fault while copying out moves it back.
- A write with `O_APPEND` (as the open file's flags are now) asks the daemon for the size first
  and writes there, one append per node at a time from the size to the write (the node's
  `appending`, waited for on the lock channel, as 6.1 holds the inode lock); `RLIMIT_FSIZE` cuts
  writes (`SIGXFSZ` and EFBIG at the limit).
- lseek: `SEEK_END` from the daemon's size, `SEEK_DATA`/`SEEK_HOLE` through `FUSE_LSEEK` (a daemon
  without it has data up to the end).
- copy_file_range and splice treat FUSE files like regular ones and copy block by block.

### Directories

- getdents64 lists through `FUSE_READDIR` a page at a time on the first call and after a seek to
  0, keeping the daemon's own `.` and `..`; an empty name, one over 1024 bytes or with `/` is EIO.
  Seeking takes only `SEEK_SET` and `SEEK_CUR`.

### Namespace changes

- mkdir, mknod (devices need `CAP_MKNOD` unless 0:0), symlink: `.`, `..` or an existing name
  EEXIST, then EROFS, then the directory's permission; the reply's type must be the one asked
  (EIO). mkdir keeps the permission and sticky bits.
- link (6.1's `do_linkat`): EEXIST, EROFS, EXDEV unless both on one mount, `protected_hardlinks`
  on the cached attributes with read and write permission as the mount decides, the directory's
  permission, EPERM for a directory.
- unlink and rmdir: `.`/`..` rules, EROFS, the lookup, a trailing slash, the directory's
  permission, the sticky rule (with `default_permissions`), the type, a mount point EBUSY; the
  daemon answers ENOTEMPTY.
- rename: EXDEV across mounts, `.`/`..`, EROFS, the lookups, `NOREPLACE` and `EXCHANGE`
  preconditions, both directories' permission and sticky rules, type compatibility, mount points
  EBUSY, a directory under itself EINVAL; then `FUSE_RENAME`, or `FUSE_RENAME2` with flags (EINVAL
  from a daemon without it).

### Attributes and xattrs

- chmod, chown, utimensat, truncate and ftruncate are `FUSE_SETATTR` (with the open file's handle
  when changed through one); EROFS and the mount's check come first, and only with
  `default_permissions` the owner, group and capability rules. truncate and fallocate apply
  `RLIMIT_FSIZE`; fallocate takes only `KEEP_SIZE` and a hole with it (EOPNOTSUPP otherwise or from
  a daemon without it).
- xattrs: `trusted.*` needs `CAP_SYS_ADMIN` (hidden otherwise), `user.*` only on regular files and
  directories, owner-only in a sticky directory, and the mount's permission; `security.capability`
  writes need `CAP_SETFCAP` over the file, other `security.*` writes `CAP_SYS_ADMIN`. Values are
  passed through unconverted; listxattr passes the daemon's list as it is; a daemon without an
  operation gives EOPNOTSUPP.

### Closing, flushing and releasing

- close(2) sends `FUSE_FLUSH` and returns its error; descriptors replaced by dup2, closed by
  close_range or on exec are flushed with errors ignored; at exit the last thread using an fd table
  flushes its FUSE files. A flush is forced: a killed process still waits for it. No flush with `FOPEN_NOFLUSH`, without an open, or from a daemon
  without it. The open file's last reference queues `FUSE_RELEASE` or `FUSE_RELEASEDIR` in the
  background.

### Mappings and programs

- mmap of a FUSE file (`map_fill`) makes a `Fill` (`kernel/mm`): an empty memfd as long as the
  cached size (fetched by a retry when stale) and the open file, kept open until the last mapping
  goes, so the release comes after munmap as on Linux. In do_mmap's order a file not open for
  reading or a shared writable mapping is EACCES and an executable one on a noexec mount EPERM;
  then a file opened `FOPEN_DIRECT_IO` cannot be mapped shared (ENODEV, 6.1's `fuse_file_mmap`).
  A shared mapping is never made writable later.
- Pages are read as they are touched (`fill`: `FUSE_READ` through the mapped file's handle, the
  BKL released): on a guest fault, on a kernel copy failing with `FUSE_RETRY`, before read and
  write syscalls for their buffers (`prefault`), and for the whole mapping with `MAP_POPULATE`
  (errors ignored). How much one fault reads is `kernel/mm`'s (`Fill::window`).
- execve of a FUSE program (or interpreter) reads it whole into a memfd through a retry
  (`Query::Program`, one copy per file per syscall), runs it as from a nosuid mount, and gives it
  its node's key as inode number, so running it and writing it exclude each other (ETXTBSY).

### inotify

- Watches key FUSE nodes by `key`; reads and writes report `IN_ACCESS`/`IN_MODIFY`, opens and
  closes through the open file's record, creations `IN_CREATE`, removals `IN_ATTRIB`, delete-self
  and `IN_DELETE`, renames the moved pair and move-self (a replaced name's delete-self only for its
  last link). Only changes made through this kernel are seen.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/mount`: mount(2) (`fuse_mount`) and collection.
- `kernel/vfs`: the walk (`fuse_child`, `fuse_permission`, kind, readlink, parents) and the FUSE
  branches of getdents, stat, statfs, access, chdir and the attribute and namespace syscalls.
- `kernel/files`: opening `/dev/fuse` and the FUSE branches of open, the I/O routes, readv/writev,
  lseek, the close paths, fsync and copy_file_range; `kernel/pipe` and `kernel/xattr`: those of
  splice and the xattr calls.
- `kernel/process`: the dispatcher's retry loop (`new_syscall`, `serve`), thread exit
  (`flush_at_exit`).
- `kernel/exec`: execve (`open_fuse`, close-on-exec flush).
- `kernel/mm` (`map_fill` for mmap, `fill` from the fault path), `kernel/poll` (device readiness
  and wake object), `kernel/procfs` (mountinfo type and options, `drop_caches`).
- The dispatcher: `fill` and `prefault` for mapped pages, `sweep` after every syscall.

### Outbound

- `kernel/process`: the wait API (`wait_object`, wake objects), `interrupted`, `killed`,
  `stopping`.
- `kernel/mount`: the mount table (`readonly`, `mount_flags`, `is_point_in`, `dev_of`).
- `kernel/vfs`: `resolve_parent`, `encode_stat`; `kernel/files`: `open_mask`, `executing`,
  `check_fsize`.
- `kernel/inotify`: the hooks for FUSE namespace changes.
- `kernel/cred`: `Cred`, user namespace ID maps, `permission`, `owns`, `may_delete`,
  `capable_wrt`.
- `kernel/signal`: `SIGXFSZ` through the VFS's `file_too_big`.
- `hal`: `memfd` for program copies and mapping fills.
- `kernel/mm`: `Fill` (`window`, `store`, `first_unread`, `has_fills`).
- The task table, fd tables, mm `exe`, `Vfs::mount_nodes` and `kernel/inotify`'s registry
  (`watched`) for `in_use`.

## Invariants and gotchas

- A syscall handler may run many times for one syscall: nothing that changes state (a request
  other than a query, a cache update the daemon did not answer for) may come before a walk or
  cached attribute that can need a retry. Every change request here comes after its handler's
  walks, permission and attribute checks.
- Code that swallows walk errors (`.ok()`, `has_child`) must not turn `FUSE_RETRY` into "not
  found" where that changes what is done; `fuse_permission` results must pass it through.
- No borrow may be held across `call` (it waits): handles are copied into `Io`. An umount may
  remove the instance while a request is answered, so after a reply the panicking `Fuses::node`
  is used only behind a fresh existence check: code needing the node fails with ENOTCONN
  (`getattr`, `setattr`, `enter`), cache updates skip a vanished node, and a removal counts a
  vanished child as its last link.
- `enter` and `moved` keep parents free of loops; everything that climbs parents (`reach`,
  `fs_path`, `is_within`, `is_under`) depends on it.
- Uniques are even, so the low bit marks interrupt replies.
- Store inode numbers (below 2^62), overlay keys (top bits 10), FUSE keys (11) and cgroup2 files'
  inotify keys (01) must never collide.
- The type of a node never changes; a reply giving another type is EIO (getattr, setattr) or a new
  node (lookup).
- Nodes are forgotten only between syscalls (`sweep`), so a `FuseId` in a syscall's locals stays
  valid; across a wait it stays valid only while an open handle, a request in flight or one of
  `in_use`'s holders keeps it. Code keeping a `Node::Fuse` somewhere new across syscalls must be
  added to `in_use`.

## Confirmed bugs / technical debt

- **No page cache:** a mapped page keeps what was read when first touched, so a shared mapping
  never sees later writes through the mount or another process, separate mmap calls read
  separately, and a shared writable mapping is refused (SQLite's WAL `-shm` file and programs
  writing through `MAP_SHARED` fail on FUSE); reads and writes always reach the daemon (no
  read-ahead for read(2), writeback or readdirplus).
- **Forgetting is coarser than Linux's:** nodes go only above 16384 or with `drop_caches`, not when
  a file unlinked through the mount is no longer used (Linux forgets it at once); an O_PATH
  descriptor in flight in an `SCM_RIGHTS` message does not keep its node, which then answers
  ENOTCONN; and when nothing can go (everything above the limit in use) every new entry makes the
  next syscall's end scan every task again.
- **Locks stay local:** POSIX, OFD and flock locks on FUSE files work only between processes of this
  kernel; the daemon never sees them.
- **Missing pieces:** no `fuseblk` (ntfs-3g), no fusectl (`/sys/fs/fuse/connections` and its
  `abort`), no `FUSE_IOCTL`, `FUSE_POLL`, `FUSE_COPY_FILE_RANGE`, `FUSE_TMPFILE` or notify
  retrieve; device nodes, FIFOs and sockets on FUSE cannot be opened (ENXIO), and an AF_UNIX bind
  in a FUSE directory is EXDEV (Linux makes the socket with `FUSE_MKNOD`).
- **Programs:** set-id bits and file capabilities of FUSE programs never apply (even on a mount
  without nosuid), `/proc/<pid>/exe` is followed by path, and maps show the node's key as inode.
- **inotify gaps:** path-based chmod, chown, utimensat, truncate, ftruncate and xattr changes
  report no `IN_ATTRIB`/`IN_MODIFY`, an `O_TRUNC` open no `IN_MODIFY`, and creating through the
  MKNOD fallback reports `IN_CREATE` twice.
- **Directory changes run concurrently:** creations, removals and renames in one directory reach
  the daemon in parallel where Linux serializes them under the directory's lock; only the
  create-after-create race is handled (EEXIST without `O_EXCL` opens the file).
- **Retries cost:** an uncached path of n components takes n retries, each walking again from the
  start; any error but ENOENT of a lookup ends the syscall even where the walk would tolerate it.
- **Small fidelity gaps:** fstat asks `FUSE_GETATTR` without the open file's handle; seeking a
  FUSE directory to its end is EINVAL; spliced replies have their header length replaced
  (`kernel/pipe`).

## Open questions

- Would readdirplus or a page cache for read-only mappings be worth their complexity for sshfs and
  rclone workloads?

## To verify

- The Linux 6.1 behaviour cited above comes from knowledge of `fs/fuse` without a source checkout;
  what the `fuse` and `fuse-tools` differential cases exercise was observed on a native 6.8 kernel
  (fuse-overlayfs, sshfs, rclone, mount option errors, `/dev/fuse` before a mount, the clone
  ioctl, ETXTBSY, `RLIMIT_FSIZE`, mapped pages, which nodes `drop_caches` forgets), while
  interrupts, oversized requests, notifications, refused INIT, the `max_read` shown, the forget
  interleaving and `FOPEN_DIRECT_IO` mappings were not (a 6.6+ kernel lets libfuse 3.17 map
  direct-I/O files shared).
