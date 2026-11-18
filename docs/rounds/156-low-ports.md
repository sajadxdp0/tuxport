# M9 之前：低端口映射和监听通知（2026-11-01）

第 151 轮之后筛兼容性推迟项时找出来的两件之一（设计文档 §10 的"之后再加可配置的端口映射"、§12 #7 的"未实现"）：tuxport 默认以 root 登录，guest 里的 sshd（Debian、Ubuntu 默认 22）、nginx、caddy（80、443）、dnsmasq（53）都要绑 1024 以下的端口；guest 的权限检查通过，交给宿主之后，Android 上的 App 没有 `CAP_NET_BIND_SERVICE`，得到 EACCES，这些服务按默认配置起不来。普查的 sshd 一直用 `-p 2222`，开发环境的宿主进程又能绑低端口，所以从来没碰到。Termux 不是 root，用户早就习惯了 8022。

和用户讨论定的做法：guest 以为自己绑的是 22，宿主上实际绑另一个端口；偏移 10000，总是映射（开发虚拟机和手机走同一条路，测得到）；被占了就换，不要逐个加一（会占掉相邻端口的位置）；App 在第一次看到非回环地址上的监听时发通知，告诉用户别的设备连哪个端口。

## 做法

- **登记表**（`core/kernel/src/portmap.rs`，整个内核一份）：按（网络命名空间、协议、guest 端口）记宿主端口和绑着它的 socket 数，另记宿主端口到 guest 端口的反查。socket 持有租约，关掉时归还。
- **bind**：TCP、UDP（不是 raw socket）的 1–1023 端口、偏移不为 0 时，宿主 socket 依次试 `端口 + 偏移`、`+ 2 × 偏移`、`+ 3 × 偏移`（不超过 65535）、宿主分配的端口，前三个被占（EADDRINUSE）才往下试，别的错误照常返回。后三位始终是原来的端口（22 是 10022、20022、30022），一眼认得出来，也不会占掉别的端口的位置。
  - 同一个端口还有 socket 绑着（`SO_REUSEPORT`、IPv4 和 IPv6 分开绑、不同的地址）时只用原来的宿主端口，冲突是 guest 自己的，照 Linux 回 EADDRINUSE。
  - 全部关掉之后 2 分钟内再绑仍先用原来的宿主端口、绑不上就回 EADDRINUSE：TCP 连接关掉之后宿主端口处在 TIME_WAIT，Linux 上不带 `SO_REUSEADDR` 再绑也是 EADDRINUSE。第一版没有这一条，差分测试比出来 tuxport 换到了 20081，原生是 EADDRINUSE。
  - 同一个端口的 TCP 和 UDP 先试对方的宿主端口（DNS 的 53 两个协议一起用）。
  - 新建的映射写一条内核日志：`tuxport: port 22/tcp is 10022 on the host`（`dmesg` 看得到）。
- **guest 一侧看不出来**：getsockname 报告 socket 记着的 guest 端口（accept 出来的连接继承监听的那个，监听关掉之后也是）；connect、sendto 发往本机（回环、通配地址、宿主接口上的地址）上 guest 绑着的低端口时换成宿主端口，guest 里没有程序绑着时不换（手机上别的 App 恰好占着那个宿主端口时，连过去就错了）；getpeername 报告 connect 时记下的；accept 到的对端、recvfrom 的来源、`/proc/net/{tcp,udp}` 的两列按反查换回来（用低端口当源端口连过来的也是）。新的网络命名空间里先照第 122 轮换地址，再换端口，登记表按命名空间分开。
- **选项**：`MachineOptions::low_port_offset`（None 是 10000，0 表示不映射，绑低端口照旧交给宿主）、`tuxport-run --low-port-offset`。
- **监听通知**（§12 #7）：listen 时本端不是回环地址的，按（guest 端口, 宿主端口）第一次出现时把（进程名、端口、宿主端口）排进事件队列（最多 32 个，tuxport-run 不取）。`Machine::take_listens` 取走它们，JNI 的 `Native.takeListens` 交给 App；`TerminalService` 在内核启动之后每 2 秒在 worker 线程上取一次，在新的"网络服务"渠道里发通知：「sshd 在端口 22 上监听」「其他设备请连接端口 10022：App 用不了 1024 以下的端口，tuxport 把它换到了这里」，没有映射的端口是「同一网络里的其他设备连得上它」。每个端口一条，点开回到 App。

## 测试

- `portmap` 的单元测试：依次试几个偏移和让宿主分配、租约的共用和归还、刚释放的再绑、命名空间和协议各自独立、监听事件只报一次。
- 新的差分测试组 `lowports`：低端口上 TCP、IPv6 双栈、UDP 的监听和连接，几方看到的 getsockname、getpeername、recvfrom 的来源和 `/proc/net` 里的行，用低端口当源端口，EADDRINUSE、ECONNREFUSED 和 TIME_WAIT 里再绑，两个执行层下都与原生（容器里的 root 直接绑这些端口）逐字相同。原生容器里普通用户也绑得了低端口（docker 把 `ip_unprivileged_port_start` 设成 0），这一项不比；还没 accept 的连接 tuxport 的 `/proc/net` 本来就不列，只数监听的行。CI 的 Linux 宿主分支上 tuxport-run 以普通用户运行、宿主本来就绑不了低端口，原来这一组全是 EACCES，现在在 colima 里按这个分支跑也通过。
- `rootfs_programs` 的 `low_ports_bind_elsewhere_on_the_host`：宿主上先占住偏移之后的端口，guest 的 nc 换到下一个，`Machine::take_listens` 报告它，宿主经由它连得上。
- Android 模拟器（Pixel 4a，API 34）上 `MachineTest.lowPortsMoveToHighPortsOnTheHost` 通过；在 App 里手工起 `nc -lk -p 22`，`dmesg` 有映射的那一行，通知栏出现监听的通知。

## 推迟项

- 别的设备、手机上别的 App 要连宿主上的端口（10022）；程序自己打印的、写进配置文件的仍是 22（"Listening on :22"），用户要看通知。→ 宿主让绑原来的端口时（2026 年 5 月起的 Play 系统更新放开了 22、80、443 等几个）就绑原来的，不换（[M9：宿主让绑的低端口就绑原来的](208-low-ports-direct.md)）；其余的端口、没有这个更新的系统上仍是这样
- 用低端口当**源**端口的客户端（rsh、NFS 的 secure 端口、`bindresvport`）：对方看到的是 10xxx，检查源端口的服务会拒绝。
- guest 绑了 22（宿主 10022）之后再直接绑 10022 是 EADDRINUSE，Linux 上两个都能绑；手机上别的 App 占着 10022 时 guest 绑 22 换到 20022，不再是"冲突"。→ 宿主让绑原来的端口时不换，没有这个问题（[M9：宿主让绑的低端口就绑原来的](208-low-ports-direct.md)）；换了的端口仍是这样
- DNS 存根占着 127.0.0.53:53：guest 自己的 DNS 服务收不到发往这个地址的查询（与 Linux 上 systemd-resolved 占着它一样）。
- 监听通知只看 TCP 的 listen，UDP 的服务（DNS、mDNS、WireGuard）不通知；同一个（端口, 宿主端口）每次开机只通知一次。
- raw ICMP 汇集点拼回的 ICMP 错误里引用的 UDP 头的端口是宿主上的（traceroute 到本机映射了的端口时看得到）。
- 只在模拟器上验证了 App 绑不了低端口、映射之后别的进程连得上；真机上别的设备经由 Wi-Fi 连 10022 没试（R1）。
