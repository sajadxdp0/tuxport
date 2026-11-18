# Tuxport milestone records

Each round (the milestones, and the hardening rounds after M8 and before M9) gets one file with its conclusions, measurements, implementation notes and deferred items, numbered in order of completion under [rounds/](rounds/). The rounds are written in Chinese. This page is the index; it also records the [measurement conditions](#measurement-conditions) the rounds share, the [known flaky tests](#known-flaky-tests) and the [R1 measurements](#r1-measurements). The milestone plan and the reasons for its order are in §14 of the [design doc](design/14-milestones.md).

Rounds stopped after round 208 (2026-11-04). Since then each change is recorded in its commit (Decision blocks as described in [AGENTS.md](../AGENTS.md)) and in the module maps in `.agents/maps/`, and the rounds are frozen history.

- Each file is a record of its time. Where an approach changed later, sections 1–13 of the design doc and the module maps in `.agents/maps/` are authoritative (migrated sections were removed from the design doc: §4 CPU engine → [engine](../.agents/maps/engine.md), §5 memory → [kernel/mm](../.agents/maps/kernel/mm.md), §6 processes → [kernel/process](../.agents/maps/kernel/process.md), §7 signals and ABI → [kernel/signal](../.agents/maps/kernel/signal.md), [kernel/ptrace](../.agents/maps/kernel/ptrace.md), [kernel/vdso](../.agents/maps/kernel/vdso.md), §8 file system → [kernel/store](../.agents/maps/kernel/store.md), [kernel/vfs](../.agents/maps/kernel/vfs.md), [kernel/procfs](../.agents/maps/kernel/procfs.md), §9 TTY and terminal → [kernel/tty](../.agents/maps/kernel/tty.md), §10 networking → [kernel/net](../.agents/maps/kernel/net.md), [kernel/netns](../.agents/maps/kernel/netns.md), [kernel/nft](../.agents/maps/kernel/nft.md), [kernel/unix](../.agents/maps/kernel/unix.md), [kernel/poll](../.agents/maps/kernel/poll.md)). The design doc describes only the current approach: what matches Linux gets one sentence, and only Tuxport's own mechanisms and its differences from Linux are spelled out; how changes came about, measurements and sources went in the rounds (now in the commits), not in the design doc.
- A deferred item (or anything the text leaves for later) is followed by "→" naming the round that later did it, tried it and dropped it, or did only part of it; a partly done item says what remains. An item without "→" has not been done. In the "改动和推迟项" (changes and deferred items) sections of M3b–M8, entries marked "（修改 §x）" (changes §x) are design changes, not deferred items. No "→" is added any more: a deferred item that comes up again is checked against the current code and, if it is still open, moved into its module map ("Confirmed bugs / technical debt" or "Open questions").
- §x in a round means the design doc section as it was when the round was written (for sections migrated to maps, the corresponding map is authoritative); R1–R6 and A1–A3 are the risk numbers in §16 of the design doc.
- This page stays maintained for what spans changes: when a change gives a new ratio, add a row to the R1 measurements; add newly found flaky tests to the known flaky tests.
- Structure of each file: an untitled lead paragraph with the motivation and the conclusion, then the four `##` sections 「做法」 (approach), 「结果」 (results), 「测试」 (tests) and 「推迟项」 (deferred items), leaving out any that would be empty (in M3b–M8 the last section is called 「改动和推迟项」). Investigation and experiment sections are named after their content (such as 「原因」 cause, 「时间花在哪里」 where the time goes, 「试过没有采用：……」 tried and not adopted: …) and go before 「做法」. Subtopics within a section are `- **标签**：` list items; large blocks use `###`.
- The tests section of each file records only which aspects were tested, the problems found while testing, and pitfalls worth knowing later; it leaves out individual cases, mutation checks, and the routine results of cargo test, clippy and LTP.

| Round | Date | Key points |
|---|---|---|
| [M2 results](rounds/01-m2.md) | 2026-10-24 | Geometric mean of the prototype Tier 1 on the integer benchmarks 1.85×, 2.37× on Neoverse N2 |
| [M1a results](rounds/02-m1a.md) | 2026-10-24 | SIMD&FP, LL/SC, LSE; lockstep extended to a three-way comparison |
| [M1b results](rounds/03-m1b.md) | 2026-10-24 | busybox runs through `ld-musl`; `gzip` 4.75× |
| [M3a results](rounds/04-m3a.md) | 2026-10-24 | Multiple processes and threads, the BKL, signal frames, vDSO |
| [M3b results](rounds/05-m3b.md) | 2026-10-24 | In-process PTY, job control, procfs |
| [M4 results](rounds/06-m4.md) | 2026-10-24 | Networking, DNS stub, AF_UNIX; `apk add` and `git clone` work; R5 closed |
| [M5 results](rounds/07-m5.md) | 2026-10-24 | Android app; passes on 4K and 16K page emulators |
| [M6 results](rounds/08-m6.md) | 2026-10-25 | Building Lua and SQLite about 10×; credentials, record locks, IPC, timers; LTP 849 |
| [M7 results](rounds/09-m7.md) | 2026-10-25 | Production Tier 1 (SIMD, 4096-entry TLB, W^X); builds 4.7–6.1× |
| [M8 results](rounds/10-m8.md) | 2026-10-25 | Go and Node.js behave correctly; LTP 856 |
| [M8 follow-up: JVM](rounds/11-m8-jvm.md) | 2026-10-25 | OpenJDK 21/25, Maven and Gradle work; inotify added |
| [Pre-M9: stability and memory](rounds/12-stability-memory.md) | 2026-10-25 | Truncating mapped files, `PROT_NONE` kept out of page tables, robust futex |
| [Pre-M9: profiling](rounds/13-profiling.md) | 2026-10-25 | Metadata cache, fork looks only at touched pages; `git status` 19× → 6× |
| [Pre-M9: the translation cost of spawning processes](rounds/14-spawn-translation.md) | 2026-10-25 | Translations kept across exec; shell spawning 9× → 6.3× |
| [Pre-M9: the stack pointer in a register and return prediction](rounds/15-sp-return-prediction.md) | 2026-10-25 | SP pinned in X26, shadow return stack; gcc 10% faster |
| [Pre-M9: targeted futex wakeups](rounds/16-futex-targeted-wake.md) | 2026-10-25 | FUTEX_WAKE notifies only the threads it wakes; Gradle warm build 6.1 → 4.8 s |
| [Pre-M9: compatibility gaps](rounds/17-compat.md) | 2026-10-25 | maps naming, real-time signal queuing, pidfd, `execveat`; LTP 889 |
| [Pre-M9: the remaining small gaps](rounds/18-small-gaps.md) | 2026-10-25 | `/dev/shm` cleared at boot, smaps/pagemap, unshare; LTP 905 |
| [Pre-M9: deferred items](rounds/19-deferred.md) | 2026-10-25 | `O_ASYNC`, leases, POSIX message queues, AIO; no io_uring |
| [Pre-M9: robustness](rounds/20-robustness.md) | 2026-10-25 | A kernel bug ends only one process; app memory limit and OOM killer |
| [Pre-M9: finishing robustness](rounds/21-robustness-finish.md) | 2026-10-25 | Panics at thread start and exit, in the timer thread and in JNI; recovery under libsigchain |
| [Pre-M9: the stack size limit](rounds/22-stack-limit.md) | 2026-10-26 | The main thread's stack grows on demand, limited by `RLIMIT_STACK` |
| [Pre-M9: memory-access checks and the translation table](rounds/23-tlb-checks.md) | 2026-10-26 | Accesses off the same base share one TLB check, crossing a page no longer exits; gcc 3.6× |
| [Pre-M9: translations shared between threads and targeted wakeups](rounds/24-shared-translation.md) | 2026-10-26 | Measurements move to a local volume; `go build` 5.4× → 3.2×, Maven 8.5× → 4.4× |
| [Pre-M9: stat without the BKL](rounds/25-stat-without-bkl.md) | 2026-10-26 | `git status` with parallel lstat 24 → 13 ms |
| [Pre-M9: copy-on-write fork](rounds/26-fork-cow.md) | 2026-10-26 | fork of a process that wrote 100 MB 21 → 2.3 ms |
| [Pre-M9: shared checks for interleaved accesses](rounds/27-interleaved-checks.md) | 2026-10-26 | python loop 0.82 → 0.75 s; two other experiments gained nothing |
| [Pre-M9: the host cost of spawning processes](rounds/28-spawn-host-cost.md) | 2026-10-26 | fork then exec 221 → 163 µs (native 154) |
| [Pre-M9: waiting for and holding the BKL](rounds/29-bkl-wait-hold.md) | 2026-10-26 | Spin before waiting for the lock; throughput of 4 processes spawning at once up 40% |
| [Pre-M9: memory accesses in translated code](rounds/30-translated-access.md) | 2026-10-26 | TLB checks are 32% of gcc and 56% of python; python loop −4% |
| [Pre-M9: the superblock experiment](rounds/31-superblocks.md) | 2026-10-26 | Translating on past calls and forward jumps made gcc slower; not adopted |
| [Pre-M9: other workloads and file operations](rounds/32-workloads-file-ops.md) | 2026-10-26 | Ratios for various workloads; batched store commits, `cp -r` then `rm -rf` 2.8× → 1.6× |
| [Pre-M9: the host cost of file operations](rounds/33-file-ops-host-cost.md) | 2026-10-26 | Objects deleted in the background, sendfile handed to the host; `tar x` 2.3× → 1.7× |
| [Pre-M9: node startup](rounds/34-node-startup.md) | 2026-10-26 | Outline atomics handled inside Tier 1; `node -e 0` 12% faster |
| [Pre-M9: JIT workloads](rounds/35-jit.md) | 2026-10-26 | Code invalidated by content; Maven 6.7 → 6.2 s |
| [Pre-M9: parallel processes](rounds/36-parallel-processes.md) | 2026-10-26 | The translation pool holds 16 instances; parallel exec and pipes about 20% faster |
| [Pre-M9: onTrimMemory](rounds/37-trim-memory.md) | 2026-10-26 | Rebuildable caches released when the UI goes away or memory is tight |
| [Pre-M9: splitting the BKL](rounds/38-split-bkl.md) | 2026-10-26 | Clock reads, futex and nanosleep do not take the BKL |
| [Pre-M9: load average, sysfs and limits](rounds/39-loadavg-sysfs.md) | 2026-10-27 | Load average sampling, CPU topology in sysfs, `/proc/<pid>/limits` |
| [Pre-M9: ptrace step one, strace](rounds/40-ptrace-strace.md) | 2026-10-27 | Syscall, signal and event stops and regsets; strace works |
| [Pre-M9: ptrace step two, gdb](rounds/41-ptrace-gdb.md) | 2026-10-27 | Breakpoints, single-stepping, software watchpoints and `catch syscall` match native |
| [Pre-M9: epoll edges](rounds/42-epoll-edges.md) | 2026-10-27 | `EPOLLET` re-triggers when Linux does; tokio programs no longer hang |
| [Pre-M9: core dumps](rounds/43-coredump.md) | 2026-10-27 | ELF cores written per `core_pattern`; gdb can analyze them afterwards |
| [Pre-M9: hardware breakpoints and watchpoints](rounds/44-hw-watchpoints.md) | 2026-10-27 | 6 breakpoints and 4 watchpoints, checked by the interpreter |
| [Pre-M9: ptrace and procfs details](rounds/45-ptrace-procfs.md) | 2026-10-27 | Single-stepping into signal handlers, the mask inside sigsuspend, `/proc/<pid>/mem` |
| [Pre-M9: mremap and sysfs](rounds/46-mremap-sysfs.md) | 2026-10-27 | mremap grows private file mappings; sysfs `cache/` and `cpufreq/` |
| [Pre-M9: nested epoll edges](rounds/47-nested-epoll.md) | 2026-10-27 | Every event on the inner instance re-triggers an edge-triggered outer one |
| [Pre-M9: exclusive wakeups](rounds/48-exclusive-wakeup.md) | 2026-10-27 | One edge wakes only one `EPOLLEXCLUSIVE` waiter |
| [Pre-M9: per-object wakeups](rounds/49-targeted-wakeups.md) | 2026-10-27 | poll, epoll and blocking reads and writes are woken only by the objects they wait on |
| [Pre-M9: watchpoints in Tier 1](rounds/50-watchpoints-tier1.md) | 2026-10-27 | Only watched pages go to the interpreter; gdb `watch` 10.8 → 0.45 s |
| [Pre-M9: spawning and setting timers no longer wake everyone](rounds/51-spawn-timer-wakeups.md) | 2026-10-27 | vfork, timers and signalfd wake only the threads concerned |
| [Pre-M9: signalfd looks at the calling thread](rounds/52-signalfd-caller.md) | 2026-10-27 | poll and epoll look at the calling thread, as read does, not at the creating one |
| [Pre-M9: locks and host sockets wake only their waiters](rounds/53-locks-host-sockets.md) | 2026-10-27 | With other processes sitting in waits, one flock 10 → 0.2 µs |
| [Pre-M9: message queues and signal-driven I/O no longer wake everyone](rounds/54-mqueue-fasync.md) | 2026-10-27 | Message queues wake per object; `O_ASYNC` wakes only the timer thread |
| [Pre-M9: blocking reads on host stdio wait only on the host fd](rounds/55-host-stdio.md) | 2026-10-27 | Affects only `tuxport-run`; the app has no such files |
| [Pre-M9: blocked signals wake only their waiters](rounds/56-blocked-signals.md) | 2026-10-27 | Wake only sigtimedwait and signalfd waiters |
| [Pre-M9: tracing stopped processes, the tracer reaping first](rounds/57-ptrace-stopped-reaped.md) | 2026-10-27 | Report order of group-stop and ATTACH; the tracer reaps the exit first |
| [Pre-M9: a cgroup v2 view](rounds/58-cgroup.md) | 2026-10-27 | A read-only view; the JVM and Node.js size their heaps by `memory.max` |
| [Pre-M9: seccomp](rounds/59-seccomp.md) | 2026-10-28 | Strict mode, cBPF filters, notification fds; `strace --seccomp-bpf` works |
| [Pre-M9: the seccomp cache and fast path](rounds/60-seccomp-cache.md) | 2026-10-28 | Always-allowed numbers skip running the program; futex wake 313 → 60 ns |
| [Pre-M9: each step of process exit caught separately](rounds/61-finish-steps.md) | 2026-10-28 | Each of the nine steps catches kernel bugs on its own; no more processes stuck forever |
| [Pre-M9: workload survey](rounds/62-workload-survey.md) | 2026-10-28 | Most of 20-odd services and tools work; 6 problems found |
| [Pre-M9: pages returned after fork get their own permissions back](rounds/63-returned-loans.md) | 2026-10-28 | Restoring permissions no longer crosses mappings; rustc no longer crashes on exit |
| [Pre-M9: terminal owner and permissions](rounds/64-pty-owner.md) | 2026-10-28 | sshd can hand the terminal to the user; `ssh -t` and mosh work |
| [Pre-M9: shared pages in smaps](rounds/65-smaps-shared.md) | 2026-10-28 | redis and valkey pass their copy-on-write check and start |
| [Pre-M9: io_destroy wakes threads waiting for events](rounds/66-aio-destroy.md) | 2026-10-28 | mariadb's install-db and shutdown no longer hang |
| [Pre-M9: cgroup memory.stat](rounds/67-memory-stat.md) | 2026-10-28 | .NET's GC computes memory load from the whole app's usage |
| [Pre-M9: /proc/meminfo copied from the host](rounds/68-meminfo.md) | 2026-10-28 | Available memory in free and htop matches native |
| [Pre-M9: probing glibc distributions](rounds/69-glibc-probe.md) | 2026-10-28 | Debian 13 and Ubuntu 24.04 rootfs work without kernel-layer changes |
| [Pre-M9: the second survey, language test suites and large projects](rounds/70-survey-suites.md) | 2026-10-28 | Language test suites and large projects compared with native; 6 problems found |
| [Pre-M9: dropping weak references outside the BKL on thread exit](rounds/71-thread-exit-race.md) | 2026-10-28 | Fixed host heap corruption when threads start and exit often |
| [Pre-M9: tombstones missed in the translation table](rounds/72-table-tombstones.md) | 2026-10-28 | Fixed a full translation table leaving a process unkillable even by SIGKILL |
| [Pre-M9: directory and file timestamps](rounds/73-timestamps.md) | 2026-10-28 | Python can import modules created later |
| [Pre-M9: FICLONE returns EOPNOTSUPP](rounds/74-ficlone.md) | 2026-10-28 | Python 3.14's `Path.copy()` works |
| [Pre-M9: PTY TCFLSH and TCXONC](rounds/75-pty-flow.md) | 2026-10-28 | CPython's test_termios and test_ioctl no longer hang |
| [Pre-M9: a FIFO reports no POLLHUP before a writer arrives](rounds/76-fifo-pollhup.md) | 2026-10-28 | Ruby's test_require passes in full |
| [Pre-M9: nothing can be created in a deleted directory, sockets can be fchmod'ed](rounds/77-dead-dir-socket-fchmod.md) | 2026-10-28 | Node's test-fs-mkdir and one other test pass |
| [Pre-M9: gaps in network information](rounds/78-net-info-gaps.md) | 2026-10-28 | Docs only: the gaps in routes, traffic counters and `if_inet6` |
| [Pre-M9: terminal access and modification times](rounds/79-pty-times.md) | 2026-10-28 | Idle times in w and who mean something now |
| [Pre-M9: CPU time never goes backwards](rounds/80-cpu-times.md) | 2026-10-28 | Follows `cputime_adjust`; times03 no longer fails intermittently |
| [Pre-M9: a woken process is R in /proc](rounds/81-woken-state.md) | 2026-10-28 | shmctl01 no longer fails |
| [Pre-M9: the tests' own races and lost evidence](rounds/82-test-races.md) | 2026-10-28 | A race in the job-control case; evidence kept when a test fails |
| [Pre-M9: unimplemented netlink protocols cannot be created](rounds/83-netlink-protocols.md) | 2026-10-28 | sshd, su and login through PAM no longer fail |
| [Pre-M9: re-testing the open questions](rounds/84-debt-checks.md) | 2026-10-28 | Tests only: several pending failures match native |
| [Pre-M9: if_inet6, ioctl on O_PATH, and the exit after a stop midway](rounds/85-inet6-opath-exit.md) | 2026-10-28 | The JVM no longer turns IPv6 off; a fork event is followed by a stop at syscall exit |
| [Pre-M9: hunting Rc count changes outside the BKL](rounds/86-rc-outside-bkl.md) | 2026-10-28 | A panic while the BKL is released takes the lock back before unwinding |
| [Pre-M9: panics caught in syscalls without the BKL, no open-file borrows held while waiting](rounds/87-fast-panic-held-borrows.md) | 2026-10-29 | A bug in a syscall that does not take the lock ends only that process |
| [Pre-M9: cleaning up futex waiters, borrows checked when releasing the BKL](rounds/88-futex-cleanup-borrow-check.md) | 2026-10-29 | Waiters leave the queue through a guard; no more lost wakeups |
| [Pre-M9: release builds for testing check borrows too](rounds/89-release-borrow-check.md) | 2026-10-29 | The differential tests, LTP and the survey recognize internal errors reported by the kernel layer |
| [Pre-M9: network interface traffic counters](rounds/90-interface-counters.md) | 2026-10-29 | `/proc/net/dev`, sysfs and netlink report the same numbers |
| [Pre-M9: NETLINK_GENERIC with only nlctrl](rounds/91-generic-netlink.md) | 2026-10-29 | Replies byte-for-byte identical to Linux 6.8 |
| [Pre-M9: debuggers, valgrind and headless Chromium](rounds/92-debuggers-valgrind-chromium.md) | 2026-10-29 | 3 kernel-layer problems found; matches native after the fixes |
| [Pre-M9: paths in `/proc/<pid>/fd` and maps resolve symlinks](rounds/93-fd-paths-resolve-symlinks.md) | 2026-10-29 | valgrind can reopen Debian's ld.so |
| [Pre-M9: the link count of `/proc/<pid>/task`](rounds/94-task-dir-links.md) | 2026-10-29 | Chromium's zygote no longer crashes at startup |
| [Pre-M9: signals a tracer had already taken were lost when it exited](rounds/95-tracer-exit-drops-signal.md) | 2026-10-29 | delve reads EOF and exits after attaching, and the process no longer stays stopped |
| [Pre-M9: `/proc/<pid>/net`](rounds/96-proc-pid-net.md) | 2026-10-29 | `/proc/net` becomes a link to `self/net` |
| [Pre-M9: deleted files carry ` (deleted)`](rounds/97-deleted-suffix.md) | 2026-10-29 | needrestart uses it to find processes still using old libraries |
| [Pre-M9: routing tables](rounds/98-routes.md) | 2026-10-29 | The main table comes from the host; `ip route` and others byte-for-byte identical to Linux |
| [Pre-M9: choosing a rootfs in the app](rounds/99-choose-rootfs.md) | 2026-10-29 | Alpine, Debian and Ubuntu to choose from, and a reset |
| [Pre-M9: file capabilities](rounds/100-file-caps.md) | 2026-10-29 | File and ambient capabilities computed as in Linux 6.1; permission checks on extended attributes |
| [Pre-M9: netlink messages for links and addresses](rounds/101-link-addr.md) | 2026-10-29 | `ip addr` and `ip link` show `brd` and `state UP` |
| [Pre-M9: `/proc/net/snmp` and EOF at the end of input](rounds/102-snmp-pty-eof.md) | 2026-10-29 | `ss -s` works; `tuxport-run --pty` sends VEOF at the end of input |
| [Pre-M9: AI coding CLIs](rounds/103-ai-cli-survey.md) | 2026-10-29 | Claude Code, Codex and Gemini CLI match native; their sandboxes cannot be set up |
| [Pre-M9: closing the master hangs up the slave](rounds/104-pty-hangup.md) | 2026-10-29 | Hangs up as `tty_vhangup` does; no processes left after `tmux kill-server` |
| [Pre-M9: a subset of mount](rounds/105-mount-subset.md) | 2026-10-29 | procfs and others mountable anywhere, bind mounts; ps works after chroot into Debian |
| [Pre-M9: counting the code cache once](rounds/106-code-cache-rss.md) | 2026-10-30 | App usage with 20 idle python3 321 → 218 MB |
| [Pre-M9: density of translated code](rounds/107-code-density.md) | 2026-10-30 | About 8 → 6 host words per guest instruction; Maven and Gradle 3–5% faster |
| [Pre-M9: one program shares its translations](rounds/108-shared-translation.md) | 2026-10-30 | 20 idle python3 193 → 84 MB; shell spawning 30% faster |
| [Pre-M9: lock hold time of spawning and mmap](rounds/109-spawn-mmap-bkl.md) | 2026-10-30 | 4 concurrent execs about 10% faster |
| [Pre-M9: the cost of openat](rounds/110-openat-cost.md) | 2026-10-30 | No cheap fix; no code changed |
| [Pre-M9: checkpoints moved to the background](rounds/111-background-checkpoint.md) | 2026-10-30 | Longest commit under the lock while unpacking 8.4 → 0.6 ms |
| [Pre-M9: host memory seams](rounds/112-host-seams.md) | 2026-10-30 | Even with no group split at all it is only 1.4% faster; no code changed |
| [Pre-M9: Go gets a different set of reserved registers](rounds/113-go-registers.md) | 2026-10-30 | `go build` 3–5% faster |
| [Pre-M9: branching to an unaligned address](rounds/114-unaligned-branch.md) | 2026-10-30 | SIGBUS as on Linux; it used to end with an internal error |
| [Pre-M9: processes with JIT code share translations too](rounds/115-shared-with-jit.md) | 2026-10-30 | App usage with 4 idle node 190 → 119 MB |
| [Pre-M9: bind mounts of directories in the store](rounds/116-store-binds.md) | 2026-10-30 | bubblewrap round one: directories and files in the store can be bind-mounted |
| [Pre-M9: tmpfs](rounds/117-tmpfs.md) | 2026-10-30 | bubblewrap round two |
| [Pre-M9: mount namespaces](rounds/118-mount-namespaces.md) | 2026-10-30 | bubblewrap round three |
| [Pre-M9: user namespaces](rounds/119-user-namespaces.md) | 2026-10-30 | bubblewrap round four |
| [Pre-M9: pid namespaces](rounds/120-pid-namespaces.md) | 2026-10-30 | bubblewrap round five |
| [Pre-M9: UTS and IPC namespaces](rounds/121-uts-ipc-namespaces.md) | 2026-10-30 | bubblewrap round six |
| [Pre-M9: network namespaces](rounds/122-net-namespaces.md) | 2026-10-30 | bubblewrap round seven |
| [Pre-M9: bubblewrap and the AI coding CLI sandboxes](rounds/123-bwrap.md) | 2026-10-30 | The Codex and Claude Code sandboxes work |
| [Pre-M9: V8 and HotSpot registers](rounds/124-v8-hotspot-registers.md) | 2026-10-30 | Docs only: after measuring, the registers stay as they are |
| [Pre-M9: emulating raw ICMP](rounds/125-raw-icmp.md) | 2026-10-30 | busybox `ping` and `ping6` work |
| [Pre-M9: PR_SET_CHILD_SUBREAPER](rounds/126-child-subreaper.md) | 2026-10-31 | Used by process supervisors such as tini and s6 |
| [Pre-M9: LTP's remaining small differences](rounds/127-ltp-leftovers.md) | 2026-10-31 | Mostly musl behavior; 6 places changed |
| [Pre-M9: traceroute and mtr](rounds/128-traceroute.md) | 2026-10-31 | One ICMP tap per network namespace; traceroute and mtr work |
| [Pre-M9: namespace files keep namespaces alive](rounds/129-namespace-files.md) | 2026-10-31 | `ip netns` and `nsenter` work |
| [Pre-M9: vhangup](rounds/130-vhangup.md) | 2026-10-31 | Hangs up as `tty_vhangup` does; the node stays, and reopening works as usual |
| [Pre-M9: tmpfs usage and limits](rounds/131-tmpfs-usage.md) | 2026-10-31 | statfs reports usage; ENOSPC when something does not fit or the tmpfs is full |
| [Pre-M9: setns through a pidfd](rounds/132-setns-pidfd.md) | 2026-10-31 | `nsenter -t` used to fail every time and now works |
| [Pre-M9: the ASIMDDP dot product instructions](rounds/133-asimddp.md) | 2026-10-31 | VP9 encoding 4.5% faster |
| [Pre-M9: children in task directories](rounds/134-task-children.md) | 2026-10-31 | A child remembers its parent thread and moves to another thread when that one exits |
| [Pre-M9: SHA3, SHA512 and I8MM](rounds/135-sha3-sha512-i8mm.md) | 2026-10-31 | OpenSSL's SHA-512 4.1× faster |
| [Pre-M9: LRCPC](rounds/136-lrcpc.md) | 2026-10-31 | All the v1.x features of §4.4 are done |
| [Pre-M9: PDEATHSIG and __WNOTHREAD](rounds/137-pdeathsig-wnothread.md) | 2026-10-31 | Sent as soon as the thread that set it exits; `__WNOTHREAD` waits only for the thread's own children |
| [Pre-M9: LRCPC2](rounds/138-lrcpc2.md) | 2026-10-31 | LDAPUR, STLUR and the other forms with offsets |
| [Pre-M9: unimplemented syscalls](rounds/139-unimplemented-syscalls.md) | 2026-10-31 | Docs only: records the numbers that return ENOSYS |
| [Pre-M9: ioprio, syslog and adjtimex](rounds/140-ioprio-syslog-adjtimex.md) | 2026-10-31 | `ionice` works; syslog(2) and adjtimex answer queries |
| [Pre-M9: FP16 and JSCVT](rounds/141-fp16-jscvt.md) | 2026-10-31 | llama.cpp F16 models 44–82% faster |
| [Pre-M9: RDM, FCMA, FHM and BF16](rounds/142-rdm-fcma-fhm-bf16.md) | 2026-10-31 | `gcc -mcpu=native` uses them; BF16 dot product 1.5× faster |
| [Pre-M9: /dev/kmsg and the kernel log](rounds/143-kmsg.md) | 2026-10-31 | `dmesg` in the guest shows OOM kills and internal errors |
| [Pre-M9: openat2](rounds/144-openat2.md) | 2026-10-31 | New versions of GNU tar cannot do without it |
| [Pre-M9: termios2](rounds/145-termios2.md) | 2026-10-31 | Ubuntu 26.04 recognizes the terminal |
| [Pre-M9: PTY packet mode](rounds/146-pty-packet-mode.md) | 2026-10-31 | GNU screen works |
| [Pre-M9: ptrace access checks for /proc/<pid>/exe](rounds/147-proc-exe-access.md) | 2026-10-31 | EACCES for other users' processes and non-dumpable ones |
| [Pre-M9: ID mapping in user namespaces](rounds/148-userns-ids.md) | 2026-10-31 | `SO_PEERCRED`, si_uid, IPC owners and the like are mapped per namespace |
| [Pre-M9: the Ubuntu 26.04 survey](rounds/149-ubuntu26-survey.md) | 2026-10-31 | apt installed 238 packages; three problems found |
| [Pre-M9: newly allocated PTYs](rounds/150-new-pty-state.md) | 2026-11-01 | The master does not EIO before the slave has ever been opened; the window size starts at 0×0 |
| [Pre-M9: short symlinks take no blocks](rounds/151-short-symlink-blocks.md) | 2026-11-01 | The total in `ls -l` matches native |
| [Pre-M9: nmap running as root](rounds/152-nmap-unprivileged.md) | 2026-11-01 | The app sets `NMAP_UNPRIVILEGED=1`, so nmap uses connect scans |
| [Pre-M9: /proc/<pid>/exe points at the program file itself](rounds/153-exe-file.md) | 2026-11-01 | After deletion or replacement, opening through it still gets the original file |
| [Pre-M9: FRINTTS, FlagM, SB and DPB](rounds/154-frintts-flagm-sb-dpb.md) | 2026-11-01 | `gcc -mcpu=native` matches native |
| [Pre-M9: DIT](rounds/155-dit.md) | 2026-11-01 | Carried through signals, ptrace and fork; cleared by execve |
| [Pre-M9: low port mapping and listen notifications](rounds/156-low-ports.md) | 2026-11-01 | Moved to +10000 on the host; a notification when a listen is seen |
| [Pre-M9: the podman and docker survey](rounds/157-container-survey.md) | 2026-11-01 | podman's common features match native; 12 problems found |
| [Pre-M9: four small problems container runtimes ran into](rounds/158-container-small-fixes.md) | 2026-11-01 | capget, `RTM_NEWLINK`, the synthetic root directory's inode, fds in thread directories |
| [Pre-M9: mounting cgroup2 writable](rounds/159-cgroup2-rw-mount.md) | 2026-11-01 | crun mounts it this way for containers; it still cannot be changed from inside |
| [Pre-M9: the mqueue file system](rounds/160-mqueue-fs.md) | 2026-11-01 | podman starts containers without a temporary workaround |
| [Pre-M9: executing memfds and deleted files through an fd](rounds/161-exec-open-file.md) | 2026-11-01 | Needed by runc to start containers and by `podman build --isolation chroot` |
| [Pre-M9: time namespaces](rounds/162-time-namespaces.md) | 2026-11-01 | Needed by dockerd and runc |
| [Pre-M9: the cgroup tree: creating child cgroups, migrating processes](rounds/163-cgroup-tree.md) | 2026-11-01 | dockerd can create the containers' cgroups |
| [Pre-M9: cgroup pids.max and memory.max take effect](rounds/164-cgroup-limits.md) | 2026-11-01 | fork checks `pids.max`; the OOM killer picks by cgroup |
| [Pre-M9: cgroup.freeze](rounds/165-cgroup-freeze.md) | 2026-11-01 | Frozen processes stop before returning to the guest |
| [Pre-M9: cgroup device filtering (eBPF)](rounds/166-bpf-devices.md) | 2026-11-01 | docker and podman run containers without `--cgroups disabled` |
| [Pre-M9: devpts instances](rounds/167-devpts-instances.md) | 2026-11-01 | A container sees only its own terminals |
| [Pre-M9: open_tree, move_mount and mount_setattr](rounds/168-open-tree.md) | 2026-11-01 | podman containers with `--uidmap` start |
| [Pre-M9: the container network survey](rounds/169-container-network-survey.md) | 2026-11-01 | Both get stuck creating a bridge; the four steps of the address-mapping plan are set |
| [Pre-M9: stateful network devices](rounds/170-netdev.md) | 2026-11-01 | Address mapping step one: docker and podman bridge networks come up |
| [Pre-M9: connections through virtual NICs](rounds/171-netmap.md) | 2026-11-01 | Address mapping step two: container-to-container traffic and docker `-p` work |
| [Pre-M9: an nftables subset and DNAT in the socket layer](rounds/172-nftables.md) | 2026-11-01 | Address mapping step three: docker custom networks and podman's nftables driver work |
| [Pre-M9: published ports: PREROUTING DNAT and forwarding on the host](rounds/173-port-forwarding.md) | 2026-11-01 | Address mapping step four: published ports are reachable from outside |
| [Pre-M9: each network namespace has its own ports](rounds/174-namespace-ports.md) | 2026-11-01 | `-p 8080:8080`, with the same port inside and out, starts |
| [Pre-M9: filter chains](rounds/175-filter.md) | 2026-11-01 | Isolation between docker networks matches native |
| [Pre-M9: connections forwarded from outside carry the real client address](rounds/176-real-client-address.md) | 2026-11-01 | Containers see the outside device's real address instead of the gateway's |
| [Pre-M9: completing the network namespace sysctls, UDP also checks whether anyone is bound](rounds/177-net-sysctls.md) | 2026-11-02 | docker `--sysctl` works; UDP in new namespaces is registered too |
| [Pre-M9: faster tests, and a few problems found along the way](rounds/178-test-speed.md) | 2026-11-02 | The full suite down from over half an hour to about 9 minutes |
| [Pre-M9: page tables built on demand, AddressSanitizer works](rounds/179-demand-page-tables.md) | 2026-11-02 | Large mappings get page tables only when accessed; ASan and LSan work |
| [Pre-M9: transparent proxies: SO_ORIGINAL_DST, SO_MARK and owner matching](rounds/180-original-dst.md) | 2026-11-02 | Transparent proxying (`sslocal --protocol redir`) works |
| [Pre-M9: when the last member leaves a shared-translation group, blocks others left behind are invalidated](rounds/181-last-member-leaves.md) | 2026-11-02 | Fixed the intermittent SIGILL with shared translations |
| [Pre-M9: container traffic to the outside also goes through the outer PREROUTING](rounds/182-outer-prerouting.md) | 2026-11-02 | A REDIRECT in the outer layer transparently proxies all containers |
| [Pre-M9: unbound UDP sockets in a new namespace send inside the namespace first, then outside](rounds/183-udp-wide-first.md) | 2026-11-02 | Programs that sendto without binding first can reach any destination |
| [Pre-M9: /dev, /dev/shm, /dev/mqueue and /run mounted at boot](rounds/184-boot-tmpfs.md) | 2026-11-02 | OpenRC support, round one |
| [Pre-M9: the sysctls Alpine writes at boot, protected_symlinks/hardlinks and the random ioctls](rounds/185-protected-links-random.md) | 2026-11-02 | OpenRC support, round two |
| [Pre-M9: the built-in init boots and shuts down per inittab](rounds/186-inittab.md) | 2026-11-02 | OpenRC support, round three |
| [Pre-M9: the app boots and shuts down per inittab and stays in the background with no tabs open](rounds/187-app-init.md) | 2026-11-02 | OpenRC support, last round |
| [Pre-M9: scaling on 8 cores and BKL handoff](rounds/188-bkl-spinners.md) | 2026-11-02 | Only concurrent spawning fails to scale; allowing up to 4 spinners is 7–13% faster |
| [Pre-M9: trying rt_sigprocmask without the BKL, and the shared counter for entering and leaving the dispatcher](rounds/189-sigmask-dbt-running.md) | 2026-11-02 | rt_sigprocmask without the lock not adopted; 8 threads reading the clock 844 → 155 ns |
| [M9: settings page, Alpine imported by default](rounds/190-settings-default-alpine.md) | 2026-11-02 | Switching distributions, reset, choosing a mirror, changing the execution tier |
| [M9: keep running with the screen off](rounds/191-keep-awake.md) | 2026-11-02 | Holds a `PARTIAL_WAKE_LOCK`; does not ask for a battery optimization exemption |
| [M9: license changed to AGPL-3.0-or-later, open source licenses page](rounds/192-license.md) | 2026-11-02 | The Termux terminal libraries treated as GPLv3-only; R3 goes away |
| [M9: logging in as a chosen user](rounds/193-login-account.md) | 2026-11-02 | Logs in as the first regular user created by default; fixed `FIONREAD` on the DNS stub |
| [M9: restoring tabs after being killed](rounds/194-restore-tabs.md) | 2026-11-02 | A new shell starts with the same account and directory and shows the old scrollback |
| [M9: notices when a process is ended or the service fails to start](rounds/195-kill-reports.md) | 2026-11-02 | The reason is shown in the tab or a notification |
| [M9: browsing in the Files app (read-only)](rounds/196-documents-read.md) | 2026-11-02 | A DocumentsProvider that reads the store directly, bypassing the kernel |
| [M9: editing the home directory in the Files app](rounds/197-documents-write.md) | 2026-11-02 | Create, write, delete and rename, confined to the home directory |
| [M9: the store survives power loss](rounds/198-power-loss.md) | 2026-11-03 | Flush ordering added following ext4; 75 dm-flakey power cuts without damage |
| [M9: the 39-bit host address space](rounds/199-reserved-mappings.md) | 2026-11-03 | `PROT_NONE` reservations take no host address space; Chromium 4.83 TB → 12.4 GB |
| [M9: the source released with the app](rounds/200-release-sources.md) | 2026-11-03 | `scripts/release-sources` produces three source archives |
| [M9: two-way notifications between the Files app and the guest](rounds/201-documents-notify.md) | 2026-11-03 | The guest receives inotify events, and the Files app refreshes |
| [M9: the kernel's store is used only while holding its BKL](rounds/202-store-bkl-check.md) | 2026-11-03 | The cause of the intermittent panic in round 193 |
| [M9: switching mirrors keeps the sources users added](rounds/203-mirror-keeps-lines.md) | 2026-11-03 | Only the built-in mirror's address changes; every other line stays |
| [M9: the Files app recognizes text files by content](rounds/204-text-by-content.md) | 2026-11-03 | Files with no NUL that are valid UTF-8 get `text/plain` |
| [M9: kernel bug reports can be copied and carry the panic location](rounds/205-copy-bug-report.md) | 2026-11-03 | The panic message and code location go into the report |
| [M9: installing sudo on Debian shows progress and can be skipped, regular users' shells have job control](rounds/206-debian-sudo-progress.md) | 2026-11-03 | Ctrl-C skips just this once; fixed `su -c` starting a new session |
| [M9: kernel bug reports carry the kernel log from before the error](rounds/207-report-kernel-log.md) | 2026-11-03 | Reports include the last 10 log entries before the error |
| [M9: binding the original low port when the host allows it](rounds/208-low-ports-direct.md) | 2026-11-03 | A Play system update opened up ports such as 22, 80 and 443 |

## Measurement conditions

Unless a file says otherwise, its measurements were taken under these conditions:

- **Host**: an arm64 Linux VM run by colima on Apple Silicon (4 cores; 8 cores from [faster tests](rounds/178-test-speed.md) on). Tuxport is a release build running Tier 1.
- **Native baseline**: an Alpine 3.24 container on the same VM. From M8 on it is a docker image exported from the differential tests' store, so both sides have exactly the same files. The [profiling](rounds/13-profiling.md) round found that the baseline needs exactly the same files and state: the baseline image at first did not pack `/var`, so native apk had no repository index to read.
- **Warm cache**: from [profiling](rounds/13-profiling.md) on, each workload runs twice in the same Tuxport session and the second run counts (likewise on the native side). On a phone the app is a long-running process, so the second run is closer to real use.
- **Store location**: from [translations shared between threads and targeted wakeups](rounds/24-shared-translation.md) on, the benchmark store lives on the VM-local docker volume `tuxport-bench`. Before that it was in `target/`, shared into the VM over virtiofs, so every host open and stat crossed the VM boundary and the ratios of file-system and process-spawning workloads came out too high. From [the host cost of file operations](rounds/33-file-ops-host-cost.md) on, native file operations run on the same volume as well (before, on the container's own overlay).
- **A/B**: the "before" or "HEAD" column is the binary from the start of the round, alternated with the changed binary over several runs in the same session (`target/mem/ab.sh` and similar scripts), giving a range.
- **Neoverse N2**: GitHub's `ubuntu-24.04-arm` runner (Azure Cobalt 100, 2 vCPUs), where CI ran the performance probe until 2026-11-07 (it now runs only by hand). Its cores are narrower than Apple Silicon's and closer to the big and mid cores of phones.

## Known flaky tests

When the LTP syscall tests run in full in parallel (`scripts/ltp -j 8`), the following tests pass and fail intermittently, unrelated to the change at hand: each time they were rerun at the same moment with the previous round's binary, or rerun on their own. The rounds only record whether they passed or failed that time.

| Test | Cause |
|---|---|
| futex_wait05, epoll_pwait03, epoll_wait02, clock_nanosleep02, nanosleep01, select02, pselect01, pselect01_64, poll02, prctl09 | They check sleep timeout precision and fail when a 1 ms sleep measures 1.3–1.6 ms, depending on the VM's timer latency at the time (since M7); in [exclusive wakeups](rounds/48-exclusive-wakeup.md) a native 1 ms nanosleep in the VM also took 1.5 ms and these failed run after run, with the old binary too; in [dropping weak references outside the BKL on thread exit](rounds/71-thread-exit-race.md) native averaged 1.58 ms with the VM idle and 7 of them failed with both the old and the new binary; in [routing tables](rounds/98-routes.md) select02 failed even on its own, with a native 1 ms sleep median of 1.59 ms |
| getrusage03 | Needs at least 512 MB of available memory; skipped when a full parallel run leaves too little, passes on its own |
| nice05 | Compares the CPU time of two threads with different nice values; Tuxport did not allocate CPU by nice, so the sign of the difference was random (since M8); passes since [deferred items](rounds/19-deferred.md) handed nice to the host threads |
| exit_group01 | In [terminal owner and permissions](rounds/64-pty-owner.md) it failed once with exit code 134 (SIGABRT) in a full parallel run, and the rerun overwrote the log (since [the tests' own races and lost evidence](rounds/82-test-races.md), `scripts/ltp` keeps the previous log); it passed 8 runs on its own and 30 sequential runs under the load of 7 other LTP lanes; the cause was not investigated. Most likely it is the race fixed in [dropping weak references outside the BKL on thread exit](rounds/71-thread-exit-race.md): glibc ends tuxport-run with SIGABRT when the host heap is corrupted |
| mmap21_01, mmap21_02 | In [the host cost of file operations](rounds/33-file-ops-host-cost.md) the cleanup got EIO unlinking a file under `/dev/shm`; it did not recur in 3 reruns on their own or in 3 reruns of the 162 tests related to `/dev/shm`, mmap and file operations at 16-way parallelism, and the October 24 baseline had one occurrence too (with the LTP store on virtiofs). In [ptrace step two](rounds/41-ptrace-gdb.md) the log showed `unlink(/dev/shm/ltp_mmap21_2)` getting ENOENT: LTP's library keeps results in `/dev/shm/ltp_<program name>_<pid>`; mmap21_01 and mmap21_02 are the same program, and the first process in every tuxport-run is pid 2, so when they run in parallel the two cases share the store's `/dev/shm` and the same file, and one deletes the other's. This comes from how LTP is run in parallel, not from the kernel; in [`/proc/net/snmp` and EOF at the end of input](rounds/102-snmp-pty-eof.md) `open` of this file got EEXIST, for the same reason; in [low port mapping](rounds/156-low-ports.md) mmap21_01 failed (unlink got ENOENT), for the same reason |

In the differential tests (`scripts/difftest`), the native side of `loop` uses the development VM's own loop devices: on 2026-11-07 it failed once with `control remove 4000: Resource busy` (the VM probing the device just created) and then once with `control remove missing: 0` (the device left behind by that failed run), and passed on the next runs. Rerun it on its own: the failed run's leftover device is removed by the next run, which then passes.

In `gvisor-process`, Tuxport's side of `ExecTest.ExecuteOnlySpecialFiles` fails intermittently when the whole group runs: on 2026-11-08 it failed 2 of 5 runs with the binary under test and 3 of 3 with the previous commit's, while 30 runs of the test alone passed. The test calls setresuid on a thread (until 2026-11-09 Tuxport changed the whole process's credentials; since then each thread has its own, and on that day the group passed 6 of 6 runs, but the previous binary also passed 3 of 3, so the change cannot be credited) and execs a FIFO, a socket and a device node under a 10 s alarm each. A failed run that is killed by the 60 s timeout leaves `/tmp/exec-only-*.<pid>` in the lane's store, and later runs whose test gets that pid failed at once on `EEXIST`; since 2026-11-08 `tests/diff/gtest/run.sh` deletes those files before running, so only the first failure is intermittent. The cause was not investigated.

In `gvisor-fs`, the native side of `PtyTest.TCFLSHWakesReplicaPolloutAfterMasterInputFlush` failed once on 2026-11-08 while four groups ran in parallel (`-j 4`), and passed when the group was rerun alone; it waits for a replica's `POLLOUT` after the master flushes its input, and was presumably slowed by the load. The cause was not investigated.

In `starnix-io`, the native side of `Pty.EchoModes` failed once on 2026-11-08 in a full run with four lanes (`-j 4`), while Tuxport's side passed, and the group passed when rerun alone. The cause was not investigated.

In `mount-locks`, Tuxport's side failed once on 2026-11-08 in a full Tier 0 run with four lanes: right after `waitpid` returned for a child that had entered a new user and mount namespace and sent an fd into it, a listing through that fd still showed the mount over `hidden` (`['secret']`), which Linux has taken down by then. The group passed 3 of 3 reruns alone. Presumably the dead namespace's mounts are sometimes collected only after the parent's wait returns (the vfs map's collection at task exit); the cause was not investigated.

In `pty`, Tuxport's side of the packet-mode `^C` lines differed once on 2026-11-09 in a full run with four lanes: the master's first read after `^C` returned `\x03` (both flush bits) and the read after the `NOFLSH` `^C` found nothing (`EAGAIN`), where the native side reports the write flush first and the read flush on the next read. The group passed 5 of 5 runs alone. Which bits the first read sees depends on when it comes; the cause was not investigated.

In `fuse`, Tuxport's side failed once on 2026-11-10 in a full run with four lanes: after the open file of `f2` was closed and the second `drop_caches`, the daemon's count still showed `f2` as remembered (`f2 1`), which the native side has forgotten by then. The group passed 3 of 3 reruns alone. The check slept one second after `drop_caches`, and under load the forget may come later; it failed the same way once more on CI on 2026-11-11 (a manual full run, one lane), and again after the check waited up to 5 s. The cause was Tuxport's: when `drop_caches` came before the daemon had answered the release of the just-closed `f2`, the node was kept and forgotten only at a later sweep, while Linux evicts the inode as soon as the release ends. Since then Tuxport forgets such a node once its release is answered (the kernel/fuse map), and the check still waits up to 5 s for the forgets.

In `netmap`, Tuxport's side failed once on CI on 2026-11-10 (the nightly full tests, one lane): in `loopback-then-outside` the reply from the server on 127.0.0.1 inside namespace A did not arrive within the one second `multi.py` waits after its sends (`answered []`, natively `['m0']`). It did not recur in the CI runs on 2026-11-09 and 2026-11-11; the cause was not investigated.

In `namespaces`, Tuxport's side failed once on CI on 2026-11-11 (a manual full run, one lane): `unshare -p -f` whose process 1 exits 0.5 s after starting `sh -c "sleep 100 & sleep 100 & wait"` did not end within its 20 s `timeout` (`Terminated`, exit 143 instead of 7). On the development VM the whole group passed 20 of 20 runs (four at a time), the command 40 of 40 runs, and with process 1 exiting 0 to 300 ms after the start 270 of 270 runs (the 120 of them that looked found no `sleep` left behind). Presumably process 1 exited while the inner shell was still forking on the slower runner, and something in the pid namespace's teardown then waited for a process that was not killed; the cause was not investigated.

In `gvisor-process` and `gvisor-fs`, Tuxport's side of a test was killed by the runner's 60 s timeout (`exit 137`) in two manual full runs on CI on 2026-11-11: `ExecTest.OneEnv` and `ExecTest.InterpreterScriptTrailingWhitespace` in the first, `FcntlLockTest.SetLockDroppedOnClose` in the second. Each forks and execs a child and waits for it; neither run's tuxport stderr had a BKL watchdog report, so no syscall held the BKL that long, which points to a wait that was never woken. They did not recur on the development VM. Since then the gtest runner follows a test killed by its timeout with each thread of its processes and the syscall it waits in (`hang:` lines, from Tuxport's `/proc/<pid>/task/<tid>/syscall`), which full.yml uploads with the failed outputs.

On 2026-11-17, `vfs::store::tests::inode_numbers_reused_after_boot_keep_their_objects` failed once
in the Linux arm64 workspace suite. After reopening the store, the test counted 60 objects handed
to the reaper where it expected zero. It passed on its own and on a full rerun. The cause is unknown.

## R1 measurements

Ratios relative to native, under the [measurement conditions](#measurement-conditions); details in the corresponding file.

| Point | Workload | Ratio |
|---|---|---|
| [M2](rounds/01-m2.md) | Geometric mean of the integer benchmarks | 1.85×; 2.37× on Neoverse N2 |
| [M1b](rounds/03-m1b.md) | busybox `sha256sum`, `gzip -c` | 1.51×, 4.75× |
| [M6](rounds/08-m6.md) | Building Lua, SQLite | About 10×, mostly SIMD that Tier 1 did not translate yet falling back to Tier 0 |
| [M7](rounds/09-m7.md) | Building `lvm.c`, SQLite `make`, Lua `make linux`; the performance probe on Neoverse N2 | 4.8×, 4.7×, 6.1×; 2.25× |
| [M8](rounds/10-m8.md) | JIT-generated code in node; `go build` cold build | 1.9×; 8.7× (5.6× single-threaded, poor multithreaded scaling) |
| [M8 follow-up: JVM](rounds/11-m8-jvm.md) | Test programs; Maven warm build; Gradle warm build | 4–5×; 11×; 14× (jump cache misses, patching of JIT code) |
| [Profiling](rounds/13-profiling.md), [keeping translations](rounds/14-spawn-translation.md) | `git status`; shell spawning | 6×; 6.3× |
| [SP in a register and return prediction](rounds/15-sp-return-prediction.md) | gcc building `lvm.c`; python | 3.9×; 6.2× |
| [Targeted futex wakeups](rounds/16-futex-targeted-wake.md) | Gradle warm build | 9× |
| [Memory-access checks and the translation table](rounds/23-tlb-checks.md) | gcc building `lvm.c`; python; Maven, Gradle warm builds; `go build` cold build | 3.6×; 4.9×; 9.4×, 5.6×; 6.5× |
| [Translations shared between threads and targeted wakeups](rounds/24-shared-translation.md) (the store moved to a VM-local volume; every row above was measured on virtiofs, with file-system and spawning ratios too high) | gcc; python; Maven, Gradle warm builds; `go build` cold build; shell spawning; `git status`; `node -e 0` | 3.4×; 4.3×; 4.4×, 4.0×; 3.2×; 2.2×; 1.1× (parallel lstat 2.3×); 2.9× |
| [stat without the BKL](rounds/25-stat-without-bkl.md) | `git status` with parallel lstat | 1.3–1.4× |
| [Copy-on-write fork](rounds/26-fork-cow.md) | fork, child exit, wait (process having written 0, 10, 100 MB) | 1.25×, 2.2×, 3.8× (was 1.6×, 18×, 35×) |
| [The host cost of spawning processes](rounds/28-spawn-host-cost.md) | fork then exec, vfork then exec, `posix_spawn`; `sh -c 'cat \| wc'`; shell spawning | 1.05×, 1.15×, 1.2×; 1.4×; 1.4× (was 1.45×, 1.6×, 1.6×; 2.2×; 1.8×) |
| [Waiting for and holding the BKL](rounds/29-bkl-wait-hold.md) | 4 processes doing fork then exec at once (total throughput); `sh -c 'cat \| wc'`; shell spawning | 2.7× (was 3.7×); 1.15×; 1.3× |
| [Memory accesses in translated code](rounds/30-translated-access.md) | gcc building `lvm.c`; python loop | 3.2×; 4.6× |
| [Other workloads and file operations](rounds/32-workloads-file-ops.md) | At the start of the round: `find`, `du`; read-only `tar`; `grep -r`; `gzip`; sqlite in-memory / file database; `awk`, lua loops; node startup / compute; java startup / compute. After the round: `cp -r` then `rm -rf` | About 1×; 1.4×; 3.0×; 2.0×; 3.8× / 2.7×; 2.8×, 2.0×; 3.1× / 2.6×; 4.0× / 2.8×. 1.6× (was 2.4×) |
| [The host cost of file operations](rounds/33-file-ops-host-cost.md) (native baseline moved to the same volume) | `cp -r` then `rm -rf`; `tar x` then `rm -rf` | 1.6×; 1.7× (was 2.0×, 2.3×) |
| [Node startup](rounds/34-node-startup.md) | `node -e 0` warm start; a loop of outline-atomics atomic operations | 2.2×; 1.2× (was 6×) |
| [JIT workloads](rounds/35-jit.md) | Maven, Gradle warm builds | 3.6×, 3.4× |
| [Parallel processes](rounds/36-parallel-processes.md) | 4 processes exec at once; 4 pipelines at once | 2.5×; 3.1× (was 3.1×, 4×) |
| [Splitting the BKL](rounds/38-split-bkl.md) | `go build` cold build | 2.8× |
| [One program shares its translations](rounds/108-shared-translation.md) | Shell spawning | 1.13× (was 1.6×) |
| [Go gets a different set of reserved registers](rounds/113-go-registers.md) | `go build` cold build | 2.6× (2.7× with the default set) |
| [The ASIMDDP dot product instructions](rounds/133-asimddp.md) | `vpxenc` VP9 encoding (single-threaded, 360p, 60 frames) | 2.2× (2.3× without advertising dot product) |
| [SHA3, SHA512 and I8MM](rounds/135-sha3-sha512-i8mm.md) | Python `hashlib.sha512` (32 MiB) | 1.0× (4.1× without advertising SHA512) |
| [FP16 and JSCVT](rounds/141-fp16-jscvt.md) | llama.cpp `llama-bench` (4 threads): Qwen2.5-0.5B F16 prompt 128 and generation 32; the same two for Q4_0 | 1.64×, 1.22× (2.98×, 1.74× without advertising FP16); 1.38×, 1.21× |
| [Scaling on 8 cores and BKL handoff](rounds/188-bkl-spinners.md) (the VM has 8 cores since [faster tests](rounds/178-test-speed.md)) | Building Lua with `xargs -P1`, `-P4`, `-P8`; `git grep` with 1, 4, 8 threads; 4 and 8 processes doing fork then exec at once; 4 and 8 pipelines at once | 3.3×, 3.0–3.3×, 3.5×; 2.3×, 2.0×, 1.7×; 2.2–2.3×, 4.1–4.3× (was 2.5×, 4.4×); 2.5–2.6×, 4.0–4.3× (was 3.0×, 4.8×) |
