//! Rust bindings for the Tuxport engine (implemented in C).
//!
//! The engine executes guest instructions and the soft-MMU fast path (the engine and kernel/mmu
//! maps). The kernel layer drives execution with [`Cpu::run`] and then handles syscalls,
//! exceptions and other events according to the returned [`Exit`].

use std::ffi::{c_int, c_uint, c_void};
use std::fmt;
use std::mem::offset_of;
use std::ptr::NonNull;
use std::sync::Arc;

/// Guest page size, fixed at 4K (the kernel/mm map).
pub const PAGE_SIZE: u64 = 4096;
/// Number of guest virtual address bits.
pub const VA_BITS: u32 = 48;
/// Number of software data TLB entries per CPU.
pub const TLB_ENTRIES: usize = 4096;
/// Number of software instruction-fetch TLB entries per CPU.
pub const ITLB_ENTRIES: usize = 512;

/// Page permissions, with the same values as Linux's `PROT_*`.
pub mod prot {
    /// Readable.
    pub const READ: u32 = 1;
    /// Writable.
    pub const WRITE: u32 = 2;
    /// Executable.
    pub const EXEC: u32 = 4;
}

#[repr(C)]
struct RawMmu {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TlbEntry {
    tag_read: u64,
    addend_read: u64,
    tag_write: u64,
    addend_write: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ItlbEntry {
    tag: u64,
    addend: u64,
}

/// Maximum number of hardware breakpoints, and of watchpoints (C's `TUXPORT_HW_SLOTS`).
pub const HW_SLOTS: usize = 16;
/// Watchpoint kinds: read and write (can be combined).
pub const WATCH_LOAD: u32 = 1;
pub const WATCH_STORE: u32 = 2;

/// C's `struct tuxport_watch`.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq)]
struct WatchSlot {
    lo: u64,
    hi: u64,
    ty: u32,
    reserved: u32,
}

/// Guest CPU state, mirroring C's `struct tuxport_cpu` field by field.
#[repr(C)]
pub struct CpuState {
    /// General registers X0–X30.
    pub x: [u64; 31],
    /// Stack pointer.
    pub sp: u64,
    /// Program counter.
    pub pc: u64,
    /// Condition flags; only bits 31..28 are used.
    pub nzcv: u64,
    /// Floating-point control register.
    pub fpcr: u64,
    /// Floating-point status register.
    pub fpsr: u64,
    /// User-mode thread pointer.
    pub tpidr_el0: u64,
    /// The original X0 restored when a syscall restarts, maintained by the kernel layer.
    pub orig_x0: u64,
    fault_addr: u64,
    fault_access: u32,
    fault_kind: u32,
    exit_info: u32,
    dbt_lookup_seq: u32,
    pending: u64,
    mmu: *mut RawMmu,
    excl_addr: u64,
    excl_value: [u64; 2],
    excl_size: u32,
    dbt_tag: u32,
    dbt_reason: u64,
    dbt_site: u64,
    dbt_jump_cache: *mut c_void,
    dbt_rs: *mut c_void,
    /// The page recorded by `ic ivau` in Tier 1 whose translations are not invalidated yet (0
    /// means none). [`Dbt::run`] invalidates it in place at the next dsb or isb; when an exit
    /// leaves it set, the caller collects and invalidates it.
    pub ivau_page: u64,
    dbt_groups: [u64; 4],
    tlb: [TlbEntry; TLB_ENTRIES],
    itlb: [ItlbEntry; ITLB_ENTRIES],
    /// V0–V31, the low 64 bits of each register first.
    pub v: [[u64; 2]; 32],
    tlb_used: [u64; TLB_ENTRIES / 64],
    nbrk: u32,
    nwatch: u32,
    /// PSTATE.DIT (0 or 1); bit 24 of pstate in signal frames and ptrace.
    pub dit: u32,
    reserved: u32,
    brk: [u64; HW_SLOTS],
    watch: [WatchSlot; HW_SLOTS],
}

impl CpuState {
    /// Sets hardware breakpoints (instruction addresses) and watchpoints (start, end,
    /// [`WATCH_LOAD`]/[`WATCH_STORE`]), at most [`HW_SLOTS`] of each. Only the interpreter checks
    /// them ([`Cpu::run`], [`Cpu::step`]): a hit exits with [`FaultKind::HwBreakpoint`] or
    /// [`FaultKind::Watchpoint`], and the instruction has not executed. A thread with breakpoints
    /// can only run under the interpreter ([`CpuState::hw_breakpoints_active`]); with watchpoints,
    /// watched pages stay out of the TLB and Tier 1 hands an access to them to the interpreter for
    /// that one instruction, so the TLB must be flushed when the watched ranges change.
    pub fn set_hw_debug(&mut self, breakpoints: &[u64], watchpoints: &[(u64, u64, u32)]) {
        assert!(breakpoints.len() <= HW_SLOTS && watchpoints.len() <= HW_SLOTS);
        let before = (self.nwatch, self.watch);
        self.brk[..breakpoints.len()].copy_from_slice(breakpoints);
        for (slot, &(lo, hi, ty)) in self.watch.iter_mut().zip(watchpoints) {
            *slot = WatchSlot {
                lo,
                hi,
                ty,
                reserved: 0,
            };
        }
        self.nbrk = breakpoints.len() as u32;
        self.nwatch = watchpoints.len() as u32;
        let n = self.nwatch as usize;
        // The tracer sets them again every time it resumes the thread; do not flush when unchanged
        if before.0 != self.nwatch || before.1[..n] != self.watch[..n] {
            unsafe { tuxport_cpu_tlb_flush(self) }
        }
    }

    /// Hardware breakpoints are set: this thread can only run under the interpreter (Tier 1 does
    /// not compare pc before every instruction).
    pub fn hw_breakpoints_active(&self) -> bool {
        self.nbrk != 0
    }
}

unsafe extern "C" {
    fn tuxport_mmu_new() -> *mut RawMmu;
    fn tuxport_mmu_free(mmu: *mut RawMmu);
    fn tuxport_mmu_map(mmu: *mut RawMmu, gva: u64, host: *mut c_void, len: u64, prot: u32)
    -> c_int;
    fn tuxport_mmu_unmap(mmu: *mut RawMmu, gva: u64, len: u64) -> c_int;
    fn tuxport_mmu_protect(mmu: *mut RawMmu, gva: u64, len: u64, prot: u32) -> c_int;
    fn tuxport_mmu_fill(
        mmu: *mut RawMmu,
        gva: u64,
        host: *mut c_void,
        len: u64,
        prot: u32,
    ) -> c_int;
    fn tuxport_mmu_move(mmu: *mut RawMmu, from: u64, to: u64, len: u64) -> c_int;
    fn tuxport_mmu_next_present(mmu: *const RawMmu, gva: u64, end: u64) -> u64;
    fn tuxport_mmu_translate(mmu: *const RawMmu, gva: u64, required_prot: u32) -> *mut c_void;

    fn tuxport_cpu_new(mmu: *mut RawMmu) -> *mut CpuState;
    fn tuxport_cpu_free(cpu: *mut CpuState);
    fn tuxport_cpu_init(cpu: *mut CpuState, mmu: *mut RawMmu);
    fn tuxport_cpu_tlb_flush(cpu: *mut CpuState);
    fn tuxport_cpu_run(cpu: *mut CpuState) -> c_int;
    fn tuxport_cpu_exec_insn(cpu: *mut CpuState, insn: u32) -> c_int;
    fn tuxport_cpu_step(cpu: *mut CpuState) -> c_int;
    fn tuxport_cpu_kick(cpu: *mut CpuState);
    fn tuxport_cpu_layout_check(
        size: usize,
        off_x: usize,
        off_pc: usize,
        off_tpidr: usize,
        off_pending: usize,
        off_mmu: usize,
        off_jump_cache: usize,
        off_tlb: usize,
        off_v: usize,
    ) -> c_int;

    fn tuxport_fault_guard_install();
    fn tuxport_host_fault_last(out: *mut RawHostFault);

    fn tuxport_guest_hwcap() -> u64;
    fn tuxport_guest_hwcap2() -> u64;
    fn tuxport_guest_midr() -> u64;

    fn tuxport_dbt_new(key: u64, regs: DbtRegs) -> *mut RawDbt;
    fn tuxport_dbt_trim();
    fn tuxport_dbt_unmap_written();
    fn tuxport_dbt_written_bytes() -> u64;
    fn tuxport_dbt_free(dbt: *mut RawDbt, key: u64);
    fn tuxport_dbt_set_sources(dbt: *mut RawDbt, s: *const DbtSource, n: usize);
    fn tuxport_dbt_trust(dbt: *mut RawDbt, s: *const DbtSource, n: usize);
    fn tuxport_dbt_retire_outside(dbt: *mut RawDbt, s: *const DbtSource, n: usize);
    fn tuxport_dbt_share(dbt: *mut RawDbt, s: *const DbtSource, n: usize);
    fn tuxport_dbt_new_tag(dbt: *mut RawDbt) -> c_uint;
    fn tuxport_dbt_attach(dbt: *mut RawDbt, tag: c_uint) -> *mut RawDbtThread;
    fn tuxport_dbt_detach(t: *mut RawDbtThread);
    fn tuxport_dbt_flush(t: *mut RawDbtThread);
    fn tuxport_dbt_recheck(t: *mut RawDbtThread);
    fn tuxport_dbt_invalidate(t: *mut RawDbtThread, cpu: *const CpuState, start: u64, len: u64);
    fn tuxport_dbt_run(t: *mut RawDbtThread, cpu: *mut CpuState) -> c_int;
    fn tuxport_dbt_get_stats(t: *const RawDbtThread, out: *mut DbtStats);
}

#[repr(C)]
struct RawDbt {
    _private: [u8; 0],
}

#[repr(C)]
struct RawDbtThread {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Default)]
struct RawHostFault {
    signo: i32,
    code: i32,
    addr: u64,
    pc: u64,
}

// ---- Page table ----

/// Invalid argument or out of memory while modifying the page table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// Address or length not 4K-aligned, beyond the 48-bit address space, or invalid permission
    /// bits.
    InvalidArgument,
    /// Failed to allocate a page table.
    OutOfMemory,
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MapError::InvalidArgument => f.write_str("invalid mapping argument"),
            MapError::OutOfMemory => f.write_str("out of memory for page tables"),
        }
    }
}

impl std::error::Error for MapError {}

/// The software page table of one guest address space (the kernel/mmu map).
pub struct Mmu {
    raw: NonNull<RawMmu>,
}

impl Mmu {
    /// Creates an empty page table.
    pub fn new() -> Self {
        let raw = unsafe { tuxport_mmu_new() };
        Mmu {
            raw: NonNull::new(raw).expect("tuxport_mmu_new: out of memory"),
        }
    }

    /// Maps guest `[gva, gva + len)` to host `[host, host + len)`, replacing existing mappings.
    ///
    /// # Safety
    ///
    /// The `len` bytes at `host` must be readable and writable, and stay valid while the mapping
    /// exists (until unmap, until it is replaced, or until the `Mmu` is destroyed).
    pub unsafe fn map(
        &mut self,
        gva: u64,
        host: NonNull<u8>,
        len: u64,
        prot: u32,
    ) -> Result<(), MapError> {
        map_result(unsafe {
            tuxport_mmu_map(self.raw.as_ptr(), gva, host.as_ptr().cast(), len, prot)
        })
    }

    /// Unmaps `[gva, gva + len)`.
    pub fn unmap(&mut self, gva: u64, len: u64) -> Result<(), MapError> {
        map_result(unsafe { tuxport_mmu_unmap(self.raw.as_ptr(), gva, len) })
    }

    /// Changes the permissions of existing entries, possibly to 0 (the host page is kept but
    /// cannot be accessed); pages without entries are unchanged.
    pub fn protect(&mut self, gva: u64, len: u64, prot: u32) -> Result<(), MapError> {
        map_result(unsafe { tuxport_mmu_protect(self.raw.as_ptr(), gva, len, prot) })
    }

    /// Moves the existing entries in `[from, from + len)` unchanged to the same positions starting
    /// at `to`, clearing the originals; the two ranges must not overlap.
    pub fn move_entries(&mut self, from: u64, to: u64, len: u64) -> Result<(), MapError> {
        map_result(unsafe { tuxport_mmu_move(self.raw.as_ptr(), from, to, len) })
    }

    /// Same as [`Mmu::map_shared`], but pages with existing entries (including ones with
    /// permission 0) stay unchanged: on-demand filling fills pages that have no entry yet and
    /// leaves the ones copy-on-write has changed alone.
    ///
    /// # Safety
    ///
    /// Same as [`Mmu::map`].
    pub unsafe fn fill_shared(
        &self,
        gva: u64,
        host: NonNull<u8>,
        len: u64,
        prot: u32,
    ) -> Result<(), MapError> {
        map_result(unsafe {
            tuxport_mmu_fill(self.raw.as_ptr(), gva, host.as_ptr().cast(), len, prot)
        })
    }

    /// The first page in `[gva, end)` with an entry (including ones with permission 0); `end` if
    /// there is none. Spans without tables are skipped whole, so it is fast even when only a few
    /// pages of a multi-TB mapping were touched.
    pub fn next_present(&self, gva: u64, end: u64) -> u64 {
        unsafe { tuxport_mmu_next_present(self.raw.as_ptr(), gva, end) }
    }

    /// Same as [`Mmu::map`], but needs only `&self`: during copy-on-write the kernel layer modifies
    /// another address space's page table through a shared borrow. `Mmu` cannot be shared across
    /// threads, so modifications are still serial; other threads may still walk it at the same
    /// time.
    ///
    /// # Safety
    ///
    /// Same as [`Mmu::map`].
    pub unsafe fn map_shared(
        &self,
        gva: u64,
        host: NonNull<u8>,
        len: u64,
        prot: u32,
    ) -> Result<(), MapError> {
        map_result(unsafe {
            tuxport_mmu_map(self.raw.as_ptr(), gva, host.as_ptr().cast(), len, prot)
        })
    }

    /// Same as [`Mmu::protect`], but needs only `&self` (see [`Mmu::map_shared`]).
    pub fn protect_shared(&self, gva: u64, len: u64, prot: u32) -> Result<(), MapError> {
        map_result(unsafe { tuxport_mmu_protect(self.raw.as_ptr(), gva, len, prot) })
    }

    /// Looks up the host pointer for a guest address; every permission in `required_prot` must be
    /// present, and passing 0 only requires an accessible entry (ones with permission 0 do not
    /// count).
    pub fn translate(&self, gva: u64, required_prot: u32) -> Option<NonNull<u8>> {
        NonNull::new(unsafe { tuxport_mmu_translate(self.raw.as_ptr(), gva, required_prot) }.cast())
    }
}

impl Default for Mmu {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Mmu {
    fn drop(&mut self) {
        unsafe { tuxport_mmu_free(self.raw.as_ptr()) }
    }
}

fn map_result(rc: c_int) -> Result<(), MapError> {
    match rc {
        0 => Ok(()),
        -2 => Err(MapError::OutOfMemory),
        _ => Err(MapError::InvalidArgument),
    }
}

// ---- CPU state and exits ----

/// Memory access type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Data read.
    Read,
    /// Data write.
    Write,
    /// Instruction fetch.
    Exec,
}

/// Kind of memory access failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// Unmapped or wrong permissions; corresponds to SIGSEGV.
    Page,
    /// The PC, or a data access that requires alignment (exclusive access, atomics), is
    /// misaligned; corresponds to SIGBUS.
    Alignment,
    /// SP misaligned on an SP-based memory access (`addr` is SP); corresponds to SIGBUS.
    StackAlignment,
    /// The access hit a hardware watchpoint (`addr` is the start of the access); corresponds to
    /// SIGTRAP; the instruction has not executed.
    Watchpoint,
    /// The pc hit a hardware breakpoint (`addr` is the pc); corresponds to SIGTRAP.
    HwBreakpoint,
}

/// Exit reasons returned by [`Cpu::run`]: events that need the kernel layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// `svc #imm`; pc already points at the next instruction.
    Svc {
        /// The svc immediate; Linux only uses 0.
        imm: u16,
    },
    /// An undefined instruction, or one outside the whitelist; pc points at the instruction.
    Undefined {
        /// Instruction encoding.
        insn: u32,
    },
    /// `brk #imm`; pc points at the instruction.
    Breakpoint {
        /// The brk immediate.
        imm: u16,
    },
    /// Memory access failure; pc points at the faulting instruction, which had no side effects.
    Fault {
        /// The faulting guest address (TBI tag removed).
        addr: u64,
        /// Memory access type.
        access: Access,
        /// Failure kind.
        kind: FaultKind,
    },
    /// Interrupted by a kick from another thread.
    Kick,
    /// `ic ivau`: the guest modified the code at `addr`; pc already points at the next
    /// instruction. Every [`Dbt`] sharing this address space must invalidate its translations
    /// there.
    InvalidateCode {
        /// The instruction's address operand (TBI tag removed).
        addr: u64,
    },
    /// A bug in the engine itself made the host receive a fault signal (only after
    /// [`install_fault_guard`]): guest state cannot be trusted and this guest process should be
    /// ended. The [`Dbt`] that was in use is already poisoned and is released directly when
    /// dropped.
    HostFault {
        /// The signal the host received: SIGSEGV, SIGBUS or SIGILL.
        signal: i32,
        /// The host's `si_code`.
        code: i32,
        /// The host's `si_addr`.
        addr: u64,
        /// The faulting host instruction.
        pc: u64,
    },
}

/// A guest hardware thread.
///
/// `Cpu` holds a raw pointer to the [`Mmu`] it is bound to, and the caller must ensure that the
/// `Mmu` outlives the `Cpu`; after changing the `Mmu`'s mappings, [`Cpu::tlb_flush`] must be
/// called.
pub struct Cpu {
    /// Allocated (mmap) by `tuxport_cpu_new`, released on `Drop`.
    state: NonNull<CpuState>,
}

// SAFETY: only one thread runs a Cpu at a time (`&mut self`), but it can be handed over between
// threads; the caller keeps the Mmu it points to alive, and the page table itself allows
// concurrent walks
unsafe impl Send for Cpu {}

impl Cpu {
    /// Creates a CPU with all registers 0, bound to `mmu`.
    ///
    /// # Safety
    ///
    /// `mmu` must outlive the returned `Cpu`.
    pub unsafe fn new(mmu: &Mmu) -> Self {
        let state =
            NonNull::new(unsafe { tuxport_cpu_new(mmu.raw.as_ptr()) }).unwrap_or_else(|| {
                std::alloc::handle_alloc_error(std::alloc::Layout::new::<CpuState>())
            });
        Cpu { state }
    }

    /// A handle for interrupting this Cpu.
    pub fn kicker(&self) -> Kicker {
        Kicker(self.state)
    }

    /// Executes from the current pc until the kernel layer is needed.
    pub fn run(&mut self) -> Exit {
        let rc = unsafe { tuxport_cpu_run(self.state.as_ptr()) };
        self.decode_exit(rc)
            .expect("tuxport_cpu_run returned EXIT_NONE")
    }

    /// Executes the given instruction at the current pc (without fetching). Returns `None` when
    /// it completes normally.
    pub fn exec_insn(&mut self, insn: u32) -> Option<Exit> {
        let rc = unsafe { tuxport_cpu_exec_insn(self.state.as_ptr(), insn) };
        self.decode_exit(rc)
    }

    /// Executes the instruction at pc under the interpreter (ptrace single-step), ignoring kicks.
    /// Returns `None` when it completes normally.
    pub fn step(&mut self) -> Option<Exit> {
        let rc = unsafe { tuxport_cpu_step(self.state.as_ptr()) };
        self.decode_exit(rc)
    }

    pub(crate) fn decode_exit(&self, rc: c_int) -> Option<Exit> {
        let s = self.state();
        Some(match rc {
            0 => return None,
            1 => Exit::Svc {
                imm: s.exit_info as u16,
            },
            2 => Exit::Undefined { insn: s.exit_info },
            3 => Exit::Breakpoint {
                imm: s.exit_info as u16,
            },
            4 => Exit::Fault {
                addr: s.fault_addr,
                access: match s.fault_access {
                    1 => Access::Read,
                    2 => Access::Write,
                    _ => Access::Exec,
                },
                kind: match s.fault_kind {
                    2 => FaultKind::Alignment,
                    3 => FaultKind::StackAlignment,
                    4 => FaultKind::Watchpoint,
                    5 => FaultKind::HwBreakpoint,
                    _ => FaultKind::Page,
                },
            },
            5 => Exit::Kick,
            6 => Exit::InvalidateCode { addr: s.fault_addr },
            7 => {
                let mut f = RawHostFault::default();
                unsafe { tuxport_host_fault_last(&mut f) };
                Exit::HostFault {
                    signal: f.signo,
                    code: f.code,
                    addr: f.addr,
                    pc: f.pc,
                }
            }
            _ => panic!("unknown tuxport_exit {rc}"),
        })
    }

    /// Looks up the host pointer for a guest address through the bound page table
    /// ([`Mmu::translate`]); returns None when no page table is bound. The page table may be
    /// walked while another thread modifies it; the caller guarantees how long the memory the
    /// pointer refers to stays valid.
    pub fn translate(&self, gva: u64, required_prot: u32) -> Option<NonNull<u8>> {
        let mmu = self.state().mmu;
        if mmu.is_null() {
            return None;
        }
        // SAFETY: the bound mmu outlives the Cpu (Cpu::new, Cpu::reset)
        NonNull::new(unsafe { tuxport_mmu_translate(mmu, gva, required_prot) }.cast())
    }

    /// Invalidates the whole software TLB.
    pub fn tlb_flush(&mut self) {
        unsafe { tuxport_cpu_tlb_flush(self.state.as_ptr()) }
    }

    /// Clears all state and rebinds to `mmu` (used by execve); the [`Kicker`] stays valid.
    ///
    /// # Safety
    ///
    /// `mmu` must outlive this `Cpu`.
    pub unsafe fn reset(&mut self, mmu: &Mmu) {
        unsafe { tuxport_cpu_init(self.state.as_ptr(), mmu.raw.as_ptr()) }
    }

    /// Guest registers.
    pub fn state(&self) -> &CpuState {
        // SAFETY: state is valid while the Cpu is alive, and &self enforces the borrow rules
        unsafe { self.state.as_ref() }
    }

    /// Mutable guest registers.
    pub fn state_mut(&mut self) -> &mut CpuState {
        // SAFETY: as above; &mut self guarantees exclusive access
        unsafe { self.state.as_mut() }
    }
}

impl Drop for Cpu {
    fn drop(&mut self) {
        unsafe { tuxport_cpu_free(self.state.as_ptr()) }
    }
}

/// A handle for interrupting a [`Cpu`] from other threads ([`Cpu::kicker`]).
#[derive(Clone, Copy)]
pub struct Kicker(NonNull<CpuState>);

// SAFETY: kick only does an atomic write to the CpuState's pending field
unsafe impl Send for Kicker {}
unsafe impl Sync for Kicker {}

impl Kicker {
    /// Makes the corresponding Cpu return [`Exit::Kick`] at the next instruction boundary (the
    /// next block entry in Tier 1); if it is not running now, its next run returns immediately.
    ///
    /// # Safety
    ///
    /// The Cpu that created this handle must still be alive.
    pub unsafe fn kick(&self) {
        unsafe { tuxport_cpu_kick(self.0.as_ptr()) }
    }
}

// ---- Tier 1 ----

/// Tier 1 translation and run statistics (the engine map).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DbtStats {
    /// Blocks translated.
    pub blocks: u64,
    /// Guest instructions translated.
    pub insns: u64,
    /// Instructions copied verbatim.
    pub insns_copied: u64,
    /// Instructions rewritten before execution because they involve a stolen register or SP.
    pub insns_remapped: u64,
    /// Memory access instructions rewritten into the inline TLB fast path.
    pub insns_memory: u64,
    /// Instructions handed to Tier 0 (static count).
    pub insns_fallback: u64,
    /// Exits to the dispatcher for block linking (once per link point).
    pub exits_chain: u64,
    /// Indirect branches that missed the jump cache.
    pub exits_lookup: u64,
    /// Times one instruction was handed to Tier 0 (TLB refill failure, misaligned SP, unsupported
    /// instruction, and so on).
    pub exits_interp: u64,
    /// Exits for syscalls.
    pub exits_svc: u64,
    /// Exits for kicks.
    pub exits_kick: u64,
    /// Times the code cache was flushed.
    pub flushes: u64,
    /// Blocks invalidated by [`Dbt::invalidate`].
    pub invalidated: u64,
    /// Blocks among `blocks` obtained by copying a translation template directly (the same code
    /// translated by another instance).
    pub blocks_shared: u64,
    /// Accesses among `insns_memory` that share one TLB check with an earlier access in the same
    /// block based on the same register.
    pub insns_grouped: u64,
    /// Times a shared check failed and the block was invalidated and retranslated without
    /// grouping.
    pub ungrouped: u64,
}

/// Which host registers Tier 1's translated code takes for itself (the guest registers with the
/// same numbers are kept in [`CpuState`] instead, and instructions referring to them need an
/// extra load or store). Only affects speed.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DbtRegs {
    /// x16, x17, x26–x28: rarely used in compiler-generated code.
    #[default]
    Default,
    /// x21–x25: Go programs use x16 and x26–x28 heavily (stack checks, closure context, the
    /// assembler's temporary register, the current goroutine) and these hardly at all.
    Go,
}

/// A mapping that translations come from ([`DbtSpace::set_sources`]): a guest address range, and
/// an identity (the file and its version) from which identical content can be concluded.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbtSource {
    pub start: u64,
    pub len: u64,
    pub id: [u64; 5],
}

/// The Tier 1 translations of one address space (code cache, translation table), shared by all
/// threads in that address space: each thread gets its own [`Dbt`] through [`Dbt::attach`]. Once
/// the last [`Dbt`] and the references here are gone, it goes back to the pool; when there are
/// not many translations they are kept, and checked block by block before the next user runs.
pub struct DbtSpace {
    raw: NonNull<RawDbt>,
    key: u64,
}

// SAFETY: the C side protects the shared parts with its own lock (dbt.c's struct tuxport_dbt)
unsafe impl Send for DbtSpace {}
unsafe impl Sync for DbtSpace {}

impl DbtSpace {
    /// The most ranges [`DbtSpace::set_sources`] records.
    pub const SOURCES: usize = 64;

    /// Creates an instance (or takes one from the pool); returns `None` when the host is not
    /// aarch64 Linux. `key` distinguishes programs (0 means no distinction): when the pool holds
    /// translations left with the same key and the same `regs`, those are taken back first, and
    /// the instance is also recorded under it when it goes back to the pool.
    pub fn new(key: u64, regs: DbtRegs) -> Option<Arc<Self>> {
        NonNull::new(unsafe { tuxport_dbt_new(key, regs) })
            .map(|raw| Arc::new(DbtSpace { raw, key }))
    }

    /// Before translations taken from the pool first run in a new address space: blocks in
    /// mappings of the new address space that are exactly the same as the recorded ones
    /// ([`Self::set_sources`]) need not be checked block by block.
    pub fn trust(&self, sources: &[DbtSource]) {
        unsafe { tuxport_dbt_trust(self.raw.as_ptr(), sources.as_ptr(), sources.len()) }
    }

    /// Allocates a tag for one address space sharing this instance (not repeated until the
    /// instance goes back to the pool); returns `None` when they run out (60-some thousand).
    pub fn new_tag(&self) -> Option<u16> {
        match unsafe { tuxport_dbt_new_tag(self.raw.as_ptr()) } {
            0 => None,
            tag => Some(tag as u16),
        }
    }

    /// Retires blocks whose guest pc is not in these mappings (parked ones included), except
    /// those valid only for one address space (tagged, see [`DbtSpace::share`]): before several
    /// address spaces share it, drops the translations valid only for the original address
    /// space. Other threads may be running.
    pub fn retire_outside(&self, sources: &[DbtSource]) {
        unsafe { tuxport_dbt_retire_outside(self.raw.as_ptr(), sources.as_ptr(), sources.len()) }
    }

    /// When several address spaces share this instance, the trusted file mappings they all map
    /// identically (all of them given every time, only growing): blocks in these mappings are
    /// shared by all address spaces, and blocks elsewhere (JIT-generated code) are only for the
    /// address space that translated them, told apart by the tag given at [`Dbt::attach`].
    pub fn share(&self, sources: &[DbtSource]) {
        unsafe { tuxport_dbt_share(self.raw.as_ptr(), sources.as_ptr(), sources.len()) }
    }

    /// The address space is no longer in use (the translations go back to the pool): records the
    /// mappings the translations come from, at most [`Self::SOURCES`] ranges. The caller
    /// guarantees that mappings with the same identity have the same content.
    pub fn set_sources(&self, sources: &[DbtSource]) {
        unsafe { tuxport_dbt_set_sources(self.raw.as_ptr(), sources.as_ptr(), sources.len()) }
    }
}

impl Drop for DbtSpace {
    fn drop(&mut self) {
        // Every attached Dbt holds an Arc, so by the time we get here they are all detached
        unsafe { tuxport_dbt_free(self.raw.as_ptr(), self.key) }
    }
}

/// Tier 1: one thread's part of a same-ISA pass-through DBT (jump cache, shadow return stack,
/// statistics), attached to a [`DbtSpace`].
///
/// [`Dbt::run`] handles `ic ivau` in place: it invalidates the page recorded in
/// [`CpuState::ivau_page`] at the next dsb or isb, and the page of an `ic ivau` it hands to Tier 0
/// as soon as Tier 0 runs it. Otherwise translations never invalidate themselves: after
/// [`Exit::InvalidateCode`], after an exit that leaves `ivau_page` set, or after an executable
/// mapping is torn down or changes permissions, [`Dbt::invalidate`] or [`Dbt::flush`] must be
/// called (once, through any one thread attached to the same [`DbtSpace`]). Only aarch64 Linux
/// hosts are supported.
pub struct Dbt {
    raw: NonNull<RawDbtThread>,
    space: Arc<DbtSpace>,
}

// SAFETY: the thread's part is used only by the thread holding this Dbt; the C side locks the
// shared part
unsafe impl Send for Dbt {}

impl Dbt {
    /// Creates an instance used only by this thread; returns `None` when the host is not aarch64
    /// Linux.
    pub fn new() -> Option<Self> {
        Self::with_regs(DbtRegs::Default)
    }

    /// Same as [`Dbt::new`], with the translated code taking the `regs` set of registers.
    pub fn with_regs(regs: DbtRegs) -> Option<Self> {
        Self::attach(&DbtSpace::new(0, regs)?, 0)
    }

    /// Attaches to `space`. `tag` is the tag of the thread's address space
    /// ([`DbtSpace::new_tag`]), 0 when the instance is not shared by several address spaces.
    pub fn attach(space: &Arc<DbtSpace>, tag: u16) -> Option<Self> {
        NonNull::new(unsafe { tuxport_dbt_attach(space.raw.as_ptr(), tag.into()) }).map(|raw| Dbt {
            raw,
            space: space.clone(),
        })
    }

    /// Same semantics as [`Cpu::run`], but executes in translated code.
    pub fn run(&mut self, cpu: &mut Cpu) -> Exit {
        let rc = unsafe { tuxport_dbt_run(self.raw.as_ptr(), cpu.state.as_ptr()) };
        cpu.decode_exit(rc)
            .expect("tuxport_dbt_run returned EXIT_NONE")
    }

    /// Discards all translations: this has to wait until every thread has left translated code,
    /// and is completed by the next [`Dbt::run`].
    pub fn flush(&mut self) {
        unsafe { tuxport_dbt_flush(self.raw.as_ptr()) }
    }

    /// About to run in a different address space (after exec): keeps the translations and,
    /// before the next run, checks the guest instruction words block by block, invalidating the
    /// ones that do not match, and clearing everything when most do not match. Cheaper than
    /// copying the translation templates again after [`Dbt::flush`].
    pub fn recheck(&mut self) {
        unsafe { tuxport_dbt_recheck(self.raw.as_ptr()) }
    }

    /// Invalidates, among the blocks starting in the pages covered by `[start, start + len)`,
    /// those whose guest instruction words differ from the current ones in `cpu`'s address space
    /// (a translation depends only on the pc and the instruction words); when there is no room,
    /// it is the same as [`Dbt::flush`].
    pub fn invalidate(&mut self, cpu: &Cpu, start: u64, len: u64) {
        unsafe { tuxport_dbt_invalidate(self.raw.as_ptr(), cpu.state.as_ptr(), start, len) }
    }

    /// The attached instance.
    pub fn space(&self) -> &Arc<DbtSpace> {
        &self.space
    }

    /// This thread's statistics: how many times it exited, and the translations and
    /// invalidations it did.
    pub fn stats(&self) -> DbtStats {
        let mut stats = DbtStats::default();
        unsafe { tuxport_dbt_get_stats(self.raw.as_ptr(), &mut stats) };
        stats
    }
}

impl Drop for Dbt {
    fn drop(&mut self) {
        unsafe { tuxport_dbt_detach(self.raw.as_ptr()) }
    }
}

/// Releases the Tier 1 translations kept in the pool: nobody is using them, so they can be
/// dropped when host memory is tight (Android's `onTrimMemory`), and the next run of the same
/// program copies the templates again.
pub fn trim_dbt_pool() {
    unsafe { tuxport_dbt_trim() }
}

/// Unmaps the pages of Tier 1's code cache that were written through the writable view (content
/// kept). The code cache is mapped twice for W^X, the host's RSS counts pages per mapping, and
/// pages touched through both views count twice; writing again later only faults them back in.
/// Must be called periodically: the kernel layer's timer thread does it once a second.
pub fn unmap_written_code() {
    unsafe { tuxport_dbt_unmap_written() }
}

/// How many bytes in total were written through the writable view and not yet unmapped
/// ([`unmap_written_code`]): they count twice in the host's RSS.
pub fn written_code_bytes() -> u64 {
    unsafe { tuxport_dbt_written_bytes() }
}

// ---- Process-wide ----

/// Installs the host-side fault guard (the engine map, design doc §12 item 5); may be called
/// repeatedly. Afterwards, SIGSEGV, SIGBUS and SIGILL received by the host while [`Cpu::run`] or
/// [`Dbt::run`] runs become [`Exit::HostFault`] and affect only the guest running at the time;
/// other faults first write a one-line report to stderr and are then passed to the original
/// handling (on Android, debuggerd produces a tombstone from it).
pub fn install_fault_guard() {
    unsafe { tuxport_fault_guard_install() }
}

/// The `AT_HWCAP` advertised to the guest: host support ∩ implemented by Tier 0 ∩ the whitelist
/// (the engine map).
pub fn guest_hwcap() -> u64 {
    unsafe { tuxport_guest_hwcap() }
}

/// The `AT_HWCAP2` advertised to the guest, by the same rule as [`guest_hwcap`].
pub fn guest_hwcap2() -> u64 {
    unsafe { tuxport_guest_hwcap2() }
}

/// The `MIDR_EL1` the guest reads (used by `/proc/cpuinfo`); 0 when the host does not allow
/// reading ID registers.
pub fn guest_midr() -> u64 {
    unsafe { tuxport_guest_midr() }
}

/// HWCAP bits, with the same values as Linux's `HWCAP_*`.
pub mod hwcap {
    /// Floating point.
    pub const FP: u64 = 1 << 0;
    /// Advanced SIMD.
    pub const ASIMD: u64 = 1 << 1;
    /// AES instructions.
    pub const AES: u64 = 1 << 3;
    /// The 1Q form of PMULL.
    pub const PMULL: u64 = 1 << 4;
    /// SHA1 instructions.
    pub const SHA1: u64 = 1 << 5;
    /// SHA256 instructions.
    pub const SHA2: u64 = 1 << 6;
    /// CRC32 instructions.
    pub const CRC32: u64 = 1 << 7;
    /// LSE atomic instructions.
    pub const ATOMICS: u64 = 1 << 8;
    /// Scalar half-precision floating point (FP16).
    pub const FPHP: u64 = 1 << 9;
    /// Advanced SIMD half-precision floating point (FP16).
    pub const ASIMDHP: u64 = 1 << 10;
    /// EL0 may read ID registers.
    pub const CPUID: u64 = 1 << 11;
    /// RDM: SQRDMLAH, SQRDMLSH.
    pub const ASIMDRDM: u64 = 1 << 12;
    /// FJCVTZS (double to int32 with JavaScript semantics).
    pub const JSCVT: u64 = 1 << 13;
    /// FCMA: complex multiply-add and add (FCMLA, FCADD).
    pub const FCMA: u64 = 1 << 14;
    /// RCpc load-acquire (LDAPR, LDAPRB, LDAPRH).
    pub const LRCPC: u64 = 1 << 15;
    /// DC CVAP (clean to the point of persistence).
    pub const DCPOP: u64 = 1 << 16;
    /// SHA3 instructions (EOR3, RAX1, XAR, BCAX).
    pub const SHA3: u64 = 1 << 17;
    /// Advanced SIMD dot product instructions (SDOT, UDOT).
    pub const ASIMDDP: u64 = 1 << 20;
    /// SHA512 instructions.
    pub const SHA512: u64 = 1 << 21;
    /// FHM: half-precision multiply, accumulating into single precision (FMLAL, FMLSL).
    pub const ASIMDFHM: u64 = 1 << 23;
    /// DIT: "data independent timing" in PSTATE (MSR, MRS DIT).
    pub const DIT: u64 = 1 << 24;
    /// LRCPC2: LDAPUR, STLUR and the like, with an offset.
    pub const ILRCPC: u64 = 1 << 26;
    /// FlagM: CFINV, RMIF, SETF8, SETF16.
    pub const FLAGM: u64 = 1 << 27;
    /// The speculation barrier SB.
    pub const SB: u64 = 1 << 29;
}

/// `AT_HWCAP2` bits, with the same values as Linux's `HWCAP2_*`.
pub mod hwcap2 {
    /// DC CVADP (clean to the point of deep persistence).
    pub const DCPODP: u64 = 1 << 0;
    /// FlagM2: AXFLAG, XAFLAG.
    pub const FLAGM2: u64 = 1 << 7;
    /// FRINTTS: FRINT32Z, FRINT32X, FRINT64Z, FRINT64X.
    pub const FRINT: u64 = 1 << 8;
    /// int8 matrix multiply (SMMLA, UMMLA, USMMLA, USDOT, SUDOT).
    pub const I8MM: u64 = 1 << 13;
    /// BFloat16 (BFDOT, BFMMLA, BFMLALB/T, BFCVT, BFCVTN).
    pub const BF16: u64 = 1 << 14;
}

/// Checks whether [`CpuState`]'s layout matches the C struct.
pub fn layout_matches() -> bool {
    unsafe {
        tuxport_cpu_layout_check(
            size_of::<CpuState>(),
            offset_of!(CpuState, x),
            offset_of!(CpuState, pc),
            offset_of!(CpuState, tpidr_el0),
            offset_of!(CpuState, pending),
            offset_of!(CpuState, mmu),
            offset_of!(CpuState, dbt_jump_cache),
            offset_of!(CpuState, tlb),
            offset_of!(CpuState, v),
        ) == 1
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn layout_matches_c() {
        assert!(super::layout_matches());
    }

    /// A failed internal check (an engine bug found without a host fault) under the guard ends
    /// the run as a host fault with SIGABRT, so only the guest process is ended, instead of
    /// aborting the app; the registered locks are released, nested ones beyond the four recorded
    /// included in the pairing.
    #[test]
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        target_arch = "aarch64"
    ))]
    fn failed_internal_checks_end_only_the_run() {
        unsafe extern "C" {
            fn tuxport_guard_selftest() -> i32;
        }
        super::install_fault_guard();
        assert_eq!(unsafe { tuxport_guard_selftest() }, libc::SIGABRT);
    }
}
