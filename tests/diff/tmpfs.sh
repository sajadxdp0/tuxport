# docker-run-options: --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# tmpfs: mount options, mountinfo, statfs, the files, directories, hard links, sockets, FIFOs and
# shared mappings inside it, EXDEV across mounts, the contents being gone after unmounting, and
# what is bound elsewhere and open files keeping it alive. Finally the root is switched in one
# process following bubblewrap's steps (without creating namespaces): a tmpfs as the base,
# pivot_root into it, read-only binds of the program directories, --dev, --proc, --tmpfs,
# unmount the old root, then pivot_root(".", "."). The native side is a docker container with
# CAP_SYS_ADMIN
cd /tmp
rm -rf t
mkdir -p t/a t/b t/c t/base

# Mounts under /tmp/t: IDs replaced by order of appearance, the directory seen, mount point,
# options, type, source, superblock options
show() {
    awk '$5 ~ /^\/tmp\/t/ {
        id[$1] = ++n
        for (i = 7; $i != "-"; i++) {}
        print id[$1], ($2 in id) ? id[$2] : "-", $4, $5, $6, $(i + 1), $(i + 2), $(i + 3)
    }' /proc/self/mountinfo
}

grep -w tmpfs /proc/filesystems
mount -t tmpfs tmpfs /tmp/t/a && echo "tmpfs $?"
mount -t tmpfs -o size=1m,mode=750,uid=12,gid=34,nr_inodes=100 sized /tmp/t/b && echo "with options $?"
mount -t tmpfs -o size=50%,mode=1777 half /tmp/t/c && echo "default size $?"
show
stat -c '%a %u %g' /tmp/t/a /tmp/t/b
stat -f -c %T /tmp/t/a
echo "own device: $([ "$(stat -c %d /tmp/t/a)" != "$(stat -c %d /tmp/t)" ] && [ "$(stat -c %d /tmp/t/a)" != "$(stat -c %d /tmp/t/b)" ] && echo yes)"
umount /tmp/t/c
# Symlinks of up to 128 bytes are stored in the inode and take no blocks (SHORT_SYMLINK_LEN of
# shmem_symlink), longer ones take a page
ln -s "$(printf %0127d 0)" /tmp/t/a/s127 && ln -s "$(printf %0128d 0)" /tmp/t/a/s128
stat -c '%s %b' /tmp/t/a/s127 /tmp/t/a/s128
rm /tmp/t/a/s127 /tmp/t/a/s128

python3 - <<'PY'
import ctypes, errno, mmap, os, socket, stat

libc = ctypes.CDLL(None, use_errno=True)
libc.mount.argtypes = [ctypes.c_void_p] * 5
A, B = "/tmp/t/a", "/tmp/t/b"


def attempt(name, f, *args):
    try:
        r = f(*args)
        print(name, "ok" if r is None or isinstance(r, int) else r)
    except OSError as e:
        print(name, errno.errorcode[e.errno])


def mount(source, target, fstype, data=None):
    r = libc.mount(source, target, fstype, 0, data)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


s = os.statvfs(B)
print("size", s.f_blocks * s.f_frsize, "free", s.f_bfree * s.f_frsize, "name max", s.f_namemax)
# Files, directories, hard links and renames inside it
with open(A + "/f", "w") as f:
    f.write("hello\n")
os.mkdir(A + "/d")
os.link(A + "/f", A + "/hard")
os.rename(A + "/f", A + "/d/f")
print("nlink", os.stat(A + "/hard").st_nlink, open(A + "/d/f").read().strip())
print("listing", sorted(os.listdir(A)))
attempt("rename across", os.rename, A + "/hard", "/tmp/t/hard")
attempt("link across", os.link, A + "/hard", B + "/hard")
attempt("rename to the other tmpfs", os.rename, A + "/hard", B + "/hard")
# Sockets, FIFOs, shared mappings
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(A + "/sock")
srv.listen(1)
cli = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
cli.connect(A + "/sock")
conn, _ = srv.accept()
cli.sendall(b"ping")
print("socket", conn.recv(4), stat.S_ISSOCK(os.stat(A + "/sock").st_mode))
os.mkfifo(A + "/fifo")
print("fifo", stat.S_ISFIFO(os.stat(A + "/fifo").st_mode))
fd = os.open(A + "/shared", os.O_RDWR | os.O_CREAT, 0o644)
os.ftruncate(fd, 4096)
m = mmap.mmap(fd, 4096, mmap.MAP_SHARED)
m[:5] = b"world"
m.flush()
print("shared mapping", os.pread(fd, 5, 0))
print("fstat device", os.fstat(fd).st_dev == os.stat(A).st_dev)
m.close()
os.close(fd)
for x in (conn, cli, srv):
    x.close()
# Option errors
print("unknown option", mount(b"tmpfs", b"/tmp/t/c", b"tmpfs", b"bogus=1"))
print("bad mode", mount(b"tmpfs", b"/tmp/t/c", b"tmpfs", b"mode=9"))
print("bad size", mount(b"tmpfs", b"/tmp/t/c", b"tmpfs", b"size=1q"))
print("uid -1", mount(b"tmpfs", b"/tmp/t/c", b"tmpfs", b"uid=4294967295"))
print("generic flags", mount(b"tmpfs", b"/tmp/t/c", b"tmpfs", b"rw,nr_blocks=0x10,inode32,huge=never"))
with open("/proc/self/mountinfo") as f:
    print("options", [l.split()[-1] for l in f if l.split()[4] == "/tmp/t/c"])
libc.umount2.argtypes = [ctypes.c_char_p, ctypes.c_int]
libc.umount2(b"/tmp/t/c", 0)
PY

cd /tmp/t/a/d && pwd -P && cd .. && pwd -P && cd .. && pwd -P
cd /tmp
# A read-only tmpfs
mount -t tmpfs -o ro tmpfs /tmp/t/c && echo "read-only $?"
touch /tmp/t/c/x 2>/dev/null; echo "touch $?"
umount /tmp/t/c
# After unmounting, the contents are gone
umount /tmp/t/b && mount -t tmpfs tmpfs /tmp/t/b && echo "again: $(ls -A /tmp/t/b | wc -l) entries"
umount /tmp/t/b
# A subdirectory bound elsewhere and open files keep it alive
mkdir /tmp/t/a/sub && echo kept >/tmp/t/a/sub/y
mount --bind /tmp/t/a/sub /tmp/t/c && echo "bind a subdirectory $?"
show
exec 3</tmp/t/a/d/f
umount /tmp/t/a 2>/dev/null; echo "busy $?"
umount -l /tmp/t/a && echo "detach $?"
echo "still bound: $(cat /tmp/t/c/y)"
echo "still open: $(cat <&3)"
exec 3<&-
umount /tmp/t/c && echo "umount the bind $?"
echo "left: $(show | wc -l)"
# Usage and limits (Linux's shmem_statfs, shmem_reserve_inode, shmem_inode_acct_block): statfs's
# blocks and inodes follow usage (the root directory and each name take one inode, hard links
# too; regular files count by pages, long symlinks take one page); filling it up is ENOSPC (when
# part can be written, it writes until full), and once inodes run out, creating files, mkdir,
# link and symlink are all ENOSPC; deleting and truncating free space again, and so does closing
# a deleted file written after its last name went, again and again. Remounting can
# change size= and nr_inodes=: a value smaller than what is already used, or a limit on what was
# unlimited, is EINVAL, and mode= and the like stay unchanged
mkdir -p /tmp/quota
mount -t tmpfs -o size=64k,nr_inodes=8,mode=700 quota /tmp/quota
python3 - <<'PY'
import errno, os
D = "/tmp/quota"
def usage(what):
    s = os.statvfs(D)
    print(what, "bsize", s.f_bsize, "blocks", s.f_blocks, "bfree", s.f_bfree, "bavail", s.f_bavail,
          "files", s.f_files, "ffree", s.f_ffree, "namemax", s.f_namemax)
def err(f, *args):
    try:
        r = f(*args)
        return "ok" if r is None else r
    except OSError as e:
        return errno.errorcode[e.errno]
usage("empty")
os.mkdir(D + "/d")
with open(D + "/d/a", "wb") as f:
    f.write(b"x" * 10000)
os.link(D + "/d/a", D + "/b")
os.symlink("short", D + "/s")
os.symlink("L" * 200, D + "/long")
usage("used")
# Filling up: what fits is written, then ENOSPC
fd = os.open(D + "/big", os.O_WRONLY | os.O_CREAT, 0o644)
print("write", err(os.write, fd, b"y" * 100000), "then", err(os.write, fd, b"y"))
print("size", os.fstat(fd).st_size)
usage("full")
af = os.open(D + "/b", os.O_WRONLY | os.O_APPEND)
print("pwrite inside", err(os.pwrite, fd, b"z" * 100, 0), "append to other", err(os.write, af, b"q" * 5000))
# A deleted file that is still open is freed on Linux only when it is closed, in Tuxport as soon
# as its last name is removed, so this is not compared
os.close(af)
print("fallocate", err(os.posix_fallocate, fd, 0, 200000))
os.ftruncate(fd, 4096)
usage("truncated")
os.close(fd)
# Running out of inodes
names = []
for i in range(10):
    r = err(os.mknod, D + "/n%d" % i)
    names.append(r)
print("mknod", names)
print("mkdir", err(os.mkdir, D + "/d2"), "link", err(os.link, D + "/b", D + "/b2"),
      "symlink", err(os.symlink, "x", D + "/s2"), "open", err(os.open, D + "/o", os.O_CREAT | os.O_WRONLY))
usage("no inodes")
os.unlink(D + "/b")
os.unlink(D + "/n0")
usage("unlinked")
os.unlink(D + "/d/a")
usage("last link")
os.rename(D + "/s", D + "/long")
usage("renamed over")
# Copying in with sendfile (busybox's cp uses it): it also writes until full; when inodes run out,
# bind of AF_UNIX is ENOSPC too
src = os.open("/bin/busybox", os.O_RDONLY)
dst = os.open(D + "/copy", os.O_WRONLY | os.O_CREAT, 0o644)
print("sendfile", err(os.sendfile, dst, src, None, 1 << 20), "then", err(os.sendfile, dst, src, None, 4096))
os.close(dst)
made = [n for n in range(10) if err(os.mknod, D + "/m%d" % n) == "ok"]
import socket
print("made", len(made), "bind", err(socket.socket(socket.AF_UNIX).bind, D + "/sock"))
usage("filled")
os.unlink(D + "/copy")
for n in made:
    os.unlink(D + "/m%d" % n)
# Creating and writing directly after deleting, without going through statfs: the freed space can
# be used as usual
fd = os.open(D + "/again", os.O_WRONLY | os.O_CREAT, 0o644)
print("after unlink", err(os.mknod, D + "/again2"), err(os.write, fd, b"a" * 40000))
os.close(fd)
os.unlink(D + "/again")
os.unlink(D + "/again2")
# A deleted file that is still open is written and closed again and again, each time more than
# half the instance: closed, it frees what it took, so the next one fits
for how in ("write", "fallocate"):
    for i in range(3):
        fd = os.open(D + "/gone", os.O_RDWR | os.O_CREAT, 0o644)
        os.unlink(D + "/gone")
        if how == "write":
            print("deleted file, write", i, err(os.write, fd, b"g" * 40000))
        else:
            print("deleted file, fallocate", i, err(os.posix_fallocate, fd, 0, 40000))
        os.close(fd)
usage("after the deleted files")
PY
grep ' /tmp/quota ' /proc/self/mountinfo | awk '{ for (i = 7; $i != "-"; i++) {} print $(i + 3) }'
for o in size=16k size=4k nr_inodes=2 nr_inodes=20,mode=755,inode32 nr_inodes=0 size=0 size=1m; do
    mount -o remount,$o /tmp/quota 2>/dev/null; echo "remount $o: $?"
    grep ' /tmp/quota ' /proc/self/mountinfo | awk '{ for (i = 7; $i != "-"; i++) {} print $(i + 3) }'
done
python3 -c 'import os; s = os.statvfs("/tmp/quota"); print(s.f_blocks, s.f_files, oct(os.stat("/tmp/quota").st_mode))'
umount /tmp/quota
mount -t tmpfs -o size=0,nr_inodes=0 unlimited /tmp/quota
python3 -c 'import os; s = os.statvfs("/tmp/quota"); print("unlimited", s.f_blocks, s.f_bfree, s.f_files, s.f_ffree)'
mount -o remount,size=1m /tmp/quota 2>/dev/null; echo "limit unlimited: $?"
mount -o remount,nr_inodes=10 /tmp/quota 2>/dev/null; echo "limit unlimited inodes: $?"
umount /tmp/quota

# Switch the root as bubblewrap does
cat >t/minibwrap.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static void check(int r, const char *what)
{
    if (r < 0) {
        printf("%s: %s\n", what, strerror(errno));
        fflush(stdout);
        exit(1);
    }
}

/* Like bwrap's bind_mount: after binding, add nosuid, nodev (except for devices) and ro to the
 * original flags */
static void bind(const char *src, const char *dest, int ro, int dev)
{
    check(mount(src, dest, NULL, MS_SILENT | MS_BIND | MS_REC, NULL), dest);
    unsigned long flags = MS_SILENT | MS_BIND | MS_REMOUNT | MS_NOSUID | MS_RELATIME;
    flags |= (ro ? MS_RDONLY : 0) | (dev ? 0 : MS_NODEV);
    check(mount("none", dest, NULL, flags, NULL), dest);
}

int main(int argc, char **argv)
{
    const char *base = "/tmp/t/base";
    check(mount(NULL, "/", NULL, MS_SILENT | MS_SLAVE | MS_REC, NULL), "slave");
    check(mount("tmpfs", base, "tmpfs", MS_NODEV | MS_NOSUID, NULL), "base");
    check(chdir(base), "chdir base");
    check(mkdir("newroot", 0755), "mkdir newroot");
    check(mount("newroot", "newroot", NULL, MS_SILENT | MS_MGC_VAL | MS_BIND | MS_REC, NULL), "newroot");
    check(mkdir("oldroot", 0755), "mkdir oldroot");
    check(syscall(SYS_pivot_root, base, "oldroot"), "pivot_root");
    check(chdir("/"), "chdir /");
    /* --ro-bind /bin, /lib, /usr */
    const char *dirs[] = {"bin", "lib", "usr"};
    for (int i = 0; i < 3; i++) {
        char src[64], dest[64];
        snprintf(src, sizeof src, "/oldroot/%s", dirs[i]);
        snprintf(dest, sizeof dest, "/newroot/%s", dirs[i]);
        check(mkdir(dest, 0755), dest);
        bind(src, dest, 1, 0);
    }
    /* --dev /dev */
    check(mkdir("/newroot/dev", 0755), "mkdir dev");
    check(mount("tmpfs", "/newroot/dev", "tmpfs", MS_NOSUID | MS_NODEV, "mode=0755"), "dev");
    const char *devs[] = {"null", "zero", "full", "random", "urandom", "tty"};
    for (int i = 0; i < 6; i++) {
        char src[64], dest[64];
        snprintf(src, sizeof src, "/oldroot/dev/%s", devs[i]);
        snprintf(dest, sizeof dest, "/newroot/dev/%s", devs[i]);
        check(close(creat(dest, 0444)), dest);
        bind(src, dest, 0, 1);
    }
    check(symlink("/proc/self/fd", "/newroot/dev/fd"), "fd");
    check(symlink("/proc/self/fd/0", "/newroot/dev/stdin"), "stdin");
    check(mkdir("/newroot/dev/shm", 0755), "shm");
    check(mkdir("/newroot/dev/pts", 0755), "pts");
    check(mount("devpts", "/newroot/dev/pts", "devpts", MS_NOSUID | MS_NOEXEC,
                "newinstance,ptmxmode=0666,mode=620"), "devpts");
    check(symlink("pts/ptmx", "/newroot/dev/ptmx"), "ptmx");
    /* --proc /proc */
    check(mkdir("/newroot/proc", 0755), "mkdir proc");
    check(mount("proc", "/newroot/proc", "proc", MS_NOSUID | MS_NOEXEC | MS_NODEV, NULL), "proc");
    bind("/newroot/proc/sys", "/newroot/proc/sys", 1, 0);
    /* --perms 0700 --tmpfs /tmp */
    check(mkdir("/newroot/tmp", 0755), "mkdir tmp");
    check(mount("tmpfs", "/newroot/tmp", "tmpfs", MS_NOSUID | MS_NODEV, "mode=0700"), "tmp");
    /* Unmount the old root, then switch once more */
    check(mount("oldroot", "oldroot", NULL, MS_SILENT | MS_REC | MS_PRIVATE, NULL), "private");
    check(umount2("oldroot", MNT_DETACH), "umount oldroot");
    check(chdir("/newroot"), "chdir newroot");
    check(syscall(SYS_pivot_root, ".", "."), "pivot_root .");
    check(umount2(".", MNT_DETACH), "umount .");
    check(chdir("/"), "chdir / again");
    execv(argv[1], argv + 1);
    check(-1, argv[1]);
    return 1;
}
C
gcc -O1 -static -o t/minibwrap t/minibwrap.c
exec /tmp/t/minibwrap /bin/busybox sh -c '
    echo "root: $(ls / | tr "\n" " ")"
    echo "dev: $(ls /dev | tr "\n" " ")"
    echo "cwd: $(pwd -P) $(readlink /proc/self/cwd) root: $(readlink /proc/self/root)"
    touch /bin/x 2>/dev/null; echo "touch /bin $?"
    echo hi >/tmp/f && echo "tmp: $(cat /tmp/f) $(stat -c %a /tmp) $(stat -f -c %T /tmp /dev | tr "\n" " ")"
    echo x >/dev/null && echo "dev/null ok, zero: $(head -c 4 /dev/zero | wc -c)"
    echo "old root gone: $(ls /oldroot 2>&1 | wc -l)"
    # For what is bound in from the old root the type is not compared (natively overlay), and for
    # the devices the options are not compared either (the source mounts differ)
    awk "{
        id[\$1] = ++n
        for (i = 7; \$i != \"-\"; i++) {}
        dev = \$5 ~ /^\/dev\/(null|zero|full|random|urandom|tty)\$/
        bound = dev || \$5 ~ /^\/(bin|lib|usr)\$/
        print id[\$1], (\$2 in id) ? id[\$2] : \"-\", \$5, dev ? \"-\" : \$6, bound ? \"-\" : \$(i + 1)
    }" /proc/self/mountinfo
'

