# M9 之前：bubblewrap 和 AI 编程 CLI 的沙箱（2026-10-30）

让 bubblewrap 能用的最后一轮：前六轮补上了仓库里的目录 bind、tmpfs 和挂载、用户、pid、UTS、IPC、网络命名空间，这一轮直接用 bwrap 和 [AI 编程 CLI](103-ai-cli-survey.md) 的沙箱端到端地试，修掉试出来的三个问题，再加一个 bwrap 的差分测试组。

## 做法

试出来的问题：

1. `bwrap --unshare-all` 看到 `/proc/self/ns/cgroup` 就带上 `CLONE_NEWCGROUP`，clone 是 EPERM。补上 cgroup 命名空间（`crate::nsproxy::CgroupNs`）：tuxport 里所有进程都在同一个 cgroup "/" 里，从哪个命名空间看都是根，所以它只有编号和属主；clone、unshare、setns（`cgroupns_install`，两处的 `CAP_SYS_ADMIN`）、`/proc/<pid>/ns/cgroup` 与别的命名空间一样。
2. bwrap 挂载时源和挂载点都经由 `/proc/self/fd/N` 给出，挂完之后用 readlink 那个 fd 得到的路径在 mountinfo 里找这个挂载、再改成只读。挂载点是 tmpfs 里的文件（`--dev` 里的 `/dev/null` 这些、`--ro-bind` 文件）时找不到：跟随魔法链接到了文件，丢了它所在的目录和名字，挂载记不下挂载点的路径（不是目录的文件没有唯一的上级目录，[仓库里的目录 bind](116-store-binds.md)那一轮起靠这个算 mountinfo 的路径），bwrap 报 `Unable to find "/newroot/dev/null" in mount table`。现在经由路径打开的文件记下最后经过的目录和名字（`OpenFile::dentry`），跟随 `/proc/<pid>/fd/<n>` 到不是目录的文件时交出来（`Pseudo::follow_dentry`）。
3. 经由魔法链接给出被盖住的节点时，remount 原来也像新挂载一样找挂在最上面的那个（Linux 的 `lock_mount` 只对新挂载这样），于是 bind 之前打开的 fd 也 remount 得了；Linux 上它不是挂载的根，是 EINVAL。改成 remount 按解析出来的节点（bwrap 是 bind 之后重新打开再 remount 的，原来就能用）。

## 结果

普查用 `tests/survey/ai/agents.sh`（做法见 [AI 编程 CLI](103-ai-cli-survey.md)）：在 tuxport 的 Alpine 3.24 仓库里跑，原生对照是 alpine:3.24 容器；`scripts/survey` 新增 `SURVEY_DOCKER_OPTS`，原生的容器加上 `--cap-add SYS_ADMIN --cap-add NET_ADMIN`、不限制 AppArmor 和 seccomp、`--security-opt systempaths=unconfined`（docker 默认盖住 `/proc` 下的几个路径，Linux 的 `mount_too_revealing` 因此不让在用户命名空间里挂新的 procfs，bwrap 报 `Can't mount proc on /proc`）。

| 项目 | tuxport | 原生 |
|---|---|---|
| Codex 0.166 `exec -s workspace-write`（每条命令都经由 bwrap，见下） | 8 步全部成功，11 秒 | 8 步全部成功，9 秒 |
| `codex sandbox -- sh -c 'echo inside'` | inside | inside |
| `bwrap --ro-bind / / --dev /dev --proc /proc true` | 成功 | 成功 |
| Claude Code 2.1.314 的 `sandbox.enabled` | 8 步全部成功，4 秒 | 执行命令的 3 步失败（见下） |

上一轮之前这几项在 tuxport 里都是 `bwrap: No permissions to create a new namespace`。Codex 实际的调用是 `bwrap --as-pid-1 --new-session --die-with-parent --tmpfs / --dev /dev --ro-bind /bin /bin --ro-bind /etc /etc --ro-bind /lib /lib --ro-bind /sbin /sbin --ro-bind /usr /usr --unshare-user --unshare-pid --unshare-ipc --unshare-net --proc /proc --cap-drop ALL -- 命令`，`-s workspace-write` 时再 `--bind` 工作目录。在 `codex sandbox` 里试了一下关不关得住：写 `/root`、工作目录都是只读文件系统（它默认的策略是只读），netlink socket 被 Codex 自己的 seccomp 过滤器拒绝，域名解析不了。

Claude Code 的沙箱在原生容器里失败，报 `apply-seccomp: write /proc/self/setgroups (nested userns is capability-restricted; caller must provide CAP_SYS_ADMIN)`：它在 bwrap 建的用户命名空间里再建一层，Ubuntu 24.04 的 `kernel.apparmor_restrict_unprivileged_userns` 不让这样的命名空间里有能力（[用户命名空间](119-user-namespaces.md)那一轮遇到的同一个限制）。tuxport 照上游 Linux 的行为，这一步成功。

三个 CLI 不开沙箱时、七个 CLI 的交互界面都与上次一样，与原生相同（npm 这次装到了 Claude Code 2.1.314、Codex 0.166）。

## 测试

- 新的差分测试组 `bwrap`（`scripts/difftest` 的软件包加上 bubblewrap；原生一侧的 docker 选项同上）：bwrap 建的挂载和只读、`--unshare-all --hostname` 里看到的、Codex 那组参数下的环境、`--uid`、参数错误，两个执行层下都与原生相同。附加组、不在新 pid 命名空间里时的 pid、外面带进来的挂载两边环境不同，不比。
- 差分测试 `namespaces` 组加上 cgroup 命名空间和 bwrap 的挂法（经由 `/proc/self/fd/N` 挂到 tmpfs 里的文件上、bind 前后打开的 fd 的 remount）。

## 推迟项

- 没有 `mount_too_revealing`（[用户命名空间](119-user-namespaces.md)的推迟项）：tuxport 的 procfs 上本来没有盖住的路径，看不出区别。
- 没有 time 命名空间（`CLONE_NEWTIME` 仍是 EPERM），bwrap、Codex、Claude Code 都不用。 → 已补上（M9 之前：time 命名空间）：dockerd 看到 `/proc/self/ns/time` 就让 runc 建一个
- Claude Code 沙箱里的网络代理、Codex `-s workspace-write` 下写工作目录以外这些关不关得住，只在 `codex sandbox` 里手工试过，普查的 8 步里没有。
