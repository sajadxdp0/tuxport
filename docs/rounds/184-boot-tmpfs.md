# M9 之前：开机时挂好 /dev、/dev/shm、/dev/mqueue 和 /run（2026-11-02）

支持 OpenRC 的第一轮。调研时（Alpine 3.24、OpenRC 0.63.2）OpenRC 本身大多能用，有两处会把 tuxport 的目录盖住：

- `init.sh`（每次 `openrc sysinit` 都执行）看到 `/run` 不是挂载点，就在上面挂一个 tmpfs。之前写进 `/run` 的东西都看不到了：比如 App 先开的标签页、`/etc/profile.d` 里留的标记。
- devfs（`setup-alpine` 和 Alpine 的 wiki 都把它加进 sysinit）看到 `/dev` 不是挂载点，就挂一个空的 tmpfs，只补了 console、tty1、tty、null、kmsg 几个节点。ptmx、`/dev/pts`、urandom、zero、full 都没了：tmux、sshd 分配终端失败，seedrng、machine-id 打不开 urandom。原因是 M3b 定下的"静态 `/dev`"：`/dev` 是仓库里的目录。Linux 上内核开机时就在 `/dev` 上挂好 devtmpfs，容器运行时挂 tmpfs，devfs 只重新挂载。

## 做法

开机时的挂载改成与 systemd、容器运行时挂的一样（`Vfs::open`）：

| 挂载点 | 文件系统 | 标志、选项 |
|---|---|---|
| `/dev` | tmpfs | nosuid；`mode=755` |
| `/dev/pts` | devpts（不变，挂在 `/dev` 的 tmpfs 里） | nosuid、noexec；`gid=5,mode=620,ptmxmode=666` |
| `/dev/shm` | tmpfs | nosuid、nodev |
| `/dev/mqueue` | mqueue（开机时的 IPC 命名空间的） | nosuid、nodev、noexec |
| `/run` | tmpfs | nosuid、nodev；`mode=755`，物理内存的 20%，`nr_inodes=800k` |

- 标准的设备节点、`fd`/`stdin`/`stdout`/`stderr` 符号链接和 `pts`、`shm`、`mqueue` 三个目录在开机时建在 `/dev` 的 tmpfs 里（`dev::populate`，相当于 devtmpfs）。mknod 建的节点与 Linux 一样留不到下次。
- 仓库里的 `/dev` 目录是挂载点，里面照旧补齐同样的节点，卸下 `/dev` 之后看到的是它们。
- `/run` 的大小和 inode 数与 systemd、OpenRC 的 `init.sh` 挂的一样。OpenRC 看到 `/run` 已经挂着，就不再挂。
- `/dev/shm`、`/dev/mqueue` 的选项与 systemd 的相同。docker 的 `/dev/shm` 还加了 noexec 和 64 MB 的上限；这里与原来一样可以执行、不限大小，避免 JIT 双重映射之类的用法受影响。
- 挂载编号：原来的 1–6 不变（devpts 仍是 3），新的四个是 7–10。mountinfo 按挂上的先后列出：`/`、`/proc`、`/dev`、`/dev/pts`、`/dev/shm`、`/dev/mqueue`、`/sys`、`/sys/fs/cgroup`、`/run`。
- tmpfs 照常是仓库里不挂在根目录下面的树（[tmpfs](117-tmpfs.md)），不占 App 的内存。App 被杀掉留下的，在下次开机清理。开机时建三个这样的目录和十几个节点，是一个写事务。
- 仓库里的 `/dev/shm`、`/run` 照旧在开机时清空：以前的版本没有在上面挂 tmpfs，里面可能留着东西。
- 新建仓库、`/run` 不存在时补一个空目录（`ensure_dev_nodes`）。

devfs 现在打印 "Remounting tmpfs on /dev"，按它的选项重新挂载（`noexec`、`size=10M`），节点和终端都还在。

## 测试

- 单元测试：开机时的挂载树，`/dev` 的 tmpfs 里的节点和路径。
- `mount` 差分测试组新增一节：`/dev` 下面各个挂载的类型，照 OpenRC 的 devfs 重新挂载 `/dev` 之后的超级块选项、设备节点、`openpty` 和 urandom，与原生相同。原生的容器没有挂 `/run`，`/run` 不比。
- 手动（`target/openrc/devfs.sh`）：OpenRC 的 sysinit、boot 里加上 devfs、mdev、seedrng 等服务之后，`/dev` 的节点都在，tmux 能开会话，`/run` 没有被再挂一层；改之前 `/dev` 里没有 ptmx。

## 推迟项

- seedrng 仍然失败：`/dev/urandom` 的 `RNDADDENTROPY` 是 ENOTTY（下一轮）。→ 已补上（M9 之前：Alpine 开机时写的 sysctl、protected_symlinks/hardlinks 和随机数的 ioctl）
- mdev 的 `/proc/sys/kernel/hotplug` 写不了、没有 `/sys/dev`：tuxport 没有热插拔，不做，文档里说明。
