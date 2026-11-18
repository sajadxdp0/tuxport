# M9 之前：有状态的网卡：bridge、veth、地址和路由（2026-11-01）

[容器网络的普查](169-container-network-survey.md)定下的方案 1 的第一步：docker、podman 搭 bridge 网络用的 netlink 请求原来都是 EOPNOTSUPP（dockerd 报 `Failed to create bridge docker0 via netlink: operation not supported` 退出，netavark 报 `create bridge: Netlink error: Not supported`），新的网络命名空间里也只有 lo。这一轮照 Linux 6.1 记下虚拟网卡的状态：bridge、veth、dummy 的建删改，地址，路由，接口的 sysctl。做完之后两家的 bridge 网络建得起来，容器起得来，容器里 `ip addr`、`ip route` 看到的与原生相同。数据包仍然不经过这些设备：容器之间、容器与外面的流量要下一步在 socket 层按地址映射。

## 做法

动手之前在原生的特权容器里用 strace 记下 dockerd、netavark 发的每条 netlink 请求（容器命名空间里的那些 strace 不解码，另写了个小脚本按 ifinfomsg、ifaddrmsg、rtmsg 和属性解出来），差分测试的用例也先在原生上跑出样子。

- **设备表**（新的 `core/kernel/src/netdev.rs`）：每个网络命名空间（开机时的那个也是）有一张表，记着虚拟设备、经由它们的路由和 sysctl。
  - 开机时的命名空间里虚拟设备叠在宿主的接口之上：编号从宿主最大的编号往后分配（docker 的容器里 lo、eth0 之后是 3，与原生相同），名字不能与宿主的重复；宿主的接口照旧改不了（[第 158 轮](158-container-small-fixes.md)的规矩）。新的命名空间里与 lo 并列，编号从 2 起。
  - veth 的两端各在自己的命名空间里，经由弱引用找到对端；删掉一端另一端随之删除。命名空间没人用了，它的设备表随之释放，别的命名空间里对端在那边的 veth 在下一次查表时清掉（Linux 的 `default_device_exit_batch` 也删 veth 连同对端）。
- **netlink 的修改请求**（`netdev::request`，原来 `RTM_NEWLINK` 之外的都直接 EOPNOTSUPP）：
  - `RTM_NEWLINK` 新建（`IFLA_LINKINFO` 的种类 bridge、veth、dummy，别的是 EOPNOTSUPP "Unknown device type"）：名字可以含 `%d`（没给时是 `bridge%d` 这样），编号可以指定。veth 的对端照 `veth_newlink` 先登记：`VETH_INFO_PEER` 里是对端的 ifinfomsg 和属性，带 `IFLA_NET_NS_FD`、`IFLA_NET_NS_PID` 时直接建在那个命名空间里（netavark、docker 29 都这样建容器里的 eth0），没给时在发请求的这个里。之后按 ifinfomsg 改标志、挂到 `IFLA_MASTER` 上，挂不上就删掉新建的。
  - 改已有的（`RTM_SETLINK`，或者 `RTM_NEWLINK` 找到了）照 `do_setlink` 的顺序：移到别的命名空间（先停用、摘下，地址和路由都去掉，sysctl 按那边的 `default` 重来，编号在那边被占了就重新分配，名字被占了用 `IFLA_IFNAME` 改名、没给是 EEXIST；bridge 不能移，EINVAL）、MAC（要合法的单播地址，EADDRNOTAVAIL）、MTU（种类的上下限，"mtu less than device minimum" 等）、组、改名（按编号找到时；6.1 里启用着的不能改，EBUSY）、别名、标志（`rtnl_dev_combine_flags`）、master、发送队列长度。前面改了的在后面出错时也留着，与 Linux 一样。
  - 挂到 bridge 上（`do_set_master`、`br_add_if`）：bridge 不能挂到 bridge 上（ELOOP），lo 是 EINVAL，挂到不是 bridge 的接口上是 EOPNOTSUPP。bridge 的 MTU、MAC 没有由用户设过时跟着端口（端口里最小的；去掉最后一个端口时 MAC 变成全 0，与 `br_stp_recalculate_bridge_id` 一样，差分测试里原生也是）。
  - `RTM_DELLINK`：lo、宿主的接口是 EOPNOTSUPP。
- **载波和状态**照 6.1：veth 两端都启用了才有载波；bridge 有一个转发状态的端口（端口启用、有载波，STP 关着马上转发）才有，但在按端口算过状态之前（端口增删、改 MAC 时算，带着 MAC 建的 bridge 建好就算一次）载波是建出来时的"有"、状态一直是 UNKNOWN；dummy 总有载波，状态总是 UNKNOWN。启用了、没有载波的 veth 是 LOWERLAYERDOWN（`default_operstate` 看的是 iflink 是不是自己）。qdisc 在启用过之后是 noqueue，没启用过是 noop。
- **地址**：IPv4 照 `__inet_insert_ifa`：同一网段、同一地址是 EEXIST（`NLM_F_REPLACE` 时改），同一网段的第二个是 secondary，primary 排在 secondary 前面；删掉 primary 时同一网段的 secondary 一起删掉。IPv6 的地址按范围排（global 在 link 前面，同一范围新加的在前），接口关了 IPv6 时 EACCES。
- **IPv6 的自动配置**：接口启用并有载波、没有关掉 IPv6 时照 addrconf 配置：按 MAC 生成链路本地地址（EUI-64，`addr_gen_mode` 为 1 时不生成，`IFA_PROTO` 是 kernel_ll），加 fe80::/64 和 ff00::/8 的路由。没有 DAD，地址马上可用（原生上要等一会儿才不是 tentative）。接口停用时 IPv6 的地址和路由都去掉（`keep_addr_on_down` 为 0）。
- **路由**：按地址加的照 `fib_add_ifaddr`：本地路由总加（secondary 的首选源地址是 primary），显式的广播地址、网段、网段的广播在接口启用时加；接口停用时去掉经由它的路由，本地路由留着（`fib_sync_down_dev` 不动 scope 为 nowhere 的下一跳），再启用只补回按地址加的。`RTM_NEWROUTE`、`RTM_DELROUTE`：只有单播路由，下一跳要是虚拟设备、启用着（"Device for nexthop is not up"），网关要在这个命名空间里某条范围不小于 link 的路由的网段里（"Nexthop has invalid gateway"），目标的主机位不是 0 是 EINVAL，首选源地址要是本机地址；`NLM_F_EXCL`、`NLM_F_REPLACE`、`NLM_F_APPEND` 照 `fib_table_insert`。没有载波的接口上的路由带 linkdown（本地路由不带）。转储时与宿主的（或者 lo 的）路由合在一起，按表的编号和 Linux 转储的顺序排，同样的按加入的先后。
- **`ip route get`**：主表里最长前缀匹配到虚拟设备上的路由，或者不在开机时的命名空间里时，按这个命名空间的表选出口和源地址（首选源地址，没有时取出口接口上同一网段的地址，`inet_select_addr`）；开机时的命名空间里匹配到宿主的路由时照旧由宿主选。本机地址的源地址改成本地路由的首选源地址（secondary 是 primary 的）。
- **转储**：`RTM_GETLINK` 的回答里虚拟设备带 `IFLA_LINKINFO`（种类；bridge 的参数是默认值；端口带 `IFLA_INFO_SLAVE_KIND` bridge 和端口状态）、`IFLA_LINK`（veth 的对端）、`IFLA_MASTER`、`IFLA_MIN_MTU`、`IFLA_MAX_MTU`、`IFLA_CARRIER_CHANGES` 等；对端在别的命名空间里时带 `IFLA_LINK_NETNSID`，这个编号与 Linux 的 `peernet2id_alloc` 一样转储时分配，`RTM_GETNSID` 回答它（原来总是 -1，[第 129 轮](129-namespace-files.md)）。按 `IFLA_MASTER`、种类过滤的转储（netavark 用它看 bridge 上还有没有端口）带 `NLM_F_DUMP_FILTERED`。接口多时转储分成几个数据报（每个不超过 8 KiB）。
- **extack**：socket 设了 `NETLINK_EXT_ACK` 时出错的确认附上消息（iproute2 打印成 `Error: ….`），只给 6.1 里就有的那几条；`NETLINK_CAP_ACK` 时出错的确认只附上消息头。
- **ioctl**：虚拟设备的 `SIOCGIFINDEX`、`SIOCGIFNAME`、`SIOCGIFFLAGS`、`SIOCSIFFLAGS`、`SIOCGIFMTU`、`SIOCSIFMTU`、`SIOCSIFNAME`、`SIOCGIFHWADDR`、`SIOCSIFHWADDR`、`SIOCGIFTXQLEN`、`SIOCSIFTXQLEN`、`SIOCGIFADDR` 等（busybox 的 `ip link set` 用的就是这些），开机时的命名空间里按名字认出来的不交给宿主；`SIOCGIFCONF` 接在宿主的后面。
- **`/proc/<pid>/net`**、**`/sys/class/net`**：接口、路由、`if_inet6` 都含虚拟设备。`/sys/class/net` 改成按读者所在的网络命名空间（Linux 按挂载 sysfs 的进程的；容器里 runc 在新的命名空间里挂 sysfs，看到的一样）。虚拟设备的 `iflink` 是 veth 的对端，`operstate` 可以是 lowerlayerdown；bridge 有 `bridge/`（`stp_state` 等）和 `brif/<端口>`（指向 `../../<端口>/brport` 的链接），端口有 `brport/state`。dockerd 起容器时读 `bridge/stp_state`、端口的 `state`，等端口进入转发状态；原来读不到，每个容器打两条警告、多等一会儿。
- **`/proc/sys/net`**（新的 `core/kernel/src/netconf.rs`，原来整个目录都没有）：按读者所在的网络命名空间（与 Linux 一样）。
  - `ipv4/conf/`、`ipv6/conf/` 下是 `all`、`default` 和每个接口（虚拟设备的、lo 的、宿主接口的），项和默认值照 6.1 的 `devinet_sysctl_table`（33 项）、`addrconf_sysctl`（58 项）。新接口照抄 `default`；新的命名空间里 IPv4 照抄开机时的那个现在的值、IPv6 是默认值（`devconf_inherit_init_net` 为 0）。
  - 起作用的：`disable_ipv6`（去掉、恢复接口的 IPv6；`all` 上写改到每个接口）、`forwarding`（`all` 上写改到每个接口，`ip_forward` 是它的别名）、IPv4 的 `default`（改到还没单独设过这一项的接口）、IPv6 的 `mtu`（不能超过接口的 MTU）、`stable_secret`（没设过时读是 EIO）。别的只记下：tuxport 不转发、不收发 ARP 和路由通告。
  - `ipv4/ip_unprivileged_port_start`：bind 低于它的端口要 `CAP_NET_BIND_SERVICE`（原来固定是 1024；docker 给容器设成 0）。`ipv4/ip_local_port_range`、`ipv4/ping_group_range`（podman 默认给容器设，crun 打不开就起不了容器）、`core/somaxconn` 只记下：端口由宿主分配，ping socket 交给宿主（Android 允许所有组）。
  - 写入照 `proc_dointvec`、各项的范围（EINVAL）。

## 测试

- 新的差分测试组 `netdev`（原生一侧是特权容器，要写 `/proc/sys`）：新的和开机时的命名空间里建 bridge、veth、dummy 之后的状态、地址、路由、`ip route get` 和 sysctl，接口停用、启用之后的变化，改名、MTU、MAC 和各种错误，veth 移到别的命名空间和删除。两个执行层下都与原生逐字相同。链路本地地址的本地路由按随机的地址排，挪到最后排好再比。原生上 IPv6 地址在 DAD 完成前是 tentative（即使 `accept_dad` 是 0 也要等最多 1 秒），bridge 经由 linkwatch 最多晚 1 秒才知道端口的载波变了，这两处等一下再看。单元测试（`netdev`、`netconf`）测 veth 两端、bridge、地址和路由、sysctl。
- 普查（[第 169 轮](169-container-network-survey.md)的两个负载，与原生对照）：
  - docker（`"iptables": false`）：docker0 和容器里的地址、路由、resolv.conf 都与原生逐行相同。容器之间、外层到容器不通（下一步）；`-p` 的容器起不来：dockerd 给监听 socket 装 BPF 过滤器（`SO_ATTACH_FILTER`），结构里的指针没有换，EFAULT；自定义网络按名字解析、iptables 开着时都要 `NETLINK_NETFILTER`（第 3 步）。 → 都通了（M9 之前：nftables 的子集和 socket 层的 DNAT）dockerd 的警告里还有发免费 ARP 要 AF_PACKET、conntrack 和 IPsec 的模块（`NETLINK_NETFILTER`、`NETLINK_XFRM`）。
  - podman（`firewall_driver = "none"`）：容器里的地址、路由、resolv.conf 与原生逐行相同；起初 crun 打不开 `/proc/sys/net/ipv4/ping_group_range`，补上之后才起得来。自定义网络的 aardvark-dns 绑不上网关地址（EADDRNOTAVAIL，下一步），nftables 驱动和 `-p` 分别在第 3、4 步。

## 推迟项

- 数据包不经过这些设备：容器之间、外层到容器、容器到网关的连接都还到不了（"Network unreachable"），容器里的程序只能用 lo 和宿主的网络（下一步：socket 层的地址映射）。 → 在 socket 层按地址映射连通了（M9 之前：经由虚拟网卡的连接）；数据包仍不经过它们
- 没有 DAD，IPv6 地址加上就可用；没有路由通告、`autoconf`、临时地址。
- 只有 bridge、veth、dummy 三种；vlan、macvlan、vxlan、tun 等是 EOPNOTSUPP "Unknown device type"。没有 AF_PACKET（dockerd 发免费 ARP 时只打一条警告）。
- 只有单播路由，表的编号不大于 255；没有多路径、`onlink`、blackhole 这些；没有自定义规则（`ip rule`）。开机时的命名空间里只能经由虚拟设备加路由，删宿主的路由仍是 EOPNOTSUPP。
- 新命名空间里的 lo 不能改名（Linux 停用着时可以）；6.1 里启用着的接口不能改名（6.2 起可以）；`promote_secondaries`、`keep_addr_on_down` 只记下（前者不提升，后者关接口时仍去掉地址）。
- bridge 的参数（STP、VLAN 过滤、组播）只有默认值，`IFLA_INFO_DATA` 接受但不起作用；bridge 的 FDB、端口的参数没有。
- `/sys/class/net` 按读者的网络命名空间，不是挂载 sysfs 的进程的；`/proc/sys/net` 下只有上面这些，`ipv4/` 下别的（`tcp_*` 等）、`netfilter/`、`unix/` 都没有。 → `core/`、`ipv4/`、`ipv6/`、`unix/` 下的补齐了（只记下值，M9 之前：网络命名空间的 sysctl 补齐）；`netfilter/` 仍没有；net 的 sysctl 文件属于 root，不是命名空间属主里的 root（用户命名空间里的 root 写不了）。
- `ping_group_range` 只记下，不限制建 ping socket。
- `RTM_NEWNSID` 仍是 EOPNOTSUPP。
