# M9 之前：跟踪停住的进程、跟踪者先取走结束（2026-10-27）

[ptrace 的第一步](40-ptrace-strace.md)留下的两处与 Linux 不同的细节，都出在用 `strace -p`、gdb 的 attach 跟踪别人的进程时（§7.6）。

## 做法

- **跟踪已经停住的进程**：线程组被 SIGTSTP 这类停止信号停住、父进程已经用 wait 取走了停止事件之后，再 PTRACE_ATTACH 或者 SEIZE 它，原来 group-stop 报告成 SIGSTOP。原因是停止信号只记在"还没被父进程取走的停止事件"里。Linux 的每个线程在 `JOBCTL_STOP_SIGMASK` 里记着停住它的信号，跟父进程取没取走无关。现在线程组另记一份（`ThreadGroup::stop_signal`）。

  在原生上量了顺序，ATTACH 时还有一处不同：Linux 先报告 group-stop（SIGTSTP，没有 siginfo），再报告 ATTACH 发的 SIGSTOP。`get_signal` 每取一个信号之前先处理 `JOBCTL_TRAP_*`。tuxport 原来是线程在 group-stop 里被叫醒之后，直接去取挂起的信号，先报告了 SIGSTOP。

  现在 `signal::deliver` 的循环与 `get_signal` 一样，每取一个信号之前先等线程组的停止、再看 ptrace 要不要它停（group-stop、PTRACE_INTERRUPT）。原来这两步只在循环之前做一次，停止信号的默认动作另外写了一遍。
- **跟踪者不是父进程**：原来被跟踪的进程结束时，同时交给父进程（变成僵尸进程、发 SIGCHLD）和跟踪者，父进程马上就能回收它。Linux 的僵尸进程还被跟踪着时只有跟踪者看得见：
  - 只给跟踪者发 SIGCHLD；
  - 父进程的 wait 接着等（WNOHANG 返回 0），不是 ECHILD；
  - 跟踪者用 wait 取走之后（`wait_task_zombie` 的 `EXIT_TRACE`），才交给父进程、给它发 `exit_signal`；
  - 跟踪者没取走就退出时（`__ptrace_detach`），也交给父进程。

  现在 `ptrace::group_finished` 在这种情况下推迟交给父进程：进程是僵尸，记下 `exit_tracer`，父进程的 wait 跳过它。跟踪者 wait 取走（WNOWAIT 不算）或者退出时，再交给父进程（`process::tracer_released`）。父进程先退出了的话，这个僵尸进程交给 init，跟踪者取走之后由 init 回收。跟踪者 wait 取走整个进程的结束时，也与 Linux 一样报告它的资源用量（原来是 0）。

## 测试

差分测试 `ptrace` 新增两段。

- 父进程取走停止事件之后跟踪停住的子进程，ATTACH、SEIZE 时报告的信号和顺序。
- 祖父进程跟踪孙进程（Yama 也允许祖先跟踪），跟踪者取走之前、之后和没取走就退出时，父进程的 wait 和 SIGCHLD。

## 推迟项

- 跟踪 pid 1 仍是 ESRCH，跟踪者记的仍是进程（[ptrace 的第一步](40-ptrace-strace.md)的推迟项）。
- 被跟踪的线程（不是主线程）退出之后，Linux 让它作为僵尸等跟踪者取走，取走之前父进程回收不了整个进程（`delay_group_leader`），tuxport 不等。只有跟踪者不是父进程、又不马上取走线程的结束时才看得出来。
