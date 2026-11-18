# Netlink sockets (kernel/netlink) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/netlink"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions on netlink sockets made before kernel/netlink was split from kernel/net keep their `kernel/net` tags and live in `.agents/decisions/kernel/net.md`: D3, D24, D26 and the netlink halves of D19 and D23.

## Active

### D1: kernel/netlink is split out of kernel/net
- **What**: kernel/netlink owns `net.rs`'s sections from netlink through `NETLINK_SOCK_DIAG` and, in `syscall/net.rs`, the `netlink_*` functions, the netlink section (`netlink_send`, `rtnl_watched`, `CHANGING_IOCTLS`, `rtnl_notify`, `KernelHost`) and the `Socket::Netlink` arms: binding, groups and options, requests and reply queues, rtnetlink queries, changes and notifications, `nlctrl`, `NETLINK_SOCK_DIAG`, the `NETLINK_NETFILTER` entry point and `NETLINK_KOBJECT_UEVENT`.
- **Why**: Netlink is a subsystem of its own in Linux (af_netlink, rtnetlink, genetlink, sock_diag) with its own protocols, notifications and the largest group of kernel/net's bugs, and the fully refreshed kernel/net map neared one Read.
- **Tradeoffs**: Route and rule messages and `ip route get` come from kernel/net's `route.rs`, the boundary inside `net.rs` and `syscall/net.rs` is recorded only in the registry, and the map carries the facts verified in kernel/net's full mapping of 2026-11-11 rather than a mapping of its own; nftables control-plane handling belongs to kernel/nft while netlink transport remains here.
- **Watch out**: Code added to the netlink sections or arms may be described in the wrong map, the netlink unit tests stay in `net.rs`'s test module, and bugs open in the map include non-resumable dumps, requests not validated as Linux validates them, notifications only for rtnetlink link, address and route changes, and ignored destination addresses.
- **Source**: 0c1b416; 60790c14; 39e55b4b
