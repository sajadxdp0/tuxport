# M9 之前：open_tree、move_mount 和 mount_setattr（2026-11-01）

[podman 和 docker 的普查](157-container-survey.md)的问题 10：容器有用户命名空间（`--uidmap`、`--userns`）时，crun 在命名空间外面用 `open_tree(OPEN_TREE_CLONE)` 打开要 bind 的东西，把 fd 交给里面的进程去 `move_mount`。tuxport 的新挂载接口都是 ENOSYS，crun 退回在用户命名空间里按路径 stat，`userdata/shm` 所在的目录是 0700、属于没有映射的 root，得到 EACCES，容器起不来。照 Linux 6.1 的 `fs/namespace.c` 做了 open_tree、move_mount、mount_setattr 三个。

open_tree 一旦能用，crun 所有的 bind 挂载（`get_bind_mount`：屏蔽的路径、`/sys`、卷……）都改走 open_tree、mount_setattr、move_mount，不只是 `--uidmap` 的容器，所以三个要一起做对。fsopen、fsconfig、fsmount、fspick 仍是 ENOSYS：crun、util-linux 的 mount 遇到 ENOSYS 都退回 mount(2)。

## 做法

- **open_tree**（`syscall/mount.rs`）：标志只能是 `AT_EMPTY_PATH`、`AT_NO_AUTOMOUNT`、`AT_RECURSIVE`、`AT_SYMLINK_NOFOLLOW`、`OPEN_TREE_CLONE`、`OPEN_TREE_CLOEXEC`，`AT_RECURSIVE` 要带 `CLONE`（EINVAL）。路径照常解析，越过挂载点到最上面的。
  - 不带 `CLONE`：得到 O_PATH 的 fd，不要权限。
  - 带 `CLONE`：要 `CAP_SYS_ADMIN`（EPERM）。源要在调用者的挂载命名空间里挂着，所以还没接上的复制品不能再复制（EINVAL）。复制出一个 bind 挂载（`AT_RECURSIVE` 时连同下面的挂载，与 `mount --rbind` 共用 `copy_submounts`），放在一个新的匿名挂载命名空间里（`MntNs::anon`），不挂在任何地方。返回的 O_PATH fd 指向它的根，由打开文件持有那个匿名命名空间（`OpenFile::tree`）。经由 fd 的 `*at` 调用照常用得了；fd 关掉、树又没被接上时，命名空间没人用了，下一次清理（`Vfs::collect`）就拆掉里面的挂载，与 Linux 的 `FMODE_NEED_UNMOUNT` 一样。
- **move_mount**：要 `CAP_SYS_ADMIN`；标志只能是 6.1 认识的那些（`MOVE_MOUNT__MASK`）。源、目标各自从目录 fd 起解析：`F_EMPTY_PATH`、`T_EMPTY_PATH` 时可以是 fd 本身，`F_SYMLINKS`、`T_SYMLINKS` 时跟随最后的符号链接。挪法与 `MS_MOVE` 共用（`move_tree`）：
  - 源要是一个挂载的根（EINVAL）：挂在调用者命名空间里的，就挪过去；是还没接上的复制品的根，就接上，树里的挂载都进调用者的命名空间（`Vfs::attach_tree`）。
  - 目标要在调用者的命名空间里，与源同是目录或者同不是（EINVAL）；挪挂着的挂载时目标不能在它里面（ELOOP）。
  - tuxport 没有共享的挂载，`MOVE_MOUNT_SET_GROUP` 总是 EINVAL（Linux 上源不是共享的也是）。
- **mount_setattr**：`struct mount_attr` 至少 32 字节（EINVAL），超过一页是 E2BIG，多给的要是 0（E2BIG）；要 `CAP_SYS_ADMIN`；什么也不改时直接成功。检查照 `build_mount_kattr`：
  - 传播类型只能是 0 或者四种之一，属性的未知位、高 32 位是 EINVAL；改访问时间要在 attr_clr 里给出整个 `MOUNT_ATTR__ATIME`，attr_set 是 relatime、noatime、strictatime 之一。
  - 目标要是挂载的根，在调用者的命名空间里或者是还没接上的复制品（EINVAL）。只读、nosuid、nodev、noexec、nodiratime、nosymfollow、访问时间记在挂载上，`AT_RECURSIVE` 时连同下面的挂载（`Vfs::subtree`）。传播类型检查之后不起作用，与 mount(2) 的一样。
  - idmapped 挂载（`MOUNT_ATTR_IDMAP`）：检查 userns 的 fd（不是打开的 fd 是 EBADF，不是用户命名空间是 EINVAL，开机时的那个是 EPERM）之后是 EINVAL，与文件系统不支持它时一样。crun 只在要求 idmap 的卷上用它，会报出这个原因。
- **删除别的命名空间里的挂载点**：`--uidmap` 的容器起得来之后，`podman run --rm` 结束时删 `userdata/shm` 报 EBUSY。tuxport 原来删除、改名时，看的是所有命名空间里的挂载（[仓库里的目录 bind](116-store-binds.md) 时这样改的），容器的挂载命名空间里复制过去的 shm 挂载还没清理，就挂在它上面。Linux 只看调用者自己命名空间里的（`is_local_mountpoint`），删掉之后别的命名空间里挂在那个目录项上的都卸下（`detach_mounts`）。改成一样：rmdir、unlink、被 rename 覆盖的目标都是这样；挂在文件上的按找到它的目录和名字区分硬链接。
- **用户命名空间里装 seccomp 过滤器**：`--uidmap` 的下一步，crun 在用户命名空间里装 seccomp 过滤器得到 EACCES。没有 no_new_privs 时，Linux 要的是自己所在的用户命名空间里的 `CAP_SYS_ADMIN`（`seccomp_prepare_filter` 用 `ns_capable(current_user_ns(), ...)`），tuxport 原来查的是开机时的那个，用户命名空间里的 root 装不了。改成一样。

## 测试

- 新的差分测试组 `newmount`（原生一侧是特权容器）：open_tree 带不带 CLONE、递归与否，mount_setattr、move_mount 的标志和错误，没接上的复制品，nobody 的权限。
- `mount` 组新增删除别的命名空间里的挂载点；`userns-ids` 组新增用户命名空间里装 seccomp 过滤器，原生的 Ubuntu 宿主开着 `apparmor_restrict_unprivileged_userns`，所以用 root 建用户命名空间。这一项第一版用了 Alpine 里没有的内核头文件，两边都编不过、等于没测。
- 两个执行层下都与原生逐字相同。LTP 的 mount_setattr02 由 CONF 变成 FAIL：原来 ENOSYS 时跳过，现在走到了检查共享、从属的传播类型，tuxport 没有传播（见推迟项）。
- 在 tuxport 里实际跑：`podman run --uidmap 0:100000:65536` 起得来、`--rm` 清理不报错；podman 的普查与原生对照各步相同（只有原生嵌套时 `--memory` 不行），docker 的照常。

## 推迟项

- fsopen、fsconfig、fsmount、fspick 仍是 ENOSYS（程序退回 mount(2)）；listmount、statmount 是 6.8 起的，6.1 没有。
- idmapped 挂载不支持（`MOUNT_ATTR_IDMAP` 是 EINVAL），podman、crun 的 `idmap` 卷选项用不了。
- 挂载之间没有传播，`MOVE_MOUNT_SET_GROUP` 总是 EINVAL，mount_setattr 的传播类型不起作用；LTP 的 mount_setattr02 因此失败。
- 没接上的复制品在 fd 关掉之后不是马上拆掉，要等下一次清理（有挂载命名空间没人用了的时候）；在那之前它不在任何命名空间里，看不到也碰不到。
