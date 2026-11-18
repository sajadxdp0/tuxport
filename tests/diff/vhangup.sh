# docker-run-options: --cap-add SYS_ADMIN --cap-add SYS_TTY_CONFIG
# vhangup(2) and TIOCVHANGUP (Linux's tty_vhangup): on a hangup, files already open read EOF and
# writes and ioctls get EIO, the session leader receives SIGHUP and SIGCONT, the session loses its
# controlling terminal, and unread input is discarded; the node remains and works as usual when
# reopened (login and agetty rely on this to drive off processes left on the terminal). The
# native side adds CAP_SYS_TTY_CONFIG (vhangup needs it) and CAP_SYS_ADMIN (TIOCVHANGUP needs it)
python3 - <<'PY'
import ctypes, errno, fcntl, functools, os, select, signal, struct, termios, time

print = functools.partial(print, flush=True)
libc = ctypes.CDLL(None, use_errno=True)
TIOCVHANGUP, TIOCSCTTY, TIOCGSID = 0x5437, 0x540e, 0x5429
POLL = {select.POLLIN: "IN", select.POLLOUT: "OUT", select.POLLERR: "ERR", select.POLLHUP: "HUP",
        select.POLLRDNORM: "RDNORM", select.POLLWRNORM: "WRNORM"}


def err(f, *args):
    try:
        r = f(*args)
        return "ok" if r is None else r
    except OSError as e:
        return errno.errorcode[e.errno]
    except termios.error as e:
        return errno.errorcode[e.args[0]]


def call(f, *args):
    r = f(*args)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


def polled(fd):
    p = select.poll()
    p.register(fd, select.POLLIN | select.POLLOUT)
    ev = p.poll(0)
    return " ".join(n for b, n in POLL.items() if ev and ev[0][1] & b) or "-"


def describe(what, fd):
    # Non-blocking: a read after the hangup is still 0, and without a hangup it does not hang the
    # test case
    os.set_blocking(fd, False)
    print(what, "poll", polled(fd), "read", err(os.read, fd, 100), "write", err(os.write, fd, b"x"),
          "tcgetattr", err(lambda: termios.tcgetattr(fd) and None), "tcsetpgrp", err(os.tcsetpgrp, fd, os.getpgrp()),
          "isatty", os.isatty(fd))


def fork(f):
    """Run f in a child process and return its exit code; an exception also ends the child, which
    does not go on to run the parent's code."""
    pid = os.fork()
    if pid == 0:
        code = 0
        try:
            f()
        except BaseException as e:
            print("child:", repr(e))
            code = 1
        os._exit(code)
    return os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1])


got = []
signal.signal(signal.SIGHUP, lambda s, f: got.append("HUP"))
signal.signal(signal.SIGCONT, lambda s, f: got.append("CONT"))

# With no controlling terminal vhangup does nothing
print("no ctty", call(libc.vhangup), got)

master, slave = os.openpty()
name = os.ttyname(slave)
os.close(slave)


def session():
    os.setsid()
    fd = os.open(name, os.O_RDWR)  # opened by the session leader: becomes the controlling terminal
    print("ctty", os.tcgetpgrp(fd) == os.getpgrp())
    # Another process in the session also has it open: after the hangup its file also reads EOF,
    # and it receives no signal
    r, w = os.pipe()
    other = os.fork()
    if other == 0:
        os.setpgid(0, 0)
        mine = os.open(name, os.O_RDWR | os.O_NOCTTY)
        mine_got = []
        signal.signal(signal.SIGHUP, lambda s, f: mine_got.append("HUP"))
        os.write(w, b"x")
        time.sleep(0.5)
        os.set_blocking(mine, False)
        print("other process", polled(mine), err(os.read, mine, 10), mine_got)
        os._exit(0)
    os.read(r, 1)
    os.write(master, b"typed but not read\n")
    time.sleep(0.1)
    print("vhangup", call(libc.vhangup), got)
    describe("old fd", fd)
    print("ioctl TIOCSCTTY", err(fcntl.ioctl, fd, TIOCSCTTY, 0), "fcntl", err(fcntl.fcntl, fd, fcntl.F_GETFL) != "EIO")
    print("/dev/tty", err(os.open, "/dev/tty", os.O_RDWR))
    os.waitpid(other, 0)
    # Hang up again: there is no controlling terminal any more, so nothing happens
    print("again", call(libc.vhangup), got)
    # The node remains and works as usual when reopened: the session leader gets a controlling
    # terminal again, and the input from before the hangup is gone
    fd2 = os.open(name, os.O_RDWR)
    print("reopen", os.isatty(fd2), polled(fd2), "sid", os.getsid(0) == struct.unpack("i", fcntl.ioctl(fd2, TIOCGSID, b"\0" * 4))[0],
          "/dev/tty", err(lambda: os.close(os.open("/dev/tty", os.O_RDWR))))
    os.write(master, b"after\n")
    print("read", os.read(fd2, 100))
    os.write(fd2, b"out\n")
    time.sleep(0.1)
    describe("old fd still", fd)
    # Hang up the terminal this file is on with TIOCVHANGUP
    print("TIOCVHANGUP", err(fcntl.ioctl, fd2, TIOCVHANGUP), got)
    describe("hung by ioctl", fd2)
    print("TIOCVHANGUP again", err(fcntl.ioctl, fd2, TIOCVHANGUP))


print("session", fork(session))
# The master is not affected: what was written before the slave was hung up can still be read
print("master", polled(master), os.read(master, 100))
fd = os.open(name, os.O_RDWR | os.O_NOCTTY)
os.write(master, b"to new\n")
print("new open", os.read(fd, 100))

# Without the capabilities: vhangup needs CAP_SYS_TTY_CONFIG, TIOCVHANGUP needs CAP_SYS_ADMIN
def unprivileged():
    os.setsid()
    t = os.open(name, os.O_RDWR)
    os.setresuid(65534, 65534, 65534)
    print("unprivileged", call(libc.vhangup), err(fcntl.ioctl, t, TIOCVHANGUP), os.isatty(t))
print("unprivileged", fork(unprivileged))

# TIOCVHANGUP on the master: it is the master side that is hung up
def master_side():
    m, s = os.openpty()
    os.write(s, b"from slave\n")
    time.sleep(0.1)
    print("master TIOCVHANGUP", err(fcntl.ioctl, m, TIOCVHANGUP))
    print("master after", polled(m), err(os.read, m, 100), err(os.write, m, b"y"))
    print("slave after", polled(s), err(os.write, s, b"z"), os.isatty(s))
    m2 = os.open("/dev/ptmx", os.O_RDWR | os.O_NOCTTY)
    print("new master", polled(m2))
print("master side", fork(master_side))

# The slave hung up in packet mode: the slave's input and output are flushed, and the master first
# reads these two states
def packet():
    m, s = os.openpty()
    fcntl.ioctl(m, 0x5420, struct.pack("i", 1))  # TIOCPKT
    os.set_blocking(m, False)
    print("packet TIOCVHANGUP", err(fcntl.ioctl, s, TIOCVHANGUP))
    print("packet master", polled(m), err(os.read, m, 100), err(os.read, m, 100))
print("packet", fork(packet))
PY
