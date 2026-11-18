# kernel/mmu Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/mmu.md` for the paired decision history (decisions from before the split are D1, D13, D16, D39 and the page-table half of D4 in `.agents/decisions/kernel/mm.md`).
>
> Verified: 2026-11-11 (split out of the kernel/mm full mapping of 2026-11-10; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-17 (targeted verification only since the full mapping; latest change: data TLB tags and write-entry permissions)

## Responsibilities

`kernel/mmu` is the soft-MMU under `kernel/mm`'s address spaces: how a guest address becomes a
host address, and how every thread of an address space sees changes. It does not emulate ARM
hardware translation (no TTBR, descriptor formats, TLBI/ASID, exception levels or physical address
layer):

- the software radix page table of each address space and its entries;
- populating entries, eagerly or on first access (`fault_in`);
- the per-thread software TLB and its Tier 0 refill;
- multi-thread consistency: `sync`, `quiesce` and `shootdown`, and the generation counters the run
  loop reads;
- the code-change log that Tier 1 translations consume, and code groups (address spaces sharing
  one translation space).

Root:

- `core/engine/src/c/mmu.c` and the `Mmu` wrapper in `core/engine/src/lib.rs` — the radix page
  table.
- `core/engine/src/c/cpu.c` and `core/engine/src/c/internal.h` — the per-thread software TLB and
  Tier 0 refill.
- In `core/kernel/src/mm.rs`: the `Consistency and code` section, and the `Mm` methods that fill
  entries (`map_pages`, `fault_in`) and keep threads consistent (`add_user`, `remove_user`,
  `generations`, `code_space`, `invalidate_code`, `sync`, `quiesce`, `shootdown`).

Outside the module: VMAs, host backing, the fault policy, copy-on-write and what each change
records for `sync` (`kernel/mm`); the run loop that reads the counters (`kernel/process`); the
Tier 1 translator with its inline TLB fast path and refill (`engine`).

## Key types

### Translation

- `struct tuxport_mmu` / `Mmu` — 4-level radix tree, 512 entries per level, covering 48-bit guest
  addresses; each leaf table spans 2 MB. A leaf entry is a 4 KB-aligned host page address ORed with
  R/W/X bits; 0 means absent, and prot 0 with a nonzero address means "present, no access". `Mmu`
  is neither `Send` nor `Sync`; `map`/`unmap`/`protect`/`move_entries` take `&mut`, while
  `fill_shared`/`map_shared`/`protect_shared`/`next_present`/`translate` take `&self` so CoW can
  edit another address space's table. Guest address spaces only fill empty slots (`fill_shared`,
  `map_shared` for CoW); the replacing `map` serves the vDSO's own probe table.
- `struct tuxport_tlb_entry` — data TLB entry `{read tag, read addend, write tag, write addend}`,
  4096 direct-mapped entries per thread; tag is the inverted page number and 0 is invalid, so
  zeroed memory is an empty TLB. A `tlb_used` bitmap records filled slots.
- `struct tuxport_itlb_entry` — 512-entry instruction TLB, used only by Tier 0 instruction fetch.

### Consistency and code

- `TaskSync` — per-thread `in_guest` flag plus a `Kicker` (sticky: a CPU not running returns at
  its next run), shared without the BKL.
- `Generations` — clones of the three counters (`tlb`, `code`, `space`) and the code log; the run
  loop reads them without the BKL.
- `CodeLog` — log of `(generation, start, len)` code changes covering the last 4096 code
  generations.
- `CodeSpace` — the Tier 1 translation space (`DbtSpace`) used by an `Mm`, with its tag and the last
  code generation applied to that space.
- `CodeGroup` / `CodeGroups` — groups of address spaces running the same executable that share one
  `DbtSpace` (at most 256 registered trusted ranges, which only grow; each member's private
  executable ranges as claims under its tag); the kernel keeps a list of weak references, pruned
  when a group is founded.
- `Mm`'s consistency fields (`kernel/mm`): `dirty`, the `graveyard`, `code_ranges`,
  `exec_changed`, the users and the code-group membership.

## Public entry points

- **Threads and the run loop**: `add_user`, `remove_user`, `generations`, `code_space`,
  `invalidate_code`; `Cpu::tlb_flush`.
- **After changes**: `sync` (after BKL syscalls, `ic ivau` exits, `write_force`, truncation and
  file moves), `quiesce` (fork, membarrier, `sync`), `shootdown` (CoW re-pointing an entry).
- **Entries**: `fault_in` (the fault path), `mmu().translate`, `map_pages` (inside `kernel/mm`).
- **Tier 0**: `tuxport_tlb_fill` on a TLB miss.

## Data flow / lifecycle

### Page-table entries

- `map_pages` builds entries eagerly when the part of the VMA before EOF is at most 1 MB, or when
  the mapping is executable and not private anonymous (file mappings, shared anonymous, SysV shm).
  `PROT_NONE` mappings get none. If building them fails it removes every entry of the range.
- Larger mappings are filled on demand by `fault_in`. It fails if no VMA covers the page, the page
  is at or past EOF, the VMA prot (with write implying read) lacks the access, or the page already
  has an entry (including a prot-0 one). Otherwise it fills every empty slot of the 2 MB leaf range
  clipped to the VMA and EOF; existing entries are left alone and no flush is needed. A `Fill` VMA
  gets entries only for pages already read (`kernel/mm`).
- `mprotect` to `PROT_NONE` keeps entries with prot 0 (the host page keeps its content); changing
  back rewrites the kept entries and fills the rest under the eager rule above, otherwise on demand.
- Execute-only mappings (`PROT_EXEC` without `PROT_READ`) do not gain read permission: data loads
  and kernel reads fault, instruction fetch works.

### The TLB

- Tier 0 looks up the data TLB in C (data addresses untagged first) and calls `tuxport_tlb_fill` on
  a miss. The fill fails if the entry is absent or lacks the needed bit. A data fill sets both
  halves at once, leaving the read half invalid on pages with a load watchpoint and the write half
  invalid on pages with any watchpoint or without read permission (Tier 1 can use a write entry
  for a group that also reads). It records the slot in `tlb_used` and stores `addend =
  host page − guest page base`. Exec fills only the ITLB. Tier 1's refill defers to Tier 0
  whenever any watchpoint is set. Its data entries can include the guest pointer's top byte in
  both the inverted page number and the page base used for the addend. Tier 0's untagged lookup
  then misses and refills the same slot.
- Cross-page accesses of up to 16 bytes look up both pages before copying anything; a failing
  second page reports the start of that page as the fault address.
- A TLB flush clears only the slots marked in `tlb_used`, plus the whole ITLB; CPU states kept
  for reuse (up to 8) are cleared the same way.

### Consistency: shootdown and code invalidation

- Mutators run under the BKL and only record what changed: `dirty` (unmap, mremap moves, permission
  downgrades, fork lending, `unshare`, truncation and file moves, leaving a code group), the
  `graveyard` (always together with `dirty`), `code_ranges` (executable ranges unmapped, moved,
  re-protected, discarded or truncated, and invalidations), and `exec_changed` (executable VMAs
  inserted, re-protected or grown, kernel writes into them; never an unmap).
- `Mm::sync` runs after every syscall that takes the BKL (except exit, `exit_group`,
  `rt_sigreturn`, a successful execve or execveat, a syscall a tracer or seccomp skipped, and a
  kill at the entry stop; the no-BKL fast path never changes the mm), on `ic ivau` exits (and any
  exit with an `ivau` page pending), after `write_force`, and on every address space a truncation
  or file move changed. It re-checks the code group; returns when nothing is dirty and no code
  changed; publishes `code_ranges` to the `CodeLog` and bumps the code generation; returns if
  nothing is dirty and the graveyard is empty; otherwise bumps the TLB generation (when dirty),
  runs `quiesce`, and clears the graveyard.
- `quiesce` kicks every other user of the mm and spins until none is in guest code. The kick is
  sticky, so a thread returning from a no-BKL fast syscall stops at its next guest entry. fork and
  membarrier quiesce too.
- `shootdown` is the immediate form used when a PTE is re-pointed (CoW copy, return of a borrowed
  page): with users, it bumps the TLB generation, kicks only users in guest code, and waits until
  none is in guest code, kicking again any it still finds there (one that entered after the first
  kicks, which nothing else would stop). It does not hold users back: one that enters guest code
  after the wait has passed it (such as a thread waking from a no-BKL fast-path sleep) runs with a
  flushed TLB while the caller still holds the BKL.
- The run loop sets `in_guest`, then reads the counters (a SeqCst store-then-load pairing with the
  mutator's bump-then-read). If `space` changed it re-attaches to a new `CodeSpace` (when it had a
  translator) and looks again on the next pass; if `tlb` changed it flushes its TLB; if `code`
  changed it invalidates Tier 1 blocks for the ranges its space has not yet applied (once per
  space), or flushes all translations when the log no longer covers them.
- Code-only changes kick nobody; other threads see them on their next guest entry, and membarrier
  is the way to force it: every barrier command fences and quiesces the caller's address space
  only (the private ones need registration, `EPERM`). Tier 1 handles `ic ivau` inside the
  translator; the log carries mapping changes, Tier 0 invalidations, and leftover `ivau` pages.

### Code groups (mm side of translation sharing)

- The first thread to run an `Mm` (and after exec) calls `code_space`: for its executable it takes a
  tag in an open group with the same key (a group out of tags is closed) and joins if `admit`
  accepts its layout (its private executable ranges overlap no registered range or other member's
  claims, and the shared sources it adds are the same file, version and placement, at most 256 in
  all; a second member makes the group retire blocks outside the consistent layout once); else it
  founds a new group with its own `DbtSpace`, unshareable from the start when its layout has more
  than 256 ranges or no tag is left. A failed admission does not give its tag back.
- After any executable change, `sync` re-runs `admit`. A member that no longer fits becomes
  `unshareable` (sticky) and drops its claims: a sole member closes the group, retires the shared
  blocks outside those of its mappings that agree with the registered ranges, and keeps the space;
  otherwise it leaves, drops its `CodeSpace`, bumps the space generation and sets `dirty`, so
  `sync` quiesces. Unmapping an executable VMA does not count as an executable change, and
  registered ranges never shrink.
- A forked child starts outside any group with its parent's executable and may join any open
  group for it. On drop a member leaves the group (its claims released, the group neither closed
  nor its ranges unregistered), then gives the shared space its sources (`set_sources`).

## Dependencies (inbound / outbound)

### Inbound

- `kernel/mm`: every VMA change (`map_pages`, the recorded `dirty`, `graveyard`, `code_ranges`
  and `exec_changed`), CoW (`shootdown`, `fill_shared`, `map_shared`, `protect_shared`), the fault
  path (`fault_in`), `Mm` drop (leaving the code group).
- `kernel/process`: the run loop (`in_guest`, `generations`, TLB flushes, code invalidation, `ic
  ivau`), the dispatcher (`sync` after BKL syscalls), membarrier (`quiesce`), users at thread start
  and exit.
- The fast path reads entries through the CPU's `translate`.

### Outbound

- `tuxport_engine`: `Mmu`, `Cpu::tlb_flush`, `Kicker`, `DbtSpace`/`Dbt` (trust, new_tag, share,
  retire_outside, set_sources, invalidate, flush).
- The Tier 1 translator depends on the page-table geometry, the PTE bit layout, the TLB entry
  layout and offsets, `tlb_used`, and the watchpoint count.

## Invariants and gotchas

- Only the kernel layer modifies page tables, serialised by the BKL. Other threads may walk them
  concurrently: entries are loaded with acquire and stored with release, new tables start zeroed,
  and no table (intermediate or leaf) is freed before the page table itself is destroyed, which is
  deferred until the BKL is released.
- After an unmap, a permission downgrade or re-pointing an entry, every other thread of the address
  space must flush before the old host memory is released. Adding permissions never requires a
  flush: a stale TLB half only misses.
- Only `quiesce` (sticky kicks to every user) keeps other threads out of guest code until the BKL
  is released; after a `shootdown` other threads may run guest code at once, so every page-table
  change made under the BKL must leave entries valid for guest code at each step.
- Waiting for users to leave guest code (`quiesce`, `shootdown`) holds the BKL, so it must kick
  every user it waits for: an unkicked one may run guest code indefinitely, such as a spin waiting
  for a thread that waits for that BKL, and the whole kernel stops (LTP shmctl05). Kicks are sticky,
  so a kicked thread stops at its next guest entry too.
- Kicking a thread is safe only because a thread removes itself from `users` under the BKL before
  its CPU state is destroyed.
- Page-table memory only grows over an address space's lifetime.
- `CodeGroup` uses `Rc`/`RefCell` and is correct only under the BKL; the counters, `CodeLog`,
  `TaskSync` and `CodeSpace` are shared across threads.

## Confirmed bugs / technical debt

- **A failed page-table allocation drops entries with contents:** `map_pages` removes every entry
  of its range when building them fails, so an mprotect making a small `PROT_NONE` anonymous VMA
  accessible again can lose the entries kept for its pages (fork and RSS then miss them), and a
  move failing there loses the moved VMA; this needs a host allocation failure.

## Open questions

- In a code group, every member's drop calls `set_sources` on the shared `DbtSpace`; is
  last-writer-wins intended?

## To verify

- Linux 6.1 arm64 semantics for `PROT_EXEC`-only mappings (execute-only with Enhanced PAN, or
  exec implying read): needs the Linux 6.1 source to decide whether Tuxport's execute-only behaviour
  matches.
- That Tier 1's dependency-ordered (non-acquire) page-table walk is safe against concurrent entry
  stores: only a code comment claims it; the translator is outside this module.
- That `Dbt::invalidate` by range is correct for code-group members whose address spaces differ:
  translator internals, outside this module.
