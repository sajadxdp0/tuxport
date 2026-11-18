# M9 之前：`/proc/net/snmp` 和输入结束时的 EOF（2026-10-29）

审推迟项时挑出来的两个便宜的兼容性缺口：

- **`/proc/net/snmp`、`netstat`、`arp`**（[网络信息的缺口](78-net-info-gaps.md)、[路由表](98-routes.md)的推迟项）原来没有。iproute2 的 `ss -s` 读 `snmp` 里的 CurrEstab，打印 `ss: get_snmpstat: No such file or directory`；net-tools 的 `netstat -s`、iproute2 的 `nstat`、sysstat 的 `sar -n TCP,UDP` 也读这两个文件。
- **`tuxport-run --pty` 的 stdin 结束**（[M3b](05-m3b.md) 的推迟项）：桥接线程不再读，程序却收不到 EOF。`printf 'make\n' | tuxport-run --pty sh -i` 跑完命令之后 shell 一直等输入，不退出。

## 做法

- `/proc/net/snmp`（`core/kernel/src/net.rs`）：各组的字段和顺序照 Linux 6.1 的 `snmp_seq_show`（6.2 起 Icmp 在 OutErrors 后面插了两个限速的计数，6.3 起 Ip 末尾多了 OutTransmits，这里都没有）。计数取自 `core/kernel/src/traffic.rs` 新增的协议计数，与接口的计数在同一个地方数：
  - IP：IPv4 收发的包数（与接口的计数一样按载荷估计）；Forwarding 是 2（不转发），DefaultTTL 是 64。
  - ICMP：ping socket 的回显请求和回答；IcmpMsg 与 Linux 一样只列出不为 0 的类型。
  - TCP：RTO 的几项是 Linux 的初值。ActiveOpens 在宿主发出 SYN 时数（connect 成功、正在连接、马上被拒绝）；AttemptFails 在得知这次连接失败时数（阻塞的 connect 的结果、非阻塞时再 connect 一次的结果、SO_ERROR 读到 ECONNREFUSED），socket 上记着发起的连接还没有数过失败，不会数两次；PassiveOpens 数 accept 到的连接；CurrEstab 是现在 ESTABLISHED 的 guest TCP socket 数（与 Linux 一样不含 CLOSE_WAIT）；InSegs、OutSegs 是估计的包数。
  - UDP：IPv4 收发的数据报，IPv6 socket 上 IPv4 映射的地址也算（Linux 的 `udpv6_sendmsg` 把它交给 IPv4），偷看的不算。
  - 错误、重传、复位、分片这些内核层看不到，是 0。
- `/proc/net/netstat`：TcpExt、IpExt、MPTcpExt 三组，字段照 6.1。只有 IpExt 的 InOctets、OutOctets（IPv4 的载荷字节数）和 InNoECTPkts（收到的包都当作不带 ECN 标记）不是 0。
- `/proc/net/arp`：只有表头，与 `ip neigh` 的空表一致。
- `InetSocket` 记下协议号：建 socket 时 guest 给的，给的是 0 时按类型取 TCP、UDP；accept 到的跟着监听的。
- 桥接线程（`core/kernel/src/terminal.rs`）：stdin 读到结束（或者出错）之后不再读，等程序把已经送进去的输入读完（规范模式下没有完整的行，非规范模式下队列空了），往控制台送一个 termios 里的 VEOF（被禁用时不送），只送一次。与 util-linux 2.35 起的 `script` 相同：它在 stdin 结束时等从设备读空了再写一个 ^D。不当作挂断：发 SIGHUP 会把还在跑的命令杀掉，`printf 'sleep 3; echo done\n'` 就看不到 done。

## 测试

- cargo test：`tests/guest/traffic.c` 加上 snmp、netstat 的格式和常数项，连接、被拒绝的连接、数据报、ping 前后各项的变化，IP 的计数与 lo 的一致。rootfs 测试新增：`--pty` 下经由管道给 `sh -i`、`cat` 输入，输入结束之后它们自己退出。
- 差分测试 compat 组：两个文件的格式和常数项，回环上的连接、被拒绝的连接、数据报前后 TCP、UDP 几项的变化，与原生相同。字段列表随内核版本变化，不比；IP 的包数、TCP 的段 Linux 还算握手和 ACK，不比。
- Debian 的仓库里 `ss -s`、`nstat -a` 都正常。
- 变异检验最后一次 `cargo test --release` 留下的是变异过的 release 二进制，之后只跑了 clippy，第一次差分测试用的就是它：还原之后要重编。

## 推迟项

- IP 的包数、TCP 的段与接口的计数一样是估计的，握手、ACK、重传都不算；Ip 的 InReceives 总是等于 InDelivers。
- 非阻塞的连接因为超时、不可达而失败，guest 只从 poll 和 SO_ERROR 得知时，AttemptFails 不数（SO_ERROR 的 ETIMEDOUT 也可能来自已经建立的连接，分不出来）；EstabResets、OutRsts、RetransSegs、InErrs 都是 0。
- PassiveOpens 在 accept 时数，Linux 在握手完成时数：还在监听队列里、没有被 accept 的连接看不到。
- ICMP 只有 ping socket 自己的回显：ping 回环地址时 Linux 还数本机收到请求、发出回答（InEchos、OutEchoReps），这里答复的是宿主，不算。IPv6 的 ICMP、UDP 在 `/proc/net/snmp6` 里，没有这个文件。
- DNS 存根的查询不经过宿主 socket，不算 UDP 的数据报（与接口的计数一样）。
- `/proc/net` 仍没有 `snmp6`、`protocols`、`dev_mcast`、`igmp` 等（原生的容器里 57 个文件，这里 18 个）。
- VEOF 只送一次：`printf 'cat\necho after\n' | tuxport-run --pty sh -i` 卡住。ash 在非规范模式下一次读走了两行，^D 随即送出；ash 起 cat 时切回规范模式，^D 只是一个还没读的普通字符，cat 一直等一整行（Linux 上用 `script` 也这样）。
