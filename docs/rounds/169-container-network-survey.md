# M9 之前：容器网络的普查（2026-11-01）

[podman 和 docker 的普查](157-container-survey.md)的问题 12：容器现在只能用 host 网络。讨论之后先走"按地址映射模拟 bridge"这条路（方案 1）：容器的 socket 仍是宿主 socket，tuxport 里不出现包，容器的地址在系统调用的边界上换成宿主上的回环地址，与[网络命名空间](122-net-namespaces.md)现在的做法一样。动手之前先查清楚 docker 和 podman 的 bridge 网络到底依赖什么，重点是三件事：关掉防火墙之后还剩哪些依赖；自定义网络按容器名解析靠什么；端口发布（`-p`）是怎么实现的。

## 做法

- 两个普查负载：`tests/survey/docker-bridge.sh`（`"iptables": false` 和默认两种配置）、`tests/survey/podman-bridge.sh`（netavark 的 `firewall_driver = "none"` 和 `"nftables"`）。各测：容器里的地址、路由和 resolv.conf；访问 bridge 以外（ping 外层的网关）；容器之间按 IP 互访；从外层的命名空间访问容器的 IP；`-p 8080:80`；自定义网络里按容器名解析和访问。用 strace 记下 dockerd、netavark 发的 netlink 消息、写的 sysctl、执行的程序，nft 列出两层命名空间里的规则。
- 原生对照在特权容器里跑（docker 29.9.2、podman 5.8.9、netavark 1.17.2），tuxport 里用第 157 轮的两个仓库。
- 写脚本时撞到的：
  - Alpine 的 busybox 没有 httpd，服务端改用 `nc -l` 的循环。
  - podman 默认把宿主的代理环境变量带进容器，容器里的 wget 走了代理，要加 `-Y off`。
  - netavark 默认的防火墙驱动是 iptables，原生的镜像里没有，改成显式用 nftables。
  - 原生嵌套时 podman 开不了 cgroup 控制器，这一轮只看网络，两边都配 `cgroups = "disabled"`。
  - `strace -f` 跟着 `podman run -d` 时，它跟踪着后台容器里的进程，命令一直不返回。后台运行的不跟踪，netavark 的调用在前台的 `run`、`network create` 里看得到。
- `scripts/survey` 的原生对照加了 `--init`：busybox 的 timeout 自己执行命令，sh 成了容器里的 1 号进程，1 号进程不理会超时时的 SIGTERM，卡住的负载过了上限也不结束。这次就卡了 18 分钟才发现。

## 结果

### 原生的结果

| | docker，iptables 关 | docker，iptables 开 | podman，防火墙 none | podman，nftables |
|---|---|---|---|---|
| 容器之间按 IP | 通 | 通 | 通 | 通 |
| 外层访问容器的 IP | 通 | 通 | 通 | 通 |
| 访问 bridge 以外 | 不通（没有 NAT） | 通 | 不通 | 通 |
| `-p 8080:80` | 通（docker-proxy） | 通（docker-proxy） | **不通** | 通（DNAT） |
| 自定义网络按名字 | 通 | 通 | 通 | 通 |

- **网络都经由 netlink 搭起来**，两家的做法相同：
  - `RTM_NEWLINK` 建一个 bridge（docker0、podman0），每个容器建一对 veth。netavark 一次建好：`VETH_INFO_PEER` 里带着对端的名字 `eth0`、MTU、容器网络命名空间的 fd（`IFLA_NET_NS_FD`），对端建出来就在容器里，本端带 `IFLA_MASTER` 挂到 bridge 上。
  - `RTM_SETLINK` 启用接口、设 MAC。
  - `RTM_NEWADDR` 给 bridge 配网关地址，setns 进容器的命名空间给 eth0 配地址。
  - `RTM_NEWROUTE` 在容器里加默认路由（经由网关，netavark 的 metric 是 100）。
  - 拆掉时 `RTM_DELLINK`。期间还有大量 `RTM_GETLINK`、`GETADDR`、`GETROUTE` 的查询和转储。
- **写的 sysctl 都是按接口的**：docker 写 bridge 的 `ipv6/conf/<if>/accept_ra`、容器里 eth0 的 `disable_ipv6`；netavark 写 eth0 的 `ipv4/conf/<if>/arp_notify`、bridge 的 `route_localnet`、eth0 的 `ipv6/conf/<if>/autoconf`。
- **docker 的 `-p`**：两种配置都起 docker-proxy（用户态代理），IPv4、IPv6 各一个。监听 socket 由 dockerd 打开、以 fd 交给它（`-use-listen-fd`），docker-proxy 再去连容器的 IP。
- **docker 自定义网络的名字解析（关键）**：resolv.conf 指向 127.0.0.11，dockerd 的内置 DNS 在每个容器的网络命名空间里监听两个随机端口（TCP、UDP）。**即使 `"iptables": false`**，dockerd 也照样用 iptables（Alpine 的是 nftables 后端，经由 `NETLINK_NETFILTER`、`nft_compat` 的 xt 目标）在容器的命名空间里写规则：
  - `nat` 表 `OUTPUT` 链：发往 127.0.0.11 的跳到 `DOCKER_OUTPUT`，再把 53 端口的 TCP、UDP DNAT 到 127.0.0.11 的那两个端口；
  - `POSTROUTING` 链：从 127.0.0.11 那两个端口回来的 SNAT 回源端口 53。
  - 没有这几条规则，compose 这种默认用自定义网络、靠服务名互找的就不能用。
- **docker 开着 iptables 时**：外层还有 filter、raw、nat 的一大批规则，用于隔离、防止从外面直连容器、MASQUERADE；方案 1 不打算执行这些。
- **podman 的名字解析**：aardvark-dns 直接监听自定义网络的网关地址的 53 端口（10.89.0.1:53），容器的 resolv.conf 指向它，不需要 DNAT。
- **podman 的 `-p`**：rootful 的 podman 没有用户态代理，端口发布完全靠防火墙规则。nftables 时 `inet netavark` 表里有 `NETAVARK-HOSTPORT-DNAT`、`SETMARK` 链和隔离、MASQUERADE 的链。驱动是 none 时宿主上只是占着端口（一个监听 socket），连上去没有回应。

### tuxport 里

两家都在第一步失败：dockerd 报 `Failed to create bridge docker0 via netlink: operation not supported` 后退出；netavark 报 `create bridge: Netlink error: Not supported (os error 95)`。tuxport 的 netlink 现在建不了 bridge、veth，新的网络命名空间里只有 lo（[网络命名空间](122-net-namespaces.md)）。

## 推迟项

### 方案 1 要做的

1. **有状态的 netlink 设备**：
   - 建 bridge、veth 对（含 `VETH_INFO_PEER`、经由命名空间的 fd 或 pid 建在别的命名空间里、`IFLA_MASTER`），启用、改名、设 MAC、挂到 bridge、移到别的命名空间，删除。
   - 地址、路由（现在改路由是 EOPNOTSUPP）记在各自的命名空间里，转储、`ip` 的输出照 Linux 的格式。
   - 接口的 sysctl 目录（`ipv4/conf/<if>`、`ipv6/conf/<if>`）跟着接口出现，上面这些项可以写。
   - → 已做（M9 之前：有状态的网卡）：两家的 bridge 网络建得起来，容器起得来；`-p` 时 docker 给监听 socket 装 BPF 过滤器失败（`SO_ATTACH_FILTER` 的指针没换），归到第 2 步
2. **socket 层的地址映射**（→ 已做，M9 之前：经由虚拟网卡的连接；docker 的 `-p` 也通了）：veth、bridge 上的地址各自对应宿主上的一个回环地址。容器之间、外层到容器、容器到网关（aardvark-dns、宿主上的服务）都换成宿主回环上的连接。访问 bridge 以外的直接用宿主 socket，所以即使防火墙关着也通（比原生"关着防火墙"时好）。docker-proxy 从外层连容器的 IP 也走这个映射。
3. **netfilter 的一个子集**（→ 已做，M9 之前：nftables 的子集和 socket 层的 DNAT；docker 自定义网络按名字解析，`"iptables": true` 的 dockerd 和 podman 的 nftables 驱动都能用了，只执行 OUTPUT 上的 nat 链）：
   - 至少要接受 docker 写进容器命名空间的那几条 nat 规则，并在 socket 层执行：发往 127.0.0.11:53 的换成那个端口，回来的源端口换回 53。这要实现 `NETLINK_NETFILTER` 上 nftables 的消息（表、链、规则的增删查），以及 iptables-nft 用的 `nft_compat` xt 目标（DNAT、SNAT）。
   - 别的规则（隔离、MASQUERADE、raw 表）接受下来、能列出来，但不执行。这一点要写进文档：防火墙的隔离不起作用。
4. **podman 的端口发布**（→ 已做，M9 之前：发布的端口：从别的命名空间进来的执行 PREROUTING 的 DNAT，从 tuxport 外面进来的由宿主上的线程转发；占端口的那个 socket 原来在，只是 MPTCP 的没列出来）：只有 nftables 的 hostport DNAT 规则。在方案 1 里，要由 tuxport 在宿主上监听那个端口、把连接转给容器（相当于内核层里的 docker-proxy）。还要处理 podman 自己占着同一个端口的那个监听 socket。这一项可以放到最后，或者先说明 rootful podman 的 `-p` 不支持。

### 推荐的配置

- docker：`"iptables": false`（外层不写那一大批规则；容器命名空间里的 DNS 规则照样会写，需要第 3 项）。
- podman：`firewall_driver = "none"`；要 `-p` 的话等第 4 项，或者用 nftables 驱动加 netfilter 子集。

### 后续的顺序（建议）

1. 有状态的 netlink 设备和接口的 sysctl：docker、podman 的 bridge 网络能建起来，容器能起。
2. socket 层的地址映射：容器之间、外层到容器、访问外面、docker-proxy 的 `-p`、podman 的按名字解析。
3. nftables 的子集和 socket 层的 DNAT/SNAT：docker 自定义网络的按名字解析（compose）。
4. podman 的 `-p`。
