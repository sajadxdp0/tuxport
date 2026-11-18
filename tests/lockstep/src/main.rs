//! Long-running lockstep:
//!
//! ```text
//! tuxport-lockstep [--iterations N] [--seed S] [--class NAME]
//! ```

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use tuxport_lockstep::{AVAILABLE, Config, classes::CLASSES, run};

const USAGE: &str = "usage: tuxport-lockstep [--iterations N] [--seed S] [--class NAME]";

fn main() -> ExitCode {
    let mut config = Config {
        iterations: 100_000,
        seed: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
        class: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                println!(
                    "classes: {}",
                    CLASSES
                        .iter()
                        .map(|c| c.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                return ExitCode::SUCCESS;
            }
            "--iterations" | "--seed" | "--class" => args.next(),
            _ => None,
        };
        let Some(value) = value else {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        };
        match arg.as_str() {
            "--iterations" | "--seed" => {
                let Ok(n) = value.parse() else {
                    eprintln!("tuxport-lockstep: {arg} expects a number");
                    return ExitCode::from(2);
                };
                if arg == "--seed" {
                    config.seed = n;
                } else {
                    config.iterations = n;
                }
            }
            _ => {
                if !CLASSES.iter().any(|c| c.name == value) {
                    eprintln!("tuxport-lockstep: unknown class `{value}`");
                    return ExitCode::from(2);
                }
                config.class = Some(value);
            }
        }
    }

    if !AVAILABLE {
        eprintln!("tuxport-lockstep: needs a Linux aarch64 host");
        return ExitCode::from(2);
    }
    println!(
        "seed = {}, iterations per class = {}",
        config.seed, config.iterations
    );
    match run(&config) {
        Ok(reports) => {
            for r in reports {
                if r.skipped {
                    println!("{:<18} skipped: the guest feature set lacks it", r.name);
                    continue;
                }
                println!(
                    "{:<18} executed {:>8}  both-undefined {:>8}  rejected-by-whitelist {:>8}  tier1-fallback {:>8}  tier1-grouped {:>8}",
                    r.name,
                    r.executed,
                    r.both_undefined,
                    r.stricter,
                    r.tier1_fallback,
                    r.tier1_grouped
                );
            }
            ExitCode::SUCCESS
        }
        Err(mismatch) => {
            eprint!("{mismatch}");
            ExitCode::FAILURE
        }
    }
}
