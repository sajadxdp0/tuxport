# M9：以指定用户登录（2026-11-02）

设计文档 §11.1 原来写的是"默认以 root 登录，设置里提供以指定用户登录"。动手之前和用户确认了几点：

- 建账号不需要设置页：在命令行里 `adduser`（Debian 是 `adduser`/`useradd`）、`passwd`、`su - alice` 都能用，权限也照常生效（先在 `tuxport-run` 里试过）。设置只管新标签页以谁登录。
- 用户希望像桌面 Linux 一样：建了账号以后，冷启动、新开标签页都自动以它登录。默认值因此是"自动"：有普通用户就用最先建的那个，没有就是 root；设置里也能明确选 root 或某个账号。
- 自动换成普通用户之后有个坑：Alpine 的 root 默认没有密码（锁着的），`su` 回不去，`apk add` 也用不了。桌面发行版的安装程序会把第一个用户加进 wheel、配好 sudo，用户问能不能也这样做：可以，照 Alpine 自己的 `setup-user -a`（`apk add doas`，`/etc/doas.d/20-wheel.conf` 写 `permit persist :wheel`，`addgroup 用户 wheel`）。
- 提权要不要密码，用户选了"有密码就问"：设了密码的照发行版的惯例要输，没设的（`adduser -D`）不问。

## 做法

### 登录脚本（`android/app/src/main/assets/login.sh`）

原来标签页运行的是一行 shell（`ConsolePty.LOGIN`）：从 `/etc/passwd` 找 root 的 shell 再 `exec`。现在换成 assets 里的一个脚本，仍以 root 运行（`sh -c 脚本 tuxport-login 账号`），参数是设置里的登录账号：

1. **选账号**：`auto` 用 UID 1000–65533、shell 不是 nologin/false 的账号里 UID 最小的，没有就是 root；`root` 就是 root；账号名在 `/etc/passwd` 里就用它，不在了打一行提示、用 root。
2. **root**：与原来一样，以 passwd 里 root 的 shell 作为登录 shell（chsh 改过也照样）。
3. **普通用户第一次成为登录账号**（不在 `/var/lib/tuxport/admins` 里）：Alpine 加进 wheel、`20-wheel.conf` 里没有那一行就加上；Debian 加进 sudo 组（Debian 默认的 sudoers 已经有 `%sudo ALL=(ALL:ALL) ALL`）。记进 admins，打一行"alice is in the wheel group and can become root with doas"。之后用户自己把它移出组，不再加回去。
4. **提权工具**：Alpine 用 App 带着的 doas 包离线装（`apk add --no-network --repositories-file /dev/null /var/cache/tuxport/doas.apk`，不读仓库索引，没有警告），再按名字 `apk add doas` 一次：用文件装的包在 `/etc/apk/world` 里记成固定那个文件（`doas><哈希>`），之后 `apk upgrade` 不会升级它，按名字加一次就变回普通的 `doas`。Debian 联网 `apt-get update`、`apt-get install sudo`（`DEBIAN_FRONTEND=noninteractive`）。装上过一次（或者本来就有）就在 `/var/lib/tuxport/` 记下，之后用户自己卸掉也不再装；没装上（Debian 当时没网）的之后每次登录再试，打一行说明。
5. **免密规则**：每次登录，按当时 `/etc/shadow` 的密码（空的、`!`、`*` 开头的算没设）重写 `/etc/doas.d/30-tuxport.conf`（`permit nopass alice`，排在 `20-wheel.conf` 后面，doas 最后匹配的规则生效）或 `/etc/sudoers.d/tuxport`（`alice ALL=(ALL:ALL) NOPASSWD: ALL`，0440），只列 admins 里还在组里的。设了密码之后下一次登录起这一行就没了，要输密码。工具没装上时（目录不在）不写。
6. **`su -l 账号`**：busybox 和 util-linux 的 su 以 root 调用都不问密码，`-l` 设好 HOME、SHELL、USER，进家目录、读 profile。

装软件的命令都从 `/dev/null` 读：标签页一开用户就打字时，提前敲的内容要留给登录 shell。测试时就是这样卡住的：apt、dpkg 读走了测试写进去的命令。

### App

- `TerminalService.login`：设置里的登录账号，记在 SharedPreferences 里，默认 `auto`；新开标签页时交给 `ConsolePty`，开着的不变。换发行版、重置 rootfs 时回到 `auto`（原来的账号随仓库删了）。
- `Rootfs.ensureLoginTools`：Alpine 上每次启动内核之前把 assets 里的 `doas.apk` 写进 `/var/cache/tuxport/`（App 升级之后换成新的）。
- 设置页新加"登录账号"：说明是"自动：alice（最先建的普通用户）"、"自动：root。在终端里用 adduser 建了普通用户之后，新标签页就以它登录"、"root"、"alice"或"bob（找不到这个账号，新标签页以 root 登录）"；点了是单选（自动、root、各个普通用户），选了提示"新开的标签页以 alice 登录"。账号列表在后台读 `/etc/passwd`，规则与脚本一样（`Rootfs.regularAccounts`）。
- 读 `/etc/passwd` 用新加的 `Native.readFile`（内核层的 `vfs::read_file`、`tuxport_kernel::read_file`，与 `write_file` 对称：路径里的符号链接不跟随，另外打开仓库，内核推迟的写事务先提交）。
- doas 包由 `scripts/fetch-rootfs` 下载（同一个 Alpine 分支，sha256 固定；包本身带 Alpine 的签名，apk 装的时候用 rootfs 里的公钥验），Gradle 把它和 rootfs 一起放进 assets。开源许可页的 Alpine 一节加上它（版本、ISC、aports 的提交，从包里的 `.PKGINFO` 读）。

### 顺带查出、修了的：DNS 存根的 `FIONREAD`

在模拟器上验证 Debian 装 sudo 时，`apt-get update` 一直"Temporary failure resolving 'deb.debian.org'"。查下来：

- 只查 IPv4（`Acquire::ForceIPv4`）或只查 IPv6 都好，`getent ahosts`（`AF_UNSPEC`）什么也查不到。glibc 这时用 `sendmmsg` 一次发出 A、AAAA 两个查询。
- 存根收到的两个查询、交回去的两个回答都正常（ID、标志、记录数都对，Android 解析器的日志里也是缓存命中、正常回答）。
- glibc 收第二个回答之前用 `ioctl(FIONREAD)` 看下一个数据报多大，按它给第二个回答分配缓冲区。tuxport 对 inet socket 的 `FIONREAD` 只问宿主 socket，存根的回答在自己的队列里，于是报成 0。Linux 上数据报 socket 的 `FIONREAD` 是下一个数据报的长度。
- 改成：队列里有存根的回答时报告第一个的长度（recvmsg 也是先读它们）。之后模拟器上 `getent ahosts`、apt 都正常了。

这意味着 Android 上 Debian（以及原来的 Ubuntu）里用 glibc 解析域名的程序一直有这个问题，只是没被发现：[App 里选 rootfs](99-choose-rootfs.md)、[设置页](190-settings-default-alpine.md)两轮的联网用例里 `apt-get update -qq && echo apt-ok` 是假的通过，apt 取不到索引时只警告、退出码仍是 0。用例改成看输出里有没有 "Failed to fetch"。Linux 宿主上（`tuxport-run`）为什么一直没出问题没有细查；musl（Alpine）的解析器不用 `FIONREAD`，不受影响。

## 测试

- 先在 `tuxport-run`（开发容器）里直接跑登录脚本：Alpine 和 Debian 上选账号、第一次登录加组和装提权工具、按有没有密码写免密规则、移出组之后不再加回去、账号不在时用 root、`su -l` 之后的环境都对。
- guest 测试 `tests/guest/net.c` 新增 `test_dns_stub_fionread`：`sendmmsg` 发 A、AAAA 两个查询给存根，每个回答的 `FIONREAD` 都等于随后 `recv` 的长度。不需要外网（没网时存根回答 SERVFAIL）。
- 仪器测试新增 `loginAccountsAndDoas`（选账号、加进 wheel、doas 免密和要认证）；联网的 `downloadedDistributionsRun` 的 `apt-get update` 改成看输出，并加上自动以新建的用户登录、装上 sudo。在 Pixel 4a（API 34）的模拟器上通过，只在 4K 页的模拟器上跑了。
- 手动在模拟器上的 App 里看了设置页的"登录账号"随 `adduser` 变化、新标签页以 alice 登录并能 `doas`、选 root 之后新开的标签页是 root。
- 全量并发的 `cargo test` 里 `init_runs_the_inittab` 失败过一次，单独重跑都通过（见推迟项）。

## 推迟项

- 普通用户第一次登录时自动加进管理员组，只在 App 打开的标签页里发生；用 `su - alice`、ssh 登录的不经过登录脚本，不会被加组、装工具。
- Debian 上装 sudo 要联网，模拟器上要几分钟，期间这个标签页一直在装。没有进度，也不能跳过（Ctrl-C 会连登录一起中断）。 → 显示 apt 的进度，Ctrl-C 跳过这一次、登录照常（[M9：Debian 上装 sudo 显示进度、能跳过，普通用户的 shell 有作业控制](206-debian-sudo-progress.md)）；仍要联网，跳过的下一次登录再装
- "自动"选 UID 最小的普通用户；有几个时没有办法说"自动选另一个"，只能明确选。设置页列出的只有 UID 1000–65533 的账号，UID 更小的普通账号（比如手动指定的）不在列表里。
- 免密规则只看 `/etc/shadow` 的密码字段：用别的认证方式（PAM 模块、`!` 锁住但有 SSH 密钥）的账号算"没设密码"，提权不问。
- 设置页的账号列表在打开设置页时读一次：设置页开着时在终端里建的账号要重新进设置页才看得到。
- `init_runs_the_inittab`（`hosts/cli/tests/rootfs_programs.rs`）在全量并发的 `cargo test` 里失败过一次：guest 线程在 rusqlite 的语句缓存里 panic（`cache.rs:142`，`RefCell` 已经被借走），时机是 `poweroff` 让所有进程同时退出的时候。说明有两处同时用了同一个仓库连接（两个线程，或者同一个线程重入），单独重跑 5 次都没再出现（那次的 core 文件留在 `target/u/core-init_runs_the_inittab`）。与这一轮的改动无关，原因没查，值得单独查：如果真是两个线程同时用一个连接，后果不只是 panic。→ 找到一处这样的用法：宿主线程开机、关机时不持 BKL 读 `/etc/inittab`，改成持锁，可能就是这次的原因，没能复现确认（[M9：进程被结束、服务没起来时提示](195-kill-reports.md)）；之后在检查的构建里让不持着内核的 BKL 用它的仓库直接当作 bug，全量测试、差分测试、LTP、仪器测试都没有别处这样用，改回不持锁读 inittab 时这个用例第一次就失败，确认就是它（[M9：内核的仓库只在持着它的 BKL 时用](202-store-bkl-check.md)）
- Linux 宿主上 `FIONREAD` 原来也报 0，`tuxport-run` 里 glibc 的解析却一直正常，差别在哪里没有细查。
