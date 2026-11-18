# Pipes and splicing (kernel/pipe) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/pipe"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Pipe and splice decisions made before kernel/pipe was split from kernel/vfs keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D31 and D54.

## Active

### D1: kernel/pipe is split out of kernel/vfs
- **What**: kernel/pipe owns `pipe.rs`, `pipe2`, `open_fifo`, the pipe-size fcntl commands, `pipe_read` and `pipe_write`, and the splice block of `syscall/fs.rs` from `splice` to `wait_splice_done`, with the pipe and splice rules and the two splice invariants.
- **Why**: Pipes and splice are Linux's `fs/pipe.c` and `fs/splice.c` and are changed on their own, and the vfs map was still 54.3 KB after overlay and locks left.
- **Tradeoffs**: A splice into or out of a file or socket uses kernel/files's read and write paths, so such changes read both maps.
- **Watch out**: Code that takes data out of a pipe without waiting for `wait_splice_done`, now that the rule lives here.
- **Source**: 5f327de; ce193253 (module boundary); 20137e4c; d053be24; 39e55b4b

### D2: splice and sendfile share directional support predicates

- **What**: `splice_reads` and `splice_writes` distinguish device directions, exclude `/dev/kmsg` and `/dev/loop-control` entirely, and serve both splice and sendfile.
- **Why**: One kind list incorrectly permitted reading `/dev/null` and writing `/dev/full` through splice.
- **Tradeoffs**: Procfs and sysfs remain broadly treated as splice-capable, including per-process files Linux rejects.
- **Watch out**: The `/dev/zero` and `/dev/full` read behavior follows native 6.8 and may differ from the 6.1 contract.
- **Source**: 01bfd882; 20137e4c; d053be24; 39e55b4b
