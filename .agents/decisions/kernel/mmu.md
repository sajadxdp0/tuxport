# Soft-MMU (kernel/mmu) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/mmu"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Soft-MMU decisions made before kernel/mmu was split from kernel/mm keep their `kernel/mm` tags and live in `.agents/decisions/kernel/mm.md`: D1, D13, D16, D39 and the page-table half of D4.

## Active

### D1: kernel/mmu is split out of kernel/mm
- **What**: kernel/mmu owns `mmu.c` and the `Mmu` wrapper, the software TLB in `cpu.c` and `internal.h`, and in `mm.rs` the Consistency and code section and the `Mm` methods that fill entries (`map_pages`, `fault_in`) and keep threads consistent (users, generations, code spaces, `sync`, `quiesce`, `shootdown`), and the engine map's references to the page table and code groups point here.
- **Why**: The page table, TLB and consistency machinery are a contract with the engine and the run loop whose changes rarely need the VMA syscalls' detail, and the mm map (69.6 KB) could not be read in one Read.
- **Tradeoffs**: Every VMA change records what kernel/mmu acts on, so changes to unmapping or permissions still read both maps.
- **Watch out**: A VMA change that skips recording for `sync` because the rule now lives in this map; the map keeps open the bug of entries dropped when a host allocation fails.
- **Source**: 44a21b7

### D2: Unwatched scalar hits use a fixed-width same-page copy

- **What**: Tier 0 computes the candidate host address alongside the shared tag check and uses a fixed-width copy for an unwatched scalar hit on one page.
- **Why**: Generic copies and slow-path stack setup cost time on ordinary hits.
- **Tradeoffs**: The outlined miss path can repeat the probe, and the probe expression also affects instruction-fetch code generation.
- **Watch out**: Use the candidate only after a hit and handle faults and watchpoints before register or memory writeback.
- **Source**: 16914003

### D3: Tier 1 retains the top byte in data TLB entries

- **What**: Cache the full guest page base in the tag and addend, walk the low 48 bits, and require both read and write permission for a cached write.
- **Why**: Tagged pointers otherwise missed repeatedly, and grouped or atomic operations can use a write entry to read.
- **Tradeoffs**: Aliases with different top bytes compete for a slot, and raw write-only mappings take the Tier 0 store path.
- **Watch out**: Check endpoints and watchpoints before access and use untagged addresses for exclusive monitors and instruction-cache invalidation.
- **Source**: 8bc0e63b
