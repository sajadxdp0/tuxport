# M9 之前：平均负载、sysfs 和 limits（2026-10-27）

把推迟项里顺手能补的几个内核兼容性缺口补上（ptrace 另做）：

## 做法

- **平均负载**（`/proc/loadavg`、sysinfo 的 `loads`，M3b 起一直是 0）：与 Linux 的 `kernel/sched/loadavg.c` 一样，每 5 秒取一次可运行的线程数，按 1、5、15 分钟做指数衰减平均，定点数的系数和取整都与 Linux 相同（`loadavg.rs`）。可运行的是不在内核里等待的 guest 线程，等 BKL 的也算，相当于 Linux 的 R 加 D。
  - 取样由定时器线程做。所有线程都在等待时它停下来，不为平均负载每 5 秒醒一次：下一个要执行 guest 代码的线程叫醒它（与内存上限的检查共用一个标志，`Shared::resume_checks`），停下来期间错过的取样按 0 补上；读的时候定时器线程还没来得及补的，读的一方先补上。这相当于 Linux 的 NO_HZ 在空闲的 CPU 上做的事。
  - 两个忙循环跑一阵、再停下来，读到的 1 分钟平均值与按 Linux 的公式算出来的相同，停下来期间补上了 0。`uptime`、`top` 的负载不再总是 0。`sleep 24` 期间，定时器线程在 18 秒里只醒了一次（就是发现所有线程都在等待、停下来的那次取样）。
  - `/proc/loadavg` 的第四列是可运行的线程数和全部线程数（原来固定是 1），第五列是最近分配的 pid（原来是现存的最大 pid）。
- **`/proc/<pid>/limits`**（`task/<tid>/` 下也有）：格式与 Linux 的 `proc_pid_limits` 相同，包括列宽、`unlimited`，以及没有单位的行末尾留着的空格。
- **sysfs**（原来 `/sys` 只是仓库里的一个空目录）：只有 CPU 的拓扑，即 `/sys/devices/system/cpu` 下的 `online`、`possible`、`present`、`offline` 和各个 `cpu<n>`（`online`、`topology/*`、`regs/identification/midr_el1`、`revidr_el1`）。
  - CPU 都一样（与 `/proc/cpuinfo` 一致），各自是一个核，没有超线程，都在一个封装里。掩码和列表的写法与 Linux 的 `%*pb`、`%*pbl` 相同（4 个 CPU 时 cpu2 的 `core_cpus` 是 `4`，8 个时 cpu0 的是 `01`）。
  - busybox 的 `nproc --all` 数的就是这里的 `cpu<n>` 目录，原来一个也数不到，输出 1，现在与 `nproc` 相同。lscpu、glibc 的 `get_nprocs` 也读这里。
  - 文件与 Linux 一样报告 4096 字节、0 块，目录是 0755；statfs 是 `SYSFS_MAGIC`，设备号 0:22。`/proc/mounts`、`mountinfo`、`/proc/filesystems` 里有了它，fdinfo 的 mnt_id 能在 mountinfo 里找到（没有挂载点的 pipe、socket 从 4 改成 5）。
  - 没有 `/sys/kernel/mm/transparent_hugepage`，相当于没有编进 THP 的内核：tuxport 不支持大页，Go、jemalloc 读不到它就不会去用。
  - procfs 的 `net/`、`sys/` 这些固定的目录和 sysfs 共用一套按路径表查找、列目录的代码。合成文件系统里的节点的设备号都按它所在的文件系统给（epoll 的 fdinfo 原来把 `/dev/pts` 目录也报成 procfs 的）。
- **删除之后仍然打开着的文件的 `futimens`**（[文件操作的宿主开销](33-file-ops-host-cost.md)那一轮的推迟项）：普通文件的时间记在宿主 object 上，原来按 inode 编号拼出 object 的名字再设置，object 删掉之后是 ENOENT。按 fd 设置时（futimens，`AT_EMPTY_PATH`）改为直接设置打开着的 object。

## 测试

单元测试测平均负载的取样和停下来之后的补 0、恢复。`rootfs_programs` 新增 `nproc --all`、CPU 拓扑的文件和 `/proc/self/limits`。差分测试 `compat` 新增 sysfs 的内容和挂载信息、`/proc/loadavg` 的格式、`/proc/self/limits`、删除之后仍然打开着的文件的 futimens。`/proc/loadavg` 的数值取决于宿主，possible、present 在有的宿主上比 online 多，都不比。

## 推迟项

- sysfs 只有 CPU 的拓扑：没有 `cpufreq`（Node.js 的 `os.cpus()` 的频率是 0）、`cache/`、`/sys/class`、`/sys/fs/cgroup`。`cpu<n>/online` 是只读的（Linux 上 root 可以写它让 CPU 下线）。→ `cache/`、`cpufreq/` 已补上，照搬宿主对应的 CPU 的（M9 之前：mremap 和 sysfs）；`/sys/fs/cgroup` 已补上，是只读的 cgroup v2（M9 之前：cgroup v2 的视图）；`/sys/class` 仍没有，`online` 仍是只读的；`/sys/class/net` 已补上（M9 之前：网络接口的流量计数），`/sys/class` 下别的仍没有
