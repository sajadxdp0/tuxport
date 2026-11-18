# kernel/store Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/store.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 4 research concerns; 15 claims checked by 2 independent verifiers — 7 confirmed and 8 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-10 (targeted verification only since the full mapping: boot cleanup numbers; browse writes refusing the root as `top`; memfd owners; no links to unlinked inodes; host block counts; shared objects; host writers; boot mounts in kernel/mount last)

## Responsibilities

`kernel/store` is the guest root filesystem's backing store, the "inode store": an SQLite database
holding the whole namespace and non-data metadata, plus one host file per regular file. It
provides:

- namespace and metadata operations (lookup, stat, create, link, unlink, rename, xattrs, times,
  tmpfs trees, memfd-style anonymous files);
- regular files sharing one object until one of them is changed (the whole-file reflink behind
  FICLONE, copy_file_range and overlay copy-up);
- atomic operations, deferred commits, and power-loss ordering that mimics ext4;
- an in-memory metadata cache readable without the BKL;
- sharing one store between the kernel and host-side callers (the app's DocumentsProvider);
- importing a rootfs tarball, and a path-based browse API for the host.

Root: `core/kernel/src/vfs/store.rs`, `vfs/import.rs`, `vfs/browse.rs`, and the host wrappers in
`core/kernel/src/lib.rs` (`import_rootfs`, `sync_store`, `browse`, `browse_changing`,
`write_file`, `read_file`).

Outside the module: path resolution, the no-BKL resolver and namespace and metadata syscalls
(`kernel/vfs`), file I/O and moving open files to an unshared object (`kernel/files`), mounts
(`kernel/mount`),
synthetic filesystems (`kernel/procfs`), the BKL and fast path (`kernel/process`), the Android
provider and rootfs installation UI (`android`).

## Key types

- `Store` — one SQLite connection with transaction state: pending deferred transaction, `writing`
  flag, `doomed` objects to delete after commit, `replaced` files to flush before commit, the
  waker, the read-snapshot state, the owning kernel id, changed directories.
- `Meta` — state shared with readers and the reaper thread: the cache behind an `RwLock`, the
  commit counter, the reaper queue, the WAL path and durability watermark.
- `Stat`, `DirEntry`, `FileTime` (`Now` or `At(ns)`), the `mode` constants; `Ino = u64`, `ROOT = 1`.
- `Commits` — 32 bytes of the `lock` file mapped shared: commit counter, number of open
  connections (`users`), and the range of inode numbers allocated by uncommitted transactions.
- `Cache` — dentries (including negative entries), database rows with where each regular file's
  data and times are (`Data`: the object's number, and the access time while the file is held),
  symlink targets, `..` parents, names (for getcwd), directory listings, and the set of inodes with
  file capabilities.
- `Unshare` — a held file's private copy in the making: the shared object open for reading, the
  new one open for writing, the length to copy, and both numbers.
- `Fill` — an entry created by an operation, inserted into the cache only if the operation
  succeeds (at most 64 per operation).
- `Op` — savepoint guard; rolls back and discards fills on failure or unwinding.
- `Reader` — a read-locked view of the cache for threads without the BKL.
- `Reaper` — the background thread that deletes objects and checkpoints the WAL.
- `ImportReport`; `browse::Change` (created, removed, moved, opened, written), `browse::Entry`.

## Public entry points

- **Lifecycle**: `Store::create(dir)` (new store, `EEXIST` if one exists), `Store::open(dir,
  volatile)`, `Drop` (final commit, reaper join), `guard_with_bkl`.
- **Reads**: `lookup`, `name_of`, `stat`, `stat_open`, `attrs`, `kind`, `readlink`, `parent`,
  `readdir`, xattr reads, `has_file_caps`, `fs_stats`; `snapshot` (one read transaction);
  `Meta::reader` (no-BKL).
- **Objects**: `open_object`, `open_object_append`, `truncate` (each makes a held file private
  first when it writes), `copy_object` (data with holes).
- **Shared objects**: `share`, `held`, `unshare` with `finish_unshare` or `abandon_unshare` (the
  copy in between is the caller's, so the kernel can release the BKL), `make_private` (all three
  in one, the BKL held), `release`.
- **Changes**: `mkdir`, `create_file`, `create_anonymous`, `symlink`, `mknod`, `link`, `unlink`,
  `rmdir`, `rename` (no-replace, exchange), `create_detached_dir`/`remove_detached_dir`,
  `set_mode`, `set_owner`, `set_owner_mode`, `remove_privs`, xattr writes, `set_mtime`,
  `set_times`, `set_times_open`; `transaction`.
- **Durability**: `commit`, `commit_due`, `set_waker`, `Meta::sync_metadata`, `Meta::sync_all`.
- **Host-facing**: `import_rootfs`, `sync_store`, `browse` (stat, list, open, text sniffing,
  search), `browse_changing` (create, remove, rename, move, open for writing, written),
  `take_changed_dirs`.

## Data flow / lifecycle

### On-disk layout and schema

- A store directory holds `meta.db` (SQLite, WAL mode), `objects/` and `lock`.
- Tables: `inodes` (mode, owner, nlink, `..` parent for directories, rdev, mtime/ctime, symlink
  targets), `dentries`, `xattrs`, `detached_roots` (tmpfs roots), `shared` (regular files whose data
  is not plainly their own object: the object's number and, while held, the access time). Only
  primary keys are indexed, and `shared` by object. Tables are created with `IF NOT EXISTS`, so
  older stores gain `shared` when opened.
- Pragmas: `journal_mode = WAL`, `synchronous = NORMAL`, `journal_size_limit = 8 MB`, a 10 s busy
  timeout, a statement cache.
- `Store::create` writes the root directory (0755, its own parent).

### Objects and metadata ownership

- Each regular file's data is `objects/<ino>` (mode 0600), or the object `shared` names for it,
  always opened by number relative to an `O_PATH` directory fd, never by path. Directories,
  symlinks, device nodes, FIFOs and sockets exist only in the database.
- Regular files take size, mtime, atime, ctime and the allocated block count (`Stat::blocks`, so
  holes take no storage) from the host object; a metadata change to a
  regular file chmods the object back to 0600 so the host bumps its ctime (one clock per file); the
  database ctime column is still written for regular files but never reported. Time changes use
  the host's `utimensat` on the object by number, except an empty-path change on a (non-`O_PATH`)
  regular-file fd, which uses `futimens`.
- Other inodes keep mtime and ctime in the database; their atime is reported equal to mtime.
  Symlinks report the target length as size and mode 0777.
- Reads and writes go straight to the host fd (`pread`/`pwrite`); copies between two regular files
  use the host's `sendfile`.

### Shared objects

- `share(src, dst)` makes `dst` use `src`'s object, in one operation: a source not yet held moves
  its host times into the database (atime into `shared`, mtime and ctime into its row); the
  destination keeps its access time, its modification and change times become now, and its old
  object is doomed when no row uses it. Both rows are then held. A deleted source is `ENOENT` (its
  object may already be queued for the reaper). The caller guarantees nothing writes the source's
  object in place and nothing maps the destination.
- A held file reports its times from the database (set by `set_times`/`set_times_open` there, ctime
  now), its size and blocks from the shared object; metadata changes do not chmod the object, whose
  host times belong to no one file.
- `unshare(ino, keep)` before a held file changes: when no other row uses its object, the file
  settles in place (the object's atime and mtime set from the database, the row deleted when the
  object is its own number, else its access time cleared) and nothing is returned; otherwise an
  inode row with no links is inserted only to number a new empty object, returned with the length
  to copy (all, or what a truncation keeps). `finish_unshare` then points the row at the new object,
  settles its times, dooms the old object if unused and queues the file in `replaced`, so its data
  is flushed before the commit that records it; it is `EAGAIN` (the copy removed) when the row no
  longer uses the object it was copied from. `make_private` loops over the three with the copy done
  in place.
- `open_object` for writing, `open_object_append` and `truncate` call `make_private` first, so host
  callers (the browse API, `write_file`) never write a shared object; they do not move the kernel's
  open descriptions themselves: the browse API's `Opened` change says when it unshared the file
  (`unshared`), and the kernel then moves its own (`kernel/files`).
- Objects the provider handed out for writing are recorded process-wide (`HOST_WRITERS`: the
  store's identity, the object, the path it was opened by) from `open_write` until `written`
  reports the close by the same path, whatever became of the file; `host_writing` tells the kernel
  not to share such an object with another file, as the provider's writes would reach both. A
  close that is never reported keeps its object out of sharing (only copies are made).

### Namespace operations

- Every operation is atomic. Hardlinks are several dentries for one inode (directories cannot be
  linked, `EPERM`; an inode with link count 0 cannot get a name again, `ENOENT`, as its object is
  doomed).
- Errors: `EEXIST` (name taken, no-replace rename, `XATTR_CREATE`), `ENOTDIR`, `ENOENT` (missing name,
  creating inside a deleted directory), `EISDIR` (unlink of a directory), `EBUSY` (rmdir of root),
  `ENOTEMPTY`, `EINVAL` (moving a directory under itself), `ENODATA` (missing xattr); SQLite errors
  become `EIO`. Cross-device checks belong to the syscall layer.
- Inode numbers are allocated by SQLite (`max(rowid) + 1`, no AUTOINCREMENT); a leftover object
  under a reused number is truncated and reused.
- Removing a regular file's last link leaves the row with nlink 0 and queues the object for deletion
  after commit when no other row uses it (open host fds keep working); a held file instead keeps
  its row in `shared` until `release` (called by the kernel when nothing has it open or mapped) or
  the next boot, since its descriptions still read the shared object and a write through them must
  find it. rmdir likewise leaves an nlink-0 row. Rows stay until
  the next boot cleanup, so numbers are not reused while the store runs; after boot, numbers above
  the highest surviving row (and numbers of rolled-back inserts) are reused.
- tmpfs trees are detached directories (their own parent) recorded in `detached_roots`; their files
  are ordinary objects.
- `create_anonymous` (memfd) creates an nlink-0 row owned by the given ids (`memfd_create` passes
  the caller's fsuid and fsgid) and an object that is unlinked at once, so only the returned fd
  holds the data.

### Transactions and deferred commits

- The first write opens `BEGIN IMMEDIATE`; each operation runs in a `SAVEPOINT` (single statements
  run without one; nested operations join the enclosing one). Failure or a kernel-layer panic rolls
  back only that savepoint.
- The transaction is committed at most 5 ms later: at the end of an operation if it is overdue, or by
  the timer thread through `commit_due` (signalled via the waker). It is committed immediately when
  another connection is open (`users > 1`), and by guest fsync/sync, `Machine::sync`, `trim_memory`,
  and machine/store teardown.
- A failed `COMMIT` rolls back the whole transaction, clears the cache and removes the objects
  created by it; every deferred operation in it is lost.
- After a successful commit the uncommitted-inode range is cleared, the commit counter bumped, and
  doomed objects handed to the reaper.

### Power-loss ordering

- Commits write WAL frames without syncing; the WAL and objects reach disk independently. Three
  orderings mimic ext4 (`data=ordered`, `auto_da_alloc`):
  - `fsync`/`fdatasync`: sync a regular file's object (BKL released; directories skip this, host
    fds return 0), commit (BKL held), then sync the WAL (BKL released); `sync`/`syncfs`: commit,
    then `syncfs` the host filesystem; synchronous writes (`O_SYNC`/`O_DSYNC` files, `pwritev2`
    with `RWF_SYNC`/`RWF_DSYNC`) sync the object only;
    `Machine::sync` and `trim_memory` only commit (protection against being killed, not against
    power loss).
  - A regular file renamed over an existing name (not exchanges, not symlinks) has its data
    flushed before the commit containing the rename (`sync_file_range` + `fdatasync` per file, or
    one `syncfs` beyond 16 files), done inside the commit with the BKL held; errors are only logged
    and the first `fdatasync` error skips the remaining files.
  - The reaper syncs the WAL before deleting an object, so an object is never deleted before the
    commit that removed its last link is on disk.
  - A held file's private copy is flushed before the commit that points it there, through the same
    `replaced` list as a rename-over.
- New regular files' objects exist immediately; the `lock` header records the uncommitted inode
  range so the next boot can delete objects without rows.

### Reaper and checkpoints

- One background thread per connection (every short-lived host connection spawns and joins one):
  when objects are queued it syncs the WAL (unless nothing was committed since the last sync) and
  unlinks them; it runs PASSIVE checkpoints on its own lazily opened connection whenever the WAL has
  grown 1000 pages since the last request, or after 100 ms without commits while the WAL is at least
  1000 pages and not fully backfilled. The committing thread checkpoints in place only when the WAL
  reaches 10,000 pages. Without a reaper thread SQLite's auto-checkpoint is kept and deletions
  happen inline.

### Opening and boot cleanup

- Every connection takes a shared `flock` on `lock`. If it can get the lock exclusively (nobody else
  has the store open), it runs boot cleanup: reset `users`, delete objects in the recorded
  uncommitted range that have no row, empty the volatile directories passed in (the kernel passes
  `/dev/shm` and `/run`; host-side openers pass none), remove detached tmpfs trees, sync, drop the
  `shared` rows of deleted files, delete nlink-0 rows no `shared` row uses as an object and their
  objects, delete the objects those dropped rows used that no row uses any more (unless one is a
  live file's own data), taking all those numbers back out of the doomed list the emptying filled,
  commit. Then it downgrades to shared and increments `users`; if
  others are open it waits (`BEGIN IMMEDIATE; COMMIT`) for their deferred transaction.

### Metadata cache

- Caches dentries (including names that do not exist), database rows (with the `shared` row
  joined in), symlink targets, `..` parents, names, directory listings and the file-capability
  set; xattr values, emptiness checks
  and regular-file sizes and times are not cached.
- Reads hit the cache first; on a miss the database is queried and the result inserted only if the
  commit counter did not move. Inside a `snapshot` all misses share one lazily started read
  transaction (path walks and the following stat).
- Writes invalidate the affected entries as they happen (names and listings of the directory; the
  row, target and parent of the inode); entries created by a successful operation are inserted
  afterwards (fills are discarded on rollback). There is no eviction: after 131,072 fills the whole
  cache is cleared.
- The cache is labelled with the commit count it matches. A commit by this connection advances the
  label; any other change of the shared counter (another connection's commit) clears everything,
  checked lazily on each query, reader and commit. A failed commit clears it too.
- Only the BKL-holding thread mutates the cache, under the write lock. Threads without the BKL use
  `Meta::reader` (read lock), which refuses when the counter moved; the no-BKL resolver falls back
  to the BKL path on any miss. Regular-file stats in a reader query the host while holding the lock.

### Sharing the store

- Connections, not processes, are counted in `users`. The kernel and the app's DocumentsProvider run
  in the same process; each host call (`browse`, `browse_changing`, `write_file`, `read_file`) opens
  its own short-lived connection, and while it is open every operation commits immediately. A count
  leaked by a crash can only persist while another process holds the shared lock (e.g. several
  `tuxport-run` processes on one store), so on Android the next open is exclusive and resets it.
- Changes made by host calls are replayed to running kernels of the same store in this process
  (found in the machine's registry), under their BKL, as inotify events. Directories the guest
  changes are recorded (up to 256 per poll, starting from the first poll) and handed to the
  provider through `Machine::take_changed_dirs`, which maps them to paths (deleted and tmpfs
  directories drop out).

### Import

- `import_rootfs` creates a new store and imports a gzip-compressed tar in one transaction: paths are
  normalised (`..` is `EINVAL`), missing parents created as 0755 root, directories' attributes
  applied (also onto an existing non-directory of that name) and their mtimes set last; files,
  symlinks, hardlinks (to earlier entries), device nodes and FIFOs are created with their mode bits
  (setuid included), numeric owner and whole-second mtime; a non-directory entry whose name already
  exists aborts with `EEXIST`; xattrs (and file capabilities), pax metadata and whiteouts are not
  imported. Then the device and mount-point skeleton (`/proc`, `/sys`, `/run`, `/dev`) is added and
  the host calls `sync_store` (host `syncfs`). The Android caller imports into a temporary store
  directory that is discarded before a retry.

### Browse API

- Paths resolve like `realpath` inside the guest tree (links followed, at most 40). Only directories
  and regular files are visible; mounts are not.
- Reads (stat, list, open read-only, text sniffing on the first 4 KB of up to 1000 files, search with
  a 50,000-entry budget) accept any guest path. Writes (create with ` (n)` name variants, remove,
  rename and move without overwriting, open for writing with a read-write or append fd) are confined
  to a caller-supplied `top` directory compared by real path (`EACCES` outside, and for a `top`
  whose real path is the root directory, which would cover everything; `top` itself cannot be
  removed). No guest permission checks are applied, tree removal is one operation per entry, and
  provider writes do not strip setuid/setgid bits or capabilities.
- A provider write reports `Opened`, writes the object directly, and reports `Written` on close if
  the path still resolves (`IN_MODIFY`, `IN_CLOSE_WRITE` in the guest).

## Dependencies (inbound / outbound)

### Inbound

- `kernel/vfs` (path walks, namespace and metadata syscalls, tmpfs instances, the no-BKL
  resolver), `kernel/files` (objects, sharing and unsharing, syncs), `kernel/mount`
  (`Vfs::open`'s boot with the volatile list), `kernel/process` (`syscall::fast`), the machine
  (waker, timer `commit_due`, `sync`, `trim_memory`, teardown, the store registry,
  `take_changed_dirs`), `kernel/inotify` (`host_change`), host wrappers in `lib.rs` (JNI and
  `tuxport-run`).

### Outbound

- rusqlite with bundled SQLite (WAL hook, checkpoints); `tuxport_hal` (`open_at`, `stat_at`,
  `chmod_at`, `unlink_at`, times, `flock`, `map_file`, `fsync`, `syncfs`, `sync_file_range`,
  `statvfs`); the `tar` and `flate2` crates; `kernel::bkl_holder`; `bug::catch`.

## Invariants and gotchas

- The commit counter is bumped only after `COMMIT` succeeds; the reaper and the cache both rely on
  this ordering.
- Cache contents always describe the database at the commit count they are labelled with; fills
  whose query raced a commit are dropped.
- Only the thread holding the owning kernel's BKL may use a kernel's `Store` (checked in debug and
  `borrow-check` builds).
- Objects are always addressed by inode number, never by guest path.
- Inode numbers are reused only after boot cleanup has deleted their rows. Boot cleanup therefore
  deletes those objects itself and must hand none of their numbers to the reaper, which unlinks
  `objects/<ino>` without checking whose it is.
- A number some `shared` row uses as its object keeps its inode row (live, deleted, or one that only
  numbers a copy) at boot, so no new file gets it while the object lives.
- When two or more rows use one object, every one of them is held, and nothing may write that
  object in place; a row never just repeats a plain file (object its own number, not held).
- A number becomes a shared object only along with a `shared` row for its own file, so a plain
  file's object is never used by another row.
- A crash that leaves `users` too high disables deferred commits until the next exclusive open.

- A failed `COMMIT` loses every deferred operation already reported as successful (the documented
  cost of deferral; only host I/O errors or a full disk trigger it).
- Cached directory listings are not invalidated by chmod/chown, so their permission bits can be
  stale; every current caller uses only the file-type bits.

## Confirmed bugs / technical debt

- **Shared mappings across connections:** when a Files app write gives a held file an object of its
  own, the running kernel moves its open descriptions there but not its shared mappings, which
  keep the old shared object.
- **Shared objects of deleted files:** a held file deleted while open keeps its share until boot,
  so the next write to another file sharing the object copies it although nobody reads the deleted
  one any more.
- **Held files' access times:** reads never update them; only utimensat does.
- **Whole-file sharing:** the first change to a file sharing its object copies all of it, where
  btrfs and XFS copy only the extents written.

- **Storage leaks:** objects created inside a rolled-back savepoint (an operation that fails after
  creating a regular file) are dropped from the uncommitted range by the next commit and stay on
  disk until their number is reused by a regular file (forever if reused by another kind); a failed
  `import_rootfs` is not affected in practice, as the Android caller discards that new store
  before a retry; the shared `lock` header is not
  ordered against the WAL, so after power loss a cleared range can be durable while the commit is
  not, leaking objects the same way.
- **Import error reporting:** if adding the device skeleton fails after a successful import,
  `import_rootfs` returns an error while the imported tree is committed (the comment promises an
  empty store on failure).

## Open questions

- Should importing a rootfs keep xattrs and file capabilities (the store supports them)?
- Should `top` for provider writes be validated in the kernel, or is the JNI caller fully trusted?
- Should provider writes strip setuid/setgid bits as Linux's `file_remove_privs` would?

## To verify

- The crash test covers ext4 hosts only; whether a durable commit implies a durable object dentry on
  f2fs (common on phones) is not checked.
