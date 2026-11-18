//! File and directory syscalls (the kernel/vfs map; fd tables, opening, I/O, clones, truncation
//! and syncing: the kernel/files map; overlay paths: the kernel/overlay map; flock and fcntl
//! locks: the kernel/locks map; pipes, FIFOs and splice: the kernel/pipe map).
//!
//! Reads and writes have two layers: `read_bytes`/`write_bytes` do one operation on an open file
//! with a host buffer, and `read_file`/`write_file` copy to and from guest memory. Host fds and
//! pipes may block; while blocked they release the BKL and hold no borrow of any open file (other
//! threads may be using the same open file).

use std::os::unix::fs::FileExt;
use std::rc::{Rc, Weak};
use std::time::Duration;

use super::SocketRef;
use super::{FUSE_CHUNK, IO_CHUNK, MAX_COPY, SysResult, get, inotify, put, read_path, tty};
use crate::cell::RefCell;
use crate::cred::{Cred, MAY_EXEC, MAY_READ, MAY_WRITE, cap};
use crate::errno::Errno;
use crate::fd::{EventFd, FileKind, FileRef, OpenFile, TimerFd, oflags};
use crate::kernel::{Cx, Kernel, Tid, request_wake_locks, request_wake_object, wake};
use crate::locks::{self, Lock, Owner, Waiter};
use crate::pipe::{PIPE_BUF, PIPE_CAPACITY, Pipe, PipeEnd};
use crate::process::{RLIMIT_FSIZE, Signal};
use crate::pseudo::{self, PseudoNode};
use crate::signal::{self, SigInfo};
use crate::tty::{PTS_MAJOR, Tty};
use crate::uaccess::UserSlice;
use crate::vfs::dev::{self, Device, rdev};
use crate::vfs::overlay::{self, CopyUp, OvlId};
use crate::vfs::store::{DirEntry, FileTime, Ino, Stat, Store, mode};
use crate::vfs::{Dentry, Fs, MountId, Node, Pseudo, TMPFS_PAGE, Vfs, ms, normalize};

pub(super) const AT_FDCWD: i32 = -100;
pub(super) const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
const AT_REMOVEDIR: u32 = 0x200;
const AT_EACCESS: u32 = 0x200;
const AT_SYMLINK_FOLLOW: u32 = 0x400;
const AT_EMPTY_PATH: u32 = 0x1000;

const F_DUPFD: u32 = 0;
const F_GETFD: u32 = 1;
const F_SETFD: u32 = 2;
const F_GETFL: u32 = 3;
const F_SETFL: u32 = 4;
const F_GETLK: u32 = 5;
const F_SETLK: u32 = 6;
const F_SETLKW: u32 = 7;
const F_OFD_GETLK: u32 = 36;
const F_OFD_SETLK: u32 = 37;
const F_OFD_SETLKW: u32 = 38;
const F_SETOWN: u32 = 8;
const F_GETOWN: u32 = 9;
const F_SETSIG: u32 = 10;
const F_GETSIG: u32 = 11;
const F_SETOWN_EX: u32 = 15;
const F_GETOWN_EX: u32 = 16;
const F_OWNER_TID: i32 = 0;
const F_OWNER_PID: i32 = 1;
const F_OWNER_PGRP: i32 = 2;
const F_SETLEASE: u32 = 1024;
const F_GETLEASE: u32 = 1025;
const F_DUPFD_CLOEXEC: u32 = 1030;
const F_SETPIPE_SZ: u32 = 1031;
const F_GETPIPE_SZ: u32 = 1032;
const F_ADD_SEALS: u32 = 1033;
const F_GET_SEALS: u32 = 1034;
const FD_CLOEXEC: u64 = 1;

/// memfd seals.
const F_SEAL_SEAL: u32 = 0x1;
const F_SEAL_SHRINK: u32 = 0x2;
const F_SEAL_GROW: u32 = 0x4;
const F_SEAL_WRITE: u32 = 0x8;
const F_SEAL_FUTURE_WRITE: u32 = 0x10;
const F_SEAL_EXEC: u32 = 0x20;
const F_SEAL_ALL: u32 = 0x3f;
const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;

const SEEK_SET: u32 = 0;
const SEEK_CUR: u32 = 1;
const SEEK_END: u32 = 2;
const SEEK_DATA: u32 = 3;
const SEEK_HOLE: u32 = 4;

const RENAME_NOREPLACE: u32 = 1;
const RENAME_EXCHANGE: u32 = 2;

const UTIME_NOW: i64 = (1 << 30) - 1;
const UTIME_OMIT: i64 = (1 << 30) - 2;

const CLOSE_RANGE_UNSHARE: u32 = 2;
const CLOSE_RANGE_CLOEXEC: u32 = 4;

/// The most iovecs readv/writev accept (`UIO_MAXIOV`).
const UIO_MAXIOV: usize = 1024;

pub(super) use crate::vfs::ROOT_DEV;
/// Device number pipes report (on Linux pipefs is an anonymous device).
const PIPE_DEV: u64 = 0xd;
/// Device number sockets and anon_inodes (epoll, eventfd) report.
const SOCK_DEV: u64 = 0x8;
const ANON_DEV: u64 = 0xe;
/// Filesystem types statfs reports.
const EXT4_SUPER_MAGIC: u64 = 0xef53;
const MQUEUE_MAGIC: u64 = 0x1980_0202;
const TMPFS_MAGIC: u64 = 0x0102_1994;
const NSFS_MAGIC: u64 = 0x6e73_6673;
const PIPEFS_MAGIC: u64 = 0x5049_5045;
const PROC_SUPER_MAGIC: u64 = 0x9fa0;
const DEVPTS_SUPER_MAGIC: u64 = 0x1cd1;
const SYSFS_MAGIC: u64 = 0x6265_6572;
const CGROUP2_SUPER_MAGIC: u64 = 0x6367_7270;
const OVERLAYFS_SUPER_MAGIC: u64 = 0x794c_7630;
/// Device number of the PTY master device (`/dev/ptmx`).
const PTMX_RDEV: u64 = rdev(5, 2);

/// ioctls that can be passed through to a host fd (the terminal, or the pipe or file `tuxport-run`
/// was given): number, argument size, whether the argument is an output.
const HOST_IOCTLS: [(u32, usize, bool); 15] = [
    (0x5401, 36, true),       // TCGETS
    (0x5402, 36, false),      // TCSETS
    (0x5403, 36, false),      // TCSETSW
    (0x5404, 36, false),      // TCSETSF
    (0x5405, 18, true),       // TCGETA
    (0x5406, 18, false),      // TCSETA
    (0x5407, 18, false),      // TCSETAW
    (0x5408, 18, false),      // TCSETAF
    (0x5413, 8, true),        // TIOCGWINSZ
    (0x5414, 8, false),       // TIOCSWINSZ
    (0x802c_542a, 44, true),  // TCGETS2
    (0x402c_542b, 44, false), // TCSETS2
    (0x402c_542c, 44, false), // TCSETSW2
    (0x402c_542d, 44, false), // TCSETSF2
    (FIONREAD, 4, true),
];
const FIONREAD: u32 = 0x541b;
const RNDGETENTCNT: u32 = 0x8004_5200;
const RNDADDTOENTCNT: u32 = 0x4004_5201;
const RNDADDENTROPY: u32 = 0x4008_5203;
const RNDZAPENTCNT: u32 = 0x5204;
const RNDCLEARPOOL: u32 = 0x5206;
const RNDRESEEDCRNG: u32 = 0x5207;
/// Number of bits in the entropy pool (`/proc/sys/kernel/random/entropy_avail`).
pub(crate) const ENTROPY: i32 = 256;

/// ioctls of `/dev/random` and `/dev/urandom` (Linux 6.1's `random_ioctl`). Random numbers come
/// from the host, and as on Linux since 5.18 the entropy pool is always full: added data and
/// entropy only have their arguments checked and are read once, with no effect (seedrng adds the
/// seed saved last time back with `RNDADDENTROPY` at boot). Changing the entropy pool needs
/// `CAP_SYS_ADMIN` in the initial namespace; unknown requests are EINVAL (tty requests too, not
/// ENOTTY, as on Linux).
fn random_ioctl(cx: &mut Cx<'_>, request: u32, arg: u64) -> SysResult {
    let admin = || {
        if cx.cred().capable(cap::SYS_ADMIN) {
            Ok(())
        } else {
            Err(Errno::EPERM)
        }
    };
    match request {
        RNDGETENTCNT => put(cx, arg, ENTROPY.to_le_bytes()).map(|()| 0),
        RNDADDTOENTCNT => {
            admin()?;
            if i32::from_le_bytes(get(cx, arg)?) < 0 {
                return Err(Errno::EINVAL);
            }
            Ok(0)
        }
        // struct rand_pool_info: entropy bit count, data byte count, then the data; all of the
        // data must be readable
        RNDADDENTROPY => {
            admin()?;
            if i32::from_le_bytes(get(cx, arg)?) < 0 {
                return Err(Errno::EINVAL);
            }
            let len = i32::from_le_bytes(get(cx, arg + 4)?);
            let len = usize::try_from(len).map_err(|_| Errno::EFAULT)?;
            let mut buf = [0u8; 4096];
            let mut done = 0;
            while done < len {
                let n = (len - done).min(buf.len());
                cx.mm()
                    .read(UserSlice::new(arg + 8 + done as u64, n), &mut buf[..n])?;
                done += n;
            }
            Ok(0)
        }
        RNDZAPENTCNT | RNDCLEARPOOL | RNDRESEEDCRNG => admin().map(|()| 0),
        _ => Err(Errno::EINVAL),
    }
}
const FIONBIO: u32 = 0x5421;
const FIOASYNC: u32 = 0x5452;
const FIOSETOWN: u32 = 0x8901;
const SIOCSPGRP: u32 = 0x8902;
const FIOGETOWN: u32 = 0x8903;
const SIOCGPGRP: u32 = 0x8904;
const FIONCLEX: u32 = 0x5450;
const FIOCLEX: u32 = 0x5451;
const FICLONE: u32 = 0x4004_9409;
const FICLONERANGE: u32 = 0x4020_940d;
const FIDEDUPERANGE: u32 = 0xc018_9436;

const POLLIN: i16 = 0x001;

// ---- Opening, closing, duplicating ----

/// The status flags an open file keeps from open, as Linux keeps them in `f_flags`: F_GETFL
/// shows them, and `O_DSYNC`/`O_SYNC` make every write wait for the disk ([`write_sync`]).
const KEPT_AT_OPEN: u32 = oflags::O_ACCMODE
    | oflags::O_APPEND
    | oflags::O_NONBLOCK
    | oflags::O_PATH
    | oflags::O_DSYNC
    | oflags::O_SYNC_BIT;

/// The status flags an open file keeps from `flags`: with `O_PATH` only `O_PATH` (Linux's
/// build_open_how drops the access mode and every other flag but `O_DIRECTORY`, `O_NOFOLLOW` and
/// `O_CLOEXEC`), otherwise [`KEPT_AT_OPEN`].
fn kept(flags: u32) -> u32 {
    if flags & oflags::O_PATH != 0 {
        oflags::O_PATH
    } else {
        KEPT_AT_OPEN
    }
}

/// Type and length in a file handle (`struct file_handle`): the handle is the store inode
/// number.
const FILE_HANDLE_TYPE: i32 = 1;
const FILE_HANDLE_BYTES: u32 = 8;
/// Type written back when the handle does not fit (Linux's `FILEID_INVALID`).
const FILEID_INVALID: i32 = 255;
const MAX_HANDLE_SZ: u32 = 128;
const AT_HANDLE_FID: u32 = 0x200;

pub(super) fn openat(cx: &mut Cx<'_>, dirfd: i32, path: u64, flags: u32, perm: u32) -> SysResult {
    // build_open_flags checks O_TMPFILE for open(2) as for openat2 (O_PATH has dropped it): it
    // must come with O_DIRECTORY and without O_CREAT, and be writable
    const O_TMPFILE_BIT: u32 = 0o20000000; // __O_TMPFILE
    let o_tmpfile = O_TMPFILE_BIT | oflags::O_DIRECTORY;
    if flags & (O_TMPFILE_BIT | oflags::O_PATH) == O_TMPFILE_BIT
        && (flags & (o_tmpfile | oflags::O_CREAT) != o_tmpfile
            || flags & oflags::O_ACCMODE == oflags::O_RDONLY)
    {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    let file = open_path(cx, dirfd, path, flags, perm)?;
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

/// openat2 (Linux 6.1's `fs/open.c`): the flags, mode and resolve of `struct open_how`. Unlike
/// openat, unknown flags and a mode that should not be there are EINVAL, and path resolution
/// carries the `resolve` restrictions ([`crate::vfs::resolve`]).
pub(super) fn openat2(cx: &mut Cx<'_>, dirfd: i32, path: u64, how: u64, size: usize) -> SysResult {
    use crate::vfs::resolve;
    const OPEN_HOW_SIZE_VER0: usize = 24;
    /// `FMODE_NONOTIFY`: user space should not pass it; it is removed along with `O_CLOEXEC`
    /// and is not an error.
    const FMODE_NONOTIFY: u64 = 0x400_0000;
    const O_SYNC_BIT: u64 = 0o4000000; // __O_SYNC
    const O_DSYNC: u64 = 0o10000;
    const O_NOATIME: u64 = oflags::O_NOATIME as u64;
    const O_TMPFILE_BIT: u64 = 0o20000000; // __O_TMPFILE
    const VALID_OPEN_FLAGS: u64 = (oflags::O_ACCMODE
        | oflags::O_CREAT
        | oflags::O_EXCL
        | oflags::O_NOCTTY
        | oflags::O_TRUNC
        | oflags::O_APPEND
        | oflags::O_NONBLOCK
        | oflags::O_ASYNC
        | oflags::O_DIRECT
        | oflags::O_LARGEFILE
        | oflags::O_DIRECTORY
        | oflags::O_NOFOLLOW
        | oflags::O_CLOEXEC
        | oflags::O_PATH) as u64
        | O_SYNC_BIT
        | O_DSYNC
        | O_NOATIME
        | O_TMPFILE_BIT;
    const O_PATH_FLAGS: u64 =
        (oflags::O_DIRECTORY | oflags::O_NOFOLLOW | oflags::O_PATH | oflags::O_CLOEXEC) as u64;
    if size < OPEN_HOW_SIZE_VER0 {
        return Err(Errno::EINVAL);
    }
    if size > 4096 {
        return Err(Errno::E2BIG);
    }
    // copy_struct_from_user: anything past 24 bytes must be all zero (fields added by later
    // versions)
    let mut raw = vec![0u8; size];
    cx.mm().read(UserSlice::new(how, size), &mut raw)?;
    if raw[OPEN_HOW_SIZE_VER0..].iter().any(|&b| b != 0) {
        return Err(Errno::E2BIG);
    }
    let field = |i: usize| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap());
    let (mut flags, mode, res) = (field(0), field(1), field(2));
    // O_LARGEFILE is added only without O_PATH
    if flags & oflags::O_PATH as u64 == 0 {
        flags |= oflags::O_LARGEFILE as u64;
    }
    // The checks of build_open_flags, in its order
    let f = flags & !(oflags::O_CLOEXEC as u64 | FMODE_NONOTIFY);
    if f & !VALID_OPEN_FLAGS != 0 || res & !resolve::ALL != 0 {
        return Err(Errno::EINVAL);
    }
    if res & resolve::SCOPED == resolve::SCOPED {
        return Err(Errno::EINVAL);
    }
    let will_create = f & (oflags::O_CREAT as u64 | O_TMPFILE_BIT) != 0;
    if (will_create && mode & !0o7777 != 0) || (!will_create && mode != 0) {
        return Err(Errno::EINVAL);
    }
    // O_TMPFILE must come with O_DIRECTORY and without O_CREAT, and must be writable
    let o_tmpfile = O_TMPFILE_BIT | oflags::O_DIRECTORY as u64;
    if f & O_TMPFILE_BIT != 0
        && (f & (o_tmpfile | oflags::O_CREAT as u64) != o_tmpfile
            || f & oflags::O_ACCMODE as u64 == oflags::O_RDONLY as u64)
    {
        return Err(Errno::EINVAL);
    }
    if f & oflags::O_PATH as u64 != 0 && f & !O_PATH_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    // RESOLVE_CACHED: creation, truncation and O_TMPFILE (which includes O_DIRECTORY, so
    // O_DIRECTORY counts too) are not tried; everything else resolves as usual, since Tuxport's
    // resolution waits for nothing but host I/O
    let busy = (oflags::O_TRUNC | oflags::O_CREAT | oflags::O_DIRECTORY) as u64 | O_TMPFILE_BIT;
    if res & resolve::CACHED != 0 && f & busy != 0 {
        return Err(Errno::EAGAIN);
    }
    let path = read_path(cx, path)?;
    let flags = flags as u32;
    let file = open_path_with(cx, dirfd, path, flags, mode as u32, res)?;
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

/// The kernel creates a file itself (core dump): relative to the current directory, EEXIST when
/// it already exists, symlinks not followed, opened write-only.
pub(crate) fn create_exclusive(cx: &mut Cx<'_>, path: &[u8], perm: u32) -> Result<FileRef, Errno> {
    let flags = oflags::O_CREAT | oflags::O_EXCL | oflags::O_NOFOLLOW | oflags::O_WRONLY;
    open_path(cx, AT_FDCWD, path.to_vec(), flags, perm)
}

/// The kernel deletes a file itself (the old file of the same name before a core dump), relative
/// to the current directory.
pub(crate) fn unlink_path(cx: &mut Cx<'_>, path: &[u8]) -> Result<(), Errno> {
    unlink(cx, AT_FDCWD, path.to_vec(), 0)
}

/// The kernel writes an open file itself at `offset` (core dump), as pwrite does.
pub(crate) fn write_at(
    cx: &mut Cx<'_>,
    file: &FileRef,
    data: &[u8],
    offset: u64,
) -> Result<usize, Errno> {
    write_bytes(cx, file, data, Some(offset))
}

/// Opens (or creates) `path` for openat, not yet put in the fd table.
fn open_path(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: Vec<u8>,
    flags: u32,
    perm: u32,
) -> Result<FileRef, Errno> {
    open_path_with(cx, dirfd, path, flags, perm, 0)
}

/// [`open_path`], with path resolution carrying openat2's restrictions ([`crate::vfs::resolve`]).
fn open_path_with(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: Vec<u8>,
    flags: u32,
    perm: u32,
    res: u64,
) -> Result<FileRef, Errno> {
    use crate::vfs::resolve;
    let vfs = cx.vfs()?;
    let (start, root) = if res == 0 {
        (start_dir(cx, dirfd, &path)?, cx.root())
    } else {
        openat2_start(cx, dirfd, &path, res)?
    };
    let create = flags & oflags::O_CREAT != 0;
    let excl = create && flags & oflags::O_EXCL != 0;
    let follow = flags & oflags::O_NOFOLLOW == 0 && !excl;
    let cred = cx.cred();

    let resolved = vfs.walk_resolve(&cx.view(&vfs), &cred, &root, &start, &path, follow, res);
    // A queue was created in the mqueue filesystem (open permission is not checked again, as for
    // files just created in the store)
    let mut mq_created = false;
    // A file was created and opened in a FUSE filesystem
    let mut fuse_created = None;
    // parent is the last directory and name passed through, for reporting inotify events
    let (node, created, parent) = match resolved {
        Ok(_) if excl => return Err(Errno::EEXIST),
        Ok((node, found)) => (node, None, found),
        Err(e) if e == Errno::ENOENT && create => {
            // When the last component is a dangling symlink, follow it and create the target
            // file, as on Linux
            let mut root_set = path.first() == Some(&b'/')
                || res & resolve::SCOPED != 0
                || path.split(|&b| b == b'/').any(|c| c == b"..");
            let (mut start, mut path) = (start.clone(), path.clone());
            let mut links = 0;
            loop {
                let (dir, name) =
                    vfs.resolve_parent_with(&cx.view(&vfs), &cred, &root, &start, &path, res)?;
                if path.ends_with(b"/") || name == b"." || name == b".." {
                    return Err(Errno::EISDIR);
                }
                // In the mqueue filesystem a message queue is created (Linux's mqueue_create),
                // with default attributes
                if let Node::Pseudo(mnt, PseudoNode::MqRoot(ns)) = dir {
                    if vfs.readonly(&dir) {
                        return Err(Errno::EROFS);
                    }
                    let ipc = cx.kr().ipc(ns);
                    let dir_st = ipc.mq_dir.borrow().stat(ipc.mqueues.len());
                    cred.permission(&dir_st, MAY_WRITE | MAY_EXEC)?;
                    let perm = perm & !cx.umask();
                    let q = super::mqueue::new_queue(ipc, ns, &cred, perm, None)?;
                    let ino = q.borrow().ino;
                    super::mqueue::insert_queue(cx.k().ipc_mut(ns), name.to_vec(), q);
                    mq_created = true;
                    break (
                        Node::Pseudo(mnt, PseudoNode::Mq(ns, ino)),
                        None,
                        Some((dir.clone(), name.to_vec())),
                    );
                }
                // Files cannot be created in cgroup directories (they have no create; Linux's
                // vfs_create is EACCES)
                if let Node::Pseudo(_, PseudoNode::Cgroup(..)) = dir
                    && !vfs.readonly(&dir)
                {
                    return Err(Errno::EACCES);
                }
                // A dangling symlink under the name (in an overlay, as the overlay shows it)
                let link = match &dir {
                    Node::Overlay(mnt, o) => vfs.overlay_child((*mnt, *o), name).ok(),
                    Node::Fuse(mnt, d) => match vfs.fuse_child(*d, name) {
                        Ok(c) => Some(Node::Fuse(*mnt, c)),
                        Err(e) if e == Errno::ENOENT => None,
                        Err(e) => return Err(e),
                    },
                    _ => {
                        let (mnt, d) = store_dir(&vfs, &dir)?;
                        vfs.store().lookup(d, name)?.map(|i| Node::Store(mnt, i))
                    }
                };
                if let Some(link) = link
                    && follow
                    && vfs.kind(&cx.view(&vfs), &link)? == mode::S_IFLNK
                {
                    links += 1;
                    if links > crate::vfs::MAX_SYMLINKS {
                        return Err(Errno::ELOOP);
                    }
                    // As in walk (pick_link, nd_jump_root): RESOLVE_NO_SYMLINKS and nosymfollow
                    // mounts do not follow; under NO_XDEV an absolute target must be in the same
                    // mount as the root
                    if res & resolve::NO_SYMLINKS != 0
                        || vfs
                            .mount_flags(&link)
                            .is_some_and(|f| f & ms::NOSYMFOLLOW != 0)
                    {
                        return Err(Errno::ELOOP);
                    }
                    path = vfs.readlink(&cx.view(&vfs), &link)?;
                    // Whether resolution has taken the root: approximated from the original path
                    // (absolute, scoped, containing `..`, or a jump already made here); those in
                    // earlier symlinks do not count
                    if path.first() == Some(&b'/')
                        && res & resolve::NO_XDEV != 0
                        && (!root_set || dir.mnt() != root.mnt())
                    {
                        return Err(Errno::EXDEV);
                    }
                    root_set |= path.first() == Some(&b'/');
                    start = dir;
                    continue;
                }
                // In a FUSE filesystem the daemon creates and opens it
                if let Node::Fuse(mnt, d) = dir {
                    let (id, kind) =
                        super::fuse::create(cx, &cred, &vfs, (mnt, d), name, perm, flags)?;
                    fuse_created = Some(kind);
                    break (
                        Node::Fuse(mnt, id),
                        None,
                        Some((dir.clone(), name.to_vec())),
                    );
                }
                let (mnt, d) = create_dir(&vfs, &dir)?;
                let dir_st = may_create(&vfs, &cred, (mnt, d), name)?;
                let (uid, gid, perm) = cred.new_owner(&dir_st, perm, cx.umask(), false);
                let tmpfs = tmpfs_reserve(&vfs, (mnt, d), 0)?;
                let (ino, file) = create_in(
                    &vfs,
                    (mnt, d),
                    name,
                    || vfs.store().create_file(d, name, perm, uid, gid),
                    |(ino, _)| *ino,
                )?;
                tmpfs_created(&vfs, tmpfs, 0);
                // In an overlay the new file is the overlay's node of that name
                if let Node::Overlay(mnt, o) = dir {
                    break (
                        vfs.overlay_child((mnt, o), name)?,
                        Some(file),
                        Some((dir.clone(), name.to_vec())),
                    );
                }
                break (
                    Node::Store(mnt, ino),
                    Some(file),
                    Some((Node::Store(mnt, d), name.to_vec())),
                );
            }
        }
        Err(e) => return Err(e),
    };

    let mut display = display_path(cx, &vfs, &start, &path, &node, parent.as_ref());
    // A namespace shows as `mnt:[number]` and the like in `/proc/<pid>/fd/<n>`; a namespace file
    // bind-mounted elsewhere opens as the mount point's path (Linux's d_path)
    if let Node::Pseudo(mnt, p @ PseudoNode::NsFile(..)) = &node
        && vfs.mount(*mnt).is_none_or(|m| m.fs != Fs::Nsfs)
    {
        display = Some(Rc::from(cx.view(&vfs).name(*mnt, p)));
    }
    let was_created = created.is_some() || fuse_created.is_some();
    let via_devpts = matches!(node, Node::Pseudo(_, PseudoNode::Pts(..)));
    let mut opened_ino = inotify::key_of(&node).filter(|_| flags & oflags::O_PATH == 0);
    // A file without a name reopened through /proc/<pid>/fd or exe is the same file again, as
    // Linux reopens its dentry: on the mount it was opened or run through, with the path it shows
    // (`/memfd:<name> (deleted)`, `pipe:[<n>]`). Being opened, it counts as a writer
    // ([`OpenFile::pseudo`] stays unset)
    let reopened = match &node {
        Node::Pseudo(_, PseudoNode::OpenFd(pid, fd)) if flags & oflags::O_PATH == 0 => {
            cx.view(&vfs).open_file(*pid, *fd).map(|f| {
                let f = f.borrow();
                let ino = match f.kind {
                    FileKind::Regular { ino, .. } => Some(ino),
                    _ => None,
                };
                (f.mnt, f.path.clone(), ino)
            })
        }
        Node::Pseudo(_, PseudoNode::OpenExe(pid)) if flags & oflags::O_PATH == 0 => {
            exe_file(cx, *pid).ok().and_then(|e| {
                let (mnt, ino) = e.node?;
                Some((mnt, Some(e.path.clone()), Some(ino)))
            })
        }
        _ => None,
    };
    if let Some((mnt, path, ino)) = &reopened {
        display = path.clone();
        opened_ino = ino.and_then(|ino| inotify::key_of(&Node::Store(*mnt, ino)));
    }
    // Files in the store record the mount they were reached through ([`OpenFile::mnt`]), and
    // those in an overlay their overlay node too ([`OpenFile::ovl`])
    let store_mnt = node.ino().map(|_| node.mnt());
    let fuse_mnt = match &node {
        Node::Fuse(mnt, _) => Some(*mnt),
        _ => None,
    };
    let overlay = match &node {
        Node::Overlay(mnt, o) => Some((*mnt, *o)),
        _ => None,
    };
    let kind = if flags & oflags::O_PATH != 0 {
        if flags & oflags::O_DIRECTORY != 0 && !vfs.is_dir(&cx.view(&vfs), &node)? {
            return Err(Errno::ENOTDIR);
        }
        FileKind::Path(node)
    } else {
        match node {
            Node::Store(mnt, ino) => {
                // A just-created file is already empty, so it is not truncated: ext4 treats a
                // file written after truncation to 0 as a whole replacement and starts writeback
                // right at close, which is slower than the write itself
                let flags = if was_created {
                    flags & !oflags::O_TRUNC
                } else {
                    flags
                };
                let beside = parent.as_ref().map(|(dir, _)| dir);
                let how = StoreOpen::by_path(created, beside);
                open_store(cx, &cred, &vfs, (mnt, ino), flags, how)?
            }
            Node::Pseudo(_, PseudoNode::Mq(ns, ino)) => {
                open_mq(cx, &cred, (ns, ino), flags, mq_created)?
            }
            Node::Pseudo(mnt, p) => open_pseudo(cx, &cred, &vfs, mnt, p, flags)?,
            Node::Overlay(mnt, o) => {
                let flags = if was_created {
                    flags & !oflags::O_TRUNC
                } else {
                    flags
                };
                let beside = parent.as_ref().map(|(dir, _)| dir);
                open_overlay(cx, &cred, &vfs, (mnt, o), flags, (created, beside))?
            }
            Node::Fuse(..) if fuse_created.is_some() => fuse_created.take().unwrap(),
            Node::Fuse(mnt, f) => super::fuse::open(cx, &cred, &vfs, (mnt, f), flags)?,
        }
    };
    // A device node like /dev/tty opens a specific slave: its path is the one in devpts (fd links
    // are computed by the mount reached through); the master, and slaves opened through devpts,
    // keep the opened path as usual
    if let FileKind::Pty(end) = &kind
        && !end.master
        && !via_devpts
    {
        display = None;
    }
    let overlay = overlay.filter(|_| {
        matches!(
            kind,
            FileKind::Regular { .. } | FileKind::Device { .. } | FileKind::Pipe(_)
        )
    });
    let file = OpenFile::with_path(kind, flags & kept(flags) | largefile(flags), display);
    if let Some(mnt) = store_mnt {
        file.borrow_mut().mnt = mnt;
    }
    if let Some((mnt, ..)) = reopened {
        file.borrow_mut().mnt = mnt;
    }
    if let Some(mnt) = fuse_mnt {
        file.borrow_mut().mnt = mnt;
    }
    if let Some((mnt, o)) = overlay {
        let mut f = file.borrow_mut();
        f.mnt = mnt;
        f.ovl = Some(o);
        f.hold(&Node::Overlay(mnt, o));
    }
    if let Some((dir, _)) = &parent {
        file.borrow_mut().hold(dir);
    }
    file.borrow_mut().dentry = parent.clone();
    if let Some(ino) = opened_ino {
        open_events(cx, &file, ino, parent, was_created, flags);
    }
    Ok(file)
}

/// Start and root directory of openat2 (path_init). With a scope (`BENEATH`, `IN_ROOT`) the root
/// is the start; absolute `IN_ROOT` paths also start at dirfd; other absolute paths ignore dirfd
/// (resolution reports EXDEV for `BENEATH` ones).
fn openat2_start(cx: &Cx<'_>, dirfd: i32, path: &[u8], flags: u64) -> Result<(Node, Node), Errno> {
    use crate::vfs::resolve;
    if path.is_empty() {
        return Err(Errno::ENOENT);
    }
    if path[0] == b'/' && flags & resolve::IN_ROOT == 0 {
        let root = cx.root();
        return Ok((root.clone(), root));
    }
    let start = if dirfd == AT_FDCWD {
        cx.cwd()
    } else {
        let file = cx.fds().get(dirfd)?;
        let node = file.borrow().node().ok_or(Errno::ENOTDIR)?;
        let vfs = cx.vfs()?;
        if !vfs.is_dir(&cx.view(&vfs), &node)? {
            return Err(Errno::ENOTDIR);
        }
        node
    };
    let root = if flags & resolve::SCOPED != 0 {
        start.clone()
    } else {
        cx.root()
    };
    Ok((start, root))
}

/// Opens a queue in the mqueue filesystem: the resulting fd is the same as mq_open's (on Linux it
/// is the same kind of file), and mq_timedsend and the like work on it. A just-created one is not
/// checked for permission again.
fn open_mq(
    cx: &Cx<'_>,
    cred: &Cred,
    (ns, ino): (u64, u64),
    flags: u32,
    created: bool,
) -> Result<FileKind, Errno> {
    if flags & oflags::O_DIRECTORY != 0 {
        return Err(Errno::ENOTDIR);
    }
    let q = mq_queue(cx, ns, ino)?;
    if !created {
        cred.permission(&q.borrow().stat(), open_mask(flags))?;
    }
    Ok(FileKind::MessageQueue(q))
}

/// Opens a node in a synthetic filesystem.
fn open_pseudo(
    cx: &mut Cx<'_>,
    cred: &Cred,
    vfs: &Vfs,
    mnt: crate::vfs::MountId,
    node: PseudoNode,
    flags: u32,
) -> Result<FileKind, Errno> {
    let accmode = flags & oflags::O_ACCMODE;
    // O_TRUNC asks for write permission too (Linux's build_open_flags), although nothing here can
    // be truncated
    let mask = open_mask(flags)
        | if flags & oflags::O_TRUNC != 0 {
            MAY_WRITE
        } else {
            0
        };
    // /proc/sys/user/max_*: anyone may read the limits of their user namespace, and holders of
    // CAP_SYS_RESOURCE in it may write them, whoever owns the files (Linux's set_permissions for
    // the per-user-namespace sysctl tables)
    let user_limit = matches!(&node, PseudoNode::Static(p) if p.starts_with(b"sys/user/max_"));
    // Namespace files are immutable (nsfs): asking to write is EPERM before any permission check
    if matches!(node, PseudoNode::NsFile(..)) && mask & MAY_WRITE != 0 {
        return Err(Errno::EPERM);
    }
    if user_limit {
        if mask & MAY_WRITE != 0 && !cred.capable_here(cap::SYS_RESOURCE) {
            return Err(Errno::EACCES);
        }
    } else if let Some(net) = pseudo::sysctl_kind(&node) {
        // The rest of /proc/sys (proc_sys_permission, sysctl_perm, test_perm): the mode's owner
        // bits for euid 0, its group bits for a member of group 0, the others' otherwise, whatever
        // the capabilities; a net.* sysctl gives its owner bits to anyone with CAP_NET_ADMIN over
        // the network namespace's owner (net_ctl_permissions), so root of a container's user
        // namespace may write its own namespace's
        let st = pseudo::stat(&cx.view(vfs), &node)?;
        let mut m = st.mode & 0o777;
        if net {
            let owner = cx.task.ns.borrow().net.owner.clone();
            if cred.ns_capable(&owner, cap::NET_ADMIN) {
                let o = m >> 6;
                m = o << 6 | o << 3 | o;
            }
        }
        let bits = if cred.euid == 0 {
            m >> 6
        } else if cred.egid == 0 || cred.groups.binary_search(&0).is_ok() {
            m >> 3
        } else {
            m
        } & 7;
        if mask & 7 & !bits != 0 {
            return Err(Errno::EACCES);
        }
    } else if !matches!(node, PseudoNode::OpenFd(..) | PseudoNode::OpenExe(_)) {
        cred.permission(&pseudo::stat(&cx.view(vfs), &node)?, mask)?;
    }
    // /proc/<pid>/mem requires being able to trace that process, as ptrace does
    // (PTRACE_MODE_ATTACH); auxv, environ, maps, smaps and pagemap require being able to trace it
    // read-only (PTRACE_MODE_READ, same rules): even for a process of the same user they cannot
    // be opened when it is not dumpable. Both use the filesystem credentials (proc_mem_open adds
    // PTRACE_MODE_FSCREDS)
    let mut mm = None;
    if let PseudoNode::Entry(
        pid,
        tid,
        e @ (pseudo::Entry::Mem
        | pseudo::Entry::Auxv
        | pseudo::Entry::Environ
        | pseudo::Entry::Maps
        | pseudo::Entry::Smaps
        | pseudo::Entry::SmapsRollup
        | pseudo::Entry::Pagemap),
    ) = node
    {
        // pid may name any thread (/proc/<tid>, pseudo::group_of): the check is on that thread
        pseudo::group_of(cx.kr(), pid).ok_or(Errno::ESRCH)?;
        if !super::proc::may_ptrace_fs(cx, tid.unwrap_or(pid)) {
            return Err(Errno::EACCES);
        }
        // mem and pagemap work on the named thread's address space: one that has exited (a
        // zombie, a main thread whose process lives on) has none, ESRCH at open as on Linux
        if matches!(e, pseudo::Entry::Mem | pseudo::Entry::Pagemap) {
            let task = cx
                .kr()
                .tasks
                .get(&tid.unwrap_or(pid))
                .cloned()
                .ok_or(Errno::ESRCH)?;
            if e == pseudo::Entry::Mem {
                mm = Some(Rc::downgrade(&task.res.borrow().mm));
            }
        }
    }
    // Writing setgroups needs CAP_SYS_ADMIN in the process's user namespace, checked at open
    // (proc_setgroups_open)
    if let PseudoNode::Entry(pid, _, pseudo::Entry::Setgroups) = node
        && mask & MAY_WRITE != 0
    {
        let group = pseudo::group_of(cx.kr(), pid).ok_or(Errno::ESRCH)?;
        if !cred.ns_capable(&group.borrow().cred.user_ns, cap::SYS_ADMIN) {
            return Err(Errno::EACCES);
        }
    }
    // Device nodes cannot be opened in a nodev mount (Linux's may_open)
    if matches!(node, PseudoNode::Pts(..) | PseudoNode::PtsPtmx(_))
        && vfs.mount(mnt).is_some_and(|m| m.flags & ms::NODEV != 0)
    {
        return Err(Errno::EACCES);
    }
    // devpts terminals also go through the cgroup device filter
    let rdev = match node {
        PseudoNode::Pts(_, n) => Some(rdev(PTS_MAJOR, n)),
        PseudoNode::PtsPtmx(_) => Some(PTMX_RDEV),
        _ => None,
    };
    if let Some(r) = rdev {
        super::bpf::check_device(cx, mode::S_IFCHR, r, device_access(mask))?;
    }
    // No synthetic file supports direct I/O (do_dentry_open: EINVAL), terminals included; a
    // directory opened for writing is EISDIR first. Files reopened through /proc/<pid>/fd are
    // their own kind
    if flags & oflags::O_DIRECT != 0
        && !matches!(node, PseudoNode::OpenFd(..) | PseudoNode::OpenExe(_))
        && (accmode == oflags::O_RDONLY || Pseudo::kind(&cx.view(vfs), &node)? != mode::S_IFDIR)
    {
        return Err(Errno::EINVAL);
    }
    match node {
        PseudoNode::Pts(fs, n) => return tty::open_slave(cx, (fs, n, mnt, true), flags),
        PseudoNode::PtsPtmx(fs) => {
            return tty::open_ptmx(cx.k(), (fs, mnt, true), (cred.fsuid, cred.fsgid));
        }
        PseudoNode::OpenFd(pid, fd) => return reopen(cx, cred, vfs, pid, fd, flags),
        PseudoNode::OpenExe(pid) => return reopen_exe(cx, cred, vfs, pid, flags),
        _ => {}
    }
    let view = cx.view(vfs);
    // With O_NOFOLLOW it stopped at a symlink (such as /proc/self/fd/<n>)
    if Pseudo::kind(&view, &node)? == mode::S_IFLNK {
        return Err(Errno::ELOOP);
    }
    let full = Node::Pseudo(mnt, node.clone());
    if vfs.is_dir(&view, &full)? {
        if accmode != oflags::O_RDONLY {
            return Err(Errno::EISDIR);
        }
        // /proc/<pid> remembers which process it is: Linux's open file references a struct pid,
        // so after the process is reaped, using it as a pidfd is ESRCH even if the pid has been
        // given to a new process
        let process = match node {
            PseudoNode::Pid(pid, None) => pseudo::group_of(cx.kr(), pid).map(Rc::downgrade),
            _ => None,
        };
        return Ok(FileKind::Directory {
            node: full,
            entries: None,
            process,
            fuse: None,
        });
    }
    if flags & oflags::O_DIRECTORY != 0 {
        return Err(Errno::ENOTDIR);
    }
    if mask & MAY_WRITE != 0 && vfs.readonly(&full) {
        return Err(Errno::EROFS);
    }
    // cgroup files are checked only by permission bits (root can open read-only ones; writing is
    // what gets EINVAL), the others by pseudo::open_unwritable
    if mask & MAY_WRITE != 0 && !pseudo::writable(&node) && !matches!(node, PseudoNode::Cgroup(..))
    {
        pseudo::open_unwritable(&node, accmode != oflags::O_RDONLY)?;
    }
    // Network sysctls fail only when read (an unset stable_secret is EIO, Linux's
    // proc_sys_call_handler): open starts with empty contents and the first read regenerates them.
    // So does /proc/<pid>/syscall, which shows the thread as it is when read (/proc/self/syscall
    // shows the read) and checks ptrace access then
    let mut fresh = true;
    let data = if pseudo::unreadable(&node) {
        Vec::new()
    } else if matches!(node, PseudoNode::Entry(_, _, pseudo::Entry::Syscall)) {
        fresh = false;
        Vec::new()
    } else {
        match pseudo::content(&view, mnt, &node) {
            Err(_) if matches!(node, PseudoNode::NetConf(..)) => {
                fresh = false;
                Vec::new()
            }
            r => r?,
        }
    };
    // /proc/kmsg (`kmsg_open`): open checks permission as syslog's open does, and later reads do
    // not check again
    let klog = match node {
        PseudoNode::Global(pseudo::Global::Kmsg) => {
            super::misc::do_syslog(cx, super::misc::syslog_action::OPEN, 0, 0, true)?;
            Some(cx.kr().klog.clone())
        }
        _ => None,
    };
    // A namespace file holds that namespace
    let ns = match node {
        PseudoNode::NsFile(kind, id) => {
            Some(super::ns::find(cx.kr(), Some(vfs), kind, id).ok_or(Errno::ENOENT)?)
        }
        _ => None,
    };
    // Change notifications: the mount table for mounts and mountinfo, a cgroup file's changes
    // for the files cgroup2 notifies, none (but kernfs's first-read rule) for other sysfs and
    // cgroup2 files
    let event = match node {
        PseudoNode::Entry(_, _, pseudo::Entry::Mounts | pseudo::Entry::MountInfo) => {
            Some(crate::fd::ProcEvent::mounts(vfs.mount_events()))
        }
        PseudoNode::Cgroup(id, name) if !name.is_empty() => Some(crate::fd::ProcEvent::kernfs(
            crate::cgroup::observe(cx.k(), id, name)
                .unwrap_or_else(|| Rc::new(std::cell::Cell::new(1))),
        )),
        PseudoNode::Sys(_)
        | PseudoNode::SysCpu(..)
        | PseudoNode::SysNet(..)
        | PseudoNode::SysBlock(..) => Some(crate::fd::ProcEvent::kernfs(Rc::new(
            std::cell::Cell::new(1),
        ))),
        _ => None,
    };
    Ok(FileKind::Proc {
        mnt,
        node,
        data,
        fresh,
        mm,
        ns,
        klog,
        event,
        opener: Rc::new(cred.clone()),
    })
}

/// Reopens a file without a path through `/proc/<pid>/fd/<n>`: a pipe gives an end of the same
/// pipe, a host fd the same host fd, and a deleted regular file (memfd, or one still open after
/// deletion) opens the same host file through the host's `/proc/self/fd` (the object in the
/// store is already deleted), with the checks of an open on the mount it was opened through
/// ([`StoreOpen::Reopened`]).
fn reopen(
    cx: &mut Cx<'_>,
    cred: &Cred,
    vfs: &Vfs,
    pid: i32,
    fd: i32,
    flags: u32,
) -> Result<FileKind, Errno> {
    let accmode = flags & oflags::O_ACCMODE;
    let file = cx.view(vfs).open_file(pid, fd).ok_or(Errno::ENOENT)?;
    let f = file.borrow();
    if let FileKind::Regular {
        ino, file: host, ..
    } = &f.kind
    {
        let ino = *ino;
        if flags & oflags::O_TRUNC != 0 && accmode != oflags::O_RDONLY {
            check_seals(cx, &f, false, |_| 0)?;
        }
        // The host fd is always readable, and also writable when originally opened for write: if
        // that suffices, duplicate it (reads and writes are positional, with offsets kept by each
        // open file). Otherwise reopen it through the host's /proc/self/fd; deleted files cannot
        // be opened that way on filesystems like virtiofs. A held file's descriptions only read
        // the shared object (open_store)
        let new = if accmode == oflags::O_RDONLY || f.writable() || vfs.store().held(ino)? {
            host.try_clone()
        } else {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!(
                    "/proc/self/fd/{}",
                    std::os::fd::AsRawFd::as_raw_fd(host)
                ))
        }
        .map_err(host_errno)?;
        let (mnt, memfd) = (f.mnt, f.pseudo);
        drop(f);
        let how = StoreOpen::Reopened(new, memfd);
        return open_store(cx, cred, vfs, (mnt, ino), flags, how);
    }
    match &f.kind {
        FileKind::Pipe(end) => Ok(FileKind::Pipe(PipeEnd::open(
            &end.pipe,
            accmode != oflags::O_WRONLY,
            accmode != oflags::O_RDONLY,
        ))),
        FileKind::Host(fd) => Ok(FileKind::Host(*fd)),
        _ => Err(Errno::ENXIO),
    }
}

/// Opens a deleted program file through `/proc/<pid>/exe` ([`PseudoNode::OpenExe`]): duplicates
/// the recorded host file, as [`reopen`] does for deleted regular files, with the same checks (a
/// program still being executed cannot be opened for write: ETXTBSY).
fn reopen_exe(
    cx: &mut Cx<'_>,
    cred: &Cred,
    vfs: &Vfs,
    pid: i32,
    flags: u32,
) -> Result<FileKind, Errno> {
    let exe = exe_file(cx, pid)?;
    // A removed overlay program has no store node: it can be run (process::open_exe_file) but not
    // opened, since a descriptor of the store inode behind it would bypass the overlay
    let (Some((mnt, ino)), Some(host)) = (exe.node, &exe.host) else {
        return Err(Errno::ENOENT);
    };
    let accmode = flags & oflags::O_ACCMODE;
    let new = if accmode == oflags::O_RDONLY {
        host.try_clone()
    } else {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!(
                "/proc/self/fd/{}",
                std::os::fd::AsRawFd::as_raw_fd(&**host)
            ))
    }
    .map_err(host_errno)?;
    open_store(
        cx,
        cred,
        vfs,
        (mnt, ino),
        flags,
        StoreOpen::Reopened(new, false),
    )
}

/// inotify events of open, in the same order as Linux: create (reported to the directory),
/// truncation by `O_TRUNC`, open. `ino` is the file's key ([`inotify::key_of`]). Attaches an
/// [`inotify::Opened`] to the open file.
fn open_events(
    cx: &Cx<'_>,
    file: &FileRef,
    ino: Ino,
    parent: Option<(Node, Vec<u8>)>,
    created: bool,
    flags: u32,
) {
    let k = cx.kr();
    // A file created through an overlay was created in its upper directory first, which reports it
    // too (Linux's overlay creates it there through the VFS)
    let upper = match &parent {
        Some((Node::Overlay(_, d), _)) if created => {
            cx.vfs().ok().and_then(|v| v.overlay().node(*d).upper)
        }
        _ => None,
    };
    let parent = parent.and_then(|(dir, name)| Some((inotify::key_of(&dir)?, name)));
    if inotify::active(k) {
        if let (Some(up), Some((_, name))) = (upper, &parent) {
            inotify::dir_event(k, up, name, inotify::IN_CREATE, 0);
        }
        open_notify(k, file, ino, parent.as_ref(), created, flags);
    }
    let watch = inotify::opened(k, &file.borrow(), parent);
    file.borrow_mut().watch = watch;
}

fn open_notify(
    k: &Kernel,
    file: &FileRef,
    ino: Ino,
    parent: Option<&(Ino, Vec<u8>)>,
    created: bool,
    flags: u32,
) {
    let parent = parent.map(|(d, n)| (*d, n.as_slice()));
    let (dir, truncated) = {
        let f = file.borrow();
        let regular = matches!(f.kind, FileKind::Regular { .. });
        let dir = matches!(f.kind, FileKind::Directory { .. });
        let truncated = regular && f.writable() && flags & oflags::O_TRUNC != 0;
        (if dir { inotify::IN_ISDIR } else { 0 }, truncated)
    };
    match parent {
        Some((d, name)) if created => inotify::dir_event(k, d, name, inotify::IN_CREATE, 0),
        _ if truncated => inotify::path_event(k, ino, parent, inotify::IN_MODIFY),
        _ => {}
    }
    inotify::path_event(k, ino, parent, inotify::IN_OPEN | dir);
}

/// name_to_handle_at: the handle of a file in the store is its inode number; synthetic
/// filesystems do not support handles.
pub(super) fn name_to_handle_at(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    handle: u64,
    mount_id: u64,
    flags: u32,
) -> SysResult {
    if flags & !(AT_SYMLINK_FOLLOW | AT_EMPTY_PATH | AT_HANDLE_FID) != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    let node = lookup_at_or_fd(
        cx,
        &cx.cred(),
        dirfd,
        &path,
        flags,
        flags & AT_SYMLINK_FOLLOW != 0,
    )?;
    let Node::Store(mnt, ino) = node else {
        return Err(Errno::EOPNOTSUPP);
    };
    let header: [u8; 8] = get(cx, handle)?;
    let room = u32::from_le_bytes(header[..4].try_into().unwrap());
    if room > MAX_HANDLE_SZ {
        return Err(Errno::EINVAL);
    }
    // When it does not fit, only the handle header (the needed length, FILEID_INVALID) is
    // written back and the result is EOVERFLOW. Both cases write mount_id, and failing to write
    // it or the handle is EFAULT (Linux's do_sys_name_to_handle)
    let mut b = FILE_HANDLE_BYTES.to_le_bytes().to_vec();
    let ret = if room < FILE_HANDLE_BYTES {
        b.extend_from_slice(&FILEID_INVALID.to_le_bytes());
        Err(Errno::EOVERFLOW)
    } else {
        b.extend_from_slice(&FILE_HANDLE_TYPE.to_le_bytes());
        b.extend_from_slice(&ino.to_le_bytes());
        Ok(0)
    };
    put(cx, mount_id, (mnt as i32).to_le_bytes())?;
    cx.mm().write(UserSlice::new(handle, b.len()), &b)?;
    ret
}

/// open_by_handle_at: opens by inode number; requires `CAP_DAC_READ_SEARCH`.
pub(super) fn open_by_handle_at(
    cx: &mut Cx<'_>,
    mount_fd: i32,
    handle: u64,
    flags: u32,
) -> SysResult {
    if !cx.cred().capable(cap::DAC_READ_SEARCH) {
        return Err(Errno::EPERM);
    }
    // As in Linux's handle_to_path, read the handle header and check its length first, then the
    // whole handle, and look at mount_fd only last
    let header: [u8; 8] = get(cx, handle)?;
    let bytes = u32::from_le_bytes(header[..4].try_into().unwrap());
    if bytes == 0 || bytes > MAX_HANDLE_SZ {
        return Err(Errno::EINVAL);
    }
    let ty = i32::from_le_bytes(header[4..].try_into().unwrap());
    let mut f_handle = vec![0u8; bytes as usize];
    cx.mm()
        .read(UserSlice::new(handle + 8, f_handle.len()), &mut f_handle)?;
    // The file is opened through the mount mount_fd is in. Handles decode only in the store: when
    // mount_fd is on a pipe, a socket or a synthetic filesystem it is ESTALE, as on Linux where
    // these filesystems have no export_operations
    let node = if mount_fd == AT_FDCWD {
        Some(cx.cwd())
    } else {
        cx.fds().get(mount_fd)?.borrow().node()
    };
    let Some(node @ Node::Store(..)) = node else {
        return Err(Errno::ESTALE);
    };
    let mnt = node.mnt();
    let ino = match f_handle.first_chunk::<8>() {
        Some(b) if bytes == FILE_HANDLE_BYTES && ty == FILE_HANDLE_TYPE => u64::from_le_bytes(*b),
        _ => return Err(Errno::ESTALE),
    };
    let vfs = cx.vfs()?;
    // The file has been deleted (or the handle was not issued here): ESTALE
    let st = match vfs.store().stat(ino) {
        Ok(st) if st.nlink > 0 => st,
        _ => return Err(Errno::ESTALE),
    };
    let kind = if flags & oflags::O_PATH != 0 {
        if flags & oflags::O_DIRECTORY != 0 && !st.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        FileKind::Path(Node::Store(mnt, ino))
    } else {
        open_store(
            cx,
            &cx.cred(),
            &vfs,
            (mnt, ino),
            flags,
            StoreOpen::Found(None),
        )?
    };
    let file = OpenFile::new(kind, flags & kept(flags) | largefile(flags));
    file.borrow_mut().mnt = mnt;
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

/// A directory opened for writing, creating or truncating is EISDIR before O_DIRECT is looked at.
fn direct_open_is_eisdir(st: &Stat, flags: u32) -> bool {
    st.kind() == mode::S_IFDIR
        && (flags & oflags::O_ACCMODE != oflags::O_RDONLY
            || flags & (oflags::O_CREAT | oflags::O_TRUNC) != 0)
}

/// How [`open_store`] came to the inode it opens.
enum StoreOpen<'a> {
    /// Found by path, beside directory node `beside` when known (a ptmx node opens the devpts
    /// there): every check is made.
    Found(Option<&'a Node>),
    /// Created by this open, through the given host handle: no checks.
    Created(std::fs::File, Option<&'a Node>),
    /// A file without a name reopened through `/proc/<pid>/fd` or `exe`, through the given host
    /// handle (the store's own may be gone with its last name); a memfd (the bool) is on no mount
    /// of the store and never read-only. Every check is made, as Linux reopens the file's dentry.
    Reopened(std::fs::File, bool),
}

impl<'a> StoreOpen<'a> {
    fn by_path(created: Option<std::fs::File>, beside: Option<&'a Node>) -> Self {
        match created {
            Some(f) => StoreOpen::Created(f, beside),
            None => StoreOpen::Found(beside),
        }
    }
}

/// Opens a store inode reached through mount `mnt`.
fn open_store(
    cx: &mut Cx<'_>,
    cred: &Cred,
    vfs: &Vfs,
    (mnt, ino): (MountId, Ino),
    flags: u32,
    how: StoreOpen<'_>,
) -> Result<FileKind, Errno> {
    let (created, beside, check, memfd) = match how {
        StoreOpen::Found(beside) => (None, beside, true, false),
        StoreOpen::Created(f, beside) => (Some(f), beside, false, false),
        StoreOpen::Reopened(f, memfd) => (Some(f), None, true, memfd),
    };
    let store = vfs.store();
    let accmode = flags & oflags::O_ACCMODE;
    // Only the type, permissions, owner and device number are used; no need to stat the object on
    // the host
    let st = store.attrs(ino)?;
    if check {
        let mnt_flags = vfs.mount_flags(&Node::Store(mnt, ino)).unwrap_or(0);
        // Read-only means the mount's flag or its instance's (mnt_want_write checks both), so a
        // bind keeping rw flags over an instance remounted read-only cannot write either
        let readonly = !memfd && vfs.readonly(&Node::Store(mnt, ino));
        let mut mask = open_mask(flags);
        match st.kind() {
            mode::S_IFLNK => return Err(Errno::ELOOP),
            // O_TRUNC asks for write access even with O_RDONLY (Linux's build_open_flags)
            mode::S_IFDIR
                if accmode != oflags::O_RDONLY
                    || flags & (oflags::O_CREAT | oflags::O_TRUNC) != 0 =>
            {
                return Err(Errno::EISDIR);
            }
            // Truncation: EROFS comes first in a read-only mount (Linux's do_open, before
            // may_open)
            mode::S_IFREG if flags & oflags::O_TRUNC != 0 => {
                if readonly {
                    return Err(Errno::EROFS);
                }
                mask |= MAY_WRITE;
            }
            // Device nodes in a nodev mount cannot be opened (may_open_dev)
            mode::S_IFCHR | mode::S_IFBLK if mnt_flags & ms::NODEV != 0 => {
                return Err(Errno::EACCES);
            }
            _ => {}
        }
        cred.permission(&st, mask)?;
        // Device nodes then go through the cgroup device filter (Linux's
        // devcgroup_inode_permission)
        if matches!(st.kind(), mode::S_IFCHR | mode::S_IFBLK) && st.rdev != 0 {
            super::bpf::check_device(cx, st.kind(), st.rdev, device_access(mask))?;
        }
        if flags & oflags::O_NOATIME != 0 && !cred.owns(&st) {
            return Err(Errno::EPERM);
        }
        // Opening a regular file for write in a read-only mount is EROFS; devices and FIFOs as
        // usual (do_dentry_open leaves special_file alone)
        if readonly && st.kind() == mode::S_IFREG && accmode != oflags::O_RDONLY {
            return Err(Errno::EROFS);
        }
    }
    // O_DIRECT needs a file whose pages support direct I/O (do_dentry_open's direct_IO check):
    // regular files of the store (ext4's) and loop devices; directories, tmpfs files (shmem has
    // none in 6.1), character devices and FIFOs are EINVAL. Linux checks after the file's own open
    // (a FIFO waits for its peer first); here before it
    let direct_io = || match st.kind() {
        mode::S_IFREG => vfs.mount(mnt).is_none_or(|m| m.fs != Fs::Tmpfs),
        mode::S_IFBLK | mode::S_IFSOCK => true,
        _ => false,
    };
    if flags & oflags::O_DIRECT != 0 && !direct_io() && !direct_open_is_eisdir(&st, flags) {
        return Err(Errno::EINVAL);
    }
    Ok(match st.kind() {
        mode::S_IFLNK => return Err(Errno::ELOOP),
        mode::S_IFDIR => {
            if accmode != oflags::O_RDONLY || flags & (oflags::O_CREAT | oflags::O_TRUNC) != 0 {
                return Err(Errno::EISDIR);
            }
            FileKind::Directory {
                node: Node::Store(mnt, ino),
                entries: None,
                process: None,
                fuse: None,
            }
        }
        _ if flags & oflags::O_DIRECTORY != 0 => return Err(Errno::ENOTDIR),
        mode::S_IFREG => {
            let write = accmode != oflags::O_RDONLY || flags & oflags::O_TRUNC != 0;
            if write && check && executing(cx, ino) {
                return Err(Errno::ETXTBSY);
            }
            // Leases of others: opening for write conflicts with every lease, opening for read
            // only with write leases
            if check {
                let nonblock = flags & oflags::O_NONBLOCK != 0;
                super::lease::break_leases(cx, ino, write, nonblock, None)?;
            }
            // A held file shares its object: truncating it first gives it an empty one of its
            // own; otherwise a description opened for writing gets a read-only handle on the
            // shared object, and its first change gives the file its own copy (prepare_write)
            let mut created = created;
            if flags & oflags::O_TRUNC != 0 && store.held(ino)? {
                make_private(cx, ino, Some(0))?;
                if created.is_some() {
                    created = Some(store.open_object(ino, accmode != oflags::O_RDONLY)?);
                }
            }
            let cow = accmode != oflags::O_RDONLY && store.held(ino)?;
            let file = match created {
                Some(f) => f,
                None => store.open_object(ino, accmode != oflags::O_RDONLY && !cow)?,
            };
            // Even with O_RDONLY, as on Linux (do_open truncates whatever the access mode),
            // through a writable handle of its own then
            if flags & oflags::O_TRUNC != 0 {
                remove_privs(cx, ino)?;
                let writable = match accmode {
                    oflags::O_RDONLY => Some(store.open_object(ino, true)?),
                    _ => None,
                };
                let target = writable.as_ref().unwrap_or(&file);
                let tmpfs = tmpfs_size(vfs, &Node::Store(mnt, ino), target)?;
                truncating(cx, target, 0);
                target.set_len(0).map_err(host_errno)?;
                tmpfs_resized(vfs, tmpfs, target);
            }
            FileKind::Regular { ino, file, cow }
        }
        mode::S_IFCHR => match st.rdev {
            // /dev/tty, /dev/console: the caller's controlling terminal
            r if r == rdev(5, 0) || r == rdev(5, 1) => tty::open_ctty(cx)?,
            PTMX_RDEV => {
                let (fs, m) = devpts_beside(vfs, beside)?;
                tty::open_ptmx(cx.k(), (fs, m, false), (cred.fsuid, cred.fsgid))?
            }
            crate::vfs::fuse::DEV_RDEV => {
                FileKind::FuseDev(crate::vfs::fuse::DevFile::new(cred, ino))
            }
            // Terminal device nodes outside devpts: those in the boot instance
            r => match pts_index(r) {
                Some(index) => {
                    let at = (crate::tty::BOOT_PTS, index, crate::vfs::PTS_MOUNT, false);
                    tty::open_slave(cx, at, flags)?
                }
                None => {
                    // A character device without a driver (Linux's `chrdev_open`)
                    let dev = dev::open(store, ino).inspect_err(|&e| {
                        if e == Errno::ENXIO {
                            let (major, minor) = dev::split_rdev(r);
                            crate::stub::hit(cx, crate::stub::Stub::Device(false, major, minor));
                        }
                    })?;
                    let kmsg = match dev {
                        Device::Kmsg => Some(open_kmsg(cx, accmode)?),
                        _ => None,
                    };
                    FileKind::Device {
                        ino,
                        dev,
                        kmsg,
                        block: None,
                    }
                }
            },
        },
        mode::S_IFBLK => super::loopdev::open(cx, ino, st.rdev, flags)?,
        mode::S_IFIFO => open_fifo(cx, ino, flags)?,
        // Socket files cannot be opened (they use connect)
        _ => return Err(Errno::ENXIO),
    })
}

/// Opens overlay node `o` reached through mount `mnt` (Linux's `ovl_open`): the store inode
/// behind it is opened as a store file would be, and a directory is listed through the overlay.
/// A regular file opened for writing is copied up first (without its data for `O_TRUNC`), after
/// the checks open makes on the overlay's file: a read-only filesystem (EROFS, before permission
/// as 6.1's `sb_permission`), permission, and a program being executed (ETXTBSY). A file this
/// open just created (`created`) is already in the upper layer.
fn open_overlay(
    cx: &mut Cx<'_>,
    cred: &Cred,
    vfs: &Vfs,
    (mnt, o): (MountId, crate::vfs::overlay::OvlId),
    flags: u32,
    (created, beside): (Option<std::fs::File>, Option<&Node>),
) -> Result<FileKind, Errno> {
    let mut real = vfs.overlay().node(o).real();
    let st = vfs.store().attrs(real)?;
    let write = flags & oflags::O_ACCMODE != oflags::O_RDONLY || flags & oflags::O_TRUNC != 0;
    if write && created.is_none() && st.kind() == mode::S_IFREG {
        if vfs.readonly(&Node::Overlay(mnt, o)) {
            return Err(Errno::EROFS);
        }
        let truncate = flags & oflags::O_TRUNC != 0;
        let mask = open_mask(flags) | if truncate { MAY_WRITE } else { 0 };
        cred.permission(&st, mask)?;
        if executing(cx, real) {
            return Err(Errno::ETXTBSY);
        }
        let what = if truncate {
            CopyUp::NoData
        } else {
            CopyUp::All
        };
        real = copy_up(cx, vfs, o, what)?;
    }
    Ok(
        match open_store(
            cx,
            cred,
            vfs,
            (mnt, real),
            flags,
            StoreOpen::by_path(created, beside),
        )? {
            FileKind::Directory { process, .. } => FileKind::Directory {
                node: Node::Overlay(mnt, o),
                entries: None,
                process,
                fuse: None,
            },
            kind => kind,
        },
    )
}

/// The access permissions open requires (`MAY_*`).
pub(super) fn open_mask(flags: u32) -> u32 {
    match flags & oflags::O_ACCMODE {
        oflags::O_RDONLY => MAY_READ,
        oflags::O_WRONLY => MAY_WRITE,
        _ => MAY_READ | MAY_WRITE,
    }
}

/// Turns access permissions (`MAY_*`) into device-filter access (`BPF_DEVCG_ACC_*`).
fn device_access(mask: u32) -> u32 {
    let mut access = 0;
    if mask & MAY_WRITE != 0 {
        access |= crate::bpf::ACC_WRITE;
    }
    if mask & MAY_READ != 0 {
        access |= crate::bpf::ACC_READ;
    }
    access
}

/// Which devpts instance a ptmx device node outside devpts opens (Linux's `devpts_acquire`): the
/// one mounted on `pts` in the node's directory `dir` (its mount and device number). No `pts` is
/// ENOENT, and a `pts` that is not a devpts root is ENODEV; when the node's directory is unknown
/// (opened by handle) it is the boot one.
fn devpts_beside(vfs: &Vfs, dir: Option<&Node>) -> Result<(u64, MountId), Errno> {
    let Some(dir) = dir else {
        return Ok((crate::tty::BOOT_PTS, crate::vfs::PTS_MOUNT));
    };
    let Node::Store(mnt, ino) = dir else {
        return Err(Errno::ENODEV);
    };
    let pts = vfs.store().lookup(*ino, b"pts")?.ok_or(Errno::ENOENT)?;
    match vfs.mounted(Node::Store(*mnt, pts)) {
        Node::Pseudo(m, PseudoNode::PtsRoot(fs)) => Ok((fs, m)),
        _ => Err(Errno::ENODEV),
    }
}

/// The PTY slave index of a device number: major 136, and the minor is the index (Linux's pts
/// driver registers all 2^20 minors of 136).
fn pts_index(r: u64) -> Option<u32> {
    let major = ((r >> 8) & 0xfff) as u32;
    let minor = ((r & 0xff) | ((r >> 12) & !0xff)) as u32;
    (major == PTS_MAJOR).then_some(minor)
}

/// Opens `/dev/kmsg` (`devkmsg_open`): when `printk_devkmsg` is `off` nobody can open it; a read
/// open checks permission as syslog's read-all does (`dmesg_restrict`), and a write-only open is
/// not checked. Reading starts at the oldest record.
fn open_kmsg(cx: &mut Cx<'_>, accmode: u32) -> Result<Box<crate::klog::KmsgFile>, Errno> {
    let log = cx.kr().klog.clone();
    if log.borrow().devkmsg == crate::klog::Devkmsg::Off {
        return Err(Errno::EPERM);
    }
    if accmode != oflags::O_WRONLY {
        super::misc::check_syslog_permissions(cx, super::misc::syslog_action::READ_ALL, false)?;
    }
    Ok(Box::new(crate::klog::KmsgFile::new(log)))
}

/// Opens a named pipe (FIFO): all opens of one inode share one pipe. As on Linux, a read-only
/// open waits until there is a writer, a write-only open waits until there is a reader (with
/// `O_NONBLOCK` it returns ENXIO when there is no reader), and a read-write open does not wait.
fn open_fifo(cx: &mut Cx<'_>, ino: Ino, flags: u32) -> Result<FileKind, Errno> {
    let accmode = flags & oflags::O_ACCMODE;
    let nonblock = flags & oflags::O_NONBLOCK != 0;
    let (read, write) = match accmode {
        oflags::O_RDONLY => (true, false),
        oflags::O_WRONLY => (false, true),
        oflags::O_RDWR => (true, true),
        // Access mode 3 opens neither end (Linux's fifo_open)
        _ => return Err(Errno::EINVAL),
    };
    let capacity = pipe_capacity(cx);
    let pipe = {
        let k = cx.k();
        k.fifos.retain(|_, w| w.strong_count() > 0);
        match k.fifos.get(&ino).and_then(Weak::upgrade) {
            Some(p) => p,
            None => {
                let p = Pipe::fifo(ino, capacity);
                k.fifos.insert(ino, Rc::downgrade(&p));
                p
            }
        }
    };
    if write && !read && nonblock && pipe.borrow().readers == 0 {
        return Err(Errno::ENXIO);
    }
    let peer_opens = |p: &Pipe| if read { p.writer_opens } else { p.reader_opens };
    let before = peer_opens(&pipe.borrow());
    let mut end = PipeEnd::open(&pipe, read, write);
    if read && !write && nonblock && pipe.borrow().writers == 0 {
        end.hup_after = Some(before);
    }
    if read != write && !nonblock {
        // Wait for the other end to open: it is enough that it came, even if it has closed
        // already
        loop {
            {
                let p = pipe.borrow();
                let peers = if read { p.writers } else { p.readers };
                if peers > 0 || peer_opens(&p) != before {
                    break;
                }
            }
            if cx.interrupted() {
                return Err(Errno::ERESTARTSYS);
            }
            cx.wait_object(crate::pipe::object(&pipe))?;
        }
    }
    Ok(FileKind::Pipe(end))
}

/// Files opened by open, openat2 and open_by_handle_at carry `O_LARGEFILE` (on 64-bit Linux
/// always adds it, and drops it with `O_PATH`), visible in F_GETFL and fdinfo; files created by
/// other syscalls, such as pipes and sockets, do not have it.
fn largefile(flags: u32) -> u32 {
    if flags & oflags::O_PATH != 0 {
        0
    } else {
        oflags::O_LARGEFILE
    }
}

pub(super) fn close(cx: &mut Cx<'_>, fd: i32) -> SysResult {
    // Take it out of the table first, then release it: closing the last end of a pipe wakes the
    // other end
    let r = cx.fds_mut().close(fd);
    let file = r?;
    let fds = cx.t.fds.clone();
    locks::closed(cx.k(), &fds, std::slice::from_ref(&file));
    super::mqueue_closed(cx.tgid(), std::slice::from_ref(&file));
    // A FUSE file is flushed, and the error is close's (6.1's filp_close)
    let flushed = super::fuse::flush(cx, &file);
    crate::unix::released(cx.k(), vec![file]);
    flushed.map(|()| 0)
}

pub(super) fn close_range(cx: &mut Cx<'_>, first: u32, last: u32, flags: u32) -> SysResult {
    if flags & !(CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC) != 0 || first > last {
        return Err(Errno::EINVAL);
    }
    // Only when the table is shared with another thread or process (each thread holds two
    // references): copying an unshared one would make a new lock owner and drop the POSIX record
    // locks held through it
    if flags & CLOSE_RANGE_UNSHARE != 0 && Rc::strong_count(&cx.t.fds) > 2 {
        let copy = cx.fds().clone();
        cx.set_fds(Rc::new(RefCell::new(copy)));
    }
    let closed = cx.fds_mut().close_range(
        first as usize,
        last as usize,
        flags & CLOSE_RANGE_CLOEXEC != 0,
    );
    let fds = cx.t.fds.clone();
    locks::closed(cx.k(), &fds, &closed);
    super::mqueue_closed(cx.tgid(), &closed);
    super::fuse::flush_all(cx, &closed);
    crate::unix::released(cx.k(), closed);
    Ok(0)
}

pub(super) fn dup(cx: &mut Cx<'_>, fd: i32) -> SysResult {
    let file = cx.fds().get(fd)?;
    cx.fds_mut().install(file, false, 0).map(|fd| fd as u64)
}

pub(super) fn dup3(cx: &mut Cx<'_>, old: i32, new: i32, flags: u32) -> SysResult {
    if flags & !oflags::O_CLOEXEC != 0 || old == new {
        return Err(Errno::EINVAL);
    }
    let file = cx.fds().get(old)?;
    let replaced = cx.fds_mut().install_at(new, file, flags != 0);
    let fds = cx.t.fds.clone();
    let replaced: Vec<_> = replaced?.into_iter().collect();
    locks::closed(cx.k(), &fds, &replaced);
    super::mqueue_closed(cx.tgid(), &replaced);
    super::fuse::flush_all(cx, &replaced);
    crate::unix::released(cx.k(), replaced);
    Ok(new as u64)
}

/// Files `replaced` in fd table `fds` of thread group `tgid`, another process's (seccomp's
/// `SECCOMP_ADDFD_FLAG_SETFD`), are closed as dup3 closes the file it replaces: locks, message
/// queue notifications and FUSE flushes are that process's.
pub(crate) fn close_replaced(
    cx: &mut Cx<'_>,
    fds: &Rc<RefCell<crate::fd::FdTable>>,
    tgid: Tid,
    replaced: Vec<FileRef>,
) {
    locks::closed(cx.k(), fds, &replaced);
    super::mqueue_closed(tgid, &replaced);
    for f in &replaced {
        let _ = super::fuse::flush_as(cx, f, Rc::as_ptr(fds) as usize);
    }
    crate::unix::released(cx.k(), replaced);
}

pub(super) fn pipe2(cx: &mut Cx<'_>, fds: u64, flags: u32) -> SysResult {
    const O_DIRECT: u32 = 0o200000;
    if flags & !(oflags::O_CLOEXEC | oflags::O_NONBLOCK | O_DIRECT) != 0 {
        return Err(Errno::EINVAL);
    }
    let cloexec = flags & oflags::O_CLOEXEC != 0;
    let status = flags & oflags::O_NONBLOCK;
    let (r, w) = PipeEnd::pair(pipe_capacity(cx));
    let r = OpenFile::new(FileKind::Pipe(r), oflags::O_RDONLY | status);
    let w = OpenFile::new(FileKind::Pipe(w), oflags::O_WRONLY | status);
    let rfd = cx.fds_mut().install(r, cloexec, 0)?;
    // End the borrow of the fd table first: on failure it must be borrowed again to close the
    // read end
    let installed = cx.fds_mut().install(w, cloexec, 0);
    let wfd = match installed {
        Ok(fd) => fd,
        Err(e) => {
            let _ = cx.fds_mut().close(rfd);
            return Err(e);
        }
    };
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&rfd.to_le_bytes());
    out[4..].copy_from_slice(&wfd.to_le_bytes());
    if let Err(e) = put(cx, fds, out) {
        let _ = cx.fds_mut().close(rfd);
        let _ = cx.fds_mut().close(wfd);
        return Err(e);
    }
    Ok(0)
}

/// Capacity of a new pipe: as on Linux, at most pipe-max-size, except with `CAP_SYS_RESOURCE`.
fn pipe_capacity(cx: &Cx<'_>) -> usize {
    if cx.cred().capable(cap::SYS_RESOURCE) {
        PIPE_CAPACITY
    } else {
        PIPE_CAPACITY.min(cx.kr().pipe_max_size)
    }
}

pub(super) fn memfd_create(cx: &mut Cx<'_>, name: u64, flags: u32) -> SysResult {
    const MFD_CLOEXEC: u32 = 0x1;
    const MFD_ALLOW_SEALING: u32 = 0x2;
    const MFD_NOEXEC_SEAL: u32 = 0x8;
    const MFD_EXEC: u32 = 0x10;
    // MFD_HUGETLB is not supported
    if flags & !(MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_NOEXEC_SEAL | MFD_EXEC) != 0
        || flags & (MFD_NOEXEC_SEAL | MFD_EXEC) == MFD_NOEXEC_SEAL | MFD_EXEC
    {
        return Err(Errno::EINVAL);
    }
    let name = cx.mm().read_cstr(name, 249).map_err(|e| {
        if e == Errno::ENAMETOOLONG {
            Errno::EINVAL
        } else {
            e
        }
    })?;
    let noexec = flags & MFD_NOEXEC_SEAL != 0;
    // As shmem_get_inode with no directory: owned by the creator's filesystem ids
    let cred = cx.cred();
    let (ino, file) = cx.vfs()?.store().create_anonymous(
        if noexec { 0o666 } else { 0o777 },
        cred.fsuid,
        cred.fsgid,
    )?;
    let seals = match (flags & MFD_ALLOW_SEALING != 0, noexec) {
        (_, true) => F_SEAL_EXEC,
        (true, false) => 0,
        (false, false) => F_SEAL_SEAL,
    };
    cx.k().seals.insert(ino, seals);
    let mut path = b"/memfd:".to_vec();
    path.extend_from_slice(&name);
    path.extend_from_slice(b" (deleted)");
    let file = OpenFile::with_path(
        FileKind::Regular {
            ino,
            file,
            cow: false,
        },
        oflags::O_RDWR | oflags::O_LARGEFILE,
        Some(Rc::from(path)),
    );
    file.borrow_mut().pseudo = true;
    cx.fds_mut()
        .install(file, flags & MFD_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

pub(super) fn fcntl(cx: &mut Cx<'_>, fd: i32, cmd: u32, arg: u64) -> SysResult {
    let file = cx.fds().get(fd)?;
    // An O_PATH descriptor allows only dup, the descriptor flags and F_GETFL (check_fcntl_cmd)
    if file.borrow().flags & oflags::O_PATH != 0
        && !matches!(cmd, F_DUPFD | F_DUPFD_CLOEXEC | F_GETFD | F_SETFD | F_GETFL)
    {
        return Err(Errno::EBADF);
    }
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            // A start not below RLIMIT_NOFILE is EINVAL; only having no free slot is EMFILE
            let min = usize::try_from(arg as i32)
                .ok()
                .filter(|&m| m < cx.fds().limit)
                .ok_or(Errno::EINVAL)?;
            cx.fds_mut()
                .install(file, cmd == F_DUPFD_CLOEXEC, min)
                .map(|fd| fd as u64)
        }
        F_GETFD => Ok(cx.fds().cloexec(fd)? as u64),
        F_SETFD => cx
            .fds_mut()
            .set_cloexec(fd, arg & FD_CLOEXEC != 0)
            .map(|()| 0),
        F_GETFL => Ok(file.borrow().flags as u64),
        F_SETFL => {
            let async_on = {
                let mut f = file.borrow_mut();
                let was = f.flags & oflags::O_ASYNC != 0;
                f.flags = (f.flags & !oflags::SETFL_MASK) | (arg as u32 & oflags::SETFL_MASK);
                !was && f.flags & oflags::O_ASYNC != 0
            };
            if async_on {
                super::poll::register_fasync(cx, &file, fd);
            }
            Ok(0)
        }
        F_GETLK | F_SETLK | F_SETLKW | F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW => {
            record_lock(cx, &file, cmd, arg)
        }
        // The dispatcher translates F_SETOWN's argument and F_GETOWN's result (pid_args)
        F_SETOWN => set_owner(cx, &file, arg as i32).map(|()| 0),
        F_GETOWN => Ok(get_owner(&file) as i64 as u64),
        F_SETOWN_EX => {
            let b: [u8; 8] = get(cx, arg)?;
            let ty = i32::from_le_bytes(b[..4].try_into().unwrap());
            let pid = i32::from_le_bytes(b[4..].try_into().unwrap());
            if !matches!(ty, F_OWNER_TID | F_OWNER_PID | F_OWNER_PGRP) {
                return Err(Errno::EINVAL);
            }
            record_owner(cx, &file, (ty, cx.pid_in(pid))).map(|()| 0)
        }
        F_GETOWN_EX => {
            let (ty, pid) = file.borrow().owner;
            let pid = cx.pid_out(pid);
            let mut b = [0u8; 8];
            b[..4].copy_from_slice(&ty.to_le_bytes());
            b[4..].copy_from_slice(&pid.to_le_bytes());
            put(cx, arg, b).map(|()| 0)
        }
        F_SETSIG => {
            if arg != 0 && Signal::new(arg as i32).is_none() {
                return Err(Errno::EINVAL);
            }
            file.borrow_mut().sig = arg as i32;
            Ok(0)
        }
        F_GETSIG => Ok(file.borrow().sig as u64),
        F_SETLEASE => super::lease::set(cx, &file, fd, arg as i16).map(|()| 0),
        F_GETLEASE => Ok(super::lease::get(cx, &file) as u64),
        F_GETPIPE_SZ | F_SETPIPE_SZ => {
            let f = file.borrow();
            let FileKind::Pipe(end) = &f.kind else {
                return Err(Errno::EBADF);
            };
            let mut p = end.pipe.borrow_mut();
            if cmd == F_GETPIPE_SZ {
                return Ok(p.capacity as u64);
            }
            // Round up by pages to a power of two; exceeding pipe-max-size requires
            // CAP_SYS_RESOURCE, and it cannot be smaller than the data already buffered
            let size = (arg as u32 as usize)
                .max(PIPE_BUF)
                .checked_next_power_of_two()
                .filter(|&n| n <= 1 << 31)
                .ok_or(Errno::EINVAL)?;
            if size > cx.kr().pipe_max_size && !cx.cred().capable(cap::SYS_RESOURCE) {
                return Err(Errno::EPERM);
            }
            if size < p.buf.len() {
                return Err(Errno::EBUSY);
            }
            p.capacity = size;
            request_wake_object(crate::pipe::object(&end.pipe));
            Ok(size as u64)
        }
        // Only memfds can have seals added
        F_ADD_SEALS | F_GET_SEALS => {
            let f = file.borrow();
            let FileKind::Regular { ino, .. } = f.kind else {
                return Err(Errno::EINVAL);
            };
            let seals = cx.k().seals.get_mut(&ino).ok_or(Errno::EINVAL)?;
            if cmd == F_GET_SEALS {
                return Ok(*seals as u64);
            }
            let mut add = arg as u32;
            if add & !F_SEAL_ALL != 0 {
                return Err(Errno::EINVAL);
            }
            if !f.writable() || *seals & F_SEAL_SEAL != 0 {
                return Err(Errno::EPERM);
            }
            // Sealing an executable memfd's execute bits also seals its contents, W^X from then
            // on (6.3's memfd_add_seals)
            if add & F_SEAL_EXEC != 0 && cx.vfs()?.store().attrs(ino)?.mode & 0o111 != 0 {
                add |= F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE;
            }
            // Writes cannot be sealed while writable shared mappings exist
            if add & F_SEAL_WRITE != 0 && writably_mapped(cx, &f) {
                return Err(Errno::EBUSY);
            }
            *cx.k().seals.get_mut(&ino).unwrap() |= add;
            Ok(0)
        }
        _ => {
            crate::stub::hit(cx, crate::stub::Stub::Fcntl(cmd));
            Err(Errno::EINVAL)
        }
    }
}

/// `F_SETOWN` and the socket ioctls `FIOSETOWN`/`SIOCSPGRP` (Linux's `f_setown`): `pid` is
/// already a global number, a negative one a process group; `INT_MIN`, which has no negation, is
/// `EINVAL`.
fn set_owner(cx: &Cx<'_>, file: &FileRef, pid: i32) -> Result<(), Errno> {
    let owner = match pid.checked_neg() {
        None => return Err(Errno::EINVAL),
        Some(g) if pid < 0 => (F_OWNER_PGRP, g),
        Some(_) => (F_OWNER_PID, pid),
    };
    record_owner(cx, file, owner)
}

/// Records a file's owner (type, global number) for `F_SETOWN*` (Linux's `__f_setown` with
/// force): a number no pid has is `ESRCH` (`find_vpid`; numbers the caller cannot see arrive as
/// [`super::NO_PID`]), 0 removes the owner, and the caller's real and effective user IDs are kept
/// with it for the permission check at delivery ([`super::poll::send_to_owner`]).
fn record_owner(cx: &Cx<'_>, file: &FileRef, (ty, pid): (i32, i32)) -> Result<(), Errno> {
    if pid != 0 {
        let k = cx.kr();
        let named = k.tasks.contains_key(&pid)
            || k.groups.contains_key(&pid)
            || k.groups.values().any(|g| {
                let g = g.borrow();
                g.pgid == pid || g.sid == pid
            });
        if !named {
            return Err(Errno::ESRCH);
        }
    }
    let cred = cx.cred();
    let mut f = file.borrow_mut();
    f.owner = (ty, pid);
    if pid != 0 {
        f.owner_ids = (cred.ruid, cred.euid);
    }
    Ok(())
}

/// `F_GETOWN`, `FIOGETOWN`, `SIOCGPGRP`: the owner as a global number, a process group negated.
fn get_owner(file: &FileRef) -> Tid {
    let (ty, pid) = file.borrow().owner;
    if ty == F_OWNER_PGRP { -pid } else { pid }
}

/// fcntl's record-lock commands ([`crate::locks`]): `arg` points at a `struct flock`.
fn record_lock(cx: &mut Cx<'_>, file: &FileRef, cmd: u32, arg: u64) -> SysResult {
    let mut b: [u8; 32] = get(cx, arg)?;
    let half = |off: usize| i16::from_le_bytes(b[off..off + 2].try_into().unwrap());
    let long = |off: usize| i64::from_le_bytes(b[off..off + 8].try_into().unwrap());
    let (l_type, whence, l_start, l_len) = (half(0), half(2), long(8), long(16));
    let l_pid = i32::from_le_bytes(b[24..28].try_into().unwrap());
    let ofd = matches!(cmd, F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW);
    let getlk = matches!(cmd, F_GETLK | F_OFD_GETLK);
    if ofd && l_pid != 0 {
        return Err(Errno::EINVAL);
    }
    let write = match l_type {
        F_RDLCK => false,
        F_WRLCK => true,
        F_UNLCK if !getlk => false,
        _ => return Err(Errno::EINVAL),
    };
    let unlock = l_type == F_UNLCK;
    if !matches!(whence as u32, SEEK_SET | SEEK_CUR | SEEK_END) {
        return Err(Errno::EINVAL);
    }
    let (ino, start, end) = {
        let f = file.borrow();
        if !getlk && !unlock && !(if write { f.writable() } else { f.readable() }) {
            return Err(Errno::EBADF);
        }
        let Some(ino) = f.lock_ino() else {
            // Files not in the store (pipes, sockets): other processes cannot see their locks
            if getlk {
                b[..2].copy_from_slice(&F_UNLCK.to_le_bytes());
                put(cx, arg, b)?;
            }
            return Ok(0);
        };
        let base = match whence as u32 {
            SEEK_SET => 0,
            SEEK_CUR => f.pos,
            SEEK_END => match &f.kind {
                FileKind::Regular { file, .. } => file.metadata().map_err(host_errno)?.len(),
                _ => 0,
            },
            _ => return Err(Errno::EINVAL),
        };
        // Linux's flock64_to_posix_lock, in its order: the start may not pass OFFSET_MAX
        // (EOVERFLOW) or be negative (EINVAL); an l_len of 0 means to the end of the file and
        // beyond, a positive one may end at OFFSET_MAX at most (EOVERFLOW), a negative one is
        // [start + l_len, start) and may not begin below 0 (EINVAL)
        let base = i64::try_from(base).map_err(|_| Errno::EOVERFLOW)?;
        if l_start > i64::MAX - base {
            return Err(Errno::EOVERFLOW);
        }
        let start = base + l_start;
        if start < 0 {
            return Err(Errno::EINVAL);
        }
        // The last byte, inclusive
        let (start, last) = match l_len {
            0 => (start, i64::MAX),
            n if n > 0 => {
                if n - 1 > i64::MAX - start {
                    return Err(Errno::EOVERFLOW);
                }
                (start, start + (n - 1))
            }
            n => {
                if start + n < 0 {
                    return Err(Errno::EINVAL);
                }
                (start + n, start - 1)
            }
        };
        let end = if last == i64::MAX {
            u64::MAX
        } else {
            last as u64 + 1
        };
        (ino, start as u64, end)
    };
    let owner = if ofd {
        file.borrow_mut().holds_locks = true;
        Owner::Ofd(Rc::downgrade(file))
    } else {
        cx.fds_mut().holds_locks = true;
        Owner::Posix(Rc::downgrade(&cx.t.fds))
    };
    let req = Lock {
        owner,
        pid: if ofd { -1 } else { cx.tgid() },
        start,
        end,
        write,
    };
    if getlk {
        // With no conflict only l_type is changed to F_UNLCK; the other fields stay
        match cx.k().locks.conflict(ino, &req) {
            None => b[..2].copy_from_slice(&F_UNLCK.to_le_bytes()),
            Some(l) => {
                let ty = if l.write { F_WRLCK } else { F_RDLCK };
                let len = if l.end == u64::MAX {
                    0
                } else {
                    l.end - l.start
                };
                b[..2].copy_from_slice(&ty.to_le_bytes());
                b[2..4].copy_from_slice(&(SEEK_SET as i16).to_le_bytes());
                b[8..16].copy_from_slice(&l.start.to_le_bytes());
                b[16..24].copy_from_slice(&len.to_le_bytes());
                // -1 for OFD locks, otherwise the number the caller sees (Linux's
                // locks_translate_pid)
                let pid = if l.pid > 0 { cx.pid_out(l.pid) } else { l.pid };
                b[24..28].copy_from_slice(&pid.to_le_bytes());
            }
        }
        return put(cx, arg, b).map(|()| 0);
    }
    loop {
        let locks = &mut cx.k().locks;
        if unlock || locks.conflict(ino, &req).is_none() {
            locks.set_waiting(&req.owner, None);
            locks.set(ino, req, unlock);
            return Ok(0);
        }
        if matches!(cmd, F_SETLK | F_OFD_SETLK) {
            return Err(Errno::EAGAIN);
        }
        if locks.deadlock(ino, &req) {
            locks.set_waiting(&req.owner, None);
            return Err(Errno::EDEADLK);
        }
        locks.set_waiting(
            &req.owner,
            Some(Waiter {
                ino,
                req: req.clone(),
            }),
        );
        if cx.interrupted() {
            cx.k().locks.set_waiting(&req.owner, None);
            return Err(Errno::ERESTARTSYS);
        }
        // The holder requests a wakeup when it unlocks, closes the file or exits
        cx.wait_on(None, &mut [], crate::kernel::wake::LOCK)?;
    }
}

/// Whether some address space maps this open file with a writable shared mapping.
fn writably_mapped(cx: &Cx<'_>, f: &OpenFile) -> bool {
    use std::os::unix::fs::MetadataExt;
    let FileKind::Regular { file, .. } = &f.kind else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    cx.kr().tasks.values().any(|t| {
        t.res
            .borrow()
            .mm
            .borrow()
            .writably_maps(meta.dev(), meta.ino())
    })
}

pub(super) fn flock(cx: &mut Cx<'_>, fd: i32, op: u32) -> SysResult {
    const LOCK_SH: u32 = 1;
    const LOCK_EX: u32 = 2;
    const LOCK_NB: u32 = 4;
    const LOCK_UN: u32 = 8;
    let file = cx.fds().get(fd)?;
    // O_PATH descriptors are not found (fdget)
    if file.borrow().flags & oflags::O_PATH != 0 {
        return Err(Errno::EBADF);
    }
    // Files not in the store (pipes, sockets) have no lock to share: always succeeds
    let Some(ino) = file.borrow().lock_ino() else {
        return Ok(0);
    };
    let me = Rc::downgrade(&file);
    let tgid = cx.tgid();
    let exclusive = match op & !LOCK_NB {
        LOCK_SH => false,
        LOCK_EX => true,
        LOCK_UN => {
            if let Some(locks) = cx.k().flocks.get_mut(&ino) {
                locks.retain(|(f, ..)| !f.ptr_eq(&me));
            }
            request_wake_locks();
            return Ok(0);
        }
        _ => return Err(Errno::EINVAL),
    };
    loop {
        let locks = cx.k().flocks.entry(ino).or_default();
        locks.retain(|(f, ..)| f.strong_count() > 0);
        // Locking the same open file again converts the lock type
        let conflict = locks
            .iter()
            .any(|(f, ex, _)| !f.ptr_eq(&me) && (exclusive || *ex));
        if !conflict {
            // When converting the lock type, release the old one first (as on Linux): when an
            // exclusive lock becomes shared, those waiting for a shared lock can take it
            let before = locks.len();
            locks.retain(|(f, ..)| !f.ptr_eq(&me));
            if locks.len() != before {
                request_wake_locks();
            }
            // The caller's tgid, as Linux's flock_make_lock records it (a conversion too)
            locks.push((me, exclusive, tgid));
            file.borrow_mut().holds_locks = true;
            return Ok(0);
        }
        if op & LOCK_NB != 0 {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        // The file holding the lock requests a wakeup when it unlocks, converts or closes
        // (OpenFile's destructor)
        cx.wait_on(None, &mut [], crate::kernel::wake::LOCK)?;
    }
}

pub(super) fn ioctl(cx: &mut Cx<'_>, fd: i32, request: u32, arg: u64) -> SysResult {
    let file = io_file(cx, fd)?;
    match request {
        FIOCLEX | FIONCLEX => {
            return cx.fds_mut().set_cloexec(fd, request == FIOCLEX).map(|()| 0);
        }
        FIONBIO => {
            let on = u32::from_le_bytes(get(cx, arg)?) != 0;
            let mut f = file.borrow_mut();
            f.flags = (f.flags & !oflags::O_NONBLOCK) | if on { oflags::O_NONBLOCK } else { 0 };
            return Ok(0);
        }
        FIOASYNC => {
            let on = u32::from_le_bytes(get(cx, arg)?) != 0;
            let was = {
                let mut f = file.borrow_mut();
                let was = f.flags & oflags::O_ASYNC != 0;
                f.flags = (f.flags & !oflags::O_ASYNC) | if on { oflags::O_ASYNC } else { 0 };
                was
            };
            if on && !was {
                super::poll::register_fasync(cx, &file, fd);
            }
            return Ok(0);
        }
        // Handled before the file type is looked at, as on Linux (do_vfs_ioctl)
        FICLONE | FICLONERANGE => {
            let (src, range) = if request == FICLONE {
                (arg as i32, (0, 0, 0))
            } else {
                // struct file_clone_range: the source fd (s64), then the source offset, the
                // length (0 is to the source's end) and the destination offset
                let b: [u8; 32] = get(cx, arg)?;
                let word = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
                (word(0) as i64 as i32, (word(1), word(2), word(3)))
            };
            let src = io_file(cx, src)?;
            return clone_range(cx, &src, &file, range);
        }
        FIDEDUPERANGE => {
            // Header of struct file_dedupe_range: two u64s, dest_count (u16) and two reserved
            // fields
            let b: [u8; 24] = get(cx, arg)?;
            if b[18..24].iter().any(|&x| x != 0) {
                return Err(Errno::EINVAL);
            }
            return match file.borrow().kind {
                FileKind::Regular { .. } => Err(Errno::EOPNOTSUPP),
                FileKind::Directory { .. } => Err(Errno::EISDIR),
                _ => Err(Errno::EINVAL),
            };
        }
        // Socket owner (as with F_SETOWN and F_GETOWN; negative is a process group)
        FIOSETOWN | SIOCSPGRP | FIOGETOWN | SIOCGPGRP
            if matches!(file.borrow().kind, FileKind::Socket(_)) =>
        {
            // The number is in memory, so translated here, as the dispatcher does for F_SETOWN
            if matches!(request, FIOSETOWN | SIOCSPGRP) {
                let pid = match i32::from_le_bytes(get(cx, arg)?) {
                    p if p > 0 => cx.pid_in(p),
                    p if p < 0 && p != i32::MIN => -cx.pid_in(-p),
                    p => p,
                };
                return set_owner(cx, &file, pid).map(|()| 0);
            }
            let pid = get_owner(&file);
            let local = cx.pid_out(pid.abs());
            let pid = if pid < 0 { -local } else { local };
            return put(cx, arg, pid.to_le_bytes()).map(|()| 0);
        }
        // Before the file's own ioctls, as in Linux's do_vfs_ioctl: every regular inode answers,
        // in the store and tmpfs, procfs, sysfs and nsfs, and message queues
        FIONREAD
            if matches!(
                file.borrow().kind,
                FileKind::Regular { .. } | FileKind::Proc { .. } | FileKind::MessageQueue(_)
            ) =>
        {
            if let Some(n) = unread(cx, &file)? {
                return put(cx, arg, n.to_le_bytes()).map(|()| 0);
            }
        }
        _ => {}
    }
    let ns = match &file.borrow().kind {
        FileKind::Proc {
            mnt,
            node: PseudoNode::NsFile(kind, _),
            ns: Some(r),
            ..
        } => Some((*mnt, *kind, r.clone())),
        _ => None,
    };
    if let Some((mnt, kind, r)) = ns {
        return super::ns::ioctl(cx, mnt, kind, &r, request, arg);
    }
    if matches!(file.borrow().kind, FileKind::FuseDev(_)) {
        return super::fuse::dev_ioctl(cx, &file, request, arg);
    }
    // Take out the object needed, then release the borrow of the open file: NOTIF_RECV waits,
    // and while it waits other threads may want to change this open file (fcntl, FIONBIO)
    let notify = match &file.borrow().kind {
        FileKind::SeccompNotify(l) => Some(l.filter()),
        _ => None,
    };
    if let Some(f) = notify {
        return crate::seccomp::ioctl(cx, &f, request, arg);
    }
    let pty = match &file.borrow().kind {
        FileKind::Pty(end) => Some((end.tty.clone(), (end.master, end.born), end.mnt)),
        _ => None,
    };
    if let Some((t, side, mnt)) = pty {
        if request == tty::TIOCGPTPEER {
            return tty::open_peer(cx, &t, side, mnt, arg as u32);
        }
        let readable = file.borrow().readable();
        return tty::ioctl(cx, &t, side, readable, request, arg);
    }
    let sock = match &file.borrow().kind {
        FileKind::Socket(s) => Some(s.clone()),
        _ => None,
    };
    if let Some(s) = sock {
        return super::net::ioctl(cx, &s, request, arg);
    }
    let device = match &file.borrow().kind {
        FileKind::Device { dev, .. } => Some(*dev),
        _ => None,
    };
    match device {
        Some(Device::Loop) => return super::loopdev::ioctl(cx, &file, request, arg),
        Some(Device::LoopControl) => return super::loopdev::control_ioctl(cx, request, arg),
        _ => {}
    }
    let host_fd = match &file.borrow().kind {
        FileKind::Host(fd) => *fd,
        FileKind::Pipe(end) if request == FIONREAD => {
            let n = end.pipe.borrow().buf.len() as i32;
            return put(cx, arg, n.to_le_bytes()).map(|()| 0);
        }
        FileKind::Inotify(i) if request == FIONREAD => {
            let n = i.borrow().pending() as i32;
            return put(cx, arg, n.to_le_bytes()).map(|()| 0);
        }
        FileKind::Device {
            dev: Device::Random,
            ..
        } => return random_ioctl(cx, request, arg),
        kind => {
            crate::stub::ioctl(cx, request, crate::stub::file_kind(kind));
            return Err(Errno::ENOTTY);
        }
    };
    let Some(&(_, size, out)) = HOST_IOCTLS.iter().find(|(r, ..)| *r == request) else {
        crate::stub::ioctl(cx, request, "host file");
        return Err(Errno::ENOTTY);
    };
    let mut buf = vec![0u8; size];
    if !out {
        cx.mm().read(UserSlice::new(arg, size), &mut buf)?;
    }
    let rc = tuxport_hal::ioctl(host_fd, request as u64, &mut buf).map_err(Errno)?;
    if out {
        cx.mm().write(UserSlice::new(arg, size), &buf)?;
    }
    Ok(rc as u64)
}

/// FIONREAD on a regular file (Linux's `do_vfs_ioctl`): the size as stat reports it less the file
/// position, as an int (negative past the end, cut to 32 bits as Linux's `put_user` into an int
/// cuts it); None when the file is not regular.
fn unread(cx: &Cx<'_>, file: &FileRef) -> Result<Option<i32>, Errno> {
    // asm-generic's struct stat: st_mode at 16, st_size at 48 (encode_stat)
    let st = file_stat(cx, file)?;
    let mode = u32::from_le_bytes(st[16..20].try_into().unwrap());
    if mode & mode::S_IFMT != mode::S_IFREG {
        return Ok(None);
    }
    let size = i64::from_le_bytes(st[48..56].try_into().unwrap());
    Ok(Some(size.wrapping_sub(file.borrow().pos as i64) as i32))
}

/// The open file of an ioctl fd or FICLONE's source fd: as with Linux's `fdget`, files opened
/// with `O_PATH` are not accepted (EBADF).
fn io_file(cx: &Cx<'_>, fd: i32) -> Result<FileRef, Errno> {
    let file = cx.fds().get(fd)?;
    if file.borrow().flags & oflags::O_PATH != 0 {
        return Err(Errno::EBADF);
    }
    Ok(file)
}

/// FICLONE, FICLONERANGE: checked in Linux 6.1's order (`ioctl_file_clone`,
/// `do_clone_file_range`, `generic_remap_file_range_prep`, `generic_remap_checks`), with the
/// store's regular files cloning as those of btrfs and XFS do: `len` 0 is to the source's end,
/// offsets must be block-aligned, and so must the length unless it reaches the source's end. A
/// clone of a whole file into the start of one no longer than it shares the object
/// ([`try_share`]); any other range, or a file that cannot share now, is copied, which leaves the
/// same contents. tmpfs, overlay files and memfds have no reflink (EOPNOTSUPP), as in Linux
/// (shmem has none, and an overlay on ext4 has none either); files not on the same mount are
/// EXDEV.
fn clone_range(
    cx: &mut Cx<'_>,
    input: &FileRef,
    output: &FileRef,
    (pos_in, len, pos_out): (u64, u64, u64),
) -> SysResult {
    const BLOCK: u64 = 4096;
    let vfs = cx.vfs()?;
    {
        let (i, o) = (input.borrow(), output.borrow());
        // A memfd is on the internal shm mount
        let mount = |f: &OpenFile| match f.pseudo {
            true => None,
            false => f.node().map(|n| n.mnt()),
        };
        if i.pseudo != o.pseudo || mount(&i) != mount(&o) {
            return Err(Errno::EXDEV);
        }
        // generic_file_rw_checks
        if [&i, &o]
            .iter()
            .any(|f| matches!(f.kind, FileKind::Directory { .. }))
        {
            return Err(Errno::EISDIR);
        }
        if [&i, &o]
            .iter()
            .any(|f| !matches!(f.kind, FileKind::Regular { .. }))
        {
            return Err(Errno::EINVAL);
        }
        if !i.readable() || !o.writable() || o.flags & oflags::O_APPEND != 0 {
            return Err(Errno::EBADF);
        }
        let tmpfs = |f: &OpenFile| {
            f.data_ino()
                .is_some_and(|ino| vfs.tmpfs_dev(&Node::Store(f.mnt, ino)).is_some())
        };
        if i.pseudo || i.ovl.is_some() || o.ovl.is_some() || tmpfs(&i) {
            return Err(Errno::EOPNOTSUPP);
        }
    }
    // remap_verify_area
    let fits = |pos: u64| {
        pos as i64 >= 0 && len as i64 >= 0 && pos.checked_add(len).is_some_and(|e| e as i64 >= 0)
    };
    if !fits(pos_in) || !fits(pos_out) {
        return Err(Errno::EINVAL);
    }
    let size = |f: &FileRef| -> Result<u64, Errno> {
        match &f.borrow().kind {
            FileKind::Regular { ino, file, .. } => Ok(vfs.store().stat_open(*ino, file)?.size),
            _ => Err(Errno::EINVAL),
        }
    };
    let (size_in, size_out) = (size(input)?, size(output)?);
    let mut len = len;
    if len == 0 {
        if pos_in == size_in {
            return Ok(0);
        }
        if pos_in > size_in {
            return Err(Errno::EINVAL);
        }
        len = size_in - pos_in;
    }
    if pos_in % BLOCK != 0 || pos_out % BLOCK != 0 || pos_in >= size_in {
        return Err(Errno::EINVAL);
    }
    let mut count = len.min(size_in - pos_in);
    // generic_write_check_limits
    let limit = cx.t.group.borrow().rlimits[RLIMIT_FSIZE as usize].0;
    if limit != u64::MAX {
        if pos_out >= limit {
            return Err(file_too_big(cx));
        }
        count = count.min(limit - pos_out);
    }
    count = count.min(i64::MAX as u64 - pos_out);
    let bcount = if pos_in + count == size_in {
        size_in.next_multiple_of(BLOCK) - pos_in
    } else if count % BLOCK != 0 {
        return Err(Errno::EINVAL);
    } else {
        count
    };
    let same = input.borrow().data_ino() == output.borrow().data_ino();
    if same && pos_out + bcount > pos_in && pos_out < pos_in + bcount {
        return Err(Errno::EINVAL);
    }
    // A request cut short is bounced back (no REMAP_FILE_CAN_SHORTEN)
    if count != len {
        return Err(Errno::EINVAL);
    }
    let whole = pos_in == 0 && pos_out == 0 && count == size_in && size_out <= size_in;
    if whole && try_share(cx, input, output)? {
        inotify::file_event(cx.kr(), &input.borrow(), inotify::IN_ACCESS);
        inotify::file_event(cx.kr(), &output.borrow(), inotify::IN_MODIFY);
        return Ok(0);
    }
    let mut done = 0;
    while done < count {
        let n = (count - done).min(MAX_COPY as u64) as usize;
        let at = (Some(pos_in + done), Some(pos_out + done));
        match copy_data(cx, input, at.0, output, at.1, n)? {
            0 => return Err(Errno::EIO),
            k => done += k as u64,
        }
    }
    Ok(0)
}

// ---- Reading and writing ----

/// The route one read or write takes, carried out without holding a borrow of the open file.
enum IoPath {
    Host(i32),
    Pipe(Rc<RefCell<Pipe>>),
    /// A terminal, whether the open file is the master, and how many hangups it had seen when
    /// opened ([`crate::tty::PtyEnd::born`]).
    Pty(Rc<RefCell<Tty>>, (bool, u32)),
    Socket(SocketRef),
    EventFd(Rc<RefCell<EventFd>>),
    TimerFd(Rc<RefCell<TimerFd>>),
    SignalFd(Rc<RefCell<crate::fd::SignalFd>>),
    Inotify(Rc<RefCell<super::Inotify>>),
    /// `/proc/<pid>/pagemap` (or a task's): computed on the fly from the read position, from the
    /// address space of that thread.
    Pagemap(Tid),
    /// `/proc/<pid>/mem`: the position is the address.
    /// The address space of `/proc/<pid>/mem` at open, None when it has been released.
    Mem(Option<Rc<RefCell<crate::mm::Mm>>>),
    /// `/dev/kmsg`: read and written by record, with the read position in the open file
    /// ([`crate::klog::KmsgFile`]).
    Kmsg,
    /// `/proc/kmsg`: syslog's read.
    ProcKmsg(Rc<RefCell<crate::klog::Klog>>),
    /// A loop device: read and written through its backing file (`syscall::loopdev`).
    Loop(crate::syscall::loopdev::LoopRef),
    /// A file in a FUSE filesystem: read and written through its daemon (`syscall::fuse`).
    Fuse(super::fuse::Io),
    /// `/dev/fuse`: a daemon reading requests and writing replies.
    FuseDev,
    /// Everything else (files in the store, devices, other synthetic files, and files with no
    /// read or write operation): never waits for data. Regular files are read and written with
    /// the BKL released ([`unlocked_io`]), except `O_APPEND` writes; the rest is done directly
    /// while holding the borrow.
    Local,
}

const SPLICE_F_NONBLOCK: u32 = 2;

pub(super) fn read(cx: &mut Cx<'_>, fd: i32, buf: u64, len: usize) -> SysResult {
    let file = cx.fds().get(fd)?;
    read_file(cx, &file, buf, len, None)
}

pub(super) fn write(cx: &mut Cx<'_>, fd: i32, buf: u64, len: usize) -> SysResult {
    let file = cx.fds().get(fd)?;
    write_file(cx, &file, buf, len, None)
}

pub(super) fn pread(cx: &mut Cx<'_>, fd: i32, buf: u64, len: usize, offset: i64) -> SysResult {
    let file = cx.fds().get(fd)?;
    read_file(cx, &file, buf, len, checked_offset(offset)?)
}

pub(super) fn pwrite(cx: &mut Cx<'_>, fd: i32, buf: u64, len: usize, offset: i64) -> SysResult {
    let file = cx.fds().get(fd)?;
    let offset = append_offset(&file, checked_offset(offset)?)?;
    write_file(cx, &file, buf, len, offset)
}

pub(super) fn readv(cx: &mut Cx<'_>, fd: i32, iov: u64, count: usize) -> SysResult {
    vectored(cx, fd, iov, count, None, false)
}

pub(super) fn writev(cx: &mut Cx<'_>, fd: i32, iov: u64, count: usize) -> SysResult {
    vectored(cx, fd, iov, count, None, true)
}

/// preadv, pwritev; with `flags` not None, preadv2 and pwritev2, where an offset of -1 means the
/// file position.
pub(super) fn pvectored(
    cx: &mut Cx<'_>,
    fd: i32,
    iov: u64,
    count: usize,
    offset: i64,
    flags: Option<u32>,
    write: bool,
) -> SysResult {
    // RWF_HIPRI does not affect the result; RWF_DSYNC and RWF_SYNC wait for the write to reach
    // disk ([`write_sync`]); RWF_NOAPPEND (since Linux 6.9, used by musl's pwrite) makes a write
    // with an offset on an O_APPEND file also write at the given offset; RWF_NOWAIT and
    // RWF_APPEND are not supported, and as on Linux every unknown flag is EOPNOTSUPP
    const RWF_NOAPPEND: u32 = 0x20;
    const RWF_SUPPORTED: u32 = 0x7 | RWF_NOAPPEND;
    let mut offset = match flags {
        Some(f) if f & !RWF_SUPPORTED != 0 => return Err(Errno::EOPNOTSUPP),
        Some(_) if offset == -1 => None,
        _ => checked_offset(offset)?,
    };
    if write && flags.is_none_or(|f| f & RWF_NOAPPEND == 0) {
        offset = append_offset(&cx.fds().get(fd)?, offset)?;
    }
    let n = vectored(cx, fd, iov, count, offset, write)?;
    if let Some(rwf) = flags.filter(|_| write && n > 0) {
        let file = cx.fds().get(fd)?;
        write_sync(cx, &file, rwf)?;
    }
    Ok(n)
}

fn checked_offset(offset: i64) -> Result<Option<u64>, Errno> {
    u64::try_from(offset).map(Some).map_err(|_| Errno::EINVAL)
}

/// A write with an offset on an `O_APPEND` regular file still writes at the end, leaving the
/// file position unchanged (Linux's generic_write_checks, the BUGS section of pwrite(2)); only
/// pwritev2's `RWF_NOAPPEND` writes at the given offset. Such writes are done with the BKL held
/// ([`write_bytes`]), so they never write at the same place as other appends.
pub(super) fn append_offset(file: &FileRef, offset: Option<u64>) -> Result<Option<u64>, Errno> {
    let f = file.borrow();
    match &f.kind {
        FileKind::Regular { file, .. } if offset.is_some() && f.flags & oflags::O_APPEND != 0 => {
            Ok(Some(file.metadata().map_err(host_errno)?.len()))
        }
        _ => Ok(offset),
    }
}

/// splice: moves data between a pipe and another file (or two pipes). Copies through a host
/// buffer, not zero-copy. When to wait follows Linux 6.1's `do_splice`: between two pipes, for data
/// and then for room unless `SPLICE_F_NONBLOCK` or either end's `O_NONBLOCK`; from a pipe, for
/// data unless `SPLICE_F_NONBLOCK` or the input pipe's `O_NONBLOCK`, writing the output as its own
/// `O_NONBLOCK` says (data leaves the pipe only after the write succeeds, so a failed write loses
/// none); into a pipe, for room unless `SPLICE_F_NONBLOCK` or the output pipe's `O_NONBLOCK`,
/// reading the input as its own `O_NONBLOCK` says ([`splice_read`]).
pub(super) fn splice(
    cx: &mut Cx<'_>,
    fd_in: i32,
    off_in: u64,
    fd_out: i32,
    off_out: u64,
    len: usize,
    flags: u32,
) -> SysResult {
    if len == 0 {
        return Ok(0);
    }
    if flags & !SPLICE_F_ALL != 0 {
        return Err(Errno::EINVAL);
    }
    let input = cx.fds().get(fd_in)?;
    let output = cx.fds().get(fd_out)?;
    if !input.borrow().readable() || !output.borrow().writable() {
        return Err(Errno::EBADF);
    }
    let (in_pipe, out_pipe) = (pipe_of(&input), pipe_of(&output));
    if in_pipe.is_none() && out_pipe.is_none()
        || output.borrow().flags & oflags::O_APPEND != 0
        || !splice_reads(&input.borrow())
        || !splice_writes(&output.borrow())
    {
        return Err(Errno::EINVAL);
    }
    if in_pipe.is_some() && off_in != 0 || out_pipe.is_some() && off_out != 0 {
        return Err(Errno::ESPIPE);
    }
    let read_off = |cx: &Cx<'_>, addr: u64| -> Result<Option<u64>, Errno> {
        match addr {
            0 => Ok(None),
            a => Ok(Some(
                u64::try_from(i64::from_le_bytes(get(cx, a)?)).map_err(|_| Errno::EINVAL)?,
            )),
        }
    };
    let (src, dst) = (read_off(cx, off_in)?, read_off(cx, off_out)?);
    let nonblock = flags & SPLICE_F_NONBLOCK != 0;
    let wrote = match (in_pipe, out_pipe) {
        (Some((from, _)), Some((to, _))) => {
            if Rc::ptr_eq(&from, &to) {
                return Err(Errno::EINVAL);
            }
            let nonblock =
                nonblock || input.borrow().nonblocking() || output.borrow().nonblocking();
            pipe_to_pipe(cx, &from, &to, len, nonblock, true)?
        }
        (Some((pipe, _)), None) => {
            let nonblock = nonblock || input.borrow().nonblocking();
            loop {
                if !pipe_wait_data(cx, &pipe, nonblock)? {
                    return Ok(0);
                }
                if !wait_splice_done(cx, &pipe)? {
                    break;
                }
            }
            // The data stays in the pipe until written; other consumers wait meanwhile, since
            // writing can release the BKL or wait
            let _splicing = Splicing::start(&pipe);
            let mut data: Vec<u8> = pipe.borrow().buf.iter().take(len).copied().collect();
            // A FUSE reply spliced from a pipe is the `len` bytes asked for (6.1's
            // fuse_dev_splice_write). libfuse vmsplices the header first and sets its length only
            // after splicing the data behind it, which Linux sees because vmsplice puts the
            // caller's pages into the pipe; here vmsplice copied the header with the old length,
            // so the length given to splice stands in for it
            if matches!(output.borrow().kind, FileKind::FuseDev(_)) && data.len() >= 4 {
                let n = data.len() as u32;
                data[..4].copy_from_slice(&n.to_le_bytes());
            }
            let n = write_bytes(cx, &output, &data, dst)?;
            let mut p = pipe.borrow_mut();
            let before = p.space();
            debug_assert!(
                n <= p.buf.len(),
                "spliced data left the pipe while being written"
            );
            let n = n.min(p.buf.len());
            p.buf.drain(..n);
            if p.drained(before) {
                request_wake_object(crate::pipe::object(&pipe));
            }
            n
        }
        // A FUSE daemon takes one whole request into the pipe (6.1's fuse_dev_splice_read): the
        // device's own O_NONBLOCK decides whether to wait, and a request the pipe has no room
        // for is EIO
        (None, Some((pipe, _))) if matches!(input.borrow().kind, FileKind::FuseDev(_)) => {
            let nonblock = input.borrow().nonblocking();
            let message = crate::vfs::fuse::dev_read(cx, &input, len, nonblock)?;
            if message.len() > pipe.borrow().space() {
                return Err(Errno::EIO);
            }
            pipe_push(cx, &pipe, &message);
            message.len()
        }
        (None, Some((pipe, _))) => {
            // Linux's splice_file_to_pipe: wait for room, then read at most what fits
            let nonblock = nonblock || output.borrow().nonblocking();
            let room = wait_room(cx, &pipe, nonblock)?;
            let mut buf = vec![0u8; len.min(room)];
            // As on Linux, an unconnected socket cannot splice out data: EINVAL
            let got = match splice_read(cx, &input, &mut buf, src, nonblock) {
                Err(e) if e == Errno::ENOTCONN => return Err(Errno::EINVAL),
                r => r?,
            };
            pipe_push(cx, &pipe, &buf[..got]);
            got
        }
        (None, None) => unreachable!("refused above"),
    };
    if let Some(s) = src {
        put(cx, off_in, (s + wrote as u64).to_le_bytes())?;
    }
    if let Some(d) = dst {
        put(cx, off_out, (d + wrote as u64).to_le_bytes())?;
    }
    Ok(wrote as u64)
}

/// The flags splice, tee and vmsplice know (`SPLICE_F_ALL`): `MOVE`, `NONBLOCK`, `MORE`, `GIFT`.
const SPLICE_F_ALL: u32 = 0xf;

/// Whether data can be spliced out of `f` (Linux's `splice_read`), which splice and sendfile from
/// it need (EINVAL otherwise): see [`splice_kind`]; of the devices, not `/dev/null`, `/dev/kmsg`
/// or `/dev/loop-control`.
fn splice_reads(f: &OpenFile) -> bool {
    match &f.kind {
        FileKind::Device { dev, .. } => {
            !matches!(dev, Device::Null | Device::Kmsg | Device::LoopControl)
        }
        kind => splice_kind(kind),
    }
}

/// Whether data can be spliced into `f` (Linux's `splice_write`), which splice and sendfile into
/// it need (EINVAL otherwise): see [`splice_kind`]; of the devices, not `/dev/full` (6.1's
/// `full_fops` has none, so it never gets to ENOSPC), `/dev/kmsg` or `/dev/loop-control`.
fn splice_writes(f: &OpenFile) -> bool {
    match &f.kind {
        FileKind::Device { dev, .. } => {
            !matches!(dev, Device::Full | Device::Kmsg | Device::LoopControl)
        }
        kind => splice_kind(kind),
    }
}

/// The kinds of files that splice both ways (Linux's pipes, regular files, sockets, terminals,
/// FUSE files and `/dev/fuse`; procfs and sysfs files are all taken as splicing). Directories,
/// eventfd, epoll and the other anonymous files do not.
fn splice_kind(kind: &FileKind) -> bool {
    matches!(
        kind,
        FileKind::Regular { .. }
            | FileKind::Pipe(_)
            | FileKind::Socket(_)
            | FileKind::Pty(_)
            | FileKind::Proc { .. }
            | FileKind::Host(_)
            | FileKind::Fuse(_)
            | FileKind::FuseDev(_)
    )
}

/// Reads splice's input into a buffer bound for a pipe: as the file's own `O_NONBLOCK` says (TCP's
/// `tcp_splice_read`, and `read_iter` for the rest), except that an AF_UNIX stream socket also
/// stops waiting with `nonblock` (`SPLICE_F_NONBLOCK` or the output pipe's `O_NONBLOCK`), as
/// `unix_stream_splice_read` does.
fn splice_read(
    cx: &mut Cx<'_>,
    file: &FileRef,
    buf: &mut [u8],
    offset: Option<u64>,
    nonblock: bool,
) -> Result<usize, Errno> {
    let sock = match &file.borrow().kind {
        FileKind::Socket(s) => Some(s.clone()),
        _ => None,
    };
    let unix_stream = sock.filter(
        |s| matches!(&*s.borrow(), crate::net::Socket::Unix(u) if u.ty == crate::net::SOCK_STREAM),
    );
    let Some(sock) = unix_stream else {
        return read_bytes(cx, file, buf, offset);
    };
    let nonblock = nonblock || file.borrow().nonblocking();
    let m = super::net::recv(cx, &sock, nonblock, buf.len(), 0, 0)?;
    buf[..m.data.len()].copy_from_slice(&m.data);
    Ok(m.data.len())
}

/// tee: copies the data in one pipe to another without taking it from the source pipe (Linux
/// 6.1's `do_tee`): it waits for data (0 once the source has no writers), then for room in the
/// destination (`EPIPE` without readers), unless `SPLICE_F_NONBLOCK` or either end's `O_NONBLOCK`,
/// and copies what is there and fits at that moment.
pub(super) fn tee(cx: &mut Cx<'_>, fd_in: i32, fd_out: i32, len: usize, flags: u32) -> SysResult {
    if flags & !SPLICE_F_ALL != 0 {
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    let input = cx.fds().get(fd_in)?;
    let output = cx.fds().get(fd_out)?;
    if !input.borrow().readable() || !output.borrow().writable() {
        return Err(Errno::EBADF);
    }
    let (Some((src, _)), Some((dst, _))) = (pipe_of(&input), pipe_of(&output)) else {
        return Err(Errno::EINVAL);
    };
    if Rc::ptr_eq(&src, &dst) {
        return Err(Errno::EINVAL);
    }
    let nonblock = flags & SPLICE_F_NONBLOCK != 0
        || input.borrow().nonblocking()
        || output.borrow().nonblocking();
    pipe_to_pipe(cx, &src, &dst, len, nonblock, false).map(|n| n as u64)
}

/// splice between two pipes (`take`) and tee: Linux's `ipipe_prep` (wait for data in `src`, going
/// on at end of file), `opipe_prep` (wait for room in `dst`), then `splice_pipe_to_pipe` or
/// `link_pipe`, which move or copy what is there and fits. When another reader or writer took the
/// data or the room during the waits, splice waits again (`EAGAIN` when nonblocking) and tee
/// returns 0; a source at end of file gives 0 once the destination has room.
fn pipe_to_pipe(
    cx: &mut Cx<'_>,
    src: &Rc<RefCell<Pipe>>,
    dst: &Rc<RefCell<Pipe>>,
    len: usize,
    nonblock: bool,
    take: bool,
) -> Result<usize, Errno> {
    loop {
        pipe_wait_data(cx, src, nonblock)?;
        if take && wait_splice_done(cx, src)? {
            continue;
        }
        wait_room(cx, dst, nonblock)?;
        let (mut s, mut d) = (src.borrow_mut(), dst.borrow_mut());
        if d.readers == 0 {
            drop((s, d));
            return Err(broken_pipe(cx));
        }
        if s.buf.is_empty() && s.writers == 0 {
            return Ok(0);
        }
        if s.buf.is_empty() || splice_room(&d) == 0 {
            if !take {
                return Ok(0);
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            continue;
        }
        let n = len.min(s.buf.len()).min(splice_room(&d));
        if d.buf.is_empty() {
            request_wake_object(crate::pipe::object(dst));
        } else {
            super::poll::arrived(cx);
        }
        if take {
            let before = s.space();
            d.buf.extend(s.buf.drain(..n));
            if s.drained(before) {
                request_wake_object(crate::pipe::object(src));
            }
        } else {
            let data: Vec<u8> = s.buf.iter().take(n).copied().collect();
            d.buf.extend(data);
        }
        d.writes += 1;
        return Ok(n);
    }
}

/// Waits for room in a pipe that splice, tee or vmsplice writes into (Linux's `wait_for_space`
/// and `opipe_prep`): no readers is `EPIPE` with `SIGPIPE`, a full pipe `EAGAIN` when nonblocking.
/// Returns the room ([`splice_room`]).
fn wait_room(cx: &mut Cx<'_>, pipe: &Rc<RefCell<Pipe>>, nonblock: bool) -> Result<usize, Errno> {
    loop {
        {
            let p = pipe.borrow();
            if p.readers == 0 {
                drop(p);
                return Err(broken_pipe(cx));
            }
            if splice_room(&p) > 0 {
                return Ok(splice_room(&p));
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        pipe.borrow_mut().want_space(SPLICE_PAGE);
        cx.wait_object(crate::pipe::object(pipe))?;
    }
}

/// A pipe buffer's size: Linux's pipes are rings of page-sized buffers, and splice, tee and
/// vmsplice fill whole free buffers rather than adding to a partly used one.
const SPLICE_PAGE: usize = 4096;

/// The room splice, tee and vmsplice see in a pipe: its free space in whole pages, as Linux counts
/// free buffers (`do_splice_to` reads at most that many pages), so a pipe with less than a page
/// free is full to them although a small write would still fit.
fn splice_room(p: &Pipe) -> usize {
    p.space() / SPLICE_PAGE * SPLICE_PAGE
}

/// Puts data read for splice or vmsplice into a pipe whole, waking readers as a pipe write does.
/// Linux holds the pipe's lock from the wait for room to the end, so no other writer takes the
/// room meanwhile; here one may have taken it while the input was read with the BKL released, and
/// the pipe then holds more than its capacity until it is read.
fn pipe_push(cx: &mut Cx<'_>, pipe: &Rc<RefCell<Pipe>>, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let mut p = pipe.borrow_mut();
    if p.buf.is_empty() {
        request_wake_object(crate::pipe::object(pipe));
    } else {
        super::poll::arrived(cx);
    }
    p.buf.extend(data);
    p.writes += 1;
}

/// vmsplice (Linux 6.1): an fd open for writing (checked first, so a FIFO opened read-write is
/// written) puts the iovecs into its pipe up to the room there is, and one open only for reading
/// takes data from its pipe like readv; nothing is done for an empty request, and either way the
/// wait (for room, or for data) is skipped only with `SPLICE_F_NONBLOCK`, whatever the pipe's own
/// `O_NONBLOCK` (`wait_for_space`, `splice_from_pipe_next`).
pub(super) fn vmsplice(cx: &mut Cx<'_>, fd: i32, iov: u64, count: usize, flags: u32) -> SysResult {
    if flags & !SPLICE_F_ALL != 0 {
        return Err(Errno::EINVAL);
    }
    let file = cx.fds().get(fd)?;
    let write = file.borrow().writable();
    if !write && !file.borrow().readable() {
        return Err(Errno::EBADF);
    }
    let iovecs = iovecs(cx, iov, count)?;
    let total: usize = iovecs.iter().map(|v| v.1).sum();
    if total == 0 {
        return Ok(0);
    }
    let Some((pipe, _)) = pipe_of(&file) else {
        return Err(Errno::EBADF);
    };
    let nonblock = flags & SPLICE_F_NONBLOCK != 0;
    if !write {
        if !pipe_wait_data(cx, &pipe, nonblock)? {
            return Ok(0);
        }
        // Data is there now, so the read through the open file does not wait
        let mut buf = vec![0u8; total.min(IO_CHUNK)];
        let k = pipe_read(cx, &pipe, true, &mut buf)?;
        note_read(cx, &file, k);
        let mut done = 0;
        for (base, len) in iovecs {
            let n = len.min(k - done);
            if n == 0 {
                break;
            }
            let copied = cx.write_user_partial(base, &buf[done..done + n]);
            done += copied;
            if copied < n {
                break;
            }
        }
        if done == 0 {
            return Err(Errno::EFAULT);
        }
        return Ok(done as u64);
    }
    let room = wait_room(cx, &pipe, nonblock)?;
    let mut data = Vec::new();
    for (base, len) in iovecs {
        let n = len.min(room - data.len());
        let start = data.len();
        data.resize(start + n, 0);
        cx.mm().read(UserSlice::new(base, n), &mut data[start..])?;
        if data.len() == room {
            break;
        }
    }
    pipe_push(cx, &pipe, &data);
    Ok(data.len() as u64)
}

/// When the open file is a pipe, returns the pipe and whether it is the read end.
fn pipe_of(file: &FileRef) -> Option<(Rc<RefCell<Pipe>>, bool)> {
    match &file.borrow().kind {
        FileKind::Pipe(end) => Some((end.pipe.clone(), end.read)),
        _ => None,
    }
}

/// Waits for data in the pipe: returns false when all write ends are closed (EOF).
fn pipe_wait_data(
    cx: &mut Cx<'_>,
    pipe: &Rc<RefCell<Pipe>>,
    nonblock: bool,
) -> Result<bool, Errno> {
    loop {
        {
            let p = pipe.borrow();
            if !p.buf.is_empty() {
                return Ok(true);
            }
            if p.writers == 0 {
                return Ok(false);
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_object(crate::pipe::object(pipe))?;
    }
}

/// Waits while another task splices data out of the pipe ([`Pipe::splicing`]), whatever
/// `O_NONBLOCK` says, as Linux's consumers wait for the pipe's lock. Returns whether it waited, so
/// that the caller looks at the pipe again.
fn wait_splice_done(cx: &mut Cx<'_>, pipe: &Rc<RefCell<Pipe>>) -> Result<bool, Errno> {
    let mut waited = false;
    while pipe.borrow().splicing {
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_object(crate::pipe::object(pipe))?;
        waited = true;
    }
    Ok(waited)
}

/// A splice writing data out of a pipe ([`Pipe::splicing`]) until dropped, which wakes the
/// consumers that waited for it.
struct Splicing(Rc<RefCell<Pipe>>);

impl Splicing {
    fn start(pipe: &Rc<RefCell<Pipe>>) -> Splicing {
        pipe.borrow_mut().splicing = true;
        Splicing(pipe.clone())
    }
}

impl Drop for Splicing {
    fn drop(&mut self) {
        self.0.borrow_mut().splicing = false;
        request_wake_object(crate::pipe::object(&self.0));
    }
}

/// readv, writev and their positional variants. Pipes, terminals and sockets (and eventfd reads,
/// and host stdio that is not a regular file on the host) do one read or write over all the
/// iovecs, as Linux's `read_iter` and `write_iter` see them
/// ([`stream_vectored`]); everything else handles each iovec in order and stops at an incomplete
/// one, which is also what Linux does for files with only `read` or `write` (eventfd writes,
/// timerfd, signalfd, inotify, `/dev/kmsg` reads). With `offset` None the file position is used
/// and advanced.
pub(super) fn vectored(
    cx: &mut Cx<'_>,
    fd: i32,
    iov: u64,
    count: usize,
    offset: Option<u64>,
    write: bool,
) -> SysResult {
    let file = cx.fds().get(fd)?;
    // /dev/kmsg's write_iter: the whole writev is one record (systemd logs this way)
    if write && matches!(file.borrow().kind, FileKind::Device { kmsg: Some(_), .. }) {
        let vecs = iovecs(cx, iov, count)?;
        let total: usize = vecs.iter().map(|&(_, len)| len).sum();
        if !file.borrow().writable() {
            return Err(Errno::EBADF);
        }
        // A writev with total length 0 does not call the file's write (do_iter_write)
        if total == 0 {
            return Ok(0);
        }
        if total > crate::klog::LOG_LINE_MAX {
            return Err(Errno::EINVAL);
        }
        let mut data = Vec::new();
        for (base, len) in vecs {
            let start = data.len();
            data.resize(start + len, 0);
            cx.mm()
                .read(UserSlice::new(base, len), &mut data[start..])?;
        }
        return kmsg_write(cx, &file, &data).map(|n| n as u64);
    }
    let vecs = iovecs(cx, iov, count)?;
    // Nothing to transfer returns 0 after the offset and mode checks without reaching the file
    // (Linux's do_iter_read and do_iter_write; a plain write of 0 bytes does reach it, so it sends
    // an empty datagram where this does not)
    if vecs.iter().all(|v| v.1 == 0) {
        let f = file.borrow();
        positional(&f, offset)?;
        if !(if write { f.writable() } else { f.readable() }) {
            return Err(Errno::EBADF);
        }
        return Ok(0);
    }
    if let Some(r) = stream_vectored(cx, &file, &vecs, offset, write) {
        return r;
    }
    // A writev checks RLIMIT_FSIZE once for all its iovecs (generic_write_checks), so one that
    // ends at the limit only shortens it
    let room = match write {
        true => fsize_allowed(cx, &file.borrow(), offset, vecs.iter().map(|v| v.1).sum())?,
        false => usize::MAX,
    };
    let mut total = 0;
    for (base, len) in vecs {
        if total as usize >= room {
            break;
        }
        let len = len.min(room - total as usize);
        let at = offset.map(|o| o + total);
        let r = if write {
            write_file(cx, &file, base, len, at)
        } else {
            read_file(cx, &file, base, len, at)
        };
        match r {
            Ok(n) => {
                total += n;
                if (n as usize) < len {
                    break;
                }
            }
            Err(_) if total > 0 => break,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

/// readv and writev on files whose Linux operations are `read_iter`/`write_iter` and that have no
/// positions: one read or write over all the iovecs, so a datagram is one message, a pipe or
/// terminal read returns what was ready instead of waiting again once an iovec is full, a pipe
/// write up to `PIPE_BUF` stays atomic, and an eventfd value read may span iovecs. Host stdio that
/// is a pipe, terminal or socket on the host goes the same way (musl's stdio reads with a readv
/// into the caller's buffer and its own, and waiting for the second once the first was filled
/// blocks a program whose peer waits for its answer). None for the other files, which go iovec by
/// iovec: a host regular file is read whole, as Linux reads regular files.
fn stream_vectored(
    cx: &mut Cx<'_>,
    file: &FileRef,
    vecs: &[(u64, usize)],
    offset: Option<u64>,
    write: bool,
) -> Option<SysResult> {
    let path = match positional(&file.borrow(), offset) {
        Ok(p) => p,
        Err(e) => return Some(Err(e)),
    };
    Some(match path {
        IoPath::Socket(s) => {
            let nonblock = file.borrow().nonblocking();
            super::net::rw(cx, &s, nonblock, vecs, write)
        }
        IoPath::Pipe(_) | IoPath::Pty(..) if write => write_gathered(cx, file, vecs, IO_CHUNK),
        IoPath::Pipe(_) | IoPath::Pty(..) | IoPath::EventFd(_) if !write => {
            read_scattered(cx, file, vecs, IO_CHUNK)
        }
        // A FUSE message is read and written whole (fuse_dev_read and fuse_dev_write take an
        // iov_iter): libfuse writes a reply's header and data as separate iovecs
        IoPath::FuseDev if write => write_gathered(cx, file, vecs, FUSE_CHUNK),
        IoPath::FuseDev => read_scattered(cx, file, vecs, FUSE_CHUNK),
        IoPath::Host(fd) if !host_regular(fd) => {
            if write {
                write_gathered(cx, file, vecs, IO_CHUNK)
            } else {
                read_scattered(cx, file, vecs, IO_CHUNK)
            }
        }
        _ => return None,
    })
}

/// Whether host fd `fd` (host stdio) is a regular file on the host.
fn host_regular(fd: i32) -> bool {
    tuxport_hal::fstat(fd).is_ok_and(|st| st.st_mode & mode::S_IFMT == mode::S_IFREG)
}

/// readv on a pipe, terminal, eventfd or `/dev/fuse`: one read of up to `step` bytes, scattered
/// over the iovecs in order. As on Linux, when copying out faults after data was taken, the part
/// copied is returned (the rest is lost).
fn read_scattered(
    cx: &mut Cx<'_>,
    file: &FileRef,
    vecs: &[(u64, usize)],
    step: usize,
) -> SysResult {
    if !file.borrow().readable() {
        return Err(Errno::EBADF);
    }
    let total: usize = vecs.iter().map(|v| v.1).sum();
    if total == 0 {
        return Ok(0);
    }
    let mut buf = vec![0u8; total.min(step)];
    let k = read_bytes(cx, file, &mut buf, None)?;
    note_read(cx, file, k);
    let mut done = 0;
    for &(base, len) in vecs {
        let n = len.min(k - done);
        if n == 0 {
            break;
        }
        let copied = cx.write_user_partial(base, &buf[done..done + n]);
        done += copied;
        if copied < n {
            break;
        }
    }
    if done == 0 && k > 0 {
        return Err(Errno::EFAULT);
    }
    Ok(done as u64)
}

/// writev on a pipe, terminal or `/dev/fuse`: the iovecs written as one stream, in pieces of up
/// to `step` bytes that cross iovec boundaries, stopping at a short write; a fault ends the data
/// there, and what came before it is still written.
fn write_gathered(
    cx: &mut Cx<'_>,
    file: &FileRef,
    vecs: &[(u64, usize)],
    step: usize,
) -> SysResult {
    if !file.borrow().writable() {
        return Err(Errno::EBADF);
    }
    let mut rest = vecs.iter().copied();
    let mut cur = None;
    let mut done = 0;
    let mut chunk = Vec::with_capacity(step.min(vecs.iter().map(|v| v.1).sum()));
    loop {
        chunk.clear();
        let mut fault = None;
        while chunk.len() < step {
            let Some((base, len)) = cur.take().or_else(|| rest.next()) else {
                break;
            };
            let n = len.min(step - chunk.len());
            let start = chunk.len();
            chunk.resize(start + n, 0);
            if let Err(e) = cx.read_user(UserSlice::new(base, n), &mut chunk[start..]) {
                chunk.truncate(start);
                fault = Some(e);
                break;
            }
            if n < len {
                cur = Some((base.wrapping_add(n as u64), len - n));
            }
        }
        if chunk.is_empty() {
            return match fault {
                Some(e) if done == 0 => Err(e),
                _ => Ok(done as u64),
            };
        }
        match write_bytes(cx, file, &chunk, None) {
            Ok(k) => {
                done += k;
                if k < chunk.len() || fault.is_some() {
                    return Ok(done as u64);
                }
            }
            Err(_) if done > 0 => return Ok(done as u64),
            Err(e) => return Err(e),
        }
    }
}

/// Reads an iovec array.
fn iovecs(cx: &Cx<'_>, iov: u64, count: usize) -> Result<Vec<(u64, usize)>, Errno> {
    if count > UIO_MAXIOV {
        return Err(Errno::EINVAL);
    }
    let mut out = Vec::with_capacity(count);
    let mut total: usize = 0;
    for i in 0..count as u64 {
        let raw: [u8; 16] = get(cx, iov.wrapping_add(i * 16))?;
        let base = u64::from_le_bytes(raw[..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..].try_into().unwrap()) as usize;
        total = total
            .checked_add(len)
            .filter(|&t| t <= isize::MAX as usize)
            .ok_or(Errno::EINVAL)?;
        out.push((base, len));
    }
    Ok(out)
}

pub(super) fn sendfile(
    cx: &mut Cx<'_>,
    out_fd: i32,
    in_fd: i32,
    offset: u64,
    count: usize,
) -> SysResult {
    let input = cx.fds().get(in_fd)?;
    let output = cx.fds().get(out_fd)?;
    if !input.borrow().readable() || !output.borrow().writable() {
        return Err(Errno::EBADF);
    }
    // The input is a seekable file that splices (splice_direct_to_actor's FMODE_LSEEK, then
    // splice_read): a regular or FUSE file or a device, not /dev/null
    {
        let f = input.borrow();
        let seekable = match &f.kind {
            FileKind::Regular { .. } | FileKind::Device { .. } => true,
            FileKind::Fuse(h) => super::fuse::seekable(h),
            _ => false,
        };
        if !seekable || !splice_reads(&f) {
            return Err(Errno::EINVAL);
        }
    }
    // Into anything but a pipe Linux goes through do_splice_direct, which refuses O_APPEND and an
    // output without splice_write (such as /dev/full or an eventfd)
    {
        let out = output.borrow();
        if !matches!(out.kind, FileKind::Pipe(_))
            && (out.flags & oflags::O_APPEND != 0 || !splice_writes(&out))
        {
            return Err(Errno::EINVAL);
        }
    }
    let mut off = match offset {
        0 => None,
        addr => Some(u64::try_from(i64::from_le_bytes(get(cx, addr)?)).map_err(|_| Errno::EINVAL)?),
    };
    if let Some(r) = copy_regular(cx, &input, off, &output, None, count) {
        let n = r?;
        if let Some(o) = off {
            put(cx, offset, (o + n as u64).to_le_bytes())?;
        }
        return Ok(n as u64);
    }
    let mut chunk = vec![0u8; count.min(IO_CHUNK)];
    let mut done = 0;
    while done < count {
        let n = (count - done).min(IO_CHUNK);
        let before = input.borrow().pos;
        let got = match read_bytes(cx, &input, &mut chunk[..n], off) {
            Ok(0) => break,
            Ok(k) => k,
            Err(_) if done > 0 => break,
            Err(e) => return Err(e),
        };
        let wrote = match write_bytes(cx, &output, &chunk[..got], None) {
            Ok(k) => k,
            Err(_) if done > 0 => 0,
            Err(e) => {
                if off.is_none() {
                    give_back(&input, before, got);
                }
                return Err(e);
            }
        };
        // The part not written out goes back to the input
        match &mut off {
            Some(o) => *o += wrote as u64,
            None => give_back(&input, before, got - wrote),
        }
        done += wrote;
        if wrote < got || got < n {
            break;
        }
    }
    if let Some(o) = off {
        put(cx, offset, o.to_le_bytes())?;
    }
    Ok(done as u64)
}

/// copy_file_range: copies between two regular files without passing through guest memory. A
/// null offset pointer uses and advances the file position; otherwise the offset it points at
/// is used and updated.
pub(super) fn copy_file_range(
    cx: &mut Cx<'_>,
    fd_in: i32,
    off_in: u64,
    fd_out: i32,
    off_out: u64,
    len: usize,
    flags: u32,
) -> SysResult {
    // O_PATH descriptors are not found (fdget)
    let open = |fd: i32| {
        cx.fds()
            .get(fd)
            .and_then(|f| match f.borrow().flags & oflags::O_PATH {
                0 => Ok(f.clone()),
                _ => Err(Errno::EBADF),
            })
    };
    let input = open(fd_in)?;
    let output = open(fd_out)?;
    let read_off = |addr: u64| -> Result<Option<i64>, Errno> {
        match addr {
            0 => Ok(None),
            a => Ok(Some(i64::from_le_bytes(get(cx, a)?))),
        }
    };
    let (src, dst) = (read_off(off_in)?, read_off(off_out)?);
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let len = copy_range_checks(cx, (&input, src), (&output, dst), len)?;
    let (src, dst) = (src.map(|s| s as u64), dst.map(|d| d as u64));
    if len == 0 {
        return Ok(0);
    }
    // A copy of the whole input into an output no longer than it shares the input's object, as
    // Linux's vfs_copy_file_range tries remap_file_range first on filesystems with reflinks. All
    // of it is shared at once, beyond the MAX_RW_COUNT Linux remaps per call: copying the rest
    // would first make a private copy of the part already shared
    let pos_in = src.unwrap_or_else(|| input.borrow().pos);
    let pos_out = dst.unwrap_or_else(|| output.borrow().pos);
    let n = if whole_copy(cx, &input, pos_in, &output, pos_out, len as u64)?
        && try_share(cx, &input, &output)?
    {
        for (f, given) in [(&input, src), (&output, dst)] {
            if given.is_none() {
                f.borrow_mut().pos += len as u64;
            }
        }
        inotify::file_event(cx.kr(), &input.borrow(), inotify::IN_ACCESS);
        inotify::file_event(cx.kr(), &output.borrow(), inotify::IN_MODIFY);
        len
    } else {
        copy_data(cx, &input, src, &output, dst, len)?
    };
    if let Some(s) = src {
        put(cx, off_in, (s + n as u64).to_le_bytes())?;
    }
    if let Some(d) = dst {
        put(cx, off_out, (d + n as u64).to_le_bytes())?;
    }
    Ok(n as u64)
}

/// Copies up to `len` bytes from `input` to `output` (at `src`/`dst`, or the file positions,
/// which advance), returning how many were copied: between two regular files through the host
/// ([`copy_regular`]), otherwise block by block.
fn copy_data(
    cx: &mut Cx<'_>,
    input: &FileRef,
    mut src: Option<u64>,
    output: &FileRef,
    mut dst: Option<u64>,
    len: usize,
) -> Result<usize, Errno> {
    if let Some(r) = copy_regular(cx, input, src, output, dst, len) {
        return r;
    }
    // As with sendfile, an error after some bytes were copied returns those bytes
    let mut chunk = vec![0u8; len.min(IO_CHUNK)];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(IO_CHUNK);
        let before = input.borrow().pos;
        let got = match read_bytes(cx, input, &mut chunk[..n], src) {
            Ok(0) => break,
            Ok(k) => k,
            Err(_) if done > 0 => break,
            Err(e) => return Err(e),
        };
        let wrote = match write_bytes(cx, output, &chunk[..got], dst) {
            Ok(k) => k,
            Err(e) => {
                if src.is_none() {
                    give_back(input, before, got);
                }
                if done > 0 {
                    break;
                }
                return Err(e);
            }
        };
        if let Some(s) = &mut src {
            *s += wrote as u64;
        } else {
            give_back(input, before, got - wrote);
        }
        if let Some(d) = &mut dst {
            *d += wrote as u64;
        }
        done += wrote;
        if wrote < got {
            break;
        }
    }
    Ok(done)
}

/// Moves `file`'s position back over `n` bytes that a read from position `before` took but that
/// went nowhere (not written on, not copied out), never further than that read moved it: devices
/// and FUSE stream files keep their positions when read.
fn give_back(file: &FileRef, before: u64, n: usize) {
    let mut f = file.borrow_mut();
    let moved = f.pos.saturating_sub(before);
    f.pos -= moved.min(n as u64);
}

/// Whether copying `len` bytes of `input` at `pos_in` to `output` at `pos_out` leaves the output
/// a copy of the whole input: both from the start, all of a non-empty input, and an output no
/// longer than it.
fn whole_copy(
    cx: &Cx<'_>,
    input: &FileRef,
    pos_in: u64,
    output: &FileRef,
    pos_out: u64,
    len: u64,
) -> Result<bool, Errno> {
    if pos_in != 0 || pos_out != 0 {
        return Ok(false);
    }
    let vfs = cx.vfs()?;
    let size = |f: &FileRef| -> Result<Option<u64>, Errno> {
        match &f.borrow().kind {
            FileKind::Regular { ino, file, .. } => {
                Ok(Some(vfs.store().stat_open(*ino, file)?.size))
            }
            _ => Ok(None),
        }
    };
    let (Some(size_in), Some(size_out)) = (size(input)?, size(output)?) else {
        return Ok(false);
    };
    Ok(size_in > 0 && len == size_in && size_out <= size_in)
}

/// FICLONE, FICLONERANGE and a whole-file copy_file_range: when `output` is to end up a copy of
/// all of `input`, makes it share `input`'s object instead ([`Store::share`]), as btrfs and XFS
/// share extents; returns whether it did. Not done when the two are not regular files of one
/// store outside tmpfs (Linux's shmem has no reflink), when one was opened through an overlay or
/// is a memfd, when something could still change the input's object in place (a description
/// open for writing it that is not marked `cow`, a shared mapping that can be made writable) or
/// the output is mapped, sealed or being copied: a mapping does not follow a file to another
/// object.
fn try_share(cx: &mut Cx<'_>, input: &FileRef, output: &FileRef) -> Result<bool, Errno> {
    use std::os::unix::fs::MetadataExt;
    let vfs = cx.vfs()?;
    let (src, dst, src_host, dst_host) = {
        let (i, o) = (input.borrow(), output.borrow());
        let (
            FileKind::Regular {
                ino: src, file: si, ..
            },
            FileKind::Regular {
                ino: dst, file: so, ..
            },
        ) = (&i.kind, &o.kind)
        else {
            return Ok(false);
        };
        let tmpfs = |f: &OpenFile, ino: Ino| vfs.tmpfs_dev(&Node::Store(f.mnt, ino)).is_some();
        if i.pseudo || o.pseudo || i.ovl.is_some() || o.ovl.is_some() || src == dst {
            return Ok(false);
        }
        if tmpfs(&i, *src) || tmpfs(&o, *dst) {
            return Ok(false);
        }
        let host = |f: &std::fs::File| f.metadata().map(|m| (m.dev(), m.ino()));
        (
            *src,
            *dst,
            host(si).map_err(host_errno)?,
            host(so).map_err(host_errno)?,
        )
    };
    // A deleted source's object may already be on its way to the reaper; an object the Files app
    // is writing would carry its writes into both files
    let k = cx.kr();
    if k.unsharing.contains(&dst)
        || k.seals.contains_key(&dst)
        || vfs.store().attrs(src)?.nlink == 0
        || vfs.store().host_writing(src)?
        || vfs.store().host_writing(dst)?
    {
        return Ok(false);
    }
    let writer = |f: &FileRef| {
        f.try_borrow()
            .is_ok_and(|f| f.writable() && matches!(f.kind, FileKind::Regular { cow: false, .. }))
    };
    if opens_of(k, src).iter().any(writer) {
        return Ok(false);
    }
    let mapped = k.tasks.values().any(|t| {
        let res = t.res.borrow();
        let mm = res.mm.borrow();
        mm.writably_maps(src_host.0, src_host.1) || mm.maps_file(dst_host.0, dst_host.1)
    });
    if mapped {
        return Ok(false);
    }
    remove_privs(cx, dst)?;
    vfs.store().share(src, dst)?;
    rehome(cx.kr(), &vfs, dst);
    Ok(true)
}

/// copy_file_range's checks, in 6.1's `generic_copy_file_checks` and `rw_verify_area` order:
/// directories `EISDIR`, other files that are not regular `EINVAL`; an input not open for reading
/// or an output not open for writing or appending `EBADF`; files on different filesystems
/// `EXDEV`; ranges whose end wraps around `EOVERFLOW`; then the length is cut to the input's end,
/// and to `RLIMIT_FSIZE` (at or past it, `SIGXFSZ` and `EFBIG`) and the largest file size
/// (`EFBIG`); overlapping ranges in one file `EINVAL`; negative positions `EINVAL`. Returns the
/// length to copy. `src`/`dst` are the offsets given, None for the file positions.
fn copy_range_checks(
    cx: &Cx<'_>,
    (input, src): (&FileRef, Option<i64>),
    (output, dst): (&FileRef, Option<i64>),
    len: usize,
) -> Result<usize, Errno> {
    let (i, o) = (input.borrow(), output.borrow());
    for f in [&i, &o] {
        match f.kind {
            // A FUSE file copies through reads and writes (6.1's fuse_copy_file_range falls back
            // to generic_copy_file_range when the daemon has no FUSE_COPY_FILE_RANGE)
            FileKind::Regular { .. } | FileKind::Fuse(_) => {}
            FileKind::Directory { .. } => return Err(Errno::EISDIR),
            _ => return Err(Errno::EINVAL),
        }
    }
    if !i.readable() || !o.writable() || o.flags & oflags::O_APPEND != 0 {
        return Err(Errno::EBADF);
    }
    let vfs = cx.vfs()?;
    let dev = |f: &OpenFile| vfs.mount(f.mnt).map(|m| m.dev);
    if dev(&i) != dev(&o) {
        return Err(Errno::EXDEV);
    }
    let pos_in = src.unwrap_or(i.pos as i64);
    let pos_out = dst.unwrap_or(o.pos as i64);
    let mut count = len as u64;
    let wraps = |pos: i64| (pos as u64).wrapping_add(count) < pos as u64;
    if wraps(pos_in) || wraps(pos_out) {
        return Err(Errno::EOVERFLOW);
    }
    let size_in = match &i.kind {
        FileKind::Regular { ino, file, .. } => vfs.store().stat_open(*ino, file)?.size as i64,
        FileKind::Fuse(h) => crate::vfs::fuse::cached_attr(&vfs, h.id)?.size as i64,
        _ => return Err(Errno::EINVAL),
    };
    count = if pos_in >= size_in {
        0
    } else {
        count.min((size_in - pos_in) as u64)
    };
    // generic_write_check_limits
    let limit = cx.t.group.borrow().rlimits[RLIMIT_FSIZE as usize].0;
    if limit != u64::MAX {
        if pos_out as u64 >= limit {
            return Err(file_too_big(cx));
        }
        count = count.min(limit - pos_out as u64);
    }
    // The largest file size (MAX_LFS_FILESIZE)
    if pos_out == i64::MAX {
        return Err(Errno::EFBIG);
    }
    count = count.min(i64::MAX.wrapping_sub(pos_out) as u64);
    let end = |pos: i64| pos.wrapping_add(count as i64);
    let same = match (i.data_ino(), o.data_ino()) {
        (Some(a), Some(b)) => a == b,
        _ => i.node() == o.node(),
    };
    if same && end(pos_out) > pos_in && pos_out < end(pos_in) {
        return Err(Errno::EINVAL);
    }
    // rw_verify_area
    if pos_in < 0 || pos_out < 0 || end(pos_in) < 0 || end(pos_out) < 0 {
        return Err(Errno::EINVAL);
    }
    Ok(count as usize)
}

/// When sendfile and copy_file_range copy between two regular files, the copy is left to the
/// host kernel ([`tuxport_hal::copy_file`]): the data does not pass through the kernel layer, and
/// it takes only one host call, without the BKL. With `src`/`dst` None the file positions are
/// used and advanced; the output fd's host position is also used, so the output's position is
/// always taken as [`unlocked_io`] does, and while waiting neither is taken, so copies in the
/// two directions do not wait for each other. When it does not apply (not two different regular
/// files, appending writes, seals, unsupported by the host) returns None, and the caller reads
/// and writes block by block.
fn copy_regular(
    cx: &mut Cx<'_>,
    input: &FileRef,
    src: Option<u64>,
    output: &FileRef,
    dst: Option<u64>,
    len: usize,
) -> Option<Result<usize, Errno>> {
    if let Err(e) = follow_copy_up(cx, input) {
        return Some(Err(e));
    }
    if len > 0
        && let Err(e) = prepare_write(cx, output, None)
    {
        return Some(Err(e));
    }
    let (fd_in, fd_out, ino) = {
        let (i, o) = (input.borrow(), output.borrow());
        if o.flags & oflags::O_APPEND != 0 || i.data_ino() == o.data_ino() {
            return None;
        }
        (regular_fd(&i)?, regular_fd(&o)?, o.ino()?)
    };
    // Seals must be checked block by block; tmpfs must be accounted block by block and stop when
    // full (write_bytes)
    let in_tmpfs = cx.vfs().is_ok_and(|v| {
        v.tmpfs_dev(&Node::Store(output.borrow().mnt, ino))
            .is_some()
    });
    if cx.kr().seals.contains_key(&ino) || in_tmpfs {
        return None;
    }
    let len = match fsize_allowed(cx, &output.borrow(), dst, len.min(MAX_COPY)) {
        Ok(n) => n,
        Err(e) => return Some(Err(e)),
    };
    if len > 0
        && let Err(e) = remove_privs(cx, ino)
    {
        return Some(Err(e));
    }
    let busy = |f: &FileRef, used: bool| used && f.borrow().pos_busy != 0;
    while busy(input, src.is_none()) || busy(output, true) {
        if cx.task.killed.get() {
            return Some(Err(Errno::EINTR));
        }
        for (f, used) in [(input, src.is_none()), (output, true)] {
            if busy(f, used) {
                f.borrow_mut().pos_busy = 2;
            }
        }
        if let Err(e) = cx.wait_on(None, &mut [], crate::kernel::wake::LOCK) {
            return Some(Err(e));
        }
    }
    let from = src.unwrap_or_else(|| input.borrow().pos);
    let to = dst.unwrap_or_else(|| output.borrow().pos);
    if src.is_none() {
        input.borrow_mut().pos_busy = 1;
    }
    output.borrow_mut().pos_busy = 1;
    let r = cx.unlocked(|| tuxport_hal::copy_file(fd_in, from, fd_out, to, len));
    let mut wake = false;
    for (f, used, dst, at) in [(input, src.is_none(), src, from), (output, true, dst, to)] {
        if used {
            let mut f = f.borrow_mut();
            if let (Ok(n), None) = (r, dst) {
                f.pos = at + n as u64;
            }
            wake |= std::mem::take(&mut f.pos_busy) == 2;
        }
    }
    if wake {
        request_wake_locks();
    }
    match r.map_err(Errno) {
        // The host does not support this pair of files: the caller reads and writes block by
        // block
        Err(Errno::EINVAL | Errno::ENOSYS | Errno::EOPNOTSUPP | Errno::EXDEV) => None,
        Err(e) => Some(Err(e)),
        Ok(n) => {
            if n > 0 {
                inotify::file_event(cx.kr(), &input.borrow(), inotify::IN_ACCESS);
                inotify::file_event(cx.kr(), &output.borrow(), inotify::IN_MODIFY);
            }
            Some(Ok(n))
        }
    }
}

/// Reads from an open file into a guest buffer (read, pread, readv).
pub(super) fn read_file(
    cx: &mut Cx<'_>,
    file: &FileRef,
    buf: u64,
    len: usize,
    offset: Option<u64>,
) -> SysResult {
    // A socket receives one message of up to the whole length (super::net::rw). The borrow must
    // end before: receiving may wait with the BKL released
    let sock = socket(&file.borrow());
    if let Some(s) = sock {
        positional(&file.borrow(), offset)?;
        let nonblock = file.borrow().nonblocking();
        return super::net::rw(cx, &s, nonblock, &[(buf, len)], false);
    }
    // Each read of a terminal or pipe returns only data already ready and must not block again
    // to fill the buffer; /dev/zero and /dev/urandom fill it in one read as on Linux
    // (`dd bs=256M` reads whole blocks)
    let fill = matches!(
        file.borrow().kind,
        FileKind::Regular { .. }
            | FileKind::Proc { klog: None, .. }
            | FileKind::Device {
                dev: Device::Zero | Device::Full | Device::Random | Device::Loop,
                ..
            }
    );
    let step = io_chunk(&file.borrow());
    let mut chunk = vec![0u8; len.min(step)];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(step);
        let before = file.borrow().pos;
        let k = match read_bytes(cx, file, &mut chunk[..n], offset.map(|o| o + done as u64)) {
            Ok(k) => k,
            Err(_) if done > 0 => break,
            Err(e) => return Err(e),
        };
        note_read(cx, file, k);
        // As on Linux, a fault partway returns the bytes copied before it (and EFAULT when there
        // are none); a regular file is read up to there. Data from other files has already been
        // consumed
        let copied = cx.write_user_partial(buf.wrapping_add(done as u64), &chunk[..k]);
        done += copied;
        if copied < k {
            let positioned = matches!(
                file.borrow().kind,
                FileKind::Regular { .. } | FileKind::Fuse(_)
            );
            if offset.is_none() && positioned {
                give_back(file, before, k - copied);
            }
            if done == 0 {
                return Err(Errno::EFAULT);
            }
            break;
        }
        if k < n || !fill {
            break;
        }
    }
    Ok(done as u64)
}

/// How much one read or write of open file `f` moves at once: [`FUSE_CHUNK`] for `/dev/fuse` and
/// FUSE files, [`IO_CHUNK`] for the rest.
fn io_chunk(f: &OpenFile) -> usize {
    match f.kind {
        FileKind::FuseDev(_) | FileKind::Fuse(_) => FUSE_CHUNK,
        _ => IO_CHUNK,
    }
}

/// Bookkeeping after a read that took `k` bytes: counted for signal-driven I/O, which the timer
/// thread checks (`check_fasync`), so it is only woken.
fn note_read(cx: &Cx<'_>, file: &FileRef, k: usize) {
    if k > 0 {
        file.borrow_mut().reads += 1;
        if file.borrow().flags & oflags::O_ASYNC != 0 {
            crate::kernel::signal_later(&cx.shared.timers);
        }
    }
}

/// The socket of an open file.
fn socket(f: &OpenFile) -> Option<SocketRef> {
    match &f.kind {
        FileKind::Socket(s) => Some(s.clone()),
        _ => None,
    }
}

/// Writes a guest buffer into an open file (write, pwrite, writev).
pub(super) fn write_file(
    cx: &mut Cx<'_>,
    file: &FileRef,
    buf: u64,
    len: usize,
    offset: Option<u64>,
) -> SysResult {
    // ESPIPE and EBADF are reported before an unreadable buffer
    positional(&file.borrow(), offset)?;
    if !file.borrow().writable() {
        return Err(Errno::EBADF);
    }
    // write(2) does not skip zero-length writes: /dev/kmsg therefore logs an empty record
    if len == 0 && matches!(file.borrow().kind, FileKind::Device { kmsg: Some(_), .. }) {
        return kmsg_write(cx, file, &[]).map(|n| n as u64);
    }
    // A socket sends the whole buffer as one message, or a stream until it has all gone
    // (super::net::rw)
    let sock = socket(&file.borrow());
    if let Some(s) = sock {
        let nonblock = file.borrow().nonblocking();
        return super::net::rw(cx, &s, nonblock, &[(buf, len)], true);
    }
    // RLIMIT_FSIZE is checked once for the whole write, as generic_write_checks does: a piece
    // that ends at the limit only shortens it, without SIGXFSZ for the next
    let len = fsize_allowed(cx, &file.borrow(), offset, len)?;
    let step = io_chunk(&file.borrow());
    let mut chunk = vec![0u8; len.min(step)];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(step);
        let copied = cx.read_user(
            UserSlice::new(buf.wrapping_add(done as u64), n),
            &mut chunk[..n],
        );
        let step = copied
            .and_then(|()| write_bytes(cx, file, &chunk[..n], offset.map(|o| o + done as u64)));
        match step {
            Ok(k) => {
                done += k;
                if k < n {
                    break;
                }
            }
            // When part was already written, return the bytes written, as Linux does
            Err(_) if done > 0 => break,
            Err(e) => return Err(e),
        }
    }
    if done > 0 {
        write_sync(cx, file, 0)?;
    }
    Ok(done as u64)
}

/// Regular files opened with `O_DSYNC` or `O_SYNC`, or pwritev2 with `RWF_DSYNC` or `RWF_SYNC`
/// (`rwf`): after the write, wait for it to reach disk as fdatasync and fsync do (Linux's
/// generic_write_sync). Data, length and times are all in the host object, so the host flushes
/// them, waited for with the BKL released; names are not among them, and on Linux the directory
/// also needs a separate fsync.
pub(super) fn write_sync(cx: &mut Cx<'_>, file: &FileRef, rwf: u32) -> Result<(), Errno> {
    use oflags::{O_DSYNC, O_SYNC_BIT};
    const RWF_DSYNC: u32 = 2;
    const RWF_SYNC: u32 = 4;
    let (fd, data_only) = {
        let f = file.borrow();
        let FileKind::Regular { file: host, .. } = &f.kind else {
            return Ok(());
        };
        if f.flags & O_DSYNC == 0 && rwf & (RWF_DSYNC | RWF_SYNC) == 0 {
            return Ok(());
        }
        let full = f.flags & O_SYNC_BIT != 0 || rwf & RWF_SYNC != 0;
        (std::os::fd::AsRawFd::as_raw_fd(host), !full)
    };
    // The open file is kept alive by file and not touched while the BKL is released
    cx.unlocked(|| tuxport_hal::fsync(fd, data_only))
        .map_err(Errno)
}

/// Does one read from an open file into a host buffer. With `offset` None the file position is
/// used and advanced.
pub(super) fn read_bytes(
    cx: &mut Cx<'_>,
    file: &FileRef,
    buf: &mut [u8],
    offset: Option<u64>,
) -> Result<usize, Errno> {
    let (path, nonblock) = {
        let f = file.borrow();
        let path = positional(&f, offset)?;
        if !f.readable() {
            return Err(Errno::EBADF);
        }
        (path, f.nonblocking())
    };
    match path {
        IoPath::Host(fd) => {
            wait_host(cx, fd, POLLIN, nonblock)?;
            cx.unlocked(|| tuxport_hal::read(fd, buf)).map_err(Errno)
        }
        IoPath::Pipe(pipe) => pipe_read(cx, &pipe, nonblock, buf),
        IoPath::Pty(t, side) => tty::read(cx, &t, side, nonblock, buf),
        IoPath::Socket(s) => {
            let m = super::net::recv(cx, &s, nonblock, buf.len(), 0, 0)?;
            buf[..m.data.len()].copy_from_slice(&m.data);
            Ok(m.data.len())
        }
        IoPath::EventFd(e) => super::poll::eventfd_read(cx, &e, nonblock, buf),
        IoPath::TimerFd(t) => super::timerfd::read(cx, &t, nonblock, buf),
        IoPath::SignalFd(s) => super::sig::signalfd_read(cx, &s, nonblock, buf),
        IoPath::Inotify(i) => super::inotify::read(cx, &i, nonblock, buf),
        IoPath::Pagemap(pid) => {
            let pos = offset.unwrap_or(file.borrow().pos);
            let n = pseudo::read_pagemap(cx.kr(), pid, buf, pos)?;
            if offset.is_none() {
                file.borrow_mut().pos = pos + n as u64;
            }
            Ok(n)
        }
        IoPath::Kmsg => kmsg_read(cx, file, nonblock, buf),
        IoPath::ProcKmsg(log) => proc_kmsg_read(cx, &log, nonblock, buf),
        IoPath::Loop(dev) => super::loopdev::read(cx, file, &dev, buf, offset),
        IoPath::Fuse(io) => {
            let n = super::fuse::read(cx, file, &io, buf, offset)?;
            if n > 0 {
                inotify::file_event(cx.kr(), &file.borrow(), inotify::IN_ACCESS);
            }
            Ok(n)
        }
        IoPath::FuseDev => {
            let message = crate::vfs::fuse::dev_read(cx, file, buf.len(), nonblock)?;
            buf[..message.len()].copy_from_slice(&message);
            Ok(message.len())
        }
        IoPath::Mem(mm) => {
            let pos = offset.unwrap_or(file.borrow().pos);
            let n = pseudo::read_mem(mm, buf, pos, cx.stack_limit())?;
            if offset.is_none() {
                file.borrow_mut().pos = pos.wrapping_add(n as u64);
            }
            Ok(n)
        }
        IoPath::Local => {
            follow_copy_up(cx, file)?;
            let regular = regular_fd(&file.borrow());
            let n = match regular {
                Some(fd) => unlocked_io(cx, file, fd, (offset, false), |fd, pos| {
                    tuxport_hal::pread(fd, buf, pos)
                })?,
                None => {
                    refresh_pseudo(cx, file, offset)?;
                    local_read(&mut file.borrow_mut(), buf, offset)?
                }
            };
            if n > 0 {
                inotify::file_event(cx.kr(), &file.borrow(), inotify::IN_ACCESS);
            }
            Ok(n)
        }
    }
}

/// Waits until a host fd (the host stdio `tuxport-run` gives the guest) is readable or writable;
/// does not wait when nonblocking. Waits only on the host fd itself and registers no object:
/// nothing in the process can make it ready, and previously every pipe or PTY read or write in
/// the system woke the threads blocked on host stdin once.
fn wait_host(cx: &mut Cx<'_>, fd: i32, events: i16, nonblock: bool) -> Result<(), Errno> {
    let mut pfd = [(fd, events, 0)];
    loop {
        tuxport_hal::poll(&mut pfd, Some(Duration::ZERO)).map_err(Errno)?;
        if pfd[0].2 != 0 {
            return Ok(());
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_objects(None, &mut pfd, crate::kernel::wake::FILE, &[])?;
        if pfd[0].2 != 0 {
            return Ok(());
        }
    }
}

fn pipe_read(
    cx: &mut Cx<'_>,
    pipe: &Rc<RefCell<Pipe>>,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    loop {
        if !buf.is_empty() && wait_splice_done(cx, pipe)? {
            continue;
        }
        {
            let mut p = pipe.borrow_mut();
            if !p.buf.is_empty() || buf.is_empty() {
                let n = buf.len().min(p.buf.len());
                let before = p.space();
                for (d, s) in buf.iter_mut().zip(p.buf.drain(..n)) {
                    *d = s;
                }
                if p.drained(before) {
                    request_wake_object(crate::pipe::object(pipe));
                }
                return Ok(n);
            }
            if p.writers == 0 {
                return Ok(0);
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_object(crate::pipe::object(pipe))?;
    }
}

/// Read of `/dev/kmsg` (`devkmsg_read`): reads one record at a time
/// ([`crate::klog::Klog::ext_text`]), waiting when there is no new record (EAGAIN when
/// nonblocking). When the record to read has been pushed out it is EPIPE and the position jumps
/// to the oldest record; a buffer too small for the record is EINVAL and the record counts as
/// read, as on Linux.
fn kmsg_read(
    cx: &mut Cx<'_>,
    file: &FileRef,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    loop {
        let object = {
            let mut f = file.borrow_mut();
            let FileKind::Device {
                kmsg: Some(user), ..
            } = &mut f.kind
            else {
                return Err(Errno::EBADF);
            };
            let log = user.log.clone();
            let l = log.borrow();
            if let Some(r) = l.ext_text(user.seq) {
                if r.seq != user.seq {
                    user.seq = r.seq;
                    return Err(Errno::EPIPE);
                }
                user.seq = r.seq + 1;
                if r.text.len() > buf.len() {
                    return Err(Errno::EINVAL);
                }
                buf[..r.text.len()].copy_from_slice(&r.text);
                return Ok(r.text.len());
            }
            wake::of(&*log)
        };
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_object(object)?;
    }
}

/// Read of `/proc/kmsg` (`kmsg_read`): syslog's read, sharing the read position with syslog(2);
/// what is read is gone. Nonblocking with no unread record is EAGAIN.
fn proc_kmsg_read(
    cx: &mut Cx<'_>,
    log: &Rc<RefCell<crate::klog::Klog>>,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    if nonblock && log.borrow_mut().size_unread(true) == 0 {
        return Err(Errno::EAGAIN);
    }
    if buf.is_empty() {
        return Ok(0);
    }
    while !log.borrow().syslog_readable() {
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_object(wake::of(&**log))?;
    }
    let text = log.borrow_mut().syslog_read(buf.len());
    buf[..text.len()].copy_from_slice(&text);
    Ok(text.len())
}

/// As with Linux's seq_file (`seq_read_iter` starts over at offset 0), reading a procfs, sysfs or
/// cgroup2 file from offset 0 regenerates the content: programs that keep it open and seek back
/// to 0 to read again (procps; runc writes `cgroup.freeze` and reads it back to confirm) read the
/// current content. Content just generated at open and not yet read or written is not
/// regenerated.
fn refresh_pseudo(cx: &mut Cx<'_>, file: &FileRef, offset: Option<u64>) -> Result<(), Errno> {
    let (mnt, node, opener) = {
        let mut f = file.borrow_mut();
        let pos = offset.unwrap_or(f.pos);
        match &mut f.kind {
            FileKind::Proc {
                mnt,
                node,
                fresh,
                ns: None,
                klog: None,
                opener,
                event,
                ..
            } if pos == 0 && !pseudo::unreadable(node) => {
                // Reading a kernfs file from the start takes in its changes so far
                // (kernfs_seq_show; mounts and mountinfo take them in as poll reports them);
                // contents generated before a change are made again
                let current = event.as_ref().is_none_or(|e| {
                    let now = e.counter.get();
                    if !e.mounts {
                        e.seen.set(now);
                    }
                    e.generated.replace(now) == now
                });
                if std::mem::take(fresh) && current {
                    return Ok(());
                }
                (*mnt, node.clone(), opener.clone())
            }
            _ => return Ok(()),
        }
    };
    let vfs = cx.vfs()?;
    // With the opener's credentials, as Linux's seq_file handlers use the file's f_cred: a
    // descriptor passed to another process shows the same IDs
    let view = pseudo::View {
        cred: Some(opener),
        ..cx.view(&vfs)
    };
    let content = pseudo::content(&view, mnt, &node)?;
    if let FileKind::Proc { data, .. } = &mut file.borrow_mut().kind {
        *data = content;
    }
    Ok(())
}

fn local_read(f: &mut OpenFile, buf: &mut [u8], offset: Option<u64>) -> Result<usize, Errno> {
    let pos = offset.unwrap_or(f.pos);
    let n = match &f.kind {
        FileKind::Regular { file, .. } => file.read_at(buf, pos).map_err(host_errno)?,
        FileKind::Device { dev, .. } => match dev {
            Device::Null => 0,
            Device::Zero | Device::Full => {
                buf.fill(0);
                buf.len()
            }
            Device::Random => {
                tuxport_hal::fill_random(buf).map_err(Errno)?;
                buf.len()
            }
            Device::Kmsg => return Err(Errno::EBADF),
            // No read operation (loop devices are read through IoPath::Loop)
            Device::LoopControl | Device::Loop => return Err(Errno::EINVAL),
        },
        // Namespaces (nsfs) cannot be read
        FileKind::Proc {
            node: PseudoNode::NsFile(..),
            ..
        } => return Err(Errno::EINVAL),
        FileKind::Proc { node, .. } if pseudo::unreadable(node) => return Err(Errno::EINVAL),
        FileKind::Proc { data, .. } => {
            let start = (pos as usize).min(data.len());
            let n = buf.len().min(data.len() - start);
            buf[..n].copy_from_slice(&data[start..start + n]);
            n
        }
        // A message queue reads as its status line
        FileKind::MessageQueue(q) => {
            let data = q.borrow().status();
            let start = (pos as usize).min(data.len());
            let n = buf.len().min(data.len() - start);
            buf[..n].copy_from_slice(&data[start..start + n]);
            n
        }
        FileKind::Directory { .. } => return Err(Errno::EISDIR),
        FileKind::Epoll(_) | FileKind::Pidfd(_) | FileKind::SeccompNotify(_) | FileKind::Bpf(_) => {
            return Err(Errno::EINVAL);
        }
        _ => return Err(Errno::EBADF),
    };
    let positioned = matches!(
        f.kind,
        FileKind::Regular { .. } | FileKind::Proc { .. } | FileKind::MessageQueue(_)
    );
    if offset.is_none() && positioned {
        f.pos = pos + n as u64;
    }
    Ok(n)
}

/// Does one write to an open file. With `offset` None the file position is used and advanced,
/// and with `O_APPEND` it first moves to the end of the file.
pub(super) fn write_bytes(
    cx: &mut Cx<'_>,
    file: &FileRef,
    data: &[u8],
    offset: Option<u64>,
) -> Result<usize, Errno> {
    let (path, nonblock) = {
        let f = file.borrow();
        let path = positional(&f, offset)?;
        if !f.writable() {
            return Err(Errno::EBADF);
        }
        (path, f.nonblocking())
    };
    match path {
        IoPath::Host(fd) => match cx.unlocked(|| tuxport_hal::write(fd, data)) {
            Ok(n) => Ok(n),
            Err(e) if e == Errno::EPIPE.0 => Err(broken_pipe(cx)),
            Err(e) => Err(Errno(e)),
        },
        IoPath::Pipe(pipe) => pipe_write(cx, &pipe, nonblock, data),
        IoPath::Pty(t, side) => tty::write(cx, &t, side, nonblock, data),
        IoPath::Socket(s) => super::net::send(cx, &s, nonblock, data, &[], &[], 0),
        IoPath::EventFd(e) => super::poll::eventfd_write(cx, &e, nonblock, data),
        IoPath::TimerFd(_) | IoPath::SignalFd(_) | IoPath::Inotify(_) | IoPath::Pagemap(_) => {
            Err(Errno::EINVAL)
        }
        IoPath::Kmsg => kmsg_write(cx, file, data),
        // Opened for writing by root: proc_reg_write without a proc_write
        IoPath::ProcKmsg(_) => Err(Errno::EIO),
        IoPath::Loop(dev) => super::loopdev::write(cx, file, &dev, data, offset),
        IoPath::Fuse(io) => {
            let n = super::fuse::write(cx, file, &io, data, offset)?;
            if n > 0 {
                inotify::file_event(cx.kr(), &file.borrow(), inotify::IN_MODIFY);
            }
            Ok(n)
        }
        IoPath::FuseDev => {
            let vfs = cx.vfs()?;
            crate::vfs::fuse::dev_write(cx, &vfs, file, data)
        }
        IoPath::Mem(mm) => {
            let pos = offset.unwrap_or(file.borrow().pos);
            let sync = cx.task.sync.clone();
            let n = pseudo::write_mem(mm, data, pos, &sync, cx.stack_limit())?;
            if offset.is_none() {
                file.borrow_mut().pos = pos.wrapping_add(n as u64);
            }
            Ok(n)
        }
        IoPath::Local => {
            // A write at the position of a regular file holds the position from its checks to
            // its end, so they see where it writes (Linux takes f_pos_lock before
            // generic_write_checks); an O_APPEND one is checked and written under the BKL
            let held = offset.is_none()
                && regular_fd(&file.borrow()).is_some()
                && file.borrow().flags & oflags::O_APPEND == 0;
            if held {
                take_pos(cx, file)?;
            }
            let r = write_local(cx, file, data, (offset, held));
            if held {
                release_pos(file);
            }
            r
        }
    }
}

/// [`write_bytes`] to the files written here (`IoPath::Local`): regular and synthetic files.
/// `held`: the caller holds the position ([`take_pos`]).
fn write_local(
    cx: &mut Cx<'_>,
    file: &FileRef,
    data: &[u8],
    (offset, held): (Option<u64>, bool),
) -> Result<usize, Errno> {
    let data = &data[..fsize_allowed(cx, &file.borrow(), offset, data.len())?];
    let (n, tmpfs) = tmpfs_write_room(cx, &file.borrow(), offset, data.len())?;
    let data = &data[..n];
    check_seals(cx, &file.borrow(), true, |size| {
        let f = file.borrow();
        let pos = match offset {
            Some(off) => off,
            None if f.flags & oflags::O_APPEND != 0 => size,
            None => f.pos,
        };
        size.max(pos + data.len() as u64)
    })?;
    let proc_node = match &file.borrow().kind {
        FileKind::Proc { node, opener, .. } => Some((node.clone(), opener.clone())),
        _ => None,
    };
    let mut written = data.len();
    if let Some((node, opener)) = proc_node {
        let cred = cx.cred();
        let (ns, pid_ns) = (cx.ns(), cx.pid_ns());
        let tid = cx.tid();
        pseudo::write(cx.k(), (tid, &cred, &opener), (&ns, &pid_ns), &node, data)?;
        written = pseudo::written(&node, data.len());
        if let FileKind::Proc { fresh, .. } = &mut file.borrow_mut().kind {
            *fresh = false;
        }
        // The cgroup memory limit changed: wake the timer thread to measure once right away
        if let PseudoNode::Cgroup(_, b"memory.max") = node {
            cx.k().oom.recheck();
            crate::kernel::signal_later(&cx.shared.timers);
        }
        // A write is IN_MODIFY (fsnotify_modify); of the synthetic files only cgroup2's can be
        // watched
        if let PseudoNode::Cgroup(id, name) = node {
            let key = crate::cgroup::key(id, name);
            super::kernfs_modified(cx.kr(), key, crate::cgroup::key(id, b""), name);
        }
    }
    let ino = match file.borrow().kind {
        FileKind::Regular { ino, .. } => Some(ino),
        _ => None,
    };
    if let Some(ino) = ino.filter(|_| !data.is_empty()) {
        prepare_write(cx, file, None)?;
        remove_privs(cx, ino)?;
    }
    // Regular files are written without the BKL; O_APPEND ones are still written with the lock
    // held (including writes with an offset, [`append_offset`]), so several processes appending
    // at once do not overwrite each other
    let regular =
        regular_fd(&file.borrow()).filter(|_| file.borrow().flags & oflags::O_APPEND == 0);
    let n = match regular {
        Some(fd) => unlocked_io(cx, file, fd, (offset, held), |fd, pos| {
            tuxport_hal::pwrite(fd, data, pos)
        })?,
        None => local_write(&mut file.borrow_mut(), data, offset)?.min(written),
    };
    if let (Some(t), FileKind::Regular { file: host, .. }) = (tmpfs, &file.borrow().kind) {
        tmpfs_resized(&*cx.vfs()?, Some(t), host);
    }
    if n > 0 {
        inotify::file_event(cx.kr(), &file.borrow(), inotify::IN_MODIFY);
    }
    Ok(n)
}

fn pipe_write(
    cx: &mut Cx<'_>,
    pipe: &Rc<RefCell<Pipe>>,
    nonblock: bool,
    data: &[u8],
) -> Result<usize, Errno> {
    let mut done = 0;
    loop {
        let progress = {
            let mut p = pipe.borrow_mut();
            if p.readers == 0 {
                drop(p);
                let e = broken_pipe(cx);
                return if done > 0 { Ok(done) } else { Err(e) };
            }
            let want = data.len() - done;
            // Writes up to PIPE_BUF must complete at once and not interleave with other writes
            let space = p.space();
            if (data.len() <= PIPE_BUF && space >= want) || (data.len() > PIPE_BUF && space > 0) {
                let n = want.min(space);
                // Readers wait only while the pipe is empty, and poll reports it unreadable only
                // while empty; when it is not empty only edge-triggered epoll needs to know that
                // more data came
                if p.buf.is_empty() {
                    request_wake_object(crate::pipe::object(pipe));
                } else {
                    super::poll::arrived(cx);
                }
                p.buf.extend(&data[done..done + n]);
                p.writes += 1;
                done += n;
                true
            } else {
                // Wait for the reader to make room: a small write needs room for all of it at
                // once; a large write waits until at least a page fits, so it is not woken every
                // time the reader reads a little
                p.want_space(if data.len() <= PIPE_BUF {
                    want
                } else {
                    want.min(PIPE_BUF)
                });
                false
            }
        };
        if done == data.len() {
            return Ok(done);
        }
        if progress {
            continue;
        }
        if nonblock || cx.interrupted() {
            let e = if nonblock {
                Errno::EAGAIN
            } else {
                Errno::ERESTARTSYS
            };
            return if done > 0 { Ok(done) } else { Err(e) };
        }
        cx.wait_object(crate::pipe::object(pipe))?;
    }
}

/// The write end has no readers left: EPIPE, and SIGPIPE to the current thread.
fn broken_pipe(cx: &mut Cx<'_>) -> Errno {
    let task = cx.task.clone();
    signal::send_to_task(cx.kr(), &task, Signal::PIPE, SigInfo::kernel());
    Errno::EPIPE
}

/// Write of `/dev/kmsg` (`devkmsg_write`): one write is one record
/// ([`crate::klog::KmsgFile::write`]), and more than `LOG_LINE_MAX` is EINVAL; without a prefix
/// the level is the default message level of `/proc/sys/kernel/printk`.
fn kmsg_write(cx: &mut Cx<'_>, file: &FileRef, data: &[u8]) -> Result<usize, Errno> {
    if data.len() > crate::klog::LOG_LINE_MAX {
        return Err(Errno::EINVAL);
    }
    let level = cx.kr().printk[1];
    if let FileKind::Device {
        kmsg: Some(user), ..
    } = &mut file.borrow_mut().kind
    {
        user.write(data, level);
    }
    Ok(data.len())
}

/// `RLIMIT_FSIZE`: a write to a regular file past the limit writes only up to the limit; when
/// not a single byte can be written, sends SIGXFSZ to the current thread and returns EFBIG.
/// Returns the number of bytes allowed.
fn fsize_allowed(
    cx: &Cx<'_>,
    f: &OpenFile,
    offset: Option<u64>,
    len: usize,
) -> Result<usize, Errno> {
    let FileKind::Regular { file, .. } = &f.kind else {
        return Ok(len);
    };
    let limit = cx.t.group.borrow().rlimits[RLIMIT_FSIZE as usize].0;
    if limit == u64::MAX || len == 0 {
        return Ok(len);
    }
    let pos = match offset {
        Some(off) => off,
        None if f.flags & oflags::O_APPEND != 0 => file.metadata().map_err(host_errno)?.len(),
        None => f.pos,
    };
    if pos >= limit {
        return Err(file_too_big(cx));
    }
    Ok(len.min((limit - pos) as usize))
}

fn local_write(f: &mut OpenFile, data: &[u8], offset: Option<u64>) -> Result<usize, Errno> {
    match &f.kind {
        FileKind::Regular { file, .. } => {
            let pos = match offset {
                Some(off) => off,
                None if f.flags & oflags::O_APPEND != 0 => {
                    file.metadata().map_err(host_errno)?.len()
                }
                None => f.pos,
            };
            let n = file.write_at(data, pos).map_err(host_errno)?;
            if offset.is_none() {
                f.pos = pos + n as u64;
            }
            Ok(n)
        }
        FileKind::Device { dev, .. } => match dev {
            Device::Full => Err(Errno::ENOSPC),
            // No write operation (loop devices are written through IoPath::Loop)
            Device::LoopControl | Device::Loop => Err(Errno::EINVAL),
            _ => Ok(data.len()),
        },
        // Writable procfs files: the content was already handled by pseudo::write
        FileKind::Proc { .. } => Ok(data.len()),
        // Anonymous inodes without a write operation
        FileKind::Epoll(_)
        | FileKind::Pidfd(_)
        | FileKind::MessageQueue(_)
        | FileKind::SeccompNotify(_)
        | FileKind::Bpf(_) => Err(Errno::EINVAL),
        _ => Err(Errno::EBADF),
    }
}

/// Which route a read or write takes. Reads and writes with an offset (pread, pwritev) on a file
/// that cannot seek are ESPIPE: as on Linux this comes before the open-mode check (pwrite on a
/// pipe opened read-only is also ESPIPE), except for `O_PATH`.
fn positional(f: &OpenFile, offset: Option<u64>) -> Result<IoPath, Errno> {
    let path = io_path(f);
    // pread and pwrite of /dev/kmsg and /proc/kmsg ignore the offset, like read and write
    let seekable = matches!(
        path,
        IoPath::Local
            | IoPath::Pagemap(_)
            | IoPath::Mem(_)
            | IoPath::Kmsg
            | IoPath::ProcKmsg(_)
            | IoPath::Loop(_)
    ) || matches!(&f.kind, FileKind::Fuse(h) if super::fuse::seekable(h));
    if offset.is_some() && !seekable && f.flags & oflags::O_PATH == 0 {
        return Err(Errno::ESPIPE);
    }
    Ok(path)
}

/// Whether the file can be read and written by position (AIO ignores the offset for files that
/// cannot seek).
pub(super) fn seekable(f: &OpenFile) -> bool {
    matches!(
        io_path(f),
        IoPath::Local | IoPath::Pagemap(_) | IoPath::Mem(_) | IoPath::Loop(_)
    ) || matches!(&f.kind, FileKind::Fuse(h) if super::fuse::seekable(h))
}

fn io_path(f: &OpenFile) -> IoPath {
    match &f.kind {
        FileKind::Host(fd) => IoPath::Host(*fd),
        FileKind::Pipe(end) => IoPath::Pipe(end.pipe.clone()),
        FileKind::Pty(end) => IoPath::Pty(end.tty.clone(), (end.master, end.born)),
        FileKind::Socket(s) => IoPath::Socket(s.clone()),
        FileKind::EventFd(e) => IoPath::EventFd(e.clone()),
        FileKind::TimerFd(t) => IoPath::TimerFd(t.clone()),
        FileKind::SignalFd(s) => IoPath::SignalFd(s.clone()),
        FileKind::Inotify(i) => IoPath::Inotify(i.clone()),
        FileKind::Proc {
            node: PseudoNode::Entry(pid, tid, pseudo::Entry::Pagemap),
            ..
        } => IoPath::Pagemap(tid.unwrap_or(*pid)),
        FileKind::Proc {
            node: PseudoNode::Entry(_, _, pseudo::Entry::Mem),
            mm,
            ..
        } => IoPath::Mem(mm.as_ref().and_then(std::rc::Weak::upgrade)),
        FileKind::Device { kmsg: Some(_), .. } => IoPath::Kmsg,
        FileKind::Device {
            block: Some(open), ..
        } => IoPath::Loop(open.dev.clone()),
        FileKind::Proc {
            klog: Some(log), ..
        } => IoPath::ProcKmsg(log.clone()),
        FileKind::Fuse(h) => IoPath::Fuse(super::fuse::Io::of(h)),
        FileKind::FuseDev(_) => IoPath::FuseDev,
        _ => IoPath::Local,
    }
}

/// The host fd of a regular file: the BKL can be released while reading or writing it
/// ([`unlocked_io`]).
/// A regular file opened through an overlay before it was copied up still holds the lower
/// inode's object: reads, fstat, seeks and copies out of it then switch to the upper one, as
/// 6.1's `ovl_real_fdget` does, so they see what was changed through the overlay since (a
/// mapping keeps what it mapped). The lower object stays open with the file
/// ([`OpenFile::ovl_lower`]): a read without the BKL may still be using it.
pub(super) fn follow_copy_up(cx: &Cx<'_>, file: &FileRef) -> Result<(), Errno> {
    let (o, ino) = match &*file.borrow() {
        OpenFile {
            ovl: Some(o),
            kind: FileKind::Regular { ino, .. },
            ..
        } => (*o, *ino),
        _ => return Ok(()),
    };
    let vfs = cx.vfs()?;
    let real = vfs.overlay().node(o).real();
    if real == ino {
        return Ok(());
    }
    let writable = file.borrow().writable();
    // The copied-up file shares the lower one's object until written (open_store)
    let held = vfs.store().held(real)?;
    let object = vfs.store().open_object(real, writable && !held)?;
    let mut f = file.borrow_mut();
    if let FileKind::Regular { ino, file, cow } = &mut f.kind {
        *ino = real;
        *cow = writable && held;
        let lower = std::mem::replace(file, object);
        f.ovl_lower = Some(lower);
    }
    Ok(())
}

/// Copies overlay node `o` up ([`Vfs::overlay_upper`]) and returns its upper inode. A regular
/// file's copy shares the lower file's object until either is written, so nothing may still write
/// that object in place, as for [`try_share`]: when something could (a writable shared mapping of
/// the lower file, the Files app writing it) the data is copied instead, and descriptions of the
/// lower file open for writing move to the shared object read-only, marked `cow` ([`rehome`]).
pub(super) fn copy_up(cx: &Cx<'_>, vfs: &Vfs, o: OvlId, what: CopyUp) -> Result<Ino, Errno> {
    use std::os::unix::fs::MetadataExt;
    let (upper, lower) = {
        let ovl = vfs.overlay();
        let n = ovl.node(o);
        (n.upper, n.real())
    };
    if let Some(up) = upper {
        return Ok(up);
    }
    if what != CopyUp::All || vfs.store().attrs(lower)?.kind() != mode::S_IFREG {
        return vfs.overlay_upper(o, what);
    }
    let k = cx.kr();
    let host = vfs.store().open_object(lower, false)?;
    let host = host.metadata().map_err(host_errno)?;
    let mapped = k.tasks.values().any(|t| {
        t.res
            .borrow()
            .mm
            .borrow()
            .writably_maps(host.dev(), host.ino())
    });
    if mapped || vfs.store().host_writing(lower)? {
        return vfs.overlay_upper(o, CopyUp::Copied);
    }
    let up = vfs.overlay_upper(o, CopyUp::All)?;
    let writer = |f: &FileRef| {
        f.try_borrow()
            .is_ok_and(|f| f.writable() && matches!(f.kind, FileKind::Regular { cow: false, .. }))
    };
    if opens_of(k, lower).iter().any(writer) {
        rehome(k, vfs, lower);
    }
    Ok(up)
}

/// Before regular file `file` is changed (written, truncated to `keep` bytes, given space, mapped
/// shared): a description opened for writing while its file was held gives the file an object of
/// its own first ([`make_private`]). The caller holds no borrow of `file`.
pub(super) fn prepare_write(
    cx: &mut Cx<'_>,
    file: &FileRef,
    keep: Option<u64>,
) -> Result<(), Errno> {
    let ino = match &file.borrow().kind {
        FileKind::Regular { ino, cow: true, .. } => *ino,
        _ => return Ok(()),
    };
    make_private(cx, ino, keep)
}

/// Gives held file `ino` an object of its own ([`Store::unshare`]), copying the data (up to
/// `keep` bytes) with the BKL released, as copy_file_range does: copying a large file would
/// otherwise stop every process for seconds. Meanwhile others wanting to change the file wait
/// for it ([`Kernel::unsharing`](crate::kernel::Kernel::unsharing)). Then every description of
/// the file moves to its object ([`rehome`]).
pub(super) fn make_private(cx: &mut Cx<'_>, ino: Ino, keep: Option<u64>) -> Result<(), Errno> {
    use std::os::unix::fs::MetadataExt;
    let vfs = cx.vfs()?;
    let was_held = vfs.store().held(ino).unwrap_or(true);
    let mut moved_from = None;
    loop {
        if cx.kr().unsharing.contains(&ino) {
            cx.wait_on(None, &mut [], crate::kernel::wake::LOCK)?;
            continue;
        }
        let Some(job) = vfs.store().unshare(ino, keep)? else {
            break;
        };
        cx.k().unsharing.insert(ino);
        let copied = cx.unlocked(|| crate::vfs::store::copy_object(&job.from, &job.to, job.len));
        cx.k().unsharing.remove(&ino);
        request_wake_locks();
        if let Err(e) = copied {
            vfs.store().abandon_unshare(job);
            return Err(e);
        }
        let from = job.from.metadata().map(|m| (m.dev(), m.ino()));
        match vfs.store().finish_unshare(job) {
            Err(Errno::EAGAIN) => continue,
            r => r?,
        }
        moved_from = from.ok();
        break;
    }
    // Descriptions need new handles only when the file was sharing its object, or when some still
    // hold a read-only one from then: rehoming always would keep every replaced handle open until
    // close, a host fd per description each time a file kept open (a log) is truncated
    let cow = |f: &FileRef| {
        f.try_borrow()
            .is_ok_and(|f| matches!(f.kind, FileKind::Regular { cow: true, .. }))
    };
    if was_held || opens_of(cx.kr(), ino).iter().any(cow) {
        rehome(cx.kr(), &vfs, ino);
    }
    if let Some(from) = moved_from {
        move_mappings(cx, &vfs, ino, from);
    }
    Ok(())
}

/// Held file `ino` moved from host object `from` to an object of its own: its shared mappings
/// follow it ([`crate::mm::Mm::file_moving`], [`crate::mm::Mm::file_moved`]), so they show what
/// is written to the file from now on. Pages past the new object's end leave the page tables, and
/// every address space that had some syncs, before any host mapping changes. When an address
/// space is busy the mappings all stay on the old object, unchanged but safe: re-mapping a host
/// region it shares (after fork) without clipping it could let a thread touch the new object past
/// its end.
fn move_mappings(cx: &Cx<'_>, vfs: &Vfs, ino: Ino, from: (u64, u64)) {
    use std::os::unix::fs::MetadataExt;
    let Ok(object) = vfs.store().open_object(ino, true) else {
        return;
    };
    let Ok(meta) = object.metadata() else {
        return;
    };
    let mut mms: Vec<Rc<RefCell<crate::mm::Mm>>> = Vec::new();
    for t in cx.kr().tasks.values() {
        let mm = t.res.borrow().mm.clone();
        if !mms.iter().any(|m| Rc::ptr_eq(m, &mm)) {
            mms.push(mm);
        }
    }
    if mms.iter().any(|m| m.try_borrow_mut().is_err()) {
        crate::debug::log(format_args!(
            "move_mappings: an address space is busy; inode {ino}'s mappings stay"
        ));
        return;
    }
    let sync = cx.task.sync.clone();
    for mm in &mms {
        let mut m = mm.borrow_mut();
        if m.file_moving(ino, from, meta.len()) {
            m.sync(&sync);
        }
    }
    let to = Rc::new(std::os::fd::OwnedFd::from(object));
    for mm in &mms {
        if let Err(e) = mm
            .borrow_mut()
            .file_moved(ino, from, &to, (meta.dev(), meta.ino()))
        {
            crate::debug::log(format_args!("move_mappings: inode {ino}: {e:?}"));
        }
    }
}

/// Store regular file `ino` changed objects, or stopped being held: every description of it
/// takes a handle on its object as it is now, only for reading (and marked `cow`) while the file
/// is held, and the recorded program file of it goes. The old handles stay with the descriptions
/// until they are closed, since a read or write without the BKL may still be using them. A
/// description borrowed at this moment keeps its handle; one opened for writing then fails to
/// write (EBADF, its handle being read-only) rather than writing the shared object, or comes back
/// here before writing when still marked.
fn rehome(k: &crate::kernel::Kernel, vfs: &Vfs, ino: Ino) {
    let store = vfs.store();
    let held = store.held(ino).unwrap_or(true);
    for file in opens_of(k, ino) {
        let Ok(mut f) = file.try_borrow_mut() else {
            crate::debug::log(format_args!("rehome: an open file of inode {ino} is busy"));
            continue;
        };
        let write = f.writable() && !held;
        let Ok(object) = store.open_object(ino, write) else {
            continue;
        };
        let f = &mut *f;
        let cow_now = f.writable() && held;
        if let FileKind::Regular { file, cow, .. } = &mut f.kind {
            f.retired.push(std::mem::replace(file, object));
            *cow = cow_now;
        }
    }
    vfs.forget_executable(ino);
}

/// The host (the Files app) gave held regular file `ino` an object of its own as it opened it for
/// writing ([`crate::vfs::browse::open_write`]): this kernel's descriptions of it move there too, as
/// after the kernel's own unsharing (shared mappings stay on the old object). Called with the BKL
/// held.
pub(crate) fn host_unshared(k: &crate::kernel::Kernel, ino: Ino) {
    if let Some(vfs) = k.vfs.clone() {
        rehome(k, &vfs, ino);
    }
}

/// Every description of store regular file `ino` in this kernel: in fd tables, in AF_UNIX
/// messages in flight and behind loop devices, each once. Those borrowed at this moment (by the
/// caller) are left out.
fn opens_of(k: &crate::kernel::Kernel, ino: Ino) -> Vec<FileRef> {
    let mut all = k.open_files();
    all.extend(crate::unix::inflight_files(k));
    all.extend(k.loops.backing_files());
    let mut out: Vec<FileRef> = Vec::new();
    for f in all {
        let ours = f
            .try_borrow()
            .is_ok_and(|f| matches!(f.kind, FileKind::Regular { ino: i, .. } if i == ino));
        if ours && !out.iter().any(|o| Rc::ptr_eq(o, &f)) {
            out.push(f);
        }
    }
    out
}

fn regular_fd(f: &OpenFile) -> Option<i32> {
    match &f.kind {
        FileKind::Regular { file, .. } => Some(std::os::fd::AsRawFd::as_raw_fd(file)),
        _ => None,
    }
}

/// Reads or writes a regular file without the BKL: a host pread/pwrite of cold data waits on
/// flash (hundreds of microseconds to milliseconds), and while the lock is held every process's
/// syscalls would have to wait too. With `offset` None the file position is used, and an open
/// file has only one such read or write at a time (Linux's f_pos_lock); others wait for it to
/// finish. `io` is called with the host fd and position while the BKL is released, and must not
/// touch any kernel object; the open file is kept alive by the caller's `file`, whose reference
/// count and borrows are not touched while the BKL is released. `held`: the caller already took
/// the position ([`take_pos`]) and gives it back itself.
fn unlocked_io(
    cx: &mut Cx<'_>,
    file: &FileRef,
    fd: i32,
    (offset, held): (Option<u64>, bool),
    io: impl FnOnce(i32, u64) -> Result<usize, i32> + Send,
) -> Result<usize, Errno> {
    let pos = match offset {
        Some(off) => off,
        None => {
            if !held {
                take_pos(cx, file)?;
            }
            file.borrow().pos
        }
    };
    let r = cx.unlocked(|| io(fd, pos));
    if offset.is_none() {
        if let Ok(n) = r {
            file.borrow_mut().pos = pos + n as u64;
        }
        if !held {
            release_pos(file);
        }
    }
    r.map_err(Errno)
}

/// Takes `file`'s position for one read or write by position (Linux's `f_pos_lock`), waiting
/// while another has it; [`release_pos`] gives it back.
fn take_pos(cx: &mut Cx<'_>, file: &FileRef) -> Result<(), Errno> {
    while file.borrow().pos_busy != 0 {
        if cx.task.killed.get() {
            return Err(Errno::EINTR);
        }
        file.borrow_mut().pos_busy = 2;
        cx.wait_on(None, &mut [], crate::kernel::wake::LOCK)?;
    }
    file.borrow_mut().pos_busy = 1;
    Ok(())
}

fn release_pos(file: &FileRef) {
    if std::mem::take(&mut file.borrow_mut().pos_busy) == 2 {
        request_wake_locks();
    }
}

// ---- Directories ----

/// getdents64 (Linux's `iterate_dir` and `filldir64`): `O_PATH` descriptors are not found
/// (`EBADF`); a removed directory is `ENOENT`; the count is an int, so one above `INT_MAX` fits no
/// entry (`EINVAL`); entries are copied one by one, so a buffer running into memory that cannot be
/// written returns the whole entries before it (`EFAULT` when there are none).
pub(super) fn getdents64(cx: &mut Cx<'_>, fd: i32, buf: u64, count: usize) -> SysResult {
    let file = cx.fds().get(fd)?;
    if file.borrow().flags & oflags::O_PATH == 0 {
        super::fuse::fill_dir(cx, &file)?;
    }
    let mut f = file.borrow_mut();
    if f.flags & oflags::O_PATH != 0 {
        return Err(Errno::EBADF);
    }
    let OpenFile { kind, pos, .. } = &mut *f;
    let FileKind::Directory { node, entries, .. } = kind else {
        return Err(Errno::ENOTDIR);
    };
    if let Node::Store(_, ino) = node
        && cx.vfs()?.store().stat(*ino)?.nlink == 0
    {
        return Err(Errno::ENOENT);
    }
    let count = count as u32 as usize;
    if entries.is_none() {
        let vfs = cx.vfs()?;
        let view = cx.view(&vfs);
        let ino_of = |n: &Node| match n {
            Node::Store(_, i) => Ok(*i),
            Node::Pseudo(_, p) => Ok(pseudo::node_ino(view.k, p)),
            Node::Overlay(_, o) => vfs.overlay_stat(*o).map(|st| st.ino),
            Node::Fuse(_, f) => Ok(vfs.fuse().get(*f).map_or(0, |n| n.attr.ino)),
        };
        let parent = vfs.parent(&view, node)?;
        let mut list = vec![
            DirEntry {
                name: b".".to_vec(),
                ino: ino_of(node)?,
                mode: mode::S_IFDIR,
            },
            DirEntry {
                name: b"..".to_vec(),
                ino: ino_of(&parent)?,
                mode: mode::S_IFDIR,
            },
        ];
        match node {
            Node::Store(_, ino) => list.extend(vfs.store().readdir(*ino)?),
            Node::Pseudo(id, p) => list.extend(pseudo::readdir(&view, *id, p)?),
            Node::Overlay(_, o) => list.extend(vfs.overlay_readdir(*o)?),
            // Listed by fill_dir, unless the daemon's directory went away
            Node::Fuse(..) => return Err(Errno::ENOTCONN),
        }
        *entries = Some(list);
    }
    let list = entries.as_ref().unwrap();
    let mut out = Vec::new();
    let mut next = *pos as usize;
    if next < list.len() && count > i32::MAX as usize {
        return Err(Errno::EINVAL);
    }
    // Where each entry ends in `out`
    let mut ends = Vec::new();
    while let Some(e) = list.get(next) {
        // struct linux_dirent64: d_ino, d_off, d_reclen, d_type, d_name, aligned to 8 bytes
        let reclen = (19 + e.name.len() + 1).next_multiple_of(8);
        if out.len() + reclen > count {
            break;
        }
        let start = out.len();
        out.extend_from_slice(&e.ino.to_le_bytes());
        out.extend_from_slice(&(next as u64 + 1).to_le_bytes());
        out.extend_from_slice(&(reclen as u16).to_le_bytes());
        out.push((e.mode >> 12) as u8 & 0xf); // DT_* is just S_IFMT >> 12
        out.extend_from_slice(&e.name);
        out.resize(start + reclen, 0);
        ends.push(out.len());
        next += 1;
    }
    if out.is_empty() && next < list.len() {
        return Err(Errno::EINVAL);
    }
    let written = cx.write_user_partial(buf, &out);
    if written < out.len() {
        let whole = ends.iter().take_while(|&&end| end <= written).count();
        if whole == 0 {
            return Err(Errno::EFAULT);
        }
        next -= ends.len() - whole;
        out.truncate(ends[whole - 1]);
    }
    *pos = next as u64;
    inotify::file_event(cx.kr(), &f, inotify::IN_ACCESS);
    Ok(out.len() as u64)
}

pub(super) fn lseek(cx: &mut Cx<'_>, fd: i32, offset: i64, whence: u32) -> SysResult {
    let file = cx.fds().get(fd)?;
    if matches!(file.borrow().kind, FileKind::Fuse(_)) {
        return super::fuse::lseek(cx, &file, offset, whence);
    }
    follow_copy_up(cx, &file)?;
    let mut f = file.borrow_mut();
    let OpenFile { kind, pos, .. } = &mut *f;
    let base = match kind {
        FileKind::Host(fd) => return tuxport_hal::lseek(*fd, offset, whence as i32).map_err(Errno),
        FileKind::Path(_) => return Err(Errno::EBADF),
        FileKind::Pipe(_)
        | FileKind::Pty(_)
        | FileKind::Socket(_)
        | FileKind::Epoll(_)
        | FileKind::EventFd(_)
        | FileKind::TimerFd(_)
        | FileKind::SignalFd(_)
        | FileKind::Inotify(_)
        | FileKind::Pidfd(_)
        | FileKind::SeccompNotify(_)
        | FileKind::FuseDev(_)
        | FileKind::Bpf(_) => return Err(Errno::ESPIPE),
        FileKind::Fuse(_) => return Err(Errno::EBADF),
        FileKind::Proc { data, .. } => match whence {
            SEEK_SET => 0,
            SEEK_CUR => *pos as i64,
            SEEK_END => data.len() as i64,
            _ => return Err(Errno::EINVAL),
        },
        FileKind::MessageQueue(q) => match whence {
            SEEK_SET => 0,
            SEEK_CUR => *pos as i64,
            SEEK_END => q.borrow().status().len() as i64,
            _ => return Err(Errno::EINVAL),
        },
        // /dev/kmsg seeks by record (`devkmsg_llseek`): the offset can only be 0; SEEK_SET goes
        // to the oldest record, SEEK_DATA to the first one after syslog last cleared (after
        // `dmesg -c`), SEEK_END to the next one; the file position is unchanged and 0 is returned
        FileKind::Device {
            kmsg: Some(user), ..
        } => {
            if offset != 0 {
                return Err(Errno::ESPIPE);
            }
            let log = user.log.borrow();
            user.seq = match whence {
                SEEK_SET => log.first_seq(),
                SEEK_DATA => log.clear_seq(),
                SEEK_END => log.next_seq(),
                _ => return Err(Errno::EINVAL),
            };
            return Ok(0);
        }
        FileKind::Device {
            block: Some(open), ..
        } => return super::loopdev::seek(&open.dev, pos, offset, whence),
        // As with Linux's /dev/null and the like, seek always succeeds and stays at 0
        FileKind::Device { .. } => return Ok(0),
        FileKind::Directory { entries, node, .. } => {
            // Where SEEK_END and SEEK_DATA/SEEK_HOLE count from: store directories stand in for
            // ext4's, whose hashed directories end at the largest position (ext4_dir_llseek);
            // procfs, sysfs and cgroup2 ones are of size 0 (generic_file_llseek); tmpfs, devpts
            // and mqueue directories take only SEEK_SET and SEEK_CUR (dcache_dir_lseek)
            let eof = match cx.vfs()?.fs_of(node) {
                Fs::Store => Some(i64::MAX),
                Fs::Proc | Fs::Sys | Fs::Cgroup => Some(0),
                _ => None,
            };
            let new = match (whence, eof) {
                (SEEK_SET, _) => offset,
                (SEEK_CUR, _) => (*pos as i64).checked_add(offset).ok_or(Errno::EINVAL)?,
                (SEEK_END, Some(eof)) => eof.checked_add(offset).ok_or(Errno::EINVAL)?,
                (SEEK_DATA | SEEK_HOLE, Some(eof)) if offset as u64 >= eof as u64 => {
                    return Err(Errno::ENXIO);
                }
                (SEEK_DATA, Some(_)) => offset,
                (SEEK_HOLE, Some(eof)) => eof,
                _ => return Err(Errno::EINVAL),
            };
            if new < 0 {
                return Err(Errno::EINVAL);
            }
            if new == 0 {
                *entries = None;
            }
            *pos = new as u64;
            return Ok(new as u64);
        }
        FileKind::Regular { file, .. } => {
            let size = file.metadata().map_err(host_errno)?.len() as i64;
            match whence {
                SEEK_SET => 0,
                SEEK_CUR => *pos as i64,
                SEEK_END => size,
                SEEK_DATA | SEEK_HOLE if offset < 0 || offset >= size => {
                    return Err(Errno::ENXIO);
                }
                // Store objects and tmpfs files are host files, whose holes the host knows; a
                // host filesystem that cannot tell has data everywhere (a hole only at the end)
                SEEK_DATA | SEEK_HOLE => {
                    let fd = std::os::fd::AsRawFd::as_raw_fd(file);
                    let found = match tuxport_hal::lseek(fd, offset, whence as i32) {
                        Ok(at) => at,
                        Err(e) if e == Errno::ENXIO.0 => return Err(Errno::ENXIO),
                        Err(_) if whence == SEEK_DATA => offset as u64,
                        Err(_) => size as u64,
                    };
                    *pos = found;
                    return Ok(found);
                }
                _ => return Err(Errno::EINVAL),
            }
        }
    };
    let new = base
        .checked_add(offset)
        .filter(|&v| v >= 0)
        .ok_or(Errno::EINVAL)?;
    *pos = new as u64;
    Ok(new as u64)
}

// ---- Cache hints and syncing ----

/// posix_fadvise: affects only performance; ignored after the arguments are checked.
pub(super) fn fadvise64(cx: &mut Cx<'_>, fd: i32, len: i64, advice: i32) -> SysResult {
    let file = cx.fds().get(fd)?;
    if matches!(file.borrow().kind, FileKind::Pipe(_) | FileKind::Socket(_)) {
        return Err(Errno::ESPIPE);
    }
    if len < 0 || !(0..=5).contains(&advice) {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

/// sync_file_range, readahead, syncfs: mostly argument checks (data is written straight into the
/// host file, and reads are cached by the host too). sync_file_range with flags on a regular file
/// is passed to the host object with the BKL released; syncfs's flush is [`sync`], called after
/// this. `kind` tells the three apart: 0 is sync_file_range, 1 readahead, 2 syncfs.
pub(super) fn cache_hint(
    cx: &mut Cx<'_>,
    fd: i32,
    offset: i64,
    len: i64,
    flags: u32,
    kind: u32,
) -> SysResult {
    let file = cx.fds().get(fd)?;
    let f = file.borrow();
    match kind {
        0 => {
            if flags & !7 != 0 || offset < 0 || len < 0 || offset.checked_add(len).is_none() {
                return Err(Errno::EINVAL);
            }
            match &f.kind {
                // Handed to the host object: tools like dpkg first start writeback for a batch of
                // files and then fsync them one by one, merged into one journal commit. The open
                // file is kept alive by file and not touched while the BKL is released
                FileKind::Regular { file: host, .. } if flags != 0 => {
                    let fd = std::os::fd::AsRawFd::as_raw_fd(host);
                    drop(f);
                    cx.unlocked(|| tuxport_hal::sync_file_range(fd, offset, len, flags))
                        .map(|()| 0)
                        .map_err(Errno)
                }
                FileKind::Regular { .. } | FileKind::Directory { .. } => Ok(0),
                FileKind::Path(_) => Err(Errno::EBADF),
                _ => Err(Errno::ESPIPE),
            }
        }
        1 => match &f.kind {
            _ if !f.readable() => Err(Errno::EBADF),
            FileKind::Regular { .. } if offset >= 0 => Ok(0),
            _ => Err(Errno::EINVAL),
        },
        _ => Ok(0),
    }
}

/// cachestat (Linux 6.5): the state of a file in the page cache. Files in the store look at the
/// host file's page cache ([`tuxport_hal::cachestat`], only the number of pages in the cache);
/// other files have no page cache and get all 0s.
pub(super) fn cachestat(cx: &mut Cx<'_>, fd: i32, range: u64, out: u64, flags: u32) -> SysResult {
    let file = cx.fds().get(fd)?;
    if matches!(file.borrow().kind, FileKind::Path(_)) {
        return Err(Errno::EBADF);
    }
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let r: [u8; 16] = get(cx, range)?;
    let off = u64::from_le_bytes(r[..8].try_into().unwrap());
    let len = u64::from_le_bytes(r[8..].try_into().unwrap());
    let stats = match &file.borrow().kind {
        FileKind::Regular { file, .. } => {
            tuxport_hal::cachestat(std::os::fd::AsRawFd::as_raw_fd(file), off, len).map_err(Errno)?
        }
        _ => [0; 5],
    };
    let mut b = [0u8; 40];
    for (i, n) in stats.iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&n.to_le_bytes());
    }
    put(cx, out, b).map(|()| 0)
}

/// fsync(2), fdatasync(2) (`data_only`): a regular file's data is in its host object, so the host
/// is asked to flush it to disk; metadata (names, permissions...) is in the store's database, so
/// the deferred write transaction is committed and then the WAL is awaited on disk
/// ([`durable`]). The BKL is released while flushing.
pub(super) fn fsync(cx: &mut Cx<'_>, fd: i32, data_only: bool) -> SysResult {
    let mut file = cx.fds().get(fd)?;
    if file.borrow().flags & oflags::O_PATH != 0 {
        return Err(Errno::EBADF);
    }
    if !has_fsync(&file.borrow()) {
        return Err(Errno::EINVAL);
    }
    if let Some((io, dir)) = super::fuse::io_of(&file) {
        return super::fuse::fsync(cx, &io, dir, data_only).map(|()| 0);
    }
    // A loop device flushes by syncing its backing file, which may be another loop device
    loop {
        let next = match &file.borrow().kind {
            FileKind::Device {
                block: Some(open), ..
            } => match super::loopdev::flush_target(&open.dev) {
                Some(backing) => backing,
                None => return Ok(0),
            },
            _ => break,
        };
        file = next;
    }
    let host = {
        let f = file.borrow();
        match &f.kind {
            // A regular file syncs its host object; a directory has only the store's metadata
            // (host fds are left to the host)
            FileKind::Regular { file, .. } => Some(std::os::fd::AsRawFd::as_raw_fd(file)),
            FileKind::Host(_) => return Ok(0),
            _ => None,
        }
    };
    // The open file is kept alive by file and not touched while the BKL is released
    if let Some(fd) = host {
        cx.unlocked(|| tuxport_hal::fsync(fd, data_only))
            .map_err(Errno)?;
    }
    durable(cx, false)
}

/// Whether a file can be synced, which on Linux is having an fsync operation: regular files,
/// directories, loop devices and host fds (left to the host). For anything else fsync is EINVAL
/// (vfs_fsync_range), and so is submitting an AIO fsync of it (aio_fsync).
pub(super) fn has_fsync(f: &OpenFile) -> bool {
    matches!(
        f.kind,
        FileKind::Regular { .. }
            | FileKind::Directory { .. }
            | FileKind::Host(_)
            | FileKind::Fuse(_)
            | FileKind::Device { block: Some(_), .. }
    )
}

/// sync(2): commits the deferred write transaction and writes the whole host filesystem holding
/// the store to disk.
pub(super) fn sync(cx: &mut Cx<'_>) -> SysResult {
    durable(cx, true)
}

/// Commits the store's deferred write transaction and waits for it to reach disk
/// ([`Store::commit`] only writes into the WAL): with `all`, syncfs the whole host filesystem
/// together with the data of all objects, otherwise wait only for the WAL. Waits with the BKL
/// released.
fn durable(cx: &mut Cx<'_>, all: bool) -> SysResult {
    let Ok(vfs) = cx.vfs() else {
        return Ok(0);
    };
    let store = vfs.store();
    store.commit()?;
    let meta = store.meta().clone();
    drop(vfs);
    cx.unlocked(move || {
        if all {
            meta.sync_all()
        } else {
            meta.sync_metadata()
        }
    })?;
    Ok(0)
}

// ---- Metadata ----

pub(super) fn faccessat(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    amode: u32,
    flags: u32,
) -> SysResult {
    if amode & !7 != 0 || flags & !(AT_SYMLINK_NOFOLLOW | AT_EACCESS | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    // By default checks use the real user ID (including search permission along the path), and
    // with AT_EACCESS the effective user ID
    let cred = if flags & AT_EACCESS != 0 {
        cx.cred()
    } else {
        cx.cred().for_access()
    };
    let node = lookup_at_or_fd(
        cx,
        &cred,
        dirfd,
        &path,
        flags,
        flags & AT_SYMLINK_NOFOLLOW == 0,
    )?;
    if let Node::Fuse(_, f) = node {
        let vfs = cx.vfs()?;
        super::fuse::access(cx, &vfs, &cred, f, amode)?;
        let kind = vfs.kind(&cx.view(&vfs), &node)?;
        if amode & MAY_WRITE != 0
            && matches!(kind, mode::S_IFREG | mode::S_IFDIR | mode::S_IFLNK)
            && vfs.readonly(&node)
        {
            return Err(Errno::EROFS);
        }
        return Ok(0);
    }
    if amode == 0 {
        return Ok(0);
    }
    let b = node_stat(cx, &node)?;
    let word = |off: usize| u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
    let st = Stat {
        mode: word(16),
        uid: word(24),
        gid: word(28),
        ..Stat::default()
    };
    // R_OK, W_OK, X_OK have the same values as MAY_READ, MAY_WRITE, MAY_EXEC
    cred.permission(&st, amode)?;
    // Asking whether a regular file, directory or symlink in a read-only mount can be written is
    // EROFS (after the permission check)
    let special = !matches!(
        st.mode & mode::S_IFMT,
        mode::S_IFREG | mode::S_IFDIR | mode::S_IFLNK
    );
    if amode & MAY_WRITE != 0 && !special && cx.vfs()?.readonly(&node) {
        return Err(Errno::EROFS);
    }
    Ok(0)
}

pub(super) fn chdir(cx: &mut Cx<'_>, path: u64) -> SysResult {
    let path = read_path(cx, path)?;
    let node = lookup_at(cx, AT_FDCWD, &path, true)?;
    set_cwd(cx, node)
}

pub(super) fn fchdir(cx: &mut Cx<'_>, fd: i32) -> SysResult {
    let node = cx.fds().get(fd)?.borrow().node().ok_or(Errno::ENOTDIR)?;
    set_cwd(cx, node)
}

/// chdir, fchdir: the target must be a searchable directory.
fn set_cwd(cx: &mut Cx<'_>, node: Node) -> SysResult {
    let vfs = cx.vfs()?;
    if !vfs.is_dir(&cx.view(&vfs), &node)? {
        return Err(Errno::ENOTDIR);
    }
    if let Some(st) = searched_attrs(&vfs, &cx.view(&vfs), &node)? {
        cx.cred().permission(&st, MAY_EXEC)?;
    }
    if let Node::Fuse(_, f) = node {
        let cred = cx.cred();
        super::fuse::may_chdir(cx, &vfs, &cred, f)?;
    }
    cx.t.fs.borrow_mut().set_cwd(node);
    cx.shared.fast_changed();
    Ok(0)
}

/// The attributes chdir and chroot check search permission on: a store directory's, or those of
/// the store directory behind an overlay one; for synthetic directories what their view says
/// ([`Pseudo::searched`]: cgroup directories; the others are always searchable).
fn searched_attrs(vfs: &Vfs, view: &dyn Pseudo, node: &Node) -> Result<Option<Stat>, Errno> {
    Ok(match node {
        Node::Store(_, ino) => Some(vfs.store().attrs(*ino)?),
        Node::Overlay(_, o) => Some(vfs.store().attrs(vfs.overlay().node(*o).real())?),
        Node::Pseudo(_, p) => view.searched(p),
        // Checked by the daemon or on its attributes ([`super::fuse::may_chdir`])
        Node::Fuse(..) => None,
    })
}

/// chroot: the target must be a searchable directory; requires `CAP_SYS_CHROOT`. The current
/// directory is unchanged (it may be outside the new root, as on Linux).
pub(super) fn chroot(cx: &mut Cx<'_>, path: u64) -> SysResult {
    let path = read_path(cx, path)?;
    let node = lookup_at(cx, AT_FDCWD, &path, true)?;
    let vfs = cx.vfs()?;
    if !vfs.is_dir(&cx.view(&vfs), &node)? {
        return Err(Errno::ENOTDIR);
    }
    let cred = cx.cred();
    if let Some(st) = searched_attrs(&vfs, &cx.view(&vfs), &node)? {
        cred.permission(&st, MAY_EXEC)?;
    }
    if let Node::Fuse(_, f) = node {
        super::fuse::may_chdir(cx, &vfs, &cred, f)?;
    }
    if !cred.capable_here(cap::SYS_CHROOT) {
        return Err(Errno::EPERM);
    }
    cx.t.fs.borrow_mut().set_root(node);
    cx.shared.fast_changed();
    Ok(0)
}

pub(super) fn getcwd(cx: &mut Cx<'_>, buf: u64, size: u64) -> SysResult {
    let vfs = cx.vfs()?;
    let (path, reached) = vfs.reach(&cx.view(&vfs), &cx.root(), &cx.cwd())?;
    // As on Linux, when the current directory is not under the root the path starts with
    // (unreachable) (musl and glibc treat it as ENOENT)
    let mut path = if reached {
        path
    } else {
        [&b"(unreachable)"[..], &path].concat()
    };
    path.push(0);
    if (size as usize) < path.len() {
        return Err(Errno::ERANGE);
    }
    cx.mm().write(UserSlice::new(buf, path.len()), &path)?;
    Ok(path.len() as u64)
}

pub(super) fn readlinkat(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    buf: u64,
    size: usize,
) -> SysResult {
    if size as isize <= 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    // An empty path reads dirfd itself (a symlink opened with O_PATH | O_NOFOLLOW), and
    // `AT_FDCWD` is the current directory; when it is not a symlink it is ENOENT (Linux's
    // do_readlinkat: `empty ? -ENOENT : -EINVAL`)
    let empty = path.is_empty();
    let node = if !empty {
        lookup_at(cx, dirfd, &path, false)?
    } else if dirfd == AT_FDCWD {
        cx.cwd()
    } else {
        cx.fds().get(dirfd)?.borrow().node().ok_or(Errno::ENOENT)?
    };
    let vfs = cx.vfs()?;
    let target = match vfs.readlink(&cx.view(&vfs), &node) {
        Err(e) if empty && e == Errno::EINVAL => return Err(Errno::ENOENT),
        r => r?,
    };
    let n = target.len().min(size);
    cx.mm().write(UserSlice::new(buf, n), &target[..n])?;
    Ok(n as u64)
}

pub(super) fn newfstatat(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    buf: u64,
    flags: u32,
) -> SysResult {
    newfstatat_flags(flags)?;
    let path = read_path(cx, path)?;
    let st = stat_at(cx, dirfd, &path, flags)?;
    put(cx, buf, st).map(|()| 0)
}

/// The `flags` check of newfstatat.
pub(super) fn newfstatat_flags(flags: u32) -> Result<(), Errno> {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

pub(super) fn statx(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    flags: u32,
    mask: u32,
    buf: u64,
) -> SysResult {
    let flags = statx_flags(flags, mask)?;
    let path = read_path(cx, path)?;
    let st = stat_at(cx, dirfd, &path, flags)?;
    put(cx, buf, encode_statx(&st)).map(|()| 0)
}

/// The `flags` and `mask` checks of statx; returns the flags for [`stat_at`].
pub(super) fn statx_flags(flags: u32, mask: u32) -> Result<u32, Errno> {
    const AT_NO_AUTOMOUNT: u32 = 0x800;
    const AT_STATX_SYNC_TYPE: u32 = 0x6000;
    const STATX_RESERVED: u32 = 0x8000_0000;
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH | AT_NO_AUTOMOUNT | AT_STATX_SYNC_TYPE) != 0
        || flags & AT_STATX_SYNC_TYPE == AT_STATX_SYNC_TYPE
        || mask & STATX_RESERVED != 0
    {
        return Err(Errno::EINVAL);
    }
    Ok(flags & (AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH))
}

/// Builds a `struct statx` from a `struct stat`: always gives all the basic fields
/// (`STATX_BASIC_STATS`), with no creation time.
pub(super) fn encode_statx(st: &[u8; 128]) -> [u8; 256] {
    const STATX_BASIC_STATS: u32 = 0x7ff;
    let u32_at = |o: usize| u32::from_le_bytes(st[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(st[o..o + 8].try_into().unwrap());
    // glibc's and musl's major()/minor()
    let major = |d: u64| (((d >> 8) & 0xfff) | ((d >> 32) & !0xfff)) as u32;
    let minor = |d: u64| ((d & 0xff) | ((d >> 12) & !0xff)) as u32;
    let mut b = [0u8; 256];
    let mut put = |off: usize, bytes: &[u8]| b[off..off + bytes.len()].copy_from_slice(bytes);
    put(0, &STATX_BASIC_STATS.to_le_bytes());
    put(4, &u32_at(56).to_le_bytes()); // blksize
    put(16, &u32_at(20).to_le_bytes()); // nlink
    put(20, &u32_at(24).to_le_bytes()); // uid
    put(24, &u32_at(28).to_le_bytes()); // gid
    put(28, &(u32_at(16) as u16).to_le_bytes()); // mode
    put(32, &u64_at(8).to_le_bytes()); // ino
    put(40, &u64_at(48).to_le_bytes()); // size
    put(48, &u64_at(64).to_le_bytes()); // blocks
    // atime, ctime, mtime: seconds and nanoseconds (statx_timestamp is i64 + u32 + reserved)
    for (dst, src) in [(64, 72), (96, 104), (112, 88)] {
        put(dst, &u64_at(src).to_le_bytes());
        put(dst + 8, &(u64_at(src + 8) as u32).to_le_bytes());
    }
    let (rdev, dev) = (u64_at(32), u64_at(0));
    put(128, &major(rdev).to_le_bytes());
    put(132, &minor(rdev).to_le_bytes());
    put(136, &major(dev).to_le_bytes());
    put(140, &minor(dev).to_le_bytes());
    b
}

/// Shared by newfstatat and statx: finds the object `*at`-style and returns its `struct stat`.
fn stat_at(cx: &Cx<'_>, dirfd: i32, path: &[u8], flags: u32) -> Result<[u8; 128], Errno> {
    stat_at_kernel(cx, dirfd, path, flags).map(|st| owner_out(cx, st))
}

/// [`stat_at`], with the owner and group as kernel IDs.
fn stat_at_kernel(cx: &Cx<'_>, dirfd: i32, path: &[u8], flags: u32) -> Result<[u8; 128], Errno> {
    if path.is_empty() && flags & AT_EMPTY_PATH != 0 && dirfd != AT_FDCWD {
        file_stat(cx, &cx.fds().get(dirfd)?)
    } else {
        // Resolution and fetching the attributes share one read transaction (which really starts
        // only on a cache miss)
        let vfs = cx.vfs()?;
        vfs.store().snapshot(|_| {
            let cred = cx.cred();
            let node = lookup_at_or_fd(
                cx,
                &cred,
                dirfd,
                path,
                flags,
                flags & AT_SYMLINK_NOFOLLOW == 0,
            )?;
            node_stat(cx, &node)
        })
    }
}

pub(super) fn fstat(cx: &mut Cx<'_>, fd: i32, buf: u64) -> SysResult {
    let st = owner_out(cx, file_stat(cx, &cx.fds().get(fd)?)?);
    put(cx, buf, st).map(|()| 0)
}

/// Turns the owner and group in a stat result into those in the caller's user namespace (65534
/// when they do not map).
fn owner_out(cx: &Cx<'_>, mut st: [u8; 128]) -> [u8; 128] {
    let cred = cx.cred();
    if cred.user_ns.level > 0 {
        let word =
            |b: &[u8; 128], off: usize| u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
        let (uid, gid) = (cred.uid_out(word(&st, 24)), cred.gid_out(word(&st, 28)));
        st[24..28].copy_from_slice(&uid.to_le_bytes());
        st[28..32].copy_from_slice(&gid.to_le_bytes());
    }
    st
}

pub(super) fn statfs(cx: &mut Cx<'_>, path: u64, buf: u64) -> SysResult {
    let path = read_path(cx, path)?;
    let node = lookup_at(cx, AT_FDCWD, &path, true)?;
    let b = statfs_of(cx, &node)?;
    put(cx, buf, b).map(|()| 0)
}

pub(super) fn fstatfs(cx: &mut Cx<'_>, fd: i32, buf: u64) -> SysResult {
    let file = cx.fds().get(fd)?;
    let (pipe, node) = {
        let f = file.borrow();
        (matches!(f.kind, FileKind::Pipe(_)), f.node())
    };
    // Message queue fds are in the mqueue filesystem
    if matches!(file.borrow().kind, FileKind::MessageQueue(_)) {
        let empty = tuxport_hal::FsStats {
            block_size: 4096,
            ..Default::default()
        };
        return put(cx, buf, encode_statfs(MQUEUE_MAGIC, &empty, 0)).map(|()| 0);
    }
    let b = match (pipe, node) {
        (true, _) => encode_statfs(
            PIPEFS_MAGIC,
            &tuxport_hal::FsStats {
                block_size: 4096,
                ..Default::default()
            },
            0,
        ),
        (false, Some(node)) => statfs_of(cx, &node)?,
        (false, None) => statfs_of(cx, &Node::ROOT)?,
    };
    put(cx, buf, b).map(|()| 0)
}

/// statfs of the filesystem a node is in.
fn statfs_of(cx: &mut Cx<'_>, node: &Node) -> Result<[u8; 120], Errno> {
    let empty = tuxport_hal::FsStats {
        block_size: 4096,
        ..Default::default()
    };
    let vfs = cx.vfs()?;
    // The mount's flags plus the filesystem's read-only, sync and mand ones (Linux's flags_by_mnt
    // and flags_by_sb)
    let sb = vfs.mount(node.mnt()).map_or(0, |m| vfs.sb_flags(&m));
    let flags = vfs.mount_flags(node).unwrap_or(0)
        | if vfs.readonly(node) { ms::RDONLY } else { 0 }
        | sb & (ms::SYNCHRONOUS | ms::MANDLOCK);
    // tmpfs reports by pages and inodes (Linux's shmem_statfs): an unlimited item is all 0s;
    // usage is counted afresh
    if let Some(dev) = vfs.tmpfs_dev(node) {
        let (p, pages, inodes) = vfs.tmpfs_usage(dev, true)?;
        let free = p.blocks.saturating_sub(pages);
        let s = tuxport_hal::FsStats {
            block_size: TMPFS_PAGE,
            blocks: p.blocks,
            free,
            available: free,
            files: p.inodes,
            files_free: p.inodes.saturating_sub(inodes),
        };
        return Ok(encode_statfs(TMPFS_MAGIC, &s, flags));
    }
    if let Node::Fuse(_, f) = node {
        let words = super::fuse::statfs(cx, &vfs, *f, statfs_flags(flags))?;
        let mut b = [0u8; 120];
        for (i, w) in words.iter().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        return Ok(b);
    }
    Ok(match node {
        Node::Store(..) => encode_statfs(EXT4_SUPER_MAGIC, &vfs.store().fs_stats()?, flags),
        // The capacity of the filesystem the layers are in (Linux's ovl_statfs asks the upper
        // layer's, or the topmost lower one's)
        Node::Overlay(..) => encode_statfs(OVERLAYFS_SUPER_MAGIC, &vfs.store().fs_stats()?, flags),
        Node::Pseudo(_, PseudoNode::NsFile(..)) => encode_statfs(NSFS_MAGIC, &empty, 0),
        Node::Pseudo(_, p) => {
            let magic = match pseudo::fs_of(p) {
                Fs::Store => EXT4_SUPER_MAGIC,
                Fs::Tmpfs => TMPFS_MAGIC,
                Fs::Proc => PROC_SUPER_MAGIC,
                Fs::Pts => DEVPTS_SUPER_MAGIC,
                Fs::Sys => SYSFS_MAGIC,
                Fs::Cgroup => CGROUP2_SUPER_MAGIC,
                Fs::Nsfs => NSFS_MAGIC,
                Fs::Mqueue => MQUEUE_MAGIC,
                Fs::Overlay => OVERLAYFS_SUPER_MAGIC,
                Fs::Fuse => crate::vfs::fuse::SUPER_MAGIC,
            };
            encode_statfs(magic, &empty, flags)
        }
        Node::Fuse(..) => unreachable!("handled above"),
    })
}

fn encode_statfs(magic: u64, s: &tuxport_hal::FsStats, flags: u64) -> [u8; 120] {
    // asm-generic's struct statfs (64-bit)
    let mut b = [0u8; 120];
    let words = [
        magic,
        s.block_size,
        s.blocks,
        s.free,
        s.available,
        s.files,
        s.files_free,
        0, // f_fsid
        255,
        s.block_size,
        statfs_flags(flags),
    ];
    for (i, w) in words.iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    b
}

/// statfs's `f_flags` (Linux's `calculate_f_flags`): the mount's flags plus `ST_VALID`.
fn statfs_flags(flags: u64) -> u64 {
    const ST_VALID: u64 = 0x20;
    const ST_RELATIME: u64 = 0x1000;
    const ST_NOSYMFOLLOW: u64 = 0x2000;
    // RDONLY, NOSUID, NODEV, NOEXEC, SYNCHRONOUS, MANDLOCK, NOATIME, NODIRATIME have the same
    // values as MS_*
    let same = ms::RDONLY
        | ms::NOSUID
        | ms::NODEV
        | ms::NOEXEC
        | ms::SYNCHRONOUS
        | ms::MANDLOCK
        | ms::NOATIME
        | ms::NODIRATIME;
    let mut f = ST_VALID | (flags & same);
    if flags & ms::RELATIME != 0 {
        f |= ST_RELATIME;
    }
    if flags & ms::NOSYMFOLLOW != 0 {
        f |= ST_NOSYMFOLLOW;
    }
    f
}

// ---- stat ----

/// Metadata of a filesystem node.
fn node_stat(cx: &Cx<'_>, node: &Node) -> Result<[u8; 128], Errno> {
    let vfs = cx.vfs()?;
    let view = cx.view(&vfs);
    match node {
        Node::Store(_, ino) => Ok(encode_stat(&vfs.store().stat(*ino)?, vfs.dev_of(node))),
        Node::Overlay(_, o) => Ok(encode_stat(&vfs.overlay_stat(*o)?, vfs.dev_of(node))),
        Node::Fuse(..) => super::fuse::stat(&vfs, &cx.cred(), node),
        // Namespaces are in nsfs
        Node::Pseudo(_, p @ PseudoNode::NsFile(..)) => {
            Ok(encode_stat(&pseudo::stat(&view, p)?, Fs::Nsfs.dev()))
        }
        // The open file a magic link points at: report that file itself
        Node::Pseudo(_, PseudoNode::OpenFd(pid, fd)) => {
            let file = view.open_file(*pid, *fd).ok_or(Errno::ENOENT)?;
            file_stat(cx, &file)
        }
        // A deleted program file
        Node::Pseudo(_, PseudoNode::OpenExe(pid)) => {
            let exe = exe_file(cx, *pid)?;
            let (Some((mnt, ino)), Some(host)) = (exe.node, &exe.host) else {
                return Err(Errno::ENOENT);
            };
            let st = vfs.store().stat_open(ino, host)?;
            Ok(encode_stat(&st, vfs.dev_of(&Node::Store(mnt, ino))))
        }
        Node::Pseudo(_, p) => {
            let mut b = encode_stat(&pseudo::stat(&view, p)?, vfs.dev_of(node));
            // procfs and devpts have 1024-byte blocks (proc_fill_super, devpts_fill_super)
            if matches!(vfs.fs_of(node), Fs::Proc | Fs::Pts) {
                b[56..60].copy_from_slice(&1024u32.to_le_bytes());
            }
            Ok(b)
        }
    }
}

pub(super) fn file_stat(cx: &Cx<'_>, file: &FileRef) -> Result<[u8; 128], Errno> {
    follow_copy_up(cx, file)?;
    let f = file.borrow();
    let anon = |ino: u64, mode: u32, rdev: u64, dev: u64| {
        let st = Stat {
            ino,
            mode,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev,
            size: 0,
            atime_ns: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            blocks: None,
        };
        encode_stat(&st, dev)
    };
    match &f.kind {
        FileKind::Host(fd) => host_stat(*fd),
        FileKind::Pipe(end) => match end.pipe.borrow().fifo {
            Some(ino) => {
                let vfs = cx.vfs()?;
                let dev = vfs.dev_of(&Node::Store(f.mnt, ino));
                Ok(encode_stat(&vfs.store().stat(ino)?, dev))
            }
            None => Ok(anon(
                end.pipe.borrow().ino,
                mode::S_IFIFO | 0o600,
                0,
                PIPE_DEV,
            )),
        },
        FileKind::Pty(end) if end.master => Ok(anon(2, mode::S_IFCHR | 0o666, PTMX_RDEV, ROOT_DEV)),
        FileKind::Socket(s) => Ok(anon(
            pseudo::socket_ino(s),
            mode::S_IFSOCK | 0o777,
            0,
            SOCK_DEV,
        )),
        // anon_inode: no file type bits
        FileKind::Epoll(_)
        | FileKind::EventFd(_)
        | FileKind::TimerFd(_)
        | FileKind::SignalFd(_)
        | FileKind::Inotify(_)
        | FileKind::Pidfd(_)
        | FileKind::SeccompNotify(_)
        | FileKind::Bpf(_) => Ok(anon(1, 0o600, 0, ANON_DEV)),
        // The device number is that of the mqueue filesystem of the queue's IPC namespace
        FileKind::MessageQueue(q) => {
            let q = q.borrow();
            Ok(encode_stat(&q.stat(), cx.vfs()?.mq_dev(q.ns)))
        }
        FileKind::Pty(end) => {
            let t = end.tty.borrow();
            let st = Stat {
                ino: pseudo::ino_of(&PseudoNode::Pts(t.fs, t.index)),
                mode: mode::S_IFCHR | t.mode,
                nlink: 1,
                uid: t.owner.0,
                gid: t.owner.1,
                rdev: rdev(PTS_MAJOR, t.index),
                atime_ns: t.atime_ns,
                mtime_ns: t.mtime_ns,
                ctime_ns: t.ctime_ns,
                ..Stat::default()
            };
            // The device number is that of the devpts instance the terminal is in (ttyname
            // compares it with that of /dev/pts/<n>)
            Ok(encode_stat(&st, t.fs))
        }
        FileKind::Regular { ino, file, .. } => {
            let vfs = cx.vfs()?;
            let dev = vfs.dev_of(&Node::Store(f.mnt, *ino));
            let mut st = vfs.store().stat_open(*ino, file)?;
            // Through an overlay: the inode number stat shows for its node
            if let Some(o) = f.ovl {
                st.ino = vfs.overlay().shown_ino(o);
            }
            Ok(encode_stat(&st, dev))
        }
        _ => {
            let node = f.node().ok_or(Errno::EBADF)?;
            drop(f);
            node_stat(cx, &node)
        }
    }
}

/// Metadata of a host fd.
// The st_* fields have different widths on different hosts
#[allow(clippy::unnecessary_cast)]
fn host_stat(fd: i32) -> Result<[u8; 128], Errno> {
    let s = tuxport_hal::fstat(fd).map_err(Errno)?;
    let st = Stat {
        ino: s.st_ino as u64,
        mode: s.st_mode as u32,
        nlink: s.st_nlink as u32,
        uid: s.st_uid,
        gid: s.st_gid,
        rdev: s.st_rdev as u64,
        size: s.st_size as u64,
        atime_ns: s.st_atime as i64 * 1_000_000_000 + s.st_atime_nsec as i64,
        mtime_ns: s.st_mtime as i64 * 1_000_000_000 + s.st_mtime_nsec as i64,
        ctime_ns: s.st_ctime as i64 * 1_000_000_000 + s.st_ctime_nsec as i64,
        blocks: Some(s.st_blocks as u64),
    };
    Ok(encode_stat(&st, s.st_dev as u64))
}

/// Encodes as asm-generic's `struct stat` (128 bytes).
pub(super) fn encode_stat(st: &Stat, dev: u64) -> [u8; 128] {
    let mut b = [0u8; 128];
    let mut put = |off: usize, bytes: &[u8]| b[off..off + bytes.len()].copy_from_slice(bytes);
    let (asec, ansec) = split_ns(st.atime_ns);
    let (msec, mnsec) = split_ns(st.mtime_ns);
    let (csec, cnsec) = split_ns(st.ctime_ns);
    put(0, &dev.to_le_bytes());
    put(8, &st.ino.to_le_bytes());
    put(16, &st.mode.to_le_bytes());
    put(20, &st.nlink.to_le_bytes());
    put(24, &st.uid.to_le_bytes());
    put(28, &st.gid.to_le_bytes());
    put(32, &st.rdev.to_le_bytes());
    // Directories in the store have no data; report one block, following ext4's habit
    let size = if st.is_dir() && dev == ROOT_DEV {
        4096
    } else {
        st.size
    };
    put(48, &size.to_le_bytes());
    put(56, &4096u32.to_le_bytes());
    // sysfs files report a size of one page but take no blocks. Short symlinks live in the inode
    // and take no blocks: on ext4 targets shorter than 60 bytes (`ext4_symlink`; with the
    // trailing NUL they fit in `i_data`), on others such as tmpfs within 128 bytes
    // (`shmem_symlink`'s `SHORT_SYMLINK_LEN`)
    let inline = if dev == ROOT_DEV { 60 } else { 128 };
    let blocks = if let Some(blocks) = st.blocks {
        blocks
    } else if dev == Fs::Sys.dev() || (st.is_symlink() && size < inline) {
        0
    } else {
        size.div_ceil(4096) * 8
    };
    put(64, &blocks.to_le_bytes());
    put(72, &asec.to_le_bytes());
    put(80, &ansec.to_le_bytes());
    put(88, &msec.to_le_bytes());
    put(96, &mnsec.to_le_bytes());
    put(104, &csec.to_le_bytes());
    put(112, &cnsec.to_le_bytes());
    b
}

fn split_ns(ns: i64) -> (u64, u64) {
    (
        ns.div_euclid(1_000_000_000) as u64,
        ns.rem_euclid(1_000_000_000) as u64,
    )
}

/// The (st_dev, st_ino) fstat would report, the part known without querying the store (epoll's
/// fdinfo). Device numbers of synthetic nodes are those of the boot instances (procfs and devpts
/// instances mounted later have their own device numbers, [`Vfs::dev_of`]).
pub(crate) fn dev_ino(f: &OpenFile) -> (u64, u64) {
    match (&f.kind, f.node()) {
        (_, Some(Node::Store(_, ino))) => (ROOT_DEV, ino),
        (FileKind::Pipe(end), None) => (PIPE_DEV, end.pipe.borrow().ino),
        (FileKind::Socket(s), _) => (SOCK_DEV, pseudo::socket_ino(s)),
        (_, Some(Node::Pseudo(_, p))) => (pseudo::fs_of(&p).dev(), pseudo::ino_of(&p)),
        _ => (ANON_DEV, 1),
    }
}

// ---- Changing the namespace ----

pub(super) fn mkdirat(cx: &mut Cx<'_>, dirfd: i32, path: u64, perm: u32) -> SysResult {
    let path = read_path(cx, path)?;
    if let Some(r) = cgroup_mkdir(cx, dirfd, &path, perm) {
        return r.map(|()| 0);
    }
    if let Some((mnt, dir, name)) = super::fuse::parent(cx, dirfd, &path)? {
        return super::fuse::mkdir(cx, (mnt, dir), name, perm).map(|()| 0);
    }
    let (mnt, dir, name, ovl) = create_parent_at(cx, dirfd, &path)?;
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    let vfs = cx.vfs()?;
    let cred = cx.cred();
    let dir_st = may_create(&vfs, &cred, (mnt, dir), name)?;
    let (uid, gid, perm) = cred.new_owner(&dir_st, perm, cx.umask(), true);
    let tmpfs = tmpfs_reserve(&vfs, (mnt, dir), 0)?;
    create_in(
        &vfs,
        (mnt, dir),
        name,
        || vfs.store().mkdir(dir, name, perm, uid, gid),
        |&ino| ino,
    )?;
    tmpfs_created(&vfs, tmpfs, 0);
    created_events(cx.kr(), (dir, ovl), name, inotify::IN_ISDIR);
    Ok(0)
}

/// mkdir in a cgroup directory: creates a child cgroup ([`crate::cgroup::Cgroups::mkdir`]) with
/// the permission bits `perm` gives after the umask (vfs_mkdir's `S_IRWXUGO|S_ISVTX`, which kernfs
/// keeps). The order follows Linux: `.`, `..` and existing names are EEXIST, a read-only mount
/// EROFS, no write permission on the directory EACCES.
fn cgroup_mkdir(cx: &mut Cx<'_>, dirfd: i32, path: &[u8], perm: u32) -> Option<Result<(), Errno>> {
    let (dir, id, st, name) = cgroup_parent(cx, dirfd, path)?;
    Some((|| {
        let vfs = cx.vfs()?;
        if vfs.readonly(&dir) {
            return Err(Errno::EROFS);
        }
        let cgroups = &cx.kr().cgroups;
        let exists = cgroups
            .get(id)
            .is_some_and(|g| g.children.contains_key(name))
            || cgroups.files(id).any(|f| f == name);
        if name == b"." || name == b".." || exists {
            return Err(Errno::EEXIST);
        }
        let cred = cx.cred();
        cred.permission(&st, MAY_WRITE | MAY_EXEC)?;
        let mode = perm & 0o1777 & !cx.umask();
        cx.k().cgroups.mkdir(id, name, &cred, mode).map(|_| ())
    })())
}

pub(super) fn mknodat(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    mode_arg: u32,
    dev: u64,
) -> SysResult {
    let path = read_path(cx, path)?;
    let kind = mode_arg & mode::S_IFMT;
    if let Some((mnt, dir, name)) = super::fuse::parent(cx, dirfd, &path)? {
        match kind {
            0 | mode::S_IFREG | mode::S_IFCHR | mode::S_IFBLK | mode::S_IFIFO | mode::S_IFSOCK => {}
            mode::S_IFDIR => return Err(Errno::EPERM),
            _ => return Err(Errno::EINVAL),
        }
        return super::fuse::mknod(cx, (mnt, dir), name, mode_arg, dev).map(|()| 0);
    }
    let (mnt, dir, name, ovl) = create_parent_at(cx, dirfd, &path)?;
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    match kind {
        0 | mode::S_IFREG | mode::S_IFCHR | mode::S_IFBLK | mode::S_IFIFO | mode::S_IFSOCK => {}
        mode::S_IFDIR => return Err(Errno::EPERM),
        _ => return Err(Errno::EINVAL),
    }
    let vfs = cx.vfs()?;
    let cred = cx.cred();
    reject_trailing_slash(&vfs, (mnt, dir), name, &path)?;
    let dir_st = may_create(&vfs, &cred, (mnt, dir), name)?;
    // A whiteout (character device 0:0) needs no CAP_MKNOD and does not go through the device
    // filter (Linux's vfs_mknod)
    let whiteout = kind == mode::S_IFCHR && dev & 0xffff_ffff == 0;
    if matches!(kind, mode::S_IFCHR | mode::S_IFBLK) && !whiteout {
        if !cred.capable(cap::MKNOD) {
            return Err(Errno::EPERM);
        }
        super::bpf::check_device(cx, kind, dev & 0xffff_ffff, crate::bpf::ACC_MKNOD)?;
    }
    // An overlay does not make whiteouts for others (6.1's ovl_mknod)
    if whiteout && vfs.overlay_prefix(mnt).is_some() {
        return Err(Errno::EPERM);
    }
    let (uid, gid, perm) = cred.new_owner(&dir_st, mode_arg, cx.umask(), false);
    let tmpfs = tmpfs_reserve(&vfs, (mnt, dir), 0)?;
    let made = || {
        if kind == 0 || kind == mode::S_IFREG {
            vfs.store()
                .create_file(dir, name, perm, uid, gid)
                .map(|(ino, _)| ino)
        } else {
            vfs.store()
                .mknod(dir, name, kind | perm, dev & 0xffff_ffff, uid, gid)
        }
    };
    create_in(&vfs, (mnt, dir), name, made, |&ino| ino)?;
    tmpfs_created(&vfs, tmpfs, 0);
    created_events(cx.kr(), (dir, ovl), name, 0);
    Ok(0)
}

pub(super) fn unlinkat(cx: &mut Cx<'_>, dirfd: i32, path: u64, flags: u32) -> SysResult {
    if flags & !AT_REMOVEDIR != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    unlink(cx, dirfd, path, flags).map(|()| 0)
}

fn unlink(cx: &mut Cx<'_>, dirfd: i32, path: Vec<u8>, flags: u32) -> Result<(), Errno> {
    if let Some(r) = unlink_mq(cx, dirfd, &path, flags) {
        return r;
    }
    if let Some(r) = cgroup_unlink(cx, dirfd, &path, flags) {
        return r;
    }
    if let Some(r) = unlink_overlay(cx, dirfd, &path, flags) {
        return r;
    }
    if let Some((mnt, dir, name)) = super::fuse::parent(cx, dirfd, &path)? {
        let rmdir = flags & AT_REMOVEDIR != 0;
        let slash = path.ends_with(b"/");
        return super::fuse::unlink(cx, (mnt, dir), name, rmdir, slash);
    }
    let (mnt, dir, name) = parent_at(cx, dirfd, &path)?;
    let store_vfs = cx.vfs()?;
    let store = store_vfs.store();
    let cred = cx.cred();
    // With inotify watches, note the inode to delete first
    let victim = match name {
        b"." | b".." => None,
        _ if inotify::active(cx.kr()) => store.lookup(dir, name)?,
        _ => None,
    };
    if flags & AT_REMOVEDIR != 0 {
        match name {
            b"." => return Err(Errno::EINVAL),
            b".." => return Err(Errno::ENOTEMPTY),
            _ => {
                writable_dir(&store_vfs, (mnt, dir))?;
                may_delete(store, &cred, dir, name)?;
                let gone = not_mount_point(cx, &store_vfs, dir, name)?;
                let tmpfs = tmpfs_victim(&store_vfs, (mnt, dir), name)?;
                store.rmdir(dir, name)?;
                tmpfs_removed(&store_vfs, tmpfs);
                detach_points(cx, &store_vfs, gone, (dir, name));
            }
        }
        if let Some(ino) = victim {
            let k = cx.kr();
            inotify::deleted(k, ino, dir, name);
            inotify::dir_event(k, dir, name, inotify::IN_DELETE | inotify::IN_ISDIR, 0);
        }
    } else {
        if name == b"." || name == b".." {
            return Err(Errno::EISDIR);
        }
        writable_dir(&store_vfs, (mnt, dir))?;
        // A name ending in / can only be a directory: EISDIR when it is a directory, otherwise
        // ENOTDIR
        if path.ends_with(b"/") {
            let ino = store.lookup(dir, name)?.ok_or(Errno::ENOENT)?;
            return Err(if store.stat(ino)?.is_dir() {
                Errno::EISDIR
            } else {
                Errno::ENOTDIR
            });
        }
        may_delete(store, &cred, dir, name)?;
        let gone = not_mount_point(cx, &store_vfs, dir, name)?;
        let tmpfs = tmpfs_victim(&store_vfs, (mnt, dir), name)?;
        let held = gone.filter(|&ino| store.held(ino).unwrap_or(false));
        store.unlink(dir, name)?;
        tmpfs_removed(&store_vfs, tmpfs);
        release_if_closed(cx, &store_vfs, held);
        detach_points(cx, &store_vfs, gone, (dir, name));
        if let Some(ino) = victim {
            unlinked_events(cx, store, ino, (dir, name));
            inotify::dir_event(cx.kr(), dir, name, inotify::IN_DELETE, 0);
        }
    }
    Ok(())
}

/// unlink and rmdir in an overlay directory: returns Some when the parent is an overlay
/// directory. Linux's do_unlinkat and do_rmdir order: `.` and `..`, the read-only filesystem,
/// not found, a name ending in `/` on something that is not a directory, then `may_delete` on
/// what the overlay shows (search, then write permission and the sticky bit, then the type),
/// a mount point (EBUSY), a directory that the overlay does not show empty (ENOTEMPTY); then the
/// overlay's own removal ([`crate::vfs::overlay::Overlays::remove`]).
fn unlink_overlay(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: &[u8],
    flags: u32,
) -> Option<Result<(), Errno>> {
    let start = start_dir(cx, dirfd, path).ok()?;
    let vfs = cx.vfs().ok()?;
    let cred = cx.cred();
    let (dir, name) = vfs
        .resolve_parent(&cx.view(&vfs), &cred, &cx.root(), &start, path)
        .ok()?;
    let Node::Overlay(mnt, d) = dir else {
        return None;
    };
    let rmdir = flags & AT_REMOVEDIR != 0;
    Some((|| {
        match name {
            b"." if rmdir => return Err(Errno::EINVAL),
            b".." if rmdir => return Err(Errno::ENOTEMPTY),
            b"." | b".." => return Err(Errno::EISDIR),
            _ => {}
        }
        if vfs.readonly(&dir) {
            return Err(Errno::EROFS);
        }
        let child = vfs.overlay_child((mnt, d), name)?;
        let Node::Overlay(_, c) = child else {
            return Err(Errno::EIO);
        };
        let st = vfs.overlay_stat(c)?;
        if !rmdir && path.ends_with(b"/") {
            return Err(if st.is_dir() {
                Errno::EISDIR
            } else {
                Errno::ENOTDIR
            });
        }
        let dir_st = vfs.store().attrs(vfs.overlay().node(d).real())?;
        cred.permission(&dir_st, MAY_EXEC)?;
        cred.may_delete(&dir_st, &st)?;
        match (rmdir, st.is_dir()) {
            (true, false) => return Err(Errno::ENOTDIR),
            (false, true) => return Err(Errno::EISDIR),
            _ => {}
        }
        if vfs.is_point_in(&child, cx.mnt_ns()) {
            return Err(Errno::EBUSY);
        }
        if rmdir && !vfs.overlay_readdir(c)?.is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        vfs.overlay_remove(d, name, rmdir)?;
        // As the store's removals report: a file's link count changed, the node is deleted with
        // its last link (once the files opened through it have closed), then the directory's
        // entry goes
        let k = cx.kr();
        let (key, dir) = (overlay::key(c), overlay::key(d));
        if rmdir {
            inotify::deleted(k, key, dir, name);
            inotify::dir_event(k, dir, name, inotify::IN_DELETE | inotify::IN_ISDIR, 0);
        } else {
            inotify::path_event(k, key, None, inotify::IN_ATTRIB);
            if st.nlink <= 1 {
                inotify::deleted(k, key, dir, name);
            }
            inotify::dir_event(k, dir, name, inotify::IN_DELETE, 0);
        }
        Ok(())
    })())
}

/// Deletes a queue in the mqueue filesystem (as mq_unlink does): returns Some when the parent
/// directory is the mqueue root directory. The order follows Linux's do_unlinkat and do_rmdir:
/// `.` and `..`, the read-only mount, not found, unlink of a name ending in `/`, write permission
/// on the directory, the sticky bit (EPERM), rmdir's ENOTDIR.
fn unlink_mq(cx: &mut Cx<'_>, dirfd: i32, path: &[u8], flags: u32) -> Option<Result<(), Errno>> {
    let start = start_dir(cx, dirfd, path).ok()?;
    let vfs = cx.vfs().ok()?;
    let cred = cx.cred();
    let (dir, name) = vfs
        .resolve_parent(&cx.view(&vfs), &cred, &cx.root(), &start, path)
        .ok()?;
    let Node::Pseudo(_, PseudoNode::MqRoot(ns)) = dir else {
        return None;
    };
    let rmdir = flags & AT_REMOVEDIR != 0;
    Some((|| {
        match name {
            b"." if rmdir => return Err(Errno::EINVAL),
            b".." if rmdir => return Err(Errno::ENOTEMPTY),
            b"." | b".." => return Err(Errno::EISDIR),
            _ => {}
        }
        if vfs.readonly(&dir) {
            return Err(Errno::EROFS);
        }
        let ipc = cx.kr().ipc(ns);
        if !ipc.mqueues.contains_key(name) {
            return Err(Errno::ENOENT);
        }
        // unlink of a name ending in / is ENOTDIR before may_delete
        if !rmdir && path.ends_with(b"/") {
            return Err(Errno::ENOTDIR);
        }
        let dir_st = ipc.mq_dir.borrow().stat(ipc.mqueues.len());
        cred.permission(&dir_st, MAY_WRITE | MAY_EXEC)?;
        if rmdir {
            // rmdir checks the sticky bit first (may_delete); a queue is not a directory
            let q = ipc.mqueues[name].borrow().stat();
            let dir_uid = ipc.mq_dir.borrow().uid;
            if cred.fsuid != q.uid && cred.fsuid != dir_uid && !cred.capable_wrt(&q, cap::FOWNER) {
                return Err(Errno::EPERM);
            }
            return Err(Errno::ENOTDIR);
        }
        super::mqueue::remove_queue(cx.k().ipc_mut(ns), &cred, name)
    })())
}

/// unlink and rmdir in a cgroup directory: rmdir removes a child cgroup
/// ([`crate::cgroup::rmdir`]); files cannot be deleted (EPERM, cgroup directories have no
/// unlink).
fn cgroup_unlink(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: &[u8],
    flags: u32,
) -> Option<Result<(), Errno>> {
    let (dir, id, st, name) = cgroup_parent(cx, dirfd, path)?;
    let rmdir = flags & AT_REMOVEDIR != 0;
    Some((|| {
        match name {
            b"." if rmdir => return Err(Errno::EINVAL),
            b".." if rmdir => return Err(Errno::ENOTEMPTY),
            b"." | b".." => return Err(Errno::EISDIR),
            _ => {}
        }
        let vfs = cx.vfs()?;
        if vfs.readonly(&dir) {
            return Err(Errno::EROFS);
        }
        let cgroups = &cx.kr().cgroups;
        let child = cgroups.get(id).and_then(|g| g.children.get(name).copied());
        let file = cgroups.files(id).any(|f| f == name);
        if child.is_none() && !file {
            return Err(Errno::ENOENT);
        }
        cx.cred().permission(&st, MAY_WRITE | MAY_EXEC)?;
        match (rmdir, child) {
            (true, Some(c)) => crate::cgroup::rmdir(cx.k(), c),
            (true, None) => Err(Errno::ENOTDIR),
            (false, Some(_)) => Err(Errno::EISDIR),
            (false, None) => Err(Errno::EPERM),
        }
    })())
}

/// When the parent directory is a cgroup directory: `name` is that cgroup's id, its metadata
/// and the last component.
fn cgroup_parent<'p>(
    cx: &Cx<'_>,
    dirfd: i32,
    path: &'p [u8],
) -> Option<(Node, u64, Stat, &'p [u8])> {
    let start = start_dir(cx, dirfd, path).ok()?;
    let vfs = cx.vfs().ok()?;
    let (dir, name) = vfs
        .resolve_parent(&cx.view(&vfs), &cx.cred(), &cx.root(), &start, path)
        .ok()?;
    let Node::Pseudo(_, PseudoNode::Cgroup(id, b"")) = dir else {
        return None;
    };
    let st = pseudo::stat(&cx.view(&vfs), &PseudoNode::Cgroup(id, b"")).ok()?;
    Some((dir, id, st, name))
}

pub(super) fn symlinkat(cx: &mut Cx<'_>, target: u64, dirfd: i32, path: u64) -> SysResult {
    let target = read_path(cx, target)?;
    if target.is_empty() {
        return Err(Errno::ENOENT);
    }
    let path = read_path(cx, path)?;
    if let Some((mnt, dir, name)) = super::fuse::parent(cx, dirfd, &path)? {
        return super::fuse::symlink(cx, (mnt, dir), name, &target).map(|()| 0);
    }
    let (mnt, dir, name, ovl) = create_parent_at(cx, dirfd, &path)?;
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    let vfs = cx.vfs()?;
    let cred = cx.cred();
    reject_trailing_slash(&vfs, (mnt, dir), name, &path)?;
    let dir_st = may_create(&vfs, &cred, (mnt, dir), name)?;
    let (uid, gid, _) = cred.new_owner(&dir_st, 0o777, 0, false);
    // A long symlink takes an extra page in tmpfs (Linux's SHORT_SYMLINK_LEN)
    let pages = u64::from(target.len() >= 128);
    let tmpfs = tmpfs_reserve(&vfs, (mnt, dir), pages)?;
    create_in(
        &vfs,
        (mnt, dir),
        name,
        || vfs.store().symlink(dir, name, &target, uid, gid),
        |&ino| ino,
    )?;
    tmpfs_created(&vfs, tmpfs, pages);
    created_events(cx.kr(), (dir, ovl), name, 0);
    Ok(0)
}

pub(super) fn linkat(
    cx: &mut Cx<'_>,
    olddirfd: i32,
    oldpath: u64,
    newdirfd: i32,
    newpath: u64,
    flags: u32,
) -> SysResult {
    if flags & !(AT_SYMLINK_FOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    // Linux 6.1's do_linkat: linking an fd by AT_EMPTY_PATH needs CAP_DAC_READ_SEARCH, or anyone
    // could give a name to a file passed to it
    let cred = cx.cred();
    if flags & AT_EMPTY_PATH != 0 && !cred.capable(cap::DAC_READ_SEARCH) {
        return Err(Errno::ENOENT);
    }
    let oldpath = read_path(cx, oldpath)?;
    let src = lookup_at_or_fd(
        cx,
        &cred,
        olddirfd,
        &oldpath,
        flags,
        flags & AT_SYMLINK_FOLLOW != 0,
    )?;
    let newpath = read_path(cx, newpath)?;
    let start = start_dir(cx, newdirfd, &newpath)?;
    let vfs = cx.vfs()?;
    // A deleted regular file reached through /proc/<pid>/fd/N is the open file itself
    // (follow_fd); Linux finds its dentry on the mount it was opened through, so the link is EXDEV
    // into another mount and refused as unlinked (ENOENT, below) in the same one
    let src = match src {
        Node::Pseudo(_, PseudoNode::OpenFd(pid, fd)) => {
            let file = cx.view(&vfs).open_file(pid, fd).ok_or(Errno::ENOENT)?;
            let f = file.borrow();
            match &f.kind {
                // A memfd is on no mount of the caller's (Linux's internal shm mount): EXDEV
                FileKind::Regular { ino, .. } if !f.pseudo && f.ovl.is_none() => {
                    Node::Store(f.mnt, *ino)
                }
                _ => Node::Pseudo(f.mnt, PseudoNode::OpenFd(pid, fd)),
            }
        }
        src => src,
    };
    let (dir, name) = vfs.resolve_parent(&cx.view(&vfs), &cred, &cx.root(), &start, &newpath)?;
    if matches!(dir, Node::Pseudo(..)) && vfs.has_child(&cx.view(&vfs), &dir, name) {
        return Err(Errno::EEXIST);
    }
    if let (Node::Overlay(..), Node::Overlay(..)) = (&src, &dir) {
        return link_overlay(cx, &vfs, (&src, &dir), name, &newpath);
    }
    if let Node::Fuse(mnt, d) = dir {
        return super::fuse::link(cx, &src, (mnt, d), name).map(|()| 0);
    }
    // A synthetic filesystem is another "device": hard links cannot cross devices
    let (Node::Store(src_mnt, src), Node::Store(mnt, dir)) = (src, dir) else {
        return Err(Errno::EXDEV);
    };
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    reject_trailing_slash(&vfs, (mnt, dir), name, &newpath)?;
    // Two sides reached through different mounts (even one filesystem bound in two places) are
    // EXDEV, after the existing name and the read-only mount and before the directory being
    // writable (Linux's do_linkat)
    let dir_st = may_create_name(&vfs, &cred, (mnt, dir), name)?;
    if src_mnt != mnt {
        return Err(Errno::EXDEV);
    }
    // fs.protected_hardlinks (Linux's may_linkat, after EXDEV and before the directory's
    // permission): without being the owner or having CAP_FOWNER, only readable and writable
    // regular files can be linked, and not setuid ones or executable setgid ones
    if cx.kr().protected_hardlinks {
        // The database row: a deleted file's object may already be gone
        let st = vfs.store().attrs(src)?;
        let setgid_exec = mode::S_ISGID | 0o010;
        let safe = st.mode & mode::S_IFMT == mode::S_IFREG
            && st.mode & mode::S_ISUID == 0
            && st.mode & setgid_exec != setgid_exec
            && cred.permission(&st, MAY_READ | MAY_WRITE).is_ok();
        if !safe && !cred.owns(&st) {
            return Err(Errno::EPERM);
        }
    }
    cred.permission(&dir_st, MAY_WRITE | MAY_EXEC)?;
    // In tmpfs every hard link also takes an inode (Linux's shmem_link)
    let tmpfs = tmpfs_reserve(&vfs, (mnt, dir), 0)?;
    vfs.store().link(dir, name, src)?;
    tmpfs_created(&vfs, tmpfs, 0);
    inotify::path_event(cx.kr(), src, None, inotify::IN_ATTRIB);
    inotify::dir_event(cx.kr(), dir, name, inotify::IN_CREATE, 0);
    Ok(0)
}

/// linkat within an overlay (6.1's `ovl_link`, after `do_linkat`'s checks in the same order as
/// for the store, made on what the overlay shows): the file is copied up and linked in the upper
/// directory, replacing a whiteout of the name.
fn link_overlay(
    cx: &Cx<'_>,
    vfs: &Vfs,
    (src, dir): (&Node, &Node),
    name: &[u8],
    newpath: &[u8],
) -> SysResult {
    let (&Node::Overlay(src_mnt, so), &Node::Overlay(mnt, d)) = (src, dir) else {
        return Err(Errno::EXDEV);
    };
    if name == b"." || name == b".." || vfs.has_child(&cx.view(vfs), dir, name) {
        return Err(Errno::EEXIST);
    }
    if newpath.ends_with(b"/") {
        return Err(Errno::ENOENT);
    }
    let cred = cx.cred();
    let dir_st = vfs.store().attrs(vfs.overlay().node(d).real())?;
    cred.permission(&dir_st, MAY_EXEC)?;
    if vfs.readonly(dir) {
        return Err(Errno::EROFS);
    }
    if src_mnt != mnt {
        return Err(Errno::EXDEV);
    }
    let st = vfs.store().attrs(vfs.overlay().node(so).real())?;
    if cx.kr().protected_hardlinks {
        let setgid_exec = mode::S_ISGID | 0o010;
        let safe = st.mode & mode::S_IFMT == mode::S_IFREG
            && st.mode & mode::S_ISUID == 0
            && st.mode & setgid_exec != setgid_exec
            && cred.permission(&st, MAY_READ | MAY_WRITE).is_ok();
        if !safe && !cred.owns(&st) {
            return Err(Errno::EPERM);
        }
    }
    cred.permission(&dir_st, MAY_WRITE | MAY_EXEC)?;
    if st.is_dir() {
        return Err(Errno::EPERM);
    }
    let file = copy_up(cx, vfs, so, CopyUp::All)?;
    let (mnt, up) = create_dir(vfs, dir)?;
    create_in(
        vfs,
        (mnt, up),
        name,
        || vfs.store().link(up, name, file).map(|()| file),
        |&ino| ino,
    )?;
    // The new name in the upper directory, then as 6.1's `fsnotify_link` reports the link through
    // the overlay: the file's link count changed, then the name
    let k = cx.kr();
    inotify::dir_event(k, up, name, inotify::IN_CREATE, 0);
    inotify::path_event(k, overlay::key(so), None, inotify::IN_ATTRIB);
    inotify::dir_event(k, overlay::key(d), name, inotify::IN_CREATE, 0);
    Ok(0)
}

pub(super) fn renameat2(
    cx: &mut Cx<'_>,
    olddirfd: i32,
    oldpath: u64,
    newdirfd: i32,
    newpath: u64,
    flags: u32,
) -> SysResult {
    if flags & !(RENAME_NOREPLACE | RENAME_EXCHANGE) != 0
        || flags == RENAME_NOREPLACE | RENAME_EXCHANGE
    {
        return Err(Errno::EINVAL);
    }
    let oldpath = read_path(cx, oldpath)?;
    let newpath = read_path(cx, newpath)?;
    let parent = |dirfd, path| -> Result<(Node, &[u8]), Errno> {
        let start = start_dir(cx, dirfd, path)?;
        let vfs = cx.vfs()?;
        vfs.resolve_parent(&cx.view(&vfs), &cx.cred(), &cx.root(), &start, path)
    };
    let (old_parent, old_name) = parent(olddirfd, &oldpath)?;
    let (new_parent, new_name) = parent(newdirfd, &newpath)?;
    // Two sides reached through different mounts are EXDEV, and only then come `.` and `..` (a
    // destination one is EEXIST with RENAME_NOREPLACE), the read-only mount (Linux's
    // do_renameat2)
    if old_parent.mnt() != new_parent.mnt() {
        return Err(Errno::EXDEV);
    }
    let dots = |n: &[u8]| n == b"." || n == b"..";
    if dots(old_name) {
        return Err(Errno::EBUSY);
    }
    if dots(new_name) {
        return Err(if flags & RENAME_NOREPLACE != 0 {
            Errno::EEXIST
        } else {
            Errno::EBUSY
        });
    }
    if let (Node::Pseudo(..), _) | (_, Node::Pseudo(..)) = (&old_parent, &new_parent) {
        return rename_synthetic(
            cx,
            (olddirfd, &oldpath),
            (newdirfd, &newpath),
            flags,
            &old_parent,
        );
    }
    if let (Node::Overlay(mnt, od), Node::Overlay(_, nd)) = (&old_parent, &new_parent) {
        let slashes = oldpath.ends_with(b"/") || newpath.ends_with(b"/");
        return rename_overlay(cx, *mnt, (*od, old_name), (*nd, new_name), (flags, slashes));
    }
    if let (Node::Fuse(om, od), Node::Fuse(nm, nd)) = (&old_parent, &new_parent) {
        let (from, to) = ((*om, *od, old_name), (*nm, *nd, new_name));
        return super::fuse::rename_at(cx, from, to, flags).map(|()| 0);
    }
    let vfs = cx.vfs()?;
    let (old_mnt, old_dir) = store_dir(&vfs, &old_parent)?;
    let (new_mnt, new_dir) = store_dir(&vfs, &new_parent)?;
    writable_dir(&vfs, (old_mnt, old_dir))?;
    // If either side ends in /, what is moved must be a directory
    if (oldpath.ends_with(b"/") || newpath.ends_with(b"/"))
        && let Some(src) = vfs.store().lookup(old_dir, old_name)?
        && !vfs.store().attrs(src)?.is_dir()
    {
        return Err(Errno::ENOTDIR);
    }
    let exchange = flags & RENAME_EXCHANGE != 0;
    may_rename(
        vfs.store(),
        &cx.cred(),
        (old_dir, old_name),
        (new_dir, new_name),
        exchange,
    )?;
    not_mount_point(cx, &vfs, old_dir, old_name)?;
    let overwritten = not_mount_point(cx, &vfs, new_dir, new_name)?;
    // With inotify watches, note both sides' inodes and whether they are directories first
    let entry = |dir, name| -> Result<Option<(Ino, u32)>, Errno> {
        match vfs.store().lookup(dir, name)? {
            Some(ino) if vfs.store().attrs(ino)?.is_dir() => Ok(Some((ino, inotify::IN_ISDIR))),
            Some(ino) => Ok(Some((ino, 0))),
            None => Ok(None),
        }
    };
    let (src, dst) = if inotify::active(cx.kr()) {
        (entry(old_dir, old_name)?, entry(new_dir, new_name)?)
    } else {
        (None, None)
    };
    // An overwritten name is freed in tmpfs (renaming to another hard link of itself does
    // nothing)
    let replaced = match exchange {
        false
            if vfs.store().lookup(old_dir, old_name)?
                != vfs.store().lookup(new_dir, new_name)? =>
        {
            tmpfs_victim(&vfs, (new_mnt, new_dir), new_name)?
        }
        _ => None,
    };
    let moving = vfs.store().lookup(old_dir, old_name)?;
    let held = overwritten.filter(|&ino| vfs.store().held(ino).unwrap_or(false));
    vfs.store().rename(
        old_dir,
        old_name,
        new_dir,
        new_name,
        flags & RENAME_NOREPLACE != 0,
        exchange,
    )?;
    tmpfs_removed(&vfs, replaced);
    // Detach the mounts of other namespaces on the overwritten target
    if !exchange && overwritten != moving {
        release_if_closed(cx, &vfs, held);
        detach_points(cx, &vfs, overwritten, (new_dir, new_name));
    }
    // Files open through the moved name follow the rename
    let k = cx.kr();
    let store = vfs.store();
    let lookup = |dir, name| move || store.lookup(dir, name).ok().flatten();
    inotify::renamed(
        k,
        (old_dir, old_name),
        (new_dir, new_name),
        lookup(new_dir, new_name),
    );
    if exchange {
        inotify::renamed(
            k,
            (new_dir, new_name),
            (old_dir, old_name),
            lookup(old_dir, old_name),
        );
    }
    // Order as in Linux's fsnotify_move; for an exchange the other side moves once more
    let moved = |isdir: u32, from: (Ino, &[u8]), to: (Ino, &[u8])| {
        let cookie = inotify::cookie(k);
        inotify::dir_event(k, from.0, from.1, inotify::IN_MOVED_FROM | isdir, cookie);
        inotify::dir_event(k, to.0, to.1, inotify::IN_MOVED_TO | isdir, cookie);
    };
    if let Some(src) = src {
        moved(src.1, (old_dir, old_name), (new_dir, new_name));
        match dst {
            Some(dst) if exchange => {
                inotify::path_event(k, src.0, None, inotify::IN_MOVE_SELF);
                moved(dst.1, (new_dir, new_name), (old_dir, old_name));
                inotify::path_event(k, dst.0, None, inotify::IN_MOVE_SELF);
            }
            // fsnotify_move: the replaced object's link count changed, then the moved one moved
            // itself; the replaced one is deleted with its last link
            Some((replaced, isdir)) => {
                inotify::path_event(k, replaced, None, inotify::IN_ATTRIB | isdir);
                inotify::path_event(k, src.0, None, inotify::IN_MOVE_SELF);
                if isdir != 0 || vfs.store().stat(replaced).map_or(0, |st| st.nlink) == 0 {
                    inotify::deleted(k, replaced, new_dir, new_name);
                }
            }
            None => inotify::path_event(k, src.0, None, inotify::IN_MOVE_SELF),
        }
    }
    Ok(0)
}

/// renameat2 within an overlay mount (after the mount and `.`/`..` checks): the read-only
/// filesystem, a trailing `/` on something that is not a directory, then [`may_rename`]'s checks
/// and mount points (EBUSY) on what the overlay shows, then the overlay's own rename
/// ([`crate::vfs::overlay::Overlays::rename`]).
fn rename_overlay(
    cx: &Cx<'_>,
    mnt: MountId,
    (od, old_name): (crate::vfs::overlay::OvlId, &[u8]),
    (nd, new_name): (crate::vfs::overlay::OvlId, &[u8]),
    (flags, slashes): (u32, bool),
) -> SysResult {
    let vfs = cx.vfs()?;
    let attrs = |o| vfs.store().attrs(vfs.overlay().node(o).real());
    let old_parent = Node::Overlay(mnt, od);
    if vfs.readonly(&old_parent) {
        return Err(Errno::EROFS);
    }
    let cred = cx.cred();
    let od_st = attrs(od)?;
    cred.permission(&od_st, MAY_EXEC)?;
    let Node::Overlay(m, src) = vfs.overlay_child((mnt, od), old_name)? else {
        return Err(Errno::EIO);
    };
    let src_st = attrs(src)?;
    if slashes && !src_st.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    cred.may_delete(&od_st, &src_st)?;
    let nd_st = attrs(nd)?;
    let dst = match vfs.overlay_child((m, nd), new_name) {
        Ok(Node::Overlay(_, d)) => {
            let st = attrs(d)?;
            cred.permission(&nd_st, MAY_EXEC)?;
            cred.may_delete(&nd_st, &st)?;
            Some((d, st))
        }
        _ => {
            cred.permission(&nd_st, MAY_WRITE | MAY_EXEC)?;
            None
        }
    };
    let exchange = flags & RENAME_EXCHANGE != 0;
    if od != nd {
        let moved = [
            Some(&src_st),
            dst.as_ref().map(|d| &d.1).filter(|_| exchange),
        ];
        for st in moved.iter().flatten().filter(|st| st.is_dir()) {
            cred.permission(st, MAY_WRITE)?;
        }
    }
    let ns = cx.mnt_ns();
    let busy = |o| vfs.is_point_in(&Node::Overlay(m, o), ns);
    if busy(src) || dst.as_ref().is_some_and(|d| busy(d.0)) {
        return Err(Errno::EBUSY);
    }
    // Files that move are copied up here, as copy_up copies them, before the rename finds them up
    let moved = [
        Some((src, &src_st)),
        dst.as_ref().map(|(d, st)| (*d, st)).filter(|_| exchange),
    ];
    for (o, st) in moved.into_iter().flatten() {
        if !st.is_dir() {
            copy_up(cx, &vfs, o, CopyUp::All)?;
        }
    }
    vfs.overlay_rename(
        (od, old_name),
        (nd, new_name),
        (flags & RENAME_NOREPLACE != 0, exchange),
    )?;
    // Renaming a name to itself changes nothing
    if dst.as_ref().is_some_and(|d| d.0 == src) {
        return Ok(0);
    }
    // As the store's rename reports, on the overlay's nodes: files open through a moved name
    // follow it, then fsnotify_move's order
    let k = cx.kr();
    let (old, new) = ((overlay::key(od), old_name), (overlay::key(nd), new_name));
    inotify::renamed(k, old, new, || Some(overlay::key(src)));
    if exchange && let Some(d) = dst.as_ref().map(|d| d.0) {
        inotify::renamed(k, new, old, || Some(overlay::key(d)));
    }
    let isdir = |st: &Stat| {
        if st.is_dir() { inotify::IN_ISDIR } else { 0 }
    };
    let moved = |isdir: u32, from: (Ino, &[u8]), to: (Ino, &[u8])| {
        let cookie = inotify::cookie(k);
        inotify::dir_event(k, from.0, from.1, inotify::IN_MOVED_FROM | isdir, cookie);
        inotify::dir_event(k, to.0, to.1, inotify::IN_MOVED_TO | isdir, cookie);
    };
    let src = overlay::key(src);
    moved(isdir(&src_st), old, new);
    match dst {
        Some((d, st)) if exchange => {
            inotify::path_event(k, src, None, inotify::IN_MOVE_SELF);
            moved(isdir(&st), new, old);
            inotify::path_event(k, overlay::key(d), None, inotify::IN_MOVE_SELF);
        }
        Some((d, st)) => {
            let d = overlay::key(d);
            inotify::path_event(k, d, None, inotify::IN_ATTRIB | isdir(&st));
            inotify::path_event(k, src, None, inotify::IN_MOVE_SELF);
            if st.is_dir() || st.nlink <= 1 {
                inotify::deleted(k, d, new.0, new.1);
            }
        }
        None => inotify::path_event(k, src, None, inotify::IN_MOVE_SELF),
    }
    Ok(0)
}

/// Checks before rename: both directories must be writable and obey the sticky bit; when a
/// directory changes parent, it must itself be writable too (its `..` is modified).
fn may_rename(
    store: &Store,
    cred: &Cred,
    (old_dir, old_name): (Ino, &[u8]),
    (new_dir, new_name): (Ino, &[u8]),
    exchange: bool,
) -> Result<(), Errno> {
    let src = may_delete(store, cred, old_dir, old_name)?;
    let dst = match store.lookup(new_dir, new_name)? {
        Some(_) => Some(may_delete(store, cred, new_dir, new_name)?),
        None => {
            cred.permission(&store.stat(new_dir)?, MAY_WRITE | MAY_EXEC)?;
            None
        }
    };
    if old_dir != new_dir {
        let moved = [Some(src), dst.filter(|_| exchange)];
        for st in moved.iter().flatten().filter(|st| st.is_dir()) {
            cred.permission(st, MAY_WRITE)?;
        }
    }
    Ok(())
}

/// Before deleting or renaming: EBUSY when a mount of the caller's namespace is on `name` in
/// directory `dir` (Linux's `is_local_mountpoint`). Returns its inode, used after the deletion
/// to detach the mounts on it in other namespaces ([`detach_points`]).
fn not_mount_point(cx: &Cx<'_>, vfs: &Vfs, dir: Ino, name: &[u8]) -> Result<Option<Ino>, Errno> {
    match vfs.store().lookup(dir, name)? {
        Some(ino) if vfs.is_store_point((ino, dir, name), cx.mnt_ns()) => Err(Errno::EBUSY),
        found => Ok(found),
    }
}

/// A held file (`ino`, looked at before the unlink, while its row was still cached) that has just
/// lost its last link stops sharing its object at once when nothing in this kernel has it open or
/// mapped ([`Store::release`]), rather than at the next boot: its object can go, and another file
/// still sharing it need not copy it before a write.
fn release_if_closed(cx: &Cx<'_>, vfs: &Vfs, ino: Option<Ino>) {
    use std::os::unix::fs::MetadataExt;
    let Some(ino) = ino else {
        return;
    };
    let store = vfs.store();
    if !store.attrs(ino).is_ok_and(|st| st.nlink == 0) || !opens_of(cx.kr(), ino).is_empty() {
        return;
    }
    let Ok(meta) = store
        .open_object(ino, false)
        .and_then(|f| f.metadata().map_err(host_errno))
    else {
        return;
    };
    let mapped = cx
        .kr()
        .tasks
        .values()
        .any(|t| t.res.borrow().mm.borrow().maps_file(meta.dev(), meta.ino()));
    if !mapped {
        let _ = store.release(ino);
    }
}

/// `name` in directory `dir` (formerly `ino`) has been deleted: the mounts on it in other
/// namespaces are detached (`detach_mounts` in Linux's vfs_rmdir, vfs_unlink and vfs_rename),
/// and mounts of it show it as deleted.
fn detach_points(cx: &Cx<'_>, vfs: &Vfs, ino: Option<Ino>, (dir, name): (Ino, &[u8])) {
    if let Some(ino) = ino {
        vfs.detach_store_point((ino, dir, name));
        vfs.roots_removed((ino, dir, name));
        cx.shared.fast_changed();
    }
}

/// Link `name` of file `ino` was unlinked: the link count changed (IN_ATTRIB), and
/// IN_DELETE_SELF when the last link is gone too. Both events are reported only to the file
/// itself.
fn unlinked_events(cx: &Cx<'_>, store: &Store, ino: Ino, (dir, name): (Ino, &[u8])) {
    let k = cx.kr();
    inotify::path_event(k, ino, None, inotify::IN_ATTRIB);
    if store.stat(ino).map_or(0, |st| st.nlink) == 0 {
        inotify::deleted(k, ino, dir, name);
    }
}

// ---- chmod, chown and utimensat ----

/// What chmod, chown and utimensat act on: a store inode, or `/dev/pts/<n>` (owner, permissions
/// and times are kept on the terminal, as with Linux's devpts). Other nodes in synthetic
/// filesystems cannot be modified.
enum Attrs {
    Store(Ino),
    /// A node in an overlay: checked as what the overlay shows, changed in the upper layer
    /// after a copy-up (6.1's `ovl_setattr`).
    Overlay(crate::vfs::overlay::OvlId),
    Pts(Rc<RefCell<Tty>>),
    /// An fd of a socket or anonymous pipe: on Linux this changes their own inode in sockfs or
    /// pipefs, owned by the process that created it (libuv's `uv_pipe_chmod` fchmods the
    /// socket's fd first). Here it is treated as the caller's, the change is not recorded, and
    /// the owner and permissions fstat reports stay synthetic.
    Anon,
    /// A POSIX message queue: a file in the mqueue filesystem, or an fd from mq_open.
    Mq(Rc<RefCell<super::MessageQueue>>),
    /// The root directory of the mqueue filesystem of an IPC namespace (its id).
    MqDir(u64),
    /// A node in a FUSE filesystem, changed by its daemon (`FUSE_SETATTR`, through the handle of
    /// the open file it was reached by, if any).
    Fuse(crate::vfs::fuse::FuseId, Option<u64>),
}

impl Attrs {
    fn of(cx: &Cx<'_>, node: &Node) -> Result<Attrs, Errno> {
        match node {
            Node::Pseudo(_, PseudoNode::Pts(..)) if cx.vfs()?.readonly(node) => Err(Errno::EROFS),
            Node::Pseudo(_, PseudoNode::Pts(fs, n)) => pseudo::pty(cx.kr(), *fs, *n)
                .map(Attrs::Pts)
                .ok_or(Errno::ENOENT),
            Node::Pseudo(_, PseudoNode::MqRoot(_) | PseudoNode::Mq(..))
                if cx.vfs()?.readonly(node) =>
            {
                Err(Errno::EROFS)
            }
            Node::Pseudo(_, PseudoNode::MqRoot(ns)) => Ok(Attrs::MqDir(*ns)),
            Node::Pseudo(_, PseudoNode::Mq(ns, ino)) => mq_queue(cx, *ns, *ino).map(Attrs::Mq),
            Node::Overlay(..) if cx.vfs()?.readonly(node) => Err(Errno::EROFS),
            Node::Overlay(_, o) => Ok(Attrs::Overlay(*o)),
            Node::Fuse(..) if cx.vfs()?.readonly(node) => Err(Errno::EROFS),
            Node::Fuse(_, f) => Ok(Attrs::Fuse(*f, None)),
            _ => store_ino(cx, node).map(Attrs::Store),
        }
    }

    /// An open file: a terminal slave, or a file in the store (not one opened with `O_PATH`).
    fn of_fd(cx: &Cx<'_>, fd: i32) -> Result<Attrs, Errno> {
        let file = cx.fds().get(fd)?;
        let f = file.borrow();
        match &f.kind {
            FileKind::Pty(end) if !end.master && f.flags & oflags::O_PATH == 0 => {
                Ok(Attrs::Pts(end.tty.clone()))
            }
            FileKind::Socket(_) => Ok(Attrs::Anon),
            FileKind::Pipe(end) if end.pipe.borrow().fifo.is_none() => Ok(Attrs::Anon),
            FileKind::MessageQueue(q) => Ok(Attrs::Mq(q.clone())),
            _ if matches!(f.node(), Some(Node::Fuse(..))) => {
                let Some(node @ Node::Fuse(_, id)) = f.node() else {
                    return Err(Errno::EBADF);
                };
                if f.flags & oflags::O_PATH != 0 {
                    return Err(Errno::EBADF);
                }
                if cx.vfs()?.readonly(&node) {
                    return Err(Errno::EROFS);
                }
                let fh = match &f.kind {
                    FileKind::Fuse(h) => Some(h.fh),
                    _ => None,
                };
                Ok(Attrs::Fuse(id, fh))
            }
            _ if matches!(f.node(), Some(Node::Overlay(..))) => {
                let Some(node @ Node::Overlay(_, o)) = f.node() else {
                    return Err(Errno::EBADF);
                };
                if f.flags & oflags::O_PATH != 0 {
                    return Err(Errno::EBADF);
                }
                if cx.vfs()?.readonly(&node) {
                    return Err(Errno::EROFS);
                }
                Ok(Attrs::Overlay(o))
            }
            _ => {
                drop(f);
                file_ino(cx, fd).map(Attrs::Store)
            }
        }
    }

    /// The store inode a change by path went to, for inotify ([`attrib_at`], which finds an
    /// overlay node again by its path): an overlay node's topmost inode.
    fn ino(&self, cx: &Cx<'_>) -> Option<Ino> {
        match self {
            Attrs::Store(ino) => Some(*ino),
            Attrs::Overlay(o) => Some(cx.vfs().ok()?.overlay().node(*o).real()),
            _ => None,
        }
    }

    fn stat(&self, cx: &Cx<'_>) -> Result<Stat, Errno> {
        match self {
            Attrs::Store(ino) => cx.vfs()?.store().attrs(*ino),
            Attrs::Overlay(o) => {
                let vfs = cx.vfs()?;
                vfs.store().attrs(vfs.overlay().node(*o).real())
            }
            Attrs::Pts(t) => {
                let t = t.borrow();
                Ok(Stat {
                    mode: mode::S_IFCHR | t.mode,
                    uid: t.owner.0,
                    gid: t.owner.1,
                    ..Stat::default()
                })
            }
            Attrs::Anon => {
                let cred = cx.cred();
                Ok(Stat {
                    uid: cred.fsuid,
                    gid: cred.fsgid,
                    ..Stat::default()
                })
            }
            Attrs::Mq(q) => Ok(q.borrow().stat()),
            Attrs::MqDir(ns) => {
                let ipc = cx.kr().ipc(*ns);
                Ok(ipc.mq_dir.borrow().stat(ipc.mqueues.len()))
            }
            Attrs::Fuse(id, _) => Ok(crate::vfs::fuse::cached_attr(&*cx.vfs()?, *id)?.stat()),
        }
    }

    /// For a FUSE node: whether the kernel checks the change itself (`default_permissions`);
    /// a process the mount does not allow is EACCES (6.1's `fuse_setattr`). True for the rest.
    fn checked(&self, cx: &Cx<'_>) -> Result<bool, Errno> {
        let Attrs::Fuse(id, _) = self else {
            return Ok(true);
        };
        let vfs = cx.vfs()?;
        if !vfs.fuse().conn_of(*id)?.borrow().allows(&cx.cred()) {
            return Err(Errno::EACCES);
        }
        super::fuse::checks_permissions(&vfs, *id)
    }
}

pub(super) fn fchmodat(cx: &mut Cx<'_>, dirfd: i32, path: u64, perm: u32) -> SysResult {
    let path = read_path(cx, path)?;
    let target = Attrs::of(cx, &lookup_at(cx, dirfd, &path, true)?)?;
    chmod(cx, &target, perm)?;
    if let Some(ino) = target.ino(cx) {
        attrib_at(cx, ino, dirfd, &path, true);
    }
    Ok(0)
}

pub(super) fn fchmod(cx: &mut Cx<'_>, fd: i32, perm: u32) -> SysResult {
    let target = Attrs::of_fd(cx, fd)?;
    chmod(cx, &target, perm)?;
    attrib_fd(cx, fd);
    Ok(0)
}

/// Only the owner (or a holder of `CAP_FOWNER`) can change permission bits; a caller not in the
/// file's group cannot set the setgid bit (silently dropped without `CAP_FSETID`).
fn chmod(cx: &mut Cx<'_>, target: &Attrs, perm: u32) -> SysResult {
    let mut perm = perm & 0o7777;
    if target.checked(cx)? {
        let st = target.stat(cx)?;
        let cred = cx.cred();
        if !cred.owns(&st) {
            return Err(Errno::EPERM);
        }
        if !cred.in_group(st.gid) && !cred.capable_wrt(&st, cap::FSETID) {
            perm &= !mode::S_ISGID;
        }
    }
    match target {
        Attrs::Store(ino) => {
            // A memfd sealed with F_SEAL_EXEC keeps its execute bits (6.3's shmem_setattr)
            let sealed = cx.kr().seals.get(ino).is_some_and(|s| s & F_SEAL_EXEC != 0);
            if sealed && (target.stat(cx)?.mode ^ perm) & 0o111 != 0 {
                return Err(Errno::EPERM);
            }
            cx.vfs()?.store().set_mode(*ino, perm)?
        }
        Attrs::Overlay(o) => {
            let vfs = cx.vfs()?;
            let ino = copy_up(cx, &vfs, *o, CopyUp::All)?;
            vfs.store().set_mode(ino, perm)?;
        }
        Attrs::Pts(t) => {
            let mut t = t.borrow_mut();
            t.mode = perm;
            t.changed();
        }
        Attrs::Anon => {}
        Attrs::Mq(q) => q.borrow_mut().set_mode(perm),
        Attrs::MqDir(ns) => {
            let ipc = cx.kr().ipc(*ns);
            let mut d = ipc.mq_dir.borrow_mut();
            d.mode = perm;
            d.ctime_ns = crate::tty::wall_ns();
        }
        Attrs::Fuse(id, fh) => {
            let set = super::fuse::SetAttr {
                mode: Some(perm),
                ..Default::default()
            };
            super::fuse::setattr(cx, &*cx.vfs()?, *id, &set, *fh)?;
        }
    }
    Ok(0)
}

pub(super) fn fchownat(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    uid: u32,
    gid: u32,
    flags: u32,
) -> SysResult {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    let node = lookup_at_or_fd(cx, &cx.cred(), dirfd, &path, flags, follow)?;
    let target = Attrs::of(cx, &node)?;
    chown(cx, &target, uid, gid)?;
    if path.is_empty() {
        attrib_fd(cx, dirfd);
    } else if let Some(ino) = target.ino(cx) {
        attrib_at(cx, ino, dirfd, &path, follow);
    }
    Ok(0)
}

pub(super) fn fchown(cx: &mut Cx<'_>, fd: i32, uid: u32, gid: u32) -> SysResult {
    let target = Attrs::of_fd(cx, fd)?;
    chown(cx, &target, uid, gid)?;
    attrib_fd(cx, fd);
    Ok(0)
}

/// Without `CAP_CHOWN`, the owner can only change the group to one it is in, and cannot change
/// the owner. IDs are in the caller's user namespace, and unmapped ones are EINVAL; `CAP_CHOWN`
/// requires the file's current owner and group to be mapped there.
fn chown(cx: &mut Cx<'_>, target: &Attrs, uid: u32, gid: u32) -> SysResult {
    let cred = cx.cred();
    let uid = owner_arg(uid)
        .map(|u| cred.uid_in(u).ok_or(Errno::EINVAL))
        .transpose()?;
    let gid = owner_arg(gid)
        .map(|g| cred.gid_in(g).ok_or(Errno::EINVAL))
        .transpose()?;
    if let Attrs::Fuse(id, fh) = target {
        let checked = target.checked(cx)?;
        if checked {
            let st = target.stat(cx)?;
            let owner = cred.fsuid == st.uid;
            let uid_ok = uid.is_none_or(|u| owner && u == st.uid);
            let gid_ok = gid.is_none_or(|g| owner && (g == st.gid || cred.in_group(g)));
            if !(cred.capable_wrt(&st, cap::CHOWN) || uid_ok && gid_ok) {
                return Err(Errno::EPERM);
            }
        }
        if uid.is_none() && gid.is_none() {
            return Ok(0);
        }
        let set = super::fuse::SetAttr {
            uid,
            gid,
            ..Default::default()
        };
        super::fuse::setattr(cx, &*cx.vfs()?, *id, &set, *fh)?;
        return Ok(0);
    }
    let st = target.stat(cx)?;
    let owner = cred.fsuid == st.uid;
    let uid_ok = uid.is_none_or(|u| owner && u == st.uid);
    let gid_ok = gid.is_none_or(|g| owner && (g == st.gid || cred.in_group(g)));
    if !(cred.capable_wrt(&st, cap::CHOWN) || uid_ok && gid_ok) {
        return Err(Errno::EPERM);
    }
    // Changing a non-directory's owner, even chown(-1, -1), clears its setuid bit, and its setgid
    // bit when group-executable or when the caller is neither in its group nor has CAP_FSETID
    // (Linux's setattr_should_drop_sgid, since 6.2): a mode change, which only the owner or
    // CAP_FOWNER may make (notify_change adds ATTR_MODE, checked by inode_owner_or_capable).
    // Otherwise anyone could disable root's set-id programs (su, doas)
    let keep_sgid = cred.in_group(st.gid) || cred.capable_wrt(&st, cap::FSETID);
    if let Attrs::Store(_) | Attrs::Overlay(_) = target
        && !st.is_dir()
        && (st.mode & mode::S_ISUID != 0
            || st.mode & mode::S_ISGID != 0 && (st.mode & 0o010 != 0 || !keep_sgid))
        && !cred.owns(&st)
    {
        return Err(Errno::EPERM);
    }
    match target {
        Attrs::Store(ino) => cx.vfs()?.store().set_owner(*ino, uid, gid, keep_sgid)?,
        Attrs::Overlay(o) => {
            let vfs = cx.vfs()?;
            let ino = copy_up(cx, &vfs, *o, CopyUp::All)?;
            vfs.store().set_owner(ino, uid, gid, keep_sgid)?;
        }
        Attrs::Pts(t) => {
            let mut t = t.borrow_mut();
            t.owner = (uid.unwrap_or(t.owner.0), gid.unwrap_or(t.owner.1));
            t.changed();
        }
        Attrs::Anon => {}
        Attrs::Mq(q) => q.borrow_mut().set_owner(uid, gid),
        Attrs::MqDir(ns) => {
            let ipc = cx.kr().ipc(*ns);
            let mut d = ipc.mq_dir.borrow_mut();
            (d.uid, d.gid) = (uid.unwrap_or(d.uid), gid.unwrap_or(d.gid));
            d.ctime_ns = crate::tty::wall_ns();
        }
        Attrs::Fuse(..) => unreachable!("handled above"),
    }
    Ok(0)
}

fn owner_arg(id: u32) -> Option<u32> {
    (id != u32::MAX).then_some(id)
}

pub(super) fn utimensat(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    times: u64,
    flags: u32,
) -> SysResult {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    // A NULL path acts on dirfd itself (futimens)
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    let (target, path) = if path == 0 {
        if dirfd == AT_FDCWD {
            return Err(Errno::EFAULT);
        }
        (Attrs::of_fd(cx, dirfd)?, Vec::new())
    } else {
        let path = read_path(cx, path)?;
        let node = lookup_at_or_fd(cx, &cx.cred(), dirfd, &path, flags, follow)?;
        (Attrs::of(cx, &node)?, path)
    };
    // times[0] is atime and times[1] is mtime. The current time is taken by the store (for
    // regular files it is left to the host)
    let (atime, mtime, explicit) = if times == 0 {
        (Some(FileTime::Now), Some(FileTime::Now), false)
    } else {
        let b: [u8; 32] = get(cx, times)?;
        let word = |i: usize| i64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        for nsec in [word(1), word(3)] {
            if nsec != UTIME_NOW && nsec != UTIME_OMIT && !(0..1_000_000_000).contains(&nsec) {
                return Err(Errno::EINVAL);
            }
        }
        if word(1) == UTIME_OMIT && word(3) == UTIME_OMIT {
            return Ok(0);
        }
        let explicit = [word(1), word(3)]
            .iter()
            .any(|&n| n != UTIME_NOW && n != UTIME_OMIT);
        let time = |sec: i64, nsec: i64| match nsec {
            UTIME_OMIT => None,
            UTIME_NOW => Some(FileTime::Now),
            nsec => Some(FileTime::At(sec * 1_000_000_000 + nsec)),
        };
        (time(word(0), word(1)), time(word(2), word(3)), explicit)
    };
    // Only the owner can set given times; write permission also allows setting the current
    // time
    if target.checked(cx)? {
        let st = target.stat(cx)?;
        let cred = cx.cred();
        if !cred.owns(&st) {
            if explicit {
                return Err(Errno::EPERM);
            }
            if let Attrs::Fuse(id, _) = target {
                cx.vfs()?.fuse_permission(&cred, id, MAY_WRITE)?;
            } else {
                cred.permission(&st, MAY_WRITE)?;
            }
        }
    }
    let ino = match target {
        Attrs::Store(ino) => ino,
        Attrs::Overlay(o) => {
            let vfs = cx.vfs()?;
            copy_up(cx, &vfs, o, CopyUp::All)?
        }
        // A terminal's times are kept on the terminal (changeable, as with Linux's devpts)
        Attrs::Pts(t) => {
            let mut t = t.borrow_mut();
            let now = crate::tty::wall_ns();
            let ns = |time: FileTime| match time {
                FileTime::Now => now,
                FileTime::At(ns) => ns,
            };
            t.atime_ns = atime.map_or(t.atime_ns, ns);
            t.mtime_ns = mtime.map_or(t.mtime_ns, ns);
            t.changed();
            return Ok(0);
        }
        Attrs::Anon => return Ok(0),
        // Times of message queues and the mqueue root directory are kept in the kernel layer
        Attrs::Mq(q) => {
            let now = crate::tty::wall_ns();
            let ns = |time: FileTime| match time {
                FileTime::Now => now,
                FileTime::At(ns) => ns,
            };
            q.borrow_mut().set_times(atime.map(ns), mtime.map(ns));
            return Ok(0);
        }
        Attrs::MqDir(id) => {
            let now = crate::tty::wall_ns();
            let ns = |time: FileTime| match time {
                FileTime::Now => now,
                FileTime::At(ns) => ns,
            };
            let ipc = cx.kr().ipc(id);
            let mut d = ipc.mq_dir.borrow_mut();
            d.atime_ns = atime.map_or(d.atime_ns, ns);
            d.mtime_ns = mtime.map_or(d.mtime_ns, ns);
            d.ctime_ns = now;
            return Ok(0);
        }
        Attrs::Fuse(id, fh) => {
            let time = |t: FileTime| match t {
                FileTime::Now => None,
                FileTime::At(ns) => Some(ns),
            };
            let set = super::fuse::SetAttr {
                atime: atime.map(time),
                mtime: mtime.map(time),
                ..Default::default()
            };
            super::fuse::setattr(cx, &*cx.vfs()?, id, &set, fh)?;
            return Ok(0);
        }
    };
    // Regular files set by fd are set through the open object, which also works for files still
    // open after deletion
    let open = match (path.is_empty(), dirfd) {
        (true, fd) if fd != AT_FDCWD => Some(cx.fds().get(fd)?),
        _ => None,
    };
    let vfs = cx.vfs()?;
    // (not one opened through an overlay, whose open object may be the lower inode's)
    if let Some(f) = open.as_ref().map(|f| f.borrow())
        && let FileKind::Regular { file, .. } = &f.kind
        && f.ovl.is_none()
    {
        vfs.store().set_times_open(ino, file, atime, mtime)?;
    } else {
        vfs.store().set_times(ino, atime, mtime)?;
    }
    // fsnotify_change: both times are a utime call (IN_ATTRIB), the access time alone is
    // IN_ACCESS and the modification time alone IN_MODIFY
    let event = match (atime, mtime) {
        (Some(_), None) => inotify::IN_ACCESS,
        (None, Some(_)) => inotify::IN_MODIFY,
        _ => inotify::IN_ATTRIB,
    };
    if path.is_empty() {
        event_fd(cx, dirfd, event);
    } else {
        event_at(cx, ino, dirfd, &path, follow, event);
    }
    Ok(0)
}

/// Metadata changed by path: report IN_ATTRIB (also to the containing directory).
pub(super) fn attrib_at(cx: &Cx<'_>, ino: Ino, dirfd: i32, path: &[u8], follow: bool) {
    event_at(cx, ino, dirfd, path, follow, inotify::IN_ATTRIB);
}

/// Reports `event` for a change made by path to store inode `ino` (also to the containing
/// directory); through an overlay, to the overlay's node the path names.
fn event_at(cx: &Cx<'_>, ino: Ino, dirfd: i32, path: &[u8], follow: bool, event: u32) {
    if inotify::active(cx.kr()) {
        let (target, parent) = inotify::watched_at(cx, ino, (dirfd, path), follow);
        let parent = parent.as_ref().map(|(d, n)| (*d, n.as_slice()));
        let dir = match cx.vfs().map(|v| v.store().kind(ino)) {
            Ok(Ok(mode::S_IFDIR)) => inotify::IN_ISDIR,
            _ => 0,
        };
        inotify::path_event(cx.kr(), target, parent, event | dir);
    }
}

/// Metadata changed by fd: report IN_ATTRIB.
pub(super) fn attrib_fd(cx: &Cx<'_>, fd: i32) {
    event_fd(cx, fd, inotify::IN_ATTRIB);
}

/// Reports `event` for a change made through fd `fd`.
fn event_fd(cx: &Cx<'_>, fd: i32, event: u32) {
    if let Ok(file) = cx.fds().get(fd) {
        inotify::file_event(cx.kr(), &file.borrow(), event);
    }
}

// ---- truncate and fallocate ----

pub(super) fn truncate(cx: &mut Cx<'_>, path: u64, len: i64) -> SysResult {
    let len = u64::try_from(len).map_err(|_| Errno::EINVAL)?;
    let path = read_path(cx, path)?;
    let vfs = cx.vfs()?;
    let node = lookup_at(cx, AT_FDCWD, &path, true)?;
    if let Node::Fuse(..) = node {
        return super::fuse::truncate(cx, &node, len).map(|()| 0);
    }
    // A regular file without a name reached through /proc/<pid>/fd/N (deleted, or a memfd) is
    // truncated itself, as Linux follows the magic link to its dentry on the mount it was opened
    // through. A memfd's internal mount is never read-only, and its object is only its open
    // files' (Store::create_anonymous), so their handle is used
    let (node, memfd, nameless) = match node {
        Node::Pseudo(proc, PseudoNode::OpenFd(pid, fd)) => {
            let file = cx.view(&vfs).open_file(pid, fd).ok_or(Errno::ENOENT)?;
            let f = file.borrow();
            match &f.kind {
                FileKind::Regular {
                    ino, file: host, ..
                } if f.ovl.is_none() => {
                    let host = host.try_clone().map_err(host_errno)?;
                    (Node::Store(f.mnt, *ino), f.pseudo, Some(host))
                }
                _ => (Node::Pseudo(proc, PseudoNode::OpenFd(pid, fd)), false, None),
            }
        }
        node => (node, false, None),
    };
    let object = |ino| match (vfs.store().open_object(ino, true), &nameless) {
        (Err(Errno::ENOENT), Some(h)) => h.try_clone().map_err(host_errno),
        (r, _) => r,
    };
    // vfs_truncate: the type first (directories EISDIR, other non-regular files EINVAL), then a
    // read-only mount, then write permission
    if let Node::Pseudo(_, p) = &node {
        let st = pseudo::stat(&cx.view(&vfs), p)?;
        match st.kind() {
            mode::S_IFDIR => return Err(Errno::EISDIR),
            mode::S_IFREG => {}
            _ => return Err(Errno::EINVAL),
        }
        if vfs.readonly(&node) {
            return Err(Errno::EROFS);
        }
        cx.cred().permission(&st, MAY_WRITE)?;
        // Synthetic files have no size of their own to change (proc_setattr and kernfs accept
        // it and do nothing)
        return Ok(0);
    }
    // In an overlay the checks are made on what the overlay shows; then the file is copied up
    // (without its data when truncated to 0) and truncated there
    let node = match node {
        Node::Overlay(mnt, o) => {
            let real = vfs.overlay().node(o).real();
            let st = vfs.store().attrs(real)?;
            match st.kind() {
                mode::S_IFDIR => return Err(Errno::EISDIR),
                mode::S_IFREG => {}
                _ => return Err(Errno::EINVAL),
            }
            if vfs.readonly(&Node::Overlay(mnt, o)) {
                return Err(Errno::EROFS);
            }
            cx.cred().permission(&st, MAY_WRITE)?;
            if executing(cx, real) {
                return Err(Errno::ETXTBSY);
            }
            let what = if len == 0 {
                CopyUp::NoData
            } else {
                CopyUp::All
            };
            Node::Store(mnt, copy_up(cx, &vfs, o, what)?)
        }
        node => node,
    };
    let ino = node.ino().ok_or(Errno::EACCES)?;
    let st = vfs.store().attrs(ino)?;
    match st.kind() {
        mode::S_IFDIR => Err(Errno::EISDIR),
        mode::S_IFREG => {
            if !memfd && vfs.readonly(&node) {
                return Err(Errno::EROFS);
            }
            cx.cred().permission(&st, MAY_WRITE)?;
            if executing(cx, ino) {
                return Err(Errno::ETXTBSY);
            }
            check_fsize(cx, len)?;
            super::lease::break_leases(cx, ino, true, false, None)?;
            // A memfd's seals hold for truncation by path too (shmem_setattr)
            if cx.kr().seals.contains_key(&ino) {
                seals_permit(cx, ino, &object(ino)?, false, |_| len)?;
            }
            make_private(cx, ino, Some(len))?;
            remove_privs(cx, ino)?;
            let host = object(ino)?;
            let tmpfs = tmpfs_size(&vfs, &node, &host)?;
            truncating(cx, &host, len);
            host.set_len(len).map_err(host_errno)?;
            tmpfs_resized(&vfs, tmpfs, &host);
            if inotify::active(cx.kr()) {
                let (target, parent) = inotify::watched_at(cx, ino, (AT_FDCWD, &path), true);
                let parent = parent.as_ref().map(|(d, n)| (*d, n.as_slice()));
                inotify::path_event(cx.kr(), target, parent, inotify::IN_MODIFY);
            }
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn ftruncate(cx: &mut Cx<'_>, fd: i32, len: i64) -> SysResult {
    let len = u64::try_from(len).map_err(|_| Errno::EINVAL)?;
    let file = cx.fds().get(fd)?;
    if let Some(io) = super::fuse::file_io(&file) {
        if !file.borrow().writable() {
            return Err(Errno::EINVAL);
        }
        check_fsize(cx, len)?;
        return super::fuse::ftruncate(cx, &io, len).map(|()| 0);
    }
    // The borrow ends before the lease break, which can wait with the BKL released
    let leased = match &*file.borrow() {
        f @ OpenFile {
            kind: FileKind::Regular { ino, .. },
            ..
        } if f.writable() => Some(*ino),
        _ => None,
    };
    if let Some(ino) = leased {
        super::lease::break_leases(cx, ino, true, false, Some(&file))?;
    }
    prepare_write(cx, &file, Some(len))?;
    let f = file.borrow();
    match &f.kind {
        FileKind::Regular { ino, file, .. } if f.writable() => {
            check_fsize(cx, len)?;
            check_seals(cx, &f, false, |_| len)?;
            remove_privs(cx, *ino)?;
            let vfs = cx.vfs()?;
            let tmpfs = tmpfs_size(&vfs, &Node::Store(f.mnt, *ino), file)?;
            truncating(cx, file, len);
            file.set_len(len).map_err(host_errno)?;
            tmpfs_resized(&vfs, tmpfs, file);
            inotify::file_event(cx.kr(), &f, inotify::IN_MODIFY);
            Ok(0)
        }
        FileKind::Path(_) => Err(Errno::EBADF),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn fallocate(cx: &mut Cx<'_>, fd: i32, mode: u32, offset: i64, len: i64) -> SysResult {
    const FALLOC_FL_KEEP_SIZE: u32 = 0x1;
    const FALLOC_FL_PUNCH_HOLE: u32 = 0x2;
    const FALLOC_FL_ZERO_RANGE: u32 = 0x10;
    let (Ok(offset), Ok(len)) = (u64::try_from(offset), u64::try_from(len)) else {
        return Err(Errno::EINVAL);
    };
    if len == 0 {
        return Err(Errno::EINVAL);
    }
    // Beyond the largest file length (i64::MAX)
    let end = offset
        .checked_add(len)
        .filter(|&e| e <= i64::MAX as u64)
        .ok_or(Errno::EFBIG)?;
    let file = cx.fds().get(fd)?;
    if let Some(io) = super::fuse::file_io(&file) {
        if !file.borrow().writable() {
            return Err(Errno::EBADF);
        }
        return super::fuse::fallocate(cx, &io, mode, offset, len).map(|()| 0);
    }
    prepare_write(cx, &file, None)?;
    let f = file.borrow();
    if !f.writable() {
        return Err(Errno::EBADF);
    }
    let (ino, host) = match &f.kind {
        FileKind::Regular { ino, file, .. } => (*ino, file),
        FileKind::Directory { .. } => return Err(Errno::EISDIR),
        FileKind::Pipe(_) | FileKind::Socket(_) => return Err(Errno::ESPIPE),
        _ => return Err(Errno::ENODEV),
    };
    let size = host.metadata().map_err(host_errno)?.len();
    let keep = mode & FALLOC_FL_KEEP_SIZE != 0;
    let vfs = cx.vfs()?;
    let tmpfs = tmpfs_size(&vfs, &Node::Store(f.mnt, ino), host)?;
    // Store files stand in for ext4's, which can also zero a range; tmpfs only reserves and
    // punches (shmem_fallocate)
    let zero = match mode & !FALLOC_FL_KEEP_SIZE {
        0 => false,
        FALLOC_FL_PUNCH_HOLE if keep => true,
        FALLOC_FL_ZERO_RANGE if tmpfs.is_none() => true,
        _ => return Err(Errno::EOPNOTSUPP),
    };
    if !keep {
        check_fsize(cx, end)?;
    }
    check_seals(cx, &f, zero, |size| if keep { size } else { size.max(end) })?;
    // In tmpfs the pages of the range that have none must all fit (Linux's shmem_fallocate gets
    // them one by one and undoes them on ENOSPC); punching needs none
    if let Some((dev, own)) = tmpfs
        && mode & FALLOC_FL_PUNCH_HOLE == 0
    {
        let fits = || -> Result<bool, Errno> {
            let Some(room) = vfs.tmpfs_room(dev, own)? else {
                return Ok(true);
            };
            let free = room.saturating_sub(own) + reserved_unwritten(host, size, own);
            Ok(pages_needed(host, size, offset, end, free).is_ok())
        };
        // A recorded usage that may have drifted is counted again before refusing
        let room = fits()? || vfs.tmpfs_recount(dev)? && fits()?;
        if !room {
            return Err(Errno::ENOSPC);
        }
    }
    remove_privs(cx, ino)?;
    // Store objects and tmpfs files are host files: the host reserves, punches (freeing the
    // hole's storage) or zeroes; on a host filesystem that cannot, a hole or zeroed range is
    // written with zeros and a reservation only lengthens the file
    let fd = std::os::fd::AsRawFd::as_raw_fd(host);
    match tuxport_hal::fallocate(fd, mode as i32, offset, len) {
        Ok(()) => {}
        Err(e) if e == Errno::EOPNOTSUPP.0 => {
            if zero {
                let zeros = vec![0u8; IO_CHUNK];
                let mut pos = offset;
                while pos < end.min(size) {
                    let n = (end.min(size) - pos).min(IO_CHUNK as u64) as usize;
                    host.write_all_at(&zeros[..n], pos).map_err(host_errno)?;
                    pos += n as u64;
                }
            }
            if !keep && end > size {
                host.set_len(end).map_err(host_errno)?;
            }
        }
        Err(e) => return Err(Errno(e)),
    }
    tmpfs_resized(&vfs, tmpfs, host);
    inotify::file_event(cx.kr(), &f, inotify::IN_MODIFY);
    Ok(0)
}

/// The length after truncation or reserving space must not exceed `RLIMIT_FSIZE`.
pub(super) fn check_fsize(cx: &Cx<'_>, len: u64) -> Result<(), Errno> {
    if len > cx.t.group.borrow().rlimits[RLIMIT_FSIZE as usize].0 {
        return Err(file_too_big(cx));
    }
    Ok(())
}

/// Over `RLIMIT_FSIZE`: sends SIGXFSZ and returns EFBIG.
/// `RLIMIT_FSIZE` for a write of `len` bytes at `pos` to a file other than a store one (a FUSE
/// file): how many may be written, or `SIGXFSZ` and EFBIG when none (generic_write_checks).
pub(super) fn fsize_room(cx: &Cx<'_>, pos: u64, len: usize) -> Result<usize, Errno> {
    let limit = cx.t.group.borrow().rlimits[RLIMIT_FSIZE as usize].0;
    if limit == u64::MAX || len == 0 {
        return Ok(len);
    }
    if pos >= limit {
        return Err(file_too_big(cx));
    }
    Ok(len.min((limit - pos) as usize))
}

fn file_too_big(cx: &Cx<'_>) -> Errno {
    signal::send_to_task(cx.kr(), cx.task, Signal::XFSZ, SigInfo::kernel());
    Errno::EFBIG
}

// ---- Paths ----

/// Resolves the path argument of the `*at` family.
pub(super) fn lookup_at(cx: &Cx<'_>, dirfd: i32, path: &[u8], follow: bool) -> Result<Node, Errno> {
    resolve_at(cx, &cx.cred(), dirfd, path, follow)
}

/// The common part of "an empty path acts on dirfd itself" (`AT_EMPTY_PATH`) in the `*at`
/// family.
fn lookup_at_or_fd(
    cx: &Cx<'_>,
    cred: &Cred,
    dirfd: i32,
    path: &[u8],
    flags: u32,
    follow: bool,
) -> Result<Node, Errno> {
    if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        if dirfd == AT_FDCWD {
            return Ok(cx.cwd());
        }
        return cx.fds().get(dirfd)?.borrow().node().ok_or(Errno::EBADF);
    }
    resolve_at(cx, cred, dirfd, path, follow)
}

/// Resolves the path argument of the `*at` family with credentials `cred`.
fn resolve_at(
    cx: &Cx<'_>,
    cred: &Cred,
    dirfd: i32,
    path: &[u8],
    follow: bool,
) -> Result<Node, Errno> {
    let start = start_dir(cx, dirfd, path)?;
    let vfs = cx.vfs()?;
    vfs.resolve(&cx.view(&vfs), cred, &cx.root(), &start, path, follow)
}

/// Resolves a path to create or delete: returns the mount the parent directory was reached
/// through, the parent directory and the last component. Only directories in the store can be
/// modified; whether the mount is read-only is checked by the caller in Linux's order.
pub(super) fn parent_at<'p>(
    cx: &Cx<'_>,
    dirfd: i32,
    path: &'p [u8],
) -> Result<(MountId, Ino, &'p [u8]), Errno> {
    let start = start_dir(cx, dirfd, path)?;
    let vfs = cx.vfs()?;
    let (dir, name) = vfs.resolve_parent(&cx.view(&vfs), &cx.cred(), &cx.root(), &start, path)?;
    let (mnt, dir) = store_dir(&vfs, &dir)?;
    Ok((mnt, dir, name))
}

/// renameat2 in a synthetic filesystem (Linux's `do_renameat2` and `vfs_rename`, after the mount
/// and `.`/`..` checks): a read-only mount is EROFS, a missing source ENOENT (and in procfs a
/// missing destination too: its lookups fail rather than give a negative dentry), an existing
/// destination with `RENAME_NOREPLACE` EEXIST, a name renamed to itself succeeds without change;
/// anything else is refused as by [`store_dir`] (no synthetic directory renames), except that in
/// procfs, sysfs and devpts a caller who could delete there is told EPERM (no rename operation)
/// and others EACCES (`may_delete`).
fn rename_synthetic(
    cx: &Cx<'_>,
    (olddirfd, oldpath): (i32, &[u8]),
    (newdirfd, newpath): (i32, &[u8]),
    flags: u32,
    old_parent: &Node,
) -> SysResult {
    let vfs = cx.vfs()?;
    if vfs.readonly(old_parent) {
        return Err(Errno::EROFS);
    }
    let src = lookup_at(cx, olddirfd, oldpath, false)?;
    let dst = lookup_at(cx, newdirfd, newpath, false);
    if let Err(e) = &dst
        && vfs.fs_of(old_parent) == Fs::Proc
    {
        return Err(*e);
    }
    if flags & RENAME_NOREPLACE != 0 && dst.is_ok() {
        return Err(Errno::EEXIST);
    }
    if dst.is_ok_and(|d| d == src) {
        return Ok(0);
    }
    let Node::Pseudo(_, p) = old_parent else {
        return store_dir(&vfs, old_parent).map(|_| 0);
    };
    if matches!(p, PseudoNode::MqRoot(_) | PseudoNode::Cgroup(..)) {
        return Err(Errno::EPERM);
    }
    let st = pseudo::stat(&cx.view(&vfs), p)?;
    cx.cred().permission(&st, MAY_WRITE | MAY_EXEC)?;
    Err(Errno::EPERM)
}

/// Where a name is created ([`create_parent_at`]): the mount, the store directory, the name, and
/// the overlay directory it was reached as, if any.
pub(super) type CreateParent<'p> = (MountId, Ino, &'p [u8], Option<OvlId>);

/// The parent directory of a name to create ([`parent_at`]): in a directory of a synthetic
/// filesystem an existing name is EEXIST, ahead of the error that the directory cannot be
/// modified (Linux's `filename_create`: `mkdir -p /sys/fs/cgroup/x` relies on `mkdir /sys/fs`
/// being EEXIST). In an overlay it is the upper directory, copied up, and the overlay's directory
/// comes too (for [`created_events`]).
pub(super) fn create_parent_at<'p>(
    cx: &Cx<'_>,
    dirfd: i32,
    path: &'p [u8],
) -> Result<CreateParent<'p>, Errno> {
    let start = start_dir(cx, dirfd, path)?;
    let vfs = cx.vfs()?;
    let (dir, name) = vfs.resolve_parent(&cx.view(&vfs), &cx.cred(), &cx.root(), &start, path)?;
    if matches!(dir, Node::Pseudo(..) | Node::Overlay(..))
        && vfs.has_child(&cx.view(&vfs), &dir, name)
    {
        return Err(Errno::EEXIST);
    }
    let ovl = match dir {
        Node::Overlay(_, o) => Some(o),
        _ => None,
    };
    let (mnt, dir) = create_dir(&vfs, &dir)?;
    Ok((mnt, dir, name, ovl))
}

/// `name` was created in store directory `dir`: `IN_CREATE` (with `isdir`) to it, then to the
/// overlay directory `ovl` it was created through, if any (Linux's overlay creates the name in its
/// upper directory through the VFS, inside the creation through the overlay).
pub(super) fn created_events(
    k: &Kernel,
    (dir, ovl): (Ino, Option<OvlId>),
    name: &[u8],
    isdir: u32,
) {
    inotify::dir_event(k, dir, name, inotify::IN_CREATE | isdir, 0);
    if let Some(o) = ovl {
        inotify::dir_event(k, overlay::key(o), name, inotify::IN_CREATE | isdir, 0);
    }
}

/// Start directory of the `*at` family. An empty path is ENOENT at once (dirfd is not looked
/// at), as on Linux.
pub(super) fn start_dir(cx: &Cx<'_>, dirfd: i32, path: &[u8]) -> Result<Node, Errno> {
    if path.is_empty() {
        return Err(Errno::ENOENT);
    }
    if path.first() == Some(&b'/') {
        return Ok(cx.root());
    }
    if dirfd == AT_FDCWD {
        return Ok(cx.cwd());
    }
    let file = cx.fds().get(dirfd)?;
    let node = file.borrow().node().ok_or(Errno::ENOTDIR)?;
    let vfs = cx.vfs()?;
    if !vfs.is_dir(&cx.view(&vfs), &node)? {
        return Err(Errno::ENOTDIR);
    }
    Ok(node)
}

/// The directory to create or delete in: one in the store. Those in synthetic filesystems cannot
/// be modified, and are EROFS in a read-only mount. In the root directory of the mqueue filesystem
/// only queues can be created and deleted (openat and unlinkat handle them separately); everything
/// else (mkdir, mknod, symlink, link, rename) is EPERM as on Linux: its directory has no such
/// operations. cgroup2 directories are EPERM too (mkdirat and unlinkat handle child cgroups
/// before this).
fn store_dir(vfs: &Vfs, dir: &Node) -> Result<(MountId, Ino), Errno> {
    match dir {
        Node::Store(mnt, ino) => Ok((*mnt, *ino)),
        // Changing an overlay directory goes through its upper one ([`create_dir`]) or the
        // overlay's own removal and rename; never straight to a store inode
        // FUSE directories change through their daemon (`syscall::fuse`)
        Node::Overlay(..) | Node::Fuse(..) => Err(Errno::EXDEV),
        Node::Pseudo(..) if vfs.readonly(dir) => Err(Errno::EROFS),
        Node::Pseudo(_, PseudoNode::MqRoot(_) | PseudoNode::Cgroup(..)) => Err(Errno::EPERM),
        Node::Pseudo(..) => Err(Errno::EACCES),
    }
}

/// The store directory something is created in: a store directory as [`store_dir`] gives it,
/// or the upper directory of an overlay one, copied up first (EROFS in a read-only overlay).
fn create_dir(vfs: &Vfs, dir: &Node) -> Result<(MountId, Ino), Errno> {
    match dir {
        Node::Overlay(mnt, o) => {
            if vfs.readonly(dir) {
                return Err(Errno::EROFS);
            }
            Ok((*mnt, vfs.overlay_upper(*o, CopyUp::All)?))
        }
        _ => store_dir(vfs, dir),
    }
}

/// The queue with inode number `ino` in the mqueue filesystem of IPC namespace `ns`; a deleted
/// one is ENOENT.
fn mq_queue(cx: &Cx<'_>, ns: u64, ino: u64) -> Result<Rc<RefCell<super::MessageQueue>>, Errno> {
    super::mqueue::find(cx.kr().ipc(ns), ino)
        .map(|(_, q)| q.clone())
        .ok_or(Errno::ENOENT)
}

/// Checks before creating `name` in directory `dir` (`may_create`): [`may_create_name`], then the
/// directory must be writable. Returns the directory's metadata.
pub(super) fn may_create(
    vfs: &Vfs,
    cred: &Cred,
    dir: (MountId, Ino),
    name: &[u8],
) -> Result<Stat, Errno> {
    let st = may_create_name(vfs, cred, dir, name)?;
    cred.permission(&st, MAY_WRITE | MAY_EXEC)?;
    Ok(st)
}

/// Checks on the name and the mount before creating `name` in directory `dir` (Linux's
/// `filename_create`): the directory must be searchable and the name must not exist, and only
/// then comes the read-only mount (EROFS). Returns the directory's metadata.
fn may_create_name(
    vfs: &Vfs,
    cred: &Cred,
    (mnt, dir): (MountId, Ino),
    name: &[u8],
) -> Result<Stat, Errno> {
    let store = vfs.store();
    let st = store.stat(dir)?;
    cred.permission(&st, MAY_EXEC)?;
    if exists_in(vfs, (mnt, dir), name)? {
        return Err(Errno::EEXIST);
    }
    if vfs.readonly(&Node::Store(mnt, dir)) {
        return Err(Errno::EROFS);
    }
    Ok(st)
}

/// Before deleting or renaming: EROFS in a read-only mount (Linux's `mnt_want_write`, before the
/// name is looked up).
fn writable_dir(vfs: &Vfs, (mnt, dir): (MountId, Ino)) -> Result<(), Errno> {
    if vfs.readonly(&Node::Store(mnt, dir)) {
        return Err(Errno::EROFS);
    }
    Ok(())
}

/// What is being created is not a directory but its name ends in `/`: EEXIST when it exists,
/// otherwise ENOENT (as in Linux's `filename_create`).
fn reject_trailing_slash(
    vfs: &Vfs,
    dir: (MountId, Ino),
    name: &[u8],
    path: &[u8],
) -> Result<(), Errno> {
    if !path.ends_with(b"/") {
        return Ok(());
    }
    Err(if exists_in(vfs, dir, name)? {
        Errno::EEXIST
    } else {
        Errno::ENOENT
    })
}

/// Whether `name` exists in store directory `dir` of mount `mnt`; in an overlay's upper
/// directory a whiteout does not count (it hides a name, [`create_in`] replaces it).
fn exists_in(vfs: &Vfs, (mnt, dir): (MountId, Ino), name: &[u8]) -> Result<bool, Errno> {
    let store = vfs.store();
    match store.lookup(dir, name)? {
        Some(_) if vfs.fs_of(&Node::Store(mnt, dir)) == Fs::Overlay => {
            Ok(!crate::vfs::overlay::whiteout_at(store, dir, name)?)
        }
        found => Ok(found.is_some()),
    }
}

/// Creates `name` in store directory `dir` of mount `mnt` with `make`, which returns what `made`
/// takes the new inode from. In an overlay's upper directory a whiteout of the name is replaced
/// in the same transaction, and a directory made there is opaque, so what the lower layers have
/// under the name stays hidden (6.1's `ovl_create_over_whiteout`).
pub(super) fn create_in<T>(
    vfs: &Vfs,
    (mnt, dir): (MountId, Ino),
    name: &[u8],
    make: impl FnOnce() -> Result<T, Errno>,
    made: impl Fn(&T) -> Ino,
) -> Result<T, Errno> {
    let Some(prefix) = vfs.overlay_prefix(mnt) else {
        return make();
    };
    vfs.store().transaction(|s| {
        let whiteout = crate::vfs::overlay::whiteout_at(s, dir, name)?;
        if whiteout {
            s.unlink(dir, name)?;
        }
        let new = make()?;
        let ino = made(&new);
        if whiteout && s.kind(ino)? == mode::S_IFDIR {
            s.set_xattr(ino, &[prefix, b"opaque"].concat(), b"y", false, false)?;
        }
        Ok(new)
    })
}

/// Checks before deleting `name` in directory `dir` or moving it away (`may_delete`); returns its
/// metadata.
fn may_delete(store: &Store, cred: &Cred, dir: Ino, name: &[u8]) -> Result<Stat, Errno> {
    let dir_st = store.stat(dir)?;
    cred.permission(&dir_st, MAY_EXEC)?;
    let victim = store.stat(store.lookup(dir, name)?.ok_or(Errno::ENOENT)?)?;
    cred.may_delete(&dir_st, &victim)?;
    Ok(victim)
}

/// The store inode of the open file that fchmod, fchown and the like act on: not for files
/// opened with `O_PATH`, and EROFS in a read-only mount (Linux's `mnt_want_write_file`).
fn file_ino(cx: &Cx<'_>, fd: i32) -> Result<Ino, Errno> {
    let file = cx.fds().get(fd)?;
    let f = file.borrow();
    if f.flags & oflags::O_PATH != 0 {
        return Err(Errno::EBADF);
    }
    let node = f.node().ok_or(Errno::EBADF)?;
    let ino = node.ino().ok_or(Errno::EBADF)?;
    if cx.vfs()?.readonly(&node) {
        return Err(Errno::EROFS);
    }
    Ok(ino)
}

/// The store inode to modify: EROFS in a read-only mount; nodes in synthetic filesystems cannot
/// be modified (EACCES).
fn store_ino(cx: &Cx<'_>, node: &Node) -> Result<Ino, Errno> {
    if cx.vfs()?.readonly(node) {
        return Err(Errno::EROFS);
    }
    node.ino().ok_or(Errno::EACCES)
}

/// The absolute path recorded at open (`/proc/<pid>/fd/<n>`, maps): as with Linux's `d_path`,
/// symlinks are fully resolved, and it is computed from the directory and name `found` where the
/// file was last found. valgrind reopens Debian's `/lib/ld-linux-aarch64.so.1` (whose last
/// component is a link) by its name in `/proc/self/fd`, with `O_NOFOLLOW`. A file reached through
/// a link like `/proc/<pid>/fd/<n>`, whose name is unknown, gets the lexically normalized path.
fn display_path(
    cx: &Cx<'_>,
    vfs: &Vfs,
    start: &Node,
    path: &[u8],
    node: &Node,
    found: Option<&Dentry>,
) -> Option<Rc<[u8]>> {
    if let Ok(p) = vfs.found_path(&cx.view(vfs), &cx.root(), node, found) {
        return Some(Rc::from(p));
    }
    let full = if path.first() == Some(&b'/') {
        path.to_vec()
    } else {
        let mut p = vfs.path_of(&cx.view(vfs), &cx.root(), start).ok()?;
        p.push(b'/');
        p.extend_from_slice(path);
        p
    };
    Some(Rc::from(normalize(&full)))
}

// ---- tmpfs accounting ----

/// Before creating a name (a new inode, or a hard link) in directory `dir` (the mount reached
/// through and the inode): in tmpfs, checks whether it fits, ENOSPC when it does not (Linux's
/// `shmem_reserve_inode`; `pages` is the page a long symlink needs). Returns the instance's
/// device number, handed to [`tmpfs_created`] once created.
pub(super) fn tmpfs_reserve(
    vfs: &Vfs,
    (mnt, dir): (MountId, Ino),
    pages: u64,
) -> Result<Option<u64>, Errno> {
    let Some(dev) = vfs.tmpfs_dev(&Node::Store(mnt, dir)) else {
        return Ok(None);
    };
    vfs.tmpfs_check(dev, pages, 1)?;
    Ok(Some(dev))
}

/// A name [`tmpfs_reserve`] found room for has been created: accounts it.
pub(super) fn tmpfs_created(vfs: &Vfs, dev: Option<u64>, pages: u64) {
    if let Some(dev) = dev {
        vfs.tmpfs_charge(dev, pages as i64, 1);
    }
}

/// Writing a regular file in tmpfs: writes until the instance is full (Linux's
/// `shmem_write_begin` asks page by page and returns what was already written when it cannot get
/// one), ENOSPC when not even a page can be written; pages that already hold data need nothing.
/// Returns the number of bytes allowed, plus the device number and the page count before the
/// write that the accounting needs afterwards ([`tmpfs_resized`]).
fn tmpfs_write_room(
    cx: &Cx<'_>,
    f: &OpenFile,
    offset: Option<u64>,
    len: usize,
) -> Result<(usize, Option<(u64, u64)>), Errno> {
    let FileKind::Regular { ino, file, .. } = &f.kind else {
        return Ok((len, None));
    };
    let vfs = cx.vfs()?;
    let Some(t @ (dev, own)) = tmpfs_size(&vfs, &Node::Store(f.mnt, *ino), file)? else {
        return Ok((len, None));
    };
    if len == 0 || vfs.tmpfs_room(dev, own)?.is_none() {
        return Ok((len, Some(t)));
    }
    let size = file.metadata().map_err(host_errno)?.len();
    let pos = match offset {
        Some(off) => off,
        None if f.flags & oflags::O_APPEND != 0 => size,
        None => f.pos,
    };
    let end = pos.saturating_add(len as u64);
    loop {
        let room = vfs.tmpfs_room(dev, own)?.unwrap_or(u64::MAX);
        // Looking for holes only when the pages the write spans might not all fit
        let free = room.saturating_sub(own);
        if end.div_ceil(TMPFS_PAGE) - pos / TMPFS_PAGE <= free {
            return Ok((len, Some(t)));
        }
        let free = free + reserved_unwritten(file, size, own);
        return match pages_needed(file, size, pos, end, free) {
            Ok(_) => Ok((len, Some(t))),
            // Short of room: the recorded usage may have drifted, so count it again first
            Err(_) if vfs.tmpfs_recount(dev)? => continue,
            Err(stop) if stop <= pos => Err(Errno::ENOSPC),
            Err(stop) => Ok(((stop - pos) as usize, Some(t))),
        };
    }
}

/// How many pages of `[from, to)` in host file `host` (`size` bytes long) would need storage
/// when written: holes and pages past the end, found with the host's `SEEK_DATA` and
/// `SEEK_HOLE` (a page holding any data needs none; a host that cannot tell has data everywhere
/// before the end). When more than `budget` are needed, `Err` gives the offset of the first page
/// that no longer fits.
fn pages_needed(
    host: &std::fs::File,
    size: u64,
    from: u64,
    to: u64,
    budget: u64,
) -> Result<u64, u64> {
    let fd = std::os::fd::AsRawFd::as_raw_fd(host);
    let end = to.div_ceil(TMPFS_PAGE);
    let mut page = from / TMPFS_PAGE;
    let mut needed = 0;
    while page < end {
        let at = page * TMPFS_PAGE;
        let data = if at >= size {
            end
        } else {
            match tuxport_hal::lseek(fd, at as i64, SEEK_DATA as i32) {
                Ok(d) => d / TMPFS_PAGE,
                Err(e) if e == Errno::ENXIO.0 => end,
                Err(_) => page,
            }
        };
        let holes = data.min(end) - page;
        if needed + holes > budget {
            return Err((page + budget - needed) * TMPFS_PAGE);
        }
        needed += holes;
        if data >= end {
            break;
        }
        let hole = tuxport_hal::lseek(fd, (data * TMPFS_PAGE) as i64, SEEK_HOLE as i32)
            .unwrap_or(size)
            .div_ceil(TMPFS_PAGE);
        page = hole.max(data + 1);
    }
    Ok(needed)
}

/// How many of the `own` pages host file `host` (`size` bytes long) takes hold no data: space
/// reserved by fallocate and not written yet, which the host reports as holes although it is
/// allocated (Linux's tmpfs has those pages already). Writes there need no new pages, so they make
/// up for that many of the holes [`pages_needed`] finds.
fn reserved_unwritten(host: &std::fs::File, size: u64, own: u64) -> u64 {
    let fd = std::os::fd::AsRawFd::as_raw_fd(host);
    let mut data = 0;
    let mut at = 0;
    while at < size {
        let d = match tuxport_hal::lseek(fd, at as i64, SEEK_DATA as i32) {
            Ok(d) => d,
            Err(e) if e == Errno::ENXIO.0 => break,
            // A host that cannot tell: data everywhere
            Err(_) => {
                data += size.div_ceil(TMPFS_PAGE) - at / TMPFS_PAGE;
                break;
            }
        };
        let h = tuxport_hal::lseek(fd, d as i64, SEEK_HOLE as i32).unwrap_or(size);
        data += h.div_ceil(TMPFS_PAGE) - d / TMPFS_PAGE;
        at = h.max(d + 1);
    }
    own.saturating_sub(data)
}

/// When `node` (a regular file, `host` on the host) is in tmpfs, that instance's device number
/// and the pages the file's object takes: after it changes, they go to [`tmpfs_resized`] for
/// accounting.
fn tmpfs_size(vfs: &Vfs, node: &Node, host: &std::fs::File) -> Result<Option<(u64, u64)>, Errno> {
    let Some(dev) = vfs.tmpfs_dev(node) else {
        return Ok(None);
    };
    Ok(Some((dev, object_pages(host)?)))
}

/// The pages a regular file's object takes on the host (holes take none).
fn object_pages(host: &std::fs::File) -> Result<u64, Errno> {
    use std::os::unix::fs::MetadataExt;
    let blocks = host.metadata().map_err(host_errno)?.blocks();
    Ok(blocks.div_ceil(TMPFS_PAGE / 512))
}

/// A regular file in tmpfs was written, truncated, reserved or punched: accounts the difference
/// in pages ([`tmpfs_size`] recorded the old count).
fn tmpfs_resized(vfs: &Vfs, tmpfs: Option<(u64, u64)>, host: &std::fs::File) {
    if let Some((dev, own)) = tmpfs
        && let Ok(pages) = object_pages(host)
    {
        vfs.tmpfs_charge(dev, pages as i64 - own as i64, 0);
    }
}

/// Before deleting `name` in directory `dir` (unlink, rmdir, overwritten by rename): in tmpfs,
/// records what it takes: one inode for the name, plus its pages when the last link is deleted
/// (Linux frees them only at the last close; here they are freed at deletion). Handed to
/// [`tmpfs_removed`] after the deletion.
fn tmpfs_victim(
    vfs: &Vfs,
    (mnt, dir): (MountId, Ino),
    name: &[u8],
) -> Result<Option<(u64, u64)>, Errno> {
    let Some(dev) = vfs.tmpfs_dev(&Node::Store(mnt, dir)) else {
        return Ok(None);
    };
    let Some(ino) = vfs.store().lookup(dir, name)? else {
        return Ok(None);
    };
    let st = vfs.store().stat(ino)?;
    let pages = if st.is_dir() || st.nlink <= 1 {
        crate::vfs::tmpfs_pages(&st)
    } else {
        0
    };
    Ok(Some((dev, pages)))
}

/// The name [`tmpfs_victim`] recorded has been deleted: accounts it.
fn tmpfs_removed(vfs: &Vfs, victim: Option<(u64, u64)>) {
    if let Some((dev, pages)) = victim {
        vfs.tmpfs_charge(dev, -(pages as i64), -1);
    }
}

// ---- Shared helpers ----

/// The program file process `pid` is executing (`/proc/<pid>/exe`).
pub(super) fn exe_file(cx: &Cx<'_>, pid: i32) -> Result<Rc<crate::process::ExeFile>, Errno> {
    let group = pseudo::group_of(cx.kr(), pid)
        .cloned()
        .ok_or(Errno::ENOENT)?;
    group.borrow().exe.clone().ok_or(Errno::ENOENT)
}

/// Whether some process is executing the program `ino` in the store (opening it for write is
/// ETXTBSY).
pub(super) fn executing(cx: &Cx<'_>, ino: Ino) -> bool {
    cx.kr()
        .tasks
        .values()
        .any(|t| t.res.borrow().mm.borrow().exe == Some(ino))
}

/// Whether `ino` is open for write (executing it is ETXTBSY).
/// fds from memfd_create do not count ([`OpenFile::pseudo`]).
pub(super) fn open_for_write(cx: &Cx<'_>, ino: Ino) -> bool {
    cx.kr().open_files().iter().any(|f| {
        let f = f.borrow();
        f.writable()
            && !f.pseudo
            && match &f.kind {
                FileKind::Regular { ino: i, .. } => *i == ino,
                // A FUSE program carries its node's key ([`crate::process`]'s open_fuse)
                FileKind::Fuse(h) => crate::vfs::fuse::key(h.id) == ino,
                _ => false,
            }
    })
}

/// Writes (write, pwrite) and length changes (ftruncate) forbidden by seals return EPERM.
/// `grow_to` is the length after the write or truncation.
fn check_seals(
    cx: &Cx<'_>,
    f: &OpenFile,
    write: bool,
    grow_to: impl FnOnce(u64) -> u64,
) -> Result<(), Errno> {
    let FileKind::Regular { ino, file, .. } = &f.kind else {
        return Ok(());
    };
    seals_permit(cx, *ino, file, write, grow_to)
}

/// [`check_seals`] for regular file `ino`, whose host object `host` gives its size.
fn seals_permit(
    cx: &Cx<'_>,
    ino: Ino,
    host: &std::fs::File,
    write: bool,
    grow_to: impl FnOnce(u64) -> u64,
) -> Result<(), Errno> {
    let Some(&seals) = cx.kr().seals.get(&ino) else {
        return Ok(());
    };
    if write && seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
        return Err(Errno::EPERM);
    }
    let size = host.metadata().map_err(host_errno)?.len();
    let new = grow_to(size);
    if (new > size && seals & F_SEAL_GROW != 0) || (new < size && seals & F_SEAL_SHRINK != 0) {
        return Err(Errno::EPERM);
    }
    Ok(())
}

/// Removes the privileges of regular file `ino` before writing or truncating it
/// ([`Store::remove_privs`]): the setuid and setgid bits are removed when the caller lacks
/// `CAP_FSETID`, and file capabilities are always removed.
fn remove_privs(cx: &Cx<'_>, ino: Ino) -> Result<(), Errno> {
    let keep_setid = cx.cred().capable(cap::FSETID);
    cx.vfs()?.store().remove_privs(ino, keep_setid)
}

/// Called before a file is cut to `len` bytes: in every address space, pages mapping it beyond
/// the new end are removed from the page tables ([`crate::mm::Mm::file_truncated`]) and every
/// address space that had some syncs at once, the current one too, so no thread can still reach
/// that tail through a stale translation when the host file shrinks (the host would raise SIGBUS
/// in the engine, ending the process as a kernel bug). Afterwards an access there faults into the
/// kernel, waits for the BKL and gets the guest's SIGBUS. Nothing happens when `len` is not
/// shorter.
fn truncating(cx: &Cx<'_>, host: &std::fs::File, len: u64) {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = host.metadata() else {
        return;
    };
    if len >= meta.len() {
        return;
    }
    let sync = cx.task.sync.clone();
    let mut seen = Vec::new();
    for t in cx.kr().tasks.values() {
        let mm = t.res.borrow().mm.clone();
        if seen.contains(&Rc::as_ptr(&mm)) {
            continue;
        }
        seen.push(Rc::as_ptr(&mm));
        let Ok(mut m) = mm.try_borrow_mut() else {
            crate::debug::log(format_args!("truncate: an address space is busy"));
            continue;
        };
        if m.file_truncated(meta.dev(), meta.ino(), len) {
            m.sync(&sync);
        }
    }
}

fn host_errno(e: std::io::Error) -> Errno {
    Errno(e.raw_os_error().unwrap_or(Errno::EIO.0))
}
