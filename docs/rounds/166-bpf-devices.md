# M9 之前：cgroup 的设备过滤（eBPF）（2026-11-01）

cgroup v2 没有 v1 的 `devices.allow`：runc、crun 给每个容器的 cgroup 挂一个 eBPF 程序（`BPF_PROG_TYPE_CGROUP_DEVICE`），容器里打开设备节点、mknod 时内核跑它决定放不放行。[cgroup 树](163-cgroup-tree.md)之后 docker 停在 `bpf_prog_query(BPF_CGROUP_DEVICE) failed: function not implemented`，podman（crun）停在 `bpf create`：runc 不在用户命名空间里、规则里有拒绝时不能跳过这一步，crun 也一样。这一轮做 bpf(2) 的一个子集，只有这一种程序，照 Linux 6.1 的 `kernel/bpf/syscall.c`、`kernel/bpf/cgroup.c`、`kernel/bpf/verifier.c`。

## 做法

- **bpf(2)**（`syscall/bpf.rs`）：都要 `CAP_BPF` 或 `CAP_SYS_ADMIN`（`kernel.unprivileged_bpf_disabled` 默认是 2）。`bpf_attr` 按 6.1 的长度（144 字节）：给得更长时多出来的要是 0（否则 E2BIG，超过一页也是），每个命令用到的最后一个字段之后要是 0（`CHECK_ATTR`，EINVAL）。
  - `BPF_PROG_LOAD`：只有 `BPF_PROG_TYPE_CGROUP_DEVICE`，别的类型是 EINVAL；指令数 0 或者超过一百万是 E2BIG；名字只能有字母、数字、`_`、`.`；许可证判断是否与 GPL 兼容；校验器的日志缓冲区给了就要合理（至少 128 字节、级别有效），被拒绝时把原因写进去。得到的 fd 是 `anon_inode:bpf-prog`，close-on-exec，读写是 EINVAL。程序有全局递增的编号、tag（指令的 SHA-1 的前 8 个字节，与 Linux 的 `bpf_prog_calc_tag` 相同）、加载时刻、加载者。
  - `BPF_PROG_ATTACH`、`BPF_PROG_DETACH`：目标是 cgroup2 目录的 fd（O_PATH 的也行，不是的是 EBADF），挂的类型只有 `BPF_CGROUP_DEVICE`。规则照 `__cgroup_bpf_attach`、`__cgroup_bpf_detach`：`ALLOW_OVERRIDE` 和 `ALLOW_MULTI` 不能同时给，`REPLACE` 要带 `ALLOW_MULTI`（EINVAL）；祖先上不带这两个标志挂了程序时子 cgroup 不能再挂（EPERM，`hierarchy_allows_attach`）；已经挂着的标志不同是 EPERM；最多 64 个（E2BIG）；`ALLOW_MULTI` 时同一个程序不能挂两次（EINVAL），要替换的没挂着是 ENOENT；不带的替换原来那一个。取下时带 `ALLOW_MULTI` 挂的要给出程序（EINVAL），没挂着是 ENOENT；不带的取下唯一的那个，fd 不是程序也行。
  - `BPF_PROG_QUERY`：要 `CAP_NET_ADMIN`。写回标志（只查有效的时是 0）和个数，放不下时只写放得下的、返回 ENOSPC（runc 照个数重试）；`BPF_F_QUERY_EFFECTIVE` 查子 cgroup 实际要跑的。
  - `BPF_PROG_GET_FD_BY_ID`（要 `CAP_SYS_ADMIN`，没有是 ENOENT）、`BPF_OBJ_GET_INFO_BY_FD`（6.1 的 `bpf_prog_info`：类型、编号、tag、名字、GPL、加载时刻、加载者、指令；不是程序是 EINVAL，fd 不存在是 EBADFD）。runc 用它们取已经挂着的程序。
  - 别的命令（map、pin 到 bpffs、link……）是 EINVAL。cilium/ebpf（runc 用的库）探测特性时建 map 失败就不给程序起名，加载 CGroupSKB 失败就认为不支持 `BPF_F_REPLACE`，改成逐个取下旧的，都不影响加载设备程序。
- **校验器**（`bpf.rs` 的 `verify`）：先看控制流（`check_subprogs`、`check_cfg`）：跳转出界、跳进 `LD_IMM64` 的后一半、往回跳、最后一条之后还会接着执行、有走不到的指令都是 EINVAL。再按程序的顺序检查每条指令（`do_check`），走到同一条的几条路径在那里汇合：寄存器类型不同就当作没有初始化，标量只记无符号的范围，条件跳转与常数比较时收窄（收窄成空的那一边不检查，与 Linux 剪掉死分支一样）。错误码与 Linux 相同：没有初始化的寄存器、写 r10、读写越界或者没对齐的上下文和栈、写上下文、对指针做别的运算是 EACCES；不认识的指令、保留字段不是 0、除以常数 0、移位超出位宽、返回值不是 0 或 1、返回指针、调用别的 helper 是 EINVAL。上下文是 `struct bpf_cgroup_dev_ctx`（access_type、major、minor），只读，只有 access_type 可以读一部分（`cgroup_dev_is_valid_access`）；特权的程序可以读没写过的栈。能调用的 helper 是 6.1 给设备程序、不要 map 的那几个（ktime_get_ns、get_prandom_u32、get_smp_processor_id、get_current_uid_gid、get_numa_node_id、get_current_cgroup_id、ktime_get_boot_ns）。
- **解释器**（`run`）：校验过的程序直接解释，ALU 的语义照 `___bpf_prog_run`（32 位的高位清零，除以 0 得 0、对 0 取模不变）。上下文和栈在解释器里各占一段"地址"，访存按落在哪一段找；只往前跳，最多执行指令数那么多步。
- **检查设备**（`bpf::check_device`）：跑调用者所在的 cgroup 的有效程序（`compute_effective_progs`：自己的；已经有了之后，祖先的只在它带 `ALLOW_MULTI` 挂的时候算），有一个返回值的最低位是 0 就是 EPERM。打开仓库里的设备节点时在 DAC 检查之后（`devcgroup_inode_permission`，读、写按打开的方式），devpts 的终端、ptmx 也是；mknod 字符设备、块设备在 `CAP_MKNOD` 之后以 MKNOD 访问检查（whiteout 0:0 不查）。
- **伪文件从偏移 0 再读时重新生成**：docker 能起容器之后，`docker pause` 报 `expected "cgroup.freeze" to be in state "FROZEN" but was in "THAWED"`。runc 以读写方式打开 `cgroup.freeze`，写 1，seek 回 0 再读确认；tuxport 的 procfs、sysfs、cgroup2 文件的内容只在打开时生成，读到的还是打开时的 0。Linux 的 seq_file 从偏移 0 读时都重新生成（`seq_read_iter`），procps 这样一直开着 `/proc/stat` 再 seek 回 0 读的程序也靠它。改成一样：打开时生成的内容第一次读还用它（不白生成两遍），写过之后、再从 0 读（read 或者 pread）时重新生成。

## 测试

- 新的差分测试组 `bpf-devices`（原生一侧是特权容器）：加载 runc 那样的程序，挂上前后在 cgroup 和子 cgroup 里打开设备、mknod 的结果；挂、替换、取下、查询、取信息的各种标志和错误；校验器拒绝的十九种程序。错误码逐个与原生相同，tag 也相同（SHA-1 和指令的编码都对）。
- `cgroup-freeze`、`edge-cases` 组新增伪文件写了之后、进程改名之后从 0 再读。两个执行层下都与原生逐字相同。`bpf.rs` 的单元测试测设备程序、校验器和 SHA-1。
- LTP 的 bpf_map01、bpf_prog01–04 由 CONF 变成 FAIL（TBROK）：原来 bpf 是 ENOSYS，LTP 当作不支持；现在有了 bpf，它们要的 map 和 socket 过滤程序是 EINVAL（见推迟项）。
- 在 tuxport 里实际跑：`docker run --network host` 和不加 `--cgroups disabled` 的 podman（`cgroup_manager = "cgroupfs"`）都起得来，容器里的内存、任务数上限起作用，`pause`、`unpause` 成功。

## 推迟项

- 只有设备程序这一种：没有 map、pin 到 bpffs、link（`BPF_LINK_CREATE`）。crun 用 systemd 管 cgroup 时要把程序 pin 到 `/sys/fs/bpf`，这里只能用 cgroupfs；要 map 的程序（cilium、bpftrace 这些）都用不了。
- 校验器比 Linux 的保守：不分路径（汇合处类型不同就当作没有初始化），没有有界循环（5.3 起）、BPF 之间的调用、原子操作、把指针存进栈、变量偏移的栈指针。Linux 接受的这类程序这里会被拒绝；runc、crun 生成的用不到。校验器的日志只在拒绝时写一行原因，措辞不全与 Linux 相同，成功时不写。
- `BPF_OBJ_GET_INFO_BY_FD` 没有 JIT 的信息（`jited_prog_len` 是 0）和运行统计；`/proc/<pid>/fdinfo` 里没有 `prog_type`、`prog_tag` 这些行。
- faccessat 不过设备过滤（Linux 的 `inode_permission` 也查，`access("/dev/xxx", R_OK)` 在容器里会是 EPERM）。
- 6.8 起设备程序还能用 `get_current_pid_tgid` 等 helper，这里照 6.1 没有。
- 没有 map、socket 过滤程序，LTP 的 bpf_map01、bpf_prog01–04 从"不支持"（CONF）变成了失败（TBROK）。
- 伪文件打开之后第一次读用的是打开时生成的内容，Linux 在第一次读时才生成；打开之后过了很久才读的程序会看到稍旧一点的内容。
