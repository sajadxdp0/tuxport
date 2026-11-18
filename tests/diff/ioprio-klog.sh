# docker-run-options: --cap-add SYS_NICE --cap-add SYSLOG
# IO priorities (ioprio_set, ioprio_get, ionice), syslog(2), and the queries of adjtimex and
# clock_adjtime (block/ioprio.c, kernel/printk/printk.c and kernel/time/ntp.c of Linux 6.1). The
# native side adds CAP_SYS_NICE (the RT class needs it) and CAP_SYSLOG, like root in Tuxport; it
# has no CAP_SYS_TIME, and changing the clock in Tuxport is also treated as lacking it. The
# contents of the kernel log, the buffer size, and NTP's offset, freq and maxerror differ between
# the two sides (Tuxport's log is empty, and the NTP state comes from the host but keeps
# changing), so only the error codes, the permissions and the fields that do not change are
# compared; syslog's read (type 2) would consume the real log on the native side, so it is not
# called
set -e
cd /tmp
cat > ioprio.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timex.h>
#include <sys/wait.h>
#include <unistd.h>

#define PROCESS 1
#define PGRP 2
#define USER 3
#define V(c, d) (((c) << 13) | (d))

static long set(int which, int who, int prio) { return syscall(SYS_ioprio_set, which, who, prio); }
static long get(int which, int who) { return syscall(SYS_ioprio_get, which, who); }

static void show(const char *what, long r)
{
    if (r < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %#lx\n", what, r);
    fflush(stdout);
}

static void *thread_get(void *arg)
{
    (void)arg;
    show("  thread inherits", get(PROCESS, 0));
    return NULL;
}

static void in_child(const char *what, int which)
{
    pid_t p = fork();
    if (p == 0) {
        show(what, get(which, 0));
        _exit(0);
    }
    waitpid(p, NULL, 0);
}

/* The child process stays here until it is killed */
static pid_t parked(int how, int pipefd[2])
{
    pid_t p = fork();
    if (p == 0) {
        long r = 0;
        if (how == 1)
            r = nice(10) == -1 ? -1 : 0;
        if (how == 2) {
            /* musl's sched_setscheduler is always ENOSYS */
            struct sched_param sp = {0};
            r = syscall(SYS_sched_setscheduler, 0, SCHED_IDLE, &sp);
        }
        if (how == 3) {
            struct sched_param sp = {.sched_priority = 1};
            r = syscall(SYS_sched_setscheduler, 0, SCHED_FIFO, &sp);
        }
        if (how == 4)
            r = setresgid(12345, 12345, 12345) | setresuid(12345, 12345, 12345);
        char c = r == 0 ? 'y' : 'n';
        write(pipefd[1], &c, 1);
        pause();
        _exit(0);
    }
    char c;
    read(pipefd[0], &c, 1);
    printf("  parked child %d ready: %c\n", how, c);
    return p;
}

static void ioprio(void)
{
    puts("ioprio");
    show("fresh raw", get(PROCESS, 0));
    show("bad class", set(PROCESS, 0, V(4, 0)));
    show("none with data", set(PROCESS, 0, V(0, 1)));
    /* Levels 8 and above are EINVAL on 6.1; from 6.5 on the high bits of data are a hint
       (IOPRIO_PRIO_HINT) and newer kernels accept them, so they are not compared */
    show("check before lookup", set(PROCESS, 99999, V(4, 0)));
    show("no such thread", set(PROCESS, 99999, V(2, 1)));
    show("bad which", set(0, 0, V(2, 1)));
    show("bad which get", get(4, 0));
    show("idle any data", set(PROCESS, 0, V(3, 100)));
    show("  raw", get(PROCESS, 0));
    show("high bits dropped", set(PROCESS, 0, 0x10000 | V(2, 3)));
    show("  raw", get(PROCESS, 0));
    show("rt with CAP_SYS_NICE", set(PROCESS, 0, V(1, 0)));
    show("  raw", get(PROCESS, 0));

    puts("inherit");
    set(PROCESS, 0, V(3, 0));
    in_child("  fork inherits idle", PROCESS);
    pthread_t t;
    pthread_create(&t, NULL, thread_get, NULL);
    pthread_join(t, NULL);
    set(PROCESS, 0, V(0, 0));
    show("  set back to none", get(PROCESS, 0));
    in_child("  fork after none", PROCESS);

    puts("CLONE_IO");
    set(PROCESS, 0, V(2, 3));
    long c = syscall(SYS_clone, CLONE_IO | SIGCHLD, 0, 0, 0, 0);
    if (c == 0) {
        set(PROCESS, 0, V(2, 6));
        _exit(0);
    }
    waitpid(c, NULL, 0);
    show("  shared with CLONE_IO", get(PROCESS, 0));
    pid_t f = fork();
    if (f == 0) {
        set(PROCESS, 0, V(2, 1));
        _exit(0);
    }
    waitpid(f, NULL, 0);
    show("  not shared after fork", get(PROCESS, 0));

    puts("effective from nice and policy");
    pid_t e = fork();
    if (e == 0) {
        /* A process group of its own: a priority never set is derived from the nice value and
         * the scheduling policy, with level (nice + 20) / 5 */
        struct sched_param sp = {0};
        setpgid(0, 0);
        set(PROCESS, 0, V(0, 0));
        show("  nice 0", get(PGRP, 0));
        nice(10);
        show("  nice 10", get(PGRP, 0));
        syscall(SYS_sched_setscheduler, 0, SCHED_IDLE, &sp);
        show("  SCHED_IDLE", get(PGRP, 0));
        sp.sched_priority = 1;
        syscall(SYS_sched_setscheduler, 0, SCHED_FIFO, &sp);
        show("  SCHED_FIFO", get(PGRP, 0));
        show("  raw stays none", get(PROCESS, 0));
        _exit(0);
    }
    waitpid(e, NULL, 0);

    puts("process group");
    setpgid(0, 0);
    set(PROCESS, 0, V(0, 0));
    int pfd[2];
    pipe(pfd);
    pid_t niced = parked(1, pfd), idle = parked(2, pfd), fifo = parked(3, pfd);
    show("  raw of niced child", get(PROCESS, niced));
    show("  best of group", get(PGRP, 0));
    kill(fifo, SIGKILL);
    waitpid(fifo, NULL, 0);
    show("  best without fifo", get(PGRP, 0));
    show("  set one", set(PROCESS, idle, V(2, 1)));
    show("  best of group", get(PGRP, 0));
    show("  set group", set(PGRP, 0, V(3, 0)));
    show("  raw of niced child", get(PROCESS, niced));
    show("  best of group", get(PGRP, 0));
    show("  group by number", get(PGRP, getpgrp()));
    show("  no such group", get(PGRP, 99999));
    kill(niced, SIGKILL);
    kill(idle, SIGKILL);
    waitpid(niced, NULL, 0);
    waitpid(idle, NULL, 0);

    puts("user");
    pid_t u = parked(4, pfd);
    show("  effective of user", get(USER, 12345));
    show("  set user", set(USER, 12345, V(3, 7)));
    show("  raw of its process", get(PROCESS, u));
    show("  no such user", get(USER, 54321));
    show("  set no such user", set(USER, 54321, V(2, 1)));
    pid_t me = getpid();
    pid_t w = fork();
    if (w == 0) {
        setresgid(12345, 12345, 12345);
        setresuid(12345, 12345, 12345);
        show("  other's process", set(PROCESS, me, V(2, 0)));
        show("  rt without caps", set(PROCESS, 0, V(1, 0)));
        show("  own process", set(PROCESS, 0, V(2, 7)));
        show("  same user's process", set(PROCESS, u, V(2, 2)));
        show("  user 0 means root here", set(USER, 0, V(2, 7)));
        show("  get user 0 means me", get(USER, 0));
        _exit(0);
    }
    waitpid(w, NULL, 0);
    show("  raw after same user set", get(PROCESS, u));
    kill(u, SIGKILL);
    waitpid(u, NULL, 0);
}

static void klog(void)
{
    char buf[64];
    puts("syslog");
    show("close", syscall(SYS_syslog, 0, NULL, 0));
    show("open", syscall(SYS_syslog, 1, NULL, 0));
    show("read all null", syscall(SYS_syslog, 3, NULL, 10));
    show("read all negative", syscall(SYS_syslog, 3, buf, -1));
    show("read all empty", syscall(SYS_syslog, 3, buf, 0));
    show("read all beyond user space", syscall(SYS_syslog, 3, (char *)0xffff000000000000, 10));
    show("read null", syscall(SYS_syslog, 2, NULL, 0));
    show("read empty", syscall(SYS_syslog, 2, buf, 0));
    show("console level 0", syscall(SYS_syslog, 8, NULL, 0));
    show("console level 9", syscall(SYS_syslog, 8, NULL, 9));
    show("type 11", syscall(SYS_syslog, 11, NULL, 0));
    show("type -1", syscall(SYS_syslog, -1, NULL, 0));
    printf("buffer size positive: %d\n", syscall(SYS_syslog, 10, NULL, 0) > 0);
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        setresuid(65534, 65534, 65534);
        FILE *f = fopen("/proc/sys/kernel/dmesg_restrict", "r");
        int restrict_ = fgetc(f) == '1';
        fclose(f);
        show("  unprivileged clear", syscall(SYS_syslog, 5, NULL, 0));
        show("  unprivileged type 11", syscall(SYS_syslog, 11, NULL, 0));
        show("  unprivileged open", syscall(SYS_syslog, 1, NULL, 0));
        long r = syscall(SYS_syslog, 10, NULL, 0);
        printf("  unprivileged size follows dmesg_restrict: %d\n",
               restrict_ ? r == -1 && errno == EPERM : r > 0);
        r = syscall(SYS_syslog, 3, buf, 0);
        printf("  unprivileged read all follows dmesg_restrict: %d\n",
               restrict_ ? r == -1 && errno == EPERM : r == 0);
        fflush(stdout);
        _exit(0);
    }
    waitpid(p, NULL, 0);
}

static long adj(struct timex *t) { return syscall(SYS_adjtimex, t); }
static long cadj(int clock, struct timex *t) { return syscall(SYS_clock_adjtime, clock, t); }

static void adjtime_query(void)
{
    puts("adjtimex");
    struct timex t;
    memset(&t, 0x5a, sizeof t);
    t.modes = 0;
    long r = adj(&t);
    struct timeval now;
    gettimeofday(&now, NULL);
    long usec = t.status & STA_NANO ? t.time.tv_usec / 1000 : t.time.tv_usec;
    double diff = (now.tv_sec - t.time.tv_sec) + (now.tv_usec - usec) / 1e6;
    printf("query: %ld status %#x constant %ld precision %ld tolerance %ld tick %ld tai %d\n", r,
           t.status, t.constant, t.precision, t.tolerance, t.tick, t.tai);
    printf("  modes kept %#x, padding kept %d, time close %d\n", t.modes,
           t.__padding[0] == 0x5a5a5a5a, diff >= 0 && diff < 2);

    struct timex q = {.modes = ADJ_OFFSET_SS_READ};
    show("adjtime read", adj(&q));
    printf("  offset %ld modes %#x\n", q.offset, q.modes);
    /* In the kernel ADJ_ADJTIME is 0x8000 and ADJ_OFFSET_SINGLESHOT is 0x0001 */
    struct timex m = {.modes = 0x8000};
    show("adjtime without singleshot", adj(&m));
    m.modes = 0xa000;
    show("adjtime readonly without singleshot", adj(&m));
    m.modes = ADJ_OFFSET_SINGLESHOT;
    show("adjtime singleshot", adj(&m));
    m.modes = ADJ_STATUS;
    show("set status", adj(&m));
    m.modes = ADJ_TICK;
    m.tick = 1;
    show("bad tick", adj(&m));
    m.modes = ADJ_SETOFFSET | ADJ_OFFSET_SS_READ;
    show("set offset", adj(&m));
    m.modes = ADJ_OFFSET_SS_READ | ADJ_FREQUENCY;
    m.freq = 0x7fffffffffffffffL;
    show("frequency overflow", adj(&m));
    m.freq = 0;
    show("frequency ok when readonly", adj(&m));
    show("null", adj(NULL));

    struct timex *ro = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ro->modes = ADJ_STATUS;
    mprotect(ro, 4096, PROT_READ);
    show("eperm then write back to read-only", adj(ro));
    show("clock_adjtime eperm on read-only", cadj(CLOCK_REALTIME, ro));
    mprotect(ro, 4096, PROT_READ | PROT_WRITE);
    ro->modes = 0;
    mprotect(ro, 4096, PROT_READ);
    show("query into read-only", adj(ro));
    show("clock_adjtime query into read-only", cadj(CLOCK_REALTIME, ro));

    puts("clock_adjtime");
    int clocks[] = {CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW, CLOCK_BOOTTIME, CLOCK_REALTIME_ALARM,
                    10, CLOCK_TAI, 12, 1000, -6, -2, -5};
    for (unsigned i = 0; i < sizeof clocks / sizeof *clocks; i++) {
        struct timex z = {0};
        char what[32];
        snprintf(what, sizeof what, "  clock %d", clocks[i]);
        show(what, cadj(clocks[i], &z));
    }
    struct timex z = {0};
    printf("  realtime same as adjtimex: %d\n", cadj(CLOCK_REALTIME, &z) == r);
    show("  bad pointer before bad clock", cadj(10, NULL));
}

int main(void)
{
    ioprio();
    klog();
    adjtime_query();
    return 0;
}
C
gcc -O1 -pthread -o ioprio ioprio.c && ./ioprio
# ionice (util-linux's; busybox's is compared in a separate pass): when the setting fails, the
# command is not run unless -t is given
echo ionice
ionice
ionice -c3 sh -c ionice
ionice -c2 -n5 ionice
ionice -c1 -n2 ionice
su -s /bin/sh nobody -c 'ionice -c1 -n0 echo not run' || echo "failed: $?"
su -s /bin/sh nobody -c 'ionice -t -c1 -n0 echo tolerant'
busybox ionice -c3 sh -c 'busybox ionice'
su -s /bin/sh nobody -c 'busybox ionice -c1 -n0 echo not run' || echo "busybox failed: $?"
