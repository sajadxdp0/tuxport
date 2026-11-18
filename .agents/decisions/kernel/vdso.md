# Guest vDSO images (kernel/vdso) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/vdso"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions on kernel/vdso made before it was split from kernel/signal keep their `kernel/signal` tags and live in `.agents/decisions/kernel/signal.md`: D2, D4 and the vDSO portion of D12.

## Active

### D1: vDSO construction owns the host-process image cache

- **What**: kernel/vdso owns vdso.rs image discovery, validation and construction plus the host-process OnceLock lifetime, while mapping and dispatch remain outside.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14; 39e55b4b
