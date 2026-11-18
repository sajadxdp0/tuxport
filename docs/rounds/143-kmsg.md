# M9 之前：/dev/kmsg 和内核日志（2026-10-31）

[ioprio、syslog 和 adjtimex](140-ioprio-syslog-adjtimex.md)那一轮让 syslog(2) 回答一份空的日志，留下两条推迟项：日志总是空的，OOM killer 和内部错误结束进程只写宿主的 stderr（Android 上是 logcat），guest 里看不到进程为什么死了（[稳健性](20-robustness.md)那一轮起的"终端里只看到 Killed"）；没有 `/dev/kmsg`、`/proc/kmsg`，util-linux 的 `dmesg`、往 `/dev/kmsg` 写日志的程序（systemd、一些 init 脚本）打不开它。这一轮按 Linux 6.1 的 `kernel/printk/printk.c` 加上一份真的内核日志。

## 做法

- **日志**（`core/kernel/src/klog.rs`，设计文档 §7.8）：照 6.1 的 printk 环形缓冲区，128 KB（`CONFIG_LOG_BUF_SHIFT=17`，按记录在数据环里占的字节算）、最多 4096 条（`PRB_AVGBITS`），满了丢掉最老的。每条记录有序号、时间戳（宿主的 `CLOCK_MONOTONIC`，相当于 `local_clock`）、facility、级别、标志和正文。存的时候（`vprintk_store`）正文到第一个 NUL 为止、最多 `LOG_LINE_MAX - 1` 个字节，结尾的一个换行去掉、记成标志。结尾没有换行的记录还没定稿（等 `KERN_CONT` 接着写），读的人看不到，下一条记录来了才看得到，`prb_next_seq`、清空的位置也不算它。日志在 `Kernel` 里（`Rc<RefCell<Klog>>`），与别的内核对象一样只在持有 BKL 时访问；新的记录按对象叫醒等它的线程（Linux 的 `log_wait`）。
- **`/dev/kmsg`**（`devkmsg_*`，字符设备 1:11，开机时与 `/dev/null` 这些一起补上，0644）：
  - 打开：`printk_devkmsg` 为 `off` 时 EPERM；读方式打开按 syslog 的读全部查权限（`dmesg_restrict` 为 1 时要 `CAP_SYSLOG`），只写不查；读的位置从最老的一条开始。
  - 读：一次一条，`前缀,序号,微秒,标志;正文\n`，正文里的控制字符、127 以上的字节和 `\` 转义成 `\xNN`；没有新的记录时阻塞（非阻塞时 EAGAIN）；要读的那条被挤掉了是 EPIPE、位置跳到最老的一条；缓冲区放不下是 EINVAL，这条也算读过了。pread 与 read 一样，不看偏移。
  - 写：一次 write 或一次 writev 是一条记录（`write_iter`，systemd 用 writev 一段段拼一条），超过 992 字节（`LOG_LINE_MAX`）是 EINVAL；开头的 `<数字>` 是前缀，低三位是级别，其余是 facility，facility 为 0 时仍当作 `LOG_USER`（与内核自己的消息分开），没有前缀时是默认的消息级别（`/proc/sys/kernel/printk` 的第二个数）和 `LOG_USER`。长度 0 的 write 也是一条空记录（write(2) 不跳过 0，writev 跳过）。
  - 限速：`printk_devkmsg` 默认 `ratelimit`，每个打开文件 5 秒内最多 10 条，多的照样算写成功、丢掉，关闭时记一条 `printk: <进程名>: N output lines suppressed due to ratelimiting`；`on` 不限。
  - lseek 只能是偏移 0：`SEEK_SET` 到最老的一条，`SEEK_DATA` 到 syslog 上次清空之后的第一条，`SEEK_END` 到下一条，别的 EINVAL，返回 0，文件位置不变。poll 只报告可读（被挤掉时加 POLLERR、POLLPRI），从不报告可写；可以加进 epoll，边沿触发按读得到的记录数。splice 是 EINVAL（没有 splice_read、splice_write）。
- **syslog(2)**（`syscall/misc.rs` 的 `do_syslog`）：读全部（3，busybox 的 `dmesg`）从上次清空之后的记录里取放得下的最新的那些，每行前面加 `<前缀>[秒.微秒] `，一条记录加上前缀超过 1024 字节时按 `record_print_text` 丢掉后面的行；读后清空（4）、清空（5）只影响读全部和 `SEEK_DATA`；读（2）等到有记录，读走放得下的，第一条放不下时读走前一部分、下次接着读，与 `/proc/kmsg` 共用位置；未读的字节数（9）照 `get_record_print_text_size` 算。只有 `CAP_SYS_ADMIN`、没有 `CAP_SYSLOG` 的进程读日志时照 Linux 记一次警告。
- **`/proc/kmsg`**（`fs/proc/kmsg.c`，0400）：打开时按 syslog 的打开查权限，读是 syslog 的读（非阻塞时没有未读的是 EAGAIN），poll 看有没有未读的记录。
- **`/proc/sys/kernel/printk_devkmsg`**：`on`、`off`、`ratelimit`，照 `devkmsg_sysctl_set_loglvl` 必须恰好是其中之一再加一个字节（echo 的换行），否则 EINVAL、保持原样。
- **内核层自己的消息**：
  - OOM killer 结束进程（`oom.rs`）：App 的上限在 guest 看来是它的 cgroup 的 `memory.max`，所以按 Linux 的 memcg OOM 记三行：`memory: usage …kB, limit …kB, failcnt …`、`oom-kill:constraint=CONSTRAINT_MEMCG,…,task=<名字>,pid=<pid>,uid=<uid>`、`Memory cgroup out of memory: Killed process <pid> (<名字>) total-vm:…kB, anon-rss:…kB, file-rss:…kB, shmem-rss:…kB, UID:… pgtables:0kB oom_score_adj:…`（KERN_ERR）。驻留的页按映射的种类分开（与 smaps 一样按宿主内存算）。调用的栈、"invoked oom-killer"那一行（tuxport 的 OOM killer 是定时检查，没有触发它的分配）、各个进程的表略去。
  - 内核层、引擎的 bug 结束进程（`bug.rs`）：写给宿主 stderr 的同一份报告，KERN_ERR（Linux 的 oops 用 KERN_EMERG，那是整个内核出了错，这里只结束一个进程）。
  - 宿主 stderr 上的报告照旧。

## 测试

- 单元测试（`klog.rs`）：存记录、未定稿的记录、转义、满了丢掉最老的、syslog 格式和几种读法、前缀的解析、限速。
- 新的差分测试组 `kmsg`（原生一侧加 `--device /dev/kmsg`、`CAP_SYSLOG`）：设备号和权限、lseek、poll、各种写读回来的前缀、标志和正文、writev、缓冲区放不下、pread、syslog(2) 读全部、普通用户的打开，再用 util-linux 和 busybox 的 `dmesg` 读这次写的几条。宿主的日志里有别的消息，所以每条都带随机的记号、只看带记号的；6.2 起一次能写 1024 字节（6.1 是 992），这之间的长度不比。
- 差分测试的仓库加装 util-linux 的 `dmesg`（Alpine 的 `dmesg` 包）。导出镜像时 `/tmp` 只留目录：开发机上仓库的 `/tmp` 里攒了 830 MB 历次运行留下的东西，原来都进了镜像，docker 的盘放不下了。
- rootfs 用例 `kmsg_records_reach_dmesg`（busybox 的 `dmesg`、限速、`printk_devkmsg`、`/proc/kmsg`），`cgroup_memory_files_follow_the_oom_killer` 加上 `dmesg` 里 OOM killer 的那两行；`tests/guest/waits.c` 加上 syslog 的读、`/proc/kmsg` 和 `/dev/kmsg` 的阻塞读（调试构建检查放开 BKL 时没有借着内核对象）。

## 推迟项

- seccomp 的 `SECCOMP_RET_LOG` 和 `actions_logged` 仍只写 tuxport 的调试日志：Linux 没有 auditd 时经由 audit 写进内核日志（`audit: type=1326 …`），可以照这一轮加上（[seccomp](59-seccomp.md) 的推迟项）。
- guest 进程因为未处理的 SIGSEGV 等结束时，Linux arm64 记一条 `<名字>[pid]: unhandled exception: …` 和寄存器（`show_unhandled_signals`），这里没有。
- OOM 的报告没有调用栈、`invoked oom-killer` 那一行和各个进程的表；`pgtables` 总是 0。
- 没有 `CONFIG_PRINTK_CALLER`、`/sys/module/printk/parameters/time` 这些开关；时间戳总带着。
- App 的标签页里仍没有"进程被结束"的提示（M9 的外壳）。→ 已做（[M9：进程被结束、服务没起来时提示](195-kill-reports.md)）
