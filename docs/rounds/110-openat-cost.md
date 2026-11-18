# M9 之前：openat 的开销（2026-10-30）

[上一轮](109-spawn-mmap-bkl.md)的推迟项：`go build` 里 openat 每次持锁 10–21 µs，没有拆开看。这一轮给 openat 分阶段计时（临时插桩 `target/mem/openinstr.py`，`openbuild.sh` 编到 `target/linux-arm64-exp`，`OPENSTAT=1` 时退出前打印），结论是不用改：openat 合计只占 `go build` 墙钟的 1.3%，平均数是被新建文件拉高的，新建文件的大头在宿主上建 object 和 SQLite，没有便宜的改法。

## `go build`

本地卷上的仓库，冷缓存，墙钟 5.9 秒：

| 种类 | 次数 | 平均 | 合计 |
|---|---|---|---|
| 新建普通文件 | 604 | 55–66 µs | 33–40 ms |
| 打开已有的普通文件 | 3945 | 7.6 µs | 30 ms |
| 目录、合成文件、`O_TRUNC` 等 | 约 900 | 2.6–37 µs | 4 ms |
| 不存在（ENOENT 等） | 1206 | 3.1 µs | 3.7 ms |

新建文件的 55 µs 里：

| 部分 | 平均 |
|---|---|
| 宿主上在 `objects` 目录里建 object（`openat`，O_CREAT） | 墙钟 26.7 µs，其中线程 CPU 时间 19.6 µs |
| SQLite：插入 inode、目录项 | 各约 4 µs |
| 事务本身（保存点、安排推迟的提交） | 约 8 µs |
| 事务之外（再解析一次上级目录、权限检查、显示用的路径、inotify） | 约 12 µs |

同一个二进制在 fsbench 的紧循环里新建一个文件只要 18 µs（宿主建 object 8.9 µs，两次插入合计 2.5 µs），各部分在 `go build` 里都慢 2–4 倍：新建夹在大量 guest 代码中间，宿主内核和内核层的缓存都是冷的。`objects` 目录本身的大小不是原因：虚拟机里原生在一个有 14 万个文件的目录（与 `/bench/store` 的 `objects` 一样大）里新建文件 6–9 µs，在空目录里 12 µs。

打开已有文件的 7.6 µs 里宿主打开 object 约 4 µs，路径解析（缓存命中）1.2 µs，显示用的路径 1.4 µs。有一次宿主打开 object 用了 6.2 ms（阻塞在宿主内核里），这期间持着 BKL，别的线程都要等；这类宿主 I/O 的停顿在手机的存储上可能更多，没有办法预先避开。

## 别的负载

python 启动并 import json、asyncio、email 打开 235 个文件共 2 ms；gcc 编译 `lvm.c` 打开 212 个、新建 77 个共 2.2 ms；`git status` 打开 896 个目录、22 个文件共 2.2 ms。都不值得再优化。

## 能做但没做的

把建 object 挪出 guest 线程。后台线程按下一个要分配的 inode 编号（SQLite 的 rowid 在一次会话里只增不减）预先建好一批 object、留着打开的 fd，新建文件时直接拿来用，省掉宿主上的那 9–27 µs。崩溃之后要删掉预先建好、没有用上的 object：`lock` 文件里的新分配范围在提交时清零，得另记一个不随提交清零的上界，`lock` 文件也要从 32 字节加长。收益：`tar x` 解包 Python 标准库（1262 个文件，70 ms）里建 object 共 9 ms，最多快 13%；`go build` 只有 0.3%。

## 推迟项

- 预先建 object（见上）：解包、`npm install`、`git clone` 这类新建文件多的负载最多快一成多，崩溃恢复和 `lock` 文件的格式要跟着改。
- 新建文件时路径解析了两遍（`walk` 失败之后 `resolve_parent` 从头再解析，名字也查了三次），在 `go build` 里合计约 12 µs 中的一部分，fsbench 里只有约 3 µs。
