# M9 之前：测试用的发布构建也检查借用（2026-10-29）

[上一轮](88-futex-cleanup-borrow-check.md)的推迟项：放开 BKL 时借着内核对象的检查只在调试构建里有，差分测试、LTP、普查用的发布构建都不查。

## 做法

- **发布构建里也数借用**：
  - kernel crate 新增 `borrow-check` 特性，打开时与调试构建一样数着每个线程借着几个 `RefCell`，放开 BKL 时检查是 0（`core/kernel/src/cell.rs`）。
  - `tuxport-run` 默认打开它：差分测试、LTP、普查、手工测量用的都是 `tuxport-run` 的发布构建，不用记着另加参数。`--no-default-features` 可以去掉。
  - debug 版 App 也打开（与 `test-hooks` 一样由 `app/build.gradle.kts` 给 debug 变体加上），模拟器上的 instrumentation 测试也查。App 的发布版不打开，计数不存在。
  - 发布构建里撞上时与别的内核 bug 一样：panic 被兜住，结束这个进程（SIGSEGV），往 stderr 写 `tuxport: internal error in pid …`（§12 第 5 条）。
- **测试脚本认出内核层报告的内部错误**：发布构建只结束出问题的进程，测试未必因此失败（比如结束的是后台的辅助进程），所以三个脚本都去看 stderr 里有没有 `tuxport: internal error`：
  - `scripts/difftest` 原来把 `tuxport-run` 的 stderr 丢掉，现在写到 `<名字>.tuxport.err`，有内部错误时输出一致也算失败，失败时一起另存；
  - `scripts/ltp` 的结果多一种 `BUG`：日志里有内部错误，不论退出码；
  - `scripts/survey` 每个负载的那一行加上内部错误的条数。
- **`waits` 用例补上三种等待**（上一轮列出的 cargo test 没跑到的）：
  - **租约的打破**：父进程持着读租约，子进程以写方式打开时等它；父进程看到 `F_GETLEASE` 报告正在打破（子进程已经在等了）之后放弃租约。
  - **文件位置锁**（`f_pos_lock`）：两个线程同时从同一个打开文件各读 2 字节，检查读到的位置不重不漏。一个线程放开 BKL 读宿主文件时另一个等它；临时加日志数过，4000 次读里两层各等了 2000 多次。
  - **AIO 的完成**：管道上的 `IOCB_CMD_POLL` 由另一个线程写管道完成，`io_getevents` 不带超时一直等到它。
  - 上一轮列的另外两项，放开 BKL 时的 execve 和 DNS 存根，走的是 `Cx::unlocked`，进入时就检查，不必真的碰上并发：cargo test 里每次 execve 都查，DNS 存根由 CI 里需要外网的 apk 用例查。

## 结果

在噪声范围内。

## 测试

- 差分测试、LTP、普查（包括语言的测试集和大项目）都没有报告内部错误，结果与以前相同。
- debug 版 APK 的 `libtuxport.so` 里有这项检查；模拟器上的 instrumentation 测试这一轮没有跑。

## 推迟项

- cargo test、差分测试、LTP、普查都没跑到的等待仍然不查；发布构建只在这些路径真的等待时才查得出来（比如文件位置锁要碰上争用）。
