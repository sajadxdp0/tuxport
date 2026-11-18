# M9："文件"App 与 guest 双向的通知（2026-11-03）

[在"文件"App 里改家目录](197-documents-write.md)之后留下两个方向的缺口：

- "文件"App（以及经由它打开文件的编辑器）改的东西不产生 inotify 事件。在 Android 的编辑器里改代码、guest 里跑着 `vite`、`cargo watch` 这类热重载，就收不到改动。Termux 里这条路是通的：它的文件是宿主上真的文件，宿主的 inotify 照样起作用。tuxport 的 inotify 是内核层模拟的，provider 另外打开仓库改的，内核不知道。
- guest 改了文件，"文件"App 不会自动刷新（[在"文件"App 里浏览](196-documents-read.md)的推迟项）。

## 做法

### "文件"App → guest

provider 的修改都经由 `tuxport_kernel::browse_changing`：与原来的 `browse` 一样另外打开一次仓库，`vfs::browse` 的各个函数把改了什么记成 `Change`，仓库关闭（提交）之后交给用着同一个仓库、正在运行的内核（`machine::store_changed`）。内核启动时按规范化的仓库目录登记自己（弱引用，停了就失效），收到时持着 BKL 照对应的 syscall 报事件（`inotify::host_change`）：

| 改动 | 报的事件 |
|---|---|
| 新建文件、目录 | 目录上 `IN_CREATE`（目录带 `IN_ISDIR`） |
| 删除（整个目录树先里后外，一项一个） | 同 unlinkat、rmdir：文件自己 `IN_ATTRIB`，最后一个链接时 `IN_DELETE_SELF`；目录上 `IN_DELETE` |
| 改名、移动（不覆盖） | 同 renameat2：`IN_MOVED_FROM`、`IN_MOVED_TO`（同一个 cookie），`IN_MOVE_SELF`；经由旧名字打开着的文件跟着改名 |
| 为写打开 | 截断时 `IN_MODIFY`，然后 `IN_OPEN` |
| 写完关闭 | `IN_MODIFY`、`IN_CLOSE_WRITE` |

最后一行要知道对方什么时候关：provider 交出去的 fd 由别的 App 直接写宿主的 object，内核看不到每一次写。`ParcelFileDescriptor.wrap`（能给已有的 fd 加关闭的回调）要 API 30，App 的 minSdk 是 29，所以照旧由本地代码检查、截断，再按 `/proc/self/fd` 找到 object 的路径，用 API 19 起就有的 `ParcelFileDescriptor.open(file, mode, handler, onClose)` 重新打开交出去（截断已经做过，不再带 `MODE_TRUNCATE`），关闭时在 `documents-close` 线程上调 `Native.browseWritten`。重新打开失败时退回原来的 fd，只是没有关闭的事件。

### guest → "文件"App

仓库在 `dir_changed`（新建、删除、改名时改目录时间的地方）里记下改过的目录，App 第一次来取时才开始记，两次之间最多 256 个。服务原来每 2 秒取一次监听的端口、被结束的进程，现在也取走这些目录（`Machine::take_changed_dirs`，按各级目录记着的上级和名字拼出仓库里的路径，删掉了的、tmpfs 里的不给），对每个目录 `notifyChange` 它的子项 URI（文档 ID 就是路径），"文件"App 正在看那个目录的话就重新列。

### 顺带修的：别的内核放锁时取走唤醒请求

这一轮的全量 `cargo test` 卡住了 20 分钟：`one_machine_runs_several_terminal_sessions` 的进程里 guest 都已经退出，测试线程还在等控制台。单独跑 60 次都不卡，整个 `rootfs_programs` 并发地跑，约 15% 卡在它；上一轮的代码 25 次一次也不卡。

原因原来就有：持着 BKL 时请求的唤醒（`request_wake` 这些，通道和对象）放在整个宿主进程共用的全局变量里，放 BKL 时取出来、对这个内核的等待者逐个比。可等待者是每个内核各一份，同一个进程里有几个内核同时跑时（测试就是这样，App 换内核时也可能），内核 B 持锁时请求的唤醒，可能被内核 A 放锁时取走、清零，B 的等待者就一直睡着。这一轮的 `store_changed` 让测试线程去拿别的测试的内核的 BKL，跨内核的放锁多了，才露出来。

现在唤醒请求按线程记：请求唤醒的总是持着 BKL 的线程，放锁的也是它，放锁时只处理自己攒下的。万一有不持 BKL 时的请求（不该有），照原来的做法放进全局，由下一个放锁的线程处理。

内核层新增 `wakeups_stay_with_their_kernel`：A 持锁时请求唤醒，另一个线程拿 B 的锁再放开，A 的等待者这时还没被叫醒，A 放锁之后被叫醒。改回全局的做法时它失败。

## 测试

- 内核层（`cargo test`）：`changes_stay_inside_the_top_directory` 加上记下的改动。新增 `files_app_changes_reach_inotify_watchers`（`hosts/cli/tests/rootfs_programs.rs`）：宿主经由 `browse_changing` 新建、截断打开、关闭、改名、删除，guest 里 busybox 的 `inotifyd` 依次收到对应的事件；guest 新建之后 `take_changed_dirs` 给出改过的目录。
- 仪器测试新增 `documentsChangesReachTheKernelAndBack`：经由 `DocumentsContract` 和 `ContentResolver` 的修改（包括关闭回调来的 `w`）guest 的 `inotifyd` 都收到，guest 改了之后注册在子项 URI 上的观察者收到通知；在 Pixel 4a（API 34）的模拟器上通过。测试本身的坑：`inotifyd` 要在自己的控制台上跑，`sh` 辅助函数用完会关掉控制台、结束整个会话，`nohup` 也挡不住。
- 修好唤醒请求之后，整个 `rootfs_programs` 并发地跑 30 次都不再卡。

## 推迟项

- 对方一直开着 fd 写（不关）时，guest 只在关闭时收到一次 `IN_MODIFY`；对方为写打开、什么也没写就关，也照样报 `IN_MODIFY`（没有比较内容）。
- 只改了文件内容（大小、时间变了）的目录不刷新"文件"App：仓库只在目录项变化时记。
- 两次取之间超过 256 个目录的不再记，那些目录要重新进才刷新。
