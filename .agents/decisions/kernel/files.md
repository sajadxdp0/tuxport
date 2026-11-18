# Open files (kernel/files) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/files"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions on open files made before kernel/files was split from kernel/vfs keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D5, D11, D17, D22, D24, D26, D27, D29, D33, D34, D41, D44, D45, the owner half of D25, the `open_pseudo` half of D19 and the file I/O halves of D36 and D37; the pre-split fd reservation decision is D55.

## Active

### D1: kernel/files owns open descriptions and file operations

- **What**: kernel/files covers `fd.rs` except eventfd/timerfd, plus opening, data I/O, lseek, syncing, truncation, fallocate and shared file helpers in `syscall/fs.rs`, excluding the registered overlay, lock and pipe portions.
- **Why**: Open files occupied about half the oversized VFS map and change largely apart from path resolution and metadata.
- **Tradeoffs**: The split inherits the parent’s verification and keeps earlier decisions in kernel/vfs.
- **Watch out**: Open and write changes must also read kernel/vfs for resolution, mount flags and tmpfs accounting, kernel/cgroup for device checks and observation, and kernel/procfs for synthetic opens.
- **Source**: ce193253; 20137e4c; cf029f9a; d053be24; 39e55b4b

### D2: Reopens retain their mount and normal open checks

- **What**: `StoreOpen` routes reopens through open checks on their original mount with the path and IN_OPEN event, keeps reopened memfds as writers, and lets truncate through fd links use the open handle for deleted files and memfds.
- **Why**: Procfs reopens bypassed mount and open restrictions, while truncating deleted files or memfds through fd links did nothing.
- **Tradeoffs**: Reopens now expose normal EROFS, O_NOATIME, ETXTBSY and lease errors, and writable memfd reopens block execution.
- **Watch out**: Container runtimes reopening through `/proc/self/fd` can now receive errors they previously bypassed.
- **Source**: 01bfd882; 20137e4c; d053be24; 39e55b4b

### D3: File I/O preserves position ownership and partial progress

- **What**: Writes acquire `pos_busy` before checks and apply RLIMIT_FSIZE once per call, rollbacks are bounded by actual movement, copy_data returns progress, ftruncate waits without a held borrow, and make_private rehomes only held or COW objects.
- **Why**: Chunked writes, device reads and truncation could miscount progress, signal spuriously, deadlock or leak host fds.
- **Tradeoffs**: The existing lseek race outside `pos_busy` remains.
- **Watch out**: New I/O paths must preserve the same position, borrow and partial-progress rules.
- **Source**: 01bfd882; 20137e4c; d053be24; 39e55b4b

### D4: sendfile shares directional splice checks

- **What**: sendfile requires a seekable, splice-readable input and a splice-writable output unless the output is a pipe, using kernel/pipe’s predicates.
- **Why**: A separate permissive kind list returned the wrong errors for devices and unsupported outputs.
- **Tradeoffs**: Unsupported ends fail before reading, and FUSE inputs use the chunked fallback.
- **Watch out**: Programs expecting ENOSPC from sendfile into `/dev/full` now receive EINVAL.
- **Source**: 01bfd882; 20137e4c; d053be24; 39e55b4b

### D5: Replacing an fd closes it in the target process context

- **What**: The shared `close_replaced` helper releases locks, mqueue notifications, FUSE flush ownership and Unix references using the target table and tgid, including seccomp ADDFD.
- **Why**: ADDFD previously closed the target’s old fd as the supervisor.
- **Tradeoffs**: The replacement paths share an additional helper.
- **Watch out**: Future fd replacement paths must use the same target-context close.
- **Source**: 01bfd882; 20137e4c; d053be24; 39e55b4b
