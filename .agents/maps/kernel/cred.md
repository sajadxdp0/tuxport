# kernel/cred Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/cred.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 4 research concerns; 15 claims checked by 2 independent verifiers — 11 confirmed and 4 partial; bugs and debts recorded)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: repoint dependencies to the split module maps)

## Responsibilities

`kernel/cred` implements Linux credentials for guest processes:

- the credential structure (four uids, four gids, supplementary groups, five capability sets,
  securebits, user namespace) and the id syscalls (`set*uid`, `set*gid`, `setfs*`, groups);
- capability checks in their different scopes, `capget`/`capset` and the capability prctls;
- the execve credential transition (setuid/setgid bits, file capabilities, ambient sets,
  no_new_privs and tracer suppression, `AT_SECURE`);
- access-decision helpers: DAC `permission`, ownership, sticky deletion, new-file ownership,
  faccessat credentials, signal permission, ptrace access;
- guest seccomp: strict mode, cBPF filters, actions, inheritance, TSYNC, user notifications and the
  always-allow cache.

Root: `core/kernel/src/cred.rs` (except user namespaces), `syscall/cred.rs`, `seccomp.rs`.

Outside the module: user namespace creation, id maps and namespace entry (`kernel/ns`); the syscalls
that call the access helpers and their error ordering, xattr storage of `security.capability`
(`kernel/vfs`, `kernel/files`, `kernel/xattr`); signal delivery (`kernel/signal`) and ptrace stops (`kernel/ptrace`);
procfs views of credentials (`kernel/procfs`); the cgroup device eBPF (`kernel/cgroup`).

## Key types

- `Cred` — `ruid`/`euid`/`suid`/`fsuid`, the four gids, sorted `groups`, `cap_effective`,
  `cap_permitted`, `cap_inheritable`, `cap_bounding`, `cap_ambient` (u64), `securebits`,
  `user_ns`. All ids are kernel (initial-namespace) ids. Kept per thread (`Task.cred`), as on
  Linux; the thread group keeps a copy of its main thread's (`ThreadGroup.cred`), which is what
  other processes go by and outlives the main thread. `dumpable` and `no_new_privs` stay per
  thread group; umask lives in the fs context, not here.
- `cap` constants (`LAST` = 40, `CAP_FULL`), `secbit` (NOROOT, NO_SETUID_FIXUP, KEEP_CAPS,
  NO_CAP_AMBIENT_RAISE and their locks), `MAY_EXEC`/`MAY_WRITE`/`MAY_READ`.
- `FileCaps` — parsed `security.capability` (permitted, inheritable, effective).
- `Seccomp` — per-thread `mode` (disabled, strict, filter, dead) and `chain`; `Filter` (checked
  program, converted length, `allow` bitmap, original bytes, log and wait-killable flags, `prev`,
  user count, optional `Notif`); `Chain` (a thread's handle on its newest filter); `Notif`/
  `Knotif` (notification queue; states Init, Sent, Replied); `Listener` (the notify fd's object);
  `Verdict` (`Run(nr)`, `Skip`, `Exit`). `Task::seccomp_on` and `seccomp_allow` are lock-free copies
  for the fast path.

## Public entry points

- Id syscalls: `getuid`…`getresgid`, `setuid`, `setreuid`, `setresuid`, `setfsuid` and the gid
  forms, `getgroups`, `setgroups`; `capget`, `capset`; prctl `PR_*KEEPCAPS`, `PR_*SECUREBITS`,
  `PR_CAPBSET_READ/DROP`, `PR_CAP_AMBIENT`, `PR_*NO_NEW_PRIVS`.
- Checks: `Cred::capable` (initial namespace), `capable_here` (own namespace), `ns_capable`,
  `capable_wrt` (with respect to an inode's owner), `permission`, `owns`, `may_delete`,
  `new_owner`, `for_access`, `searches_all`, `in_group`, `may_signal`, `may_ptrace`; id
  translation `uid_in`/`uid_out`/`gid_in`/`gid_out`.
- Exec: `Cred::exec` with `FileCaps` from the program file.
- Seccomp: `seccomp(2)` (277), prctl `PR_SET/GET_SECCOMP`, `seccomp::check` (syscall entry),
  `always_allowed` (fast path), the notify-fd ioctls, `get_filter`/`get_metadata` (ptrace),
  `actions_avail`/`actions_logged` sysctls.

## Data flow / lifecycle

### Credentials and id syscalls

- The first process gets root credentials (all ids 0, permitted/effective/bounding full) in the
  initial user namespace. Readers take a cheap by-value snapshot (`Cx::cred`, the calling
  thread's); clone gives a new thread or process the creating thread's credentials (moved into a
  new user namespace with `CLONE_NEWUSER`). Every change goes through `Cx::set_cred`, which also
  updates `ThreadGroup.cred` when the caller is the main thread (tid equal to tgid) and marks the
  no-BKL path's snapshot stale; execve sets both, the caller being the main thread by then.
  libc's set*id functions run the syscall on every thread, so they change all threads; raw
  syscalls and `setfsuid`/`setfsgid` change only the caller.
- Whose credentials a check uses: the caller's own thread for its permission checks, new-file
  owners, `SO_PEERCRED`/`SCM_CREDENTIALS`, procfs's reader, the no-BKL resolver and the core file
  name; a target
  thread's own (`Kernel::cred_of`: the live thread, else the process's copy) for tgkill and
  F_SETOWN to a thread, ptrace access (attach, process_vm, kcmp, procfs views and memory files
  under `task/<tid>`), capget, prlimit, get_robust_list, the sched and priority calls and
  `PRIO_USER`/`IOPRIO_WHO_USER` matching, `RLIMIT_NPROC` counting, and procfs status and owners
  (under `task/<tid>` the thread's, under `/proc/<pid>` the main thread's); the process's copy (its
  main thread's) for kill and signals to a process, wait's `si_uid`, OOM reports and the core
  file's process info.
- Set syscalls map guest ids through the caller's user namespace (`-1` means unchanged; unmapped
  `EINVAL`), apply Linux 6.1's rules with `capable_here` (`CAP_SETUID`/`CAP_SETGID` in the caller's
  own namespace), and commit through `update`, which — like `commit_creds` — clears the process's
  dumpable flag and the calling thread's pdeathsig when effective or filesystem ids change or
  permitted grows. The capability prctls commit without these side effects, as Linux's do not
  trigger them. `unshare(CLONE_NEWUSER)` and setns into a user namespace (both single-threaded
  only) replace the credential without them either (correct for unshare, as in Linux).
- Capability fixups on uid changes (`cap_emulate_setxuid`, root meaning uid 0 of the credential's
  namespace): leaving root on all three uids clears permitted and effective unless `KEEP_CAPS`
  and always clears ambient; euid root → non-root clears effective, non-root → root sets
  effective = permitted; `setfsuid` drops or raises the filesystem capabilities;
  `SECBIT_NO_SETUID_FIXUP` disables all of it.
- `setfsuid`/`setfsgid` never fail: bad values are ignored and the old value is returned.
  `getgroups` (negative size `EINVAL`, 0 returns the count, too small `EINVAL`); `setgroups` (size
  0..65536, mapped ids, then `CAP_SETGID` and the namespace's setgroups permission with a written
  gid map, else `EPERM`; stored sorted).

### Capabilities

- Scopes: `capable` only in the initial user namespace; `capable_here` in the caller's own;
  `ns_capable` in a given namespace (including the parent-owner rule); `capable_wrt` additionally
  requires the inode's uid and gid to be mapped in the caller's namespace.
- `capset`: v1 (one word) or v2/v3 (two); unknown versions write v3 back and fail `EINVAL`; null
  data is a probe; another pid `EPERM`; new permitted ⊆ old permitted, effective ⊆ new permitted,
  inheritable ⊆ old inheritable ∪ (`CAP_SETPCAP` ? all : old permitted) and ⊆ old inheritable ∪
  bounding; ambient is then masked to permitted ∩ inheritable.
- prctl: bounding drop needs `CAP_SETPCAP`; ambient raise needs the capability in permitted and
  inheritable and no `NO_CAP_AMBIENT_RAISE`; securebits need `CAP_SETPCAP`, cannot change locked
  bits or unlock; keepcaps fails when locked.

### execve

- Inputs: the final interpreter's stat; setuid/setgid apply only with the mode bit, no
  no_new_privs, a mount without nosuid, owner and group mapped in the caller's namespace (setgid
  also needs group exec); file caps are read unless the mount is nosuid (parse errors fail exec with
  `EINVAL`) and apply only when their root (kernel uid 0 for v1/v2, the rootid for v3) is the root
  of the caller's user namespace or an ancestor's (`rootid_owns_currentns`). The execution is
  unsafe (Linux's `bprm->unsafe`) when traced by a tracer whose credentials when tracing began
  (`ptracer_cred`) lack `CAP_SYS_PTRACE` over the caller's user namespace (`ptracer_capable`),
  or when a task of another thread group shares the caller's fs context (clone with `CLONE_FS`
  but not `CLONE_THREAD`; `LSM_UNSAFE_SHARE`), found by scanning the tasks when the context has
  more references than one thread's two.
- `Cred::exec` (Linux 6.1 `bprm_fill_uid` + `cap_bprm_creds_from_file`): euid/egid from the file;
  permitted = (bounding ∩ file permitted) | (inheritable ∩ file inheritable), `EPERM` when the file's
  effective bit is set and not all file permitted capabilities were granted; root emulation unless
  `NOROOT` (and not for a non-root running a setuid-root binary that has file caps): a root euid or
  ruid gives permitted = bounding | inheritable, a root euid sets the effective bit; gains (setid or
  more permitted) under no_new_privs or an unsafe execution limit permitted to the old set and
  reset euid/egid (unsafe ones only without `CAP_SETUID`); saved and filesystem ids follow the effective ones; ambient is cleared for setid
  or file-caps programs, then permitted |= ambient and effective = file effective ? permitted :
  ambient; `KEEP_CAPS` is cleared; secureexec = setid or (non-root real uid and (file effective or
  permitted ⊄ ambient)).
- After the transition: secureexec sets `AT_SECURE` and caps the stack rlimit at 8 MB; dumpable is
  cleared when ids differ or permitted grew, else set from whether the caller can read the program
  file and its interpreter; pdeathsig is cleared in those cases. The address space's user namespace
  (`ThreadGroup::mm_user_ns`, Linux's `mm->user_ns`, kept by fork) is the caller's, raised for a
  file it cannot read to the nearest ancestor where that file's owner and group are mapped
  (`UserNs::holding`, `would_dump`). A setuid or setgid program, or one whose file capabilities
  would grant more than the caller had (judged before no_new_privs or an unsafe execution takes
  the gain away), clears the personality flags in `PER_CLEAR_ON_SETID` (Linux's
  `bprm->per_clear`).
- Writes, truncation and chown remove file caps (and set-id bits) from the inode (in the store).

### Access decisions

- `permission(stat, mask)`: owner class if fsuid matches, group if fsgid or a supplementary group
  matches, else other; if the class allows everything requested, allow; otherwise directories
  allow with `CAP_DAC_READ_SEARCH` (no write) or `CAP_DAC_OVERRIDE`, non-directories allow with
  `CAP_DAC_READ_SEARCH` (read only) or `CAP_DAC_OVERRIDE` (no exec, or some exec bit set), all via
  `capable_wrt`; else `EACCES`. No ACLs, immutable or append flags.
- `owns`: fsuid is the owner, or `CAP_FOWNER` in the caller's namespace with the owner mapped there
  (Linux's `inode_owner_or_capable`; the group need not be mapped). `may_delete`: write and search
  on the directory, then for sticky directories the owner of file or directory or `CAP_FOWNER`
  (`EPERM`). `new_owner` (Linux 6.1's `vfs_prepare_mode` and `inode_init_owner`) takes the mode
  and the umask: uid fsuid; gid the directory's when it is setgid, else fsgid; in a setgid
  directory a non-member without `CAP_FSETID` loses `S_ISGID` on a group-executable file before
  the umask applies; a directory keeps only its permissions and the sticky bit and inherits the
  setgid bit of a setgid parent. `for_access`: filesystem ids set to the real ids, effective capabilities
  to permitted only when the real uid is root in the namespace. `searches_all`: `CAP_DAC_READ_SEARCH`
  or `CAP_DAC_OVERRIDE` in the initial namespace (gates the no-BKL resolver).
- `may_signal` (Linux `kill_ok_by_cred`): sender's real or effective uid equals the target's real
  or saved uid, or `CAP_KILL` over the target's namespace; callers add same group and SIGCONT
  within the session.
- `may_ptrace(target, (dumpable, mm namespace), fscreds)` (Linux's `__ptrace_may_access` and
  `cap_ptrace_access_check`): all three target uids and gids equal the caller's filesystem or real
  ids, or `CAP_SYS_PTRACE` over the target's namespace; and the target is dumpable or the caller
  has `CAP_SYS_PTRACE` over its address space's namespace; and the same namespace with the target's
  permitted ⊆ the caller's effective set with filesystem credentials (its permitted set with real
  ones), or `CAP_SYS_PTRACE` over the target's namespace; procfs links and the opens of
  `/proc/<pid>/mem`, `auxv`, `environ`, `maps`, `smaps` and `pagemap` use filesystem credentials,
  ptrace attach, process_vm, pidfd_getfd, kcmp and setns through a pidfd real ones.

### seccomp

- Installing (`seccomp(2)`/prctl, Linux order): flag combinations `EINVAL`; length 0 or over 4096
  `EINVAL`; without no_new_privs (read from the thread group) or `CAP_SYS_ADMIN` in the caller's
  namespace `EACCES`; the classic and seccomp checkers (aligned loads of `seccomp_data`, forward
  jumps, packet loads and MOD rejected); mode must be disabled or filter; a second listener
  `EBUSY`; total converted length plus 4 per filter over 32768 `ENOMEM`; TSYNC requires every other
  thread to be disabled or on an ancestor chain (else that thread's tid in the caller's pid
  namespace, or `ESRCH` with `TSYNC_ESRCH` or when the caller cannot see it). The always-allow bitmap is built (ANDed with the previous filter's) and the filter
  pushed; TSYNC copies the chain to other threads; the listener fd is close-on-exec.
- Filters belong to threads, are copied by clone/fork and kept across execve; an exiting leader's
  chain is held until the group is reaped.
- At syscall entry (after a `PTRACE_SYSCALL` entry stop): disabled, `PTRACE_O_SUSPEND_SECCOMP` and
  the vDSO clock stubs (an svc from a stub at a vDSO clock entry, for gettimeofday and the clocks
  Linux's vDSO reads in user space; the host's vDSO code makes only real syscalls,
  `kernel/vdso`) skip; strict mode allows read, write, exit and rt_sigreturn and kills otherwise;
  in filter mode a number in the top filter's bitmap runs at once, otherwise every filter runs
  (newest first) on `seccomp_data` (nr, `AUDIT_ARCH_AARCH64`, pc, original x0 and x1..x5) and the
  most severe action wins (ties: the newer filter). Actions: ERRNO (capped at 4095), TRAP (`SIGSYS`
  with syscall/arch/address), TRACE (`PTRACE_EVENT_SECCOMP` stop and re-check, or `ENOSYS` without
  a tracer), USER_NOTIF, LOG, ALLOW, KILL_THREAD (or KILL_PROCESS when it is the last live
  thread), KILL_PROCESS and unknown actions (fatal `SIGSYS` with core dump). Logged actions go to
  Tuxport's debug log only (stderr with `TUXPORT_LOG`); USER_NOTIF is never logged.
- User notifications: the waiting thread is woken by replies, ends on fatal signals, and returns
  `ERESTARTSYS` on other signals (unless `WAIT_KILLABLE_RECV` and already received, where a signal
  costs one spurious wakeup before blocking again). RECV (zeroed struct, blocks even when
  nonblocking, `ENOENT` for withdrawn; the `pid` field is the notified thread as the receiver's pid
  namespace sees it, 0 when invisible), SEND (flags, `CONTINUE`, `ENOENT`, `EINPROGRESS` for a
  second answer), ID_VALID, ADDFD (installs into the target's table, optionally replacing an fd,
  closed as the target's, and answering); closing the listener answers waiters with `ENOSYS`; poll
  readiness and `POLLHUP` when no thread uses the filter.
- The cache: for numbers 0..511, a filter whose evaluation with only nr/arch known reaches
  `RET ALLOW` marks the number always allowed; such calls skip the interpreter and keep the no-BKL
  fast path.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/vfs` and `kernel/files` (every permission and ownership check, file-cap removal),
  `kernel/xattr` (xattr capability rules), `kernel/process` (fork/clone copies, setpriority/ioprio
  `PRIO_USER`, `RLIMIT_NPROC`), `kernel/exec` (execve), `kernel/ns` (namespaces), `kernel/signal`
  (kill), `kernel/ptrace` (ptrace access), `kernel/procfs` (status lines, ownership, sysctls), `kernel/unix`
  (credentials), `kernel/net`/`kernel/netns`/`kernel/nft` (capability checks), the syscall dispatcher and fast
  path (seccomp).

### Outbound

- User namespaces and id maps (`kernel/ns`); `Stat` and modes (`kernel/store`); ptrace stops (`kernel/ptrace`),
  `signal::force`/`force_fatal` (`kernel/signal`); the vDSO stub check (`kernel/vdso`); fd tables; the kernel's
  waits and wake objects; `tuxport_hal::fill_random` (notification ids); the debug log.

## Invariants and gotchas

- Stored ids are kernel ids; translation happens only at syscall and procfs boundaries (unmapped
  ids show as 65534).
- Capability checks must use the right scope: `capable` is false inside any child user namespace
  even with a full effective set.
- `Seccomp` is touched only under the BKL; the fast path reads the published atomics (a stale
  bitmap during TSYNC belongs to the older, looser chain).
- `in_group` relies on `setgroups` keeping the list sorted.
- Credentials change only through `Cx::set_cred` (or execve, which sets both copies): writing
  `Task.cred` alone leaves the process's copy stale when the main thread changes, and writing
  `ThreadGroup.cred` alone changes nothing the caller's own checks see.

## Confirmed bugs / technical debt

- **seccomp logging is incomplete.** Logged actions reach only Tuxport's debug stderr (not the guest
  kernel log, where Linux's audit records appear without auditd), and USER_NOTIF is never logged
  although `SECCOMP_FILTER_FLAG_LOG` asks for it.
- **`for_access` ignores `SECBIT_NO_SETUID_FIXUP`**, so access(2) answers differ for
  capability-only sandboxes.
- **Errno order differences.** setgroups reports `EINVAL`/`EFAULT` before the permission check's
  `EPERM`; capset does not mask bits above `CAP_LAST_CAP` (`EPERM` instead of silently ignoring
  them); `PR_CAPBSET_DROP` beyond 40 is `EINVAL` before `EPERM`.
- **setns into a foreign user namespace keeps dumpable.** Entering a namespace not owned by the
  caller's euid skips Linux's dumpable and pdeathsig reset (until the next execve).

## Open questions

- `no_new_privs` is kept per thread group, while Linux keeps it per thread (prctl sets only the
  caller's; seccomp TSYNC spreads it): does anything set it on one thread only and rely on the
  others staying free of it?

## To verify

- The Linux 6.1 comparisons above (`access_override_creds`, `cap_ptrace_access_check`,
  `inode_owner_or_capable`, `vfs_prepare_mode`, `ptracer_capable`) come from the
  verifiers' knowledge of the kernel source, not from a side-by-side reading or a differential test.
