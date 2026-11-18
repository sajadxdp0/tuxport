# Android instrumentation (android/tests) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/tests"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions on android/tests made before it was split from android/native keep their `android/native` tags and live in `.agents/decisions/android/native.md`: D5, D8, the test portions of D9, D12–D14, D16, D19 and D23–D32.

## Active

### D1: Android instrumentation has its own maintenance boundary

- **What**: android/tests owns instrumentation source and verification entry points, while runtime behavior and build configuration retain their existing owners.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14; 39e55b4b
