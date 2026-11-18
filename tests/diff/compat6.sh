# docker-run-options: --privileged
# The sequel to compat5.sh (difftest passes the whole script as one argument of sh -c, which must
# not exceed 128 KB): sparse files (holes punched, found with SEEK_DATA/SEEK_HOLE and freeing
# storage, reservations, tmpfs counting the pages files take rather than their sizes) and
# MADV_REMOVE
set -e
cd /tmp
rm -rf holes
mkdir holes holes/t
mount -t tmpfs -o size=256k holes /tmp/holes/t

python3 - <<'PY'
import ctypes, errno, mmap, os
libc = ctypes.CDLL(None, use_errno=True)
libc.fallocate.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_long, ctypes.c_long]
KEEP, PUNCH, ZERO = 1, 2, 0x10

def fallocate(fd, mode, off, n):
    if libc.fallocate(fd, mode, off, n) == 0:
        return "ok"
    return errno.errorcode[ctypes.get_errno()]

def extents(fd):
    # The data extents, as SEEK_DATA and SEEK_HOLE find them
    out, off, size = [], 0, os.fstat(fd).st_size
    while off < size:
        try:
            d = os.lseek(fd, off, os.SEEK_DATA)
        except OSError as e:
            out.append(errno.errorcode[e.errno])
            break
        h = os.lseek(fd, d, os.SEEK_HOLE)
        out.append((d, h))
        off = h
    return out

def show(what, fd, holes=True):
    st = os.fstat(fd)
    print(" ", what, "size", st.st_size, "blocks", st.st_blocks, extents(fd) if holes else "")

def nonzero(fd, n):
    return sum(1 for b in os.pread(fd, n, 0) if b)

def seek(fd, off, whence):
    try:
        return os.lseek(fd, off, whence)
    except OSError as e:
        return errno.errorcode[e.errno]

for where in ["/tmp/holes", "/tmp/holes/t"]:
    print(where)
    fd = os.open(where + "/f", os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
    os.write(fd, b"x" * 65536)
    show("written", fd)
    print("  punch", fallocate(fd, PUNCH | KEEP, 4096, 16384))
    show("punched", fd)
    print("  nonzero", nonzero(fd, 65536))
    print("  punch without keep", fallocate(fd, PUNCH, 0, 4096))
    print("  data at 4096", seek(fd, 4096, os.SEEK_DATA), "hole at 0", seek(fd, 0, os.SEEK_HOLE))
    print("  at the end", seek(fd, 65536, os.SEEK_DATA), seek(fd, 65536, os.SEEK_HOLE),
          "negative", seek(fd, -1, os.SEEK_DATA), seek(fd, -1, os.SEEK_HOLE))
    print("  position", os.lseek(fd, 0, os.SEEK_CUR))
    os.pwrite(fd, b"y" * 4096, 8192)
    show("rewritten inside the hole", fd)
    os.ftruncate(fd, 1 << 20)
    show("lengthened", fd)
    os.pwrite(fd, b"z", 900000)
    show("written far away", fd)
    print("  data after the first hole", seek(fd, 65536, os.SEEK_DATA))
    os.ftruncate(fd, 65536)
    print("  reserve past the end", fallocate(fd, 0, 65536, 65536))
    show("reserved", fd, False)
    print("  reserve keeping the size", fallocate(fd, KEEP, 131072, 65536))
    show("reserved beyond", fd, False)
    # How many blocks a zeroed range keeps depends on the host filesystem (APFS frees them)
    print("  zero", fallocate(fd, ZERO | KEEP, 32768, 8192), "nonzero", nonzero(fd, 65536))
    os.close(fd)
    os.unlink(where + "/f")

# tmpfs counts the pages files take: holes are free, punching gives pages back and writing into
# a hole needs them again
t = "/tmp/holes/t"

def free():
    s = os.statvfs(t)
    return s.f_bfree

def write_all(fd, data, off):
    try:
        return os.pwrite(fd, data, off)
    except OSError as e:
        return errno.errorcode[e.errno]

print("tmpfs pages", os.statvfs(t).f_blocks, "free", free())
fd = os.open(t + "/sparse", os.O_RDWR | os.O_CREAT, 0o644)
os.ftruncate(fd, 10 << 20)
print("sparse 10 MB, free", free())
os.close(fd)
fd = os.open(t + "/big", os.O_RDWR | os.O_CREAT, 0o644)
print("fill", write_all(fd, b"a" * (300 << 10), 0), "free", free())
print("more", write_all(fd, b"b", 300 << 10))
print("rewrite inside", write_all(fd, b"c" * 8192, 4096))
print("punch", fallocate(fd, PUNCH | KEEP, 65536, 32768), "free", free())
print("write into the hole", write_all(fd, b"d" * 40960, 61440), "free", free())
print("punch again", fallocate(fd, PUNCH | KEEP, 0, 16384), "free", free())
# At the end of the file: APFS (the store on a macOS host) reserves everything from the end to
# the range, gaps included
print("reserve too much", fallocate(fd, KEEP, 256 << 10, 32768), "free", free())
print("reserve what fits", fallocate(fd, KEEP, 256 << 10, 16384), "free", free())
print("size", os.fstat(fd).st_size, "blocks", os.fstat(fd).st_blocks)
os.close(fd)
os.unlink(t + "/big")
os.unlink(t + "/sparse")
print("emptied, free", free())
# Space reserved before the instance fills up is the file's: writing it needs nothing more
fd = os.open(t + "/reserved", os.O_RDWR | os.O_CREAT, 0o644)
print("reserve", fallocate(fd, 0, 0, 65536), "free", free())
filler = os.open(t + "/filler", os.O_RDWR | os.O_CREAT, 0o644)
print("fill", write_all(filler, b"f" * (256 << 10), 0), "free", free())
print("write the reserved range", write_all(fd, b"r" * 65536, 0), "free", free())
os.close(fd)
os.close(filler)
os.unlink(t + "/reserved")
os.unlink(t + "/filler")

# MADV_REMOVE frees what a shared mapping holds: in a file the range becomes a hole
fd = os.open("/tmp/holes/m", os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(fd, b"m" * 65536)
m = mmap.mmap(fd, 65536, mmap.MAP_SHARED)
addr = ctypes.addressof(ctypes.c_char.from_buffer(m))
libc.madvise.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
print("remove", libc.madvise(addr + 8192, 16384, 9), "reads", m[8192:8195], m[24576:24579])
show("removed", fd)
del addr
m.close()
os.close(fd)
os.unlink("/tmp/holes/m")
PY
umount /tmp/holes/t
rm -rf holes

# Netlink sockets: the generic SOL_SOCKET options, SOL_NETLINK flags and memberships, autobind
# on connect and send, credentials and pktinfo on what is received; socketpair of inet types;
# timeouts by the new option numbers; the peer reported after connecting to the wildcard address
# or with an IPv4 address on an IPv6 socket; unprivileged rtnetlink changes
python3 - <<'PY'
import ctypes, errno, os, socket, struct, sys, time
SOL_NETLINK = 270

def err(f, *a):
    try:
        r = f(*a)
        return "ok" if r is None else r
    except OSError as e:
        return errno.errorcode[e.errno]

def nl(ty=socket.SOCK_RAW):
    return socket.socket(socket.AF_NETLINK, ty, 0)

def getlink(s):
    hdr = struct.pack("=IHHII", 32, 18, 0x301, 1, 0)
    s.send(hdr + bytes(16))

s = nl()
print("unbound", s.getsockname())
s.connect((0, 0))
print("connect binds", s.getsockname() == (os.getpid(), 0))
s.close()
s = nl()
getlink(s)
print("send binds", s.getsockname() == (os.getpid(), 0))
s.close()
s = nl(socket.SOCK_DGRAM)
# The default receive buffer and the cap on SO_RCVBUF come from net.core.rmem_default and rmem_max
# (212992 unless changed; CI's runner sets them to 1 MiB), so only their bounds are compared
for name, opt in [("type", 3), ("protocol", 38), ("domain", 39), ("sndbuf", 7),
                  ("passcred", 16), ("error", 4), ("acceptconn", 30), ("keepalive", 9),
                  ("rcvlowat", 18), ("sndlowat", 19), ("priority", 12)]:
    print(name, err(s.getsockopt, socket.SOL_SOCKET, opt))
print("rcvbuf at least 212992", s.getsockopt(socket.SOL_SOCKET, 8) >= 212992)
print("peercred", struct.unpack("3i", s.getsockopt(socket.SOL_SOCKET, 17, 12)))
print("sndtimeo", s.getsockopt(socket.SOL_SOCKET, 21, 16))
for opt, val in [(7, 1000), (8, 100000), (8, 1 << 30), (18, 0), (9, 1), (12, 7), (2, 1)]:
    s.setsockopt(socket.SOL_SOCKET, opt, val)
    got = s.getsockopt(socket.SOL_SOCKET, opt)
    print("set", opt, val, got if val < 1 << 30 else f"capped {212992 * 2 <= got < val}")
print("short", err(s.setsockopt, socket.SOL_SOCKET, 7, b"\1"))
print("read-only", err(s.setsockopt, socket.SOL_SOCKET, 3, 1))
s.setsockopt(socket.SOL_SOCKET, 13, struct.pack("ii", 1, 30))
print("linger", struct.unpack("ii", s.getsockopt(socket.SOL_SOCKET, 13, 8)))
s.setsockopt(socket.SOL_SOCKET, 13, struct.pack("ii", 0, 5))
print("linger off", struct.unpack("ii", s.getsockopt(socket.SOL_SOCKET, 13, 8)))
s.setsockopt(socket.SOL_SOCKET, 67, struct.pack("qq", 3, 500000))
print("sndtimeo new", struct.unpack("qq", s.getsockopt(socket.SOL_SOCKET, 21, 16)))
print("other level", err(s.setsockopt, socket.SOL_IP, 1, 1), err(s.getsockopt, socket.SOL_IP, 1))
print("memberships before", s.getsockopt(SOL_NETLINK, 9, 8))
for g in [1, 3, 33]:
    s.setsockopt(SOL_NETLINK, 1, g)
print("memberships", s.getsockopt(SOL_NETLINK, 9, 8), s.getsockopt(SOL_NETLINK, 9, 4))
s.setsockopt(SOL_NETLINK, 2, 3)
print("dropped", s.getsockopt(SOL_NETLINK, 9, 8))
print("groups 0 and 1000", err(s.setsockopt, SOL_NETLINK, 1, 0), err(s.setsockopt, SOL_NETLINK, 1, 1000))
s.bind((0, 0x30))
print("bound", s.getsockname()[1], s.getsockopt(SOL_NETLINK, 9, 8))
print("rebind elsewhere", err(s.bind, (s.getsockname()[0] + 1, 0)))
for opt in [3, 4, 5, 10, 11, 12]:
    s.setsockopt(SOL_NETLINK, opt, 1)
    print("flag", opt, s.getsockopt(SOL_NETLINK, opt), err(s.getsockopt, SOL_NETLINK, opt, 2))
s.setsockopt(SOL_NETLINK, 5, 0)
print("flag off", s.getsockopt(SOL_NETLINK, 5))
s.setsockopt(socket.SOL_SOCKET, 16, 1)
getlink(s)
data, anc, flags, _ = s.recvmsg(65536, socket.CMSG_SPACE(4) + socket.CMSG_SPACE(12))
print("ancillary", [(l, t, d) for l, t, d in anc], flags)
data, anc, flags, _ = s.recvmsg(65536, socket.CMSG_SPACE(4) + 20)
print("short buffer", [(l, t, d) for l, t, d in anc], flags)
s.close()

# Unprivileged: rtnetlink changes, port and group destinations, privileged options and groups
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    os.setresgid(65534, 65534, 65534)
    os.setresuid(65534, 65534, 65534)
    s = nl()
    print("nobody connect to a port", err(s.connect, (5, 0)), "join", err(s.setsockopt, SOL_NETLINK, 1, 1),
          "mroute group", err(s.setsockopt, SOL_NETLINK, 1, 30))
    print("nobody debug", err(s.setsockopt, socket.SOL_SOCKET, 1, 1),
          "priority", err(s.setsockopt, socket.SOL_SOCKET, 12, 7),
          "force", err(s.setsockopt, socket.SOL_SOCKET, 32, 1))
    for ty, what in [(32, "newrule"), (88, "newnsid"), (28, "newneigh")]:
        s.send(struct.pack("=IHHII", 28, ty, 0x5, 7, 0) + bytes(12))
        print("nobody", what, struct.unpack("=i", s.recv(4096)[16:20])[0])
    sys.stdout.flush()
    os._exit(0)
os.waitpid(pid, 0)

print("socketpair", [err(socket.socketpair, socket.AF_INET, t, p) for t, p in
                     [(4, 0), (socket.SOCK_STREAM, 17), (socket.SOCK_DGRAM, 0), (15, 0)]])

# SO_RCVTIMEO_NEW (66) on UDP and AF_UNIX
for fam in [socket.AF_INET, socket.AF_UNIX]:
    u = socket.socket(fam, socket.SOCK_DGRAM)
    if fam == socket.AF_INET:
        u.bind(("127.0.0.1", 0))
    else:
        u.bind("\0compat6-timeo")
    u.setsockopt(socket.SOL_SOCKET, 66, struct.pack("qq", 0, 200000))
    t = time.monotonic()
    r = err(u.recv, 10)
    print("rcvtimeo new", fam, r, round(time.monotonic() - t, 1),
          struct.unpack("qq", u.getsockopt(socket.SOL_SOCKET, 66, 16)))
    u.close()

# The peer after connecting
libc = ctypes.CDLL(None, use_errno=True)
short = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
r = libc.connect(short.fileno(), struct.pack("=H", socket.AF_INET) + b"\0\0", 4)
print("short address", r, errno.errorcode.get(ctypes.get_errno()))
srv = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
srv.bind(("127.0.0.1", 0))
port = srv.getsockname()[1]
u6 = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
sin = struct.pack("!HH4s8x", 0, port, bytes([127, 0, 0, 1]))
sin = struct.pack("=H", socket.AF_INET) + sin[2:]
def peer(s):
    a = s.getpeername()
    return (a[0], a[1] == port) + tuple(a[2:])

print("v4 on v6", libc.connect(u6.fileno(), sin, 16), peer(u6))
u4 = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
u4.connect(("0.0.0.0", port))
print("wildcard", peer(u4))
# With an IPv4-mapped local address the loopback is IPv4-mapped too
u6.connect(("::", port))
print("v6 wildcard, mapped local", peer(u6))
u6.connect(("::ffff:0.0.0.0", port))
print("mapped wildcard", peer(u6))
u6 = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
u6.connect(("::", port))
print("v6 wildcard", peer(u6))
l = socket.socket()
l.bind(("127.0.0.1", 0))
l.listen()
t = socket.socket()
t.connect(("0.0.0.0", l.getsockname()[1]))
print("tcp wildcard", t.getpeername() == ("127.0.0.1", l.getsockname()[1]))
PY

# epoll_ctl's error order (the event first, then both fds, poll support, then epfd being an epoll);
# epoll_wait refusing a buffer outside the address space; ppoll writing revents back when
# interrupted; a pipe's write end without readers being writable with POLLERR. Not compared: which
# of a bad buffer and a bad epfd epoll_wait reports, the buffer in 6.1 and 6.8 and the fd in CI's
# newer kernel (tests/guest/linux61.c checks 6.1's answer)
python3 - <<'PY'
import ctypes, errno, os, select, signal, struct
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long

def sc(*a):
    r = libc.syscall(*[ctypes.c_long(x) for x in a])
    return r if r >= 0 else errno.errorcode[ctypes.get_errno()]

EPOLL_CTL, EPOLL_PWAIT, PPOLL = 21, 22, 73
ep = select.epoll()
ev = ctypes.create_string_buffer(16)
r, w = os.pipe()
f = os.open("/tmp", os.O_RDONLY)
bad = 1 << 50
print("bad event and bad fds", sc(EPOLL_CTL, ep.fileno(), 1, 999, bad), sc(EPOLL_CTL, 999, 1, r, bad))
print("bad target", sc(EPOLL_CTL, ep.fileno(), 1, 999, ctypes.addressof(ev)))
print("not an epoll, unpollable target", sc(EPOLL_CTL, r, 1, f, ctypes.addressof(ev)))
print("not an epoll", sc(EPOLL_CTL, w, 1, r, ctypes.addressof(ev)))
print("delete ignores the event", sc(EPOLL_CTL, ep.fileno(), 2, r, bad))
print("wait, bad buffer", sc(EPOLL_PWAIT, ep.fileno(), bad, 1, 0, 0, 8))

signal.signal(signal.SIGALRM, lambda *a: None)
fds = struct.pack("ihh", r, select.POLLIN, 0x7777)
buf = ctypes.create_string_buffer(fds)
signal.setitimer(signal.ITIMER_REAL, 0.1)
print("ppoll interrupted", sc(PPOLL, ctypes.addressof(buf), 1, 0, 0, 8), struct.unpack("ihh", buf.raw[:8]))
os.close(r)
p = select.poll()
p.register(w, select.POLLOUT)
print("no readers", p.poll(0))
PY

# futex: FUTEX_WAKE_OP, wake counts of 0 or less waking one, FUTEX_CLOCK_REALTIME only with the
# absolute waits, unknown command bits
python3 - <<'PY'
import ctypes, errno, mmap, os, threading, time
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long

def futex(addr, op, val, t=0, addr2=0, val3=0):
    r = libc.syscall(ctypes.c_long(98), ctypes.c_void_p(addr), ctypes.c_int(op), ctypes.c_int(val),
                     ctypes.c_void_p(t), ctypes.c_void_p(addr2), ctypes.c_int(val3))
    return r if r >= 0 else errno.errorcode[ctypes.get_errno()]

words = (ctypes.c_uint32 * 2)()
a, b = ctypes.addressof(words), ctypes.addressof(words) + 4
PRIV = 128

def waiters(addr, n, val=0):
    ts = []
    for _ in range(n):
        t = threading.Thread(target=futex, args=(addr, 0 | PRIV, val), daemon=True)
        t.start()
        ts.append(t)
    time.sleep(0.2)
    return ts

def op(o, oparg, cmp, cmparg, shift=False):
    return (o << 28) | (8 << 28 if shift else 0) | (cmp << 24) | ((oparg & 0xfff) << 12) | (cmparg & 0xfff)

ws = waiters(a, 3)
print("wake 0", futex(a, 1 | PRIV, 0), "wake -1", futex(a, 1 | PRIV, -1), "wake 5", futex(a, 1 | PRIV, 5))
for o, oparg, cmp, cmparg, shift in [(1, 5, 0, 7, False), (0, -3, 4, 7, False), (2, 4, 1, 7, True),
                                     (3, 1, 2, 0, False), (4, -1, 5, 100, False), (1, 1, 3, -8, False)]:
    words[1] = 7
    ws = waiters(a, 1) + waiters(b, 1, 7)
    r = futex(a, 5 | PRIV, 1, 1, b, op(o, oparg, cmp, cmparg, shift))
    print("wake_op", o, cmp, r, words[1])
    futex(a, 1 | PRIV, 10)
    futex(b, 1 | PRIV, 10)
words[1] = 7
print("unknown op", futex(a, 5 | PRIV, 1, 1, b, op(6, 1, 0, 0)), words[1])
print("unknown cmp", futex(a, 5 | PRIV, 1, 1, b, op(0, 9, 7, 0)), words[1])
print("unaligned", futex(a, 5 | PRIV, 1, 1, b + 1, 0))
libc.mmap.restype = ctypes.c_void_p
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
ro = libc.mmap(None, 4096, 1, 0x22, -1, 0)
print("read-only", futex(a, 5 | PRIV, 1, 1, ro, 0))
ts = (ctypes.c_long * 2)(0, 1000)
print("realtime", [futex(a, c | 256 | PRIV, 0, ctypes.addressof(ts) if c in (0, 9) else 0, 0, -1)
                   for c in (0, 1, 3, 9, 10)])
print("realtime, bad timeout", futex(a, 0 | 256 | PRIV, 0, 8))
print("high bits", futex(a, 1 | PRIV | 0x200, 1), futex(a, 1 | 0x10000, 1))
# A private operation's key needs no mapping, a shared one does
print("unmapped", futex(0, 1 | PRIV, 1), futex(0, 1, 1), futex(0, 0 | PRIV, 0), futex(1 << 50, 1 | PRIV, 1),
      futex(0, 5 | PRIV, 1, 1, a, 0))
PY

# AF_UNIX: a connected datagram socket whose peer died; datagram connect to a peer connected
# elsewhere; SO_PEERCRED of listeners and unconnected sockets; re-listen; FIONREAD on seqpacket
# and listeners; recvmsg with a NULL, short or unwritable control buffer, or a bad data buffer;
# generic options
python3 - <<'PY'
import array, ctypes, errno, fcntl, os, socket, struct, termios

def err(f, *a):
    try:
        r = f(*a)
        return "ok" if r is None else r
    except OSError as e:
        return errno.errorcode[e.errno]

a = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
a.bind("\0compat6-a")
b = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
b.connect("\0compat6-a")
a.close()
print("dead peer", err(b.send, b"x"), err(b.send, b"x"), err(b.getpeername))
c = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
c.bind("\0compat6-c")
d = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
d.bind("\0compat6-d")
e = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
e.bind("\0compat6-e")
c.connect("\0compat6-d")
print("connect to a peer connected elsewhere", err(e.connect, "\0compat6-c"),
      "to its own peer", err(d.connect, "\0compat6-c"))

l = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
print("unconnected peercred", struct.unpack("3i", l.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)))
l.bind("\0compat6-l")
l.listen(1)
pc = struct.unpack("3i", l.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
print("listener peercred", pc == (os.getpid(), os.geteuid(), os.getegid()))
print("listener FIONREAD", err(fcntl.ioctl, l, termios.FIONREAD, b"\0" * 4))
l.listen(5)

p, q = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
p.send(b"12345")
p.send(b"678")
print("seqpacket FIONREAD", struct.unpack("i", fcntl.ioctl(q, termios.FIONREAD, b"\0" * 4))[0])

# recvmsg on raw msghdrs
libc = ctypes.CDLL(None, use_errno=True)
class iovec(ctypes.Structure):
    _fields_ = [("base", ctypes.c_void_p), ("len", ctypes.c_size_t)]
class msghdr(ctypes.Structure):
    _fields_ = [("name", ctypes.c_void_p), ("namelen", ctypes.c_uint32), ("iov", ctypes.c_void_p),
                ("iovlen", ctypes.c_size_t), ("control", ctypes.c_void_p),
                ("controllen", ctypes.c_size_t), ("flags", ctypes.c_int)]

def recvmsg(s, data=64, control=None, controllen=0, bad_data=False):
    buf = ctypes.create_string_buffer(data)
    iov = iovec(1 << 50 if bad_data else ctypes.addressof(buf), data)
    m = msghdr(None, 0, ctypes.addressof(iov), 1, control, controllen, 0)
    r = libc.recvmsg(s.fileno(), ctypes.byref(m), 0)
    if r < 0:
        return errno.errorcode[ctypes.get_errno()]
    return r, buf.raw[:r], m.controllen, m.flags

x, y = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
y.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
r, w = os.pipe()
fds_before = len(os.listdir("/proc/self/fd"))
def send_fd():
    x.sendmsg([b"data"], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [r]))])
send_fd()
print("bad data buffer", recvmsg(y, bad_data=True), "then", recvmsg(y, controllen=0)[:2])
print("fds", len(os.listdir("/proc/self/fd")) - fds_before)
send_fd()
print("NULL control", recvmsg(y, control=None, controllen=64))
print("fds", len(os.listdir("/proc/self/fd")) - fds_before)
send_fd()
print("unwritable control", recvmsg(y, control=1 << 50, controllen=64))
print("fds", len(os.listdir("/proc/self/fd")) - fds_before)
for n in [16, 20, 27, 32]:
    send_fd()
    cb = ctypes.create_string_buffer(n)
    out = recvmsg(y, control=ctypes.addressof(cb), controllen=n)
    raw = bytearray(cb.raw[:out[2]])
    if len(raw) >= 20 and struct.unpack("i", raw[16:20])[0] == os.getpid():
        raw[16:20] = b"PPPP"
    print("control", n, out, raw.hex(), "fds", len(os.listdir("/proc/self/fd")) - fds_before)

o = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for opt, val in [(socket.SO_KEEPALIVE, 1), (socket.SO_RCVLOWAT, 10), (socket.SO_SNDBUF, 4096),
                 (socket.SO_RCVBUF, 0), (socket.SO_BROADCAST, 1), (socket.SO_PRIORITY, 3)]:
    o.setsockopt(socket.SOL_SOCKET, opt, val)
    print("unix", opt, o.getsockopt(socket.SOL_SOCKET, opt))
PY

# renameat2 with RENAME_NOREPLACE onto . or ..; renames in procfs and sysfs (to itself, onto
# dots, elsewhere, by root and by nobody); SEEK_END, SEEK_DATA and SEEK_HOLE on directories of
# the store, procfs, sysfs and tmpfs; the inotify event utimensat reports for each time
mkdir -p /tmp/holes/t
mount -t tmpfs tmpfs /tmp/holes/t
python3 - <<'PY'
import ctypes, errno, os, struct, sys
libc = ctypes.CDLL(None, use_errno=True)

def rename2(a, b, flags=0):
    r = libc.renameat2(-100, a.encode(), -100, b.encode(), flags)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]

os.makedirs("/tmp/holes/r", exist_ok=True)
open("/tmp/holes/r/f", "w").close()
print("noreplace onto dots", rename2("/tmp/holes/r/f", "/tmp/holes/r/.", 1),
      rename2("/tmp/holes/r/f", "/tmp/holes/r/..", 1), "without", rename2("/tmp/holes/r/f", "/tmp/holes/r/.."),
      "from dots", rename2("/tmp/holes/r/.", "/tmp/holes/r/g", 1))
cpu = "/sys/devices/system/cpu/online"
for who in ["root", "nobody"]:
    if who == "nobody":
        sys.stdout.flush()
        pid = os.fork()
        if pid:
            os.waitpid(pid, 0)
            break
        os.setresgid(65534, 65534, 65534)
        os.setresuid(65534, 65534, 65534)
    print(who, "sysfs to itself", rename2(cpu, cpu), rename2("/sys/devices", "/sys/devices"),
          "onto dots", rename2(cpu, "/sys/."), "elsewhere", rename2(cpu, "/sys/devices/system/cpu/x"),
          "noreplace", rename2(cpu, "/sys/devices/system/cpu/possible", 1),
          "missing", rename2("/sys/nothing", "/sys/x"),
          "proc", rename2("/proc/self/status", "/proc/self/status"), rename2("/proc/self/status", "/proc/self/x"))
    if who == "nobody":
        sys.stdout.flush()
        os._exit(0)

def seek(path, off, whence):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        return os.lseek(fd, off, whence)
    except OSError as e:
        return errno.errorcode[e.errno]
    finally:
        os.close(fd)

for d in ["/tmp/holes/r", "/proc", "/proc/self", "/sys", "/tmp/holes/t", "/dev/pts"]:
    print("seek", d, [seek(d, off, w) for off, w in [(0, os.SEEK_END), (-5, os.SEEK_END),
          (5, os.SEEK_END), (3, os.SEEK_DATA), (3, os.SEEK_HOLE)]])
fd = os.open("/tmp/holes/r", os.O_RDONLY | os.O_DIRECTORY)
os.lseek(fd, 0, os.SEEK_END)
print("listing at the end", os.listdir(fd))
os.close(fd)

IN_NONBLOCK = 0o4000
ino = libc.inotify_init1(IN_NONBLOCK)
libc.inotify_add_watch(ino, b"/tmp/holes/r/f", 0xfff)
class ts(ctypes.Structure):
    _fields_ = [("s", ctypes.c_long), ("ns", ctypes.c_long)]
UTIME_NOW, UTIME_OMIT = (1 << 30) - 1, (1 << 30) - 2
for a, m in [(UTIME_NOW, UTIME_OMIT), (UTIME_OMIT, UTIME_NOW), (UTIME_NOW, UTIME_NOW), (UTIME_OMIT, UTIME_OMIT)]:
    t = (ts * 2)(ts(0, a), ts(0, m))
    libc.utimensat(-100, b"/tmp/holes/r/f", t, 0)
    try:
        ev = os.read(ino, 4096)
        print("utimensat", hex(struct.unpack_from("iIII", ev)[1]))
    except BlockingIOError:
        print("utimensat no event")
PY
umount /tmp/holes/t
rm -rf /tmp/holes

# A shm segment marked for removal goes with its last attachment however it ends (munmap, exit,
# execve), so attaching it again is EINVAL and /proc/sysvipc/shm no longer lists it; message
# queues created without attributes take the defaults capped by msg_max and msgsize_max
python3 - <<'PY'
import ctypes, errno, os, sys
libc = ctypes.CDLL(None, use_errno=True)
libc.shmat.restype = ctypes.c_void_p
libc.shmat.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
IPC_PRIVATE, IPC_RMID = 0, 0

def listed(i):
    return any(l.split()[1] == str(i) for l in open("/proc/sysvipc/shm").readlines()[1:])

def attach(i):
    p = libc.shmat(i, None, 0)
    return errno.errorcode[ctypes.get_errno()] if p in (None, 2**64 - 1) else p

for how in ["munmap", "exit", "execve"]:
    i = libc.shmget(IPC_PRIVATE, 4096, 0o600)
    if how == "munmap":
        p = attach(i)
        libc.shmctl(i, IPC_RMID, None)
        print(how, "before", listed(i))
        libc.munmap(p, 4096)
    else:
        sys.stdout.flush()
        pid = os.fork()
        if pid == 0:
            attach(i)
            libc.shmctl(i, IPC_RMID, None)
            if how == "execve":
                os.execv("/bin/sh", ["sh", "-c", "awk -v i=%d 'NR > 1 && $2 == i' /proc/sysvipc/shm | wc -l" % i])
            os._exit(0)
        os.waitpid(pid, 0)
    print(how, "after", listed(i), attach(i))

class attr(ctypes.Structure):
    _fields_ = [("flags", ctypes.c_long), ("maxmsg", ctypes.c_long), ("msgsize", ctypes.c_long),
                ("curmsgs", ctypes.c_long), ("pad", ctypes.c_long * 4)]
def limit(name, v=None):
    path = "/proc/sys/fs/mqueue/" + name
    old = open(path).read().strip()
    if v is not None:
        open(path, "w").write(str(v))
    return old
old = (limit("msg_max", 4), limit("msgsize_max", 1024))
q = libc.mq_open(b"/compat6", os.O_CREAT | os.O_RDWR, 0o600, None)
a = attr()
libc.mq_getattr(q, ctypes.byref(a))
print("mq defaults capped", a.maxmsg, a.msgsize)
libc.mq_unlink(b"/compat6")
limit("msg_max", old[0])
limit("msgsize_max", old[1])
PY

# Echoes stop growing the output once it is full (a master that never reads while the slave
# reads its input); MADV_DONTNEED over the vDSO brings back its code rather than zeros
python3 - <<'PY'
import ctypes, os, sys, termios, time
m, s = os.openpty()
a = termios.tcgetattr(s)
a[3] &= ~termios.ICANON
a[3] |= termios.ECHO
a[6][termios.VMIN], a[6][termios.VTIME] = 1, 0
termios.tcsetattr(s, termios.TCSANOW, a)
N = 200000
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    os.close(m)
    got = 0
    while got < N:
        got += len(os.read(s, 65536))
    os._exit(0)
os.close(s)
sent = 0
while sent < N:
    sent += os.write(m, b"a" * min(4096, N - sent))
os.waitpid(pid, 0)
os.set_blocking(m, False)
echoed = 0
try:
    while True:
        echoed += len(os.read(m, 65536))
except (BlockingIOError, OSError):
    pass
print("echo bounded", 1000 < echoed <= 66000)

libc = ctypes.CDLL(None, use_errno=True)
libc.madvise.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
vdso = [l for l in open("/proc/self/maps") if l.rstrip().endswith("[vdso]")][0]
start, end = (int(x, 16) for x in vdso.split()[0].split("-"))
print("dontneed vdso", libc.madvise(start, end - start, 4))
t = time.clock_gettime(time.CLOCK_MONOTONIC)
print("clock after", t > 0, ctypes.string_at(start, 4))
PY

# procfs: the fields of each /proc/stat line (softirq has the total and 10 types) and processes
# counting threads; Cpus_allowed_list following the affinity; the modes of /proc/<pid>/fd links;
# procfs and devpts block sizes; loading a device program needing CAP_NET_ADMIN or CAP_SYS_ADMIN
# besides CAP_BPF. Credentials: PRIO_USER's user ID in the caller's user namespace; seccomp
# notifications reporting the pid as the agent's namespace sees it
python3 - <<'PY'
import array, ctypes, errno, os, socket, struct, sys, threading
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long

def forks():
    return int([l for l in open("/proc/stat") if l.startswith("processes")][0].split()[1])

before = forks()
ts = [threading.Thread(target=lambda: None) for _ in range(3)]
for t in ts:
    t.start()
for t in ts:
    t.join()
pid = os.fork()
if pid == 0:
    os._exit(0)
os.waitpid(pid, 0)
print("forks counted", forks() - before)
# intr has a count for every interrupt number, which differ
fields = {}
for l in open("/proc/stat"):
    w = l.split()
    if w[0] != "intr":
        fields.setdefault("cpuN" if w[0][3:].isdigit() else w[0], set()).add(len(w))
print("stat fields", sorted((k, sorted(v)) for k, v in fields.items()))

def allowed(path="/proc/self/status"):
    return [l.split()[1] for l in open(path) if l.startswith("Cpus_allowed_list")][0]
cpus = os.sched_getaffinity(0)
os.sched_setaffinity(0, {0})
print("allowed", allowed(), allowed("/proc/self/task/%d/status" % os.getpid()))
os.sched_setaffinity(0, cpus)

for flags in [os.O_RDONLY, os.O_WRONLY, os.O_RDWR, os.O_PATH]:
    fd = os.open("/tmp", flags | os.O_DIRECTORY) if flags in (os.O_RDONLY, os.O_PATH) else os.open("/tmp/holes-fdmode", flags | os.O_CREAT, 0o600)
    print("fd link mode", oct(os.lstat("/proc/self/fd/%d" % fd).st_mode & 0o7777))
    os.close(fd)
os.unlink("/tmp/holes-fdmode")
print("blksize", [os.stat(p).st_blksize for p in ["/proc/self/status", "/proc", "/proc/sys/kernel", "/dev/pts", "/sys/devices", "/tmp"]])

def bpf_load():
    insns = bytes.fromhex("b700000001000000") + bytes.fromhex("9500000000000000")
    ibuf = ctypes.create_string_buffer(insns)
    lic = ctypes.create_string_buffer(b"GPL")
    attr = ctypes.create_string_buffer(128)
    struct.pack_into("IIQQ", attr, 0, 15, 2, ctypes.addressof(ibuf), ctypes.addressof(lic))
    r = libc.syscall(ctypes.c_long(280), ctypes.c_long(5), attr, ctypes.c_long(128))
    return "ok" if r >= 0 else errno.errorcode[ctypes.get_errno()]

def capset(effective):
    hdr = struct.pack("Ii", 0x20080522, 0)
    data = struct.pack("6I", effective & 0xffffffff, 0xffffffff, 0, effective >> 32, 0xff, 0)
    return libc.capset(hdr, data)

print("bpf device program as root", bpf_load())
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    capset(1 << 39)
    print("bpf with CAP_BPF only", bpf_load())
    capset((1 << 39) | (1 << 12))
    print("bpf with CAP_BPF and CAP_NET_ADMIN", bpf_load())
    sys.stdout.flush()
    os._exit(0)
os.waitpid(pid, 0)

def prio(which, who):
    ctypes.set_errno(0)
    r = libc.getpriority(which, who)
    e = ctypes.get_errno()
    return errno.errorcode[e] if r == -1 and e else r

CLONE_NEWUSER, CLONE_NEWPID = 0x10000000, 0x20000000
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    libc.unshare(CLONE_NEWUSER)
    open("/proc/self/uid_map", "w").write("1000 0 1")
    print("prio user 1000", prio(2, 1000) == prio(2, 0), "unmapped", prio(2, 5))
    sys.stdout.flush()
    os._exit(0)
os.waitpid(pid, 0)

# A notification agent at pid 1 of a new pid namespace and the notified process at pid 2
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    libc.unshare(CLONE_NEWPID)
    a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    agent = os.fork()
    if agent == 0:
        target = os.fork()
        if target == 0:
            filt = struct.pack("HBBI", 0x20, 0, 0, 0) + struct.pack("HBBI", 0x15, 0, 1, 155) + \
                struct.pack("HBBI", 0x06, 0, 0, 0x7fc00000) + struct.pack("HBBI", 0x06, 0, 0, 0x7fff0000)
            fbuf = ctypes.create_string_buffer(filt)
            prog = struct.pack("HxxxxxxQ", 4, ctypes.addressof(fbuf))
            libc.prctl(38, 1, 0, 0, 0)
            fd = libc.syscall(ctypes.c_long(277), ctypes.c_long(1), ctypes.c_long(8), ctypes.c_char_p(prog))
            b.sendmsg([b"l"], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [fd]))])
            libc.getpgid(0)
            os._exit(0)
        msg, anc, _, _ = a.recvmsg(1, socket.CMSG_SPACE(4))
        listener = array.array("i", anc[0][2])[0]
        notif = ctypes.create_string_buffer(80)
        print("ioctl recv", libc.ioctl(listener, ctypes.c_ulong(0xc0502100), notif))
        nid, npid = struct.unpack_from("QI", notif)
        print("notified pid", npid, "agent pid", os.getpid(), "target", target)
        resp = ctypes.create_string_buffer(struct.pack("QqiI", nid, 0, 0, 1))
        libc.ioctl(listener, ctypes.c_ulong(0xc0182101), resp)
        os.waitpid(target, 0)
        sys.stdout.flush()
        os._exit(0)
    os.waitpid(agent, 0)
    os._exit(0)
os.waitpid(pid, 0)
PY
