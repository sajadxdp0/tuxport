# M9 之前：翻译表里漏掉的墓碑（2026-10-28）

[第二轮普查](70-survey-suites.md)的问题 7：Ruby 的测试 `-j4` 一起跑时工作进程不再推进。[上一轮](71-thread-exit-race.md)修完宿主堆的破坏之后，测试跑完了，但留下 3 个进程各占满一个 CPU，tuxport-run 一直不退出。

## 现象

用 gdb 看这 3 个宿主线程，都停在分发器的 `table_relink` 里（把停放的块放回翻译表，[node 启动](34-node-startup.md)那一轮加的），线性探测找空位的循环一直转。把其中两张翻译表读出来：131072 项一个空位都没有，墓碑分别有 12.1 万、11.4 万项；而计数里占用的只有 7.8 万项、墓碑 6.8 万项，还没到扩表或原样重建的四分之三。这个循环持着翻译的锁，不检查 kick，SIGKILL 也结束不了这些进程。上一轮的 `-j4` 四个工作进程不推进，多半也是这样。

## 原因

翻译表开放寻址，分发器不持锁查表，所以作废只把 pc 改成墓碑、插入只用空位，墓碑等扩表或原样重建时才去掉。整体清空（`flush_locked`）不重建表，只按 `table_used` 记下的位置逐个清掉，计数归零。停放的块恢复时，`table_relink` 把它插到新的空位，`table_used` 改指新位置，停放时留下的那个墓碑就没有任何记录再指向它了：它还占着位置，计数却在下一次整体清空时归零，之后再也清不掉。从池里拿到同一个程序留下的翻译时，核对把还没映射的共享库里的块停放，动态链接器映射之后恢复；之后换了程序再整体清空，每来一回就漏掉一批墓碑。计数一直偏小，表也就一直不会扩大或重建。Ruby 的测试几分钟里起上千个进程，三张表都被这样占满了。

## 做法

恢复停放的块时，停放时留下的墓碑还在（这之后没有重建过表）就原样恢复，把 pc 写回去。代码地址没变，不持锁查表的线程读到恢复前还是恢复后的都对；占用数不变，墓碑数减一。这样表里每个不空的位置都有 `table_used` 记着，整体清空能全部清掉。

- 顺带修了一处编译错误：非 aarch64 Linux 宿主上编译的 `tuxport_dbt_invalidate` 桩的参数没有跟着[JIT 负载](35-jit.md)那一轮改了的签名一起改（少了 cpu），在 macOS 上直接编译引擎报类型冲突。CI 和 Android 都是 aarch64 Linux，编译不到这一支。

## 结果

用改好的二进制重跑被挡住的测试集，与原生对照：

| 负载 | 原生 | tuxport |
|---|---|---|
| Ruby 3.4，`-j4` | 2 分钟 | 479 秒，tuxport-run 正常退出（[上一轮](71-thread-exit-race.md) 1781 秒：3 个卡住的线程从第 3 分钟起就占了 4 个 CPU 里的 3 个）；10427 个测试，5 个失败、12 个错误；只在 tuxport 上出问题的是 test_require 的 3 个 FIFO 测试（问题 3）和 3 个 `*_memory_leak` 测试超时（只是慢） |
| ripgrep | 1242 个测试通过、3 个忽略 | 与原生相同（构建用了上次留下的结果） |
| Node.js 24，test/parallel | 5 个失败，6.7 分钟 | 13 个失败，742 秒：同样的 5 个；5 个被 OOM killer 结束（问题 5：test-runner-* 和 test-cli-eval 一次起几十个 node，一共 23 次）；另外 3 个是下面的新问题 |
| Go 1.26 标准库 | 250 个包全部通过 | 250 个包全部通过 |

Go 第一次重跑只用了 73 秒：上一轮崩溃之前已经通过的包的结果被 `go test` 缓存了。负载改成 `-count=1` 之后重跑，每个包都真的运行了（编译仍然用了缓存的结果，149 秒和原生的 5 分钟不能比）。

重跑新找到的问题（Node.js 的测试）：

1. **已经删除的目录里还能新建**。进程 chdir 进一个目录、把它 rmdir 之后，用相对路径 mkdir 成功了，Linux 上是 ENOENT（`may_create` 检查 `IS_DEADDIR`）。仓库的 `add_dentry` 只检查上级是不是目录，没看它的链接数是不是已经是 0。test-fs-mkdir 因此失败。
2. **对 AF_UNIX socket 的 fd 做 fchmod 返回 EACCES**。libuv 的 `uv_pipe_chmod` 先 fchmod socket 的 fd（Linux 上改的是 sockfs 里它自己的 inode，会成功），只有 EINVAL、EOPNOTSUPP 才退回按路径 chmod；tuxport 的 fchmod 只认仓库里的文件和终端的从设备，对 socket 返回 EACCES。Node 的 `listen({ path, readableAll, writableAll })` 因此失败（test-net-server-listen-path、test-cluster-listen-pipe-readable-writable）。

## 测试

引擎测试新增 `parking_and_flushing_over_and_over_keeps_free_slots`：在最小的表里让一个块反复停放、恢复、整体清空。在另一个线程里跑、限时 60 秒，免得改错了卡住。

## 推迟项

- 上面两个新问题，排在普查剩下的问题之后修。→ 已修好（M9 之前：删除的目录里不能新建，socket 可以 fchmod）
- 插入时找空位的循环持着翻译的锁，不检查 kick。表真的满了的话（这一轮之前就是这样），进程连 SIGKILL 都结束不了。现在计数准确了，表到四分之三就扩大或重建，满不了，所以没有另加兜底。
