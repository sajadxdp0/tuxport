# harness Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/harness.md` for the paired decision history, including pre-split decisions for harness/cli and harness/diff.
>
> Verified: 2026-11-10 (full mapping: 4 research concerns; 15 claims checked by 3 independent verifiers — 12 confirmed and 3 partial, corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: split harness/cli and harness/diff into separate maps)

## Responsibilities

`harness` owns development-container and CI orchestration, benchmarks, LTP, workload surveys
and power-loss testing. Root: `tests/bench/`, `tests/survey/`, `tests/crash/`, `scripts/dev`,
`scripts/dev.Dockerfile`, `scripts/build-ltp`, `scripts/ltp`, `scripts/survey` and `.github/workflows/`.

The command-line front end, cargo integration tests and guest programs are `harness/cli`;
differential cases and Starnix/gVisor builders are `harness/diff`. Instruction lockstep tests
remain `engine`, crate unit tests their respective modules, Android instrumentation `android/tests`,
Android builds `android/build`, rootfs fetching `android/setup`, kernel run/console handling
`kernel/boot`/`kernel/tty`, and kernel bug hooks `kernel/process`.

## Key types

- `tuxport_bench::Program` (name, rounds, test rounds, workload); `PROGRAMS` (sha256, lz, sort,
  crc32, vm), the shared freestanding `CFLAGS` and `build()` (with `$CC`).
- LTP's `results.txt` lines `<name> <PASS|CONF|FAIL|TIMEOUT|BUG|SKIP> <code> <secs>` (`- -` for
  SKIP), `logs/<name>.log` (and `.prev.log`), `durations.txt`, and the store's `.ltp` stamp;
  survey's `target/survey/<name>.{out,err}` (and `.native.*`) and report lines.

## Public entry points

- `tuxport-bench [--runs N] [--quick] [--skip-tier0] [--only NAME] [--stats]`.
- `scripts/dev [command…]`; `scripts/build-ltp`; `scripts/ltp [-a] [-j n] [-t tier] [regex]`;
  `scripts/survey [-t tier] [-s store] [-n image] [names…]`;
  `tests/crash/crash.sh <tuxport|native> <power|kill> [runs] [bin]` (as root in the VM).
- CI: quick checks on every push and pull request (`ci.yml`), the APK when its inputs change or on
  demand (`android.yml`), the slow suites nightly, on pushes to master and on demand (`full.yml`,
  `gh workflow run full.yml --ref <branch>`).

## Data flow / lifecycle

### Benchmarks

- `tuxport-bench` builds the benchmark programs, runs each natively and under Tier 1 (minimum of N
  runs, 5 by default) and under Tier 0 (once, unless skipped), fails if any output differs, and
  prints times, ratios and geometric means; `--quick` uses a twentieth of the rounds; it uses the
  `tuxport-run` next to its own executable. `bench_programs` checks the outputs with small round
  counts inside `cargo test`.

### LTP

- `build-ltp` builds a pinned LTP release in `alpine:3.24` with gcc: its libraries (a failure
  there stops the build), then each `testcases/kernel/syscalls` directory with `make -k` (a
  directory that fails is reported and installed as far as it built), into
  `target/ltp/ltp-<ver>.tar`.
- `ltp` (GNU tools, so inside `scripts/dev` on macOS) builds `tuxport-run` into
  `target/linux-arm64` first (unless `TUXPORT_RUN` names one), and uses a store on the `tuxport-ltp`
  volume when `/ltp` exists (inside `scripts/dev`), else `target/ltp/store` (`LTP_STORE`
  overrides), made again with LTP unpacked whenever the stamp of the LTP tarball and rootfs it
  records (names, sizes, times) changes (not when `tuxport-run` changes); a kernel config listing
  Tuxport's features (loop devices included) is rewritten on every run and passed as
  `KCONFIG_PATH`.
- Tests that need a device get one of Tuxport's loop devices over a file in their `/tmp` directory,
  and `LTP_DEV_FS_TYPE=tmpfs` makes LTP skip mkfs and mount a tmpfs there instead (Tuxport has no
  disk filesystems), as Starnix runs LTP; tests that need a particular disk filesystem report
  TCONF, and some (remounting the device to find files again, mounting a read-only device, mount
  errors of a block device) fail on Linux's tmpfs too.
- Tests from `runtest/syscalls` matching the regex (minus `sigtimedwait01`, `sigwaitinfo01` unless
  `-a`) run `-j` at a time (8 by default; tier 1 by default), slowest first, all on the one store,
  each in a fresh `tuxport-run` under `timeout 900` with `LTP_TIMEOUT_MUL=5`, `LTP_RUNTIME_MUL=0.1`;
  exit 0 PASS, 32 CONF, 124 TIMEOUT, else FAIL, and any `tuxport: internal error` in the log makes
  it BUG.

### Survey and crash

- `survey` keeps a store with 36 packages (network needed to build it; one whose creation did not
  finish, marked by `<store>.partial`, is made again; a store is never re-provisioned when the
  package list changes) and uses a prebuilt `tuxport-run` (`TUXPORT_RUN`, else the
  `target/linux-arm64` or `target/release` one), going through `scripts/dev` off aarch64 Linux.
  Each workload script runs under `TUXPORT_LOG=1` and a timeout (`SURVEY_TIMEOUT`, 1500 s), and
  one line reports exit code, time, the count of internal errors, the unimplemented syscalls seen
  (`ENOSYS:`) and the other unimplemented paths (`UNIMPLEMENTED:`: prctl, fcntl, ioctl, madvise and
  futex values, missing names under `/proc`, `/sys` and `/dev`, devices without a driver), read
  from the kernel's first-hit lines (`tuxport: unimplemented …`, at most 128 distinct paths a run).
  `-n image` runs only the native side, in docker, reporting exit code and time. With no names only
  the top-level scripts run, the container-engine ones included, though those need a store of
  their own (`-s target/survey/docker` or `podman`); `glibc/`, `suites/` and `ai/` (a mock model
  API) run by name. Nothing is judged automatically.
- `tests/crash` (root in the VM, dm-flakey, loop devices, the `tuxport-dev` image, Alpine 3.24.3
  hard-coded) runs a write workload in `tuxport-run`, drops writes at a random moment (power loss)
  or kills it, remounts, and checks that each file is one complete version and every fsynced file
  is whole; `native` mode is the control. `fsbench.c` is a standalone benchmark.

### Development container and CI

- `scripts/dev` rebuilds the image `tuxport-dev` (quietly, from `scripts/`) on every call and runs
  a command in it (a shell without one): `rust:1.90.0-slim-trixie` plus rustfmt, clippy,
  rust-analyzer, llvm, clang and musl-tools, as root, with the repository at `/work`,
  `CARGO_TARGET_DIR=/work/target/linux-arm64`, volumes for the cargo registry and `/ltp`,
  `--ulimit nice=40:40`, the Docker daemon's proxy settings, `-it` only when stdin and stdout are
  terminals, docker's default seccomp, and only `TUXPORT_DBT_REGS` forwarded (when set).
- `ci.yml`, on every push and pull request that changes more than `docs/`, `.agents/`, Markdown
  or `android/`: Linux arm64 (`ubuntu-24.04-arm`) fmt, clippy (`-D warnings`), fetch the rootfs
  and `cargo test --workspace` with the require switches.
- `android.yml`, on pushes and pull requests that change `core/`, `hosts/android/`, `android/`,
  the Cargo files or the rootfs and Ghostty scripts (or on demand), on x86_64 (`ubuntu-24.04`,
  for aapt2): NDK 29.0.14206865, Java 21, cargo-ndk (unpinned), the rootfs, Zig 0.17.0 (from
  ziglang.org, checked against a pinned sha256) and `scripts/fetch-ghostty` (with
  `target/ghostty` and Zig's global cache cached by the script's hash), then cargo-ndk clippy at
  API level 30, `assembleDebug lintDebug`, the APK as an artifact. Both quick workflows cancel a
  run in progress when the same branch is pushed again.
- `full.yml`, nightly at 18:00 UTC on main when it has new commits, on
  pushes to master and on demand: the network test, a lockstep run of 200,000 iterations,
  `scripts/build-starnix-tests` (its tarball cached by the hash of the script and
  `tests/starnix/compat`) and `scripts/build-gvisor-tests` (cached by the hash of the script,
  `tests/gvisor` and its program list), both caches restored and saved in steps of their own so
  that a build is saved even when a later step fails, a release build of `tuxport-run`,
  `scripts/difftest -R` at Tier 1 with one lane (`ping_group_range` opened, `vmstat` sampling the
  runner every 10 s, `target/difftest` deleted before and after, so every run imports the store and
  installs its packages again; every case's `.tuxport.err`, the failed cases' outputs and the
  samples uploaded as the `difftest` artifact whatever the result), and the engine tests built
  with clang. No workflow sets a timeout, and `full.yml` has no concurrency group.
- Not in CI: LTP, the survey, instrumentation tests (no KVM), Tier 0 difftest, parallel lanes,
  the performance probe (`tuxport-bench`).

## Dependencies (inbound / outbound)

### Inbound

- Developers and scripts; CI.

### Outbound

- `harness/cli` supplies `tuxport-run` and cargo integration tests; `harness/diff` supplies
  differential cases and upstream-suite builders. CI builds the CLI before invoking difftest.
- `tuxport-bench` also serves CLI integration tests as a dev-dependency.
- Docker/colima, musl-tools, `android/setup`'s rootfs artifacts and package/LTP downloads.
- GitHub Actions/runners and the SDK manager/ziglang.org for `android/build`.

## Invariants and gotchas

- The bug detector everywhere is a line starting with `tuxport: internal error` on `tuxport-run`'s
  stderr; release builds kill only the guest process, so the run continues and only the grep
  notices.
- Survey and LTP stores persist across runs. `harness/diff` records its own persistent-store
  and clone behavior; a survey store is not re-provisioned merely because its package list changes.
- A partial LTP run overwrites `results.txt` with that subset; durations are merged.
- The rustup components in `scripts/dev.Dockerfile` must match those in `rust-toolchain.toml`:
  rustup installs a listed component the image lacks on every `cargo` call, and the container is
  removed after each run, so it downloads it again every time. CI installs all listed components
  through `rustup toolchain install`.
- A test that holds the BKL in one long syscall, or spins in one, cannot be stopped by the case's
  guest `timeout`; only killing `tuxport-run` ends it.
- GitHub runs scheduled workflows only from the default branch: the nightly `full.yml`
  uses main's copy of the workflow and checks out main.
- `full.yml` must build release `tuxport-run` itself before the differential tests: no other step
  there makes it (cargo test builds debug, lockstep builds only `tuxport-lockstep`), and difftest
  never builds one.
- CI's clang step relies on the runner image's clang; CI installs only llvm and musl-tools.
- No workflow sets a timeout, so GitHub's default 6-hour job limit applies: the 2026-11-19
  nightly ended cancelled at exactly that limit, and a slow differential run can end the same
  way without anything failing.

## Open questions

- Should partial LTP runs merge into `results.txt` as the durations do?

## To verify

- Whether IPv6 is available inside `scripts/dev` containers (if not, the two always-map low-port
  cases skip with a message captured by the test harness); how much Docker's default seccomp in
  `scripts/dev` differs from the unconfined Linux difftest path.
