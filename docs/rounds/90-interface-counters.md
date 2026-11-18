# M9 之前：网络接口的流量计数（2026-10-29）

[网络信息的缺口](78-net-info-gaps.md)那一轮记下的推迟项之一：`/proc/net/dev` 的计数全是 0，`ifconfig` 的 RX/TX、htop、nload 的网络面板一直是 0。btop 读的是 `/sys/class/net/<接口>/statistics/{rx,tx}_bytes`（`strings` 看过它的二进制），tuxport 原来没有 `/sys/class`；`ip -s link` 和 getifaddrs 的 `ifa_data` 来自 netlink 的 `IFLA_STATS`，原来也没有。

## 做法

- **内核层自己数**（`core/kernel/src/traffic.rs`）：Android 上 App 读不到宿主的计数，宿主的计数里也还有 App 自己和别的 App 的流量。guest 的 TCP、UDP、ping socket 都经由内核层的 `send`、`recv` 收发，在这两处记下实际收发的字节数。和容器里看到的是自己网络命名空间里的流量一样，只有 guest 的。
  - 只数载荷，不含协议头、ACK 和重传；流式 socket 的包数按接口的 MTU 减去 52 字节（IPv4、TCP 头和时间戳选项）估计，数据报一个算一个。偷看（`MSG_PEEK`）的和错误队列（`MSG_ERRQUEUE`）里的不算。
  - 接口按 socket 的本端地址认：那是宿主选路的结果（发往 docker0 网段的连接本端在 docker0 上），第一次收发时取一次（一个宿主的 getsockname），记在 socket 上，bind、connect 时重新认。本端是通配地址（没有 connect 的 UDP、ping）时也记下这一点，之后每一次按这一次的对端认：回环地址算回环接口，别的算默认接口。
  - 地址到接口的表取自宿主的 getifaddrs，用了 5 秒之后重新取；本端地址不在表里（网络刚切换）时也重新取，两次之间至少隔 1 秒。默认接口是发往公网时宿主选的本端地址所在的接口：对 `8.8.8.8`（没有 IPv4 路由时 `2001:4860:4860::8888`）connect 一个 UDP socket，不发出数据报。
  - DNS 存根的查询和回答不经过宿主 socket，不算；AF_UNIX 不是接口上的流量。
- **三处报告同样的计数**：
  - `/proc/net/dev` 的每一行改成 Linux 的 `dev_seq_printf_stats` 的格式（原来每列只隔一个空格）；
  - netlink 的 `RTM_NEWLINK` 加上 `IFLA_STATS`（24 个 u32，截成 32 位）和 `IFLA_STATS64`（Linux 6.1 的 25 个 u64），musl 的 getifaddrs 把前者放进 `AF_PACKET` 那一项的 `ifa_data`；
  - 新增 `/sys/class/net/<接口>`：`statistics/` 下 Linux 的 24 个文件，另有 `address`、`addr_len`、`broadcast`、`carrier`、`flags`、`ifindex`、`iflink`、`mtu`、`operstate`、`type`，取值与 netlink 的回答一致（类型只分回环和以太网，`flags` 与 Linux 一样不含 `IFF_RUNNING` 这些按状态算的位）。Linux 上接口目录是指向 `/sys/devices` 下面的符号链接，这里是目录：宿主的设备路径拿不到，都指向 `devices/virtual/net` 又会让按它区分虚拟接口的程序把 wlan0 当成虚拟的。

## 结果

只在 socket 的收发路径上。用一个回环上的微基准（`target/mem/netbench.c`：5000 次短连接，每次各收发 100 字节；5 万次 UDP 来回；1 GB 的 TCP 流），与上一轮的二进制交替跑三轮：

- 短连接 0.103–0.106 → 0.108–0.112 秒（每条连接两端各一次 getsockname，约 1 微秒）；
- UDP 来回 0.121–0.125 → 0.125–0.128 秒；
- TCP 流 0.273–0.276 → 0.272–0.283 秒，在噪声之内。
- 第一版每次给通配地址的 socket 都 getsockname，UDP 来回多了 15%；连接两端还各取一次 `TCP_MAXSEG`，短连接多了 10%。改成记下通配地址、MSS 按 MTU 估计之后是上面的数。

## 测试

- 新增的 guest 程序 `tests/guest/traffic.c`：在回环上用 TCP、UDP 收发之后，`/proc/net/dev`、sysfs、getifaddrs 的 `ifa_data` 三处 lo 的计数相同、增加的字节数正好是载荷。只在 tuxport 里成立（Linux 还算协议头），放在 cargo test 里。变异检验发现收发的字节数正好相等时 `rx_bytes` 读成发出的也查不出来，所以加了一个没人读的数据报。
- 差分测试 compat 组新增一段：`/proc/net/dev` 的格式，`/sys/class/net/lo` 的属性和 `statistics/` 下的文件列表，回环上收发之后计数增加、`/proc/net/dev` 与 sysfs 一致，与原生逐行相同。
- 单元测试：sockaddr 的解析、回环连接的计数和 MSS、没有 connect 的数据报按对端认，`IFLA_STATS` 截成 32 位、`IFLA_STATS64` 完整。

## 推迟项

- 包数是估计的，字节数不含协议头：与原生的计数比偏小，`ifconfig` 里的平均包长偏大。
- 发往本机别的接口上的地址（比如自己的 wlan0 地址）的连接算在那个接口上；Linux 上这种流量走 lo。
- 错误、丢弃、组播等计数都是 0：宿主 socket 上看不到这些。
- `/sys/class/net/<接口>` 只有上面这些文件，没有 `device`、`queues/`、`speed`、`duplex`、`wireless/` 等；`/sys/class` 下只有 `net`。
