//! Instruction-level lockstep tests (design doc §13.3, layer 1): the host CPU is the reference
//! answer.
//!
//! Each random case (at most 4 instructions, random registers and FP state, random data area
//! contents) runs once each on the hardware, in Tier 0 and in Tier 1 (once with each of the two
//! register assignments, see `DbtRegs`), comparing X0–X30, NZCV, V0–V31, FPCR, FPSR, PC, SP and
//! the data area item by item. The native side can only be compiled on Linux aarch64 hosts; on
//! other hosts [`AVAILABLE`] is false.

pub mod classes;
#[cfg(lockstep_native)]
mod native;
pub mod rng;
#[cfg(lockstep_native)]
pub mod whitelist;

use std::fmt;

/// Whether the current host can run lockstep.
pub const AVAILABLE: bool = cfg!(lockstep_native);

/// The state before and after the tested instructions run, with the same layout as oracle.c's
/// `struct lockstep_state`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    /// X0–X30.
    pub x: [u64; 31],
    /// Condition flags, bits 31..28.
    pub nzcv: u64,
    /// V0–V31, low 64 bits first.
    pub v: [[u64; 2]; 32],
    /// Floating-point control register.
    pub fpcr: u64,
    /// Floating-point status register.
    pub fpsr: u64,
}

/// Parameters of one run.
#[derive(Debug, Clone)]
pub struct Config {
    /// How many cases each class generates (multiplied by the class's weight).
    pub iterations: u64,
    /// Random seed; the same seed always generates the same test sequence.
    pub seed: u64,
    /// Run only this class.
    pub class: Option<String>,
}

/// Statistics for one class.
#[derive(Debug, Clone)]
pub struct ClassReport {
    /// Class name.
    pub name: &'static str,
    /// Times the hardware and both interpreter tiers executed with matching results.
    pub executed: u64,
    /// Times all three judged the instruction undefined.
    pub both_undefined: u64,
    /// Times the hardware could execute and the interpreter rejected by the whitelist (only in
    /// fuzz classes).
    pub stricter: u64,
    /// Times Tier 1 fell back to Tier 0 for a single instruction (TLB refill failure or an
    /// untranslated instruction).
    pub tier1_fallback: u64,
    /// Accesses translated by Tier 1 that share one TLB check with an earlier access on the same
    /// base.
    pub tier1_grouped: u64,
    /// The guest feature set lacks a feature the class needs, so it did not run.
    pub skipped: bool,
}

/// The result after one side executes.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Register state.
    pub state: State,
    /// Data area contents.
    pub data: Vec<u8>,
}

/// The first mismatch.
#[derive(Debug, Clone)]
pub struct Mismatch {
    /// Class name.
    pub class: &'static str,
    /// The tested instructions.
    pub insns: Vec<u32>,
    /// This run's seed.
    pub seed: u64,
    /// Sequence number within the class.
    pub iteration: u64,
    /// Reason for the mismatch.
    pub reason: String,
    /// State before execution.
    pub before: State,
    /// Result after the hardware executes.
    pub hardware: Option<Outcome>,
    /// Result after the interpreter tier that diverged executes.
    pub interpreter: Option<Outcome>,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let insns: Vec<String> = self.insns.iter().map(|i| format!("{i:#010x}")).collect();
        writeln!(
            f,
            "lockstep mismatch in class `{}`: insns [{}]: {}",
            self.class,
            insns.join(", "),
            self.reason
        )?;
        writeln!(
            f,
            "  reproduce: cargo run -p tuxport-lockstep -- --class {} --seed {}  (iteration {})",
            self.class, self.seed, self.iteration
        )?;
        let (Some(hw), Some(interp)) = (&self.hardware, &self.interpreter) else {
            return Ok(());
        };
        let (b, h, i) = (&self.before, &hw.state, &interp.state);
        for r in 0..31 {
            if h.x[r] != i.x[r] {
                writeln!(
                    f,
                    "  x{r:<2} before={:#018x} hardware={:#018x} interpreter={:#018x}",
                    b.x[r], h.x[r], i.x[r]
                )?;
            }
        }
        for r in 0..32 {
            if h.v[r] != i.v[r] {
                let q = |v: [u64; 2]| format!("{:016x}_{:016x}", v[1], v[0]);
                writeln!(
                    f,
                    "  v{r:<2} before={} hardware={} interpreter={}",
                    q(b.v[r]),
                    q(h.v[r]),
                    q(i.v[r])
                )?;
            }
        }
        for (name, bv, hv, iv) in [
            ("nzcv", b.nzcv, h.nzcv, i.nzcv),
            ("fpcr", b.fpcr, h.fpcr, i.fpcr),
            ("fpsr", b.fpsr, h.fpsr, i.fpsr),
        ] {
            if hv != iv {
                writeln!(
                    f,
                    "  {name} before={bv:#x} hardware={hv:#x} interpreter={iv:#x}"
                )?;
            }
        }
        let diff: Vec<usize> = (0..hw.data.len().min(interp.data.len()))
            .filter(|&k| hw.data[k] != interp.data[k])
            .collect();
        if let Some(&first) = diff.first() {
            writeln!(
                f,
                "  data: {} bytes differ, first at offset {first:#x} (hardware {:#04x}, interpreter {:#04x})",
                diff.len(),
                hw.data[first],
                interp.data[first]
            )?;
        }
        Ok(())
    }
}

/// Runs lockstep and returns the statistics of each class; returns the first mismatch when one
/// is found.
#[cfg(lockstep_native)]
pub fn run(config: &Config) -> Result<Vec<ClassReport>, Box<Mismatch>> {
    runner::run(config)
}

/// The current host does not support lockstep ([`AVAILABLE`] is false); calling this panics.
#[cfg(not(lockstep_native))]
pub fn run(_config: &Config) -> Result<Vec<ClassReport>, Box<Mismatch>> {
    panic!("lockstep needs a Linux aarch64 host")
}

#[cfg(lockstep_native)]
mod runner {
    use std::ptr::NonNull;

    use tuxport_engine::{Cpu, Dbt, DbtRegs, Exit, Mmu, prot};

    use super::*;
    use crate::classes::{CLASSES, Case, Class, DATA_BYTES, Env, Gen};
    use crate::native::Oracle;
    use crate::rng::Rng;

    /// SP is set to this value before the interpreter runs; the tested instructions should not
    /// change it.
    const SP_SENTINEL: u64 = 0x0000_7fff_dead_bef0;
    const NOP: u32 = 0xd503_201f;
    const SVC: u32 = 0xd400_0001;
    /// Exclusive access cases may occasionally fail on hardware; on a mismatch, rerun at most this
    /// many times.
    const FLAKY_RETRIES: usize = 8;

    #[repr(C, align(4096))]
    struct Page([u8; 4096]);

    /// The interpreter side's execution environment: the tested instructions' code page and the
    /// data area, mapped at the same guest addresses as on the hardware.
    struct Machine {
        // Drop order: dbt → cpu → mmu → pages
        dbt: Option<Dbt>,
        cpu: Cpu,
        _mmu: Mmu,
        code: Box<Page>,
        data: Box<[Page; 2]>,
        slot: u64,
    }

    impl Machine {
        /// `tier1` is the register assignment for Tier 1; without one, Tier 0 is used.
        fn new(slot: u64, data_va: u64, tier1: Option<DbtRegs>) -> Self {
            let mut code = Box::new(Page([0; 4096]));
            let mut data = Box::new([Page([0; 4096]), Page([0; 4096])]);
            let mut mmu = Mmu::new();
            let code_host = NonNull::from(&mut code.0).cast::<u8>();
            let data_host = NonNull::from(&mut *data).cast::<u8>();
            // SAFETY: Machine owns both pieces of memory, which outlive mmu
            unsafe {
                mmu.map(slot & !0xfff, code_host, 4096, prot::READ | prot::EXEC)
                    .unwrap();
                mmu.map(data_va, data_host, DATA_BYTES, prot::READ | prot::WRITE)
                    .unwrap();
            }
            // SAFETY: Machine owns mmu, which outlives cpu
            let cpu = unsafe { Cpu::new(&mmu) };
            let dbt =
                tier1.map(|regs| Dbt::with_regs(regs).expect("tier 1 is unavailable on this host"));
            Machine {
                dbt,
                cpu,
                _mmu: mmu,
                code,
                data,
                slot,
            }
        }

        /// Lays out the code page and data area as on the hardware, and runs up to the svc at the
        /// 5th word.
        fn run(
            &mut self,
            page: &[u8],
            insns: &[u32],
            before: &State,
            data: &[u8],
        ) -> (Exit, State) {
            let off = (self.slot & 0xfff) as usize;
            self.code.0.copy_from_slice(page);
            for k in 0..4 {
                let insn = insns.get(k).copied().unwrap_or(NOP);
                self.code.0[off + 4 * k..off + 4 * k + 4].copy_from_slice(&insn.to_le_bytes());
            }
            self.code.0[off + 16..off + 20].copy_from_slice(&SVC.to_le_bytes());
            // SAFETY: the two pages are stored contiguously in the array
            let dst = unsafe {
                std::slice::from_raw_parts_mut(
                    self.data.as_mut_ptr().cast::<u8>(),
                    DATA_BYTES as usize,
                )
            };
            dst.copy_from_slice(data);

            let s = self.cpu.state_mut();
            s.x = before.x;
            s.nzcv = before.nzcv;
            s.v = before.v;
            s.fpcr = before.fpcr;
            s.fpsr = before.fpsr;
            s.sp = SP_SENTINEL;
            s.pc = self.slot;
            let exit = match &mut self.dbt {
                Some(dbt) => {
                    dbt.flush();
                    dbt.run(&mut self.cpu)
                }
                None => self.cpu.run(),
            };
            let s = self.cpu.state();
            let after = State {
                x: s.x,
                nzcv: s.nzcv,
                v: s.v,
                fpcr: s.fpcr,
                fpsr: s.fpsr,
            };
            (exit, after)
        }

        fn data(&self) -> &[u8] {
            // SAFETY: the two pages are stored contiguously in the array
            unsafe {
                std::slice::from_raw_parts(self.data.as_ptr().cast::<u8>(), DATA_BYTES as usize)
            }
        }
    }

    struct Harness {
        oracle: Oracle,
        page: Vec<u8>,
        tiers: [(&'static str, Machine); 3],
    }

    pub(super) fn run(config: &Config) -> Result<Vec<ClassReport>, Box<Mismatch>> {
        let oracle = Oracle::new().expect("cannot create lockstep oracle");
        let slot = oracle.slot_addr();
        let data_va = oracle.data_addr();
        let page = oracle.slot_page().to_vec();
        let mut h = Harness {
            oracle,
            page,
            tiers: [
                ("tier 0", Machine::new(slot, data_va, None)),
                (
                    "tier 1",
                    Machine::new(slot, data_va, Some(DbtRegs::Default)),
                ),
                (
                    "tier 1 (Go registers)",
                    Machine::new(slot, data_va, Some(DbtRegs::Go)),
                ),
            ],
        };
        let mut reports = Vec::new();
        for class in CLASSES {
            if config.class.as_deref().is_some_and(|c| c != class.name) {
                continue;
            }
            reports.push(run_class(class, config, &mut h)?);
        }
        Ok(reports)
    }

    fn run_class(
        class: &'static Class,
        config: &Config,
        h: &mut Harness,
    ) -> Result<ClassReport, Box<Mismatch>> {
        let mut report = ClassReport {
            name: class.name,
            executed: 0,
            both_undefined: 0,
            stricter: 0,
            tier1_fallback: 0,
            tier1_grouped: 0,
            skipped: crate::classes::guest_caps() & class.requires != class.requires,
        };
        if report.skipped {
            return Ok(report);
        }
        let stats = |h: &Harness| {
            h.tiers[1]
                .1
                .dbt
                .as_ref()
                .map(|d| d.stats())
                .unwrap_or_default()
        };
        let start = stats(h);
        let mut g = Rng::new(class_seed(config.seed, class.name));
        let slot = h.oracle.slot_addr();
        let data_va = h.oracle.data_addr();
        let env = Env {
            slot_offset: slot & 0xfff,
        };

        for iteration in 0..config.iterations * class.weight {
            let case = match class.generate {
                Gen::Insn(f) => Case {
                    insns: vec![f(&mut g)],
                    ..Case::default()
                },
                Gen::Case(f) => f(&mut g, &env),
            };
            let mut before = random_state(&mut g);
            let mut data = vec![0u8; DATA_BYTES as usize];
            if !case.pointers.is_empty() {
                for chunk in data.chunks_mut(8) {
                    chunk.copy_from_slice(&g.next_u64().to_le_bytes());
                }
            }
            for &(r, off) in &case.pointers {
                before.x[r as usize] = data_va.wrapping_add(off as u64);
            }
            for &(r, v) in &case.values {
                if r < 31 {
                    before.x[r as usize] = v;
                }
            }
            for &(r, off) in &case.loads {
                // Near the end of the data area there are fewer than 8 bytes; the high part is
                // filled with 0 (the instruction only uses the low part)
                let mut bytes = [0u8; 8];
                let end = (off as usize + 8).min(data.len());
                bytes[..end - off as usize].copy_from_slice(&data[off as usize..end]);
                before.x[r as usize] = u64::from_le_bytes(bytes);
            }

            let mismatch =
                |reason: String, hardware: Option<Outcome>, interpreter: Option<Outcome>| {
                    Box::new(Mismatch {
                        class: class.name,
                        insns: case.insns.clone(),
                        seed: config.seed,
                        iteration,
                        reason,
                        before,
                        hardware,
                        interpreter,
                    })
                };

            let mut hw = run_hardware(&mut h.oracle, &case.insns, &before, &data);
            let mut kinds = Vec::new();
            for t in 0..h.tiers.len() {
                let name = h.tiers[t].0;
                let machine = &mut h.tiers[t].1;
                let (exit, state) = machine.run(&h.page, &case.insns, &before, &data);
                let pc = machine.cpu.state().pc;
                let sp = machine.cpu.state().sp;
                let interp = Outcome {
                    state,
                    data: machine.data().to_vec(),
                };
                let verdict = |hw: &(Result<(), i32>, Outcome)| -> Result<&'static str, String> {
                    match (hw.0, exit) {
                        (Err(4), Exit::Undefined { .. }) => Ok("undefined"),
                        (Err(4), other) => {
                            Err(format!("hardware raised SIGILL, {name} returned {other:?}"))
                        }
                        (Err(sig), _) => Err(format!(
                            "hardware raised signal {sig}; the generator must not emit such cases"
                        )),
                        (Ok(()), Exit::Undefined { .. }) if !class.strict => Ok("stricter"),
                        (Ok(()), Exit::Svc { .. }) => {
                            if hw.1.state != interp.state || hw.1.data != interp.data {
                                Err(format!("{name}: results differ"))
                            } else if pc != slot + 20 || sp != SP_SENTINEL {
                                Err(format!("{name} left pc={pc:#x} sp={sp:#x}"))
                            } else {
                                Ok("executed")
                            }
                        }
                        (Ok(()), other) => {
                            Err(format!("hardware executed it, {name} returned {other:?}"))
                        }
                    }
                };
                let mut result = verdict(&hw);
                // On hardware an interrupt may clear the mark of an exclusive access; rerun a few
                // times to see whether the same result comes out
                let mut retries = 0;
                while result.is_err() && class.flaky && retries < FLAKY_RETRIES {
                    hw = run_hardware(&mut h.oracle, &case.insns, &before, &data);
                    result = verdict(&hw);
                    retries += 1;
                }
                match result {
                    Ok(kind) => kinds.push(kind),
                    Err(reason) => return Err(mismatch(reason, Some(hw.1.clone()), Some(interp))),
                }
            }
            if let Some(t) = (1..kinds.len()).find(|&t| kinds[t] != kinds[0]) {
                return Err(mismatch(
                    format!(
                        "{} and {} disagree: {} vs {}",
                        h.tiers[0].0, h.tiers[t].0, kinds[0], kinds[t]
                    ),
                    None,
                    None,
                ));
            }
            match kinds[0] {
                "executed" => report.executed += 1,
                "undefined" => report.both_undefined += 1,
                _ => report.stricter += 1,
            }
        }
        let end = stats(h);
        report.tier1_fallback = end.exits_interp - start.exits_interp;
        report.tier1_grouped = end.insns_grouped - start.insns_grouped;
        Ok(report)
    }

    /// Makes the random sequences of the classes independent of each other while each can still
    /// be reproduced alone.
    fn class_seed(seed: u64, name: &str) -> u64 {
        name.bytes().fold(seed ^ 0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }

    fn random_state(g: &mut Rng) -> State {
        let mut x = [0; 31];
        for v in &mut x {
            *v = random_value(g);
        }
        let mut v = [[0; 2]; 32];
        for lanes in &mut v {
            *lanes = [random_lane(g), random_lane(g)];
        }
        State {
            x,
            nzcv: (g.bits(4) as u64) << 28,
            v,
            fpcr: g.next_u64() & 0x07c0_0000, // AHP, DN, FZ, RMode
            fpsr: g.next_u64() & 0x0800_009f, // QC and the cumulative exception bits
        }
    }

    /// General register values biased towards boundary values, to cover more flag and overflow
    /// paths.
    fn random_value(g: &mut Rng) -> u64 {
        const SPECIAL: &[u64] = &[
            0,
            1,
            2,
            0x7f,
            0x80,
            0xff,
            0x7fff_ffff,
            0x8000_0000,
            0xffff_ffff,
            0x1_0000_0000,
            0xffff_ffff_8000_0000,
            0x7fff_ffff_ffff_ffff,
            0x8000_0000_0000_0000,
            0xffff_ffff_ffff_fffe,
            0xffff_ffff_ffff_ffff,
        ];
        match g.below(8) {
            0 | 1 => g.pick(SPECIAL),
            2 => g.below(64) as u64,
            3 => (g.below(64) as i64 - 32) as u64,
            4 => g.next_u64() & 0xffff_ffff,
            5 => g.pick(SPECIAL) ^ (1 << g.below(64)),
            _ => g.next_u64(),
        }
    }

    /// A 64-bit vector lane biased towards floating-point special values: one double, or two
    /// singles.
    fn random_lane(g: &mut Rng) -> u64 {
        const F64: &[u64] = &[
            0,
            0x8000_0000_0000_0000, // -0
            0x3ff0_0000_0000_0000, // 1.0
            0xbff0_0000_0000_0000, // -1.0
            0x3fe0_0000_0000_0000, // 0.5
            0x7ff0_0000_0000_0000, // +inf
            0xfff0_0000_0000_0000, // -inf
            0x7ff8_0000_0000_0000, // qNaN
            0x7ff4_0000_0000_0001, // sNaN
            0x0000_0000_0000_0001, // smallest subnormal
            0x000f_ffff_ffff_ffff, // largest subnormal
            0x0010_0000_0000_0000, // smallest normal
            0x7fef_ffff_ffff_ffff, // largest finite
            0x41e0_0000_0000_0000, // 2^31
            0x43e0_0000_0000_0000, // 2^63
        ];
        const F32: &[u32] = &[
            0,
            0x8000_0000,
            0x3f80_0000,
            0xbf80_0000,
            0x3f00_0000,
            0x7f80_0000,
            0xff80_0000,
            0x7fc0_0000,
            0x7fa0_0001,
            0x0000_0001,
            0x007f_ffff,
            0x0080_0000,
            0x7f7f_ffff,
            0x4f00_0000,
            0x5f00_0000,
        ];
        match g.below(4) {
            0 => g.pick(F64),
            1 => g.pick(F32) as u64 | (g.pick(F32) as u64) << 32,
            _ => g.next_u64(),
        }
    }

    fn run_hardware(
        oracle: &mut Oracle,
        insns: &[u32],
        before: &State,
        data: &[u8],
    ) -> (Result<(), i32>, Outcome) {
        oracle.data().copy_from_slice(data);
        let mut state = *before;
        let result = oracle.run(insns, &mut state);
        (
            result,
            Outcome {
                state,
                data: oracle.data().to_vec(),
            },
        )
    }
}
