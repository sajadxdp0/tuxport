/*
 * Jumps of the host's wall clock (set, or the device resuming from sleep), simulated with a test
 * hook of debug builds (tuxport-run must set TUXPORT_TEST_HOOKS=1), since a test cannot set the
 * host's clock. A TFD_TIMER_CANCEL_ON_SET timerfd on an absolute realtime time becomes readable
 * and reads ECANCELED once (also waking a blocked read and an epoll_wait); other timerfds are left
 * alone; and waits on wall-clock deadlines (an absolute realtime sleep, a relative boottime sleep,
 * a futex wait with an absolute realtime timeout, an absolute realtime POSIX timer) are woken,
 * look at their deadline again and still end on time.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

#define TEST_SYSCALL 0xa991
#define HOOK_CLOCK_JUMP 11

#ifndef TFD_TIMER_CANCEL_ON_SET
#define TFD_TIMER_CANCEL_ON_SET 2
#endif
#define FUTEX_WAIT_BITSET 9
#define FUTEX_CLOCK_REALTIME 256

static void fail(const char *what)
{
    printf("FAIL: %s (%s)\n", what, strerror(errno));
    exit(1);
}

static void jump(void)
{
    if (syscall(TEST_SYSCALL, HOOK_CLOCK_JUMP, 0) != 0)
        fail("clock jump hook");
}

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

/* The realtime clock `ms` milliseconds from now */
static struct timespec realtime_in(long ms)
{
    struct timespec t;
    clock_gettime(CLOCK_REALTIME, &t);
    t.tv_nsec += ms % 1000 * 1000000L;
    t.tv_sec += ms / 1000 + t.tv_nsec / 1000000000L;
    t.tv_nsec %= 1000000000L;
    return t;
}

static void *jumper(void *arg)
{
    (void)arg;
    usleep(50000);
    jump();
    return NULL;
}

/* Jumps the clock from another thread 50 ms from now */
static pthread_t jump_soon(void)
{
    pthread_t t;
    if (pthread_create(&t, NULL, jumper, NULL))
        fail("pthread_create");
    return t;
}

/* A wait of `ms` milliseconds across a jump took about that long */
static void on_time(const char *what, double start, long ms)
{
    double took = now() - start;
    if (took < ms / 1000.0 - 0.01 || took > ms / 1000.0 + 2) {
        printf("FAIL: %s took %.3f s\n", what, took);
        exit(1);
    }
}

int main(void)
{
    uint64_t n;
    struct itimerspec far = {.it_value = realtime_in(3600 * 1000L)};

    /* timerfds: only an absolute realtime timer with CANCEL_ON_SET is cancelled */
    int cancel = timerfd_create(CLOCK_REALTIME, TFD_NONBLOCK);
    if (timerfd_settime(cancel, TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET, &far, NULL))
        fail("timerfd_settime with CANCEL_ON_SET");
    int plain = timerfd_create(CLOCK_REALTIME, TFD_NONBLOCK);
    if (timerfd_settime(plain, TFD_TIMER_ABSTIME, &far, NULL))
        fail("timerfd_settime");
    int mono = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    struct itimerspec hour = {.it_value = {3600, 0}};
    if (timerfd_settime(mono, TFD_TIMER_CANCEL_ON_SET, &hour, NULL))
        fail("monotonic timerfd_settime with CANCEL_ON_SET");
    if (read(cancel, &n, 8) != -1 || errno != EAGAIN)
        fail("read before the jump");
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN};
    if (epoll_ctl(ep, EPOLL_CTL_ADD, cancel, &ev))
        fail("epoll_ctl");
    pthread_t t = jump_soon();
    double start = now();
    if (epoll_wait(ep, &ev, 1, 5000) != 1)
        fail("epoll_wait on the cancelled timerfd");
    if (now() - start > 2)
        fail("epoll_wait woke late");
    pthread_join(t, NULL);
    if (read(cancel, &n, 8) != -1 || errno != ECANCELED)
        fail("read after the jump");
    if (read(cancel, &n, 8) != -1 || errno != EAGAIN)
        fail("second read after the jump");
    struct itimerspec cur;
    if (timerfd_gettime(cancel, &cur) || cur.it_value.tv_sec < 3000)
        fail("the cancelled timer stays armed");
    if (read(plain, &n, 8) != -1 || errno != EAGAIN)
        fail("timer without CANCEL_ON_SET");
    if (read(mono, &n, 8) != -1 || errno != EAGAIN)
        fail("monotonic timer with CANCEL_ON_SET");
    int blocking = timerfd_create(CLOCK_REALTIME, 0);
    if (timerfd_settime(blocking, TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET, &far, NULL))
        fail("blocking timerfd_settime");
    t = jump_soon();
    if (read(blocking, &n, 8) != -1 || errno != ECANCELED)
        fail("blocked read across the jump");
    pthread_join(t, NULL);

    /* An absolute realtime sleep */
    struct timespec until = realtime_in(300);
    t = jump_soon();
    start = now();
    if (clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME, &until, NULL))
        fail("absolute realtime sleep");
    on_time("absolute realtime sleep", start, 300);
    pthread_join(t, NULL);

    /* A relative boottime sleep */
    struct timespec rel = {0, 200000000};
    t = jump_soon();
    start = now();
    if (clock_nanosleep(CLOCK_BOOTTIME, 0, &rel, NULL))
        fail("relative boottime sleep");
    on_time("relative boottime sleep", start, 200);
    pthread_join(t, NULL);

    /* A futex wait with an absolute realtime timeout */
    static uint32_t word;
    until = realtime_in(300);
    t = jump_soon();
    start = now();
    long r = syscall(SYS_futex, &word, FUTEX_WAIT_BITSET | FUTEX_CLOCK_REALTIME, 0, &until, NULL,
                     0xffffffff);
    if (r != -1 || errno != ETIMEDOUT)
        fail("futex wait with an absolute realtime timeout");
    on_time("futex wait", start, 300);
    pthread_join(t, NULL);

    /* An absolute realtime POSIX timer */
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigprocmask(SIG_BLOCK, &set, NULL);
    struct sigevent sev = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGUSR1};
    timer_t timer;
    if (timer_create(CLOCK_REALTIME, &sev, &timer))
        fail("timer_create");
    struct itimerspec at = {.it_value = realtime_in(300)};
    if (timer_settime(timer, TIMER_ABSTIME, &at, NULL))
        fail("timer_settime");
    t = jump_soon();
    start = now();
    struct timespec limit = {5, 0};
    if (sigtimedwait(&set, NULL, &limit) != SIGUSR1)
        fail("absolute realtime POSIX timer");
    on_time("POSIX timer", start, 300);
    pthread_join(t, NULL);

    printf("all ok\n");
    return 0;
}
