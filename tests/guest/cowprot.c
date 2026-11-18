/*
 * mprotect over copy-on-write pages must never make them writable in place, not even for a
 * moment (the kernel/mm map): after fork, one process (the owner in even rounds, the borrower in
 * odd ones) keeps giving the shared pages read-write permission again while another of its threads
 * wakes from short sleeps and writes them. A sleeping thread is not kicked by a shootdown, so it
 * can run guest code during the mprotect; its writes must be copied, never seen by the other
 * process, which checks the pages all along. Prints "done" when no round saw a write.
 *
 * Arguments: rounds and milliseconds per round (default 400 and 5; the code before the fix failed
 * every run of 400 at both tiers).
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define PAGES 64
#define PAGE 4096

static volatile unsigned long *region;
static atomic_int stop;

static unsigned long expected(int page, int round)
{
    return (unsigned long)round << 16 | (unsigned long)page;
}

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

/* Sleeps briefly, then writes a page, round robin */
static void *writer(void *arg)
{
    struct timespec t = {0, 10000};
    for (int k = 0; !atomic_load(&stop); k = (k + 1) % PAGES) {
        nanosleep(&t, NULL);
        region[k * (PAGE / sizeof *region)] = ~0UL;
    }
    return arg;
}

/* Gives the pages read-write permission again and again while writer runs */
static void mutate(double ms)
{
    pthread_t w;
    atomic_store(&stop, 0);
    if (pthread_create(&w, NULL, writer, NULL) != 0) {
        perror("pthread_create");
        exit(2);
    }
    double end = now() + ms / 1000;
    while (now() < end) {
        if (mprotect((void *)region, PAGES * PAGE, PROT_READ | PROT_WRITE) != 0) {
            perror("mprotect");
            exit(2);
        }
    }
    atomic_store(&stop, 1);
    pthread_join(w, NULL);
}

/* The first page whose contents changed, or -1 */
static int changed(int round)
{
    for (int p = 0; p < PAGES; p++) {
        if (region[p * (PAGE / sizeof *region)] != expected(p, round))
            return p;
    }
    return -1;
}

/* Checks the pages until the other side closes the pipe */
static int check(int done, int round)
{
    char c;
    for (;;) {
        int p = changed(round);
        if (p >= 0)
            return p;
        if (read(done, &c, 1) == 0)
            return changed(round);
    }
}

int main(int argc, char **argv)
{
    int rounds = argc > 1 ? atoi(argv[1]) : 400;
    double ms = argc > 2 ? atof(argv[2]) : 5;
    region = mmap(NULL, PAGES * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (region == MAP_FAILED) {
        perror("mmap");
        return 2;
    }
    for (int round = 0; round < rounds; round++) {
        int owner = round % 2 == 0;
        for (int p = 0; p < PAGES; p++)
            region[p * (PAGE / sizeof *region)] = expected(p, round);
        int fds[2];
        if (pipe(fds) != 0) {
            perror("pipe");
            return 2;
        }
        pid_t pid = fork();
        if (pid < 0) {
            perror("fork");
            return 2;
        }
        if (pid == 0) {
            if (owner) {
                /* The parent mutates; the borrower checks */
                close(fds[1]);
                fcntl(fds[0], F_SETFL, O_NONBLOCK);
                int p = check(fds[0], round);
                if (p >= 0)
                    printf("round %d: the borrower saw the owner's write to page %d\n", round, p);
                _exit(p >= 0);
            }
            close(fds[0]);
            mutate(ms);
            _exit(0);
        }
        int bad = -1;
        if (owner) {
            close(fds[0]);
            mutate(ms);
            close(fds[1]);
        } else {
            close(fds[1]);
            fcntl(fds[0], F_SETFL, O_NONBLOCK);
            bad = check(fds[0], round);
            close(fds[0]);
            if (bad >= 0)
                printf("round %d: the owner saw the borrower's write to page %d\n", round, bad);
        }
        int status;
        if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) || WEXITSTATUS(status) != 0 ||
            bad >= 0) {
            fflush(stdout);
            return 1;
        }
    }
    printf("done\n");
    return 0;
}
