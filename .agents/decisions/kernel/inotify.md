# inotify (kernel/inotify) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/inotify"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> inotify decisions made before kernel/inotify was split from kernel/vfs keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D13, D46 and D51.

## Active

### D1: kernel/inotify is split out of kernel/vfs
- **What**: kernel/inotify owns `syscall/inotify.rs`: instances, watches and per-user limits, the registry keyed by store inode or by overlay, FUSE or cgroup2 node key, the hooks at fsnotify's call sites, the `Opened` record and the replay of the Files app's and cgroup2 files' changes; the file's doc comment now says cgroup2 files do get events (through `kernfs_modified`).
- **Why**: inotify is the largest of the parts moved out of vfs, with its own registry, limits and replay of outside changes, and the vfs map (99 KB) could not be read in one Read.
- **Tradeoffs**: Hooks span vfs and fuse, cgroup change producers belong to kernel/cgroup, and procfs still owns limits and fdinfo, so producer changes consult both maps.
- **Watch out**: A filesystem change added without its hook, now that the hook list lives in another map.
- **Source**: cc160c1; cf029f9a; d053be24; 39e55b4b

### D2: A file or parent watch keeps a cgroup event observation alive

- **What**: When an open fd no longer holds the observation counter, probe the exact cgroup file key and its parent directory key.
- **Why**: Copying unrelated watch keys made checkpoint work grow with the entire registry.
- **Tradeoffs**: A dirty observation without an open fd costs up to two watch lookups.
- **Watch out**: File-only and directory-only watches must each preserve the observation.
- **Source**: 0f5c5132
