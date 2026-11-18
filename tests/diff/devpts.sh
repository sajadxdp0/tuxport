# docker-run-options: --privileged
# devpts instances (docs/rounds/167-devpts-instances.md): each mount -t devpts is a new instance
# (since Linux 4.7), with its own set of terminals (numbered from 0), device number and options
# (uid, gid, mode, ptmxmode, max); bind mounts share the instance; fd links and ttyname give the
# path inside the mount the terminal was opened through; a ptmx device node outside devpts uses
# the instance mounted at pts in the same directory; TIOCGPTPEER. The native side is a privileged
# container; the container's /dev/pts is its own instance and Tuxport's is the boot-time one, and
# both start with no terminals
cd /tmp
rm -rf p1 p2 p3 d e f x
mkdir -p p1 p2 p3 d e f/pts x
cat > /tmp/pts.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>
#ifndef TIOCGPTPEER
#define TIOCGPTPEER 0x5441
#endif

static void info(const char *what, int fd)
{
    struct stat st;
    fstat(fd, &st);
    char link[64] = "", p[32];
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    readlink(p, link, sizeof link - 1);
    printf("%s: link %s rdev %u:%u ino %lu mode %o gid %u\n", what, link, major(st.st_rdev),
           minor(st.st_rdev), (unsigned long)st.st_ino, st.st_mode & 07777, st.st_gid);
}

static int ptmx(const char *path)
{
    int m = open(path, O_RDWR | O_NOCTTY);
    if (m < 0) {
        printf("open %s: %s\n", path, strerror(errno));
        return -1;
    }
    unsigned n = 99;
    int unlock = 0;
    ioctl(m, TIOCGPTN, &n);
    ioctl(m, TIOCSPTLCK, &unlock);
    printf("open %s: index %u\n", path, n);
    return m;
}

static void mounted(const char *opts)
{
    int r = mount("devpts", "/tmp/x", "devpts", 0, opts);
    printf("mount %s: %s\n", opts, r ? strerror(errno) : "ok");
    if (r)
        return;
    FILE *f = fopen("/proc/self/mountinfo", "r");
    char line[512];
    while (fgets(line, sizeof line, f))
        if (strstr(line, " /tmp/x "))
            printf("  %s", strstr(line, " - ") + 3);
    fclose(f);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    alarm(60);
    struct stat a, b, c, r;
    stat("/dev/pts", &a);
    stat("/tmp/p1", &b);
    stat("/tmp/p2", &c);
    stat("/tmp/p1/ptmx", &r);
    printf("p1 differs from /dev/pts %d, bind p2 same as p1 %d, root ino %lu, ptmx ino %lu\n",
           a.st_dev != b.st_dev, b.st_dev == c.st_dev, (unsigned long)b.st_ino,
           (unsigned long)r.st_ino);
    int m0 = ptmx("/dev/ptmx");
    int m1 = ptmx("/tmp/p1/ptmx");
    int m1b = ptmx("/tmp/p1/ptmx");
    int m3 = ptmx("/tmp/p3/ptmx");
    int md = ptmx("/tmp/d/ptmx");
    printf("umount d/pts with a master via d/ptmx: %s\n", umount("/tmp/d/pts") ? strerror(errno) : "ok");
    close(md);
    ptmx("/tmp/e/ptmx");
    ptmx("/tmp/f/ptmx");
    int s1 = open("/tmp/p1/0", O_RDWR | O_NOCTTY);
    info("slave p1/0", s1);
    struct stat s, f;
    stat("/tmp/p1/0", &s);
    fstat(s1, &f);
    printf("fstat dev matches p1/0 %d, matches /dev/pts %d\n", f.st_dev == s.st_dev,
           f.st_dev == a.st_dev);
    int peer = ioctl(m1b, TIOCGPTPEER, O_RDWR | O_NOCTTY | O_CLOEXEC);
    info("peer of p1 #1", peer);
    printf("peer cloexec %d\n", (fcntl(peer, F_GETFD) & FD_CLOEXEC) != 0);
    printf("peer from a slave: %s\n", ioctl(s1, TIOCGPTPEER, O_RDWR) < 0 ? strerror(errno) : "ok");
    int locked = open("/tmp/p1/ptmx", O_RDWR | O_NOCTTY);
    printf("peer of a locked one: %s\n", ioctl(locked, TIOCGPTPEER, O_RDWR) < 0 ? strerror(errno) : "ok");
    close(locked);
    int s2 = open("/tmp/p2/1", O_RDWR | O_NOCTTY);
    info("slave via bind p2/1", s2);
    int s3 = open("/tmp/p3/0", O_RDWR | O_NOCTTY);
    info("slave p3/0", s3);
    info("master p1", m1);
    // The link of /dev/ptmx itself is not compared: in a docker container it is a symlink to
    // pts/ptmx, in Tuxport a device node
    close(m0);
    char *n = ttyname(s1);
    printf("ttyname p1/0: %s\n", n ? n : strerror(errno));
    n = ttyname(s3);
    printf("ttyname p3/0: %s\n", n ? n : strerror(errno));
    n = ttyname(peer);
    printf("ttyname peer: %s\n", n ? n : strerror(errno));
    // Terminals in an instance work as usual
    char buf[8] = "";
    write(s1, "hi", 2);
    // Wait at most 2 seconds: if the kernel is wrong (the write went to another terminal), the
    // test case fails instead of hanging
    struct pollfd pf = {m1, POLLIN, 0};
    int got = poll(&pf, 1, 2000) > 0 ? read(m1, buf, sizeof buf - 1) : 0;
    printf("echo through p1: %d %.2s\n", got, buf);
    system("ls /tmp/p1 /tmp/p2 /tmp/p3");
    system("stat -c '%n %A %u:%g' /tmp/p1/0 /tmp/p3/0 /tmp/p1/ptmx /tmp/p3/ptmx");
    // p3 cannot be unmounted while a terminal opened through it is open; a master opened through
    // the ptmx device node in d does not keep d/pts busy
    printf("umount p3 while in use: %s\n", umount("/tmp/p3") ? strerror(errno) : "ok");
    close(m3);
    close(s3);
    system("ls /tmp/p3");
    // Options
    const char *opts[] = {"mode=xyz", "bogus", "gid=abc", "max=1048577", "ptmxmode=0777",
                          "uid=7,gid=8,mode=0640", "newinstance", "max=1"};
    for (int i = 0; i < 8; i++) {
        mounted(opts[i]);
        if (i == 5) {
            int x = ptmx("/tmp/x/ptmx");
            system("stat -c '%u:%g %a' /tmp/x/0");
            close(x);
        }
        if (i == 7) {
            int x = ptmx("/tmp/x/ptmx");
            ptmx("/tmp/x/ptmx");
            close(x);
        }
        umount("/tmp/x");
    }
    return 0;
}
C
gcc -o /tmp/pts /tmp/pts.c
mount -t devpts devpts /tmp/p1 -o newinstance,ptmxmode=0666,mode=0620,gid=5 && echo "mount p1"
mount --bind /tmp/p1 /tmp/p2 && echo "bind p2"
mount -t devpts devpts /tmp/p3 && echo "mount p3 with the defaults"
# ptmx outside devpts: pts in the same directory is a devpts (d), missing (e), or just a
# directory (f)
mknod /tmp/d/ptmx c 5 2
mkdir /tmp/d/pts
mount -t devpts devpts /tmp/d/pts -o ptmxmode=666
mknod /tmp/e/ptmx c 5 2
mknod /tmp/f/ptmx c 5 2
ls /tmp/p1 /tmp/p3
stat -c '%n %A %u:%g' /tmp/p1/ptmx /tmp/p3/ptmx
grep ' /tmp/' /proc/self/mountinfo | grep devpts | cut -d ' ' -f 5- | sed 's/^[^ ]* //'
/tmp/pts
umount /tmp/p3 /tmp/p2 /tmp/p1
echo "left: $(grep -c ' /tmp/.* devpts ' /proc/self/mountinfo)"
