# M9 之前：过滤链（2026-11-01）

[第 172 轮](172-nftables.md)的推迟项：nftables、iptables-nft 的规则集只执行 nat 链，过滤的链只记下、能列出来，不起作用。所以下面这些都没有效果：

- docker 网络之间的隔离；
- `--internal` 网络（compose 的 `internal: true`）；
- 写在 DOCKER-USER 里的规则；
- podman 的 isolate；
- 容器里、外层命名空间里自己写的防火墙。

这一轮在 connect、sendto 时执行连接沿途经过的过滤链。

## 做法

- **沿途经过哪些链**（`netns::path_filter`），只看 IPv4，与 Linux 一样按数据包实际经过的命名空间：
  1. 发送方命名空间的 OUTPUT。
  2. 新命名空间里发出的，要经过开机时的命名空间（容器经由网关出去）：
     - 出到宿主网络：它的 FORWARD，出口是宿主上第一个有地址的接口。
     - 到它自己的地址：它的 INPUT。
     - 到别的命名空间、而目的地不在发送方的直连网段里（经由网关路由）：它的 FORWARD。 → 原来按 DNAT 之后的目的地判断，连外层地址上发布的端口、连外网被外层 DNAT 到同一个网段里的容器时不过 FORWARD；现在按发送方路由时的目的地：经由网关的、外层的本机地址都过（M9 之前：容器连外网也经过外层的 PREROUTING）
     - 同一个 bridge 上的容器之间是二层转发，不经过它（相当于没有 br_netfilter）。
  3. 到了目的地的命名空间：它的 INPUT（同一个命名空间里也算，回环上的包 Linux 也过 INPUT）。
  - 进入一个命名空间时先过它 PREROUTING 上的 filter 链（raw 表这些）。
  - 入口接口是那个命名空间里面向源地址的接口，FORWARD 的出口接口是面向目的地的接口。PREROUTING、INPUT 上 oifname 是空的。
  - 目的地是 OUTPUT、PREROUTING 的 nat 之后的（`nftables` 的 DNAT 之后的地址才是过滤的对象）。
- **过滤链的执行**（`Nft::filter`）：
  - 跑法：钩子上 filter、route 类型的基础链按优先级逐个跑。一个链接受了，接着跑下一个；有一个丢弃或者拒绝就停。
  - reject 带上发起方收到的错误码，与 Linux 的 `icmp_err_convert` 一样：TCP RST、ICMP 端口不可达是 ECONNREFUSED，主机不可达、管理禁止是 EHOSTUNREACH，网络不可达是 ENETUNREACH。xt 的 REJECT 按 `--reject-with`。
  - 新增的表达式：xt 的 `conntrack`（`--ctstate`）和 `state` 匹配。
  - 连接跟踪的状态：connect、新的数据报是 NEW；UDP 回给收到过的对方是 ESTABLISHED（socket 记下收到过的对方，最多 16 个）。所以 docker 那种"先放行已建立的、再丢弃别的"的规则下，回答照常回得去。
- **结果**：
  - TCP：
    - 拒绝的，connect 马上返回那个错误码。
    - 丢弃的（包括 OUTPUT 上的：Linux 上 SYN 发不出去也是重传到超时），连到一个接受队列已经满了、从不 accept 的宿主监听上。宿主悄悄丢掉 SYN，连接照 Linux 的重传超时失败（ETIMEDOUT），guest 自己设的超时照样起作用，非阻塞的 connect 一直是 EINPROGRESS。这个监听每个内核一个，第一次用到时建好、填满。
  - UDP：
    - OUTPUT 上丢弃、拒绝的，sendto 是 EPERM（Linux 的 `ip_local_out`）。
    - 别处的悄悄丢掉。
    - 连接了的 socket 在 connect 时判断，之后不带地址的发送照这个结果。
- **从外面转发进来的**（[第 173 轮](173-port-forwarding.md)的转发线程）：判断转不转发时，外面来的包也过开机时的命名空间的 FORWARD（入口是宿主的接口）和目的地命名空间的 PREROUTING、INPUT。丢弃了的不转发。

## 测试

- 新的差分测试组 `filter`（原生一侧是特权容器，数据包真的经过过滤链）：两个 bridge 上的 A、B 经由外层命名空间路由，在外层的 FORWARD、INPUT 和 A、B 自己的链上用 nftables、iptables-nft 写丢弃、拒绝和放行已建立的规则，比连接的结果和错误码、UDP 的回答。两个执行层下都与原生逐字相同。原生上 OUTPUT 上的 DROP 对 TCP 不是 EPERM，是重传到超时；`fwd`、`in` 是 nft 的关键字，不能当链名。单元测试（`nft`）测过滤链上的丢弃和 reject、conntrack 匹配。
- 第一版用例分不出丢弃的 TCP 是马上失败还是等到超时（Python 把 ETIMEDOUT 也当作超时，变异检验发现的），客户端加上等了多久，再加一条非阻塞 connect 一秒之内不可写。
- 普查：docker 的普查加了"网络之间的隔离"一步，开着 iptables 时从另一个网络连不上，与原生相同（原来 tuxport 里连得上）；关着时和 `--internal` 网络也与原生相同。

## 推迟项

- 只看 TCP、UDP 的 IPv4。ICMP（ping）、IPv6、raw socket 不过过滤链。
- 没有 br_netfilter：同一个 bridge 上的容器之间不过外层的 FORWARD。docker 的 `--icc=false`（同一个网络里的容器互相隔离）不起作用。
- 回答方向只认 UDP 的 ESTABLISHED（收到过的对方）。TCP 的回答都在宿主的连接上，不再过过滤链；Linux 上已建立的连接被后来加的规则丢弃的情形这里没有。连接跟踪的别的状态（RELATED、INVALID）、`ct status`、连接数、`limit` 这些都没有。
- 从外面转发进来的连接按探测的包判断（与转发本身一样）。宿主上转发线程接受的连接，只看规则允不允许外面来的包。
- 丢弃的 UDP 不产生 ICMP。reject 的 UDP 与丢弃一样：连接了的 socket 收不到 ECONNREFUSED。
- 每次 connect、新的 sendto 都按规则判断：没有连接跟踪的缓存，规则多时有开销。没有规则的命名空间只看一下有没有过滤链。
