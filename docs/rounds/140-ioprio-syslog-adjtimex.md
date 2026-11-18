# M9 之前：ioprio、syslog 和 adjtimex（2026-10-31）

[未实现的系统调用](139-unimplemented-syscalls.md)那一轮记下的推迟项里挑了三组，都是 Linux 6.1 上总有、tuxport 原来返回 ENOSYS 的：

- **ioprio_set、ioprio_get**：util-linux 的 `ionice` 设不上时不带 `-t` 就 `err(EXIT_FAILURE)`（`schedutils/ionice.c` 的 `ioprio_setid`），busybox 的是 `bb_perror_msg_and_die`，都在 execvp 之前退出：`ionice -c3 make`、cron 和备份脚本里常见的 `nice ionice …` 后面的命令根本不执行。
- **syslog(2)**（klogctl）：busybox 的 `dmesg` 直接用它；util-linux 的 `dmesg` 先开 `/dev/kmsg`（tuxport 没有），退回 syslog(2)，也是 ENOSYS。两种 dmesg 都报错退出。
- **adjtimex、clock_adjtime**：只读的查询（modes 为 0）谁都能调，busybox 的 `adjtimex`、`ntptime`、node_exporter 的 timex 收集器靠它看时钟同步了没有。

Android 的应用 seccomp（bionic 的 `SECCOMP_BLOCKLIST_APP.TXT`）不让调 adjtimex、clock_adjtime 和 syslog，调了整个 App 收到 SIGSYS；ioprio_set、ioprio_get 是放行的（`SECCOMP_ALLOWLIST_COMMON.TXT`）。所以 IO 优先级可以交给宿主线程，时钟状态在 Android 上只能合成，内核日志本来也拿不到宿主的。

## 做法

- **IO 优先级**（`core/kernel/src/syscall/sched.rs`，照 Linux 6.1 的 `block/ioprio.c`、`blk-ioc.c`）：
  - 按线程记原始值（`Task::ioprio`，相当于 `io_context->ioprio`，unsigned short，高位丢掉）。没有设置过时没有 io_context，ioprio_get 读到 0（`IOPRIO_DEFAULT`，NONE 类）。新线程、新进程照 `copy_io`：创建者有 io_context 时 `CLONE_IO` 共用一份，否则类别不是 NONE 的复制一份；创建者没有的，新的也没有。execve 保留。
  - 检查（`ioprio_check_cap`）先于找目标：RT 类要开机时的命名空间里的 `CAP_SYS_ADMIN` 或 `CAP_SYS_NICE`，RT、BE 的级别是 0–7，NONE 的级别只能是 0，IDLE 的级别不检查，类别 4–7 是 EINVAL。6.5 起 data 的高位是提示（`IOPRIO_PRIO_HINT`），级别 8 以上在新内核上接受，tuxport 按 6.1 是 EINVAL。
  - 目标：`IOPRIO_WHO_PROCESS` 是一个线程（tid），`IOPRIO_WHO_PGRP` 是进程组里所有进程的所有线程，`IOPRIO_WHO_USER` 是真实用户 ID 为它的所有线程（看不到的 pid 命名空间里的不算）。set 的 who = 0 照 Linux 的 `make_kuid(current_user_ns(), 0)` 是 root，get 的 who = 0 是自己的真实用户（`current_user()`）。权限照 `set_task_ioprio`：目标的真实用户 ID 等于自己的有效或真实用户 ID，或者有 `CAP_SYS_NICE`；进程组、用户的第一个不行就返回，前面的已经改了。pid、进程组号在 pid 命名空间里照 setpriority 换算。内置的 init 没有线程，改它没有效果。
  - ioprio_get 对一个线程返回原始值（`get_task_raw_ioprio`）；对进程组、用户返回实际的优先级里最高的（数值最小，`ioprio_best`），NONE 类的按调度策略和 nice 值推出来：`SCHED_IDLE` 是 IDLE，FIFO、RR、DEADLINE 是 RT，其余是 BE，级别是 (nice + 20) / 5。
  - 交给宿主线程（`tuxport_hal::set_thread_ioprio`）：设置时、宿主线程开始运行 guest 线程时（线程池里复用的宿主线程还留着上一个 guest 线程的，没设过的也写一次 0）。RT 降成同一级的 BE，与 nice 值最低到 -4 一样不让 guest 的 IO 压过界面；NONE 让宿主按宿主线程的 nice 值推。`CLONE_IO` 共用一份的线程一起改。guest 线程自己做的文件读写（仓库里的 object、pread/pwrite）就按这个优先级排队。
- **syslog(2)**（`syscall/misc.rs`）：tuxport 没有内核日志，日志总是空的。权限照 `check_syslog_permissions`，先于参数检查：`dmesg_restrict` 为 0 时谁都能读全部（3）和问缓冲区大小（10），别的动作和 `dmesg_restrict` 为 1 时的所有动作要开机时的命名空间里的 `CAP_SYSLOG` 或 `CAP_SYS_ADMIN`。读全部、读后清空：buf 为空或长度为负是 EINVAL，长度 0 返回 0，地址超出 48 位用户空间是 EFAULT（`access_ok`），之后读到 0 个字节。读（2）等新的日志，一直等到被信号打断（ERESTARTSYS）。关闭、打开、清空、未读的字节数都是 0，缓冲区大小是 128 KB（`CONFIG_LOG_BUF_SHIFT` 的默认值 17）。控制台的开关和级别（6、7、8）照常记下：8 的级别是 1–8，不低于最低级别，同时重新打开控制台。
- **procfs**：新增 `/proc/sys/kernel/printk`（四个级别，默认 `7 4 1 7`，按 `proc_dointvec` 写前几个）和 `dmesg_restrict`（默认 0，没有 `CONFIG_SECURITY_DMESG_RESTRICT`；写要 `CAP_SYS_ADMIN`，只能是 0、1）。
- **adjtimex、clock_adjtime**（`syscall/misc.rs`）：
  - 要改东西的一律 EPERM，与 clock_settime 一样当作没有 `CAP_SYS_TIME`。照 `timekeeping_validate_timex` 的顺序：`ADJ_ADJTIME` 要带 `ADJ_OFFSET_SINGLESHOT`，否则 EINVAL（内核里这两个是 0x8000、0x0001；用户态头文件的 `ADJ_OFFSET_SINGLESHOT` 是 0x8001，带着 `ADJ_ADJTIME`），再带 `ADJ_OFFSET_READONLY` 的（adjtime(3) 查剩下的调整量）是查询；不是 `ADJ_ADJTIME` 时 modes 不为 0 的是 EPERM；`ADJ_SETOFFSET` 是 EPERM；`ADJ_FREQUENCY` 的值乘 `PPM_SCALE` 会溢出的是 EINVAL。
  - 查询的结果取自宿主（`tuxport_hal::adjtimex_query`）：Linux 宿主上直接问宿主内核（modes 为 0 或 `ADJ_OFFSET_SS_READ`），返回值是宿主的时钟状态，有名字的字段都换成宿主的，modes 和填充不动。Android 上按 Android 内核的实际状态合成：Android 用 settimeofday 校时、不运行内核的 NTP 调整，`ntp_clear` 之后一直是未同步（`STA_UNSYNC`），maxerror、esterror 是 `NTP_PHASE_LIMIT`（16 秒），time_constant 2，precision 1，tolerance 32768000，tick 10000，返回 `TIME_ERROR`。
  - adjtimex 无论成败都把结构体写回去（写不进去是 EFAULT，所以 EPERM 的调用碰上只读的内存也是 EFAULT），clock_adjtime 只在成功时写回，与 Linux 相同。clock_adjtime 只有 `CLOCK_REALTIME` 能调，别的合法时钟（含 CPU 时钟）是 EOPNOTSUPP，10、11 以上是 EINVAL，fd 编码的动态时钟（PTP 设备）是 EINVAL：tuxport 里没有这种设备。

## 测试

- 新的差分测试组 `ioprio-klog`（原生一侧加 `CAP_SYS_NICE`、`CAP_SYSLOG`，与 tuxport 里的 root 一样）：ioprio 的参数检查、继承和 `CLONE_IO`、按调度策略和 nice 推出的优先级、按进程组和用户设和查、权限；syslog 的参数错误、权限和 `dmesg_restrict`；adjtimex、clock_adjtime 的查询、EINVAL 和 EPERM 的先后、写回和 EFAULT、各种时钟编号；util-linux 和 busybox 的 `ionice`。两个执行层下都与原生相同。原生是 6.8、读的是宿主的日志和 NTP 状态，所以 ioprio 的级别 8、日志内容、缓冲区大小、syslog 的读（2）、adjtimex 里随宿主的 NTP 在变的字段都不比。
- rootfs 用例 `kernel_log_is_empty_and_keeps_console_levels`（busybox 的 dmesg、`dmesg -n`、`dmesg_restrict`）和 `io_priority_reaches_the_host_thread`（宿主上的线程拿到对应的 IO 优先级，没有 RT 的）。
- `tests/guest/waits.c` 加上 syslog 的读（等到信号）；hal 的单元测试比对合成的时钟状态和宿主内核的只读查询。
- LTP 新通过 ioprio_get01、ioprio_set01–03、syslog11、syslog12、adjtimex03。adjtimex01、adjtimex02、clock_adjtime01、clock_adjtime02 仍失败：它们以 root 改时钟、NTP 参数，期望成功或者过了能力检查之后的 EINVAL，tuxport 当作没有 `CAP_SYS_TIME`，是 EPERM。

## 推迟项

- 日志总是空的。OOM killer 结束进程、内核层报告的内部错误（§12 第 4、5 条）只写宿主的 stderr（Android 上是 logcat），没有进 dmesg；Linux 上它们都在内核日志里（[稳健性](20-robustness.md)那一轮"终端里只看到 Killed"的推迟项可以借它做）。→ 已补上（M9 之前：/dev/kmsg 和内核日志）：OOM killer、内部错误结束进程和 guest 写 `/dev/kmsg` 的都记进日志
- 没有 `/dev/kmsg` 和 `/proc/kmsg`：util-linux 的 dmesg 退回 syslog(2)，往 `/dev/kmsg` 写日志的程序（systemd、一些 init 脚本）打不开它。→ 已补上（M9 之前：/dev/kmsg 和内核日志）
- io_context 只在 ioprio_set 之后才有。Linux 上做过块 IO 的线程可能已经有了（看 IO 调度器），这时 `CLONE_IO` 照样共用，tuxport 里不共用；正在退出的线程（`PF_EXITING`）Linux 不设，tuxport 照设。
- 仓库的后台线程（检查点、删除 object）的 IO 不跟着哪个 guest 线程的优先级。
- 改时钟、NTP 参数的 adjtimex 是 EPERM：chronyd、ntpd 调不了时钟（宿主的时间 guest 本来就改不了）。Android 上合成的状态总是未同步，maxerror 不随时间增长。
