//! POSIX message queues: mq_open, mq_unlink, mq_timedsend, mq_timedreceive, mq_notify,
//! mq_getsetattr.
//!
//! Queues are registered by name in their IPC namespace ([`crate::ipc::Ipc::mqueues`]); opening one
//! gives an fd that can be polled (readable when there are messages, writable when not full), and
//! reading it gives the same status line as on Linux. The mqueue filesystem (`mount -t mqueue`,
//! `MqRoot` and `Mq` in `crate::pseudo`) lists the same table as a directory: open(O_CREAT) there
//! creates these queues, unlink removes them, chmod and chown change them, and opening one gives
//! the same fd as mq_open. Messages are ordered by priority, first in first out within a priority.
//! mq_notify notifies once when a message arrives in an empty queue and nobody is waiting to
//! receive it: it sends a signal, or delivers the 32-byte cookie given at registration to a
//! netlink socket (which is how musl implements `SIGEV_THREAD`). A registration also ends when its
//! process cancels it or closes any descriptor of the queue (exit included); a `SIGEV_THREAD` one
//! then gets the cookie marked removed, which ends musl's helper thread. Limits follow
//! `/proc/sys/fs/mqueue/*`.

use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{SysResult, get, put, read_timespec};
use crate::cell::RefCell;
use crate::cred::{Cred, MAY_READ, MAY_WRITE, cap};
use crate::errno::Errno;
use crate::fd::{FileKind, OpenFile, oflags};
use crate::kernel::{Cx, Tid, request_wake_object, wake};
use crate::net::Socket;
use crate::process::Signal;
use crate::signal::{self, InfoFields, SigInfo};
use crate::uaccess::UserSlice;
use crate::vfs::store::{Stat, mode};

/// Upper bound of priorities (exclusive).
const MQ_PRIO_MAX: u32 = 32768;
/// The limits with `CAP_SYS_RESOURCE` (Linux's HARD_MSGMAX, HARD_MSGSIZEMAX).
const HARD_MSGMAX: u64 = 65536;
const HARD_MSGSIZEMAX: u64 = 16 << 20;
const NAME_MAX: usize = 255;

const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD: i32 = 2;
/// The si_code of message queue notifications.
const SI_MESGQ: i32 = -3;
/// The last byte of a netlink notification cookie: a message arrived, or the registration ended.
const NOTIFY_WOKENUP: u8 = 1;
const NOTIFY_REMOVED: u8 = 2;

/// `/proc/sys/fs/mqueue/*`.
pub(crate) struct Limits {
    pub queues_max: u64,
    pub msg_max: u64,
    pub msgsize_max: u64,
    pub msg_default: u64,
    pub msgsize_default: u64,
}

impl Limits {
    /// The value of `/proc/sys/fs/mqueue/<name>`.
    pub fn get(&self, name: &[u8]) -> Option<u64> {
        Some(match name {
            b"queues_max" => self.queues_max,
            b"msg_max" => self.msg_max,
            b"msgsize_max" => self.msgsize_max,
            b"msg_default" => self.msg_default,
            b"msgsize_default" => self.msgsize_default,
            _ => return None,
        })
    }

    /// Writes `/proc/sys/fs/mqueue/<name>`.
    pub fn slot(&mut self, name: &[u8]) -> Option<&mut u64> {
        Some(match name {
            b"queues_max" => &mut self.queues_max,
            b"msg_max" => &mut self.msg_max,
            b"msgsize_max" => &mut self.msgsize_max,
            b"msg_default" => &mut self.msg_default,
            b"msgsize_default" => &mut self.msgsize_default,
            _ => return None,
        })
    }
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            queues_max: 256,
            msg_max: 10,
            msgsize_max: 8192,
            msg_default: 10,
            msgsize_default: 8192,
        }
    }
}

/// An mq_notify registration.
enum Notify {
    Signal {
        tgid: Tid,
        sig: Signal,
        value: u64,
    },
    /// Does nothing, only holding the notification (`SIGEV_NONE`).
    None {
        tgid: Tid,
    },
    /// Delivers the cookie to a netlink socket (`SIGEV_THREAD`).
    Socket {
        tgid: Tid,
        sock: Weak<RefCell<Socket>>,
        cookie: [u8; 32],
    },
}

impl Notify {
    fn tgid(&self) -> Tid {
        match self {
            Notify::Signal { tgid, .. } | Notify::None { tgid } | Notify::Socket { tgid, .. } => {
                *tgid
            }
        }
    }
}

/// Queue inode numbers (Linux's `get_next_ino`), numbered across all namespaces together.
static NEXT_INO: AtomicU64 = AtomicU64::new(1);

/// The size of one directory entry and of one queue file in the mqueue filesystem (Linux's
/// DIRENT_SIZE, FILENT_SIZE): the root directory reports 2 + the number of queues entries.
const DIRENT_SIZE: u64 = 20;
const FILENT_SIZE: u64 = 80;

/// The root directory of the mqueue filesystem in one IPC namespace: owned by the process that set
/// up the namespace (root for the one at boot), mode 01777, its times updated when queues are
/// created or removed. It can be chmod'ed and chown'ed.
pub(crate) struct MqDir {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub atime_ns: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl MqDir {
    pub fn new(uid: u32, gid: u32) -> MqDir {
        let now = crate::tty::wall_ns();
        MqDir {
            uid,
            gid,
            mode: mode::S_ISVTX | 0o777,
            atime_ns: now,
            mtime_ns: now,
            ctime_ns: now,
        }
    }

    /// stat: `queues` is the number of queues in it.
    pub fn stat(&self, queues: usize) -> Stat {
        Stat {
            ino: 0,
            mode: mode::S_IFDIR | self.mode,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            size: (2 + queues as u64) * DIRENT_SIZE,
            atime_ns: self.atime_ns,
            mtime_ns: self.mtime_ns,
            ctime_ns: self.ctime_ns,
            blocks: None,
        }
    }

    fn touch(&mut self) {
        let now = crate::tty::wall_ns();
        (self.atime_ns, self.mtime_ns, self.ctime_ns) = (now, now, now);
    }
}

impl Default for MqDir {
    fn default() -> MqDir {
        MqDir::new(0, 0)
    }
}

/// A message queue.
pub struct MessageQueue {
    /// Inode number: nodes in the mqueue filesystem find their queue by it.
    pub ino: u64,
    /// The number of the IPC namespace it is in (fstat's device number follows it).
    pub ns: u64,
    uid: u32,
    gid: u32,
    mode: u32,
    atime_ns: i64,
    mtime_ns: i64,
    ctime_ns: i64,
    maxmsg: usize,
    msgsize: usize,
    /// (priority, contents), higher priorities first, and within a priority earlier arrivals first.
    messages: VecDeque<(u32, Vec<u8>)>,
    notify: Option<Notify>,
    /// The number of threads waiting to receive: no notification is sent while someone is waiting.
    receivers: usize,
    /// Messages sent and received: as on Linux, each one fires the other side's `EPOLLET` again.
    sent: u64,
    received: u64,
}

impl MessageQueue {
    /// Messages sent and received so far (`syscall::poll`'s data generation).
    pub fn arrivals(&self) -> [u64; 2] {
        [self.sent, self.received]
    }

    /// poll readiness events.
    pub fn readiness(&self) -> i16 {
        const POLLIN: i16 = 0x001;
        const POLLOUT: i16 = 0x004;
        const POLLRDNORM: i16 = 0x040;
        const POLLWRNORM: i16 = 0x100;
        let mut ev = 0;
        if !self.messages.is_empty() {
            ev |= POLLIN | POLLRDNORM;
        }
        if self.messages.len() < self.maxmsg {
            ev |= POLLOUT | POLLWRNORM;
        }
        ev
    }

    /// The status line read returns, the same as Linux's mqueue filesystem.
    pub fn status(&self) -> Vec<u8> {
        let qsize: usize = self.messages.iter().map(|(_, m)| m.len()).sum();
        let (notify, signo, pid) = match &self.notify {
            Some(Notify::Signal { tgid, sig, .. }) => (SIGEV_SIGNAL, sig.number(), *tgid),
            Some(Notify::None { tgid }) => (SIGEV_NONE, 0, *tgid),
            Some(Notify::Socket { tgid, .. }) => (SIGEV_THREAD, 0, *tgid),
            None => (0, 0, 0),
        };
        format!("QSIZE:{qsize:<10} NOTIFY:{notify:<5} SIGNO:{signo:<5} NOTIFY_PID:{pid:<6}\n")
            .into_bytes()
    }

    /// fstat: a regular file in the mqueue filesystem.
    pub fn stat(&self) -> Stat {
        Stat {
            ino: self.ino,
            mode: mode::S_IFREG | self.mode,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            size: FILENT_SIZE,
            atime_ns: self.atime_ns,
            mtime_ns: self.mtime_ns,
            ctime_ns: self.ctime_ns,
            blocks: None,
        }
    }

    /// The status line was read, a notification registered or cancelled, or the flags changed:
    /// atime and ctime are now.
    pub fn touch_read(&mut self) {
        let now = crate::tty::wall_ns();
        (self.atime_ns, self.ctime_ns) = (now, now);
    }

    /// A message was sent or received: all three times are now.
    fn touch_all(&mut self) {
        let now = crate::tty::wall_ns();
        (self.atime_ns, self.mtime_ns, self.ctime_ns) = (now, now, now);
    }

    /// chmod: changes only the permission bits (there is no file type, setuid and the like).
    pub fn set_mode(&mut self, perm: u32) {
        self.mode = perm & 0o7777;
        self.ctime_ns = crate::tty::wall_ns();
    }

    /// chown: the ones that are None are left unchanged.
    pub fn set_owner(&mut self, uid: Option<u32>, gid: Option<u32>) {
        if let Some(uid) = uid {
            self.uid = uid;
        }
        if let Some(gid) = gid {
            self.gid = gid;
        }
        self.ctime_ns = crate::tty::wall_ns();
    }

    /// utimensat: the ones that are None are left unchanged; ctime is now.
    pub fn set_times(&mut self, atime: Option<i64>, mtime: Option<i64>) {
        if let Some(t) = atime {
            self.atime_ns = t;
        }
        if let Some(t) = mtime {
            self.mtime_ns = t;
        }
        self.ctime_ns = crate::tty::wall_ns();
    }
}

pub(super) fn mq_open(cx: &mut Cx<'_>, name: u64, flags: u32, perm: u32, attr: u64) -> SysResult {
    let name = queue_name(cx, name)?;
    let accmode = flags & oflags::O_ACCMODE;
    if accmode == oflags::O_ACCMODE {
        return Err(Errno::EINVAL);
    }
    let cred = cx.cred();
    let existing = cx.ipc().mqueues.get(&name).cloned();
    let queue = match existing {
        Some(_)
            if flags & (oflags::O_CREAT | oflags::O_EXCL) == oflags::O_CREAT | oflags::O_EXCL =>
        {
            return Err(Errno::EEXIST);
        }
        Some(q) => {
            let st = q.borrow().stat();
            let mask = match accmode {
                oflags::O_RDONLY => MAY_READ,
                oflags::O_WRONLY => MAY_WRITE,
                _ => MAY_READ | MAY_WRITE,
            };
            cred.permission(&st, mask)?;
            q
        }
        None if flags & oflags::O_CREAT == 0 => return Err(Errno::ENOENT),
        None => {
            let privileged = cred.capable(cap::SYS_RESOURCE);
            let mut size = None;
            if attr != 0 {
                let limits = &cx.ipc().mq_limits;
                let b: [u8; 64] = get(cx, attr)?;
                let field = |i: usize| i64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
                let (m, s) = (field(1), field(2));
                let (cap_m, cap_s) = if privileged {
                    (HARD_MSGMAX, HARD_MSGSIZEMAX)
                } else {
                    (limits.msg_max, limits.msgsize_max)
                };
                if m <= 0 || s <= 0 || m as u64 > cap_m || s as u64 > cap_s {
                    return Err(Errno::EINVAL);
                }
                size = Some((m as u64, s as u64));
            }
            let ns = cx.ns().ipc.id;
            new_queue(cx.ipc(), ns, &cred, perm & !cx.umask(), size)?
        }
    };
    let path: Rc<[u8]> = Rc::from([b"/".as_slice(), &name].concat());
    let file = OpenFile::with_path(
        FileKind::MessageQueue(queue.clone()),
        accmode | (flags & oflags::O_NONBLOCK),
        Some(path),
    );
    // Always close-on-exec, as on Linux; when out of fds (EMFILE) the new queue is not left behind
    let fd = cx.fds_mut().install(file, true, 0)?;
    insert_queue(cx.ipc_mut(), name, queue);
    Ok(fd as u64)
}

/// Creates queue `name` in IPC namespace `ns` (whose objects are `ipc`) (Linux's
/// `mqueue_create_attr`): owned by the caller's fsuid and fsgid, and `perm` has the umask removed
/// already; `size` is the limit of (message count, message length), with `msg_default` and
/// `msgsize_default` when None. When the queue count has reached `queues_max`, it is ENOSPC without
/// `CAP_SYS_RESOURCE`. Not registered in the name table yet (mq_open registers it only after
/// installing the fd). The defaults are capped by `msg_max` and `msgsize_max`
/// (`mqueue_get_inode`).
pub(crate) fn new_queue(
    ipc: &crate::ipc::Ipc,
    ns: u64,
    cred: &Cred,
    perm: u32,
    size: Option<(u64, u64)>,
) -> Result<Rc<RefCell<MessageQueue>>, Errno> {
    let limits = &ipc.mq_limits;
    if ipc.mqueues.len() as u64 >= limits.queues_max && !cred.capable(cap::SYS_RESOURCE) {
        return Err(Errno::ENOSPC);
    }
    let (maxmsg, msgsize) = size.unwrap_or((
        limits.msg_default.min(limits.msg_max),
        limits.msgsize_default.min(limits.msgsize_max),
    ));
    let now = crate::tty::wall_ns();
    Ok(Rc::new(RefCell::new(MessageQueue {
        ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
        ns,
        uid: cred.fsuid,
        gid: cred.fsgid,
        mode: perm & 0o777,
        atime_ns: now,
        mtime_ns: now,
        ctime_ns: now,
        maxmsg: maxmsg as usize,
        msgsize: msgsize as usize,
        messages: VecDeque::new(),
        notify: None,
        receivers: 0,
        sent: 0,
        received: 0,
    })))
}

/// Registers a newly created queue in the name table, updating the root directory's times with it.
pub(crate) fn insert_queue(ipc: &mut crate::ipc::Ipc, name: Vec<u8>, q: Rc<RefCell<MessageQueue>>) {
    ipc.mqueues.entry(name).or_insert(q);
    ipc.mq_dir.borrow_mut().touch();
}

pub(super) fn mq_unlink(cx: &mut Cx<'_>, name: u64) -> SysResult {
    let name = queue_name(cx, name)?;
    let cred = cx.cred();
    remove_queue(cx.ipc_mut(), &cred, &name)?;
    Ok(0)
}

/// Removes queue `name` (mq_unlink, and unlink in the mqueue filesystem, both Linux's
/// `vfs_unlink`): the root directory has the sticky bit, so the caller must own the queue or the
/// root directory, or have `CAP_FOWNER` over the queue; otherwise EPERM (libc's mq_unlink turns
/// that into EACCES).
pub(crate) fn remove_queue(
    ipc: &mut crate::ipc::Ipc,
    cred: &Cred,
    name: &[u8],
) -> Result<(), Errno> {
    let q = ipc.mqueues.get(name).ok_or(Errno::ENOENT)?;
    let st = q.borrow().stat();
    let dir_uid = ipc.mq_dir.borrow().uid;
    if cred.fsuid != st.uid && cred.fsuid != dir_uid && !cred.capable_wrt(&st, cap::FOWNER) {
        return Err(Errno::EPERM);
    }
    ipc.mqueues.remove(name);
    ipc.mq_dir.borrow_mut().touch();
    Ok(())
}

/// Strips the leading `/` from the name (musl has already stripped it) and checks length and
/// characters.
fn queue_name(cx: &Cx<'_>, addr: u64) -> Result<Vec<u8>, Errno> {
    let name = cx.mm().read_cstr(addr, 4096)?;
    let name = name.strip_prefix(b"/").unwrap_or(&name).to_vec();
    if name.len() > NAME_MAX {
        return Err(Errno::ENAMETOOLONG);
    }
    if name.is_empty() {
        return Err(Errno::ENOENT);
    }
    if name.contains(&b'/') || name == b"." || name == b".." {
        return Err(Errno::EACCES);
    }
    Ok(name)
}

pub(super) fn mq_timedsend(
    cx: &mut Cx<'_>,
    fd: i32,
    msg: u64,
    len: usize,
    prio: u32,
    timeout: u64,
) -> SysResult {
    let (q, flags) = queue_of(cx, fd)?;
    if flags & oflags::O_ACCMODE == oflags::O_RDONLY {
        return Err(Errno::EBADF);
    }
    if prio >= MQ_PRIO_MAX {
        return Err(Errno::EINVAL);
    }
    if len > q.borrow().msgsize {
        return Err(Errno::EMSGSIZE);
    }
    let mut data = vec![0u8; len];
    cx.mm().read(UserSlice::new(msg, len), &mut data)?;
    let deadline = deadline(cx, timeout)?;
    loop {
        if q.borrow().messages.len() < q.borrow().maxmsg {
            break;
        }
        if flags & oflags::O_NONBLOCK != 0 {
            return Err(Errno::EAGAIN);
        }
        if deadline.is_some_and(crate::itimer::Expiry::passed) {
            return Err(Errno::ETIMEDOUT);
        }
        // Restarted after a handler with SA_RESTART or a stop, as Linux's wq_sleep; the deadline is
        // absolute, so a restart keeps it
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        // Taking a message requests a wake for this queue
        cx.wait_objects(
            deadline.map(crate::itimer::Expiry::instant),
            &mut [],
            wake::FILE,
            &[wake::of(&q)],
        )?;
    }
    let notify = {
        let mut q = q.borrow_mut();
        let was_empty = q.messages.is_empty();
        let at = q
            .messages
            .iter()
            .position(|(p, _)| *p < prio)
            .unwrap_or(q.messages.len());
        q.messages.insert(at, (prio, data));
        q.sent += 1;
        q.touch_all();
        // A message arrived in an empty queue and nobody is waiting to receive it: notify once, and
        // the registration is cancelled with it
        if was_empty && q.receivers == 0 {
            q.notify.take()
        } else {
            None
        }
    };
    match notify {
        Some(Notify::Signal { tgid, sig, value }) => {
            // si_pid and si_uid are as the registering process sees them (Linux's __do_notify
            // uses the user namespace at registration; here its current one is used), and after
            // that, like any other signal, they are translated once more as sent by the sender
            let cred = cx.cred();
            let uid = cx
                .kr()
                .groups
                .get(&tgid)
                .map_or(cred.ruid, |g| g.borrow().cred.uid_out(cred.ruid));
            let info = SigInfo {
                code: SI_MESGQ,
                fields: InfoFields::Queue {
                    pid: cx.kr().pid_seen_by(tgid, cx.tgid()),
                    uid,
                    value,
                },
                sender: cx.tgid(),
                ..SigInfo::default()
            }
            .sent_by(&cred);
            signal::send_to_group(cx.kr(), tgid, sig, info);
        }
        Some(Notify::Socket { sock, cookie, .. }) => send_cookie(&sock, cookie, NOTIFY_WOKENUP),
        _ => {}
    }
    // Wake only those waiting for this queue (poll, epoll, blocking receives): this used to wake
    // all waiters
    request_wake_object(wake::of(&q));
    Ok(0)
}

pub(super) fn mq_timedreceive(
    cx: &mut Cx<'_>,
    fd: i32,
    msg: u64,
    len: usize,
    prio: u64,
    timeout: u64,
) -> SysResult {
    let (q, flags) = queue_of(cx, fd)?;
    if flags & oflags::O_ACCMODE == oflags::O_WRONLY {
        return Err(Errno::EBADF);
    }
    if len < q.borrow().msgsize {
        return Err(Errno::EMSGSIZE);
    }
    let deadline = deadline(cx, timeout)?;
    let (p, data) = loop {
        let mut queue = q.borrow_mut();
        if let Some(m) = queue.messages.pop_front() {
            queue.received += 1;
            queue.touch_all();
            break m;
        }
        drop(queue);
        if flags & oflags::O_NONBLOCK != 0 {
            return Err(Errno::EAGAIN);
        }
        if deadline.is_some_and(crate::itimer::Expiry::passed) {
            return Err(Errno::ETIMEDOUT);
        }
        // Restarted after a handler with SA_RESTART or a stop, as Linux's wq_sleep; the deadline is
        // absolute, so a restart keeps it
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        q.borrow_mut().receivers += 1;
        // A message arriving requests a wake for this queue
        let r = cx.wait_objects(
            deadline.map(crate::itimer::Expiry::instant),
            &mut [],
            wake::FILE,
            &[wake::of(&q)],
        );
        q.borrow_mut().receivers -= 1;
        r?;
    };
    request_wake_object(wake::of(&q));
    cx.mm().write(UserSlice::new(msg, data.len()), &data)?;
    if prio != 0 {
        put(cx, prio, p.to_le_bytes())?;
    }
    Ok(data.len() as u64)
}

/// Absolute timeout (`CLOCK_REALTIME`, so it moves when the wall clock is set); 0 means wait
/// forever.
fn deadline(cx: &Cx<'_>, timeout: u64) -> Result<Option<crate::itimer::Expiry>, Errno> {
    if timeout == 0 {
        return Ok(None);
    }
    let t = read_timespec(cx, timeout)?;
    crate::itimer::Expiry::new(0, t, true).map(Some)
}

pub(super) fn mq_notify(cx: &mut Cx<'_>, fd: i32, sev: u64) -> SysResult {
    // As on Linux, check the notification method first, then the fd
    let sev = if sev != 0 {
        let b: [u8; 64] = get(cx, sev)?;
        let how = i32::from_le_bytes(b[12..16].try_into().unwrap());
        let signo = i32::from_le_bytes(b[8..12].try_into().unwrap());
        if !matches!(how, SIGEV_NONE | SIGEV_SIGNAL | SIGEV_THREAD)
            || how == SIGEV_SIGNAL && Signal::new(signo).is_none()
        {
            return Err(Errno::EINVAL);
        }
        Some(b)
    } else {
        None
    };
    let (q, _) = queue_of(cx, fd)?;
    let tgid = cx.tgid();
    let Some(b) = sev else {
        // Cancel one's own registration
        let mut q = q.borrow_mut();
        if q.notify.as_ref().is_some_and(|n| n.tgid() == tgid) {
            remove_notification(&mut q);
            q.touch_read();
        }
        return Ok(0);
    };
    let value = u64::from_le_bytes(b[..8].try_into().unwrap());
    let signo = i32::from_le_bytes(b[8..12].try_into().unwrap());
    let how = i32::from_le_bytes(b[12..16].try_into().unwrap());
    let notify = match how {
        SIGEV_NONE => Notify::None { tgid },
        SIGEV_SIGNAL => Notify::Signal {
            tgid,
            sig: Signal::new(signo).ok_or(Errno::EINVAL)?,
            value,
        },
        // sigev_signo is the netlink socket's fd, and sigev_value points at the 32-byte cookie
        SIGEV_THREAD => {
            let file = cx.fds().get(signo)?;
            let sock = match &file.borrow().kind {
                FileKind::Socket(s) if matches!(&*s.borrow(), Socket::Netlink(_)) => {
                    Rc::downgrade(s)
                }
                FileKind::Socket(_) => return Err(Errno::EINVAL),
                _ => return Err(Errno::ENOTSOCK),
            };
            let cookie: [u8; 32] = get(cx, value)?;
            Notify::Socket { tgid, sock, cookie }
        }
        _ => return Err(Errno::EINVAL),
    };
    let mut q = q.borrow_mut();
    // A queue can have only one registered process; a registrant that has exited does not count
    if let Some(n) = &q.notify
        && cx
            .kr()
            .groups
            .get(&n.tgid())
            .is_some_and(|g| g.borrow().zombie.is_none())
    {
        return Err(Errno::EBUSY);
    }
    remove_notification(&mut q);
    q.notify = Some(notify);
    q.touch_read();
    Ok(0)
}

/// Files that process `tgid` closed (close, dup3 over a descriptor, close_range, close-on-exec,
/// its last thread's exit): its registrations on their queues end, whatever other descriptors
/// remain (Linux's `mqueue_flush_file`).
pub(crate) fn closed(tgid: Tid, files: &[crate::fd::FileRef]) {
    for f in files {
        if let FileKind::MessageQueue(q) = &f.borrow().kind {
            let mut q = q.borrow_mut();
            if q.notify.as_ref().is_some_and(|n| n.tgid() == tgid) {
                remove_notification(&mut q);
            }
        }
    }
}

/// Ends a queue's registration (Linux's `remove_notification`): a `SIGEV_THREAD` one gets its
/// cookie marked `NOTIFY_REMOVED` on the netlink socket, so musl's helper thread, blocked
/// receiving there, ends instead of waiting forever.
fn remove_notification(q: &mut MessageQueue) {
    if let Some(Notify::Socket { sock, cookie, .. }) = q.notify.take() {
        send_cookie(&sock, cookie, NOTIFY_REMOVED);
    }
}

/// Queues a `SIGEV_THREAD` cookie, its last byte set to `code`, on the registrant's netlink socket.
fn send_cookie(sock: &Weak<RefCell<Socket>>, mut cookie: [u8; 32], code: u8) {
    if let Some(sock) = sock.upgrade()
        && let Socket::Netlink(n) = &mut *sock.borrow_mut()
    {
        cookie[31] = code;
        n.queue.push_back((cookie.to_vec(), 0));
        n.arrivals += 1;
        request_wake_object(wake::of(&sock));
    }
}

/// The queue of an mq fd, and the open file's access mode.
fn queue_of(cx: &Cx<'_>, fd: i32) -> Result<(Rc<RefCell<MessageQueue>>, u32), Errno> {
    let file = cx.fds().get(fd)?;
    let f = file.borrow();
    match &f.kind {
        FileKind::MessageQueue(q) => Ok((q.clone(), f.flags)),
        _ => Err(Errno::EBADF),
    }
}

pub(super) fn mq_getsetattr(cx: &mut Cx<'_>, fd: i32, new: u64, old: u64) -> SysResult {
    let file = cx.fds().get(fd)?;
    let q = match &file.borrow().kind {
        FileKind::MessageQueue(q) => q.clone(),
        _ => return Err(Errno::EBADF),
    };
    let attr = if new != 0 {
        let b: [u8; 64] = get(cx, new)?;
        let flags = i64::from_le_bytes(b[..8].try_into().unwrap());
        if flags as u32 & !oflags::O_NONBLOCK != 0 {
            return Err(Errno::EINVAL);
        }
        Some(flags as u32)
    } else {
        None
    };
    if old != 0 {
        let q = q.borrow();
        let mut b = [0u8; 64];
        let fields = [
            (file.borrow().flags & oflags::O_NONBLOCK) as i64,
            q.maxmsg as i64,
            q.msgsize as i64,
            q.messages.len() as i64,
        ];
        for (i, v) in fields.into_iter().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        put(cx, old, b)?;
    }
    if let Some(flags) = attr {
        let mut f = file.borrow_mut();
        f.flags = (f.flags & !oflags::O_NONBLOCK) | flags;
        q.borrow_mut().touch_read();
    }
    Ok(0)
}

/// The queue whose inode number is `ino` in the namespace, and its name (a node in the mqueue
/// filesystem).
pub(crate) fn find(ipc: &crate::ipc::Ipc, ino: u64) -> Option<(&[u8], &Rc<RefCell<MessageQueue>>)> {
    ipc.mqueues
        .iter()
        .find(|(_, q)| q.borrow().ino == ino)
        .map(|(n, q)| (n.as_slice(), q))
}
