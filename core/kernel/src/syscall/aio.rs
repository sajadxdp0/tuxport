//! Linux native AIO (the kernel/aio map): io_setup, io_destroy, io_submit, io_cancel, io_getevents,
//! io_pgetevents.
//!
//! Requests run synchronously in io_submit, and completion events go into the context's queue,
//! where io_getevents takes them; only `IOCB_CMD_POLL` stays pending while the fd is not ready.
//! Linux completes such a poll from the file's wake-up; here it completes when it is looked at
//! again: by io_getevents, which waits on the pending polls' files as ppoll would, by io_cancel,
//! and, for a poll that signals an eventfd (programs wait on the eventfd with epoll rather than
//! in io_getevents), by the timer thread, which watches it as it watches `O_ASYNC` files
//! ([`check_polls`]). Contexts belong to the thread group: a forked child has none, and they are
//! cleared on execve. A context id is the address of a page of read-only memory in the guest:
//! libaio reads the ring buffer header there, and when the magic number is wrong it always goes
//! through the syscall.

use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Instant;

use tuxport_engine::prot;

use super::{SysResult, get, put, read_timespec};
use crate::cell::RefCell;
use crate::errno::Errno;
use crate::fd::{EventFd, FileKind, FileRef, OpenFile, oflags};
use crate::kernel::{Cx, Kernel, request_wake_object, wake};
use crate::mm::flags;
use crate::uaccess::UserSlice;
use crate::vfs::dev::Device;

const IOCB_CMD_PREAD: u16 = 0;
const IOCB_CMD_PWRITE: u16 = 1;
const IOCB_CMD_FSYNC: u16 = 2;
const IOCB_CMD_FDSYNC: u16 = 3;
const IOCB_CMD_POLL: u16 = 5;
const IOCB_CMD_PREADV: u16 = 7;
const IOCB_CMD_PWRITEV: u16 = 8;
/// On completion, notify the eventfd `aio_resfd`.
const IOCB_FLAG_RESFD: u32 = 1;
/// `aio_rw_flags`: RWF_HIPRI does not affect the result.
const RWF_DSYNC: u32 = 0x2;
const RWF_SYNC: u32 = 0x4;
const RWF_NOWAIT: u32 = 0x8;
const RWF_APPEND: u32 = 0x10;
const RWF_KNOWN: u32 = 0x1 | RWF_DSYNC | RWF_SYNC | RWF_NOWAIT | RWF_APPEND;

/// An `IOCB_CMD_POLL` waiting for its fd to be ready.
struct PendingPoll {
    data: u64,
    obj: u64,
    file: FileRef,
    events: i16,
    /// The eventfd of `aio_resfd`, taken at submission as Linux's `eventfd_ctx_fdget` does: closing
    /// or reusing the fd number later changes nothing.
    resfd: Option<Rc<RefCell<EventFd>>>,
}

/// An AIO context.
pub(crate) struct AioContext {
    /// io_setup's `nr_events`: the limit on completed events not yet taken plus pending requests.
    max: usize,
    /// Completion events (`struct io_event`).
    events: VecDeque<[u8; 32]>,
    polls: Vec<PendingPoll>,
    /// io_destroy'ed: threads waiting in io_getevents return after waking (Linux's `ctx->dead`).
    dead: bool,
}

impl AioContext {
    fn room(&self) -> bool {
        self.events.len() + self.polls.len() < self.max
    }
}

pub(super) fn io_setup(cx: &mut Cx<'_>, nr: u32, ctxp: u64) -> SysResult {
    let cur = u64::from_le_bytes(get(cx, ctxp)?);
    if cur != 0 || nr == 0 || nr > 0x1000_0000 / 32 {
        return Err(Errno::EINVAL);
    }
    // nr counts towards aio-nr (Linux before 4.12 counted twice nr)
    let cost = u64::from(nr);
    if cx.kr().aio_nr + cost > cx.kr().aio_max_nr {
        return Err(Errno::EAGAIN);
    }
    let anon = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS;
    let ring = cx.mm_mut().mmap(0, 4096, prot::READ, anon, None)?;
    if let Err(e) = put(cx, ctxp, ring.to_le_bytes()) {
        let _ = cx.mm_mut().munmap(ring, 4096);
        return Err(e);
    }
    cx.k().aio_nr += cost;
    let ctx = AioContext {
        max: nr as usize,
        events: VecDeque::new(),
        polls: Vec::new(),
        dead: false,
    };
    cx.t.group
        .borrow_mut()
        .aio
        .insert(ring, Rc::new(RefCell::new(ctx)));
    Ok(0)
}

pub(super) fn io_submit(cx: &mut Cx<'_>, ctx: u64, nr: i64, iocbpp: u64) -> SysResult {
    let c = context(cx, ctx)?;
    if nr < 0 {
        return Err(Errno::EINVAL);
    }
    let mut done = 0;
    for i in 0..nr as u64 {
        let r = get::<8>(cx, iocbpp.wrapping_add(i * 8))
            .map(u64::from_le_bytes)
            .and_then(|obj| {
                if obj == 0 {
                    Err(Errno::EFAULT)
                } else {
                    Ok(obj)
                }
            })
            .and_then(|obj| submit_one(cx, &c, obj));
        match r {
            Ok(()) => done += 1,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        }
    }
    // Threads waiting in io_getevents take the new events, or wait on the new pending polls too
    request_wake_object(wake::of(&c));
    Ok(done)
}

/// Submits one request: reads, writes and fsync complete on the spot, and a poll that is not ready
/// stays pending. What is refused, and in which order, follows 6.1's io_submit_one and the
/// per-opcode preparation (aio_prep_rw with aio_read/aio_write, aio_fsync, aio_poll).
fn submit_one(cx: &mut Cx<'_>, c: &Rc<RefCell<AioContext>>, obj: u64) -> Result<(), Errno> {
    let b: [u8; 64] = get(cx, obj)?;
    let u64_at = |off: usize| u64::from_le_bytes(b[off..off + 8].try_into().unwrap());
    let u32_at = |off: usize| u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
    let data = u64_at(0);
    let rw_flags = u32_at(12);
    let op = u16::from_le_bytes([b[16], b[17]]);
    let fd = u32_at(20) as i32;
    let (buf, len, offset) = (u64_at(24), u64_at(32), u64_at(40) as i64);
    let (reserved, aio_flags, resfd) = (u64_at(48), u32_at(56), u32_at(60) as i32);
    // A negative aio_nbytes is EINVAL for every opcode (io_submit_one's overflow check)
    if reserved != 0 || (len as i64) < 0 {
        return Err(Errno::EINVAL);
    }
    if !c.borrow().room() {
        return Err(Errno::EAGAIN);
    }
    // An O_PATH fd is no file to fget
    let file = cx.fds().get(fd)?;
    if file.borrow().flags & oflags::O_PATH != 0 {
        return Err(Errno::EBADF);
    }
    let resfd = if aio_flags & IOCB_FLAG_RESFD != 0 {
        match &cx.fds().get(resfd)?.borrow().kind {
            FileKind::EventFd(e) => Some(e.clone()),
            _ => return Err(Errno::EINVAL),
        }
    } else {
        None
    };
    // The kernel writes aio_key as 0 (KIOCB_KEY), and io_cancel checks against it
    put(cx, obj + 8, 0u32.to_le_bytes())?;
    let res = match op {
        IOCB_CMD_PREAD | IOCB_CMD_PREADV | IOCB_CMD_PWRITE | IOCB_CMD_PWRITEV => {
            let write = matches!(op, IOCB_CMD_PWRITE | IOCB_CMD_PWRITEV);
            if rw_flags & !RWF_KNOWN != 0 {
                return Err(Errno::EOPNOTSUPP);
            }
            let f = file.borrow();
            if !(if write { f.writable() } else { f.readable() }) {
                return Err(Errno::EBADF);
            }
            if !has_iter(&f, write) {
                return Err(Errno::EINVAL);
            }
            drop(f);
            // rw_verify_area: a negative position is EINVAL for every file, also one that ignores
            // positions (a pipe)
            if offset < 0 {
                return Err(Errno::EINVAL);
            }
            read_write(cx, &file, fd, op, (buf, len, offset), rw_flags)
        }
        IOCB_CMD_FSYNC | IOCB_CMD_FDSYNC => {
            // Fields an fsync does not use must be 0, and the file must have an fsync
            if buf != 0 || offset != 0 || len != 0 || rw_flags != 0 {
                return Err(Errno::EINVAL);
            }
            if !super::fs::has_fsync(&file.borrow()) {
                return Err(Errno::EINVAL);
            }
            match super::fs::fsync(cx, fd, op == IOCB_CMD_FDSYNC) {
                Ok(_) => 0,
                Err(e) => -(e.0 as i64),
            }
        }
        IOCB_CMD_POLL => {
            // Events beyond the 16 bits of the poll mask and the fields a poll does not use are
            // refused
            if buf > u64::from(u16::MAX) || offset != 0 || len != 0 || rw_flags != 0 {
                return Err(Errno::EINVAL);
            }
            let events = buf as u16 as i16;
            let ready = super::poll::poll_once(&file.borrow(), events, false);
            if ready == 0 {
                let signals = resfd.is_some();
                c.borrow_mut().polls.push(PendingPoll {
                    data,
                    obj,
                    file,
                    events,
                    resfd,
                });
                if signals {
                    watch(cx, c);
                }
                return Ok(());
            }
            ready as i64
        }
        _ => return Err(Errno::EINVAL),
    };
    c.borrow_mut().events.push_back(event(data, obj, res));
    notify_resfd(resfd.as_ref());
    Ok(())
}

/// Whether Linux 6.1 gives the file a `read_iter` (or, with `write`, a `write_iter`) operation,
/// which AIO reads and writes need (`EINVAL` without). Files with only `read`/`write`, or none:
/// directories, the read side of `/dev/kmsg`, the write side of `/dev/full` and of eventfds,
/// `/dev/loop-control`, epoll, timerfd, signalfd, inotify, pidfd, message queues, seccomp
/// notification and bpf fds. Synthetic files count as having both.
fn has_iter(f: &OpenFile, write: bool) -> bool {
    match &f.kind {
        FileKind::Directory { .. }
        | FileKind::Epoll(_)
        | FileKind::TimerFd(_)
        | FileKind::SignalFd(_)
        | FileKind::Inotify(_)
        | FileKind::Pidfd(_)
        | FileKind::MessageQueue(_)
        | FileKind::SeccompNotify(_)
        | FileKind::Bpf(_) => false,
        FileKind::EventFd(_) => !write,
        FileKind::Device { dev, .. } => match dev {
            Device::Kmsg => write,
            Device::Full => !write,
            Device::LoopControl => false,
            _ => true,
        },
        _ => true,
    }
}

/// A pending poll signals an eventfd: the timer thread watches its context ([`check_polls`]),
/// since a program may wait on the eventfd rather than in io_getevents. Wakes the timer thread,
/// which listens only on the "other" channel while it has nothing to watch.
fn watch(cx: &mut Cx<'_>, c: &Rc<RefCell<AioContext>>) {
    let k = cx.k();
    k.aio_polls.retain(|w| w.strong_count() > 0);
    if !k.aio_polls.iter().any(|w| w.as_ptr() == Rc::as_ptr(c)) {
        k.aio_polls.push(Rc::downgrade(c));
    }
    crate::kernel::signal_later(&cx.shared.timers);
}

/// Runs a read or write request, returning the io_event's res (negative is an errno).
fn read_write(
    cx: &mut Cx<'_>,
    file: &FileRef,
    fd: i32,
    op: u16,
    (buf, len, offset): (u64, u64, i64),
    rw_flags: u32,
) -> i64 {
    // Files that cannot seek (pipes, sockets) ignore the offset; with RWF_NOWAIT, do not wait
    let at = super::fs::seekable(&file.borrow()).then_some(offset as u64);
    let saved = file.borrow().flags;
    let mut f = saved;
    if rw_flags & RWF_NOWAIT != 0 {
        f |= oflags::O_NONBLOCK;
    }
    if rw_flags & RWF_APPEND != 0 {
        f |= oflags::O_APPEND;
    }
    file.borrow_mut().flags = f;
    let r = transfer(cx, file, fd, op, (buf, len as usize, at), rw_flags);
    // Restore only the two flags added temporarily: other threads may have changed other flags
    // while the request ran
    let temp = oflags::O_NONBLOCK | oflags::O_APPEND;
    let restored = (file.borrow().flags & !temp) | (saved & temp);
    file.borrow_mut().flags = restored;
    match r {
        Ok(n) => n as i64,
        // A request interrupted by a signal cannot be restarted with the others already done
        // (Linux's aio_rw_done): EINTR
        Err(
            Errno::ERESTARTSYS
            | Errno::ERESTARTNOINTR
            | Errno::ERESTARTNOHAND
            | Errno::ERESTART_RESTARTBLOCK,
        ) => -(Errno::EINTR.0 as i64),
        Err(e) => -(e.0 as i64),
    }
}

/// The transfer itself, with `RWF_APPEND` already set as `O_APPEND`. A write to an `O_APPEND`
/// regular file goes to its end whatever the offset (Linux's `IOCB_APPEND` in
/// generic_write_checks), and `RWF_DSYNC`/`RWF_SYNC` wait for the data to reach disk after it, as
/// pwritev2 does (generic_write_sync).
fn transfer(
    cx: &mut Cx<'_>,
    file: &FileRef,
    fd: i32,
    op: u16,
    (buf, len, at): (u64, usize, Option<u64>),
    rw_flags: u32,
) -> Result<u64, Errno> {
    let n = match op {
        IOCB_CMD_PREAD => super::fs::read_file(cx, file, buf, len, at)?,
        IOCB_CMD_PREADV => super::fs::vectored(cx, fd, buf, len, at, false)?,
        _ => {
            let at = super::fs::append_offset(file, at)?;
            if op == IOCB_CMD_PWRITE {
                super::fs::write_file(cx, file, buf, len, at)?
            } else {
                super::fs::vectored(cx, fd, buf, len, at, true)?
            }
        }
    };
    let write = matches!(op, IOCB_CMD_PWRITE | IOCB_CMD_PWRITEV);
    if write && n > 0 && rw_flags & (RWF_DSYNC | RWF_SYNC) != 0 {
        super::fs::write_sync(cx, file, rw_flags)?;
    }
    Ok(n)
}

pub(super) fn io_cancel(cx: &mut Cx<'_>, ctx: u64, iocb: u64, _result: u64) -> SysResult {
    let key = u32::from_le_bytes(get(cx, iocb.wrapping_add(8))?);
    let c = context(cx, ctx)?;
    if key != 0 {
        return Err(Errno::EINVAL);
    }
    // A poll whose fd is ready has already completed on Linux, from the file's wake-up, and can no
    // longer be cancelled
    complete_polls(&c);
    // Only a pending poll can be cancelled: its completion event still goes into the queue (with
    // no ready events) and signals its eventfd, and EINPROGRESS is returned, as on Linux
    let p = {
        let mut c = c.borrow_mut();
        let i = c
            .polls
            .iter()
            .position(|p| p.obj == iocb)
            .ok_or(Errno::EINVAL)?;
        let p = c.polls.remove(i);
        c.events.push_back(event(p.data, p.obj, 0));
        p
    };
    request_wake_object(wake::of(&c));
    notify_resfd(p.resfd.as_ref());
    Err(Errno::EINPROGRESS)
}

pub(super) fn io_getevents(
    cx: &mut Cx<'_>,
    ctx: u64,
    min_nr: i64,
    nr: i64,
    events: u64,
    timeout: u64,
    sigmask: Option<(u64, usize)>,
) -> SysResult {
    if min_nr < 0 || nr < 0 || min_nr > nr {
        return Err(Errno::EINVAL);
    }
    let c = context(cx, ctx)?;
    let deadline = match timeout {
        0 => None,
        t => Some(Instant::now() + read_timespec(cx, t)?),
    };
    if let Some((mask, size)) = sigmask {
        super::poll::set_temp_mask(cx, mask, size)?;
    }
    let r = loop {
        complete_polls(&c);
        let have = c.borrow().events.len() as i64;
        if have >= min_nr.max(1) || min_nr == 0 || deadline.is_some_and(|d| Instant::now() >= d) {
            break Ok(());
        }
        // The context was destroyed while waiting: events already completed are taken all the
        // same, and when there are none it is EINVAL (Linux's aio_read_events)
        if c.borrow().dead {
            break if have > 0 { Ok(()) } else { Err(Errno::EINVAL) };
        }
        if cx.interrupted() {
            break Err(Errno::EINTR);
        }
        let polls: Vec<(FileRef, i16)> = c
            .borrow()
            .polls
            .iter()
            .map(|p| (p.file.clone(), p.events))
            .collect();
        if let Err(e) = super::poll::wait_aio_polls(cx, &polls, wake::of(&c), deadline) {
            break Err(e);
        }
    };
    if sigmask.is_some() {
        super::poll::restore_mask(cx, &r);
    }
    r?;
    let take = (c.borrow().events.len() as i64).min(nr) as usize;
    if take == 0 {
        return Ok(0);
    }
    let out: Vec<[u8; 32]> = c.borrow_mut().events.drain(..take).collect();
    let bytes: Vec<u8> = out.concat();
    if let Err(e) = cx.mm().write(UserSlice::new(events, bytes.len()), &bytes) {
        // Linux leaves the events in the ring when copying them out faults: the next call takes
        // them
        let mut c = c.borrow_mut();
        for ev in out.into_iter().rev() {
            c.events.push_front(ev);
        }
        return Err(e);
    }
    Ok(take as u64)
}

/// Completes the pending polls of context `c` that have become ready: queues their events, wakes
/// the threads waiting for the context and signals the eventfds. Needs no task, so the timer
/// thread uses it too.
fn complete_polls(c: &Rc<RefCell<AioContext>>) {
    let mut done = Vec::new();
    {
        let mut c = c.borrow_mut();
        let mut i = 0;
        while i < c.polls.len() {
            let p = &c.polls[i];
            let ready = super::poll::poll_once(&p.file.borrow(), p.events, true);
            if ready != 0 {
                let p = c.polls.remove(i);
                c.events.push_back(event(p.data, p.obj, ready as i64));
                done.push(p.resfd);
            } else {
                i += 1;
            }
        }
    }
    if done.is_empty() {
        return;
    }
    request_wake_object(wake::of(c));
    for resfd in done {
        notify_resfd(resfd.as_ref());
    }
}

/// The timer thread's check of the contexts with pending polls that signal an eventfd
/// ([`watch`]): completes those that have become ready, as Linux does from the file's wake-up, and
/// returns the host fds to poll and the earliest timerfd expiration for the polls still pending.
/// Contexts that are gone, destroyed or left with no such poll are dropped. Called every time the
/// timer thread wakes, which is on every change while there are such contexts.
pub(crate) fn check_polls(k: &mut Kernel) -> (Vec<(i32, i16, i16)>, Option<Instant>) {
    let mut host = Vec::new();
    let mut timer = None;
    k.aio_polls.retain(|w| {
        let Some(c) = w.upgrade() else {
            return false;
        };
        complete_polls(&c);
        let mut watched = false;
        for p in c.borrow().polls.iter().filter(|p| p.resfd.is_some()) {
            super::poll::watch_aio_poll(&p.file.borrow(), p.events, &mut host, &mut timer);
            watched = true;
        }
        watched
    });
    (host, timer)
}

/// The last argument of io_pgetevents: `struct { const sigset_t *sigmask; size_t sigsetsize; }`.
pub(super) fn sigmask_arg(cx: &Cx<'_>, usig: u64) -> Result<Option<(u64, usize)>, Errno> {
    if usig == 0 {
        return Ok(None);
    }
    let b: [u8; 16] = get(cx, usig)?;
    let mask = u64::from_le_bytes(b[..8].try_into().unwrap());
    let size = u64::from_le_bytes(b[8..].try_into().unwrap()) as usize;
    Ok((mask != 0).then_some((mask, size)))
}

/// Destroys a context ([`released`]) and unmaps its page. mariadb relies on the waiters returning
/// to end its event-waiting threads at shutdown.
pub(super) fn io_destroy(cx: &mut Cx<'_>, ctx: u64) -> SysResult {
    let c =
        cx.t.group
            .borrow_mut()
            .aio
            .remove(&ctx)
            .ok_or(Errno::EINVAL)?;
    released(cx.k(), std::iter::once((ctx, c)));
    let _ = cx.mm_mut().munmap(ctx, 4096);
    Ok(0)
}

/// Contexts are no longer used (io_destroy, execve, process exit), as Linux's kill_ioctx: each is
/// marked dead, so threads waiting for it in io_getevents return after waking; its pending polls
/// are cancelled, producing no events that can be taken but signalling their eventfds as their
/// cancelled completions do; and it is subtracted from aio-nr.
pub(crate) fn released(
    k: &mut Kernel,
    contexts: impl IntoIterator<Item = (u64, Rc<RefCell<AioContext>>)>,
) {
    for (_, c) in contexts {
        let polls = {
            let mut c = c.borrow_mut();
            c.dead = true;
            std::mem::take(&mut c.polls)
        };
        for p in polls {
            notify_resfd(p.resfd.as_ref());
        }
        request_wake_object(wake::of(&c));
        k.aio_nr = k.aio_nr.saturating_sub(c.borrow().max as u64);
    }
}

fn context(cx: &Cx<'_>, ctx: u64) -> Result<Rc<RefCell<AioContext>>, Errno> {
    cx.t.group
        .borrow()
        .aio
        .get(&ctx)
        .cloned()
        .ok_or(Errno::EINVAL)
}

fn event(data: u64, obj: u64, res: i64) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&data.to_le_bytes());
    b[8..16].copy_from_slice(&obj.to_le_bytes());
    b[16..24].copy_from_slice(&res.to_le_bytes());
    b
}

/// On completion, notifies the eventfd (`IOCB_FLAG_RESFD`): its count goes up by one.
fn notify_resfd(efd: Option<&Rc<RefCell<EventFd>>>) {
    let Some(efd) = efd else {
        return;
    };
    {
        let mut e = efd.borrow_mut();
        e.count = e.count.saturating_add(1).min(u64::MAX - 1);
        // Counts as one arrival, like a write to the eventfd (EPOLLET fires again)
        e.writes += 1;
    }
    request_wake_object(wake::of(efd));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::Tier;

    /// A pending poll that signals an eventfd, on a host fd (a host socket in practice), is
    /// completed by the timer thread's check once data arrives from outside the kernel, with no
    /// wake-up requested at all: until then the check hands the fd to the timer thread's poll.
    #[test]
    fn timer_thread_completes_eventfd_polls_on_host_fds() {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let mut k = Kernel::new(Tier::Interpreter);
        let efd = Rc::new(RefCell::new(EventFd {
            count: 0,
            semaphore: false,
            writes: 0,
            reads: 0,
        }));
        let c = Rc::new(RefCell::new(AioContext {
            max: 4,
            events: VecDeque::new(),
            polls: Vec::new(),
            dead: false,
        }));
        c.borrow_mut().polls.push(PendingPoll {
            data: 7,
            obj: 0x1000,
            file: OpenFile::new(FileKind::Host(fds[0]), oflags::O_RDONLY),
            events: 1,
            resfd: Some(efd.clone()),
        });
        k.aio_polls.push(Rc::downgrade(&c));
        assert_eq!(check_polls(&mut k), (vec![(fds[0], 1, 0)], None));
        assert_eq!(k.aio_polls.len(), 1);
        assert_eq!(efd.borrow().count, 0);
        assert_eq!(unsafe { libc::write(fds[1], b"x".as_ptr().cast(), 1) }, 1);
        assert_eq!(check_polls(&mut k), (Vec::new(), None));
        assert!(k.aio_polls.is_empty());
        assert_eq!(efd.borrow().count, 1);
        let ev = c.borrow().events[0];
        assert_eq!(u64::from_le_bytes(ev[..8].try_into().unwrap()), 7);
        assert_ne!(i64::from_le_bytes(ev[16..24].try_into().unwrap()) & 1, 0);
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }
}
