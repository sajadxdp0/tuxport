/*
 * Guest test program in which threads start and exit constantly: 4 processes, each with 4 threads
 * that keep creating and joining short-lived threads, running for argv[1] seconds (default 5). When
 * an exiting thread drops an Rc weak reference outside the BKL, it changes the same count as
 * another thread's Rc::downgrade on start, and within a few seconds the host heap is corrupted
 * (docs/rounds/71-thread-exit-race.md). Build with musl-gcc -static and run directly in
 * tuxport-run. When all is well it prints "all ok" at the end.
 */
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

static atomic_int stop;

static void *leaf(void *arg)
{
    return arg;
}

static void *spawner(void *arg)
{
    long n = 0;
    (void)arg;
    while (!atomic_load(&stop)) {
        pthread_t t;
        if (pthread_create(&t, NULL, leaf, NULL) == 0) {
            pthread_join(t, NULL);
            n++;
        }
    }
    return (void *)n;
}

int main(int argc, char **argv)
{
    int seconds = argc > 1 ? atoi(argv[1]) : 5;
    for (int p = 0; p < 4; p++) {
        pid_t pid = fork();
        if (pid < 0) {
            perror("fork");
            return 1;
        }
        if (pid == 0) {
            pthread_t t[4];
            long total = 0;
            for (int i = 0; i < 4; i++)
                pthread_create(&t[i], NULL, spawner, NULL);
            sleep(seconds);
            atomic_store(&stop, 1);
            for (int i = 0; i < 4; i++) {
                void *r;
                pthread_join(t[i], &r);
                total += (long)r;
            }
            _exit(total > 0 ? 0 : 1);
        }
    }
    int status, failed = 0;
    while (wait(&status) > 0)
        failed += !(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    if (failed) {
        printf("%d processes failed\n", failed);
        return 1;
    }
    printf("all ok\n");
    return 0;
}
