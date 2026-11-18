# M9 之前：Alpine 开机时写的 sysctl、protected_symlinks/hardlinks 和随机数的 ioctl（2026-11-02）

支持 OpenRC 的第二轮。开机运行级别里的两个服务在 tuxport 里报错：

- sysctl 服务执行 `/usr/lib/sysctl.d/00-alpine.conf`，其中 4 项在 tuxport 里没有：`kernel.panic`、`fs.protected_hardlinks`、`fs.protected_symlinks`、`kernel.unprivileged_bpf_disabled`。busybox 的 sysctl 用 `O_CREAT` 打开，procfs 里新建是 EACCES，报 "Permission denied"。
- seedrng（Alpine 默认在 boot 里）对 `/dev/urandom` 调 `RNDADDENTROPY`，把上次存下的种子加回熵池。tuxport 只实现了 `RNDGETENTCNT`，别的都是 ENOTTY，服务失败。

## 做法

### 随机数设备的 ioctl

按 Linux 6.1 的 `random_ioctl`（`syscall::fs::random_ioctl`）：

| 请求 | 行为 |
|---|---|
| `RNDGETENTCNT` | 256（不变） |
| `RNDADDTOENTCNT` | 要 `CAP_SYS_ADMIN`，负数是 EINVAL |
| `RNDADDENTROPY` | 要 `CAP_SYS_ADMIN`；熵的比特数是负数 EINVAL；数据要都读得到，否则 EFAULT（长度是负数也是 EFAULT） |
| `RNDZAPENTCNT`、`RNDCLEARPOOL`、`RNDRESEEDCRNG` | 要 `CAP_SYS_ADMIN`，别的什么也不做 |
| 别的（包括 `RNDGETPOOL`、tty 的 `TCGETS`） | EINVAL，与 Linux 一样不是 ENOTTY |

随机数取自宿主，熵池总是满的（与 Linux 5.18 起相同）。加进来的数据只读一遍、不用。

### 四个 sysctl

| 项 | 默认值 | 写入 |
|---|---|---|
| `kernel.panic` | 0 | int（`proc_dointvec`）。只记下：tuxport 的内核层没有 panic，内核 bug 只结束撞上它的进程（设计文档 §12） |
| `fs.protected_symlinks`、`fs.protected_hardlinks` | 1 | 0 或 1，别的 EINVAL |
| `kernel.unprivileged_bpf_disabled` | 2 | 要 `CAP_SYS_ADMIN`；0 到 2；改成 1 之后不能再改（EPERM），与 Linux 的 `bpf_unpriv_handler` 相同 |

- 四个文件的权限都是 0644。Linux 5.17 把 `protected_*` 挪进 fs/namei.c 之后从 0600 变成了 0644，6.1、6.8 都是 0644。
- 两个 `protected_*` 默认打开。Linux 内核自己的默认是 0，systemd、Android 的 init、Alpine 都设成 1，原生的虚拟机上也是 1。

### protected_symlinks、protected_hardlinks 的语义

只接受写入、不起作用，就是在安全设置上骗人。所以两个都按 Linux 6.1 实现了：

- **`may_follow_link`**（`Vfs::may_follow_link`）：
  - 只管路径的最后一个组件（`pick_link` 的 `WALK_TRAILING`），中间的不管。在 nosymfollow、`RESOLVE_NO_SYMLINKS` 的检查之前。
  - 链接的属主是跟随者（fsuid）就跟随。
  - 否则，粘滞位并且所有人可写的目录（/tmp）里的链接，只有属主与目录的属主相同时才跟随，不然 EACCES。root 也一样：这正是用来防 root 跟随别人放在 /tmp 里的链接。
  - `open(O_CREAT)` 经由悬空的链接新建，也在这里先被挡住。
- **不持 BKL 的路径解析**（`Cached::walk`）不知道凭据。遇到粘滞位、所有人可写的目录里属主与目录不同的链接，就交给正常的解析。
- **`may_linkat`**（`linkat`，在 EXDEV 之后、目录的写权限之前）：
  - 不是文件的属主、也没有 `CAP_FOWNER` 时，源要是普通文件，不是 setuid 的、不是可执行的 setgid 的，并且能读写。不然 EPERM。
  - 设备节点、FIFO 都不行。

### unprivileged_bpf_disabled 的作用

- 不是 0 时，与原来一样：没有 `CAP_BPF`/`CAP_SYS_ADMIN` 的进程，bpf(2) 一律 EPERM。
- 是 0 时，按 Linux 各个命令自己的检查：
  - PROG_LOAD 除了套接字过滤、cgroup skb 都要有能力。tuxport 没有这两种，是 EINVAL。
  - PROG_QUERY 要 `CAP_NET_ADMIN`，PROG_GET_FD_BY_ID 要 `CAP_SYS_ADMIN`（原来就有）。
  - PROG_ATTACH/DETACH 不另外检查。
  - OBJ_GET_INFO_BY_FD 不给指令和各种记录的个数（`bpf_prog_get_info_by_fd` 的 `goto done`）。

## 测试

- 新的差分测试组 `protected`（带 `CAP_SYS_ADMIN`）：四个 sysctl 的值和权限、protected_symlinks 和 protected_hardlinks 允许和挡住的各种情形、随机数的 ioctl，两个执行层都与原生逐字相同。`kernel.panic` 只比权限、是不是一个整数：它的值是宿主的配置，推送后 CI 的 runner 上是 -1（colima 是 0），第一次推送时这一组因此失败。
- rootfs 的集成测试新增一个，测 `sysctl -w` 写四项再读回、写入的限制、关掉 `protected_symlinks` 之后能跟随（容器里 `/proc/sys` 只读，写入比不了原生）。
- 手动用 Python 直接调 bpf(2)，看了 `unprivileged_bpf_disabled` 是 2 和 0 时 nobody 和 root 的加载、查询。
- 手动（OpenRC，`target/openrc/devfs.sh`）：boot 里的 sysctl、seedrng 都是 `[ ok ]`。
