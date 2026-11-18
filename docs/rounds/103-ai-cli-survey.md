# M9 之前：AI 编程 CLI（2026-10-29）

审完推迟项之后决定的普查：现在在手机终端里跑 AI 编程 CLI 的人很多。它们的形式各不相同，有 Bun 编译的单个可执行文件、node 上的打包脚本、Rust 和 Go 的静态二进制；用到 ripgrep、PTY、子进程和后台作业，有的还要用户命名空间来建沙箱。

## 做法

### 普查

普查脚本是 `tests/survey/ai/agents.sh`（`scripts/survey ai/agents`）：

- 不用账号。脚本里带一个模拟服务（Python 标准库），冒充三家的模型 API：Anthropic Messages（Claude Code，`ANTHROPIC_BASE_URL`）、OpenAI Responses（Codex，`config.toml` 里的自定义 provider）、Gemini（Gemini CLI，`GOOGLE_GEMINI_BASE_URL`）。带执行命令工具的请求按固定的 8 步依次要求调用工具：
  1. 执行命令；
  2. 写文件；
  3. 读文件；
  4. 改文件；
  5. 运行改过的 Python；
  6. grep；
  7. glob；
  8. 后台作业和 git。

  工具结果都回来之后，回答一段文字；别的请求（起标题、模型路由的分类）按请求里的 JSON schema 回答。每一步工具的结果都打印出来。Codex 只给 `exec_command`（这个版本没有 apply_patch），文件类的几步换成命令。
- 再看两家用 bubblewrap 的沙箱：Codex 的 `-s workspace-write` 和 `codex sandbox`，Claude Code 的 `sandbox.enabled`。
- 交互界面在 tmux 里一次开一个，截屏，记下所有进程合计用的内存（cgroup 的 `memory.current`）。opencode、GitHub Copilot CLI、Crush、Qwen Code 只看版本和界面。
- 在 tuxport 的 Alpine 3.24 仓库（普查仓库）里跑。原生对照是 colima 虚拟机里的 alpine:3.24 容器，跑同一个脚本。

### 修复

`core/kernel/src/pseudo.rs` 新增 `/proc/sys/kernel/overflowuid`、`overflowgid` 和 `/proc/sys/fs/overflowuid`、`overflowgid`。差分测试 compat 组读这四个文件，与原生相同；用改动之前的代码跑时读不到。

## 结果

三个有模拟服务的 CLI，无头模式的 8 步在 tuxport 里全部成功，结果与原生相同；七个 CLI 的交互界面都与原生相同。Debian 13 的仓库（glibc 的构建、node 20）里也跑了一遍，结果一样，只是 GitHub Copilot CLI 在这里能用。

| CLI | 形式 | 版本 | 无头模式的 8 步（tuxport / 原生） | 交互界面 |
|---|---|---|---|---|
| Claude Code | Bun 编译的单个可执行文件 | 2.1.312 | 全部成功，3 秒 / 2 秒 | 正常 |
| Codex | Rust 的静态二进制，npm 包里是 node 写的启动脚本 | 0.165.0 | 全部成功，6 秒 / 7 秒 | 正常（要 `--no-daemon`，原生也一样） |
| Gemini CLI | node 上的打包脚本 | 0.66.0 | 全部成功，6 秒 / 3 秒 | 正常 |
| opencode | Bun 编译的单个可执行文件 | 1.19.5 | — | 正常 |
| GitHub Copilot CLI | node，带原生模块 | 1.0.105 | — | Alpine 上起不来（原生也一样），Debian 上正常 |
| Crush | Go 的静态二进制 | 0.98.2 | — | 正常 |
| Qwen Code | node 上的打包脚本 | 0.26.3 | — | 正常 |

- 沙箱：Codex 的 `-s workspace-write` 下 8 条命令都失败，`codex sandbox` 也一样，报 `bwrap: No permissions to create a new namespace`；Claude Code 开了沙箱之后，3 次 Bash 都这样失败，读写文件、grep、glob 不受影响。与原生的 docker 容器相同。
- 内存（每个界面开起来之后 cgroup 的 `memory.current` 增加了多少，MiB）：

  | | Claude Code | Codex | Gemini CLI | opencode | Crush | Qwen Code |
  |---|---|---|---|---|---|---|
  | tuxport | 171 | 211 | 583 | 699 | 164 | 375 |
  | 原生 | 77 | 40 | 286 | 453 | 53 | 133 |

  只能大致比：tuxport 的是 App 的匿名内存（OOM killer 量的），原生的是容器的用量，含读进来的文件的页缓存。
- npm 一个一个装这七个包，在 tuxport 里每个 6–17 秒，其间 `memory.peak` 1188 MiB。
- 未实现的系统调用只有 io_uring_setup（425，Codex 和 node 探测它，有退路），Debian 上还有 glibc 注册的 rseq（293）。都是设计上不做的。

### 找到的问题

1. tuxport 没有 `/proc/sys/kernel/overflowuid`、`overflowgid`（还有 `fs` 下的同名文件）。bubblewrap 启动时先读它们，读不到就报错退出，比原生更早失败，报错也不同。这一轮补上了，值是 Linux 的默认值 65534。
2. tuxport 关掉 PTY 的主设备时不挂断从设备：`tmux kill-server` 之后 opencode 留下来一直在跑，原生上它退出了。见[下一轮](104-pty-hangup.md)。
3. 内存：七个界面同时开着时，tuxport 里超过了开发虚拟机上 App 的内存上限（物理内存 4 GB 的一半），OOM killer 结束了 opencode 和一个 node 进程；npm 一次装七个包、都命中缓存时，npm 自己的 RSS 到了 1.77 GB 也被结束了。原因是[第二轮普查](70-survey-suites.md)的问题 5：每个进程各一份翻译。普查脚本改成一次开一个界面、一个一个装。
4. 与 tuxport 无关、原生上一样的：
   - Claude Code 的 Bash 工具要 `SHELL` 或者 bash，都没有时报 "No suitable shell found"。App 的登录环境设了 `SHELL`，没有问题。
   - Claude Code 首次启动时连 api.anthropic.com 检查连通性；不经代理连不上的地方显示 403，进不去界面。
   - Gemini CLI 要先在设置里选认证方式（`gemini-api-key`），不信任当前目录时 `--yolo` 不起作用；执行命令要 bash，Alpine 上要 `apk add bash`；它把 `$((…))` 当作命令注入拦下来。
   - Codex 的交互界面默认起一个后台服务，在 Alpine 上读不到进程的启动时间（busybox 的 ps），退出码 1，要加 `--no-daemon`。
   - GitHub Copilot CLI 在 Alpine 上起不来，报一串 `Node-API symbol … has not been loaded`。

## 测试

差分测试 compat 组读那四个文件，与原生相同，用改动之前的代码跑时读不到。普查脚本在 tuxport 的 Alpine、Debian 仓库和原生的 alpine:3.24 里各跑完一遍。cargo test、clippy、两层的差分测试和 LTP 与[下一轮](104-pty-hangup.md)的改动一起跑，结果记在那一篇。

## 推迟项

- Codex、Claude Code 的沙箱用不了，与在没有特权的 docker 容器里一样：没有用户命名空间（§6）。要关掉沙箱用：Codex 用 `--dangerously-bypass-approvals-and-sandbox` 或者 `-s danger-full-access`，Claude Code 的沙箱默认就是关的。 → 拆成几轮在做：仓库里的目录 bind、`MS_MOVE`、pivot_root 已补上（M9 之前：仓库里的目录 bind），tmpfs 也补上了（M9 之前：tmpfs），挂载命名空间也补上了（M9 之前：挂载命名空间），用户命名空间也补上了（M9 之前：用户命名空间），pid 命名空间也补上了（M9 之前：pid 命名空间），UTS、IPC 命名空间也补上了（M9 之前：UTS 和 IPC 命名空间），网络命名空间也补上了（M9 之前：网络命名空间）；端到端试过了，两家的沙箱在 tuxport 里都能用（M9 之前：bubblewrap 和 AI 编程 CLI 的沙箱）
- 内存：每个 CLI 的界面在 tuxport 里多占的是原生的 1.5 到 5 倍（Codex 最多：211 MiB 对 40 MiB），几个一起开，或者同时开着别的大程序，手机上容易碰到 App 的内存上限（[第二轮普查](70-survey-suites.md)的问题 5）。→ 代码缓存的两个视图原来各算一次，已改为只算一次（M9 之前：代码缓存只算一次），Codex 的界面 294 → 208 MB；剩下多的主要是 codex 本身的翻译（43.9 MB），要提高翻译代码的密度；密度提高之后 codex 的代码缓存 44.4 → 38.3 MB（M9 之前：翻译代码的密度），Codex 只有两个大进程，同一个程序共用翻译帮不上忙（M9 之前：同一个程序共用翻译）
- 只跑了无头模式和交互界面的启动画面，没有在交互界面里走完一轮对话；没有连真正的模型（没有账号）；Debian 上没有原生对照（开发虚拟机上 docker 的磁盘满了），Ubuntu 没跑；没上手机。
- `overflowuid` 这几个文件只读，Linux 上 root 可以改。没有用户命名空间，这几个值用不上。 → 用户命名空间补上之后用上了（M9 之前：用户命名空间）；仍只读
