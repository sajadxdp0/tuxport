# M9：内核 bug 的报告能复制，带着 panic 的位置（2026-11-03）

[进程被结束的提示](195-kill-reports.md)那一轮的推迟项：tuxport 自己的 bug 结束进程时，App 只说"内部错误"，报告要用 `dmesg` 看，没有"复制报告"的入口。上架之后，用户能把问题带给开发者的就是这份报告。

动手时发现报告本身也不够用：内核日志和交给 App 的那一行只说 `kernel panic in syscall 56`，panic 的消息和代码位置只有默认的 panic hook 打印到 stderr（Android 上转到 logcat，用户拿不到）。只凭系统调用号查不出是哪里出的错。

## 做法

### 报告带上 panic 的消息和位置

`Machine::start` 装上一个 panic hook（每个宿主进程一次，原来的 hook 照样调用）：把消息和位置（`… at core/kernel/src/xxx.rs:行号`）记在出错的线程上，结束进程（`bug::kill`、`kill_process`）、报告不属于进程的 bug（`bug::report`）时接在原因后面。消息最多 1000 字节（`assert_eq!` 带着两边的值，可能很长）。内核日志（`dmesg`）、stderr、交给 App 的报告都是这一行：

```
tuxport: internal error in pid 12 (kernel-panic), thread 12, last guest pc 0x400084: kernel panic in syscall 43409: panic injected by the test hook at core/kernel/src/bug.rs:343; killed the process with SIGSEGV
```

### 通知上"复制报告"

tuxport 自己的 bug 结束进程时，标签页里照旧用黄色说明，并且总是发一条通知（原来界面在、标签页里说明过了就不发），通知上有"复制报告"：复制 App 的版本、设备（Android 版本、厂商和型号、ABI、页大小）、发行版和执行层，加上上面那一行，然后收起这条通知。Android 13 起系统自己提示已复制，之前的版本弹一个 toast。标签页里的说明改成"报告可以在通知里复制，用 dmesg 也看得到"。OOM killer 结束的不是 bug，照旧。

```
tuxport 0.0.0 (1, debug)
Android 14 (API 34), Google sdk_gphone64_arm64, arm64-v8a, page size 4096
Alpine Linux 3.24, Tier 1
tuxport: internal error in pid 12 (kernel-panic), …
```

## 测试

- `guest_programs` 里撞上 bug 的那个用例检查报告里有 panic 的消息和位置；仪器测试 `killsAreReportedToTheApp` 同样检查交给 App 的报告，仪器测试通过。
- 在模拟器上手动走了一遍：临时让登录时运行测试钩子的 `kernel-panic`，标签页里出现说明，通知上有"复制报告"，点了之后通知收起，粘到终端里就是上面那四行。

## 推迟项

- 没有"反馈问题"的入口（打开 issue 页、发邮件）：仓库还是私有的，等定了公开与否再加。
- 报告只有出错的那一行，没有调用栈，也没有出错之前的内核日志。 → 带上了内核日志里它前面的 10 条和它自己（[M9：内核 bug 的报告带上出错之前的内核日志](207-report-kernel-log.md)）；仍没有调用栈：APK 里的 `.so` 去了符号，要自己展开栈、按偏移还原
- panic 的消息记在线程上：别处用 `catch_unwind` 兜住、没有写进报告的 panic（只在测试里有），会留到这个线程下一次报告 bug 时才被取走。
