# M9 之前：pid 命名空间（2026-10-30）

让 bubblewrap 能用的第五轮。bwrap 的 `--unshare-pid` 在 `clone` 里带上 `CLONE_NEWPID`，它自己（`--as-pid-1` 时是要运行的命令）成为新命名空间里的 1 号进程，再挂一个新的 procfs；Codex 每条命令都这样跑。原来 `CLONE_NEWPID` 是 EPERM。

## 做法

- **编号**（`core/kernel/src/pidns.rs`）：内核里的 pid、tid 照旧是全局的（开机时的命名空间里的编号），各处的查找、`groups`、`tasks` 都不用改。每个新建的命名空间记一张全局编号到本层编号的表（和反过来的），进程在它所在的命名空间和每个祖先里各有一个编号；开机时的命名空间不记表，全局编号就是它的编号。编号不再使用（线程退出、僵尸进程被回收、非主线程 execve 接过主线程的 tid）时从各层的表里去掉。本层编号从上次分配的下一个起找，用到 pid_max 之后从 300 起（Linux 的 `alloc_pid`、`RESERVED_PIDS`）；`/proc/sys/kernel/ns_last_pid` 读写的是读者、写者所在的命名空间的，`/proc/loadavg` 的最后一列也是。
- **系统调用的边界**：
  - 参数里的 pid 在分发时换成全局编号（Linux 的 `find_vpid`），看不到的换成一个不存在的编号，处理函数里原来的查找自然报各自的错（ESRCH、ECHILD、EPERM）；0、-1 不变，负数的进程组号取反再换。按参数的位置换的有 ptrace、process_vm_readv/writev、get_robust_list、sched_* 系列、kill、tkill、tgkill、rt_sigqueueinfo、rt_tgsigqueueinfo、setpgid、getpgid、getsid、prlimit64、wait4、waitid 的 `P_PID`/`P_PGID`、kcmp、pidfd_open、setpriority/getpriority 的 `PRIO_PROCESS`/`PRIO_PGRP`、fcntl 的 `F_SETOWN`；在结构体里的各自换：capget/capset 的头、`F_SETOWN_EX`、`FIOSETOWN`/`SIOCSPGRP`、`TIOCSPGRP`、timer_create 的 `SIGEV_THREAD_ID`。
  - 出口换成调用者那层的编号，看不到的是 0：getpid、getppid（1 号进程是 0）、gettid、set_tid_address、getpgid、getsid、setsid、`F_GETOWN`、`F_GETOWN_EX`、`FIOGETOWN`、`TIOCGPGRP`、`TIOCGSID`、`F_GETLK` 的 `l_pid`、SysV IPC 的 `msg_lspid`/`msg_lrpid`/`shm_cpid`/`shm_lpid`/`GETPID`、netlink 自动分配的 `nl_pid`；clone 的返回值和 `CLONE_PARENT_SETTID` 是父进程看到的，`CLONE_CHILD_SETTID` 是新线程自己看到的；wait4、waitid 的结果在回收之前换好（回收之后编号就不在表里了）；`SO_PEERCRED`、`SCM_CREDENTIALS` 是接收者看到的；robust futex 链表里比较的是线程自己看到的 tid；core 文件的 prstatus、prpsinfo 和文件名的 `%p`、`%i` 是进程自己看到的，`%P`、`%I` 是全局的（Linux 的 `format_corename`）。
- **siginfo**（`core/kernel/src/signal.rs`）：`SigInfo` 记下发送者的全局编号（不编码），si_pid 填发送者自己看到的编号，发给的进程所在的命名空间看不到发送者时改成 0（Linux 的 `send_signal_locked`；发给祖先命名空间里的进程时与 Linux 一样仍是发送者自己看到的编号）。SIGCHLD 和 waitid 的 si_pid 是子进程在父进程那层的编号（`do_notify_parent`），报告给跟踪者的按跟踪者的；ptrace 的 `PTRACE_GETEVENTMSG`（fork、vfork、clone、vfork-done 的新线程，exec 之前的 tid）按跟踪者的（`ptrace_event_pid`），停住时的 siginfo 按被跟踪的线程自己的，跟踪者换了信号时 si_pid 是被跟踪的线程看到的跟踪者；mq_notify 的 si_pid 按登记通知的进程的。
- **1 号进程**：命名空间里第一个登记的进程编号是 1，是它的 reaper。
  - 孤儿过继给退出的父进程所在的命名空间的 1 号进程（Linux 的 `find_child_reaper`），没有时（开机时的命名空间里、或者 1 号进程自己退出）给内置的 init。原来只有内置的 init，它收到的僵尸直接回收；现在过继给 1 号进程的僵尸与 Linux 的 `reparent_leader` 一样再通知一次新的父进程，退出信号改成 SIGCHLD。
  - 它不接收按默认方式处理的信号（Linux 的 `SIGNAL_UNKILLABLE`）：发送时没屏蔽、不被跟踪的直接丢掉（SIGCONT 让它继续这类副作用照做，`prepare_signal` 在判断忽略之前），祖先命名空间里发来的和内核产生的 SIGKILL、SIGSTOP 除外；屏蔽着的挂起，投递时再丢。按默认方式处理的同步信号（异常、断点、seccomp 的 SIGSYS）被强制投递时照常结束它（`force_sig_info_to_task` 去掉 `SIGNAL_UNKILLABLE`）。它不能 `CLONE_PARENT`（EINVAL）；kill(-1) 不发给它，只发给自己的命名空间里看得到的进程。
  - 它退出时命名空间里的进程（含更深的命名空间里的）都收到 SIGKILL，之后不能再在里面建进程（fork 是 ENOMEM）；它要等到它们都被回收了才算结束（Linux 的 `zap_pid_ns_processes`）：`finish_group` 拆成两半，报告给跟踪者、父进程的后一半先记在内核里，`release_pid` 发现命名空间空了再做。父进程的 wait 因此在里面的进程都没了之后才返回。它的子进程交给内置的 init，被 SIGKILL 结束后自动回收（Linux 上是退出中的 1 号进程自己在 `zap_pid_ns_processes` 里回收，结果一样）。
- **新建和进入**（`core/kernel/src/syscall/ns.rs`、`syscall/proc.rs`）：
  - clone 的 `CLONE_NEWPID` 要在（可能是新建的）用户命名空间里有 `CAP_SYS_ADMIN`（EPERM），和 `CLONE_THREAD` 同用是 EINVAL；照 Linux 6.1 的 `copy_pid_ns`、`create_pid_namespace`：给子进程用的命名空间不是自己所在的（unshare 了还没进去）是 EINVAL，属主要是上一层的属主或者它的后代（EPERM），最多 32 层（ENOSPC）。建线程要求给子进程用的命名空间就是自己所在的，所以 unshare 了之后不能建线程。
  - unshare 的 `CLONE_NEWPID` 只改调用的线程之后的子进程所在的命名空间（Linux 的 `nsproxy->pid_ns_for_children`），自己不进去；它按线程记在 `Task` 上，`/proc/<pid>/task/<tid>/ns/pid_for_children` 要读别的线程的。
  - setns 到 pid 命名空间（`pidns_install`）：要在它的属主和自己的用户命名空间里都有 `CAP_SYS_ADMIN`（EPERM），只能是自己所在的或者它的后代（EINVAL），改的也是之后的子进程所在的命名空间。
  - clone3 的 `set_tid`（原来一律 EINVAL）照 Linux 的 `alloc_pid`：从新进程所在的那层起往外指定各层的编号，层数不能比那里的多，编号在 1 到 pid_max 之间、命名空间里还没有进程时只能是 1（EINVAL），要在那层的属主里有 `CAP_SYS_ADMIN` 或 `CAP_CHECKPOINT_RESTORE`（EPERM），用掉了的是 EEXIST；最外层也指定了时全局编号就用它。
  - `/proc/<pid>/ns/pid` 是那个进程的，`pid_for_children` 是那个线程的，unshare 了、里面还没有进程时没有（ENOENT，Linux 的 `pidns_for_children_get` 看有没有 reaper）。
- **procfs**：mount(2) 挂上的每个 procfs 实例记下挂载的进程所在的 pid 命名空间（按设备号，bind 挂载与源共用；开机时的是开机时的命名空间），里面的编号都按它算（Linux 的 `proc_pid_ns`）。合成文件系统的接口 `name`、`lookup`、`readlink` 和 `readdir`、`content` 因此带上挂载编号。`/proc` 只列那个命名空间里看得到的进程，按那里的编号排；`/proc/<n>` 按那里的编号找；读者在里面看不到时 `self`、`thread-self` 是 ENOENT；`stat` 的 pid、ppid、pgrp、session、tpgid，`status` 的 Tgid、Pid、PPid、TracerPid 按它算，`status` 新增 Linux 有的 `NStgid`、`NSpid`、`NSpgid`、`NSsid` 四行（从挂载的命名空间到进程所在的那个，每层一列；原来没有这几行）；pidfd 的 fdinfo 的 `Pid`、`NSpid` 也按它。

## 测试

- 单元测试 `pid_namespaces`：各层的编号和分配、指定的编号、1 号进程退出之后 ENOMEM；`pid_namespace_limits`：层数、EINVAL 和 EPERM。
- 差分测试 `namespaces` 组加上 pid 命名空间：busybox 的 `unshare -p -f --mount-proc` 和在里面 `strace -f`；Python 里 unshare、setns、clone3 的 `set_tid` 和它们的错误，1 号进程的编号、过继和回收、它收到的信号和 si_pid、它退出时里面的进程被结束，新挂的 procfs 从里面和外面看，`SO_PEERCRED`。两个执行层下都与原生相同。docker 的容器本身在一个 pid 命名空间里，原生一侧少嵌套一层，所以嵌套只比错误，层数在单元测试里查。
- 开发中用例暴露的两处：wait 的结果在回收之后才换算时是 0，busybox 的 unshare 等子进程时 wait4 一直返回 0、空转；1 号进程的过滤放在 `prepare_signal` 那一步之前时，里面发的 SIGCONT 让不了它继续，用例卡住。
- `scripts/ltp` 给 LTP 的内核配置加上 `CONFIG_NAMESPACES`、`CONFIG_USER_NS`、`CONFIG_PID_NS`，clone11、clone301、clone304、ioctl_ns05、unshare05 由跳过变成通过。

## 推迟项

- 命名空间文件不让 pid 命名空间活着（与[挂载命名空间](118-mount-namespaces.md)的推迟项相同）；`NS_GET_PID_FROM_PIDNS` 这些 ioctl 没有，setns 仍不接受 pidfd。→ 命名空间文件留住 pid 命名空间已补上（M9 之前：命名空间文件留住命名空间），1 号进程退出了的照样 setns 进得去、fork 是 ENOMEM；`NS_GET_PID_FROM_PIDNS` 这些是 6.1 之后才有的，tuxport 报告 6.1，与它一样是 ENOTTY（6.1 有的 `NS_GET_PARENT` 等四个已补上，同一轮）；setns 接受 pidfd 已补上（M9 之前：setns 经由 pidfd）
- `/proc/sysvipc/*` 里的 pid 仍是全局的（Linux 按 procfs 实例的命名空间）；没有 `PR_SET_CHILD_SUBREAPER`，孤儿只过继给 1 号进程或内置的 init。 → `PR_SET_CHILD_SUBREAPER` 已补上（M9 之前：PR_SET_CHILD_SUBREAPER）；`/proc/sysvipc/*` 里的 pid 仍是全局的
- 写 `ns_last_pid` 只有文件本身的权限检查（root 才能写），没有 Linux 按命名空间属主查的 `checkpoint_restore_ns_capable`。
- siginfo 的 `si_uid` 仍按内核 ID（[用户命名空间](119-user-namespaces.md)的推迟项）。→ 已补上（M9 之前：用户命名空间里的 ID 换算）
- `max_pid_namespaces` 只读、不实际限制。→ 可以写了（M9 之前：网络命名空间），仍不实际限制
