//! The host-side fault guard (`install_fault_guard`, the engine map, design doc §12 item 5): when
//! the engine accesses host memory it should not, both execution tiers return `Exit::HostFault`
//! and then keep running as usual. A page-table entry pointing at a `PROT_NONE` host page
//! simulates the engine bug (the kernel layer never builds page tables like that). When the host
//! is not aarch64 Linux (Android included) there are no native stubs and no Tier 1, and the whole
//! file is not compiled.
#![cfg(all(
    any(target_os = "linux", target_os = "android"),
    target_arch = "aarch64"
))]

use std::ptr::NonNull;

use tuxport_engine::{Cpu, Dbt, Exit, Mmu, PAGE_SIZE, hwcap, prot};

#[repr(C, align(4096))]
struct Page([u8; 4096]);

const CODE: u64 = 0x40_0000;
/// A page that can be read and written normally.
const DATA: u64 = 0x50_0000;
/// A page the page table says is read-write, but which is `PROT_NONE` on the host side.
const BROKEN: u64 = 0x60_0000;

const SVC: u32 = 0xd400_0001;
const LDR_X0_X1: u32 = 0xf940_0020; // ldr x0, [x1]
const LDADD_X2_X3_X1: u32 = 0xf822_0023; // ldadd x2, x3, [x1]
const LDADD_X4_X5_X6: u32 = 0xf824_00c5; // ldadd x4, x5, [x6]

const SIGSEGV: i32 = 11;

struct Guest {
    // Drop order: cpu → mmu → pages, broken
    cpu: Cpu,
    mmu: Mmu,
    pages: Box<[Page]>,
    broken: NonNull<u8>,
}

impl Guest {
    fn new() -> Self {
        tuxport_engine::install_fault_guard();
        let mut pages: Box<[Page]> = (0..2).map(|_| Page([0; 4096])).collect();
        let broken = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(broken, libc::MAP_FAILED);
        let broken = NonNull::new(broken.cast::<u8>()).unwrap();
        let mut mmu = Mmu::new();
        let base = NonNull::new(pages.as_mut_ptr().cast::<u8>()).unwrap();
        unsafe {
            mmu.map(CODE, base, PAGE_SIZE, prot::READ | prot::EXEC)
                .unwrap();
            mmu.map(DATA, base.add(4096), PAGE_SIZE, prot::READ | prot::WRITE)
                .unwrap();
            mmu.map(BROKEN, broken, PAGE_SIZE, prot::READ | prot::WRITE)
                .unwrap();
        }
        let cpu = unsafe { Cpu::new(&mmu) };
        Guest {
            cpu,
            mmu,
            pages,
            broken,
        }
    }

    fn code(&mut self, insns: &[u32]) {
        for (i, insn) in insns.iter().enumerate() {
            self.pages[0].0[4 * i..4 * i + 4].copy_from_slice(&insn.to_le_bytes());
        }
        let _ = &self.mmu;
        self.cpu.state_mut().pc = CODE;
        self.cpu.tlb_flush();
    }

    fn assert_host_fault(&self, exit: Exit) {
        match exit {
            Exit::HostFault { signal, addr, .. } => {
                assert_eq!(signal, SIGSEGV);
                assert_eq!(addr, self.broken.as_ptr() as u64);
            }
            other => panic!("expected a host fault, got {other:?}"),
        }
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.broken.as_ptr().cast(), 4096) };
    }
}

#[test]
fn interpreter_survives_a_host_fault() {
    let mut g = Guest::new();
    g.code(&[LDR_X0_X1, SVC]);
    g.cpu.state_mut().x[1] = BROKEN;
    let exit = g.cpu.run();
    g.assert_host_fault(exit);
    // Then it keeps running as usual
    g.code(&[LDR_X0_X1, SVC]);
    g.pages[1].0[..8].copy_from_slice(&42u64.to_le_bytes());
    g.cpu.state_mut().x[1] = DATA;
    assert_eq!(g.cpu.run(), Exit::Svc { imm: 0 });
    assert_eq!(g.cpu.state().x[0], 42);
}

#[test]
fn translator_survives_a_host_fault() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    g.code(&[LDR_X0_X1, SVC]);
    g.cpu.state_mut().x[1] = BROKEN;
    let exit = dbt.run(&mut g.cpu);
    g.assert_host_fault(exit);
    // An instance that hit a fault is released directly and does not go back to the pool; a new
    // instance runs the same code as usual
    drop(dbt);
    let mut dbt = Dbt::new().unwrap();
    g.code(&[LDR_X0_X1, SVC]);
    g.pages[1].0[..8].copy_from_slice(&7u64.to_le_bytes());
    g.cpu.state_mut().x[1] = DATA;
    assert_eq!(dbt.run(&mut g.cpu), Exit::Svc { imm: 0 });
    assert_eq!(g.cpu.state().x[0], 7);
}

#[test]
fn locks_held_at_the_fault_are_released() {
    if tuxport_engine::guest_hwcap() & hwcap::ATOMICS == 0 {
        return;
    }
    let mut g = Guest::new();
    // Tier 0 runs native stubs under the read lock: when a stub's access faults this lock must be
    // released, otherwise generating a new stub below needs the write lock and the same thread
    // would hang
    g.code(&[LDADD_X2_X3_X1, SVC]);
    g.cpu.state_mut().x[1] = BROKEN;
    let exit = g.cpu.run();
    g.assert_host_fault(exit);
    g.code(&[LDADD_X4_X5_X6, SVC]);
    g.cpu.state_mut().x[4] = 5;
    g.cpu.state_mut().x[6] = DATA;
    assert_eq!(g.cpu.run(), Exit::Svc { imm: 0 });
    assert_eq!(&g.pages[1].0[..8], &5u64.to_le_bytes());
}
