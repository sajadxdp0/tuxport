# docker-run-options: --cap-add SYS_ADMIN --cap-add NET_ADMIN --security-opt apparmor=unconfined
# Namespaces: the /proc/<pid>/ns links and the files opened from them, mount namespaces (unshare,
# clone's CLONE_NEWNS, the two sides' mounts invisible to each other, /proc/<pid>/mountinfo
# showing that process's, setns back, pivot_root in a new namespace not affecting the outside),
# and various errors. The native side is a docker container with CAP_SYS_ADMIN. Namespace IDs
# differ between the two sides, so only the format and whether they are the same or different
# are compared
cd /tmp
rm -rf ns
mkdir -p ns/a ns/b ns/root

echo "ns: $(ls /proc/self/ns | tr '\n' ' ')"
for n in /proc/self/ns/*; do
    readlink "$n" | sed 's/[0-9][0-9]*/N/'
done
stat -L -c '%F %a %h %u %s' /proc/self/ns/mnt
echo "same device for all: $(stat -L -c %d /proc/self/ns/* | sort -u | wc -l)"
echo "not the procfs device: $([ "$(stat -L -c %d /proc/self/ns/mnt)" != "$(stat -c %d /proc/self)" ] && echo yes)"
echo "inode is the number: $([ "$(stat -L -c %i /proc/self/ns/mnt)" = "$(readlink /proc/self/ns/mnt | tr -dc 0-9)" ] && echo yes)"
echo "same for threads: $([ "$(readlink /proc/$$/ns/mnt)" = "$(readlink /proc/$$/task/$$/ns/mnt)" ] && echo yes)"
stat -f -c %t /proc/self/ns/mnt

# busybox's unshare -m: what is mounted inside is not seen outside
busybox unshare -m sh -c '
    mount --make-rprivate /
    mount -t tmpfs inside /tmp/ns/a && echo "inside: $(grep -c " /tmp/ns/a " /proc/self/mountinfo)"
    echo "different ns: $([ "$(readlink /proc/self/ns/mnt)" != "$(readlink /proc/$PPID/ns/mnt)" ] && echo yes)"
'
echo "outside: $(grep -c ' /tmp/ns/a ' /proc/self/mountinfo)"

python3 - <<'PY'
import ctypes, errno, functools, os, threading, time

print = functools.partial(print, flush=True)

libc = ctypes.CDLL(None, use_errno=True)
libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_ulong, ctypes.c_void_p]
libc.syscall.restype = ctypes.c_long
CLONE_NEWNS, CLONE_FS, CLONE_NEWUTS, CLONE_NEWUSER = 0x20000, 0x200, 0x04000000, 0x10000000
MS_REC, MS_PRIVATE, MS_BIND = 0x4000, 1 << 18, 0x1000


def call(f, *args):
    r = f(*args)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


def join(t):
    # When t.join() returns the thread may not have finished exiting in the kernel, and when
    # forking right after that Python warns about a "fork of a multithreaded process" based on
    # the thread count in /proc/self/stat (natively only sometimes), so wait until it has exited
    t.join()
    for _ in range(500):
        if len(os.listdir("/proc/self/task")) == 1:
            break
        time.sleep(0.01)


def mounted(path, pid="self"):
    with open("/proc/%s/mountinfo" % pid) as f:
        return any(l.split()[4] == path for l in f)


def ns(pid="self"):
    return os.readlink("/proc/%s/ns/mnt" % pid)


def private():
    return call(libc.mount, None, b"/", None, MS_REC | MS_PRIVATE, None)


mine = os.open("/proc/self/ns/mnt", os.O_RDONLY)
print("fd link is the namespace", os.readlink("/proc/self/fd/%d" % mine) == ns())
try:
    os.read(mine, 10)
except OSError as e:
    print("read", errno.errorcode[e.errno])
print("setns, wrong type", call(libc.setns, mine, CLONE_NEWUTS))
print("setns, not a namespace", call(libc.setns, 0, 0))
print("setns, own namespace", call(libc.setns, mine, CLONE_NEWNS))
print("cwd after setns", os.getcwd())
os.chdir("/tmp")
print("clone NEWNS with FS", libc.syscall(220, CLONE_NEWNS | CLONE_FS | 17, 0, 0, 0, 0), errno.errorcode[ctypes.get_errno()])

# The child in a new mount namespace: what either side mounts later is invisible to the other,
# and its mountinfo seen from outside shows its mounts
r1, w1 = os.pipe()
r2, w2 = os.pipe()
pid = os.fork()
if pid == 0:
    os.unshare(CLONE_NEWNS)
    print("child private", private(), flush=True)
    print("child new namespace", ns() != ns(os.getppid()), flush=True)
    print("child mount", call(libc.mount, b"child", b"/tmp/ns/a", b"tmpfs", 0, None), flush=True)
    os.write(w1, b"x")
    os.read(r2, 1)
    print("child sees parent's mount", mounted("/tmp/ns/b"), flush=True)
    print("child sees own mount", mounted("/tmp/ns/a"), flush=True)
    # setns back to the original namespace: the root and current directories become its root
    os.chdir("/tmp/ns")
    print("child setns back", call(libc.setns, mine, CLONE_NEWNS), flush=True)
    print("child back", ns() == ns(os.getppid()), os.getcwd(), mounted("/tmp/ns/a"), mounted("/tmp/ns/b"), flush=True)
    os._exit(0)
os.read(r1, 1)
print("parent mount", call(libc.mount, b"parent", b"/tmp/ns/b", b"tmpfs", 0, None))
print("parent sees child's mount", mounted("/tmp/ns/a"))
print("child's mountinfo", mounted("/tmp/ns/a", pid), mounted("/tmp/ns/b", pid))
# Mounts in another namespace: they cannot be bound or unmounted even through /proc/<pid>/root
print("bind from another namespace", call(libc.mount, ("/proc/%d/root/tmp/ns/a" % pid).encode(), b"/tmp/ns/root", None, MS_BIND, None))
libc.umount2.argtypes = [ctypes.c_char_p, ctypes.c_int]
print("umount in another namespace", call(libc.umount2, ("/proc/%d/root/tmp/ns/a" % pid).encode(), 0))
os.write(w2, b"x")
os.waitpid(pid, 0)
libc.umount2(b"/tmp/ns/b", 0)

# pivot_root in a new namespace: the outside is not affected
pid = os.fork()
if pid == 0:
    os.unshare(CLONE_NEWNS)
    private()
    libc.mount(b"newroot", b"/tmp/ns/root", b"tmpfs", 0, None)
    os.mkdir("/tmp/ns/root/old")
    os.chdir("/tmp/ns/root")
    print("pivot_root inside", call(libc.syscall, 41, b".", b"old"), flush=True)
    os.chdir("/")
    print("new root", sorted(os.listdir("/")), sorted(os.listdir("/old/tmp/ns")), flush=True)
    print("umount old", call(libc.umount2, b"/old", 2), sorted(os.listdir("/old")), flush=True)
    os._exit(0)
os.waitpid(pid, 0)
print("outside root", os.path.isdir("/tmp/ns/root"), mounted("/tmp/ns/root"), os.path.exists("/bin/sh"))

# A multithreaded process cannot setns into a mount namespace; without CAP_SYS_ADMIN one cannot
# unshare or setns
ev = threading.Event()
t = threading.Thread(target=ev.wait)
t.start()
print("setns with threads", call(libc.setns, mine, CLONE_NEWNS))
ev.set()
join(t)
pid = os.fork()
if pid == 0:
    os.setuid(65534)
    try:
        os.unshare(CLONE_NEWNS)
        print("unprivileged unshare ok", flush=True)
    except OSError as e:
        print("unprivileged unshare", errno.errorcode[e.errno], flush=True)
    print("unprivileged setns", call(libc.setns, mine, CLONE_NEWNS), flush=True)
    os._exit(0)
os.waitpid(pid, 0)
PY
echo "left: $(grep -c ' /tmp/ns' /proc/self/mountinfo)"

# User namespaces
cat /proc/self/uid_map /proc/self/gid_map /proc/self/projid_map /proc/self/setgroups
echo "max_user_namespaces: $(test -r /proc/sys/user/max_user_namespaces && echo yes)"
echo "unshare -r: $(busybox unshare -r sh -c 'id -u; id -g; cat /proc/self/uid_map' | tr -s ' \n' ' ')"
python3 - <<'PY2'
import ctypes, errno, functools, os, threading, time

print = functools.partial(print, flush=True)
libc = ctypes.CDLL(None, use_errno=True)
libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_ulong, ctypes.c_void_p]
libc.syscall.restype = ctypes.c_long
CLONE_NEWNS, CLONE_NEWUSER, CLONE_FS = 0x20000, 0x10000000, 0x200


def call(f, *args):
    r = f(*args)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


def join(t):
    # When t.join() returns the thread may not have finished exiting in the kernel, and when
    # forking right after that Python warns about a "fork of a multithreaded process" based on
    # the thread count in /proc/self/stat (natively only sometimes), so wait until it has exited
    t.join()
    for _ in range(500):
        if len(os.listdir("/proc/self/task")) == 1:
            break
        time.sleep(0.01)


def write(path, data):
    try:
        with open(path, "w") as f:
            f.write(data)
        return "ok"
    except OSError as e:
        return errno.errorcode[e.errno]


def read(path):
    with open(path) as f:
        return " ".join(f.read().split())


def capeff():
    with open("/proc/self/status") as f:
        return [l.split()[1] for l in f if l.startswith("CapEff")][0]


def fork(f):
    """Run f in a child process: print whatever goes wrong, then exit, without going on to run
    the parent's code."""
    pid = os.fork()
    if pid == 0:
        try:
            f()
        except OSError as e:
            print("raised", errno.errorcode[e.errno])
        except BaseException as e:
            print("raised", repr(e))
        finally:
            os._exit(0)
    return pid


def child(f):
    pid = fork(f)
    os.waitpid(pid, 0)
    return pid


def errno_of(f, *args):
    try:
        f(*args)
        return "ok"
    except OSError as e:
        return errno.errorcode[e.errno]


# /proc/<pid>/exe requires being able to look at that process (PTRACE_MODE_READ_FSCREDS): other
# users' and non-dumpable ones cannot be looked at, one's own always can
def exe_as_nobody():
    os.setgid(65534)
    os.setuid(65534)
    ppid = os.getppid()
    print("nobody: parent's exe", errno_of(os.readlink, "/proc/%d/exe" % ppid),
          "open", errno_of(os.open, "/proc/%d/exe" % ppid, os.O_RDONLY),
          "own", os.readlink("/proc/self/exe") == os.readlink("/proc/%d/exe" % os.getpid()))
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:
        libc.prctl(4, 0, 0, 0, 0)  # PR_SET_DUMPABLE
        print("non-dumpable own exe", errno_of(os.readlink, "/proc/self/exe"))
        os.write(w, b"x")
        time.sleep(0.5)
        os._exit(0)
    os.read(r, 1)
    print("non-dumpable child's exe", errno_of(os.readlink, "/proc/%d/exe" % pid))
    os.waitpid(pid, 0)


child(exe_as_nobody)

init_user = os.open("/proc/self/ns/user", os.O_RDONLY)
print("setns to own user namespace", call(libc.setns, init_user, CLONE_NEWUSER))


# Created by root: before the maps are written the IDs are 65534, with all capabilities; it writes
# 0 0 1 itself; gid_map requires denying setgroups first
def root_inside():
    os.unshare(CLONE_NEWUSER)
    print("new user ns", os.readlink("/proc/self/ns/user") != os.readlink("/proc/self/fd/%d" % init_user))
    # The links of a process in another user namespace (the parent) cannot be read: we are not in
    # the same user namespace, and have no CAP_SYS_PTRACE there
    try:
        os.readlink("/proc/%d/ns/user" % os.getppid())
    except OSError as e:
        print("parent's link", errno.errorcode[e.errno])
    # exe is like root and cwd (proc_fd_access_allowed): neither reading the link nor following it
    # works
    for link in ("exe", "cwd", "root"):
        path = "/proc/%d/%s" % (os.getppid(), link)
        print("parent's", link, errno_of(os.readlink, path), errno_of(os.stat, path),
              "lstat", errno_of(os.lstat, path))
    print("unmapped", os.getuid(), os.getgid(), os.getresuid(), "map", repr(read("/proc/self/uid_map")))
    print("caps", capeff(), "setgroups", read("/proc/self/setgroups"))
    print("bad map", write("/proc/self/uid_map", "0 0\n"), write("/proc/self/uid_map", "0 0 0\n"))
    print("map someone else", write("/proc/self/uid_map", "0 1000 1\n"))
    print("uid_map", write("/proc/self/uid_map", "0 0 1\n"), os.getuid(), read("/proc/self/uid_map"))
    print("again", write("/proc/self/uid_map", "0 0 1\n"))
    print("gid_map before deny", write("/proc/self/gid_map", "0 0 1\n"))
    print("deny", write("/proc/self/setgroups", "deny"), read("/proc/self/setgroups"))
    print("allow again", write("/proc/self/setgroups", "allow"), write("/proc/self/setgroups", "bogus"))
    print("gid_map", write("/proc/self/gid_map", "0 0 1\n"), os.getgid())
    try:
        os.setgroups([])
        print("setgroups ok")
    except OSError as e:
        print("setgroups", errno.errorcode[e.errno])
    for uid in (1, 5000):
        try:
            os.setuid(uid)
        except OSError as e:
            print("setuid", uid, errno.errorcode[e.errno])
    print("stat /", os.stat("/").st_uid, "stat /proc/1", os.stat("/proc/1").st_uid)
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(r)
        os.dup2(w, 1)
        os.execv("/bin/sh", ["sh", "-c", "id -u; grep CapEff /proc/self/status"])
    os.close(w)
    # Read to the end: the two commands write separately, so one read may see only the first
    out = b""
    while chunk := os.read(r, 100):
        out += chunk
    print("exec", out.split())
    os.waitpid(pid, 0)
    # From this user namespace one cannot go back to the parent one
    print("setns to the parent", call(libc.setns, init_user, CLONE_NEWUSER))
    # root outside looks at its maps
    os.write(wfd, b"x")
    os.read(rfd2, 1)


rfd, wfd = os.pipe()
rfd2, wfd2 = os.pipe()
pid = fork(root_inside)
os.close(wfd)
os.read(rfd, 1)
print("parent sees", read("/proc/%d/uid_map" % pid), "|", read("/proc/%d/gid_map" % pid), "|", read("/proc/%d/setgroups" % pid))
with open("/proc/%d/status" % pid) as f:
    print("parent sees ids", [l.split()[1:3] for l in f if l.split()[0] in ("Uid:", "Gid:")])


# root outside can setns into it
def enter():
    fd = os.open("/proc/%d/ns/user" % pid, os.O_RDONLY)
    print("setns into it", call(libc.setns, fd, CLONE_NEWUSER), os.getuid(), capeff())


child(enter)
os.write(wfd2, b"x")
os.waitpid(pid, 0)


# User namespaces that ordinary users create themselves are not compared here: Ubuntu's
# kernel.apparmor_restrict_unprivileged_userns from 23.10 on (on in the development VM and on
# GitHub's runners) denies them capabilities inside, so what the native side shows is not the
# behavior of upstream Linux. This part is checked against the upstream behavior in
# hosts/cli/tests (user_namespace_of_an_unprivileged_user)


# Errors: a multithreaded process cannot unshare, CLONE_NEWUSER and CLONE_FS cannot be used
# together, none can be created after chroot, nesting is at most 32 levels
ev = threading.Event()
t = threading.Thread(target=ev.wait)
t.start()
try:
    os.unshare(CLONE_NEWUSER)
except OSError as e:
    print("threaded unshare", errno.errorcode[e.errno])
ev.set()
join(t)
print("clone NEWUSER with FS", libc.syscall(220, CLONE_NEWUSER | CLONE_FS | 17, 0, 0, 0, 0), errno.errorcode[ctypes.get_errno()])


def chrooted():
    os.chroot("/tmp")
    os.unshare(CLONE_NEWUSER)
    print("chrooted unshare ok")


child(chrooted)


def nested():
    for level in range(1, 40):
        try:
            os.unshare(CLONE_NEWUSER)
        except OSError as e:
            print("nesting stops at", level, errno.errorcode[e.errno])
            return
        write("/proc/self/uid_map", "0 0 1\n")
        write("/proc/self/setgroups", "deny")
        write("/proc/self/gid_map", "0 0 1\n")


child(nested)
PY2

# pid namespaces: busybox's unshare -p -f --mount-proc, where the sh inside is process 1
busybox unshare -p -f --mount-proc sh -c '
    echo "pid $$ ppid $PPID"
    echo /proc/[0-9]*
    echo "self $(readlink /proc/self) thread-self $(readlink /proc/thread-self)"
    sh -c "echo child \$\$ ppid \$PPID"
    cut -d" " -f1,4-6 /proc/self/stat
    grep -E "^(Tgid|Pid|PPid|NStgid|NSpid|NSpgid|NSsid)" /proc/self/status
    sleep 100 &
    kill -0 $! && echo "background $!"
    # When ash reports the job as killed (Terminated) depends on speed, so it is not compared
    kill $! 2>/dev/null; wait $! 2>/dev/null; echo "killed with status $?"
    kill -TERM 1; kill -KILL 1; echo "init survives its own SIGTERM and SIGKILL"
    echo "last pid $(cat /proc/sys/kernel/ns_last_pid)"
'
echo "unshare exits with $?"
# strace -f inside: the new process IDs the tracer gets (PTRACE_GETEVENTMSG) and the results of
# wait are all the inner ones
busybox unshare -p -f --mount-proc strace -f -qq -e trace=exit_group,kill sh -c 'sh -c "exit 3"; kill -0 $$; echo "inner exit $?"'
# task/1/children of process 1 has the inner IDs (the two background sleeps and cat itself)
busybox unshare -p -f --mount-proc sh -c 'sleep 100 & sleep 100 & echo "children of 1: $(cat /proc/1/task/1/children)."; kill 2 3'
# Owners of signal-driven I/O are numbers in the caller's pid namespace: F_SETOWN/F_GETOWN (whose
# number is in the syscall's arguments) and the socket ioctls FIOSETOWN/FIOGETOWN and
# SIOCSPGRP/SIOCGPGRP (whose number is in memory)
busybox unshare -p -f --mount-proc python3 -c '
import fcntl, os, socket, struct
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
fcntl.fcntl(s, fcntl.F_SETOWN, os.getpid())
print("pid", os.getpid(), "F_GETOWN", fcntl.fcntl(s, fcntl.F_GETOWN))
fcntl.ioctl(s, 0x8901, struct.pack("i", os.getpid()))
print("FIOGETOWN", struct.unpack("i", fcntl.ioctl(s, 0x8903, b"\0" * 4))[0])
os.setpgid(0, 0)
fcntl.ioctl(s, 0x8902, struct.pack("i", -os.getpid()))
print("SIOCGPGRP", struct.unpack("i", fcntl.ioctl(s, 0x8904, b"\0" * 4))[0])
# F_GETOWN would return -1, which Python takes for an error
fcntl.fcntl(s, fcntl.F_SETOWN, -1)
print("F_SETOWN -1, then SIOCGPGRP", struct.unpack("i", fcntl.ioctl(s, 0x8904, b"\0" * 4))[0])
'
# Process 1 exits while a deeper tree still runs (1 → sh → two sleeps): everything inside is
# killed, the orphans left by the killed sh are reaped, and unshare sees process 1 end (Linux's
# zap_pid_ns_processes)
timeout 20 busybox unshare -p -f sh -c 'sh -c "sleep 100 & sleep 100 & wait" & sleep 0.5; exit 7'
echo "process 1 left a deeper tree, unshare exits with $?"
# The parent is killed outside the pid namespace: process 1 inside still receives the
# PR_SET_PDEATHSIG (SIGKILL) it set, and since the sender is invisible inside, it is handled as a
# forced signal (util-linux's unshare --kill-child and bwrap's --die-with-parent rely on this).
# It is already set before exec'ing into sleep
unshare -p -f --kill-child sleep 1000 &
u=$!
c=
while [ "$(cat /proc/$c/comm 2>/dev/null)" != sleep ]; do sleep 0.01; c=$(pgrep -P $u); done
kill -9 $u
wait $u 2>/dev/null
n=0
while [ -e /proc/$c ] && [ "$(cut -d" " -f3 /proc/$c/stat 2>/dev/null)" != Z ] && [ $n -lt 300 ]; do
    sleep 0.01
    n=$((n + 1))
done
echo "pid 1 of the namespace dies with its parent: $([ $n -lt 300 ] && echo yes)"

python3 - <<'PY3'
import ctypes, errno, functools, os, signal, socket, struct, threading, time

print = functools.partial(print, flush=True)

libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long
libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_ulong, ctypes.c_void_p]
libc.umount.argtypes = [ctypes.c_char_p]
CLONE_NEWPID, CLONE_NEWNS, CLONE_THREAD, CLONE_SIGHAND, CLONE_VM, CLONE_PARENT = (
    0x20000000, 0x20000, 0x10000, 0x800, 0x100, 0x8000)
SIGCHLD = 17
os.makedirs("/tmp/ns/p", exist_ok=True)


def err(f, *args):
    try:
        f(*args)
        return "ok"
    except OSError as e:
        return errno.errorcode[e.errno]


def join(t):
    # When t.join() returns the thread may not have finished exiting in the kernel, and when
    # forking right after that Python warns about a "fork of a multithreaded process" based on
    # the thread count in /proc/self/stat (natively only sometimes), so wait until it has exited
    t.join()
    for _ in range(500):
        if len(os.listdir("/proc/self/task")) == 1:
            break
        time.sleep(0.01)


def fork(f):
    """Run f in a child process: print whatever goes wrong, then exit, without going on to run
    the parent's code."""
    pid = os.fork()
    if pid == 0:
        try:
            f()
        except OSError as e:
            print("raised", errno.errorcode[e.errno])
        except BaseException as e:
            print("raised", repr(e))
        finally:
            os._exit(0)
    return pid


def child(f):
    pid = fork(f)
    return os.waitpid(pid, 0)[1]


def try_fork():
    try:
        pid = os.fork()
    except OSError as e:
        return errno.errorcode[e.errno]
    if pid == 0:
        os._exit(0)
    os.waitpid(pid, 0)
    return "ok"


def link(path):
    try:
        return os.readlink(path)
    except OSError as e:
        return errno.errorcode[e.errno]


def status(pid, key, proc="/proc"):
    with open("%s/%s/status" % (proc, pid)) as f:
        return next(l.split(":", 1)[1].split() for l in f if l.startswith(key + ":"))


def pids(proc):
    return sorted(int(d) for d in os.listdir(proc) if d.isdigit())


def inner_global(init, local):
    """When process 1 is the global init, the ID outside of the process whose ID is local in its
    namespace."""
    for d in pids("/proc"):
        try:
            if status(d, "NSpid")[-1] == str(local) and init in (d, int(status(d, "PPid")[0])):
                return d
        except (OSError, StopIteration):
            pass


# After unshare one's own process does not change, and later children are in the new namespace,
# the first being process 1 there; before entering it one cannot unshare again or create
# threads; after process 1 exits no more processes can be created inside
def unshared():
    me = os.getpid()
    os.unshare(CLONE_NEWPID)
    print("pid unchanged", os.getpid() == me, "ns unchanged", link("/proc/self/ns/pid") == mine)
    print("pid_for_children before fork", link("/proc/self/ns/pid_for_children"))
    print("unshare again", err(os.unshare, CLONE_NEWPID))
    try:
        threading.Thread(target=lambda: None).start()
        print("thread started")
    except RuntimeError as e:
        print("thread:", e)
    r, w = os.pipe()

    def init():
        print("init pid", os.getpid(), "ppid", os.getppid(), "tid", threading.get_native_id())
        print("pgid", os.getpgid(0), "sid", os.getsid(0), "getpgid(1)", os.getpgid(1))
        os.write(w, link("/proc/self/ns/pid").encode())
        print("outside pid", err(os.kill, me, 0), "wait for it", err(os.waitpid, me, 0))
        print("CLONE_PARENT", libc.syscall(220, CLONE_PARENT | SIGCHLD, 0, 0, 0, 0), errno.errorcode[ctypes.get_errno()])

    pid = fork(init)
    os.close(w)
    inner = os.read(r, 100).decode()
    os.waitpid(pid, 0)
    print("child pid is not 1", pid != 1)
    print("child's ns is pid_for_children", inner == link("/proc/self/ns/pid_for_children"))
    print("pid_for_children differs from pid", inner != mine)
    print("fork after init exited", try_fork())


mine = link("/proc/self/ns/pid")
child(unshared)
print("thread NEWPID", libc.syscall(220, CLONE_NEWPID | CLONE_THREAD | CLONE_SIGHAND | CLONE_VM, 0, 0, 0, 0), errno.errorcode[ctypes.get_errno()])


# Process 1: its children, orphans reparented to it, setsid, threads, kill(-1), ns_last_pid, a new
# procfs instance
def first():
    os.unshare(CLONE_NEWPID)

    def init():
        def grandchild():
            print("grandchild", os.getpid(), "ppid", os.getppid())
            orphan = fork(lambda: (time.sleep(1), print("orphan", os.getpid(), "adopted by", os.getppid())))
            print("orphan's parent exits, orphan is", orphan)

        print("grandchild status", child(grandchild))
        print("reaped orphan", os.waitpid(-1, 0))
        print("setsid", os.setsid(), "pgid", os.getpgid(0), "sid", os.getsid(0))
        ev = threading.Event()
        t = threading.Thread(target=ev.wait)
        t.start()
        print("thread", t.native_id, "pthread_kill", signal.pthread_kill(t.ident, 0))
        ev.set()
        join(t)
        sleepers = [fork(lambda: time.sleep(30)) for _ in range(2)]
        time.sleep(0.2)
        os.kill(-1, signal.SIGTERM)
        print("kill -1:", [os.waitpid(p, 0) for p in sleepers])
        print("mount proc", libc.mount(b"proc", b"/tmp/ns/p", b"proc", 0, None))
        print("pids inside", pids("/tmp/ns/p"), "self", link("/tmp/ns/p/self"))
        print("task", sorted(int(x) for x in os.listdir("/tmp/ns/p/self/task")))
        with open("/tmp/ns/p/sys/kernel/ns_last_pid") as f:
            print("ns_last_pid", f.read().strip())
        with open("/tmp/ns/p/sys/kernel/ns_last_pid", "w") as f:
            f.write("100")
        print("next child", child(lambda: print("child pid", os.getpid())))
        print("stat", open("/tmp/ns/p/1/stat").read().split()[:8:3])
        print("status", status(1, "Pid", "/tmp/ns/p"), status(1, "PPid", "/tmp/ns/p"), status(1, "NSpid", "/tmp/ns/p"))
        os.write(w, b"x")
        os.read(r2, 1)
        print("umount", libc.umount(b"/tmp/ns/p"))

    r, w = os.pipe()
    r2, w2 = os.pipe()
    pid = fork(init)
    os.read(r, 1)
    # Looking inside from outside: IDs in the new procfs instance are the inner ones, and self is
    # absent; the global procfs has the IDs of both levels
    print("outside: pids", pids("/tmp/ns/p"), "self", link("/tmp/ns/p/self"))
    print("outside: init status", status(1, "Pid", "/tmp/ns/p"), len(status(pid, "NStgid")), status(pid, "NStgid")[-1])
    print("outside: own NSpid", len(status("self", "NSpid")))
    os.write(w2, b"x")
    print("init status", os.waitpid(pid, 0)[1])


child(first)


# Signals: si_pid of signals sent to process 1 from inside is the sender's inner ID, and 0 for
# ones sent from outside; SIGTERM from outside is ignored under default handling, SIGKILL is not
def signals():
    os.unshare(CLONE_NEWPID)
    r, w = os.pipe()

    def init():
        signal.pthread_sigmask(signal.SIG_BLOCK, [signal.SIGUSR1])
        sender = fork(lambda: os.kill(1, signal.SIGUSR1))
        info = signal.sigwaitinfo([signal.SIGUSR1])
        print("from inside: si_pid", info.si_pid, "sender", sender, "code", info.si_code)
        os.waitpid(sender, 0)
        os.write(w, b"x")
        info = signal.sigwaitinfo([signal.SIGUSR1])
        print("from outside: si_pid", info.si_pid, "code", info.si_code)
        os.write(w, b"x")
        time.sleep(30)

    pid = fork(init)
    os.read(r, 1)
    os.kill(pid, signal.SIGUSR1)
    os.read(r, 1)
    os.kill(pid, signal.SIGTERM)
    time.sleep(0.3)
    print("after SIGTERM", os.waitpid(pid, os.WNOHANG))
    os.kill(pid, signal.SIGKILL)
    print("after SIGKILL", os.waitpid(pid, 0)[1])


child(signals)


# SIGSTOP from outside stops process 1, and SIGCONT from inside resumes it even though it is
# discarded; synchronous exceptions handled the default way kill it as usual
def stop_cont():
    os.unshare(CLONE_NEWPID)
    ready, go = os.pipe(), os.pipe()

    def init():
        fork(lambda: (os.read(go[0], 1), os.kill(1, signal.SIGCONT)))
        os.write(ready[1], b"x")
        time.sleep(2)
        print("init resumed")
        ctypes.string_at(0)

    def state(pid):
        with open("/proc/%d/stat" % pid) as f:
            return f.read().rsplit(")", 1)[1].split()[0]

    def until(pid, f):
        for _ in range(200):
            if f(state(pid)):
                return True
            time.sleep(0.01)

    pid = fork(init)
    os.read(ready[0], 1)
    os.kill(pid, signal.SIGSTOP)
    print("stopped", until(pid, lambda s: s == "T"))
    os.write(go[1], b"x")
    print("continued", until(pid, lambda s: s != "T"))
    print("killed by", os.WTERMSIG(os.waitpid(pid, 0)[1]))


child(stop_cont)


# Process 1 exits: the parent can wait for it only after all other processes inside have been
# killed and reaped. Those whose parent is outside (forked after unshare or setns) must wait for
# that parent to reap them after they end
def zap():
    os.unshare(CLONE_NEWPID)
    r, w = os.pipe()
    go = os.pipe()

    def init():
        fork(lambda: time.sleep(30))
        os.write(w, b"x")
        os.read(go[0], 1)

    pid = fork(init)
    os.read(r, 1)
    sleeper = inner_global(pid, 2)
    print("sleeper found", sleeper is not None)
    zombie = fork(lambda: None)
    time.sleep(0.2)
    os.write(go[1], b"x")
    time.sleep(0.3)
    print("init waits for the zombie", os.waitpid(pid, os.WNOHANG))
    print("zombie", os.waitpid(zombie, 0)[1])
    print("init status", os.waitpid(pid, 0)[1], "sleeper gone", err(os.kill, sleeper, 0))


child(zap)


# setns into a pid namespace: later children are inside and cannot see the parent; only one's own
# namespace and descendant namespaces can be entered
def entered():
    os.unshare(CLONE_NEWPID)
    r, w = os.pipe()
    outer = os.open("/proc/self/ns/pid", os.O_RDONLY)

    def init():
        print("setns to an ancestor", err(os.setns, outer, CLONE_NEWPID))
        os.write(w, b"x")
        time.sleep(30)

    pid = fork(init)
    os.read(r, 1)
    fd = os.open("/proc/%d/ns/pid" % pid, os.O_RDONLY)
    print("wrong type", err(os.setns, fd, CLONE_NEWNS))
    print("setns back to own", err(os.setns, outer, CLONE_NEWPID))
    print("children in own", link("/proc/self/ns/pid_for_children") == mine)
    print("setns into the child's", err(os.setns, fd, CLONE_NEWPID))
    print("own unchanged", link("/proc/self/ns/pid") == mine)
    print("child", child(lambda: print("joined: pid", os.getpid(), "ppid", os.getppid())))
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)


child(entered)


# SO_PEERCRED: listening outside and connecting inside, the peer pid seen inside is 0
def peercred():
    os.unshare(CLONE_NEWPID)
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind("\0tuxport-pidns")
    srv.listen()

    def init():
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.connect("\0tuxport-pidns")
        cred = c.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
        print("inside sees peer pid", struct.unpack("3i", cred)[0])
        c.send(b"y")
        c.recv(1)

    pid = fork(init)
    conn, _ = srv.accept()
    conn.recv(1)
    cred = conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
    print("outside sees the init", struct.unpack("3i", cred)[0] == pid)
    conn.send(b"x")
    os.waitpid(pid, 0)


child(peercred)


# clone3's set_tid: specifies the new process's ID at each level, from its own level outwards.
# Only the inner two levels are compared: the docker container itself is in a pid namespace, and
# the levels further out differ between the two sides
class CloneArgs(ctypes.Structure):
    _fields_ = [(n, ctypes.c_uint64) for n in (
        "flags", "pidfd", "child_tid", "parent_tid", "exit_signal", "stack", "stack_size",
        "tls", "set_tid", "set_tid_size", "cgroup")]


def clone3(*set_tid):
    tids = (ctypes.c_int * len(set_tid))(*set_tid)
    args = CloneArgs(exit_signal=SIGCHLD, set_tid=ctypes.addressof(tids), set_tid_size=len(set_tid))
    pid = libc.syscall(435, ctypes.byref(args), ctypes.sizeof(args))
    if pid == 0:
        print("  clone3 child: pid", os.getpid())
        os._exit(0)
    if pid < 0:
        return errno.errorcode[ctypes.get_errno()]
    os.waitpid(pid, 0)
    return pid


def set_tid():
    os.unshare(CLONE_NEWPID)
    print("no init yet", clone3(5))
    r, w = os.pipe()
    pid = fork(lambda: os.read(r, 1))
    print("taken", clone3(1))
    print("inner only", clone3(50) not in (50, "EINVAL"))
    print("two levels", clone3(50, 30000))
    print("out of range", clone3(-1), clone3(0))
    os.write(w, b"x")
    os.waitpid(pid, 0)


child(set_tid)


def unprivileged_set_tid():
    os.setuid(65534)
    print("unprivileged", clone3(30000))


child(unprivileged_set_tid)


# Nesting is at most 32 levels. The docker container itself is in a pid namespace, so the native
# side nests one level less; only the error is compared (the level count is in the unit test
# pid_namespaces)
def nested():
    for level in range(1, 40):
        try:
            os.unshare(CLONE_NEWPID)
        except OSError as e:
            print("nesting stops:", errno.errorcode[e.errno], "pid", os.getpid(), "deep", level > 30)
            return
        pid = os.fork()
        if pid:
            os.waitpid(pid, 0)
            os._exit(0)


child(nested)


# PR_SET_CHILD_SUBREAPER: a subreaper outside does not adopt orphans inside (not across the
# namespace), one inside adopts them as usual
PR_SET_CHILD_SUBREAPER = 36


def subreapers():
    libc.prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0)
    os.unshare(CLONE_NEWPID)

    def init():
        def orphan(note):
            fork(lambda: (time.sleep(0.3), print(note, "adopted by", os.getppid())))

        print("parent status", child(lambda: orphan("without a subreaper inside:")))
        print("init reaped", os.waitpid(-1, 0)[1])

        def middle():
            libc.prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0)
            print("middle is", os.getpid())
            print("parent status", child(lambda: orphan("with a subreaper inside:")))
            print("middle reaped", os.waitpid(-1, 0)[1])

        print("middle status", child(middle))

    print("init status", child(init))


child(subreapers)
PY3

# UTS namespaces: a hostname changed in busybox's unshare -u is not seen outside
before=$(hostname)
busybox unshare -u sh -c '
    hostname inner
    echo "inside: $(hostname) $(uname -n) $(cat /proc/sys/kernel/hostname)"
    echo "different ns: $([ "$(readlink /proc/self/ns/uts)" != "$(readlink /proc/$PPID/ns/uts)" ] && echo yes)"
'
echo "outside unchanged: $([ "$(hostname)" = "$before" ] && echo yes)"

python3 - <<'PY4'
import ctypes, errno, functools, os, socket, struct, time

print = functools.partial(print, flush=True)

libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long
libc.shmat.restype = ctypes.c_void_p
libc.shmat.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
libc.shmdt.argtypes = [ctypes.c_void_p]
libc.mq_open.argtypes = [ctypes.c_char_p, ctypes.c_int, ctypes.c_uint, ctypes.c_void_p]
libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_ulong, ctypes.c_void_p]
libc.umount.argtypes = [ctypes.c_char_p]
CLONE_NEWUTS, CLONE_NEWIPC, CLONE_NEWUSER, CLONE_SYSVSEM = 0x04000000, 0x08000000, 0x10000000, 0x40000
IPC_CREAT, IPC_EXCL, IPC_RMID, IPC_STAT, SEM_UNDO, GETVAL = 0o1000, 0o2000, 0, 2, 0x1000, 12
KEY = 0x4170704E
os.makedirs("/tmp/ns/p", exist_ok=True)


def err(f, *args):
    try:
        f(*args)
        return "ok"
    except OSError as e:
        return errno.errorcode[e.errno]


def c(r):
    return r if r != -1 else errno.errorcode[ctypes.get_errno()]


def fork(f):
    pid = os.fork()
    if pid == 0:
        try:
            f()
        except OSError as e:
            print("raised", errno.errorcode[e.errno])
        except BaseException as e:
            print("raised", repr(e))
        finally:
            os._exit(0)
    return pid


def child(f):
    return os.waitpid(fork(f), 0)[1]


def link(kind, pid="self"):
    return os.readlink("/proc/%s/ns/%s" % (pid, kind))


def sysvipc(which):
    with open("/proc/sysvipc/" + which) as f:
        return [l.split()[:2] for l in f.readlines()[1:]]


def semop(sem, op, flg):
    buf = struct.pack("HhH", 0, op, flg)
    return c(libc.semop(sem, buf, 1))


name = socket.gethostname()


# A new UTS namespace copies the original names; changes inside are not seen outside; a name that
# is too long is EINVAL; without capabilities it is EPERM
def uts():
    os.unshare(CLONE_NEWUTS)
    print("copied", socket.gethostname() == name, "domain", os.uname().nodename == name)
    print("too long", err(socket.sethostname, "x" * 65))
    socket.sethostname("inside")
    print("domain", c(libc.setdomainname(b"dom", 3)), open("/proc/sys/kernel/domainname").read().strip())
    print("inside", socket.gethostname(), os.uname().nodename, open("/proc/sys/kernel/hostname").read().strip())
    r, w = os.pipe()
    pid = fork(lambda: (os.write(w, b"x"), time.sleep(30)))
    os.read(r, 1)
    print("child shares it", link("uts", pid) == link("uts"))
    os.kill(pid, 9)
    os.waitpid(pid, 0)


child(uts)
print("outside", socket.gethostname() == name, open("/proc/sys/kernel/domainname").read().strip())
child(lambda: (os.setuid(65534), print("unprivileged sethostname", err(socket.sethostname, "x"),
                                     "unshare", err(os.unshare, CLONE_NEWUTS))))


# A UTS namespace in a user namespace created by root: its owner is the new user namespace, and
# the name can be changed inside; after setns into it, its name is seen
def owned():
    os.unshare(CLONE_NEWUSER | CLONE_NEWUTS)
    socket.sethostname("owned")
    print("in a user namespace", socket.gethostname())


child(owned)


def joined():
    r, w = os.pipe()

    def other():
        os.unshare(CLONE_NEWUTS)
        socket.sethostname("other")
        os.write(w, b"x")
        time.sleep(30)

    pid = fork(other)
    os.read(r, 1)
    fd = os.open("/proc/%d/ns/uts" % pid, os.O_RDONLY)
    print("setns uts", err(os.setns, fd, CLONE_NEWUTS), socket.gethostname())
    os.kill(pid, 9)
    os.waitpid(pid, 0)


child(joined)


# IPC namespaces: System V IPC objects and POSIX message queues outside are not visible inside,
# and ids are numbered afresh; a segment attached with shmat can still be detached with shmdt
# after unshare; SEM_UNDO adjustments are applied when switching namespaces
shm = libc.shmget(KEY, 4096, IPC_CREAT | 0o600)
sem = libc.semget(KEY, 1, IPC_CREAT | 0o600)
msq = libc.msgget(KEY, IPC_CREAT | 0o600)
mq = libc.mq_open(b"/tuxport-ns", os.O_CREAT | os.O_RDONLY, 0o600, None)
print("outside objects", shm >= 0, sem >= 0, msq >= 0, mq >= 0)


step, go = os.pipe(), os.pipe()


def ipc():
    addr = libc.shmat(shm, None, 0)
    print("semop with undo", semop(sem, 1, SEM_UNDO), "value", libc.semctl(sem, 0, GETVAL))
    me = link("ipc")
    os.unshare(CLONE_NEWIPC)
    os.write(step[1], b"x")
    os.read(go[0], 1)
    print("new ns", link("ipc") != me)
    print("shmget", c(libc.shmget(KEY, 4096, 0)), "semget", c(libc.semget(KEY, 1, 0)),
          "msgget", c(libc.msgget(KEY, 0)), "mq_open", c(libc.mq_open(b"/tuxport-ns", os.O_RDONLY, 0, None)))
    print("sysvipc", sysvipc("shm"), sysvipc("sem"), sysvipc("msg"))
    mine = libc.shmget(0, 4096, IPC_CREAT | 0o600)
    print("first id", mine, "same key again", c(libc.shmget(KEY, 8192, IPC_CREAT | IPC_EXCL | 0o600)) >= 0)
    print("shmdt the old one", c(libc.shmdt(addr)))
    print("mount proc", libc.mount(b"proc", b"/tmp/ns/p", b"proc", 0, None))
    with open("/tmp/ns/p/sys/kernel/msgmax", "w") as f:
        f.write("1234")
    print("msgmax", open("/tmp/ns/p/sys/kernel/msgmax").read().strip())
    print("umount", libc.umount(b"/tmp/ns/p"))


pid = fork(ipc)
os.read(step[0], 1)
print("undone at unshare", libc.semctl(sem, 0, GETVAL))
os.write(go[1], b"x")
os.waitpid(pid, 0)
print("outside after: nattch",
      [l.split()[6] for l in open("/proc/sysvipc/shm") if l.split()[1:2] == [str(shm)]],
      "msgmax", open("/proc/sys/kernel/msgmax").read().strip())
print("clone NEWIPC with SYSVSEM", libc.syscall(220, CLONE_NEWIPC | CLONE_SYSVSEM | 17, 0, 0, 0, 0),
      errno.errorcode[ctypes.get_errno()])
child(lambda: (os.setuid(65534), print("unprivileged unshare", err(os.unshare, CLONE_NEWIPC))))


# setns into another process's IPC namespace: its objects are seen
def joined_ipc():
    r, w = os.pipe()

    def other():
        os.unshare(CLONE_NEWIPC)
        libc.msgget(KEY + 1, IPC_CREAT | 0o600)
        os.write(w, b"x")
        time.sleep(30)

    pid = fork(other)
    os.read(r, 1)
    fd = os.open("/proc/%d/ns/ipc" % pid, os.O_RDONLY)
    print("setns ipc", err(os.setns, fd, CLONE_NEWIPC), "msgget", libc.msgget(KEY + 1, 0) >= 0,
          "outer one", c(libc.msgget(KEY, 0)))
    os.kill(pid, 9)
    os.waitpid(pid, 0)


child(joined_ipc)
libc.shmctl(shm, IPC_RMID, None)
libc.semctl(sem, 0, IPC_RMID)
libc.msgctl(msq, IPC_RMID, None)
libc.mq_unlink(b"/tuxport-ns")
PY4

# Network namespaces: inside busybox's unshare -n there is only lo, down; after ip brings it up
# there are 127.0.0.1/8, ::1 and routes in the local table, and outside addresses are unreachable
busybox unshare -n sh -c '
    ip link show
    ip -o addr
    ip link set lo up
    ip link show
    ip -o addr
    ip route show table local
    sed 1,2d /proc/net/dev | cut -d: -f1
    ping -c 1 -W 1 10.0.0.1 2>&1 | tail -1
'

# ICMPv6 in a new network namespace: once lo is up, ::1 can be pinged (Tuxport's ICMPv6 sockets use
# the host's ::1)
busybox unshare -n sh -c 'ping6 -c 1 -W 1 ::1 2>&1 | tail -1; ip link set lo up; ping6 -c 1 ::1 | sed "s/ time=.*//; /round-trip/d"'

python3 - <<'PY5'
import ctypes, errno, functools, os, socket, struct, time

print = functools.partial(print, flush=True)

CLONE_NEWNET, CLONE_NEWUSER = 0x40000000, 0x10000000


def err(f, *args):
    try:
        f(*args)
        return "ok"
    except OSError as e:
        return errno.errorcode[e.errno]


def fork(f):
    pid = os.fork()
    if pid == 0:
        try:
            f()
        except OSError as e:
            print("raised", errno.errorcode[e.errno])
        except BaseException as e:
            print("raised", repr(e))
        finally:
            os._exit(0)
    return pid


def child(f):
    return os.waitpid(fork(f), 0)[1]


def tcp(family=socket.AF_INET):
    return socket.socket(family, socket.SOCK_STREAM)


def udp(family=socket.AF_INET):
    return socket.socket(family, socket.SOCK_DGRAM)


def lo_up():
    os.system("ip link set lo up")


def rtattr(ty, data):
    return struct.pack("=HH", 4 + len(data), ty) + data + b"\0" * (-len(data) % 4)


def rtnl(*msgs):
    """Send requests over NETLINK_ROUTE and return the error code in each request's ack."""
    s = socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, 0)
    s.bind((0, 0))
    out = []
    for seq, (ty, flags, body) in enumerate(msgs, 1):
        s.send(struct.pack("=LHHLL", 16 + len(body), ty, flags | 1, seq, 0) + body)
        r = s.recv(4096)
        out.append(struct.unpack("=i", r[16:20])[0] if struct.unpack("=H", r[4:6])[0] == 2 else None)
    return out


# bubblewrap's loopback_setup: RTM_NEWADDR adds 127.0.0.1/8, then RTM_NEWLINK brings lo up, both
# asking for an ack
NEWADDR = (20, 0x4 | 0x200 | 0x400, struct.pack("=BBBBI", socket.AF_INET, 8, 0x80, 254, 1)
           + rtattr(2, bytes([127, 0, 0, 1])) + rtattr(1, bytes([127, 0, 0, 1])))
NEWLINK = (16, 0x4, struct.pack("=BBHiII", 0, 0, 0, 1, 1, 1))


# A loopback port and an abstract address listened on outside: neither can be reached from inside
outer = tcp()
outer.bind(("127.0.0.1", 0))
outer.listen()
outer_port = outer.getsockname()[1]
outer_unix = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
outer_unix.bind("\0tuxport-netns")
outer_unix.listen()
me = os.readlink("/proc/self/ns/net")


def netns():
    os.unshare(CLONE_NEWNET)
    print("new ns", os.readlink("/proc/self/ns/net") != me)
    print("interfaces", socket.if_nameindex(), "lo", socket.if_nametoindex("lo"),
          "eth0", err(socket.if_nametoindex, "eth0"))
    # lo down: the wildcard address and 127.0.0.1 can be bound, but neither 127.0.0.1 nor ::1 can
    # be reached
    print("down: bind any", err(tcp().bind, ("0.0.0.0", 0)), "bind lo", err(tcp().bind, ("127.0.0.1", 0)),
          "connect", err(tcp().connect, ("127.0.0.1", 9)), "v6", err(tcp(socket.AF_INET6).connect, ("::1", 9)))
    r, w = os.pipe()
    pid = fork(lambda: (os.setuid(65534), os.write(w, str(rtnl(NEWADDR)).encode())))
    os.waitpid(pid, 0)
    print("unprivileged rtnl", os.read(r, 100).decode())
    print("bwrap's loopback_setup", rtnl(NEWADDR, NEWLINK), "again", rtnl(NEWADDR))
    # TCP over loopback: the addresses of both ends are 127.0.0.1
    srv = tcp()
    srv.bind(("127.0.0.1", 0))
    srv.listen()
    port = srv.getsockname()[1]
    c = tcp()
    c.connect(("127.0.0.1", port))
    conn, peer = srv.accept()
    c.sendall(b"hello")
    print("tcp", srv.getsockname()[0], c.getpeername() == ("127.0.0.1", port), peer[0],
          conn.getsockname() == c.getpeername(), conn.recv(5))
    # The wildcard address: getsockname is 0.0.0.0, and connecting to 127.0.0.1 works too
    any4 = tcp()
    any4.bind(("0.0.0.0", 0))
    any4.listen()
    print("any", any4.getsockname()[0], err(tcp().connect, ("127.0.0.1", any4.getsockname()[1])))
    # IPv6 ::1 and ::
    s6 = tcp(socket.AF_INET6)
    s6.bind(("::", 0))
    s6.listen()
    c6 = tcp(socket.AF_INET6)
    c6.connect(("::1", s6.getsockname()[1]))
    a6, p6 = s6.accept()
    print("v6", s6.getsockname()[0], p6[0], c6.getpeername()[0])
    # UDP and autobinding
    u = udp()
    u.bind(("127.0.0.1", 0))
    v = udp()
    v.sendto(b"x", u.getsockname())
    data, frm = u.recvfrom(10)
    print("udp", data, frm[0], frm == v.getsockname()[:0] or frm[1] == v.getsockname()[1], v.getsockname()[0])
    # Outside addresses are unreachable; the outside loopback ports and abstract addresses are a
    # separate set here
    print("outside", err(tcp().connect, ("10.0.0.1", 80)), err(udp().sendto, b"x", ("8.8.8.8", 53)),
          err(udp().connect, ("192.0.2.1", 9)), "bind", err(tcp().bind, ("10.0.0.1", 0)))
    print("outer port", err(tcp().connect, ("127.0.0.1", outer_port)))
    un = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    print("outer abstract", err(un.connect, "\0tuxport-netns"),
          "bind it here", err(socket.socket(socket.AF_UNIX, socket.SOCK_STREAM).bind, "\0tuxport-netns"))
    # /proc/net is this namespace's: dev has only lo, tcp has only the sockets here
    with open("/proc/self/net/dev") as f:
        print("dev", [l.split(":")[0].strip() for l in f.readlines()[2:]])
    with open("/proc/self/net/tcp") as f:
        listed = {l.split()[1] for l in f.readlines()[1:]}
    mine = {"0100007F:%04X" % port, "00000000:%04X" % any4.getsockname()[1],
            "0100007F:%04X" % c.getsockname()[1], "0100007F:%04X" % conn.getsockname()[1]}
    print("tcp listing has ours", mine <= listed, "not the outer one", "0100007F:%04X" % outer_port not in listed)
    # setns back to the original namespace: sockets created afterwards can connect to the outside
    # port
    fd = os.open("/proc/%d/ns/net" % os.getppid(), os.O_RDONLY)
    print("setns back", err(os.setns, fd, CLONE_NEWNET), err(tcp().connect, ("127.0.0.1", outer_port)),
          "old socket", err(c.send, b"y"))


child(netns)
child(lambda: (os.setuid(65534), print("unprivileged unshare", err(os.unshare, CLONE_NEWNET))))


# A network namespace in a user namespace created by root: its owner is the new user namespace,
# and lo can be configured inside
def owned():
    os.unshare(CLONE_NEWUSER | CLONE_NEWNET)
    lo_up()
    s = tcp()
    s.bind(("127.0.0.1", 0))
    s.listen()
    print("in a user namespace", err(tcp().connect, s.getsockname()))


child(owned)
PY5

# cgroup namespaces: only the ID differs, and the cgroup seen inside is still the root
python3 -c '
import os
me = os.readlink("/proc/self/ns/cgroup")
os.unshare(0x02000000)
print("cgroup:", open("/proc/self/cgroup").read().strip(), "different ns:", os.readlink("/proc/self/ns/cgroup") != me)
'

# bubblewrap's way of mounting: both the source and the mount point are given through
# /proc/self/fd/N, the mount point being a file in a tmpfs; the mount point in mountinfo is still
# that file's path (bwrap relies on this to find the mount and make it read-only)
python3 - <<'PY6'
import ctypes, os

libc = ctypes.CDLL(None, use_errno=True)
libc.mount.argtypes = [ctypes.c_char_p] * 3 + [ctypes.c_ulong, ctypes.c_void_p]
MS_BIND, MS_REC, MS_PRIVATE, MS_RDONLY, MS_REMOUNT = 0x1000, 0x4000, 1 << 18, 1, 32
os.unshare(0x20000)
libc.mount(None, b"/", None, MS_REC | MS_PRIVATE, None)
os.makedirs("/tmp/ns/m", exist_ok=True)
with open("/tmp/ns/src", "w") as f:
    f.write("source\n")
print("tmpfs", libc.mount(b"tmpfs", b"/tmp/ns/m", b"tmpfs", 0, None))
open("/tmp/ns/m/h", "w").close()
src = os.open("/tmp/ns/src", os.O_PATH)
dst = os.open("/tmp/ns/m/h", os.O_PATH)
print("bind by fd", libc.mount(b"/proc/self/fd/%d" % src, b"/proc/self/fd/%d" % dst, None, MS_BIND | MS_REC, None))
# What was opened before the bind is the covered file, not the mount root; only what is reopened
# after the bind is
print("remount the covered one", libc.mount(None, b"/proc/self/fd/%d" % dst, None, MS_BIND | MS_REMOUNT | MS_RDONLY, None))
top = os.open("/tmp/ns/m/h", os.O_PATH)
print("remount ro", libc.mount(None, b"/proc/self/fd/%d" % top, None, MS_BIND | MS_REMOUNT | MS_RDONLY, None))
print([l.split()[4:6] for l in open("/proc/self/mountinfo") if "/tmp/ns/m" in l])
print(open("/tmp/ns/m/h").read().strip())
PY6
rm -rf /tmp/ns
