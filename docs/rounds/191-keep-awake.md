# M9：熄屏时保持运行（2026-11-02）

设计文档 §11.2 的"熄屏后继续运行依赖 `PARTIAL_WAKE_LOCK`，由用户手动开启，默认关闭"，M9 计划里的 wake lock 开关。[App 按 inittab 开机](187-app-init.md)之后，OpenRC 起的服务在没有标签页时也留在后台，熄屏后 CPU 一休眠它们就停住，那一轮把这个记成了推迟项。

## 先查清楚：Doze 里锁还算不算数

Android 的文档说 Doze 时"系统忽略 wake lock"，又在资源限制表里写"运行前台服务的 App：网络不受限制"，两处放在一起看不出前台服务的 App 的 wake lock 在 Doze 里怎么样。如果也被忽略，只加 wake lock 不够，还得引导用户把 App 加进电池优化的豁免名单；直接请求豁免（`ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`）在 Play 上只许几类 App 用，终端不在其中。

读了 AOSP（main）的源码：

- `PowerManagerService.setWakeLockDisabledStateLocked`：Doze 时只禁用不在豁免名单里、并且进程状态在 `PROCESS_STATE_BOUND_FOREGROUND_SERVICE` 之后的 App 的 partial wake lock。前台服务的进程状态是 `FOREGROUND_SERVICE`，排在它前面，不禁用。
- `NetworkPolicyManager.isProcStateAllowedWhileIdleOrPowerSaveMode`：Doze、省电模式下按同一个门槛（`FOREGROUND_THRESHOLD_STATE` = `BOUND_FOREGROUND_SERVICE`）放行网络。

tuxport 只要有内核就有前台服务（`specialUse`），所以只加 `PARTIAL_WAKE_LOCK` 就够了，不申请电池优化的豁免。另一个限制 Low Power Standby（只放行 `BOUND_TOP` 之前的）在手机上默认关着，没有处理。

## 做法

- **开关**：设置页新的一行"熄屏时保持运行"，右边是开关，点整行切换。说明文字随开关变：开着时是"熄屏之后 CPU 不休眠，后台的服务、编译照常运行；更耗电"，关着时是"熄屏之后，系统可能暂停在后台运行的程序"。内核没在运行时也能改，只是记下。开关记在 SharedPreferences 里，默认关。
- **通知**：多一个按钮，关着时是"保持唤醒"，开着时是"允许休眠"，点了就切换（与 Termux 通知上的 wake lock 按钮一样，不用打开 App）。持有时通知的文字后面加"· 保持唤醒"。下载新的发行版时一共三个按钮（取消下载、这个、全部退出），正好是通知按钮的上限。
- **何时持有**（`TerminalService.updateWakeLock`）：开关开着，并且内核在运行或者正在关机（`exitAll` 到停止服务之前，OpenRC 停服务时熄屏也不被打断）。内核起来时、`detach`（重启、换发行版、退出）时、关机完成时、开关变化时重新判断；`onDestroy` 一定放开。锁不计数（`setReferenceCounted(false)`），没有超时：lint 的 `WakelockTimeout`、`Wakelock`（同一个方法里没有放开）都标了 `@SuppressLint`，注释写明原因。
- 清单里加 `android.permission.WAKE_LOCK`（普通权限，不用用户授权）。

## 测试

在 Pixel 4a（API 34）的模拟器上，用 `dumpsys power` 看锁：

- 默认没有锁；设置里、通知上打开和关掉都对；"全部退出"时关机完成后才放开；重新打开 App，内核起来之后自动又拿了锁。
- 熄屏、拔电、强制 Doze 之后这把锁仍在、没有 `DISABLED` 标记，过了一分钟仍是这样：上面从源码读出的规则在模拟器上成立。
- 模拟器的 CPU 本来就不会休眠，熄屏之后 guest 的程序持不持锁都照常运行，看不出差别，要真机（见推迟项）。
- 原来的仪器测试照旧通过。没有新的仪器测试：锁在服务里，要验证的是系统怎么对待它，靠上面的 `dumpsys`。

测试时碰到的问题：上一轮用 `settings delete global http_proxy` 删掉模拟器的系统代理之后，系统由它派生出的 `global_http_proxy_host`、`global_http_proxy_port` 还在，排除列表却空了，连接服务一直按这个代理走到重启，两个用本地 HTTP 服务的用例因此返回 404。清掉代理要用 `settings put global http_proxy :0`；`android/README.md` 上一轮写的办法改正了。

## 推迟项

- 只在模拟器上验证了系统怎么对待这把锁；熄屏之后 guest 的程序是不是真的一直在跑、多耗多少电，要真机（模拟器的 CPU 不休眠）。一些厂商的系统（MIUI、ColorOS 等）在 AOSP 之外另有后台管理，前台服务和 wake lock 都可能被它们停掉，没有处理，也没有引导用户去改。
- Play Console 的 Android vitals 把长时间持有 partial wake lock 算作"过多唤醒锁定"，可能影响 App 在商店里的展示；这里是用户自己打开的，前台服务期间的锁怎么计算没有查。
- 开着时只要内核在运行就持锁，不看有没有 guest 进程在干活（比如只开着一个空闲的 shell 也不让 CPU 休眠）。
