# M9 之前：网络信息的缺口（只补文档，2026-10-28）

审推迟项时列出的网络信息缺口，以前只有 [M4](06-m4.md) 提过一句 `RTM_GETROUTE` 返回空的转储。这一轮不改代码，把现状、影响和可能的做法记下来，设计文档 §10 的表里也补上。

## 现状

在普查仓库里与原生的 docker 容器对照。

| | 原生 | tuxport |
|---|---|---|
| `ip route`、`ip -6 route` | 默认路由和网段的路由 | 什么都没有：`RTM_GETROUTE` 的转储是空的 |
| busybox 的 `route -n`（读 `/proc/net/route`） | 路由表 | `can't open '/proc/net/route'` |
| `/proc/net/dev` | 各个接口收发的字节数、包数 | 列出宿主的接口，计数全是 0 |
| `/proc/net/if_inet6`、`snmp`、`netstat`、`sockstat`、`arp` | 有 | 没有 |
| `ip neigh` | 邻居表 | `RTNETLINK answers: Not supported`：`RTM_GETNEIGH` 和别的没有模拟的请求回答 EOPNOTSUPP |

接口和地址是全的：`ip addr`、`ifconfig`、musl 的 `getifaddrs`、Node.js 的 `os.networkInterfaces()` 都取自宿主的 `getifaddrs`（§10）。`/proc/net` 下的 tcp、udp、unix、icmp、raw 由各进程的 fd 表合成，也是全的。

## 影响

看路由的命令（`ip route`、`route -n`、`netstat -r`）没有输出或者报错；按 `/proc/net/route` 或 `RTM_GETROUTE` 找默认路由所在的接口和网关的程序拿不到结果；看流量的工具（`ip -s link`、`ifconfig` 的 RX/TX、nload、bmon、htop 和 btop 的网络面板）一直是 0。两轮普查的负载都没有因此失败。

## 为什么没有

Android 10 起 App 读不了宿主的 `/proc/net`；targetSdk ≥ 30 不能 bind `NETLINK_ROUTE`，`RTM_GETLINK` 也被拦截（§2 的硬约束），宿主的路由表和流量计数都不能直接转给 guest。

## 可以怎么做（都没做）

- 路由：App 从 `ConnectivityManager` 取默认网络的 `LinkProperties.getRoutes()`，经由 Machine 接口交给内核层，用它合成 `/proc/net/route`、`ipv6_route` 和 `RTM_GETROUTE` 的回答，网络变化时（`NetworkCallback`）更新。Linux 开发宿主上可以直接读宿主的 `/proc/net/route`。 → 已按这个做法补上（M9 之前：路由表）
- 流量：guest 的 TCP、UDP 都经过内核层，可以自己数收发的字节；但选哪个接口是宿主决定的，分不出接口，只能都算在默认接口上。`TrafficStats` 只有整台设备和每个 uid 的总量（包括 App 自己的流量），也分不出接口。→ 已补上（M9 之前：网络接口的流量计数）：内核层自己数，接口按 socket 的本端地址（宿主选路的结果）认，分得出来
- `if_inet6`（地址已经有了，格式化一下就行）、`snmp`、`sockstat` 等按需要再补。→ `if_inet6` 已补上（M9 之前：if_inet6、O_PATH 的 ioctl 和中途停住之后的出口）；`sockstat` 已补上（M9 之前：路由表）；`snmp` 已补上（M9 之前：snmp 和输入结束时的 EOF）

## 推迟项

- 路由表：`RTM_GETROUTE`、`/proc/net/route`、`/proc/net/ipv6_route`。→ 已补上（M9 之前：路由表）：主表取自宿主（Android 上由 App 从 `LinkProperties` 交来），本地表按接口的地址合成，还有 `ip route get`、`ip rule`
- `/proc/net/dev` 的流量计数。→ 已补上（M9 之前：网络接口的流量计数），另外新增 `/sys/class/net/<接口>/statistics/`（btop 读它）和 netlink 的 `IFLA_STATS`、`IFLA_STATS64`；只数载荷，包数是估计的
- `/proc/net/if_inet6`、`snmp`、`netstat`、`sockstat`、`arp`，`RTM_GETNEIGH`。→ `if_inet6` 已补上（M9 之前：if_inet6、O_PATH 的 ioctl 和中途停住之后的出口）；`sockstat`（和 `sockstat6`）已补上，`RTM_GETNEIGH` 回答空的邻居表（M9 之前：路由表）；`snmp`、`netstat`、`arp` 已补上（M9 之前：snmp 和输入结束时的 EOF）
