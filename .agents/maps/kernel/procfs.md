# kernel/procfs Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/procfs.md` for the paired decision history (it still holds the decisions on cgroups and device BPF made before they were split out into `kernel/cgroup`).
>
> Verified: 2026-11-11 (full mapping: 6 research concerns; 22 claims checked by 3 independent verifiers — 19 confirmed and 3 partial, corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: repoint dependencies to the split module maps)

## Responsibilities

`kernel/procfs` provides the synthetic filesystems, generated live from kernel state with nothing
stored:

- procfs: the per-process and per-thread trees, system-wide files, `/proc/sys` (sysctls, many
  scoped by namespace) and `/proc/<pid>/net`;
- sysfs: CPU topology (partly copied from the host), `/sys/class/net`, `/sys/block` (loop
  devices), overlayfs's module parameters, the cgroup mount point;
- devpts as a filesystem (instances, nodes, owners and modes), nsfs nodes and the node views of
  the mqueue filesystem;
- the cgroup2 node adapter: lookup, metadata, directory listing and dispatch to `kernel/cgroup`
  for interface-file content and writes, plus the procfs entries that show cgroup membership.

Root: `core/kernel/src/pseudo.rs`.

The cgroup tree, migration, controllers, change observation and device BPF belong to
`kernel/cgroup` (`cgroup.rs`, `bpf.rs`, `syscall/bpf.rs`). The shared synthetic node and
open/read/write contracts remain here; cgroup policy is described in that map.

Outside the module: the `Pseudo` trait, the walker, mounts and mountinfo order and magic-link
resolution (`kernel/vfs`); open/read/write dispatch for pseudo nodes (`kernel/files`, whose
`open_pseudo` and `refresh_pseudo` behaviour this map describes as procfs sees it); maps and
`/proc/<pid>/mem` semantics (`kernel/mm`, `kernel/ptrace`); smaps, pagemap and the Rss figures
(`kernel/oom`); id maps, `setgroups`, `timens_offsets`, `ns/*` and namespaces (`kernel/ns`);
`children`, state letters and the kernel-entry record behind `syscall` (`kernel/process`);
`/proc/kmsg`, `/proc/version` and ABI sysctls (`kernel/signal`); terminal behaviour (`kernel/tty`);
network semantics behind `/proc/net` and `net.*` (`kernel/net`, `kernel/netns`); the OOM killer
(`kernel/oom`); SysV IPC and message queues (`kernel/ipc`); locks (`kernel/locks`); loop devices
(`kernel/loop`); the mount table (`kernel/mount`).

## Key types

- `PseudoNode` — every synthetic node: devpts (`PtsRoot(dev)`, `Pts(dev, n)`, `PtsPtmx(dev)`),
  procfs (`ProcRoot`, `Global`, `SelfLink`, `ThreadSelf`, `Static(path)`, `Pid(pid, tid)`,
  `Entry(pid, tid, Entry)`, `Net`, `Fd`, `OpenFd` (the open file an fd link reaches when it is not
  in the store), `OpenExe` (a deleted program), `FdInfo`, `Ns` (the magic link), `NsFile(kind,
  id)` (the nsfs file), `NetConf`, `NetSysctl`), sysfs (`SysRoot`, `Sys`, `SysCpu`, `SysNet`,
  `SysBrif`, `SysBlock`), `Cgroup(id, name)` (an empty name is the directory), mqueue (`MqRoot`,
  `Mq`). Nodes are plain values, resolved again on every call.
- Tables: `GLOBALS` (15 top-level files), `ENTRIES` (34 per-process names), `TASK_ENTRIES` (the 30
  allowed under `task/<tid>`), `NsKind::ALL` (10), `PROC_NET` (18), `STATIC` (129 fixed paths
  under `/proc/sys` and `/proc/sysvipc`), `RECORDED` (40 sysctls), `SYS_STATIC` (23) and the sysfs
  file tables (`BLOCK_FILES`, `NET_FILES`, `BRIDGE_FILES`, `PORT_FILES`, `CPU_FILES`,
  `HOST_CPU_FILES`); `netconf::SYSCTLS` (145, `kernel/netns`) backs `NetSysctl`.
- `View` — a read-only view of the kernel for one reading thread (`tid`, `tgid`), taken under the
  BKL, with optional replacement credentials (`cred`, an opener's); implements `Pseudo`.
  `Offline` serves an empty root before boot.
- `FileKind::Proc` — an open synthetic file: mount, node, snapshot `data`, `fresh` flag, a weak
  address space for `mem`, the namespace reference of an nsfs file, the kernel-log reader for
  `/proc/kmsg`, the opener's credentials (`opener`, Linux's `f_cred`) and change state
  (`fd::ProcEvent`: a shared counter, what the file last reported and when its contents were
  made).
- `PtsOptions` (kept by the VFS per devpts instance) and the owner, mode and times kept on `Tty`.

## Public entry points

- `Pseudo for View`: `lookup`, `missing`, `parent`, `name`, `kind`, `readlink`, `follow`,
  `follow_dentry`, `magic`, `protected_symlinks`, `searched`.
- `pseudo::stat`, `node_ino`, `ino_of`, `fs_of`, `readdir`, `content`, `writable`, `unwritable_error`,
  `open_unwritable`, `unreadable`, `pollable`, `sysctl_kind`, `write`, `written`, `read_pagemap`,
  `read_mem`, `write_mem`, `describe`, `sockets`, `socket_ino`, `pty`, `group_of`, `comm_str`,
  `mark_deleted`, `recorded_value`, `uptime_ns`.

## Data flow / lifecycle

### Instances, lookup and metadata

- Each procfs mount is a new instance with its own device; if the mounter is in a non-initial pid
  namespace, the instance is bound to it (by device, so bind mounts share it), otherwise it uses
  the initial one. Mount options (`hidepid`, `subset`) are ignored.
- `/proc` lookup tries `self`, `thread-self`, the top-level static directories (`sys/`,
  `sysvipc/`), the global files, then a pid; readdir lists them in that order, then the processes
  visible in the instance's namespace, sorted (zombies until reaped; threads are never listed). A
  pid name is mapped through that namespace and may name any thread (`group_of`): `/proc/<tid>` is
  its process's directory seen through that thread (`status`, `comm`, `cwd` and `fd/` are the
  thread's, `stat` adds up the group's CPU time, `task/` lists the group; its ptrace-gated entries
  open on the process's address space with the check made against that thread), and it
  disappears when the thread exits. `self`/`thread-self` are `ENOENT` for readers not visible in
  the instance.
  Numeric names (pids, fds, terminals) are decimal digits only, at most 10 without a leading zero
  and at most `i32::MAX` (Linux's `name_to_int`).
- `/proc/<pid>` has every `ENTRIES` name except `children`; `task/<tid>` has the `TASK_ENTRIES`
  (no `task`, `pagemap`, `coredump_filter` or `timens_offsets`, plus `children`); both list in
  `ENTRIES` order. `task/<n>` resolves for a live thread of the group or an exited main thread
  (`leader_exit`).
- Inode numbers hash the node (low bit set); roots are 1 (except the mqueue root); devpts `ptmx`
  is 2 and terminal n is n+3; nodes under a pid mix in the thread group's serial, so a reused pid
  gets new inodes (`node_ino`, used by stat and getdents; `ino_of` alone, used by epoll fdinfo and
  inotify, omits the serial); nsfs files use the namespace id. Inodes are stable within one run.
- Modes: directories 0555 (0755 on sysfs, a cgroup's from its mkdir); `fd/<n>` links by the open
  mode as `proc_fd_instantiate` (0500 readable, 0300 writable, 0700 both, 0 for `O_PATH`); other
  symlinks 0777; `mem` 0600; `syscall`, `pagemap`, `auxv` and `/proc/kmsg` 0400; recorded sysctls
  and `NetSysctl` entries their table modes (`vm.drop_caches` 0200, `vm.mmap_rnd_bits` 0600);
  other writable files 0644; everything else 0444 (so `fd/`, `fdinfo/`, `ns/` are 0555, `environ`
  0444, `comm` and `projid_map` read-only); cgroup files their `FILES` modes; devpts nodes the
  terminal's mode and `ptmxmode`.
- Owners: per-process nodes (`Pid`, `Entry`, `Fd`, including the `fd/`, `fdinfo/`, `ns/`, `net/`
  directories) belong to the euid/egid of the thread (under `task/<tid>`) or the main thread, or
  to root of those credentials' user namespace when the process is not dumpable; `net.*` sysctls
  to root of the user namespace owning the network namespace when it maps one
  (`net_ctl_set_ownership`); cgroups to their creator; terminals to their owner; the entries
  inside `fdinfo/`, `ns/` and `net/`, reopened open files and everything else to root.
- Stat reports `st_blksize` 1024 for procfs and devpts paths, 4096 elsewhere (slave and master
  fds included). Times are "now" except devpts terminals; directories have nlink 2 (`task/` 2
  plus threads, an exited main thread included); sizes are 0 except sysfs and cgroup2 files
  (4096).
- Names path resolution does not find (`View::missing`, Starnix's `not_found`) are counted as
  unimplemented paths (`kernel/signal`) when Linux would have them: under the procfs root (not
  numeric names), the fixed `/proc/sys` and `/proc/sysvipc` trees (not interface names in
  `conf/`), an existing process's or thread's directory and its `net/` and `ns/`, the fixed sysfs
  tree (not interfaces, loop devices or `cpu<n>` names), and inside a CPU's, interface's (not
  `brif/`) or loop device's sysfs directory; and at the root of the boot `/dev` tmpfs. Process,
  thread, CPU, interface and loop numbers become placeholders (`/proc/<pid>/task/<tid>/x`,
  `/sys/devices/system/cpu/cpu<n>/x`); fds, terminals, message queues, cgroups and the files of
  a process that has gone are never counted.

### Opening, reading and writing

- Open (`open_pseudo` in `kernel/files`), in order: `O_TRUNC` asks for write permission too; nsfs
  files refuse any write intent (`EPERM`, before DAC); `/proc/sys/user/max_*` skip DAC and need
  `CAP_SYS_RESOURCE` in the opener's own user namespace to open for writing; the rest of
  `/proc/sys` decides as `proc_sys_permission`/`test_perm` do, not by DAC (below); other nodes
  check DAC against the synthetic stat. Then: for `mem`, `auxv`, `environ`, `maps`, `smaps`,
  `smaps_rollup` and `pagemap`, the process must exist (found through any of its threads with
  `group_of`, else `ESRCH`) and the named thread pass one ptrace check with
  filesystem credentials and no read/attach distinction (`EACCES`, as Linux's `proc_mem_open`);
  `mem` and `pagemap` also need a live thread (`ESRCH`); writing `setgroups` needs
  `CAP_SYS_ADMIN` in the process's user namespace (`EACCES`, `proc_setgroups_open`); devpts nodes
  on nodev mounts `EACCES`, then the cgroup device filter; `O_DIRECT` `EINVAL`; directories open
  read-only (`EISDIR` for a write mode); write modes on ro mounts `EROFS`.
- `/proc/sys` permission: the mode's owner bits when the euid is 0, the group bits when the egid
  or a supplementary group is 0, the others' bits otherwise, capabilities aside; a `net.*` entry
  gives its owner bits to anyone with `CAP_NET_ADMIN` over the owner of the reader's network
  namespace (`net_ctl_permissions`, so root of a container writes its namespace's).
- A node that is not writable still opens for writing when DAC passes (root) if it is a procfs
  file outside `/proc/sys`, and each write then fails: `EIO` for the files of proc entries (global
  files such as `cpuinfo` and `kmsg`, `/proc/sysvipc/*`, the single-record `netstat`, `snmp`,
  `sockstat` and `sockstat6` of `/proc/<pid>/net`; `proc_reg_write`), `EINVAL` for per-process and
  `fdinfo` files (no write operation). `/proc/sys` refuses it (`EACCES`, `O_TRUNC` included);
  sysfs (`kernfs_fop_open`) and the other `/proc/<pid>/net` files (`seq_open_net`) refuse only a
  write access mode, so `O_TRUNC` alone opens them; cgroup files are checked by DAC only (a
  read-only one opens for root, and the write is `EINVAL`); creating names `EACCES`.
- The content is generated at open with the opener's credentials and marked `fresh`, except
  write-only files (empty), a `NetConf` file whose generation fails (empty, the error reported by
  the first read) and `/proc/<pid>/syscall` (empty: it shows the thread when read, so
  `/proc/self/syscall` shows the read). Only a read or pread at offset 0 can regenerate: the first
  one keeps a fresh snapshot unless its change counter moved, later ones regenerate with the
  opener's credentials and the current reader's namespaces, root and cgroup root; reads elsewhere
  use the current snapshot, so a first read at a non-zero offset leaves the snapshot fresh (only a
  write marks it stale). The opener's credentials include their user namespace, which translates
  the ids shown; pid numbers follow the mount's pid namespace. lseek
  (`SEEK_SET`/`CUR`/`END` against the snapshot length) never regenerates. `pagemap`, `mem` and
  `/proc/kmsg` are computed at read time (`mem` from the address space held weakly since open,
  readable until it is freed; `pagemap` from the address space of the thread looked up by tid at
  each read); nsfs files cannot be read (`EINVAL`), nor can write-only files.
- Writes pass each call's buffer (each 64 KiB chunk, each iovec) to `pseudo::write` with the
  writer's credentials and namespaces (the id maps, `setgroups`, `timens_offsets` and cgroup
  migration also check the opener's), apply immediately, do not advance the position (except
  `mem`), and mark the snapshot stale. Per-process numbers use `proc_number`/`kstrto` (first 12
  bytes, base 0, `EINVAL`/`ERANGE`); most sysctls parse decimal after trimming.
- Change notifications (`fd::ProcEvent`): epoll accepts the files with a Linux poll operation
  (`pseudo::pollable`: global procfs files, `/proc/sys`, `/proc/sysvipc`, `/proc/<pid>/net`,
  mounts, mountinfo, every sysfs and cgroup2 file; other per-process files and nsfs are `EPERM`).
  mounts and mountinfo poll as `POLLIN|POLLRDNORM`, plus `POLLERR|POLLPRI` once the mount table
  has changed since the open or since the last change a poll or epoll reported (reading does not
  take it in); one counter (`Vfs::mount_events`) serves every mount namespace. sysfs and cgroup2
  files poll as always readable and writable, plus `POLLERR|POLLPRI` before their first read from
  the start. Cgroup change observation, the four notifying files and their inotify keys belong to
  `kernel/cgroup`; the shared open/read path consumes its counters. Other synthetic files never
  report inotify events.

### Per-process files

- Entries: `auxv`, `stat`, `status`, `statm`, `cmdline`, `comm`, `syscall`, `environ`, `limits`,
  `maps`, `mem`, `smaps`, `smaps_rollup`, `pagemap`, `mounts`, `mountinfo`, `cwd`, `exe`,
  `root`, `fd/`, `fdinfo/`, `task/`, `net/`, `oom_score`, `oom_score_adj`, `coredump_filter`,
  `cgroup`, `ns/`, `uid_map`, `gid_map`, `projid_map`, `setgroups`, `timens_offsets`, and
  `children` under `task/<tid>` (its `oom_score` and `oom_score_adj` are the process's, and
  writing the latter sets the process's, as Linux's tid_base_stuff). Absent: `oom_adj`, `io`,
  `wchan`, `sched*`, `personality`, `loginuid`/`sessionid`, `attr/`, `stack`, `cpuset`,
  `mountstats`, `timerslack_ns`.
- `syscall` (Linux's `proc_pid_syscall`) reads `kernel/process`'s kernel-entry record: `running`
  for a live thread that is not the reader and whose state is R (in guest code or kernel code);
  for one waiting in the kernel, or the reader itself, the syscall number with x0 to x5, sp and pc
  (pc after the `svc`), or a negative number with sp and pc outside a syscall (a fault, a kick, or
  a stop in signal delivery, which forgets the syscall and shows the restart pc); `-1 0x0 0x0` for
  an exited main thread (a zombie) or no record; `ESRCH` once another thread has gone (Linux
  releases it at once). Like `lock_trace`, it needs ptrace access to that thread, checked when
  read with the reader's own credentials rather than the opener's (`EPERM`).
- `oom_score` is the OOM killer's badness scaled against the host's RAM plus swap (`kernel/oom`'s
  `proc_score`): 0 to 2000, 0 for the built-in init, `oom_score_adj` -1000, a process without
  live threads and a vfork child its parent still waits for.
- `stat`: all 52 fields; the layout fields (code bounds, start of stack, data bounds, start of
  heap, arguments, environment; `kernel/mm`) go to readers passing the ptrace read check with
  filesystem credentials (6.1's `do_task_stat`), others getting 1 for the code bounds and 0 for the
  rest (0 everywhere for a process without an address space); fault counters, `kstkesp`,
  `kstkeip`, `wchan`, `processor`, priority and policy are 0; the `signal` field is the thread's
  and the group's pending sets; start time is the process's, in clock ticks plus the reader's
  time-namespace boottime offset; the thread's or the group's CPU times depending on the view
  (`/proc/<tid>` the group's); `tty_nr` from the controlling terminal.
- `status`: the thread's ids (the main thread's for `/proc/<pid>`) translated into the reader's
  user namespace (the reading thread's credentials, or the opener's when regenerated),
  `NStgid`/`NSpid`/`NSpgid`/`NSsid` from the instance's namespace down, capabilities,
  `NoNewPrivs`, `Seccomp`, signal masks (`SigPnd` the thread's, `ShdPnd` the group's); memory
  lines approximate (`VmPeak` = `VmSize` = `VmData`, `VmHWM` from max RSS, `VmRSS` the sum of
  `RssAnon`, `RssFile` and `RssShmem` from `Mm::resident_split`, `VmStk` the grows-down mappings,
  exe/lib/PTE/swap 0; omitted with no live task); `FDSize` the open fds rounded up to 64;
  `Cpus_allowed_list` is the thread's affinity (the main thread's in the process's `status`)
  within the online CPUs. `statm` is `size rss 0 0 0 rss 0`.
- `maps` and `smaps` escape a newline in a mapped file's path as `\012` (Linux's
  `seq_file_path`); file mappings show device `08:01`; smaps counts anonymous resident pages as
  dirty, file pages as clean, and copy-on-write-shared pages as shared with a divided Pss.
- `cmdline`/`environ` copy the argument and environment ranges of the current mm (empty over
  1 MiB or when unreadable); `comm` is read-only; `limits` lists the rlimits. The thread name is
  bytes, not text: `comm` and `stat` give it as set, `status` escapes only a newline and a
  backslash (Linux's `proc_task_name`).
- `fd/` lists the fds of the table of the thread the directory's pid names: the main thread's
  for `/proc/<pid>` and under `task/<tid>` (even for a thread with its own table), the thread's
  for `/proc/<tid>`; listing and lookup in `fd/` and `fdinfo/` and reading or following `fd/N`
  need the ptrace read check with filesystem credentials (`EACCES`, Linux's `proc_fd_permission`
  and `proc_fd_access_allowed`), as do `root`, `cwd`, `exe`, `ns/*` and `stat`'s layout fields,
  always against the credentials of the directory's pid (the main thread's under `task/<tid>`);
  `fd/N` links show the path stored at open with
  ` (deleted)` appended when nlink is 0; `fdinfo/N` has `pos`, `flags` (octal, with
  `O_CLOEXEC`), `mnt_id`, `ino`, the `lock:` lines of the locks held through that open file
  (`kernel/locks`'s `fd_locks`; only for files or fd tables that have taken locks) and per-kind
  lines (eventfd count, inotify watches, epoll `tfd:` lines, timerfd clock and times, signalfd
  mask, pidfd pids).
- `exe` reads as the program's path recorded at execve, with ` (deleted)` once the file's last
  link is gone or, for a program found through an overlay, once that name is removed; following
  it reaches the program itself even after a rename (an overlay program through its overlay node)
  and a deleted program through the host file recorded at execve (`OpenExe`), which a removed
  overlay program only runs from: opening it is `ENOENT`.
- `mounts`/`mountinfo` come from the mount table (`kernel/mount`) for the thread's namespace and
  root (an exited thread has none: `EINVAL` at open, as `mounts_open_common`), each mount's type
  as `Vfs::type_of` gives it (`fuse.<subtype>` for a FUSE mount with a subtype); mountinfo's
  optional fields are `shared:N`, `master:N` and `unbindable` (no `propagate_from`), and its
  superblock options start with the instance's `ro`/`rw` and `sync`, `dirsync`, `mand`,
  `lazytime` (`Vfs::sb_flags`), which `mounts` puts after its `ro` (the mount's or the instance's)
  as `show_vfsmnt` does; `cgroup` is the path relative to the reader's cgroup-namespace root;
  `net/` uses the target's network namespace.
- Writable: `oom_score_adj` (-1000..1000; going below the minimum needs `CAP_SYS_RESOURCE`, and a
  privileged write sets the new minimum), `coredump_filter` (masked to 0x1ff), `mem`, the id maps,
  `setgroups`, `timens_offsets`.
- Zombies keep their group without tasks: stat/status work (state Z), cmdline/environ/auxv are
  empty, `fd/` is empty, `cwd`/`root`/`exe`/`ns` are `ENOENT`, `mounts`/`mountinfo` are `EINVAL`
  to open and `mem`/`pagemap` `ESRCH` (they use the address space of the thread named, which has
  none; native 6.8 answers the same). A main thread that has exited while other threads run is
  shown the same way, as Linux's zombie leader: `task/` still lists it (first), `task/<pid>` and
  the process directory report Z with its name (kept in the group) and its own CPU time for
  `task/<pid>/stat`, and `Threads`/`num_threads` and `task/`'s link count include it; the process
  directory no longer falls back to another thread.

### System-wide files and `/proc/sys`

- `meminfo`: 16 fields copied from the host's `/proc/meminfo` (falling back to host `sysinfo`), not
  clamped to cgroup limits. `vmstat`: the host's file when the app may read it, else the page
  counts meminfo gives (free, slab, anon, mapped, file, dirty, shmem) and the event counters
  procps reads at 0. `sysinfo(2)` takes memory from the host but uptime (with the time
  namespace offset), loads and the process count from the guest.
- `cpuinfo`: one block per guest CPU with the guest-visible hwcaps (HWCAP2 names up to 6.1's) and
  MIDR, BogoMIPS 100. `stat`: guest threads' CPU time (plus exited threads') with idle derived
  from uptime, split evenly over CPUs; `procs_running` counts threads in guest code (at least 1);
  `intr`, `ctxt`, `softirq` (the total and Linux's 10 types), `procs_blocked` are 0; `processes`
  counts every new task, threads included (Linux's `total_forks`); btime is recomputed per read.
  `uptime` includes the reader's boottime offset. `loadavg` comes from the guest's own load
  average. `filesystems` is a fixed list (`nodev fuse` included, no `fuseblk` or `fusectl`);
  `cmdline` is `tuxport`; `cgroups` reports the v1 controller names with hierarchy 0 and the total
  cgroup count. `mounts` and `net` link into `self`. `swaps` is the host's file when the app may
  read it (as meminfo's swap lines are the host's), otherwise Linux's header line alone; `locks`
  lists every lock and lease (`kernel/locks`'s `proc_locks`) with pids in the instance's pid
  namespace.
- Absent: `partitions`, `devices`, `diskstats`, `interrupts`, `modules`, `slabinfo`, `buddyinfo`,
  `sysrq-trigger`, `zoneinfo`, `pressure/*`, `kallsyms`, `sys/fs/file-nr`,
  `protected_regular`/`protected_fifos`, Yama.
- Sysctl scoping: reads follow the reading thread's namespaces and writes the writer's. SysV IPC
  limits and `fs/mqueue/*` per IPC namespace; `hostname`/`domainname` per UTS namespace (one
  trailing newline stripped, over 64 bytes `EINVAL`); `ns_last_pid` per pid namespace (the
  process's, not the mount's); `net.*` per network namespace (`ip_forward` aliases
  `conf/all/forwarding`; `NetConf` directories per interface; entries flagged init-only exist only
  in the initial namespace); `user/max_*_namespaces` per user namespace (the reader's or writer's:
  the limits of `kernel/ns`'s namespace counts, `INT_MAX` by default in a new one, 0 to
  `INT_MAX`); everything else kernel-wide (`max_map_count`, `mmap_min_addr`, inotify limits,
  `aio-max-nr`, `lease-break-time`, `nr_open`, `threads-max`, `mount-max` (1 to `INT_MAX`; the
  mount limit of kernel/mount), `pipe-max-size`, `core_pattern`, `core_uses_pid`,
  `protected_symlinks`/`hardlinks`, `printk*`, `panic`). `dmesg_restrict`, `kernel.tainted`,
  `kptr_restrict`, `unprivileged_bpf_disabled` and seccomp `actions_logged` need `CAP_SYS_ADMIN`,
  `mmap_min_addr` `CAP_SYS_RAWIO`, all in the initial user namespace (`Cred::capable`), so root of
  a container never passes.
- Several writable sysctls are recorded without effect (`kernel.panic`, every `NetSysctl` entry,
  `ping_group_range`, `somaxconn`, `ip_local_port_range`; of the port sysctls only
  `ip_unprivileged_port_start` is used, by bind); `ping_group_range` keeps kernel group IDs,
  shown in the reader's user namespace (65534 when unmapped) and written from the writer's (an
  unmapped one is `EINVAL`); `kernel/pid_max` accepts writes and ignores them. The `RECORDED`
  table adds 40 more with Linux 6.1's modes, defaults and accepted ranges (`overflowuid`/`gid`,
  `vm.dirty_*`, `drop_caches`, `swtuxportss`, `overcommit_ratio`, `page-cluster`,
  `watermark_scale_factor`, `mmap_rnd_*bits`, `kernel.sysrq`, `randomize_va_space` (default 0:
  nothing is randomized), `kptr_restrict`, `perf_*`, `hung_task_*`, `sched_rt_*`,
  `sched_schedstats`, `panic_on_oops`, `core_pipe_limit`, `modules_disabled` (only 1 can be
  written), `fs.suid_dumpable`, `debug.exception-trace`), kept kernel-wide in `Kernel::sysctls`
  and read through `recorded_value`; all change nothing except `debug.exception-trace` (default 0,
  arm64's `show_unhandled_signals`, or 1 when the host asks through
  `MachineOptions::exception_trace`, as the app does), which turns on kernel/signal's
  unhandled-exception reports when non-zero, and `drop_caches`, whose 2 and 3 make `kernel/fuse`
  forget the FUSE nodes nothing uses at the end of the syscall (the host keeps the page cache);
  `drop_caches` is write-only (reads `EINVAL`). `kernel.modprobe` keeps a string, `kernel.tainted`
  only gains flags (of the 19 defined), and `vm.overcommit_memory` (0 to 2) is used: 1 turns off
  fork's commit check (`kernel/mm`). Constants: `file-max`, `cap_last_cap` 40, `ngroups_max`,
  `pid_max` 4194304, `osrelease` `6.1.0-tuxport`; `random/uuid` is fresh per read, `boot_id` fixed
  per boot, the entropy values 256.
- `/proc/<pid>/net/*` (`arp`, `dev`, `icmp`, `icmp6`, `if_inet6`, `ipv6_route`, `netstat`, `raw`,
  `raw6`, `route`, `snmp`, `sockstat`, `sockstat6`, `tcp`, `tcp6`, `udp`, `udp6`, `unix`) uses the
  target task's network namespace: sockets from every fd table plus in-flight AF_UNIX sockets,
  filtered by namespace, uids mapped for the reader; host interface counters and routes only in
  the initial namespace (new namespaces show zeros); `arp` is a header line.

### sysfs

- Tree: `block/`, `class/net/`, `devices/system/cpu/` (`online`, `possible`, `present`,
  `offline`, one `cpu<n>/` per guest CPU with synthesized `topology/` — each CPU its own core, one
  package — and `regs/identification/`), `cache/` and `cpufreq/` files copied from the host when
  the host lets the app read them (CPU numbers translated, missing files absent), the empty
  `fs/cgroup/` mount point, and `module/overlay/parameters/` with 6.1's Kconfig defaults, which
  are what the overlay does (`redirect_always_follow` Y, `redirect_max` 256, `check_copy_up`,
  `index`, `metacopy`, `nfs_export`, `redirect_dir`, `xino_auto` N; containerd adds `index=off`
  and BuildKit `redirect_dir=off` because the files exist). No `kernel/`, other modules, `dev/`,
  `bus/`, `firmware/`, `power/`, NUMA nodes, other classes or `uevent` files.
- `/sys/block/loop<n>/` exists for every loop device (`kernel/loop`): `dev`, `removable`, `ro`,
  `size` (sectors), `queue/` (logical, physical, minimum and hardware sector sizes, optimal I/O 0,
  `dma_alignment` 511, `rotational`, `read_ahead_kb`, `max_sectors_kb`, `rq_affinity`), and while
  the device is bound `loop/` (`backing_file` as the backing open file's display path, `offset`,
  `sizelimit`, `autoclear`, `partscan`, `dio`). The values come from `Loops::sysfs_read`; device
  directories are directories rather than symlinks into `/sys/devices/virtual/block`.
- The guest CPU count is a snapshot of the host process's affinity at kernel creation (capped at
  64); the same count drives sysfs, `/proc/cpuinfo`, `/proc/stat`, `status`, `sched_getaffinity`
  and cgroup `cpuset`; guest CPU i is host CPU `host_cpus()[i]`.
- `/sys/class/net/<if>/` follows the reader thread's network namespace; interfaces are directories
  (Linux uses symlinks into `/sys/devices`); bridges have `bridge/` and `brif/` (symlinks to the
  ports' `brport/`), ports `brport/`; only byte and packet counters of host interfaces and `lo`
  are real; `carrier` of a down interface is `EINVAL`, a removed interface `ENODEV`.
- sysfs is read-only (write opens `EACCES`, chmod/chown `EACCES`); one instance, so new mounts
  share the boot device and stacking on its root is `EBUSY`; mounting it needs `CAP_SYS_ADMIN`
  over the owner of the caller's network namespace.

### devpts, nsfs and mqueue nodes

- Each devpts mount is an instance (`PtsRoot(dev)`) with its own terminals and `PtsOptions`
  (`uid`, `gid`, `mode`, `ptmxmode`, `max`; defaults `mode=600,ptmxmode=000`; the boot instance
  `gid=5,mode=620,ptmxmode=666`; `newinstance` accepted and ignored; unknown options or unmapped
  ids `EINVAL`; remount does not change them; options are dropped with the instance's last mount,
  after which `BOOT`'s are reported).
- Opening `ptmx` (the instance's, or a `/dev/ptmx` node, which uses the devpts mounted on `pts`
  beside it) allocates the lowest free index (`ENOSPC` at `max`); the terminal's owner is the
  option's uid/gid or the opener's fsuid/fsgid, its mode the option's mode; chmod, chown and
  utimensat are stored on the terminal (on the root and `ptmx` they are `EACCES`). Nodes exist
  while the terminal does; readdir lists `ptmx` and every live terminal. Devices 136:n and 5:2;
  the root is 0555 (Linux 0755). fstat on a master fd always reports inode 2, mode 0666, rdev 5:2
  on the root filesystem's device, uid/gid 0 and zero times, whatever the instance and
  `ptmxmode`; a slave fd reports its terminal.
- nsfs: following `/proc/<pid>/ns/*` (after the ptrace-read check; `ENOENT` when the namespace has
  no id yet, as `pid_for_children` before its reaper) yields a regular 0444 file whose inode is
  the namespace id, device 0:4, statfs `NSFS_MAGIC`; opening finds the namespace among live ones,
  mount pins and other open nsfs files and keeps it alive; reading is `EINVAL`; bind-mounting it
  pins the namespace.
- mqueue: the root and queue nodes of an IPC namespace's queues take their stat from the queue
  directory and the queues; chmod, chown and utimensat are stored there.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/files` (pseudo open/read/write/lseek); `kernel/vfs` (walker and magic links, pseudo
  stat/getdents, chmod/chown/utimensat on devpts nodes); `kernel/mount` (synthetic mounts, devpts
  options and `mount_events`); `kernel/ns` (unshare/setns, nsfs files); `kernel/poll` (epoll
  refusal and readiness); the machine (`Offline` before boot); `kernel/signal` (`recorded_value`
  for `debug.exception-trace`, `mark_deleted` for unhandled-exception reports); `kernel/net`
  (`sockets`, `socket_ino`).

### Outbound

- Kernel state: thread groups and tasks, credentials (`permission`, `may_ptrace`, `capable`,
  `uid_out`/`gid_out`), pid namespaces (`vnr`, `find`), nsproxy, `mm` (sizes, residency, argument
  ranges, pagemap, forced writes), the kernel-entry record (`TaskSync`), `signal`, `tty` (`Tty`
  owner/mode/times, `new_pty`), `netdev`/`netconf`/`route`/`net`/`unix`/`traffic`, `ipc`
  tunables, `mqueue` limits, `seccomp`, `klog`, `loadavg`, the VFS (`reach`, `path_of`), the locks
  (`proc_locks` and `fd_locks`, `kernel/locks`), the loop devices' `Loops` (`kernel/loop`), the
  mount table (`mounts_from`, devpts options; `kernel/mount`), the OOM killer (`proc_score`),
  unimplemented-path counting (`stub::record`), `vfs::fuse::drop_caches`.
- `kernel/cgroup`: cgroup directory lookup and metadata use `Cgroups` and `file_mode`;
  `content` delegates to `file`, `proc_cgroup` and `proc_cgroups`; `write` supplies a `Writer`
  with the writer's namespaces and the opener's credentials. Device filtering and change
  observation are called by the file syscalls, not owned by this adapter.
- Host: `tuxport_hal` (`host_meminfo`, `host_vmstat`, `host_swaps`, `host_cpus`, `cpu_sysfs`,
  `clock_gettime`, `fill_random`, `thread_ctxt_switches`, routes, `tx_queue_len`); the engine
  (`guest_hwcap*`, `guest_midr`).

## Invariants and gotchas

- Content always reflects the kernel at generation time; nothing is cached across opens, and a
  regeneration uses the opener's credentials but the current reader's namespaces.
- `/proc/sys` lookups use the reader's namespaces; `/proc/<pid>/net` uses the target's.
- Cgroup node adapters must preserve `kernel/cgroup`'s directory modes, controller-dependent
  file visibility and opener-credential checks; policy changes belong in that module.
- Synthetic nodes have no store inode, so store-only operations (chown/chmod outside devpts and
  mqueue, xattrs, links) fail on them.
- `dev_ino` (epoll fdinfo) and inotify use `ino_of`, which omits the thread group's serial and the
  mount's device, so they can disagree with stat for pid nodes, later procfs instances and nsfs
  files.
- A lookup under `task/<tid>/fd` (or `fdinfo`) drops the tid: the node is the process's, and `..`
  from it leads to `/proc/<pid>/fd`.
- The kernel-entry record behind `syscall` is published with atomics and no sequence lock: a
  thread entering and leaving the kernel while another reads it could mix two entries, which only a
  thread shown as `running` can do.
- A dynamic directory added to a view must be excluded in `missing_path`, or each of its absent
  entries becomes a counted path.

## Confirmed bugs / technical debt

- **Per-thread fd views are wrong:** `task/<tid>/fd` and `fdinfo` show the group leader's table even
  when the thread has its own (clone without `CLONE_FILES`, `unshare(CLONE_FILES)`).
- **Change notifications are partial:** a mount change in any namespace marks every mounts and
  mountinfo reader (Linux counts per namespace), as do propagation-type changes (Linux does not
  touch the namespace for them); sysctl poll (`hostname`, `domainname`) never reports `POLLPRI`;
  other synthetic files report no inotify events (opens, reads, writes).
- **Regeneration uses the current reader's namespaces:** a file reread from offset 0 is generated in
  the reading task's namespaces, while Linux binds a network or IPC sysctl to the namespace it was
  looked up in, so a descriptor of `/proc/sys/net/*` or an IPC limit passed to a process in another
  namespace shows that namespace's value.
- **Fidelity gaps:** modes and owners differ (`environ` 0444, `fd/` and `ns/` 0555, `comm`
  read-only, `fdinfo/<n>` and `ns/*` owned by root, devpts root 0555, a master's fstat fixed);
  `/sys/class/net` follows the reader rather than the mounter's network namespace; procfs mount
  options are ignored; many standard files are absent; `kernel/pid_max` writes are ignored;
  `status` has no `Cpus_allowed` mask line.
- **No `/sys/dev/block` or block `uevent`s:** tools that go from a device number to sysfs
  (`tst_find_backing_dev`, so LTP ioctl_loop05; udev-style lookups) find nothing, and the root
  filesystem has no block device behind it at all.
- **Socket owners in `/proc/net/*` can differ from Linux:** the uid column is the fsuid of the
  first process (lowest tid) holding the socket in its fd table, where Linux shows the creator's
  (`sock_i_uid`); sockets passed on, or whose creator closed its copy, show another uid, and
  sockets only in `SCM_RIGHTS` messages show 0.
- **`/proc/zoneinfo`, `/proc/pressure/*` and `/proc/kallsyms` are absent:** low-memory daemons
  (systemd-oomd and oomd need PSI's `/proc/pressure/memory` and its poll triggers), zone readers
  and symbol lookups (perf, bpftrace) find nothing.
- **Access checks under `task/<tid>` use the process's credentials:** `fd/`, `fdinfo/`, the magic
  links and `stat`'s layout fields check the directory pid's (the main thread's) credentials,
  while owners and the open-time check of `mem` and `maps` use the thread's; with per-thread
  credentials (a thread that changed its ids) the two can disagree.
- **Per-thread values that are the process's:** `task/<tid>/stat`'s start time is the process's
  (Tuxport keeps none per thread); `/proc/<pid>/net/*` of a process whose main thread has gone (a
  zombie, or one whose other threads live) shows the initial network namespace rather than the
  process's.
- **pagemap follows the tid, not the address space opened:** it looks the thread up at each read,
  so after an execve it shows the new image (Linux pins the address space at open, as `mem` does),
  and a reused tid would show another process's mappings without a new ptrace check.
- **Writes never advance a synthetic file's position** (except `mem`): repeated writes on one
  descriptor keep taking effect, where Linux ignores a strict-mode sysctl write at a non-zero
  position and refuses a second id-map write (`EINVAL`; Tuxport's is `EPERM`, the map being set).
- **`/sys/class/net/<if>/statistics` leaks the initial namespace's counters:** host-interface and
  `lo` counters are read by name from the kernel-wide traffic table without a namespace check, so
  a new namespace's `lo` shows the initial loopback's counts, while `/proc/net/dev` and netlink
  show zeros there.
- **A reaped process's held directory behaves like a live or missing one:** through a kept
  `/proc/<pid>` fd, lookups give `ENOENT` (Linux `ESRCH`), listing still succeeds (Linux
  `ENOENT`) and the first read of an open file returns the content from open (Linux `ESRCH`); as
  the fd is resolved by number, it shows the new process once the pid is reused, defeating monitors
  that hold `/proc/<pid>` fds to avoid pid-reuse races.

## Open questions

- Should procfs honour `hidepid`/`subset` (containers sometimes mount with `hidepid=2`)?
- Should `meminfo`/`sysinfo` be clamped to cgroup `memory.max` (LXCFS style), or is following
  docker deliberate?
- Should `/sys/class/net` follow the mounter's network namespace, as Linux tags sysfs mounts?
- Should `task/<tid>` have `pagemap`, as Linux's tid_base_stuff is believed to?

## To verify

- Linux 6.1's permission rules for SysV IPC/mqueue sysctls in child user namespaces (believed
  to be refused until about 6.8).
- Whether reopening a deleted file through `/proc/<pid>/fd/N` (the open-file node) checks
  permission (pipes do not).
- Field-by-field fdinfo layouts against Linux (eventfd ids, timerfd formatting).
- The Linux 6.1 behaviour behind the items found by Starnix's syscall tests (a zombie leader's
  files, `proc_sys_permission`, write opens of read-only proc files)
  was observed on a native 6.8 kernel and attributed to 6.1 from knowledge of the kernel; so was
  which write each kind of read-only file fails with.
- The bounds Linux puts on the mqueue, `aio-max-nr` and `lease-break-time` sysctls (Tuxport accepts
  any positive value, or any number).
