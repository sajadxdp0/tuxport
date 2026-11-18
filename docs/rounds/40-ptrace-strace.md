# M9 之前：ptrace 的第一步，strace（2026-10-27）

ptrace 原来一律返回 EPERM，gdb、strace 都不能用（[M6](08-m6.md)、[M8](10-m8.md) 起推迟到 v1 之后）。分两步做，这一轮先让 strace 能用；单步和改写被跟踪进程的内存（gdb 要的）留给下一步。设计见 §7.6。

## 做法

- **被跟踪的一方**（`ptrace.rs`）：被跟踪的线程在投递信号之前、group-stop、系统调用的入口和出口（PTRACE_SYSCALL）、`PTRACE_EVENT_*`（fork、vfork、clone、exec、vfork 结束、退出、`PTRACE_EVENT_STOP`）停在 ptrace-stop 里，通知跟踪者（叫醒它的 wait，发 `CLD_TRAPPED` 的 SIGCHLD），只等自己的事件。停住时把寄存器存一份，跟踪者读写这一份，继续时写回 CPU 状态。
  - 系统调用的停止照 arm64 Linux：x7 在入口是 0、出口是 1，跟踪者看不到也改不了它原来的值；入口处的编号可以经由 `NT_ARM_SYSTEM_CALL` 改，改成 -1 时跳过这个系统调用；跟踪者在系统调用停止时注入的信号当作新信号发给自己。execve 成功之后先报告 `PTRACE_EVENT_EXEC`（没打开它、又不是 SEIZE 跟踪的给自己发 SIGTRAP），再报告系统调用的出口；rt_sigreturn 的出口报告恢复出来的寄存器；exit、exit_group 只有入口，打开了 `PTRACE_EVENT_EXIT` 时退出之前停一次（exit_group 已经要求所有线程退出，调用者自己仍然停下来报告）。
  - 信号投递停止之后，跟踪者可以换掉信号或者不投递；换了信号又没有用 PTRACE_SETSIGINFO 给 siginfo 时，当作跟踪者用 kill 发的；换成的信号被屏蔽着就重新挂起。跟踪者给的 siginfo 原样投递（`InfoFields::Raw`）。
  - 被跟踪的线程不丢弃被忽略的信号（SIGCHLD、SIGCONT 这些默认忽略的也是），跟踪者要看到它们。group-stop 时被跟踪的线程停在 ptrace-stop 里报告，跟踪者让它继续它就继续执行，即使线程组仍然停着；阻塞着的系统调用因此只在要参加 group-stop、或者跟踪者 PTRACE_INTERRUPT 时提前返回（`signal::interrupted`）。SEIZE 跟踪的报告成 `PTRACE_EVENT_STOP`，PTRACE_LISTEN 之后 SIGCONT 让它重新报告一次（`SIGTRAP`）。
  - clone 时父线程被跟踪、打开了对应的事件（或者用了 `CLONE_PTRACE`）、没有 `CLONE_UNTRACED`，新线程由同一个跟踪者跟踪，执行第一条指令之前先停下：SEIZE 的停一次 `PTRACE_EVENT_STOP`，否则挂一个 SIGSTOP。
  - 被跟踪的线程退出时：不是主线程的单独报告给跟踪者（`Kernel::trace_exits`）；主线程记下跟踪者，整个进程结束时跟踪者不是父进程的话也报告给它。跟踪者退出时，设了 `PTRACE_O_EXITKILL` 的被跟踪线程被结束，其余的不再被跟踪；停在信号投递停止里的信号照常投递，跟踪者改过的寄存器照样写回（PTRACE_DETACH 也是）。→ 已改正（M9 之前：跟踪者退出时丢掉已经取走的信号）：停住已经被 wait 取走的，Linux 上信号丢掉
  - 跟踪者用 PTRACE_SYSCALL 让线程继续之后，它的系统调用都交给持有 BKL 的路径（`Task::syscall_traced`，不持 BKL 的路径读它）；vDSO 里读时钟的存根不停，与 Linux 上它们不进内核、strace 看不到一样。
  - 被跟踪的线程停着时 `/proc/<pid>/stat`、`status` 的状态是 `t (tracing stop)`，`TracerPid` 是跟踪者。
- **跟踪者的一方**（`syscall/ptrace.rs`）：PTRACE_TRACEME、ATTACH、SEIZE、INTERRUPT、LISTEN、DETACH、CONT、SYSCALL、KILL、SETOPTIONS、GETEVENTMSG、GETSIGINFO、SETSIGINFO、GETREGSET、SETREGSET、GETSIGMASK、SETSIGMASK、GET_SYSCALL_INFO、PEEKTEXT、PEEKDATA。错误码按 Linux：不是自己跟踪的、或者没有停着（LISTEN 时也算没停）是 ESRCH，SEIZE 不认得的选项是 EIO、SETOPTIONS 的是 EINVAL，regset 的长度不是元素大小的整数倍是 EINVAL，PEEKUSR 这些 arm64 上没有的请求是 EIO。
  - 跟踪者记的是进程（tgid），请求可以来自跟踪者进程里的任何线程（Linux 只接受跟踪者线程自己）：tuxport 不记 fork 的是哪个线程，PTRACE_TRACEME 的跟踪者就是父进程。
  - regset 有 `NT_PRSTATUS`、`NT_PRFPREG`、`NT_ARM_TLS`、`NT_ARM_SYSTEM_CALL`，`NT_ARM_HW_BREAK`、`NT_ARM_HW_WATCH` 报告 0 个槽（调试器改用软件的），可以只读写前面一部分。
  - wait 里，跟踪着的线程（不一定是子进程）的 ptrace-stop 不要求 `WUNTRACED`、也不要求 `__WALL`，与 Linux 一样；主线程被自己跟踪着的子进程，停止和继续只按 ptrace-stop 报告一次。报告 ptrace-stop 时带上被跟踪进程的 rusage：strace -c 用它算每个系统调用的时间，原来全是 0。
- **process_vm_readv、process_vm_writev**：权限与 PTRACE_ATTACH 相同（`PTRACE_MODE_ATTACH_REALCREDS`）。远端按页拷贝，碰到不能访问的页就停在那里，返回已经拷了多少，一字节也没拷时是 EFAULT。strace 用它读字符串参数。
- **信号的重启**：跟踪者改了 pc 时不再按"重新执行"把它改回去，与 Linux 的 `do_signal` 一样。
- 真实负载上的 strace（Tier 1）：`strace -f` 跟踪 python 的线程、`go version`、`node -e`，`strace -c`、`strace -p` 挂到运行中的 sleep 上（显示 `restart_syscall(<... resuming interrupted nanosleep ...>)`）、`-e inject=getpid:retval=7`、`-k` 打印调用栈，都正常。

## 结果

没有被跟踪时，系统调用只多看一次跟踪状态，开销在噪声范围内。

## 测试

新的差分测试 `ptrace`：一个 C 写的跟踪器逐项检查系统调用的停止、各种事件、SEIZE 和 group-stop、被忽略的信号、线程、`PTRACE_O_EXITKILL`、process_vm_readv/writev 和 PEEKDATA、错误码；再用 strace 跟踪几个程序，地址、pid 归一化之后比。跟踪器设了 60 秒的 alarm，等不到该来的停止时失败而不是卡住。差分测试的仓库加装 strace 和 gdb。guest 测试 `posix.c` 新增 ptrace 一项。LTP 新通过 process_vm_readv01–03、process_vm_writev01–02、ptrace01、03、05、06；ptrace04、07–10 只测 x86（arm64 的原生 Linux 上也跳过），ptrace11 要跟踪 pid 1（见推迟项）。

## 推迟项

- PTRACE_SINGLESTEP、PTRACE_POKETEXT/POKEDATA（改写被跟踪进程的内存，下断点要用）、`/proc/<pid>/mem`：gdb 要的，下一步做。→ 已补上（M9 之前：ptrace 的第二步，gdb）
- 跟踪 pid 1 是 ESRCH（LTP 的 ptrace11）：内置的 init 不是 guest 进程。
- 跟踪者记的是进程：跟踪者线程退出时，它跟踪的线程不会像 Linux 那样被放开，要等整个跟踪者进程退出；请求可以来自跟踪者进程里的任何线程。
- 跟踪者不是父进程时，被跟踪的进程结束后父进程马上就能回收它（Linux 等跟踪者先取走它的结束）。→ 已补上（M9 之前：跟踪停住的进程、跟踪者先取走结束）；被跟踪的非主线程退出时 Linux 也让父进程等跟踪者取走它（`delay_group_leader`），仍没有
- 没有 seccomp，`PTRACE_O_TRACESECCOMP` 不会触发，`PTRACE_O_SUSPEND_SECCOMP` 是 EINVAL；PTRACE_PEEKSIGINFO、PTRACE_GET_RSEQ_CONFIGURATION 是 EIO。PTRACE_GETSIGMASK 不报告 sigsuspend 临时换掉的屏蔽字。→ 查明是反过来：Linux 报告原来的屏蔽字（`TIF_RESTORE_SIGMASK` 时读 `saved_sigmask`），tuxport 报告的是临时的；已改成与 Linux 相同，SETSIGMASK 之后也不再恢复原来的（M9 之前：ptrace 和 procfs 的细节）；seccomp 已补上，`PTRACE_O_TRACESECCOMP`、`PTRACE_O_SUSPEND_SECCOMP`、PTRACE_SECCOMP_GET_FILTER、PTRACE_SECCOMP_GET_METADATA 与 Linux 相同（M9 之前：seccomp）；PTRACE_PEEKSIGINFO、PTRACE_GET_RSEQ_CONFIGURATION 仍没有
- 跟踪一个已经被停住的进程时，group-stop 的信号在父进程取走之后记不得，报告成 SIGSTOP。→ 已补上（M9 之前：跟踪停住的进程、跟踪者先取走结束），ATTACH 时先报告 group-stop、再报告 ATTACH 发的 SIGSTOP 的顺序也与 Linux 相同了
