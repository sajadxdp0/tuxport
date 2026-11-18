# M9 之前：已经删除的文件带 ` (deleted)`（2026-10-29）

[第 93 轮](93-fd-paths-resolve-symlinks.md)的推迟项之一：打开着、映射着的文件被删除之后，Linux 的 `d_path` 在路径后面加上 ` (deleted)`，tuxport 没有（只有 memfd 的名字本来就带着）。用到它的有两类程序：

- needrestart：apt 升级时 dpkg 用 rename 换掉旧的库，needrestart 按 maps 里的 ` (deleted)` 找出还在用旧库的进程；
- lsof：靠它标出已经删除、还开着的文件。

## 做法

改在 `core/kernel/src/pseudo.rs`：

- `/proc/<pid>/fd/<n>` 指向普通文件时，以及 maps、smaps 里文件映射的名字，在文件已经删除时加上 ` (deleted)`。
- 怎么判断已经删除：仓库里删掉最后一个名字之后，inode 的行以 nlink = 0 留到下次独占打开仓库时（§8.1），所以查的是元数据缓存里的 nlink。
- 名字已经以 ` (deleted)` 结尾的（memfd）不再加。

## 结果

生成 maps、smaps 时，每个文件映射查一次元数据缓存。用 `target/mem/procbench.c` 读 `/proc/self/maps`：

- 进程有 6 个文件映射时，读 2 万次 0.127–0.133 → 0.135–0.137 秒；
- 再多映射 300 个文件区域、读 2000 次，0.211–0.213 → 0.215–0.218 秒，每次多约 2.5 µs（+2–3%）。

## 测试

- cargo test 新增 `procfs_marks_deleted_files`。
- 差分测试 compat 组新增一个 C 程序：打开之后删除、映射之后删除、映射着的文件被 rename 盖掉、memfd，fd 链接和 maps 里的名字与原生逐行相同。

## 推迟项

- 按 nlink 判断与 Linux 不同的地方：
  - Linux 看的是打开或映射时那个名字被删掉了没有，只删了这个名字、别的硬链接还在时也加；这里不加。
  - Linux 上文件改了名路径会跟着变，这里不变（[第 93 轮](93-fd-paths-resolve-symlinks.md)的推迟项）。
- `/proc/<pid>/exe` 没有加。tuxport 的 exe 链接按读出来的文字跟随，Linux 的是直接指向那个文件的链接。加了后缀，经由 `/proc/self/exe` 重新 exec 自己的程序会得到 ENOENT。要加的话，exe 先得改成与 fd 一样指向打开文件本身的链接，执行中的程序文件被删除之后也要留着它的 object。→ 已补上（M9 之前：/proc/<pid>/exe 指向程序文件本身）：exe 指向程序文件本身，删除之后带 ` (deleted)`，经由它打开、执行的仍是原来那个（靠记着的宿主文件）
- core dump 按 `coredump_filter` 决定写不写共享映射时，Linux 把已经删除的文件的共享映射当作匿名共享内存（`vma_dump_size` 看 `i_nlink`），这里只认出 memfd。
