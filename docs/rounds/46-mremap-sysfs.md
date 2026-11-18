# M9 之前：mremap 和 sysfs（2026-10-27）

内核兼容性里剩下的两个顺手的缺口：mremap 不能扩展私有文件映射（[M6](08-m6.md) 起），sysfs 只有 CPU 的拓扑、没有缓存和频率（[平均负载、sysfs 和 limits](39-loadavg-sysfs.md)）。

## 做法

- **mremap 扩展私有文件映射**：原来返回 ENOMEM。Linux 扩展出来的部分接着映射同一个文件后面的内容，私有地写不影响文件，文件末尾之后的页访问是 SIGBUS。私有文件映射的 VMA 不像共享映射那样留着宿主 fd，所以系统调用这一层先按 VMA 记的仓库 inode 打开文件（`Mm::private_file_ino`、`Store::open_object`，与 `MREMAP_DONTUNMAP` 补回原地址的做法相同），交给 `Mm::mremap`，扩展的部分私有地映射文件的那一段，有文件内容的长度照旧按文件大小算（之后的页不进页表，访问是 SIGBUS）。不知道 inode 的（比如 memfd 的私有映射）仍然 ENOMEM。
- **sysfs 的 `cache/`、`cpufreq/`**：各个 `cpu<n>` 下照搬宿主对应的那个 CPU（`host_cpus` 的第 n 个）的 `cache/index*/`（level、type、size、coherency_line_size、ways_of_associativity、number_of_sets、shared_cpu_list、shared_cpu_map、id 等）和 `cpufreq/`（cpuinfo_*_freq、scaling_*、affected_cpus、related_cpus 等）。名单是固定的（`HOST_CPU_FILES`），宿主有、App 读得了的才出现（Android 的 SELinux 可能不让 App 读 cpufreq，这时与没有 cpufreq 驱动的内核一样），内容现读；CPU 列表、掩码和 affected_cpus、related_cpus 里宿主的 CPU 编号换成 guest 的，guest 用不了的 CPU 去掉。lscpu、hwloc、OpenBLAS 读缓存的层级和大小，Node.js 的 `os.cpus()` 读 `cpufreq/scaling_cur_freq`（宿主让 App 读 cpufreq 时就不再是 0；没有真机，这一点还没有验证）。本地 colima 的虚拟机有 `cache/`（三级，只有 level、type 和共用的 CPU），没有 cpufreq，tuxport 与原生读到的完全相同。

## 测试

差分测试 `compat` 新增 mremap 扩展只读、可写的私有文件映射（文件后面的内容、私有地写、文件末尾之后 SIGBUS），以及各个 CPU 的 `cache/index*` 和有没有 `cpufreq/`。单元测试新增 CPU 列表、掩码的解析和格式化。

## 推迟项

- `/sys/class`、`/sys/fs/cgroup` 仍没有，`cpu<n>/online` 仍是只读的。→ `/sys/fs/cgroup` 已补上，是只读的 cgroup v2（M9 之前：cgroup v2 的视图）；`/sys/class` 仍没有，`online` 仍是只读的；`/sys/class/net` 已补上（M9 之前：网络接口的流量计数），`/sys/class` 下别的仍没有
- cpufreq 的文件不能写（Linux 上 root 可以改 `scaling_governor` 等），App 也改不了宿主的。
