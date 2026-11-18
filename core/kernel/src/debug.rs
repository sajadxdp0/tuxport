//! Local debug log (the kernel/signal map, design doc §12.4): written to the host's stderr when
//! the environment variable `TUXPORT_LOG=1` is set. With `TUXPORT_STRACE=1` it also prints every
//! returning syscall (number, arguments and result), like strace; each line starts with the
//! seconds since start at the time of return and ends with the milliseconds the syscall itself
//! took.

use std::fmt;
use std::sync::OnceLock;
use std::time::Instant;

/// When the syscall started; None when tracing is off.
pub(crate) fn trace_start() -> Option<Instant> {
    tracing().then(|| {
        epoch();
        Instant::now()
    })
}

pub(crate) fn tracing() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("TUXPORT_STRACE").is_some_and(|v| !v.is_empty() && v != "0")
    })
}

/// `TUXPORT_STRACE`: prints one syscall and its result.
pub(crate) fn trace_syscall(
    start: Option<Instant>,
    tid: i32,
    nr: u64,
    args: &[u64; 6],
    result: &Result<u64, crate::errno::Errno>,
) {
    if let Some(start) = start {
        let at = epoch().elapsed().as_secs_f64();
        let ms = start.elapsed().as_secs_f64() * 1e3;
        match result {
            Ok(v) => eprintln!("{at:.3} [{tid}] {nr}({:x?}) = {v:#x} <{ms:.1}>", args),
            Err(e) => eprintln!("{at:.3} [{tid}] {nr}({:x?}) = -{} <{ms:.1}>", args, e.0),
        }
    }
}

/// The moment of the first call, the zero point of the trace output.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Records one debug log line (unimplemented paths reached for the first time among them,
/// [`crate::stub`]).
pub(crate) fn log(args: fmt::Arguments<'_>) {
    if enabled() {
        eprintln!("tuxport: {args}");
    }
}

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os("TUXPORT_LOG").is_some_and(|v| !v.is_empty() && v != "0"))
}
