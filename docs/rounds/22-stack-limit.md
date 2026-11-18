# M9 之前：栈的大小上限（2026-10-26）

原来 execve 总是映射固定 8 MB 的主线程栈，`RLIMIT_STACK` 只是记下来的一个数：`ulimit -s unlimited` 返回成功但没有效果，深递归的程序（刷题时常这样用）照样在 8 MB 处段错误，调小也不起作用。这一轮改成与 Linux 一样按需增长（§5.4）。

## 做法

- **按需增长**：execve 只映射参数、auxv 下面再多 128 KB（Linux `setup_arg_pages` 的 `stack_expand`），栈的 VMA 标成向下增长，guest 访问栈底下面的地址出错时延伸到那一页再重新执行，沿用上一轮给 `MAP_GROWSDOWN` 做的路径。延伸之后的大小按 `RLIMIT_STACK` 的当前值检查（Linux 的 `acct_stack_growth`），所以运行中 setrlimit 调高、调低都立即生效；`MAP_GROWSDOWN` 的映射也受它限制。
- **宿主内存**：上一轮的 `MAP_GROWSDOWN` 每长一次就是一个新的 VMA（一个宿主映射），主线程栈一页一页地长下去，几百 MB 就会用完 `max_map_count`。
  - 现在 execve 时在栈下面预留到 8 MB 宿主地址空间（`MAP_NORESERVE`），在这个范围里增长只是加长同一个 VMA。超出之后另外申请一段，下面再预留到与栈现在的大小相同，VMA 的个数按对数增加（1 GB 的栈是 8 个）。
  - `/proc/<pid>/maps` 把这几段合成一行 `[stack]`：与 Linux 一样按初始栈指针（`start_stack`）认出栈，原来是按固定的起点。
  - fork 时子进程照样保留栈下面的预留。fork 原来不把向下增长的标记复制给子进程，子进程里的 `MAP_GROWSDOWN` 映射不会增长，一起修正了。
- **内核访问**：栈不再预先映射 8 MB 之后，内核写进栈底下面的缓冲区会得到 EFAULT（上一轮的推迟项）。内核访问 guest 内存的地方有几十处，都只拿着地址空间的只读引用，不方便就地增长，所以只在两处补上：
  - syscall 入口发现 SP 已经低于栈底（程序分配了大的栈帧还没碰）时，先把栈延伸到 SP。arm64 没有 red zone，syscall 用到的栈上缓冲区都在 SP 以上。主线程上这个检查只比较一次 SP 和栈底；别的线程的 SP 在 mmap 区里，再比较一次上限就停。
  - 构造信号帧之前延伸到帧的起点。
- **布局**：mmap 区的上界原来固定在栈顶往下 1 GB 加 8 MB，现在与 Linux 的 `mmap_base` 一样按 execve 时的 `RLIMIT_STACK` 留出间隔：至少是原来的大小（默认布局不变），最多地址空间的 5/6。不限栈大小时 Linux 改用自底向上的 legacy 布局，这里仍然自顶向下，间隔取最大值，ld-musl 从 `0x7fff_bf7…` 移到 `0x1555_5549…`。
- **参数大小**：execve 的参数上限原来固定是 2 MB，现在按 Linux 的 `bprm_stack_limits`：栈大小上限的 1/4，不超过 6 MB，不少于 128 KB。setuid、setgid 程序的栈大小上限与 Linux 一样压到 8 MB，并写回进程的 `RLIMIT_STACK`。

## 结果

与上一轮的 release 二进制交替运行、各 5 次：100 万次 `getppid` 在噪声之内；2000 次 fork 加 wait 从 240–243 毫秒降到 209–219 毫秒（栈的 VMA 从 8 MB 变成 132 KB，fork 查 pagemap 的范围小了）；第一次用到 6 MB 栈从 1.1–1.3 毫秒变成 1.8–2.1 毫秒（约 1500 次增长，每次 0.4 微秒左右）。

## 测试

- 内核层单元测试新增栈的就地增长、按倍数申请、maps 里合成一行、上限的计算、fork 之后的增长和 mmap 区的上界；原来的 `MAP_GROWSDOWN` 用例加上 fork 之后的增长。
- 新的 guest 程序 `tests/guest/stack.c` 在两个执行层下检查 SP 挪到栈底下面之后的 read 和信号处理，以及不同上限下的递归深度和运行中调整上限。
- rootfs 用例新增一个：初始的 `[stack]` 大小、`ulimit -s` 对参数上限的影响、`ulimit -s unlimited` 之后的布局。

## 推迟项

- 内核别的地方访问栈底下面时不会让栈增长，返回 EFAULT（Linux 上会增长）：指向 SP 下面的缓冲区（arm64 的 ABI 不让程序用 SP 下面的内存，只有故意这样做的程序会碰到），以及 `/proc/<pid>/mem`、`process_vm_writev` 访问别的进程的栈。→ 后两个接口本身还没有实现（2026-10-27 核对代码）；两个接口后来都补上了（M9 之前：ptrace 的第一步、第二步），访问别的进程的栈底下面仍是 EFAULT
- 不限栈大小时没有用 Linux 的 legacy 布局（见上）；mmap 的地址提示仍然只在 mmap 区的上界以下才采用。
- 没有 `/proc/<pid>/limits`。→ 已补上（M9 之前：平均负载、sysfs 和 limits）
