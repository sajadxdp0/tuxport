# Synthetic filesystems (kernel/procfs) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/procfs"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b). D1–D8 were seeded by hand from the retired design
> doc §8.2 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Constraint index (compressed active decisions)

> Full text of D2, D3, D6–D8, D10–D15, D17–D24, D26, D27 and D29: `git show 260512d530d136e70f10564e248f7db876f07062:.claude/decisions/kernel/procfs.md`

- **D2** — Every procfs mount is a new instance with its own device and the mounter's pid namespace and every devpts mount has its own terminals and options, while sysfs and cgroup2 share the boot instance; procfs mount options (`hidepid`, `subset`) are ignored, and a terminal must report its own instance's device consistently for ttyname (rounds 105, 167)
- **D3** — `/proc/meminfo` and `sysinfo` copy the host's memory figures with the real limit in cgroup `memory.max`, and the guest sees the host process's CPUs with a uniform synthesized topology, so programs sizing themselves from meminfo alone see more than the OOM killer allows, and `/proc/cpuinfo`, sysfs, `/proc/stat` and `sched_getaffinity` must agree on the CPU count (rounds 39, 46, 58, 68)
- **D6** — bpf(2) supports only `BPF_PROG_TYPE_CGROUP_DEVICE` programs (load, attach, detach, query, get-fd-by-id, info) with a verifier and errors after 6.1 and an interpreter, loading needing `CAP_NET_ADMIN` or `CAP_SYS_ADMIN` besides `bpf_capable()`, so runc and crun can attach their device filters while maps, links and every other program type are unavailable (round 166; 5d7b936)
- **D7** — devpts terminals keep their own owner, group, mode and times, changeable with chown, chmod and utimensat for sshd's `pty_setowner`, defaulting from the instance's mount options and gone with the terminal (rounds 64, 167)
- **D8** — `/proc/net` links to `self/net` and each `/proc/<pid>/net` shows the target's network namespace (counters and routes are real only in the initial one), while `/proc/sys/net` follows the reader's (round 96)
- **D10** — `/proc/stat`'s btime is recomputed in nanoseconds on every read as realtime minus the uptime with the reader's boottime offset, so it stays stable and follows the time namespace and host clock steps, though it can still be one second off when the boot instant's fraction falls between the two clock reads (eb3bc8b)
- **D11** — smaps' VmFlags show `dc` and `wf` in Linux's bit order for kernel/mm D19's fork advice (e020977)
- **D12** — Any thread id resolves under `/proc` through `group_of` (by group id or the task's tgid), so `/proc/<tid>` shows the thread with its group's Tgid and stat sums the group, and code outside procfs that treats a pid node's id as a thread-group id can still miss threads (173da76)
- **D13** — pseudo.rs, cgroup.rs, bpf.rs and syscall/bpf.rs read in the order their code runs, code appended at a file's end drifts from that order, and syscall/mqueue.rs is now kernel/ipc's (a63a7b4; registry since 4140c75)
- **D14** — Loop devices appear as plain directories under `/sys/block/` with dev, removable, ro, size, `queue/*` and, while bound, `loop/*` from `Loops::sysfs_read`, with no `/sys/dev/block` or uevents, so ioctl_loop05 and udev-style lookups still fail (a26c8b4)
- **D15** — `/proc/sys/vm/mmap_min_addr` is a global sysctl written with `CAP_SYS_RAWIO`, VmStk comes from grows-down VMAs, mapped file names print newlines as `\012`, and `/proc/<pid>/mem` grows stacks within the accessor's `RLIMIT_STACK`, while VmData, VmExe and VmLib stay approximate (e03c0cc)
- **D17** — An exited main thread shows as Z through `ThreadGroup::leader_exit` (kernel/process D21) until the process is reaped, with exe `ENOENT`, mounts and mountinfo `EINVAL`, mem and pagemap `ESRCH` at open and task-needing entries behaving as for a zombie, so nothing may reach a live thread's state through the process directory after its leader exits (79a60cf, 70c1bc6, 5d23297)
- **D18** — `/proc/vmstat` copies the host's file through hal (D13 there), or a 16-line fallback with zero event counters, which phones are likely to show since Android may refuse the file to apps (60921b2)
- **D19** — `comm_of` returns bytes, and comm, stat and status are built as byte vectors with status escaping the name as Linux does, while log lines elsewhere still show names lossily (6591eb6)
- **D20** — status prints RssAnon, RssFile and RssShmem from kernel/mm's `resident_split` (D26 there) with VmRSS as their sum, while statm and stat use `resident_pages` and can differ slightly (566da95)
- **D21** — `/proc/sys/fs/mount-max` reads and sets the kernel-wide mount limit (1 to `INT_MAX`, else `EINVAL`), and mounts and mountinfo are generated through `Vfs::mounts_from` (kernel/vfs D30) rather than a rescan per mount (0a4712e)
- **D22** — `/proc/sys/user/max_*_namespaces` read and write the caller's own user-namespace limits (0 to `INT_MAX`), open for reading to anyone and for writing to holders of `CAP_SYS_RESOURCE` in their namespace whoever owns the files (kernel/process D28), with the namespace taken at read time rather than from the opener (6d05aa9)
- **D23** — `may_read` gates lookup and listing of `/proc/<pid>/fd` and fdinfo and following fd links, numeric names accept only plain digits up to `i32::MAX`, and fd links take their modes from the open mode, so other users' descriptors give `EACCES` as on Linux (5d23297, 5d7b936)
- **D24** — cgroup files show ids in the reader's pid namespace through a `vnr` closure (0 for invisible processes), and writes to a removed cgroup fail with `ENODEV` (5d23297)
- **D26** — `cgroup.procs` migrations check the common ancestor with the opener's credentials and `CLONE_INTO_CGROUP` checks the target and the common ancestor with the caller's, after `cgroup_procs_write_permission` and `cgroup_css_set_fork`, so non-root runtimes placing children into root-owned cgroups get `EACCES` (b43ff37)
- **D27** — status's Cpus_allowed_list lists the online CPUs in the thread's affinity mask, with still no Cpus_allowed mask line (5d7b936)
- **D29** — `/sys/module/overlay/parameters/` holds read-only files with 6.1's Kconfig defaults so that containerd and BuildKit add `index=off` and `redirect_dir=off`, though `redirect_always_follow` says Y while lower-layer redirects are not followed (450fb00)

## Active (full detail)

### D1: Synthetic files are generated from kernel state when opened, like seq_file
- **What**: Nothing is stored; opening a procfs, sysfs or cgroup file generates its whole content (`syscall` at its first read instead, D40), later reads use that snapshot, and a read from offset 0 generates it again.
- **Why**: Linux's seq_file gives the same effect for a file read in one go, and generating from live kernel structures keeps every file consistent with the kernel layer without bookkeeping.
- **Tradeoffs**: Large files are generated in full per open; of the files that change, only mounts, mountinfo and four cgroup2 files notify pollers (D38).
- **Watch out**: Regeneration uses the opener's credentials (D25) but the current reader's namespaces.
- **Source**: design review 2026-10-24 (§8.2); f6a82cc, affb1e2; f1212d8 (`syscall`)

### D4: The root cgroup looks like a docker container's root
- **What**: `/proc/<pid>/cgroup` paths are relative to the reader's cgroup-namespace root, the root offers cpuset, cpu, memory and pids, and only `cgroup.procs`, `cgroup.threads` and `cgroup.subtree_control` are writable there; `memory.max`/`memory.current` at the root are the OOM killer's limit and measurement.
- **Why**: JVMs and Node.js recognise cgroup v2 and size heaps from it ("No metrics available" before); dockerd, podman, runc, crun and systemd then needed a real hierarchy with migration, built after Linux 6.1's `kernel/cgroup/cgroup.c`.
- **Tradeoffs**: No threaded cgroups and no chmod/chown delegation, and only four cgroup2 files report changes (D38).
- **Watch out**: Mounts of cgroup2 are rooted at the caller's cgroup-namespace root.
- **Source**: rounds 58 (`58-cgroup`), 159 (`159-cgroup2-rw-mount`), 163 (`163-cgroup-tree`); b43ff37, affb1e2

### D5: Only pids, memory, freeze, kill and the device filter are enforced
- **What**: `pids.max` stops fork and thread creation, `memory.max` is enforced by the OOM killer's periodic check (and immediately when lowered) against a `memory.current` that counts what the members share once (kernel/mm's `Usage`, kernel/mm D44), `cgroup.freeze` stops threads transparently and counts a parent waiting for its vfork child as frozen (`Task::frozen`), `cgroup.kill` kills the subtree; other limits are format-checked and stored.
- **Why**: These are what `docker run --pids-limit`/`--memory`, podman's equivalents, `docker pause` and systemd's `TasksMax=`/`MemoryMax=` write. Host threads' scheduling is not under Tuxport's control, so `cpu.max`, `cpu.weight` and `cpuset.cpus` have nothing to act on.
- **Tradeoffs**: `memory.max` can be exceeded by tens of MB before the check notices; `memory.current` is no longer the plain sum of the members' VmRSS; `memory.high`/`low`/`min` and swap limits do nothing.
- **Watch out**: Freezing must not look like a stop signal: parents and wait must not see it, and interrupted syscalls restart after thaw; `Task::frozen` also means waiting for a vfork child, so a new reader must not take it for a freezer stop.
- **Source**: rounds 164 (`164-cgroup-limits`), 165 (`165-cgroup-freeze`); b593172, 1d3d559

### D9: kernel/procfs owns synthetic filesystem views and cgroup node adapters
- **What**: kernel/procfs owns pseudo.rs and shared synthetic-node contracts, while cgroup tree policy, interface behavior and device BPF belong to kernel/cgroup.
- **Why**: Synthetic filesystem views and the substantial cgroup policy implementation need separate maintenance boundaries.
- **Tradeoffs**: Adapter and policy changes require both maps, and pre-split cgroup decisions remain here under their original tags; tracing implementation belongs to kernel/ptrace while procfs memory-file handling remains here.
- **Watch out**: Bugs confirmed at the split or found since stay open in the map, among them per-thread fd views showing the leader's table, access checks under `task/<tid>` using the main thread's credentials, pagemap following the tid rather than the address space opened, writes that never advance a file's position, `/sys/class/net` statistics leaking the initial namespace's counters, partial change notifications, regeneration in the current reader's namespaces and no zoneinfo, pressure or kallsyms files.
- **Source**: c5236a1 (bug list since updated by 173da76, 4e64e74, 70c1bc6, 5d23297, f6a82cc, b43ff37, 5d7b936, 2aca260, 6d9a9e8, f5c09f7, affb1e2 and b593172; map fully refreshed in f1212d8); 45be9fae; 20137e4c; cf029f9a; d053be24; 60790c14; 39e55b4b

### D16: Common sysctls exist, and root can write the read-only ones
- **What**: 40 recorded sysctls with 6.1's ranges are kept in `Kernel::sysctls`, with kernel.modprobe, kernel.tainted and vm.overcommit_memory (kernel/mm D24) handled on their own and kptr_restrict, drop_caches and mmap_rnd_bits given Linux's permissions; debug.exception-trace starts at arm64's 0 and is read through `recorded_value` by kernel/signal's unhandled-exception reports (kernel/signal D23), and writing 2 or 3 to drop_caches requests a FUSE node sweep while the host keeps the page cache (kernel/fuse D2).
- **Why**: Setup scripts write these sysctls and checkers read them, and `sysctl -p` and `sysctl -w` failed where Linux succeeds because the files were missing or read-only for root; exception-trace had x86's default of 1 where arm64 Linux 6.1 starts at 0, and drop_caches is the one way to make Linux forget unused FUSE inodes on demand.
- **Tradeoffs**: Apart from debug.exception-trace and drop_caches the recorded values change nothing, and randomize_va_space starts at 0, true of Tuxport's fixed layout, where Linux starts at 2.
- **Watch out**: Programs that read a recorded value back may act as if the setting applied, and `recorded_value` panics on a path missing from the table, so kernel/signal's `EXCEPTION_TRACE` must stay equal to its path there.
- **Source**: 42d2ffe, 7b7f73b, 3957a03

### D25: Write opens of read-only procfs files follow each file kind, and contents keep the opener's credentials
- **What**: Root may open read-only system-wide proc files and `/proc/sysvipc` for writing and their writes fail with `EIO` (`EINVAL` for per-process and fdinfo files), `/proc/<pid>/net`'s seq files and sysfs refuse only a write mode, `/proc/sys` refuses `O_TRUNC` too and namespace files are `EPERM`, while `FileKind::Proc` keeps the opener's credentials and regeneration builds its view with them, namespaces still coming from the current reader.
- **Why**: Linux lets root open read-only procfs files for writing (std::fstream and fopen "r+" on `/proc/cpuinfo` or status) and keeps the opener's credentials (`f_cred`) for regenerated content, while Tuxport refused the open and regenerated with the reader's credentials.
- **Tradeoffs**: One more `Rc<Cred>` per open procfs file, and `/proc/sys` values regenerated through a passed descriptor still follow the reader's network and IPC namespaces.
- **Watch out**: The per-kind errors were observed on native 6.8 and attributed to 6.1, and cgroup files keep their own rule (read-only ones open for root), which may not match `kernfs_fop_open`.
- **Source**: f6a82cc

### D28: `/proc/sys` opens by `test_perm`, and net sysctls belong to the network namespace's owner
- **What**: Opening a `/proc/sys` node uses `test_perm`'s bits (owner bits for euid 0, group bits for group 0; user/max_* keeps D22's rule), net.* sysctls are owned by root of the user namespace that owns the network namespace (`net_ctl_set_ownership`) and give the owner bits to holders of `CAP_NET_ADMIN` over it, ping_group_range shows its kernel group ids in the reader's namespace (65534 when unmapped, kernel/netns D14), and `/proc/stat`'s softirq line has the total and Linux's 10 per-type counts (all 0).
- **Why**: Root of a user namespace could not write the sysctls of a network namespace it owns, `CAP_DAC_OVERRIDE` wrongly opened `/proc/sys`, and BuildKit refused `/proc/stat`'s one-number softirq line, failing every docker build.
- **Tradeoffs**: `/proc/sys` now ignores capabilities other than `CAP_NET_ADMIN` on net.*, which can refuse callers that used to pass through `CAP_DAC_OVERRIDE`, as on Linux.
- **Watch out**: Sysctls that need their own capability checks rely on the write path's checks, and a node classified by path prefix (`sys/net/`) must stay accurate as nodes are added.
- **Source**: 2aca260

### D30: OOM scores, swaps and locks are shown as Linux shows them
- **What**: Process and thread directories have `oom_score` and a writable `oom_score_adj`, `/proc/swaps` is the host's file (or Linux's header alone), `/proc/locks` lists the lock tables in the instance's pid namespace, and fdinfo appends `lock:` lines after `ino:` when the open file or its fd table has taken locks.
- **Why**: earlyoom, swapon and lslocks found no `oom_score`, `/proc/swaps` or `/proc/locks`, lslocks 2.40 needs fdinfo's lock lines to attribute OFD locks, and thread directories lacked the oom files.
- **Tradeoffs**: `/proc/swaps` lists host swap areas the guest cannot use, as meminfo's swap lines and a container's view do.
- **Watch out**: Tools that refuse to run with swap on (kubelet) see the host's swap wherever the host lets the app read `/proc/swaps`.
- **Source**: 39ea702

### D31: Names missing from the fixed proc, sys and dev trees are counted as unimplemented paths
- **What**: `View::missing` counts, through `missing_path`, a name missing from the procfs root (not numeric names), the fixed `/proc/sys` and `/proc/sysvipc` trees, an existing process's or thread's directory with its net/ and ns/, the fixed sysfs tree, a CPU's, interface's or loop device's directory, or the root of the boot `/dev` tmpfs, with pids, tids, CPUs, interfaces and loop devices turned into placeholders (kernel/signal D24).
- **Why**: Missing sysctls and proc files were among the largest causes of failures in the Starnix and gVisor runs, and Starnix's not_found counts names missing under /proc, /sys and /dev.
- **Tradeoffs**: cgroup files are not counted, since the root and other cgroups have different files, and probes for files Linux lacks as well are counted.
- **Watch out**: A dynamic directory added to a view must be excluded in `missing_path`, or each of its absent entries becomes a counted path.
- **Source**: 99b4216

### D32: `/proc/cpuinfo` names HWCAP2 bits up to Linux 6.1's last
- **What**: The HWCAP2 feature names go on past bf16 to 6.1's dgh, rng, bti, mte and ecv.
- **Why**: The names stopped at bf16, so an advertised ECV would have had no name.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 16364c0

### D33: FUSE mounts show their type and options
- **What**: mountinfo and mounts show `fuse.<subtype>` with the instance's user_id, group_id, default_permissions, allow_other and max_read, and `/proc/filesystems` lists `nodev fuse` (kernel/fuse D1).
- **Why**: Mount tools and fusermount3 read the FUSE type and options from mountinfo.
- **Tradeoffs**: fuseblk and fusectl are not listed.
- **Watch out**: None known.
- **Source**: c008749

### D34: stat's layout fields follow `do_task_stat`'s ptrace check
- **What**: `/proc/<pid>/stat`'s code, stack, data, argument and environment addresses come from the address space for readers passing `View::may_read` with filesystem credentials, other readers get 1, 1, 0 and zeros, and a process without an address space shows zeros.
- **Why**: The code, stack and data fields read 0 even for the process itself, while the argument and environment addresses went to every reader.
- **Tradeoffs**: kstkesp, kstkeip and the exit code stay 0.
- **Watch out**: None known.
- **Source**: 6d9a9e8

### D35: mountinfo shows propagation fields
- **What**: `MountLine` prints shared:N, master:N and unbindable after the mount options, as `show_mountinfo` does, from the propagation kernel/vfs D6 records.
- **Why**: mountinfo's optional fields were always empty.
- **Tradeoffs**: propagate_from never appears.
- **Watch out**: None known.
- **Source**: 970cf6a

### D36: An overlay program's `/proc/<pid>/exe` leads to its overlay node
- **What**: readlink appends " (deleted)" (`with_deleted`, shared with `mark_deleted`) once the recorded overlay node is no longer linked, and following the link leads to the `Node::Overlay` while it is linked, else to `OpenExe` when a host file was recorded, else `ENOENT`, as Linux's `proc_exe_link` does.
- **Why**: An overlay program's exe showed no " (deleted)" and was followed by its path, so a renamed program could not be reached and a removed one could not be run again.
- **Tradeoffs**: Opening a removed overlay program through exe is `ENOENT`, since reopening needs a store node, and the path shown after a rename is still the one recorded at execve, for store programs too.
- **Watch out**: Code that treats `OpenExe` as always backed by `ExeFile::node` must also handle the overlay case, and a collected overlay instance makes its programs' links read as deleted.
- **Source**: 9590431

### D37: cgroup directories keep mkdir's mode and check search
- **What**: `Cgroup::mode` comes from mkdir's mode with the umask (permission and sticky bits), stat reports it, and `View::searched` gives the walker and `searched_attrs` its attributes for search checks on lookups, chdir and chroot (kernel/vfs D50), as kernfs does.
- **Why**: cgroup directories were always 0755 whatever mkdir asked, so a private 0700 cgroup was listed and searched by everyone.
- **Tradeoffs**: cgroup directories still cannot be chmod'ed or chown'ed, so the mode is fixed at mkdir.
- **Watch out**: A runtime that makes cgroups under a restrictive umask and then enters them as another user now gets `EACCES`, as on Linux.
- **Source**: f5c09f7

### D38: Mounts, mountinfo and four cgroup2 files report changes to poll and inotify
- **What**: Open files carry an fd::ProcEvent counter and read snapshot, pseudo::pollable admits the supported procfs, sysfs and cgroup2 files, Vfs::mount_events counts mount changes, and cgroup::notify_changes compares dirty observations of cgroup.events, memory.events, memory.events.local and pids.events, updating poll counters before IN_MODIFY and regenerating stale contents at the next read from offset zero (kernel/vfs D51, kernel/cgroup D2–D3).
- **Why**: These files could not be epoll'ed and never reported a change, so mount monitors and cgroup event watchers (OOM reporting, freezer state) never woke.
- **Tradeoffs**: One mount counter serves every namespace, so any mount or propagation-type change marks every mountinfo reader; cgroup changes are seen at the next syscall, thread exit, freezer stop or timer pass, so a change undone in between is missed; other synthetic files still report no inotify events.
- **Watch out**: `notify_changes` regenerates the observed files after every BKL syscall while anything observes them, a process scan for cgroup.events, and a new place that changes a cgroup outside those points delays its notification until the next one.
- **Source**: affb1e2; 0f5c5132, b65799c8

### D39: Per-thread entries show the thread's credentials, and the reader is the reading thread
- **What**: status ids and the owners of per-process entries come from the thread under `task/<tid>` or the main thread (`Kernel::cred_of`, kernel/cred D1), the reader's credentials are the reading thread's, `may_read`'s access checks use the directory pid's (the main thread's) credentials except `syscall`'s (D40), and socket owners in `/proc/net` use the holding thread's fsuid.
- **Why**: `/proc/<pid>/task/<tid>` showed and was owned by the process's credentials, and the reader's credentials were the process's, which per-thread credentials made wrong.
- **Tradeoffs**: None known.
- **Watch out**: With per-thread credentials a thread's owners and the main thread's access checks can disagree, recorded as a bug in the map.
- **Source**: 1be1142; f1212d8 (access checks corrected)

### D40: `/proc/<pid>/syscall` follows `proc_pid_syscall`, generated at its first read
- **What**: `Entry::Syscall` (mode 0400) in the process and thread directories generates nothing at open and builds its content at the first read, so `/proc/self/syscall` shows the read itself, printing `running` for a live thread in state R other than the reader, the number with x0 to x5, sp and pc for one in the kernel (kernel/process D55), a negative number with sp and pc outside a syscall, `-1 0x0 0x0` for an exited main thread and `ESRCH` once another thread has gone, after a ptrace access check of that thread with the reader's own credentials (`may_read_as`, `EPERM`), as lock_trace does.
- **Why**: The hang dump (harness D39) needs Linux's file, which Tuxport lacked.
- **Tradeoffs**: With no Yama, any reader passing the ptrace check reads another process's file where Linux's ptrace_scope 1 limits it to ancestors, and the record is a snapshot whose registers can be a moment old.
- **Watch out**: Only `syscall` checks the target thread itself; the other per-thread checks use the main thread's credentials (D39).
- **Source**: f1212d8

### D41: Cgroup tokens are byte-safe and cpu.max retains an omitted period

- **What**: `subtree_control` rejects anything except a one-byte + or - prefix, and quota-only cpu.max writes retain the stored period or default to 100000.
- **Why**: Multibyte tokens could panic the kernel, and quota-only updates lost the period.
- **Tradeoffs**: Extra words and nonnumeric periods still fail where Linux’s sscanf can ignore them.
- **Watch out**: Native comparison requires the container to expose and enable the CPU controller.
- **Source**: 45be9fae; 20137e4c; d053be24; 39e55b4b

## Superseded

- ~~Regenerated content uses the current reader's credentials, not the opener's (D1 in part, §8.2)~~ → replaced by **D25** in f6a82cc (2026-11-08)
- ~~No common-ancestor permission check on cgroup migration (D4 in part, round 163)~~ → replaced by **D26** in b43ff37 (2026-11-08)
- ~~Recorded sysctls change nothing, and debug.exception-trace starts at x86's 1 (D16 in part, 42d2ffe)~~ → replaced by **D16** in 7b7f73b (2026-11-08); drop_caches acts since 3957a03 (2026-11-09, no SUPERSEDES declared)
- ~~Synthetic files that change do not notify pollers, mountinfo and `cgroup.events` included (D1's and D4's tradeoffs, §8.2, rounds 58, 163)~~ → narrowed by **D38** in affb1e2 (2026-11-09, no SUPERSEDES declared): mounts, mountinfo and four cgroup2 files report
- ~~`memory.current` counts shared pages once per process (D5's tradeoff, round 164)~~ → replaced by **D5** in 1d3d559 (2026-11-10, no SUPERSEDES declared)
- ~~`may_read`'s access checks go by the target thread (D39 in part, 1be1142)~~ → corrected in f1212d8 (2026-11-11): they use the main thread's credentials (a bug in the map), and only **D40**'s `syscall` checks the thread
- ~~D9 combined ownership of procfs, cgroup policy and device BPF~~ → replaced by **D9** in cf029f9a (2026-11-14)
- ~~Comparing every observed cgroup file at every checkpoint under D38~~ → replaced by **kernel/cgroup D2** in 0f5c5132 (2026-11-17), preserving poll and inotify delivery.
