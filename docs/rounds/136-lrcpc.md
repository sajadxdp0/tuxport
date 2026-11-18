# M9 之前：LRCPC（2026-10-31）

§4.4 的 v1.x 里剩下的最后一项。LRCPC（ARMv8.3）是 RCpc 语义的 load-acquire：LDAPR、LDAPRH、LDAPRB 只保证不越过之前的 release 之后的读（比 LDAR 弱），C/C++ 的 `memory_order_acquire` 在 `+rcpc` 下编成它。编译时就假定有 LRCPC 的程序（`-march=armv8.3-a` 以上、`-mcpu=neoverse-n1` 这些）原来在 tuxport 里一读就 SIGILL；运行时按 HWCAP 选它的很少，所以没有要测的收益。与前几轮不同，它是访存指令，两个执行层都要写。

## 做法

- **Tier 0**（`core/engine/src/c/atomic.c`）：编码在 LSE 那一组里（A = 1、R = 0、Rs 全是 1、o3 = 1、opc = 100）。Rs 不全是 1 时 LLVM 不认，这里也当未定义（Rs 是"应当全为 1"的字段，§4.4 的白名单沿用 LLVM 的口径）。地址的检查、对齐、观察点与 LDAR 相同，照 LDAR 用 acquire 读：acquire 比 RCpc 强，架构允许实现得更强。
- **Tier 1**（`dbt.c` 的 `translate_atomic`）：与 LDAR 一样先做软件 MMU 的检查（按读检查），再在宿主地址上原样执行 LDAPR（只有宿主有 LRCPC 时 Tier 0 才认它，Tier 1 只翻译 Tier 0 认的编码）。不单独处理的话它会落进 LD<op>、SWP 那一支：Rs 被当作输入，地址按写检查，只读的页上权限检查不过，退到慢路径由 Tier 0 重做，结果对但多走一趟。
- **宣告**：宿主的 `AT_HWCAP` 有第 15 位才宣告；`ID_AA64ISAR1_EL1` 的 LRCPC 字段是 1，`/proc/cpuinfo` 的 `lrcpc` 原来就在名字表里。宿主还有 LRCPC2（LDAPUR 这些带偏移的形式，cpuinfo 的 `ilrcpc`）时 Linux 报 2，这里仍是 1，LRCPC2 不宣告。

## 测试

- lockstep 新类别 `lrcpc`，加长的 lockstep 里与硬件全部一致；白名单与 llvm-mc 的对照加上 `+rcpc`，样本一致。
- 单元测试 `ldapr_follows_the_feature_set`（含只读的页上也能读、没对齐是对齐错误），`id_registers_match_hwcap` 加上 LRCPC 字段；`unimplemented_encodings_are_undefined` 里原来的 LDAPR 换成 LRCPC2 的 LDAPUR 和 Rs 不全是 1 的 LDAPR。
- 差分测试 `c-programs` 组新增 `lrcpc.c`（`-march=armv8-a+rcpc`）：特性的宣告（ID 寄存器的字段只比至少是 1，原生是 2）、各种宽度的 LDAPR、acquire 读编成 LDAPR、两个线程用 STLR、LDAPR 传消息，两个执行层下都与原生相同。Tier 1 不单独处理 LDAPR 时差分测试照样通过（只是走慢路径）。

## 推迟项

- LRCPC2（LDAPUR、STLUR 这些带偏移的形式）不做。→ 已补上（M9 之前：LRCPC2）
- 宿主没有 LRCPC 时，编译时假定它的程序仍会 SIGILL（与在没有 LRCPC 的 CPU 上一样）。
