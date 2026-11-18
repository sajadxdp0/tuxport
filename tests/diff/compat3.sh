# The sequel to compat2.sh (difftest passes the whole script as one argument of sh -c, which must
# not exceed 128 KB): MSG_WAITALL and MSG_PEEK on TCP; arguments at the edges of their types;
# taking a controlling terminal; kill with a thread id; memfd owners; linking unlinked files;
# recorded sysctls; process signals taken by another thread; blocking connect; an exited main
# thread; /proc/vmstat; RLIMIT_MSGQUEUE; sched_setattr flags; membarrier after fork;
# CLONE_PARENT exit signals; timers of exited threads; sendfile into O_APPEND files; thread
# names as bytes; RssAnon, RssFile and RssShmem; AF_UNIX reads of nothing and shutdown; robust
# mutexes on copy-on-write pages; when splice and tee wait; a pipe spliced out while another
# thread reads it
set -e
cd /tmp
# MSG_WAITALL on TCP waits for the whole length even when the data comes in pieces, and over 1 MiB;
# end of file, the receive timeout, a signal (even with SA_RESTART) and MSG_DONTWAIT end it with
# what has come. MSG_PEEK|MSG_WAITALL waits the same way and shows the head of the queue once,
# leaving it to be read
cat > waitall.c <<'C'
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

static int lfd;
static struct sockaddr_in addr;
static volatile int handled;

static void on_alarm(int sig)
{
    (void)sig;
    handled++;
}

static void fill(unsigned char *p, size_t n, size_t from)
{
    for (size_t i = 0; i < n; i++)
        p[i] = (unsigned char)((from + i) * 7 + 1);
}

static int check(const unsigned char *p, size_t n, size_t from)
{
    for (size_t i = 0; i < n; i++)
        if (p[i] != (unsigned char)((from + i) * 7 + 1))
            return 0;
    return 1;
}

static void send_all(int fd, size_t from, size_t n)
{
    static unsigned char b[65536];
    while (n > 0) {
        size_t k = n < sizeof b ? n : sizeof b;
        fill(b, k, from);
        ssize_t w = send(fd, b, k, 0);
        if (w <= 0)
            exit(3);
        from += w;
        n -= w;
    }
}

/* The child connects and sends `first` bytes, waits 100 ms, sends `second` more, then either
 * closes (close_after) or waits for a byte on the pipe before closing. Returns the accepted fd. */
static int start(size_t first, size_t second, int close_after, int *done, pid_t *child)
{
    int p[2];
    if (pipe(p) < 0)
        exit(4);
    pid_t pid = fork();
    if (pid == 0) {
        close(p[1]);
        int c = socket(AF_INET, SOCK_STREAM, 0);
        if (connect(c, (struct sockaddr *)&addr, sizeof addr) < 0)
            _exit(5);
        send_all(c, 0, first);
        usleep(100000);
        send_all(c, first, second);
        if (!close_after) {
            char x;
            read(p[0], &x, 1);
        }
        close(c);
        _exit(0);
    }
    close(p[0]);
    *done = p[1];
    *child = pid;
    return accept(lfd, NULL, NULL);
}

static void finish(int s, int done, pid_t child)
{
    write(done, "x", 1);
    close(done);
    close(s);
    waitpid(child, NULL, 0);
}

static void report(const char *what, ssize_t n, const unsigned char *b, size_t from)
{
    if (n < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %zd, data %s\n", what, n, check(b, n, from) ? "right" : "WRONG");
}

int main(void)
{
    static unsigned char b[3 << 20];
    setvbuf(stdout, NULL, _IOLBF, 0);
    /* finish() writes to children that may already have exited */
    signal(SIGPIPE, SIG_IGN);
    int done, s;
    pid_t child;
    ssize_t n;
    lfd = socket(AF_INET, SOCK_STREAM, 0);
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t len = sizeof addr;
    if (bind(lfd, (struct sockaddr *)&addr, sizeof addr) < 0 || listen(lfd, 8) < 0 ||
        getsockname(lfd, (struct sockaddr *)&addr, &len) < 0)
        return 1;

    s = start(10, 90, 0, &done, &child);
    n = recv(s, b, 100, MSG_WAITALL);
    report("waitall 10 then 90", n, b, 0);
    finish(s, done, child);

    s = start(10, 90, 0, &done, &child);
    n = recv(s, b, 100, MSG_PEEK | MSG_WAITALL);
    report("peek|waitall 10 then 90", n, b, 0);
    n = recv(s, b, 100, MSG_DONTWAIT);
    report("then read", n, b, 0);
    finish(s, done, child);

    s = start(10, 0, 1, &done, &child);
    n = recv(s, b, 100, MSG_WAITALL);
    report("waitall at end of file", n, b, 0);
    n = recv(s, b, 100, MSG_WAITALL);
    report("waitall after end of file", n, b, 0);
    finish(s, done, child);

    s = start(10, 0, 1, &done, &child);
    n = recv(s, b, 100, MSG_PEEK | MSG_WAITALL);
    report("peek|waitall at end of file", n, b, 0);
    n = recv(s, b, 100, 0);
    report("then read", n, b, 0);
    finish(s, done, child);

    s = start(10, 0, 0, &done, &child);
    struct timeval tv = {0, 300000};
    setsockopt(s, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    n = recv(s, b, 100, MSG_WAITALL);
    report("waitall with SO_RCVTIMEO", n, b, 0);
    n = recv(s, b, 100, MSG_WAITALL);
    report("waitall with SO_RCVTIMEO and nothing", n, b, 0);
    n = recv(s, b, 100, MSG_PEEK | MSG_WAITALL);
    report("peek|waitall with SO_RCVTIMEO and nothing", n, b, 0);
    finish(s, done, child);

    s = start(10, 0, 0, &done, &child);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alarm;
    sa.sa_flags = SA_RESTART;
    sigaction(SIGALRM, &sa, NULL);
    struct itimerval it = {{0, 0}, {0, 300000}};
    setitimer(ITIMER_REAL, &it, NULL);
    n = recv(s, b, 100, MSG_WAITALL);
    report("waitall cut by a SA_RESTART signal", n, b, 0);
    printf("handled: %d\n", handled);
    finish(s, done, child);

    s = start(10, 0, 0, &done, &child);
    usleep(50000);
    n = recv(s, b, 100, MSG_WAITALL | MSG_DONTWAIT);
    report("waitall|dontwait", n, b, 0);
    finish(s, done, child);

    s = start(1 << 20, 2 << 20, 0, &done, &child);
    n = recv(s, b, 3 << 20, MSG_WAITALL);
    report("waitall 3 MiB", n, b, 0);
    finish(s, done, child);

    s = start(1 << 20, 2 << 20, 0, &done, &child);
    struct iovec iov[3] = {{b, 1000}, {b + 1000, (3 << 20) - 2000}, {b + (3 << 20) - 1000, 1000}};
    struct msghdr m = {.msg_iov = iov, .msg_iovlen = 3};
    n = recvmsg(s, &m, MSG_WAITALL);
    report("recvmsg waitall 3 MiB over 3 iovecs", n, b, 0);
    finish(s, done, child);
    return 0;
}
C
gcc -O1 -o waitall waitall.c && ./waitall
rm -f waitall waitall.c

# Arguments at the edges of their types, as Linux takes them: mlock and munlock page-align the
# length with wrapping arithmetic (an empty range succeeds, one that wraps is EINVAL);
# F_SETOWN(INT_MIN) is EINVAL; record locks may end at OFFSET_MAX but not past it, and may not
# start below 0 (F_GETLK reports a lock to OFFSET_MAX with l_len 0); a negative iov_len is EINVAL,
# one past the address space EFAULT, and huge ones are cut to MAX_RW_COUNT; semtimedop's nsops is
# an unsigned int
cat > edges.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/sem.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void show(const char *what, long r)
{
    if (r < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %ld\n", what, r);
}

static int fd;

static long lock(short type, short whence, int64_t start, int64_t len)
{
    struct flock l = {.l_type = type, .l_whence = whence, .l_start = start, .l_len = len};
    return fcntl(fd, F_SETLK, &l);
}

/* What another process sees: F_GETLK over the whole file */
static void probe(const char *what)
{
    pid_t pid = fork();
    if (pid == 0) {
        struct flock l = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0};
        if (fcntl(fd, F_GETLK, &l) < 0)
            printf("%s: %s\n", what, strerror(errno));
        else if (l.l_type == F_UNLCK)
            printf("%s: unlocked\n", what);
        else
            printf("%s: start %lld len %lld\n", what, (long long)l.l_start, (long long)l.l_len);
        fflush(stdout);
        _exit(0);
    }
    waitpid(pid, NULL, 0);
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    char *p = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    show("mlock len SIZE_MAX", mlock(p, SIZE_MAX));
    /* Without CAP_IPC_LOCK (docker's root has none) RLIMIT_MEMLOCK is checked first */
    pid_t pid = fork();
    if (pid == 0) {
        /* capget/capset's version 3 header and data (CAP_IPC_LOCK is bit 14) */
        struct { uint32_t version; int pid; } h = {0x20080522, 0};
        struct { uint32_t effective, permitted, inheritable; } d[2];
        syscall(SYS_capget, &h, d);
        d[0].effective &= ~(1u << 14);
        syscall(SYS_capset, &h, d);
        show("mlock len that wraps, no CAP_IPC_LOCK", mlock(p + 4096, SIZE_MAX - 8191));
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    show("munlock len that wraps", munlock(p + 4096, SIZE_MAX - 8191));
    show("mlock 0 bytes inside a page", mlock(p + 1, 0));
    show("mlock 2 pages", mlock(p + 100, 4096));
    show("munlock 2 pages", munlock(p + 100, 4096));

    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    show("F_SETOWN INT_MIN", fcntl(sv[0], F_SETOWN, INT32_MIN));
    show("F_SETOWN self", fcntl(sv[0], F_SETOWN, getpid()));
    printf("F_GETOWN is self: %d\n", fcntl(sv[0], F_GETOWN) == getpid());
    show("F_SETOWN own group", fcntl(sv[0], F_SETOWN, -getpgrp()));
    printf("F_GETOWN is -group: %d\n", fcntl(sv[0], F_GETOWN) == -getpgrp());

    char b1[8] = {0}, b2[8] = {0};
    write(sv[1], "hello", 5);
    struct iovec bad[2] = {{b1, 2}, {b2, (size_t)1 << 63}};
    struct msghdr m = {.msg_iov = bad, .msg_iovlen = 2};
    show("recvmsg with a negative iov_len", recvmsg(sv[0], &m, MSG_DONTWAIT));
    struct iovec past[2] = {{b1, 2}, {b2, (size_t)1 << 48}};
    m.msg_iov = past;
    show("recvmsg with an iovec past the address space", recvmsg(sv[0], &m, MSG_DONTWAIT));
    /* Huge lengths that stay inside the address space: low addresses */
    char *low = mmap((void *)0x10000000, 4096, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    struct iovec big[3] = {{low, 2}, {low + 8, (size_t)1 << 46}, {low + 8, (size_t)1 << 46}};
    m.msg_iov = big;
    m.msg_iovlen = 3;
    long n = recvmsg(sv[0], &m, MSG_DONTWAIT);
    printf("recvmsg with huge iov_lens: %ld %.2s %.3s\n", n, low, low + 8);

    fd = open("edges.lock", O_RDWR | O_CREAT | O_TRUNC, 0600);
    write(fd, "0123456789", 10);
    show("lock to OFFSET_MAX - 1", lock(F_WRLCK, SEEK_SET, 0, INT64_MAX));
    probe(" seen");
    show("lock to OFFSET_MAX", lock(F_WRLCK, SEEK_SET, 1, INT64_MAX));
    probe(" seen");
    show("lock past OFFSET_MAX", lock(F_WRLCK, SEEK_SET, 2, INT64_MAX));
    lock(F_UNLCK, SEEK_SET, 0, 0);
    show("lock INT64_MIN, INT64_MIN", lock(F_WRLCK, SEEK_SET, INT64_MIN, INT64_MIN));
    show("lock from the end past OFFSET_MAX", lock(F_WRLCK, SEEK_END, INT64_MAX, 1));
    show("lock from the end, negative", lock(F_WRLCK, SEEK_END, -11, 1));
    show("lock 10, -5", lock(F_WRLCK, SEEK_SET, 10, -5));
    probe(" seen");
    lock(F_UNLCK, SEEK_SET, 0, 0);
    show("lock 3, -5", lock(F_WRLCK, SEEK_SET, 3, -5));
    show("lock 5, INT64_MIN", lock(F_WRLCK, SEEK_SET, 5, INT64_MIN));
    unlink("edges.lock");

    int id = semget(IPC_PRIVATE, 1, 0600);
    struct sembuf op = {0, 1, 0};
    show("semop with nsops 2^32 + 1", syscall(SYS_semop, id, &op, (1UL << 32) + 1));
    printf("value: %d\n", semctl(id, 0, GETVAL));
    show("semop with nsops 2^32", syscall(SYS_semop, id, &op, 1UL << 32));
    semctl(id, 0, IPC_RMID);
    return 0;
}
C
gcc -O1 -o edges edges.c && ./edges
rm -f edges edges.c

# Taking a controlling terminal: TIOCSCTTY takes one from another session only with argument 1
# and CAP_SYS_ADMIN, and needs read access to the fd without CAP_SYS_ADMIN; opening a slave
# without O_NOCTTY makes it the controlling terminal only with read access. The tests drop
# CAP_SYS_ADMIN (docker's root has none)
cat > ctty.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

static void show(const char *what, long r)
{
    if (r < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %ld\n", what, r);
}

static void drop_sys_admin(void)
{
    /* capget/capset's version 3 header and data (CAP_SYS_ADMIN is bit 21) */
    struct { uint32_t version; int pid; } h = {0x20080522, 0};
    struct { uint32_t effective, permitted, inheritable; } d[2];
    syscall(SYS_capget, &h, d);
    d[0].effective &= ~(1u << 21);
    syscall(SYS_capset, &h, d);
}

static char *new_pty(int *master)
{
    *master = posix_openpt(O_RDWR | O_NOCTTY);
    grantpt(*master);
    unlockpt(*master);
    return ptsname(*master);
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    int m1, m2, done[2];
    char *name = strdup(new_pty(&m1));
    pipe(done);
    /* The victim's session owns the first terminal */
    pid_t victim = fork();
    if (victim == 0) {
        setsid();
        int fd = open(name, O_RDWR);
        printf("victim got it: %d\n", tcgetsid(fd) == getpid());
        char x;
        read(done[0], &x, 1);
        printf("victim still has it: %d\n", tcgetsid(fd) == getpid());
        _exit(0);
    }
    usleep(100000);
    pid_t pid = fork();
    if (pid == 0) {
        drop_sys_admin();
        setsid();
        int fd = open(name, O_RDWR | O_NOCTTY);
        show("TIOCSCTTY 0 on another session's terminal", ioctl(fd, TIOCSCTTY, 0));
        show("TIOCSCTTY 1 without CAP_SYS_ADMIN", ioctl(fd, TIOCSCTTY, 1));
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    write(done[1], "x", 1);
    waitpid(victim, NULL, 0);

    char *name2 = strdup(new_pty(&m2));
    pid = fork();
    if (pid == 0) {
        drop_sys_admin();
        setsid();
        int w = open(name2, O_WRONLY);
        show("after a write-only open, /dev/tty", open("/dev/tty", O_RDWR) >= 0 ? 0 : -1);
        show("TIOCSCTTY on a write-only fd", ioctl(w, TIOCSCTTY, 0));
        int r = open(name2, O_RDONLY);
        show("after a read-only open, /dev/tty", open("/dev/tty", O_RDWR) >= 0 ? 0 : -1);
        close(r);
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    return 0;
}
C
gcc -O1 -o ctty ctty.c && ./ctty
rm -f ctty ctty.c

# kill with a thread's id signals that thread's process (Linux's kill_pid_info), as kill -0 does
# for checking it exists; tgkill with a wrong thread group is ESRCH
cat > killtid.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static volatile pid_t tid;
static volatile int got;

static void on_usr1(int sig)
{
    (void)sig;
    got++;
}

static void *thread(void *arg)
{
    (void)arg;
    tid = syscall(SYS_gettid);
    for (;;)
        pause();
    return NULL;
}

int main(void)
{
    signal(SIGUSR1, on_usr1);
    pthread_t t;
    pthread_create(&t, NULL, thread, NULL);
    while (!tid)
        usleep(1000);
    printf("kill(tid, 0): %d\n", kill(tid, 0));
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &s, NULL);
    printf("kill(tid, SIGUSR1): %d\n", kill(tid, SIGUSR1));
    for (int i = 0; i < 200 && !got; i++)
        usleep(10000);
    printf("a thread handled it: %d\n", got);
    errno = 0;
    printf("tgkill with another group: %ld %s\n", syscall(SYS_tgkill, getppid(), tid, 0),
           strerror(errno));
    return 0;
}
C
gcc -O1 -pthread -o killtid killtid.c && ./killtid
rm -f killtid killtid.c

# A memfd belongs to its creator's filesystem ids, so a non-root creator can fchmod it
cat > memfdown.c <<'C'
#define _GNU_SOURCE
#include <grp.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static void show(const char *who)
{
    int fd = memfd_create("x", 0);
    struct stat st;
    fstat(fd, &st);
    printf("%s: owner %u:%u mode %o", who, st.st_uid, st.st_gid, st.st_mode & 07777);
    int r = fchmod(fd, 0600);
    fstat(fd, &st);
    printf(", fchmod 0600 %d, mode %o\n", r, st.st_mode & 07777);
    fflush(stdout);
}

int main(void)
{
    show("root");
    if (fork() == 0) {
        setgroups(0, NULL);
        setresgid(1001, 1001, 1001);
        setresuid(1000, 1000, 1000);
        show("user 1000:1001");
        _exit(0);
    }
    wait(NULL);
    return 0;
}
C
gcc -O1 -o memfdown memfdown.c && ./memfdown
rm -f memfdown memfdown.c

# A file whose last name is gone gets no new one: linking it through /proc/self/fd/N is ENOENT (a
# live one links), a memfd is EXDEV; linkat with AT_EMPTY_PATH needs CAP_DAC_READ_SEARCH (ENOENT;
# docker's root has none)
cat > relink.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void show(const char *what, long r)
{
    if (r < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %ld\n", what, r);
    fflush(stdout);
}

int main(void)
{
    char p[64];
    int fd = open("lk.a", O_RDWR | O_CREAT | O_TRUNC, 0644);
    write(fd, "x", 1);
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    show("link a live file through /proc/self/fd", linkat(AT_FDCWD, p, AT_FDCWD, "lk.b", AT_SYMLINK_FOLLOW));
    unlink("lk.a");
    unlink("lk.b");
    show("link an unlinked file through /proc/self/fd", linkat(AT_FDCWD, p, AT_FDCWD, "lk.c", AT_SYMLINK_FOLLOW));
    show("lk.c exists", access("lk.c", F_OK));
    int m = memfd_create("m", 0);
    snprintf(p, sizeof p, "/proc/self/fd/%d", m);
    show("link a memfd through /proc/self/fd", linkat(AT_FDCWD, p, AT_FDCWD, "lk.d", AT_SYMLINK_FOLLOW));
    int live = open("lk.e", O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fork() == 0) {
        /* capget/capset's version 3 header and data (CAP_DAC_READ_SEARCH is bit 2) */
        struct { uint32_t version; int pid; } h = {0x20080522, 0};
        struct { uint32_t effective, permitted, inheritable; } d[2];
        syscall(SYS_capget, &h, d);
        d[0].effective &= ~(1u << 2);
        syscall(SYS_capset, &h, d);
        show("AT_EMPTY_PATH without CAP_DAC_READ_SEARCH", linkat(live, "", AT_FDCWD, "lk.f", AT_EMPTY_PATH));
        _exit(0);
    }
    wait(NULL);
    unlink("lk.e");
    unlink("lk.f");
    return 0;
}
C
gcc -O1 -o relink relink.c && ./relink
rm -f relink relink.c

# Sysctls Tuxport only records exist with Linux's modes (values come from the host's kernel on the
# native side, so only their form is compared)
cd /proc/sys
for f in debug/exception-trace fs/overflowgid fs/overflowuid fs/suid_dumpable \
    kernel/core_pipe_limit kernel/hung_task_all_cpu_backtrace kernel/hung_task_check_count \
    kernel/hung_task_check_interval_secs kernel/hung_task_panic kernel/hung_task_timeout_secs \
    kernel/hung_task_warnings kernel/kptr_restrict kernel/modprobe kernel/modules_disabled \
    kernel/overflowgid kernel/overflowuid kernel/panic_on_oops kernel/perf_cpu_time_max_percent \
    kernel/perf_event_max_contexts_per_stack kernel/perf_event_max_sample_rate \
    kernel/perf_event_max_stack kernel/perf_event_mlock_kb kernel/perf_event_paranoid \
    kernel/randomize_va_space kernel/sched_rt_period_us kernel/sched_rt_runtime_us \
    kernel/sched_schedstats kernel/sysrq kernel/tainted vm/dirty_background_bytes \
    vm/dirty_background_ratio vm/dirty_bytes vm/dirty_expire_centisecs vm/dirty_ratio \
    vm/dirty_writeback_centisecs vm/drop_caches vm/mmap_rnd_bits vm/mmap_rnd_compat_bits \
    vm/overcommit_memory vm/overcommit_ratio vm/page-cluster vm/swtuxportss \
    vm/watermark_scale_factor; do
    mode=$(stat -c %a $f)
    case $mode in
    [46]*) case $(cat $f) in '' | *[!0-9-]*) form=text ;; *) form=number ;; esac ;;
    *) form=- ;;
    esac
    echo "$f $mode $form"
done
cd /tmp

# A signal sent to the process while the thread that would take it is about to block it (stopped
# by a tracer at the entry of rt_sigprocmask) is taken by another thread that does not block it
# (Linux's retarget_shared_pending; Linux picks the other thread at once, Tuxport when the first one
# blocks it)
cat > retarget.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <elf.h>

static volatile pid_t handler_tid;
static pid_t worker_tid;

static void on_usr1(int sig)
{
    (void)sig;
    handler_tid = syscall(SYS_gettid);
}

static void *worker(void *arg)
{
    (void)arg;
    worker_tid = syscall(SYS_gettid);
    for (;;)
        pause();
    return NULL;
}

/* The tracer: stops the parent's main thread at the entry of rt_sigprocmask, sends SIGUSR1 to the
 * process, and lets it go on */
static void tracer(pid_t main_tid, int go)
{
    if (ptrace(PTRACE_SEIZE, main_tid, 0, 0) < 0 || ptrace(PTRACE_INTERRUPT, main_tid, 0, 0) < 0)
        _exit(2);
    int st;
    waitpid(main_tid, &st, __WALL);
    ptrace(PTRACE_SYSCALL, main_tid, 0, 0);
    write(go, "g", 1);
    for (int i = 0; i < 1000; i++) {
        waitpid(main_tid, &st, __WALL);
        struct user_regs_struct { unsigned long long regs[31], sp, pc, pstate; } r;
        struct iovec io = {&r, sizeof r};
        ptrace(PTRACE_GETREGSET, main_tid, NT_PRSTATUS, &io);
        /* x7 is 0 at a syscall's entry; 135 is rt_sigprocmask */
        if (r.regs[8] == 135 && r.regs[7] == 0) {
            kill(main_tid, SIGUSR1);
            ptrace(PTRACE_DETACH, main_tid, 0, 0);
            _exit(0);
        }
        ptrace(PTRACE_SYSCALL, main_tid, 0, 0);
    }
    _exit(3);
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    signal(SIGUSR1, on_usr1);
    pthread_t t;
    pthread_create(&t, NULL, worker, NULL);
    while (!worker_tid)
        usleep(1000);
    prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY, 0, 0, 0);
    int p[2];
    pipe(p);
    pid_t me = getpid();
    pid_t c = fork();
    if (c == 0)
        tracer(me, p[1]);
    char x;
    read(p[0], &x, 1);
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &s, NULL);
    for (int i = 0; i < 300 && !handler_tid; i++) {
        struct timespec ts = {0, 10000000};
        nanosleep(&ts, NULL);
    }
    int st;
    waitpid(c, &st, 0);
    printf("tracer exit %d; taken by %s\n", WEXITSTATUS(st),
           handler_tid == worker_tid ? "the worker" : handler_tid ? "the main thread" : "nobody");
    return 0;
}
C
gcc -O1 -pthread -o retarget retarget.c && ./retarget
rm -f retarget retarget.c

# A blocking connect that cannot complete (the listener's accept queue is full, so its SYNs are
# dropped): SO_SNDTIMEO ends it with EINPROGRESS and a second connect with EALREADY; a signal
# whose handler has SA_RESTART restarts it, and it completes once the handler has made room; with
# SO_SNDTIMEO set the same signal makes it EINTR
cat > connwait.c <<'C'
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

static int lfd;
static volatile int handled;
static struct sockaddr_in addr;

static void on_alarm(int sig)
{
    (void)sig;
    handled++;
    int c = accept(lfd, NULL, NULL);
    if (c >= 0)
        close(c);
}

static void show(const char *what, int r)
{
    printf("%s: %s\n", what, r < 0 ? strerror(errno) : "0");
}

static int with_timeout(int ms)
{
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct timeval tv = {ms / 1000, ms % 1000 * 1000};
    setsockopt(s, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv);
    return s;
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    lfd = socket(AF_INET, SOCK_STREAM, 0);
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t len = sizeof addr;
    bind(lfd, (struct sockaddr *)&addr, sizeof addr);
    listen(lfd, 0);
    getsockname(lfd, (struct sockaddr *)&addr, &len);
    /* One connection fills the accept queue of a backlog of 0 */
    int c1 = socket(AF_INET, SOCK_STREAM, 0);
    show("first connect", connect(c1, (struct sockaddr *)&addr, sizeof addr));

    int s = with_timeout(300);
    show("connect with SO_SNDTIMEO", connect(s, (struct sockaddr *)&addr, sizeof addr));
    show("connect again", connect(s, (struct sockaddr *)&addr, sizeof addr));
    close(s);

    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alarm;
    sa.sa_flags = SA_RESTART;
    sigaction(SIGALRM, &sa, NULL);
    struct itimerval it = {{0, 0}, {0, 200000}};
    setitimer(ITIMER_REAL, &it, NULL);
    int t = socket(AF_INET, SOCK_STREAM, 0);
    show("connect restarted after a SA_RESTART handler", connect(t, (struct sockaddr *)&addr, sizeof addr));
    printf("handled: %d\n", handled);

    /* t's connection now fills the queue */
    int u = with_timeout(5000);
    setitimer(ITIMER_REAL, &it, NULL);
    show("connect with SO_SNDTIMEO and a SA_RESTART signal", connect(u, (struct sockaddr *)&addr, sizeof addr));
    return 0;
}
C
gcc -O1 -o connwait connwait.c && ./connwait
rm -f connwait connwait.c

# The main thread exits while another runs on (pthread_exit): it stays as a zombie thread until the
# process is reaped, so task/ still lists it with state Z and its name, the process directory shows
# it (Z, both threads counted, an empty cmdline and fd/, no cwd or exe)
cat > leaderzombie.c <<'C'
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>

static pid_t pid;

static char *slurp(const char *path, int *n)
{
    static char buf[4096];
    int fd = open(path, O_RDONLY);
    *n = -1;
    if (fd < 0)
        return NULL;
    *n = read(fd, buf, sizeof buf - 1);
    close(fd);
    buf[*n > 0 ? *n : 0] = 0;
    return buf;
}

/* The state letter and the thread count of a stat file */
static void stat_of(const char *what, const char *path)
{
    int n;
    char *s = slurp(path, &n);
    if (!s) {
        printf("%s: %s\n", what, strerror(errno));
        return;
    }
    char *p = strrchr(s, ')');
    char state;
    long threads = 0;
    sscanf(p + 2, "%c", &state);
    /* field 20 is num_threads: the 18th space after the ')' precedes it */
    for (int i = 0; i < 18 && p; i++)
        p = strchr(p + 1, ' ');
    if (p)
        threads = strtol(p + 1, NULL, 10);
    char comm[32] = "";
    sscanf(strchr(s, '(') + 1, "%31[^)]", comm);
    printf("%s: state %c, name %s, threads %ld\n", what, state, comm, threads);
}

static void *other(void *arg)
{
    (void)arg;
    usleep(200000);
    char path[64], line[256];
    snprintf(path, sizeof path, "/proc/%d/task", pid);
    DIR *d = opendir(path);
    struct dirent *e;
    int mine = 0, main_ = 0, others = 0;
    while ((e = readdir(d))) {
        if (e->d_name[0] == '.')
            continue;
        long t = strtol(e->d_name, NULL, 10);
        if (t == pid)
            main_++;
        else if (t == syscall(SYS_gettid))
            mine++;
        else
            others++;
    }
    closedir(d);
    printf("task/ lists the main thread %d, this thread %d, others %d\n", main_, mine, others);
    snprintf(path, sizeof path, "/proc/%d/task/%d/stat", pid, pid);
    stat_of("task/<main>/stat", path);
    snprintf(path, sizeof path, "/proc/%d/stat", pid);
    stat_of("process stat", path);
    int n;
    snprintf(path, sizeof path, "/proc/%d/status", pid);
    char *s = slurp(path, &n);
    char *st = strstr(s, "State:"), *th = strstr(s, "Threads:");
    sscanf(st, "%255[^\n]", line);
    printf("status %s", line);
    sscanf(th, "%255[^\n]", line);
    printf(", %s\n", line);
    snprintf(path, sizeof path, "/proc/%d/cmdline", pid);
    slurp(path, &n);
    printf("cmdline: %d bytes\n", n);
    snprintf(path, sizeof path, "/proc/%d/fd", pid);
    d = opendir(path);
    int fds = 0;
    while (d && (e = readdir(d)))
        fds += e->d_name[0] != '.';
    if (d)
        closedir(d);
    printf("fd/: %d entries\n", fds);
    snprintf(path, sizeof path, "/proc/%d/exe", pid);
    printf("exe: %s\n", readlink(path, line, sizeof line) < 0 ? strerror(errno) : "a link");
    snprintf(path, sizeof path, "/proc/%d/cwd", pid);
    printf("cwd: %s\n", readlink(path, line, sizeof line) < 0 ? strerror(errno) : "a link");
    fflush(stdout);
    exit(0);
}

int main(void)
{
    pid = getpid();
    prctl(PR_SET_NAME, "zmain");
    pthread_t t;
    pthread_create(&t, NULL, other, NULL);
    pthread_exit(NULL);
}
C
gcc -O1 -pthread -o leaderzombie leaderzombie.c && ./leaderzombie
rm -f leaderzombie leaderzombie.c

# /proc/vmstat exists with the counters procps' vmstat reads (the values are the host kernel's on
# the native side)
for k in nr_free_pages nr_anon_pages nr_file_pages pgpgin pgpgout pswpin pswpout pgfault pgmajfault; do
    echo "vmstat $k: $(grep -c "^$k [0-9][0-9]*\$" /proc/vmstat)"
done

# RLIMIT_MSGQUEUE starts at Linux's MQ_BYTES_MAX
python3 -c 'import resource; print("RLIMIT_MSGQUEUE", resource.getrlimit(resource.RLIMIT_MSGQUEUE))'

# sched_setattr's flags: SCHED_FLAG_KEEP_POLICY and KEEP_PARAMS keep what they say, the utilization
# clamps are checked (at most 1024, minimum not above maximum, -1 resets, the structure must hold
# them) and read back by sched_getattr; unknown flags are EINVAL
cat > schedattr.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

struct attr {
    uint32_t size, policy;
    uint64_t flags;
    int32_t nice;
    uint32_t priority;
    uint64_t runtime, deadline, period;
    uint32_t util_min, util_max;
};

static void set(const char *what, uint64_t flags, int nice, uint32_t lo, uint32_t hi, uint32_t size)
{
    struct attr a = {.size = size, .policy = 0, .flags = flags, .nice = nice, .util_min = lo, .util_max = hi};
    long r = syscall(SYS_sched_setattr, 0, &a, 0);
    struct attr g;
    memset(&g, 0, sizeof g);
    syscall(SYS_sched_getattr, 0, &g, sizeof g, 0);
    printf("%s: %s; now policy %u nice %d clamps %u..%u\n", what, r < 0 ? strerror(errno) : "0",
           g.policy, g.nice, g.util_min, g.util_max);
}

int main(void)
{
    set("keep policy and params, clamps 100..800", 0x08 | 0x10 | 0x20 | 0x40, 7, 100, 800, sizeof(struct attr));
    set("keep the policy, nice 5", 0x08, 5, 0, 0, sizeof(struct attr));
    set("clamps 900..100", 0x18 | 0x60, 0, 900, 100, sizeof(struct attr));
    set("clamp min 2000", 0x18 | 0x20, 0, 2000, 0, sizeof(struct attr));
    set("clamp min -1 resets it", 0x18 | 0x20, 0, UINT32_MAX, 0, sizeof(struct attr));
    set("clamps with the first structure", 0x18 | 0x20, 0, 10, 0, 48);
    set("the first structure, nice 6", 0, 6, 0, 0, 48);
    set("unknown flag", 0x80, 0, 0, 0, sizeof(struct attr));
    return 0;
}
C
gcc -O1 -o schedattr schedattr.c && ./schedattr
rm -f schedattr schedattr.c

# membarrier registrations are kept by a forked child (until execve)
cat > membarrier.c <<'C'
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* The commands of <linux/membarrier.h> (not installed here) */
#define MEMBARRIER_CMD_PRIVATE_EXPEDITED (1 << 3)
#define MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED (1 << 4)
#define MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE (1 << 5)
#define MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE (1 << 6)

static const char *mb(int cmd)
{
    return syscall(SYS_membarrier, cmd, 0, 0) < 0 ? strerror(errno) : "0";
}

int main(void)
{
    printf("register private expedited: %s\n", mb(MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED));
    printf("register private expedited sync core: %s\n",
           mb(MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE));
    fflush(stdout);
    if (fork() == 0) {
        printf("child private expedited: %s\n", mb(MEMBARRIER_CMD_PRIVATE_EXPEDITED));
        printf("child private expedited sync core: %s\n",
               mb(MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE));
        fflush(stdout);
        _exit(0);
    }
    wait(NULL);
    return 0;
}
C
gcc -O1 -o membarrier membarrier.c && ./membarrier
rm -f membarrier membarrier.c

# A CLONE_PARENT child (clone3 requires exit signal 0 for it) signals the parent it shares with
# its creator as its creator does: with SIGCHLD here
cat > cloneparent.c <<'C'
#define _GNU_SOURCE
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

struct clone_args {
    uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid,
        set_tid_size, cgroup;
};

int main(void)
{
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGCHLD);
    sigprocmask(SIG_BLOCK, &s, NULL);
    int p[2];
    pipe(p);
    pid_t forked = fork();
    if (forked == 0) {
        struct clone_args a;
        memset(&a, 0, sizeof a);
        a.flags = CLONE_PARENT;
        long c = syscall(SYS_clone3, &a, sizeof a);
        if (c == 0)
            _exit(7);
        write(p[1], &c, sizeof c);
        usleep(300000);
        _exit(0);
    }
    long clone_child;
    read(p[0], &clone_child, sizeof clone_child);
    struct timespec ts = {2, 0};
    siginfo_t si;
    int sig = sigtimedwait(&s, &si, &ts);
    printf("first SIGCHLD: %s, from %s\n", sig == SIGCHLD ? "yes" : "no",
           si.si_pid == clone_child ? "the clone3 child" : si.si_pid == forked ? "the forked child" : "?");
    int st;
    printf("plain waitpid reaps the clone3 child: %d, status %d\n",
           waitpid(clone_child, &st, 0) == clone_child, WEXITSTATUS(st));
    waitpid(forked, &st, 0);
    return 0;
}
C
gcc -O1 -o cloneparent cloneparent.c && ./cloneparent
rm -f cloneparent cloneparent.c

# A SIGEV_THREAD_ID timer whose thread has exited sends nothing (not to the process)
cat > deadtimer.c <<'C'
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/* The kernel's struct sigevent */
struct ksigevent {
    long value;
    int signo, notify;
    int tid, pad[11];
};

static volatile pid_t tid;
static int go[2];

static void *thread(void *arg)
{
    (void)arg;
    tid = syscall(SYS_gettid);
    char x;
    read(go[0], &x, 1);
    return NULL;
}

int main(void)
{
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR1);
    sigprocmask(SIG_BLOCK, &s, NULL);
    pipe(go);
    pthread_t t;
    pthread_create(&t, NULL, thread, NULL);
    while (!tid)
        usleep(1000);
    struct ksigevent ev;
    memset(&ev, 0, sizeof ev);
    ev.signo = SIGUSR1;
    ev.notify = 4; /* SIGEV_THREAD_ID */
    ev.tid = tid;
    int id = -1;
    long r = syscall(SYS_timer_create, CLOCK_MONOTONIC, &ev, &id);
    printf("timer_create for the thread: %ld\n", r);
    /* The thread exits, and is gone from the kernel too */
    write(go[1], "x", 1);
    pthread_join(t, NULL);
    char path[64];
    snprintf(path, sizeof path, "/proc/self/task/%d", tid);
    for (int i = 0; i < 300 && access(path, F_OK) == 0; i++)
        usleep(10000);
    struct itimerspec its = {{0, 0}, {0, 50000000}};
    syscall(SYS_timer_settime, id, 0, &its, NULL);
    struct timespec ts = {0, 300000000};
    printf("signal after the thread exited: %s\n",
           sigtimedwait(&s, NULL, &ts) == SIGUSR1 ? "delivered" : "none");
    return 0;
}
C
gcc -O1 -pthread -o deadtimer deadtimer.c && ./deadtimer
rm -f deadtimer deadtimer.c

# sendfile into an O_APPEND file is EINVAL (into a pipe it is fine)
cat > sendappend.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/sendfile.h>
#include <unistd.h>

int main(void)
{
    int in = open("sa.in", O_RDWR | O_CREAT | O_TRUNC, 0644);
    write(in, "hello", 5);
    int out = open("sa.out", O_WRONLY | O_CREAT | O_TRUNC | O_APPEND, 0644);
    off_t off = 0;
    long r = sendfile(out, in, &off, 5);
    printf("sendfile into an O_APPEND file: %s\n", r < 0 ? strerror(errno) : "copied");
    int p[2];
    pipe(p);
    fcntl(p[1], F_SETFL, O_APPEND);
    off = 0;
    r = sendfile(p[1], in, &off, 5);
    printf("sendfile into an O_APPEND pipe: %ld\n", r);
    unlink("sa.in");
    unlink("sa.out");
    return 0;
}
C
gcc -O1 -o sendappend sendappend.c && ./sendappend
rm -f sendappend sendappend.c

# Thread names are bytes: comm and stat give them as set (here cut by the 15-byte limit in the
# middle of a UTF-8 character), status escapes only a newline and a backslash
cat > rawcomm.c <<'C'
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <unistd.h>

static void hex(const char *what, const char *path, int field)
{
    char buf[4096];
    int fd = open(path, O_RDONLY), n = read(fd, buf, sizeof buf);
    close(fd);
    char *p = buf, *end = buf + n;
    if (field == 1) { /* stat: between the parentheses */
        p = memchr(buf, '(', n) + 1;
        end = memrchr(buf, ')', n);
    } else if (field == 2) { /* status: the Name line */
        p = buf + 6;
        end = memchr(buf, '\n', n);
    } else {
        end = buf + n - 1;
    }
    printf("%s:", what);
    for (; p < end; p++)
        printf(" %02x", (unsigned char)*p);
    printf("\n");
}

int main(void)
{
    const char *names[] = {"a\xe4\xb8\xad\xe6\x96\x87\xe5\x90\x8d\xe5\xad\x97\xe6\x98\xaf\xe9\x95\xbf", "a\nb\\c"};
    for (int i = 0; i < 2; i++) {
        prctl(PR_SET_NAME, names[i]);
        hex("comm", "/proc/self/comm", 0);
        hex("stat", "/proc/self/stat", 1);
        hex("status", "/proc/self/status", 2);
    }
    return 0;
}
C
gcc -O1 -D_GNU_SOURCE -o rawcomm rawcomm.c && ./rawcomm
rm -f rawcomm rawcomm.c

# status splits VmRSS into RssAnon, RssFile and RssShmem: touching 8 MB of private anonymous
# memory, 4 MB of shared anonymous memory and reading 4 MB of a mapped file grow each (rounded to
# MB; the totals themselves differ between kernels)
cat > rsssplit.c <<'C'
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

struct rss { long vm, anon, file, shmem; };

static struct rss now(void)
{
    char buf[8192];
    int fd = open("/proc/self/status", O_RDONLY), n = read(fd, buf, sizeof buf - 1);
    close(fd);
    buf[n] = 0;
    struct rss r = {-1, -1, -1, -1};
    char *p;
    if ((p = strstr(buf, "VmRSS:")))
        r.vm = strtol(p + 6, NULL, 10);
    if ((p = strstr(buf, "RssAnon:")))
        r.anon = strtol(p + 8, NULL, 10);
    if ((p = strstr(buf, "RssFile:")))
        r.file = strtol(p + 8, NULL, 10);
    if ((p = strstr(buf, "RssShmem:")))
        r.shmem = strtol(p + 9, NULL, 10);
    return r;
}

static long mb(long kb) { return (kb + 512) / 1024; }

int main(void)
{
    const size_t M = 1 << 20;
    int fd = open("rss.file", O_RDWR | O_CREAT | O_TRUNC, 0644);
    char *junk = malloc(4 * M);
    memset(junk, 1, 4 * M);
    write(fd, junk, 4 * M);
    free(junk);
    struct rss a = now();
    printf("VmRSS is the sum: %d\n", a.vm == a.anon + a.file + a.shmem);
    char *p = mmap(NULL, 8 * M, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memset(p, 1, 8 * M);
    struct rss b = now();
    printf("after 8 MB anonymous: anon +%ld MB, file +%ld, shmem +%ld\n", mb(b.anon - a.anon),
           mb(b.file - a.file), mb(b.shmem - a.shmem));
    char *q = mmap(NULL, 4 * M, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    memset(q, 1, 4 * M);
    struct rss c = now();
    printf("after 4 MB shared: anon +%ld MB, file +%ld, shmem +%ld\n", mb(c.anon - b.anon),
           mb(c.file - b.file), mb(c.shmem - b.shmem));
    char *f = mmap(NULL, 4 * M, PROT_READ, MAP_PRIVATE, fd, 0);
    volatile long sum = 0;
    for (size_t i = 0; i < 4 * M; i += 4096)
        sum += f[i];
    struct rss d = now();
    printf("after reading a 4 MB file: anon +%ld MB, file +%ld, shmem +%ld\n", mb(d.anon - c.anon),
           mb(d.file - c.file), mb(d.shmem - c.shmem));
    printf("VmRSS is the sum: %d\n", d.vm == d.anon + d.file + d.shmem);
    unlink("rss.file");
    return 0;
}
C
gcc -O1 -o rsssplit rsssplit.c && ./rsssplit
rm -f rsssplit rsssplit.c

# AF_UNIX streams: a read of nothing returns 0 at once when something is queued and takes the
# first message's files (EAGAIN on an empty queue); shutdown works on unconnected and listening
# sockets
cat > unixzero.c <<'C'
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static void show(const char *what, long r)
{
    printf("%s: %s\n", what, r < 0 ? strerror(errno) : r == 0 ? "0" : "more");
}

int main(void)
{
    int sv[2];
    char buf[16];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    show("read of nothing, empty queue", recv(sv[0], buf, 0, MSG_DONTWAIT));
    send(sv[1], "hello", 5, 0);
    show("read of nothing, data queued", recv(sv[0], buf, 0, MSG_DONTWAIT));
    long n = recv(sv[0], buf, sizeof buf, MSG_DONTWAIT);
    printf("then: %.*s\n", (int)n, buf);

    /* One byte with an fd */
    char c = 'x';
    struct iovec iov = {&c, 1};
    union { struct cmsghdr h; char b[CMSG_SPACE(sizeof(int))]; } u;
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.b, .msg_controllen = sizeof u.b};
    struct cmsghdr *h = CMSG_FIRSTHDR(&m);
    h->cmsg_level = SOL_SOCKET;
    h->cmsg_type = SCM_RIGHTS;
    h->cmsg_len = CMSG_LEN(sizeof(int));
    int one = 1;
    memcpy(CMSG_DATA(h), &one, sizeof one);
    sendmsg(sv[1], &m, 0);
    struct iovec none = {buf, 0};
    struct msghdr r = {.msg_iov = &none, .msg_iovlen = 1, .msg_control = u.b, .msg_controllen = sizeof u.b};
    n = recvmsg(sv[0], &r, MSG_DONTWAIT);
    printf("read of nothing with a file queued: %ld, files: %d\n", n,
           r.msg_controllen > 0 && CMSG_FIRSTHDR(&r) && CMSG_FIRSTHDR(&r)->cmsg_type == SCM_RIGHTS);
    struct msghdr r2 = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.b, .msg_controllen = sizeof u.b};
    n = recvmsg(sv[0], &r2, MSG_DONTWAIT);
    printf("then: %ld byte, files: %d\n", n, r2.msg_controllen > 0);

    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    show("shutdown of an unconnected socket", shutdown(s, SHUT_RDWR));
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    strcpy(a.sun_path + 1, "tuxport-unixzero");
    int l = socket(AF_UNIX, SOCK_STREAM, 0);
    bind(l, (struct sockaddr *)&a, sizeof a);
    listen(l, 1);
    show("shutdown of a listening socket", shutdown(l, SHUT_RD));
    return 0;
}
C
gcc -O1 -o unixzero unixzero.c && ./unixzero
rm -f unixzero unixzero.c

# A thread that dies holding a robust mutex whose page is still shared copy-on-write with a forked
# child: the lock still gets FUTEX_OWNER_DIED, so the next locker sees EOWNERDEAD
cat > robustcow.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* In private anonymous memory, which fork lends copy-on-write (a data segment is copied) */
static pthread_mutex_t *m;
static int locked[2], die[2];
static volatile pid_t holder_tid;

static void *holder(void *arg)
{
    (void)arg;
    holder_tid = syscall(SYS_gettid);
    pthread_mutex_lock(m);
    write(locked[1], "l", 1);
    char x;
    read(die[0], &x, 1);
    /* Exits holding the lock, without libc's own robust-list cleanup in pthread_exit, so the
     * kernel walks the robust list */
    syscall(SYS_exit, 0);
    return NULL;
}

int main(void)
{
    pthread_mutexattr_t a;
    pthread_mutexattr_init(&a);
    pthread_mutexattr_setrobust(&a, PTHREAD_MUTEX_ROBUST);
    /* musl hands only process-shared robust mutexes to the kernel's robust list */
    pthread_mutexattr_setpshared(&a, PTHREAD_PROCESS_SHARED);
    m = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    pthread_mutex_init(m, &a);
    pipe(locked);
    pipe(die);
    pthread_t t;
    pthread_create(&t, NULL, holder, NULL);
    char x;
    read(locked[0], &x, 1);
    /* The child keeps the pages shared until the holder has died */
    pid_t c = fork();
    if (c == 0) {
        usleep(500000);
        _exit(0);
    }
    write(die[1], "d", 1);
    /* pthread_join would wait for libc's own exit path: wait for the thread to be gone instead */
    char path[64];
    snprintf(path, sizeof path, "/proc/self/task/%d", holder_tid);
    for (int i = 0; i < 300 && access(path, F_OK) == 0; i++)
        usleep(10000);
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    ts.tv_sec += 2;
    int r = pthread_mutex_timedlock(m, &ts);
    printf("lock after the holder died: %s\n", r == EOWNERDEAD ? "EOWNERDEAD" : strerror(r));
    waitpid(c, NULL, 0);
    return 0;
}
C
gcc -O1 -pthread -o robustcow robustcow.c && ./robustcow
rm -f robustcow robustcow.c
# When splice, tee and vmsplice wait (Linux 6.1's do_splice, do_tee, vmsplice_to_pipe and
# vmsplice_to_user): into a pipe, for a free page unless SPLICE_F_NONBLOCK or the output pipe's
# O_NONBLOCK; an AF_UNIX stream socket read into a pipe also stops at either; between two pipes and
# for tee, either end's O_NONBLOCK counts too; tee from a source at end of file gives 0, EAGAIN
# into a full nonblocking pipe and EPIPE without readers; vmsplice looks only at
# SPLICE_F_NONBLOCK, both ways. Blocking calls are bounded by an alarm (EINTR)
cat > splicewait.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

static void on_alarm(int s) { (void)s; }

static void show(const char *what, long r) {
    if (r < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %ld\n", what, r);
    fflush(stdout);
}

/* Every call that could wait is bounded by its own alarm, so a broken wait ends in EINTR */
#define TRY(what, call)                                                                            \
    do {                                                                                           \
        alarm(2);                                                                                  \
        long r_ = (call);                                                                          \
        int e_ = errno;                                                                            \
        alarm(0);                                                                                  \
        errno = e_;                                                                                \
        show(what, r_);                                                                            \
    } while (0)

/* A pipe of one page, filled to `fill` bytes */
static void small_pipe(int p[2], int flags, int fill) {
    char buf[4096];
    pipe2(p, flags);
    fcntl(p[1], F_SETPIPE_SZ, 4096);
    memset(buf, 'x', sizeof buf);
    if (fill)
        write(p[1], buf, fill);
}

static int late_fd;
static void *write_late(void *arg) {
    usleep(100000);
    write(late_fd, "late", 4);
    return arg;
}
static void *read_late(void *arg) {
    char buf[4096];
    usleep(100000);
    read(late_fd, buf, sizeof buf);
    return arg;
}

int main(void) {
    struct sigaction sa = {.sa_handler = on_alarm};
    sigaction(SIGALRM, &sa, NULL);
    signal(SIGPIPE, SIG_IGN);
    int p[2], q[2], s[2], f;
    char buf[64];
    pthread_t t;

    /* An empty blocking AF_UNIX stream socket into a pipe */
    socketpair(AF_UNIX, SOCK_STREAM, 0, s);
    pipe(p);
    TRY("unix socket, SPLICE_F_NONBLOCK", splice(s[0], NULL, p[1], NULL, 8, SPLICE_F_NONBLOCK));
    close(p[0]), close(p[1]);
    pipe2(p, O_NONBLOCK);
    TRY("unix socket, O_NONBLOCK pipe", splice(s[0], NULL, p[1], NULL, 8, 0));
    show("socket still blocking", !!(fcntl(s[0], F_GETFL) & O_NONBLOCK));
    close(p[0]), close(p[1]);

    /* A file into a pipe with less than a page free */
    f = open("splicewait.c", O_RDONLY);
    small_pipe(p, 0, 4000);
    TRY("file into a full pipe, SPLICE_F_NONBLOCK", splice(f, NULL, p[1], NULL, 8, SPLICE_F_NONBLOCK));
    TRY("file into a full pipe, blocking", splice(f, NULL, p[1], NULL, 8, 0));
    late_fd = p[0];
    pthread_create(&t, NULL, read_late, NULL);
    TRY("file into a pipe being read", splice(f, NULL, p[1], NULL, 8, 0));
    pthread_join(t, NULL);
    show("file position", lseek(f, 0, SEEK_CUR));
    close(p[0]), close(p[1]);

    /* Between two pipes: the source has data, the destination is full */
    pipe(q);
    write(q[1], "abc", 3);
    small_pipe(p, 0, 4096);
    TRY("pipe to a full pipe, SPLICE_F_NONBLOCK", splice(q[0], NULL, p[1], NULL, 8, SPLICE_F_NONBLOCK));
    fcntl(p[1], F_SETFL, O_NONBLOCK);
    TRY("pipe to a full O_NONBLOCK pipe", splice(q[0], NULL, p[1], NULL, 8, 0));
    show("source kept", read(q[0], buf, sizeof buf));
    fcntl(q[0], F_SETFL, O_NONBLOCK);
    fcntl(p[1], F_SETFL, 0);
    read(p[0], buf, sizeof buf);
    TRY("empty O_NONBLOCK source", splice(q[0], NULL, p[1], NULL, 8, 0));
    TRY("the same pipe", splice(q[0], NULL, q[1], NULL, 8, 0));
    close(p[0]), close(p[1]), close(q[0]), close(q[1]);

    /* tee: an empty source, a source at end of file, a full and a reader-less destination */
    pipe(q);
    pipe2(p, O_NONBLOCK);
    TRY("tee from an empty pipe into an O_NONBLOCK one", tee(q[0], p[1], 16, 0));
    close(q[1]);
    TRY("tee from end of file", tee(q[0], p[1], 16, 0));
    close(p[0]), close(p[1]);
    small_pipe(p, O_NONBLOCK, 4096);
    TRY("tee from end of file into a full pipe", tee(q[0], p[1], 16, 0));
    close(p[0]);
    TRY("tee into a pipe without readers", tee(q[0], p[1], 16, 0));
    close(p[1]), close(q[0]);
    /* A blocked tee whose source reaches end of file while it waits copies nothing */
    pipe(q);
    write(q[1], "U000", 4);
    close(q[1]);
    small_pipe(p, 0, 4096);
    late_fd = p[0];
    pthread_create(&t, NULL, read_late, NULL);
    char drained[4];
    read(q[0], drained, 4);
    TRY("blocked tee after the source drained", tee(q[0], p[1], 4, 0));
    pthread_join(t, NULL);
    close(p[0]), close(p[1]), close(q[0]);

    /* vmsplice ignores the pipe's O_NONBLOCK both ways */
    struct iovec v = {buf, 4};
    pipe2(p, O_NONBLOCK);
    TRY("vmsplice from an empty pipe, SPLICE_F_NONBLOCK", vmsplice(p[0], &v, 1, SPLICE_F_NONBLOCK));
    late_fd = p[1];
    pthread_create(&t, NULL, write_late, NULL);
    TRY("vmsplice from an empty O_NONBLOCK pipe", vmsplice(p[0], &v, 1, 0));
    pthread_join(t, NULL);
    close(p[0]), close(p[1]);
    small_pipe(p, O_NONBLOCK, 4096);
    TRY("vmsplice into a full pipe, SPLICE_F_NONBLOCK", vmsplice(p[1], &v, 1, SPLICE_F_NONBLOCK));
    late_fd = p[0];
    pthread_create(&t, NULL, read_late, NULL);
    TRY("vmsplice into a full O_NONBLOCK pipe", vmsplice(p[1], &v, 1, 0));
    pthread_join(t, NULL);
    close(p[0]);
    struct iovec none = {buf, 0};
    TRY("empty vmsplice without readers", vmsplice(p[1], &none, 1, 0));
    close(p[1]);
    return 0;
}
C
gcc -O1 -pthread -o splicewait splicewait.c && ./splicewait
rm -f splicewait splicewait.c
# A splice out of a pipe keeps the pipe to itself until its write is done (Linux holds the pipe's
# lock): 1 MiB spliced into an AF_UNIX socket that nobody reads yet blocks, a reader of the same
# pipe started meanwhile waits for the splice, and the socket and the reader then hold the stream
# between them, in order, each byte once (the reader takes what the splice left, here nothing).
# The write end is closed only once the socket is drained: Linux's close takes the pipe's lock too
cat > splicerace.c <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define SIZE (1 << 20)
static int p[2], s[2];
static unsigned char *want, *got_sock, *got_pipe;
static long spliced, read_pipe;

static void *splicer(void *arg)
{
    spliced = splice(p[0], NULL, s[0], NULL, SIZE, 0);
    shutdown(s[0], SHUT_WR);
    return arg;
}

static void *reader(void *arg)
{
    long n;
    while ((n = read(p[0], got_pipe + read_pipe, SIZE - read_pipe)) > 0)
        read_pipe += n;
    return arg;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    want = malloc(SIZE), got_sock = malloc(SIZE), got_pipe = malloc(SIZE);
    for (long i = 0; i < SIZE; i++)
        want[i] = i * 7 + i / 251;
    pipe(p);
    printf("pipe size: %d\n", fcntl(p[1], F_SETPIPE_SZ, SIZE));
    printf("filled: %ld\n", (long)write(p[1], want, SIZE));
    socketpair(AF_UNIX, SOCK_STREAM, 0, s);
    pthread_t a, b;
    pthread_create(&a, NULL, splicer, NULL);
    usleep(200000);
    pthread_create(&b, NULL, reader, NULL);
    usleep(200000);
    long total = 0, n;
    while ((n = read(s[1], got_sock + total, SIZE - total)) > 0)
        total += n;
    pthread_join(a, NULL);
    close(p[1]);
    pthread_join(b, NULL);
    int intact = spliced == total && total + read_pipe == SIZE && !memcmp(got_sock, want, total) &&
                 !memcmp(got_pipe, want + total, read_pipe);
    printf("socket and reader: %ld bytes, the stream intact: %s\n", total + read_pipe,
           intact ? "yes" : "no");
    return 0;
}
C
gcc -O1 -pthread -o splicerace splicerace.c && ./splicerace
rm -f splicerace splicerace.c
