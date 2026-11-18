//! The host-side console: an in-process PTY master held by the host (the kernel/tty map).
//!
//! [`crate::Machine`] opens a console for each terminal session, and the session leader uses its
//! slave as stdin/stdout/stderr and controlling terminal. Bytes the host writes go through the
//! line discipline (^C becomes SIGINT sent to the foreground process group), and the host takes
//! the guest's output. Host threads are not guest threads, but like them they access kernel
//! objects only while holding the BKL, and they wait through the same mechanism: a guest write to
//! the terminal requests a wake, and a host thread waiting for output wakes up.
//!
//! `tuxport-run` uses [`bridge`] to connect the host terminal to a console.

use std::rc::Rc;
use std::sync::Arc;

use tuxport_hal::Event;

use crate::cell::RefCell;
use crate::errno::Errno;
use crate::fd::{FileKind, FileRef, OpenFile, oflags};
use crate::kernel::{Kernel, Shared, Tid, block_for, request_wake, request_wake_object, wake};
use crate::process::Signal;
use crate::signal::{self, SigInfo};
use crate::tty::flags::{ECHO, ICANON};
use crate::tty::{PtyEnd, Tty, Winsize};

// ---- Consoles ----

/// A console.
pub(crate) struct Console {
    /// The PTY master.
    pub master: PtyEnd,
    /// A process already has it as controlling terminal: from then on, all slaves being closed
    /// means the session has ended.
    pub attached: bool,
}

/// Creates a console and returns its PTY number (`/dev/pts/<number>`).
pub(crate) fn open(k: &mut Kernel, size: Winsize) -> u32 {
    let fs = crate::tty::BOOT_PTS;
    let index = (0..).find(|i| !k.ptys.contains_key(&(fs, *i))).unwrap();
    let tty = Rc::new(RefCell::new(Tty::new(index)));
    {
        let mut t = tty.borrow_mut();
        t.locked = false;
        t.winsize = size;
    }
    k.ptys.insert((fs, index), Rc::downgrade(&tty));
    k.consoles.insert(
        index,
        Console {
            master: PtyEnd::new(tty, true),
            attached: false,
        },
    );
    index
}

/// Makes `leader` the console's session leader: returns the slave's open file and the PTY.
/// Returns EBUSY when the console already belongs to a session.
pub(crate) fn attach(
    k: &mut Kernel,
    id: u32,
    leader: Tid,
) -> Result<(FileRef, Rc<RefCell<Tty>>), Errno> {
    let console = k.consoles.get_mut(&id).ok_or(Errno::ENOENT)?;
    let tty = console.master.tty.clone();
    {
        let mut t = tty.borrow_mut();
        if t.session.is_some() {
            return Err(Errno::EBUSY);
        }
        t.session = Some(leader);
        t.fg = Some(leader);
    }
    console.attached = true;
    let path: Rc<[u8]> = Rc::from(format!("/dev/pts/{id}").into_bytes());
    let slave = OpenFile::with_path(
        FileKind::Pty(PtyEnd::new(tty.clone(), false)),
        oflags::O_RDWR | oflags::O_LARGEFILE,
        Some(path),
    );
    Ok((slave, tty))
}

/// Undoes [`attach`] for `leader`, whose process could not be started: the console has no session
/// again, and counts as attached only if it was before (`attached`), so its end of session is
/// what it was.
pub(crate) fn detach(k: &mut Kernel, id: u32, leader: Tid, attached: bool) {
    let Some(console) = k.consoles.get_mut(&id) else {
        return;
    };
    {
        let mut t = console.master.tty.borrow_mut();
        if t.session == Some(leader) {
            t.session = None;
            t.fg = None;
        }
    }
    console.attached = attached;
}

/// The wake objects to register when waiting for console `id`'s output or for room in its input
/// queue: its PTY. Closing a console requests a wake of the whole channel. None when the console
/// does not exist.
pub(crate) fn wake_objects(k: &Kernel, id: u32) -> Option<Vec<wake::Object>> {
    tty_of(k, id).ok().map(|t| vec![wake::of(&t)])
}

/// Takes up to `max` bytes of output. Returns None when there is no output; returns an empty Vec
/// when the session has ended (all slaves closed) and all output has been taken.
pub(crate) fn take_output(k: &Kernel, id: u32, max: usize) -> Result<Option<Vec<u8>>, Errno> {
    let tty = tty_of(k, id)?;
    let out = tty.borrow_mut().take_output(max);
    Ok(taken(k, id, &tty, out, false).map(|(out, _)| out))
}

/// As [`take_output`], but only up to where the output's writer changes between the foreground
/// process group and another, with whether another wrote it (a job left in the background, or a
/// process of another session).
pub(crate) fn take_output_run(
    k: &Kernel,
    id: u32,
    max: usize,
) -> Result<Option<(Vec<u8>, bool)>, Errno> {
    let tty = tty_of(k, id)?;
    let (out, background) = tty.borrow_mut().take_output_run(max);
    Ok(taken(k, id, &tty, out, background))
}

fn taken(
    k: &Kernel,
    id: u32,
    tty: &Rc<RefCell<Tty>>,
    out: Vec<u8>,
    background: bool,
) -> Option<(Vec<u8>, bool)> {
    if !out.is_empty() {
        // The output queue has room again, so threads waiting to write the terminal can go on
        request_wake_object(wake::of(tty));
        return Some((out, background));
    }
    let ended = k.consoles[&id].attached && tty.borrow().slaves == 0;
    ended.then(|| (Vec::new(), false))
}

/// How many more bytes the input queue can hold.
pub(crate) fn input_room(k: &Kernel, id: u32) -> Result<usize, Errno> {
    Ok(tty_of(k, id)?.borrow().input_room())
}

/// Hands host input to the line discipline and sends the resulting signals to the foreground
/// process group.
pub(crate) fn feed(k: &Kernel, id: u32, data: &[u8]) -> Result<(), Errno> {
    let tty = tty_of(k, id)?;
    let signals = tty.borrow_mut().write_input(data);
    request_wake_object(wake::of(&tty));
    let fg = tty.borrow().fg;
    if let Some(fg) = fg {
        for sig in signals {
            signal::send_to_pgrp(k, fg, sig, SigInfo::kernel());
        }
    }
    Ok(())
}

/// The console's foreground process group, for the app to tell when the program that reported
/// progress or its status gave the terminal back (the android/vt map). ESRCH when there is
/// none (the shell has exited).
pub(crate) fn fg_pgrp(k: &Kernel, id: u32) -> Result<Tid, Errno> {
    tty_of(k, id)?.borrow().fg.ok_or(Errno::ESRCH)
}

/// The current directory of the console's foreground process group (where restored tabs start after
/// the app is killed, the android/service map, and new tabs, the android/terminal map): the leader's, or any process in the group's once the
/// leader has exited; the path is relative to that process's root (as its `/proc/<pid>/cwd` is).
/// ESRCH when there is no foreground process group (the shell has exited).
pub(crate) fn fg_cwd(k: &Kernel, id: u32) -> Result<Vec<u8>, Errno> {
    let fg = tty_of(k, id)?.borrow().fg.ok_or(Errno::ESRCH)?;
    let leader = k.groups.get(&fg).filter(|g| g.borrow().pgid == fg);
    let group = leader
        .or_else(|| k.groups.values().find(|g| g.borrow().pgid == fg))
        .ok_or(Errno::ESRCH)?;
    let (tgid, tid) = {
        let g = group.borrow();
        (g.tgid, *g.threads.first().ok_or(Errno::ESRCH)?)
    };
    let task = k.tasks.get(&tid).ok_or(Errno::ESRCH)?;
    let (root, cwd) = {
        let res = task.res.borrow();
        let fs = res.fs.borrow();
        (fs.root.clone(), fs.cwd.clone())
    };
    let vfs = k.vfs.as_ref().ok_or(Errno::ENOENT)?;
    let view = crate::pseudo::View {
        k,
        vfs,
        tid,
        tgid,
        cred: None,
    };
    vfs.path_of(&view, &root, &cwd)
}

/// Whether the console's line discipline reads whole lines without echoing them (ICANON on, ECHO
/// off), as programs set it while reading a password: the app then draws the cursor as a lock, as
/// Ghostty does by the same rule (the android/vtview map).
pub(crate) fn password_input(k: &Kernel, id: u32) -> Result<bool, Errno> {
    let lflag = tty_of(k, id)?.borrow().termios.lflag;
    Ok(lflag & (ICANON | ECHO) == ICANON)
}

/// The window size changed: updates the PTY and sends SIGWINCH to the foreground process group.
pub(crate) fn resize(k: &Kernel, id: u32, w: Winsize) -> Result<(), Errno> {
    let tty = tty_of(k, id)?;
    let (changed, fg) = {
        let mut t = tty.borrow_mut();
        (std::mem::replace(&mut t.winsize, w) != w, t.fg)
    };
    if let (true, Some(fg)) = (changed, fg) {
        signal::send_to_pgrp(k, fg, Signal::WINCH, SigInfo::kernel());
    }
    Ok(())
}

/// Closes a console: as when Linux hangs up a terminal, sends SIGHUP and SIGCONT to the session
/// leader. Afterwards the slave reads EOF and writes fail.
pub(crate) fn close(k: &mut Kernel, id: u32) -> Result<(), Errno> {
    let console = k.consoles.remove(&id).ok_or(Errno::ENOENT)?;
    let tty = console.master.tty.clone();
    drop(console);
    crate::syscall::hang_up(k, &tty);
    request_wake();
    Ok(())
}

// ---- The tuxport-run bridge ----

/// A terminal attached to the host ([`RunOptions::terminal`](crate::RunOptions::terminal)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terminal {
    /// Host fd that keyboard input is read from.
    pub input: i32,
    /// Host fd that terminal output is written to.
    pub output: i32,
    /// Host terminal whose window size is followed (TIOCGWINSZ); without one the window size is
    /// 24×80.
    pub size: Option<i32>,
    /// Host fd that becomes readable when the window size changes (for example a pipe written by
    /// a SIGWINCH handler).
    pub resize: Option<i32>,
}

const POLLIN: i16 = 1;
/// How much input to read from the host at a time.
const READ_CHUNK: usize = 4096;

/// The EOF handed to the program after the host's input ends ([`send_eof`]).
#[derive(Clone, Copy, PartialEq)]
enum Eof {
    /// Input has not ended yet.
    Open,
    /// Input has ended; waiting for the program to read the input already sent in.
    Pending,
    Sent,
}

/// The host terminal's current window size; 24×80 when it is not a terminal.
pub(crate) fn initial_size(term: &Terminal) -> Winsize {
    term.size.and_then(host_winsize).unwrap_or_default()
}

/// The bridge thread of `tuxport-run`: connects the host terminal to console `id` until process
/// `pid` ends, all guest threads have exited and the output has been written.
pub(crate) fn bridge(shared: Arc<Shared>, term: Terminal, id: u32, pid: Tid) {
    let event = Arc::new(Event::new().expect("cannot create an eventfd for the terminal"));
    let mut eof = Eof::Open;
    let mut buf = vec![0u8; READ_CHUNK];
    let mut k = shared.lock();
    loop {
        match take_output(&k, id, 64 * 1024) {
            Ok(Some(out)) if !out.is_empty() => {
                drop(k);
                write_all(term.output, &out);
                k = shared.lock();
                continue;
            }
            // The console has closed, or all slaves have closed and the output has been taken
            Err(_) | Ok(Some(_)) => break,
            Ok(None) => {}
        }
        if k.watched.get(&pid).is_some_and(Option::is_some) && k.tasks.is_empty() {
            break;
        }
        if eof == Eof::Pending && send_eof(&k, id) {
            eof = Eof::Sent;
        }
        let room = input_room(&k, id).unwrap_or(0);
        let mut fds = Vec::new();
        if eof == Eof::Open && room > 0 {
            fds.push((term.input, POLLIN, 0));
        }
        if let Some(r) = term.resize {
            fds.push((r, POLLIN, 0));
        }
        let objects = wake_objects(&k, id);
        let (k2, _) = block_for(
            &shared,
            k,
            &event,
            None,
            &mut fds,
            wake::ALL,
            objects.as_deref(),
        );
        k = k2;
        for (fd, _, revents) in fds {
            if revents == 0 {
                continue;
            }
            if fd == term.input {
                drop(k);
                let n = tuxport_hal::read(fd, &mut buf[..room.min(READ_CHUNK)]);
                k = shared.lock();
                match n {
                    Ok(n) if n > 0 => {
                        let _ = feed(&k, id, &buf[..n]);
                    }
                    // The host's input has ended (stdin is a pipe or file): stop reading, and once
                    // the program has read the input already sent in, hand it an EOF
                    _ => eof = Eof::Pending,
                }
            } else {
                tuxport_hal::drain(fd);
                if let Some(w) = term.size.and_then(host_winsize) {
                    let _ = resize(&k, id, w);
                }
            }
        }
    }
}

/// Writes all the data to the host fd; drops it when the host side has closed.
fn write_all(fd: i32, mut data: &[u8]) {
    while !data.is_empty() {
        match tuxport_hal::write(fd, data) {
            Ok(0) | Err(_) => return,
            Ok(n) => data = &data[n..],
        }
    }
}

/// After the host's input ends, sends a VEOF (^D) into the console's input, as util-linux's
/// script does: in canonical mode a VEOF at the start of a line makes read return 0, so programs
/// that end at EOF, such as a shell or cat, exit; outside canonical mode the program receives the
/// character (a shell's line editor treats it as EOF). As script does, it waits until the program
/// has read the input already sent in: while that is unread it returns false, and the console's
/// PTY wakes the bridge thread when the program reads input. A program that leaves canonical mode
/// before reading the EOF gets the VEOF character ([`Tty::inject_eof`]). Sends nothing when VEOF
/// is disabled.
fn send_eof(k: &Kernel, id: u32) -> bool {
    let Ok(tty) = tty_of(k, id) else {
        return true;
    };
    if tty.borrow().input_pending() {
        return false;
    }
    let signals = tty.borrow_mut().inject_eof();
    request_wake_object(wake::of(&tty));
    let fg = tty.borrow().fg;
    if let Some(fg) = fg {
        for sig in signals {
            signal::send_to_pgrp(k, fg, sig, SigInfo::kernel());
        }
    }
    true
}

/// The host terminal's window size.
fn host_winsize(fd: i32) -> Option<Winsize> {
    const TIOCGWINSZ: u64 = 0x5413;
    let mut b = [0u8; 8];
    tuxport_hal::ioctl(fd, TIOCGWINSZ, &mut b).ok()?;
    let w = Winsize::decode(&b);
    (w.rows > 0 && w.cols > 0).then_some(w)
}

// ---- Shared helpers ----

fn tty_of(k: &Kernel, id: u32) -> Result<Rc<RefCell<Tty>>, Errno> {
    k.consoles
        .get(&id)
        .map(|c| c.master.tty.clone())
        .ok_or(Errno::ENOENT)
}
