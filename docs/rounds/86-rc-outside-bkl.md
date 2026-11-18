# M9 之前：排查在 BKL 外改 Rc 的计数（2026-10-28）

[线程退出时在 BKL 外丢掉弱引用](71-thread-exit-race.md)那一轮的推迟项。那一次，线程退出时在 BKL 外丢掉一个弱引用，与同时持有 BKL 的线程一起改同一个计数，计数改丢之后对象被提前释放，宿主的堆被破坏；`catch_unwind` 兜不住，整个 App 崩溃。那一轮只查了线程局部变量。这一轮把所有不持 BKL 运行内核代码的地方逐个查了一遍。`RefCell` 的借用标志和 `Rc` 的计数一样不是原子的，所以一起查。

## 查的范围

别的线程只能经由 `unsafe impl Send/Sync` 的类型接触到 `Rc`，即 `Kernel` 和元数据缓存 `Meta`；`HostRegion`、`FileMap` 里没有 `Rc`。静态变量必须是 `Sync`，线程局部变量每个线程一份，这两类编译器都替我们查了。所以要查的是：手里拿着从 `Kernel` 取出来的 `Rc` 的线程，在不持 BKL 时做了什么。逐个看了：

- **guest 线程的运行循环**：执行 guest 代码、不持 BKL 的系统调用（`syscall::fast`，§6），以及 exec 和退出时丢掉翻译。这些代码只碰原子变量、`Arc`、`Mutex`、引擎的页表和元数据缓存（持读锁）。`Fast` 和翻译（`CodeSpace`）里也只有 `Arc`。线程自己的 `TaskState` 只有它自己借：不持 BKL 的 nanosleep 写 `fast_sleep`，别的线程不借别人的。
- **系统调用中途放开 BKL 的两处**：一是等待（`Cx::wait_on` 等，经由 `block_for`）；二是 `Cx::unlocked`，用在 execve 建地址空间、DNS 存根、普通文件和宿主 fd 的读写、`copy_file_range`。正常执行时这两处都干净：
  - 放锁期间只做宿主调用。
  - execve 闭包里的 `Mm`、`auxv` 以及映射用的 `Rc<OwnedFd>` 都是新建的，别的线程拿不到。
  - 共用的只读文件映射是 `Arc<Mutex>`。
  - 闭包出错时新建的 `Mm` 在锁外析构，它只动自己的东西。
- **别的线程和路径**：
  - 线程的开始和退出（71 轮修过）、定时器线程、宿主调用 `Machine` 的方法、`tuxport-run` 的终端桥接线程、删 object 的后台线程、BKL 放开之后做的事（叫醒、交还宿主内存）。
  - 这些地方要么在持锁时丢 `Rc`，要么只碰 `Arc` 和普通数据。
  - 每个取锁的地方，锁之前声明的局部变量里都没有 `Rc`；有的话，它会在锁放开之后才被丢掉。
- **`task::current`**：它 upgrade 线程局部变量里的弱引用，只在持有 BKL 的 poll 路径上调用。

## 找到的问题：放锁期间 panic

- **原因**：等待和 `Cx::unlocked` 先从 `Cx` 里拿走 BKL 再做事。闭包里或者等待中 panic 时（内核层的 bug，§12 第 5 条），锁不在 `Cx` 里，展开经过的外层各帧丢掉的 `Rc` 都不持 BKL。
- **例子**：execve 在锁外建地址空间时，`load_prepared` 里 panic。execve 那一帧的 `vfs: Rc<Vfs>` 是所有进程共用的，每个碰文件的系统调用都 clone 它；这时它的减一可能和别的线程持锁做的加一撞在一起。
- **后果**：`bug::catch` 兜住 panic 之后，别的进程照常运行；计数改丢了的话，`Vfs` 可能被提前释放，宿主的堆被破坏。

## 做法

- **`Relock`**：放开 BKL 之后由它借着 `Cx`，析构时还没有拿回锁就拿回来。正常返回和 panic 展开都经过它，等待和 `unlocked` 都用它。热路径上只多一次判断。
- **`Cx::unlocked` 的闭包要求 `Send`**：`Rc`、`RefCell` 和它们的引用都不是 `Send`，捕获它们的闭包编译不过。现有的 6 处调用，加上 `unlocked_io` 的两个调用方，不用改就通过了。
- **`task::current` 加了 debug 断言**：调用时必须持有 BKL。

## 测试

- 测试钩子新增第 8 种：在 `Cx::unlocked` 的闭包里 panic；外层放一个守卫，展开到它时检查是否持有 BKL，没有就终止宿主进程。
- `tests/guest/bugs.c` 加了这一种：子进程被 SIGSEGV 结束，`tuxport-run` 照常运行。

## 推迟项

- 不持 BKL 的系统调用（`syscall::fast`）没有兜住 panic：那里出了内核 bug，整个 App 会终止（`thread_main` 的 `catch_unwind` 之后 abort），而不是只结束那个进程。展开经过的帧里没有 `Rc`，要兜的话，在它外面加 `bug::catch`、再持锁结束进程就行。→ 已补上（M9 之前：不持 BKL 的系统调用兜住 panic，等待时不借着打开文件）
- 只查了在 BKL 外改 `Rc` 的计数和 `RefCell` 的借用标志。另一种情况没有逐个排查：持着某个 `RefCell` 的借用去等待（借用活过了放锁的那一刻）。它不会改坏计数，但别的线程这时借它会 panic（already borrowed）。→ 已排查（M9 之前：不持 BKL 的系统调用兜住 panic，等待时不借着打开文件）：找到 epoll_pwait 和 seccomp 通知 fd 的 ioctl 两处，已修
