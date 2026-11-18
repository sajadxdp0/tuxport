# M9 之前：core dump（2026-10-27）

[ptrace 的第二步](41-ptrace-gdb.md)之后 gdb 能调试运行中的程序，但程序崩溃之后没有 core 文件，不能事后分析；`WCOREDUMP` 总是 0，LTP 的 abort01、kill11、waitid10、waitpid01 因此失败（[M6](08-m6.md) 起推迟）。这一轮按 Linux 的 `do_coredump`、`elf_core_dump` 写 core 文件（§7.7）。

## 做法

- **什么时候写**：默认动作是生成 core 的信号（SIGQUIT、SIGILL、SIGTRAP、SIGABRT、SIGBUS、SIGFPE、SIGSEGV、SIGXCPU、SIGXFSZ、SIGSYS）结束进程，而且是它让线程组开始退出的（别的线程已经在 exit_group 时不写）。与 Linux 一样，`RLIMIT_CORE` 小于一页、进程不可 dump（`PR_SET_DUMPABLE`、setuid 程序）时不写。默认的软限制是 0，要先 `ulimit -c`。
- **其他线程的寄存器**：信号所在的线程让线程组退出之后等其他线程退出（与 execve 结束其他线程一样等 `wake::EXITED`），它们在 `task::exit` 里把寄存器、挂起的信号、屏蔽字和 CPU 时间留在线程组里（`ThreadGroup::core`）。阻塞在系统调用里被结束的线程，原来停在 svc 的下一条、x0 是 `-ERESTART*`；与 Linux 的 `do_signal` 一样，现在先按"重新执行"把 pc 退回 svc、x0 恢复成原来的参数再结束，core 文件和 `PTRACE_EVENT_EXIT` 看到的都是这样（gdb 里这些线程停在 musl 的 `__syscall_cp_asm`，原来是 `__cp_end`）。
- **文件名**：按 `/proc/sys/kernel/core_pattern`（原来只读出 `core`、写入不起作用，现在可以设置，最长 127 字节）展开 `%p`、`%i`、`%u`、`%g`、`%s`、`%t`、`%h`、`%e`、`%E`、`%c`、`%d`，新加的 `core_uses_pid` 为 1 而模式里没有 `%p` 时加上 `.<pid>`；相对进程的当前目录，先删掉同名的文件，再以 0600、`O_EXCL | O_NOFOLLOW` 新建（`fs` 拆出内部用的 `create_exclusive`、`unlink_path`、`write_at`）。管道形式（`|程序`）不支持，不写。
- **格式**与 arm64 Linux 写的相同：
  - 每个 VMA 一个 `PT_LOAD`，写多少按 `/proc/<pid>/coredump_filter`（原来只读出 0x33，现在可以设置、fork 继承）和 Linux 的 `vma_dump_size`（`Mm::dump_regions`）：vDSO 总是写；写过的私有匿名映射、匿名的共享内存（含 memfd、SysV 共享内存）整个写，没碰过（不驻留）的页留空洞；从来没有可写过的预留（`PROT_NONE` 的 wasm 区、Go 的 arena）不写；私有文件映射可写过的（数据段）整个写，否则只写 ELF 文件的第一页；文件的共享映射默认不写。按页读内容不检查权限（`PROT_NONE` 的页按 VMA 读宿主内存），写时复制借来的页按页表读拥有者的，文件末尾之后的页留空洞（`Mm::dump_page`）。
  - `PT_NOTE`：每个线程依次是 `NT_PRSTATUS`（392 字节：信号、挂起的信号和屏蔽字、pid、CPU 时间、x0–x30/sp/pc/pstate）、`NT_PRFPREG`、`NT_ARM_TLS`、`NT_ARM_HW_BREAK`、`NT_ARM_HW_WATCH`、`NT_ARM_SYSTEM_CALL`（内容与 PTRACE_GETREGSET 读到的相同），产生信号的线程在最前面，它的 `NT_PRSTATUS` 之后接着进程的 `NT_PRPSINFO`（状态、凭据、comm、命令行）、`NT_SIGINFO`、`NT_AUXV`、`NT_FILE`。程序头超过 65535 个时与 Linux 一样用 `PN_XNUM` 和一个节头。
  - 与 Linux 的 `dump_emit` 一样，写出的字节（空洞不算）到了 `RLIMIT_CORE` 就截断，截断了也算写了（Linux 在开始写之前就记下 `has_dumped`）：waitpid01 把限制设得比 core 小，原来按 EFBIG 整个不写，它仍然失败。
- **报告**：写成了的进程 `ExitStatus::Signaled` 带 `core_dumped`，wait 的状态带 `WCOREDUMP`（0x80），SIGCHLD 和 waitid 的 `si_code` 是 `CLD_DUMPED`。
- **`MADV_DONTDUMP`、`MADV_DODUMP`**：原来接受但不起作用，现在记在 VMA 上（fork 继承），core 里不写，smaps 的 VmFlags 带 `dd`。

## 结果

热路径上只多了线程退出时看一眼线程组在不在写 core、信号投递时先做重启的准备再看线程是否被结束，没有单独测量。

## 测试

rootfs 测试新增两项：busybox 的 shell 里留下 core、`core_pattern` 和 `coredump_filter`；`tests/guest/coredump.c` 检查多线程进程的 core 文件的内容、`RLIMIT_CORE` 的截断和 wait 报告的状态。单元测试新增 note 的对齐和程序头的位置。差分测试没有加：原生一侧在 docker 里，core 写到哪里取决于宿主内核的 `core_pattern`，CI 的 Ubuntu 宿主上是 apport 的管道，容器里改不了。LTP 新通过 abort01、kill11、waitid10、waitpid01，以及原来跳过的 madvise08（检查 `MADV_DONTDUMP` 的内存不进 core 文件）。

与原生对照：一个三线程的程序在嵌套调用里写空指针（`target/mem/coredump/compare.sh`），本地 colima 宿主的 `core_pattern` 是 `core`，原生和 tuxport 各生成 core，gdb 读出的调用栈、线程、变量、`$_siginfo` 都一致（地址和 LWP 号归一化之后）。

## 推迟项

- 管道形式的 `core_pattern`（`|程序`）。
- Linux 写 core 时连同共用地址空间的别的进程（vfork 的父进程、`CLONE_VM` 的子进程）一起结束，tuxport 只结束自己的线程组。→ 这一条记错了：那是 Linux 5.16 之前的行为，5.16 起（`coredump: Limit coredumps to a single thread group`）只结束自己的线程组，tuxport 报告的 6.1 也是这样，现在的做法与 Linux 相同（2026-10-27 核对）
- 读内存持着 BKL（写文件时放开），几 GB 的进程写 core 时别的进程要等。
- `NT_PRPSINFO` 的 `pr_flag`（Linux 的 `PF_*`）只有可以推断的几个位。
