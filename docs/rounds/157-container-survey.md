# M9 之前：podman 和 docker 的普查（2026-11-01）

设计文档 §1.2 把 docker、podman 放在 D 档（不做），理由是"namespace、cgroup"。命名空间那一半后来为 bubblewrap 做了（除 time 以外都有，[bubblewrap](123-bwrap.md)），容器服务又很常见，这一轮普查现在离能跑容器还有多远：先用最省事的一组参数（vfs 存储驱动、host 网络、不用 cgroup），看依次撞到什么。

## 做法

- 仓库：Alpine 3.24 的 minirootfs 各建一个。`target/survey/podman` 装 podman 5.8.9、crun 1.30.1、conmon、netavark、passt；`target/survey/docker` 装 docker 29.9.2、containerd 2.3.7、runc 1.4.3。两个分开，是因为 Alpine 的 runc 与 crun 冲突，装了 runc 会换掉 crun。镜像从 docker.io 拉（经由开发机的代理）。
- 负载 `tests/survey/podman.sh`：`/etc/containers/storage.conf` 改成 vfs，`podman run --network host --cgroups disabled`。测了退出码、pid 1、挂载表、能力和 seccomp、环境变量和工作目录、bind 和具名卷、`--tmpfs`、`--read-only`、`--user`、后台运行加 exec、logs、stop、信号、Debian 13 的镜像、`podman build`（默认的、`--runtime-flag cgroup-manager=disabled` 的、`--isolation chroot` 的）、save 和 load、`--uidmap`、`--pid host`、端口发布、pasta、`--memory`、默认网络，以及起一个容器的时间。
- 负载 `tests/survey/docker.sh`：`daemon.json` 设 vfs、`"bridge": "none"`、不用 iptables，起 dockerd，`docker run --network host`。
- 交互式的 `podman run -it` 单独试：让它直接作为 `tuxport-run --pty` 的程序，从管道送命令。
- 原生对照：同一版本的 Alpine 镜像装好 podman、docker，在 `--privileged` 的容器里跑同样的负载（`scripts/survey -n`）。
- 撞到问题时，原因一看就清楚、改动小的，先在工作区里改掉（问题 1–4，还没提交）；要大改的先临时绕过（把 mqueue 当作空的 tmpfs、允许可写地挂 cgroup2、藏起 `ns/time`），看后面还有什么，最后都撤掉了。
- 都在开发用的虚拟机里跑，没上手机。

## 结果

### podman

- 不改的话，每个 podman 命令一开头就是 `unable to get capability version from the kernel: invalid argument`（问题 1）；改了之后，每个容器都在 crun 挂 mqueue 时失败（问题 5）。
- 改掉问题 1–4、临时绕过问题 5、6 之后，下面这些与原生的输出相同：容器的输出和退出码、pid 1、`--user`、环境变量和工作目录、bind 和具名卷（容器里写的宿主上看得到）、`--tmpfs`、`--read-only`（EROFS）、能力集合（`CapEff` 是 podman 默认的 `800405fb`）和 seccomp 过滤器、后台运行的容器上 exec、logs、stop、按 SIGTERM 处理信号、Debian 13（glibc 2.41）的镜像、加 `--runtime-flag cgroup-manager=disabled` 的 `podman build`（RUN 一步在容器里跑 `apk`）、save 和 load。
- 交互式的 `podman run -it`：`tty`、`TERM`、`stty size`（24 80）正常，`exit 5` 的退出码传了出来。
- 起一个容器（`podman run --rm alpine true`）0.30 秒，原生 0.07 秒。
- 与原生不同、属于环境的：挂载表里根文件系统和 bind 进来的文件是仓库（`ext4`，源是 `tuxport`），原生是 overlay；crun 遮住的 `/proc/kcore`、`/proc/timer_list` 这些路径 tuxport 的 procfs 里本来就没有，crun 跳过了它们；crun 挂 cgroup2 时用的 source 名字不同。
- 与原生一样失败的（podman 自己的限制）：不用 cgroup 时 `podman top`、`--pid host` 都不行，runc 也不让用；pasta 只给无 root 的 podman 用。
- 用了 `--uidmap` 的容器在原生上能跑，tuxport 里 crun 报 `cannot stat … userdata/resolv.conf: Permission denied`（问题 10）。

### docker

- dockerd 起得来，`docker info` 报告 vfs、cgroupfs、cgroup v2、runc，拉镜像正常。
- `docker run --network host` 依次撞到问题 2、7、3、4、8（问题 7 是临时绕过的），现在停在问题 8。按 runc 的做法，之后就要建容器的 cgroup（问题 9），没有跑到。
- 原生对照里 dockerd 嵌套在容器中，启用控制器失败（`cgroup.subtree_control` 写不了）、找不到 `memory.events`，都只记一条错误，容器照常运行：docker 不一定要各个控制器都能用，有一棵能建目录、能写 `cgroup.procs` 的 cgroup 树也许就够了。

### 未实现的系统调用

219（keyctl）、293（rseq）、428（open_tree）、430（fsopen）、442（mount_setattr）、452（fchmodat2）、458（listmount）、60（quotactl），都是以前记过的，程序都退回了别的做法；open_tree 在用户命名空间里退不回去（问题 10）。

### 找到的问题

小的（普查时在工作区里改掉，下一轮补了测试）：

1. **capget 探测版本**：数据指针为空、版本不认识时，Linux 写回内核的版本之后返回 0（`(dataptr == NULL) && (ret == -EINVAL) ? 0 : ret`），也不看 pid；tuxport 返回 EINVAL。moby/sys/capability 就这样取版本，podman、buildah、runc 都用它。→ 已改（M9 之前：容器运行时碰到的四处小问题）
2. **初始网络命名空间里拉起 lo**：dockerd 给容器的默认网络沙箱拉起 lo（`loopbackUp`，`RTM_NEWLINK` 带 `IFF_UP`），tuxport 对开机时命名空间里的所有修改请求都回 EOPNOTSUPP，dockerd 报 `failed to create default sandbox: operation not supported`。Linux 上 lo 本来就起着，这是个什么也不改的请求。改法：什么也不改的 `RTM_NEWLINK`、`RTM_SETLINK` 照 Linux 成功，真要改的仍是 EOPNOTSUPP。→ 已改（M9 之前：容器运行时碰到的四处小问题）
3. **合成文件系统根目录的 inode 编号**：procfs、sysfs、cgroup2、devpts 的根目录在 Linux 上都是 1，tuxport 是哈希值。runc（pathrs-lite 的 `verifyProcRoot`）打开 `/proc` 之后检查它，报 `unsafe procfs detected: incorrect procfs root inode number`。→ 已改（M9 之前：容器运行时碰到的四处小问题）
4. **线程目录里没有 fd、fdinfo**：`/proc/<pid>/task/<tid>/` 里缺这两项（Linux 有），runc 经由 `/proc/thread-self/fd/` 读自己的 fd 的链接，得到 ENOENT。→ 已改（M9 之前：容器运行时碰到的四处小问题）

大一些的，按建议的顺序：

5. **mqueue 文件系统**（[UTS 和 IPC 命名空间](121-uts-ipc-namespaces.md)的推迟项）：OCI 的默认挂载里有 `/dev/mqueue`，crun 给每个容器都挂，tuxport 是 ENODEV，podman 的容器一个也起不来。要做一个按 IPC 命名空间列出 POSIX 消息队列的文件系统（读文件得到 `QSIZE:… NOTIFY:…`，可以建、删）。→ 已补上（M9 之前：mqueue 文件系统）
6. **可写地挂 cgroup2**：crun 挂容器里的 `/sys/fs/cgroup` 时不带只读，tuxport 对可写的 cgroup2 挂载回 EROFS（[mount 的子集](105-mount-subset.md)："cgroup2 只能只读"）。可以先允许可写地挂上，写文件、建目录仍是现在的 EROFS，问题 9 再改。→ 已改（M9 之前：可写地挂 cgroup2），里面照旧改不了；里面也能改了（M9 之前：cgroup 树：建子 cgroup、迁移进程）
7. **time 命名空间**：tuxport 有 `/proc/<pid>/ns/time`，dockerd 据此在规范里加上 time 命名空间；runc 再检查 `/proc/self/timens_offsets`，没有就报 `time namespaces aren't enabled in the kernel`。tuxport 的 `CLONE_NEWTIME` 是 EPERM，没有 `timens_offsets`，与哪种 Linux 都对不上：配了 `CONFIG_TIME_NS` 的都有，没配的连 `ns/time` 也没有、`CLONE_NEWTIME` 是 EINVAL。两条路：做 time 命名空间（时钟都经过内核层，CLOCK_MONOTONIC、CLOCK_BOOTTIME 加偏移不难，要把绝对时间的定时器、`/proc/uptime` 这些都换算），或者像没配的内核一样去掉 `ns/time`。→ 已补上（M9 之前：time 命名空间）：做了 time 命名空间
8. **经由 fd 执行 memfd、已删除的文件**（[/proc/<pid>/exe 指向程序文件本身](153-exe-file.md)的推迟项）：runc 把自己复制进 memfd 再经由 `/proc/self/fd/<n>` 执行，得到 `fork/exec /proc/self/fd/6: permission denied`；buildah 的 chroot 隔离（containers/storage 的 `pkg/unshare`，memfd 加 fexecve）得到 `Error during reexec(...): No such file or directory`。153 篇说"用到它的多是 runc、systemd 这类 D 档的程序"，现在容器就要用。→ 已补上（M9 之前：经由 fd 执行 memfd 和已经删除的文件）
9. **可写的 cgroup v2**：podman 不加 `--cgroups disabled` 时、`podman build` 不加 runtime 参数时、`--memory` 这类限制、以 root 运行的 runc（docker）都要建子 cgroup、写 `cgroup.procs`、启用控制器。可以分两步：先只有层级和迁移（原生的 docker 在没有控制器时也能跑），再让 `pids.max`、`memory.max`（接 OOM killer）、`cgroup.freeze`、`cgroup.kill` 真正生效；`cpu.max`、`io.max` 只能记下，宿主线程的调度不归 tuxport 管。→ 第一步已做（M9 之前：cgroup 树：建子 cgroup、迁移进程），限制还不起作用；`pids.max`、`memory.max` 起作用了（M9 之前：cgroup 的 pids.max 和 memory.max 起作用），`cgroup.freeze` 还没有；`cgroup.freeze` 也有了（M9 之前：cgroup.freeze）
10. **open_tree、move_mount**（[未实现的系统调用](139-unimplemented-syscalls.md)里的新挂载接口）：容器有用户命名空间（`--uidmap`、`--userns`）时，crun 在命名空间外面先用 `open_tree(OPEN_TREE_CLONE)` 打开要 bind 的文件，把 fd 交给里面（crun 1.30.1 的 `prepare_and_send_mount_mounts`）。open_tree 是 ENOSYS 时它就在用户命名空间里面按路径 stat，`/run/containers` 是 0700、属于没有映射的 root，得到 EACCES。原生上目录权限一样，靠的就是 open_tree。→ 已改（M9 之前：open_tree、move_mount 和 mount_setattr），`--uidmap` 的容器起得来了
11. **devpts 的新实例共用终端**（[mount 的子集](105-mount-subset.md)的推迟项）：容器里 `tty` 是 `/dev/pts/1`，`ls /dev/pts` 看得到宿主的 0 号终端；Linux 上 `newinstance` 的 devpts 有自己的一套，编号从 0 开始。到了容器里这就是隔离上的漏洞。→ 已改（M9 之前：devpts 的实例）
12. **容器网络**：默认的 bridge 网络、端口发布要 iptables 或 nftables（netfilter 的 netlink 是 EPROTONOSUPPORT）、bridge 和 veth（[网络命名空间](122-net-namespaces.md)的推迟项），无 root 的 podman 用的 pasta 要 tun。最大的一块，可以一直只用 host 网络。→ 普查了 bridge 网络的依赖（M9 之前：容器网络的普查），按地址映射的方案做

## 推迟项

- 没测的：无 root 的 podman（普通用户、newuidmap）、compose、buildah 单独用、quadlet 这类要 systemd 的（D 档）、大的镜像（vfs 每层完整复制，占空间）、在手机上跑。
- 起一个容器是原生的 4 倍多，没有剖析时间花在哪里。
