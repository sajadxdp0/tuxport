# 14. Milestones

## 14.1 Plan

Listed in execution order. The numbers were kept, so M2 comes before M1a.

| Milestone | Status | Content | Exit criteria |
|---|---|---|---|
| **M0 skeleton** | ✅ | Repository layout, engine and kernel-layer projects, `tuxport-run`, CI, the lockstep test framework | A hand-written static aarch64 program (`write` + `exit`) runs in the interpreter; lockstep tests are in CI |
| **M2 performance probe** | ✅ | Prototype Tier 1: integer instructions + memory access + branches + inline TLB + block chaining; separately compiled integer benchmarks (no libc, no SIMD) | Measure the native, Tier 0 and Tier 1 time ratios and **re-check the 3–5× target against the decision rule**; after M1, measure again with busybox `gzip`/`sha256sum` |
| **M1a instruction set completion** | ✅ | SIMD&FP, LL/SC exclusive access, HWCAPs advertised as in the [engine map](../../.agents/maps/engine.md); lockstep extended to these instructions | lockstep covers SIMD&FP and exclusive access and passes in CI |
| **M1b loading programs** | ✅ | ELF loader (PIE, `ld-musl`), full auxv, soft-MMU v1 (mmap/brk/mprotect), basic syscalls, inode store v1 + minirootfs import | `tuxport-run /bin/busybox ls /` works (through `ld-musl`) |
| **M3a multiple processes** | ✅ | One host pthread per guest thread, the BKL, a unified wait mechanism; fork/vfork/threads, execve, wait4; signal frames, sigaltstack, vDSO; pipes, futex; basic device nodes; syscalls that change the file namespace | Pipes, command substitution, trap, `find -exec` and background jobs under `sh -c`, plus a multithreaded test program, pass in CI under both execution tiers |
| **M3b interactive shell** | ✅ | In-process PTY + line discipline, job control with sessions and process groups (stop and continue), `tuxport-run` taking over the host terminal, devpts and procfs | Pipes, Ctrl-C, Ctrl-Z/`fg` and `vi` all work under `ash` |
| **M4 networking + apk** | ✅ | Socket passthrough, DNS stub, netlink emulation, hard links, AES/SHA instructions | `apk update && apk add git python3` succeeds; `git clone https://…` succeeds |
| **M5 Android shell** | ✅ not submitted to Play, not tried on a real device | Kotlin app, vendored terminal libraries, `specialUse` foreground service, first-launch flow | Usable right after install on a real device; passes on both 4K and 16K page devices; **submitted to a Play internal or closed testing track** |
| **M6 class B workloads** | ✅ | Wider syscall coverage, shared file mmap, an LTP subset, `./configure` runs | Lua or SQLite builds from source in Tuxport; a subset of the python3 test suite passes |
| **M7 production Tier 1** | ✅ | Block chaining, a translation cache shared across processes, precise exceptions, LL/SC handling | Meets the performance target; lockstep and the differential tests all pass under Tier 1 |
| **M8 class C correctness** | ✅ | Go (SIGURG preemption, nil dereference turned into a panic), node (guest JIT, `ic ivau`, wasm trap handler); the JVM tested afterwards | A Go CLI runs, `node -e` works, `npm install` of a small package succeeds |
| **M9 release** | In progress: the app work is done; real-device validation, the pre-release license obligations (public repository or a written offer, undecided) and the Play submission remain | Settings page, restoring tabs after being killed, wake lock toggle, license audit, logging in as a chosen user, notices when a process is ended, DocumentsProvider | Production release on Play |

Between M8 and M9 came more than 170 rounds of hardening: stability, compatibility, deferred kernel features, robustness, ptrace, namespaces and containers, performance; see [milestones.md](../milestones.md) for each round.

## 14.2 Why this order

- **Test the shakiest premise first**: M2 checks the performance of soft-MMU + DBT with measured data. M0 already provided a static ELF loader and the soft-MMU, so M2 did not need to wait for M1 and moved ahead of it, rather than finding out that the memory model needed rework after M1 had invested heavily in it.
- **M2's decision rule** (fixed in advance): Tier 1 at ≤ 3× native, continue as planned; at 3–5×, continue and note directions for optimization; still > 5× after the obvious optimizations such as block chaining and the inline TLB, revisit decision 4 (the soft-MMU, see the [kernel/mm map](../../.agents/maps/kernel/mm.md)) before starting M1.
- **M1 is split in two**: M1a completes the interpreter's instruction set (the largest piece of work, and fully verifiable by lockstep); M1b opens up the loading and syscall paths.
- **M3 is split in two as well**: M3a makes the kernel layer multiprocess and multithreaded, which `sh -c` and test programs can verify automatically in CI; M3b then does the parts that depend on terminal interaction. Threads and fork share the "one host thread per task, BKL, futex" machinery, which M4's `git` also needs, so it goes into M3a.
- **Run into the risks that only show up on real devices or on Play early**: they are in M5, not at the end.
- **Workloads before the DBT**: production Tier 1 comes after the class B workloads, so there are real workloads to measure. Class C comes last because it depends on both precise signals and a mature DBT.

## 14.3 Records

Each round's conclusions, measurements, implementation notes and deferred items get one file in [rounds/](../rounds/) (written in Chinese). [milestones.md](../milestones.md) is the index by round; it also records the measurement conditions the rounds share, the known flaky tests and the history of R1 measurements.
