# M9 之前：没有实现的 netlink 协议建不了（2026-10-28）

补测[终端的访问时间和修改时间](79-pty-times.md)那一轮的推迟项（在 sshd 的登录会话里看 w 的空闲时间）时发现：Alpine 的 musl 不写 utmp，w、who 什么也不列，换到 Debian 的仓库（[glibc 发行版的探测](69-glibc-probe.md)那一轮建的）里跑，sshd 根本登录不进去，日志是 "Access denied for user root by PAM account configuration"，原生的 `debian:trixie` 上同样的配置能登录。

## 原因

Debian 的 sshd 开着 PAM，libpam 编进了 libaudit，每个阶段都往内核发一条审计消息：`audit_open` 建一个 `NETLINK_AUDIT` 的 socket，发出消息之后等内核的 ACK，每次等半秒、最多重试 80 次。tuxport 对 `NETLINK_ROUTE` 以外的协议照样建出 socket，收到请求却从不回答，40 秒之后得到 EAGAIN，PAM 的 account 阶段以系统错误结束（直接调 `pam_acct_mgmt` 得到 "System error"，PAM 写的 syslog 是 "audit_log_acct_message() failed: Resource temporarily unavailable"）。Linux 上没有编进审计时，`socket(AF_NETLINK, …, NETLINK_AUDIT)` 返回 EPROTONOSUPPORT，libpam 把它当作没有审计，直接往下走。Debian、Ubuntu 上所有走 PAM 的登录（sshd、su、login）在 tuxport 上都不能用；Alpine 的 sshd 和 busybox 的 su 不用 PAM，[负载普查](62-workload-survey.md)没有碰到。

## 做法

netlink 只建得了 `NETLINK_ROUTE`（在进程内回答查询）和 `NETLINK_KOBJECT_UEVENT`（mdev、udev 听热插拔事件，这里不会有，与原来一样什么也收不到）。别的协议与 Linux 上没有编进内核的子系统一样，`socket` 返回 EPROTONOSUPPORT，免得一问一答的程序等一个不会来的回答。

## 结果

- Debian 的仓库里 sshd 能登录了，会话里 `w -h` 报告的空闲时间与原生相同（`who -u` 在原生上也不列这个会话，不比）。
- su 原来等 42 秒之后 "su: System error"，现在立刻成功，Ubuntu 24.04 的仓库里也一样。
- iproute2 的 `ss -tln`（用 `NETLINK_SOCK_DIAG` 列 socket）原来一直等回答；现在 socket 建不了，它退回读 `/proc/net/tcp`。

## 测试

- guest 测试程序 `net.c` 加了各个 netlink 协议的 socket 建不建得了。原生的内核编进了审计，差分测试比不了。
- 普查新增 `tests/survey/glibc/debian-sshd.sh`（Debian 上的 sshd 登录和 w 的空闲时间）。

## 推迟项

- `NETLINK_GENERIC` 在 Linux 上总是有的，用它的程序（iw、ethtool 的 netlink 接口、wireguard 的工具）可能不预期 EPROTONOSUPPORT；原来是一直等，现在至少会报错。要的话可以只回答 `CTRL_CMD_GETFAMILY`：没有这个族（ENOENT）。→ 已补上（M9 之前：NETLINK_GENERIC 只有 nlctrl）：建得了，有 nlctrl 一个族，查别的族是 ENOENT，与 Linux 6.8 逐字节相同
