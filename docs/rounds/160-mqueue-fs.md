# M9 之前：mqueue 文件系统（2026-11-01）

[podman 和 docker 的普查](157-container-survey.md)的问题 5：OCI 的默认挂载里有 `/dev/mqueue`，crun 给每个容器都挂（`mount -t mqueue`），tuxport 没有这种文件系统（[UTS 和 IPC 命名空间](121-uts-ipc-namespaces.md)的推迟项），是 ENODEV，podman 的容器一个也起不来。

照 Linux 6.1 的 `ipc/mqueue.c` 做：mqueue 文件系统就是 POSIX 消息队列的名字表，列出的是挂上它的进程所在的 IPC 命名空间里的队列。

## 做法

- **文件系统**（`vfs`、`syscall/mount.rs`）：新的类型 `mqueue`。每个 IPC 命名空间一个实例（Linux 的 `get_tree_keyed`）：挂上的是调用者所在的命名空间的，要在它的属主里有 `CAP_SYS_ADMIN`（`mount_capable`）；同一个命名空间的实例不能再挂在自己的挂载的根上（EBUSY），别的命名空间的可以。挂载经由 `Mount::pin` 留住那个命名空间，里面的队列随之留着。设备号按命名空间：开机时的命名空间是原来 mq fd 用的 0:19，别的第一次用到时分配（`Vfs::mq_dev`），mq_open 得到的 fd 的 fstat 也报告它。statfs 的类型是 `MQUEUE_MAGIC`（0x19800202），mq fd 的 fstatfs 也是（原来报告的是根文件系统的 ext4）。
- **节点**（`pseudo.rs`）：根目录 `MqRoot(命名空间)`，队列 `Mq(命名空间, inode 编号)`。队列有了自己的 inode 编号（全局递增，Linux 的 `get_next_ino`）和三个时间：发送、接收改 atime、mtime、ctime，读状态行、登记或取消通知、mq_getsetattr 改了标志时改 atime、ctime，与 Linux 相同。
  - 根目录 01777，属主是建立 IPC 命名空间的进程的 fsuid、fsgid（开机时的是 root），nlink 2，大小是 (2 + 队列数) × 20（`DIRENT_SIZE`），新建、删除队列时更新时间；可以 chmod、chown、utimensat。
  - 队列文件：权限、属主是队列的，大小 80（`FILENT_SIZE`），读到的是原来 mq fd 上 read 的状态行。
- **操作**（`syscall/fs.rs`、`syscall/mqueue.rs`）：
  - open：得到的 fd 与 mq_open 的一样（`FileKind::MessageQueue`），mq_timedsend 这些照样能用；write 是 EINVAL。`O_CREAT` 新建一个默认大小的队列（`msg_default`、`msgsize_default`），权限去掉 umask，要有根目录的写权限，队列数到上限时没有 `CAP_SYS_RESOURCE` 是 ENOSPC（与 mq_open 共用 `new_queue`）；刚新建的打开时不再查权限。
  - unlink：删除队列（与 mq_unlink 共用 `remove_queue`）。根目录有粘滞位：不是队列或根目录的属主、对队列也没有 `CAP_FOWNER` 时是 **EPERM**（Linux 的 `may_delete`）。mq_unlink 系统调用原来回的是 EACCES，现在也是 EPERM：Linux 的 mq_unlink 就是 `vfs_unlink`，是 musl 和 glibc 的 `mq_unlink` 把 EPERM 换成了 EACCES，经由 libc 调用的看不出区别。rmdir 一个队列先查粘滞位再 ENOTDIR，unlink `q/` 是 ENOTDIR，`.`、`..` 与别处一样。
  - mkdir、mknod、symlink、link、rename 是 EPERM：mqueue 的目录没有这些操作。
  - chmod、chown、utimensat（按路径和按 fd，mq_open 得到的 fd 上的 fchmod 也行）改的是队列自己的，权限检查与别的文件相同。

## 测试

- 差分测试 `containers` 组新增：挂上 mqueue 之后的 mountinfo、根目录和 statfs，mq_open 建的队列在文件系统里的样子，在文件系统里新建、打开、改属性、删除队列，不支持的操作，粘滞位下删别人的队列（直接的系统调用是 EPERM、libc 的是 EACCES），新的 IPC 命名空间里的实例。两个执行层下都与原生逐字相同。
- 写用例时子进程的输出丢过一次：stdout 接的是管道、全缓冲，fork 出来的子进程 `_exit` 不刷，原生上也一样丢，等于没测到，fork 之前和 `_exit` 之前都加了 fflush。
- 在 tuxport 里实际跑：不再需要任何临时绕过，`podman run --network host --cgroups disabled` 起得来，容器里的 `/dev/mqueue` 是 mqueue 文件系统。

## 推迟项

- 状态行的 `NOTIFY_PID` 是全局的 pid，Linux 按读者的 pid 命名空间（`pid_vnr`）。原来就是这样。
- 打开着的队列不算卸载时"还在用"：Linux 上经由挂载打开的文件让 umount 是 EBUSY。
- 已经存在的队列 `O_TRUNC` 打开，Linux 会把文件大小改成 0（`simple_setattr`），这里仍是 80。
- mkdir 已经存在的名字，Linux 先是 EEXIST，这里是 EPERM。→ 已改：合成文件系统里名字已经存在时先是 EEXIST，mqueue 的根目录也一样（[M9：进程被结束、服务没起来时提示](195-kill-reports.md)）
- inotify 在 mqueue 文件系统里没有事件。
