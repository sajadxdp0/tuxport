# M9 之前：调试器、valgrind 和 headless Chromium（2026-10-29）

这一轮跑以前的普查（[第一轮](62-workload-survey.md)、[第二轮](70-survey-suites.md)、[glibc 发行版](69-glibc-probe.md)）没跑过的几类负载。它们碰的是 tuxport 最容易出偏差的地方：

- **delve、lldb**：gdb、strace 以外的调试器，大量用 ptrace 的各种请求，读 `/proc/<pid>/` 下的东西；
- **valgrind**：自己也是一个二进制翻译器，跑在 tuxport 的翻译器上面；
- **headless Chromium**：经由 Puppeteer、Playwright 驱动，是多进程、共享内存、V8 即时编译的大程序，手机上做网页抓取和前端测试要用它。

## 做法

- 仓库沿用 [glibc 探测](69-glibc-probe.md)那一轮的 Debian 13（`target/glibc/debian-store`），负载是 `tests/survey/glibc/debian-{valgrind,lldb,delve,chromium}.sh`，各自用 apt 装缺的包，经由 `scripts/survey -s target/glibc/debian-store glibc/<名字>` 运行。在 colima 的虚拟机里用 Tier 1 跑。
- 原生对照是同一个 `debian:trixie` 镜像里的容器，`target/survey/native-tools.sh` 装一次包，再依次跑这四个负载。容器去掉 seccomp、加上 `CAP_SYS_PTRACE`，让调试器像在普通的 Linux 上一样能关掉 ASLR；fd 的上限与 tuxport 的默认值一样，是 1024/4096。
- 虚拟机的磁盘满了，装好的容器提交不成镜像，所以对照每次都要重装。

工具：四个负载在 `tests/survey/glibc/`；原生对照的脚本是 `target/survey/native-tools.sh`（不在仓库里，做法见上）。

## 结果

### 找到的三个问题，都在内核层

1. **valgrind 的 memcheck 报了几千个"未初始化"的错误**（`3421 errors from 313 contexts`，应该是 3 个）。原因是 `/proc/<pid>/fd` 和 maps 里的路径不解析符号链接。valgrind 按这个名字带着 `O_NOFOLLOW` 重新打开 Debian 的 `/lib/ld-linux-aarch64.so.1`，最后一级是链接，得到 ELOOP；读不到 ld.so 的符号，就替换不了 ld.so 里按字读的字符串函数。见[下一轮](93-fd-paths-resolve-symlinks.md)。
2. **Chromium 一启动就 abort**（`GPU process isn't usable. Goodbye.`）。zygote 在 fork 之前按 `/proc/self/task` 的链接数判断自己是不是单线程，`CHECK` 它至少是 3（2 加线程数）。tuxport 的合成目录都报告 2，zygote 于是撞上 `brk #0`，被 SIGTRAP 结束，浏览器起不了 GPU 进程。见[第 94 轮](94-task-dir-links.md)。
3. **delve attach 之后不 detach 就退出，进程停住，SIGTERM 也结束不了它**。原因是跟踪者退出时，tuxport 把信号投递停止里的信号投递了出去，这里就是 attach 送的 SIGSTOP。delve 的 `exit` 会问要不要结束进程，stdin 是 /dev/null 时读到 EOF，`handleExit` 不 detach 就返回 2。Linux 上这时信号已经被跟踪者的 wait 取走，丢掉了，进程继续运行。见[第 95 轮](95-tracer-exit-drops-signal.md)。现在负载里经由管道回答 `n`，走显式的 detach。

### 修好之后都正常

与原生的差别只在不确定的地方。这些地方是：用时，lldb 里没初始化的变量的值，同时停在断点上的线程、goroutine 有几个、先停下的是哪个。下表的用时是 tuxport / 原生：

| 负载 | 跑了什么 | 用时 |
|---|---|---|
| valgrind 3.24 | memcheck 报出越界读、未初始化、泄漏的 40 字节，带源码行号；helgrind、drd 报出数据竞争；massif、callgrind 的输出由 ms_print、callgrind_annotate 读出；`--trace-children` 跟进 sh、ls；在 valgrind 里跑 python | 整个负载 10 / 4 秒，python 5 / 2 秒 |
| lldb 19.1 | 线程里的断点、`bt`、`thread list`、`register read`、`next`；表达式求值（`expr square(9)`：clang 编出代码，经由 lldb-server 写进客户程序的内存里执行）；硬件观察点报告旧值、新值；attach 到运行中的进程再 detach，进程继续运行；读 core 文件 | 5 / 2 秒 |
| delve 1.24、Go 1.24 | goroutine 里的断点、`stack -full`、单步、`locals`、列出 goroutine 和线程；写观察点；`dlv debug` 现编现调；headless 的服务端加 `dlv connect`；attach 再 detach | 11 / 6 秒（go build 6 / 1 秒） |
| Chromium 156 | `--dump-dom`（页面里的 JS、fetch、Web Worker）、`--screenshot`、`--print-to-pdf`；Puppeteer 25.16 经由 WebSocket 上的 DevTools 协议：输入、点击、读 canvas 的像素、截图、PDF、第二个标签页；Playwright 1.64 经由 fd 3、4 上的管道做同样的事 | `--dump-dom` 3 / 0 秒，Puppeteer 3 / 1 秒，Playwright 4 / 0 秒 |

未实现的 syscall 与以前的普查相同，都是没有配对应功能的 Linux 也会返回 ENOSYS 的：293（rseq）、425（io_uring_setup，libuv）、444（landlock）、452（fchmodat2）。Chromium 给匿名映射起名字的 `PR_SET_VMA_ANON_NAME` 是 EINVAL，与没编进 `CONFIG_ANON_VMA_NAME` 的 Linux 一样，它不在意。

## 推迟项

- 只在 colima 的虚拟机里跑过，没上手机。手机上 Chromium 的内存（浏览器、zygote、GPU、网络服务、渲染器各是一个进程，每个进程各有一份翻译，见[第二轮普查](70-survey-suites.md)的问题 5）没量。
- Chromium 都带着 `--no-sandbox`（root 运行时它要求这样，容器里也都这样用）。它的沙箱要用户命名空间或者 setuid 的辅助程序，加上 seccomp-bpf 的过滤，没试。 → 用户命名空间补上了（M9 之前：用户命名空间），pid 命名空间也补上了（M9 之前：pid 命名空间），网络命名空间也补上了（M9 之前：网络命名空间），仍没试
- `/proc/<pid>/net` 不存在（`/proc/net` 有）。Linux 上 `/proc/net` 是指向 `self/net` 的链接，经由 `/proc/self/net/...` 读的程序读不到。这是对照目录的链接数时顺带看到的。→ 已补上（M9 之前：`/proc/<pid>/net`）
