# kernel/overlay Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/overlay.md` for the paired decision history (decisions from before the split are D39, D40 and D49 in `.agents/decisions/kernel/vfs.md`, and D45 and D46 in `.agents/decisions/kernel/process.md`).
>
> Verified: 2026-11-11 (split out of the kernel/vfs full mapping of 2026-11-10; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-12 (targeted verification only since the split; latest change: copy-ups go through `copy_up`, which copies when the lower object could change in place)

## Responsibilities

`kernel/overlay` is overlayfs (6.1's `fs/overlayfs`), a third kind of VFS node beside store and
synthetic nodes:

- mount(2) of type `overlay`: options, layers and their checks, the work directory, read-only
  instances without an upper layer, and the superblock options mountinfo shows;
- lookup through the layers (whiteouts, opaque directories), node identity and lifetime, and the
  sweep of removed nodes;
- readdir merging, stat, statfs and the overlay's own xattrs;
- copy-up, and creating, removing and renaming through an overlay;
- opening files through an overlay and following a file to its copy.

Root: `core/kernel/src/vfs/overlay.rs`; the overlay methods of `Vfs` in `vfs/mod.rs` (`new_overlay`
to `overlay_readdir`, `sweep_overlays`); in `syscall/fs.rs` `open_overlay`, `follow_copy_up`,
`copy_up`, `unlink_overlay`, `link_overlay` and `rename_overlay`; in `syscall/mount.rs` the overlay
options (`overlay_layers`, `overlay_dir`) and the node sweep at syscall end.

Outside the module: the walk, attribute and namespace syscalls that branch into this module, and
the rules every file operation keeps for overlay nodes (`kernel/vfs`); the opens and truncations
that branch into it (`kernel/files`); store transactions,
objects shared by copy-up and whiteout inodes (`kernel/store`); the mount table, `collect` and
mountinfo (`kernel/mount`); inotify on overlay nodes (`kernel/inotify`); locks keyed on overlay
nodes (`kernel/locks`); syscall numbering for the sweep (`kernel/process`); a program's `exe_file`
holding its node (`kernel/exec`).

## Key types

- `Overlays` (`vfs/overlay.rs`) — `Instance`s by device number (upper root, the mounter's IDs, the
  tmpfs instances its layers are in, `Options`) and `OvlNode`s by `OvlId` (Linux's overlay dentry:
  parent and name, upper inode, lower inodes — every merged lower directory for a directory, the
  topmost otherwise — `origin`, children looked up), plus orphans stamped with the syscall count
  and the next sweep threshold. Ids are per kernel, never reused.
- `overlay::Hold` — an open file's or running program's claim on an overlay node, in one static
  table for every kernel in the process (a hold in another kernel can only delay a sweep).
- `Node::Overlay(MountId, OvlId)` (`kernel/vfs`); `OpenFile::ovl`, `ovl_holds` and `ovl_lower`;
  `fs::Attrs::Overlay`.

## Public entry points

- **Instances and nodes**: `Vfs::new_overlay`, `overlay`, `overlay_stat`, `overlay_readdir`,
  `overlay_upper`, `overlay_remove`, `overlay_rename`, `overlay_child`, `overlay_prefix`,
  `sweep_overlays`; `Overlays::upper_of` (copy-up), `remove`, `rename`.
- **Syscall paths**: mount(2) of type `overlay`; `open_overlay`, `follow_copy_up`, `unlink_overlay`,
  `link_overlay`, `rename_overlay`; overlay branches of creation, attribute changes and readdir in
  `kernel/vfs`, of opening and truncate in `kernel/files`, and of xattr calls in `kernel/xattr`.

## Data flow / lifecycle

### Mounting

- mount(2) of type `overlay` follows 6.1's `ovl_parse_opt` and `ovl_fill_super`: options split at
  unescaped commas, empty ones skipped, unknown ones or missing values `EINVAL`, the last of a
  repeated one wins; `workdir`, `index` and `volatile` without `upperdir` ignored; conflicting
  `metacopy`, `nfs_export`, `index`, `redirect_dir` and `userxattr` `EINVAL`, the accepted ones
  changing nothing else. `lowerdir` is split first at unescaped colons (missing, an empty
  component or over 500 `EINVAL`). Then upper layer and work directory: both or neither, non-empty,
  upper not on a read-only mount, both on one mount, neither inside the other (`EINVAL`); without
  an upper layer at least two lower layers (`EINVAL`); each lower layer then resolves, none inside
  the upper layer or work directory (`ELOOP`). Each layer resolves from the caller's root and cwd,
  following links, and must be a store directory on a mount of the caller's namespace with no
  locked mount under it (`EINVAL`, also for synthetic or overlay directories;
  `clone_private_mount`'s checks). The work directory gets an empty `work` (mode 0, the
  mounter's; an old one removed) that nothing uses: copy-up builds in place in one store
  transaction. An instance keeps the tmpfs instances its layers are in from being collected.
- Without an upper layer it is read-only as a filesystem (`sb_readonly`): `ro` in mountinfo's
  superblock options (the mount's own flags as given), read-only in `/proc/<pid>/mounts` and
  statfs; every change is `EROFS` after creation's `EEXIST`, also through a bind remounted
  read-write, and remounting it read-write is `EROFS`. Superblock options are `lowerdir`,
  `upperdir`, `workdir` as given, then what differs from 6.1's defaults (`ovl_show_options`;
  `xino` parsed, never shown), escaped as `seq_show_option` does.

### Lookup and node lifetime

- Lookup (`ovl_lookup`): the upper inode, then each lower directory merged into the parent; a
  whiteout (char 0:0) is `ENOENT`; a non-directory ends the search and hides what is below a
  directory; an opaque directory (`trusted.overlay.opaque` = `y`, or `user.overlay.opaque` with
  `userxattr`, in any layer) ends it after itself. One directory and name give one node, its
  inodes refreshed on each lookup. Mounts inside a layer are not seen through the overlay.
- Node lifetime: a node lasts while its name does. A removed name (unlink, rmdir, a replacing
  rename) makes it an orphan no lookup finds; once 1024 orphans gather (then twice as many as the
  last sweep left), the end of a syscall sweeps them (`sweep_overlays`): an orphan goes, with every
  node below it, unless it or a node below is held (`Hold`), is a process's root or cwd, a mount's
  root or point (detached ones too), watched by inotify, or was removed after another thread's
  syscall began — unless that syscall waits only on files it holds (`waits_on_files_only`: I/O,
  polls, child and signal waits, sleeps, futexes, locks, receives and accepts, fcntl, ioctl,
  fsync, copy_file_range, IPC, AIO events), as `kernel/process` numbers syscalls. A sweep gives up
  for the round when a task's resources or the inotify registry are borrowed. An instance and its
  nodes go in `kernel/mount`'s `collect` once it has no mount.

### readdir, stat and xattrs

- readdir merges the layers' entries sorted by name (even from one layer), the highest layer's
  entry per name, whiteouts hiding names below, each inode number the one stat shows. Stat is the
  topmost inode's with the overlay's device number and the inode number of the lower inode a
  directory, or a file with one link, was copied up from (in memory only); a merged directory
  reports one link. Reads, readlink, exec, search permission and xattr reads use the topmost
  inode; the overlay's own attributes (`trusted.overlay.*`, `user.overlay.*` with `userxattr`) are
  `EOPNOTSUPP` to get or set (`ovl_own_xattr_get`/`_set`) and hidden from listings. statfs reports
  overlayfs's magic with the store's capacity, even with the upper layer on a tmpfs.

### Copy-up

- Copy-up (`Overlays::upper_of`, `ovl_copy_up`): parents first, then the node as its topmost lower
  inode is (directory; regular file sharing the lower object, as 6.1 clones with reflink; symlink;
  special file), with mode (set-id bits too), owner, non-overlay attributes and times, in one store
  transaction; the parent's times are put back. Sharing marks both inodes held, so a later write
  open of either first gives it an object of its own. The syscalls copy up through
  `syscall::fs::copy_up`, which, as `try_share` does, copies a regular file's data instead
  (`CopyUp::Copied`) when a writable shared mapping or the Files app could write the lower object in
  place, and after sharing rehomes the lower file's descriptions open for writing (`cow`); a rename
  copies up the files it moves that way first. A non-directory then forgets its lower inodes; a
  copied-up directory keeps them and stays merged (one link, rename `EXDEV`). A file opened with
  `O_TRUNC` or truncated to 0 is copied without data. chmod, chown, utimensat (`Attrs::Overlay`),
  setxattr, removexattr, truncate and write opens check `EROFS` and permission on what the overlay
  shows (the last two also `ETXTBSY` for a program run from the lower inode), copy up only then, and
  change the upper inode. utimensat by fd goes by number, never through the open (possibly lower)
  object.

### Creating, removing and renaming

- Creating (`create_dir`, `create_in`): the directory is copied up, a whiteout of the name does not
  count as existing and is replaced in the same transaction, and a directory made in its place is
  opaque (`ovl_create_over_whiteout`); this covers open with `O_CREAT` (the new file is the
  overlay's node, so it locks with later opens), mkdir, mknod, symlink, link (the file copied up
  and linked there) and AF_UNIX bind (connect finds the upper inode). mknod of a whiteout through
  an overlay is `EPERM` (`ovl_mknod`).
- Removing (`unlink_overlay`, `Overlays::remove`; do_unlinkat's and do_rmdir's order, `may_delete`
  on what the overlay shows, a mount of the caller's namespace on it `EBUSY`, rmdir of a directory
  not shown empty `ENOTEMPTY`): the upper inode goes with the whiteouts left in a directory, and a
  whiteout owned by the mounter replaces it when a lower layer has the name.
- Renaming (`rename_overlay`, `Overlays::rename`; may_rename's checks on what the overlay shows,
  mount points `EBUSY`, then `ENOENT`, `EEXIST`, `ENOTDIR`, `EISDIR`, `ENOTEMPTY` on the merged
  view): a directory with lower layers (merged or lower only) is `EXDEV` (6.1 without
  `redirect_dir`; with `RENAME_EXCHANGE` the destination too), so tools fall back to copying;
  otherwise both sides are copied up and renamed in the upper layer, a whiteout at the new name or
  left in a replaced directory is removed first, a whiteout is left at the old name when a lower
  layer has it, and a directory moved into a merged one becomes opaque (with exchange, the other
  too).

### Opening

- Opening goes through `open_store` on the topmost inode; a directory stays an overlay node for
  getdents, fchdir and `*at` calls, and regular files, devices and FIFOs record the node in
  `OpenFile::ovl`, so `OpenFile::node`, fchmod-like calls and `/proc/<pid>/fd` give the overlay
  node; fstat of a regular file shows the node's inode number, of a device the node's stat, of a
  FIFO its layer inode and attributes. `O_DIRECT` is decided by the mount's filesystem, so overlay
  files pass it even with the upper layer on a tmpfs. A file opened before its copy-up switches to
  the upper object on its next read, fstat, lseek or copy out (`follow_copy_up`,
  `ovl_real_fdget`; a writable one marked `cow` while the upper file is held), keeping the lower
  object open until close (`OpenFile::ovl_lower`). Locks key on the node (`lock_ino`), leases on
  the store inode behind it.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/vfs`: the walk, creation, attribute changes, readdir, stat and statfs on overlay nodes;
  `kernel/files`: opening and truncating them; `kernel/xattr`: xattr calls on overlay nodes.
- `kernel/mount`: mount(2) of type `overlay`, `collect`, mountinfo's superblock options.
- The syscall dispatcher (the sweep at syscall end; `Task::syscall` numbering, `kernel/process`),
  `kernel/exec` (`ExeFile` holding a program's node), `kernel/inotify` (overlay keys),
  `kernel/locks` (`lock_ino`), `kernel/unix` (bind and connect through an overlay).

### Outbound

- `kernel/store`: lookups in each layer, one transaction per copy-up, shared objects, whiteouts,
  xattrs.
- `kernel/files`: `open_store`, `prepare_write`; `kernel/vfs`: the tmpfs instances its layers are
  in.
- `kernel/mount` (layer checks, `clone_private_mount`'s rules), `kernel/cred` (permission and
  ownership on what the overlay shows), every task's root and cwd and the inotify registry for the
  sweep.

## Invariants and gotchas

- An overlay node's store inodes may be in lower layers that must never change, so `Node::ino` is
  None for it and nothing may turn an overlay file's store inode back into a `Node::Store`: an
  open file reaches its node through `OpenFile::ovl`, and every change goes through a copy-up
  (`create_dir`, `Attrs::Overlay`, the overlay's removal and rename); `store_dir` refuses overlay
  directories (`EXDEV`). A `Node::Store` of an overlay mount and its upper inode is built only
  inside one call (`create_parent_at`'s result, the tmpfs accounting helpers, `file_stat`,
  truncate, `may_create`), for mount flags and accounting, and never stored.
- Overlay nodes live while their names do, and a removed one while anything may use it: every
  open file naming one must hold it (`OpenFile::hold`, or the kind's node at creation), as
  `ExeFile` holds a program's, and every other long-lived holder must be in the sweep's scan; a
  syscall that waits while keeping a node it looked up (an open waiting for a FIFO peer or a lease
  break) is safe only by staying out of `waits_on_files_only`.

## Confirmed bugs / technical debt

- **Overlay gaps**, one per line:
  - an overlay cannot be a layer (Linux accepts one as a lower layer);
  - `/proc/<pid>/exe` of a program whose overlay name was removed can be run again but not opened
    (`ENOENT`; a descriptor of the store inode would change it behind the overlay);
  - nodes of existing names are never freed while mounted (one per name looked up), and removed
    ones stay while a path-based syscall begun before their removal runs in another thread;
  - the no-BKL stat never serves overlay paths;
  - a FIFO opened through an overlay fstats as its layer inode, so after a copy-up fstat and stat
    disagree;
  - no `trusted.overlay.origin`, `impure` or shared whiteout inode: a copied-up file's inode
    number changes at the next mount, and a hard link through the overlay shows the upper number;
  - changes through an overlay whose upper layer is on a tmpfs are not charged to it;
  - a mapping made from a file opened before its copy-up keeps the lower object;
  - parents are copied up before a creation's permission check, so a refused creation leaves them
    copied;
  - other namespaces' mounts on a removed or renamed overlay name are not detached;
  - a lower layer's `trusted.overlay.redirect` is not followed, though sysfs shows
    `redirect_always_follow` as Y (nothing Tuxport runs writes one).
- Leases taken before a copy-up are lost (`kernel/locks`).

## Open questions

- None recorded.

## To verify

- None beyond the full mapping's: the 6.1 comparisons here were checked from knowledge of the
  kernel, not against its source.
