# docker-run-options: --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# mount and umount: procfs, sysfs, devpts and cgroup2 mounted on other directories, bind mounts
# of directories inside them, `..` and getcwd through mount points, mountinfo listed by the
# process's root directory, unmounting, errors like EBUSY, and the fs.mount-max limit. The
# native side is a docker
# container with CAP_SYS_ADMIN whose /sys is read-only and whose /proc/sys has a separate
# read-only bind, so only the new mounts are compared, and superblock options are not (a new
# devpts instance defaults to mode=600 on Linux)
cd /tmp
rm -rf mnt
mkdir -p mnt/proc mnt/sys mnt/pts mnt/b mnt/rb mnt/dir "mnt/sp ace"
touch mnt/f

# Mounts under /tmp/mnt: IDs replaced by order of appearance (a parent mounted elsewhere is
# written as -); device numbers and superblock options are not compared
show() {
    awk '$5 ~ /^\/tmp\/mnt/ {
        id[$1] = ++n
        for (i = 7; $i != "-"; i++) {}
        print id[$1], ($2 in id) ? id[$2] : "-", $4, $5, $6, $(i + 1), $(i + 2)
    }' /proc/self/mountinfo
}
# Which mount's device number it shares (new procfs and devpts instances have their own)
device() {
    d=$(stat -c %d "$1")
    for b in /proc /sys /sys/fs/cgroup /dev/pts /tmp/mnt/proc; do
        if [ "$b" != "$1" ] && [ "$d" = "$(stat -c %d $b)" ]; then
            echo "$1: same device as $b"
            return
        fi
    done
    echo "$1: a device of its own"
}

mount -t proc proc /tmp/mnt/proc && echo "proc $?"
mount -t sysfs sysfs /tmp/mnt/sys -o nosuid,nodev && echo "sysfs $?"
mount -t devpts devpts /tmp/mnt/pts -o noexec && echo "devpts $?"
# In a newly mounted sysfs fs/cgroup is empty, so cgroup2 has to be mounted separately (read-only)
echo "fs/cgroup: $(ls /tmp/mnt/sys/fs/cgroup | wc -l) entries"
mount -t cgroup2 -o ro cgroup /tmp/mnt/sys/fs/cgroup && echo "cgroup2 $?"
grep -qx memory.max /dev/stdin <<EOF && echo "cgroup2 has memory.max"
$(ls /tmp/mnt/sys/fs/cgroup)
EOF
mount -t proc other "/tmp/mnt/sp ace" && echo "space $?"
show
for p in /tmp/mnt/proc /tmp/mnt/sys /tmp/mnt/sys/fs/cgroup /tmp/mnt/pts; do
    device $p
done
grep -E '^(proc|sysfs|devpts|cgroup|other) /tmp/mnt' /proc/mounts | cut -d ' ' -f 1-3
echo "fs types: $(stat -f -c %t /tmp/mnt/proc /tmp/mnt/sys /tmp/mnt/pts /tmp/mnt/sys/fs/cgroup | tr '\n' ' ')"
echo "ptmx: $(ls /tmp/mnt/pts | grep -c ptmx)"
head -c 5 /tmp/mnt/proc/version && echo

# .. and getcwd go back up a level through mount points
cd /tmp/mnt/proc/self && /bin/pwd -P | sed "s/$$/PID/" && cd ../.. && pwd -P
cd /tmp/mnt/sys/fs/cgroup && cd .. && pwd -P && cd ../.. && pwd -P
cd /tmp
echo "proc/self/cwd: $(cd /tmp/mnt/sys/fs && readlink /proc/self/cwd)"

# Same instance: sysfs and cgroup2 cannot be mounted again on the root of their own mount, while
# procfs and devpts are a new instance each time
mount -t sysfs sysfs /tmp/mnt/sys; echo "sysfs again $?"
mount -t cgroup2 -o ro cgroup /tmp/mnt/sys/fs/cgroup; echo "cgroup2 again $?"
mount -t proc proc /tmp/mnt/proc; echo "proc again $?"
show | tail -n 1
umount /tmp/mnt/proc; echo "umount the top $?"
mount -t tuxportfs nosuchsource /tmp/mnt/dir; echo "unknown type $?"
mount -t proc proc /tmp/mnt/f; echo "on a file $?"

# bind: directories and files inside procfs, --rbind brings along the mounts inside
mount --bind /tmp/mnt/proc/sys /tmp/mnt/b && echo "bind $?"
ls /tmp/mnt/b | grep -x kernel
mount --bind /tmp/mnt/proc/version /tmp/mnt/f && echo "bind a file $?"
head -c 5 /tmp/mnt/f && echo
mount --bind /tmp/mnt/proc/version /tmp/mnt/dir; echo "file on a directory $?"
mount --rbind /tmp/mnt/sys /tmp/mnt/rb && echo "rbind $?"
show
device /tmp/mnt/b
device /tmp/mnt/rb
device /tmp/mnt/rb/fs/cgroup

# A mount in use cannot be unmounted, and a mount point cannot be removed or renamed
rmdir /tmp/mnt/b; echo "rmdir $?"
mv /tmp/mnt/b /tmp/mnt/b2; echo "rename $?"
rm /tmp/mnt/f; echo "unlink $?"
(cd /tmp/mnt/b && umount /tmp/mnt/b; echo "cwd inside $?")
umount /tmp/mnt/rb; echo "with a mount inside $?"
umount /tmp/mnt/rb/fs/cgroup /tmp/mnt/rb /tmp/mnt/b /tmp/mnt/f; echo "umount $?"
umount /tmp/mnt/dir; echo "not a mount point $?"

# Remount read-only: writing files is EROFS
mount -o remount,ro /tmp/mnt/proc && echo "remount $?"
show | head -n 1
echo x 2>/dev/null >/tmp/mnt/proc/self/comm; echo "write $?"
mount -o remount,rw /tmp/mnt/proc && echo "remount rw $?"

python3 - <<'PY'
import ctypes, errno, os, sys

libc = ctypes.CDLL(None, use_errno=True)
libc.mount.argtypes = [ctypes.c_void_p] * 3 + [ctypes.c_ulong, ctypes.c_void_p]
libc.umount2.argtypes = [ctypes.c_char_p, ctypes.c_int]


def call(f, *args):
    r = f(*args)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


def mount(source, target, fstype, flags=0, data=None):
    return call(libc.mount, source, target, fstype, flags, data)


d = b"/tmp/mnt/py"
os.mkdir(d)
print("mount", mount(b"proc", d, b"proc"))
# Mount IDs match fdinfo's mnt_id
fd = os.open(d + b"/self/status", os.O_RDONLY)
with open("/proc/self/fdinfo/%d" % fd) as f:
    mnt = [l.split()[1] for l in f if l.startswith("mnt_id:")]
with open("/proc/self/mountinfo") as f:
    ids = [l.split()[0] for l in f if l.split()[4] == "/tmp/mnt/py"]
print("mnt_id matches mountinfo", ids == mnt)
os.close(fd)
# statfs's f_flags follow the mount flags (ST_VALID, ST_RELATIME, ST_RDONLY and so on)
print("f_flag /", hex(os.statvfs("/").f_flag))
print("f_flag proc", hex(os.statvfs(d).f_flag))
print("f_flag noexec", hex(os.statvfs("/tmp/mnt/pts").f_flag))
print("f_flag cgroup2", hex(os.statvfs("/tmp/mnt/sys/fs/cgroup").f_flag))
print("remount,bind,ro", mount(None, d, None, 0x20 | 0x1000 | 1))
print("f_flag after", hex(os.statvfs(d).f_flag))
print("remount,bind,noexec", mount(None, d, None, 0x20 | 0x1000 | 8))
with open("/proc/self/mountinfo") as f:
    print("options", [l.split()[5] for l in f if l.split()[4] == "/tmp/mnt/py"])
print("remount,bind,strictatime", mount(None, d, None, 0x20 | 0x1000 | (1 << 24)))
with open("/proc/self/mountinfo") as f:
    print("options", [l.split()[5] for l in f if l.split()[4] == "/tmp/mnt/py"])
# Propagation types
print("private", mount(None, d, None, 1 << 18))
print("rprivate", mount(None, d, None, (1 << 18) | 0x4000))
print("private and slave", mount(None, d, None, (1 << 18) | (1 << 19)))
print("private with nosuid", mount(None, d, None, (1 << 18) | 2))
print("private, not a mount point", mount(None, b"/tmp/mnt", None, 1 << 18))
print("remount, not a mount point", mount(None, b"/tmp/mnt", None, 0x20))
# Arguments
print("no type", mount(b"x", d, None))
print("MS_NOUSER", mount(b"proc", d, b"proc", 1 << 31))
print("bad type pointer", mount(b"proc", d, 8))
print("bad data pointer", mount(b"proc", d, b"proc", 0, 8))
print("missing target", mount(b"proc", b"/tmp/mnt/none", b"proc"))
print("bind without source", mount(None, d, None, 0x1000))
print("bind a missing source", mount(b"/tmp/mnt/none", d, None, 0x1000))
# Through /proc/self/fd onto deleted files and pipes. Mounting on the /proc/self/fd/<n> entry
# itself (LTP's mount08) is not compared: from Linux 6.12 on it is ENOENT, and the native VM runs
# 6.8
fd = os.open("/tmp/mnt/foo", os.O_RDONLY | os.O_CREAT)
os.unlink("/tmp/mnt/foo")
print("on a deleted file", mount(d + b"/version", ("/proc/self/fd/%d" % fd).encode(), None, 0x1000))
r, w = os.pipe()
print("on a pipe", mount(d + b"/version", ("/proc/self/fd/%d" % r).encode(), None, 0x1000))
for x in (fd, r, w):
    os.close(x)
# MNT_EXPIRE: the first call only marks it
print("expire", call(libc.umount2, d, 4))
print("expire again", call(libc.umount2, d, 4))
print("mount", mount(b"proc", d, b"proc"))
print("unknown umount flag", call(libc.umount2, d, 16))
print("expire and detach", call(libc.umount2, d, 6))
# Without CAP_SYS_ADMIN
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    os.setuid(65534)
    print("unprivileged mount", mount(b"proc", d, b"proc"))
    print("unprivileged umount", call(libc.umount2, d, 0), flush=True)
    os._exit(0)
os.waitpid(pid, 0)
# MNT_DETACH: it can be unmounted even with the current directory inside, after which the getcwd
# system call starts with (unreachable) (musl treats it as ENOENT), /proc/self/cwd is the path
# from the root of the unmounted mount, and files can still be read
os.chdir(b"/tmp/mnt/py/self")
print("busy", call(libc.umount2, d, 0))
print("detach", call(libc.umount2, d, 2))
buf = ctypes.create_string_buffer(4096)
n = libc.syscall(17, buf, 4096)
print("getcwd syscall", buf.value.replace(str(os.getpid()).encode(), b"PID") if n > 0 else n)
try:
    os.getcwd()
except FileNotFoundError:
    print("getcwd ENOENT")
print("cwd link", os.readlink("/proc/self/cwd").replace(str(os.getpid()), "PID"))
with open("comm") as f:
    print("comm", f.read().strip())
os.chdir("..")
print("still a directory", os.path.isdir("self"))
os.chdir("/")
print("gone", call(libc.umount2, d, 0))
PY

# chroot in and mount procfs and devpts: mountinfo lists only what is below the root directory,
# with paths relative to it; ttyname finds a terminal opened through the newly mounted devpts
# (the name in /proc/self/fd matches fstat's device number and inode)
mkdir -p mnt/root/bin mnt/root/lib mnt/root/proc mnt/root/dev/pts
cp /bin/busybox mnt/root/bin/
cp /lib/ld-musl-aarch64.so.1 mnt/root/lib/
ln -s pts/ptmx mnt/root/dev/ptmx
cat > mnt/pty.c <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void)
{
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0 || grantpt(m) || unlockpt(m)) {
        perror("ptmx");
        return 1;
    }
    char *name = ptsname(m);
    int s = open(name, O_RDWR | O_NOCTTY);
    struct stat a, b;
    if (s < 0 || stat(name, &a) || fstat(s, &b)) {
        perror(name);
        return 1;
    }
    char *tty = ttyname(s);
    printf("ttyname %s\n", tty && strcmp(tty, name) == 0 ? "is ptsname" : "fails");
    printf("same device and inode %d\n", a.st_dev == b.st_dev && a.st_ino == b.st_ino);
    return 0;
}
C
gcc -O1 -static -o mnt/root/bin/pty mnt/pty.c
chroot /tmp/mnt/root /bin/busybox sh -c '
    /bin/busybox mount -t proc proc /proc && echo "proc in chroot $?"
    /bin/busybox mount -t devpts devpts /dev/pts && echo "devpts in chroot $?"
    /bin/pty
    /bin/busybox awk "{ print \$4, \$5, \$6, \$8, \$9 }" /proc/self/mountinfo
    /bin/busybox cut -d " " -f 1-3 /proc/self/mounts
    cd /proc/self && /bin/busybox pwd -P | /bin/busybox sed "s/[0-9][0-9]*/PID/" && cd ../.. && /bin/busybox pwd -P
    echo "root: $(/bin/busybox readlink /proc/self/root)"
    # Outside the root directory: as with d_path in Linux, the path counts from the real root,
    # without (unreachable)
    echo "outside cwd: $(/bin/busybox readlink /proc/$PPID/cwd)"
    echo "outside root: $(/bin/busybox readlink /proc/$PPID/root)"
    echo "processes: $(/bin/busybox ps -o comm | /bin/busybox grep -c busybox)"
'
grep -c ' /tmp/mnt/root/proc ' /proc/self/mountinfo
umount /tmp/mnt/root/proc /tmp/mnt/root/dev/pts /tmp/mnt/sys/fs/cgroup /tmp/mnt/sys /tmp/mnt/pts /tmp/mnt/proc "/tmp/mnt/sp ace"
echo "left: $(show | wc -l)"
rm -rf /tmp/mnt

# Removing a mount point in another mount namespace: only mounts in one's own namespace count as
# EBUSY (Linux's is_local_mountpoint), and after removal the mounts on it elsewhere are unmounted
# (detach_mounts)
mkdir -p /tmp/mp/d /tmp/mp/e
mount -t tmpfs t /tmp/mp/e
rmdir /tmp/mp/e 2>&1 | sed 's/^rmdir: //'
umount /tmp/mp/e
unshare -m sh -c 'mount -t tmpfs t /tmp/mp/d && touch /tmp/mp/d/x && sleep 1
    echo "the other namespace: $(grep -c " /tmp/mp/d " /proc/self/mountinfo) mounts, $(ls /tmp/mp 2>&1)"' &
sleep 0.4
rmdir /tmp/mp/d && echo "rmdir a mount point of another namespace: ok"
wait
rm -rf /tmp/mp

# At boot /dev is a tmpfs, with devpts, the tmpfs of /dev/shm and mqueue mounted inside it (as
# container runtimes mount them, round 184). OpenRC's devfs sees something already mounted on
# /dev and only remounts it as below: device nodes and terminals are still there afterwards. The
# mount flags are not compared (docker's /dev is strictatime)
awk '$5 ~ /^\/dev(\/|$)/ { for (i = 7; $i != "-"; i++) {}; print $5, $(i + 1) }' /proc/self/mountinfo | sort
for d in /dev/pts /dev/shm /dev/mqueue; do
    parent=$(awk -v d=$d '$5 == d { print $2 }' /proc/self/mountinfo)
    echo "$d: on /dev: $(awk -v p="$parent" '$1 == p { print ($5 == "/dev") }' /proc/self/mountinfo)"
done
mount -t tmpfs -o remount,noexec,nosuid,mode=0755,size=10M dev /dev && echo "remount /dev $?"
awk '$5 == "/dev" { for (i = 7; $i != "-"; i++) {}; print $(i + 1), $(i + 2), $(i + 3) }' /proc/self/mountinfo
echo "noexec: $(grep -c ' /dev .*noexec' /proc/self/mountinfo)"
for n in null zero full random urandom tty ptmx pts/ptmx; do
    [ -c /dev/$n ] && echo "/dev/$n: a character device"
done
python3 -c 'import os; m, s = os.openpty(); print("openpty:", os.ttyname(s).startswith("/dev/pts/"))'
head -c 8 /dev/urandom | wc -c
# fs.mount-max (Linux's count_mounts): each recursive bind of a tree onto a directory inside it
# doubles the mounts in the namespace, until adding them would pass the limit (ENOSPC). The limit
# is only read: it is global, and on the native side it is the development VM's (0 is refused
# either way, as below the minimum here and read-only /proc/sys there)
echo "mount-max: $(cat /proc/sys/fs/mount-max)"
echo 0 2>/dev/null > /proc/sys/fs/mount-max || echo "mount-max 0: refused"
unshare -m sh -c '
mkdir -p /tmp/rb && mount -t tmpfs none /tmp/rb && mount --make-private /tmp/rb
i=0
while mkdir /tmp/rb/d$i && mount --rbind /tmp/rb /tmp/rb/d$i 2>/tmp/rb.err; do i=$((i + 1)); done
echo "recursive binds before the limit: $i ($(sed "s/.*: //" /tmp/rb.err))"
umount -l /tmp/rb'
rm -f /tmp/rb.err
