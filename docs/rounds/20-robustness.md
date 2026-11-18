# M9 之前：稳健性（2026-10-25）

开始 M9 之前，把设计文档 §12 里"一个 guest 程序就能弄垮整个 App"的几种情况补上：内核层 panic、引擎让宿主收到故障信号、guest 用掉太多内存、fork 炸弹。原来前两种直接终止宿主进程，后两种等 Android 的 LMK 杀掉整个 App（或者把宿主的线程和 fd 用完），所有会话一起丢掉。现在都只结束出问题的那个 guest 进程，别的会话照常运行。

## 做法

- **内核层 panic**（§12 第 5 条，`core/kernel/src/bug.rs`）：运行循环在每次进入内核（syscall、同步异常、kick）的地方 `catch_unwind`。panic hook 先打印消息和位置，再写一行报告（pid、comm、线程、最后的 guest pc、哪个 syscall），用 SIGSEGV 结束整个线程组，相当于 Linux 的 oops 结束当前任务；之后获取 BKL 时忽略中毒标记。BKL 保护的状态可能只改了一半，但出问题的进程已经结束，大部分不一致随之消失。线程开始和退出时的 panic 仍然终止宿主进程。
- **引擎的宿主故障**（§7.4，`core/engine/src/c/guard.c`）：引擎装 SIGSEGV、SIGBUS、SIGILL 的处理函数，原来的处理方式记下来。`tuxport_cpu_run()`、`tuxport_dbt_run()` 改为经由 `tuxport_guard_run()` 运行：先 `sigsetjmp`，运行期间出了故障，处理函数记下信号、`si_code`、宿主 pc 和地址，`siglongjmp` 回去，返回新的退出原因 `TUXPORT_EXIT_HOST_FAULT`。
  - 跳回之后恢复宿主的 FPCR（翻译后的代码里装的是 guest 的），释放运行期间登记的锁（原生桩缓存的读写锁、翻译模板的互斥锁），解除对这个信号的屏蔽（`sigsetjmp` 不保存屏蔽字，省掉每次一个 syscall）。运行期间引擎不调用 malloc、不回调内核层，原来就是这样。
  - 出过故障的 Tier 1 实例的几张表可能只改了一半，不回到池里，直接释放；翻译模板整体丢弃，坏掉的可能是模板本身。
  - 内核层收到这个退出原因后，像 panic 一样报告并用 SIGSEGV 结束进程。
  - 不在保险期间的故障（内核层自己的、ART 没认领的别的线程的）先写一行报告，再交给原来的处理函数：Android 上 libsigchain 记着的是 debuggerd 的，照样生成 tombstone；Linux 开发宿主上是 Rust 标准库检查栈溢出的处理函数。别人用 kill 发来的（`si_code` ≤ 0）不当作引擎的故障。
- **debug 构建**按设计文档的约定仍然直接终止宿主进程（不装处理函数，panic 时 abort），方便就地调试；`TUXPORT_ON_BUG=kill` 改成 release 的做法。端到端测试用的钩子：debug 构建设置 `TUXPORT_TEST_HOOKS=1` 时，syscall `0xa991` 让内核层 panic，或者让一页在页表里指向 `PROT_NONE` 的宿主内存，guest 下次访问它时引擎在宿主上出错。
- **App 内存的上限**（§12 第 4 条，`core/kernel/src/oom.rs`）：
  - 量的是整个 App 进程的匿名内存：`/proc/self/status` 的 RssAnon + RssShmem + VmSwap。LMK 看的就是它；guest 的私有、共享内存之外，内核层为 guest 付出的翻译缓存、元数据缓存也在里面，仓库文件和程序的文件页可以回收，不算。默认上限是物理内存的一半，`MachineOptions::memory_limit` 和 `tuxport-run --memory-limit` 可以改。
  - guest 碰新的页由宿主内核直接处理，内核层看不到缺页，只能定时检查。有 guest 线程不在内核里等待时，定时器线程每 20 毫秒量一次，超过上限的 3/4 之后每 5 毫秒（这台虚拟机上读一次只要 1.6 微秒）；所有线程都在等待时内存不会再涨，检查停下来，某个线程要回到 guest 代码时经由一个原子标记叫醒它。最早用的 100 毫秒间隔下，不停分配内存的程序（Tier 1 下每秒碰好几 GB 的新页）在 512 MB 的上限上多用了 200–300 MB；改成 20/5 毫秒之后是 10–35 MB。
  - 超出时按 Linux 的 oom_badness 挑进程：驻留的页数，加上 `oom_score_adj` 按物理内存折算的页数，`oom_score_adj` 原来只记录，现在生效。第一版按上限折算，awk 的驻留内存超过整个上限时盖过了 +1000，改成与 Linux 一样按全部内存折算。挑中的用 SIGKILL 结束，报告写到 stderr；等它退完（最多 1 秒，卡在宿主 I/O 里的不再等也不再挑）再看要不要结束下一个。第一版等的是"任何正在退出的进程"，碰上一个无关的卡住的进程会白等，改成与 Linux 一样只等自己挑中的；挑中的进程已经在退出时不再结束它，与 Linux 的 `task_will_free_mem` 一样。
- **线程数的上限**（§6）：`/proc/sys/kernel/threads-max` 原来只显示 pid_max，现在生效，可以写（20 到 `FUTEX_TID_MASK`），对 root 也有效。默认按物理内存每 MB 一个（Linux 是每 128 KB 一个：tuxport 的每个线程是一个宿主线程，还占一个 eventfd），并且不超过宿主 fd 上限的一半。`RLIMIT_NPROC` 原来不限，现在默认是 threads-max 的一半，与 Linux 一样按真实用户 ID 数线程，root 和有 `CAP_SYS_RESOURCE`、`CAP_SYS_ADMIN` 的不受它限制。

## 结果

与上一轮的二进制交替运行、各取最短：100 万次 `getppid` 从 59 毫秒变成 65 毫秒（每次进入引擎多一次 `sigsetjmp` 和一次 TLS 查找，约 6 纳秒；实际的 syscall 在 tuxport 里要几百纳秒到几微秒）；shell 起 300 个进程在噪声之内。

## 测试

- 引擎新增 `tests/guard.rs`：两个执行层出了宿主故障之后返回 `HostFault`、照常运行，原生桩的读锁被释放。这个测试也用 cargo-ndk 编成 Android 程序，在 4K（API 34）和 16K（API 37）的模拟器上直接运行，验证了 bionic 下的 `sigsetjmp`/`siglongjmp` 和 `ucontext`。
- `tests/guest/bugs.c` 在两个执行层下用测试钩子触发内核层 panic 和引擎故障，子进程被 SIGSEGV 结束，父进程照常运行；`tests/guest/limits.c` 检查 `RLIMIT_NPROC` 和内存上限。
- rootfs 用例新增 threads-max 和 `oom_score_adj` 两个（后者只在 Tier 1 下跑，Tier 0 的 debug 构建拼 128 MB 的字符串要十几秒）。
- 两个模拟器上的 instrumentation 测试都通过：App 进程（release 构建，装着兜底）里 ART 照常工作。
- 差分测试里的 Go、Java、Node.js 在默认的内存上限下照常运行。

## 推迟项

- 引擎故障在 App 进程里的恢复（要经过 ART 的 libsigchain）没有直接测过：Android 上验证了 bionic 下的恢复（模拟器上直接运行引擎测试），以及 App 装上处理函数之后 ART 照常工作；libsigchain 把 ART 不认领的故障转给我们的处理函数，`siglongjmp` 离开它时它没有要恢复的状态（按它的源码）。→ 已补上：App 进程里的 instrumentation 测试（M9 之前：稳健性的收尾）
- 被结束的进程只在宿主 stderr（Android 上是 logcat）里留下报告，终端里只看到 shell 打印的 "Segmentation fault" 或 "Killed"。App 可以在标签页里提示，留给 M9 的外壳。→ 报告和 OOM killer 结束进程也记进了内核日志，guest 里 `dmesg` 看得到（M9 之前：/dev/kmsg 和内核日志）；App 在标签页里说明原因，不在标签页里的发通知（[M9：进程被结束、服务没起来时提示](195-kill-reports.md)）
- 内存上限只看 App 自己：系统整体内存紧张时（`onTrimMemory`）还没有接到 OOM killer 上。→ 界面离开 1 分钟之后、收到 `onTrimMemory` 时释放可以重建的缓存（M9 之前：onTrimMemory），没有接到 OOM killer；有前台服务时 Android 14 起收不到 `onTrimMemory`
- `oom_score_adj` 的写入不检查权限：Linux 不让没有 `CAP_SYS_RESOURCE` 的进程把它调到 `oom_score_adj_min` 以下。→ 已补上（2026-10-26，单独修补）：没有这个能力时调到下限以下返回 EACCES，有它时写的值成为新的下限，fork 继承
- 线程开始、退出时的 panic 仍然终止宿主进程；定时器线程、`Machine` 方法（JNI 调用）里的 panic 还没有处理。→ 已补上（M9 之前：稳健性的收尾）
