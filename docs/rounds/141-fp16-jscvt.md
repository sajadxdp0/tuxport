# M9 之前：FP16 和 JSCVT（2026-10-31）

宿主有时宣告半精度浮点运算（FEAT_FP16，`AT_HWCAP` 的 FPHP、ASIMDHP）和 FJCVTZS（FEAT_JSCVT）。§4.4 原来把 FP16 列在"v1 不做"，那时 SIMD&FP 的运算还没有交给原生指令桩；现在数据处理指令在 Tier 0 里由原生指令桩执行、在 Tier 1 里原样拷贝，加一个这类特性只要放行它的编码（与 [ASIMDDP](133-asimddp.md)、[SHA3、SHA512 和 I8MM](135-sha3-sha512-i8mm.md) 一样）。

为什么是它：llama.cpp 的 ggml 按 HWCAP 挑 CPU 后端（`GGML_CPU_ALL_VARIANTS`，`ggml/src/ggml-feats.h` 的 `has_fp16 = FPHP && ASIMDHP`）。Linux 上的几档里只有 armv8.2_1 只要求 DOTPROD，往上每一档都要求 FP16，带 I8MM 的那档还要求 SVE：tuxport 宣告了 DOTPROD 和 I8MM，没有 FP16，ggml 一直停在 armv8.2_1，第 135 轮的 I8MM 在它这里也用不上。在 tuxport 里自己编译时，gcc 的 `-mcpu=native` 读 `/proc/cpuinfo` 的 Features 行，没有 fphp 就编出不带半精度运算的代码。2018 年以后的手机核心（Cortex-A55、A75 起）都有 FP16。JSCVT 是 V8 把 double 转成 int32（JavaScript 的 ToInt32）时用的一条指令，引擎里只差一行白名单，一起做了。

## 做法

- **白名单**（`core/engine/src/c/simd.c`，guest 有 FPHP、ASIMDHP 时）：
  - 新的编码组：向量和标量的"三寄存器同类型（FP16）"、"两寄存器杂项（FP16）"。三寄存器的两张表与单、双精度的那组相同（a 位代替 size 的高位）；杂项按 ARM ARM 的表逐条放行（FRINT*、FCVT*、SCVTF/UCVTF、和 0 比较、FABS/FNEG、FRECPE/FRSQRTE/FRECPX、FSQRT）。
  - 已有的组里半精度的形式：按元素的 FMLA/FMLS/FMUL/FMULX（size 00，H:L:M 选元素）、跨元素的 FMAXNMV/FMINNMV/FMAXV/FMINV（U = 0）、标量成对的 FMAXNMP/FMINNMP/FADDP/FMAXP/FMINP（U = 0）、向量 FMOV 立即数（o2 = 1）、定点转换（immh = 001x）。
  - 标量浮点各组的 type = 11：算术、乘加、比较、条件比较、条件选择、FMOV 立即数、一源操作数（FMOV/FABS/FNEG/FSQRT、FRINT*，原来只有 FCVT）、与整数和定点互转，以及 W/X 与 H 之间的 FMOV。
  - FJCVTZS（有 JSCVT 时）：写 W 寄存器和 NZCV，与 FCVTZS 一样带一个通用寄存器操作数，原生指令桩本来就载入、写回 NZCV。
  - RDM、FHM（FMLAL）、FCMA、BF16、FRINTTS 仍拒绝。
- **特性集合**（`features.c`）：FPHP、ASIMDHP 只一起宣告（白名单按一个开关判定，LLVM 的 `fullfp16` 也是一个特性）。ID_AA64PFR0_EL1 的 FP、AdvSIMD 字段在宣告时是 1（带半精度），ID_AA64ISAR1_EL1 的 JSCVT（第 12–15 位）是 1；`/proc/cpuinfo` 的名字表原来就有 fphp、asimdhp、jscvt。
- **FPCR**：Tier 0 处理 `msr fpcr` 时可写的位加上 FZ16（第 19 位，有 FP16 时）。Tier 1 的 FPCR 就是宿主的，写 FPCR 本来交给 Tier 0，回到翻译后的代码时载入。

## 结果

测量（开发用的虚拟机，Apple Silicon 上的 colima，4 核）：模仿 ggml 的 `ggml_vec_dot_f16` 写了两种 4096 个元素的 F16 点积，一种用 FP16 向量 FMLA（armv8.2_2 那一档的做法），一种先用 FCVTL 转成 f32 再 FMLA（armv8.2_1 和更低）。原生 0.0326 对 0.0734 ns/元素（2.25 倍）；tuxport 的 Tier 1 下 0.069 对 0.149 ns/元素（2.2 倍，两种都比原生慢一倍左右，是 soft-MMU 的访存检查）。上一轮的二进制不宣告 FP16，前一种 SIGILL，程序只能走后一种。

llama.cpp（v0.8.0）：在差分测试的 Alpine 镜像里原生编译 `-DGGML_BACKEND_DL=ON -DGGML_CPU_ALL_VARIANTS=ON -DGGML_NATIVE=OFF`，得到 armv8.0_1 到 armv9.2_2 八档 CPU 后端；一个目录放全部后端，另一个只留 armv8.0_1、armv8.2_1（相当于没有 FP16）。模型是 stories15M、Qwen2.5-0.5B-Instruct 的 Q4_0 和由它们转出来的 F16（`llama-quantize`）。`llama-bench -p 128 -n 32 -t 4 -r 3`，两个目录交替各跑两遍取平均，单位 tokens/s。原生在本地盘上，tuxport 的仓库在虚拟机本地的卷上（`/bench/store`），都用 Tier 1。

| 模型 | 测试 | 原生 8.2_1 | 原生 8.2_2 | tuxport 8.2_1 | tuxport 8.2_2 |
|---|---|---|---|---|---|
| Qwen2.5-0.5B F16 | 提示 128 | 229.6 | 378.1（+65%） | 126.9 | 230.9（+82%） |
| Qwen2.5-0.5B F16 | 生成 32 | 83.8 | 95.6（+14%） | 54.2 | 78.3（+44%） |
| stories15M F16 | 提示 128 | 11886 | 16170（+36%） | 5982 | 8725（+46%） |
| stories15M F16 | 生成 32 | 2042 | 2640 | 1257 | 1691（+35%） |
| Qwen2.5-0.5B Q4_0 | 提示 128 / 生成 32 | 1128 / 230 | 1171 / 225 | 843 / 190 | 851 / 187 |

- 带全部后端时，新的二进制挑中 armv8.2_2，上一轮的二进制挑中 armv8.2_1（ggml 的 `load_backend` 打印），数字与只留 armv8.2_1 的目录相同：差别全部来自宣告 FP16。
- F16 的权重和注意力（KV cache 默认是 F16）走半精度的向量 FMA，提示处理在 tuxport 里快 82%，比原生的收益（65%）还大，原因没有细查（可能是没有 FP16 的那条路径访存更多，每次访存在 Tier 1 里都要过 soft-MMU 的检查）。Q4_0 的矩阵乘走 int8 点积，两档没有差别。
- 相对原生（都用最好的那档）：Qwen F16 的提示 1.64×、生成 1.22×（不宣告 FP16 时 2.98×、1.74×），Q4_0 1.38×、1.21×。

## 测试

- lockstep：白名单与 `llvm-mc` 的对照（`-mattr` 按 guest 特性加 `fullfp16`、`jsconv`）一致；新类别 `fp16`（按表挑有定义的编码）和 `jscvt`（随机的双精度位型，含 NaN、无穷、超出范围的），原生、Tier 0、Tier 1 三方一致。
- 引擎的单元测试 `half_precision_follows_the_feature_set`、`javascript_conversion_follows_the_feature_set`，`id_registers_match_hwcap` 加上新字段；`unimplemented_encodings_are_undefined` 里原来的 FP16 例子换成 RDM 的 SQRDMLAH 和 FHM 的 FMLAL。
- 差分测试 `c-programs` 加上 `fp16.c`（`-march=armv8.2-a+fp16`）：特性的宣告、FJCVTZS 边界输入的结果和 Z、编译器向量化出的半精度乘加、向量和标量的各种半精度运算、FZ16，两个执行层下都与原生逐字相同。
- 全量 cargo test 和差分测试同时跑时 `stop_and_continue_signals` 失败过一次（STOP、CONT 之后各只等 0.2 秒就读进程状态）。 → 2026-11-02 推送第 180–183 轮之后 CI 上又失败一次（SIGCONT 之后读到 R），改成轮询状态、最多等 5 秒

## 推迟项

- FHM（FMLAL/FMLSL，半精度乘加到单精度）、BF16、RDM、FCMA 仍不宣告；FHM 依赖 FP16，ML 库（XNNPACK、onnxruntime）用它，可以照这一轮的做法加。→ 已补上（M9 之前：RDM、FCMA、FHM 和 BF16），FHM 只和 FP16 一起宣告
- 宿主只有 FPHP、ASIMDHP 中的一个时两个都不宣告（实际的 CPU 总是一起实现）。
