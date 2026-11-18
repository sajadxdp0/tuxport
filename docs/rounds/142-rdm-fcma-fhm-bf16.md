# M9 之前：RDM、FCMA、FHM 和 BF16（2026-10-31）

宿主有时宣告的另外四个 SIMD 扩展，原来都不宣告（§4.4 表里的"还没做"，[ASIMDDP](133-asimddp.md)、[SHA3、SHA512 和 I8MM](135-sha3-sha512-i8mm.md)、[FP16 和 JSCVT](141-fp16-jscvt.md) 的推迟项）：

- RDM（`asimdrdm`）：SQRDMLAH、SQRDMLSH，Q15、Q31 定点数的"乘、舍入、加到累加值"一条做完。
- FCMA（`fcma`）：FCMLA、FCADD，交错存放的复数的乘加和旋转加。
- FHM（`asimdfhm`）：FMLAL、FMLSL 及其 `2` 形式，半精度相乘、在单精度里累加。
- BF16（`AT_HWCAP2` 的 `bf16`）：BFDOT、BFMMLA、BFMLALB/T、BFCVT、BFCVTN。

做法与前几轮相同：SIMD&FP 的数据处理指令在 Tier 0 里由原生指令桩执行、在 Tier 1 里原样拷贝，加一个这类特性只要放行它的编码、宣告它。不宣告时，按 HWCAP 挑内核的库走慢一些的退路；`-march` 写死了这些扩展的程序 SIGILL；在 tuxport 里用 `gcc -mcpu=native` 编译时，gcc 读 `/proc/cpuinfo`，编出的代码也不用它们。

## 做法

- **白名单**（`core/engine/src/c/simd.c`）：
  - RDM：三寄存器扩展组 U = 1、opcode 0000/0001 的向量形式；新认出标量的三寄存器扩展组（`(insn & 0xdf208400) == 0x5e008400`，只有这两条）；向量、标量按元素组 U = 1 的 opcode 1101、1111。都只有 H、S 两种元素。
  - FCMA：三寄存器扩展组 U = 1 的 opcode 10xx（FCMLA，低两位是旋转）、11x0（FCADD）；按元素组 U = 1、opcode 0xx1 的 FCMLA。半精度的形式（size 01）还要 FP16，双精度只有 128 位的形式；按元素的半精度按 H:L 选一对元素（64 位时只有两对，H = 1 未定义），单精度按 H 选、只有 128 位、L 必须是 0。
  - FHM：三寄存器同类型组的 FP 部分，U = 0 的 11101（FMLAL、FMLSL）、U = 1 的 11001（FMLAL2、FMLSL2），sz 必须是 0；按元素组 size 10 的 opcode 0000、0100（U = 0）、1000、1100（U = 1），H:L:M 选元素。
  - BF16：三寄存器扩展组 U = 1 的 opcode 1101、size 01（BFMMLA，只有 Q = 1）和 opcode 1111 的 size 01（BFDOT）、size 11（BFMLALB/T，Q 选 B 还是 T）；按元素组 U = 0、opcode 1111 的 size 01、11（size 00、10 是原来就有的 I8MM 的 SUDOT、USDOT）；两寄存器杂项 U = 0、opcode 10110 的 size 10（BFCVTN/BFCVTN2，原来只有 FCVTN）；标量一源操作数组 type 01、opcode 000110（BFCVT，只能从单精度转）。
- **特性集合**（`features.c`）：HWCAP 第 12、14、23 位（ASIMDRDM、FCMA、ASIMDFHM），HWCAP2 第 14 位（BF16）。FHM 的源操作数是半精度，架构上要求有 FP16（LLVM 的 `fp16fml` 也隐含 `fullfp16`），没有 FP16 时不宣告。ID_AA64ISAR0_EL1 的 RDM（第 28–31 位）、FHM（第 48–51 位），ID_AA64ISAR1_EL1 的 FCMA（第 16–19 位）、BF16（第 44–47 位，1 表示没有 EBF16）；`/proc/cpuinfo` 的 HWCAP2 名字表加上 `bf16`。
- FPCR 没有变：这四个扩展都不加新的控制位（EBF16 才有 FPCR.EBF，开发机的 M2 Pro 也没有 EBF16）。

## 结果

测量（开发用的虚拟机，M2 Pro 上的 colima；程序在 `target/ext142/bench.c`）：每种扩展写一个循环，与不用它的常见写法比，4096 个元素算 2 万遍，取 3 次里最快的，原生和 tuxport 的 Tier 1 各两遍取平均，单位 ns/元素：

| 内核 | 原生：用扩展 | 原生：不用 | tuxport：用扩展 | tuxport：不用 |
|---|---|---|---|---|
| BF16 点积：BFDOT 对比移位展开成单精度再 FMLA | 0.058 | 0.139（2.4 倍） | 0.102 | 0.150（1.5 倍） |
| 半精度点积、单精度累加：FMLAL 对比 FCVTL 再 FMLA | 0.133 | 0.136 | 0.147 | 0.151 |
| 复数点积：两条 FCMLA 对比 TRN、REV、FMUL、两条 FMLA | 0.279 | 0.294 | 0.296 | 0.297 |
| Q15 乘加：SQRDMLAH 对比 SQRDMULH 再 SQADD | 0.0435 | 0.0447 | 0.261 | 0.260 |

- BFDOT 一条做了两次展开、两次乘加，原生快 2.4 倍；tuxport 里循环的两次读都要过 soft-MMU 的检查，指令少了一半多的收益占比变小，仍快 1.5 倍。
- 另外三种在 M2 上差别都在 5% 以内：这几个循环卡在累加器的依赖链上，M2 的 FMLAL、FCMLA 也不比拆开的写法快多少。Q15 的循环每 8 个元素一读一写，tuxport 里慢 6 倍是写的检查，与扩展无关。收益要看核心：别的核心上这几条指令的延迟、吞吐与 M2 不同，没有量。

## 测试

- 白名单与 `llvm-mc` 的对照（`-mattr` 按 guest 特性加 `rdm`、`complxnum`、`fp16fml`、`bf16`）一致；lockstep 新类别 `rdm`、`fcma`、`fhm`、`bf16`，原生、Tier 0、Tier 1 三方一致。几个边界编码在宿主 CPU 上逐条执行过，都是 SIGILL，与白名单相同。
- 引擎的单元测试 `rdm_fcma_fhm_bf16_follow_the_feature_set`，`id_registers_match_hwcap` 加上四个字段和"FHM 只和 FP16 一起"；`unimplemented_encodings_are_undefined` 里原来的 SQRDMLAH、FMLAL、BFDOT 换成 FRINT32Z（FRINTTS）、SM3PARTW1，以及几种未定义的 BFDOT、FCMLA、FCADD。
- 差分测试 `c-programs` 加上 `ext.c`（`-march=armv8.3-a+fp16fml+bf16`）：特性的宣告和四个扩展的各种 intrinsics（含按元素、饱和时的 FPSR.QC、BFCVT 的舍入），两个执行层下都与原生逐字相同。在 tuxport 里 `gcc -mcpu=native` 现在定义这四个扩展的 `__ARM_FEATURE_*` 宏，与原生相同。

## 推迟项

- §4.4 的表里，宿主（M2 Pro、常见的手机核心）有、仍不宣告的：FRINTTS（FRINT32Z 等）、FLAGM/FLAGM2（CFINV、RMIF、SETF、AXFLAG）、SB、DIT、DCPOP/DCPODP（DC CVAP、CVADP）、SM3/SM4、LSE2（`uscat`，[LRCPC2](138-lrcpc2.md) 的推迟项）。EBF16 宿主没有，没做。→ FRINTTS、FLAGM/FLAGM2、SB、DCPOP/DCPODP 已补上（M9 之前：FRINTTS、FlagM、SB 和 DPB），DIT 也补上了（M9 之前：DIT）；LSE2 仍没有；SM3/SM4 开发机的 M2 没有，与 EBF16 一样测不了
- 这几个扩展只在 M2 上量过：BF16 以外的三种在这里收益很小，在手机的核心上没量（R1）。
