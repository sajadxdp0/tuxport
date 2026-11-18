//! Correctness of the M2 benchmark programs: run natively, under Tier 0 and under Tier 1, the
//! outputs must be identical byte for byte.
//!
//! Only very small round counts are used here, to check correctness; for timing comparisons see
//! `tuxport-bench`. Needs a Linux aarch64 host.
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

use std::path::Path;
use std::process::{Command, Output};

use tuxport_bench::{PROGRAMS, build};

#[test]
fn benchmarks_agree_across_tiers() {
    let out_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("bench-programs");
    std::fs::create_dir_all(&out_dir).unwrap();
    for program in PROGRAMS {
        let exe = build(program, &out_dir).unwrap_or_else(|e| panic!("{e}"));
        let rounds = program.test_rounds.to_string();
        let native = check(
            Command::new(&exe).arg(&rounds).output().unwrap(),
            &format!("{} (native)", program.name),
        );
        for tier in ["0", "1"] {
            let output = Command::new(env!("CARGO_BIN_EXE_tuxport-run"))
                .args(["--tier", tier])
                .arg(&exe)
                .arg(&rounds)
                .output()
                .unwrap();
            let got = check(output, &format!("{} (tier {tier})", program.name));
            assert_eq!(
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&native),
                "{} under tier {tier}",
                program.name
            );
        }
    }
}

fn check(output: Output, what: &str) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{what} failed ({}): {}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
