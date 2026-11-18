# M9 之前：/proc/meminfo 照搬宿主的（2026-10-28）

[负载普查](62-workload-survey.md)的问题 6：free 显示可用内存 650 MB（原生 3.4 GB），htop 的内存条几乎是满的。

## 原因

guest 的 `/proc/meminfo` 和 sysinfo 由宿主的 sysinfo 算，它只给得出总量和空闲量：`MemAvailable` 就是 `MemFree`，`Buffers`、`Cached` 都是 0，宿主的页缓存全算作已用。`AnonPages` 是总量减空闲量。按 `MemAvailable` 判断的程序（libuv 的 `uv_get_available_memory`、psutil）读到的同样偏小。

## 做法

- hal 的 `host_meminfo` 读宿主的 `/proc/meminfo`，guest 的 `/proc/meminfo` 照搬那 16 项，与 docker 的容器看到的一样；guest 能用的上限另由 cgroup 的 `memory.max` 告诉程序。读不了宿主的文件时退回 sysinfo，可用量按空闲加缓冲区估计。
- guest 的 sysinfo 也照搬：除了总量和空闲量，`sharedram`（Shmem）、`bufferram`（Buffers）、`totalswap`、`freeswap` 原来都是 0。

## 测试

- 差分测试 `compat` 新增：`MemTotal` 与原生相同（两边是同一台宿主），别的项只比大小关系、不是 0，sysinfo 与 `/proc/meminfo` 一致。
- free 和普查的 `tui` 里 htop 显示的内存与原生的 `/proc/meminfo` 一致。

## 推迟项

- 没有在 Android 上确认 App 能读宿主的 `/proc/meminfo`（读不了时退回 sysinfo，可用量就又偏小了），仍待真机。
- 只照搬了这 16 项；Linux 的 `/proc/meminfo` 还有四十多项（`Committed_AS`、`VmallocTotal`、`HugePages_*` 等），程序要读的话仍然没有。
