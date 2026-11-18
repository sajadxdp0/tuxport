# M8 补测：JVM（2026-10-25）

OpenJDK 21 和 25 在 tuxport 里行为正确。覆盖线程、虚拟线程、JIT、GC、异常、子进程、网络、文件和信号的测试程序，在两个执行层下、各种 GC 和 JIT 配置下输出都和原生一致；jshell、jcmd、jstack 和 `kill -3` 的线程转储正常；Maven 和 Gradle 能从 Maven Central 下载依赖，编译项目、跑 JUnit 5 测试并打包。补测发现两个问题并已修好：经由符号链接启动的程序找不到自己的库（`/proc/self/exe` 没有解析符号链接），以及没有 inotify（WatchService 用不了）。

## 做法

- **`/proc/<pid>/exe`**：原来是 execve 拿到的路径做词法规范化，不解析符号链接。musl 按 `/proc/self/exe` 展开主程序的 `$ORIGIN`，经由 `/usr/bin/java`（指向 `../lib/jvm/default-jvm/bin/java`）启动时找不到 `libjli.so`。现在与 Linux 的 `d_path` 一样，给出解析完符号链接的路径（`Vfs::real_path`），maps 里的名字相同。Go 的 `os.Executable`、Node.js 的 `process.execPath` 同样受益。
- **代码失效日志**（§4.2）：原来只保留 64 个代数。HotSpot 每装一个编译好的方法就 `ic ivau` 一串缓存行，睡一会儿的 GC、编译线程醒来几乎都落在日志之外，只能整体清空翻译缓存。一次 Maven 构建清空了 2449 次，其中 2388 次是这个原因。改成保留 4096 个代数之后，清空降到 248 次，翻译的块从 1466 万降到 128 万，用户态 CPU 时间少了 22%。墙钟时间没有变，因为这些重新翻译都在后台线程上，虚拟机的 4 个核本来就有空闲；核少的手机上应当能看出来。
- **inotify**（§8.3）：`inotify_init1`、`inotify_add_watch`、`inotify_rm_watch` 和对它的 read、poll、FIONREAD。事件的种类、顺序、`IN_ISDIR`（`*_SELF` 事件不带）、rename 的 cookie、`IN_ONESHOT`、`IN_EXCL_UNLINK`、`IN_MASK_ADD`、`IN_MASK_CREATE`、队列溢出、合并相同事件都与 Linux 相同。`IN_CLOSE_*` 在打开文件的最后一个引用释放时报告。新增 `/proc/sys/fs/inotify/*` 和 `/proc/<pid>/fdinfo/<fd>`（位置、标志、inode，eventfd 的计数，inotify 的监视）。Java 的 WatchService、Node.js 的 `fs.watch` 因此可用。

## 结果

测量条件：与 M8 相同，原生是同一个仓库导出的 docker 镜像（Alpine 3.24 的 openjdk21 21.0.12、maven 3.9.16、gradle 8.14.4）：

| 负载 | 原生 | tuxport Tier 1 | 倍数 |
|---|---|---|---|
| 测试程序（`-XX:+UseSerialGC`） | 0.84s | 3.93s | 4.7× |
| 同上，ZGC | 0.97s | 4.91s | 5.1× |
| 同上，只用 C1 | 0.87s | 3.57s | 4.1× |
| 同上，纯解释（`-Xint`） | 7.58s | 28.4s | 3.7× |
| Maven `package`，依赖已在本地 | 1.68s | 18.5s | 11× |
| Gradle `build`，守护进程已在运行 | 0.52s | 7.5s | 14× |
| Gradle `build`，冷启动并下载依赖 | 8.9s | 64.7s | 7.3× |

Maven、Gradle 慢在 JIT 生成的代码上：Gradle 那次运行里，间接跳转（包括 `ret`）没命中跳转缓存、退回分发器 1.09 亿次；HotSpot 不停地给编译好的代码打补丁（内联缓存、调用点），作废了 82 万个块。

- **不用改就正常的**：
  - 隐式空指针检查、栈溢出保护页（第二次溢出还能抓到，说明黄区重新打开了）、safepoint，都靠 SIGSEGV。
  - Serial、Parallel、G1、ZGC（含分代 ZGC 和多重映射的堆）、Shenandoah；JDK 25 的紧凑对象头。
  - 虚拟线程，CDS 归档的映射。
  - jcmd/jstack 的 attach：SIGQUIT 加 AF_UNIX；jshell 经由 socket 用 JDI 控制另一个 JVM。
  - `sun.misc.Signal`、SIGTERM 触发关闭钩子，posix_spawn 加 jspawnhelper，NIO 的 epoll Selector，HttpClient 和 HttpServer，内存映射和文件锁，crypto 指令内建。
  - `-XshowSettings:system` 显示"No metrics available"，因为没有 cgroup（D 档）；JVM 退回到宿主的 CPU 数和内存，算出的参数与原生相同。

## 测试

- 差分测试新增两组：`java`（上面的测试程序、ZGC、jcmd attach）和 `inotify`（C 程序打印的事件序列和错误码，以及 WatchService 和 `fs.watch`），两个执行层下都和原生一致。差分测试的仓库多装 openjdk21-jdk。
- 单元测试新增 `real_path` 和 inotify；rootfs 用例里 `readlink /proc/self/exe` 的期望值改为 `/bin/busybox`。
- LTP 新通过 inotify01、02、04、05、06、09–12 和 inotify_init1_01、02；inotify03、07、08 要挂载块设备。

## 推迟项

- JIT 负载的性能：跳转缓存是 4096 项直接映射，JIT 代码里间接跳转和 `ret` 的目标多，冲突严重；给编译好的代码打补丁按页作废翻译。见 R1 的退路。→ `ret` 改走返回预测（M9 之前：栈指针常驻寄存器和返回预测），跳转缓存没命中时先在翻译后的代码里查翻译表（M9 之前：多线程共用翻译和定向唤醒），作废改为按内容（M9 之前：JIT 负载）；跳转缓存仍是 4096 项直接映射
- inotify 的近似：监视建立之前就打开的文件关闭时不报告；打开之后改名的文件，按 fd 的事件仍然报给原来的目录；`IN_DELETE_SELF` 在最后一个链接删除时报告，不等文件关闭；`max_user_instances`、`max_user_watches` 可写但不起作用。→ 都已去掉（M9 之前：推迟项）
- fdinfo 只有通用的几行和 eventfd、inotify 的内容，没有 epoll、timerfd、signalfd 的；挂载编号是合成的，没有 `/proc/<pid>/mountinfo`。→ 已补上（M9 之前：兼容性补漏）
- 没有 fanotify。
