//! Unit tests for the interpreter: memory accesses, branches, system instructions and the
//! soft-MMU.
//!
//! Lockstep (tests/lockstep) tests instructions against hardware, but cannot run instructions that
//! use SP or change control flow, and its memory accesses stay inside a data area that never
//! faults; those cases are tested here with fixed encodings. The encodings come from GNU as.

use std::ptr::NonNull;

use tuxport_engine::{Access, Cpu, Exit, FaultKind, Mmu, PAGE_SIZE, hwcap, hwcap2, prot};

#[repr(C, align(4096))]
struct Page([u8; 4096]);

/// Two read-write data pages are mapped at `DATA`, and one read-only page at `RODATA`.
const DATA: u64 = 0x10000;
const RODATA: u64 = 0x20000;
const CODE: u64 = 0x40_0000;

struct Guest {
    // Drop order: cpu → mmu → pages
    cpu: Cpu,
    mmu: Mmu,
    pages: Box<[Page]>,
}

impl Guest {
    fn new() -> Self {
        let mut pages: Box<[Page]> = (0..4).map(|_| Page([0; 4096])).collect();
        let mut mmu = Mmu::new();
        let base = NonNull::new(pages.as_mut_ptr().cast::<u8>()).unwrap();
        unsafe {
            mmu.map(DATA, base, 2 * PAGE_SIZE, prot::READ | prot::WRITE)
                .unwrap();
            mmu.map(RODATA, base.add(2 * 4096), PAGE_SIZE, prot::READ)
                .unwrap();
            mmu.map(CODE, base.add(3 * 4096), PAGE_SIZE, prot::READ | prot::EXEC)
                .unwrap();
        }
        let cpu = unsafe { Cpu::new(&mmu) };
        let mut g = Guest { cpu, mmu, pages };
        g.cpu.state_mut().pc = CODE;
        g
    }

    /// The host bytes for a guest address (only the pages this test maps are supported).
    fn bytes(&mut self, addr: u64) -> &mut [u8] {
        let (page, off) = match addr {
            a if (DATA..DATA + 2 * PAGE_SIZE).contains(&a) => {
                ((a - DATA) / PAGE_SIZE, (a - DATA) % PAGE_SIZE)
            }
            a if (RODATA..RODATA + PAGE_SIZE).contains(&a) => (2, a - RODATA),
            a if (CODE..CODE + PAGE_SIZE).contains(&a) => (3, a - CODE),
            _ => panic!("address {addr:#x} not mapped by the test"),
        };
        &mut self.pages[page as usize].0[off as usize..]
    }

    fn put_u64(&mut self, addr: u64, v: u64) {
        self.bytes(addr)[..8].copy_from_slice(&v.to_le_bytes());
    }

    fn get_u64(&mut self, addr: u64) -> u64 {
        u64::from_le_bytes(self.bytes(addr)[..8].try_into().unwrap())
    }

    fn x(&self, r: usize) -> u64 {
        self.cpu.state().x[r]
    }

    fn set_x(&mut self, r: usize, v: u64) {
        self.cpu.state_mut().x[r] = v;
    }

    fn pc(&self) -> u64 {
        self.cpu.state().pc
    }

    /// Executes one instruction, requiring it to complete normally and pc to advance by 4.
    fn step(&mut self, insn: u32) {
        let pc = self.pc();
        assert_eq!(self.cpu.exec_insn(insn), None, "insn {insn:#010x}");
        assert_eq!(self.pc(), pc + 4, "insn {insn:#010x}");
    }
}

#[test]
fn add_sub_distinguishes_sp_from_zr_and_preserves_flags() {
    let mut g = Guest::new();
    g.cpu.state_mut().sp = 0x1234_5678_ffff_fff8;
    g.cpu.state_mut().nzcv = 0x9000_0000;
    g.set_x(0, 1);
    g.step(0x1100_43ff); // add wsp, wsp, #16
    assert_eq!(g.cpu.state().sp, 8);
    g.step(0xcb20_4bff); // sub sp, sp, w0, uxtw #2
    g.step(0x8b00_043f); // add xzr, x1, x0, lsl #1
    assert_eq!((g.cpu.state().sp, g.cpu.state().nzcv), (4, 0x9000_0000));
    g.step(0xf100_13ff); // cmp sp, #4
    assert_eq!((g.cpu.state().sp, g.cpu.state().nzcv), (4, 0x6000_0000));
}

#[test]
fn load_store_immediate_forms() {
    let mut g = Guest::new();
    g.put_u64(DATA + 8, 0x1122_3344_5566_7788);
    g.set_x(1, DATA);
    g.step(0xf9400420); // ldr x0, [x1, #8]
    assert_eq!(g.x(0), 0x1122_3344_5566_7788);

    g.set_x(1, DATA + 16);
    g.set_x(2, 0xdead_beef);
    g.step(0xf81f8c22); // str x2, [x1, #-8]!
    assert_eq!(g.get_u64(DATA + 8), 0xdead_beef);
    assert_eq!(g.x(1), DATA + 8);

    g.put_u64(DATA, 42);
    g.set_x(1, DATA);
    g.step(0xf8410423); // ldr x3, [x1], #16
    assert_eq!(g.x(3), 42);
    assert_eq!(g.x(1), DATA + 16);

    g.set_x(1, DATA);
    g.set_x(2, 0xabcd);
    g.step(0x79000422); // strh w2, [x1, #2]
    assert_eq!(&g.bytes(DATA)[2..4], &[0xcd, 0xab]);
}

#[test]
fn load_store_pairs() {
    let mut g = Guest::new();
    g.set_x(1, DATA + 32);
    g.set_x(6, 6);
    g.set_x(7, 7);
    g.step(0xa9bf1c26); // stp x6, x7, [x1, #-16]!
    assert_eq!(g.x(1), DATA + 16);
    assert_eq!((g.get_u64(DATA + 16), g.get_u64(DATA + 24)), (6, 7));

    g.set_x(1, DATA);
    g.step(0xa9411424); // ldp x4, x5, [x1, #16]
    assert_eq!((g.x(4), g.x(5)), (6, 7));

    g.bytes(DATA)[..8].copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 1, 0, 0, 0]);
    g.step(0x29401424); // ldp w4, w5, [x1]
    assert_eq!((g.x(4), g.x(5)), (0xffff_ffff, 1));
    g.step(0x69401424); // ldpsw x4, x5, [x1]
    assert_eq!((g.x(4), g.x(5)), (u64::MAX, 1));
}

#[test]
fn sign_extending_loads() {
    let mut g = Guest::new();
    g.set_x(1, DATA);
    g.bytes(DATA)[..4].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
    g.step(0xb9800028); // ldrsw x8, [x1]
    assert_eq!(g.x(8), 0xffff_ffff_ffff_fff0);
    g.bytes(DATA)[0] = 0x80;
    g.step(0x39c00029); // ldrsb w9, [x1]
    assert_eq!(g.x(9), 0xffff_ff80);
}

#[test]
fn register_offset_and_literal() {
    let mut g = Guest::new();
    g.set_x(1, DATA);
    g.set_x(2, 3);
    g.bytes(DATA)[3] = 0x5a;
    g.step(0x3862682a); // ldrb w10, [x1, x2]
    assert_eq!(g.x(10), 0x5a);
    g.put_u64(DATA + 24, 77);
    g.step(0xf862782b); // ldr x11, [x1, x2, lsl #3]
    assert_eq!(g.x(11), 77);

    let literal = g.pc() + 8;
    g.put_u64(literal, 0x1234);
    g.step(0x5800004c); // ldr x12, .+8
    assert_eq!(g.x(12), 0x1234);
}

#[test]
fn access_crossing_a_page_boundary() {
    let mut g = Guest::new();
    let data: Vec<u8> = (1..=8).collect();
    g.bytes(DATA + PAGE_SIZE - 2)[..2].copy_from_slice(&data[..2]);
    g.bytes(DATA + PAGE_SIZE)[..6].copy_from_slice(&data[2..]);
    g.set_x(1, DATA + PAGE_SIZE + 1);
    g.step(0xf85fd020); // ldur x0, [x1, #-3]
    assert_eq!(g.x(0), u64::from_le_bytes(data.try_into().unwrap()));
}

#[test]
fn faults_have_no_side_effects() {
    let mut g = Guest::new();
    // The second page is unmapped: the whole instruction fails and x0 is unchanged
    g.set_x(0, 0x5555);
    g.set_x(1, DATA + 2 * PAGE_SIZE + 1);
    assert_eq!(
        g.cpu.exec_insn(0xf85fd020), // ldur x0, [x1, #-3]
        Some(Exit::Fault {
            addr: DATA + 2 * PAGE_SIZE,
            access: Access::Read,
            kind: FaultKind::Page
        })
    );
    assert_eq!(g.x(0), 0x5555);
    assert_eq!(g.pc(), CODE);

    // When a pre-index store fails, the base register is not written back
    g.set_x(1, 0x5008);
    assert!(matches!(
        g.cpu.exec_insn(0xf81f8c22), // str x2, [x1, #-8]!
        Some(Exit::Fault {
            addr: 0x5000,
            access: Access::Write,
            ..
        })
    ));
    assert_eq!(g.x(1), 0x5008);

    // A read-only page cannot be written
    g.set_x(1, RODATA);
    assert!(matches!(
        g.cpu.exec_insn(0x79000422), // strh w2, [x1, #2]
        Some(Exit::Fault { addr, access: Access::Write, kind: FaultKind::Page }) if addr == RODATA + 2
    ));
}

#[test]
fn sp_based_access_must_be_aligned() {
    let mut g = Guest::new();
    g.cpu.state_mut().sp = DATA + 8;
    assert!(matches!(
        g.cpu.exec_insn(0xf94003e0), // ldr x0, [sp]
        Some(Exit::Fault {
            access: Access::Read,
            kind: FaultKind::StackAlignment,
            ..
        })
    ));
    assert!(matches!(
        g.cpu.exec_insn(0xf90003e0), // str x0, [sp]
        Some(Exit::Fault {
            access: Access::Write,
            kind: FaultKind::StackAlignment,
            ..
        })
    ));
    g.step(0xf98003e0); // prfm pldl1keep, [sp]: prefetch does not check alignment
    g.cpu.state_mut().sp = DATA + 16;
    g.put_u64(DATA + 16, 9);
    g.step(0xf94003e0);
    assert_eq!(g.x(0), 9);
}

#[test]
fn data_accesses_ignore_the_top_byte() {
    let mut g = Guest::new();
    g.put_u64(DATA + 8, 99);
    g.set_x(1, (0x5a << 56) | DATA);
    g.step(0xf9400420); // ldr x0, [x1, #8]
    assert_eq!(g.x(0), 99);
}

#[test]
fn unimplemented_encodings_are_undefined() {
    let mut g = Guest::new();
    for insn in [
        0x1ee84020, // half-precision frint32z is undefined
        0xd50331ff, // sb with nonzero CRm is undefined
        0xd500411f, // cfinv with nonzero CRm is undefined
        0xd503413f, // msr ssbs, #1: SSBS is not advertised
        0xce62c020, // sm3partw1 v0.4s, v1.4s, v2.4s: SM3 is not in the whitelist
        0xcec08420, // sm4e v0.4s, v1.4s: SM4 is not in the whitelist
        0x6e02fc20, // bfdot with size 00: BF16 only has size 01 and 11
        0x2ec2f420, // double-precision fcadd only has the 128-bit form
        0x6e02cc20, // fcmla with size 00 is undefined
        0x4f42e020, // by-element sdot needs 32-bit elements (size = 01 is undefined)
        0xd9800020, // size 11, opc 10: there is no LDAPURS extending to 64 bits
        0xb8a0c020, // ldapr with Rs not all ones (LLVM rejects it)
        0x00000000, // udf #0
        0xd51bd060, // msr tpidrro_el0, x0: EL0 cannot write it
    ] {
        assert_eq!(g.cpu.exec_insn(insn), Some(Exit::Undefined { insn }));
        assert_eq!(g.pc(), CODE);
    }
}

#[test]
fn exclusive_store_succeeds_only_after_a_matching_load() {
    let mut g = Guest::new();
    g.put_u64(DATA, 5);
    g.set_x(1, DATA);
    g.set_x(3, 9);
    g.set_x(2, 7);
    g.step(0xc8027c23); // stxr w2, x3, [x1]: no exclusive mark, fails
    assert_eq!((g.x(2), g.get_u64(DATA)), (1, 5));

    g.step(0xc85f7c20); // ldxr x0, [x1]
    assert_eq!(g.x(0), 5);
    g.step(0xc8027c23); // stxr w2, x3, [x1]
    assert_eq!((g.x(2), g.get_u64(DATA)), (0, 9));

    g.step(0xc85f7c20); // ldxr x0, [x1]
    g.step(0xd5033f5f); // clrex
    g.step(0xc8027c23); // stxr w2, x3, [x1]: the mark has been cleared
    assert_eq!(g.x(2), 1);

    g.set_x(1, DATA + 4); // misaligned: alignment fault
    assert!(matches!(
        g.cpu.exec_insn(0xc85f7c20),
        Some(Exit::Fault {
            kind: FaultKind::Alignment,
            ..
        })
    ));
}

#[test]
fn simd_and_fp_round_trip() {
    if !has_simd() {
        return;
    }
    let mut g = Guest::new();
    g.set_x(1, 0x4045_0000_0000_0000); // 42.0
    g.step(0x9e670020); // fmov d0, x1
    g.step(0x9e660002); // fmov x2, d0
    assert_eq!(g.x(2), 0x4045_0000_0000_0000);
    assert_eq!(g.cpu.state().v[0], [0x4045_0000_0000_0000, 0]);

    let bytes: Vec<u8> = (0..32).collect();
    g.bytes(DATA)[..32].copy_from_slice(&bytes);
    g.set_x(1, DATA);
    g.step(0x4c40a020); // ld1 {v0.16b, v1.16b}, [x1]
    let v = g.cpu.state().v;
    assert_eq!(v[0][0].to_le_bytes(), [0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(v[1][1].to_le_bytes(), [24, 25, 26, 27, 28, 29, 30, 31]);
}

#[test]
fn contiguous_vectors_wrap_and_probe_before_writeback() {
    if !has_simd() {
        return;
    }
    let mut g = Guest::new();
    let end = DATA + 2 * PAGE_SIZE;
    let addr = end - 16;
    g.put_u64(addr, 1);
    g.put_u64(addr + 8, 2);
    g.set_x(1, addr);
    g.cpu.state_mut().v[31] = [0, u64::MAX];
    g.cpu.state_mut().v[0] = [0, u64::MAX];
    g.step(0x0cdf_a03f); // ld1 {v31.8b, v0.8b}, [x1], #16
    assert_eq!(g.cpu.state().v[31], [1, 0]);
    assert_eq!(g.cpu.state().v[0], [2, 0]);
    assert_eq!(g.x(1), end);

    const ST1_POST: u32 = 0x0c9f_a03f; // st1 {v31.8b, v0.8b}, [x1], #16
    let pc = g.pc();
    g.set_x(1, end - 8);
    assert_eq!(
        g.cpu.exec_insn(ST1_POST),
        Some(Exit::Fault {
            addr: end,
            access: Access::Write,
            kind: FaultKind::Page
        })
    );
    assert_eq!(g.get_u64(end - 8), 2);
    assert_eq!((g.pc(), g.x(1)), (pc, end - 8));

    g.set_x(1, addr);
    g.cpu
        .state_mut()
        .set_hw_debug(&[], &[(end - 1, end, tuxport_engine::WATCH_STORE)]);
    g.cpu.state_mut().v[31] = [3, 4];
    assert_eq!(
        g.cpu.exec_insn(ST1_POST),
        Some(Exit::Fault {
            addr,
            access: Access::Write,
            kind: FaultKind::Watchpoint
        })
    );
    assert_eq!(g.get_u64(addr), 1);
    assert_eq!((g.pc(), g.x(1)), (pc, addr));
}

#[test]
fn integer_vector_stubs_preserve_flags_and_other_registers() {
    if !has_simd() {
        return;
    }
    let mut g = Guest::new();
    let s = g.cpu.state_mut();
    s.fpcr = 1 << 22;
    s.fpsr = 1 << 27;
    s.nzcv = 0xb000_0000;
    let mut expected = std::array::from_fn(|i| [0x1111 * i as u64, u64::MAX - i as u64]);
    s.v = expected;
    g.step(0x2e69_1fec); // bsl v12.8b, v31.8b, v9.8b: old Vd supplies the mask
    let mask = expected[12][0];
    expected[12] = [(mask & expected[31][0]) | (!mask & expected[9][0]), 0];
    assert_eq!(g.cpu.state().v, expected);
    g.step(0x4eec_858c); // add v12.2d, v12.2d, v12.2d: all operands alias
    expected[12][0] *= 2;
    let s = g.cpu.state();
    assert_eq!(s.v, expected);
    assert_eq!((s.fpcr, s.fpsr, s.nzcv), (1 << 22, 1 << 27, 0xb000_0000));
}

#[test]
fn id_registers_match_hwcap() {
    let mut g = Guest::new();
    g.step(0xd5380600); // mrs x0, id_aa64isar0_el1
    let isar0 = g.x(0);
    let caps = tuxport_engine::guest_hwcap();
    assert_eq!(caps & hwcap::CRC32 != 0, (isar0 >> 16) & 0xf == 1);
    assert_eq!(caps & hwcap::ATOMICS != 0, (isar0 >> 20) & 0xf == 2);
    let sha2 = if caps & hwcap::SHA512 != 0 { 2 } else { 1 };
    assert_eq!(caps & hwcap::SHA2 != 0, (isar0 >> 12) & 0xf == sha2);
    assert_eq!(caps & hwcap::SHA3 != 0, (isar0 >> 32) & 0xf == 1);
    assert_eq!(caps & hwcap::ASIMDDP != 0, (isar0 >> 44) & 0xf == 1);
    assert_eq!(caps & hwcap::ASIMDRDM != 0, (isar0 >> 28) & 0xf == 1);
    assert_eq!(caps & hwcap::ASIMDFHM != 0, (isar0 >> 48) & 0xf == 1);
    let caps2 = tuxport_engine::guest_hwcap2();
    let ts = match (caps & hwcap::FLAGM != 0, caps2 & hwcap2::FLAGM2 != 0) {
        (_, true) => 2,
        (true, false) => 1,
        _ => 0,
    };
    assert_eq!((isar0 >> 52) & 0xf, ts);
    // FlagM2 and DPB2 are advertised only on top of the previous level
    assert!(caps2 & hwcap2::FLAGM2 == 0 || caps & hwcap::FLAGM != 0);
    assert!(caps2 & hwcap2::DCPODP == 0 || caps & hwcap::DCPOP != 0);
    // FHM is advertised only together with FP16
    assert!(caps & hwcap::ASIMDFHM == 0 || caps & hwcap::FPHP != 0);
    // SHA3 and SHA512 are advertised only together
    assert_eq!(caps & hwcap::SHA3 != 0, caps & hwcap::SHA512 != 0);
    g.step(0xd5380620); // mrs x0, id_aa64isar1_el1
    let i8mm = tuxport_engine::guest_hwcap2() & hwcap2::I8MM != 0;
    assert_eq!(i8mm, (g.x(0) >> 52) & 0xf == 1);
    let lrcpc = match (caps & hwcap::LRCPC != 0, caps & hwcap::ILRCPC != 0) {
        (_, true) => 2,
        (true, false) => 1,
        _ => 0,
    };
    assert_eq!((g.x(0) >> 20) & 0xf, lrcpc);
    assert_eq!(caps & hwcap::JSCVT != 0, (g.x(0) >> 12) & 0xf == 1);
    assert_eq!(caps & hwcap::FCMA != 0, (g.x(0) >> 16) & 0xf == 1);
    let bf16 = tuxport_engine::guest_hwcap2() & hwcap2::BF16 != 0;
    assert_eq!(bf16, (g.x(0) >> 44) & 0xf == 1);
    let dpb = match (caps & hwcap::DCPOP != 0, caps2 & hwcap2::DCPODP != 0) {
        (_, true) => 2,
        (true, false) => 1,
        _ => 0,
    };
    assert_eq!(g.x(0) & 0xf, dpb);
    assert_eq!(caps2 & hwcap2::FRINT != 0, (g.x(0) >> 32) & 0xf == 1);
    assert_eq!(caps & hwcap::SB != 0, (g.x(0) >> 36) & 0xf == 1);
    // FP and AdvSIMD of ID_AA64PFR0_EL1: 1 when half precision is included; the two are
    // advertised only together
    g.step(0xd5380400); // mrs x0, id_aa64pfr0_el1
    let (fp, simd) = ((g.x(0) >> 16) & 0xf, (g.x(0) >> 20) & 0xf);
    if caps & hwcap::FP != 0 {
        assert_eq!(fp, (caps & hwcap::FPHP != 0) as u64);
        assert_eq!(simd, (caps & hwcap::ASIMDHP != 0) as u64);
    }
    assert_eq!(caps & hwcap::FPHP != 0, caps & hwcap::ASIMDHP != 0);
    assert_eq!(caps & hwcap::DIT != 0, (g.x(0) >> 48) & 0xf == 1);
}

#[test]
fn ldapr_follows_the_feature_set() {
    let mut g = Guest::new();
    let ldapr = 0xf8bfc020; // ldapr x0, [x1]
    if tuxport_engine::guest_hwcap() & hwcap::LRCPC == 0 {
        assert_eq!(
            g.cpu.exec_insn(ldapr),
            Some(Exit::Undefined { insn: ldapr })
        );
        return;
    }
    g.put_u64(DATA, 0x1122_3344_5566_7788);
    g.set_x(1, DATA);
    g.step(ldapr);
    assert_eq!(g.x(0), 0x1122_3344_5566_7788);
    g.step(0x78bfc022); // ldaprh w2, [x1]
    assert_eq!(g.x(2), 0x7788);
    g.set_x(1, DATA + 1);
    g.step(0x38bfc023); // ldaprb w3, [x1]
    assert_eq!(g.x(3), 0x77);
    // Readable on a read-only page as well
    g.set_x(1, RODATA);
    g.step(0xb8bfc024); // ldapr w4, [x1]
    assert_eq!(g.x(4), g.get_u64(RODATA) & 0xffff_ffff);
    // Requires alignment, like LDAR
    g.set_x(1, DATA + 1);
    assert!(matches!(
        g.cpu.exec_insn(0xb8bfc020),
        Some(Exit::Fault {
            kind: FaultKind::Alignment,
            ..
        })
    ));
}

#[test]
fn lrcpc2_follows_the_feature_set() {
    let mut g = Guest::new();
    let ldapur = 0x99404020; // ldapur w0, [x1, #4]
    if tuxport_engine::guest_hwcap() & hwcap::ILRCPC == 0 {
        assert_eq!(
            g.cpu.exec_insn(ldapur),
            Some(Exit::Undefined { insn: ldapur })
        );
        return;
    }
    g.put_u64(DATA, 0x8899_aabb_ccdd_eeff);
    g.set_x(1, DATA);
    g.step(ldapur);
    assert_eq!(g.x(0), 0x8899_aabb);
    g.set_x(1, DATA + 8);
    g.step(0x199fd022); // ldapursb x2, [x1, #-3]: the 5th byte 0xaa sign-extended to 64 bits
    assert_eq!(g.x(2), 0xffff_ffff_ffff_ffaa);
    g.step(0x19dfd023); // ldapursb w3, [x1, #-3]: extended to 32 bits
    assert_eq!(g.x(3), 0xffff_ffaa);
    g.set_x(4, 0x1234_5678_9abc_def0);
    g.step(0xd9000024); // stlur x4, [x1]
    assert_eq!(g.get_u64(DATA + 8), 0x1234_5678_9abc_def0);
    // Requires alignment, like LDAR
    g.set_x(1, DATA + 1);
    assert!(matches!(
        g.cpu.exec_insn(ldapur),
        Some(Exit::Fault {
            kind: FaultKind::Alignment,
            ..
        })
    ));
}

#[test]
fn sha3_and_i8mm_follow_the_feature_set() {
    if !has_simd() {
        return;
    }
    let mut g = Guest::new();
    let eor3 = 0xce020c20; // eor3 v0.16b, v1.16b, v2.16b, v3.16b
    let smmla = 0x4e85a483; // smmla v3.4s, v4.16b, v5.16b
    if tuxport_engine::guest_hwcap() & hwcap::SHA3 == 0 {
        assert_eq!(g.cpu.exec_insn(eor3), Some(Exit::Undefined { insn: eor3 }));
    } else {
        let s = g.cpu.state_mut();
        s.v[1] = [0x0f0f_0f0f_0f0f_0f0f, 1];
        s.v[2] = [0x00ff_00ff_00ff_00ff, 2];
        s.v[3] = [0x3333_3333_3333_3333, 4];
        g.step(eor3);
        assert_eq!(
            g.cpu.state().v[0],
            [
                0x0f0f_0f0f_0f0f_0f0f ^ 0x00ff_00ff_00ff_00ff ^ 0x3333_3333_3333_3333,
                7
            ]
        );
    }
    if tuxport_engine::guest_hwcap2() & hwcap2::I8MM == 0 {
        assert_eq!(
            g.cpu.exec_insn(smmla),
            Some(Exit::Undefined { insn: smmla })
        );
        return;
    }
    // 2x8 times 8x2: the two rows of v4 are 1..=8 and all -1, the two columns of v5 are all 1 and
    // 0, 1, 0, 1, ...
    let row0: Vec<u8> = (1..=8).collect();
    let s = g.cpu.state_mut();
    s.v[3] = [100, 0];
    s.v[4] = [u64::from_le_bytes(row0.try_into().unwrap()), u64::MAX];
    s.v[5] = [0x0101_0101_0101_0101, 0x0100_0100_0100_0100];
    g.step(smmla);
    let v = g.cpu.state().v[3];
    let lanes = [
        v[0] as u32 as i32,
        (v[0] >> 32) as i32,
        v[1] as u32 as i32,
        (v[1] >> 32) as i32,
    ];
    // [r0·c0 + 100, r0·c1, r1·c0, r1·c1] = [36 + 100, 2 + 4 + 6 + 8, -8, -4]
    assert_eq!(lanes, [136, 20, -8, -4]);
}

#[test]
fn dot_product_follows_the_feature_set() {
    if !has_simd() {
        return;
    }
    let mut g = Guest::new();
    let sdot = 0x4e829420; // sdot v0.4s, v1.16b, v2.16b
    if tuxport_engine::guest_hwcap() & hwcap::ASIMDDP == 0 {
        assert_eq!(g.cpu.exec_insn(sdot), Some(Exit::Undefined { insn: sdot }));
        return;
    }
    // The bytes of v1 are -1, 2, -3, 4, ..., v2 is all 3: each group of four sums to
    // -3 + 6 - 9 + 12 = 6
    let a: Vec<u8> = (1..=16i8)
        .map(|i| if i % 2 == 1 { -i } else { i } as u8)
        .collect();
    let s = g.cpu.state_mut();
    s.v[0] = [10 | 20 << 32, 30 | 40 << 32];
    s.v[1] = [
        u64::from_le_bytes(a[..8].try_into().unwrap()),
        u64::from_le_bytes(a[8..].try_into().unwrap()),
    ];
    s.v[2] = [0x0303_0303_0303_0303; 2];
    s.v[3] = [0; 2];
    g.step(sdot);
    let lanes = |v: [u64; 2]| {
        [
            v[0] as u32,
            (v[0] >> 32) as u32,
            v[1] as u32,
            (v[1] >> 32) as u32,
        ]
    };
    assert_eq!(lanes(g.cpu.state().v[0]), [16, 26, 36, 46]);
    // udot v3.4s, v1.16b, v2.4b[1]: the 32-bit element 1 of v2 is 3, 3, 3, 3, taken as unsigned
    g.step(0x6fa2e023);
    let sum = |k: usize| {
        a[4 * k..4 * k + 4]
            .iter()
            .map(|&b| b as u32 * 3)
            .sum::<u32>()
    };
    assert_eq!(lanes(g.cpu.state().v[3]), [sum(0), sum(1), sum(2), sum(3)]);
}

#[test]
fn half_precision_follows_the_feature_set() {
    if !has_simd() {
        return;
    }
    let mut g = Guest::new();
    let fadd = 0x1ee22820; // fadd h0, h1, h2
    let fmla = 0x4e420c20; // fmla v0.8h, v1.8h, v2.8h
    let fmov = 0x4f03fe00; // fmov v0.8h, #1.0
    if tuxport_engine::guest_hwcap() & hwcap::FPHP == 0 {
        for insn in [fadd, fmla, fmov] {
            assert_eq!(g.cpu.exec_insn(insn), Some(Exit::Undefined { insn }));
        }
        return;
    }
    // 1.0 + 2.0 = 3.0 (0x3c00 + 0x4000 = 0x4200); a scalar write clears the high part
    let s = g.cpu.state_mut();
    s.v[0] = [u64::MAX; 2];
    s.v[1] = [0x3c00, 0];
    s.v[2] = [0x4000, 0];
    g.step(fadd);
    assert_eq!(g.cpu.state().v[0], [0x4200, 0]);
    // Each half-precision element: 1.0 + 1.5 × 2.0 = 4.0 (0x4400)
    let s = g.cpu.state_mut();
    s.v[0] = [0x3c00_3c00_3c00_3c00; 2];
    s.v[1] = [0x3e00_3e00_3e00_3e00; 2];
    s.v[2] = [0x4000_4000_4000_4000; 2];
    g.step(fmla);
    assert_eq!(g.cpu.state().v[0], [0x4400_4400_4400_4400; 2]);
    g.step(fmov);
    assert_eq!(g.cpu.state().v[0], [0x3c00_3c00_3c00_3c00; 2]);
}

/// One or two instructions each from RDM, FCMA, FHM, BF16: executable only when advertised, with
/// results as the architecture defines them.
/// FlagM, FlagM2, SB, DPB, DPB2: undefined when not advertised; when advertised, NZCV changes as
/// in the ARM ARM pseudocode, SB does nothing, and DC CVAP and CVADP only check the address for
/// read access.
#[test]
fn flagm_sb_dpb_follow_the_feature_set() {
    let mut g = Guest::new();
    let caps = tuxport_engine::guest_hwcap();
    let caps2 = tuxport_engine::guest_hwcap2();
    let flagm = caps & hwcap::FLAGM != 0;
    let flagm2 = caps2 & hwcap2::FLAGM2 != 0;
    let nzcv = |n: u64, z: u64, c: u64, v: u64| n << 31 | z << 30 | c << 29 | v << 28;
    let cases = [
        (flagm, 0xd500401f),                       // cfinv
        (flagm, 0xba02040a),                       // rmif x0, #4, #0b1010
        (flagm, 0x3a00080d),                       // setf8 w0
        (flagm, 0x3a00480d),                       // setf16 w0
        (flagm2, 0xd500403f),                      // xaflag
        (flagm2, 0xd500405f),                      // axflag
        (caps & hwcap::SB != 0, 0xd50330ff),       // sb
        (caps & hwcap::DCPOP != 0, 0xd50b7c21),    // dc cvap, x1
        (caps2 & hwcap2::DCPODP != 0, 0xd50b7d21), // dc cvadp, x1
    ];
    for (on, insn) in cases {
        if !on {
            assert_eq!(g.cpu.exec_insn(insn), Some(Exit::Undefined { insn }));
            continue;
        }
        match insn {
            0xd500401f => {
                for f in 0..16u64 {
                    g.cpu.state_mut().nzcv = f << 28;
                    g.step(insn);
                    assert_eq!(g.cpu.state().nzcv, (f ^ 2) << 28);
                }
            }
            0xba02040a => {
                // 0xa0 rotated right by 4, low 4 bits 1010: mask 1010 selects N and C; Z and V
                // are unchanged
                g.set_x(0, 0xa0);
                g.cpu.state_mut().nzcv = nzcv(0, 1, 0, 1);
                g.step(insn);
                assert_eq!(g.cpu.state().nzcv, nzcv(1, 1, 1, 1));
                g.set_x(0, 0x50);
                g.step(insn);
                assert_eq!(g.cpu.state().nzcv, nzcv(0, 1, 0, 1));
            }
            0x3a00080d | 0x3a00480d => {
                let top = if insn == 0x3a00080d { 7 } else { 15 };
                for w in [0u64, 0x80, 0x180, 0x100, 0x7f, 0x8000, 0x18000, 0x1_0000] {
                    g.set_x(0, w);
                    g.cpu.state_mut().nzcv = nzcv(0, 0, 1, 0);
                    g.step(insn);
                    let low = w & ((2 << top) - 1);
                    let (n, v) = ((w >> top) & 1, ((w >> (top + 1)) ^ (w >> top)) & 1);
                    assert_eq!(
                        g.cpu.state().nzcv,
                        nzcv(n, (low == 0) as u64, 1, v),
                        "{w:#x}"
                    );
                }
            }
            0xd500403f | 0xd500405f => {
                for f in 0..16u64 {
                    let (z, c, v) = ((f >> 2) & 1, (f >> 1) & 1, f & 1);
                    g.cpu.state_mut().nzcv = f << 28;
                    g.step(insn);
                    let want = if insn == 0xd500403f {
                        nzcv((c | z) ^ 1, z & c, c | z, (c ^ 1) & z)
                    } else {
                        nzcv(0, z | v, c & (v ^ 1), 0)
                    };
                    assert_eq!(g.cpu.state().nzcv, want, "flags {f:#x}");
                }
            }
            0xd50330ff => {
                g.cpu.state_mut().nzcv = nzcv(1, 0, 1, 0);
                g.step(insn);
                assert_eq!(g.cpu.state().nzcv, nzcv(1, 0, 1, 0));
            }
            _ => {
                g.set_x(1, RODATA + 8);
                g.step(insn);
                g.set_x(1, 0x5000);
                let pc = g.pc();
                assert!(matches!(
                    g.cpu.exec_insn(insn),
                    Some(Exit::Fault {
                        addr: 0x5000,
                        access: Access::Read,
                        ..
                    })
                ));
                assert_eq!(g.pc(), pc);
            }
        }
    }
}

/// DIT: MSR DIT, #imm takes the lowest bit of CRm as the hardware does, and MSR and MRS DIT have
/// only bit 24; when not advertised they are all undefined instructions.
#[test]
fn dit_follows_the_feature_set() {
    let mut g = Guest::new();
    let mrs = 0xd53b42a0; // mrs x0, dit
    let msr = 0xd51b42a1; // msr dit, x1
    let imm = |crm: u32| 0xd503405f | crm << 8; // msr dit, #crm
    if tuxport_engine::guest_hwcap() & hwcap::DIT == 0 {
        for insn in [mrs, msr, imm(1)] {
            assert_eq!(g.cpu.exec_insn(insn), Some(Exit::Undefined { insn }));
        }
        return;
    }
    // The host thread's DIT follows the guest's (tuxport_sync_dit)
    #[cfg(target_arch = "aarch64")]
    let host = || {
        let v: u64;
        // SAFETY: mrs x0, dit (DIT being advertised means the host has it)
        unsafe { std::arch::asm!(".inst 0xd53b42a0", out("x0") v) };
        v >> 24
    };
    #[cfg(not(target_arch = "aarch64"))]
    let host = || 0;
    assert_eq!(g.cpu.state().dit, 0);
    for (crm, want) in [(1, 1), (0, 0), (3, 1), (2, 0), (15, 1), (14, 0)] {
        g.step(imm(crm));
        assert_eq!(g.cpu.state().dit, want, "crm {crm}");
        assert_eq!(host(), u64::from(want), "crm {crm}");
        g.step(mrs);
        assert_eq!(g.x(0), u64::from(want) << 24);
    }
    g.set_x(1, !0);
    g.step(msr);
    g.step(mrs);
    assert_eq!(g.x(0), 1 << 24);
    assert_eq!(host(), 1);
    g.set_x(1, !(1 << 24));
    g.step(msr);
    assert_eq!(g.cpu.state().dit, 0);
    assert_eq!(host(), 0);
}

/// FRINTTS: rounds to an integral value; values outside the 32- or 64-bit integer range and NaN
/// give the most negative integer and set FPSR.IOC. The host does the computation; this checks
/// the whitelist and where the registers go.
#[test]
fn frintts_follows_the_feature_set() {
    if !has_simd() {
        return;
    }
    let mut g = Guest::new();
    let frint32z = 0x1e284020; // frint32z s0, s1
    let frint64x = 0x1e69c020; // frint64x d0, d1
    let vfrint32x = 0x6e21e820; // frint32x v0.4s, v1.4s
    if tuxport_engine::guest_hwcap2() & hwcap2::FRINT == 0 {
        for insn in [frint32z, frint64x, vfrint32x] {
            assert_eq!(g.cpu.exec_insn(insn), Some(Exit::Undefined { insn }));
        }
        return;
    }
    let s = g.cpu.state_mut();
    s.v[1] = [3.7f32.to_bits() as u64, 0];
    s.fpsr = 0;
    g.step(frint32z);
    assert_eq!(g.cpu.state().v[0], [3.0f32.to_bits() as u64, 0]);
    assert_eq!(g.cpu.state().fpsr & 1, 0);
    g.cpu.state_mut().v[1] = [3e10f32.to_bits() as u64, 0];
    g.step(frint32z);
    assert_eq!(g.cpu.state().v[0][0], (-2147483648.0f32).to_bits() as u64);
    assert_eq!(g.cpu.state().fpsr & 1, 1);
    g.cpu.state_mut().v[1] = [2.5f64.to_bits(), 0];
    g.step(frint64x);
    assert_eq!(g.cpu.state().v[0], [2.0f64.to_bits(), 0]);
    let lanes = |a: [f32; 4]| {
        let b = a.map(|f| f.to_bits() as u64);
        [b[0] | b[1] << 32, b[2] | b[3] << 32]
    };
    g.cpu.state_mut().v[1] = lanes([1.5, -1.5, 1e10, f32::NAN]);
    g.step(vfrint32x);
    assert_eq!(
        g.cpu.state().v[0],
        lanes([2.0, -2.0, -2147483648.0, -2147483648.0])
    );
}

#[test]
fn rdm_fcma_fhm_bf16_follow_the_feature_set() {
    if !has_simd() {
        return;
    }
    let mut g = Guest::new();
    let caps = tuxport_engine::guest_hwcap();
    let sqrdmlah = 0x6e428420; // sqrdmlah v0.8h, v1.8h, v2.8h
    let fcadd = 0x6ec2f420; // fcadd v0.2d, v1.2d, v2.2d, #270
    let fmlal = 0x4e22ec20; // fmlal v0.4s, v1.4h, v2.4h
    let bfdot = 0x6e42fc20; // bfdot v0.4s, v1.8h, v2.8h
    let bfcvt = 0x1e634020; // bfcvt h0, s1
    let cases = [
        (caps & hwcap::ASIMDRDM != 0, sqrdmlah),
        (caps & hwcap::FCMA != 0, fcadd),
        (caps & hwcap::ASIMDFHM != 0, fmlal),
        (tuxport_engine::guest_hwcap2() & hwcap2::BF16 != 0, bfdot),
        (tuxport_engine::guest_hwcap2() & hwcap2::BF16 != 0, bfcvt),
    ];
    for (on, insn) in cases {
        if !on {
            assert_eq!(g.cpu.exec_insn(insn), Some(Exit::Undefined { insn }));
            continue;
        }
        let s = g.cpu.state_mut();
        match insn {
            // 0.5 × 0.5 accumulated into a Q15 value: d + 0x2000, the upper half saturating to
            // 0x7fff
            0x6e428420 => {
                s.v[0] = [0x1000_1000_1000_1000, 0x7000_7000_7000_7000];
                s.v[1] = [0x4000_4000_4000_4000; 2];
                s.v[2] = [0x4000_4000_4000_4000; 2];
            }
            // (1 + 2i) + (−i)(3 + 4i) = 5 − i
            0x6ec2f420 => {
                s.v[1] = [1f64.to_bits(), 2f64.to_bits()];
                s.v[2] = [3f64.to_bits(), 4f64.to_bits()];
            }
            // (1 + 2^-10)² accumulated in single precision: 1 + 2^-9 + 2^-20, whose last term
            // half precision cannot represent
            0x4e22ec20 => {
                s.v[0] = [0; 2];
                s.v[1] = [0x3c01_3c01_3c01_3c01, u64::MAX];
                s.v[2] = [0x3c01_3c01_3c01_3c01, u64::MAX];
            }
            // Each single-precision element: 1.0 + 2 × 3 + 2 × 3 = 13.0
            0x6e42fc20 => {
                s.v[0] = [0x3f80_0000_3f80_0000; 2];
                s.v[1] = [0x4000_4000_4000_4000; 2];
                s.v[2] = [0x4040_4040_4040_4040; 2];
            }
            // 0x3f818000 is exactly halfway between 0x3f81 and 0x3f82 and rounds to even; a
            // scalar write clears the high part
            _ => {
                s.v[0] = [u64::MAX; 2];
                s.v[1] = [0x3f81_8000, 0];
            }
        }
        g.step(insn);
        let want = match insn {
            0x6e428420 => [0x3000_3000_3000_3000, 0x7fff_7fff_7fff_7fff],
            0x6ec2f420 => [5f64.to_bits(), (-1f64).to_bits()],
            0x4e22ec20 => [0x3f80_4008_3f80_4008; 2],
            0x6e42fc20 => [0x4150_0000_4150_0000; 2],
            _ => [0x3f82, 0],
        };
        assert_eq!(g.cpu.state().v[0], want, "{insn:#010x}");
    }
}

#[test]
fn javascript_conversion_follows_the_feature_set() {
    if !has_simd() {
        return;
    }
    let mut g = Guest::new();
    let fjcvtzs = 0x1e7e0020; // fjcvtzs w0, d1
    if tuxport_engine::guest_hwcap() & hwcap::JSCVT == 0 {
        assert_eq!(
            g.cpu.exec_insn(fjcvtzs),
            Some(Exit::Undefined { insn: fjcvtzs })
        );
        return;
    }
    // Modulo 2^32 as JavaScript's ToInt32 does; only Z is set when exact
    for (d, w, z) in [
        (3.0f64, 3u64, true),
        (4294967301.0, 5, false),
        (-1.5, u32::MAX as u64, false),
    ] {
        let s = g.cpu.state_mut();
        s.v[1] = [d.to_bits(), 0];
        s.nzcv = 0xf000_0000;
        g.set_x(0, u64::MAX);
        g.step(fjcvtzs);
        assert_eq!(g.x(0), w, "{d}");
        assert_eq!(g.cpu.state().nzcv, if z { 0x4000_0000 } else { 0 }, "{d}");
    }
}

#[test]
fn branches() {
    let mut g = Guest::new();
    assert_eq!(g.cpu.exec_insn(0x14000002), None); // b .+8
    assert_eq!(g.pc(), CODE + 8);

    g.cpu.state_mut().pc = CODE;
    assert_eq!(g.cpu.exec_insn(0x94000002), None); // bl .+8
    assert_eq!((g.pc(), g.x(30)), (CODE + 8, CODE + 4));

    g.cpu.state_mut().pc = CODE;
    g.set_x(0, 0);
    assert_eq!(g.cpu.exec_insn(0xb4000040), None); // cbz x0, .+8
    assert_eq!(g.pc(), CODE + 8);
    g.set_x(0, 1);
    g.step(0xb4000040);

    g.set_x(0, 0b1000);
    let pc = g.pc();
    assert_eq!(g.cpu.exec_insn(0x37180040), None); // tbnz w0, #3, .+8
    assert_eq!(g.pc(), pc + 8);

    g.cpu.state_mut().nzcv = 0;
    g.step(0x54000040); // b.eq .+8: Z=0, not taken
    g.cpu.state_mut().nzcv = 1 << 30;
    let pc = g.pc();
    assert_eq!(g.cpu.exec_insn(0x54000040), None);
    assert_eq!(g.pc(), pc + 8);

    g.set_x(30, 0x1234_5670);
    assert_eq!(g.cpu.exec_insn(0xd65f03c0), None); // ret
    assert_eq!(g.pc(), 0x1234_5670);

    g.set_x(1, CODE);
    assert_eq!(g.cpu.exec_insn(0xd63f0020), None); // blr x1
    assert_eq!((g.pc(), g.x(30)), (CODE, 0x1234_5674));
}

#[test]
fn exceptions_hints_and_system_registers() {
    let mut g = Guest::new();
    assert_eq!(g.cpu.exec_insn(0xd4000001), Some(Exit::Svc { imm: 0 })); // svc #0
    assert_eq!(g.pc(), CODE + 4);
    assert_eq!(
        g.cpu.exec_insn(0xd4200020),
        Some(Exit::Breakpoint { imm: 1 })
    ); // brk #1
    assert_eq!(g.pc(), CODE + 4);

    g.step(0xd503201f); // nop
    g.set_x(30, 0x4000_1000);
    g.step(0xd503233f); // paciasp: treated as NOP per the engine map
    assert_eq!(g.x(30), 0x4000_1000);
    g.step(0xd5033bbf); // dmb ish

    g.set_x(1, 0xfeed);
    g.step(0xd51bd041); // msr tpidr_el0, x1
    g.step(0xd53bd040); // mrs x0, tpidr_el0
    assert_eq!(g.x(0), 0xfeed);
    assert_eq!(g.cpu.state().tpidr_el0, 0xfeed);
    g.step(0xd53bd060); // mrs x0, tpidrro_el0
    assert_eq!(g.x(0), 0);

    g.step(0xd53b00e5); // mrs x5, dczid_el0: 64-byte blocks, DC ZVA allowed
    assert_eq!(g.x(5), 4);
    g.bytes(DATA + 0x80)[..0x100].fill(0xaa);
    g.set_x(3, DATA + 0xc5);
    g.step(0xd50b7423); // dc zva, x3
    assert!(g.bytes(DATA + 0xc0)[..64].iter().all(|&b| b == 0));
    assert_eq!(g.bytes(DATA + 0xbf)[0], 0xaa);
    assert_eq!(g.bytes(DATA + 0x100)[0], 0xaa);
    g.set_x(3, RODATA + 8);
    assert_eq!(
        g.cpu.exec_insn(0xd50b7423),
        Some(Exit::Fault {
            addr: RODATA,
            access: Access::Write,
            kind: FaultKind::Page
        })
    );
}

#[test]
fn run_fetches_through_the_tlb() {
    let mut g = Guest::new();
    g.bytes(CODE)[..8].copy_from_slice(&[0x40, 0x05, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4]); // movz x0, #42; svc #0
    assert_eq!(g.cpu.run(), Exit::Svc { imm: 0 });
    assert_eq!(g.x(0), 42);

    // Falling through from the last word must check the new page before fetching.
    let last = CODE + PAGE_SIZE - 4;
    g.bytes(last)[..4].copy_from_slice(&0xd503_201fu32.to_le_bytes()); // nop
    g.cpu.state_mut().pc = last;
    assert_eq!(
        g.cpu.run(),
        Exit::Fault {
            addr: last + 4,
            access: Access::Exec,
            kind: FaultKind::Page
        }
    );
    assert_eq!(g.pc(), last + 4);

    // Misaligned pc
    g.cpu.state_mut().pc = CODE + 2;
    assert!(matches!(
        g.cpu.run(),
        Exit::Fault {
            kind: FaultKind::Alignment,
            ..
        }
    ));

    // Data pages are not executable
    g.cpu.state_mut().pc = DATA;
    assert_eq!(
        g.cpu.run(),
        Exit::Fault {
            addr: DATA,
            access: Access::Exec,
            kind: FaultKind::Page
        }
    );

    // After unmapping and flushing the TLB, instruction fetch fails
    g.mmu.unmap(CODE, PAGE_SIZE).unwrap();
    g.cpu.tlb_flush();
    g.cpu.state_mut().pc = CODE;
    assert!(matches!(
        g.cpu.run(),
        Exit::Fault {
            addr: CODE,
            access: Access::Exec,
            ..
        }
    ));
}

#[test]
fn run_stops_at_a_breakpoint_on_the_current_page() {
    let mut g = Guest::new();
    g.bytes(CODE)[..4].copy_from_slice(&0xd503_201fu32.to_le_bytes()); // nop
    g.cpu.state_mut().set_hw_debug(&[CODE + 4], &[]);
    assert_eq!(
        g.cpu.run(),
        Exit::Fault {
            addr: CODE + 4,
            access: Access::Exec,
            kind: FaultKind::HwBreakpoint
        }
    );
    assert_eq!(g.pc(), CODE + 4);
}

/// SIMD&FP and native stubs are available only on aarch64 Linux hosts (HWCAP reflects this
/// faithfully; see the engine map).
fn has_simd() -> bool {
    let need = hwcap::FP | hwcap::ASIMD;
    tuxport_engine::guest_hwcap() & need == need
}
