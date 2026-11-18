# M9 之前：cgroup 的 memory.stat（2026-10-28）

[负载普查](62-workload-survey.md)的问题 5：另一个 guest 进程占着 400 MB 时，.NET 的 GC 报告的内存负载仍是 39 MiB，`memory.current` 却已经 543 MB。几个进程一起把内存用到 OOM killer 的上限时，.NET 不会提前收紧堆，结果被结束的可能是它。

## 原因

.NET 在 cgroup v2 下的用量是 `memory.current` 减去 `memory.stat` 里的 `inactive_file`（与 docker、kubernetes 算法相同），读不到 `memory.stat` 就退回 `/proc/self/statm` 里自己的 RSS。[cgroup v2 的视图](58-cgroup.md)那一轮没有做 `memory.stat`。（.NET 的这段逻辑是按记忆写的，没有对照源码；实测的行为与它一致：补上之后负载随别的进程变化。）

## 做法

- `memory.stat` 按 Linux 6.1 的 `memory_stat_format` 输出，取有 swap 和透明大页、没有 zswap 的配置（Android 的 GKI 用 zram，不用 zswap），单位是字节，字段和顺序对照了 6.1 的 `mm/memcontrol.c`。
- 量得到的只有宿主进程的匿名内存，与 `memory.current` 相同：私有的（RssAnon）和换出到 zram 的（VmSwap）算 `anon`，共享的（RssShmem）算 `shmem`，Linux 的 `file` 包括 shmem，所以 `file` 等于它；这些都在 `active_anon` 里，没有可以回收的页缓存，`inactive_file`、`active_file` 是 0。所以 `anon + file` 就是 `memory.current`，.NET 算出的用量与它相同。别的计数（内核栈、页表、slab、缺页次数……）是 0。
- hal 的 `memory_footprint` 拆成分项的 `memory_usage`，OOM killer 量的总数不变。

## 测试

- 差分测试 `cgroup` 新增 `memory.stat`：权限、字段的顺序、每行的格式；新内核加的和取决于内核配置（swap、透明大页、zswap）的字段不比。
- 普查的 `dotnet`：内存负载随别的进程变化了，原来总是 39 MiB。

## 推迟项

- `memory.stat` 只分得出匿名内存和共享内存，别的计数是 0；宿主的页缓存不算在 App 里（与 OOM killer 量的一致）。
- 仍没有 memory.numa_stat、memory.reclaim、io.*、压力（PSI）等文件（[cgroup v2 的视图](58-cgroup.md)的推迟项）。
