# M9 之前：容器连外网也经过外层的 PREROUTING（2026-11-02）

[透明代理那一轮](180-original-dst.md)的推迟项：不能在外层命名空间里透明代理容器连外网的流量。Linux 上给 docker、podman 的容器做透明代理，常见的写法是在外层对从 bridge 进来的连接做 REDIRECT：

```sh
nft add rule ip px pre iifname "podman0" tcp dport '{80, 443}' redirect to :1081
nft add rule ip px pre iifname "podman0" udp dport 53 redirect to :1053
```

一条规则管所有容器，包括 `podman build`、`docker build` 里 RUN 的 pip、apt，容器里不用设代理的环境变量。第二条把容器的 DNS 劫持到外层的解析器。

这些规则在 tuxport 里原来不起作用。容器经由默认路由出去的连接直接交给宿主网络。只有目的地是外层的本机地址时（连网关、连外层地址上发布的端口），才执行外层的 PREROUTING（[发布的端口](173-port-forwarding.md)）。

写测试时又发现三个原来就有的问题，一起修了：

- 外层绑在通配地址上的 UDP 服务端回答容器时，来源错了。
- 被外层 DNAT 到别的容器的连接不过外层的 FORWARD。
- nft 规则里的 `oif` 不认。

## 做法

- **进入外层**（`netns::entering`）：新命名空间里经由宿主网络出去的连接、数据报，网关是外层命名空间的地址时（容器挂在外层的 bridge 上），也进入外层、执行它的 PREROUTING nat。之后的处理与连网关时一样：
  - 被 REDIRECT、DNAT 到外层或者别的命名空间的，换成那里的宿主地址。
  - `SO_ORIGINAL_DST` 记下外层看到的原目的地址。
  - UDP 的回答报告成来自原来的目的地。
  - 没有被 nat 的照旧交给宿主网络。
  - 网关在别的命名空间里时（路由器那样的"容器"）不变，不经过谁的 PREROUTING。
- **docker、podman 自己的规则不受影响**：它们放在外层 PREROUTING 上的端口发布规则用 `fib daddr type local`、`-m addrtype --dst-type LOCAL` 限定目的地是本机地址。tuxport 按每个包的目的地址算地址类型，容器连外网的连接不会落进这些规则。
- **DNAT 到外面的别的地址**：原来进入外层之后，总是先给发送方绑上它的回环地址，外层和别的命名空间才认得出来源。可被 DNAT 到外面的地址时，绑了回环地址就出不去（EINVAL）。现在只在 DNAT 到命名空间里时才绑。
- **外层绑通配地址的 UDP socket 回答容器**（`sendto`）：
  - 原来：外层没绑定的 socket 发往容器时，先替它绑到外层面向容器的地址（宿主上的 127.255.x.y）；服务端早就绑了 `0.0.0.0:端口`，绑不上，宿主选的源地址是 127.0.0.1。容器看到回答来自 127.0.0.1，连接了的 socket 收不到。被 REDIRECT 来的 DNS 是这样，容器直接问网关上的 dnsmasq 这类也是这样（podman 的 aardvark-dns 绑的是网关的地址，没有这个问题）。
  - 现在：这种 socket 用 `IP_PKTINFO` 指定源地址，与新命名空间里绑通配地址的 socket 一样。
- **外层的 FORWARD**（`netns::path_filter`）：
  - 原来：按 DNAT 之后的目的地在不在发送方的直连网段里，判断经不经过外层。容器连外层地址上的端口、连外网，被外层 DNAT 到同一个 bridge 上的别的容器时，外层的 FORWARD 不执行。
  - 现在：按发送方路由时的目的地判断。经由网关的、外层的本机地址都经过外层的 FORWARD；发往直连网段里别的容器的是二层转发，照旧不经过。
- **`meta oif`**（`nft.rs`）：按编号匹配出口接口。原来只有 `oifname`：iptables-nft 把 `-o` 翻译成 `oifname`，手写的 nft 规则的 `oif br0` 却从不匹配。`Packet::oif` 改成同时给出编号和名字。

## 测试

- `origdst` 差分测试组的跨命名空间部分加了容器连外面的地址时外层的 REDIRECT、DNAT、`fib daddr type local`、FORWARD 上的 `oif`、劫持 UDP 53 端口时回答的来源（外层开 `ip_forward`），两个执行层下都与原生逐字相同。A 和 B 在同一个网段，原生要 masquerade，B 的回答才经过外层换回来。
- 第一次写的 FORWARD 情形连的是 B 上已经关掉的端口，拒绝没拒绝都是 ECONNREFUSED，变异检验查不出来，改成 B 在那个端口上一直监听着。
- DNAT 到外面的地址：差分测试里没有稳定能连到的外部地址，手动验证。外层把容器的连接 DNAT 到宿主上的 HTTP 代理，现在连得上；改之前是 EINVAL。
- 实测（Tier 1，`target/origdst/ctr-redir.sh`）：外层跑 ssserver 和 sslocal 的 redir 模式、一条 `iifname br0 … redirect` 的规则，容器里的 curl 拿到了外面的 HTTP 服务的回答。
- LTP 8 路并发共用一个仓库，每个 tuxport 里的 pid 都是 2：process_vm_writev01 和同时跑的 process_vm_readv01 在 `/dev/shm` 里的文件撞名，偶尔 EEXIST。

## 推迟项

- 进入外层的只有经由外层网关的 IPv4 连接、数据报，IPv6 的不执行 PREROUTING（与原来一样）。
- 一个没连接的 UDP socket，先发给被 REDIRECT 到外层的地址（这时给它绑上了回环地址），之后再发往外网的地址就出不去。原来经由网关连外层时就是这样。DNS 的解析器发给几个服务器时用同一个 socket，只有其中一部分被劫持时会碰到。 → 先发命名空间里的也在宿主上绑通配地址了；musl 的解析器其实先 bind 了通配地址、端口 0，本来就碰不到，碰到的是不先 bind 就 sendto 的程序（M9 之前：新命名空间里没绑定的 UDP socket 先发命名空间里、再发外面）
- 外层绑通配地址的 UDP socket 只在 `sendto` 带地址时指定源地址。它 connect 到容器之后用 `send` 时，宿主仍选 127.0.0.1：容器看到的来源、它自己 getsockname 报告的本端都是 127.0.0.1，原生是 10.50.0.1（`target/origdst/udpconn2.sh`）。容器回给 127.0.0.1 的数据报落到它自己的回环上，双向不通。TCP 也一样：外层的 socket 先显式绑通配地址（含 `bind(("", 0))`）再连容器，两边看到的都是 127.0.0.1（`target/origdst/order.sh`），连接本身能用。宿主 socket 已经绑在通配地址上、换不了，要改得换一个宿主 socket（按面向容器的地址和同一个端口重新绑，搬过 guest 设的选项）；连接之前先显式绑本端的程序不多，没做。
