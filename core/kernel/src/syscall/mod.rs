//! Syscall dispatch (the kernel/process map; restarts and unimplemented syscalls: the kernel/signal
//! map).
//!
//! Numbers and argument layouts follow aarch64 Linux (asm-generic). Unimplemented syscalls always
//! return `-ENOSYS`, never crash, and record a local debug log line. Handlers run holding the BKL;
//! those that need to wait release it temporarily through [`Cx::wait`].

mod aio;
mod bpf;
mod cred;
pub(crate) mod fast;
mod fs;
mod fuse;
pub(crate) use fuse::{fill, flush_at_exit};
mod inotify;
mod ipc;
mod lease;
mod loopdev;
mod mem;
mod misc;
mod mount;
mod mqueue;
mod net;
mod ns;
mod poll;
mod posix_timer;
mod proc;
mod ptrace;
mod sched;
mod sig;
mod timerfd;
mod tty;
mod unix;
mod xattr;

pub(crate) use aio::{AioContext, check_polls as check_aio_polls, released as aio_released};
pub(crate) use fs::ENTROPY;
pub(crate) use fs::{close_replaced, create_exclusive, unlink_path, write_at};
pub(crate) use inotify::{Inotify, Opened, Registry, host_change, kernfs_modified};
pub(crate) use lease::Lease;
pub(crate) use loopdev::{LoopOpen, Loops};
pub(crate) use misc::Restart;
pub(crate) use mount::collect_unused;
pub(crate) use mqueue::{
    Limits as MqLimits, MessageQueue, MqDir, closed as mqueue_closed, find as mqueue_find,
};
pub(crate) use net::{SocketRef, deliver_dns, unmap_addr};
pub(crate) use poll::{Epoll, ExGate, Fasync, check_exclusive, check_fasync};
pub(crate) use proc::{interp_label, label_image};
pub(crate) use ptrace::encode_regset;
pub(crate) use sched::host_started;
pub(crate) use tty::{hang_up, hang_up_closed};

use std::rc::Rc;
use std::sync::atomic::Ordering;

use tuxport_engine::Cpu;

use crate::debug;
use crate::errno::Errno;
use crate::kernel::{Cx, Tid};
use crate::process::ExitStatus;
use crate::signal;
use crate::uaccess::UserSlice;

/// How the run loop should continue after handling one entry into the kernel.
pub(crate) enum Flow {
    /// Continue running the guest.
    Continue,
    /// execve replaced the address space and the program: flush the translation cache, then
    /// continue.
    Exec,
    /// The thread exits.
    Exit,
}

/// A syscall's return value: on success, the value written back to x0.
type SysResult = Result<u64, Errno>;

/// The most bytes copied between guest and host at once, so as not to allocate memory in one go
/// by a length the guest gives.
const IO_CHUNK: usize = 64 * 1024;

/// The most one read or write of `/dev/fuse` or of a FUSE file moves at once: a FUSE message must
/// be read and written whole (up to 256 pages of data and its header), and a FUSE file's reads and
/// writes are split into requests by the negotiated size anyway.
const FUSE_CHUNK: usize = 8 << 20;

/// The most sendfile and copy_file_range copy at once, the same as Linux's `MAX_RW_COUNT`.
const MAX_COPY: usize = 0x7fff_f000;

/// Maximum length of a path argument (including the NUL), the same as Linux's `PATH_MAX`.
const PATH_MAX: usize = 4096;

/// The number of restart_syscall: after a signal without a handler interrupts an
/// `ERESTART_RESTARTBLOCK` syscall, the re-executed svc is changed to it
/// ([`crate::signal::deliver`]).
pub(crate) const NR_RESTART_SYSCALL: u64 = 128;

/// The most times one syscall is run again for FUSE replies its walks needed (a path has at
/// most 2048 components and 40 links, each needing a lookup, an attribute fetch and a readlink);
/// past it the syscall fails with EIO rather than looping on a daemon whose replies never stay
/// valid.
const FUSE_RETRIES: u32 = 16384;

/// Number -1: when the tracer changes the number to it at entry, this system call is skipped
/// (Linux's `NO_SYSCALL`).
const NO_SYSCALL: u64 = u64::MAX;

/// The number a process invisible in the pid namespace is translated to: lookups everywhere fail
/// to find it and report ESRCH, ECHILD etc. by their own rules (it is larger than `PID_MAX`).
pub(crate) const NO_PID: Tid = 0x3fff_ffff;

/// Handles one `svc #0` by the aarch64 Linux convention: x8 is the number, x0–x5 the arguments,
/// and the return value is written back to x0.
pub(crate) fn dispatch(cx: &mut Cx<'_>, cpu: &mut Cpu) -> Flow {
    let state = cpu.state_mut();
    state.orig_x0 = state.x[0];
    let mut nr = state.x[8];
    cx.grow_stack_to(state.sp);
    // PTRACE_SYSCALL: stop at entry, where the tracer may change the number (NT_ARM_SYSTEM_CALL)
    // and the arguments. When the number is changed to -1 this system call is skipped, and x0 is
    // the return value the tracer set; when the program itself uses -1 it is ENOSYS, as on Linux
    // (crate::ptrace)
    let watched = crate::ptrace::syscall_start(cx, cpu, nr);
    let traced = watched == Some(true);
    let filtered = cx.task.seccomp_on.load(Ordering::Relaxed);
    if (traced || filtered) && nr == NO_SYSCALL {
        cpu.state_mut().x[0] = Errno::ENOSYS.to_syscall_return();
    }
    if traced {
        match crate::ptrace::syscall_entry(cx, cpu) {
            Some(n) => nr = n,
            None => return Flow::Exit,
        }
    }
    // seccomp: checked after the entry stop, as on arm64 Linux, including calls the tracer wants
    // skipped (-1). For calls not executed, x0 is already the return value (crate::seccomp)
    if filtered {
        match crate::seccomp::check(cx, cpu, nr) {
            crate::seccomp::Verdict::Run(n) => nr = n,
            crate::seccomp::Verdict::Skip => nr = NO_SYSCALL,
            crate::seccomp::Verdict::Exit => return Flow::Exit,
        }
    }
    // Whether to stop at exit depends, as on arm64 Linux, on the tracing state at exit
    // (syscall_trace_exit rereads the flags): when, at a stop in the middle of the system call
    // (PTRACE_EVENT_SECCOMP, the PTRACE_EVENT_* of fork and clone), the tracer switches to
    // continuing with PTRACE_SYSCALL, it stops at exit too, even if it never stopped at entry (this
    // is how strace --seccomp-bpf gets return values). The vDSO's clock-reading stubs do not enter
    // the kernel on Linux, so with seccomp there is no exit stop for them either
    let exit_stop = watched.is_some() || (filtered && !crate::vdso::clock_stub(cx, cpu, nr));
    if (traced || filtered) && nr == NO_SYSCALL {
        if !crate::ptrace::syscall_exit(cx, cpu) {
            return Flow::Exit;
        }
        return signal::deliver(cx, cpu, true);
    }
    let mut a: [u64; 6] = cpu.state().x[..6].try_into().unwrap();
    // In a new pid namespace: pids in the arguments are translated to global numbers, and pids
    // returned are translated back
    let pid_ns = {
        let g = cx.t.group.borrow();
        (g.pid_ns.level > 0).then(|| g.pid_ns.clone())
    };
    if let Some(ns) = &pid_ns {
        pid_args(ns, nr, &mut a);
    }
    let started = debug::trace_start();
    use xattr::Target::{Fd, Path};

    // A walk that needs a FUSE daemon's reply asks for it and runs the syscall again from the
    // start (crate::vfs::fuse); every retry finds one more thing in the cache. So does a copy
    // finding a page of a FUSE file's mapping not yet read (crate::mm::Fill), though the buffers
    // of reads and writes are read in first
    crate::vfs::fuse::new_syscall();
    fuse::prefault(cx, nr, &a);
    crate::mm::take_wanted();
    // Until the end of the syscall, other threads see that this one may hold overlay nodes it
    // looked up (mount::sweep_overlays)
    let in_syscall = InSyscall::enter(cx.task, nr);
    let mut retries = 0;
    let result = loop {
        let result = match nr {
            // These do not return, or set all registers themselves. When traced they may stop to
            // report before exiting (PTRACE_EVENT_EXIT), and after execve the exec and the system
            // call exit must be reported
            93 => {
                let flow = proc::exit(cx, a[0] as i32);
                crate::ptrace::exit(cx, cpu, ExitStatus::Exited(a[0] as i32 & 0xff));
                return flow;
            }
            94 => {
                let flow = proc::exit_group(cx, a[0] as i32);
                crate::ptrace::exit(cx, cpu, ExitStatus::Exited(a[0] as i32 & 0xff));
                return flow;
            }
            139 => return signal::sigreturn(cx, cpu),
            #[cfg(any(debug_assertions, feature = "test-hooks"))]
            crate::bug::TEST_SYSCALL => crate::bug::test_hook(cx, cpu, a[0], a[1]),
            221 => {
                let tid = cx.tid();
                match proc::execve(cx, cpu, a[0], a[1], a[2]) {
                    Ok(()) => return exec_done(cx, cpu, tid),
                    Err(e) => Err(e),
                }
            }
            281 => {
                let tid = cx.tid();
                match proc::execveat(cx, cpu, a[0] as i32, a[1], (a[2], a[3]), a[4] as u32) {
                    Ok(()) => return exec_done(cx, cpu, tid),
                    Err(e) => Err(e),
                }
            }
            117 => ptrace::ptrace(cx, a[0] as i64, a[1] as i32, a[2], a[3]),
            277 => crate::seccomp::seccomp(cx, a[0] as u32, a[1] as u32, a[2]),
            270 | 271 => {
                ptrace::process_vm(cx, a[0] as i32, (a[1], a[2]), (a[3], a[4]), a[5], nr == 271)
            }

            5 | 6 => xattr::setxattr(
                cx,
                Path(a[0], nr == 5),
                a[1],
                a[2],
                a[3] as usize,
                a[4] as u32,
            ),
            7 => xattr::setxattr(cx, Fd(a[0] as i32), a[1], a[2], a[3] as usize, a[4] as u32),
            8 | 9 => xattr::getxattr(cx, Path(a[0], nr == 8), a[1], a[2], a[3] as usize),
            0 => aio::io_setup(cx, a[0] as u32, a[1]),
            1 => aio::io_destroy(cx, a[0]),
            2 => aio::io_submit(cx, a[0], a[1] as i64, a[2]),
            3 => aio::io_cancel(cx, a[0], a[1], a[2]),
            4 => aio::io_getevents(cx, a[0], a[1] as i64, a[2] as i64, a[3], a[4], None),
            292 => match aio::sigmask_arg(cx, a[5]) {
                Ok(mask) => aio::io_getevents(cx, a[0], a[1] as i64, a[2] as i64, a[3], a[4], mask),
                Err(e) => Err(e),
            },
            10 => xattr::getxattr(cx, Fd(a[0] as i32), a[1], a[2], a[3] as usize),
            11 | 12 => xattr::listxattr(cx, Path(a[0], nr == 11), a[1], a[2] as usize),
            13 => xattr::listxattr(cx, Fd(a[0] as i32), a[1], a[2] as usize),
            14 | 15 => xattr::removexattr(cx, Path(a[0], nr == 14), a[1]),
            16 => xattr::removexattr(cx, Fd(a[0] as i32), a[1]),
            17 => fs::getcwd(cx, a[0], a[1]),
            19 => poll::eventfd2(cx, a[0] as u32, a[1] as u32),
            20 => poll::epoll_create1(cx, a[0] as u32),
            21 => poll::epoll_ctl(cx, a[0] as i32, a[1] as u32, a[2] as i32, a[3]),
            22 => {
                let deadline = poll::deadline_ms(a[3] as i32);
                poll::epoll_pwait(
                    cx,
                    a[0] as i32,
                    a[1],
                    a[2] as i32,
                    deadline,
                    a[4],
                    a[5] as usize,
                )
            }
            23 => fs::dup(cx, a[0] as i32),
            24 => fs::dup3(cx, a[0] as i32, a[1] as i32, a[2] as u32),
            25 => fs::fcntl(cx, a[0] as i32, a[1] as u32, a[2]),
            26 => inotify::inotify_init1(cx, a[0] as u32),
            27 => inotify::inotify_add_watch(cx, a[0] as i32, a[1], a[2] as u32),
            28 => inotify::inotify_rm_watch(cx, a[0] as i32, a[1] as i32),
            29 => fs::ioctl(cx, a[0] as i32, a[1] as u32, a[2]),
            30 => sched::ioprio_set(cx, a[0] as i32, a[1] as i32, a[2] as i32),
            31 => sched::ioprio_get(cx, a[0] as i32, a[1] as i32),
            32 => fs::flock(cx, a[0] as i32, a[1] as u32),
            33 => fs::mknodat(cx, a[0] as i32, a[1], a[2] as u32, a[3]),
            34 => fs::mkdirat(cx, a[0] as i32, a[1], a[2] as u32),
            35 => fs::unlinkat(cx, a[0] as i32, a[1], a[2] as u32),
            36 => fs::symlinkat(cx, a[0], a[1] as i32, a[2]),
            37 => fs::linkat(cx, a[0] as i32, a[1], a[2] as i32, a[3], a[4] as u32),
            38 => fs::renameat2(cx, a[0] as i32, a[1], a[2] as i32, a[3], 0),
            39 => mount::umount2(cx, a[0], a[1] as u32),
            40 => mount::mount(cx, a[0], a[1], a[2], a[3], a[4]),
            41 => mount::pivot_root(cx, a[0], a[1]),
            268 => ns::setns(cx, a[0] as i32, a[1] as i32),
            43 => fs::statfs(cx, a[0], a[1]),
            44 => fs::fstatfs(cx, a[0] as i32, a[1]),
            45 => fs::truncate(cx, a[0], a[1] as i64),
            46 => fs::ftruncate(cx, a[0] as i32, a[1] as i64),
            47 => fs::fallocate(cx, a[0] as i32, a[1] as u32, a[2] as i64, a[3] as i64),
            48 => fs::faccessat(cx, a[0] as i32, a[1], a[2] as u32, 0),
            49 => fs::chdir(cx, a[0]),
            50 => fs::fchdir(cx, a[0] as i32),
            51 => fs::chroot(cx, a[0]),
            52 => fs::fchmod(cx, a[0] as i32, a[1] as u32),
            53 => fs::fchmodat(cx, a[0] as i32, a[1], a[2] as u32),
            54 => fs::fchownat(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32, a[4] as u32),
            55 => fs::fchown(cx, a[0] as i32, a[1] as u32, a[2] as u32),
            56 => fs::openat(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32),
            437 => fs::openat2(cx, a[0] as i32, a[1], a[2], a[3] as usize),
            57 => fs::close(cx, a[0] as i32),
            58 => tty::vhangup_ctty(cx),
            59 => fs::pipe2(cx, a[0], a[1] as u32),
            61 => fs::getdents64(cx, a[0] as i32, a[1], a[2] as usize),
            62 => fs::lseek(cx, a[0] as i32, a[1] as i64, a[2] as u32),
            63 => fs::read(cx, a[0] as i32, a[1], a[2] as usize),
            64 => fs::write(cx, a[0] as i32, a[1], a[2] as usize),
            65 => fs::readv(cx, a[0] as i32, a[1], a[2] as usize),
            66 => fs::writev(cx, a[0] as i32, a[1], a[2] as usize),
            67 => fs::pread(cx, a[0] as i32, a[1], a[2] as usize, a[3] as i64),
            68 => fs::pwrite(cx, a[0] as i32, a[1], a[2] as usize, a[3] as i64),
            // On 64-bit, pos_h does not take part in the offset (Linux's pos_from_hilo)
            69 | 70 => fs::pvectored(
                cx,
                a[0] as i32,
                a[1],
                a[2] as usize,
                a[3] as i64,
                None,
                nr == 70,
            ),
            71 => fs::sendfile(cx, a[0] as i32, a[1] as i32, a[2], a[3] as usize),
            72 => poll::pselect6(cx, a[0] as i32, [a[1], a[2], a[3]], a[4], a[5]),
            73 => poll::ppoll(cx, a[0], a[1] as usize, a[2], a[3], a[4] as usize),
            74 => sig::signalfd4(cx, a[0] as i32, a[1], a[2] as usize, a[3] as u32),
            75 => fs::vmsplice(cx, a[0] as i32, a[1], a[2] as usize, a[3] as u32),
            76 => fs::splice(
                cx,
                a[0] as i32,
                a[1],
                a[2] as i32,
                a[3],
                a[4] as usize,
                a[5] as u32,
            ),
            77 => fs::tee(cx, a[0] as i32, a[1] as i32, a[2] as usize, a[3] as u32),
            78 => fs::readlinkat(cx, a[0] as i32, a[1], a[2], a[3] as usize),
            79 => fs::newfstatat(cx, a[0] as i32, a[1], a[2], a[3] as u32),
            80 => fs::fstat(cx, a[0] as i32, a[1]),
            81 => fs::sync(cx),
            82 => fs::fsync(cx, a[0] as i32, false),
            83 => fs::fsync(cx, a[0] as i32, true),
            84 => fs::cache_hint(cx, a[0] as i32, a[1] as i64, a[2] as i64, a[3] as u32, 0),
            85 => timerfd::timerfd_create(cx, a[0] as i32, a[1] as u32),
            86 => timerfd::timerfd_settime(cx, a[0] as i32, a[1] as u32, a[2], a[3]),
            87 => timerfd::timerfd_gettime(cx, a[0] as i32, a[1]),
            88 => fs::utimensat(cx, a[0] as i32, a[1], a[2], a[3] as u32),
            90 => cred::capget(cx, a[0], a[1]),
            91 => cred::capset(cx, a[0], a[1]),
            95 => proc::waitid(cx, a[0] as u32, a[1] as i32, a[2], a[3] as u32, a[4]),
            96 => proc::set_tid_address(cx, a[0]),
            98 => misc::futex(cx, a[0], a[1] as u32, a[2] as u32, a[3], a[4], a[5] as u32),
            92 => misc::personality(cx, a[0] as u32),
            99 => misc::set_robust_list(cx, a[0], a[1]),
            100 => misc::get_robust_list(cx, a[0] as i32, a[1], a[2]),
            101 => misc::nanosleep(cx, a[0], a[1]),
            102 => misc::getitimer(cx, a[0] as i32, a[1]),
            103 => misc::setitimer(cx, a[0] as i32, a[1], a[2]),
            107 => posix_timer::timer_create(cx, a[0] as i32, a[1], a[2]),
            108 => posix_timer::timer_gettime(cx, a[0] as i32, a[1]),
            109 => posix_timer::timer_getoverrun(cx, a[0] as i32),
            110 => posix_timer::timer_settime(cx, a[0] as i32, a[1] as u32, a[2], a[3]),
            111 => posix_timer::timer_delete(cx, a[0] as i32),
            112 => misc::clock_settime(cx, a[0] as i32, a[1]),
            113 => misc::clock_gettime(cx, a[0] as i32, a[1]),
            114 => misc::clock_getres(cx, a[0] as i32, a[1]),
            115 => misc::clock_nanosleep(cx, a[0] as i32, a[1] as u32, a[2], a[3]),
            116 => misc::syslog(cx, a[0] as i32, a[1], a[2] as i32),
            118 => sched::sched_setparam(cx, a[0] as i32, a[1]),
            119 => sched::sched_setscheduler(cx, a[0] as i32, a[1] as i32, a[2]),
            120 => sched::sched_getscheduler(cx, a[0] as i32),
            121 => sched::sched_getparam(cx, a[0] as i32, a[1]),
            122 => sched::sched_setaffinity(cx, a[0] as i32, a[1] as usize, a[2]),
            123 => sched::sched_getaffinity(cx, a[0] as i32, a[1] as usize, a[2]),
            124 => sched::sched_yield(cx),
            125 | 126 => sched::sched_priority_range(a[0] as i32, nr == 125),
            127 => sched::sched_rr_get_interval(cx, a[0] as i32, a[1]),
            NR_RESTART_SYSCALL => misc::restart_syscall(cx),
            129 => sig::kill(cx, a[0] as i32, a[1] as i32),
            130 => sig::tkill(cx, a[0] as i32, a[1] as i32),
            131 => sig::tgkill(cx, a[0] as i32, a[1] as i32, a[2] as i32),
            132 => sig::sigaltstack(cx, cpu.state().sp, a[0], a[1]),
            133 => sig::rt_sigsuspend(cx, a[0], a[1] as usize),
            134 => sig::rt_sigaction(cx, a[0] as i32, a[1], a[2], a[3] as usize),
            135 => sig::rt_sigprocmask(cx, a[0] as u32, a[1], a[2], a[3] as usize),
            136 => sig::rt_sigpending(cx, a[0], a[1] as usize),
            137 => sig::rt_sigtimedwait(cx, a[0], a[1], a[2], a[3] as usize),
            138 => sig::sigqueueinfo(cx, a[0] as i32, None, a[1] as i32, a[2]),
            140 => sched::setpriority(cx, a[0] as i32, a[1] as i32, a[2] as i32),
            141 => sched::getpriority(cx, a[0] as i32, a[1] as i32),
            142 => misc::reboot(cx, a[0] as u32, a[1] as u32, a[2] as u32),
            143 => cred::setregid(cx, a[0], a[1]),
            144 => cred::setgid(cx, a[0]),
            145 => cred::setreuid(cx, a[0], a[1]),
            146 => cred::setuid(cx, a[0]),
            147 => cred::setresuid(cx, a[0], a[1], a[2]),
            148 => cred::getres(cx, false, [a[0], a[1], a[2]]),
            149 => cred::setresgid(cx, a[0], a[1], a[2]),
            150 => cred::getres(cx, true, [a[0], a[1], a[2]]),
            151 => cred::setfsuid(cx, a[0]),
            152 => cred::setfsgid(cx, a[0]),
            153 => misc::times(cx, a[0]),
            154 => proc::setpgid(cx, a[0] as i32, a[1] as i32),
            155 => proc::getpgid(cx, a[0] as i32),
            156 => proc::getsid(cx, a[0] as i32),
            157 => proc::setsid(cx),
            158 => cred::getgroups(cx, a[0] as i32, a[1]),
            159 => cred::setgroups(cx, a[0] as i32, a[1]),
            160 => misc::uname(cx, a[0]),
            161 => misc::set_uts_name(cx, a[0], a[1] as i64, false),
            162 => misc::set_uts_name(cx, a[0], a[1] as i64, true),
            163 => misc::prlimit(cx, 0, a[0] as u32, 0, a[1]),
            164 => misc::prlimit(cx, 0, a[0] as u32, a[1], 0),
            165 => misc::getrusage(cx, a[0] as i32, a[1]),
            166 => misc::umask(cx, a[0] as u32),
            167 => misc::prctl(cx, a[0] as i32, [a[1], a[2], a[3], a[4]]),
            169 => misc::gettimeofday(cx, a[0], a[1]),
            170 => Err(Errno::EPERM), // settimeofday: the guest cannot change the host's time
            171 => misc::adjtimex(cx, None, a[0]),
            172 => Ok(cx.tgid() as u64),                // getpid
            173 => Ok(cx.t.group.borrow().ppid as u64), // getppid
            174 | 175 => cred::getuid(cx, nr == 175),
            176 | 177 => cred::getgid(cx, nr == 177),
            168 => misc::getcpu(cx, a[0], a[1]),
            178 => Ok(cx.tid() as u64), // gettid
            179 => misc::sysinfo(cx, a[0]),
            186 => ipc::msgget(cx, a[0] as i32, a[1] as i32),
            187 => ipc::msgctl(cx, a[0] as i32, a[1] as i32, a[2]),
            188 => ipc::msgrcv(cx, a[0] as i32, a[1], a[2], a[3] as i64, a[4] as i32),
            189 => ipc::msgsnd(cx, a[0] as i32, a[1], a[2], a[3] as i32),
            190 => ipc::semget(cx, a[0] as i32, a[1] as i32, a[2] as i32),
            191 => ipc::semctl(cx, a[0] as i32, a[1] as i32, a[2] as i32, a[3]),
            192 => ipc::semtimedop(cx, a[0] as i32, a[1], a[2], a[3]),
            193 => ipc::semtimedop(cx, a[0] as i32, a[1], a[2], 0),
            194 => ipc::shmget(cx, a[0] as i32, a[1], a[2] as i32),
            195 => ipc::shmctl(cx, a[0] as i32, a[1] as i32, a[2]),
            196 => ipc::shmat(cx, a[0] as i32, a[1], a[2] as i32),
            97 => proc::unshare(cx, a[0]),
            180 => mqueue::mq_open(cx, a[0], a[1] as u32, a[2] as u32, a[3]),
            181 => mqueue::mq_unlink(cx, a[0]),
            182 => mqueue::mq_timedsend(cx, a[0] as i32, a[1], a[2] as usize, a[3] as u32, a[4]),
            183 => mqueue::mq_timedreceive(cx, a[0] as i32, a[1], a[2] as usize, a[3], a[4]),
            184 => mqueue::mq_notify(cx, a[0] as i32, a[1]),
            185 => mqueue::mq_getsetattr(cx, a[0] as i32, a[1], a[2]),
            197 => ipc::shmdt(cx, a[0]),
            198 => net::socket(cx, a[0] as i32, a[1] as u32, a[2] as i32),
            199 => net::socketpair(cx, a[0] as i32, a[1] as u32, a[2] as i32, a[3]),
            200 => net::bind(cx, a[0] as i32, a[1], a[2] as u32),
            201 => net::listen(cx, a[0] as i32, a[1] as i32),
            202 => net::accept4(cx, a[0] as i32, a[1], a[2], 0),
            203 => net::connect(cx, a[0] as i32, a[1], a[2] as u32),
            204 => net::getsockname(cx, a[0] as i32, a[1], a[2]),
            205 => net::getpeername(cx, a[0] as i32, a[1], a[2]),
            206 => net::sendto(
                cx,
                a[0] as i32,
                a[1],
                a[2] as usize,
                a[3] as u32,
                a[4],
                a[5] as u32,
            ),
            207 => net::recvfrom(
                cx,
                a[0] as i32,
                a[1],
                a[2] as usize,
                a[3] as u32,
                a[4],
                a[5],
            ),
            208 => net::setsockopt(cx, a[0] as i32, a[1] as i32, a[2] as i32, a[3], a[4] as u32),
            209 => net::getsockopt(cx, a[0] as i32, a[1] as i32, a[2] as i32, a[3], a[4]),
            210 => net::shutdown(cx, a[0] as i32, a[1] as i32),
            211 => net::sendmsg(cx, a[0] as i32, a[1], a[2] as u32),
            212 => net::recvmsg(cx, a[0] as i32, a[1], a[2] as u32),
            214 => {
                let exec = mem::read_implies_exec(cx);
                Ok(cx.mm_mut().brk(a[0], exec))
            }
            223 => fs::fadvise64(cx, a[0] as i32, a[2] as i64, a[3] as i32),
            213 => fs::cache_hint(cx, a[0] as i32, a[1] as i64, a[2] as i64, 0, 1),
            215 => mem::munmap(cx, a[0], a[1]),
            216 => mem::mremap(cx, a[0], a[1], a[2], a[3] as u32, a[4]),
            220 => proc::clone(cx, cpu, a[0], a[1], a[2], a[3], a[4]),
            222 => mem::mmap(cx, a[0], a[1], a[2] as u32, a[3] as u32, a[4] as i32, a[5]),
            226 => mem::mprotect(cx, a[0], a[1], a[2] as u32),
            227 => mem::msync(cx, a[0], a[1], a[2] as u32),
            228 => mem::mlock(cx, a[0], a[1], 0, true),
            229 => mem::mlock(cx, a[0], a[1], 0, false),
            230 => mem::mlockall(cx, a[0] as u32),
            231 => mem::munlockall(cx),
            232 => mem::mincore(cx, a[0], a[1], a[2]),
            233 => mem::madvise(cx, a[0], a[1], a[2] as i32),
            234 => mem::remap_file_pages(cx, a[0], a[1], a[2], a[3], a[4] as u32),
            240 => sig::sigqueueinfo(cx, a[0] as i32, Some(a[1] as i32), a[2] as i32, a[3]),
            242 => net::accept4(cx, a[0] as i32, a[1], a[2], a[3] as u32),
            243 => net::recvmmsg(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32, a[4]),
            260 => proc::wait4(cx, a[0] as i32, a[1], a[2] as u32, a[3]),
            261 => misc::prlimit(cx, a[0] as i32, a[1] as u32, a[2], a[3]),
            266 => misc::adjtimex(cx, Some(a[0] as i32), a[1]),
            267 => fs::cache_hint(cx, a[0] as i32, 0, 0, 0, 2).and_then(|_| fs::sync(cx)),
            264 => fs::name_to_handle_at(cx, a[0] as i32, a[1], a[2], a[3], a[4] as u32),
            265 => fs::open_by_handle_at(cx, a[0] as i32, a[1], a[2] as u32),
            269 => net::sendmmsg(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32),
            272 => proc::kcmp(cx, a[0] as i32, a[1] as i32, a[2] as i32, a[3], a[4]),
            274 => sched::sched_setattr(cx, a[0] as i32, a[1], a[2] as u32),
            275 => sched::sched_getattr(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32),
            276 => fs::renameat2(cx, a[0] as i32, a[1], a[2] as i32, a[3], a[4] as u32),
            278 => misc::getrandom(cx, a[0], a[1] as usize, a[2] as u32),
            283 => misc::membarrier(cx, a[0] as i32, a[1] as u32),
            279 => fs::memfd_create(cx, a[0], a[1] as u32),
            280 => bpf::bpf(cx, a[0] as i32, a[1], a[2] as u32),
            428 => mount::open_tree(cx, a[0] as i32, a[1], a[2] as u32),
            429 => mount::sys_move_mount(cx, (a[0] as i32, a[1]), (a[2] as i32, a[3]), a[4] as u32),
            442 => mount::mount_setattr(cx, (a[0] as i32, a[1]), a[2] as u32, a[3], a[4] as usize),
            284 => mem::mlock(cx, a[0], a[1], a[2] as u32, true),
            285 => fs::copy_file_range(
                cx,
                a[0] as i32,
                a[1],
                a[2] as i32,
                a[3],
                a[4] as usize,
                a[5] as u32,
            ),
            286 | 287 => fs::pvectored(
                cx,
                a[0] as i32,
                a[1],
                a[2] as usize,
                a[3] as i64,
                Some(a[5] as u32),
                nr == 287,
            ),
            291 => fs::statx(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32, a[4]),
            424 => sig::pidfd_send_signal(cx, a[0] as i32, a[1] as i32, a[2], a[3] as u32),
            434 => proc::pidfd_open(cx, a[0] as i32, a[1] as u32),
            435 => proc::clone3(cx, cpu, a[0], a[1] as usize),
            436 => fs::close_range(cx, a[0] as u32, a[1] as u32, a[2] as u32),
            438 => proc::pidfd_getfd(cx, a[0] as i32, a[1] as i32, a[2] as u32),
            439 => fs::faccessat(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32),
            441 => match poll::deadline_ts(cx, a[3]) {
                Ok(deadline) => poll::epoll_pwait(
                    cx,
                    a[0] as i32,
                    a[1],
                    a[2] as i32,
                    deadline,
                    a[4],
                    a[5] as usize,
                ),
                Err(e) => Err(e),
            },
            448 => mem::process_mrelease(cx, a[0] as i32, a[1] as u32),
            449 => misc::futex_waitv(cx, a[0], a[1] as u32, a[2] as u32, a[3], a[4] as i32),
            451 => fs::cachestat(cx, a[0] as i32, a[1], a[2], a[3] as u32),
            _ => {
                // -1 from the program itself is ENOSYS on Linux too
                if nr != NO_SYSCALL {
                    crate::stub::hit(cx, crate::stub::Stub::Syscall(nr));
                }
                Err(Errno::ENOSYS)
            }
        };
        if result == Err(Errno::FUSE_RETRY) {
            retries += 1;
            if retries > FUSE_RETRIES {
                break Err(Errno::EIO);
            }
            let served = match crate::mm::take_wanted() {
                // A copy that cannot be completed is EFAULT, as on Linux; a signal before the
                // daemon took the read restarts the syscall
                Some(w) => fuse::fill(cx, w).map_err(|e| match e {
                    Errno::EINTR => Errno::ERESTARTSYS,
                    _ => Errno::EFAULT,
                }),
                None => crate::vfs::fuse::serve(cx),
            };
            match served {
                Ok(()) => continue,
                Err(e) => break Err(e),
            }
        }
        break result;
    };
    // Releases of FUSE files dropped while their connection was borrowed
    crate::vfs::fuse::flush_released();
    // Between syscalls nothing holds a FUSE node but what in_use finds
    fuse::sweep(cx);
    // A detached mount whose last open file or directory this syscall let go of
    if let Ok(vfs) = cx.vfs() {
        mount::collect_released(cx.kr(), &vfs);
    }
    // Removed overlay nodes nothing uses any more
    mount::sweep_overlays(cx);
    // Changes to watched cgroup files (crate::cgroup::notify_changes)
    crate::cgroup::notify_changes(cx.k());
    drop(in_syscall);
    // pids returned by set_tid_address, getpgid, getsid, setsid, getpid, getppid and gettid, and
    // the pid or negative process group ID returned by fcntl(F_GETOWN) (wait4's are translated
    // before reaping)
    let result = match &pid_ns {
        Some(ns) if matches!(nr, 96 | 155 | 156 | 157 | 172 | 173 | 178) => {
            result.map(|v| ns.vnr(v as Tid).unwrap_or(0) as u64)
        }
        Some(ns) if nr == 25 && a[1] == 9 => result.map(|v| {
            let v = v as i32;
            let l = ns.vnr(v.abs()).unwrap_or(0);
            (if v < 0 { -l } else { l }) as u32 as u64
        }),
        _ => result,
    };
    // A read or write on an fd returned EAGAIN, or read, write, recvfrom or sendto did not
    // transfer the full length asked for: the program has used up, in this direction, the event
    // EPOLLET last reported (poll::drained). The table holds (argument index, direction):
    // sendfile writes a[0] and reads a[1], splice and tee read a[0] and write a[2]
    let dirs: &[(usize, usize)] = match nr {
        63 | 65 | 202 | 207 | 212 | 242 | 243 => &[(0, 0)],
        64 | 66 | 206 | 211 | 269 => &[(0, 1)],
        71 => &[(0, 1), (1, 0)],
        76 | 77 => &[(0, 0), (2, 1)],
        _ => &[],
    };
    let short = matches!((nr, &result), (63 | 64 | 206 | 207, Ok(n)) if *n < a[2]);
    if !dirs.is_empty() && (short || result == Err(Errno::EAGAIN)) {
        for &(i, dir) in dirs {
            let file = cx.fds().get(a[i] as i32);
            if let Ok(file) = file {
                poll::drained(cx, &file, dir);
            }
        }
    }
    debug::trace_syscall(started, cx.tid(), nr, &a, &result);
    cpu.state_mut().x[0] = match result {
        Ok(v) => v,
        Err(errno) => errno.to_syscall_return(),
    };
    // Mappings were removed or permissions changed: other threads sharing the address space must
    // flush their TLBs first before the host memory can be freed
    let sync = cx.task.sync.clone();
    cx.mm_mut().sync(&sync);
    // PTRACE_SYSCALL: stop at exit, where the tracer sees the return value; a single-stepped svc is
    // reported here
    if exit_stop && !crate::ptrace::syscall_exit(cx, cpu) {
        return Flow::Exit;
    }
    crate::ptrace::syscall_stepped(cx);
    // Handle pending signals and syscall restarts before returning to user space, as on Linux
    signal::deliver(cx, cpu, true)
}

/// The thread is inside BKL system call `nr` while this lives ([`crate::task::Task::syscall`]),
/// left on every way out of [`dispatch`].
struct InSyscall(Rc<crate::task::Task>);

impl InSyscall {
    fn enter(task: &Rc<crate::task::Task>, nr: u64) -> InSyscall {
        let n = crate::task::SYSCALLS.fetch_add(1, Ordering::Relaxed) + 1;
        task.syscall.set((n, nr));
        InSyscall(task.clone())
    }
}

impl Drop for InSyscall {
    fn drop(&mut self) {
        self.0.syscall.set((0, 0));
    }
}

/// Calling `nr` in pid namespace `ns`: pids, tids and process group IDs in the arguments are
/// translated to global numbers (Linux's `find_vpid`), and invisible ones to [`NO_PID`]. Special
/// values such as 0 and -1 are unchanged, and a negative process group ID is translated negated.
/// Those inside structs (capget, `F_SETOWN_EX`, `TIOCSPGRP`, `SCM_CREDENTIALS`) are each
/// translated on their own.
fn pid_args(ns: &crate::pidns::PidNs, nr: u64, a: &mut [u64; 6]) {
    let global = |v: u64| -> u64 {
        let p = v as i32;
        let g = |l: Tid| ns.find(l).unwrap_or(NO_PID);
        (match p {
            p if p > 0 => g(p),
            p if p < -1 && p != i32::MIN => -g(-p),
            p => p,
        }) as u32 as u64
    };
    let idx: &[usize] = match nr {
        // process_vm_readv/writev, get_robust_list, sched_*, kill, tkill, rt_sigqueueinfo,
        // getpgid, getsid, prlimit64, wait4, sched_setattr/getattr, pidfd_open
        270
        | 271
        | 100
        | 118..=123
        | 127
        | 129
        | 130
        | 138
        | 155
        | 156
        | 261
        | 260
        | 274
        | 275
        | 434 => &[0],
        // ptrace (the first argument is the request)
        117 => &[1],
        // tgkill, rt_tgsigqueueinfo, setpgid, kcmp
        131 | 240 | 154 | 272 => &[0, 1],
        // waitid(P_PID or P_PGID), setpriority/getpriority(PRIO_PROCESS or PRIO_PGRP)
        95 if matches!(a[0], 1 | 2) => &[1],
        140 | 141 if matches!(a[0], 0 | 1) => &[1],
        // ioprio_set/ioprio_get(IOPRIO_WHO_PROCESS or IOPRIO_WHO_PGRP)
        30 | 31 if matches!(a[0], 1 | 2) => &[1],
        // fcntl(F_SETOWN)
        25 if a[1] == 8 => &[2],
        _ => &[],
    };
    for &i in idx {
        a[i] = global(a[i]);
    }
    // fcntl(F_SETOWN): -1 is process group 1 there, not every process as for kill
    if nr == 25 && a[1] == 8 && a[2] as i32 == -1 {
        a[2] = (-ns.find(1).unwrap_or(NO_PID)) as u32 as u64;
    }
}

/// execve succeeded (`old_tid` is the tid before): when traced, reports the exec
/// (`PTRACE_EVENT_EXEC` or SIGTRAP) and the system call exit, and delivers signals before the
/// first instruction of the new program runs.
fn exec_done(cx: &mut Cx<'_>, cpu: &mut Cpu, old_tid: crate::kernel::Tid) -> Flow {
    crate::ptrace::exec_hw_flush(cx, cpu);
    if !crate::ptrace::traced(cx.task) {
        return Flow::Exec;
    }
    if !crate::ptrace::exec(cx, cpu, old_tid) || !crate::ptrace::syscall_exit(cx, cpu) {
        return Flow::Exit;
    }
    match signal::deliver(cx, cpu, false) {
        Flow::Exit => Flow::Exit,
        _ => Flow::Exec,
    }
}

/// Reads a guest path argument.
fn read_path(cx: &Cx<'_>, addr: u64) -> Result<Vec<u8>, Errno> {
    cx.mm().read_cstr(addr, PATH_MAX - 1)
}

/// Reads a `struct timespec` ([`timespec`]); returns EINVAL when the seconds or nanoseconds are
/// out of range.
fn read_timespec(cx: &Cx<'_>, addr: u64) -> Result<std::time::Duration, Errno> {
    let b: [u8; 16] = get(cx, addr)?;
    let sec = i64::from_le_bytes(b[..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(b[8..].try_into().unwrap());
    timespec(sec, nsec).ok_or(Errno::EINVAL)
}

/// Reads a fixed-length byte array from guest memory.
fn get<const N: usize>(cx: &Cx<'_>, addr: u64) -> Result<[u8; N], Errno> {
    let mut b = [0u8; N];
    cx.read_user(UserSlice::new(addr, N), &mut b)?;
    Ok(b)
}

/// The longest time span Linux represents (`KTIME_MAX`, about 292 years). Longer timeouts and
/// timer values saturate to it, as in `timespec64_to_ktime`: they never expire in practice (GNU
/// `sleep infinity` sleeps for `i64::MAX` seconds), and adding them to the current instant cannot
/// overflow.
const KTIME_MAX: std::time::Duration = std::time::Duration::from_nanos(i64::MAX as u64);

/// The time span of a `struct timespec`, saturated at [`KTIME_MAX`]; None when the seconds or
/// nanoseconds are out of range.
fn timespec(sec: i64, nsec: i64) -> Option<std::time::Duration> {
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return None;
    }
    Some(std::time::Duration::new(sec as u64, nsec as u32).min(KTIME_MAX))
}

/// Writes a pair of 64-bit integers (timespec, timeval, rlimit).
fn write_pair(cx: &Cx<'_>, addr: u64, a: i64, b: i64) -> Result<(), Errno> {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&a.to_le_bytes());
    buf[8..].copy_from_slice(&b.to_le_bytes());
    put(cx, addr, buf)
}

/// Writes an integer type to guest memory.
fn put<const N: usize>(cx: &Cx<'_>, addr: u64, bytes: [u8; N]) -> Result<(), Errno> {
    cx.write_user(UserSlice::new(addr, N), &bytes)
}
