# M9 之前：cgroup v2 的视图（2026-10-27）

运行时按物理内存定堆的大小：JVM 的默认最大堆是物理内存的 1/4，Node.js 按物理内存给 V8 定上限。但 guest 能用的只有 OOM killer 的上限，默认是物理内存的一半（§12 第 4 条），几个运行时一起用满时，就有进程被结束。它们在容器里改按 cgroup 的 `memory.max` 算：JVM 的容器支持（UseContainerSupport），Node.js 经由 libuv 的 `uv_get_constrained_memory`。原来 tuxport 没有 cgroup，JVM 报告"No metrics available"。

## 做法

- **一个只读的 cgroup v2**（`cgroup.rs`）：所有 guest 进程在同一个 cgroup 里，看起来与 docker 用 cgroup 命名空间的容器一样（§8.2）。
  - `/proc/<pid>/cgroup` 是 `0::/`。`/proc/cgroups` 列出 cpuset、cpu、cpuacct、memory、pids，层级都是 0，JVM 据此认出 cgroup v2。
  - cgroup2 只读地挂在 `/sys/fs/cgroup`，mountinfo、`/proc/mounts` 的那一行与 docker 的相同（选项 `nsdelegate,memory_recursiveprot`），statfs 是 `CGROUP2_SUPER_MAGIC`。`/proc/filesystems` 加上 cgroup2。
  - 写文件、建文件和子 cgroup、删除、改权限都是 EROFS，与 docker 默认的挂载一样。
- **文件**（37 个，权限与 Linux 相同）：
  - cgroup.*：controllers 是 `cpuset cpu memory pids`，procs、threads 列出 guest 的进程和线程，type 是 domain，events、freeze、max.depth、max.descendants、stat、subtree_control 是没有子 cgroup 时的值。
  - memory.*：
    - `memory.max` 是 OOM killer 的上限（不限制时是 max）；
    - `memory.current` 是它量的 App 的内存，`memory.peak` 是量到过的最大值；
    - 换出到 zram 的内存已经算在 current 里，所以 `memory.swap.max` 是 0；
    - `memory.events` 的 max、oom、oom_kill 是超出上限、因此要挑进程、结束了进程的次数（OOM killer 新记的计数）；
    - min、low、high、oom.group、swap.current、swap.high、swap.events、events.local 是不限制时的值。
  - cpu.*：没有配额和权重，`cpu.max` 是 `max 100000`，weight 100；`cpu.stat` 是所有 guest 线程用掉的 CPU 时间，按 6.1 的格式。
  - cpuset.*：effective 是 guest 看到的所有 CPU 和内存节点 0，cpus、mems 是空的，partition 是 member。
  - pids.*：current 是 guest 的线程数，max 不限（线程数的限制在 threads-max 和 `RLIMIT_NPROC`），events 是 `max 0`。

## 结果

colima 的虚拟机（3.8 GiB）上，默认上限 1.91 GiB。

| | 之前 | 之后 |
|---|---|---|
| JVM 的默认最大堆 | 978 MiB | 490 MiB |
| Node.js 的 V8 堆上限 | 2.09 GiB | 1.05 GiB |

之前 Node.js 的堆上限比 guest 能用的还大。`java -XshowSettings:system` 报告 `Provider: cgroupv2`，Memory Limit 是上限，`process.constrainedMemory()` 也是。

## 测试

新的差分测试 `cgroup`：`/proc/<pid>/cgroup`、`/proc/cgroups`、挂载的那一行和 statfs，各个文件的权限和内容，写、建、删、改权限都是 EROFS，JVM、Node.js 看到的上限与 `memory.max` 一致。CI 的宿主内核比 6.1 新、版本不一，只比 6.1 就有的部分（`/proc/cgroups` 列出的控制器，cgroup.stat、memory.events、cpu.stat 的项，6.12 起可写的 `memory.peak`）；取决于宿主的文件只比权限和格式；原生一侧 docker 没有设 `--memory`，`memory.max` 是 max。

rootfs 测试新增 `cgroup_memory_files_follow_the_oom_killer`：`memory.max` 跟着 `--memory-limit`，吃内存被结束之后 `memory.events`、`memory.peak` 跟着变。差分测试只比自洽，`memory.max` 没接到上限查不出，要靠它。

## 推迟项

- 只有这 37 个文件：没有 memory.stat、memory.numa_stat、memory.reclaim、io.*、hugetlb.*、rdma.*、misc.*、cgroup.kill 和压力（PSI）的文件，`/proc/cgroups` 只列这五个控制器。→ memory.stat 已补上（M9 之前：cgroup 的 memory.stat）；cgroup.kill 也补上了（M9 之前：cgroup 树：建子 cgroup、迁移进程）；memory.numa_stat、memory.reclaim、io.*、hugetlb.*、rdma.*、misc.* 和压力（PSI）的文件仍没有，`/proc/cgroups` 仍只列这五个控制器
- 只读，不能建子 cgroup、迁移进程、设置限制：要按 cgroup 管资源的程序（systemd、容器运行时）用不了，它们本来也在 D 档（§1.2）。→ 能建子 cgroup、迁移进程了，开机时的挂载也改成可写的（M9 之前：cgroup 树：建子 cgroup、迁移进程）；限制只记下，还不起作用
- `memory.peak` 只是 OOM killer 量的时候（有线程运行时每 20 毫秒）和读 memory.current、memory.peak 时取到的最大值，两次之间的峰值看不到。
