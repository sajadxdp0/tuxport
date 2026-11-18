/*
 * A shootdown must not wait for a thread that entered guest code after it looked (the kernel/mm
 * map): a parent takes back pages it lent to its child at fork, one write at a time, and each one
 * shoots down the child's address space while holding the BKL; meanwhile one of the child's threads
 * makes syscalls (so it often waits for the BKL) and others make one and then spin in guest code
 * until it moves on. A spinner that marked itself as running guest code just after the shootdown
 * looked was not kicked, and waiting for it deadlocked the kernel (LTP shmctl05 under load).
 * Prints "done" when every round finished.
 *
 * Arguments: rounds and pages per round (default 8 and 4096).
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define SPINNERS 4

static atomic_long counter;

/* Syscalls that take the BKL */
static void *busy(void *arg)
{
    for (;;) {
        syscall(SYS_getppid);
        atomic_fetch_add(&counter, 1);
    }
    return arg;
}

/* A syscall, then a spin in guest code until busy moves on */
static void *spinner(void *arg)
{
    for (;;) {
        syscall(SYS_getppid);
        long seen = atomic_load(&counter);
        while (atomic_load(&counter) == seen)
            ;
    }
    return arg;
}

static void round_of(long pages)
{
    char *mem = mmap(NULL, pages * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mem == MAP_FAILED) {
        perror("mmap");
        exit(1);
    }
    memset(mem, 1, pages * 4096);
    int ready[2];
    if (pipe(ready) != 0) {
        perror("pipe");
        exit(1);
    }
    pid_t pid = fork();
    if (pid == 0) {
        pthread_t t;
        pthread_create(&t, NULL, busy, NULL);
        for (int i = 0; i < SPINNERS; i++)
            pthread_create(&t, NULL, spinner, NULL);
        if (write(ready[1], "x", 1) != 1)
            _exit(1);
        pause();
        _exit(0);
    }
    char x;
    if (read(ready[0], &x, 1) != 1) {
        perror("read");
        exit(1);
    }
    /* Each write takes back a page lent to the child */
    for (long i = 0; i < pages; i++)
        mem[i * 4096] = 2;
    kill(pid, SIGKILL);
    waitpid(pid, NULL, 0);
    munmap(mem, pages * 4096);
    close(ready[0]);
    close(ready[1]);
}

int main(int argc, char **argv)
{
    int rounds = argc > 1 ? atoi(argv[1]) : 8;
    long pages = argc > 2 ? atol(argv[2]) : 4096;
    for (int r = 0; r < rounds; r++)
        round_of(pages);
    puts("done");
    return 0;
}
