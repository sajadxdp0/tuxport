# M9 之前：futex 等待者的清理，放开 BKL 时检查借用（2026-10-29）

[上一轮](87-fast-panic-held-borrows.md)的两个推迟项。

## 做法

- **futex 等待者的清理**：
  - **问题**：futex 的三条等待路径（不持 BKL 的、持锁的、`futex_waitv`）都是先登记成等待者、等待、再移除。
    - 中途 panic 时，条目留在队列里，之后的一次 FUTEX_WAKE 会算在它身上，真正的等待者叫不醒。键是宿主地址时，那段内存以后还可能分给别的进程。
    - 查的时候另外发现：持锁的路径和 `futex_waitv` 超时、被信号打断时，是放开 futex 的锁之后才移除自己。这中间如果不持 BKL 的唤醒（`syscall::fast`）取走了它，唤醒算在了它身上，它却返回 ETIMEDOUT 或 ERESTARTSYS，这次唤醒就丢了。Linux 在这种情况下返回 0（`futex_unqueue`），`futex_waitv` 返回被唤醒的下标（`futex_unqueue_multiple`）。
  - **改动**：
    - 登记之后由守卫 `futex::Enqueued` 持有编号，等完了用 `leave` 离开：在同一次持锁里移除还在队列里的，报告哪个已经被唤醒。被唤醒与超时、被信号打断同时发生时算唤醒。
    - 中途 panic 时，守卫析构把还在队列里的移除。
    - 三条路径都用它。不持 BKL 的路径原来就是在一次持锁里看和移除，加锁的次数不变。
  - **测试**：测试钩子新增第 10 种：这个进程下一次在 futex 上等待、登记之后 panic。`tests/guest/bugs.c` 让一个子进程在共享内存的 futex 上撞上 bug、被结束，再检查 FUTEX_WAKE 叫醒的是接着等同一个 futex 的另一个子进程。
- **放开 BKL 时检查借用**：
  - **问题**：持着从 `Rc` 拿到的借用去等待，编译器拦不住，上一轮靠文本扫描查的。
  - **改动**：
    - kernel crate 改用自己的 `RefCell`（`core/kernel/src/cell.rs`）。它包着标准库的；调试构建里在线程局部变量里数着这个线程借着几个，放开 BKL 时（`block_for`、`Cx::unlocked`）检查是 0，不是就 panic。
    - 别的线程不用来抢，只要有一次等待时借着，cargo test（调试构建）就查得出来。
    - 发布构建里计数不存在：借用的守卫多一个没有析构的零大小字段，与标准库的 `RefCell` 一样。
    - 各文件只改了导入。线程自己的 `TaskState`（只有它自己借，整个系统调用期间借着）和三个线程局部变量仍用标准库的。
    - 现有的 cargo test 一次没有触发。
  - **覆盖**：只有 cargo test 跑到的等待才查。新增的 guest 程序 `tests/guest/waits.c` 在仓库里运行，让 19 种会在内核里等待的系统调用（epoll、poll、select、信号、消息队列、AIO、SysV IPC、futex、管道、eventfd、AF_UNIX、timerfd、inotify、PTY、文件锁、waitpid、seccomp 通知）各真正等一次：有超时的用很短的超时，别的由另一个线程稍后让它返回。

## 结果

在误差范围内。

## 推迟项

- 借用的检查只在调试构建里有，差分测试、LTP、普查用的发布构建都不查。cargo test 没跑到的等待也不查，比如：
  - `copy_file_range`、sendfile 等文件位置的锁；
  - 放开 BKL 时的 execve 和 DNS 存根；
  - 租约的打破；
  - AIO 的完成。

  → 已补上（M9 之前：测试用的发布构建也检查借用）：`tuxport-run` 和 debug 版 App 的发布构建也检查，差分测试、LTP、普查把内核层报告的内部错误当作失败；`waits` 用例补上文件位置锁、租约的打破和 AIO 的完成，放开 BKL 时的 execve 和 DNS 存根原来就在进入 `Cx::unlocked` 时检查。几处都没跑到的等待仍然不查
