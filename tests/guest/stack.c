/*
 * The main thread's stack grows downwards on demand, its size limited by the current value of
 * RLIMIT_STACK (the kernel/mm map). When the kernel accesses stack that has not grown yet (syscall
 * buffers, signal frames) it must grow it too, as on Linux.
 */
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define MB (1L << 20)

static void fail(const char *what)
{
    printf("FAIL: %s (errno %d)\n", what, errno);
    exit(1);
}

/* Each level takes 1 KB of stack, using about `bytes` bytes in all */
__attribute__((noinline)) static int recurse(long bytes)
{
    volatile char buf[1024];
    buf[0] = 1;
    if (bytes <= (long)sizeof buf)
        return buf[0];
    return recurse(bytes - sizeof buf) + buf[0];
}

/*
 * Moves SP down by off bytes (without touching those pages) and runs read(fd, sp, len) there; *at
 * is where it read to. Returns the result of read (-errno on failure).
 */
static long read_below_sp(long off, int fd, long len, char **at)
{
    register long x0 __asm__("x0") = fd;
    register long x2 __asm__("x2") = len;
    register long x8 __asm__("x8") = SYS_read;
    char *where;
    __asm__ volatile("mov x9, sp\n\t"
                     "sub x1, x9, %[off]\n\t"
                     "and x1, x1, #~15\n\t"
                     "mov sp, x1\n\t"
                     "svc #0\n\t"
                     "mov sp, x9\n\t"
                     "mov %[where], x1\n\t"
                     : "+r"(x0), [where] "=&r"(where)
                     : [off] "r"(off), "r"(x2), "r"(x8)
                     : "x1", "x9", "memory");
    *at = where;
    return x0;
}

/* Moves SP down by off bytes (without touching those pages) and sends itself signal sig there: the
 * signal frame lands below that */
static long kill_below_sp(long off, int sig)
{
    register long x0 __asm__("x0") = getpid();
    register long x1 __asm__("x1") = sig;
    register long x8 __asm__("x8") = SYS_kill;
    __asm__ volatile("mov x9, sp\n\t"
                     "sub x10, x9, %[off]\n\t"
                     "and sp, x10, #~15\n\t"
                     "svc #0\n\t"
                     "mov sp, x9\n\t"
                     : "+r"(x0)
                     : [off] "r"(off), "r"(x1), "r"(x8)
                     : "x9", "x10", "memory");
    return x0;
}

static volatile sig_atomic_t handled;

static void on_usr1(int sig)
{
    /* The handler itself uses stack there too */
    volatile char buf[8192];
    buf[0] = (char)sig;
    buf[sizeof buf - 1] = 1;
    handled = buf[0] == SIGUSR1 && buf[sizeof buf - 1] == 1;
}

/* Calls f in a child process and returns its exit status */
static int in_child(int (*f)(long), long arg)
{
    pid_t pid = fork();
    if (pid < 0)
        fail("fork");
    if (pid == 0)
        _exit(f(arg));
    int status;
    if (waitpid(pid, &status, 0) != pid)
        fail("waitpid");
    return status;
}

static int deep(long bytes)
{
    return recurse(bytes) > 0 ? 0 : 1;
}

/* First sets RLIMIT_STACK's soft limit to the low 32 bits of arg (MB), then recurses the high 32
 * bits (MB) */
static int deep_with_limit(long arg)
{
    struct rlimit rl;
    getrlimit(RLIMIT_STACK, &rl);
    rl.rlim_cur = (arg & 0xffffffff) * MB;
    if (setrlimit(RLIMIT_STACK, &rl) != 0)
        return 2;
    return deep((arg >> 32) * MB);
}

static long sized(long limit_mb, long depth_mb)
{
    return depth_mb << 32 | limit_mb;
}

static void expect_exit(int status, const char *what)
{
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        printf("FAIL: %s (status %#x)\n", what, status);
        exit(1);
    }
}

static void expect_segv(int status, const char *what)
{
    if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGSEGV) {
        printf("FAIL: %s (status %#x)\n", what, status);
        exit(1);
    }
}

int main(void)
{
    struct rlimit rl;
    if (getrlimit(RLIMIT_STACK, &rl) != 0 || rl.rlim_cur != 8 * MB)
        fail("default RLIMIT_STACK");

    /* The kernel writes into a buffer below the stack bottom and above SP: the stack grows first,
     * not EFAULT, as on Linux */
    int p[2];
    if (pipe(p) != 0 || write(p[1], "kernelok", 8) != 8)
        fail("pipe");
    char *at;
    long n = read_below_sp(1 * MB, p[0], 8, &at);
    if (n != 8 || memcmp(at, "kernelok", 8) != 0) {
        printf("FAIL: read below the stack: %ld\n", n);
        return 1;
    }
    printf("read below the stack ok\n");

    /* The signal frame lands below the stack bottom */
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    if (sigaction(SIGUSR1, &sa, NULL) != 0)
        fail("sigaction");
    if (kill_below_sp(2 * MB, SIGUSR1) != 0 || !handled)
        fail("signal frame below the stack");
    printf("signal frame below the stack ok\n");

    /* Beyond the limit: EFAULT */
    if (write(p[1], "kernelok", 8) != 8)
        fail("pipe");
    n = read_below_sp(16 * MB, p[0], 8, &at);
    if (n != -EFAULT) {
        printf("FAIL: read past the limit: %ld\n", n);
        return 1;
    }
    printf("read past the limit is EFAULT\n");

    /* Growth on a page fault: fine within the default 8 MB limit, SIGSEGV beyond it */
    if (deep(4 * MB) != 0)
        fail("4 MB of stack");
    expect_segv(in_child(deep, 16 * MB), "16 MB of stack under the default limit");
    printf("default limit ok\n");

    /* Raising the limit while running takes effect at once, and so does lowering it */
    expect_exit(in_child(deep_with_limit, sized(64, 32)), "32 MB of stack under 64 MB");
    expect_segv(in_child(deep_with_limit, sized(16, 24)), "24 MB of stack under 16 MB");
    printf("setrlimit ok\n");

    printf("all ok\n");
    return 0;
}
