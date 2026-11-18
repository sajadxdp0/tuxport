# M9：内核 bug 的报告带上出错之前的内核日志（2026-11-03）

[内核 bug 的报告能复制](205-copy-bug-report.md)那一轮的推迟项：通知上"复制报告"复制的只有出错的那一行，出错之前发生过什么要用户自己去 `dmesg` 里找。

内核日志里有的是（`printk` 的调用方和 `/dev/kmsg`）：tuxport 自己的 bug 报告、OOM killer 结束进程、pids 控制器拒绝 fork、1024 以下的端口绑到了宿主上的哪个端口，以及 guest 写进 `/dev/kmsg` 的。没有开机信息。出错之前有没有别的 bug（一个 bug 留下的不一致可能引出下一个）、刚结束过哪个进程、内存紧不紧，都在这里。

## 做法

- **`Klog::lines_from(seq)`**：序号不小于 `seq`、读得到的记录，像 `dmesg` 那样每行一项，前面加 `[秒.微秒] `。总是带时间（不看 `/sys/module/printk/parameters/time`），`dmesg -c` 清空过也照样有；控制字符转义成 `\xNN`（guest 写进来的可能带终端的转义序列），不是 UTF-8 的字节换成 U+FFFD。还没定稿的最后一条（等着 `KERN_CONT`）不算，与读日志的接口一样。
- **`bug::log`**（结束进程时把报告记进内核日志）记之前先看接下来的序号，记完之后取它前面的 10 条和报告自己。报告本来就在最上面，日志里再出现一次是为了带着它的时间戳，和前面几条放在一起看得出隔了多久。日志正被借用（在日志自己的代码里出了错）时是空的。一条记录最长 991 字节，最多约 11 KB，放进剪贴板、放进通知的 PendingIntent 都没问题。
- **`KillReason::Bug`** 改成 `Bug { report, log }`（`tuxport_kernel` 的公开接口），OOM 的报告不带。
- **JNI**：`takeKills` 每一项原来空着的 b 字段放日志的各行，以 U+001E 分隔；各项里的换行、制表符、U+001E 换成空格。
- **App**：复制的报告在原来四行后面空一行，接 `Kernel log:` 和各行。

模拟器上（临时让登录时先 `echo before the bug > /dev/kmsg` 再运行测试钩子的 `kernel-panic`），点通知上的"复制报告"，粘到终端里是：

```
tuxport 0.0.0 (1, debug)
Android 14 (API 34), Google sdk_gphone64_arm64, arm64-v8a, page size 4096
Alpine Linux 3.24, Tier 1
tuxport: internal error in pid 10 (kernel-panic), thread 10, last guest pc 0x400084: kernel panic in syscall 43409: panic injected by the test hook at core/kernel/src/bug.rs:360; killed the process with SIGSEGV

Kernel log:
[11791.395193] before the bug
[11791.396111] tuxport: internal error in pid 10 (kernel-panic), thread 10, last guest pc 0x400084: kernel panic in syscall 43409: panic injected by the test hook at core/kernel/src/bug.rs:360; killed the process with SIGSEGV
```

时间戳是宿主的 `CLOCK_MONOTONIC`（与 `dmesg` 一样），这里是模拟器开机之后的秒数。

### 调用栈没有做

APK 里的 `libtuxport.so` 是 AGP 去掉了符号的（`stripDebugDebugSymbols`、`stripReleaseDebugSymbols`），Rust 的 `Backtrace` 在里面只打得出绝对地址。要能用，得自己展开栈、按 `dladdr` 换成相对 `libtuxport.so` 的偏移，报告里带上 build-id，每个发布的版本留一份没去符号的 `.so` 在开发者手里还原。等真有用户报来查不出的 bug 再做；panic 的位置（文件和行号，第 205 轮）多数时候已经够定位。

## 测试

- 单元测试 `lines_from_are_dmesg_lines_with_escapes`：拆行、转义、总是带时间、没定稿的那条不算。
- `guest_programs` 里撞上 bug 的那个用例看每份报告带的日志：格式对，最后一行是报告自己，从第二份起有前一份报告。仪器测试 `killsAreReportedToTheApp` 看交给 App 的日志里有之前写进 `/dev/kmsg` 的那一行，仪器测试通过。
- 在模拟器上手动走了一遍（上面那段）。

## 推迟项

- 没有调用栈（见上）。
- 固定取 10 条，不看时间：很久以前的记录（比如开机时发布端口的那几行）也会带上，隔得多远看时间戳。
- 日志里没有 tuxport 自己的开机信息（版本、执行层、内存上限这些），报告的前几行由 App 补上；`tuxport-run` 撞上 bug 时只有 stderr 的那一行。
