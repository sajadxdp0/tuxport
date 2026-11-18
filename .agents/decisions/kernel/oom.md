# Memory accounting and the OOM killer (kernel/oom) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/oom"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions on memory accounting and the OOM killer made before kernel/oom was split from kernel/mm keep their `kernel/mm` tags and live in `.agents/decisions/kernel/mm.md`: D26, D29, D32, D35 and D44.

## Active

### D1: kernel/oom is split out of kernel/mm
- **What**: kernel/oom owns `oom.rs`, the resident-memory measures and `Usage` in `mm.rs`, `Mm::release`, and process_mrelease in `syscall/mem.rs`: resident memory for procfs, rusage and child cgroups, the app-wide and cgroup limits, victim choice and kills, `oom_score` and the release of written code views.
- **Why**: `oom.rs` is the most independent part of mm (only 9 of its 17 commits touched `mm.rs` too), and its consumers are procfs, cgroups and rusage rather than mmap.
- **Tradeoffs**: Measures depend on mm internals, procfs owns views, kernel/cgroup owns cgroup policy, kernel/files triggers immediate memory.max rechecks, and kernel/time owns periodic checks and the idle handshake called from the process run loop.
- **Watch out**: A new kind of mapping that the measures do not classify, now that the classification lives apart from the mapping code.
- **Source**: 44a21b7; cf029f9a; 8a17e4a9; d053be24; 39e55b4b
