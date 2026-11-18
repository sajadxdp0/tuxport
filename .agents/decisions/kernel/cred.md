# Credentials and seccomp (kernel/cred) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/cred"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b). D1–D6 were seeded by hand from the retired design doc
> §11.1 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: Each thread has its own credentials
- **What**: `Task.cred` is per thread and changes only through `Cx::set_cred`, which also updates `ThreadGroup.cred` when the main thread calls; the caller's own checks (DAC, `SO_PEERCRED`, `SCM_CREDENTIALS`, the no-BKL resolver, seccomp, the capability prctls, setns and unshare of user namespaces) use the calling thread's, `Kernel::cred_of` gives a target thread's own (else the process's copy) for signals, ptrace access, scheduling and priority, rlimits and `RLIMIT_NPROC` counting, and `commit_creds`' side effects clear only the calling thread's pdeathsig.
- **Why**: Credentials were kept per thread group, so raw per-thread id syscalls and `setfsuid` (which libc does not broadcast) changed every thread, unlike Linux.
- **Tradeoffs**: The main thread has two copies kept in step by one setter, and no_new_privs and dumpable stay per thread group (no_new_privs is per thread on Linux, an open question in the map), while seccomp filters are per thread as in Linux.
- **Watch out**: Code that writes `Task.cred` or `ThreadGroup.cred` directly leaves the copies apart, and libc's set*id broadcast now matters (musl and glibc kill or abort the process when one thread's call fails after another's succeeded).
- **Source**: round 08 (`08-m6`); 1be1142

### D2: Id changes, DAC and capability fixups follow Linux 6.1 and capabilities(7)
- **What**: The set*id syscalls, `capset`, the capability prctls and securebits use Linux's rules; uid changes adjust capabilities as `cap_emulate_setxuid` does; changing effective or filesystem ids, or gaining capabilities, clears the process's dumpable flag and the calling thread's pdeathsig as `commit_creds` does; DAC checks follow `generic_permission`, with root bypassing through capabilities.
- **Why**: M6 replaced the all-root model so that dropping to an ordinary user is enforced (a rootfs test checks DAC after `su` to `nobody`); the `commit_creds` side effects were added with user namespaces, having happened only at execve before.
- **Tradeoffs**: No ACLs, immutable or append-only attributes; `system.*` xattrs are stored but have no effect.
- **Watch out**: Securebits were recorded but ineffective until round 100; new paths must honour `NOROOT`, `NO_SETUID_FIXUP` and `NO_CAP_AMBIENT_RAISE`.
- **Source**: rounds 08 (`08-m6`), 100 (`100-file-caps`), 119 (`119-user-namespaces`); 1be1142

### D3: Capability checks name their namespace, as in Linux
- **What**: Three checks: `capable` (initial user namespace only), `capable_wrt` (own namespace, with the inode's owner and group mapped — DAC overrides, `CAP_FOWNER`, `CAP_CHOWN`, `CAP_FSETID`) and `ns_capable` (a given namespace — set*id, capset, mounts by the mount namespace's owner, ptrace and kill by the target's); "root" means uid 0 of the credential's own namespace.
- **Why**: A process in a new user namespace holds a full capability set; when user namespaces were added for bubblewrap, the 74 call sites that had checked the caller's own effective set were each switched to the scope Linux uses there.
- **Tradeoffs**: The no-BKL `stat` resolver serves only processes in the initial namespace, since an override elsewhere depends on the file.
- **Watch out**: A new check that uses the wrong scope either grants host-level privilege inside a namespace or denies what a container needs.
- **Source**: round 119 (`119-user-namespaces`)

### D4: execve transforms credentials like `cap_bprm_creds_from_file`, with file capabilities
- **What**: Setuid/setgid bits (owner and group mapped, no nosuid, no no_new_privs), file capabilities from `security.capability` (v1–v3; v3 only where its root owns the namespace, D12), ambient capabilities, root emulation and `AT_SECURE` for privileged execution; under no_new_privs, or in an unsafe execution (traced by a tracer without `CAP_SYS_PTRACE` over the tracee's namespace, kernel/signal D20, or sharing the fs context with another thread group), gains are capped at the old permitted set and set-id effective ids are kept only with `CAP_SETUID` (never under no_new_privs).
- **Why**: Ordinary users (Debian `su`, sshd logins, a non-root login account) need file capabilities such as iputils-ping's `cap_net_raw`; before round 100 the xattr was stored and ignored at exec.
- **Tradeoffs**: An execve whose fs context has more than one thread's references scans the task table for a sharer; no audit records.
- **Watch out**: File capabilities are only safe together with the xattr permission checks (`security.capability` needs `CAP_SETFCAP`, other `security.*` and `trusted.*` need `CAP_SYS_ADMIN`) and with privilege removal on write, truncate and chown; round 100 found all three missing and added them together.
- **Source**: rounds 08 (`08-m6`), 100 (`100-file-caps`), 119 (`119-user-namespaces`); f6a82cc, 2aca260, 6d9a9e8

### D5: Guest seccomp is implemented in the kernel layer with Linux 6.1 semantics
- **What**: Strict mode and cBPF filters with Linux's program checks, chain size limit, action precedence, inheritance and TSYNC; filters run in an interpreter at syscall entry (after a `PTRACE_SYSCALL` entry stop), skipped only by the vDSO stubs for gettimeofday and the clocks Linux's vDSO reads in user space, while the host vDSO's own syscalls are filtered like any other (kernel/signal D4); `PTRACE_EVENT_SECCOMP` and the notification fd (including `ADDFD`) are supported; `KILL_PROCESS`'s `SIGSYS` cannot be caught, as with `SA_IMMUTABLE`.
- **Why**: Programs that sandbox themselves either failed or ran without the sandbox; `strace --seccomp-bpf` and notification supervisors did not work.
- **Tradeoffs**: The interpreter costs about 4.5 ns per instruction (Linux uses a JIT); `LOG` and logged actions go to Tuxport's debug log instead of audit; a `TRACE` re-check uses the chain from before the stop; `ADDFD` installs directly and never sees the target interrupted.
- **Watch out**: Every syscall entry, including the no-BKL fast path, must consult the filter; a fast path that misses `seccomp_on` bypasses filters silently, which the first tests did not catch.
- **Source**: round 59 (`59-seccomp`); 16364c0

### D6: An always-allow cache keeps filtered threads on the fast path
- **What**: Installing a filter emulates it for syscall numbers 0..511 with only the number and architecture known (`seccomp_is_const_allow`); numbers that always reach `ALLOW` form a bitmap ANDed with the previous filter's; the thread publishes it atomically, and those numbers plus the vDSO clock stubs skip the interpreter and keep the no-BKL fast path.
- **Why**: Without it every syscall ran the whole chain under the BKL, so futex wakes and clock reads in multithreaded sandboxed programs queued on the lock; with it, filtered threads run at unfiltered speed for libseccomp-style filters.
- **Tradeoffs**: Only the program shapes Linux recognises are cached; other numbers still take the BKL and the interpreter.
- **Watch out**: During TSYNC another thread may read the old bitmap; this is acceptable only because a new chain is never looser, so the call counts as happening before the sync.
- **Source**: round 60 (`60-seccomp-cache`)

### D7: Credentials, capabilities and seccomp are a module of their own
- **What**: kernel/cred covers `cred.rs` except user namespaces, `syscall/cred.rs` and `seccomp.rs`, while user namespaces stay in kernel/ns, device BPF in kernel/cgroup and synthetic credential views in kernel/procfs.
- **Why**: §11.1's credential and seccomp design had to leave the retired design doc, and the user asked for a module of its own.
- **Tradeoffs**: User namespaces sit in kernel/ns although their code is in `cred.rs`, and the Linux 6.1 comparisons rest on the verifiers' knowledge of the kernel source rather than a side-by-side reading; tracing, vDSO construction and nftables dependencies belong to kernel/ptrace, kernel/vdso and kernel/nft.
- **Watch out**: Divergences recorded at the split or found since stay open in the map, among them incomplete seccomp logging, `for_access` ignoring `SECBIT_NO_SETUID_FIXUP`, errno-order differences in setgroups, capset and `PR_CAPBSET_DROP`, and setns into a foreign user namespace keeping dumpable.
- **Source**: 12c4afa (bug list since updated by 4e64e74, and shortened by D1, D4, D5, D9–D11, D13 and kernel/process D33); cf029f9a; d053be24; 60790c14; 39e55b4b

### D8: Credential and seccomp files read in the order their code runs
- **What**: cred.rs runs credentials and file capabilities with the user namespace types (owned by kernel/process) last and `impl Cred`'s entry points before id translation and scope checks; syscall/cred.rs runs id syscalls, capabilities, then committing; seccomp.rs goes from installing filters through syscall entry, the sysctls, user notifications, ptrace and thread exit to the always-allow cache, cBPF and shared helpers; constants several sections use stay at the top.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; cred.rs opened with the user namespace types and seccomp.rs mixed the cBPF interpreter into the filter paths.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; two seccomp sections (thread exit, shared helpers) hold one function each.
- **Watch out**: Code appended at the end of a file drifts from this order.
- **Source**: 159d4f4

### D9: New files take their mode and owner through one helper following Linux
- **What**: `new_owner(dir, mode, umask, is_dir)` follows `vfs_prepare_mode` and `inode_init_owner`, stripping setgid before the umask, masking directories to permission and sticky bits and adding setgid under setgid parents; `owns` needs `CAP_FOWNER` with only the owner mapped, as `inode_owner_or_capable` does.
- **Why**: New files got the umask before the setgid strip, mkdir kept caller-supplied set-id bits, and `owns` also required the group to be mapped.
- **Tradeoffs**: None.
- **Watch out**: Callers creating nodes must pass the raw mode and the umask.
- **Source**: 5d23297

### D10: ptrace access checks pick credentials by mode, and procfs memory views use filesystem credentials
- **What**: With `PTRACE_MODE_FSCREDS` `may_ptrace` compares the filesystem ids and the effective set, as `cap_ptrace_access_check` does, and otherwise the real ids and the permitted set; opening `mem`, `auxv`, `environ`, `maps`, `smaps` and `pagemap` uses fscreds; user namespaces record whether their creator had `CAP_SETFCAP` (for id-map writes, kernel/process D32).
- **Why**: `may_ptrace` compared the target's permitted set with the caller's even for filesystem-credential checks, and the procfs memory views were checked with real credentials.
- **Tradeoffs**: None known.
- **Watch out**: Set-id helpers that changed their filesystem ids now get the access Linux gives them.
- **Source**: f6a82cc

### D11: seccomp reports pids in the caller's pid namespace
- **What**: The pid in `SECCOMP_IOCTL_NOTIF_RECV` and the tid of a failed TSYNC are given in the caller's pid namespace (`ESRCH` when invisible), and `CAP_NET_BROADCAST` joins the capability constants.
- **Why**: Notifications and TSYNC failures reported global pids.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 5d7b936

### D12: File capabilities set in a user namespace are stored as v3 with its root
- **What**: `FileCaps::xattr_to_store` follows `cap_convert_nscap` (a v2 value from an initial-namespace holder of `CAP_SETFCAP` stays v2, otherwise v3 with the mapped root as rootid, `EINVAL` when unmapped), `xattr_for_reader` follows `cap_inode_getsecurity` (v3 with the reader's view of the rootid, v2 where that root owns the reader's namespace, else `EOVERFLOW`), and execve applies capabilities only where `Cred::root_owns` the rootid (kernel/vfs D38).
- **Why**: setcap inside a user namespace (a podman build step) failed, and v3 capabilities applied only with rootid 0.
- **Tradeoffs**: Reading a value back depends on the reader's namespace, as on Linux, and existing v3 values with nonzero rootids, once ignored, now apply where their root owns the namespace.
- **Watch out**: getxattr can now return `EOVERFLOW` to callers that only handled `ENODATA`.
- **Source**: 2aca260

### D13: A privileged execve clears the `PER_CLEAR_ON_SETID` personality flags
- **What**: execve clears `PER_CLEAR_ON_SETID` when the program is setuid or setgid or its file capabilities would grant more than the caller had, judged by a second `Cred::exec` before no_new_privs or an unsafe execution (D4) takes the gain away, as `bprm_fill_uid` and `cap_bprm_creds_from_file` do.
- **Why**: A privileged execve kept those flags, a difference without effect until READ_IMPLIES_EXEC took effect.
- **Tradeoffs**: The capability gain is computed by a second credential transition.
- **Watch out**: A change to `Cred::exec`'s rules must keep that second call meaningful.
- **Source**: 5760479

### D14: Unreadable programs tie the address space to the user namespace that can read them
- **What**: `ThreadGroup::mm_user_ns` (unset until the first execve, copied at fork) is set at execve to the caller's user namespace, raised by `UserNs::holding` to the nearest ancestor where an unreadable program file or interpreter is owned by mapped ids, an unreadable interpreter also clears dumpable, and `Cred::may_ptrace` takes the group's `dump_scope()` and follows `__ptrace_may_access` with `cap_ptrace_access_check`: matching ids or `CAP_SYS_PTRACE` over the target, then dumpable or `CAP_SYS_PTRACE` over the address space's namespace, then the commoncap rule (D10).
- **Why**: An interpreter the caller could not read left the process dumpable, and a tracer holding `CAP_SYS_PTRACE` only in a user namespace below the owner of an execute-only program could attach to it, both of which Linux's `would_dump` prevents.
- **Tradeoffs**: The namespace is kept per process, as dumpable is, rather than per address space.
- **Watch out**: A new caller of `may_ptrace` must pass the group's `dump_scope()`, since passing None lets the target's own namespace decide.
- **Source**: ef68770

### D15: seccomp ADDFD closes replaced descriptors as the target

- **What**: `addfd` uses `syscall::close_replaced` with the target fd table and tgid, including `fuse::flush_as`.
- **Why**: Supervisor-context close used the wrong mqueue notification and FUSE lock ownership.
- **Tradeoffs**: The path now shares the fd-replacement helper with normal replacement operations.
- **Watch out**: New replacement paths must also close in the target process context.
- **Source**: 01bfd882; 20137e4c; d053be24; 39e55b4b

## Superseded

- ~~The tracer's privilege is a single flag recorded at attach and checked in the initial namespace (D4 in part)~~ → replaced by **kernel/signal D20** in f6a82cc (2026-11-08)
- ~~v3 file capabilities apply only with rootid 0 and are not converted when set inside a user namespace (D4 in part)~~ → replaced by **D12** in 2aca260 (2026-11-08)
- ~~The vDSO clock stubs skip seccomp filters for every clock (D5's tradeoff, round 59)~~ → replaced by **D5** in 16364c0 (2026-11-08)
- ~~Sharing the fs context (`CLONE_FS`) does not suppress execve's privilege gains (D4's tradeoff, round 119)~~ → replaced by **D4** in 6d9a9e8 (2026-11-09, no SUPERSEDES declared)
- ~~Credentials live in the thread group, so per-thread id syscalls and `setfsuid` change every thread (D1, round 08)~~ → replaced by **D1** in 1be1142 (2026-11-09)
