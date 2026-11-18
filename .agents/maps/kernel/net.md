# kernel/net Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/net.md` for the paired decision history; it still holds the decisions on netlink sockets (now `kernel/netlink`) from before the split.
>
> Verified: 2026-11-11 (full mapping: 5 research concerns; 20 claims checked by 3 independent verifiers — 16 confirmed and 4 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: link the nftables evaluator split from kernel/netns)

## Responsibilities

`kernel/net` implements the socket layer for internet sockets and the socket syscalls every family
goes through:

- TCP/UDP passthrough to host sockets, with blocking, timeouts, signals and a set of emulated
  options handled in the kernel layer;
- raw ICMP and ICMPv6 sockets emulated on unprivileged host ping sockets, including traceroute's
  ICMP errors;
- the routing table, with the route and rule messages and `ip route get` answers that
  `kernel/netlink` sends;
- the DNS stub at `127.0.0.53:53`;
- low-port mapping on the host and hiding it from the guest, and the hooks that hand
  initial-namespace sockets to the inbound forwarder;
- traffic counters for interfaces and protocols, and the socket, protocol and route files of
  `/proc/net`;
- `SO_ORIGINAL_DST` records and `SO_MARK`.

Root: `core/kernel/src/net.rs` (except its netlink sections), `syscall/net.rs` (except its
`netlink_*` functions, its netlink section and the `Socket::Netlink` arms), `rawicmp.rs`,
`route.rs`, `portmap.rs`, `traffic.rs`, `origdst.rs`.

Outside the module: netlink sockets (`kernel/netlink`); network namespaces, address translation,
virtual devices, nftables, namespace port registration and the inbound forwarder itself
(`kernel/netns`); AF_UNIX sockets (`kernel/unix`); poll, epoll and readiness plumbing
(`kernel/poll`); `/proc/net/dev`, `if_inet6`, sysfs and the procfs file structure
(`kernel/procfs`); host socket wrappers and the host resolver (`hal`); waiting and the BKL
(`kernel/process`); the app's network callbacks and listen notifications (`android`).

## Key types

- `Socket` — `Inet`, `Netlink` or `Unix`, shared as `SocketRef` (`Rc<RefCell<Socket>>`) inside
  `FileKind::Socket`. Guest `O_NONBLOCK` lives on the open file.
- `InetSocket` — the host fd (always nonblocking and close-on-exec on the host); family; `ty`, the
  host type (`SOCK_DGRAM` for raw ICMP); `proto`; `raw` (the guest asked for `SOCK_RAW`); `peer`
  (a connected datagram socket's peer, for `/proc/net`) and `guest_peer` (what getpeername
  reports); DNS stub state (`stub_peer`, queued `answers`, `answered`); `rcvtimeo`/`sndtimeo`;
  the cached traffic `route` and `opening` (ActiveOpens counted for this connect); `listening`;
  `rd_shut` (the guest shut it down for reading); namespace state owned by `kernel/netns` (`net`,
  `bound`, `any`, `vaddr`, `wide`, `vconn`, `listener`, `self_port`, `v6only`) and the forwarder
  handle `fwd`; `track` (nat state, seen peers, a blocked verdict, the claimed original
  destination, `SO_MARK`); raw ICMP and tap state (`icmp`, `ttl_set`, `guest_recverr`,
  `udp_tap`, `icmp_err`); port-map `lease`, `local_map`, `peer_map`; `filter` (a TCP socket's
  classic BPF program the host refused, kept as given). Dropping it closes the host fd and removes
  its namespace UDP port and listener.
- `Message` (one recv result), `MsgHdr` (a guest msghdr read in), `Routed` (connect/sendto
  routing result: namespace map, nat, filter verdict, original destinations).
- `route::Entry` — one route (`host` marks entries whose source the host chooses).
- `RawIcmp` (per raw socket: family, last guest echo id, `ICMP_FILTER`, guest `IP_RECVTTL`,
  `IP_PKTINFO` and `IP_RECVERR`, `IPV6_CHECKSUM`, its `Tap`), `Tap` (per-namespace aggregation
  point on a host epoll: sources and receivers), `Source`, `Receiver` (data queue, byte count,
  arrivals, error queue), `Item`/`ErrItem`; `Kernel::icmp_taps` (weak).
- `PortMap` (kernel-wide: offset, `always_map`, entries keyed by `(namespace, protocol, guest
  port, IPv6-only)`, `by_host`, `released`, `reported`, listen events), `Lease` (refcount on an
  entry, released on drop), `ListenEvent` (comm, guest port, host port).
- `Traffic` (per-interface `IfStats`, an address table refetched at most once a second and kept
  5 s, `Protocols` counters), `traffic::Route`.
- `origdst::Table` (host 4-tuple → original destinations per namespace, with age and a cap),
  held as `Kernel::orig_dst`.
- DNS: `Kernel::dns_queries` (query number → weak socket), `Shared::dns_answers` (answers waiting
  for the timer thread).
- `SockOpts` — the generic `SOL_SOCKET` options netlink (`kernel/netlink`) and AF_UNIX sockets only
  record (debug, reuse, dontroute, broadcast, keepalive, oobinline, no_check, buffer sizes,
  priority, rcvlowat, linger), set and read back by Linux's `sk_setsockopt`/`sk_getsockopt` rules:
  an int needed (`EINVAL`), buffer sizes capped at 212992 unless the `FORCE` form (`CAP_NET_ADMIN`),
  then doubled with Linux's minimums, `SO_DEBUG` on needs `CAP_NET_ADMIN` (`EACCES`), priorities
  outside 0..6 `CAP_NET_RAW` or `CAP_NET_ADMIN` over the namespace owner, the read-only ones
  `ENOPROTOOPT`; other `SOL_SOCKET` names are accepted and do nothing.

## Public entry points

- Syscalls: `socket`, `socketpair`, `bind`, `listen`, `accept4`, `connect`, `getsockname`,
  `getpeername`, `sendto`/`sendmsg`/`sendmmsg`, `recvfrom`/`recvmsg`/`recvmmsg`, `setsockopt`,
  `getsockopt`, `shutdown`; read, write, readv and writev on sockets through `net::rw`
  (`read_bytes`, `write_bytes` and AF_UNIX splice through `net::recv`/`send` with flags 0);
  `net::ioctl`; `readiness`, `wake_objects`, `arrivals` for poll/epoll.
- Routes: `route::{tables, merge, get, route_message, rule_messages, proc_route,
  proc_ipv6_route, host_routes}` (`get`, `route_message` and `rule_messages` answer
  `kernel/netlink`'s route and rule requests); `Machine::set_routes`.
- DNS stub: `net::{is_stub, stub_addr, servfail}`, `deliver_dns` (timer thread),
  `hal::net::dns_query`, `vfs::ensure_resolv_conf`.
- Raw ICMP: `tap_for`, `register`, `RawIcmp::{sent, setsockopt, getsockopt, present}`,
  `udp_setsockopt`, `before_io`, `untap`, `dispatch`, `check_taps` (timer thread); `raw_recv`
  and `take_errors` in `syscall/net.rs`.
- Ports: `portmap::{bind, bind_any}`, `PortMap::{maps, host_port, guest_port, listened}`,
  `Machine::take_listens`; `map_dest` (private) and `unmap_addr` (also used by the `/proc/net`
  listings).
- Forwarding hooks: `forward_socket`, `forward_target`, `forward_all`.
- Traffic and `/proc/net`: `Traffic::{account, get}`, `count_connect`; the listings
  `net::{proc_listing, sockstat, sockstat6, proc_snmp, proc_netstat}` and `PROC_ARP`.
- `SO_ORIGINAL_DST`: `origdst::Table::{register, claim}` and `getsockopt(SO_ORIGINAL_DST)`.

## Data flow / lifecycle

### Creating sockets

- `socket`: flags other than the type, `SOCK_NONBLOCK` and `SOCK_CLOEXEC` are `EINVAL`.
  AF_INET/AF_INET6: `SOCK_RAW` with a protocol 1..255 is `EPERM` unless it is ICMP (AF_INET) or
  ICMPv6 (AF_INET6) and the caller has `CAP_NET_RAW` over the network namespace's owner; raw ICMP
  becomes a host ping socket (host `SOCK_DGRAM`) registered with its namespace's tap; every other
  type/protocol (raw with protocol 0 or 256 and above included) goes to the host, which validates
  it. The namespace is recorded only for a new namespace. AF_NETLINK goes to `kernel/netlink`,
  AF_UNIX to `kernel/unix`. Other families (AF_PACKET, AF_VSOCK, …) `EAFNOSUPPORT`.
- `socketpair` is AF_UNIX only: inet first creates the socket as Linux does (a type of
  `SOCK_MAX` or more `EINVAL`, one no inet protocol has `ESOCKTNOSUPPORT`, a protocol the type
  lacks `EPROTONOSUPPORT`, raw without `CAP_NET_RAW` `EPERM`), then `EOPNOTSUPP`; netlink
  `EOPNOTSUPP`; others `EAFNOSUPPORT`. A fault writing the pair closes both fds.

### Binding, listening, connecting, accepting

- bind: sockaddr over 128 bytes `EINVAL`; inet sockets then check the length and family as Linux
  6.1 does first (`inet_bind_addr`: shorter than the family's sockaddr `EINVAL`; IPv4 takes
  `AF_UNSPEC` with the wildcard address as `AF_INET` and refuses other families with
  `EAFNOSUPPORT`, raw IPv4 ignores the family; IPv6 takes only `AF_INET6`, raw IPv6 answers
  `EINVAL`), so the host only sees `AF_INET` and `AF_INET6`; raw sockets drop the port; the
  namespace translation runs (in a new namespace a datagram socket binding the wildcard with port
  0 binds the host wildcard, `wide`); ports below the namespace's `ip_unprivileged_port_start`
  need `CAP_NET_BIND_SERVICE` over the namespace owner (`EACCES`, checked after the translation);
  then the port map (owned ports in new namespaces through `bind_any`, low ports in the initial
  namespace through `bind`) or a plain host bind (a raw socket already bound to the same address
  succeeds without binding again); then namespace registration, `forward_socket` for datagram
  sockets and, after a remap with PREROUTING nat, `forward_all`.
- listen: implicit bind (only in a new namespace; the host autobinds otherwise), host listen,
  then a TCP/MPTCP socket in a new namespace registers its listener once; `forward_socket`; a
  listen on a non-loopback host address is reported to the app.
- connect: in the initial namespace a UDP socket connecting to the DNS stub records `stub_peer`
  and connects its host socket there too (see below). Otherwise `route()` applies OUTPUT nat,
  the namespace translation, PREROUTING nat when entering another namespace and the filter chains
  (`kernel/netns`); a TCP
  reject returns its errno, a drop connects to a blackhole listener (`ETIMEDOUT` without one,
  skipping the bind, the port map, the counters and `SO_ORIGINAL_DST`), no listener in the target
  namespace is `ECONNREFUSED`; then implicit bind, portmap destination mapping and the host
  connect. A `wide` datagram socket connecting into a namespace only records the peer (`vconn`),
  and `AF_UNSPEC` undoes that; a UDP destination the filter blocks is recorded and fails or drops
  later sends.
- Nonblocking connect returns the host's `EINPROGRESS`; blocking (also when the host answers
  `EALREADY` to a connect made again while one is in progress) waits for `POLLOUT` with the BKL
  released, as Linux's `__inet_stream_connect`: `SO_SNDTIMEO` bounds the wait (then
  `EINPROGRESS`, or `EALREADY` for the repeated connect, and the connection goes on), and a
  signal gives `ERESTARTSYS` (restarted under `SA_RESTART`, the restarted call waiting again) or
  `EINTR` when there is a timeout; then it reads `SO_ERROR` and connects again, so the host reports
  `EISCONN` after a success and, after a failure, has reset the socket so that the guest's next
  connect sends a new SYN. TCP connects register `SO_ORIGINAL_DST` records when the host answered
  success or `EINPROGRESS`, before the BKL is released.
- The peer getpeername reports is recorded at connect as Linux reports it: an IPv4 address given
  to an IPv6 socket IPv4-mapped, the wildcard as the loopback (IPv4-mapped when the IPv6 socket's
  local address is), no IPv6 flow information, a scope only for link-local addresses.
  getsockname and getpeername unmap host ports and translate addresses; a raw ICMP socket's local
  port is its protocol number; the stub peer is `127.0.0.53:53`.
- accept4, in `__sys_accept4_file`'s order: a bad fd `EBADF`, bad flags `EINVAL`, then the new fd
  is reserved in the fd table (`EMFILE`; no other thread can take it while this one waits), then
  `ENOTSOCK`; host accept with waits bounded by the listener's `SO_RCVTIMEO` (`EAGAIN` on
  timeout; a signal `ERESTARTSYS`, or `EINTR` when there is a timeout, as AF_UNIX); the new socket
  inherits protocol, local port map (not the lease), namespace and its address, mark, the claimed
  original destination, the listener's `SO_RCVTIMEO`/`SO_SNDTIMEO`, recorded `IPV6_V6ONLY` and a
  filter kept in Tuxport (the host's own filter is inherited on the host); the peer address is
  unmapped, translated and written back before the fd is filled (a non-NULL address with a NULL
  length `EFAULT`, as recvfrom's), so a fault drops the connection and gives the fd back.
- shutdown: inet goes to the host, and `SHUT_RD`/`SHUT_RDWR` set `rd_shut`, also when the host
  answers `ENOTCONN` (Linux's `inet_shutdown` takes effect then too).

### Sending and receiving

- send (sendto, sendmsg, write; sendmmsg one message at a time): AF_UNIX goes to `kernel/unix`,
  netlink to `kernel/netlink`; DNS stub queries go to the stub (see below); a connected datagram
  socket whose peer the filter blocked fails or drops; datagrams with an address go through
  `route()` (`EPERM` or a silent drop from the filter; UDP to an unbound port in a new namespace is
  dropped silently); TCP drops the guest address unless `MSG_FASTOPEN`; destinations are portmapped;
  an `IP_PKTINFO`/`IPV6_PKTINFO` may be appended; the host gets the guest flags minus
  `MSG_DONTWAIT`/`MSG_NOSIGNAL` (the host call always adds `MSG_NOSIGNAL`). Blocking stream sends
  loop until done; `EAGAIN` waits (bounded by `SO_SNDTIMEO`, then `EAGAIN` or the partial count);
  `EPIPE` after part of the data returns the partial count (the next call gets the error),
  otherwise it raises `SIGPIPE` unless `MSG_NOSIGNAL`.
- recv: AF_UNIX, netlink and raw ICMP go to their own code; queued stub answers come first;
  `MSG_ERRQUEUE` never waits; the host gets the flags minus `MSG_DONTWAIT`/`MSG_WAITALL`; a socket
  connected into a namespace (`vconn`) drops datagrams from anyone else; source addresses are
  unmapped, translated and nat-reversed, and UDP peers recorded for the filter's replies; control
  data is returned verbatim; `EAGAIN` checks the stub queue again, then waits (`SO_RCVTIMEO`,
  `ERESTARTSYS` on signals). `MSG_TRUNC` on a stream discards the data the host returns without
  copying any (a NULL buffer included) and reports its length, as `tcp_recvmsg` does, also with
  `MSG_PEEK` and `MSG_WAITALL`. `MSG_WAITALL` on a stream reads until the length is complete,
  waiting between pieces; nonblocking, the timeout, a signal (even under `SA_RESTART`), end of
  file or a host error ends it with what was read, as Linux's `tcp_recvmsg` returns what it
  copied.
  `MSG_PEEK|MSG_WAITALL` peeks from the head each time and waits with the host socket's
  `SO_RCVLOWAT` raised to the length (restored afterwards); when the host fd was ready but the
  peek did not grow (end of file, a receive buffer that can never hold the length) it returns what
  is there instead of spinning. A datagram socket shut down for reading returns 0 from a blocking
  recv with nothing queued, before timeouts and signals, where the nonblocking host socket would
  answer `EAGAIN` while polling readable; queued datagrams come first, and a nonblocking recv stays
  `EAGAIN`. recvmmsg honours `MSG_WAITFORONE` and checks its timeout after each message.
- read, write, readv and writev on a socket are one recvmsg or sendmsg over all the iovecs with no
  address, control data or flags (Linux's `sock_read_iter`/`sock_write_iter`): a datagram or
  seqpacket record is one message whatever the iovecs or the length, a read of nothing returns 0
  at once, and a write of nothing sends an empty datagram (a writev of nothing does not reach the
  socket); a stream socket's write goes on in 1 MiB pieces until all is sent or one is short, the
  pieces after the first sent with `MSG_NOSIGNAL`, so an error after some data returns the count
  without `SIGPIPE`. A fault ends a stream write's data there and fails a message with `EFAULT`.
- Limits: at most 1 MiB per call, 1024 iovecs (`EMSGSIZE`), 4096 bytes of control data each way;
  recvfrom and recvmsg with `MSG_WAITALL` (not `MSG_PEEK`) on an internet stream go on past 1 MiB
  piece by piece, writing each piece out at its offset until one comes up short; msghdr iovecs are
  read as Linux's `import_iovec` does (a negative length anywhere `EINVAL`, then an iovec reaching
  past 2^48 `EFAULT`, the total cut to `MAX_RW_COUNT`); setsockopt values are cut to 256 bytes and
  getsockopt asks the host for at most 256 (`SO_GET_FILTER` aside).
- Host sendmsg/recvmsg run under the BKL (nonblocking); only waits release it. There is no
  BKL-free fast path for sockets.

### Options and ioctls

- `SO_RCVTIMEO_NEW`/`SO_SNDTIMEO_NEW` are taken as the old numbers for every socket kind (the same
  16-byte timeval on a 64-bit kernel).
- Emulated on inet sockets, in this order: `SO_SNDBUFFORCE`/`SO_RCVBUFFORCE` (need `CAP_NET_ADMIN`,
  then become the plain options); raw ICMP's own options (`ICMP_FILTER`, `IP_RECVTTL`,
  `IP_PKTINFO`, `IP_RECVERR`/`IPV6_RECVERR`, `IPV6_CHECKSUM` recorded only, other `SOL_RAW` names
  `ENOPROTOOPT`); on UDP, `IP_TTL`/`IPV6_UNICAST_HOPS` recorded for the raw ICMP taps and still
  passed to the host, and `IP_RECVERR`/`IPV6_RECVERR` intercepted while a tap watches the socket;
  `SO_ATTACH_FILTER`/`SO_ATTACH_REUSEPORT_CBPF`, checked in Linux 6.1.y's order (an optlen under
  4 `EINVAL`; `SO_ATTACH_FILTER` on a TCP socket without `CAP_NET_ADMIN` over the namespace owner
  `EPERM`, as Linux 5d39580f68e6; an optlen other than 16 `EINVAL`; a host filter locked with
  `SO_LOCK_FILTER` `EPERM`; 0 or more than 4096 instructions, or no program, `EINVAL`), then the
  instructions are copied and the pointer rewritten for the host, which checks them; when the host
  refuses a TCP `SO_ATTACH_FILTER` with `EPERM` (a host with 5d39580f68e6, or test hook 14), the
  program is checked by attaching it to a scratch host UDP socket and kept in `filter`, filtering
  nothing; a host attach that succeeds clears `filter`; `SO_DETACH_FILTER` with a kept program
  clears it (`EPERM` while the host filter is locked); `SO_MARK` (needs `CAP_NET_RAW` or
  `CAP_NET_ADMIN` over the namespace owner; stored only, inherited by accepted sockets, fed to
  nftables, which sees 0 outside the sender's namespace); `SO_RCVTIMEO`/`SO_SNDTIMEO` (Linux
  6.1's `sock_set_timeout`: a short buffer `EINVAL`, microseconds outside `[0, 1000000)` `EDOM`, a
  negative time zero (no waiting, `EAGAIN` at once), 0 or at least `KTIME_MAX` no timeout, read
  back as 0 like Linux's `MAX_SCHEDULE_TIMEOUT`; AF_UNIX sockets parse them the same way);
  `IPV6_V6ONLY` in new namespaces (recorded). getsockopt answers `SO_ORIGINAL_DST`, `SO_ERROR`
  (pending ICMP errors first), the timeouts, `SO_MARK`, `SO_TYPE` (`SOCK_RAW` for raw sockets) and
  recorded `IPV6_V6ONLY` itself, and `SO_GET_FILTER` the way Linux's `sk_get_filter` reads it,
  from a kept program or else by asking the host: optlen is a count of 8-byte instructions (passed
  to the host capped at `BPF_MAXINSNS`, 4096, which no attached program exceeds), 0 asks only for
  the count, fewer than the program has is `EINVAL`, and optlen comes back as the program's count.
- Everything else (including `SO_DETACH_FILTER` without a kept program, `SO_LOCK_FILTER`,
  `SO_PEERCRED`, `SO_BINDTODEVICE` — resolved against host interface names —, `SO_PRIORITY`,
  `IP_TRANSPARENT`, `TCP_*`, and `SO_ATTACH_BPF` with its guest fd number) goes to the host and is
  checked against the app's host credentials. Netlink (`kernel/netlink`) and AF_UNIX sockets keep
  their options in process (`SockOpts`).
- ioctls: `FIONREAD`/`SIOCINQ` (stub answers emulated) and `SIOCATMARK` pass
  through; `SIOCGIF*` getters pass through (`SIOCGIFHWADDR`'s family fixed up), setters `EPERM`;
  `SIOCGIFCONF` appends virtual devices; interface ioctls also work on AF_UNIX and netlink sockets
  through a temporary host socket; in a new namespace interface ioctls go to `kernel/netns`, and
  in the initial namespace virtual devices are tried first; anything else is counted as an
  unimplemented path and `ENOTTY` (`SIOCOUTQ`, `SIOCGSTAMP*`).
- `SIOCADDRT`/`SIOCDELRT` on an internet socket read the whole `struct rtentry` (an IPv6 socket's
  `struct in6_rtmsg`; `EFAULT`), need `CAP_NET_ADMIN` over the namespace owner (`EPERM`), and
  become the `RTM_NEWROUTE`/`RTM_DELROUTE` request Linux's `rtentry_to_fib_config` and
  `rtmsg_to_fib6_config` describe (destination family `EAFNOSUPPORT`, mask checks `EINVAL`,
  `rt_dev` by name with `name:label` giving the label's address as source `ENODEV`, `RTF_GATEWAY`
  with a unicast gateway universe-scoped, metric minus one as priority, an added route
  `RTPROT_BOOT`; IPv6 source prefixes `EOPNOTSUPP`), which `kernel/netns`'s `netdev::request`
  carries out as for rtnetlink in the socket's namespace, so only routes through virtual devices
  change; `RTF_MTU`, `RTF_WINDOW` and `RTF_IRTT` are dropped. Other sockets answer `ENOTTY`.
- Loopback in the initial namespace goes straight to the host, so other apps on the device can
  reach guest services.

### DNS stub

- `Vfs::open` writes `nameserver 127.0.0.53` (mode 0644) when `/etc` exists and has no
  `resolv.conf` entry of any kind (files, empty files and symlinks are left alone; a failure is
  only logged); the Android app overwrites Debian's at import.
- Interception: in the initial namespace, a UDP socket (`stub_capable`: not UDP-Lite, a ping or a
  raw ICMP socket, which reach the real `127.0.0.53` on the host's loopback) sending to
  `127.0.0.53:53` (or its IPv4-mapped form), by address or after connecting to it; checked before
  routing and accounting, so nftables never sees these queries and they are not counted; TCP and
  other ports are not intercepted; sockets in new namespaces are not either. Nothing is sent
  through the host socket, but it gets the local end Linux gives: a connect to the stub connects
  it there (127.0.0.1 and an autobound port, and `/proc/net/udp` shows the peer), and a send binds
  an unbound one to the wildcard address with an autobound port.
- Each query is registered by number in `Kernel::dns_queries` (a weak reference to the socket)
  and handed to a new host thread (`dns-stub`; `EAGAIN` if it cannot start), unless 128 queries
  (`DNS_MAX_INFLIGHT`) are already registered kernel-wide, when it is dropped as by a busy server;
  the send returns the query length at once, as a UDP send does. The thread resolves it —
  Android uses `android_res_nsend` on the default network (so Private DNS, VPNs and per-network
  routing apply; 30 s limit, then cancel; the rcode is passed through in the answer); other hosts
  send to up to
  three servers from the host's `/etc/resolv.conf` (3 s each, ID checked, TCP retry on truncation
  with its own 3 s, then the next server; `EHOSTUNREACH` with none); queries under 12 bytes fail —
  turning failures into a synthesized SERVFAIL, then puts the answer in `Shared::dns_answers` and
  wakes the timer thread, whose `deliver_dns` queues it on the socket (dropped if the socket has
  closed, or when the answers queued already exceed its host `SO_RCVBUF`, counted by length) with
  source `127.0.0.53:53` and wakes its waiters. Until then reads are `EAGAIN` or wait as for any
  datagram; readiness and `FIONREAD` follow the queue. The thread holds no kernel object, only the
  answer queue and the timer event.

### Low ports and listen events

- Mapping applies to TCP/UDP (MPTCP as TCP) non-raw sockets: in the initial namespace for guest
  ports 1..1023 (when the offset is nonzero), in new namespaces for every explicitly bound port
  (`bind_any`: reuse the namespace's host port or let the host pick; an AF_INET6 socket with
  `IPV6_V6ONLY` bound to a real IPv6 address gets its own entry and host port). Destinations are
  rewritten by `map_dest` with `host_port`, which takes an IPv6 destination (as seen in the target
  namespace) to an IPv6-only entry first, then the dual-stack one, and an IPv4 destination only to
  the latter.
- `bind` (initial namespace) candidates: if the dual-stack key is live or was released within
  120 s, bind that host port and return its error (so `TIME_WAIT` gives `EADDRINUSE`); otherwise
  the original port (unless `always_map`), the other protocol's host port for the same guest port,
  port + k×offset for k = 1..3, then 0. Only `EADDRINUSE` (or `EACCES` on the original port) moves
  to the next candidate. A new mapping that differs writes a kernel log line. `bind_any` has no
  such reuse.
- Hiding: getsockname/getpeername, accept, recvfrom and `/proc/net` unmap host ports
  (`local_map`, `peer_map`, and the host→guest table, which lingers 10 s after release); connect
  and sendto to a local address with a mapped port use the host port.
- Listen events: every listen on a non-loopback host address (comm, guest port, host port),
  deduplicated per (port, host port) for the kernel's lifetime, at most 32 queued, drained by
  `Machine::take_listens`; a listen dropped by a full queue is not marked reported, so its next
  listen is queued. Listens in new namespaces are on host loopback and are not reported.

### Inbound forwarding hooks

- In the initial namespace, a datagram bind and a listen call `forward_socket`, which registers
  the socket with `kernel/netns`'s forwarder when the namespace's PREROUTING has nat for it
  (`forward_target`: the guest port, `external_nat`, `external_filter` and the mapped host port);
  `forward_all` runs again after a new-namespace bind that remapped a port and after a
  `NETLINK_NETFILTER` change in the initial namespace. Sockets in new namespaces and raw sockets
  are never forwarded.

### Traffic counters and `/proc/net`

- Traffic: payload bytes and packets of initial-namespace sockets (zero-byte stream calls
  skipped), per interface chosen from the socket's local address (cached until the next connect;
  wildcards resolved per call by the peer: loopback on lo, else the default interface), packets
  estimated from the MSS for streams, one per datagram; `Protocols` feed `/proc/net/snmp` and
  `netstat` (IP/UDP/ICMP IPv4 only, ICMP for ping echoes only, TCP both families but only
  `IPPROTO_TCP`, not MPTCP; `CurrEstab` live from `TCP_INFO`, ActiveOpens and AttemptFails from
  connect, PassiveOpens from accept; `Forwarding` fixed at 2; error and retransmission counters
  0; `TcpExt` all 0). DNS stub and AF_UNIX traffic are not counted; `/proc/net/dev` and netlink
  statistics show zeros in new namespaces, while `CurrEstab` there counts that namespace's
  connections. Counters are plain fields under the BKL.
- `/proc/net` files rendered here: the `tcp`, `udp`, `icmp`, `raw` listings (and their IPv6
  versions, filtered to the reader's namespace), `sockstat`, `sockstat6`, `snmp`, `netstat`,
  `arp`, `route` and `ipv6_route`; procfs renders `dev` and `if_inet6`, `kernel/unix` renders
  `unix`.

### `SO_ORIGINAL_DST` and transparent proxying

- TCP connects whose destination nat rewrote register `(host local, host destination)` → the
  original destination per namespace (before OUTPUT nat for the sender's, before each entered
  namespace's PREROUTING); records survive the client's close (like conntrack) and go away when
  accept claims them (for the listener's namespace), after 120 s (pruned lazily), beyond 4096
  entries, or when a later non-nat connect reuses the 4-tuple. The inbound forwarder registers
  none.
- `getsockopt(SOL_IP|SOL_IPV6, SO_ORIGINAL_DST)`: non-TCP `ENOPROTOOPT`, short buffers `EINVAL`;
  the connecting side answers its guest peer when the namespace's nftables tracks connections;
  the accepted side answers the claimed address, or its own local address when tracking; otherwise
  `ENOENT`. No TPROXY: the nft `tproxy`/`TPROXY` names parse but never match.

### Raw ICMP

- A raw ICMP socket is a host ping socket with host `IP_RECVTTL`, `IP_PKTINFO` and `IP_RECVERR`
  (or `IPV6_RECVERR`), registered with its namespace's `Tap` (created on demand; the timer thread
  waits on every Tap's epoll fd and dispatches them all on each wake). Binding drops the port;
  getsockname reports the protocol number; `SO_TYPE` reports `SOCK_RAW`. nftables nat and filters
  do not apply to it; the namespace translation does.
- Sending records the guest's echo id (one per socket, the last echo request of 8 bytes or more
  sent, recorded even if the send fails) and passes the ICMP message to the host unchanged (no IP
  header); the host enforces echo-only and rewrites the id. An ICMP error already recorded for the
  socket fails the send; a host error that ICMP can cause makes it drain the socket and retry once.
- UDP sockets that set a TTL are tapped lazily at their next send, recv or `SO_ERROR` when a raw
  socket of the same family exists in the namespace and the guest has not enabled `IP_RECVERR`
  itself: host `IP_RECVERR` is turned on and the fd becomes an error-only source; connected UDP
  sockets sending elsewhere are reconnected on the host; the tap is removed when the guest turns
  `IP_RECVERR` on, or at the next I/O once no raw socket is left.
- Dispatch drains error queues (skipping non-ICMP entries) and rebuilds whole ICMP error messages
  (quoted IP header, quoted UDP header or echo request with the guest id restored, checksums),
  applies Linux's `sk_err`/`IP_RECVERR` rules (with `IP_RECVERR` every error is queued, at most
  64; without it only hard errors from the connected peer set the socket error), then drains echo
  replies, restoring an id that is some raw socket's host ident to that socket's last guest id.
  Every live raw socket of the same family whose `ICMP_FILTER` passes gets a copy of replies and
  errors (queue capped at 212992 bytes of ICMP data, overflow dropped).
- Receiving prepends a synthesized IPv4 header (TTL from the host, protocol 1, translated
  source, `IP_PKTINFO` destination, checksum) or returns ICMPv6 as is; `IP_PKTINFO`/`IP_TTL`
  control messages only when enabled, other control messages only for what reached the socket's
  own ping socket; `MSG_TRUNC`/`MSG_CTRUNC` as Linux; `MSG_ERRQUEUE` returns the socket's own
  errors (`EAGAIN` at once when there are none). Readiness comes from the receiver queues
  (`POLLOUT` always, `POLLERR` while errors are pending).

### Routing table

- Main table: the host's routes from `Machine::set_routes` (Android: the default network's
  `LinkProperties.getRoutes()`, unicast only on Android 13 and later, IPv4 metric 0 and IPv6 1024
  for the default route, 256 otherwise; an empty list on network loss, which leaves the main table
  empty), else the host's `/proc/net/route` and `/proc/net/ipv6_route` (reject, local, multicast
  and cache routes dropped; an empty file gives an empty table), else, when those cannot be read,
  synthesized from interface addresses with a default route on the interface the host would use.
  Local table: local, broadcast, loopback, IPv6 local and multicast entries derived from addresses
  of interfaces that are up. Virtual-device routes are merged in; new namespaces have no host main
  table.
- `ip route get`: families other than IPv4 and IPv6, and `RTA_IIF`, are `EOPNOTSUPP`;
  local-table hits are answered in process; virtual routes and new namespaces choose the source
  themselves; otherwise the source is `RTA_SRC` or what the host chooses by connecting a UDP
  socket, and the interface and gateway follow; a 464xlat fallback covers hosts with no main
  route; `RTM_F_FIB_MATCH` returns the matched route with its own table, while other IPv4 answers
  always name table main. Rules: IPv4 0/32766/32767, IPv6 0/32766, and the multicast routing
  families' 32767. `kernel/netlink` sends these route, `ip route get` and rule messages.
  `/proc/net/route` lists the IPv4 main table's unicast routes and `ipv6_route` the IPv6 main and
  local tables.

## Dependencies (inbound / outbound)

### Inbound

- The syscall dispatcher; `kernel/files` (read/write/ioctl on sockets), `kernel/vfs` (`Vfs::open`
  writing `resolv.conf`); `kernel/poll`; `kernel/procfs` (`/proc/net`, sysfs counters); the machine
  (timer thread `check_taps` and `deliver_dns`, `set_routes`, `take_listens`); `kernel/netns`
  (forward targets); `kernel/netlink` (route and rule messages and `ip route get`, `SockOpts`, the
  timeout parser, traffic counters for link statistics, `forward_all` after a ruleset change).

### Outbound

- `tuxport_hal::net` (host sockets, `dns_query`, `interfaces`, `routes`, `source_for`, epoll
  `Watch`); `kernel/netns` (`to_host`, `from_host`, `implicit_bind`, nat, filter, listener
  registration), `nft`, `netdev`, `forward`; `kernel/netlink` (netlink sockets dispatched there,
  `rtnl_watched`/`rtnl_notify` around changing ioctls, `message` and `attr` for route messages);
  `kernel/unix`; `kernel/nft` (NAT/conntrack presence and verdict types); credentials (`ns_capable`); `kernel/netns`'s `netdev::request` for the route
  ioctls; the
  kernel's waits (`wait_objects`, `unlocked`), wake objects; signals (`SIGPIPE`); the kernel log;
  the table of unimplemented paths; the test hooks (`kernel/process`, host-refused TCP filters).

## Invariants and gotchas

- Host sockets are always nonblocking; guest blocking is emulated, and every wait must release
  the BKL.
- Addresses are rewritten at the boundary in both directions (namespaces, ports, nat, DNS stub);
  anything returning an address to the guest must go through the same unmapping.
- A socket ioctl that can change an interface, address or route must stay in `kernel/netlink`'s
  `CHANGING_IOCTLS` and run between `rtnl_watched` and `rtnl_notify`, or rtnetlink listeners miss
  the change (the kernel/netlink map).
- A bind must not reach the host with `AF_UNSPEC`: Ubuntu's 6.8 kernels (colima's 6.8.0-117) oops
  in AppArmor's `aa_inet_bind_perm` on one from a confined process, and the host thread dies
  holding the BKL, hanging every guest process. bind rewrites the family to `AF_INET` first.
- The portmap key has no address, so binds of one guest port share one host port and the host
  decides their conflicts, except that IPv6-only binds in new namespaces get their own.
- The raw ICMP id rewrite assumes one echo id per raw socket (the last one sent).
- A `MSG_PEEK|MSG_WAITALL` wait changes the host socket's `SO_RCVLOWAT` while the BKL is released,
  so another thread polling the same socket meanwhile sees the raised low-water mark.
- `dispatch` of the raw ICMP taps must run with no socket borrowed.
- Options passed to the host meet the host kernel's rules, which change with the device's kernel:
  a host with Linux 5d39580f68e6 (in 6.1.y since June 2026) refuses `SO_ATTACH_FILTER` on a TCP
  socket without `CAP_NET_ADMIN`, which the app never has, so such programs live in `filter` and
  every path that reads, replaces, detaches or inherits a filter must look there before the host.

## Confirmed bugs / technical debt

- **An error after part of a `MSG_WAITALL` read is lost:** the partial count is returned, but the
  host has already taken the error (a reset, say), so the next read sees end of file where Linux
  reports the error then.
- **Option passthrough uses host semantics:** privileged options (`SO_PRIORITY` above 6,
  `IP_TRANSPARENT`) are judged by the app's host credentials (a privileged guest gets `EPERM`);
  `SO_BINDTODEVICE` names are resolved against host interfaces; `SO_ATTACH_BPF` passes a guest fd
  number to the host.
- **A TCP filter the host refused filters nothing:** on a host with Linux 5d39580f68e6 the program
  is only kept in Tuxport, so a drop-everything filter (dockerd's on each port it publishes, until
  its DNAT rules are in place) still lets connections through and other programs see unfiltered
  data; a guest filter that the host accepts on an older kernel does filter.
- **`ip route get` with `iif` is `EOPNOTSUPP`:** Linux 6.1 answers input-route lookups
  (`RTA_IIF`).
- **`/proc/net/snmp` departs from the namespace's state:** `Forwarding` is always 2 although each
  namespace has `ip_forward` (Linux shows 1 when it is on), and MPTCP sockets are left out of the
  Tcp counters and `CurrEstab`, where Linux counts their subflows.
- **Raw ICMP keeps one echo id per socket:** every reply to a socket's ping ident is rewritten to
  the last id that socket sent, so programs using several ids on one socket (liboping) see them
  collapse; the queue cap ignores `SO_RCVBUF` and drops silently.
- **New-namespace loopback statistics leak the initial namespace's:** sysfs `lo/statistics/*` in a
  new network namespace reads the initial namespace's counters for `lo` (and `tx_queue_len` the
  host's real value), while `/proc/net/dev` and netlink show zeros (and 1000) there.
- **A wildcard connect from a bound IPv4 socket reports the loopback:** connecting to `0.0.0.0`
  gives getpeername `127.0.0.1` even when the socket is bound to an address of its own, which
  Linux's route lookup would take instead.
- **A raw ICMP socket shut down for reading still waits in recv:** the aggregation point's receive
  path does not look at the read shutdown, so a blocking recv with nothing queued sleeps until a
  reply, a timeout or a signal, where Linux's `skb_recv_datagram` returns 0 at once (and wakes
  receivers already waiting); only internet datagram sockets on host sockets return 0.
- **Odd destinations reach the host unchecked:** sendto hands a guest address the layer does not
  understand to the host socket as is, so the host kernel's own checks answer instead of Linux
  6.1's; under AppArmor that is `EAFNOSUPPORT` for a bogus family where Linux says `EINVAL`.
- **A raw socket cannot be bound to another address:** Linux's `raw_bind` lets a raw socket be
  bound any number of times; the host ping socket that emulates a raw ICMP socket refuses a second
  bind, so binding it again succeeds only for the address it already has (`EINVAL` otherwise).

## Open questions

- Should container namespaces reach the DNS stub (for example through the gateway address), and
  should nftables see stub queries?
- Should AF_PACKET report `EPERM` (Linux without `CAP_NET_RAW`) instead of `EAFNOSUPPORT`?

## To verify

- Host behaviour for passed-through options (`SO_PRIORITY`, `IP_TRANSPARENT`, `SO_PEERCRED` on
  inet) and that the host ping socket rewrites the id, rejects non-echo types and refuses a second
  bind.
- `SO_ORIGINAL_DST` for a DNAT whose target is in the initial namespace (Linux answers the
  pre-DNAT address; here the forwarder records nothing).
- On a real phone, that other apps can reach guest services on loopback (the design expected it
  and wanted it documented for users; not yet confirmed on a device), and how `android_res_nsend`
  behaves with Private DNS and VPNs.
