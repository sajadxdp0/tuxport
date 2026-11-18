//! Opening, reading, writing and ioctls of PTYs, plus terminal job control (the kernel/tty map).
//!
//! The line discipline is in [`crate::tty`]; this module handles blocking and waking, sessions
//! and controlling terminals, and SIGTTIN/SIGTTOU when a background process group reads or writes
//! the terminal.

use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use super::{SysResult, get, put};
use crate::cell::RefCell;
use crate::cred::cap;
use crate::errno::Errno;
use crate::fd::{FileKind, oflags};
use crate::kernel::{Cx, Kernel, Tid, request_wake_object, wake};
use crate::process::Signal;
use crate::signal::{self, SigAction, SigInfo};
use crate::tty::{PtyEnd, Termios, Tty, Winsize, cc, flags, pkt};

const TCGETS: u32 = 0x5401;
const TCSETS: u32 = 0x5402;
const TCSETSW: u32 = 0x5403;
const TCSETSF: u32 = 0x5404;
/// The old `struct termio`: only the low 16 bits of the flags and only 8 control characters.
const TCGETA: u32 = 0x5405;
const TCSETA: u32 = 0x5406;
const TCSETAW: u32 = 0x5407;
const TCSETAF: u32 = 0x5408;
const TCSBRK: u32 = 0x5409;
const TCXONC: u32 = 0x540a;
const TCFLSH: u32 = 0x540b;
const TIOCEXCL: u32 = 0x540c;
const TIOCNXCL: u32 = 0x540d;
const TIOCSCTTY: u32 = 0x540e;
const TIOCGPGRP: u32 = 0x540f;
const TIOCSPGRP: u32 = 0x5410;
const TIOCOUTQ: u32 = 0x5411;
const TIOCSTI: u32 = 0x5412;
const TIOCGWINSZ: u32 = 0x5413;
const TIOCSWINSZ: u32 = 0x5414;
const FIONREAD: u32 = 0x541b;
const TIOCNOTTY: u32 = 0x5422;
const TIOCSETD: u32 = 0x5423;
const TIOCGETD: u32 = 0x5424;
const TCSBRKP: u32 = 0x5425;
const TIOCGSID: u32 = 0x5429;
const TIOCVHANGUP: u32 = 0x5437;
/// `struct termios2`: adds input and output speeds (since glibc 2.42, tcgetattr and tcsetattr use
/// only these).
const TCGETS2: u32 = 0x802c_542a;
const TCSETS2: u32 = 0x402c_542b;
const TCSETSW2: u32 = 0x402c_542c;
const TCSETSF2: u32 = 0x402c_542d;
/// Packet mode of the master: set and query ([`crate::tty::pkt`]).
const TIOCPKT: u32 = 0x5420;
const TIOCGPKT: u32 = 0x8004_5438;
const TIOCGPTN: u32 = 0x8004_5430;
const TIOCSPTLCK: u32 = 0x4004_5431;
const TIOCGPTLCK: u32 = 0x8004_5439;
/// Sends a signal through the master to the slave's foreground process group (only SIGINT,
/// SIGQUIT, SIGTSTP).
const TIOCSIG: u32 = 0x4004_5436;

// ---- Allocation and opening ----

/// Opens ptmx: allocates a new PTY pair in devpts instance `fs` (found through mount `mnt`) and
/// returns the master. `pin`: it was opened through the ptmx in that mount.
pub(crate) fn open_ptmx(
    k: &mut Kernel,
    (fs, mnt, pin): (u64, crate::vfs::MountId, bool),
    owner: (u32, u32),
) -> Result<FileKind, Errno> {
    let tty = new_pty(k, fs, owner)?;
    Ok(FileKind::Pty(PtyEnd::new(tty, true).via(mnt, pin)))
}

/// Allocates a new PTY pair in devpts instance `fs` (a device number) (Linux's
/// `devpts_new_index`, `devpts_pty_new`): the index is the lowest free one in the instance, and
/// reaching the instance's `max` is ENOSPC; the slave's owner, group and mode follow the
/// instance's options, and without uid or gid options they are the caller's fsuid and fsgid.
pub(crate) fn new_pty(
    k: &mut Kernel,
    fs: u64,
    (fsuid, fsgid): (u32, u32),
) -> Result<Rc<RefCell<Tty>>, Errno> {
    k.ptys.retain(|_, w| w.strong_count() > 0);
    let opts = k
        .vfs
        .as_ref()
        .map_or(crate::tty::PtsOptions::BOOT, |v| v.devpts_options(fs));
    let used = k.ptys.range((fs, 0)..=(fs, u32::MAX)).count();
    if used >= opts.max as usize {
        return Err(Errno::ENOSPC);
    }
    let index = (0..).find(|i| !k.ptys.contains_key(&(fs, *i))).unwrap();
    let tty = Rc::new(RefCell::new(Tty::new(index)));
    {
        let mut t = tty.borrow_mut();
        t.fs = fs;
        t.owner = (opts.uid.unwrap_or(fsuid), opts.gid.unwrap_or(fsgid));
        t.mode = opts.mode;
    }
    k.ptys.insert((fs, index), Rc::downgrade(&tty));
    Ok(tty)
}

/// Opens `/dev/pts/<n>`. When a session leader without a controlling terminal opens it (without
/// `O_NOCTTY`), it becomes the controlling terminal, as on Linux.
pub(crate) fn open_slave(
    cx: &mut Cx<'_>,
    (fs, index, mnt, pin): (u64, u32, crate::vfs::MountId, bool),
    flags: u32,
) -> Result<FileKind, Errno> {
    let tty = cx
        .kr()
        .ptys
        .get(&(fs, index))
        .and_then(Weak::upgrade)
        .ok_or(Errno::ENOENT)?;
    {
        let t = tty.borrow();
        if t.locked || t.masters == 0 {
            return Err(Errno::EIO);
        }
    }
    // As Linux's tty_open_proc_set_tty, only an open with read access makes it the controlling
    // terminal: write access alone (group tty has it) must not bring reopening it through /dev/tty
    // or its signals
    let readable = matches!(flags & oflags::O_ACCMODE, oflags::O_RDONLY | oflags::O_RDWR);
    if flags & oflags::O_NOCTTY == 0 && readable {
        let mut g = cx.t.group.borrow_mut();
        if g.sid == g.tgid && g.ctty.is_none() && tty.borrow().session.is_none() {
            let mut t = tty.borrow_mut();
            t.session = Some(g.sid);
            t.fg = Some(g.pgid);
            g.ctty = Some(tty.clone());
            g.tty_old_pgrp = None;
        }
    }
    Ok(FileKind::Pty(PtyEnd::new(tty, false).via(mnt, pin)))
}

/// Opens the slave through the master (`TIOCGPTPEER`).
pub(crate) const TIOCGPTPEER: u32 = 0x5441;

/// TIOCGPTPEER (Linux's `ptm_open_peer`, since 4.13): opens the slave matching the master,
/// through the devpts mount the master was found in, without a path lookup and without permission
/// checks; `flags` are open flags, and O_CLOEXEC applies to the new fd. EIO on a slave.
pub(crate) fn open_peer(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    (master, born): (bool, u32),
    mnt: crate::vfs::MountId,
    flags: u32,
) -> SysResult {
    if !master || tty.borrow().hung(master, born) {
        return Err(Errno::EIO);
    }
    let (fs, index) = {
        let t = tty.borrow();
        (t.fs, t.index)
    };
    let kind = open_slave(cx, (fs, index, mnt, true), flags)?;
    let keep = oflags::O_ACCMODE | oflags::O_APPEND | oflags::O_NONBLOCK;
    // The path is <number> in that devpts mount (used by the fd's link and ttyname)
    let vfs = cx.vfs()?;
    let node = crate::vfs::Node::Pseudo(mnt, crate::pseudo::PseudoNode::Pts(fs, index));
    let path = vfs.path_of(&cx.view(&vfs), &cx.root(), &node).ok();
    let file = crate::fd::OpenFile::with_path(
        kind,
        flags & keep | oflags::O_LARGEFILE,
        path.map(Rc::from),
    );
    Ok(cx
        .fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)? as u64)
}

/// Opens `/dev/tty`: the caller's controlling terminal.
pub(crate) fn open_ctty(cx: &mut Cx<'_>) -> Result<FileKind, Errno> {
    let tty = cx.t.group.borrow().ctty.clone().ok_or(Errno::ENXIO)?;
    Ok(FileKind::Pty(PtyEnd::new(tty, false)))
}

// ---- Reading and writing ----

/// One read of a PTY. `born` is the open file's [`PtyEnd::born`].
pub(crate) fn read(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    (master, born): (bool, u32),
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    if master {
        master_read(cx, tty, born, nonblock, buf)
    } else {
        let n = slave_read(cx, tty, born, nonblock, buf)?;
        if n > 0 {
            tty.borrow_mut().touch(false);
        }
        Ok(n)
    }
}

/// A master read: takes output. Returns EIO once the slave has been opened and then all its ends
/// closed, and 0 when the master side was hung up by TIOCVHANGUP. In packet mode (Linux's
/// `n_tty_read`), when there is a status only that one status byte is read; otherwise the data is
/// preceded by a `TIOCPKT_DATA` (which also counts toward the read length).
fn master_read(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    born: u32,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    loop {
        {
            let mut t = tty.borrow_mut();
            if t.hung(true, born) {
                return Ok(0);
            }
            if buf.is_empty() {
                return Ok(0);
            }
            if let Some(status) = t.take_pktstatus() {
                buf[0] = status;
                return Ok(1);
            }
            let head = t.packet as usize;
            if t.has_output() {
                let out = t.take_output(buf.len() - head);
                if head == 1 {
                    buf[0] = pkt::DATA;
                }
                buf[head..head + out.len()].copy_from_slice(&out);
                request_wake_object(wake::of(tty));
                return Ok(head + out.len());
            }
            if t.slave_closed {
                return Err(Errno::EIO);
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_objects(None, &mut [], wake::FILE, &[wake::of(tty)])?;
    }
}

/// A slave read: one line in canonical mode; by VMIN/VTIME in non-canonical mode. For `born` see
/// [`Tty::hung`].
fn slave_read(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    born: u32,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    if tty.borrow().hung(false, born) {
        return Ok(0);
    }
    check_background(cx, tty, Signal::TTIN)?;
    let mut deadline: Option<Instant> = None;
    // Non-canonical bytes seen so far, to restart VTIME's inter-byte timer on each arrival
    let mut seen = 0;
    // Reading makes room for input that was waiting, which may hold signal characters
    let deliver = |cx: &mut Cx<'_>, n: usize| {
        let signals = tty.borrow_mut().take_signals();
        for sig in signals {
            signal_fg(cx.kr(), tty, sig);
        }
        request_wake_object(wake::of(tty));
        Ok(n)
    };
    loop {
        {
            let mut t = tty.borrow_mut();
            if t.hung(false, born) {
                return Ok(0);
            }
            // With EXTPROC a canonical-mode read returns whatever has arrived, as VMIN 1 would
            let icanon = t.termios.lflag & flags::ICANON != 0;
            let (vmin, vtime) = if icanon {
                (1, Duration::ZERO)
            } else {
                let tenths = t.termios.cc[cc::VTIME] as u64;
                (
                    t.termios.cc[cc::VMIN] as usize,
                    Duration::from_millis(100 * tenths),
                )
            };
            let expired = deadline.is_some_and(|d| Instant::now() >= d);
            if t.canonical() {
                if let Some(n) = t.read(buf) {
                    drop(t);
                    return deliver(cx, n);
                }
            } else {
                let avail = t.raw_len();
                let enough = avail >= vmin.min(buf.len()).max(1);
                // A nonblocking read takes what is there even below VMIN (Linux's n_tty_read
                // returns the bytes copied when it would otherwise wait)
                if avail > 0 && (enough || expired || vmin == 0 || nonblock) {
                    let n = t.read(buf).unwrap_or(0);
                    drop(t);
                    return deliver(cx, n);
                }
                if vmin == 0 && (vtime.is_zero() || expired) {
                    return Ok(0);
                }
                // VTIME: with VMIN 0 it counts from the start of the read; otherwise it is an
                // inter-byte timer, started at the first byte and again whenever more arrive
                // (n_tty_read sets the timeout again after copying what came), and with nothing
                // left (taken by another reader or flushed) the read waits for a first byte again
                if !vtime.is_zero() {
                    if vmin == 0 {
                        deadline.get_or_insert_with(|| Instant::now() + vtime);
                    } else if avail != seen {
                        deadline = (avail > 0).then(|| Instant::now() + vtime);
                        seen = avail;
                    }
                }
            }
            if t.masters == 0 {
                return Err(Errno::EIO);
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_objects(deadline, &mut [], wake::FILE, &[wake::of(tty)])?;
    }
}

/// One write of a PTY. `born` is the open file's [`PtyEnd::born`].
pub(crate) fn write(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    (master, born): (bool, u32),
    nonblock: bool,
    data: &[u8],
) -> Result<usize, Errno> {
    if master {
        master_write(cx, tty, born, nonblock, data)
    } else {
        let n = slave_write(cx, tty, born, nonblock, data)?;
        if n > 0 {
            tty.borrow_mut().touch(true);
        }
        Ok(n)
    }
}

/// A master write: hands the bytes to the line discipline as terminal input and sends the
/// resulting signals to the foreground process group.
fn master_write(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    born: u32,
    nonblock: bool,
    data: &[u8],
) -> Result<usize, Errno> {
    let mut done = 0;
    loop {
        let signals = {
            let mut t = tty.borrow_mut();
            if t.hung(true, born) {
                return if done > 0 { Ok(done) } else { Err(Errno::EIO) };
            }
            let n = t.input_room().min(data.len() - done);
            let s = t.write_input(&data[done..done + n]);
            done += n;
            if n > 0 {
                request_wake_object(wake::of(tty));
            }
            s
        };
        for sig in signals {
            signal_fg(cx.kr(), tty, sig);
        }
        if done == data.len() {
            return Ok(done);
        }
        if nonblock || cx.interrupted() {
            let e = if nonblock {
                Errno::EAGAIN
            } else {
                Errno::ERESTARTSYS
            };
            return if done > 0 { Ok(done) } else { Err(e) };
        }
        cx.wait_objects(None, &mut [], wake::FILE, &[wake::of(tty)])?;
    }
}

/// A slave write: goes through output processing, and waits for the master to read when the
/// output queue is full. Echoes that found no room go out ahead of each piece written (Linux
/// calls `n_tty_write`, which starts with `process_echoes`, once per chunk of a write); reading
/// the master alone does not send them, on Linux either.
fn slave_write(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    born: u32,
    nonblock: bool,
    data: &[u8],
) -> Result<usize, Errno> {
    if tty.borrow().hung(false, born) {
        return Err(Errno::EIO);
    }
    if tty.borrow().termios.lflag & flags::TOSTOP != 0 {
        check_background(cx, tty, Signal::TTOU)?;
    }
    let mut done = 0;
    loop {
        {
            let mut t = tty.borrow_mut();
            if t.masters == 0 || t.hung(false, born) {
                return if done > 0 { Ok(done) } else { Err(Errno::EIO) };
            }
            let echoed = t.echoes_before_write();
            let n = t.output_room().min(data.len() - done);
            if n > 0 {
                // Noted for the host console: a job the shell left in the background writes
                // while another group (the shell at its prompt) has the terminal
                let pgid = cx.t.group.borrow().pgid;
                if t.fg.is_some_and(|fg| fg != pgid) {
                    t.write_background(&data[done..done + n]);
                } else {
                    t.write_output(&data[done..done + n]);
                }
                done += n;
            }
            if n > 0 || echoed {
                request_wake_object(wake::of(tty));
            }
        }
        if done == data.len() {
            return Ok(done);
        }
        if nonblock || cx.interrupted() {
            let e = if nonblock {
                Errno::EAGAIN
            } else {
                Errno::ERESTARTSYS
            };
            return if done > 0 { Ok(done) } else { Err(e) };
        }
        cx.wait_objects(None, &mut [], wake::FILE, &[wake::of(tty)])?;
    }
}

/// Readiness for ppoll. `born` is the open file's [`PtyEnd::born`].
pub(crate) fn readiness(tty: &Tty, master: bool, born: u32) -> i16 {
    const POLLIN: i16 = 0x001;
    const POLLOUT: i16 = 0x004;
    const POLLHUP: i16 = 0x010;
    const POLLRDNORM: i16 = 0x040;
    const POLLWRNORM: i16 = 0x100;
    let mut ev = 0;
    if tty.hung(master, born) {
        const POLLERR: i16 = 0x008;
        ev = POLLIN | POLLOUT | POLLERR | POLLHUP | POLLRDNORM | POLLWRNORM;
    } else if master {
        if tty.has_output() {
            ev |= POLLIN | POLLRDNORM;
        }
        if tty.packet && tty.pktstatus != 0 {
            const POLLPRI: i16 = 0x002;
            ev |= POLLPRI | POLLIN | POLLRDNORM;
        }
        if tty.input_room() > 0 {
            ev |= POLLOUT | POLLWRNORM;
        }
        if tty.slave_closed {
            ev |= POLLHUP;
        }
    } else {
        if tty.poll_readable() {
            ev |= POLLIN | POLLRDNORM;
        }
        if tty.output_room() > 0 {
            ev |= POLLOUT | POLLWRNORM;
        }
        if tty.masters == 0 {
            ev |= POLLHUP | POLLIN;
        }
    }
    ev
}

// ---- ioctls ----

/// The ioctls of a PTY. `born` is the open file's [`PtyEnd::born`].
pub(crate) fn ioctl(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    (master, born): (bool, u32),
    readable: bool,
    request: u32,
    arg: u64,
) -> SysResult {
    if tty.borrow().hung(master, born) {
        return Err(if request == TIOCSPGRP {
            Errno::ENOTTY
        } else {
            Errno::EIO
        });
    }
    match request {
        TCGETS => {
            let t = tty.borrow().termios.encode();
            put(cx, arg, <[u8; 36]>::try_from(&t[..36]).unwrap()).map(|()| 0)
        }
        TCGETS2 => put(cx, arg, tty.borrow().termios.encode()).map(|()| 0),
        TCGETA => {
            let t = tty.borrow().termios.encode();
            let mut b = [0u8; 18];
            for i in 0..4 {
                b[i * 2..i * 2 + 2].copy_from_slice(&t[i * 4..i * 4 + 2]);
            }
            b[8..17].copy_from_slice(&t[16..25]);
            put(cx, arg, b).map(|()| 0)
        }
        TCSETS | TCSETSW | TCSETSF | TCSETA | TCSETAW | TCSETAF | TCSETS2 | TCSETSW2 | TCSETSF2 => {
            if !master {
                check_background(cx, tty, Signal::TTOU)?;
            }
            // Replace the given part of the existing settings (Linux's set_termios): termio
            // replaces only the low 16 bits of the flags and the first 8 control characters,
            // termios leaves the speed fields alone
            let mut t = tty.borrow().termios.encode();
            match request {
                TCSETA | TCSETAW | TCSETAF => {
                    let b: [u8; 18] = get(cx, arg)?;
                    for i in 0..4 {
                        t[i * 4..i * 4 + 2].copy_from_slice(&b[i * 2..i * 2 + 2]);
                    }
                    t[16..25].copy_from_slice(&b[8..17]);
                }
                TCSETS | TCSETSW | TCSETSF => {
                    t[..36].copy_from_slice(&get::<36>(cx, arg)?);
                }
                _ => t = get(cx, arg)?,
            }
            let mut t = Termios::decode(&t);
            t.load_speeds();
            let signals = {
                let mut tt = tty.borrow_mut();
                if matches!(request, TCSETSF | TCSETAF | TCSETSF2) {
                    // Both sides flush the slave's input (Linux does set_termios on real_tty)
                    tt.flush_input();
                    tt.report_flush(true, false);
                }
                tt.set_termios(t)
            };
            for sig in signals {
                signal_fg(cx.kr(), tty, sig);
            }
            request_wake_object(wake::of(tty));
            Ok(0)
        }
        TIOCGWINSZ => put(cx, arg, tty.borrow().winsize.encode()).map(|()| 0),
        TIOCSWINSZ => {
            let w = Winsize::decode(&get(cx, arg)?);
            let changed = std::mem::replace(&mut tty.borrow_mut().winsize, w) != w;
            if changed {
                signal_fg(cx.kr(), tty, Signal::WINCH);
            }
            Ok(0)
        }
        TCFLSH => {
            // tcflush, tcflow, tcdrain and changing the line discipline from a background job
            // get SIGTTOU like setting the termios (tty_check_change)
            if !master {
                check_background(cx, tty, Signal::TTOU)?;
            }
            let (input, output) = match arg {
                0 => (true, false),
                1 => (false, true),
                2 => (true, true),
                _ => return Err(Errno::EINVAL),
            };
            // Master and slave are a pair of ttys, and the master's input is the slave's output.
            // TCOFLUSH drops only what the other side has not taken yet (pty_flush_buffer): on the
            // master, input not taken by the slave's line discipline; on the slave, output beyond
            // what the master side has taken
            let mut t = tty.borrow_mut();
            if master {
                if input {
                    t.clear_output();
                }
                if output {
                    t.discard_unprocessed();
                }
            } else {
                if input {
                    t.discard_unprocessed();
                    t.flush_input();
                }
                if output {
                    t.flush_output();
                }
                t.report_flush(input, output);
            }
            request_wake_object(wake::of(tty));
            Ok(0)
        }
        TCXONC => {
            if !master {
                check_background(cx, tty, Signal::TTOU)?;
            }
            let mut signals = Vec::new();
            {
                let mut t = tty.borrow_mut();
                match (arg, master) {
                    (0, false) => t.stop_output(),
                    (1, false) => t.start_output(),
                    (0, true) => t.master_stopped = true,
                    (1, true) => t.master_stopped = false,
                    // TCIOFF, TCION: send the STOP or START character (not when disabled). What
                    // the master sends is input to the slave
                    (2 | 3, _) => {
                        let c = t.termios.cc[if arg == 2 { cc::VSTOP } else { cc::VSTART }];
                        if c != 0 && master {
                            signals = t.write_input(&[c]);
                        } else if c != 0 {
                            t.send_xchar(c);
                        }
                    }
                    _ => return Err(Errno::EINVAL),
                }
            }
            for sig in signals {
                signal_fg(cx.kr(), tty, sig);
            }
            request_wake_object(wake::of(tty));
            Ok(0)
        }
        TCSBRK | TCSBRKP | TIOCSETD => {
            if !master {
                check_background(cx, tty, Signal::TTOU)?;
            }
            Ok(0)
        }
        TIOCEXCL | TIOCNXCL => Ok(0),
        TIOCGETD => put(cx, arg, 0i32.to_le_bytes()).map(|()| 0),
        FIONREAD => {
            let t = tty.borrow();
            let n = if master {
                t.output_len()
            } else {
                t.input_len()
            };
            put(cx, arg, (n as i32).to_le_bytes()).map(|()| 0)
        }
        TIOCOUTQ => {
            let n = tty.borrow().output_len() as i32;
            put(cx, arg, n.to_le_bytes()).map(|()| 0)
        }
        // Linux's tiocsti: only on the caller's controlling terminal, which a master never is, or
        // with CAP_SYS_ADMIN; otherwise anyone holding a terminal fd (inherited across setsid or
        // su -c, passed over a socket) could type commands into the shell reading it. On a master
        // the byte goes to the master's own input, which master reads return (6.2's
        // dev.tty.legacy_tiocsti, EIO without CAP_SYS_ADMIN when off, is not emulated)
        TIOCSTI => {
            if (master || !is_ctty(cx, tty)) && !cx.cred().capable(cap::SYS_ADMIN) {
                return Err(Errno::EPERM);
            }
            let c: [u8; 1] = get(cx, arg)?;
            if master {
                tty.borrow_mut().inject_output(c[0]);
                request_wake_object(wake::of(tty));
                return Ok(0);
            }
            let signals = tty.borrow_mut().inject_input(c[0]);
            for sig in signals {
                signal_fg(cx.kr(), tty, sig);
            }
            request_wake_object(wake::of(tty));
            Ok(0)
        }
        TIOCGPTN if master => put(cx, arg, tty.borrow().index.to_le_bytes()).map(|()| 0),
        TIOCSPTLCK if master => {
            let lock = u32::from_le_bytes(get(cx, arg)?) != 0;
            tty.borrow_mut().locked = lock;
            Ok(0)
        }
        TIOCGPTLCK if master => {
            put(cx, arg, (tty.borrow().locked as i32).to_le_bytes()).map(|()| 0)
        }
        // Enabling packet mode discards the earlier status (Linux's pty_set_pktmode)
        TIOCPKT if master => {
            let on = i32::from_le_bytes(get(cx, arg)?) != 0;
            let mut t = tty.borrow_mut();
            if on && !t.packet {
                t.pktstatus = 0;
            }
            t.packet = on;
            Ok(0)
        }
        TIOCGPKT if master => put(cx, arg, (tty.borrow().packet as i32).to_le_bytes()).map(|()| 0),
        // Linux's pty_signal: sent to the slave's foreground process group, by the kernel, as
        // signals from keystrokes are
        TIOCSIG if master => {
            let sig = match arg {
                2 => Signal::INT,
                3 => Signal::QUIT,
                20 => Signal::TSTP,
                _ => return Err(Errno::EINVAL),
            };
            signal_fg(cx.kr(), tty, sig);
            Ok(0)
        }
        TIOCGPGRP => {
            // On the master, any holder may ask (Linux's tiocgpgrp): a tmux server, which has no
            // controlling terminal, names windows and finds a pane's cwd this way
            if !master && !is_ctty(cx, tty) {
                return Err(Errno::ENOTTY);
            }
            let fg = tty.borrow().fg.map_or(0, |p| cx.pid_out(p));
            put(cx, arg, fg.to_le_bytes()).map(|()| 0)
        }
        TIOCSPGRP => {
            if !is_ctty(cx, tty) {
                return Err(Errno::ENOTTY);
            }
            check_background(cx, tty, Signal::TTOU)?;
            let pgid = i32::from_le_bytes(get(cx, arg)?);
            if pgid < 0 {
                return Err(Errno::EINVAL);
            }
            let pgid = cx.pid_in(pgid);
            let sid = cx.t.group.borrow().sid;
            let exists = cx
                .kr()
                .groups
                .values()
                .any(|g| g.borrow().pgid == pgid && g.borrow().sid == sid);
            if !exists {
                return Err(Errno::EPERM);
            }
            tty.borrow_mut().fg = Some(pgid);
            Ok(0)
        }
        // Linux's tty_ioctl uses capable(): only the boot user namespace counts
        TIOCVHANGUP => {
            if !cx.cred().capable(cap::SYS_ADMIN) {
                return Err(Errno::EPERM);
            }
            vhangup(cx.k(), tty, master);
            Ok(0)
        }
        TIOCGSID => {
            // Like TIOCGPGRP, needs no controlling terminal on the master (Linux's tiocgsid)
            if !master && !is_ctty(cx, tty) {
                return Err(Errno::ENOTTY);
            }
            let sid = tty.borrow().session.ok_or(Errno::ENOTTY)?;
            put(cx, arg, cx.pid_out(sid).to_le_bytes()).map(|()| 0)
        }
        TIOCSCTTY => {
            let (sid, pgid, tgid, has_ctty) = {
                let g = cx.t.group.borrow();
                (g.sid, g.pgid, g.tgid, g.ctty.is_some())
            };
            if is_ctty(cx, tty) {
                return Ok(0);
            }
            if sid != tgid || has_ctty {
                return Err(Errno::EPERM);
            }
            // Linux's tiocsctty: the controlling terminal of another session can be taken over
            // only with arg 1 and CAP_SYS_ADMIN (in the initial user namespace, so not by a
            // sandbox's root), and without read access only with CAP_SYS_ADMIN
            let admin = cx.cred().capable(cap::SYS_ADMIN);
            let other = tty.borrow().session;
            if other.is_some() && (arg != 1 || !admin) {
                return Err(Errno::EPERM);
            }
            if !readable && !admin {
                return Err(Errno::EPERM);
            }
            if let Some(other) = other {
                for g in cx.kr().groups.values() {
                    let mut g = g.borrow_mut();
                    if g.sid == other {
                        g.ctty = None;
                    }
                }
            }
            {
                let mut t = tty.borrow_mut();
                t.session = Some(sid);
                t.fg = Some(pgid);
            }
            let mut g = cx.t.group.borrow_mut();
            g.ctty = Some(tty.clone());
            g.tty_old_pgrp = None;
            Ok(0)
        }
        TIOCNOTTY => {
            // A controlling terminal is always a slave, so the master never matches it
            if master || !is_ctty(cx, tty) {
                return Err(Errno::ENOTTY);
            }
            let (sid, tgid) = {
                let g = cx.t.group.borrow();
                (g.sid, g.tgid)
            };
            if sid == tgid {
                // The session leader gives up its controlling terminal: the whole session loses
                // it, and the foreground process group receives SIGHUP then SIGCONT (Linux's
                // disassociate_ctty)
                let fg = {
                    let mut t = tty.borrow_mut();
                    t.session = None;
                    t.fg.take()
                };
                for g in cx.kr().groups.values() {
                    let mut g = g.borrow_mut();
                    if g.sid == sid {
                        g.ctty = None;
                    }
                }
                if let Some(fg) = fg {
                    signal::send_to_pgrp(cx.kr(), fg, Signal::HUP, SigInfo::kernel());
                    signal::send_to_pgrp(cx.kr(), fg, Signal::CONT, SigInfo::kernel());
                }
            } else {
                cx.t.group.borrow_mut().ctty = None;
            }
            Ok(0)
        }
        _ => {
            crate::stub::ioctl(cx, request, crate::stub::TERMINAL);
            Err(Errno::ENOTTY)
        }
    }
}

/// Whether this terminal is the caller's controlling terminal.
fn is_ctty(cx: &Cx<'_>, tty: &Rc<RefCell<Tty>>) -> bool {
    cx.t.group
        .borrow()
        .ctty
        .as_ref()
        .is_some_and(|c| Rc::ptr_eq(c, tty))
}

// ---- Job control ----

/// A background process group accessing its controlling terminal (`sig` is SIGTTIN or SIGTTOU):
/// following Linux's `tty_check_change`, signals its own process group and makes the syscall
/// restart; when the signal is ignored or blocked, reads return EIO while writes and setting
/// changes proceed as usual.
fn check_background(cx: &mut Cx<'_>, tty: &Rc<RefCell<Tty>>, sig: Signal) -> Result<(), Errno> {
    let (pgid, is_ctty) = {
        let g = cx.t.group.borrow();
        (g.pgid, g.ctty.as_ref().is_some_and(|c| Rc::ptr_eq(c, tty)))
    };
    let fg = tty.borrow().fg;
    if !is_ctty || fg.is_none_or(|fg| fg == pgid) {
        return Ok(());
    }
    let ignored = cx.t.group.borrow().sighand.borrow().get(sig).handler == SigAction::IGNORE;
    let blocked = cx.task.sigmask.get() & sig.bit() != 0;
    if ignored || blocked {
        return if sig == Signal::TTIN {
            Err(Errno::EIO)
        } else {
            Ok(())
        };
    }
    if signal::pgrp_orphaned(cx.kr(), pgid) {
        return Err(Errno::EIO);
    }
    signal::send_to_pgrp(cx.kr(), pgid, sig, SigInfo::kernel());
    Err(Errno::ERESTARTSYS)
}

/// Sends a signal to the terminal's foreground process group (SIGINT and the like from the line
/// discipline, window size changes).
pub(crate) fn signal_fg(k: &Kernel, tty: &Rc<RefCell<Tty>>, sig: Signal) {
    if let Some(fg) = tty.borrow().fg {
        signal::send_to_pgrp(k, fg, sig, SigInfo::kernel());
    }
}

// ---- Hangup ----

/// Hangs up the terminals whose last master this thread closed ([`crate::tty::take_closed`]).
/// Called before releasing the BKL ([`crate::kernel::Bkl`]).
pub(crate) fn hang_up_closed(k: &mut Kernel) {
    for tty in crate::tty::take_closed() {
        hang_up(k, &tty);
    }
}

/// Hangs up a terminal (Linux's `__tty_hangup`): the session leader receives SIGHUP and SIGCONT,
/// and the foreground process group is recorded so that they are sent to it when the leader exits
/// (`process::release_ctty`); the processes in the session that have it as controlling terminal
/// lose it, and open slaves read EOF and fail writes from then on ([`Tty::hung_up`]).
/// `/dev/pts/<n>` disappears with it and the index can go to a new terminal (Linux's
/// `devpts_pty_kill`). Called when the last master is closed (Linux's `pty_close`,
/// [`hang_up_closed`]) and when a console is closed ([`crate::terminal::close`]); does nothing
/// for a terminal already hung up.
pub(crate) fn hang_up(k: &mut Kernel, tty: &Rc<RefCell<Tty>>) {
    let (key, session, fg) = {
        let mut t = tty.borrow_mut();
        if t.hung_up {
            return;
        }
        t.hung_up = true;
        ((t.fs, t.index), t.session.take(), t.fg.take())
    };
    if k.ptys
        .get(&key)
        .is_some_and(|w| std::ptr::eq(w.as_ptr(), Rc::as_ptr(tty)))
    {
        k.ptys.remove(&key);
    }
    detach(k, tty, session, fg);
}

/// vhangup(2): hangs up the caller's controlling terminal ([`vhangup`]); does nothing without a
/// controlling terminal. Requires `CAP_SYS_TTY_CONFIG` (Linux's capable(), which looks only at the
/// boot user namespace). A caller that is the session leader also receives SIGHUP and SIGCONT
/// itself (login ignores SIGHUP first).
pub(super) fn vhangup_ctty(cx: &mut Cx<'_>) -> SysResult {
    if !cx.cred().capable(cap::SYS_TTY_CONFIG) {
        return Err(Errno::EPERM);
    }
    let ctty = cx.t.group.borrow().ctty.clone();
    if let Some(tty) = ctty {
        vhangup(cx.k(), &tty, false);
    }
    Ok(0)
}

/// Hangs up one side of a terminal (Linux's `tty_vhangup`, vhangup(2), TIOCVHANGUP): every file
/// open on this side is hung up ([`Tty::hung`]) and unread input is discarded (the line
/// discipline's buffers are cleared with it). The slave side also signals the session leader and
/// detaches the session from the controlling terminal, as [`hang_up`] does; unlike closing the
/// master, `/dev/pts/<n>` stays and works again when reopened (login and agetty rely on this to
/// chase leftover processes off the terminal). The master side has no session: only the master's
/// files are hung up and the slave's output that has not been read yet is discarded. Does nothing
/// when the side has not been reopened since its hangup (Linux's `TTY_HUPPED`).
pub(crate) fn vhangup(k: &mut Kernel, tty: &Rc<RefCell<Tty>>, master: bool) {
    let side = master as usize;
    let session = {
        let mut t = tty.borrow_mut();
        if t.hupped[side] || (!master && t.hung_up) {
            return;
        }
        t.hupped[side] = true;
        t.vhangups[side] = t.vhangups[side].wrapping_add(1);
        if master {
            t.clear_output();
            None
        } else {
            // Linux's tty_ldisc_hangup flushes the slave's input, then __tty_hangup discards the
            // packet-mode status
            t.discard_unprocessed();
            t.flush_input();
            t.pktstatus = 0;
            Some((t.session.take(), t.fg.take()))
        }
    };
    match session {
        Some((session, fg)) => detach(k, tty, session, fg),
        None => request_wake_object(wake::of(tty)),
    }
}

/// The session part of a hangup (Linux's `tty_signal_session_leader`): the processes that have it
/// as controlling terminal lose it, the session leader receives SIGHUP and SIGCONT, and the
/// foreground process group `fg` is recorded so that they are sent to it when the leader exits
/// (`process::release_ctty`). Wakes the threads waiting on this terminal.
fn detach(k: &mut Kernel, tty: &Rc<RefCell<Tty>>, session: Option<Tid>, fg: Option<Tid>) {
    for g in k.groups.values() {
        let mut g = g.borrow_mut();
        if g.ctty.as_ref().is_some_and(|c| Rc::ptr_eq(c, tty)) {
            g.ctty = None;
        }
    }
    if let Some(leader) = session {
        // When the leader exits it has no controlling terminal any more, so SIGHUP goes to the
        // foreground process group of this moment (Linux's tty_old_pgrp)
        if let Some(g) = k.groups.get(&leader) {
            g.borrow_mut().tty_old_pgrp = fg;
        }
        signal::send_to_group(k, leader, Signal::HUP, SigInfo::kernel());
        signal::send_to_group(k, leader, Signal::CONT, SigInfo::kernel());
    }
    request_wake_object(wake::of(tty));
}
