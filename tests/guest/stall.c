/*
 * The BKL watchdog (the kernel/process map): a hold of the BKL longer than the watchdog's
 * threshold is reported on the host's stderr while it lasts and in the kernel log once it has
 * ended. The hold is simulated with the test hooks of debug builds: tuxport-run must set
 * TUXPORT_TEST_HOOKS=1 and a threshold of 1 s (TUXPORT_BKL_WATCHDOG=1). Holds the BKL for 2.5 s,
 * then reads the kernel log until the watchdog's record appears and prints it.
 *
 * With the argument "hold" it only holds the BKL (for the Machine tests). With "twice" a second
 * thread, already waiting for the BKL, holds it for 2.5 s more right after the first hold: the
 * watchdog writes the first hold's record only once it gets the BKL, and must report the second
 * hold meanwhile; then both records are printed.
 */
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/klog.h>
#include <sys/syscall.h>
#include <unistd.h>

#define TEST_SYSCALL 0xa991
#define HOOK_HOLD_BKL 12 /* hold the BKL for the given milliseconds */
#define SYSLOG_ACTION_READ_ALL 3

static char log_buf[1 << 17];

static int ready;

static void *hold_next(void *arg)
{
    /* The first syscalls of a thread take the BKL (they set up its fast path); after them its
     * sleep does not, so the hold below is what waits for the main thread's */
    usleep(1000);
    usleep(1000);
    __atomic_store_n(&ready, 1, __ATOMIC_SEQ_CST);
    usleep(300 * 1000);
    if (syscall(TEST_SYSCALL, HOOK_HOLD_BKL, 2500) != 0)
        perror("FAIL: holding the BKL again");
    return arg;
}

int main(int argc, char **argv)
{
    int twice = argc > 1 && strcmp(argv[1], "twice") == 0;
    pthread_t next;
    if (twice) {
        pthread_create(&next, NULL, hold_next, NULL);
        while (!__atomic_load_n(&ready, __ATOMIC_SEQ_CST))
            usleep(1000);
    }
    if (syscall(TEST_SYSCALL, HOOK_HOLD_BKL, 2500) != 0) {
        perror("FAIL: holding the BKL");
        return 1;
    }
    if (argc > 1 && strcmp(argv[1], "hold") == 0)
        return 0;
    if (twice)
        pthread_join(next, NULL);
    /* A record is written soon after the watchdog's first look after its hold ended */
    int want = twice ? 2 : 1;
    for (int i = 0; i < 50; i++) {
        int n = klogctl(SYSLOG_ACTION_READ_ALL, log_buf, sizeof log_buf - 1);
        if (n < 0) {
            perror("FAIL: klogctl");
            return 1;
        }
        log_buf[n] = 0;
        char *lines[2];
        int found = 0;
        for (char *at = log_buf; found < want && (at = strstr(at, " held the BKL for ")); at++) {
            char *start = at;
            while (start > log_buf && start[-1] != '\n')
                start--;
            lines[found++] = start;
        }
        if (found == want) {
            for (int j = 0; j < found; j++) {
                char *end = strchr(lines[j], '\n');
                printf("%.*s\n", end ? (int)(end - lines[j]) : (int)strlen(lines[j]), lines[j]);
            }
            printf("all ok\n");
            return 0;
        }
        usleep(100 * 1000);
    }
    printf("FAIL: no record of the hold in the kernel log\n");
    return 1;
}
