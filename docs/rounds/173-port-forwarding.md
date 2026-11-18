# M9 之前：发布的端口：PREROUTING 的 DNAT 和宿主上的转发（2026-11-01）

[容器网络的普查](169-container-network-survey.md)定下的方案 1 的第四步，也是最后一步。[上一轮](172-nftables.md)之后，podman 用 nftables 驱动时，`-p 8080:80` 只在 tuxport 里面经由 127.0.0.1、外层地址连得上：那是 OUTPUT 钩子上的 DNAT。别的情形还不行：

- 从 tuxport 外面连（手机上的浏览器开 localhost:8080、别的设备），连接到的是 podman 占着端口、从不 accept 的那个监听 socket，没有回应。
- 容器经由网关、外层地址连发布的端口，原生经过外层命名空间的 PREROUTING，tuxport 不执行它。
- 普查里 podman 占着端口的那个监听 socket 不在 `netstat` 里。

## 做法

- **MPTCP 的 socket**：podman 是 Go 写的。Go 1.24 起监听默认用 MPTCP（`socket(AF_INET, SOCK_STREAM, IPPROTO_MPTCP)`），宿主支持就用。podman 占端口的 socket 其实在 conmon 手上（fd 5），只是 tuxport 的 `/proc/net/tcp` 只列协议号是 TCP 的。现在 MPTCP 的也列：原生内核也列，它的第一个子流就是 TCP socket。低端口映射里 MPTCP 的端口也按 TCP 登记：原来按协议号 262 登记，Go 写的服务监听低端口时，别人连不上它。
- **从别的命名空间进来的**（`route`，`core/kernel/src/syscall/net.rs`）：connect、sendto 先过自己命名空间的 OUTPUT nat，再换成宿主上的地址。目的地在别的命名空间时，还要过那里的 PREROUTING nat（`netns::prerouting_nat`），DNAT 之后按那个命名空间换成宿主上的地址。"目的地在别的命名空间"有两种：
  - 目的地是那个命名空间的虚拟地址，比如容器连网关 10.88.0.1。
  - 新命名空间里经由宿主网络出去，目的地却是开机时的命名空间的本机地址，比如容器连外层地址（宿主的地址），这时进入的是开机时的命名空间。
  - 合成的包：源地址是发送方面向目的地选的地址，入口接口是目的地命名空间里面向这个源地址的接口（`iifname "br0"` 这类匹配用得上）。
  - redirect：在 PREROUTING 上换成入口接口的地址，在 OUTPUT 上换成回环地址（`nf_nat_redirect_ipv4`）。
  - meta：PREROUTING 上没有出口接口，`skuid`、`skgid` 取不到。
  - 收到的数据报的来源照上一轮换回 guest 给的地址：发送方看到的实际目的地是按发送方换算的目的地命名空间里的那个地址。
- **从 tuxport 外面进来的**（`core/kernel/src/forward.rs`）：
  - 哪些 socket 要转发：开机时的命名空间里监听着的 TCP socket、绑了端口的 UDP socket。对每个这样的 socket，假设一个从外面来的包，在 PREROUTING 上跑一遍：源地址 192.0.2.1，目的地是它绑的地址（通配地址换成宿主上第一个不是回环的 IPv4 地址），入口接口是那个地址所在的接口。被 DNAT 了的，宿主 fd 复制一份交给转发线程。
  - 转发线程：只碰宿主 fd，不拿 BKL。它接受连接，先绑开机时的命名空间面向目的地的那个地址（容器看到的来源是网关，与 docker-proxy 一样），再连到 DNAT 的目的地在宿主上的地址（低端口换成映射的），在两边之间双向转发，一边读完了就对另一边半关。UDP 按来源各开一个宿主 socket 转发，回答从原来那个 socket 发回来源，闲置两分钟就关掉。
  - 什么时候重新判断：guest listen、绑定 UDP 端口时；开机时的命名空间里的 nftables 规则集提交了改动时；别的命名空间新映射了低端口时（容器里的服务后起，探测时它的端口还没有映射）。socket 关掉、规则不再 DNAT 了，就不再转发；内核没了，线程关掉手上的 fd 退出。
  - podman 的端口这样就从外面连得通了。docker 开着 iptables 时，docker-proxy 的监听也会被转发（docker 在 PREROUTING 里同样有 DNAT），效果相同；关着时没有规则，仍由 docker-proxy 转发。

## 测试

- 新的差分测试组 `portfwd`（原生一侧是特权容器，数据包真的经过 bridge 和 nat）：A、B 两个命名空间各挂在一个 bridge 上，外层命名空间里有 PREROUTING 的规则，还像 podman 那样占着 8080、从不 accept；测经由网关地址和外层地址的 TCP、UDP 端口转发和两边看到的地址、`iifname`、redirect、外层自己连只过 OUTPUT。两个执行层下都与原生逐字相同。A、B 在同一个网段时，原生上 B 的回答不经过网关、NAT 换不回来（netavark 为此做 masquerade），所以用两个网段。单元测试（`forward`）测 TCP、UDP 的转发。
- 从 tuxport 外面连没法做成差分测试（原生一侧没有"tuxport 外面"），手动验证：开发容器里后台起 tuxport，里面的 podman 起 `-p 8080:80` 的容器，从开发容器里连 127.0.0.1:8080 拿到了页面。
- 普查：podman、docker 各种配置下都与原生一致（podman 占着 8080 的监听也有了），不同的只剩容器里 ping 外层的网关（推迟项）。

## 推迟项

- 只执行 PREROUTING 和 OUTPUT 上的 nat，POSTROUTING 不执行。所以：
  - 容器经由网关连回同一个网段里发布的端口（hairpin），原生靠 masquerade，容器看到的来源是网关；这里看到的是对方容器的地址。
  - 同一个网段里原生连不上的情形（没有 masquerade 时回答不经过网关），这里连得上。
- 从外面来的连接，转不转发、转到哪里，按探测的包定，不按每个连接真实的来源：只看来源地址、入口接口的规则可能判断得不一样。转发线程接管了监听 socket 之后，guest 自己 accept 时与它抢连接：podman 的那个从不 accept；docker-proxy 抢到的同样转给容器。只转发 IPv4 的监听。
- 从外面转发进来的连接，容器看到的来源是网关（与 docker-proxy 一样），原生 DNAT 时是外面客户端的真实地址。 → 外面的设备（IPv4、TCP）来的报告真实的地址了（M9 之前：从外面转发进来的连接带上真实的客户端地址）
- 开机时的命名空间里监听通配地址的 socket，在宿主上也盖住了别的命名空间的回环地址：别的命名空间里这个端口没有监听时，连它的地址会连到这个 socket 上，原生是 ECONNREFUSED。 → TCP 按命名空间里登记的监听判断，修了（M9 之前：各个网络命名空间的端口互不相干）
- 从外面来的 UDP，每个来源一个转发 socket，闲置两分钟才关；没有连接跟踪那样的超时和计数。
