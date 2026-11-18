# kernel/netlink Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/netlink.md` for the paired decision history; decisions from before the split are D3, D24, D26 and the netlink halves of D19 and D23 in `.agents/decisions/kernel/net.md`.
>
> Verified: 2026-11-11 (split out of the kernel/net full mapping of 2026-11-11; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: repoint dependencies to the split module maps)

## Responsibilities

`kernel/netlink` implements AF_NETLINK sockets in process, with every reply synthesized and queued
on the socket:

- `NETLINK_ROUTE`: queries answered from the host's interfaces, the routing table and the virtual
  devices; change requests handed to `kernel/netns`'s device layer; link, address and route
  notifications;
- `NETLINK_GENERIC` with `nlctrl` only;
- the `NETLINK_NETFILTER` entry point to nftables;
- `NETLINK_SOCK_DIAG`, refusing every query as a 6.1 kernel without its diag modules does;
- `NETLINK_KOBJECT_UEVENT` sockets that never receive.

Root: in `core/kernel/src/net.rs` the sections netlink, rtnetlink notifications,
`NETLINK_GENERIC`, Netlink messages and `NETLINK_SOCK_DIAG`; in `core/kernel/src/syscall/net.rs`
the `netlink_*` socket functions (`netlink_bind`, `netlink_connect`, `netlink_autobind`,
`netlink_admin`, `netlink_listen_unprivileged`, `netlink_setsockopt`, `netlink_getsockopt`), the
netlink section (`netlink_send`, `rtnl_watched`, `CHANGING_IOCTLS`, `rtnl_notify`, `KernelHost`)
and the `Socket::Netlink` arms of the socket calls.

Outside the module: the socket syscalls' dispatch, the generic `SOL_SOCKET` options (`SockOpts`),
the routing table with its route and rule messages and `ip route get` (`route.rs`), traffic
counters, the timeout parser and the ioctls that changing ioctls wrap (`kernel/net`); the device
layer and namespace ids (`kernel/netns`), nftables (`kernel/nft`); mq_notify (`kernel/ipc`); readiness plumbing
(`kernel/poll`). `net.rs`'s unit-test module, which also tests netlink, stays with `kernel/net`.

## Key types

- `NetlinkSocket` — protocol, type, `pid`, datagram queue (each with the multicast group it went
  to, 0 for answers), arrival counter, `overrun`, `rcvtimeo`/`sndtimeo`, namespace,
  `ext_ack`/`cap_ack`, joined groups, the other `SOL_NETLINK` flags (bit n for option n),
  `passcred`, `SockOpts`. `Kernel::rtnl_sockets` holds every `NETLINK_ROUTE` socket weakly.
- `Host` trait (interfaces, stats, routes, `tx_queue_len`, uid; with defaults configure,
  `peer_net`, links, tables, `all_interfaces`, `init_net`, `net_admin`, netfilter), implemented by
  `KernelHost` (kernel, credentials, namespace, pid namespace, fds, the namespaces `watched` for
  notifications and the `notes` a request caused); `Link` (host interface or virtual device);
  `Snapshot` (a namespace's link, address and route message bodies).

## Public entry points

- The `Socket::Netlink` arms of `socket`, `bind` (`netlink_bind`), `connect`
  (`netlink_connect`), `getsockname`/`getpeername`, `listen`, `accept4`, `shutdown`,
  `setsockopt`/`getsockopt` (`netlink_setsockopt`/`netlink_getsockopt`), send (`netlink_send`),
  recv, `FIONREAD` and readiness.
- `NetlinkSocket::handle` (`route_request`, `generic_request`, `sock_diag_request`,
  `Host::netfilter`); `NetlinkSocket::{subscribed, notify, ngroups, sockaddr}`.
- Notifications: `rtnl_watched` and `rtnl_notify` (around rtnetlink sends and the changing
  socket ioctls), `KernelHost::{snapshots, changed_since, configure}`, `Snapshot::{take,
  changes}`.
- Helpers other modules use: `configure_host_link` (`kernel/netns`'s device layer), `message`
  and `attr` (the device layer, namespace routes and `kernel/net`'s routing table).

## Data flow / lifecycle

### Sockets

- `socket`: `SOCK_RAW`/`SOCK_DGRAM` with ROUTE, SOCK_DIAG, NETFILTER, KOBJECT_UEVENT or GENERIC
  (others `EPROTONOSUPPORT`, other types `ESOCKTNOSUPPORT`); ROUTE sockets join `rtnl_sockets`.
  socketpair is `EOPNOTSUPP` (`kernel/net`).
- bind (Linux's `netlink_bind`): a whole `sockaddr_nl` of `AF_NETLINK` (`EINVAL`); groups need a
  protocol anyone may join (all but `NETLINK_NETFILTER`) or `CAP_NET_ADMIN` over the namespace
  owner, as do rtnetlink's multicast-routing groups (`EPERM`), and are cut to the protocol's
  count (36 for `NETLINK_ROUTE`, 32 for the others); a bound socket must keep its `nl_pid`
  (`EINVAL`); an unbound one takes the given id or, for 0, the caller's tgid in its pid namespace
  (ids are not checked for uniqueness); the groups replace the first 32 joined, and getsockname
  reports them. connect (`netlink_connect`): `AF_UNSPEC` does nothing, otherwise a whole
  `sockaddr_nl` (`EINVAL`), a port or group needing `CAP_NET_ADMIN` (`EPERM`); the peer is not
  recorded, and getpeername always reports `{AF_NETLINK, 0, 0}`. connect and send autobind an
  unbound socket to the tgid. shutdown does nothing; listen and accept `EOPNOTSUPP`.
- send ignores the destination address, control data and flags, and handles each message
  synchronously (a message under 16 bytes or overrunning the datagram ends the batch), queuing
  reply datagrams with group 0; `NETLINK_NETFILTER` hands the whole datagram to nftables (and a
  change of the initial namespace's ruleset re-runs the inbound forwarding hooks);
  `NETLINK_KOBJECT_UEVENT` swallows it. recv first takes a pending overrun (`ENOBUFS` once), then
  pops one datagram (`MSG_PEEK` clones; oversized datagrams are truncated with `MSG_TRUNC` and the
  rest lost); source `{AF_NETLINK, 0, group mask}`; with `NETLINK_PKTINFO` a pktinfo control
  message (the group, 0 for answers), then with `SO_PASSCRED` the kernel's credentials (pid 0, uid
  and gid 0 as the receiver sees them), cut as `put_cmsg` cuts them; with nothing queued,
  nonblocking, `MSG_DONTWAIT`, `MSG_ERRQUEUE` or an expired `SO_RCVTIMEO` give `EAGAIN` and a
  signal `ERESTARTSYS`. Readiness: `POLLOUT` always, `POLLIN` with data, `POLLERR` while an
  overrun is pending. `FIONREAD` is `ENOTTY`, as on Linux.
- Options: `SOL_SOCKET` reports the type, protocol, `AF_NETLINK`, `SO_PASSCRED`, the timeouts
  (`SO_SNDTIMEO` only recorded), `SO_ERROR` (takes a pending overrun's `ENOBUFS`, else 0),
  `SO_ACCEPTCONN` 0, `SO_PEERCRED` pid 0 and uid/gid -1, and the `SockOpts` ones; other names are
  accepted and ignored by setsockopt and `ENOPROTOOPT` to getsockopt. `SOL_NETLINK` (value read
  only when a whole int): `NETLINK_ADD_MEMBERSHIP`/`DROP` (permission as for bind; the set is
  allocated first, then group 0 or beyond the count `EINVAL`), `NETLINK_LIST_MEMBERSHIPS` (32-bit
  words while they fit, optlen the whole set's size, 0 before any group), and the flags
  `NETLINK_PKTINFO`, `BROADCAST_ERROR`, `NO_ENOBUFS`, `LISTEN_ALL_NSID` (`CAP_NET_BROADCAST` over
  the namespace owner), `CAP_ACK`, `EXT_ACK`, `GET_STRICT_CHK` (reading one needs room for an int,
  `EINVAL`); `NETLINK_PKTINFO` adds the control message, `NETLINK_CAP_ACK` shapes error acks of
  device-layer changes, nftables and sock_diag, `NETLINK_EXT_ACK` adds extended acks to
  device-layer and nftables errors, and `NETLINK_NO_ENOBUFS` affects overruns; the others are only
  recorded; other names and levels `ENOPROTOOPT`.

### rtnetlink notifications

- Every `NETLINK_ROUTE` socket is registered (weakly) in `Kernel::rtnl_sockets`. A send on
  `NETLINK_ROUTE` first collects the namespaces that have a socket in the link, IPv4/IPv6 address
  or IPv4/IPv6 route groups (1, 5, 7, 9, 11), the sender included; for a change request
  (`RTM_NEW`/`DEL`/`SETLINK`, `NEW`/`DELADDR`, `NEW`/`DELROUTE`) `KernelHost::configure`
  snapshots each of them before and after the device layer runs it, even when it fails (a request
  can change another namespace: moving a link, a veth's peer); a socket ioctl that may change an
  interface, address or route (`SIOCSIF*`, `SIOCDIFADDR`, `SIOCADDRT`/`SIOCDELRT`) is wrapped the
  same way.
- The difference becomes messages in Linux's order: new or changed links (`RTM_NEWLINK`,
  statistics ignored), addresses gone and new, routes gone and new (an address with the same
  family, prefix length, interface and addresses, or a route with the same family, destination
  length, TOS, table, destination and priority, that changed in place is only new, as `ip route
  replace` is), links gone. After the sender's borrow ends they are queued on every registered
  socket of that namespace that joined the group, with seq, port and flags 0, and each such socket
  is woken.
- A socket holding 1024 datagrams drops the notification and, unless `NETLINK_NO_ENOBUFS`, sets
  `overrun`; answers, nftables replies and mq_notify cookies (the other asynchronous messages)
  are queued without that cap.

### `NETLINK_ROUTE`

- Change requests (`NEW`/`DEL`/`SET` for links, addresses, routes) need `CAP_NET_ADMIN` over the
  namespace owner and go to `kernel/netns`'s device layer; other change types Linux 6.1 knows (up
  to `RTM_MAX`) are `EPERM` without it and `EOPNOTSUPP` with it (host interfaces cannot be
  changed; requests that change nothing succeed). Errors are acked through `ack_ext`, successes
  only on `NLM_F_ACK`.
- Queries: link dumps (filtered by master and kind), single links by index or name (`EINVAL`,
  `ENODEV`), address dumps (per family, IPv4 first), route dumps (per family) and `ip route get`
  (both built by `kernel/net`'s routing table), rule dumps (one datagram; `EAFNOSUPPORT` for a
  family without rules), nsid (by pid or fd), neighbour dumps (empty) and single neighbour gets
  (`ENOENT`); others `EOPNOTSUPP`; control messages are acked only on `NLM_F_ACK`.
- Link, address and route dumps are packed into datagrams of at most 3776 bytes (Linux's
  `NLMSG_GOODSIZE` on 4 KiB pages, which bounds a dump's first datagram), so readers with a
  page-sized buffer (Go's `syscall.NetlinkRIB`, behind `net.Interfaces`) get whole messages; dumps
  are not resumable, so a datagram larger than the reader's buffer is lost.
- In the initial namespace interfaces and addresses come from the host's `getifaddrs` (MTU fixed,
  MAC from `AF_PACKET` entries, host statistics and queue length) plus its virtual devices; new
  namespaces see only their own devices, with zero statistics and a queue length of 1000.

### `NETLINK_GENERIC`, `NETLINK_SOCK_DIAG` and the others

- `NETLINK_GENERIC`: `nlctrl` only, modelled on 6.8 (the differential tests run on a 6.8 host,
  where 6.1 would report a different maxattr): a short header `EINVAL`, unknown families
  `ENOENT`; `GETPOLICY` only as a dump (`EINVAL` without a family, `ENOENT` for another family,
  `EOPNOTSUPP` for nlctrl); the `GETFAMILY` dump ends with `NLMSG_DONE` in the same datagram.
- `NETLINK_SOCK_DIAG` answers as 6.1 does without any family diag module (`sock_diag_rcv_msg`):
  the old `TCPDIAG_GETSOCK`/`DCCPDIAG_GETSOCK` `EOPNOTSUPP`; `SOCK_DIAG_BY_FAMILY` and
  `SOCK_DESTROY` `EINVAL` for a body under two bytes or a family at or past `AF_MAX` (46), else
  `ENOENT`; other types `EINVAL`; each as an error ack honouring `NETLINK_CAP_ACK`, non-requests
  and control messages acked only on `NLM_F_ACK`. iproute2's ss takes `ENOENT`/`EOPNOTSUPP` there
  silently and reads `/proc/net` instead.
- `NETLINK_NETFILTER` goes to nftables (`kernel/nft`). `NETLINK_KOBJECT_UEVENT` never receives.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/net`: the socket syscalls dispatch netlink sockets here (creation, names, options,
  send, recv, readiness), and its ioctl path runs the changing ioctls between `rtnl_watched` and
  `rtnl_notify`.
- `kernel/netns`: the device layer calls `configure_host_link` and builds netlink messages with
  `message` and `attr`; `kernel/nft` uses the same encoding for ruleset replies.
- `kernel/net`: `route.rs` encodes route messages with `message` and `attr`.
- `kernel/ipc`: mq_notify queues `SIGEV_THREAD` cookies on a netlink socket.
- `kernel/poll` through `kernel/net`'s readiness.

### Outbound

- `kernel/net`: the routing table's `route_message`, `get` and `rule_messages`, `SockOpts`, the
  timeout parser, traffic counters for link statistics, and the inbound forwarding hooks
  (`forward_all`) after an initial-namespace ruleset change.
- `kernel/netns`: `netdev::{request, base, views, tables, interfaces, nsid, nsid_alloc}`.
- `kernel/nft`: `nft::handle`.
- `tuxport_hal::net` (`interfaces` through `getifaddrs`, `tx_queue_len`); credentials
  (`ns_capable`); the kernel's waits and wake objects.

## Invariants and gotchas

- Netlink replies are snapshots taken at send time; notifications are differences between
  snapshots, so what a request changes counts, not how (several steps of one request collapse,
  and a namespace nobody listens to is never snapshotted).
- Notifications are queued only after the sender's borrow ends, and `rtnl_watched` skips sockets
  it cannot borrow.
- Every `NETLINK_ROUTE` send walks `rtnl_sockets`, and each change request with a listener renders
  the watched namespaces' link, address and route messages twice.

## Confirmed bugs / technical debt

- **Netlink dumps are not resumable:** a reader whose buffer is smaller than a dump datagram (at
  most 3776 bytes, a whole rule dump in one) gets `MSG_TRUNC` and loses the rest, where Linux
  sizes later datagrams to the reader's buffer and continues the dump on the next recv.
- **Only rtnetlink link, address and route changes are notified:** groups other than rtnetlink's
  link, address and route ones (neighbours, rules, netconf, nsid, other protocols) never receive
  anything; changes made through sysctls (`disable_ipv6`) or to the host's own interfaces (boot
  namespace) are not notified; notifications carry seq, port and flags 0 (Linux copies the
  request's seq and port into address and route ones, and new routes carry
  `NLM_F_CREATE`/`EXCL`), `NLM_F_ECHO` is not honoured, and the queue cap counts datagrams (1024),
  not `SO_RCVBUF` bytes.
- **Netlink ids are only recorded:** `NETLINK_CAP_ACK`/`EXT_ACK` are ignored for query and
  generic-netlink errors; ids are not checked for uniqueness, so a second socket of a process
  autobinds to the same tgid (Linux picks a negative number) and an explicit id another socket
  holds is not `EADDRINUSE`.
- **Netlink sockets differ from Linux's outside requests:** shutdown returns 0 (Linux's
  `sock_no_shutdown` is `EOPNOTSUPP`); getpeername always reports `{AF_NETLINK, 0, 0}` (Linux
  reports the connected port and group); handled rtnetlink types run without `NLM_F_REQUEST`,
  where Linux skips such messages and acks them with 0 on `NLM_F_ACK`; answers, nftables replies
  and mq_notify cookies queue without limit, where Linux drops replies with `ENOBUFS` beyond
  `SO_RCVBUF`, so a program sending requests without reading grows the queue without bound.
- **Netlink requests are not validated as Linux validates them:** `NETLINK_GET_STRICT_CHK` is
  accepted and ignored (no header checks, no header-based dump filtering), attributes of the wrong
  size are skipped instead of rejected (`ERANGE`; `EINVAL` for a short IPv6 `RTA_DST` of `ip route
  get`, while a longer one is used by its first bytes), and a single IPv6 `RTM_GETADDR` is
  `EOPNOTSUPP` without Linux's header checks (`EINVAL` for a short body); malformed or mis-typed
  requests, such as a route dump carrying an `ifinfomsg` or an `ip route get` with a short
  `RTA_FLOW`, are answered with data where Linux returns an error.
- **Single `RTM_GETNEIGH` requests always answer `ENOENT`:** the header and address are never
  checked, so a bad state or type, a missing or wrong-size `NDA_DST` (`EINVAL`), an unknown family
  (`EAFNOSUPPORT`) and an unknown interface index (`ENODEV`) are all reported as "not found", and
  `ip neigh get` with a wrong device or address family prints a misleading error.
- **Netlink destination addresses are ignored:** sendto/sendmsg on a netlink socket never looks at
  `msg_name`, so a message addressed to another port or to multicast groups is handled as a kernel
  request, without `netlink_sendmsg`'s checks (`EINVAL` for a short or non-`AF_NETLINK` address,
  `EPERM` for sending to a group or port without `CAP_NET_ADMIN` on rtnetlink).
- **`nlctrl`'s `GETPOLICY` dump of nlctrl itself is `EOPNOTSUPP`:** Linux returns the policy, and
  takes `CTRL_ATTR_FAMILY_ID` before the name.

## Open questions

None.

## To verify

- "Byte-identical to Linux" for link, address, route and rule messages beyond the differential
  cases that compare dumps: link messages omit `IFLA_AF_SPEC` and use a fixed MTU.
