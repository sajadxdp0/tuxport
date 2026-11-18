# kernel/inotify Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/inotify.md` for the paired decision history (decisions from before the split are D13, D46 and D51 in `.agents/decisions/kernel/vfs.md`).
>
> Verified: 2026-11-11 (split out of the kernel/vfs full mapping of 2026-11-10, whose inotify claims its independent verifiers checked; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-17 (targeted verification only since the full mapping; latest change: direct file and directory watch lookup for cgroup notifications)

## Responsibilities

`kernel/inotify` is inotify(7) inside the kernel layer. Every filesystem change passes through the
kernel, so no host notification is used. It provides:

- instances (`inotify_init1`), their watches and event queues, the per-user limits and the reads;
- the per-kernel registry of watches, keyed by store inode number or by the key of an overlay,
  FUSE or cgroup2 node;
- the hooks the file syscalls call at Linux's fsnotify call sites, and the event sequences each
  reports;
- the `Opened` record each open file carries, which gives fd-based events their directory and
  name and reports `IN_CLOSE_*`;
- replaying the Android Files app's changes as events (`host_change`), and cgroup2 file changes
  (`kernfs_modified`).

Root: `core/kernel/src/syscall/inotify.rs`.

Outside the module: where the hooks are called — the file syscalls and renames in `kernel/vfs`,
opens and file I/O in `kernel/files`, FUSE operations in `kernel/fuse`, cgroup2 file changes in
`kernel/cgroup`; readiness, epoll and `O_ASYNC` of inotify fds (`kernel/poll`);
`/proc/sys/fs/inotify/*` and the fdinfo text's place (`kernel/procfs`); the Files app's change list
(`kernel/store`) and the machine's `store_changed`, which finds each kernel and takes its BKL
(`kernel/process`); overlay node lifetime and the sweep that keeps watched nodes (`kernel/overlay`).

## Key types

- `Inotify` — one instance: watches by descriptor, the event queue with its byte total (for
  `FIONREAD`), an overflow mark and a count of events queued (the `EPOLLET` generation), the queue
  limit taken at creation, the creator's effective uid (charged for instances and watches) and a
  weak reference to the `Registry`, from which closing it removes its watches.
- `Event` — descriptor, mask, cookie and name (padded to 16 bytes).
- `Registry` (`Kernel::inotify`, one per kernel) — the (instance, descriptor) pairs on each key,
  the last rename cookie, the `Vfs` (for `IN_EXCL_UNLINK`'s link checks), instance and watch
  counts per user, the three limits under `/proc/sys/fs/inotify/`, and the removed inodes whose
  `IN_DELETE_SELF` waits for files still open through the removed name.
- `Opened` (on `OpenFile`) — the file's key, the directory key and name it was opened through, and
  the close mask; its drop reports `IN_CLOSE_WRITE` or `IN_CLOSE_NOWRITE` (Linux's fput).
- Keys: store inode numbers stay below 2^62; overlay nodes take top bits 10 (`overlay::key`, also
  the lock tables' key), FUSE nodes 11 (`fuse::key`), cgroup2 files and directories 01
  (`cgroup::key`).

## Public entry points

- Syscalls `inotify_init1`, `inotify_add_watch`, `inotify_rm_watch`; `read` through
  `kernel/files`' `IoPath::Inotify`; `FIONREAD` in `kernel/files`' ioctl dispatch.
- Hooks for the file syscalls: `active` (whether any watch exists), `path_event`, `file_event`,
  `dir_event`, `cookie`, `opened`, `renamed`, `deleted`, `watched_at`, `key_of`.
- `host_change` (the Files app's changes, from the machine's `store_changed`), `kernfs_modified`
  (cgroup2 file changes).
- `Registry::watched` (the overlay sweep and FUSE's `in_use`), `Registry::watches` (cgroup observation);
  `Inotify::ready`, `queued`, `pending` and `fdinfo` (`kernel/poll`, `kernel/procfs`).

## Data flow / lifecycle

### Instances and watches

- `inotify_init1` accepts `O_CLOEXEC|O_NONBLOCK`; per-user instance (`EMFILE`) and watch
  (`ENOSPC`) limits and the queue limit come from `/proc/sys/fs/inotify/*` (defaults 128
  instances, 2^20 watches, 16384 queued events).
- Watches are keyed by store inode number, an overlay node's key, a FUSE node's key or a cgroup2
  node's key, whose changes `kernel/cgroup` reports as `IN_MODIFY`; watches on other synthetic
  nodes are accepted, counted and never report. Adding one needs read permission on what the
  store or the overlay shows (a FUSE node `fuse_permission`'s; synthetic nodes are not checked);
  a watch is identified by its node, mount included. `IN_MASK_ADD`/`IN_MASK_CREATE`,
  `IN_ONLYDIR`, `IN_DONT_FOLLOW`, `IN_EXCL_UNLINK`, `IN_ONESHOT`.
- read returns whole events (`EINVAL` if the first does not fit) and blocks unless nonblocking;
  `FIONREAD`, poll readiness and fdinfo read the instance. No fanotify (`ENOSYS`), no dnotify.

### Reporting

- With no watches every hook costs one emptiness check. An event goes first to the parent
  directory's watches with the name (not for `*_SELF`), then to the inode's own. Identical
  consecutive events merge; a full queue drops events and queues one `IN_Q_OVERFLOW`.
  `IN_IGNORED` follows removal, `IN_DELETE_SELF` and a matching `IN_ONESHOT`. Directory events
  carry `IN_ISDIR`, `IN_DELETE_SELF` and `IN_MOVE_SELF` never do.
- Hooks follow the fsnotify call sites: open (`IN_CREATE`, or `IN_MODIFY` for a truncating open
  for writing, then `IN_OPEN`), mkdir, mknod, symlink and AF_UNIX bind, link (`IN_ATTRIB` on the
  source, then `IN_CREATE`), unlink (`IN_ATTRIB`, delete-self, `IN_DELETE`), rmdir (delete-self,
  `IN_DELETE`), rename (a cookie pair, then as `fsnotify_move`: a replaced object's `IN_ATTRIB`,
  with `IN_ISDIR` for a directory, the moved one's `IN_MOVE_SELF`, the replaced one's
  delete-self; exchange reported both ways), attribute changes (`IN_ATTRIB`, also for utimensat
  changing both times; `IN_ACCESS` or `IN_MODIFY` when only the access or the modification time
  changes, as `fsnotify_change`), writes, truncation and fallocate (`IN_MODIFY`), reads and
  getdents (`IN_ACCESS`), the last close of an open file (`IN_CLOSE_*`).
- Path-based operations resolve the path a second time to find the directory, only when watches
  exist (`watched_at`). fd-based events (I/O, truncation, close) go to the directory and name the
  open went through, recorded in `Opened` (the dentry a Linux open file references): every
  non-`O_PATH` open of a regular file, device or directory (store, overlay, FUSE, cgroup2
  directories) or FUSE file records one, whether or not watches exist; store FIFOs and cgroup2
  files get none, so they report `IN_OPEN` and never `IN_CLOSE_*`. Renames update the records of
  open files, scanning all open files even with no watches; after a deletion the records stay, so
  events still go to the original directory.
- `IN_DELETE_SELF` waits until the last open file of the removed name closes (`deleted`).
- Through an overlay the hooks report to the overlay's nodes, as Linux's VFS reports on overlay
  inodes and dentries: the node and its overlay directory with its name (an open file's events
  through the node it was opened through, whose upper inode copying up changes; path-based
  attribute changes and truncation find the node by resolving the path again), not the layers'
  inodes behind them, so changes made in a layer directly do not reach them. Removal deletes the
  node when it was a directory or had one link; `IN_EXCL_UNLINK` takes a node its directory no
  longer has as unlinked (`Overlays::linked`). Creations through an overlay (open with `O_CREAT`,
  mkdir, mknod, symlink, link, AF_UNIX bind) also report `IN_CREATE` to the upper directory first,
  where Linux's overlay makes them through the VFS.
- Files app changes (`host_change`, under the target kernel's BKL) map `Created`, `Removed`,
  `Moved`, `Opened` (write opens) and `Written` to the same event sequences; the provider reports
  no access, close-nowrite, read-open or attribute events. An `Opened` that gave a held file an
  object of its own also moves this kernel's descriptions of it there (`kernel/files`'
  `host_unshared` and `rehome`), even with no watches.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/vfs`: the hooks from the metadata and namespace syscalls, renames (`renamed`) and
  removals (`deleted`); the overlay sweep reading `watched`.
- `kernel/files`: the hooks from opens (`opened`), I/O and closes; the instance's read and
  `FIONREAD`.
- `kernel/fuse`: hooks for FUSE namespace changes; `watched` in `in_use`.
- `kernel/cgroup`: `kernfs_modified` for cgroup2 file changes.
- `kernel/procfs`: the registry's limits behind `/proc/sys/fs/inotify/*`; the instance's fdinfo lines.
- `kernel/poll`: readiness (`ready`), the `queued` generation and the instance's wake object.
- The machine (`kernel/process`): `store_changed` calling `host_change` for the Files app.
- The dispatcher: the three syscalls.

### Outbound

- `kernel/vfs`: `Node`, path resolution (`read_path`, `watched_at`'s second walk), overlay keys
  and `Overlays::linked`, the store's link counts for `IN_EXCL_UNLINK`; `kernel/files`:
  `OpenFile`, `host_unshared`.
- `kernel/store`: the `Change` records of the browse API.
- Credentials: `MAY_READ` permission on the watched node.
- The kernel's wake objects (`request_wake_object`).

## Invariants and gotchas

- Keep the no-watch cost at one emptiness check per event: hooks that need extra lookups (the
  second walk, the directory of an open) must test `active` first.
- A hook given the store inode behind something reached through an overlay reports to the layer,
  where no overlay watch sees it, so overlay paths must pass the node's key (`key_of`,
  `file_key`).
- Store inode numbers and the keys of overlay nodes (top bits 10), FUSE nodes (11) and cgroup2
  files (01) must never collide; a new kind takes a free pattern and every `of_key`, `id_of` and
  `is_key` check must test both bits. cgroup2 nodes are never taken as unlinked.
- Watches hold `Node` values without counting: the overlay sweep keeps watched overlay nodes and
  FUSE's forget scan keeps watched FUSE nodes, so both scans must keep reading the registry.
- The overlay sweep gives up for a round when the registry is borrowed.

## Confirmed bugs / technical debt

- **Small leaks and ordering:** a delayed delete-self entry is never removed if every watch
  disappears before the last close; an event identical to the last one is merged even when the
  queue is full, where Linux queues `IN_Q_OVERFLOW`.
- **Identity and event gaps:** a watch is identified by its node, mount included, so one inode
  watched through two bind mounts gives two descriptors and each event twice (Linux keeps one mark
  per inode per instance); `IN_EXCL_UNLINK` on a FUSE directory drops every child event (the
  unlinked check asks the store about a FUSE key); fdinfo shows device 8:1 for every store-node
  watch, tmpfs files included; an `O_RDONLY|O_TRUNC` open truncates without `IN_MODIFY`;
  `chown(-1, -1)` that clears nothing still reports `IN_ATTRIB`; store FIFOs and cgroup2 files
  report `IN_OPEN` but never `IN_CLOSE_*`.
- **`IN_EXCL_UNLINK` filters the wrong events:** only directory watches honour it, keyed on the
  child's link count, so a file's own watch keeps reporting writes and closes after an unlink, a
  directory watch keeps reporting I/O through an unlinked name while another link remains, and a
  directory watch loses `IN_ATTRIB`/`IN_MODIFY` from fchmod, futimes and ftruncate of an unlinked
  child. Linux excludes exactly the open, access, modify and close events of an unlinked name, on
  both watches.
- **Executing a file reports nothing:** execve opens and reads the program (and a script's
  interpreter) without `IN_OPEN`, `IN_ACCESS` or `IN_CLOSE_NOWRITE`.
- **Overlay layers see little:** watches on a layer's own directories and files see, of what is
  done through the overlay, only creations (Linux's overlay changes the upper layer through the
  VFS, which reports copy-ups, removals, renames and attribute changes there too); a hard link
  made through the overlay and then removed leaves the file's watch reporting its deletion with
  its last name, where Linux's overlay keeps the link counted and never does.

## To verify

- Per-hook ordering against every Linux fsnotify call site (only spot-checked; the differential
  test `inotify` covers the common sequences).
