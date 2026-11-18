# Cgroup v2 and device BPF (kernel/cgroup) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/cgroup"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions made before kernel/cgroup was split from kernel/procfs retain their `kernel/procfs` tags in `.agents/decisions/kernel/procfs.md`: D4–D6, D9, D24, D26, D37, D41 and the cgroup parts of D1–D3, D13, D25, D31 and D38.

## Active

### D1: Cgroups and device BPF share one policy module

- **What**: kernel/cgroup owns cgroup.rs, bpf.rs and syscall/bpf.rs, with syscall/exit observation callers in kernel/process and timer callers in kernel/time.
- **Why**: The tree and device programs form a coherent subsystem distinct from synthetic file adapters.
- **Tradeoffs**: Node adapters, namespace mechanics, syscall hooks, freezer waits and OOM selection remain in their existing modules.
- **Watch out**: The split inherits verified facts, Linux-version uncertainty and device-filter gaps; preserve every observation caller and do not treat boundary checks as a new full verification.
- **Source**: cf029f9a, 8a17e4a9; d053be24; 39e55b4b

### D2: Only dirty cgroup event files are compared at checkpoints

- **What**: Mutations mark affected observations dirty and set a per-kernel pending flag, and checkpoints refresh poll counters before delivering inotify events.
- **Why**: Regenerating watched event files after unrelated syscalls lengthened BKL holds.
- **Tradeoffs**: A dirty pass still scans observations, and a closed observation may retain its snapshot until a relevant change or a new observer.
- **Watch out**: Every mutation affecting a reported value needs a dirty mark, and no-observer timings also depend on borrow accounting in checking builds (kernel/process D58).
- **Source**: affb1e2d, 0f5c5132; b65799c8

### D3: Existing directory watches follow controller visibility

- **What**: Enabling pids or memory registers newly visible event files for watched child directories, and disabling a controller retires its observation state.
- **Why**: A watch installed before enablement otherwise missed later changes.
- **Tradeoffs**: Controller enablement rechecks visible files in watched child directories.
- **Watch out**: Native directory notifications can differ for a child event file whose dentry has never been instantiated.
- **Source**: b65799c8

## Superseded

- ~~Unconditional content comparisons for observed cgroup files in affb1e2d (pre-split kernel/procfs D38)~~ → replaced by **D2** in 0f5c5132 (2026-11-17).
