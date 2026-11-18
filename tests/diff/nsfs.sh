# docker-run-options: --cap-add SYS_ADMIN --cap-add NET_ADMIN --security-opt apparmor=unconfined
# Namespace files (nsfs): open namespace files, and ones bind-mounted elsewhere, keep the
# namespace alive, and it can still be entered with setns after all processes in it have exited;
# the NS_GET_* ioctls; mount namespace files can only be bound into namespaces older than them,
# and copying a mount namespace leaves them out; the persistent namespaces of busybox's
# unshare and nsenter. The native side is a docker container with CAP_SYS_ADMIN. Namespace IDs
# differ between the two sides, so only whether they are the same or different is compared
cd /tmp
rm -rf nsfs
mkdir -p nsfs/run nsfs/m

python3 - <<'PY'
import ctypes, errno, fcntl, functools, os, socket

print = functools.partial(print, flush=True)
libc = ctypes.CDLL(None, use_errno=True)
libc.ioctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_void_p]
CLONE_NEWNS, CLONE_NEWCGROUP, CLONE_NEWUTS, CLONE_NEWIPC = 0x20000, 0x2000000, 0x4000000, 0x8000000
CLONE_NEWUSER, CLONE_NEWPID, CLONE_NEWNET = 0x10000000, 0x20000000, 0x40000000
MS_BIND, MS_REC, MS_PRIVATE = 0x1000, 0x4000, 1 << 18
MNT_DETACH = 2
NS_GET_USERNS, NS_GET_PARENT, NS_GET_NSTYPE, NS_GET_OWNER_UID = 0xb701, 0xb702, 0xb703, 0xb704
RUN = "/tmp/nsfs/run"


def call(f, *args):
    r = f(*args)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


def ioctl(fd, req, arg=None):
    r = libc.ioctl(fd, req, arg)
    return r if r >= 0 else errno.errorcode[ctypes.get_errno()]


def link(fd):
    return os.readlink("/proc/self/fd/%d" % fd)


def rel(fd, req):
    """The kind of namespace NS_GET_USERNS or NS_GET_PARENT gives, or the error."""
    r = ioctl(fd, req)
    return r if isinstance(r, str) else link(r).split(":")[0]


def same(a, b):
    return "same" if a == b else "different"


def fork(f):
    """Run f in a child process and return its exit code. An exception also ends the child, which
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


def mounted(path):
    with open("/proc/self/mountinfo") as f:
        return [l.split() for l in f if l.split()[4] == path]


libc.mount(None, b"/", None, MS_REC | MS_PRIVATE, None)

# The child creates new network, UTS and IPC namespaces, bind-mounts them elsewhere, and exits
def holder():
    assert libc.unshare(CLONE_NEWNET | CLONE_NEWUTS | CLONE_NEWIPC) == 0
    socket.sethostname("pinned")
    # A shared memory segment that is never removed: it exists as long as the IPC namespace does
    assert libc.shmget(0x5157, 4096, 0o1600) >= 0
    for k in ("net", "uts", "ipc"):
        open("%s/%s" % (RUN, k), "w").close()
        assert libc.mount(("/proc/self/ns/" + k).encode(), ("%s/%s" % (RUN, k)).encode(), None, MS_BIND, None) == 0

print("holder", fork(holder))
own = os.stat("/proc/self/ns/net")
for k in ("net", "uts", "ipc"):
    st = os.stat("%s/%s" % (RUN, k))
    print(k, "dev", same(st.st_dev, own.st_dev), "ino", same(st.st_ino, os.stat("/proc/self/ns/" + k).st_ino),
          "mode", oct(st.st_mode), "nlink", st.st_nlink)
net_dev = os.stat(RUN + "/net").st_dev
for m in mounted(RUN + "/net"):
    # The columns other than the ID; the device number of nsfs differs between kernel versions
    # (0:4 on 6.1 and 6.8, 0:5 on newer ones), so only whether it is the one stat gives for this
    # file is compared
    dev = "%d:%d" % (os.major(net_dev), os.minor(net_dev))
    print("mountinfo dev is the file's", m[2] == dev, m[3].split(":")[0], m[5], m[6:])

def enter():
    fd = os.open(RUN + "/net", os.O_RDONLY)
    # Opened through the bind mount, its path is the mount point
    print("open link", link(fd), "fdinfo", open("/proc/self/fdinfo/%d" % fd).read().split()[2:4])
    print("setns net", call(libc.setns, fd, CLONE_NEWNET))
    print("now in it", same(os.stat("/proc/self/ns/net").st_ino, os.fstat(fd).st_ino))
    print("interfaces", socket.if_nameindex())
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", 0))
        print("bind lo ok")
    except OSError as e:
        print("bind lo", errno.errorcode[e.errno])
    print("setns uts", call(libc.setns, os.open(RUN + "/uts", os.O_RDONLY), CLONE_NEWUTS), socket.gethostname())
    print("setns ipc", call(libc.setns, os.open(RUN + "/ipc", os.O_RDONLY), CLONE_NEWIPC),
          "shm", libc.shmget(0x5157, 0, 0) >= 0)
    print("wrong type", call(libc.setns, os.open(RUN + "/uts", os.O_RDONLY), CLONE_NEWNET))

print("enter", fork(enter))
print("shm outside", libc.shmget(0x5157, 0, 0) >= 0)

# After unmounting, the file is the original regular file
held = os.open(RUN + "/net", os.O_RDONLY)
print("umount", call(libc.umount2, (RUN + "/net").encode(), MNT_DETACH), "mounted", len(mounted(RUN + "/net")))
fd = os.open(RUN + "/net", os.O_RDONLY)
print("plain file", call(libc.setns, fd, 0))
os.close(fd)
# One opened before unmounting still refers to that namespace
print("held", fork(lambda: print("setns held", call(libc.setns, held, CLONE_NEWNET), socket.if_nameindex())))
os.close(held)
for k in ("uts", "ipc"):
    libc.umount2(("%s/%s" % (RUN, k)).encode(), MNT_DETACH)

# Only through an open fd: setns through it after the child has exited
r1, w1 = os.pipe()
r2, w2 = os.pipe()
pid = os.fork()
if pid == 0:
    libc.unshare(CLONE_NEWNET | CLONE_NEWNS)
    libc.mount(b"tmpfs", b"/tmp/nsfs/m", b"tmpfs", 0, None)
    open("/tmp/nsfs/m/inside", "w").close()
    os.write(w1, b"x")
    os.read(r2, 1)
    os._exit(0)
os.read(r1, 1)
net = os.open("/proc/%d/ns/net" % pid, os.O_RDONLY)
mnt = os.open("/proc/%d/ns/mnt" % pid, os.O_RDONLY)
print("fd link", link(net).split(":")[0], same(int(link(net).split("[")[1][:-1]), os.fstat(net).st_ino))
os.write(w2, b"x")
os.waitpid(pid, 0)
def enter_fds():
    print("setns net fd", call(libc.setns, net, CLONE_NEWNET), socket.if_nameindex())
    print("setns mnt fd", call(libc.setns, mnt, CLONE_NEWNS), os.listdir("/tmp/nsfs/m"), os.getcwd())
print("enter fds", fork(enter_fds))
print("outside", os.listdir("/tmp/nsfs/m"))

# A mount namespace file can only be bound into a namespace older than it (Linux's mnt_ns_loop)
open(RUN + "/mnt", "w").close()
print("bind own mnt", call(libc.mount, b"/proc/self/ns/mnt", (RUN + "/mnt").encode(), None, MS_BIND, None))
print("bind newer mnt", call(libc.mount, b"/proc/self/fd/%d" % mnt, (RUN + "/mnt").encode(), None, MS_BIND, None))
open(RUN + "/net2", "w").close()
print("bind net", call(libc.mount, b"/proc/self/fd/%d" % net, (RUN + "/net2").encode(), None, MS_BIND, None))
# Copying a mount namespace leaves out mount namespace files; other ones are copied as usual
def copied():
    libc.unshare(CLONE_NEWNS)
    print("copied mnt", len(mounted(RUN + "/mnt")), "net", len(mounted(RUN + "/net2")))
    # The original one is older than our own: it cannot be bound in
    open(RUN + "/mnt2", "w").close()
    print("bind older mnt", call(libc.mount, b"/proc/%d/ns/mnt" % os.getppid(), (RUN + "/mnt2").encode(), None, MS_BIND, None))
print("copy", fork(copied))
os.close(mnt)
os.close(net)
def enter_bound():
    fd = os.open(RUN + "/mnt", os.O_RDONLY)
    print("setns bound mnt", call(libc.setns, fd, CLONE_NEWNS), os.listdir("/tmp/nsfs/m"))
print("enter bound", fork(enter_bound))
libc.umount2((RUN + "/mnt").encode(), MNT_DETACH)
libc.umount2((RUN + "/net2").encode(), MNT_DETACH)

# NS_GET_*
own = {k: os.open("/proc/self/ns/" + k, os.O_RDONLY) for k in ("cgroup", "ipc", "mnt", "net", "pid", "pid_for_children", "user", "uts")}
for k, fd in own.items():
    print(k, "type", hex(ioctl(fd, NS_GET_NSTYPE)), "userns", rel(fd, NS_GET_USERNS), "parent", rel(fd, NS_GET_PARENT))
print("owner uid of net", ioctl(own["net"], NS_GET_OWNER_UID, ctypes.byref(ctypes.c_uint())))
uid = ctypes.c_uint(1234)
print("owner uid of init", ioctl(own["user"], NS_GET_OWNER_UID, ctypes.byref(uid)), uid.value)
print("unknown", ioctl(own["net"], 0xb7ff), "tcgets", ioctl(own["net"], 0x5401, ctypes.byref(ctypes.create_string_buffer(64))))

def nested():
    # A user namespace created by root, with network and pid namespaces created inside it (an
    # Ubuntu host does not give capabilities inside user namespaces created by ordinary users).
    # Their files are held outside
    r1, w1 = os.pipe()
    r2, w2 = os.pipe()
    pid = os.fork()
    if pid == 0:
        assert libc.unshare(CLONE_NEWUSER) == 0
        with open("/proc/self/uid_map", "w") as f:
            f.write("0 0 1")
        assert libc.unshare(CLONE_NEWNET | CLONE_NEWPID) == 0
        user = os.open("/proc/self/ns/user", os.O_RDONLY)
        u = ioctl(os.open("/proc/self/ns/net", os.O_RDONLY), NS_GET_USERNS)
        # The user namespaces outside one's own cannot be seen
        print("inside: parent", rel(user, NS_GET_PARENT), "userns", rel(user, NS_GET_USERNS),
              "net's userns", same(os.fstat(u).st_ino, os.fstat(user).st_ino))
        kid = os.fork()
        if kid == 0:
            os.read(r2, 1)
            os._exit(0)
        os.write(w1, b"%d" % kid)
        os.waitpid(kid, 0)
        os._exit(0)
    kid = int(os.read(r1, 16))
    user = os.open("/proc/%d/ns/user" % pid, os.O_RDONLY)
    net = os.open("/proc/%d/ns/net" % pid, os.O_RDONLY)
    u = ioctl(net, NS_GET_USERNS)
    print("net's userns", link(u).split(":")[0], same(os.fstat(u).st_ino, os.fstat(user).st_ino),
          "cloexec", fcntl.fcntl(u, fcntl.F_GETFD), "flags", oct(fcntl.fcntl(u, fcntl.F_GETFL)))
    p = ioctl(user, NS_GET_PARENT)
    print("user's parent", same(os.fstat(p).st_ino, os.stat("/proc/self/ns/user").st_ino),
          "userns", same(os.fstat(ioctl(user, NS_GET_USERNS)).st_ino, os.stat("/proc/self/ns/user").st_ino),
          "parent of that", rel(p, NS_GET_PARENT))
    owner = ctypes.c_uint(1234)
    print("owner uid", ioctl(user, NS_GET_OWNER_UID, ctypes.byref(owner)), owner.value)
    child = os.open("/proc/%d/ns/pid" % kid, os.O_RDONLY)
    pp = ioctl(child, NS_GET_PARENT)
    print("pid parent", link(pp).split(":")[0], same(os.fstat(pp).st_ino, os.stat("/proc/self/ns/pid").st_ino),
          "parent of that", rel(pp, NS_GET_PARENT), "type", hex(ioctl(child, NS_GET_NSTYPE)),
          "userns", same(os.fstat(ioctl(child, NS_GET_USERNS)).st_ino, os.fstat(user).st_ino))
    os.write(w2, b"x")
    os.waitpid(pid, 0)
    # After process 1 exits: it can still be entered with setns, but no processes can be created
    print("setns dead pid", call(libc.setns, child, CLONE_NEWPID))
    try:
        if os.fork() == 0:
            os._exit(0)
        print("fork ok")
    except OSError as e:
        print("fork", errno.errorcode[e.errno])
    # All processes in the user namespace have exited: it can still be entered
    print("setns user", call(libc.setns, user, CLONE_NEWUSER), os.getuid())
print("nested", fork(nested))
PY

# busybox's unshare and nsenter: paths of namespace files
touch nsfs/run/bbnet nsfs/run/bbuts
busybox unshare --net=/tmp/nsfs/run/bbnet --uts=/tmp/nsfs/run/bbuts sh -c 'hostname bb; ip link show | grep -c "^[0-9]"'
# On the native side the hostname is the container's ID (random hex), which may happen to contain
# bb: the whole line must match
echo "outside: $(hostname | grep -cx bb)"
busybox nsenter --net=/tmp/nsfs/run/bbnet --uts=/tmp/nsfs/run/bbuts sh -c 'hostname; ip -o link show | cut -d: -f2'
umount /tmp/nsfs/run/bbnet /tmp/nsfs/run/bbuts
busybox nsenter --net=/tmp/nsfs/run/bbnet true; echo "after umount: $?"

# iproute2's ip netns: add bind-mounts a network namespace created by unshare under
# /var/run/netns (the native side's image has no /var or /run)
[ -e /var/run ] || { mkdir -p /var /run && ln -s ../run /var/run; }
ip netns add tuxport-a
ip netns list
ip netns exec tuxport-a ip -o link show | cut -d: -f2 | cut -d' ' -f2
ip netns exec tuxport-a sh -c 'ip link set lo up && ip -4 -o addr show lo | cut -d" " -f2,7'
ip netns exec tuxport-a ip -o link show lo | grep -o 'state [A-Z]*'
ip -n tuxport-a -o link show | wc -l
ip netns identify $$ | wc -c
ip netns exec tuxport-a sh -c 'ip netns identify $$'
ip netns delete tuxport-a
ip netns list | wc -l
ip netns exec tuxport-a true 2>&1 | sed 's/: .*$//'
