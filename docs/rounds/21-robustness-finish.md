# M9 之前：稳健性的收尾（2026-10-25）

上一轮兜住了运行循环里的内核层 panic 和引擎的宿主故障，还剩三处：线程开始、退出时的 panic 仍然终止宿主进程；定时器线程和宿主调用（Android 上是 JNI 调用）里的 panic 没有处理；App 进程里经过 libsigchain 的恢复没有直接测过。这一轮补上（§12 第 5 条，`core/kernel/src/bug.rs`）。

## 做法

- **统一的兜底**：`bug::catch` 执行一段内核代码，panic 时返回 None，由调用方收拾；不兜的时候（debug 构建的默认）直接终止宿主进程。运行循环原来的写法也改用它。
- **线程开始**：取 CPU 时钟和宿主 tid、设置 nice 和亲和性、读地址空间的代数这一段兜住，出错时和进入内核时一样用 SIGSEGV 结束这个进程，线程直接走退出的路径。
- **线程退出**：分几步分别兜住：结算（翻译统计、CPU 时间、驻留内存的峰值）并离开地址空间（robust futex、`clear_child_tid`、vfork）；结束进程（`finish_group`）；释放资源（地址空间、fd 表）；AF_UNIX 的垃圾回收。一步出错时，还有别的线程就结束这个进程，后面的步骤照样做，所以线程总会从内核里注销，进程总会结束，等它的父进程和宿主不会卡住。
  - `finish_group` 自己出错时退回到最要紧的两件事：记下宿主等着的结束方式，把进程交给父进程（变成僵尸进程并发 SIGCHLD，或者直接回收）。后一件从 `finish_group` 里提成 `hand_to_parent`，两边共用。子进程交给 init、孤儿进程组的信号这些就丢掉了。
  - 进程已经调用了 exit_group 时，它定下的退出码不变，和 Linux 的 oops 一样。
- **定时器线程**：原来这里 panic 会让定时器线程悄悄退出，之后所有进程的 alarm、POSIX 定时器、OOM killer、SIGIO 都不再工作。
  - `itimer::expire` 改为每个进程单独处理、单独兜住，这个进程的信号也在里面发。出错时清掉它的定时器，用 SIGSEGV 结束它，别的进程照常。
  - 内存上限的检查和信号驱动的 I/O 分不出是哪个进程的，出错时关掉这项功能：上限改为不限，清掉 `O_ASYNC` 的登记，并报告一行。
- **宿主调用**：`Machine` 的方法和 `import_rootfs`、`write_file` 经由 `bug::host_call` 执行，出错时只让这次调用失败（EIO 或新加的 `RunError::Internal`）。JNI 调用随之抛出 IOException，panic 不会穿过 JNI 边界让整个 App abort。
  - 读写控制台、改窗口大小时出错，还像终端挂断一样关闭那个控制台，会话组长收到 SIGHUP，标签页随之结束，不会卡在坏掉的控制台上。
  - `open_console` 因此改为返回 `Result`，JNI 的 openConsole 失败时也抛出 IOException（`TerminalSession` 本来就处理 `Pty.start` 的 IOException）。
  - 只读一张表的 `wait`、`try_wait`、`dbt_stats` 不需要兜。
- **测试钩子**：syscall `0xa991` 新增四种，用 `bug::trap` 布下：调用的线程退出时、这个进程下一个开始运行的线程、定时器线程处理这个进程时、宿主下一次调用时 panic。`bug::trap` 在没有测试钩子的构建里是空函数。测试钩子改为在 debug 构建或者打开 `test-hooks` 特性时存在：Android 的原生库总是 release 构建，`app/build.gradle.kts` 改为按变体分别构建 Rust 部分，debug 版 App 打开这个特性，release 版没有（检查过 .so 里没有测试钩子的字符串）。

## 结果

正常路径上只多了几次 `catch_unwind`（不 panic 时没有开销）和一次 `bug::trap`（release 构建里是空函数），开销在噪声之内。

## 测试

- `tests/guest/bugs.c` 新增线程退出时、新线程开始时、定时器线程上的 panic，子进程都被 SIGSEGV 结束；之后父进程的 `ualarm` 照常到期，说明定时器线程还在。
- 新用例 `host_calls_survive_kernel_bugs` 在测试进程里直接用 `Machine`，检查 spawn 和读控制台出错之后照常新开控制台运行程序。内核层只在第一次撞上 bug 时读 `TUXPORT_ON_BUG`，所以这个用例在设置了环境变量的子进程里重新运行自己。几个用例同时编译同一个 guest 程序时会运行到写了一半的文件，编译改为先写临时文件再改名。
- Android：`MachineTest` 新增 `bugsKillOnlyTheGuestProcess`，把手工拼的三个静态 ELF 写进仓库，在 App 进程里、两个执行层下模拟引擎故障（SIGSEGV 先到 libsigchain，ART 不认领，交给我们的处理函数）、内核层 panic 和宿主调用里的 panic，之后 GC、栈溢出、busybox 都正常。4K（API 34）和 16K（API 37）模拟器上都通过。

## 推迟项

- `tuxport-run` 的终端桥接线程（`terminal::bridge`）直接调用控制台的内核代码，没有兜住。它只在开发用的 `tuxport-run` 里，App 不经过它。
- `finish_group` 中途出错时，子进程交给 init、孤儿进程组的 SIGHUP/SIGCONT、IPC 的清理会丢掉。→ 已补上（M9 之前：进程结束的每一步分别兜住）：九步各自兜住，只丢掉出错的那一步做的事
- 结束进程的报告仍然只在 stderr（logcat）里（上一轮的推迟项，留给 M9 的外壳）。→ 也记进了内核日志（M9 之前：/dev/kmsg 和内核日志）；App 在标签页里说明原因，不在标签页里的发通知（[M9：进程被结束、服务没起来时提示](195-kill-reports.md)）
