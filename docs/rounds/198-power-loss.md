# M9：断电之后仓库不坏（2026-11-03）

M9 的 App 工作做完之后筛推迟项，排在第一的是[设置页](190-settings-default-alpine.md)那一轮遇到的怪事：4K 模拟器上留着的旧仓库里，`/bin/sh` 读出来不是 ELF，用到 shell 的用例全失败，数据跟着卸载没了，没查。Android 结束 App 是常态，仓库要是因此坏掉，整个环境就用不了了。

## 原因

设计上只考虑了 App 被杀：提交推迟最多 5 毫秒，被杀时丢掉最后几毫秒的修改，仓库仍是某次提交时的完整状态。这时宿主的页缓存都在，写进 object 的数据不会丢。没考虑的是**断电**：宿主崩溃、手机没电、长按电源键强制重启、模拟器被强行关掉，页缓存里没写回的都丢了，盘上只剩各个文件各自写回了的部分。这时有四处顺序不对：

1. **改名覆盖**。apk（Alpine 3.24 的 apk-tools）装文件时先写 `.apk.<哈希>`，再改名覆盖原来的文件，整个过程一次 fsync 也不调（在 tuxport 里用 strace 数过：装 doas 只有 6 次 renameat）。原生的 ext4 在改名覆盖已有文件时替程序把新文件写回（auto_da_alloc），日志提交在数据之后，断电之后名字下面要么是旧的、要么是完整的新文件。tuxport 的改名只在数据库里，宿主只看到新建了一个 object，没有这层保护；数据库的提交又随时可能落盘（检查点会 fsync，WAL 的脏页宿主随时会写回），于是名字指向一个数据还没落盘的 object，断电之后就是空的。
2. **删除旧 object**。被覆盖、被删除的文件的 object 在提交之后由后台线程删掉。可是提交只写进 WAL、不刷（`synchronous = NORMAL`），而宿主的 unlink 几秒之内就随 ext4 的日志落盘了：断电之后数据库回到提交之前的状态，名字还指着旧的 object，object 却没了。
3. **guest 的 fsync 不刷盘**。原来的 fsync 只提交推迟的写事务（注释说"为了 apk 这类频繁 fsync 的负载"，可现在的 apk 根本不调 fsync），不让宿主刷 object，也不等 WAL 落盘。sync、syncfs 也一样；`O_SYNC`、`msync(MS_SYNC)`、sync_file_range 都什么也不做。
4. **导入 rootfs**。导入、写好配置之后直接改名就位，没刷盘：导入完几十秒之内断电的话，仓库在了，里面的 object 却可能是空的。

`/bin/sh` 那次正好是第 1 种的样子，但当时的数据没了，没法确认就是它。

### 复现

`tests/crash/crash.sh` 在 colima 虚拟机里用 dm-flakey 模拟断电（xfstests 的做法）：设备上建 ext4，放一个刚导入的 Alpine 仓库，在 tuxport 里跑 `workload.sh`：

- 每一轮把 f1–f20 用"写临时文件、改名覆盖"换成新的一版（每个 2.5–100 KB，内容能看出是第几版、完整不完整）；
- 每 5 轮新建一个 `s<v>`，`fsync` 它和目录之后打印 "synced v"，再停 0.3 秒（让后台线程做完检查点、WAL 从头再用，见下面"测试"）。

3–15 秒之间的随机时刻，先截下负载已经打印的内容，再把设备切成丢掉所有的写，结束负载、卸下、恢复设备、重新挂上，在 tuxport 里开机（清理照常做）跑 `check.sh`：f1–f20 都要在、是某一版的完整内容；截下来的每个 "synced v" 的 `s<v>` 都要完整。同一个负载在 alpine 容器里原生跑一遍当对照，确认原生的 ext4 满足这套标准。

| | 次数 | 失败 |
|---|---|---|
| 原生 | 10 | 0 |
| tuxport，这一轮之前 | 8 | 8：f1–f20 全是 0 字节，fsync 过的 `s<v>` 也全是空的 |

App 被杀的情形（`kill`：只结束负载，不丢写）原来就不坏。

## 做法

照原生 ext4（data=ordered、auto_da_alloc）在断电时的语义补上顺序，设计文档 §8.1 的"断电"一条：

- **改名覆盖之前刷新文件**（`Store::flush_replaced`）。`Store::rename` 覆盖已有的名字、源是普通文件时记下它；提交（`Store::commit`）之前让这些文件的数据落盘：不超过 16 个时先都用 sync_file_range 开始写回、再逐个 fdatasync（ext4 上合进一次日志提交），多了就对整个宿主文件系统 syncfs 一次（apk 升级一个包时一次提交里有几百个）。符号链接、`RENAME_EXCHANGE` 不刷，与 ext4 一样。这一步在提交里，持着 BKL。截断到 0 再写（`> file`、App 的 `writeFile`、"文件"App 的 `wt`）发生在 object 本身上，宿主的 ext4 自己会管。
- **删除 object 之前让 WAL 落盘**（`Meta::sync_metadata`）：后台线程每次删一批之前 fdatasync 一次 WAL（打开 `meta.db-wal`），刷的时候新交来的攒着，下一次一起。检查点写回了的帧已经在刷过的数据库文件里，WAL 不在（最后一个连接关闭时写回、删掉）也没有要刷的。按共享的提交计数器记下刷到了哪里，之后没有新的提交就不再刷；为此提交计数器改到交给后台线程之前加。开机清理上次留下的 nlink = 0 的行时直接删 object，之前也先刷一次：上次是被杀掉的话，最后的提交可能还只在宿主的页缓存里。
- **guest 要求落盘的**：
  - fsync、fdatasync（以及 AIO 的 `IOCB_CMD_FSYNC`、`FDSYNC`）：宿主对 object fsync、fdatasync，再提交、等 WAL 落盘；目录只做后两步。
  - sync、syncfs：提交之后对整个宿主文件系统 syncfs。
  - `O_DSYNC`、`O_SYNC` 打开的文件（以及 pwritev2 的 `RWF_DSYNC`、`RWF_SYNC`）每次写完让宿主 fdatasync、fsync object（Linux 的 generic_write_sync；名字不在其列，Linux 上也要另外 fsync 目录）。
  - `msync(MS_SYNC)`：文件的共享映射就是宿主对 object 的共享映射，对按宿主页对齐的那几段调宿主的 msync。
  - sync_file_range：交给宿主的 object（dpkg 先让一批文件开始写回、再逐个 fsync）。
  - 刷盘时都放开 BKL。
- **导入**：`sync_store`（App 的 `Native.syncRootfs`，`tuxport-run --import` 最后也调）在导入、写好配置之后对仓库所在的文件系统 syncfs，App 再改名就位，然后 fsync `files` 目录，免得改名没落盘、之后的修改随断电回到 `rootfs.tmp` 被当成没导入完删掉。

试过、没有采用的：guest 的 fsync 在数据库也有没落盘的修改时改成 syncfs 一次（一次日志提交同时刷 object 和 WAL），在虚拟机里 1000 次反而更慢（770–808 ms，分别刷是 506–872 ms），还会把别的进程正在写的东西一起刷下去。

## 测试

- 断电（`crash.sh tuxport power`）：

  | | 次数 | 失败 |
  |---|---|---|
  | 这一轮 | 45（带停顿）＋ 30（不停顿） | 0 |
  | 去掉改名覆盖之前的刷新 | 6 | 4：又有 0 字节的 f |
  | 去掉删除 object 之前刷 WAL，不停顿 | 8 | 0 |
  | 同上，带停顿 | 8 | 1：f3、f4、f5 不见了（名字在，object 没了） |

  不停顿时第二种变异抓不到：WAL 一直在变长，ext4 的每次日志提交（比如刷改名覆盖的文件时）会把新分配了块的 WAL 一起写下去，数据库正好跟着落盘了。停顿让后台线程做完检查点，WAL 从头再用、覆盖已经分配的块，不再随日志写回，这才看得出来。交互使用时这是常态。
- App 被杀（`crash.sh tuxport kill`）：这一轮之前、之后的二进制各 10 次，都不坏。
- 测试脚本自己的坑：一开始在切换设备之后才取负载的输出，切换之后 fsync 里的写被丢掉、guest 看来照样成功，打印出来的 "synced" 不算数，10 次里有 2 次因此误报最后一个 `s<v>` 不见了。现在先截输出再切换。
- 内核层（`cargo test`）新增 `power_loss_ordering`：改名覆盖普通文件时记下、提交时刷，改名到新名字、符号链接覆盖不刷；被覆盖的文件的 object 删除之前 WAL 刷过；没有新的提交时不再刷，有了再刷；开机清理之前刷过一次。去掉记下改名、删除之前的刷新、开机时的刷新，它都失败。
- 开销（`tests/crash/fsbench.c`，静态链接，在 tuxport 里和原生各跑 3 次，各 1000 个 4 KB 的文件，仓库在虚拟机本地的卷上；虚拟机的磁盘刷一次比手机快得多）：

  | | 原生 | 这一轮之前 | 这一轮 |
  |---|---|---|---|
  | 新建（不 fsync） | 9–10 ms | 15–36 ms | 15–17 ms |
  | 写临时文件、改名覆盖 | 23–25 ms | 26–28 ms | 41–48 ms |
  | 新建、每个 fsync | 288–361 ms | 36–38 ms（没刷盘） | 506–872 ms |

  新建之后马上 fsync 要刷两样（object 和 WAL），是原生的 1.7–2.4 倍；改写已有文件再 fsync（SQLite、日志）元数据没变，只刷 object。apk 装、删 doas 各 10 次：之前 7.00、6.99 秒，之后 7.19、6.97 秒，看不出差别（apk 每次覆盖的只有它自己的 4 个数据库文件）。
- 仪器测试（Pixel 4a、API 34 的模拟器）：卸载之后跑 `busyboxRunsUnderBothTiers`，全新导入 Alpine（走新的 `Native.syncRootfs`）之后通过。
- Android 上 libc crate 没有 `sync_file_range` 的绑定和 `SYNC_FILE_RANGE_WRITE`，改成按编号调用、常量放在 hal 里。

## 推迟项

- 只在虚拟机的 ext4 上模拟过。很多手机的 `/data` 是 f2fs：f2fs 没有 auto_da_alloc，原生程序在 f2fs 上改名覆盖、断电之后也可能是空的；tuxport 刷的是 fdatasync、syncfs，在 f2fs 上照样起作用。虚拟机里没有 mkfs.f2fs，没试。
- 手机上刷一次要几毫秒到几十毫秒：新建文件就 fsync 的负载（dpkg、apt）大约是原生的两倍；改名覆盖的刷新持着 BKL，apk 升级大的包时别的进程跟着等。都没在真机上量（R1）。
- 断电之后可能留下没人用的 object：新建普通文件时 object 马上建好，它的 inode 行在没落盘的提交里随断电丢了，`lock` 文件里记的编号范围也可能没写回，开机时就清理不到。它只占空间：编号以后分给普通文件时清空接着用，分给目录这类没有 object 的就一直留着。可以在断电之后（宿主的 boot_id 变了、上次没正常关）开机时对一遍 `objects` 目录，没做。
- "文件"App 复制进来的新文件、guest 新建而没有 fsync 的文件，断电之后可能是空的，与原生 ext4 新建的文件一样。
- tmpfs（`/run`、`/dev/shm`）里的文件的数据也在仓库里，fsync 它们照样刷盘；Linux 上 tmpfs 的 fsync 什么也不做。只是慢一点，没有区分。
