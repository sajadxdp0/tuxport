# M9：39 位的宿主地址空间（2026-11-03）

[页表按需建立](179-demand-page-tables.md)那一轮只为 ASan 记了一笔："不少 Android 设备的用户地址空间只有 39 位（512 GB），tuxport 每个 guest 映射对应一段同样大的宿主映射。"筛推迟项时想到这不只关系到 ASan：所有 guest 进程都在 App 这一个宿主进程里，它们的映射加在一起要放进 512 GB，而原生的 39 位系统上每个进程各有 512 GB。这一轮先量，再修。

## 宿主是不是 39 位

模拟器（Pixel 4a、API 34）的内核是 GKI 的 `6.1.23-android14-4`，`/proc/config.gz` 里是 `CONFIG_ARM64_4K_PAGES=y`、`CONFIG_ARM64_VA_BITS=39`，栈在 `0x7fdff1b000` 附近。4K 页的 GKI 设备用的是同一份配置，所以大多数手机都是 39 位。

## 量

在 guest 里起各个运行时，看 `/proc/<pid>/status` 的 VmSize（在宿主上是同样大的映射）：

| 程序 | VmSize |
|---|---|
| `python3 -c 'time.sleep(20)'` | 10 MB |
| `node -e 'setTimeout(…)'` | 0.78 GB |
| 同上，再建一块 `WebAssembly.Memory` | 9.2 GB |
| `java Sleep`（8 GB 内存的虚拟机） | 2.7 GB |
| Go 编译的程序 | 1.2 GB |
| headless Chromium（Debian 13 的 `chromium`，`about:blank`），11 个进程 | 合计 4.7 TB |

Chromium 每个进程（浏览器、两个 zygote、两个 crashpad、GPU、两个 utility）各约 52 GB 的 `PROT_NONE`（PartitionAlloc 的池：4 GB 加三个 15 GB），渲染器各 1447 GB（再加 V8 的沙箱，1 TB 和 64 GB 两块）；真正可以访问的每个进程只有 0.3–0.8 GB。别的运行时 512 GB 里放得下几十个。

在 docker 里用 `ulimit -v 512G` 把 tuxport-run 限制在 512 GB 模拟 39 位的宿主：Chromium `--dump-dom` 一直不返回（等了 10 分钟），stderr 里是 "Zygote could not fork: process_type renderer … child_pid -1"：前面几个进程已经占了四百多 GB，fork 渲染器时宿主映射不出来，浏览器进程之后一直等着。`--no-zygote` 一样超时；`--single-process` 不限制时也以 SIGTRAP 结束（headless 不支持），绕不过去。

## 做法

宿主内存总是可读写，guest 的权限由页表管；私有匿名内存里有内容的页一定有表项（[页表按需建立](179-demand-page-tables.md)的规则）。所以从建立起一直是 `PROT_NONE` 的私有匿名映射既没有内容、也没有表项，用不着宿主内存。这样的映射现在只是预留（`HostRegion::reserved`）：

- **哪些**：mmap 的私有匿名 `PROT_NONE`（`MAP_GROWSDOWN` 除外，它要在宿主内存里往下长），mremap 扩展这样的映射接上的部分，`MREMAP_DONTUNMAP` 在原地址补的 `PROT_NONE` 空映射。共享映射、文件映射、可以访问过的不变。
- **什么时候有宿主内存**（`Mm::back_reserved`）：mprotect 变得可以访问时，只给拆出来的那一段申请（全零，计不计入提交量照 mmap 的规则）；内核往里写（PTRACE_POKEDATA、`/proc/<pid>/mem`）时只给写到的那几页。之后与普通的私有匿名映射一样；再变回 `PROT_NONE` 也留着（可能有内容）。PartitionAlloc、V8 用 mprotect 提交，JVM、Go 用 `MAP_FIXED` 的 mmap 盖上去（原来就是新的宿主内存），都只占用到的部分。
- **别的操作**：fork 时子进程也只是预留；mincore、pagemap、smaps 的驻留都是 0；`MADV_DONTNEED` 什么也不做；core dump 本来就不写没写过的匿名映射。
- 预留的 VMA 没有真的宿主地址，记的是 `0x00f0_0000_0000_0000` 起按 guest 偏移算出来的，只在拆开 VMA 时按整数算，从不访问；第 55 位是 1，落在宿主内核那一半（arm64 的 TBI 只忽略最高 8 位），万一被访问也只是宿主的段错误。
- 一段预留里先后提交的几块各有各的宿主内存，宿主地址不连续。Tier 1 的跨页访问在两页的宿主地址不连续时本来就退回回填例程，相邻的两次 mmap 原来也是这样。

## 测试

- 模拟 39 位（`ulimit -v 512G`）：Chromium `--dump-dom` 正常输出（`<title>js:42</title>`）。起着 headless Chromium 时 tuxport-run 在宿主上的 VmSize 从 4.83 TB 降到 12.4 GB，宿主的映射从 2019 个变成 2244 个（提交的块各是一段宿主映射）。node（带 wasm）、java、`go build`、python3 在限制下照常。
- 内核层（`cargo test`）新增 `prot_none_reservations_take_no_host_memory`：大的预留只有 mprotect 过、内核写过的页有宿主内存，mincore、pagemap、驻留量、丢弃、fork、mremap 原地扩展都照预留对待。
- 常用负载的速度都在波动范围内。
- 仪器测试在 Pixel 4a（API 34）的模拟器上通过，宿主就是 39 位。

## 推迟项

- 可写的大映射仍然一对一占宿主地址空间：ASan 的影子内存（可写、`MAP_NORESERVE`，十几 TB）在 39 位的设备上仍然映射不出来，TSan、MSan 也一样。要做得把宿主内存分块、用到时再申请（[页表按需建立](179-demand-page-tables.md)的推迟项）。
- 提交过又变回 `PROT_NONE` 的部分（PartitionAlloc 的 decommit）留着宿主映射，宿主地址空间按提交的峰值算。
- 每次提交是一段单独的宿主映射：Chromium 多了约 200 个。所有 guest 进程共用 App 进程的宿主映射数上限（`vm.max_map_count`，默认 65530），频繁小块提交的程序会更快碰到它，没有量。
- 只在虚拟机里用 `ulimit -v` 模拟：真机上 App 自己（ART）也占着一部分地址空间，能同时开几个 Chromium 没量；16K 页的设备是不是 39 位没查（R1）。第 92 轮的 Chromium 普查（Puppeteer、Playwright，要联网 npm install）没有在限制下重跑。
