//! lockstep within `cargo test`: 20000 instructions per class by default, with a random seed
//! each time.
//!
//! The environment variables `TUXPORT_LOCKSTEP_ITERS` and `TUXPORT_LOCKSTEP_SEED` override the
//! defaults; failure messages include a command to reproduce.

use std::time::{SystemTime, UNIX_EPOCH};

use tuxport_lockstep::{AVAILABLE, Config, run};

#[test]
fn interpreter_matches_hardware() {
    if !AVAILABLE {
        eprintln!("skipped: lockstep needs a Linux aarch64 host");
        return;
    }
    let seed = env_u64("TUXPORT_LOCKSTEP_SEED").unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    });
    let config = Config {
        iterations: env_u64("TUXPORT_LOCKSTEP_ITERS").unwrap_or(20_000),
        seed,
        class: None,
    };
    eprintln!("lockstep seed = {seed}");
    match run(&config) {
        Ok(reports) => {
            let tier1 = tuxport_engine::Dbt::new().is_some();
            for r in reports.iter().filter(|r| !r.skipped) {
                assert!(
                    r.executed > 0,
                    "class {} never executed an instruction",
                    r.name
                );
                assert!(
                    !tier1 || r.name != "same_base" || r.tier1_grouped > 0,
                    "tier 1 never shared a TLB check in class same_base"
                );
            }
        }
        Err(mismatch) => panic!("{mismatch}"),
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().map(|v| {
        v.parse()
            .unwrap_or_else(|_| panic!("{name} must be a number"))
    })
}
