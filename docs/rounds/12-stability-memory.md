# M9 之前：稳定性和内存（2026-10-25）

开始 M9 之前，先补上推迟项里手机上最容易碰到的几处：一个 guest 程序就能让整个 App 崩溃的情况，以及让内存白白多占的两处。

## 做法

- **截短已映射的文件**（M7 推迟，§5.2、§7.4）：一个进程映射着文件，另一个进程截短它，再访问超出新末尾的页，宿主进程收到 SIGBUS，所有会话一起退出。差分测试在上一版上复现了这一点（"Bus error"）。现在 truncate、ftruncate、`O_TRUNC` 之后，遍历所有地址空间，把映射这个文件、超出新末尾的页从页表里拿掉（`Mm::file_truncated`），别的进程的地址空间就地做 shootdown。guest 访问这些页得到自己的 SIGBUS，与 Linux 相同。VMA 因此给私有文件映射也记下宿主文件的标识，原来只有共享映射记了（futex 的键）。
- **`PROT_NONE` 不进页表**（M8 推迟，§5.2）：mprotect 加上权限时再按 VMA 放进去，去掉全部权限时拿掉；内核写 guest 内存（加载器）改为按 VMA 找宿主地址。C 里的解除映射和修改权限遇到没有中间表的地方，整张叶子表（2 MB）跳过，原来对 8 GB 的预留要逐页查两百万次。
- **`MADV_DONTNEED` 交给宿主**（M8 推迟，§5.2）：原来是清零，反而把要归还的页都碰了一遍。
- 两项合起来，创建 10 块 wasm 内存的 Node.js 进程，宿主峰值 RSS 从 536 MB 降到 118 MB；`java -version` 从 116 MB 降到 106 MB。
- **`restart_syscall`**（M3a 推迟）：nanosleep 和相对时间的 clock_nanosleep 被信号打断时记下到期时刻，返回 `ERESTART_RESTARTBLOCK`。有处理函数时是 EINTR，没有时（比如停止之后又继续）重新执行的 svc 改成 restart_syscall，接着睡剩下的时间。原来按原来的时长重新睡满：`sleep 10` 中途 Ctrl-Z、`fg`，要再睡 10 秒。
- **robust futex**（M6 推迟）：线程退出和 execve 时按 Linux 的 `exit_robust_list` 处理 `set_robust_list` 登记的链表，线程还持有的锁标上 `FUTEX_OWNER_DIED` 并唤醒一个等待者，等待者拿到 EOWNERDEAD。musl 的 `pthread_exit` 自己处理链表，所以这条路径管的是进程直接退出或被杀的情况；原来进程间共享的 robust 互斥锁的等待者会永远卡住。
- **`EPOLLEXCLUSIVE`**（M6 推迟）：参数检查与 Linux 相同（只能在加入时用，不能用于 epoll，不能配别的标志，加了它的关注不能修改）；唤醒仍然是全部唤醒。→ 一个边沿只报告给一个 epoll、只叫醒一个等待者（M9 之前：epoll 的边沿、独占的唤醒）

## 测试

差分测试新增 `edge-cases`（别的进程截短映射着的文件、`MADV_DONTNEED`、`PROT_NONE` 预留、nanosleep 的重启、进程间 robust 锁、`EPOLLEXCLUSIVE`），两个执行层下都和原生一致。单元测试新增截短、`PROT_NONE` 不进页表、`MADV_DONTNEED`。LTP 多出来的 futex_wake05、nice05 是[偶发项](../milestones.md#known-flaky-tests)。→ futex_wake05 不是偶发项：它检查对未映射、`PROT_NONE`、被截短的文件映射的地址做 futex 时返回 EFAULT，这一轮之前每次都失败，从这一轮起每次都通过，是这一轮修好的（M9 之前：CPU 时间不倒退）
