# M9：Debian 上装 sudo 显示进度、能跳过，普通用户的 shell 有作业控制（2026-11-03）

[以指定用户登录](193-login-account.md)那一轮的推迟项：Debian 上普通用户第一次登录时，登录脚本联网装 sudo，`apt-get -qq` 加上 `>/dev/null`，标签页里只有一行 "installing sudo..."，模拟器上几分钟没有动静；按 Ctrl-C 会连登录一起结束。

## 时间花在哪里

在 `tuxport-run`（Tier 1，colima 里，仓库放在虚拟机本地的卷上）里用 Debian 13 的 rootfs 分步量：

| 步骤 | 时间 |
|---|---|
| `apt-get update`（三个源，约 10 MB 索引） | 19.7 s（另一次网络好时 4 s） |
| `apt-get install --download-only sudo`（libapparmor1 和 sudo，2 MB） | 33.2 s |
| `apt-get install sudo`（包已经下载好，dpkg 装） | 2.4 s |

慢的都是联网：网络慢时（国内直连 deb.debian.org）要几分钟。dpkg 那一步只要两三秒。

## 做法

### 显示 apt 的输出

去掉 `-qq` 和 `>/dev/null`：`apt-get update`、`apt-get install` 在终端上有自己的进度（`45% [4 Packages 2125 kB/9614 kB 22%]`）、下载了什么、装了什么。开头那一行改成 "tuxport: installing sudo so that alice can become root (Ctrl-C skips it until the next login)"。

### Ctrl-C 跳过这一次

登录脚本一开始 `trap 'interrupted=1' INT`：脚本自己接住 SIGINT，不结束；子进程照样收到（接住的信号 exec 时恢复默认，忽略的会一直传下去，那样 apt-get 就停不下来了），被打断的 apt-get 失败，这次不装，打一行 "skipped installing sudo; tuxport tries again at the next login"（前面先换行：apt 的进度行停在一半）。之后照常登录。下一次登录还会再装：没有 sudo 这个账号提不了权，Debian 的 root 是锁着的（`/etc/shadow` 里是 `*`），`su` 也进不去。

按在哪一步都不会把 dpkg 弄坏，在 `tuxport-run` 里实测（包先下载好，在 1.2、1.8、2.4 秒时按）：

- 还没开始装：什么都没装。
- 在 `dpkg-preconfigure`（apt 在调用 dpkg 之前运行）里：它被打断，apt 报 "Sub-process … received signal 2" 退出，什么都没装。
- 在 dpkg 里：apt 给 dpkg 开了一个伪终端（`Dpkg::Use-Pty`），dpkg 在里面另起会话（轮询 `/proc` 看到 apt-get 在 pts/0、会话 31，dpkg 在 pts/1、会话 166），终端上的 Ctrl-C 到不了它；apt 自己接住 SIGINT，等 dpkg 装完才停，报 "Operation was interrupted before it could finish"。两个包都是 `install ok installed`，`dpkg --audit` 没有问题，登录脚本看到 sudo 在就记下装好了。

跳过之后下一次装上时，原来不说"可以用 sudo 成为 root"（那一行只在第一次加进组时打印），现在这一次装上的也打印。

### 顺带查出、修了的：Debian 上普通用户的 shell 没有作业控制，Ctrl-C 关掉标签页

验证跳过时，进了 alice 的 shell 之后打印 "bash: cannot set terminal process group (-1): Inappropriate ioctl for device"、"no job control in this shell"。改动之前的脚本一样：

- 登录脚本最后用 `su alice -c '… exec "$SHELL" -l'` 切换账号。util-linux 的 su（Debian 用的）对 `-c` 的命令另起会话（防止它用 TIOCSTI 往 root 的终端里塞字），shell 没有控制终端，也就没有作业控制。busybox 的 su（Alpine）不这样，所以第 193 轮在 Alpine 上手动验证时没碰到，Debian 只在联网的仪器测试里看了输出。
- 后果不只是没有 Ctrl-Z、`fg`：终端的前台进程组还是 su 的，按 Ctrl-C 时 su 收到 SIGINT，打印 "Session terminated, killing shell... ...killed." 结束整个会话，标签页就关了。原生的 Debian 13（util-linux 2.41.5）一样，tuxport 与它一致。
- 改成：su 支持 `--session-command` 时用它（`su --help` 里有这个选项），不另起会话。它防的 TIOCSTI 在这里不成问题：su 是标签页这个会话的组长，它退出时会话也就结束了，没有以 root 身份接着读终端的进程。busybox su 没有这个选项，照旧用 `-c`。
- 改了之后 alice 的 shell 里 `tty` 是 `/dev/pts/0`，`sleep 30` 时按 Ctrl-C 只打断 sleep，shell 还在；恢复的标签页从原来的目录开始照旧。

## 测试

- 在 `tuxport-run` 里（Debian 13 的 rootfs，用 guest 里的 `script` 开一个伪终端运行登录脚本，从管道定时送入 Ctrl-C 和命令）：第一次登录按 Ctrl-C 跳过、进了 shell 之后 Ctrl-C 只打断前台的命令，第二次登录显示进度、装上 sudo，第三次不再装；Alpine 上照旧用 `-c`。
- 仪器测试 `downloadedDistributionsRun`（联网，默认跳过）加上同样的几步：跳过、shell 里 Ctrl-C 之后会话还在、下一次登录装上；新加的 `loginSteps` 按步骤交互：等输出里出现某段文字再输入下一段。在 Pixel 4a（API 34）的模拟器上带 `network=true` 通过。第一次失败在用例的断言上：bash 在命令输出前先写 `\e[?2004l\r`，`/dev/pts/` 前面不是换行。

## 推迟项

- 下一次登录还会再装：网络一直慢时，每开一个标签页都要按一次 Ctrl-C。不装又没法提权（Debian 的 root 是锁着的），只能在设置里把登录账号改成 root 再自己装。
- apt 的输出有三十来行（索引、依赖、dpkg 的每一步），只在装的那一次出现。
- 装的那一步（dpkg）按 Ctrl-C 不会马上停，要等 dpkg 装完；在 `tuxport-run` 里两三秒。
