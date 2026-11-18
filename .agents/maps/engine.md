# engine Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/engine.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 6 research concerns; 23 claims checked by 3 independent verifiers — 14 confirmed and 9 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-17 (targeted verification only since the full mapping; latest change: FPCR writes, code invalidation at barriers and tagged data TLB entries)

## Responsibilities

`engine` executes guest aarch64 user code inside the app process. Downloaded code may only run
under an interpreter or translator, so the engine never jumps to guest code directly:

- **Tier 0** (`interp.c`, `simd.c`, `atomic.c`, `native.c`, `cpu.c`) interprets one instruction at
  a time. It handles cold code, single-stepping and hardware breakpoints, executes every instruction
  Tier 1 hands back, and is the semantic reference (Tier 1 asks it whether an encoding is defined).
- **Tier 1** (`dbt.c`, `emit.h`) is a same-ISA dynamic binary translator: most instructions are
  copied verbatim, memory accesses get an inline soft-TLB check, and blocks are linked in a shared
  code cache.
- The public contract (`include/tuxport/engine.h`, Rust bindings in `src/lib.rs`): the per-thread
  CPU state, exit reasons, the kick handle, hardware debug slots, the Tier 1 instance API.
- CPU feature advertisement and ID-register emulation (`features.c`).
- The host fault guard (`guard.c`), which turns a host fault inside engine code into an exit.

Outside the module: the radix page table (`mmu.c`), the TLB entry layout and Tier 0 TLB fill/flush,
and the mm-side code-group policy belong to `kernel/mmu`; the kernel layer decides which tier runs
and what each exit means to the guest. Build: `core/engine/build.rs` compiles the C files with the
`cc` crate (`-std=c17`, warnings as errors); non-aarch64 hosts get stubs (no native stubs, no
Tier 1, only CPUID advertised).

## Key types

### CPU state and contract

- `struct tuxport_cpu` / `CpuState` — one per guest thread: X0–X30, SP, PC, NZCV, FPCR, FPSR,
  TPIDR_EL0, exit and fault information, the `pending` kick flag, the bound page table, the
  exclusive monitor (`excl_addr`, `excl_value`, `excl_size`; size 0 means none), Tier 1 scratch and
  group slots (`dbt_groups[4]`), `ivau_page`, the data TLB (4096) and ITLB (512), V0–V31,
  `tlb_used`, DIT, and 16 hardware breakpoint and 16 watchpoint slots. Fields before the TLB are
  addressed by Tier 1 with immediate offsets.
- `Cpu` — owns one state (pooled: up to 8 spares); `Send`, not `Sync`.
- `Kicker` — raw handle whose only operation, `kick`, sets `pending`; the one thread-safe call.
- `Exit` — `Svc{imm}`, `Undefined{insn}`, `Breakpoint{imm}`, `Fault{addr, access, kind}`, `Kick`,
  `InvalidateCode{addr}`, `HostFault{signal, code, addr, pc}`. `FaultKind`: page, alignment,
  stack alignment, watchpoint, hardware breakpoint. `Access`: read, write, exec.

### Tier 1

- `DbtSpace` (`struct tuxport_dbt`) — one translation instance: 128 MB code cache with the shared
  routines at its start, the lock-free translation table, block records, saved guest words, per-page
  block buckets, the shared-range list, the register set, the lock, flush state, trust sources, and
  the poisoned flag. Shared by every thread of an address space and by every member of a code group.
- `Dbt` (`struct tuxport_dbt_thread`) — one thread's attachment: its tag, 4096-entry jump cache,
  64-entry shadow return stack, statistics, and a private translation workspace.
- `struct block` — guest pc, entry in the code cache (NULL when dead), saved guest words, parked
  flag, tag.
- `struct table` / `struct slot` — open-addressed table keyed by `pc | tag << 48`, hashed on pc only;
  removed entries become tombstones.
- `DbtRegs` / `struct regs` — the host register assignment: `Default` (context x28, temporaries
  x16/x17/x27, guest SP in x26) or `Go` (context x25, temporaries x21/x22/x23, SP in x24).
- `DbtSource` — a trusted code range: start, length and an identity (device, inode, offset, ctime,
  size; all-ones for the vDSO).
- Templates — a process-global 32 MB arena of translated blocks shared by all instances.
- The instance pool — up to 16 retired instances (64 MB of kept code) and 8 spare thread parts.

## Public entry points

- **CPU**: `Cpu::new`, `reset`, `run` (guarded loop until an exit), `step` (one instruction, ignores
  kicks), `exec_insn` (run a given word; tests and lockstep only), `translate`, `tlb_flush`,
  `state`/`state_mut`, `kicker`; `CpuState::set_hw_debug`, `hw_breakpoints_active`.
- **Tier 1**: `DbtSpace::new(key, regs)`, `set_sources`, `trust`, `retire_outside`, `share`,
  `new_tag`; `Dbt::new`, `with_regs`, `attach(space, tag)`, `run(cpu)`, `flush`, `recheck`,
  `invalidate(cpu, start, len)`, `stats`; `trim_dbt_pool`, `unmap_written_code`,
  `written_code_bytes`.
- **Process-wide**: `install_fault_guard`, `guest_hwcap`, `guest_hwcap2`, `guest_midr`,
  `layout_matches`.
- **C-internal** (used across engine files): `tuxport_interp_step`/`tuxport_interp_exec`,
  `tuxport_native_exec`, `tuxport_simd_dp_decode`, `tuxport_atomic_exec`, `tuxport_id_register`,
  `tuxport_guard_run`/`hold`/`drop`/`fail`, `tuxport_map_code`; `tuxport_guard_selftest` for the unit
  test.

## Data flow / lifecycle

### Tier selection (decided by the kernel run loop)

- Tier 1 runs when the address space has a `DbtSpace`; Tier 0 (`Cpu::run`) runs when there is no
  instance, when hardware breakpoints are set, and when the instance is poisoned; `Cpu::step` is
  used for single-stepping.
- Tier 1 itself executes any instruction it does not translate by calling Tier 0 for that one
  instruction.

### CPU state and exits

- `Cpu::new` takes a pooled state or maps a new one and zeroes everything before the TLB, the
  V registers and the debug slots, flushing only used TLB entries.
- `run` syncs host PSTATE.DIT with the guest's, arms the fault guard, and loops; each iteration
  consumes `pending` (a kick ends the run with `Kick`) and executes one instruction.
- The C exit code is decoded from the state's exit and fault fields; `HostFault` reads the
  thread's last recorded host fault. Exit code 0 or an unknown code panics.
- The kernel collects `ivau_page` on every exit and turns `Undefined` into `SIGILL`/`ILL_ILLOPC`.

### Tier 0 execution

- Step order: PC alignment (alignment fault), hardware breakpoint (before executing), instruction
  fetch through the ITLB (exec page fault), then decode. During a run, an aligned PC with no
  hardware breakpoints can reuse the last code page's address offset. The run loop still checks
  for a kick before fetching each instruction word from guest memory. Every exit discards the
  offset so the next run observes any intervening TLB flush.
- Decoding is hand-written mask tests and `switch` on encoding fields, dispatching first on bits
  28:25 (data processing immediate, branches and system, data processing register, loads and
  stores, SIMD and FP data processing); everything else is undefined.
- ADD/SUB immediate and shifted or extended register forms compute NZCV only when the S bit is
  set. With S clear, immediate and extended forms can write SP and shifted forms discard a write
  to register 31.
- A failing or undefined instruction leaves PC and registers untouched; only `svc` and `ic ivau`
  exits advance PC.
- Data addresses have the top byte ignored (TBI); PC and branch targets are not untagged.
- Memory accesses check every page touched before copying anything, so a fault has no side
  effects; SP-based accesses require 16-byte SP alignment; watchpoints are checked on every access.
- Ordinary single-register integer loads and stores use fixed-width copies on same-page data TLB hits
  when the thread has no watchpoints. Misses, cross-page accesses and watched threads use the
  shared memory helpers.
- System registers: NZCV, FPCR (writes masked to AHP, DN, FZ, RMode and, with FP16, FZ16), FPSR
  (cumulative flags only) and TPIDR_EL0 are read-write; TPIDRRO_EL0 reads 0; DCZID_EL0 reads 4
  (64-byte blocks); CTR_EL0 reads `0x8444c004` (DIC = IDC = 0, so guests must issue `ic ivau`);
  CNTVCT and CNTFRQ read the host, and so does CNTVCTSS (no `isb`) when ECV is offered; DIT is
  feature-gated; the ID space comes from the feature set; anything else is undefined.
- HINT space (NOP, YIELD, `PACIASP`/`AUTIASP`, BTI) is a no-op; non-hint PAC (PACGA, one-source
  PAC, BRAA/BLRAA/RETAA) and HLT are undefined. DMB and DSB are full fences; ISB is a no-op.
- `dc zva` zeroes 64 bytes and needs write permission; other DC operations and `ic ivau` only check
  read permission; `ic ivau` always exits with `InvalidateCode` in Tier 0. PRFM touches nothing.

### Native stubs and FP state

- SIMD and FP data processing, CRC32 and the LSE atomics run as cached host "native stubs": C
  decides whether the encoding is whitelisted, translates the address and moves registers, and a
  generated stub executes the same instruction on the host with general-register fields remapped to
  host temporaries (address fields replaced by a translated host pointer).
- The stub cache is one process-global 2 MB code region (W^X double mapping) with a 4096-slot
  table; it is flushed entirely when half full or out of space. Stubs run under a read lock
  (registered with the fault guard); generating a stub takes the write lock.
- Vector ADD/SUB and the three-register bitwise instructions use caller-saved V0–V2 for their
  operands. BSL, BIT and BIF also load the old destination. Each writes back the full destination
  vector, including the cleared upper half for 64-bit forms.
- The other SIMD/FP stubs save host d8–d15 and FPCR, load guest FPCR, FPSR, NZCV and V0–V31,
  execute one instruction, write V, FPSR and NZCV back, and restore the host FPCR.
- Tier 1's enter routine does the same for a whole run (saves host d8–d15 and FPCR, loads guest
  FPCR, FPSR, NZCV, V0–V31); its exit routine stores V0–V31, FPSR and NZCV and restores host state.
- SIMD and FP loads and stores are implemented in C through the soft-MMU, checking every page first.
  Multiple-structure LD1/ST1 copy one 8- or 16-byte register at a time after checking the whole
  range for page faults and watchpoints. Interleaved LD2–LD4/ST2–ST4 copy one element at a time.
- If native stubs are unavailable, those instructions are undefined and no SIMD or FP feature is
  advertised.

### Exclusives and atomics (both tiers)

- LDXR records address, size and the loaded value; STXR succeeds only if address and size match
  and a compare-and-swap against the recorded value succeeds on the host address (ABA allowed).
  Every STXR clears the monitor; CLREX and `svc` clear it. Natural alignment is required.
- Tier 0: 16-byte pairs use host `ldaxp`/`stlxp`; LDAR/STLR are C acquire/release accesses; LDAPR
  and LDAPUR are implemented as acquire; LSE and CAS/CASP run as native stubs on the translated
  host pointer.
- Tier 1: LDXR loads from the host address after the TLB check (LDAXR with `ldar`); STXR checks the
  monitor and then runs an `ldaxr`/`stlxr` compare-and-swap loop with no memory access in between,
  borrowing an unused guest register (saved in its own CPU slot) when the value to store is in a
  stolen register. LDAR/STLR, LDAPR, LDAPUR/STLUR, CAS and LSE run natively on the host address.
  LDXP/STXP, CASP with stolen operands, and misaligned atomics go to Tier 0.

### Tier 1 register convention

- Guest registers live in the same-numbered host registers, except the "stolen" ones, which live
  in the CPU state: X16, X17, X18, X26, X27, X28 for the default set; X18 and X21–X25 for the Go
  set. Host x18 (the Android platform register) is never touched.
- A stolen operand is loaded into a temporary, the instruction is re-encoded to use it, and the
  result is written back; an instruction with more than three distinct stolen registers goes to
  Tier 0. Guest SP lives in a host register, so SP-based accesses and writebacks need no extra
  loads.
- Inserted code never changes NZCV (it uses `cbnz`, `tbnz`, `eor`/`eon` instead of compares).
- The set is fixed per instance at creation: the loader picks `Go` when the executable carries a Go
  build-ID note (`TUXPORT_DBT_REGS=go|default` overrides); routines and templates are generated per
  set.

### Tier 1 blocks and instruction handling

- A block starts with a two-word kick check, holds at most 64 guest instructions, and never crosses
  a guest page (a forward chain exit to the next pc ends it). It ends at B, BL, SVC, BR/BLR/RET and
  at any instruction handed to Tier 0; conditional branches become side exits and translation
  continues on the fall-through path.
- Every non-SIMD instruction must first be accepted by Tier 0 (`tier0_defined`, which runs the word
  on a probe CPU with no page table and caches the answer per instruction word in a 4096-entry
  per-thread cache); SIMD data processing uses Tier 0's whitelist decoder directly. Anything not
  accepted ends the block with an exit to Tier 0.
- Handling:
  - register-only ALU, FP and NEON instructions and barriers are copied verbatim (operands renamed
    when stolen); DSB and ISB check `ivau_page` after executing the barrier;
  - ADR/ADRP and literal loads become constants based on the guest PC; registers set by ADR/ADRP
    (plus immediate adds) are tracked as known constants within the block, cleared conservatively;
  - loads and stores get the inline TLB path (section 8); PRFM emits nothing;
  - `svc #0` exits to the kernel inline; other `svc` immediates, BRK, and PSTATE writes other than
    FlagM go to Tier 0;
  - `mrs fpcr` and `mrs`/`msr tpidr_el0` use the guest CPU state. `mrs dczid_el0` becomes the
    constant 4. `mrs fpsr` reads the host FPSR, including flags raised earlier in the block.
    NZCV, CNTVCT, CNTVCTSS and CNTFRQ accesses also run natively after the Tier 0 whitelist
    check. An FPCR write stays in the block if its masked value equals the saved guest FPCR.
    Changed FPCR writes, FPSR writes and other system registers go to Tier 0;
  - HINT space, including PAC and BTI hints, emits nothing; authenticated branches go to Tier 0;
  - DC CVAC/CVAU/CIVAC/CVAP/CVADP and `ic ivau` only check read access; `ic ivau` records its page
    in `ivau_page` (page zero or a second, different page exits to Tier 0); `dc zva` zeroes via the translated
    address; CLREX clears `excl_size`.
- Exit stubs carry the guest pc, the patch site and a packed reason word back to the dispatcher.

### Tier 1 memory access

- Handled inline: integer LDR/STR in all immediate, pre/post-index and register-offset forms,
  literal loads, LDP/STP/LDNP/LDPSW, SIMD single-register, pair and literal forms, and LD1–LD4 /
  ST1–ST4 (multiple, single lane, replicate, with post-index). Others go to Tier 0.
- Hit path for an 8-byte read with the default registers (x16 holds the guest address):

  ```asm
  add  x17, x16, #7            // last byte
  ubfx x17, x17, #12, #12      // low 12 bits of its page number → TLB index
  add  x17, x28, x17, lsl #5   // x28 points at the CPU state
  ldp  x17, x27, [x17, #TLB]   // tag (inverted page number), addend
  eon  x17, x17, x16, lsr #12  // compare with the first byte's page; never equal across pages
  cbnz x17, refill_stub
  ldr  x0, [x27, x16]          // host address = addend + guest address
  ```

- The entry is chosen by the last byte's page and the tag compared with the first byte's page, so a
  cross-page access never hits inline. Variants of the listing: for immediate offsets within
  ±4 KB the last-byte address is computed from the base in parallel with the access address;
  1-byte accesses, and accesses known to stay inside one aligned 16 bytes (SP-based after the
  alignment check, atomics), use the first byte's page directly; pairs and structure accesses add
  the address into the addend register and access through it.
- Known addresses (ADR/ADRP-derived bases, literals) use a constant TLB slot; the tag is still
  compared with the real address.
- SP alignment is checked once per block and not again while SP only changes by multiples of 16.
- A miss branches to a per-access 5-word refill stub (`adr`, branch to the routine, an info word
  with size, failure reason and resume offset, and the guest pc), then to a shared refill routine in
  the code cache. The routine borrows the CPU state's X0–X4 slots as scratch, fails whenever any
  watchpoint is set or either endpoint exceeds 48 bits after ignoring its top byte. The page-table
  walk uses plain loads and low address bits. The TLB tag and addend use the full guest page base,
  so another access with the same top byte can hit inline. Atomics untag their effective address
  before lookup. Filling a write entry requires read and write permission because grouped accesses
  and atomics can also read. The refill records the slot in `tlb_used` and resumes. A cross-page
  access succeeds only if both pages are permitted and have the same addend (contiguous host memory).
- On failure the stub exits to the dispatcher and Tier 0 executes the instruction. Nothing guest
  visible has happened before the exit, so exceptions are precise.
- Grouped checks: several accesses in a block with the same base share one check. The leader checks
  the whole range (at most 512 bytes, at most 16 members) and leaves the host base in a temporary;
  members access `[T2, #offset]`. Between members only instructions that do not write the base
  (constant adjustments are tracked), SIMD data processing, conditional branches, hints, barriers,
  CLREX and accesses on other bases are allowed; an access on another base makes the next member
  reload the host base from one of four slots in the CPU state. Atomics, unconditional branches and
  other system instructions end a group; literal, register-offset, register-writeback and
  stolen-base accesses cannot lead; a register-writeback member must be last.
- A group check failure exits to Tier 0, which runs the leader. The first failure of a leader is
  recorded (64-entry direct map keyed by pc); the second retires the blocks containing it (or
  requests a full flush if a retirement cannot be placed) and stops grouping for leaders in the
  following 256 bytes (64-entry ring; templates are versioned by it). Failures while watchpoints
  are set are not counted.

### Tier 1 control flow

- Direct branches are linked by patching a `b`: a link stub starts with a branch to its next word,
  which is rewritten to branch to the target block (concurrent `b`→`b` patching is allowed by
  CMODX). Conditional branches are rewritten directly only while one thread is attached to the
  instance.
- At install time only conditional side exits whose target is already translated are linked; other
  exits are linked by the dispatcher the first time they are taken.
- A patched link point only gets cache maintenance (no wait, no `isb`); the patching thread enters
  the target directly, others may run the old code briefly and still reach the target through the
  dispatcher.
- Indirect branches (`br`, `blr`, `ret`) probe the thread's 4096-entry jump cache in generated
  code; on a miss an in-cache lookup routine probes the shared table (and fills the jump cache)
  before exiting to the dispatcher.
- Return prediction: `bl`/`blr` become a host `bl` that skips a landing pad and push
  (guest return address, landing pad) onto the 64-entry shadow return stack; `ret x30` pops when the
  top matches x30 and returns with a host `ret`. Mismatches (longjmp, signal handlers, coroutines)
  and `ret` through other registers use the jump cache. The stack is reset when the translations are
  flushed.
- Kick checks: every block entry checks `pending`; forward direct links skip the check, so every
  loop passes through at least one check. The dispatcher also checks before entering code.
- Stubs keep their data (guest pc, reason) in data words at their end, read by the routines.

### Dispatcher

- `tuxport_dbt_run` runs under the fault guard; a poisoned instance runs Tier 0 for the whole call.
- Each iteration: join or perform a pending flush, catch up with the instance's generation (reset
  the return stack, clear the jump cache), return `Kick` if pending, look up the block without a
  lock, revive parked blocks of that page if any, otherwise translate and install, then enter.
- Exit reasons: chain (patch the link point), lookup (fill the jump cache), ungroup (record the group
  failure, then run Tier 0), interp (run one instruction in Tier 0, then invalidate any recorded
  `ivau_page` in place), code (invalidate the pending page after a translated DSB/ISB, then look up
  the next PC), svc (clear the monitor, return `Svc`), kick (loop). Tier 0's own exits are
  returned to the kernel, except `ic ivau`, which is absorbed in place.
- A host fault inside the run marks the instance poisoned and clears the template arena.

### Code cache, table and memory

- The code cache is 128 MB (the reach of `b`), mapped twice from one shared anonymous region with
  `mremap` (read-write for writing, read-execute for running; read-write-execute if the host
  refuses) and bump-allocated. Dead blocks free no code; only a full flush reclaims space.
- Translation happens without the lock in the thread's own workspace; under the lock `install`
  re-compares the guest words (retranslating if they changed), copies the code, fixes routine
  branches, links known exits, syncs the instruction cache and only then publishes the block in the
  table. A thread alone in the instance syncs the new block and a pending link point together.
- The table is read without locks: insertion only fills empty slots (pc before code), removal only
  writes tombstones (never reused, except that relinking a parked block reuses its own tombstone),
  and readers announce themselves with a per-CPU sequence count so a replaced table is freed only
  after they finish. The C reader loads code with acquire before the key; the generated lookup
  routine loads both with one plain `ldp` and leaves pc 0 to the dispatcher, since a torn read can
  pair new code with an empty slot's key 0.
- Sizing: a new or emptied instance starts at 2^12 slots (a kept pooled instance keeps its size).
  At 3/4 occupancy the table doubles up to 2^19, or is rebuilt at the same size when tombstones are
  at least half of the occupied slots or it is already at 2^19; if load is still at 3/4 a full flush
  follows. The block log is compacted (records only) when it reaches its limit with enough dead
  records.
- Retiring a block writes a 16-word forwarding stub, rewrites the block's entry to branch to it,
  then tombstones the table entry. Jump-cache entries are not cleared; they reach the forwarder.
  Translating the pc again patches the old entry to the new block.
- A full flush is triggered by a full code cache, a table that cannot be kept below 3/4 load, a block
  log at its limit with fewer than 1/8 of its records dead (or a failed compaction), no room for a
  forwarding stub, a recheck with too many mismatches, or `Dbt::flush` (which only requests it). It
  kicks the other threads running in the dispatcher, waits until each is parked or has left it,
  clears the table and records, resets the cache allocation and bumps the generation. Dropping an
  instance without keeping its translations also flushes it.
- Pages written through the read-write view are tracked; `unmap_written_code` drops their host page
  table entries (content kept) because the host counts pages touched through both views twice. The
  kernel's timer thread calls it every second while guest threads run (and once after they all go
  idle), and the OOM and cgroup measurements subtract `written_code_bytes`.

### Sharing translations

- A block's translation depends only on its guest pc, its guest instruction words, the register set
  and whether it is shared; the words are saved and re-compared at install, recheck, revive and
  content-based invalidation.
- Threads of one address space share one instance; each has its own jump cache, return stack,
  statistics and workspace.
- Code groups (engine side): members share one instance. Blocks in ranges registered with `share`
  are shared (tag 0); other blocks are private to the member that translated them (key
  `pc | tag << 48`, tags from `new_tag`). Lookup finds either; shared blocks never link to private
  ones, so such exits go through the dispatcher.
- Templates: every translated block is stored in a process-global 32 MB arena (64K buckets, 64 shard
  locks, at most 4 versions per pc), keyed by pc and matched on guest words, register set, the
  ungroup state and the shared flag. Another instance copies a matching template into its own cache
  and re-points only the branches to routines. The arena is cleared when full and on a host fault.
- Pool: a dropped instance that is not poisoned is kept (marked stale, private blocks dropped) if it
  has a key, at most 131072 block records (dead ones included) and 64 MB of code, no pending flush,
  and, above 32 MB, fits in the remaining 64 MB code budget; otherwise it is flushed. A poisoned
  instance is released. The pool holds 16 instances, evicting empty ones first, then the least
  recently used. `DbtSpace::new` takes the newest pooled instance with the same key and register
  set, else the first emptied one (regenerating routines if the register set differs), else a new
  one.
- Reuse of a stale instance: `trust` (only on a stale instance) marks ranges whose start, length and
  identity match exactly as trusted, and makes the previous user's `set_sources` ranges parkable.
  On the first run `recheck` skips trusted blocks and compares the rest. A mismatching block whose
  page has no executable mapping and that lies within a parkable range is parked, and revived
  (whole page re-checked) the first time that page is executed; other mismatches are retired; if
  non-parkable mismatches exceed half of the live blocks, nothing is retired and a full flush is
  requested instead.

### Invalidation

- The kernel replays its code-change log through `Dbt::invalidate(cpu, start, len)`, or calls
  `Dbt::flush` when the log no longer covers what the thread missed.
- Invalidation with a CPU is by content: blocks whose saved words differ from current memory, or
  whose page no longer has an executable mapping, are retired; parked blocks are left alone.
  Without a CPU every block in the range is retired and parked ones are killed. Ranges spanning more
  than 256 pages (or wrapping) scan every block; blocks of every group member are compared against
  the caller's memory (no tag filter).
- A small-range invalidation unlinks dead block records from each page bucket it visits. Parked
  blocks stay in the bucket for revival. Compaction reclaims dead records in the block log.
- `ic ivau` in Tier 1 records the untagged page. Tier 0 handles page zero (the pending-page
  sentinel) or a second, different page and returns its address explicitly to the dispatcher.
  After a translated `dsb`/`isb`, a dedicated exit invalidates any pending page in place before
  resuming at the following PC. The kernel collects a pending page left at another exit.
- `retire_outside` (first join of a group) and `ungroup` also retire blocks.

### CPU features and ID registers

- Guest features = host features ∩ Tier 0's implemented set ∩ the whitelist, computed once.
  CPUID is always advertised. The implemented set (FP, ASIMD, AES, PMULL, SHA1/2/3/512, CRC32,
  ATOMICS, ASIMDDP, LRCPC, ILRCPC, FPHP/ASIMDHP, JSCVT, ASIMDRDM, FCMA, ASIMDFHM, and in HWCAP2 I8MM,
  BF16, FRINT) is intersected with the host's and offered only if the host has FP and ASIMD and
  native stubs work. FlagM, FlagM2, SB, DCPOP, DCPODP, DIT and ECV (HWCAP2, the self-synchronized
  counter `CNTVCTSS_EL0`, which the host's vDSO reads instead of `CNTVCT_EL0` on hosts that have
  it; guests run that code, `kernel/vdso`) are offered from host support regardless of that gate.
- Dependencies: SHA3 and SHA512 only together and with SHA2; FPHP and ASIMDHP together; FHM needs
  FPHP; LRCPC2 needs LRCPC; FlagM2 needs FlagM; DCPODP needs DCPOP. Not implemented: LSE2, SM3/SM4,
  SVE/SME, PAC (PACA/PACG), BTI, MTE.
- Adding a SIMD/FP extension only needs whitelisting its encodings: Tier 0 executes them through
  native stubs and Tier 1 copies them.
- The same set feeds `AT_HWCAP`/`AT_HWCAP2`, `/proc/cpuinfo` and `mrs` of the ID registers
  (op0=3, op1=0, CRn=0, CRm 0–7): MIDR is the host value (0 if the host does not advertise CPUID),
  MPIDR has only bit 31, PFR0 (AArch64-only EL0/EL1 plus FP, AdvSIMD, DIT) and ISAR0/ISAR1 are
  derived from the set, every other register in that range reads 0; CRm 8–15 is undefined.
- DIT is kept in the CPU state; the host thread's DIT follows the guest's at each entry.

### Host fault guard

- `install_fault_guard` installs SIGSEGV, SIGBUS and SIGILL handlers (chained after ART's); the
  machine installs it only when kernel bugs are contained (release builds by default, or
  `TUXPORT_ON_BUG`). During a guarded run, a host fault with `si_code > 0` long-jumps back: the host
  FPCR is restored, registered locks are released newest first (the outer four of those held;
  deeper ones are counted so drops still pair), the signal is unblocked, and the run returns
  `HostFault`. The locks taken during a run are registered: the instance lock, the template
  shard of a bucket, the template arena lock, all template locks together while `template_clear`
  holds them (one entry with a release function), the native-stub lock and the perf-map lock. Other faults are reported to stderr and passed to the
  previous handler. If the per-thread guard cannot be allocated, the run proceeds unguarded.
- Guarded code must not allocate or call back into the kernel layer; the one documented exception
  is the opt-in perf map (`TUXPORT_PERF_MAP`), which writes `/tmp/perf-<pid>.map` from `install`.
- Guest state is not reconstructed from the host context; after a host fault the kernel kills the
  guest process, the Tier 1 instance is poisoned, and the template arena is cleared.
- Internal overflow checks (native stub length, refill info word, stub/fixup/block words,
  relocations, a group slot mismatch) call `tuxport_guard_fail`: under an armed guard the run ends
  as after a host fault, recorded as `SIGABRT` with the caller's address (the instance is poisoned
  as for any host fault); with no guard armed (debug builds, unguarded calls) it aborts as before.
  The checks run during translation and stub generation, where only registered locks are held.

## Dependencies (inbound / outbound)

### Inbound

- The kernel task run loop (`Cpu::run`/`step`, `Dbt::run`, exit handling, `ivau_page`, kicks).
- `kernel/mmu` (`DbtSpace` creation, code groups, `trust`/`share`/`retire_outside`/`set_sources`,
  `Dbt::invalidate`/`flush`, TLB flushes, `Mmu`), the loader (register set, `AT_HWCAP`), signals and
  ptrace (FP registers, kicks, hardware debug slots), procfs (`/proc/cpuinfo`, MIDR), the machine
  (fault guard), the OOM timer (`kernel/oom`: `unmap_written_code`, `written_code_bytes`).
- Tests: `core/engine/tests` (interpreter, translator, page table, guard) and `tests/lockstep`
  (random instruction sequences compared between hardware, Tier 0 and Tier 1; the whitelist check
  against `llvm-mc`, which runs only on Linux aarch64 and is skipped without `llvm-mc`).

### Outbound

- `kernel/mmu` structures: the page table (`tuxport_mmu_translate`, the PTE layout walked by the
  refill routine), the TLB entry layout and `tlb_used`.
- Host: `getauxval`, `mrs midr_el1`/`fpcr`/`cntvct_el0`/`cntvctss_el0`/`cntfrq_el0`, `msr dit`, `mmap`/`mremap`/
  `madvise`, pthreads (locks, once, TLS), `sigaction`/`sigsetjmp`/`siglongjmp`, instruction-cache
  maintenance.

## Invariants and gotchas

- Both tiers must agree on every instruction: Tier 1 copies or translates only what Tier 0 accepts,
  and SIMD data processing uses the same whitelist decoder. Encodings outside the whitelist are
  undefined in both tiers and are never copied.
- Precise guest exceptions come from checking before acting (TLB checks, alignment checks, Tier 0
  fallback), not from host signals: a host fault in translated code is never mapped back to guest
  state.
- While translated code runs, the CPU state does not hold the guest's registers (routines also
  borrow its X0–X4 slots); it is consistent only after an exit.
- Fields before the TLB in `struct tuxport_cpu` must stay within immediate reach of Tier 1 loads, and
  the TLB must be 16-byte aligned; the Rust mirror is checked by `layout_matches` (size plus eight
  field offsets, in a unit test).
- Watched pages never enter the TLB (read half absent for load watchpoints, write half for any),
  and Tier 1's refill fails while any watchpoint is set, so all watched accesses run in Tier 0.
  Hardware breakpoints are checked only in Tier 0.
- Code memory is reclaimed only by a full flush, which happens only after every other thread has
  left translated code.
- Guest FPCR stays constant until translated code exits. A changed masked FPCR write takes an
  interpreter exit, so Tier 1's exit does not save FPCR. Tier 0 handles all FPSR writes. The host FPSR is
  left holding the guest's value.
- `Kicker::kick` is the only operation allowed from other threads; it is safe only while the `Cpu`
  is alive. The kernel guarantees this by kicking only under the BKL, only tasks it still lists or
  registered users of an address space, and by removing a thread from both before its `Cpu` is
  freed; `Cpu::reset` (execve) zeroes `pending`, and the kernel re-kicks afterwards if the task was
  interrupted. The engine's own flush kick may race a reset harmlessly.
- Atomic instructions act directly on host memory reached through the page table, so threads and
  processes sharing a page get correct atomicity for free.
- The `llvm-mc` whitelist check follows LLVM on should-be-one fields: exclusives and LDAR/STLR
  ignore them, CAS/CASP require Rt2 all ones.
- Templates are copied, never referenced, so dropping the arena never invalidates running code.

- Known-address checks are safe only because constant tracking is exact: it is reset at every block
  start and cleared by any instruction that may write a tracked register. A stale constant would let
  a real cross-page access alias the constant slot and pass with only its first page checked.

## Confirmed bugs / technical debt

- **The kernel drops page-zero invalidation from pure Tier 0:** its code-change path filters out
  zero even for an explicit `InvalidateCode` exit. A single-stepped write and cache flush on
  page zero can therefore leave stale code when execution returns to Tier 1. Tier 1's own
  dispatcher handles page zero directly.
- **Lock-free conditional-branch relinking races a new thread:** the `nthreads == 1` check is not
  serialized with `attach`, and the kernel attaches new threads and code-group members without
  stopping running ones, so a conditional branch can be rewritten while another thread executes it
  (architecturally unpredictable; in practice old and new targets behave the same).
- **Unallocated ID registers read 0:** `mrs` of CRm=1 and unallocated CRm=0 registers returns 0
  where Linux 6.1 delivers `SIGILL`.
- **Test-only API:** `Dbt::recheck` is called only by the engine tests (`tests/dbt.rs`); the
  kernel's exec path reaches the same check through instances taken from the pool.

## Open questions

- Is it intended that FAULT, KICK and signal exits leave the exclusive monitor set (architecturally
  an exception return clears it)? The compare-and-swap semantics make this mostly harmless.
- Should FPCR written by sigreturn or ptrace be masked like `msr fpcr` in Tier 0?
- Is it intended that threads of other code-group members disable direct conditional-branch
  patching for the whole instance?
- Can `revive` restore a parked block whose pc already has a newer translation, leaving two table
  entries for one key?
- Two group leaders that alternate failing in the same strike slot are never ungrouped; accepted?
- Should the template arena ever be trimmed under memory pressure?

## To verify

- Tagged branch targets fault in both tiers (the PC is never untagged). Whether this diverges from
  Linux 6.1 depends on whether it leaves TBID0 clear for EL0 (instruction addresses then also
  ignore the top byte); needs the Linux source.
- Template copies reproduce a fresh translation exactly; there is no runtime self-check.
- Tier 1's page-table walk uses plain, address-dependent loads where `mmu.c` uses acquire loads;
  only a code comment argues they are equivalent.
- `layout_matches` checks only the size and eight field offsets and runs only in a unit test; other
  mirrored fields are covered only indirectly.
