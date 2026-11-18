# M9 之前：glibc 发行版的探测（2026-10-28）

guest 里跑过的都是 Alpine 的 musl 程序（§1.1 的目标），glibc 的用户态从来没在 tuxport 里跑过。手机终端的用户常常需要 glibc：pip 上没有 musl 版本的 wheel、VS Code 的服务端、各种预编译好的二进制，Termux 的用户大量用 proot-distro 跑 Debian、Ubuntu 也是为了这个。这一轮看两件事：glibc 的程序本身在 tuxport 里能不能用；从 Alpine chroot 进一个 glibc 发行版缺什么，以此决定要不要做 mount 的一个子集。

## 做法

把 `debian:trixie`、`ubuntu:24.04` 的 arm64 镜像用 `docker export` 导出、gzip 压缩，`tuxport-run --import` 直接建成仓库，guest 的根就是 Debian 或 Ubuntu。负载是 `tests/survey/glibc/*.sh`，经由 `scripts/survey -s <仓库> glibc/<名字>` 运行（普查的负载名可以带子目录了）。chroot 用一个新导入的 Alpine 仓库，在 guest 里把同一个 Debian 的 tarball 解到 `/debian`。都在 colima 的虚拟机里用 Tier 1 跑。

工具：`tests/survey/glibc/` 里 `debian-install`（先运行，装后面要的软件包）、`debian-c`、`debian-py`、`debian-java`、`debian-node`、`debian-rust`、`debian-code` 和 `ubuntu`；建仓库的命令写在 `debian-install.sh` 的开头。

## 结果

### 以 glibc 发行版为根，都正常

| 负载 | 跑了什么 |
|---|---|
| Debian 13（glibc 2.41） | apt 3.0 装了 571 个包（OpenJDK 的安装脚本会运行 java）；ps、free、df、getent（NSS、经由 DNS 存根解析）、locale-gen、iconv、file |
| gcc 14.2、g++、gdb 16.3、strace 6.13 | 多线程的 C、C++ 程序（`std::thread`、`std::async`、异常、`<regex>`）；gdb 在线程里的断点、`info threads`（libthread_db 照常工作）；`strace -f -c` |
| Python 3.13 | venv 里 pip 装 numpy 2.5、pandas 3.1（manylinux 的 aarch64 wheel，带 OpenBLAS）：矩阵乘法、特征值、groupby，`multiprocessing.Pool`、线程、sqlite3、ssl |
| Node.js | Debian 的 node 20；npm 装 sharp（预编译的 glibc 版 libvips）生成 PNG；nodejs.org 的官方 glibc 构建 v24.22.1（xz 解包、`worker_threads`）和它自带的 npm |
| Rust | rustup 装 glibc 的工具链 1.99（`aarch64-unknown-linux-gnu`），cargo 拉 regex 编译、运行多线程程序 |
| OpenJDK 21 | javac、线程池、并行流；默认最大堆按 cgroup 的 `memory.max` 定（490 MiB） |
| code-server 4.143（VS Code 1.143） | 安装脚本装独立版（自带 glibc 的 node）；在 Chrome 里连上去：资源管理器、扩展宿主（git 扩展找到了仓库）、集成终端（node-pty，`/dev/pts/0`，shell integration 是 Rich）、在终端里新建的文件自动出现在资源管理器里（@parcel/watcher 走 inotify）、在编辑器里改了保存，终端里 `cat` 看到的一致；SIGTERM 正常退出 |
| Ubuntu 24.04（glibc 2.39） | apt 装 python3、gcc，pthread 程序、python 的线程池和 ssl、ps、free、curl |

内核层不用改。未实现的 syscall 有五个，都与没配对应功能的 Linux 6.1 一样返回 ENOSYS，程序照常退回别的做法：293（rseq，glibc 2.35 起每个线程开始时注册一次，不成功就不用）、235（mbind，numpy 带的 OpenBLAS 探测 NUMA）、425（io_uring_setup，libuv）、444（landlock_create_ruleset，xz 5.6 的沙箱）、452（fchmodat2，6.6 才有，glibc 2.39 起 `fchmodat` 带 `AT_SYMLINK_NOFOLLOW` 时先试它，再退回 `O_PATH` 加 `/proc/self/fd`）。

### 碰到的两处问题，都不在 tuxport

1. **docker export 出来的 rootfs 里 `/etc/resolv.conf`、`/etc/hosts` 是空文件**（docker 运行容器时才 bind 挂载真正的）。内核层只在 resolv.conf 不存在时写 DNS 存根（§10），空的 resolv.conf 让 glibc 去问 127.0.0.1:53，没有回答，与 Linux 上一样解析失败；`getent hosts localhost` 也是空的。探测脚本自己补上这两个文件。
2. **VS Code 的第一个终端刚打开时，最先输入的 5 个字符丢了一次**。PTY 这一层与原生相同：python 的 `pty.fork()` 起 bash，bash 起来之前就写进主设备的一行，tuxport 和原生的 Debian 上都完整执行了。第二个终端打开 1 秒后就输入，没有丢。丢字发生在终端子系统整个初始化的时候，多半是 VS Code 在终端进程准备好之前没有收下输入；没有和原生的 code-server 对照。

### 从 Alpine chroot 进去

`chroot /debian` 之后 `/proc` 是空目录，`/dev` 里只有 docker 留下的 `console`（一个空的普通文件）和空的 `pts`、`shm`，`/dev/null` 第一次被写时成了普通文件，没有 `/sys`。apt-get update 仍然能用，ps、top、free 这些读 `/proc` 的都不行。util-linux 的 mount 先试新的挂载接口（430 fsopen、428 open_tree），ENOSYS 之后退回 mount（40），也是 ENOSYS。

### mount 的子集先不做

- chroot 进别的发行版的脚本（debootstrap、arch-chroot 和各种教程）要的是：在任意目录上挂 proc、sysfs、devpts，挂 tmpfs（`/dev/shm`、`/run`），以及 bind 挂载（`mount --rbind /dev`、把 `/root` 这类目录共享进去）。
- 现在的挂载表是固定的（§8.2）：每种合成文件系统只有一个挂载点，路径里的节点不带"经由哪个挂载点进来"。只挂合成文件系统要给它们的节点记上挂载点，`..` 和 getcwd 才对（chroot 在 `/debian` 时，从 `/debian/proc` 往上 `..` 现在会到 `/`，越出 chroot），工作量中等。bind 挂载和 tmpfs 要把路径改成（挂载，节点）的二元组，当前目录、根目录、打开文件记的路径、`/proc/<pid>/{cwd,root,fd}`、mountinfo、inotify、AF_UNIX 的路径名都要跟着改，工作量大。
- 上面的结果说明 glibc 发行版直接当作仓库就能完整地用，内核层一行都不用改。给用户 glibc 环境更便宜的路是 App 支持用 Debian、Ubuntu 的 rootfs 建仓库（首次启动时选，或者"重置 rootfs"时换，§11.4），要做的是准备 rootfs（上面的 resolv.conf、hosts，profile）和镜像列表。用户想同时用 Alpine 和 Debian、在两边共用文件时，再回来做 mount。 → 已按这个做法补上（M9 之前：App 里选 rootfs）：首次启动可以选 Debian 13、Ubuntu 24.04，选了才下载

## 推迟项

- 没有 mount、umount2、pivot_root（ENOSYS）：chroot 进别的发行版缺 `/proc`、`/sys`、`/dev`，做法和工作量见上。 → 先做了 App 里选 rootfs（M9 之前：App 里选 rootfs）；之后做了第一档（M9 之前：mount 的子集）：procfs、sysfs、devpts、cgroup2 可以挂到任意目录上，它们里面的目录可以 bind，chroot 进去之后 ps、top、free 能用了；仓库里的目录的 bind、pivot_root 后来也补上了（M9 之前：仓库里的目录 bind），tmpfs 也补上了（M9 之前：tmpfs）
- 导入 docker export 出来的 rootfs 时，空的 resolv.conf、hosts 要由导入的一方写好：内核层只在 resolv.conf 不存在时写存根，App 首次启动写的 hosts 也是已有就不动（§11.4）。 → App 导入 Debian、Ubuntu 时写好了（M9 之前：App 里选 rootfs）
- rseq 没有实现：glibc 不注册也照常工作；用 rseq 做每个 CPU 缓存的程序（新版 tcmalloc）退回别的做法。
- VS Code 第一个终端刚打开时丢字的那一次没有和原生的 code-server 对照。→ 已对照（M9 之前：补测待查的几项）：原生的 code-server 上同样丢字，而且丢得更多，是 VS Code 在终端起来之前不收输入
- 没跑的：conda/miniforge、Ubuntu 上更多的负载、Tier 0；都只在 colima 的虚拟机里跑，没上手机。→ conda 能用，Alpine 普查的 12 个负载在 Tier 0 下与 Tier 1 一致，Ubuntu 上补了 su 和 sshd（M9 之前：补测待查的几项；补的时候发现并修好了 PAM 登录不了，见 M9 之前：没有实现的 netlink 协议建不了）；仍没上手机
