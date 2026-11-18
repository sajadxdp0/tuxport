# M9 之前：兼容性补漏（2026-10-25）

把推迟项里程序会直接碰到的接口缺口补上一轮：

## 做法

- **`/proc/<pid>/maps`**：所有文件映射（程序、解释器、mmap 的库和文件）都显示路径、文件偏移、设备号和 inode，与 stat 一致。原来只有程序和解释器有名字，vDSO 那一行还被错标成解释器。
- **brk 和 VMA**：堆第一次扩展时预留 256 MB 的宿主地址空间（`MAP_NORESERVE`，不碰不占内存），之后的扩展只是加长同一个 VMA；收缩后再扩展的页清零，fork 出来的子进程照样预留。maps 里相邻、权限相同的匿名映射（以及同一个文件里接着的部分）与 Linux 一样合成一段：每次匿名 mmap 都是单独的宿主内存，VMA 本身合不起来。python 跑一会儿，maps 从 276 行变成 40 行左右。
- **实时信号排队**：挂起的信号从位图换成 Linux 的 `sigpending` 结构，标准信号同一种只挂一个，实时信号每次产生都排队，按先后投递，各带各的 siginfo。一个线程组最多排 `RLIMIT_SIGPENDING` 个（默认 16384，与 4 GB 内存的 Linux 相同）；排满时 sigqueue 返回 EAGAIN，kill 照样挂起但丢掉 siginfo。`/proc/<pid>/status` 的 SigQ 是真实的数字。
- **pidfd**：`pidfd_open`、`pidfd_send_signal`（也接受打开的 `/proc/<pid>` 目录）、`pidfd_getfd`（按 ptrace 的规则检查权限）、clone 的 `CLONE_PIDFD`、`waitid(P_PIDFD)`（非阻塞的 pidfd 没有事件时是 EAGAIN）。pidfd 拿着线程组的弱引用，pid 分给别的进程之后仍然指向原来那个；所有线程退出时 poll 可读，fdinfo 有 `Pid:`。另外实现了 `clone3`，包括 `CLONE_CLEAR_SIGHAND`；指定 pid 的 `set_tid` 和 `CLONE_INTO_CGROUP` 返回 EINVAL（→ `set_tid` 已补上（M9 之前：pid 命名空间））。Go 探测到这些之后，os/exec 改用 pidfd 等子进程、发信号（strace 里能看到 `waitid(P_PIDFD)` 和带 `CLONE_PIDFD` 的 vfork），Go 的差分测试照样通过。
- **`execveat`**：路径相对 dirfd，`AT_EMPTY_PATH` 执行 fd 本身（fexecve），`AT_SYMLINK_NOFOLLOW` 遇到符号链接是 ELOOP。新程序看到的 `AT_EXECFN` 是 `/dev/fd/<n>[/<路径>]`，comm 取文件名；用 close-on-exec 的 fd 执行脚本是 ENOENT（解释器打开它时 fd 已经关了），与 Linux 相同。
- **`/proc/net/{tcp,tcp6,udp,udp6,unix}`**：遍历所有进程的 fd 表收集 socket。TCP、UDP 的地址、协议和 TCP 状态向宿主 socket 查，不读宿主的 `/proc/net`（Android 10 起 App 读不了）；格式照抄 Linux，包括 IPv4 的两个文件每行补空格到固定宽度。只在 SCM_RIGHTS 消息里、或者还在 accept 队列里的 socket 不列出。→ `/proc/net/unix` 后来列出了这两种（M9 之前：推迟项）
- **`/proc/<pid>/mountinfo`** 和 epoll、timerfd、signalfd 的 fdinfo：挂载编号与 fdinfo 的 `mnt_id` 一致，设备号与 stat 的 `st_dev` 一致。
- **零碎的**：
  - `/proc/sys/fs/pipe-max-size` 可写并且生效，没有 `CAP_SYS_RESOURCE` 时新管道的容量不超过它（LTP fcntl35）。
  - 老式的 `TCGETA`/`TCSETA*`（ioctl01）。
  - `/dev/urandom` 的 `RNDGETENTCNT` 和 `entropy_avail`（ioctl07）。
  - 带偏移的读写遇到不能定位的文件先报 ESPIPE，再看打开方式（pwritev202）。
  - `MADV_REMOVE`：有锁住的页或私有匿名映射是 EINVAL，不能获得写权限的是 EACCES，共享映射清零（madvise02）。
  - `MREMAP_DONTUNMAP`：与 5.7–5.12 一样只支持私有匿名映射，原地址留下空的映射。
  - 读写 epoll 和 pidfd 是 EINVAL。
  - kcmp（LTP 的 pidfd_getfd01 用它核对复制来的 fd）：文件、地址空间、fd 表、fs、信号处理表比较对象本身，`KCMP_EPOLL_TFD` 到 epoll 里找目标文件；tuxport 没有 IO 上下文，SysV 信号量的 undo 表也不在进程之间共享，这两种总是相同。
- LTP 的 sigtimedwait01、sigwaitinfo01 超时不是 tuxport 的问题：第一项测试等的是"空集合被信号打断返回 EINTR"，而 musl 的 `sigtimedwait` 遇到 EINTR 会自己重试，在原生 Alpine 上一样卡住；直接发 syscall 的 rt_sigtimedwait01 是通过的。
- 测试：差分测试新增 `compat`（以上各项，只比较与环境无关的列），在两个执行层下都和原生一致。原生一侧的容器改为不加 seccomp 过滤：Docker 默认的规则让 clone3 返回 ENOSYS、pidfd_getfd 返回 EPERM，比对的应当是内核本身的行为。内核版本相关的部分（6.9 起 pidfd 的几处变化）只测各版本相同的。单元测试新增堆的扩展、maps 的合并和信号的排队。LTP 新通过 execveat01/02、pidfd 的 8 个、kcmp01–03、fcntl35、ioctl01、ioctl07、madvise02、pwritev202 和 splice06；getrusage03 由跳过变成失败（上次虚拟机内存不足没有运行），暴露出 `ru_maxrss` 总是 0。

## 推迟项

- pidfd：`PIDFD_THREAD`、`/proc/sys/kernel/ns_last_pid`（pidfd_send_signal03 要用它让新进程拿到同一个 pid）。→ `ns_last_pid` 已补上（M9 之前：推迟项）；`PIDFD_THREAD` 是 6.9 的功能，tuxport 报告的是 6.1，不做
- 不产生 core dump，`WCOREDUMP` 总是 0（waitid10）。→ 已补上（M9 之前：core dump）
- getrusage 的 `ru_maxrss` 总是 0（getrusage03）。→ 已补上（M9 之前：剩下的小缺口）
- `MREMAP_DONTUNMAP` 不支持文件映射；没有 `/proc/net/{icmp,raw}`。→ 都已补上（M9 之前：推迟项）
