# M9 之前：cgroup 树：建子 cgroup、迁移进程（2026-11-01）

[podman 和 docker 的普查](157-container-survey.md)的问题 9 的第一步：dockerd 起容器时先 `mkdir /sys/fs/cgroup/docker`，tuxport 的 cgroup v2 是只读的（[cgroup v2 的视图](58-cgroup.md)），回 `read-only file system`。podman 不加 `--cgroups disabled`、`podman build` 不加 runtime 参数时也要建子 cgroup、写 `cgroup.procs`。普查时原生的 docker 嵌套在容器里、启用不了控制器也能跑，所以分两步：这一轮只做层级和迁移，限制先按 Linux 的格式检查、记下，下一步再让 `pids.max`、`memory.max`、`cgroup.freeze` 真正起作用。照 Linux 6.1 的 `kernel/cgroup/cgroup.c`。

## 做法

- **cgroup 树**（`cgroup.rs`）：`Kernel::cgroups` 记着所有的 cgroup（名字、上级、子 cgroup、`subtree_control`、属主、写过的接口文件的值），编号不重复使用，根是 1（也是它的 inode 号）。进程（`ThreadGroup::cgroup`）记着所在的 cgroup，fork 时随父进程。
- **目录**（`pseudo.rs` 的 `PseudoNode::Cgroup(编号, 文件名)`，空的文件名是目录本身）：列出的是这个 cgroup 有的接口文件和子 cgroup。核心文件（`cgroup.*`、`cpu.stat`）每个都有，控制器的文件按父 cgroup 的 `subtree_control`（根的控制器都开着，与原来一样）。属主是建立它的进程的 fsuid、fsgid（Linux 的 `cgroup_kn_set_ugid`），文件的权限位与 Linux 相同，`cgroup.kill` 是 0200。
  - mkdir 建子 cgroup：重名（包括这个 cgroup 有的接口文件）是 EEXIST，名字里有换行是 EINVAL，超出某个祖先的 `cgroup.max.depth`、`cgroup.max.descendants` 是 EAGAIN。rmdir：里面还有进程、还有子 cgroup 是 EBUSY，根删不了。
  - 不能建文件（EACCES）、删接口文件（EPERM）、mknod（EPERM）；rmdir 文件是 ENOTDIR，unlink 目录是 EISDIR。
  - 打开时只按权限位查：root 能以写方式打开只读的文件，写的时候才是 EINVAL；`cgroup.kill` 能读方式打开，读是 EINVAL（`cgroup_seqfile_show` 没有读的回调）。原来打开时就算好内容，只能写的文件改成不算。
- **迁移**：写 `cgroup.procs`（整个进程）、`cgroup.threads`（线程，Linux 的 `__cgroup_procs_write`）。0 是自己，编号按写的进程所在的 pid 命名空间；不是数字、负数是 EINVAL，找不到是 ESRCH；原来的和目的地要都在写的进程的 cgroup 命名空间的根下面（ENOENT）；目的地不是根、又开了控制器时不能放进程（EBUSY，"没有内部进程"）；domain cgroup 之间单独迁移线程是 EOPNOTSUPP。
- **clone3 的 `CLONE_INTO_CGROUP`**（原来是 EINVAL）：`cgroup` 字段是 cgroup2 目录的 fd（O_PATH 的也行），不是的是 EBADF，结构体不到这一项是 EINVAL；目的地与迁移一样检查，已经删掉的是 ENODEV（`cgroup_css_set_fork`）。
- **控制器**：`cgroup.controllers` 是父 cgroup 的 `subtree_control`；写 `subtree_control` 是 `+名字`、`-名字`，不认识的、没有正负号的是 EINVAL，要开的不在 `cgroup.controllers` 里是 ENOENT，不是根、里面有进程时开不了（EBUSY），子 cgroup 还开着的关不了（EBUSY）。
- **接口文件的内容**：`cgroup.procs`、`cgroup.threads` 列出里面的进程、线程，`cgroup.events` 的 populated 看子树里有没有活着的进程，`cgroup.stat` 数子孙，子 cgroup 的 `memory.current`、`memory.stat` 是子树里的进程的驻留内存，`pids.current` 数线程，`cpu.stat` 加起来。根的照旧是整个 App 的（OOM killer 量的）。
- **限制**（`memory.max`、`pids.max`、`cpu.max`、`cpu.weight`、`cpuset.cpus`……）：按 Linux 的格式检查（memparse 的后缀、按页向下取整，`cpu.max` 的周期范围，`cpu.weight` 越界是 ERANGE 这些），记下来、读出来是写进去的值，还不起作用。`cgroup.max.depth`、`cgroup.max.descendants` 起作用。
- **`cgroup.kill`**：写 1 给子树里的进程都发 SIGKILL；不是整数是 EINVAL，别的数是 ERANGE。`cgroup.freeze` 写 0 成功，写 1 还是 EOPNOTSUPP（下一步），别的是 ERANGE。`cgroup.type` 写 `threaded` 是 EOPNOTSUPP（没有 threaded cgroup），别的是 EINVAL。
- **命名空间的根**（nsdelegate）：Linux 在不是开机时的 cgroup 命名空间的根上，只让写 `cgroup.procs`、`cgroup.threads`、`cgroup.subtree_control`，别的是 EPERM（`cgroup_file_write`）。tuxport 的根看起来是容器的根，也照这条，写它的 `memory.max` 这些不会改掉 OOM killer 的上限；但它是真正的根，有进程时也能开控制器（dockerd 直接在根上写 `+cpu +memory`）。
- **cgroup 命名空间**：原来只有编号和属主，现在以建立它的进程所在的 cgroup 为根。`/proc/<pid>/cgroup` 是相对读者的命名空间的根的路径，不在下面时像 `kernfs_path_from_node` 那样先用 `..` 回到共同的祖先；新挂的 cgroup2 的根是挂的进程的命名空间的根。
- **挂载**：开机时挂在 `/sys/fs/cgroup` 的改成可写的（与特权容器一样），只读的挂载里照旧是 EROFS（`Vfs::readonly` 只看挂载的标志了）。
- **`oom_score_adj`、`coredump_filter` 的写**：建好 cgroup 之后 runc 的下一步是写 `/proc/self/oom_score_adj`，值结尾带着 NUL（netlink 消息里的字符串，长度也算上它），tuxport 当作不是数字回 EINVAL。改成照 Linux：拷进 12 个字节的缓冲区（`PROC_NUMBUF`，多出来的不收，write 返回 12）、到 NUL 为止、去掉两头的空白，按 `kstrtoint` 的基数 0 解析（`0x`、八进制、正负号），超出 32 位是 ERANGE。

## 测试

- 新的差分测试组 `cgroup-tree`（原生一侧是特权容器，cgroup 可写；只在根下面建的 `t` 里比）：建删子 cgroup、属主和权限、各个文件在不在、控制器、迁移之后的 `/proc/<pid>/cgroup` 和接口文件、命名空间的根上的 EPERM、各种写错的值、深度和个数的上限、clone3 的 `CLONE_INTO_CGROUP`、`cgroup.kill`、`unshare -C` 之后看到的路径、nobody 的权限。
- `cgroup` 组的原生一侧也改成特权容器（挂载可写），原来比只读的那一段换成在根上写只读的文件、建删子 cgroup 这些；根的 `cgroup.freeze`、`cgroup.kill` 不写，原生上会冻住、结束整个容器；chmod 去掉了（见推迟项）。JVM 21.0.12 在 cgroup 挂载可写时只在有 CPU 或内存的限制时才当作在容器里，tuxport 的 `memory.max` 是 OOM killer 的上限、原生一侧通常是 max，改成比"有限制时才当作在容器里"、在容器里时没有 CPU 配额。
- `containers` 组新增 `oom_score_adj`、`coredump_filter` 各种写法的解析（NUL、空白、进制、范围、只收 12 个字节）。原生上写 `oom_score_adj` 负数要 `CAP_SYS_RESOURCE`，docker 默认不给，用例只写正数。
- 两个执行层下都与原生逐字相同。写用例时撞到的：Linux 上 root 能以写方式打开只读的 cgroup 文件，写才是 EINVAL；`wait` 一个被 `cgroup.kill` 结束的后台进程时，ash 打不打印 `Killed` 是时间上的竞争，用例去掉了 `wait` 的 stderr。
- `cgroup.rs` 的单元测试：建子 cgroup、文件按 subtree_control、相对命名空间的根的路径、深度和个数的上限、接口文件的值的格式；`pseudo.rs`：写数字的 /proc 文件的解析。
- LTP 新通过 clone303（clone3 的 `CLONE_INTO_CGROUP`，原来是 CONF）。
- 在 tuxport 里实际跑：`docker run` 建好了 `/sys/fs/cgroup/docker/<容器>`、写过了 `oom_score_adj`，下一步停在 runc 给 cgroup 挂设备过滤的 eBPF 程序（`bpf_prog_query(BPF_CGROUP_DEVICE)` 是 ENOSYS）。

## 推迟项

- 限制不起作用（`pids.max`、`memory.max`、`memory.high`、`cpu.max`、`cpuset.cpus` 这些只记下），`cgroup.freeze` 写 1 是 EOPNOTSUPP：下一步（普查的问题 9 的第二步）。子 cgroup 的 `memory.events`、`pids.events` 总是 0，`memory.peak` 就是当时的驻留内存。→ `pids.max`、`memory.max` 起作用了，事件和峰值也有了（M9 之前：cgroup 的 pids.max 和 memory.max 起作用）；`cgroup.freeze` 也有了（M9 之前：cgroup.freeze）；`memory.high`、`cpu.max`、`cpuset.cpus` 这些仍只记下
- 没有 threaded cgroup（`cgroup.type` 写 `threaded` 是 EOPNOTSUPP），线程总在进程所在的 cgroup 里。
- cgroup 的目录、文件不能 chmod、chown：Linux 上可以，systemd、无 root 的 podman 靠它把一棵子树委派给普通用户。委派的另外几条检查（写 `cgroup.procs` 时对共同祖先的 `cgroup.procs` 也要有写权限，`cgroup_procs_write_permission`）也没有，普通用户的迁移只看目的地的文件权限。
- `cgroup.events`、`memory.events` 内容变了不通知（inotify 的 IN_MODIFY、poll 的 POLLPRI，Linux 的 `kernfs_notify`）：systemd 靠它知道子树空了，containerd 靠它看 OOM。
- 没有 PSI（`cpu.pressure` 这些）、`io.*`、`hugetlb.*`，与原来一样（[cgroup v2 的视图](58-cgroup.md)）。
- 只读的挂载里照旧是 EROFS，但超级块的标志不分开记（[mount 的子集](105-mount-subset.md)的推迟项）：cgroup2 只有一个实例，Linux 上不带 bind 地重新挂成只读会让每一处都只读。
