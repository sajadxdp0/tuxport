/*
 * Guest test program for System V IPC, POSIX timers, clocks and CPU times, signalfd, splice,
 * mremap, credentials, scheduling settings and ptrace (M6): build with musl-gcc -static and run
 * directly in tuxport-run (no rootfs needed). Each check that passes prints a line "ok <name>"; a
 * failure prints the reason and exits with 1; when all pass it prints "all ok" at the end.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdatomic.h>
#include <grp.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/fsuid.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/msg.h>
#include <sys/ptrace.h>
#include <sys/resource.h>
#include <sys/sem.h>
#include <poll.h>
#include <pthread.h>
#include <sys/shm.h>
#include <fcntl.h>
#include <sys/signalfd.h>
#include <sys/uio.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/times.h>
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

static void ok(const char *name)
{
    printf("ok %s\n", name);
    fflush(stdout);
}

/* The child's exit code */
static int wait_child(pid_t pid)
{
    int status;
    CHECK(waitpid(pid, &status, 0) == pid, "waitpid");
    CHECK(WIFEXITED(status), "child status %#x", status);
    return WEXITSTATUS(status);
}

/* ---- Message queues: receiving by type, blocking msgrcv, EIDRM after removal ---- */

struct message {
    long mtype;
    char text[16];
};

static void test_msg(void)
{
    int q = msgget(IPC_PRIVATE, IPC_CREAT | 0600);
    CHECK(q >= 0, "msgget");
    struct message m = {2, "two"};
    CHECK(msgsnd(q, &m, 4, 0) == 0, "msgsnd 2");
    m.mtype = 1;
    strcpy(m.text, "one");
    CHECK(msgsnd(q, &m, 4, 0) == 0, "msgsnd 1");
    CHECK(msgrcv(q, &m, sizeof m.text, 1, 0) == 4 && strcmp(m.text, "one") == 0, "type 1");
    CHECK(msgrcv(q, &m, 2, 0, IPC_NOWAIT) < 0 && errno == E2BIG, "E2BIG");
    CHECK(msgrcv(q, &m, sizeof m.text, 0, 0) == 4 && m.mtype == 2, "first message");
    CHECK(msgrcv(q, &m, sizeof m.text, 0, IPC_NOWAIT) < 0 && errno == ENOMSG, "ENOMSG");

    /* The child waits to receive, then the parent sends */
    pid_t pid = fork();
    if (pid == 0) {
        struct message r;
        ssize_t n = msgrcv(q, &r, sizeof r.text, 7, 0);
        _exit(n == 6 && strcmp(r.text, "hello") == 0 ? 0 : 1);
    }
    usleep(50000);
    m.mtype = 7;
    strcpy(m.text, "hello");
    CHECK(msgsnd(q, &m, 6, 0) == 0, "msgsnd to waiter");
    CHECK(wait_child(pid) == 0, "child msgrcv");

    struct msqid_ds ds;
    CHECK(msgctl(q, IPC_STAT, &ds) == 0 && ds.msg_qnum == 0 && ds.msg_lrpid == pid, "IPC_STAT");
    CHECK(msgctl(q, IPC_RMID, NULL) == 0, "IPC_RMID");
    CHECK(msgsnd(q, &m, 1, 0) < 0 && errno == EINVAL, "removed queue");
    ok("msg");
}

/* ---- Semaphores: several operations done atomically, blocking, SEM_UNDO ---- */

static void test_sem(void)
{
    int s = semget(IPC_PRIVATE, 2, IPC_CREAT | 0600);
    CHECK(s >= 0, "semget");
    unsigned short vals[2] = {1, 0};
    CHECK(semctl(s, 0, SETALL, vals) == 0, "SETALL");
    /* The second operation cannot be done: the whole semop takes no effect */
    struct sembuf both[2] = {{0, -1, IPC_NOWAIT}, {1, -1, IPC_NOWAIT}};
    CHECK(semop(s, both, 2) < 0 && errno == EAGAIN, "atomic semop");
    CHECK(semctl(s, 0, GETVAL) == 1, "value unchanged");

    /* The child waits on semaphore 1, and the parent adds one */
    pid_t pid = fork();
    if (pid == 0) {
        struct sembuf down = {1, -1, 0};
        _exit(semop(s, &down, 1) == 0 ? 0 : 1);
    }
    while (semctl(s, 1, GETNCNT) != 1) {
        usleep(1000);
    }
    struct sembuf up = {1, 1, 0};
    CHECK(semop(s, &up, 1) == 0, "semop up");
    CHECK(wait_child(pid) == 0, "child semop");

    /* SEM_UNDO: the child's operations are undone when it exits */
    pid = fork();
    if (pid == 0) {
        struct sembuf take = {0, -1, SEM_UNDO};
        _exit(semop(s, &take, 1) == 0 && semctl(s, 0, GETVAL) == 0 ? 0 : 1);
    }
    CHECK(wait_child(pid) == 0, "child SEM_UNDO");
    CHECK(semctl(s, 0, GETVAL) == 1, "undone at exit");
    CHECK(semctl(s, 0, GETPID) == pid, "GETPID");
    CHECK(semctl(s, 0, IPC_RMID) == 0, "IPC_RMID");
    ok("sem");
}

/* ---- Shared memory: parent and child see the same memory, and IPC_RMID waits for the last
 * shmdt ---- */

static void test_shm(void)
{
    int id = shmget(IPC_PRIVATE, 10000, IPC_CREAT | 0600);
    CHECK(id >= 0, "shmget");
    char *p = shmat(id, NULL, 0);
    CHECK(p != (void *)-1, "shmat");
    CHECK(p[9999] == 0, "zeroed");
    pid_t pid = fork();
    if (pid == 0) {
        strcpy(p, "from child");
        _exit(0);
    }
    CHECK(wait_child(pid) == 0, "child");
    CHECK(strcmp(p, "from child") == 0, "shared contents: %s", p);

    struct shmid_ds ds;
    CHECK(shmctl(id, IPC_STAT, &ds) == 0, "IPC_STAT");
    CHECK(ds.shm_segsz == 10000 && ds.shm_nattch == 1, "segsz %zu nattch %lu", ds.shm_segsz,
          (unsigned long)ds.shm_nattch);
    /* A second attach at another address, read-only */
    char *q = shmat(id, NULL, SHM_RDONLY);
    CHECK(q != (void *)-1 && q != p && strcmp(q, "from child") == 0, "second shmat");
    CHECK(shmctl(id, IPC_STAT, &ds) == 0 && ds.shm_nattch == 2, "nattch 2");
    CHECK(shmdt(q) == 0, "shmdt");
    CHECK(shmdt(q) < 0 && errno == EINVAL, "shmdt twice");

    /* Still usable after removal, until the last shmdt */
    CHECK(shmctl(id, IPC_RMID, NULL) == 0, "IPC_RMID");
    strcpy(p, "still here");
    CHECK(shmctl(id, IPC_STAT, &ds) == 0 && (ds.shm_perm.mode & SHM_DEST), "SHM_DEST");
    CHECK(shmdt(p) == 0, "last shmdt");
    CHECK(shmctl(id, IPC_STAT, &ds) < 0 && errno == EINVAL, "freed");
    ok("shm");
}

/* ---- POSIX timers: periodic signals, overrun counts, SIGEV_THREAD, CPU clocks ---- */

static volatile sig_atomic_t ticks;
/* si_overrun at each delivery, in order of delivery */
static volatile int overruns[64];

static void on_tick(int sig, siginfo_t *si, void *uc)
{
    (void)sig;
    (void)uc;
    if (ticks < 64)
        overruns[ticks] = si->si_overrun;
    ticks++;
}

static atomic_int thread_ticks;

static void on_thread_tick(union sigval v)
{
    atomic_fetch_add(&thread_ticks, v.sival_int);
}

static void test_timers(void)
{
    struct sigaction sa = {0};
    sa.sa_sigaction = on_tick;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    CHECK(sigaction(SIGUSR1, &sa, NULL) == 0, "sigaction");

    struct sigevent sev = {0};
    sev.sigev_notify = SIGEV_SIGNAL;
    sev.sigev_signo = SIGUSR1;
    timer_t t;
    CHECK(timer_create(CLOCK_MONOTONIC, &sev, &t) == 0, "timer_create");
    struct itimerspec its = {{0, 10000000}, {0, 10000000}};
    CHECK(timer_settime(t, 0, &its, NULL) == 0, "timer_settime");
    while (ticks < 3) {
        pause();
    }
    struct itimerspec cur;
    CHECK(timer_gettime(t, &cur) == 0 && cur.it_interval.tv_nsec == 10000000, "timer_gettime");

    /* Expiries while the signal is pending are counted in si_overrun. The first one delivered
     * after unblocking is the pending one; the timer may have expired once more before the check
     * (the period is only 10 milliseconds), so only the first is looked at */
    sigset_t block;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR1);
    sigprocmask(SIG_BLOCK, &block, NULL);
    int before = ticks;
    usleep(100000);
    sigprocmask(SIG_UNBLOCK, &block, NULL);
    CHECK(ticks > before && overruns[before] > 0, "overrun %d", overruns[before]);
    CHECK(timer_delete(t) == 0, "timer_delete");
    /* musl's timer_delete returns -EINVAL directly, without setting errno */
    CHECK(timer_delete(t) != 0, "deleted");

    /* SIGEV_THREAD: musl starts a thread in user space to call the callback */
    sev.sigev_notify = SIGEV_THREAD;
    sev.sigev_notify_function = on_thread_tick;
    sev.sigev_value.sival_int = 5;
    CHECK(timer_create(CLOCK_REALTIME, &sev, &t) == 0, "SIGEV_THREAD");
    struct itimerspec once = {{0, 0}, {0, 20000000}};
    CHECK(timer_settime(t, 0, &once, NULL) == 0, "arm once");
    for (int i = 0; i < 200 && atomic_load(&thread_ticks) == 0; i++) {
        usleep(10000);
    }
    CHECK(atomic_load(&thread_ticks) == 5, "callback %d", atomic_load(&thread_ticks));
    timer_delete(t);

    /* Process CPU clock: expires after 30 milliseconds of CPU time spent busy-waiting */
    sev.sigev_notify = SIGEV_SIGNAL;
    CHECK(timer_create(CLOCK_PROCESS_CPUTIME_ID, &sev, &t) == 0, "cpu timer");
    struct itimerspec cpu = {{0, 0}, {0, 30000000}};
    before = ticks;
    CHECK(timer_settime(t, 0, &cpu, NULL) == 0, "arm cpu timer");
    while (ticks == before) {
    }
    timer_delete(t);
    ok("timers");
}

/* ---- Clocks: the host's clocks and one's own CPU clock without the BKL, the others holding the
 * lock, with consistent results; invalid arguments ---- */

static long long ns_of(const struct timespec *t)
{
    return t->tv_sec * 1000000000LL + t->tv_nsec;
}

static void *spin(void *arg)
{
    while (!atomic_load((atomic_int *)arg))
        ;
    return NULL;
}

/* From a new thread, watches the CPU clock of another, spinning thread: that one grows a lot,
 * while its own starts at 0 and barely moves while sleeping */
static void *watch(void *arg)
{
    atomic_int stop = 0;
    pthread_t t;
    clockid_t other;
    struct timespec start, now, a, own;
    (void)arg;
    CHECK(pthread_create(&t, NULL, spin, &stop) == 0, "pthread_create");
    CHECK(pthread_getcpuclockid(t, &other) == 0, "getcpuclockid other");
    CHECK(clock_gettime(CLOCK_MONOTONIC, &start) == 0, "monotonic");
    do {
        struct timespec nap = {0, 10000000};
        nanosleep(&nap, NULL);
        CHECK(clock_gettime(other, &a) == 0, "other thread's clock");
        CHECK(clock_gettime(CLOCK_MONOTONIC, &now) == 0 &&
                  ns_of(&now) - ns_of(&start) < 10000000000LL,
              "other thread's clock stays at %lld", ns_of(&a));
    } while (ns_of(&a) < 50000000);
    CHECK(clock_gettime(CLOCK_THREAD_CPUTIME_ID, &own) == 0 && ns_of(&own) < ns_of(&a) / 2,
          "own clock %lld while the other thread used %lld", ns_of(&own), ns_of(&a));
    atomic_store(&stop, 1);
    CHECK(pthread_join(t, NULL) == 0, "pthread_join");
    return NULL;
}

static void test_clocks(void)
{
    struct timespec a, b, p;
    struct timeval tv;
    CHECK(clock_gettime(CLOCK_MONOTONIC, &a) == 0, "monotonic");
    CHECK(clock_gettime(CLOCK_MONOTONIC, &b) == 0 && ns_of(&b) >= ns_of(&a), "monotonic goes back");
    CHECK(clock_gettime(CLOCK_REALTIME, &a) == 0 && gettimeofday(&tv, NULL) == 0, "realtime");
    CHECK(tv.tv_sec >= a.tv_sec && tv.tv_sec - a.tv_sec <= 1, "gettimeofday %ld vs %ld",
          (long)tv.tv_sec, (long)a.tv_sec);
    CHECK(clock_getres(CLOCK_MONOTONIC, &p) == 0 && p.tv_sec == 0 && p.tv_nsec > 0, "getres");
    CHECK(syscall(SYS_clock_gettime, 12345, &a) == -1 && errno == EINVAL, "bad clock");
    CHECK(syscall(SYS_clock_gettime, CLOCK_MONOTONIC, (void *)8) == -1 && errno == EFAULT,
          "bad timespec");
    CHECK(syscall(SYS_gettimeofday, (void *)8, NULL) == -1 && errno == EFAULT, "bad timeval");

    /* Own CPU clock: both ways of naming it read the same clock, which grows after being busy for
     * a while; the process's is no smaller */
    clockid_t self;
    CHECK(pthread_getcpuclockid(pthread_self(), &self) == 0, "getcpuclockid");
    CHECK(clock_gettime(CLOCK_THREAD_CPUTIME_ID, &a) == 0, "thread clock");
    for (volatile long i = 0; i < 20000000; i++)
        ;
    CHECK(clock_gettime(self, &b) == 0 && ns_of(&b) > ns_of(&a), "own clock %lld -> %lld",
          ns_of(&a), ns_of(&b));
    CHECK(clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &p) == 0 && ns_of(&p) >= ns_of(&b),
          "process clock %lld < thread clock %lld", ns_of(&p), ns_of(&b));

    /* Another thread's CPU clock */
    pthread_t t;
    CHECK(pthread_create(&t, NULL, watch, NULL) == 0, "pthread_create");
    CHECK(pthread_join(t, NULL) == 0, "pthread_join");
    ok("clocks");
}

static long long us_of(struct timeval t)
{
    return t.tv_sec * 1000000LL + t.tv_usec;
}

/* User and system CPU time each never go backwards: user time is the total minus the system time,
 * and when the system time suddenly gained a clock tick, user time used to lose just as much
 * (LTP's times03). Keep busy while making the host kernel work (page faults, mmap), until the
 * system time has crossed a few ticks */
static void test_cpu_times(void)
{
    struct rusage prev = {0}, cur, tprev = {0}, tcur;
    struct tms mprev = {0}, mcur;
    struct timespec start, now;
    long long last_tick = -1;
    int ticks = 0;
    CHECK(clock_gettime(CLOCK_MONOTONIC, &start) == 0, "monotonic");
    do {
        for (volatile long i = 0; i < 100000; i++)
            ;
        char *p = mmap(NULL, 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        CHECK(p != MAP_FAILED, "mmap");
        memset(p, 1, 1 << 20);
        CHECK(munmap(p, 1 << 20) == 0, "munmap");
        CHECK(getrusage(RUSAGE_SELF, &cur) == 0 && getrusage(RUSAGE_THREAD, &tcur) == 0,
              "getrusage");
        CHECK(times(&mcur) != (clock_t)-1, "times");
        CHECK(us_of(cur.ru_utime) >= us_of(prev.ru_utime) &&
                  us_of(cur.ru_stime) >= us_of(prev.ru_stime),
              "process: user %lld -> %lld, system %lld -> %lld", us_of(prev.ru_utime),
              us_of(cur.ru_utime), us_of(prev.ru_stime), us_of(cur.ru_stime));
        CHECK(us_of(tcur.ru_utime) >= us_of(tprev.ru_utime) &&
                  us_of(tcur.ru_stime) >= us_of(tprev.ru_stime),
              "thread: user %lld -> %lld, system %lld -> %lld", us_of(tprev.ru_utime),
              us_of(tcur.ru_utime), us_of(tprev.ru_stime), us_of(tcur.ru_stime));
        CHECK(mcur.tms_utime >= mprev.tms_utime && mcur.tms_stime >= mprev.tms_stime,
              "times: user %ld -> %ld, system %ld -> %ld", (long)mprev.tms_utime,
              (long)mcur.tms_utime, (long)mprev.tms_stime, (long)mcur.tms_stime);
        if (us_of(cur.ru_stime) / 10000 != last_tick) {
            last_tick = us_of(cur.ru_stime) / 10000;
            ticks++;
        }
        prev = cur;
        tprev = tcur;
        mprev = mcur;
        CHECK(clock_gettime(CLOCK_MONOTONIC, &now) == 0, "monotonic");
    } while (ticks < 4 && ns_of(&now) - ns_of(&start) < 5000000000LL);
    ok("cpu times");
}

/* ---- signalfd: takes blocked signals, and poll sees them ---- */

static void test_signalfd(void)
{
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    CHECK(sigprocmask(SIG_BLOCK, &set, NULL) == 0, "block");
    int fd = signalfd(-1, &set, SFD_NONBLOCK | SFD_CLOEXEC);
    CHECK(fd >= 0, "signalfd");
    struct signalfd_siginfo si;
    CHECK(read(fd, &si, sizeof si) < 0 && errno == EAGAIN, "empty");
    CHECK(kill(getpid(), SIGUSR2) == 0, "kill");
    struct pollfd p = {fd, POLLIN, 0};
    CHECK(poll(&p, 1, 1000) == 1 && (p.revents & POLLIN), "poll");
    CHECK(read(fd, &si, sizeof si) == sizeof si, "read");
    CHECK(si.ssi_signo == SIGUSR2 && si.ssi_pid == (uint32_t)getpid() && si.ssi_code == SI_USER,
          "signo %u pid %u code %d", si.ssi_signo, si.ssi_pid, si.ssi_code);
    sigset_t pending;
    CHECK(sigpending(&pending) == 0 && !sigismember(&pending, SIGUSR2), "consumed");
    close(fd);
    sigprocmask(SIG_UNBLOCK, &set, NULL);
    ok("signalfd");
}

/* ---- splice, tee, vmsplice: moving data between pipes ---- */

static void test_splice(void)
{
    int a[2], b[2];
    CHECK(pipe(a) == 0 && pipe(b) == 0, "pipe");
    struct iovec iov[2] = {{"hel", 3}, {"lo", 2}};
    CHECK(vmsplice(a[1], iov, 2, 0) == 5, "vmsplice");
    /* tee copies without taking the data, splice moves it */
    CHECK(tee(a[0], b[1], 100, 0) == 5, "tee");
    CHECK(splice(a[0], NULL, b[1], NULL, 100, 0) == 5, "splice");
    char buf[16] = {0};
    CHECK(read(b[0], buf, sizeof buf) == 10 && strcmp(buf, "hellohello") == 0, "read %s", buf);
    CHECK(splice(a[0], NULL, b[1], NULL, 1, SPLICE_F_NONBLOCK) < 0 && errno == EAGAIN, "empty");
    loff_t off = 0;
    CHECK(splice(a[0], &off, b[1], NULL, 1, 0) < 0 && errno == ESPIPE, "offset on a pipe");
    close(a[0]);
    close(a[1]);
    close(b[0]);
    close(b[1]);
    ok("splice");
}

/* ---- mremap: growing in place, moving, shrinking ---- */

static void test_mremap(void)
{
    size_t page = 4096;
    char *a = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(a != MAP_FAILED, "mmap");
    strcpy(a, "data");
    char *b = mremap(a, 2 * page, 64 * page, MREMAP_MAYMOVE);
    CHECK(b != MAP_FAILED && strcmp(b, "data") == 0, "grow");
    b[63 * page] = 1;
    CHECK(mremap(b, 64 * page, page, 0) == b, "shrink");
    CHECK(mremap(b + 1, page, page, 0) == MAP_FAILED && errno == EINVAL, "unaligned");
    CHECK(munmap(b, page) == 0, "munmap");
    ok("mremap");
}

/* ---- Credentials: the rules after dropping privileges, setgroups, capabilities ---- */

static void test_creds(void)
{
    pid_t pid = fork();
    if (pid == 0) {
        gid_t groups[] = {10, 20};
        CHECK(setgroups(2, groups) == 0, "setgroups");
        gid_t back[4];
        CHECK(getgroups(4, back) == 2 && back[1] == 20, "getgroups");
        CHECK(setresgid(100, 100, 100) == 0, "setresgid");
        /* Change only the effective user ID: root can still be regained afterwards */
        CHECK(setresuid(-1, 1000, -1) == 0 && geteuid() == 1000 && getuid() == 0, "seteuid");
        CHECK(setgroups(0, NULL) < 0 && errno == EPERM, "no CAP_SETGID");
        CHECK(setresuid(-1, 0, -1) == 0 && geteuid() == 0, "back to root");
        /* Change all three IDs: there is no way back */
        CHECK(setuid(1000) == 0, "setuid");
        uid_t r, e, s;
        CHECK(getresuid(&r, &e, &s) == 0 && r == 1000 && e == 1000 && s == 1000, "getresuid");
        CHECK(setuid(0) < 0 && errno == EPERM, "setuid 0");
        CHECK(kill(getppid(), 0) < 0 && errno == EPERM, "kill root's process");
        CHECK(setfsuid(0) == 1000 && setfsuid(-1) == 1000, "setfsuid");
        struct rlimit rl;
        CHECK(getrlimit(RLIMIT_NOFILE, &rl) == 0, "getrlimit");
        rl.rlim_max++;
        CHECK(setrlimit(RLIMIT_NOFILE, &rl) < 0 && errno == EPERM, "raise hard limit");
        _exit(0);
    }
    CHECK(wait_child(pid) == 0, "child");
    CHECK(getuid() == 0 && geteuid() == 0, "parent still root");
    ok("creds");
}

/* ---- Scheduling: affinity, real-time policies, RLIMIT_NOFILE limiting fds ---- */

static void test_sched(void)
{
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(0, &set);
    CHECK(sched_setaffinity(0, sizeof set, &set) == 0, "setaffinity");
    CPU_ZERO(&set);
    CHECK(sched_getaffinity(0, sizeof set, &set) == 0 && CPU_COUNT(&set) == 1, "getaffinity");
    CHECK(sched_getcpu() == 0, "getcpu");
    /* musl's sched_setscheduler and the like do not call the kernel and return ENOSYS directly */
    struct sched_param sp = {.sched_priority = 10};
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_RR, &sp) == 0, "SCHED_RR");
    CHECK(syscall(SYS_sched_getscheduler, 0) == SCHED_RR, "getscheduler");
    struct timespec ts;
    CHECK(sched_rr_get_interval(0, &ts) == 0 && ts.tv_nsec > 0, "rr interval");
    sp.sched_priority = 0;
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_OTHER, &sp) == 0, "SCHED_OTHER");

    struct rlimit rl = {8, 8};
    CHECK(setrlimit(RLIMIT_NOFILE, &rl) == 0, "setrlimit");
    int fds[2];
    while (pipe(fds) == 0) {
    }
    CHECK(errno == EMFILE, "EMFILE");
    ok("sched");
}

/* ---- ptrace: signal-delivery stops, single-stepping, syscall stops, changing the return value,
 * process_vm_readv ---- */

/* The part of struct ptrace_syscall_info that is used */
struct syscall_info {
    unsigned char op;
    unsigned char pad[3];
    unsigned arch;
    unsigned long long pc, sp, nr;
};

static char secret[16] = "ptrace secret";

static void test_ptrace(void)
{
    pid_t pid = fork();
    if (pid == 0) {
        /* If it cannot be traced it must not stop, or the parent would wait forever */
        if (ptrace(PTRACE_TRACEME, 0, 0, 0) != 0)
            _exit(2);
        raise(SIGSTOP);
        _exit(syscall(SYS_getppid) == 4242 ? 0 : 1);
    }
    int st;
    CHECK(waitpid(pid, &st, 0) == pid && WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP, "first stop %#x", st);
    char buf[16];
    struct iovec local = {buf, sizeof buf}, remote = {secret, sizeof secret};
    CHECK(process_vm_readv(pid, &local, 1, &remote, 1, 0) == sizeof buf && !strcmp(buf, "ptrace secret"),
          "process_vm_readv");
    /* Single-step: every stop is SIGTRAP, TRAP_TRACE for ordinary instructions, SI_USER for svc */
    for (int i = 0; i < 3; i++) {
        CHECK(ptrace(PTRACE_SINGLESTEP, pid, 0, 0) == 0, "SINGLESTEP");
        CHECK(waitpid(pid, &st, 0) == pid && WIFSTOPPED(st) && WSTOPSIG(st) == SIGTRAP, "step stop %#x", st);
        siginfo_t si;
        CHECK(ptrace(PTRACE_GETSIGINFO, pid, 0, &si) == 0 && (si.si_code == 2 || si.si_code == SI_USER),
              "step code %d", si.si_code);
    }
    CHECK(ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACESYSGOOD) == 0, "SETOPTIONS");
    CHECK(ptrace(PTRACE_SYSCALL, pid, 0, 0) == 0, "SYSCALL");
    unsigned long long nr = 0;
    for (;;) {
        CHECK(waitpid(pid, &st, 0) == pid, "waitpid");
        if (WIFEXITED(st))
            break;
        CHECK(WIFSTOPPED(st) && WSTOPSIG(st) == (SIGTRAP | 0x80), "syscall stop %#x", st);
        struct syscall_info info;
        long size = ptrace(0x420e /* PTRACE_GET_SYSCALL_INFO */, pid, sizeof info, &info);
        CHECK(size == 80 || size == 33, "GET_SYSCALL_INFO %ld", size);
        if (info.op == 1) {
            nr = info.nr;
        } else if (nr == SYS_getppid) {
            unsigned long long regs[34];
            struct iovec iov = {regs, sizeof regs};
            CHECK(ptrace(PTRACE_GETREGSET, pid, 1 /* NT_PRSTATUS */, &iov) == 0, "GETREGSET");
            regs[0] = 4242;
            CHECK(ptrace(PTRACE_SETREGSET, pid, 1, &iov) == 0, "SETREGSET");
        }
        CHECK(ptrace(PTRACE_SYSCALL, pid, 0, 0) == 0, "SYSCALL");
    }
    CHECK(WEXITSTATUS(st) == 0, "the changed return value");
    ok("ptrace");
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    test_msg();
    test_sem();
    test_shm();
    test_timers();
    test_clocks();
    test_cpu_times();
    test_signalfd();
    test_splice();
    test_mremap();
    test_creds();
    test_sched();
    test_ptrace();
    printf("all ok\n");
    return 0;
}
