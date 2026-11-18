# Signals, ptrace and ABI (kernel/signal) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/signal"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b). D1–D11 were seeded by hand from the retired design
> doc §7 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: Signal frames copy the Linux arm64 layout byte for byte
- **What**: siginfo, ucontext, sigcontext and the `__reserved` records (fpsimd always, esr for faults) are laid out exactly as arm64 Linux writes them.
- **Why**: Unwinders, debuggers and runtimes (Go's asynchronous preemption, V8's wasm traps) read and modify the frame directly.
- **Tradeoffs**: No SVE/SME records, so the corresponding extensions can never be advertised without extending the frame.
- **Watch out**: `AT_MINSIGSTKSZ` (4720) and the record sizes are pinned by a test.
- **Source**: rounds 03 (`03-m1b`), 04 (`04-m3a`)

### D2: Handlers return through a vDSO trampoline with fixed bytes
- **What**: Without `SA_RESTORER`, x30 points at the vDSO's `__kernel_rt_sigreturn`, which is exactly `mov x8, #139; svc #0`.
- **Why**: Go on arm64 does not set `SA_RESTORER` and returns through the vDSO, and libgcc's unwinder recognises signal frames by matching these bytes.
- **Tradeoffs**: Every process needs a vDSO mapping.
- **Watch out**: Changing the trampoline bytes breaks unwinding through signal handlers.
- **Source**: round 10 (`10-m8`); risk A1

### D3: Asynchronous signals interrupt guest code with a kick, never with host signals
- **What**: Sending sets the engine's pending flag; translated code checks it at block entries and the interpreter at instruction boundaries, then exits to the kernel, which delivers.
- **Why**: Host signals would conflict with ART/bionic's signal handling, and stopping only at defined points keeps guest state precise.
- **Tradeoffs**: A delivery waits until the thread reaches a check (at most one block).
- **Watch out**: Every loop in translated code must pass through a kick check.
- **Source**: design review 2026-10-24 (§7.3, §7.4), round 04 (`04-m3a`)

### D4: vDSO clock reads run the host's own vDSO code, with `svc` stubs as the fallback
- **What**: The guest's vDSO is the host's `[vdso]` (hal D18) with every symbol but the clock functions and the sigreturn trampoline made undefined and its Linux note set to 6.1.0, used once the interpreter has run its clock functions against host readings; otherwise, and in time namespaces with offsets, the built image's `svc` stubs take the no-BKL fast path.
- **Why**: Every vDSO clock read entered the kernel layer, about 70 ns on top of the host's 19 ns, and Go, the JVM and node read clocks constantly.
- **Tradeoffs**: Guests see the host kernel's vDSO code, build id and vvar size, a host vDSO the engine cannot run falls back to the stubs silently, gettimeofday reports the host's timezone (the device's on Android, not 0), and time namespaces with offsets keep the stub speed.
- **Watch out**: Time-namespace detection reads the clock mode as the second word of the host's vvar data (Linux 5.6 on), an instruction a future host vDSO needs and the engine lacks shows only as the fallback, and the stubs must still not stop under ptrace or seccomp.
- **Source**: 16364c0 (replacing the stubs of design review 2026-10-24 §7.2 and rounds 25, 38)

### D5: ptrace follows Linux 6.1, with tracers as processes
- **What**: strace, gdb, delve, lldb and valgrind work through a Linux-compatible ptrace in which any thread of the tracer process may issue requests; there are no Yama restrictions.
- **Why**: ptrace used to return EPERM, so neither strace nor gdb worked; developers need both on the device.
- **Tradeoffs**: Tracees stay attached until the whole tracer process exits.
- **Watch out**: Register changes must go through the saved copy taken at each stop.
- **Source**: rounds 40 (`40-ptrace-strace`), 41 (`41-ptrace-gdb`), 45 (`45-ptrace-procfs`)

### D6: Hardware breakpoints and watchpoints are emulated, watchpoints without leaving Tier 1
- **What**: 6 breakpoint and 4 watchpoint slots; breakpoints force the interpreter, watchpoints keep translated code and only route accesses to watched pages through the interpreter.
- **Why**: Without slots gdb fell back to single-stepping software watchpoints, and running a watched thread entirely in the interpreter was an order of magnitude slower.
- **Tradeoffs**: Breakpointed threads still run in the interpreter.
- **Watch out**: Watched pages must never enter the TLB.
- **Source**: rounds 44 (`44-hw-watchpoints`), 50 (`50-watchpoints-tier1`)

### D7: Core dumps follow Linux 6.1's `do_coredump`
- **What**: Default core-generating signals write an arm64 ELF core per `core_pattern`, `core_uses_pid` and `coredump_filter`; pipe patterns are not supported.
- **Why**: Programs that crashed could not be analysed afterwards, and `WCOREDUMP` was always 0 (several LTP tests failed).
- **Tradeoffs**: No pipe form (no user-mode helper), no `suid_dumpable`.
- **Watch out**: The dumping thread waits for all other threads to exit first.
- **Source**: round 43 (`43-coredump`)

### D8: The kernel layer keeps its own printk-style log
- **What**: A 128 KB / 4096-record ring buffer, read through `/dev/kmsg`, `syslog(2)` and `/proc/kmsg`, fed by guest writes and kernel-layer events (OOM kills, bug kills, unhandled-exception reports (D23), the BKL watchdog's record of a long hold (kernel/process D38), cgroup fork rejections, low-port remapping and permission warnings).
- **Why**: Processes killed by the OOM killer or internal errors showed only "Killed" in the terminal, with the reason in host logcat; `dmesg` and programs writing `/dev/kmsg` had nothing to use.
- **Tradeoffs**: The host kernel's log is not available (Android's app seccomp forbids `syslog`).
- **Watch out**: Permission rules (`dmesg_restrict`, `CAP_SYSLOG`) must match Linux so unprivileged reads behave the same.
- **Source**: rounds 140 (`140-ioprio-syslog-adjtimex`), 143 (`143-kmsg`); writers since added by 7b7f73b and 64a6107

### D9: `uname` reports a fixed Linux 6.1 as the ABI contract
- **What**: The guest always sees `6.1.0-tuxport`, and unspecified behaviour follows Linux 6.1; a few newer features are deliberate exceptions.
- **Why**: A fixed LTS version decouples guest behaviour from the phone's kernel; 6.1 is new enough for guest software and used by Debian 12.
- **Tradeoffs**: Native comparison hosts run newer kernels, so tests compare only behaviour common to versions.
- **Watch out**: Upgrade the contract (likely to 6.12) when important software needs newer features, when exceptions approach about ten, or when major distributions leave 6.1.
- **Source**: design review 2026-10-24 (§7.5), round 10 (`10-m8`); contract review 2026-11-02

### D10: Unimplemented syscalls return ENOSYS and never crash
- **What**: The dispatcher's default arm returns `ENOSYS` and counts the call among the unimplemented paths (D24); io_uring stays `ENOSYS`.
- **Why**: Guest programs probe syscalls and fall back on `ENOSYS`; a crash would take down the session. io_uring was tried (about 3800 lines for the interface and 49 opcodes) and abandoned.
- **Tradeoffs**: Programs that require a missing syscall fail with an error instead of working.
- **Watch out**: Keep the list of `ENOSYS` numbers documented (round 139, with the map noting those implemented since, such as process_mrelease).
- **Source**: rounds 19 (`19-deferred`), 139 (`139-unimplemented-syscalls`); 99b4216, 39ea702

### D11: The guest cannot change host time
- **What**: Setting the clock or modifying `adjtimex` is `EPERM`, as if `CAP_SYS_TIME` were missing; `adjtimex` queries report the host's NTP state.
- **Why**: Guest time is the host's, and tools (busybox `adjtimex`, `ntptime`, node_exporter) only need read-only queries; Android's app seccomp forbids calling `adjtimex`/`clock_adjtime` (the whole app would get `SIGSYS`), so the state there must be synthesized.
- **Tradeoffs**: NTP daemons in the guest only observe.
- **Watch out**: On Android the synthesized state is unsynchronized, which is the real state there (Android sets time with `settimeofday` and runs no kernel NTP adjustment).
- **Source**: round 140 (`140-ioprio-syslog-adjtimex`)

### D12: Signal delivery and the Linux ABI remain separate from tracing and vDSO construction
- **What**: kernel/signal owns signals, frames, restart, faults, core dumps, kernel logging and Linux ABI policy; tracing implementation belongs to kernel/ptrace and vDSO image construction to kernel/vdso, with integration call sites retaining their existing owners.
- **Why**: Separating independently maintained subsystems makes the oversized parent map practical to read and maintain.
- **Tradeoffs**: Cross-boundary work follows child maps, while historical decisions keep their original parent tags and stable IDs.
- **Watch out**: Bugs confirmed at the split or found by Starnix's and gVisor's tests stay open in the map, among them an early `CLD_STOPPED`, late single-step reports, unforced ptrace reads, no `PTRACE_EVENT_EXIT` for threads ended by another thread, a repeated stop report after detaching at an attach-time group stop, `SIGILL` frames carrying an `esr_context` with EC 0 where Linux 6.1 emits none, and synthesized abort ESRs that differ from Linux's; these pre-split qualifications remain discoverable through the child maps and are not resolved by the ownership split.
- **Source**: 9e5be2b, d2b1dc3 (bug list since updated by 4e64e74, ce8a9ce, 5d7b936 and 7b7f73b, and shortened by D17–D21 and D25); 8a17e4a9; d053be24; 60790c14; 39e55b4b

### D13: pidfd_send_signal through a `/proc/<tid>` directory signals the thread group
- **What**: A `/proc/<tid>` directory fd finds its process with `group_of` and the signal goes to its tgid, as `kill_pid_info` does for such an fd up to Linux 6.8.
- **Why**: pidfd_send_signal through a non-leader thread's `/proc` directory found no process.
- **Tradeoffs**: The differential test cannot cover it, since kernels from 6.9 infer the scope from the fd and treat this case differently.
- **Watch out**: None known.
- **Source**: 173da76

### D14: Signal files read in the order their code runs
- **What**: signal.rs puts senders and `deliver` before their helpers (the queue limits under generation, the alternate stack under frames); ptrace.rs follows stops from signal delivery to traced exits; klog.rs runs storing, readers, formatting, lookups and positions; the other files, syscall/misc.rs included, put helpers after their callers.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; signal.rs and klog.rs put most helpers above their callers.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; Default impls stay before the inherent impls, as constructors.
- **Watch out**: Code appended at the end of a file drifts from this order.
- **Source**: bf240bc

### D15: kill takes a thread's id for its process
- **What**: A positive pid names the thread group with that id if one exists (so a group whose leader has exited stays reachable) and otherwise the thread group of the task with that id, as Linux's `kill_pid_info` does.
- **Why**: Users pick threads by id in htop and `top -H`, and kill returned `ESRCH` for them.
- **Tradeoffs**: None known.
- **Watch out**: None known.
- **Source**: f85963d

### D16: Process signals are retargeted to a thread that will take them
- **What**: `retarget` wakes, per signal, the first thread that does not block it; delivery calls it for shared pending signals the thread blocks, thread exit for all of them (kernel/process D20), and group sends skip threads in a ptrace stop.
- **Why**: Linux retargets shared pending signals when a thread blocks them or exits and never picks a stopped or traced thread, while Tuxport left them with the chosen thread, so Ctrl-C or SIGTERM sometimes did nothing.
- **Tradeoffs**: While such a signal stays pending, every delivery on the blocking thread kicks the other thread again, and `CLD_STOPPED` still comes early.
- **Watch out**: A thread that blocks a signal another thread is slow to take wakes that thread repeatedly until it does.
- **Source**: 22e2b41

### D17: Detach keeps the syscall number and the hardware debug slots
- **What**: Detach keeps `syscallno` with the stop and saved registers of a stopped thread, detach and attach carry the recorded hardware debug slots and their pending change over the reset of the trace state, and a tracer's exit detaches through the same path.
- **Why**: The number of a thread stopped at syscall entry lives in its trace state so the tracer can change it, and detach reset it to 0; detach also wiped the recorded debug slots while the CPU kept trapping on those already applied, where arm64 Linux keeps them.
- **Tradeoffs**: As on Linux, a tracer that dies with watchpoints set leaves the tracee trapping into `SIGTRAP`.
- **Watch out**: Other trace-state fields reset by detach and attach would be lost the same way, since only stop, regs, resume, syscallno and the debug slots are kept.
- **Source**: b877707, 6d9a9e8

### D18: A signal frame that cannot be used forces SIGSEGV through normal delivery
- **What**: When `setup_frame` fails, delivery puts back a swapped sigsuspend/ppoll mask, leaves the mask alone and forces `SIGSEGV` with `SI_KERNEL` (`force_fatal` when the signal was `SIGSEGV`) before continuing the delivery loop, with restart decided once by the first signal taken; rt_sigreturn with a bad frame forces `SIGSEGV` at SP (`SEGV_ACCERR` when `Mm::mapped_above`, else `SEGV_MAPERR`; kernel/mm D28) and returns before the syscall-exit stop and delivery.
- **Why**: Frame-setup failures and invalid sigreturn frames called group exit directly, skipping the handlers, core dumps and ptrace stops that Linux's forced `SIGSEGV` goes through.
- **Tradeoffs**: The frame is validated before anything changes, so after a bad sigreturn the registers are the caller's, where Linux may have restored some before failing.
- **Watch out**: A program looping on signals over a broken stack now runs one `SIGSEGV` handler per failure instead of dying at the first.
- **Source**: 41bd6ec

### D19: rt_sigqueueinfo reaches zombies, and core_pattern's `%d` prints the dumpable value
- **What**: rt_sigqueueinfo accepts any existing thread group except pid 1, as kill does, and `%d` in `core_pattern` prints the dumpable value.
- **Why**: rt_sigqueueinfo refused zombies that kill accepts, and `%d` printed the inverse of the dumpable flag.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 5d23297

### D20: Tracers keep their credentials and are judged over the tracee's user namespace
- **What**: `Trace::cred` holds the tracer's credentials from `PTRACE_ATTACH`, `PTRACE_SEIZE` and auto-attach and the caller's own from `PTRACE_TRACEME`, as Linux's `ptrace_link` records them, and `tracer_capable` checks `CAP_SYS_PTRACE` over a given user namespace, so `PEEK*`/`POKE*` on a non-dumpable tracee are `EIO` unless the tracer was capable over its namespace, and execve's suppression of privilege gains under a tracer (kernel/cred D4) uses the same check.
- **Why**: A tracer attached without `CAP_SYS_PTRACE` kept reading and writing a process that later made itself non-dumpable (ssh-agent), a tracer privileged only inside a user namespace was judged in the initial one, and `PTRACE_TRACEME` kept the parent's credentials, letting a privileged parent that later runs child-controlled code keep a privileged ptrace relationship (CVE-2019-13272).
- **Tradeoffs**: Each traced thread holds a cloned `Cred`, and a traced unprivileged child of a root parent gains nothing from setuid programs, as on Linux.
- **Watch out**: Linux checks against `mm->user_ns` while Tuxport checks the namespace of the tracee's current credentials, which differ after setns into another user namespace.
- **Source**: f6a82cc, 1be1142

### D21: rt_sigaction masks unknown flags, and sender-supplied siginfo keeps its fields
- **What**: rt_sigaction masks flags with `UAPI_SA_FLAGS`, and `InfoFields::User` keeps `si_errno`, `si_pid` and `si_uid` (converted like `Queue`'s) and bytes 24 to 48 of siginfo given by rt_sigqueueinfo and the like, which signalfd reports with its errno and value.
- **Why**: Echoed unknown flags made `SA_UNSUPPORTED` probing read Tuxport as an old kernel, and sender-supplied siginfo lost `si_errno` and every field beyond `si_value`.
- **Tradeoffs**: Bytes past `kernel_siginfo` are dropped as Linux drops them, and layouts without pid and uid (an `SI_TIMER` code sent by a process) still get the conversion.
- **Watch out**: None known.
- **Source**: dcacfb6

### D22: signalfds open read-write and poll as `POLLIN` alone
- **What**: signalfds are opened `O_RDWR`, as on Linux, with write still `EINVAL` through the write path, and report only `POLLIN` in kernel/poll's readiness table (kernel/poll D15).
- **Why**: signalfds were opened `O_RDONLY` and reported `POLLRDNORM`, unlike Linux.
- **Tradeoffs**: None known.
- **Watch out**: None known.
- **Source**: e6bfdf8

### D23: Unhandled exceptions are reported in the kernel log as arm64's `arm64_show_signal` does
- **What**: While `debug.exception-trace` is non-zero (0 by default as on arm64, kernel/procfs D16, and 1 from boot in the app, kernel/process D35), `show_unhandled` logs each synchronous exception that ends an untraced process whose handler is default or ignored (page faults, `SIGBUS` past EOF, alignment, undefined instructions, `brk`, hardware breakpoints and bad rt_sigreturn frames) with Linux 6.1's names, the faulting mapping and the registers, at most 10 per 5 s kernel-wide.
- **Why**: Guest programs killed by a fault left nothing in the kernel log beyond the shell's "Segmentation fault", and the Starnix survey listed arm64's per-fault report as a small gain.
- **Tradeoffs**: A report takes about 16 of the log's 4096 records, synthesized ESRs name other levels and fault kinds than native, there is no Hardware name line, a bad sigreturn frame shows the caller's registers (D18), and seccomp's `SIGSYS`, unwritable frames and single-step traps go unreported, as on Linux.
- **Watch out**: A program crashing in a loop keeps adding 10 reports per 5 s, pushing older records out of the log that bug reports carry, and a fault path added later without a `show_unhandled` call goes unreported.
- **Source**: 7b7f73b

### D24: Unimplemented paths are counted per kernel and named in bug reports
- **What**: `stub.rs` keeps a table of the paths Tuxport does not implement (syscalls, prctl options, fcntl commands, ioctl requests by kind of file, madvise advice, futex commands, missing paths and devices without a driver), each with its count and first command and at most 128 paths, whose ten most-called go into `KillReason::Bug`'s report while `TUXPORT_LOG` writes each path's first hit and nothing reaches the guest.
- **Why**: Unimplemented paths ended in a bare error code, at best one `ENOSYS` line under `TUXPORT_LOG`, so neither bug reports nor surveys showed what a failing workload had asked for; Starnix counts them with `track_stub!`.
- **Tradeoffs**: Invalid values from tests take table slots, counts run per kernel since boot and a full table names no new paths, `KillReason::Bug` gained a public field, and `TUXPORT_LOG` no longer prints every `ENOSYS` call.
- **Watch out**: A default arm that also returns Linux's own refusals counts noise unless filtered, as prctl's refused arguments and terminal ioctls on files that are not terminals are.
- **Source**: 99b4216, c008749 (the unit test's device without a driver)

### D25: A pending standard signal keeps the siginfo of its first send
- **What**: `Pending::add` stores a standard signal's siginfo only when the signal becomes pending, as Linux's `legacy_queue` drops later sends.
- **Why**: Later sends replaced the pending siginfo, so `SIGCHLD` from several children reported the last one and a forced `SIGSYS` replaced a pending `SI_TKILL`.
- **Tradeoffs**: A synchronous fault forced while the same signal is already pending loses its fault siginfo, as on Linux.
- **Watch out**: Code that relied on a later send refreshing the siginfo must update it in place, as POSIX timers add overruns through `infos_mut`.
- **Source**: f5c09f7

### D26: Signal permission and ptrace reports use the thread's own credentials
- **What**: Over per-thread credentials (kernel/cred D1), `may_kill` judges tkill, tgkill and rt_tgsigqueueinfo by the target thread's credentials and process signals by the main thread's, a traced thread's exit and stop reports carry its own real uid, a signal the tracer replaces names the tracer's, and a core file name's `%u` and `%g` are the dumping thread's.
- **Why**: Kill permission and ptrace reports used the process's credentials for thread targets, unlike Linux.
- **Tradeoffs**: None known.
- **Watch out**: A guest relying on threads sharing one set of credentials for signal permission sees per-thread results, as on Linux.
- **Source**: 1be1142

### D27: Delivery after a syscall forgets it, as Linux's `forget_syscall`
- **What**: `deliver`, after the restart rewind of a syscall's return, calls `KernelEntry::forget_syscall` (kernel/process D55) with the possibly rewound pc, so a thread stopped by `SIGSTOP` in a syscall shows -1 with its sp and the restart pc in `/proc/<pid>/syscall` (kernel/procfs D40).
- **Why**: Linux's do_signal forgets the syscall before taking a signal, while Tuxport showed the syscall the thread had been in.
- **Tradeoffs**: Every BKL-path syscall pays the record's stores once more, and the difference shows only while the thread waits inside `deliver` (stops, and ptrace stops after this point).
- **Watch out**: A ptrace stop placed later in `deliver` shows -1 where Linux might still show the syscall.
- **Source**: f1212d8

### D28: Freezer entry and group kills mark cgroup observations

- **What**: Use the same freezer-entry transition as vfork and mark the group when setting killed flags.
- **Why**: Entering a freezer stop or killing tasks can complete a subtree freeze.
- **Tradeoffs**: Group exit can mark observations whose contents stay unchanged.
- **Watch out**: Stop, thaw and kill paths must agree on which tasks contribute to freeze completion.
- **Source**: 0f5c5132

## Superseded

- ~~vDSO time functions are `svc` stubs served without the BKL, with no vvar page (D4, rounds 25, 38)~~ → replaced by **D4** in 16364c0 (2026-11-08)
- ~~D12 combined ownership of the newly extracted child modules~~ → replaced by **D12** in 60790c14 (2026-11-15; scope only, inherited implementation decisions remain here)
