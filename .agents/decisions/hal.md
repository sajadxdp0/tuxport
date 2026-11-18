# Host abstraction layer (hal) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: hal"`
>
> Last distilled: 2026-11-12 (HEAD = 0553229). D1–D7 were seeded by hand from the design doc §3 and
> the round records in `docs/rounds/`, so their sources cite rounds; later entries cite commits.

## Active

### D1: One implementation for Android and Linux hosts, passing host errnos through
- **What**: The hal has a single implementation for bionic and glibc, with `cfg` branches only where the hosts differ and a `compile_error!` pointing to `scripts/dev` on any other target; functions return the host's errno, which the kernel hands to the guest unchanged; the core crates depend on no Android component.
- **Why**: Development runs on Linux arm64 and production on Android, both Linux kernels whose errno numbering matches the guest's, so keeping Android-specific code in the hal and the Kotlin shell lets 90% of the work happen in a Linux VM; the fallbacks meant to keep other hosts compiling had been broken for two weeks unnoticed, and even a macOS build that compiled could not run a guest.
- **Tradeoffs**: `tuxport-run` no longer builds natively on macOS and IDE analysis on a Mac must target Linux or Android or run in `scripts/dev`; an iOS port would need a Darwin hal written from scratch.
- **Watch out**: A host-only behaviour (Android's app seccomp, missing `/proc` files) must be handled in the hal or its caller, not assumed from the Linux development host.
- **Source**: design doc §3, §13.3; round 07 (`07-m5`); 7c5e636

### D2: DNS goes to the host's resolver: `android_res_nsend` on Android
- **What**: `dns_query` sends the guest's query packet through `android_res_nsend` on the default network (linked against libandroid) with a 30 s backstop, and on other hosts to the servers in the host's `/etc/resolv.conf`.
- **Why**: Through the system resolver, Android's Private DNS, VPNs and per-network routing apply to the guest (see `kernel/net` D5 for the stub that calls it).
- **Tradeoffs**: The call blocks the calling thread; callers must not hold locks.
- **Source**: round 07 (`07-m5`)

### D3: Edge-triggered readiness of a host socket comes from a per-fd host epoll
- **What**: `Edges` is a host epoll registered edge-triggered on one fd; `take()` reports whether read or write edges arrived since the last call.
- **Why**: Host socket data never passes through the kernel layer, and a host poll only shows current readiness; guest `EPOLLET` and `EPOLLEXCLUSIVE` need to know that new data arrived.
- **Tradeoffs**: One extra host fd per such open file; several arrivals between two calls fold into one edge; fds the host epoll rejects (regular files as stdio) fall back to level checks in the caller.
- **Source**: round 42 (`42-epoll-edges`)

### D4: Thread policy and statistics act on host threads, best effort
- **What**: Affinity, nice and ioprio are applied to the host thread running the guest thread, ignoring failures (the kernel applies its own floors first); context switches and CPU time come from the host thread (`getrusage(RUSAGE_THREAD)` for the calling thread, `/proc/self/task/<tid>` for others).
- **Why**: Guest threads are host threads scheduled by the host, so the host's scheduler and I/O queues should see the guest's priorities; reading the counters with one `getrusage` replaced parsing the host status file under the BKL (about ten microseconds a call).
- **Tradeoffs**: A host refusal is invisible to the guest; values from pooled host threads need a base taken when a guest thread starts.
- **Source**: rounds 48 (`48-exclusive-wakeup`), 109 (`109-spawn-mmap-bkl`), 140 (`140-ioprio-syslog-adjtimex`)

### D5: Host routes and source addresses come from the host kernel
- **What**: `routes()` parses the host's `/proc/net/route` and `/proc/net/ipv6_route` (unicast main-table routes; `None` where unreadable, as on Android); `source_for` connects a UDP socket to the destination and reads its local address.
- **Why**: `ip route` and `ip route get` should show what the host would actually use, and the host picks the source address itself.
- **Tradeoffs**: On Android, the routes come from the app side instead (`kernel/net`).
- **Source**: round 98 (`98-routes`)

### D6: `adjtimex` is a read-only host query, synthesized on Android
- **What**: `adjtimex_query` makes the real read-only call on Linux hosts and returns a synthesized unsynchronized state on Android.
- **Why**: Android's app seccomp forbids `adjtimex`, and Android sets the time with settimeofday instead of the kernel's NTP discipline, so the synthesized state matches what the Android kernel actually holds.
- **Source**: round 140 (`140-ioprio-syslog-adjtimex`)

### D7: Host file copies use `sendfile`, not `copy_file_range`
- **What**: `copy_file` seeks the output fd and calls the host's `sendfile`; the kernel uses it for `sendfile` and `copy_file_range` between regular files, falling back to chunked copies when the host refuses.
- **Why**: Before Android 14 the app seccomp does not allow `copy_file_range` (the call gets `SIGSYS`).
- **Tradeoffs**: `sendfile` writes at the output fd's host position, so the caller must hold the output file position while copying.
- **Source**: round 33 (`33-file-ops-host-cost`)

### D8: The host abstraction layer is a module of its own
- **What**: hal covers `core/hal` (`lib.rs`, `net.rs`), described in one map rather than inside each kernel map or left to design §3.
- **Why**: Apart from the design doc's §3, nothing documented the host layer, and the user asked to map it with §11.
- **Tradeoffs**: Its DNS entry (D2) overlaps kernel/net D5 and refers to it.
- **Watch out**: The map records no open bugs: of the two confirmed at the split, a signal no longer fails a non-Android DNS lookup and the non-Linux fallbacks that no longer compiled were removed.
- **Source**: 12c4afa (bug list since updated by 5d23297 and 7c5e636)

### D9: Random bytes come from getrandom(2), not `/dev/urandom`
- **What**: `fill_random` calls getrandom(2) in a loop that retries `EINTR` and short results.
- **Why**: Opening `/dev/urandom` per call needs a free fd in the table the guest shares with the app, so randomness failed exactly when the guest exhausted fds.
- **Tradeoffs**: A held fd would also have worked but occupies a slot and must survive fd-closing code.
- **Watch out**: getrandom blocks until the host's entropy pool is initialised, which a booted Android or Linux system already is.
- **Source**: 9e72dbb, 7c5e636

### D10: hal files read in the order of the map's responsibilities
- **What**: lib.rs runs memory, files and storage, host statistics, waiting, threads, CPUs and time, the CLI's terminal helpers and errors (`last_errno` last), and net.rs runs host sockets, interfaces and routes and DNS with `sockaddr` last; cfg alternatives of one function stay adjacent.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; lib.rs had no sections and mixed memory, file, thread and terminal calls.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; `sockaddr`, used by the kernel too, sits under the DNS header.
- **Watch out**: Code appended at the end of a file drifts from this order, and the Android cfg branches that moved with their Linux twins were only compiled by CI.
- **Source**: 9e7bfd5

### D11: Host clock jumps are watched with a cancel-on-set timerfd
- **What**: `ClockWatch` arms a timerfd at 2^40 s with `ABSTIME` and `CANCEL_ON_SET`, and `take()` reads `ECANCELED` and re-arms it, for the timer thread (kernel/process D19).
- **Why**: The kernel needs to know when the host's wall clock jumps against the monotonic clock.
- **Tradeoffs**: It costs one host fd per kernel.
- **Watch out**: An Android kernel whose seccomp policy forbids timerfd_create silently disables it (`ClockWatch::new` returns None).
- **Source**: 63b9e59

### D12: pagemap tells whether a private file mapping holds written pages
- **What**: `has_private_pages` reads pagemap for pages present without the file bit or swapped, sharing one pagemap fd with `touched_pages`.
- **Why**: fork needs to know whether a private file mapping has pages written since it was mapped (kernel/mm D22).
- **Tradeoffs**: One pread per call.
- **Watch out**: Hosts that hide pagemap fall back to "has private pages".
- **Source**: e03c0cc

### D13: The host's `/proc/vmstat` and `/proc/swaps` are read through hal
- **What**: `host_vmstat` and `host_swaps` return the host's text, or None when it is unreadable or empty, for the guest's `/proc/vmstat` (kernel/procfs D18) and `/proc/swaps` (kernel/procfs D30).
- **Why**: The kernel layer reads host files through hal, and the guest's `/proc/swaps` copies the host's.
- **Tradeoffs**: None.
- **Watch out**: Android probably refuses `/proc/swaps` to apps, so phones show only its header.
- **Source**: 60921b2, 39ea702

### D14: Host waits retry interrupts, and the hal's signal handlers keep errno
- **What**: `poll_until` retries `EINTR` up to a deadline set once per server (once in all on Android), the SIGWINCH handler saves and restores errno, and `stop_notifier` catches SIGINT, SIGTERM and SIGHUP into a pipe for the CLI, undoing itself when a sigaction fails.
- **Why**: An interrupted poll failed a DNS lookup and mismatched answer IDs restarted the wait, the SIGWINCH handler could clobber errno and a failing sigaction leaked its pipe, and the CLI needed a stop-signal pipe.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 5d23297

### D15: Sparse files and MADV_REMOVE go to the host's fallocate and madvise
- **What**: `fallocate(fd, mode, offset, len)` passes Linux's `FALLOC_FL_*` modes to the host and retries `EINTR`, and `remove(ptr, len)` applies the host's `MADV_REMOVE` to host-page-aligned shared memory (kernel/vfs D37, kernel/mm D33).
- **Why**: Sparse files and `MADV_REMOVE` need the host to allocate, punch and free storage.
- **Tradeoffs**: A host filesystem without a mode answers `EOPNOTSUPP` and the callers fall back.
- **Watch out**: Android's app seccomp is assumed to allow fallocate and `MADV_REMOVE`; bionic exposes both, but this was checked only by building for the target.
- **Source**: 5d7b936

### D16: Stall reports ask the host what a stuck thread is doing
- **What**: `thread_activity(tid)` returns a host thread's comm and whether it is running, in a host syscall (from `/proc/self/task/<tid>/syscall`) or in the state its `stat` shows, for the BKL watchdog's reports (kernel/process D38).
- **Why**: A report of a stuck BKL holder needs to say whether its host thread spins or blocks in a host syscall, which the kernel's own records cannot show.
- **Tradeoffs**: It gives no host backtrace, and the syscall file is readable only by the process itself.
- **Watch out**: A host policy that refuses the syscall file silently falls back to the state letter.
- **Source**: 64a6107

### D17: A shared file mapping is re-pointed with one fixed host mapping
- **What**: `map_file_over(base, len, fd, offset, writable)` maps a host file over an existing range with `MAP_FIXED`, so threads touching the range see the old file or the new one but never a hole, for shared mappings that follow a file to its own store object (kernel/mm D37).
- **Why**: Re-pointing a shared file mapping at another host file in place needs a fixed mapping.
- **Tradeoffs**: None known.
- **Watch out**: The function is unsafe: a wrong range replaces unrelated memory.
- **Source**: df6ee70

### D18: The host's vDSO, vvar pages and timezone are found through hal
- **What**: `host_vdso` takes the vDSO from `AT_SYSINFO_EHDR` and its length and the contiguous `[vvar*]` mappings below it from `/proc/self/maps`, and `timezone` makes the raw gettimeofday syscall, since glibc's wrapper zeroes the timezone (kernel/signal D4, kernel/mm D38).
- **Why**: Guests run the host's vDSO for clock reads, which needs where the host mapped it and its data pages, and gettimeofday reports the host kernel's timezone.
- **Tradeoffs**: Each kernel reads `/proc/self/maps` once.
- **Watch out**: None known.
- **Source**: 16364c0

### D19: Host files in memory come from memfd_create
- **What**: `memfd(name)` creates a close-on-exec memfd, available on Android 30+ and Linux, for the FUSE files copied for execve and the sparse files FUSE mappings read into (kernel/mm D40, kernel/fuse D1).
- **Why**: FUSE files have no host object, so their mappings and execve need host files in memory.
- **Tradeoffs**: The pages copied into them take the app's memory.
- **Watch out**: None known.
- **Source**: c008749

### D20: Socket options whose optlen is not a byte count get calls of their own
- **What**: `net::get_filter(fd, count)` allocates count × 8 bytes, passes count as optlen and returns the program's count with the instructions copied (none when count is 0), while `getsockopt` keeps reading optlen as bytes (kernel/net D27).
- **Why**: `getsockopt` sized its buffer by optlen read as bytes, which `SO_GET_FILTER`'s count of instructions broke.
- **Tradeoffs**: One function per such option.
- **Watch out**: The Android build takes `SO_GET_FILTER` from bionic's libc and is checked only by CI.
- **Source**: c65da9d

## Superseded

- ~~Other hosts keep fallbacks so that they at least compile (design doc §3, round 07)~~ → replaced by **D1** in 7c5e636 (2026-11-08)
- ~~Other hosts read random bytes from `/dev/urandom` (D9, 9e72dbb)~~ → replaced by **D1** in 7c5e636 (2026-11-08)
