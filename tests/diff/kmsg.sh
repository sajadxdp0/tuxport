# docker-run-options: --device /dev/kmsg --cap-add SYSLOG
# The kernel log's /dev/kmsg and syslog(2)'s read-all (kernel/printk/printk.c of Linux 6.1). The
# native side passes the host's /dev/kmsg to the container and adds CAP_SYSLOG (like root in
# Tuxport). The host's log has other messages, so only the records written this time are looked
# at: each carries a random token, replaced with TOKEN when printed; sequence numbers and
# timestamps differ between the two sides and are not printed. Every write opens a new fd:
# Tuxport's default printk_devkmsg is ratelimit, at most 10 records per open file within 5
# seconds, and the native host's may be on. The buffer size and dmesg_restrict may also differ
# between the two sides, so only whether the results they determine agree is compared
set -e
cd /tmp
cat > kmsg.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

static char token[32];
static int rd;

/* Print s with the token in it replaced by TOKEN */
static void put_masked(const char *s)
{
    size_t n = strlen(token);
    for (const char *p = s; *p;) {
        if (!strncmp(p, token, n)) {
            fputs("TOKEN", stdout);
            p += n;
        } else {
            putchar(*p++);
        }
    }
}

static void result(const char *what, long r)
{
    if (r < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %ld\n", what, r);
}

/* Write once through a new fd */
static long kw(const char *s, size_t len)
{
    int fd = open("/dev/kmsg", O_WRONLY);
    if (fd < 0)
        return -1;
    long r = write(fd, s, len);
    int e = errno;
    close(fd);
    errno = e;
    return r;
}

static void kws(const char *fmt)
{
    char buf[256];
    snprintf(buf, sizeof buf, fmt, token, token);
    result("write", kw(buf, strlen(buf)));
}

/* Print the records readable on rd that carry the token: prefix, flags, text */
static void records(const char *what)
{
    char buf[8192];
    for (;;) {
        ssize_t n = read(rd, buf, sizeof buf - 1);
        if (n < 0) {
            if (errno != EAGAIN)
                printf("%s: read: %s\n", what, strerror(errno));
            if (errno == EPIPE)
                continue;
            return;
        }
        buf[n] = 0;
        if (!strstr(buf, token))
            continue;
        int prefix;
        char flag;
        char *semi = strchr(buf, ';');
        if (!semi || sscanf(buf, "%d,%*u,%*u,%c", &prefix, &flag) != 2) {
            printf("%s: bad record\n", what);
            continue;
        }
        printf("%s: <%d> %c ", what, prefix, flag);
        put_masked(semi + 1);
    }
}

int main(void)
{
    FILE *u = fopen("/dev/urandom", "r");
    unsigned long long r1;
    fread(&r1, sizeof r1, 1, u);
    fclose(u);
    snprintf(token, sizeof token, "kmsg%016llx", r1);

    struct stat st;
    result("stat", stat("/dev/kmsg", &st));
    printf("chr %d %u:%u mode %o\n", S_ISCHR(st.st_mode), major(st.st_rdev), minor(st.st_rdev),
           st.st_mode & 0777);

    rd = open("/dev/kmsg", O_RDONLY | O_NONBLOCK);
    result("open read", rd >= 0 ? 0 : -1);
    result("seek end", lseek(rd, 0, SEEK_END));
    result("seek set 1", lseek(rd, 1, SEEK_SET));
    result("seek cur", lseek(rd, 0, SEEK_CUR));
    result("seek hole", lseek(rd, 0, SEEK_HOLE));
    records("drain");
    struct pollfd p = {.fd = rd, .events = POLLIN | POLLOUT};
    result("poll at the end", poll(&p, 1, 0));

    kws("%s plain\n");
    p.revents = 0;
    result("poll after a write", poll(&p, 1, 0));
    printf("revents %#x\n", p.revents);
    kws("<11>%s user err\n");
    kws("<0>%s level 0\n");
    kws("<>%s empty prefix\n");
    kws("<x>%s not a prefix\n");
    kws("<190>%s local7 info\n");
    kws("<4294967297>%s wraps to 1\n");
    kws("<6>%s no newline");
    kws("%s two newlines\n\n");
    kws("%s a\n%s b\n");
    kws("%s escapes \\ \t \001 \377 \303\251\n");
    char nul[64];
    int len = snprintf(nul, sizeof nul, "%s before a NUL\n", token);
    memcpy(nul + len, "\0after\n", 7);
    result("write with a NUL", kw(nul, len + 7));
    records("read");

    /* A record without a trailing newline is not finalized yet; it can be read only when the next
     * one arrives */
    kws("%s open record");
    p.revents = 0;
    result("poll with only an open record", poll(&p, 1, 0));
    records("before the next record");
    kws("%s closes it\n");
    records("after the next record");

    /* A zero-length write is a record too (an empty one without a newline) */
    char buf[8192];
    result("write nothing", kw("", 0));
    kws("%s after nothing\n");
    for (int i = 0; i < 2; i++) {
        ssize_t n = read(rd, buf, sizeof buf - 1);
        if (n < 0) {
            result("read", n);
            break;
        }
        buf[n] = 0;
        printf("record %d: <%d> ", i, atoi(buf));
        put_masked(strchr(buf, ';') + 1);
    }

    /* 6.1 writes at most 992 bytes at a time (1024 from 6.2 on); only what is the same on both
     * sides is compared */
    char big[1100];
    memset(big, 'a', sizeof big);
    memcpy(big, token, strlen(token));
    result("write 1025", kw(big, 1025));
    big[899] = '\n';
    result("write 900", kw(big, 900));
    for (;;) {
        ssize_t n = read(rd, buf, sizeof buf - 1);
        if (n < 0)
            break;
        buf[n] = 0;
        if (strstr(buf, token))
            printf("long record body: %zu\n", strlen(strchr(buf, ';') + 1));
    }

    int w = open("/dev/kmsg", O_WRONLY);
    char head[] = "<14>", tail[] = " writev in three parts\n";
    struct iovec iov[3] = {{head, 4}, {token, strlen(token)}, {tail, strlen(tail)}};
    result("writev", writev(w, iov, 3));
    struct iovec none[2] = {{head, 0}, {tail, 0}};
    result("writev nothing", writev(w, none, 2));
    result("read a write-only fd", read(w, buf, sizeof buf));
    result("write a read-only fd", write(rd, "x", 1));
    close(w);
    records("writev");

    /* A buffer too small for a record is EINVAL, and that record also counts as read */
    kws("%s first\n");
    kws("%s second\n");
    result("read into 8 bytes", read(rd, buf, 8));
    records("after EINVAL");
    kws("%s pread\n");
    ssize_t n = pread(rd, buf, sizeof buf - 1, 12345);
    if (n > 0) {
        buf[n] = 0;
        printf("pread: ");
        put_masked(strchr(buf, ';') + 1);
    } else {
        result("pread", n);
    }

    /* syslog(2)'s read-all: each line starts with <prefix>[time]; replace the time */
    long size = syscall(SYS_syslog, 10, NULL, 0);
    char *all = malloc(size + 1);
    long got = syscall(SYS_syslog, 3, all, size);
    printf("read all positive: %d\n", got > 0);
    all[got > 0 ? got : 0] = 0;
    for (char *line = strtok(all, "\n"); line; line = strtok(NULL, "\n")) {
        if (!strstr(line, token))
            continue;
        char *close_ = strchr(line, ']');
        char *open_ = strchr(line, '[');
        if (!close_ || !open_)
            continue;
        printf("syslog: %.*s[T]", (int)(open_ - line), line);
        put_masked(close_ + 1);
        putchar('\n');
    }

    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        setresuid(65534, 65534, 65534);
        FILE *f = fopen("/proc/sys/kernel/dmesg_restrict", "r");
        int restrict_ = fgetc(f) == '1';
        fclose(f);
        result("  unprivileged open for writing", open("/dev/kmsg", O_WRONLY));
        int fd = open("/dev/kmsg", O_RDONLY);
        printf("  unprivileged open for reading follows dmesg_restrict: %d\n",
               restrict_ ? fd == -1 && errno == EPERM : fd >= 0);
        fflush(stdout);
        _exit(0);
    }
    waitpid(c, NULL, 0);
    return 0;
}
C
gcc -O2 -Wall -o kmsg kmsg.c && ./kmsg
# util-linux's dmesg reads /dev/kmsg (it is the dmesg on Debian and Ubuntu), busybox's uses
# syslog(2)
t=kmsgsh$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')
m() { grep "$t" | sed "s/$t/TOKEN/g; s/\[ *[0-9]*\.[0-9]*\]/[T]/"; }
echo "<13>$t from the shell" > /dev/kmsg
printf '<11>%s open record' "$t" > /dev/kmsg
echo "<14>$t closes it" > /dev/kmsg
echo "util-linux dmesg"; dmesg | m
echo "raw"; dmesg -r | m
echo "decoded"; dmesg -x | m
echo "level err"; dmesg -l err | m
echo "facility user, level info"; dmesg -f user -l info | m
echo "busybox"; busybox dmesg | m
