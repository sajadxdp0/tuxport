# Native AIO beyond compat.sh's basics (Linux 6.1's fs/aio.c): a pending IOCB_CMD_POLL completes
# from the file's wake-up, so its eventfd is signalled without anyone in io_getevents (the poll,
# resfd and epoll pattern), also for a host socket; the eventfd is the one aio_resfd named at
# submission; io_cancel's completion signals it too; a poll that has become ready can no longer be
# cancelled; io_getevents waits for a timerfd or a host socket to become ready. Writes to an
# O_APPEND file or with RWF_APPEND go to the end whatever the offset, leaving the file position
# alone; a negative offset or length is EINVAL from io_submit, even on a pipe; events whose copy
# out faults stay queued. Every wait has a time limit, and alarm ends a run that would hang.
set -e
cd /tmp
cat > aio2.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

/* The image has no linux-headers: definitions from linux/aio_abi.h */
typedef unsigned long aio_context_t;
enum { IOCB_CMD_PREAD = 0, IOCB_CMD_PWRITE = 1, IOCB_CMD_FSYNC = 2, IOCB_CMD_POLL = 5, IOCB_CMD_PWRITEV = 8 };
#define IOCB_FLAG_RESFD 1
struct iocb {
    uint64_t aio_data;
    uint32_t aio_key, aio_rw_flags;
    uint16_t aio_lio_opcode;
    int16_t aio_reqprio;
    uint32_t aio_fildes;
    uint64_t aio_buf, aio_nbytes;
    int64_t aio_offset;
    uint64_t aio_reserved2;
    uint32_t aio_flags, aio_resfd;
};
struct io_event {
    uint64_t data, obj;
    int64_t res, res2;
};

static aio_context_t ctx;

static struct iocb cb(int fd, int op, const void *buf, size_t n, long long off, uint64_t data)
{
    struct iocb c;
    memset(&c, 0, sizeof c);
    c.aio_fildes = fd;
    c.aio_lio_opcode = op;
    c.aio_buf = (uintptr_t)buf;
    c.aio_nbytes = n;
    c.aio_offset = off;
    c.aio_data = data;
    return c;
}

/* A second context for the submission checks, so that what they queue stays apart */
static aio_context_t ctx2;

static void check(const char *what, struct iocb c)
{
    struct iocb *one[] = {&c};
    errno = 0;
    long r = syscall(SYS_io_submit, ctx2, 1, one);
    printf("%s: %s\n", what, r == 1 ? "submitted" : strerror(errno));
}

static struct iocb with_resfd(struct iocb c, int efd)
{
    c.aio_flags = IOCB_FLAG_RESFD;
    c.aio_resfd = efd;
    return c;
}

static long submit(struct iocb *c)
{
    struct iocb *one[] = {c};
    return syscall(SYS_io_submit, ctx, 1, one);
}

static void events(long min, const char *what)
{
    struct io_event ev[8];
    struct timespec t = {0, 200000000};
    long n = syscall(SYS_io_getevents, ctx, min, 8, ev, &t);
    printf("%s: %ld", what, n);
    for (long i = 0; i < n; i++)
        printf(" [data %llu res %lld]", (unsigned long long)ev[i].data, (long long)ev[i].res);
    printf("\n");
}

static unsigned long long count(int efd)
{
    uint64_t n = 0;
    /* Linux signals the eventfd of a cancelled poll from a workqueue: wait for it a little */
    poll(&(struct pollfd){.fd = efd, .events = POLLIN}, 1, 1000);
    if (read(efd, &n, 8) != 8)
        return 0;
    return n;
}

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

/* A thread that connects to the port after 100 ms and then stays (blocked on a pipe), so that
 * nothing else happens in the kernel meanwhile */
static int port, idle[2];

static void *connect_later(void *arg)
{
    (void)arg;
    usleep(100000);
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    connect(s, (struct sockaddr *)&a, sizeof a);
    char c;
    read(idle[0], &c, 1);
    return NULL;
}

static int listener(void)
{
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET};
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    bind(s, (struct sockaddr *)&a, sizeof a);
    listen(s, 4);
    socklen_t len = sizeof a;
    getsockname(s, (struct sockaddr *)&a, &len);
    port = ntohs(a.sin_port);
    return s;
}

static void show(int fd, const char *what)
{
    char buf[32] = {0};
    pread(fd, buf, sizeof buf - 1, 0);
    printf("%s: \"%s\", position %ld, O_APPEND %d\n", what, buf, (long)lseek(fd, 0, SEEK_CUR),
           !!(fcntl(fd, F_GETFL) & O_APPEND));
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    alarm(30);
    pipe(idle);
    printf("setup %ld\n", syscall(SYS_io_setup, 16, &ctx));

    /* The eventfd is signalled when the pipe becomes readable, without io_getevents */
    int p[2], efd = eventfd(0, EFD_NONBLOCK), ep = epoll_create1(0);
    pipe(p);
    struct epoll_event ev = {.events = EPOLLIN}, out;
    epoll_ctl(ep, EPOLL_CTL_ADD, efd, &ev);
    struct iocb pl = with_resfd(cb(p[0], IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 0, 0, 1), efd);
    submit(&pl);
    printf("epoll before data %d\n", epoll_wait(ep, &out, 1, 0));
    write(p[1], "x", 1);
    printf("epoll after data %d\n", epoll_wait(ep, &out, 1, 2000));
    printf("eventfd count %llu\n", count(efd));
    events(1, "poll with eventfd");

    /* The same for a poll on a listening TCP socket (a host socket in Tuxport) */
    int ls = listener(), efd2 = eventfd(0, EFD_NONBLOCK), ep2 = epoll_create1(0);
    epoll_ctl(ep2, EPOLL_CTL_ADD, efd2, &ev);
    struct iocb pt = with_resfd(cb(ls, IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 0, 0, 2), efd2);
    submit(&pt);
    pthread_t t;
    pthread_create(&t, NULL, connect_later, NULL);
    printf("epoll after connect %d\n", epoll_wait(ep2, &out, 1, 2000));
    printf("eventfd count %llu\n", count(efd2));
    struct io_event e;
    struct timespec limit = {1, 0};
    long n = syscall(SYS_io_getevents, ctx, 1, 1, &e, &limit);
    printf("socket poll: %ld data %llu readable %d\n", n, (unsigned long long)e.data,
           !!(e.res & POLLIN));
    close(ls);

    /* The eventfd is the one named at submission: closing it and reusing its number changes
     * nothing */
    int q[2], e1 = eventfd(0, EFD_NONBLOCK);
    pipe(q);
    int kept = dup(e1);
    struct iocb pr = with_resfd(cb(q[0], IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 0, 0, 3), e1);
    submit(&pr);
    close(e1);
    int e2 = eventfd(0, EFD_NONBLOCK);
    printf("number reused %d\n", e2 == e1);
    write(q[1], "x", 1);
    events(1, "poll after the fd number changed");
    printf("submitted eventfd %llu, new eventfd %llu\n", count(kept), count(e2));

    /* io_cancel's completion signals the eventfd */
    int r[2], e3 = eventfd(0, EFD_NONBLOCK);
    pipe(r);
    struct iocb pc = with_resfd(cb(r[0], IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 0, 0, 4), e3);
    submit(&pc);
    printf("cancel %ld %s\n", syscall(SYS_io_cancel, ctx, &pc, &e), strerror(errno));
    events(1, "after cancel");
    printf("eventfd after cancel %llu\n", count(e3));

    /* A poll that has become ready has completed and can no longer be cancelled */
    int s[2];
    pipe(s);
    struct iocb pd = cb(s[0], IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 0, 0, 5);
    submit(&pd);
    write(s[1], "x", 1);
    errno = 0;
    printf("cancel after data %ld %s\n", syscall(SYS_io_cancel, ctx, &pd, &e), strerror(errno));
    events(1, "after the late cancel");

    /* io_getevents waits for a timerfd and for a host socket to become ready */
    int tfd = timerfd_create(CLOCK_MONOTONIC, 0);
    struct itimerspec in100 = {.it_value = {0, 100000000}};
    timerfd_settime(tfd, 0, &in100, NULL);
    struct iocb ptf = cb(tfd, IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 0, 0, 6);
    submit(&ptf);
    struct timespec three = {3, 0};
    double t0 = now();
    n = syscall(SYS_io_getevents, ctx, 1, 1, &e, &three);
    printf("timerfd: %ld data %llu readable %d in time %d\n", n, (unsigned long long)e.data,
           !!(e.res & POLLIN), now() - t0 < 1.5);
    ls = listener();
    struct iocb pls = cb(ls, IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 0, 0, 7);
    submit(&pls);
    pthread_create(&t, NULL, connect_later, NULL);
    t0 = now();
    n = syscall(SYS_io_getevents, ctx, 1, 1, &e, &three);
    printf("socket: %ld data %llu readable %d in time %d\n", n, (unsigned long long)e.data,
           !!(e.res & POLLIN), now() - t0 < 1.5);

    /* Writes to an O_APPEND file, or with RWF_APPEND, go to the end, whatever the offset */
    int fa = open("/tmp/aio-append", O_RDWR | O_CREAT | O_TRUNC | O_APPEND, 0644);
    write(fa, "12345", 5);
    lseek(fa, 1, SEEK_SET);
    struct iocb wa = cb(fa, IOCB_CMD_PWRITE, "ab", 2, 0, 10);
    printf("submit %ld\n", submit(&wa));
    struct iovec iov = {"cd", 2};
    struct iocb wv = cb(fa, IOCB_CMD_PWRITEV, &iov, 1, 2, 11);
    printf("submit %ld\n", submit(&wv));
    events(2, "O_APPEND writes");
    show(fa, "O_APPEND file");
    int fb = open("/tmp/aio-rwf", O_RDWR | O_CREAT | O_TRUNC, 0644);
    write(fb, "12345", 5);
    struct iocb wb = cb(fb, IOCB_CMD_PWRITE, "ef", 2, 0, 12);
    wb.aio_rw_flags = RWF_APPEND;
    struct iocb ws = cb(fb, IOCB_CMD_PWRITE, "g", 1, 1, 13);
    ws.aio_rw_flags = RWF_DSYNC;
    struct iocb wf = cb(fb, IOCB_CMD_PWRITE, "h", 1, 2, 14);
    wf.aio_rw_flags = RWF_SYNC;
    struct iocb *three_writes[] = {&wb, &ws, &wf};
    printf("submit %ld\n", syscall(SYS_io_submit, ctx, 3, three_writes));
    events(3, "RWF_APPEND, RWF_DSYNC and RWF_SYNC writes");
    show(fb, "file written with flags");

    /* A negative offset or length is EINVAL, even where the offset is not used */
    char buf[16];
    struct iocb neg = cb(fb, IOCB_CMD_PREAD, buf, 4, -1, 20);
    errno = 0;
    printf("negative offset %ld %s\n", submit(&neg), strerror(errno));
    int u[2];
    pipe(u);
    write(u[1], "z", 1);
    struct iocb negp = cb(u[0], IOCB_CMD_PREAD, buf, 1, -1, 21);
    errno = 0;
    printf("negative offset on a pipe %ld %s\n", submit(&negp), strerror(errno));
    struct iocb negl = cb(fb, IOCB_CMD_PREAD, buf, (size_t)-1, 0, 22);
    errno = 0;
    printf("negative length %ld %s\n", submit(&negl), strerror(errno));
    events(0, "nothing queued");

    /* Events whose copy out faults stay queued */
    struct iocb rd = cb(fb, IOCB_CMD_PREAD, buf, 4, 0, 30);
    submit(&rd);
    errno = 0;
    n = syscall(SYS_io_getevents, ctx, 1, 1, (struct io_event *)8, NULL);
    printf("bad events buffer %ld %s\n", n, strerror(errno));
    events(1, "after the fault");

    /* What io_submit refuses before running anything (6.1's aio_poll, aio_fsync, aio_read and
     * aio_write): fields a poll or fsync does not use, files without fsync or without
     * read_iter/write_iter, O_PATH fds; and the rw flags are checked before the open mode */
    syscall(SYS_io_setup, 64, &ctx2);
    int v[2];
    pipe(v);
    struct iocb c = cb(v[0], IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 0, 1, 40);
    check("poll with an offset", c);
    check("poll with a length", cb(v[0], IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 1, 0, 40));
    c = cb(v[0], IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 0, 0, 40);
    c.aio_rw_flags = 1;
    check("poll with rw flags", c);
    check("poll with events above 16 bits",
          cb(v[0], IOCB_CMD_POLL, (void *)(uintptr_t)(0x10000 | POLLIN), 0, 0, 40));
    check("fsync with a buffer", cb(fb, IOCB_CMD_FSYNC, buf, 0, 0, 41));
    check("fsync with an offset", cb(fb, IOCB_CMD_FSYNC, NULL, 0, 1, 41));
    check("fsync with a length", cb(fb, IOCB_CMD_FSYNC, NULL, 1, 0, 41));
    c = cb(fb, IOCB_CMD_FSYNC, NULL, 0, 0, 41);
    c.aio_rw_flags = 2;
    check("fsync with rw flags", c);
    check("fdatasync with a buffer", cb(fb, 3, buf, 0, 0, 41));
    check("fsync of a pipe", cb(v[1], IOCB_CMD_FSYNC, NULL, 0, 0, 41));
    check("fsync of a file", cb(fb, IOCB_CMD_FSYNC, NULL, 0, 0, 41));
    int ifd = inotify_init1(IN_NONBLOCK);
    int pfd = syscall(SYS_pidfd_open, getpid(), 0);
    int dfd = open("/tmp", O_RDONLY | O_DIRECTORY);
    struct mq_attr qa = {.mq_maxmsg = 4, .mq_msgsize = 8};
    mqd_t mq = mq_open("/aio-queue", O_RDWR | O_CREAT, 0600, &qa);
    int full = open("/dev/full", O_WRONLY), zero = open("/dev/zero", O_RDWR);
    /* Not a timerfd or signalfd: 6.10 gave them read_iter, so CI's newer kernel reads them while
     * 6.1 and 6.8 answer EINVAL (tests/guest/linux61.c checks 6.1's answer) */
    struct { const char *name; int fd; } reads[] = {
        {"inotify", ifd}, {"epoll", ep}, {"pidfd", pfd},
        {"directory", dfd}, {"message queue", mq}, {"eventfd", efd}, {"/dev/zero", zero},
    };
    char what[64];
    for (unsigned i = 0; i < sizeof reads / sizeof *reads; i++) {
        snprintf(what, sizeof what, "read of a %s", reads[i].name);
        check(what, cb(reads[i].fd, IOCB_CMD_PREAD, buf, 8, 0, 42));
    }
    uint64_t one = 1;
    struct { const char *name; int fd; } writes[] = {
        {"timerfd", tfd}, {"epoll", ep}, {"message queue", mq}, {"eventfd", efd},
        {"/dev/full", full}, {"/dev/zero", zero},
    };
    for (unsigned i = 0; i < sizeof writes / sizeof *writes; i++) {
        snprintf(what, sizeof what, "write of a %s", writes[i].name);
        check(what, cb(writes[i].fd, IOCB_CMD_PWRITE, &one, 8, 0, 43));
    }
    mq_unlink("/aio-queue");
    int wonly = open("/tmp/aio-rwf", O_WRONLY);
    c = cb(wonly, IOCB_CMD_PREAD, buf, 4, 0, 44);
    c.aio_rw_flags = 0x80000;
    check("read of a write-only file with an unknown flag", c);
    int opath = open("/tmp/aio-rwf", O_PATH);
    check("poll of an O_PATH fd", cb(opath, IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 0, 0, 45));
    check("read of an O_PATH fd", cb(opath, IOCB_CMD_PREAD, buf, 4, 0, 45));
    check("fsync of an O_PATH fd", cb(opath, IOCB_CMD_FSYNC, NULL, 0, 0, 45));
    return 0;
}
C
gcc -O1 -o aio2 aio2.c -pthread
./aio2
