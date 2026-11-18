# M9 之前：跟踪者退出时丢掉已经取走的信号（2026-10-29）

[第 92 轮](92-debuggers-valgrind-chromium.md)用 delve attach 到一个 Go 进程，执行 `exit`。delve 问要不要结束进程，stdin 是 /dev/null 时读到 EOF，`handleExit` 不 detach 就返回 2，dlv 退出。之后那个 Go 进程的每个线程都停在 T，TracerPid 是 0，SIGTERM 也结束不了它（Go 自己处理 SIGTERM，进程停着，处理函数跑不了），普查的脚本在 `wait` 上一直等到超时。

## 原因

- 跟踪者退出时内核替它 detach。tuxport 照着 ptrace(2) 的 "If the tracee is restarted from signal-delivery-stop, the pending signal is injected"（[第 40 轮](40-ptrace-strace.md)），把信号投递停止里的信号投递出去。这里那个信号是 attach 送的 SIGSTOP，进程于是进入 group-stop。
- 在原生的 Linux 6.8 上试的结果不一样。跟踪者先用 wait 取走这次停住、再退出时，信号被丢掉，进程继续运行：
  - SIGSTOP 不让它停住；
  - 有处理函数的 SIGUSR1，处理函数不运行；
  - SIGTERM 不结束它。

  还没 wait 就退出时，信号照常投递。
- 原因在 Linux 的 `wait_task_stopped`：wait 取走 ptrace-stop 时把 `p->exit_code` 清成 0（`WNOWAIT` 除外）。被跟踪的线程醒来后从 `ptrace_stop` 拿到的就是 0。ptrace(2) 那句话只在停住还没被取走时成立。
- 调试器、strace 都先 wait 再处理，所以实际碰到的几乎都是"已经取走"。

## 做法

改在 `core/kernel/src/ptrace.rs` 的 `detach`：跟踪者退出时，这次停住已经被 wait 取走（`Trace::reported`，`WNOWAIT` 不算）的，让它以信号 0 继续；没取走的照旧投递。PTRACE_DETACH 不变，按它的参数投递。group-stop 里的仍然停着，与原生相同。

## 测试

- 差分测试 ptrace 组新增一个 C 程序：跟踪者在已经 wait 取走、还没取走的信号投递停止和 group-stop 里退出之后，处理函数有没有运行、进程的状态、有没有被结束，与原生逐行相同。原生的容器没有 `CAP_SYS_PTRACE`、Yama 的 `ptrace_scope` 是 1，所以被跟踪的进程先 `PR_SET_PTRACER_ANY`；tuxport 上它返回 EINVAL，与没有 Yama 的 Linux 一样。
- 第 92 轮最初卡住的用法，delve 读到 EOF 退出，现在进程照常运行。

## 推迟项

- 停在系统调用的停止、`PTRACE_EVENT_*` 上时跟踪者退出，这里让它以 0 继续（原来也是）。Linux 上没取走的话，`ptrace_report_syscall` 会把 `exit_code` 当作信号发给自己。这种情况没有对照过。
- 用 `WNOWAIT` 取过、没有真正取走的停住，这里照常投递。这是按 Linux 的源码定的，没有和原生对照。
