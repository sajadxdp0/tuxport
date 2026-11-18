# M9 之前：可写地挂 cgroup2（2026-11-01）

[podman 和 docker 的普查](157-container-survey.md)的问题 6：crun 给容器挂 `/sys/fs/cgroup` 时不带只读（podman 的规范里写的是 `rw`，之后按选项再 bind 地重新挂），tuxport 对不带只读的 cgroup2 挂载回 EROFS（[mount 的子集](105-mount-subset.md)："cgroup2 只能只读"），每个容器都起不来（`--cgroups disabled` 时也挂）。

## 做法

- new_mount、remount 不再拒绝不带只读的 cgroup2。挂载的标志照给的记，mountinfo、statfs 的 `f_flags` 是 `rw`。
- cgroup2 里的文件照旧改不了、建不了子 cgroup（EROFS，`Vfs::readonly` 不论挂载的标志），相当于一个只读的超级块。写 `cgroup.procs`、`memory.max` 这些要等可写的 cgroup（普查的问题 9）。
- 开机时挂在 `/sys/fs/cgroup` 的那个仍是只读的，与 docker 默认给容器的一样。

## 测试

- 差分测试 `containers` 组新增：不带只读地挂 cgroup2，mountinfo 和 statvfs 是可写的，里面有 `cgroup.procs`；bind 地重新挂成只读、再挂回可写。写文件、建子 cgroup 在原生上真会改，不比；不带 bind 的 remount 改的是超级块，原生上会改到 docker 给容器的 cgroup，也不做。
- 两个执行层下都与原生逐字相同。busybox 的 mount 得到 EROFS 时会改成只读再挂，只看挂没挂上查不出来，要看 mountinfo 和 statvfs。

## 推迟项

- 可写的挂载里写文件是 EROFS：Linux 上可写的挂载里写 cgroup 文件、建目录照常成功（权限够的话）。等可写的 cgroup。→ 已改（M9 之前：cgroup 树：建子 cgroup、迁移进程）
