# Machine lifecycle and virtual init (kernel/boot) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/boot"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions made before kernel/boot was split from kernel/process retain their `kernel/process` tags in `.agents/decisions/kernel/process.md`: D9, D35, D53, the host-stop part of D30 and the host-spawn part of D43; D16 retains the process-reaping contract.

## Active

### D1: Boot owns machine startup, host spawn and virtual init

- **What**: kernel/boot owns machine lifecycle types and methods, inittab, INIT and reboot.
- **Why**: Startup, host-launched programs and PID 1 policy have a lifecycle separate from general task execution.
- **Tradeoffs**: Shared kernel state, general reaping, waits, statistics and console behavior stay with their existing owners; host CLI launch integration belongs to harness/cli.
- **Watch out**: Keep spawn registration and worker teardown ordered, and retain the inherited Android poweroff question.
- **Source**: 8a17e4a9; d053be24; 60790c14; 39e55b4b
