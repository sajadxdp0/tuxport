# Native AIO (kernel/aio) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/aio"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Native AIO decisions made before kernel/aio was split from kernel/vfs keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D42.

## Active

### D1: kernel/aio is split out of kernel/vfs
- **What**: kernel/aio owns `syscall/aio.rs`: io_setup to io_pgetevents, contexts per thread group and their `aio-nr` charge, synchronous I/O at submission, pending `IOCB_CMD_POLL`s and context release at execve and exit.
- **Why**: Native AIO is self-contained (one file with its own contexts and limits) and changed on its own, and the vfs map could no longer be read in one Read.
- **Tradeoffs**: Pending poll completion uses kernel/poll readiness and kernel/time's check_aio_polls, while exit cleanup stays in kernel/process.
- **Watch out**: Low risk beyond the small map going stale.
- **Source**: cc160c1; 8a17e4a9; d053be24; 39e55b4b
