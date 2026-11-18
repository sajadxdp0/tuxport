# I/O multiplexing (kernel/poll) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/poll"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b). D1–D6 were seeded by hand from the retired design doc
> §10 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.
>
> timerfd decisions made before timerfd moved from kernel/vfs to kernel/poll keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D23 and the timerfd halves of D16 and D41.

## Active

### D1: poll, select and epoll are implemented in process over one wait mechanism
- **What**: Every multiplexer scans readiness of guest files (host fds polled with zero timeout) and waits through the kernel's wait, which puts host fds and the thread's wake event in one host poll; the guest epoll is not a host epoll.
- **Why**: Guest files mix host fds and in-process objects (pipes, PTYs, eventfds, AF_UNIX); one wait covering both is simpler, and a host epoll would only cover host fds.
- **Tradeoffs**: Epoll still traverses interests and needs temporary storage for eligible level-triggered host fds, which are polled in cursor order with scalar retry on batch failure.
- **Watch out**: Preserve per-registration guest masks and report order under maxevents, keep single-event, edge-tracked and exclusive probes scalar, and release the BKL and include the thread wake event when waiting.
- **Source**: design review 2026-10-24 (§10), round 06 (`06-m4`); 5b5ee4b6, 123ddb76

### D2: `EPOLLET` fires on every new arrival, as Linux's wait-queue wakeups do
- **What**: Per-direction data generations (bumped at each event) and drain counts decide when an edge-triggered interest reports again; host sockets get a private edge-triggered host epoll so each host arrival counts.
- **Why**: tokio/mio's waker never reads its eventfd and relies on each write re-reporting; the earlier "not ready → ready" approximation lost the second wakeup and `uv pip install --offline` hung forever (eventfd, pipes, AF_UNIX and TCP all behaved that way on Linux).
- **Tradeoffs**: Every readiness source must maintain generation counters and drains must be detected from syscall results.
- **Watch out**: Leave reported-and-still-ready bits out of waits, or the host poll returns at once and busy-loops.
- **Source**: round 42 (`42-epoll-edges`)

### D3: Nested epolls re-report when inner interests see new events
- **What**: An inner epoll's generation is the sum of its interests' event counts, and an outer edge-triggered interest fires again when it grows.
- **Why**: Linux's `ep_poll_callback` wakes the outer epoll on every inner wakeup, even when the interest is already on the ready list; Tuxport reported only the first event.
- **Tradeoffs**: Generations only grow; removed interests' shares are kept in a base.
- **Watch out**: Edge-watching an inner epoll must propagate host edge tracking inward.
- **Source**: round 47 (`47-nested-epoll`)

### D4: `EPOLLEXCLUSIVE` reports and wakes only one waiter
- **What**: Each edge is claimed by one epoll, and exclusive waiters sleep on a per-file gate watched by the timer thread, which wakes only the first and also waits until the earliest deadline of the gated timerfds.
- **Why**: Several processes waiting on one listening socket were all woken (host poll for TCP, the file-class broadcast for AF_UNIX); Linux wakes only one exclusive waiter.
- **Tradeoffs**: Wakeups go through the timer thread, which then follows every kernel change while gates exist.
- **Watch out**: Gated waiters must still see deadlines and edges of every kind of file.
- **Source**: rounds 42 (`42-epoll-edges`), 48 (`48-exclusive-wakeup`); f5c09f7

### D5: Waiters are woken only by changes to the objects they wait on
- **What**: Readiness sources wake their own objects (and the lock channel for locks); poll/epoll waiters register exactly the objects of their files.
- **Why**: Any pipe, socket, eventfd or PTY operation woke every thread parked in poll/epoll (8 idle processes woke 520,000 times; socketpair round trips went from 30 to 56 µs), hurting Go's netpoller, libuv, asyncio and Java NIO.
- **Tradeoffs**: Every module must name precise wake objects.
- **Watch out**: A missing wake request becomes a hang, not just a slowdown.
- **Source**: rounds 49 (`49-targeted-wakeups`), 53 (`53-locks-host-sockets`), 54 (`54-mqueue-fasync`)

### D6: Signal-driven I/O is checked by the timer thread with the same counters
- **What**: `O_ASYNC` files are checked by the timer thread, which sends `SIGIO` or the `F_SETSIG` signal (with band and fd) to the owner when new events appear, using the edge-triggering counters.
- **Why**: `O_ASYNC`, `F_SETOWN*` and `F_SETSIG` used to be recorded only; the timer thread already wakes on kernel changes and polls host sockets.
- **Tradeoffs**: Delivery waits for the timer thread to run.
- **Watch out**: Re-notify when more data arrives on an already-readable file, as Linux does.
- **Source**: rounds 19 (`19-deferred`), 54 (`54-mqueue-fasync`)

### D7: I/O multiplexing is a module of its own, with timerfd
- **What**: kernel/poll covers `syscall/poll.rs` and `syscall/timerfd.rs`: the readiness model, ppoll and pselect6, epoll, eventfd2, timerfd and signal-driven I/O.
- **Why**: The retired design doc's multiplexing section described EPOLLET, nesting and exclusive wakeups but not the readiness contract, select semantics or signal-driven I/O permissions, and timerfd, which left kernel/vfs with inotify, AIO and loop devices, is an anonymous readiness file like eventfd.
- **Tradeoffs**: The readiness sources stay in their own modules and are only described here as a contract, and timerfd's clocks, Expiry and worker belong to kernel/time, BKL and generic waits to kernel/process, and ProcEvent producers span kernel/procfs and kernel/cgroup.
- **Watch out**: Bugs confirmed at the split or found since stay open in the map, among them exclusive gates that ignore direction and order, epoll results lost on `EFAULT` and pipe `POLLOUT` counting bytes rather than buffers.
- **Source**: c3df5d3, cc160c1 (bug list since updated by 09188ec, 4e64e74, ce8a9ce, f6a82cc and 5d7b936, and shortened by D4); cf029f9a; 8a17e4a9; d053be24; 39e55b4b

### D8: select waits only for what its sets ask and clamps nfds to Linux's table size
- **What**: nfds is clamped to an estimate of Linux's fd table size from the highest open fd (64, then 128 · 2^k), bits above it are neither read nor written back, only events in an fd's masks end the wait, and all three masks include `POLLNVAL` as native select does.
- **Why**: select ended its wait on events it then did not report, and refused nfds values Linux accepts.
- **Tradeoffs**: Linux's table never shrinks and a forked child's is sized by its parent's open fds, so a set bit on a closed fd just above the estimate is ignored where Linux returns `EBADF` (recorded in the map).
- **Watch out**: A hung-up host socket in the except set is no longer polled, so a later event on it is noticed only on another wakeup or the timeout.
- **Source**: 5c179b0

### D9: poll.rs reads from the syscalls down to the readiness model
- **What**: Shared constants stay at the top, then ppoll and pselect6, epoll, waiting (the exclusive gates in trigger order), signal-driven I/O, the readiness model and data generations, and eventfd last.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; poll.rs started with the readiness helpers and ended with the syscalls that use them.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; the readiness model the module docs describe first now comes near the end.
- **Watch out**: Code appended at the end of the file drifts from this order.
- **Source**: d795d07

### D10: epoll_ctl refuses loop devices
- **What**: `Device::Loop` and `Device::LoopControl` join epoll_ctl's `EPERM` list, while poll keeps reporting them ready.
- **Why**: Linux's block devices and loop-control have no poll operation, so epoll_ctl refuses them (kernel/vfs D21).
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: a26c8b4

### D11: eventfd reads scatter over iovecs
- **What**: eventfd reads go through `read_scattered`, so the 8-byte value may span iovecs, while writes stay per-iovec as Linux's plain write operation does (kernel/vfs D22).
- **Why**: eventfd readv checked each iovec separately.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: b295644

### D12: Pipe write ends count arrivals, and O_ASYNC registers only files with fasync
- **What**: `Pipe::room` counts reads that make the pipe writable and is the write end's arrival count, and `register_fasync` registers only pipes, sockets, terminals, inotify and host fds.
- **Why**: Without an arrival count on the write end, a writer draining with a short writev could miss `POLLOUT` under `EPOLLET`, and `O_ASYNC` raised `SIGIO` for file kinds without fasync (every read of a regular file).
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 5d23297

### D13: Signal-driven I/O signals only processes its setter may signal
- **What**: `OpenFile::owner_ids` keeps the uids of whoever set the owner, and `send_to_owner` sends only to targets whose real or saved uid matches one of them, or to any when the setter's euid is root, checking each process of a group, a thread owner by its own credentials and a process by its main thread's.
- **Why**: An unprivileged user could point a file at a root process and kill it with `SIGIO` or an `F_SETSIG` signal.
- **Tradeoffs**: A group owner now scans the thread groups once for members and once more per member for the check.
- **Watch out**: The uids are kernel ids, as Linux's are, and a setter that later changes its own ids keeps the rights it had at `F_SETOWN`, as on Linux.
- **Source**: f6a82cc, d9e1f0c (credentials per thread since 1be1142)

### D14: epoll and ppoll follow Linux 6.1's error order and write-backs
- **What**: epoll_ctl copies the event first, then checks for `EBADF`, `EPERM`, `EINVAL` and `ELOOP`; epoll_wait checks its buffer with `access_ok` before the epoll fd; an interrupted ppoll writes zero revents back; and a pipe write end without readers reports `POLLOUT` when it has room, plus `POLLERR`.
- **Why**: These differed from 6.1, so the matching gVisor tests had to be skipped.
- **Tradeoffs**: None known.
- **Watch out**: Edge-triggered waiters on reader-less pipes now see `POLLOUT` too.
- **Source**: 5d7b936

### D15: Anonymous fds report exactly their Linux poll bits
- **What**: eventfd reports `POLLIN`/`POLLOUT`, timerfd and signalfd `POLLIN`, and an epoll fd `POLLIN|POLLRDNORM`, as their Linux poll operations do, and epoll.sh checks every anonymous fd kind's mode, poll bits and write result against native (kernel/vfs D41).
- **Why**: eventfd, timerfd and signalfd reported `POLLRDNORM`/`POLLWRNORM`, which `eventfd_poll`, `timerfd_poll` and `signalfd_poll` never set, and an epoll fd lacked the `POLLRDNORM` that `ep_eventpoll_poll` reports, as a native 6.8 run confirmed.
- **Tradeoffs**: A poll asking these fds only for `POLLRDNORM`/`POLLWRNORM` now waits, as on Linux.
- **Watch out**: A nested epoll interest asking only for `POLLRDNORM` now fires on a ready inner epoll and takes the edge-wait path while it is not ready; watch nested-epoll users.
- **Source**: e6bfdf8

### D16: AIO polls wait through the same `Waits` as ppoll and epoll
- **What**: Two helpers over the private `Waits` serve native AIO: `wait_aio_polls` for io_getevents (the polls' files plus the context's object) and `watch_aio_poll` for the timer thread (host fds and timerfd expirations) (kernel/vfs D42).
- **Why**: AIO's waits need the same view of a file's readiness sources (in-process objects, host fds, timerfd deadlines) that ppoll and epoll build with `Waits`.
- **Tradeoffs**: AIO waits inherit everything `Waits` does for nested epolls and exclusive gates, which AIO polls rarely need.
- **Watch out**: `watch_aio_poll` drops `Waits`' gated files, so a poll on an epoll whose interests are all `EPOLLEXCLUSIVE` relies on the timer thread's all-channel wake-ups.
- **Source**: fffe1c1

### D17: `/dev/fuse` is always writable and readable while its connection has requests
- **What**: A `/dev/fuse` file reports writable always, readable while a request or an interrupt for a held request waits, and `POLLERR` when its connection is aborted or unattached, and waits on the connection's reader object (kernel/fuse D1).
- **Why**: FUSE daemons may poll `/dev/fuse`.
- **Tradeoffs**: One wake object per connection wakes every reader on each new request.
- **Watch out**: None known.
- **Source**: c008749

### D18: Changed mountinfo and cgroup2 files report `POLLERR|POLLPRI` until a poll takes the change in
- **What**: Readiness reports procfs's `ProcEvent` files with their Linux base bits plus `POLLERR|POLLPRI` while changed, with their counter as wake object and read arrival, epoll_ctl refuses only the synthetic files `pseudo::pollable` rejects, and `took_change` takes a mount change in when ppoll, pselect6 or epoll reports `POLLPRI` or `POLLERR` for `mounts` or `mountinfo` (kernel/procfs D38).
- **Why**: epoll refused every synthetic file but kmsg and readiness had no notion of a changed file, while gVisor's PollMountInfo expects a mount change reported once and taken in by the report, as Linux's `mounts_poll` records the namespace's event.
- **Tradeoffs**: A poll that does not ask for `POLLPRI` still takes the change in through the `POLLERR` it always reports, as on Linux.
- **Watch out**: A new report path that returns events without calling `took_change` would report a mount change forever.
- **Source**: affb1e2

## Superseded

- ~~One host probe per fd during an ordinary level-triggered epoll scan under D1~~ → replaced by **D1** in 123ddb76 (2026-11-17).
