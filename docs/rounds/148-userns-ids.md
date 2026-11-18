# M9 之前：用户命名空间里的 ID 换算（2026-10-31）

[用户命名空间](119-user-namespaces.md)那一轮把 getuid、stat、`/proc/<pid>/status` 这些按调用者的命名空间换算了，记下几处还按内核 ID 报告：`SO_PEERCRED`、`SCM_CREDENTIALS`、siginfo 的 `si_uid`、SysV IPC 的属主、core dump 里的 ID。只有映射改了 ID 时（`unshare -r`、rootless 的容器、`0 1000 1` 这类映射）看得出来，比如 ssh-agent、D-Bus 拿对端的 uid 和自己的 getuid 比，会拒绝连接。这一轮照 Linux 6.1 补上，顺带查出几处与命名空间无关的老问题：SIGCHLD 和 waitid 的 si_uid 总是 0，没有声明凭据时附带的 `SCM_CREDENTIALS` 用的是有效 ID，发送者在 `SCM_CREDENTIALS` 里声明的凭据被直接忽略（注释还写着"所有进程都是 root"）。

## 做法

- **SO_PEERCRED**（`cred_to_ucred`）：记下的对端有效 ID 按读者的命名空间换算。
- **SCM_CREDENTIALS**：
  - 发送（`__scm_send`、`scm_check_creds`）：长度不是 `CMSG_LEN(sizeof(struct ucred))` 是 EINVAL；声明的 ID 在自己的命名空间里映射不到是 EINVAL；pid 要是自己（或者在自己的 pid 命名空间的属主那里有 CAP_SYS_ADMIN），用户 ID 要是自己的真实、有效、保存的之一（或者在自己的命名空间里有 CAP_SETUID），组 ID 同样（CAP_SETGID），否则 EPERM；pid 不存在是 ESRCH。通过的按声明的发（原来忽略，总是发送者自己）。
  - 没有声明时附带发送者的 pid 和**真实**用户 ID、组 ID（`maybe_add_creds` 的 `current_uid_gid`），原来是有效 ID。
  - 接收（`scm_recv`）：按接收者的命名空间换算，映射不到的是 65534。
- **si_uid**（`SigInfo::sent_by`、`retarget_uid`）：
  - Linux 的 `send_signal_locked`：kill、tkill、pidfd_send_signal、rt_sigqueueinfo 这些带 si_uid 的（kill、SIGCHLD、实时信号的布局），si_uid 是发送者按自己的命名空间填的（sigqueue 是调用者给的值）；目标在别的用户命名空间里时，先在发送者的命名空间里映射回内核 ID（映射不到的是无效 ID），再按目标的换算，同一个命名空间里原样投递。SigInfo 因此记下 si_uid 是按哪个命名空间填的、对应的内核 ID，在 `for_target` 里换算。
  - SIGCHLD：si_uid 是子进程的真实用户 ID 在父进程（或跟踪者）的命名空间里的值，原来总是 0。退出时的不再换算（`do_notify_parent` 用 `__send_signal_locked`）；停止、继续时的经过 `send_signal_locked`，当作子进程自己发的再换算一次：子进程在别的用户命名空间里、那里映射不到父进程看到的值时成了 65534，原生上也是这样。
  - PDEATHSIG（原来就按目标换算）、exec 时给被跟踪者的 SIGTRAP、跟踪者换了信号时的 si_uid（`ptrace_signal`）、ptrace 停住时的 siginfo：按被跟踪者自己的命名空间。POSIX 消息队列的通知：按登记通知的进程现在的命名空间填（Linux 用登记时的），再当作发送者发的换算。
- **waitid**：si_uid 是子进程的真实用户 ID 在调用者的命名空间里的值，原来总是 0。跟踪者取走的结束也是（`TraceExit` 记下退出时的凭据）。
- **SysV IPC**：`IPC_STAT` 和 `*_STAT` 的 `ipc_perm`（uid、gid、cuid、cgid）按调用者换算（`kernel_to_ipc64_perm`）；`IPC_SET` 给的属主、属组按调用者的命名空间换成内核 ID，映射不到是 EINVAL（`ipc_update_perm`；开机时的命名空间里只有 -1 映射不到）；`/proc/sysvipc/*` 按读者换算（`seq_user_ns`）。
- **别的**：auxv 的 `AT_UID`、`AT_EUID`、`AT_GID`、`AT_EGID`（`create_elf_tables`）和 core dump 的 `NT_PRPSINFO` 的 `pr_uid`、`pr_gid`（`fill_psinfo`）按进程自己的命名空间；`/proc/net/tcp`、`udp` 这些的 uid 列按读者。

## 测试

- 新的差分测试组 `userns-ids`：同一个命名空间里 `SCM_CREDENTIALS` 的声明和权限；改了 ID 的映射之后，两边看到的 `SO_PEERCRED`、`SCM_CREDENTIALS`、kill 和 sigqueue 的 si_uid、SysV 消息队列的属主、`/proc/net/tcp` 的 uid、auxv、SIGCHLD 和 waitid 的 si_uid。
- 单元测试 `si_uid_follows_the_user_namespace`（`signal`）。
- 全量运行时原生一侧失败过一次：子进程继续之后马上退出，父进程再用 waitid 等继续的事件时它已经是僵尸，Linux 返回 ECHILD；用例改成子进程等父进程取走继续的事件再退出。

## 推迟项

- POSIX 消息队列的通知按登记通知的进程现在的用户命名空间填 si_uid，Linux 用登记时的（`notify_user_ns`）。
- 停止、继续时的 SIGCHLD 当作子进程自己发的再换算一次：Linux 用的是当时的 current，通常就是子进程，由别的线程、进程触发的情形没有分开。
- 没有声明凭据、两边都没开 `SO_PASSCRED` 时 Linux 不附带凭据，之后打开 `SO_PASSCRED` 收到的是 pid 0、65534；tuxport 总是附带发送者。
- `SO_PEERGROUPS` 没有实现（ENOPROTOOPT）。
