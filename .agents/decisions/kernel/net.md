# Socket layer (kernel/net) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/net"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b). D1–D8 were seeded by hand from the retired design doc
> §10 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: TCP and UDP pass through to host sockets
- **What**: Each guest internet socket owns a nonblocking host socket; addresses, most options and control messages are passed byte for byte, and blocking is emulated in the kernel layer's waits (interruptible by signals, `SO_RCVTIMEO`/`SO_SNDTIMEO` timed by the kernel layer).
- **Why**: Guest and host share the arm64 Linux ABI, so the host's TCP/IP stack can serve the guest directly without an own network stack or privileges; emulated blocking keeps guest threads interruptible and the BKL free while they wait.
- **Tradeoffs**: Host restrictions apply to the guest (no raw sockets, no marks, privileged options checked against the app); no tun device, since that would need a user-space TCP/IP stack.
- **Watch out**: Every path that returns addresses to the guest must undo the boundary rewrites (namespaces, ports, nat, DNS stub).
- **Source**: design review 2026-10-24 (§10), round 06 (`06-m4`); tun rejected in round 180 (`180-original-dst`)

### D2: Raw ICMP is emulated on unprivileged ping sockets
- **What**: A guest raw ICMP/ICMPv6 socket is a host ping socket; a per-namespace aggregation point collects echo replies and `IP_RECVERR` errors (including those of TTL-limited UDP probes) and rebuilds what raw sockets see — IPv4 headers, restored echo ids, full ICMP error messages — for every raw socket of the family.
- **Why**: The app has no `CAP_NET_RAW`; busybox `ping` as root, `ping6`, traceroute and mtr use raw sockets and received nothing from bare ping sockets (no IP header, rewritten ids, no ICMP errors).
- **Tradeoffs**: Only echo requests can be sent; other raw protocols stay `EPERM` as on Android.
- **Watch out**: The timer thread must keep polling the aggregation points so raw sockets become readable.
- **Source**: rounds 125 (`125-raw-icmp`), 128 (`128-traceroute`)

### D3: Netlink is answered in process; unimplemented protocols cannot be created
- **What**: `NETLINK_ROUTE` replies are built from the host's `getifaddrs`, the synthesized routing table and the virtual devices, matching Linux's message layout; `NETLINK_GENERIC` has `nlctrl`; `NETLINK_KOBJECT_UEVENT` sockets never receive; `NETLINK_SOCK_DIAG` opens (D24); any other protocol is `EPROTONOSUPPORT`.
- **Why**: Apps cannot use the host's netlink fully on Android. Creating sockets for unimplemented protocols made PAM's libaudit wait 40 s for an ACK and fail every Debian/Ubuntu login; `EPROTONOSUPPORT` is what Linux returns without the subsystem. Generic netlink always exists on Linux, and programs (iw, ethtool, wg, iotop) expect the socket to open and then look up their family.
- **Tradeoffs**: Multicast notifications only for rtnetlink's link, address and route changes (D26); host interfaces and routes cannot be changed.
- **Watch out**: Single (non-dump) `RTM_GETLINK`/`RTM_GETROUTE` requests are used by iproute2 and scripts and must be answered.
- **Source**: rounds 06 (`06-m4`), 83 (`83-netlink-protocols`), 91 (`91-generic-netlink`), 101 (`101-link-addr`); 39ea702, 7a13dd2

### D4: The routing table is synthesized like Linux's, from routes the app passes in
- **What**: Main and local tables are built as Linux builds them without custom rules; on Android the main table comes from the default network's `LinkProperties.getRoutes()` through `Machine::set_routes`, elsewhere from the host's `/proc/net/route`; `ip route get` lets the host choose the source address.
- **Why**: `ip route`, `route -n`, `netstat -r` and the common `ip route get 1.1.1.1` idiom for finding the primary address returned nothing or `EOPNOTSUPP`; Android apps cannot read the host's routing table.
- **Tradeoffs**: Routes are only as fresh as the app's network callbacks.
- **Watch out**: An empty route list empties the main table instead of falling back to synthesis.
- **Source**: round 98 (`98-routes`)

### D5: DNS goes through a stub at 127.0.0.53 answered by the host resolver
- **What**: The initial namespace intercepts only UDP DNS at 127.0.0.53, resolves through the host on query threads, connects or autobinds the host socket to expose a real local endpoint, caps in-flight queries at 128 and drops queued answers once SO_RCVBUF has been exceeded.
- **Why**: Going through the system resolver makes Android's Private DNS, VPNs and per-network routing apply to the guest; failing fast with SERVFAIL stops resolvers from waiting for their timeouts.
- **Tradeoffs**: Each admitted query starts a host thread, answers can arrive out of order, overload drops queries for resolver timeout, payload lengths approximate skb accounting, and TCP/container DNS is not intercepted.
- **Watch out**: Preserve edited resolv.conf files, keep worker threads independent of kernel lifetime, and leave raw/ping traffic to the host rather than the DNS stub.
- **Source**: rounds 06 (`06-m4`), 07 (`07-m5`); fe4bb2b; 73127176; 20137e4c; d053be24; 39e55b4b

### D6: Low ports first try the real port, then fixed offsets, hidden from the guest
- **What**: Binding a port below 1024 tries the port itself on the host, then +10000, +20000, +30000, then a host-chosen port; the guest always sees its own port, and the app is told about the first non-loopback listen.
- **Why**: The app lacks `CAP_NET_BIND_SERVICE`, so sshd, nginx, caddy and dnsmasq failed on their default ports. Fixed offsets (not +1) keep neighbouring ports free and make mappings predictable; the user agreed this design, including the notification. Google Play system updates (May 2026) opened 22, 80, 443 and a few others, so the real port is tried first.
- **Tradeoffs**: Other devices must use the mapped port, which the user only learns from the notification or the kernel log.
- **Watch out**: A port released within 120 s keeps its host port, so `TIME_WAIT` shows as `EADDRINUSE` rather than a silent remap.
- **Source**: rounds 156 (`156-low-ports`), 208 (`208-low-ports-direct`)

### D7: The kernel layer counts its own traffic
- **What**: Payload bytes and packets of guest sockets are counted per interface and per protocol and reported by `/proc/net/dev`, sysfs `statistics/`, netlink stats and `/proc/net/snmp`.
- **Why**: `/proc/net/dev` was all zeros (ifconfig, htop, nload, btop); apps cannot read the host's counters on Android, and those would include other apps' traffic anyway.
- **Tradeoffs**: Errors, retransmissions and header bytes are not visible; new namespaces show zeros.
- **Watch out**: Count on both send and receive paths, skipping `MSG_PEEK` and the error queue.
- **Source**: round 90 (`90-interface-counters`)

### D8: `SO_ORIGINAL_DST` is answered from records the socket layer keeps
- **What**: Connects rewritten by nat record the original destination keyed by the host 4-tuple; accept claims it; `SO_MARK` is stored on the socket and only used by nftables.
- **Why**: Transparent proxies (`sslocal --protocol redir`, redsocks, clash) ask `SO_ORIGINAL_DST` after accepting; nat happens in Tuxport's socket layer, so the host has no conntrack entry and answered `ENOENT`. The app cannot set host marks.
- **Tradeoffs**: TCP only; no TPROXY.
- **Watch out**: Register the record before releasing the BKL so the peer cannot accept first.
- **Source**: round 180 (`180-original-dst`)

### D9: kernel/net covers internet sockets and the socket syscalls; netlink sockets are kernel/netlink
- **What**: kernel/net covers `net.rs` and `syscall/net.rs` except their netlink sections, `netlink_*` functions and `Socket::Netlink` arms, raw ICMP, the routing table (`route.rs`, with the route and rule messages and `ip route get` that netlink sends), the port map, traffic counters, `/proc/net`'s socket, protocol and route files, the DNS stub and `SO_ORIGINAL_DST`, with host socket wrappers in hal, the namespace hooks described from kernel/netns's side and netlink sockets in kernel/netlink.
- **Why**: The retired design doc's §10 rows for sockets, ICMP, netlink, routes, DNS, low ports and counters had drifted and omitted their limits, and by 2026-11-11 more than half the map postdated its verification and its full refresh reached 48.8 KB, near one Read, with netlink a subsystem of its own.
- **Tradeoffs**: The map's Linux comparisons come from knowledge of the kernel sources, socket dispatch, the changing ioctls and route messages read two maps, and decisions on netlink sockets made before the split keep their kernel/net tags here (D3, D24, D26 and the netlink halves of D19 and D23, listed in kernel/netlink's preamble); nftables rules and evaluation belong to kernel/nft while socket verdict application remains here.
- **Watch out**: Changing socket ioctls must run between kernel/netlink's `rtnl_watched` and `rtnl_notify` (a map invariant), and bugs open in the map include an error after part of a `MSG_WAITALL` read being lost, option passthrough with host semantics, one echo id per raw ICMP socket, raw ICMP sockets that cannot be rebound or woken by a read shutdown, odd sendto destinations reaching the host unchecked and the host's counters in a new namespace's loopback statistics.
- **Source**: c3df5d3 (bug list since updated by 58ce3df, 4e64e74, ce8a9ce, 5d23297, 5d7b936 and 7a13dd2); 0c1b416 (full refresh and the netlink split); 52dcc177; 211a5867; 20137e4c; d053be24; 60790c14; 39e55b4b

### D10: Socket timeouts of KTIME_MAX or more mean none
- **What**: An `SO_RCVTIMEO` or `SO_SNDTIMEO` of at least KTIME_MAX means no timeout and reads back as 0, like Linux's `MAX_SCHEDULE_TIMEOUT`.
- **Why**: `LONG_MAX` seconds, a common "never" value, overflowed the receive and send deadlines.
- **Tradeoffs**: Values between KTIME_MAX (about 9.2e9 s) and Linux's `CONFIG_HZ`-dependent cut-off read back as 0 where Linux returns them.
- **Watch out**: None known; kernel/unix uses the same parser.
- **Source**: 96cd8bd

### D11: Namespace port-map entries tell IPv6-only binds apart
- **What**: The port map's key gains an IPv6-only flag (AF_INET6 with `IPV6_V6ONLY` and a non-mapped address), so such binds get their own host port, and `host_port` prefers that entry for a real IPv6 destination and never for IPv4.
- **Why**: Keyed by namespace, protocol and guest port only, an IPv6-only and an IPv4 socket on one port (nginx's usual pair) got the same host port and their dual-stack host sockets conflicted.
- **Tradeoffs**: An IPv6-only `[::]:P` and a dual-stack `[::]:P` now both bind where Linux refuses the second, and binds on different guest addresses that share a host address are still judged by host address.
- **Watch out**: A new caller of `host_port` must pass the destination's family, or an IPv6 client could reach the IPv4 socket's host port.
- **Source**: ea08993

### D12: Socket files read in a socket's life order
- **What**: syscall/net.rs runs creation, binding to accepting, names, options, sending and receiving (shutdown last), netlink, ioctls, readiness, routing, forwarding, low ports and shared helpers, and net.rs, rawicmp.rs, route.rs, traffic.rs and portmap.rs put callers before helpers, with net.rs's `IPPROTO_*` constants at the top and /proc/net last.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; syscall/net.rs kept its socket calls in no particular order and most of net.rs's helpers sat above their callers.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; `ensure_bound`, `register_udp` and `count_connect` stay with binding although sending and options use them too.
- **Watch out**: Code appended at the end of a file drifts from this order, and a constant paragraph shared by several sections must stay above its first user (`IPPROTO_*` was once carried to the end and moved back by hand).
- **Source**: 412ffba

### D13: Socket read, write, readv and writev are one recv or send
- **What**: `net::rw` scatters one receive and gathers one message send or successive 1 MiB stream sends, suppressing SIGPIPE on later pieces and returning partial progress when a subsequent send hits EPIPE.
- **Why**: Linux's `sock_read_iter`/`sock_write_iter` make each call one recvmsg or sendmsg, while Tuxport wrote 64 KiB chunks per iovec and capped reads at 64 KiB.
- **Tradeoffs**: Messages and direct send/sendto calls remain capped at 1 MiB, while stream write can span pieces; AF_UNIX shares the partial-write rule.
- **Watch out**: A blocking stream write of many MiB now goes in 1 MiB sends, which shows only in partial progress under signals.
- **Source**: b295644; 211a5867; 20137e4c; d053be24; 39e55b4b

### D14: MSG_WAITALL waits for the whole length on TCP
- **What**: recv keeps reading and waiting between pieces until it has the length or meets nonblocking, a timeout, a signal, end of file or a host error, a peek re-reads from the head with the host's `SO_RCVLOWAT` raised for the wait, and recvfrom and recvmsg continue past 1 MiB piece by piece; MSG_TRUNC keeps the received length without copying payload, including continued MSG_WAITALL reads.
- **Why**: Fixed-size readers using MSG_WAITALL got short reads, and `MSG_PEEK|MSG_WAITALL` returned corrupted data, once data arrived in more than one piece.
- **Tradeoffs**: An error after a partial read is lost (the host already took it), a peek against a buffer that can never hold the length returns the part where Linux blocks, and AF_UNIX is not covered.
- **Watch out**: Another thread polling the socket sees the raised `SO_RCVLOWAT` while a peek waits, and a fault on a later piece loses that piece's data.
- **Source**: 58ce3df; 211a5867; 20137e4c; d053be24; 39e55b4b

### D15: recvmsg and sendmsg check iovecs in `import_iovec`'s order
- **What**: A negative length is `EINVAL`, an iovec reaching past 2^48 (arm64's `TASK_SIZE_MAX`) is `EFAULT`, and the total is cut to `MAX_RW_COUNT` by shortening the iovec that reaches it.
- **Why**: recvmsg summed the guest's iovec lengths unchecked and accepted negative lengths.
- **Tradeoffs**: `TASK_SIZE_MAX` is fixed at 48-bit addresses, where a 52-bit Linux accepts up to 2^52.
- **Watch out**: sendmsg shares `read_msghdr` and returns the same new errors.
- **Source**: 09188ec

### D16: Blocking connect honours `SO_SNDTIMEO` and restarts after signals
- **What**: The host's `EALREADY` on a blocking socket joins the wait, a timeout returns the host's own answer (`EINPROGRESS` or `EALREADY`), and a signal is `ERESTARTSYS` without a timeout and `EINTR` with one.
- **Why**: Programs bound connects with `SO_SNDTIMEO` or rely on `SA_RESTART` handlers (SIGWINCH on resize, SIGCHLD) not to break a blocking connect, and Tuxport ignored the first and broke the second.
- **Tradeoffs**: A restarted connect redoes routing and nat before the host says `EALREADY`, though nothing is sent twice.
- **Watch out**: None known.
- **Source**: 5296a0a

### D17: A listen counts as reported only once it is queued
- **What**: The port map checks the 32-event queue's room before marking a listen reported, so a listen dropped while the queue was full is reported when the program listens again.
- **Why**: Marking it first lost the listen for good, so the app was never told about its mapped port.
- **Tradeoffs**: A dropped listen is reported only if the program listens on it again.
- **Watch out**: None known.
- **Source**: 6adbd6c

### D18: Inet binds check their family before the host sees it
- **What**: `inet_bind_addr` checks every inet bind first: a sockaddr shorter than the family's is `EINVAL`, IPv4 takes `AF_INET`, or `AF_UNSPEC` with `INADDR_ANY` rewritten to `AF_INET`, and otherwise `EAFNOSUPPORT`, raw IPv4 ignores the family, and IPv6 takes only `AF_INET6` (`EINVAL` for raw, `EAFNOSUPPORT` otherwise), so the host only ever sees `AF_INET` or `AF_INET6`.
- **Why**: An `AF_UNSPEC` bind passed to the host oopsed the development VM's kernel and hung Tuxport, and other bogus families got the host's (AppArmor's) error instead of Linux 6.1's.
- **Tradeoffs**: Rebinding a raw socket to another address, which `raw_bind` allows, is still refused by the host ping socket underneath (D22), and sendto's destination is still not checked by the layer.
- **Watch out**: Programs relying on the host accepting an odd bind family now get Linux's error, and differential cases that bind with `AF_UNSPEC` need an unconfined native side, since a confined one can still hit the AppArmor oops.
- **Source**: 0b2c3c8

### D19: Netlink dumps fit in a page, and UDP receives end after a read shutdown
- **What**: One `pack` helper fills link, address and route dump datagrams up to 3776 bytes (Linux's first dump datagram on 4 KiB pages), keeping one run per family and `NLMSG_DONE` alone; `SHUT_RD` or `SHUT_RDWR` sets `InetSocket::rd_shut`, also when the host answers `ENOTCONN` as Linux's `inet_shutdown` does, and a blocking recv on a datagram socket with nothing queued then returns 0 before timeouts and signals.
- **Why**: Link dumps came in 8 KiB datagrams that page-sized readers (Go's `net.Interfaces`) cannot read once there are about nine interfaces, and a blocking UDP recv after a read shutdown looped between the host's `EAGAIN` and a readable poll at full CPU, hanging programs that wake receiver threads with shutdown.
- **Tradeoffs**: Readers with large buffers get more datagrams than Linux sends them after the first, dumps are still not resumable for buffers under 3776 bytes, and raw ICMP sockets still wait after a read shutdown (recorded in the map).
- **Watch out**: The read shutdown is a guest-side flag, so a host socket shut down some other way is not seen; a reader that assumes one datagram per family would break (none known).
- **Source**: b3e134a

### D20: accept4 installs only a complete socket, and socket timeouts are parsed strictly
- **What**: accept4 checks fd and flags, reserves an fd before resolving and waiting on the listener, writes the address before installation, releases reservations on errors, inherits timeouts and v6only, and returns EINTR for interrupted timed accepts while retaining strict timeout parsing.
- **Why**: accept4 installed the fd before writing the address and took a connection before finding no free fd, accepted sockets lost the listener's timeouts, and the lenient parser let a negative timeout block forever.
- **Tradeoffs**: A supplied address with a NULL length is EFAULT, including recvfrom after consuming data, and a kernel panic between reserve and fill can leave a reservation until the process ends.
- **Watch out**: Other wait-then-install paths still choose slots without reserving them; preserve the existing short-timeout EINVAL, invalid-microseconds EDOM and negative-seconds-as-zero rules.
- **Source**: 5d23297; 4e01f78b; 20137e4c; d053be24; 39e55b4b

### D21: A refused blocking connect leaves the host socket ready for another connect
- **What**: When a blocking connect fails, the failure branch connects the host socket once more and ignores the answer (`ECONNABORTED`, nothing sent).
- **Why**: After a refused blocking connect the host socket stayed connecting, so the guest's next connect on it got `ECONNABORTED` where Linux sends a new SYN.
- **Tradeoffs**: One extra host syscall on a failed blocking connect.
- **Watch out**: None known; the extra connect goes through `__inet_stream_connect`'s `sock_error` path and sends nothing.
- **Source**: f6a82cc

### D22: Binding a raw ICMP socket again to its own address succeeds
- **What**: A raw socket already bound on the host to the same address returns success without binding again.
- **Why**: A second bind of a raw ICMP socket failed with `EINVAL` because its host ping socket refuses a second bind.
- **Tradeoffs**: Binding to another address is still `EINVAL` (recorded in the map), since recreating the socket would lose its options and its aggregation-point registration.
- **Watch out**: None known.
- **Source**: dcacfb6

### D23: Netlink options, getpeername and socketpair follow Linux's rules
- **What**: `SockOpts` records generic options and reads them back by Linux's rules; netlink sockets gain a type, groups, flags, `SO_PASSCRED` and a send timeout, with bind, connect, autobind, `SOL_NETLINK` options, pktinfo and kernel credentials on receive; `SO_*TIMEO_NEW` map to the old options; getpeername reports the connected peer as Linux does; socketpair returns Linux's `EINVAL`, `ESOCKTNOSUPPORT` or `EPERM`; and rtnetlink change types up to `RTM_MAX` are `EPERM` without `CAP_NET_ADMIN`.
- **Why**: Netlink options were misreported (`SO_TYPE` 0, no memberships, no autobind, `SO_PASSCRED` ignored), `SO_*TIMEO_NEW` had no effect, getpeername echoed the connect address, socketpair's errnos differed, and unprivileged rtnetlink changes answered `EOPNOTSUPP`.
- **Tradeoffs**: Buffer sizes are only recorded, memberships receive only rtnetlink's link, address and route notifications (D26), and autobind does not look for other sockets holding the tgid.
- **Watch out**: Programs that relied on unknown netlink levels being accepted now get `ENOPROTOOPT`, and a wildcard connect from a socket bound to its own address reports the loopback.
- **Source**: 5d7b936, 7a13dd2

### D24: `NETLINK_SOCK_DIAG` opens and answers like a 6.1 kernel without its diag modules
- **What**: `NETLINK_SOCK_DIAG` sockets open, and `sock_diag_request` answers `TCPDIAG`/`DCCPDIAG` with `EOPNOTSUPP`, `SOCK_DIAG_BY_FAMILY` and `SOCK_DESTROY` with `EINVAL` (short body, family at or past `AF_MAX`) or `ENOENT`, and other types with `EINVAL`, honouring `NETLINK_CAP_ACK` and acking non-requests only on `NLM_F_ACK`.
- **Why**: ss printed "Cannot open netlink socket: Protocol not supported" for every table, since `NETLINK_SOCK_DIAG` sockets could not be created.
- **Tradeoffs**: ss shows only what `/proc/net` gives, without socket memory, timers or diag-only details.
- **Watch out**: A program that needs sock_diag data gets `ENOENT`, where it used to fail at socket().
- **Source**: 39ea702

### D25: Socket ioctls nothing takes are counted as unimplemented paths
- **What**: The default arms of `interface_ioctl` and `netns_ioctl` count the request as on a socket in kernel/signal's table of unimplemented paths (kernel/signal D24), while netlink's `FIONREAD` stays uncounted, its `ENOTTY` being Linux's too.
- **Why**: Socket ioctls the interface passthrough does not take (`SIOCOUTQ`, `SIOCGSTAMP`) and unhandled ones in a new network namespace went unrecorded.
- **Tradeoffs**: Requests the host socket itself refuses are not counted.
- **Watch out**: None known.
- **Source**: 99b4216

### D26: rtnetlink link, address and route changes are notified by diffing namespace snapshots
- **What**: Every `NETLINK_ROUTE` socket is registered weakly in `Kernel::rtnl_sockets`, and a change request (an rtnetlink `RTM_NEW`/`DEL`/`SETLINK`, `NEW`/`DELADDR` or `NEW`/`DELROUTE` diffed in `KernelHost::configure`, or an interface, address or route ioctl) snapshots each namespace with a socket in groups 1, 5, 7, 9 or 11 before and after, encoded as dumps, and queues the difference as Linux-ordered `RTM_NEWLINK`, `DELADDR`/`NEWADDR`, `DELROUTE`/`NEWROUTE` and `DELLINK` on every member socket, the sender included, with the group in `nl_groups` and `NETLINK_PKTINFO`, and `ENOBUFS` after 1024 queued datagrams unless `NETLINK_NO_ENOBUFS`.
- **Why**: Joined rtnetlink groups never received anything, so ip monitor, systemd-networkd's sd-netlink and libnl-based tools subscribed and waited forever.
- **Tradeoffs**: The device layer stays untouched and unlistened namespaces cost nothing, but several steps of one request collapse, sysctl and host interface changes are not notified, seq, port and flags are 0, `NLM_F_ECHO` is ignored, the cap counts datagrams rather than `SO_RCVBUF` bytes, and no suite test could be unskipped because their multicast tests change host lo.
- **Watch out**: Each change request with a listener rebuilds the namespace's link, address and route messages twice (quadratic for batches over thousands of addresses, with host getifaddrs and route reads in the boot namespace), and a dump encoding that varies between calls would notify spuriously.
- **Source**: 7a13dd2

### D27: Socket filters retain Linux checks even when the host refuses TCP attachment
- **What**: Filter attachment follows Linux 6.1 capability, optlen, lock and program checks; a host-refused TCP program is validated on a scratch UDP socket and retained for count-based SO_GET_FILTER, detach and accept inheritance, while normal host programs still use `hal::net::get_filter`.
- **Why**: SO_GET_FILTER’s instruction count must not be treated as bytes, and newer hosts’ TCP attachment refusal otherwise breaks Docker’s published-port setup.
- **Tradeoffs**: Retained TCP programs filter no traffic, attachment adds host checks and possibly a scratch socket, and netlink/AF_UNIX filter behavior remains limited.
- **Watch out**: SO_LOCK_FILTER relies on host state, count-based copies remain bounded to 4096 instructions, and test hook 14 is needed to force host refusal on accepting hosts.
- **Source**: c65da9d (recorded in 0c1b416); 52dcc177; 20137e4c; d053be24; 39e55b4b

### D28: Legacy route ioctls translate into namespace route requests

- **What**: SIOCADDRT and SIOCDELRT decode arm64 IPv4/IPv6 structures, check namespace CAP_NET_ADMIN, translate Linux’s masks, metrics, labels and gateway rules, and reuse `netdev::request` with rtnetlink notifications.
- **Why**: BusyBox and net-tools route use these ioctls rather than rtnetlink.
- **Tradeoffs**: Only virtual-device routes change, unreachable and IPv6 source routes are refused, and MTU/window/IRTT flags are ignored.
- **Watch out**: Structure offsets are arm64-specific, and IPv4 unicast classification approximates the local table from interface addresses.
- **Source**: 29e59fd3; 20137e4c; d053be24; 39e55b4b

## Superseded

- ~~DNS stub queries are resolved synchronously in the sending thread (rounds 06, 07)~~ → replaced by **D5** in fe4bb2b (2026-11-05)
- ~~Internet and netlink sockets are one kernel/net module (D9, c3df5d3)~~ → replaced by **D9** and kernel/netlink **D1** in 0c1b416 (2026-11-11)
