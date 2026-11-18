# Inode store (kernel/store) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/store"`
>
> Last distilled: 2026-11-10 (HEAD = 300d384). D1–D9 were seeded by hand from the retired design doc
> §8.1 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: The rootfs is an inode store — SQLite for the namespace, one host file per regular file
- **What**: Directories, names, inodes, symlinks, device nodes, xattrs and hardlinks live in SQLite; each regular file's data is `objects/<ino>`, opened by number relative to the objects directory.
- **Why**: A real directory tree plus a metadata database (iSH fakefs style) was rejected: Android cannot create hardlinks, and keeping the tree and the database consistent across rename and unlink is hard. SQLite gives atomic metadata operations; it is bundled as the amalgamation (rusqlite `bundled`) because the NDK exposes no system SQLite.
- **Tradeoffs**: Every metadata operation goes through SQLite; host tools cannot browse the rootfs as plain files.
- **Watch out**: Objects must always be addressed by inode number, never by guest path.
- **Source**: design review 2026-10-24 (§8.1, §15)

### D2: Regular files' size and times come from the host object
- **What**: Size, mtime, atime and ctime of regular files are read from `objects/<ino>`; metadata changes chmod the object so the host bumps ctime.
- **Why**: Data I/O goes straight to the host fd, so the host already maintains size and times. Directory and file timestamps used to go stale (breaking Python's import cache, PHP tests, Maildir, git's untracked cache); taking the later of the database and object ctime failed because the two clocks can differ (on virtiofs the object's clock was 84 ms ahead), so each file uses only the host's clock.
- **Tradeoffs**: Non-regular inodes keep times in SQLite with atime equal to mtime.
- **Watch out**: Changing a regular file's database metadata without touching the object leaves a stale ctime.
- **Source**: design review 2026-10-24 (§8.1), round 73 (`73-timestamps`)

### D3: Writes are deferred up to 5 ms in one transaction with a savepoint per operation
- **What**: A write transaction stays open up to 5 ms; each operation is a savepoint that alone is rolled back on error or panic.
- **Why**: A commit costs far more than the operation itself (package installs were dominated by per-operation commits); if the app is killed only the last few milliseconds are lost and the store stays at a consistent commit.
- **Tradeoffs**: A failed commit loses every deferred operation in it; guest fsync, host sync and sharing with another connection force immediate commits.
- **Watch out**: The timer thread must commit overdue transactions even when no further operation arrives.
- **Source**: rounds 06 (`06-m4`, risk R5), 32 (`32-workloads-file-ops`)

### D4: A metadata cache, invalidated across connections by a shared commit counter
- **What**: Dentries (including negative ones), rows, symlink targets and listings are cached in memory; own writes invalidate precisely, and a foreign change of the counter in the `lock` file clears everything.
- **Why**: Path lookup and stat dominated file-heavy workloads; the counter keeps several users of one store from reading stale data.
- **Tradeoffs**: No eviction (cleared wholesale after a fill budget); readers without the BKL must fall back on any miss.
- **Watch out**: The counter must be bumped only after `COMMIT`, so nothing older can be filled under a newer label.
- **Source**: rounds 13 (`13-profiling`), 25 (`25-stat-without-bkl`)

### D5: WAL checkpoints run on a background thread
- **What**: A reaper thread with its own connection runs PASSIVE checkpoints; the committing thread only checkpoints when the WAL grows very large.
- **Why**: SQLite's automatic checkpoint (two fsyncs) ran in the committing connection, usually under the BKL; on phone UFS/f2fs an fsync often takes milliseconds to tens of milliseconds, stalling every guest thread entering the kernel.
- **Tradeoffs**: An extra thread and connection per store connection.
- **Watch out**: Keep the inline fallback so the WAL cannot grow without bound.
- **Source**: round 111 (`111-background-checkpoint`)

### D6: Power-loss ordering mimics ext4 `data=ordered` with `auto_da_alloc`
- **What**: Commits do not sync; fsync/sync really reach disk; files renamed over others are flushed before the rename commits; objects are deleted only after the unlinking commit is durable.
- **Why**: Android kills apps routinely and phones lose power; a corrupted store (a `/bin/sh` that was not an ELF was once seen) would make the whole environment unusable, and programs like apk and editors rely on ext4's rename-over behaviour without calling fsync.
- **Tradeoffs**: Rename-over commits flush data while holding the BKL.
- **Watch out**: The crash test (`tests/crash`) only covers ext4 hosts.
- **Source**: round 198 (`198-power-loss`)

### D7: Several connections share one store through the `lock` file
- **What**: Shared `flock`s, a shared-mapped header (commit counter, users, uncommitted inode range), `BEGIN IMMEDIATE`, no deferral while another connection is open, boot cleanup only with an exclusive lock.
- **Why**: The app's DocumentsProvider must read and write the same store as the running kernel without stale or lost metadata.
- **Tradeoffs**: Commits become immediate while the provider has a connection open.
- **Watch out**: A crashed connection leaves `users` high, disabling deferral until the next exclusive open.
- **Source**: rounds 32 (`32-workloads-file-ops`), 196 (`196-documents-read`), 197 (`197-documents-write`), 201 (`201-documents-notify`)

### D8: tmpfs trees live in the store, not in memory
- **What**: A tmpfs mount is a detached directory tree in the store whose files are ordinary objects, removed at the next boot if left over.
- **Why**: bubblewrap builds its new root on a tmpfs, and keeping tmpfs data in host files avoids consuming the app's memory.
- **Tradeoffs**: tmpfs is as fast as the store, not as RAM.
- **Watch out**: Detached trees must be cleaned up when their last mount goes away and at boot.
- **Source**: round 117 (`117-tmpfs`)

### D9: The host browse API skips guest permissions but confines writes
- **What**: The provider resolves guest paths itself, reads anywhere and writes only below a caller-supplied directory (the login account's home), refusing with `EACCES` one whose real path is the root directory.
- **Why**: Users copy files in and out of Tuxport through Android's Files app; the provider acts for the device owner, not a guest user.
- **Tradeoffs**: Guest permission bits do not restrict the provider; confinement relies on the caller-supplied directory.
- **Watch out**: Host writes are replayed to running kernels as inotify events so guest watchers notice them.
- **Source**: rounds 196 (`196-documents-read`), 197 (`197-documents-write`), 201 (`201-documents-notify`); d5dbe29

### D10: The inode store is a module of its own, below the VFS
- **What**: kernel/store covers `vfs/store.rs`, `vfs/import.rs`, `vfs/browse.rs` and the host wrappers in `lib.rs` (the namespace and objects, deferred commits and power-loss ordering, the reaper and checkpoints, boot cleanup, the metadata cache, import and the browse API), with the VFS layer above it in kernel/vfs.
- **Why**: The retired design doc's §8.1 described the store in prose that had drifted from the code.
- **Tradeoffs**: The rationale lives only in this decisions file, and rounds keep § references to the design doc of their time.
- **Watch out**: Bugs confirmed at the split stay open in the map (leaked objects and misreported import errors), and so do four debts of sharing (D17): shared mappings left on the old object by a Files app write, a deleted held file keeping its share until boot, reads never updating a held file's access time, and whole-file copies on the first change.
- **Source**: c5236a1 (bug list since updated by 907b82b, c108be0, df6ee70 and 1f99a1c)

### D11: Boot cleanup keeps the numbers it removed away from the reaper
- **What**: `remove_orphans` drops the inode numbers it just deleted from the doomed list, so the boot commit hands none of them to the reaper.
- **Why**: Boot cleanup's unlinks queued objects that `remove_orphans` then deleted directly, and the reaper, still given those numbers, could delete a new file's object.
- **Tradeoffs**: It relies on `remove_orphans` running in the same transaction as the emptying, which boot cleanup always does, and the reaper itself stays blind to identity.
- **Watch out**: A boot-time step that unlinks files after `remove_orphans` would queue numbers for the reaper again (a map invariant).
- **Source**: b8bf44c

### D12: chown's setgid clearing is decided by the syscall layer
- **What**: `set_owner` takes `keep_sgid` from its caller and clears setgid when it is false as well as when the file is group-executable, while setuid is still cleared for every caller.
- **Why**: The store alone could not know the caller's group membership.
- **Tradeoffs**: None.
- **Watch out**: None known; the only other caller is a unit test.
- **Source**: ce7d0d8

### D13: Store files read in the order their code runs
- **What**: store.rs runs constants and `SCHEMA`, then `struct Store` with `impl Store` right under it in sections (opening and boot cleanup, reads, changes, transactions and deferred commits, metadata cache, objects, connection) and its Drop, then operations, shared state, cache entries, the lock file's counters, reaper and checkpoints, errors and names; browse.rs ends with path resolution, import.rs starts with `import_tar_gz`, and lib.rs's host wrappers follow their JNI functions.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; store.rs had no sections and kept `impl Store` hundreds of lines below its struct.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; `remove_lost_objects` stays in boot cleanup although commit calls it too.
- **Watch out**: Code appended at the end of `impl Store` or of a file drifts from this order.
- **Source**: 27365c6

### D14: memfds belong to their creator's filesystem ids
- **What**: `create_anonymous` takes a uid and gid and writes them into the new row in the same transaction, and memfd_create passes the caller's fsuid and fsgid (kernel/vfs D27).
- **Why**: Linux's memfd inode is owned by its creator, and root-owned memfds broke fchmod and ownership checks for every other user.
- **Tradeoffs**: None known.
- **Watch out**: None known.
- **Source**: 907b82b

### D15: An unlinked inode cannot get a new name
- **What**: `Store::link` returns `ENOENT` for a row with link count 0, after the directory check, as Linux's `vfs_link` does (kernel/vfs D28).
- **Why**: An nlink-0 inode's object is deleted after the next commit, so a new name would dangle and boot cleanup would never remove it.
- **Tradeoffs**: O_TMPFILE's `I_LINKABLE` exception has no counterpart, since O_TMPFILE is not implemented.
- **Watch out**: None known.
- **Source**: c108be0

### D16: st_blocks comes from the host object
- **What**: `Stat::blocks` is an `Option<u64>` filled from the host object's st_blocks by `fill_from_host` and `fill_from_object` and `None` elsewhere, where the size stands in, and st_blocks and tmpfs accounting follow it (kernel/vfs D37).
- **Why**: Now that holes are real holes in the host object, st_blocks and tmpfs accounting must follow the storage a file takes, which only the host object knows.
- **Tradeoffs**: st_blocks varies with the host filesystem (APFS on a macOS development host frees zeroed ranges; f2fs keeps inline data).
- **Watch out**: Hosts that report delayed allocation late would undercount tmpfs writes until the instance is recounted.
- **Source**: 5d7b936

### D17: Files share a store object until one is written
- **What**: `share` makes a file use another's object, recorded in the `shared` table that the cached row joins, the first change to either side unshares it (`unshare`, `finish_unshare`, `abandon_unshare`, or `make_private` for all three under the BKL; in place when no other row uses the object, else into an object numbered by an nlink-0 row), boot drops the shares of deleted files and removes objects nothing uses, and `Store::host_writing` keeps an object the Files app has open for writing (`HOST_WRITERS`, kept by `browse::open_write` and `browse::written`) out of sharing, for kernel/vfs's clones and overlay copy-up (kernel/vfs D45 and D40).
- **Why**: Copying files cost their whole data in time and flash although most copies are never changed, phones' host filesystems have no reflink, and a guest clone made while the Files app held the host object open for writing would have received the app's later writes.
- **Tradeoffs**: Sharing is whole-file, so the first change copies everything; reads never update a held file's atime; a deleted held file that was open keeps its share until boot; a close the provider never reports keeps its object out of sharing until the process ends; an older build opening a store with shares reads the wrong objects.
- **Watch out**: Nothing may write a held file's object in place, `drop_object` trusts without a query that no row uses a plain file's number, numbers used as objects must keep their rows at boot, browse callers must report every close through `written` with the path they opened, and a Files app write that unshares a file leaves the running kernel's shared mappings on the old object (a map bug).
- **Source**: df6ee70, 1f99a1c
