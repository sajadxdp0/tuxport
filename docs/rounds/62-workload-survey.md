# M9 之前：负载普查（2026-10-28）

剩下的内核推迟项按"常见程序碰不碰得到"筛下来，多是长尾。差分测试只覆盖 C 程序、coreutils、Go、Java、Node.js、Python、Lua/SQLite 和 gdb，手机终端上常见的服务和工具大多没在 tuxport 里跑过。这一轮把它们跑一遍，找出实际会碰到的缺口。

## 做法

`scripts/survey` 在 Alpine 3.24 的仓库里装好软件包（约 1 GB），把 `tests/survey/<名字>.sh` 拷进仓库运行，带 `TUXPORT_LOG=1`，最后每个负载一行：退出码、用时和未实现的 syscall。输出要人看，不和原生比对。看出异常的，再导出仓库在原生 Linux 上跑同样的命令，或者在 tuxport 里用 `TUXPORT_STRACE`、core 文件和 gdb 追下去。都在 colima 的虚拟机里用 Tier 1 跑。

工具：`scripts/survey [-t 执行层] [-s 仓库] [负载……]`，负载在 `tests/survey/`。

## 结果

### 正常的

| 负载 | 跑了什么 |
|---|---|
| OpenSSH 10.3（sshd） | 公钥登录（root 和普通用户）、执行命令、管道、scp、sftp、端口转发；sshd 的 seccomp 沙箱照常工作 |
| tmux 3.7 | 后台服务器、面板、窗口、改窗口大小 |
| htop 3.5、btop 1.4、fish 4.6、zsh 5.9、neovim 0.12 | 在 tmux 里截屏：画面正常，nvim 的 `:terminal` 能用 |
| PostgreSQL 18.6 | initdb、事务和保存点、索引、并行查询（两个 worker，动态共享内存）、VACUUM、LISTEN/NOTIFY；pgbench 4 个连接约 3400 tps |
| Rust 1.96（cargo） | 第一次普查时 release 构建、测试、从 crates.io 拉 regex 编译都成功；之后要生成代码的编译每次都崩，见下面的问题 1 |
| .NET SDK 10.0.303 | dotnet new、build、run（线程池、Task、GC、子进程）；GC 按 `memory.max` 的 75% 定堆的上限 |
| PHP 8.5、Ruby 3.4、nginx 1.30 | PHP 的内置 web 服务器；Ruby 的线程、fork、TCP；nginx 4 个 worker、`accept_mutex off`（`EPOLLEXCLUSIVE`）、reload |
| rsync 3.5、git 2.54 | 经由 ssh 的 rsync 和 git clone/push，git daemon |
| zig 0.16、pip 26.1、socat 1.8 | `zig build-exe`、`zig cc`；venv 里 pip 安装 requests；socat 的 TCP 和 UDP |

未实现的 syscall 只有两个，都没有影响：236（`get_mempolicy`，.NET 的 GC 探测 NUMA，没有 NUMA 的 Linux 也返回 ENOSYS）和 425（`io_uring_setup`，nvim 里的 libuv 探测，按 §15 保持 ENOSYS，它退回 epoll）。

### 找到的问题

原因都已查到代码：

1. **fork 之后父进程的页权限被改错，随后崩溃**。rustc 生成代码的编译（cargo 的 release 构建、直接调用 rustc）每次都在退出时 SIGSEGV，Tier 0 也一样；把同一个仓库导出到原生 Linux 上，同样的命令每次都成功。core 文件显示 musl 的 `__funcs_on_exit` 读 atexit 链表的节点时出错，那一页是 malloc 早先 mmap 的，之后 rustc 自己没有动过它。给页表的修改加上监视之后看到：rustc fork 出的链接器进程 exec 时把借来的页还回来，rustc 的 `Mm::repossess` 恢复写权限，`Runs` 把客户机地址和宿主地址都连续的页接成一段，再整段用第一页所在 VMA 的权限。这一段从信号栈的保护页（PROT_NONE）开始，一直跨到 malloc 的那两页，15 页都成了不能访问的。宿主的 mmap 自顶向下分配，相邻的小映射宿主地址往往也相邻，所以只要进程 fork 过、子进程 exec 或退出，带保护页的映射（线程栈、信号栈）旁边的页就可能出错。会不会撞上取决于地址恰好怎么排：第一次普查时编译都成功了，之后每次都崩，多半是装了别的软件包之后映射的排法变了。→ 已修好（M9 之前：fork 之后还回的页按各自的权限恢复）
2. **ssh 分配不了 PTY**。`ssh -t` 和交互式登录（root 和普通用户都是）被 sshd 关掉，日志是 `chown(/dev/pts/1, 0, 5) failed: Permission denied`；mosh 经由 `ssh -t` 起 mosh-server，因此也用不了。sshd 的特权进程把终端交给登录的用户和 tty 组（gid 5）：tuxport 的 `/dev/pts/<n>` 属组是打开主设备的进程的（Linux 按 devpts 的挂载选项 `gid=5`，mountinfo 里也是这么写的），所以 root 登录也要 chown，而合成文件系统里的节点一律不能 chown、chmod（EACCES）。→ 已修好（M9 之前：终端的属主和权限）
3. **redis、valkey 不肯启动**。它们在 arm64 上启动时检查一个内核的 bug（`ARM64-COW-BUG`）：写过一页之后 fork，子进程读 `/proc/self/smaps` 里那一页的 `Shared_Dirty`，是 0 就认为内核会在后台保存时损坏数据，退出。tuxport 的 smaps 把匿名内存都算作私有的脏页，`Shared_Dirty` 总是 0。→ 已修好（M9 之前：smaps 里共用的页）
4. **mariadb 卡住**。`mariadb-install-db` 永远不结束。InnoDB 用 Linux 原生 AIO，关闭时主线程 `io_destroy`，等着读事件的线程应当从 `io_getevents` 返回 EINVAL（Linux 的 `aio_read_events` 看 `ctx->dead`），主线程再 join 它。tuxport 的 `io_getevents` 拿着上下文自己的引用循环等待，`io_destroy` 只把上下文从表里拿掉、叫醒一次，等待者醒来接着等。→ 已修好（M9 之前：io_destroy 叫醒等事件的线程）
5. **.NET 看不到别的进程用的内存**。另一个 guest 进程占着 400 MB 时，.NET 的 GC 报告的内存负载仍是 39 MB（`memory.current` 已经 543 MB）。.NET 按 `memory.current` 减去 `memory.stat` 里的 `inactive_file` 算 cgroup 的用量，没有 `memory.stat` 就退回进程自己的 RSS；几个进程一起把内存用到上限时，它的 GC 不会提前收紧。→ 已修好（M9 之前：cgroup 的 memory.stat）
6. **free、htop 显示内存几乎用完**。`/proc/meminfo` 由 `sysinfo` 算，`MemAvailable` 就是 `MemFree`，`Buffers`、`Cached` 都是 0：宿主的页缓存全算作已用，free 显示可用 650 MB（原生 3.4 GB）。libuv 的 `uv_get_available_memory`、psutil 这些按 `MemAvailable` 判断的程序同样读到偏小的值。→ 已修好（M9 之前：/proc/meminfo 照搬宿主的）

接下来：按影响依次修，每轮修完重跑对应的负载：fork 之后的页权限（1），devpts 的 chown/chmod 和属组（2），smaps 的共享页（3），`io_destroy` 叫醒等待者（4），cgroup 的 `memory.stat`（5），`/proc/meminfo`（6）。

## 推迟项

- 只在 colima 的虚拟机里跑，没上手机：Android 上 App 的 sshd 能不能被别的设备连上、SELinux 下宿主的哪些文件读得到，仍待真机确认（R1、M5 起的推迟项）。
- 普查的输出不和原生自动比对，也不进 CI（要联网安装约 1 GB 软件包）。→ 决定不做（M9 之前：补测待查的几项）：找到的问题修好时都在差分测试或 cargo test 里加回归检查，普查留作人看的工具
- 没跑的：docker/podman、systemd（D 档）、图形界面，以及 PHP、Ruby、Rust 的大型项目和语言自带的测试集。→ 大型项目和测试集已经跑了（M9 之前：第二轮普查，语言的测试集和大项目）；docker/podman、systemd、图形界面仍没有
