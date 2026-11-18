# M9 之前：nftables 的子集和 socket 层的 DNAT（2026-11-01）

[容器网络的普查](169-container-network-survey.md)定下的方案 1 的第三步。[上一轮](171-netmap.md)之后，docker 自定义网络里还是不能按名字解析。dockerd 的内置 DNS 在每个容器的网络命名空间里监听 127.0.0.11 上的两个随机端口，再用 iptables 写 nat 规则，把发往 127.0.0.11:53 的转过去。Alpine 的 iptables 是 nftables 后端，走 `NETLINK_NETFILTER`，而 tuxport 原来没有这个协议（EPROTONOSUPPORT），所以 iptables 失败，容器里的 nslookup 等不到回答。同样的原因，`"iptables": true` 时 dockerd 起不来；podman 用 nftables 驱动时 netavark 调 nft 也失败，容器起不来。这一轮在内核层照 Linux 记下 nftables 的规则集，在 connect、sendto 时执行 OUTPUT 钩子上的 nat 链，DNAT 在 socket 层换目的地址。

## 做法

- **`NETLINK_NETFILTER`**（`core/kernel/src/nft.rs`）：每个网络命名空间有一份规则集（`NetNs::nft`）。
  - 修改要放在批里（`NFNL_MSG_BATCH_BEGIN` 到 `BATCH_END`，`res_id` 要是 nftables 子系统，不然 EOPNOTSUPP）。批在规则集的副本上逐条做，都成功才换上，生成号加一（新命名空间里从 1 起）。有一条失败，整批作废，回答失败的那几条和要求了确认的。批开头给的生成号过时了是 ERESTART。
  - 6.3 起的 `NFT_MSG_DESTROY*` 与对应的删除一样，只是要删的东西不存在时也算成功。
  - 查询在批外：GETGEN，以及表、链、规则、集合、元素、对象、flowtable 的单条查询和转储。修改放在批外、查询放在批里都是 EINVAL。查询也要在命名空间的属主里有 `CAP_NET_ADMIN`（只确认第一条）。
  - 能建的东西：
    - 表：地址族 inet、ip、ip6、arp、bridge、netdev，可以设 dormant 标志。
    - 链：基础链（钩子、优先级、类型 filter/nat/route、策略、计数器）和普通链。
    - 规则：追加、插在最前、按位置插、按句柄替换。
    - 集合和元素：匿名集合 `__set%d` 取最小的没用过的编号，同一批里可以按 `SET_ID` 引用它。
  - 句柄和引用计数：表的句柄是命名空间里的计数器，删掉的不再用；链、规则、集合共用表里的计数器。链的 USE 是规则数加上跳到它的规则数，表的 USE 是链和集合的个数。
  - 规则的表达式原样记下（去掉 `NLA_F_NESTED`），转储时原样带回：iptables 的 `-C` 靠比较转储出来的规则。引用匿名集合的 lookup、按编号跳转的 immediate 换成名字。转储的属性顺序和标志照 6.8 的内核（原生对照）：规则带 `NLM_F_APPEND` 和前一条规则的句柄（`NFTA_RULE_POSITION`），集合元素的转储最后总有一条空的。
  - 检查：
    - 表达式的名字要认识（ENOENT）。
    - 跳到的链要存在（ENOENT），不能是基础链（EOPNOTSUPP）。跳回自己（直接或者经由别的链）是 EMLINK：6.8 的内核在提交前按跳转深度检查。
    - 被跳到的链、被用着的集合删不掉（EBUSY）。
    - 给已有的链换钩子、类型或者优先级是 EOPNOTSUPP。nft 收到这个错误后重新取缓存，自己报告 "already exists ... with different declaration"。
    - 带 `NLM_F_EXCL` 时，已有的是 EEXIST。
  - 出错的属性按 Linux 的 `NL_SET_BAD_ATTR` 记下。设了 `NETLINK_EXT_ACK` 的 socket 收到的错误里带着这个属性在请求里的偏移（`NLMSGERR_ATTR_OFFS`），nft 按它在命令里标出出错的那个词。
  - `nft_compat`：`NFNL_MSG_COMPAT_GET` 回答 iptables-nft 问的 xt 扩展有没有、最高版本是多少，共 52 个：DNAT、SNAT、MASQUERADE、REDIRECT、MARK 等目标，tcp、udp、addrtype、conntrack、comment 等匹配。没有的是 ENOENT。
  - conntrack（`CTNETLINK`）的转储总是空的。
- **执行**（`Nft::output_nat`）：
  - 何时跑：connect、sendto 之前，按目的地址跑 OUTPUT 钩子上 nat 类型的基础链。IPv4 的包看 ip 和 inet 的表，IPv6 的看 ip6 和 inet 的。链按优先级排，有一条做了 DNAT 或者丢弃就停（`nf_nat_inet_fn`）。
  - 包和寄存器：包按 socket 合成（IPv4 或 IPv6 头；TCP 头是 SYN，UDP 头照常）。寄存器照 `nft_regs`，`NFT_REG_1..4` 与 `NFT_REG32_00..15` 重叠。
  - 支持的表达式：
    - payload（网络层、传输层的头）、cmp、bitwise（布尔、移位）、range、immediate。
    - meta：l4proto、nfproto、protocol、mark 的读写、skuid/skgid、oifname；入口接口是空的。 → 出口接口的编号（`oif`，iptables-nft 的 `-o` 翻译成 oifname，手写的 nft 规则常用 `oif`）也认了（M9 之前：容器连外网也经过外层的 PREROUTING）
    - fib 的地址类型：本机地址是命名空间里各个接口上的地址和 127.0.0.0/8。
    - ct：状态总是 NEW，方向总是 ORIGINAL。
    - lookup：精确匹配、区间、映射和判决映射。
    - nftables 自己的 dnat 和 redirect；xt 目标 DNAT（版本 0–2）、REDIRECT、MARK；xt 匹配 comment、addrtype、tcp、udp。
    - counter、log、limit 这类当作没有作用。别的表达式当作规则不匹配。
  - 判决：accept、drop（connect、sendto 返回 EPERM）、jump、goto、return、continue，跳转最多 16 层。
- **socket 层**（`core/kernel/src/syscall/net.rs`）：
  - DNAT 了的 socket 连向新的目的地址，然后照上一轮换成宿主上的地址；getpeername 仍报告 guest 连的那个。
  - socket 记下（换成的地址, 原来的地址）。从换成的地址收到的数据报，报告成来自原来的地址（Linux 上由 conntrack 换回）。所以 docker 内置 DNS 的回答来源是 127.0.0.11:53：musl、glibc 的解析器都按来源认回答。未连接的数据报 socket 最多记 16 对。
  - 没有规则的命名空间不跑规则，只看一下表是不是空的。
- **发给自己的虚拟地址的 UDP**：这是上一轮留下的问题。新命名空间里，发给自己 eth0 地址的 socket 绑在 127.a.b.254 上，这样两边看到的都是虚拟地址；可对方的回答发往 127.a.b.1，收不到。现在这种 UDP socket 的端口登记在命名空间里：发往虚拟地址上这些端口的数据报送到 127.a.b.254，它收到的来自 127.a.b.1 的数据报报告成来自虚拟地址。

## 测试

- 新的差分测试组 `nftables`（原生一侧是特权容器，规则真的在内核里执行）：用 iptables-nft 照 dockerd 的顺序建内置 DNS 的规则、用 `nft -f` 装 netavark 那样的规则，比列出的规则和句柄、出错的情形和 nft 标出的位置、经由 DNAT 的流量两边看到的地址、删规则和删表之后。两个执行层下都与原生逐字相同。原生上表的句柄不总是从 1 起，这一项不比。单元测试（`nft`）测这两种批和区间集合的查找。
- 普查（与原生对照）：
  - docker 两种配置下自定义网络都能按名字解析了，`"iptables": true` 时 dockerd 起得来，两层命名空间里的规则与原生相同。
  - podman 的 nftables 驱动原来容器起不来，现在各项与原生一致，`-p 8080:80` 经由 127.0.0.1 和外层地址都拿到页面（走 OUTPUT 上的 DNAT）。
  - 与原生不同的地方：
    - 容器里 ping 外层的网关通了（上一轮的推迟项）。
    - podman 占着 8080 的那个监听 socket 没有（第 4 步）。 → 有，原来是 MPTCP 的没列出来（M9 之前：发布的端口）
  - podman 的 `-p` 那一节有一次"经由外层地址"是 Connection refused：服务端是 `nc -l` 的循环，tuxport 里起进程慢，紧接着的第二个连接可能赶在 nc 重新起来之前。
- 差分测试的仓库加装了 `iptables`、`nftables`（`scripts/difftest` 的软件包列表），镜像要用 `-r` 重新导出。

## 推迟项

- 只执行 OUTPUT 钩子上的 nat 链。别的钩子上的链只记下、能列出来，不执行：
  - PREROUTING 的 DNAT：从宿主外面连进来的端口转发，第 4 步。 → 从别的命名空间进来的执行 PREROUTING，从 tuxport 外面进来的由宿主上的线程转发（M9 之前：发布的端口）
  - POSTROUTING 的 SNAT、MASQUERADE。
  - INPUT、FORWARD 等过滤的链：防火墙的隔离、丢弃规则都不起作用。容器出去总是有 NAT（上一轮的推迟项）。 → 连接沿途经过的 OUTPUT、FORWARD、INPUT、PREROUTING 上的过滤链执行了（M9 之前：过滤链）
- 没有 conntrack：
  - 每次 connect 和每个未连接的 sendto 都按规则判断，服务端的回答也一样。回答的目的端口碰巧匹配 DNAT 规则时会被换掉；Linux 上回答属于已有的连接，不再看 nat 规则。
  - DNAT 的换回只按 socket 记，未连接的 UDP socket 最多 16 对。
  - `conntrack -L` 总是空的。
- 不支持的功能：
  - 计数器总是 0。
  - `nft monitor` 收不到事件：没有 `NFNLGRP_NFTABLES` 的组播。
  - 表的 owner 标志（`NFT_TABLE_F_OWNER`）是 EOPNOTSUPP。
  - 对象（counter、quota、ct helper 等）和 flowtable 是 EOPNOTSUPP；单条查集合元素（`nft get element`）也是。
  - DNAT 到另一个地址族（NAT64 那样）。
- 检查和求值都不全：
  - 表达式只检查名字，不检查参数：Linux 初始化表达式时会拒绝的规则，这里照样收下。xt 扩展的 info 不检查大小。
  - 只能求值上面列出的表达式。不认识的（exthdr、hash、numgen、rt、socket、osf……）当作规则不匹配。xt 匹配只有 comment、addrtype、tcp、udp，multiport、conntrack、mark 等当作不匹配。 → conntrack、state 认了（M9 之前：过滤链）；mark、owner 也认了（M9 之前：透明代理）；multiport 等别的 xt 匹配和 exthdr、hash 这些表达式仍当作不匹配
- 发给自己的虚拟地址的 UDP：绑在 127.a.b.254 上的 socket 的端口，可能碰巧与 127.a.b.1 上某个 socket 的端口相同（宿主分别分配），这时发往虚拟地址这个端口的数据报都送到前者。
- 合成的包不全对：
  - 源地址是 socket 绑的虚拟地址，没绑时按路由选。开机时的命名空间里经由宿主网络出去的选不出源地址，用第一个虚拟地址代替。
  - 出口接口的名字只认得 lo 和经由虚拟设备的路由。
