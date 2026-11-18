# M9 之前：App 按 inittab 开机、关机，没有标签页时留在后台（2026-11-02）

支持 OpenRC 的第四轮，也是最后一轮：把上一轮的 `Machine::boot`、`shutdown`、`take_power` 接到 App 上。原来 App 这边有三个问题：

- 冷启动时没有地方起服务。
- 关掉最后一个标签页时整个 App 退出（`MainActivity.onSessionsChanged` 停止服务），后台的服务一起没了。设计文档 §11.2 写的是"还有终端会话或 guest 进程就保持前台服务"，两者不一致。
- 退出时直接 SIGKILL 所有进程。

## 做法

### JNI（`hosts/android/src/lib.rs`、`Native.kt`）

| 函数 | 作用 |
|---|---|
| `boot` | `Machine::boot` |
| `shutdown` | `Machine::shutdown(PowerOff, 30 秒)`，然后从表里删掉句柄 |
| `takePower` | 取走关机请求：0 没有，1 重启，2 停机，3 关机 |
| `processes` | 还在运行的 guest 进程数 |

`stop`（直接结束所有进程）留给重置 rootfs 和测试用。

### TerminalService

- **开机**：内核启动之后，在单独的线程（`tuxport-boot`）上 `Native.boot`。OpenRC 起服务要一会儿，不占 `worker`，也不挡标签页。第 184 轮之后 `/run`、`/dev` 已经挂好，OpenRC 不会盖住标签页正在用的东西，标签页不必等开机。
- **网络的配置**：启动内核之前，没有 `/etc/network/interfaces` 时写一个只有回环接口的（`auto lo`、`iface lo inet loopback`，`Rootfs.ensureNetworkConfig`），与 setup-alpine 写的一样。
  - 调研时发现，只 `rc-update add nginx` 时，nginx 要 `net`，OpenRC 去拉起 networking；没有这个文件时它失败，nginx 也起不来。
  - 已经装好的 rootfs 也会补上，用户改过的不动；没装 OpenRC、ifupdown 时没人读它。
  - 一开始用的是 docker 镜像常见的 `/etc/rc.conf.d/tuxport.conf`：`rc_provide="loopback net"`。
  - 实测它有两个问题：rc.conf 里的 `rc_provide` 加到每一个服务上；loopback 本来就是一个真服务，每次开机都报 "loopback is the name of a real and virtual service"。
  - 想再提供 `clock`（busybox 的 syslog `need clock`，于是拉起 hwclock）时，依赖成了环，syslog、crond、nginx 都起不来。
  - 换成 interfaces 之后，networking 照常启动，这些问题都没有了。
- **用 osclock 提供 clock**：开机之前，先在 guest 里运行一段脚本（`Rootfs.PREFER_OSCLOCK`，经由不带控制台的 `spawn`：JNI 的 `console` 是负数时用 App 的 stdio）。装了 OpenRC（有 `rc-update` 和 `/etc/init.d/osclock`）、hwclock/osclock/swclock 都不在任何运行级别里时，执行 `rc-update -q add osclock boot`。
  - busybox 的 syslog `need clock`，crond 等又要 logger，所以只要开了常见的服务，OpenRC 就用 hwclock 提供 clock。
  - hwclock 读不到 RTC（没有 `/dev/rtc0`）、也设不了时间（`settimeofday` 是 EPERM，guest 的时间就是宿主的），开机日志里必然有一个 `[ !! ]`。
  - osclock 表示时间由操作系统管、什么也不做，正是 tuxport 的情形。
  - 用户自己放了哪个时钟服务就不动；没装 OpenRC 时什么也不做。
  - 不用 `rc_provide="clock"`：它加到每一个服务上，与 syslog 的依赖成环（见下面）。
- **轮询**：原来每 2 秒取一次监听事件，现在同时取关机请求和进程数（都要取 BKL，在 `worker` 上）：
  - `reboot` → `restart`：结束所有会话，关机，启动一个新的内核。Activity 先显示"正在重启"，新内核起来之后开一个新标签页。
  - `poweroff`、`halt` → `exitAll`。
  - 没有会话、界面不在（`listener` 是 null）、也没有 guest 进程了 → `exitAll`。
- **`exitAll`**（通知里的"全部退出"、上面两种情况）：
  1. 告诉 Activity 退出，结束会话，通知改成"正在停止服务"。
  2. 在 `worker` 上 `Native.shutdown`。
  3. 完了才 `stopSelf`。
  - 关机期间前台服务还在，App 进程不会被当作缓存进程杀掉。
  - 关机期间用户又打开了 App（又有了 `listener`）时，不停止服务，重新启动内核。
  - 被系统直接销毁（`onDestroy` 时内核还在）的照样在 `worker` 上关机，尽力而为。
- **通知**：有会话时"N 个会话运行中"，没有会话时"在后台运行"，退出时"正在停止服务"。

### MainActivity

- 关掉最后一个标签页只 `finish()`，不再停止服务。服务在 2 秒内看一次：没有 guest 进程了就自己退出，还有（OpenRC 起的服务、`nohup` 的后台任务）就留在后台。
- 再打开 App 时，服务里没有会话，照旧开一个新标签页。
- 新增 `onRestarting`：显示"正在重启"，直到新内核起来。

### 写的时候碰到的问题

- `exitAll` 先让 Activity `finish()`，Activity 要到 `onStop` 才放开 `listener`。没有慢服务时关机不到 1 秒就完成了，这时 `listener` 还在，被当作"用户又打开了 App"，重新启动了内核，还开了一个会话。
- 表现是 `poweroff` 之后界面退出，服务和通知（"1 个会话运行中"）却一直在。
- 现在 `exitAll` 告诉 Activity 之后立即清掉 `listener`。之后再有 `listener`，就一定是用户回来了。

## 测试

- 仪器测试新增 `bootsAndShutsDownByInittab`（按 inittab 开机、`processes`、guest 的 `poweroff` 和 `takePower`、`shutdown` 之后没有进程且关机的命令运行了、`ensureNetworkConfig`）和 `prefersOsclockOverHwclock`（没装 OpenRC、该加 osclock、boot 里已有 hwclock 三种情形）。整个 MachineTest 在 Pixel 4a、API 34 的模拟器上通过。
- 手动在同一个模拟器上的 App 里试了 guest 的 `reboot`（换成新内核上的新会话，关机的命令运行了）、关掉唯一的标签页之后留在后台、`poweroff` 之后服务退出、没有 guest 进程之后服务退出；新装 App、选 Alpine 时 `/etc/network/interfaces` 写好了，没有错误。
- 手动（OpenRC 的仓库，`tuxport-run --init`）：那段脚本没有时钟服务时加上 osclock、已有时钟服务和没装 OpenRC 时不动，加上之后开机日志里没有 `[ !! ]`；只有 lo 的 interfaces 时常见的服务和 networking 都起来了；用了 PostgreSQL 的那次，nginx、PostgreSQL、Redis 等都起来了，`tuxport-run` 退出时逐个被 OpenRC 停下。

## 推迟项

- 熄屏之后继续运行要的 `PARTIAL_WAKE_LOCK`（设计文档 §11.2）还没做：在后台跑着的服务熄屏后可能被暂停。→ 设置里、通知上可以打开"熄屏时保持运行"，持有 `PARTIAL_WAKE_LOCK`（M9：熄屏时保持运行）；默认仍是关的
- 开机的输出只在 `/var/log/boot.log` 里，App 不提示有服务启动失败。→ 开机完成后找 OpenRC 的 "failed to start"，有就发通知（[M9：进程被结束、服务没起来时提示](195-kill-reports.md)）
