# kernel/ptrace Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/ptrace.md` for the paired decision history (not yet created; pre-split decisions D5, D6, D12, the ptrace half of D14, D17, D20 and D26 stay in `.agents/decisions/kernel/signal.md`).
>
> Verified: 2026-11-15 (split out of the kernel/signal full mapping of 2026-11-03; the boundary was checked against the source at the split)

## Responsibilities

`kernel/ptrace` owns tracer requests, tracee stop/resume state, register sets, hardware-debug
slots, traced exit reporting and `process_vm_readv/writev`. Root: `core/kernel/src/ptrace.rs`
and `core/kernel/src/syscall/ptrace.rs`.

Call sites in signal delivery, the task loop, dispatcher, clone/exec/wait and seccomp stay in
`kernel/signal`, `kernel/process`, `kernel/exec` and `kernel/cred`. Access policy is `kernel/cred`;
memory writes/invalidation are `kernel/mm`/`kernel/mmu`; engine stepping and traps remain `engine`.
Procfs memory-file handling stays `kernel/procfs`, core writing `kernel/signal`.

## Key types

- `ptrace::Trace` — per-thread tracing state: tracer tgid, seized, options, run mode
  (`Cont`/`Syscall`/`Step`), current `Stop` (`Signal`, `Group`, `Syscall(entry)`, `Event`), the saved
  `Regs` copy, siginfo, event message, syscall number, `HwDebug` slots.
- `TraceExit`, `leader_tracer`, `exit_tracer` — tracee exits held back for the tracer.

## Public entry points

- **ptrace**: `ptrace(2)` requests, `process_vm_readv`/`writev`; tracee hooks called from the
  dispatcher, delivery, clone/exec/exit, seccomp and wait.
- Shared `Regs`, pstate helpers and `encode_regset` also serve signal frames and core dumps.

## Data flow / lifecycle

### ptrace

- Tracees are threads; the tracer is a process (any of its threads may issue requests; tracees are
  released only when the whole tracer process exits). No Yama restrictions. Tracing pid 1 is
  `ESRCH`; `PTRACE_TRACEME` under init is `EPERM`. Access needs the same user (and dumpable, with a
  capability subset) or `CAP_SYS_PTRACE` against the target thread's own credentials. The
  credentials of whoever asked for the relationship are kept as the tracer's (Linux's
  `ptracer_cred` through `ptrace_link`): the attaching thread's for `ATTACH`/`SEIZE`, the tracee's
  own for `TRACEME` (since the fix for CVE-2019-13272, so an unprivileged child of a privileged
  parent gains nothing from a setuid program), inherited by auto-attached children; they are
  judged later with `CAP_SYS_PTRACE` over the relevant user namespace.
- Attaching: `ATTACH` sends `SIGSTOP`; `SEIZE` takes options and never stops. Requests other than
  `KILL` and `INTERRUPT` need the tracee in a ptrace stop (else `ESRCH`); unknown requests are `EIO`.
- Every ptrace stop saves a copy of the registers; the tracer reads and writes the copy and it is
  written back on resume. Stopping notifies the tracer (`CLD_STOPPED` or `CLD_TRAPPED`, `SIGCHLD`
  unless suppressed) and waits until resumed, detached or killed.
- Signal-delivery stops: resuming with 0 suppresses the signal, another number replaces it (as
  `SI_USER` unless `SETSIGINFO` was used); blocked replacements are re-queued.
- Group stops: non-seized tracees report `Group` (no siginfo); seized ones report `EVENT_STOP`;
  `LISTEN` and `INTERRUPT` need `SEIZE`.
- Syscall stops (`PTRACE_SYSCALL`): x7 is 0 at entry and 1 at exit (restored afterwards); the
  tracer may change the number through `NT_ARM_SYSTEM_CALL` (-1 skips the call); seccomp runs after
  the entry stop; the no-BKL fast path is off while tracing syscalls or stepping. Detaching (or the
  tracer exiting) at an entry or seccomp stop keeps the syscall number, so the call goes on as
  made.
- Event stops: fork, vfork, clone, exec (message: former tid), exit (message: wait status),
  vfork-done, seccomp. `CLONE_PTRACE` forces and `CLONE_UNTRACED` prevents tracing of children.
  A non-seized tracee exec without `TRACEEXEC` gets `SIGTRAP`.
- Single-step: the interpreter executes one instruction (`Cpu::step`) and the tracee reports
  `SIGTRAP`/`TRAP_TRACE`; a stepped `svc` reports after the syscall.
- Register sets: `NT_PRSTATUS`, `NT_PRFPREG`, `NT_ARM_TLS`, `NT_ARM_SYSTEM_CALL`,
  `NT_ARM_HW_BREAK` (6 slots), `NT_ARM_HW_WATCH` (4 slots); nothing for SVE, PAC, MTE, SME. Debug
  slots are handed to the engine (`set_hw_debug`) when the tracee resumes: breakpoints force Tier
  0, watchpoints keep Tier 1 with watched pages out of the TLB. As on arm64 Linux, whose
  `ptrace_disable` leaves them, detaching and the tracer's exit keep the slots (still applied if
  set during the last stop) and a later attach finds them; only execve clears them.
- Memory: `PEEK*`/`POKE*` on a tracee that is not dumpable are `EIO` unless the tracer had
  `CAP_SYS_PTRACE` over the tracee's user namespace when it attached (`ptrace_access_vm`).
  `POKETEXT`/`POKEDATA` and `/proc/<pid>/mem` writes use forced writes (private copies of
  non-writable mappings, translation invalidation; written mappings stop being trusted translation
  sources). `process_vm_writev` uses plain writes.
- wait: the tracer finds its tracees without `WUNTRACED`; a stop reports `code << 8 | 0x7f`. A
  traced leader's zombie is hidden from its parent until the tracer reaps it. When the tracer
  process exits, tracees are killed with `EXITKILL`, otherwise detached; detaching a tracee in a
  stopped group returns it to the group stop.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/signal` offers group/signal-delivery stops, handler entry and fatal exits.
- `kernel/process` calls syscall, stepping, clone, wait and exit hooks; `kernel/exec` triggers
  exec handling, and `kernel/cred` supplies seccomp stops and checks tracer capability at exec.
- `kernel/procfs` exposes tracer state; `kernel/signal` consumes register helpers for frames/cores.

### Outbound

- `kernel/signal` supplies siginfo, signals and delivery on resume; `kernel/process` supplies
  task/group tables, wakes and waits. `kernel/ns` translates visible IDs.
- `kernel/cred` checks access; `kernel/mm` performs reads/plain and forced writes, with
  `kernel/mmu` code invalidation; `engine` provides Cpu state, stepping and hardware-debug slots.
- `kernel/vdso` identifies clock stubs that bypass ptrace stops.

## Invariants and gotchas

- The tracer works on the saved register copy; changes made directly to the CPU state during a
  stop would be overwritten on resume.
- Only threads calling `exit`/`exit_group` or taking the fatal signal report
  `PTRACE_EVENT_EXIT`.

- Signal-delivery and fatal-exit ordering crosses `kernel/signal`; changing stops requires
  reading both maps. Immutable signal actions bypass signal-delivery stops.

## Confirmed bugs / technical debt

- **Single-step over some syscalls reports one instruction late:** after `rt_sigreturn`, execve or a
  skipped syscall (number -1), the step is not reported at syscall return.
- **ptrace reads are not forced:** `PTRACE_PEEK*` and `/proc/<pid>/mem` reads fail on `PROT_NONE`
  pages Linux can read; `PTRACE_PEEKSIGINFO` is unsupported (`EIO`).
- **Threads ended by another thread report no `PTRACE_EVENT_EXIT`:** only threads calling
  `exit`/`exit_group` or taking the fatal signal stop there, so the other threads of an
  `exit_group`, and the leader killed by a non-leader's execve, exit without the stop Linux 6.1
  gives them (there the execve waits for it); a tracer that collects state at exit misses them.
- **Detaching at an attach-time group stop reports the stop again:** after `PTRACE_ATTACH` to a
  stopped process and a detach during its group-stop report, the thread takes the `SIGSTOP` that
  ATTACH queued, so the parent gets a second `CLD_STOPPED`/`SIGCHLD` and waitpid stop report,
  where Linux 6.1 re-enters the stop silently; a shell whose stopped job was briefly attached by
  gdb or strace sees a duplicate stop.
- `process_vm_readv`/`writev` with a non-empty local vector but no remote bytes look up the pid and
  check access (`ESRCH`, `EPERM`) where Linux 6.1 returns 0 first.

## Open questions

None.

## To verify

- Linux 6.1 comparison claims (FOLL_FORCE reads, `PTRACE_MODE_FSCREDS`, exit/group-stop
  ordering and process_vm zero-length behavior) were inherited from knowledge-based comparisons,
  not a Linux source checkout.
