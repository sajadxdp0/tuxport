# docker-run-options: --privileged
# Gaps left by the twelfth map-bug triage: sendfile and splice need a file that can splice at
# that end, so /dev/full as the output and /dev/null as the input are EINVAL (as an eventfd
# output is, whatever the count), with nothing consumed; and an instance cannot turn read-only
# while a file of it whose last name was removed is still in use (an fd, an O_PATH fd, a cwd, an
# fd in flight in an AF_UNIX message, a running program, an overlay file) or while a file is open
# for writing through a lazily unmounted bind of it, while a bind remount ignores deleted files and
# a memfd, open for writing or not, does not keep the root busy
set -e
cd /tmp
cat > splicedev.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/sendfile.h>
#include <unistd.h>

static void report(const char *what, ssize_t n)
{
    if (n < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %zd\n", what, n);
}

int main(void)
{
    int fd = open("/tmp/src", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    write(fd, "12345678", 8);
    close(fd);
    int src = open("/tmp/src", O_RDONLY);
    int full = open("/dev/full", O_WRONLY);
    int null = open("/dev/null", O_RDWR);
    int zero = open("/dev/zero", O_RDONLY);
    int dst = open("/tmp/dst", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    int efd = eventfd(0, EFD_NONBLOCK);
    int p[2], left;
    pipe(p);

    report("sendfile file -> /dev/full", sendfile(full, src, NULL, 8));
    printf("input position: %ld\n", (long)lseek(src, 0, SEEK_CUR));
    write(p[1], "abcdefgh", 8);
    report("splice pipe -> /dev/full", splice(p[0], NULL, full, NULL, 8, 0));
    ioctl(p[0], FIONREAD, &left);
    printf("left in the pipe: %d\n", left);
    report("sendfile file -> eventfd", sendfile(efd, src, NULL, 8));
    uint64_t v;
    report("eventfd read", read(efd, &v, 8));
    printf("input position: %ld\n", (long)lseek(src, 0, SEEK_CUR));
    report("sendfile /dev/null -> file", sendfile(dst, null, NULL, 8));
    report("sendfile /dev/null -> pipe", sendfile(p[1], null, NULL, 8));
    report("splice /dev/null -> pipe", splice(null, NULL, p[1], NULL, 8, 0));
    report("sendfile /dev/zero -> /dev/null", sendfile(null, zero, NULL, 8));
    report("splice pipe -> /dev/null", splice(p[0], NULL, null, NULL, 8, 0));
    report("sendfile file -> /dev/null", sendfile(null, src, NULL, 8));
    return 0;
}
C
cc -o splicedev splicedev.c
./splicedev

cat > rorm.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

/* Remounts `path` read-only (the instance, or with MS_BIND the mount) and back */
static void ro(const char *what, const char *path, unsigned long bind)
{
    int r = mount(NULL, path, NULL, MS_REMOUNT | MS_RDONLY | bind, NULL);
    printf("%s: %s\n", what, r ? strerror(errno) : "ok");
    if (r == 0 && mount(NULL, path, NULL, MS_REMOUNT | bind, NULL))
        printf("  back to read-write: %s\n", strerror(errno));
}

static void touch(const char *path)
{
    close(open(path, O_WRONLY | O_CREAT, 0755));
}

static void send_fd(int sock, int fd)
{
    char data = 'x';
    struct iovec iov = {&data, 1};
    union {
        struct cmsghdr h;
        char buf[CMSG_SPACE(sizeof(int))];
    } u;
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.buf,
                         .msg_controllen = sizeof u.buf};
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &fd, sizeof fd);
    sendmsg(sock, &msg, 0);
}

static int recv_fd(int sock)
{
    char data;
    struct iovec iov = {&data, 1};
    union {
        struct cmsghdr h;
        char buf[CMSG_SPACE(sizeof(int))];
    } u;
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.buf,
                         .msg_controllen = sizeof u.buf};
    int fd = -1;
    if (recvmsg(sock, &msg, 0) == 1 && CMSG_FIRSTHDR(&msg))
        memcpy(&fd, CMSG_DATA(CMSG_FIRSTHDR(&msg)), sizeof fd);
    return fd;
}

static void copy(const char *from, const char *to)
{
    char buf[65536];
    int in = open(from, O_RDONLY), out = open(to, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    ssize_t n;
    while ((n = read(in, buf, sizeof buf)) > 0)
        write(out, buf, n);
    close(in);
    close(out);
}

int main(int argc, char **argv)
{
    if (argc > 1) {
        pause();
        return 0;
    }
    mkdir("/tmp/r", 0755);
    if (mount("tmpfs", "/tmp/r", "tmpfs", 0, NULL)) {
        perror("mount tmpfs");
        return 1;
    }

    touch("/tmp/r/f");
    int fd = open("/tmp/r/f", O_RDONLY);
    ro("file open for reading", "/tmp/r", 0);
    unlink("/tmp/r/f");
    ro("deleted file still open", "/tmp/r", 0);
    ro("deleted file still open, bind remount", "/tmp/r", MS_BIND);
    close(fd);
    ro("deleted file closed", "/tmp/r", 0);

    touch("/tmp/r/p");
    fd = open("/tmp/r/p", O_PATH);
    unlink("/tmp/r/p");
    ro("deleted file held by O_PATH", "/tmp/r", 0);
    close(fd);

    mkdir("/tmp/r/d", 0755);
    chdir("/tmp/r/d");
    rmdir("/tmp/r/d");
    ro("removed cwd", "/tmp/r", 0);
    chdir("/tmp");
    ro("cwd left", "/tmp/r", 0);

    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    touch("/tmp/r/s");
    fd = open("/tmp/r/s", O_RDONLY);
    send_fd(sv[0], fd);
    close(fd);
    unlink("/tmp/r/s");
    ro("deleted file in flight", "/tmp/r", 0);
    close(recv_fd(sv[1]));
    ro("received and closed", "/tmp/r", 0);
    close(sv[0]);
    close(sv[1]);

    copy("/proc/self/exe", "/tmp/r/prog");
    pid_t pid = fork();
    if (pid == 0) {
        execl("/tmp/r/prog", "prog", "wait", (char *)NULL);
        _exit(127);
    }
    char link[PATH_MAX];
    for (int i = 0; i < 500; i++) {
        char path[64];
        snprintf(path, sizeof path, "/proc/%d/exe", pid);
        ssize_t n = readlink(path, link, sizeof link - 1);
        if (n > 0 && (link[n] = 0, strcmp(link, "/tmp/r/prog") == 0))
            break;
        usleep(10000);
    }
    unlink("/tmp/r/prog");
    ro("deleted program running", "/tmp/r", 0);
    kill(pid, SIGKILL);
    waitpid(pid, NULL, 0);
    ro("program gone", "/tmp/r", 0);

    mkdir("/tmp/b", 0755);
    mount("/tmp/r", "/tmp/b", NULL, MS_BIND, NULL);
    fd = open("/tmp/b/w", O_WRONLY | O_CREAT, 0644);
    umount2("/tmp/b", MNT_DETACH);
    ro("writer through a detached bind", "/tmp/r", 0);
    close(fd);
    ro("writer closed", "/tmp/r", 0);
    umount2("/tmp/r", MNT_DETACH);

    mkdir("/tmp/ov", 0755);
    mount("tmpfs", "/tmp/ov", "tmpfs", 0, NULL);
    mkdir("/tmp/ov/l", 0755);
    mkdir("/tmp/ov/u", 0755);
    mkdir("/tmp/ov/w", 0755);
    mkdir("/tmp/ov/m", 0755);
    touch("/tmp/ov/l/low");
    if (mount("overlay", "/tmp/ov/m", "overlay", 0,
              "lowerdir=/tmp/ov/l,upperdir=/tmp/ov/u,workdir=/tmp/ov/w")) {
        perror("mount overlay");
        return 1;
    }
    touch("/tmp/ov/m/up");
    fd = open("/tmp/ov/m/up", O_RDONLY);
    int low = open("/tmp/ov/m/low", O_RDONLY);
    unlink("/tmp/ov/m/up");
    ro("deleted overlay file still open", "/tmp/ov/m", 0);
    close(fd);
    unlink("/tmp/ov/m/low");
    ro("deleted lower file still open", "/tmp/ov/m", 0);
    close(low);
    ro("overlay files closed", "/tmp/ov/m", 0);
    umount2("/tmp/ov/m", MNT_DETACH);
    umount2("/tmp/ov", MNT_DETACH);

    int m = memfd_create("m", 0);
    write(m, "x", 1);
    ro("root with a memfd open", "/", 0);
    ro("root with a memfd open, bind remount", "/", MS_BIND);
    close(m);
    return 0;
}
C
cc -o rorm rorm.c
./rorm
