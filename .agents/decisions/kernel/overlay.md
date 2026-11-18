# Overlay filesystem (kernel/overlay) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/overlay"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Overlay decisions made before kernel/overlay was split from kernel/vfs keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D39, D40 and D49; those tagged `kernel/process` live in `.agents/decisions/kernel/process.md`: D45 and D46.

## Active

### D1: kernel/overlay is split out of kernel/vfs
- **What**: kernel/overlay owns `vfs/overlay.rs`, the overlay methods of `Vfs`, `open_overlay`, `follow_copy_up`, `copy_up`, `unlink_overlay`, `link_overlay` and `rename_overlay`, and the overlay options and node sweep in `syscall/mount.rs`: mounting and layers, lookup and node lifetime, merged readdir and stat, copy-up, namespace changes and opening.
- **Why**: overlayfs is the largest part of the VFS with its own file and Linux counterpart (`fs/overlayfs`), like kernel/fuse, and the vfs map could no longer be read in one Read.
- **Tradeoffs**: The invariants on `Node::ino` and node holds bind code in kernel/vfs too, which keeps only a pointer to them, and every overlay commit so far also changed the VFS core, so overlay changes read two maps.
- **Watch out**: A new file operation that skips the overlay rules because they now live in another map.
- **Source**: 5f327de; 01bfd882; 20137e4c; d053be24; 39e55b4b

### D2: Copy-up isolates lower files that can still change

- **What**: Every file copy-up uses `syscall::fs::copy_up`, copying data immediately under a live host writer or writable mapping and otherwise sharing while rehoming non-COW lower writers.
- **Why**: Unconditional sharing let later lower-file writes change the copied-up upper file.
- **Tradeoffs**: A live writer or writable mapping forces a full copy.
- **Watch out**: The writable-mapping check scans tasks; the superseded pre-split sharing rule is kernel/vfs D40, with related clone rules in D45.
- **Source**: df6ee70, 01bfd882; 20137e4c; d053be24; 39e55b4b

## Superseded
- ~~Always sharing the lower regular-file object on copy-up (df6ee70; kernel/vfs D40)~~ → replaced by **D2** in 01bfd882 (2026-11-12)
