# Host CLI and integration (harness/cli) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: harness/cli"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions on harness/cli made before it was split from harness keep their `harness` tags and live in `.agents/decisions/harness.md`: D5, D6, D9, D11, D15, D20, D21, D25, D28, D32, D35 and the guest-test portions of D36, D40 and D41.

## Active

### D1: CLI and cargo integration are separate from shared tooling

- **What**: harness/cli owns the host CLI and cargo integration checks, while shared development tooling remains in harness.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14; 39e55b4b
