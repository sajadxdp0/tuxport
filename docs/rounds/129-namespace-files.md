# M9 之前：命名空间文件留住命名空间（2026-10-31）

[挂载命名空间](118-mount-namespaces.md)到[网络命名空间](122-net-namespaces.md)几轮的同一条推迟项：打开着的、bind 挂载出去的命名空间文件不让命名空间活着，里面的进程都退出之后，对它 setns 是 EINVAL。`ip netns add` 正是这样留住网络命名空间的：它 unshare 出一个网络命名空间，把 `/proc/self/ns/net` bind 到 `/var/run/netns/<名字>` 上就退出，之后 `ip netns exec`、`ip -n` 打开这个文件 setns 进去。改之前在 tuxport 里 `ip netns add` 成功，`exec` 全是 "setting the network namespace failed: Invalid argument"；busybox 的 `unshare --net=<文件>` 和 `nsenter --net=<文件>` 也一样。顺带补上命名空间文件上的 `NS_GET_*` ioctl（原来是 ENOTTY），以及 `ip netns list` 要的 RTM_GETNSID。

## 做法

- **打开着的文件持有命名空间**：新加 `NsRef`（`core/kernel/src/nsproxy.rs`），是七种命名空间对象的引用（time 命名空间只有开机时的那个）。打开命名空间文件时把它放进打开文件（`FileKind::Proc` 的 `ns`），setns 直接用它，不再按编号在还有线程的命名空间里找。各种命名空间的资源本来就跟着对象的引用计数走，被留住的就都还在：挂载命名空间的挂载（`Vfs` 按弱引用清理）、IPC 命名空间里的对象（`Kernel::ipcs`）、网络命名空间占着的宿主回环地址 127.a.b.0/24。pid 命名空间的 1 号进程退出之后照旧不能再在里面建进程：setns 进得去，之后 fork 是 ENOMEM，与 Linux 相同。
- **bind 挂载出去的文件**：bind 的源是命名空间文件时，新挂载在 nsfs 里（新的 `Fs::Nsfs`，设备号 0:4），持有那个命名空间（`Mount::pin`），标志只有 rw，mountinfo 里是 `net:[编号] <挂载点> rw - nsfs nsfs rw`，与 Linux 相同。与 Linux 的 `do_loopback` 一样，经由别的挂载命名空间里的路径找到的也能 bind。经由挂载点打开的文件在 `/proc/<pid>/fd` 里是挂载点的路径（Linux 的 `d_path`），经由 `/proc/<pid>/ns` 打开的仍是 `net:[编号]`。
- **挂载命名空间不能经由挂载留住自己**：挂载命名空间的文件只能 bind 进比它旧的命名空间（Linux 的 `mnt_ns_loop`，编号按建立的先后分配，比编号），bind 自己的、更旧的都是 EINVAL；复制挂载命名空间（unshare、clone 的 `CLONE_NEWNS`）时不复制 bind 出去的挂载命名空间文件和挂在它上面的（Linux 的 `copy_tree` 不带 `CL_COPY_MNT_NS_FILE`），别的命名空间文件照样复制。清理挂载时丢掉的挂载里有这种文件，它留着的命名空间可能也没人用了，接着再清理一遍。
- **按编号找命名空间**：经由 `/proc/<pid>/ns/*` 打开、经由 `/proc/<pid>/fd/<n>` 重新打开、bind 的源这些地方只知道种类和编号：先在还有线程的命名空间里找（原来 setns 的做法），再找 bind 挂载出去的（卸下了还有人用的也算），最后找各个进程打开着的命名空间文件。
- **`NS_GET_*`**（`syscall/ns.rs` 的 `ioctl`，按 Linux 6.1 的 `ns_ioctl`）：`NS_GET_NSTYPE` 是它的 `CLONE_NEW*`；`NS_GET_USERNS` 打开它的属主（用户命名空间的属主是上一层），属主要是调用者所在的用户命名空间或者它下面的，开机时的用户命名空间没有属主，都是 EPERM；`NS_GET_PARENT` 对用户命名空间同 `NS_GET_USERNS`，对 pid 命名空间打开上一层（要是调用者所在的 pid 命名空间或者它下面的，否则 EPERM），别的种类 EINVAL；`NS_GET_OWNER_UID` 是用户命名空间的建立者在调用者的用户命名空间里的 ID，别的种类 EINVAL。打开得到的 fd 带 `FD_CLOEXEC`，F_GETFL 是 0。6.9 起才有的 `NS_GET_MNTNS_ID` 这些与 6.1 一样是 ENOTTY（tuxport 报告的是 6.1）。
- **RTM_GETNSID**（`net.rs`）：`ip netns list` 对每个文件问它的编号。tuxport 不给网络命名空间分配编号（RTM_NEWNSID 仍是 EOPNOTSUPP），对方找得到时总是回答 -1（`NETNSA_NSID_NOT_ASSIGNED`，Linux 上没有 `ip netns set` 过时也是）：`NETNSA_PID` 的进程要还在（ESRCH），`NETNSA_FD` 要是网络命名空间的文件（EBADF、EINVAL）；按 `NETNSA_NSID` 找的、要换算到 `NETNSA_TARGET_NSID` 的都找不到；什么都没给是 EINVAL，顺序与 Linux 的 `rtnl_net_getid` 相同。原来回答 EOPNOTSUPP，`ip netns list` 每次多打两行 "RTNETLINK answers: Not supported"。
- **F_GETFL 里的 `O_LARGEFILE`**：对照 `NS_GET_USERNS` 得到的 fd 时发现的。原来 F_GETFL 和 fdinfo 的标志对所有打开文件都加上 `O_LARGEFILE`，Linux 只有 open、openat2、open_by_handle_at 打开的才有（64 位上总是加上，`O_PATH` 时去掉）。现在在这几处打开时记在打开文件上；memfd 原来就带着（Linux 的 memfd_create 也加），控制台的从设备、`tuxport-run` 交给 guest 的宿主 stdio 照旧带着；管道、socket、eventfd、epoll、pidfd、消息队列这些与 Linux 一样不带了。

## 测试

- 新的差分测试组 `nsfs`（原生一侧加 `CAP_SYS_ADMIN`、`CAP_NET_ADMIN`）：bind 出去的、打开着的命名空间文件留住没有进程的命名空间，setns 进去看到的，卸下之后，挂载命名空间文件的 bind 和复制，`NS_GET_*` ioctl 从里面和外面看，busybox 的 `unshare --net=<文件>` 和 `nsenter`，iproute2 的 `ip netns` 各个子命令。差分测试的软件包加上 iproute2（原生一侧的镜像只打包 bin、etc、lib、sbin、usr、tmp，用例先补上 `/var/run`）。`compat2` 组新增各种打开文件的 F_GETFL 和 fdinfo 的标志。
- 单元测试：`bound_namespace_files_keep_namespaces`（留住、复制时不带、卸下之后清理）；`netns_ids_are_never_assigned`（RTM_GETNSID 的回答和错误）。
- `compat` 组里原来按 busybox 的 `ip` 写的几行改成显式的 `busybox ip`：装了 iproute2 之后 `/sbin/ip` 是它，原生的容器里 eth0 是 veth，iproute2 多显示一个 `link-netnsid 0`（`IFLA_LINK_NETNSID`），tuxport 拿不到。
- LTP 的 ioctl_ns01–04、ioctl_ns06 由跳过变成通过。

## 推迟项

- 最后一个打开着的命名空间文件关掉之后，命名空间留下的挂载、IPC 对象要等下一次线程退出或者 umount 才清理（Linux 上马上释放）。没有进程在里面，看不出来，只是多占一会儿。
- setns 仍不接受 pidfd（一次换几种命名空间）。→ 已补上（M9 之前：setns 经由 pidfd）
- `NS_GET_*` 得到的 fd 记在原来那个文件经由的 procfs 挂载上（fdinfo 的 `mnt_id`），Linux 上在看不到的 nsfs 挂载上。
- RTM_NEWNSID（`ip netns set`）仍是 EOPNOTSUPP，`ip netns list-id` 是空的。 → 转储 veth 时给对端所在的命名空间分配编号，`RTM_GETNSID` 回答它（M9 之前：有状态的网卡）；RTM_NEWNSID 仍是 EOPNOTSUPP
- `tuxport-run` 交给 guest 的宿主 stdio 的 F_GETFL 总带着 `O_LARGEFILE`，宿主那边是管道时 Linux 上没有。
