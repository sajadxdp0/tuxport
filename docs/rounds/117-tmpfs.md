# M9 之前：tmpfs（2026-10-30）

让 bubblewrap 能用的第二轮（第一轮见[仓库里的目录 bind](116-store-binds.md)）。bwrap 的新根本身就是一个 tmpfs（`mount("tmpfs", base, "tmpfs", MS_NODEV | MS_NOSUID)`，在里面建 `newroot`、`oldroot` 再 pivot_root），`--dev` 先在 `/dev` 上挂 tmpfs 再把设备 bind 进去，Codex 还要 `--tmpfs /tmp/codex-daemon-0`。原来 mount 别的类型都是 ENODEV。

Linux 的 tmpfs 在内存里。另写一个内存里的文件系统要让所有按路径、按 fd 的系统调用都多一种节点，mmap、文件锁、inotify、AF_UNIX 的路径名也都要重新接一遍；而且 tmpfs 里的东西占 App 的内存，`/tmp` 当初就是因此放在仓库里的（§8.2）。所以这里的 tmpfs 是仓库里不挂在根目录下面的一棵树，其余照旧：

## 做法

- **仓库**（`core/kernel/src/vfs/store.rs`）：`create_detached_dir` 新建一个上级是它自己的目录（与根目录一样），记在新表 `detached_roots` 里；`remove_detached_dir` 连同里面的一起删掉，与 unlink 一样还打开着、映射着的文件照常能用，inode 的行留到下次开机；独占打开仓库（开机）时先删掉表里上次留下的。rename 检查祖先时原来一直找到根目录，改成找到"上级是自己"的目录为止，不然在 tmpfs 里挪目录会一直找下去。
- **VFS**（`Vfs::new_tmpfs`）：每次 mount 是一个新的实例，有自己的设备号，按设备号登记（bind 挂载与源共用设备号）。umount 之后清理卸下的挂载的记录时（[上一轮](116-store-binds.md)），哪个挂载（挂在树上的、卸下了还有进程在用的）都不再看得到的实例连同里面的文件一起删掉，所以 `umount -l` 之后打开着的文件、bind 出去的子目录都让它活着。路径、`..`、getcwd、mountinfo 的第四列在 tmpfs 的根上停下。
- **mount(2)**（`core/kernel/src/syscall/mount.rs`）：选项按 Linux 6.1 的 `shmem_parse_one` 解析：`size=`（可以带 K、M、G 这些后缀，或者是物理内存的百分比）、`nr_blocks=`、`nr_inodes=`、`mode=`（八进制）、`uid=`、`gid=`（-1 是 EINVAL）、`inode32`、`inode64`、`huge=`、`mpol=`，以及 ro、rw、sync 这些通用的超级块标志；不认识的、值不对的是 EINVAL。根目录的权限默认 1777，属主、属组默认是挂载的进程的 fsuid、fsgid。mountinfo 的超级块选项照 `shmem_show_options`：大小、inode 数不是默认值（物理内存的一半）才写，权限不是 1777、属主属组不是 0 才写，最后是 `inode64` 或 `inode32`。原来 mount 读 data 只检查第一个字节读得了，现在读最多一页的字符串，只有 tmpfs 用。
- statfs 报告 `TMPFS_MAGIC`，总量是 `size=` 或物理内存的一半，用量不记、都算空闲（Linux 数页数）。fstat 打开着的普通文件、FIFO 原来设备号总是根文件系统的，改成按经由的挂载。`/proc/filesystems` 加上 `nodev tmpfs`。

## 测试

- 单元测试：仓库里不挂在根下的目录（新建、挪目录、删掉、下次开机删掉上次留下的）；VFS 的 tmpfs（设备号和选项、路径经由挂载点、卸下之后什么时候删掉里面的文件）。
- 新的差分测试组 `tmpfs`：选项和 mountinfo、statfs、里面的各种文件和 EXDEV、只读的 tmpfs、卸下和 `umount -l`，最后用一个静态链接的小程序照 bubblewrap 的步骤在一个进程里换根（不建命名空间），在新根里跑 busybox。两个执行层下都与原生相同。
- LTP 要只读文件系统的用例（`.needs_rofs`，LTP 自己挂一个只读的 tmpfs）原来卡在挂载这一步，现在 access04、chmod06、chown04、creat06、fchmod06、fchown04、fchownat03、lchown02、lchown02_16、link08、mkdir03、mkdirat02、mknod07、rmdir02、unlink10、utime06、utimes01 通过，madvise01 也通过（要一个 tmpfs 做文件映射）；chown04_16、fchown04_16 由失败变成跳过（arm64 没有 16 位的 chown）。

## 推迟项

- 不限大小：`size=` 只影响 statfs，写满了也不会 ENOSPC；inode 数也不限。用量不记，df 看到的已用总是 0。→ 已补上（M9 之前：tmpfs 的用量和限制）：按页数、inode 数记用量和限制，普通文件按大小算页数（当作没有空洞），删掉最后一个名字时就释放
- 数据在宿主的文件里、元数据在 SQLite 里，比 Linux 的内存 tmpfs 慢，也写闪存。
- 重新挂载时不看选项（Linux 可以改 `size=`、`nr_inodes=` 这些）。→ 已补上（M9 之前：tmpfs 的用量和限制）：`size=`、`nr_inodes=`、`inode32`/`inode64` 照 Linux 6.1 的 `shmem_reconfigure` 改，`mode=` 这些与 Linux 一样不变
- epoll 的 fdinfo 里 tmpfs 文件的设备号仍是根文件系统的（与合成文件的一样，见 [mount 的子集](105-mount-subset.md)的推迟项）。
- 别的文件系统类型仍是 ENODEV。
