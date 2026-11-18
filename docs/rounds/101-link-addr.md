# M9 之前：接口和地址的 netlink 消息（2026-10-29）

[路由表](98-routes.md)那一轮对照时发现、记为推迟项的一条：`RTM_NEWADDR` 少了几个属性，`ip addr` 不显示 `brd`。再和原生对照，缺的不止这些：

- busybox 的 `ip addr`、`ip link` 少了 `brd`、`valid_lft forever preferred_lft forever` 那一行，以及链路那一行的 `qdisc noqueue state UP`。`ip link show eth0 | grep 'state UP'` 是脚本里判断网卡起没起来的常见写法，原来会判断错。
- iproute2 的 `ip link show eth0`、`ip addr show dev eth0` 发的是不带 `NLM_F_DUMP` 的单条 `RTM_GETLINK`，原来是 EOPNOTSUPP：Debian、Ubuntu 上这两条命令报 `Operation not supported`、退出码 1。`ip -br addr` 也没有状态那一列。

## 做法

改在 `core/kernel/src/net.rs`：

- 链路消息按 Linux 的 `rtnl_fill_ifinfo` 的顺序加上 `IFLA_TXQLEN`、`IFLA_OPERSTATE`、`IFLA_LINKMODE`、`IFLA_GROUP`、`IFLA_PROMISCUITY`、收发队列数、`IFLA_CARRIER`、`IFLA_QDISC`、`IFLA_PROTO_DOWN`；`IFLA_STATS64` 挪到 `IFLA_STATS` 前面，与 Linux 相同。
  - 状态与 `/sys/class/net/<接口>/operstate` 用同一个函数：回环接口是 unknown（Linux 的回环驱动不设置它），启用并且连着的是 up，别的是 down。
  - 发送队列长度取自宿主的 `/sys/class/net/<接口>/tx_queue_len`，取不到时是 Linux 的默认值 1000；sysfs 里也新增了这个文件。
  - guest 的数据包交给宿主 socket，接口上没有自己的排队规则，qdisc 都是 noqueue，收发队列各一个。
- 地址消息按 `inet_fill_ifaddr`、`inet6_fill_ifaddr` 的顺序：
  - IPv4 加上 `IFA_BROADCAST`（接口支持广播、前缀短于 31 位时是网段的最后一个地址，与本地表里的广播路由一致）、`IFA_FLAGS`、`IFA_CACHEINFO`；
  - IPv6 加上 `IFA_CACHEINFO`、`IFA_FLAGS`，以及回环、链路本地地址的 `IFA_PROTO`（内核加的）。
  - 地址都当作永久的（`IFA_F_PERMANENT`，显示 forever），创建和更新的时刻不知道，是 0。
- 单条 `RTM_GETLINK`：按 `ifi_index` 或者 `IFLA_IFNAME` 找（`rtnl_getlink`）；查不到是 ENODEV，两个都没给是 EINVAL；要确认时另回一个确认。
- 接口、地址的转储与 Linux 一样，NLMSG_DONE 另是一个数据报（原来跟在同一个里，[M4](06-m4.md) 起）。`AF_UNSPEC` 的地址转储先是所有 IPv4、再是所有 IPv6，各是一个数据报（原来按接口交错）；musl 的 getifaddrs 发的就是 `AF_UNSPEC`。

## 测试

- cargo test：netlink 的单元测试检查转储的数据报、新加的属性和单条查询的几种结果。
- 差分测试 compat 组新增一个 C 程序，比对地址、链路的转储和单条 `RTM_GETLINK` 的几种结果，还比对 busybox 的 `ip addr`、`ip link` 和 lo 的 `tx_queue_len`、`operstate`，都与原生相同。不确定的和 docker 特有的不比：`ifa_cacheinfo` 里的两个时刻，收发队列数（docker 的 veth 按 CPU 数分配），MAC 地址、veth 的 `@ifN` 和 `M-DOWN`。
- 在 Debian 的仓库里用 iproute2 6.15 试了 `ip link show eth0`、`ip addr show dev eth0`、`ip -br addr`、`ip -d link`，输出正常。
- **推送之后**：CI 的差分测试 compat 组失败，问题都在测试环境，内核层没有问题。
  - Linux 宿主上 `tuxport-run` 直接跑在宿主上、原生一侧在 docker 容器里，路由、接口、地址类的用例两边看到的网络不一样（Mac 上两边都在容器里，所以本地一直通过）。`scripts/difftest` 改为在 Linux 宿主上也把 `tuxport-run` 放进容器（默认 `debian:trixie-slim`，`TUXPORT_IMAGE` 可以换）里，以调用者的 uid、gid 运行（以 root 运行时 CI 上新建的仓库属于 root，宿主上接着 `apk add` 时打不开）。
  - runner 的内核比 colima 的 6.8 新：有的转储把 NLMSG_DONE 并进了最后一个数据报（6.10 起），securebits 的第 8 位有了定义（6.14 的 `SECBIT_EXEC_RESTRICT_FILE`），链路消息里 `IFLA_QDISC` 的位置变了。用例改为不看转储怎么分成数据报，"不认识的位"换成第 20 位，链路消息的属性按类型排好再比。
  - [第 95 轮](95-tracer-exit-drops-signal.md)的用例读被跟踪的进程的状态，它在 `usleep` 的循环里，读到 R 还是 S 看时机，改为只看停没停住。

## 推迟项

- 链路消息没有 Linux 的另外二十多个属性：`IFLA_MIN_MTU`、`IFLA_MAX_MTU`、GSO、GRO 的参数，`IFLA_MAP`、`IFLA_XDP`，`IFLA_AF_SPEC`（inet、inet6 的设备参数和统计）等。`ip -d link` 少了 minmtu、maxmtu 这些。→ bridge、veth、dummy 的链路消息带 `IFLA_MIN_MTU`、`IFLA_MAX_MTU` 了（M9 之前：有状态的网卡）；宿主接口的仍没有，别的属性也仍没有
- qdisc、收发队列数是固定的：宿主上实际的 qdisc（手机的 wlan0 多半是 mq 或者 fq_codel）拿不到。
- 地址都是永久的、创建时刻是 0：分不出 DHCP、SLAAC 得到的有期限的地址（[if_inet6](85-inet6-opath-exit.md) 那一轮的同一条推迟项）；广播地址是按前缀算的，宿主设了别的广播地址时不对。
- 单条的 `RTM_GETADDR`（IPv6 有）没有做，是 EOPNOTSUPP。
