/*
 * A kernel-layer or engine bug ends only the guest process that hits it (the engine map, design
 * doc §12 item 5). Bugs are simulated with the test hooks of debug builds: tuxport-run must set
 * TUXPORT_TEST_HOOKS=1 and TUXPORT_ON_BUG=kill. Each kind of bug is triggered in a child process;
 * the parent checks that the child was killed by SIGSEGV and keeps running itself. Once the exit
 * status of the whole process is settled, a bug during exit cleanup no longer overrides it.
 *
 * With an argument it only plants a bug for the host's calls into Machine methods (for the Machine
 * tests): host-call-then-exit exits after planting it, host-call-then-read reads stdin after
 * planting it, and thread-start-fails-then-exit instead makes the next host thread for a guest
 * thread fail to be created, so the host's next spawn fails.
 */
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define TEST_SYSCALL 0xa991
#define HOOK_PANIC 1           /* kernel-layer panic */
#define HOOK_BREAK_PAGE 2      /* make a page in the page table point at inaccessible host memory */
#define HOOK_PANIC_ON_EXIT 3   /* panic when the calling thread exits */
#define HOOK_PANIC_ON_START 4  /* the next thread of this process to start running panics */
#define HOOK_PANIC_IN_TIMERS 5 /* panic when the timer thread handles this process */
#define HOOK_PANIC_IN_HOST 6   /* panic the next time the host calls a Machine method */
#define HOOK_PANIC_UNLOCKED 8  /* panic with the BKL released: unwinding must retake it first */
#define HOOK_PANIC_FAST 9      /* the next no-BKL system call panics */
#define HOOK_PANIC_IN_FUTEX 10 /* panic on the next futex wait, after registering as a waiter */
#define HOOK_THREAD_START_FAILS 13 /* the next host thread for a guest thread cannot be created */

#define FUTEX_WAIT 0
#define FUTEX_WAKE 1

static void fail(const char *what)
{
    printf("FAIL: %s\n", what);
    exit(1);
}

static void expect_segv(pid_t pid, const char *what)
{
    int status;
    if (waitpid(pid, &status, 0) != pid)
        fail(what);
    if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGSEGV) {
        printf("%s: status %#x\n", what, status);
        fail(what);
    }
    printf("%s: killed by SIGSEGV\n", what);
}

/* Simulates an engine bug on the page p is on, then accesses it */
static void touch_broken_page(void)
{
    volatile long *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED)
        _exit(2);
    *p = 1;
    if (syscall(TEST_SYSCALL, HOOK_BREAK_PAGE, p) != 0)
        _exit(3);
    *p = 2;
    _exit(4);
}

static int pipefd[2];

static void *blocked_reader(void *arg)
{
    char c;
    (void)arg;
    if (read(pipefd[0], &c, 1) != 0)
        _exit(5);
    _exit(6);
}

static void *worker(void *arg)
{
    return arg;
}

static void *exit_with_bug(void *arg)
{
    if (syscall(TEST_SYSCALL, HOOK_PANIC_ON_EXIT) != 0)
        _exit(10);
    return arg;
}

static volatile sig_atomic_t alarmed;

static void on_alarm(int sig)
{
    (void)sig;
    alarmed = 1;
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IONBF, 0);

    if (argc > 1) {
        if (strcmp(argv[1], "thread-start-fails-then-exit") == 0)
            return syscall(TEST_SYSCALL, HOOK_THREAD_START_FAILS) == 0 ? 0 : 2;
        if (syscall(TEST_SYSCALL, HOOK_PANIC_IN_HOST) != 0)
            return 2;
        if (strcmp(argv[1], "host-call-then-read") == 0) {
            char c;
            read(0, &c, 1);
        }
        return 0;
    }

    /*
     * The kernel layer counts the paths it does not implement, and the reports of the bugs below
     * carry them (crate::stub): a prctl option no Linux defines, asked twice
     */
    for (int i = 0; i < 2; i++) {
        if (syscall(SYS_prctl, 0x7fff0000, 0, 0, 0, 0) != -1 || errno != EINVAL)
            fail("an unknown prctl option is EINVAL");
    }

    pid_t pid = fork();
    if (pid == 0) {
        syscall(TEST_SYSCALL, HOOK_PANIC);
        _exit(7);
    }
    expect_segv(pid, "kernel panic");

    pid = fork();
    if (pid == 0) {
        syscall(TEST_SYSCALL, HOOK_PANIC_UNLOCKED);
        _exit(7);
    }
    expect_segv(pid, "kernel panic without the BKL");

    /* clock_gettime is done without the BKL (syscall::fast) */
    pid = fork();
    if (pid == 0) {
        struct timespec ts;
        syscall(TEST_SYSCALL, HOOK_PANIC_FAST);
        syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &ts);
        _exit(7);
    }
    expect_segv(pid, "kernel panic in a syscall without the BKL");

    /*
     * A panic while waiting on a futex: the waiter must be removed from the queue, otherwise a
     * later FUTEX_WAKE would be counted against it and the real waiter could not be woken
     */
    int *word = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (word == MAP_FAILED)
        fail("mmap");
    pid = fork();
    if (pid == 0) {
        syscall(TEST_SYSCALL, HOOK_PANIC_IN_FUTEX);
        syscall(SYS_futex, word, FUTEX_WAIT, 0, NULL, NULL, 0);
        _exit(7);
    }
    expect_segv(pid, "kernel panic while waiting on a futex");
    pid = fork();
    if (pid == 0)
        _exit(syscall(SYS_futex, word, FUTEX_WAIT, 0, NULL, NULL, 0) == 0 ? 0 : 1);
    /* Waking once is enough: when it first returns 1, the one woken must be it */
    while (syscall(SYS_futex, word, FUTEX_WAKE, 1, NULL, NULL, 0) == 0)
        usleep(10000);
    int woken = 0, wst;
    for (int i = 0; i < 200 && !woken; i++) {
        usleep(10000);
        woken = waitpid(pid, &wst, WNOHANG) == pid;
    }
    if (!woken) {
        kill(pid, SIGKILL);
        waitpid(pid, &wst, 0);
        fail("the futex waiter after a panicked one is woken");
    }
    printf("futex waiter after a panicked one: woken\n");

    pid = fork();
    if (pid == 0)
        touch_broken_page();
    expect_segv(pid, "engine fault");

    /* Multithreaded: another thread is blocked in the kernel, and the whole process is still
     * killed */
    pid = fork();
    if (pid == 0) {
        pthread_t t;
        if (pipe(pipefd) != 0 || pthread_create(&t, NULL, blocked_reader, NULL) != 0)
            _exit(8);
        usleep(20000);
        touch_broken_page();
    }
    expect_segv(pid, "engine fault with another thread");

    /*
     * When the last thread enters exit, synchronize_group_exit has already settled the process's
     * exit code, and, as with exit_group, a later oops during exit cleanup no longer changes it.
     */
    pid = fork();
    if (pid == 0) {
        exit_with_bug(NULL);
        syscall(SYS_exit, 11);
    }
    int exit_status;
    if (waitpid(pid, &exit_status, 0) != pid || !WIFEXITED(exit_status) || WEXITSTATUS(exit_status) != 11)
        fail("panic while exiting preserves the group exit code");
    printf("panic while exiting: kept exit code 11\n");

    /*
     * A vfork child whose exit panics before it leaves the address space: the parent still goes
     * on (an alarm ends a parent left waiting)
     */
    pid = fork();
    if (pid == 0) {
        alarm(5);
        pid_t child = vfork();
        if (child == 0) {
            syscall(TEST_SYSCALL, HOOK_PANIC_ON_EXIT);
            _exit(0);
        }
        _exit(child > 0 ? 20 : 21);
    }
    if (waitpid(pid, &exit_status, 0) != pid || !WIFEXITED(exit_status) || WEXITSTATUS(exit_status) != 20)
        fail("a vfork parent goes on after its child's exit panicked");
    printf("vfork child's exit panicked: parent went on\n");

    /* Some threads have not exited yet: the fault still ends the whole process, and the parent
     * sees SIGSEGV. */
    pid = fork();
    if (pid == 0) {
        pthread_t t;
        if (pthread_create(&t, NULL, exit_with_bug, NULL) != 0)
            _exit(12);
        pthread_join(t, NULL);
        _exit(13);
    }
    expect_segv(pid, "panic while a thread exits");

    /* When a new thread starts running */
    pid = fork();
    if (pid == 0) {
        pthread_t t;
        if (syscall(TEST_SYSCALL, HOOK_PANIC_ON_START) != 0 ||
            pthread_create(&t, NULL, worker, NULL) != 0)
            _exit(14);
        pthread_join(t, NULL);
        _exit(15);
    }
    expect_segv(pid, "panic while a thread starts");

    /* When the timer thread handles this process */
    pid = fork();
    if (pid == 0) {
        if (syscall(TEST_SYSCALL, HOOK_PANIC_IN_TIMERS) != 0)
            _exit(16);
        for (;;)
            pause();
    }
    expect_segv(pid, "panic in the timer thread");

    /* After hitting a bug, threads and fork work as usual */
    pthread_t t;
    void *r;
    if (pthread_create(&t, NULL, worker, (void *)42) != 0 || pthread_join(t, &r) != 0 || r != (void *)42)
        fail("threads after the bugs");
    pid = fork();
    if (pid == 0)
        _exit(9);
    int status;
    if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) || WEXITSTATUS(status) != 9)
        fail("fork after the bugs");
    /* The timer thread is still there */
    sigset_t block, old;
    sigemptyset(&block);
    sigaddset(&block, SIGALRM);
    sigprocmask(SIG_BLOCK, &block, &old);
    signal(SIGALRM, on_alarm);
    ualarm(20000, 0);
    while (!alarmed)
        sigsuspend(&old);
    printf("timers after the bugs: ok\n");
    printf("all ok\n");
    return 0;
}
