# Clocks, sleeps and timers (kernel/time) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/time"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions made before kernel/time was split from kernel/process retain their `kernel/process` tags in `.agents/decisions/kernel/process.md`: D11, D15, D19, D25, D54 and the periodic-check part of D30; D6 and D52 retain the fast-path and deadline-handover contracts.

## Active

### D1: Time owns deadlines, clocks, timers and periodic checks

- **What**: kernel/time owns clock and sleep handlers, deadline helpers, interval and POSIX timers, load averages and the timer worker with stop_checks/resume_checks.
- **Why**: Time handling forms a distinct maintenance boundary within the oversized process map.
- **Tradeoffs**: CPU accounting, generic waits, fast-path dispatch, time namespaces and timerfd retain their other owners.
- **Watch out**: Preserve deadline handover and the idle handshake; the exited-leader clock debt and Linux verification residual are inherited, not independently reverified.
- **Source**: 8a17e4a9; d053be24; 39e55b4b
