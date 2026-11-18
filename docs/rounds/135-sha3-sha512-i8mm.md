# M9 之前：SHA3、SHA512 和 I8MM（2026-10-31）

[点积指令 ASIMDDP](133-asimddp.md)那一轮的推迟项：§4.4 的 v1.x 里还有 SHA3、SHA512，表外的 I8MM 和点积一样是 int8 的 SIMD 指令。三者都是 SIMD&FP 的数据处理指令，Tier 0 交给原生指令桩、Tier 1 原样拷贝，只要放行编码、宿主有时宣告。用得到它们的：OpenSSL、Go 的 `crypto/sha512`、`crypto/sha3` 和 ed25519（SHA-512）用 SHA512、SHA3 指令；llama.cpp、libvpx、libaom、onnxruntime 按 `HWCAP2_I8MM` 选 int8 矩阵乘的代码路径。

## 做法

- **编码**（`core/engine/src/c/simd.c`）：
  - `1100 1110` 开头的加密扩展原来整组拒绝。现在放行 SHA3 的 EOR3、BCAX（四寄存器组里 Op0 = 00、01，SM3SS1 是 10）、XAR、RAX1（三寄存器 SHA512 组的 opcode 11），SHA512 的 SHA512H、SHA512H2、SHA512SU1（同一组的 opcode 00–10）、SHA512SU0（两寄存器组）；同一片编码里的 SM3、SM4 仍拒绝。
  - I8MM：三寄存器扩展组里 size = 10 的 USDOT（opcode 0011，U = 0）、SMMLA/UMMLA（0100，只有 Q = 1 的 128 位形式）、USMMLA（0101，U = 0），按元素组 opcode 1111、U = 0 的 USDOT（size 10）和 SUDOT（size 00）；同一组里 BF16 的 BFDOT、BFMMLA、BFMLAL 和 RDM、FCMA 仍拒绝。各条的编码用 llvm-mc 核对过。
- **宣告**（`features.c`）：SHA3、SHA512 只一起宣告，还要有 SHA2：LLVM 的 `sha3` 特性同时管这两组，白名单的核对分不开，实际的 CPU 也总是一起实现。I8MM 在 `AT_HWCAP2` 里（第 13 位），原来 guest 的 `AT_HWCAP2` 总是 0，现在照 `AT_HWCAP` 的规则取宿主的 ∩ 已实现的（引擎的 `guest_hwcap2`），auxv 交给 guest。`ID_AA64ISAR0_EL1` 的 SHA2 字段有 SHA512 时是 2、SHA3 字段是 1，`ID_AA64ISAR1_EL1` 的 I8MM 字段是 1；`/proc/cpuinfo` 的名字表按 Linux 6.1 的 `hwcap_str` 补到 `sha512`，再接上 HWCAP2 的到 `i8mm`。

## 测试

- lockstep 新类别 `sha3_sha512` 和 `i8mm`，加长的 lockstep 里与硬件全部一致；类别要求的 HWCAP2 位放在 `requires` 的高 32 位。白名单与 llvm-mc 的对照加上 `+sha3`、`+i8mm`，随机样本一致。
- 单元测试 `sha3_and_i8mm_follow_the_feature_set`，`id_registers_match_hwcap` 加上新的字段和 SHA3、SHA512 一起宣告；`unimplemented_encodings_are_undefined` 里的 USDOT 换成 BF16 的 BFDOT 和 SM4E；auxv 的单元测试比对 `AT_HWCAP2`。
- 差分测试 `c-programs` 组新增 `crypto.c`：特性的几处宣告、各条指令的 intrinsics 的结果和汇编里的条数，两个执行层下都与原生相同。
- **收益**：原生、上一轮的二进制、这一轮的二进制交替各跑三次，结果的摘要、输出的 md5 在各种组合下都相同。
  - SHA-512：Python 的 `hashlib.sha512`（OpenSSL 3.5.9，按 HWCAP 选 `sha512_block_armv8`）算 32 MiB。原生 1495–1532 MB/s，用 `OPENSSL_armcap` 关掉 SHA512 指令是 634–639 MB/s；tuxport 里 362–365 → 1488–1500 MB/s（4.1 倍），与原生用硬件指令时持平：整个循环都是原样执行的 SHA512 指令。
  - SHA3-256：`hashlib.sha3_256` 原生 669–692 MB/s，关不关 SHA3 都一样，tuxport 里新旧都是 418–425 MB/s：这台机器上 OpenSSL 的 Keccak 不走 SHA3 指令（它只在认为划算的 CPU 上用）。
  - I8MM：[上一轮](133-asimddp.md)的 `vpxenc`（`VPX_SIMD_CAPS_MASK` 0x3 是 NEON 加点积，0x7 再加 I8MM）。原生 0x3 1.49–1.59s、0x7 1.49–1.55s，tuxport 里新旧二进制交替（ABBA）都是 3.5s 左右：VP9 编码在这个配置下几乎不走 I8MM 的路径，原生上也没有收益。

## 推迟项

- 宿主只有 SHA3、SHA512 中的一个（或者没有 SHA2）时两个都不宣告。
- BF16、FP16、RDM、FHM、FCMA、JSCVT 这些仍不宣告；SVE 和 HWCAP2 里别的位仍是 0。→ FP16、JSCVT 已补上（M9 之前：FP16 和 JSCVT）；BF16、RDM、FHM、FCMA 已补上（M9 之前：RDM、FCMA、FHM 和 BF16），HWCAP2 有了 BF16；HWCAP2 的 DCPODP、FLAGM2、FRINT 也补上了（M9 之前：FRINTTS、FlagM、SB 和 DPB）；SVE 和 HWCAP2 里别的位仍是 0
