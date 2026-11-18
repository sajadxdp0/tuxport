# M9 之前：推迟项（2026-10-25）

把前几轮推迟的、在单进程模型里做得了的补上；io_uring 不做（见下面的推迟项和 §15）。

## 做法

- **信号驱动的 I/O**：`O_ASYNC`（`F_SETFL`、`FIOASYNC`），属主（`F_SETOWN`、`F_SETOWN_EX`，socket 上还有 `FIOSETOWN`/`SIOCSPGRP`）和 `F_SETSIG` 原来只记录，现在生效：文件变得可读、可写、出错或挂断时给属主（线程、进程或进程组）发 SIGIO，或者 `F_SETSIG` 设置的信号，带 `si_band`、`si_fd`（signalfd 也给出这两项），band 与 Linux 相同。读过之后仍然可读的（又来了数据）再通知一次。检查放在定时器线程上：内核状态的任何修改都会叫醒它，它顺便检查登记了 `O_ASYNC` 的文件，宿主 socket 的就绪也由它一起 poll。
- **文件租约**：`F_SETLEASE`、`F_GETLEASE`、`/proc/sys/fs/lease-break-time`。规则与 Linux 相同：只有属主（或有 `CAP_LEASE`）能设置；读租约要求自己不可写、没有人以写方式打开，写租约要求没有别的打开。以写方式打开或者截短打破所有租约，以读方式打开打破写租约：持有者收到 `F_SETSIG` 的信号（`POLL_MSG`）或 SIGIO；`O_NONBLOCK` 的 open 返回 EWOULDBLOCK，否则等持有者降级或放弃，超过 lease-break-time 强制降级。被写方式的打开打破时不能降成读租约。
- **POSIX 消息队列**：`mq_open`、`mq_unlink`、`mq_timedsend`、`mq_timedreceive`、`mq_notify`、`mq_getsetattr`；按优先级排序，同一优先级先进先出；`/proc/sys/fs/mqueue/*` 可写，属性按它们和 `CAP_SYS_RESOURCE` 检查。队列的 fd 能 poll、epoll，read 给出与 Linux 相同的状态行。`mq_notify` 支持 `SIGEV_SIGNAL`、`SIGEV_NONE` 和 musl 的 `SIGEV_THREAD`（经由 netlink socket 送 cookie，与 Linux 相同）。队列在内存里，App 进程重启就没了，相当于 Linux 重启后的 mqueue 文件系统。
- **原生 AIO**：`io_setup`、`io_destroy`、`io_submit`、`io_cancel`、`io_getevents`、`io_pgetevents`，`/proc/sys/fs/aio-nr`、`aio-max-nr`。读写（含向量）、`fsync`、`fdatasync` 提交时就同步做完，`IOCB_CMD_POLL` 在文件就绪时完成，可以取消；`IOCB_FLAG_RESFD` 通知 eventfd，`RWF_NOWAIT`、`RWF_APPEND` 生效。上下文的 id 是一页只读的空映射：libaio 看到环的魔数不对，就不在用户态读完成队列，改用 syscall。
- **epoll**：水平触发的关注在就绪的比 maxevents 多时轮流报告：从上次报告的后面接着查，效果与 Linux 把报告过的放到就绪链表末尾相同（epoll_wait14）。边沿触发的 `EPOLLEXCLUSIVE` 关注，同一次数据变化只报告给一个 epoll（epoll_wait16）：打开文件记着已经报告过的"数据代数"（读完、写满和读到数据的次数）。唤醒仍然叫醒所有等待者，没轮到的重新检查之后接着睡。
- **inotify 的三处近似都去掉了**（§8.3）：每个打开的仓库文件记着打开时经过的目录和名字（原来只有打开时已经有监视的才记），监视建立之前打开的文件关闭时也报告；rename 移动目录项时，经由它打开的文件跟着改（扫一遍所有 fd 表）；`IN_DELETE_SELF` 在还有经由被删除的那个名字打开着的文件时等它们都关闭，顺序（`IN_CLOSE_*`、`IN_DELETE_SELF`、`IN_IGNORED`）与 Linux 相同。`max_user_instances`、`max_user_watches` 生效。
- **nice 和亲和性交给宿主**：nice 改为按线程记录（Linux 的 `PRIO_PROCESS` 作用于线程，原来记在线程组上，第二个线程调 nice 拿到的是叠加的值），并设置到宿主线程上；guest 提高优先级时宿主线程最低到 -4，不超过 Android 上与显示有关的线程，guest 的忙循环不会拖慢界面。`SCHED_IDLE` 设成宿主的 19。`sched_setaffinity` 设置宿主线程的亲和性：guest 的 CPU i 是宿主允许本进程用的第 i 个 CPU，按启动时的快照（`available_parallelism` 读的是调用线程的亲和性，guest 线程被绑核之后再读会少报 CPU）。新线程开始运行时换成自己的设置，不继承创建它的宿主线程的。`RLIMIT_NICE`、`RLIMIT_RTPRIO` 的默认值改为 Linux 的 0，提高优先级按 `RLIMIT_NICE` 检查（Linux 的 can_nice）。宿主上没有权限时（普通 Linux 宿主不给降低 nice）保持原样；Android 的 init 给所有进程 `setrlimit nice 40 40`，开发容器也加上 `--ulimit nice=40:40`。另外 `SCHED_RESET_ON_FORK` 的子线程负的 nice 回到 0，`sched_*` 对还没回收的僵尸进程不再是 ESRCH。
- **内存**：
  - `remap_file_pages`：与 Linux 4.0 起一样用 mmap 模拟，在原地用同一个文件、新的偏移重新做共享映射。
  - `MREMAP_DONTUNMAP` 支持所有映射（Linux 5.13 起）：原来的地址上私有匿名映射变空；共享映射（文件或匿名）与新地址共用同一段宿主内存，相当于之后从文件或共享内存重新读进来；私有文件映射按 inode 重新打开仓库里的文件再映射，打不开（已经删除并回收）时给空的匿名内存。
  - `MAP_GROWSDOWN`：缺页地址在一个向下增长的私有匿名映射下面、中间没有别的映射时，把它延伸到那一页（mmap18），与下面那个可访问的映射之间要留出 256 页（`stack_guard_gap`）。宿主内存不必连续，长出来的部分是一个新的 VMA，所以不需要预留地址空间。共享匿名映射加它是 EINVAL，文件映射加它不增长。
  - cachestat（6.5）：在缓存里的页数用 mincore 查宿主文件映射，不依赖宿主内核的版本（宿主的 cachestat 在开发容器的 overlayfs 上也数不到底层文件的页）；脏页、写回、逐出几项是 0。
- **procfs、pidfd**：
  - `/proc/sys/kernel/ns_last_pid` 可读写（pidfd_send_signal03）。
  - `/proc/<pid>` 目录的 fd 记着打开时的那个进程：当作 pidfd 用时，进程回收之后是 ESRCH，即使 pid 已经给了新进程。`/proc/<pid>` 下的 inode 也随进程实例不同（加进了进程的编号），与 Linux 一样。
  - `/proc/net/{icmp,icmp6,raw,raw6}`，格式与 Linux 相同（raw 的本端"端口"是协议号，ping 和 raw 的对端端口是 0）。ping 和 raw socket 没有对端端口，宿主的 getpeername 不给对端，数据报 socket connect 的对端由内核层记下。guest 的 raw ICMP socket 在宿主上是 ping socket，`SO_TYPE` 照样报告 `SOCK_RAW`。
  - `/proc/net/unix` 列出只在 `SCM_RIGHTS` 消息里的 socket，和还没 accept 的连接（与 Linux 一样没有 inode，状态是 `SS_CONNECTING`）。
- **AF_UNIX 的垃圾回收**：只经由在途消息互相引用的 socket 形成 Rc 的环（打开文件 → socket → 接收队列 → 消息 → 打开文件），程序已经够不着，原来永远不会释放。现在与 Linux 的 unix_gc 一样：消息里带着打开文件的 channel 登记下来；关闭 unix socket（close、close_range、dup2 覆盖、execve）和进程退出之后，候选是所有引用都来自在途消息的 unix socket，去掉从候选的接收队列出发的引用之后还有引用的够得着，它们收到的也够得着；剩下的清空接收队列，环就断开了。别处临时多持有一个引用的不是候选，只会少回收、不会错收。Linux 6.8 起 GC 在工作队列里异步执行，所以这一项没法和原生比对，由单元测试检查。
- **零碎的**：fd 表没有空位时总是 EMFILE（`F_DUPFD` 的起点超过上限由它自己返回 EINVAL），`mq_open` 先装 fd 再登记名字，装不下时不留下队列。
- musl 的问题：mq_open01 的空名字一项要 EINVAL，那是 glibc 自己的检查，musl 直接交给内核，Linux 返回 ENOENT；sched_setscheduler04 用 libc 的 `sched_getscheduler`，musl 的这个函数直接返回 ENOSYS。这两项在原生 Alpine 上同样失败。用 libaio 的 AIO 测试跳过，因为 LTP 构建时没有 libaio。
- 测试：差分测试 `compat` 新增信号驱动的 I/O、文件租约（在 /dev/shm 里，原生一侧的 overlayfs 不让设写租约）、消息队列、AIO、`MREMAP_DONTUNMAP` 和 remap_file_pages，`/proc/net` 那组加上 icmp、icmp6、raw（CI 的 Ubuntu 宿主默认不让普通用户建 ping socket，差分测试之前把 `ping_group_range` 改成允许所有组）；`inotify` 新增按 fd 的事件报给哪个目录。单元测试新增 inotify 推迟的 `IN_DELETE_SELF`、AF_UNIX 的垃圾回收、`MAP_GROWSDOWN`，以及 hal 的第一个单元测试（cachestat）。LTP 新通过 fcntl23–27、31–33、mq_notify01–03、mq_timedreceive01、mq_timedsend01、mq_unlink01、io_cancel01、io_destroy02、io_getevents01、io_setup02、io_submit02–04、epoll_wait14、epoll_wait16、nice05、mmap18、remap_file_pages01/02、shmctl05（用 remap_file_pages）、cachestat02、pidfd_send_signal03；cachestat01/04 要块设备，03 要 hugetlbfs。
- 性能：每次 open 多挂一个记着目录项的对象，rename 要扫一遍所有 fd 表，新线程开始时多两个宿主 syscall；开销都在噪声之内。

## 推迟项

- io_uring 不做，保持 ENOSYS。之后试做过一轮又放弃了：环放在共享内存里，请求在提交者的上下文里先不等待地执行一次（Cx 加"不等待"模式，让现有的阻塞循环直接返回 EAGAIN），会阻塞的挂起，由定时器线程把提交者叫进内核推进（相当于 task_work），SQPOLL 靠一直置位 `IORING_SQ_NEED_WAKEUP` 实现。接口和 49 个操作码写完是 3800 行，liburing 的测试先抽了 10 个，过了一半，剩下的细节（DEFER_TASKRUN 的完成时机、多次接收的 recvmsg、提供的缓冲区等）还有很长一串。考虑到用的人很少（见 §15），代码没有留下。
- `PIDFD_THREAD` 是 6.9 的功能，tuxport 报告的是 6.1，不做。
- 内核访问 guest 内存（比如 read 写进还没长出来的栈）不会让 `MAP_GROWSDOWN` 的映射增长，返回 EFAULT。→ syscall 入口和构造信号帧之前会让它增长了（M9 之前：栈的大小上限）；内核别的访问仍然是 EFAULT
- `EPOLLEXCLUSIVE` 只保证报告给一个，唤醒还是全部唤醒。→ 都已补上：水平触发的关注也只报告给一个 epoll（M9 之前：epoll 的边沿），文件上来了新的边沿只叫醒一个等待者，没轮到的进程不醒（M9 之前：独占的唤醒）
