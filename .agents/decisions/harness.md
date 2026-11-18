# Run and test harness (harness) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: harness"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b). D1–D8 were seeded by hand from the design doc §13.3
> and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite commits.

## Constraint index (compressed active decisions)

> Full text of D1, D3–D6, D8, D11, D12, D14–D17, D20, D21: `git show 248d79f637c8f05dadc6ef9fdb0733c568c6a468:.claude/decisions/harness.md`

- **D1** — Each `tests/diff` case must print byte-identical output (stderr merged, exit code appended) in a docker container built from the store's own files and under `tuxport-run`, itself in a container so both sides share the network setup; behaviour that changes with the host kernel version is not compared, and scripts normalise addresses, pids and random markers themselves (round 09)
- **D3** — `-j` lanes (4 on macOS, 1 on Linux hosts and CI) each run on a copy-on-write clone of the store, slowest cases first, and the clones must be rebuilt with `-r` after packages are added to the main store (round 178)
- **D4** — `scripts/ltp` keeps its store on the VM-local `tuxport-ltp` volume (`LTP_STORE` overrides it), because eight `tuxport-run`s committing to one store through virtiofs outwaited SQLite's busy timeout and got `EIO` (round 178)
- **D5** — `tuxport-run` enables the kernel's `borrow-check` by default, and difftest, LTP and the survey treat `tuxport: internal error` on stderr as a failed case, a `BUG` or a counted line, so measurements with the default build include the check (`--no-default-features` removes it) (round 89)
- **D6** — Integration tests skip without a rootfs tarball or `musl-gcc` unless `TUXPORT_REQUIRE_ROOTFS=1` / `TUXPORT_REQUIRE_MUSL=1`, which CI sets so that a missing one fails (rounds 03, 04)
- **D8** — `tests/crash` checks store durability by dropping writes on a dm-flakey device (or killing the process) against a native control run; it needs root, loop devices and dm-flakey in the VM, so it runs by hand, not in CI (rounds 190, 198)
- **D11** — Where Linux 6.1 differs from the native kernel the differential tests run on (unprivileged TIOCSTI), a `tests/guest` program runs on both tiers through `rootfs_programs` and the differential case keeps only what both kernels share (ef5f836)
- **D12** — difftest counts every requested case without an ok or FAIL line as a failed "no result", fails a group name without a script and starts lanes by the script's absolute path, so a lane printing a malformed report line fails its case (1f93e3d)
- **D14** — Cases check only what Linux guarantees: they count processes with the shell's own glob (`set -- /proc/[0-9]*`) rather than a pipeline that races, accept independent answers (DNS A and AAAA) in either order, wait up to 5 s for asynchronous effects such as a joined thread's `/proc/<tid>` going, and read a pipe written by several commands until EOF (404a804, fe4bb2b, c11c1db, 093cb96)
- **D15** — Test files keep their tests in order with a single-use helper after its test and shared helpers after all tests in run order (tests/bench puts `main` right after the types), and helpers appended above the tests drift from this order (50f9284)
- **D16** — rust-analyzer is a component in rust-toolchain.toml and scripts/dev.Dockerfile installs the same rustup components, so a component added to one list but not the other is downloaded again by every `scripts/dev` call (503022d)
- **D17** — scripts/ltp runs LTP's device tests on tmpfs over loop devices (`LTP_DEV_FS_TYPE=tmpfs`, kernel/vfs D21) and `tests/diff/loop.sh` makes its own nodes and waits for autoclear; each test fallocates a sparse 300 MB file in the store's `/tmp`, and the native side sharing the host's loop devices is flaky (recorded in `docs/milestones.md`) (a26c8b4)
- **D20** — difftest, ltp and survey choose Alpine tarballs with `sort -V` and the rootfs tests with `version_key`, as the APK build does (android/native D12), never by lexical order (b014e84)
- **D21** — `tuxport-run` rejects an inline value on a flag (`--pty=0`) with exit status 2, while options that take a value still accept one inline (`--tier=0`) (c11c1db)

## Active (full detail)

### D2: Test cases carry their own container and `tuxport-run` options
- **What**: A case line `# docker-run-options: …` is added to its native container (capabilities, AppArmor off) and `# tuxport-options: …` to `tuxport-run`.
- **Why**: Mount tests need `CAP_SYS_ADMIN` and AppArmor off on colima and GitHub's runners; the low-port case needs `--always-map-low-ports` because a root container may bind the real port; a native side making an `AF_UNSPEC` inet bind must run unconfined (as inet-bind does), since the VM's kernel oopses on one from an AppArmor-confined container.
- **Watch out**: A colima update may change the VM's kernel, and with it which cases need AppArmor off.
- **Source**: rounds 105 (`105-mount-subset`), 208 (`208-low-ports-direct`); 0b2c3c8

### D7: The workload survey is read by a human, not compared
- **What**: `scripts/survey` runs services, tools, suites and AI CLIs inside a store with `TUXPORT_LOG=1` and reports exit code, time, internal errors, unimplemented syscalls (its ENOSYS column) and the other unimplemented paths the kernel counts (an UNIMPLEMENTED column, kernel/signal D24) per workload; `-n` runs the same scripts natively for comparison when something looks wrong.
- **Why**: The differential tests cover only a few runtimes (C, coreutils, Go, Java, Node.js, Python, Lua/SQLite, gdb); most services and tools common on a phone terminal had never run in Tuxport, and the survey finds the gaps they actually hit.
- **Tradeoffs**: The survey shows which paths were hit but not how often.
- **Watch out**: The split depends on the format of the kernel's `tuxport: unimplemented` lines.
- **Source**: round 62 (`62-workload-survey`); 99b4216

### D9: Shared host tooling is separate from CLI and differential suites
- **What**: harness owns development containers, CI, benchmarks, LTP, surveys and crash tools; tuxport-run and cargo integration belong to harness/cli and differential/upstream runners to harness/diff.
- **Why**: Separating independently maintained subsystems makes the oversized parent map practical to read and maintain.
- **Tradeoffs**: Cross-boundary work follows child maps, while historical decisions keep their original parent tags and stable IDs.
- **Watch out**: Recorded weaknesses stay open in the map: a forced PTY on a terminal with piped stdout is half set up, and three waiters can still pass difftest's lock together (D29); these pre-split qualifications remain discoverable through the child maps and are not resolved by the ownership split.
- **Source**: 16ac7a2 (list since updated by 1f93e3d, 1459292 and ce8a9ce; bug list by 5d23297, 7c5e636, dcacfb6, 6d9a9e8, f5c09f7, d9e1f0c, 300d384 and ca8c583); 8a17e4a9; d053be24; 60790c14; 39e55b4b

### D10: CI installs a pinned Zig and caches the libghostty-vt build tree for the Android job
- **What**: The Android job downloads zig-x86_64-linux-0.17.0 from ziglang.org, checks it against the sha256 copied from ziglang.org/download/index.json, puts it on `PATH`, runs `scripts/fetch-ghostty`, and caches only `target/ghostty` (the unpacked release tarballs with the Zig caches the build keeps inside them) under the key `libghostty-vt-<hash of fetch-ghostty>`.
- **Why**: The Android native library always builds libghostty-vt, which needs Zig and its source (android/native D6), and since libghostty-vt 1.0.0 the build keeps its Zig caches in `target/ghostty` and must not use `~/.cache/zig`.
- **Tradeoffs**: No third-party setup action, but ziglang.org asks CI users to prefer mirrors; a new key fetches and builds from scratch on its first run, and the cache holds several hundred MB.
- **Watch out**: A download failure from ziglang.org fails the job, and actions/cache saves only on a miss, so a key keeps whatever its first run built.
- **Source**: b38d29c, 172be7c

### D13: Kernel fixes and features get a differential case that reproduces their calls
- **What**: Each fix adds a case that reproduces the calls in Python, C or busybox to a differential group (an existing one such as edge-cases, namespaces, pty, fork, nsports, compat2 or mount, since the third map-bug triage one privileged group per batch, compat5 to compat10, and for a new feature a case of its own such as vdso or overlay-inotify) instead of a kernel unit test or the real program (nginx, tmux), checks that it fails with the previous binary (moving a check whose trigger a later fix removes to one that still fails without the earlier fix, as `stale.c` in edge-cases moved to a seccomp filter installed with TSYNC), has C cases define the kernel constants and structures they need, since the image has no Linux headers, runs anything that could hang under `timeout`, and removes the Starnix and gVisor skips the fix makes pass.
- **Why**: Native Linux is the reference for each fix's semantics, the programs that hit the bugs are not in the difftest store, gtests skipped for a fixed bug guard nothing until their skips are removed, and a C case that fails to compile on both sides prints the same and passes.
- **Tradeoffs**: The programs themselves are not exercised, each interrupted wait costs 100 ms, timing cases such as `stale.c` take seconds on each side, `O_SYNC` durability is not checked, compat5 to compat10 need `--privileged` on the native side, values that differ between the sides are compared by range or relation (oom_score) or filtered to the case's own (`/proc/locks`), and checks whose answer depends on the host (block counts on the macOS development host's APFS, tmpfs `O_DIRECT` where 6.8 differs from the 6.1 contract, writing the VM's global fs.mount-max) are left out.
- **Watch out**: CI's newer native kernel may differ (netlink group counts, block accounting, the fs.mount-max default, the order of overlay events, sealed system mappings for vdso's mprotect checks), unprivileged user namespaces depend on the native host's settings (Ubuntu's `apparmor_restrict_unprivileged_userns`), the nftables rate case needs ten sends to finish well within half a second, and `stale.c` loses its reproductions without failing when the waiting thread takes more than 0.3 s to start its wait; cgroup quota/period comparisons require a native CPU controller, and thread procfs checks compare readability rather than unstable contents.
- **Source**: 96cd8bd, 65c44f6, d8136b6, e020977, ea08993, 68cafce, 0a4712e, f6a82cc, b43ff37, dcacfb6, 5d7b936, 39ea702, 99b4216, 16364c0, d7076fc, edd7f95, 970cf6a, f5c09f7, 9593bae, 27609fe; 45be9fae; 20137e4c; d053be24; 39e55b4b

### D18: Starnix's PTY scenarios run from files packed into the difftest script
- **What**: A `# difftest-files:` line makes scripts/difftest pack that `tests/diff/<dir>` as a gzipped, base64 ustar archive prepended to the script, which both sides unpack with busybox into `/tmp/difftest-files/<dir>`, and the line-discipline case runs all 83 of Starnix's recorded scenarios (copied unchanged with Fuchsia's licence) one by one against the native kernel.
- **Why**: The scenarios and generator are about 410 KB, a difftest script travels as one `sh -c` argument (128 KB limit), neither side has the work tree, and the store should not change for test data.
- **Tradeoffs**: Packed files count towards the 128 KB, behaviour that changed after 6.1 is compared with the native kernel, and a future failing scenario needs the removed skip mechanism back.
- **Watch out**: CI's native kernel may trace some scenario differently, the generator's 2 s deadlines leave little margin at Tier 0 (where the case takes about three minutes), and updating the copied files must keep the README's commit current.
- **Source**: 24ecd25, 8dc166b

### D19: Fuchsia Starnix's and gVisor's syscall tests run as differential cases, built in CI
- **What**: scripts/build-starnix-tests (a pinned Fuchsia commit and googletest built with clang statically against musl) and scripts/build-gvisor-tests (gVisor's BUILD files parsed with awk, building only the programs the cases name plus helpers) make tarballs that difftest installs into its store and image when their VERSION changes and CI caches by the build inputs' hash, and six `starnix-*` cases by subsystem (plus starnix-overlay, D31) and four `gvisor-*` cases ship the shared `tests/diff/gtest/run.sh` through difftest-files, which runs each test in its own process under `timeout -s KILL 60` with stdin from `/dev/null`, skipping divergences by name under a comment naming the map item, the decision or the native-side cause.
- **Why**: Both suites are Linux-validated gtest corpora (gVisor's covers AF_UNIX, netlink, ptrace, mounts, PTYs and signals in depth, Tuxport's weaker areas), and while only mmap_test and the waitpid programs had cases and CI never built the tests, the cases passed vacuously there and the other built programs guarded nothing.
- **Tradeoffs**: Skip lists must be edited when a recorded deviation is fixed or the fix goes untested; programs that reboot, suspend, set the VM's clock, test features Tuxport decided against or need devices are left out, tests that hang on both sides or sleep 10 s or more by design are skipped, and tests the native side is denied under this VM's AppArmor (a file-capability test in an unprivileged user namespace) are not compared; the awk build breaks if gVisor changes its BUILD format; building needs the network, and the suites add about 15 minutes to a one-lane run.
- **Watch out**: CI's newer native kernel may differ on tests that pass under 6.8, googlesource must keep serving fetches by hash on cache misses, privileged cases act on the development VM, busybox's `timeout` sends only SIGTERM unless told otherwise, Pty.SendSignals hangs only under difftest for a reason not found, and a Fuchsia or gVisor upgrade can change tests and need new skips.
- **Source**: 1459292, e03c0cc, 612d238, c11c1db, 70c1bc6, ce8a9ce, 84ff4a7

### D22: CI's Android clippy checks the app's API level
- **What**: The Android job runs `cargo ndk --platform 30 clippy`, the minSdk the app is built for (android/native D3).
- **Why**: CI's Android clippy should check the API level the app is built for.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 0709982

### D23: Script comments say Tuxport; labels, names and test data stay lowercase
- **What**: Comments in `scripts/`, `tests/diff`, `tests/guest`, `tests/survey`, `tests/crash` and the CLI tests name the system Tuxport (knowledge-loop D6), while difftest's `tuxport()` function, the `.tuxport.err` suffix and the `Ns tuxport)` label its timing parser reads back, crash.sh's `tuxport|native` mode, the `tuxport: internal error` lines the scripts grep for and test data stay lowercase.
- **Why**: Scripts and cases use the same word both as the system's name in comments and as a label, a function name or data.
- **Tradeoffs**: A comment can say "the Tuxport side" next to an output line that labels it "tuxport".
- **Watch out**: Renaming the label later would also need difftest's timing parser and the saved reports changed together.
- **Source**: 248d79f

### D24: CI is split into quick checks, the Android build and nightly full tests
- **What**: ci.yml runs fmt, clippy and cargo test on every push and pull request that touches more than docs, `.agents`, Markdown or `android/`; android.yml runs when `core/`, `hosts/android/`, `android/`, the Cargo files or the rootfs and Ghostty scripts change, or by hand; full.yml runs the network test, extended lockstep, the Starnix and gVisor builds, the differential tests (`scripts/difftest -R`, D38, after building the release `tuxport-run` in a step of its own) and the clang engine tests nightly at 02:00 China time when develop has a commit from the last 25 hours, on pushes to master and by hand; the quick workflows cancel a run a newer push supersedes, and the performance probe is dropped.
- **Why**: Each of the frequent pushes waited 16 minutes (30 or more with the Starnix and gVisor cases) for suites that rarely change their verdict between two pushes.
- **Tradeoffs**: Regressions only the differential tests catch show up the next morning, so risky changes need a local difftest or a manual full run; the nightly run uses master's copy of full.yml; a rust-cache miss adds the release build to full.yml; Tier 1's overhead on Neoverse N2 is no longer recorded in CI logs.
- **Watch out**: GitHub runs scheduled workflows only from the default branch, so the nightly run picks up a change to full.yml only once master has it (without 48dce39's build step the differential tests never ran from 0202f1e on), while a `workflow_dispatch` started with `--ref <branch>` runs that branch's copy; CI's newer native kernel answers some cases otherwise than colima's 6.8 (D36); and a new Android build input outside the path filters goes unbuilt until a run that matches.
- **Source**: 0202f1e, 48dce39; ba6200f (D36); 4083007 (`-R` and manual runs' branch); 36083476; 20137e4c; d053be24; 39e55b4b

### D25: Signals to tuxport-run go through the guest, so shutdown still runs
- **What**: `tuxport-run` catches SIGINT, SIGTERM and SIGHUP with `tuxport_hal::stop_notifier` (hal D14) and hands them to the kernel's run through `RunOptions::stop` (kernel/process D30): the first is sent on to the program, one at least a second later ends every guest process, and the run then ends as usual.
- **Why**: Killing `tuxport-run` (`timeout`, ^C with `--no-pty`, a closed terminal) skipped the inittab shutdown, the final store sync and the host terminal's restore.
- **Tradeoffs**: A guest that ignores the forwarded signal needs a second one at least a second later, and duplicates within that second (`timeout` and terminals signal the whole process group) are dropped.
- **Watch out**: None known.
- **Source**: 5d23297

### D26: Test scripts remake stores and binaries they cannot trust
- **What**: scripts/ltp builds `tuxport-run` into `target/linux-arm64` unless `TUXPORT_RUN` names one and remakes its store whenever the recorded stamp of the LTP tarball and rootfs (names, sizes, times) changes, and scripts/survey writes `<store>.partial` before creating a store, removes it after `apk add`, and remakes a store it finds marked.
- **Why**: scripts/ltp reused a stale store and binary, and a failed survey store creation left a store the next run used as is.
- **Tradeoffs**: The LTP store was remade once after the change, and a survey store created before the marker existed is trusted as complete.
- **Watch out**: None known.
- **Source**: 5d23297, dcacfb6

### D27: Both sides start every case from the same files
- **What**: difftest exports the store's bin, etc, home, lib, media, mnt, opt, run, sbin, srv, tmp, usr and var (with `/tmp` and `/var/tmp` empty) into the native image, labelled `tuxport.export` with that list and exported again when it changes, and both sides remove `/root` and `/tmp` before each case and make them again (modes 700 and 1777); the shared gtest runner still deletes gVisor's `/tmp/exec-only-*` leftovers and restores `/tmp` to 1777 within its case.
- **Why**: The native image held only six top-level directories while the reused Tuxport lane store kept `/root` (Go's and uv's caches) and `/tmp` between groups and runs, so cases made `/var/run` and cleaned up by hand, and leftovers (gVisor's fs tests leaving `/tmp` 0777, pid-named files from a failed ExecTest.ExecuteOnlySpecialFiles) made later cases fail like regressions.
- **Tradeoffs**: Go and uv rebuild their caches every run, the first run after a change to the list exports the image again, and the intermittent ExecuteOnlySpecialFiles failure is only recorded as a known flaky test.
- **Watch out**: A case that relied on files an earlier case or run left in `/root` or `/tmp` now finds them gone.
- **Source**: 5d23297, f6a82cc, 84ff4a7, 6d9a9e8, f5c09f7

### D28: tuxport-run runs only on Linux hosts
- **What**: Native macOS is not a host for `tuxport-run` (hal D1), so the map's open question and its macOS console-size bug (`TIOCGWINSZ`) are gone; a Mac remains a development machine through `scripts/dev` and difftest's APFS lanes.
- **Why**: The harness map's open question whether native macOS is a supported host for `tuxport-run` had its answer: it is not.
- **Tradeoffs**: `tuxport-run` cannot be tried on a Mac outside the container.
- **Watch out**: None for CI, which never built macOS.
- **Source**: 7c5e636

### D29: One difftest runs at a time, and a case whose suite is not built reports SKIP
- **What**: The top-level run makes `target/difftest/.lock` a symbolic link naming its pid before it touches the store and a second run waits, checking the holder with `ps -p` and taking a dead lock (or an earlier version's lock directory) over by moving it aside and putting back a newer lock it moved by mistake (lanes run under their parent's lock); a case calling `run.sh <suite> "$SKIP" <programs>` is reported SKIP, counted but not failed, when `target/<suite>-tests` has no tarball, and fails when the tarball lacks a named program that SKIPPED.txt does not list as not building.
- **Why**: Runs share `target/difftest` (per-case outputs, lane claims, lane clones that `-r` deletes), so a second run started during one made both report nonsense, and a case without its suite printed the same "not installed" line on both sides and passed, so verifying a fix by removing its skips proved nothing locally; the first lock, a directory made before its pid file, could not tell a killed run from one starting, let two waiters take a dead lock together and took another user's live run (`kill -0` failing with `EPERM`) for dead.
- **Tradeoffs**: Runs are serialized, so another tier or an agent waits instead of sharing the VM; three waiters can still run together when one moves aside a lock another has just made and a third makes its own before it is put back; SKIP keeps exit status 0, so a missing suite is visible only in the output.
- **Watch out**: A pid reused by an unrelated process after a run was killed keeps the lock held until that process ends (`rm target/difftest/.lock` frees it), the host needs `ps -p`, and a case written in another shape than `run.sh <suite> "$SKIP"` escapes the absent-program check.
- **Source**: 3c6bf5e, 300d384

### D30: Container engines are surveyed on their native drivers with storage of their own
- **What**: Surveys of their own (podman-rootless, podman-overlay, docker-overlay, podman-rootless-overlay, docker-rootless-overlay) give each engine its own storage root, run root, data root and socket, the overlay ones on a tmpfs natively because docker's overlay root cannot be an upper layer, and check probes, changes over image layers, diff, commit, build, volumes (`:O` included), whiteouts, `user.overlay.opaque` and start time; rootless docker runs dockerd inside RootlessKit's host network with containers on `--network none`.
- **Why**: Whether the engines really use Tuxport's overlay, including rootless podman's own `userxattr` mounts and rootless containerd's layer extraction, needs a run of each with its native driver compared with native Linux, without disturbing the vfs surveys' images.
- **Tradeoffs**: The surveys need the network for image pulls and are compared by reading their output, and neither rootless docker's networking nor its usual start through dockerd-rootless (which needs a TUN device Tuxport lacks) is tested.
- **Watch out**: `docker build` fails on both sides under rootless docker ("driver not connecting"), so builds there stay unchecked, and engine, RootlessKit or dockerd-rootless updates can change probe output or break the manual start.
- **Source**: 5d7b936, 450fb00, 869de68, 8089088

### D31: Overlay gtests run on a tmpfs TEST_TMPDIR, picked by a positive filter
- **What**: The starnix-overlay case mounts a tmpfs as `TEST_TMPDIR` and runs Starnix's overlayfs_test and fs_test's overlay tests in one `run.sh` call limited by the runner's `ONLY` gtest filter, starnix-fs skips fs_test's OverlayFs tests, gvisor-fs keeps only the sysfs overlay test skipped, and an overlay-upper case covers the upper layer.
- **Why**: These tests could not run natively because docker's root is an overlay, which Linux refuses as an upper layer, and gVisor's overlay mount tests were skipped while Tuxport had no overlayfs.
- **Tradeoffs**: A case can still call `run.sh` only once, since difftest reads the suite name from that call.
- **Watch out**: Three overlayfs_test tests fail on both sides in docker and so compare equal without checking anything.
- **Source**: 4b4af75

### D32: What native Linux cannot show is checked by guest programs in cargo tests
- **What**: Tuxport's own reports and internals are checked by `tests/guest` programs that `rootfs_programs` and `guest_programs` run rather than by differential cases: exception reports against the lines Linux writes with Tuxport's synthesized ESRs (`crashlog.c`, kernel/signal D23), unimplemented paths logged once (`bugs.c` and a test reading `tuxport-run`'s stderr, kernel/signal D24), a 2.5 s BKL hold through test hook 12 (`stall.c`, kernel/process D38), a host spawn whose thread start fails through test hook 13 followed by a spawn on the same console (`bugs.c`, kernel/process D53), readv on `tuxport-run`'s host stdio answered as input arrives, within 20 s per answer (`stdinreadv.c`, kernel/vfs D22), clones sharing data until written (`reflink.c`, kernel/store D17) and a shootdown race forced every round (`cowspin.c` under a 60 s limit, kernel/mm D39), the last four at both tiers.
- **Why**: The native side cannot set a global sysctl from a container, differs in ESRs, CPU numbers and pids, answers `FICLONE` with `EOPNOTSUPP` on ext4 and has no BKL watchdog or host stdio, a hang seen only under full-LTP load needs a reproduction that fails every time, and thread creation cannot otherwise be made to fail, nor spawn be called on a console outside a Machine test.
- **Tradeoffs**: These programs encode Tuxport's own formats and are not compared with native, the watchdog and spawn-failure tests run only in debug builds (the watchdog ones adding about 3 s each), `stdinreadv.c` checks only the read side (a writev's atomicity against other writers is not observable deterministically), `reflink.c` checks contents and errors rather than the sharing itself, and python.sh now prints the same line for a correct clone and for `EOPNOTSUPP`.
- **Watch out**: A slow runner skips crashlog's ratelimit item with only an `ok ratelimited (skipped: too slow)` line and could approach cowspin's 60 s limit or fail `stdinreadv.c` by taking 20 s for one answer, crashlog's items depend on engine alignment checks that could change, and a thread start between arming hook 13 and the host's spawn would take the failure instead.
- **Source**: 7b7f73b, 99b4216, 64a6107, df6ee70, f51f688, a36860b, 143bb86

### D33: Skipped suite tests are re-run on both sides and unskipped when they match
- **What**: Every skipped Starnix and gVisor test except the slow, hanging and incomparable ones was run on both sides, those matching natively were unskipped (narrowing wildcards where only some instances still differ), and a test that passes alone but fails within its whole case stays skipped.
- **Why**: Skips added with the suite cases (70c1bc6, ce8a9ce) outlived the fixes for their causes, so about 140 passing tests guarded nothing and several skip comments named causes that no longer existed.
- **Tradeoffs**: The suite cases run about 140 more tests, and tests that skip themselves on both sides now run only to report that.
- **Watch out**: The unskipped tests were compared only against colima's Linux 6.8, so CI's newer native kernel may answer some of them differently.
- **Source**: f713148

### D34: FUSE is compared with native through real daemons and one written in the case
- **What**: difftest installs fuse3, fuse-overlayfs, sshfs, openssh-sftp-server and rclone into its store; `fuse` drives fuse-overlayfs (I/O, namespace and attribute changes, xattrs, programs run from the mount, mappings read on demand, errors and unmount) and a Python daemon of its own that counts lookups not forgotten after `drop_caches`, which a `settle` helper polls every 0.1 s for up to 5 s until they equal the counts Linux leaves, and `fuse-tools` mounts sshfs against a local sftp-server as root and through fusermount3, and rclone (kernel/fuse D1).
- **Why**: FUSE behaviour has to match a real kernel with the daemons people use, and forgets need counts no packaged daemon reports.
- **Tradeoffs**: The store gains five packages, the native side needs `--device /dev/fuse`, the case writes `drop_caches` on the whole docker VM, interrupts, oversized requests, notifications and a refused INIT are covered only by unit tests or not at all, and the counts Linux leaves are written into the case, so a change in which nodes Linux keeps needs the case edited.
- **Watch out**: Alpine's sshfs and rclone versions change what the cases see, and a forget taking more than 5 s on a loaded runner would still fail the group.
- **Source**: c008749, 3957a03, 8f50f11

### D35: The console's password-input query is tested on a real console
- **What**: `console_tells_password_input` in rootfs_programs.rs expects false on a new console, true after `stty -echo`, false in non-canonical mode and with echo back, and `ENOENT` once the console is closed.
- **Why**: The termios rule behind the lock cursor (kernel/tty D23) needed a test on a real console rather than a unit test in tty.rs or an Android instrumentation test.
- **Tradeoffs**: Skipped without the Alpine rootfs tarball, and runs on Tier 0 only.
- **Watch out**: It relies on busybox's `stty` and `read` in the Alpine rootfs.
- **Source**: 2b04a43

### D36: Cases compare only what CI's native kernel and the development VM both answer
- **What**: Where CI's native side (kernel 7.x, SVE and AArch32 at EL0, 1 MiB `rmem_default` and `rmem_max`, `fs.suid_dumpable` 2, docker's stack limit, overlayfs defaulting to `redirect_dir=on`) answers otherwise than colima's 6.8, cases compare relations (buffer bounds, the dumpable flag against `fs.suid_dumpable`) or name 6.1's options (`redirect_dir=follow`), checks newer kernels answer otherwise move to `tests/guest/linux61.c`, which `rootfs_programs` runs on both tiers (D11), and about 170 suite test instances are skipped under their cause.
- **Why**: full.yml's differential tests failed every run from 2026-11-09, mostly because CI's native side answered differently, not Tuxport.
- **Tradeoffs**: The skipped suite tests compare nothing now (eight of the syslog ones comparable on both sides), and 6.1's answers are checked only in Tuxport and natively on 6.8, not on CI's kernel.
- **Watch out**: The runner image can change its kernel or settings again, the namespaces timeout may be a real pid-namespace teardown race, and the one-off CI failures of namespaces and netmap are recorded as known flaky tests in `docs/milestones.md`.
- **Source**: ba6200f

### D37: CI saves the suite caches by themselves and uploads the differential tests' diagnostics
- **What**: full.yml restores each suite build's cache before it and saves it as soon as a missed build finishes (actions/cache/restore and save), samples vmstat every 10 s during the differential tests (stopped by a trap however the step ends), and uploads every case's `.tuxport.err`, the failed cases' outputs and the samples as the `difftest` artifact whatever the result, removing the store in a later step.
- **Why**: actions/cache saved only when the whole job succeeded, so the Starnix and gVisor builds ran on every failing run, and stalls seen only on CI cost 23–35 minutes a run with nothing kept to show why.
- **Tradeoffs**: Every run uploads an artifact, and parallel lanes stay off on CI, since four lanes made timing-sensitive cases flaky on the development VM.
- **Watch out**: A stall that holds no BKL leaves no watchdog report, only vmstat's view of the runner.
- **Source**: 4b5f458

### D38: `difftest -R` reruns failed groups once and passes those that pass as flaky
- **What**: `-R` collects the failed groups (from the single-lane loop, the lanes' report lines and lanes that never reported) and reruns each once, alone on the main store after all the others; one that passes prints `flaky <name>`, counted in the summary and not failed, one that fails again prints its second report too, the first failure's outputs stay in `target/difftest/failed/` for full.yml's upload (D37), and full.yml runs `scripts/difftest -R`.
- **Why**: On CI's one lane a single group failing only sometimes failed the whole 30-minute run, and telling it from a regression meant starting another run by hand.
- **Tradeoffs**: A regression that fails only some of the time passes CI as flaky, with the `flaky` lines and the uploaded first outputs as its only trace, and each failure costs one more run of its group.
- **Watch out**: Flaky reports need reading or intermittent Tuxport bugs go unnoticed (the FUSE forget failure, kernel/fuse D2, was one), and a group whose first failure leaves state in the shared store can fail its retry for that alone.
- **Source**: 4083007

### D39: The gtest runner dumps a hung test's threads with their syscalls
- **What**: `tests/diff/gtest/run.sh` (D19) writes each test's name to `/tmp/gtest.cur` with the builtin echo and runs it in the foreground; a background watcher checking every 5 s writes a test seen for 45 s to `hang.out`, one `hang:` line per thread of its process tree (the processes whose command line has `--gtest_filter=<test> ` and their descendants) with pid, tid, name, state and its `syscall` file (kernel/procfs D40), and the main loop prints those lines after the test's line only when it exited 137 and `hang.out` names it.
- **Why**: CI's hangs left only `exit 137`, and a dump taken while the test still hangs shows which syscall each of its threads waits in.
- **Tradeoffs**: One sleep and one date every 5 s for the whole run, a test can see one more process in the system, and on the native side Yama makes most `syscall` files unreadable (the watcher is no ancestor), so only Tuxport's dumps are useful.
- **Watch out**: A test that kills its process group or every process stops the watcher for the rest of the run, and a dump finishing just after its test ended leaves a stale `hang.out` that only the name check keeps from the next test.
- **Source**: f1212d8

### D40: TCP filter fallback has forced-host and native comparison coverage

- **What**: A both-tier test-hook guest program forces TCP attachment refusal, compat9 checks filter lengths, inheritance, detach and locks, and gVisor drops the fixed attach/detach skip while retaining the incomparable capability case.
- **Why**: Host-dependent refusal otherwise left the retained-program path untested locally.
- **Tradeoffs**: Native differential coverage depends on whether the host refuses TCP filters.
- **Watch out**: If CI stops refusing attachment, only the forced hook test exercises retained programs.
- **Source**: 52dcc177; 20137e4c; d053be24; 39e55b4b

### D41: compat10 groups socket regressions while Tuxport-only queue limits stay in guest tests

- **What**: Privileged compat10 compares TCP MSG_TRUNC/partial writes, IPv4/IPv6 route ioctls, accept4 reservations/timeouts and DNS-stub endpoints, while net.c bounds DNS queued answers by SO_RCVBUF plus one datagram.
- **Why**: The socket fixes need Linux comparisons, but native Linux cannot expose Tuxport’s internal answer cap.
- **Tradeoffs**: Large send can agree without exercising the fix, route tests omit unsupported unreachable/host routes, and the in-flight DNS cap lacks an independent test.
- **Watch out**: Shutdown/accept cases rely on 300/200 ms scheduling and the queue case waits 1.5 s, which can hide undelivered answers on slow runners.
- **Source**: 211a5867, 29e59fd3, 4e01f78b, 73127176; 20137e4c; d053be24; 39e55b4b

## Superseded

- ~~Line-discipline scenarios skipped by group until fixed (24ecd25)~~ → replaced by **D18** in 8dc166b (2026-11-07)
- ~~Each Starnix test captured with `$(timeout 60 …)` (1459292, kept in 612d238)~~ → replaced by **D19** in c11c1db (2026-11-07)
- ~~Starnix cases with their own runner loops, only mmap_test and the waitpid programs, and no CI build (1459292)~~ → replaced by **D19** in 70c1bc6 (2026-11-07)
- ~~A Starnix-only runner at `tests/diff/starnix/run.sh` (70c1bc6)~~ → replaced by **D19** in ce8a9ce (2026-11-07)
- ~~Cases clean up what they leave in the reused lane store's `/tmp` themselves (5d23297, f6a82cc, 84ff4a7)~~ → replaced by **D27** in f5c09f7 (2026-11-09)
- ~~difftest's lock as a directory holding a pid file, checked with `kill -0` (3c6bf5e)~~ → replaced by **D29** in 300d384 (2026-11-10)
- ~~`stale.c` reproduces the stale deadline through an unmapped timeout and prints only the later 3 s wait (9593bae)~~ → replaced by **D13** in 27609fe (2026-11-10, no SUPERSEDES declared)
- ~~CI caches `target/ghostty` and `~/.cache/zig` by fetch-ghostty's hash (D10, b38d29c)~~ → replaced by **D10** in 172be7c (2026-11-11)
- ~~Manual full.yml runs also use master's copy (D24's watch-out, 0202f1e, 48dce39)~~ → replaced by **D24** in 4083007 (2026-11-11)
- ~~The native side's forgets must arrive within the 1 s the case sleeps after `drop_caches` (D34's watch-out, 3957a03)~~ → replaced by **D34** in 8f50f11 (2026-11-11)
- ~~D9 combined ownership of the newly extracted child modules~~ → replaced by **D9** in 60790c14 (2026-11-15; scope only, inherited implementation decisions remain here)
