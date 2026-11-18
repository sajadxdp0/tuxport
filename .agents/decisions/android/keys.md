# Extra keys (android/keys) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/keys"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions on android/keys made before it was split from android/terminal keep their `android/terminal` tags and live in `.agents/decisions/android/terminal.md`: D4, D12, D14, D15 and D19–D23.

## Active

### D1: Extra-key views own their layouts, gestures and modifiers

- **What**: android/keys owns extra-key rendering, gestures, layouts and shared indicator helpers, while activity integration remains in android/terminal.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14; 39e55b4b

### D2: The SYM page keeps percent and caret in their original positions

- **What**: Both layouts restore ^ and %, with % above SYM in two rows and after & in one row.
- **Why**: The IME already makes ! and @ easy to enter.
- **Tradeoffs**: Those two characters again require the IME or another input source.
- **Watch out**: The combined revert received no new build or device check, and future symbol selection is open.
- **Source**: 34df148a, 5576cc1d, 7c3725c1, 718d7f26, d3723ec7

### D3: Modifier labels and indicators use the shared terminal palette

- **What**: Apply terminalControlColor to ordinary and active labels and indicators, retaining the locked-modifier and SYM drawing rules.
- **Why**: One-shot modifier state was hard to distinguish from ordinary labels.
- **Tradeoffs**: One-shot state still relies on color alone.
- **Watch out**: Check one-shot and locked modifiers and the alternate page under device palettes.
- **Source**: 71e94b55

## Superseded

- ~~The ! replacement for % in 7c3725c1 and the @ replacement for ^ in 718d7f26 (inherited android/terminal D15)~~ → both reverted by **D2** in d3723ec7 (2026-11-16).
- ~~Unmodified primary color for active labels and indicators in android/terminal D20~~ → replaced by **D3** in 71e94b55 (2026-11-17), retaining indicator geometry.
