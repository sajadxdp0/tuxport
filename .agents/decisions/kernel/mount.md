# Mounts (kernel/mount) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/mount"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Mount decisions made before kernel/mount was split from kernel/vfs keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D2, D4, D6–D8, D10, D30, D32, D35, D48, D52 and D53.

## Active

### D1: kernel/mount is split out of kernel/vfs, with the boundary inside two files

- **What**: kernel/mount owns `syscall/mount.rs` except vfs's tmpfs and overlay option parsing and the overlay node sweep, plus the mount table in `vfs/mod.rs` (`Mount`, `Mounts`, `MntNs`, `Propagation`, `Index`, `GroupAlloc` and the `Vfs` methods that add, copy, move, propagate, unmount and collect mounts and namespaces).
- **Why**: The mount table, mount syscalls, propagation and mount namespaces made up a large part of the vfs map, while most VFS changes never touch them.
- **Tradeoffs**: The boundary runs inside `vfs/mod.rs` and `syscall/mount.rs`, and mount-flag checks stay in kernel/vfs, so read-only or remount changes usually read both maps; cgroup mount adapters remain in kernel/procfs and tree/interface policy in kernel/cgroup.
- **Watch out**: A mount-table method added in the vfs part of `vfs/mod.rs` needs the registry's description updated, or it falls between the two maps.
- **Source**: d2b1dc3; cf029f9a; d053be24; 39e55b4b

### D2: Pre-split mount decisions stay in the vfs snapshot, listed here and in the map

- **What**: Decisions tagged kernel/mount are distilled into this snapshot, while the mount decisions made before the split (vfs D2, D4, D6–D8, D10, D30, D32, D35, D48, D52, D53) keep their kernel/vfs tags and IDs in the vfs snapshot, listed by this snapshot's preamble and the kernel/mount map's See line.
- **Why**: Those decisions are indexed by their kernel/vfs tags, yet readers of the mount map still need to find them, and the map's See line had named a mount snapshot that did not yet exist.
- **Tradeoffs**: The list has to follow later vfs distills that add or rewrite mount entries, and mount history stays split between two snapshots.
- **Watch out**: Low risk; a new mount decision tagged kernel/vfs instead of kernel/mount would land in the wrong snapshot.
- **Source**: a536142, 49bb4f6

### D3: The kernel/mount map is verified on its own

- **What**: The map was fully remapped (five research concerns, 23 claims checked by four independent verifiers) instead of carrying facts verified as part of kernel/vfs, and adds bug items for detached mounts outliving their last use, anonymous-namespace mounts in propagation, and further 6.1 fidelity gaps.
- **Why**: Facts inherited through the split had not been checked on their own and could have been dropped or distorted.
- **Tradeoffs**: The 6.1 behaviour behind the fidelity gaps and the propagation bug rests on knowledge of the kernel, not 6.1 source, and stays under To verify.
- **Watch out**: Lazily unmounted mounts, unattached open_tree clones and mounts under removed mount points can hold overlay, FUSE, procfs and tmpfs instances until an unrelated collection.
- **Source**: e09877d (bug list since updated by 0e4d261)

### D4: A remount without `MS_BIND` needs `CAP_SYS_ADMIN` over the instance's owner

- **What**: Each mount records the user namespace owning its instance (`Mount::owner`, Linux's `s_user_ns`, copied with the mount and set by `new_superblock` or `set_owner`: the mounter's for tmpfs, devpts, overlay and a new FUSE connection, the pid namespace's owner for procfs, the IPC namespace's for mqueue, the initial one for sysfs and cgroup2 as Linux's `fc->global` has it, and a FUSE connection's own when it is mounted again), and a remount without `MS_BIND` needs `CAP_SYS_ADMIN` over it (`EPERM`) after parsing tmpfs options and the locked-flags check and before the writer check, as 6.1's `do_remount` does.
- **Why**: A remount without `MS_BIND` changes the instance for every mount of it (kernel/vfs D53) yet asked only for `CAP_SYS_ADMIN` over the caller's mount namespace owner, so root of a user namespace with its own mount namespace could make the boot root filesystem read-only for every namespace or resize a shared tmpfs.
- **Tradeoffs**: Each mount holds a strong reference to its owner, keeping a user namespace alive and counted against `max_user_namespaces` while a mount of an instance it owns exists, as Linux's superblock does, and the owners of sysfs, cgroup2 and the store follow knowledge of 6.1's fs_context code, observed natively only for sysfs.
- **Watch out**: A new way of creating a mount of a new instance must record its owner, or remounting it without `MS_BIND` needs `CAP_SYS_ADMIN` in the initial namespace, and container setups that remounted a shared instance without `MS_BIND` from a user namespace now get `EPERM`, as on Linux.
- **Source**: 0e4d261

### D5: Read-only remounts inspect detached writers and deleted holders

- **What**: `sb_busy` checks writers across attached and detached mounts plus deleted store/overlay files held by fds, Unix messages, loops, task roots/cwds or executables, excluding memfds; bind remounts and mount_setattr check writers only.
- **Why**: Remounts missed deleted files and detached writers, while unrelated memfds incorrectly blocked remounting the root.
- **Tradeoffs**: Each remount scans holders, and mapping-only holders and deleted FUSE files still do not count.
- **Watch out**: Shutdown may now need to kill holders of deleted files before remounting read-only, and memfd identification must follow future storage changes.
- **Source**: 01bfd882; 20137e4c; d053be24; 39e55b4b
