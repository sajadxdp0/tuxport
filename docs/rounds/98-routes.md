# M9 之前：路由表（2026-10-29）

[网络信息的缺口](78-net-info-gaps.md)那一轮的推迟项。原来没有路由表，看路由、按路由取地址的命令和程序都拿不到结果：

- `ip route` 什么也不列，busybox 的 `route -n`、`netstat -r` 报 `can't open '/proc/net/route'`；
- 脚本里取本机主地址的常用写法 `ip route get 1.1.1.1 | ...` 得到 `RTNETLINK answers: Operation not supported`：不带 `NLM_F_DUMP` 的 `RTM_GETROUTE` 落到了回答 EOPNOTSUPP 的分支；
- 按 `/proc/net/route` 找默认接口和网关的程序找不到。

这一轮顺手带上了同一篇里两个小的：`ip neigh`（原来 `RTNETLINK answers: Not supported`）和 `/proc/net/sockstat`。

## 做法

- **两张表**（`core/kernel/src/route.rs`）：按 Linux 没有自定义规则时的样子拼出主表和本地表。
  - 主表是宿主的路由，来源有三个：
    - Android 上 App 读不了宿主的路由表，`TerminalService` 注册默认网络的 `NetworkCallback`，把 `LinkProperties.getRoutes()` 经由新加的 `Machine::set_routes`（JNI 的 `Native.setRoutes`）交给内核层，网络变化时重新交。清单里加了 `ACCESS_NETWORK_STATE`。Android 13 之前 `RouteInfo.getType()` 不公开，路由都当作单播。
    - 别的宿主上读宿主的 `/proc/net/route`、`/proc/net/ipv6_route`（`tuxport_hal::net::routes`），只要主表的单播路由。
    - 都没有时（比如 App 还没交来）按接口的地址合成：每个地址所在的网段一条，再加一条走默认接口（[流量计数](90-interface-counters.md)那一轮认出的接口）、没有网关的默认路由。
  - 没有网关、正好是接口上某个地址所在网段的路由与 Linux 一样算内核加的（proto kernel、scope link，IPv4 带首选源地址），别的算 boot（`ip route` 不显示）。IPv6 路由的度量 App 交来的没有，按 Linux 的默认值：默认路由 1024，别的 256。
  - 本地表按接口的地址合成，与 Linux 的 `fib_add_ifaddr`、addrconf 加的相同：IPv4 每个地址一条 local、一条网段的广播（回环接口上网段本身也是 local），IPv6 每个地址一条 local，每个有 IPv6 地址的非回环接口一条 `ff00::/8` 的多播。
  - 顺序与 Linux 转储的相同：IPv4 按目标地址从小到大、同一地址前缀长的在前（fib trie 的叶子），IPv6 按 fib6 树的后序；主表在本地表之前。
- **netlink**（`core/kernel/src/net.rs`）：
  - `RTM_GETROUTE` 的转储：与 Linux 一样，每个地址族的路由各是一个数据报，`NLMSG_DONE` 另是一个。IPv4 带 `RTA_TABLE`、`RTA_DST`、度量不为 0 时的 `RTA_PRIORITY`、`RTA_PREFSRC`、`RTA_GATEWAY`、`RTA_OIF`，IPv6 还总有 `RTA_PRIORITY`、`RTA_CACHEINFO`、`RTA_PREF`（`fib_dump_info`、`rt6_fill_node`）。
  - `ip route get`：源地址由宿主选（对目标 connect 一个 UDP socket，`tuxport_hal::net::source_for`），出口接口是那个地址所在的接口，网关按主表里的最长前缀匹配。目标是本机地址、回环网段时类型是 local、走 lo；网段的广播、组播各按 Linux 的类型和缓存标志（224.0.0.1 本机总在组里，带 `RTCF_LOCAL`）。IPv4 的回答带 `RTM_F_CLONED`、`RTA_UID`；本地表这时并在主表里，表号是主表。`RTM_F_FIB_MATCH` 回答匹配到的那条路由本身。宿主选得出源地址、主表里却没有匹配的路由时（Android 的 464xlat：`getRoutes()` 不含叠加链路的路由），当作经由源地址所在接口、没有网关的路由回答，不报 ENETUNREACH。
  - `RTM_GETRULE` 的转储：Linux 默认的规则（IPv4 三条、IPv6 两条，`AF_UNSPEC` 时还有两条多播路由的），一个数据报，`NLMSG_DONE` 另是一个。
  - `RTM_GETNEIGH` 的转储只有 `NLMSG_DONE`，单条查询是 ENOENT：Android 上 App 看不到宿主的 ARP、NDP 表。
  - 错误消息改为附上整条请求、确认只附上消息头并带 `NLM_F_CAPPED`，与 Linux 的 `netlink_ack` 相同（[NETLINK_GENERIC](91-generic-netlink.md) 那一轮的推迟项，genetlink 的回答改用同一个函数）。
- **procfs**（`core/kernel/src/pseudo.rs`）：`/proc/<pid>/net` 下新增四个文件（11 → 15 个）。
  - `route`：主表里 IPv4 的单播路由，格式与 `fib_route_seq_show` 相同，每行补空格到 127 个字符。
  - `ipv6_route`：IPv6 的两张表，每张表最后是树根上的空路由（没有 `::/0` 时），引用数都写 1。
  - `sockstat`、`sockstat6`：guest 的 socket 数。TCP 的 inuse 数监听着、连接着（不是 CLOSE）的，alloc 数所有 TCP socket，UDP 数绑定了端口的，raw 数所有的；宿主内核里已经关掉、还在 TIME_WAIT 或者孤儿状态的连接看不到，内存页数也不知道，都是 0。
- **模拟器上**（Android 14，4K 页）：交来 `LinkProperties` 之前是合成的 `default dev eth0 scope link`，交来之后是 `default via 10.0.2.2 dev eth0`，`ip route get 1.1.1.1`、`route -n` 都经由网关。模拟器的宿主内核里 eth0 是 10.0.2.15/8，DHCP 交给 `LinkProperties` 的却是 /24，所以本地表的广播是 10.255.255.255（与宿主内核一致），主表那条 10.0.2.0/24 不认作内核加的。

## 结果

只在查路由、读这几个文件时计算，收发数据的路径上没有改动。

## 测试

- cargo test：`route` 的单元测试（两张表的内容和顺序、两个 `/proc` 文件的格式、`ip route get` 的各种目标、规则），netlink 的转储和错误，宿主路由表的解析；端到端新增 `routing_tables_are_visible`。
- 差分测试 compat 组新增一个 C 程序，逐字节比对路由和规则的转储、各种目标的 `ip route get`，还比对 busybox 的 `ip route`、`ip rule`、`route -n`、`netstat -rn` 和两个 `/proc` 文件，以及开 socket 前后 sockstat 的变化。不确定的不比：邻居表只看转储以 `NLMSG_DONE` 结束（Linux 上前面的用例在 lo 上留下了条目），`rta_cacheinfo` 只比错误码，`ipv6_route` 不比引用数，TCP 的 alloc 是整个系统的。对照时发现两处不同：224.0.0.1 少了 `RTCF_LOCAL`（已改），`ip addr` 少了 `brd`（见推迟项）。
- Android：`MachineTest` 新增 `routesComeFromLinkProperties`，在 Android 14（4K 页）和 Android 17（16K 页）的模拟器上通过。

## 推迟项

- `RTM_NEWADDR` 少了 `IFA_BROADCAST`、`IFA_FLAGS`、`IFA_CACHEINFO`（[M4](06-m4.md) 起）：`ip addr` 不显示 `brd`，原生的地址消息长 76、88 字节，tuxport 的短一些。→ 已补上（M9 之前：接口和地址的 netlink 消息），地址消息与 Linux 逐字节相同；链路消息也补了 `ip link` 用到的属性，iproute2 的单条 `RTM_GETLINK` 也能用了
- `/proc/net` 仍没有 `snmp`、`netstat`、`arp` 等（原生的容器里 57 个文件，这里 15 个）：iproute2 的 `ss -s` 的汇总要读 `snmp` 里的 `CurrEstab`。→ `snmp`、`netstat`、`arp` 已补上（M9 之前：snmp 和输入结束时的 EOF），现在 18 个；`snmp6` 等仍没有
- 主表的细节：
  - Android 上只有默认网络的路由。VPN 之外别的网络（同时连着的 Wi-Fi 和蜂窝）、464xlat 叠加链路（clat）的路由都没有，`getRoutes()` 不含它们。
  - 不是内核按地址加的路由都算 boot，分不出 dhcp、ra、static。IPv6 的 `RTF_ADDRCONF`、`RTF_EXPIRES` 等标志和 mtu、hoplimit 这些度量都没有。
  - 不可达、throw 等类型的路由不进主表：Linux 宿主上按 `RTF_REJECT` 去掉，Android 13 起按 `RouteInfo.getType()` 去掉，之前的当作单播。
- `ip route get` 的模拟收到数据包（`iif`）是 EOPNOTSUPP；严格检查时请求里的过滤条件（`RTA_TABLE` 等）不看，iproute2 自己会再过滤一遍。
- 改路由（`ip route add` 等）仍是 EOPNOTSUPP：App 改不了宿主的路由。
- sockstat 的 orphan、tw、mem 都是 0，alloc 只数 guest 的 socket；邻居表总是空的；`ipv6_route` 的引用数都是 1。
