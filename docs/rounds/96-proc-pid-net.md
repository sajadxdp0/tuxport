# M9 之前：`/proc/<pid>/net`（2026-10-29）

[第 92 轮](92-debuggers-valgrind-chromium.md)对照目录的链接数时顺带看到的推迟项。Linux 上网络的那些文件按网络命名空间分，放在 `/proc/<pid>/net` 下（task 子目录里也有），`/proc/net` 只是指向 `self/net` 的链接。tuxport 的 `/proc/net` 是个普通目录，没有 `/proc/<pid>/net`，所以：

- 经由 `/proc/self/net/...`、`/proc/<pid>/net/...` 读的程序得到 ENOENT；
- `ls -l /proc`、`readlink /proc/net` 与 Linux 不同；
- 打开 `/proc/net/dev` 之后，fd 链接显示 `/proc/net/dev`，Linux 是 `/proc/<pid>/net/dev`。

## 做法

改在 `core/kernel/src/pseudo.rs`：

- `/proc/<pid>/net` 和 `/proc/<pid>/task/<tid>/net` 是目录，里面是原来 `/proc/net` 下的 11 个文件。内容与原来相同：所有进程都在同一个网络命名空间里，看到的一样。
- `/proc/net` 与 `/proc/mounts` 一样，是指向 `self/net` 的符号链接。
- 属主和权限与原生相同：目录属于进程的用户，文件属于 root、0444，别的用户的进程也能读。

## 结果

经由 `/proc/net` 打开的文件多跟随一次链接。用 `target/mem/procbench.c` 读 `/proc/net/dev` 2 万次，0.471–0.477 → 0.477–0.487 秒，每次多约 0.5 µs；时间主要花在向宿主取接口上。

## 测试

- cargo test 新增 `procfs_net_is_under_each_process`。
- 差分测试 compat 组新增一段：`/proc/net` 的链接，`/proc/self/net` 和 task 子目录里的文件，fd 链接，权限、属主，与原生逐行相同。

## 推迟项

- `/proc/<pid>/net` 仍然只有这 11 个文件（原生的容器里有 57 个），没有的见[网络信息的缺口](78-net-info-gaps.md)那一轮。→ 新增 `route`、`ipv6_route`、`sockstat`、`sockstat6`（M9 之前：路由表），现在 15 个；新增 `snmp`、`netstat`、`arp`（M9 之前：snmp 和输入结束时的 EOF），现在 18 个，`snmp6` 等仍没有
