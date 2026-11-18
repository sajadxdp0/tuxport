# docker-run-options: --cap-add SYS_ADMIN
# PTY modes: TCGETS2, TCSETS2, TCSETSW2 and TCSETSF2 of struct termios2 (from glibc 2.42 on
# tcgetattr, tcsetattr and isatty use only these), speed fields converted by CBAUD and CIBAUD
# (Linux's set_termios), a PTY always being 8-bit, without parity and with receiving enabled
# (pty_set_termios), and the address bit not changing
python3 - <<'PY'
import errno, fcntl, functools, os, select, struct, termios

print = functools.partial(print, flush=True)
TCGETS2, TCSETS2, TCSETSW2, TCSETSF2 = 0x802c542a, 0x402c542b, 0x402c542c, 0x402c542d
FMT2 = "<4IB19s2I"
BOTHER, CBAUD, CBAUDEX, IBSHIFT, ADDRB = 0o10000, 0o10017, 0o10000, 16, 0o4000000000


def err(f, *args):
    try:
        r = f(*args)
        return "ok" if r is None else r
    except OSError as e:
        return errno.errorcode[e.errno]


def get2(fd):
    b = fcntl.ioctl(fd, TCGETS2, bytes(44))
    return list(struct.unpack(FMT2, b))


def set2(fd, t, req=TCSETS2):
    return fcntl.ioctl(fd, req, struct.pack(FMT2, *t))


def show(label, fd):
    i, o, c, l, line, cc, isp, osp = get2(fd)
    print(label, "iflag=%o oflag=%o cflag=%o lflag=%o line=%d" % (i, o, c, l, line),
          "cc=" + cc.hex(), "ispeed=%d ospeed=%d" % (isp, osp))


m, s = os.openpty()
show("slave", s)
show("master", m)
# The first 36 bytes of TCGETS are the same as TCGETS2's
old = fcntl.ioctl(s, termios.TCGETS, bytes(36))
print("tcgets prefix", old == fcntl.ioctl(s, TCGETS2, bytes(44))[:36])

# Not a terminal: ENOTTY; a bad pointer: EFAULT
r, w = os.pipe()
nul = os.open("/dev/null", os.O_RDWR)
f = os.open("/tmp", os.O_RDONLY)
for name, fd in [("pipe", r), ("null", nul), ("dir", f)]:
    print(name, err(fcntl.ioctl, fd, TCGETS2, bytes(44)), err(fcntl.ioctl, fd, TCSETS2, bytes(44)))
import ctypes
libc = ctypes.CDLL(None, use_errno=True)
libc.ioctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_void_p]
for req in (TCGETS2, TCSETS2):
    rc = libc.ioctl(s, req, ctypes.c_void_p(8))
    print("fault", hex(req), rc, errno.errorcode.get(ctypes.get_errno()))


def setcflag(fd, cbaud, ibaud=0, isp=0, osp=0, req=TCSETS2, keep=None):
    t = get2(fd)
    t[2] = t[2] & ~CBAUD & ~(CBAUD << IBSHIFT) | cbaud | (ibaud << IBSHIFT)
    if keep is None:
        t[6], t[7] = isp, osp
    set2(fd, t, req)
    i, o, c, l, line, cc, isp2, osp2 = get2(fd)
    return "cflag=%o ispeed=%d ospeed=%d" % (c, isp2, osp2)


# The old Bxxx style: the speed is converted by the table, the values in the fields do not count
print("B9600", setcflag(s, 0o15, isp=1, osp=2))
print("B115200/B1200", setcflag(s, 0o10002, 0o11))
print("B4000000", setcflag(s, 0o10017))
print("B0", setcflag(s, 0))
# BOTHER: use the speed in the fields; input and output can differ
print("BOTHER", setcflag(s, BOTHER, BOTHER, 12345, 54321))
print("BOTHER out only", setcflag(s, BOTHER, 0, 777, 31250))
print("BOTHER in only", setcflag(s, 0o15, BOTHER, 4242, 0))
# What is set on the master is the slave's
print("via master", setcflag(m, BOTHER, 0, 0, 2400))
show("slave after master", s)

# The old TCSETS with BOTHER: the speed fields keep their previous values
t = get2(s)
t[2] = t[2] & ~CBAUD & ~(CBAUD << IBSHIFT) | BOTHER
t[6], t[7] = 1111, 2222
set2(s, t)
t2 = list(struct.unpack("<4IB19s", fcntl.ioctl(s, termios.TCGETS, bytes(36))))
t2[2] = t2[2] & ~CBAUD | BOTHER | (0o15 << IBSHIFT)
fcntl.ioctl(s, termios.TCSETS, struct.pack("<4IB19s", *t2))
show("tcsets bother", s)
t2[2] = t2[2] & ~CBAUD & ~(CBAUD << IBSHIFT) | 0o17
fcntl.ioctl(s, termios.TCSETS, struct.pack("<4IB19s", *t2))
show("tcsets b38400", s)

# TCSETA (termio) also recomputes the speeds
ta = bytearray(fcntl.ioctl(s, termios.TCGETA, bytes(18)))
c = struct.unpack_from("<H", ta, 4)[0]
struct.pack_into("<H", ta, 4, c & ~0o17 | 0o13)
fcntl.ioctl(s, termios.TCSETA, bytes(ta))
show("tcseta b2400", s)

# A PTY is always CS8, CREAD, without parity, and the address bit cannot be changed
t = get2(s)
t[2] = t[2] & ~0o60 & ~0o200 | 0o20 | 0o400 | 0o1000 | ADDRB
set2(s, t)
show("cs6 parenb parodd addrb", s)

# Other flags, the control characters and the line discipline number are kept as is; TCSETSF2
# flushes unread input, TCSETSW2 works as usual
t = get2(s)
t[0] |= termios.IXANY
t[3] &= ~termios.ECHO
t[4] = 0
cc = bytearray(t[5])
cc[termios.VINTR] = 1
t[5] = bytes(cc)
set2(s, t, TCSETSW2)
show("tcsetsw2", s)
os.write(m, b"abc\n")
# Natively the data reaches the line discipline asynchronously through the flip buffer
select.select([s], [], [], 5)
print("before flush", struct.unpack("<i", fcntl.ioctl(s, termios.FIONREAD, bytes(4)))[0])
set2(s, get2(s), TCSETSF2)
print("after tcsetsf2", struct.unpack("<i", fcntl.ioctl(s, termios.FIONREAD, bytes(4)))[0])

# Input already there is kept when canonical mode is switched on or off (as with TCSETS)
os.write(m, b"xy")
t = get2(s)
t[3] &= ~termios.ICANON
set2(s, t)
print("raw read", os.read(s, 10))

# Python's termios module (through libc's tcgetattr/tcsetattr) agrees with the ioctls
a = termios.tcgetattr(s)
a[4] = a[5] = termios.B57600
termios.tcsetattr(s, termios.TCSANOW, a)
show("tcsetattr b57600", s)
print("isatty", os.isatty(s), os.isatty(m), os.isatty(r))
PY

# Packet mode (TIOCPKT, Linux's pty_set_pktmode, n_tty_read, n_tty_poll): each read on the master
# first gives a status byte. When there is a status (the slave flushing, ^S/^Q stopping and
# resuming, flow control switching on or off, termios changing under EXTPROC) only that is read,
# otherwise it is TIOCPKT_DATA (0) plus data; with a status, poll includes POLLPRI. GNU screen
# needs it for every window it opens, and opens no window if it fails
python3 - <<'PY'
import errno, fcntl, functools, os, select, signal, struct, termios, time

print = functools.partial(print, flush=True)
TIOCPKT, TIOCGPKT, TIOCGPTLCK, TIOCSIG = 0x5420, 0x80045438, 0x80045439, 0x40045436
EXTPROC = 0o200000
NAMES = {select.POLLIN: "IN", select.POLLPRI: "PRI", select.POLLOUT: "OUT", select.POLLERR: "ERR",
         select.POLLHUP: "HUP", select.POLLRDNORM: "RDNORM", select.POLLWRNORM: "WRNORM"}


def err(f, *args):
    try:
        r = f(*args)
        return "ok" if r is None else r
    except OSError as e:
        return errno.errorcode[e.errno]


def geti(fd, req):
    return struct.unpack("i", fcntl.ioctl(fd, req, bytes(4)))[0]


def pkt(fd, on):
    return fcntl.ioctl(fd, TIOCPKT, struct.pack("i", on))


def events(fd):
    p = select.poll()
    p.register(fd, select.POLLIN | select.POLLPRI | select.POLLOUT)
    r = p.poll(0)
    return "|".join(n for b, n in sorted(NAMES.items()) if r and r[0][1] & b) or "-"


def wait(fd, pri=False):
    # Natively data written to the master reaches the line discipline asynchronously through the
    # flip buffer; wait until it produces a result. When waiting for a status (pri), also wait
    # for readability: Linux's pty_stop and pty_start wake with only EPOLLIN, so a select that
    # waits only for exceptional conditions is not woken and has to wait for the timeout
    select.select([fd], [], [fd] if pri else [], 5)


def rd(fd, n=100):
    try:
        return os.read(fd, n)
    except OSError as e:
        return errno.errorcode[e.errno]


def change(fd, f):
    a = termios.tcgetattr(fd)
    f(a)
    termios.tcsetattr(fd, termios.TCSANOW, a)


m, s = os.openpty()
os.set_blocking(m, False)
print("slave TIOCPKT", err(pkt, s, 1), "slave TIOCGPKT", err(geti, s, TIOCGPKT),
      "slave TIOCGPTLCK", err(geti, s, TIOCGPTLCK), "slave TIOCSIG", err(fcntl.ioctl, s, TIOCSIG, 2))
print("TIOCGPKT", geti(m, TIOCGPKT), "TIOCGPTLCK", geti(m, TIOCGPTLCK))
print("bad pointer", err(fcntl.ioctl, m, TIOCPKT, 0))
os.write(s, b"before")
wait(m)
print("plain read", rd(m))
pkt(m, 1)
print("TIOCGPKT on", geti(m, TIOCGPKT), "events", events(m), "empty read", rd(m), "zero read", rd(m, 0))
os.write(s, b"hello\n")
wait(m)
print("events with data", events(m))
print("one byte", rd(m, 1), "then", rd(m))
print("select exc", select.select([], [], [m], 0)[2] == [])

# Flushes on the slave side: input is FLUSHREAD, output is FLUSHWRITE; those on the master side
# are not reported
for name, q in [("TCIFLUSH", termios.TCIFLUSH), ("TCOFLUSH", termios.TCOFLUSH), ("TCIOFLUSH", termios.TCIOFLUSH)]:
    termios.tcflush(s, q)
    print("slave", name, events(m), select.select([], [], [m], 0)[2] == [m], rd(m), rd(m))
termios.tcflush(m, termios.TCIOFLUSH)
print("master TCIOFLUSH", events(m), rd(m))
# TCSETSF from either side flushes the slave's input
termios.tcsetattr(m, termios.TCSAFLUSH, termios.tcgetattr(m))
print("TCSAFLUSH via master", rd(m))
termios.tcsetattr(s, termios.TCSADRAIN, termios.tcgetattr(s))
print("TCSADRAIN", rd(m))

# Stop and resume: TCOOFF/TCOON and ^S/^Q; when both have happened only the latter remains
termios.tcflow(s, termios.TCOOFF)
print("TCOOFF", rd(m))
termios.tcflow(s, termios.TCOOFF)
print("TCOOFF again", rd(m))
termios.tcflow(s, termios.TCOON)
print("TCOON", rd(m))
termios.tcflow(s, termios.TCOOFF)
termios.tcflow(s, termios.TCOON)
print("off and on", rd(m))
os.write(m, b"\x13")
wait(m, True)
print("^S", rd(m))
os.write(m, b"\x11")
wait(m, True)
print("^Q", rd(m), rd(m))
termios.tcflow(m, termios.TCOOFF)
termios.tcflow(m, termios.TCOON)
print("master TCOOFF/TCOON", rd(m))

# Toggling ^S/^Q flow control: it counts only if IXON, VSTOP and VSTART are all at their defaults
change(s, lambda a: a.__setitem__(0, a[0] & ~termios.IXON))
print("IXON off", rd(m))
change(s, lambda a: a.__setitem__(0, a[0] & ~termios.IXON))
print("again", rd(m))
change(m, lambda a: a.__setitem__(0, a[0] | termios.IXON))
print("IXON on via master", rd(m))


def vstop(c):
    def f(a):
        a[6][termios.VSTOP] = c
    return f


change(s, vstop(b"\x01"))
print("VSTOP changed", rd(m))
change(s, vstop(b"\x13"))
print("VSTOP back", rd(m))
change(s, lambda a: a.__setitem__(3, a[3] | EXTPROC))
print("EXTPROC on", rd(m))
change(s, lambda a: a.__setitem__(1, a[1]))
print("EXTPROC kept", rd(m))
change(s, lambda a: a.__setitem__(3, a[3] & ~EXTPROC))
print("EXTPROC off", rd(m), rd(m))

# ^C: flushes both sides even without a foreground process group (NOFLSH not set). Echo turned
# off: Linux's output flush only clears what is still in the flip buffer, and when the echo
# reaches the master side is unpredictable
change(s, lambda a: a.__setitem__(3, a[3] & ~termios.ECHO))
os.write(m, b"abc\x03")
wait(m, True)
print("^C", rd(m), "input left", fcntl.ioctl(s, termios.FIONREAD, bytes(4)) == bytes(4))
change(s, lambda a: a.__setitem__(3, a[3] | termios.NOFLSH))
os.write(m, b"x\x03")
time.sleep(0.2)
print("^C with NOFLSH", rd(m))
change(s, lambda a: a.__setitem__(3, a[3] & ~termios.NOFLSH | termios.ECHO))
termios.tcflush(s, termios.TCIFLUSH)
rd(m)

# Turning packet mode off: no header, and statuses are not recorded; when it is turned on again
# the earlier statuses are gone too
termios.tcflush(s, termios.TCIFLUSH)
pkt(m, 0)
print("off", geti(m, TIOCGPKT), events(m))
os.write(s, b"plain")
wait(m)
print("off read", rd(m))
termios.tcflush(s, termios.TCOFLUSH)
pkt(m, 1)
print("on again", events(m), rd(m))
termios.tcflush(s, termios.TCIFLUSH)
pkt(m, 1)
print("on while on", rd(m))

# EPOLLPRI of epoll
ep = select.epoll()
ep.register(m, select.EPOLLIN | select.EPOLLPRI)
termios.tcflow(s, termios.TCOOFF)
print("epoll", ep.poll(0), rd(m))
termios.tcflow(s, termios.TCOON)
rd(m)

# After all slaves are closed: first the status and data are read out, then EIO
os.write(s, b"last")
wait(m)
termios.tcflush(s, termios.TCIFLUSH)
os.close(s)
print("closed", events(m), rd(m), rd(m), rd(m))
os.close(m)

# TIOCSIG: sends a signal to the slave's foreground process group; only SIGINT, SIGQUIT and
# SIGTSTP
m, s = os.openpty()
name = os.ttyname(s)
print("TIOCSIG without fg", err(fcntl.ioctl, m, TIOCSIG, signal.SIGINT), err(fcntl.ioctl, m, TIOCSIG, signal.SIGKILL),
      err(fcntl.ioctl, m, TIOCSIG, 0))
r, w = os.pipe()
pid = os.fork()
if pid == 0:
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    os.setsid()
    fd = os.open(name, os.O_RDWR)
    os.write(w, b"x")
    signal.pause()
    os._exit(0)
os.read(r, 1)
print("TIOCSIG SIGINT", err(fcntl.ioctl, m, TIOCSIG, signal.SIGINT))
_, st = os.waitpid(pid, 0)
print("child", os.WIFSIGNALED(st) and signal.Signals(os.WTERMSIG(st)).name)
PY

# A newly allocated terminal: the window size is all 0; reading the master is EIO only after the
# slave has been opened and then all closed (poll includes POLLHUP, Linux's TTY_OTHER_CLOSED),
# and waits as usual when it has never been opened. GNU screen's main process is already reading
# the master before its child opens the slave
python3 - <<'PY'
import errno, fcntl, functools, os, select, struct, time

print = functools.partial(print, flush=True)
TIOCGPTN, TIOCSPTLCK = 0x80045430, 0x40045431


def st(m):
    p = select.poll()
    p.register(m, select.POLLIN | select.POLLOUT)
    ev = p.poll(0)
    try:
        r = os.read(m, 10)
    except OSError as e:
        r = errno.errorcode[e.errno]
    return (ev[0][1] if ev else 0), r


def lock(m, on):
    fcntl.ioctl(m, TIOCSPTLCK, struct.pack("i", on))


m = os.open("/dev/ptmx", os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
name = "/dev/pts/%d" % struct.unpack("I", fcntl.ioctl(m, TIOCGPTN, bytes(4)))[0]
print("winsize", struct.unpack("4H", fcntl.ioctl(m, 0x5413, bytes(8))))  # TIOCGWINSZ
print("locked", st(m))
lock(m, 0)
print("unlocked, never opened", st(m))
s = os.open(name, os.O_RDWR | os.O_NOCTTY)
print("slave open", st(m))
s2 = os.dup(s)
os.close(s)
print("one of two closed", st(m))
os.close(s2)
print("all closed", st(m))
lock(m, 1)
print("locked again", st(m))
lock(m, 0)
s = os.open(name, os.O_RDWR | os.O_NOCTTY)
print("reopened", st(m))
os.close(s)
os.close(m)

# Blocking read: the child opens the slave only after a while, writes a line, and closes it
m = os.open("/dev/ptmx", os.O_RDWR | os.O_NOCTTY)
name = "/dev/pts/%d" % struct.unpack("I", fcntl.ioctl(m, TIOCGPTN, bytes(4)))[0]
lock(m, 0)
pid = os.fork()
if pid == 0:
    time.sleep(0.3)
    s = os.open(name, os.O_RDWR | os.O_NOCTTY)
    os.write(s, b"late\n")
    time.sleep(0.2)
    os._exit(0)
print("blocking read", os.read(m, 100))
os.waitpid(pid, 0)
print("after the child", st(m))
PY

# The master reports the slave's foreground group and session to any holder (Linux's tiocgpgrp and
# tiocgsid): a tmux server, which has no controlling terminal, names windows and finds a pane's cwd
# this way. A slave that is not the caller's controlling terminal answers ENOTTY, and so does
# TIOCNOTTY on a master
python3 - <<'PY'
import errno, fcntl, functools, os, struct, time

print = functools.partial(print, flush=True)
TIOCSCTTY, TIOCGPGRP, TIOCNOTTY, TIOCGSID = 0x540E, 0x540F, 0x5422, 0x5429


def ask(fd, req):
    try:
        return struct.unpack("i", fcntl.ioctl(fd, req, bytes(4)))[0]
    except OSError as e:
        return errno.errorcode[e.errno]


m, s = os.openpty()
print("no session: master pgrp", ask(m, TIOCGPGRP), "sid", ask(m, TIOCGSID))
print("not our slave: pgrp", ask(s, TIOCGPGRP), "sid", ask(s, TIOCGSID))
r, w = os.pipe()
pid = os.fork()
if pid == 0:
    os.setsid()
    fcntl.ioctl(s, TIOCSCTTY, 0)
    os.write(w, str(ask(m, TIOCNOTTY)).encode().ljust(8))
    time.sleep(30)
    os._exit(0)
os.close(w)
print("leader's master TIOCNOTTY", os.read(r, 8).decode().strip())
print("child's session: master pgrp is the child", ask(m, TIOCGPGRP) == pid,
      "sid is the child", ask(m, TIOCGSID) == pid)
print("still not our slave: pgrp", ask(s, TIOCGPGRP), "sid", ask(s, TIOCGSID))
print("our master TIOCNOTTY", ask(m, TIOCNOTTY))
os.kill(pid, 9)
os.waitpid(pid, 0)
print("leader gone: master pgrp", ask(m, TIOCGPGRP), "sid", ask(m, TIOCGSID))
PY

# TIOCSTI fakes input: only on the caller's controlling terminal, or with CAP_SYS_ADMIN (Linux's
# tiocsti), so a process holding a terminal fd it got some other way (inherited after setsid or
# su -c, passed over a socket) cannot type commands into the shell reading it. On a master the
# byte goes to the master's own input (what master reads return), and a master is never the
# controlling terminal. Since 6.2 dev.tty.legacy_tiocsti off (as distribution kernels ship it)
# refuses everyone without CAP_SYS_ADMIN with EIO before 6.1's EPERM, so both count as refused, and
# typing into one's own controlling terminal is tested only in Tuxport (tests/guest/tiocsti.c)
python3 - <<'PY'
import errno, fcntl, functools, os, select

print = functools.partial(print, flush=True)
TIOCSCTTY, TIOCSTI = 0x540E, 0x5412


def sti(fd, text):
    try:
        for c in text:
            fcntl.ioctl(fd, TIOCSTI, c.encode())
        return "ok"
    except OSError as e:
        return "refused" if e.errno in (errno.EPERM, errno.EIO) else errno.errorcode[e.errno]


def pending(fd):
    out = b""
    while select.select([fd], [], [], 0.2)[0]:
        out += os.read(fd, 100)
    return out


m, s = os.openpty()
print("CAP_SYS_ADMIN on a slave that is not the ctty:", sti(s, "ab\n"), "slave reads", pending(s),
      "master reads the echo", pending(m))
print("CAP_SYS_ADMIN on the master:", sti(m, "z"), "master reads", pending(m), "slave reads",
      pending(s))
r, w = os.pipe()
pid = os.fork()
if pid == 0:
    os.close(r)
    os.setgid(65534)
    os.setuid(65534)
    out = ["unprivileged, slave not its ctty: " + sti(s, "x")]
    out.append("unprivileged, master: " + sti(m, "x"))
    os.setsid()
    fcntl.ioctl(s, TIOCSCTTY, 0)
    out.append("unprivileged, master of its ctty: " + sti(m, "y"))
    os.write(w, "\n".join(out).encode())
    os._exit(0)
os.close(w)
print(os.read(r, 1000).decode())
os.waitpid(pid, 0)
print("nothing typed: slave reads", pending(s), "master reads", pending(m))
PY

# Echoes that found the output full wait until the slave's next write, which sends them ahead of
# its own data (6.1's n_tty_write starts with process_echoes); reading the master alone does not
# send them. The slave fills the output, the master types a character whose echo has no room,
# reads everything, and the slave writes
python3 - <<'PY'
import fcntl, functools, os, select, time

print = functools.partial(print, flush=True)


def pending(fd):
    out = b""
    while select.select([fd], [], [], 0.3)[0]:
        out += os.read(fd, 65536)
    return out


m, s = os.openpty()
fcntl.fcntl(s, fcntl.F_SETFL, fcntl.fcntl(s, fcntl.F_GETFL) | os.O_NONBLOCK)
full = False
while not full:
    try:
        while True:
            os.write(s, b"o" * 1024)
    except BlockingIOError:
        pass
    time.sleep(0.2)
    try:
        os.write(s, b"o")
    except BlockingIOError:
        full = True
os.write(m, b"x")
time.sleep(0.2)
drained = pending(m)
print("output drained:", len(drained) > 0, "with the echo in it:", b"x" in drained)
os.write(s, b"DATA")
print("after the slave's write the master reads", pending(m))
PY

# Non-canonical reads with VMIN and VTIME both set: VTIME is an inter-byte timer (n_tty_read sets
# the timeout again after each arrival), so bytes 0.4 s apart keep a 1 s read waiting until VMIN
# of them have come, and only a pause of VTIME ends it with fewer
python3 - <<'PY'
import functools, os, termios, threading, time, tty

print = functools.partial(print, flush=True)
m, s = os.openpty()
tty.setraw(s)
attrs = termios.tcgetattr(s)
attrs[6][termios.VMIN] = 4
attrs[6][termios.VTIME] = 10
termios.tcsetattr(s, termios.TCSANOW, attrs)


def typed(data, gap):
    for c in data:
        os.write(m, bytes([c]))
        time.sleep(gap)


for data, gap in ((b"abcd", 0.4), (b"xy", 0.1)):
    writer = threading.Thread(target=typed, args=(data, gap), daemon=True)
    writer.start()
    print("read", os.read(s, 100))
    writer.join()
PY
