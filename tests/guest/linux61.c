/*
 * Answers of Linux 6.1 that kernels after 6.8 changed, so the differential tests, whose native
 * side on CI runs a newer kernel, compare only what both give: a timerfd read split across iovecs
 * is EINVAL and native AIO cannot read a timerfd or a signalfd (6.10 gave both read_iter);
 * epoll_wait reports a buffer outside the address space before a bad epoll fd; FUSE_DEV_IOC_CLONE
 * from a /dev/fuse not attached to a connection is EINVAL; a loop device's block size cannot
 * exceed the page size. Run in the store as root (it needs /dev/fuse and /dev/loop-control). Each
 * check that passes prints "ok <name>"; a failure prints the reason and exits with 1; when all pass
 * it prints "all ok" at the end.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/signalfd.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/timerfd.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            printf("FAIL: ");                                                                      \
            printf(__VA_ARGS__);                                                                   \
            printf(" (errno %d)\n", errno);                                                        \
            exit(1);                                                                               \
        }                                                                                          \
    } while (0)

/* The kernel's struct iocb (linux/aio_abi.h), which musl does not provide */
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
#define IOCB_CMD_PREAD 0
#define FUSE_DEV_IOC_CLONE 0x8004e500
#define LOOP_SET_FD 0x4C00
#define LOOP_CLR_FD 0x4C01
#define LOOP_SET_BLOCK_SIZE 0x4C09
#define LOOP_CTL_REMOVE 0x4C81
#define LOOP_CTL_GET_FREE 0x4C82

static void ok(const char *name)
{
    printf("ok %s\n", name);
    fflush(stdout);
}

static void timerfd_reads(void)
{
    int t = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    CHECK(t >= 0, "timerfd_create");
    struct itimerspec its = {.it_value = {0, 1000000}};
    CHECK(timerfd_settime(t, 0, &its, NULL) == 0, "timerfd_settime");
    usleep(20000);
    uint64_t count = 0;
    struct iovec split[2] = {{&count, 4}, {(char *)&count + 4, 4}};
    CHECK(readv(t, split, 2) == -1 && errno == EINVAL, "readv of a timerfd in two iovecs");
    ok("a timerfd read in two iovecs of 4 bytes: EINVAL");
    CHECK(read(t, &count, 8) == 8 && count == 1, "the expiration is still there, count %llu",
          (unsigned long long)count);
    ok("the expiration is still there for a read of 8 bytes");
    close(t);
}

/* io_submit of a read of 8 bytes from `fd`: 0, or the errno */
static int aio_read(unsigned long ctx, int fd, void *buf)
{
    struct iocb c;
    memset(&c, 0, sizeof c);
    c.aio_lio_opcode = IOCB_CMD_PREAD;
    c.aio_fildes = fd;
    c.aio_buf = (uintptr_t)buf;
    c.aio_nbytes = 8;
    struct iocb *one[] = {&c};
    return syscall(SYS_io_submit, ctx, 1, one) == 1 ? 0 : errno;
}

static void aio_reads(void)
{
    unsigned long ctx = 0;
    CHECK(syscall(SYS_io_setup, 4, &ctx) == 0, "io_setup");
    static char buf[8];
    int zero = open("/dev/zero", O_RDONLY);
    CHECK(aio_read(ctx, zero, buf) == 0, "an AIO read of /dev/zero");
    ok("an AIO read of /dev/zero is submitted");
    int t = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    CHECK(aio_read(ctx, t, buf) == EINVAL, "an AIO read of a timerfd");
    ok("an AIO read of a timerfd: EINVAL");
    sigset_t usr1;
    sigemptyset(&usr1);
    sigaddset(&usr1, SIGUSR1);
    int s = signalfd(-1, &usr1, SFD_NONBLOCK);
    CHECK(s >= 0, "signalfd");
    CHECK(aio_read(ctx, s, buf) == EINVAL, "an AIO read of a signalfd");
    ok("an AIO read of a signalfd: EINVAL");
    close(s);
    close(t);
    close(zero);
    syscall(SYS_io_destroy, ctx);
}

static void epoll_wait_order(void)
{
    errno = 0;
    long r = syscall(SYS_epoll_pwait, 999, (void *)(1UL << 50), 1, 0, NULL, 8);
    CHECK(r == -1 && errno == EFAULT, "epoll_pwait with a bad buffer and a bad fd returned %ld", r);
    ok("epoll_wait with a bad buffer and a bad fd: EFAULT");
}

static void fuse_clone(void)
{
    int fd = open("/dev/fuse", O_RDWR);
    int other = open("/dev/fuse", O_RDWR);
    CHECK(fd >= 0 && other >= 0, "open /dev/fuse");
    uint32_t old = fd;
    CHECK(ioctl(other, FUSE_DEV_IOC_CLONE, &old) == -1 && errno == EINVAL,
          "FUSE_DEV_IOC_CLONE from an unattached /dev/fuse");
    ok("FUSE_DEV_IOC_CLONE from an unattached /dev/fuse: EINVAL");
    close(other);
    close(fd);
}

static void loop_block_size(void)
{
    int ctl = open("/dev/loop-control", O_RDWR);
    CHECK(ctl >= 0, "open /dev/loop-control");
    int n = ioctl(ctl, LOOP_CTL_GET_FREE);
    CHECK(n >= 0, "LOOP_CTL_GET_FREE");
    const char *node = "/tmp/linux61-loop", *backing = "/tmp/linux61-backing";
    unlink(node);
    CHECK(mknod(node, S_IFBLK | 0600, makedev(7, n)) == 0, "mknod");
    int file = open(backing, O_RDWR | O_CREAT | O_TRUNC, 0600);
    CHECK(file >= 0 && ftruncate(file, 65536) == 0, "the backing file");
    int dev = open(node, O_RDWR);
    CHECK(dev >= 0, "open the loop device");
    CHECK(ioctl(dev, LOOP_SET_FD, file) == 0, "LOOP_SET_FD");
    CHECK(ioctl(dev, LOOP_SET_BLOCK_SIZE, 8192) == -1 && errno == EINVAL, "block size 8192");
    ok("a loop block size of 8192: EINVAL");
    CHECK(ioctl(dev, LOOP_SET_BLOCK_SIZE, 4096) == 0, "block size 4096");
    ok("a loop block size of 4096");
    CHECK(ioctl(dev, LOOP_CLR_FD, 0) == 0, "LOOP_CLR_FD");
    close(dev);
    close(file);
    /* Kernels after 6.1 unbind at the last close, a little later (EBUSY until then) */
    for (int i = 0; i < 100 && ioctl(ctl, LOOP_CTL_REMOVE, n) == -1 && errno == EBUSY; i++)
        usleep(20000);
    close(ctl);
    unlink(node);
    unlink(backing);
}

int main(void)
{
    timerfd_reads();
    aio_reads();
    epoll_wait_order();
    fuse_clone();
    loop_block_size();
    printf("all ok\n");
    return 0;
}
