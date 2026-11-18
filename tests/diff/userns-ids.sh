# docker-run-options: --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# IDs seen by processes in other user namespaces (from_kuid_munged): SO_PEERCRED, SCM_CREDENTIALS
# (declared credentials are checked by scm_check_creds; without a declaration they are the real
# IDs), the si_uid of signals (the sender fills it in by its own namespace, and it is mapped again
# when the target is in another namespace; so the SIGCHLD for a stop or continue is mapped once
# more, the one for an exit is not), waitid's si_uid, the owners of SysV IPC (IPC_STAT, IPC_SET,
# /proc/sysvipc), the uid in /proc/net/tcp and auxv's AT_UID. In the child's user namespace 0 is
# 1000 outside and 100 is 0 outside. Root of a user namespace may chmod a file whose owner is
# mapped there even when its group is not (inode_owner_or_capable)
python3 - <<'PY'
import ctypes, errno, functools, os, signal, socket, struct, sys

print = functools.partial(print, flush=True)
libc = ctypes.CDLL(None, use_errno=True)
CLONE_NEWUSER = 0x10000000
IPC_RMID, IPC_SET, IPC_STAT, IPC_CREAT = 0, 1, 2, 0o1000
SOCK = "/tmp/ids.sock"


def err(f, *args):
    try:
        r = f(*args)
        return "ok" if r is None else r
    except OSError as e:
        return errno.errorcode[e.errno]


def creds(pid, uid, gid):
    return [(socket.SOL_SOCKET, socket.SCM_CREDENTIALS, struct.pack("iII", pid, uid, gid))]


def recv_creds(s):
    msg, anc, _, _ = s.recvmsg(100, socket.CMSG_SPACE(12))
    for lvl, typ, data in anc:
        if (lvl, typ) == (socket.SOL_SOCKET, socket.SCM_CREDENTIALS):
            return msg, struct.unpack("iII", data)
    return msg, None


def peercred(s):
    return struct.unpack("iII", s.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))[1:]


def msgctl(qid, cmd, buf):
    if libc.msgctl(qid, cmd, buf) < 0:
        return errno.errorcode[ctypes.get_errno()]
    return "ok"


def owner(qid):
    buf = ctypes.create_string_buffer(120)
    r = msgctl(qid, IPC_STAT, buf)
    return r if r != "ok" else struct.unpack_from("5I", buf.raw, 4)


def set_owner(qid, uid, gid):
    buf = ctypes.create_string_buffer(120)
    msgctl(qid, IPC_STAT, buf)
    struct.pack_into("II", buf, 4, uid & 0xffffffff, gid & 0xffffffff)
    return msgctl(qid, IPC_SET, buf)


def sysvipc(keys):
    out = []
    with open("/proc/sysvipc/msg") as f:
        for line in f.read().splitlines()[1:]:
            w = line.split()
            if int(w[0]) in keys:
                out.append((hex(int(w[0])), w[7], w[8], w[9], w[10]))
    return sorted(out)


def tcp_uid(port):
    with open("/proc/net/tcp") as f:
        for line in f.read().splitlines()[1:]:
            w = line.split()
            if int(w[1].split(":")[1], 16) == port:
                return w[7]


def siginfo(info):
    return "%s code=%d uid=%d" % (signal.Signals(info.si_signo).name, info.si_code, info.si_uid)


# In the same namespace: without a declaration SCM_CREDENTIALS gives the real IDs (SO_PEERCRED the
# effective IDs); only one's own pid and one of the real, effective and saved IDs can be declared;
# root can declare anything, a nonexistent pid is ESRCH; a wrong length is EINVAL
a, b = socket.socketpair()
b.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
pid = os.fork()
if pid == 0:
    os.setresgid(5001, 6001, 6001)
    os.setresuid(5000, 6000, 6000)
    a.send(b"default")
    a.sendmsg([b"euid"], creds(os.getpid(), 6000, 6001))
    print("declare root", err(a.sendmsg, [b"x"], creds(os.getpid(), 0, 6001)),
          "declare parent", err(a.sendmsg, [b"x"], creds(os.getppid(), 5000, 5001)),
          "short", err(a.sendmsg, [b"x"], [(socket.SOL_SOCKET, socket.SCM_CREDENTIALS, struct.pack("ii", 1, 2))]))
    os._exit(0)
os.waitpid(pid, 0)
for _ in range(2):
    msg, c = recv_creds(b)
    print("received", msg, c[0] == pid, c[1:])
a.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
b.sendmsg([b"root"], creds(os.getpid(), 4242, 4343))
print("root declares", recv_creds(a)[1][1:], "missing pid", err(b.sendmsg, [b"x"], creds(999999, 0, 0)))
a.close()
b.close()

# A child process in another user namespace
try:
    os.unlink(SOCK)
except FileNotFoundError:
    pass
listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(SOCK)
os.chmod(SOCK, 0o777)
listener.listen(1)
pa, pc = socket.socketpair()
q1 = libc.msgget(0x1234, IPC_CREAT | 0o644)
q2 = libc.msgget(0x1235, IPC_CREAT | 0o644)
print("set q2 owner", set_owner(q2, 5000, 5001), "unmapped", set_owner(q1, -1, 0))
signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGCHLD})
r1, w1 = os.pipe()
r2, w2 = os.pipe()
# After continuing, the new program waits for the parent to collect the continue event with
# waitid before it exits: for a child that has exited it cannot be collected (ECHILD)
r3, w3 = os.pipe()
child = os.fork()
if child == 0:
    try:
        os.unshare(CLONE_NEWUSER)
        os.write(w1, b"x")
        os.read(r2, 1)
        print("inside", os.getresuid(), os.getresgid())
        os.setresgid(0, 0, 0)
        os.setresuid(0, 0, 0)
        print("after setresuid", os.getresuid(), os.getresgid())
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(SOCK)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
        print("child SO_PEERCRED", peercred(s))
        s.send(b"a")
        s.sendmsg([b"b"], creds(os.getpid(), 100, 100))
        print("declare unmapped", err(s.sendmsg, [b"x"], creds(os.getpid(), 7, 0)),
              "declare parent's pid", err(s.sendmsg, [b"x"], creds(os.getppid(), 0, 0)))
        print("child IPC_STAT", owner(q1), owner(q2))
        q3 = libc.msgget(0x1236, IPC_CREAT | 0o644)
        print("child IPC_SET", set_owner(q3, 7, 0), set_owner(q3, 100, 100), owner(q3))
        print("child sysvipc", sysvipc({0x1234, 0x1235, 0x1236}))
        t = socket.socket()
        t.bind(("127.0.0.1", 0))
        t.listen(1)
        port = t.getsockname()[1]
        print("child tcp uid", tcp_uid(port))
        pc.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
        rt = signal.SIGRTMIN
        signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGUSR1, signal.SIGUSR2, rt})
        os.write(w1, struct.pack("i", port))
        os.read(r2, 1)
        for _ in range(2):
            msg, c = recv_creds(pc)
            print("child received", msg, c[0] == os.getppid(), c[1:])
        for _ in range(3):
            print("child got", siginfo(signal.sigwaitinfo({signal.SIGUSR1, signal.SIGUSR2, rt})))
        # The new program's AT_UID, AT_EUID, AT_GID and AT_EGID; then stop, continue, exit
        os.set_inheritable(r3, True)
        code = ("import os, signal, struct, sys\n"
                "a = open('/proc/self/auxv', 'rb').read()\n"
                "v = dict(struct.iter_unpack('QQ', a))\n"
                "print('auxv', v[11], v[12], v[13], v[14], flush=True)\n"
                "os.kill(os.getpid(), signal.SIGSTOP)\n"
                "os.read(int(sys.argv[1]), 1)\n")
        os.execv(sys.executable, [sys.executable, "-c", code, str(r3)])
    except BaseException as e:
        print("child raised", repr(e))
    os._exit(1)

os.read(r1, 1)
for f in ("uid_map", "gid_map"):
    with open("/proc/%d/%s" % (child, f), "w") as m:
        m.write("0 1000 1\n100 0 1\n")
os.write(w2, b"x")
port = struct.unpack("i", os.read(r1, 4))[0]
conn, _ = listener.accept()
conn.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
print("parent SO_PEERCRED", peercred(conn))
for _ in range(2):
    msg, c = recv_creds(conn)
    print("parent received", msg, c[0] == child, c[1:])
print("parent tcp uid", tcp_uid(port))
q3 = libc.msgget(0x1236, 0)
print("parent IPC_STAT", owner(q3), "sysvipc", sysvipc({0x1234, 0x1235, 0x1236}))
pa.sendmsg([b"p"], [])
pa.sendmsg([b"q"], creds(os.getpid(), 4242, 4343))
os.kill(child, signal.SIGUSR1)
helper = os.fork()
if helper == 0:
    os.setresuid(5000, 1000, 1000)
    os.kill(child, signal.SIGUSR2)
    os._exit(0)
os.waitpid(helper, 0)
print("helper SIGCHLD", siginfo(signal.sigwaitinfo({signal.SIGCHLD})))
# rt_sigqueueinfo: si_uid is what the caller gives, by the caller's namespace
info = struct.pack("iiiiiIq", signal.SIGRTMIN, 0, -1, 0, os.getpid(), 0, 7).ljust(128, b"\0")
libc.syscall.argtypes = [ctypes.c_long, ctypes.c_long, ctypes.c_long, ctypes.c_char_p]
print("sigqueue", libc.syscall(138, child, signal.SIGRTMIN, info))
os.write(w2, b"x")
print("stop SIGCHLD", siginfo(signal.sigwaitinfo({signal.SIGCHLD})))
w = os.waitid(os.P_PID, child, os.WSTOPPED)
print("waitid stopped", w.si_code, w.si_uid)
os.kill(child, signal.SIGCONT)
print("continue SIGCHLD", siginfo(signal.sigwaitinfo({signal.SIGCHLD})))
w = os.waitid(os.P_PID, child, os.WCONTINUED)
print("waitid continued", w.si_code, w.si_uid)
os.write(w3, b"x")
print("exit SIGCHLD", siginfo(signal.sigwaitinfo({signal.SIGCHLD})))
w = os.waitid(os.P_PID, child, os.WEXITED)
print("waitid exited", w.si_code, w.si_uid, w.si_status)
for q in (q1, q2, q3):
    libc.msgctl(q, IPC_RMID, None)
os.unlink(SOCK)
PY

# Root in a user namespace can install a seccomp filter without no_new_privs: what it needs is
# CAP_SYS_ADMIN in the namespace it is in (Linux's seccomp_prepare_filter uses ns_capable), and
# after entering a new user namespace the capabilities in the boot-time one are gone; an ordinary
# user in the boot-time namespace cannot. The user namespaces are created as root: the native
# Ubuntu host has apparmor_restrict_unprivileged_userns on, so in one created by an ordinary user
# the capabilities are denied
cat > /tmp/sfilter.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>
int main(int argc, char **argv)
{
    if (argc > 1 && unshare(CLONE_NEWUSER) != 0) {
        printf("unshare: %s\n", strerror(errno));
        return 0;
    }
    // No kernel headers: BPF_RET | BPF_K returns SECCOMP_RET_ALLOW, SECCOMP_SET_MODE_FILTER is 1
    struct { uint16_t code; uint8_t jt, jf; uint32_t k; } allow = {0x06, 0, 0, 0x7fff0000};
    struct { unsigned short len; void *filter; } prog = {1, &allow};
    long r = syscall(SYS_seccomp, 1, 0, &prog);
    printf("%s: %s\n", argc > 1 ? "in a new user namespace" : "as is", r ? strerror(errno) : "ok");
    return 0;
}
C
gcc -o /tmp/sfilter /tmp/sfilter.c && su nobody -s /bin/sh -c /tmp/sfilter && /tmp/sfilter userns
cat > /tmp/ownsmapped.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int put(const char *path, const char *s) {
    int fd = open(path, O_WRONLY);
    int r = fd >= 0 && write(fd, s, strlen(s)) == (ssize_t)strlen(s) ? 0 : -1;
    close(fd);
    return r;
}

int main(void) {
    close(open("/tmp/owned", O_CREAT | O_WRONLY, 0644));
    chown("/tmp/owned", 1000, 1234);
    int go[2], ready[2];
    pipe(go);
    pipe(ready);
    pid_t pid = fork();
    if (pid == 0) {
        char c;
        if (unshare(CLONE_NEWUSER)) {
            printf("unshare: %s\n", strerror(errno));
            _exit(0);
        }
        write(ready[1], "r", 1);
        read(go[0], &c, 1);
        printf("chmod with the owner mapped, the group not: %s\n",
               chmod("/tmp/owned", 0600) == 0 ? "ok" : strerror(errno));
        _exit(0);
    }
    char c, path[64];
    read(ready[0], &c, 1);
    // Written by the parent, which has CAP_SETUID over the child's namespace
    snprintf(path, sizeof path, "/proc/%d/uid_map", pid);
    int u = put(path, "0 0 1\n1000 1000 1\n");
    snprintf(path, sizeof path, "/proc/%d/gid_map", pid);
    int g = put(path, "0 0 1\n");
    printf("maps written: %s\n", u == 0 && g == 0 ? "yes" : "no");
    write(go[1], "g", 1);
    waitpid(pid, NULL, 0);
    return 0;
}
C
gcc -o /tmp/ownsmapped /tmp/ownsmapped.c && /tmp/ownsmapped
