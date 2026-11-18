# M9 之前：挂载命名空间（2026-10-30）

让 bubblewrap 能用的第三轮（[仓库里的目录 bind](116-store-binds.md)、[tmpfs](117-tmpfs.md) 之后）。bwrap 用一次 `clone(CLONE_NEWNS | CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWIPC | CLONE_NEWNET)` 建好所有命名空间，原来这些标志都是 EPERM（§6 定过不做）。用户命名空间牵涉 70 多处能力检查和所有对外的 uid、gid，单独一轮；这一轮先做挂载命名空间，连同 `/proc/<pid>/ns` 和 setns 这些各种命名空间共用的部分。

## 做法

- **放在哪**：Linux 的挂载命名空间在 nsproxy 里，这里放在 `FsContext`（根目录、当前目录、umask，`CLONE_FS` 共用的部分）里：`CLONE_NEWNS` 不能和 `CLONE_FS` 同用，unshare 的 `CLONE_NEWNS` 连带 `CLONE_FS`，setns 要求 fs 没有和别人共用，所以共用一个 fs 的任务总在同一个挂载命名空间里，放在一起是一样的，也省得这一轮先搭起整套 nsproxy。
- **挂载表**（`core/kernel/src/vfs/mod.rs`）：挂载记录照旧全局按编号存（挂载编号本来就是全局的），每个记着所属的命名空间；新挂载属于挂载点所在的挂载的命名空间。路径解析不用改：挂载点节点本身带着挂载编号，复制出来的挂载有自己的编号，各个命名空间的挂载互相挂不到对方上。`copy_ns` 复制一个命名空间里挂在树上的所有挂载（先全部分配编号再复制，`MS_MOVE`、pivot_root 之后挂载点所在的挂载可能比它后挂上），tmpfs 实例由两边共用。命名空间由 `Rc` 持有、挂载表记弱引用：线程退出（释放了它的 fs）之后有命名空间没人用了，就卸下它的所有挂载，丢掉没人用的记录，里面的 tmpfs 随之删掉；umount 时也顺带清理。
- **clone、unshare**（`core/kernel/src/syscall/ns.rs`）：`CLONE_NEWNS` 要 `CAP_SYS_ADMIN`，复制原来的命名空间，新任务（unshare 时是自己，fs 先不再共用）的根目录、当前目录换到复制出来的对应挂载上（Linux 的 `copy_mnt_ns`）。别的 `CLONE_NEW*` 仍是 EPERM。
- **mountinfo** 只列那个进程的命名空间里的挂载。mount、umount、remount、`MS_MOVE`、pivot_root 涉及的挂载都要在调用者的命名空间里（Linux 的 `check_mnt`），否则 EINVAL。pivot_root 换根时只会换到同一个命名空间里的进程（它们的根目录节点才会相同），新命名空间里的 pivot_root 不影响外面。
- **`/proc/<pid>/ns/`**（`task/<tid>` 里也有）：十种命名空间的魔法链接，按名字排序，读出来是 `mnt:[4026531841]` 这样（`pid_for_children` 写 `pid`）。挂载命名空间的编号按那个任务的，别的是开机时的那个（与 Linux 常见的编号相同）；新的命名空间从 4026532160 起编号。跟随之后是 nsfs 里的文件：普通文件、权限 444、设备号 0:4（原来开机时的 procfs 用 0x4，改成 0x17）、inode 号是命名空间的编号，statfs 是 `NSFS_MAGIC`；打开之后读是 EINVAL，`/proc/<pid>/fd/<n>` 显示 `mnt:[…]`。
- **setns(2)**（原来是 ENOSYS）：fd 要是打开的命名空间（不支持 pidfd，EINVAL），`nstype` 是 0 或与它相同的 `CLONE_NEW*`。挂载命名空间照 Linux 的 `mntns_install`：先要 `CAP_SYS_ADMIN`、`CAP_SYS_CHROOT`（EPERM），fs 和别的线程、进程共用时 EINVAL，然后根目录、当前目录都换成那边的根（setns 到自己的命名空间也是）。别的种类只有开机时的那个：用户命名空间是 EINVAL（已经在里面），其余要 `CAP_SYS_ADMIN`，什么也不做。
- **`/proc/<pid>/root`、`cwd` 改成魔法链接**：原来按读出来的文字在读者自己的根下面解析，经由 `/proc/<pid>/root/...` 到不了别的命名空间（测试里从外面 bind 子进程命名空间里的挂载，Linux 是 EINVAL，tuxport 却在自己的命名空间里找到了同名的目录挂上了）。现在与 Linux 的 `proc_root_link`、`proc_cwd_link` 一样直接指到那个任务的节点，chroot 外面的也进得去。readlink 照旧。

## 测试

- 单元测试 `copies_and_drops_mount_namespaces`：复制出来的挂载、新命名空间里的 tmpfs 外面看不到、没人用之后卸下。
- 新的差分测试组 `namespaces`（以后几轮接着加）：`/proc/self/ns` 的链接和 nsfs 文件，unshare 之后两边挂的互不可见、从外面经由 `/proc/<pid>/root` 碰它的挂载，setns 和它的错误，新命名空间里 pivot_root，两个执行层下都与原生相同。
- LTP 新通过 pivot_root01、setns01、unshare01、unshare04；ioctl_ns01–04 由失败变成跳过（走到了还没有的 `NS_GET_*` ioctl），mount_setattr02 也变成跳过（走到了还没有的 mount_setattr）。

## 推迟项

- 用户、pid、网络、IPC、UTS、cgroup、time 命名空间还没有（下一轮起），`/proc/<pid>/ns` 里它们总是开机时的那个。 → 用户命名空间已补上（M9 之前：用户命名空间），pid 命名空间也补上了（M9 之前：pid 命名空间），UTS、IPC 命名空间也补上了（M9 之前：UTS 和 IPC 命名空间），网络命名空间也补上了（M9 之前：网络命名空间），cgroup 命名空间也补上了（M9 之前：bubblewrap 和 AI 编程 CLI 的沙箱），time 命名空间也补上了（M9 之前：time 命名空间）
- 打开着的命名空间文件、bind 挂载出去的命名空间文件不让命名空间活着：用它的进程都退出之后就卸下了，之后对那个 fd setns 是 EINVAL。→ 已补上（M9 之前：命名空间文件留住命名空间）
- 命名空间文件的 `NS_GET_USERNS`、`NS_GET_PARENT`、`NS_GET_NSTYPE`、`NS_GET_OWNER_UID` 这些 ioctl 没有（ENOTTY）；setns 不接受 pidfd。→ 这四个 ioctl 已补上（M9 之前：命名空间文件留住命名空间）；setns 接受 pidfd 也补上了（M9 之前：setns 经由 pidfd）
- `/proc/<pid>/ns/*`、`root`、`cwd` 跟随时不检查能不能跟踪那个进程（Linux 要 `PTRACE_MODE_READ_FSCREDS`）；`/proc/<pid>/exe` 仍按读出来的文字跟随。→ `ns/*`、`root`、`cwd` 的检查已补上，跟随和读链接时都查（M9 之前：用户命名空间）；`exe` 的检查后来也补上了（M9 之前：/proc/<pid>/exe 的跟踪权限），跟随后来也改成直接到程序文件本身（M9 之前：/proc/<pid>/exe 指向程序文件本身）
- 删除、改名一个目录时，它在别的挂载命名空间里是挂载点也算 EBUSY（Linux 只看调用者的命名空间）。
- 不持 BKL 的 stat 只在开机时的根文件系统的挂载里走：新命名空间里的进程的 stat 都要取 BKL。
- 没人用的命名空间只在线程退出、umount 时清理；没有 `/proc/sys/user/max_mnt_namespaces`。→ `max_mnt_namespaces` 在下一轮补上了（M9 之前：用户命名空间），后来可以写（M9 之前：网络命名空间），不实际限制；清理的时机没变，命名空间文件留住的关掉之后也一样（M9 之前：命名空间文件留住命名空间）
- 传播类型仍不模拟（[mount 的子集](105-mount-subset.md)的推迟项）：新命名空间和原来的之间没有传播。
