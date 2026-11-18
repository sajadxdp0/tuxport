# M9：内核的仓库只在持着它的 BKL 时用（2026-11-03）

[以指定用户登录](193-login-account.md)那一轮，`init_runs_the_inittab` 在全量并发的 `cargo test` 里失败过一次：`poweroff` 让所有进程同时退出时，guest 线程在 rusqlite 的语句缓存里 panic（`cache.rs:142`，`RefCell` 已经被借走）。[进程被结束的提示](195-kill-reports.md)那一轮找到一处可疑的地方：宿主线程开机、关机时不持 BKL 读 `/etc/inittab`，改成了持锁，但没能复现，不能确认就是它，也不知道还有没有别处。

要紧的不是 panic。rusqlite 打开连接时带 `SQLITE_OPEN_NO_MUTEX`（`Connection` 不是 Sync，由类型系统保证同一时刻只有一个线程用），SQLite 自己不给这个连接加锁；两个线程同时在里面，连接的状态、预编译的语句、进行中的事务都可能被弄乱，后果可能是仓库损坏。`Store` 也不是 Sync，内核的仓库能被几个线程拿到，靠的是 `Kernel` 手写的 `unsafe impl Send` 和"只在持着 BKL 时碰"的约定：宿主线程忘了拿锁，编译照样通过，只有撞上并发时才出事，测试里很难碰到。

## 是两个线程，不是重入

`cache.rs:142` 是 `StatementCache::get` 里的 `borrow_mut`，只在取出或者预编译语句时借着（`sqlite3_prepare`），这期间不会回调到 Rust 里再用缓存（仓库注册的 WAL 钩子只在提交时调用，而且只是通知后台线程）。同一个线程不可能在这里重入，所以那次确实是两个线程同时用了一个连接。

## 那次的 core

那次留下的 core（`target/u/core-init_runs_the_inittab`）是 debug 版 `tuxport-run` 的，生成时工作区的内核层与提交 c18796e 相同：在同样的 `/work` 路径下重编那个提交，build-id 与 core 里记的一致，gdb 看得到所有线程当时在哪：

| 线程 | 在做什么 | 碰不碰仓库 |
|---|---|---|
| guest 线程 tid 5 | 系统调用里 panic，已经展开到 `bug::catch`，在 abort | 持着 BKL 用（panic 的就是它） |
| guest 线程 tid 8 | 线程退出（`task::exit`） | 持着 BKL |
| guest 线程 tid 6 | shell 在 `wait4` 里等子进程（放开了 BKL） | 不碰 |
| 定时器线程 | 在 `block` 里等（放开了 BKL） | 不碰 |
| 删除 object 的后台线程 | 等活干 | 只用自己另外打开的连接 |
| 主线程 | `Machine::shutdown` 按 inittab 运行关机的命令，在 `run_init_process` 开头等 BKL | 当时的 `inittab()`、`run_init_process()` 取出 `vfs` 之后放开 BKL 解析路径 |

主线程正在关机命令的循环里：上一条命令刚做完一次不持锁的路径解析，正要为下一条再做一次。别的线程都不会不持锁碰仓库，与 guest 线程同时用连接的只能是它，也就是第 195 轮改掉的那两处。

## 做法

在检查的构建（调试构建，以及打开 `borrow-check` 特性的发布构建：`tuxport-run`、debug 版 App，与[放锁时不能借着内核对象](88-futex-cleanup-borrow-check.md)的检查相同，见[测试用的发布构建也检查借用](89-release-borrow-check.md)）里，把"只在持着 BKL 时用"变成检查：

- `Store` 的连接字段改名为 `conn`，原来直接用它的 47 处都改为经由 `Store::db()`；元数据缓存的读、改（`cached`、`update`）也经过同一个检查。仓库属于运行中的内核、当前线程却没有持着那个内核的 BKL 时，当作内核层的 bug panic（"the inode store of a running kernel is used without holding its BKL"）：结束当前进程并报告，调试构建里直接终止，带着调用栈。不用撞上并发，第一次走到这条路就查出来。
- `Machine::start` 把仓库交给内核时登记（`Store::guard_with_bkl`）。登记之前（打开仓库、开机时清空 `/dev/shm`）和另外打开的仓库（"文件"App 的 `browse`、导入 rootfs、`sync_store`）只有一个线程用，不检查。丢掉 `Store` 时也不检查：最后一个 `Arc<Shared>` 已经没了，别的线程拿不到它。
- 要知道持的是哪一个内核的 BKL：同一个宿主进程里可以有几个内核（测试；App 换内核时；[上一轮](201-documents-notify.md)的 `store_changed` 去拿别的内核的锁），持着 A 的锁用 B 的仓库一样是错的。原来线程局部的 `HELD` 只记"持着 BKL"，现在记持着的是哪一把（`Shared::id`，取它的 `held` 字段的地址），`bkl_held()` 的意思不变。
- 后台线程不经过 `Store`：检查点用自己另外打开的连接，删除 object、不持 BKL 读缓存（`Meta::reader`）用的是 `Meta`，不受影响。
- App 的发布版里这些都不存在，没有开销。

## 测试

- 检查打开之后，全量 `cargo test`（包括 `rootfs_programs` 里开机、关机、终端、"文件"App 的那些）、两层的差分测试、LTP、仪器测试（Pixel 4a、API 34 的模拟器，debug 版带着检查）都没有撞上它。加上那次的 core，第 195 轮改掉的两处就是全部。
- 把 `Machine::inittab` 改回先取出 `vfs`、放开 BKL 再读（第 195 轮之前的样子），`init_runs_the_inittab` 第一次就失败，栈指向 `Machine::inittab`；原来要在全量并发时碰运气。
- 新增 `a_running_kernels_store_needs_its_bkl`（`vfs/store.rs`）：没登记时谁都能用；登记给内核 A 之后，不持锁、持着内核 B 的锁时用都 panic，持着 A 的锁时正常；丢掉时不检查。

## 推迟项

- 只检查仓库的连接和元数据缓存。别的由 BKL 保护的内核对象（`Rc<RefCell<…>>`）被不持锁的线程碰到时，仍然只有撞上并发才看得出来。要推广得给每个对象记下属于哪个内核：内核对象在内核运行之前、在单元测试里都不持 BKL 地建立和使用，不能简单地让每次借用都要求持锁。
- 只查得到测试走到的路径。App 里测试没走到的宿主调用，用 debug 版手动操作时同样会查。
