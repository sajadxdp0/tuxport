# docker-run-options: --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# Bind mounts of directories and files in the store: `..`, getcwd and mountinfo through the mount
# point, read-only, noexec, nosuid, nodev and nosymfollow judged by the mount gone through, rename
# and link across mounts are EXDEV, --rbind brings along the mounts inside, MS_MOVE, and finally
# pivot_root replaces the root directory. The native side is a docker container with
# CAP_SYS_ADMIN whose root filesystem is overlay: only the mount points, the directories seen and
# the mount options are compared, not the filesystem types and sources
cd /tmp
rm -rf b
mkdir -p b/src/sub b/dst b/mv b/mv2 b/rb b/fd
echo hello >b/src/f
echo inner >b/src/sub/g
touch b/fd/t
mknod b/src/null c 1 3
cp /bin/busybox b/src/bb
ln -s f b/src/link
cat >b/euid.c <<'C'
#include <stdio.h>
#include <unistd.h>
int main(void) { printf("euid %d\n", geteuid()); return 0; }
C
gcc -O1 -static -o b/src/euid b/euid.c
chmod 4755 b/src/euid

# Mounts under /tmp/b: IDs replaced by order of appearance, the fourth column (the directory seen),
# mount point, options
show() {
    awk '$5 ~ /^\/tmp\/b/ {
        id[$1] = ++n
        print id[$1], ($2 in id) ? id[$2] : "-", $4, $5, $6
    }' /proc/self/mountinfo
}

mount --bind /tmp/b/src /tmp/b/dst && echo "bind $?"
show
cat /tmp/b/dst/f /tmp/b/dst/sub/g
ls /tmp/b/dst | tr '\n' ' ' && echo
echo "same device: $([ "$(stat -c %d /tmp/b/src)" = "$(stat -c %d /tmp/b/dst)" ] && echo yes)"
echo "same inode: $([ "$(stat -c %i /tmp/b/src/f)" = "$(stat -c %i /tmp/b/dst/f)" ] && echo yes)"
echo written >/tmp/b/dst/new && cat /tmp/b/src/new
# .. and getcwd through the mount point
cd /tmp/b/dst/sub && pwd -P && cd .. && pwd -P && cd .. && pwd -P
echo "cwd link: $(cd /tmp/b/dst/sub && readlink /proc/self/cwd)"
cd /tmp

# Bind a file onto a file
mount --bind /tmp/b/src/f /tmp/b/fd/t && echo "bind a file $?"
cat /tmp/b/fd/t
show | tail -n 1
umount /tmp/b/fd/t && echo "umount the file $?"

# --rbind brings along the mounts inside; with a mount inside it cannot be unmounted, and
# MNT_DETACH unmounts it together with the ones inside
mount -t proc proc /tmp/b/src/sub && echo "proc on sub $?"
mount --rbind /tmp/b/src /tmp/b/rb && echo "rbind $?"
show
head -c 5 /tmp/b/rb/sub/version && echo
echo "plain bind sees the directory: $(cat /tmp/b/dst/sub/g)"
umount /tmp/b/rb; echo "with a mount inside $?"
umount -l /tmp/b/rb && echo "detach $?"
umount /tmp/b/src/sub
show

python3 - <<'PY'
import ctypes, errno, mmap, os

libc = ctypes.CDLL(None, use_errno=True)
libc.mount.argtypes = [ctypes.c_void_p] * 3 + [ctypes.c_ulong, ctypes.c_void_p]
libc.umount2.argtypes = [ctypes.c_char_p, ctypes.c_int]
libc.mmap.restype = ctypes.c_void_p
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
libc.mprotect.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
libc.syscall.restype = ctypes.c_long

MS_RDONLY, MS_NOSUID, MS_NODEV, MS_NOEXEC = 1, 2, 4, 8
MS_REMOUNT, MS_BIND, MS_MOVE, MS_REC = 0x20, 0x1000, 0x2000, 0x4000
MS_NOSYMFOLLOW = 0x100
S, D = "/tmp/b/src", "/tmp/b/dst"


def err(e):
    return errno.errorcode[e.errno]


def attempt(name, f, *args):
    try:
        r = f(*args)
        print(name, "ok" if r is None or isinstance(r, (int, bool)) else r)
    except OSError as e:
        print(name, err(e))


def mount(source, target, fstype, flags):
    r = libc.mount(source, target, fstype, flags, None)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


def remount(flags):
    return mount(None, D.encode(), None, MS_REMOUNT | MS_BIND | flags)


def options():
    with open("/proc/self/mountinfo") as f:
        return [l.split()[5] for l in f if l.split()[4] == D]


# Across mounts: rename and link are EXDEV (also when the same filesystem is bound in two places),
# inside a mount they work as usual
attempt("rename across", os.rename, D + "/new", S + "/new2")
attempt("link across", os.link, D + "/new", S + "/hard")
attempt("rename inside", os.rename, D + "/new", D + "/new2")
attempt("link inside", os.link, D + "/new2", D + "/hard")
os.unlink(D + "/hard")
# Renaming or removing a mount point is EBUSY (through whichever mount)
attempt("rmdir the mount point", os.rmdir, D)
attempt("rename the mount point", os.rename, D, "/tmp/b/dst2")

# Read-only: modifications through this mount are all EROFS, the source side works as usual; a
# name that already exists reports EEXIST first, and device nodes can be written as usual
print("remount ro", remount(MS_RDONLY), options())
print("f_flag", hex(os.statvfs(D).f_flag), hex(os.statvfs(S).f_flag))
attempt("open for write", os.open, D + "/f", os.O_WRONLY)
attempt("open truncating", os.open, D + "/f", os.O_RDONLY | os.O_TRUNC)
attempt("create", os.open, D + "/created", os.O_WRONLY | os.O_CREAT, 0o644)
attempt("create existing, read only", lambda: os.close(os.open(D + "/f", os.O_RDONLY | os.O_CREAT)))
attempt("mkdir", os.mkdir, D + "/d")
attempt("mkdir existing", os.mkdir, D + "/sub")
attempt("unlink", os.unlink, D + "/f")
attempt("unlink missing", os.unlink, D + "/missing")
attempt("rmdir", os.rmdir, D + "/sub")
attempt("rename", os.rename, D + "/f", D + "/f3")
attempt("symlink", os.symlink, "f", D + "/l2")
attempt("symlink existing", os.symlink, "f", D + "/link")
attempt("mknod", os.mknod, D + "/fifo", 0o10644)
attempt("chmod", os.chmod, D + "/f", 0o600)
attempt("chown", os.chown, D + "/f", 1, 1)
attempt("utime", os.utime, D + "/f", (1, 1))
attempt("truncate", os.truncate, D + "/f", 0)
attempt("setxattr", os.setxattr, D + "/f", "user.x", b"1")
attempt("removexattr", os.removexattr, D + "/f", "user.x")
fd = os.open(D + "/f", os.O_RDONLY)
attempt("fchmod", os.fchmod, fd, 0o600)
attempt("fchown", os.fchown, fd, 1, 1)
attempt("futimes", os.utime, fd, (1, 1))
os.close(fd)
print("access W_OK", os.access(D + "/f", os.W_OK), os.access(D + "/null", os.W_OK))
attempt("read", lambda: open(D + "/f").read().strip())
attempt("write the device", lambda: os.write(os.open(D + "/null", os.O_WRONLY), b"x"))
attempt("write through the source", lambda: open(S + "/f", "a").write("more\n"))
attempt("bind of a read-only mount", mount, D.encode(), b"/tmp/b/mv", None, MS_BIND)
print("inherits ro", [l.split()[5] for l in open("/proc/self/mountinfo") if l.split()[4] == "/tmp/b/mv"])
libc.umount2(b"/tmp/b/mv", 0)
print("remount rw", remount(0), options())

# noexec: exec is EACCES, mapping executable is EPERM, and it cannot be mprotected executable later
# either
print("remount noexec", remount(MS_NOEXEC), options())
pid = os.fork()
if pid == 0:
    try:
        os.execv(D + "/bb", ["bb", "true"])
    except OSError as e:
        print("exec", err(e), flush=True)
    os._exit(1)
os.waitpid(pid, 0)
fd = os.open(D + "/bb", os.O_RDONLY)
p = libc.mmap(None, 4096, mmap.PROT_READ | mmap.PROT_EXEC, mmap.MAP_PRIVATE, fd, 0)
print("mmap exec", "ok" if p not in (None, 2**64 - 1) else errno.errorcode[ctypes.get_errno()])
p = libc.mmap(None, 4096, mmap.PROT_READ, mmap.MAP_PRIVATE, fd, 0)
r = libc.mprotect(p, 4096, mmap.PROT_READ | mmap.PROT_EXEC)
print("mprotect exec", "ok" if r == 0 else errno.errorcode[ctypes.get_errno()])
with open("/proc/self/smaps") as f:
    lines = f.read().split("\n")
for i, l in enumerate(lines):
    if l.startswith("%x-" % p):
        flags = next(x for x in lines[i:] if x.startswith("VmFlags")).split()
        print("may exec", "me" in flags, "may write", "mw" in flags)
os.close(fd)
print("exec from the source", os.system(S + "/bb true"))

# nosuid: the setuid bit has no effect; nodev: device nodes cannot be opened
print("remount nosuid,nodev", remount(MS_NOSUID | MS_NODEV), options())
for path in (S + "/euid", D + "/euid"):
    pid = os.fork()
    if pid == 0:
        os.setuid(65534)
        os.execv(path, [path])
    os.waitpid(pid, 0)
attempt("open the device", os.open, D + "/null", os.O_RDONLY)
attempt("open the device in the source", lambda: os.close(os.open(S + "/null", os.O_RDONLY)))

# nosymfollow: symlinks (including magic links) are not followed, readlink works as usual
print("remount nosymfollow", remount(MS_NOSYMFOLLOW), options())
attempt("follow", lambda: open(D + "/link").read().strip())
attempt("readlink", os.readlink, D + "/link")
attempt("follow in the source", lambda: open(S + "/link").read().strip())
print("remount plain", remount(0), options())

# MS_MOVE: moves together with the mounts inside; the source must be a mount root, and the target
# cannot be inside it
print("move", mount(D.encode(), b"/tmp/b/mv2", None, MS_MOVE))
print("moved", os.path.exists(D + "/f"), open("/tmp/b/mv2/f").read().split())
print("move a directory that is not a mount", mount(b"/tmp/b/mv2/sub", b"/tmp/b/mv", None, MS_MOVE))
print("move inside itself", mount(b"/tmp/b/mv2", b"/tmp/b/mv2/sub", None, MS_MOVE))
print("move the root", mount(b"/", b"/tmp/b/mv", None, MS_MOVE))
print("move back", mount(b"/tmp/b/mv2", D.encode(), None, MS_MOVE))

# pivot_root errors: needs CAP_SYS_ADMIN, new_root must be a mount root and not the mount the
# root directory is on, put_old must be under new_root
def pivot(new, old):
    r = libc.syscall(41, new, old)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


print("pivot, not a mount", pivot(b"/tmp/b/mv", b"/tmp/b/mv"))
print("pivot onto the root", pivot(b"/", b"/tmp"))
print("pivot, put_old outside", pivot(D.encode(), b"/tmp/b/mv"))
print("pivot, not a directory", pivot(D.encode(), (D + "/f").encode()))
pid = os.fork()
if pid == 0:
    os.setuid(65534)
    print("pivot, unprivileged", pivot(D.encode(), (D + "/sub").encode()), flush=True)
    os._exit(0)
os.waitpid(pid, 0)
PY
show
umount /tmp/b/dst && echo "umount $?"

# Finally replace the root directory with pivot_root: the new root is a directory bound onto
# itself, and the old root is mounted on old inside it; every process whose root directory is the
# old root switches over. Afterwards only the busybox in the new root is used
mkdir -p b/nr/bin b/nr/lib b/nr/old b/nr/proc b/nr/tmp
cp /bin/busybox b/nr/bin/
cp /lib/ld-musl-aarch64.so.1 b/nr/lib/
mount --bind /tmp/b/nr /tmp/b/nr && echo "bind onto itself $?"
cd /tmp/b/nr
/bin/busybox pivot_root . old && echo "pivot_root $?"
cd /
/bin/busybox echo "root: $(/bin/busybox ls / | /bin/busybox tr '\n' ' ')"
/bin/busybox echo "old: $(/bin/busybox ls /old/tmp/b | /bin/busybox tr '\n' ' ')"
/bin/busybox mount -t proc proc /proc && /bin/busybox echo "proc $?"
/bin/busybox echo "pwd: $(/bin/busybox pwd -P)"
/bin/busybox umount -l /old && /bin/busybox echo "umount old $?"
/bin/busybox awk '{ id[$1] = ++n; print id[$1], ($2 in id) ? id[$2] : "-", $4, $5 }' /proc/self/mountinfo
/bin/busybox echo "old is empty: $(/bin/busybox ls /old | /bin/busybox wc -l)"
