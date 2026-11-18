# Processes and threads (kernel/process) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/process"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b). D1–D13 were seeded by hand from the retired design
> doc §6 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Constraint index (compressed active decisions)

> Full text of D1–D5, D7–D10, D12, D15–D17, D20–D22, D24–D26: `git show 248d79f637c8f05dadc6ef9fdb0733c568c6a468:.claude/decisions/kernel/process.md`

> Full text of D27–D32: `git show 260512d530d136e70f10564e248f7db876f07062:.claude/decisions/kernel/process.md`

> Full text of D33–D37, D39–D46 and D48: `git show 0553229be58764ca5a42f2ea64949373e99d9553:.claude/decisions/kernel/process.md`

- **D1** — Each guest thread is a host pthread scheduled by the host and pooled with its engine CPU state, so every guest thread costs a host thread and an eventfd, and a pooled host thread keeps the previous guest thread's nice, affinity and I/O priority until they are re-applied (rounds 04, 28)
- **D2** — All kernel-layer state sits behind one big kernel lock that guest code runs without and only waits and `Cx::unlocked` release mid-syscall, so kernel objects use non-atomic `Rc`/`RefCell` and nothing may hold them across a release (rounds 04, 29, 36, 38, 109, 189)
- **D3** — Up to half the host CPUs (1 to 4) spin about 30 µs on the BKL before sleeping, reading only its "held" flag and never touching the mutex until the flag reads free (round 188)
- **D4** — Every guest wait is one host `poll()` on the thread's own eventfd plus the host fds it waits on, never a blocking host syscall, and since events are level-triggered waiters must re-check their condition in a loop (design review §6, round 04)
- **D5** — Wakers request channels or objects and only matching waiters are signalled, after the BKL is released, so every file kind must register each object that affects its readiness or its waiters hang (rounds 16, 28, 49)
- **D7** — execve resolves and checks under the BKL but maps segments and builds the stack unlocked, touching only the new address space and self-locked structures (file maps) and re-checking after re-locking whether it was killed (round 28)
- **D8** — `threads-max` (by memory, at most half the host fd limit) binds root too and `RLIMIT_NPROC` defaults to half of it, so a fork bomb cannot exhaust the app's threads and fds, while host-launched processes bypass both by design (round 20)
- **D9** — PID 1 is a built-in init with no guest program that adopts orphans, runs a subset of `/etc/inittab` (no `respawn` or `askfirst`) at boot and shutdown on its own host thread so it never blocks the tabs, and takes root's SIGTERM/SIGUSR1/SIGUSR2 as reboot/halt/poweroff (design review §6, rounds 186, 187)
- **D10** — nice, affinity and I/O priority are per thread and applied to host threads, with raised priority stopping at host nice -4, real-time I/O becoming best-effort and real-time policies only recorded, and they must be re-applied whenever a pooled host thread starts a guest thread (rounds 19, 140)
- **D12** — Kernel pids and credential IDs stay global and pid and user namespaces translate them at every boundary (syscall arguments and results, procfs, siginfo, socket credentials), where a missed translation leaks a global ID into a namespace (rounds 116–123)
- **D15** — Every reader of guest times (`read_timespec`, the fast path, `timer_settime`, `read_itimerval`) goes through `timespec()` in `syscall/mod.rs`, which validates and saturates at KTIME_MAX, and a new reader that bypasses it brings back the `Instant + Duration` overflow panic (96cd8bd, 63b9e59)
- **D16** — A namespace's pid 1 becomes the new parent only while the namespace is not dead, so a dying namespace's orphans fall to the built-in init, which reaps them at once, though a subreaper inside the namespace still takes them first (65c44f6)
- **D17** — execve copies the fd table only above two references (another process sharing it through `CLONE_FILES`), as `unshare_files` does, relying on exiting threads dropping their references under the BKL before `kill_other_threads` returns (68cafce)
- **D20** — A thread leaving a live process hands every shared pending signal to `signal::retarget` once it has left its group, as `exit_signals` does (kernel/signal D16) (22e2b41)
- **D21** — `ThreadGroup::leader_exit` keeps an exited leader's name and CPU time from its exit until reaping, cleared when another thread's execve takes over the tgid, so procfs can show it as a zombie (kernel/procfs D17) (79a60cf)
- **D22** — `RLIMIT_MSGQUEUE` defaults to (819200, 819200) as on Linux, though mq_open does not enforce it (97b1d62)
- **D24** — With `CLONE_PARENT` the child takes the creating thread group's exit signal, while ptrace still picks the fork or clone event from the requested signal, as on Linux (14dede6)
- **D25** — A thread-targeted POSIX timer signal is sent only while its task still exists in the timer's process and is otherwise dropped, though a thread of the same process reusing that number still receives it (d98f60b)
- **D26** — Marking a robust lock owner-died translates its address with copy-on-write broken (or the page faulted in) before the compare-and-swap loop, since a plain translate does not break copy-on-write (kernel/mm's rule for writers through translate) (e4d3386)
- **D27** — `set_policy` follows `__sched_setscheduler`'s checks (reset-on-fork only with a kept policy, clamps against the current ones, `KEEP_PARAMS` validating only, nice only for `SCHED_OTHER` and `SCHED_BATCH`, `EPERM` beyond `RLIMIT_NICE` or for clearing reset-on-fork without `CAP_SYS_NICE`) and `KCMP_IO` compares I/O priority contexts, though the policy is still applied before the nice check (70c1bc6, dcacfb6)
- **D28** — `Kernel::ucounts` charges a new namespace to (namespace, euid) and each (parent, owner) pair up to the initial namespace, and clone, unshare and open_tree return `ENOSPC` at a `max_*_namespaces` limit before creating anything, so a new namespace kind must be checked and counted where it is created, and dead namespaces leave the counts only at the next check (6d05aa9)
- **D29** — When the first stage of `task::exit` panics, a second `bug::catch` still completes `vfork_done` and calls `remove_user` before the kill, while the rest of the stage is skipped and a salvage over borrowed state is only reported (6d1d9c0)
- **D30** — wait applies one eligibility rule (`__WCLONE` and `__WALL` by exit signal, zombies only for waits taking exits), #! nests five deep, tids wrap to 300 below `PID_MAX`, `tuxport-run` forwards its first stop signal and kills everything on one at least a second later, and the periodic checks stop through `Shared::stop_checks` with SeqCst on both sides (5d23297)
- **D31** — `loader.rs` reads a host file's `st_dev` and `st_ino` without a cast now that only Linux and Android hosts are built, so a 32-bit or other-libc target would only fail to compile (7c5e636)
- **D32** — Private memory and `FUTEX_PRIVATE_FLAG` operations key futexes by (mm, address) and shared memory by host memory or file, and `uid_map`/`gid_map` writes are checked against their opener as well as the writer, so the no-BKL path and `Mm::futex_key` must agree on which addresses are private and every writable shared mapping must call `note_shared_maps` (f6a82cc)
- **D33** — `FUTEX_WAKE_OP` runs its op on `futex::writable_word` with Linux's comparisons and futex argument rules (left by the fast path to the BKL path), `/proc/stat`'s `processes` counts every task, `PRIO_USER` translates through the caller's user namespace and matches each thread by its own real uid, and `reap_shm` runs at every thread exit and execve, cheap only while no segment is marked for removal (5d7b936, 1be1142)
- **D34** — setpriority and sched_setaffinity accept the same owner (by the target thread's credentials) or `CAP_SYS_NICE` over the target's user namespace, while sched_setscheduler and sched_setattr still require it in the initial namespace, so `syscall/sched.rs`'s two owner checks must stay matched to their syscalls (2aca260, 1be1142)
- **D35** — `MachineOptions::exception_trace` records `debug.exception-trace` before PID 1 exists, the app setting 1 and `tuxport-run` keeping arm64's default 0, so a new host must choose the value explicitly (7b7f73b)
- **D36** — `Task::vfork` mirrors `vfork_done` for the OOM killer on the timer thread, so every path completing `vfork_done` must clear it too; syscall 448 dispatches to process_mrelease and `FlockHolder` records the taker's tgid (39ea702)
- **D37** — prctl's guarded options given refused arguments return `EINVAL` from an arm of their own so that the prctl and futex default arms count only what is missing (options newer than 6.1 included), and a new guarded prctl arm must join that arm (99b4216)
- **D39** — `Kernel::unsharing` makes files whose shared store object is being copied with the BKL released wait per inode on `wake::LOCK`, and `ExeCache::forget` drops an inode whose data moved to another object (df6ee70)
- **D40** — With nonzero clock offsets, clone, execve and setns put the vDSO's svc stubs back through `Mm::vdso_syscalls`, never undone (an address space shared with a process back in the boot namespace keeps them), the loader maps `[vvar]` below the vDSO, and any new way to enter a time namespace must patch too (16364c0)
- **D41** — execve checks a script for writers (`ETXTBSY`) before looking at its interpreter, though the ELF interpreter and nested scripts are still unchecked, and tells the credential transition when another process shares the fs context, so set-id bits and file capabilities gain nothing then (c3e2f57, 6d9a9e8)
- **D42** — `dispatch()` reruns a syscall on `FUSE_RETRY` (a fresh token per syscall, at most 16384 times, then `EIO`, seen once by ptrace and seccomp) after reading the recorded page or serving the walk queries, so a side effect before a handler's last walk repeats and a handler mapping memory errors to `EFAULT` itself loses the retry (c008749, 3957a03)
- **D43** — The ELF loader follows 6.1's header, `PT_INTERP` and `AT_PHDR` rules, maps segments in order over each other (a shared page takes the later segment's permissions), loads ET_EXEC interpreters at their own addresses and turns late failures into an `Image::doomed` killed with SIGSEGV after the point of no return, while `Machine::spawn` still starts a doomed image at pc 0 (ef68770, 6d9a9e8)
- **D44** — PI futexes keep `PiWaiter`s in the futex queue and the word as the owner record, following `futex_lock_pi_atomic` without priority inheritance or ordering, with the requeue-PI commands `ENOSYS`, no hand-over at execve, and `pi_owners_exited` giving a gone owner's locks to their first waiter with `FUTEX_OWNER_DIED` (edd7f95)
- **D45** — `InSyscall` stamps each BKL syscall's global number on `Task::syscall` from after the entry stop and seccomp until the end-of-syscall sweeps, cleared on every way out of dispatch before stops and signal delivery, so anything waiting inside dispatch after the sweeps must come after the guard is dropped or it holds back the overlay sweep (0d82807)
- **D46** — `ExeFile::ovl` keeps the (mount, node, `Hold`) of a program run through an overlay, so `open_exe_file` can run a removed one from its store inode with the overlay mount's noexec and nosuid, until the overlay instance is gone (`ENOENT`) (9590431)
- **D48** — Each thread carries its own credentials: clone sets the creating thread's, execve sets both of the main thread's copies, and the scheduling, ioprio, `PRIO_USER` and `RLIMIT_NPROC` checks use the target thread's, so code writing `Task.cred` or `ThreadGroup.cred` directly leaves the two apart (1be1142)

## Active (full detail)

### D6: A few hot syscalls run without the BKL
- **What**: stat, clock reads, futex wait/wake, nanosleep and sched_yield (D49) have a lock-free fast path that falls back to the BKL path when it cannot finish.
- **Why**: Parallel `lstat` (git's default) and runtime thread pools were serialised by the BKL, making `git status` slower than single-threaded, and spinning threads' sched_yield took the BKL only to return 0.
- **Tradeoffs**: Per-thread caches must be invalidated by a global epoch whenever root, cwd, credentials, mounts or namespaces change.
- **Watch out**: The fast path must stay "in guest" while touching guest memory so shootdowns wait for it, and a call that stops counting as running guest code (sched_yield) must not touch guest memory afterwards.
- **Source**: rounds 25 (`25-stat-without-bkl`), 38 (`38-split-bkl`); 78b3284

### D11: A dedicated timer thread delivers timers and runs periodic checks
- **What**: One host thread in `Machine` sleeps until the earliest deadline and handles interval and POSIX timers, CPU limits, the memory limit, load average, DNS stub answers, eventfd-signalling AIO polls (waiting on their host fds and timerfd deadlines, and off after a kernel-layer panic; kernel/vfs D42), the next expiry of timerfds behind `EPOLLEXCLUSIVE` gates (kernel/poll D4) and other periodic work.
- **Why**: Timer expiry, limits and AIO polls that signal an eventfd (which must complete while no thread is in `io_getevents`) cannot rely on guest threads, and the memory limit can only be checked periodically because the host kernel handles guest page faults directly; when every thread waits, memory cannot grow, so the periodic checks stop.
- **Tradeoffs**: Its work runs under the BKL and competes with guest syscalls; a finite `RLIMIT_CPU` keeps a 10 ms tick; while such AIO polls are pending it listens on every wake channel, so every kernel change wakes it.
- **Watch out**: Anything that changes a deadline or hands it work (the DNS threads, new AIO polls, exclusive gates) must signal its event, and programs keeping many eventfd-signalling polls pending load it.
- **Source**: rounds 19 (`19-deferred`), 20 (`20-robustness`); fe4bb2b, fffe1c1, f5c09f7

### D13: Time namespaces follow Linux 6.1 plus the 6.2 relaxations
- **What**: Monotonic and boottime offsets apply to clock reads and absolute waits; threads and vfork children enter at execve; entering one with offsets (at fork, execve or setns) puts svc stubs over the host vDSO's clock entries so clock reads reach the kernel layer, which adds the offsets (D40).
- **Why**: dockerd adds a time namespace when `/proc/<pid>/ns/time` exists and runc then requires `timens_offsets`; since all clocks (including the vDSO stubs) then passed through the kernel layer, offsets were cheap, and it was the last namespace type missing.
- **Tradeoffs**: Diverges slightly from the 6.1 contract where 6.2 relaxed it, and processes in a namespace with offsets read clocks through svc stubs, slower than the host's vDSO.
- **Watch out**: The fast path caches the offsets and must be invalidated on namespace entry, and a new way to enter a time namespace must patch the vDSO too.
- **Source**: round 162 (`162-time-namespaces`); 16364c0

### D14: kernel/process owns task execution, dispatch, the BKL, waits and accounting
- **What**: kernel/process keeps tasks, dispatch, the BKL and watchdog, fast paths, waits, accounting and diagnostics; time and periodic checks belong to kernel/time, machine lifecycle and PID 1 to kernel/boot, with the existing exec, namespace, futex and credential boundaries retained.
- **Why**: The process map still mixed task execution with independently maintained timers and boot policy after its earlier splits.
- **Tradeoffs**: Shared-file boundaries remain explicit in the registry, cross-boundary changes require several maps, and earlier decisions keep their parent tags and IDs; vDSO construction belongs to kernel/vdso while clone and fast-dispatch integration remain here.
- **Watch out**: Preserve fast_sleep deadline handover, BKL charging and vfork freezer accounting; sched_setattr still changes policy before validating nice and BKL-release wake-up matching still runs outside bug::catch, while the exited-leader CPU-clock debt is inherited by kernel/time.
- **Source**: dd6c288, d2b1dc3, b320f8e (bug list since updated by 1459292, 4e64e74, ce8a9ce and d9e1f0c, and shortened by D29, D30, D32, D43, D44, D52, D53 and D54); cf029f9a, 8a17e4a9; d053be24; 60790c14; 39e55b4b

### D18: Process files read in the order their code runs
- **What**: kernel.rs runs global state, the BKL and the syscall context before waiting and waking; syscall/proc.rs follows the module docs from clone and execve through exit and reaping to pidfds, unshare and sessions; `impl Machine` follows a machine's life; syscall/mod.rs puts dispatch first and the shared argument helpers last; the other files move single helpers after their callers, bug.rs and debug.rs included (the registry now puts debug.rs in kernel/signal).
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; kernel.rs opened with the wake-request helpers and syscall/proc.rs with exit.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; `store_changed` sits at the end of machine.rs, away from `RUNNING`, which describes it.
- **Watch out**: Code appended at the end of a file drifts from this order.
- **Source**: 0758e3c; d2b1dc3 (registry)

### D19: Wall-clock deadlines stay on their own clocks
- **What**: `itimer::Expiry` keeps a deadline as a monotonic instant or as a time on its host clock, mapping clocks as Linux's `__hrtimer_init` does (absolute realtime and TAI on the realtime clock, boottime on the boottime clock, relative realtime on monotonic); waits re-derive their instant on every pass, and the timer thread watches the host for clock jumps (hal D11) and wakes every task when one happens, for timerfds (kernel/vfs D23) and message queues (kernel/ipc D3) too.
- **Why**: Absolute realtime and boottime deadlines were turned into monotonic instants when set, so they fired late after the device slept or the wall clock was stepped.
- **Tradeoffs**: A clock jump wakes every thread once, and alarm clocks still cannot wake a sleeping device.
- **Watch out**: An instant cached across a wait comes back late, and without the host watch (non-Linux hosts, a seccomp policy refusing timerfds) jumps go unnoticed until something else wakes the waiter.
- **Source**: 63b9e59

### D23: sched_setattr accepts the keep and clamp flags and short structures
- **What**: The keep flags are resolved before validation in `sys_sched_setattr`'s order, utilization clamps are recorded on the task and inherited like Linux's `uclamp_req`, and a structure shorter than the current version is read into a zeroed full-size buffer.
- **Why**: Linux 6.1 accepts these flags (with uclamp built in, as on Android) and refusing them broke tools that only meant to set a hint; the first fix then read past the 48-byte first version and killed the caller.
- **Tradeoffs**: The clamps change nothing on the host, and clamp flags with the short structure are still `EINVAL`.
- **Watch out**: Other syscalls taking versioned structures may read past a short copy the same way (LTP's clone3, openat2 and mount_setattr tests would show it).
- **Source**: bc44b65, e43c40c

### D38: A watchdog thread reports long BKL holds without waiting for the BKL
- **What**: Every BKL acquisition records its holder (count, host tid, guest tid, tgid, comm, `Doing` and pc) in the cache-line `Hold`; a `bkl-watchdog` thread samples it every fifth of the threshold (10 s by default, `TUXPORT_BKL_WATCHDOG` in seconds, 0 off), reports a hold unchanged that long on stderr and in `Shared::stall` (which `Machine::stall` reads without the BKL), and at the hold's end spawns a `bkl-watchdog-log` thread that writes the KERN_ERR record under the BKL; nothing is killed.
- **Why**: A hold that never ended (a loop in kernel code, a host call blocking with the lock held) froze everything without saying where, and the first version's `ended` took the BKL itself to log, blocking the only watcher behind the next holder.
- **Tradeoffs**: Each acquisition pays a few relaxed stores and each entry a 40-byte thread-local write, durations are right only to one look (2 s by default), a thread releasing and retaking the lock in a loop is not caught, and the kernel-log record may arrive well after the stall or be lost if its thread cannot start.
- **Watch out**: Anything added to `Hold` must stay on its cache line as a plain store, a logger waiting for a BKL that is never released stays blocked and may outlive its `Machine`, and a hold that never ends still blocks `Machine`'s drop.
- **Source**: 64a6107, d20fd3c (the blocking `ended` recorded in d9e1f0c)

### D49: sched_yield runs without the BKL and yields the host thread
- **What**: The fast path (D6) handles sched_yield by clearing `in_guest` and calling `std::thread::yield_now`, and when the fast path is off (tracing, a seccomp filter that does not always allow it) the BKL path yields inside `Cx::unlocked`.
- **Why**: sched_yield took the BKL just to return 0, so spinning threads (parking_lot and crossbeam backoff, Go's osyield, HotSpot's GC termination) slowed every other thread's syscalls up to 8.6 times and kept their CPU, and eight threads passing a turn ran 230 times slower than natively.
- **Tradeoffs**: A call costs 296 ns instead of 173 (a host syscall), a thread yielding on the BKL path reads as sleeping in /proc, and real workloads gain nothing measurable.
- **Watch out**: A fast-path call that stops counting as running guest code must not touch guest memory afterwards, and the host scheduler (CFS or Android's EAS) decides how long a yielder waits.
- **Source**: 78b3284

### D50: A parent waiting for its vfork child counts as frozen
- **What**: The vfork wait marks the parent frozen before waiting and afterwards (or after an error) clears the mark unless its thread group is freezing, leaving the signal path's freezer stop to clear it after thaw, as Linux 6.1's `wait_for_vfork_done` does.
- **Why**: Linux enters and leaves the frozen state around the vfork wait, and clearing the mark early would let `cgroup.events` flicker to unfrozen before the parent stops for a freeze still in progress.
- **Tradeoffs**: A parent woken while freezing stays counted while it runs on to its freezer stop, including through a ptrace `VFORK_DONE` stop.
- **Watch out**: A path back to guest code that skips the freezer check would leave the mark set.
- **Source**: b593172

### D51: execve clears `READ_IMPLIES_EXEC`, as arm64 does for every 64-bit program
- **What**: execve's point of no return clears `READ_IMPLIES_EXEC` and then, when privileged, the `PER_CLEAR_ON_SETID` flags, and brk reads the flag before borrowing the address space (kernel/mm D43).
- **Why**: personality(2)'s `READ_IMPLIES_EXEC` now takes effect, and natively a plain execve also cleared it, since arm64's `SET_PERSONALITY` drops it for every 64-bit program, which the first version kept.
- **Tradeoffs**: `setarch -X program` has no effect on the program, as on arm64 Linux.
- **Watch out**: None known.
- **Source**: 5760479

### D52: A fast-path wait woken early hands its deadline to the BKL handler for one dispatch
- **What**: When a signal wakes a no-BKL timed wait (futex relative and absolute, nanosleep, clock_nanosleep relative and absolute), the fast path records its deadline in `TaskState::fast_sleep`, the futex, nanosleep and clock_nanosleep handlers take it before checking or reading the time argument, and the run loop's `Exit::Svc` arm clears it after `dispatch` whatever the dispatch did.
- **Why**: The handler redid the wait from the syscall's arguments, so time memory unmapped meanwhile turned EINTR into EFAULT where Linux's restart never rereads it, and a deadline whose handler did not run or failed first (the call skipped by seccomp or a tracer) ended the thread's next BKL-path wait early.
- **Tradeoffs**: One store per BKL-path syscall, and a handed-over deadline is trusted as validated by the fast path, so a tracer rewriting the arguments at the entry stop cannot change a wait already begun (as on Linux).
- **Watch out**: A new no-BKL wait that records no deadline, or a handler that reads its time before taking `fast_sleep`, brings the EFAULT back, and a hand-over meant to outlive one dispatch (through restart_syscall, say) needs a field of its own.
- **Source**: 9593bae, 27609fe

### D53: A host spawn registers its process only once its thread has started
- **What**: `Machine::spawn` creates the task and starts its host thread before inserting the thread group, init's child and the watched slot, and a failure of `Task::new` or `task::start` detaches the console it attached (kernel/tty D21) and returns the error; test hook 13 arms `Trap::ThreadStart`, which `task::start` checks through `bug::fails` and answers with EAGAIN.
- **Why**: A spawn whose task or thread could not be created left a process without threads registered with init and the watched table, never to finish, and its console EBUSY for later spawns.
- **Tradeoffs**: A panic inside spawn after the console was attached still leaves its session set (`host_call` turns it into an error), and hook 13 fails whichever thread start comes next, a guest clone included.
- **Watch out**: Code expecting the group in `k.groups` before `task::start` returns would break (none today, since the thread runs only after the BKL is released).
- **Source**: 143bb86

### D54: CPU-clock timers name their target by serial and are gone with it
- **What**: `Clock::Process { tgid, serial }` and `Clock::Thread { tid, serial }` carry `Task::serial` from a global counter, so a timer whose thread left the task table or whose process was reaped is gone (disarmed at the timer thread's next look, zeros from timer_gettime, `ESRCH` from timer_settime, still deletable), a zombie process's timers read its stopped clock without firing or ticking, and CPU clock ids are decoded through `Cx::pid_in` (the fast path compares its own-clock ids with `Fast::ns_tid`).
- **Why**: Timers named their target by number only, so they outlived it (ticking the timer thread every 10 ms) and followed a reused number, and clock ids built inside a pid namespace were decoded as global pids.
- **Tradeoffs**: An exited main thread's clock is gone at once where Linux keeps the zombie leader's until reap, a timer on the main thread's clock made before a non-leader's execve is gone afterwards, and every task is eight bytes larger.
- **Watch out**: Code making a `Clock` must take the serial of the same table entry it checked, and a new CPU clock decoder must translate the pid as the three existing ones do.
- **Source**: 82433e6

### D55: Each thread publishes its last kernel entry for `/proc/<pid>/syscall`
- **What**: `KernelEntry` on `TaskSync` holds the syscall number (`u64::MAX` for an exit that is not an svc), x0 to x5, sp and pc in atomics with an `inside` flag, recorded by the run loop right after every exit from guest code, before the no-BKL fast path, and cleared when the thread is marked back in guest code (storing only when the flag is set); procfs reads it (kernel/procfs D40) and signal delivery forgets the syscall (kernel/signal D27).
- **Why**: Nothing outside a thread could tell which syscall it was blocked in, which a dump of a hung test needs (harness D39), and `Task::state` is a `RefCell` the running thread owns, entered without the BKL on the fast path.
- **Tradeoffs**: Nine relaxed stores and one release store per kernel entry, and with no sequence lock a reader can mix two entries of a thread that leaves and re-enters while it reads (procfs shows such a running thread as `running` anyway).
- **Watch out**: A new path out of guest code that skips the record leaves a stale entry visible until the next exit.
- **Source**: f1212d8

### D56: Test hook 14 forces host refusal of TCP filters

- **What**: Test hook 14 toggles `REFUSE_TCP_FILTERS` for the whole kernel, while `host_refuses_tcp_filters` is always false without test hooks.
- **Why**: The retained TCP-filter path needs deterministic coverage on hosts that accept filters.
- **Tradeoffs**: The hook persists until explicitly disabled.
- **Watch out**: Tests must turn it off before later sockets in the same run.
- **Source**: 52dcc177; 20137e4c; d053be24; 39e55b4b

### D57: Guest CPU samples combine live and exited threads

- **What**: `Machine::cpu_time_ns` saturating-adds exited-thread CPU totals and every live thread’s CPU time under the BKL.
- **Why**: The screen-off policy needs guest activity without counting the UI or host polling.
- **Tradeoffs**: Each sample takes the BKL and visits the live threads.
- **Watch out**: An exit-stage panic can skip CPU credit, so consumers must compare changes rather than assume monotonicity.
- **Source**: 1e5de64e; 20137e4c; d053be24; 39e55b4b

### D47: Watched cgroup2 files are compared where cgroups change
- **What**: `cgroup::notify_changes` runs after each BKL syscall's sweeps, at the end of `task::exit`, before a thread waits in the freezer and in the timer thread after the OOM check, returning immediately unless an observation is pending (kernel/procfs D38).
- **Why**: cgroup2 changes happen in syscalls, thread exits, freezer stops and the OOM killer, and the comparison that wakes pollers and inotify watches must run after them.
- **Tradeoffs**: Start, exit, vfork waits and sibling kills during exec can walk the cgroup ancestor chain while observations exist.
- **Watch out**: Mark membership and freezer changes before a checkpoint so dirty observations reach poll and inotify (kernel/cgroup D2).
- **Source**: affb1e2; 0f5c5132

### D58: Borrow accounting updates its thread-local count in one access

- **What**: The borrow guard updates the count within one TLS access and its destructor may inline.
- **Why**: Profiling found measurable TLS overhead even with no cgroup observers.
- **Tradeoffs**: Only builds that count borrows benefit.
- **Watch out**: The measured saving depends on compiler code generation.
- **Source**: b65799c8

## Superseded

- ~~Deadlines stay plain `Instant`s (D15 in part, 96cd8bd)~~ → replaced by **D19** in 63b9e59 (2026-11-07)
- ~~PI futexes stay `ENOSYS` (D33's tradeoff, 5d7b936)~~ → replaced by **D44** in edd7f95 (2026-11-09)
- ~~bug.rs, debug.rs and syscall/mod.rs are filed in kernel/process though the registry does not list them (D18 in part, 0758e3c)~~ → replaced by **D14** in d2b1dc3 (2026-11-10)
- ~~The futex handler re-reads the timeout of an interrupted wait, and only relative deadlines are handed over (9593bae's tradeoff)~~ → replaced by **D52** in 27609fe (2026-11-10, no SUPERSEDES declared)
- ~~kernel/process also covers execve and the ELF loader, futexes and namespaces (D14, dd6c288, d2b1dc3)~~ → replaced by **D14** in b320f8e (2026-11-11)
- ~~D14 ownership of timers, periodic checks and PID 1~~ → replaced by **D14** in 8a17e4a9 (2026-11-14)
- ~~Comparing observations at each checkpoint under D47~~ → narrowed by **D47 and kernel/cgroup D2** in 0f5c5132 (2026-11-17).
