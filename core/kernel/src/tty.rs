//! In-process PTYs and the line discipline (the kernel/tty map).
//!
//! One [`Tty`] is the whole state of a PTY pair: the slave's termios, the window size, the input
//! queue (organized by lines in canonical mode), the output queue, and the session and foreground
//! process group job control needs. What the master writes is "keyboard input", which goes
//! through the line discipline (ICRNL, echo, line editing, ISIG) into the input queue; data the
//! slave writes goes through output processing (ONLCR) into the output queue, from which the
//! master reads it.
//!
//! Only plain data structures and the line discipline live here; blocking, signals and job
//! control checks are done in the syscall layer.
//!
//! The line discipline follows the rules of Linux's n_tty. Its echo queue (`Echo` and
//! `commit_echoes`), erasing (`erase`, `echo_erased`) and output processing (`output_char`) are
//! adapted from the line discipline of Fuchsia's Starnix (`src/starnix/lib/line_discipline` at
//! commit 86962cb40d56e1a2bc033ead5b4c89bb02c34ca2), under its BSD-style license:
//!
//! ```text
//! Copyright 2025-2026 The Fuchsia Authors. All rights reserved.
//! Use of this source code is governed by a BSD-style license that can be
//! found in android/licenses/fuchsia-LICENSE.
//! ```

use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use crate::cell::RefCell;
use crate::kernel::{Tid, request_wake_object, wake};
use crate::process::Signal;

/// The major number of PTY slaves (`/dev/pts/<n>`).
pub const PTS_MAJOR: u32 = 136;

/// The tty group: `gid=5` when devpts is mounted (also written so in mountinfo); every new
/// terminal belongs to it.
pub const TTY_GID: u32 = 5;

/// The device number of the devpts instance mounted at `/dev/pts` at boot
/// ([`crate::vfs::Fs::dev`]), which is also its id.
pub(crate) const BOOT_PTS: u64 = 0x18;
/// The most terminals one devpts instance can have (`NR_UNIX98_PTY_MAX`).
pub(crate) const PTY_MAX: u32 = 1 << 20;

/// Mount options of a devpts instance (Linux's `pts_mount_opts`): the owner and group of new
/// terminals (when not given, the fsuid and fsgid of the process creating it) and their mode, the
/// mode of ptmx, and the limit on the number of terminals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PtsOptions {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: u32,
    pub ptmxmode: u32,
    pub max: u32,
}

impl PtsOptions {
    /// The boot instance: the same as docker mounts for containers
    /// (`gid=5,mode=620,ptmxmode=666`).
    pub const BOOT: PtsOptions = PtsOptions {
        uid: None,
        gid: Some(TTY_GID),
        mode: 0o620,
        ptmxmode: 0o666,
        max: PTY_MAX,
    };

    /// Parses mount(2) options (`devpts_parse_param`): uid and gid are decimal and must be
    /// mappable in the caller's user namespace (`uid_in` converts to kernel IDs), mode and
    /// ptmxmode are octal; `newinstance` has had no effect since 4.7; anything else, or a bad
    /// format, is EINVAL. Options not given take the defaults (`mode=600,ptmxmode=000`).
    pub fn parse(
        data: &[u8],
        uid_in: impl Fn(u32) -> Option<u32>,
        gid_in: impl Fn(u32) -> Option<u32>,
    ) -> Result<PtsOptions, crate::errno::Errno> {
        use crate::errno::Errno;
        let mut o = PtsOptions {
            uid: None,
            gid: None,
            mode: 0o600,
            ptmxmode: 0,
            max: PTY_MAX,
        };
        let num = |v: &[u8], radix: u32| {
            std::str::from_utf8(v)
                .ok()
                .and_then(|s| u32::from_str_radix(s, radix).ok())
                .ok_or(Errno::EINVAL)
        };
        for opt in data.split(|&b| b == b',').filter(|o| !o.is_empty()) {
            let (key, value) = match opt.iter().position(|&b| b == b'=') {
                Some(i) => (&opt[..i], Some(&opt[i + 1..])),
                None => (opt, None),
            };
            match (key, value) {
                (b"uid", Some(v)) => o.uid = Some(uid_in(num(v, 10)?).ok_or(Errno::EINVAL)?),
                (b"gid", Some(v)) => o.gid = Some(gid_in(num(v, 10)?).ok_or(Errno::EINVAL)?),
                (b"mode", Some(v)) => o.mode = num(v, 8)? & 0o7777,
                (b"ptmxmode", Some(v)) => o.ptmxmode = num(v, 8)? & 0o7777,
                (b"max", Some(v)) => {
                    o.max = num(v, 10)?;
                    if o.max > PTY_MAX {
                        return Err(Errno::EINVAL);
                    }
                }
                (b"newinstance", None) => {}
                _ => return Err(Errno::EINVAL),
            }
        }
        Ok(o)
    }

    /// The superblock options in mountinfo (`devpts_show_options`).
    pub fn show(&self) -> String {
        let mut s = String::new();
        if let Some(u) = self.uid {
            s += &format!(",uid={u}");
        }
        if let Some(g) = self.gid {
            s += &format!(",gid={g}");
        }
        s += &format!(",mode={:03o},ptmxmode={:03o}", self.mode, self.ptmxmode);
        if self.max < PTY_MAX {
            s += &format!(",max={}", self.max);
        }
        s
    }
}
/// The line discipline's input buffer (Linux's `N_TTY_BUF_SIZE`): one byte always stays free,
/// except for the terminator of a canonical line.
pub const INPUT_MAX: usize = 4096;
/// Master writes the line discipline has not taken yet wait in the PTY's buffer up to this much
/// (Linux's flip buffer, `TTYB_DEFAULT_MEM_LIMIT`); master writes block once it is full.
pub const UNPROCESSED_MAX: usize = 64 * 1024;
/// The output queue capacity: once the slave has filled it, writes block until the master reads.
pub const OUTPUT_MAX: usize = 64 * 1024;
/// The most echoes waiting for room in the output queue (n_tty's 4096-byte echo buffer).
const ECHO_MAX: usize = 4096;
/// How much of the slave's output the master side has taken on Linux: its own line discipline
/// buffers [`INPUT_MAX`] bytes less the free one, and the rest is still in the PTY's buffer, where
/// TCOFLUSH on the slave drops it.
const PASSED_ON: usize = INPUT_MAX - 1;

/// termios flag bits (asm-generic values).
pub mod flags {
    /// Mark parity errors; a 0xff byte is read doubled.
    pub const PARMRK: u32 = 0o10;
    /// Strip the 8th bit.
    pub const ISTRIP: u32 = 0o40;
    /// Translate NL to CR.
    pub const INLCR: u32 = 0o100;
    /// Ignore CR.
    pub const IGNCR: u32 = 0o200;
    /// Translate CR to NL.
    pub const ICRNL: u32 = 0o400;
    /// Map upper case to lower case (with IEXTEN).
    pub const IUCLC: u32 = 0o1000;
    /// ^S/^Q flow control.
    pub const IXON: u32 = 0o2000;
    /// Any character restarts output stopped by ^S.
    pub const IXANY: u32 = 0o4000;
    /// Input is UTF-8 (erase deletes by character).
    pub const IUTF8: u32 = 0o40000;

    /// Output processing.
    pub const OPOST: u32 = 0o1;
    /// Map lower case to upper case on output.
    pub const OLCUC: u32 = 0o2;
    /// Translate NL to CR NL on output.
    pub const ONLCR: u32 = 0o4;
    /// Translate CR to NL on output.
    pub const OCRNL: u32 = 0o10;
    /// No CR output at column 0.
    pub const ONOCR: u32 = 0o20;
    /// NL also returns the carriage.
    pub const ONLRET: u32 = 0o40;
    /// The tab delay bits; all set (`XTABS`, `TAB3`) expands tabs to spaces.
    pub const TABDLY: u32 = 0o14000;
    pub const XTABS: u32 = 0o14000;

    /// The character size bits.
    pub const CSIZE: u32 = 0o60;
    /// 8-bit characters.
    pub const CS8: u32 = 0o60;
    /// Enable the receiver.
    pub const CREAD: u32 = 0o200;
    /// Parity.
    pub const PARENB: u32 = 0o400;
    /// The output speed bits: the low 4 bits are B0–B38400, and with `CBAUDEX` 15 more steps from
    /// B57600.
    pub const CBAUD: u32 = 0o10017;
    /// The extension bit within `CBAUD`.
    pub const CBAUDEX: u32 = 0o10000;
    /// `CBAUDEX` alone means the speed is given separately in `c_ospeed` (in `c_ispeed` for the
    /// input speed bits).
    pub const BOTHER: u32 = 0o10000;
    /// The input speed is at `CBAUD` shifted left by this many bits (`CIBAUD`); 0 means the same as
    /// the output.
    pub const IBSHIFT: u32 = 16;
    /// The address bit (RS-485), changeable only through the driver.
    pub const ADDRB: u32 = 0o4000000000;
    /// Baud rate 38400.
    pub const B38400: u32 = 0o17;

    /// INTR, QUIT and SUSP generate signals.
    pub const ISIG: u32 = 0o1;
    /// Canonical mode (line editing).
    pub const ICANON: u32 = 0o2;
    /// Echo.
    pub const ECHO: u32 = 0o10;
    /// ERASE echoes as backspace.
    pub const ECHOE: u32 = 0o20;
    /// Echo a newline after KILL.
    pub const ECHOK: u32 = 0o40;
    /// Echo NL even when ECHO is off.
    pub const ECHONL: u32 = 0o100;
    /// Do not flush the queues when generating signals.
    pub const NOFLSH: u32 = 0o200;
    /// Background processes writing the terminal get SIGTTOU.
    pub const TOSTOP: u32 = 0o400;
    /// Echo control characters as `^X`.
    pub const ECHOCTL: u32 = 0o1000;
    /// Echo erased characters between `\` and `/` (for printing terminals).
    pub const ECHOPRT: u32 = 0o2000;
    /// KILL erases the whole line character by character.
    pub const ECHOKE: u32 = 0o4000;
    /// Extended functions (LNEXT, WERASE, REPRINT).
    pub const IEXTEN: u32 = 0o100000;
    /// Line editing is done externally (on the master side).
    pub const EXTPROC: u32 = 0o200000;
}

/// The status bytes the master reads in packet mode (TIOCPKT) (`TIOCPKT_*`).
pub mod pkt {
    /// Data follows.
    pub const DATA: u8 = 0;
    /// The slave's input was flushed.
    pub const FLUSHREAD: u8 = 1;
    /// The slave's output was flushed.
    pub const FLUSHWRITE: u8 = 2;
    /// The slave's output was stopped (^S, TCOOFF).
    pub const STOP: u8 = 4;
    /// Restarted.
    pub const START: u8 = 8;
    /// No longer ^S/^Q flow control.
    pub const NOSTOP: u8 = 16;
    /// Changed to ^S/^Q flow control.
    pub const DOSTOP: u8 = 32;
    /// termios changed while EXTPROC is set.
    pub const IOCTL: u8 = 64;
}

/// Indices into `c_cc`.
pub mod cc {
    pub const VINTR: usize = 0;
    pub const VQUIT: usize = 1;
    pub const VERASE: usize = 2;
    pub const VKILL: usize = 3;
    pub const VEOF: usize = 4;
    pub const VTIME: usize = 5;
    pub const VMIN: usize = 6;
    pub const VSTART: usize = 8;
    pub const VSTOP: usize = 9;
    pub const VSUSP: usize = 10;
    pub const VEOL: usize = 11;
    pub const VREPRINT: usize = 12;
    pub const VWERASE: usize = 14;
    pub const VLNEXT: usize = 15;
    pub const VEOL2: usize = 16;
    /// The length of `c_cc` (asm-generic's `NCCS`).
    pub const NCCS: usize = 19;
}

use cc::*;
use flags::*;

/// The kernel's `ktermios`, that is asm-generic's `struct termios2` (44 bytes): `struct termios`
/// (36 bytes, used by TCGETS and TCSETS) followed by the input and output speeds (used by TCGETS2
/// and TCSETS2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; NCCS],
    pub ispeed: u32,
    pub ospeed: u32,
}

impl Default for Termios {
    /// The initial settings of a PTY slave, the same as Linux's `tty_std_termios`.
    fn default() -> Self {
        let mut c = [0u8; NCCS];
        c[..17].copy_from_slice(
            b"\x03\x1c\x7f\x15\x04\x00\x01\x00\x11\x13\x1a\x00\x12\x0f\x17\x16\x00",
        );
        Termios {
            iflag: ICRNL | IXON,
            oflag: OPOST | ONLCR,
            cflag: B38400 | CS8 | CREAD,
            lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
            line: 0,
            cc: c,
            ispeed: 38400,
            ospeed: 38400,
        }
    }
}

impl Termios {
    /// Encodes as the guest's `struct termios2`; the first 36 bytes are `struct termios`.
    pub fn encode(&self) -> [u8; 44] {
        let mut b = [0u8; 44];
        for (i, v) in [self.iflag, self.oflag, self.cflag, self.lflag]
            .iter()
            .enumerate()
        {
            b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        b[16] = self.line;
        b[17..36].copy_from_slice(&self.cc);
        b[36..40].copy_from_slice(&self.ispeed.to_le_bytes());
        b[40..].copy_from_slice(&self.ospeed.to_le_bytes());
        b
    }

    /// Decodes from the guest's `struct termios2`.
    pub fn decode(b: &[u8; 44]) -> Termios {
        let word = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        let mut c = [0u8; NCCS];
        c.copy_from_slice(&b[17..36]);
        Termios {
            iflag: word(0),
            oflag: word(1),
            cflag: word(2),
            lflag: word(3),
            line: b[16],
            cc: c,
            ispeed: word(9),
            ospeed: word(10),
        }
    }

    /// Fills in both speeds from `CBAUD` and `CIBAUD` (what Linux's `set_termios` does before
    /// applying the settings, `tty_termios_baud_rate`, `tty_termios_input_baud_rate`): the speed
    /// fields are always usable even when set the old `Bxxx` way, and with `BOTHER` the values in
    /// the fields are kept. When the input speed bits are 0 it is the same as the output.
    pub fn load_speeds(&mut self) {
        let ospeed = baud(self.cflag & CBAUD, self.ospeed);
        let input = (self.cflag >> IBSHIFT) & CBAUD;
        self.ispeed = if input == 0 {
            ospeed
        } else {
            baud(input, self.ispeed)
        };
        self.ospeed = ospeed;
    }

    fn l(&self, f: u32) -> bool {
        self.lflag & f != 0
    }

    fn i(&self, f: u32) -> bool {
        self.iflag & f != 0
    }

    fn o(&self, f: u32) -> bool {
        self.oflag & f != 0
    }

    /// Whether `c` is a given special character (`_POSIX_VDISABLE`, i.e. 0, means disabled).
    fn is(&self, c: u8, which: usize) -> bool {
        self.cc[which] != 0 && self.cc[which] == c
    }
}

/// The speeds of the `CBAUD` steps (Linux's `baud_table`, not the sparc one): B0–B38400, then
/// `CBAUDEX` plus 1–15.
const BAUD_TABLE: [u32; 31] = [
    0, 50, 75, 110, 134, 150, 200, 300, 600, 1200, 1800, 2400, 4800, 9600, 19200, 38400, 57600,
    115200, 230400, 460800, 500000, 576000, 921600, 1000000, 1152000, 1500000, 2000000, 2500000,
    3000000, 3500000, 4000000,
];

/// The speed for a set of `CBAUD` bits; `other` for `BOTHER`.
fn baud(cbaud: u32, other: u32) -> u32 {
    if cbaud == BOTHER {
        return other;
    }
    let i = if cbaud & CBAUDEX != 0 {
        (cbaud & !CBAUDEX) + 15
    } else {
        cbaud
    };
    BAUD_TABLE.get(i as usize).copied().unwrap_or(0)
}

/// The window size (`struct winsize`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

impl Default for Winsize {
    /// The 24×80 the console uses when the host terminal's size is unknown.
    fn default() -> Self {
        Winsize {
            rows: 24,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        }
    }
}

impl Winsize {
    /// Encodes as the guest's `struct winsize`.
    pub fn encode(&self) -> [u8; 8] {
        let mut b = [0u8; 8];
        for (i, v) in [self.rows, self.cols, self.xpixel, self.ypixel]
            .iter()
            .enumerate()
        {
            b[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    /// Decodes from the guest's `struct winsize`.
    pub fn decode(b: &[u8; 8]) -> Winsize {
        let h = |i: usize| u16::from_le_bytes([b[i * 2], b[i * 2 + 1]]);
        Winsize {
            rows: h(0),
            cols: h(1),
            xpixel: h(2),
            ypixel: h(3),
        }
    }
}

/// One line completed in canonical mode.
struct Line {
    data: Vec<u8>,
    /// Ended by VEOF, which Linux keeps after the data as a `__DISABLED_CHAR` (0) that a read takes
    /// without returning it: a line with no data is end of file.
    eof: bool,
    /// The end of file the host console hands the program when its input ends
    /// ([`Tty::inject_eof`]): leaving canonical mode turns it into the VEOF character, not a NUL.
    injected: bool,
}

/// An echo waiting to be written to the output (Linux's echo buffer). Echoes are formatted, and
/// the cursor column followed, only when they are committed at the end of a batch of input, so a
/// signal character later in the batch discards them; while output is stopped they wait.
#[derive(Clone, Copy, Debug)]
enum Echo {
    /// A character through output processing (`echo_char_raw`); 0xff goes out as it is (Linux's
    /// escaped `ECHO_OP_START`).
    Char(u8),
    /// A control character shown as `^X` (ECHOCTL): two columns, no output processing.
    Ctrl(u8),
    /// Backspaces over an erased tab (`ECHO_OP_ERASE_TAB`): `columns` taken by the characters
    /// after the previous tab, or after the start of the line when there is none (`after_tab`
    /// false), which then counts from the column the line started at.
    EraseTab { columns: usize, after_tab: bool },
    /// The canonical line starts at the current column (`ECHO_OP_SET_CANON_COL`).
    SetCanonColumn,
    /// One column back (`ECHO_OP_MOVE_BACK_COL`, for each extra byte of a multibyte character that
    /// ECHOPRT shows).
    MoveBackColumn,
}

/// A PTY pair.
pub struct Tty {
    /// `/dev/pts/<index>`.
    pub index: u32,
    /// The devpts instance it is in (its device number, [`PtsOptions`]).
    pub fs: u64,
    pub termios: Termios,
    pub winsize: Winsize,
    /// Master writes the line discipline has not taken yet (Linux's flip buffer): taken as it
    /// gets room, when the slave reads or termios changes.
    unprocessed: VecDeque<u8>,
    /// The line being edited in canonical mode.
    editing: Vec<u8>,
    /// Lines completed in canonical mode and ready to be read.
    lines: VecDeque<Line>,
    /// Bytes ready to be read in non-canonical mode (and with EXTPROC).
    raw: VecDeque<u8>,
    /// Echoes not written to the output yet ([`Echo`]).
    echoes: Vec<Echo>,
    /// Output waiting for the master to read it.
    output: VecDeque<u8>,
    /// How many bytes of output the master has taken so far: where `output` starts in the stream
    /// of everything written, which `background` counts in
    taken: u64,
    /// The parts of `output` written by a process group that was not in the foreground, for the
    /// host console ([`Tty::take_output_run`])
    background: VecDeque<std::ops::Range<u64>>,
    /// The cursor column output processing follows (Linux's `ldata->column`), and the column the
    /// canonical line being edited started at (`canon_column`): erasing a tab moves back by what
    /// it took.
    column: usize,
    canon_column: usize,
    /// The next character is entered literally (LNEXT).
    lnext: bool,
    /// An ECHOPRT erase (`\` and the erased characters) is open; the next character typed closes
    /// it with `/`.
    erasing: bool,
    /// Signals generated by input, for the caller to send to the foreground process group.
    signals: Vec<Signal>,
    /// The slave's output is stopped (^S or TCOOFF, Linux's `flow.stopped`): slave writes block
    /// and echoes wait.
    stopped: bool,
    /// The output was stopped by TCOOFF: ^Q cannot restart it, TCOON is needed (Linux's
    /// `flow.tco_stopped`).
    tco_stopped: bool,
    /// The master's output is stopped by TCOOFF: master writes wait until TCOON. The master's
    /// termios has no IXON, so only TCXONC can stop it.
    pub master_stopped: bool,
    /// The master is in packet mode (TIOCPKT): each read first returns a status byte ([`pkt`]),
    /// then data.
    pub packet: bool,
    /// Status not yet read in packet mode (Linux records it in the slave's `ctrl.pktstatus`).
    pub pktstatus: u8,
    /// The session that has it as controlling terminal.
    pub session: Option<Tid>,
    /// The foreground process group.
    pub fg: Option<Tid>,
    /// The numbers of open masters and slaves.
    pub masters: usize,
    pub slaves: usize,
    /// The slave has been opened and now all its ends are closed (Linux's `TTY_OTHER_CLOSED`):
    /// master reads are EIO and poll includes POLLHUP; cleared when a slave is opened again. A
    /// slave never opened does not count, and master reads block as usual.
    pub slave_closed: bool,
    /// Whether the slave is still locked (unlockpt).
    pub locked: bool,
    /// All masters are closed and it has been hung up (`syscall::tty::hang_up`): open slaves
    /// read 0, writes and ioctls are EIO, and poll is always ready with POLLERR and POLLHUP
    /// (Linux's `hung_up_tty_fops`).
    pub hung_up: bool,
    /// How many times the slave and master sides were hung up by vhangup or TIOCVHANGUP
    /// (`syscall::tty::vhangup`): files opened before that are hung up ([`Tty::hung`]), those
    /// opened later work as usual.
    pub vhangups: [u32; 2],
    /// This side has not been reopened since its hangup (Linux's `TTY_HUPPED`): hanging it up
    /// again does nothing.
    pub hupped: [bool; 2],
    /// The owner, group and mode of `/dev/pts/<index>`: opening ptmx takes them from the devpts
    /// instance's [`PtsOptions`] (`syscall::tty::new_pty`; without uid or gid options, the
    /// opener's fsuid and fsgid), and console terminals keep the default root:tty 0620; chown and
    /// chmod can change them, and the change lasts until the terminal is closed.
    pub owner: (u32, u32),
    pub mode: u32,
    /// How many times data arrived for input (read by the slave) and output (read by the master):
    /// as on Linux, each arrival makes the reader's `EPOLLET` fire again.
    pub arrivals: [u64; 2],
    /// The access, modification and status change times of `/dev/pts/<index>` (nanoseconds): the
    /// idle time of w and who comes from the access time ([`Tty::touch`]).
    pub atime_ns: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl Tty {
    /// Creates a PTY pair with neither end open yet.
    pub fn new(index: u32) -> Tty {
        let now = wall_ns();
        let fs = BOOT_PTS;
        Tty {
            index,
            termios: Termios::default(),
            // All 0, as for a terminal newly allocated by Linux; the console sets it separately
            // once created (`terminal::open`)
            winsize: Winsize {
                rows: 0,
                cols: 0,
                xpixel: 0,
                ypixel: 0,
            },
            unprocessed: VecDeque::new(),
            editing: Vec::new(),
            lines: VecDeque::new(),
            raw: VecDeque::new(),
            echoes: Vec::new(),
            output: VecDeque::new(),
            taken: 0,
            background: VecDeque::new(),
            column: 0,
            canon_column: 0,
            lnext: false,
            erasing: false,
            signals: Vec::new(),
            stopped: false,
            tco_stopped: false,
            master_stopped: false,
            packet: false,
            pktstatus: 0,
            session: None,
            fg: None,
            masters: 0,
            slaves: 0,
            slave_closed: false,
            locked: true,
            hung_up: false,
            vhangups: [0; 2],
            hupped: [false; 2],
            owner: (0, TTY_GID),
            mode: 0o620,
            fs,
            arrivals: [0; 2],
            atime_ns: now,
            mtime_ns: now,
            ctime_ns: now,
        }
    }

    // ---- Input: master writes through the line discipline ----

    /// How many more bytes master writes can hand over now: they wait for the line discipline in
    /// [`Tty::unprocessed`]. None while TCOOFF stops the master's output.
    pub fn input_room(&self) -> usize {
        if self.master_stopped {
            return 0;
        }
        UNPROCESSED_MAX.saturating_sub(self.unprocessed.len())
    }

    /// A master write (input from the terminal); returns the signals to send to the foreground
    /// process group. The caller keeps `data` within [`Tty::input_room`].
    pub fn write_input(&mut self, data: &[u8]) -> Vec<Signal> {
        self.unprocessed.extend(data);
        self.process_input();
        self.take_signals()
    }

    /// TIOCSTI on the slave: the byte goes to the line discipline directly, ahead of input not
    /// taken yet, and is dropped when there is no room (Linux's `tiocsti` calls `receive_buf`).
    pub fn inject_input(&mut self, c: u8) -> Vec<Signal> {
        if self.ldisc_room() > 0 {
            self.input_char(c);
            self.commit_echoes();
            self.arrivals[0] += 1;
        }
        self.take_signals()
    }

    /// The end of file the host console sends when its input has ended and the program has read
    /// everything before it (`terminal::send_eof`): VEOF, as util-linux's script sends it. With
    /// nothing being edited in canonical mode it is an empty line ended by VEOF, which reads as end
    /// of file; marked so that if the program leaves canonical mode before reading it, it arrives
    /// as the VEOF character, which a shell's line editor takes as end of file. Linux would leave a
    /// NUL there instead, which the line editor inserts into its line (script has that problem).
    /// Otherwise it is VEOF typed as input. Nothing when VEOF is disabled.
    pub fn inject_eof(&mut self) -> Vec<Signal> {
        let eof = self.termios.cc[VEOF];
        if eof == 0 {
            return Vec::new();
        }
        if self.canonical() && self.editing.is_empty() && self.unprocessed.is_empty() {
            self.lines.push_back(Line {
                data: Vec::new(),
                eof: true,
                injected: true,
            });
            self.arrivals[0] += 1;
            return Vec::new();
        }
        self.write_input(&[eof])
    }

    /// Takes the signals input has generated since the last call (also by input taken when the
    /// slave read or termios changed).
    pub fn take_signals(&mut self) -> Vec<Signal> {
        std::mem::take(&mut self.signals)
    }

    /// Whether input is waiting for the slave: readable now, or not taken by the line discipline
    /// yet.
    pub fn input_pending(&self) -> bool {
        self.readable() || !self.unprocessed.is_empty()
    }

    /// Hands as much unprocessed input to the line discipline as it has room for, then commits the
    /// echoes (Linux's `flush_to_ldisc` into `n_tty_receive_buf`).
    fn process_input(&mut self) {
        let mut taken = false;
        while !self.unprocessed.is_empty() && self.ldisc_room() > 0 {
            let c = self.unprocessed.pop_front().unwrap();
            self.input_char(c);
            taken = true;
        }
        self.commit_echoes();
        if taken {
            self.arrivals[0] += 1;
        }
    }

    /// Room in the line discipline's buffer (Linux's `n_tty_receive_buf_common`): one byte stays
    /// free; in canonical mode with no complete line one byte is always taken, so a full line
    /// still lets its terminator or an erase through (other characters are dropped).
    fn ldisc_room(&self) -> usize {
        let room = (INPUT_MAX - 1).saturating_sub(self.pending_input());
        if room == 0 && self.canonical() && self.lines.is_empty() {
            1
        } else {
            room
        }
    }

    /// Bytes in the line discipline's buffer, counting a VEOF kept at the end of a line.
    fn pending_input(&self) -> usize {
        let lines: usize = self
            .lines
            .iter()
            .map(|l| l.data.len() + l.eof as usize)
            .sum();
        self.editing.len() + lines + self.raw.len()
    }

    /// One byte of input (Linux's `n_tty_receive_buf_standard`): ISTRIP and IUCLC first; after
    /// VLNEXT the byte is literal, with EXTPROC it is queued as it is, and otherwise characters
    /// that may be special ([`Tty::special`]) are told apart from ordinary ones.
    fn input_char(&mut self, c: u8) {
        let t = self.termios;
        let c = if t.i(ISTRIP) { c & 0x7f } else { c };
        let c = if t.i(IUCLC) && t.l(IEXTEN) {
            to_lower(c)
        } else {
            c
        };
        if std::mem::take(&mut self.lnext) {
            self.receive(c);
        } else if t.l(EXTPROC) {
            self.raw.push_back(c);
        } else if self.special(c) {
            self.receive_special(c);
        } else {
            self.receive(c);
        }
    }

    /// Whether `c` may need special handling under the current termios (Linux's `char_map`):
    /// CR and NL when translated, the editing and line-end characters in canonical mode, the flow
    /// control characters with IXON and the signal characters with ISIG. NUL never is.
    fn special(&self, c: u8) -> bool {
        let t = &self.termios;
        let any = |which: &[usize]| which.iter().any(|&i| t.cc[i] == c);
        c != 0
            && (c == b'\r' && (t.i(IGNCR) || t.i(ICRNL))
                || c == b'\n' && t.i(INLCR)
                || t.l(ICANON)
                    && (c == b'\n'
                        || any(&[VERASE, VKILL, VEOF, VEOL])
                        || t.l(IEXTEN)
                            && (any(&[VWERASE, VLNEXT, VEOL2]) || t.l(ECHO) && any(&[VREPRINT])))
                || t.i(IXON) && any(&[VSTART, VSTOP])
                || t.l(ISIG) && any(&[VINTR, VQUIT, VSUSP]))
    }

    /// An ordinary character (Linux's `n_tty_receive_char`): restarts output with IXANY, is echoed
    /// and queued. In canonical mode a full line drops it.
    fn receive(&mut self, c: u8) {
        self.restart_on_any();
        if !self.fits(c) {
            return;
        }
        if self.termios.l(ECHO) {
            self.finish_erasing();
            self.mark_line_start();
            self.echo_char(c);
        }
        self.put(c);
    }

    /// A character that may be special (Linux's `n_tty_receive_char_special`), in this order: flow
    /// control (IXON), signal characters (ISIG), IXANY, CR and NL translation, then in canonical
    /// mode erasing, VLNEXT, VREPRINT and the line ends; anything left is queued as an ordinary
    /// character, a newline echoed as itself even with ECHOCTL.
    fn receive_special(&mut self, mut c: u8) {
        let t = self.termios;
        if t.i(IXON) {
            if t.is(c, VSTART) {
                self.start();
                return;
            }
            if t.is(c, VSTOP) {
                self.stop();
                return;
            }
        }
        if t.l(ISIG) {
            let sig = [
                (VINTR, Signal::INT),
                (VQUIT, Signal::QUIT),
                (VSUSP, Signal::TSTP),
            ]
            .into_iter()
            .find(|&(i, _)| t.is(c, i));
            if let Some((_, sig)) = sig {
                self.signal_char(sig, c);
                return;
            }
        }
        self.restart_on_any();
        if c == b'\r' {
            if t.i(IGNCR) {
                return;
            }
            if t.i(ICRNL) {
                c = b'\n';
            }
        } else if c == b'\n' && t.i(INLCR) {
            c = b'\r';
        }
        if t.l(ICANON) {
            if t.is(c, VERASE) || t.is(c, VKILL) || t.l(IEXTEN) && t.is(c, VWERASE) {
                self.erase(c);
                return;
            }
            if t.l(IEXTEN) && t.is(c, VLNEXT) {
                self.lnext = true;
                if t.l(ECHO) {
                    self.finish_erasing();
                    if t.l(ECHOCTL) {
                        self.echoes.extend([Echo::Char(b'^'), Echo::Char(b'\x08')]);
                    }
                }
                return;
            }
            if t.l(IEXTEN) && t.l(ECHO) && t.is(c, VREPRINT) {
                self.finish_erasing();
                self.echo_char(c);
                self.echoes.push(Echo::Char(b'\n'));
                for b in self.editing.clone() {
                    self.echo_char(b);
                }
                return;
            }
            if c == b'\n' {
                if t.l(ECHO) || t.l(ECHONL) {
                    self.echoes.push(Echo::Char(b'\n'));
                }
                self.end_line(Some(c));
                return;
            }
            if t.is(c, VEOF) {
                self.end_line(None);
                return;
            }
            // VEOL and VEOL2 are echoed like ordinary characters, without closing an ECHOPRT
            // erase
            if t.is(c, VEOL) || t.l(IEXTEN) && t.is(c, VEOL2) {
                if t.l(ECHO) {
                    self.mark_line_start();
                    self.echo_char(c);
                }
                if c == 0xff && t.i(PARMRK) {
                    self.editing.push(c);
                }
                self.end_line(Some(c));
                return;
            }
        }
        if !self.fits(c) {
            return;
        }
        if t.l(ECHO) {
            self.finish_erasing();
            if c == b'\n' {
                self.echoes.push(Echo::Char(b'\n'));
            } else {
                self.mark_line_start();
                self.echo_char(c);
            }
        }
        self.put(c);
    }

    /// A signal character (Linux's `n_tty_receive_signal_char` and `isig`): unless NOFLSH, the
    /// echoes not written yet, the output the master has not read and the input are discarded
    /// (reported in packet mode); output stopped by ^S restarts (IXON); the character is echoed.
    ///
    /// Linux drops only the output the master side has not taken from the PTY's buffer yet, which
    /// a workqueue moves on asynchronously; a signal character typed right after other input
    /// catches its echoes still there, as the recorded scenarios show, so all unread output goes.
    fn signal_char(&mut self, sig: Signal, c: u8) {
        self.signals.push(sig);
        if !self.termios.l(NOFLSH) {
            self.echoes.clear();
            self.clear_output();
            self.flush_input();
            self.report_flush(true, true);
        }
        if self.termios.i(IXON) {
            self.start();
        }
        if self.termios.l(ECHO) {
            self.echo_char(c);
        }
    }

    /// IXANY: any character restarts output stopped by ^S.
    fn restart_on_any(&mut self) {
        if self.termios.i(IXON) && self.termios.i(IXANY) {
            self.start();
        }
    }

    /// Whether `c` (doubled when it is 0xff and PARMRK is set) fits in the canonical line being
    /// edited, which keeps one byte for its terminator; anything fits outside canonical mode.
    fn fits(&self, c: u8) -> bool {
        let n = if c == 0xff && self.termios.i(PARMRK) {
            2
        } else {
            1
        };
        !self.canonical() || self.editing.len() + n < INPUT_MAX
    }

    /// Queues `c` (Linux's `put_tty_queue`): to the line being edited in canonical mode, readable
    /// at once otherwise; with PARMRK a 0xff is queued twice.
    fn put(&mut self, c: u8) {
        let n = if c == 0xff && self.termios.i(PARMRK) {
            2
        } else {
            1
        };
        if self.canonical() {
            self.editing.extend(std::iter::repeat_n(c, n));
        } else {
            self.raw.extend(std::iter::repeat_n(c, n));
        }
    }

    /// Completes the canonical line, ended by `end` (kept in the line) or by VEOF (None).
    fn end_line(&mut self, end: Option<u8>) {
        let mut data = std::mem::take(&mut self.editing);
        data.extend(end);
        self.lines.push_back(Line {
            data,
            eof: end.is_none(),
            injected: false,
        });
    }

    /// Erases from the line being edited (Linux's `eraser`), with the matching echo: one character
    /// for VERASE, a word for VWERASE (back over one run of letters, digits and underscores, Latin-1
    /// included, after any other characters: BSD's ALTWERASE), the whole line for VKILL. With IUTF8
    /// a character is all its bytes, and a stray continuation byte is not erased. VKILL without
    /// ECHO, or without all of ECHOK, ECHOKE and ECHOE, empties the line at once and echoes the
    /// character itself (and a newline with ECHOK).
    fn erase(&mut self, c: u8) {
        #[derive(PartialEq)]
        enum Kind {
            Char,
            Word,
            Line,
        }
        let t = self.termios;
        if self.editing.is_empty() {
            return;
        }
        let kind = if t.cc[VERASE] == c {
            Kind::Char
        } else if t.cc[VWERASE] == c {
            Kind::Word
        } else {
            if !t.l(ECHO) {
                self.editing.clear();
                return;
            }
            if !t.l(ECHOK) || !t.l(ECHOKE) || !t.l(ECHOE) {
                self.editing.clear();
                self.finish_erasing();
                self.echo_char(t.cc[VKILL]);
                if t.l(ECHOK) {
                    self.echoes.push(Echo::Char(b'\n'));
                }
                return;
            }
            Kind::Line
        };
        let mut seen_alnums = 0;
        while !self.editing.is_empty() {
            let mut head = self.editing.len() - 1;
            while head > 0 && self.continuation(self.editing[head]) {
                head -= 1;
            }
            let first = self.editing[head];
            if self.continuation(first) {
                break;
            }
            if kind == Kind::Word {
                if is_alnum(first) || first == b'_' {
                    seen_alnums += 1;
                } else if seen_alnums > 0 {
                    break;
                }
            }
            let erased = self.editing.split_off(head);
            if t.l(ECHO) {
                self.echo_erased(&erased, kind == Kind::Char);
            }
            if kind == Kind::Char {
                break;
            }
        }
        if self.editing.is_empty() && t.l(ECHO) {
            self.finish_erasing();
        }
    }

    /// The echo for an erased character (all its bytes in `erased`): with ECHOPRT the character
    /// itself after an opening `\`; for a single VERASE without ECHOE the erase character; for a
    /// tab, backspaces back to where it started; otherwise a backspace-space-backspace for each
    /// column it took (none for a control character echoed without ECHOCTL).
    fn echo_erased(&mut self, erased: &[u8], single: bool) {
        let t = self.termios;
        let c = erased[0];
        if t.l(ECHOPRT) {
            if !self.erasing {
                self.echoes.push(Echo::Char(b'\\'));
                self.erasing = true;
            }
            self.echo_char(c);
            for &b in &erased[1..] {
                self.echoes.extend([Echo::Char(b), Echo::MoveBackColumn]);
            }
        } else if single && !t.l(ECHOE) {
            self.echo_char(t.cc[VERASE]);
        } else if c == b'\t' {
            let (mut columns, mut after_tab) = (0, false);
            for &b in self.editing.iter().rev() {
                if b == b'\t' {
                    after_tab = true;
                    break;
                }
                if is_ctrl(b) {
                    columns += if t.l(ECHOCTL) { 2 } else { 0 };
                } else if !self.continuation(b) {
                    columns += 1;
                }
            }
            self.echoes.push(Echo::EraseTab { columns, after_tab });
        } else {
            let width = match (is_ctrl(c), t.l(ECHOCTL)) {
                (true, true) => 2,
                (true, false) => 0,
                (false, _) => 1,
            };
            for _ in 0..width {
                self.echoes
                    .extend([Echo::Char(b'\x08'), Echo::Char(b' '), Echo::Char(b'\x08')]);
            }
        }
    }

    /// Echoes `c` (Linux's `echo_char`): with ECHOCTL a control character other than tab shows as
    /// `^X`.
    fn echo_char(&mut self, c: u8) {
        let echo = if self.termios.l(ECHOCTL) && is_ctrl(c) && c != b'\t' {
            Echo::Ctrl(c)
        } else {
            Echo::Char(c)
        };
        self.echoes.push(echo);
    }

    /// Closes an open ECHOPRT erase with `/`.
    fn finish_erasing(&mut self) {
        if std::mem::take(&mut self.erasing) {
            self.echoes.push(Echo::Char(b'/'));
        }
    }

    /// Before echoing the first character of a canonical line: the line starts at the column the
    /// cursor will then be at.
    fn mark_line_start(&mut self) {
        if self.canonical() && self.editing.is_empty() {
            self.echoes.push(Echo::SetCanonColumn);
        }
    }

    /// Writes the waiting echoes to the output unless output is stopped (Linux's
    /// `process_echoes`).
    fn commit_echoes(&mut self) {
        // n_tty's echo buffer holds 4096 bytes and drops the oldest echoes when full
        if self.echoes.len() > ECHO_MAX {
            self.echoes.drain(..self.echoes.len() - ECHO_MAX);
        }
        if self.stopped || self.echoes.is_empty() {
            return;
        }
        // __process_echoes: echoes go out only while the output queue has room for each whole;
        // the rest wait for the next commit
        let mut done = 0;
        while let Some(&echo) = self.echoes.get(done) {
            let need = match echo {
                Echo::Char(0xff) => 1,
                Echo::Char(c) => self.processed_len(c),
                Echo::Ctrl(_) => 2,
                Echo::EraseTab { columns, after_tab } => {
                    let columns = columns + if after_tab { 0 } else { self.canon_column };
                    8 - columns % 8
                }
                Echo::SetCanonColumn | Echo::MoveBackColumn => 0,
            };
            if need > OUTPUT_MAX.saturating_sub(self.output.len()) {
                break;
            }
            done += 1;
            match echo {
                Echo::Char(0xff) => {
                    self.output.push_back(0xff);
                    self.column += 1;
                }
                Echo::Char(c) => self.output_char(c),
                Echo::Ctrl(c) => {
                    self.output.extend([b'^', c ^ 0x40]);
                    self.column += 2;
                }
                Echo::EraseTab { columns, after_tab } => {
                    let columns = columns + if after_tab { 0 } else { self.canon_column };
                    for _ in 0..8 - columns % 8 {
                        self.output.push_back(b'\x08');
                        self.column = self.column.saturating_sub(1);
                    }
                }
                Echo::SetCanonColumn => self.canon_column = self.column,
                Echo::MoveBackColumn => self.column = self.column.saturating_sub(1),
            }
        }
        self.echoes.drain(..done);
        if done > 0 {
            self.arrivals[1] += 1;
        }
    }

    /// How many bytes output processing makes of `c` now ([`Tty::output_char`]).
    fn processed_len(&self, c: u8) -> usize {
        let t = self.termios;
        match c {
            _ if !t.o(OPOST) => 1,
            b'\n' if t.o(ONLCR) => 2,
            b'\r' if t.o(ONOCR) && self.column == 0 => 0,
            b'\t' if t.oflag & TABDLY == XTABS => 8 - self.column % 8,
            _ => 1,
        }
    }

    // ---- Slave reads ----

    /// Whether the slave has data to read now (in canonical mode a complete line is needed).
    pub fn readable(&self) -> bool {
        if self.canonical() {
            !self.lines.is_empty()
        } else {
            !self.raw.is_empty()
        }
    }

    /// Whether poll reports the slave readable (Linux's `input_available_p` for poll): in
    /// non-canonical mode with VTIME 0 and VMIN above 0, only once VMIN bytes have arrived, the
    /// amount a blocking read waits for; otherwise as [`readable`](Self::readable).
    pub fn poll_readable(&self) -> bool {
        let (vmin, vtime) = (self.termios.cc[VMIN] as usize, self.termios.cc[VTIME]);
        if self.canonical() || vtime != 0 || vmin == 0 {
            self.readable()
        } else {
            self.raw.len() >= vmin
        }
    }

    /// The number of bytes that have arrived in non-canonical mode (for VMIN).
    pub fn raw_len(&self) -> usize {
        self.raw.len()
    }

    /// One slave read: at most one line in canonical mode, returning 0 at EOF. Returns None when
    /// there is no data. The room it makes lets waiting input in ([`Tty::take_signals`]).
    pub fn read(&mut self, buf: &mut [u8]) -> Option<usize> {
        let n = if self.canonical() {
            let line = self.lines.front_mut()?;
            let n = buf.len().min(line.data.len());
            buf[..n].copy_from_slice(&line.data[..n]);
            line.data.drain(..n);
            // The VEOF ending a line goes with the last of its data when the buffer has room for
            // it too (Linux's `canon_copy_from_read_buf`); otherwise the next read returns 0
            if line.data.is_empty() && (!line.eof || n < buf.len()) {
                self.lines.pop_front();
            }
            n
        } else {
            if self.raw.is_empty() {
                return None;
            }
            let n = buf.len().min(self.raw.len());
            for (d, s) in buf.iter_mut().zip(self.raw.drain(..n)) {
                *d = s;
            }
            n
        };
        self.process_input();
        Some(n)
    }

    // ---- Output: slave writes ----

    /// How many more bytes the output queue can take: none while output is stopped (Linux's
    /// `pty_write_room`).
    pub fn output_room(&self) -> usize {
        if self.stopped {
            return 0;
        }
        OUTPUT_MAX.saturating_sub(self.output.len())
    }

    /// Before a slave write's data: echoes still waiting for room go out first, as
    /// `n_tty_write` starts with `process_echoes`. Whether any output was added.
    pub fn echoes_before_write(&mut self) -> bool {
        let before = self.output.len();
        self.commit_echoes();
        self.output.len() != before
    }

    /// A slave write: goes through output processing into the output queue.
    pub fn write_output(&mut self, data: &[u8]) {
        for &c in data {
            self.output_char(c);
        }
        self.arrivals[1] += 1;
    }

    /// A slave write by a process group that is not the terminal's foreground one: as
    /// [`Tty::write_output`], and remembers which bytes of the output queue it added.
    pub fn write_background(&mut self, data: &[u8]) {
        let from = self.taken + self.output.len() as u64;
        self.write_output(data);
        let to = self.taken + self.output.len() as u64;
        match self.background.back_mut() {
            Some(last) if last.end == from => last.end = to,
            _ if to > from => self.background.push_back(from..to),
            _ => {}
        }
    }

    /// One byte of output (Linux's `do_output_char`), following the cursor column: with OPOST, NL
    /// becomes CR NL (ONLCR) and returns the carriage (ONLRET), CR is dropped at column 0 (ONOCR)
    /// or becomes NL (OCRNL), tabs expand to spaces (XTABS), and other printable characters go
    /// upper case (OLCUC); a UTF-8 continuation byte (IUTF8) takes no column, a control character
    /// none. Without OPOST it goes out as it is.
    fn output_char(&mut self, mut c: u8) {
        let t = self.termios;
        if !t.o(OPOST) {
            self.output.push_back(c);
            return;
        }
        match c {
            b'\n' => {
                if t.o(ONLRET) {
                    self.column = 0;
                }
                if t.o(ONLCR) {
                    self.column = 0;
                    self.canon_column = 0;
                    self.output.extend(b"\r\n");
                    return;
                }
                self.canon_column = self.column;
            }
            b'\r' => {
                if t.o(ONOCR) && self.column == 0 {
                    return;
                }
                if t.o(OCRNL) {
                    c = b'\n';
                    if t.o(ONLRET) {
                        self.column = 0;
                        self.canon_column = 0;
                    }
                } else {
                    self.column = 0;
                    self.canon_column = 0;
                }
            }
            b'\t' => {
                let spaces = 8 - self.column % 8;
                self.column += spaces;
                if t.oflag & TABDLY == XTABS {
                    self.output.extend(std::iter::repeat_n(b' ', spaces));
                    return;
                }
            }
            b'\x08' => self.column = self.column.saturating_sub(1),
            _ if !is_ctrl(c) => {
                if t.o(OLCUC) {
                    c = to_upper(c);
                }
                if !self.continuation(c) {
                    self.column += 1;
                }
            }
            _ => {}
        }
        self.output.push_back(c);
    }

    /// Whether `c` is a UTF-8 continuation byte, which IUTF8 treats as part of the character before.
    fn continuation(&self, c: u8) -> bool {
        self.termios.i(IUTF8) && c & 0xc0 == 0x80
    }

    // ---- Master reads ----

    /// The status byte one master read returns in packet mode; takes it.
    pub fn take_pktstatus(&mut self) -> Option<u8> {
        if self.packet && self.pktstatus != 0 {
            Some(std::mem::take(&mut self.pktstatus))
        } else {
            None
        }
    }

    /// Whether the master has output to read.
    pub fn has_output(&self) -> bool {
        !self.output.is_empty()
    }

    /// One master read: takes output.
    pub fn take_output(&mut self, max: usize) -> Vec<u8> {
        let n = max.min(self.output.len());
        self.taken += n as u64;
        while self.background.front().is_some_and(|b| b.end <= self.taken) {
            self.background.pop_front();
        }
        self.output.drain(..n).collect()
    }

    /// One read by the host console: takes output up to where its writer changes between the
    /// foreground process group (or the line discipline's echo) and another, and says whether
    /// another wrote it, so the app's terminal can tell a background job's output at the shell's
    /// prompt from the shell's own (the android/vtcore map).
    pub fn take_output_run(&mut self, max: usize) -> (Vec<u8>, bool) {
        let (run, background) = match self.background.front() {
            Some(b) if b.start <= self.taken => (b.end - self.taken, true),
            Some(b) => (b.start - self.taken, false),
            None => (u64::MAX, false),
        };
        let max = max.min(usize::try_from(run).unwrap_or(usize::MAX));
        (self.take_output(max), background)
    }

    /// After output was dropped from the queue's end: forgets who wrote it.
    fn clip_background(&mut self) {
        let end = self.taken + self.output.len() as u64;
        self.background.retain_mut(|b| {
            b.end = b.end.min(end);
            b.start < b.end
        });
    }

    // ---- ioctl helpers ----

    /// Changes termios; returns the signals generated by input it lets in. The address bit stays
    /// (Linux's `tty_set_termios`), and a PTY is always 8 bits, without parity, with the receiver
    /// enabled (`pty_set_termios`). As Linux's `n_tty_set_termios` does: switching ICANON or EXTPROC
    /// ends LNEXT and an ECHOPRT erase, and input already queued changes queue: from canonical
    /// mode the lines become raw data, a VEOF in it a NUL (the host console's end of file the VEOF
    /// character instead, [`Tty::inject_eof`]); into canonical mode all of it becomes
    /// one line, readable at once, with a final NUL taken as its VEOF. Clearing IXON restarts
    /// output that ^S stopped.
    pub fn set_termios(&mut self, mut t: Termios) -> Vec<Signal> {
        t.cflag ^= (t.cflag ^ self.termios.cflag) & ADDRB;
        t.cflag = t.cflag & !(CSIZE | PARENB) | CS8 | CREAD;
        // In packet mode report ^S/^Q flow control being switched, and with EXTPROC report that
        // termios changed (`pty_set_termios`)
        let flow = |t: &Termios| t.i(IXON) && t.cc[VSTOP] == 0o23 && t.cc[VSTART] == 0o21;
        let (old_flow, new_flow) = (flow(&self.termios), flow(&t));
        let extproc = (self.termios.lflag | t.lflag) & EXTPROC != 0;
        if old_flow != new_flow {
            let set = if new_flow { pkt::DOSTOP } else { pkt::NOSTOP };
            self.report(set, pkt::DOSTOP | pkt::NOSTOP);
        }
        if extproc {
            self.report(pkt::IOCTL, 0);
        }
        let (old, was) = (self.termios, self.canonical());
        self.termios = t;
        if (old.lflag ^ t.lflag) & (ICANON | EXTPROC) != 0 {
            self.lnext = false;
            self.erasing = false;
        }
        match (was, self.canonical()) {
            (true, false) => {
                for line in std::mem::take(&mut self.lines) {
                    self.raw.extend(line.data);
                    if line.eof {
                        self.raw
                            .push_back(if line.injected { old.cc[VEOF] } else { 0 });
                    }
                }
                self.raw.extend(self.editing.drain(..));
            }
            (false, true) if !self.raw.is_empty() => {
                let mut data: Vec<u8> = self.raw.drain(..).collect();
                let eof = data.last() == Some(&0);
                if eof {
                    data.pop();
                }
                self.lines.push_back(Line {
                    data,
                    eof,
                    injected: false,
                });
            }
            _ => {}
        }
        if old.i(IXON) && !t.i(IXON) {
            self.start();
        }
        self.process_input();
        self.take_signals()
    }

    /// Flushes the line discipline's input (TCIFLUSH, TCSETSF, signal characters; Linux's
    /// `n_tty_flush_buffer`) and an open ECHOPRT erase. Input not taken by the line discipline yet
    /// stays ([`Tty::discard_unprocessed`]).
    pub fn flush_input(&mut self) {
        self.editing.clear();
        self.lines.clear();
        self.raw.clear();
        self.erasing = false;
    }

    /// Discards master writes not taken by the line discipline yet (TCIFLUSH on the slave, TCOFLUSH
    /// on the master: Linux's `tty_buffer_flush` of the slave's flip buffer).
    pub fn discard_unprocessed(&mut self) {
        self.unprocessed.clear();
    }

    /// TCOFLUSH on the slave (Linux's `pty_flush_buffer`): drops only the output the master side
    /// has not taken yet ([`PASSED_ON`]).
    pub fn flush_output(&mut self) {
        self.output.truncate(PASSED_ON);
        self.clip_background();
    }

    /// TCIFLUSH on the master, a hangup of the master side and signal characters: all output the
    /// master has not read.
    pub fn clear_output(&mut self) {
        self.output.clear();
        self.clip_background();
    }

    /// In packet mode, reports a flush on the slave side (Linux's `n_tty_packet_mode_flush` for
    /// input, `pty_flush_buffer` for output). Flushes on the master side are not reported.
    pub fn report_flush(&mut self, input: bool, output: bool) {
        let set = if input { pkt::FLUSHREAD } else { 0 } | if output { pkt::FLUSHWRITE } else { 0 };
        if set != 0 {
            self.report(set, 0);
        }
    }

    /// TCOOFF on the slave.
    pub fn stop_output(&mut self) {
        if !self.tco_stopped {
            self.tco_stopped = true;
            self.stop();
        }
    }

    /// TCOON on the slave: output stopped by ^S restarts too.
    pub fn start_output(&mut self) {
        if self.tco_stopped {
            self.tco_stopped = false;
            self.start();
        }
    }

    /// Stops the slave's output (^S, Linux's `__stop_tty`; `pty_stop` reports STOP).
    fn stop(&mut self) {
        if !self.stopped {
            self.stopped = true;
            self.report(pkt::STOP, pkt::START);
        }
    }

    /// Restarts the slave's output (^Q, IXANY, signal characters, clearing IXON; Linux's
    /// `__start_tty`, `pty_start` reports START) and writes the echoes that waited; output stopped
    /// by TCOOFF is left alone.
    fn start(&mut self) {
        if self.stopped && !self.tco_stopped {
            self.stopped = false;
            self.report(pkt::START, pkt::STOP);
            self.commit_echoes();
        }
    }

    /// TCIOFF and TCION on the slave send the STOP or START character to the master (Linux's
    /// `tty_send_xchar`): output stopped by ^S restarts for that one character and stops again
    /// (reported in packet mode); while TCOOFF holds it the character is lost.
    pub fn send_xchar(&mut self, c: u8) {
        // A full output queue drops it (pty_write takes what fits)
        if self.tco_stopped || self.output.len() >= OUTPUT_MAX {
            return;
        }
        if self.stopped {
            self.report(pkt::STOP, pkt::START);
        }
        self.output.push_back(c);
        self.arrivals[1] += 1;
    }

    /// TIOCSTI on the master: the byte is added to what the master reads (Linux's `tiocsti` feeds
    /// the master's own line discipline).
    pub fn inject_output(&mut self, c: u8) {
        self.output.push_back(c);
        self.arrivals[1] += 1;
    }

    /// In packet mode, records status `set` and removes `clear`, which makes the master's readers
    /// ready (Linux records it only when the master is in packet mode).
    fn report(&mut self, set: u8, clear: u8) {
        if self.packet {
            self.pktstatus = self.pktstatus & !clear | set;
            self.arrivals[1] += 1;
        }
    }

    // ---- Accessors ----

    /// The number of readable bytes in the input queue (FIONREAD), without the VEOF ending a line.
    pub fn input_len(&self) -> usize {
        if self.canonical() {
            self.lines.iter().map(|l| l.data.len()).sum()
        } else {
            self.raw.len()
        }
    }

    /// The number of bytes in the output queue (TIOCOUTQ).
    pub fn output_len(&self) -> usize {
        self.output.len()
    }

    /// Whether input is read line by line: canonical mode (ICANON) without EXTPROC, which hands
    /// line editing to the master and makes input readable at once.
    pub fn canonical(&self) -> bool {
        self.termios.l(ICANON) && !self.termios.l(EXTPROC)
    }

    /// Whether a file opened after this side's `born`-th vhangup (on the master when `master`) has
    /// been hung up: for the slave once all masters have closed, and for either side when it was
    /// hung up again by vhangup or TIOCVHANGUP after the open.
    pub fn hung(&self, master: bool, born: u32) -> bool {
        (!master && self.hung_up) || self.vhangups[master as usize] != born
    }

    /// The slave read data (`mtime` false) or wrote data: as with Linux 6.1's `tty_update_time`,
    /// only the seconds change, and only when they differ from the old value by 8 seconds or more;
    /// the exact moment would leak the rhythm of someone's typing.
    pub fn touch(&mut self, mtime: bool) {
        let sec = wall_ns().div_euclid(NS);
        let t = if mtime {
            &mut self.mtime_ns
        } else {
            &mut self.atime_ns
        };
        if (sec ^ t.div_euclid(NS)) & !7 != 0 {
            *t = sec * NS + t.rem_euclid(NS);
        }
    }

    /// The owner, mode or times were changed (chmod, chown, utimensat): the status change time
    /// is now.
    pub fn changed(&mut self) {
        self.ctime_ns = wall_ns();
    }
}

/// One end of a PTY, held by an open file; opening and closing maintain the counts of both ends.
pub struct PtyEnd {
    pub tty: Rc<RefCell<Tty>>,
    /// Whether this is the master.
    pub master: bool,
    /// The devpts mount it was opened through (where the terminal's instance is mounted): the
    /// slave's path (the fd's link, what TIOCGPTPEER opens) is computed from it.
    pub mnt: crate::vfs::MountId,
    /// The open file is in that mount (opened through a path in devpts, or by TIOCGPTPEER): while
    /// it is open, the mount cannot be unmounted (EBUSY). Opening through a device node elsewhere
    /// does not count.
    pub pin: bool,
    /// How many times this side had been vhangup'd at open ([`Tty::hung`]).
    pub born: u32,
}

impl PtyEnd {
    pub fn new(tty: Rc<RefCell<Tty>>, master: bool) -> PtyEnd {
        let born = {
            let mut t = tty.borrow_mut();
            if master {
                t.masters += 1;
            } else {
                t.slaves += 1;
                t.slave_closed = false;
                // After a reopen it can be hung up again (Linux's tty_open clears TTY_HUPPED)
                t.hupped[0] = false;
            }
            t.vhangups[master as usize]
        };
        PtyEnd {
            tty,
            master,
            mnt: crate::vfs::PTS_MOUNT,
            pin: false,
            born,
        }
    }

    /// Opened through devpts mount `mnt`; with `pin`, the open file is in that mount.
    pub fn via(mut self, mnt: crate::vfs::MountId, pin: bool) -> PtyEnd {
        self.mnt = mnt;
        self.pin = pin;
        self
    }
}

impl Drop for PtyEnd {
    fn drop(&mut self) {
        let closed = {
            let mut t = self.tty.borrow_mut();
            if self.master {
                t.masters -= 1;
            } else {
                t.slaves -= 1;
                t.slave_closed = t.slaves == 0;
            }
            self.master && t.masters == 0 && !t.hung_up
        };
        if closed {
            CLOSED.with_borrow_mut(|c| c.push(Rc::downgrade(&self.tty)));
        }
        // The other end must see the hangup
        request_wake_object(wake::of(&self.tty));
    }
}

thread_local! {
    /// Terminals whose masters are all closed but which are not hung up yet. Open files are
    /// closed in Drop, which cannot reach the kernel, so the thread releasing the BKL hangs them
    /// up before releasing it ([`take_closed`]).
    #[allow(clippy::missing_const_for_thread_local)] // already const; Android clippy false positive
    static CLOSED: std::cell::RefCell<Vec<Weak<RefCell<Tty>>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Takes the terminals whose last master this thread closed and which are not hung up yet
/// (recorded by [`PtyEnd`]), for `syscall::tty::hang_up_closed` to hang up.
pub(crate) fn take_closed() -> Vec<Rc<RefCell<Tty>>> {
    if CLOSED.with_borrow(Vec::is_empty) {
        return Vec::new();
    }
    let closed = CLOSED.with_borrow_mut(std::mem::take);
    closed.iter().filter_map(Weak::upgrade).collect()
}

/// Nanoseconds per second.
const NS: i64 = 1_000_000_000;

/// The current wall-clock time (nanoseconds).
pub(crate) fn wall_ns() -> i64 {
    tuxport_hal::clock_gettime(0)
        .map(|(s, ns)| s * NS + ns)
        .unwrap_or(0)
}

/// A control character (the kernel's `iscntrl`): C0 and DEL. ECHOCTL shows them as `^X`, tab
/// excepted, and output processing gives them no column.
fn is_ctrl(c: u8) -> bool {
    c < 0x20 || c == 0x7f
}

/// The kernel's `isalnum` (`lib/ctype.c`, Latin-1): digits, letters, and Latin-1 letters except
/// the multiplication and division signs.
fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, 0xc0..=0xd6 | 0xd8..=0xf6 | 0xf8..=0xff)
}

/// The kernel's `tolower` (IUCLC): Latin-1 upper case letters too.
fn to_lower(c: u8) -> u8 {
    if c.is_ascii_uppercase() || matches!(c, 0xc0..=0xd6 | 0xd8..=0xde) {
        c + 0x20
    } else {
        c
    }
}

/// The kernel's `toupper` (OLCUC): Latin-1 lower case letters too, ß and ÿ included (which it
/// maps to 0xbf and 0xdf).
fn to_upper(c: u8) -> u8 {
    if c.is_ascii_lowercase() || matches!(c, 0xdf..=0xf6 | 0xf8..=0xff) {
        c - 0x20
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// devpts options: defaults, the various spellings, errors, and the mountinfo form, as on
    /// Linux.
    #[test]
    fn devpts_options() {
        let id = |x: u32| Some(x);
        let parse = |d: &[u8]| PtsOptions::parse(d, id, id);
        let o = parse(b"").unwrap();
        assert_eq!(o.show(), ",mode=600,ptmxmode=000");
        let o = parse(b"newinstance,ptmxmode=0666,mode=0620,gid=5").unwrap();
        assert_eq!(o.show(), ",gid=5,mode=620,ptmxmode=666");
        let o = parse(b"uid=7,gid=8,max=1").unwrap();
        assert_eq!((o.uid, o.gid, o.max), (Some(7), Some(8), 1));
        assert_eq!(o.show(), ",uid=7,gid=8,mode=600,ptmxmode=000,max=1");
        for bad in [
            &b"mode=xyz"[..],
            b"bogus",
            b"gid=abc",
            b"max=1048577",
            b"mode",
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        // A uid unmappable in the user namespace
        assert!(PtsOptions::parse(b"uid=7", |_| None, id).is_err());
        assert_eq!(PtsOptions::BOOT.show(), ",gid=5,mode=620,ptmxmode=666");
    }

    fn read_all(t: &mut Tty) -> Vec<u8> {
        let mut buf = [0u8; 256];
        let n = t.read(&mut buf).unwrap_or(0);
        buf[..n].to_vec()
    }

    #[test]
    fn canonical_mode_edits_lines_and_echoes() {
        let mut t = Tty::new(0);
        assert!(t.write_input(b"helo\x7flo wor\x17world\r").is_empty());
        assert!(t.readable());
        assert_eq!(read_all(&mut t), b"hello world\n");
        assert_eq!(
            t.take_output(1024),
            b"helo\x08 \x08lo wor\x08 \x08\x08 \x08\x08 \x08world\r\n"
        );
        // ^D at the start of a line is EOF, ^D in the middle of a line hands over what is there
        t.write_input(b"ab\x04\x04");
        assert_eq!(read_all(&mut t), b"ab");
        assert_eq!(t.read(&mut [0; 8]), Some(0));
        assert!(!t.readable());
        // ^U erases the whole line, ^V makes the next character literal
        t.write_input(b"junk\x15\x16\x03\n");
        assert_eq!(read_all(&mut t), b"\x03\n");
    }

    #[test]
    fn signals_flush_input_and_echo() {
        let mut t = Tty::new(0);
        t.write_input(b"partial");
        t.take_output(1024);
        assert_eq!(t.write_input(b"\x03"), vec![Signal::INT]);
        assert_eq!(t.take_output(1024), b"^C");
        assert_eq!(t.write_input(b"\x1a\x1c"), vec![Signal::TSTP, Signal::QUIT]);
        t.write_input(b"x\n");
        assert_eq!(read_all(&mut t), b"x\n");
        // With ISIG off, ^C is an ordinary character
        let mut raw = t.termios;
        raw.lflag &= !(ISIG | ICANON | ECHO);
        t.set_termios(raw);
        assert!(t.write_input(b"\x03\r").is_empty());
        assert_eq!(read_all(&mut t), b"\x03\n");
    }

    #[test]
    fn switching_modes_keeps_pending_input() {
        let mut t = Tty::new(0);
        t.write_input(b"one\ntw");
        let mut raw = t.termios;
        raw.lflag &= !ICANON;
        t.set_termios(raw);
        assert_eq!(read_all(&mut t), b"one\ntw");
        let mut buf = [0u8; 4];
        t.write_input(b"abcdef");
        assert_eq!(t.read(&mut buf), Some(4));
        assert_eq!(t.raw_len(), 2);
    }

    #[test]
    fn output_processing_and_flow_control() {
        let mut t = Tty::new(0);
        t.write_output(b"a\nb");
        assert_eq!(t.take_output(16), b"a\r\nb");
        t.write_input(b"\x13");
        assert_eq!(t.output_room(), 0);
        t.write_input(b"x");
        assert!(!t.has_output());
        t.write_input(b"\x11");
        assert_eq!(t.take_output(16), b"x");
    }

    /// Output processing follows the cursor column: tabs expand to the next multiple of 8
    /// (XTABS), CR at column 0 is dropped (ONOCR), and erasing a tab moves back to where it
    /// started, counting from the column the line started at (the prompt).
    #[test]
    fn output_and_tab_erase_follow_the_column() {
        let mut t = Tty::new(0);
        let mut tio = t.termios;
        tio.oflag |= XTABS | ONOCR;
        t.set_termios(tio);
        t.write_output(b"ab\tc\n\r$ ");
        assert_eq!(t.take_output(64), b"ab      c\r\n$ ");
        tio.oflag &= !XTABS;
        t.set_termios(tio);
        t.write_input(b"x\t\x7f\x7f\t\x7f");
        // The line starts at column 2: the tab after x took 5 columns, the one at the start 6
        assert_eq!(
            t.take_output(64),
            b"x\t\x08\x08\x08\x08\x08\x08 \x08\t\x08\x08\x08\x08\x08\x08"
        );
    }

    /// The host console reads output in runs by who wrote it: a background write is told apart
    /// from the foreground's and from echoes around it, read in pieces or not, and output dropped
    /// from the queue takes its marks along.
    #[test]
    fn output_runs_follow_their_writers() {
        let mut t = Tty::new(0);
        t.write_output(b"$ ");
        t.write_background(b"one\n");
        t.write_background(b"two\n");
        t.write_input(b"x");
        t.write_output(b"y");
        assert_eq!(t.take_output_run(64), (b"$ ".to_vec(), false));
        assert_eq!(t.take_output_run(3), (b"one".to_vec(), true));
        assert_eq!(t.take_output_run(64), (b"\r\ntwo\r\n".to_vec(), true));
        assert_eq!(t.take_output_run(64), (b"xy".to_vec(), false));
        assert_eq!(t.take_output_run(64), (Vec::new(), false));
        // A plain master read passes over the marks
        t.write_background(b"a");
        t.write_output(b"b");
        t.write_background(b"c");
        assert_eq!(t.take_output(2), b"ab");
        assert_eq!(t.take_output_run(64), (b"c".to_vec(), true));
        // Dropped output is no longer marked
        t.write_background(b"gone");
        t.clear_output();
        t.write_output(b"kept");
        assert_eq!(t.take_output_run(64), (b"kept".to_vec(), false));
    }

    /// Echo and erase rules of n_tty: ECHOCTL shows a newline taken literally as `^J`, ECHOPRT
    /// shows erased characters between `\` and `/`, VWERASE stops at punctuation, and without
    /// ECHOE an erase echoes the erase character.
    #[test]
    fn echoes_follow_linux() {
        let mut t = Tty::new(0);
        t.write_input(b"a\x16\nb\n");
        assert_eq!(t.take_output(64), b"a^\x08^Jb\r\n");
        assert_eq!(read_all(&mut t), b"a\nb\n");
        let mut tio = t.termios;
        tio.lflag |= ECHOPRT;
        t.set_termios(tio);
        t.write_input(b"abc\x7f\x7fd\n");
        assert_eq!(t.take_output(64), b"abc\\cb/d\r\n");
        read_all(&mut t);
        tio.lflag &= !(ECHOPRT | ECHOE);
        t.set_termios(tio);
        t.write_input(b"x.yz\x17\x7f\n");
        assert_eq!(read_all(&mut t), b"x\n");
        assert_eq!(t.take_output(64), b"x.yz\x08 \x08\x08 \x08^?\r\n");
    }

    /// Master writes beyond the line discipline's room wait, and are taken (signal characters
    /// included) as the slave reads; a signal character drops the output not read yet and the
    /// echoes of the same batch, while TCOFLUSH keeps what the master side has taken.
    #[test]
    fn input_waits_for_room_and_signals_flush_like_linux() {
        let mut t = Tty::new(0);
        let mut raw = t.termios;
        raw.lflag &= !(ICANON | ECHO);
        t.set_termios(raw);
        let mut data = vec![b'A'; INPUT_MAX - 1];
        data.push(3);
        assert!(t.write_input(&data).is_empty());
        let mut buf = vec![0u8; INPUT_MAX];
        assert_eq!(t.read(&mut buf), Some(INPUT_MAX - 1));
        assert_eq!(t.take_signals(), vec![Signal::INT]);
        let mut t = Tty::new(0);
        t.write_output(b"unread");
        assert_eq!(t.write_input(b"abc\x03"), vec![Signal::INT]);
        assert_eq!(t.take_output(64), b"^C");
        t.write_output(&vec![b'x'; PASSED_ON + 10]);
        t.flush_output();
        assert_eq!(t.output_len(), PASSED_ON);
    }

    #[test]
    fn reads_and_writes_touch_the_times_every_8_seconds() {
        let mut t = Tty::new(0);
        let sec = wall_ns().div_euclid(NS);
        // A time long ago: changed to the current second, nanoseconds unchanged
        t.atime_ns = 100 * NS + 123;
        t.touch(false);
        assert!((t.atime_ns.div_euclid(NS) - sec).abs() <= 1);
        assert_eq!(t.atime_ns.rem_euclid(NS), 123);
        // Unchanged within the same 8 seconds
        t.mtime_ns = (sec & !7) * NS + 456;
        let before = t.mtime_ns;
        if wall_ns().div_euclid(NS) & !7 == sec & !7 {
            t.touch(true);
            assert_eq!(t.mtime_ns, before);
        }
    }

    #[test]
    fn stopped_output_blocks_the_slave_but_keeps_what_was_written() {
        let mut t = Tty::new(0);
        t.write_output(b"ab");
        t.write_input(b"\x13");
        // What was written before the stop can still be read, the slave cannot write any more,
        // and later echoes wait until restart
        assert_eq!(t.output_room(), 0);
        assert_eq!(t.take_output(1), b"a");
        t.write_input(b"x");
        assert_eq!(t.take_output(16), b"b");
        assert!(!t.has_output());
        t.write_input(b"\x11");
        assert!(t.output_room() > 0);
        assert_eq!(t.take_output(16), b"x");
        // A character sent by TCIOFF passes output stopped by ^S, but is lost under TCOOFF; ^Q
        // cannot restart output stopped by TCOOFF, TCOON is needed
        t.write_input(b"\x13");
        t.send_xchar(0x13);
        assert_eq!(t.take_output(16), b"\x13");
        t.write_input(b"\x11");
        t.stop_output();
        t.write_input(b"\x11");
        assert_eq!(t.output_room(), 0);
        t.send_xchar(0x13);
        assert!(!t.has_output());
        t.start_output();
        assert!(t.output_room() > 0);
    }

    #[test]
    fn utf8_erase_removes_whole_characters() {
        let mut t = Tty::new(0);
        let mut tio = t.termios;
        tio.iflag |= IUTF8;
        t.set_termios(tio);
        t.write_input("aé\x7f\n".as_bytes());
        assert_eq!(read_all(&mut t), b"a\n");
    }

    #[test]
    fn speeds_follow_the_baud_bits() {
        let mut t = Termios::default();
        t.load_speeds();
        assert_eq!((t.ispeed, t.ospeed), (38400, 38400));
        // B115200 (CBAUDEX plus 2) output, B1200 input
        t.cflag = t.cflag & !CBAUD & !(CBAUD << IBSHIFT) | 0o10002 | (0o11 << IBSHIFT);
        t.load_speeds();
        assert_eq!((t.ispeed, t.ospeed), (1200, 115200));
        // BOTHER uses the speed in the field; with B0 it is 0 too
        t.cflag = t.cflag & !CBAUD & !(CBAUD << IBSHIFT) | BOTHER | (BOTHER << IBSHIFT);
        (t.ispeed, t.ospeed) = (12345, 54321);
        t.load_speeds();
        assert_eq!((t.ispeed, t.ospeed), (12345, 54321));
        t.cflag &= !CBAUD & !(CBAUD << IBSHIFT);
        t.load_speeds();
        assert_eq!((t.ispeed, t.ospeed), (0, 0));
    }

    #[test]
    fn pty_keeps_eight_bits_and_the_address_bit() {
        let mut tty = Tty::new(0);
        let mut t = tty.termios;
        t.cflag = t.cflag & !(CSIZE | CREAD) | PARENB | ADDRB;
        tty.set_termios(t);
        assert_eq!(
            tty.termios.cflag & (CSIZE | CREAD | PARENB | ADDRB),
            CS8 | CREAD
        );
    }

    #[test]
    fn termios_round_trip() {
        let t = Termios::default();
        assert_eq!(Termios::decode(&t.encode()), t);
        assert_eq!(t.cc[VINTR], 3);
        assert_eq!(t.cc[VMIN], 1);
        let w = Winsize {
            rows: 50,
            cols: 132,
            xpixel: 0,
            ypixel: 0,
        };
        assert_eq!(Winsize::decode(&w.encode()), w);
    }
}
