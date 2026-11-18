# Extended attributes (kernel/xattr) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/xattr"`
>
> Last distilled: 2026-11-11 (HEAD = c1a37da)
>
> Extended-attribute decisions made before kernel/xattr was split from kernel/vfs keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D38 and D50.

## Active

### D1: kernel/xattr is split out of kernel/vfs
- **What**: kernel/xattr owns `syscall/xattr.rs`: name prefixes and permissions, POSIX ACLs stored but not enforced and `security.capability`'s checks and conversions, while where values live is kernel/store's, as the file's doc comment now says.
- **Why**: `syscall/xattr.rs` is self-contained and called only from the dispatcher, and the vfs map was still 49.2 KB after pipes left.
- **Tradeoffs**: Overlay and FUSE xattrs are described in their own maps.
- **Watch out**: The map keeps open the unenforced-ACL bug and the question of `ENODATA` on synthetic nodes.
- **Source**: 5f327de
