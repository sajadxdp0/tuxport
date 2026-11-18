//! Paths the kernel layer does not implement (the kernel/signal map), counted as Starnix's
//! `track_stub!` and `not_found` count their own: the default arms of the syscall dispatcher,
//! prctl, fcntl, ioctl, madvise and futex record each value that reaches them, path resolution the
//! names Linux would have under `/proc`, `/sys` and `/dev` ([`crate::vfs::Pseudo::missing`]), and
//! opening a device node the device numbers without a driver; each with how often, and which
//! program reached it first. Linux 6.1 would have done something there, so a program that fails
//! after reaching one has probably failed because of it.
//!
//! - Reports of kernel bugs carry the paths reached most often ([`top`], [`crate::bug`]), so the
//!   report the app copies for developers also shows what the workload asked for that Tuxport lacks.
//! - With `TUXPORT_LOG=1` the first hit of each path is written to the host's stderr
//!   (`tuxport: unimplemented <path>, first by <comm> (pid <pid>)`), which `scripts/survey`
//!   collects per workload.
//!
//! Nothing reaches the guest: its kernel log stays what Linux would write. Where a default arm also
//! gives Linux's own refusals, those are not counted: terminal ioctls on files that are not
//! terminals ([`ioctl`]), options the kernel layer knows given arguments it refuses (prctl), and
//! names whose absence is a fact rather than a gap (a pid, fd or terminal number that does not
//! exist).

use std::fmt;

use crate::fd::FileKind;
use crate::kernel::{Cx, Kernel, Tid};
use crate::task::Task;
use crate::vfs::dev::Device;

/// The most distinct paths kept per kernel. New paths after that are only counted
/// ([`Stubs::dropped`]), so a program trying every ioctl number or every name cannot grow the
/// table without bound.
const MAX_STUBS: usize = 128;

/// How many paths a kill report carries.
const REPORTED: usize = 10;

/// What [`file_kind`] calls the kinds of file the ioctl rules look at ([`linux_refuses`]).
pub(crate) const TERMINAL: &str = "terminal";
pub(crate) const SOCKET: &str = "socket";
const REGULAR: &str = "regular file";
const DIRECTORY: &str = "directory";
const PSEUDO_FILE: &str = "procfs or sysfs file";

/// A path the kernel layer does not implement, with the value that reached it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stub {
    /// A syscall number the dispatcher does not know (ENOSYS).
    Syscall(u64),
    /// A prctl option (EINVAL).
    Prctl(i32),
    /// A fcntl command (EINVAL).
    Fcntl(u32),
    /// An ioctl request and the kind of file it was made on ([`file_kind`]; ENOTTY).
    Ioctl(u32, &'static str),
    /// madvise advice the kernel layer refuses (EINVAL); of what Linux 6.1 knows, that is KSM,
    /// transparent huge pages and memory failure injection.
    Madvise(i32),
    /// A futex command (ENOSYS); of what Linux 6.1 knows, those of priority-inheritance futexes.
    Futex(u32),
    /// A name path resolution did not find under `/proc`, `/sys` or `/dev` (ENOENT), with
    /// process, thread and device numbers in it replaced by placeholders
    /// (`/proc/<pid>/autogroup`).
    Missing(String),
    /// A device node whose device has no driver (ENXIO): block or character, major, minor.
    Device(bool, u32, u32),
}

/// Records that the current thread reached `stub`.
pub(crate) fn hit(cx: &Cx<'_>, stub: Stub) {
    record(cx.kr(), Some(cx.task), cx.tgid(), stub);
}

/// Records that `task` (None for a host call), of process `pid`, reached `stub`: for callers that
/// have the kernel but no [`Cx`] (procfs views).
pub(crate) fn record(k: &Kernel, task: Option<&Task>, pid: Tid, stub: Stub) {
    // Taken only here and by `top`, neither of which calls back
    let Ok(mut stubs) = k.stubs.try_borrow_mut() else {
        return;
    };
    let first = stubs.hit(&stub, || crate::pseudo::comm_str(task, pid));
    if let Some(comm) = first {
        crate::debug::log(format_args!(
            "unimplemented {stub}, first by {comm} (pid {pid})"
        ));
    }
}

/// An ioctl `request` on a file of kind `on` ([`file_kind`]) reached a default arm (ENOTTY).
/// Requests Linux also refuses on that kind of file are not counted ([`linux_refuses`]).
pub(crate) fn ioctl(cx: &Cx<'_>, request: u32, on: &'static str) {
    if !linux_refuses(request, on) {
        hit(cx, Stub::Ioctl(request, on));
    }
}

/// Whether Linux also answers ENOTTY to `request` on a file of kind `on`: the requests of type
/// `'T'` on files that are not terminals, which `isatty` asks of every file it is given. Of them,
/// sockets answer TIOCOUTQ (SIOCOUTQ), and regular files and directories FIOQSIZE
/// (`do_vfs_ioctl`); FIONREAD has no exception, since every other kind of file Linux answers it for
/// (regular files, pipes, sockets, inotify) answers it here before any default arm.
fn linux_refuses(request: u32, on: &str) -> bool {
    const TIOCOUTQ: u32 = 0x5411;
    const FIOQSIZE: u32 = 0x5460;
    if (request >> 8) & 0xff != u32::from(b'T') || on == TERMINAL {
        return false;
    }
    match request {
        TIOCOUTQ => on != SOCKET,
        FIOQSIZE => !matches!(on, REGULAR | DIRECTORY | PSEUDO_FILE),
        _ => true,
    }
}

/// The kind of file an ioctl was made on, as reports name it.
pub(crate) fn file_kind(kind: &FileKind) -> &'static str {
    match kind {
        FileKind::Host(_) => "host file",
        FileKind::Regular { .. } | FileKind::Fuse(_) => REGULAR,
        FileKind::FuseDev(_) => "/dev/fuse",
        FileKind::Directory { .. } => DIRECTORY,
        FileKind::Path(_) => "O_PATH file",
        FileKind::Pipe(_) => "pipe",
        FileKind::Device { dev, .. } => match dev {
            Device::Null => "/dev/null",
            Device::Zero => "/dev/zero",
            Device::Full => "/dev/full",
            Device::Random => "/dev/random",
            Device::Kmsg => "/dev/kmsg",
            Device::LoopControl => "/dev/loop-control",
            Device::Loop => "loop device",
        },
        FileKind::Pty(_) => TERMINAL,
        FileKind::Socket(_) => SOCKET,
        FileKind::Epoll(_) => "epoll",
        FileKind::EventFd(_) => "eventfd",
        FileKind::TimerFd(_) => "timerfd",
        FileKind::SignalFd(_) => "signalfd",
        FileKind::Inotify(_) => "inotify",
        FileKind::Pidfd(_) => "pidfd",
        FileKind::MessageQueue(_) => "message queue",
        FileKind::SeccompNotify(_) => "seccomp listener",
        FileKind::Bpf(_) => "bpf program",
        FileKind::Proc { .. } => PSEUDO_FILE,
    }
}

/// The paths reached most often since boot, for a kill report: one line each, most calls first
/// (`prctl option 0x59616d61: 2 calls, first by gdb`), and a last line counting the rest. Empty
/// when the table is in use (a bug inside [`record`]).
pub(crate) fn top(k: &Kernel) -> Vec<String> {
    k.stubs
        .try_borrow()
        .map(|s| s.top(REPORTED))
        .unwrap_or_default()
}

// ---- The table ----

/// The paths a kernel's guest has reached ([`crate::kernel::Kernel::stubs`]).
#[derive(Default)]
pub(crate) struct Stubs {
    /// In the order first reached.
    entries: Vec<Entry>,
    /// Hits of new paths after the table was full.
    dropped: u64,
}

struct Entry {
    stub: Stub,
    count: u64,
    /// The name of the program that reached it first.
    first: String,
}

impl Stubs {
    /// Counts a hit of `stub`. For its first hit the program is named by `first`, and is returned.
    fn hit(&mut self, stub: &Stub, first: impl FnOnce() -> String) -> Option<&str> {
        if let Some(i) = self.entries.iter().position(|e| e.stub == *stub) {
            self.entries[i].count += 1;
            return None;
        }
        if self.entries.len() >= MAX_STUBS {
            self.dropped += 1;
            return None;
        }
        self.entries.push(Entry {
            stub: stub.clone(),
            count: 1,
            first: first(),
        });
        self.entries.last().map(|e| e.first.as_str())
    }

    /// The `n` paths with the most calls (among equal counts, the one reached first first), then a
    /// line for the rest if any.
    fn top(&self, n: usize) -> Vec<String> {
        let mut order: Vec<&Entry> = self.entries.iter().collect();
        // A stable sort keeps the order of first hits among equal counts
        order.sort_by(|a, b| b.count.cmp(&a.count));
        let mut lines: Vec<String> = order
            .iter()
            .take(n)
            .map(|e| format!("{}: {}, first by {}", e.stub, calls(e.count), e.first))
            .collect();
        let rest = &order[n.min(order.len())..];
        let more = rest.iter().map(|e| e.count).sum::<u64>() + self.dropped;
        if more > 0 {
            let s = if rest.len() == 1 { "" } else { "s" };
            let full = if self.dropped > 0 {
                " (the table is full: more not named)"
            } else {
                ""
            };
            lines.push(format!(
                "{} more path{s}, {}{full}",
                rest.len(),
                calls(more)
            ));
        }
        lines
    }
}

fn calls(n: u64) -> String {
    if n == 1 {
        "1 call".to_owned()
    } else {
        format!("{n} calls")
    }
}

impl fmt::Display for Stub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Stub::Missing(ref path) => write!(f, "missing {path}"),
            Stub::Syscall(nr) => write!(f, "syscall {nr}"),
            // Options outside the numbered range are four-letter tags (PR_SET_PTRACER is "Yama"),
            // read more easily in hex
            Stub::Prctl(o) if o as u32 >= 0x10000 => write!(f, "prctl option {:#x}", o as u32),
            Stub::Prctl(o) => write!(f, "prctl option {o}"),
            Stub::Fcntl(c) => write!(f, "fcntl command {c}"),
            Stub::Ioctl(r, on) => write!(f, "ioctl {r:#x} ({on})"),
            Stub::Madvise(a) => write!(f, "madvise advice {a}"),
            Stub::Futex(c) => write!(f, "futex command {c}"),
            Stub::Device(block, major, minor) => {
                let kind = if block { "block" } else { "char" };
                write!(f, "{kind} device {major}:{minor}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(s: &mut Stubs, stub: Stub, comm: &str) -> Option<String> {
        s.hit(&stub, || comm.to_owned()).map(str::to_owned)
    }

    #[test]
    fn counts_each_path_and_names_its_first_program() {
        let mut s = Stubs::default();
        assert_eq!(hit(&mut s, Stub::Syscall(293), "bash"), Some("bash".into()));
        assert_eq!(hit(&mut s, Stub::Syscall(293), "node"), None);
        assert_eq!(
            hit(&mut s, Stub::Prctl(0x59616d61), "gdb"),
            Some("gdb".into())
        );
        // The same request on another kind of file is another path
        assert!(hit(&mut s, Stub::Ioctl(0x80086601, REGULAR), "lsattr").is_some());
        assert!(hit(&mut s, Stub::Ioctl(0x80086601, DIRECTORY), "lsattr").is_some());
        let missing = || Stub::Missing("/proc/<pid>/autogroup".into());
        assert!(hit(&mut s, missing(), "top").is_some());
        assert!(hit(&mut s, missing(), "top").is_none());
        assert!(hit(&mut s, Stub::Device(false, 10, 200), "openvpn").is_some());
        assert_eq!(
            s.top(10),
            [
                "syscall 293: 2 calls, first by bash",
                "missing /proc/<pid>/autogroup: 2 calls, first by top",
                "prctl option 0x59616d61: 1 call, first by gdb",
                "ioctl 0x80086601 (regular file): 1 call, first by lsattr",
                "ioctl 0x80086601 (directory): 1 call, first by lsattr",
                "char device 10:200: 1 call, first by openvpn",
            ]
        );
    }

    #[test]
    fn reports_the_most_called_and_counts_the_rest() {
        let mut s = Stubs::default();
        for (i, stub) in [Stub::Fcntl(1026), Stub::Madvise(14), Stub::Futex(6)]
            .into_iter()
            .enumerate()
        {
            for _ in 0..=i {
                hit(&mut s, stub.clone(), "a");
            }
        }
        assert_eq!(
            s.top(2),
            [
                "futex command 6: 3 calls, first by a",
                "madvise advice 14: 2 calls, first by a",
                "1 more path, 1 call",
            ]
        );
        assert!(Stubs::default().top(2).is_empty());
    }

    #[test]
    fn a_full_table_only_counts_new_paths() {
        let mut s = Stubs::default();
        for nr in 0..MAX_STUBS as u64 {
            hit(&mut s, Stub::Syscall(1000 + nr), "fuzz");
        }
        assert_eq!(hit(&mut s, Stub::Prctl(99), "late"), None);
        assert_eq!(hit(&mut s, Stub::Prctl(99), "late"), None);
        // Paths already in it still count
        assert_eq!(hit(&mut s, Stub::Syscall(1000), "fuzz"), None);
        let top = s.top(1);
        assert_eq!(top[0], "syscall 1000: 2 calls, first by fuzz");
        assert_eq!(
            top[1],
            format!(
                "{} more paths, {} calls (the table is full: more not named)",
                MAX_STUBS - 1,
                MAX_STUBS - 1 + 2
            )
        );
    }

    #[test]
    fn terminal_requests_count_only_where_linux_answers_them() {
        const TCGETS: u32 = 0x5401;
        const TCGETS2: u32 = 0x802c_542a;
        const TIOCGWINSZ: u32 = 0x5413;
        const TIOCOUTQ: u32 = 0x5411;
        const FIONREAD: u32 = 0x541b;
        const FIOQSIZE: u32 = 0x5460;
        for on in [REGULAR, SOCKET, "pipe", "/dev/null"] {
            for r in [TCGETS, TCGETS2, TIOCGWINSZ, FIONREAD] {
                assert!(linux_refuses(r, on), "{r:#x} {on}");
            }
        }
        for r in [TCGETS, TIOCOUTQ, FIONREAD, FIOQSIZE] {
            assert!(!linux_refuses(r, TERMINAL), "{r:#x}");
        }
        // SIOCOUTQ on sockets, FIOQSIZE on regular files and directories
        assert!(!linux_refuses(TIOCOUTQ, SOCKET) && linux_refuses(TIOCOUTQ, REGULAR));
        assert!(!linux_refuses(FIOQSIZE, DIRECTORY) && linux_refuses(FIOQSIZE, "pipe"));
        // Requests of other types are left to the default arms' own judgement
        for r in [0x8008_6601, 0x8905] {
            assert!(!linux_refuses(r, REGULAR), "{r:#x}");
        }
    }

    #[test]
    fn prctl_tags_are_shown_in_hex() {
        assert_eq!(Stub::Prctl(35).to_string(), "prctl option 35");
        assert_eq!(
            Stub::Prctl(0x5356_4d41).to_string(),
            "prctl option 0x53564d41"
        );
        assert_eq!(Stub::Prctl(-1).to_string(), "prctl option 0xffffffff");
    }
}
