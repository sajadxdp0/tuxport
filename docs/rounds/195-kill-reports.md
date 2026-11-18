# M9：进程被结束、服务没起来时提示（2026-11-02）

M9 计划里的第六轮。内核层结束 guest 进程（内核层的 bug、引擎让宿主出错、OOM killer）时，报告原来只写进 logcat 和内核日志（`dmesg`），标签页里只看到 shell 打印的 "Killed"、"Segmentation fault"，看不出是内存不够还是 tuxport 的 bug（[第 20 轮](20-robustness.md)、[第 21 轮](21-robustness-finish.md)、[第 143 轮](143-kmsg.md)的推迟项）。开机时 OpenRC 有服务没起来，输出只在 `/var/log/boot.log` 里，没人看（[第 187 轮](187-app-init.md)的推迟项）。

## 做法

### 内核层：结束进程的报告（`Machine::take_kills`）

内核层每结束一个进程记一份报告（`KillReport`）：pid、进程名、控制台的编号、原因。原因是两种（`KillReason`）：

- **bug**：内核层的 bug、引擎的宿主故障（`bug::kill`、`bug::kill_process`），带着报告的那一行（与写进 stderr、内核日志的相同）。
- **内存不够**：OOM killer 结束的进程（`oom::out_of_memory`），连同 `memory.oom.group` 时一起结束的（Linux 整组结束时把挑中的那个再算一次，报告不重复）。带着是哪个 cgroup 的 `memory.max`（App 的内存上限时没有 cgroup）、用量和上限。

进程的控制终端是 App 的某个控制台（`Machine::open_console` 建的）时记下它的编号，App 据此找到标签页。撞上 bug 时线程组可能正被借着，借不到就当作没有控制终端，报告照样记下。最多攒 32 份，App 来不及取时丢掉最早的；`tuxport-run` 不取，报告只在 stderr。

`Native.takeKills` 把报告转成一行一项、制表符分隔的文本。App 在原来每秒取监听端口和关机请求的那次轮询里一起取。

### App：标签页里说明原因，或者发通知

控制终端是某个标签页时，在那个标签页里用黄色显示一行，比如：

```
[tuxport] sort (pid 12) was killed: cgroup /t reached its memory.max (16.1 MiB of 16 MiB).
```

App 的内存上限时说"App 的内存用到了上限，OOM killer 挑中了它"；bug 时说是 tuxport 的内部错误，`dmesg` 里有报告。大小按 1024 进制显示：第一版用 Android 的 `Formatter`（1000 进制），16M 的 `memory.max` 显示成 "17 MB of 17 MB"，看不出超了。

不在标签页里显示、改发通知的情况：

- 正在全屏程序里（终端在备用屏幕上，vim 这些）：写进去会弄乱它的画面。
- 没有控制终端：OpenRC 起的服务、`setsid` 出去的进程。
- 界面不在（App 在后台）：在标签页里显示了，另外发通知。

通知在新的"进程和服务"渠道里，按 pid 编号，点开回到 App。标签页还没显示过（恢复出来的、终端模拟器还没建）时提示先攒着，建好之后先显示。

#### 插在哪里

提示到的时候，shell 多半已经打印了 "Killed" 和新的提示符（每秒才取一次）。第一版直接追加在后面，提示落在提示符之后，光标停在下一行的空行上，看起来像提示符丢了，用户接着输入时 shell 的行编辑和屏幕也对不上。改成插在光标所在的行上面（vendored 的 `TerminalSession.showMessage`）：

1. 先换行再回到原来的行（只用 LF，列不变）：光标在最后一行时屏幕上卷，腾出地方；
2. 记下光标（DECSC），回到行首，在这一行插入提示要占的行数（IL）：提示符和输入了一半的命令跟着下移；
3. 写提示，恢复光标（DECRC）再下移同样的行数，回到提示符后面原来的位置。

占几行按终端的列数和每个字的宽度（`WcWidth`，中文占两列）算。shell 不知道屏幕变了，但它的光标、行内容和屏幕上的仍然一致，接着输入、回车都正常（手动在屏幕最后一行、输入了一半的命令时试过）。

### 开机时没起来的服务

开机完成之后（`Native.boot` 返回，OpenRC 的 sysinit、boot、default 都跑完了）读 `/var/log/boot.log`，找 OpenRC 写的 `ERROR: <服务> failed to start`，去掉重复的。有的话发一条通知："2 个服务没有起来"，正文列出服务名，说明开机的输出在 `/var/log/boot.log`。

手动测试时第一次打开 App 就报了两个服务没起来，是仪器测试里写进 `boot.log` 的假服务：minirootfs 没装 OpenRC，inittab 里的程序都不存在、一个命令也不运行，`boot.log` 原来只在第一个命令运行时清空，就一直留着上次开机的内容。现在一个命令也没运行时也清空它（有内容才写）。

### 顺带修的

- **合成文件系统里已有的名字，mkdir 是 EACCES**：写 OOM 的测试时发现 `mkdir -p /sys/fs/cgroup/t` 失败。busybox 的 `mkdir -p` 从头逐级 mkdir，`mkdir /sys/fs` 在 Linux 上是 EEXIST，它接着往下；tuxport 先检查父目录 `/sys` 能不能修改，在合成文件系统里是 EACCES，就停下了。Linux 的 `filename_create` 先查名字，已经存在就是 EEXIST，然后才轮到只读、没有权限。新的 `create_parent_at` 照这个顺序，mkdirat、mknodat、symlinkat、linkat 的新名字和 bind 一个 unix socket 的路径（EADDRINUSE）都用它。原来在 cgroup 的根下面直接 `mkdir /sys/fs/cgroup/x` 是好的，只是 `-p` 不行。
- **/dev/zero、/dev/urandom 一次只读 64 KB**：`dd if=/dev/zero bs=256M count=1` 在 tuxport 里读到的是 `0+1 records`（64 KB），Linux 读满 256 MB。内核层与 guest 之间按 64 KB 分块拷贝，普通文件、procfs 读满所有块，设备文件只读一块就返回了。/dev/zero、/dev/full、/dev/urandom（/dev/random 相同）现在也读满。/dev/kmsg 一次读一条，照旧。
- **宿主线程不持 BKL 用仓库**：`Machine::inittab`（开机、关机时读 `/etc/inittab`）和开机时检查 inittab 的程序在不在，都把 `Rc<Vfs>` 克隆出来、放开 BKL 再用。关机时 guest 进程还在运行，同一个 SQLite 连接、同一组缓存就可能被两个线程同时用，`Rc` 的计数也可能被同时改。现在都持着 BKL 做。[第 193 轮](193-login-account.md)记过 `init_runs_the_inittab` 偶发一次失败：关机时 guest 线程在 rusqlite 的语句缓存里撞上 `RefCell` 已被借走，正是两个线程同时用一个连接的样子，可能就是这个原因。没能复现来确认：修之前的写法用 6 个不停 `ls -lR`、`stat` 的进程压着关机，跑了 40 次都没出现（窗口只有读 inittab 的几微秒）。

## 测试

- 内核层（`cargo test`）：`host_calls_survive_kernel_bugs` 加了一段，看各种 bug 的报告；新增 `oom_kills_are_reported_to_the_app`，看 cgroup 的 `memory.max`、App 的内存上限、没有控制终端、`memory.oom.group` 几种情形的报告（看代码时发现第一版里挑中的那个报告了两份，修了）；`init_runs_the_inittab` 加了一段，看一个命令也没运行时上次的 `boot.log` 被清空。
- 差分测试 `cgroup-tree` 新增合成文件系统里已有名字的 mkdir 和 `mkdir -p`，`coreutils` 新增 `dd` 从 /dev/zero、/dev/urandom 读大块，两种执行层都与原生一致。
- 仪器测试新增 `killsAreReportedToTheApp`（测试钩子模拟的内核 panic、cgroup 里的 OOM），`bootsAndShutsDownByInittab` 加上读出没起来的服务；在 Pixel 4a（API 34）的模拟器上通过。第一版用 `dd bs=64M` 吃内存，模拟器上 dd 17 毫秒就做完退出了，OOM killer 每 20 毫秒才量一次，有时发现不了（§12 第 4 条），改成吃完内存之后接着运行一会儿的 awk。
- 手动（模拟器上的 App）：OOM 的提示插在 shell 的提示符上面，屏幕已满、输入了一半的命令时也一样，回车照常；没有控制终端、在 vi 里时改发通知；busybox 的 less 见推迟项。真装 OpenRC、加一个起不来的服务的试验没做成：当时宿主的网络很差，模拟器里 `apk add openrc` 下载一直停着。

## 推迟项

- 报告每秒取一次，提示总在 shell 的 "Killed" 和新提示符之后才到，靠插在提示符上面弥补。
- 不用备用屏幕、自己整屏重画的程序（busybox 的 less、top）在前台时，提示照样插在光标那一行上面：手动试了 busybox 的 less，提示出现在它的状态行上面，上面的内容被顶上去两行，直到它下次重画时被盖掉；这种情况不发通知。
- OOM killer 定时量用量：一下子分配完、在下一次测量之前就退出的进程不会被结束，也就没有提示（§12 第 4 条原有的限制）。
- 内核层的 bug 只说"内部错误"，报告的原文要用 `dmesg` 看；没有"复制报告"或者"反馈问题"的入口。→ 通知上有"复制报告"了（版本、设备、发行版、执行层和报告），报告里也有了 panic 的消息和位置（[M9：内核 bug 的报告能复制，带着 panic 的位置](205-copy-bug-report.md)）；"反馈问题"仍没有，等定了仓库公开与否
- 开机失败的通知只认 OpenRC 的 `ERROR: <服务> failed to start`；sysvinit（Debian 的 `/etc/init.d/rc`）的失败格式不统一，没有提示。之后运行中的服务崩溃（`rc-service` 里是 crashed）也不提示。
