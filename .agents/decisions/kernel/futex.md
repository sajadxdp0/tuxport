# Futexes (kernel/futex) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/futex"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Futex decisions made before kernel/futex was split from kernel/process keep their `kernel/process` tags and live in `.agents/decisions/kernel/process.md`: D26, D44 and the futex halves of D32, D33 and D37.

## Active

### D1: kernel/futex is split out of kernel/process
- **What**: kernel/futex owns `futex.rs`, the futex and robust-list handlers in `syscall/misc.rs` and the futex call of `syscall/fast.rs`: keys, the queue the BKL and fast paths share, the commands with `FUTEX_WAKE_OP` and `futex_waitv`, PI locks and their hand-over at exit, and robust lists, every kernel write to a futex word going through `writable_word`.
- **Why**: The process map was still 52.4 KB after exec and ns left, and futexes are self-contained, as Linux's `kernel/futex` is.
- **Tradeoffs**: Fast-path waits and deadline handover stay in kernel/process, while Expiry and clock jumps belong to kernel/time, so timed futex changes read both maps.
- **Watch out**: Low risk beyond the small map going stale.
- **Source**: b320f8e; 8a17e4a9; d053be24; 39e55b4b
