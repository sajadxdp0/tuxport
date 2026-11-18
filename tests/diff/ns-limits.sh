# docker-run-options: --privileged
# Limits on namespaces per user namespace (Linux 6.1's ucounts, /proc/sys/user/max_*_namespaces):
# each user namespace has its own limits (INT_MAX in a new one), written by holders of
# CAP_SYS_RESOURCE in it; a namespace counts against every user namespace above the one it is
# created in, so a limit of 1 user namespace lets one be created below and nothing below that
# (bubblewrap's --disable-userns), and a limit of 0 on a kind makes creating it ENOSPC. Only
# user namespaces created by root are compared, and a nested one only for what is decided before
# Ubuntu's AppArmor userns restriction applies (the limit check comes first in create_user_ns).
set -e
cd /tmp
python3 - <<'PY'
import errno, os

NEWUSER, NEWNS, NEWPID, NEWNET = os.CLONE_NEWUSER, os.CLONE_NEWNS, os.CLONE_NEWPID, os.CLONE_NEWNET


def show(*a):
    print(*a, flush=True)


def err(f):
    try:
        r = f()
        return "ok" if r is None else r
    except OSError as e:
        return errno.errorcode[e.errno]


def limit(kind, value=None):
    path = f"/proc/sys/user/max_{kind}_namespaces"
    if value is None:
        with open(path) as f:
            return f.read().strip()

    def write():
        with open(path, "w") as f:
            f.write(value)

    return err(write)


def child(f):
    pid = os.fork()
    if pid == 0:
        try:
            f()
        except BaseException as e:
            show("error", type(e).__name__, e)
        finally:
            os._exit(0)
    os.waitpid(pid, 0)


def enter_userns():
    os.unshare(NEWUSER)
    with open("/proc/self/setgroups", "w") as f:
        f.write("deny")
    for m in ("uid_map", "gid_map"):
        with open(f"/proc/self/{m}", "w") as f:
            f.write("0 0 1")


show("initial limit set:", int(limit("user")) > 0)


def unprivileged():
    os.setgid(65534)
    os.setuid(65534)
    show("nobody writes", limit("user", limit("user")))


child(unprivileged)
show("root rewrites", limit("user", limit("user")))


def in_userns():
    enter_userns()
    show("== in a new user namespace")
    show("limits", [limit(k) for k in ("user", "mnt", "pid", "net", "uts", "ipc", "cgroup", "time")])
    show("too large", limit("user", "2147483648"), "negative", limit("user", "-1"))
    show("set user to 1", limit("user", "1"), limit("user"))
    r, w = os.pipe()
    first = os.fork()
    if first == 0:
        os.close(w)
        show("first nested", err(lambda: os.unshare(NEWUSER)))
        show("limit seen there", limit("user"))
        show("below the nested one", err(lambda: os.unshare(NEWUSER)))
        os.read(r, 1)
        os._exit(0)
    os.close(r)
    # The first one holds its namespace until the pipe closes
    import time
    time.sleep(0.5)
    child(lambda: show("second nested while the first lives", err(lambda: os.unshare(NEWUSER))))
    os.close(w)
    os.waitpid(first, 0)
    # Linux frees a user namespace after an RCU grace period and in a work queue, so its count
    # drops a moment after the last process in it is reaped

    def after():
        for _ in range(40):
            r = err(lambda: os.unshare(NEWUSER))
            if r != "ENOSPC":
                break
            time.sleep(0.05)
        show("after it is gone", r)

    child(after)
    show("set mnt, pid and net to 0", limit("mnt", "0"), limit("pid", "0"), limit("net", "0"))
    child(lambda: show("mount namespace", err(lambda: os.unshare(NEWNS))))
    child(lambda: show("pid namespace", err(lambda: os.unshare(NEWPID))))
    child(lambda: show("network namespace", err(lambda: os.unshare(NEWNET))))
    show("raised again", limit("mnt", "1"))
    child(lambda: show("mount namespace", err(lambda: os.unshare(NEWNS))))


child(in_userns)
show("initial limit kept:", int(limit("user")) > 1)
PY
