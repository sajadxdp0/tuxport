# M9 之前：仓库里的目录 bind（2026-10-30）

Codex、Claude Code 的沙箱用 bubblewrap 建，在 tuxport 里第一步就失败（[AI 编程 CLI](103-ai-cli-survey.md)的推迟项）。看了 Codex 0.165 实际的调用：默认参数下每条命令都经由 `bwrap --unshare-user --unshare-pid --unshare-ipc --unshare-net --ro-bind / / --tmpfs … --remount-ro … --dev /dev --proc /proc` 运行；它留着的 Landlock 退路（`features.use_legacy_landlock`）也要 bwrap，直接 panic（"filesystem-restricted execution requires bubblewrap"）。所以要让 bwrap 在 tuxport 里跑通，拆成几轮：仓库里的目录 bind（这一轮）、tmpfs、挂载命名空间和用户命名空间、pid 命名空间、网络等命名空间，最后用 bwrap、Codex、Claude Code 端到端地试。

这一轮是[mount 的子集](105-mount-subset.md)说的第二档：仓库的节点也带上挂载，仓库里的目录、文件可以 bind，挂载的标志对仓库生效，再加上 bwrap 要的 `MS_MOVE`（它不用，顺手）和 pivot_root。

## 做法

- **仓库节点带着挂载**（`core/kernel/src/vfs/mod.rs`）：`Node::Store(挂载编号, inode)`，与 `Node::Pseudo` 一样相当于 Linux 的 `struct path`。同一个目录 bind 到两处是两个节点，`..`、getcwd、`/proc/<pid>/{cwd,root,fd}` 经由各自的挂载点回去；挂载的根没有上级（`parent_in_fs`），bind 挂载里看不到被 bind 的目录外面。
  - 根文件系统也进了挂载表（编号 1，原来单独处理），挂载点改成可选的：挂载树的根没有，Linux 上它挂在看不到的 rootfs 上，所以 mountinfo 里它的上级写 0（原来写它自己的 1）。pivot_root 可以换掉挂载树的根，原来的根挂到别处、卸下都照常。
  - 卸下（`MNT_DETACH`）的挂载原来直接从表里去掉，`..` 和设备号按文件系统算：bind 进来的合成目录卸下之后 `..` 能走到被 bind 的目录外面。现在卸下的挂载留一份记录，里面的节点照常能用、挂载的根没有上级，umount 之后丢掉没有进程的根目录、当前目录、打开文件在里面的。挂载可以被 `MS_MOVE`、pivot_root 挪到比它后挂上的挂载里，找挂在里面的挂载改成一层层找。
  - 打开文件（`OpenFile::mnt`）记下仓库里的普通文件、设备节点和 FIFO 是经由哪个挂载打开的（目录和 `O_PATH` 的节点本来就带着），fdinfo 的 `mnt_id`、`/proc/<pid>/fd/<n>` 跟随之后的节点、fstatfs、umount 时算不算在用都按它。`name_to_handle_at` 报告的挂载编号也是实际的（原来总是 1），`open_by_handle_at` 打开的文件算 `mount_fd` 所在的挂载。
  - 不持 BKL 的 stat 只在根文件系统的挂载里走：根目录、当前目录在别的挂载里，根文件系统不是挂载树的根了（`..` 到它的根要出去），或者它是 nosymfollow 的，都交给正常的解析。
- **bind**（`core/kernel/src/syscall/mount.rs`）：源是仓库里的目录、文件时也挂得上，新挂载看到源的那个节点，标志和设备号（8:1）与源所在的挂载相同；`--rbind` 带上源下面的挂载（不论挂在仓库里还是合成文件系统里）。bind 文件时记下找到它的目录和名字，mountinfo 的第四列按它算（文件没有唯一的上级目录）。
- **挂载的标志对仓库生效**，检查的顺序照 Linux 6.1：
  - 只读：新建（mkdir、mknod、symlink、link、open 的 `O_CREAT`、AF_UNIX 的 bind）在名字已经存在（EEXIST）之后、目录可写之前报 EROFS，所以 `mkdir -p` 碰到已有的目录照常；unlink、rmdir、rename 在查找名字之前报；chmod、chown、utimes、truncate、setxattr、removexattr 和按 fd 的 fchmod、fchown、futimens 都是 EROFS；写方式打开普通文件是 EROFS（截断在权限检查之前，别的在之后），设备节点、FIFO 照常能写；faccessat 问 `W_OK` 在权限之后报 EROFS。
  - noexec：execve（包括解释器和 `#!` 的解释器）是 EACCES；mmap 带 `PROT_EXEC` 是 EPERM，没带的以后也不能 mprotect 成可执行（EACCES）：VMA 多了 `may_exec`（Linux 的 `VM_MAYEXEC`），smaps 的 VmFlags 按它写 `me`（原来总有）。
  - nosuid：execve 不让 setuid、setgid 位和文件能力生效。
  - nodev：仓库里的设备节点打不开（EACCES，原来只管 devpts）。
  - nosymfollow：路径里的符号链接（包括 procfs 的魔法链接）不跟随，ELOOP；readlink 照常。
  - 跨挂载的 rename、link 是 EXDEV，同一个文件系统 bind 到两处也是（Linux 比较的是挂载）；顺序也照 Linux（rename 的 EXDEV 在 `.`、`..` 和只读之前，link 的在名字已经存在、只读之后）。删除、改名挂载点的 EBUSY 改成经由哪个挂载都算。
  - 重新挂载根文件系统原来只接受访问时间的标志（别的是 EOPNOTSUPP），现在与别的挂载一样记在挂载上。
- **`MS_MOVE`**：源要是挂在树上的挂载的根，目录和非目录不配是 EINVAL，目标在源里面是 ELOOP（挪挂载树的根也是：Linux 上它挂在 rootfs 上，目标总在它里面），连同挂在里面的一起挪。原来是 EOPNOTSUPP。
- **pivot_root(2)**（原来是 ENOSYS）：照 Linux 6.1 的 `pivot_root`：先要 `CAP_SYS_ADMIN`；两个路径都要是目录；`put_old` 上挂着东西时挂在最上面的那个上；已经删除的目录是 ENOENT；`new_root`、`put_old` 在根目录所在的挂载里是 EBUSY；根目录、`new_root` 要是挂载的根，`put_old` 要在 `new_root` 下面，`new_root` 要在根目录下面，否则 EINVAL。然后 `new_root` 的挂载换到根目录所在的挂载的位置，原来的根挂到 `put_old` 上，根目录、当前目录是原来的根的进程都换过去（`chroot_fs_refs`）。bwrap 用的 `pivot_root(".", ".")` 加 `umount2(".", MNT_DETACH)` 也照 Linux：原来的根叠在新根上，umount 的路径最后要跟进挂在上面的（`LOOKUP_MOUNTPOINT`），mount 的挂载点也是（`lock_mount`），原来这两处对 `.`、当前目录这类不经过查找的路径不跟。

## 测试

- 单元测试新增 `binds_store_directories`：bind 目录和文件之后的 `..` 和路径、chroot 进挂载里、只读按经由的挂载算、`MS_MOVE`、卸下之后的记录。
- 新的差分测试组 `bind`：bind 目录和文件、`--rbind`、`umount -l`，EXDEV 和 EBUSY，只读、noexec、nosuid、nodev、nosymfollow 下的各种操作，`MS_MOVE` 和 pivot_root 的错误，以及照 bwrap 那样 pivot_root 进 bind 的目录之后的 mountinfo，两个执行层下都与原生相同。原生一侧的根文件系统是 overlay，只比挂载点、看到的目录和选项。

## 推迟项

- 没有 tmpfs（下一轮）。→ 已补上（M9 之前：tmpfs）
- 没有挂载命名空间（§6），pivot_root、`umount -l` 原来的根这些都作用于所有进程看到的同一张挂载表。 → 已补上（M9 之前：挂载命名空间）
- 卸下的挂载的记录只在 umount 时清理：进程退出、关掉文件之后不再用的，要等下一次 umount 才丢掉。
- 经由 `/proc/<pid>/fd/<n>` 重新打开的已经删除的文件算根文件系统的挂载，不是原来打开它时经由的。
- 不持 BKL 的 stat 不在 bind 挂载、pivot_root 换上来的根里走：沙箱里的进程的 stat 都要取 BKL。
- 超级块的标志仍不分开记（[mount 的子集](105-mount-subset.md)的推迟项）：仓库的几个挂载各管各的，重新挂载根文件系统成只读不影响 bind 出去的；调用者的根目录所在的挂载不带 `MNT_DETACH` 卸是 EBUSY（Linux 把根文件系统重新挂成只读）。
- 传播类型仍不模拟，`MS_MOVE`、pivot_root 里对 shared 挂载的 EINVAL 也就没有。
