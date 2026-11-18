//! tuxport-run: runs a guest program directly in the host terminal (the development front end,
//! design doc §13.3).
//!
//! Without `--rootfs`, `<program>` is a host path and the guest sees no filesystem at all; with
//! `--rootfs`, `<program>` is a guest path in the inode store, and dynamically linked programs are
//! loaded through the store's `ld-musl`. The guest starts with `argv[0] = <program>`, and the
//! environment holds only the variables given by `--env`. Exit codes follow the shell's
//! convention: the guest's exit code on a normal exit, 128 + the signal number when killed by a
//! signal, 127 when the program is not found and 126 when it cannot start for another reason.
//! tuxport-run's own failures exit with 1 (the inode store cannot be opened, or `--import` failed)
//! and usage errors with 2.
//!
//! When stdin and stdout are both terminals, the guest runs on an in-process PTY (the kernel/tty
//! map): the host terminal switches to raw mode, keys go unchanged to the guest's line discipline,
//! which turns ^C and ^Z into signals to the foreground process group, and the window size follows
//! the host terminal. `--pty` does this even without a terminal (stdin and stdout may be pipes,
//! for tests), and `--no-pty` lets the guest use the host's stdin/stdout/stderr directly.
//!
//! `--init` first boots by `/etc/inittab` (as the app does; OpenRC runs on Alpine), and shuts down
//! by it when the program ends or the guest requests a shutdown (busybox's reboot, halt, poweroff)
//! (the kernel/boot map).
//!
//! SIGINT, SIGTERM and SIGHUP to tuxport-run (`timeout`, ^C with `--no-pty`, a closed terminal) are
//! passed on to the program, and another a second or more later ends every guest process; either way tuxport-run
//! still shuts down, syncs the store and restores the host terminal before it exits.

use std::env;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;

use tuxport_kernel::errno::Errno;
use tuxport_kernel::{DbtStats, ExitStatus, RunError, RunOptions, Terminal, Tier};

/// The legal notice `--version` prints (AGPL section 5: an interactive interface must display the
/// copyright, the absence of warranty and the license).
const NOTICE: &str = "Copyright (C) 2026 Matthew Lee
License AGPLv3+: GNU AGPL version 3 or later <https://gnu.org/licenses/agpl.html>.
This is free software: you are free to change and redistribute it.
There is NO WARRANTY, to the extent permitted by law.";

const USAGE: &str = "usage: tuxport-run [options] [--] <program> [args...]
       tuxport-run --rootfs <dir> --import <rootfs.tar.gz>

  --tier 0             run in the interpreter (default)
  --tier 1             run in the dynamic binary translator (aarch64 Linux hosts only)
  --rootfs <dir>       resolve <program> in the inode store at <dir>
  --import <tar.gz>    create the inode store at <dir> from a rootfs tarball, then exit
  --env NAME=VALUE     add a guest environment variable (repeatable)
  --pty                run on an in-process terminal even if stdin is not a tty
  --no-pty             give the guest the host stdin/stdout/stderr directly
  --memory-limit <n>   kill a guest process when Tuxport's anonymous memory exceeds n
                       bytes (K, M, G suffixes; \"none\" for no limit; default: half the RAM)
  --low-port-offset <n>
                       bind guest ports below 1024 to port + n on the host (then + 2n,
                       + 3n, or any free port) when the host refuses the port itself;
                       0 binds them as they are (default: 10000)
  --always-map-low-ports
                       move guest ports below 1024 even when the host lets Tuxport bind them
  --init               boot by /etc/inittab first (sysinit, wait, once entries; output in
                       /var/log/boot.log) and shut down by it when the program exits or
                       the guest asks to (reboot, halt, poweroff)
  --stats              print Tier 1 statistics to stderr on exit";

fn main() -> ExitCode {
    let mut args: Vec<OsString> = env::args_os().skip(1).collect();
    let mut options = RunOptions::default();
    let mut stats = false;
    let mut import = None;
    let mut pty = None;
    let mut envp: Vec<Vec<u8>> = Vec::new();
    while let Some(arg) = args.first().and_then(|a| a.to_str()).map(str::to_owned) {
        if !arg.starts_with('-') {
            break;
        }
        args.remove(0);
        // An option with a value can be written either as `--opt value` or as `--opt=value`
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_owned(), Some(OsString::from(v))),
            _ => (arg.clone(), None),
        };
        // Options without a value refuse one, as getopt_long does: `--pty=0` must not turn the
        // PTY on
        let flag = matches!(
            name.as_str(),
            "-h" | "--help"
                | "--version"
                | "--stats"
                | "--init"
                | "--always-map-low-ports"
                | "--pty"
                | "--no-pty"
        );
        if flag && inline.is_some() {
            return usage_error();
        }
        let mut value = || {
            inline
                .clone()
                .or_else(|| (!args.is_empty()).then(|| args.remove(0)))
        };
        match name.as_str() {
            "--" => break,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--version" => {
                println!("tuxport-run {}\n{NOTICE}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--stats" => stats = true,
            "--init" => options.init = true,
            "--always-map-low-ports" => options.always_map_low_ports = true,
            "--pty" => pty = Some(true),
            "--no-pty" => pty = Some(false),
            "--tier" => {
                options.tier = match value().as_ref().and_then(|v| v.to_str()) {
                    Some("0") => Tier::Interpreter,
                    Some("1") => Tier::Dbt,
                    _ => return usage_error(),
                };
            }
            "--rootfs" => match value() {
                Some(v) => options.rootfs = Some(PathBuf::from(v)),
                None => return usage_error(),
            },
            "--import" => match value() {
                Some(v) => import = Some(PathBuf::from(v)),
                None => return usage_error(),
            },
            "--env" => match value() {
                Some(v) if v.as_bytes().contains(&b'=') => envp.push(v.as_bytes().to_vec()),
                _ => return usage_error(),
            },
            "--memory-limit" => match value().as_ref().and_then(|v| v.to_str()).map(parse_size) {
                Some(Some(n)) => options.memory_limit = Some(n),
                _ => return usage_error(),
            },
            "--low-port-offset" => match value().as_ref().and_then(|v| v.to_str()?.parse().ok()) {
                Some(n) => options.low_port_offset = Some(n),
                None => return usage_error(),
            },
            _ => return usage_error(),
        }
    }

    if let Some(tarball) = import {
        let Some(store) = &options.rootfs else {
            return usage_error();
        };
        let imported = tuxport_kernel::import_rootfs(store, &tarball)
            .and_then(|r| tuxport_kernel::sync_store(store).map(|()| r));
        return match imported {
            Ok(r) => {
                eprintln!(
                    "tuxport-run: imported {} files, {} directories, {} symlinks, {} hard links, {} device nodes",
                    r.files, r.dirs, r.symlinks, r.hardlinks, r.nodes
                );
                ExitCode::SUCCESS
            }
            Err(err) => {
                let reason = if err == Errno::EEXIST {
                    "the inode store already exists".to_owned()
                } else {
                    err.to_string()
                };
                eprintln!("tuxport-run: cannot import {}: {reason}", tarball.display());
                ExitCode::from(1)
            }
        };
    }
    let Some(program) = args.first().cloned() else {
        return usage_error();
    };

    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.as_bytes().to_vec()).collect();
    let on_tty = tuxport_hal::isatty(0) && tuxport_hal::isatty(1);
    let mut raw = None;
    if pty.unwrap_or(on_tty) {
        if on_tty {
            match tuxport_hal::RawMode::enter(0) {
                Ok(r) => raw = Some(r),
                Err(e) => {
                    eprintln!("tuxport-run: cannot switch the terminal to raw mode: errno {e}")
                }
            }
        }
        options.terminal = Some(Terminal {
            input: 0,
            output: 1,
            size: on_tty.then_some(1),
            resize: on_tty
                .then(tuxport_hal::sigwinch_notifier)
                .and_then(Result::ok),
        });
    }
    // Without the pipe the signals keep their default action
    options.stop = tuxport_hal::stop_notifier().ok();
    let result = tuxport_kernel::run(&program, &argv, &envp, &options);
    // Restore the host terminal first, then print the message
    drop(raw);
    let report = match result {
        Ok(report) => report,
        Err(err) => {
            eprintln!("tuxport-run: {}: {err}", program.to_string_lossy());
            return match err {
                RunError::Io(io) if io.kind() == ErrorKind::NotFound => ExitCode::from(127),
                RunError::Exec(errno) if errno == Errno::ENOENT => ExitCode::from(127),
                RunError::Rootfs(_) => ExitCode::from(1),
                _ => ExitCode::from(126),
            };
        }
    };
    if let (true, Some(s)) = (stats, report.dbt_stats) {
        print_stats(&s);
    }
    match report.status {
        ExitStatus::Exited(code) => ExitCode::from(code as u8),
        ExitStatus::Signaled {
            signal,
            pc,
            fault_addr,
            ..
        } => {
            match (pc, fault_addr) {
                (Some(pc), Some(addr)) => {
                    eprintln!("tuxport-run: guest killed by {signal} (pc={pc:#x}, addr={addr:#x})")
                }
                (Some(pc), None) => eprintln!("tuxport-run: guest killed by {signal} (pc={pc:#x})"),
                _ => eprintln!("tuxport-run: guest killed by {signal}"),
            }
            ExitCode::from((128 + signal.number()) as u8)
        }
    }
}

/// The value of `--memory-limit`: a byte count, optionally with a K, M or G suffix; `none` means no
/// limit.
fn parse_size(s: &str) -> Option<u64> {
    if s == "none" {
        return Some(u64::MAX);
    }
    let (digits, shift) = match s.as_bytes().last()? {
        b'K' | b'k' => (&s[..s.len() - 1], 10),
        b'M' | b'm' => (&s[..s.len() - 1], 20),
        b'G' | b'g' => (&s[..s.len() - 1], 30),
        _ => (s, 0),
    };
    digits.parse::<u64>().ok()?.checked_mul(1 << shift)
}

fn usage_error() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn print_stats(s: &DbtStats) {
    let pct = |n: u64| 100.0 * n as f64 / s.insns.max(1) as f64;
    eprintln!("tuxport-run: tier 1 statistics");
    eprintln!("  blocks translated   {:>12}", s.blocks);
    eprintln!("    from templates    {:>12}", s.blocks_shared);
    eprintln!("  guest instructions  {:>12}", s.insns);
    eprintln!(
        "    copied            {:>12}  ({:.1}%)",
        s.insns_copied,
        pct(s.insns_copied)
    );
    eprintln!(
        "    remapped          {:>12}  ({:.1}%)",
        s.insns_remapped,
        pct(s.insns_remapped)
    );
    eprintln!(
        "    memory (inline)   {:>12}  ({:.1}%)",
        s.insns_memory,
        pct(s.insns_memory)
    );
    eprintln!(
        "      sharing a check {:>12}  ({:.1}%)",
        s.insns_grouped,
        pct(s.insns_grouped)
    );
    eprintln!(
        "    tier 0 fallback   {:>12}  ({:.1}%)",
        s.insns_fallback,
        pct(s.insns_fallback)
    );
    eprintln!(
        "  exits: chain {}, lookup {}, interp {}, svc {}, kick {}",
        s.exits_chain, s.exits_lookup, s.exits_interp, s.exits_svc, s.exits_kick
    );
    eprintln!("  code cache flushes  {:>12}", s.flushes);
    eprintln!("  blocks invalidated  {:>12}", s.invalidated);
    eprintln!("  groups undone       {:>12}", s.ungrouped);
}
