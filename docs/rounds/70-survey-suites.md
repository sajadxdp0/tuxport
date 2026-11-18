# M9 之前：第二轮普查，语言的测试集和大项目（2026-10-28）

[第一轮普查](62-workload-survey.md)跑的是服务和工具，留下了语言自带的测试集和 PHP、Ruby、Rust 的大型项目。测试集比手写的负载碰到的系统调用多得多，而且有现成的判定。这一轮在 tuxport 和原生上跑同样的负载，对比两边的失败。

## 做法

- 普查仓库加装 go、nodejs、python3-tests、composer、PHP 的常用扩展、ruby-dev 等。原生对照用同一个仓库：在 guest 里用 tar 打包整个根目录，`docker import` 成镜像，两边的文件完全相同。
- `scripts/survey -n <镜像>` 在这个镜像里原生运行同样的负载，输出写到 `<名字>.native.out`；`SURVEY_TIMEOUT` 放宽单个负载的时限。负载在 `tests/survey/suites/`。
- 测试集：CPython 的 `python -m test`（不联网）；PHP 源码包的 `run-tests.php`（Zend、tests 和几个常用扩展）；Ruby 源码包的 `tool/test/runner.rb`（test/ruby、socket、fiber、fileutils）；Node.js 源码包的 `test/parallel`，用装好的 node 跑；Go 标准库的 `go test -short std`（Alpine 的 go 包删掉了 `src` 里的 `_test.go`，只能用 go.dev 的官方构建当 GOROOT）。
- 大项目：ripgrep 的 release 构建和全部测试；composer 新建 Laravel 项目、迁移、测试、内置服务器；gem 装 Rails、新建应用、scaffold、测试、服务器。
- 两边在 colima 的虚拟机（4 个 CPU）里同时跑，tuxport 用 Tier 1。

工具：`scripts/survey -n <镜像> [-s 仓库] 负载……`、`SURVEY_TIMEOUT=秒数`；负载在 `tests/survey/suites/`；导出仓库做原生镜像的命令写在 `scripts/survey` 的开头。

## 结果

| 负载 | 原生 | tuxport |
|---|---|---|
| CPython 3.14，538 个测试文件 | 447 个通过；4 个失败（test_build_details、test_ctypes 是打包的问题，test_urllib、test_urllib2 是环境里的代理），4.4 分钟 | 434 个通过；17 个失败：同样的 4 个，加上下面的问题 1、4、5、6；48 分钟，其中约 40 分钟是两个卡住的测试在等超时 |
| PHP 8.5，12279 个测试 | 0 个失败，1.4 分钟 | 5 个失败（问题 1），7.2 分钟 |
| Ruby 3.4，164 个测试文件 | `-j4` 一起跑 2 分钟 | `-j4` 一起跑时发到第 158 个文件后四个工作进程都不再推进（问题 7）；逐个文件跑：test_require 多了 3 个失败（问题 3），test_module、test_syntax 多出的是 `*_memory_leak` 测试超时（它们按原生速度定时限），其余与原生相同；在同一个 tuxport 里接连跑多个文件时，跑到 test_process 时整个 tuxport 崩溃（问题 2） |
| Node.js 24，test/parallel 约 3900 个 | 5 个失败（Alpine 用系统的 ICU、SQLite 造成的），6.7 分钟 | 103 秒时整个 tuxport 崩溃（问题 2），没有结果 |
| Go 1.26 标准库，250 个包 | 全部通过，5 分钟 | 442 秒时整个 tuxport 崩溃（问题 2），没有结果 |
| ripgrep | 构建 85 秒，约 1200 个测试全部通过 | 构建 177 秒；`cargo test` 时整个 tuxport 崩溃（问题 2） |
| Laravel 13 | 新建项目 28 秒，迁移、2 个测试、服务器正常 | 41 秒，都正常 |
| Rails 8.1 | gem 装 48 个包 53 秒，`rails new` 84 秒，scaffold、7 个测试、服务器正常 | 83 秒、142 秒，都正常 |

未实现的 syscall 与[上一轮](69-glibc-probe.md)相同，另有 Go 的 syscall 测试调用的 39、40（umount2、mount），失败时它们自己跳过。

### 找到的问题

1. **目录的 mtime、ctime 不随目录项变化，普通文件的 ctime 不随内容变化**。在目录里新建、删除、改名都不改父目录的时间（仓库的 `add_dentry` 和删除目录项不碰父目录）；普通文件的 ctime 只取数据库里那一列，write、truncate、utimensat 改的是宿主 object 的 mtime，ctime 不动；目录和符号链接的 utimensat 也不改 ctime。PHP 的 `lstat_stat_*`、`touch_basic` 因此失败。实际的影响更大：Python 的 importlib 按目录的 mtime 决定要不要重新列目录，进程从一个目录 import 过之后，在那里新建的模块 import 不到（原生上能）；Maildir（test_mailbox）、git 的 untracked cache 这类按目录 mtime 发现变化的程序都会漏掉修改。→ 已修好（M9 之前：目录和文件的时间戳）
2. **线程频繁开始、退出时宿主的堆被破坏，整个 tuxport 崩溃**。Node、Go、ripgrep 的测试和 Ruby 的连续运行都以 glibc 的 `corrupted double-linked list` 之类结束，有时先有几处 "RefCell already borrowed" 的内核层 panic（定时器线程、wait4、进程结束时放开被跟踪的进程）。只让 4 个进程各用 4 个线程不停地创建、join 短命线程（一个 C 小程序，修的那一轮放进测试），60 秒内必崩。读代码找到一处：`task::thread_main` 在 `run` 返回、BKL 已经放开之后才 `CURRENT.set(None)`，丢掉线程局部变量里的 `Weak<Task>` 和 `Weak<RefCell<ThreadGroup>>`。Rc 的计数不是原子的，同一个进程里刚开始的线程正持有 BKL 做 `Rc::downgrade(&group)`，两边同时改弱引用计数就可能丢掉一次加一，计数偏小，分配被提前释放，之后的访问读写的都是释放掉的内存。是不是唯一的原因，修的那一轮用 churn 验证。→ 已修好（M9 之前：线程退出时在 BKL 外丢掉弱引用），就是这一处
3. **FIFO 的读端在还没有写端连上过时，poll 报 POLLHUP**。Linux 的 `pipe_poll` 在第一个写端出现之前（`w_counter` 没变）不报挂断，poll 会一直等。Ruby 的 `load` 一个 FIFO 时以 `O_NONBLOCK` 打开再 poll，tuxport 上立刻读到 EOF，当作空文件 load 成功，test_require 的三个 FIFO 测试失败。→ 已修好（M9 之前：FIFO 在写端来之前不报 POLLHUP）
4. **PTY 的 TCFLSH 在主设备上方向反了，TCOOFF 不让从设备的写阻塞**。`syscall::tty` 的 TCFLSH 不管是主设备还是从设备，一律按从设备的方向清：对主设备 TCOFLUSH 清掉了从设备写出、等主设备读的数据，之后主设备的读永远等不到（test_termios、test_ioctl 各卡到 20 分钟的超时）。TCXONC 的 TCOOFF 只让主设备读不到，从设备的 write 照样立刻成功，Linux 上要阻塞到 TCOON。→ 已修好（M9 之前：PTY 的 TCFLSH 和 TCXONC）
5. **每个进程比原生多占约 12 MB**。20 个空闲的 python3 在 tuxport 里让宿主进程多了约 300 MB（RssAnon 约 100 MB、RssShmem 约 200 MB），原生上它们的匿名内存一共约 60 MB。RssShmem 是 Tier 1 的翻译缓冲区（W^X 的两个视图用共享匿名映射，`tuxport_map_code`），每个进程从模板复制一份，python 约 10 MB。CPython 的多进程测试一次起上百个进程，App 的匿名内存到了默认上限（虚拟机的一半，2 GB），OOM killer 结束了 33 个 python 进程，9 个多进程测试因此失败。这是已知的设计代价（[性能剖析](13-profiling.md)就提过翻译要在进程之间直接共用），不是 bug。
6. **FICLONE 返回 ENOTTY**。Linux 上不支持 reflink 的文件系统返回 EOPNOTSUPP，Python 3.14 的 `pathlib.Path.copy()` 遇到它才退回普通的复制，遇到 ENOTTY 就报错：tuxport 上 `Path.copy()` 复制任何文件都失败（test_pathlib 的 28 个 error）。→ 已修好（M9 之前：FICLONE 返回 EOPNOTSUPP）
7. **Ruby 的测试 `-j4` 一起跑时卡住**：四个工作进程各占一个 CPU 却不推进，原因没查到。逐个文件跑都正常，可能与问题 2 的内存破坏有关，修完 2 之后重跑。→ 与问题 2 无关：修完 2 之后测试跑完了，留下的进程停在 Tier 1 往翻译表插入时找空位的循环里，表被作废的块留下的墓碑占满（M9 之前：线程退出时在 BKL 外丢掉弱引用）；已修好（M9 之前：翻译表里漏掉的墓碑）

接下来：先修 2（会让整个 App 崩溃），修完重跑 Node、Go、ripgrep 的测试和 Ruby 的 `-j4`，它们这次都被它挡住了；然后依次修 1、6、4、3。5 是优化方向，不在这一串里。

## 推迟项

- Node.js、Go 标准库、ripgrep 的测试结果被问题 2 挡住了，要修完重跑。→ 修完 2 之后又被翻译表的 bug 挡住（M9 之前：线程退出时在 BKL 外丢掉弱引用），修完它重跑了：ripgrep、Go 与原生相同，Node 多出的失败是问题 5 的 OOM 和两个新问题（M9 之前：翻译表里漏掉的墓碑）
- 问题 7（Ruby `-j4` 卡住）没有查明。→ 查明了，是翻译表的墓碑（M9 之前：线程退出时在 BKL 外丢掉弱引用），已修好（M9 之前：翻译表里漏掉的墓碑）
- 问题 5：每个进程各一份翻译，多进程负载的内存是原生的几倍；要在同一个程序的进程之间共用翻译，或者按设备调整内存上限。→ 量清楚了一部分（M9 之前：代码缓存只算一次）：RssShmem 里代码缓存的写入、执行两个视图各算一次，翻译多算了一倍，已改为只算一次，20 个空闲的 python3 的 App 用量 321 → 218 MB（原生 59 MB）；翻译代码的密度提高了约两成（M9 之前：翻译代码的密度）；同一个程序的进程映射一致时共用一个实例（M9 之前：同一个程序共用翻译），20 个 python3 的 App 用量 84 MB；有 JIT 代码的进程也共用程序文件里的翻译（M9 之前：有 JIT 代码的进程也共用翻译），4 个空闲的 node 的代码缓存 70 → 21 MB，几个 java 进程同时运行时没量
- Ruby 的 `*_memory_leak` 测试在 tuxport 上超时：只是慢，测试的时限按原生速度定。→ 已确认（M9 之前：补测待查的几项）：放宽时限之后都通过，其中一个在原生上也超时；普查脚本改为放宽 5 倍
- CPython 的 test_os 第一遍失败一个、重跑通过，没有查。→ 已查（M9 之前：补测待查的几项）：`test_timerfd_ns_TFD_TIMER_ABSTIME` 要求三次系统调用在 1 毫秒之内，满载时原生也会超过，tuxport 没有系统性的偏差
- 仍然只在 colima 的虚拟机里跑，没上手机。
