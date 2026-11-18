# M9 之前：内置的 init 按 inittab 开机、关机（2026-11-02）

支持 OpenRC 的第三轮。前两轮之后 OpenRC 在 tuxport 里能跑，缺的是"谁来跑它"：

- 1 号进程是内核层内置的、不对应任何 guest 程序，不读 inittab。App 冷启动时没有地方执行 `openrc sysinit/boot/default`，只能在 `/etc/profile.d` 里靠标记文件自己起服务（只有开了标签页才会执行）。
- App 退出时 `kill_all` 直接 SIGKILL 所有进程：PostgreSQL、MariaDB 下次要做崩溃恢复，Redis 丢掉上次保存之后的数据。
- busybox 的 `reboot`、`halt`、`poweroff` 不带 `-f` 时给 1 号进程发信号，内置的 init 忽略，命令返回 0、什么也没发生。

设计文档 §6 原来写着"v1 不运行 OpenRC"。

## 做法

内置的 init 不变成 guest 程序（ptrace 它是 ESRCH 等都照旧），而是在宿主要求时执行 `/etc/inittab` 的一个子集（`core/kernel/src/inittab.rs`）。

### 开机：`Machine::boot`

依次运行：

1. `sysinit`；
2. `boot`、`bootwait`；
3. 默认运行级别的 `wait`；
4. 默认运行级别的 `once`。

- 默认运行级别看 `initdefault`，没有时是 2。没写运行级别的项属于所有运行级别。busybox 的格式里第二列总是空的。
- `wait`、`sysinit`、`bootwait` 等它结束，`boot`、`once` 不等。
- 与 busybox init 一样，所有 `wait` 在所有 `once` 之前。
- 返回时 `once` 的命令可能还在运行。

于是：

- Alpine 的 inittab（`::sysinit:/sbin/openrc sysinit` 等）开机运行 OpenRC 的三个运行级别。
- Debian 的 sysvinit-core（`si::sysinit:/etc/init.d/rcS`、`l2:2:wait:/etc/init.d/rc 2`）运行 rcS 和运行级别 2 的脚本。

### 关机：`Machine::shutdown`

照 busybox init 的 `run_shutdown_and_kill_processes`：

1. 开过机的，依次运行关机的命令：busybox 格式的 `shutdown`，以及 sysvinit 格式里写明了运行级别 0（停机、关机）或 6（重启）的 `wait`、`once`。没写运行级别的不算，不然 Alpine 的 `::wait:/sbin/openrc default` 关机时也会跑。
2. 给所有进程发 SIGTERM，最多等 1 秒（都结束了就不等）。
3. 结束剩下的（`kill_all`）。

- 关机的命令一共最多等 `timeout`（tuxport-run 是 30 秒，`SHUTDOWN_TIMEOUT`：OpenRC 停 PostgreSQL 最多等 15 秒），到了就不再等。
- 没有开过机的也是 SIGTERM、等、SIGKILL，交互式的 shell 忽略 SIGTERM，最后被 SIGKILL。
- 开始关机（含 `kill_all`）之后，还在开机的 `boot` 不再运行后面的命令（`Kernel::stopping`）。

### 命令怎样执行

- 每一项由 `/bin/sh -c` 执行：busybox init 遇到 shell 的特殊字符时也这样，前面加 `exec`。这里总是经由 shell，好重定向。
- stdin 是 `/dev/null`。
- 输出写到 `/var/log/boot.log`：tuxport 没有控制台，App 的 stdio 是 `/dev/null`。开机的第一个命令清空它，之后的追加。`/var/log` 不在时丢掉。
- 环境变量与 busybox init 设的相同：`HOME=/`、`PATH=/sbin:/usr/sbin:/bin:/usr/bin`、`SHELL=/bin/sh`、`USER=root`、`TERM=linux`。
- 命令的程序是不存在的绝对路径时跳过。Alpine 的 minirootfs 里就有引用 `/sbin/openrc` 的 inittab；没装 OpenRC 时什么也不运行，日志也不动。
- 没有终端，`respawn`、`askfirst`（tty1–6 上的 getty）不运行：登录 shell 是 App 的标签页。`ctrlaltdel`、`restart`、`powerfail` 等也不运行。

### guest 请求关机

- `boot` 之后，root（能给 1 号进程发信号的）发来的 SIGTERM、SIGUSR1、SIGUSR2 记作重启、停机、关机的请求（busybox init 的处理），叫醒在等的宿主（`wake::WATCHED`）。
- 宿主用 `Machine::take_power` 取走，或者用 `Machine::wait_or_power` 等程序结束、请求关机二者之一。
- 别的信号、没有开机时的这三个照旧忽略，`kill` 返回 0。没有权限的照旧 EPERM。
- `reboot -f` 的 reboot(2) 照旧是 EPERM。

### 别的

- `Machine::processes`：还在运行的 guest 进程数（不算 init 和僵尸进程），下一轮 App 关掉最后一个标签页时据此决定留在后台还是退出。
- `tuxport-run --init`：程序之前开机；程序结束时，或者 guest 请求关机时，按 inittab 关机。请求的是重启也只是关机、退出。

## 测试

- 单元测试（`inittab`）：Alpine 的 inittab 的开机和三种关机，Debian sysvinit-core 的按运行级别取舍，各种动作的先后、命令里的 `:`、空命令、注释。
- rootfs 的集成测试新增一个，在自己写的 inittab 上测 `tuxport-run --init` 开机、关机时各种动作运行与否、跳过没装的程序、环境变量、guest 的 `poweroff`、不带 `--init` 时 `poweroff` 什么也不做，以及 `Machine` 的接口（开机之前 `kill -TERM 1` 不算请求、`processes`、取走关机请求、`shutdown`）。
- 手动（OpenRC 的仓库，`target/r186/`）：`tuxport-run --init` 开机之后服务都起来了，程序结束或 guest 里 `poweroff` 之后服务被正常停止，开机加关机一共 1 秒。
