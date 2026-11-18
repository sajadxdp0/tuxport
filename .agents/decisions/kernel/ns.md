# Namespaces (kernel/ns) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/ns"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Namespace decisions made before kernel/ns was split from kernel/process keep their `kernel/process` tags and live in `.agents/decisions/kernel/process.md`: D12, D13, D28, D40 and the id-map half of D32.

## Active

### D1: kernel/ns is split out of kernel/process
- **What**: kernel/ns owns `nsproxy.rs`, `pidns.rs`, `syscall/ns.rs`, the unshare section of `syscall/proc.rs`, the time-namespace offsets in `syscall/misc.rs` and `UserNs` in `cred.rs`: kinds and creation, limits, setns, user, pid and time namespaces and namespace files, with the invariant that IDs shown outward are translated.
- **Why**: The namespace mechanism has its own files and is the hub the mount, netns and ipc maps point to, and the process map could no longer be read in one Read.
- **Tradeoffs**: What each namespace holds stays in kernel/mount, kernel/netns, kernel/ipc and kernel/cgroup, with synthetic views in kernel/procfs, so a namespace change usually reads two maps.
- **Watch out**: A new boundary that shows pids or IDs without translating them, now that the invariant lives in this map only.
- **Source**: b320f8e; cf029f9a; d053be24; 39e55b4b
