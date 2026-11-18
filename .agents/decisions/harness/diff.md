# Differential and upstream suites (harness/diff) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: harness/diff"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions on harness/diff made before it was split from harness keep their `harness` tags and live in `.agents/decisions/harness.md`: D1–D3, D5, D9, D11–D14, D17–D20, D23, D27, D29, D31, D33, D34 and D36–D41.

## Active

### D1: Differential suites own runners, cases and recorded deviations

- **What**: harness/diff owns differential and upstream runners, case inputs and recorded test deviations, while implementation policy remains in the kernel maps.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14; 39e55b4b

### D2: Cgroup watch tests avoid registering an event-file reader

- **What**: Test inherited freeze with separate file and directory watches, then retain a directory watch across pids enable and re-enable and require IN_MODIFY after each denied fork.
- **Why**: Opening an event file for reading could mask a broken watch lifetime or missing controller registration.
- **Tradeoffs**: The directory cases use O_PATH to pin the child dentry for comparison with native Linux.
- **Watch out**: A never-looked-up dentry can behave differently on Linux 6.8, and a notification delayed beyond two seconds fails the pids case.
- **Source**: 0f5c5132, b65799c8
