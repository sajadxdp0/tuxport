//! Linux errno (the guest ABI's numbers, independent of the host; the kernel/signal map).

use std::fmt;

/// A Linux errno value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Errno(pub i32);

impl Errno {
    /// Operation not permitted.
    pub const EPERM: Errno = Errno(1);
    /// No such file.
    pub const ENOENT: Errno = Errno(2);
    /// No such process.
    pub const ESRCH: Errno = Errno(3);
    /// Interrupted by a signal.
    pub const EINTR: Errno = Errno(4);
    /// I/O error.
    pub const EIO: Errno = Errno(5);
    /// No such device or address.
    pub const ENXIO: Errno = Errno(6);
    /// Argument list too long.
    pub const E2BIG: Errno = Errno(7);
    /// Executable format error.
    pub const ENOEXEC: Errno = Errno(8);
    /// Invalid fd.
    pub const EBADF: Errno = Errno(9);
    /// No child processes.
    pub const ECHILD: Errno = Errno(10);
    /// Temporarily unavailable.
    pub const EAGAIN: Errno = Errno(11);
    /// Out of memory.
    pub const ENOMEM: Errno = Errno(12);
    /// Permission denied.
    pub const EACCES: Errno = Errno(13);
    /// Invalid guest address.
    pub const EFAULT: Errno = Errno(14);
    /// Device or resource busy.
    pub const EBUSY: Errno = Errno(16);
    /// File exists.
    pub const EEXIST: Errno = Errno(17);
    /// Cross-filesystem.
    pub const EXDEV: Errno = Errno(18);
    /// The device does not support the operation.
    pub const ENODEV: Errno = Errno(19);
    /// Not a directory.
    pub const ENOTDIR: Errno = Errno(20);
    /// Is a directory.
    pub const EISDIR: Errno = Errno(21);
    /// Invalid argument.
    pub const EINVAL: Errno = Errno(22);
    /// The system's open file table is full (also used here when interface name numbers run
    /// out).
    pub const ENFILE: Errno = Errno(23);
    /// Out of fds.
    pub const EMFILE: Errno = Errno(24);
    /// Not a terminal.
    pub const ENOTTY: Errno = Errno(25);
    /// No space left on device.
    pub const ENOSPC: Errno = Errno(28);
    /// Cannot seek.
    pub const ESPIPE: Errno = Errno(29);
    /// Read-only filesystem.
    pub const EROFS: Errno = Errno(30);
    /// Too many links (an nftables jump depth over the limit).
    pub const EMLINK: Errno = Errno(31);
    /// The pipe's read ends are all closed.
    pub const EPIPE: Errno = Errno(32);
    /// Argument out of the function's domain (a socket timeout's microseconds).
    pub const EDOM: Errno = Errno(33);
    /// Result out of range.
    pub const ERANGE: Errno = Errno(34);
    /// Would deadlock.
    pub const EDEADLK: Errno = Errno(35);
    /// Name too long.
    pub const ENAMETOOLONG: Errno = Errno(36);
    /// A program file being executed cannot be written, and a file open for writing cannot be
    /// executed.
    pub const ETXTBSY: Errno = Errno(26);
    /// File too large (a semop semaphore number out of range).
    pub const EFBIG: Errno = Errno(27);
    /// No message matching the conditions (msgrcv).
    pub const ENOMSG: Errno = Errno(42);
    /// The IPC object has been removed.
    pub const EIDRM: Errno = Errno(43);
    /// The file handle is stale.
    pub const ESTALE: Errno = Errno(116);
    /// Operation canceled (a `TFD_TIMER_CANCEL_ON_SET` timerfd after the wall clock jumped).
    pub const ECANCELED: Errno = Errno(125);
    /// No such attribute (xattr).
    pub const ENODATA: Errno = Errno(61);
    /// Unimplemented syscall.
    pub const ENOSYS: Errno = Errno(38);
    /// Directory not empty.
    pub const ENOTEMPTY: Errno = Errno(39);
    /// Too many levels of symbolic links.
    pub const ELOOP: Errno = Errno(40);
    /// Value overflow.
    pub const EOVERFLOW: Errno = Errno(75);
    /// The fd is in a bad state (the fd does not exist when bpf(2) fetches info).
    pub const EBADFD: Errno = Errno(77);
    /// Protocol error (a FUSE daemon answering INIT with another major version).
    pub const EPROTO: Errno = Errno(71);
    /// Too many users (a multithreaded process setns-ing into a time namespace).
    pub const EUSERS: Errno = Errno(87);
    /// Accessing a corrupted shared library (execve: an interpreter that is not an aarch64 ELF).
    pub const ELIBBAD: Errno = Errno(80);
    /// Not a socket.
    pub const ENOTSOCK: Errno = Errno(88);
    /// Message too long.
    pub const EMSGSIZE: Errno = Errno(90);
    /// Socket type does not match the protocol.
    pub const EPROTOTYPE: Errno = Errno(91);
    /// Unsupported socket option.
    pub const ENOPROTOOPT: Errno = Errno(92);
    /// Unsupported protocol.
    pub const EPROTONOSUPPORT: Errno = Errno(93);
    /// Unsupported socket type.
    pub const ESOCKTNOSUPPORT: Errno = Errno(94);
    /// Unsupported operation.
    pub const EOPNOTSUPP: Errno = Errno(95);
    /// Unsupported address family.
    pub const EAFNOSUPPORT: Errno = Errno(97);
    /// Address already in use.
    pub const EADDRINUSE: Errno = Errno(98);
    pub const EADDRNOTAVAIL: Errno = Errno(99);
    /// The network (interface) is down.
    pub const ENETDOWN: Errno = Errno(100);
    /// Network unreachable.
    pub const ENETUNREACH: Errno = Errno(101);
    /// Connection aborted (a FUSE request in flight when its connection was aborted).
    pub const ECONNABORTED: Errno = Errno(103);
    /// Connection reset by peer (an AF_UNIX connection closed with data it had not read).
    pub const ECONNRESET: Errno = Errno(104);
    /// No buffer space (a netlink socket that lost notifications to a full queue).
    pub const ENOBUFS: Errno = Errno(105);
    /// Already connected.
    pub const EISCONN: Errno = Errno(106);
    /// Not connected.
    pub const ENOTCONN: Errno = Errno(107);
    /// Timed out.
    pub const ETIMEDOUT: Errno = Errno(110);
    /// Connection refused.
    pub const ECONNREFUSED: Errno = Errno(111);
    /// Host unreachable.
    pub const EHOSTUNREACH: Errno = Errno(113);
    /// A connection is already in progress.
    pub const EALREADY: Errno = Errno(114);
    /// Connection in progress.
    pub const EINPROGRESS: Errno = Errno(115);

    // What follows are syscall restart codes used inside the kernel (the kernel/signal map):
    // before returning to user space they are always replaced by re-executing the syscall or by
    // EINTR, so the guest never sees them.

    /// Interrupted by a signal: re-executed when the handler has `SA_RESTART`, otherwise EINTR
    /// is returned.
    pub const ERESTARTSYS: Errno = Errno(512);
    /// Interrupted by a signal: always re-executed.
    pub const ERESTARTNOINTR: Errno = Errno(513);
    /// Interrupted by a signal: EINTR is returned when there is a handler, otherwise
    /// re-executed.
    pub const ERESTARTNOHAND: Errno = Errno(514);
    /// Interrupted by a signal: EINTR is returned when there is a handler, otherwise it is changed
    /// into running `restart_syscall`, which goes on with the rest (the thread's
    /// [`crate::task::TaskState::restart`]).
    pub const ERESTART_RESTARTBLOCK: Errno = Errno(516);

    /// Not an errno: a walk needs a reply from a FUSE daemon that it cannot wait for, so the
    /// syscall dispatcher asks for it and runs the syscall again ([`crate::vfs::fuse::serve`]).
    /// Never returned to the guest.
    pub const FUSE_RETRY: Errno = Errno(600);

    /// The value a syscall returns to the guest: `-errno`.
    pub fn to_syscall_return(self) -> u64 {
        (-(self.0 as i64)) as u64
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "errno {}", self.0)
    }
}

impl std::error::Error for Errno {}
