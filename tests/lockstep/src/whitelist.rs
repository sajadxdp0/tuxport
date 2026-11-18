//! Whitelist cross-check (the engine map): the encodings Tier 0 considers executable must be
//! exactly the encodings LLVM can decode with the features we advertise.
//!
//! lockstep checks semantics with the hardware, but the hardware cannot tell whether an encoding
//! belongs to the ARMv8.0 base set or to an extension we do not advertise (FP16, for example).
//! LLVM's disassembler only decodes enabled features, which fills exactly that gap.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use tuxport_engine::{Cpu, Exit, Mmu, hwcap, hwcap2};

use crate::rng::Rng;

/// A sampled encoding space.
pub struct Space {
    /// Name.
    pub name: &'static str,
    /// Bits fixed at 1 or 0.
    pub mask: u32,
    /// The values of those bits.
    pub value: u32,
    /// The space has executable encodings only when the guest feature set has these HWCAP bits.
    pub requires: u64,
}

/// All sampled spaces.
pub const SPACES: &[Space] = &[
    Space {
        name: "simd_fp_dp",
        mask: 0x0e00_0000,
        value: 0x0e00_0000, // bits 27:25 = 111
        requires: 0,
    },
    Space {
        name: "simd_ldst",
        mask: 0x0e00_0000,
        value: 0x0c00_0000, // bits 27:25 = 110: memory accesses with bit 26 = 1
        requires: 0,
    },
    Space {
        name: "exclusive",
        mask: 0x3f00_0000,
        value: 0x0800_0000,
        requires: 0,
    },
    Space {
        name: "lse",
        mask: 0x3f20_0c00,
        value: 0x3820_0000,
        requires: 0,
    },
    Space {
        name: "dp_2src",
        mask: 0x5fe0_0000,
        value: 0x1ac0_0000,
        requires: 0,
    },
    Space {
        name: "adc_flags",
        mask: 0x1fe0_0000,
        value: 0x1a00_0000, // ADC, SBC and FlagM's RMIF, SETF8, SETF16
        requires: 0,
    },
    Space {
        name: "rcpc_unscaled",
        mask: 0x3f20_0c00,
        value: 0x1900_0000, // LRCPC2's LDAPUR*, STLUR*
        requires: hwcap::ILRCPC,
    },
];

/// One disagreement.
#[derive(Debug, Clone)]
pub struct Disagreement {
    /// Encoding space.
    pub space: &'static str,
    /// Instruction encoding.
    pub insn: u32,
    /// Whether Tier 0 considers it executable.
    pub tier0: bool,
    /// Whether LLVM can decode it (with the advertised features).
    pub llvm: bool,
    /// LLVM's disassembly with all features enabled, to identify the instruction.
    pub text: String,
}

/// Sampling statistics of one encoding space.
#[derive(Debug, Clone)]
pub struct SpaceReport {
    /// Encoding space.
    pub space: &'static str,
    /// Number of samples.
    pub samples: usize,
    /// Number of samples Tier 0 considers executable.
    pub defined: usize,
    /// The guest has the features this space needs (without them the space should have no
    /// executable encodings).
    pub available: bool,
}

/// Finds a usable llvm-mc: the environment variable `TUXPORT_LLVM_MC`, otherwise from PATH.
pub fn find_llvm_mc() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("TUXPORT_LLVM_MC") {
        return Some(PathBuf::from(p));
    }
    let names = [
        "llvm-mc",
        "llvm-mc-19",
        "llvm-mc-18",
        "llvm-mc-17",
        "llvm-mc-16",
    ];
    names.iter().map(PathBuf::from).find(|name| {
        Command::new(name)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Compares Tier 0 with LLVM by sampling. Returns each space's statistics and the list of
/// disagreements (at most `limit`).
pub fn check(
    llvm_mc: &Path,
    samples: usize,
    seed: u64,
    limit: usize,
) -> (Vec<SpaceReport>, Vec<Disagreement>) {
    let caps = tuxport_engine::guest_hwcap();
    let caps2 = tuxport_engine::guest_hwcap2();
    let dir = std::env::temp_dir();
    let mmu = Mmu::new();
    // SAFETY: mmu is declared before cpu and dropped after it
    let mut cpu = unsafe { Cpu::new(&mmu) };
    let mut found = Vec::new();
    let mut reports = Vec::new();
    for (i, space) in SPACES.iter().enumerate() {
        let mut g = Rng::new(seed ^ (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let insns: Vec<u32> = (0..samples)
            .map(|_| (g.next_u64() as u32 & !space.mask) | space.value)
            .collect();
        let restricted = disassemble(llvm_mc, &insns, &mattr(caps, caps2), &dir);
        let mut disagree = Vec::new();
        let mut defined = 0;
        for (k, &insn) in insns.iter().enumerate() {
            cpu.state_mut().pc = 0x1000;
            cpu.state_mut().sp = 0;
            let tier0 = !matches!(cpu.exec_insn(insn), Some(Exit::Undefined { .. }));
            defined += tier0 as usize;
            if tier0 != restricted[k].0 {
                disagree.push((insn, tier0, restricted[k].0));
            }
        }
        reports.push(SpaceReport {
            space: space.name,
            samples,
            defined,
            available: caps & space.requires == space.requires,
        });
        if disagree.is_empty() {
            continue;
        }
        // Disassemble again with all features, to identify which instruction a disagreeing
        // encoding is
        let codes: Vec<u32> = disagree.iter().map(|d| d.0).collect();
        let full = disassemble(llvm_mc, &codes, "+all", &dir);
        for ((insn, tier0, llvm), (_, text)) in disagree.into_iter().zip(full) {
            if found.len() < limit {
                found.push(Disagreement {
                    space: space.name,
                    insn,
                    tier0,
                    llvm,
                    text,
                });
            }
        }
    }
    (reports, found)
}

/// The LLVM `-mattr` for a guest feature set.
fn mattr(caps: u64, caps2: u64) -> String {
    let flag = |on: bool, name: &str| format!("{}{name}", if on { '+' } else { '-' });
    // LLVM's fullfp16 covers both scalar and vector half precision, and the guest always
    // advertises them together too
    let mut attrs = vec![flag(caps & hwcap::FPHP != 0, "fullfp16")];
    attrs.push(flag(caps & hwcap::FP != 0, "fp-armv8"));
    attrs.push(flag(caps & hwcap::ASIMD != 0, "neon"));
    attrs.push(flag(caps & (hwcap::AES | hwcap::PMULL) != 0, "aes"));
    attrs.push(flag(caps & (hwcap::SHA1 | hwcap::SHA2) != 0, "sha2"));
    attrs.push(flag(caps & hwcap::CRC32 != 0, "crc"));
    attrs.push(flag(caps & hwcap::ATOMICS != 0, "lse"));
    attrs.push(flag(caps & hwcap::ASIMDDP != 0, "dotprod"));
    // LLVM's sha3 covers both SHA3 and SHA512, and the guest always advertises them together too
    attrs.push(flag(caps & hwcap::SHA3 != 0, "sha3"));
    attrs.push(flag(caps2 & hwcap2::I8MM != 0, "i8mm"));
    attrs.push(flag(caps & hwcap::LRCPC != 0, "rcpc"));
    attrs.push(flag(caps & hwcap::ILRCPC != 0, "rcpc-immo"));
    attrs.push(flag(caps & hwcap::JSCVT != 0, "jsconv"));
    attrs.push(flag(caps & hwcap::ASIMDRDM != 0, "rdm"));
    attrs.push(flag(caps & hwcap::FCMA != 0, "complxnum"));
    attrs.push(flag(caps & hwcap::ASIMDFHM != 0, "fp16fml"));
    attrs.push(flag(caps2 & hwcap2::BF16 != 0, "bf16"));
    attrs.push(flag(caps2 & hwcap2::FRINT != 0, "fptoint"));
    attrs.push(flag(caps & hwcap::FLAGM != 0, "flagm"));
    attrs.push(flag(caps2 & hwcap2::FLAGM2 != 0, "altnzcv"));
    attrs.push(flag(caps & hwcap::SB != 0, "sb"));
    attrs.join(",")
}

/// Disassembles a batch of encodings with llvm-mc, returning whether each is valid and its
/// disassembly text.
fn disassemble(llvm_mc: &Path, insns: &[u32], mattr: &str, dir: &Path) -> Vec<(bool, String)> {
    let input = dir.join(format!("whitelist-{}.txt", std::process::id()));
    let mut text = String::new();
    for insn in insns {
        let b = insn.to_le_bytes();
        writeln!(
            text,
            "{:#04x} {:#04x} {:#04x} {:#04x}",
            b[0], b[1], b[2], b[3]
        )
        .unwrap();
    }
    std::fs::write(&input, text).expect("cannot write llvm-mc input");
    let out = Command::new(llvm_mc)
        .args([
            "--disassemble",
            "-triple=aarch64",
            &format!("-mattr={mattr}"),
        ])
        .arg(&input)
        .output()
        .expect("cannot run llvm-mc");
    let _ = std::fs::remove_file(&input);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let invalid: HashSet<usize> = stderr
        .lines()
        .filter(|l| l.contains("invalid instruction encoding"))
        .filter_map(|l| l.split(':').nth(1)?.parse().ok())
        .collect();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut decoded = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('.'));
    (1..=insns.len())
        .map(|line| {
            if invalid.contains(&line) {
                (false, String::new())
            } else {
                (true, decoded.next().unwrap_or("?").to_string())
            }
        })
        .collect()
}
