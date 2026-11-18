# kernel/unix Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/unix.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 2 research concerns; 9 claims checked by 1 independent verifier — 8 confirmed and 1 partial, corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-10 (targeted verification only since the full mapping: timeouts; reads of nothing; shutdowns; SO_PASSCRED at accept; send addresses; dead peers, listen credentials, options; datagram queue length; resets, typed abstract names, sender addresses; EINTR on timed waits last)

## Responsibilities

`kernel/unix` emulates AF_UNIX sockets entirely inside the kernel layer, never touching host
sockets:

- stream, datagram and seqpacket sockets and `socketpair`;
- pathname names (socket nodes in the store), abstract names and autobind;
- connection lifecycle: listen, connect, accept, shutdown, close;
- data transfer with Linux's merge and boundary rules, `SCM_RIGHTS` with guest open files,
  `SCM_CREDENTIALS` and `SO_PEERCRED` with guest ids;
- readiness for poll/epoll, `FIONREAD`, and `/proc/net/unix`;
- a `unix_gc`-style collector for sockets reachable only through in-flight messages.

Root: `core/kernel/src/unix.rs`, `syscall/unix.rs`.

Outside the module: the generic socket syscalls that dispatch here (`kernel/net`); creating socket
nodes and path resolution (`kernel/vfs`, `kernel/store`); fd tables (`kernel/files`); poll/epoll
mechanics (`kernel/poll`); `/proc/net` file structure (`kernel/procfs`); waiting (`kernel/process`).

## Key types

- `UnixSocket` — type, bound address bytes, `link`, the datagram receive queue `rx`, `peer_cred`,
  `passcred`, `rcvtimeo`/`sndtimeo`, `shut_wr`, `embryo` (a server end not accepted yet), the
  namespace it was created in, and the generic options it only records (`SockOpts`,
  `kernel/net`).
- `Link` — `None`, `Listening { backlog, pending, cred }`, `Stream { rx, tx, peer_addr }`,
  `Dgram { tx: Weak, peer_addr }`.
- `Channel` (`ChannelRef`) — one direction: a queue of `Packet`s, byte count (capacity 212992),
  `eof`, `closed`, `arrivals`/`taken` counters, and for a datagram socket's queue `max_qlen` and
  `peer` (the queue of the socket it is connected to, set by `set_dgram_link`), and `reset` (the
  receiver's pending `ECONNRESET`); `Packet` — data, read offset, passed `FileRef`s, sender `Ucred`,
  sender address; `Received` — one read's result (`len` may exceed the copied data).
- `Ucred` — pid (global tgid), uid, gid (kernel ids).
- `Name` — `Path(Ino)` or `Abstract(namespace id, socket type, bytes)`; `Names` — the kernel-wide
  table of weak socket references plus the autobind counter.
- `Kernel::unix_inflight` — weak references to channels that have queued files.

## Public entry points

- `syscall::unix::{socket, pair, bind, listen, accept, connect, connect_dgram, send, send_dgram,
  recv, setsockopt, getsockopt, sockname, shutdown}`, reached from `kernel/net`'s socket syscalls
  and read/write.
- `UnixSocket::{readiness, wake_objects, arrivals, unread}` for poll, epoll and `FIONREAD`.
- `unix::released` (from close, close_range, dup3 over an fd, close-on-exec, seccomp addfd) and
  `unix::collect` (also at a group's last thread exit); `unix::proc_listing`,
  `unix::inflight_sockets`.

## Data flow / lifecycle

### Creating sockets

- `socket`: protocol 0 or 1 (else `EPROTONOSUPPORT`); stream, datagram, seqpacket (`SOCK_RAW`
  becomes datagram; others `ESOCKTNOSUPPORT`); the socket records a new network namespace if the
  creator is in one.
- `socketpair` (any of the three types) builds two connected stream channels or two datagram links
  pointing at each other; both ends record the creator's credentials as peer credentials.

### Names

- Addresses: over 128 bytes `EINVAL` (generic layer), then 2..110 bytes and `AF_UNIX`; an empty
  path is unnamed, a leading NUL abstract (all bytes kept), otherwise a path cut at the first NUL.
- bind: already bound `EINVAL`. Unnamed: autobind to a sequential `\0%05x` name free in the
  socket's namespace for its type (`EADDRINUSE` when exhausted). Abstract: inserted in the table
  under the namespace and the socket type (`EADDRINUSE` while a live socket of that type holds
  it), as Linux's `unix_abstract_hash` mixes the type in. Path: the parent is resolved; `.`/`..` as the last name `EADDRINUSE`;
  creation checks (search, `EEXIST` → `EADDRINUSE`, `EROFS`, write+search), owner and mode
  `0o777 & ~umask` with setgid-directory rules, tmpfs room (`ENOSPC`), then an `S_IFSOCK` node in
  the store, an inotify `IN_CREATE`, and the name `Path(ino)`.
- Path names are keyed by inode only, so hard links, bind mounts and renames reach the same socket
  and path names are shared by all network namespaces; abstract names and autobind are per
  namespace (the socket's creation namespace) and socket type. Names die with their socket (pruned lazily); path
  nodes stay in the store.

### Connections

- listen: datagram `EOPNOTSUPP`; unbound `EINVAL`; backlog clamped to 4096 (negative → 4096);
  records the listener's credentials, again at every re-listen, which also wakes connectors
  blocked on a full backlog when the backlog grows.
- connect (stream/seqpacket): listening `EINVAL`, connected `EISCONN`; the target is found by
  path (following symlinks; a store socket node needs write permission; anything else
  `ECONNREFUSED`) or by abstract name among the caller's namespace's sockets of its type (none
  `ECONNREFUSED`, so another type's socket of that name is never found); self or not listening
  `ECONNREFUSED`; a path socket of another type `EPROTOTYPE`. With room (backlog + 1 pending)
  a server end is created in the client's namespace and queued; credentials are exchanged (client
  gets the listener's listen-time credentials). A full queue blocks (`EAGAIN` nonblocking or after
  `SO_SNDTIMEO`, interrupted by signals as below) until an accept or the listener closing.
- accept: not listening `EINVAL`; pops a pending end, turns on its `SO_PASSCRED` when the
  listener has it (`unix_sock_inherit_flags`), wakes blocked connectors, returns the peer name
  (2 bytes for unnamed clients); waits bounded by `SO_RCVTIMEO`.
- Datagram connect: `AF_UNSPEC` disconnects; otherwise the target must be a datagram socket
  (`EPROTOTYPE`) not connected to another socket (`EPERM`, `unix_may_send`).
- shutdown: `how` outside 0..2 `EINVAL`; on unconnected and listening sockets the flags are only
  recorded (Linux 6.1 returns 0 there too); `SHUT_RD` closes the own receive channel (the peer's writes get `EPIPE`), `SHUT_WR` marks the
  send channel EOF (the peer reads EOF and polls `POLLIN|POLLRDHUP`, `POLLHUP` once both are shut);
  on datagram sockets the flags are local.
- close: wakes waiters; the peer's receive side gets EOF and the own receive queue (with its
  files) is discarded, so the peer's writes get `EPIPE`; a listener's pending ends are dropped (their
  clients see EOF/`EPIPE`). A stream or seqpacket end closed with anything left in its own receive
  queue, or as a pending end never accepted, resets the connection (6.1's `unix_release_sock`):
  the peer's `reset` is reported once as `ECONNRESET` by its next read (a stream one once its own
  queue is empty, a seqpacket one before anything queued) or `SO_ERROR`, and polls `POLLERR`
  until then.

### Sending and receiving

- Control messages on send: 16-byte headers, 8-byte alignment, at most 253 fds in total; other
  levels or types `EINVAL`; `SCM_CREDENTIALS` must match the sender (pid/uid/gid) unless
  privileged (`EINVAL`, `EPERM`, `ESRCH`); default credentials are tgid, real uid and real gid.
- Stream send: a destination address is `EISCONN` (`EOPNOTSUPP` unconnected; seqpacket ignores
  it); peer gone → `EPIPE` + `SIGPIPE` (unless `MSG_NOSIGNAL`) or the partial count; otherwise
  pushes what fits (files with the first chunk) and blocks for the rest. Seqpacket: over the
  capacity `EMSGSIZE`, otherwise waits until the whole packet fits. Datagram (never `SIGPIPE`):
  after `SHUT_WR` `EPIPE`; dead or closed target `ECONNREFUSED`, and a dead connected peer also
  disconnects the socket (then `ENOTCONN`); a target connected to another socket `EPERM`; a target shut down for reading
  `EPIPE`; over the capacity `EMSGSIZE`; blocks for room: bytes, and unless the target is connected
  back to the sender, the target holding more datagrams than its `max_qlen` (6.1's
  `unix_recvq_full`; `net.unix.max_dgram_qlen` of the namespace the target was created in, 10 by
  default), so 11 queue and the 12th waits.
- Stream and seqpacket sends record the sender's bound name (none when unbound), which recvfrom
  returns: a stream read the first message's, as `unix_stream_read_generic` copies it.
- Stream receive merges packets in order, stopping after the first packet with files and before a
  packet whose credentials differ (even without `SO_PASSCRED`); a read of nothing returns 0 at
  once when something is queued, taking the first message's files (blocking or `EAGAIN` on an
  empty queue, as on Linux); `MSG_PEEK` clones files;
  `MSG_WAITALL` loops until full, files, EOF or an error/timeout. Packet receive pops one packet and
  truncates (`MSG_TRUNC` set; with the `MSG_TRUNC` flag the full length is returned).
- Delivery, as Linux's `scm_recv`: `SCM_CREDENTIALS` first when `SO_PASSCRED` is set (pid and ids
  translated into the receiver's namespaces; with less room than the message, the header and what
  fits, `cmsg_len` cut, `MSG_CTRUNC`), then as many files as fit after a header installed into the
  receiver's fd table (`MSG_CMSG_CLOEXEC`); when the control buffer or the fd limit runs out,
  `MSG_CTRUNC` is set and the remaining files are dropped (closed). A NULL control buffer holds
  nothing whatever its length; one that cannot be written loses its messages but not the data
  (the installed files are closed again, `MSG_CTRUNC`, `msg_controllen` 0). recvmsg first checks
  that the part of the iovecs the data already queued would fill lies in writable mappings
  (`EFAULT`, leaving the data queued). Control
  buffers are capped at 4096 bytes, data at 1 MiB per call.
- Waits interrupted by a signal return `EINTR` when `SO_RCVTIMEO`/`SO_SNDTIMEO` gave them a
  timeout and `ERESTARTSYS` otherwise (Linux's `sock_intr_errno`), so `SA_RESTART` never restarts
  a timed wait with its whole timeout; a stream send or `MSG_WAITALL` read that already moved data
  returns the count instead.

### Options, readiness, listing

- setsockopt: `SO_PASSCRED` (an int, `EINVAL`), `SO_RCVTIMEO`/`SO_SNDTIMEO` (parsed as for inet
  sockets: negative times and times of at least `KTIME_MAX` mean none), the generic options
  `SockOpts` records (buffer sizes, linger, keepalive, rcvlowat, priority and the like, by
  Linux's rules); other `SOL_SOCKET` names are accepted and ignored; other levels `EOPNOTSUPP`.
  getsockopt: `SO_TYPE`, `SO_DOMAIN` 1, `SO_PROTOCOL` 0, `SO_ERROR` (a pending reset's
  `ECONNRESET` once, else 0), `SO_PASSCRED`,
  `SO_ACCEPTCONN`, timeouts, `SO_PEERCRED` (pid, euid, egid recorded at pair/connect time, or a
  listener's own from its last listen, translated into the caller's pid and user namespaces; pid 0
  and uid/gid -1 without either), the recorded generic options; others `ENOPROTOOPT`.
- Readiness: `POLLIN` when data or EOF is readable, `POLLOUT` when the queue is under a quarter of
  the capacity (or the peer is gone, so writers get `EPIPE`) and, for a connected datagram socket,
  its peer does not hold back its datagrams (`unix_dgram_poll`), `POLLRDHUP`/`POLLHUP` from shutdown
  state (a datagram socket's `POLLHUP` once both directions are shut), `POLLERR` while a reset is
  pending; listeners are readable with pending connections. `FIONREAD` (`unix_inq_len`):
  every byte queued on a stream or seqpacket connection, the next datagram's length, `EINVAL` on
  a listener; `SIOCOUTQ` is not implemented.
- `/proc/net/unix`: sockets from fd tables plus in-flight ones, filtered by the reader's namespace;
  state 1 (unconnected/listening), 3 (connected), pending connections as state 2; abstract names
  printed with `@`.

### Garbage collection

- Channels that queue files are recorded. `released` drops closed files and, if any was a unix
  socket, runs `collect`: candidates are unix-socket files all of whose references are in-flight
  (plus the collector's own); references from candidates' own inboxes (including pending
  connections) are subtracted; candidates still referenced are reachable, and so is everything
  reachable through their inboxes; the rest have their inboxes emptied, breaking the cycles.
  `collect` also runs at a group's last thread exit.
- Conservative: any extra strong reference (a blocked thread) keeps a socket out of the candidates;
  epoll holds weak references and does not. Files dropped by `MSG_CTRUNC` or by a socket's own
  drop do not trigger it.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/net` (socket syscalls, read/write, ioctl, readiness, `accept4` installing the fd);
  `kernel/files` close paths (`released`); `kernel/exec` (execve close-on-exec), `kernel/process`
  (last-thread exit); seccomp addfd; `kernel/procfs` (`/proc/net/unix`).

### Outbound

- `kernel/vfs` and `kernel/store` (path lookup, creation checks, `mknod` with its inotify
  `IN_CREATE`, tmpfs accounting); fd tables (`get`, `install`; `kernel/files`); credentials and
  namespaces (`permission`, `new_owner`, `uid_out`/`gid_out`, `pid_in`/`pid_out`, `ns_capable`); the
  kernel's waits and wake objects; signals (`SIGPIPE`).

## Invariants and gotchas

- Sockets reference each other through `Rc` channels; any cycle through in-flight files must be
  broken by the collector.
- Drop must release borrows before dropping queued files, since dropping files can recurse into
  other sockets.
- Credentials are kernel ids and are translated only when shown to a reader.

## Confirmed bugs / technical debt

- **Connected datagram sends skip the may-send check:** a send without an address to a peer that
  has since connected to a third socket goes through, where Linux's `unix_may_send` makes it
  `EPERM` (connect and addressed sends do check).
- **Stream reads split and merge at the wrong places:** reads stop at a change of sender
  credentials even without `SO_PASSCRED` (Linux merges), while `MSG_WAITALL` joins data from
  different writers across iterations and reports the first one's credentials even with
  `SO_PASSCRED`. A zero-byte stream send carrying files still queues the files.
- **Error order on connect:** a non-socket path is `ECONNREFUSED` before the write-permission
  check (Linux: `EACCES` first).
- **Delayed collection:** files dropped by `MSG_CTRUNC` or with a dropped socket's queue do not
  trigger the collector, so their cycles wait for the next unix close or exit (and stay listed in
  `/proc/net/unix` meanwhile).
- **Smaller deviations:** no `SIOCOUTQ`; a datagram queue counts payload bytes, not Linux's
  per-message `truesize` against the sender's buffer; non-`SOL_SOCKET`
  control messages are rejected instead of skipped; `/proc/net/unix` shows connected datagram sockets
  as state 3; buffer sizes are only recorded: the capacity stays 212992, so a small send buffer
  does not make writes block and a larger one does not unblock them, and `SO_RCVLOWAT` does not
  change when stream reads return.
- **Credentials are attached to messages sent without `SO_PASSCRED`:** every message records the
  sender's pid, uid and gid, so a receiver that turns `SO_PASSCRED` on later gets real credentials
  for data sent earlier, where Linux delivers pid 0 and uid/gid 65534 for them (Linux attaches
  credentials only while the sender or receiver has the option). The same stamping causes the read
  splits of **Stream reads split and merge at the wrong places**.
- **recvmsg copy faults can still lose data:** the iovecs are checked only against the data queued
  when recvmsg starts, so data that arrives while it waits, or a buffer that is mapped but faults
  anyway (past the end of a file mapping), is taken off the queue and lost when the copy fails
  (Linux keeps a stream message queued); a datagram whose copy would fault is kept, where Linux
  drops it.
- **Sender addresses are taken when sent:** recvfrom gives the sender's name as it was bound when
  it sent, where Linux reads the sending socket's name at receive time, so a sender bound between
  its send and the receive shows no name here.
- **Connected datagram sockets are never reset:** Linux's `unix_dgram_disconnected` gives a datagram
  socket `ECONNRESET` when the peer it is connected to disconnects or reconnects with this
  socket's datagrams still queued; here none is reported.

## Open questions

- Should `SO_PASSCRED` autobind on connect and send?
- Should the seqpacket send path raise `SIGPIPE` (Linux routes seqpacket through the datagram
  send path)?

## To verify

- Linux 6.1 comparisons above come from knowledge of `af_unix.c`, `scm.c` and `sock.c`, not a
  source checkout; the differential tests cover the common paths natively.
