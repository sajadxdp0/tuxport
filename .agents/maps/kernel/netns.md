# kernel/netns Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/netns.md` for the paired decision history, including pre-split decisions for kernel/nft.
>
> Verified: 2026-11-03 (full mapping: 6 research concerns; 19 claims checked by 3 independent verifiers — 13 confirmed and 6 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: split kernel/nft into separate maps)

## Responsibilities

`kernel/netns` emulates Linux network namespaces without a network stack of its own: guest sockets
stay host sockets, and everything a namespace changes is applied at the socket boundary. It
provides:

- network namespaces as host loopback blocks, with address translation in both directions;
- namespace-local ports and listener registration (`ECONNREFUSED`, UDP drops);
- virtual devices (bridge, veth, dummy) with addresses, routes and sysctls, and the mapping of
  their IPv4 addresses to host loopback addresses;
- namespace-path construction and socket-boundary integration for `kernel/nft` evaluation;
- forwarding of connections from outside Tuxport to published (DNAT) ports.

Root: `core/kernel/src/netns.rs`, `netdev.rs`, `forward.rs`, `netconf.rs`.

Outside the module: the socket syscalls that call into it, host interfaces, routes and low-port
mapping in the initial namespace (`kernel/net`); netlink socket mechanics (`kernel/netlink`);
nftables rulesets, requests and expression evaluation (`kernel/nft`, `nft.rs`);
creating and entering namespaces, nsproxy and `/proc/<pid>/ns` (`kernel/ns`); `/proc/net` and sysfs
file structure (`kernel/procfs`); host sockets (`hal`).

## Key types

- `NetNs` (in nsproxy) — id, owner user namespace, `host_net` (the 127.a.b block; `None` for the
  initial namespace, which is the host network), `lo` (`Loopback`: up, IPv4 addresses, `::1`,
  MTU, `self_udp`), `devs` (`Devices`), `nft` (`Nft`), `listeners`.
- `World` — every live namespace (initial first), built per call; `Use` (`Bind` → `EADDRNOTAVAIL`,
  `Reach` → `ENETUNREACH`); `Mapped` (host address, target namespace, implicit-bind source,
  virtual address); `Role` (`Peer` or `Local`).
- `Listener` — id, port, address (or wildcard), family, `v6only`, UDP flag.
- `Devices` — devices, index counter, routes through virtual devices (`Rt`), `NetConf`, nsids,
  `hostmap` (initial namespace: virtual IPv4 address → 127.255.x.y); `Dev` (index, name, kind,
  up, flags, MTU, MAC, txqlen, group, alias, peer/master as weak references, IPv4/IPv6 addresses,
  per-device settings, carrier counters); `View` (read-only snapshot for netlink, procfs, sysfs).
- `NetConf` / `Conf` — interface and namespace sysctls (`all`, `default`, per device; host
  interfaces and lo keyed by index).
- `forward::Forwarder` (per kernel; one host thread), `Shared`, `Target` (protocol, host
  destination, source address), `Handle` (in `InetSocket::fwd`), `Conn`, `Relay`, the global
  `OUTSIDE` registry (127.254.x.y → real client address and port).
- `netns::NatPacket`, `Flow`, `Step` — synthetic packet construction and hook routing into `kernel/nft`.

## Public entry points

- Namespaces: `Kernel::new_net_ns`, `Kernel::world`; translation `to_host`, `init_to_host`,
  `from_host`, `from_self`, `toward_self`, `implicit_bind`, `local_toward`, `entering`,
  `owner_of_host`; listeners `add_listener`, `listening`.
- Devices: `netdev::request` (rtnetlink changes), `link_message`, `addr_messages`, `nsid`,
  `nsid_alloc`, `ioctl`, `ifconf`, readers (`views`, `interfaces`, `base`, `tables`, `entries`,
  `local_v4`, `route_v4`, `routes_v6`, `first_v4`), sysctls (`conf_dirs`, `conf_read`,
  `conf_write`, `ip_forward`), `Devices::for_new_ns`.
- Forwarding: `forward_target`/`forward_socket`/`forward_all` (in the socket layer),
  `Forwarder::add`, `forward::run`, `outside_peer`.
- nftables integration: `output_nat`, `prerouting_nat`, `path_filter`, `external_nat`,
  `external_filter` call `kernel/nft` with the namespace path and synthetic packet.

## Data flow / lifecycle

### Namespaces

- Created by clone/unshare with `CLONE_NEWNET` (`CAP_SYS_ADMIN` in the new credentials' user
  namespace). The block is the first n in 1..=65535 whose bytes [a, b] are free and not 255
  (reserved for the initial namespace's virtual addresses) or 254 (outside clients); the first
  namespace gets 127.0.1.0/24; no free block is `ENOMEM`; a block is reused once its namespace is
  dropped. lo starts down without addresses; IPv4 sysctls are copied from the initial namespace,
  IPv6 ones start at defaults.
- The kernel holds weak references; threads' nsproxies, sockets (inet, netlink, unix) and
  namespace files keep a namespace alive. A socket keeps the namespace it was created in, even
  after setns. Dropping a namespace drops its devices; a veth whose peer died is pruned lazily.

### Address translation

- Binding in a new namespace: the wildcard and `::` → 127.a.b.1 (flag `any`); 127.0.0.x →
  127.a.b.x (bindable even with lo down); other 127/8 addresses `EADDRNOTAVAIL`; the namespace's
  own virtual IPv4 address → 127.a.b.1 (recording `vaddr`); anything else `EADDRNOTAVAIL`. IPv6
  `::1`/`::` become `::ffff:127.a.b.1` (host sockets stay dual-stack). A wildcard UDP bind to port
  0 gets a real host wildcard bind (`wide`) so it can also reach outside.
- Reaching from a new namespace: 127.0.0.x needs lo up and a covering address; `::1` needs lo up
  with IPv6; the own virtual address → 127.a.b.1 with source 127.a.b.254; another namespace's
  virtual address (through a route over a virtual device) → its 127.c.d.1, or 127.255.x.y for the
  initial namespace's; an address nobody owns, routed through a gateway, goes to the host network
  unchanged (the host binds the source); a miss on a directly connected subnet `EHOSTUNREACH`; no
  route `ENETUNREACH`. Only IPv4 virtual addresses are mapped; IPv6 virtual addresses cannot be
  bound or reached — in a new namespace IPv6 destinations matching any route over a virtual device
  (on-link and link-local included) are handed to the host network, and in the initial namespace
  all IPv6 goes to the host unchanged.
- Unbound sockets are bound implicitly before connect, sendto or listen: 127.a.b.1 (or
  `::ffff:127.a.b.1`; host `::1` for ICMPv6; 127.a.b.254 for UDP to the own virtual address); none
  when the destination goes to the host network.
- The initial namespace: its virtual addresses map to permanent 127.255.x.y entries; destinations
  in other namespaces map to their 127.c.d.1; 127.x and everything else pass through unchanged.
- Back to the guest (`from_host`, by who is looking): wildcard for `any` sockets; 127.254.x.y →
  the real outside client; own 127.a.b.x → 127.0.0.x (`::1` for IPv6 sockets at .1); 127.a.b.254 →
  the first virtual address; another namespace's 127.c.d.1 → that namespace's address facing the
  viewer; 127.255.x.y → the initial namespace's virtual address; non-loopback local ends → the
  namespace's route source toward the peer.
- `/proc/<pid>/net` lists the sockets created in the target's namespace, translated through each
  socket's own namespace.

### Ports and listeners in a namespace

- Explicit nonzero binds keep the guest port and let the host choose (`portmap::bind_any`, keyed
  by namespace, protocol, guest port and whether the socket takes only IPv6 — an AF_INET6 socket
  with `IPV6_V6ONLY`, which gets a host port of its own; no address in the key, and the guest's
  `IPV6_V6ONLY` is never set on the host socket; no linger, so the next bind gets a new host
  port). Connects and sends to the port pick the host port by destination: an IPv6 address goes
  to the IPv6-only binding first, an IPv4 address never does. bind(0) and implicit binds use host
  ephemeral ports directly (`ip_local_port_range` is stored only), which can equal a guest port
  bound explicitly in the same namespace.
- TCP listens and bound UDP sockets register `Listener`s with the guest address (or wildcard).
  Matching: same port and protocol; AF_INET wildcards accept IPv4, AF_INET6 wildcards IPv6 and —
  unless `v6only` — IPv4; specific addresses only themselves.
- A TCP connect to a new namespace with no matching listener is `ECONNREFUSED` before the host is
  asked; a UDP sendto there with no match is dropped silently (reporting success). Targets in the
  initial namespace are not checked.
- Connected UDP sockets that are host-wide emulate the connection with `vconn` (foreign sources
  are discarded on receive); address-less sends skip the listener check.
- Accepted sockets inherit the port map, `vaddr` and namespace; their peer is translated as above;
  getsockname reports the wildcard until a connect, then the facing address.

### Virtual devices

- Kinds: bridge, veth, dummy (others `EOPNOTSUPP`); bridges cannot move between namespaces.
  Changes need `CAP_NET_ADMIN` over the namespace owner (and over a target namespace named by
  `IFLA_NET_NS_PID`/`FD`; `IFLA_TARGET_NETNSID` `EOPNOTSUPP`).
- Create: validate kind, MAC (6 bytes, unicast) and MTU range; default names `bridge%d`,
  `veth%d`, `dummy%d` (lowest free); long names `ERANGE`/`EINVAL`; duplicates `EEXIST`; a taken
  requested index `EBUSY`; indexes above the host's largest in the initial namespace, from 2 in new
  ones; random locally administered MAC, MTU 1500, txqlen 1000; veth peers registered first
  (possibly in another namespace) and paired; then flags and master (failure deletes the device).
- Change (`do_setlink` order, earlier changes kept on a later error): namespace move (down,
  detached, addresses and routes cleared, renamed or `EEXIST` on a name clash), MAC, MTU (below
  1280 tears IPv6 down), group, rename (`EBUSY` while up), alias, flags, master (`ELOOP` for a
  bridge under a bridge, `EOPNOTSUPP` for non-bridge masters), carrier (dummy only), txqlen.
  Host interfaces in the initial namespace accept only no-op requests; lo in a new namespace
  supports up/down and MTU; deleting lo or host interfaces `EOPNOTSUPP`.
- Carrier and operstate: veth when both ends are up; bridges when a port forwards (after the first
  computation); dummy always. IPv6 setup on carrier: multicast and link-local routes, an EUI-64
  link-local address (unless `addr_gen_mode` 1), no DAD.
- IPv4 addresses: same-subnet extras become secondary; duplicates `EEXIST` unless replacing; local
  route always, subnet and broadcast routes while up; deleting a primary deletes its secondaries.
  IPv6 addresses: `EACCES` with IPv6 disabled; removed on down unless `keep_addr_on_down`.
- Routes: unicast only, table ≤ 255, next hop an up virtual device (host interfaces and lo
  `EOPNOTSUPP`), gateways must be on-link (`ENETUNREACH`) unless `RTNH_F_ONLINK`, which needs a
  device that is up (`ENODEV`, `ENETDOWN`) and, for IPv4, a scope below link and a gateway that is
  not local, broadcast or multicast (`EINVAL`), and shows in dumps; flags as `fib_table_insert`; deleting a
  missing route `EOPNOTSUPP` in the initial namespace, `ESRCH` elsewhere; dumps merge with the host
  or lo routes.
- Sysctls: 33 IPv4 and 58 IPv6 settings per device plus `all`/`default`; effective are
  `disable_ipv6`, `addr_gen_mode` (only "none"), `keep_addr_on_down` and IPv4 `default`
  propagation; IPv6 `mtu` and `stable_secret` are checked and stored only; `forwarding`/
  `ip_forward` are stored only (traffic through a gateway always flows as if routing and NAT were
  on).
- ioctls: get/set flags, MTU, name, hardware address, txqlen, and the address getters; address
  setters `EPERM`. Statistics of virtual devices are zero.

### nftables integration

- At connect and at every sendto with an address (connected UDP: once at connect), the socket
  layer builds a fake packet (addresses, protocol, ports, uid/gid, mark, interfaces, ct state) and
  runs one hook's base chains of the matching families in priority order:
  1. OUTPUT nat of the sender (TCP/UDP/MPTCP, IPv4 and IPv6);
  2. PREROUTING nat of one entered namespace (the target namespace, or the initial namespace for a
     container going out through it), IPv4 only, once;
  3. filter chains along the path (IPv4 only): the sender's OUTPUT, then per step PREROUTING
     filter chains (evaluated after DNAT) and the step's own hook — INPUT on lo, the initial
     namespace's INPUT or FORWARD, the target's INPUT; the mark is cleared outside the sender.
  POSTROUTING (nat and filter), INPUT-hook nat and arp/bridge/netdev tables are never evaluated.
  (Inbound forwarding's probe can evaluate IPv6 PREROUTING nat.)
- Effects: a nat drop or reject is `EPERM` at connect/sendto (also for drops in a remote
  namespace's PREROUTING); a TCP filter reject returns its errno at once (RST →
  `ECONNREFUSED`, ICMP codes mapped), a TCP filter drop connects to a blackhole listener so it
  times out like Linux's retransmissions; a UDP drop or reject is `EPERM` on OUTPUT and silent
  elsewhere; DNAT rewrites the destination (and may move it to another namespace); `meta mark set`
  updates the mark.
- `tracks()` (any conntrack-using table) decides whether `SO_ORIGINAL_DST` answers.
- Expression and control-plane details are in `kernel/nft`; initial-namespace commits
  trigger the socket layer's forwarding refresh.

### Forwarding from outside

- Candidates: initial-namespace TCP listeners and UDP sockets bound to a nonzero port (not raw)
  when the initial namespace has PREROUTING nat. A synthetic packet from 192.0.2.1:40000 to the
  socket's port (wildcards become the first non-loopback IPv4 interface address) runs PREROUTING
  nat; a DNAT result is translated to the target's host address and gateway source, filtered
  through the initial FORWARD and the target's PREROUTING/INPUT chains (a drop or reject means no
  forwarding), and low ports are replaced by their host ports. Re-evaluated on listen, UDP bind,
  initial-namespace nft commits and explicit-port binds in namespaces (when PREROUTING nat
  exists) — not on interface address changes or container-namespace filter changes. An AF_INET6
  wildcard listener is probed as IPv6, so IPv4 DNAT rules never match it.
- One host thread (`forward`, no BKL) owns dups of the selected host sockets: it accepts up to 64
  connections per round, opens a nonblocking connection to the target, and copies both ways with
  64 KiB buffers, propagating half-close with `SHUT_WR`; errors close both ends, and so does a side
  that reports an error or hangs up after its end of file before it was shut for writing (a reset
  half-closed side). A listener that polls readable while its first accept fails with anything but
  `EAGAIN` (out of fds, shut down) is left out of the poll for 1 s; a failing poll waits 100 ms. UDP uses one relay
  socket per client address (120 s idle), replies sent from the held port.
- Real client address: for IPv4, non-loopback TCP clients and IPv4 targets the relay binds a
  127.254.x.y address with the client's port and records it, so accept, getpeername and
  `/proc/net` in the container report the real client; loopback, IPv6 and UDP clients see the
  gateway. The record is removed when the forwarded connection closes.
- Closing the socket or a rule change drops the handle; established forwarded TCP connections stay,
  UDP relays of that port are closed at once. The guest's own accept races with the forwarder on
  the same host listener. There is no cap on connections or relays and no connect timeout.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/net` (bind, listen, connect, accept, send, recv, getsockname/getpeername, forward targets,
  `SO_ORIGINAL_DST`, `SIOCADDRT`/`SIOCDELRT` through `netdev::request`); `kernel/netlink`
  (device configuration); `kernel/ns` (clone/unshare/setns
  create and enter namespaces); `kernel/procfs` (`/proc/net`, `/proc/sys/net`, `/sys/class/net`);
  the machine (the forwarder per kernel).

### Outbound

- `tuxport_hal::net` (host sockets, host interfaces, poll, dup, events); `kernel/net`'s portmap
  and routes (`route::merge`, `attrs`); `kernel/netlink`'s encoding (`message`, `attr`,
  `configure_host_link`); credentials (`ns_capable`); the kernel
  log; `std::thread` for the forwarder.
- `kernel/nft` owns each namespace's ruleset; this module builds packets and selects hooks,
  applies verdicts and translates DNAT destinations. Forwarding probes use the same evaluator.

## Invariants and gotchas

- Isolation exists only on the guest side: host loopback sockets of every namespace are
  reachable by the initial namespace (which passes 127.x through) and by other apps on the device.
- Namespace translation depends on who is looking; every address leaving the kernel layer must be
  translated for the viewing socket's namespace.
- Wildcard, 127.0.0.1 and the namespace's virtual address share host 127.a.b.1; only the listener
  registry tells them apart.
- Device snapshots (`View`) hold their veth peer's namespace alive; devices refer to each other only
  weakly.
- The forwarder never takes the BKL and touches only host fds and its own shared state.

## Confirmed bugs / technical debt

- **No TUN/TAP device:** `/dev/net/tun` does not exist (there is no network stack to attach a tap
  device to), so pasta and slirp4netns cannot start: rootless podman's default network, its port
  publishing and `--network slirp4netns` fail, and rootless containers work only with
  `--network host` (the Android app makes that rootless podman's default, `android/setup`).
- **Namespace isolation is guest-side only:** the initial namespace passes 127.x through, so its
  guests (and other apps on the device) reach a container's host sockets directly, including
  services bound only to the container's 127.0.0.1.
- **Same-port binds are judged by host address, not guest address:** 127.0.0.1:P and the virtual
  address:P conflict (both are 127.a.b.1 on the host) where Linux allows them, while wildcard:P
  plus 127.0.0.2:P succeed, and so do an IPv6-only `[::]:P` plus a dual-stack `[::]:P` (separate
  host ports), where Linux refuses the second.
- **Ephemeral ports can alias explicit ports:** bind(0) and implicit binds take host ephemeral
  ports, which may equal a guest port bound explicitly in the same namespace; connects to that
  number then go to the explicit socket, and a later explicit bind to an ephemeral number succeeds.
- **Connected UDP escapes namespace checks:** address-less sends skip the listener check, and a
  host-wide UDP socket connecting to a namespace target returns before the filter verdict is
  recorded, so a drop rule never applies to its later sends.
- **nat-chain drops become `EPERM`:** a drop or reject in a nat chain (even a remote namespace's
  PREROUTING) fails connect/sendto with `EPERM`; Linux times TCP out and never filters UDP connect.
- **Dual-stack listeners are not forwarded:** an AF_INET6 wildcard listener in the initial
  namespace is probed as IPv6, so IPv4 DNAT rules never match it and outside IPv4 clients reach the
  listener itself (older docker-proxy setups lose the real client address).
- **Forwarder limits:** there are no connection or relay limits and no connect timeout; UDP relays
  are cut when the rule changes.
- **Forwarded peers revert to 127.254.x.y:** the real-client record is removed when the forwarder
  closes the connection, so a container still holding the socket (CLOSE_WAIT) sees the raw
  address in getpeername.
- **Smaller deviations:** only 127.0.0.0/24 works as loopback in a namespace (Linux: 127/8); IPv6 on
  virtual devices is unusable and on-link IPv6 is handed to the host; `hostmap` entries are never
  freed and collide once 65534 are used; PREROUTING filter chains see the post-DNAT destination at
  every priority. `kernel/nft` records the unsupported POSTROUTING, SNAT and MASQUERADE effects.
- **Route requests insist on exact address sizes:** `RTA_DST`, `RTA_GATEWAY` and `RTA_PREFSRC` in
  `RTM_NEWROUTE`/`RTM_DELROUTE` must be exactly 4 or 16 bytes, otherwise `EINVAL`; Linux 6.1
  answers `ERANGE` for a short IPv4 attribute or IPv6 gateway, uses the first 4 bytes of a longer
  IPv4 attribute, and needs only the prefix's bytes of an IPv6 destination. iproute2 sends exact
  sizes, so only oddly encoded requests are affected.
- **Gateways resolve to the last matching subnet:** a route with a gateway but no device takes the
  device of the last longest-prefix route covering the gateway, where Linux takes the first
  (lowest index); with two devices on one subnet the route lands on the other device, and a later
  duplicate naming the first device is accepted instead of `EEXIST`.
- **Next-hop checks miss Linux's special cases:** an all-zero `RTA_GATEWAY` counts as a gateway
  instead of none; IPv6 link-local gateways are looked up in the
  table (failing while the device has no carrier) where Linux accepts them with a device; and an
  unreachable IPv6 gateway is `ENETUNREACH` where Linux says `EHOSTUNREACH`.

## Open questions

- Is reaching a namespace's 127.a.b.x sockets directly from the initial namespace acceptable?
- Is the first block 127.0.1.0/24 (Debian's `127.0.1.1` hostname convention) deliberate?
- Should forwarded connections be torn down when the DNAT rule disappears?
- Should `hostmap` entries be freed when virtual addresses are deleted?

## To verify

- How docker (with and without docker-proxy) and podman listeners interact with the dual-stack
  forwarding gap on a real device.
