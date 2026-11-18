# docker-run-options: --privileged
# Mounts copied into a mount namespace owned by another user namespace are locked (Linux 6.1's
# lock_mnt_tree): there they cannot be unmounted, moved or used as pivot_root's new_root
# (EINVAL), a remount or mount_setattr cannot clear their ro, nodev, nosuid or noexec or change
# their access-time flags (EPERM), and a bind without MS_REC of a directory with a locked mount
# below is EINVAL; a bind's own root is not locked, pivot_root hands the root's lock to the new
# root, and the old root can then be detached. proc and sysfs need CAP_SYS_ADMIN over the owner of
# the caller's pid and network namespace, and a mount namespace not owned by the initial user
# namespace must already see the whole filesystem (mount_too_revealing): a locked mount over a
# non-empty directory of it is EPERM, one on an empty mount point (/sys/fs/cgroup) is not;
# cgroup2 needs CAP_SYS_ADMIN over the owner of the caller's cgroup namespace, and a remount without
# MS_BIND over the user namespace owning the instance. A lazy unmount, or
# the end of the namespace, leaves locked mounts on their detached parent (disconnect_mount), so an
# fd kept in the detached tree still sees what hides a path, while unlocked mounts are
# disconnected. The native side is privileged so that its /proc and /sys are rw and unmasked, as
# Tuxport's are.
set -e
cd /tmp
python3 - <<'PY'
import ctypes, errno, os, sys

libc = ctypes.CDLL(None, use_errno=True)
MS_RDONLY, MS_NOSUID, MS_NOATIME = 1, 2, 0x400
MS_REMOUNT, MS_BIND, MS_MOVE, MS_REC, MS_PRIVATE = 0x20, 0x1000, 0x2000, 0x4000, 1 << 18
MNT_DETACH = 2
SYS_pivot_root, SYS_mount_setattr = 41, 442
MOUNT_ATTR_RDONLY = 1


def res(r):
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


def b(s):
    return s.encode() if s is not None else None


def mount(src, tgt, fstype, flags, data=None):
    return res(libc.mount(b(src), b(tgt), b(fstype), ctypes.c_ulong(flags), b(data)))


def umount(tgt, flags=0):
    return res(libc.umount2(b(tgt), flags))


def pivot_root(new, old):
    return res(libc.syscall(SYS_pivot_root, b(new), b(old)))


def setattr(path, set_, clr):
    attr = (ctypes.c_uint64 * 4)(set_, clr, 0, 0)
    return res(libc.syscall(SYS_mount_setattr, -100, b(path), 0, attr, 32))


def show(*a):
    print(*a, flush=True)


def child(f):
    """Runs f in a child process, after its output so far."""
    pid = os.fork()
    if pid == 0:
        try:
            f()
        except BaseException as e:
            show("error", type(e).__name__, e)
        finally:
            os._exit(0)
    os.waitpid(pid, 0)


def enter_userns(extra=0):
    os.unshare(os.CLONE_NEWUSER | os.CLONE_NEWNS | extra)
    with open("/proc/self/setgroups", "w") as f:
        f.write("deny")
    with open("/proc/self/uid_map", "w") as f:
        f.write("0 0 1")
    with open("/proc/self/gid_map", "w") as f:
        f.write("0 0 1")


# The sandbox, set up in a private mount namespace of the initial user namespace: a tmpfs with a
# tmpfs below, a read-only bind and a tmpfs hiding a file
os.unshare(os.CLONE_NEWNS)
mount(None, "/", None, MS_REC | MS_PRIVATE)
os.makedirs("/tmp/lk", exist_ok=True)
show("base", mount("lk", "/tmp/lk", "tmpfs", 0))
for d in ("a", "ro", "hidden", "x", "p"):
    os.mkdir("/tmp/lk/" + d)
open("/tmp/lk/hidden/secret", "w").close()
show("a", mount("a", "/tmp/lk/a", "tmpfs", MS_NOSUID))
os.mkdir("/tmp/lk/a/old")
show("ro", mount("/tmp/lk/ro", "/tmp/lk/ro", None, MS_BIND))
show("ro remount", mount(None, "/tmp/lk/ro", None, MS_REMOUNT | MS_BIND | MS_RDONLY))
show("hide", mount("h", "/tmp/lk/hidden", "tmpfs", 0))


def locked():
    enter_userns()
    show("== in a new user namespace")
    show("umount hidden", umount("/tmp/lk/hidden"), umount("/tmp/lk/hidden", MNT_DETACH))
    show("remount ro rw", mount(None, "/tmp/lk/ro", None, MS_REMOUNT | MS_BIND))
    show("remount ro, noatime", mount(None, "/tmp/lk/ro", None, MS_REMOUNT | MS_BIND | MS_RDONLY | MS_NOATIME))
    show("remount ro, nosuid", mount(None, "/tmp/lk/ro", None, MS_REMOUNT | MS_BIND | MS_RDONLY | MS_NOSUID))
    show("remount a without nosuid", mount(None, "/tmp/lk/a", None, MS_REMOUNT | MS_BIND))
    show("setattr ro clear rdonly", setattr("/tmp/lk/ro", 0, MOUNT_ATTR_RDONLY))
    show("setattr a set rdonly", setattr("/tmp/lk/a", MOUNT_ATTR_RDONLY, 0))
    show("bind the tree", mount("/tmp/lk", "/tmp/lk/x", None, MS_BIND))
    show("rbind the tree", mount("/tmp/lk", "/tmp/lk/x", None, MS_BIND | MS_REC))
    show("still hidden", os.listdir("/tmp/lk/x/hidden"))
    show("umount the copy of hidden", umount("/tmp/lk/x/hidden"))
    show("detach the rbind", umount("/tmp/lk/x", MNT_DETACH))
    show("bind a", mount("/tmp/lk/a", "/tmp/lk/x", None, MS_BIND))
    show("umount the bind of a", umount("/tmp/lk/x"))
    show("move a", mount("/tmp/lk/a", "/tmp/lk/x", None, MS_MOVE))
    show("proc without a pid namespace", mount("proc", "/tmp/lk/p", "proc", 0))
    show("cgroup2 without a cgroup namespace", mount("cgroup2", "/tmp/lk/p", "cgroup2", 0))
    show("sysfs without a network namespace", mount("sysfs", "/tmp/lk/p", "sysfs", 0))
    show("pivot_root to a", pivot_root("/tmp/lk/a", "/tmp/lk/a/old"))
    show("new root", mount("n", "/tmp/lk/x", "tmpfs", 0))
    os.mkdir("/tmp/lk/x/old")
    os.chdir("/tmp/lk/x")
    show("pivot_root to a new tmpfs", pivot_root(".", "old"))
    os.chroot(".")
    show("detach the old root", umount("/old", MNT_DETACH))
    show("umount the new root", umount("/", MNT_DETACH))


child(locked)


def listing(fd, name):
    d = os.open(name, os.O_RDONLY | os.O_DIRECTORY, dir_fd=fd)
    try:
        return sorted(os.listdir(d))
    finally:
        os.close(d)


def lazy():
    enter_userns()
    show("== lazy unmounts in a new user namespace")
    os.mkdir("/tmp/lk/x/y")
    show("rbind the tree", mount("/tmp/lk", "/tmp/lk/x/y", None, MS_BIND | MS_REC))
    show("own mount on a", mount("own", "/tmp/lk/x/y/a/old", "tmpfs", 0))
    open("/tmp/lk/x/y/a/old/mine", "w").close()
    fd = os.open("/tmp/lk/x/y", os.O_RDONLY | os.O_DIRECTORY)
    show("detach the rbind", umount("/tmp/lk/x/y", MNT_DETACH))
    show("hidden through the detached tree", listing(fd, "hidden"))
    show("a through the detached tree", listing(fd, "a"))
    show("own mount through the detached tree", listing(fd, "a/old"))
    os.chdir(fd)
    show("dotdot from hidden", sorted(os.listdir("hidden/..")) == sorted(os.listdir(".")))
    os.close(fd)


child(lazy)


def dead_ns():
    import socket
    a, b = socket.socketpair()
    pid = os.fork()
    if pid == 0:
        try:
            enter_userns()
            fd = os.open("/tmp/lk", os.O_RDONLY | os.O_DIRECTORY)
            socket.send_fds(a, [b"x"], [fd])
        finally:
            os._exit(0)
    _, fds, _, _ = socket.recv_fds(b, 1, 1)
    os.waitpid(pid, 0)
    show("== after the namespace ended")
    show("hidden through its tree", listing(fds[0], "hidden"))
    os.close(fds[0])


child(dead_ns)


def own_cgroup():
    enter_userns(os.CLONE_NEWCGROUP)
    show("== with its own cgroup namespace")
    show("cgroup2", mount("cgroup2", "/tmp/lk/p", "cgroup2", 0))
    show("cgroup.procs", os.path.exists("/tmp/lk/p/cgroup.procs"))
    show("umount", umount("/tmp/lk/p"))


child(own_cgroup)


def own_proc_sysfs():
    enter_userns(os.CLONE_NEWPID | os.CLONE_NEWNET)
    pid = os.fork()
    if pid == 0:
        show("== with its own pid and network namespaces")
        show("proc", mount("proc", "/tmp/lk/p", "proc", 0))
        show("proc read-only", mount("proc", "/tmp/lk/p", "proc", MS_RDONLY))
        show("sysfs", mount("sysfs", "/tmp/lk/p", "sysfs", 0))
        os._exit(0)
    os.waitpid(pid, 0)


child(own_proc_sysfs)


# A remount without MS_BIND changes the instance, which needs CAP_SYS_ADMIN over the user namespace
# owning it (6.1's do_remount): root of a new user namespace cannot remount the root filesystem or
# a tmpfs of the initial one (a bind remount still changes its own mount), nor even a sysfs it
# mounted itself (sysfs and cgroup2 superblocks belong to the initial user namespace), but can
# remount a tmpfs it mounted; the outside's tmpfs and /sys stay writable
os.mkdir("/tmp/lk/s")
show("shared tmpfs", mount("s", "/tmp/lk/s", "tmpfs", 0))


def remount_owner():
    enter_userns(os.CLONE_NEWNET)
    show("== remounts in a new user namespace")
    show("remount the root ro", mount(None, "/", None, MS_REMOUNT | MS_RDONLY))
    show("remount the shared tmpfs ro", mount(None, "/tmp/lk/s", None, MS_REMOUNT | MS_RDONLY))
    show("bind remount it ro", mount(None, "/tmp/lk/s", None, MS_REMOUNT | MS_BIND | MS_RDONLY))
    show("own tmpfs", mount("o", "/tmp/lk/p", "tmpfs", 0))
    show("remount it ro", mount(None, "/tmp/lk/p", None, MS_REMOUNT | MS_RDONLY, "size=1m"))
    show("umount", umount("/tmp/lk/p"))
    show("own sysfs", mount("sysfs", "/tmp/lk/p", "sysfs", 0))
    show("remount it ro", mount(None, "/tmp/lk/p", None, MS_REMOUNT | MS_RDONLY))
    show("its statvfs ro", bool(os.statvfs("/tmp/lk/p").f_flag & os.ST_RDONLY))
    show("umount", umount("/tmp/lk/p"))


child(remount_owner)
open("/tmp/lk/s/written", "w").close()
show("outside: the shared tmpfs and /sys still writable",
     not os.statvfs("/tmp/lk/s").f_flag & os.ST_RDONLY,
     not os.statvfs("/sys").f_flag & os.ST_RDONLY)
# Something mounted over a non-empty directory of /proc and of /sys hides it
show("mask /proc/sys", mount("m", "/proc/sys", "tmpfs", 0))
show("mask /sys/class", mount("m", "/sys/class", "tmpfs", 0))
child(own_proc_sysfs)
PY
