# docker-run-options: --privileged
# The sequel to compat6.sh (difftest passes the whole script as one argument of sh -c, which must
# not exceed 128 KB): process_mrelease, /proc/<pid>/oom_score, /proc/swaps, /proc/locks, and ss
# reading /proc/net quietly when NETLINK_SOCK_DIAG refuses its queries, ETXTBSY for running a
# `#!` script that is open for writing, setuid programs run with a shared fs context, and the layout
# fields of /proc/<pid>/stat, the length limit of AF_UNIX datagram queues, PI futexes, and mremap
# at the map-count limit
set -e
cd /tmp
# Linux's swap_show header, then the host's swap areas, which a container sees too
cat /proc/swaps

python3 - <<'PY'
import ctypes, errno, os, signal, sys
libc = ctypes.CDLL(None, use_errno=True)
PIDFD_OPEN, MRELEASE = 434, 448

def call(nr, *args):
    r = libc.syscall(ctypes.c_long(nr), *[ctypes.c_long(a) for a in args])
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]

def pidfd(pid):
    return libc.syscall(ctypes.c_long(PIDFD_OPEN), ctypes.c_long(pid), ctypes.c_long(0))

def rss(pid):
    for line in open(f"/proc/{pid}/status"):
        if line.startswith("VmRSS:"):
            return int(line.split()[1])

# process_mrelease's errors: not a pidfd (a /proc/<pid> directory neither), flags, a process that
# is not exiting, a zombie, a reaped process
print("bad fd", call(MRELEASE, -1, 0), "stdin", call(MRELEASE, 0, 0))
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    signal.pause()
    os._exit(0)
fd = pidfd(pid)
dfd = os.open(f"/proc/{pid}", os.O_RDONLY | os.O_DIRECTORY)
print("flags", call(MRELEASE, fd, 1), "running", call(MRELEASE, fd, 0), "proc dir", call(MRELEASE, dfd, 0))
os.kill(pid, signal.SIGKILL)
os.waitid(os.P_PID, pid, os.WEXITED | os.WNOWAIT)
print("zombie", call(MRELEASE, fd, 0))
os.waitpid(pid, 0)
print("reaped", call(MRELEASE, fd, 0))
os.close(fd)
os.close(dfd)

# A process stopped at PTRACE_EVENT_EXIT after exit_group: its memory goes, its maps stay, and it
# then exits as it meant to
PTRACE_TRACEME, PTRACE_CONT, PTRACE_SETOPTIONS, PTRACE_O_TRACEEXIT = 0, 7, 0x4200, 0x40
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    data = b"x" * (64 << 20)
    libc.ptrace(PTRACE_TRACEME, 0, 0, 0)
    os.kill(os.getpid(), signal.SIGSTOP)
    os._exit(len(data) & 3)
_, st = os.waitpid(pid, 0)
print("stopped", os.WIFSTOPPED(st) and os.WSTOPSIG(st) == signal.SIGSTOP)
fd = pidfd(pid)
libc.ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACEEXIT)
libc.ptrace(PTRACE_CONT, pid, 0, 0)
_, st = os.waitpid(pid, 0)
print("exit event", st >> 8 == signal.SIGTRAP | 6 << 8)
maps, before = open(f"/proc/{pid}/maps").read(), rss(pid)
print("release", call(MRELEASE, fd, 0), "again", call(MRELEASE, fd, 0))
print("64 MB freed", before - rss(pid) > 60 << 10, "maps kept", open(f"/proc/{pid}/maps").read() == maps)
libc.ptrace(PTRACE_CONT, pid, 0, 0)
_, st = os.waitpid(pid, 0)
print("exited", os.WIFEXITED(st), os.WEXITSTATUS(st))
os.close(fd)

# oom_score: 1000 plus the share of RAM and swap the process uses and oom_score_adj, times 2/3; 0
# for oom_score_adj -1000 and for a process with no memory left (a zombie)
def score(pid="self"):
    return int(open(f"/proc/{pid}/oom_score").read())

def adj(v):
    with open("/proc/self/oom_score_adj", "w") as f:
        f.write(str(v))

print("oom_score mode", oct(os.stat("/proc/self/oom_score").st_mode & 0o7777))
print("small process", 666 <= score() < 700)
adj(1000)
print("adj 1000", score() >= 1332)
adj(-1000)
print("adj -1000", score())
adj(0)
# The thread's directory has both too, the same as the process's
t = f"/proc/self/task/{os.getpid()}"
print("task oom files", [oct(os.stat(f"{t}/{n}").st_mode & 0o7777) for n in ["oom_score", "oom_score_adj"]])
with open(f"{t}/oom_score_adj", "w") as f:
    f.write("500")
print("task's oom_score_adj sets the process's", open("/proc/self/oom_score_adj").read().strip(),
      "task's score is the process's", abs(int(open(f"{t}/oom_score").read()) - score()) <= 1)
adj(0)
r, w = os.pipe()
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    data = b"y" * (256 << 20)
    os.write(w, b"1")
    signal.pause()
    os._exit(len(data) & 3)
os.read(r, 1)
print("bigger child scores higher", score(pid) > score())
os.kill(pid, signal.SIGKILL)
os.waitid(os.P_PID, pid, os.WEXITED | os.WNOWAIT)
print("zombie", score(pid))
os.waitpid(pid, 0)
PY

# A vfork child is never the OOM killer's choice until it execs or exits (Linux's in_vfork: ending
# it frees nothing): its oom_score is 0, its waiting parent's is not
cat >vfork.c <<'C'
#include <time.h>
#include <unistd.h>
int main(void)
{
    if (vfork() == 0) {
        struct timespec t = {3, 0};
        nanosleep(&t, 0);
        _exit(0);
    }
    return 0;
}
C
gcc -o vfork vfork.c
./vfork &
p=$!
c=
for i in $(seq 50); do
    c=$(cat /proc/$p/task/$p/children 2>/dev/null || true)
    [ -z "$c" ] || break
    sleep 0.1
done
c=$(echo $c)
echo "vfork parent scores $([ "$(cat /proc/$p/oom_score)" -gt 0 ] && echo above 0), child $(cat /proc/$c/oom_score)"
wait $p
rm -f vfork vfork.c

python3 - <<'PY'
import fcntl, os, re, signal, struct, subprocess, sys
# Locks of each kind, some taken by a child, as /proc/locks lists them: the kind and type in their
# columns, who took them, the file by device and inode number as stat shows them, the range
signal.signal(signal.SIGIO, signal.SIG_IGN)
names = ["flock", "shared", "posix", "ofd", "lease", "child"]
files = {}
for n in names:
    open(f"/tmp/lk-{n}", "w").close()
    files[n] = open(f"/tmp/lk-{n}", "r" if n == "lease" else "r+")
ids = {}
for n, f in files.items():
    st = os.fstat(f.fileno())
    ids[f"{os.major(st.st_dev):02x}:{os.minor(st.st_dev):02x}:{st.st_ino}"] = n
fcntl.flock(files["flock"], fcntl.LOCK_EX)
fcntl.flock(files["shared"], fcntl.LOCK_SH)
fcntl.lockf(files["posix"], fcntl.LOCK_EX, 100, 10)
fcntl.lockf(files["posix"], fcntl.LOCK_SH, 0, 200)
fcntl.fcntl(files["ofd"], fcntl.F_OFD_SETLK, struct.pack("hhqqi", fcntl.F_WRLCK, 0, 5, 1, 0))
fcntl.fcntl(files["lease"], fcntl.F_SETLEASE, fcntl.F_RDLCK)
r, w = os.pipe()
r2, w2 = os.pipe()
sys.stdout.flush()
child = os.fork()
if child == 0:
    f, g = open("/tmp/lk-child", "r+"), open("/tmp/lk-shared")
    fcntl.lockf(f, fcntl.LOCK_EX, 0, 0)
    fcntl.flock(g, fcntl.LOCK_SH)
    # An open for writing breaks the lease (without waiting, being nonblocking)
    try:
        os.open("/tmp/lk-lease", os.O_WRONLY | os.O_NONBLOCK)
    except OSError as e:
        print("open of the leased file", e.strerror)
    sys.stdout.flush()
    os.write(w, b"1")
    os.read(r2, 1)
    os._exit(0)
os.read(r, 1)
lines = open("/proc/locks").read().splitlines()
# Numbered by position in the list (with gaps natively, for the locks of other pid namespaces)
nums = [int(l.split(":")[0]) for l in lines]
print("numbered", nums == sorted(set(nums)) and all(n > 0 for n in nums))
out = []
for l in lines:
    m = re.match(r"\d+: (.{17})(\S+) (-?\d+) (\S+) (.*)$", l)
    if not m or m.group(4) not in ids:
        continue
    kind, ty, pid, fid, rng = m.groups()
    who = {str(os.getpid()): "self", str(child): "child"}.get(pid, pid)
    out.append(f"{kind!r} {ty} {who} {ids[fid]} {rng}")
for l in sorted(out):
    print(" ", l)
# fdinfo's lock lines, numbered per file: the locks held through that open file (a POSIX lock
# through its table's), which lslocks reads to tell who holds OFD locks
who = {str(os.getpid()): "self", str(child): "child"}
for n in names:
    for l in open(f"/proc/self/fdinfo/{files[n].fileno()}").read().splitlines():
        m = re.match(r"lock:\t(\d+): (.{17})(\S+) (-?\d+) (\S+) (.*)$", l)
        if m:
            num, kind, ty, pid, fid, rng = m.groups()
            print(" ", n, num, repr(kind), ty, who.get(pid, pid), ids.get(fid), rng)
for pid in [os.getpid(), child]:
    p = subprocess.run(["lslocks", "-n", "-o", "COMMAND,TYPE,MODE,M,START,END,PATH", "-p", str(pid)],
                       capture_output=True, text=True)
    # In the reverse of /proc/locks' order, which Linux keeps newest first
    print("lslocks", who[str(pid)], sorted(" ".join(l.split()) for l in p.stdout.splitlines()),
          repr(p.stderr), p.returncode)
os.write(w2, b"1")
os.waitpid(child, 0)
fcntl.fcntl(files["lease"], fcntl.F_SETLEASE, fcntl.F_UNLCK)
for n in names:
    os.unlink(f"/tmp/lk-{n}")
PY

python3 - <<'PY'
import os, socket, subprocess
# NETLINK_SOCK_DIAG opens; ss lists the listeners, from it or from /proc/net, without errors
s = socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, 4)
s.close()
tcp = socket.socket()
tcp.bind(("127.0.0.1", 38211))
tcp.listen()
if os.path.exists("/tmp/ss.sock"):
    os.unlink("/tmp/ss.sock")
unix = socket.socket(socket.AF_UNIX)
unix.bind("/tmp/ss.sock")
unix.listen()
for args, col in [(["-tlnH"], 3), (["-xlH"], 4)]:
    p = subprocess.run(["ss"] + args, capture_output=True, text=True)
    mine = [l.split() for l in p.stdout.splitlines()]
    mine = [f[:2] + [f[col]] for f in mine if f[col] in ("127.0.0.1:38211", "/tmp/ss.sock")]
    print("ss", *args, mine, "stderr", repr(p.stderr), "exit", p.returncode)
os.unlink("/tmp/ss.sock")
PY

# A script open for writing cannot be run (do_open_execat's deny_write_access on the script itself,
# before its interpreter is looked at)
printf '#!/bin/sh\necho ran\n' > /tmp/etxt.sh
chmod 755 /tmp/etxt.sh
python3 - <<'PY'
import os
fd = os.open("/tmp/etxt.sh", os.O_WRONLY)
try:
    os.execv("/tmp/etxt.sh", ["etxt.sh"])
except OSError as e:
    print("script open for writing:", e.strerror)
os.close(fd)
PY
/tmp/etxt.sh
rm /tmp/etxt.sh

# A setuid program gains nothing while another process shares the caller's fs context (clone with
# CLONE_FS but not CLONE_THREAD), which could change its root, cwd or umask (LSM_UNSAFE_SHARE): run
# by nobody, it keeps nobody's effective ID; with a fs context of its own it gets root's
cat >euid.c <<'C'
#include <stdio.h>
#include <unistd.h>
int main(void)
{
    printf("euid %d\n", (int)geteuid());
    return 0;
}
C
cat >sharefs.c <<'C'
#define _GNU_SOURCE
#include <sched.h>
#include <signal.h>
#include <sys/wait.h>
#include <unistd.h>
static char stack[65536];
static int run(void *arg)
{
    (void)arg;
    execl("/tmp/euid", "euid", (char *)0);
    _exit(127);
}
int main(int argc, char **argv)
{
    (void)argv;
    int status;
    pid_t p = clone(run, stack + sizeof stack, SIGCHLD | (argc > 1 ? CLONE_FS : 0), 0);
    waitpid(p, &status, 0);
    return 0;
}
C
gcc -o euid euid.c
gcc -o sharefs sharefs.c
chmod 4755 euid
su -s /bin/sh nobody -c '/tmp/sharefs; /tmp/sharefs shared'
rm -f euid euid.c sharefs sharefs.c

# /proc/<pid>/stat's layout fields (6.1's do_task_stat): a process's own show where its code, stack,
# data, heap, arguments and environment are; another user's show 1 for the code's bounds and 0 for
# the rest, without the ptrace read check
cat >/tmp/statf.py <<'PY'
import os, sys
def stat(pid):
    s = open(f"/proc/{pid}/stat").read()
    post = s[s.rfind(")") + 2:].split()
    return lambda n: int(post[n - 3])
if len(sys.argv) > 1:
    f = stat(sys.argv[1])
    print("another user's process:", [f(n) for n in (26, 27, 28)], [f(n) for n in range(45, 52)])
    sys.exit()
f = stat("self")
code, stack = (f(26), f(27)), f(28)
data, brk, args, env = (f(45), f(46)), f(47), (f(48), f(49)), (f(50), f(51))
maps = [l.split() for l in open("/proc/self/maps")]
def within(a, pred):
    for m in maps:
        lo, hi = (int(x, 16) for x in m[0].split("-"))
        if lo <= a < hi:
            return pred(m)
    return False
exe = os.readlink("/proc/self/exe")
print("code in the program's executable mapping",
      within(code[0], lambda m: "x" in m[1] and m[-1] == exe), code[0] < code[1])
print("stack in [stack]", within(stack, lambda m: m[-1] == "[stack]"))
print("data from the code on", code[0] <= data[0] <= data[1], "heap after the data", brk >= data[1])
print("arguments, then the environment, on the stack", args[0] < args[1] <= env[0] < env[1],
      within(args[0], lambda m: m[-1] == "[stack]"))
PY
python3 /tmp/statf.py
sleep 5 &
sp=$!
su -s /bin/sh nobody -c "python3 /tmp/statf.py $sp"
kill $sp 2>/dev/null || true
rm -f /tmp/statf.py

# A datagram receiver not connected back to the sender holds net.unix.max_dgram_qlen + 1 messages,
# then the sender waits (EAGAIN when nonblocking) and does not poll writable until one is read
# (unix_recvq_full); a socket's own peer is held back only by bytes; a new socket takes the
# namespace's current limit
python3 - <<'PY'
import os, select, socket
def fill(s, send, most=100):
    n = 0
    try:
        while n < most:
            send(s)
            n += 1
    except BlockingIOError:
        pass
    return n
def receiver(path):
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass
    r = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
    r.bind(path)
    return r
r = receiver("/tmp/dq.sock")
s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
s.connect("/tmp/dq.sock")
s.setblocking(False)
print("queued before EAGAIN:", fill(s, lambda s: s.send(b"x")))
p = select.poll()
p.register(s, select.POLLOUT)
print("writable while full:", bool(p.poll(0)))
u = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
u.setblocking(False)
print("unconnected sendto:", fill(u, lambda u: u.sendto(b"y", "/tmp/dq.sock")))
r.recv(10)
print("writable after a read:", bool(p.poll(0)))
a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_DGRAM)
a.setblocking(False)
print("to its own peer:", fill(a, lambda a: a.send(b"z")))
q = "/proc/sys/net/unix/max_dgram_qlen"
old = open(q).read()
open(q, "w").write("3")
r2 = receiver("/tmp/dq2.sock")
s.close()
s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
s.setblocking(False)
print("with max_dgram_qlen 3:", fill(s, lambda s: s.sendto(b"x", "/tmp/dq2.sock")))
open(q, "w").write(old)
os.unlink("/tmp/dq.sock")
os.unlink("/tmp/dq2.sock")
PY

# PI futexes (FUTEX_LOCK_PI, TRYLOCK_PI, UNLOCK_PI), without priority inheritance itself: musl's
# PTHREAD_PRIO_INHERIT mutexes under contention; the errors (a word the caller owns EDEADLK,
# unlocking another's EPERM, an owner that does not exist ESRCH after FUTEX_WAITERS is set, a
# trylock EAGAIN, a deadline ETIMEDOUT); an owner whose robust list holds the lock dying makes the
# waiter's lock EOWNERDEAD, and one exiting without it hands the word over with FUTEX_OWNER_DIED too
cat >pi.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define FUTEX_LOCK_PI 6
#define FUTEX_UNLOCK_PI 7
#define FUTEX_TRYLOCK_PI 8
#define FUTEX_TID_MASK 0x3fffffff

static pthread_mutex_t m;
static long count;
static volatile unsigned word;
static volatile int ready;

static long futex(volatile unsigned *w, int op, struct timespec *t)
{
    return syscall(SYS_futex, w, op, 0, t, 0, 0) < 0 ? -errno : 0;
}

static void *add(void *a)
{
    for (int i = 0; i < 20000; i++) {
        pthread_mutex_lock(&m);
        count++;
        pthread_mutex_unlock(&m);
    }
    return a;
}

static void *hold(void *a)
{
    futex(&word, FUTEX_LOCK_PI, 0);
    ready = 1;
    usleep(300000);
    return a;
}

static void *die_robust(void *a)
{
    pthread_mutex_lock(&m);
    ready = 1;
    usleep(300000);
    return a;
}

int main(void)
{
    pthread_mutexattr_t at;
    pthread_mutexattr_init(&at);
    printf("PRIO_INHERIT: %d\n", pthread_mutexattr_setprotocol(&at, PTHREAD_PRIO_INHERIT));
    pthread_mutex_init(&m, &at);
    pthread_t t[4];
    for (int i = 0; i < 4; i++)
        pthread_create(&t[i], 0, add, 0);
    for (int i = 0; i < 4; i++)
        pthread_join(t[i], 0);
    printf("count %ld\n", count);
    unsigned tid = gettid();
    word = 0;
    printf("lock free: %ld, owner is me %d\n", futex(&word, FUTEX_LOCK_PI, 0), word == tid);
    printf("again: %ld\n", futex(&word, FUTEX_LOCK_PI, 0));
    printf("unlock: %ld, word %u\n", futex(&word, FUTEX_UNLOCK_PI, 0), word);
    word = 0x3ffffff0;
    printf("unlock another's: %ld\n", futex(&word, FUTEX_UNLOCK_PI, 0));
    printf("owner missing: %ld, waiters set %d\n", futex(&word, FUTEX_LOCK_PI, 0), word >> 31);
    word = 0;
    ready = 0;
    pthread_t h;
    pthread_create(&h, 0, hold, 0);
    while (!ready)
        usleep(1000);
    printf("trylock: %ld\n", futex(&word, FUTEX_TRYLOCK_PI, 0));
    struct timespec dl;
    clock_gettime(CLOCK_REALTIME, &dl);
    dl.tv_nsec += 50000000;
    if (dl.tv_nsec >= 1000000000) {
        dl.tv_sec++;
        dl.tv_nsec -= 1000000000;
    }
    printf("deadline: %ld\n", futex(&word, FUTEX_LOCK_PI, &dl));
    long r = futex(&word, FUTEX_LOCK_PI, 0);
    printf("after the owner exited: %ld, owner is me %d, waiters %d, owner died %d\n", r,
           (word & FUTEX_TID_MASK) == tid, word >> 31, (word >> 30) & 1);
    pthread_join(h, 0);
    pthread_mutexattr_setrobust(&at, PTHREAD_MUTEX_ROBUST);
    pthread_mutex_init(&m, &at);
    ready = 0;
    pthread_create(&h, 0, die_robust, 0);
    while (!ready)
        usleep(1000);
    r = pthread_mutex_lock(&m);
    printf("robust owner died: %s\n", r == EOWNERDEAD ? "EOWNERDEAD" : r ? "error" : "0");
    pthread_mutex_consistent(&m);
    pthread_mutex_unlock(&m);
    pthread_join(h, 0);
    return 0;
}
C
gcc -O1 -pthread -o pi pi.c
./pi
rm -f pi pi.c

# mremap at the map-count limit: a move that would need more VMAs than are left is refused with
# ENOMEM before anything changes (Linux's margin in move_vma and mremap_to), whether it moves the
# middle of a mapping, a whole one that cannot grow in place, to a fixed target or with
# MREMAP_DONTUNMAP, and the data stays at the old address; with room again the move goes ahead
cat > maplimit.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#ifndef MREMAP_DONTUNMAP
#define MREMAP_DONTUNMAP 4
#endif

static sigjmp_buf env;

static void on_fault(int sig)
{
    siglongjmp(env, sig);
}

static const char *peek(volatile char *p)
{
    static char buf[5];
    if (sigsetjmp(env, 1))
        return "fault";
    for (int i = 0; i < 4; i++)
        buf[i] = p[i];
    return buf;
}

static void report(const char *what, void *r)
{
    printf("%s: %s\n", what, r == MAP_FAILED ? strerror(errno) : "moved");
}

int main(void)
{
    signal(SIGSEGV, on_fault);
    signal(SIGBUS, on_fault);
    long pg = sysconf(_SC_PAGESIZE), max = 0;
    FILE *f = fopen("/proc/sys/vm/max_map_count", "r");
    fscanf(f, "%ld", &max);
    fclose(f);
    int rw = PROT_READ | PROT_WRITE, anon = MAP_PRIVATE | MAP_ANONYMOUS;
    char *a = mmap(NULL, 4 * pg, rw, anon, -1, 0);
    /* `w` cannot grow in place: a page of other permissions follows it */
    char *w = mmap(NULL, 3 * pg, rw, anon, -1, 0);
    mprotect(w + 2 * pg, pg, PROT_READ);
    char *target = mmap(NULL, 4 * pg, PROT_NONE, anon, -1, 0);
    munmap(target, 4 * pg);
    memcpy(a + pg, "keep", 4);
    memcpy(w, "keep", 4);
    /* Single pages with gaps between them, cut out of one reservation until munmap may not split
     * any more */
    size_t n = 2 * (size_t)(max + 64);
    char *fill = mmap(NULL, n * pg, PROT_NONE, anon | MAP_NORESERVE, -1, 0);
    printf("start\n");
    fflush(stdout);
    size_t i = 1;
    while (i < n && munmap(fill + i * pg, pg) == 0)
        i += 2;
    printf("filled: %s\n", i < n ? strerror(errno) : "no limit reached");
    report("middle", mremap(a + pg, 2 * pg, 3 * pg, MREMAP_MAYMOVE));
    report("whole", mremap(w, 2 * pg, 4 * pg, MREMAP_MAYMOVE));
    report("fixed", mremap(a + pg, 2 * pg, 2 * pg, MREMAP_MAYMOVE | MREMAP_FIXED, target));
    report("dontunmap", mremap(a + pg, 2 * pg, 2 * pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP, 0));
    printf("kept: %s, %s\n", peek(a + pg), peek(w));
    munmap(fill, 200 * pg);
    char *b = mremap(a + pg, 2 * pg, 3 * pg, MREMAP_MAYMOVE);
    report("with room", b);
    if (b != MAP_FAILED)
        printf("moved: %s, old address %s\n", peek(b), strcmp(peek(a + pg), "fault") ? "kept" : "gone");
    munmap(fill, n * pg);
    return 0;
}
C
gcc -O1 -o maplimit maplimit.c
./maplimit
rm -f maplimit maplimit.c
