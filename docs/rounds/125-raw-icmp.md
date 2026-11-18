# M9 之前：raw ICMP 的模拟（2026-10-30）

Alpine 默认的 `ping` 是 busybox 的，以 root 运行时用 raw ICMP socket。tuxport 一直把 guest 的 raw ICMP socket 换成宿主的 ping socket（Android 上 App 没有 `CAP_NET_RAW`，§2、§10），但两者收到的数据不一样，busybox 的 ping 一个回答也收不到：回环和外面的地址都是 `0 packets received`。`ping6` 更早，设 `ICMP6_FILTER` 失败就退出了。[网络命名空间](122-net-namespaces.md)那一轮记过这一条，这次盘点推迟项时实测确认。

收不到有两个原因：

- IPv4 的 raw socket 收到的数据带 IP 头，busybox 按 IP 头的长度跳过去再看 ICMP；ping socket 收到的不带。
- ping socket 发出 echo 请求时把 id 换成自己的编号，回答里也是这个编号；raw socket 原样发出，busybox 按自己的 id（pid）认回答，对不上就丢掉。

## 做法

guest 建 raw ICMP socket 时，内核层记下一份 `RawIcmp`（`core/kernel/src/net.rs`），宿主上仍是 ping socket：

- **id 换回来**：记下 guest 最后发出的 echo 请求的 id，收到 echo 回答时换回它，校验和按 RFC 1624 增量更新（不用知道 ICMPv6 的伪头部）。
- **IPv4 补 IP 头**：宿主上总是打开 `IP_RECVTTL`、`IP_PKTINFO`，用它们给的 TTL 和目的地址拼一个 20 字节的头（总长度、协议、源地址、校验和都对，TOS、IP 的 id 和标志是 0）。guest 自己没打开这两项时，这两条控制消息不交给它；`getsockopt` 报告 guest 设的值。ping socket 的 `IP_PKTINFO` 不填 `ipi_spec_dst`，raw socket 填回答用的本端地址（Linux 的 `fib_compute_spec_dst`，发给本机单播地址的就是那个地址），这里照填。新的网络命名空间里，头里和 `IP_PKTINFO` 里的地址换成 guest 的。
- **完整的长度**：ping socket 的 recvmsg 不认 `MSG_TRUNC` 标志（只返回拷进去的长度），所以宿主一侧整个收下来（最多 64 KB），拼上头之后再按 guest 给的长度截，`MSG_TRUNC` 和返回值与 raw socket 相同。控制消息比 guest 给的空间多时按 Linux 的 `put_cmsg` 截：放不下的那一条截到剩下的长度、之后的不放，带 `MSG_CTRUNC`。错误队列（`MSG_ERRQUEUE`）里的原样交出。
- **raw socket 才有的选项**：`ICMP_FILTER`（SOL_RAW）、`ICMPV6_FILTER`（SOL_ICMPV6）记下来，收的时候丢掉被过滤的类型，偷看（`MSG_PEEK`）时也真的读走它，不然一直偷看到它；短的只盖住前面一部分，与 `raw_seticmpfilter` 相同。IPv6 的 `IPV6_CHECKSUM` 在 SOL_RAW 一级记下来（默认 2，负数是 -1，奇数是 EINVAL），在 IPPROTO_IPV6 一级是 EINVAL（RFC 3542），宿主的 ping socket 本来就算校验和。SOL_RAW 一级别的选项是 ENOPROTOOPT。
- **`CAP_NET_RAW`**：原来不查，普通用户也建得出 raw socket。现在与 Linux 的 `inet_create` 一样，要在网络命名空间的属主里有 `CAP_NET_RAW`，没有是 EPERM（协议号不对的先报告 EPROTONOSUPPORT、EINVAL）。busybox 的 ping 以普通用户运行时据此改用 ping socket，与原生一样。

## 测试

- 单元测试 `raw_icmp_replies_look_like_raw_ones`：id 换回和校验和、IPv4 头、控制消息和截短、过滤器、IPv6。
- 差分测试 `compat` 组新增 `rawicmp.c`：raw socket 的选项，回环上的 echo 回答的 IP 头和 ICMP，控制消息放不下时的截短，`MSG_TRUNC`，过滤器，IPv6，`CAP_NET_RAW`；另外跑 busybox 的 `ping`、`ping6` 和以 nobody 运行的 `ping`。两个执行层下都与原生相同。原生一侧回环上的 raw socket 还收到自己发出的 echo 请求，用例都先用过滤器只留回答。
- 开发中差分测试抓到两处：ping socket 不填 `ipi_spec_dst`、不认 `MSG_TRUNC` 标志。

## 推迟项

- ping socket 只收得到自己发出的 echo 请求的回答：raw socket 上收得到的别的 ICMP（发给本机的 echo 请求、目的不可达、超时）这里收不到，traceroute、mtr 这类靠 raw socket 收超时报文的仍不能用（可以经由宿主 socket 的 `IP_RECVERR` 错误队列转过来，没做）；也只能发 echo 请求，别的类型是 EINVAL。 → 汇集点把 raw socket 背后的 ping socket、设过 TTL 的 UDP socket 上到了的报文和 ICMP 错误分给所有 raw socket，busybox 和 Debian 的 traceroute、mtr 能用了（M9 之前：traceroute 和 mtr）；发给本机的 echo 请求、别的 UDP socket 引起的错误仍收不到，仍只能发 echo 请求
- 一个 socket 上用几个 id 时，回答都换成最后发出的那个（宿主的 ping socket 只有一个编号，分不出来）。
- IP 头的 TOS、id、标志都是 0，没有 IP 选项；`IP_HDRINCL`、`IPV6_HDRINCL` 设不上（LTP 的 sendmsg03 仍失败）。
- 被过滤器挡掉的报文仍让 poll、epoll 报告可读，读的时候才丢掉。 → 已改为分发时就过滤掉（M9 之前：traceroute 和 mtr）
- 新的网络命名空间里 ICMPv6 发不到 ::1：::1 换成了 IPv4 映射的地址，ICMPv6 的 ping socket 发不过去（EINVAL），`ping6 ::1` 失败，ping socket 也一样（[网络命名空间](122-net-namespaces.md)的换算）。 → 已补上：ICMPv6 的 socket 在新的命名空间里用宿主的 ::1（M9 之前：traceroute 和 mtr）
- 只在 colima 的虚拟机里跑，没上手机（Android 允许 App 建 ping socket，`ping_group_range` 包括所有组）。
