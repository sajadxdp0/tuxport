# M9：宿主让绑的低端口就绑原来的（2026-11-03）

[设置页](190-settings-default-alpine.md)那一轮的推迟项：Android 17（API 37）的 16K 模拟器上 App 自己就能绑 22，`lowPortsMoveToHighPortsOnTheHost` 的前提断言失败，原因没查。第 207 轮之后筛推迟项时查到了原因：Google 从 2026 年 5 月的 Google Play 系统更新起放开了一批 1024 以下的端口，App 没有 `CAP_NET_BIND_SERVICE` 也能绑（[heise](https://heise.de/-11314477)、[Android Authority](https://www.androidauthority.com/p-3672981/) 的报道，来源是 Issue Tracker 里 Google 工程师的回复）：

- TCP：20、21、22、23、80、443、445、515、631（FTP、SSH、Telnet、HTTP、HTTPS、SMB、打印）；
- UDP：319、320（PTP）、443（QUIC）；
- 要 Android 13 以上、内核 5.15 以上。做在 Connectivity 的 Mainline 模块里（eBPF 的 bind 钩子），随 Play 系统更新到达，不用升级系统；Android 17 的预览版先放开了 UDP 319、320。

在两个模拟器上用 `adb shell` 的 `toybox nc -l` 逐个试（shell 用户同样没有 `CAP_NET_BIND_SERVICE`，两边的 `ip_unprivileged_port_start` 都是 1024）：

| | API 34（Pixel 4a，内核 6.1，Connectivity 模块 340818080） | API 37（Qenix 16K，内核 6.12，370399999） |
|---|---|---|
| TCP 20、21、22、23、80、443、445、515、631 | 都绑不了 | 都绑得了 |
| UDP 319、320、443 | 都绑不了 | 都绑得了 |
| TCP 25、53、79、319，UDP 22、53、123 | 绑不了 | 绑不了 |

与那份名单一个不差，190 篇的现象就是它。名单按协议分：22 只放开了 TCP。

[第 156 轮](156-low-ports.md)定的是总是映射：开发虚拟机和手机走同一条路，测得到。有了这个更新的手机上，guest 的 sshd 本来可以就用 22，却被换到 10022：别的设备要连 10022，程序自己打印、配置文件里写的却是 22，用户得看通知。改成宿主让绑原来的端口就绑原来的。

## 做法

- **先试原来的端口**（`core/kernel/src/portmap.rs` 的 `bind`）：宿主回 EACCES（不在放开的名单里、没有这个更新的系统）或者 EADDRINUSE（手机上别的 App 占着）时，再照原来的顺序换：同一个端口另一个协议换到的宿主端口，`+ 10000`、`+ 20000`、`+ 30000`，宿主分配。换过的端口还是 EACCES、别的错误照旧直接返回。
- **名单不写进 App**：每次直接试，系统以后再放开、厂商改了都跟着变，试不成只多一次 bind。
- **绑上原来的端口也登记**，宿主端口就是它自己：同一个端口还有 socket 绑着、刚释放不到 2 分钟时仍先用它，guest 自己的冲突、TIME_WAIT 照 Linux 是 EADDRINUSE，不会把宿主的 EADDRINUSE 当作别的 App 占着、换到 10022 去（第 156 轮的第一版就因为这个被差分测试抓到过）。getsockname、`/proc/net`、连本机的换算原样照用，换出来还是原来的端口。
- **只放开了一个协议的端口**：TCP 22 绑原来的之后，guest 的 UDP 22 先试 22（EACCES），TCP 那边的宿主端口也是 22、已经试过，接着就是 10022。
- **内核日志和通知**：没换端口时不写 "port 22/tcp is 22 on the host"。监听事件照常报告，宿主端口与 guest 端口相同，App 原来就有这种情况的说法（"同一网络里的其他设备连得上它"）；换了端口时的说法原来是"App 用不了 1024 以下的端口"，现在不对了，改成"这台手机上的 App 用不了端口 79"（被占着也是"用不了"）。
- **开发用的宿主**：以 root 运行、在 docker 容器里（`ip_unprivileged_port_start` 是 0）时同样绑原来的端口，`tuxport-run` 里 guest 绑 79 就是宿主的 79。要在这样的宿主上测映射，加了 `MachineOptions::always_map_low_ports`（`tuxport-run --always-map-low-ports`），照旧总是换。差分测试的 `lowports` 组靠它测换算：`scripts/difftest` 认用例里 `# tuxport-options: …` 这一行，加给 tuxport-run，与 `# docker-run-options:` 一样。

## 测试

- `portmap` 的单元测试新增 `the_real_port_when_the_host_allows_it`（宿主让绑时用原来的端口并登记、guest 自己的冲突和 TIME_WAIT、只放开了一个协议、被别的程序占着、总是映射），`tries_the_offsets_then_lets_the_host_choose` 改成宿主对低端口回 EACCES 时依次试的顺序。
- 集成测试 `low_ports_bind_the_real_port_or_elsewhere`（原来的 `low_ports_bind_elsewhere_on_the_host`）两个执行层各跑总是映射和默认两种。默认的先在宿主上试绑那个端口，绑得了（开发容器里）就要求 guest 也在原来的端口上，绑不了（CI 的 runner 不是 root）就要求 10000 + 端口。
- 差分测试 `lowports` 组带 `--always-map-low-ports`，两个执行层照旧与原生逐字相同。手动在 uid 1000、`ip_unprivileged_port_start` 为 1024 的容器里（与 Android 一样绑不了）看了 guest 的 22、79 换到 10022、10079，guest 的 netstat 里仍是 22、79。
- 仪器测试 `lowPortsMoveToHighPortsOnTheHost` 拆成用 79 的 `lowPortsTheHostRefusesMoveToHighPorts` 和用 22 的 `lowPortsTheHostAllowsAreBoundAsTheyAre`（App 自己绑不了就跳过）。Pixel 4a（API 34）上通过，绑 22 的那个因为宿主不让绑而跳过；Qenix 16K（API 37）上都通过，190 篇里在 API 37 上失败的就是这个用例。通知的文字没有在界面上手动看。
- 差分测试 `nsfs` 用 `hostname | grep -c bb` 看外面的主机名有没有被 unshare 里的改掉，原生一侧的主机名是容器的编号，随机的十六进制里正好有 "bb" 时 Tier 0 失败了一次，改成整行比较 `grep -cx`。

## 推迟项

- 只在 API 37 的模拟器上看到放开的名单；真机上 Play 系统更新到了没有、厂商有没有改，要真机（R1）。
- 同一台手机更新前后不一样：更新之前在 10022 上的 sshd，重启之后就在 22 上了，别的设备上写死了 10022 的（`~/.ssh/config`）连不上，要看新的通知。
- 绑原来的端口时 guest 和手机上别的 App 抢同一个端口：别的 App 先占着时 guest 换到 10022（与原来一样）；guest 先占着时别的 App 绑不上，原来两边不冲突。
