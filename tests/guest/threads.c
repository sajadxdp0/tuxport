/*
 * Guest test program for threads, futexes and signals (M3a): build with musl-gcc -static and run
 * directly in tuxport-run (no rootfs needed). Each check that passes prints a line "ok <name>"; a
 * failure prints the reason and exits with 1; when all pass it prints "all ok" at the end.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            printf("FAIL %s:%d: ", __func__, __LINE__);                                            \
            printf(__VA_ARGS__);                                                                   \
            printf("\n");                                                                          \
            exit(1);                                                                               \
        }                                                                                          \
    } while (0)

static void ok(const char *name)
{
    printf("ok %s\n", name);
    fflush(stdout);
}

static void sleep_ms(long ms)
{
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000};
    while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
    }
}

static void handle(int sig, void (*fn)(int), int flags)
{
    struct sigaction sa = {0};
    sa.sa_handler = fn;
    sa.sa_flags = flags;
    sigemptyset(&sa.sa_mask);
    CHECK(sigaction(sig, &sa, NULL) == 0, "sigaction %d", sig);
}

/* ---- Mutexes, atomics, thread-local storage ---- */

#define THREADS 8
#define ROUNDS 4000

static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static long counter;
static atomic_long atomic_counter;
static __thread int tls_value;

static void *count_worker(void *arg)
{
    tls_value = (int)(intptr_t)arg;
    for (int i = 0; i < ROUNDS; i++) {
        pthread_mutex_lock(&mutex);
        counter++;
        pthread_mutex_unlock(&mutex);
        atomic_fetch_add(&atomic_counter, 1);
        if (i % 500 == 0)
            sched_yield();
    }
    return (void *)(intptr_t)(tls_value * 10);
}

static void test_counters(void)
{
    pthread_t t[THREADS];
    for (int i = 0; i < THREADS; i++)
        CHECK(pthread_create(&t[i], NULL, count_worker, (void *)(intptr_t)(i + 1)) == 0, "create");
    for (int i = 0; i < THREADS; i++) {
        void *ret;
        CHECK(pthread_join(t[i], &ret) == 0, "join");
        CHECK((intptr_t)ret == (i + 1) * 10, "thread %d returned %ld", i, (long)(intptr_t)ret);
    }
    CHECK(counter == THREADS * ROUNDS, "mutex counter %ld", counter);
    CHECK(atomic_counter == THREADS * ROUNDS, "atomic counter %ld", (long)atomic_counter);
    CHECK(tls_value == 0, "main thread tls %d", tls_value);
    ok("mutex-atomic-tls");
}

/* ---- Condition variables: producer and consumer ---- */

#define ITEMS 2000
#define QUEUE 8

static pthread_mutex_t qmutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t not_empty = PTHREAD_COND_INITIALIZER;
static pthread_cond_t not_full = PTHREAD_COND_INITIALIZER;
static int queue[QUEUE];
static int q_head, q_len;

static void *producer(void *arg)
{
    (void)arg;
    for (int i = 1; i <= ITEMS; i++) {
        pthread_mutex_lock(&qmutex);
        while (q_len == QUEUE)
            pthread_cond_wait(&not_full, &qmutex);
        queue[(q_head + q_len) % QUEUE] = i;
        q_len++;
        pthread_cond_signal(&not_empty);
        pthread_mutex_unlock(&qmutex);
    }
    return NULL;
}

static void *consumer(void *arg)
{
    (void)arg;
    long sum = 0;
    for (int i = 0; i < ITEMS; i++) {
        pthread_mutex_lock(&qmutex);
        while (q_len == 0)
            pthread_cond_wait(&not_empty, &qmutex);
        sum += queue[q_head];
        q_head = (q_head + 1) % QUEUE;
        q_len--;
        pthread_cond_signal(&not_full);
        pthread_mutex_unlock(&qmutex);
    }
    return (void *)sum;
}

static void test_condvar(void)
{
    pthread_t p, c;
    void *sum;
    CHECK(pthread_create(&c, NULL, consumer, NULL) == 0, "create consumer");
    CHECK(pthread_create(&p, NULL, producer, NULL) == 0, "create producer");
    pthread_join(p, NULL);
    pthread_join(c, &sum);
    CHECK((long)sum == (long)ITEMS * (ITEMS + 1) / 2, "sum %ld", (long)sum);

    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    ts.tv_nsec += 50 * 1000000;
    if (ts.tv_nsec >= 1000000000) {
        ts.tv_sec++;
        ts.tv_nsec -= 1000000000;
    }
    pthread_mutex_lock(&qmutex);
    int rc = pthread_cond_timedwait(&not_empty, &qmutex, &ts);
    pthread_mutex_unlock(&qmutex);
    CHECK(rc == ETIMEDOUT, "timedwait returned %d", rc);
    ok("condvar");
}

/* ---- Signals sent to a given thread ---- */

static pthread_t handled_by;
static atomic_int usr1_count;

static void on_usr1(int sig)
{
    (void)sig;
    handled_by = pthread_self();
    atomic_fetch_add(&usr1_count, 1);
}

static void *wait_for_usr1(void *arg)
{
    (void)arg;
    while (atomic_load(&usr1_count) == 0)
        sleep_ms(1);
    return NULL;
}

static void test_thread_signal(void)
{
    handle(SIGUSR1, on_usr1, 0);
    pthread_t t;
    CHECK(pthread_create(&t, NULL, wait_for_usr1, NULL) == 0, "create");
    sleep_ms(10);
    CHECK(pthread_kill(t, SIGUSR1) == 0, "pthread_kill");
    pthread_join(t, NULL);
    CHECK(pthread_equal(handled_by, t), "SIGUSR1 handled by the wrong thread");
    ok("thread-signal");
}

/* ---- Alternate signal stack ---- */

static char *alt_base;
static size_t alt_size;
static volatile int on_alt, reported_onstack;

static void on_usr2(int sig)
{
    (void)sig;
    char local;
    on_alt = &local >= alt_base && &local < alt_base + alt_size;
    stack_t cur;
    sigaltstack(NULL, &cur);
    reported_onstack = (cur.ss_flags & SS_ONSTACK) != 0;
}

static void test_altstack(void)
{
    alt_size = 64 * 1024;
    alt_base = malloc(alt_size);
    stack_t ss = {.ss_sp = alt_base, .ss_size = alt_size, .ss_flags = 0};
    CHECK(sigaltstack(&ss, NULL) == 0, "sigaltstack");
    stack_t small = {.ss_sp = alt_base, .ss_size = 1024, .ss_flags = 0};
    CHECK(sigaltstack(&small, NULL) == -1 && errno == ENOMEM, "small altstack accepted");
    handle(SIGUSR2, on_usr2, SA_ONSTACK);
    raise(SIGUSR2);
    CHECK(on_alt, "handler did not run on the alternate stack");
    CHECK(reported_onstack, "SS_ONSTACK not reported");
    stack_t cur;
    sigaltstack(NULL, &cur);
    CHECK(!(cur.ss_flags & SS_ONSTACK) && cur.ss_sp == alt_base, "altstack after return");
    ok("altstack");
}

/* ---- Memory access faults: the handler fixes things up and returns, or jumps out with
 * siglongjmp ---- */

static char *page;
static volatile int faults, last_code;
static sigjmp_buf jump;

static void on_segv_fix(int sig, siginfo_t *si, void *uc)
{
    (void)sig;
    (void)uc;
    faults++;
    last_code = si->si_code;
    if (si->si_addr == page)
        mprotect(page, 4096, PROT_READ | PROT_WRITE);
}

static void on_segv_jump(int sig, siginfo_t *si, void *uc)
{
    (void)sig;
    (void)uc;
    last_code = si->si_code;
    siglongjmp(jump, 1);
}

static void test_faults(void)
{
    struct sigaction sa = {0};
    sa.sa_sigaction = on_segv_fix;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &sa, NULL);
    page = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(page != MAP_FAILED, "mmap");
    *(volatile char *)page = 42;
    CHECK(faults == 1 && page[0] == 42, "faults %d value %d", faults, page[0]);
    CHECK(last_code == SEGV_ACCERR, "si_code %d", last_code);

    sa.sa_sigaction = on_segv_jump;
    sigaction(SIGSEGV, &sa, NULL);
    /* The address is passed through a volatile variable, so the compiler cannot tell it is a
     * constant */
    static volatile uintptr_t unmapped = 16;
    if (sigsetjmp(jump, 1) == 0) {
        *(volatile int *)unmapped = 1;
        CHECK(0, "no fault at address 16");
    }
    CHECK(last_code == SEGV_MAPERR, "si_code %d", last_code);
    /* siglongjmp restored the signal mask, so SIGSEGV must not still be blocked */
    sigset_t cur;
    sigprocmask(SIG_BLOCK, NULL, &cur);
    CHECK(!sigismember(&cur, SIGSEGV), "SIGSEGV still blocked");
    signal(SIGSEGV, SIG_DFL);
    ok("faults");
}

/* ---- SA_RESTART: whether a read interrupted by a signal is restarted ---- */

static int restart_pipe[2];
static pthread_t main_thread;

static void *interrupt_read(void *arg)
{
    (void)arg;
    sleep_ms(30);
    pthread_kill(main_thread, SIGUSR1);
    sleep_ms(30);
    write(restart_pipe[1], "x", 1);
    return NULL;
}

static void test_restart(void)
{
    main_thread = pthread_self();
    for (int restart = 0; restart <= 1; restart++) {
        CHECK(pipe(restart_pipe) == 0, "pipe");
        handle(SIGUSR1, on_usr1, restart ? SA_RESTART : 0);
        atomic_store(&usr1_count, 0);
        pthread_t t;
        pthread_create(&t, NULL, interrupt_read, NULL);
        char c;
        ssize_t n = read(restart_pipe[0], &c, 1);
        if (restart) {
            CHECK(n == 1 && c == 'x', "restarted read returned %zd", n);
        } else {
            CHECK(n == -1 && errno == EINTR, "interrupted read returned %zd", n);
            CHECK(read(restart_pipe[0], &c, 1) == 1, "second read");
        }
        CHECK(atomic_load(&usr1_count) == 1, "handler ran %d times", atomic_load(&usr1_count));
        pthread_join(t, NULL);
        close(restart_pipe[0]);
        close(restart_pipe[1]);
    }
    ok("restart");
}

/* ---- futex and nanosleep (the no-BKL paths): interrupted by a signal, timeouts, being woken, the
 * time remaining. Cross-process futexes in shared file mappings need a filesystem and are in the
 * differential test c-programs ---- */

#define FUTEX_WAIT 0
#define FUTEX_WAKE 1
#define FUTEX_WAIT_PRIVATE (FUTEX_WAIT | 128)
#define FUTEX_WAKE_PRIVATE (FUTEX_WAKE | 128)

static atomic_int fword;

static long futex(atomic_int *addr, int op, int val, const struct timespec *ts)
{
    return syscall(SYS_futex, addr, op, val, ts, NULL, 0);
}

static long long ms_since(const struct timespec *start)
{
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (now.tv_sec - start->tv_sec) * 1000LL + (now.tv_nsec - start->tv_nsec) / 1000000;
}

static void *interrupt_main(void *arg)
{
    (void)arg;
    sleep_ms(50);
    pthread_kill(main_thread, SIGUSR1);
    return NULL;
}

static void *wake_main(void *arg)
{
    (void)arg;
    sleep_ms(30);
    atomic_store(&fword, 1);
    futex(&fword, FUTEX_WAKE_PRIVATE, 1, NULL);
    return NULL;
}

static void test_futex_sleep(void)
{
    main_thread = pthread_self();
    handle(SIGUSR1, on_usr1, 0);
    pthread_t t;
    struct timespec start;

    /* A wait interrupted by a signal (the handler has no SA_RESTART): EINTR */
    atomic_store(&fword, 0);
    pthread_create(&t, NULL, interrupt_main, NULL);
    long r = futex(&fword, FUTEX_WAIT_PRIVATE, 0, NULL);
    CHECK(r == -1 && errno == EINTR, "interrupted futex wait returned %ld errno %d", r, errno);
    pthread_join(t, NULL);

    /* Timeout, wrong value */
    struct timespec ts = {0, 50000000};
    clock_gettime(CLOCK_MONOTONIC, &start);
    r = futex(&fword, FUTEX_WAIT_PRIVATE, 0, &ts);
    CHECK(r == -1 && errno == ETIMEDOUT, "futex timeout returned %ld errno %d", r, errno);
    CHECK(ms_since(&start) >= 50, "futex timed out after %lld ms", ms_since(&start));
    r = futex(&fword, FUTEX_WAIT_PRIVATE, 1, NULL);
    CHECK(r == -1 && errno == EAGAIN, "futex with a stale value returned %ld errno %d", r, errno);

    /* Woken by another thread */
    pthread_create(&t, NULL, wake_main, NULL);
    while (atomic_load(&fword) == 0) {
        r = futex(&fword, FUTEX_WAIT_PRIVATE, 0, NULL);
        CHECK(r == 0 || errno == EAGAIN, "futex wait returned %ld errno %d", r, errno);
    }
    pthread_join(t, NULL);

    /* nanosleep interrupted by a signal: what remains is what really remains, not recomputed from
     * the moment of the interruption */
    pthread_create(&t, NULL, interrupt_main, NULL);
    struct timespec req = {1, 0}, rem = {0, 0};
    r = nanosleep(&req, &rem);
    long long left = rem.tv_sec * 1000LL + rem.tv_nsec / 1000000;
    CHECK(r == -1 && errno == EINTR, "interrupted nanosleep returned %ld errno %d", r, errno);
    CHECK(left > 500 && left < 990, "%lld ms left of 1000 after about 50", left);
    pthread_join(t, NULL);

    ok("futex_sleep");
}

/* ---- While one thread keeps running guest code, another thread munmaps repeatedly: the kicked
 * thread does not enter the kernel and goes back to guest code at once, yet the unmapping side must
 * still be able to wait until it has left (if it cannot, it hangs holding the BKL) ---- */

static atomic_int spinning, stop_spin;

static void *spin_only(void *arg)
{
    (void)arg;
    atomic_store(&spinning, 1);
    while (!atomic_load(&stop_spin)) {
    }
    return NULL;
}

static void test_unmap_while_spinning(void)
{
    pthread_t t;
    pthread_create(&t, NULL, spin_only, NULL);
    while (!atomic_load(&spinning))
        sleep_ms(1);
    for (int i = 0; i < 2000; i++) {
        char *p = mmap(NULL, 65536, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        CHECK(p != MAP_FAILED, "mmap");
        p[0] = 1;
        CHECK(munmap(p, 65536) == 0, "munmap");
    }
    atomic_store(&stop_spin, 1);
    pthread_join(t, NULL);
    ok("unmap_while_spinning");
}

/* ---- fork in a multithreaded process: the child has only the thread that called fork, and its
 * memory is a copy ---- */

static atomic_int stop_spinning;

static void *spin(void *arg)
{
    (void)arg;
    while (!atomic_load(&stop_spinning))
        sleep_ms(1);
    return NULL;
}

static void test_fork(void)
{
    pthread_t t;
    pthread_create(&t, NULL, spin, NULL);
    long before = counter;
    pid_t pid = fork();
    CHECK(pid >= 0, "fork");
    if (pid == 0) {
        counter = -1;
        pthread_mutex_lock(&mutex);
        pthread_mutex_unlock(&mutex);
        _exit(getppid() > 1 ? 7 : 8);
    }
    int status;
    CHECK(waitpid(pid, &status, 0) == pid, "waitpid");
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 7, "child status %#x", status);
    CHECK(counter == before, "child changed the parent's memory");
    atomic_store(&stop_spinning, 1);
    pthread_join(t, NULL);
    ok("fork");
}

/* ---- A detached thread exits: musl frees its own stack with __unmapself ---- */

static atomic_int detached_done;

static void *detached(void *arg)
{
    (void)arg;
    atomic_fetch_add(&detached_done, 1);
    return NULL;
}

static void test_detached(void)
{
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
    for (int i = 0; i < 16; i++) {
        pthread_t t;
        CHECK(pthread_create(&t, &attr, detached, NULL) == 0, "create detached");
    }
    for (int i = 0; i < 2000 && atomic_load(&detached_done) < 16; i++)
        sleep_ms(1);
    CHECK(atomic_load(&detached_done) == 16, "%d detached threads ran", atomic_load(&detached_done));
    ok("detached");
}

int main(void)
{
    test_counters();
    test_condvar();
    test_thread_signal();
    test_altstack();
    test_faults();
    test_restart();
    test_futex_sleep();
    test_unmap_while_spinning();
    test_fork();
    test_detached();
    printf("all ok\n");
    return 0;
}
