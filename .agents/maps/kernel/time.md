# kernel/time Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/time.md` for the paired decision history (not yet created; decisions from before the split are D11, D15, D19, D25, D54 and the periodic-check part of D30 in `.agents/decisions/kernel/process.md`; the fast-path and deadline-handover contracts remain D6 and D52 there).
>
> Verified: 2026-11-14 (split out of the kernel/process full mapping of 2026-11-10; the boundary was checked against the source at the split)

## Responsibilities

`kernel/time` supplies guest clock reads, sleeps, interval and POSIX timers, and the timer
thread that schedules periodic kernel work.

Root: `core/kernel/src/itimer.rs`, `syscall/posix_timer.rs`, `loadavg.rs`;
`machine.rs`'s `timers`; `Shared::stop_checks` and `resume_checks` in `kernel.rs`;
`read_timespec`, `timespec` and `KTIME_MAX` in `syscall/mod.rs`; and in `syscall/misc.rs`
`nanosleep`, `clock_nanosleep`, `sleep_rest`, `sleep_until`, `getitimer`, `setitimer`,
`check_itimer`, `read_itimerval`, `itimer_value`, `write_itimerval`, `clock_gettime`,
`clock_getres`, `cpu_clock` and `gettimeofday`.

Outside the module: task and thread-group CPU accounting, rlimits, `times`, `getrusage`,
`prlimit`, `sysinfo`, the BKL, wait registration and the no-BKL fast path (`kernel/process`);
starting and joining the timer thread (`kernel/boot`); time-namespace offsets (`kernel/ns`);
vDSO images, clock-setting policy, `timezone`, restart dispatch and signal delivery
(`kernel/signal`); timerfd and its readiness (`kernel/poll`). The timer thread calls the OOM,
store, network, cgroup and I/O modules; it does not own their policies.

## Key types

- `itimer::ITimer` (`ITIMER_REAL`, a monotonic instant), `CpuTimer`, `PosixTimer`;
  `itimer::Expiry` — a wall-clock deadline on Linux's hrtimer base: `Mono(Instant)` (monotonic,
  absolute times converted; relative realtime; relative times of other clocks but boottime and
  TAI) or `On(host clock, time)` (absolute realtime, boottime and TAI either way; the `_ALARM`
  clocks as realtime and boottime), compared with that clock whenever looked at; the global
  wall-clock jump count (`clock_epoch`, shared by every kernel in the host process);
  `loadavg::LoadAvg` — the periodic load-average samples.
- `Clock::Process { tgid, serial }` and `Clock::Thread { tid, serial }` identify a CPU timer's
  target across pid reuse; `Deadline` distinguishes wall-clock and CPU-time expirations.
- Timer state lives on `ThreadGroup`; `Shared::timers` is the event for new work and
  `checks_idle` coordinates stopping periodic checks with guest threads resuming.

## Public entry points

- Syscalls: `nanosleep`, `clock_nanosleep`, `clock_gettime`, `clock_getres`, `gettimeofday`,
  `setitimer`, `getitimer`, `timer_create`, `timer_settime`, `timer_gettime`, `timer_getoverrun`
  and `timer_delete`.
- Deadline helpers: `Expiry`, `clock_epoch`, `clock_jumped`, `read_timespec`, `timespec`.
- Periodic work: `itimer::expire`, `loadavg::sample`, `proc_loadavg`, `sysinfo_loads`,
  `Shared::stop_checks`/`resume_checks`; `machine::timers` runs until machine teardown.

## Data flow / lifecycle

### Clock reads and sleeps

- Host clock reads use the HAL; monotonic and boottime readings add the caller's time-namespace
  offset (`kernel/ns`). CPU-clock ids name a thread or process in the caller's pid namespace;
  accounting comes from `kernel/process`'s task and group tables.
- The no-BKL path stays in `kernel/process`. An early wake hands its validated `Expiry` to
  the BKL sleep handler through `TaskState::fast_sleep`; the handler takes it before reading
  the time argument, and the run loop clears any unconsumed deadline after that dispatch.
- Each sleep pass turns its `Expiry` into an instant again and rechecks its own clock after
  waking. Relative `CLOCK_BOOTTIME` sleeps count device sleep. An interrupted relative sleep
  writes the remaining time and records `Restart::Sleep`; restart dispatch belongs to
  `kernel/signal` and calls back into the sleep helper.
- Timeouts and timer values read from the guest (every `timespec` reader, `itimerval`) saturate at
  `KTIME_MAX` (about 292 years) as Linux's `timespec64_to_ktime` does, so GNU `sleep infinity`
  waits until interrupted and timers report the saturated time left; a socket timeout of
  `KTIME_MAX` or more means none.

### Timer thread

- One host thread ("timers") loops, holding the BKL while it works, each step but the clock check
  contained (interval timers per process): if its host `ClockWatch` reports a wall-clock jump, run `clock_jumped` (bump the jump count, signal
  every task's event: BKL waiters, futex waiters and no-BKL sleepers alike look at their deadlines
  again); expire interval timers, POSIX timers and CPU-time limits; run the OOM check (only with a
  memory limit or a cgroup `memory.max`), compare watched cgroup files, the translator code-view
  release, load-average sampling, deferred store commits, `O_ASYNC` and `EPOLLEXCLUSIVE` checks,
  AIO polls that signal eventfds (`kernel/aio`), DNS stub answers (`kernel/net`) and raw ICMP taps;
  then wait (BKL released) until the earliest deadline (timerfds of those AIO polls and of
  exclusive gates included), its event or the host fds of those checks and the clock watch. It
  listens on `OTHER` (or all channels while `O_ASYNC` files, exclusive gates or such AIO polls
  exist). A panic in a step turns that feature off for the kernel's life (the memory limit, `O_ASYNC`
  signals, exclusive gates, AIO polls), or kills only the process whose timers it was handling.
- Setting a timer writes the deadline into the thread group and signals the timer thread's event.
  POSIX timers on wall clocks keep an `Expiry`, so absolute realtime and boottime timers fire by
  their own clock; the watch's fd is in the timer thread's poll.
- `ITIMER_REAL` is a monotonic instant (wall-clock jumps leave it); `ITIMER_VIRTUAL`/`PROF` and
  process CPU-time timers use total host thread CPU time and wake a running thread. Armed CPU
  timers whose clock still advances or a finite `RLIMIT_CPU` make it tick every 10 ms. Over the
  soft `RLIMIT_CPU` (whole seconds) sends `SIGXCPU` and raises the soft limit by 1 s; over the hard
  limit `SIGKILL`.
- POSIX timers: the `_ALARM` clocks need `CAP_WAKE_ALARM`, CPU-clock ids of other threads and
  processes are accepted, at most 4096 per process with the lowest free id; without a `sigevent`
  the signal is `SIGALRM` with the id as value; a `SIGEV_THREAD_ID` target must be in the process.
  A CPU clock id (`clock_getcpuclockid`, `pthread_getcpuclockid`) names its pid as the caller's
  pid namespace numbers it, for clock_gettime, clock_getres and clock_nanosleep too.
- A CPU-clock timer keeps to the thread or process it was made for (number and serial, as Linux's
  timer holds a `struct pid`), never one that takes the number over. Once that thread has left
  the task table (an exited main thread included) or that process is reaped, the timer is gone:
  it never fires and is disarmed, timer_gettime reads all zeros (the interval too) and
  timer_settime is `ESRCH`, while timer_delete still works. A process whose threads have all
  exited and that is not reaped yet still reads its stopped clock and can be set, but its timers
  neither fire nor keep the timer thread ticking.
  A timer whose previous signal is still pending counts an overrun instead (`timer_getoverrun`
  reports the count when the last signal was dequeued). A `SIGEV_THREAD_ID` timer signals its
  thread only while that thread lives in the process; after it exits the signal is dropped, as on
  Linux.
- When every thread is waiting, the periodic checks (OOM, code views, load average) stop until a
  thread next enters guest code; armed timers still wake it. Stopping sets `checks_idle` and then
  looks at the threads again (`Shared::stop_checks`), while a waking thread clears its `sleeping`
  flag before reading `checks_idle` (`resume_checks`), all SeqCst, so a thread waking in between
  either wakes the timer thread or is seen running. Changing `RLIMIT_CPU` does not wake the timer
  thread; the new limit takes effect at its next pass.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/process`: syscall dispatch and the fast path, the run loop's `resume_checks`,
  `sysinfo`'s load values; every timed wait using the shared deadline helpers.
- `kernel/boot`: starts the timer thread and joins it after setting `shutting_down`.
- `kernel/exec`: clears the thread group's POSIX timers at execve.
- `kernel/poll`, `kernel/futex`, `kernel/ipc`: deadlines and clock-jump handling;
  `kernel/procfs`: load average; `kernel/oom`: stopping idle periodic checks.
- `kernel/store`, `kernel/net`, `kernel/aio`, `kernel/poll`: wake the timer thread for new work.

### Outbound

- `kernel/process`: BKL, task/group tables and CPU accounting, `block`, wake channels,
  events, panic containment and process kills. Setting `RLIMIT_CPU` remains `prlimit`'s job.
- `kernel/signal`: timer signal delivery and pending-signal overruns; `kernel/ns`: translating
  clock ids and absolute times; `hal`: clocks, `ClockWatch`, events and polling.
- `kernel/oom`: memory limits and written code views; `kernel/cgroup`: change observation;
  `kernel/store`: deferred commits; `kernel/poll`: signal-driven I/O and exclusive gates;
  `kernel/aio`: eventfd-signalling polls; `kernel/net`: DNS replies and raw ICMP taps.

## Invariants and gotchas

- A finite soft `RLIMIT_CPU` alone keeps the timer thread ticking every 10 ms even while idle.
- A deadline is `Instant::now()` plus a guest duration, which panics on overflow: new readers of
  guest times must saturate through the shared `timespec` helper in `syscall/mod.rs`.
- Waiters must derive a new instant from `Expiry` after each wake, rather than cache one across
  clock jumps. The jump count is shared by every kernel in the host process.
- Changing deadlines or handing the timer thread work must wake its event. The idle handshake
  pairs `checks_idle` with the tasks' `sleeping` flags; the run loop must resume checks before
  entering guest code (`kernel/process`).
- CPU-clock timers retain the target's serial, not just its reusable pid. CPU accounting and
  task removal remain `kernel/process`'s responsibility.

## Confirmed bugs / technical debt

- **An exited main thread's CPU clock is gone at once:** clock_gettime on it is `EINVAL`, and a
  timer on it reads zeros and cannot be set (`ESRCH`), while the other threads run on; Linux keeps
  the zombie leader's clock, stopped, until the process is reaped.

## Open questions

None recorded.

## To verify

- The inherited Linux 6.1 comparison for `send_sigqueue` on dead threads was checked from
  kernel knowledge, not against Linux source; the cases found by Starnix's syscall tests
  were also observed on a native 6.8 kernel.
