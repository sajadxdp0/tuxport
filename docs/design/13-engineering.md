# 13. Engineering

## 13.1 Languages and interfaces

- **Engine**: C17 + aarch64 assembly, built with Clang (NDK).
- **Kernel layer**: Rust; it can draw on the designs of Fuchsia Starnix and Asterinas.
- **Android shell**: Kotlin.

The interface between the engine and the kernel layer is narrow and follows an **exit-reason** model: the engine never calls back into the kernel layer. `tuxport_cpu_run()` (Tier 0) or `tuxport_dbt_run()` (Tier 1) runs until an event needs the kernel layer and returns an exit reason; the kernel layer handles it and calls back in to continue. There is no reentrancy between C and Rust, which keeps the FFI boundary as simple as it can be. The full definitions are in `core/engine/include/tuxport/engine.h`, the Rust bindings in `core/engine/src/lib.rs`.

```c
/* Run: returns one of SVC / UNDEF / BRK / FAULT / KICK / CODE / HOST_FAULT */
enum tuxport_exit tuxport_cpu_run(struct tuxport_cpu *cpu);                          /* Tier 0 */
enum tuxport_exit tuxport_dbt_run(struct tuxport_dbt *dbt, struct tuxport_cpu *cpu);  /* Tier 1 */
void tuxport_cpu_kick(struct tuxport_cpu *cpu);   /* callable from any thread: the target exits at the next instruction boundary */
void tuxport_cpu_tlb_flush(struct tuxport_cpu *cpu);

/* soft-MMU page tables: maintained by the kernel layer; after a change, flush the TLB of every CPU sharing them */
int tuxport_mmu_map(struct tuxport_mmu *mmu, uint64_t gva, void *host, uint64_t len, uint32_t prot);
int tuxport_mmu_unmap(struct tuxport_mmu *mmu, uint64_t gva, uint64_t len);
int tuxport_mmu_protect(struct tuxport_mmu *mmu, uint64_t gva, uint64_t len, uint32_t prot);
void *tuxport_mmu_translate(const struct tuxport_mmu *mmu, uint64_t gva, uint32_t required_prot);

/* Tier 1 translations never invalidate themselves: the kernel layer discards them from the code invalidation log, see the engine map */
void tuxport_dbt_invalidate(struct tuxport_dbt *dbt, uint64_t start, uint64_t len);
void tuxport_dbt_flush(struct tuxport_dbt *dbt);
void tuxport_dbt_recheck(struct tuxport_dbt *dbt);  /* after exec: keep the translations, verify each block before it next runs */
/* Before going back to the pool, record the mappings the translations came from; after coming out of the pool,
 * blocks in mappings identical to the recorded ones need no verification */
void tuxport_dbt_set_sources(struct tuxport_dbt *dbt, const struct tuxport_dbt_source *s, size_t n);
void tuxport_dbt_trust(struct tuxport_dbt *dbt, const struct tuxport_dbt_source *s, size_t n);

/* Host-side fault guard (engine map): from then on, SIGSEGV/SIGBUS/SIGILL while running become HOST_FAULT exits */
void tuxport_fault_guard_install(void);
```

- On a FAULT exit, pc points at the faulting instruction, and `fault_addr`/`fault_access`/`fault_kind` give the address, the access type, and whether it was a page fault, an alignment fault or an SP alignment fault; the instruction has had no side effects.
- A CODE exit means the guest executed `ic ivau`: pc already points at the next instruction, `fault_addr` is the address the instruction names, and the kernel layer records that page in the code invalidation log.
- A HOST_FAULT exit means a bug in the engine itself made the host receive a fault signal ([engine map](../../.agents/maps/engine.md)): the guest state cannot be trusted, the kernel layer ends the process, and `tuxport_host_fault_last()` gives the details.

The guest state (`struct tuxport_cpu`) holds `x[31]`, `sp`, `pc`, `nzcv`, `fpcr`, `fpsr`, `tpidr_el0`, `orig_x0`, the fault information, `exit_info`, the pending flags, the mmu pointer, the exclusive monitor (address, size, value read), a few fields Tier 1 uses internally (exit reason, jump cache, shadow return stack), the software TLB and the instruction-fetch TLB, and `v[32]` (128-bit). The fields that Tier 1's generated code addresses directly with immediate offsets come before the TLB. The Rust side mirrors the struct field by field, and `tuxport_cpu_layout_check()` verifies the layout in tests.

## 13.2 Repository layout

```
core/engine/     C engine (include/tuxport/engine.h, src/c/) and its Rust bindings; tests/ holds the Tier 0 and Tier 1 tests
core/kernel/     Rust kernel layer (syscall/ split into files by category; vfs/ holds the store and the device nodes)
core/hal/        host abstraction layer (one implementation shared by Android and Linux)
hosts/cli/       tuxport-run (the CLI front end for development) and end-to-end tests (guest programs, rootfs, benchmarks)
hosts/android/   the Android app's native library libtuxport.so (JNI)
tests/guest/     guest test programs: hand-written assembly, and C programs built with musl-gcc
tests/lockstep/  instruction-level lockstep: native trampoline, random instruction generation, Tier 0 and Tier 1 compared at once; the whitelist checked against llvm-mc
tests/bench/     performance probe: separately compiled benchmark programs and the measurement tool tuxport-bench
tests/diff/      differential test scripts
tests/survey/    workload survey scripts
tests/crash/     power-loss test: cuts a write workload off on dm-flakey at a random moment, then checks the store's integrity (root in the VM)
scripts/         dev (runs a command in a Linux arm64 container), fetch-rootfs, difftest, build-ltp, ltp, survey, release-sources
android/         Gradle project: Kotlin shell (app/), vendored terminal libraries (terminal-emulator/, terminal-view/)
docs/            design docs (design/, overview in README.md, one file per section), per-round records (rounds/, indexed by milestones.md)
```

Build: a Cargo workspace; the engine is compiled through the `cc` crate, and the C code builds with `-Wall -Wextra -Werror` under both gcc and clang. The Rust toolchain is pinned by `rust-toolchain.toml`. The app shell is built with Gradle and the native library with cargo-ndk.

## 13.3 Development environment and testing

- The main development and test host is a **Linux arm64 VM** on a Mac (colima, OrbStack or Lima); 90% of the work never touches a phone. `scripts/dev <command>` runs a command in an arm64 Linux container, for example `scripts/dev cargo test --workspace`; when the Docker daemon has a proxy configured, the container picks it up.
- **CI** (`.github/workflows`) has three workflows, so that frequent pushes wait only for the quick checks:
  - **CI** (every push that changes more than documentation; GitHub's `ubuntu-24.04-arm` runner): format check, clippy and `cargo test --workspace` (including the three-way lockstep, the whitelist check against `llvm-mc`, the guest program and rootfs cases, and comparing benchmark output under both execution tiers).
  - **Android** (pushes that change the core, the Android host or app, or their build inputs; x86_64 runner): clippy for the Android target, `assembleDebug` and lint; the APK is uploaded as an artifact.
  - **Full tests** (nightly on develop when it changed, on pushes to master, and on demand): the apk + git cases that need the internet, an extended lockstep run, the differential tests (with Fuchsia Starnix's and gVisor's syscall tests) and the engine tests built with clang.
- **Performance probe**: after `cargo build --release -p tuxport-run -p tuxport-bench`, run `tuxport-bench`; it runs the programs under `tests/bench/programs/` natively, under Tier 0 and under Tier 1, requires identical output, and prints the time ratios.
- **Debugging**: `TUXPORT_LOG=1` prints the local debug log; `TUXPORT_STRACE=1` prints each syscall's arguments, result, timestamp and duration; `TUXPORT_PERF_MAP=1` makes Tier 1 write `/tmp/perf-<pid>.map` for perf; `tuxport-run --stats` prints translation statistics. In debug builds a bug in the kernel layer or the engine aborts the host process by default, and `TUXPORT_ON_BUG=kill` makes it end only the guest process that hit it (§12 item 5). With `TUXPORT_TEST_HOOKS=1`, a syscall outside Linux's numbering (`0xa991`) can simulate various kernel bugs for end-to-end tests (the debug build of the Android app includes it through the `test-hooks` feature). `scripts/difftest`, `scripts/ltp` and `scripts/survey` all watch `tuxport-run`'s stderr: when the kernel layer reports an internal error, the differential test fails, LTP records `BUG`, and the survey flags how many there were.
- Testing has five layers:
  1. **Instruction-level lockstep**: the host CPU is the reference. Random instructions with random register values run natively and in the interpreter/DBT, and the results are compared instruction by instruction. This doubles as fuzzing of the decoder. The guest addresses under test equal the host addresses on the native side, so it cannot catch an access that uses the wrong addend; such errors are left to the differential tests and real programs.
  2. **LTP syscall tests**: `scripts/build-ltp` builds them natively in an `alpine` container; `scripts/ltp` starts a fresh `tuxport-run` for each test, 8 in parallel, locally rather than in CI. The tests that took longest last time go first; `LTP_TIMEOUT_MUL=5`, `LTP_RUNTIME_MUL=0.1` (race tests that run for a fixed duration run a tenth of it). The store sits on a docker volume local to the VM: over virtiofs, eight parallel SQLite writers time out waiting for locks. sigtimedwait01 and sigwaitinfo01 are skipped by default (musl's sigtimedwait retries on EINTR by itself, so they run into the timeout on native Alpine too). Known flaky tests are listed in [milestones.md](../milestones.md#known-flaky-tests). musl libc-test is not wired in.
  3. **End-to-end scenarios**: the busybox, interactive shell, `apk add`, `git clone` and other cases in `hosts/cli/tests/rootfs_programs.rs`. `scripts/survey` runs common services and tools in Tuxport (`tests/survey/`), glibc programs in Debian (`glibc/`), language test suites and large projects (`suites/`), and AI coding CLIs (`ai/`, with a mock server standing in for the model API); `-n` runs the same workloads in a native image exported from the store, for comparison. Its output needs a human to read and installing packages needs the internet, so it is not in CI.
  4. **Differential tests**: the same command runs in Tuxport and on real arm64 Linux and the outputs are compared; signal tests compare the contents of `ucontext`. `scripts/difftest` first packs the test store's root file system inside Tuxport and imports it as a docker image, then hands each script under `tests/diff/` to both a docker container and `tuxport-run`, comparing output and exit code (a script may not exceed 128 KB). `tuxport-run` also runs in a container, so both sides see the same kind of network, which is what makes route, interface and address cases comparable; cases leave out whatever changes with the kernel version. On macOS 4 groups run at once by default, each parallel lane on its own APFS clone of the store, and the groups that took longest last time start first.
  5. **Android-specific**: instrumentation tests (`MachineTest`) on an arm64 Android emulator, covering 4K and 16K pages, including simulating kernel-layer and engine bugs in the app process through the test hooks. GitHub's runners have no KVM, so this layer runs locally; CI only builds the APK and runs lint.

## 13.4 License

Tuxport is released under **AGPL-3.0-or-later**. The reasons: its network clause covers someone running a modified core as an online service (a code-execution sandbox, for example), and it is compatible with everything the app links or bundles (permissive code, including the Apache-2.0 AndroidX libraries and Kotlin standard library that GPLv2 could not be combined with).

- **Dependencies**: the Rust crates all have permissive licenses and all offer MIT; the terminal emulator, Ghostty's libghostty-vt, and what it links (uucode, simdutf, Highway, Wuffs, translate-c, Zig's compiler_rt) are MIT, BSD-3-Clause and Unicode-3.0 (where a choice is offered, MIT or BSD); the JetBrains Mono font is OFL-1.1, which allows bundling with software under any license; the Android libraries are all Apache-2.0; SQLite is in the public domain; parts of the terminal line discipline in `core/kernel/src/tty.rs` are adapted from Fuchsia's Starnix under its BSD-style license, whose notice they keep. All are compatible with AGPLv3.
- **Notices**: "Open source licenses" in settings lists every third-party component in the app with the full license texts, generated at build time (`NoticesTask`, see `android/README.md`): for Rust crates it finds the ones linked into `libtuxport.so` from `cargo metadata` and includes each one's published license files verbatim; for Android libraries it takes the runtime dependency graph and fails the build on any group not known to be Apache-2.0. That row in settings and `tuxport-run --version` show the Appropriate Legal Notices that AGPL section 5 requires.
- **The Alpine minirootfs in the APK**: it is merely aggregated with Tuxport and not affected by Tuxport's license; but busybox, apk-tools, alpine-baselayout and scanelf are GPL-2.0-only and part of musl-utils is GPL-2.0-or-later, so their source must be offered when the APK ships. The notices list each package's aports commit. The Debian that settings can download is fetched by the user from the sources of the official Docker image and is not distributed by Tuxport.
- **Source**: GPLv3 and AGPLv3 both require the complete corresponding source (including statically linked crates) to accompany the APK. The repository is private for now and must be made public before release (or a written offer of source must be included). `scripts/release-sources` produces the archives released alongside it: Tuxport itself (`git archive HEAD`, refused when the working tree has changes), `cargo vendor`, the libghostty-vt source release and its dependency tarball of the Zig packages its build uses, both as published, and the GPL packages of the Alpine minirootfs (the aports directories are fetched from GitLab by the origin and aports commit in the installed database, and the upstream sources from Alpine's distfiles mirror, checked against sha512sums; no APKBUILD is run).
