# AF_UNIX waits interrupted by a signal whose handler has SA_RESTART: with SO_RCVTIMEO or
# SO_SNDTIMEO set they end with EINTR (accept, connect to a full backlog, stream send to a full
# buffer, datagram send to a full queue, stream, seqpacket and datagram receives), as Linux's
# sock_intr_errno says; without a timeout the call is restarted and completes. The timer stops
# after five signals, so a timed wait that is restarted instead ends at its timeout (EAGAIN)
set -e
cd /tmp
cat > uintr.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t alarms;

static void on_alarm(int sig)
{
    (void)sig;
    if (++alarms == 5) {
        struct itimerval off = {{0, 0}, {0, 0}};
        setitimer(ITIMER_REAL, &off, NULL);
    }
}

// SIGALRM every 20 ms, five times
static void arm(void)
{
    alarms = 0;
    struct itimerval it = {{0, 20000}, {0, 20000}};
    setitimer(ITIMER_REAL, &it, NULL);
}

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static void timeout(int s, int name)
{
    struct timeval tv = {2, 0};
    setsockopt(s, SOL_SOCKET, name, &tv, sizeof tv);
}

static socklen_t abstract(struct sockaddr_un *a, const char *name)
{
    memset(a, 0, sizeof *a);
    a->sun_family = AF_UNIX;
    strcpy(a->sun_path + 1, name);
    return offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name);
}

// Prints how the call ended and whether it came back long before the 2 s timeout
static void show(const char *what, long r, double start)
{
    int e = errno;
    printf("%s: %ld %s, %s\n", what, r, r < 0 ? strerror(e) : "",
           now() - start < 1 ? "before the timeout" : "at the timeout or later");
}

static void receive(int type, const char *what)
{
    int sv[2];
    char buf[8];
    socketpair(AF_UNIX, type, 0, sv);
    timeout(sv[0], SO_RCVTIMEO);
    arm();
    double t = now();
    show(what, recv(sv[0], buf, sizeof buf, 0), t);
    close(sv[0]);
    close(sv[1]);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alarm;
    sa.sa_flags = SA_RESTART;
    sigaction(SIGALRM, &sa, NULL);
    double t;
    char buf[4096];
    memset(buf, 'x', sizeof buf);

    receive(SOCK_STREAM, "stream recv");
    receive(SOCK_SEQPACKET, "seqpacket recv");
    receive(SOCK_DGRAM, "datagram recv");

    // accept with nothing pending
    struct sockaddr_un a;
    socklen_t len = abstract(&a, "uintr-listener");
    int l = socket(AF_UNIX, SOCK_STREAM, 0);
    bind(l, (struct sockaddr *)&a, len);
    listen(l, 0);
    timeout(l, SO_RCVTIMEO);
    arm();
    t = now();
    show("accept", accept(l, NULL, NULL), t);

    // connect to a full backlog: the first connection fills it
    int c1 = socket(AF_UNIX, SOCK_STREAM, 0), c2 = socket(AF_UNIX, SOCK_STREAM, 0);
    t = now();
    show("first connect", connect(c1, (struct sockaddr *)&a, len), t);
    timeout(c2, SO_SNDTIMEO);
    arm();
    t = now();
    show("connect to a full backlog", connect(c2, (struct sockaddr *)&a, len), t);
    close(c1);
    close(c2);
    close(l);

    // stream send once the buffer is full
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    while (send(sv[0], buf, sizeof buf, MSG_DONTWAIT) > 0) {
    }
    timeout(sv[0], SO_SNDTIMEO);
    arm();
    t = now();
    show("stream send to a full buffer", send(sv[0], buf, 1, 0), t);
    close(sv[0]);
    close(sv[1]);

    // datagram send once the receiver holds its max_dgram_qlen datagrams
    len = abstract(&a, "uintr-datagram");
    int r = socket(AF_UNIX, SOCK_DGRAM, 0), s = socket(AF_UNIX, SOCK_DGRAM, 0);
    bind(r, (struct sockaddr *)&a, len);
    int queued = 0;
    while (sendto(s, "d", 1, MSG_DONTWAIT, (struct sockaddr *)&a, len) == 1) {
        queued++;
    }
    printf("datagrams queued before EAGAIN: %d\n", queued);
    timeout(s, SO_SNDTIMEO);
    arm();
    t = now();
    show("datagram send to a full queue", sendto(s, "d", 1, 0, (struct sockaddr *)&a, len), t);
    close(r);
    close(s);

    // Without a timeout SA_RESTART restarts the receive, which gets what a child sends later
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pid_t child = fork();
    if (child == 0) {
        usleep(300000);
        write(sv[1], "late", 4);
        _exit(0);
    }
    arm();
    ssize_t n = recv(sv[0], buf, sizeof buf, 0);
    printf("untimed recv: %zd %s, signals while waiting: %s\n", n, n < 0 ? strerror(errno) : "",
           alarms == 5 ? "five" : "fewer");
    waitpid(child, NULL, 0);
    return 0;
}
C
gcc -O1 -o uintr uintr.c
./uintr
