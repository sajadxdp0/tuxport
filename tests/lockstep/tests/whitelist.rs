//! Tier 0's instruction whitelist compared one by one with the LLVM disassembler (the engine
//! map).
//!
//! Needs a Linux aarch64 host and llvm-mc (`TUXPORT_LLVM_MC` or llvm-mc in PATH); skipped when
//! either is missing. The number of samples can be adjusted with `TUXPORT_WHITELIST_SAMPLES`.

#[cfg(lockstep_native)]
#[test]
fn whitelist_matches_llvm() {
    use tuxport_lockstep::whitelist::{check, find_llvm_mc};

    let Some(llvm_mc) = find_llvm_mc() else {
        eprintln!("skipped: llvm-mc not found");
        return;
    };
    let samples = std::env::var("TUXPORT_WHITELIST_SAMPLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_000);
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    eprintln!("whitelist seed = {seed}");
    let (reports, found) = check(&llvm_mc, samples, seed, 40);
    for r in &reports {
        eprintln!(
            "  {:<12} {} samples, {} executable",
            r.space, r.samples, r.defined
        );
        assert!(
            r.defined > 0 || !r.available,
            "no executable samples in {}",
            r.space
        );
    }
    if !found.is_empty() {
        let lines: Vec<String> = found
            .iter()
            .map(|d| {
                format!(
                    "  [{}] {:#010x} tier0={} llvm={}  {}",
                    d.space, d.insn, d.tier0, d.llvm, d.text
                )
            })
            .collect();
        panic!(
            "tier 0 whitelist disagrees with llvm-mc (seed {seed}):\n{}",
            lines.join("\n")
        );
    }
}
