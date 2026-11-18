# docker-run-options: --privileged
# tuxport-options: --memory-limit none
# The small batch of the sixth map-bug triage: both sides start from the same top-level
# directories and an empty /tmp; a standard signal sent again while pending keeps the first
# sender's siginfo; a timerfd watched only by an EPOLLEXCLUSIVE interest wakes epoll_wait when it
# expires; a cgroup directory has the mode mkdir gave it and is searched by it; the POSIX ACL
# attributes are the only system.* ones, need the owner to change them, are validated, and an
# ACL that restates the mode is a chmod (without a memory limit, so no OOM check wakes Tuxport's
# timer thread on its own and the timerfd's expiry is what wakes it). Not compared: the mode after an ACL with named entries
# (Linux sets the group bits from the mask, Tuxport leaves the mode, as ACLs are not enforced)
set -e
cd /
for d in bin etc home lib media mnt opt run sbin srv tmp usr var; do
    [ -d "$d" ] && echo "/$d"
done
echo "/tmp has $(ls -A /tmp | wc -l) entries"
cd /tmp

cat > sig.c <<'C'
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void)
{
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGCHLD);
    sigprocmask(SIG_BLOCK, &s, NULL);
    pid_t first = fork();
    if (first == 0)
        _exit(1);
    /* Wait until the first child's SIGCHLD is pending before the second exits */
    for (;;) {
        sigset_t p;
        sigpending(&p);
        if (sigismember(&p, SIGCHLD))
            break;
        usleep(1000);
    }
    pid_t second = fork();
    if (second == 0)
        _exit(2);
    waitid(P_PID, second, NULL, WEXITED | WNOWAIT);
    siginfo_t info;
    sigwaitinfo(&s, &info);
    printf("SIGCHLD from the %s child, status %d\n",
           info.si_pid == first ? "first" : info.si_pid == second ? "second" : "unknown",
           info.si_status);
    sigset_t p;
    sigpending(&p);
    printf("still pending: %d\n", sigismember(&p, SIGCHLD));
    waitpid(first, NULL, 0);
    waitpid(second, NULL, 0);
    return 0;
}
C
gcc -O1 -o sig sig.c
./sig

cat > extimer.c <<'C'
#define _GNU_SOURCE
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

int main(void)
{
    int t = timerfd_create(CLOCK_MONOTONIC, 0);
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN | EPOLLEXCLUSIVE, .data.fd = t};
    epoll_ctl(ep, EPOLL_CTL_ADD, t, &ev);
    struct itimerspec its = {.it_value = {0, 200000000}};
    timerfd_settime(t, 0, &its, NULL);
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    int n = epoll_wait(ep, &ev, 1, 3000);
    clock_gettime(CLOCK_MONOTONIC, &b);
    double secs = (b.tv_sec - a.tv_sec) + (b.tv_nsec - a.tv_nsec) / 1e9;
    printf("exclusive timerfd: %d event%s, %s\n", n, n == 1 ? "" : "s",
           secs < 0.6 ? "at its expiry" : "late");
    return 0;
}
C
gcc -O1 -o extimer extimer.c
# The store's deferred commits of what was just written would wake Tuxport's timer thread too
sleep 1
./extimer

# cgroup directories: the mode from mkdir (with the umask), searched by it
cd /sys/fs/cgroup
rmdir private open 2>/dev/null || true
(umask 077 && mkdir private)
(umask 027 && mkdir open)
stat -c '%a %n' private open
su -s /bin/sh nobody -c 'cat /sys/fs/cgroup/private/cgroup.procs' 2>&1 | sed 's/^cat: //'
su -s /bin/sh nobody -c 'cd /sys/fs/cgroup/private' 2>&1 | sed 's/.*cd: //'
su -s /bin/sh nobody -c 'ls /sys/fs/cgroup/open' 2>&1 | sed 's/^ls: //'
rmdir private open
cd /tmp

# POSIX ACL attributes
touch acl-file
mkdir -p acl-dir
ln -sf acl-file acl-link
chmod 644 acl-file
python3 - <<'PY'
import errno, os, struct, subprocess

def acl(*entries):
    return struct.pack("<I", 2) + b"".join(struct.pack("<HHI", t, p, i) for t, p, i in entries)

USER_OBJ, USER, GROUP_OBJ, GROUP, MASK, OTHER = 1, 2, 4, 8, 0x10, 0x20
U = 0xFFFFFFFF
access, default = "system.posix_acl_access", "system.posix_acl_default"

def try_(what, f):
    try:
        r = f()
        print(f"{what}: ok" + ("" if r is None else f" {r}"))
    except OSError as e:
        print(f"{what}: {errno.errorcode[e.errno]}")

def get(path, name, follow=True):
    return os.getxattr(path, name, follow_symlinks=follow).hex()

f = "/tmp/acl-file"
try_("other system name", lambda: os.setxattr(f, "system.foo", b"x"))
try_("get other system name", lambda: get(f, "system.foo"))
try_("short header", lambda: os.setxattr(f, access, b"\2\0"))
try_("bad version", lambda: os.setxattr(f, access, struct.pack("<I", 1)))
try_("bad length", lambda: os.setxattr(f, access, acl((USER_OBJ, 6, U)) + b"\0"))
try_("bad tag", lambda: os.setxattr(f, access, acl((0x40, 6, U))))
try_("bad order", lambda: os.setxattr(f, access, acl((GROUP_OBJ, 4, U), (USER_OBJ, 6, U), (OTHER, 4, U))))
try_("named entry without mask", lambda: os.setxattr(f, access, acl((USER_OBJ, 6, U), (USER, 4, 1000), (GROUP_OBJ, 4, U), (OTHER, 4, U))))
try_("bad permissions", lambda: os.setxattr(f, access, acl((USER_OBJ, 8, U), (GROUP_OBJ, 4, U), (OTHER, 4, U))))
try_("default on a file", lambda: os.setxattr(f, default, acl((USER_OBJ, 6, U), (GROUP_OBJ, 4, U), (OTHER, 4, U))))
try_("remove default on a file", lambda: os.removexattr(f, default))
try_("remove a missing ACL", lambda: os.removexattr(f, access))
try_("symlink", lambda: os.setxattr("/tmp/acl-link", access, acl((USER_OBJ, 6, U), (GROUP_OBJ, 4, U), (OTHER, 4, U)), follow_symlinks=False))
try_("get on a symlink", lambda: get("/tmp/acl-link", access, False))
# Restating the mode: a chmod, nothing stored
try_("mode-only ACL", lambda: os.setxattr(f, access, acl((USER_OBJ, 7, 0), (GROUP_OBJ, 5, 0), (OTHER, 0, 0))))
print("mode", oct(os.stat(f).st_mode & 0o7777))
try_("get mode-only ACL", lambda: get(f, access))
# Named entries: stored, IDs undefined for the others
named = acl((USER_OBJ, 6, 0), (USER, 4, 1000), (GROUP_OBJ, 4, 0), (GROUP, 6, 1001), (MASK, 6, 0), (OTHER, 0, 0))
try_("named ACL", lambda: os.setxattr(f, access, named))
try_("get named ACL", lambda: get(f, access))
try_("listed", lambda: sorted(n for n in os.listxattr(f) if n.startswith("system.")))
try_("default on a directory", lambda: os.setxattr("/tmp/acl-dir", default, acl((USER_OBJ, 7, U), (GROUP_OBJ, 5, U), (OTHER, 5, U))))
try_("get default", lambda: get("/tmp/acl-dir", default))
try_("empty ACL removes", lambda: os.setxattr("/tmp/acl-dir", default, struct.pack("<I", 2)))
try_("get removed default", lambda: get("/tmp/acl-dir", default))
# Another user: EPERM to change, reading allowed
os.chmod("/tmp", 0o1777)
for what, code in [
    ("other user sets", f"os.setxattr('{f}', '{access}', bytes.fromhex('{acl((USER_OBJ, 6, U), (GROUP_OBJ, 4, U), (OTHER, 4, U)).hex()}'))"),
    ("other user removes", f"os.removexattr('{f}', '{access}')"),
    ("other user reads", f"print(len(os.getxattr('{f}', '{access}')), end=' ')"),
]:
    r = subprocess.run(["su", "-s", "/bin/sh", "nobody", "-c", f"python3 -c \"import os\n{code}\""],
                       capture_output=True, text=True)
    err = r.stderr.strip().splitlines()[-1] if r.stderr.strip() else ""
    print(f"{what}: {r.stdout.strip()} {err.split(':')[0] if err else 'ok'}".replace("  ", " "))
PY
rm -rf acl-file acl-dir acl-link sig sig.c extimer extimer.c
