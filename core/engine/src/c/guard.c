/*
 * Host-side fault guard (the engine map, design doc §12 item 5): when a bug in the engine itself
 * (wrongly translated code, a broken page-table entry) makes the host receive SIGSEGV, SIGBUS or
 * SIGILL, only the guest process running at the time is ended, instead of crashing the whole app.
 *
 * tuxport_cpu_run() and tuxport_dbt_run() run through tuxport_guard_run(), and during that time the
 * guard is "armed": when a fault happens then, the handler siglongjmps back to
 * tuxport_guard_run(), which returns TUXPORT_EXIT_HOST_FAULT. So during that time the engine does
 * not call back into the kernel layer and does not call malloc (except for writing the perf map
 * file when profiling with TUXPORT_PERF_MAP), and every lock it holds is registered with
 * tuxport_guard_hold() (or, for a set of locks taken together, tuxport_guard_hold_fn()) and
 * released after jumping back.
 *
 * Other faults (the kernel layer's own, other host threads') first write a one-line report to
 * stderr and are then passed to the original handler (on Android, debuggerd, which produces a
 * tombstone) or the default action.
 */
#define _GNU_SOURCE /* needed for sigjmp_buf, siginfo_t and ucontext_t under -std=c17 */
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdlib.h>
#include <unistd.h>
#ifdef __linux__
#include <sys/prctl.h>
#include <ucontext.h>
#endif

#include "internal.h"

#define MAX_LOCKS 4

/* One per thread, allocated the first time it runs guest code and freed when the thread exits */
struct guard {
    sigjmp_buf env;
    volatile sig_atomic_t armed; /* the handler reads it on the same thread */
    uint64_t fpcr;               /* the host's FPCR when armed: translated code holds the guest's */
    /* Locks held, innermost last: the first MAX_LOCKS are recorded (nlocks), deeper ones only
     * counted, so that drops still pair with the right holds */
    unsigned depth;
    unsigned nlocks;
    struct {
        void *lock;
        void (*release)(void *);
    } locks[MAX_LOCKS];
    struct tuxport_host_fault fault;
};

static const int signals[] = {SIGSEGV, SIGBUS, SIGILL};
#define NSIGNALS (sizeof(signals) / sizeof(signals[0]))

static pthread_once_t once = PTHREAD_ONCE_INIT;
static pthread_key_t key;
static bool installed;
static struct sigaction previous[NSIGNALS];

static uint64_t read_fpcr(void)
{
    uint64_t v = 0;
#ifdef __aarch64__
    __asm__ volatile("mrs %0, fpcr" : "=r"(v));
#endif
    return v;
}

static void write_fpcr(uint64_t v)
{
#ifdef __aarch64__
    __asm__ volatile("msr fpcr, %0" ::"r"(v));
#else
    (void)v;
#endif
}

static uint64_t context_pc(void *uc)
{
#if defined(__aarch64__) && defined(__linux__)
    return ((ucontext_t *)uc)->uc_mcontext.pc;
#else
    (void)uc;
    return 0;
#endif
}

/* ---- Signal handlers may only use async-signal-safe functions: build the report by hand ---- */

struct line {
    char buf[192];
    size_t n;
};

static void put_str(struct line *l, const char *s)
{
    while (*s && l->n < sizeof(l->buf))
        l->buf[l->n++] = *s++;
}

static void put_num(struct line *l, uint64_t v, unsigned base)
{
    char digits[20];
    unsigned n = 0;
    if (base == 16)
        put_str(l, "0x");
    do {
        digits[n++] = "0123456789abcdef"[v % base];
        v /= base;
    } while (v);
    while (n && l->n < sizeof(l->buf))
        l->buf[l->n++] = digits[--n];
}

static void report(int sig, const siginfo_t *info, void *uc)
{
    struct line l = {.n = 0};
    char name[17] = "?";
#ifdef __linux__
    prctl(PR_GET_NAME, name, 0, 0, 0);
    name[16] = 0;
#endif
    put_str(&l, "tuxport: host signal ");
    put_num(&l, (uint64_t)sig, 10);
    put_str(&l, " (code ");
    if (info->si_code < 0)
        put_str(&l, "-");
    put_num(&l, (uint64_t)(info->si_code < 0 ? -(int64_t)info->si_code : info->si_code), 10);
    put_str(&l, ") in thread ");
    put_str(&l, name);
    put_str(&l, " at pc ");
    put_num(&l, context_pc(uc), 16);
    put_str(&l, ", addr ");
    put_num(&l, (uint64_t)(uintptr_t)info->si_addr, 16);
    put_str(&l, "\n");
    ssize_t r = write(2, l.buf, l.n);
    (void)r;
}

/* Pass on to the handling in place before the guard was installed */
static void chain(int sig, siginfo_t *info, void *uc)
{
    const struct sigaction *old = &previous[0];
    for (size_t i = 0; i < NSIGNALS; i++) {
        if (signals[i] == sig)
            old = &previous[i];
    }
    if (old->sa_handler != SIG_DFL && old->sa_handler != SIG_IGN) {
        if (old->sa_flags & SA_SIGINFO)
            old->sa_sigaction(sig, info, uc);
        else
            old->sa_handler(sig);
        return;
    }
    /* Default action (a synchronous fault that is ignored is also handled as default): restore
     * the default handling and return, and the host kernel ends the process when the faulting
     * instruction runs again. A signal someone else sent with kill does not recur, so send it
     * again ourselves */
    struct sigaction dfl = {.sa_flags = 0};
    dfl.sa_handler = SIG_DFL;
    sigemptyset(&dfl.sa_mask);
    sigaction(sig, &dfl, NULL);
    if (info->si_code <= 0)
        raise(sig);
}

static void handler(int sig, siginfo_t *info, void *uc)
{
    struct guard *g = pthread_getspecific(key);
    /* si_code > 0: generated by the host kernel for this instruction of this thread, not sent by
     * someone else */
    if (g && g->armed && info->si_code > 0) {
        g->armed = 0;
        g->fault = (struct tuxport_host_fault){
            .signo = sig,
            .code = info->si_code,
            .addr = (uint64_t)(uintptr_t)info->si_addr,
            .pc = context_pc(uc),
        };
        siglongjmp(g->env, 1);
    }
    report(sig, info, uc);
    chain(sig, info, uc);
}

static void install(void)
{
    if (pthread_key_create(&key, free) != 0)
        return;
    struct sigaction sa = {.sa_flags = SA_SIGINFO | SA_ONSTACK};
    sa.sa_sigaction = handler;
    sigemptyset(&sa.sa_mask);
    for (size_t i = 0; i < NSIGNALS; i++)
        sigaction(signals[i], &sa, &previous[i]);
    __atomic_store_n(&installed, true, __ATOMIC_RELEASE);
}

void tuxport_fault_guard_install(void)
{
    pthread_once(&once, install);
}

/* The current thread's guard; NULL when the guard is not installed */
static struct guard *current(void)
{
    if (!__atomic_load_n(&installed, __ATOMIC_ACQUIRE))
        return NULL;
    return pthread_getspecific(key);
}

static void unlock_mutex(void *lock)
{
    pthread_mutex_unlock(lock);
}

static void unlock_rwlock(void *lock)
{
    pthread_rwlock_unlock(lock);
}

void tuxport_guard_hold_fn(void *lock, void (*release)(void *))
{
    struct guard *g = current();
    if (g && g->armed) {
        if (g->nlocks == g->depth && g->nlocks < MAX_LOCKS) {
            g->locks[g->nlocks].lock = lock;
            g->locks[g->nlocks].release = release;
            g->nlocks++;
        }
        g->depth++;
    }
}

void tuxport_guard_hold(void *lock, bool rw)
{
    tuxport_guard_hold_fn(lock, rw ? unlock_rwlock : unlock_mutex);
}

void tuxport_guard_drop(void)
{
    struct guard *g = current();
    if (g && g->armed && g->depth) {
        g->depth--;
        if (g->nlocks > g->depth)
            g->nlocks = g->depth;
    }
}

_Noreturn void tuxport_guard_fail(void)
{
    struct guard *g = current();
    if (g && g->armed) {
        g->armed = 0;
        g->fault = (struct tuxport_host_fault){
            .signo = SIGABRT,
            .code = 0,
            .addr = 0,
            .pc = (uint64_t)(uintptr_t)__builtin_return_address(0),
        };
        siglongjmp(g->env, 1);
    }
    abort();
}

/* For the engine's unit test: a run whose function fails an internal check returns
 * TUXPORT_EXIT_HOST_FAULT with SIGABRT recorded; returns that signal (-1 when the run returned
 * otherwise, -2 when the locks held were not released as registered). Only with the guard
 * installed: without it the check aborts the process */
static pthread_mutex_t test_locks[5] = {PTHREAD_MUTEX_INITIALIZER, PTHREAD_MUTEX_INITIALIZER,
                                        PTHREAD_MUTEX_INITIALIZER, PTHREAD_MUTEX_INITIALIZER,
                                        PTHREAD_MUTEX_INITIALIZER};
static int test_released;

static void test_release(void *unused)
{
    (void)unused;
    test_released++;
}

/* Three locks and a released set are registered; two more locks nested inside them, beyond
 * MAX_LOCKS, are taken and given back before the check fails: the four registered are released */
static enum tuxport_exit fail_check(void *ctx, struct tuxport_cpu *cpu)
{
    (void)ctx;
    (void)cpu;
    for (unsigned i = 0; i < 3; i++) {
        pthread_mutex_lock(&test_locks[i]);
        tuxport_guard_hold(&test_locks[i], false);
    }
    tuxport_guard_hold_fn(NULL, test_release);
    for (unsigned i = 3; i < 5; i++) {
        pthread_mutex_lock(&test_locks[i]);
        tuxport_guard_hold(&test_locks[i], false);
    }
    for (unsigned i = 5; i-- > 3;) {
        tuxport_guard_drop();
        pthread_mutex_unlock(&test_locks[i]);
    }
    tuxport_guard_fail();
}

int tuxport_guard_selftest(void)
{
    if (tuxport_guard_run(fail_check, NULL, NULL) != TUXPORT_EXIT_HOST_FAULT)
        return -1;
    for (unsigned i = 0; i < 5; i++) {
        if (pthread_mutex_trylock(&test_locks[i]) != 0)
            return -2;
        pthread_mutex_unlock(&test_locks[i]);
    }
    if (test_released != 1)
        return -2;
    struct tuxport_host_fault f;
    tuxport_host_fault_last(&f);
    return f.signo;
}

/* After jumping back from the handler: restore the host's FPCR, release the locks still held,
 * and unblock this signal (it is blocked while the handler runs, and siglongjmp does not restore
 * the signal mask) */
static enum tuxport_exit recover(struct guard *g)
{
    write_fpcr(g->fpcr);
    while (g->nlocks) {
        g->nlocks--;
        g->locks[g->nlocks].release(g->locks[g->nlocks].lock);
    }
    g->depth = 0;
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, g->fault.signo);
    pthread_sigmask(SIG_UNBLOCK, &set, NULL);
    return TUXPORT_EXIT_HOST_FAULT;
}

/* The current thread's guard, allocated on first use; NULL when the guard is not installed */
static struct guard *current_or_new(void)
{
    struct guard *g = current();
    if (!g && __atomic_load_n(&installed, __ATOMIC_ACQUIRE)) {
        g = calloc(1, sizeof(*g));
        if (g && pthread_setspecific(key, g) != 0) {
            free(g);
            g = NULL;
        }
    }
    return g;
}

enum tuxport_exit tuxport_guard_run(enum tuxport_exit (*fn)(void *, struct tuxport_cpu *), void *ctx,
                                  struct tuxport_cpu *cpu)
{
    /* Still needed after siglongjmp returns: keep it in memory, not only in a register that may be
     * clobbered */
    struct guard *volatile g = current_or_new();
    if (!g)
        return fn(ctx, cpu);
    if (sigsetjmp(g->env, 0))
        return recover(g);
    g->fpcr = read_fpcr();
    g->depth = 0;
    g->nlocks = 0;
    g->armed = 1;
    enum tuxport_exit r = fn(ctx, cpu);
    g->armed = 0;
    return r;
}

void tuxport_host_fault_last(struct tuxport_host_fault *out)
{
    struct guard *g = current();
    *out = g ? g->fault : (struct tuxport_host_fault){0};
}
