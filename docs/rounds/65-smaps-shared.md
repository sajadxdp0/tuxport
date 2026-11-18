# M9 之前：smaps 里共用的页（2026-10-28）

[负载普查](62-workload-survey.md)的问题 3：redis 8.8 和 valkey 9.0 启动就退出，日志说 "Your kernel has a bug that could lead to data corruption during background save"。

## 原因

arm64 Linux 有过一个 bug（`arm64: pgtable: Ensure dirty bit is preserved across pte_wrprotect()`），fork 时页表项的脏位可能丢掉，后台保存（fork 之后写 RDB）会写出坏数据。redis、valkey 在 arm64 上启动时检查它（`ARM64-COW-BUG`）：一页写过、`MADV_FREE`、再写，fork 之后子进程读 `/proc/self/smaps` 里那一页所在映射的 `Shared_Dirty`，是 0 就认为内核有这个 bug，退出（可以用 `ignore-warnings ARM64-COW-BUG` 跳过，但没人知道要这样做）。tuxport 的 smaps 把匿名内存的驻留页都算作私有的脏页，`Shared_Dirty` 总是 0。

## 做法

- `Mm::cow_shared` 统计一段地址里和别的地址空间共用的页：写时复制借出的、借来的，按借出记录数映射着它的地址空间（拥有者还在的话算一个，加上每个借用方），与 Linux 的 mapcount 一样；只剩一个借用方的孤儿页只属于它，不算。它同时给出按共用的个数分摊之后的字节数。
- smaps 和 smaps_rollup 里私有匿名映射的这些页算 `Shared_Dirty`，其余仍算 `Private_Dirty`；Pss、Pss_Dirty、Pss_Anon 按分摊的字节累加，最后才换成 kB，与 Linux 相同。共用的页数不超过驻留页数（换出到 zram 的不算驻留）。

## 测试

- 差分测试 `fork` 新增 smaps 里共用的页：照 redis 的检查做一遍，再看子进程写过那一页之后。
- 单元测试 `cow_shared_counts_pages_mapped_by_others`：父子、孙进程共用时的分摊，写过之后，拥有者销毁之后的孤儿页。
- 普查的 `redis`（后台保存、AOF）、`valkey` 都能启动。valkey 改用自己的数据目录：原来读到 redis 8 写的 RDB 第 14 版，不认识就退出。

## 推迟项

- smaps 仍看不出页有没有被写过：匿名内存的驻留页都算脏页，文件映射都算干净页，写过的私有文件映射也是。`Referenced`、`Swap` 等仍是近似。
