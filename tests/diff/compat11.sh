# docker-run-options: --privileged
# The twelfth map-bug triage's file items: sendfile from a device keeps the input position when
# the output takes nothing, and write and writev check RLIMIT_FSIZE once per call, so one that
# reaches the limit exactly is only shortened (no SIGXFSZ until a later call starts at it); a file
# without a name reopened through /proc/self/fd is on the mount it was opened through (noexec,
# read-only, st_dev), keeps its path and reports IN_OPEN; truncate(2) through /proc/self/fd
# truncates a deleted file or memfd, within its seals; `..` from a directory renamed out of a
# bind's subtree through another mount is ENOENT; tmpfs's uid= and gid= are IDs in the mounter's
# user namespace; a message queue fd that seccomp's ADDFD replaces in another process ends that
# process's mq_notify registration; and an overlay file copied up while its lower file is open for
# writing or mapped writable keeps its data when the lower file is then written
set -e
cd /tmp
cat > pos.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/sendfile.h>
#include <sys/uio.h>
#include <unistd.h>

static volatile sig_atomic_t xfsz;

static void on_xfsz(int sig)
{
    (void)sig;
    xfsz++;
}

static void report(const char *what, ssize_t n)
{
    if (n < 0)
        printf("%s: %s", what, strerror(errno));
    else
        printf("%s: %zd", what, n);
    printf(", SIGXFSZ %d\n", (int)xfsz);
}

/* sendfile from a device whose output takes nothing: the input position stays where it was
 * (fdinfo shows it; lseek on these devices always answers 0) */
static void device(const char *what, const char *dev, int out)
{
    int in = open(dev, O_RDONLY);
    ssize_t n = sendfile(out, in, NULL, 4096);
    printf("%s: %s, ", what, n < 0 ? strerror(errno) : "sent");
    fflush(stdout);
    char cmd[64];
    snprintf(cmd, sizeof cmd, "grep pos: /proc/%d/fdinfo/%d", getpid(), in);
    system(cmd);
    close(in);
}

int main(void)
{
    int p[2];
    pipe2(p, O_NONBLOCK);
    static char fill[65536];
    while (write(p[1], fill, sizeof fill) > 0)
        ;
    device("/dev/zero into a full pipe", "/dev/zero", p[1]);
    device("/dev/urandom into a full pipe", "/dev/urandom", p[1]);

    signal(SIGXFSZ, on_xfsz);
    struct rlimit rl = {65536, RLIM_INFINITY};
    setrlimit(RLIMIT_FSIZE, &rl);
    static char buf[131072];
    int f = open("fsize.out", O_RDWR | O_CREAT | O_TRUNC, 0644);
    report("write past the limit", write(f, buf, sizeof buf));
    report("write at the limit", write(f, buf, 1));
    ftruncate(f, 0);
    lseek(f, 0, SEEK_SET);
    struct iovec iov[2] = {{buf, 65536}, {buf, 65536}};
    report("writev ending the first iovec at the limit", writev(f, iov, 2));
    ftruncate(f, 0);
    lseek(f, 65536 - 100, SEEK_SET);
    report("write across the limit", write(f, buf, 200));
    close(f);
    unlink("fsize.out");
    return 0;
}
C
gcc -O1 -o pos pos.c
./pos
rm -f pos pos.c

mkdir -p /tmp/nx /tmp/ro
mount -t tmpfs -o noexec none /tmp/nx
mount -t tmpfs none /tmp/ro
cp /bin/true /tmp/nx/t
echo data > /tmp/ro/f
cat > reopen.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int reopen(int fd, int flags)
{
    char p[32];
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    return open(p, flags);
}

static void link_of(const char *what, int fd)
{
    char p[32], t[256];
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    ssize_t n = readlink(p, t, sizeof t - 1);
    t[n < 0 ? 0 : n] = 0;
    if (!strncmp(t, "pipe:[", 6))
        strcpy(t, "pipe:[N]");
    printf("%s: %s\n", what, t);
}

static void say(const char *what, int r)
{
    printf("%s: %s\n", what, r < 0 ? strerror(errno) : "ok");
}

int main(void)
{
    /* A deleted program on a noexec mount, reopened: still noexec */
    int prog = open("/tmp/nx/t", O_RDONLY);
    unlink("/tmp/nx/t");
    int again = reopen(prog, O_RDONLY);
    struct stat a, b;
    fstat(prog, &a);
    fstat(again, &b);
    printf("reopened program: same st_dev %d\n", a.st_dev == b.st_dev);
    link_of("reopened program", again);
    pid_t c = fork();
    if (!c) {
        char *argv[] = {"t", NULL}, *envp[] = {NULL};
        fexecve(again, argv, envp);
        printf("fexecve of the reopened program: %s\n", strerror(errno));
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);

    /* A deleted file on a mount made read-only since: no write reopen */
    int ro = open("/tmp/ro/f", O_RDONLY);
    int in = inotify_init1(IN_NONBLOCK);
    inotify_add_watch(in, "/tmp/ro/f", IN_OPEN);
    unlink("/tmp/ro/f");
    int r = reopen(ro, O_RDONLY);
    struct inotify_event ev[4];
    ssize_t n = read(in, ev, sizeof ev);
    printf("reopen reports IN_OPEN: %d\n", n > 0 && (ev[0].mask & IN_OPEN) != 0);
    close(r);
    /* A bind remount: remounting the filesystem itself read-only is EBUSY on Linux while a
     * deleted file of it is open */
    say("bind remount read-only", mount(NULL, "/tmp/ro", NULL, MS_REMOUNT | MS_BIND | MS_RDONLY, NULL));
    say("write reopen on a read-only mount", reopen(ro, O_RDWR));
    say("read reopen on it", reopen(ro, O_RDONLY));

    /* Truncation through /proc/self/fd: a deleted file and memfds, within their seals */
    char p[32];
    int del = open("/tmp/del.f", O_RDWR | O_CREAT | O_TRUNC, 0600);
    write(del, "0123456789", 10);
    unlink("/tmp/del.f");
    snprintf(p, sizeof p, "/proc/self/fd/%d", del);
    say("truncate a deleted file by its fd link", truncate(p, 4));
    fstat(del, &a);
    printf("its size: %lld\n", (long long)a.st_size);
    int m = memfd_create("m", MFD_ALLOW_SEALING);
    write(m, "0123456789", 10);
    link_of("memfd", m);
    int m2 = reopen(m, O_RDWR);
    link_of("reopened memfd", m2);
    snprintf(p, sizeof p, "/proc/self/fd/%d", m);
    say("truncate a memfd by its fd link", truncate(p, 3));
    fstat(m, &a);
    printf("its size: %lld\n", (long long)a.st_size);
    fcntl(m, F_ADD_SEALS, F_SEAL_SHRINK);
    say("truncate a shrink-sealed memfd by its fd link", truncate(p, 1));
    say("grow it", truncate(p, 8));
    snprintf(p, sizeof p, "/proc/self/fd/%d", m2);
    say("truncate the reopened memfd by its fd link", truncate(p, 9));
    fstat(m, &a);
    printf("its size: %lld\n", (long long)a.st_size);
    int pp[2];
    pipe(pp);
    int pr = reopen(pp[0], O_RDONLY);
    link_of("reopened pipe", pr);
    return 0;
}
C
gcc -O1 -o reopen reopen.c
./reopen
rm -f reopen reopen.c
umount /tmp/nx /tmp/ro
rmdir /tmp/nx /tmp/ro

mkdir -p /tmp/b/sub/dir/in /tmp/b/out /tmp/bm
mount --bind /tmp/b/sub /tmp/bm
cat > dotdot.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static void up(const char *what, int dir, const char *path)
{
    int fd = openat(dir, path, O_RDONLY | O_DIRECTORY);
    printf("%s: %s\n", what, fd < 0 ? strerror(errno) : "found");
}

int main(void)
{
    int dir = open("/tmp/bm/dir", O_RDONLY | O_DIRECTORY);
    int in = open("/tmp/bm/dir/in", O_RDONLY | O_DIRECTORY);
    up(".. inside the bind", dir, "..");
    rename("/tmp/b/sub/dir", "/tmp/b/out/dir");
    up(".. of a directory moved out of the bind", dir, "..");
    up("../.. below it", in, "../..");
    up(".. below it", in, "..");
    rename("/tmp/b/out/dir", "/tmp/b/sub/dir");
    up(".. once moved back", dir, "..");
    return 0;
}
C
gcc -O1 -o dotdot dotdot.c
./dotdot
rm -f dotdot dotdot.c
umount /tmp/bm
rm -rf /tmp/b /tmp/bm

mkdir -p /tmp/tu
cat > tmpfsid.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static void put(pid_t pid, const char *file, const char *text)
{
    char p[64];
    snprintf(p, sizeof p, "/proc/%d/%s", pid, file);
    int fd = open(p, O_WRONLY);
    write(fd, text, strlen(text));
    close(fd);
}

int main(void)
{
    int up[2], down[2];
    pipe(up);
    pipe(down);
    char c;
    pid_t pid = fork();
    if (!pid) {
        unshare(CLONE_NEWUSER | CLONE_NEWNS);
        write(up[1], "u", 1);
        read(down[0], &c, 1);
        int r = mount("none", "/tmp/tu", "tmpfs", 0, "uid=5");
        printf("uid= unmapped in the namespace: %s\n", r ? strerror(errno) : "mounted");
        r = mount("none", "/tmp/tu", "tmpfs", 0, "uid=0,gid=0,mode=755");
        printf("uid=0,gid=0 in the namespace: %s\n", r ? strerror(errno) : "mounted");
        struct stat st;
        stat("/tmp/tu", &st);
        printf("owner inside: %u %u\n", st.st_uid, st.st_gid);
        fflush(stdout);
        system("grep ' /tmp/tu ' /proc/self/mountinfo | awk '{print $NF}'");
        write(up[1], "m", 1);
        read(down[0], &c, 1);
        _exit(0);
    }
    read(up[0], &c, 1);
    put(pid, "uid_map", "0 1000 1");
    put(pid, "setgroups", "deny");
    put(pid, "gid_map", "0 1000 1");
    write(down[1], "g", 1);
    read(up[0], &c, 1);
    char p[64];
    snprintf(p, sizeof p, "/proc/%d/root/tmp/tu", pid);
    struct stat st;
    stat(p, &st);
    printf("owner outside: %u %u\n", st.st_uid, st.st_gid);
    write(down[1], "x", 1);
    waitpid(pid, NULL, 0);
    return 0;
}
C
gcc -O1 -o tmpfsid tmpfsid.c
./tmpfsid
rm -f tmpfsid tmpfsid.c
rmdir /tmp/tu

cat > addfd.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* No linux-headers: define them ourselves */
struct sock_filter {
    uint16_t code;
    uint8_t jt, jf;
    uint32_t k;
};
struct sock_fprog {
    unsigned short len;
    struct sock_filter *filter;
};
struct seccomp_notif {
    uint64_t id;
    uint32_t pid, flags;
    int nr;
    uint32_t arch;
    uint64_t ip, args[6];
};
struct seccomp_notif_resp {
    uint64_t id;
    int64_t val;
    int32_t error;
    uint32_t flags;
};
struct seccomp_notif_addfd {
    uint64_t id;
    uint32_t flags, srcfd, newfd, newfd_flags;
};
#define NOTIF_RECV 0xc0502100
#define NOTIF_SEND 0xc0182101
#define NOTIF_ADDFD 0x40182103

int main(void)
{
    struct mq_attr at = {.mq_maxmsg = 4, .mq_msgsize = 16};
    mq_unlink("/addfd");
    mqd_t q = mq_open("/addfd", O_RDWR | O_CREAT, 0600, &at);
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pid_t c = fork();
    if (!c) {
        struct sigevent ev = {.sigev_notify = SIGEV_NONE};
        printf("target registers: %s\n", mq_notify(q, &ev) ? strerror(errno) : "ok");
        fflush(stdout);
        struct sock_filter f[] = {
            {0x20, 0, 0, 0},                   /* A = nr */
            {0x15, 0, 1, SYS_getppid},         /* nr == getppid? */
            {0x06, 0, 0, 0x7fc00000U},         /* USER_NOTIF */
            {0x06, 0, 0, 0x7fff0000U},         /* ALLOW */
        };
        struct sock_fprog prog = {4, f};
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        int l = syscall(SYS_seccomp, 1, 8, &prog);
        char b = 0;
        struct iovec iov = {&b, 1};
        char cbuf[CMSG_SPACE(sizeof(int))] = {0};
        struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf,
                           .msg_controllen = sizeof cbuf};
        struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
        cm->cmsg_level = SOL_SOCKET;
        cm->cmsg_type = SCM_RIGHTS;
        cm->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(cm), &l, sizeof l);
        sendmsg(sv[1], &m, 0);
        syscall(SYS_getppid);
        _exit(0);
    }
    char b;
    struct iovec iov = {&b, 1};
    char cbuf[CMSG_SPACE(sizeof(int))];
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf,
                       .msg_controllen = sizeof cbuf};
    recvmsg(sv[0], &m, 0);
    int l;
    memcpy(&l, CMSG_DATA(CMSG_FIRSTHDR(&m)), sizeof l);
    struct seccomp_notif n;
    memset(&n, 0, sizeof n);
    ioctl(l, NOTIF_RECV, &n);
    struct sigevent ev = {.sigev_notify = SIGEV_NONE};
    printf("supervisor while the target is registered: %s\n",
           mq_notify(q, &ev) ? strerror(errno) : "ok");
    int null = open("/dev/null", O_RDONLY);
    struct seccomp_notif_addfd a = {.id = n.id, .flags = 1, .srcfd = null, .newfd = q};
    printf("addfd over the target's queue fd: %s\n",
           ioctl(l, NOTIF_ADDFD, &a) < 0 ? strerror(errno) : "ok");
    printf("supervisor once it is replaced: %s\n", mq_notify(q, &ev) ? strerror(errno) : "ok");
    struct seccomp_notif_resp r = {.id = n.id};
    ioctl(l, NOTIF_SEND, &r);
    waitpid(c, NULL, 0);
    mq_unlink("/addfd");
    return 0;
}
C
gcc -O1 -o addfd addfd.c
./addfd
rm -f addfd addfd.c

mkdir -p /tmp/ol
mount -t tmpfs none /tmp/ol
mkdir /tmp/ol/lower /tmp/ol/upper /tmp/ol/work /tmp/ol/m
echo lower-a > /tmp/ol/lower/a
echo lower-b > /tmp/ol/lower/b
mount -t overlay overlay -o lowerdir=/tmp/ol/lower,upperdir=/tmp/ol/upper,workdir=/tmp/ol/work /tmp/ol/m
cat > copyup.c <<'C'
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

static void show(const char *what, const char *path)
{
    char b[16] = {0};
    int fd = open(path, O_RDONLY);
    read(fd, b, sizeof b - 1);
    close(fd);
    b[strcspn(b, "\n")] = 0;
    printf("%s: %s\n", what, b);
}

int main(void)
{
    int w = open("/tmp/ol/lower/a", O_RDWR);
    chmod("/tmp/ol/m/a", 0600);
    pwrite(w, "CHANGED", 7, 0);
    show("copied up while the lower file was open for writing", "/tmp/ol/m/a");
    int fd = open("/tmp/ol/lower/b", O_RDWR);
    char *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    chmod("/tmp/ol/m/b", 0600);
    memcpy(p, "CHANGED", 7);
    msync(p, 4096, MS_SYNC);
    show("copied up while the lower file was mapped writable", "/tmp/ol/m/b");
    return 0;
}
C
gcc -O1 -o copyup copyup.c
./copyup
rm -f copyup copyup.c
umount /tmp/ol/m
umount /tmp/ol
rmdir /tmp/ol
