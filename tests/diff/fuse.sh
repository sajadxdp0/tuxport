# docker-run-options: --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# FUSE through fuse-overlayfs (libfuse 3.17, which reads requests with splice and writes data
# replies through vmsplice and splice): the mount and its line in mountinfo, lookups, stat,
# readdir, symlinks, reads and writes (one larger than a request), O_APPEND (two processes appending
# at once) and O_TRUNC, creating,
# renaming and removing files and directories, links, chmod, truncate and times, xattrs, statfs,
# running a script and a program from the mount, mappings (read page by page as they are touched:
# far-apart pages, read(2) and write(2) on mapped buffers, fork, MADV_DONTNEED, mremap,
# MAP_POPULATE, SIGBUS past the end, a library loaded from the mount), nodes forgotten after
# drop_caches unless open or a current directory (a daemon of its own), the errors the daemon and the
# kernel return, the daemon leaving when the mount goes and when a lazily unmounted one is no
# longer used, ENOTCONN once the daemon is killed, and
# the errors of mount options and of /dev/fuse before a mount. The layers are on one tmpfs on
# both sides. Not compared: inode numbers (fuse-overlayfs takes them from the layers' files) and
# times
cd /tmp
umount -l /tmp/fuse/m 2>/dev/null
umount -l /tmp/fuse 2>/dev/null
rm -rf fuse
mkdir fuse
mount -t tmpfs tmpfs fuse
cd fuse
mkdir -p l/sub u w m
echo hello > l/a.txt
echo deep > l/sub/b
ln -s a.txt l/link
printf '#!/bin/sh\necho script ran "$@"\n' > l/s.sh
chmod 755 l/s.sh
cp /bin/busybox l/busybox
fuse-overlayfs -o lowerdir=/tmp/fuse/l,upperdir=/tmp/fuse/u,workdir=/tmp/fuse/w m || exit 1
echo "-- mountinfo"
awk '$5 == "/tmp/fuse/m" { for (i = 1; $i != "-"; i++); print $6, $(i + 1), $(i + 2), $(i + 3) }' \
    /proc/self/mountinfo
echo "-- read"
ls -a m | sort
ls -l m | awk 'NR > 1 { print $1, $3, $4, ($1 ~ /^d/ ? "-" : $5), $9, $10, $11 }' | sort -k5
cat m/a.txt m/sub/b m/link
readlink m/link
stat -c '%n %F %a %h %u %g' m m/a.txt m/sub m/link
stat -c '%n %s' m/a.txt m/link
stat -f -c '%t %b %l' m | awk '{ print $1, ($2 > 0), $3 }'
test -e m/missing || echo "missing: no"
cat m/missing 2>&1
cat m/a.txt/x 2>&1
echo "-- write"
echo one > m/new
echo two >> m/new
cat m/new
printf abcdefghij > m/trunc
: > m/trunc
stat -c '%n %s' m/trunc
printf 0123456789 > m/t2
truncate -s 4 m/t2
cat m/t2; echo
truncate -s 8 m/t2
od -An -c m/t2
dd if=/dev/urandom of=/tmp/fuse/big bs=1024 count=700 2>/dev/null
cp /tmp/fuse/big m/big
cmp /tmp/fuse/big m/big && echo "big: same"
stat -c '%n %s' m/big
python3 -c '
import os
fd = os.open("/tmp/fuse/m/new", os.O_RDWR)
os.lseek(fd, 1, 0)
os.write(fd, b"X")
print(os.pread(fd, 3, 0), os.lseek(fd, 0, 2), os.fstat(fd).st_size)
os.close(fd)
fd = os.open("/tmp/fuse/m/a.txt", os.O_WRONLY | os.O_APPEND)
os.write(fd, b"more\n")
os.close(fd)
'
cat m/a.txt
cat l/a.txt
echo "-- namespace"
mkdir m/d m/d/e
mkdir m/d 2>&1
echo f > m/d/f
mv m/d/f m/d/g
mv m/new m/d/
ls m/d | sort
rmdir m/d 2>&1
ln m/d/g m/d/h
stat -c '%n %h' m/d/g
ln -s ../a.txt m/d/sl
cat m/d/sl
rm m/d/g m/d/sl
rmdir m/d/e
rm m/link
ls m m/d | sort
mv m/sub m/sub2
cat m/sub2/b
cd m/sub2 && pwd && cd /tmp/fuse
python3 -c '
import os
os.chdir("/tmp/fuse/m/d")
print(os.getcwd())
os.rename("/tmp/fuse/m/d", "/tmp/fuse/m/d2")
print(os.getcwd())
for f, n in [(os.rename, ("/tmp/fuse/m/d2", "/tmp/fuse/m/d2/x")), (os.rmdir, ("/tmp/fuse/m",)),
             (os.unlink, ("/tmp/fuse/m/sub2",)), (os.mkdir, ("/tmp/fuse/m/no/x",))]:
    try:
        f(*n)
    except OSError as e:
        print(f.__name__, e.strerror)
'
echo "-- attributes"
chmod 640 m/a.txt
stat -c '%n %a' m/a.txt
chown 1:2 m/trunc
stat -c '%n %u %g' m/trunc
touch -d '2001-02-03 04:05:06' m/trunc
stat -c '%n %Y' m/trunc
python3 -c '
import os
p = "/tmp/fuse/m/trunc"
os.setxattr(p, "user.note", b"kept")
print(os.getxattr(p, "user.note"), "user.note" in os.listxattr(p))
os.removexattr(p, "user.note")
try:
    os.getxattr(p, "user.note")
except OSError as e:
    print("getxattr", e.strerror)
'
python3 -c '
import fcntl, os, resource, signal
p = "/tmp/fuse/m/new2"
fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(fd, b"ab")
fcntl.fcntl(fd, fcntl.F_SETFL, os.O_APPEND)
os.lseek(fd, 0, 0)
os.write(fd, b"cd")
fcntl.fcntl(fd, fcntl.F_SETFL, 0)
os.lseek(fd, 0, 0)
os.write(fd, b"X")
os.close(fd)
print(open(p, "rb").read())
signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
hard = resource.getrlimit(resource.RLIMIT_FSIZE)[1]
resource.setrlimit(resource.RLIMIT_FSIZE, (6, hard))
fd = os.open(p, os.O_WRONLY)
print("write past the limit:", os.pwrite(fd, b"123456", 2))
try:
    os.pwrite(fd, b"7", 6)
except OSError as e:
    print("write at the limit:", e.strerror)
os.close(fd)
resource.setrlimit(resource.RLIMIT_FSIZE, (hard, hard))
fd = os.open(p, os.O_RDWR)
os.posix_fallocate(fd, 0, 100)
print("fallocated:", os.fstat(fd).st_size)
os.close(fd)
'
# Two processes appending at once: no line lands on another
for i in 1 2; do
    (for j in $(seq 200); do echo "writer $i line $j" >> m/log; done) &
done
wait
echo "appended: $(wc -l < m/log) lines, $(sort -u m/log | wc -l) distinct"
echo "-- programs and mappings"
m/s.sh arg
m/busybox echo "busybox ran"
printf '#!/tmp/fuse/m/busybox sh\necho "interpreter on the mount"\n' > m/i.sh
chmod 755 m/i.sh
m/i.sh
cp m/busybox m/sleep
m/sleep 2 &
sleep 0.5
python3 -c '
import os
try:
    os.open("/tmp/fuse/m/sleep", os.O_WRONLY)
except OSError as e:
    print("writing a running program:", e.strerror)
fd = os.open("/tmp/fuse/m/s.sh", os.O_WRONLY)
try:
    os.execv("/tmp/fuse/m/s.sh", ["s.sh"])
except OSError as e:
    print("running a file open for writing:", e.strerror)
'
wait
python3 -c '
import mmap
with open("/tmp/fuse/m/sub2/b", "rb") as f:
    m = mmap.mmap(f.fileno(), 0, prot=mmap.PROT_READ)
    print(m[:4])
    for flags, prot in [(mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE),
                        (mmap.MAP_PRIVATE, mmap.PROT_READ | mmap.PROT_EXEC)]:
        try:
            mmap.mmap(f.fileno(), 0, flags=flags, prot=prot)
            print("mapped")
        except OSError as e:
            print("mmap:", e.strerror)
'
mount -o remount,noexec m
python3 -c '
import mmap
with open("/tmp/fuse/m/sub2/b", "rb") as f:
    try:
        mmap.mmap(f.fileno(), 0, flags=mmap.MAP_PRIVATE, prot=mmap.PROT_READ | mmap.PROT_EXEC)
    except OSError as e:
        print("noexec mmap:", e.strerror)
'
mount -o remount,exec m
echo "-- mappings read on demand"
python3 -c '
import ctypes, hashlib, os, signal
libc = ctypes.CDLL(None, use_errno=True)
vp, sz = ctypes.c_void_p, ctypes.c_size_t
libc.mmap.restype = libc.mremap.restype = vp
libc.mmap.argtypes = [vp, sz, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
libc.mremap.argtypes = [vp, sz, sz, ctypes.c_int]
libc.madvise.argtypes = [vp, sz, ctypes.c_int]
libc.read.argtypes = libc.write.argtypes = [ctypes.c_int, vp, sz]
libc.read.restype = libc.write.restype = ctypes.c_ssize_t
R, W, SHARED, PRIVATE, POPULATE, P = 1, 2, 1, 2, 0x8000, 4096
data = b"".join(hashlib.sha256(b"%d" % i).digest() * 128 for i in range(768)) + b"tail"
with open("/tmp/fuse/m/big", "wb") as f:
    f.write(data)
fd = os.open("/tmp/fuse/m/big", os.O_RDWR)
def page(a, p):
    return ctypes.string_at(a + p * P, P)
def same(a, pages):
    return all(page(a, p) == data[p * P:(p + 1) * P] for p in pages)
a = libc.mmap(None, len(data), R, SHARED, fd, 0)
print("pages far apart:", same(a, [700, 3, 512, 255, 0, 767]))
whole = hashlib.sha256(ctypes.string_at(a, len(data))).digest()
print("whole:", whole == hashlib.sha256(data).digest(), page(a, 768)[:6])
b = libc.mmap(None, len(data), R, PRIVATE, fd, 0)
out = os.open("/tmp/fuse/out", os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
n = libc.write(out, b + 5 * P + 7, 40 * P)
print("write(2) from it:", n, os.pread(out, 40 * P, 0) == data[5 * P + 7:45 * P + 7])
c = libc.mmap(None, len(data), R | W, PRIVATE, fd, 0)
os.lseek(out, 0, 0)
n = libc.read(out, c + 300 * P + 10, 6)
want = data[300 * P:300 * P + 10] + data[5 * P + 7:5 * P + 13] + data[300 * P + 16:301 * P]
print("read(2) into it:", n, page(c, 300) == want)
ctypes.memmove(c + 400 * P, b"parent", 6)
pid = os.fork()
if pid == 0:
    ok = page(c, 400)[:6] == b"parent" and same(c, [401, 600])
    ctypes.memmove(c + 401 * P, b"child", 5)
    os._exit(0 if ok else 1)
_, status = os.waitpid(pid, 0)
print("fork:", os.waitstatus_to_exitcode(status), same(c, [401]))
libc.madvise(c + 400 * P, P, 4)
print("MADV_DONTNEED:", same(c, [400]))
d = libc.mmap(None, 8 * P, R, PRIVATE, fd, 100 * P)
d = libc.mremap(d, 8 * P, 64 * P, 1)
print("mremap:", ctypes.string_at(d + 60 * P, P) == data[160 * P:161 * P])
e = libc.mmap(None, len(data) + 3 * P, R, PRIVATE | POPULATE, fd, 0)
print("MAP_POPULATE:", same(e, [2, 766]))
pid = os.fork()
if pid == 0:
    ctypes.string_at(e + 770 * P, 1)
    os._exit(0)
_, status = os.waitpid(pid, 0)
print("past the end:", signal.Signals(os.WTERMSIG(status)).name)
'
cp /usr/lib/libz.so.1 m/libz.so.1
python3 -c '
import ctypes, zlib
z = ctypes.CDLL("/tmp/fuse/m/libz.so.1")
z.zlibVersion.restype = ctypes.c_char_p
print("a library from the mount:", z.zlibVersion().decode() == zlib.ZLIB_RUNTIME_VERSION)
'
rm m/big m/libz.so.1 out
echo "-- upper layer"
# Without RENAME_WHITEOUT (Tuxport) fuse-overlayfs leaves .wh. files instead of whiteout devices
find u ! -type c ! -name '.wh.*' | sort | sed 's/^/  /'
echo "-- unmount"
umount m && echo unmounted
sleep 1
pgrep -x fuse-overlayfs || echo "daemon gone"
echo "-- daemon killed"
fuse-overlayfs -o lowerdir=/tmp/fuse/l,upperdir=/tmp/fuse/u,workdir=/tmp/fuse/w m
cat m/a.txt
stat -c %F m
pkill -9 -x fuse-overlayfs
sleep 1
ls m 2>&1
stat m 2>&1 | head -1
umount m && echo unmounted
echo "-- lazy unmount"
# The connection lasts while a file or a current directory is in the detached mount, and is
# aborted (the daemon exits) when the last one goes
fuse-overlayfs -o lowerdir=/tmp/fuse/l,upperdir=/tmp/fuse/u,workdir=/tmp/fuse/w m || exit 1
exec 3< m/a.txt
umount -l m && echo detached
sleep 1
pgrep -x fuse-overlayfs > /dev/null && echo "daemon still serving"
cat <&3
exec 3<&-
sleep 1
pgrep -x fuse-overlayfs || echo "daemon gone"
fuse-overlayfs -o lowerdir=/tmp/fuse/l,upperdir=/tmp/fuse/u,workdir=/tmp/fuse/w m || exit 1
sh -c 'cd /tmp/fuse/m && umount -l /tmp/fuse/m && echo "detached from inside"
sleep 1
pgrep -x fuse-overlayfs > /dev/null && echo "daemon still serving"
head -1 a.txt'
sleep 1
pgrep -x fuse-overlayfs || echo "daemon gone"
echo "-- forgetting nodes"
# A daemon speaking the protocol itself, recording for each name the lookups not yet forgotten
cat > fz.py <<'EOF'
import ctypes, os, struct, sys
libc = ctypes.CDLL(None, use_errno=True)
fd = os.open("/dev/fuse", os.O_RDWR)
opts = b"fd=%d,rootmode=40000,user_id=0,group_id=0" % fd
if libc.mount(b"fz", b"/tmp/fuse/z", b"fuse.fz", 0, opts) != 0:
    sys.exit("mount: %d" % ctypes.get_errno())
names = {1: {b"d": 2, b"f1": 3, b"f2": 4, b"f3": 5}, 2: {b"g": 6}}
label = {2: "d", 3: "f1", 4: "f2", 5: "f3", 6: "g"}
held = {n: 0 for n in label}
def attr(n):
    mode = 0o40755 if n in names else 0o100644
    return struct.pack("<6Q4x4x4xIIIIIII", n, 0, 0, 0, 0, 0, mode, 1, 0, 0, 0, 4096, 0)
def state():
    with open("/tmp/fuse/zs.new", "w") as f:
        f.write("".join("%s %d\n" % (label[n], held[n]) for n in sorted(label)))
    os.rename("/tmp/fuse/zs.new", "/tmp/fuse/zs")
def reply(unique, error=0, body=b""):
    os.write(fd, struct.pack("<IiQ", 16 + len(body), -error, unique) + body)
while True:
    try:
        m = os.read(fd, 1 << 17)
    except OSError:
        break
    _, op, unique, node = struct.unpack_from("<IIQQ", m)
    body = m[40:]
    if op == 26:
        reply(unique, body=struct.pack("<IIIIHHIIHHI28x", 7, 31, 0, 0, 0, 0, 4096, 1, 0, 0, 0))
    elif op == 1:
        child = names.get(node, {}).get(body.rstrip(b"\0"))
        if child is None:
            reply(unique, 2)
            continue
        held[child] += 1
        state()
        reply(unique, body=struct.pack("<QQQQII", child, 0, 3600, 3600, 0, 0) + attr(child))
    elif op == 3:
        reply(unique, body=struct.pack("<QII", 3600, 0, 0) + attr(node))
    elif op in (14, 27):
        reply(unique, body=struct.pack("<QII", 0, 0, 0))
    elif op in (18, 29, 25):
        reply(unique)
    elif op == 2:
        held[node] -= struct.unpack_from("<Q", body)[0]
        state()
    elif op == 42:
        count = struct.unpack_from("<I", body)[0]
        for i in range(count):
            n, k = struct.unpack_from("<QQ", body, 8 + 16 * i)
            held[n] -= k
        state()
    elif op != 36:
        reply(unique, 38)
EOF
mkdir z
python3 fz.py &
for i in 1 2 3 4 5 6 7 8 9 10; do
    grep -q ' /tmp/fuse/z ' /proc/self/mountinfo && break
    sleep 0.5
done
stat -c %n z/f1 z/f2 z/f3 z/d/g
exec 3< z/f2
(cd z/d && sleep 3) &
sleeper=$!
sleep 1
# FUSE_FORGET has no reply, so the daemon records a forget some time after drop_caches: wait up
# to 5 s for the counts Linux leaves before printing them
settle() {
    for i in $(seq 50); do
        [ "$(cat zs)" = "$1" ] && break
        sleep 0.1
    done
    cat zs
}
# Docker mounts /proc/sys read-only
mount -o remount,rw /proc/sys 2>/dev/null
# What nothing uses goes; the open file and the current directory stay
echo 2 > /proc/sys/vm/drop_caches
settle "$(printf 'd 1\nf1 0\nf2 1\nf3 0\ng 0')"
exec 3<&-
wait $sleeper
echo 2 > /proc/sys/vm/drop_caches
settle "$(printf 'd 0\nf1 0\nf2 0\nf3 0\ng 0')"
umount z && echo unmounted
sleep 0.5
rm -f fz.py zs
echo "-- mount errors"
python3 -c '
import ctypes, errno, os
libc = ctypes.CDLL(None, use_errno=True)
def mount(data, fstype=b"fuse"):
    r = libc.mount(b"none", b"/tmp/fuse/m", fstype, 0, data)
    print(fstype.decode(), data.decode(), "->", "ok" if r == 0 else errno.errorcode[ctypes.get_errno()])
    if r == 0:
        libc.umount2(b"/tmp/fuse/m", 2)
fd = os.open("/dev/fuse", os.O_RDWR)
for call in [lambda: os.read(fd, 8192), lambda: os.write(fd, bytes(16))]:
    try:
        call()
    except OSError as e:
        print("unattached /dev/fuse:", errno.errorcode[e.errno])
mount(b"rootmode=40000,user_id=0,group_id=0")
mount(b"fd=%d,user_id=0,group_id=0" % fd)
mount(b"fd=%d,rootmode=40000,user_id=0,group_id=0,bogus" % fd)
mount(b"fd=%d,rootmode=170000,user_id=0,group_id=0" % fd)
mount(b"fd=0,rootmode=40000,user_id=0,group_id=0")
mount(b"fd=99,rootmode=40000,user_id=0,group_id=0")
mount(b"fd=%d,rootmode=40000,user_id=0,group_id=0" % fd, b"fuse.")
mount(b"fd=%d,rootmode=40000,user_id=0,group_id=0,blksize=4096" % fd)
mount(b"fd=%d,rootmode=40000,user_id=0,group_id=0,subtype=x" % fd, b"fuse.test")
import fcntl, struct
other = os.open("/dev/fuse", os.O_RDWR)
# A clone from an unattached one is left out: EINVAL in 6.1 and 6.8, EPERM in the newer kernel
# of CI (tests/guest/linux61.c checks the 6.1 answer)
try:
    fcntl.ioctl(other, 0x8004e500, struct.pack("I", 99))
except OSError as e:
    print("clone of a bad fd:", errno.errorcode[e.errno])
mount(b"fd=%d,rootmode=40000,user_id=0,group_id=0" % fd, b"fuse.test")
mount(b"fd=%d" % fd)
try:
    os.read(fd, 100)
except OSError as e:
    print("short read:", errno.errorcode[e.errno])
'
cd /tmp
umount fuse
