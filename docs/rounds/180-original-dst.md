# M9 之前：透明代理：SO_ORIGINAL_DST、SO_MARK 和 owner 匹配（2026-11-02）

起因是用户问 ShadowSocks、proxychains 能不能用。在 tuxport 里实测了 shadowsocks-rust 1.24（Alpine 3.24 没有 shadowsocks-libev）和 proxychains-ng 4.17：

- 能用：`sslocal` 的 SOCKS5、HTTP、tunnel 模式，TCP 和 UDP（SOCKS5 的 UDP ASSOCIATE、`tunnel -U`）；proxychains 的 LD_PRELOAD 链到 sslocal 或者外面的 HTTP 代理；`proxy_dns` 的远程解析；经由 ssserver 访问外网。
- 不能用的是透明代理：`sslocal --protocol redir`（原来的 ss-redir）配上 iptables 的 REDIRECT。规则写得进去，connect 时 socket 层也照规则把连接换到了 sslocal 上（[nftables 的子集](172-nftables.md)），但 sslocal 接受连接之后用 `getsockopt(SOL_IP, SO_ORIGINAL_DST)` 问原来要连哪里，tuxport 把它交给了宿主，宿主上没有这个连接的连接跟踪，是 ENOENT（"couldn't get destination"），客户端被重置。
- tun 模式（`sslocal --protocol tun`、clash、sing-box 的 TUN）没有 `/dev/net/tun`，sslocal 启动就 panic。这个不做（要在 tuxport 里另做一套用户态的 TCP/IP 栈，见推迟项）。

这一轮补上 `SO_ORIGINAL_DST`，让透明代理能用。做完之后实测 sslocal 的 redir 模式，又发现两种常见的配置不行，一起补上：

- `iptables -m owner --uid-owner …` 排除代理自己的连接：tuxport 不执行 xt 的 owner 匹配，规则不匹配。
- 代理给自己的出站连接设 `SO_MARK`（`sslocal --outbound-fwmark`，clash 的 `routing-mark`，v2ray 的 `sockopt.mark`），规则放过有标记的：tuxport 把 `SO_MARK` 交给宿主，nftables 看不到标记，代理连上游又被 REDIRECT 回自己，8 秒里转了 1529 圈。

## 做法

- **原生的语义**：先在特权容器里量了 Linux 6.8 在各种情形下回答什么（`nf_conntrack_proto.c` 的 `getorigdst`、`ipv6_getorigdst`）：
  - 不是 TCP 的是 ENOPROTOOPT，optlen 不够（`sockaddr_in` 的 16、`sockaddr_in6` 的 28）是 EINVAL，都在查找之前。写进 sockaddr，optlen 原样写回（给 64 的还是 64）。
  - 按 socket 的四元组找连接跟踪的条目。条目里原方向的目的地址就是答案：接受的一端报告 nat 之前的地址，发起的一端报告它连的那个。客户端在 accept 之前就关掉了、accept 之后才问，都还找得到。
  - 连接跟踪只在规则集里有用到它的表达式时才开（`nf_ct_netns_get`）：没有规则、只有空的 nat 链时都是 ENOENT；有 ct（比如 `ct state`）、nat、redirect、masquerade 或者 xt 的 REDIRECT、DNAT、conntrack、state 这类的规则时，没被 nat 的连接也找得到，报告本端地址。ip、ip6 的表各管各的，inet 的两种都管。
  - 按包的协议族分开：IPv4（含 AF_INET6 的 socket 上 IPv4 映射的地址）的连接只有 `SOL_IP` 找得到，IPv6 的只有 `SOL_IPV6`，另一个是 ENOENT；AF_INET 的 socket 上的 `SOL_IPV6` 是 EOPNOTSUPP。
  - 跨命名空间时各个命名空间有各自的条目：B 的 PREROUTING 做的 REDIRECT，B 里接受的一端报告 PREROUTING 之前的地址；A 的 OUTPUT 做 DNAT 连到 B，B 里看到的是 DNAT 之后的（就是它的本端地址），A 里的客户端报告它连的那个。
- **记下被 nat 的连接**（`core/kernel/src/origdst.rs`）：tuxport 没有连接跟踪，nat 在 connect 时由 socket 层做，宿主上的连接连的就是 nat 之后的地址。connect 把被 nat 换过目的地址的 TCP 连接按宿主上的四元组（客户端在宿主上的本端，宿主上连的目的地；IPv4 写成 IPv4 映射的，两种 socket 连起来也对得上）记下各个命名空间里看到的原目的地址：发起连接的命名空间里是 OUTPUT nat 之前的，进入的别的命名空间里是那里的 PREROUTING nat 之前的（`route` 的结果里多了这一项）。阻塞的 connect 也在放开 BKL 等待之前记下，对方不会先 accept。没被 nat 的连接去掉同一个四元组以前留下的记录。
- **接受的一端领走**：accept 时按宿主上的对端、本端找到记录，取走自己的命名空间里的那个原目的地址，记在 socket 上（`Track::orig_dst`），之后随时问都在。没人领的记录（连到 tuxport 外面的监听、连接失败的）留 120 秒（Linux 的 SYN_SENT、TIME_WAIT 超时），最多 4096 条，满了丢最早的。
- **getsockopt**（`syscall::net::original_dst`）照上面的原生语义：接受的一端有领到的就报告它，没有而命名空间开着连接跟踪（`Nft::tracks`，按规则里的表达式判断，表停用了也算，与 Linux 在表达式初始化时打开一样）就报告本端地址；发起的一端开着连接跟踪就报告它连的；没有连接的是 ENOENT。发起的一端的协议族按它连的地址判断：新命名空间里 AF_INET6 的 socket 的本端在宿主上是 127.a.b.1，换回来总是 `::1`，分不出连的是不是 IPv4 映射的地址。
- **`SO_MARK`**：只记在 socket 上（`Track::mark`），不设到宿主 socket。App 在宿主上本来就没有这个权限（EPERM），而且 Android 的 netd 用宿主 socket 上的标记选网络，guest 不该碰它；原来 getsockopt 读到的也是 netd 设的标记。照 Linux 要 socket 的网络命名空间的属主里的 `CAP_NET_RAW` 或者 `CAP_NET_ADMIN`，optlen 不到 4 是 EINVAL；accept 出来的沿用监听的（`inet_csk_clone_lock` 的 `ir_mark`）。本机发出的包在 nftables 里带着它（`Packet::mark`）：OUTPUT 的 nat、filter 和经由 lo 回到自己的命名空间的 INPUT 上 `meta mark` 读到它，经由 veth 进入别的命名空间时清掉（`skb_scrub_packet`）。
- **xt 的 owner、mark 匹配**（`nft.rs` 的 `xt_match`）：owner 照 `owner_mt`，本机发出的包看 uid、gid 的范围和取反，没有 socket 的包（PREROUTING 上）只在什么都不要求时匹配，所以 `! --uid-owner` 匹配；mark 按掩码比。Alpine 的 iptables-nft 把 `-m mark` 翻译成 nft 原生的 `meta mark`，用不到 xt 的那个，较早的版本才用，所以只有单元测试。

## 测试

- 新的差分测试组 `origdst`（原生一侧是特权容器，规则真的在内核里执行）：各种规则和表下接受的一端、发起的一端回答的原目的地址（含双栈、跨命名空间），`SO_MARK`，owner、mark 匹配，一个 Python 写的透明代理。两个执行层下都与原生逐字相同。单元测试在 `origdst`、`Nft::tracks` 和 xt 的 mark、owner 里。
- 实测 sslocal 的 redir 模式（Tier 1）：以 nobody 运行的 curl 经由它访问源站，三种排除代理自己的连接的写法（`-m owner --uid-owner`、nft 的 `meta skuid`、`meta mark` 加上 `--outbound-fwmark`）都拿到了页面、ssserver 每次只转一次；改之前三种都不行。

## 推迟项

- TPROXY（`IP_TRANSPARENT` 加 mangle 表的 TPROXY 和策略路由）：UDP 的透明代理（`sslocal --udp-redir tproxy`）要它。App 在宿主上没有 `IP_TRANSPARENT` 要的权限，包也不经过 tuxport 的规则，要模拟得在 socket 层另做一套，没做。TCP 用 REDIRECT 就够了。
- tun（`/dev/net/tun`）：clash、sing-box 的 TUN 模式和 tun2socks 要它，需要在 tuxport 里另做一套用户态的 TCP/IP 栈（[容器网络的普查](169-container-network-survey.md)讨论过的方案 3），不做。
- 从 tuxport 外面经由转发线程（[发布的端口](173-port-forwarding.md)）连进来、被开机时的命名空间的 PREROUTING DNAT 到同一个命名空间里的监听：转发线程不登记，接受的一端报告的是本端地址，Linux 上是外面连的那个地址。手机上用不到（别的设备的流量不经过 tuxport 的规则）。
- 容器经由默认路由出去的连接不经过外层的 PREROUTING（[经由虚拟网卡的连接](171-netmap.md)，原来就是），在外层用 REDIRECT 透明代理容器连外网的流量不行；容器里自己的 OUTPUT 上的规则可以。 → 经由外层网关出去的也执行外层的 PREROUTING 了（M9 之前：容器连外网也经过外层的 PREROUTING）
- owner 匹配和 `meta skuid` 一样用 connect 时的有效 uid、gid，Linux 用的是打开 socket 文件的那个凭据的 fsuid、fsgid，平时相同；附加组（`--suppl-groups`）不看；规则里的 uid 不按网络命名空间的属主换算（容器的用户命名空间里写的规则按内核 uid 比）。
- 包在 OUTPUT 上被 `meta mark set` 改过的标记不带到之后的钩子上（INPUT 上读到的还是 socket 的），原来就是每条链各算各的。
- 真正的 MPTCP 连接（两端都是 MPTCP）上 Linux 的 `SO_ORIGINAL_DST` 是 EOPNOTSUPP，tuxport 当作 TCP 回答。Go 1.24 起监听默认用 MPTCP，但客户端默认不用，接受的连接会退回 TCP，与这里一样。
- 用 `sendto` 带 `MSG_FASTOPEN` 发起的 TCP 连接原来就不过 nat，这里也不登记。
- **测试时发现的另一个问题**：跨命名空间那组的探测脚本里，一个 python3 进程偶发 SIGILL：日志总是先报它离开 `/usr/bin/python3.14` 的共用翻译（"an executable mapping differs from theirs"），接着在它自己的一段匿名内存里执行到数据（`undefined instruction 0x00000034`）。触发的条件是同一个程序的几个进程加载的扩展模块不同（服务端 import 了 select，客户端没有），同一个地址上映射的东西不一样。第 178 轮的二进制也有（8 次里 3 次），不是这一轮引入的，像是[同一个程序共用翻译](108-shared-translation.md)离开共用组时没断干净。下一轮查；`origdst` 组里各个角色都先 import 同一组模块，避开它。 → 是组里最后一个成员离开时留着的翻译里有别的成员翻译的块，修了（M9 之前：共用翻译的组里最后一个成员离开时作废别人留下的块）
