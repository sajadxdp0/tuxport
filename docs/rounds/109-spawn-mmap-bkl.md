# M9 之前：起进程和 mmap 的持锁时间（2026-10-30）

内存的三轮做完，接着拆 BKL。先用第 38 轮的插桩（`target/mem/bklinstr.py`、`bklsurvey.sh`，按系统调用记持锁、等锁的时间）重新量了一遍，前几轮之后的分布（arm64 的编号：222 mmap、215 munmap、56 openat、94 exit_group、220 clone、221 execve、260 wait4、165 getrusage、62 lseek）：

| 负载 | 持锁最多的 | 等锁最多的 |
|---|---|---|
| 4 个同时 exec（spawnbench） | exit_group 7.4 µs/次、clone 18.7、execve 4.9 | wait4 0.12 秒、execve 0.06 秒 |
| `xargs -P4` 编译 Lua | mmap 17 万次共 0.42 秒（2.5 µs/次）、munmap 0.09 秒 | mmap 0.14 秒、munmap 0.14 秒 |
| `go build` | openat 21 µs/次、mmap 21 µs/次 | mmap、读、`9999`（线程开始等）各约 0.1 秒 |
| Maven | mmap 0.21 秒（10 µs/次）、getrusage 0.07 秒（11 µs/次）、lseek 0.05 秒 | 被打断的线程（Kick）0.34 秒 |

4 个同时起进程时锁大半时间被占着，`make -j`、configure 这类负载卡在这里。perf 剖析单个进程反复起进程（`spawnbench exec`），持锁的路径上最大的一项出人意料：线程开始时为上下文切换次数取基数，读的是宿主的 `/proc/self/task/<tid>/status`，占了全部时间的 7.2%；`Mm::fork` 5.5%；线程挂上翻译的 `tuxport_dbt_attach`（清空 64 KB 的跳转缓存）1.8%。`Mm::mmap` 的时间 86% 是宿主的 mmap 调用。

## 试过、量过没有做的

- **getrusage 不取 BKL**：HotSpot 的 `os::elapsedVTime` 用 `RUSAGE_THREAD`，Maven 一次构建 6000 次、每次持锁 11 µs。线程自己的 CPU 时间可以不持锁读（`clock_gettime` 已经这样做），但为了让用户态、内核态时间不倒退（[第 80 轮](80-cpu-times.md)），计算时要改线程上的几个 Cell，持锁读 `/proc` 的一方也改它们，不持锁就是数据竞争。只占 Maven 的 1%，没做。
- **fork 时可写的私有文件映射写时复制**（[第 29 轮](29-bkl-wait-hold.md)的推迟项）：给 `Mm::fork` 计时，一次 fork（spawnbench，11 个 VMA）约 10 µs：复制数据段等可写私有文件映射的页 3.4 µs，给写过的匿名页记借出 2.2 µs，建子进程的页表 1.9 µs，新建 Mm 1.2 µs。写时复制现在只认私有匿名内存（借出、还回、孤儿页、`discard_unlent` 都按匿名内存写），扩到文件映射牵涉面大，只省 3.4 µs，没做。
- **被打断的线程不取 BKL**：Maven 里 3.6 万次打断等锁共 0.34 秒，仍是[第 38 轮](38-split-bkl.md)撤回的那个问题：只为 shootdown 的打断不进内核时，线程比核多时拆映射的一方要等被打断的线程被宿主调度上来。

## 做法

- **线程开始时用 getrusage 取上下文切换次数的基数**（`tuxport_hal::thread_ctxt_switches_self`）：宿主线程从线程池里复用，`/proc/<pid>/status` 的 `voluntary_ctxt_switches` 要减去线程开始时的读数（[第 48 轮](48-exclusive-wakeup.md)）。原来持着 BKL 读宿主的 status 文件、逐行找这两项，每次十来微秒；`getrusage(RUSAGE_THREAD)` 的 `ru_nvcsw`、`ru_nivcsw` 是内核里同一对计数，一次系统调用。读别的线程的仍然读 status（只在读 `/proc` 时）。
- **私有匿名的宿主内存交还时留着复用**（`mm::recycled`）：
  - 所有 guest 进程都在同一个宿主进程里，宿主的 mmap、munmap 都要拿这一个进程的 `mmap_lock` 写锁，原生的每个进程各有一把。
  - 现在 `new_anonymous` 和 fork 预先映射的那几段（`spare`）申请的私有匿名内存，交还时只丢弃内容（`MADV_DONTNEED`，只拿读锁），按（长度, 计不计入提交量）留着；最多 256 MB 地址空间，每种大小最多 32 段，页已经还给宿主。
  - 下一次申请同样大小的直接用，丢弃过的内容读出来是零，与新映射的一样。
  - 复用发生在 shootdown 完成、宿主内存原来就要交还的时刻（`release_unmapped`），之前宿主也可能把同一段地址再分出去。
  - `xargs -P4` 编译 Lua 时 mmap 每次持锁 2.47 → 1.27 µs（合计 0.42 → 0.22 秒），mmap、munmap 的等锁时间 0.14、0.14 → 0.09、0.08 秒。
- **挂上翻译挪到放开 BKL 之后**：线程开始、exec 之后、离开共用的组之后，在锁里只拿翻译空间（`Mm::code_space`），清空跳转缓存这些引擎里的事放开 BKL 再做。

## 结果

与第 108 轮的二进制交替：

| 负载 | 之前 | 之后 |
|---|---|---|
| 单个进程 fork 后 exec（spawnbench，每次） | 146–153 µs | 136–144 µs |
| 4 个同时 exec（每次） | 414–474 µs | 376–402 µs |
| 4 个同时跑管道（每次） | 1539–1629 µs | 1352–1396 µs |
| `xargs -P4` / `-P1` 编译 Lua 的 33 个文件 | 5.08–5.12s / 15.39–15.46s | 4.96–5.06s / 15.02–15.17s |
| python / gcc / shell 起进程 / `git status` | 1.32–1.39s / 2.18–2.21s / 0.17–0.18s / 0.07s | 1.32–1.38s / 2.13–2.28s / 0.16–0.18s / 0.07–0.08s |
| `node -e 0` 之后 10 次 / `go build` | 0.41–0.44s / 5.66–5.86s | 0.40–0.45s / 5.79–5.88s |
| Maven / Gradle 热构建 | 5.77–5.98s / 1.31–1.47s | 5.66–5.81s / 1.29–1.35s |

原生的 spawnbench exec 约 145 µs（第 36 轮），4 个同时 158 µs：单个进程已经和原生一样快，4 个同时的仍是原生的 2.4–2.5 倍（第 36 轮是 2.5–3.1 倍）。`go build` 交替 4 轮是 5.83–6.03 对 5.89–5.98 秒，没有变化。

## 测试

- 差分测试 `c-programs` 新增：几个线程和一个子进程反复映射、写满、拆掉几种大小的匿名内存，新映射里不能有非零的字节。
- 只在 colima 的虚拟机里跑，没上手机。

## 推迟项

- mmap、munmap 仍持 BKL（复用之后每次约 1.3 µs、0.6 µs），R1 的退路（地址空间的操作改用每个 mm 自己的锁）没做。→ 8 核上量过（M9 之前：8 核上的扩展性和 BKL 的交接）：`xargs -P8` 编译只比 `-P1` 的倍数多约 5%，不做
- Go、JVM 的大块映射（`go build` 里 mmap 每次约 18 µs，最长 4.5 ms）：软件页表要逐页填进去，Linux 是缺页时才填。→ 1 MB 以上的映射改为第一次访问时按 2 MB 补表（M9 之前：页表按需建立），`go build`、Maven 的时间在噪声以内
- clone 里 `Mm::fork` 约 10 µs（见上），exit_group 约 7 µs，execve 在锁里的部分约 5 µs；4 个同时起进程仍是原生的 2.4 倍多。→ 等锁时最多让 4 个线程自旋，8 核上 4 个同时 exec 是原生的 2.2–2.3 倍、8 个同时 4.1–4.3 倍（M9 之前：8 核上的扩展性和 BKL 的交接）；仍持 BKL，拆出来的代价和收益见那一篇的推迟项
- `go build` 的 openat 每次 10–21 µs，没有拆开看。→ 拆开看了（M9 之前：openat 的开销）：合计只占墙钟的 1.3%，平均数是被新建文件（55 µs，大头是宿主上建 object 和 SQLite）拉高的，没有便宜的改法，没改
- 被打断的线程取 BKL 看信号（Maven 0.34 秒等锁），见上。
- 所有 guest 进程共用宿主进程的一把 `mmap_lock`：复用只省掉了匿名内存的 mmap、munmap，文件映射、fork 预先映射以外的、exec 建地址空间的映射照旧。
