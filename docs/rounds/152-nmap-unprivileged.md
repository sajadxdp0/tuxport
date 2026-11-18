# M9 之前：nmap 以 root 运行（2026-11-01）

第 151 轮之后筛兼容性推迟项时发现的，以前没记过：tuxport 默认以 root 登录，nmap 以 root 运行时一上来就退出。

- nmap 按 `geteuid() == 0` 判断有没有特权（`NmapOps.cc` 的 `NmapOps::Initialize`，环境变量 `NMAP_PRIVILEGED`、`NMAP_UNPRIVILEGED` 优先），有特权时默认做 SYN 扫描，主机发现用 ICMP、ARP，都要 raw socket 或者 AF_PACKET。`UltraScanInfo::Init` 两样都打不开时 `fatal("Couldn't open a raw socket or eth handle.")`。
- Android 上的 App 没有 CAP_NET_RAW，tuxport 对 ICMP 以外的 raw socket 回 EPERM（§10），nmap 不带参数扫什么都是 `QUITTING!`。Termux 不是 root，nmap 自己就用 connect 扫描，所以那边能用。

## 做法

- App 起的会话的环境（`ConsolePty.ENVIRONMENT`）加上 `NMAP_UNPRIVILEGED=1`，已经装好的 rootfs 也生效；首次启动写的 `/etc/profile.d/tuxport.sh` 也导出它，ssh 登录、`su -` 这类重新读 profile 的 shell 里也有（§11.4）。
- 内核层不改：raw socket 本来就做不到。

## 测试

- 普查负载 `tests/survey/nmap.sh`（Alpine 3.24 的 nmap 7.99）：默认扫描、端口扫描、主机发现、服务版本、从读 profile 的 shell 里运行。tuxport 里不设变量时是 raw socket 的 EPERM，设了之后都正常。
- 原生对照：容器里的 root 有 CAP_NET_RAW，加 `SURVEY_DOCKER_OPTS=--cap-drop=NET_RAW` 之后输出与 tuxport 逐字相同，包括默认扫描的失败。
- 没有改内核层，没跑全量测试。

## 推迟项

- `sudo nmap` 仍会失败：sudo 默认 `env_reset`，去掉了这个变量。可以用 `nmap --unprivileged`，或者在 sudoers 里 `env_keep` 它。
- 不读 profile 的场合（`ssh 主机 nmap …` 这类不起登录 shell 的命令、cron）没有这个变量。
- nmap 自带的 nping 等别的要 raw socket 的工具（tcpdump、arping、masscan）照样用不了，与 Android 上的 Termux 一样。
