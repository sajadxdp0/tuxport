# M9 之前：cgroup.freeze（2026-11-01）

[podman 和 docker 的普查](157-container-survey.md)的问题 9 的第二步的后一半：`docker pause`、`podman pause`、runc/crun 的 `pause` 写 `cgroup.freeze`，systemd 冻结单元也用它。[cgroup 树](163-cgroup-tree.md)里写 1 是 EOPNOTSUPP。照 Linux 6.1 的 `kernel/cgroup/freezer.c`：冻结不是停止信号，父进程、wait 看不到，等待中的系统调用解冻后接着做。

## 做法

- **要求冻结**（`cgroup.rs`）：cgroup 记着自己的 `cgroup.freeze`（读到的是它），它或者祖先要求冻结时里面的进程都要停下（`Cgroups::freezing`）。进程记着该不该冻结（`ThreadGroup::frozen`），在三处更新、变了就叫醒（kick）它的线程：写 `cgroup.freeze` 时子树里的进程；迁移（写 `cgroup.procs`、`cgroup.threads`）时那个进程，迁移进冻结的 cgroup 就停下、迁移出来就接着运行（`cgroup_freezer_migrate_task`）；fork 时子进程随它要去的 cgroup，包括 clone3 的 `CLONE_INTO_CGROUP`，建好之后 kick 一下，第一次运行就进内核停下（`cgroup_post_fork`）。
- **停下**（`signal.rs`）：线程在返回 guest 之前投递信号的地方停下，与停止信号用同一处等待（Linux 的 `do_freezer_trap`），被跟踪的也停；被结束（SIGKILL、`cgroup.kill`、OOM）时离开，别的信号留到解冻之后处理。等待中的系统调用把冻结当作被打断（`interrupted`）：与没有处理函数的信号一样返回 ERESTART\*，解冻之后重新执行，nanosleep 经由 restart_syscall 按原来的到期时刻接着睡。停着的线程状态是 S（在内核里等待），不占 CPU，父进程收不到 SIGCHLD，`waitpid(WUNTRACED)` 看不到。
- **`cgroup.events` 的 frozen**：要求冻结，并且子树里的线程都已经停下了（要退出的不算）。空的 cgroup 一要求就是冻结的；子 cgroup 跟着祖先冻结，但它自己的 `cgroup.freeze` 仍是 0。写 `cgroup.freeze` 之后马上返回，frozen 等线程都停下了才变（runc 等着读到它）。
- **被打断之后怎样重新执行**：写用例时发现三处与 Linux 不同，没有处理函数的信号（停止之后又继续）原来也是这样，冻结又解冻之后就很明显，一起改了（signal(7) 列的就是这几个）：
  - **epoll_wait、epoll_pwait** 被打断是 EINTR，不重新执行（Linux 的 `ep_poll`），原来是 ERESTARTNOHAND，停止又继续之后从头再等一遍。epoll_pwait 的临时屏蔽字与 Linux 的 `restore_saved_sigmask_unless(error == -EINTR)` 一样留给信号投递恢复。
  - **ppoll、pselect6** 与 Linux 的 `poll_select_finish` 一样把剩下的时间写回 guest 给的 timespec（给了、不是 0 时），被打断时也写，重新执行时就按剩下的时间等；写不了时被打断改成 EINTR。原来 ppoll 不写，pselect6 只在成功时写，重新执行都从头再等。
  - **有超时的 futex 等待**被打断时返回 ERESTART_RESTARTBLOCK，记下绝对的到期时刻（Linux 的 `futex_wait_restart`）：没有处理函数时由 restart_syscall 接着等到原来的时刻，有处理函数时是 EINTR，即使有 SA_RESTART。原来是 ERESTARTSYS，重新执行时相对的超时从头算，SA_RESTART 时也重新执行。不持 BKL 的快速路径睡过一阵再交给持锁的路径时，像 nanosleep 一样交出算好的到期时刻。`restart_syscall` 记的东西（`TaskState::restart`）改成枚举，有 nanosleep 和 futex 两种。

## 测试

- 新的差分测试组 `cgroup-freeze`（原生一侧是特权容器）：阻塞在各种系统调用里的和死循环的进程冻结、解冻前后的状态、信号和结果，冻结期间迁移进出、clone3 放进来的进程，子 cgroup 的冻结，结束冻结的进程。两个执行层下都与原生逐字相同。读子进程的结果最多等 3 秒、整个程序有 60 秒的 alarm，内核不对时用例失败而不是卡住（第一次做变异检查时卡住了）。
- `edge-cases` 组新增停止又继续之后的 poll、epoll_wait 和有超时的 futex 等待，改之前 tuxport 在这三处都与原生不同。
- 在 tuxport 里实际跑 `docker pause`、`podman pause` 要等容器起得来（下一步：eBPF 的设备过滤）。→ 起得来之后 `docker pause` 先撞上伪文件读不到写过的值，改了之后正常（M9 之前：cgroup 的设备过滤（eBPF））

## 推迟项

- 被跟踪、停在 ptrace-stop 里的线程不算已经停下，这时 `cgroup.events` 的 frozen 一直是 0；Linux 把停止、ptrace-stop 中的任务都算作冻结了的。
- 不经过内核层等待的地方停不下来：卡在一次宿主调用里的线程（比如读一个阻塞的宿主文件）、vfork 之后等子进程的父进程（与 Linux 一样只有致命信号能打断，但 Linux 的那一处可以冻结），要等它们回来，frozen 才变成 1。
- `cgroup.events` 变了不通知（inotify、poll），与 [cgroup 树](163-cgroup-tree.md)的推迟项相同：等 frozen 的程序要轮询（runc 本来就是）。
- 解冻之后 Linux 的 `ERESTART_RESTARTBLOCK` 还有几个用户：clock_nanosleep 的绝对时刻（这里重新执行同一个调用，结果相同）、poll（arm64 上没有这个系统调用）。别的被打断的系统调用（accept、connect、wait4……）的重启码原来就照 Linux，这一轮没有逐个核对。
