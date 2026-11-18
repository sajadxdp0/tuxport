/*
 * TIOCSTI fakes terminal input only on the caller's controlling terminal, or with CAP_SYS_ADMIN
 * (Linux 6.1's tiocsti); on a master the byte goes to what the master reads. The differential
 * test pty compares only the privileged cases: newer distribution kernels turn
 * dev.tty.legacy_tiocsti off, refusing everyone else. Run in the store as root (it needs
 * /dev/ptmx). Each check that passes prints "ok <name>"; a failure prints the reason and exits
 * with 1; when all pass it prints "all ok" at the end.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            printf("FAIL: ");                                                                      \
            printf(__VA_ARGS__);                                                                   \
            printf(" (errno %d)\n", errno);                                                        \
            exit(1);                                                                               \
        }                                                                                          \
    } while (0)

static void ok(const char *name)
{
    printf("ok %s\n", name);
    fflush(stdout);
}

/* TIOCSTI of each byte of `s`: 0, or the errno of the first failure */
static int sti(int fd, const char *s)
{
    for (; *s; s++)
        if (ioctl(fd, TIOCSTI, s) != 0)
            return errno;
    return 0;
}

/* Everything readable from `fd` now (nonblocking), as a string */
static const char *drain(int fd)
{
    static char buf[256];
    size_t n = 0;
    fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK);
    for (ssize_t r; n < sizeof buf - 1 && (r = read(fd, buf + n, sizeof buf - 1 - n)) > 0;)
        n += r;
    buf[n] = 0;
    return buf;
}

int main(void)
{
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    CHECK(m >= 0 && grantpt(m) == 0 && unlockpt(m) == 0, "posix_openpt");
    int s = open(ptsname(m), O_RDWR | O_NOCTTY);
    CHECK(s >= 0, "open the slave");

    pid_t pid = fork();
    CHECK(pid >= 0, "fork");
    if (pid == 0) {
        CHECK(setgid(65534) == 0 && setuid(65534) == 0, "drop privileges");
        CHECK(sti(s, "x") == EPERM, "a terminal that is not the controlling one");
        ok("unprivileged, another terminal: EPERM");
        CHECK(sti(m, "x") == EPERM, "a master");
        ok("unprivileged, a master: EPERM");
        CHECK(setsid() > 0 && ioctl(s, TIOCSCTTY, 0) == 0, "take the controlling terminal");
        CHECK(sti(m, "x") == EPERM, "the master of the controlling terminal");
        ok("unprivileged, the master of its controlling terminal: EPERM");
        CHECK(sti(s, "a\n") == 0, "the controlling terminal");
        ok("unprivileged, its controlling terminal");
        exit(0);
    }
    int st;
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) && WEXITSTATUS(st) == 0, "the child");

    CHECK(sti(s, "r\n") == 0, "root on a terminal that is not its controlling one");
    ok("CAP_SYS_ADMIN, another terminal");
    CHECK(sti(m, "z") == 0, "root on a master");
    ok("CAP_SYS_ADMIN, a master");
    const char *got = drain(s);
    CHECK(strcmp(got, "a\nr\n") == 0, "the slave reads the two lines, got \"%s\"", got);
    ok("the slave reads only what was typed into it");
    got = drain(m);
    CHECK(strcmp(got, "a\r\nr\r\nz") == 0, "the master reads the echoes and its byte, got \"%s\"",
          got);
    ok("the master reads the echoes, then its own byte");
    printf("all ok\n");
    return 0;
}
