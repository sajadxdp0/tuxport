# M9 之前：页表按需建立，AddressSanitizer 能用了（2026-11-02）

[第 178 轮](178-test-speed.md)查 LTP 的 fork14 时记下的推迟项：soft-MMU 的页表按页建，映射多大页表就有多大（每 GB 2 MB）。当时以为真实程序很少碰到，实测 AddressSanitizer 之后发现不是这样，而且后果比 fork14 严重：

- Ubuntu 26.04 仓库里 `gcc -fsanitize=address` 编出来的程序一启动就映射约 14 TB 的影子内存（可写、`MAP_NORESERVE`；Linux 上没碰过的部分不占内存也不占页表）。tuxport 在这一次 mmap 里逐页建表，要建约 28 GB。
- 建表时持着 BKL，内核层的 OOM killer 是定时检查、也要 BKL，插不进来。结果是宿主的 OOM killer 把整个 `tuxport-run` 杀了（当时 7.7 GB），在 Android 上就是 LMK 杀掉整个 App，所有会话一起丢。一个进程的一次 mmap 带走了所有进程，违背了[稳健性](../design/12-threat-model.md)那几轮的设计。
- ASan、LSan 一个都用不了。TSan、MSan、Go 的 `-race` 也映射几 TB 的影子内存，应该一样（没试）。

这一轮让页表按需建立：大的映射 mmap 时不建表项，guest 第一次访问时再补。

## 做法

- **页表的操作**（`mmu.c`）：
  - 新增"只填空表项"（`tuxport_mmu_fill`：已有的表项，包括写时复制改过的、权限为 0 的，都不动）、"搬表项"（`tuxport_mmu_move`，mremap 用）、"找下一个有表项的页"（`tuxport_mmu_next_present`）。
  - 遍历一段范围（解除映射、改权限、搬、找）时，没有中间表的地方按缺的那一级整段跳过：一项覆盖 2 MB、1 GB 或 512 GB。原来只按 2 MB 跳，14 TB 要走 700 万次。
  - 每张叶子表只从根走一次，原来每页走一次。
  - 权限为 0 的表项（见下面的 `PROT_NONE`）`translate` 不认，要求的权限传 0 时也不算已映射。
- **按需建表**（`Mm::map_pages`、`Mm::fault_in`）：
  - 1 MB 以内的映射照旧建好表项，大的不建（`EAGER_MAX`）。文件的可执行映射也照旧建好：翻译共用时 DBT 按页表核对 guest 指令字（recheck），没有表项的块要停放，第一次执行时再一页页恢复（revive）。它只建到文件末尾：原来 mmap 先按整个映射的长度建、再拿掉文件末尾之后的部分（`trim_to_file`），小文件以可执行权限映射成 1 GB 也要建 2 MB 的页表；现在新建 VMA 时就带着文件长度。
  - guest 访问出错时，内核层先看是不是还没建表：页在某个 VMA 里、VMA 的权限允许这次访问、这一页没有表项，就把同一张叶子表（2 MB）里属于这个 VMA、在文件末尾之前、还没有表项的页一起建好，重新执行。顺序访问 1 GB 多 512 次缺页。
  - 内核层经由页表读写 guest 内存（`copy_from_user`、`copy_to_user`、读路径字符串、futex 的键、robust futex 链表）也一样先补。不持 BKL 的快速路径查不到时本来就交给持锁的路径，不用改。
  - DBT 恢复停放的块时，页没有表项（`host` 为空）就留着不作废：分发器取不到指令时由 Tier 0 报告缺页，内核层补上之后再来。原来直接作废，按需建表的匿名可执行映射里会白白重新翻译。
- **有内容的页一定有表项**：fork 借页、量驻留内存原来逐页查宿主（pagemap、mincore），14 TB 的映射要读 28 GB 的 pagemap、分配几 GB 的结果数组。现在只看有表项的叶子表连成的几段（`Mm::touched_windows`，每段不超过 1 GB）。这要求私有匿名内存里有内容的页一定有表项，逐个检查了不经过页表的地方：
  - mprotect 成 `PROT_NONE`：原来拿掉表项，现在表项留着、权限清零。新建的 `PROT_NONE` 映射仍然没有表项（V8 每块 wasm 内存预留 8 GB）。从没有权限改回有权限时，留着的表项改权限，其余的照常按需建。
  - 内核层直接写 VMA 的宿主内存（`write_kernel`：加载程序、初始栈、`PTRACE_POKEDATA`）之后补上表项，`PROT_NONE` 的权限为 0。
  - mlock、`MAP_POPULATE` 调入的页也补上表项。
  - mremap 搬走时表项一起搬，不再在新地址重建（重建只能建全部或者不建）。
  - 写时复制：借用方在 `PROT_NONE` 的映射里复制自己的一份时，表项改指自己的页（权限为 0），原来留着指向借来的页、权限为 0 的表项。fork 借出、mprotect 之后恢复只读，原来是"改权限"，表项可能还没有，改成"建表项"。借来的页在 `PROT_NONE` 的映射里没有表项，所以写时复制的页另外算进来。
- **fork、mremap 申请宿主内存时漏看了 `MAP_NORESERVE`**（测试时发现，原来就有）：mmap 时按 Linux 的规则不计入提交量，fork 给子进程申请、mremap 扩展、给共用的私有文件映射复制一份时却按"可写就计入"申请。宿主的 `vm.overcommit_memory` 是 0 时，一次申请超过内存加交换区就失败：有 64 GB `MAP_NORESERVE` 映射的进程 fork 是 ENOMEM，原生上成功。ASan 程序 fork 都失败。现在三处都照 mmap 的规则。
- **驻留内存**：`/proc/<pid>/status`、`statm`、`ru_maxrss`、OOM killer 选进程（`Mm::rss_plan`）和 smaps、OOM 的记录（新的 `Mm::resident_in`，原来数 `residency` 的逐页数组）对按需建表的大映射只量有表项的几段。`tuxport_hal::resident_pages` 分段 mincore，结果数组不超过 64 KB。

## 测试

- AddressSanitizer（Ubuntu 26.04 仓库里的 gcc 15，Tier 1）：堆越界写、释放后使用的报告和原生一样，LSan（用 ptrace 停住所有线程扫描内存）、fork、多线程也正常；原来整个 `tuxport-run` 被宿主的 OOM killer 杀掉。
- LTP 的 fork14 通过（原来 TBROK）：它一直映射到 16 TB，fork 在第 4 次之后按提交量失败，映射本身不再占页表。
- 有 64 GB `MAP_NORESERVE` 映射的进程 fork，原生和 tuxport 都成功、子进程读到内容。
- 单元测试：引擎（`tests/mmu.rs`）测页表的新操作和跳过空洞；内核层测按需补表、驻留内存和 fork 只看碰过的页，写过的页在 `PROT_NONE`、内核层直接写、mremap 之后都还有表项。
- 差分测试 `fork` 组新增一项：1 TB 的 `MAP_NORESERVE` 映射只碰几页，fork、`PROT_NONE` 再改回来、`MREMAP_FIXED` 搬走之后的内容和 VmRSS。
- DBT 恢复停放的块时页没有表项就留着：没有单独的测试（文件的可执行映射一次建好表项，平时碰不到）。
- 性能：python、gcc、shell、git、node、java、go build、Maven 和 Gradle 的耗时在噪声以内。大映射里只碰一点的微基准：

  | | 原生 | HEAD | 这一轮 |
  |---|---|---|---|
  | mmap 2 GB、碰一页、munmap | 0.01 ms | 2.39 ms | 0.01 ms |
  | 映射了 2 GB、碰过 1 MB 时 fork 加子进程退出 | 0.09 ms | 4.06 ms | 0.15 ms |

## 推迟项

- 不少 Android 设备的用户地址空间只有 39 位（512 GB）。tuxport 每个 guest 映射对应一段同样大的宿主映射，14 TB 的影子内存在宿主上映射不出来，ASan 在这些设备上仍然不能用：启动时报"failed to allocate … Perhaps you're using ulimit -v"退出，同一个会话里别的进程照常（在 docker 里用 `ulimit -v` 把宿主限制在 512 GB 模拟）。
  - 试过让 guest 的地址空间也缩到 39 位（临时把栈顶改成 `1 << 39`）：ASan 按栈的位置认出 39 位，影子内存只要约 48 GB，但它的分配器放不下——gcc 13、14 的固定在 `0x5000_0000_0000`，gcc 15 的要 4 TB（"heap size 40000002000 exceeds max user virtual address 7fffffffff"），第一次 malloc 就失败。原生的 39 位内核上也一样，这些 libasan 不支持 39 位。所以缩小 guest 的地址空间没用，反而让需要高地址的程序（固定地址映射）也用不了。
  - 能让 ASan 在这些设备上用的只有让 guest 的映射不再一对一占宿主地址空间：`PROT_NONE` 的预留不占，宿主内存分块、用到时再申请。VMA 对应一段连续宿主内存的假设在 mm.rs 里到处都是（按需建表、fork 借页、内核直接写、丢弃、mremap、fork 的小映射切块），Tier 1 的跨页访问也依赖它，改动大，等有需求再说。→ `PROT_NONE` 的预留不占宿主地址空间了（[39 位的宿主地址空间](199-reserved-mappings.md)，为了 Chromium：它在 512 GB 里根本起不来）；Tier 1 的跨页访问在宿主地址不连续时本来就退回慢路径，不受影响。ASan 的影子内存是可写的 `MAP_NORESERVE`，仍然一对一，宿主内存分块、用到时再申请没做，ASan 在 39 位的设备上仍然不能用
- 文件的可执行映射仍然一次建好表项：把一个稀疏的大文件以可执行权限映射进来仍然会建很大的页表。
- 叶子表在地址空间销毁之前不释放（原来就是），munmap 之后留着空表。只碰每 2 MB 里一页的程序，页表和它碰过的内存一样多（各 4 KB），OOM killer 看得到。
- 按需建表的缺页要取 BKL：多线程程序碰新内存时排队，每 2 MB 一次。
