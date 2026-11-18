# M9 之前：mount 的子集（2026-10-29）

[glibc 发行版的探测](69-glibc-probe.md)那一轮发现，从 Alpine chroot 进 Debian 之后 `/proc`、`/sys` 是空的，ps、top、free 都不能用：mount、umount2 是 ENOSYS。那时估了两档工作量：只挂合成文件系统（procfs、sysfs、devpts）要让它们的节点记着经由的挂载点，中等；bind 仓库里的目录和 tmpfs 要把路径都换成"挂载加节点"，大。后来先做了 App 里选 rootfs（[第 99 轮](99-choose-rootfs.md)），这一轮做第一档：合成文件系统可以挂到任意目录上，它们里面的目录可以 bind。

## 做法

- **节点带着挂载**（`core/kernel/src/vfs/mod.rs`）：
  - `Node::Pseudo` 多了挂载的编号，相当于 Linux 的 `struct path`：开机时的 `/proc` 和后来挂在 `/debian/proc` 上的 procfs 里，同一个目录是两个节点，`..` 各自回到自己的挂载点。仓库里的节点不带挂载，根文件系统只挂在 `/`。
  - 挂载表（`Mount`：文件系统、看到的根、挂载点、标志、设备号、source）放在 `Vfs` 里，挂载点可以是仓库里的目录、文件（bind 文件时，另记找到它的目录和名字，文件没有唯一的上级目录），也可以是别的挂载里的目录。路径解析每查到一个节点，上面挂着东西就换成最上面那个挂载的根；`..` 照 Linux 的 `follow_dotdot`：在挂载的根上先回到挂载点（叠着的一层层回去），到了 chroot 的根就不动，取上级之后上级上挂着东西时再进去；getcwd、`/proc/<pid>/{cwd,root}` 经由挂载点算路径。
  - 开机时的五个挂载编号不变（根 1、`/proc` 2、`/dev/pts` 3、`/sys` 4、`/sys/fs/cgroup` 6，fdinfo 里没有节点的文件仍是 5），之后的从 7 起，卸下的编号不再使用：卸下（`MNT_DETACH`）之后还开着的文件、当前目录带着原来的编号，不能算到新的挂载上。cgroup2 改成挂在 sysfs 的 `fs/cgroup` 目录上，原来是 sysfs 查 `fs/cgroup` 直接得到 cgroup2 的根，新挂的 sysfs 里它是空目录（与 Linux 一样要另外挂）。
  - `/proc/<pid>/mounts`、`mountinfo` 改为按挂载表生成（原来是固定的字符串）：按挂上的先后，挂载点相对那个进程的根目录，根目录以外的不列（Linux 的 `seq_path_root`，chroot 进去之后连根文件系统也不列），bind 挂载的第四列是被 bind 的目录，路径和 source 里的空格等写成八进制的转义。开机时的输出与原来逐字节相同。
  - 不持 BKL 的 stat（[第 25 轮](25-stat-without-bkl.md)）记着仓库里的挂载点，遇到就交给正常的路径解析；`..` 到了挂载点也交回去。挂载表变了时用 `fast_changed` 让各线程重新取。
- **mount(2)、umount2(2)**（新的 `core/kernel/src/syscall/mount.rs`），按 Linux 6.1 的 `path_mount`、`path_umount` 的顺序检查：
  - 新挂载：`proc`、`sysfs`、`devpts`、`cgroup2`，别的类型是 ENODEV（与没有编进内核的一样），挂载点不是目录是 ENOTDIR。原生 Linux 6.8 上实测：procfs（5.8 起）、devpts（4.7 起）每次挂载是一个新的实例，可以叠在同一处；sysfs、cgroup2 在同一个命名空间里只有一个实例，同一个挂载的根上再挂是 EBUSY。这里一样，新的 procfs 实例另外分配设备号（0:48 起）。devpts 的新实例在 Linux 上有自己的一套终端，这里各个挂载看到的是同一套，设备号也就相同（见下面 ttyname 的问题）。cgroup2 只能只读地挂，rw 时是 EROFS，busybox、util-linux 的 mount 会改成只读再试。
  - bind：源在合成文件系统里时，新挂载看到源的那个节点，标志和设备号与源所在的挂载相同；`MS_REC` 把源下面的挂载照样挂进去（先记下来再挂，目标在源里面时不会把自己也复制一遍）。源在仓库里是 EOPNOTSUPP。目录和非目录不配是 ENOTDIR。挂载点是经由 `/proc/<pid>/fd/<n>` 到了已经删除的文件、管道上是 ENOENT；是 `/proc/<pid>/fd`、`fdinfo` 里的项本身（经由指向它的 `O_PATH` fd 到达）也是 ENOENT，这是 Linux 6.12 的行为（"proc: block mounting on top of /proc/<pid>/fd/*"，LTP 的 mount08 测它），6.1 和原生的 6.8 上挂得上，tuxport 按新的。
  - 标志：nosuid、nodev、noexec、ro、访问时间（默认 relatime、`MS_STRICTATIME`、重新挂载时没给就保持原来的）记在挂载上。只读的挂载里打开文件写、改 `/dev/pts/<n>` 的属性是 EROFS（原来只对 cgroup2），nodev 的 devpts 里打不开终端；statfs 的 `f_flags` 按挂载的标志报告（原来总是 `ST_VALID`，根文件系统现在是 0x1020，与 docker 的 overlay 相同）。重新挂载只改这一个挂载。传播类型（private、slave、shared、unbindable）只检查参数：要是挂载的根、只能给一种，挂载之间没有传播。`MS_MOVE` 是 EOPNOTSUPP。
  - umount2：不是挂载的根是 EINVAL；挂载里还挂着别的、有进程的根目录、当前目录或者打开的文件在里面是 EBUSY；`MNT_DETACH` 连同里面的挂载一起卸下，还在里面的节点照常能用，卸下了的挂载的根没有上级，getcwd 以 `(unreachable)` 开头（musl 当作 ENOENT）；`MNT_EXPIRE` 第一次只标记、返回 EAGAIN。仓库是所有进程的根文件系统，卸它是 EBUSY。权限都要 `CAP_SYS_ADMIN`。
  - 删除、改名挂载点是 EBUSY（Linux 的 `is_local_mountpoint`）。原来 `rmdir /proc` 能把开机时的挂载点删掉。
- **chroot 里的 ttyname**：从 Alpine chroot 进 Debian、挂上 devpts 之后，Python 的 `os.ttyname` 报 ENODEV。第一版给每个新的 devpts 挂载分配了设备号，可各个挂载用的是同一套终端，从设备的 fstat 报告开机时那个设备号；glibc、musl 的 ttyname 拿 `/proc/self/fd` 里的名字 stat 一下，与 fstat 比对设备号和 inode，对不上就是 ENODEV。改成 devpts 的挂载都用同一个设备号。

## 测试

- 单元测试新增 `mounts_on_other_directories`：叠挂和 bind 之后的 `..`、路径、chroot 之后的不可达、设备号，不持 BKL 的解析交回正常路径，卸下之后的节点和编号。
- 新的差分测试组 `mount`：挂载、bind、重新挂载、卸下和各种错误，之后的 mountinfo、设备号、`..`，以及 chroot 进去再挂 procfs、devpts 之后的 ttyname，两个执行层下都与原生相同。原生一侧的容器要有 `CAP_SYS_ADMIN`，colima 的虚拟机和 GitHub 的 runner 上还要关掉 AppArmor：`scripts/difftest` 认用例里 `# docker-run-options:` 这一行，把选项加给它的容器。mountinfo 不比设备号和超级块的选项：docker 的 `/sys` 是只读的，devpts 的新实例在 Linux 上默认 `mode=600`。
- 从 Alpine chroot 进 Debian（第 69 轮的仓库），挂上 proc、sysfs、devpts 和 cgroup2 之后 ps、top、free、findmnt、`os.ttyname` 都正常。
- LTP 新通过 mount08。

## 推迟项

- 仓库里的目录不能 bind（`mount --rbind /dev`、`--bind /run`、把 `/root` 这类目录共享进 chroot），没有 tmpfs：第二档，仓库的节点也要带上挂载，当前目录、打开文件记的路径、inotify、AF_UNIX 的路径名都要跟着改（[第 69 轮](69-glibc-probe.md)）。chroot 进去之后 `/dev` 里的设备节点要自己复制过去（`cp -a /dev/null …`），`/dev/ptmx` 指向 `pts/ptmx`。 → 仓库里的目录、文件的 bind 已补上（M9 之前：仓库里的目录 bind）；tmpfs 也补上了（M9 之前：tmpfs）
- `MS_MOVE`、pivot_root、新的挂载接口（fsopen、open_tree、move_mount、mount_setattr 等）都没有，util-linux 的 mount 在 ENOSYS 之后退回 mount(2)。LTP 里它们的测试和要 loop 设备的 mount、umount 测试照旧失败（loop 设备、块设备也没有）。 → `MS_MOVE`、pivot_root 已补上（M9 之前：仓库里的目录 bind）；新的挂载接口、loop 设备仍没有；open_tree、move_mount、mount_setattr 有了（M9 之前：open_tree、move_mount 和 mount_setattr），fsopen 这一组仍没有
- devpts 的各个挂载共用一套终端：Linux 上新的实例有自己的终端（编号从 0 开始），`/dev/ptmx` 按它旁边的 `pts` 找实例。挂载选项都不看（procfs 的 hidepid、subset，devpts 的 mode、gid、ptmxmode 等），mountinfo 的超级块选项总是开机时的那几个。→ devpts 已经按实例，选项也看了（M9 之前：devpts 的实例）；procfs 的选项仍不看
- 超级块的标志不分开记：重新挂载只改这一个挂载（Linux 不带 bind 时改的是超级块，挂在别处的同一个实例也跟着变）；cgroup2 只能只读。→ cgroup2 能可写地挂了（M9 之前：可写地挂 cgroup2）；超级块的标志仍不分开记
- 传播类型不模拟：shared、slave 设置成功，但挂载之间没有传播，mountinfo 里没有 `shared:N` 这些可选字段。没有挂载命名空间（§6），所有进程看到同一张挂载表。 → 挂载命名空间已补上（M9 之前：挂载命名空间）；传播类型仍不模拟
- 终端从设备的打开文件不记得经由哪个 devpts 挂载打开的：`/proc/<pid>/fd/<n>` 跟随之后按开机时的那个算，卸下时也不算它在用。→ 已改（M9 之前：devpts 的实例）
- `MNT_EXPIRE` 的标记在之后有人用过这个挂载时不清掉（Linux 的 `mntput` 清掉）。
- 卸下（`MNT_DETACH`）了的挂载里还开着的节点，设备号按开机时的实例算；`/proc/<pid>/cwd`、`root` 这类链接带 `(unreachable)`，Linux 的 `d_path` 不带（只有 getcwd 带），chroot 之后看根目录以外的也一样（以前就是这样）。epoll 的 fdinfo 里合成文件的设备号也按开机时的实例算。→ 链接和打开文件记的路径已改成与 `d_path` 一样不带 `(unreachable)`（2026-10-30，单独修补）：根目录以外的是从真正的根算起的路径，卸下了的挂载里的从那个挂载的根算起，只有 getcwd 还带；设备号后来也改了：卸下的挂载留着记录，还开着的节点的设备号按那个挂载的算（M9 之前：仓库里的目录 bind）；epoll 的 fdinfo 里合成文件的设备号仍按开机时的实例算
- inotify 监视着的合成文件所在的挂载卸下时没有 `IN_UNMOUNT`。
- 只在 colima 的虚拟机里跑，没上手机。
