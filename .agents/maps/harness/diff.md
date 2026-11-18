# harness/diff Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/harness/diff.md` for the paired decision history (not yet created; pre-split decisions D1–D3, D5, D9, D11–D14, D17–D20, D23, D27, D29, D31, D33, D34, D36–D41 stay in `.agents/decisions/harness.md`).
>
> Verified: 2026-11-15 (split out of the harness full mapping of 2026-11-10; the boundary was checked against the source at the split)

## Responsibilities

`harness/diff` owns native-vs-Tuxport differential runs, case inputs/normalization and
Starnix/gVisor suite builds. Root: `scripts/difftest`, `tests/diff/`, `scripts/build-starnix-tests`,
`tests/starnix/`, `scripts/build-gvisor-tests` and `tests/gvisor/`.

The CLI and guest-only regression tests remain `harness/cli`; development containers, CI, LTP,
benchmarks, surveys and crash testing remain `harness`.

## Key types

- difftest's per-case directives, matched on any line: `# docker-run-options:`,
  `# tuxport-options:` and `# difftest-files:`; outputs `target/difftest/<case>.{native,tuxport,
  tuxport.err}`, failure copies `failed/<time>-t<tier>-<case>.*`, timings
  `durations-t<tier>.txt` (`<case> <seconds>`), the run lock `.lock` (a symbolic link naming the
  run's pid), lane claims
  `.slot-<n>`, lane stores `<store>-w<n>` (marked `.cloned`); the image `tuxport-difftest`,
  labelled with the exported directory list.
- Suite tarballs: `target/starnix-tests/starnix-tests-<commit12>.tar` and
  `target/gvisor-tests/gvisor-tests-<commit12>-<list hash>.tar`, each with `bin/`, `BUILT.txt`,
  `SKIPPED.txt` (`<program>: <first error>`), `VERSION` and the licences.

## Public entry points

- `scripts/difftest [-t tier] [-r] [-s store] [-j n] [-R] [case…]`.
- `scripts/build-starnix-tests`; `scripts/build-gvisor-tests [--list | --all]`.
- Case suite driver: `tests/diff/gtest/run.sh <suite> <skip> <program>…` with optional `ONLY`.

## Data flow / lifecycle

### Differential tests

- One run at a time: a run creates `target/difftest/.lock`, a symbolic link naming its pid made in
  one step, before touching the store; it waits while the run it names is alive (`ps -p`, which
  also finds another user's process), takes over a lock whose run is gone (or an earlier version's
  lock directory, with or without its pid file) by moving it aside first, so that of two waiters
  finding the same dead run the one that moved away the other's new lock puts it back, and
  removes it on exit; its lanes run under it.
- On aarch64 Linux `tuxport-run` runs inside a container (`TUXPORT_IMAGE`, default
  `debian:trixie-slim`, the caller's uid, seccomp unconfined, the repository at its own path) for
  the store import, the tool check, the export and the cases, while the store's package and suite
  installs run it on the host; elsewhere
  everything goes through `scripts/dev`. The binary is a prebuilt one, which difftest never
  builds: on aarch64 Linux `TUXPORT_RUN`, else `target/linux-arm64/release/tuxport-run`, falling
  back to `target/release/tuxport-run` when the chosen one is not executable; elsewhere always
  `target/linux-arm64/release/tuxport-run`. Only `TUXPORT_DBT_REGS` is passed through.
- A store (`target/difftest/store`, inside the work tree, since both containers mount only the
  repository) is imported once and gets 22 packages (fuse3, fuse-overlayfs, sshfs, rclone and
  openssh-sftp-server among them, for the `fuse` and `fuse-tools` cases, whose native side needs
  `--device /dev/fuse`); a tool check on every top-level run (`command -v` of each of 17 commands,
  and util-linux's `nsenter` and `dmesg`) aborts if one is missing.
- Each suite tarball (`starnix`, `gvisor`; the last one in `ls` order) is installed into the store
  at `/usr/lib/<suite>-tests` when its `VERSION` differs from the store's.
- The store's top-level directories `bin etc home lib media mnt opt run sbin srv tmp usr var`
  (those that exist, `/tmp` and `/var/tmp` empty; docker provides `/dev`, `/proc` and `/sys`, and
  each case makes `/root`) are exported from inside Tuxport and imported as the docker image
  `tuxport-difftest`, labelled with that list, replacing the previous image: on the first run, with
  `-r`, after a package or suite install, when the image is missing, when its label changed or
  when its suite versions differ from the store's (which also remakes the lane clones).
- Each case script runs with stderr merged into stdout on both sides, after `/root` (`$HOME`) and
  `/tmp` are removed and made again empty (modes 700 and 1777), so caches and files earlier runs
  left in the store do not make the sides differ: natively with `docker run` (seccomp unconfined,
  `PATH`, `HOME=/root`) plus the case's `# docker-run-options:` (62 of the 93 cases, 40 of them
  `--privileged`), and as `/bin/sh -c` under `tuxport-run` plus its `# tuxport-options:` (two
  cases); an `exit N` line is appended to each output. A case passes when the two outputs are
  byte-identical and `tuxport-run`'s own stderr has no `tuxport: internal error` line; scripts
  normalise varying output themselves. A script of 128 KB or more (prefix and packed files
  included) fails, which is why `compat` is split into `compat` to `compat9`.
- `# difftest-files: <dir>` ships `tests/diff/<dir>` with the script: a ustar archive, gzipped and
  base64-encoded, unpacked by the guest's busybox into `/tmp/difftest-files/<dir>` before the case
  runs on either side. `line-discipline` uses it for Starnix's PTY scenarios and trace generator
  (`tests/diff/line-discipline`, BSD-licensed, copied unchanged with its source commit in a
  README) and runs all 83 of them, each under a 300 s timeout; the eleven suite cases use it for
  the shared gtest runner.
- Lanes (`-j`, default 4 on macOS, 1 elsewhere): each lane claims a slot and uses that slot's store
  clone (APFS clone, or a copy with `--reflink=auto`; remade only when unmarked or on a rebuild),
  cases go slowest first by recorded time, and each lane prints its report in one write; failures
  are copied to `failed/`. Only parallel runs record durations, and only for cases that passed.
- A case name without `tests/diff/<name>.sh` is a failure, and so is every requested case that no
  lane reported (a lane that died); lanes rerun the script by its absolute path, so a run started
  from another directory works.
- With `-R` every failed case runs once more after all the others, alone and on the main store: one
  that passes then is reported `flaky` (counted in the summary, the run's exit status 0 if nothing
  else failed), one that fails again prints both reports; the first failure's outputs stay in
  `failed/`.
- A case running a prebuilt suite (`run.sh <suite> "$SKIP" <program>…`) is reported `SKIP` when
  that suite's tarball has not been built, and fails when the tarball lacks a program it names
  (unless `SKIPPED.txt` lists it as not building); the summary counts the skipped cases, which do
  not fail the run.

### Starnix's and gVisor's syscall tests

- `build-starnix-tests` fetches a pinned Fuchsia commit (a sparse checkout of
  `src/starnix/tests/syscalls/cpp` and the libraries it uses: fit, stdcompat, fbl's headers, files,
  fxl strings, utf-utils) and a pinned googletest (sha256), and in `alpine:3.24` builds every
  `*_test.cc` with clang, statically against musl, stripped, into its tarball; failures go to
  `SKIPPED.txt` with their first error. The sources are used unchanged; `tests/starnix/compat`
  supplies glibc's `TEMP_FAILURE_RETRY` (forced include), a `<linux/prctl.h>` that defers to
  musl's `<sys/prctl.h>`, and Android's copy of `<linux/bpf.h>`.
- `build-gvisor-tests` fetches a pinned gVisor commit (a sparse checkout of `test/syscalls/linux`
  and `test/util`) and pinned abseil, googletest and google benchmark releases (sha256), and in
  `alpine:3.24` (`tests/gvisor/build.sh`, which reuses dependencies built earlier in the same
  container) compiles the test library and, without Bazel, each program from the sources and local
  libraries `tests/gvisor/parse-build.awk` reads from the BUILD file (arm64 branches of
  `select_arch` only; test-case libraries linked as objects so their tests register; a source that
  does not compile shows only when a program needing it fails to link). `tests/gvisor/compat`
  supplies glibc names musl lacks (forced include), headers that defer kernel headers to musl's
  where both define the same structures, and a `RunfilePath` that finds helper programs in
  `/usr/lib/gvisor-tests/bin`. Only the programs named in the `gvisor-*` cases (the list `--list`
  prints and whose hash names the tarball) and every helper (targets not ending in `_test`) are
  built, older tarballs removed; `--all` builds every program that builds for arm64 into
  `target/gvisor-tests/all/` for a full run by hand (too large for the difftest image on the
  development VM).
- Each builder does nothing while its tarball's name exists, so a local change to `tests/starnix`
  or `tests/gvisor` takes effect only after the tarball is deleted (CI keys its caches on them).
- Seven Starnix cases cover 65 of its built programs by subsystem (`starnix-mm`, `-process`,
  `-signal`, `-fs`, `-proc`, `-io`, and `-overlay`, which runs overlayfs_test and fs_test's
  `OverlayFs` tests with `TEST_TMPDIR` on a tmpfs, since Linux refuses docker's overlay root as an
  upper layer), and four gVisor cases 43 of its programs (`gvisor-process`, `-fs`, `-net`,
  `-unix`). Each calls the shipped `tests/diff/gtest/run.sh` once (difftest reads the suite from
  that one call) with the suite, its skip filter and its program list, and optionally `ONLY`, a
  gtest filter of the only tests to run; the runner lists each program's tests (minus disabled
  ones) and runs each in its own process under `timeout -s KILL 60`, stdin from `/dev/null` and
  its output in a file (a crash or hang, a test ignoring `SIGTERM`, a child keeping the output open
  or a test reading stdin does not take the rest with it), printing `<test> ok|failed|skipped|exit
  <code>`, with core files off, the shell's crash reports filtered out and `/tmp` set back to 1777
  at the end. Tests stay in the foreground (a background one would inherit `SIGINT` and `SIGQUIT`
  ignored); a background watcher checks the current test (named in `/tmp/gtest.cur`) every 5 s,
  and one running 45 to 50 s gets one `hang:` line per thread of its process tree (found by the
  `--gtest_filter=<test>` command line): pid, tid, name, state and its
  `/proc/<pid>/task/<tid>/syscall`. They follow the test's line only when the timeout killed it
  (exit 137), so a slow test that passes prints the same on both sides; on the native side Yama
  makes most of those files unreadable, as the watcher is no ancestor.
- Skips are a negative gtest filter in each case, grouped by reason: a deviation recorded in a
  kernel map (named with its module), a feature decided against, or a test that cannot be compared
  (it fails on the native side because of docker, the VM or a newer kernel, answers differently
  only on CI's runner, depends on timing, or hangs on both sides), or a gVisor test that sleeps
  10 s or more by design (left out for CI time). The runner's header lists the programs in no case and why: those that would reboot,
  suspend or set the clock of the VM, features Tuxport does not have (bpf maps and other program
  types, perf, userfaultfd, tun), devices neither side has, and two with nothing to compare.

## Dependencies (inbound / outbound)

### Inbound

- Developers and `harness`'s full.yml build suites and run comparisons; CI retains failure artifacts.

### Outbound

- `harness/cli` supplies a prebuilt tuxport-run; this runner never builds it.
- `harness` supplies scripts/dev off aarch64 Linux and CI orchestration.
- `android/setup` fetches rootfs artifacts; Docker/colima run native containers.
- Network package installs, Fuchsia/Google/GitHub suite sources and pinned build dependencies.

## Invariants and gotchas

- Difftest compares the two sides only with each other: a failure identical on both sides passes.
- CI's native side is not the development VM's: a 7.x kernel (overlayfs defaulting to
  `redirect_dir=on`, Linux 5d39580f68e6's `CAP_NET_ADMIN` check that `tuxport-run`'s host sockets
  then fail), a CPU with SVE and AArch32 at EL0, and runner settings (`rmem_default` and `rmem_max`
  of 1 MiB, `fs.suid_dumpable` 2, docker's stack limit). Cases print only what both kernels give
  (relations instead of such values, explicit mount options), the suite cases skip the rest, and
  6.1's answers that newer kernels changed are checked by `tests/guest` programs instead.
- The native side of a case using loop devices (`loop`) shares the host's devices: it takes
  whatever loop-control hands out, makes its own node for it (a new device has no node in the
  container's `/dev`), prints no device numbers, and waits for an autoclear device to be unbound
  (kernels after 6.1 unbind after the last close, not at once). Disk read-only state survives
  unbinding on Linux, so the case leaves its device writable, with autoclear, as it closes it.
- A case that counts processes must not count its own pipeline: `ls /proc | grep -c` sees `grep`
  only when it started before `ls` read `/proc`, which differs run to run on either side; a shell
  glob (`set -- /proc/[0-9]*; echo $#`) starts no process.
- Cases with `--privileged` act on the development VM itself: a test that sets the clock succeeds
  natively and moves the VM's clock until its time sync corrects it (Starnix's
  `timerfd_mutable_test` and `time_test` call settimeofday), so such tests must stay out of cases.
- The development VM's Ubuntu 6.8 kernel oopses in AppArmor (`aa_inet_bind_perm`) on an inet bind
  with `AF_UNSPEC` from a confined container. Tuxport no longer passes such a bind to the host (the
  kernel/net map), but a case whose native side makes one must run unconfined (`--privileged`, or
  `--security-opt apparmor=unconfined` as `inet-bind` does).
- busybox's `timeout` sends `SIGTERM` by default: a test that ignores it, or a child that keeps the
  output pipe open, makes `$(timeout …)` wait forever on either side. `timeout -s KILL` with the
  output written to a file avoids both. Output that goes through `grep` inside the guest is
  block-buffered, so a running case's `.tuxport` file shows no progress.
- Kernel bugs are detected from `tuxport: internal error` on tuxport-run's stderr, independently
  of matching guest outputs.
- Store and lane clones persist outside `/root` and `/tmp`, while native containers start fresh;
  `-r` re-exports the image and remakes clones but installs no packages.
- CI must build the release CLI before calling difftest; orchestration belongs to `harness`.

## Confirmed bugs / technical debt

- **Three waiters can still run together:** when one moves aside a lock another waiter has just
  made and a third makes its own before it is put back, the moved one's run goes on without it.

## Open questions

- Should difftest pass `TUXPORT_ON_BUG` and `TUXPORT_LOG` through, as it does `TUXPORT_DBT_REGS`?

## To verify

- The suite sizes seen in built tarballs, not in source: 81 Starnix programs build at 86962cb and
  12 do not; `--all` builds 264 gVisor programs (256 tests).
- How much Docker's default seccomp in scripts/dev differs from the unconfined Linux difftest path.
