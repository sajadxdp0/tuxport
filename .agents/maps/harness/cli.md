# harness/cli Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/harness/cli.md` for the paired decision history (not yet created; pre-split decisions D5, D6, D9, D11, D15, D20, D21, D25, D28, D32, D35 and the guest-test portions of D36, D40 and D41 stay in `.agents/decisions/harness.md`).
>
> Verified: 2026-11-15 (split out of the harness full mapping of 2026-11-10; the boundary was checked against the source at the split)

## Responsibilities

`harness/cli` owns `hosts/cli/` and `tests/guest/`: tuxport-run argument handling, host-terminal
setup, result reporting and the cargo integration/guest-test drivers. Kernel run/boot lifecycle
remains `kernel/boot`, console semantics `kernel/tty`, host terminal helpers `hal`;
benchmarks are `harness`, differential testing `harness/diff`.

## Key types

- `RunOptions` (tier, rootfs, terminal, memory limit, low-port offset, always-map-low-ports, init,
  stop pipe) and `Terminal` (input and output fds, the fd whose size is followed, the resize pipe)
  — what `tuxport-run` passes to the kernel's `run`; `RunReport` (status, Tier 1 statistics),
  `RunError` (I/O, rootfs, exec, load, console, Tier 1 unavailable, internal), `ExitStatus`
  (exited, or signalled with signal, pc, fault address and core dump).
- `Interactive` (in `rootfs_programs.rs`) — a child `tuxport-run --pty /bin/sh -i` with a fixed
  prompt and `expect`/`send`/`exit` under 30 s deadlines, killed on drop, for PTY tests.

## Public entry points

- `tuxport-run [options] [--] <program> [args…]`: `--tier 0|1` (default 0), `--rootfs <dir>`,
  `--env N=V` (repeatable), `--pty`/`--no-pty`, `--memory-limit <n[KMG]|none>`,
  `--low-port-offset <n>`, `--always-map-low-ports`, `--init`, `--stats`, `-h`/`--help`,
  `--version`; `tuxport-run --rootfs <dir> --import <tar.gz>` makes a store.
- `cargo test --workspace` (the three integration test files build only on aarch64 Linux).

## Data flow / lifecycle

### `tuxport-run`

- Options stop at the first argument that is not UTF-8, does not start with `-`, or is `--`; an
  option with a value takes it as the next argument or after `=`, and a flag given a value
  (`--pty=0`) is a usage error. argv is the program and its arguments as given; the environment is
  only the `--env` values. Usage errors (unknown options, bad values, no program) exit 2.
- `--import` (only with `--rootfs`) imports and syncs the store, prints its counts and exits (1 on
  failure, "the inode store already exists" for an existing one); anything after it is ignored.
- Without `--rootfs` the program is a host file and the guest has no filesystem (dynamically linked
  programs fail to load); with `--rootfs` the path is resolved in the store.
- A PTY is used by default when fds 0 and 1 are both terminals (then the host terminal goes raw,
  continuing with a message if it cannot, and `SIGWINCH` resizes the guest console); `--pty`
  forces one (24×80 unless both are terminals), `--no-pty` gives the guest the host's stdio.
  Through the PTY, guest stdout and stderr both reach fd 1.
- The kernel's `run` starts a machine, opens the console, boots by inittab with `--init`, spawns
  the program and bridges the terminal; afterwards it shuts down by inittab (`--init`; a reboot
  request is only a shutdown) or kills everything, then joins the bridge.
- Exit status (after the host terminal is restored): the guest's code; 128 + signal (with a line
  naming the signal, and the pc and address when known); 127 when the program is not found; 1 when
  the store cannot be opened; 126 for every other error (cannot load or execute, the console, no
  Tier 1 on this host, internal errors). `--stats` prints Tier 1's statistics.
- `SIGINT`, `SIGTERM` and `SIGHUP` to `tuxport-run` (`timeout`, ^C with `--no-pty`, a closed
  terminal) are caught once a run starts (`tuxport_hal::stop_notifier`) and handed to the kernel's
  `run` (`RunOptions::stop`): the first is sent on to the program, one at least a second later
  ends every guest process (duplicates sent to the whole process group by `timeout` or a terminal
  arrive together); the run then ends as usual, so the inittab shutdown, the final store sync and
  the host terminal's restore still happen.
- `borrow-check` is a default feature, so every build, the release binaries used by difftest,
  LTP and the survey included, checks that no borrow is held across a BKL release.
- Environment knobs read by the kernel and engine: `TUXPORT_LOG` and `TUXPORT_STRACE` (on when set,
  not empty and not `0`; tracing also routes fast syscalls through the BKL), `TUXPORT_PERF_MAP`
  (any value; Tier 1 writes `/tmp/perf-<pid>.map`), `TUXPORT_ON_BUG` (`kill`/`abort`; by default release
  builds kill only the guest process), `TUXPORT_TEST_HOOKS` (exactly `1`, only in debug or
  `test-hooks` builds), `TUXPORT_DBT_REGS` (`go` or `default`), `TUXPORT_BKL_WATCHDOG` (the
  watchdog's threshold in whole seconds, 10 by default, 0 off; the tests set 1 with a test hook
  that holds the BKL).

### Integration tests and benchmarks

- Guest programs (`tests/guest`): assembly built with `$CC` or the host `cc` (`-nostdlib -static
  -no-pie`), C with `musl-gcc -static -O2 -Wall -Wextra -Werror -pthread`, both natively on the
  aarch64 host; most run as host paths, a few are copied into the store.
- `rootfs_programs` imports the newest Alpine tarball by version under `target/rootfs/` (or
  `TUXPORT_ROOTFS_TARBALL`) into one shared store per test process, which tests running at the same
  time share; groups cover busybox and files, core dumps and the kernel log, shell semantics and
  job control, procfs/sysfs/net, the PTY (interactive shell, ^C/^Z, vi, end of input without a
  store), limits, OOM and cgroups, the Tier 1 code cache, the `Machine` API (low ports, several
  consoles, inittab with `tuxport-run --init`, the Files app's inotify and writes), importing into
  an existing store, user namespaces, answers of Linux 6.1 that kernels after 6.8 changed
  (`tiocsti.c`, and `linux61.c`: timerfd and signalfd reads, epoll_wait's error order, FUSE clones,
  loop block sizes), and one network test (`apk add`, `git clone`, Python imports
  at Tier 1 in a store of its own, gated by `TUXPORT_NETWORK_TESTS=1`, run only by `full.yml`).
  Most cases run under both tiers.
- Without a store or `musl-gcc` the tests skip with a message (each rootfs test that skips says
  so), unless `TUXPORT_REQUIRE_ROOTFS=1` / `TUXPORT_REQUIRE_MUSL=1` (set in CI); bug-injection and
  BKL-watchdog tests run only in debug builds and return silently otherwise. No cargo test sends
  `tuxport-run` a stop signal. The rootfs tests, difftest, LTP and the survey choose the newest
  Alpine tarball by version (`version_key`, `sort -V`).

## Dependencies (inbound / outbound)

### Inbound

- Developers, `harness` CI/benchmarks/LTP/survey/crash runners and `harness/diff` invoke tuxport-run.
- Cargo test runs the three aarch64-Linux integration files.

### Outbound

- `tuxport_kernel` provides run/options/reports, Machine, store import and sync.
- `hal` provides RawMode, isatty, SIGWINCH and stop notifiers.
- `harness` supplies `tuxport-bench` as a dev-dependency and development container/CI.
- `android/setup` fetches rootfs artifacts; guest builds use cc/musl-gcc.

## Invariants and gotchas

- Default builds enable borrow-check; runners detect kernel bugs on stderr via
  `tuxport: internal error`, even when guest exit status otherwise looks successful.
- Required-rootfs/musl switches make missing prerequisites fail rather than silently skip;
  bug-injection and BKL-watchdog cases only run in debug builds.
- RunOptions/Terminal/RunReport/RunError are kernel API types consumed here, not owned here.

## Confirmed bugs / technical debt

- **A forced PTY on a terminal with piped stdout is half set up.** Raw mode needs both fds to be
  terminals, so the host keeps ISIG (^C reaches the program as `SIGINT` through `tuxport-run`, ^Z
  stops `tuxport-run` itself), line-buffers input and echoes it while the guest echoes into the
  pipe; the size stays 24×80.

## Open questions

- Should `--import` install the stop handlers (SIGINT now ends it by default), and is a forced PTY
  with only one side a terminal meant to be supported?

## To verify

- IPv6 availability in `harness`'s development containers: without it the two always-map
  low-port cases skip with a captured message.
