# docker-run-options: --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# overlayfs with an upper layer: copy-up on write, chmod, chown, utimes, truncate and xattrs
# (keeping mode, owner, times and attributes, and the inode number of a file with one link),
# whiteouts left by unlink, rmdir and rename, opaque directories made over a whiteout or moved
# into a merged directory, EXDEV for renaming a merged directory, links, sockets and locks of
# files created through the overlay, the upper layer as it is left and seen again after a remount,
# the probe podman runs before using overlay, and the errors of bad upper and work directories.
# All layers are on one tmpfs on both sides. The overlay is mounted with redirect_dir=follow, 6.1's
# default, because a kernel built to default to redirect_dir=on (CI's) renames a merged directory
# in place, where mv would otherwise copy it. Not compared: whiteouts' link counts (6.8 shares one
# whiteout inode), the work directory's contents and the trusted.overlay.origin/impure attributes
# Linux writes (Tuxport keeps origins in memory)
cd /tmp
umount -R /tmp/ovl 2>/dev/null
rm -rf ovl
mkdir ovl
mount -t tmpfs tmpfs ovl
cd ovl
mkdir -p l1/d l1/e l1/o l2/d l2/sub/deep u w m
echo l1 > l1/f
echo l1 > l1/d/x
echo l2 > l2/d/y
echo l1 > l1/e/z
echo l1 > l1/mv
echo l1 > l1/gone
echo l2 > l2/sub/deep/file
echo l1 > l1/o/a
chmod 4750 l1/f
chown 12:34 l1/d/x
touch -d '2020-01-02 03:04:05' l1/f l1/d l1/mv
python3 -c 'import os; os.setxattr("l1/f", "trusted.note", b"kept")'

mount -t overlay overlay -o lowerdir=l1:l2,upperdir=u,workdir=w,redirect_dir=follow m && echo "mount $?"
awk '$5 == "/tmp/ovl/m" {
    for (i = 7; $i != "-"; i++) {}
    split($(i + 3), o, ",")
    print $6, $(i + 1), o[1], o[2], o[3], o[4]
}' /proc/self/mountinfo
stat -c 'w/work: %a %U:%G %F' w/work
python3 -c 'import os; print("statvfs read-only:", bool(os.statvfs("m").f_flag & os.ST_RDONLY))'

echo "--- copy-up"
before=$(stat -c %i m/f)
echo more >> m/f
[ "$(stat -c %i m/f)" = "$before" ] && echo "m/f keeps its inode number"
cat m/f l1/f
stat -c '%n: %a %u:%g' m/f u/f
python3 -c 'import os; print("trusted.note copied:", os.getxattr("u/f", "trusted.note"))'
chown 5:6 m/d/x
stat -c '%n: %a %u:%g %s' m/d/x u/d/x l1/d/x
[ "$(stat -c %Y u/d)" = "$(stat -c %Y l1/d)" ] && echo "u/d keeps l1/d's times"
chmod 600 m/e/z
stat -c '%n: %a' m/e/z u/e/z
[ "$(stat -c %Y u/e/z)" = "$(stat -c %Y l1/e/z)" ] && echo "u/e/z keeps l1/e/z's times"
touch -d '2021-01-01 00:00:00' m/mv
stat -c '%n: %a %Y' m/mv l1/mv
truncate -s 1 m/f && stat -c '%n: %s' m/f u/f l1/f
python3 -c 'import os; os.setxattr("m/d", "trusted.dir", b"1"); print("u/d has trusted.dir:", os.getxattr("u/d", "trusted.dir"))'

echo "--- removal"
rm m/d/y && stat -c 'u/d/y: %F %t:%T %a %U:%G' u/d/y
ls m/d
rm -r m/e && stat -c 'u/e: %F %t:%T' u/e
mkdir m/e && ls -a m/e
python3 -c 'import os; print("u/e opaque:", os.getxattr("u/e", "trusted.overlay.opaque"))'
rmdir m/d 2>&1; echo "rmdir of a merged directory with entries $?"
echo new > m/gone.tmp && rm m/gone && stat -c 'u/gone: %F' u/gone
echo back > m/gone && cat m/gone && stat -c 'u/gone: %F' u/gone
rm -r m/sub && stat -c 'u/sub: %F' u/sub && ls m

echo "--- rename"
python3 -c '
import os
try:
    os.rename("m/d", "m/d2")
    print("merged directory renamed")
except OSError as e:
    print("rename of a merged directory:", os.strerror(e.errno))
os.mkdir("m/new")
os.rename("m/new", "m/new2")
print("moved upper directory opaque:", os.getxattr("u/new2", "trusted.overlay.opaque"))
os.rename("m/mv", "m/mv2")
'
stat -c 'u/mv: %F' u/mv
cat m/mv2
[ "$(stat -c %Y m/mv2)" = "$(stat -c %Y l1/mv)" ] || echo "m/mv2 has the time touch gave it"
ls m/mv 2>&1
mv m/o m/o2 && ls m/o2 && stat -c 'u/o: %F' u/o

echo "--- links, sockets, locks"
ln m/f m/flink && stat -c '%n: %h links' m/f m/flink u/f
python3 -c '
import fcntl, os, socket, subprocess
s = socket.socket(socket.AF_UNIX)
s.bind("m/sock")
s.listen(1)
c = socket.socket(socket.AF_UNIX)
c.connect("m/sock")
print("connected through the overlay; upper has it:", os.path.exists("u/sock"))
fd = os.open("m/lock", os.O_RDWR | os.O_CREAT, 0o644)
fcntl.flock(fd, fcntl.LOCK_EX)
probe = "\n".join([
    "import fcntl, os",
    "fd = os.open(\"m/lock\", os.O_RDWR)",
    "try:",
    "    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)",
    "    print(\"locked twice\")",
    "except OSError as e:",
    "    print(\"second lock:\", os.strerror(e.errno))",
])
print(subprocess.run(["python3", "-c", probe], capture_output=True, text=True).stdout.strip())
'

echo "--- inode numbers in readdir"
python3 -c '
import os
for e in sorted(os.scandir("m"), key=lambda e: e.name):
    if e.inode() != os.lstat(e.path).st_ino:
        print(e.name, "readdir and stat disagree")
print("readdir checked")
'

echo "--- upper layer"
(cd u && find . | sort)
python3 -c '
import os
for root, dirs, files in sorted(os.walk("u")):
    for n in sorted(dirs):
        p = os.path.join(root, n)
        try:
            print(p, "opaque", os.getxattr(p, "trusted.overlay.opaque"))
        except OSError:
            pass
'
echo "--- remounted"
umount m
mount -t overlay overlay -o lowerdir=l1:l2,upperdir=u,workdir=w,redirect_dir=follow m && echo "mount again $?"
(cd m && find . | sort)
cat m/f m/gone
umount m

echo "--- the probe podman runs"
mkdir -p p/merged p/lower1 p/lower2/subdir p/upper p/work
echo x > p/lower2/subdir/file
mknod p/upper/whiteout c 0 0
mount -t overlay overlay -o lowerdir=p/lower1:p/lower2,upperdir=p/upper,workdir=p/work p/merged &&
    rm -rf p/merged/subdir && echo "subdir removed" && ls -a p/merged
umount p/merged

echo "--- bad upper and work directories"
mkdir -p u2/inner w2
for o in lowerdir=l1,upperdir=u2 lowerdir=l1,upperdir=u2,workdir=u2 \
    lowerdir=l1,upperdir=u2,workdir=u2/inner lowerdir=u2/inner,upperdir=u2,workdir=w2 \
    lowerdir=w2,upperdir=u2,workdir=w2 lowerdir=l1,upperdir=nowhere,workdir=w2 \
    lowerdir=l1,upperdir=l1/f,workdir=w2; do
    out=$(mount -t overlay overlay -o "$o" m 2>&1)
    echo "$o: $? $out"
done
mount -t overlay overlay -o lowerdir=l1,upperdir=u2,workdir=w2 m && echo "one lower layer and an upper one $?"
umount m
cd /tmp
umount ovl
rm -rf ovl
