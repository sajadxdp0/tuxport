# docker-run-options: --privileged
# The small batch of the seventh map-bug triage: memfd's F_SEAL_EXEC keeps a memfd's execute bits
# (MFD_EXEC with MFD_NOEXEC_SEAL is EINVAL, and sealing an executable memfd's execute bits seals
# its contents too); personality(2)'s READ_IMPLIES_EXEC makes the heap executable (mmap and
# mprotect are Starnix's personality_test's), every execve clears it (arm64's SET_PERSONALITY)
# and a setuid one the other PER_CLEAR_ON_SETID flags too;
# a bind mount whose source name was removed shows `//deleted` after its root in mountinfo,
# whether the source was a directory, a file or one link of a file with another; the tenth
# triage's POSIX timers on the CPU clock of a thread or process that has ended; and getsockopt's
# SO_GET_FILTER, whose optlen counts classic BPF instructions of 8 bytes (Linux's sk_get_filter):
# 0 asks for the count, fewer than the program has is EINVAL, and the program comes back whole
# with optlen its count, on a UDP socket and on a TCP one (which needs CAP_NET_ADMIN on kernels
# with 5d39580f68e6, and which such a host refuses to Tuxport, so Tuxport keeps the program): an
# optlen other than 16 or no program is EINVAL, accepted sockets inherit the listener's program,
# and a locked one can be neither replaced nor detached; and
# /proc/<pid>/syscall (Linux's proc_pid_syscall): the reader's own read, a thread waiting in read,
# "running" for one running, ESRCH once that thread has gone, -1 with sp and the svc's pc for one
# stopped by a signal (signal delivery forgets the syscall), -1 with zeros for a zombie, mode 0400,
# and ptrace access checked when read with the reader's own credentials; and the ptrace-gated files
# of /proc/<tid> for a thread other than the main one (maps, smaps, smaps_rollup, auxv, environ,
# pagemap, mem), which open on the process's address space
set -e
cd /tmp
cat > small.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/personality.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define MFD_ALLOW_SEALING_ 0x2
#define MFD_NOEXEC_SEAL_ 0x8
#define MFD_EXEC_ 0x10
#define F_ADD_SEALS_ 1033
#define F_GET_SEALS_ 1034
#define F_SEAL_EXEC_ 0x20
#define READ_IMPLIES_EXEC_ 0x0400000
#define ADDR_NO_RANDOMIZE_ 0x0040000

static int memfd(const char *name, unsigned flags)
{
    return syscall(SYS_memfd_create, name, flags);
}

static void show(const char *what, long r)
{
    printf("%s: %ld %s\n", what, r, r < 0 ? strerror(errno) : "");
}

// The permissions /proc/self/maps shows for the mapping holding addr
static void perms(const char *what, unsigned long addr)
{
    FILE *f = fopen("/proc/self/maps", "r");
    char line[512];
    while (fgets(line, sizeof line, f)) {
        unsigned long lo, hi;
        char p[8];
        if (sscanf(line, "%lx-%lx %7s", &lo, &hi, p) == 3 && lo <= addr && addr < hi) {
            printf("%s: %s\n", what, p);
            fclose(f);
            return;
        }
    }
    printf("%s: unmapped\n", what);
    fclose(f);
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1) {
        printf("%s: personality %#x\n", argv[1], personality(0xffffffff));
        return 0;
    }

    show("MFD_NOEXEC_SEAL|MFD_EXEC", memfd("both", MFD_NOEXEC_SEAL_ | MFD_EXEC_));
    int n = memfd("noexec", MFD_NOEXEC_SEAL_);
    struct stat st;
    fstat(n, &st);
    printf("MFD_NOEXEC_SEAL: mode %o seals %#x\n", st.st_mode & 07777, fcntl(n, F_GET_SEALS_));
    show("chmod 0755 of it", fchmod(n, 0755));
    show("chmod 0600 of it", fchmod(n, 0600));
    int m = memfd("exec", MFD_ALLOW_SEALING_);
    show("F_SEAL_EXEC on a 0777 memfd", fcntl(m, F_ADD_SEALS_, F_SEAL_EXEC_));
    printf("its seals %#x\n", fcntl(m, F_GET_SEALS_));
    show("write to it", write(m, "x", 1));
    show("chmod 0755 of it (execute bits kept)", fchmod(m, 0755));
    show("chmod 0644 of it", fchmod(m, 0644));
    int w = memfd("nox", MFD_ALLOW_SEALING_);
    fchmod(w, 0644);
    show("F_SEAL_EXEC on a 0644 memfd", fcntl(w, F_ADD_SEALS_, F_SEAL_EXEC_));
    printf("its seals %#x\n", fcntl(w, F_GET_SEALS_));
    show("write to it", write(w, "x", 1));

    // The heap grown with READ_IMPLIES_EXEC is executable; without it, not
    unsigned long brk0 = syscall(SYS_brk, 0);
    unsigned long page = (brk0 + 4095) & ~4095UL;
    syscall(SYS_brk, page + 0x4000);
    perms("heap grown without READ_IMPLIES_EXEC", page);
    personality(READ_IMPLIES_EXEC_ | ADDR_NO_RANDOMIZE_);
    syscall(SYS_brk, page + 0x8000);
    perms("heap grown with READ_IMPLIES_EXEC", page + 0x4000);

    // Kept by an ordinary execve, cleared by a setuid one
    char self[256];
    ssize_t len = readlink("/proc/self/exe", self, sizeof self - 1);
    self[len] = 0;
    pid_t c = fork();
    if (c == 0) {
        execl(self, self, "plain execve", (char *)NULL);
        _exit(127);
    }
    waitpid(c, NULL, 0);
    c = fork();
    if (c == 0) {
        execl("/tmp/small-suid", "/tmp/small-suid", "setuid execve", (char *)NULL);
        _exit(127);
    }
    waitpid(c, NULL, 0);
    return 0;
}
C
gcc -O1 -o small small.c
cp small small-suid
chmod 4755 small-suid
./small

# The fourth column (the mount's root) of a bind's mountinfo line, once its source is removed
root() {
    awk -v p="$1" '$5 == p { print $4 }' /proc/self/mountinfo
}
mkdir bd bdt
mount --bind bd bdt
rmdir bd
echo "directory: $(root /tmp/bdt)"
echo x > bf
touch bft
mount --bind bf bft
rm bf
echo "file: $(root /tmp/bft)"
echo y > hl
ln hl hl2
touch hlt
mount --bind hl hlt
rm hl
echo "one of two links: $(root /tmp/hlt), still read: $(cat hlt)"
umount bdt bft hlt

# POSIX timers on the CPU clock of a thread or process that ends (the tenth map-bug triage): the
# timer of an exited thread or a reaped process reads as all zeros and cannot be set (ESRCH), and
# does not follow a process that takes the number over; a zombie's clock still counts as there.
# Clock ids name pids as the caller's pid namespace numbers them, so it all repeats in a new one.
cat > deadclock.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *where;
static int go[2];
static volatile int started;

static void *idle(void *arg)
{
    (void)arg;
    started = 1;
    char x;
    read(go[0], &x, 1);
    return NULL;
}

static void show(const char *what, timer_t t)
{
    struct itimerspec v;
    if (timer_gettime(t, &v) != 0) {
        printf("%s, %s: gettime %s\n", where, what, strerror(errno));
        return;
    }
    printf("%s, %s: interval %ld.%09ld, %s\n", where, what, (long)v.it_interval.tv_sec,
           v.it_interval.tv_nsec, v.it_value.tv_sec || v.it_value.tv_nsec ? "armed" : "disarmed");
}

static void set(const char *what, timer_t t)
{
    struct itimerspec v = {{1, 0}, {100, 0}};
    printf("%s, %s: settime %s\n", where, what,
           timer_settime(t, 0, &v, NULL) == 0 ? "ok" : strerror(errno));
}

static timer_t make(clockid_t c)
{
    struct sigevent ev = {.sigev_notify = SIGEV_NONE};
    timer_t t = 0;
    if (timer_create(c, &ev, &t) != 0)
        printf("%s: timer_create %s\n", where, strerror(errno));
    return t;
}

static void thread_clock(void)
{
    pipe(go);
    started = 0;
    pthread_t th;
    pthread_create(&th, NULL, idle, NULL);
    while (!started)
        usleep(1000);
    clockid_t c;
    pthread_getcpuclockid(th, &c);
    struct timespec now;
    printf("%s, thread: clock %s\n", where, clock_gettime(c, &now) == 0 ? "read" : strerror(errno));
    timer_t t = make(c);
    set("thread", t);
    show("thread", t);
    write(go[1], "x", 1);
    pthread_join(th, NULL);
    // The clock goes once the thread has left the kernel
    for (int i = 0; i < 3000 && clock_gettime(c, &now) == 0; i++)
        usleep(1000);
    show("exited thread", t);
    set("exited thread", t);
    printf("%s, exited thread: delete %s\n", where, timer_delete(t) == 0 ? "ok" : strerror(errno));
    close(go[0]);
    close(go[1]);
}

static void process_clock(void)
{
    pid_t p = fork();
    if (p == 0) {
        pause();
        _exit(0);
    }
    clockid_t c;
    int e = clock_getcpuclockid(p, &c);
    printf("%s, child: clock %s\n", where, e ? strerror(e) : "found");
    timer_t t = make(c);
    set("child", t);
    kill(p, SIGKILL);
    siginfo_t si;
    waitid(P_PID, p, &si, WEXITED | WNOWAIT);
    show("zombie", t);
    set("zombie", t);
    waitpid(p, NULL, 0);
    show("reaped", t);
    set("reaped", t);
    // A new process with the same number
    int f = open("/proc/sys/kernel/ns_last_pid", O_WRONLY);
    dprintf(f, "%d", p - 1);
    close(f);
    pid_t q = fork();
    if (q == 0) {
        pause();
        _exit(0);
    }
    printf("%s, new child: %s number\n", where, q == p ? "same" : "another");
    show("new child", t);
    set("new child", t);
    kill(q, SIGKILL);
    waitpid(q, NULL, 0);
    timer_delete(t);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    where = "here";
    thread_clock();
    process_clock();
    if (unshare(CLONE_NEWPID) != 0) {
        perror("unshare");
        return 1;
    }
    pid_t p = fork();
    if (p == 0) {
        where = "pid namespace";
        thread_clock();
        process_clock();
        _exit(0);
    }
    waitpid(p, NULL, 0);
    return 0;
}
C
gcc -O1 -pthread -o deadclock deadclock.c
./deadclock
rm -f deadclock deadclock.c
cat > getfilter.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define SO_GET_FILTER 26
#define SO_DETACH_FILTER 27
#define SO_LOCK_FILTER 44
#define LEN 40

struct insn {
    unsigned short code;
    unsigned char jt, jf;
    unsigned int k;
};
struct fprog {
    unsigned short len;
    struct insn *filter;
};

static struct insn prog[LEN];

/* getsockopt(SO_GET_FILTER) with optlen `len` (in instructions) into a buffer twice the program's
 * size, filled with a marker: optlen afterwards, how many instructions match the program and
 * whether the first one past them is untouched */
static void get(int s, const char *what, socklen_t len)
{
    struct insn buf[2 * LEN];
    memset(buf, 0xee, sizeof buf);
    socklen_t l = len;
    if (getsockopt(s, SOL_SOCKET, SO_GET_FILTER, buf, &l) < 0) {
        printf("%s: %s\n", what, strerror(errno));
        return;
    }
    int same = 0;
    while (same < LEN && memcmp(&buf[same], &prog[same], sizeof *buf) == 0)
        same++;
    unsigned char *next = (unsigned char *)&buf[l < 2 * LEN ? l : 2 * LEN - 1];
    printf("%s: optlen %u, %d instructions match, next untouched %d\n", what, (unsigned)l, same,
           next[0] == 0xee && next[7] == 0xee);
}

int main(void)
{
    /* 39 loads of a constant, then accept the packet */
    for (int i = 0; i < LEN - 1; i++)
        prog[i] = (struct insn){0x00, 0, 0, (unsigned)i};
    prog[LEN - 1] = (struct insn){0x06, 0, 0, 0xffff};
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    get(s, "no filter, count", 0);
    get(s, "no filter, room for 10", 10);
    struct fprog fp = {LEN, prog};
    printf("attach: %d\n", setsockopt(s, SOL_SOCKET, SO_ATTACH_FILTER, &fp, sizeof fp));
    get(s, "count", 0);
    get(s, "one short", LEN - 1);
    get(s, "exact", LEN);
    get(s, "twice", 2 * LEN);
    int zero = 0, one = 1;
    printf("detach: %d\n", setsockopt(s, SOL_SOCKET, SO_DETACH_FILTER, &zero, sizeof zero));
    get(s, "detached", 2 * LEN);
    close(s);

    /* TCP, which a host with Linux 5d39580f68e6 refuses to the app (Tuxport keeps the program) */
    int t = socket(AF_INET, SOCK_STREAM, 0);
    printf("tcp attach: %d\n", setsockopt(t, SOL_SOCKET, SO_ATTACH_FILTER, &fp, sizeof fp));
    get(t, "tcp count", 0);
    get(t, "tcp one short", LEN - 1);
    get(t, "tcp exact", LEN);
    int r = setsockopt(t, SOL_SOCKET, SO_ATTACH_FILTER, &fp, sizeof fp - 1);
    printf("tcp optlen 15: %s\n", r ? strerror(errno) : "0");
    struct fprog none = {LEN, NULL};
    r = setsockopt(t, SOL_SOCKET, SO_ATTACH_FILTER, &none, sizeof none);
    printf("tcp no program: %s\n", r ? strerror(errno) : "0");
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t al = sizeof a;
    bind(t, (struct sockaddr *)&a, sizeof a);
    getsockname(t, (struct sockaddr *)&a, &al);
    listen(t, 1);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    printf("tcp connect: %d\n", connect(c, (struct sockaddr *)&a, sizeof a));
    int n = accept(t, NULL, NULL);
    get(n, "tcp accepted", LEN);
    get(c, "tcp client", LEN);
    printf("tcp detach: %d\n", setsockopt(n, SOL_SOCKET, SO_DETACH_FILTER, &zero, sizeof zero));
    r = setsockopt(n, SOL_SOCKET, SO_DETACH_FILTER, &zero, sizeof zero);
    printf("tcp detach again: %s\n", r ? strerror(errno) : "0");
    printf("tcp lock: %d\n", setsockopt(t, SOL_SOCKET, SO_LOCK_FILTER, &one, sizeof one));
    r = setsockopt(t, SOL_SOCKET, SO_ATTACH_FILTER, &fp, sizeof fp);
    printf("tcp attach locked: %s\n", r ? strerror(errno) : "0");
    r = setsockopt(t, SOL_SOCKET, SO_DETACH_FILTER, &zero, sizeof zero);
    printf("tcp detach locked: %s\n", r ? strerror(errno) : "0");
    get(t, "tcp locked", LEN);
    return 0;
}
C
gcc -O1 -o getfilter getfilter.c
./getfilter
rm -f getfilter getfilter.c

cat > procsyscall.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int fds[2];
static char tbuf[16];
static volatile pid_t reader_tid, spinner_tid;
static volatile int spin = 1;

/* A line of /proc/.../syscall: the number and the other fields (n counts them all), or the
   error at open or read */
struct line {
    int n;
    long long nr;
    unsigned long long v[8];
    char err[64];
};

static struct line parse(const char *text)
{
    struct line l = {0};
    if (strcmp(text, "running\n") == 0) {
        strcpy(l.err, "running");
        return l;
    }
    l.n = sscanf(text, "%lld %llx %llx %llx %llx %llx %llx %llx %llx", &l.nr, &l.v[0], &l.v[1],
                 &l.v[2], &l.v[3], &l.v[4], &l.v[5], &l.v[6], &l.v[7]);
    return l;
}

static struct line get(const char *path)
{
    struct line l = {0};
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        snprintf(l.err, sizeof l.err, "open: %s", strerror(errno));
        return l;
    }
    char text[256] = {0};
    ssize_t r = read(fd, text, sizeof text - 1);
    int e = errno;
    close(fd);
    if (r < 0) {
        snprintf(l.err, sizeof l.err, "read: %s", strerror(e));
        return l;
    }
    return parse(text);
}

/* Waits until the state in the stat file at path is s */
static void wait_state(const char *path, char s)
{
    for (int i = 0; i < 500; i++) {
        char b[512] = {0};
        int fd = open(path, O_RDONLY);
        if (fd >= 0) {
            read(fd, b, sizeof b - 1);
            close(fd);
        }
        char *p = strrchr(b, ')');
        if (p && p[2] == s)
            return;
        usleep(10000);
    }
    printf("%s never became %c\n", path, s);
}

static void *reader(void *a)
{
    (void)a;
    reader_tid = gettid();
    read(fds[0], tbuf, 7);
    return 0;
}

static void *spinner(void *a)
{
    (void)a;
    spinner_tid = gettid();
    while (spin)
        ;
    return 0;
}

int main(void)
{
    char path[64], stat_path[64];
    struct stat st;
    stat("/proc/self/syscall", &st);
    printf("mode %o\n", (unsigned)st.st_mode);

    /* The reader itself is in its read: the descriptor, buffer and count, sp on its stack */
    int local = 0;
    int fd = open("/proc/self/syscall", O_RDONLY);
    char text[256] = {0};
    ssize_t r = read(fd, text, 200);
    close(fd);
    struct line l = parse(text);
    unsigned long long sp = l.v[6];
    printf("self: %d, %d fields, nr %lld, fd %d, buffer %d, count %llu, sp near %d\n", r > 0, l.n,
           l.nr, l.v[0] == (unsigned)fd, l.v[1] == (unsigned long)text, l.v[2],
           sp < (unsigned long)&local && (unsigned long)&local - sp < 65536);

    /* A thread waiting in read on a pipe, in the process directory's task entry */
    pipe(fds);
    pthread_t t;
    pthread_create(&t, 0, reader, 0);
    while (!reader_tid)
        usleep(1000);
    snprintf(stat_path, sizeof stat_path, "/proc/self/task/%d/stat", reader_tid);
    wait_state(stat_path, 'S');
    snprintf(path, sizeof path, "/proc/self/task/%d/syscall", reader_tid);
    l = get(path);
    printf("waiting thread: %d fields, nr %lld, fd %d, buffer %d, count %llu, sp %d, pc %d\n", l.n,
           l.nr, l.v[0] == (unsigned)fds[0], l.v[1] == (unsigned long)tbuf, l.v[2], l.v[6] != 0,
           l.v[7] != 0);
    write(fds[1], "go", 2);
    pthread_join(t, 0);

    /* A thread running guest code */
    pthread_create(&t, 0, spinner, 0);
    while (!spinner_tid)
        usleep(1000);
    snprintf(path, sizeof path, "/proc/self/task/%d/syscall", spinner_tid);
    l = get(path);
    printf("spinning thread: %s\n", l.err);
    int gone = open(path, O_RDONLY);
    spin = 0;
    pthread_join(t, 0);
    /* Once the thread has gone (Linux releases it at once), its open file finds nothing */
    snprintf(stat_path, sizeof stat_path, "/proc/self/task/%d", spinner_tid);
    for (int i = 0; i < 500 && access(stat_path, F_OK) == 0; i++)
        usleep(10000);
    r = read(gone, text, 200);
    printf("thread gone: %s\n", r < 0 ? strerror(errno) : "read");
    close(gone);

    /* A child waiting in read, then stopped by SIGSTOP: signal delivery forgets the syscall
       (forget_syscall), and the pc goes back to the svc to restart it */
    int p2[2];
    pipe(p2);
    pid_t c = fork();
    if (c == 0) {
        char b[4];
        read(p2[0], b, 3);
        _exit(0);
    }
    snprintf(stat_path, sizeof stat_path, "/proc/%d/stat", c);
    wait_state(stat_path, 'S');
    snprintf(path, sizeof path, "/proc/%d/syscall", c);
    struct line before = get(path);
    kill(c, SIGSTOP);
    int status;
    waitpid(c, &status, WUNTRACED);
    struct line after = get(path);
    printf("child waiting: %d fields, nr %lld, count %llu\n", before.n, before.nr, before.v[2]);
    printf("child stopped: %d fields, nr %lld, same sp %d, pc back %lld\n", after.n, after.nr,
           after.v[0] == before.v[6], (long long)(before.v[7] - after.v[1]));
    kill(c, SIGCONT);
    kill(c, SIGKILL);
    waitpid(c, &status, 0);

    /* A zombie child has no stack left */
    c = fork();
    if (c == 0)
        _exit(0);
    snprintf(stat_path, sizeof stat_path, "/proc/%d/stat", c);
    wait_state(stat_path, 'Z');
    snprintf(path, sizeof path, "/proc/%d/syscall", c);
    l = get(path);
    printf("zombie: %s%d fields, nr %lld, sp %llx, pc %llx\n", l.err, l.n, l.nr, l.v[0], l.v[1]);
    waitpid(c, &status, 0);

    /* Access is checked when read, with the reader's own credentials: a descriptor root opened
       on its own syscall file is EPERM to read once the reader is nobody */
    fd = open("/proc/self/syscall", O_RDONLY);
    fflush(stdout);
    c = fork();
    if (c == 0) {
        setuid(65534);
        r = read(fd, text, 200);
        printf("read as nobody: %s\n", r < 0 ? strerror(errno) : "read");
        l = get("/proc/1/syscall");
        printf("open as nobody: %s\n", l.err);
        fflush(stdout);
        _exit(0);
    }
    waitpid(c, &status, 0);
    r = read(fd, text, 200);
    printf("read as root: %s\n", r > 0 ? "read" : strerror(errno));
    close(fd);
    return 0;
}
C
gcc -O1 -pthread -o procsyscall procsyscall.c
./procsyscall
rm -f procsyscall procsyscall.c

cat > tidfiles.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static pid_t tid;
static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t c = PTHREAD_COND_INITIALIZER;
static int done;

static void *thread(void *a)
{
    pthread_mutex_lock(&m);
    tid = syscall(SYS_gettid);
    pthread_cond_broadcast(&c);
    while (!done)
        pthread_cond_wait(&c, &m);
    pthread_mutex_unlock(&m);
    return a;
}

/* Whether a file opens and reads something, or the error */
static void first(const char *what, const char *path)
{
    char buf[256];
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        printf("%s: open %s\n", what, strerror(errno));
        return;
    }
    ssize_t n = read(fd, buf, sizeof buf);
    close(fd);
    if (n < 0)
        printf("%s: read %s\n", what, strerror(errno));
    else
        printf("%s: read %s\n", what, n > 0 ? "something" : "nothing");
}

int main(void)
{
    pthread_t t;
    pthread_create(&t, NULL, thread, NULL);
    pthread_mutex_lock(&m);
    while (!tid)
        pthread_cond_wait(&c, &m);
    pthread_mutex_unlock(&m);
    /* Through /proc/<tid>, a thread other than the main one: its process's address space */
    const char *names[] = {"maps", "smaps", "smaps_rollup", "auxv", "environ", "pagemap", "mem"};
    for (size_t i = 0; i < sizeof names / sizeof *names; i++) {
        char path[64];
        snprintf(path, sizeof path, "/proc/%d/%s", tid, names[i]);
        if (i >= 5) {
            int fd = open(path, O_RDONLY);
            printf("/proc/<tid>/%s: open %s\n", names[i], fd < 0 ? strerror(errno) : "ok");
            close(fd);
        } else {
            char what[64];
            snprintf(what, sizeof what, "/proc/<tid>/%s", names[i]);
            first(what, path);
        }
    }
    char a[64], b[64], la[256] = "", lb[256] = "";
    snprintf(a, sizeof a, "/proc/%d/maps", tid);
    snprintf(b, sizeof b, "/proc/%d/maps", getpid());
    FILE *fa = fopen(a, "r"), *fb = fopen(b, "r");
    if (fa && fb && fgets(la, sizeof la, fa) && fgets(lb, sizeof lb, fb))
        printf("maps first line as the process's: %d\n", strcmp(la, lb) == 0);
    pthread_mutex_lock(&m);
    done = 1;
    pthread_cond_broadcast(&c);
    pthread_mutex_unlock(&m);
    pthread_join(t, NULL);
    return 0;
}
C
gcc -O1 -pthread -o tidfiles tidfiles.c
./tidfiles
rm -f tidfiles tidfiles.c
