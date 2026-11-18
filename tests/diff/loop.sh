# docker-run-options: --privileged
# Loop devices (Linux 6.1's drivers/block/loop.c and the block-device ioctls): loop-control,
# binding with LOOP_SET_FD and LOOP_CONFIGURE, reads and writes through the device within its
# size, lseek, offset and size limit, capacity, block size, read-only bindings, exclusive opens,
# autoclear on the last close, loops over loops, and the errors of each. The native side is a
# privileged container sharing the host's loop devices, so the program never prints device
# numbers and makes its own node for each device it gets from loop-control (a new device has no
# node in the container's /dev). Left out because they depend on the host kernel's version or
# backing filesystem: direct I/O, discard, BLKBSZGET, BLKPBSZGET, BLKSECTGET, BLKROTATIONAL and
# the default read-ahead, and what a single opener sees after LOOP_CLR_FD (6.1 unbinds at once,
# later kernels at the close).
cd /tmp
rm -rf loopt
mkdir loopt
cat > loopt/loop.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

/* <linux/loop.h>, <linux/fs.h>, <linux/hdreg.h> (the store has no linux-headers) */
#define LO_NAME_SIZE 64
#define LO_FLAGS_READ_ONLY 1
#define LO_FLAGS_AUTOCLEAR 4
struct loop_info {
    int lo_number;
    unsigned lo_device;
    unsigned long lo_inode;
    unsigned lo_rdevice;
    int lo_offset, lo_encrypt_type, lo_encrypt_key_size, lo_flags;
    char lo_name[LO_NAME_SIZE];
    unsigned char lo_encrypt_key[32];
    unsigned long lo_init[2];
    char reserved[4];
};
struct loop_info64 {
    uint64_t lo_device, lo_inode, lo_rdevice, lo_offset, lo_sizelimit;
    uint32_t lo_number, lo_encrypt_type, lo_encrypt_key_size, lo_flags;
    uint8_t lo_file_name[LO_NAME_SIZE], lo_crypt_name[LO_NAME_SIZE], lo_encrypt_key[32];
    uint64_t lo_init[2];
};
struct loop_config {
    uint32_t fd, block_size;
    struct loop_info64 info;
    uint64_t reserved[8];
};
#define LOOP_SET_FD 0x4C00
#define LOOP_CLR_FD 0x4C01
#define LOOP_SET_STATUS 0x4C02
#define LOOP_GET_STATUS 0x4C03
#define LOOP_SET_STATUS64 0x4C04
#define LOOP_GET_STATUS64 0x4C05
#define LOOP_CHANGE_FD 0x4C06
#define LOOP_SET_CAPACITY 0x4C07
#define LOOP_SET_BLOCK_SIZE 0x4C09
#define LOOP_CONFIGURE 0x4C0A
#define LOOP_CTL_ADD 0x4C80
#define LOOP_CTL_REMOVE 0x4C81
#define LOOP_CTL_GET_FREE 0x4C82
#define BLKROGET 0x125e
#define BLKRRPART 0x125f
#define BLKGETSIZE 0x1260
#define BLKRASET 0x1262
#define BLKRAGET 0x1263
#define BLKSSZGET 0x1268
#define BLKGETSIZE64 0x80081272
#define BLKZEROOUT 0x127f
#define HDIO_GETGEO 0x0301

static void rc(const char *what, long r)
{
    if (r < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %ld\n", what, r);
}

static int ctl;

/* A free device from loop-control, opened through our own node */
static int get_dev(const char *node, int flags, int *index)
{
    int n = ioctl(ctl, LOOP_CTL_GET_FREE);
    if (n < 0) {
        printf("get free: %s\n", strerror(errno));
        exit(1);
    }
    unlink(node);
    if (mknod(node, S_IFBLK | 0600, makedev(7, n))) {
        printf("mknod: %s\n", strerror(errno));
        exit(1);
    }
    if (index)
        *index = n;
    int fd = open(node, flags);
    if (fd < 0) {
        printf("open %s: %s\n", node, strerror(errno));
        exit(1);
    }
    return fd;
}

static void sizes(const char *what, int fd)
{
    unsigned long long bytes = 0;
    unsigned long sectors = 0;
    int ssz = 0, ro = -1;
    ioctl(fd, BLKGETSIZE64, &bytes);
    ioctl(fd, BLKGETSIZE, &sectors);
    ioctl(fd, BLKSSZGET, &ssz);
    ioctl(fd, BLKROGET, &ro);
    printf("%s: size %llu sectors %lu ssz %d ro %d\n", what, bytes, sectors, ssz, ro);
}

static void status(const char *what, int fd, int index, const char *backing)
{
    struct loop_info64 info;
    memset(&info, 0xff, sizeof info);
    if (ioctl(fd, LOOP_GET_STATUS64, &info)) {
        printf("%s: get status: %s\n", what, strerror(errno));
        return;
    }
    struct stat st;
    stat(backing, &st);
    printf("%s: number %s offset %llu sizelimit %llu flags %u name '%s' inode %s device %s "
           "rdevice %llu\n",
           what, (int)info.lo_number == index ? "ok" : "wrong",
           (unsigned long long)info.lo_offset, (unsigned long long)info.lo_sizelimit,
           info.lo_flags & (LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR), info.lo_file_name,
           info.lo_inode == st.st_ino ? "ok" : "wrong", info.lo_device == st.st_dev ? "ok" : "wrong",
           (unsigned long long)info.lo_rdevice);
    struct loop_info old;
    rc("  old get status", ioctl(fd, LOOP_GET_STATUS, &old));
    printf("  old: offset %d name '%s'\n", old.lo_offset, old.lo_name);
}

/* Files under /sys/block/loop<index> */
static void sysfs(int index, const char *files)
{
    char list[256], path[128], buf[128];
    strcpy(list, files);
    printf("  sysfs:");
    for (char *f = strtok(list, " "); f; f = strtok(NULL, " ")) {
        snprintf(path, sizeof path, "/sys/block/loop%d/%s", index, f);
        int fd = open(path, O_RDONLY);
        if (fd < 0) {
            printf(" %s=(%s)", f, strerror(errno));
            continue;
        }
        long n = read(fd, buf, sizeof buf - 1);
        close(fd);
        buf[n > 0 ? n - 1 : 0] = 0;
        printf(" %s=%s", f, buf);
    }
    printf("\n");
}

/* GET_STATUS64 once the device is unbound: later kernels may unbind an autoclear device a little
   after its last close, so wait for it as LTP does (up to a second) */
static void unbound(const char *what, const char *node)
{
    struct loop_info64 info;
    int r = 0;
    for (int i = 0; i < 50; i++) {
        int fd = open(node, O_RDONLY);
        r = ioctl(fd, LOOP_GET_STATUS64, &info);
        int e = errno;
        close(fd);
        errno = e;
        if (r < 0)
            break;
        usleep(20000);
    }
    rc(what, r);
}

static void fill(const char *path, size_t size)
{
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    char *buf = malloc(size);
    for (size_t i = 0; i < size; i++)
        buf[i] = 'a' + i / 4096 % 26;
    write(fd, buf, size);
    free(buf);
    close(fd);
}

static void peek(const char *what, const char *path, off_t at, size_t len)
{
    char buf[64] = "";
    int fd = open(path, O_RDONLY);
    long n = pread(fd, buf, len, at);
    close(fd);
    printf("%s: %ld '%.*s'\n", what, n, (int)(n > 0 ? n : 0), buf);
}

int main(void)
{
    char buf[8192];
    ctl = open("/dev/loop-control", O_RDWR);
    if (ctl < 0) {
        printf("loop-control: %s\n", strerror(errno));
        return 1;
    }
    struct stat st;
    stat("/dev/loop-control", &st);
    printf("loop-control: %s %u:%u\n", S_ISCHR(st.st_mode) ? "char" : "?", major(st.st_rdev),
           minor(st.st_rdev));
    rc("control unknown", ioctl(ctl, 0x4c8f, 0));

    fill("/tmp/loopt/back", 65536 + 100);
    int a_index;
    int a = get_dev("/tmp/loopt/a", O_RDWR, &a_index);
    stat("/tmp/loopt/a", &st);
    printf("node: %s major %u\n", S_ISBLK(st.st_mode) ? "block" : "?", major(st.st_rdev));

    /* Unbound (not its read-only state: like Linux, it keeps that of its last binding) */
    unsigned long long bytes = 1;
    ioctl(a, BLKGETSIZE64, &bytes);
    printf("unbound: size %llu\n", bytes);
    rc("unbound read", read(a, buf, 512));
    rc("unbound get status", ioctl(a, LOOP_GET_STATUS64, buf));
    rc("unbound clr", ioctl(a, LOOP_CLR_FD, 0));
    rc("unbound set capacity", ioctl(a, LOOP_SET_CAPACITY, 0));
    rc("unbound set block size", ioctl(a, LOOP_SET_BLOCK_SIZE, 1024));
    rc("unbound seek end", lseek(a, 0, SEEK_END));

    /* Bind */
    int back = open("/tmp/loopt/back", O_RDWR);
    rc("set fd", ioctl(a, LOOP_SET_FD, back));
    rc("set fd again", ioctl(a, LOOP_SET_FD, back));
    sizes("bound", a);
    struct loop_info64 info;
    memset(&info, 0, sizeof info);
    strcpy((char *)info.lo_file_name, "backing-file");
    rc("set status", ioctl(a, LOOP_SET_STATUS64, &info));
    status("bound", a, a_index, "/tmp/loopt/back");
    sysfs(a_index, "size ro removable loop/backing_file loop/offset loop/sizelimit loop/autoclear "
                   "loop/partscan queue/logical_block_size queue/physical_block_size");
    rc("get status null", ioctl(a, LOOP_GET_STATUS64, NULL));

    /* Reads, writes, seeks */
    rc("read", read(a, buf, 10));
    printf("  data '%.10s'\n", buf);
    rc("seek 4096", lseek(a, 4096, SEEK_SET));
    rc("read", read(a, buf, 5));
    printf("  data '%.5s'\n", buf);
    rc("seek end", lseek(a, 0, SEEK_END));
    rc("seek past end", lseek(a, 1, SEEK_END));
    rc("seek negative", lseek(a, -1, SEEK_SET));
    rc("seek data", lseek(a, 100, SEEK_DATA));
    rc("seek hole", lseek(a, 100, SEEK_HOLE));
    rc("seek data at end", lseek(a, 65536, SEEK_DATA));
    rc("pread at end", pread(a, buf, 512, 65536));
    rc("pread across end", pread(a, buf, 1024, 65536 - 512));
    rc("pwrite at end", pwrite(a, "x", 1, 65536));
    rc("pwrite across end", pwrite(a, "ZZZZZZZZ", 8, 65536 - 4));
    rc("pwrite", pwrite(a, "HELLO", 5, 8192));
    rc("write empty", write(a, buf, 0));
    rc("fsync", fsync(a));
    peek("backing at 8192", "/tmp/loopt/back", 8192, 5);
    peek("backing at end", "/tmp/loopt/back", 65536 - 4, 8);
    unsigned long long range[2] = {8192, 512};
    rc("zeroout", ioctl(a, BLKZEROOUT, range));
    peek("backing zeroed", "/tmp/loopt/back", 8192, 5);
    range[0] = 100;
    rc("zeroout unaligned", ioctl(a, BLKZEROOUT, range));
    range[0] = 65536;
    rc("zeroout past end", ioctl(a, BLKZEROOUT, range));

    /* Offset and size limit, capacity, block size */
    info.lo_offset = 4096;
    info.lo_sizelimit = 8192 + 100;
    rc("set status offset", ioctl(a, LOOP_SET_STATUS64, &info));
    sizes("offset", a);
    sysfs(a_index, "size loop/offset loop/sizelimit");
    rc("pread", pread(a, buf, 4, 0));
    printf("  data '%.4s'\n", buf);
    info.lo_offset = 0;
    info.lo_sizelimit = 0;
    ioctl(a, LOOP_SET_STATUS64, &info);
    int grow = open("/tmp/loopt/back", O_RDWR | O_APPEND);
    write(grow, buf, 4096);
    close(grow);
    sizes("before capacity", a);
    rc("set capacity", ioctl(a, LOOP_SET_CAPACITY, 0));
    sizes("after capacity", a);
    rc("block size 1000", ioctl(a, LOOP_SET_BLOCK_SIZE, 1000));
    /* 8192 is left out: above the page size it is EINVAL in 6.1 and 6.8, while CI's newer kernel
     * takes it (tests/guest/linux61.c checks 6.1's answer) */
    rc("block size 1024", ioctl(a, LOOP_SET_BLOCK_SIZE, 1024));
    sizes("block size", a);
    sysfs(a_index, "queue/logical_block_size queue/minimum_io_size");
    int ra = 0;
    rc("raset", ioctl(a, BLKRASET, 512));
    ioctl(a, BLKRAGET, &ra);
    printf("  read-ahead %d\n", ra);

    /* Other ioctls */
    char geo[16];
    rc("getgeo", ioctl(a, HDIO_GETGEO, geo));
    rc("rrpart", ioctl(a, BLKRRPART, 0));
    rc("unknown", ioctl(a, 0x4c42, 0));
    int n;
    rc("fionread", ioctl(a, FIONREAD, &n));
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN};
    rc("epoll add", epoll_ctl(ep, EPOLL_CTL_ADD, a, &ev));
    rc("ftruncate", ftruncate(a, 0));

    /* loop-control on a bound device and on unused numbers */
    rc("control add existing", ioctl(ctl, LOOP_CTL_ADD, a_index) < 0 ? -1 : 0);
    rc("control remove bound", ioctl(ctl, LOOP_CTL_REMOVE, a_index));
    rc("control remove missing", ioctl(ctl, LOOP_CTL_REMOVE, 4000));
    rc("control add 4000", ioctl(ctl, LOOP_CTL_ADD, 4000));
    rc("control remove 4000", ioctl(ctl, LOOP_CTL_REMOVE, 4000));
    rc("control remove negative", ioctl(ctl, LOOP_CTL_REMOVE, -1));

    /* A second device: bad backing files, a loop over the loop */
    int b_index;
    int b = get_dev("/tmp/loopt/b", O_RDWR, &b_index);
    int dir = open("/tmp/loopt", O_RDONLY);
    rc("backing directory", ioctl(b, LOOP_SET_FD, dir));
    rc("backing itself", ioctl(b, LOOP_SET_FD, b));
    rc("backing bad fd", ioctl(b, LOOP_SET_FD, 999));
    int path = open("/tmp/loopt/back", O_PATH);
    rc("backing O_PATH", ioctl(b, LOOP_SET_FD, path));
    rc("backing loop", ioctl(b, LOOP_SET_FD, a));
    sizes("loop over loop", b);
    rc("pread", pread(b, buf, 5, 8192 + 1024));
    printf("  data '%.5s'\n", buf);
    rc("change fd writable", ioctl(a, LOOP_CHANGE_FD, b));
    rc("clr b", ioctl(b, LOOP_CLR_FD, 0));
    close(b);

    /* Exclusive opens */
    int x1 = open("/tmp/loopt/a", O_RDONLY | O_EXCL);
    rc("excl open", x1 < 0 ? -1 : 0);
    rc("second excl open", open("/tmp/loopt/a", O_RDONLY | O_EXCL));
    close(x1);
    int x2 = open("/tmp/loopt/a", O_RDONLY | O_EXCL);
    rc("excl open after close", x2 < 0 ? -1 : 0);
    close(x2);

    /* Clearing while another open holds it: autoclear at the last close */
    int a2 = open("/tmp/loopt/a", O_RDONLY);
    rc("clr with two opens", ioctl(a, LOOP_CLR_FD, 0));
    status("after clr", a2, a_index, "/tmp/loopt/back");
    close(a);
    status("one left", a2, a_index, "/tmp/loopt/back");
    close(a2);
    unbound("after last close", "/tmp/loopt/a");
    sysfs(a_index, "size loop/offset");

    /* Read-only bindings and LOOP_CONFIGURE */
    int ro = open("/tmp/loopt/a", O_RDONLY);
    rc("set fd read-only", ioctl(ro, LOOP_SET_FD, back));
    sizes("read-only", ro);
    status("read-only", ro, a_index, "/tmp/loopt/back");
    sysfs(a_index, "ro");
    int rw = open("/tmp/loopt/a", O_RDWR);
    rc("write read-only", pwrite(rw, "x", 1, 0));
    rc("set status read-only fd", ioctl(ro, LOOP_SET_STATUS64, &info));
    int back2 = open("/tmp/loopt/back", O_RDONLY);
    rc("change fd", ioctl(ro, LOOP_CHANGE_FD, back2));
    fill("/tmp/loopt/small", 4096);
    int small = open("/tmp/loopt/small", O_RDONLY);
    rc("change fd other size", ioctl(ro, LOOP_CHANGE_FD, small));
    rc("clr read-only", ioctl(rw, LOOP_CLR_FD, 0));
    close(rw);
    close(ro);
    a = get_dev("/tmp/loopt/a", O_RDWR, &a_index);
    struct loop_config config;
    memset(&config, 0, sizeof config);
    config.fd = back;
    config.info.lo_flags = 2;
    rc("configure bad flag", ioctl(a, LOOP_CONFIGURE, &config));
    config.info.lo_flags = LO_FLAGS_AUTOCLEAR | LO_FLAGS_READ_ONLY;
    config.block_size = 1000;
    rc("configure bad block size", ioctl(a, LOOP_CONFIGURE, &config));
    config.block_size = 0;
    config.info.lo_encrypt_type = 1;
    rc("configure encryption", ioctl(a, LOOP_CONFIGURE, &config));
    config.info.lo_encrypt_type = 0;
    config.block_size = 4096;
    config.info.lo_offset = 4096;
    strcpy((char *)config.info.lo_file_name, "configured");
    rc("configure", ioctl(a, LOOP_CONFIGURE, &config));
    sizes("configured", a);
    status("configured", a, a_index, "/tmp/loopt/back");
    sysfs(a_index, "ro loop/autoclear loop/offset loop/backing_file queue/logical_block_size");
    rc("write configured read-only", pwrite(a, "x", 1, 0));
    close(a);
    unbound("after autoclear", "/tmp/loopt/a");
    a = open("/tmp/loopt/a", O_RDWR);
    /* Leave the device writable for the next binding, on the host too */
    memset(&config, 0, sizeof config);
    config.fd = back;
    config.info.lo_flags = LO_FLAGS_AUTOCLEAR;
    rc("configure writable", ioctl(a, LOOP_CONFIGURE, &config));
    close(a);
    return 0;
}
C
gcc -O1 -o loopt/loop loopt/loop.c && ./loopt/loop
rm -rf loopt
