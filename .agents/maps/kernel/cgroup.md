# kernel/cgroup Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/cgroup.md` for the paired decision history (not yet created; decisions from before the split are D4–D6, D9, D24, D26, D37, D41 and the cgroup parts of D1–D3, D13, D25, D31 and D38 in `.agents/decisions/kernel/procfs.md`; they stay in the parent's snapshot).
>
> Verified: 2026-11-14 (split out of the kernel/procfs full mapping of 2026-11-11; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-17 (targeted verification only since the full mapping; latest change: directory watches follow controller files added after registration)

## Responsibilities

`kernel/cgroup` implements the cgroup v2 tree and its device BPF programs:

- cgroup directories and interface files, membership and migration, controller delegation,
  limits, freezing and killing subtrees;
- observing cgroup file changes for poll and inotify;
- the bpf(2) subset for loading, attaching, inspecting and executing cgroup device programs.

Root: `core/kernel/src/cgroup.rs`, `core/kernel/src/bpf.rs`,
`core/kernel/src/syscall/bpf.rs`.

Outside the module: synthetic node lookup, metadata, listing and read/write adapters in
`pseudo.rs` (`kernel/procfs`); file syscalls and their device-check call sites (`kernel/files`,
`kernel/vfs`); mount creation (`kernel/mount`); cgroup namespaces (`kernel/ns`); clone and exit
(`kernel/process`), the freezer wait in signal delivery (`kernel/signal`); memory measurement
primitives, the OOM killer and victim selection (`kernel/oom`); poll and inotify mechanics.

## Key types

- `Cgroup` (name, parent, children, `subtree_control`, owner, mode, written `values`, pids and
  memory events, memory peak, `freeze`, attached device programs and their flags) and `Cgroups`
  (the tree; ids never reused, root 1; `observed` notifying files); `CONTROLLERS` (8 in Linux's
  order, of which cpuset, cpu, memory and pids are available), `FILES` (39 interface files with
  modes and controllers), `Writer` (writing thread, pid namespace, cgroup-namespace root, opener's
  credentials); `CgroupNs` (in nsproxy).
- `bpf::Prog` (id, instructions, name, tag, gpl, load time, loader's uid), `bpf::Registry` (weak,
  by id, ids never reused), `FileKind::Bpf`.
- `PseudoNode::Cgroup(id, name)` is the `kernel/procfs` adapter (empty name for a directory).
  `fd::ProcEvent` is the `kernel/files` open-file state consuming a cgroup change counter.

## Public entry points

- cgroup: `file`, `write`, `Cgroups::mkdir`, `rmdir`, `proc_cgroup`, `proc_cgroups`, `may_enter`,
  `pids_charge`, `memory_limits`, `measure_memory`, `memory_event`, `oom_group`,
  `effective_device_progs`, `hierarchy_allows_attach`, `file_mode`, `observe`, `observe_all`,
  `notify_changes`, `key`.
- bpf(2) (`syscall::bpf::bpf`) and `check_device`.

## Data flow / lifecycle

### cgroup v2

- One global tree; the root (id 1, owned by root, 0755) looks like a docker container's root
  under a cgroup namespace with cpuset, cpu, memory and pids available. cgroup2 is mounted
  read-write at boot on sysfs's `fs/cgroup` (options `nsdelegate,memory_recursiveprot`); any
  mount needs `CAP_SYS_ADMIN` over the owner of the caller's cgroup namespace and is rooted at
  that namespace's root. A directory holds its children plus the core files and those of the
  controllers its parent enables (the root: all available ones).
- mkdir: `EROFS` on ro mounts; `EEXIST` for `.`, `..`, existing names and interface-file names;
  write and search on the parent; newline in the name `EINVAL`; ancestors' `cgroup.max.depth` /
  `max.descendants` `EAGAIN`; the new cgroup and its files belong to the creator's fsuid/fsgid,
  which is the only way to own one (path chown/chmod `EACCES`, or `EROFS` on ro mounts; fd-based
  `EBADF`), and the directory has the mode mkdir gave (umask applied, `S_IRWXUGO|S_ISVTX`), which
  path lookups through it, chdir and chroot check for search, as kernfs does. So only privileged
  processes create top-level cgroups. rmdir: write and search on the parent; children or live
  processes `EBUSY` (zombies do not count); the root `EBUSY`; removal is immediate, with no dying
  state. Creating plain files is `EACCES`, renaming `EPERM`.
- Membership: every process starts in the root; fork places the child in the parent's cgroup or a
  `clone3(CLONE_INTO_CGROUP)` target after `pids_charge` (`EAGAIN`) and starts it frozen if the
  target is freezing; the target has to exist (`ENODEV`), the caller must be allowed to write its
  `cgroup.procs` and that of the nearest cgroup above both it and the parent's (`EACCES`, judged
  by mode and owner as an open would be), both must lie inside the caller's cgroup-namespace root
  (`ENOENT`), the target must have no controllers enabled for children (`EBUSY`), and a thread
  cannot go elsewhere than its process (`EOPNOTSUPP`). Zombies stay until reaped;
  `CLONE_NEWCGROUP`/unshare roots the new namespace at the current cgroup. The BKL is held from
  the charge until the child is registered, so no fork escapes a concurrent `cgroup.kill`.
- `cgroup.procs` writes: pid 0 is the caller; pids resolve in the writer's pid namespace
  (`ESRCH`); kernel init `EINVAL`; the opener's credentials must allow writing the `cgroup.procs`
  of the nearest cgroup above both source and destination (`EACCES`, Linux's
  `cgroup_procs_write_permission`; the destination's own was checked at open); source or
  destination outside the writer's cgroup-namespace root `ENOENT`; a non-root destination with
  controllers enabled for children `EBUSY`; then the group moves (`pids.max` is not checked, and
  the process counts in the destination's `pids.current` at once, the count being recomputed from
  membership) and freezing is re-evaluated. Reads of `cgroup.procs`/`cgroup.threads` print ids in the reader's pid namespace,
  0 for ones it cannot see (Linux's task_pid_vnr). `cgroup.threads` only accepts moves within the
  same cgroup (`EOPNOTSUPP` otherwise); `cgroup.type` is always `domain` (`threaded` is
  `EOPNOTSUPP`).
- Writes, in order: a removed cgroup `ENODEV`; on the root and on the writer's namespace root only
  `cgroup.procs`, `cgroup.threads` and `cgroup.subtree_control` are writable (`EPERM`); a
  read-only file `EINVAL`. `cgroup.subtree_control`: `+name`/`-name` tokens (anything else,
  including a token starting with a multi-byte character, `EINVAL`); enabling a
  controller not in `cgroup.controllers` `ENOENT`; enabling while the cgroup has processes `EBUSY`
  (except at the real root); disabling one a child still delegates `EBUSY`; disabling clears the
  children's stored values, events and peak for it. Limits are format-checked as Linux reads them
  (`memory.*` with size suffixes rounded to pages, `pids.max` up to 2^22, `cpu.max` with a period
  of 1000 to 1000000, a quota alone keeping the stored period as `cpu_max_write` does, 100000 by
  default, `cpu.weight`, `cpuset.cpus` against the guest CPUs, and so on) and read back
  as stored; a stored value always wins over the computed default.
- Enforced: `pids.max` (fork and thread creation; `pids.current` counts live threads plus one for
  a zombie or an exited main thread), `memory.max` (the root shows the OOM killer's limit and measurement; a child's
  usage is its members' resident memory with what they share counted once (`kernel/oom`'s
  `Usage`), checked periodically by the OOM killer and immediately when a lower limit is written;
  `memory.oom.group` widens the kill), `cgroup.freeze` (threads stop before returning to guest
  code, waits return and restart after thaw; `cgroup.events` reports frozen once every task has
  stopped or is a vfork parent waiting for its child, as Linux counts that wait), `cgroup.kill`
  (SIGKILL to every live process in the subtree), `cgroup.max.depth`/`max.descendants` (at
  mkdir), the device programs. Everything else (`memory.high`, `cpu.max`, `cpuset.cpus`, …) is
  format-checked and stored.
- `/proc/<pid>/cgroup` is `0::<path>` relative to the reader's cgroup-namespace root;
  `/proc/cgroups` reports hierarchy 0 for the v1 controller names.

### bpf(2) and device programs

- Commands: program load, attach, detach, query, get-fd-by-id, info by fd (no maps or links).
  When `unprivileged_bpf_disabled` is nonzero (default 2; once 1 it cannot change) every command
  needs `CAP_BPF` or `CAP_SYS_ADMIN`; loading a device program always needs one of them and also
  `CAP_NET_ADMIN` or `CAP_SYS_ADMIN` (6.1's `is_net_admin_prog_type`, `EPERM`); query needs
  `CAP_NET_ADMIN`, get-fd-by-id `CAP_SYS_ADMIN`; all in the initial user namespace. Attach and
  detach check nothing on the target cgroup fd (any live cgroup directory fd, `O_PATH` included),
  and detaching a non-multi program needs no program fd. Attach follows Linux's flag rules
  (`OVERRIDE`, `MULTI`, `REPLACE`; an ancestor's non-override program forbids attaching below,
  mixed flags `EPERM`, at most 64 programs).
- Only `BPF_PROG_TYPE_CGROUP_DEVICE`. The verifier allows forward jumps only and requires every
  instruction to be reachable; no maps, bpf-to-bpf calls or atomics; the stack holds scalars
  (512 bytes, aligned); the context is read-only (12 bytes, aligned, through the unmodified
  pointer); pointer arithmetic is limited to constant offsets; R0 must be 0 or 1 at exit; seven
  helpers. The interpreter treats any out-of-range access as returning 0 (deny).
- Programs live while an fd or a cgroup holds them (rmdir drops a cgroup's). At device open
  (except rdev 0), devpts terminal and `ptmx` opens by path and mknod (after `CAP_MKNOD`,
  whiteouts exempt), the effective programs of the caller's cgroup run with its real uid and gid:
  walking up from the cgroup, the first programs found (its own, or else the nearest ancestor's,
  whatever its flags), then those of further ancestors that attached with `MULTI` (Linux's
  `compute_effective_progs`); every one must return 1, else `EPERM`. `TIOCGPTPEER` runs none, as
  Linux's `ptm_open_peer` skips the permission check.

### Synthetic node and open-file boundary

- `kernel/procfs` maps cgroup directories and files to `PseudoNode::Cgroup`, gets their names
  from `Cgroups::files`, modes from the cgroup or `file_mode`, and owners from the creator.
  File sizes are 4096 and times are "now"; the node adapter has no store inode.
- The file syscalls check mount flags and DAC. Read-only cgroup files can still open for root
  in a write mode, with writes failing `EINVAL`; write-only files cannot be read (`EINVAL`).
- The shared synthetic-file path generates content at open (empty for write-only files). The
  first read from offset 0 keeps that fresh snapshot unless the change counter moved; later
  reads from offset 0 regenerate with the opener's credentials and the current reader's
  namespaces and cgroup root. Other reads use the snapshot. `pseudo::content` delegates to
  `file`, `proc_cgroup` and `proc_cgroups`; `pseudo::write` delegates to `write`, supplying the writer's pid namespace
  and cgroup root plus the opener's credentials for migration. Writes apply each buffer
  separately, do not advance the position and mark the snapshot stale.

### Change observation

- Cgroup2 files poll as readable and writable, plus `POLLERR|POLLPRI` before their first read
  from the start and, for `cgroup.events`, `memory.events`, `memory.events.local` and
  `pids.events`, after a content change until read again. `kernel/files` attaches the counter
  returned by `observe` to `fd::ProcEvent`; `kernel/poll` consumes its readiness state.
- A task starting, exiting, migrating or changing its freezer participation marks
  `cgroup.events` in the affected groups and their ancestors. Freeze requests also mark
  observed descendants. Pids and memory events mark the files whose counters they change;
  controller disable and rmdir mark observations whose files disappear. Enabling pids or
  memory registers the newly visible files for existing watches on the child directories.
- A per-kernel flag lets `notify_changes` skip the observation table when clean. Otherwise it
  compares only marked files, after BKL syscalls, thread exits,
  freezer stops and timer-thread checks. Multiple changes to a file between checks yield at
  most one counter increase. A change wakes pollers and reports `IN_MODIFY` to watches
  on the file and its directory. A write to any cgroup2 file reports `IN_MODIFY` too,
  from the file-syscall adapter.
- `key` gives cgroup2 files and directories inotify keys with top bits 01; store inodes, overlay
  keys (10) and FUSE keys (11) do not collide with them. Removed or no-longer-observed files
  are forgotten at the next check of that file. Until then a closed file may retain a snapshot;
  a new observer reuses it only if an open fd or a watch on the file or directory still exists.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/procfs`: synthetic node lookup, metadata and listing; cgroup interface content and
  writes; `/proc/<pid>/cgroup` and `/proc/cgroups`.
- `kernel/vfs`: cgroup mkdir/rmdir and device checks at mknod; `kernel/files`: device checks at
  opens (including devpts), file-change observation and write notifications.
- `kernel/process`: clone/clone3 placement, pids charge, inherited freezer state, change checks
  after BKL syscalls and at thread exit; `kernel/time`: change checks on the timer thread;
  `kernel/signal`: freezer stops.
- `kernel/ns`: cgroup-namespace roots; `kernel/mount`: the cgroup2 mount's root node.
- `kernel/oom`: limits, membership-based memory measurement, OOM-group policy and events.
- `kernel/inotify`: watched files and directories, `observe`/`observe_all` and keys.
- The syscall dispatcher: bpf(2).

### Outbound

- `kernel/process`: task and thread-group membership and lifecycle, BKL and wake objects;
  `kernel/signal`: freezing/waking and killing groups; `kernel/ns`: pid translation and namespace
  roots; `kernel/cred`: DAC and capability checks, the writer/opener credentials and BPF helpers'
  real uid/gid.
- `kernel/oom`: `Usage` for shared memory counted once, the root limit and measurement,
  memory events and immediate OOM handling after lowering `memory.max`.
- `kernel/files`: BPF program fds and cgroup target fds; `kernel/procfs`: the target fd's
  `PseudoNode::Cgroup` and the `unprivileged_bpf_disabled` sysctl; guest memory access for
  bpf(2) attributes and program data.
- `kernel/inotify`: watched keys and `kernfs_modified`; `kernel/signal`: kernel log messages.

## Invariants and gotchas

- Cgroup ids and BPF program ids are never reused. A program stays alive through its fds or
  cgroup attachments; the registry itself keeps weak references.
- The BKL covers pids charging through child registration. Fork cannot escape a concurrent
  `cgroup.kill`; a vfork parent waiting for its child counts as frozen.
- Migration checks the opener's credentials, but pid resolution and the visible root follow
  the writer's namespaces. Namespace roots do not create separate cgroup trees.
- Stored controller values do not imply enforcement: only the limits and operations listed
  above act on processes. `memory.current` shares the OOM module's accounting contract.
- Call sites in `kernel/files` and `kernel/vfs` must run the device check at the documented
  opens and mknod paths. Pseudo-node adapters must keep directory search permissions and
  controller-dependent file visibility; they do not own cgroup policy.
- A mutation that can change a notifying file must mark it before a checkpoint can run.
  Membership changes affect ancestors on both sides of a move. A vfork wait or a change
  in a task's killed flag can affect freeze completion without changing membership.

## Confirmed bugs / technical debt

- **Change notifications are partial:** a cgroup2 change is noticed when the next syscall,
  thread exit, freezer stop or timer-thread pass looks, and changes that come and go between
  two looks are missed; `cgroup.stat` and other files Linux notifies in later kernels are not
  watched. A directory watch can also receive an inherited freeze notification for a child
  event file never looked up, where the native 6.8 VM reported it only with the child dentry
  instantiated.
- **Delegation and controller gaps:** cgroup directories cannot be chmod'ed or chown'ed;
  threaded cgroups are unavailable; limits such as `cpu.max`, `cpu.weight`, `cpuset.cpus` and
  `memory.high` are stored without enforcement.
- **cgroup write and read edge cases differ from kernfs:** writes are checked against
  the global file table, not the cgroup's current files, so a descriptor opened before a
  controller was disabled still stores a value (which returns when the controller is re-enabled;
  a `memory.max` one runs the OOM check) where Linux gives `ENODEV`; a regenerating read of a
  removed cgroup's file is `EINVAL` (Linux `ENODEV`).
- **mknod on FUSE skips the device programs:** it returns to `kernel/fuse` before the cgroup device
  check, which checks `CAP_MKNOD` itself but exempts every device 0:0, block ones included (Linux
  exempts only the character whiteout).

## Open questions

- Whether `meminfo`/`sysinfo` should be clamped to the cgroup memory limit remains an open
  question in `kernel/procfs`.

## To verify

- Linux 6.1's permission rules for cgroup device program attach/detach (believed to need no
  capability since CAP_BPF; Tuxport checks nothing on the target cgroup).
- `ENODEV` from removed cgroups was observed on native 6.8 and attributed to Linux 6.1 from
  knowledge of the kernel; the version-specific behavior remains unverified.
