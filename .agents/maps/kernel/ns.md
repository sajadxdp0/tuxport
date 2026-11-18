# kernel/ns Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/ns.md` for the paired decision history (decisions from before the split are D12, D13, D28, D40 and the id-map half of D32 in `.agents/decisions/kernel/process.md`).
>
> Verified: 2026-11-11 (split out of the kernel/process full mapping of 2026-11-10; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-14 (targeted verification only since the full mapping; latest change: cgroup and device BPF references follow the split into kernel/cgroup)

## Responsibilities

`kernel/ns` is the namespace mechanism (namespaces(7); 6.1's `kernel/nsproxy.c`,
`pid_namespace.c`, `user_namespace.c`, `time/namespace.c` and `ucount.c`):

- the eight kinds and where each is held;
- creating namespaces at clone and unshare, and unshare's other copies (fd table, fs, semaphore
  exit);
- entering them with setns through a namespace file or a pidfd, and the namespace-file ioctls;
- user namespaces: ID maps and their write rules, translating IDs shown outward, and the
  capability checks against namespace owners;
- pid namespaces: global↔local tables, translation at the boundaries, nesting and the dead state;
- time namespaces: offsets, and the vDSO stubs that bring clock reads to the kernel layer;
- UTS and cgroup namespaces; namespace counts and the `max_*_namespaces` limits;
- namespace files (`/proc/<pid>/ns/*`, nsfs) that keep their namespace alive.

Root: `core/kernel/src/nsproxy.rs`, `pidns.rs`, `syscall/ns.rs`, the unshare section of
`syscall/proc.rs`, the time-namespace offsets at the end of `syscall/misc.rs` (`expiry_in_ns`,
`to_host_clock`, `add_offset`), and `UserNs` in `cred.rs`.

Outside the module: what each namespace holds — mount trees (`kernel/mount`), network devices,
addresses and ports (`kernel/netns`), IPC objects (`kernel/ipc`), the cgroup tree
(`kernel/cgroup`); clone itself, pid allocation and a pid-namespace init's exit
(`kernel/process`); credentials and capability sets (`kernel/cred`); the vDSO stubs' mapping
(`kernel/mm`); the procfs files' structure (`kernel/procfs`).

## Key types

- `NsProxy` (per task: UTS, IPC, net, cgroup, time, time-for-children), `UtsNs`, `IpcNs`,
  `CgroupNs`, `TimeNs` (offsets, frozen), `UserNs` (parent, level, owner, uid/gid maps, setgroups,
  whether the creator had `CAP_SETFCAP`; held in credentials), `PidNs` (parent, level, owner,
  global↔local tables, reaper, dead; held per thread group), `MntNs` (held in `FsContext`), `NsRef`
  (a strong reference to any namespace), `NsSet`; `Counted` (the eight counted kinds and their
  sysctls) and `Ucounts` (`Kernel::ucounts`: the live namespaces by kind with the (user namespace,
  user) pairs each counts for, the counts, and each user namespace's `max_*_namespaces`).

## Public entry points

- **Syscalls**: `unshare`, `setns`, namespace ioctls (`NS_GET_*`); clone's `CLONE_NEW*` flags go
  through `new_user_ns`, `new_pid_ns`, `new_mnt_ns`, `new_cgroup_ns`, `check_counts` and `count`.
- **Kernel-internal**: `syscall::ns::find` (a namespace by kind and id, for nsfs),
  `PidNs::register`/`unregister`/`vnr`/`find`/`sees`/`set_last_pid`, `TimeNs::offset`,
  `Cred::enter_user_ns`.
- **procfs**: `ns_last_pid`, `uid_map`/`gid_map`/`setgroups`, `timens_offsets`,
  `max_*_namespaces`, `/proc/<pid>/ns/*`.

## Data flow / lifecycle

### Kinds and creation

- All eight types exist. UTS, IPC, net, cgroup and time live in the task's `NsProxy`; the mount
  namespace lives in `FsContext` (tasks sharing a fs share it); the pid namespace belongs to the
  thread group, with a per-task namespace for children; the user namespace lives in the credentials.
- clone: a new user namespace is created first (level, count, chroot and mapped-ID checks) and the
  other `CLONE_NEW*` flags then need `CAP_SYS_ADMIN` in it and pass the namespace counts; then the
  pid namespace, the pid, the mount namespace on the copied fs, UTS, IPC, net and cgroup; a new
  time namespace becomes time-for-children, which a non-`CLONE_VM` child enters.
- unshare: `CLONE_NEWUSER` implies a new thread and fs scope, `CLONE_VM` implies signal handlers,
  which imply a thread, `CLONE_NEWNS` implies fs; asking for (or implying) a thread, signal
  handlers or memory is `EINVAL` for a multithreaded caller or one sharing them. It creates and
  charges a new user namespace, checks `CAP_SYS_ADMIN` and the counts, creates a pid namespace,
  then applies: fd table and fs copies (`EMFILE` above `nr_open`), credentials, mount namespace,
  pid-for-children, the semaphore exit for `CLONE_SYSVSEM`/`CLONE_NEWIPC`, and a new `NsProxy`
  (time, cgroup, net, UTS, IPC); a new mount namespace copies the whole mount tree
  (`kernel/mount`), a new cgroup namespace is rooted at the creator's cgroup.
- Pid namespaces nest to level 32 and user namespaces to 33 (`ENOSPC` beyond); a new pid
  namespace's owner must descend from its parent's owner (`EPERM`). Time namespaces follow the
  Linux 6.2 rule: a new one becomes time-for-children; non-`CLONE_VM` children enter it at fork;
  threads and vfork children enter at execve. Entering one with offsets, at fork, execve or setns,
  puts svc stubs over the host vDSO's clock entries in the address space (`Mm::vdso_syscalls`), so
  clock reads come to the kernel layer, which adds the offsets.

### Namespace limits

- Namespace limits (Linux's ucounts): a namespace created by effective user u in user namespace n
  (for a new user namespace, n is its parent; open_tree's anonymous mount namespaces count too)
  counts for (n, u), then for (n's parent, n's owner) up to the initial user namespace, and every
  one of those counts must stay below its user namespace's `max_<kind>_namespaces` (half of
  threads-max in the initial one, as threads-max is now where Linux fixes it at boot; `INT_MAX`
  elsewhere), else `ENOSPC`: for a user namespace after the level check and before the chroot and
  mapping checks, for the others after the `CAP_SYS_ADMIN` check and before any is created. A
  namespace counts while an `Rc` to it lives; those gone are taken off at the next check (a weak
  reference per namespace), and a user namespace's limits go with it.

### setns

- setns accepts a namespace file (type 0 or its own) or a pidfd (any non-empty combination of
  types; the process's main thread must be alive and the caller allowed to ptrace it); every part
  is validated in Linux order (`validate_nsset`: user, mount, UTS, IPC, pid, cgroup, net, time)
  before anything switches, then all switch together (a new mount namespace moves root and cwd to
  its root).

### User namespaces

- Credentials hold kernel IDs; IDs shown outward (`get*id`, stat, procfs,
  `SO_PEERCRED`, `si_uid`) are mapped through the viewer's maps, unmapped as 65534. Each map can be
  written once, and the checks use the credentials the file was opened with (as Linux's
  `file_ns_capable`), though the target namespace is looked up through the process when written
  (`ESRCH` once it is reaped; a process that moved meanwhile gets its new namespace written): the
  opener must be in the namespace or its parent and have `CAP_SYS_ADMIN` in the
  namespace; mapping the parent's uid 0 needs `CAP_SETFCAP`, which the creator must have had if
  the opener is inside, or the opener must have in the parent; otherwise the opener, owning the
  namespace, may write only a single extent of its own euid (or egid after `setgroups` is set to
  deny), unless both opener and writer have `CAP_SETUID`/`CAP_SETGID` in the parent;
  `timens_offsets` checks the opener's `CAP_SYS_TIME`. Capability checks come in four forms
  (`capable` in the initial namespace, `capable_here`, `ns_capable` following Linux `cap_capable`,
  `capable_wrt`).

### Pid namespaces

- Kernel pids are global; each namespace keeps global↔local tables; syscall
  arguments are translated at dispatch (pids inside structures and CPU clock ids by their
  handlers) and results (and wait, siginfo) at exit; invisible results
  become 0. The first process numbered 1 is the reaper (`kernel/process`'s exit and reaping for its
  exit); registering into a dead namespace is `ENOMEM`; procfs instances number by the mounting
  process's namespace, while `ns_last_pid` and loadavg follow the reader.

### Time namespaces

- Monotonic and boottime offsets are added to clock reads (and `/proc/uptime`,
  sysinfo) and subtracted from absolute waits.

### Namespace files

- `/proc/<pid>/ns/*` link into nsfs; open files and bind mounts hold the namespace
  (`NsRef`), so `setns` works after its processes exit (`ip netns add`); mqueue and procfs mounts
  also hold their namespace.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/process`: clone creating namespaces; the dispatcher translating pid arguments and
  results; wait and the pid-namespace init's exit.
- `kernel/exec` (the time namespace for children at execve), `kernel/signal` (siginfo pids and
  uids), `kernel/procfs` (the files above), `kernel/mount` (anonymous namespaces' counts, nsfs
  binds), `kernel/unix` (`SO_PEERCRED`), `kernel/cred` and every capability check against a
  namespace owner.

### Outbound

- `kernel/mount` (`copy_ns`, `find_ns`, `pinned_ns`, `ns_root`), `kernel/netns` (new network
  namespaces), `kernel/ipc` (new IPC namespaces, `Ipc::exit`), `kernel/cgroup` (cgroup namespace
  roots), `kernel/cred` (credentials and capability sets), `kernel/mm` (`vdso_syscalls`),
  `kernel/process` (thread groups, `fast_changed`, ptrace access for a pidfd's setns).

## Invariants and gotchas

- Kernel pids and IDs are global inside the kernel; translation happens only at the syscall, procfs
  and signal boundaries. A new boundary that shows pids or IDs (a syscall result, a file, a message)
  must translate them or it leaks global numbers.

## Confirmed bugs / technical debt

- **`unshare` is not atomic:** a network-namespace allocation failure (`ENOMEM`, no free loopback
  block) leaves the earlier fd-table, fs, credential, mount, pid-for-children and semaphore changes
  in place. A pidfd `setns` combining `CLONE_NEWNS` with other types while the fs is shared
  switches the mount namespace of every fs sharer (Linux switches only the caller's).

## Open questions

- None recorded.

## To verify

- The Linux 6.1 comparisons above that rest on kernel behaviour rather than repository source
  (`new_idmap_permitted`, `create_pid_namespace`'s owner check) were checked from knowledge of the
  kernel, not against its source.
