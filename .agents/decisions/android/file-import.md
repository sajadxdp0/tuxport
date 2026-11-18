# Terminal file imports (android/file-import) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/file-import"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions on android/file-import made before it was split from android/terminal keep their `android/terminal` tags and live in `.agents/decisions/android/terminal.md`: D4 and D62.

## Active

### D1: Retained file-copy work is separate from activity callbacks

- **What**: android/file-import owns the retained file-import helper, while picker callbacks, final authorization and result handling remain in android/terminal.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14; 39e55b4b
