# docker-run-options: --privileged
# What rootless containers (podman run by an ordinary user) need from user namespaces: root of a
# user namespace (ids mapped as podman maps them, 0 to 1000 and 1-1000 to 100000-100999) changing
# the CPU affinity and nice value of its namespace's processes; setting file capabilities, stored
# with the namespace's root as rootid, read back and applied by exec only where that root owns the
# namespace; writing the sysctls of a network namespace it owns, ping_group_range's group IDs
# translated through the reader's and the writer's namespace, and /proc/sys deciding by euid 0
# rather than by capabilities; and SysV IPC capabilities over the IPC namespace it owns
set -e
cd /tmp
rm -rf rootless
mkdir rootless
chmod 777 rootless
cp /bin/busybox rootless/busybox
chown 1000:1000 rootless/busybox

python3 - <<'PY'
import ctypes, errno, os, struct, sys
libc = ctypes.CDLL(None, use_errno=True)
CLONE_NEWUSER, CLONE_NEWNET, CLONE_NEWIPC = 0x10000000, 0x40000000, 0x08000000
D = "/tmp/rootless"

def err():
    return errno.errorcode[ctypes.get_errno()]

def try_(f, *a):
    try:
        r = f(*a)
        return "ok" if r is None else r
    except OSError as e:
        return errno.errorcode[e.errno]

def in_userns(fn, extra=0):
    """Runs fn in a child that is root (kuid 1000) of a new user namespace, after unsharing extra"""
    r, w = os.pipe()
    r2, w2 = os.pipe()
    sys.stdout.flush()
    pid = os.fork()
    if pid == 0:
        os.close(r)
        os.close(w2)
        if libc.unshare(CLONE_NEWUSER) != 0:
            print("unshare", err())
            sys.stdout.flush()
            os._exit(1)
        os.write(w, b"x")
        os.read(r2, 1)
        os.setresgid(0, 0, 0)
        os.setresuid(0, 0, 0)
        if extra and libc.unshare(extra) != 0:
            print("unshare", err())
            sys.stdout.flush()
            os._exit(1)
        try:
            fn()
        except Exception as e:
            print("error", type(e).__name__, e)
        sys.stdout.flush()
        os._exit(0)
    os.close(w)
    os.close(r2)
    os.read(r, 1)
    for m in ["uid_map", "gid_map"]:
        open("/proc/%d/%s" % (pid, m), "w").write("0 1000 1\n1 100000 1000")
    os.write(w2, b"x")
    os.waitpid(pid, 0)

def as_user_1(fn):
    """In the namespace: runs fn in a child that is user 1 (kuid 100000), returns its pid"""
    r, w = os.pipe()
    sys.stdout.flush()
    pid = os.fork()
    if pid == 0:
        os.close(r)
        os.setresgid(1, 1, 1)
        os.setresuid(1, 1, 1)
        fn()
        os.write(w, b"x")
        sys.stdout.flush()
        os.read(os.open("/dev/null", os.O_RDONLY), 1)
        import time
        time.sleep(30)
        os._exit(0)
    os.close(w)
    os.read(r, 1)
    return pid

def write(p, v):
    fd = os.open(p, os.O_WRONLY)
    try:
        os.write(fd, v.encode())
    finally:
        os.close(fd)

def caps(path):
    return [l.split()[1] for l in os.popen(path + " grep CapPrm /proc/self/status")][0]

# Root of the namespace acting on another user's process in it
def sched():
    pid = as_user_1(lambda: None)
    print("affinity of user 1", try_(os.sched_setaffinity, pid, {0}),
          "nice", try_(os.setpriority, os.PRIO_PROCESS, pid, 5), os.getpriority(os.PRIO_PROCESS, pid))
    os.kill(pid, 9)
    os.waitpid(pid, 0)
in_userns(sched)

# File capabilities set inside: v3 with rootid 1000, read back as v2 inside, applied by exec there
V2 = struct.pack("<5I", 0x02000001, 1 << 13, 0, 0, 0)
def fcaps():
    print("set", try_(os.setxattr, D + "/busybox", "security.capability", V2))
    print("read inside", os.getxattr(D + "/busybox", "security.capability").hex())
    print("uid 1 inside runs it with", end=" ")
    sys.stdout.flush()
    pid = os.fork()
    if pid == 0:
        os.setresgid(1, 1, 1)
        os.setresuid(1, 1, 1)
        print(caps(D + "/busybox"))
        sys.stdout.flush()
        os._exit(0)
    os.waitpid(pid, 0)
in_userns(fcaps)
print("read outside", os.getxattr(D + "/busybox", "security.capability").hex())
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    os.setresgid(65534, 65534, 65534)
    os.setresuid(65534, 65534, 65534)
    print("nobody outside runs it with", caps(D + "/busybox"))
    sys.stdout.flush()
    os._exit(0)
os.waitpid(pid, 0)
os.removexattr(D + "/busybox", "security.capability")

# Sysctls of a network namespace the user namespace owns. ping_group_range holds kernel group
# IDs: the parent's and a new namespace's are not mapped inside, nor is 2147483647
PING = "/proc/sys/net/ipv4/ping_group_range"
in_userns(lambda: print("ping_group_range of the parent's namespace", open(PING).read().split()))
def sysctls():
    print("ping_group_range of a new one", open(PING).read().split(),
          "writing unmapped groups", try_(write, PING, "0 2147483647"))
    for path, v in [("net/ipv4/ping_group_range", "0 0"), ("net/ipv6/conf/default/accept_dad", "1"),
                    ("net/ipv4/ip_unprivileged_port_start", "80"), ("kernel/hostname", "x")]:
        p = "/proc/sys/" + path
        try:
            open(p, "w").write(v)
            print(path, "written", open(p).read().strip(), os.stat(p).st_uid, os.stat(p).st_gid)
        except OSError as e:
            print(path, errno.errorcode[e.errno], os.stat(p).st_uid)
in_userns(sysctls, CLONE_NEWNET)

# /proc/sys decides by euid 0, not by capabilities: a non-root holder of CAP_DAC_OVERRIDE is
# refused; CAP_NET_ADMIN opens net.* sysctls
def capset(effective):
    hdr = struct.pack("Ii", 0x20080522, 0)
    data = struct.pack("6I", effective & 0xffffffff, 0xffffffff, 0, effective >> 32, 0xff, 0)
    return libc.capset(hdr, data)
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    libc.prctl(8, 1, 0, 0, 0)
    os.setresuid(1000, 1000, 1000)
    for c, name in [(1 << 1, "DAC_OVERRIDE"), (1 << 12, "NET_ADMIN")]:
        capset(c)
        for p in ["/proc/sys/kernel/domainname", "/proc/sys/net/ipv4/ip_unprivileged_port_start"]:
            print(name, p, try_(lambda: os.close(os.open(p, os.O_WRONLY))))
    sys.stdout.flush()
    os._exit(0)
os.waitpid(pid, 0)

# SysV IPC: root of the namespace owning the IPC namespace removes and reads user 1's queue
def ipc():
    ids = []
    def make():
        ids.append(libc.msgget(0x1234, 0o1600))
        open(D + "/qid", "w").write(str(ids[0]))
    pid = as_user_1(make)
    q = int(open(D + "/qid").read())
    buf = ctypes.create_string_buffer(256)
    print("msgget of user 1's queue", libc.msgget(0x1234, 0o600) == q,
          "stat", libc.msgctl(q, 2, buf) == 0 or err(),
          "remove", libc.msgctl(q, 0, None) == 0 or err())
    os.kill(pid, 9)
    os.waitpid(pid, 0)
in_userns(ipc, CLONE_NEWIPC)
PY
rm -rf rootless
