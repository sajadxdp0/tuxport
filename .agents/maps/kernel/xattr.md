# kernel/xattr Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/xattr.md` for the paired decision history (decisions from before the split are D38 and D50 in `.agents/decisions/kernel/vfs.md`).
>
> Verified: 2026-11-11 (split out of the kernel/vfs full mapping of 2026-11-10; the boundary was checked against the source at the split)

## Responsibilities

`kernel/xattr` is extended attributes (6.1's `fs/xattr.c` and `fs/posix_acl.c`):

- the setxattr, getxattr, listxattr and removexattr families (by path, by path without following
  the last link, by fd);
- name prefixes, sizes and the permission rules of each namespace (`xattr_permission`);
- `system.*` as the two POSIX ACL attributes: checked, stored with kernel IDs, shown in the reader's
  user namespace, an access ACL of only the base entries turned into a chmod;
- `security.capability`: its checks on write and conversion between v2 and v3 for each reader.

Root: `core/kernel/src/syscall/xattr.rs`.

Outside the module: storage of the values (`kernel/store`); the overlay's own attributes and
copy-up before a change (`kernel/overlay`); xattrs of FUSE files (`kernel/fuse`); capability
checks and `FileCaps` (`kernel/cred`); path resolution (`kernel/vfs`); fd lookup (`kernel/files`);
`IN_ATTRIB`
(`kernel/inotify`).

## Key types

- `Target` — `Path(address, follow)` or `Fd(fd)`, the three forms of each call.
- `AclEntry` — one POSIX ACL entry as (tag, permissions, ID).

## Public entry points

- **Syscalls** (from the dispatcher only): `setxattr`, `getxattr`, `listxattr`, `removexattr`,
  each with its `l` and `f` forms.

## Data flow / lifecycle

### Extended attributes

- Names need a `security.`, `system.`, `trusted.` or `user.` prefix (`EOPNOTSUPP`); bad name
  lengths `ERANGE`; values over 64 KiB `E2BIG`. Unprivileged `trusted.*` reads `ENODATA`, writes
  `EPERM` and is hidden from listings. `user.*` is for regular files and directories only
  (elsewhere reads `ENODATA`, writes `EPERM`), needs read or write permission, and on a sticky
  directory only its owner may write it (`xattr_permission`); writing `security.*` other than the
  capability needs `CAP_SYS_ADMIN`.
- `system.*` holds only `system.posix_acl_access` and `system.posix_acl_default` (any other
  `EOPNOTSUPP`). Setting or removing follows `posix_acl_xattr_set` and `set_posix_acl`: `EROFS`,
  then a malformed value `EINVAL` (short header, a length not header plus 8-byte entries, an
  unknown tag, a named user or group unmapped in the caller's namespace) or `EOPNOTSUPP` (version
  other than 2), a symlink `EOPNOTSUPP`, a default ACL on a non-directory `EACCES` (removing it
  succeeds), not owner nor `CAP_FOWNER` `EPERM`, an ACL out of `posix_acl_valid`'s order or with
  bits beyond rwx `EINVAL`; an empty ACL and removing a missing one succeed;
  `XATTR_CREATE`/`XATTR_REPLACE` are ignored. An access ACL of only owner, group and other entries
  is a chmod (setgid dropped as `posix_acl_update_mode` does) and is not stored; others are stored
  with named IDs as kernel IDs and read in the reader's namespace (unmapped as -1). Reads need
  nothing; symlinks have none (`EOPNOTSUPP`).
- `security.capability` (`cap_convert_nscap`, `cap_inode_getsecurity`): writes accept v2/v3 (the
  magic checked under a mask, so stray flag bits pass) and need `CAP_SETFCAP` over the file in the
  writer's namespace (owner and group mapped there; removal too); a v2 written with
  `CAP_SETFCAP` in the initial namespace is stored as is, anything else as a v3 whose rootid is
  the kernel ID of the writer's namespace root (or the rootid given; `EINVAL` unmapped). Reads: a
  root mapped to a nonzero ID in the reader's namespace gives a v3 with that rootid, a root owning
  the reader's namespace (or an ancestor's) a v2, any other `EOVERFLOW`. A non-empty value neither
  v2 nor v3 is `EINVAL` at write; an empty one is stored unchecked and reads as `EINVAL` (6.1
  skips the conversion for size 0).
- Synthetic nodes: get `ENODATA`, set/remove `EOPNOTSUPP` (`EROFS` first on ro), empty list;
  pipes and sockets `EOPNOTSUPP`.
- Overlay nodes are copied up before a change and read through their topmost inode, and the
  overlay's own attributes are refused (`kernel/overlay`); FUSE files ask the daemon
  (`kernel/fuse`). A successful change reports `IN_ATTRIB` (`attrib`).

## Dependencies (inbound / outbound)

### Inbound

- The syscall dispatcher.

### Outbound

- `kernel/store` (the values), `kernel/vfs` (resolution, mount flags), `kernel/overlay` (copy-up,
  own attributes), `kernel/fuse` (the daemon's xattr requests), `kernel/cred` (`FileCaps`,
  capability checks, ID mapping), `kernel/inotify` (`IN_ATTRIB`).

## Invariants and gotchas

- IDs inside stored ACLs and v3 capabilities are kernel IDs; every read converts them for the
  reader's namespace, so code that copies a value between files must copy the stored form.

## Confirmed bugs / technical debt

- **POSIX ACLs are not enforced:** named users, groups and masks are stored but never consulted,
  the mode does not take the mask as its group bits, chmod leaves a stored ACL alone, and new
  files never inherit a default ACL.

## Open questions

- Is xattr `ENODATA` on every synthetic node intended? Linux's procfs and devpts have no xattr
  handlers and answer `EOPNOTSUPP`.

## To verify

- None beyond the full mapping's: the 6.1 comparisons here were checked from knowledge of the
  kernel, not against its source.
