# M9 之前：测试提速，以及顺带找到的几个问题（2026-11-02）

每轮的全套测试要半个多小时：格式、clippy、单元测试、构建约 2 分钟，差分测试 Tier 0 约 17 分钟、Tier 1 约 8 分钟，LTP 约 5 分钟。上一个提交（343d100）先把 LTP 降到 2.5 分钟（最长的先跑、`LTP_TIMEOUT_MUL`、`LTP_RUNTIME_MUL`，见设计文档 §13.3）。这一轮做剩下的三件：差分测试并发跑、把 colima 虚拟机调大、查 LTP 里最慢的 fork14。三件都顺带找到了问题。

结果：全套从半个多小时降到约 9 分钟——构建这些 2 分钟不变，差分测试 Tier 0 3 分 50 秒、Tier 1 1 分 15 秒，LTP 1 分钟。

## 做法

- **差分测试并发跑**（`scripts/difftest -j`）：
  - macOS 上默认 4 组同时跑，Linux 宿主上默认 1 组（CI 不变）。原生一侧本来就各在各的容器里。
  - tuxport 一侧起初共用一个仓库，第一次试就有 3 组失败：各组都往 guest 的 `/tmp` 里写同名的 `cli.py`、`srv.py`，互相覆盖。所以每个并发通道用一份仓库的克隆（`<仓库>-w<n>`），子进程用 `mkdir` 抢一个空闲的通道号。
  - 克隆在 macOS 上是 APFS 的 clonefile（`cp -c`，写时复制），一份 1.8 GB、3.9 万个文件约 7 秒，几乎不占空间；别处是 `cp --reflink=auto`。仓库里的对象文件是就地改写的，不能用硬链接共用。克隆在没有时、带 `-r` 时重做：主仓库装了新的软件包本来就要 `-r` 重新导出镜像。不按 `meta.db` 的修改时间判断：每次运行开头的检查都会写到它。
  - 上次耗时（原生加 tuxport）长的组先开始，记在 `target/difftest/durations-t<执行层>.txt`。每组的结果整段打印，`ok` 一行带两边各几秒。
  - 写耗时文件时踩到 awk 的坑：`NR == FNR` 在第一个文件是空的时候对第二个文件也成立，Tier 0 第一次跑时一组都没跑就"通过"了。改成 `FILENAME == ARGV[1]`；上一个提交里 `scripts/ltp` 的同样写法一起改了（耗时文件是空的时候所有测试都会被跳过）。
- **用例自己在干等的**（并发之后最长的一组决定总时间，按耗时排出来才看到）：
  - `setns-pidfd`：原生、tuxport 各 100 秒。`unshare -p -f` 的父进程等子进程时屏蔽了 SIGTERM，子进程是新的 pid 命名空间里的 1 号进程，祖先命名空间发来的信号只有 SIGKILL、SIGSTOP 一定送到，所以最后的 `kill` 两个都杀不掉，`wait` 等到里面的 `sleep 100` 自己结束。两边行为一样（tuxport 照 Linux 做了），改成对子进程发 SIGKILL，现在各 1 秒。
  - `pty`：原生 11 秒、tuxport 1 秒。包模式下等状态字节时只等异常条件（POLLPRI），Linux 的 `pty_stop`、`pty_start` 唤醒等待者时只带 `EPOLLIN`，只等异常条件的 select 被过滤掉，要等到 5 秒的超时（两次）；tuxport 马上叫醒，输出相同。改成同时等可读。
- **colima 虚拟机**：4 个 CPU、4 GiB 内存换成 8 个 CPU、8 GiB（Mac 有 8 个性能核、4 个能效核，32 GB），盘从 30 GB 加到 40 GB（见下面的 LTP）。8 个 CPU 下差分测试 `-j 8` 反而比 `-j 4` 慢（227 秒对 202 秒，当时最长的一组就要 200 秒），默认留在 4。
  - LTP 在 8 个 CPU 上 `-j 8` 跑时，几个到几十个测试的文件操作是 EIO、每个卡 15 秒（第一次 958 个通过、5 分多钟，第二次 1002 个、135 秒）。仓库在 `target/ltp/store`，macOS 上经由 virtiofs，每次文件操作都要到 Mac 那边；八个 `tuxport-run` 同时写同一个仓库，每个操作都立即提交（`BEGIN IMMEDIATE … COMMIT`），等写锁超过 10 秒（`busy_timeout`）就是 EIO。原来 4 个 CPU 时真正同时跑的少，没有撞上。现在 `scripts/dev` 挂一个虚拟机本地的 docker 卷 `tuxport-ltp`，`scripts/ltp` 在容器里把仓库放在 `/ltp/store`（`LTP_STORE` 可以另指）：没有 EIO 了，全量 61 秒（包括第一次新建仓库）。sendfile09 要 5 GB 空闲，虚拟机的盘只剩 1.6 GB 时是 CONF，所以盘加到 40 GB。
  - Android 上只有 App 一个进程用仓库，没有这种争用；几个 `tuxport-run` 共用一个仓库只在开发、测试时有。
  - 内存变了之后 `netsysctl` 组失败：`tcp_max_syn_backlog`、`tcp_ehash_entries` 原生上翻了一倍。Linux 按内存大小定它们（`alloc_large_system_hash` 给 TCP 连接表每 128 KB 内存一个桶、取 2 的幂、最多 512 K 个；`tcp_sk_init` 里 `tcp_max_tw_buckets` 是桶数的一半，`tcp_max_syn_backlog` 是 1/128、至少 128），tuxport 写死了 4 GB 时的值。第 177 轮还没推送，CI 的机器内存不同，推上去也会失败。现在按宿主的内存总量算（`netconf::memory_default`）。
  - `gdb` 组失败一次：`info threads` 里当前线程那行名字后面对齐的空格随 LWP 号的位数变，用例把号换成了 `N`，没压掉空格。用例改了。
- **并发找到的 tuxport 的问题：端口映射释放之后马上换不回来**（`portmap::LINGER`）：
  - `nftables` 组并发时失败过一次：UDP 的回答来自 `10.70.0.1:51199`，原生是 `10.70.0.1:8080`（DNAT 换回去的）。
  - 原因：新命名空间里显式绑的端口在宿主上另选（[第 174 轮](174-namespace-ports.md)），对方收到时按登记表把宿主端口换回 guest 端口。Linux 在数据包到达时换算，tuxport 在对方 recvfrom、accept 时才换：服务回完一个数据报就关掉 socket（`srv.py` 正是这样），登记随最后一个租约去掉，对方读到时已经查不到了。负载高的时候客户端醒得晚，就撞上了。
  - 现在映射全部释放之后，宿主端口换回 guest 端口还保留 10 秒。这段时间里宿主把这个端口分给同一个命名空间里另一个映射的，换成新的；分给没有映射的 socket 时会换错，宿主随机选临时端口，可能性很小。
- **fork14**（LTP 里最慢的，150 秒超时；更早是 4 分多钟后被 OOM 结束）：
  - 它每次多映射 1 GB（可写、私有、匿名）再 fork，一直到 16 TB 多，映射超过 16 TB 之后 fork 还成功就算失败（Linux 3.5 修过的 VMA 长度溢出）。
  - 原生（4 GB 内存、没有交换区）第 4 次 fork 就是 ENOMEM，两千次 0.03 秒：相邻的匿名映射合成一个 VMA，`dup_mmap` 按 `vm.overcommit_memory` 是 0 的估算逐个检查计入提交量的 VMA，合起来的超过内存和交换区的总量就失败。
  - tuxport 里 fork 总是成功：每次 mmap 各是一段宿主内存，VMA 合不起来，子进程的映射逐个向宿主申请，每段都只有 1 GB。而 soft-MMU 的页表按页建（每 GB 2 MB、26 万个表项），fork 时子进程的页表整个重建，映射了 i GB 时一次 fork 是 O(i)：前 256 次就要 66 秒，页表最后把 App 撑到内存上限。
  - 现在 fork 之前照 Linux 按合起来的 VMA 估算（`Mm::commit_too_big`）：计入的是可写的私有映射、没有 `MAP_NORESERVE`（VMA 新记下了这个标志，拆分、fork、mremap 都带着）；合起来的条件照 `vma_merge`：首尾相接，私有匿名或者同一个文件、偏移接得上，权限、mlock、`MADV_DONTDUMP`、向下增长、名字相同。上限是宿主的内存加交换区（sysinfo），与 guest 看到的 `/proc/meminfo` 一样。
  - fork14 现在约 2 秒结束，结果从超时（TBROK）变成被 OOM killer 结束（仍是 TBROK）：fork 照原生失败了，但它还要接着映射一万多次，页表每 GB 2 MB，一千多 GB 就到了 App 的上限。见推迟项。

## 测试

- 差分测试新增的用例：`nsports`、`nftables` 里服务回完就退出，客户端等它退出之后才读回答，来源照样是 guest 的端口：用改之前的二进制跑确定地复现，不靠负载。`fork` 组测几段可写私有映射合起来超过内存加交换区时 fork 的结果，首尾相接、有空隙、`MAP_NORESERVE`、只读的各种情形。
- 单元测试：`fork_counts_the_commit_of_merged_mappings`、`memory_sized_defaults`，`portmap` 的测试加了释放之后的保留。
- 耗时（8 个 CPU，差分测试 `-j 4`，LTP `-j 8`）：

  | | 原来 | 现在 | 现在最长的 |
  |---|---|---|---|
  | 差分测试 Tier 0 | 约 17 分钟 | 3 分 50 秒 | c-programs（两边合计 130 秒） |
  | 差分测试 Tier 1 | 约 8 分钟 | 1 分 15 秒 | filter（36 秒，丢弃的连接要等到客户端的超时） |
  | LTP | 约 5 分钟（上个提交 2.5 分钟） | 1 分钟 | fork13（60 秒） |

## 推迟项

- soft-MMU 的页表按页建：映射多大，页表就有多大（每 GB 2 MB），mmap、fork 的开销都随映射的大小线性增长。fork14 那样一直映射到 16 TB 的程序仍然被 OOM 结束，Linux 上没碰过的映射不占页表。要么按需建表（缺页时再填，内核层读写 guest 内存的地方也要能触发），要么大的映射用 2 MB 的块表项（Tier 1 生成的代码在 TLB 未命中时内联走表，要一起改）。真实程序里很少见，不急。 → 按需建表了；AddressSanitizer 的影子内存就是这样，一次 mmap 带走整个 App（M9 之前：页表按需建立）
- 提交量的估算只在 fork 时按合起来的 VMA 做；mmap、brk、mremap、mprotect 仍由宿主按单次申请判断（宿主的 `overcommit_memory` 不是 0 时与 guest 看到的不一样）。`VM_ACCOUNT` 只看现在的权限（Linux 可写过就一直计入）。`vm.overcommit_memory` 只有 0，写不了。
- 端口映射释放之后只多换 10 秒：更晚才读到的回答报告宿主的端口。
- CI（Linux）上差分测试仍然一组一组跑：runner 的 docker 存储不一定支持 reflink，每个通道真复制一份仓库要几 GB。
