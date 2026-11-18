# M9 之前：按对象唤醒（2026-10-27）

[futex 定向唤醒](16-futex-targeted-wake.md)起一直留着的推迟项：poll、epoll 的等待者被所有文件类的修改叫醒。系统里任何一次管道、socket、eventfd、PTY 的读写都把每个停在 poll、epoll 里的线程叫醒一遍，它们抢 BKL、重新检查、再睡下去；Go 的 netpoller、Node.js 的 libuv、Python 的 asyncio、JVM 的 NIO 线程平时都停在 epoll 里。量下来（`target/herd/herd.c`）：8 个进程各自空等在一个没人碰的 eventfd 上，另外两个进程用管道、socketpair 各来回 2 万次，空等的进程一共被叫醒 52 万次（每个来回 13 次），socketpair 的一个来回从 30 微秒变成 56 微秒，管道从 31–33 微秒变成 37 微秒（原生都是 24–25 微秒，空等的进程不醒）。这一轮让等待者只被它等的对象上的修改叫醒（§6）。

## 做法

- **等待者登记一组对象**：原来等待者最多登记一个对象（管道），现在是一组（`Cx::wait_objects`，排好序，唤醒时二分查找：一个 epoll 可能关注几千个文件）。poll、ppoll、select、epoll_wait 登记关注的每个进程内文件的唤醒对象（`syscall::poll::wake_objects`）：管道，PTY，eventfd，timerfd，inotify 实例，epoll 自己（`epoll_ctl`），AF_UNIX socket 自己的接收队列和连接的两个方向（`UnixSocket::wake_objects`）；嵌套的 epoll 逐层往里加。宿主 fd 仍由宿主 poll 等。阻塞的 read、write、recv、send、accept、connect 也只登记自己的对象（eventfd、timerfd、inotify、PTY、AF_UNIX 的 channel），宿主控制台读输出、写输入（App 的终端、`tuxport-run` 的桥接线程）只登记它的 PTY。
- **修改只请求自己的对象**：eventfd 的读写；PTY 的读写、改变状态的 ioctl、一端关闭，宿主控制台取走输出、送进输入；AF_UNIX 的收发、shutdown、关闭、connect 和 accept（监听 socket 的连接队列记在它自己的接收队列上，等 backlog 空位的 connect 也等它）；inotify 排进事件（每个实例自己请求）；`timerfd_settime`；`epoll_ctl`。仍请求整个通道的：关闭宿主 socket（等它的线程在宿主 poll 里，fd 关掉要重新算）、POSIX 消息队列和加过锁的文件，flock、fcntl 锁，netlink 和 DNS 存根的回答，`F_SETPIPE_SZ`，以及请求所有通道的信号（所有线程都屏蔽时）、SysV IPC、AIO 完成。这些修改照样叫醒所有等待者，所以只要每种文件登记全了改变它就绪状态的对象，就不会漏叫醒。
- **只在需要时听子进程**：poll、epoll 原来登记所有通道，每个进程结束都把它们叫醒一遍；只有 pidfd 关心子进程的状态，现在只在关注 pidfd 时才听子进程的通道。阻塞的读写只听文件类的通道。
- **关闭文件不再广播**：原来关闭 PTY、epoll、inotify、signalfd、pidfd、AF_UNIX socket 都请求文件类的唤醒，现在只有宿主 socket、POSIX 消息队列和加过 flock、OFD 锁、租约的文件还这样，PTY、AF_UNIX socket 的另一方由 `PtyEnd`、`UnixSocket` 按对象叫醒。fd 表析构（进程退出）原来总是广播，让等记录锁的线程重新检查，现在只在加过 POSIX 记录锁时才广播（`FdTable::holds_locks`，fork 出来的表不继承）：原来每个进程退出都把所有读着管道、停在 poll 里的线程叫醒一遍。
- **`timerfd_settime` 不叫醒等它的线程**（bug）：另一个线程或者进程正带着原来的到期时刻（或者没有到期时刻）在 epoll_wait、read 里等这个 timerfd 时，新设的时刻不生效，要等别的修改顺带叫醒它。原来那个广播多，几乎总会被顺带叫醒，没被发现；改成按对象之后就必须请求。现在请求 timerfd 的对象。
- **AIO 的 resfd**：请求完成时给 eventfd 的计数加一，原来没有算作一次到达，`EPOLLET` 的关注不再触发（[epoll 的边沿](42-epoll-edges.md)漏掉的一处），现在算。

## 结果

上面的测量，空等的进程被叫醒 0 次，socketpair 的来回 31–32 微秒、管道 32–33 微秒，与没有空等的进程时（30–33 微秒）相同。标准基准（shell、git、gcc、python、apk、go build、Maven 和 Gradle 的热构建）都在噪声范围内：这些负载里没有空等着的 poll、epoll 进程。

## 测试

新的差分测试组 `wakeups`。一半是空闲的等待者：8 个进程在 epoll、poll 和阻塞的读里等自己的对象，其间别的进程读写各种文件、起结束进程、epoll_ctl、timerfd_settime、建删被监视的文件，空闲的进程的自愿上下文切换次数都不变（原生如此）。另一半是 20 种真正的事件叫醒另一个进程里等着的线程，2 秒之内醒来才算。单元测试新增 epoll 等待时登记的对象。

- 全量运行之后补上：边沿触发的关注报告过挂断之后，等待时不再等它的宿主 fd，但仍登记它的对象（PTY 重新打开从设备之后还会来事件）。

## 推迟项

- 所有线程都屏蔽的信号（signalfd、sigtimedwait 等它）、SysV IPC、POSIX 消息队列、AIO 完成、租约仍叫醒所有等待者；关闭宿主 socket 仍叫醒所有文件类的等待者。→ 租约改走锁的通道，关闭宿主 socket 只叫醒等这个 socket 的线程（M9 之前：锁和宿主 socket 只叫醒等它们的线程）；POSIX 消息队列只叫醒等这个队列的线程（M9 之前：消息队列和信号驱动的 I/O 不再叫醒所有人）；所有线程都屏蔽的信号只叫醒 sigtimedwait、signalfd 的等待者（M9 之前：被屏蔽的信号只叫醒等它的线程）；其余仍叫醒所有等待者
- 有独占门、`O_ASYNC` 的文件时，定时器线程仍被每一次修改叫醒（[独占的唤醒](48-exclusive-wakeup.md)的推迟项）。
