# docker-run-options: --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# FUSE filesystems people mount: sshfs speaking SFTP to a local sftp-server (passive mode, no
# network or sshd), mounted by root and by an ordinary user through the setuid fusermount3 (the
# descriptor passed back over a socket), root refused by a mount without allow_other, unmounting
# with fusermount3 -u; rclone mount of a local directory (Go's FUSE library, also through
# fusermount3); copy_file_range between two FUSE files, sendfile out of one; inotify events of
# changes made through a mount. Everything lives on one tmpfs on both sides; sizes of directories
# (sftp reports the server's) and times are not compared
cd /tmp
umount -l /tmp/ft/sm 2>/dev/null
umount -l /tmp/ft/um 2>/dev/null
umount -l /tmp/ft/rm 2>/dev/null
umount -l /tmp/ft 2>/dev/null
rm -rf ft
mkdir ft
mount -t tmpfs -o mode=1777 tmpfs ft
cd ft
mkdir -p ss/d sm us um rs rm
echo hello > ss/a
echo deep > ss/d/b
listing() {
    ls -l "$@" | awk 'NF > 2 { print $1, $3, ($1 ~ /^d/ ? "-" : $5), $9, $10, $11 }'
}
echo "-- sshfs by root"
mkfifo s2c c2s
/usr/lib/ssh/sftp-server > s2c < c2s &
sshfs -f -o passive :/tmp/ft/ss /tmp/ft/sm < s2c > c2s &
for i in 1 2 3 4 5 6 7 8 9 10; do
    grep -q ' /tmp/ft/sm ' /proc/self/mountinfo && break
    sleep 0.5
done
awk '$5 == "/tmp/ft/sm" { for (i = 1; $i != "-"; i++); print $6, $(i + 1), $(i + 2), $(i + 3) }' \
    /proc/self/mountinfo
listing sm sm/d
cat sm/a sm/d/b
echo new > sm/n
cat ss/n
mkdir sm/x
mv sm/n sm/x/m
ls ss/x
ln -s ../a sm/x/l
cat sm/x/l
rm sm/x/m sm/x/l
rmdir sm/x
dd if=/dev/urandom of=big bs=1024 count=500 2>/dev/null
cp big sm/big
cmp big ss/big && cmp big sm/big && echo "big: same"
python3 -c '
import os
src = os.open("/tmp/ft/sm/big", os.O_RDONLY)
dst = os.open("/tmp/ft/sm/copy", os.O_WRONLY | os.O_CREAT, 0o644)
n = 0
while True:
    k = os.copy_file_range(src, dst, 1 << 20)
    if k == 0:
        break
    n += k
print("copy_file_range", n)
'
cmp big ss/copy && echo "copy: same"
python3 -c '
import os
src = os.open("/tmp/ft/sm/big", os.O_RDONLY)
dst = os.open("/tmp/ft/sent", os.O_WRONLY | os.O_CREAT, 0o644)
n = 0
while True:
    k = os.sendfile(dst, src, None, 1 << 20)
    if k == 0:
        break
    n += k
print("sendfile", n)
'
cmp big sent && echo "sendfile: same"
chmod 600 sm/a
stat -c '%n %a' ss/a
python3 -c '
import ctypes, os, struct
libc = ctypes.CDLL(None, use_errno=True)
fd = libc.inotify_init1(os.O_NONBLOCK)
libc.inotify_add_watch(fd, b"/tmp/ft/sm", 0xfff)
open("/tmp/ft/sm/w", "w").write("x")
os.rename("/tmp/ft/sm/w", "/tmp/ft/sm/w2")
os.unlink("/tmp/ft/sm/w2")
os.mkdir("/tmp/ft/sm/e")
os.rmdir("/tmp/ft/sm/e")
names = {1: "ACCESS", 2: "MODIFY", 4: "ATTRIB", 8: "CLOSE_WRITE", 0x10: "CLOSE_NOWRITE",
         0x20: "OPEN", 0x40: "MOVED_FROM", 0x80: "MOVED_TO", 0x100: "CREATE", 0x200: "DELETE",
         0x40000000: "ISDIR"}
buf = os.read(fd, 4096)
i = 0
events = []
while i < len(buf):
    wd, mask, cookie, size = struct.unpack_from("iIII", buf, i)
    name = buf[i + 16:i + 16 + size].rstrip(b"\0").decode()
    events.append(name + " " + "|".join(v for k, v in names.items() if mask & k))
    i += 16 + size
# 6.8 reports a created file opened before its creation, 6.1 after it
print("\n".join(sorted(events)))
'
umount sm && echo unmounted
wait
pgrep -x sshfs || echo "sshfs gone"
echo "-- sshfs by a user"
mkdir -p us/d
echo mine > us/f
chown -R nobody us um
rm -f s2c c2s
su -s /bin/sh nobody -c '
cd /tmp/ft
mkfifo s2c c2s
/usr/lib/ssh/sftp-server > s2c < c2s &
sshfs -f -o passive :/tmp/ft/us /tmp/ft/um < s2c > c2s &
for i in 1 2 3 4 5 6 7 8 9 10; do
    grep -q " /tmp/ft/um " /proc/self/mountinfo && break
    sleep 0.5
done
id -u
cat um/f
echo w > um/g
ls um
'
awk '$5 == "/tmp/ft/um" { for (i = 1; $i != "-"; i++); print $6, $(i + 1), $(i + 2), $(i + 3) }' \
    /proc/self/mountinfo
ls um 2>&1
cat um/f 2>&1
stat -c %n um 2>&1
su -s /bin/sh nobody -c 'fusermount3 -u /tmp/ft/um && echo "fusermount3 -u done"'
grep -c ' /tmp/ft/um ' /proc/self/mountinfo
sleep 1
pgrep -x sshfs || echo "sshfs gone"
echo "-- rclone"
echo r1 > rs/f
mkdir rs/d
rclone mount /tmp/ft/rs /tmp/ft/rm --daemon --config /dev/null 2>/dev/null
for i in 1 2 3 4 5 6 7 8 9 10; do
    grep -q ' /tmp/ft/rm ' /proc/self/mountinfo && break
    sleep 0.5
done
awk '$5 == "/tmp/ft/rm" { for (i = 1; $i != "-"; i++); print $6, $(i + 1), $(i + 3) }' \
    /proc/self/mountinfo
ls rm
cat rm/f
echo w > rm/g
cat rs/g
mv rm/g rm/d/h
ls rs/d
umount rm && echo unmounted
sleep 1
pkill -x sftp-server
cd /tmp
umount ft
