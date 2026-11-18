//! File syscalls on FUSE nodes and open files (the kernel/fuse map): what 6.1's `fs/fuse/dir.c`
//! and `file.c` do for them, through requests to the daemon ([`crate::vfs::fuse`]). Each request
//! here changes something or needs the open file, so it is made directly with the BKL released;
//! what only a walk needs (lookups, attributes, links) comes from the cache or a retry.
//!
//! Permission follows `fuse_permission`: nobody but the processes the mount allows gets in
//! (`allow_other`), and only with `default_permissions` does the kernel check modes; otherwise the
//! daemon decides, and the kernel asks it only for access(2) and chdir (`FUSE_ACCESS`).

use std::rc::Rc;

use super::fs::encode_stat;
use super::inotify;
use crate::cell::RefCell;
use crate::cred::{Cred, MAY_EXEC, MAY_READ, MAY_WRITE};
use crate::errno::Errno;
use crate::fd::{FileKind, FileRef, oflags};
use crate::kernel::Cx;
use crate::mm::Fill;
use crate::vfs::fuse::{self, Args, Conn, Entry, FuseId, Handle, op};
use crate::vfs::store::{DirEntry, mode};
use crate::vfs::{MountId, Node, Vfs, ms};

/// What a read or write of an open FUSE file needs, taken out of it so no borrow is held while
/// waiting: the connection, node, node id, file handle, open flags, and whether the file has a
/// position (not `FOPEN_STREAM`).
pub(super) struct Io {
    conn: Rc<RefCell<Conn>>,
    id: FuseId,
    nodeid: u64,
    fh: u64,
    flags: u32,
    stream: bool,
}

impl Io {
    /// The I/O route of an open FUSE file.
    pub fn of(h: &Handle) -> Io {
        Io {
            conn: h.conn.clone(),
            id: h.id,
            nodeid: h.nodeid,
            fh: h.fh,
            flags: h.flags,
            stream: h.open_flags & fuse::fopen::STREAM != 0,
        }
    }

    fn handle(&self) -> (&Rc<RefCell<Conn>>, u64, u64, u32) {
        (&self.conn, self.nodeid, self.fh, self.flags)
    }
}

/// Whether an open FUSE file can seek (not `FOPEN_NONSEEKABLE` or `FOPEN_STREAM`).
pub(super) fn seekable(h: &Handle) -> bool {
    h.open_flags & (fuse::fopen::NONSEEKABLE | fuse::fopen::STREAM) == 0
}

// ---- Opening and creating ----

/// Opens existing FUSE node `id` reached through mount `mnt` (6.1's `may_open` and
/// `fuse_open_common`): the checks open makes on a store file, with permission as
/// [`Vfs::fuse_permission`] decides, then `FUSE_OPEN` or `FUSE_OPENDIR`. Without
/// `FUSE_ATOMIC_O_TRUNC`, `O_TRUNC` is a `FUSE_SETATTR` after the open. Device nodes, FIFOs and
/// sockets in a FUSE filesystem cannot be opened (ENXIO).
pub(super) fn open(
    cx: &mut Cx<'_>,
    cred: &Cred,
    vfs: &Vfs,
    (mnt, id): (MountId, FuseId),
    flags: u32,
) -> Result<FileKind, Errno> {
    let node = Node::Fuse(mnt, id);
    let accmode = flags & oflags::O_ACCMODE;
    let kind = vfs.fuse().get(id).ok_or(Errno::ENOTCONN)?.kind();
    let mnt_flags = vfs.mount_flags(&node).unwrap_or(0);
    let readonly = vfs.readonly(&node);
    let mut mask = super::fs::open_mask(flags);
    match kind {
        mode::S_IFLNK => return Err(Errno::ELOOP),
        mode::S_IFDIR
            if accmode != oflags::O_RDONLY || flags & (oflags::O_CREAT | oflags::O_TRUNC) != 0 =>
        {
            return Err(Errno::EISDIR);
        }
        mode::S_IFREG if flags & oflags::O_TRUNC != 0 => {
            if readonly {
                return Err(Errno::EROFS);
            }
            mask |= MAY_WRITE;
        }
        mode::S_IFCHR | mode::S_IFBLK if mnt_flags & ms::NODEV != 0 => {
            return Err(Errno::EACCES);
        }
        _ => {}
    }
    vfs.fuse_permission(cred, id, mask)?;
    if flags & oflags::O_NOATIME != 0 && !cred.owns(&fuse::cached_attr(vfs, id)?.stat()) {
        return Err(Errno::EPERM);
    }
    if readonly && kind == mode::S_IFREG && accmode != oflags::O_RDONLY {
        return Err(Errno::EROFS);
    }
    // A program being run cannot be opened for writing (deny_write_access)
    if kind == mode::S_IFREG
        && accmode != oflags::O_RDONLY
        && super::fs::executing(cx, fuse::key(id))
    {
        return Err(Errno::ETXTBSY);
    }
    if kind != mode::S_IFDIR && flags & oflags::O_DIRECTORY != 0 {
        return Err(Errno::ENOTDIR);
    }
    match kind {
        mode::S_IFDIR => Ok(FileKind::Directory {
            node,
            entries: None,
            process: None,
            fuse: Some(Box::new(fuse::open(cx, vfs, id, flags, true)?)),
        }),
        mode::S_IFREG => {
            let h = fuse::open(cx, vfs, id, flags, false)?;
            if flags & oflags::O_TRUNC != 0 {
                if !fuse::atomic_o_trunc(&h.conn.borrow()) {
                    let set = SetAttr {
                        size: Some(0),
                        ..SetAttr::default()
                    };
                    setattr(cx, vfs, id, &set, Some(h.fh))?;
                }
                vfs.fuse_mut().stale(id);
            }
            Ok(FileKind::Fuse(Box::new(h)))
        }
        _ => Err(Errno::ENXIO),
    }
}

/// Creates and opens regular file `name` in FUSE directory `dir` (6.1's `fuse_create_open`):
/// `FUSE_CREATE` with the open flags, the mode less the umask and the umask, or, from a daemon
/// without it, `FUSE_MKNOD` and then an open. The directory must be on a writable mount (EROFS)
/// and, with `default_permissions`, writable and searchable. A name another process created since
/// the walk (EEXIST without `O_EXCL`) is looked up again and the syscall runs again, opening it.
/// Returns the new node and the open file.
pub(super) fn create(
    cx: &mut Cx<'_>,
    cred: &Cred,
    vfs: &Vfs,
    (mnt, dir): (MountId, FuseId),
    name: &[u8],
    perm: u32,
    flags: u32,
) -> Result<(FuseId, FileKind), Errno> {
    if vfs.readonly(&Node::Fuse(mnt, dir)) {
        return Err(Errno::EROFS);
    }
    vfs.fuse_permission(cred, dir, MAY_WRITE | MAY_EXEC)?;
    let umask = cx.umask();
    let (conn, nodeid) = fuse::target(vfs, dir)?;
    let mode = mode::S_IFREG | masked(&conn, perm & 0o7777, umask);
    if conn.borrow().no & fuse::no::CREATE == 0 {
        let args = Args::default()
            .u32(flags & !oflags::O_NOCTTY)
            .u32(mode)
            .u32(umask)
            .u32(0)
            .name(name);
        match fuse::call(cx, &conn, op::CREATE, nodeid, &args.0) {
            Ok(reply) => {
                let ns = conn.borrow().user_ns().clone();
                let e = match Entry::parse(&reply, &ns)? {
                    Ok(e) if e.attr.mode & mode::S_IFMT == mode::S_IFREG => e,
                    _ => return Err(Errno::EIO),
                };
                let mut f = fuse::Fields::new(reply.get(128..).ok_or(Errno::EIO)?);
                let (fh, open_flags) = (f.u64()?, f.u32()?);
                let id = {
                    let mut fuse = vfs.fuse_mut();
                    fuse.stale(dir);
                    fuse.enter(dir, name, &e)?
                };
                let mut kept = flags & !(oflags::O_CREAT | oflags::O_EXCL | oflags::O_NOCTTY);
                if !fuse::atomic_o_trunc(&conn.borrow()) {
                    kept &= !oflags::O_TRUNC;
                }
                let h = Handle {
                    conn: conn.clone(),
                    id,
                    nodeid: e.nodeid,
                    fh,
                    open_flags,
                    flags: kept,
                    dir: false,
                    no_release: false,
                }
                .counted();
                return Ok((id, FileKind::Fuse(Box::new(h))));
            }
            Err(Errno::ENOSYS) => {}
            // Created meanwhile by another process (Linux holds the directory's lock from the
            // lookup to the create, so it opens what is there): look it up again and open it
            Err(Errno::EEXIST) if flags & oflags::O_EXCL == 0 => {
                vfs.fuse_mut().forget_child(dir, name);
                return Err(fuse::need(fuse::Query::Lookup(dir, name.to_vec())));
            }
            Err(e) => return Err(e),
        }
    }
    let made = make(cx, vfs, dir, name, Make::Node(mode, 0));
    let id = match made {
        Err(Errno::EEXIST) if flags & oflags::O_EXCL == 0 => {
            vfs.fuse_mut().forget_child(dir, name);
            return Err(fuse::need(fuse::Query::Lookup(dir, name.to_vec())));
        }
        r => r?,
    };
    let h = fuse::open(cx, vfs, id, flags & !oflags::O_TRUNC, false)?;
    Ok((id, FileKind::Fuse(Box::new(h))))
}

/// Permission bits `perm` of something created by a process with umask `umask`: masked here,
/// unless the daemon applies the umask itself (`FUSE_DONT_MASK`, 6.1's `fc->dont_mask`).
pub(super) fn masked(conn: &Rc<RefCell<Conn>>, perm: u32, umask: u32) -> u32 {
    if fuse::dont_mask(&conn.borrow()) {
        perm
    } else {
        perm & !umask
    }
}

/// What [`make`] creates.
pub(super) enum Make<'a> {
    /// `FUSE_MKNOD`: type and permission bits (umask already applied), device number.
    Node(u32, u64),
    /// `FUSE_MKDIR`: permission bits (umask already applied).
    Dir(u32),
    /// `FUSE_SYMLINK`: the target.
    Symlink(&'a [u8]),
    /// `FUSE_LINK`: the node id of the file to link.
    Link(u64),
}

/// Creates `name` in FUSE directory `dir` (6.1's `create_new_entry`): the reply's entry must be
/// of the type asked for (EIO). The new entry is cached, and the directory's attributes go stale.
pub(super) fn make(
    cx: &mut Cx<'_>,
    vfs: &Vfs,
    dir: FuseId,
    name: &[u8],
    what: Make<'_>,
) -> Result<FuseId, Errno> {
    let (conn, nodeid) = fuse::target(vfs, dir)?;
    let umask = cx.umask();
    let (opcode, args, kind) = match what {
        Make::Node(mode, rdev) => (
            op::MKNOD,
            Args::default()
                .u32(mode)
                .u32(rdev as u32)
                .u32(umask)
                .u32(0)
                .name(name),
            mode & mode::S_IFMT,
        ),
        Make::Dir(perm) => (
            op::MKDIR,
            Args::default().u32(perm).u32(umask).name(name),
            mode::S_IFDIR,
        ),
        Make::Symlink(target) => (
            op::SYMLINK,
            Args::default().name(name).name(target),
            mode::S_IFLNK,
        ),
        Make::Link(old) => (op::LINK, Args::default().u64(old).name(name), 0),
    };
    let reply = fuse::call(cx, &conn, opcode, nodeid, &args.0)?;
    let ns = conn.borrow().user_ns().clone();
    let e = match Entry::parse(&reply, &ns)? {
        Ok(e) if kind == 0 || e.attr.mode & mode::S_IFMT == kind => e,
        _ => return Err(Errno::EIO),
    };
    let id = {
        let mut f = vfs.fuse_mut();
        f.stale(dir);
        f.enter(dir, name, &e)?
    };
    // As the VFS reports creations on any filesystem: a link first changes the file's link count
    if kind == 0 {
        inotify::path_event(cx.kr(), fuse::key(id), None, inotify::IN_ATTRIB);
    }
    let isdir = if kind == mode::S_IFDIR {
        inotify::IN_ISDIR
    } else {
        0
    };
    inotify::dir_event(cx.kr(), fuse::key(dir), name, inotify::IN_CREATE | isdir, 0);
    Ok(id)
}

/// Removes `name` from FUSE directory `dir` (`FUSE_UNLINK`, or `FUSE_RMDIR` for a directory):
/// the name leaves the cache, and the directory's and the removed node's attributes go stale.
pub(super) fn remove(
    cx: &mut Cx<'_>,
    vfs: &Vfs,
    dir: FuseId,
    name: &[u8],
    child: FuseId,
    rmdir: bool,
) -> Result<(), Errno> {
    let (conn, nodeid) = fuse::target(vfs, dir)?;
    let opcode = if rmdir { op::RMDIR } else { op::UNLINK };
    fuse::call(cx, &conn, opcode, nodeid, &Args::default().name(name).0)?;
    let last = {
        let mut f = vfs.fuse_mut();
        f.forget_child(dir, name);
        f.stale(dir);
        f.stale(child);
        rmdir || f.get(child).is_none_or(|n| n.attr.nlink <= 1)
    };
    // As the store's removals report: the file's link count, its deletion with its last link,
    // then the directory's entry
    let k = cx.kr();
    let (key, dkey) = (fuse::key(child), fuse::key(dir));
    if rmdir {
        inotify::deleted(k, key, dkey, name);
        inotify::dir_event(k, dkey, name, inotify::IN_DELETE | inotify::IN_ISDIR, 0);
    } else {
        inotify::path_event(k, key, None, inotify::IN_ATTRIB);
        if last {
            inotify::deleted(k, key, dkey, name);
        }
        inotify::dir_event(k, dkey, name, inotify::IN_DELETE, 0);
    }
    Ok(())
}

/// Renames `old` in FUSE directory `from` to `new` in `to` (`FUSE_RENAME`, or `FUSE_RENAME2`
/// with flags; a daemon without it makes flags EINVAL, as 6.1's `fuse_rename2`): both names leave
/// the cache, both directories' attributes go stale, and a directory moved keeps its new place
/// for `..` and getcwd.
pub(super) fn rename(
    cx: &mut Cx<'_>,
    vfs: &Vfs,
    (from, old): (FuseId, &[u8]),
    (to, new): (FuseId, &[u8]),
    moved: Option<FuseId>,
    flags: u32,
) -> Result<(), Errno> {
    let (conn, nodeid) = fuse::target(vfs, from)?;
    let to_nodeid = vfs.fuse().get(to).ok_or(Errno::ENOTCONN)?.nodeid;
    if flags != 0 {
        if conn.borrow().no & fuse::no::RENAME2 != 0 {
            return Err(Errno::EINVAL);
        }
        let args = Args::default()
            .u64(to_nodeid)
            .u32(flags)
            .u32(0)
            .name(old)
            .name(new);
        match fuse::call(cx, &conn, op::RENAME2, nodeid, &args.0) {
            Err(Errno::ENOSYS) => return Err(Errno::EINVAL),
            r => r?,
        };
    } else {
        let args = Args::default().u64(to_nodeid).name(old).name(new);
        fuse::call(cx, &conn, op::RENAME, nodeid, &args.0)?;
    }
    let mut f = vfs.fuse_mut();
    f.forget_child(from, old);
    f.forget_child(to, new);
    f.stale(from);
    f.stale(to);
    if let Some(m) = moved {
        f.stale(m);
        f.moved(m, to, new);
    }
    Ok(())
}

/// The fields a `FUSE_SETATTR` sets (`struct fuse_setattr_in`); owners as kernel IDs.
#[derive(Default)]
pub(super) struct SetAttr {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    /// Access and modification times: None to leave, Some(None) for now, Some(Some(ns)).
    pub atime: Option<Option<i64>>,
    pub mtime: Option<Option<i64>>,
}

/// `FUSE_SETATTR` on node `id` (through open handle `fh` when given): the attributes it reports
/// back are cached.
pub(super) fn setattr(
    cx: &mut Cx<'_>,
    vfs: &Vfs,
    id: FuseId,
    set: &SetAttr,
    fh: Option<u64>,
) -> Result<(), Errno> {
    use fuse::fattr;
    let (conn, nodeid) = fuse::target(vfs, id)?;
    let ns = conn.borrow().user_ns().clone();
    let mut valid = 0;
    let mut bit = |on: bool, b: u32| {
        if on {
            valid |= b;
        }
    };
    bit(set.mode.is_some(), fattr::MODE);
    bit(set.uid.is_some(), fattr::UID);
    bit(set.gid.is_some(), fattr::GID);
    bit(set.size.is_some(), fattr::SIZE);
    bit(fh.is_some(), fattr::FH);
    bit(set.atime.is_some(), fattr::ATIME);
    bit(set.mtime.is_some(), fattr::MTIME);
    bit(set.atime == Some(None), fattr::ATIME_NOW);
    bit(set.mtime == Some(None), fattr::MTIME_NOW);
    let split = |t: Option<Option<i64>>| {
        let ns = t.flatten().unwrap_or(0);
        (
            ns.div_euclid(1_000_000_000) as u64,
            ns.rem_euclid(1_000_000_000) as u32,
        )
    };
    let (asec, ansec) = split(set.atime);
    let (msec, mnsec) = split(set.mtime);
    let (csec, cnsec) = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        (now.as_secs(), now.subsec_nanos())
    };
    let up = |m: &crate::cred::IdMap, k: Option<u32>| {
        k.map_or(0, |k| m.up(k).unwrap_or(crate::cred::OVERFLOW_ID))
    };
    let args = Args::default()
        .u32(valid)
        .u32(0)
        .u64(fh.unwrap_or(0))
        .u64(set.size.unwrap_or(0))
        .u64(0)
        .u64(asec)
        .u64(msec)
        .u64(csec)
        .u32(ansec)
        .u32(mnsec)
        .u32(cnsec)
        .u32(set.mode.unwrap_or(0))
        .u32(0)
        .u32(up(&ns.uid_map.borrow(), set.uid))
        .u32(up(&ns.gid_map.borrow(), set.gid))
        .u32(0);
    let reply = fuse::call(cx, &conn, op::SETATTR, nodeid, &args.0)?;
    let (attr, until) = fuse::attr_out(&reply, &ns)?;
    let mut f = vfs.fuse_mut();
    let old = f.get(id).ok_or(Errno::ENOTCONN)?.attr.mode;
    if (attr.mode ^ old) & mode::S_IFMT != 0 {
        f.stale(id);
        return Err(Errno::EIO);
    }
    f.set_attr(id, attr, until);
    Ok(())
}

// ---- Reading and writing ----

/// Takes the position of open file `file` for a read or write without an offset, as
/// [`super::fs`]'s unlocked I/O does (Linux's `f_pos_lock`): waits while another one is in
/// progress.
fn take_pos(cx: &mut Cx<'_>, file: &FileRef, offset: Option<u64>) -> Result<u64, Errno> {
    if let Some(off) = offset {
        return Ok(off);
    }
    while file.borrow().pos_busy != 0 {
        if cx.task.killed.get() {
            return Err(Errno::EINTR);
        }
        file.borrow_mut().pos_busy = 2;
        cx.wait_on(None, &mut [], crate::kernel::wake::LOCK)?;
    }
    let mut f = file.borrow_mut();
    f.pos_busy = 1;
    Ok(f.pos)
}

/// Gives the position back after [`take_pos`], advanced past what was transferred.
fn put_pos(file: &FileRef, offset: Option<u64>, start: u64, r: &Result<usize, Errno>) {
    if offset.is_some() {
        return;
    }
    let mut f = file.borrow_mut();
    if let Ok(n) = r {
        f.pos = start + *n as u64;
    }
    if std::mem::take(&mut f.pos_busy) == 2 {
        crate::kernel::request_wake_locks();
    }
}

/// read(2) and its relatives on an open FUSE file: `FUSE_READ` from the position (0 for a
/// stream).
pub(super) fn read(
    cx: &mut Cx<'_>,
    file: &FileRef,
    io: &Io,
    buf: &mut [u8],
    offset: Option<u64>,
) -> Result<usize, Errno> {
    let offset = if io.stream { Some(0) } else { offset };
    let start = take_pos(cx, file, offset)?;
    let r = fuse::read(cx, io.handle(), start, buf.len()).map(|data| {
        buf[..data.len()].copy_from_slice(&data);
        data.len()
    });
    put_pos(file, offset, start, &r);
    r
}

/// write(2) and its relatives on an open FUSE file: `FUSE_WRITE` at the position, or with
/// `O_APPEND` at the size the daemon reports first (6.1's `fuse_cache_write_iter` refreshes it
/// the same way). The node's attributes go stale.
pub(super) fn write(
    cx: &mut Cx<'_>,
    file: &FileRef,
    io: &Io,
    data: &[u8],
    offset: Option<u64>,
) -> Result<usize, Errno> {
    // As fcntl may have changed it since the open
    let append = file.borrow().flags & oflags::O_APPEND != 0;
    let offset = if io.stream { Some(0) } else { offset };
    let start = take_pos(cx, file, offset)?;
    let vfs = cx.vfs()?;
    // Appends to one node go one at a time from asking for the size to writing there
    let appending = append
        && take_append(cx, &vfs, io.id).inspect_err(|e| {
            put_pos(file, offset, start, &Err(*e));
        })?;
    let r = (|| {
        let at = if append {
            fuse::getattr(cx, &vfs, io.id, Some(io.fh))?.size
        } else {
            start
        };
        // RLIMIT_FSIZE, as generic_write_checks applies it
        let data = &data[..super::fs::fsize_room(cx, at, data.len())?];
        let n = fuse::write(cx, io.handle(), at, data)?;
        vfs.fuse_mut().stale(io.id);
        Ok((n, at))
    })();
    if appending {
        if let Some(n) = vfs.fuse_mut().get_mut(io.id) {
            n.appending = false;
        }
        crate::kernel::request_wake_locks();
    }
    match r {
        Ok((n, at)) => {
            put_pos(file, offset, at, &Ok(n));
            Ok(n)
        }
        Err(e) => {
            put_pos(file, offset, start, &Err(e));
            Err(e)
        }
    }
}

/// Takes node `id` for an `O_APPEND` write, waiting while another one has it (on the lock
/// channel, as file positions do); true once taken.
fn take_append(cx: &mut Cx<'_>, vfs: &Vfs, id: FuseId) -> Result<bool, Errno> {
    loop {
        let busy = vfs.fuse().get(id).ok_or(Errno::ENOTCONN)?.appending;
        if !busy {
            break;
        }
        if cx.task.killed.get() {
            return Err(Errno::EINTR);
        }
        cx.wait_on(None, &mut [], crate::kernel::wake::LOCK)?;
    }
    if let Some(n) = vfs.fuse_mut().get_mut(id) {
        n.appending = true;
    }
    Ok(true)
}

/// lseek on an open FUSE file (6.1's `fuse_file_llseek`): `SEEK_END` from the size the daemon
/// reports, `SEEK_DATA` and `SEEK_HOLE` through `FUSE_LSEEK` (a daemon without it has data up to
/// the end); a file opened nonseekable is ESPIPE.
pub(super) fn lseek(
    cx: &mut Cx<'_>,
    file: &FileRef,
    offset: i64,
    whence: u32,
) -> Result<u64, Errno> {
    const SEEK_SET: u32 = 0;
    const SEEK_CUR: u32 = 1;
    const SEEK_END: u32 = 2;
    const SEEK_DATA: u32 = 3;
    const SEEK_HOLE: u32 = 4;
    let (io, seekable, pos) = {
        let f = file.borrow();
        let FileKind::Fuse(h) = &f.kind else {
            return Err(Errno::EBADF);
        };
        (Io::of(h), seekable(h), f.pos)
    };
    if !seekable {
        return Err(Errno::ESPIPE);
    }
    let vfs = cx.vfs()?;
    let new = match whence {
        SEEK_SET => offset,
        SEEK_CUR => (pos as i64).checked_add(offset).ok_or(Errno::EINVAL)?,
        SEEK_END => {
            let size = fuse::getattr(cx, &vfs, io.id, Some(io.fh))?.size as i64;
            size.checked_add(offset).ok_or(Errno::EINVAL)?
        }
        SEEK_DATA | SEEK_HOLE => {
            let found = if io.conn.borrow().no & fuse::no::LSEEK == 0 {
                let args = Args::default()
                    .u64(io.fh)
                    .u64(offset as u64)
                    .u32(whence)
                    .u32(0);
                match fuse::call(cx, &io.conn, fuse::op::LSEEK, io.nodeid, &args.0) {
                    Ok(r) => Some(fuse::Fields::new(&r).u64()? as i64),
                    Err(Errno::ENOSYS) => None,
                    Err(e) => return Err(e),
                }
            } else {
                None
            };
            match found {
                Some(at) => at,
                None => {
                    let size = fuse::getattr(cx, &vfs, io.id, Some(io.fh))?.size as i64;
                    if offset < 0 || offset >= size {
                        return Err(Errno::ENXIO);
                    }
                    if whence == SEEK_DATA { offset } else { size }
                }
            }
        }
        _ => return Err(Errno::EINVAL),
    };
    if new < 0 {
        return Err(Errno::EINVAL);
    }
    file.borrow_mut().pos = new as u64;
    Ok(new as u64)
}

/// Lists an open FUSE directory for getdents64 the first time (and after a seek to 0): the
/// daemon's entries, `.` and `..` included as it gives them.
pub(super) fn fill_dir(cx: &mut Cx<'_>, file: &FileRef) -> Result<(), Errno> {
    let io = {
        let f = file.borrow();
        match &f.kind {
            FileKind::Directory {
                fuse: Some(h),
                entries: None,
                ..
            } => Io::of(h),
            _ => return Ok(()),
        }
    };
    let list: Vec<DirEntry> = fuse::readdir(cx, io.handle())?;
    if let FileKind::Directory { entries, .. } = &mut file.borrow_mut().kind {
        *entries = Some(list);
    }
    Ok(())
}

/// fsync and fdatasync of an open FUSE file or directory (`FUSE_FSYNC`, `FUSE_FSYNCDIR`): a
/// daemon without them has nothing to sync.
pub(super) fn fsync(cx: &mut Cx<'_>, io: &Io, dir: bool, data_only: bool) -> Result<(), Errno> {
    let (conn, nodeid, fh) = (&io.conn, io.nodeid, io.fh);
    let (opcode, bit) = if dir {
        (op::FSYNCDIR, fuse::no::FSYNCDIR)
    } else {
        (op::FSYNC, fuse::no::FSYNC)
    };
    if conn.borrow().no & bit != 0 {
        return Ok(());
    }
    let args = Args::default().u64(fh).u32(u32::from(data_only)).u32(0);
    match fuse::call(cx, conn, opcode, nodeid, &args.0) {
        Err(Errno::ENOSYS) => Ok(()),
        r => r.map(|_| ()),
    }
}

/// fallocate of an open FUSE file (`FUSE_FALLOCATE`; a daemon without it is EOPNOTSUPP).
pub(super) fn fallocate(
    cx: &mut Cx<'_>,
    io: &Io,
    mode: u32,
    offset: u64,
    len: u64,
) -> Result<(), Errno> {
    const KEEP_SIZE: u32 = 1;
    const PUNCH_HOLE: u32 = 2;
    // Only these two (6.1's fuse_file_fallocate), and a hole keeps the size (vfs_fallocate)
    if mode & !(KEEP_SIZE | PUNCH_HOLE) != 0 || mode == PUNCH_HOLE {
        return Err(Errno::EOPNOTSUPP);
    }
    if mode & KEEP_SIZE == 0 {
        super::fs::check_fsize(cx, offset.saturating_add(len))?;
    }
    if io.conn.borrow().no & fuse::no::FALLOCATE != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let args = Args::default()
        .u64(io.fh)
        .u64(offset)
        .u64(len)
        .u32(mode)
        .u32(0);
    match fuse::call(cx, &io.conn, op::FALLOCATE, io.nodeid, &args.0) {
        Err(Errno::ENOSYS) => Err(Errno::EOPNOTSUPP),
        r => {
            r?;
            cx.vfs()?.fuse_mut().stale(io.id);
            Ok(())
        }
    }
}

/// The I/O route of an open FUSE file (regular file or directory), for fsync and fallocate.
pub(super) fn io_of(file: &FileRef) -> Option<(Io, bool)> {
    match &file.borrow().kind {
        FileKind::Fuse(h) => Some((Io::of(h), false)),
        FileKind::Directory { fuse: Some(h), .. } => Some((Io::of(h), true)),
        _ => None,
    }
}

/// close(2) of a FUSE file: `FUSE_FLUSH`, whose error close returns (the descriptor is gone
/// anyway), as 6.1's `filp_close` does.
pub(super) fn flush(cx: &mut Cx<'_>, file: &FileRef) -> Result<(), Errno> {
    let table = Rc::as_ptr(&cx.t.fds) as usize;
    flush_as(cx, file, table)
}

/// [`flush`] for a file closed in the fd table at address `table`, whose lock owner the daemon
/// is told (seccomp's `SECCOMP_ADDFD_FLAG_SETFD` replaces an fd in another process's table).
pub(super) fn flush_as(cx: &mut Cx<'_>, file: &FileRef, table: usize) -> Result<(), Errno> {
    let h = match &file.borrow().kind {
        FileKind::Fuse(h) => Io::of(h),
        _ => return Ok(()),
    };
    let skip = {
        let f = file.borrow();
        let FileKind::Fuse(handle) = &f.kind else {
            return Ok(());
        };
        handle.no_release || handle.open_flags & fuse::fopen::NOFLUSH != 0
    };
    if skip || h.conn.borrow().no & fuse::no::FLUSH != 0 {
        return Ok(());
    }
    let owner = fuse::lock_owner(table);
    let args = Args::default().u64(h.fh).u32(0).u32(0).u64(owner);
    match fuse::call_forced(cx, &h.conn, op::FLUSH, h.nodeid, &args.0) {
        Err(Errno::ENOSYS) => Ok(()),
        r => r.map(|_| ()),
    }
}

// ---- Metadata ----

/// stat of FUSE node `id` (6.1's `fuse_getattr`): EACCES for a process the mount does not allow,
/// otherwise the cached attributes (fetched again when out of date), with the instance's device
/// number and the block size the daemon gave.
pub(super) fn stat(vfs: &Vfs, cred: &Cred, node: &Node) -> Result<[u8; 128], Errno> {
    let Node::Fuse(_, id) = node else {
        return Err(Errno::EINVAL);
    };
    if !vfs.fuse().conn_of(*id)?.borrow().allows(cred) {
        return Err(Errno::EACCES);
    }
    let attr = fuse::cached_attr(vfs, *id)?;
    let mut b = encode_stat(&attr.stat(), vfs.dev_of(node));
    if attr.blksize != 0 {
        b[56..60].copy_from_slice(&attr.blksize.to_le_bytes());
    }
    Ok(b)
}

/// statfs of the FUSE filesystem node `id` is in (6.1's `fuse_statfs`): only `FUSE_SUPER_MAGIC`
/// for a process the mount does not allow.
pub(super) fn statfs(
    cx: &mut Cx<'_>,
    vfs: &Vfs,
    id: FuseId,
    flags: u64,
) -> Result<[u64; 11], Errno> {
    let cred = cx.cred();
    if !vfs.fuse().conn_of(id)?.borrow().allows(&cred) {
        let mut w = [0; 11];
        w[0] = fuse::SUPER_MAGIC;
        return Ok(w);
    }
    let s = fuse::statfs(cx, vfs, id)?;
    Ok([
        fuse::SUPER_MAGIC,
        s.bsize,
        s.blocks,
        s.bfree,
        s.bavail,
        s.files,
        s.ffree,
        0,
        s.namelen,
        s.frsize,
        flags,
    ])
}

/// access(2) on FUSE node `id` (6.1's `fuse_permission` with `MAY_ACCESS`): the mount's own
/// checks, then without `default_permissions` the daemon's answer to `FUSE_ACCESS`.
pub(super) fn access(
    cx: &mut Cx<'_>,
    vfs: &Vfs,
    cred: &Cred,
    id: FuseId,
    amode: u32,
) -> Result<(), Errno> {
    let conn = vfs.fuse().conn_of(id)?;
    if !conn.borrow().allows(cred) {
        return Err(Errno::EACCES);
    }
    if conn.borrow().opts.default_permissions {
        return cred.permission(&fuse::cached_attr(vfs, id)?.stat(), amode);
    }
    fuse::access(cx, vfs, cred, id, amode)
}

/// The checks chdir, fchdir and chroot make on FUSE directory `id`: search permission, asked of
/// the daemon without `default_permissions` (6.1's `MAY_CHDIR`).
pub(super) fn may_chdir(cx: &mut Cx<'_>, vfs: &Vfs, cred: &Cred, id: FuseId) -> Result<(), Errno> {
    access(cx, vfs, cred, id, MAY_EXEC)
}

// ---- Extended attributes ----

/// The xattr permission rules of [`super::xattr`] on FUSE node `id`: `trusted.*` needs
/// `CAP_SYS_ADMIN` (hidden otherwise), `user.*` only on regular files and directories and with
/// read or write permission, which for FUSE is [`Vfs::fuse_permission`].
pub(super) fn xattr_permission(
    vfs: &Vfs,
    cred: &Cred,
    id: FuseId,
    name: &[u8],
    write: bool,
) -> Result<(), Errno> {
    use crate::cred::cap;
    let hidden = if write { Errno::EPERM } else { Errno::ENODATA };
    if name.starts_with(b"trusted.") {
        return if cred.capable(cap::SYS_ADMIN) {
            Ok(())
        } else {
            Err(hidden)
        };
    }
    if name.starts_with(b"user.") {
        let kind = vfs.fuse().get(id).ok_or(Errno::ENOTCONN)?.kind();
        if kind != mode::S_IFREG && kind != mode::S_IFDIR {
            return Err(hidden);
        }
        // In a sticky directory only its owner may change them (xattr_permission)
        if write && kind == mode::S_IFDIR {
            let st = fuse::cached_attr(vfs, id)?.stat();
            if st.mode & mode::S_ISVTX != 0 && !cred.owns(&st) {
                return Err(Errno::EPERM);
            }
        }
        return vfs.fuse_permission(cred, id, if write { MAY_WRITE } else { MAY_READ });
    }
    // Program file capabilities need CAP_SETFCAP over the file (cap_convert_nscap); other
    // security attributes CAP_SYS_ADMIN (cap_inode_setxattr)
    if write && name == crate::cred::XATTR_NAME_CAPS {
        let st = fuse::cached_attr(vfs, id)?.stat();
        if !cred.capable_wrt(&st, cap::SETFCAP) {
            return Err(Errno::EPERM);
        }
    } else if write && name.starts_with(b"security.") && !cred.capable(cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    vfs.fuse_permission(cred, id, 0)
}

/// setxattr (`value` Some, with flags) or removexattr on FUSE node `id`: `FUSE_SETXATTR` or
/// `FUSE_REMOVEXATTR`, EOPNOTSUPP from a daemon without them.
pub(super) fn set_xattr(
    cx: &mut Cx<'_>,
    vfs: &Vfs,
    id: FuseId,
    name: &[u8],
    value: Option<(&[u8], u32)>,
) -> Result<(), Errno> {
    let (conn, nodeid) = fuse::target(vfs, id)?;
    let (opcode, bit, args) = match value {
        Some((v, flags)) => (
            op::SETXATTR,
            fuse::no::SETXATTR,
            Args::default()
                .u32(v.len() as u32)
                .u32(flags)
                .name(name)
                .bytes(v),
        ),
        None => (
            op::REMOVEXATTR,
            fuse::no::REMOVEXATTR,
            Args::default().name(name),
        ),
    };
    if conn.borrow().no & bit != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    match fuse::call(cx, &conn, opcode, nodeid, &args.0) {
        Err(Errno::ENOSYS) => Err(Errno::EOPNOTSUPP),
        r => {
            r?;
            vfs.fuse_mut().stale(id);
            Ok(())
        }
    }
}

// ---- The device ----

/// `FUSE_DEV_IOC_CLONE` (`_IOR(229, 0, uint32_t)`): attaches device file `file`, which has no
/// connection yet, to the connection of the device file whose fd the argument holds, so a daemon
/// can read requests on several descriptors (6.1's `fuse_dev_ioctl`). Anything but an attached
/// device file opened in the same user namespace, a bad fd included, is EINVAL.
pub(super) const DEV_IOC_CLONE: u32 = 0x8004_e500;

pub(super) fn dev_ioctl(
    cx: &mut Cx<'_>,
    file: &FileRef,
    request: u32,
    arg: u64,
) -> Result<u64, Errno> {
    if request != DEV_IOC_CLONE {
        return Err(Errno::ENOTTY);
    }
    let mut b = [0u8; 4];
    cx.read_user(crate::uaccess::UserSlice::new(arg, 4), &mut b)?;
    let old = cx
        .fds()
        .get(i32::from_le_bytes(b))
        .map_err(|_| Errno::EINVAL)?;
    let (conn, ns) = match &old.borrow().kind {
        FileKind::FuseDev(d) => (d.conn.clone().ok_or(Errno::EINVAL)?, d.opener_ns.clone()),
        _ => return Err(Errno::EINVAL),
    };
    match &mut file.borrow_mut().kind {
        FileKind::FuseDev(d) if d.conn.is_none() && Rc::ptr_eq(&d.opener_ns, &ns) => {
            d.attach(&conn)
        }
        _ => return Err(Errno::EINVAL),
    }
    Ok(0)
}

// ---- Changing the namespace ----

/// A FUSE directory (through its mount) and a name in it.
pub(super) type FuseParent<'p> = (MountId, FuseId, &'p [u8]);

/// The FUSE directory the last name of `path` is in, and the name, when it is one (the parent's
/// resolution errors are returned as they are).
pub(super) fn parent<'p>(
    cx: &Cx<'_>,
    dirfd: i32,
    path: &'p [u8],
) -> Result<Option<FuseParent<'p>>, Errno> {
    let start = super::fs::start_dir(cx, dirfd, path)?;
    let vfs = cx.vfs()?;
    let (dir, name) = vfs.resolve_parent(&cx.view(&vfs), &cx.cred(), &cx.root(), &start, path)?;
    Ok(match dir {
        Node::Fuse(mnt, d) => Some((mnt, d, name)),
        _ => None,
    })
}

/// The checks before creating `name` in FUSE directory `dir` (6.1's `filename_create` and
/// `may_create`): `.`, `..` and an existing name are EEXIST, then a read-only mount EROFS, then
/// the directory must be writable and searchable as [`Vfs::fuse_permission`] decides.
fn may_create(
    vfs: &Vfs,
    cred: &Cred,
    (mnt, dir): (MountId, FuseId),
    name: &[u8],
) -> Result<(), Errno> {
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    match vfs.fuse_child(dir, name) {
        Ok(_) => return Err(Errno::EEXIST),
        Err(e) if e == Errno::ENOENT => {}
        Err(e) => return Err(e),
    }
    if vfs.readonly(&Node::Fuse(mnt, dir)) {
        return Err(Errno::EROFS);
    }
    vfs.fuse_permission(cred, dir, MAY_WRITE | MAY_EXEC)
}

/// mkdir in FUSE directory `dir` (`FUSE_MKDIR`).
pub(super) fn mkdir(
    cx: &mut Cx<'_>,
    at: (MountId, FuseId),
    name: &[u8],
    perm: u32,
) -> Result<(), Errno> {
    let vfs = cx.vfs()?;
    may_create(&vfs, &cx.cred(), at, name)?;
    let (conn, _) = fuse::target(&vfs, at.1)?;
    // vfs_mkdir keeps only the permission and sticky bits
    let perm = masked(&conn, perm & 0o1777, cx.umask());
    make(cx, &vfs, at.1, name, Make::Dir(perm)).map(|_| ())
}

/// mknod in FUSE directory `dir` (`FUSE_MKNOD`): devices need `CAP_MKNOD`, as `vfs_mknod`
/// checks.
pub(super) fn mknod(
    cx: &mut Cx<'_>,
    at: (MountId, FuseId),
    name: &[u8],
    mode_arg: u32,
    dev: u64,
) -> Result<(), Errno> {
    let vfs = cx.vfs()?;
    let cred = cx.cred();
    may_create(&vfs, &cred, at, name)?;
    let kind = match mode_arg & mode::S_IFMT {
        0 => mode::S_IFREG,
        k => k,
    };
    if matches!(kind, mode::S_IFCHR | mode::S_IFBLK)
        && dev & 0xffff_ffff != 0
        && !cred.capable(crate::cred::cap::MKNOD)
    {
        return Err(Errno::EPERM);
    }
    let (conn, _) = fuse::target(&vfs, at.1)?;
    let perm = masked(&conn, mode_arg & 0o7777, cx.umask());
    make(
        cx,
        &vfs,
        at.1,
        name,
        Make::Node(kind | perm, dev & 0xffff_ffff),
    )
    .map(|_| ())
}

/// symlink in FUSE directory `dir` (`FUSE_SYMLINK`).
pub(super) fn symlink(
    cx: &mut Cx<'_>,
    at: (MountId, FuseId),
    name: &[u8],
    target: &[u8],
) -> Result<(), Errno> {
    let vfs = cx.vfs()?;
    may_create(&vfs, &cx.cred(), at, name)?;
    make(cx, &vfs, at.1, name, Make::Symlink(target)).map(|_| ())
}

/// link of FUSE node `src` as `name` in FUSE directory `dir` (`FUSE_LINK`): both must be on one
/// mount (EXDEV), and a directory cannot be linked (EPERM).
pub(super) fn link(
    cx: &mut Cx<'_>,
    src: &Node,
    at: (MountId, FuseId),
    name: &[u8],
) -> Result<(), Errno> {
    let vfs = cx.vfs()?;
    let cred = cx.cred();
    let (mnt, dir) = at;
    // 6.1's do_linkat: the new name (EEXIST, then EROFS), the mounts (EXDEV), may_linkat, then
    // vfs_link (the directory's permission, EPERM for a directory)
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    match vfs.fuse_child(dir, name) {
        Ok(_) => return Err(Errno::EEXIST),
        Err(e) if e == Errno::ENOENT => {}
        Err(e) => return Err(e),
    }
    if vfs.readonly(&Node::Fuse(mnt, dir)) {
        return Err(Errno::EROFS);
    }
    let Node::Fuse(src_mnt, id) = *src else {
        return Err(Errno::EXDEV);
    };
    if src_mnt != mnt {
        return Err(Errno::EXDEV);
    }
    // fs.protected_hardlinks, with the file's permission as the mount decides it
    if cx.kr().protected_hardlinks {
        let st = fuse::cached_attr(&vfs, id)?.stat();
        let setgid_exec = mode::S_ISGID | 0o010;
        let rw = match vfs.fuse_permission(&cred, id, MAY_READ | MAY_WRITE) {
            Ok(()) => true,
            Err(e) if e == Errno::FUSE_RETRY => return Err(e),
            Err(_) => false,
        };
        let safe = st.kind() == mode::S_IFREG
            && st.mode & mode::S_ISUID == 0
            && st.mode & setgid_exec != setgid_exec
            && rw;
        if !safe && !cred.owns(&st) {
            return Err(Errno::EPERM);
        }
    }
    vfs.fuse_permission(&cred, dir, MAY_WRITE | MAY_EXEC)?;
    let (kind, nodeid) = {
        let f = vfs.fuse();
        let n = f.get(id).ok_or(Errno::ENOTCONN)?;
        (n.kind(), n.nodeid)
    };
    if kind == mode::S_IFDIR {
        return Err(Errno::EPERM);
    }
    make(cx, &vfs, dir, name, Make::Link(nodeid))?;
    vfs.fuse_mut().stale(id);
    Ok(())
}

/// unlink and rmdir of `name` in FUSE directory `dir` (6.1's `do_unlinkat` and `do_rmdir`):
/// `.` and `..`, a read-only mount, the name's lookup, a trailing slash on a non-directory, the
/// directory's permission, the type, a mount point (EBUSY), then `FUSE_UNLINK` or `FUSE_RMDIR`
/// (the daemon answers ENOTEMPTY).
pub(super) fn unlink(
    cx: &mut Cx<'_>,
    (mnt, dir): (MountId, FuseId),
    name: &[u8],
    rmdir: bool,
    trailing_slash: bool,
) -> Result<(), Errno> {
    match name {
        b"." if rmdir => return Err(Errno::EINVAL),
        b".." if rmdir => return Err(Errno::ENOTEMPTY),
        b"." | b".." => return Err(Errno::EISDIR),
        _ => {}
    }
    let vfs = cx.vfs()?;
    let dnode = Node::Fuse(mnt, dir);
    if vfs.readonly(&dnode) {
        return Err(Errno::EROFS);
    }
    let child = vfs.fuse_child(dir, name)?;
    let kind = vfs.fuse().node(child).kind();
    let is_dir = kind == mode::S_IFDIR;
    if !rmdir && trailing_slash {
        return Err(if is_dir {
            Errno::EISDIR
        } else {
            Errno::ENOTDIR
        });
    }
    let cred = cx.cred();
    vfs.fuse_permission(&cred, dir, MAY_WRITE | MAY_EXEC)?;
    sticky(&vfs, &cred, dir, child)?;
    match (rmdir, is_dir) {
        (true, false) => return Err(Errno::ENOTDIR),
        (false, true) => return Err(Errno::EISDIR),
        _ => {}
    }
    if vfs.is_point_in(&Node::Fuse(mnt, child), cx.mnt_ns()) {
        return Err(Errno::EBUSY);
    }
    remove(cx, &vfs, dir, name, child, rmdir)
}

/// The sticky-directory rule of `may_delete` with `default_permissions`: in a sticky directory
/// only the owner of the file or of the directory (or `CAP_FOWNER`) may remove or rename it.
fn sticky(vfs: &Vfs, cred: &Cred, dir: FuseId, child: FuseId) -> Result<(), Errno> {
    let default_permissions = vfs.fuse().conn_of(dir)?.borrow().opts.default_permissions;
    if !default_permissions {
        return Ok(());
    }
    let d = fuse::cached_attr(vfs, dir)?.stat();
    let c = fuse::cached_attr(vfs, child)?.stat();
    cred.may_delete(&d, &c)
}

/// renameat2 between FUSE directories (6.1's `do_renameat2` and `vfs_rename` on a FUSE
/// filesystem): both on one mount (EXDEV), the names' checks in Linux's order, then the request.
pub(super) fn rename_at(
    cx: &mut Cx<'_>,
    (mnt, from, old): (MountId, FuseId, &[u8]),
    (to_mnt, to, new): (MountId, FuseId, &[u8]),
    flags: u32,
) -> Result<(), Errno> {
    const RENAME_NOREPLACE: u32 = 1;
    const RENAME_EXCHANGE: u32 = 2;
    if mnt != to_mnt {
        return Err(Errno::EXDEV);
    }
    for (name, src) in [(old, true), (new, false)] {
        if name == b"." || name == b".." {
            return Err(if !src && flags & RENAME_NOREPLACE != 0 {
                Errno::EEXIST
            } else {
                Errno::EBUSY
            });
        }
    }
    let vfs = cx.vfs()?;
    if vfs.readonly(&Node::Fuse(mnt, from)) {
        return Err(Errno::EROFS);
    }
    let src = vfs.fuse_child(from, old)?;
    let dst = match vfs.fuse_child(to, new) {
        Ok(d) => Some(d),
        Err(e) if e == Errno::ENOENT => None,
        Err(e) => return Err(e),
    };
    if flags & RENAME_EXCHANGE != 0 && dst.is_none() {
        return Err(Errno::ENOENT);
    }
    if flags & RENAME_NOREPLACE != 0 && dst.is_some() {
        return Err(Errno::EEXIST);
    }
    let cred = cx.cred();
    vfs.fuse_permission(&cred, from, MAY_WRITE | MAY_EXEC)?;
    vfs.fuse_permission(&cred, to, MAY_WRITE | MAY_EXEC)?;
    sticky(&vfs, &cred, from, src)?;
    let src_dir = vfs.fuse().node(src).kind() == mode::S_IFDIR;
    if let Some(d) = dst {
        if d == src {
            return Ok(());
        }
        sticky(&vfs, &cred, to, d)?;
        let dst_dir = vfs.fuse().node(d).kind() == mode::S_IFDIR;
        if flags & RENAME_EXCHANGE == 0 {
            match (src_dir, dst_dir) {
                (true, false) => return Err(Errno::ENOTDIR),
                (false, true) => return Err(Errno::EISDIR),
                _ => {}
            }
        }
        if vfs.is_point_in(&Node::Fuse(mnt, d), cx.mnt_ns()) {
            return Err(Errno::EBUSY);
        }
    }
    if vfs.is_point_in(&Node::Fuse(mnt, src), cx.mnt_ns()) {
        return Err(Errno::EBUSY);
    }
    // A directory cannot move under itself
    if src_dir && vfs.fuse().is_within(to, src) {
        return Err(Errno::EINVAL);
    }
    // What the replaced name was: a directory, the last link of a file
    let dst_seen = dst.and_then(|d| {
        let f = vfs.fuse();
        let n = f.get(d)?;
        Some((d, n.kind() == mode::S_IFDIR, n.attr.nlink <= 1))
    });
    rename(
        cx,
        &vfs,
        (from, old),
        (to, new),
        src_dir.then_some(src),
        flags,
    )?;
    if let Some((d, dir, _)) = dst_seen {
        let mut f = vfs.fuse_mut();
        f.stale(d);
        if flags & RENAME_EXCHANGE != 0 && dir {
            f.moved(d, from, old);
        }
    }
    // fsnotify_move: the pair of moves with one cookie, then the moved file's own event; a
    // replaced file is deleted, an exchanged one moves too
    let k = cx.kr();
    let isdir = |dir: bool| if dir { inotify::IN_ISDIR } else { 0 };
    let (fk, tk) = (fuse::key(from), fuse::key(to));
    let cookie = inotify::cookie(k);
    inotify::dir_event(k, fk, old, inotify::IN_MOVED_FROM | isdir(src_dir), cookie);
    inotify::dir_event(k, tk, new, inotify::IN_MOVED_TO | isdir(src_dir), cookie);
    match dst_seen {
        Some((d, dir, _)) if flags & RENAME_EXCHANGE != 0 => {
            let cookie = inotify::cookie(k);
            inotify::dir_event(k, tk, new, inotify::IN_MOVED_FROM | isdir(dir), cookie);
            inotify::dir_event(k, fk, old, inotify::IN_MOVED_TO | isdir(dir), cookie);
            inotify::path_event(k, fuse::key(d), None, inotify::IN_MOVE_SELF);
        }
        Some((d, dir, last)) => {
            inotify::path_event(k, fuse::key(d), None, inotify::IN_ATTRIB);
            if dir || last {
                inotify::deleted(k, fuse::key(d), tk, new);
            }
        }
        None => {}
    }
    inotify::path_event(k, fuse::key(src), None, inotify::IN_MOVE_SELF);
    Ok(())
}

// ---- Attributes ----

/// Whether the mount node `id` is in checks permissions itself (`default_permissions`); without
/// it, attribute changes are the daemon's to refuse (6.1's `fuse_setattr` sets `ATTR_FORCE`).
pub(super) fn checks_permissions(vfs: &Vfs, id: FuseId) -> Result<bool, Errno> {
    Ok(vfs.fuse().conn_of(id)?.borrow().opts.default_permissions)
}

/// truncate(2) of FUSE node `id` (6.1's `vfs_truncate`): the type, a read-only mount, write
/// permission, then `FUSE_SETATTR` with the size.
pub(super) fn truncate(cx: &mut Cx<'_>, node: &Node, len: u64) -> Result<(), Errno> {
    let Node::Fuse(_, id) = *node else {
        return Err(Errno::EINVAL);
    };
    let vfs = cx.vfs()?;
    match vfs.fuse().get(id).ok_or(Errno::ENOTCONN)?.kind() {
        mode::S_IFDIR => return Err(Errno::EISDIR),
        mode::S_IFREG => {}
        _ => return Err(Errno::EINVAL),
    }
    if vfs.readonly(node) {
        return Err(Errno::EROFS);
    }
    vfs.fuse_permission(&cx.cred(), id, MAY_WRITE)?;
    super::fs::check_fsize(cx, len)?;
    let set = SetAttr {
        size: Some(len),
        ..SetAttr::default()
    };
    setattr(cx, &vfs, id, &set, None)
}

/// ftruncate of an open FUSE file: `FUSE_SETATTR` with the size through its handle.
pub(super) fn ftruncate(cx: &mut Cx<'_>, io: &Io, len: u64) -> Result<(), Errno> {
    let vfs = cx.vfs()?;
    let set = SetAttr {
        size: Some(len),
        ..SetAttr::default()
    };
    setattr(cx, &vfs, io.id, &set, Some(io.fh))
}

/// The open FUSE regular file of descriptor `file`.
pub(super) fn file_io(file: &FileRef) -> Option<Io> {
    match &file.borrow().kind {
        FileKind::Fuse(h) => Some(Io::of(h)),
        _ => None,
    }
}

// ---- Mappings ----

/// For mmap of an open FUSE file: what the mapping reads its pages through ([`Fill`]), an empty
/// host file as long as the file is (its size from the attribute cache, fetched first when stale),
/// into which pages are read as they are touched ([`fill`]). None for any other file, one not open
/// for reading or a mapping mmap will refuse anyway (`refused`). As in 6.1's `fuse_file_mmap`, a
/// file the daemon opened with `FOPEN_DIRECT_IO` cannot be mapped shared (`ENODEV`). Linux maps
/// the file's page cache, which later writes through the mount change; here a page keeps what was
/// read when it was first touched.
pub(super) fn map_fill(
    cx: &mut Cx<'_>,
    file: &FileRef,
    shared: bool,
    refused: bool,
) -> Result<Option<Rc<Fill>>, Errno> {
    let (id, direct) = {
        let f = file.borrow();
        match &f.kind {
            FileKind::Fuse(h) if f.readable() && !refused => {
                (h.id, h.open_flags & fuse::fopen::DIRECT_IO != 0)
            }
            _ => return Ok(None),
        }
    };
    if direct && shared {
        return Err(Errno::ENODEV);
    }
    let size = fuse::cached_attr(&*cx.vfs()?, id)?.size;
    let host = tuxport_hal::memfd(c"tuxport-fuse").map_err(Errno)?;
    host.set_len(size)
        .map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))?;
    Ok(Some(Fill::new(host, size, file.clone())))
}

/// Reads into an on-demand mapping of a FUSE file ([`Fill`]) the pages missing around file page
/// `page` (Linux's `filemap_fault` read-around), the BKL released while the daemon answers.
pub(crate) fn fill(cx: &mut Cx<'_>, (fill, page): (Rc<Fill>, u64)) -> Result<(), Errno> {
    let Some(window) = fill.window(page) else {
        return Ok(());
    };
    let io = match &fill.source.borrow().kind {
        FileKind::Fuse(h) => Io::of(h),
        _ => return Err(Errno::EIO),
    };
    let data = fuse::read(cx, io.handle(), window.0, window.1 as usize)?;
    fill.store(window, &data)
}

/// Before a read or write syscall (`nr`, arguments `a`): the pages of its buffers in on-demand
/// mappings ([`Fill`]) are read in first, as Linux's `fault_in_iov_iter_readable` does, since a
/// syscall that already consumed input cannot simply run again when its copy finds one missing
/// ([`Errno::FUSE_RETRY`]). Errors are left for the syscall to meet.
pub(super) fn prefault(cx: &mut Cx<'_>, nr: u64, a: &[u64; 6]) {
    use tuxport_engine::prot;
    if !cx.mm().has_fills() {
        return;
    }
    // (buffers, whether the syscall writes into them)
    let (ranges, into) = match nr {
        63 | 67 => (vec![(a[1], a[2])], true),
        64 | 68 => (vec![(a[1], a[2])], false),
        65 | 69 | 286 | 66 | 70 | 287 => {
            let n = (a[2] as usize).min(1024);
            let mut raw = vec![0u8; n * 16];
            if cx
                .read_user(crate::uaccess::UserSlice::new(a[1], raw.len()), &mut raw)
                .is_err()
            {
                return;
            }
            let iov = raw
                .chunks_exact(16)
                .map(|c| {
                    let base = u64::from_le_bytes(c[..8].try_into().unwrap());
                    (base, u64::from_le_bytes(c[8..].try_into().unwrap()))
                })
                .collect();
            (iov, matches!(nr, 65 | 69 | 286))
        }
        _ => return,
    };
    let need = if into { prot::WRITE } else { prot::READ };
    for (addr, len) in ranges {
        loop {
            let Some(w) = cx.mm().first_unread(addr, len, need) else {
                break;
            };
            if fill(cx, w).is_err() {
                return;
            }
        }
    }
}

// ---- Forgetting nodes ----

/// Forgets the FUSE nodes nothing uses when the cache asked for it ([`fuse::SWEEP`]: it grew too
/// large, or `drop_caches`), at the end of a syscall, when no syscall of this thread holds a node.
/// Other threads hold nodes only through what [`in_use`] finds, open handles and requests in
/// flight ([`fuse::Fuses::evict`]).
pub(super) fn sweep(cx: &Cx<'_>) {
    use std::sync::atomic::Ordering::Relaxed;
    if !fuse::SWEEP.load(Relaxed) || !fuse::SWEEP.swap(false, Relaxed) {
        return;
    }
    let Ok(vfs) = cx.vfs() else {
        return;
    };
    let all = fuse::DROP.swap(false, Relaxed);
    match in_use(cx.kr(), &vfs) {
        Some(keep) => {
            vfs.fuse_mut().evict(&keep, all);
        }
        // Something is borrowed: another time
        None => {
            fuse::SWEEP.store(true, Relaxed);
            if all {
                fuse::DROP.store(true, Relaxed);
            }
        }
    }
}

/// The FUSE nodes that processes' root directories, cwds, open files (O_PATH ones too) and
/// running programs are, that mounts are on or of, and that inotify watches; None when some of
/// it is borrowed.
fn in_use(k: &crate::kernel::Kernel, vfs: &Vfs) -> Option<std::collections::HashSet<FuseId>> {
    let mut used = std::collections::HashSet::new();
    let mut add = |n: Node| {
        if let Node::Fuse(_, id) = n {
            used.insert(id);
        }
    };
    for task in k.tasks.values() {
        let res = task.res.try_borrow().ok()?;
        {
            let fs = res.fs.try_borrow().ok()?;
            add(fs.root.clone());
            add(fs.cwd.clone());
        }
        if let Some(id) = res.mm.try_borrow().ok()?.exe.and_then(fuse::id_of) {
            add(Node::Fuse(0, id));
        }
        let fds = res.fds.try_borrow().ok()?;
        for file in fds.open_fds().into_iter().filter_map(|fd| fds.get(fd).ok()) {
            if let Some(n) = file.try_borrow().ok()?.node() {
                add(n);
            }
        }
    }
    for n in vfs.mount_nodes() {
        add(n);
    }
    for key in k.inotify.try_borrow().ok()?.watched() {
        if let Some(id) = fuse::id_of(key) {
            add(Node::Fuse(0, id));
        }
    }
    Some(used)
}

/// Descriptors closed other than by close(2) (replaced by dup2, close_range, close-on-exec):
/// each FUSE file among `files` is flushed, its errors ignored, as `filp_close` does for them.
pub(super) fn flush_all(cx: &mut Cx<'_>, files: &[FileRef]) {
    for f in files {
        let _ = flush(cx, f);
    }
}

/// The thread is leaving the kernel for good: when it is the last user of its fd table, the FUSE
/// files in it are flushed before the table goes (6.1's `exit_files` closes each with
/// `filp_close`), so what a process wrote reaches the daemon's flush before its parent goes on.
/// A thread killed with requests not yet read gives them up at once ([`fuse::call`]).
pub(crate) fn flush_at_exit(cx: &mut Cx<'_>) {
    if Rc::strong_count(&cx.t.fds) > 2 {
        return;
    }
    let files: Vec<FileRef> = {
        let t = cx.fds();
        t.open_fds()
            .into_iter()
            .filter_map(|fd| t.get(fd).ok())
            .filter(|f| matches!(f.borrow().kind, FileKind::Fuse(_)))
            .collect()
    };
    flush_all(cx, &files);
}
