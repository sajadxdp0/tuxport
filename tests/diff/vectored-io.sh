# readv and writev where Linux's read_iter and write_iter see all the iovecs at once: on datagram
# and seqpacket sockets one call is one message whatever the iovecs, and a plain write or read of
# over 64 KiB is still one message; a readv on a pipe returns what was there instead of waiting
# again once the first iovec is full; an eventfd value read may span iovecs, while its write (a
# plain write operation) goes iovec by iovec; zero-length reads and writes on sockets. A timerfd
# read split across iovecs is not compared: 6.1 and 6.8 read timerfds iovec by iovec (EINVAL),
# while 6.10 gave them read_iter, so CI's newer kernel reads the count across them
# (tests/guest/linux61.c checks 6.1's answer).
cd /tmp
cat > /tmp/vio.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/timerfd.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

static void rc(const char *what, long r)
{
    if (r < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %ld\n", what, r);
}

static void drain(const char *what, int fd)
{
    char buf[300000];
    for (;;) {
        long n = recv(fd, buf, sizeof buf, MSG_DONTWAIT);
        if (n < 0) {
            printf("%s: then %s\n", what, strerror(errno));
            return;
        }
        printf("%s: message of %ld '%.*s'\n", what, n, (int)(n < 20 ? n : 20), buf);
    }
}

static void messages(const char *name, int type)
{
    int sv[2];
    if (socketpair(AF_UNIX, type, 0, sv)) {
        printf("%s: socketpair: %s\n", name, strerror(errno));
        return;
    }
    char what[64];
    struct iovec out[3] = {{"hello", 5}, {", ", 2}, {"world!!!", 8}};
    snprintf(what, sizeof what, "%s writev", name);
    rc(what, writev(sv[0], out, 3));
    snprintf(what, sizeof what, "%s recv", name);
    drain(what, sv[1]);

    rc(what, writev(sv[0], out, 3));
    char a[5], b[5];
    struct iovec in[2] = {{a, 5}, {b, 5}};
    snprintf(what, sizeof what, "%s readv 5+5", name);
    rc(what, readv(sv[1], in, 2));
    printf("  '%.5s' '%.5s'\n", a, b);
    snprintf(what, sizeof what, "%s after readv", name);
    drain(what, sv[1]);

    /* Over 64 KiB in one write and one read */
    static char big[100000];
    memset(big, 'x', sizeof big);
    snprintf(what, sizeof what, "%s write 100000", name);
    rc(what, write(sv[0], big, sizeof big));
    static char got[200000];
    snprintf(what, sizeof what, "%s read 200000", name);
    rc(what, read(sv[1], got, sizeof got));

    /* Zero-length: writev sends an empty message; readv of nothing takes nothing */
    struct iovec none[2] = {{a, 0}, {b, 0}};
    snprintf(what, sizeof what, "%s writev empty", name);
    rc(what, writev(sv[0], none, 2));
    rc(what, write(sv[0], "next", 4));
    snprintf(what, sizeof what, "%s readv empty", name);
    rc(what, readv(sv[1], none, 2));
    snprintf(what, sizeof what, "%s left", name);
    drain(what, sv[1]);
    close(sv[0]);
    close(sv[1]);
}

static void on_alarm(int sig)
{
    (void)sig;
}

int main(void)
{
    messages("unix dgram", SOCK_DGRAM);
    messages("unix seqpacket", SOCK_SEQPACKET);

    /* UDP over loopback */
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in sin = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    bind(u, (struct sockaddr *)&sin, sizeof sin);
    socklen_t sl = sizeof sin;
    getsockname(u, (struct sockaddr *)&sin, &sl);
    connect(u, (struct sockaddr *)&sin, sizeof sin);
    struct iovec out[3] = {{"udp", 3}, {"-", 1}, {"datagram", 8}};
    rc("udp writev", writev(u, out, 3));
    drain("udp recv", u);

    /* A pipe holding exactly the first iovec's worth: readv returns at once */
    struct sigaction sa = {.sa_handler = on_alarm};
    sigaction(SIGALRM, &sa, NULL);
    int p[2];
    pipe(p);
    write(p[1], "abcdef", 6);
    char x[6], y[6];
    struct iovec in[2] = {{x, 6}, {y, 6}};
    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    alarm(3);
    long n = readv(p[0], in, 2);
    alarm(0);
    clock_gettime(CLOCK_MONOTONIC, &t1);
    rc("pipe readv", n);
    printf("  '%.6s' %s\n", x, t1.tv_sec - t0.tv_sec >= 2 ? "after waiting" : "at once");

    /* eventfd: the write goes iovec by iovec (EINVAL for 4 bytes), the read spans iovecs */
    int e = eventfd(0, EFD_NONBLOCK);
    unsigned long long v = 0x0102030405060708ULL, w = 0;
    struct iovec ev_out[2] = {{&v, 4}, {(char *)&v + 4, 4}};
    rc("eventfd writev 4+4", writev(e, ev_out, 2));
    struct iovec ev_whole[2] = {{&v, 8}, {&v, 0}};
    rc("eventfd writev 8+0", writev(e, ev_whole, 2));
    struct iovec ev_in[2] = {{&w, 4}, {(char *)&w + 4, 4}};
    rc("eventfd readv 4+4", readv(e, ev_in, 2));
    printf("  value %#llx\n", w);

    /* timerfd: the count in one iovec */
    int t = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    struct itimerspec its = {.it_value = {0, 1000000}};
    timerfd_settime(t, 0, &its, NULL);
    usleep(20000);
    unsigned long long count = 0;
    struct iovec tf_whole[1] = {{&count, 8}};
    rc("timerfd readv 8", readv(t, tf_whole, 1));
    printf("  count %llu\n", count);
    return 0;
}
C
gcc -O1 -o /tmp/vio /tmp/vio.c && /tmp/vio
rm -f /tmp/vio /tmp/vio.c
