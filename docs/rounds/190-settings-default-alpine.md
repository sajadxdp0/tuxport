# M9：设置页，默认导入 Alpine（2026-11-02）

开始做发布前的 App 工作（M9，计划拆成 190–197 轮）。这一轮先做设置页，后面的 wake lock 开关、以指定用户登录、开源许可都要放在这里。同时按用户的决定改了首次启动：

- App 启动后默认就是 Alpine Linux，不再弹出选发行版的对话框；只有在设置页才能换成 Debian。
- 去掉 Ubuntu：App 里只剩 Alpine 和 Debian。tuxport 对 Ubuntu 用户态的兼容（[glibc 发行版的探测](69-glibc-probe.md)、[Ubuntu 26.04 的普查](149-ubuntu26-survey.md) 那些）不受影响，只是 App 里不能选了。
- Debian 的 tarball 暂时只用官方来源（GitHub 上 debuerreotype 的构建，Docker 官方镜像用的同一份），不自己托管（[App 里选 rootfs](99-choose-rootfs.md) 那条推迟项就此定下）。apt 的源照旧测速选最快的镜像。

## 做法

### 首次启动（`TerminalService.onCreate`）

还没有仓库（或者上次导入没完成）时直接导入 APK 里的 Alpine，界面显示"正在准备 Alpine Linux 3.24…"，好了就开第一个标签页。导入的是哪个发行版记在 SharedPreferences 里（`Rootfs.installed`），改名到位之前写到盘上：仓库在了，记录就是它的。这一轮之前导入的仓库没有记录，当作 Alpine（App 还没发布过，只有开发用的安装上有）。

### 设置页（`SettingsActivity`）

标签栏菜单里原来只有"重置 rootfs"，现在换成"设置"。设置页是普通的 Activity，四行：

| 一行 | 显示 | 点按 |
|---|---|---|
| 发行版 | 装着的发行版；正在下载时是"正在下载 Debian 13：12 / 47 MB，点按取消" | 单选（Debian 标出要下载多少），选了别的再确认；下载中点按是"不再下载？" |
| 软件源镜像 | 主机名；测速选出的后面写"（测速选出的）"；测速中是"正在测速…" | 单选："自动选择最快的"和内置的几个镜像 |
| 重置 rootfs | "删掉 rootfs 里的所有文件，重新导入 Alpine Linux 3.24" | 确认之后换成装着的那个发行版 |
| 执行层 | Tier 1（默认）或 Tier 0（解释器，排查问题用） | 单选，确认之后重启内核 |

状态都在 `TerminalService` 里，设置页只显示和转达。内核没在运行（启动中、正在换仓库）时只能看；正在下载时只有发行版这一行能点（取消）。

服务原来只有一个 `listener`，就是终端界面，它为 null 表示界面不在：60 秒后释放缓存，没有会话、也没有 guest 进程时退出。现在改成一组 `Listener`（接口的方法都有空的默认实现，界面只实现自己要的），设置页、终端界面在 onStart 加进来、onStop 去掉，有一个在就算界面可见。

### 换发行版、重置 rootfs（`TerminalService.switchTo`）

重置就是换成装着的那个。原来重置是先删掉仓库再让用户选，Debian 要下载 47 MB，国内直连 GitHub 只有 18 KB/s，四十多分钟里 App 什么也用不了。现在分两步：

1. **下载**（`Rootfs.download`）：在后台线程上下载并校验 tarball，缓存里已经有校验过的就不下。这期间原来的系统照常运行。进度显示在设置页和通知里（通知的第二行和进度条），两处都能取消（通知上的"取消下载"按钮）。下载失败时原来的系统不动，弹出原因（"无法下载 Debian 13：timeout"）。
2. **换掉**（`replace`）：下载好了才结束所有会话、停下内核、删掉仓库、导入新的。这时设置页关掉，回到终端界面看"正在准备 Debian 13…"，好了开一个新标签页。导入失败时（下载已经校验过，多半是空间不够）退回 APK 里的 Alpine，并告诉用户。

两个细节：

- **启动失败时也能换**（`canSwitch`）：内核没在运行、但启动失败了（`startError`）时，发行版和重置两行照样能用。原来仓库坏了（`meta.db` 打不开之类）只能清 App 的数据，现在可以在设置里重置。
- **取消之后等旧线程结束**（`downloader`）：取消是立刻生效的（界面马上回到原来的样子），但下载线程可能还卡在连接或者读上，要等一个超时（30 秒）才发现被取消了。这期间如果又开始一次下载，两个线程写同一个 `.part`，先结束的那个还会在 `finally` 里删掉它，后一个改名时失败。所以旧线程结束之前不开始新的下载，发行版和重置两行这时是灰的。

`Rootfs.fetch` 加了 `cancelled` 参数：每读一块之前看一次，返回 true 时抛出 `Rootfs.Cancelled`，不再试别的来源，`.part` 照旧删掉。

### 软件源镜像（`Rootfs.setMirror`、`chooseMirror`）

记录里有现在写在源里的镜像，以及它是不是用户选的。

- 手动选：写进源文件（覆盖用户自己改过的），记下是手动的；之后测速（比如导入之后的那次还没测完）不再换掉它。
- 选回"自动"：清掉手动的标记，重新测速，写进最快的。

两者在同一把锁里写，测速期间用户选了镜像，测完时看到手动的标记就不写。内核运行着也可以写源文件：`Native.writeFile` 另外打开仓库，提交计数器变了，内核层的缓存随之作废（设计文档 §8.1），guest 马上读到新的内容（仪器测试 `manualMirrorIsWritten` 验证了）。

### 执行层

记在 SharedPreferences 里（默认 1），启动内核时传给 `Native.start`。换了之后走 guest `reboot` 的那条路：关机，再启动一个新的内核。

### 写的时候碰到的问题

- 第一次截图时设置页的下载进度是"0 / 0 MB"：总大小要等第一次回报进度才有。改成一开始就用固定写着的长度。
- `Rootfs.fetch` 加 `cancelled` 参数时放在了 `progress` 后面，原来的调用 `fetch(download, dest) { done, total -> … }` 的尾随 lambda 就成了 `cancelled`，编译不过。把 `cancelled` 放到 `progress` 前面，进度仍是尾随的 lambda。
- 换发行版的下载在设置页点取消时，上面"等旧线程结束"的问题是写测试时想到的：模拟器经由代理下载得太快，几次都没来得及点取消就下完了。

## 测试

- 仪器测试新增 `downloadsCanBeCancelled`（本地的 HTTP 服务发一部分就停住，取消之后抛出 `Rootfs.Cancelled`、不留 `.part`）和 `manualMirrorIsWritten`（内核运行着时 `setMirror`，guest 马上读到新的源）；`downloadedDistributionsRun` 只下 Debian 了。
- Pixel 4a（API 34，4K 页）的模拟器上通过。带 `network=true`、给模拟器设上系统代理单独跑 `downloadedDistributionsRun`：下载 Debian、导入、登录 bash、`apt-get update` 通过。→ `apt-get update` 这一项是假的通过：apt 取不到索引时只警告、退出码仍是 0。模拟器上 glibc 一次查 A 和 AAAA 的解析一直失败，第 193 轮查出来是 DNS 存根的回答在 `FIONREAD` 里报成 0，修了（M9：以指定用户登录），用例也改成看输出
- Qenix 16K（API 37，16K 页）的模拟器上只有 `lowPortsMoveToHighPortsOnTheHost` 失败，失败的是用例开头的前提"App 进程自己绑不了 22 端口"：这个 Android 17 的镜像上 App 自己就能绑（见推迟项），与这一轮的改动无关。
- 手动（4K 模拟器上新装的 App，用 `adb shell input` 操作、截图看）：首次启动直接进 Alpine（16K 模拟器上也一样）、手动选镜像、经由系统代理换 Debian、在 Debian 上重置、连不上和断网时弹出原因并且不动原来的系统、下载中取消、换执行层、换回 Alpine、`meta.db` 写坏之后在设置页重置，都与预期相同。
- 第一次在 4K 模拟器上跑仪器测试时，模拟器上留着的旧仓库里 `/bin/sh` 读出来不是 ELF，用到 shell 的用例都失败了；数据随 App 卸载没了，没法再查，重新导入之后都通过（见推迟项）。

## 推迟项

- 下载中途 App 被杀掉时，下了一半的 `.part` 删掉，下次从头下；不跨 App 进程续传。
- 取消之后要等旧的下载线程结束（最多一个超时，30 秒）才能再换发行版。
- 导入失败退回 Alpine 的那条路只在代码里，没有手动触发过（要让导入失败，比如空间不够）。
- 镜像测速和 tarball 的下载用 App 的 `HttpURLConnection`，走系统的 HTTP 代理；guest 的 apk、apt 不走。设备上开了代理时测出来的是经由代理的速度，选出的镜像对 guest 不一定最快。
- 设置里选的镜像覆盖源文件，用户自己加的行（比如 Alpine 的 testing）也一起没了；列表以外的镜像仍要手改。→ 只换内置镜像的地址了，用户加的行留着；源里全是列表以外的镜像时不动、提示手改（[M9：换镜像不丢用户加的源](203-mirror-keeps-lines.md)）；列表以外的镜像仍要手改
- 这一轮之前导入的仓库没有发行版的记录，当作 Alpine；开发用的安装上要是装着 Ubuntu，设置页会显示成 Alpine。
- Android 17（API 37）的 16K 模拟器上 App 自己能绑 22 端口，`lowPortsMoveToHighPortsOnTheHost` 的前提断言失败，原因没查（App 没有 capability，`ip_unprivileged_port_start` 是 1024，可能是平台在 bind 上另有放行）。真机的 Android 17 上如果也这样，低端口映射在那里就不是必须的，现在照样映射。→ 原因查明：2026 年 5 月起的 Google Play 系统更新让 App 绑得了 TCP 的 20–23、80、443、445、515、631 和 UDP 的 319、320、443（Android 13 以上、内核 5.15 以上），这个模拟器上有；现在宿主让绑原来的端口就绑原来的，用例拆成绑不了的 79 和绑得了的 22 两个（[M9：宿主让绑的低端口就绑原来的](208-low-ports-direct.md)）
- 4K 模拟器上旧仓库的 `/bin/sh` 读出来不是 ELF，原因没查（数据已经没了）。仓库在宿主被强行关掉之后能不能保证程序文件完整，值得以后专门试一次（比如 `apk upgrade` 的中途杀掉模拟器）。→ 用 dm-flakey 模拟了断电：App 被杀（宿主的页缓存还在）不会坏，断电时改名覆盖的文件（apk 就这样装文件）全变成 0 字节，fsync 过的也是空的，与这里的症状一样；补上了落盘的顺序（[M9：断电之后仓库不坏](198-power-loss.md)）。那一次是不是这个原因已经没法确认
