# epoll edge triggering and EPOLLEXCLUSIVE: when data arrives while already ready, it must trigger
# again (mio's waker relies on this; tokio programs used to hang), and an exclusive interest wakes
# only one process per edge (the voluntary context switch count of the processes not picked does
# not change)
set -e
cd /tmp
rm -rf /tmp/etdir
cat > et.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <netinet/in.h>
#include <pthread.h>
#include <pty.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/signalfd.h>
#include <sys/stat.h>
#include <sys/socket.h>
#include <sys/timerfd.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

static int ep;

static int wait1(int ms)
{
    struct epoll_event e;
    return epoll_wait(ep, &e, 1, ms);
}

/* Watch fd for events (edge-triggered) and poke twice without reading in between: report how many
 * events there are after each poke */
static void twice(const char *name, int fd, uint32_t events, void (*poke)(void *), void *arg)
{
    ep = epoll_create1(0);
    struct epoll_event e = {.events = events | EPOLLET};
    if (epoll_ctl(ep, EPOLL_CTL_ADD, fd, &e) < 0) {
        printf("%s: epoll_ctl: %s\n", name, strerror(errno));
        return;
    }
    poke(arg);
    int a = wait1(100);
    poke(arg);
    int b = wait1(100);
    int c = wait1(50);
    printf("%-14s first %d, again %d, quiet %d\n", name, a, b, c);
    close(ep);
}

static void write_byte(void *arg) { write(*(int *)arg, "x", 1); }
static void write_one(void *arg) { uint64_t one = 1; write(*(int *)arg, &one, 8); }
static void read_one(void *arg) { uint64_t v; read(*(int *)arg, &v, 8); }

static struct sockaddr_in tcp_addr;
static void connect_tcp(void *arg)
{
    (void)arg;
    int c = socket(AF_INET, SOCK_STREAM, 0);
    connect(c, (void *)&tcp_addr, sizeof tcp_addr);
}

static struct sockaddr_un unix_addr;
static void connect_unix(void *arg)
{
    (void)arg;
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    connect(c, (void *)&unix_addr, sizeof unix_addr);
}

static void touch(void *arg)
{
    static int n;
    char path[64];
    snprintf(path, sizeof path, "%s/f%d", (char *)arg, n++);
    close(open(path, O_CREAT | O_WRONLY, 0600));
}

static void raise_rt(void *arg)
{
    (void)arg;
    union sigval v = {0};
    sigqueue(getpid(), SIGRTMIN, v);
}

static void mq_put(void *arg) { mq_send(*(mqd_t *)arg, "m", 1, 0); }

static void arm(void *arg)
{
    struct itimerspec it = {.it_value = {0, 1000000}};
    timerfd_settime(*(int *)arg, 0, &it, NULL);
    usleep(20000);
}

static void ready_kinds(void)
{
    int efd = eventfd(0, EFD_NONBLOCK);
    twice("eventfd", efd, EPOLLIN, write_one, &efd);
    /* The writing side: every read makes it trigger again */
    int wfd = eventfd(5, EFD_NONBLOCK);
    twice("eventfd write", wfd, EPOLLOUT, read_one, &wfd);

    int p[2];
    pipe(p);
    twice("pipe", p[0], EPOLLIN, write_byte, &p[1]);

    int s[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, s);
    twice("unix stream", s[0], EPOLLIN, write_byte, &s[1]);
    socketpair(AF_UNIX, SOCK_DGRAM, 0, s);
    twice("unix dgram", s[0], EPOLLIN, write_byte, &s[1]);

    int ul = socket(AF_UNIX, SOCK_STREAM, 0);
    unix_addr.sun_family = AF_UNIX;
    strcpy(unix_addr.sun_path, "/tmp/et.sock");
    unlink(unix_addr.sun_path);
    bind(ul, (void *)&unix_addr, sizeof unix_addr);
    listen(ul, 8);
    twice("unix listen", ul, EPOLLIN, connect_unix, NULL);

    int l = socket(AF_INET, SOCK_STREAM, 0);
    tcp_addr.sin_family = AF_INET;
    tcp_addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t al = sizeof tcp_addr;
    bind(l, (void *)&tcp_addr, sizeof tcp_addr);
    listen(l, 8);
    getsockname(l, (void *)&tcp_addr, &al);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    connect(c, (void *)&tcp_addr, sizeof tcp_addr);
    int sv = accept(l, 0, 0);
    twice("tcp", sv, EPOLLIN, write_byte, &c);
    twice("tcp listen", l, EPOLLIN, connect_tcp, NULL);

    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in ua = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    al = sizeof ua;
    bind(u, (void *)&ua, sizeof ua);
    getsockname(u, (void *)&ua, &al);
    int uc = socket(AF_INET, SOCK_DGRAM, 0);
    connect(uc, (void *)&ua, sizeof ua);
    twice("udp", u, EPOLLIN, write_byte, &uc);

    int master, slave;
    struct termios raw;
    openpty(&master, &slave, NULL, NULL, NULL);
    tcgetattr(slave, &raw);
    cfmakeraw(&raw);
    tcsetattr(slave, TCSANOW, &raw);
    twice("pty slave", slave, EPOLLIN, write_byte, &master);
    twice("pty master", master, EPOLLIN, write_byte, &slave);

    int tfd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    twice("timerfd", tfd, EPOLLIN, arm, &tfd);

    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGRTMIN);
    sigprocmask(SIG_BLOCK, &set, NULL);
    int sfd = signalfd(-1, &set, SFD_NONBLOCK);
    twice("signalfd", sfd, EPOLLIN, raise_rt, NULL);

    mkdir("/tmp/etdir", 0700);
    int ifd = inotify_init1(IN_NONBLOCK);
    inotify_add_watch(ifd, "/tmp/etdir", IN_CREATE);
    twice("inotify", ifd, EPOLLIN, touch, "/tmp/etdir");

    struct mq_attr ma = {.mq_maxmsg = 8, .mq_msgsize = 16};
    mq_unlink("/et");
    mqd_t q = mq_open("/et", O_RDWR | O_CREAT | O_NONBLOCK, 0600, &ma);
    twice("mqueue", q, EPOLLIN, mq_put, &q);
    mq_unlink("/et");
}

/* mio's waker: an eventfd watched edge-triggered whose count is never read, where every write by
 * another thread must wake epoll_wait once */
static int waker, acks[2];

static void *wake_thread(void *arg)
{
    (void)arg;
    for (int i = 0; i < 5; i++) {
        uint64_t one = 1;
        write(waker, &one, 8);
        char c;
        read(acks[0], &c, 1);
    }
    return NULL;
}

static void mio_waker(void)
{
    waker = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    pipe(acks);
    ep = epoll_create1(0);
    struct epoll_event e = {.events = EPOLLIN | EPOLLRDHUP | EPOLLET};
    epoll_ctl(ep, EPOLL_CTL_ADD, waker, &e);
    pthread_t t;
    pthread_create(&t, NULL, wake_thread, NULL);
    int woken = 0;
    while (woken < 5 && wait1(2000) == 1) {
        woken++;
        write(acks[1], "a", 1);
    }
    printf("mio waker: woken %d of 5\n", woken);
    if (woken < 5)
        exit(1);
    pthread_join(t, NULL);
}

/* voluntary_ctxt_switches in /proc/<pid>/status: one more each time the process goes to sleep; -1
 * when it cannot be read */
static long switches(pid_t pid)
{
    char path[64], line[128];
    snprintf(path, sizeof path, "/proc/%d/status", pid);
    FILE *f = fopen(path, "r");
    long n = -1;
    while (f && fgets(line, sizeof line, f))
        if (sscanf(line, "voluntary_ctxt_switches: %ld", &n) == 1)
            break;
    if (f)
        fclose(f);
    return n;
}

/* Several processes, each with its own epoll, wait on the same listening socket with
 * EPOLLEXCLUSIVE (level-triggered): for each incoming connection only one process returns from
 * epoll_wait (the same without accept), and only it was ever woken — the voluntary switch count
 * of the ones not picked does not change */
static void exclusive(const char *name, int l, void (*poke)(void *))
{
    int ready[2], out[2];
    pipe(ready);
    pipe(out);
    fcntl(out[0], F_SETFL, O_NONBLOCK);
    pid_t pids[3];
    for (int i = 0; i < 3; i++) {
        if ((pids[i] = fork()) == 0) {
            int e = epoll_create1(0);
            struct epoll_event ev = {.events = EPOLLIN | EPOLLEXCLUSIVE};
            epoll_ctl(e, EPOLL_CTL_ADD, l, &ev);
            write(ready[1], "r", 1);
            struct epoll_event got;
            if (epoll_wait(e, &got, 1, 10000) == 1)
                write(out[1], "w", 1);
            pause();
            _exit(0);
        }
    }
    char c;
    for (int i = 0; i < 3; i++)
        read(ready[0], &c, 1);
    usleep(300000);
    long before[3];
    for (int i = 0; i < 3; i++)
        before[i] = switches(pids[i]);
    poke(NULL);
    usleep(300000);
    int woken = 0, returned = 0;
    for (int i = 0; i < 3; i++)
        woken += switches(pids[i]) != before[i];
    while (read(out[0], &c, 1) == 1)
        returned++;
    for (int i = 0; i < 3; i++)
        kill(pids[i], SIGKILL);
    while (wait(NULL) > 0)
        ;
    printf("exclusive %-11s returned %d, woken %d\n", name, returned, woken);
    close(ready[0]);
    close(ready[1]);
    close(out[0]);
    close(out[1]);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    ready_kinds();
    mio_waker();

    int ul = socket(AF_UNIX, SOCK_STREAM, 0);
    strcpy(unix_addr.sun_path, "/tmp/ex.sock");
    unlink(unix_addr.sun_path);
    bind(ul, (void *)&unix_addr, sizeof unix_addr);
    listen(ul, 8);
    exclusive("unix listen", ul, connect_unix);

    int l = socket(AF_INET, SOCK_STREAM, 0);
    tcp_addr.sin_port = 0;
    socklen_t al = sizeof tcp_addr;
    bind(l, (void *)&tcp_addr, sizeof tcp_addr);
    listen(l, 8);
    getsockname(l, (void *)&tcp_addr, &al);
    exclusive("tcp listen", l, connect_tcp);
    return 0;
}
C
gcc -O1 -o et et.c
./et

# Nested epoll: when the outer one watches the inner one edge-triggered, every event on a file the
# inner one watches (even when already ready) triggers the outer one again, also with two levels
# of nesting; the outer one is also woken while waiting in epoll_wait
cat > nested.c <<'C'
#define _GNU_SOURCE
#include <netinet/in.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static int wait1(int ep, int ms)
{
    struct epoll_event e;
    return epoll_wait(ep, &e, 1, ms);
}

/* The inner epoll watches fd with inner_events, the outer one watches the inner edge-triggered;
 * after each poke report how many times the outer one reports */
static void nested(const char *name, int fd, uint32_t inner_events, int wfd, size_t len)
{
    int inner = epoll_create1(0), outer = epoll_create1(0);
    struct epoll_event e = {.events = inner_events};
    epoll_ctl(inner, EPOLL_CTL_ADD, fd, &e);
    struct epoll_event o = {.events = EPOLLIN | EPOLLET};
    epoll_ctl(outer, EPOLL_CTL_ADD, inner, &o);
    uint64_t one = 1;
    write(wfd, &one, len);
    int a = wait1(outer, 100);
    write(wfd, &one, len);
    int b = wait1(outer, 100);
    int c = wait1(outer, 50);
    /* Add a new interest that is already ready while the outer one has not reported yet */
    int d = 0;
    printf("%-22s first %d, again %d, quiet %d\n", name, a, b, c);
    (void)d;
    close(inner);
    close(outer);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    int efd = eventfd(0, EFD_NONBLOCK);
    nested("eventfd, inner LT", efd, EPOLLIN, efd, 8);
    int efd2 = eventfd(0, EFD_NONBLOCK);
    nested("eventfd, inner ET", efd2, EPOLLIN | EPOLLET, efd2, 8);
    int p[2];
    pipe(p);
    nested("pipe, inner LT", p[0], EPOLLIN, p[1], 1);
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t al = sizeof a;
    bind(l, (void *)&a, sizeof a);
    listen(l, 4);
    getsockname(l, (void *)&a, &al);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    connect(c, (void *)&a, sizeof a);
    int s = accept(l, 0, 0);
    nested("tcp, inner LT", s, EPOLLIN, c, 1);
    int s2[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, s2);
    nested("unix, inner ET", s2[0], EPOLLIN | EPOLLET, s2[1], 1);
    /* Two levels of nesting: the outermost watches the middle one edge-triggered, the middle one
     * watches the inner one level-triggered */
    int e3 = eventfd(0, EFD_NONBLOCK);
    int in = epoll_create1(0), mid = epoll_create1(0), out = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN};
    epoll_ctl(in, EPOLL_CTL_ADD, e3, &ev);
    epoll_ctl(mid, EPOLL_CTL_ADD, in, &ev);
    ev.events = EPOLLIN | EPOLLET;
    epoll_ctl(out, EPOLL_CTL_ADD, mid, &ev);
    uint64_t one = 1;
    write(e3, &one, 8);
    int x = wait1(out, 100);
    write(e3, &one, 8);
    int y = wait1(out, 100);
    int z = wait1(out, 50);
    printf("%-22s first %d, again %d, quiet %d\n", "two levels", x, y, z);

    /* After the outer one has reported it waits in epoll_wait, while another process writes again
     * into a pipe and a TCP connection that are already readable */
    int fds[2][2];
    pipe(fds[0]);
    int c2 = socket(AF_INET, SOCK_STREAM, 0);
    connect(c2, (void *)&a, sizeof a);
    int s3 = accept(l, 0, 0);
    fds[1][0] = s3;
    fds[1][1] = c2;
    const char *names[] = {"pipe, blocking", "tcp, blocking"};
    for (int i = 0; i < 2; i++) {
        int inner = epoll_create1(0), outer = epoll_create1(0);
        struct epoll_event e = {.events = EPOLLIN};
        epoll_ctl(inner, EPOLL_CTL_ADD, fds[i][0], &e);
        e.events = EPOLLIN | EPOLLET;
        epoll_ctl(outer, EPOLL_CTL_ADD, inner, &e);
        write(fds[i][1], "x", 1);
        int first = wait1(outer, 100);
        if (fork() == 0) {
            usleep(100000);
            write(fds[i][1], "y", 1);
            _exit(0);
        }
        int woke = wait1(outer, 2000);
        printf("%-22s first %d, woken %d\n", names[i], first, woke);
        wait(NULL);
    }
    return 0;
}
C
gcc -O1 -o nested nested.c && ./nested

# While one thread waits in epoll_wait, another thread changes the open flags of the epoll fd
# (fcntl, FIONBIO): the waiting side is not borrowing that open file, so the change goes through,
# and it is woken as usual afterwards
cat > flags.c <<'C'
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <fcntl.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <unistd.h>

static int ep, ev;
static long set_flags, set_nbio;

static void *change(void *arg)
{
    usleep(100000);
    int on = 1;
    set_flags = fcntl(ep, F_SETFL, O_NONBLOCK);
    set_nbio = ioctl(ep, FIONBIO, &on);
    uint64_t one = 1;
    write(ev, &one, sizeof one);
    return arg;
}

int main(void)
{
    ep = epoll_create1(0);
    ev = eventfd(0, 0);
    struct epoll_event e = {.events = EPOLLIN, .data.u32 = 7};
    epoll_ctl(ep, EPOLL_CTL_ADD, ev, &e);
    pthread_t t;
    pthread_create(&t, NULL, change, NULL);
    struct epoll_event got;
    int n = epoll_wait(ep, &got, 1, 5000);
    pthread_join(t, NULL);
    printf("flags while waiting: fcntl %ld, FIONBIO %ld, epoll_wait %d (data %u), nonblocking %d\n",
           set_flags, set_nbio, n, got.data.u32, (fcntl(ep, F_GETFL) & O_NONBLOCK) != 0);
    return 0;
}
C
gcc -O1 -o flags flags.c -lpthread && ./flags

# Anonymous fds: their open modes (timerfd and signalfd are read-write like eventfd, epoll and
# pidfd, so a write is EINVAL; inotify is read-only) and the bits their poll operations report
# when ready, asked for every read and write bit
cat > anonfd.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <unistd.h>

static void show(const char *name, int fd)
{
    struct pollfd p = {.fd = fd, .events = POLLIN | POLLRDNORM | POLLOUT | POLLWRNORM};
    poll(&p, 1, 0);
    uint64_t v = 1;
    errno = 0;
    long w = write(fd, &v, 8);
    printf("%s: mode %d, revents %#x, write %ld %s\n", name, fcntl(fd, F_GETFL) & O_ACCMODE,
           p.revents, w, w < 0 ? strerror(errno) : "");
}

int main(void)
{
    int tfd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    struct itimerspec soon = {.it_value = {0, 1000}};
    timerfd_settime(tfd, 0, &soon, NULL);
    usleep(10000);
    sigset_t usr1;
    sigemptyset(&usr1);
    sigaddset(&usr1, SIGUSR1);
    sigprocmask(SIG_BLOCK, &usr1, NULL);
    raise(SIGUSR1);
    int sfd = signalfd(-1, &usr1, SFD_NONBLOCK);
    unlink("/tmp/anonfd-created");
    int ifd = inotify_init1(IN_NONBLOCK);
    inotify_add_watch(ifd, "/tmp", IN_CREATE);
    close(open("/tmp/anonfd-created", O_CREAT | O_WRONLY, 0644));
    int efd = eventfd(1, EFD_NONBLOCK);
    int ep = epoll_create1(0);
    struct epoll_event e = {.events = EPOLLIN};
    epoll_ctl(ep, EPOLL_CTL_ADD, efd, &e);
    struct mq_attr qa = {.mq_maxmsg = 4, .mq_msgsize = 8};
    mqd_t mq = mq_open("/anonfd-queue", O_RDWR | O_CREAT | O_NONBLOCK, 0600, &qa);
    mq_send(mq, "x", 1, 0);
    mq_unlink("/anonfd-queue");
    show("timerfd", tfd);
    show("signalfd", sfd);
    show("inotify", ifd);
    show("eventfd", efd);
    show("epoll", ep);
    show("message queue", mq);
    pid_t child = fork();
    if (child == 0)
        _exit(0);
    int pfd = syscall(SYS_pidfd_open, child, 0);
    usleep(100000);
    show("pidfd of an exited child", pfd);
    return 0;
}
C
gcc -O1 -o anonfd anonfd.c && ./anonfd

# A real tokio program: uv installs a package offline (not in the cache, so it fails). mio's
# second wakeup used to be lost, and uv hung forever
rm -rf /tmp/uvenv
uv venv -q /tmp/uvenv
uv pip install -q --offline --python /tmp/uvenv/bin/python pip || echo "uv exit $?"
