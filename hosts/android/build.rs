//! Builds libghostty-vt (static, with Zig) and vt.c, the terminal emulator of the app's tabs
//! (src/vt.rs), which also uses the Wuffs that libghostty-vt links. For a host other than Android
//! (where the crate is empty) it does nothing.
//!
//! The libghostty-vt source comes from scripts/fetch-ghostty
//! (target/ghostty/libghostty-vt-<version>, or TUXPORT_GHOSTTY_SRC), with the release's Zig
//! packages beside it (the same path with `-deps`), which `zig build --system` takes, so this build
//! needs no network. The Zig binary is `zig` on PATH or ZIG; the build finds the NDK through
//! ANDROID_NDK_HOME, which the Gradle task sets.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const GHOSTTY: &str = "libghostty-vt-1.0.0";

fn main() {
    println!("cargo:rerun-if-changed=src/vt.c");
    println!("cargo:rerun-if-env-changed=TUXPORT_GHOSTTY_SRC");
    println!("cargo:rerun-if-env-changed=ZIG");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let src = env::var_os("TUXPORT_GHOSTTY_SRC").map_or_else(
        || manifest.join("../../target/ghostty").join(GHOSTTY),
        PathBuf::from,
    );
    assert!(
        src.join("build.zig").is_file(),
        "no libghostty-vt source at {}: run scripts/fetch-ghostty",
        src.display()
    );
    let mut deps = src.clone().into_os_string();
    deps.push("-deps");
    let deps = PathBuf::from(deps);
    assert!(
        deps.is_dir(),
        "no libghostty-vt packages at {}: run scripts/fetch-ghostty",
        deps.display()
    );
    // The release's version and its packages' hashes
    println!(
        "cargo:rerun-if-changed={}",
        src.join("LIB_VERSION").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        src.join("build.zig.zon").display()
    );

    // The release's own version, passed to the Zig build so it never derives one from git: the
    // unpacked source sits inside this repository's work tree, and libghostty-vt's git-based
    // detection reads that state; on a tagged checkout its check that the checked-out tag
    // matches the lib's version would panic on the repository's own tag.
    let version = fs::read_to_string(src.join("LIB_VERSION"))
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", src.join("LIB_VERSION").display()))
        .trim()
        .to_string();
    assert!(
        !version.is_empty(),
        "empty LIB_VERSION in {}",
        src.display()
    );

    let target = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "aarch64-linux-android",
        other => panic!("libghostty-vt: unsupported Android architecture {other:?}"),
    };
    let prefix = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("ghostty");
    let zig = env::var_os("ZIG").unwrap_or_else(|| "zig".into());
    // --system takes the packages from the release's dependency tarball and never fetches. The
    // global cache is the source's own, beside Zig's local cache in it (.zig-cache), so the
    // builds of every profile share it: one that holds a package's archive, as the user's
    // ~/.cache/zig may, crashes Zig 0.17's build with "reached unreachable code"
    // (PACKAGING-LIBGHOSTTY.md)
    let status = Command::new(&zig)
        .current_dir(&src)
        .args([
            "build",
            "-Demit-lib-vt",
            "-Dcpu=baseline",
            "-Doptimize=ReleaseFast",
        ])
        .arg(format!("-Dtarget={target}"))
        .arg(format!("-Dversion-string={version}"))
        .arg("--system")
        .arg(&deps)
        .arg("--prefix")
        .arg(&prefix)
        .env("ZIG_GLOBAL_CACHE_DIR", src.join(".zig-global-cache"))
        .status()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", zig.to_string_lossy()));
    assert!(
        status.success(),
        "zig build of libghostty-vt failed: {status}"
    );

    cc::Build::new()
        .file("src/vt.c")
        .include(prefix.join("include"))
        .include(wuffs(&src, &deps))
        .std("c11")
        .warnings(true)
        .extra_warnings(true)
        .compile("tuxport_vt");
    // After vt.c, which uses it
    println!(
        "cargo:rustc-link-search=native={}",
        prefix.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=ghostty-vt");
}

/// The directory of Wuffs's single-file C library among the release's Zig packages. libghostty-vt
/// links Wuffs for Kitty graphics, and vt.c decodes PNG images with it, so it compiles against
/// those declarations; the package is the one the source's `pkg/wuffs/build.zig.zon` names by hash.
fn wuffs(src: &Path, deps: &Path) -> PathBuf {
    let zon = src.join("pkg/wuffs/build.zig.zon");
    let text =
        fs::read_to_string(&zon).unwrap_or_else(|e| panic!("cannot read {}: {e}", zon.display()));
    let hash = text
        .split_once(".wuffs = .{")
        .and_then(|(_, dependency)| dependency.split_once(".hash = \""))
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(hash, _)| hash)
        .unwrap_or_else(|| panic!("no Wuffs package hash in {}", zon.display()));
    let dir = deps.join(hash).join("release/c");
    assert!(
        dir.join("wuffs-v0.4.c").is_file(),
        "no Wuffs at {}: run scripts/fetch-ghostty",
        dir.display()
    );
    dir
}
