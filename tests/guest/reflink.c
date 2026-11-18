/*
 * Guest test program for files sharing their data (FICLONE, FICLONERANGE, a whole-file
 * copy_file_range): build with musl-gcc -static and run it copied into the store (it needs a
 * filesystem). A clone reads the source's data until one of the two is changed through any path
 * (write through the clone's own descriptor or a later one, O_TRUNC, truncate, ftruncate,
 * fallocate, a writable shared mapping), and the other then keeps its contents; mappings made
 * before follow the written file; times, partial ranges, the checks on the arguments, tmpfs and
 * deleted sources. Each item that passes prints
 * "ok <name>"; a failure prints the reason and exits with 1; when all pass it prints "all ok" at
 * the end.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <signal.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            printf("FAIL %s:%d: ", __func__, __LINE__);                                            \
            printf(__VA_ARGS__);                                                                   \
            printf(" (errno %d)\n", errno);                                                        \
            exit(1);                                                                               \
        }                                                                                          \
    } while (0)

/* From <linux/fs.h> */
#define FICLONE 0x40049409
#define FICLONERANGE 0x4020940d
struct file_clone_range {
    int64_t src_fd;
    uint64_t src_offset;
    uint64_t src_length;
    uint64_t dest_offset;
};

#define DIR "/tmp/reflink-test"
#define SIZE (3 * 4096 + 100)

static char data[SIZE];

static void ok(const char *name)
{
    printf("ok %s\n", name);
    fflush(stdout);
}

/* A file holding `data`, made anew */
static void source(const char *path)
{
    unlink(path);
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    CHECK(fd >= 0, "create %s", path);
    CHECK(write(fd, data, SIZE) == SIZE, "write %s", path);
    close(fd);
}

/* An empty file, made anew and open for writing */
static int empty(const char *path)
{
    unlink(path);
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    CHECK(fd >= 0, "create %s", path);
    return fd;
}

/* Whether `path` holds exactly `len` bytes equal to `want` */
static int holds(const char *path, const char *want, size_t len)
{
    static char buf[SIZE + 1];
    int fd = open(path, O_RDONLY);
    if (fd < 0)
        return 0;
    ssize_t n = read(fd, buf, sizeof buf);
    close(fd);
    return n == (ssize_t)len && memcmp(buf, want, len) == 0;
}

static int clone_into(int dst, const char *src)
{
    int s = open(src, O_RDONLY);
    CHECK(s >= 0, "open %s", src);
    int r = ioctl(dst, FICLONE, s);
    int e = errno;
    close(s);
    errno = e;
    return r;
}

/* The clone reads the source's data; a write through the clone's own descriptor changes only it,
 * and descriptors opened on the clone before see the write */
static void write_through_clone(void)
{
    source(DIR "/a");
    int b = empty(DIR "/b");
    CHECK(clone_into(b, DIR "/a") == 0, "FICLONE");
    struct stat st;
    CHECK(fstat(b, &st) == 0 && st.st_size == SIZE, "size %lld", (long long)st.st_size);
    CHECK(holds(DIR "/b", data, SIZE), "clone contents");
    int reader = open(DIR "/b", O_RDONLY);
    CHECK(reader >= 0, "open reader");
    CHECK(pwrite(b, "XY", 2, 4096) == 2, "write the clone");
    char buf[2];
    CHECK(pread(reader, buf, 2, 4096) == 2 && memcmp(buf, "XY", 2) == 0, "reader sees the write");
    CHECK(holds(DIR "/a", data, SIZE), "source unchanged");
    close(reader);
    close(b);
    ok("write_through_clone");
}

/* Writing the source after cloning it leaves the clone as it was */
static void write_source(void)
{
    source(DIR "/a");
    int b = empty(DIR "/b");
    CHECK(clone_into(b, DIR "/a") == 0, "FICLONE");
    close(b);
    int a = open(DIR "/a", O_WRONLY | O_APPEND);
    CHECK(a >= 0 && write(a, "tail", 4) == 4, "append to the source");
    close(a);
    CHECK(holds(DIR "/b", data, SIZE), "clone unchanged");
    struct stat st;
    CHECK(stat(DIR "/a", &st) == 0 && st.st_size == SIZE + 4, "source grew");
    ok("write_source");
}

/* The source keeps its times; the clone's modification time is the clone's; times set on one do
 * not reach the other */
static void times(void)
{
    source(DIR "/a");
    struct timespec t[2] = {{1000, 1}, {2000, 2}};
    CHECK(utimensat(AT_FDCWD, DIR "/a", t, 0) == 0, "utimensat");
    time_t before = time(NULL);
    int b = empty(DIR "/b");
    CHECK(clone_into(b, DIR "/a") == 0, "FICLONE");
    struct stat sa, sb;
    CHECK(stat(DIR "/a", &sa) == 0 && sa.st_mtim.tv_sec == 2000 && sa.st_mtim.tv_nsec == 2 &&
              sa.st_atim.tv_sec == 1000,
          "source times %lld", (long long)sa.st_mtim.tv_sec);
    CHECK(fstat(b, &sb) == 0 && sb.st_mtim.tv_sec >= before - 1, "clone mtime %lld",
          (long long)sb.st_mtim.tv_sec);
    struct timespec u[2] = {{3000, 3}, {4000, 4}};
    CHECK(futimens(b, u) == 0, "futimens");
    CHECK(fstat(b, &sb) == 0 && sb.st_mtim.tv_sec == 4000 && sb.st_atim.tv_sec == 3000,
          "clone times");
    CHECK(stat(DIR "/a", &sa) == 0 && sa.st_mtim.tv_sec == 2000, "source times kept");
    /* Written, the clone's modification time is the write's */
    CHECK(write(b, "x", 1) == 1, "write");
    CHECK(fstat(b, &sb) == 0 && sb.st_mtim.tv_sec >= before - 1 && sb.st_atim.tv_sec == 3000,
          "times after the write %lld %lld", (long long)sb.st_mtim.tv_sec,
          (long long)sb.st_atim.tv_sec);
    close(b);
    ok("times");
}

/* Truncation: O_TRUNC, truncate, ftruncate and fallocate through a clone change only it */
static void truncation(void)
{
    source(DIR "/a");
    int b = empty(DIR "/b");
    CHECK(clone_into(b, DIR "/a") == 0, "FICLONE");
    close(b);
    CHECK(truncate(DIR "/b", 10) == 0, "truncate");
    CHECK(holds(DIR "/b", data, 10), "truncated clone");
    int c = empty(DIR "/c");
    CHECK(clone_into(c, DIR "/a") == 0, "FICLONE");
    CHECK(ftruncate(c, 4096) == 0, "ftruncate");
    CHECK(holds(DIR "/c", data, 4096), "ftruncated clone");
    CHECK(fallocate(c, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, 4096) == 0, "punch");
    close(c);
    int d = empty(DIR "/d");
    CHECK(clone_into(d, DIR "/a") == 0, "FICLONE");
    close(d);
    d = open(DIR "/d", O_WRONLY | O_TRUNC);
    CHECK(d >= 0, "O_TRUNC");
    close(d);
    CHECK(holds(DIR "/d", data, 0), "emptied clone");
    CHECK(holds(DIR "/a", data, SIZE), "source unchanged");
    ok("truncation");
}

/* A writable shared mapping of a clone writes only the clone */
static void shared_mapping(void)
{
    source(DIR "/a");
    int b = empty(DIR "/b");
    CHECK(clone_into(b, DIR "/a") == 0, "FICLONE");
    char *p = mmap(NULL, SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, b, 0);
    CHECK(p != MAP_FAILED, "mmap");
    CHECK(memcmp(p, data, SIZE) == 0, "mapped contents");
    memcpy(p + 100, "mapped", 6);
    CHECK(msync(p, SIZE, MS_SYNC) == 0, "msync");
    char buf[6];
    CHECK(pread(b, buf, 6, 100) == 6 && memcmp(buf, "mapped", 6) == 0, "read the mapped write");
    CHECK(holds(DIR "/a", data, SIZE), "source unchanged");
    munmap(p, SIZE);
    close(b);
    ok("shared_mapping");
}

/* A read-only shared mapping of a clone, made before the clone is written through another
 * descriptor, shows the write, while one of the source keeps the source's data; one of a clone
 * truncated afterwards gets SIGBUS past the new end */
static void mappings_follow(void)
{
    source(DIR "/a");
    int b = empty(DIR "/b");
    CHECK(clone_into(b, DIR "/a") == 0, "FICLONE");
    close(b);
    int rb = open(DIR "/b", O_RDONLY), ra = open(DIR "/a", O_RDONLY);
    CHECK(rb >= 0 && ra >= 0, "open");
    char *pb = mmap(NULL, SIZE, PROT_READ, MAP_SHARED, rb, 0);
    char *pa = mmap(NULL, SIZE, PROT_READ, MAP_SHARED, ra, 0);
    CHECK(pb != MAP_FAILED && pa != MAP_FAILED, "mmap");
    CHECK(memcmp(pb, data, SIZE) == 0, "mapped clone");
    int w = open(DIR "/b", O_RDWR);
    CHECK(w >= 0 && pwrite(w, "NEW", 3, 8192) == 3, "write the clone");
    CHECK(memcmp(pb + 8192, "NEW", 3) == 0, "the clone's mapping shows the write");
    CHECK(memcmp(pa, data, SIZE) == 0, "the source's mapping keeps its data");
    close(w);
    munmap(pb, SIZE);
    munmap(pa, SIZE);
    close(ra);

    int c = empty(DIR "/c");
    CHECK(clone_into(c, DIR "/a") == 0, "FICLONE");
    close(c);
    int rc = open(DIR "/c", O_RDONLY);
    char *pc = mmap(NULL, SIZE, PROT_READ, MAP_SHARED, rc, 0);
    CHECK(pc != MAP_FAILED && pc[8192] == data[8192], "mapped clone");
    CHECK(truncate(DIR "/c", 4096) == 0, "truncate");
    CHECK(memcmp(pc, data, 4096) == 0, "the kept part");
    pid_t pid = fork();
    if (pid == 0) {
        volatile char x = pc[8192];
        (void)x;
        _exit(0);
    }
    int status;
    CHECK(waitpid(pid, &status, 0) == pid && WIFSIGNALED(status) && WTERMSIG(status) == SIGBUS,
          "past the end: status %#x", status);
    munmap(pc, SIZE);
    close(rc);
    close(rb);
    ok("mappings_follow");
}

/* A whole-file copy_file_range shares too, advancing both positions */
static void copy_whole(void)
{
    source(DIR "/a");
    int a = open(DIR "/a", O_RDONLY);
    int b = empty(DIR "/b");
    CHECK(copy_file_range(a, NULL, b, NULL, 1 << 30, 0) == SIZE, "copy_file_range");
    CHECK(lseek(a, 0, SEEK_CUR) == SIZE && lseek(b, 0, SEEK_CUR) == SIZE, "positions");
    CHECK(copy_file_range(a, NULL, b, NULL, 1 << 30, 0) == 0, "at the end");
    CHECK(holds(DIR "/b", data, SIZE), "copy contents");
    CHECK(write(b, "more", 4) == 4, "append");
    CHECK(holds(DIR "/a", data, SIZE), "source unchanged");
    close(a);
    close(b);
    ok("copy_whole");
}

/* FICLONERANGE: a block-aligned range copies; unaligned offsets, a range cut short by the
 * source's end or one overlapping itself are EINVAL; a length of 0 reaches the source's end */
static void ranges(void)
{
    source(DIR "/a");
    int a = open(DIR "/a", O_RDONLY);
    int b = empty(DIR "/b");
    struct file_clone_range r = {.src_fd = a, .src_offset = 4096, .src_length = 4096};
    CHECK(ioctl(b, FICLONERANGE, &r) == 0, "range");
    CHECK(holds(DIR "/b", data + 4096, 4096), "range contents");
    r.dest_offset = 8192;
    r.src_length = 0;
    CHECK(ioctl(b, FICLONERANGE, &r) == 0, "range to the end");
    char buf[SIZE];
    CHECK(pread(b, buf, SIZE, 8192) == SIZE - 4096 && memcmp(buf, data + 4096, SIZE - 4096) == 0,
          "range to the end contents");
    r.src_offset = 100;
    CHECK(ioctl(b, FICLONERANGE, &r) == -1 && errno == EINVAL, "unaligned");
    r.src_offset = 0;
    r.src_length = SIZE + 4096;
    CHECK(ioctl(b, FICLONERANGE, &r) == -1 && errno == EINVAL, "past the end");
    r.src_length = 8192;
    r.dest_offset = 4096;
    r.src_fd = b;
    CHECK(ioctl(b, FICLONERANGE, &r) == -1 && errno == EINVAL, "overlapping itself");
    close(a);
    close(b);
    ok("ranges");
}

/* A clone outlives its deleted source; a source already deleted but still open clones too; a
 * clone made while the source is open for writing is a copy of that moment */
static void sources(void)
{
    source(DIR "/a");
    int b = empty(DIR "/b");
    CHECK(clone_into(b, DIR "/a") == 0, "FICLONE");
    CHECK(unlink(DIR "/a") == 0, "unlink");
    CHECK(holds(DIR "/b", data, SIZE), "clone of a deleted source");
    close(b);
    source(DIR "/a");
    int a = open(DIR "/a", O_RDONLY);
    CHECK(a >= 0 && unlink(DIR "/a") == 0, "open and unlink");
    b = empty(DIR "/b");
    CHECK(ioctl(b, FICLONE, a) == 0, "FICLONE from a deleted file");
    close(a);
    sync();
    CHECK(holds(DIR "/b", data, SIZE), "clone from a deleted file");
    close(b);
    source(DIR "/a");
    int w = open(DIR "/a", O_WRONLY);
    CHECK(w >= 0, "open for writing");
    b = empty(DIR "/b");
    CHECK(clone_into(b, DIR "/a") == 0, "FICLONE");
    CHECK(pwrite(w, "Z", 1, 0) == 1, "write the source");
    CHECK(holds(DIR "/b", data, SIZE), "clone of a written source");
    close(w);
    close(b);
    ok("sources");
}

/* tmpfs has no reflink, and files on different mounts are EXDEV */
static void other_filesystems(void)
{
    source("/dev/shm/reflink-a");
    int b = empty("/dev/shm/reflink-b");
    CHECK(clone_into(b, "/dev/shm/reflink-a") == -1 && errno == EOPNOTSUPP, "tmpfs");
    int c = empty(DIR "/c");
    CHECK(clone_into(c, "/dev/shm/reflink-a") == -1 && errno == EXDEV, "across mounts");
    close(b);
    close(c);
    unlink("/dev/shm/reflink-a");
    unlink("/dev/shm/reflink-b");
    ok("other_filesystems");
}

int main(void)
{
    for (int i = 0; i < SIZE; i++)
        data[i] = (char)(i * 7 + i / 4096);
    mkdir(DIR, 0755);
    write_through_clone();
    write_source();
    times();
    truncation();
    shared_mapping();
    mappings_follow();
    copy_whole();
    ranges();
    sources();
    other_filesystems();
    printf("all ok\n");
    return 0;
}
