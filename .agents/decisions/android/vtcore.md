# Native terminal implementation (android/vtcore) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/vtcore"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions made before android/vtcore was split from android/vt retain their `android/vt` tags in `.agents/decisions/android/vt.md`: D1, D3, D4 and the parsing side of D5–D7; earlier `android/terminal` decisions remain in `.agents/decisions/android/terminal.md`: D2, D5, D6, D8, D16, D17, D32, D33, D35–D39 and D42–D49.

## Active

### D1: Native state, locking and terminal protocols stay together

- **What**: android/vtcore owns VtNative.kt, all vt.rs and vt.c except registered interaction and frame/rendering sections, keeping prompt parsing, snapshots and compression beside shared terminal state.
- **Why**: Native implementation needs an explicit owner distinct from Kotlin session lifecycle.
- **Tradeoffs**: The map remains large because another split would duplicate shared-state and locking contracts; native runtime integration belongs to android/native and build configuration to android/build.
- **Watch out**: Assign every new vt.c section explicitly and preserve inherited device/protocol verification gaps; the 2026-11-11 mapping is inherited through android/vt, not independently repeated.
- **Source**: 81f60bef; d053be24; 60790c14; 39e55b4b

### D2: Resize output settles only after quiet parser-ground capture

- **What**: Grid changes mark output pending and subsequent nonempty feeds refresh its timestamp, with frame capture clearing it only after 32 ms of quiet at parser ground and without a synchronized-output hold.
- **Why**: A nonempty read can be a cursor update or incomplete synchronized-output prefix rather than a complete repaint.
- **Tradeoffs**: Settling adds a short delay without postponing resize or SIGWINCH.
- **Watch out**: Quiet time is a heuristic and later delayed output can follow an apparently settled burst.
- **Source**: 2e73a06a; 39e55b4b
