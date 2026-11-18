//! Benchmark programs for the M2 performance probe (design doc §14).
//!
//! The programs are in `programs/`; they do not depend on libc, use only `write`/`exit_group`, and
//! are built with `-mgeneral-regs-only` to rule out SIMD, so that both M0's kernel layer and M2's
//! prototype Tier 1 can run them. Building needs gcc on a Linux aarch64 host.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A benchmark program.
#[derive(Debug, Clone, Copy)]
pub struct Program {
    /// Name, also the source file name under `programs/`.
    pub name: &'static str,
    /// Rounds passed in when measuring, chosen so that a native run takes about 0.3 seconds.
    pub rounds: u64,
    /// A small number of rounds for correctness tests.
    pub test_rounds: u64,
    /// The main workload characteristic it exercises.
    pub workload: &'static str,
}

/// All benchmark programs.
pub const PROGRAMS: &[Program] = &[
    Program {
        name: "sha256",
        rounds: 4000,
        test_rounds: 4,
        workload: "32-bit ALU",
    },
    Program {
        name: "lz",
        rounds: 160,
        test_rounds: 1,
        workload: "loads, stores, branches",
    },
    Program {
        name: "sort",
        rounds: 24,
        test_rounds: 1,
        workload: "loads, stores, unpredictable branches",
    },
    Program {
        name: "crc32",
        rounds: 2000,
        test_rounds: 4,
        workload: "table lookups",
    },
    Program {
        name: "vm",
        rounds: 8_000_000,
        test_rounds: 20_000,
        workload: "indirect branches",
    },
];

/// Compiler flags: static, non-PIE, no libc, no SIMD and no PAC/BTI.
pub const CFLAGS: &[&str] = &[
    "-O2",
    "-std=c11",
    "-static",
    "-no-pie",
    "-fno-pie",
    "-nostdlib",
    "-ffreestanding",
    "-mgeneral-regs-only",
    "-fno-stack-protector",
    "-mbranch-protection=none",
    "-fno-tree-loop-distribute-patterns",
    "-fno-asynchronous-unwind-tables",
    "-Wall",
    "-Wextra",
    "-Werror",
    "-Wl,--build-id=none",
];

/// Compiles `program` into `out_dir` and returns the executable's path.
pub fn build(program: &Program, out_dir: &Path) -> Result<PathBuf, String> {
    let dir = programs_dir();
    let out = out_dir.join(program.name);
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let output = Command::new(&cc)
        .args(CFLAGS)
        .arg("-o")
        .arg(&out)
        .arg(dir.join(format!("{}.c", program.name)))
        .arg(dir.join("rt.c"))
        .output()
        .map_err(|e| format!("failed to run {cc}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "building {} failed:\n{}",
            program.name,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(out)
}

/// Source directory.
pub fn programs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("programs")
}

/// A short description of the host CPU, read from `/proc/cpuinfo`, counted by grouping core models,
/// for example `ARM 0xd49 Neoverse-N2 x4`. With mixed big and little cores, each kind of core is
/// listed in turn.
pub fn host_cpu() -> String {
    let Ok(info) = std::fs::read_to_string("/proc/cpuinfo") else {
        return "unknown".into();
    };
    let hex = |v: &str| u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok();
    let mut cores: Vec<(String, usize)> = Vec::new();
    for block in info.split("\n\n") {
        let field = |name: &str| {
            block.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                (k.trim() == name).then_some(v)
            })
        };
        let (Some(implementer), Some(part)) = (
            field("CPU implementer").and_then(hex),
            field("CPU part").and_then(hex),
        ) else {
            continue;
        };
        let mut label = format!("{} {part:#05x}", vendor_name(implementer));
        if let Some(name) = (implementer == 0x41).then(|| arm_part_name(part)).flatten() {
            label = format!("{label} {name}");
        }
        match cores.iter_mut().find(|(l, _)| *l == label) {
            Some((_, n)) => *n += 1,
            None => cores.push((label, 1)),
        }
    }
    if cores.is_empty() {
        return "unknown".into();
    }
    cores
        .iter()
        .map(|(label, n)| format!("{label} x{n}"))
        .collect::<Vec<_>>()
        .join(" + ")
}

fn vendor_name(implementer: u32) -> String {
    match implementer {
        0x41 => "ARM".into(),
        0x46 => "Fujitsu".into(),
        0x48 => "HiSilicon".into(),
        0x4e => "NVIDIA".into(),
        0x51 => "Qualcomm".into(),
        0x61 => "Apple".into(),
        0xc0 => "Ampere".into(),
        other => format!("implementer {other:#04x}"),
    }
}

/// The MIDR part numbers of Arm's own cores. Parts not in the table show only the number.
fn arm_part_name(part: u32) -> Option<&'static str> {
    Some(match part {
        0xd03 => "Cortex-A53",
        0xd04 => "Cortex-A35",
        0xd05 => "Cortex-A55",
        0xd07 => "Cortex-A57",
        0xd08 => "Cortex-A72",
        0xd09 => "Cortex-A73",
        0xd0a => "Cortex-A75",
        0xd0b => "Cortex-A76",
        0xd0c => "Neoverse-N1",
        0xd0d => "Cortex-A77",
        0xd40 => "Neoverse-V1",
        0xd41 => "Cortex-A78",
        0xd44 => "Cortex-X1",
        0xd46 => "Cortex-A510",
        0xd47 => "Cortex-A710",
        0xd48 => "Cortex-X2",
        0xd49 => "Neoverse-N2",
        0xd4d => "Cortex-A715",
        0xd4e => "Cortex-X3",
        0xd4f => "Neoverse-V2",
        0xd80 => "Cortex-A520",
        0xd81 => "Cortex-A720",
        0xd82 => "Cortex-X4",
        0xd84 => "Neoverse-V3",
        0xd85 => "Cortex-X925",
        0xd87 => "Cortex-A725",
        _ => return None,
    })
}
