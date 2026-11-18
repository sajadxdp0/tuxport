//! Compiles the C engine (design doc §13.1: the engine is C17 + aarch64 assembly).

fn main() {
    let sources = [
        "src/c/atomic.c",
        "src/c/cpu.c",
        "src/c/dbt.c",
        "src/c/features.c",
        "src/c/guard.c",
        "src/c/interp.c",
        "src/c/mmu.c",
        "src/c/native.c",
        "src/c/simd.c",
    ];
    for path in sources.iter().chain(&[
        "src/c/emit.h",
        "src/c/internal.h",
        "include/tuxport/engine.h",
    ]) {
        println!("cargo::rerun-if-changed={path}");
    }

    cc::Build::new()
        .files(sources)
        .include("include")
        .std("c17")
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true)
        .compile("tuxport_engine");

    // Lets dependents' C code include the headers: the environment variable
    // DEP_TUXPORT_ENGINE_INCLUDE
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let include = std::path::Path::new(&manifest_dir).join("include");
    println!("cargo::metadata=include={}", include.display());
}
