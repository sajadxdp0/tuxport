# M9 之前：剩下的小缺口（2026-10-25）

把推迟项和全量 LTP 的失败重新过了一遍：疑似 musl 的那些先在原生 Alpine 容器里跑，16 个在原生上同样失败（gethostname02、pathconf02、pwrite04、sbrk01、recvmmsg01、sendmsg01、readlink03、open13、clone08 等，sendto01 的第 4 项也是），不归 tuxport。 → 后来查明其中两处与 tuxport 有关（M9 之前：LTP 剩下的小差异）：sendto01 的第 4 项原生失败是宿主的 AppArmor 拒绝 AF_UNSPEC 的地址，上游 Linux 上成功，tuxport 已改为不把 TCP 的地址交给宿主；readlink03 在 tuxport 上还多一项空路径的失败，已修。剩下原生通过、tuxport 不通过的补上：

## 做法

- **/dev/shm 和 /run 开机清空**：Linux 上它们是 tmpfs。仓库被第一个使用者打开时（拿得到排他锁，也就是没有别的 tuxport 进程在用，相当于开机）清空这两个目录的内容，目录本身保留。原来共享内存、命名信号量、pid 文件会一直留在仓库里，LTP 的 cachestat02 就撞上了前一天的运行留下的文件。
- **`/proc/<pid>/smaps`、`smaps_rollup`、`pagemap`**：驻留页按宿主内存算（mincore），匿名内存都算私有的脏页，文件映射都算私有的干净页；pagemap 每页 8 字节，在读的时候按位置现算（文件有 2^36 项），给驻留、文件页和独占位，与没有特权的进程在 Linux 上看到的一样不给页框号。maps 里没有名字的行末尾补上空格（Linux 就是这样），合并相邻映射时也比较是否锁住。
- **把页调进内存**：mlock（不带 `MLOCK_ONFAULT`）、`mlockall(MCL_CURRENT)`、`MAP_POPULATE`、`MAP_LOCKED`、`mlockall(MCL_FUTURE)` 之后的映射，每个宿主页读一个字节；原来只是记一下，mincore 和 smaps 都看不出来。
- **`ru_maxrss` 和内核态时间**：驻留内存的峰值在 getrusage、execve 和退出时取样（跨 execve 保留，子进程回收时并入父进程的 `RUSAGE_CHILDREN`），`/proc/<pid>/status` 的 VmHWM 也用它。只在这几个点取样，进程中途用了很多又释放掉的峰值会漏掉。内核态时间用宿主内核记的宿主线程的 stime（`/proc/self/task/<tid>/stat`），也就是宿主内核替它做的事（转发的 I/O、缺页等）；内核层自己在宿主用户态做的事算不进去，所以偏少，但 `time` 和 `top` 不再总是 0。最初试过"持有 BKL 的时间算内核态"，每次加锁、解锁多读两次时钟，最快的 syscall 慢了六成，放弃了。另一个教训：wait 每次被叫醒都要重扫子进程，算子进程用量这种开销大的事要等找到事件之后再做，不然 git status 慢了三成。
- **经由 `/proc/self/fd` 重新打开已删除的文件**（memfd、删除之后还开着的）：仓库里的 object 已经删了，没法按 inode 打开。改为直接复制宿主 fd（读写都按位置进行，偏移由各自的打开文件记着）；要更多的权限时经由宿主的 `/proc/self/fd` 重新打开（开发环境的 virtiofs 上不行，Android 的 ext4、f2fs 上可以）。`O_NOFOLLOW` 停在这种链接上是 ELOOP。memfd 还有可写的共享映射时加 `F_SEAL_WRITE` 返回 EBUSY。
- **unshare**：`CLONE_FILES`、`CLONE_FS` 复制 fd 表和根目录、当前目录，`CLONE_SYSVSEM` 执行并丢掉信号量的 undo，`CLONE_THREAD`、`CLONE_SIGHAND`、`CLONE_VM` 只在本来就没有共用时成功，命名空间与 clone 一样返回 EPERM。`/proc/sys/fs/nr_open` 可写，`RLIMIT_NOFILE` 和 unshare 复制 fd 表都按它检查。
- **`vm.max_map_count`**：VMA 达到上限（默认 65530，可写）时 mmap 返回 ENOMEM，拆分 VMA 的 munmap、mprotect、mlock 也一样。每个 VMA 都是一个宿主映射，宿主的映射数是整个 App 进程共用的；原来一个 guest 进程就能把它用完。tuxport 不合并相邻的匿名 VMA，同样的程序 VMA 可能比 Linux 上多。
- **POSIX 定时器的超限次数**：间隔很短、起点在很久以前时周期数超出 32 位，原来截断之后绕回了；现在按 128 位算，报告的次数封顶在 INT_MAX。过去的绝对时间原来被截成"现在"，丢了过去了多久；`timer_getoverrun` 改为在信号送达时更新，不再被之后的每次到期覆盖。
- **零碎的**：
  - tkill、tgkill 的实时信号排不上队是 EAGAIN。
  - futex requeue 的个数是负数时 EINVAL。
  - 阻塞的 connect 返回之前，再 connect 一次宿主 socket，让它从"正在连接"变成"已连接"，之后再 connect 是 EISCONN。
  - SIOCATMARK；修改接口的 SIOCSIF* 先读参数（EFAULT），再返回 EPERM。
  - 没有 `CAP_NET_ADMIN` 时 `SO_SNDBUFFORCE`、`SO_RCVBUFFORCE` 是 EPERM，有时退回到普通的 `SO_*BUF`。
  - 对内置的 init 调 setpriority 不再是 ESRCH。
- 性能：shell 起 600 个进程慢了约 1%（退出和 execve 时的驻留内存取样），syscall 密集的小循环和 `git status` 不变。
- 测试：差分测试 `compat` 新增 smaps 和 pagemap、重新打开已删除的文件、unshare、`ru_maxrss`、tkill、connect、接口 ioctl、memfd 封印；间隔 1 纳秒的定时器在原生一侧会造成信号风暴，结果取决于时机，留给 LTP 的 timer_settime03。单元测试新增开机清空的目录。LTP 新通过 getrusage03、times03、mlock05、mlock201、mmap12、mincore03、memfd_create01、munmap04、unshare02/03、tgkill02、futex_cmp_requeue02、connect01、setpriority02、setsockopt04、sockioctl01、timer_settime03；cachestat02 由失败变成跳过（走到了还没有的 cachestat），unshare01、unshare04 剩下的失败是命名空间。

## 推迟项

- `MAP_GROWSDOWN` 的映射不会向下增长（mmap18）：要在映射下方预留宿主地址空间，并在缺页时延伸 VMA。→ 已补上（M9 之前：推迟项），不需要预留：长出来的部分是一个新的 VMA
- cachestat；unshare 的命名空间。→ cachestat 已补上（M9 之前：推迟项）；命名空间原来定为不做，后来为了 bubblewrap 先做了挂载命名空间（M9 之前：挂载命名空间）、用户命名空间（M9 之前：用户命名空间）
