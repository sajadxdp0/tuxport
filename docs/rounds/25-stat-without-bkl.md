# M9 之前：不持 BKL 的 stat（2026-10-26）

上一轮的推迟项：并行的 lstat 被 BKL 串行化，`git status` 开着并行 lstat（git 的默认）比单线程还慢；`go build`、JVM、node 的线程池也会并行 stat。

## 做法

涉及 §6、§8.1。

- 仓库的元数据缓存挪进 `store::Meta`（`Store` 和不持 BKL 的线程共用的 Arc）。持 BKL 的线程是唯一修改缓存的一方，修改时持有读写锁的写锁，自己读时不加锁；别的线程持读锁读。所以持 BKL 的路径没有多出加锁的开销，只有填缓存、作废时取写锁。
- 只用缓存的路径解析（`vfs::Cached`）与 `Vfs::walk_in` 一一对应：不检查搜索权限（只给 root 用），进入挂载点或者缺了什么就返回 None，交回正常路径，由它填进缓存。还没处理的组件放在栈上的定长数组里，不分配内存；object 的名字也在栈上拼（`hal::stat_at` 改收 `&CStr`，原来每次都分配一个 CString）。普通文件在持读锁时向宿主 stat object：inode 编号删除后被重新使用时，结果不会一半来自旧文件。
- 运行循环在 svc 退出之后、取 BKL 之前先试 `syscall::fast`：只处理 newfstatat 和 statx，要求凭据有 `CAP_DAC_READ_SEARCH` 或 `CAP_DAC_OVERRIDE`、`AT_FDCWD` 或绝对路径、没有在跟踪系统调用（`TUXPORT_STRACE`）、没有待作废的 `ic ivau`，参数不合法时也交回正常路径（由它报 `EINVAL`、`EFAULT`）。guest 内存经由 Cpu 绑定的页表访问（新加的 `Cpu::translate`），期间线程仍然标记着在执行 guest 代码，解除映射的线程等它用完再释放宿主内存。挂起的信号和 kick 留到下一次进入 guest 代码时处理：kick 的标志还在，`dbt_run` 立即退出。
- 线程记下自己的根目录、当前目录和凭据（`Fast`），每次持 BKL 的系统调用结束时看一个全局计数变了没有，变了就重新记下。chdir、fchdir、chroot、unshare(`CLONE_FS`)、setuid 一类和 setuid 程序的 execve 让计数加一：当前目录由 `CLONE_FS` 的线程共享，凭据由线程组共享，都可能被别的线程改掉，过时的线程退回正常路径。

## 结果

微基准 `target/mem/pstat.c`：每个线程对 8 个路径（含一个不存在的）轮流 lstat 20 万次；原生一侧的根目录是 docker 的 overlayfs，每一级都要经过它，tuxport 只对 objects 目录做一次单级 fstatat，所以单线程比原生还快，不完全可比：

| 负载 | 原生 | 之前 | 之后 |
|---|---|---|---|
| `git status`（5733 个文件），单线程 / 并行 lstat | 16 / 10 毫秒 | 18–19 / 24–25 毫秒 | 18 / 13–14 毫秒 |
| 并行 lstat 微基准，1 / 2 / 4 个线程，每次的微秒数 | 0.60 / 0.68 / 1.2 | 0.65 / 1.5 / 4.1 | 0.55 / 0.63 / 0.9 |
| `go build` hello，冷缓存 | 2.2s | 7.07s | 6.82–7.01s |
| Maven / Gradle 热构建 | 1.7 / 0.42s | 7.55 / 1.71s | 7.47 / 1.67–1.71s |
| shell 起 600 个进程；gcc 编译 `lvm.c`；python | 0.15 / 0.69 / 0.36s | 0.33 / 2.35 / 1.54s | 0.32 / 2.29 / 1.51s |

单线程的 `git status` 没变：不持 BKL 的路径省下的取锁、建 `Cx`、克隆凭据，和宿主 fstatat（占这条路径的七成）相比不多。并行时 BKL 的自旋从 10% 降到 0.8%，futex 唤醒从 6.3% 降到 0.8%，剩下的原子操作（读写锁、引用计数）合计约 2%。`go build` 里 BKL 已经不是瓶颈（`dispatch` 合计 3.4%，锁争用 0.2%），86% 的时间在翻译后的代码里。

## 测试

- 单元测试：用正常的路径解析填好缓存之后，只用缓存解析各种路径，结果和正常解析完全相同；挂载点、没解析过的名字、删除之后都交回正常路径。
- 差分测试 c-programs 组新增一个程序：一个线程交替 chdir、改名、改凭据，别的线程马上 stat，必须看到新的状态、拿到一致的 inode。第一版去掉 chdir 和改凭据处的计数加一也发现不了（变异检验）：检查线程记下的旧当前目录里从没查过这些名字，缓存总不命中，结果总走正常路径；改成先在起始目录里查一次同名的文件。

## 推迟项

- 带 dirfd 的 fstatat、fstat、readlinkat、faccessat 仍持 BKL：dirfd 和 fstat 要不持 BKL 读 fd 表（`Rc<RefCell<FdTable>>`），readlinkat、faccessat 可以照搬，但还没看到哪个负载并行地大量调用它们。
- 非 root 的进程仍持 BKL stat：要检查每一级目录的搜索权限，凭据也要能不持 BKL 读。
