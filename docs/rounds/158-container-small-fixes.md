# M9 之前：容器运行时碰到的四处小问题（2026-11-01）

[podman 和 docker 的普查](157-container-survey.md)找到的问题 1–4。普查时已经在工作区里改掉了，这一轮补上测试。都是与 Linux 6.1 对不上的小地方，每一处都让 podman 或 docker 的某一步直接失败。

## 做法

- **capget 探测版本**（`syscall/cred.rs`）：数据指针为空时先返回，版本不认识（写回内核的版本之后）也是 0，pid 不看，与 Linux 的 `capget` 一样（`(dataptr == NULL) && (ret == -EINVAL) ? 0 : ret`）。原来版本不认识时返回 EINVAL，pid 不对时 ESRCH。moby/sys/capability 用 `capget(&hdr{version: 0}, NULL)` 取版本，podman、buildah、runc 都用它：podman 的每个命令都报 `unable to get capability version from the kernel`。
- **开机时的命名空间里什么也不改的接口请求**（`net.rs` 的 `configure_host_link`）：dockerd 给容器的默认网络沙箱拉起 lo（`RTM_NEWLINK` 带 `IFF_UP`），原来开机时的命名空间里所有修改请求都回 EOPNOTSUPP。现在 `RTM_NEWLINK`、`RTM_SETLINK` 照 Linux 6.1 的 `rtnl_setlink`、`__rtnl_newlink` 找接口、按顺序给出错误：没有 `CAP_NET_ADMIN` 是 EPERM；`RTM_NEWLINK` 的编号是负数 EINVAL；按编号找，编号不是正数时按 `IFLA_IFNAME` 找，都没给时 `RTM_SETLINK` 是 EINVAL、`RTM_NEWLINK` 当作找不到；找不到是 ENODEV，带 `NLM_F_CREATE` 时是 EOPNOTSUPP（Linux 不认识设备类型时也是）；找到了而带 `NLM_F_EXCL` 是 EEXIST，带 `NLM_F_REPLACE` 是 EOPNOTSUPP。之后标志按 `rtnl_dev_combine_flags` 合起来（`ifi_change` 是 0 时整个 `ifi_flags` 都算），`__dev_change_flags` 改得了的位（UP、PROMISC、ALLMULTI、NOARP、MULTICAST 等）与现在的相同、属性只有相同的名字和 MTU 时成功，别的仍是 EOPNOTSUPP：App 改不了宿主的接口。新的网络命名空间里的 lo 照旧（[网络命名空间](122-net-namespaces.md)）。
- **合成文件系统根目录的 inode 编号**（`pseudo.rs` 的 `ino_of`）：procfs、sysfs、cgroup2、devpts 的根目录是 1，与 Linux 相同（procfs 的 `PROC_ROOT_INO`、kernfs 的根、devpts 的根），别的节点仍是哈希。runc 用的 pathrs-lite 打开 `/proc` 之后检查 `st_ino == 1`（`verifyProcRoot`），不是 1 就报 `unsafe procfs detected: incorrect procfs root inode number`。各个文件系统的设备号不同，（设备号, inode）仍不重复。
- **线程目录里的 fd、fdinfo**（`pseudo.rs`）：`/proc/<pid>/task/<tid>/` 加上这两项，内容是进程的打开文件（tuxport 的 fd 表按进程记）。runc 经由 `openat2(/proc, "thread-self/fd/", RESOLVE_BENEATH | RESOLVE_NO_XDEV | RESOLVE_NO_MAGICLINKS)` 读自己的 fd 的链接，原来是 ENOENT。

## 测试

- 新的差分测试组 `containers`（容器运行时用到的内核接口，之后几轮接着往里加）：capget 探测版本；开机时的命名空间里什么也不改的 `RTM_NEWLINK`、`RTM_SETLINK` 和各种错误、`ip link set lo up`（原生一侧加 `--cap-add NET_ADMIN`；真要改的请求在原生上会改掉容器的 lo，不比）；几个合成文件系统根目录的 inode 编号（docker 容器里的 `/sys/fs/cgroup` 是 cgroup 命名空间里的子目录，原生上不是 1，不比）；照 pathrs-lite 的做法经由 openat2 读 `thread-self/fd/`，含另一个线程和新的 pid 命名空间里。两个执行层下都与原生逐字相同。
- 写用例时两边都编译失败过一次（仓库里没有内核的头文件、musl 没有 `strerrorname_np`），输出一样算通过，看了 `.native` 才发现。
- `net.rs` 的单元测试 `host_links_accept_requests_that_change_nothing`：各种请求和错误码，真要改的都是 EOPNOTSUPP。

## 推迟项

- 线程目录里的 fd 用的是进程的 fd 表：clone 不带 `CLONE_FILES` 的线程（同一个线程组里很少见）在 Linux 上有自己的。
- 开机时的命名空间里真要改接口的请求仍是 EOPNOTSUPP；Linux 上 root 改得了，App 在宿主上改不了。
