# docker-run-options: --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# inotify through an overlay (LTP's inotify07 and inotify08 in more detail): watches on the
# overlay's directories and files report what is done through the overlay, before and after copy
# up — open, read, write, close, chmod, chown, utimes, xattrs, truncation, creation, links,
# removal, renames with their cookies, a name replaced by a rename, getdents — and fdinfo shows
# the overlay's inode and device; changes made directly in a layer are not reported to them.
# Watches on the layers themselves are not compared (Linux reports there what its overlay does to
# the upper layer, Tuxport only creations), nor a watched file's last unlink after a hard link to
# it came and went (Linux's overlay counts the link on the file's inode and its removal on
# another, so it never reports the deletion). All layers are on one tmpfs
cd /tmp
umount -R /tmp/ovl-in 2>/dev/null
rm -rf ovl-in
mkdir ovl-in
mount -t tmpfs tmpfs ovl-in
cd ovl-in
mkdir -p l/d u w m
echo lower > l/d/f
echo lower > l/d/keep
echo lower > l/d/gone
echo lower > l/d/old
mount -t overlay overlay -o lowerdir=l,upperdir=u,workdir=w m && echo "mount $?"

python3 - <<'PY'
import ctypes, os, struct
libc = ctypes.CDLL(None, use_errno=True)
fd = libc.inotify_init1(os.O_NONBLOCK)
labels = {}

def watch(label, path):
    wd = libc.inotify_add_watch(fd, path.encode(), 0xfff)
    labels[wd] = label
    return wd

NAMES = [(0x1, "ACCESS"), (0x2, "MODIFY"), (0x4, "ATTRIB"), (0x8, "CLOSE_WRITE"),
         (0x10, "CLOSE_NOWRITE"), (0x20, "OPEN"), (0x40, "MOVED_FROM"), (0x80, "MOVED_TO"),
         (0x100, "CREATE"), (0x200, "DELETE"), (0x400, "DELETE_SELF"), (0x800, "MOVE_SELF"),
         (0x8000, "IGNORED"), (0x40000000, "ISDIR")]

def events(what):
    print("--", what)
    try:
        data = os.read(fd, 65536)
    except BlockingIOError:
        data = b""
    cookies = {}
    i = 0
    while i < len(data):
        wd, mask, cookie, n = struct.unpack_from("iIII", data, i)
        name = data[i + 16:i + 16 + n].rstrip(b"\0").decode()
        i += 16 + n
        flags = "|".join(s for b, s in NAMES if mask & b)
        tag = " cookie %d" % cookies.setdefault(cookie, len(cookies) + 1) if cookie else ""
        print(" ", labels.get(wd, wd), flags, name + tag)

d = watch("d", "m/d")
f = watch("f", "m/d/f")
keep = watch("keep", "m/d/keep")
gone = watch("gone", "m/d/gone")
old = watch("old", "m/d/old")

# fdinfo shows the overlay's inode number and device
st = os.stat("m/d/f")
for line in open(f"/proc/self/fdinfo/{fd}"):
    if line.startswith(f"inotify wd:{f:x} "):
        fields = dict(x.split(":", 1) for x in line.split()[1:6])
        dev = os.major(st.st_dev) << 20 | os.minor(st.st_dev)
        print("fdinfo ino matches:", int(fields["ino"], 16) == st.st_ino,
              "sdev matches:", int(fields["sdev"], 16) == dev)

with open("m/d/f") as fh:
    fh.read()
events("read before copy-up")
os.listdir("m/d")
events("getdents")
os.chmod("m/d/f", 0o600)
events("chmod copies up")
os.chown("m/d/keep", 5, 6)
events("chown copies up")
with open("m/d/f", "a") as fh:
    fh.write("more")
events("append")
os.truncate("m/d/f", 2)
events("truncate by path")
os.utime("m/d/f", (1, 2))
events("utimes")
os.setxattr("m/d/f", "trusted.note", b"x")
events("setxattr")
fh = os.open("m/d/f", os.O_RDWR)
os.ftruncate(fh, 1)
os.fchmod(fh, 0o644)
os.close(fh)
events("ftruncate and fchmod by fd")

# Changes made in the layers themselves are not the overlay's
with open("l/d/keep", "a") as fh:
    fh.write("x")
with open("u/d/f", "a") as fh:
    fh.write("x")
os.chmod("u/d", 0o700)
events("written in the layers directly")

with open("m/d/c", "w") as fh:
    fh.write("c")
events("create")
os.mkdir("m/d/sub")
os.symlink("f", "m/d/sym")
os.mkfifo("m/d/fifo")
events("mkdir, symlink, mkfifo")
os.link("m/d/keep", "m/d/lnk")
events("link")
os.unlink("m/d/lnk")
events("unlink a link")
watch("c", "m/d/c")
os.rename("m/d/c", "m/d/c2")
events("rename")
os.rename("m/d/c2", "m/d/old")
events("rename over a watched name")
os.unlink("m/d/gone")
events("unlink a lower file")
os.rmdir("m/d/sub")
events("rmdir")
os.unlink("m/d/f")
events("unlink the copied-up file")
PY
cd /tmp
umount ovl-in/m
umount ovl-in
rm -rf ovl-in
