# docker-run-options: --cap-add SYS_ADMIN --cap-add NET_ADMIN --security-opt apparmor=unconfined
# setns(2) through a pidfd, switching to several of that process's namespaces at once (since
# Linux 5.8): the flag checks; checking one by one in the order user, mount, UTS, IPC, pid,
# cgroup, network, time, and switching them together only when all pass; when a user namespace is
# entered along with them, the later ones are judged by the capabilities there; multiple threads,
# shared fs, ptrace permission, processes that have already ended. nsenter -t from util-linux
# 2.41 on enters this way, and does not fall back to /proc/<pid>/ns/* when it fails. The native
# side is a docker container with CAP_SYS_ADMIN
cd /tmp
python3 - <<'PY'
import ctypes, errno, functools, os, socket, threading, time

print = functools.partial(print, flush=True)
libc = ctypes.CDLL(None, use_errno=True)
CLONE_VM = 0x100
CLONE_NEWTIME, CLONE_NEWNS, CLONE_NEWCGROUP, CLONE_NEWUTS = 0x80, 0x20000, 0x2000000, 0x4000000
CLONE_NEWIPC, CLONE_NEWUSER, CLONE_NEWPID, CLONE_NEWNET = 0x8000000, 0x10000000, 0x20000000, 0x40000000
ALL = CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWPID | CLONE_NEWNET
FIVE = CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWNET | CLONE_NEWNS | CLONE_NEWCGROUP
KINDS = ["mnt", "uts", "ipc", "net", "cgroup", "pid", "pid_for_children", "user", "time"]
CAP_SYS_PTRACE, CAP_SYS_ADMIN = 19, 21


def err(f, *a):
    try:
        f(*a)
        return "ok"
    except OSError as e:
        return errno.errorcode[e.errno]


def ns(pid="self"):
    return {k: os.readlink(f"/proc/{pid}/ns/{k}") for k in KINDS}


def same(a, b):
    return " ".join(k for k in KINDS if a[k] == b[k])


def fork(body):
    """Run body in a child process and wait for it to end; an exception body raises is printed
    too."""
    pid = os.fork()
    if pid == 0:
        try:
            body()
        except BaseException as e:
            print("child raised", repr(e))
        os._exit(0)
    os.waitpid(pid, 0)


def target(flags, hostname=None, pidns=False, nondumpable=False):
    """Start a process that has unshared flags and return (child, target, target's pidfd); with
    pidns the target is process 1 in the new pid namespace (forked by the child)."""
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(r)
        try:
            os.unshare(flags)
            if hostname:
                socket.sethostname(hostname)
            if nondumpable:
                libc.prctl(4, 0, 0, 0, 0)  # PR_SET_DUMPABLE
            if pidns:
                c = os.fork()
                if c == 0:
                    time.sleep(100)
                    os._exit(0)
                os.write(w, b"%d\n" % c)
                os.waitpid(c, 0)
                os._exit(0)
            os.write(w, b"%d\n" % os.getpid())
            time.sleep(100)
        except BaseException as e:
            print("target raised", repr(e))
        os._exit(0)
    os.close(w)
    t = int(os.read(r, 64))
    os.close(r)
    return pid, t, os.pidfd_open(t)


def stop(t):
    for p in {t[0], t[1]}:
        try:
            os.kill(p, 9)
        except OSError:
            pass
    os.waitpid(t[0], 0)
    os.close(t[2])


class CapHeader(ctypes.Structure):
    _fields_ = [("version", ctypes.c_uint32), ("pid", ctypes.c_int)]


class CapData(ctypes.Structure):
    _fields_ = [("effective", ctypes.c_uint32), ("permitted", ctypes.c_uint32), ("inheritable", ctypes.c_uint32)]


def drop_caps(effective=(), permitted=()):
    """capget, capset: remove these capabilities from the effective set (those in permitted also
    from the permitted set)."""
    h = CapHeader(0x20080522, 0)
    d = (CapData * 2)()
    assert libc.syscall(90, ctypes.byref(h), d) == 0
    for c in list(effective) + list(permitted):
        d[c // 32].effective &= ~(1 << (c % 32))
    for c in permitted:
        d[c // 32].permitted &= ~(1 << (c % 32))
    assert libc.syscall(91, ctypes.byref(h), d) == 0


def parked_thread():
    ev = threading.Event()
    t = threading.Thread(target=ev.wait)
    t.start()
    return ev, t


# The hostname differs between the two sides: first switch into a UTS namespace of our own
os.unshare(CLONE_NEWUTS)
socket.sethostname("outer")
me = os.pidfd_open(os.getpid())

print("== flags")
print("zero", err(os.setns, me, 0))
print("not a namespace flag", err(os.setns, me, CLONE_VM))
print("own uts", err(os.setns, me, CLONE_NEWUTS))
print("own user", err(os.setns, me, CLONE_NEWUSER))
print("own all but user", err(os.setns, me, ALL))
print("own time", err(os.setns, me, CLONE_NEWTIME))
reg = os.open("/etc/hostname", os.O_RDONLY)
print("regular file", err(os.setns, reg, CLONE_NEWUTS))
os.close(reg)
print("bad fd", err(os.setns, 1000, CLONE_NEWUTS))
uts = os.open("/proc/self/ns/uts", os.O_RDONLY)
print("uts file with flag 0", err(os.setns, uts, 0), "with pid", err(os.setns, uts, CLONE_NEWPID))
os.close(uts)

print("== several at once")
t = target(FIVE, "inner")


def several():
    os.chdir("/tmp")
    before = ns()
    print("setns", err(os.setns, t[2], FIVE))
    after = ns()
    print("same as target:", same(after, ns(t[1])))
    print("unchanged:", same(after, before))
    print("hostname", socket.gethostname(), "cwd", os.getcwd())


fork(several)


def only_mnt():
    os.chdir("/tmp")
    print("mnt only", err(os.setns, t[2], CLONE_NEWNS), "cwd", os.getcwd())


fork(only_mnt)


# When switching only the mount namespace, fs must not be shared with other threads; when
# switching others along with it, Linux checks a temporary copy of fs, so it succeeds anyway
def threaded():
    ev, th = parked_thread()
    print("threaded mnt only", err(os.setns, t[2], CLONE_NEWNS))
    print("threaded mnt and uts", err(os.setns, t[2], CLONE_NEWNS | CLONE_NEWUTS), socket.gethostname())
    print("threaded time", err(os.setns, t[2], CLONE_NEWTIME))
    time_ns = os.open("/proc/self/ns/time", os.O_RDONLY)
    print("threaded time file", err(os.setns, time_ns, 0))
    ev.set()
    th.join()


fork(threaded)
stop(t)

print("== pid")
t = target(CLONE_NEWPID, pidns=True)


def enter_pid():
    before = ns()
    print("setns", err(os.setns, t[2], CLONE_NEWPID))
    after = ns()
    print("own pid unchanged", after["pid"] == before["pid"], "children's is the target's", after["pid_for_children"] == ns(t[1])["pid"])
    r, w = os.pipe()
    c = os.fork()
    if c == 0:
        os.write(w, b"%d" % os.getpid())
        os._exit(0)
    os.waitpid(c, 0)
    print("child pid inside", os.read(r, 16).decode())


fork(enter_pid)
outer = os.pidfd_open(os.getpid())
other = target(CLONE_NEWUTS, "other")


# Entering the outer pid namespace from an inner one is EINVAL, and the UTS namespace requested
# along with it is not switched either
def from_inside():
    os.unshare(CLONE_NEWPID)
    c = os.fork()
    if c == 0:
        print("outer pid ns", err(os.setns, outer, CLONE_NEWPID))
        print("outer pid ns and uts", err(os.setns, other[2], CLONE_NEWPID | CLONE_NEWUTS), socket.gethostname())
        print("uts alone", err(os.setns, other[2], CLONE_NEWUTS), socket.gethostname())
        os._exit(0)
    os.waitpid(c, 0)


fork(from_inside)
stop(other)
stop(t)

print("== user")
t = target(CLONE_NEWUSER | CLONE_NEWUTS, "userns")


# Without CAP_SYS_ADMIN but owning that user namespace: entering only UTS fails, entering it
# together with the user namespace works
def without_sys_admin():
    drop_caps(effective=[CAP_SYS_ADMIN])
    print("uts alone", err(os.setns, t[2], CLONE_NEWUTS), socket.gethostname())
    print("user and uts", err(os.setns, t[2], CLONE_NEWUSER | CLONE_NEWUTS), socket.gethostname())
    print("user now the target's", ns()["user"] == ns(t[1])["user"])
    print("CapEff", [l.split()[1] for l in open("/proc/self/status") if l.startswith("CapEff")][0])


fork(without_sys_admin)
fork(lambda: print("root uts alone", err(os.setns, t[2], CLONE_NEWUTS), socket.gethostname()))


def threaded_user():
    ev, th = parked_thread()
    print("threaded user and uts", err(os.setns, t[2], CLONE_NEWUSER | CLONE_NEWUTS), socket.gethostname())
    ev.set()
    th.join()


fork(threaded_user)
stop(t)

print("== access")
t = target(CLONE_NEWUTS, "hidden", nondumpable=True)


def no_ptrace():
    drop_caps(permitted=[CAP_SYS_PTRACE])
    print("nondumpable target", err(os.setns, t[2], CLONE_NEWUTS), socket.gethostname())


fork(no_ptrace)
stop(t)

print("== exited")
c = os.fork()
if c == 0:
    os._exit(0)
fd = os.pidfd_open(c)
while open(f"/proc/{c}/stat").read().split()[2] != "Z":
    time.sleep(0.01)
print("zombie", err(os.setns, fd, CLONE_NEWUTS))
os.waitpid(c, 0)
print("reaped", err(os.setns, fd, CLONE_NEWUTS))
PY

# util-linux's nsenter -t and unshare
echo "== nsenter $(nsenter --version | grep -c util-linux)"
rm -f /tmp/ready*
wait_for() { while [ ! -e "$1" ]; do sleep 0.01; done; }
unshare -u -n -i -m sh -c 'hostname inner; touch /tmp/ready1; exec sleep 100' &
p1=$!
wait_for /tmp/ready1
echo "uts: $(nsenter -t $p1 -u hostname)"
echo "net links: $(nsenter -t $p1 -n ip -o link | wc -l)"
cmp='hostname; for k in mnt uts ipc net pid user cgroup time; do [ "$(readlink /proc/self/ns/$k)" = "$(readlink /proc/'$p1'/ns/$k)" ] && echo "  $k same"; done'
nsenter -t $p1 -m -u -i -n sh -c "$cmp"
echo "-a:"
nsenter -t $p1 -a sh -c "$cmp"
unshare -p -f --mount-proc sh -c 'touch /tmp/ready2; exec sleep 100' &
p2=$!
wait_for /tmp/ready2
nsenter -t "$(pgrep -P $p2)" -p -m sh -c 'echo "pid inside: $$"'
unshare -U -r -u sh -c 'hostname u; touch /tmp/ready3; exec sleep 100' &
p3=$!
wait_for /tmp/ready3
nsenter -t $p3 -U -u sh -c 'echo "user: $(hostname) uid $(id -u) map $(tr -s " " </proc/self/uid_map)"'
nsenter -t $p3 -a sh -c 'echo "user -a: $(hostname) uid $(id -u)"'
echo "unshare -r: $(unshare -r sh -c 'id -u; id -g; cat /proc/self/setgroups' | tr '\n' ' ')"
# The parent of unshare -f blocks SIGTERM while waiting for the child, and the child is process 1
# in a new pid namespace, to which only SIGKILL and SIGSTOP from ancestor namespaces are sure to
# be delivered: kill it with SIGKILL, otherwise wait would have to wait for the sleep 100 inside
# to end by itself
kill $p1 $p2 $p3
kill -9 $(pgrep -P $p2)
wait
nsenter -t $p1 -u true 2>/dev/null
echo "exited: $?"
