//! Tier 1 keeping translations after the address space changes and checking guest instruction
//! words block by block (`Dbt::recheck`, and instances taken from the pool), function return
//! prediction (the shadow return stack), cross-page accesses, and hardware watchpoints. When the
//! host is not aarch64 Linux there is no Tier 1 and the tests pass trivially.

use std::ptr::NonNull;

use tuxport_engine::{
    Access, Cpu, Dbt, DbtRegs, DbtSource, DbtSpace, Exit, FaultKind, Mmu, PAGE_SIZE, WATCH_LOAD,
    WATCH_STORE, prot,
};

#[repr(C, align(4096))]
struct Page([u8; 4096]);

/// Two pages of code, `CODE` and the page right after it; below `STACK` is one page of stack.
const CODE: u64 = 0x40_0000;
const STACK: u64 = 0x80_0000;
/// The data pages mapped by [`Guest::with_data`] start here; see the description there.
const DATA: u64 = 0x100_0000;

const SVC: u32 = 0xd400_0001;
const RET: u32 = 0xd65f_03c0;
const STP_FP_LR_PRE: u32 = 0xa9bf_7bfd; // stp x29, x30, [sp, #-16]!
const LDP_FP_LR_POST: u32 = 0xa8c1_7bfd; // ldp x29, x30, [sp], #16
const SUB_X0_1: u32 = 0xd100_0400; // sub x0, x0, #1
const ADD_X1_1: u32 = 0x9100_0421; // add x1, x1, #1
const LDR_X0_X2: u32 = 0xf940_0040; // ldr x0, [x2]
const STR_X3_X4: u32 = 0xf900_0083; // str x3, [x4]
const LDP_X5_X6_X7: u32 = 0xa940_18e5; // ldp x5, x6, [x7]

struct Guest {
    // Drop order: cpu → mmu → pages
    cpu: Cpu,
    mmu: Mmu,
    pages: Box<[Page]>,
}

impl Guest {
    fn new() -> Self {
        Self::with_pages(3)
    }

    /// Maps 6 data pages in addition to [`Guest::new`]; every byte of page i is `i * 16` plus the
    /// low 4 bits of its offset in the page:
    /// the host pages of `DATA` and `DATA + 1 page` are adjacent; those of `+ 2 pages` and
    /// `+ 3 pages` are in reverse order;
    /// `+ 4 pages` is read-write, `+ 5 pages` read-only, and `+ 6 pages` unmapped.
    fn with_data() -> Self {
        let mut g = Self::with_pages(9);
        for (i, page) in g.pages[3..].iter_mut().enumerate() {
            for (j, byte) in page.0.iter_mut().enumerate() {
                *byte = (i * 16 + j % 16) as u8;
            }
        }
        let base = NonNull::new(g.pages.as_mut_ptr().cast::<u8>()).unwrap();
        let rw = prot::READ | prot::WRITE;
        let layout = [
            (0, 3, rw),
            (1, 4, rw),
            (2, 6, rw),
            (3, 5, rw),
            (4, 7, rw),
            (5, 8, prot::READ),
        ];
        for (page, host, p) in layout {
            unsafe {
                g.mmu
                    .map(DATA + page * PAGE_SIZE, base.add(host * 4096), PAGE_SIZE, p)
                    .unwrap();
            }
        }
        g
    }

    fn with_pages(n: usize) -> Self {
        let mut pages: Box<[Page]> = (0..n).map(|_| Page([0; 4096])).collect();
        let mut mmu = Mmu::new();
        let base = NonNull::new(pages.as_mut_ptr().cast::<u8>()).unwrap();
        unsafe {
            mmu.map(CODE, base, 2 * PAGE_SIZE, prot::READ | prot::EXEC)
                .unwrap();
            mmu.map(
                STACK - PAGE_SIZE,
                base.add(2 * 4096),
                PAGE_SIZE,
                prot::READ | prot::WRITE,
            )
            .unwrap();
        }
        let mut cpu = unsafe { Cpu::new(&mmu) };
        cpu.state_mut().sp = STACK;
        Guest { cpu, mmu, pages }
    }

    /// Writes instructions to `addr` (only the two pages this test maps are supported).
    fn code(&mut self, addr: u64, insns: &[u32]) {
        let off = (addr - CODE) as usize;
        for (i, insn) in insns.iter().enumerate() {
            let at = off + 4 * i;
            self.pages[at / 4096].0[at % 4096..at % 4096 + 4].copy_from_slice(&insn.to_le_bytes());
        }
    }

    /// Runs from `CODE` to an svc and returns x0.
    fn run(&mut self, dbt: &mut Dbt) -> u64 {
        self.run_at(dbt, CODE)
    }

    /// Runs from `pc` to an svc and returns x0.
    fn run_at(&mut self, dbt: &mut Dbt, pc: u64) -> u64 {
        let _ = &self.mmu;
        self.cpu.state_mut().pc = pc;
        assert!(matches!(dbt.run(&mut self.cpu), Exit::Svc { imm: 0 }));
        self.cpu.state().x[0]
    }

    /// The 8 bytes at offset `off` of data page `page` (not crossing pages).
    fn data(&self, page: usize, off: usize) -> u64 {
        let host = [3, 4, 6, 5, 7, 8][page];
        u64::from_le_bytes(self.pages[host].0[off..off + 8].try_into().unwrap())
    }
}

#[test]
fn recheck_after_exec_runs_the_new_code() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    g.code(CODE, &[movz_x0(1), SVC]);
    assert_eq!(g.run(&mut dbt), 1);
    // The same address now holds another program (exec); after the check, the new code runs
    g.code(CODE, &[movz_x0(2), SVC]);
    dbt.recheck();
    assert_eq!(g.run(&mut dbt), 2);
    // When the code has not changed it is still usable after the check, without retranslation
    let blocks = dbt.stats().blocks;
    dbt.recheck();
    assert_eq!(g.run(&mut dbt), 2);
    assert_eq!(dbt.stats().blocks, blocks);
}

#[test]
fn pooled_instances_are_checked_before_use() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    g.code(CODE, &[movz_x0(3), SVC]);
    assert_eq!(g.run(&mut dbt), 3);
    // The instance goes back to the pool; when the next thread takes it, this holds different
    // code
    drop(dbt);
    g.code(CODE, &[movz_x0(4), SVC]);
    let mut dbt = Dbt::new().unwrap();
    assert_eq!(g.run(&mut dbt), 4);
}

#[test]
fn pooled_instances_keep_translations_of_the_same_program() {
    // No other test uses this key; a pooled instance with the same key is only given back to it
    const KEY: u64 = 0x5eed_0001;
    let Some(space) = DbtSpace::new(KEY, DbtRegs::Default) else {
        return;
    };
    let mut g = Guest::new();
    g.code(CODE, &[movz_x0(7), SVC]);
    let mut dbt = Dbt::attach(&space, 0).unwrap();
    assert_eq!(g.run(&mut dbt), 7);
    drop((dbt, space));
    // The same program runs again: it takes back the translations left behind and, after the
    // check, uses them directly without translating again
    let space = DbtSpace::new(KEY, DbtRegs::Default).unwrap();
    let mut dbt = Dbt::attach(&space, 0).unwrap();
    assert_eq!(g.run(&mut dbt), 7);
    assert_eq!(dbt.stats().blocks, 0);
    drop((dbt, space));
    // The code changed in the meantime: invalidated by the check and translated again
    g.code(CODE, &[movz_x0(8), SVC]);
    let space = DbtSpace::new(KEY, DbtRegs::Default).unwrap();
    let mut dbt = Dbt::attach(&space, 0).unwrap();
    assert_eq!(g.run(&mut dbt), 8);
    assert_eq!(dbt.stats().blocks, 1);
    drop((dbt, space));
    // Dropping the translations kept in the pool when host memory is tight: running again has to
    // translate again (or copy templates)
    tuxport_engine::trim_dbt_pool();
    let space = DbtSpace::new(KEY, DbtRegs::Default).unwrap();
    let mut dbt = Dbt::attach(&space, 0).unwrap();
    assert_eq!(g.run(&mut dbt), 8);
    assert_eq!(dbt.stats().blocks, 1);
}

#[test]
fn fpcr_reads_use_the_guest_state_inside_translated_code() {
    let mut g = Guest::new();
    g.code(CODE, &[0xd53b_4410, 0xd53b_4415, 0xd53b_441f, SVC]); // mrs x16/x21/xzr, fpcr
    for regs in [DbtRegs::Default, DbtRegs::Go] {
        let Some(mut dbt) = Dbt::with_regs(regs) else {
            return;
        };
        // Signal and ptrace restores can supply bits the host FPCR does not retain.
        let fpcr = 0x0040_8000;
        let s = g.cpu.state_mut();
        s.fpcr = fpcr;
        s.x[16] = 0;
        s.x[21] = 0;
        g.run(&mut dbt);
        assert_eq!((g.cpu.state().x[16], g.cpu.state().x[21]), (fpcr, fpcr));
        assert_eq!(dbt.stats().exits_interp, 0);
    }
}

#[test]
fn unchanged_fpcr_writes_stay_in_the_block() {
    let mut g = Guest::new();
    let fz16 = if tuxport_engine::guest_hwcap() & tuxport_engine::hwcap::FPHP != 0 {
        1 << 19
    } else {
        0
    };
    let mode = (1 << 22) | fz16;
    g.code(
        CODE,
        &[
            0xd51b_4410, // msr fpcr, x16
            0xd51b_4415, // msr fpcr, x21
            0xd53b_4400, // mrs x0, fpcr
            0xd51b_441f, // msr fpcr, xzr
            SVC,
        ],
    );
    for regs in [DbtRegs::Default, DbtRegs::Go] {
        let Some(mut dbt) = Dbt::with_regs(regs) else {
            return;
        };
        for (initial, exits) in [(mode, 1), (mode | 1, 2)] {
            let s = g.cpu.state_mut();
            s.fpcr = initial;
            s.nzcv = 0xa000_0000;
            s.x[16] = mode | 1 | (1 << 19);
            s.x[21] = mode;
            let before = dbt.stats().exits_interp;
            assert_eq!(g.run(&mut dbt), mode);
            assert_eq!(g.cpu.state().fpcr, 0);
            assert_eq!(g.cpu.state().nzcv, 0xa000_0000);
            assert_eq!(dbt.stats().exits_interp - before, exits);
        }
    }
}

#[test]
fn fpsr_reads_observe_exceptions_raised_inside_translated_code() {
    const IOC: u64 = 1 << 0; // Invalid operation
    const IXC: u64 = 1 << 4; // Inexact result
    for regs in [DbtRegs::Default, DbtRegs::Go] {
        let Some(mut dbt) = Dbt::with_regs(regs) else {
            return;
        };
        let mut g = Guest::new();
        g.cpu.state_mut().fpsr = IXC;
        g.code(
            CODE,
            &[
                0x1e60_1800, // fdiv d0, d0, d0: 0 / 0 sets IOC
                0xd53b_4430, // mrs x16, fpsr (stolen in the default set)
                0xd53b_4435, // mrs x21, fpsr (stolen in the Go set)
                0xd53b_443f, // mrs xzr, fpsr
                SVC,
            ],
        );
        g.run(&mut dbt);
        let s = g.cpu.state();
        assert_eq!([s.x[16], s.x[21], s.fpsr], [IXC | IOC; 3]);
        assert_eq!(dbt.stats().exits_interp, 0);
    }
}

#[test]
fn both_register_sets_run_the_same_code() {
    if Dbt::new().is_none() {
        return;
    }
    let mut g = Guest::new();
    // x0 = x1 + … + x30, then change two registers each set takes, and copy x25 to x26 through
    // the stack
    let mut code: Vec<u32> = (1..=30).map(|n| 0x8b00_0000 | n << 16).collect(); // add x0, x0, xn
    code.extend([
        0x9100_0610, // add x16, x16, #1
        0x9100_06b5, // add x21, x21, #1
        0xf81f_0ff9, // str x25, [sp, #-16]!
        0xf841_07fa, // ldr x26, [sp], #16
        SVC,
    ]);
    g.code(CODE, &code);
    let sum: u64 = (1..=30).map(|n| n * 0x101).sum();
    // The two sets alternate: translation templates for the same pc are told apart by register
    // assignment, and an empty pooled instance regenerates its routines when the set changes
    for regs in [DbtRegs::Default, DbtRegs::Go, DbtRegs::Default, DbtRegs::Go] {
        let mut dbt = Dbt::with_regs(regs).unwrap();
        let s = g.cpu.state_mut();
        s.x[0] = 0;
        for n in 1..=30 {
            s.x[n] = n as u64 * 0x101;
        }
        assert_eq!(g.run(&mut dbt), sum, "{regs:?}");
        let s = g.cpu.state();
        assert_eq!(
            (s.x[16], s.x[21], s.x[26]),
            (16 * 0x101 + 1, 21 * 0x101 + 1, 25 * 0x101),
            "{regs:?}"
        );
        assert_eq!(s.sp, STACK);
    }
    // x16 and x26–x28, which Go programs use heavily, need no rewriting with the Go set, while
    // x21–x25 do
    let other = CODE + 0x200;
    g.code(
        other,
        &[
            0x8b10_0000, // add x0, x0, x16
            0x8b1a_0000, // add x0, x0, x26
            0x8b1b_0000, // add x0, x0, x27
            0x8b1c_0000, // add x0, x0, x28
            0x8b15_0000, // add x0, x0, x21
            SVC,
        ],
    );
    for (regs, remapped) in [(DbtRegs::Default, 4), (DbtRegs::Go, 1)] {
        let mut dbt = Dbt::with_regs(regs).unwrap();
        g.run_at(&mut dbt, other);
        let st = dbt.stats();
        assert_eq!(
            (st.insns_remapped, st.insns_copied),
            (remapped, 5 - remapped),
            "{regs:?}"
        );
    }
}

#[test]
fn branches_to_unaligned_addresses_fault() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    // Page 0 holds a block that, once invalidated, leaves a tombstone in the translation table
    // (its pc field is the unaligned 1), placed exactly where a lookup of 1 starts searching;
    // page 1 holds the three kinds of indirect branch
    let mut pages: Box<[Page]> = (0..2).map(|_| Page([0; 4096])).collect();
    let mut mmu = Mmu::new();
    let base = NonNull::new(pages.as_mut_ptr().cast::<u8>()).unwrap();
    unsafe { mmu.map(0, base, 2 * PAGE_SIZE, prot::READ | prot::EXEC) }.unwrap();
    let mut cpu = unsafe { Cpu::new(&mmu) };
    let code = |page: &mut Page, off: usize, insns: &[u32]| {
        for (i, w) in insns.iter().enumerate() {
            page.0[off + 4 * i..off + 4 * i + 4].copy_from_slice(&w.to_le_bytes());
        }
    };
    code(&mut pages[0], 0, &[movz_x0(7), SVC]);
    code(&mut pages[1], 0, &[0xd61f_0020, 0xd63f_0020, RET]); // br x1, blr x1, ret
    cpu.state_mut().pc = 0;
    assert_eq!(dbt.run(&mut cpu), Exit::Svc { imm: 0 });
    code(&mut pages[0], 0, &[movz_x0(8)]);
    dbt.invalidate(&cpu, 0, PAGE_SIZE);
    for target in [1, 2, 3, PAGE_SIZE + 1] {
        for at in [PAGE_SIZE, PAGE_SIZE + 4, PAGE_SIZE + 8] {
            let s = cpu.state_mut();
            s.x[1] = target;
            s.x[30] = target;
            s.pc = at;
            // As on Linux, the alignment fault is reported after the branch, at instruction
            // fetch, with pc being the target; previously a branch to 1 entered the jump cache's
            // empty entry (code is NULL) or the invalidated block of that tombstone
            let fault = Exit::Fault {
                addr: target,
                access: Access::Exec,
                kind: FaultKind::Alignment,
            };
            assert_eq!(dbt.run(&mut cpu), fault, "{at:#x} → {target:#x}");
            assert_eq!(cpu.state().pc, target);
        }
    }
    drop((dbt, cpu, mmu));
}

#[test]
fn members_share_file_code_and_keep_jit_code_apart() {
    // Two address spaces share one instance: the first page is a registered file (shared), and
    // the second page is each one's own JIT code. A shared block calls the second page directly:
    // after the first member links it, the second member must still reach its own code
    let Some(space) = DbtSpace::new(0, DbtRegs::Default) else {
        return;
    };
    let file = DbtSource {
        start: CODE,
        len: PAGE_SIZE,
        id: [1, 2, 0, 3, 4],
    };
    space.share(&[file]);
    let jit = CODE + PAGE_SIZE;
    let tags = [space.new_tag().unwrap(), space.new_tag().unwrap()];
    let mut members: Vec<(Guest, Dbt)> = (0..2)
        .map(|i| {
            let mut g = Guest::new();
            g.code(CODE, &[bl(CODE, jit), SVC]);
            g.code(jit, &[movz_x0(i as u16 + 1), RET]);
            (g, Dbt::attach(&space, tags[i]).unwrap())
        })
        .collect();
    for round in 0..3 {
        for (n, (g, dbt)) in members.iter_mut().enumerate() {
            assert_eq!(g.run(dbt), n as u64 + 1, "round {round}");
        }
    }
    // The second member translated only its own JIT code; the two blocks of the first page are
    // the ones the first member translated
    assert_eq!(members[1].1.stats().blocks, 1);
    // A pc beyond 48 bits never finds a tagged block: the second member branches to "the first
    // member's tag | the JIT code's address"
    let (g, dbt) = &mut members[1];
    let forged = jit | u64::from(tags[0]) << 48;
    g.code(CODE + 8, &[0xd61f_0020]); // br x1
    g.cpu.state_mut().x[1] = forged;
    g.cpu.state_mut().pc = CODE + 8;
    assert!(
        matches!(dbt.run(&mut g.cpu), Exit::Fault { addr, access: Access::Exec, .. } if addr == forged)
    );
}

#[test]
fn barriers_resume_in_rewritten_code() {
    for barrier in [0xd503_3b9f, 0xd503_3fdf] {
        // dsb ish, isb
        let Some(mut dbt) = Dbt::new() else {
            return;
        };
        let mut g = Guest::new();
        let next = CODE + 8;
        g.code(CODE, &[0xd50b_7521, barrier, movz_x0(1), SVC]); // ic ivau, x1
        assert_eq!(g.run_at(&mut dbt, next), 1);
        g.code(next, &[movz_x0(2)]);
        g.cpu.state_mut().x[1] = (0xab << 56) | CODE;
        assert_eq!(g.run(&mut dbt), 2);
        assert_eq!(g.cpu.state().ivau_page, 0);
        assert_eq!(dbt.stats().exits_interp, 0);
        assert!(dbt.stats().invalidated > 0);
    }
}

#[test]
fn ic_ivau_invalidates_page_zero() {
    let Some(mut dbt) = Dbt::new() else { return };
    let mut g = Guest::with_pages(4);
    let page = &mut g.pages[3].0;
    page[..4].copy_from_slice(&movz_x0(1).to_le_bytes());
    page[4..8].copy_from_slice(&SVC.to_le_bytes());
    unsafe {
        g.mmu
            .map(
                0,
                NonNull::new(page.as_mut_ptr()).unwrap(),
                PAGE_SIZE,
                prot::READ | prot::EXEC,
            )
            .unwrap();
    }
    g.code(CODE, &[0xd50b_7521, 0xd503_3b9f, 0xd61f_0040]); // ic ivau, x1; dsb ish; br x2
    assert_eq!(g.run_at(&mut dbt, 0), 1);
    for (tag, value) in [(0xab << 56, 2), (0, 3)] {
        g.pages[3].0[..4].copy_from_slice(&movz_x0(value).to_le_bytes());
        g.cpu.state_mut().x[1] = tag;
        assert_eq!(g.run(&mut dbt), u64::from(value));
    }
}

#[test]
fn invalidation_keeps_blocks_whose_code_did_not_change() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    let other = CODE + 0x100;
    g.code(CODE, &[movz_x0(7), SVC]);
    g.code(other, &[movz_x0(8), SVC]);
    assert_eq!(g.run(&mut dbt), 7);
    assert_eq!(g.run_at(&mut dbt, other), 8);
    // The JIT changed only one place in the same page, with a per-page ic ivau: only the changed
    // block is invalidated and translated again
    g.code(other, &[movz_x0(9), SVC]);
    let before = dbt.stats();
    dbt.invalidate(&g.cpu, CODE, PAGE_SIZE);
    assert_eq!(g.run(&mut dbt), 7);
    assert_eq!(g.run_at(&mut dbt, other), 9);
    let after = dbt.stats();
    assert_eq!(after.invalidated - before.invalidated, 1);
    assert_eq!(after.blocks - before.blocks, 1);
    // The retired record for `other` is between the new block and the live block at CODE.
    g.code(CODE, &[movz_x0(10), SVC]);
    dbt.invalidate(&g.cpu, CODE, PAGE_SIZE);
    assert_eq!(g.run(&mut dbt), 10);
    assert_eq!(g.run_at(&mut dbt, other), 9);
    assert_eq!(dbt.stats().invalidated - after.invalidated, 1);
}

#[test]
fn recheck_retires_only_blocks_that_changed() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    let second = CODE + PAGE_SIZE;
    g.code(CODE, &[b(CODE, second)]);
    g.code(second, &[movz_x0(5), SVC]);
    assert_eq!(g.run(&mut dbt), 5);
    // Change only the second page: the first page's block and its link to the second page are
    // kept, and the second page's block is invalidated and translated again
    g.code(second, &[movz_x0(6), SVC]);
    let before = dbt.stats();
    dbt.recheck();
    assert_eq!(g.run(&mut dbt), 6);
    let after = dbt.stats();
    assert_eq!(after.invalidated - before.invalidated, 1);
    assert_eq!(after.blocks - before.blocks, 1);
}

#[test]
fn parking_and_flushing_over_and_over_keeps_free_slots() {
    if Dbt::new().is_none() {
        return;
    }
    // With no empty slot in the table an insertion keeps searching forever: run it in another
    // thread, and a timeout counts as failure
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut dbt = Dbt::new().unwrap();
        let mut g = Guest::new();
        let second = CODE + PAGE_SIZE;
        g.code(CODE, &[movz_x0(1), SVC]);
        g.code(second, &[movz_x0(2), SVC]);
        let source = DbtSource {
            start: CODE,
            len: 2 * PAGE_SIZE,
            id: [1, 0, 0, 0, 0],
        };
        let host = NonNull::new(g.pages[1].0.as_mut_ptr()).unwrap();
        // The smallest table has 4096 entries: leaking one tombstone per round would fill it in a
        // few thousand rounds
        for _ in 0..5000 {
            assert_eq!(g.run(&mut dbt), 1);
            assert_eq!(g.run_at(&mut dbt, second), 2);
            // The address space changed (exec of the same program) and the second page is not
            // mapped yet: the blocks there are parked
            dbt.space().set_sources(&[source]);
            dbt.recheck();
            dbt.space().trust(&[]);
            g.mmu.unmap(second, PAGE_SIZE).unwrap();
            g.cpu.tlb_flush();
            assert_eq!(g.run(&mut dbt), 1);
            // Mapped back: restored the first time it is executed, without retranslation
            unsafe {
                g.mmu
                    .map(second, host, PAGE_SIZE, prot::READ | prot::EXEC)
                    .unwrap();
            }
            let blocks = dbt.stats().blocks;
            assert_eq!(g.run_at(&mut dbt, second), 2);
            assert_eq!(dbt.stats().blocks, blocks);
            dbt.flush();
        }
        done.send(()).unwrap();
    });
    let result = finished.recv_timeout(std::time::Duration::from_secs(60));
    assert!(
        result.is_ok(),
        "translation table full: an insertion found no empty slot"
    );
}

#[test]
fn flushing_waits_for_threads_running_and_entering_the_same_space() {
    if Dbt::new().is_none() {
        return;
    }
    // A full flush has to wait until every other thread has stopped (parked) or left translated
    // code: half the threads keep spinning in a loop and stop only when kicked; half enter and
    // leave frequently. Never getting there (hanging) counts as failure. After the flush, the
    // thread that requested it first translates a few other pieces of code, overwriting the old
    // translations at the start of the code cache: if a thread were still running old
    // translations during the flush, it would run into them and compute wrong results or crash.
    // All threads run the same code (shared translations), with the loop count in x0
    const OTHER: u64 = CODE + 0x100;
    let program = |g: &mut Guest| {
        g.code(CODE, &[SUB_X0_1, cbnz_x0(CODE + 4, CODE), movz_x0(7), SVC]);
        for i in 0..16u16 {
            let at = OTHER + 0x10 * u64::from(i);
            g.code(at, &[movz_x0(100 + i), ADD_X1_1, SVC]);
        }
    };
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut dbt = Dbt::new().unwrap();
        let space = dbt.space().clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let workers: Vec<_> = (0..4)
            .map(|i| {
                let (space, stop) = (space.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut dbt = Dbt::attach(&space, 0).unwrap();
                    let mut g = Guest::new();
                    program(&mut g);
                    let mut runs = 0u64;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        g.cpu.state_mut().pc = CODE;
                        g.cpu.state_mut().x[0] = if i % 2 == 0 { 200_000 } else { 1 };
                        run_to_svc(&mut dbt, &mut g.cpu);
                        assert_eq!(g.cpu.state().x[0], 7);
                        runs += 1;
                    }
                    (runs, dbt.stats().flushes)
                })
            })
            .collect();
        let mut g = Guest::new();
        program(&mut g);
        for _ in 0..200 {
            dbt.flush();
            for i in 0..16u16 {
                g.cpu.state_mut().pc = OTHER + 0x10 * u64::from(i);
                run_to_svc(&mut dbt, &mut g.cpu);
                assert_eq!(g.cpu.state().x[0], u64::from(100 + i));
            }
            g.cpu.state_mut().pc = CODE;
            g.cpu.state_mut().x[0] = 1;
            run_to_svc(&mut dbt, &mut g.cpu);
            assert_eq!(g.cpu.state().x[0], 7);
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut flushes = dbt.stats().flushes;
        for w in workers {
            let (runs, n) = w.join().unwrap();
            assert!(runs > 0);
            flushes += n;
        }
        done.send(flushes).unwrap();
    });
    let flushes = finished
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a full flush kept waiting for the other threads to stop, or a thread failed");
    // Every flush request results in exactly one flush (done by the thread that sees it first)
    assert_eq!(flushes, 200);
}

fn cbnz_x0(from: u64, to: u64) -> u32 {
    0xb500_0000 | ((((to as i64 - from as i64) / 4) as u32 & 0x7_ffff) << 5)
}

/// Runs to an svc, continuing when interrupted (a full flush needs the other threads to stop).
fn run_to_svc(dbt: &mut Dbt, cpu: &mut Cpu) {
    loop {
        match dbt.run(cpu) {
            Exit::Svc { imm: 0 } => return,
            Exit::Kick => {}
            e => panic!("unexpected exit {e:?}"),
        }
    }
}

#[test]
fn returns_follow_x30_even_when_the_prediction_is_wrong() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    let f = CODE + 0x100;
    let other = CODE + 0x200;
    g.code(CODE, &[bl(CODE, f), movz_x0(7), SVC]);
    // An ordinary call and return: returns to just after the call
    g.code(f, &[RET]);
    assert_eq!(g.run(&mut dbt), 7);
    assert_eq!(g.run(&mut dbt), 7);
    // The callee changed x30 (as longjmp does): the shadow stack top does not match, so return by
    // x30
    g.code(f, &[adr_x30(f, other), RET]);
    g.code(other, &[movz_x0(9), SVC]);
    dbt.invalidate(&g.cpu, f, 4);
    assert_eq!(g.run(&mut dbt), 9);
}

fn adr_x30(from: u64, to: u64) -> u32 {
    let off = (to as i64 - from as i64) as u32;
    0x1000_001e | (off & 3) << 29 | ((off >> 2) & 0x7_ffff) << 5
}

#[test]
fn returning_to_the_empty_entry_marker_is_still_checked() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    // An empty shadow-stack entry records guest address 1: when x30 equals it, return still goes
    // by x30, giving a pc alignment fault
    g.code(CODE, &[0xd280_003e, RET]); // movz x30, #1; ret
    g.cpu.state_mut().pc = CODE;
    assert!(matches!(
        dbt.run(&mut g.cpu),
        Exit::Fault {
            addr: 1,
            kind: FaultKind::Alignment,
            ..
        }
    ));
}

#[test]
fn deep_recursion_wraps_the_shadow_stack() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    let f = CODE + 0x100;
    // main: x0 = 200; bl f; svc. f recurses x0 levels deep, and x1 is incremented on each return:
    // past the shadow stack's 64 entries the bottom entries are overwritten and deeper returns
    // can only be looked up by x30, with the same result
    g.code(CODE, &[movz_x0(200), bl(CODE + 4, f), SVC]);
    let done = f + 6 * 4;
    g.code(
        f,
        &[
            cbz_x0(f, done),
            STP_FP_LR_PRE,
            SUB_X0_1,
            bl(f + 12, f),
            LDP_FP_LR_POST,
            ADD_X1_1,
            RET,
        ],
    );
    for _ in 0..2 {
        g.cpu.state_mut().x[1] = 0;
        assert_eq!(g.run(&mut dbt), 0);
        assert_eq!(g.cpu.state().x[1], 200);
        assert_eq!(g.cpu.state().sp, STACK);
    }
}

#[test]
fn cross_page_accesses_stay_in_translated_code() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::with_data();
    g.code(CODE, &[LDR_X0_X2, STR_X3_X4, LDP_X5_X6_X7, SVC]);
    // Each new top byte refills both pages. Repeating it exercises the cached pair.
    for (round, top_byte) in [0, 0, 0xab, 0xab, 0xff].into_iter().enumerate() {
        let tag = top_byte << 56;
        let value = 0x1122_3344_5566_7788 + round as u64;
        let s = g.cpu.state_mut();
        s.x[2] = tag | before_page(1, 4);
        s.x[3] = value;
        s.x[4] = tag | before_page(1, 2);
        s.x[7] = tag | before_page(1, 12);
        let expect = crossing(&g, 1, 4);
        g.run(&mut dbt);
        let s = g.cpu.state();
        assert_eq!(s.x[0], expect);
        assert_eq!(g.data(0, 4096 - 8) >> 48, value & 0xffff);
        assert_eq!(g.data(1, 0) & 0xffff_ffff_ffff, value >> 16);
        assert_eq!(s.x[6], crossing(&g, 1, 4));
        assert_eq!(s.x[5], g.data(0, 4096 - 12));
    }
    assert_eq!(dbt.stats().exits_interp, 0);
}

#[test]
fn tagged_group_writeback_and_exclusive_aliases() {
    let Some(mut dbt) = Dbt::new() else { return };
    let mut g = Guest::with_data();
    let f = CODE + 0x900;
    g.code(
        f,
        &[
            LDR_X0_X2,
            0xf800_8443, // str x3, [x2], #8
            0xc85f_7c85, // ldxr x5, [x4]
            0xc806_7ce3, // stxr w6, x3, [x7]
            SVC,
        ],
    );
    let s = g.cpu.state_mut();
    s.x[2] = (0xab << 56) | DATA;
    s.x[3] = 12345;
    s.x[4] = (0xcd << 56) | (DATA + 8);
    s.x[7] = DATA + 8;
    let pair = (g.data(0, 0), g.data(0, 8));
    g.run_at(&mut dbt, f);
    let s = g.cpu.state();
    assert_eq!((s.x[0], s.x[5]), pair);
    assert_eq!(s.x[2], (0xab << 56) | (DATA + 8));
    assert_eq!(s.x[6], 0);
    assert_eq!((g.data(0, 0), g.data(0, 8)), (12345, 12345));
    assert!(dbt.stats().insns_grouped > 0);
    assert_eq!(dbt.stats().exits_interp, 0);
}

#[test]
fn a_write_only_mapping_cannot_authorize_a_grouped_read() {
    let Some(mut dbt) = Dbt::new() else { return };
    let mut g = Guest::with_data();
    // The raw MMU can represent write-only pages even though Linux adds read permission.
    g.mmu.protect(DATA, PAGE_SIZE, prot::WRITE).unwrap();
    let f = CODE + 0xb00;
    g.code(CODE, &[STR_X3_X4, SVC]);
    g.code(f, &[LDR_X0_X2, 0xf900_0443, SVC]); // ldr x0, [x2]; str x3, [x2, #8]
    for tag in [0xab << 56, 0] {
        let s = g.cpu.state_mut();
        s.x[2] = tag | DATA;
        s.x[4] = s.x[2];
        s.x[3] = 99;
        g.run(&mut dbt);
        assert_eq!(g.data(0, 0), 99);
        let before = g.data(0, 8);
        assert_eq!(
            go(&mut g, &mut dbt, f),
            Exit::Fault {
                addr: DATA,
                access: Access::Read,
                kind: FaultKind::Page
            }
        );
        assert_eq!(g.cpu.state().pc, f);
        assert_eq!(g.data(0, 8), before);
    }
}

#[test]
fn cross_page_accesses_to_scattered_host_pages_use_tier0() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::with_data();
    g.code(CODE, &[LDR_X0_X2, SVC]);
    g.cpu.state_mut().x[2] = before_page(3, 4);
    // On the host, page 2 is followed by the page before page 3, so each page must be accessed
    // through the page table separately
    assert_eq!(g.run(&mut dbt), crossing(&g, 3, 4));
    assert_eq!(dbt.stats().exits_interp, 1);
}

#[test]
fn cross_page_faults_are_precise() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::with_data();
    // A write crossing into a read-only page, a read crossing into an unmapped page: reported at
    // the second page, and the first page must not be written
    let cases = [
        (STR_X3_X4, 4, before_page(5, 4), DATA + 5 * PAGE_SIZE),
        (LDR_X0_X2, 2, before_page(6, 4), DATA + 6 * PAGE_SIZE),
        (LDR_X0_X2, 2, u64::MAX - 3, 0),
        (LDR_X0_X2, 2, (0xab01u64 << 48) | DATA, (1 << 48) | DATA),
        (
            STR_X3_X4,
            4,
            (0xab << 56) | before_page(5, 4),
            DATA + 5 * PAGE_SIZE,
        ),
    ];
    for (insn, reg, addr, fault) in cases {
        g.code(CODE, &[insn, SVC]);
        dbt.invalidate(&g.cpu, CODE, 8);
        let before = g.data(4, 4096 - 8);
        g.cpu.state_mut().x[reg] = addr;
        g.cpu.state_mut().pc = CODE;
        match dbt.run(&mut g.cpu) {
            Exit::Fault { addr, .. } if fault != 0 => assert_eq!(addr, fault),
            Exit::Fault { .. } => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(g.cpu.state().pc, CODE);
        assert_eq!(g.data(4, 4096 - 8), before);
    }
}

const LDR_X1_X2_8: u32 = 0xf940_0441; // ldr x1, [x2, #8]

// The following tests check whether accesses are grouped. An instance may come from the pool,
// carrying "no more grouping here" marks (by pc) recorded by other tests, so each test puts its
// code at its own address.

#[test]
fn exits_beyond_48_bits_keep_the_whole_pc() {
    // An exit stub keeps the guest pc and the exit reason in one data word (low 48 bits, high 16
    // bits); when the computed branch target is beyond 48 bits a different stub is used. Code in
    // the last page of the address space runs to the end of the page and falls to 2^48: the
    // instruction fetch fault reports the full address, which must not be truncated to 0
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::with_pages(4);
    const LAST: u64 = (1 << 48) - PAGE_SIZE;
    let page = &mut g.pages[3].0;
    page[..4].copy_from_slice(&b(LAST, LAST + 4092).to_le_bytes());
    page[4092..].copy_from_slice(&movz_x0(5).to_le_bytes());
    let host = NonNull::new(g.pages[3].0.as_mut_ptr()).unwrap();
    unsafe {
        g.mmu
            .map(LAST, host, PAGE_SIZE, prot::READ | prot::EXEC)
            .unwrap()
    };
    g.cpu.state_mut().pc = LAST;
    match dbt.run(&mut g.cpu) {
        Exit::Fault { addr, .. } => assert_eq!(addr, 1 << 48),
        other => panic!("{other:?}"),
    }
    assert_eq!(g.cpu.state().pc, 1 << 48);
    assert_eq!(g.cpu.state().x[0], 5);
}

#[test]
fn accesses_with_the_same_base_share_one_check() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    // A function prologue and epilogue around instructions that rewrite stolen registers: 6
    // memory accesses in all, checked only once
    let f = CODE + 0x100;
    g.code(
        f,
        &[
            0xa9bd_7bfd, // stp x29, x30, [sp, #-48]!
            0xa901_47f0, // stp x16, x17, [sp, #16]
            0xf900_17fb, // str x27, [sp, #40]
            0xd280_0010, // movz x16, #0
            0xd280_0011, // movz x17, #0
            0xd280_001b, // movz x27, #0
            0xa941_47f0, // ldp x16, x17, [sp, #16]
            0xf940_17fb, // ldr x27, [sp, #40]
            0xa8c3_7bfd, // ldp x29, x30, [sp], #48
            SVC,
        ],
    );
    let regs = [16, 17, 27, 29, 30];
    for (i, &r) in regs.iter().enumerate() {
        g.cpu.state_mut().x[r] = 0x1111 * (i as u64 + 1);
    }
    g.run_at(&mut dbt, f);
    for (i, &r) in regs.iter().enumerate() {
        assert_eq!(g.cpu.state().x[r], 0x1111 * (i as u64 + 1));
    }
    assert_eq!(g.cpu.state().sp, STACK);
    let s = dbt.stats();
    assert_eq!(s.insns_grouped, 5);
    assert_eq!(s.exits_interp, 0);
}

#[test]
fn shared_checks_across_scattered_host_pages_are_undone() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::with_data();
    let f = CODE + 0x200;
    g.code(f, &[LDR_X0_X2, LDR_X1_X2_8, SVC]);
    // Each of the two accesses stays within a page, but together they cross into the next page,
    // which is not adjacent on the host: falls back to Tier 0, and from the second time on this
    // block is no longer grouped
    for (ungrouped, interp) in [(0, 1), (1, 2), (1, 2)] {
        g.cpu.state_mut().x[2] = before_page(3, 8);
        g.run_at(&mut dbt, f);
        assert_eq!(g.cpu.state().x[0], g.data(2, 4096 - 8));
        assert_eq!(g.cpu.state().x[1], g.data(3, 0));
        let s = dbt.stats();
        assert_eq!((s.ungrouped, s.exits_interp), (ungrouped, interp));
    }
    // The same code elsewhere, accessing two pages that are adjacent on the host, still shares
    // one check
    let f = CODE + 0x300;
    g.code(f, &[LDR_X0_X2, LDR_X1_X2_8, SVC]);
    g.cpu.state_mut().x[2] = before_page(1, 8);
    g.run_at(&mut dbt, f);
    assert_eq!(g.cpu.state().x[1], g.data(1, 0));
    let s = dbt.stats();
    assert_eq!((s.ungrouped, s.insns_grouped), (1, 2));
}

#[test]
fn shared_checks_cover_accesses_behind_branches() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::with_data();
    // The second load comes after a conditional branch, and the next page it accesses is not
    // mapped; when x0 is 0 it does not execute
    let f = CODE + 0x400;
    g.code(f, &[LDR_X0_X2, cbz_x0(f + 4, f + 12), LDR_X1_X2_8, SVC]);
    g.pages[8].0[4096 - 8..].fill(0);
    g.cpu.state_mut().x[1] = 7;
    for ungrouped in [0, 1, 1] {
        g.cpu.state_mut().x[2] = before_page(6, 8);
        g.run_at(&mut dbt, f);
        assert_eq!(g.cpu.state().x[0], 0);
        assert_eq!(g.cpu.state().x[1], 7);
        assert_eq!(dbt.stats().ungrouped, ungrouped);
    }
}

#[test]
fn watchpoints_send_only_watched_pages_to_tier0() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::with_data();
    let f = CODE + 0x500;
    g.code(f, &[LDR_X0_X2, STR_X3_X4, SVC]);
    let page = DATA + 4 * PAGE_SIZE;
    let watched = page + 0x100;
    g.cpu
        .state_mut()
        .set_hw_debug(&[], &[(watched, watched + 8, WATCH_STORE)]);
    let interp = |dbt: &Dbt| dbt.stats().exits_interp;
    // Read the write-watched page and write another page: the first time misses the TLB and is
    // executed by Tier 0, which fills the table; after that everything stays in translated code
    let s = g.cpu.state_mut();
    (s.x[2], s.x[3], s.x[4]) = (page + 0x108, 5, DATA + 0x10);
    for _ in 0..2 {
        assert_eq!(go(&mut g, &mut dbt, f), Exit::Svc { imm: 0 });
    }
    let before = interp(&dbt);
    assert_eq!(go(&mut g, &mut dbt, f), Exit::Svc { imm: 0 });
    assert_eq!(interp(&dbt), before);
    assert_eq!(g.cpu.state().x[0], g.data(4, 0x108));
    // Writing bytes of the watched page that are not watched: executed by Tier 0 every time
    g.cpu.state_mut().x[4] = page + 0x108;
    for i in 1..=2 {
        assert_eq!(go(&mut g, &mut dbt, f), Exit::Svc { imm: 0 });
        assert_eq!(interp(&dbt), before + i);
    }
    assert_eq!(g.data(4, 0x108), 5);
    // Writing the watched bytes: stops at this instruction without writing; the preceding read
    // has already executed
    let old = g.data(4, 0x100);
    let s = g.cpu.state_mut();
    (s.x[0], s.x[4]) = (0, (0xab << 56) | (watched + 4));
    let hit = Exit::Fault {
        addr: watched + 4,
        access: Access::Write,
        kind: FaultKind::Watchpoint,
    };
    assert_eq!(go(&mut g, &mut dbt, f), hit);
    assert_eq!(g.cpu.state().pc, f + 4);
    assert_eq!(g.cpu.state().x[0], g.data(4, 0x108));
    assert_eq!(g.data(4, 0x100), old);
    // Read watchpoint: the read half does not enter the TLB either
    g.cpu
        .state_mut()
        .set_hw_debug(&[], &[(watched, watched + 8, WATCH_LOAD)]);
    let s = g.cpu.state_mut();
    (s.x[2], s.x[4]) = (watched, DATA + 0x10);
    let hit = Exit::Fault {
        addr: watched,
        access: Access::Read,
        kind: FaultKind::Watchpoint,
    };
    assert_eq!(go(&mut g, &mut dbt, f), hit);
    assert_eq!(g.cpu.state().pc, f);
    // After removing it, the whole page is in translated code again
    g.cpu.state_mut().set_hw_debug(&[], &[]);
    g.cpu.state_mut().x[4] = page + 0x100;
    assert_eq!(go(&mut g, &mut dbt, f), Exit::Svc { imm: 0 });
    let before = interp(&dbt);
    assert_eq!(go(&mut g, &mut dbt, f), Exit::Svc { imm: 0 });
    assert_eq!(interp(&dbt), before);
    assert_eq!(g.data(4, 0x100), 5);
}

#[test]
fn watchpoints_stop_inside_shared_checks_and_atomics() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::with_data();
    // Several stack writes in a function prologue sharing one check: stops at the instruction
    // writing the watched slot, with the earlier ones already executed
    let f = CODE + 0x600;
    g.code(
        f,
        &[
            0xa9bd_7bfd, // stp x29, x30, [sp, #-48]!
            0xa901_47f0, // stp x16, x17, [sp, #16]
            0xf900_17fb, // str x27, [sp, #40]
            0x9100_c3ff, // add sp, sp, #48
            SVC,
        ],
    );
    let slot = STACK - 8;
    g.cpu
        .state_mut()
        .set_hw_debug(&[], &[(slot, slot + 8, WATCH_STORE)]);
    g.cpu.state_mut().x[29] = 0x2929;
    let hit = Exit::Fault {
        addr: slot,
        access: Access::Write,
        kind: FaultKind::Watchpoint,
    };
    assert_eq!(go(&mut g, &mut dbt, f), hit);
    assert_eq!(g.cpu.state().pc, f + 8);
    assert_eq!(g.cpu.state().sp, STACK - 48);
    let stack = &g.pages[2].0;
    assert_eq!(
        u64::from_le_bytes(stack[4096 - 48..4096 - 40].try_into().unwrap()),
        0x2929
    );
    // Not a case of the group being unable to share a check: "no more grouping here" is not
    // recorded
    assert_eq!(dbt.stats().ungrouped, 0);
    g.cpu.state_mut().set_hw_debug(&[], &[]);
    assert_eq!(dbt.run(&mut g.cpu), Exit::Svc { imm: 0 });
    assert_eq!(g.cpu.state().sp, STACK);

    // An atomic read-modify-write counts as a read: a read watchpoint must stop it too
    let f = CODE + 0x700;
    g.code(f, &[0xf825_00e6, SVC]); // ldadd x5, x6, [x7]
    let at = DATA + 4 * PAGE_SIZE + 0x200;
    g.cpu.state_mut().x[7] = at;
    g.cpu.state_mut().x[5] = 1;
    assert_eq!(go(&mut g, &mut dbt, f), Exit::Svc { imm: 0 });
    let after = g.data(4, 0x200);
    g.cpu
        .state_mut()
        .set_hw_debug(&[], &[(at, at + 8, WATCH_LOAD)]);
    let exit = go(&mut g, &mut dbt, f);
    assert!(
        matches!(exit, Exit::Fault { kind: FaultKind::Watchpoint, addr, .. } if addr == at),
        "{exit:?}"
    );
    assert_eq!(g.cpu.state().pc, f);
    assert_eq!(g.data(4, 0x200), after);
}

fn bl(from: u64, to: u64) -> u32 {
    0x9400_0000 | (((to as i64 - from as i64) / 4) as u32 & 0x3ff_ffff)
}

fn cbz_x0(from: u64, to: u64) -> u32 {
    0xb400_0000 | ((((to as i64 - from as i64) / 4) as u32 & 0x7_ffff) << 5)
}

fn movz_x0(imm: u16) -> u32 {
    0xd280_0000 | (imm as u32) << 5
}

fn b(from: u64, to: u64) -> u32 {
    0x1400_0000 | (((to as i64 - from as i64) / 4) as u32 & 0x3ff_ffff)
}

/// The address of the first byte of data page `page` minus `before`: an access starting here
/// crosses into page `page`.
fn before_page(page: u64, before: u64) -> u64 {
    DATA + page * PAGE_SIZE - before
}

/// The expected value of a cross-page access: the last `before` bytes of page `page - 1` followed
/// by the first bytes of page `page`.
fn crossing(g: &Guest, page: usize, before: usize) -> u64 {
    let lo = g.data(page - 1, 4096 - 8) >> (8 * (8 - before));
    let hi = g.data(page, 0) << (8 * before);
    lo | hi
}

/// Runs from `pc` to an svc or a fault.
fn go(g: &mut Guest, dbt: &mut Dbt, pc: u64) -> Exit {
    g.cpu.state_mut().pc = pc;
    dbt.run(&mut g.cpu)
}
