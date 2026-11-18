# M9 之前：起进程和设定时器不再叫醒所有人（2026-10-27）

[按对象唤醒](49-targeted-wakeups.md)之后，核对剩下的"叫醒所有等待者"的地方，有两处常见：vfork 的子进程 execve 或退出时为了叫醒父进程，setitimer、timer_settime 为了让定时器线程重新计算，都请求了所有通道，读着管道的 shell、停在 poll 和 epoll 里的线程、等子进程的进程都被叫醒一遍。musl、glibc 的 `posix_spawn` 都用 `CLONE_VFORK`，gcc 的驱动程序起 cc1、as、ld，Python 的 subprocess，Java 的 ProcessBuilder，Go 的 os/exec 也都是 vfork 式的 clone，构建类的负载里每起一个进程就广播一次。量下来（`target/herd/spawn.c`）：8 个进程空等在 epoll 上，主进程 `posix_spawn` 300 次 `/bin/true`、调 2000 次 setitimer，空等的进程被叫醒 9400–10500 次，每次 spawn 从 174 微秒变成 195 微秒，每次 setitimer 从 2.1 微秒变成 11–15 微秒（原生 0.2 微秒，空等的进程不醒）。

## 做法

- **vfork**：子进程记着父进程的事件，execve 或退出离开地址空间时（`task::release_mm`）直接叫醒它（§6）。父进程只等直接的通知（`Cx::wait_alone`）：这个等待只有致命信号能打断，`signal::group_exit` 本来就直接叫醒被结束的线程。原来父进程自己也登记在所有通道上，被无关的修改叫醒。
- **定时器**：setitimer（alarm 也是它）、timer_settime 直接叫醒定时器线程（`Shared::timers`）。定时器线程平时只听"其余"通道，只有请求所有通道的修改会叫醒它。
- **signalfd 改掩码**：原来请求所有通道，现在只请求这个 signalfd 的对象，poll、epoll 等它时登记它（[按对象唤醒](49-targeted-wakeups.md)）。挂起的信号仍按原来的规则叫醒。

## 结果

上面的测量与上一轮的二进制交替各跑五次（8 个空等的进程）：空等的进程被叫醒 0 次（有一次是 11 次，上一轮每次 9500–10800 次），每次 spawn 148–154 微秒（有一次 227 微秒；上一轮 186–233 微秒），每次 setitimer 0.8–1.1 微秒（上一轮 13–16 微秒）。没有空等的进程时 setitimer 也从 2.0 微秒降到 0.9 微秒、spawn 从 171–180 微秒降到 144–152 微秒：原来的广播还叫醒宿主那边等进程结束的线程（`Machine::wait`），它也要去抢 BKL。标准基准（shell、git、gcc、python、apk、go build、Maven 和 Gradle 的热构建）都在噪声范围内，这些负载里没有空等着的进程。

## 测试

差分测试 `wakeups` 的空闲部分，别的进程的动作加上 vfork 式的起进程、设定时器和改 signalfd 的掩码；叫醒部分加上 5 项：vfork 的子进程 exec 或退出之后父进程继续，setitimer、timer_settime 改短之后信号按新的时刻到达，另一个线程改 signalfd 的掩码叫醒正在 poll 它的线程（信号要发给进程：Linux 的 signalfd 看的是 poll 它的线程自己挂起的信号，发给另一个线程的看不到）。空闲的子进程设了 `PR_SET_PDEATHSIG`，整个程序有闹钟：实现出错卡住时测试自己结束。变异检验里去掉直接叫醒定时器线程测不出来：有 guest 线程在运行时定时器线程每 20 毫秒检查一次内存上限（[稳健性](20-robustness.md)），顺带看到新的到期时刻。

## 推迟项

- 仍请求所有通道的：SIGCONT 让停住的进程继续、所有线程都屏蔽的信号、SysV IPC、POSIX 消息队列、AIO 完成、租约、关闭控制台。→ 租约改走锁的通道（M9 之前：锁和宿主 socket 只叫醒等它们的线程）；POSIX 消息队列只叫醒等这个队列的线程（M9 之前：消息队列和信号驱动的 I/O 不再叫醒所有人）；所有线程都屏蔽的信号只请求信号的通道（M9 之前：被屏蔽的信号只叫醒等它的线程）；其余仍请求所有通道
- signalfd 在 poll、epoll 时看的是创建它的线程挂起的信号，读时看的是读它的线程的；Linux 两者都看调用的线程（这一轮的差分测试里发现）。→ 已改为看调用的线程（M9 之前：signalfd 看调用的线程）
