# Kernel corners: another process truncating a mapped file (SIGBUS, which must not take down the
# whole app), MADV_DONTNEED, large PROT_NONE reservations, nanosleep sleeping only the remaining
# time after a stop and continue, waiters getting EOWNERDEAD after a process holding a robust
# lock exits, EPOLLEXCLUSIVE argument checks, jumping to an unaligned address (SIGBUS), timeouts
# too long for Linux to represent saturating, select ignoring hangups and errors its sets do not
# take (waiting without spinning), reporting fds it cannot poll (O_PATH) in every set and
# clamping nfds to the fd table, timed waits that a signal interrupts after their time was
# unmapped ending with EINTR, and a timed wait refused after a signal woke it leaving nothing
# that shortens the thread's next timed wait
set -e
mkdir -p /tmp/diff-edges
cd /tmp/diff-edges
cat > edges.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static sigjmp_buf env;
static volatile int last_sig, last_code;
static void *volatile last_addr;

static void on_fault(int sig, siginfo_t *si, void *ctx)
{
    (void)ctx;
    last_sig = sig;
    last_code = si->si_code;
    last_addr = si->si_addr;
    siglongjmp(env, 1);
}

/* Read one byte and print the result; on a fault print the signal and si_code */
static void probe(const char *label, volatile char *p)
{
    if (sigsetjmp(env, 1)) {
        printf(" %s:%s/%d", label, last_sig == SIGBUS ? "SIGBUS" : "SIGSEGV", last_code);
        return;
    }
    int v = *p;
    printf(" %s:%d", label, v);
}

static void run_child(void (*fn)(void))
{
    pid_t pid = fork();
    if (pid == 0) {
        fn();
        _exit(0);
    }
    waitpid(pid, NULL, 0);
}

static void child_ftruncate(void)
{
    int fd = open("data", O_RDWR);
    ftruncate(fd, 4096 + 10);
}

static void child_truncate(void) { truncate("data", 0); }

static void child_otrunc(void) { close(open("data", O_WRONLY | O_TRUNC)); }

static long ms_since(struct timespec *t0)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (t.tv_sec - t0->tv_sec) * 1000 + (t.tv_nsec - t0->tv_nsec) / 1000000;
}

static void on_usr1(int sig) { (void)sig; }

/* Sends signal `sig` to this process in 100 ms */
static pid_t poke_soon(int sig)
{
    pid_t parent = getpid(), poker = fork();
    if (poker == 0) {
        usleep(100000);
        kill(parent, sig);
        _exit(0);
    }
    return poker;
}

int main(void)
{
    struct sigaction sa = {.sa_sigaction = on_fault, .sa_flags = SA_SIGINFO};
    sigaction(SIGBUS, &sa, NULL);
    sigaction(SIGSEGV, &sa, NULL);
    long page = 4096;

    /* Another process truncates a mapped file: pages beyond the new end get SIGBUS */
    void (*truncators[])(void) = {child_ftruncate, child_truncate, child_otrunc};
    const char *names[] = {"ftruncate", "truncate", "O_TRUNC"};
    for (int i = 0; i < 3; i++) {
        int fd = open("data", O_RDWR | O_CREAT | O_TRUNC, 0644);
        char buf[3 * 4096];
        memset(buf, 'x', sizeof buf);
        write(fd, buf, sizeof buf);
        char *shared = mmap(NULL, 3 * page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        char *private = mmap(NULL, 3 * page, PROT_READ, MAP_PRIVATE, fd, 0);
        printf("%s before:", names[i]);
        probe("shared2", shared + 2 * page);
        probe("private2", private + 2 * page);
        run_child(truncators[i]);
        printf("\n%s after:", names[i]);
        for (int p = 0; p < 3; p++) {
            char label[16];
            snprintf(label, sizeof label, "shared%d", p);
            probe(label, shared + p * page);
            snprintf(label, sizeof label, "private%d", p);
            probe(label, private + p * page);
        }
        printf("\n");
        munmap(shared, 3 * page);
        munmap(private, 3 * page);
        close(fd);
    }

    /* MADV_DONTNEED: private anonymous memory reads back as zeros, shared memory is unchanged */
    char *anon = mmap(NULL, 8 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *shm = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    memset(anon, 7, 8 * page);
    memset(shm, 9, 2 * page);
    madvise(anon + page, 5 * page, MADV_DONTNEED);
    madvise(shm, 2 * page, MADV_DONTNEED);
    printf("dontneed %d %d %d %d shared %d\n", anon[0], anon[page], anon[6 * page - 1], anon[6 * page],
           shm[page]);

    /* An 8 GB PROT_NONE reservation, with one page in the middle made writable */
    char *reserve = mmap(NULL, 1L << 33, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    char *mid = reserve + (1L << 32);
    mprotect(mid, page, PROT_READ | PROT_WRITE);
    mid[0] = 5;
    printf("reserve");
    probe("mid", mid);
    probe("next", mid + page);
    mprotect(mid, page, PROT_NONE);
    probe("protected", mid);
    mprotect(mid, page, PROT_READ);
    probe("readable", mid);
    munmap(reserve, 1L << 33);
    probe("unmapped", mid);
    printf("\n");

    /* Reading a procfs file again from offset 0 gives the current contents (seq_file): after a
     * rename, seek back to 0, and pread at 0 */
    {
        int st = open("/proc/self/status", O_RDONLY);
        char sb[64] = "";
        read(st, sb, sizeof sb - 1);
        prctl(PR_SET_NAME, "renamed");
        lseek(st, 0, SEEK_SET);
        memset(sb, 0, sizeof sb);
        read(st, sb, sizeof sb - 1);
        printf("status after seek: %.*s\n", (int)strcspn(sb, "\n"), sb);
        prctl(PR_SET_NAME, "again");
        memset(sb, 0, sizeof sb);
        pread(st, sb, sizeof sb - 1, 0);
        printf("status by pread: %.*s\n", (int)strcspn(sb, "\n"), sb);
        close(st);
    }

    /* Stop and continue: nanosleep sleeps on for the remaining time, not the full time again */
    int pipefd[2];
    pipe(pipefd);
    pid_t sleeper = fork();
    if (sleeper == 0) {
        struct timespec t0, req = {0, 600 * 1000000L};
        clock_gettime(CLOCK_MONOTONIC, &t0);
        nanosleep(&req, NULL);
        long ms = ms_since(&t0);
        write(pipefd[1], &ms, sizeof ms);
        _exit(0);
    }
    usleep(150000);
    kill(sleeper, SIGSTOP);
    usleep(150000);
    kill(sleeper, SIGCONT);
    long slept;
    read(pipefd[0], &slept, sizeof slept);
    waitpid(sleeper, NULL, 0);
    printf("stopped sleep %s\n", slept >= 600 && slept < 850 ? "kept its deadline" : "restarted");

    /* Stop and continue: poll (the remaining time is written back to the timespec) and futex
       (restart_block) go on waiting until the original deadline, epoll_wait is EINTR (signal(7);
       the same after a cgroup is frozen and thawed) */
    for (int which = 0; which < 3; which++) {
        pid_t waiter = fork();
        if (waiter == 0) {
            int q[2];
            pipe(q);
            struct timespec t0;
            clock_gettime(CLOCK_MONOTONIC, &t0);
            long r;
            if (which == 0) {
                struct pollfd p = {q[0], POLLIN, 0};
                r = poll(&p, 1, 600);
            } else if (which == 2) {
                static int word;
                struct timespec rel = {0, 600 * 1000000L};
                r = syscall(SYS_futex, &word, 128 /* FUTEX_WAIT | FUTEX_PRIVATE_FLAG */, 0, &rel, NULL, 0);
            } else {
                int ep = epoll_create1(0);
                struct epoll_event ev = {.events = EPOLLIN};
                epoll_ctl(ep, EPOLL_CTL_ADD, q[0], &ev);
                r = epoll_wait(ep, &ev, 1, 600);
            }
            long out[3] = {r, r < 0 ? errno : 0, ms_since(&t0)};
            write(pipefd[1], out, sizeof out);
            _exit(0);
        }
        usleep(150000);
        kill(waiter, SIGSTOP);
        usleep(150000);
        kill(waiter, SIGCONT);
        long out[3];
        read(pipefd[0], out, sizeof out);
        waitpid(waiter, NULL, 0);
        printf("stopped %s: %ld %s %s\n", which == 2 ? "futex" : which ? "epoll_wait" : "poll", out[0],
               out[1] ? strerror(out[1]) : "", out[2] >= 600 && out[2] < 850 ? "kept its deadline"
               : out[2] < 600 ? "returned early" : "restarted");
    }

    /* With a handler it returns EINTR, and rem is the remaining time */
    struct sigaction su = {.sa_handler = on_usr1};
    sigaction(SIGUSR1, &su, NULL);
    pid_t parent = getpid();
    pid_t poker = fork();
    if (poker == 0) {
        usleep(100000);
        kill(parent, SIGUSR1);
        _exit(0);
    }
    struct timespec req = {2, 0}, rem = {0, 0};
    int r = nanosleep(&req, &rem);
    printf("interrupted sleep %d %s rem<2s %d\n", r, strerror(errno), rem.tv_sec < 2 && (rem.tv_sec || rem.tv_nsec));
    waitpid(poker, NULL, 0);

    /* A futex wait with a timeout that a handler interrupts is EINTR, even with SA_RESTART
     * (restart_block) */
    su.sa_flags = SA_RESTART;
    sigaction(SIGUSR1, &su, NULL);
    poker = fork();
    if (poker == 0) {
        usleep(100000);
        kill(parent, SIGUSR1);
        _exit(0);
    }
    static int fword;
    struct timespec ft = {2, 0};
    r = syscall(SYS_futex, &fword, 128, 0, &ft, NULL, 0);
    printf("interrupted futex with SA_RESTART %d %s\n", r, strerror(errno));
    waitpid(poker, NULL, 0);

    /* A process exits while holding a process-shared robust lock: the waiter gets EOWNERDEAD */
    pthread_mutex_t *m = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    pthread_mutexattr_t ma;
    pthread_mutexattr_init(&ma);
    pthread_mutexattr_setpshared(&ma, PTHREAD_PROCESS_SHARED);
    pthread_mutexattr_setrobust(&ma, PTHREAD_MUTEX_ROBUST);
    pthread_mutex_init(m, &ma);
    pid_t owner = fork();
    if (owner == 0) {
        pthread_mutex_lock(m);
        _exit(0);
    }
    waitpid(owner, NULL, 0);
    alarm(10);
    r = pthread_mutex_lock(m);
    printf("robust lock %s\n", r == EOWNERDEAD ? "EOWNERDEAD" : strerror(r));
    if (r == EOWNERDEAD)
        pthread_mutex_consistent(m);
    printf("unlock %d\n", pthread_mutex_unlock(m));
    alarm(0);

    /* EPOLLEXCLUSIVE can be used only when adding, not on an epoll, and not with other flags */
    int ep = epoll_create1(0), ep2 = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN | EPOLLEXCLUSIVE};
    printf("exclusive add %d\n", epoll_ctl(ep, EPOLL_CTL_ADD, pipefd[0], &ev));
    printf("exclusive mod %d %d\n", epoll_ctl(ep, EPOLL_CTL_MOD, pipefd[0], &ev), errno);
    ev.events = EPOLLIN;
    printf("mod exclusive interest %d %d\n", epoll_ctl(ep, EPOLL_CTL_MOD, pipefd[0], &ev), errno);
    ev.events = EPOLLIN | EPOLLEXCLUSIVE | EPOLLONESHOT;
    printf("exclusive oneshot %d %d\n", epoll_ctl(ep, EPOLL_CTL_ADD, pipefd[1], &ev), errno);
    ev.events = EPOLLIN | EPOLLEXCLUSIVE;
    printf("exclusive epoll %d %d\n", epoll_ctl(ep, EPOLL_CTL_ADD, ep2, &ev), errno);

    /* Timeouts too long for Linux to represent saturate at about 292 years (GNU `sleep infinity`
     * sleeps for LONG_MAX seconds): the waits go on until a handler interrupts them, the timers
     * report the saturated time left, and such a socket timeout reads back as none */
    const struct timespec huge = {LONG_MAX, 999999999};
    struct timespec t = huge, left = {0, 0};
    su.sa_flags = 0;
    sigaction(SIGUSR1, &su, NULL);
    poker = poke_soon(SIGUSR1);
    r = syscall(SYS_nanosleep, &t, &left);
    printf("huge nanosleep %d %s left>9e9s %d\n", r, strerror(errno), left.tv_sec > 9000000000L);
    waitpid(poker, NULL, 0);
    poker = poke_soon(SIGUSR1);
    r = clock_nanosleep(CLOCK_REALTIME, 0, &t, &left);
    printf("huge clock_nanosleep %s left>9e9s %d\n", strerror(r), left.tv_sec > 9000000000L);
    waitpid(poker, NULL, 0);
    poker = poke_soon(SIGUSR1);
    r = clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &t, NULL);
    printf("huge clock_nanosleep abs %s\n", strerror(r));
    waitpid(poker, NULL, 0);
    static int hword;
    poker = poke_soon(SIGUSR1);
    r = syscall(SYS_futex, &hword, 128, 0, &t, NULL, 0);
    printf("huge futex %d %s\n", r, strerror(errno));
    waitpid(poker, NULL, 0);
    int hq[2];
    pipe(hq);
    struct pollfd hp = {hq[0], POLLIN, 0};
    poker = poke_soon(SIGUSR1);
    r = ppoll(&hp, 1, &t, NULL);
    printf("huge ppoll %d %s\n", r, strerror(errno));
    waitpid(poker, NULL, 0);
    fd_set rd;
    FD_ZERO(&rd);
    FD_SET(hq[0], &rd);
    t = huge;
    poker = poke_soon(SIGUSR1);
    r = pselect(hq[0] + 1, &rd, NULL, NULL, &t, NULL);
    printf("huge pselect %d %s\n", r, strerror(errno));
    waitpid(poker, NULL, 0);
    t = huge;
    poker = poke_soon(SIGUSR1);
    r = syscall(441 /* epoll_pwait2 */, ep2, &ev, 1, &t, NULL, 0);
    printf("huge epoll_pwait2 %d %s\n", r, strerror(errno));
    waitpid(poker, NULL, 0);
    sigset_t usr2;
    sigemptyset(&usr2);
    sigaddset(&usr2, SIGUSR2);
    sigprocmask(SIG_BLOCK, &usr2, NULL);
    t = huge;
    poker = poke_soon(SIGUSR2);
    r = sigtimedwait(&usr2, NULL, &t);
    printf("huge sigtimedwait %s\n", r == SIGUSR2 ? "SIGUSR2" : strerror(errno));
    waitpid(poker, NULL, 0);
    sigprocmask(SIG_UNBLOCK, &usr2, NULL);

    struct itimerval iv = {{0, 0}, {LONG_MAX, 999999}}, oiv;
    printf("huge setitimer %d", setitimer(ITIMER_REAL, &iv, NULL));
    getitimer(ITIMER_REAL, &oiv);
    printf(" left>9e9s %d\n", oiv.it_value.tv_sec > 9000000000L);
    iv.it_value.tv_sec = iv.it_value.tv_usec = 0;
    setitimer(ITIMER_REAL, &iv, NULL);
    struct itimerspec its = {huge, huge}, oits;
    timer_t tid;
    struct sigevent sev = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGUSR1};
    timer_create(CLOCK_MONOTONIC, &sev, &tid);
    printf("huge timer_settime %d", timer_settime(tid, 0, &its, NULL));
    timer_gettime(tid, &oits);
    printf(" left>9e9s %d interval>9e9s %d\n", oits.it_value.tv_sec > 9000000000L,
           oits.it_interval.tv_sec > 9000000000L);
    timer_delete(tid);
    int tfd = timerfd_create(CLOCK_MONOTONIC, 0);
    printf("huge timerfd_settime %d", timerfd_settime(tfd, 0, &its, NULL));
    timerfd_gettime(tfd, &oits);
    printf(" left>9e9s %d interval>9e9s %d\n", oits.it_value.tv_sec > 9000000000L,
           oits.it_interval.tv_sec > 9000000000L);

    int us[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, us);
    int socks[] = {us[0], socket(AF_INET, SOCK_DGRAM, 0)};
    for (int i = 0; i < 2; i++) {
        struct timeval tv = {LONG_MAX, 0}, got = {1, 1};
        socklen_t len = sizeof got;
        r = setsockopt(socks[i], SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
        getsockopt(socks[i], SOL_SOCKET, SO_RCVTIMEO, &got, &len);
        printf("huge SO_RCVTIMEO %s %d reads %ld.%06ld", i ? "inet" : "unix", r, (long)got.tv_sec,
               (long)got.tv_usec);
        char c;
        poker = poke_soon(SIGUSR1);
        r = recv(socks[i], &c, 1, 0);
        printf(" recv %d %s\n", r, strerror(errno));
        waitpid(poker, NULL, 0);
    }

    /* Jumping to an unaligned address: SIGBUS (BUS_ADRALN) on instruction fetch, si_addr is the
     * target. Through a call via a function pointer and through ret */
    uintptr_t targets[] = {1, 2, 0x1001};
    for (int i = 0; i < 3; i++) {
        for (int how = 0; how < 2; how++) {
            if (sigsetjmp(env, 1)) {
                printf("unaligned %s %#lx: %s/%d addr=%p\n", how ? "ret" : "call", (unsigned long)targets[i],
                       last_sig == SIGBUS ? "SIGBUS" : "SIGSEGV", last_code, last_addr);
                continue;
            }
            if (how)
                __asm__ volatile("mov x30, %0\n ret" ::"r"(targets[i]) : "x30");
            else
                ((void (*)(void))targets[i])();
        }
    }
    return 0;
}
C
gcc -O2 -o edges edges.c
./edges

cat > sel.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static double cpu(void)
{
    struct rusage u;
    getrusage(RUSAGE_SELF, &u);
    return u.ru_utime.tv_sec + u.ru_stime.tv_sec + (u.ru_utime.tv_usec + u.ru_stime.tv_usec) / 1e6;
}

/* select on `fd` in one set (0 read, 1 write, 2 except) with a 500 ms timeout: the result,
 * whether it waited the whole timeout, and whether it waited without spinning */
static void one(const char *what, int fd, int which)
{
    static const char *names[] = {"read", "write", "except"};
    fd_set s[3];
    for (int i = 0; i < 3; i++)
        FD_ZERO(&s[i]);
    FD_SET(fd, &s[which]);
    struct timeval tv = {0, 500000};
    double t0 = now(), c0 = cpu();
    int r = select(fd + 1, &s[0], &s[1], &s[2], &tv);
    int e = errno;
    double waited = now() - t0, used = cpu() - c0;
    printf("%s in the %s set: select %d %s, waited %d", what, names[which], r,
           r < 0 ? strerror(e) : "", waited > 0.4);
    if (waited > 0.4)
        printf(", idle %d", used < 0.2);
    printf("\n");
}

int main(void)
{
    /* A pipe whose writer is gone (POLLHUP) and one whose reader is gone (POLLERR) */
    int hup[2], err[2];
    pipe(hup);
    close(hup[1]);
    pipe(err);
    close(err[0]);
    one("hung-up pipe", hup[0], 0);
    one("hung-up pipe", hup[0], 2);
    one("readerless pipe", err[1], 1);
    one("readerless pipe", err[1], 2);
    /* O_PATH: select cannot poll it (POLLNVAL), which counts as ready in every set */
    int path = open("/", O_PATH);
    one("O_PATH fd", path, 0);
    one("O_PATH fd", path, 2);
    /* A TCP socket shut down both ways (POLLHUP from the host) */
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t len = sizeof a;
    bind(l, (struct sockaddr *)&a, sizeof a);
    listen(l, 1);
    getsockname(l, (struct sockaddr *)&a, &len);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    connect(c, (struct sockaddr *)&a, sizeof a);
    shutdown(c, SHUT_RDWR);
    one("shut-down TCP socket", c, 0);
    one("shut-down TCP socket", c, 2);

    /* nfds above the soft fd limit is clamped to the fd table, not EINVAL; set bits above the
     * table are neither checked nor written back, a closed fd inside it is EBADF */
    struct rlimit rl;
    getrlimit(RLIMIT_NOFILE, &rl);
    rl.rlim_cur = 256;
    setrlimit(RLIMIT_NOFILE, &rl);
    int ready[2];
    pipe(ready);
    write(ready[1], "x", 1);
    int sizes[] = {FD_SETSIZE, INT_MAX};
    for (int i = 0; i < 2; i++) {
        fd_set rd;
        FD_ZERO(&rd);
        FD_SET(ready[0], &rd);
        FD_SET(1000, &rd);
        struct timeval tv = {0, 0};
        int r = select(sizes[i], &rd, NULL, NULL, &tv);
        printf("select(%d) with the limit at 256: %d %s, ready %d, fd 1000 bit %d\n", sizes[i], r,
               r < 0 ? strerror(errno) : "", FD_ISSET(ready[0], &rd), FD_ISSET(1000, &rd));
    }
    fd_set rd;
    FD_ZERO(&rd);
    FD_SET(50, &rd);
    struct timeval tv = {0, 0};
    int r = select(FD_SETSIZE, &rd, NULL, NULL, &tv);
    printf("select with closed fd 50: %d %s\n", r, r < 0 ? strerror(errno) : "");
    r = select(-1, NULL, NULL, NULL, &tv);
    printf("select(-1): %d %s\n", r, r < 0 ? strerror(errno) : "");
    return 0;
}
C
gcc -O2 -o sel sel.c
./sel

# Timed waits (futex relative and absolute, nanosleep, clock_nanosleep relative and absolute),
# each interrupted by a signal after another thread unmapped its time argument, end with EINTR:
# Linux's restart keeps the deadline and never reads the time again. Tuxport's no-BKL waits hand
# their deadline to the BKL path, which read the time again (EFAULT). Then a thread whose timed
# futex wait a seccomp filter (installed with TSYNC while it waited) refuses once a signal wakes
# it must still wait the whole timeout of its next timed wait, on a shared word (always the BKL
# path): the handed-over deadline used to outlive the refused call and end that wait early
cat > stale.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define FUTEX_WAIT 0
#define FUTEX_WAIT_BITSET 9
#define FUTEX_PRIVATE 128

struct sock_filter {
    unsigned short code;
    unsigned char jt, jf;
    unsigned int k;
};
struct sock_fprog {
    unsigned short len;
    struct sock_filter *filter;
};

static const char *names[] = {"futex wait", "futex wait bitset (absolute)", "nanosleep",
                              "clock_nanosleep", "clock_nanosleep (absolute)"};
static int kind;
static struct timespec *ts;
static int *shared;

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static void on_usr1(int sig)
{
    (void)sig;
}

static void *waiter(void *arg)
{
    (void)arg;
    int word = 0;
    long r = 0;
    switch (kind) {
    case 0:
        r = syscall(SYS_futex, &word, FUTEX_WAIT | FUTEX_PRIVATE, 0, ts, NULL, 0);
        break;
    case 1:
        r = syscall(SYS_futex, &word, FUTEX_WAIT_BITSET | FUTEX_PRIVATE, 0, ts, NULL, ~0u);
        break;
    case 2:
        r = syscall(SYS_nanosleep, ts, NULL);
        break;
    case 3:
        r = syscall(SYS_clock_nanosleep, CLOCK_MONOTONIC, 0, ts, NULL);
        break;
    case 4:
        r = syscall(SYS_clock_nanosleep, CLOCK_MONOTONIC, TIMER_ABSTIME, ts, NULL);
        break;
    }
    printf("%s with its time unmapped: %ld %s\n", names[kind], r, r < 0 ? strerror(errno) : "");
    return NULL;
}

// Waits on 12345, which the filter refuses once the signal sends it back through the kernel,
// then waits 3 s on a shared word
static void *refused(void *arg)
{
    (void)arg;
    int word = 12345;
    struct timespec two = {2, 0};
    syscall(SYS_futex, &word, FUTEX_WAIT | FUTEX_PRIVATE, 12345, &two, NULL, 0);
    struct timespec three = {3, 0};
    double t = now();
    long r = syscall(SYS_futex, shared, FUTEX_WAIT, 0, &three, NULL, 0);
    printf("next 3 s wait after the refused one: %ld %s after %s\n", r,
           r < 0 ? strerror(errno) : "",
           now() - t >= 2.5 ? "its whole timeout" : "less than its timeout");
    return NULL;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, NULL);
    for (kind = 0; kind < 5; kind++) {
        ts = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (kind == 1 || kind == 4)
            clock_gettime(CLOCK_MONOTONIC, ts);
        ts->tv_sec += 2;
        pthread_t t;
        pthread_create(&t, NULL, waiter, NULL);
        usleep(300000);
        munmap(ts, 4096);
        pthread_kill(t, SIGUSR1);
        pthread_join(t, NULL);
    }

    shared = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    pid_t child = fork();
    if (child == 0) {
        pthread_t t;
        pthread_create(&t, NULL, refused, NULL);
        usleep(300000);
        // futex with 12345 as its value: EPERM; anything else is allowed
        struct sock_filter code[] = {
            {0x20, 0, 0, 0},                 // ld nr
            {0x15, 0, 3, SYS_futex},         // jeq futex
            {0x20, 0, 0, 32},                // ld args[2]
            {0x15, 0, 1, 12345},             // jeq 12345
            {0x06, 0, 0, 0x00050000u | EPERM}, // ret errno
            {0x06, 0, 0, 0x7fff0000u},       // ret allow
        };
        struct sock_fprog prog = {6, code};
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        if (syscall(SYS_seccomp, 1, 1, &prog) != 0)
            printf("seccomp: %s\n", strerror(errno));
        pthread_kill(t, SIGUSR1);
        pthread_join(t, NULL);
        _exit(0);
    }
    waitpid(child, NULL, 0);
    return 0;
}
C
gcc -O2 -o stale stale.c
./stale
