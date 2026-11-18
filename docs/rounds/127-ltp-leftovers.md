# M9 之前：LTP 剩下的小差异（2026-10-31）

LTP 的 syscall 测试全量 214 个失败里，146 个要 loop 设备（"Failed to acquire device"），17 个是测试自己缺命令（退出码 127），还有改时钟（App 改不了宿主的）、跟踪 pid 1 这些已知的。剩下十几个真正的失败，这一轮逐个放到原生 Alpine 容器里对照：不只看通不通过，还看是哪几条失败，并且换掉会影响结果的原生环境（容器的 overlayfs 不支持文件句柄，docker 默认的 AppArmor 会检查 sendmsg 带的地址）。[剩下的小缺口](18-small-gaps.md)那一轮也这样对照过，当时的原生环境掩盖了其中几个。

## 原生同样失败的

musl 的行为，或者 musl 和内核版本的组合，不归 tuxport：gethostname02（musl 截短主机名不报错）、pathconf02（musl 的 pathconf 不看路径）、gethostbyname_r01、sbrk01（musl 的 sbrk 只认 0）、nftw01、nftw6401、setgroups03（musl 的 `NGROUPS` 是 32）、mq_open01、open13（musl 的 fchmod、fchown 在 `O_PATH` 的 fd 上退回 `/proc/self/fd`）、readlinkat02 和 readlink03 里长度为 0 的一项（musl 把它换成 1），pwrite04（musl 1.2.5 的 pwrite 用 pwritev2 的 `RWF_NOAPPEND`，6.9 之前的内核是 EOPNOTSUPP）。

## 做法

- **readlink 的空路径**：读 dirfd 本身，`AT_FDCWD` 是当前目录（原来是 EBADF），不是符号链接时是 ENOENT（原来是 EINVAL），与 Linux 的 `do_readlinkat` 相同。readlink03 剩下 musl 的那一项。
- **TCP 的 sendto 不把地址交给宿主**：TCP 不看 sendto、sendmsg 给的地址（`MSG_FASTOPEN` 除外），宿主的 LSM 却可能按地址族检查它：开发用的 colima（Ubuntu 24.04）的 AppArmor 拒绝 AF_UNSPEC 的地址（EAFNOSUPPORT），sendto01 的第 4 项因此在 tuxport 和原生上都失败，第 18 轮当作原生的行为；`apparmor=unconfined` 的原生容器里它是成功的。现在 TCP 的地址留在内核层（`MSG_FASTOPEN` 时照常交出）。Android 上没有这一层检查。
- **`SHM_HUGETLB`**：tuxport 没有大页，现在与没有编进 hugetlbfs 的 Linux 一样是 EINVAL（`newseg` 里 `hstate_sizelog` 是 NULL）；原来当作普通的段建出来。LTP 按 `scripts/ltp` 给的内核配置（没有 `CONFIG_HUGETLBFS`）期望 EINVAL。
- **name_to_handle_at**：句柄放不下时也写 mount_id，句柄头写回需要的长度和 `FILEID_INVALID`（原来只写长度）；mount_id 或句柄写不进去时是 EFAULT，盖过 EOVERFLOW（Linux 的 `do_sys_name_to_handle`）。
- **open_by_handle_at**：与 Linux 的 `handle_to_path` 一样先读句柄头、查长度、读整个句柄，最后才看 mount_fd。句柄只在仓库里解得开：mount_fd 在管道、socket、宿主的 stdio、合成文件系统上时是 ESTALE，与 Linux 上这些文件系统没有 `export_operations` 一样（原来当作根文件系统，照样打开了）。
- **带偏移的写在 `O_APPEND` 的文件上**：Linux 上 pwrite64、pwritev（以及不带 `RWF_NOAPPEND` 的 pwritev2）照样写到文件末尾，文件位置不变（pwrite(2) 的 BUGS 一节）；原来写在给的偏移处。glibc 的 pwrite 直接用 pwrite64，Debian、Ubuntu 上的程序看到的与 Linux 不同。这样的写持着 BKL 做，与别的追加不会写到同一个位置。musl 的 pwrite 带 `RWF_NOAPPEND`，照旧写在偏移处（[M6](08-m6.md) 为它接受了这个 6.9 才有的标志），所以 pwrite04 仍失败。
- **差分测试的脚本长度**：`scripts/difftest` 把整个脚本作为 `sh -c` 的一个参数交出去，Linux 不让单个参数超过 128 KB（`MAX_ARG_STRLEN`）。这一轮往 `compat` 组加用例时它到了 131 KB，两边都执行不了（E2BIG），输出一样，被当成了通过。现在超过时直接报失败；前两轮加在 `compat` 组里的 raw ICMP、subreaper 的用例和这一轮的挪到新的 `compat2` 组。

## 测试

- 差分测试 `compat2` 组新增：`O_APPEND` 的文件上的 pwrite64、pwritev（直接调用系统调用，绕开 musl），各种 fd 上 readlinkat 的空路径。两个执行层下都与原生相同。sendto、文件句柄、`SHM_HUGETLB` 的原生结果取决于宿主的 AppArmor、文件系统和大页配置，不放进差分测试，由 LTP 检查。
- LTP 的 name_to_handle_at02、open_by_handle_at02、sendto01、shmget02 由失败变成通过；readlink03、readlinkat02、pwrite04 仍失败，剩下的是 musl 的那几项。

## 推迟项

- mmap 的 `MAP_HUGETLB` 仍当作普通内存，与 `SHM_HUGETLB`、memfd 的 `MFD_HUGETLB`（都是 EINVAL）不一致；没有 hugetlbfs 的 Linux 上它也是 EINVAL。改了会让现在靠它"成功"的程序（postgres 的 `huge_pages=try` 等）改走退路，要另外看。
- 文件句柄只按"是不是仓库"判断：tmpfs 的数据也在仓库里，句柄就是 inode 编号，经由 tmpfs 上的 mount_fd 打开根文件系统的句柄照样成功（Linux 上是 ESTALE）。
- UDP 等 sendto 的地址仍原样交给宿主：AF_UNSPEC 的地址 Linux 当作 AF_INET，开着 AppArmor 网络管控的宿主上会被拒绝（只影响开发用的 Linux 宿主）。
- `RWF_NOAPPEND` 是 Linux 6.9 的标志，tuxport 报告 6.1 却接受它（为了 musl 的 pwrite，[M6](08-m6.md) 起）。
