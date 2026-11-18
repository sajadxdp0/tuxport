# M9 之前：不持 BKL 的系统调用兜住 panic，等待时不借着打开文件（2026-10-29）

[上一轮](86-rc-outside-bkl.md)的两个推迟项。

## 做法

- **不持 BKL 的系统调用兜住 panic**：
  - **原因**：`syscall::fast`（§6）在运行循环里直接调用，没有经过 `bug::catch`。那里出了内核 bug 时，panic 一直展开到 `thread_main` 的 `catch_unwind`，之后 abort，整个 App 终止，不符合 §12 第 5 条。
  - **改动**：
    - 用 `bug::catch` 兜住。出错时与持锁的路径一样，取 BKL 用 SIGSEGV 结束这个进程，报告 `kernel panic in syscall <编号> without the BKL`。
    - 展开经过的帧里只有原子变量、`Arc` 和 futex 队列的锁（中毒了照样用），没有 `Rc`。
  - **测试**：测试钩子新增第 9 种，让调用的线程下一次不持 BKL 的系统调用 panic。`tests/guest/bugs.c` 用 `clock_gettime` 触发，子进程被 SIGSEGV 结束。
- **持着打开文件的借用去等待**：
  - **为什么要查**：`RefCell` 的借用活过了放开 BKL 的那一刻，不会改坏计数；但别的线程这时对同一个对象 `borrow_mut` 会 panic（already borrowed），它所在的进程就被当作撞上内核 bug 结束。
  - **编译器替我们查掉的部分**：
    - 经由 `cx` 拿到的借用（`cx.mm()`、`cx.fds()`、`cx.t.group.borrow()`、`cx.kr()`）借着 `cx`，与 `cx.wait_*(&mut self)` 同时存在会编译不过。
    - 只收 `&Cx` 的函数放不开 BKL。
  - **要人查的部分**：从自己手里的 `Rc` 拿到的借用（`file.borrow()`、`pipe.borrow()`），以及经由 `cx.task` 拿到的借用（`cx.task.trace` 等；`cx.task` 是共享引用，经过它的借用编译器不拦）。
  - **做法**：
    - 先按函数名求出会放开 BKL 的函数，从等待、`Cx::unlocked` 出发按调用关系传递，一共约 180 个。同名的函数（`write`、`put`、`clone`……）让它多报，但不会漏。
    - 再找这些调用发生时还活着的借用，查了四种写法：`let` 绑定的守卫；`if let`、`match`、`while let`、`for` 头部的临时借用（edition 2024 的 `if let` 活到 then 块结束，`match` 活到整个 match 结束）；`let … else` 绑定指向借用内容的引用（临时值被延长到所在块结束）；把 `&x.borrow()` 直接传给会等待的函数。
    - 另外查了参数里直接收借用出来的对象（`&OpenFile`、`&Tty`……）、函数体里又会等待的函数，以及返回 `Ref` 的函数（只有 `Cx` 的四个）。
  - **找到两处**，都是别的线程这时改这个打开文件（`fcntl(F_SETFL)`、`ioctl(FIONBIO)`、`flock`）就会 panic：
    - **epoll_pwait**：`let FileKind::Epoll(ep) = &epf.borrow().kind else { … };` 把 epoll fd 的打开文件借到了函数结束，等待期间也一直借着。后面的 `let ep = ep.clone()` 只是遮蔽了名字，不会提前结束借用。[M4](06-m4.md) 起就有。
    - **seccomp 通知 fd 的 ioctl**：`if let FileKind::SeccompNotify(l) = &file.borrow().kind { return seccomp::ioctl(…) }` 在 `NOTIF_RECV` 等通知期间一直借着通知 fd 的打开文件。[seccomp](59-seccomp.md)那一轮起就有。
  - **改动**：两处都先在一句里取出要用的 `Rc`（epoll、过滤器，后者加了 `Listener::filter`），借用随这一句结束。epoll 的打开文件本身仍留到函数结束，等待期间 fd 被关掉也保活，与原来一样。
  - **测试**：差分测试 epoll 组、seccomp 组各新增一项：一个线程在 epoll_wait、`NOTIF_RECV` 里等，另一个线程对那个 fd 做 `fcntl(F_SETFL, O_NONBLOCK)`、`ioctl(FIONBIO)`，再叫醒它。

## 推迟项

- 不持 BKL 的 futex 等待如果在登记成等待者之后、取消登记之前 panic，队列里会留下一个已经没人等的条目，之后一次 FUTEX_WAKE 会算在它身上。只在撞上内核 bug 时出现。→ 已补上（M9 之前：futex 等待者的清理，放开 BKL 时检查借用），三条等待路径都由守卫移除；顺带修了持锁的路径和 `futex_waitv` 超时时可能丢掉一次唤醒
- 这次的排查是按函数名和缩进做的文本扫描。持着从 `Rc` 拿到的借用去等待，编译器拦不住，以后的改动可能再写出来。→ 已补上（M9 之前：futex 等待者的清理，放开 BKL 时检查借用）：调试构建里放开 BKL 时检查这个线程没有借着内核对象，`tests/guest/waits.c` 让各种等待都走一遍
