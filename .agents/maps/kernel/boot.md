# kernel/boot Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/boot.md` for the paired decision history (not yet created; decisions from before the split are D9, D35, D53, the host-stop part of D30 and the host-spawn part of D43 in `.agents/decisions/kernel/process.md`; the process reaping contract remains D16 there).
>
> Verified: 2026-11-14 (split out of the kernel/process full mapping of 2026-11-10; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: repoint dependencies to the split module maps)

## Responsibilities

`kernel/boot` owns the host-driven kernel lifecycle: starting a machine and its built-in PID 1,
spawning host-started programs, running inittab's boot and shutdown plans, handling power
requests and tearing the machine down.

Root: `core/kernel/src/inittab.rs`; in `machine.rs`, `MachineOptions`, `Stdio`, the
`Machine` owner and `SHUTDOWN_TIMEOUT`, `Machine::start`/`start_kernel`, `boot`,
`spawn`/`spawn_process`, `shutdown`, `inittab`, `run_init_process`, `kill_all`, `take_power`,
`Drop for Machine` and `run`; and `reboot` in `syscall/misc.rs`.
The `INIT` constant in `kernel.rs` belongs here. The boot/power fields (`init_handlers`,
`power`, `stopping`, `shutting_down`) are shared state on `kernel/process`'s `Kernel`,
whose process tables also remain there.

Outside the module: task creation and execution, general exit/reaping (including orphan
adoption), the BKL, the watchdog, host wait helpers, process statistics and the remaining
`Machine` methods (`kernel/process`); the timer thread's work (`kernel/time`); the ELF loader
(`kernel/exec`); signal syscall dispatch and permission checks (`kernel/signal`); guest pid
namespace creation (`kernel/ns`); console and terminal bridge behaviour (`kernel/tty`).

## Key types

- `Machine` owns one `Shared` kernel and the timer-thread and watchdog handles; dropping it
  ends the guest processes and joins/stops those workers.
- `MachineOptions` supplies the tier, store, memory limit, low-port policy and initial exception
  tracing; `Stdio` chooses host stdio or a console for a spawned process.
- `inittab::Entry` describes an action and its runlevels; `Power` is reboot, halt or poweroff.
- The virtual init is the `INIT` thread group, with no guest thread. `Kernel::init_handlers`
  enables its power requests; `power` holds the first pending request; `stopping` interrupts
  boot and `shutting_down` ends the timer loop.
- `SHUTDOWN_TIMEOUT` is the host run wrapper's 30 s budget for inittab shutdown commands.

## Public entry points

- `Machine::start`, `spawn`, `boot`, `shutdown`, `kill_all`, `take_power`, `Drop for Machine`.
- `run` is the CLI's kernel entry point; it uses `RunOptions`, returns `RunReport`/`RunError`
  and calls the console and wait APIs owned by `kernel/tty` and `kernel/process`.
- `inittab::parse`, `boot_plan`, `shutdown_plan`, `program`, `script`.
- `reboot(2)` validates its command; the usable power requests arrive through signals to PID 1.

## Data flow / lifecycle

### Machine start and host spawn

- `Machine::start` creates PID 1, the timer thread and the BKL watchdog; a contained panic is
  an internal error. Options configure the store, memory and low-port policy, with
  `debug.exception-trace` set before PID 1 is created when the host requests it.
- The Android host enables exception tracing; the CLI wrapper keeps the arm64 default off.
- Host-launched processes bypass `RLIMIT_NPROC` and `threads-max` by design; guest clone checks
  remain in `kernel/process`. Loading uses `kernel/exec`'s `loader::load`, and actual task and
  host-thread creation uses `kernel/process`'s `task::start`.

### PID 1, boot and shutdown

- The virtual init is a thread group with no threads and no guest program. Host-started processes
  (each terminal tab's login shell) are its children; it adopts orphans and its children are never
  zombies. `Machine::spawn` registers such a process (in the group table, init's children and the
  watched table) only once its host thread has started; when its task or thread cannot be
  created, it gives the console it attached back (`kernel/tty`) and leaves nothing registered.
- On request (the app always; `tuxport-run --init`) it boots from a subset of `/etc/inittab` (a
  store file; anything else is an empty table): `sysinit` (waited), then `boot` and `bootwait`
  together in file order (only `bootwait` waited), then `wait` (waited) and `once` (not waited) for
  the default runlevel (`initdefault`, else 2). Commands run through `/bin/sh -c` with stdin from
  `/dev/null` and output appended to `/var/log/boot.log` (the first command that runs truncates
  it, and the log is emptied when none ran; `/dev/null` if `/var/log` is missing); entries whose
  absolute program path does not exist are skipped; `respawn` and `askfirst` never run; a shutdown
  stops a boot in progress.
- Shutdown: after a boot, run `shutdown` entries and `wait`/`once` entries that list the requested
  runlevel (0 for halt and poweroff, 6 for reboot) in order, all within one caller-supplied
  deadline (30 s from the hosts; entries not started by then are skipped); then, always, SIGTERM
  everything, wait up to 1 s, and kill the rest. The Android host always requests poweroff.
- From the start of boot, `kill(1, SIGTERM|SIGUSR1|SIGUSR2)` by a sender allowed to signal root is
  a reboot, halt or poweroff request handed to the host (`Machine::take_power`; only the first is
  kept until taken); senders not allowed get `EPERM`; other signals to pid 1 are ignored.
  `pidfd_send_signal` to it checks permission and delivers nothing, broadcasts skip it and
  `rt_sigqueueinfo` to it is `ESRCH`.
- `reboot(2)` never reboots the host: after the capability and magic checks, only the
  Ctrl-Alt-Del switch commands succeed; other recognized commands return `EPERM`.

### CLI run and teardown

- `run` starts a machine, optionally opens a console and boots inittab, spawns the requested
  program and starts the terminal bridge. With `RunOptions::stop`, it forwards the first host
  stop signal to the program and ends every process at one arriving at least a second later,
  then finishes as usual.
- With init enabled, the program exiting or a power request starts shutdown; a reboot request
  ends the run rather than starting another kernel. Without init, the program's exit is followed
  by `kill_all`. The terminal bridge is joined before the report is returned.
- Machine drop kills all remaining tasks, syncs the store, sets `shutting_down` and wakes
  waiters, joins the timer thread, then stops the watchdog last.

## Dependencies (inbound / outbound)

### Inbound

- `android/native` and `android/service`: machine start, per-tab spawn, boot, shutdown and
  power requests; `harness/cli`: `run` and direct Machine integration tests.
- `kernel/signal`: signals to virtual PID 1 request power through the shared boot state.
- `kernel/process`: reparenting falls back to the virtual init and reaps its children at once.

### Outbound

- `kernel/process`: `Kernel`/`Shared`, BKL, task creation and group tables, waited statuses,
  wake channels, `bug::host_call`, watchdog and statistics.
- `kernel/time`: timer worker lifetime; `kernel/exec`: program loading; `kernel/tty`: console
  attachment/detachment and the CLI bridge; `kernel/signal`: shutdown and host-stop signals.
- `kernel/vfs`, `kernel/store`: opening the store, reading inittab, boot log and final sync;
  `kernel/oom` and `kernel/net`: initial limit and port-map options; `engine`: tier availability
  and fault guard; `hal`: host fd limit, events and stop-pipe polling.

## Invariants and gotchas

- PID 1 has no guest program. General reparenting and wait semantics remain in `kernel/process`;
  a guest pid namespace's init is a separate process, not this virtual init.
- Host spawn registers its group, init child and watched slot only after `task::start`
  succeeds. The guest thread cannot run until the BKL is released; returned creation errors
  detach the console and leave no registrations.
- The timer thread and watchdog use the parent's BKL/containment contracts; boot does not
  duplicate their policies. Watchdog stop remains last in machine drop.
- Inittab reads hold the BKL, including during shutdown when guest processes may still run.

## Open questions

- Is it intended that Android always shuts down as poweroff (runlevel 6 entries never run)?
