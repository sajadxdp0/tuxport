# M9 之前：网络命名空间的 sysctl 补齐、UDP 也查有没有人绑（2026-11-02）

审完 170–176 轮的推迟项之后，挑出值得做的两项，同时补跑一次没覆盖过的配置（podman 的 iptables 驱动）：

- **sysctl 不全**（[第 170 轮](170-netdev.md)的推迟项）：`/proc/sys/net` 下只有 `ip_forward`、`ip_local_port_range` 等几项，Linux 新的网络命名空间里有的 `ipv4/tcp_keepalive_time`、`tcp_fin_timeout`、`tcp_syncookies` 这些都没有。docker、compose 的 `sysctls:`、`docker run --sysctl net.ipv4...` 由 runc 在容器里写这些文件，文件不存在，容器就起不来。
- **UDP 不查有没有人绑**（[第 174 轮](174-namespace-ports.md)的推迟项）：TCP 连到新命名空间里没人监听的端口是 ECONNREFUSED，UDP 发往没人绑的端口却交给宿主，会被外层绑通配地址的 UDP socket 收到。

## 做法

- **sysctl**（`crate::netconf::SYSCTLS`）：
  - 照 Linux 在新的网络命名空间里给出的 `core/`、`ipv4/`、`ipv6/`、`unix/` 下的文件（原生 6.8 取的列表，去掉 6.2 以后才有的 `tcp_plb_*`、`tcp_shrink_window` 这些）。共 140 项，加上只在开机时的命名空间里有的 `core/rmem_max` 等 5 项。权限位也一样：`tcp_ehash_entries` 只读、`tcp_fastopen_key` 只有属主读写。
  - 值按命名空间记，读出照 Linux 的格式：几个整数之间是 tab，拥塞控制算法的列表是空格。
  - 写入照 `proc_dointvec`：整数写几个改前几个，多出来的不看，不是数是 EINVAL；拥塞控制算法要是 reno、cubic（不然 ENOENT）；保留的端口、TCP Fast Open 的密钥按格式查。
  - 写入的值只记下，不起作用：TCP 的行为由宿主决定。
- **UDP 的登记**（`netns::listening`，与[第 174 轮](174-namespace-ports.md)的 TCP 监听同一张表）：
  - 新的网络命名空间里绑了端口的 UDP socket 都登记：显式绑的、发送之前替它绑的、第一次发往外面时由宿主自动绑定的（发送之后补登记）。
  - 发往新命名空间里的数据报，那里没有对得上的 socket（端口、绑的地址、地址族）时悄悄丢掉，与 Linux 一样（Linux 回一个 ICMP 端口不可达，这里不回）。
  - 发给自己的虚拟地址时绑的 127.a.b.254 算作绑在虚拟地址上。ping socket 不算 UDP。
- **顺带修的**（第 171 轮的问题）：新命名空间里先绑通配地址、端口 0 的 UDP socket，第一次 connect 到映射的目的地时，原来先对宿主 socket 做一次 `connect(AF_UNSPEC)` 断开。Linux 断开 UDP 时会放掉内核分的端口，getsockname 的端口因此变了（原生不变），下次发送又分一个新的。现在宿主上真连着才断开，断开之后再绑回原来的端口。是 UDP 的登记暴露出来的：登记的是原来的端口，回答被丢掉了。

## 测试

- 新的差分测试组 `netsysctl`（新的网络命名空间里）：常用的 20 项的权限和默认值，各种写入，另一个命名空间各有一份。不比整个目录的列表：Linux 6.1 和 6.8 的项不一样。
- `nsports` 组加了 UDP 发往没人绑的端口，`netmap` 组加了先绑通配地址再 connect 前后端口不变。两个执行层下都与原生逐字相同。ping socket（也是 SOCK_DGRAM）一开始被当成 UDP 丢掉了，是 `netmap` 的普通用户 ping 查出来的；上面那个端口变了的问题也是 `netmap` 查出来的。单元测试在 `netconf` 和 `netns` 的监听登记里。
- 普查：podman 的普查加了 `firewall_driver = "iptables"`（Alpine 上装了 iptables 时 netavark 默认用它），与原生一致；docker 的普查加了 `--sysctl net.ipv4.tcp_keepalive_time=600` 这类选项，容器里读到的值与原生相同，原来 tuxport 里这样的容器起不来。
