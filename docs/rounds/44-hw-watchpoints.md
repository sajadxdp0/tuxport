# M9 之前：硬件断点和观察点（2026-10-27）

[ptrace 的第二步](41-ptrace-gdb.md)让 `NT_ARM_HW_BREAK`、`NT_ARM_HW_WATCH` 报告 0 个槽，gdb 的 `watch` 只能退回软件观察点：每执行一条指令都停下来让 gdb 比较一次值，在真实程序上基本不能用；`hbreak` 也不能用。这一轮实现硬件断点和观察点（§7.6）。

## 做法

- **槽和控制字**：与常见的 Cortex-A 核一样报告 6 个断点、4 个观察点（`dbg_info` 的调试架构仍是 ARMv8）。设置时按 Linux 的 `ptrace_hbp_fill_attr_ctrl`、`hw_breakpoint_arch_parse` 检查和规范化（`syscall::ptrace::hw_slot`）：停用的原样记下；启用的字节选择（BAS）必须是连续的一段，观察点的种类是读、写或者两者，断点只能是执行、4 字节，否则 EINVAL；起点是地址加上 BAS 的偏移，再按 8 字节（断点按 4 字节）对齐、把偏移挪回 BAS，特权级记成 EL0，读回的就是规范化之后的值。超出槽数的项不是零时 ENOSPC。
- **谁检查**：状态属于线程（`Trace::hw`），跟踪者让线程继续时交给 CPU（`HwDebug::apply` → `CpuState::set_hw_debug`，引擎的 `struct tuxport_cpu` 末尾多了断点地址和观察点的 [起点, 终点)、种类）。只有解释器检查：每条指令执行之前比较断点；每次访存在页表检查之后、产生副作用之前比较观察点（`watch_hit`），普通读写、原子操作、独占访问、LDAR/STLR、SIMD 的多寄存器访问、`dc zva` 都经过它，原子的读改写算读也算写，与硬件一样。内核替 guest 访问内存（`read`、`write` 的缓冲区等）不触发观察点，与 Linux 一样（Linux 只在用户态的访问上触发）。有断点或观察点的线程只用 Tier 0 执行（运行循环看 `hw_debug_active`），同一进程的别的线程和别的进程照常用 Tier 1；没有设置时解释器每次访存多比较一次计数。
- **命中**：指令不执行，以新的故障类型（`FaultKind::Watchpoint`、`HwBreakpoint`）退出，内核投递 SIGTRAP，`si_code` 是 `TRAP_HWBKPT`，`si_addr` 是断点地址或者访问的起点（不对齐的一个字节的观察点报告那个字节，16 字节的 SIMD 存储碰到后一半时报告起点），没有 esr_context，与 arm64 Linux 的 `ptrace_hbptriggered` 一样。越过它由调试器负责：gdb 在 aarch64 上把观察点当作"不能单步"的，先停用、单步、再恢复。
- **生命周期**：fork、clone 出来的线程没有（新的 CPU 状态是清零的，复用的 CPU 在 `tuxport_cpu_init` 清掉），execve 时清掉（Linux 的 `flush_ptrace_hw_breakpoint`），PTRACE_DETACH 不清（与 Linux 相同）。core 文件里的 `NT_ARM_HW_BREAK`、`NT_ARM_HW_WATCH` 是线程当时的设置。

## 结果

Tier 0、Tier 1 的开销都在噪声范围内。

## 测试

与原生对照：本地 colima 虚拟机里的原生 gdb 用的是硬件观察点。gdb 会话（各种观察点、`hbreak`、另一个线程触发的观察点和 `info threads`）在两个执行层下都与原生逐行一致。

差分测试 `gdb` 新增上面的硬件观察点会话；`ptrace` 新增一个跟踪器直接读写 regset：槽数、非法的和规范化的控制字，各种观察点停在访问之前、`si_addr`，停用之后单步完成访问，断点，原子的读改写，SIMD 存储报告访问的起点（→ CI 上 Neoverse N2 的原生内核报告的不是起点，访问跨出观察范围时报告哪个地址由核决定：这一项改为只检查 `si_addr` 落在这条指令访问的范围里，2026-10-27）。单元测试新增控制字的检查和规范化、观察点换算成的范围。

- 跑全部差分测试时 `gdb` 组失败过一次，是[第二步](41-ptrace-gdb.md)就有的竞争：程序最后打印的一行和 gdb 的 `[LWP N exited]` 写同一个管道，谁先谁后不一定（原生一侧同样可能）。threads 的会话改为 `set print thread-events off`，新的硬件观察点会话看完 `info threads` 就 kill。

## 推迟项

- 有断点或观察点的线程整个用解释器执行，比 Tier 1 慢一个数量级（仍比软件观察点快得多）。要在 Tier 1 里做，得让翻译后的代码在观察的页上走慢路径（TLB 不填这些页）、在断点处结束块。→ 观察点已在 Tier 1 里做了（M9 之前：Tier 1 里的观察点），有断点的线程仍整个用解释器执行
- 观察点不支持地址掩码（控制字的 MASK 位，Linux 也不支持）；断点不支持上下文匹配、关联的断点。→ 后一半记错了：arm64 Linux 的 `arch_hw_breakpoint_ctrl` 只有 enabled、privilege、type、len 四个字段（`decode_ctrl_reg` 只取这些），经由 ptrace 同样设不了上下文匹配和关联的断点，与 tuxport 相同（2026-10-27 核对 Linux 的 `asm/hw_breakpoint.h`）
