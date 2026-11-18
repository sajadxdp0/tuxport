//! M2 performance probe: runs the benchmark programs natively, under Tier 0 and under Tier 1, and
//! compares the times (design doc §14).
//!
//! ```text
//! cargo build --release -p tuxport-run -p tuxport-bench
//! target/release/tuxport-bench [--runs N] [--quick] [--skip-tier0] [--only NAME] [--stats]
//! ```
//!
//! Needs a Linux aarch64 host: the benchmark programs must run natively. Each way takes the
//! shortest time of several runs, and the output of the three ways must be identical byte for byte.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use tuxport_bench::{PROGRAMS, Program, build, host_cpu};

const USAGE: &str =
    "usage: tuxport-bench [--runs N] [--quick] [--skip-tier0] [--only NAME] [--stats]";

struct Options {
    runs: usize,
    quick: bool,
    skip_tier0: bool,
    only: Option<String>,
    stats: bool,
}

fn main() -> ExitCode {
    let o = match parse_options() {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    if !cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        eprintln!("tuxport-bench: needs a Linux aarch64 host");
        return ExitCode::from(2);
    }
    let exe_dir: PathBuf = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let tuxport_run = exe_dir.join("tuxport-run");
    if !tuxport_run.exists() {
        eprintln!(
            "tuxport-bench: {} not found; build it first:\n  cargo build --release -p tuxport-run -p tuxport-bench",
            tuxport_run.display()
        );
        return ExitCode::from(2);
    }
    let out_dir = exe_dir.join("bench-programs");
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("tuxport-bench: {}: {e}", out_dir.display());
        return ExitCode::FAILURE;
    }

    println!("host: {}", host_cpu());
    println!(
        "{:<8} {:<38} {:>9} {:>18} {:>16}",
        "program", "workload", "native", "tier 0", "tier 1"
    );
    let (mut log0, mut log1, mut n0, mut n1) = (0.0f64, 0.0f64, 0, 0);
    for program in PROGRAMS {
        if o.only.as_deref().is_some_and(|n| n != program.name) {
            continue;
        }
        let result =
            build(program, &out_dir).and_then(|exe| run_program(program, &exe, &tuxport_run, &o));
        let (native, tier0, tier1) = match result {
            Ok(r) => r,
            Err(msg) => {
                eprintln!("tuxport-bench: {msg}");
                return ExitCode::FAILURE;
            }
        };
        let ratio = |t: Duration| t.as_secs_f64() / native.as_secs_f64();
        let t0 = match tier0 {
            Some(t) => {
                log0 += ratio(t).ln();
                n0 += 1;
                format!("{:.2}s ({:.1}x)", t.as_secs_f64(), ratio(t))
            }
            None => "-".into(),
        };
        log1 += ratio(tier1).ln();
        n1 += 1;
        println!(
            "{:<8} {:<38} {:>8.3}s {:>18} {:>16}",
            program.name,
            program.workload,
            native.as_secs_f64(),
            t0,
            format!("{:.3}s ({:.2}x)", tier1.as_secs_f64(), ratio(tier1))
        );
    }
    let geomean = |sum: f64, n: i32| {
        if n == 0 {
            "-".to_string()
        } else {
            format!("{:.2}x", (sum / n as f64).exp())
        }
    };
    println!(
        "{:<8} {:<38} {:>9} {:>18} {:>16}",
        "geomean",
        "",
        "",
        geomean(log0, n0),
        geomean(log1, n1)
    );
    ExitCode::SUCCESS
}

fn parse_options() -> Result<Options, String> {
    let mut o = Options {
        runs: 5,
        quick: false,
        skip_tier0: false,
        only: None,
        stats: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runs" => {
                o.runs = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|&n| n > 0)
                    .ok_or("--runs expects a positive number")?;
            }
            "--quick" => o.quick = true,
            "--skip-tier0" => o.skip_tier0 = true,
            "--stats" => o.stats = true,
            "--only" => {
                let name = args.next().ok_or("--only expects a program name")?;
                if !PROGRAMS.iter().any(|p| p.name == name) {
                    return Err(format!("unknown program `{name}`"));
                }
                o.only = Some(name);
            }
            _ => return Err(USAGE.into()),
        }
    }
    Ok(o)
}

fn run_program(
    program: &Program,
    exe: &Path,
    tuxport_run: &Path,
    o: &Options,
) -> Result<(Duration, Option<Duration>, Duration), String> {
    let rounds = if o.quick {
        (program.rounds / 20).max(1)
    } else {
        program.rounds
    }
    .to_string();
    let (native, expected) = measure(&os(&[&exe, &rounds]), o.runs)?;
    let tier0 = if o.skip_tier0 {
        None
    } else {
        let (t, out) = measure(&os(&[&tuxport_run, &"--tier", &"0", &exe, &rounds]), 1)?;
        if out != expected {
            return Err(format!(
                "{}: tier 0 output differs from native",
                program.name
            ));
        }
        Some(t)
    };
    let (tier1, out) = measure(&os(&[&tuxport_run, &"--tier", &"1", &exe, &rounds]), o.runs)?;
    if out != expected {
        return Err(format!(
            "{}: tier 1 output differs from native",
            program.name
        ));
    }
    if o.stats {
        let out = Command::new(tuxport_run)
            .args(["--tier", "1", "--stats"])
            .arg(exe)
            .arg(&rounds)
            .output()
            .map_err(|e| e.to_string())?;
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok((native, tier0, tier1))
}

/// Runs `runs` times and returns the shortest time and the standard output; every run must succeed
/// with the same output.
fn measure(cmd: &[OsString], runs: usize) -> Result<(Duration, Vec<u8>), String> {
    let mut best = Duration::MAX;
    let mut first: Option<Vec<u8>> = None;
    for _ in 0..runs {
        let start = Instant::now();
        let out = Command::new(&cmd[0])
            .args(&cmd[1..])
            .output()
            .map_err(|e| format!("failed to run {:?}: {e}", cmd[0]))?;
        let elapsed = start.elapsed();
        if !out.status.success() {
            return Err(format!(
                "{:?} failed ({}):\n{}{}",
                cmd,
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        match &first {
            Some(f) if *f != out.stdout => {
                return Err(format!("{cmd:?}: output changed between runs"));
            }
            Some(_) => {}
            None => first = Some(out.stdout),
        }
        best = best.min(elapsed);
    }
    Ok((best, first.unwrap_or_default()))
}

fn os(args: &[&dyn AsRef<std::ffi::OsStr>]) -> Vec<OsString> {
    args.iter().map(|a| a.as_ref().to_owned()).collect()
}
