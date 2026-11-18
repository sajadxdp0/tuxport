# docker-run-options: --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# overlayfs with lower layers only: names merged from three layers, whiteouts and opaque
# directories, a file above a directory and a directory above a file, inode and device numbers,
# links of merged directories, the overlay's own attributes hidden, readdir, chdir, getcwd and
# /proc/<pid>/fd through it, running programs and scripts in it, flock and fcntl locks between
# processes, EROFS for every change (also through a bind mount remounted read-write) and for a
# read-write remount, and the errors of bad mount options. All layers are on one tmpfs on both
# sides; mountinfo's superblock options are compared only for ro and lowerdir (6.8 adds
# redirect_dir=on,nouserxattr where 6.1 shows none)
cd /tmp
umount -R /tmp/ovl 2>/dev/null
rm -rf ovl
mkdir ovl
mount -t tmpfs tmpfs ovl
cd ovl
mkdir -p a/d a/o a/hid b/d b/o b/fil b/sub/deep c/d m n
echo a > a/f
echo a > a/d/x
echo b > b/d/y
echo c > c/d/z
echo b > b/g
ln b/g b/g2
echo b > b/o/z
echo a > a/o/w
echo c > c/f
mknod a/gone c 0 0
echo b > b/gone
ln -s f a/link
echo b > b/hid
echo a > a/fil
echo b > b/fil/in
echo b > b/sub/deep/e
printf '#!/bin/sh\necho script ran\n' > b/s.sh
chmod +x b/s.sh
cp /bin/busybox c/bb
python3 -c '
import os
os.setxattr("a/o", "trusted.overlay.opaque", b"y")
os.setxattr("a/f", "trusted.note", b"kept")
'

mount -t overlay overlay -o lowerdir=a:b:c m && echo "mount $?"
grep -w overlay /proc/filesystems
awk '$5 == "/tmp/ovl/m" {
    for (i = 7; $i != "-"; i++) {}
    split($(i + 3), o, ",")
    print $6, $(i + 1), $(i + 2), o[1], o[2]
}' /proc/self/mountinfo
stat -f -c 'type %t' m
python3 -c 'import os; print("statvfs read-only:", bool(os.statvfs("m").f_flag & os.ST_RDONLY))'

echo "--- merged tree"
(cd m && find . | sort)
cat m/f m/d/x m/d/y m/d/z m/g2 m/link m/fil m/sub/deep/e
ls m/gone 2>&1
ls m/o/z 2>&1
ls m/fil/in 2>&1

echo "--- inodes, links and devices"
same() { [ "$(stat -c %i "$1")" = "$(stat -c %i "$2")" ] && echo "$1: inode of $2"; }
same m a
same m/f a/f
same m/d a/d
same m/g b/g
same m/g2 b/g
same m/sub b/sub
same m/bb c/bb
for f in m m/d m/o m/sub m/f m/g; do echo "$f: $(stat -c '%h links, %F, mode %a' $f)"; done
[ "$(stat -c %d m)" != "$(stat -c %d a)" ] && echo "m: a device of its own"
[ "$(stat -c %d m/d/y)" = "$(stat -c %d m)" ] && echo "m/d/y: the overlay's device"
python3 -c '
import os
for e in sorted(os.scandir("m"), key=lambda e: e.name):
    if e.inode() != os.lstat(e.path).st_ino:
        print(e.name, "readdir and stat disagree")
print("readdir checked")
print("xattrs of m/o:", os.listxattr("m/o"))
print("trusted.note of m/f:", os.getxattr("m/f", "trusted.note"))
try:
    os.getxattr("m/o", "trusted.overlay.opaque")
    print("opaque attribute visible")
except OSError:
    print("opaque attribute hidden")
'

echo "--- paths and programs"
cd m/sub/deep && pwd && cd /tmp/ovl
exec 3< m/d/y
readlink /proc/self/fd/3
cat /proc/self/fd/3
exec 3<&-
m/bb echo "busybox ran"
m/bb readlink /proc/self/exe
m/s.sh

echo "--- locks"
mkfifo ready
(flock -s 9 && echo > ready && sleep 2) 9< m/f &
read x < ready
flock -n -x m/f true && echo "locked twice" || echo "flock: busy"
wait
python3 -c '
import fcntl, os, struct, subprocess
fd = os.open("m/f", os.O_RDONLY)
fcntl.lockf(fd, fcntl.LOCK_SH)
probe = "\n".join([
    "import fcntl, os, struct",
    "fd = os.open(\"m/f\", os.O_RDONLY)",
    "r = fcntl.fcntl(fd, fcntl.F_GETLK, struct.pack(\"hhqqi\", fcntl.F_WRLCK, 0, 0, 0, 0))",
    "print(\"held\" if struct.unpack(\"hhqqi\", r)[0] == fcntl.F_RDLCK else \"free\")",
])
out = subprocess.run(["python3", "-c", probe], capture_output=True, text=True).stdout
print("F_GETLK from another process:", out.strip())
'
rm ready

echo "--- read-only"
touch m/new 2>&1; echo "touch $?"
mkdir m/nd 2>&1; echo "mkdir $?"
mkdir m/d 2>&1; echo "mkdir of an existing name $?"
mknod m/f p 2>&1; echo "mknod of an existing name $?"
ln -s x m/f 2>&1; echo "symlink of an existing name $?"
rm m/f 2>&1; echo "rm $?"
rmdir m/sub/deep 2>&1; echo "rmdir $?"
chmod 600 m/f 2>&1; echo "chmod $?"
ln m/f m/f2 2>&1; echo "ln $?"
mv m/f m/f3 2>&1; echo "mv $?"
(echo x > m/f) 2>&1; echo "write $?"
(echo x >> m/d/x) 2>&1; echo "append $?"
python3 -c '
import os
for name, call in [("setxattr", lambda: os.setxattr("m/f", "trusted.x", b"1")),
                   ("truncate", lambda: os.truncate("m/f", 0)),
                   ("utime", lambda: os.utime("m/f", (1, 1)))]:
    try:
        call()
        print(name, "succeeded")
    except OSError as e:
        print(name, os.strerror(e.errno))
'
mount -o remount,rw m 2>&1; echo "remount $?"
mount --bind m n && mount -o remount,bind,rw n && echo "bind remounted rw"
(echo x > n/f) 2>&1; echo "write through the bind $?"
umount n

echo "--- bad options"
for o in lowerdir=a lowerdir= upperdir=a lowerdir=a:b,bogus lowerdir=a:nowhere lowerdir=a:a/f \
    lowerdir=a:b,redirect_dir=maybe; do
    out=$(mount -t overlay overlay -o "$o" n 2>&1)
    echo "$o: $? $out"
done
out=$(mount -t overlay overlay n 2>&1)
echo "no options: $? $out"

umount m
echo "--- layers untouched"
find a b c | sort
cd /tmp
umount ovl
rm -rf ovl
