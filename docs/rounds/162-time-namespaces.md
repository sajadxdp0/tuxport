# M9 之前：time 命名空间（2026-11-01）

[podman 和 docker 的普查](157-container-survey.md)的问题 7：tuxport 有 `/proc/<pid>/ns/time`，dockerd 看到它就在容器的规范里加上 time 命名空间；runc 再检查 `/proc/self/timens_offsets`，没有就报 `time namespaces aren't enabled in the kernel`。原来 `CLONE_NEWTIME` 是 EPERM，又没有 `timens_offsets`，与哪种 Linux 都对不上：配了 `CONFIG_TIME_NS` 的都有，没配的连 `ns/time` 也没有。

两条路：去掉 `ns/time`，或者做 time 命名空间。tuxport 的时钟（连同 vDSO 的存根）都经过内核层，加偏移不难，命名空间也只剩这一种，就做了。照 Linux 6.1 的 `kernel/time/namespace.c`，加上 6.2 起放宽的几处（见下）。

## 做法

- **命名空间对象**（`nsproxy.rs` 的 `TimeNs`）：编号、属主（用户命名空间）、两个偏移（CLOCK_MONOTONIC 的和 CLOCK_BOOTTIME 的，各是秒和纳秒）、偏移是否已经冻结。线程的 `NsProxy` 加上 `time` 和 `time_for_children`，`NsRef::Time` 带着对象，`/proc/<pid>/ns/time`、`time_for_children` 是它们的编号，经由命名空间文件、pidfd 都能找到、setns 进去。
- **建立**：unshare(`CLONE_NEWTIME`) 要 `CAP_SYS_ADMIN`，只建给子进程用的那个（自己不进去），偏移照抄原来给子进程用的那个（Linux 的 `clone_time_ns`），属主是（一起 unshare 了用户命名空间的话是新的）用户命名空间。clone3 也可以带 `CLONE_NEWTIME`（clone 的这一位是退出信号）。
- **进入**：不共用地址空间的子进程进给子进程用的那个，偏移从此冻结（Linux 的 `timens_on_fork`）。setns 进来的线程两个都换成它，冻结；多线程的进程是 EUSERS，要在属主和自己的用户命名空间里有 `CAP_SYS_ADMIN`。
- **6.2 起的放宽**：Linux 6.1 在 unshare 了 `CLONE_NEWTIME`、还没进去的进程里建线程、vfork 是 EINVAL；6.2 起它们留在父线程的命名空间里，execve 换了地址空间时才进新的（`exec_task_namespaces`）；clone3 带 `CLONE_NEWTIME` 在 6.1 是 EINVAL，之后可以。原生对照的 6.8、CI 的更新的内核都是放宽之后的，放宽之后能跑的程序也更多，照它们做，记在设计文档 §7.5 6.1 契约的例外里。
- **`/proc/<pid>/timens_offsets`**（`pseudo.rs`，只在 `/proc/<pid>` 下）：读到给子进程用的那个的偏移，格式与 Linux 相同；写一两行 `<时钟> <秒> <纳秒>`，时钟是 `monotonic`、`boottime` 或者编号 1、7。照 `timens_offsets_write`、`proc_timens_set_offset` 检查：格式不对、纳秒不小于 10⁹、别的时钟是 EINVAL，没有属主里的 `CAP_SYS_TIME` 是 EPERM，加上偏移之后的时钟不在 [0, KTIME_SEC_MAX / 2] 里是 ERANGE，已经冻结是 EACCES。
- **时钟**：
  - clock_gettime 的 CLOCK_MONOTONIC、MONOTONIC_RAW、MONOTONIC_COARSE 加单调时钟的偏移，CLOCK_BOOTTIME、BOOTTIME_ALARM 加 boottime 的；不持 BKL 的快速路径（vDSO 的存根走它）在 `Fast` 里记着这两个偏移，setns 换命名空间时让它作废。
  - 绝对时刻的等待先减去偏移（Linux 的 `timens_ktime_to_host`，早于偏移的当作已经过去）：clock_nanosleep（两条路径）、timerfd_settime、timer_settime 的 `TIMER_ABSTIME`，futex WAIT_BITSET（两条路径）和 futex_waitv 的单调时钟的超时。
  - `/proc/uptime`、进程的启动时间（`/proc/<pid>/stat` 第 22 项）按读者所在的命名空间加 boottime 的偏移，sysinfo 的 uptime 也是；`/proc/uptime` 的空闲时间、`/proc/stat` 的 btime 与 Linux 一样不变。
- 顺带补上 [mqueue 文件系统](160-mqueue-fs.md)漏掉的：`/proc/filesystems` 里列出 `mqueue`。

## 测试

- 差分测试 `containers` 组新增（原生一侧加 `--cap-add SYS_TIME`）：unshare 和偏移照抄、写 `timens_offsets` 的各种错误和权限、子进程里各种时钟和 `/proc/uptime`、sysinfo、启动时间相差偏移、绝对时刻的几种等待不多等、进去之后写偏移是 EACCES、线程和 vfork、setns，两个执行层下都与原生逐字相同。只比是否相差偏移、是否同一个命名空间，不比具体的时刻。
- `nsproxy.rs`、`misc.rs` 的单元测试：各个时钟的偏移、新建的照抄不冻结、读数加偏移、绝对时刻减偏移。
- 在 tuxport 里实际跑：`docker run` 过了 time 命名空间和 runc 的 memfd，下一步停在 `mkdir /sys/fs/cgroup/docker: read-only file system`（普查的问题 9）。

## 推迟项

- 写 `timens_offsets` 的能力按写的进程的凭据查，Linux 按打开文件时的（`file_ns_capable`），与 [用户命名空间](119-user-namespaces.md)写 ID 映射的推迟项一样。
- 没有 ucounts：`/proc/sys/user/max_time_namespaces` 与别的几种一样可以读写，但不实际限制个数（[用户命名空间](119-user-namespaces.md)的推迟项）。
- 内核日志（dmesg）的时间戳不按 time 命名空间，与 Linux 一样。
