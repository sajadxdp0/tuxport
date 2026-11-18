# M9 之前：PDEATHSIG 和 __WNOTHREAD（2026-10-31）

[task 目录的 children](134-task-children.md)那一轮记下父线程之后剩下的两条推迟项，都是 Linux 上按线程、tuxport 原来按进程的地方：

- `PR_SET_PDEATHSIG`：prctl(2) 写明这里的"父进程"是建立它的线程，那个线程退出（`pthread_exit`）就发，父进程还在也一样；设置本身也是线程的（`task_struct.pdeath_signal`），新线程、fork 出来的子进程都从 0 开始。tuxport 原来记在进程上，要等整个父进程结束才发；siginfo 是 `SI_KERNEL`，Linux 是 `SI_USER`、发送者是父进程。
- wait 系列的 `__WNOTHREAD`：只等调用的线程建的子进程（`do_wait` 只看这个线程的 `children`，不再看同一个线程组里别的）。tuxport 原来接受这个标志但不起作用。

## 做法

- **PDEATHSIG 按线程记**：从 `ThreadGroup` 挪到 `Task`（`core/kernel/src/task.rs`），prctl 的设置和读取都是调用的线程的；新线程、新进程的是 0，execve 保留，换了有效 ID、得到新能力的 execve 和特权执行清掉执行的那个线程的。setuid 一类改凭据时清掉进程里所有线程的：tuxport 的凭据按进程记，所有线程的一起变了，Linux 上 libc 也让每个线程各自 `commit_creds`、各自清掉。
- **什么时候发**（`process::pdeath_signals`、`send_pdeath`）：线程退出时，它建的子进程里每个设了 PDEATHSIG 的线程各发一个给它所在的进程，然后再把这些子进程交给进程里还活着的线程（Linux 的 `forget_original_parent`，先发后转）；进程的最后一个线程退出时，所有子进程照旧在结束进程的第 3 步里发。已经退出的线程设过的不再算。
- **siginfo**：`SI_USER`，`si_pid` 是父进程在子进程的 pid 命名空间里的编号，`si_uid` 是父进程的真实用户 ID 在子进程的用户命名空间里的值（`SEND_SIG_NOINFO`）。记着发送者：子进程的命名空间里看不见它时（pid 命名空间外面的父进程）编号是 0，按强制的信号处理，这个命名空间的 1 号进程设的 SIGKILL 照样能结束它（util-linux 的 `unshare --kill-child`、bwrap 的 `--die-with-parent`）。原来用 `SI_KERNEL` 也是靠它算强制的。
- **`__WNOTHREAD`**（`syscall/proc.rs` 的 `find_event`）：wait4、waitid 带它时，父线程不是调用者的子进程看不见；一个也剩不下时 ECHILD。

## 测试

- 差分测试 `compat2` 组新增 `pdeath.c`：哪个线程退出时发、siginfo、多个线程各设一个、execve 保留、新线程和 fork 读到 0、setuid 清掉、整个父进程结束时的孙进程；`__WNOTHREAD` 只等自己建的、ECHILD、线程退出之后转给主线程的子进程。两个执行层下都与原生相同。孙进程那一项放在最后：原生一侧这个程序是容器的 1 号进程，会收养这个孤儿。
- `namespaces` 组新增 `unshare -p -f --kill-child`：`kill -9` 掉 unshare 之后命名空间里的 1 号进程跟着结束。

## 推迟项

- `__WNOTHREAD` 对跟踪着的线程（ptrace-stop、跟踪者先取走的结束）仍按进程算：跟踪者记的是进程（[ptrace 的第一步](40-ptrace-strace.md)的推迟项）。
- 凭据仍按进程记：单独一个线程用原始的 setuid 系统调用（不经过 libc 的广播）时，Linux 只改那个线程的凭据、只清那个线程的 PDEATHSIG，这里整个进程的都变了（凭据原来就是这样）。
