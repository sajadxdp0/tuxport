# hal Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/hal.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 3 research concerns; 7 claims checked by 1 independent verifier — 6 confirmed and 1 partial; bugs and debts recorded)
>
> Maintained: 2026-11-11 (targeted verification only since the full mapping; latest change: `get_filter` for `SO_GET_FILTER`, whose optlen counts instructions)

## Responsibilities

`hal` (crate `tuxport-hal`, depending only on `libc`) is the host abstraction layer under the kernel
layer. It wraps the host syscalls the kernel needs, with one implementation for Android (bionic)
and Linux (glibc), the only hosts it builds for, returning raw host errnos:

- memory: anonymous and reserved mappings, file mappings, discard and hole punching, msync,
  residency queries;
- files and storage: `*at` calls on the object directory, positional and plain I/O, `sendfile`
  copies, `fallocate`, the fsync family, `flock`, `ioctl`, `statvfs`, fd limits, randomness;
- host statistics: process memory usage, host meminfo, CPU set and sysfs, per-thread CPU time and
  context switches;
- waiting: `poll`/`ppoll`, `Event` (eventfd), `Edges` (edge-triggered host epoll on one fd), `Watch`
  (level-triggered host epoll over many fds);
- threads and time: tid, thread names, CPU clocks, affinity, nice, ioprio, clocks, the kernel's
  timezone, a read-only `adjtimex`, and where the host mapped its vDSO and vvar pages;
- networking: nonblocking host sockets, interfaces, routes, source selection, DNS resolution;
- terminal helpers for the CLI: raw mode, a `SIGWINCH` pipe and a stop-signal pipe.

Root: `core/hal/src/lib.rs`, `core/hal/src/net.rs`.

Outside the module: every policy built on these primitives (`kernel/*`); the Android-side network
callbacks and JNI (`android`); the CLI front end (`hosts/cli`).

## Key types

- `FileMapping` — `ptr` at the requested offset; `base`/`len` the host-page-aligned range actually
  mapped (what `unmap` takes).
- `MemoryUsage` (RssAnon — VmRSS before Linux 4.5 —, RssShmem, VmSwap), `MemInfo` (a subset of
  host `/proc/meminfo`), `FsStats` (statvfs; block size `f_frsize` or `f_bsize`).
- `Event` — wakes one thread from another: an eventfd (`EFD_CLOEXEC|EFD_NONBLOCK`); `signal`,
  `clear`, `is_signaled`, `fd`.
- `Edges` — a host epoll with `EPOLLIN|EPOLLOUT|EPOLLRDHUP|EPOLLET` on one fd; `take()` returns
  read/write edge booleans (several arrivals between calls fold into one).
- `Watch` — a level-triggered host epoll over many fds (raw ICMP taps), with an errors-only mode.
- `ClockWatch` — a timerfd on the host's `CLOCK_REALTIME`, armed at 2^40 s with
  `TFD_TIMER_ABSTIME|TFD_TIMER_CANCEL_ON_SET`; Linux cancels it when the wall clock is set and when
  the device resumes (`timerfd_resume`); `take()` reads `ECANCELED`, re-arms and reports a jump.
  None when the host refuses the timerfd (a seccomp policy).
- `RawMode` — `cfmakeraw` guard restoring the terminal on drop.
- `Received` (recvmsg result: length, raw address ≤ 128 bytes, control bytes, flags), `Interface`
  (name, index, flags, MTU, MAC, addresses with prefix lengths), `Route` (family, destination,
  prefix, gateway, interface, metric).

## Public entry points

- Memory: `map_anonymous(len, reserve)`, `map_inaccessible`, `unmap`, `discard`, `remove`,
  `map_file`,
  `map_file_read_only`, `map_file_over`, `msync`, `populate`, `page_size`, `resident_pages`, `page_residency`,
  `touched_pages`, `has_private_pages`, `cachestat`.
- Statistics: `memory_usage`, `memory_info`, `ram_and_swap`, `host_meminfo`, `host_vmstat`,
  `host_swaps`, `statvfs`,
  `fd_limit`, `raise_fd_limit`, `host_cpus`, `cpu_sysfs`, `thread_cpu_clock`,
  `thread_system_ns(_self)`, `thread_ctxt_switches(_self)`, `thread_activity`.
- Files: `open_dir`, `open_at`, `stat_at`, `fstat`, `chmod_at`, `unlink_at`, `set_times_at`,
  `set_times_fd`, `read`, `write`, `pread`, `pwrite`, `fallocate`, `lseek`, `copy_file`, `fsync`,
  `sync_file_range`, `syncfs`, `flock`, `ioctl`, `dup`, `memfd`, `fill_random`, `drain`, `isatty`.
- Waiting and threads: `poll`, `Event`, `Edges`, `Watch`, `ClockWatch`, `gettid`, `set_thread_name`,
  `set_thread_affinity`, `set_thread_nice`, `set_thread_ioprio`, `thread_ioprio`, `clock_gettime`,
  `clock_getres`, `timezone`, `host_vdso` (`HostVdso`), `adjtimex_query`, `RawMode`, `sigwinch_notifier`, `stop_notifier`.
- Network (`net`): `socket`, `close`, `bind`, `connect`, `listen`, `accept`, `sockname`,
  `setsockopt`, `getsockopt`, `get_filter`, `shutdown`, `sendmsg`, `recvmsg`, `interfaces`, `routes`,
  `tx_queue_len`, `source_for`, `sockaddr`, `dns_query`.

## Data flow / lifecycle

### Errors and hosts

- Every function returns the raw host errno (`last_errno` falls back to `EIO`); the kernel wraps it
  and passes it to the guest unchanged, which is correct because both hosts number errnos as the
  guest does.
- Android and Linux share one implementation; per-host divergences are `cfg` branches: DNS
  (`android_res_nsend` vs. the host's resolv.conf servers), `adjtimex` (synthesized on Android,
  where the call is forbidden) and the errno location in signal handlers. Any other target (macOS
  included) stops at a `compile_error!`; on macOS, development goes through `scripts/dev`.

### Memory

- `map_anonymous` maps RW private anonymous memory (guest permissions live in the soft-MMU), adding
  `MAP_NORESERVE` for reservations; `map_inaccessible` maps `PROT_NONE` with `MAP_NORESERVE` (used
  only by the kernel's bug-simulation test hook).
- `map_file` maps shared or private file ranges (private mappings are always RW copy-on-write;
  only shared read-only ones are read-only), rounding the offset down to the host page (16K on some
  devices) and the length up; `map_file_read_only` serves shared read-only mappings;
  `map_file_over` maps a file shared over an existing host range with `MAP_FIXED`, replacing the
  old mapping in one step (the kernel moves a shared file mapping to another object this way).
- `discard` is `MADV_DONTNEED` and `remove` `MADV_REMOVE` (the host punches the hole in the file or
  shared memory behind a writable shared mapping), both on host-page-aligned ranges; residency
  comes from `mincore` (`resident_pages`, `page_residency`)
  or `/proc/self/pagemap` (`touched_pages`, present or swapped bits — zram pages count as touched;
  `has_private_pages`, whether a private file mapping has a page present without the file bit or
  swapped, a copy written since it was mapped), all in 4K units (pages within one 16K host page
  share a result, so `resident_pages` may count 4K pages just outside the range); `cachestat` maps
  the file in chunks to count cached pages.
- `memory_usage` reads `/proc/self/status`; `host_meminfo` copies host `/proc/meminfo` (falling
  back to `sysinfo`); `host_vmstat` and `host_swaps` return host `/proc/vmstat` and `/proc/swaps`
  when readable and not empty (None otherwise);
  long-lived fds for `/proc/self/status` and pagemap are kept in `OnceLock`s.

### Files and storage

- The store opens its object directory with `open_dir` (`O_PATH`) and addresses objects with
  `open_at` (always `O_CLOEXEC`, follows symlinks, applies the umask), `stat_at`, `chmod_at`,
  `set_times_*`, `unlink_at`; `flock` and a shared mapping of the `lock` file coordinate store users.
- `fallocate` passes the Linux `FALLOC_FL_*` mode through (a host filesystem without it answers
  `EOPNOTSUPP`).
- `memfd` makes an empty close-on-exec host file in memory (`memfd_create`): the kernel layer's
  copies of FUSE files for mappings and execve.
- `read`/`write`/`pread`/`pwrite`/`fallocate`/`copy_file`/`flock` retry `EINTR`; others do not; partial
  transfers are returned as is. `copy_file` seeks the output fd and calls `sendfile` (callers fall
  back to chunked copies on `EINVAL`/`ENOSYS`/`EOPNOTSUPP`/`EXDEV`).
- Durability: `fsync(data_only)`, `sync_file_range` (raw syscall, arm64 argument order), `syncfs`,
  `msync`.
- `raise_fd_limit` raises the soft `RLIMIT_NOFILE` at machine start (after the store is opened; the
  result is ignored and `fd_limit` re-reads the limit later); `fill_random` fills the buffer with
  the host's `getrandom` (retrying `EINTR` and short results), so it needs no fd.

### Waiting, threads, CPUs, time

- `poll` uses `ppoll` with nanosecond timeouts (seconds clamped to `i32::MAX`), allocates per call
  and returns `EINTR` to the caller, which treats it as a wakeup. The kernel's waits drop the BKL
  around one `poll` of host fds plus the thread's `Event`.
- `Event::signal` writes 1 (ignoring overflow), `clear` reads until empty.
- `host_cpus` snapshots the process's affinity once (first N CPUs, N = `available_parallelism`
  capped at 64; fallback `0..n`), taken at kernel creation; guest CPU i is `host_cpus()[i]`.
  `cpu_sysfs` reads host CPU sysfs files (unreadable → `None`).
- Per-thread: `gettid`, `set_thread_name` (15 bytes), `thread_cpu_clock` (`pthread_getcpuclockid`),
  `getrusage(RUSAGE_THREAD)` for the calling thread, `/proc/self/task/<tid>/{stat,status}` for
  others; `thread_activity` gives another thread's name and whether it is running, in a host
  syscall (from `syscall`, which only the process itself can read) or else its `stat` state, for
  the BKL watchdog's reports; `set_thread_affinity`/`set_thread_nice`/`set_thread_ioprio` act on host tids and ignore
  failures (the kernel applies its own policy first: nice floor −4, RT ioprio downgraded).
- `clock_gettime`/`clock_getres` pass clock ids through; `timezone` makes the raw gettimeofday
  syscall (glibc's wrapper zeroes the timezone) and returns the kernel's `sys_tz`; `adjtimex_query`
  makes the real read-only call on Linux and synthesizes an unsynchronized state on Android.
- `host_vdso` takes the vDSO's start from `AT_SYSINFO_EHDR` and its length from the `[vdso]` line of
  `/proc/self/maps`, and the vvar start from the contiguous `[vvar*]` mappings right below it; None
  when either is missing.
- CLI: `RawMode::enter` on stdin; `sigwinch_notifier` installs a handler writing one byte to a
  nonblocking pipe, and `stop_notifier` handlers for SIGINT, SIGTERM and SIGHUP writing each
  signal's number to another (each callable once per process; the CLI calls each once). Both
  handlers keep the interrupted code's `errno`, and a failing `sigaction` closes the pipe (and
  restores the stop signals already caught).

### Networking

- Sockets are created `SOCK_NONBLOCK|SOCK_CLOEXEC` (`accept4` likewise); addresses, options and
  control bytes pass through raw (shared arm64 Linux ABI; `sockaddr` writes the Linux layout);
  `sendmsg` always adds `MSG_NOSIGNAL`; `recvmsg` clamps the address to 128 bytes and returns the
  full length with `MSG_TRUNC`; `getsockopt` sizes its buffer by optlen in bytes, so options whose
  optlen means something else need their own call: `get_filter` sizes `SO_GET_FILTER`'s buffer by
  its count of 8-byte instructions; `accept` and `sockname` do not clamp, which is safe because only
  `AF_INET`/`AF_INET6` host sockets exist (AF_UNIX and netlink never reach the hal) and Linux caps
  the reported length at 128.
- `interfaces()` walks `getifaddrs` (one entry per name, index from `if_nametoindex`, flags from
  the first entry, MTU fixed at 65536 for loopback and 1500 otherwise, MAC from `AF_PACKET` entries,
  IPv4/IPv6 addresses with prefix lengths; no broadcast or scope), sorted by index.
- `routes()` parses host `/proc/net/route` and `/proc/net/ipv6_route` (`None` when unreadable, as on
  Android), dropping reject/anycast/cache/local and multicast routes; `source_for(dst)` connects a
  UDP socket and reads its local address; `tx_queue_len` reads sysfs.
- `dns_query`: on Android `android_res_nsend` on the default network (linked against libandroid,
  API 29), up to 30 s, then cancel and `ETIMEDOUT`; the answer is returned whatever its rcode. On
  Linux hosts up to three servers from `/etc/resolv.conf`, UDP with one 3 s deadline per server
  (answers with another ID do not extend it) and ID check, TCP retry on truncation
  (length-prefixed, one deadline), `EHOSTUNREACH` without servers; a failed server moves on to
  the next. Waits on both go on after a host signal interrupts them (`poll_until`).

## Dependencies (inbound / outbound)

### Inbound

- `core/kernel` throughout (mm, store, VFS, syscalls, timer thread, forwarder, raw ICMP, terminal,
  procfs, OOM); `hosts/cli` (`RawMode`, `sigwinch_notifier`, `isatty`, tests).

### Outbound

- `libc` (syscalls, epoll, eventfd, getifaddrs, termios, sigaction), `libandroid` on Android (DNS),
  host files (`/proc/self/status`, `/proc/self/pagemap`, `/proc/meminfo`, `/proc/vmstat`,
  `/proc/swaps`, `/proc/net/route*`,
  `/proc/self/task/*`, `/sys/devices/system/cpu`, `/sys/class/net`, `/etc/resolv.conf`), Rust std
  (`available_parallelism`).

## Invariants and gotchas

- The hal holds no locks and knows nothing of the BKL; every function may run on any thread
  (calling-thread functions — `gettid`, `set_thread_name`, `thread_cpu_clock`, `_self` statistics —
  must run on the guest's own host thread).
- Guest pages are 4K while hosts may use 16K pages: file mappings and residency results are
  translated accordingly.
- `ioctl` is safe to call but trusts the caller's buffer size.
- `getsockopt` relies on the host reading optlen as a byte count; an option whose optlen means
  anything else (`SO_GET_FILTER`'s instruction count) needs a call of its own, or the host writes
  past the buffer.

## Confirmed bugs / technical debt

none

## Open questions

- Should `host_cpus` prefer big cores when the CPU quota is smaller than the affinity set?

## To verify

- Android permissions assumed by comments: `ioprio_*` allowed, `adjtimex` forbidden,
  `/proc/self/task/*` and pagemap readable, cpufreq sysfs blocked.
