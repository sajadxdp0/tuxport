# M9 之前：traceroute 和 mtr（2026-10-31）

[raw ICMP 的模拟](125-raw-icmp.md)的两条推迟项：traceroute、mtr 不能用，新的网络命名空间里 `ping6 ::1` 失败。

先分别在 tuxport 和原生容器里量了各个工具，宿主有 `CAP_NET_RAW`（开发用的容器）和没有的（`--cap-drop NET_RAW`，像 Android 上的 App）两种都跑：

| 工具 | 原生（root） | 原生（没有 NET_RAW） | tuxport 原来 | tuxport 现在 |
|---|---|---|---|---|
| busybox `traceroute`（Alpine 默认的，UDP 探测包） | 正常 | —（要 raw socket） | 全是 `*` | 与原生相同 |
| busybox `traceroute -I`、`traceroute6` | 正常 | — | 全是 `*` | 与原生相同 |
| Debian `traceroute`（UDP，`IP_RECVERR`）、`tracepath` | 正常 | 正常 | 正常 | 正常 |
| Debian `traceroute -I` | 正常 | 第二跳 `*`（colima 的 NAT） | 前两跳 `*` | 与没有 NET_RAW 的原生相同 |
| mtr（ICMP） | 正常 | 正常（退回 ping socket） | 开发宿主上什么都没有 | 两种宿主都正常 |
| `mtr -u` | 正常 | 什么都没有 | 什么都没有 | 什么都没有 |
| 新的网络命名空间里 `ping6 ::1` | 正常 | 正常 | EINVAL | 正常 |

原因有三处：

- **raw ICMP socket 只收得到自己的 echo 回答**。Linux 的 raw ICMP socket 收到发给本机的每个 ICMP 报文，traceroute 用一个 socket 发探测包（UDP，或者 `-I` 时的 raw socket），用另一个 raw socket 收网关报告的超时、目的地报告的不可达。tuxport 的 raw socket 在宿主上是 ping socket，只收得到自己的 echo 回答；别的 socket 发的报文引起的 ICMP，以及超时这类错误，一个也收不到。
- **mtr 在开发宿主上两头对不上**：它用 `IPPROTO_RAW` 的 raw socket 发（开发宿主有 NET_RAW，交给了宿主的 raw socket），用 raw ICMP socket 收（模拟的）。宿主的 raw socket 发出去的报文引起的 ICMP 到不了模拟的 socket。Android 上宿主不让建 raw socket，mtr 退回 ping socket，本来就能用。
- **新的网络命名空间里的 ::1**：地址换算把 ::1 换成宿主上 IPv4 映射的 127.a.b.1（[网络命名空间](122-net-namespaces.md)），ICMPv6 发不到 IPv4 映射的地址。

## 做法

guest 的 raw ICMP socket 的实现挪到 `core/kernel/src/rawicmp.rs`。

- **汇集点**（`Tap`，每个网络命名空间一个）：一个宿主 epoll 看着各个来源，来源是 raw socket 背后的 ping socket（宿主上替它打开 `IP_RECVERR`）和下面的 UDP socket。来源上到了的报文、错误队列里的错误由汇集点取下来（`dispatch`），按协议族分给命名空间里每个 raw socket 的队列（按它的 `ICMP_FILTER` 过滤，队列最多 212992 字节，与 Linux 默认的 `rmem_default` 相同）。raw socket 收的时候自己先取一遍，没有就连同汇集点的宿主 fd 一起等；poll、epoll 的就绪只看队列，由定时器线程看着各个汇集点，有东西就取下来、叫醒等待者。原来过滤器挡掉的报文也让 poll 报告可读，现在分发时就过滤掉了。
- **错误拼回 ICMP 错误报文**：错误队列给的是 `sock_extended_err`（类型、代码、附带的值）、报告者的地址、原来报文的目的地址和原来报文的开头（UDP 的只给载荷）。这里拼回 raw socket 上收到的样子：外层的 ICMP 头（需要分片时带 MTU，参数错误带位置），引用的 IP 头（源地址取错误报文的目的地址或本端地址，本端是通配地址时按路由取；目的地址取错误队列给的；TTL 记作 1），引用的报文（UDP 的补上 UDP 头，端口取自本端地址和原来的目的地址）。IPv6 的引用 IPv6 头，校验和带伪头部。外层的 TTL 取宿主的 `IP_TTL` 控制消息。
- **echo 的 id**：回答、错误不一定到发出它的那个 ping socket 上，于是按宿主上的编号（ping socket 的端口）查是哪个 guest socket 发的，换回它的 id（原来只换回到达的那个 socket 自己发的 id）。
- **raw socket 没有端口**：Linux 的 `raw_bind` 不看端口，getsockname 报告的端口是协议号；宿主的 ping socket 却把端口当作编号。busybox 的 traceroute 把收的 socket 绑到发的 socket 的地址上（getsockname 拿到的，带着端口），两个 ping socket 就共用一个编号，宿主把错误交给了先绑上的那个。现在 bind 时去掉端口，getsockname 报告协议号。
- **UDP 探测包**：设过 `IP_TTL`、`IPV6_UNICAST_HOPS` 的 UDP socket，guest 没有自己打开 `IP_RECVERR`、命名空间里又有同一个协议族的 raw socket 时，在宿主上替它打开 `IP_RECVERR`，交给汇集点看着（`before_io`）。Linux 上没有打开 `IP_RECVERR` 的 UDP socket 的规矩照样模拟：
  - 宿主收到 ICMP 错误时把它记在 socket 上，下一次收发因此失败。这些 socket 收发、查 `SO_ERROR`、poll 之前先取走错误；因为它失败了也取走、再试一次。
  - 没有连接的 socket 看不到错误；连接了的，只有与连接对得上的硬错误（端口不可达等，`icmp_err_convert` 的 fatal）报告给它，下一次收发、`SO_ERROR` 得到它，poll 报告 POLLERR（Linux 的 `sk_err`）；超时这类软错误不报告。
  - 已连接的 socket 发往别的地址（busybox 的 traceroute 连上目的地址之后每个探测包换一个端口）：宿主只把与连接对得上的报文引起的错误交给 socket，所以先把宿主 socket 改连到这个地址再发；getpeername 仍报告 guest 连的，报告给 guest 的错误也按 guest 连的对端比。
  - guest 之后自己打开 `IP_RECVERR`，就不再看着，宿主上的就是 guest 的；guest 读 `IP_RECVERR` 得到它自己设的。
- **raw socket 上的 `IP_RECVERR`**：Debian 的 traceroute 以 root 运行 `-I` 时打开它，从错误队列收超时报文，普通的数据里只认 echo 回答。原来只记下来、错误队列总是空的。现在它自己的 ping socket 上取下的错误另外放进它的错误队列（`MSG_ERRQUEUE` 读到的与 Linux 的 `ip_recv_error` 一样：原来的报文、原来的目的地址、`IP_RECVERR` 控制消息），每个错误都报告（Linux 的 `raw_err`），读走一条时换成下一条的。
- **别的 raw socket 建不了**：guest 的非 ICMP 的 raw socket（`IPPROTO_RAW` 等）不再交给宿主，与 Android 上一样是 EPERM。mtr 遇到它退回 ping socket，开发宿主和手机上表现一样。宿主有 NET_RAW 时 guest 因此少了这个能力，但交给宿主的 raw socket 发出去的报文引起的 ICMP 本来就到不了模拟的 raw ICMP socket。
- **ICMPv6 在新的网络命名空间里**：ICMPv6 的 socket 发不到 IPv4 映射的地址，::1 换成宿主真正的 ::1，通配地址也绑到 ::1，别的地址照样到不了（`netns::to_host` 多了一个参数）。echo 由宿主内核回答，没有别的东西经由它。

## 测试

- 单元测试：`rawicmp` 的三个（回答换回 id 和拼 IP 头、错误拼成 ICMP 错误报文、控制消息截短），`netns` 的地址换算加上 ICMPv6 的几种。
- 差分测试 `compat2` 组新增 `icmperr.c`：UDP socket 引起的端口不可达在 raw socket 上收到的报文、UDP socket 自己看到的错误（没连接、连接了、发往另一个端口），另一个 raw socket 的 echo 回答，raw socket 的 `IP_RECVERR` 和错误队列；另外跑 busybox 的 `traceroute`、`traceroute -I`、`traceroute6` 到本机。`namespaces` 组新增新命名空间里的 `ping6 ::1`。两个执行层下都与原生相同。到外面地址的 traceroute 没有放进去：网关对 ICMP 限速（每秒一个），几个探测包接着发时原生上也时有时无。
- 手工对照上表的各个工具：Debian 13 的仓库里装上 traceroute、mtr-tiny、iputils-tracepath，在有和没有 NET_RAW 的宿主上各跑一遍，与原生的 Debian 容器比。
- 开发中差分测试抓到：没有连接的 UDP socket 引用的源地址是 0.0.0.0（现在按路由取）。
- 非 ICMP 的 raw socket 改成 EPERM 没有改变 LTP 任何一项的结果。

## 推迟项

- 只看着设过 TTL 的 UDP socket：Linux 的 raw socket 收得到所有 UDP 报文引起的错误，这里只收得到 traceroute 这类探测包的；guest 的 ping socket（不是 raw 的）收到的回答 raw socket 也收不到；发给本机的 echo 请求、重定向这些到不了 ping socket 的 ICMP 仍收不到。
- 已连接的 UDP socket 在汇集点看着时发往别的地址，宿主连接改到了那个地址，之后只收得到从它来的数据报（Linux 上仍是从连的对端来的）。只有设过 TTL、又有 raw socket 时才会这样。
- 引用的 IP 头的 TTL 记作 1，id、标志是 0；UDP 引起的错误的外层 TTL 是 64（没有在 UDP socket 上替它打开 `IP_RECVTTL`）。IPv6 的 UDP socket 发往 IPv4 映射的地址时的错误不拼。
- raw socket 发出的 ICMP 由宿主算校验和：Linux 的 raw socket 原样发出，校验和不对的报文会被对方丢掉。
- 错误队列只放这个 raw socket 自己的探测包引起的错误（Linux 放进所有地址对得上、打开了 `IP_RECVERR` 的 raw socket），最多 64 条。
- `mtr -u` 仍不能用：它的 UDP 探测包要用 raw socket 发，与没有 NET_RAW 的原生一样。
- 新的网络命名空间里 ICMPv6 用的是宿主的 ::1，不是命名空间自己的回环。
- 只在 colima 的虚拟机里跑过，没上手机。
