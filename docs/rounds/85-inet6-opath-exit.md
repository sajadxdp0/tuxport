# M9 之前：if_inet6、O_PATH 的 ioctl 和中途停住之后的出口（2026-10-28）

审内核功能缺口时挑出来的三项，都不大：Java 实际会碰到的 `/proc/net/if_inet6`，和两处与 Linux 不一致的细节。改出口的停止时另外查出一个 bug，一起修了。

## 做法

- **`/proc/net/if_inet6`**（[网络信息的缺口](78-net-info-gaps.md)的推迟项）：
  - **原因**：OpenJDK 的 libnet（21、25 都是）判断 IPv6 能不能用时读这个文件，打不开或者是空的就整个关掉 IPv6（`IPv6_supported`）。tuxport 没有这个文件，JVM 在 `::1` 上监听不了，连 `[::1]` 得到 `SocketException: IPv6 protocol family unavailable`（原生是 `Connection refused`），只有 IPv6 地址的主机也连不上。
  - **改动**：按 Linux 的 `if6_seq_show` 的格式列出宿主 getifaddrs 给的 IPv6 地址（与 `ip addr` 用的是同一份）：32 个十六进制数字的地址、接口编号、前缀长度、scope、标志、右对齐的接口名。scope 照 `ipv6_addr_scope` 从地址看（回环 10、链路本地 20、站点本地 40、别的 00），标志与 netlink 的回答一样都是 80（`IFA_F_PERMANENT`）。
  - **结果**：在一个带 IPv6 的 docker 网络里，同一个容器中宿主内核的和 tuxport 的逐行对照：地址、编号、前缀、scope、接口名都相同，只有标志不同（宿主上 eth0 的 ULA 是 82，多了 `IFA_F_NODAD`；链路本地是 c0，多了 `IFA_F_TENTATIVE`）。JVM 在 `::1` 上监听、连接、收发正常。
- **ioctl 不接受 `O_PATH` 的 fd**（[FICLONE 返回 EOPNOTSUPP](74-ficlone.md)的推迟项）：Linux 的 ioctl 用 `fdget` 取 fd，`O_PATH` 打开的文件取不到，是 EBADF；FICLONE、FICLONERANGE 取源 fd 也一样。tuxport 原来照样取到：源或者目标是 `O_PATH` 的 FICLONE 按"不是仓库里的文件"返回 EXDEV，别的 ioctl 照常执行（`FIOCLEX` 成功）。现在 ioctl 的 fd 和 FICLONE 的源 fd 是 `O_PATH` 打开的都是 EBADF。
- **系统调用中途停住之后的出口**（[seccomp](59-seccomp.md)的推迟项）：
  - **出口**：跟踪者用 PTRACE_CONT 让线程继续，线程在系统调用中途停住（fork、vfork、clone 和 vfork 结束的 `PTRACE_EVENT_*`）时被改用 PTRACE_SYSCALL 继续。arm64 Linux 的 `syscall_trace_exit` 在出口重新看标志，这个系统调用的出口要停。tuxport 原来只有入口停过的、或者有 seccomp 的线程才在出口检查，这时不停，跟踪者下一次看到的是下一个系统调用的入口。现在有跟踪者时每个系统调用的出口都按那时的状态检查；没有跟踪者、也没有 seccomp 的线程与原来一样不查。信号投递停止、group-stop 都在出口检查之后，不受影响。
  - **编号**：原来只在入口停住时记下系统调用的编号，用 PTRACE_CONT 继续的线程停在 fork 事件上时，`NT_ARM_SYSTEM_CALL` 读到的是以前某一次入口停住时记的（测试里是 0）。现在有跟踪者时每个系统调用都记下（Linux 每次进内核都写 `regs->syscallno`）；vDSO 里读时钟的存根不记，Linux 上它们不进内核。
  - **查出来的 bug**：有 seccomp 的线程经由 vDSO 读时钟时，PTRACE_SYSCALL 下出口会多停一次。存根不停入口、不记编号，seccomp 也不检查它，但有 seccomp 时出口照样检查，拿上一次记的编号判断是不是存根，判断成普通的系统调用就停下：跟踪者看到一个没有入口的出口。自己装了过滤器的程序被 `strace -f` 跟踪时就会遇到。现在存根在出口也不停。这是 [seccomp](59-seccomp.md)那一轮起的 bug。

## 测试

- 差分测试 ptrace 组新增一节：用 PTRACE_CONT 继续到 fork 事件时读到的编号，改用 PTRACE_SYSCALL 之后停在 clone 的出口；seccomp 组新增一节：装了过滤器的子进程在 PTRACE_SYSCALL 下经由 vDSO 读时钟不多停。
- python 组的 FICLONE 那一段加了 `O_PATH` 的 fd 上的 FICLONE、FICLONERANGE、FIOCLEX。
- compat 组比对 if_inet6 里回环的那一行，java 组在 `::1` 上监听、连接、收发。环境里没有 IPv6 时两边都没有那一行、报同样的错，结果仍然一致。

## 推迟项

- if_inet6 的标志都是 `IFA_F_PERMANENT`：getifaddrs 分不出临时地址、正在检测重复的地址（宿主上的 82、c0 这类）。顺序按接口排，Linux 按地址的哈希表排。
- 不是系统调用造成的停住（缺页之类引起的信号投递停止）时，`NT_ARM_SYSTEM_CALL` 仍是上一次记下的编号，Linux 是 -1；不持 BKL 的快速路径（§6）上的系统调用也不记，被跟踪、用 PTRACE_CONT 继续的线程在它之后停在信号投递停止里时，读到的是之前的编号。
