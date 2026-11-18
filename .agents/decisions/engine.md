# CPU engine (engine) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: engine"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b). D1–D18 were seeded by hand from the retired design
> doc §4 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: The guest ISA is aarch64, the same as the host
- **What**: Tuxport runs Alpine's official aarch64 binaries and translates aarch64 to aarch64.
- **Why**: Guest and host share ARM's weak memory model (no TSO translation), most instructions can be copied verbatim, and arm64 requires `ic ivau` plus `isb` after modifying code, which gives a free invalidation hook without write-protecting code pages.
- **Tradeoffs**: Only aarch64 guests, and the engine's fast paths only work on aarch64 hosts.
- **Watch out**: Relying on `ic ivau` requires CTR_EL0 to report DIC = IDC = 0.
- **Source**: design review 2026-10-24 (§4.1)

### D2: Two tiers, with Tier 0 as the semantic reference
- **What**: An interpreter handles cold code, single-stepping and anything Tier 1 does not translate; Tier 1 copies or translates only encodings Tier 0 accepts.
- **Why**: Both tiers must give identical results for every instruction, and the interpreter is the simplest place to define that behaviour.
- **Tradeoffs**: Every new instruction or extension must be implemented in Tier 0 first.
- **Watch out**: The whitelist must be shared; Tier 1 asks Tier 0 (or its SIMD decoder) about each encoding.
- **Source**: design review 2026-10-24, rounds 01 (`01-m2`), 02 (`02-m1a`), 09 (`09-m7`)

### D3: Tier 0 uses nested-switch decoding, not threaded dispatch
- **What**: Tier 0 decodes each instruction with mask tests and `switch` on encoding fields.
- **Why**: With Tier 1 in place, Tier 0's speed barely affects overall performance.
- **Tradeoffs**: Tier 0 alone is very slow (relevant if Tier 1 is unavailable on a host).
- **Watch out**: Workloads stuck in Tier 0 (breakpoints, watchpoints, poisoned instances) run much slower.
- **Source**: design review 2026-10-24 (§4.2)

### D4: SIMD/FP, CRC32 and LSE run in Tier 0 as native single-instruction stubs
- **What**: C does the whitelist check, address translation and register moves; a cached host stub executes the instruction, specializing integer ADD/SUB and logical vector operations by which operands they read.
- **Why**: Results are bit-identical to hardware (NaN propagation, rounding, FZ/DN, FPSR flags) without reimplementing floating point.
- **Tradeoffs**: Tier 0 is no longer independent of the hardware for SIMD/FP, needs executable memory (Android allows JIT), and only works on aarch64 Linux hosts.
- **Watch out**: Specialized stubs must preserve flags, aliased operands, old destinations when read, and upper-lane clearing. Without native stubs no SIMD/FP (or other implemented) feature can be advertised.
- **Source**: rounds 02 (`02-m1a`), 09 (`09-m7`); 16914003

### D5: Exclusives are emulated as value compare-and-swap in both tiers
- **What**: LDXR records address, size and value; STXR does a compare-and-swap against the recorded value on the host address.
- **Why**: Extra memory accesses (TLB lookups) between `ldxr` and `stxr` can clear the host's exclusive monitor and livelock, and recognizing whole LL/SC sequences is unnecessary; musl hard-codes `ldaxr`/`stlxr`, so LL/SC must work even when LSE is advertised.
- **Tradeoffs**: ABA is allowed (as in QEMU), harmless for correct LL/SC use.
- **Watch out**: Tier 1's `ldaxr`/`stlxr` loop must keep only register operations between the two instructions.
- **Source**: rounds 01 (`01-m2`), 02 (`02-m1a`), 09 (`09-m7`); risk R4

### D6: Translated code never touches host x18 and never changes NZCV
- **What**: Guest X18 is always kept in the CPU state, and inserted code uses `cbnz`/`tbnz`/`eor` instead of compares.
- **Why**: x18 is Android's platform register (ShadowCallStack) and host signal handlers may run at any time; guest NZCV lives in the host's flags.
- **Tradeoffs**: One more stolen register; inserted sequences are slightly constrained.
- **Watch out**: Any new emitted sequence must respect both rules.
- **Source**: round 09 (`09-m7`); risk R6

### D7: Guest SP lives in host X26, and calls use host return prediction
- **What**: The default register set keeps guest SP in X26; `bl`/`blr` become host calls with a shadow return stack and `ret x30` returns with a host `ret`.
- **Why**: An SP-based `stp x29, x30, [sp, #-32]!` cost about 14 host instructions and every return went through a jump-cache lookup with millions of misses; SP appears in about one instruction in six and X26 is the least-used callee-saved register in compiled code.
- **Tradeoffs**: Guest X26 becomes stolen; mismatched returns (longjmp, signals, coroutines) fall back to the jump cache.
- **Watch out**: The shadow stack is reset whenever the instance is flushed.
- **Source**: round 15 (`15-sp-return-prediction`)

### D8: Inline TLB check indexes by the last byte and compares with the first
- **What**: The TLB slot is chosen by the page of the access's last byte and its tag compared with the first byte's page.
- **Why**: One compare rejects both misses and cross-page accesses, and TLB checks dominated translated code.
- **Tradeoffs**: Cross-page accesses always take the out-of-line refill routine.
- **Watch out**: Tier 1 depends on the TLB layout owned by `kernel/mmu`, whose D3 covers tagged data pointers and cached write permissions.
- **Source**: rounds 09 (`09-m7`), 23 (`23-tlb-checks`), 30 (`30-translated-access`); 8bc0e63b

### D9: Blocks continue past conditional branches, but not across calls or jumps
- **What**: A conditional branch becomes a side exit linked directly to its target block, and translation continues on the fall-through path; blocks still end at unconditional branches and calls.
- **Why**: Ending at conditional branches produced far more blocks (500k versus 140k in the M7 measurement); continuing across calls and forward jumps (superblocks) gained at most about 2% and needed every return point to be invalidatable, so it was not adopted.
- **Tradeoffs**: Conditional side exits are patched by rewriting the branch itself.
- **Watch out**: Conditional branches are not safe to patch concurrently (CMODX), so they are relinked only when one thread is attached.
- **Source**: rounds 09 (`09-m7`), 31 (`31-superblocks`)

### D10: Same-base accesses in a block share one TLB check
- **What**: A group leader checks the whole range (up to 512 bytes) and later members use the host base directly.
- **Why**: Python's interpreter loop spent most of its translated time in TLB checks.
- **Tradeoffs**: A failing leader must fall back to Tier 0; repeatedly failing leaders are ungrouped.
- **Watch out**: Members after conditional branches may not execute, so the leader's range may include pages they never touch.
- **Source**: round 27 (`27-interleaved-checks`)

### D11: PAC hints are NOPs in both tiers, and PAC is not advertised
- **What**: HINT-space PAC and BTI instructions do nothing; PACA/PACG are never advertised.
- **Why**: Executing PAC natively in Tier 1 would sign with host keys while Tier 0 can only treat it as a NOP; the tiers must agree.
- **Tradeoffs**: Guests get no pointer-authentication protection.
- **Watch out**: Non-hint PAC instructions are undefined (`SIGILL`).
- **Source**: design review 2026-10-24 (§4.4)

### D12: Guest features are host ∩ implemented ∩ whitelist, checked against LLVM
- **What**: One computed feature set drives `AT_HWCAP`, `/proc/cpuinfo` and ID-register reads, and the whitelist is sampled against `llvm-mc` decoding.
- **Why**: Hardware can only tell whether an encoding executes, not whether it belongs to an unadvertised extension.
- **Tradeoffs**: Extensions are added one at a time; LSE2 and SM3/SM4 are not done (the development machine lacks SM3/SM4, so they cannot be tested).
- **Watch out**: The `llvm-mc` check runs only on Linux aarch64 and is skipped when `llvm-mc` is missing.
- **Source**: rounds 02 (`02-m1a`), 133 (`133-asimddp`) to 155 (`155-dit`)

### D13: Threads of an address space share one Tier 1 instance with lock-free lookups
- **What**: One code cache and translation table per address space; lookups and translation run without the lock, and only installation is locked.
- **Why**: Multithreaded workloads (JVM, `go build`) paid for per-thread translations and contended on a lock.
- **Tradeoffs**: Patching must follow CMODX rules (`b`→`b` only while others run), and flushes must wait for every thread to leave translated code.
- **Watch out**: Table writers may only fill empty slots and write tombstones.
- **Source**: round 24 (`24-shared-translation`), 72 (`72-table-tombstones`)

### D14: Translations are shared through templates and kept after an address space dies
- **What**: Every block is stored in a process-global template arena, and dropped instances return to a pool keyed by program, re-checked (or parked) on reuse.
- **Why**: Each new process re-copied and re-linked common code (600 spawns translated 690k blocks, 99.5% from templates).
- **Tradeoffs**: Memory for the arena and pooled instances; reuse needs content checks or trusted sources.
- **Watch out**: A trusted source must really identify its content (version = host object ctime and size).
- **Source**: rounds 09 (`09-m7`), 14 (`14-spawn-translation`)

### D15: Processes of the same program share one instance (code groups)
- **What**: Address spaces running the same executable share one instance; blocks in consistent, trusted ranges are shared, others are private to the member that translated them.
- **Why**: Each process had its own translation cache (four parallel `cc1` used 34–40 MB each); Tuxport has no ASLR, so the same program maps at the same addresses.
- **Tradeoffs**: Shared blocks never link to private ones; members with diverging layouts leave the group.
- **Watch out**: The engine trusts the kernel to share only ranges every member maps identically.
- **Source**: round 108 (`108-shared-translation`); see `kernel/mm` D13

### D16: Go programs use a different stolen register set; V8 and HotSpot keep the default
- **What**: Programs with a Go build-ID note use X21–X25 (plus X18) as stolen registers; others use X16, X17, X26–X28 (plus X18).
- **Why**: Go's compiler fixes x28, x27, x26 and x16 for its own use (`go build` 3–5% faster), while V8 and HotSpot only read their fixed registers, so keeping them stolen costs little (switching gained V8 only 2–4%).
- **Tradeoffs**: Routines and templates exist per register set.
- **Watch out**: `TUXPORT_DBT_REGS` overrides the choice for experiments.
- **Source**: rounds 113 (`113-go-registers`), 124 (`124-v8-hotspot-registers`)

### D17: Code memory is W^X through double mapping, and the written view is dropped periodically
- **What**: The code cache and native stubs are one shared anonymous region mapped twice (RW and RX) with `mremap`; pages written through the RW view lose their host page-table entries every second.
- **Why**: W^X without depending on memfd, and the host counts pages touched through both views twice in RSS.
- **Tradeoffs**: Falls back to RWX where the host refuses; the next write to a dropped page faults it back in.
- **Watch out**: The OOM measurement must subtract the not-yet-dropped bytes.
- **Source**: rounds 09 (`09-m7`), 106 (`106-code-cache-rss`)

### D18: Invalidation is by content, and retired blocks forward to their replacement
- **What**: Only blocks whose guest words changed are retired; a retired block's entry jumps to a forwarding stub, and re-translation patches the old entry to the new block, while small-range invalidation unlinks dead page-bucket records and keeps parked blocks available for revival.
- **Why**: A JIT rewriting one spot should not invalidate other methods' blocks on the same page, and forwarding avoids rewriting every link into the old entry.
- **Tradeoffs**: Retired code is reclaimed only by a full flush.
- **Watch out**: Jump-cache entries are not cleared and rely on forwarders.
- **Source**: rounds 11 (`11-m8-jvm`), 24 (`24-shared-translation`), 35 (`35-jit`); 16914003

### D19: The engine module is `core/engine` without the page table and TLB fill
- **What**: engine covers instruction execution, translation, CPU features, the fault guard, C/Rust bindings and `tests/lockstep`, while page tables and TLB fill/flush belong to kernel/mmu; the translator points readers to `.agents/maps/engine.md`.
- **Why**: The retired design doc's §4 was the only current-state description of the CPU engine and had drifted from the source (precise state, indirect-branch lookup, flush triggers, feature gating).
- **Tradeoffs**: D1–D18 cite rounds rather than commits, and rounds keep § references to the deleted section; host-vDSO CPU-feature dependencies belong to kernel/vdso.
- **Watch out**: Bugs confirmed at the split stay open in the map, among them Tier 0 losing pending `ic ivau` on guest page 0 (the Tier 1 dispatcher now handles it directly), conditional-branch relinking racing a new thread and unallocated ID registers reading 0 where Linux delivers `SIGILL`.
- **Source**: f9edf81 (bug list since updated by e5922b1, 6d1d9c0 and b43ff37); 44a21b (MMU split); 36083476; 20137e4c; d053be24; 60790c14; 39e55b4b; 8bc0e63b

### D20: Engine files read in the order their code runs
- **What**: lib.rs is sectioned into the page table, CPU state and exits, Tier 1 and process-wide calls (`Cpu::reset` after `tlb_flush`, as at execve); in tests a helper used by one test follows it and shared helpers follow all tests; lockstep's class functions follow the `CLASSES` table and its runner goes from its types through `run` and `run_class` to the helpers in call order.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; the engine tests defined their helpers before the tests that use them.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; in lockstep's classes.rs, `imm_width`, `single_reg` and `pair` sit after their last user, further from their first.
- **Watch out**: Code appended at the end of a file drifts from this order.
- **Source**: 91fabe2

### D21: A failed internal check ends only the guest process
- **What**: `tuxport_guard_fail` unwinds through the armed fault guard like a host fault, recording `SIGABRT` and the caller's address and releasing registered locks, and aborts only when no guard is armed.
- **Why**: An engine bug caught by a check is no worse than one caught by a host fault, yet it killed the whole app instead of one guest process.
- **Tradeoffs**: The guest process ends instead of the block falling back to Tier 0, and debug builds, which install no guard, still abort as tests want.
- **Watch out**: A lock taken under the guard but never registered (D22), or nested more than four deep, stays held.
- **Source**: e5922b1, b43ff37

### D22: Locks taken under the fault guard are registered, counting depth past four
- **What**: `tuxport_guard_hold_fn` lets one recorded entry release a set of locks, a depth count pairs each drop with its hold while recovery releases the four outermost recorded entries, and `template_alloc`, `template_clear` (64 shard locks) and the perf map register their locks.
- **Why**: The template arena lock, `template_clear`'s shard locks and the perf-map lock were not released after a host fault, and a fifth nested hold went unrecorded while its drop still decremented.
- **Tradeoffs**: Locks nested deeper than four are still not released after a fault.
- **Watch out**: Code that takes a lock under the guard must register it; the selftest covers nesting past four and release functions.
- **Source**: b43ff37

### D23: The generated table lookup sends pc 0 to the miss path
- **What**: `emit_lookup` branches to the miss path for pc 0 with one `cbz` on the jump-cache miss path.
- **Why**: The lookup's unordered `ldp` could pair newly installed code with an empty slot's key 0 and enter the wrong block for target 0.
- **Tradeoffs**: One more instruction per table lookup from translated code.
- **Watch out**: None known.
- **Source**: b43ff37

### D24: `CNTVCTSS_EL0` is read like `CNTVCT_EL0`, and ECV is advertised only where the host has it
- **What**: `TUXPORT_HWCAP2_ECV` joins the host-gated integer features (D12), Tier 0 reads `CNTVCTSS_EL0` (S3_3_C14_C0_6) without an `isb` and Tier 1 copies it as it does `CNTVCT_EL0`, so a host vDSO patched for ECV runs in guests (kernel/signal D4).
- **Why**: Since Linux 5.16 the host kernel patches its vDSO to read `CNTVCTSS_EL0` on CPUs with ECV (Armv8.6 on, as in recent phones), and guests now run a copy of the host's vDSO.
- **Tradeoffs**: Guests on ECV hosts see `ecv` in `HWCAP2` and `/proc/cpuinfo`.
- **Watch out**: The path is untested on ECV hardware, since colima and both emulators lack ECV.
- **Source**: 16364c0

### D25: Translated FPCR and FPSR reads use different sources

- **What**: Tier 1 reads the saved guest FPCR and the live FPSR through register remapping, and keeps FPCR writes in the block when the masked value is unchanged.
- **Why**: FPCR stays constant during a translated run, while instructions in that run can raise FPSR flags before the saved copy is refreshed.
- **Tradeoffs**: A changed FPCR or any FPSR write still exits to Tier 0 for masking.
- **Watch out**: Keep both FPCR masks identical and preserve NZCV, newly raised FPSR flags and reads targeting a stolen register.
- **Source**: 16914003, 26ceb37c, 8bc0e63b

### D26: Tier 0 reuses fetch translations and skips work with no architectural effect

- **What**: Within a run, fetches on the same executable page reuse its offset, arithmetic skips unused NZCV calculations and contiguous vector transfers copy by register after probing the full range.
- **Why**: Repeated page probes, flag calculations and per-lane copies slowed common interpreter loops.
- **Tradeoffs**: Two arithmetic branches preserve Clang performance, and interleaved vector transfers still use the general loop.
- **Watch out**: Check kicks at every instruction, discard the offset on exit, and preserve breakpoint priority, SP/ZR semantics and faults before writeback.
- **Source**: 7bc780ca

### D27: Translated barriers finish pending code invalidation before the next lookup

- **What**: Tier 1 executes DSB and ISB in the block and sends pending invalidation to the dispatcher before looking up the following PC.
- **Why**: Interpreting barriers after guest code changes repeatedly interrupted translated execution.
- **Tradeoffs**: A pending page still requires a state transfer and a page check.
- **Watch out**: Page zero needs its own path because zero is the empty pending-page sentinel.
- **Source**: 8bc0e63b

## Superseded

- ~~Every FPCR write exits to Tier 0 in 16914003~~ → narrowed by **D25** in 8bc0e63b (2026-11-17).
