# kernel/signal Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/signal.md` for the paired decision history, including pre-split decisions for kernel/ptrace and kernel/vdso.
>
> Verified: 2026-11-03 (full mapping: 4 research concerns; 16 claims checked by 2 independent verifiers — 13 confirmed and 3 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: split kernel/ptrace and kernel/vdso into separate maps)

## Responsibilities

`kernel/signal` covers how guest code is interrupted, notified, debugged and dumped, and the
user-visible ABI contract:

- signal state, generation and delivery (handlers, masks, queues, job control);
- the arm64 Linux signal frame, `rt_sigreturn`, syscall restart, and mapping synchronous engine
  exits to signals;
- integration with `kernel/vdso`'s sigreturn trampoline and `kernel/ptrace`'s delivery stops;
- core dumps;
- the kernel log (`/dev/kmsg`, `syslog(2)`, `/proc/kmsg`);
- the kernel-version contract, Linux's errno numbers, the `ENOSYS` policy for unimplemented
  syscalls, counting the paths the kernel layer does not implement, and refusing guest clock
  changes;
- the local debug log and syscall trace on the host's stderr (`TUXPORT_LOG`, `TUXPORT_STRACE`).

Root: `core/kernel/src/signal.rs`, `syscall/sig.rs`,
`coredump.rs`, `klog.rs`, `errno.rs`, `debug.rs`, signalfd (in `fd.rs`/`syscall/sig.rs`), and in
`syscall/misc.rs`:
`uname`, `syslog`, `adjtimex`, clock setting, `restart_syscall`; the dispatcher's restart and
unimplemented-syscall handling in `syscall/mod.rs`; `stub.rs`, which counts unimplemented paths
(the default arms calling it stay with their modules); the fault-to-signal branch of the run loop
in `task.rs`.

Outside the module: waking (`kernel/process`), namespace translation (`kernel/ns`), CPU state, kicks
and the host fault guard (`engine`), page-fault classification (`kernel/mm`), seccomp (with
credentials), procfs; ptrace and process_vm requests (`kernel/ptrace`); vDSO image
construction and clock-stub detection (`kernel/vdso`).

## Key types

- `SigAction` — handler, flags, restorer, mask, and `immutable` (`SA_IMMUTABLE`, used by seccomp
  kills); `SigHandlers` — the 64-entry table, shared under `CLONE_SIGHAND`.
- `SigInfo` — code, typed fields (kill, child, queue, timer, fault, poll, sys, raw), the synthesized
  ESR for faults, sender and user-namespace data for translation; encodes the 128-byte siginfo.
- `Pending` — a set plus one siginfo per standard signal (the first sender's, sends while pending
  being dropped) and a FIFO queue for real-time signals; per thread and per thread group.
- `AltStack` — sp, size, flags; thread-private.
- Thread state: `pending`, `sigmask`, `saved_sigmask`, `killed`; group state: `pending`,
  `sighand`, `stopped`, `stop_signal`, stop/continue reports, `exiting`.
- `SignalFd` — mask and the creating group.
- `coredump::ThreadCore`, `CoreWriter` — per-thread state collected for a core file, and its writer.
- `klog::Klog`, `Record`, `KmsgFile` — the ring buffer, its records, one `/dev/kmsg` open.
- `stub::Stub` — an unimplemented path with the value that reached it (syscall number, prctl
  option, fcntl command, ioctl request with the kind of file, madvise advice, futex command, a
  missing name's path, a device without a driver); `stub::Stubs` — the kernel's table of them
  (`Kernel::stubs`, in a cell so procfs views can record): count and first caller's comm per path,
  in first-hit order.
- `Errno` (`errno.rs`) — Linux's errno numbers as the guest sees them, independent of the host
  (host calls' raw errnos are wrapped as they are, the hosts being Linux and Android on arm64);
  also the restart codes `ERESTARTSYS`, `ERESTARTNOINTR`, `ERESTARTNOHAND` and
  `ERESTART_RESTARTBLOCK` (512 to 516) and the internal `FUSE_RETRY` (600, never returned to the
  guest). `to_syscall_return` gives `-errno`.

## Public entry points

- **Signal syscalls**: `kill`, `tkill`, `tgkill`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo`,
  `pidfd_send_signal`, `rt_sigaction`, `rt_sigprocmask`, `rt_sigpending`, `rt_sigsuspend`,
  `rt_sigtimedwait`, `sigaltstack`, `signalfd4` (and reads), `rt_sigreturn`, `restart_syscall`.
- **Kernel-internal senders**: `send_to_task`, `send_to_group`, `send_to_pgrp`, `send_cpu_timer`,
  `force` (synchronous faults), `force_fatal` (seccomp), `group_exit`; `wake`, `interrupted`, `take`.
- **Delivery**: `signal::deliver(cx, cpu, syscall)` after each syscall, kick, fault handling,
  sigreturn and exec.
- **Core dumps**: `coredump::dump` from the default-action path.
- **Kernel log**: `klog::printk`/`emit` (used by OOM, bug kills, cgroups, networking), `/dev/kmsg`,
  `/proc/kmsg`, `syslog(2)`; `show_unhandled` (with `abort_name` and the `trap` names) from the
  run loop and rt_sigreturn.
- **ABI**: `uname`, `/proc/version`, `/proc/sys/kernel/{osrelease,version,ostype}`, the
  dispatcher's default arm, `settimeofday`, `clock_settime`, `adjtimex`, `clock_adjtime`.
- **Unimplemented paths**: `stub::hit` and `stub::ioctl` from default arms, `stub::record` from
  procfs views; `stub::top` for kernel-bug kill reports.
- **Debug log**: `debug::log` (core dumps, signal frames, seccomp, the vDSO choice, task exits,
  first stub hits); `trace_start`/`trace_syscall` in the dispatcher and `tracing` for the fast path.

## Data flow / lifecycle

### Sending a signal

- The siginfo is prepared for the target (`si_uid` translated, `si_pid` hidden if the target's pid
  namespace cannot see the sender); default-handled signals aimed at a pid-namespace init are
  dropped.
- `SIGKILL` is never queued: it goes straight to group exit (every thread marked killed and woken).
- Otherwise: a stop signal discards pending `SIGCONT`; `SIGCONT` discards pending stop signals,
  ends a group stop and reports `CLD_CONTINUED`. An ignored signal that is not blocked and not
  traced is discarded at send time.
- Standard signals coalesce (one pending instance; a send while it is pending is dropped, so the
  first siginfo stays, as `legacy_queue` does);
  real-time signals queue in order, limited by `RLIMIT_SIGPENDING` (default 16384), counting only
  queued real-time entries per thread group. When the queue is full, `kill` and kernel-generated
  signals still set the pending bit but lose the siginfo; `tkill`/`tgkill` of a real-time signal,
  and the queue syscalls with a non-`SI_USER` siginfo, return `EAGAIN`.
- Waking: a thread-directed signal wakes its thread if unblocked; a process-directed one wakes the
  first thread (leader first) that has it unblocked and is not in a ptrace stop (CPU-timer signals
  rotate among running threads); if no thread qualifies, only a `SIGNAL` wake is requested. Waking
  kicks the CPU and signals the thread's event (`kernel/process`).
- Retargeting (Linux's `retarget_shared_pending`): at the end of `deliver`, process-directed
  signals still pending that the thread now blocks wake a thread that does not; a thread leaving
  a live process does the same for every process-directed signal pending.
- Permissions: `may_signal` (same user or `CAP_KILL`) against the target thread's credentials
  for tkill/tgkill and the main thread's for a process; `kill` handles `-1` (all except init and
  self), `0` and negative process groups; virtual pid 1 follows `kernel/boot`'s power-request policy.

### Delivery

- `deliver` runs before every return to guest code. For an interrupted syscall it first rewinds
  (`x0 = orig_x0`, PC back to the `svc`); after a syscall it then forgets it, as Linux's
  `forget_syscall`, so a thread stopped here shows -1, its sp and the (rewound) pc in
  `/proc/<pid>/syscall`.
- Loop: wait while the group is stopped (and while frozen by a cgroup), handle ptrace group-stop
  and trap checks, then dequeue — the thread's own pending before the group's, synchronous signals
  (SEGV, BUS, ILL, TRAP, FPE, SYS) first, then the lowest number — and offer a ptrace
  signal-delivery stop (not for immutable actions).
- Disposition: ignored (or ignored by default, or an unkillable init) → skip; default stop → group
  stop (dropped for an orphaned process group except `SIGSTOP`); other defaults → group exit, with
  a core dump for core-generating signals; a handler → set up a frame.
- For the first handler frame, an interrupted syscall returns `EINTR` instead of restarting when
  the code is `ERESTARTNOHAND`, `ERESTART_RESTARTBLOCK`, or `ERESTARTSYS` without `SA_RESTART`.
- After a frame: the mask becomes mask | action mask | the signal (unless `SA_NODEFER`), never
  including `SIGKILL`/`SIGSTOP`; `SA_RESETHAND` resets the handler to default. More signals may
  stack further frames.
- If no handler ran: `saved_sigmask` (from sigsuspend or ppoll) is restored, and an
  `ERESTART_RESTARTBLOCK` syscall re-executes as `restart_syscall`, which resumes the saved sleep or
  futex deadline.
- Group stop: marks the group stopped, notifies the parent with `CLD_STOPPED` (`SIGCHLD` unless the
  parent set `SA_NOCLDSTOP`) and requests a `CHILD` wake.

### Signal frames

- The frame copies Linux arm64 byte for byte: `siginfo` (always written), `ucontext` (flags and
  link 0, `uc_stack` describing the altstack before auto-disarm, the mask to restore),
  `sigcontext` (fault address = `si_addr` or 0, x0–x30, sp, pc, pstate with NZCV and DIT), and
  `__reserved` records: `fpsimd_context` (FPSR, FPCR, V0–V31) always; `esr_context` only when the
  siginfo carries a synthesized ESR; zero-filled to a null terminator. No SVE, ZA or extra records.
- Below it a `frame_record {fp, lr}`; everything 16-byte aligned. `AT_MINSIGSTKSZ` reports 4720; the
  `sigaltstack` minimum is 5120.
- The stack is the altstack top if `SA_ONSTACK` is set, the altstack is enabled and SP is not
  already on it; `SS_AUTODISARM` resets the altstack after use.
- Handler entry: x0 = signal number; x1 = siginfo and x2 = ucontext with `SA_SIGINFO`; sp = frame;
  x29 = the frame record; x30 = `sa_restorer` with `SA_RESTORER`, else the vDSO's
  `__kernel_rt_sigreturn`; pc = handler.

### `rt_sigreturn`

- Validates before changing anything: SP 16-byte aligned, frame readable, pstate mode bits zero,
  records with valid sizes, exactly one `fpsimd_context` of 528 bytes; ESR records ignored; any
  other magic invalid.
- Restores x0–x30, SP, PC, NZCV (and DIT when advertised), FPSR, FPCR (unmasked), V0–V31, the mask
  (minus `SIGKILL`/`SIGSTOP`) and the altstack. The engine looks up translations by the new PC, so a
  handler may change it (Go preemption, V8 wasm traps).
- A frame that cannot be written (Linux's `force_sigsegv`) leaves the mask unchanged, puts back a
  mask swapped by sigsuspend or ppoll, and forces a `SIGSEGV` (`SI_KERNEL`) delivered next, which a
  handler on an alternate stack can take; when the signal was `SIGSEGV` itself, the forced one is
  fatal (`force_fatal`), ending the process with a core and the tracer's stops. Restart is decided by
  the first signal taken. A bad frame at sigreturn changes nothing, returns 0 and forces a
  `SIGSEGV` at SP (`SEGV_ACCERR` when a mapping ends above it, else `SEGV_MAPERR`).

### Synchronous faults to signals (run loop, after `kernel/mm` decides page faults)

- `brk` → `SIGTRAP`/`TRAP_BRKPT`, `si_addr` = PC, no ESR record.
- Hardware breakpoint/watchpoint → `SIGTRAP`/`TRAP_HWBKPT`, address = PC or access start, no ESR.
- Alignment → `SIGBUS`/`BUS_ADRALN` with ESR EC 0x24 (data, FSC 0x21, WnR on writes), 0x22 (PC,
  no FSC) or 0x26 (SP, `si_addr` = SP).
- Undefined or non-whitelisted encodings → `SIGILL`/`ILL_ILLOPC`.
- Just before forcing, `show_unhandled` (arm64's `arm64_show_signal`) reports the exception in the
  kernel log while `debug.exception-trace` is non-zero (0 at start as on arm64, 1 when the host
  starts the kernel with `MachineOptions::exception_trace`, as the app does) and the signal will
  go unhandled: the handler is the default or ignored and the thread is not traced (Linux's
  always-report for the initial init has no counterpart: Tuxport's pid 1 is not guest code). One
  INFO line `comm[tid]: unhandled exception: `, the exception class and ESR unless the ESR is 0,
  the exception's name (the abort's fault status name, or `undefined instruction`, `User debug
  trap`, `Hardware breakpoint trap (ptrace)`, `SP/PC alignment exception`), and ` in
  file[start+len]` for the first mapping ending above the pc when it maps a file (`find_vma`, so a
  pc in a gap names the next mapping); then at the default message level the `CPU:` line, pstate,
  pc, lr, sp and x29–x0. Undefined instructions and debug exceptions show no ESR, as on Linux. At
  most 10 reports per 5 s kernel-wide (`Kernel::unhandled_rs`); the first one after a dropped
  interval is preceded by `arm64_show_signal: N callbacks suppressed`. A bad rt_sigreturn frame is
  reported as `illegal memory access` with the caller's registers; seccomp's `SIGSYS`, a frame that
  cannot be written and single-step traps are not reported.
- `force` unblocks or resets a blocked or ignored synchronous signal so it is delivered.
- The faulting instruction has no visible effect (no register or base writeback), and the PC in the
  frame points at it (precision comes from the engine's checks).

### Signal syscalls

- `rt_sigaction`: installing an action for `SIGKILL`/`SIGSTOP` is `EINVAL` (querying them works);
  flags outside Linux's `UAPI_SA_FLAGS` are cleared, so `SA_UNSUPPORTED` probing sees a 5.11+
  kernel; any change that makes a signal ignored (`SIG_IGN`, or `SIG_DFL` for a default-ignored
  signal) discards its pending instances.
- `rt_sigsuspend` uses `saved_sigmask` and returns `ERESTARTNOHAND`; `rt_sigtimedwait` returns only
  signals from its set (`EINTR` for others); `rt_sigpending` reports pending ∩ blocked.
- `sigaltstack`: `SS_DISABLE`, `SS_ONSTACK` reporting, `SS_AUTODISARM`.
- `tkill`/`tgkill` use `SI_TKILL`. `kill(pid > 0)` and `rt_sigqueueinfo` accept any thread's tid
  and signal its process (Linux's `kill_pid_info`); both accept a zombie not yet reaped (nothing is
  queued) and refuse pid 1 (`ESRCH` for `rt_sigqueueinfo`). A siginfo given to `rt_sigqueueinfo`,
  `rt_tgsigqueueinfo` or `pidfd_send_signal` is delivered as given as far as `kernel_siginfo`
  reaches (`si_errno` and the first 32 bytes of the union), si_pid and si_uid converted as for
  sigqueue.
- signalfd: opened read-write as Linux does (a write is `EINVAL`); reads dequeue matching signals
  of the calling thread and its group, in the Linux `signalfd_siginfo` layout; readiness
  (`POLLIN` alone, as signalfd_poll) also follows the calling thread.

### exec and clone

- exec: shared handlers are unshared, handlers reset to default (ignored ones stay ignored), the
  altstack and `saved_sigmask` cleared; pending signals and the mask survive; the thread re-kicks
  itself if something is deliverable.
- clone: the child inherits the mask with nothing pending; `CLONE_SIGHAND` shares handlers,
  `CLONE_CLEAR_SIGHAND` resets them; a `CLONE_VM` child without vfork gets no altstack.

### Core dumps

- Written when a core-generating signal (`SIGQUIT`, `SIGILL`, `SIGTRAP`, `SIGABRT`, `SIGBUS`,
  `SIGFPE`, `SIGSEGV`, `SIGXCPU`, `SIGXFSZ`, `SIGSYS`) takes its default action as the group's first
  exit, `RLIMIT_CORE` is at least a page and the process is dumpable. A pipe `core_pattern` is not
  supported (skipped with a debug log).
- The dumping thread waits until the other threads have exited, each saving its registers.
- The name expands `core_pattern` (`%%`, `%p`, `%P`, `%i`, `%I`, `%u`, `%g`, `%s`, `%t`, `%h`, `%e`,
  `%E`, `%c`, `%d` the dumpable mode, 1); `core_uses_pid` appends `.pid` when the pattern has no pid. The file is
  created relative to the cwd (any old file unlinked first) with `O_CREAT|O_EXCL|O_NOFOLLOW`, mode
  0600. `core_pattern` itself (at most 127 bytes) is a root-owned 0644 sysctl, writable by euid 0
  only (`kernel/procfs`'s `/proc/sys` rule). `dumpable` is a flag (`PR_SET_DUMPABLE` 0 or 1; no
  `suid_dumpable`).
- Format: arm64 Linux ELF core — `PT_NOTE` with per-thread `NT_PRSTATUS`, `NT_PRFPREG`,
  `NT_ARM_TLS`, `NT_ARM_HW_BREAK`, `NT_ARM_HW_WATCH`, `NT_ARM_SYSTEM_CALL`, plus `NT_PRPSINFO`,
  `NT_SIGINFO`, `NT_AUXV`, `NT_FILE`; `PT_LOAD` per `coredump_filter` (`kernel/mm`) with untouched
  pages left as holes; writing stops at `RLIMIT_CORE`.
- A successful dump sets the wait status's core flag and `CLD_DUMPED`.

### Kernel log

- A printk-style ring buffer: 128 KB of text, at most 4096 records, oldest dropped first; records
  carry sequence, monotonic timestamp, facility, level and text (truncated to 991 bytes).
- Writers: guest `/dev/kmsg` writes (prefix parsing, ratelimit 10 per 5 s by default, drops
  reported at close), the OOM killer, bug kills, unhandled-exception reports, the BKL watchdog
  (a too-long hold once it ended, `kernel/process`), cgroup fork rejections, low-port remapping,
  permission warnings. The host kernel's log is not included.
- Readers: `/dev/kmsg` (a sequence per open, one record per read, `EPIPE` after records were
  dropped, seeks to start, data or end), `syslog(2)` and `/proc/kmsg` (a shared read position;
  READ_ALL/READ_CLEAR return the newest records since the clear point).
- Permissions follow `dmesg_restrict` and `CAP_SYSLOG` (`CAP_SYS_ADMIN` works with a warning);
  `printk_devkmsg=off` makes opening `/dev/kmsg` `EPERM`; console log levels are only recorded.

### ABI contract

- `uname -r` reports `6.1.0-tuxport` (version `#1 SMP PREEMPT tuxport`, machine `aarch64`, nodename
  from the UTS namespace); `/proc/version` and `/proc/sys/kernel/{osrelease,version,ostype}`
  match. Unspecified behaviour follows Linux 6.1. Deliberate exceptions taken from newer kernels:
  `cachestat` (6.5), `/proc/<pid>/fd` entries not usable as mount points (6.12), the time-namespace
  relaxations (6.2); code comments also follow newer kernels for `RWF_NOAPPEND` (6.9) and the
  generic-netlink `nlctrl` replies (6.8).
- Unimplemented syscalls (including io_uring) return `ENOSYS` from the dispatcher's default arm and
  never crash; each is counted as an unimplemented path (below). The list of
  `ENOSYS` numbers is kept in round 139 (`docs/rounds/139-unimplemented-syscalls.md`), less
  process_mrelease (448), implemented since (`kernel/oom`).
- `gettimeofday` reports the host kernel's timezone (`sys_tz`; Android sets it to the device's time
  zone), from the host's vDSO and from the syscall alike.
- The guest cannot change host time: `settimeofday` is `EPERM` without reading its arguments;
  `clock_settime` validates the timespec, then is `EPERM` for `CLOCK_REALTIME` and `EINVAL` for every
  other clock; any modifying `adjtimex`/`clock_adjtime` is `EPERM` (as without `CAP_SYS_TIME`);
  queries read the host's NTP state on Linux hosts and report unsynchronized (`STA_UNSYNC`,
  `TIME_ERROR`) on Android.

### Unimplemented paths

- `stub::hit` counts, per kernel, each path the kernel layer does not implement, as Starnix's
  `track_stub!` and `not_found` do: the dispatcher's default arm (`ENOSYS`; not number -1), prctl
  options (`EINVAL`; not the options it handles given refused arguments), fcntl commands
  (`EINVAL`), madvise advice it refuses (`EINVAL`, after the range checks), futex commands
  (`ENOSYS`), ioctl requests that reach a default arm (`ENOTTY`) with the kind of file (unhandled
  kinds and host fds not in the passthrough list, terminals, and sockets: requests the interface
  passthrough does not take, on any family, and those of a new network namespace), names path
  resolution does not find where Linux would have them (`/proc`, `/sys`, the boot `/dev`; the
  rules are `kernel/procfs`'s) and device nodes without a driver (`ENXIO`). `stub::ioctl` leaves
  out requests of type `'T'` on files that are not terminals, which Linux refuses there too,
  except TIOCOUTQ (SIOCOUTQ) on sockets and FIOQSIZE on regular files and directories. Values no
  Linux defines are counted like the others.
- Each path keeps its call count and the comm of its first caller; the table holds 128 paths, and
  hits of new paths after that are only counted as dropped.
- With `TUXPORT_LOG=1` the first hit of each path writes `tuxport: unimplemented <path>, first by
  <comm> (pid <pid>)` to the host's stderr (`syscall <nr>` for syscalls).
- A kernel-bug kill report (`KillReason::Bug::stubs`) carries the 10 most-called paths, ties in
  first-hit order (`prctl option 0x59616d61: 2 calls, first by gdb`; option tags of 0x10000 and
  above in hex), and a line counting the rest. Nothing reaches the guest: the kernel log has no
  such lines.

### Debug log and syscall trace

- `TUXPORT_LOG` and `TUXPORT_STRACE` are on when set, not empty and not `0`, each read once per
  host process at its first use.
- `debug::log` writes `tuxport: <line>` to the host's stderr (logcat on Android); nothing reaches
  the guest.
- With `TUXPORT_STRACE`, the dispatcher prints each syscall it handled as it returns: the seconds
  since the first traced syscall, `[tid]`, the number, the six arguments in hex (pid arguments
  already translated to global pids), `=` the result in hex or `-errno`, and the milliseconds it
  took in angle brackets. The fast path is off while tracing (`syscall::fast`), so every syscall
  goes through the dispatcher; `exit`, `exit_group`, `rt_sigreturn` and a successful `execve`
  return before the print and show nothing.

## Dependencies (inbound / outbound)

### Inbound

- The run loop and dispatcher (`kernel/process`): delivery after every syscall, kick and fault;
  `orig_x0`; restart handling.
- Timers, the TTY layer (job-control signals to process groups), process exit (child status),
  seccomp (`force_fatal`), procfs (signal state), the OOM
  killer, bug handling and the BKL watchdog (kernel log; bug kill reports read `stub::top`).
- Default arms reporting unimplemented paths: prctl (`kernel/process`), futex (`kernel/futex`),
  fcntl, file ioctls and device opens (`kernel/files`), terminal ioctls (`kernel/tty`), socket
  ioctls (`kernel/net`), madvise (`kernel/mm`); missing names from procfs views (`kernel/procfs`).

### Outbound

- `kernel/process`: `request_wake*`, `signal_later`, group exit, wait; `kernel/ns`: namespace
  translation.
- `kernel/mm`: `read`/`write`, `grow_stack`, `dump_regions`/`dump_page`/`residency`,
  `regions` (the mapping named in unhandled-exception reports).
- `engine`: CPU state (registers, FP, DIT) and the `Kicker`.
- vfs (core files, `/dev/kmsg`), credentials (`may_signal`, `CAP_SYSLOG`),
  `tuxport_hal` (clocks, `adjtimex` query, `timezone`), procfs (`debug.exception-trace` through
  `recorded_value`, ` (deleted)` names through `mark_deleted`, `comm_str` for a path's first
  caller).
- `kernel/ptrace`: group/signal stops, fatal-exit hooks and shared register helpers; delivery
  must preserve stop ordering and skip signal stops for immutable actions.
- `kernel/vdso`: the fixed sigreturn trampoline offset; its clock-stub integration is handled
  by `kernel/process`, `kernel/ns` and `kernel/mm`.

## Invariants and gotchas

- Signals are delivered only at the kernel boundary; nothing interrupts guest code except a kick
  that makes the engine exit.
- `SIGKILL` and `SIGSTOP` can never be blocked, caught or ignored; `SA_IMMUTABLE` actions skip
  ptrace signal-delivery stops.
- The sigreturn trampoline's bytes are part of the ABI and must not change.
- `ERESTARTNOINTR` is recognised by delivery but nothing produces it.
- `LOG_CONT` is never set (no `KERN_CONT` merging); `/dev/kmsg` writes always carry a facility, so
  this is not visible to users.

## Confirmed bugs / technical debt

- **`CLD_STOPPED` is reported early:** the parent is notified as soon as the first thread starts a
  group stop, while siblings may still be running (Linux notifies once the last thread has
  stopped).
- **`SIGILL` frames carry an `esr_context` with EC 0:** Linux 6.1 emits none for undefined
  instructions.
- **Synthesized abort ESRs differ from Linux's:** every translation fault is reported at level 3
  and every fault inside a mapping as a permission fault, where Linux names the table level its
  walk stopped at (often level 2 for an address with no page tables) and reports a never-touched
  page whose mapping forbids the access as a translation fault; `esr_context` and
  unhandled-exception reports show the difference, which only tools decoding the ESR notice.
- **Minor divergences:** `settimeofday` skips argument validation and `clock_settime`
  returns `EINVAL` for CPU clocks where Linux returns `EPERM`; sigreturn silently drops DAIF bits
  Linux would reject; kernel-generated real-time signals lose their siginfo when the queue is full.


## Open questions

- Is the per-group (instead of per-user) `RLIMIT_SIGPENDING` accounting an intended
  simplification?

## To verify

- The Linux 6.1 behaviours used for comparison above (`legacy_queue`, `retarget_shared_pending`,
  `force_sigsegv`, `do_undefinstr`, `UAPI_SA_FLAGS`,
  `format_corename`, sysctl `test_perm`, the order of checks in time-setting syscalls) were checked
  from knowledge of the kernel, not against its source.
