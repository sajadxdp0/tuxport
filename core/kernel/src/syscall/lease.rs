//! File leases (`F_SETLEASE`, `F_GETLEASE`; the kernel/locks map).
//!
//! A lease hangs off an open file and is registered by inode in
//! [`Kernel::leases`](crate::kernel::Kernel). When another open file is about to open this inode in
//! a conflicting way or truncate it, the lease is broken: the holder gets a signal (SIGIO, or the
//! signal set by `F_SETSIG`), and the opener waits for it to downgrade or give up the lease, for at
//! most `/proc/sys/fs/lease-break-time` seconds, after which it is downgraded by force. Opening for
//! write conflicts with every lease (they must go down to none); opening for read conflicts only
//! with write leases (they go down to read leases).

use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use crate::cred::cap;
use crate::errno::Errno;
use crate::fd::{FileKind, FileRef, OpenFile};
use crate::kernel::{Cx, Kernel, Tid, request_wake_locks, wake};
use crate::process::Signal;
use crate::signal::{InfoFields, SigInfo};
use crate::vfs::store::Ino;

pub(super) const F_RDLCK: i16 = 0;
pub(super) const F_WRLCK: i16 = 1;
pub(super) const F_UNLCK: i16 = 2;

/// A lease.
pub(crate) struct Lease {
    file: Weak<crate::cell::RefCell<OpenFile>>,
    /// The fd used to set the lease: an `F_SETSIG` signal puts it in si_fd.
    fd: i32,
    /// `F_RDLCK` or `F_WRLCK`.
    ty: i16,
    /// Being broken: the type to downgrade to (`F_RDLCK` or `F_UNLCK`) and the deadline.
    breaking: Option<(i16, Instant)>,
    /// The process that set it (Linux's `lease_init` records the tgid, which a later change of
    /// type keeps), for `/proc/locks`.
    pid: Tid,
}

impl Lease {
    fn is(&self, file: &FileRef) -> bool {
        self.file.as_ptr() == Rc::as_ptr(file)
    }

    /// What `/proc/locks` shows ([`crate::locks::proc_locks`]): the open file while it lives, a
    /// write lease (or, while being broken, whether the type it goes down to is one, Linux's
    /// `target_leasetype`; None for `F_UNLCK`), whether it is being broken, and who set it.
    pub(crate) fn shown(&self) -> Option<(FileRef, Option<bool>, bool, Tid)> {
        let file = self.file.upgrade()?;
        let ty = self.breaking.map_or(self.ty, |(target, _)| target);
        let write = (ty != F_UNLCK).then_some(ty == F_WRLCK);
        Some((file, write, self.breaking.is_some(), self.pid))
    }
}

/// fcntl(`F_SETLEASE`).
pub(super) fn set(cx: &mut Cx<'_>, file: &FileRef, fd: i32, arg: i16) -> Result<(), Errno> {
    let (ino, writable) = {
        let f = file.borrow();
        (ino_of(&f).ok_or(Errno::EINVAL)?, f.writable())
    };
    if !matches!(arg, F_RDLCK | F_WRLCK | F_UNLCK) {
        return Err(Errno::EINVAL);
    }
    // Only the file's owner (or a holder of CAP_LEASE) may set a lease
    let st = cx.vfs()?.store().stat_open(
        ino,
        match &file.borrow().kind {
            FileKind::Regular { file, .. } => file,
            _ => unreachable!("checked above"),
        },
    )?;
    let cred = cx.cred();
    if cred.fsuid != st.uid && !cred.capable(cap::LEASE) {
        return Err(Errno::EACCES);
    }
    prune(cx.k(), ino);
    let k = cx.k();
    let list = k.leases.entry(ino).or_default();
    let mine = list.iter().position(|l| l.is(file));
    if arg == F_UNLCK {
        let i = mine.ok_or(Errno::EAGAIN)?;
        list.remove(i);
        prune(k, ino);
        request_wake_locks();
        return Ok(());
    }
    let others = list.iter().filter(|l| !l.is(file)).count();
    let (readers, writers) = opens(cx.kr(), ino, file);
    // Read lease: this file must not be writable, and nobody else may have it open for writing;
    // write lease: no other opens besides this one, and no other leases
    // While being broken by an open for writing it cannot be downgraded to a read lease: on Linux
    // the opener already has write access by then, which counts as "someone has it open for
    // writing"
    let breaking_to_none = mine.is_some_and(|i| {
        cx.kr().leases[&ino][i]
            .breaking
            .is_some_and(|(t, _)| t == F_UNLCK)
    });
    let conflict = match arg {
        F_RDLCK => {
            writable
                || writers > 0
                || breaking_to_none
                || cx.kr().leases[&ino]
                    .iter()
                    .any(|l| !l.is(file) && l.ty == F_WRLCK)
        }
        _ => readers + writers > 0 || others > 0,
    };
    if conflict {
        return Err(Errno::EAGAIN);
    }
    let tgid = cx.tgid();
    let k = cx.k();
    let list = k.leases.get_mut(&ino).unwrap();
    match mine {
        // Changing the type: a downgrade (e.g. a write lease being broken by an open for reading
        // going down to a read lease) completes the break
        Some(i) => {
            list[i].ty = arg;
            list[i].fd = fd;
            if list[i].breaking.is_some_and(|(target, _)| arg <= target) {
                list[i].breaking = None;
            }
            request_wake_locks();
        }
        None => {
            file.borrow_mut().holds_locks = true;
            list.push(Lease {
                file: Rc::downgrade(file),
                fd,
                ty: arg,
                breaking: None,
                pid: tgid,
            });
        }
    }
    // As on Linux (lease_setup's __f_setown without force), the signal for a break goes to the
    // process that set the lease, unless the file already has an owner
    let cred = cx.cred();
    let mut f = file.borrow_mut();
    if f.owner.1 == 0 {
        f.owner = (1, tgid);
        f.owner_ids = (cred.ruid, cred.euid);
    }
    Ok(())
}

/// How many open files, other than `except`, have `ino` open for reading and for writing (across
/// all processes).
fn opens(k: &Kernel, ino: Ino, except: &FileRef) -> (usize, usize) {
    let mut files: Vec<FileRef> = Vec::new();
    for f in k.open_files() {
        if !Rc::ptr_eq(&f, except) && !files.iter().any(|g| Rc::ptr_eq(g, &f)) {
            files.push(f);
        }
    }
    let mine: Vec<_> = files
        .iter()
        .map(|f| f.borrow())
        .filter(|f| ino_of(f) == Some(ino))
        .map(|f| f.writable())
        .collect();
    let writers = mine.iter().filter(|&&w| w).count();
    (mine.len() - writers, writers)
}

/// fcntl(`F_GETLEASE`): while being broken, the type it is to be downgraded to.
pub(super) fn get(cx: &mut Cx<'_>, file: &FileRef) -> i16 {
    let Some(ino) = ino_of(&file.borrow()) else {
        return F_UNLCK;
    };
    prune(cx.k(), ino);
    cx.kr()
        .leases
        .get(&ino)
        .and_then(|list| list.iter().find(|l| l.is(file)))
        .map_or(F_UNLCK, |l| l.breaking.map_or(l.ty, |(target, _)| target))
}

/// About to open or truncate `ino` for writing (`write`) or reading: breaks the conflicting leases
/// and waits for the holders to give way. `own` is our own open file when truncating, whose lease
/// does not count as a conflict. With `nonblock`, only notifies the holders and returns
/// EWOULDBLOCK (EAGAIN).
pub(super) fn break_leases(
    cx: &mut Cx<'_>,
    ino: Ino,
    write: bool,
    nonblock: bool,
    own: Option<&FileRef>,
) -> Result<(), Errno> {
    if cx.kr().leases.is_empty() {
        return Ok(());
    }
    prune(cx.k(), ino);
    let target = if write { F_UNLCK } else { F_RDLCK };
    let conflicts = |k: &Kernel| {
        k.leases.get(&ino).is_some_and(|list| {
            list.iter()
                .any(|l| own.is_none_or(|f| !l.is(f)) && (write || l.ty == F_WRLCK))
        })
    };
    if !conflicts(cx.kr()) {
        return Ok(());
    }
    // Notify the holders that are not being broken yet
    let secs = cx.kr().lease_break_time;
    let deadline = Instant::now() + Duration::from_secs(secs).min(super::KTIME_MAX);
    let mut notify = Vec::new();
    if let Some(list) = cx.k().leases.get_mut(&ino) {
        for l in list.iter_mut() {
            if own.is_some_and(|f| l.is(f)) || !(write || l.ty == F_WRLCK) {
                continue;
            }
            match &mut l.breaking {
                Some((t, _)) => *t = (*t).max(target),
                None => {
                    l.breaking = Some((target, deadline));
                    notify.push((l.file.clone(), l.fd));
                }
            }
        }
    }
    for (file, fd) in notify {
        let Some(file) = file.upgrade() else {
            continue;
        };
        let (owner, ids, sig) = {
            let f = file.borrow();
            (f.owner, f.owner_ids, f.sig)
        };
        let Some(signo) = Signal::new(if sig == 0 { 29 } else { sig }) else {
            continue;
        };
        // POLL_MSG, si_band is POLLIN | POLLRDNORM | POLLMSG
        let info = if sig == 0 {
            SigInfo::kernel()
        } else {
            SigInfo {
                code: 3,
                fields: InfoFields::Poll { band: 0x441, fd },
                ..SigInfo::default()
            }
        };
        super::poll::send_to_owner(cx.kr(), owner, ids, signo, info);
    }
    if nonblock {
        return Err(Errno::EAGAIN);
    }
    loop {
        prune(cx.k(), ino);
        if !conflicts(cx.kr()) {
            return Ok(());
        }
        // Forced downgrades once the deadline has passed
        let now = Instant::now();
        let mut next = None::<Instant>;
        if let Some(list) = cx.k().leases.get_mut(&ino) {
            list.retain_mut(|l| match l.breaking {
                Some((t, d)) if d <= now => {
                    l.breaking = None;
                    if t == F_UNLCK {
                        return false;
                    }
                    l.ty = t;
                    true
                }
                Some((_, d)) => {
                    next = Some(next.map_or(d, |n| n.min(d)));
                    true
                }
                None => true,
            });
        }
        prune(cx.k(), ino);
        if !conflicts(cx.kr()) {
            return Ok(());
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        // Holders request wake::LOCK when they downgrade, give up the lease or close the file
        cx.wait_on(next, &mut [], wake::LOCK)?;
    }
}

/// The inode of a regular file.
fn ino_of(f: &OpenFile) -> Option<Ino> {
    match f.kind {
        FileKind::Regular { ino, .. } => Some(ino),
        _ => None,
    }
}

/// Drops the leases of open files that have been closed.
fn prune(k: &mut Kernel, ino: Ino) {
    if let Some(list) = k.leases.get_mut(&ino) {
        list.retain(|l| l.file.strong_count() > 0);
        if list.is_empty() {
            k.leases.remove(&ino);
        }
    }
}
