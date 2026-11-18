# M9 之前：ptrace 和 procfs 的细节（2026-10-27）

[ptrace 的两步](40-ptrace-strace.md)留下几处没有和 Linux 对照、或者知道不同的细节，这一轮逐个在原生上量过再改（§7.6）。

## 做法

- **单步进入信号处理函数**：跟踪者在信号投递停止里用 PTRACE_SINGLESTEP 投递一个有处理函数的信号时，原来设置好帧之后接着执行了处理函数的第一条指令，停在第二条上。arm64 Linux 先快进单步状态（`user_fastforward_single_step`），再由 `tracehook_signal_handler` 停一次：停在处理函数的第一条指令上、还没执行，siginfo 是 `ptrace_notify(SIGTRAP)` 的格式（`si_code` 也是 SIGTRAP，发送者是线程自己），再单步才执行它、报告 `TRAP_TRACE`。现在一样（`ptrace::handler_entered`，事件为 0 的停住，跟踪者给的信号不投递）。
- **sigsuspend 里的屏蔽字**：记下的推迟项是"GETSIGMASK 不报告 sigsuspend 临时换掉的屏蔽字"，量下来正相反：Linux 在 `TIF_RESTORE_SIGMASK` 时读的是 `saved_sigmask`，也就是原来的屏蔽字，tuxport 读的是临时的。PTRACE_SETSIGMASK 在 Linux 上还会清掉这个标记（`clear_tsk_restore_sigmask`），帧里保存的、处理函数返回后恢复的就是跟踪者设的，tuxport 原来仍恢复原来的。`saved_sigmask` 从线程私有的 TaskState 挪到 `Task` 上（`Cell`），跟踪者能读能清，GETSIGMASK、SETSIGMASK 照 Linux 改。
- **`/proc/<pid>/mem` 跨 execve**：原来按 pid 每次读写现找地址空间，进程 execve 之后旧的 fd 读写的是新程序。Linux 打开时拿住那个 mm，execve 之后它没有用户了，读写得到 EOF。现在打开时记下地址空间的弱引用（`FileKind::Proc::mm`），释放了就是 EOF；重新打开拿到新的。
- **打开时要能跟踪**：原来只有 mem 检查（`PTRACE_MODE_ATTACH`），auxv 不检查。Linux 的 auxv、environ、maps、smaps、smaps_rollup、pagemap 都在打开时要求能以只读方式跟踪（`proc_mem_open`，`PTRACE_MODE_READ`），同一个用户、不可 dump（`PR_SET_DUMPABLE` 为 0、setuid 程序）的进程打不开；原生上量了 auxv、environ、mem、maps 都是 EACCES，tuxport 原来 auxv、environ、maps 都能打开。现在这几项一起检查，规则与 ptrace 的相同。

## 测试

差分测试 `ptrace` 新增两段：带着信号单步、sigsuspend 里的 GETSIGMASK 和 SETSIGMASK；execve 之后旧的 mem fd，不可 dump 的进程的 auxv、environ、mem、maps 打不开。

## 推迟项

- 跟踪 pid 1 仍是 ESRCH，跟踪者记的仍是进程，非父进程的跟踪者跟踪的进程结束时父进程仍可能先回收它，跟踪一个已经停住的进程时 group-stop 的信号仍报告成 SIGSTOP（[ptrace 的第一步](40-ptrace-strace.md)的推迟项）。→ 后两项已补上（M9 之前：跟踪停住的进程、跟踪者先取走结束）；跟踪 pid 1、跟踪者记的是进程仍没有，40 篇的 seccomp（已补上，M9 之前：seccomp）、PTRACE_PEEKSIGINFO、PTRACE_GET_RSEQ_CONFIGURATION 也没有列在这里
