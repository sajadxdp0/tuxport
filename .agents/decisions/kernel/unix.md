# AF_UNIX sockets (kernel/unix) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/unix"`
>
> Last distilled: 2026-11-10 (HEAD = 27609fe). D1–D3 were seeded by hand from the retired design doc
> §10 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: AF_UNIX is emulated entirely in process
- **What**: Stream, datagram and seqpacket sockets, `socketpair`, listen/accept and message queues are kernel-layer objects; no host socket is involved.
- **Why**: `SCM_RIGHTS` must pass guest open files and `SO_PEERCRED` must name guest processes; host sockets would carry host fds and host pids.
- **Tradeoffs**: Every AF_UNIX behaviour (merging, boundaries, readiness, credentials) has to be reimplemented after Linux.
- **Watch out**: Stream reads and poll events follow Linux's `unix_stream_read_generic` and `unix_poll` rules.
- **Source**: design review 2026-10-24 (§10), round 06 (`06-m4`)

### D2: Path names are socket nodes in the store; abstract names live in the kernel
- **What**: Binding a path creates an `S_IFSOCK` node in the store (`EADDRINUSE` if it exists) and records its inode; abstract names and autobind names are only in the kernel's name table and disappear with the socket.
- **Why**: Pathname sockets must be visible and permission-checked like files (logging through `/dev/log` was among M4's tests); abstract names need no file.
- **Tradeoffs**: Path names are global across network namespaces (only abstract names are scoped per namespace).
- **Watch out**: Look path sockets up by inode, not by path string.
- **Source**: round 06 (`06-m4`)

### D3: Cycles through in-flight messages are collected like Linux's `unix_gc`
- **What**: When a unix socket is closed (close, close_range, dup2 over it, execve) or a process exits, sockets referenced only by in-flight messages and unreachable from outside have their receive queues emptied.
- **Why**: Sockets passed through themselves formed `Rc` cycles (open file → socket → queue → message → open file) that were never freed.
- **Tradeoffs**: The collector is conservative (any temporary extra reference keeps a socket) and runs synchronously on unix socket closes; Linux 6.8 runs it asynchronously, so it cannot be compared natively and is covered by unit tests.
- **Watch out**: Only unix-socket files can be candidates.
- **Source**: round 19 (`19-deferred`)

### D4: AF_UNIX is a small module of its own
- **What**: kernel/unix covers `unix.rs` and `syscall/unix.rs`, separate from kernel/net.
- **Why**: The retired design doc's §10 gave a single line to a full in-process socket family with its own garbage collector.
- **Tradeoffs**: Its map was verified more lightly (two research concerns, one verifier) than the large modules'.
- **Watch out**: Bugs confirmed at the split or found since stay open in the map, among them stream reads split and merged at the wrong places, credentials attached to messages sent without `SO_PASSCRED`, recvmsg copy faults that can still lose data, sender names taken when sent rather than when received and connected datagram sockets that are never reset.
- **Source**: c3df5d3 (bug list since updated by 2b72eeb, ce8a9ce, 90a11cd and 1a59520)

### D5: AF_UNIX parses socket timeouts with kernel/net's parser
- **What**: Unix sockets use net's `timeval()` for `SO_RCVTIMEO` and `SO_SNDTIMEO` instead of their own copy.
- **Why**: The copy had the same overflow on huge timeouts as the inet parser (kernel/net D10).
- **Tradeoffs**: The two socket families now share one parser.
- **Watch out**: None known.
- **Source**: 96cd8bd

### D6: AF_UNIX files read in a connection's life order
- **What**: unix.rs runs sockets, names, channels, /proc/net/unix and garbage collection, with methods in the order a connection lives and accessors last; syscall/unix.rs runs creation, names and connections, sending and receiving, options and shutdown, addresses and waiting; the `POLL*` constants stay at the top.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; the AF_UNIX files put most helpers above their callers.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file.
- **Watch out**: Code appended at the end of a file drifts from this order.
- **Source**: 2847887

### D7: Zero-length reads and shutdown of unconnected sockets answer as Linux does
- **What**: A stream read of nothing with data queued returns 0 and passes the head message's files (moved, or cloned with `MSG_PEEK`) and credentials while leaving its data, and shutdown sets its flags on any socket.
- **Why**: Programs probing with zero-length reads or shutting a socket down before connecting failed or hung.
- **Tradeoffs**: Shutdown flags on a listening socket change nothing observable.
- **Watch out**: None known.
- **Source**: 2b72eeb

### D8: Accepted sockets inherit the listener's `SO_PASSCRED`
- **What**: accept turns `SO_PASSCRED` on for the new socket when the listener has it, as `unix_sock_inherit_flags` does.
- **Why**: Accepted sockets did not inherit the option.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 5d23297

### D9: Sends and datagram poll follow `unix_stream_sendmsg`, `unix_dgram_sendmsg` and `unix_dgram_poll`
- **What**: A `SOCK_STREAM` send with an address is `EISCONN` when connected and `EOPNOTSUPP` otherwise (seqpacket ignores the address), a datagram send after `SHUT_WR` or to a receiver with `RCV_SHUTDOWN` is `EPIPE` without `SIGPIPE`, and a datagram socket shut both ways polls `POLLHUP`.
- **Why**: A stream sendto with an address went to the peer, datagram sends ignored the receiver's read shutdown and raised `SIGPIPE` after `SHUT_WR`, and a datagram socket shut both ways never polled `POLLHUP`.
- **Tradeoffs**: None.
- **Watch out**: A program that relied on the address being ignored on a connected stream socket now gets `EISCONN`, as on Linux.
- **Source**: dcacfb6

### D10: Dead peers, listener credentials, generic options and recvmsg faults answer as Linux does
- **What**: A datagram socket whose connected peer died is disconnected (`ECONNREFUSED`, then `ENOTCONN`); datagram connect checks `unix_may_send`; listen records credentials each time and wakes connectors when the backlog grows; `SO_PEERCRED` answers for listeners and gives -1 without a peer; generic options go through `SockOpts`; `FIONREAD` sums stream and seqpacket queues and is `EINVAL` on listeners; recvmsg checks the iovec prefix the queued data would fill before taking it, treats a NULL control buffer as empty and closes the passed fds when the control buffer cannot be written; delivery truncates control messages as `put_cmsg` does.
- **Why**: Dead datagram peers, listener `SO_PEERCRED`, generic options and `FIONREAD` differed from Linux, and recvmsg lost data or leaked passed fds when its buffers faulted.
- **Tradeoffs**: Data that arrives while recvmsg waits can still be lost on a copy fault, and connected sends skip the may-send check (both recorded in the map).
- **Watch out**: The prefix check reads the queue length under the socket borrow before blocking.
- **Source**: 5d7b936

### D11: Binding a socket in an overlay reports `IN_CREATE` on the overlay directory
- **What**: bind takes the overlay directory from `create_parent_at` and reports through `fs::created_events`, as mkdir, mknod and symlink do (kernel/vfs D46).
- **Why**: Binding an AF_UNIX socket to a path in an overlay creates a name there, which watches on the overlay directory should see as `IN_CREATE`.
- **Tradeoffs**: The upper directory also gets the `IN_CREATE`, as for the other creations.
- **Watch out**: None known.
- **Source**: d7076fc

### D12: Datagram receive queues are limited by `max_dgram_qlen`
- **What**: Each datagram socket records the namespace's `max_dgram_qlen` at creation and its connected peer (kept by `set_dgram_link`), and sends and a connected sender's `POLLOUT` wait while the receiver's queue holds more than the limit unless the receiver is connected back, as `unix_recvq_full` does.
- **Why**: Datagram receivers had no message limit, so a logger kept sending 212992 one-byte messages to a stopped syslogd where Linux makes it wait after 11, and poll reported such a sender writable.
- **Tradeoffs**: A peer's sends are still limited by payload bytes rather than Linux's per-message truesize, so a socketpair queues more small datagrams than Linux before blocking.
- **Watch out**: Code that relinks a datagram socket without `set_dgram_link` leaves `peer` stale.
- **Source**: 6d9a9e8

### D13: Closing with unread data resets the peer, abstract names are keyed by type, and connections return sender names
- **What**: Dropping a socket whose receive queue is not empty, or a server end not yet accepted, sets `Channel::reset` for its peer, reported once as `ECONNRESET` by a stream read after the queue empties, a seqpacket read before anything queued and `SO_ERROR`, and as `POLLERR` until then; abstract names are keyed by namespace, type and bytes, with autobind per type; and stream and seqpacket messages carry the sender's bound name, a stream read returning the first message's.
- **Why**: Closing with unread data gave the peer only EOF where Linux reports `ECONNRESET`, which servers and clients use to tell a crash from an orderly close, recvfrom on a connection gave no sender name, and an abstract name taken by one socket type blocked every other type, so gVisor's tests for all three were skipped.
- **Tradeoffs**: The sender's name is the one bound when it sent, not at receive time as on Linux, and connected datagram sockets still never get `unix_dgram_disconnected`'s `ECONNRESET` (both recorded in the map).
- **Watch out**: Peers of a program that closes with unread data now see `ECONNRESET` before EOF, as on Linux, and a new way of closing a stream end that bypasses `Drop` would miss the reset.
- **Source**: 90a11cd

### D14: Timed AF_UNIX waits end with EINTR on a signal
- **What**: `block()` answers a signal with EINTR when its deadline came from `SO_RCVTIMEO` or `SO_SNDTIMEO` (every caller's does today) and ERESTARTSYS otherwise, while a stream send or `MSG_WAITALL` read that already moved data still returns its count, as Linux's `sock_intr_errno` does and kernel/net already did (kernel/net D16).
- **Why**: A timed accept, connect, send or receive restarted with its whole timeout under `SA_RESTART`, so signals arriving more often than the timeout (SIGCHLD from busy children, interval timers) kept it from ever timing out.
- **Tradeoffs**: Programs with `SA_RESTART` handlers now see EINTR from timed AF_UNIX calls, as on Linux.
- **Watch out**: A future wait in this module bounded by something other than the socket timeouts must choose its own interrupt errno instead of passing its deadline to `block()`.
- **Source**: 1a59520
