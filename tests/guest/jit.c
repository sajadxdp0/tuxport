/*
 * Guest test program for self-modifying code (M7): writes instructions into memory and runs them
 * as a JIT does, checking that after rewriting and running ic ivau Tier 1 does not keep running the
 * old translation. Build with musl-gcc -static and run directly in tuxport-run. Each check that
 * passes prints a line "ok <name>"; a failure prints the reason and exits with 1; when all pass it
 * prints "all ok" at the end.
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

#define PAGE 4096

static void ok(const char *name)
{
    printf("ok %s\n", name);
    fflush(stdout);
}

typedef int (*fn_t)(void);

/* w0 = n * k: a loop with a back edge, translated into several blocks linked to each other */
static void emit_loop(uint32_t *code, unsigned n, unsigned k)
{
    code[0] = 0x52800000u;                       /* mov w0, #0 */
    code[1] = 0x52800001u | n << 5;              /* mov w1, #n */
    code[2] = 0x11000000u | k << 10;             /* add w0, w0, #k */
    code[3] = 0x71000421u;                       /* subs w1, w1, #1 */
    code[4] = 0x54000001u | (0x7fffeu << 5);     /* b.ne 2 */
    code[5] = 0xd65f03c0u;                       /* ret */
}

static void publish(uint32_t *code, unsigned words)
{
    __builtin___clear_cache((char *)code, (char *)(code + words));
}

/* ---- CTR_EL0: ic ivau must be run after modifying code ---- */

static void test_ctr(void)
{
    uint64_t ctr;
    __asm__ volatile("mrs %0, ctr_el0" : "=r"(ctr));
    CHECK(!(ctr >> 29 & 1), "CTR_EL0.DIC = 1: %#lx", (unsigned long)ctr);
    CHECK((ctr & 15) >= 2 && (ctr >> 16 & 15) >= 2, "cache lines too small: %#lx", (unsigned long)ctr);
    ok("ctr_el0");
}

/* ---- The same page rewritten again and again: the new code runs every time ---- */

static void test_rewrite(void)
{
    uint32_t *code = mmap(NULL, PAGE, PROT_READ | PROT_WRITE | PROT_EXEC,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(code != MAP_FAILED, "mmap rwx");
    fn_t f = (fn_t)(uintptr_t)code;
    for (unsigned k = 1; k <= 200; k++) {
        emit_loop(code, 50, k);
        publish(code, 6);
        for (int i = 0; i < 20; i++)
            CHECK(f() == (int)(50 * k), "k = %u: got %d", k, f());
    }
    /* Rewrite only one instruction in the loop body */
    emit_loop(code, 10, 3);
    publish(code, 6);
    CHECK(f() == 30, "before patch: %d", f());
    code[2] = 0x11000000u | 7 << 10;
    publish(code + 2, 1);
    CHECK(f() == 70, "after patch: %d", f());
    munmap(code, PAGE);
    ok("rewrite");
}

/* ---- W^X: switching between writable and executable with mprotect ---- */

static void test_wx_mprotect(void)
{
    uint32_t *code = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(code != MAP_FAILED, "mmap rw");
    fn_t f = (fn_t)(uintptr_t)code;
    for (unsigned k = 1; k <= 50; k++) {
        CHECK(mprotect(code, PAGE, PROT_READ | PROT_WRITE) == 0, "mprotect rw");
        emit_loop(code, 7, k);
        CHECK(mprotect(code, PAGE, PROT_READ | PROT_EXEC) == 0, "mprotect rx");
        publish(code, 6);
        CHECK(f() == (int)(7 * k), "k = %u: got %d", k, f());
    }
    munmap(code, PAGE);
    ok("wx_mprotect");
}

/* ---- Dual mapping: one memfd mapped twice, writable and executable, rewritten in the writable
 * one ---- */

static void test_dual_mapping(void)
{
    int fd = memfd_create("jit", 0);
    if (fd < 0 && errno == ENOENT) { /* no filesystem when run without a rootfs */
        printf("skip dual_mapping\n");
        return;
    }
    CHECK(fd >= 0, "memfd_create");
    CHECK(ftruncate(fd, PAGE) == 0, "ftruncate");
    uint32_t *rw = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    uint32_t *rx = mmap(NULL, PAGE, PROT_READ | PROT_EXEC, MAP_SHARED, fd, 0);
    CHECK(rw != MAP_FAILED && rx != MAP_FAILED && rw != rx, "mmap views");
    fn_t f = (fn_t)(uintptr_t)rx;
    for (unsigned k = 1; k <= 50; k++) {
        emit_loop(rw, 9, k);
        publish(rx, 6);
        CHECK(f() == (int)(9 * k), "k = %u: got %d", k, f());
    }
    munmap(rw, PAGE);
    munmap(rx, PAGE);
    close(fd);
    ok("dual_mapping");
}

/* ---- Code that another thread has run is rewritten ---- */

/*
 * Two slots are used in turn: round k rewrites slot k % 2 while the runner calls only the other
 * slot, so it never runs instructions while they are being rewritten; but it ran this slot in
 * round k - 2, and Tier 1 still holds the old translation.
 */
static uint32_t *shared_code;
static atomic_int phase, done;

static fn_t slot(int k)
{
    return (fn_t)(uintptr_t)(shared_code + (k % 2) * 64);
}

static void *runner(void *arg)
{
    (void)arg;
    for (int k = 1; k <= 20; k++) {
        while (atomic_load(&phase) < k)
            CHECK(slot(k - 1)() == 5 * (k - 1), "round %d: old slot", k);
        /* The writer advances phase only after publishing the new code: seeing k means the new
         * code must run */
        int r = slot(k)();
        CHECK(r == 5 * k, "round %d: got %d", k, r);
        atomic_store(&done, k);
    }
    return NULL;
}

static void test_cross_thread(void)
{
    shared_code = mmap(NULL, PAGE, PROT_READ | PROT_WRITE | PROT_EXEC,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(shared_code != MAP_FAILED, "mmap rwx");
    emit_loop((uint32_t *)(uintptr_t)slot(0), 5, 0);
    publish(shared_code, 128);
    pthread_t t;
    CHECK(pthread_create(&t, NULL, runner, NULL) == 0, "pthread_create");
    for (int k = 1; k <= 20; k++) {
        while (atomic_load(&done) < k - 1)
            usleep(100);
        usleep(1000);
        uint32_t *code = (uint32_t *)(uintptr_t)slot(k);
        emit_loop(code, 5, (unsigned)k);
        publish(code, 6);
        atomic_store(&phase, k);
    }
    pthread_join(t, NULL);
    munmap(shared_code, PAGE);
    ok("cross_thread");
}

/* ---- Cache maintenance on an unmapped address: SIGSEGV ---- */

static sigjmp_buf jump;
static void *fault_addr;

static void on_segv(int sig, siginfo_t *info, void *uc)
{
    (void)sig;
    (void)uc;
    fault_addr = info->si_addr;
    siglongjmp(jump, 1);
}

static void test_unmapped(void)
{
    struct sigaction sa = {0};
    sa.sa_sigaction = on_segv;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    CHECK(sigaction(SIGSEGV, &sa, NULL) == 0, "sigaction");
    char *p = mmap(NULL, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(p != MAP_FAILED, "mmap");
    munmap(p, PAGE);
    if (sigsetjmp(jump, 1) == 0) {
        __asm__ volatile("ic ivau, %0" ::"r"(p + 64) : "memory");
        CHECK(0, "ic ivau on an unmapped page did not fault");
    }
    CHECK(fault_addr == p + 64, "ic ivau fault address %p, expected %p", fault_addr, p + 64);
    if (sigsetjmp(jump, 1) == 0) {
        __asm__ volatile("dc cvau, %0" ::"r"(p + 128) : "memory");
        CHECK(0, "dc cvau on an unmapped page did not fault");
    }
    CHECK(fault_addr == p + 128, "dc cvau fault address %p, expected %p", fault_addr, p + 128);
    signal(SIGSEGV, SIG_DFL);
    ok("unmapped");
}

int main(void)
{
    test_ctr();
    test_rewrite();
    test_wx_mprotect();
    test_dual_mapping();
    test_cross_thread();
    test_unmapped();
    printf("all ok\n");
    return 0;
}
