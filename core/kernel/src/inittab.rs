//! The built-in init runs a subset of `/etc/inittab` (the kernel/boot map): `sysinit`, `boot`,
//! `bootwait`, `wait` and `once` at boot, and at shutdown `shutdown` (busybox format) and the
//! `wait` and `once` entries of runlevels 0 and 6 (sysvinit format). Alpine's inittab runs OpenRC
//! through these steps, and Debian's sysvinit-core runs `/etc/init.d/rc`. There is no terminal, so
//! `respawn` and `askfirst` (getty) never run: login shells are the app's tabs.
//!
//! Each entry is run by `/bin/sh -c` (as busybox init also does when it meets shell special
//! characters), with stdin `/dev/null` and stdout and stderr written to [`LOG`]: Tuxport has no
//! console.

/// Output of the inittab commands at boot and shutdown. Truncated at every boot.
pub const LOG: &str = "/var/log/boot.log";

/// Environment variables of the inittab commands, the same as busybox init sets.
pub(crate) const ENV: [&[u8]; 5] = [
    b"HOME=/",
    b"PATH=/sbin:/usr/sbin:/bin:/usr/bin",
    b"SHELL=/bin/sh",
    b"USER=root",
    b"TERM=linux",
];

/// How to shut down: busybox's reboot, halt and poweroff without `-f` send SIGTERM, SIGUSR1 and
/// SIGUSR2 to process 1, and init runs the shutdown commands and ends every process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Reboot,
    Halt,
    PowerOff,
}

impl Power {
    /// busybox init's signal handling: SIGTERM reboots, SIGUSR1 halts, SIGUSR2 powers off.
    pub(crate) fn from_signal(sig: crate::process::Signal) -> Option<Power> {
        use crate::process::Signal;
        match sig {
            Signal::TERM => Some(Power::Reboot),
            Signal::USR1 => Some(Power::Halt),
            Signal::USR2 => Some(Power::PowerOff),
            _ => None,
        }
    }

    /// sysvinit runlevels: halt and poweroff are 0, reboot is 6.
    fn runlevel(self) -> u8 {
        match self {
            Power::Reboot => b'6',
            Power::Halt | Power::PowerOff => b'0',
        }
    }
}

/// One inittab entry: `id:runlevels:action:process`. In busybox format the first column is the
/// terminal and the second is always empty; in sysvinit format the second column is the
/// runlevels, and empty means all runlevels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub runlevels: Vec<u8>,
    pub action: Vec<u8>,
    pub process: Vec<u8>,
}

impl Entry {
    fn is(&self, action: &[u8]) -> bool {
        self.action == action
    }

    /// Belongs to runlevel `level`: an entry without runlevels belongs to all of them.
    fn in_level(&self, level: u8) -> bool {
        self.runlevels.is_empty() || self.runlevels.contains(&level)
    }
}

/// Parses inittab: lines starting with `#` and empty lines are skipped, as are lines with fewer
/// than four columns; the fourth column (the command) may contain `:`.
pub(crate) fn parse(text: &[u8]) -> Vec<Entry> {
    text.split(|&b| b == b'\n')
        .filter_map(|line| {
            let line = line.trim_ascii();
            if line.is_empty() || line[0] == b'#' {
                return None;
            }
            let mut fields = line.splitn(4, |&b| b == b':');
            let _id = fields.next()?;
            let runlevels = fields.next()?.trim_ascii().to_vec();
            let action = fields.next()?.trim_ascii().to_vec();
            let process = fields.next()?.trim_ascii().to_vec();
            Some(Entry {
                runlevels,
                action,
                process,
            })
        })
        .collect()
}

/// The commands run in order at boot and whether to wait for each to finish: first `sysinit`, then
/// `boot` and `bootwait`, then the `wait` entries of the default runlevel (`initdefault`'s, 2 if
/// none), and finally its `once` entries (the same order as busybox init: every `wait` before every
/// `once`).
pub(crate) fn boot_plan(entries: &[Entry]) -> Vec<(Vec<u8>, bool)> {
    let level = entries
        .iter()
        .find(|e| e.is(b"initdefault"))
        .and_then(|e| e.runlevels.first().copied())
        .unwrap_or(b'2');
    // Each step: whether to wait for an entry that belongs to it
    type Phase<'a> = &'a dyn Fn(&Entry) -> Option<bool>;
    let phases: [Phase; 4] = [
        &|e| e.is(b"sysinit").then_some(true),
        &|e| (e.is(b"boot") || e.is(b"bootwait")).then_some(e.is(b"bootwait")),
        &|e| (e.is(b"wait") && e.in_level(level)).then_some(true),
        &|e| (e.is(b"once") && e.in_level(level)).then_some(false),
    ];
    let mut plan = Vec::new();
    for phase in phases {
        for e in entries.iter().filter(|e| !e.process.is_empty()) {
            if let Some(wait) = phase(e) {
                plan.push((e.process.clone(), wait));
            }
        }
    }
    plan
}

/// The commands run in order at shutdown (each waited for): busybox-format `shutdown` entries,
/// and sysvinit-format `wait` and `once` entries that name runlevel 0 (halt, poweroff) or 6
/// (reboot).
pub(crate) fn shutdown_plan(entries: &[Entry], power: Power) -> Vec<Vec<u8>> {
    let level = power.runlevel();
    entries
        .iter()
        .filter(|e| !e.process.is_empty())
        .filter(|e| {
            e.is(b"shutdown")
                || ((e.is(b"wait") || e.is(b"once"))
                    && !e.runlevels.is_empty()
                    && e.runlevels.contains(&level))
        })
        .map(|e| e.process.clone())
        .collect()
}

/// The command's program (its first word): returned when it is an absolute path, used to skip
/// programs that are not installed (Alpine's inittab has `/sbin/openrc`; without OpenRC
/// installed, no shell need start and fail at every boot).
pub(crate) fn program(process: &[u8]) -> Option<&[u8]> {
    let word = process.split(|b| b.is_ascii_whitespace()).next()?;
    word.starts_with(b"/").then_some(word)
}

/// The script run by `/bin/sh -c`: stdin is `/dev/null`, output goes to [`LOG`] (truncated first
/// when `first`), then the command is `exec`ed. Output is discarded when `/var/log` is missing.
pub(crate) fn script(process: &[u8], first: bool) -> Vec<u8> {
    let redirect = if first { ">" } else { ">>" };
    let mut s = format!(
        "exec </dev/null; if [ -d /var/log ]; then exec {redirect}{LOG} 2>&1; \
         else exec >/dev/null 2>&1; fi; exec "
    )
    .into_bytes();
    s.extend_from_slice(process);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALPINE: &[u8] = b"# /etc/inittab

::sysinit:/sbin/openrc sysinit
::sysinit:/sbin/openrc boot
::wait:/sbin/openrc default

# Set up a couple of getty's
tty1::respawn:/sbin/getty 38400 tty1
tty2::respawn:/sbin/getty 38400 tty2

::ctrlaltdel:/sbin/reboot
::shutdown:/sbin/openrc shutdown
";

    const DEBIAN: &[u8] = b"id:2:initdefault:
si::sysinit:/etc/init.d/rcS
~:S:wait:/sbin/sulogin --force
l0:0:wait:/etc/init.d/rc 0
l1:1:wait:/etc/init.d/rc 1
l2:2:wait:/etc/init.d/rc 2
l6:6:wait:/etc/init.d/rc 6
z6:6:respawn:/sbin/sulogin --force
ca:12345:ctrlaltdel:/sbin/shutdown -t1 -a -r now
1:2345:respawn:/sbin/getty --noclear 38400 tty1
";

    fn plan(text: &[u8]) -> Vec<(String, bool)> {
        boot_plan(&parse(text))
            .into_iter()
            .map(|(p, w)| (String::from_utf8(p).unwrap(), w))
            .collect()
    }

    fn down(text: &[u8], power: Power) -> Vec<String> {
        shutdown_plan(&parse(text), power)
            .into_iter()
            .map(|p| String::from_utf8(p).unwrap())
            .collect()
    }

    #[test]
    fn alpine_runs_openrc() {
        assert_eq!(
            plan(ALPINE),
            [
                ("/sbin/openrc sysinit".to_owned(), true),
                ("/sbin/openrc boot".to_owned(), true),
                ("/sbin/openrc default".to_owned(), true),
            ]
        );
        for power in [Power::Reboot, Power::Halt, Power::PowerOff] {
            assert_eq!(down(ALPINE, power), ["/sbin/openrc shutdown"]);
        }
    }

    #[test]
    fn sysvinit_follows_runlevels() {
        assert_eq!(
            plan(DEBIAN),
            [
                ("/etc/init.d/rcS".to_owned(), true),
                ("/etc/init.d/rc 2".to_owned(), true),
            ]
        );
        assert_eq!(down(DEBIAN, Power::PowerOff), ["/etc/init.d/rc 0"]);
        assert_eq!(down(DEBIAN, Power::Reboot), ["/etc/init.d/rc 6"]);
    }

    #[test]
    fn order_and_parsing() {
        let text = b"a::once:/bin/late
b::wait:/bin/first wait
c:3:wait:/bin/other level
  # comment
d::bootwait:/bin/bw
e::boot:/bin/b
f::sysinit:/bin/s: with a colon
g::wait
h::sysinit:
";
        assert_eq!(
            plan(text),
            [
                ("/bin/s: with a colon".to_owned(), true),
                ("/bin/bw".to_owned(), true),
                ("/bin/b".to_owned(), false),
                ("/bin/first wait".to_owned(), true),
                ("/bin/late".to_owned(), false),
            ]
        );
        assert_eq!(program(b"/sbin/openrc default"), Some(&b"/sbin/openrc"[..]));
        assert_eq!(program(b"echo hi"), None);
    }
}
