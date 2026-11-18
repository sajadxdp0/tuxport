# Fuchsia Starnix as a reference

Starnix is Fuchsia's Linux syscall layer, written in Rust. It was surveyed twice for code, tests
and ideas Tuxport could use: on 2026-11-06 at Fuchsia 86962cb and on 2026-11-07 at 06ce521f.
Almost none of its code can be moved over, because it is built on Zircon, FIDL and `starnix_core`,
and its execution model (Zircon restricted mode) has nothing in common with a soft MMU. What pays
is its test suites and its reading of Linux semantics. The deviations its tests found are recorded
in the module maps, not here.

## Code

- **Usable on its own:**
  - `lib/line_discipline` depends only on `starnix_uapi`; its algorithms and its 83 recorded PTY
    scenarios were used.
  - The arm64 assembly of `lib/usercopy` is a hermetic copy, but its fault handling would have to
    become the pc-range fixup of the engine's host fault guard.
  - The runtime lock-order checks of `starnix_sync`.
  - `lib/ebpf`, which has no use yet.
- **Not usable:**
  - `starnix_rcu` needs Fuchsia's `fuchsia_rcu`. Tuxport's no-BKL path resolver already validates
    its reads with change counters.
  - The vendored `rust_netlink` crates would add dependencies. Tuxport's netlink encoders are small
    and checked byte for byte by the differential tests.
  - The FUSE, overlayfs, userfaultfd and perf modules are written against its `FsNode`/`FileOps`
    and Zircon.
- **Locking:** Starnix's per-object locks are no argument for replacing the BKL (kernel/process D2).
  It has some of the same signal bugs as Tuxport.

## Tests

- **LTP:** the device tests run on tmpfs over loop devices, with `LTP_DEV` plus
  `LTP_DEV_FS_TYPE=tmpfs`, as Starnix runs them. LTP passes went from 1014 to 1081.
- **PTY scenarios:** the 83 recorded scenarios are the `line-discipline` differential case. All of
  them pass since the line discipline follows n_tty.
- **`tests/syscalls/cpp`:** `scripts/build-starnix-tests` builds 81 of its 93 gtest programs
  statically against musl, and CI builds them too. 65 programs run in seven differential cases by
  subsystem (`starnix-mm`, `-process`, `-signal`, `-fs`, `-proc`, `-io`, `-overlay`) through a
  shared runner, each skipping by reason the tests that differ for a deviation recorded in a map
  or cannot be compared; `starnix-overlay` puts its layers on a tmpfs, since Linux refuses
  docker's overlay root as an upper layer. The other 16 are listed with their reasons in
  `tests/diff/gtest/run.sh`: they would reboot, suspend or set the clock of the VM, test what
  Tuxport does not implement (bpf maps, perf, userfaultfd, tun), need devices, or have nothing to
  compare.
- **Full run of the other programs (2026-11-07, at 8dc166b):**
  - About 70 more programs ran one test per process, as root, on native Linux 6.8 and on Tuxport.
    Of 2484 tests, 1186 passed on both sides, 532 passed only natively (203 of them in
    `bpf_test`) and 138 were skipped only on Tuxport, mostly for missing sysctls.
  - The failures came down to about 45 unrecorded causes, now in the maps of kernel/process, mm,
    signal, net, tty, cred, poll, procfs and vfs, plus causes the maps had already recorded
    (propagation, PI futexes, absent proc files, the vvar page, the decided `ENOSYS` syscalls).
  - The biggest single causes were a thread-group leader that exits before its threads vanishing
    from `/proc/<pid>/task` (35 tests) and missing or misjudged sysctls (about 170). Both are
    fixed, with seven smaller causes (see "Taken so far").
  - Excluded:
    - behaviour newer than 6.1 (the io_uring sysctls of 6.6, a setresuid test that needs 6.3);
    - docker's environment (the `RLIMIT_NOFILE` hard limit, `/proc/2`, input devices);
    - eight cgroup tests that failed only because an earlier hung test left its directory behind;
    - a pagemap test that crashed only after its frame-number checks had already failed.
- **Second full run (2026-11-07, at fcf4c93), which the cases were cut from:**
  - All 77 programs that do not act on the VM, at Tier 1. Leaving out `mmap_test` and the waitpid
    programs, as the first run did: of 2500 tests, 1294 passed on both sides, 560 only natively
    and 31 were skipped only on Tuxport. 267 of the native-only passes are in programs now left out
    of the cases (203 in `bpf_test`); the rest are skipped in the cases by reason.
  - The sysctl fixes turned most skips into runs: `/proc/sys` permission tests that used to skip
    now fail for the recorded DAC deviation.
  - New causes, recorded in the maps: four `sched_setattr` details left by bc44b65 (kernel/process),
    a zombie's `mounts` and an exited main thread's `pagemap` taken from someone else (procfs; the
    zombie test read that pagemap until the OOM killer ended it), no superblock flags and
    propagation changes refused on detached mounts (vfs).
  - Native-side failures that are docker's or the VM's, not Linux's: the kernel log wrapping under
    docker's messages (`/dev/kmsg` reads from the start), docker's pid namespace (a dead lock
    owner's pid is 0), its device cgroup (mknod), its network namespace (no `net.core.rmem_max`),
    and a test that leaves lo's address changed for the next one.
- **gVisor's syscall tests** (Apache-2.0), taken after this survey recommended them:
  - `scripts/build-gvisor-tests` builds them without Bazel against the abseil, googletest and
    google benchmark versions gVisor pins; 264 of the 266 programs build for arm64 (the other two
    have x86 sources only). Four `gvisor-*` cases run a first set of the programs, chosen from
    Tuxport's weaker areas (processes, signals and ptrace; files, terminals and mounts; inet
    sockets and netlink; AF_UNIX); the harness/diff map has the details.
  - A full run at gVisor 9ee2906 (2026-11-07): of about 14,500 tests, about 11,200 passed on both
    sides and about 2,500 only natively, many of them one cause repeated over parameters (the
    AF_UNIX socket-option and control-message tests run once per socket kind).
  - About 40 unrecorded causes went into the maps of kernel/process, signal, vfs, net, netns, unix
    and poll. The most serious: a recursive-bind loop with no `fs.mount-max` holds the BKL for
    ever longer binds and freezes every guest process; mounts inherited by a less privileged
    namespace are not locked; a UDP socket shut down for reading spins in recv; and a bind with
    `AF_UNSPEC` passed to the host oopses the development VM's Ubuntu kernel in AppArmor, leaving
    Tuxport hung. All four are fixed (see "Taken so far").
- **Not yet used:**
  - **kselftest**: mostly GPL, so download it at build time. Of 456 sources in the relevant
    directories, 311 pass a raw syntax check under musl. Of those directories, clone3,
    pid_namespace, ptrace and timens all pass; futex, pidfd, proc and timers almost all do.

## Taken so far

| Commit | What |
|---|---|
| a26c8b4 | Loop devices; LTP's device tests on tmpfs |
| 24ecd25 | Starnix's PTY scenarios as a differential case |
| b295644 | readv/writev as one operation on pipes, terminals and sockets |
| 5dcf7db | mremap with an old length of 0 duplicates shared mappings |
| 63b9e59 | Wall-clock deadlines stay on their clocks across clock jumps and sleep |
| 1459292 | `scripts/build-starnix-tests`, the `starnix-mmap` and `starnix-wait` cases |
| e03c0cc | Linux's VMA boundaries, `PROT_GROWSDOWN` and stack growth (the gaps `mmap_test` showed) |
| 8dc166b | The line discipline follows n_tty |
| 4e64e74 | The full run's deviations recorded in nine maps |
| 09188ec | Record-lock ranges at the edges of `off_t`, among other arguments at the edges of their types |
| 42d2ffe | The common sysctls; root can write the read-only ones |
| 79a60cf | An exited main thread stays in `task/` as a zombie until the process is reaped |
| 60921b2 | `/proc/vmstat` |
| 97b1d62 | `RLIMIT_MSGQUEUE` starts at 819200 bytes |
| bc44b65, e43c40c | `sched_setattr`'s keep and clamp flags |
| cbf7267 | membarrier registrations survive fork |
| 6591eb6 | Thread names shown as their bytes |
| 566da95 | `RssAnon`, `RssFile` and `RssShmem` in `status` |
| 0b2c3c8 | inet binds check the address family before the host sees it (the AF_UNSPEC oops) |
| 0a4712e | `fs.mount-max`; recursive binds, unmounting and mount lists linear in the table size |
| b3e134a | A UDP socket shut down for reading returns 0 from a blocking recv instead of spinning (with netlink dumps that fit a page) |
| 5530624 | splice, tee and vmsplice wait as Linux 6.1 does (seven skipped Starnix pipe and vmsplice tests now pass) |
| 6d05aa9 | Mounts inherited by a less privileged namespace are locked; proc and sysfs mounts check owner and visibility; `max_*_namespaces` enforced |
| 2a244d2, 4b4af75 | overlayfs with several lower layers and an upper one, after Linux 6.1 rather than Starnix's single-lower module; its `overlayfs_test`, fs_test's overlay tests and gVisor's overlay mount tests run |
| 7b7f73b | Unhandled exceptions reported in the kernel log as arm64's `arm64_show_signal` does; `debug.exception-trace` starts at arm64's 0 (the recorded 1 was x86's), the app's kernels at 1 |
| 39ea702 | `process_mrelease`, releasing around fork's copy-on-write (Starnix's `process_mrelease_test` runs in `starnix-mm`); `/proc/<pid>/oom_score`, also in thread directories, with vfork children at 0 and no longer picked by the OOM killer; `/proc/swaps`; `/proc/locks` and fdinfo's `lock:` lines, which lslocks needs for OFD locks; `NETLINK_SOCK_DIAG` refusing every query, so ss reads `/proc/net` quietly |
| 99b4216 | Unimplemented paths counted as `track_stub!` and `not_found` count them: the `ENOSYS` arm, the default arms of prctl, fcntl, ioctl, madvise and futex, names Linux would have that are missing under `/proc`, `/sys` and `/dev`, and devices without a driver; the 10 called most go into kernel-bug reports and the app's copied report, and first hits into `TUXPORT_LOG` and the survey. Also `FIONREAD` on regular files, the first gap the counters showed |
| 64a6107 | A BKL watchdog: a host thread that never takes the BKL reports a hold over 10 s (`TUXPORT_BKL_WATCHDOG`) with the guest process, thread, syscall or exception and pc, and whether the holder's host thread is running or in which host syscall; on stderr and in a notification with the bug report at once, and in the kernel log with its length once it ends. Each holder records itself on taking the lock, with no visible cost on the BKL path |
| df6ee70 | Store reflinks: `FICLONE`, `FICLONERANGE` over a whole file, a whole-file `copy_file_range` and overlay copy-up make files share one store object, and the first change to one (any write, truncation, `fallocate`, a writable shared mapping) gives it its own, copied without the BKL; its open descriptions and shared mappings move along. Other clone ranges are copied; tmpfs, overlay files and memfds stay `EOPNOTSUPP` |
| c008749 | FUSE after Linux 6.1's `fs/fuse` rather than Starnix's module: `/dev/fuse`, `mount -t fuse[.subtype]` (and through the setuid fusermount3), INIT, interrupts and aborts, about 30 opcodes; walks read a node cache and a miss makes the dispatcher ask the daemon with the BKL released and run the syscall again; no page cache (mapped pages are read from the daemon as they are touched, programs are copies), unused nodes forgotten above a limit or on `drop_caches`; fuse-overlayfs, sshfs and rclone mount in the `fuse` and `fuse-tools` cases |
| 16364c0 | vDSO clock reads, from the host kernel's own vDSO rather than a vvar page of Tuxport's (whose parameters would drift from the host's NTP-adjusted clocks): its code is copied into each process with the host's vvar data mapped read-only below it as `[vvar]`, after a check in the interpreter at first use, with the svc-stub image as fallback; `clock_gettime` at Tier 1 went from 90 to 28 ns (19 natively). Time namespaces with offsets get svc stubs at the clock entries; tracers and seccomp see only the clocks Linux's vDSO leaves to the kernel; ECV's `CNTVCTSS_EL0` |
| 1e5de64e | Stay awake while busy: Settings offers allow sleep, run while busy and always run; busy mode releases the Android wake lock after 30 seconds without observed guest CPU activity, checked every 2 seconds, while protecting startup, boot, restart, replacement, shutdown and distro downloads. The notification shows the selected policy and toggles always run against the last chosen non-always policy. Network waits and scheduled work can still need always run; polling does not wake a sleeping device |

b295644, 5dcf7db and 63b9e59 fix bugs found while comparing Starnix's code with Tuxport's: readv
and writev split datagrams per iovec, mremap with an old length of 0 was always `EINVAL`, and
absolute timers were converted to the host's monotonic clock when set. The commits after 4e64e74
fix deviations the full runs found (0b2c3c8, 0a4712e, b3e134a and 6d05aa9 those of gVisor's;
b3e134a's netlink dumps and 6d05aa9's namespace limits came from the maps instead), except
7b7f73b, 39ea702, 99b4216, 64a6107, df6ee70, 16364c0, c008749 and 1e5de64e, which take ideas from
the list below.

## Not yet taken

The host's activity-based wake-lock policy is implemented in 1e5de64e. The remaining power work
is guest-facing interfaces:

| Idea | What Starnix does | For Tuxport | Effort |
|---|---|---|---|
| Guest wake locks and battery information | Explicit wake locks through `/sys/power/wake_lock` | Let guest programs request wake locks through `/sys/power/wake_lock`; expose Android battery information under `/sys/class/power_supply` using `ACTION_BATTERY_CHANGED` | Medium |

## Not worth taking

- **userfaultfd:** Starnix supports only the `SIGBUS` mode for Android's runtime. On Linux its users
  are CRIU and VM migration, and docker's default seccomp profile blocks it.
- **perf_event_open:** Starnix builds it on a Zircon job sampler and stubs the counters; perf and
  async-profiler fall back to itimers without it.
- **process_madvise:** Linux 6.1 takes only hints (`MADV_COLD`, `MADV_PAGEOUT`, `MADV_WILLNEED`,
  `MADV_COLLAPSE`) that Tuxport's own madvise ignores or refuses (no swap, no transparent huge
  pages), so it would only check arguments and permissions (ptrace read access, `CAP_SYS_NICE`).
  It was made for Android's ActivityManager and no Linux userspace caller was found; Starnix
  itself answers `ENOSYS` (at 33c46967, 2026-11-08), docker's default seccomp makes it `EPERM`,
  and LTP's `process_madvise01` needs `CONFIG_SWAP`.
- **PSI beyond empty files:** Starnix has real data only for memory. A zero-filled
  `/proc/pressure` would at most quiet the tools that refuse to start without it.
- **Alarm clocks through AlarmManager, and a `CLOCK_MONOTONIC` that counts suspend:** the first
  needs exact-alarm permission and is not part of the run-while-busy policy. The second changes
  the 6.1 contract.
- **Netlink stubs that only answer `NLMSG_DONE`:** Tuxport stopped doing this for unsupported
  families in round 83.
- **Guest-initiated suspend, container freezing, `wakeup_count`:** Android decides when the phone
  sleeps.
- **tun:** Starnix's `modules/tun` (looked at on 2026-11-08, at 7e70df49) only handles
  `TUNSETIFF`, which asks Fuchsia's netstack3 over FIDL for an interface; `read` and `write` return
  `EOPNOTSUPP`, and its one test only checks that the interface name appears. The packets are
  netstack3's work, in a separate component. Tuxport has no network stack to attach a device to
  (kernel/netns map), so TUN would need one in the kernel layer (smoltcp is the candidate) plus a
  second socket backend chosen by route, and policy routing for full-tunnel VPNs. Its main user,
  rootless podman through pasta, can run as root instead, where bridge networks work.
- **io_uring:** already rejected (docs/design/15-rejected.md).
