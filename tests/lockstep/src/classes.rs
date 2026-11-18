//! Random test case generator.
//!
//! Each class fills the fields of an encoding format at random and avoids encodings that read or
//! write SP or change control flow, because those instructions cannot run safely in the native
//! trampoline. Memory access cases only access the data area: the generator gives the base
//! register's offset from the start of the data area, and the runner converts it to a real
//! address. Hints such as PAC/BTI are deliberately implemented as NOPs (the engine map), unlike
//! hardware, so they are not tested here either.

use tuxport_engine::{hwcap, hwcap2};

use crate::rng::Rng;

/// In [`Class::requires`], `AT_HWCAP2` bits go in the high 32 bits (`AT_HWCAP` only uses up to
/// bit 31).
pub const HWCAP2_I8MM: u64 = hwcap2::I8MM << 32;
/// As above, `HWCAP2_BF16`.
pub const HWCAP2_BF16: u64 = hwcap2::BF16 << 32;
/// As above, `HWCAP2_FLAGM2`.
pub const HWCAP2_FLAGM2: u64 = hwcap2::FLAGM2 << 32;
/// As above, `HWCAP2_FRINT`.
pub const HWCAP2_FRINT: u64 = hwcap2::FRINT << 32;
/// As above, `HWCAP2_DCPODP`.
pub const HWCAP2_DCPODP: u64 = hwcap2::DCPODP << 32;

/// The guest's `AT_HWCAP` and its `AT_HWCAP2` shifted left by 32, compared with
/// [`Class::requires`].
pub fn guest_caps() -> u64 {
    tuxport_engine::guest_hwcap() | tuxport_engine::guest_hwcap2() << 32
}

/// Size of the data area (two pages), the same as on the native side.
pub const DATA_BYTES: u64 = 8192;

/// One test case: at most 4 instructions, plus initial settings for some registers.
#[derive(Debug, Clone, Default)]
pub struct Case {
    /// Instructions executed in order.
    pub insns: Vec<u32>,
    /// Registers set to "start of data area + offset".
    pub pointers: Vec<(u32, i64)>,
    /// Registers set to given values.
    pub values: Vec<(u32, u64)>,
    /// Registers set to the 8 bytes at some offset in the data area (to build a CAS that
    /// succeeds).
    pub loads: Vec<(u32, u64)>,
}

impl Case {
    fn one(insn: u32) -> Self {
        Case {
            insns: vec![insn],
            ..Case::default()
        }
    }
}

/// What the generator needs to know about the environment.
pub struct Env {
    /// Offset of the first tested instruction within its 4K page.
    pub slot_offset: u64,
}

/// How a case is generated.
#[derive(Clone, Copy)]
pub enum Gen {
    /// A single instruction that does not access memory.
    Insn(fn(&mut Rng) -> u32),
    /// A complete case.
    Case(fn(&mut Rng, &Env) -> Case),
}

/// A case class.
pub struct Class {
    /// Class name; `--class` runs it alone.
    pub name: &'static str,
    /// When true, encodings the hardware can execute must also be executable by the interpreter;
    /// when false (fuzz classes), the interpreter may be stricter than the hardware and treat
    /// encodings outside the whitelist as undefined instructions.
    pub strict: bool,
    /// The hardware may occasionally give different results (an interrupt clears the mark of an
    /// exclusive access); on a mismatch the hardware is rerun.
    pub flaky: bool,
    /// Multiple of the base iteration count.
    pub weight: u64,
    /// Runs only when the guest feature set contains these HWCAP bits (for HWCAP2 see
    /// [`HWCAP2_I8MM`]).
    pub requires: u64,
    /// How cases are generated.
    pub generate: Gen,
}

/// All classes.
pub const CLASSES: &[Class] = &[
    strict("pc_rel", pc_rel),
    strict("add_sub_imm", add_sub_imm),
    strict("logical_imm", logical_imm),
    strict("move_wide", move_wide),
    strict("bitfield", bitfield),
    strict("extract", extract),
    strict("logical_shifted", logical_shifted),
    strict("add_sub_shifted", add_sub_shifted),
    strict("add_sub_extended", add_sub_extended),
    strict("adc_sbc", adc_sbc),
    strict("cond_compare", cond_compare),
    strict("cond_select", cond_select),
    strict("dp_2src", dp_2src),
    strict("dp_1src", dp_1src),
    strict("dp_3src", dp_3src),
    strict("nzcv_move", nzcv_move),
    Class {
        name: "fuzz_dp",
        strict: false,
        flaky: false,
        weight: 1,
        requires: 0,
        generate: Gen::Insn(fuzz_dp),
    },
    case("int_ldst", int_ldst, 0),
    case("int_pair", int_pair, 0),
    case("same_base", same_base, hwcap::FP | hwcap::ASIMD),
    case("interleaved", interleaved, hwcap::FP | hwcap::ASIMD),
    case("literal", literal, hwcap::FP | hwcap::ASIMD),
    case("pc_page", pc_page, hwcap::FP | hwcap::ASIMD),
    case("simd_ldst", simd_ldst, hwcap::FP | hwcap::ASIMD),
    case("simd_pair", simd_pair, hwcap::FP | hwcap::ASIMD),
    case("simd_struct", simd_struct, hwcap::FP | hwcap::ASIMD),
    case("simd_lane", simd_lane, hwcap::FP | hwcap::ASIMD),
    case("fp_sysreg", fp_sysreg, hwcap::FP),
    Class {
        name: "exclusive",
        strict: true,
        flaky: true,
        weight: 1,
        requires: 0,
        generate: Gen::Case(exclusive),
    },
    case("lse", lse, hwcap::ATOMICS),
    case("lrcpc", lrcpc, hwcap::LRCPC),
    case("lrcpc2", lrcpc2, hwcap::ILRCPC),
    case("crc32", crc32, hwcap::CRC32),
    case("dotprod", dotprod, hwcap::ASIMDDP),
    case("sha3_sha512", sha3_sha512, hwcap::SHA3 | hwcap::SHA512),
    case("i8mm", i8mm, HWCAP2_I8MM),
    case("fp16", fp16, hwcap::FPHP | hwcap::ASIMDHP),
    case("jscvt", jscvt, hwcap::JSCVT),
    case("rdm", rdm, hwcap::ASIMDRDM),
    case("fcma", fcma, hwcap::FCMA),
    case("fhm", fhm, hwcap::ASIMDFHM),
    case("bf16", bf16, HWCAP2_BF16),
    case("frint", frint, HWCAP2_FRINT),
    case("flagm", flagm, hwcap::FLAGM),
    case("flagm2", flagm2, HWCAP2_FLAGM2),
    case("sb", sb, hwcap::SB),
    case("dcpop", dcpop, hwcap::DCPOP),
    case("dcpodp", dcpodp, HWCAP2_DCPODP),
    case("dit", dit, hwcap::DIT),
    Class {
        name: "fuzz_simd_fp",
        strict: false,
        flaky: false,
        weight: 4,
        requires: hwcap::FP | hwcap::ASIMD,
        generate: Gen::Insn(fuzz_simd_fp),
    },
];

const fn strict(name: &'static str, generate: fn(&mut Rng) -> u32) -> Class {
    Class {
        name,
        strict: true,
        flaky: false,
        weight: 1,
        requires: 0,
        generate: Gen::Insn(generate),
    }
}

const fn case(name: &'static str, generate: fn(&mut Rng, &Env) -> Case, requires: u64) -> Class {
    Class {
        name,
        strict: true,
        flaky: false,
        weight: 1,
        requires,
        generate: Gen::Case(generate),
    }
}

// op immlo 10000 immhi Rd
fn pc_rel(g: &mut Rng) -> u32 {
    (g.bit() << 31) | (g.bits(2) << 29) | (0b10000 << 24) | (g.bits(19) << 5) | reg(g)
}

// sf op S 100010 sh imm12 Rn Rd
fn add_sub_imm(g: &mut Rng) -> u32 {
    let s = g.bit();
    let rd = if s == 1 { reg(g) } else { reg_not_sp(g) };
    (g.bit() << 31)
        | (g.bit() << 30)
        | (s << 29)
        | (0b100010 << 23)
        | (g.bit() << 22)
        | (g.bits(12) << 10)
        | (reg_not_sp(g) << 5)
        | rd
}

// sf opc 100100 N immr imms Rn Rd
fn logical_imm(g: &mut Rng) -> u32 {
    let sf = g.bit();
    let opc = g.bits(2);
    let n = if sf == 1 { g.bit() } else { 0 };
    let rd = if opc == 3 { reg(g) } else { reg_not_sp(g) };
    (sf << 31)
        | (opc << 29)
        | (0b100100 << 23)
        | (n << 22)
        | (g.bits(6) << 16)
        | (g.bits(6) << 10)
        | (reg(g) << 5)
        | rd
}

// sf opc 100101 hw imm16 Rd
fn move_wide(g: &mut Rng) -> u32 {
    let sf = g.bit();
    let hw = if sf == 1 { g.bits(2) } else { g.bit() };
    (sf << 31)
        | (g.pick(&[0, 2, 3]) << 29)
        | (0b100101 << 23)
        | (hw << 21)
        | (g.bits(16) << 5)
        | reg(g)
}

// sf opc 100110 N immr imms Rn Rd
fn bitfield(g: &mut Rng) -> u32 {
    let sf = g.bit();
    let w = imm_width(sf);
    (sf << 31)
        | (g.pick(&[0, 1, 2]) << 29)
        | (0b100110 << 23)
        | (sf << 22)
        | (g.bits(w) << 16)
        | (g.bits(w) << 10)
        | (reg(g) << 5)
        | reg(g)
}

// sf 00 100111 N 0 Rm imms Rn Rd
fn extract(g: &mut Rng) -> u32 {
    let sf = g.bit();
    (sf << 31)
        | (0b100111 << 23)
        | (sf << 22)
        | (reg(g) << 16)
        | (g.bits(imm_width(sf)) << 10)
        | (reg(g) << 5)
        | reg(g)
}

// sf opc 01010 shift N Rm imm6 Rn Rd
fn logical_shifted(g: &mut Rng) -> u32 {
    let sf = g.bit();
    (sf << 31)
        | (g.bits(2) << 29)
        | (0b01010 << 24)
        | (g.bits(2) << 22)
        | (g.bit() << 21)
        | (reg(g) << 16)
        | (g.bits(imm_width(sf)) << 10)
        | (reg(g) << 5)
        | reg(g)
}

// sf op S 01011 shift 0 Rm imm6 Rn Rd
fn add_sub_shifted(g: &mut Rng) -> u32 {
    let sf = g.bit();
    (sf << 31)
        | (g.bit() << 30)
        | (g.bit() << 29)
        | (0b01011 << 24)
        | (g.below(3) << 22)
        | (reg(g) << 16)
        | (g.bits(imm_width(sf)) << 10)
        | (reg(g) << 5)
        | reg(g)
}

fn imm_width(sf: u32) -> u32 {
    if sf == 1 { 6 } else { 5 }
}

// sf op S 01011 00 1 Rm option imm3 Rn Rd
fn add_sub_extended(g: &mut Rng) -> u32 {
    let s = g.bit();
    let rd = if s == 1 { reg(g) } else { reg_not_sp(g) };
    (g.bit() << 31)
        | (g.bit() << 30)
        | (s << 29)
        | (0b01011 << 24)
        | (1 << 21)
        | (reg(g) << 16)
        | (g.bits(3) << 13)
        | (g.below(5) << 10)
        | (reg_not_sp(g) << 5)
        | rd
}

// sf op S 11010000 Rm 000000 Rn Rd
fn adc_sbc(g: &mut Rng) -> u32 {
    (g.bit() << 31)
        | (g.bit() << 30)
        | (g.bit() << 29)
        | (0b11010000 << 21)
        | (reg(g) << 16)
        | (reg(g) << 5)
        | reg(g)
}

// sf op 1 11010010 Rm|imm5 cond imm 0 Rn 0 nzcv
fn cond_compare(g: &mut Rng) -> u32 {
    (g.bit() << 31)
        | (g.bit() << 30)
        | (1 << 29)
        | (0b11010010 << 21)
        | (g.bits(5) << 16)
        | (g.bits(4) << 12)
        | (g.bit() << 11)
        | (reg(g) << 5)
        | g.bits(4)
}

// sf op 0 11010100 Rm cond 0 o2 Rn Rd
fn cond_select(g: &mut Rng) -> u32 {
    (g.bit() << 31)
        | (g.bit() << 30)
        | (0b11010100 << 21)
        | (reg(g) << 16)
        | (g.bits(4) << 12)
        | (g.bit() << 10)
        | (reg(g) << 5)
        | reg(g)
}

// sf 0 0 11010110 Rm opcode Rn Rd: UDIV/SDIV/LSLV/LSRV/ASRV/RORV
fn dp_2src(g: &mut Rng) -> u32 {
    (g.bit() << 31)
        | (0b11010110 << 21)
        | (reg(g) << 16)
        | (g.pick(&[2, 3, 8, 9, 10, 11]) << 10)
        | (reg(g) << 5)
        | reg(g)
}

// sf 1 0 11010110 00000 opcode Rn Rd: RBIT/REV16/REV32/REV/CLZ/CLS
fn dp_1src(g: &mut Rng) -> u32 {
    let sf = g.bit();
    let opcode = if sf == 1 {
        g.pick(&[0, 1, 2, 3, 4, 5])
    } else {
        g.pick(&[0, 1, 2, 4, 5])
    };
    (sf << 31) | (1 << 30) | (0b11010110 << 21) | (opcode << 10) | (reg(g) << 5) | reg(g)
}

// sf 00 11011 op31 Rm o0 Ra Rn Rd
fn dp_3src(g: &mut Rng) -> u32 {
    // (64-bit only, op31, o0, Ra must be 31)
    const FORMS: &[(bool, u32, u32, bool)] = &[
        (false, 0, 0, false), // MADD
        (false, 0, 1, false), // MSUB
        (true, 1, 0, false),  // SMADDL
        (true, 1, 1, false),  // SMSUBL
        (true, 2, 0, true),   // SMULH
        (true, 5, 0, false),  // UMADDL
        (true, 5, 1, false),  // UMSUBL
        (true, 6, 0, true),   // UMULH
    ];
    let (only64, op31, o0, ra_is_31) = g.pick(FORMS);
    let sf = if only64 { 1 } else { g.bit() };
    let ra = if ra_is_31 { 31 } else { reg(g) };
    (sf << 31)
        | (0b11011 << 24)
        | (op31 << 21)
        | (reg(g) << 16)
        | (o0 << 15)
        | (ra << 10)
        | (reg(g) << 5)
        | reg(g)
}

// MRS Xt, NZCV / MSR NZCV, Xt
fn nzcv_move(g: &mut Rng) -> u32 {
    let base = if g.bit() == 1 {
        0xd53b_4200
    } else {
        0xd51b_4200
    };
    base | reg(g)
}

/// Samples at random across the whole data processing (immediate, register) encoding space,
/// excluding only encodings with Rd/Rn = 31, since those two fields mean SP in some instructions.
fn fuzz_dp(g: &mut Rng) -> u32 {
    let mut insn = g.next_u64() as u32;
    insn = if g.bit() == 1 {
        (insn & !(0b111 << 26)) | (0b100 << 26) // bits 28:26 = 100: immediate
    } else {
        (insn & !(0b111 << 25)) | (0b101 << 25) // bits 27:25 = 101: register
    };
    let rd = reg_not_sp(g);
    let rn = reg_not_sp(g);
    (insn & !0x3ff) | (rn << 5) | rd
}

/// A register number other than 31, for fields where 31 means SP.
fn reg_not_sp(g: &mut Rng) -> u32 {
    g.below(31)
}

/// Integer LDR/STR/LDRS*: unsigned offset, unscaled, pre-index, post-index, register offset.
fn int_ldst(g: &mut Rng, _: &Env) -> Case {
    const OPCS: [&[u32]; 4] = [&[0, 1, 2, 3], &[0, 1, 2, 3], &[0, 1, 2], &[0, 1]];
    let size = g.bits(2);
    let opc = g.pick(OPCS[size as usize]);
    let rn = reg_except(g, &[]);
    let rt = reg_or_zr(g, &[rn]);
    single_reg(g, 0x3800_0000, size, opc, size, rn, rt, 1 << size)
}

/// Integer LDP/STP/LDPSW/LDNP/STNP.
fn int_pair(g: &mut Rng, _: &Env) -> Case {
    let opc = g.pick(&[0, 1, 2]);
    let load = if opc == 1 { 1 } else { g.bit() };
    let n = if opc == 2 { 8 } else { 4 };
    /* opc = 01 with idx = 00 (LDNPSW) is unallocated, and both sides should reject it */
    let idx = g.bits(2);
    let rn = reg_except(g, &[]);
    let rt = reg_or_zr(g, &[rn]);
    let rt2 = if load == 1 {
        reg_or_zr(g, &[rn, rt])
    } else {
        reg_or_zr(g, &[rn])
    };
    pair(g, 0x2800_0000, opc, load, idx, n, rn, rt, rt2)
}

/// 2–4 accesses based on the same register (Tier 1 makes them share one TLB check): integer and
/// SIMD&FP single-register and pair accesses, with writeback; ADD/SUB adjusting the base by a
/// constant may come in between, and the last one may be a load that overwrites the base. The
/// covered range may cross pages.
fn same_base(g: &mut Rng, _: &Env) -> Case {
    let rn = reg_except(g, &[]);
    /* The base is far enough from both ends of the data area: each instruction moves the base by
     * at most 512 bytes, and offsets are at most 512 bytes */
    let start = 2560 + g.below(DATA_BYTES as u32 - 2 * 2560 - 16) as i64;
    let mut c = Case {
        pointers: vec![(rn, start)],
        ..Case::default()
    };
    for _ in 0..2 + g.below(3) {
        let rt = reg_or_zr(g, &[rn]);
        let insn = match g.below(6) {
            0 => {
                /* LDR/STR Wt|Xt unsigned offset; a load occasionally overwrites the base, which
                 * is not used afterwards */
                let size = 2 + g.bit();
                let load = g.bit();
                let rt = if load == 1 && g.below(4) == 0 { rn } else { rt };
                let insn = size << 30 | 0x3900_0000 | load << 22 | g.below(32) << 10 | rn << 5 | rt;
                if rt == rn {
                    c.insns.push(insn);
                    break;
                }
                insn
            }
            1 => {
                /* LDUR/STUR, post-index, pre-index, all sizes */
                let op = g.pick(&[0, 1, 3]);
                g.bits(2) << 30
                    | 0x3800_0000
                    | g.bit() << 22
                    | (signed(g, 9) as u32 & 0x1ff) << 12
                    | op << 10
                    | rn << 5
                    | rt
            }
            2 => {
                /* LDP/STP Xt: post-index, signed offset, pre-index */
                let load = g.bit();
                let rt2 = if load == 1 {
                    reg_or_zr(g, &[rn, rt])
                } else {
                    reg_or_zr(g, &[rn])
                };
                2 << 30
                    | 0x2800_0000
                    | (1 + g.below(3)) << 23
                    | load << 22
                    | (signed(g, 7) as u32 & 0x7f) << 15
                    | rt2 << 10
                    | rn << 5
                    | rt
            }
            3 => {
                /* LDR/STR Qt unsigned offset */
                0x3d80_0000 | g.bit() << 22 | g.below(32) << 10 | rn << 5 | g.bits(5)
            }
            4 => {
                /* LDP/STP Dt signed offset */
                let load = g.bit();
                let rt = g.bits(5);
                let rt2 = loop {
                    let r = g.bits(5);
                    if load == 0 || r != rt {
                        break r;
                    }
                };
                1 << 30
                    | 0x2d00_0000
                    | load << 22
                    | (signed(g, 7) as u32 & 0x7f) << 15
                    | rt2 << 10
                    | rn << 5
                    | rt
            }
            _ => {
                /* ADD/SUB Xn, Xn, #imm */
                0x9100_0000 | g.bit() << 30 | g.below(64) << 10 | rn << 5 | rn
            }
        };
        c.insns.push(insn);
    }
    c
}

/// Accesses on two interleaved bases (Tier 1 makes those on the same base share one check; when
/// other accesses come in between, members take the host base back from a slot), possibly with an
/// operation using three stolen registers in between (translating it needs T2). The ranges the
/// two bases point to may overlap.
fn interleaved(g: &mut Rng, _: &Env) -> Case {
    let a = reg_except(g, &[]);
    let b = reg_except(g, &[a]);
    let mut start = || 2560 + g.below(DATA_BYTES as u32 - 2 * 2560 - 16) as i64;
    let mut c = Case {
        pointers: vec![(a, start()), (b, start())],
        ..Case::default()
    };
    let pattern = g.pick(&[b"abab", b"abaa", b"aaba", b"axab"]);
    for &p in pattern {
        let insn = match p {
            b'a' => access_on(g, a, &[a, b]),
            b'b' => access_on(g, b, &[a, b]),
            _ => {
                /* ADD Xd, Xn, Xm: three stolen registers, not writing either base */
                let stolen = [16, 17, 18, 26, 27, 28];
                let pick = |g: &mut Rng, used: &[u32]| loop {
                    let r = g.pick(&stolen);
                    if !used.contains(&r) {
                        break r;
                    }
                };
                let rd = pick(g, &[a, b]);
                let rn = pick(g, &[rd]);
                let rm = pick(g, &[rd, rn]);
                0x8b00_0000 | rm << 16 | rn << 5 | rd
            }
        };
        c.insns.push(insn);
    }
    c
}

/// One access based on `rn` (the forms of same_base), not writing the registers in `keep`
/// (except for base writeback).
fn access_on(g: &mut Rng, rn: u32, keep: &[u32]) -> u32 {
    let rt = reg_or_zr(g, keep);
    match g.below(5) {
        0 => {
            let size = 2 + g.bit();
            size << 30 | 0x3900_0000 | g.bit() << 22 | g.below(32) << 10 | rn << 5 | rt
        }
        1 => {
            let op = g.pick(&[0, 1, 3]);
            g.bits(2) << 30
                | 0x3800_0000
                | g.bit() << 22
                | (signed(g, 9) as u32 & 0x1ff) << 12
                | op << 10
                | rn << 5
                | rt
        }
        2 => {
            let load = g.bit();
            let rt2 = loop {
                let r = reg_or_zr(g, keep);
                if load == 0 || r != rt {
                    break r;
                }
            };
            2 << 30
                | 0x2800_0000
                | (1 + g.below(3)) << 23
                | load << 22
                | (signed(g, 7) as u32 & 0x7f) << 15
                | rt2 << 10
                | rn << 5
                | rt
        }
        3 => 0x3d80_0000 | g.bit() << 22 | g.below(32) << 10 | rn << 5 | g.bits(5),
        _ => 0x9100_0000 | g.bit() << 30 | g.below(64) << 10 | rn << 5 | rn,
    }
}

/// LDR (literal): both integer and SIMD&FP, with the target in the tested instruction's 4K page.
/// The interpreter side puts an svc in the 5th word, unlike the hardware, so the target must
/// avoid it.
fn literal(g: &mut Rng, env: &Env) -> Case {
    let simd = g.bit() == 1;
    let opc = g.below(3);
    let n: u64 = if simd {
        4 << opc
    } else {
        [4, 8, 4][opc as usize]
    };
    let svc = env.slot_offset as i64 + 16;
    let target = loop {
        let t = g.below(((4096 - n) / 4 + 1) as u32) as i64 * 4;
        if t + n as i64 <= svc || t >= svc + 4 {
            break t;
        }
    };
    let imm19 = (target - env.slot_offset as i64) / 4;
    let base = if simd { 0x1c00_0000 } else { 0x1800_0000 };
    Case::one((opc << 30) | base | ((imm19 as u32 & 0x7ffff) << 5) | g.bits(5))
}

/// Reads the tested instruction's page through a base computed by ADRP (possibly plus an
/// immediate): Tier 1 knows the address at translation time, and the entry's position is a
/// constant (dbt.c's konst). An instruction that changes the base (MOV, or a load into the base
/// itself) may come in between, after which accesses may no longer be treated as known
/// addresses.
fn pc_page(g: &mut Rng, env: &Env) -> Case {
    let rn = reg_except(g, &[]);
    let mut c = Case::default();
    c.insns.push(0x9000_0000 | rn); // ADRP Xn, this page
    let mut base = 0i64;
    if g.bit() == 1 {
        let k = g.below(500) as i64 * 8; // leaves at least a few 16-byte positions after it
        c.insns.push(0x9100_0000 | (k as u32) << 10 | rn << 5 | rn); // ADD Xn, Xn, #k
        base = k;
    }
    let svc = env.slot_offset as i64 + 16;
    // An n-byte access: LDR X/W/H/B, LDP X, LDR Q, with the offset aligned to n and at most `max`
    // (LDP's immediate has only 7 bits), inside this page and not touching the SVC
    let pick = |g: &mut Rng, base: i64, n: i64, max: i64| loop {
        let off = g.below(((4096 - base - n) / n).min(max / n) as u32 + 1) as i64 * n;
        let t = base + off;
        if t + n <= 4096 && (t + n <= svc || t >= svc + 4) {
            break off;
        }
    };
    let access = |g: &mut Rng, rn: u32, base: i64| -> u32 {
        match g.below(4) {
            0 => {
                let size = g.below(4);
                let n = 1i64 << size;
                let off = pick(g, base, n, 4095 * n);
                (size << 30)
                    | 0x3940_0000
                    | ((off / n) as u32) << 10
                    | rn << 5
                    | reg_or_zr(g, &[rn])
            }
            1 => {
                let off = pick(g, base, 16, 504);
                let t1 = reg_or_zr(g, &[rn]);
                let t2 = reg_or_zr(g, &[rn, t1]);
                0xa940_0000 | ((off / 8) as u32) << 15 | t2 << 10 | rn << 5 | t1
            }
            _ => {
                let off = pick(g, base, 16, 4095 * 16);
                0x3dc0_0000 | ((off / 16) as u32) << 10 | rn << 5 | g.bits(5)
            }
        }
    };
    match g.below(4) {
        0 => {
            // MOV Xn, Xm: the base now points into the data area and is no longer a known value
            let rm = reg_except(g, &[rn]);
            c.pointers.push((rm, 0));
            c.insns.push(0xaa00_03e0 | rm << 16 | rn);
            let off = (g.below(512) * 8) as i64;
            c.insns
                .push(0xf940_0000 | ((off / 8) as u32) << 10 | rn << 5 | g.bits(5));
        }
        1 => {
            // A load into the base itself (LDR Xn, [Xn, #off])
            let off = pick(g, base, 8, 4095 * 8);
            c.insns
                .push(0xf940_0000 | ((off / 8) as u32) << 10 | rn << 5 | rn);
        }
        _ => {
            c.insns.push(access(g, rn, base));
            if c.insns.len() < 4 && g.bit() == 1 {
                c.insns.push(access(g, rn, base));
            }
        }
    }
    c
}

/// SIMD&FP single-register LDR/STR (B/H/S/D/Q).
fn simd_ldst(g: &mut Rng, _: &Env) -> Case {
    const FORMS: [(u32, u32); 10] = [
        (0, 0),
        (0, 1),
        (0, 2),
        (0, 3),
        (1, 0),
        (1, 1),
        (2, 0),
        (2, 1),
        (3, 0),
        (3, 1),
    ];
    let (size, opc) = g.pick(&FORMS);
    let scale = if size == 0 && opc >= 2 { 4 } else { size };
    let rn = reg_except(g, &[]);
    let rt = g.bits(5);
    single_reg(g, 0x3c00_0000, size, opc, scale, rn, rt, 1 << scale)
}

/// The common part of single-register accesses; `base` is the encoding base value for V = 0 or 1,
/// and `scale` is log2 of the byte count.
#[allow(clippy::too_many_arguments)]
fn single_reg(
    g: &mut Rng,
    base: u32,
    size: u32,
    opc: u32,
    scale: u32,
    rn: u32,
    rt: u32,
    n: u64,
) -> Case {
    let mut c = Case::default();
    let t = target(g, n, 1);
    let fixed = (size << 30) | base | (opc << 22) | (rn << 5) | rt;
    let (insn, start) = match g.below(5) {
        0 => {
            let imm = g.below(64);
            (
                fixed | 0x0100_0000 | (imm << 10),
                t - ((imm as i64) << scale),
            )
        }
        1..=3 => {
            let op = g.pick(&[0, 1, 3]);
            let imm9 = signed(g, 9);
            let start = if op == 1 { t } else { t - imm9 };
            (fixed | ((imm9 as u32 & 0x1ff) << 12) | (op << 10), start)
        }
        _ => {
            let rm = reg_except(g, &[rn]);
            let option = g.pick(&[2, 3, 6, 7]);
            let s = g.bit();
            let index = g.below(32) as u64;
            c.values.push((rm, index));
            let off = (index << (s * scale)) as i64;
            (
                fixed | 0x0020_0800 | (rm << 16) | (option << 13) | (s << 12),
                t - off,
            )
        }
    };
    c.insns.push(insn);
    c.pointers.push((rn, start));
    c
}

/// SIMD&FP LDP/STP/LDNP/STNP (S/D/Q).
fn simd_pair(g: &mut Rng, _: &Env) -> Case {
    let opc = g.below(3);
    let load = g.bit();
    let rn = reg_except(g, &[]);
    let rt = g.bits(5);
    let mut rt2 = g.bits(5);
    if load == 1 && rt2 == rt {
        rt2 = (rt + 1) % 32;
    }
    let idx = g.bits(2);
    pair(g, 0x2c00_0000, opc, load, idx, 4 << opc, rn, rt, rt2)
}

#[allow(clippy::too_many_arguments)]
fn pair(
    g: &mut Rng,
    base: u32,
    opc: u32,
    load: u32,
    idx: u32,
    n: u64,
    rn: u32,
    rt: u32,
    rt2: u32,
) -> Case {
    let imm7 = signed(g, 7);
    let t = target(g, 2 * n, 1);
    let start = if idx == 1 { t } else { t - imm7 * n as i64 };
    Case {
        insns: vec![
            (opc << 30)
                | base
                | (idx << 23)
                | (load << 22)
                | ((imm7 as u32 & 0x7f) << 15)
                | (rt2 << 10)
                | (rn << 5)
                | rt,
        ],
        pointers: vec![(rn, start)],
        ..Case::default()
    }
}

/// LD1–LD4/ST1–ST4 (multiple structures), with post-index.
fn simd_struct(g: &mut Rng, _: &Env) -> Case {
    const FORMS: [(u32, u64, u64); 7] = [
        (0x0, 1, 4),
        (0x2, 4, 1),
        (0x4, 1, 3),
        (0x6, 3, 1),
        (0x7, 1, 1),
        (0x8, 1, 2),
        (0xa, 2, 1),
    ];
    let (opcode, rpt, selem) = g.pick(&FORMS);
    let q = g.bit();
    let mut size = g.bits(2);
    if size == 3 && q == 0 && selem != 1 {
        size = 2;
    }
    let total = rpt * selem * if q == 1 { 16 } else { 8 };
    let rn = reg_except(g, &[]);
    let mut c = Case::default();
    let mut insn =
        (q << 30) | (g.bit() << 22) | (opcode << 12) | (size << 10) | (rn << 5) | g.bits(5);
    if g.bit() == 1 {
        insn |= 0x0c80_0000 | (post_index(g, &mut c, rn) << 16);
    } else {
        insn |= 0x0c00_0000;
    }
    c.insns.push(insn);
    c.pointers.push((rn, target(g, total, 1)));
    c
}

/// LD1–LD4/ST1–ST4 (single structure) and LD1R–LD4R, with post-index.
fn simd_lane(g: &mut Rng, _: &Env) -> Case {
    let selem = g.below(4) + 1;
    let kind = g.below(5);
    let (q, s, size, scale_bits, esize) = match kind {
        0 => {
            let i = g.bits(4);
            (i >> 3, (i >> 2) & 1, i & 3, 0, 1)
        }
        1 => {
            let i = g.bits(3);
            (i >> 2, (i >> 1) & 1, (i & 1) << 1, 1, 2)
        }
        2 => {
            let i = g.bits(2);
            (i >> 1, i & 1, 0, 2, 4)
        }
        3 => (g.bit(), 0, 1, 2, 8),
        _ => {
            let size = g.bits(2);
            (g.bit(), 0, size, 3, 1 << size)
        }
    };
    let load = if kind == 4 { 1 } else { g.bit() };
    let opcode = (scale_bits << 1) | ((selem - 1) >> 1);
    let r = (selem - 1) & 1;
    let rn = reg_except(g, &[]);
    let mut c = Case::default();
    let mut insn = (q << 30)
        | (load << 22)
        | (r << 21)
        | (opcode << 13)
        | (s << 12)
        | (size << 10)
        | (rn << 5)
        | g.bits(5);
    if g.bit() == 1 {
        insn |= 0x0d80_0000 | (post_index(g, &mut c, rn) << 16);
    } else {
        insn |= 0x0d00_0000;
    }
    c.insns.push(insn);
    c.pointers.push((rn, target(g, selem as u64 * esize, 1)));
    c
}

/// Post-index form: Rm = 31 means by the number of bytes transferred, otherwise a general
/// register is added.
fn post_index(g: &mut Rng, c: &mut Case, rn: u32) -> u32 {
    if g.bit() == 1 {
        return 31;
    }
    let rm = reg_except(g, &[rn]);
    c.values.push((rm, g.next_u64() & 0xfff));
    rm
}

/// MRS/MSR FPCR, FPSR; the written values contain only bits defined by ARMv8.0.
fn fp_sysreg(g: &mut Rng, _: &Env) -> Case {
    let rt = g.bits(5);
    let mut c = Case::default();
    let insn = match g.below(4) {
        0 => {
            c.values.push((rt, g.next_u64() & 0x07c0_0000));
            0xd51b_4400
        }
        1 => 0xd53b_4400,
        2 => {
            c.values.push((rt, g.next_u64() & 0x0800_009f));
            0xd51b_4420
        }
        _ => 0xd53b_4420,
    };
    c.insns.push(insn | rt);
    c
}

/// Exclusive accesses: LDXR+STXR, LDXP+STXP, CLREX+STXR (always fails), LDAR/STLR.
fn exclusive(g: &mut Rng, _: &Env) -> Case {
    let rn = reg_except(g, &[]);
    let (acquire, release) = (g.bit(), g.bit());
    let mut c = Case::default();
    match g.below(5) {
        0 | 1 => {
            let size = g.bits(2);
            let rt = reg_or_zr(g, &[rn]);
            let ws = reg_except(g, &[rn]);
            let rt2 = reg_or_zr(g, &[rn, ws]);
            c.insns = vec![
                (size << 30) | 0x085f_7c00 | (acquire << 15) | (rn << 5) | rt,
                (size << 30) | 0x0800_7c00 | (ws << 16) | (release << 15) | (rn << 5) | rt2,
            ];
            c.pointers.push((rn, target(g, 1 << size, 1 << size)));
        }
        2 => {
            let size = 2 + g.bit();
            let rt = reg_or_zr(g, &[rn]);
            let rt2 = reg_or_zr(g, &[rn, rt]);
            let ws = reg_except(g, &[rn]);
            let (st1, st2) = (reg_or_zr(g, &[rn, ws]), reg_or_zr(g, &[rn, ws]));
            c.insns = vec![
                (size << 30) | 0x087f_0000 | (acquire << 15) | (rt2 << 10) | (rn << 5) | rt,
                (size << 30)
                    | 0x0820_0000
                    | (ws << 16)
                    | (release << 15)
                    | (st2 << 10)
                    | (rn << 5)
                    | st1,
            ];
            let n = 2u64 << size;
            c.pointers.push((rn, target(g, n, n)));
        }
        3 => {
            let size = g.bits(2);
            let ws = reg_except(g, &[rn]);
            let rt = reg_or_zr(g, &[rn, ws]);
            c.insns = vec![
                0xd503_3f5f, // clrex
                (size << 30) | 0x0800_7c00 | (ws << 16) | (release << 15) | (rn << 5) | rt,
            ];
            c.pointers.push((rn, target(g, 1 << size, 1 << size)));
        }
        _ => {
            let size = g.bits(2);
            let base = if g.bit() == 1 {
                0x08df_fc00
            } else {
                0x089f_fc00
            };
            c.insns = vec![(size << 30) | base | (rn << 5) | reg_or_zr(g, &[rn])];
            c.pointers.push((rn, target(g, 1 << size, 1 << size)));
        }
    }
    c
}

/// LSE: LD<op>, SWP, CAS, CASP. Half of the time CAS gets a compare value equal to the memory
/// value, so that it really writes.
fn lse(g: &mut Rng, _: &Env) -> Case {
    let rn = reg_except(g, &[]);
    let (a, r) = (g.bit(), g.bit());
    let mut c = Case::default();
    match g.below(4) {
        0 | 1 => {
            let size = g.bits(2);
            let op = g.below(9);
            let (o3, opc) = if op == 8 { (1, 0) } else { (0, op) };
            let rs = reg_or_zr(g, &[rn]);
            let rt = reg_or_zr(g, &[rn]);
            c.insns.push(
                (size << 30)
                    | 0x3820_0000
                    | (a << 23)
                    | (r << 22)
                    | (rs << 16)
                    | (o3 << 15)
                    | (opc << 12)
                    | (rn << 5)
                    | rt,
            );
            c.pointers.push((rn, target(g, 1 << size, 1 << size)));
        }
        2 => {
            let size = g.bits(2);
            let rs = reg_except(g, &[rn]);
            let t = target(g, 1 << size, 1 << size);
            if g.bit() == 1 {
                c.loads.push((rs, t as u64));
            }
            c.insns.push(
                (size << 30)
                    | 0x08a0_7c00
                    | (a << 22)
                    | (rs << 16)
                    | (r << 15)
                    | (rn << 5)
                    | reg_or_zr(g, &[rn]),
            );
            c.pointers.push((rn, t));
        }
        _ => {
            let size = g.bit();
            let esize = 4u64 << size;
            let even = |g: &mut Rng| loop {
                let x = g.below(15) * 2;
                if x != rn && x + 1 != rn {
                    return x;
                }
            };
            let rs = even(g);
            let rt = even(g);
            let t = target(g, 2 * esize, 2 * esize);
            if g.bit() == 1 {
                c.loads.push((rs, t as u64));
                c.loads.push((rs + 1, t as u64 + esize));
            }
            c.insns.push(
                (size << 30) | 0x0820_7c00 | (a << 22) | (rs << 16) | (r << 15) | (rn << 5) | rt,
            );
            c.pointers.push((rn, t));
        }
    }
    c
}

/// LDAPR, LDAPRB, LDAPRH (32 and 64 bits), with the base pointing at an aligned place in the data
/// area.
fn lrcpc(g: &mut Rng, _: &Env) -> Case {
    let rn = reg_except(g, &[]);
    let size = g.bits(2);
    let mut c = Case::default();
    c.insns
        .push((size << 30) | 0x38bf_c000 | (rn << 5) | reg_or_zr(g, &[rn]));
    c.pointers.push((rn, target(g, 1 << size, 1 << size)));
    c
}

/// LRCPC2's STLUR*, LDAPUR*, LDAPURS*: the base plus a 9-bit offset points at an aligned place in
/// the data area.
fn lrcpc2(g: &mut Rng, _: &Env) -> Case {
    let rn = reg_except(g, &[]);
    let size = g.bits(2);
    let bytes = 1u32 << size;
    // size 11 has no sign-extending load, size 10 none extending to 32 bits
    let opc = match size {
        3 => g.below(2),
        2 => g.below(3),
        _ => g.below(4),
    };
    let off = (g.below(512 / bytes) as i64 - (256 / bytes) as i64) * bytes as i64;
    let rt = if opc == 0 {
        reg_or_zr(g, &[])
    } else {
        reg_or_zr(g, &[rn])
    };
    let mut c = Case::default();
    c.insns.push(
        (size << 30) | 0x1900_0000 | (opc << 22) | ((off as u32 & 0x1ff) << 12) | (rn << 5) | rt,
    );
    c.pointers
        .push((rn, target(g, bytes.into(), bytes.into()) - off));
    c
}

/// CRC32B/H/W/X, CRC32CB/H/W/X.
fn crc32(g: &mut Rng, _: &Env) -> Case {
    let sz = g.bits(2);
    let sf = (sz == 3) as u32;
    Case::one(
        (sf << 31)
            | 0x1ac0_4000
            | (reg(g) << 16)
            | (g.bit() << 12)
            | (sz << 10)
            | (reg(g) << 5)
            | reg(g),
    )
}

/// SDOT, UDOT in vector and by-element forms (2S, 4S).
fn dotprod(g: &mut Rng, _: &Env) -> Case {
    let q_u = (g.bit() << 30) | (g.bit() << 29);
    let rm = reg(g);
    let rn_rd = (reg(g) << 5) | reg(g);
    Case::one(if g.bit() == 1 {
        0x0e80_9400 | q_u | (rm << 16) | rn_rd
    } else {
        // By element: H:L selects which 32-bit element of Vm, M:Rm is the full register number
        0x0f80_e000 | q_u | (g.bit() << 21) | (rm << 16) | (g.bit() << 11) | rn_rd
    })
}

/// SHA3's EOR3, BCAX, XAR, RAX1; SHA512's SHA512H, SHA512H2, SHA512SU0, SHA512SU1.
fn sha3_sha512(g: &mut Rng, _: &Env) -> Case {
    let rn_rd = (reg(g) << 5) | reg(g);
    let rm = reg(g) << 16;
    Case::one(match g.bits(3) {
        0 => 0xce00_0000 | rm | (reg(g) << 10) | rn_rd, // EOR3
        1 => 0xce20_0000 | rm | (reg(g) << 10) | rn_rd, // BCAX
        2 => 0xce80_0000 | rm | (g.bits(6) << 10) | rn_rd, // XAR
        3 => 0xce60_8c00 | rm | rn_rd,                  // RAX1
        4 => 0xcec0_8000 | rn_rd,                       // SHA512SU0
        // SHA512H, SHA512H2, SHA512SU1
        k => 0xce60_8000 | ((k - 5) << 10) | rm | rn_rd,
    })
}

/// I8MM: SMMLA, UMMLA, USMMLA (128-bit only), the vector form of USDOT, and the by-element forms
/// of USDOT and SUDOT.
fn i8mm(g: &mut Rng, _: &Env) -> Case {
    let q = g.bit() << 30;
    let rn_rd = (reg(g) << 5) | reg(g);
    let rm = reg(g) << 16;
    let lane = (g.bit() << 21) | (g.bit() << 11);
    Case::one(match g.bits(3) {
        0 => 0x4e80_a400 | rm | rn_rd,                // SMMLA
        1 => 0x6e80_a400 | rm | rn_rd,                // UMMLA
        2 => 0x4e80_ac00 | rm | rn_rd,                // USMMLA
        3 | 4 => 0x0e80_9c00 | q | rm | rn_rd,        // USDOT (vector)
        5 | 6 => 0x0f80_f000 | q | lane | rm | rn_rd, // USDOT (by element)
        _ => 0x0f00_f000 | q | lane | rm | rn_rd,     // SUDOT (by element)
    })
}

/// FP16: vector and scalar three-register and two-register miscellaneous, by element, across
/// elements, the scalar floating-point groups (arithmetic, multiply-add, compare, conditional
/// select, conversion to and from integers, FMOV), and vector FMOV immediate. Only encodings
/// that are defined are picked, by table.
fn fp16(g: &mut Rng, _: &Env) -> Case {
    // Bit tables over (U, a, opcode), the same as simd.c's: three-register opcode 0–7,
    // miscellaneous opcode 0x0c–0x1f
    const THREE: [[u32; 2]; 2] = [[0xdf, 0xc7], [0xfd, 0x75]];
    const SCALAR_THREE: [[u32; 2]; 2] = [[0x98, 0x80], [0x30, 0x34]];
    const MISC: [[u32; 2]; 2] = [[0x3f00_0000, 0x2f00_f000], [0x3f00_0000, 0xae00_b000]];
    const SCALAR_MISC: [[u32; 2]; 2] = [[0x3c00_0000, 0xac00_7000], [0x3c00_0000, 0x2c00_3000]];
    let pick = |g: &mut Rng, table: &[[u32; 2]; 2], width: u32| loop {
        let (u, a, op) = (g.bit(), g.bit(), g.bits(width));
        if (table[u as usize][a as usize] >> op) & 1 == 1 {
            return (u << 29) | (a << 23) | op;
        }
    };
    let q = g.bit() << 30;
    let (rd, rn, rm) = (reg(g), reg(g) << 5, reg(g) << 16);
    Case::one(match g.bits(4) {
        0 | 1 => {
            let f = pick(g, &THREE, 3);
            0x0e40_0400 | q | (f & !7) | ((f & 7) << 11) | rm | rn | rd
        }
        2 => {
            let f = pick(g, &MISC, 5);
            0x0e78_0800 | q | (f & !31) | ((f & 31) << 12) | rn | rd
        }
        3 => {
            let f = pick(g, &SCALAR_THREE, 3);
            0x5e40_0400 | (f & !7) | ((f & 7) << 11) | rm | rn | rd
        }
        4 => {
            let f = pick(g, &SCALAR_MISC, 5);
            0x5e78_0800 | (f & !31) | ((f & 31) << 12) | rn | rd
        }
        // FMLA, FMLS, FMUL, FMULX (by element): H:L:M selects the element, Rm has only 4 bits
        5 => {
            let op = [0x0f00_1000, 0x0f00_5000, 0x0f00_9000, 0x2f00_9000][g.bits(2) as usize];
            let lane = (g.bit() << 11) | (g.bit() << 21) | (g.bit() << 20);
            op | q | lane | ((reg(g) & 15) << 16) | rn | rd
        }
        // FMAXNMV, FMAXV, FMINNMV, FMINV (opcode 0x0c, 0x0f)
        6 => 0x0e30_c800 | q | (g.bit() << 23) | (g.bit() * 0x3000) | rn | rd,
        // Scalar FMUL, FDIV, FADD, FSUB, FMAX, FMIN, FMAXNM, FMINNM, FNMUL
        7 | 8 => 0x1ee0_0800 | rm | ((g.next_u64() % 9) as u32) << 12 | rn | rd,
        // FMADD, FMSUB, FNMADD, FNMSUB
        9 => 0x1fc0_0000 | (g.bit() << 21) | rm | (g.bit() << 15) | (reg(g) << 10) | rn | rd,
        // FCMP, FCMPE (against 0), FCCMP, FCSEL
        10 => {
            let opc = g.bits(2);
            0x1ee0_2000 | if opc & 1 == 1 { 0 } else { rm } | rn | (opc << 3)
        }
        11 => 0x1ee0_0400 | rm | (g.bits(4) << 12) | rn | (g.bit() << 4) | g.bits(4),
        12 => 0x1ee0_0c00 | rm | (g.bits(4) << 12) | rn | rd,
        // FMOV, FABS, FNEG, FSQRT, FCVT (to single, double precision), FRINT*
        13 => {
            let op = [0, 1, 2, 3, 4, 5, 8, 9, 10, 11, 12, 14, 15][(g.next_u64() % 13) as usize];
            0x1ee0_4000 | (op << 15) | rn | rd
        }
        // Conversion to and from W/X: FCVT*S/U, SCVTF/UCVTF, FMOV
        14 => {
            let (rmode, op) = [
                (0, 0),
                (1, 0),
                (2, 1),
                (3, 0),
                (0, 2),
                (0, 3),
                (0, 4),
                (0, 5),
                (0, 6),
                (0, 7),
            ][(g.next_u64() % 10) as usize];
            (g.bit() << 31) | 0x1ee0_0000 | (rmode << 19) | (op << 16) | rn | rd
        }
        // Vector FMOV immediate (half precision)
        _ => 0x0f00_fc00 | q | (g.bits(3) << 16) | (g.bits(5) << 5) | rd,
    })
}

/// FJCVTZS: the input is a random double-precision bit pattern (including NaNs, infinities and
/// out-of-range values); W and NZCV are compared.
fn jscvt(g: &mut Rng, _: &Env) -> Case {
    Case::one(0x1e7e_0000 | (reg(g) << 5) | reg(g))
}

/// RDM: SQRDMLAH, SQRDMLSH in vector, scalar and by-element forms (H and S elements).
fn rdm(g: &mut Rng, _: &Env) -> Case {
    let size = (1 + g.bit()) << 22;
    let sub = g.bit(); // 0 is SQRDMLAH, 1 is SQRDMLSH
    let (rn_rd, rm) = ((reg(g) << 5) | reg(g), reg(g) << 16);
    // By element: for H elements H:L:M selects the element and Rm has only 4 bits; for S elements
    // H:L selects the element and M:Rm is the register number
    let lane = (g.bit() << 21) | (g.bit() << 20) | ((reg(g) & 15) << 16) | (g.bit() << 11);
    Case::one(match g.bits(2) {
        0 => 0x2e00_8400 | (g.bit() << 30) | size | rm | (sub << 11) | rn_rd,
        1 => 0x7e00_8400 | size | rm | (sub << 11) | rn_rd,
        2 => 0x2f00_d000 | (sub << 13) | (g.bit() << 30) | size | lane | rn_rd,
        _ => 0x7f00_d000 | (sub << 13) | size | lane | rn_rd,
    })
}

/// FCMA: FCMLA (vector, by element, four rotations), FCADD (90, 270). Half, single and double
/// precision (128-bit only).
fn fcma(g: &mut Rng, _: &Env) -> Case {
    let (rn_rd, rm) = ((reg(g) << 5) | reg(g), reg(g) << 16);
    let size = 1 + (g.next_u64() % 3) as u32;
    let q = if size == 3 { 1 } else { g.bit() };
    Case::one(match g.bits(2) {
        0 | 1 => 0x2e00_c400 | (q << 30) | (size << 22) | rm | (g.bits(2) << 11) | rn_rd,
        2 => 0x2e00_e400 | (q << 30) | (size << 22) | rm | (g.bit() << 12) | rn_rd,
        // By element: half precision selects a pair by H:L (H = 0 for 64 bits), single precision
        // by H (128-bit only, L = 0)
        _ => {
            let half = g.bit() == 1;
            let q = if half { g.bit() } else { 1 };
            let h = if half && q == 0 { 0 } else { g.bit() };
            let l = if half { g.bit() } else { 0 };
            let size = if half { 1 } else { 2 };
            0x2f00_1000
                | (q << 30)
                | (size << 22)
                | (l << 21)
                | rm
                | (g.bits(2) << 13)
                | (h << 11)
                | rn_rd
        }
    })
}

/// FHM: FMLAL, FMLSL, FMLAL2, FMLSL2 in vector and by-element forms (H:L:M selects the element,
/// Rm has only 4 bits).
fn fhm(g: &mut Rng, _: &Env) -> Case {
    let q = g.bit() << 30;
    let rn_rd = (reg(g) << 5) | reg(g);
    let lane = (g.bit() << 21) | (g.bit() << 20) | ((reg(g) & 15) << 16) | (g.bit() << 11);
    Case::one(match g.bits(3) {
        0 => 0x0e20_ec00 | q | (reg(g) << 16) | rn_rd, // FMLAL
        1 => 0x0ea0_ec00 | q | (reg(g) << 16) | rn_rd, // FMLSL
        2 => 0x2e20_cc00 | q | (reg(g) << 16) | rn_rd, // FMLAL2
        3 => 0x2ea0_cc00 | q | (reg(g) << 16) | rn_rd, // FMLSL2
        4 => 0x0f80_0000 | q | lane | rn_rd,           // FMLAL (by element)
        5 => 0x0f80_4000 | q | lane | rn_rd,           // FMLSL (by element)
        6 => 0x2f80_8000 | q | lane | rn_rd,           // FMLAL2 (by element)
        _ => 0x2f80_c000 | q | lane | rn_rd,           // FMLSL2 (by element)
    })
}

/// BF16: BFDOT, BFMLALB/T (vector and by element), BFMMLA, BFCVT, BFCVTN/BFCVTN2.
fn bf16(g: &mut Rng, _: &Env) -> Case {
    let q = g.bit() << 30;
    let (rn_rd, rm) = ((reg(g) << 5) | reg(g), reg(g) << 16);
    // BFDOT selects a pair by H:L and M:Rm is the register number; BFMLAL selects the element by
    // H:L:M and Rm has only 4 bits
    let (l, m, h) = (g.bit() << 21, g.bit() << 20, g.bit() << 11);
    Case::one(match g.bits(3) {
        0 => 0x2e40_fc00 | q | rm | rn_rd,         // BFDOT
        1 => 0x6e40_ec00 | rm | rn_rd,             // BFMMLA
        2 => 0x2ec0_fc00 | q | rm | rn_rd,         // BFMLALB, BFMLALT
        3 => 0x0f40_f000 | q | l | rm | h | rn_rd, // BFDOT (by element)
        4 => 0x0fc0_f000 | q | l | m | (rm & 0xf_0000) | h | rn_rd, // BFMLALB/T (by element)
        5 => 0x1e63_4000 | rn_rd,                  // BFCVT
        _ => 0x0ea1_6800 | q | rn_rd,              // BFCVTN, BFCVTN2
    })
}

/// FRINTTS: FRINT32Z/X, FRINT64Z/X in scalar (single, double precision) and vector forms (2S, 4S,
/// 2D). The X forms round by a random FPCR.RMode.
fn frint(g: &mut Rng, _: &Env) -> Case {
    let (rn_rd, op) = ((reg(g) << 5) | reg(g), g.bits(2));
    Case::one(if g.bit() == 1 {
        0x1e20_4000 | (g.bit() << 22) | ((0x10 | op) << 15) | rn_rd
    } else {
        let sz = g.bit();
        let q = if sz == 1 { 1 } else { g.bit() };
        let (u, opcode) = (op & 1, 0x1e | (op >> 1));
        0x0e20_0800 | (q << 30) | (u << 29) | (sz << 22) | (opcode << 12) | rn_rd
    })
}

/// FlagM: CFINV, RMIF (random shift and mask), SETF8, SETF16; NZCV starts out random.
fn flagm(g: &mut Rng, _: &Env) -> Case {
    Case::one(match g.bits(2) {
        0 => 0xd500_401f,
        1 => 0xba00_0400 | (g.bits(6) << 15) | (reg(g) << 5) | g.bits(4),
        _ => 0x3a00_080d | (g.bit() << 14) | (reg(g) << 5),
    })
}

/// FlagM2: XAFLAG, AXFLAG.
fn flagm2(g: &mut Rng, _: &Env) -> Case {
    Case::one(0xd500_401f | ((1 + g.bit()) << 5))
}

/// SB (speculation barrier).
fn sb(_: &mut Rng, _: &Env) -> Case {
    Case::one(0xd503_30ff)
}

fn dcpop(g: &mut Rng, _: &Env) -> Case {
    dc_persist(g, 0xd50b_7c20)
}

fn dcpodp(g: &mut Rng, _: &Env) -> Case {
    dc_persist(g, 0xd50b_7d20)
}

/// DC CVAP, DC CVADP: the address points at any byte in the data area.
fn dc_persist(g: &mut Rng, insn: u32) -> Case {
    let rt = reg_except(g, &[]);
    let mut c = Case::one(insn | rt);
    c.pointers.push((rt, target(g, 1, 1)));
    c
}

/// DIT: first set with MSR DIT, #imm (any CRm) or MSR DIT, Xn (random value), then read back with
/// MRS; DIT left by the previous case on the native side does not affect the result.
fn dit(g: &mut Rng, _: &Env) -> Case {
    let (rt, rn) = (reg(g), reg(g));
    let set = if g.bit() == 1 {
        0xd503_405f | (g.bits(4) << 8)
    } else {
        0xd51b_42a0 | rn
    };
    Case {
        insns: vec![set, 0xd53b_42a0 | rt],
        ..Case::default()
    }
}

/// The whole SIMD&FP data processing encoding space (bits 27:25 = 111). The whitelist itself is
/// checked against LLVM (whitelist.rs); this checks the semantics and register moves of the
/// executed instructions.
fn fuzz_simd_fp(g: &mut Rng) -> u32 {
    g.next_u64() as u32 | 0x0e00_0000
}

/// Any register number; 31 is XZR in these encodings.
fn reg(g: &mut Rng) -> u32 {
    g.bits(5)
}

/// A register in 0–30 that is not in `avoid`.
fn reg_except(g: &mut Rng, avoid: &[u32]) -> u32 {
    loop {
        let r = g.below(31);
        if !avoid.contains(&r) {
            return r;
        }
    }
}

/// A register in 0–31 that is not in `avoid`; 31 is ZR in these encodings.
fn reg_or_zr(g: &mut Rng, avoid: &[u32]) -> u32 {
    loop {
        let r = g.bits(5);
        if !avoid.contains(&r) {
            return r;
        }
    }
}

/// Target offset of a `len`-byte access, aligned to `align`, guaranteed to fall inside the data
/// area (it may cross pages).
fn target(g: &mut Rng, len: u64, align: u64) -> i64 {
    let slots = (DATA_BYTES - len) / align + 1;
    (g.below(slots as u32) as u64 * align) as i64
}

fn signed(g: &mut Rng, bits: u32) -> i64 {
    g.bits(bits) as i64 - (1 << (bits - 1))
}
