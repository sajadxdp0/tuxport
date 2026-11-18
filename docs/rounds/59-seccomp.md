# M9 之前：seccomp（2026-10-28）

原来 guest 不能自己装 seccomp 过滤器（§11.1）：`prctl(PR_SET_SECCOMP)` 做完参数和权限检查之后返回 EINVAL，`seccomp(2)` 是 ENOSYS。用 seccomp 给自己加沙箱的程序要么报错，要么不带沙箱运行；靠 `SECCOMP_RET_TRACE` 的 `strace --seccomp-bpf`、靠通知 fd 代办系统调用的监管程序也用不了。这一轮在内核层实现 seccomp（`seccomp.rs`），行为按 Linux 6.1。

## 做法

- **模式**：
  - 严格模式只允许 read、write、exit、rt_sigreturn，别的系统调用以 SIGKILL 结束这个线程（Linux 的 `do_exit(SIGKILL)`）。exit_group 也不在里面，从 main 返回同样被结束。
  - 过滤模式：`seccomp(SECCOMP_SET_MODE_FILTER)` 或者 `prctl(PR_SET_SECCOMP, 2)` 装 cBPF 程序，要求 `no_new_privs` 或者 `CAP_SYS_ADMIN`。两种模式之间不能互换。
  - `PR_GET_SECCOMP` 报告模式，`/proc/<pid>/status` 加上 `Seccomp_filters`（过滤器的个数）。
- **程序的检查**：先按 cBPF 的规则（`bpf_check_classic`：认得的操作码、不除以常数 0、移位不超过 31、跳转不出界、草稿内存先写后读、最后一条是返回），再按 seccomp 的（`seccomp_check_filter`：只能读 `seccomp_data` 里按 4 字节对齐的字，读长度变成常数 64，不能读包、不能取模），错误码与 Linux 相同。一条链的容量也与 arm64 Linux 一样，按转换成 eBPF 之后的长度算（RET K 两条、除以 X 五条、两个分支都跳的条件跳转两条、大于 `i32::MAX` 的常数多一条，每个旧过滤器另加 4 条，超过 32768 条是 ENOMEM）。
- **执行**：在系统调用的入口、PTRACE_SYSCALL 的入口停住之后（arm64 的 `syscall_trace_enter`），在 `seccomp_data`（编号、`AUDIT_ARCH_AARCH64`、svc 之后的 pc、六个参数，第一个是原来的 x0）上运行链上的每一个程序，取动作最严格的结果（按有符号数最小，同样严格时取最新装的那个）。程序由一个解释器执行，结果与 Linux 的 JIT 相同：移位按 32 取模，除以 0 时程序返回 0。
  - 有 seccomp 的线程，系统调用都交给持 BKL 的路径（`syscall::fast` 看 `Task::seccomp_on`），不持 BKL 的 futex、nanosleep、stat 也要先检查。
  - vDSO 里读时钟的存根（clock_gettime、clock_getres、gettimeofday）不检查：Linux 上它们不进内核，跟踪者也看不到（与 ptrace 共用 `vdso::clock_stub`）。
- **动作**：
  - KILL_PROCESS（以及不认得的动作）：SIGSYS 结束整个进程，写 core。与 Linux 的 `SA_IMMUTABLE` 一样，SIGSYS 恢复默认处理、解除屏蔽，之后 sigaction 读写它都是 EINVAL，跟踪者也拦不下（`signal::force_fatal`）。
  - KILL_THREAD：只结束这个线程，结束方式是 SIGSYS；只剩这一个线程时与 KILL_PROCESS 一样。
  - TRAP：发 SIGSYS，`si_code` 是 `SYS_SECCOMP`，`si_errno` 是过滤器给的数据，`si_call_addr`、`si_syscall`、`si_arch` 也填上（signalfd 读到的也有）。处理函数看到的 x0 是原来的参数。SIGSYS 被屏蔽或者忽略时恢复默认动作。
  - ERRNO：返回过滤器给的错误码，最大 4095。
  - TRACE：跟踪者打开了 `PTRACE_O_TRACESECCOMP` 时停一次 `PTRACE_EVENT_SECCOMP`（事件消息是数据，`PTRACE_GET_SYSCALL_INFO` 报告 `OP_SECCOMP`），跟踪者可以改编号（`NT_ARM_SYSTEM_CALL`，-1 是跳过）和参数，放行之后重新检查一遍；没有这样的跟踪者时是 ENOSYS。
  - USER_NOTIF：交给通知 fd 那头的监管进程（见下）。LOG：记一笔再执行。ALLOW：执行。
  - 记一笔写进 tuxport 的调试日志（`TUXPORT_LOG`；Linux 发 audit 记录），按 `/proc/sys/kernel/seccomp/actions_logged`：结束和 LOG 总记，别的只在过滤器设了 `SECCOMP_FILTER_FLAG_LOG` 时记。`actions_avail` 列出 8 个动作；写 `actions_logged` 要 `CAP_SYS_ADMIN`，不能有 allow。
- **继承**：过滤器属于线程，clone、fork 继承，execve 保留。fork 出来的进程共用祖先的过滤器，过滤器连成一棵树，每个线程看到的是从自己最新的那个往上的一条链。`SECCOMP_FILTER_FLAG_TSYNC` 把新过滤器同步给同一线程组的其他线程，它们要么没有 seccomp，要么用的过滤器在调用者的链上，否则返回第一个不能同步的线程的 tid（`TSYNC_ESRCH` 时是 ESRCH）。
- **通知 fd**（`SECCOMP_FILTER_FLAG_NEW_LISTENER`，`anon_inode:seccomp notify`）：
  - 过滤器返回 USER_NOTIF 时，发出系统调用的线程排一个通知，等监管进程用 `SECCOMP_IOCTL_NOTIF_RECV` 取走、`NOTIF_SEND` 回答：给返回值或者错误码，或者 `FLAG_CONTINUE` 让它照常执行。通知的 id 从一个随机数开始，`NOTIF_ID_VALID` 查通知还在不在等回答。
  - `NOTIF_ADDFD` 把监管进程的一个 fd 装进对方的 fd 表：`SETFD` 时装在指定的位置上、替换原来的，`SEND` 时同时用它回答，受对方的 `RLIMIT_NOFILE` 限制。Linux 由那个线程自己去装；这里持有 BKL 直接装进它的 fd 表，结果相同。
  - poll、epoll：有没取走的通知时可读，有取走了等回答的时可写，过滤器没有用户了（用它的进程都被回收了，僵尸进程仍算）时是 POLLHUP。
  - 等回答时被信号打断返回 ERESTARTSYS，重新执行时是一个新的通知；设了 `WAIT_KILLABLE_RECV` 的，通知被取走之后只被致命信号打断。监管进程的 RECV 被信号打断是 EINTR（6.1 的行为，6.6 起是 ERESTARTSYS）。
  - 链上已经有通知 fd 时再要一个是 EBUSY。通知 fd 的最后一个引用关闭时，等着回答的系统调用和之后的都得到 ENOSYS。
- **PTRACE_SYSCALL 的出口**：与 arm64 Linux 一样（`syscall_trace_exit` 重新读标志），有 seccomp 的线程出口停不停看出口时的跟踪状态：跟踪者用 PTRACE_CONT 让它继续、在 `PTRACE_EVENT_SECCOMP` 停住时改用 PTRACE_SYSCALL 的，这次系统调用的出口也停，跳过的系统调用也一样。`strace --seccomp-bpf` 就这样只在要看的系统调用上停、在出口拿返回值。这一处是审查时查出来改的：原来出口只看入口时有没有停，strace 把下一个系统调用的入口当成出口，打印的返回值是错的（strace 自己也报 `exiting, ptrace_syscall_info.op == 1`）。
- **ptrace 的其他部分**：`PTRACE_O_SUSPEND_SECCOMP`（checkpoint/restore 用，要求 `CAP_SYS_ADMIN`、跟踪者自己没有 seccomp），`PTRACE_SECCOMP_GET_FILTER` 读原样的程序，`PTRACE_SECCOMP_GET_METADATA` 读标志。`seccomp(2)` 的 `GET_ACTION_AVAIL`、`GET_NOTIF_SIZES` 也有了。
- **进程的退出状态**（§6）：线程各自退出时，改按 6.1 的 [`synchronize_group_exit`](https://github.com/gregkh/linux/blob/linux-6.1.y/kernel/exit.c)，由最后一个进入退出流程的线程定下进程的退出状态（原来是主线程 exit 的退出码），在 `PTRACE_EVENT_EXIT` 之前定，跟踪者放行的先后不影响它。KILL_THREAD 结束了主线程、另一个线程之后 exit 时，进程的退出码是那个线程的。`tests/guest/bugs.c` 跟着改：最后一个线程用 SYS_exit 退出之后，退出清理里的内核 bug 不再把进程的结束方式改成 SIGSEGV，与 exit_group 之后一样。

## 结果

单线程、Tier 1 的每次调用（过滤器只看架构是 6 条，再比较 40 个编号是 46 条；原生一侧的过滤器由 JIT 执行）：

| | 原生：没有 / 6 条 / 46 条 | tuxport：没有 / 6 条 / 46 条 |
|---|---|---|
| clock_gettime（vDSO） | 20 / 19 / 19 纳秒 | 89 / 144 / 144 纳秒 |
| getppid | 127 / 148 / 148 | 95 / 112 / 290 |
| futex 唤醒（快速路径） | 159 / 180 / 181 | 59 / 123 / 307 |
| fstatat（快速路径） | 585 / 615 / 626 | 885 / 1250 / 1390 |
| pread | 226 / 246 / 248 | 427 / 440 / 630 |

装了过滤器之后，快速路径上的系统调用改走持 BKL 的路径：futex 唤醒慢一倍，fstatat 慢四成。vDSO 里读时钟虽然不检查，也一样进了持 BKL 的路径。解释器每条指令约 4.5 纳秒，而 libseccomp 生成的过滤器是一串线性比较，像 docker 默认规则那样放行几百个系统调用的，每次系统调用要运行几百条。

## 测试

新的差分测试 `seccomp` 比对：报告可用动作和状态的接口，严格模式，程序的检查和一条链的容量，各种动作，快速路径上的系统调用被过滤、经由 vDSO 的 clock_gettime 不被过滤，继承和 TSYNC，ptrace（`PTRACE_EVENT_SECCOMP`、改编号和跳过、出口的停止），通知 fd 和 ADDFD，`strace -f --seccomp-bpf`。

动作那一节把 `RLIMIT_CORE` 设成 1，两边都不写 core：`core_pattern` 是管道时（CI 的 Ubuntu 宿主上是 apport）Linux 不管限制是不是 0 都交给它，只有 1 不交，写文件时不到一页也不写。docker 里的 root 没有 `CAP_SYS_ADMIN`，测试开头两边都先去掉它，所以要特权的成功路径（`GET_FILTER`、写 `actions_logged`、`SUSPEND_SECCOMP`）没有比对。快速路径、ADDFD、改用 PTRACE_SYSCALL 和 strace 这几节是审查时补的：原来过滤的都是本来就持 BKL 的系统调用，快速路径漏看 `seccomp_on` 时过滤器被绕过也查不出。要写 core 的几处（只剩一个线程时的 KILL_THREAD、SIGSYS 被忽略时的 TRAP、进程的退出码、`Seccomp_filters` 跟着 fork）只用临时用例比对过，两层都与原生一致，没有放进差分测试（原生一侧写不写取决于宿主的 `core_pattern`，见[core dump](43-coredump.md)那一篇）。

单元测试新增程序的检查和执行。LTP：`scripts/ltp` 给测试看的内核配置加上 `CONFIG_SECCOMP`、`CONFIG_SECCOMP_FILTER`，原来因此跳过的 seccomp01 通过，prctl02 照常通过，这两个是 LTP 里仅有的看这两项配置的测试。

## 推迟项

- 记一笔只写 tuxport 的调试日志，没有 audit。
- 没有 Linux 的按编号缓存（`seccomp_cache`：算出某个编号总是 ALLOW 时不再运行程序），每次系统调用都运行整条链。→ 已补上（M9 之前：seccomp 的缓存和快速路径）
- 有 seccomp 的线程没有不持 BKL 的快速路径，vDSO 里读时钟也进了持 BKL 的路径（见上面的开销）。做了按编号缓存之后，缓存说总是 ALLOW 的编号可以照常走快速路径。→ 已补上：缓存说总是 ALLOW 的编号和 vDSO 里读时钟照常走快速路径（M9 之前：seccomp 的缓存和快速路径）；别的编号仍要持 BKL 检查
- 解释器没有优化（每条指令约 4.5 纳秒，按操作码顺序比较），Linux 用 JIT。→ 有了按编号缓存，常见的过滤器放行的编号不再运行程序（M9 之前：seccomp 的缓存和快速路径）；解释器本身仍没有优化
- 没有 seccomp 的线程：跟踪者在系统调用中途的停止（fork、clone 的 `PTRACE_EVENT_*`）改用 PTRACE_SYSCALL 继续时，Linux 也在出口停，这里只有入口停过的才停。→ 已补上（M9 之前：if_inet6、O_PATH 的 ioctl 和中途停住之后的出口）：出口都按那时的跟踪状态停；查的时候还发现有 seccomp 的线程经由 vDSO 读时钟时会多停一次出口，一起修了
- vDSO 的存根对所有时钟都不检查：Linux 的 vDSO 只处理 REALTIME、MONOTONIC 这几种，进程、线程的 CPU 时钟退回真正的系统调用，过滤器看得到。
- TRACE 停住之后重新检查用的是停住之前的那条链；Linux 重新读线程当前的，只有停住期间别的线程 TSYNC 了才不同。
- ADDFD 由监管进程直接装：Linux 里对方这时被信号打断会让 ADDFD 返回 ESRCH，这里总能装上。
- `SECCOMP_FILTER_FLAG_SPEC_ALLOW` 只检查：不要预测执行的缓解措施是宿主上的事。
