# M9 之前：`/proc/<pid>/task` 的链接数（2026-10-29）

[第 92 轮](92-debuggers-valgrind-chromium.md)在 Debian 上跑 headless Chromium，一启动就 abort（134）：

```
ERROR:content/browser/gpu/gpu_process_host.cc:1038] GPU process launch failed: error_code=1002
（同样的一行共 6 次）
FATAL:content/browser/gpu/gpu_data_manager_impl_private.cc:389] GPU process isn't usable. Goodbye.
```

Puppeteer、Playwright 也因此连不上它。

## 原因

- 用 strace 看，两个 zygote 进程都是被 SIGTRAP 结束的。死前的最后两个 syscall 是打开 `/proc/`，再对它 `fstatat(fd, "self/task/")`。
- 这是 Chromium 沙箱代码里的 `ThreadHelpers::IsSingleThreaded`：zygote fork 之前要确认自己只有一个线程，办法是看 `/proc/self/task` 的链接数。Linux 的 `proc_task_getattr` 在 2 上加线程数，所以单线程时是 3。代码里 `CHECK_LE(3UL, task_stat.st_nlink)`，arm64 上 `CHECK` 失败是 `brk #0`，也就是 SIGTRAP。
- tuxport 的合成目录的链接数都是 2，zygote 没有了，浏览器起不了 GPU 进程。

## 做法

`/proc/<pid>/task` 的链接数是 2 加线程组里的线程数，与 Linux 相同（`core/kernel/src/pseudo.rs`）。别的合成目录仍是 2。

## 结果

修好之后 Chromium 的负载都正常，见[第 92 轮](92-debuggers-valgrind-chromium.md)的表。

## 测试

- cargo test 新增 `procfs_task_links_count_threads`：用 busybox 的 stat，单线程时是 3。
- 差分测试 compat 组新增一段 python：不同线程数时 task 目录的链接数，包括经由 `/proc/thread-self/..` 到达的，与原生相同。

## 推迟项

- 别的合成目录的链接数仍是 2。Linux 上：
  - `/proc` 是 2 加子目录数和进程数（原生的容器里是 186）；
  - `/proc/<pid>` 按子目录数算；
  - sysfs 的目录是 2 加子目录数（`/sys` 是 13）；
  - sysctl 的目录是 1。

  这些都没看到有程序依赖。
