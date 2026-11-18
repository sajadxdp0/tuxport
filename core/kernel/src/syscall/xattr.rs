//! Extended attributes (the kernel/xattr map): setxattr, getxattr, listxattr, removexattr, and the
//! variants that act on the symlink itself (`l`) and on an fd (`f`).
//!
//! Extended attributes live in the inode store's database (the kernel/store map). Nodes of
//! synthetic filesystems (/proc, /dev/pts) have no extended attributes, and files not in a
//! filesystem, such as pipes and sockets, do not support them. Permissions are checked as in
//! Linux's `xattr_permission` and commoncap's hooks ([`permission`]); program file capabilities,
//! `security.capability`, are format-checked on write and converted on read by
//! [`FileCaps::xattr_for_reader`]. Of `system.*` only the POSIX ACL attributes exist, as on ext4
//! and tmpfs ([`change_acl`]): checked, kept with kernel IDs and shown in the reader's user
//! namespace, but not enforced.

use super::fs::{AT_FDCWD, attrib_at, attrib_fd, lookup_at};
use super::{SysResult, inotify, read_path};
use crate::cred::{Cred, FileCaps, MAY_READ, MAY_WRITE, XATTR_NAME_CAPS, cap};
use crate::errno::Errno;
use crate::fd::oflags;
use crate::kernel::Cx;
use crate::uaccess::UserSlice;
use crate::vfs::Node;
use crate::vfs::store::{Ino, Stat, mode};

const XATTR_CREATE: u32 = 1;
const XATTR_REPLACE: u32 = 2;
/// Maximum name length.
const XATTR_NAME_MAX: usize = 255;
/// Maximum value length.
const XATTR_SIZE_MAX: usize = 65536;
/// Names must carry one of these namespace prefixes.
const NAMESPACES: [&[u8]; 4] = [b"security.", b"system.", b"trusted.", b"user."];
/// The only `system.*` attributes (6.1's `posix_acl_access_xattr_handler` and
/// `posix_acl_default_xattr_handler`, the ones ext4 and tmpfs have).
const ACL_ACCESS: &[u8] = b"system.posix_acl_access";
const ACL_DEFAULT: &[u8] = b"system.posix_acl_default";

/// The object operated on.
#[derive(Clone, Copy)]
pub(super) enum Target {
    /// A path, and whether to follow the last symlink.
    Path(u64, bool),
    Fd(i32),
}

pub(super) fn setxattr(
    cx: &mut Cx<'_>,
    target: Target,
    name: u64,
    value: u64,
    size: usize,
    flags: u32,
) -> SysResult {
    if flags & !(XATTR_CREATE | XATTR_REPLACE) != 0 {
        return Err(Errno::EINVAL);
    }
    let name = read_name(cx, name)?;
    if size > XATTR_SIZE_MAX {
        return Err(Errno::E2BIG);
    }
    let mut v = vec![0u8; size];
    cx.mm().read(UserSlice::new(value, size), &mut v)?;
    let node = node_of(cx, target)?;
    if let Node::Fuse(_, f) = node {
        return fuse_change(cx, &node, f, &name, Some((&v, flags)));
    }
    if name.starts_with(b"system.") {
        return change_acl(cx, target, node, &name, Some(&v));
    }
    let ino = writable_ino(cx, node, &name)?;
    let vfs = cx.vfs()?;
    let store = vfs.store();
    // Program file capabilities: only v2 and v3 are accepted, CAP_SETFCAP is needed over the file
    // (in the writer's namespace, with its owner and group mapped there), and the value is stored
    // with the root of the writer's namespace (cap_convert_nscap, before the other checks; Linux
    // does not check when the length is 0)
    if name == XATTR_NAME_CAPS && size > 0 {
        if !FileCaps::valid_xattr(&v) {
            return Err(Errno::EINVAL);
        }
        let cred = cx.cred();
        if !cred.capable_wrt(&store.attrs(ino)?, cap::SETFCAP) {
            return Err(Errno::EPERM);
        }
        v = FileCaps::xattr_to_store(&v, &cred)?;
    }
    permission(cx, &store.attrs(ino)?, &name, true)?;
    store.set_xattr(
        ino,
        &name,
        &v,
        flags & XATTR_CREATE != 0,
        flags & XATTR_REPLACE != 0,
    )?;
    attrib(cx, target, ino);
    Ok(0)
}

pub(super) fn getxattr(
    cx: &mut Cx<'_>,
    target: Target,
    name: u64,
    value: u64,
    size: usize,
) -> SysResult {
    let name = read_name(cx, name)?;
    let node = node_of(cx, target)?;
    if let Node::Fuse(_, f) = node {
        let vfs = cx.vfs()?;
        super::fuse::xattr_permission(&vfs, &cx.cred(), f, &name, false)?;
        let size = size.min(XATTR_SIZE_MAX);
        return match crate::vfs::fuse::getxattr(cx, &vfs, f, Some(&name), size)? {
            Err(len) => Ok(len as u64),
            Ok(v) => reply(cx, value, size, &v),
        };
    }
    let ino = readable_ino(cx, node, &name)?;
    let vfs = cx.vfs()?;
    let st = vfs.store().attrs(ino)?;
    // Only the ACL attributes exist under system.*, and symlinks have none (posix_acl_xattr_get)
    if name.starts_with(b"system.") && (!is_acl(&name) || st.kind() == mode::S_IFLNK) {
        return Err(Errno::EOPNOTSUPP);
    }
    permission(cx, &st, &name, false)?;
    let mut v = vfs.store().get_xattr(ino, &name)?;
    if name == XATTR_NAME_CAPS {
        v = FileCaps::xattr_for_reader(&v, &cx.cred())?;
    }
    // ACL entries are kept with kernel IDs and shown in the reader's namespace
    // (posix_acl_fix_xattr_to_user: an unmapped one as -1)
    if is_acl(&name)
        && let Ok(mut entries) = acl_entries(&v)
    {
        let cred = cx.cred();
        for e in &mut entries {
            e.2 = match e.0 {
                ACL_USER => cred.user_ns.uid_map.borrow().up(e.2).unwrap_or(u32::MAX),
                ACL_GROUP => cred.user_ns.gid_map.borrow().up(e.2).unwrap_or(u32::MAX),
                _ => e.2,
            };
        }
        v = acl_encode(&entries);
    }
    reply(cx, value, size, &v)
}

pub(super) fn listxattr(cx: &mut Cx<'_>, target: Target, list: u64, size: usize) -> SysResult {
    // A FUSE daemon's list is passed on as it is (6.1's fuse_listxattr)
    if let Node::Fuse(_, f) = node_of(cx, target)? {
        let vfs = cx.vfs()?;
        vfs.fuse_permission(&cx.cred(), f, 0)?;
        let size = size.min(XATTR_SIZE_MAX);
        return match crate::vfs::fuse::getxattr(cx, &vfs, f, None, size)? {
            Err(len) => Ok(len as u64),
            Ok(v) => reply(cx, list, size, &v),
        };
    }
    let names = match node_of(cx, target)? {
        Node::Store(_, ino) => cx.vfs()?.store().list_xattr(ino)?,
        Node::Pseudo(..) | Node::Fuse(..) => Vec::new(),
        // The overlay's own attributes are not listed (Linux's ovl_listxattr)
        Node::Overlay(_, o) => {
            let vfs = cx.vfs()?;
            let ovl = vfs.overlay();
            let mut names = vfs.store().list_xattr(ovl.node(o).real())?;
            names.retain(|n| !ovl.is_private_xattr(o, n));
            names
        }
    };
    // As in Linux's trusted handler, trusted.* is not listed without CAP_SYS_ADMIN
    let trusted = cx.cred().capable(cap::SYS_ADMIN);
    let mut out = Vec::new();
    for n in names
        .into_iter()
        .filter(|n| trusted || !n.starts_with(b"trusted."))
    {
        out.extend_from_slice(&n);
        out.push(0);
    }
    reply(cx, list, size, &out)
}

pub(super) fn removexattr(cx: &mut Cx<'_>, target: Target, name: u64) -> SysResult {
    let name = read_name(cx, name)?;
    let node = node_of(cx, target)?;
    if let Node::Fuse(_, f) = node {
        return fuse_change(cx, &node, f, &name, None);
    }
    if name.starts_with(b"system.") {
        return change_acl(cx, target, node, &name, None);
    }
    let ino = writable_ino(cx, node, &name)?;
    let vfs = cx.vfs()?;
    permission(cx, &vfs.store().attrs(ino)?, &name, true)?;
    // Removing program file capabilities needs CAP_SETFCAP over the file (cap_inode_removexattr)
    if name == XATTR_NAME_CAPS
        && !cx
            .cred()
            .capable_wrt(&vfs.store().attrs(ino)?, cap::SETFCAP)
    {
        return Err(Errno::EPERM);
    }
    vfs.store().remove_xattr(ino, &name)?;
    attrib(cx, target, ino);
    Ok(0)
}

/// setxattr (`value` Some) or removexattr of a `system.*` attribute on a store or overlay node, as
/// 6.1's `posix_acl_xattr_set` and `set_posix_acl` do on ext4 and tmpfs: EROFS on a read-only
/// mount; names other than the two ACL attributes EOPNOTSUPP; a malformed ACL or one naming an ID
/// unmapped in the caller's namespace EINVAL (`posix_acl_from_xattr`, an empty one counting as a
/// removal); a symlink EOPNOTSUPP; a default ACL on anything but a directory EACCES (its removal
/// succeeds); not being the owner or capable over it (`CAP_FOWNER`) EPERM; an invalid ACL
/// EINVAL (`posix_acl_valid`). An access ACL that only restates the mode changes the mode, as a
/// chmod would, and is not stored (`posix_acl_update_mode`); any other is stored with kernel IDs
/// and changes nothing else: ACLs are not enforced, and the mode does not take the mask (that
/// would widen the owning group's access without the ACL's limits). Removing an ACL that is not
/// there succeeds, and `XATTR_CREATE` and `XATTR_REPLACE` are ignored, as Linux's ACL handlers
/// ignore them.
fn change_acl(
    cx: &mut Cx<'_>,
    target: Target,
    node: Node,
    name: &[u8],
    value: Option<&[u8]>,
) -> SysResult {
    let vfs = cx.vfs()?;
    if vfs.readonly(&node) {
        return Err(Errno::EROFS);
    }
    if !is_acl(name) {
        return Err(Errno::EOPNOTSUPP);
    }
    let cred = cx.cred();
    let acl = match value {
        Some(v) => acl_from_user(v, &cred)?,
        None => None,
    };
    let shown = match node {
        Node::Store(_, ino) => vfs.store().attrs(ino)?,
        Node::Overlay(_, o) => vfs.store().attrs(vfs.overlay().node(o).real())?,
        Node::Pseudo(..) | Node::Fuse(..) => return Err(Errno::EOPNOTSUPP),
    };
    if shown.kind() == mode::S_IFLNK {
        return Err(Errno::EOPNOTSUPP);
    }
    if name == ACL_DEFAULT && shown.kind() != mode::S_IFDIR {
        return if acl.is_some() {
            Err(Errno::EACCES)
        } else {
            Ok(0)
        };
    }
    if !cred.owns(&shown) {
        return Err(Errno::EPERM);
    }
    if let Some(e) = &acl {
        acl_valid(e)?;
    }
    let ino = writable_ino(cx, node, name)?;
    let store = vfs.store();
    match acl {
        // Only the owner, group and other entries: the mode says it all (posix_acl_equiv_mode);
        // the set-group-ID bit goes as posix_acl_update_mode drops it
        Some(e)
            if name == ACL_ACCESS
                && e.iter()
                    .all(|x| matches!(x.0, ACL_USER_OBJ | ACL_GROUP_OBJ | ACL_OTHER)) =>
        {
            let st = store.attrs(ino)?;
            let perm = |tag| u32::from(e.iter().find(|x| x.0 == tag).map_or(0, |x| x.1));
            let mut m = st.mode & 0o7000
                | perm(ACL_USER_OBJ) << 6
                | perm(ACL_GROUP_OBJ) << 3
                | perm(ACL_OTHER);
            if !cred.in_group(st.gid) && !cred.capable_wrt(&st, cap::FSETID) {
                m &= !mode::S_ISGID;
            }
            store.set_mode(ino, m)?;
            match store.remove_xattr(ino, name) {
                Ok(()) | Err(Errno::ENODATA) => {}
                Err(e) => return Err(e),
            }
        }
        Some(e) => store.set_xattr(ino, name, &acl_encode(&e), false, false)?,
        None => match store.remove_xattr(ino, name) {
            Ok(()) | Err(Errno::ENODATA) => {}
            Err(e) => return Err(e),
        },
    }
    attrib(cx, target, ino);
    Ok(0)
}

/// Whether `name` is one of the POSIX ACL attributes.
fn is_acl(name: &[u8]) -> bool {
    name == ACL_ACCESS || name == ACL_DEFAULT
}

// The tags of ACL entries (Linux's `ACL_USER_OBJ` and the rest)
const ACL_USER_OBJ: u16 = 0x01;
const ACL_USER: u16 = 0x02;
const ACL_GROUP_OBJ: u16 = 0x04;
const ACL_GROUP: u16 = 0x08;
const ACL_MASK: u16 = 0x10;
const ACL_OTHER: u16 = 0x20;
/// The ID of entries that name none.
const ACL_UNDEFINED_ID: u32 = u32::MAX;

/// One ACL entry: tag, permissions and ID.
type AclEntry = (u16, u16, u32);

/// An ACL attribute's entries as (tag, permissions, ID), read as `posix_acl_from_xattr` reads
/// them: a header (EINVAL when too short) of version 2 (EOPNOTSUPP otherwise), then 8-byte
/// entries with known tags (EINVAL otherwise).
fn acl_entries(v: &[u8]) -> Result<Vec<AclEntry>, Errno> {
    if v.len() < 4 {
        return Err(Errno::EINVAL);
    }
    if v[..4] != 2u32.to_le_bytes() {
        return Err(Errno::EOPNOTSUPP);
    }
    if !(v.len() - 4).is_multiple_of(8) {
        return Err(Errno::EINVAL);
    }
    v[4..]
        .chunks_exact(8)
        .map(|c| {
            let tag = u16::from_le_bytes([c[0], c[1]]);
            let perm = u16::from_le_bytes([c[2], c[3]]);
            let id = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
            match tag {
                ACL_USER | ACL_GROUP => Ok((tag, perm, id)),
                ACL_USER_OBJ | ACL_GROUP_OBJ | ACL_MASK | ACL_OTHER => {
                    Ok((tag, perm, ACL_UNDEFINED_ID))
                }
                _ => Err(Errno::EINVAL),
            }
        })
        .collect()
}

/// An ACL as the caller wrote it, its named users and groups taken from the caller's user
/// namespace to kernel IDs (posix_acl_fix_xattr_from_user; an unmapped one EINVAL); None for an
/// ACL without entries, which removes it.
fn acl_from_user(v: &[u8], cred: &Cred) -> Result<Option<Vec<AclEntry>>, Errno> {
    let mut entries = acl_entries(v)?;
    for e in &mut entries {
        e.2 = match e.0 {
            ACL_USER => cred.uid_in(e.2).ok_or(Errno::EINVAL)?,
            ACL_GROUP => cred.gid_in(e.2).ok_or(Errno::EINVAL)?,
            _ => e.2,
        };
    }
    Ok((!entries.is_empty()).then_some(entries))
}

/// 6.1's `posix_acl_valid`: permissions within rwx, and the entries in the order owner, named
/// users, owning group, named groups, mask, other, each of the owner, owning group and other once,
/// and a mask when there are named entries.
fn acl_valid(entries: &[AclEntry]) -> Result<(), Errno> {
    let mut state = ACL_USER_OBJ;
    let mut needs_mask = false;
    for &(tag, perm, _) in entries {
        if perm & !7 != 0 {
            return Err(Errno::EINVAL);
        }
        state = match (tag, state) {
            (ACL_USER_OBJ, ACL_USER_OBJ) => ACL_USER,
            (ACL_USER, ACL_USER) => {
                needs_mask = true;
                ACL_USER
            }
            (ACL_GROUP_OBJ, ACL_USER) => ACL_GROUP,
            (ACL_GROUP, ACL_GROUP) => {
                needs_mask = true;
                ACL_GROUP
            }
            (ACL_MASK, ACL_GROUP) => ACL_OTHER,
            (ACL_OTHER, ACL_OTHER) => 0,
            (ACL_OTHER, ACL_GROUP) if !needs_mask => 0,
            _ => return Err(Errno::EINVAL),
        };
    }
    if state == 0 {
        Ok(())
    } else {
        Err(Errno::EINVAL)
    }
}

/// The attribute form of ACL entries (`posix_acl_to_xattr`).
fn acl_encode(entries: &[AclEntry]) -> Vec<u8> {
    let mut v = 2u32.to_le_bytes().to_vec();
    for &(tag, perm, id) in entries {
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&perm.to_le_bytes());
        v.extend_from_slice(&id.to_le_bytes());
    }
    v
}

/// setxattr (`value` Some) or removexattr on FUSE node `id`: EROFS on a read-only mount, the
/// permission rules, then the daemon. `security.capability` is passed as it is (Linux converts
/// namespaced capabilities first).
fn fuse_change(
    cx: &mut Cx<'_>,
    node: &Node,
    id: crate::vfs::fuse::FuseId,
    name: &[u8],
    value: Option<(&[u8], u32)>,
) -> SysResult {
    let vfs = cx.vfs()?;
    if vfs.readonly(node) {
        return Err(Errno::EROFS);
    }
    super::fuse::xattr_permission(&vfs, &cx.cred(), id, name, true)?;
    super::fuse::set_xattr(cx, &vfs, id, name, value)?;
    Ok(0)
}

fn read_name(cx: &Cx<'_>, addr: u64) -> Result<Vec<u8>, Errno> {
    let name = cx.mm().read_cstr(addr, XATTR_NAME_MAX).map_err(|e| {
        if e == Errno::ENAMETOOLONG {
            Errno::ERANGE
        } else {
            e
        }
    })?;
    if name.is_empty() {
        return Err(Errno::ERANGE);
    }
    if !NAMESPACES.iter().any(|ns| name.starts_with(ns)) {
        return Err(Errno::EOPNOTSUPP);
    }
    Ok(name)
}

/// Finds the object's node; EOPNOTSUPP for files not in a filesystem.
fn node_of(cx: &Cx<'_>, target: Target) -> Result<Node, Errno> {
    match target {
        Target::Path(path, follow) => {
            let path = read_path(cx, path)?;
            lookup_at(cx, AT_FDCWD, &path, follow)
        }
        Target::Fd(fd) => {
            let file = cx.fds().get(fd)?;
            let f = file.borrow();
            if f.flags & oflags::O_PATH != 0 {
                return Err(Errno::EBADF);
            }
            f.node().ok_or(Errno::EOPNOTSUPP)
        }
    }
}

/// The inode whose extended attribute `name` is about to change: EROFS on a read-only mount
/// (Linux's `mnt_want_write`, before the other checks). In an overlay the attribute changes in
/// the upper layer, after a copy-up that needs the permission to change it on what the overlay
/// shows; the overlay's own attributes cannot be changed (EOPNOTSUPP, 6.1's
/// `ovl_own_xattr_set`).
fn writable_ino(cx: &Cx<'_>, node: Node, name: &[u8]) -> Result<Ino, Errno> {
    let vfs = cx.vfs()?;
    if vfs.readonly(&node) {
        return Err(Errno::EROFS);
    }
    if let Node::Overlay(_, o) = node {
        let real = {
            let ovl = vfs.overlay();
            if ovl.is_private_xattr(o, name) {
                return Err(Errno::EOPNOTSUPP);
            }
            ovl.node(o).real()
        };
        permission(cx, &vfs.store().attrs(real)?, name, true)?;
        return super::fs::copy_up(cx, &vfs, o, crate::vfs::overlay::CopyUp::All);
    }
    store_ino(node, Errno::EOPNOTSUPP)
}

/// The inode in the store; `pseudo` for nodes of synthetic filesystems.
fn store_ino(node: Node, pseudo: Errno) -> Result<Ino, Errno> {
    node.ino().ok_or(pseudo)
}

/// The inode whose attribute `name` is read: a store node's own, or the one behind an overlay
/// node, whose overlay keeps the attributes with its private prefix to itself (EOPNOTSUPP, as
/// 6.1's `ovl_own_xattr_get`); ENODATA for nodes of synthetic filesystems.
fn readable_ino(cx: &Cx<'_>, node: Node, name: &[u8]) -> Result<Ino, Errno> {
    match node {
        Node::Overlay(_, o) => {
            let vfs = cx.vfs()?;
            let ovl = vfs.overlay();
            if ovl.is_private_xattr(o, name) {
                return Err(Errno::EOPNOTSUPP);
            }
            Ok(ovl.node(o).real())
        }
        node => store_ino(node, Errno::ENODATA),
    }
}

/// Permission to read (`write` false) or write the extended attribute named `name`:
///
/// - `trusted.*` needs `CAP_SYS_ADMIN`; without it writes are EPERM and reads ENODATA (they are
///   invisible);
/// - `user.*` only on regular files and directories (otherwise writes are EPERM and reads
///   ENODATA); on a sticky directory only the owner may write; reads and writes need read and
///   write permission, respectively, on the file itself;
/// - `security.*` writes need `CAP_SYS_ADMIN` (the caller checks `CAP_SETFCAP` for
///   `security.capability`), reads are unrestricted;
/// - `system.*` (the ACLs) needs the owner or `CAP_FOWNER` to write ([`change_acl`] checks it
///   first), reads are unrestricted.
fn permission(cx: &Cx<'_>, st: &Stat, name: &[u8], write: bool) -> Result<(), Errno> {
    let cred = cx.cred();
    let hidden = if write { Errno::EPERM } else { Errno::ENODATA };
    if name.starts_with(b"trusted.") {
        return if cred.capable(cap::SYS_ADMIN) {
            Ok(())
        } else {
            Err(hidden)
        };
    }
    if name.starts_with(b"user.") {
        let dir = st.kind() == mode::S_IFDIR;
        if !dir && st.kind() != mode::S_IFREG {
            return Err(hidden);
        }
        if dir && st.mode & mode::S_ISVTX != 0 && write && !cred.owns(st) {
            return Err(Errno::EPERM);
        }
        return cred.permission(st, if write { MAY_WRITE } else { MAY_READ });
    }
    if write
        && name.starts_with(b"security.")
        && name != XATTR_NAME_CAPS
        && !cred.capable(cap::SYS_ADMIN)
    {
        return Err(Errno::EPERM);
    }
    if write && name.starts_with(b"system.") && !cred.owns(st) {
        return Err(Errno::EPERM);
    }
    Ok(())
}

/// An extended attribute changed: report inotify's IN_ATTRIB.
fn attrib(cx: &Cx<'_>, target: Target, ino: Ino) {
    match target {
        Target::Path(path, follow) if inotify::active(cx.kr()) => {
            if let Ok(path) = read_path(cx, path) {
                attrib_at(cx, ino, AT_FDCWD, &path, follow);
            }
        }
        Target::Path(..) => {}
        Target::Fd(fd) => attrib_fd(cx, fd),
    }
}

/// Writes the result back to the guest: with `size` 0 only the length is returned, and ERANGE if
/// the buffer is too small.
fn reply(cx: &Cx<'_>, buf: u64, size: usize, data: &[u8]) -> SysResult {
    if size == 0 {
        return Ok(data.len() as u64);
    }
    if size < data.len() {
        return Err(Errno::ERANGE);
    }
    cx.mm().write(UserSlice::new(buf, data.len()), data)?;
    Ok(data.len() as u64)
}
