# The sequel to compat3.sh (difftest passes the whole script as one argument of sh -c, which must
# not exceed 128 KB): MADV_DONTNEED on private file mappings; signal frames that cannot be written
# and bad rt_sigreturn frames; truncating a file while its tail is read through a mapping; wait by
# exit signal and for stops of zombies; nested #! scripts; rt_sigqueueinfo to a zombie; restarting
# blocked message-queue calls; new files' modes; accept4's order and inherited options; socket
# timeouts; files of an exited main thread
set -e
cd /tmp
# MADV_DONTNEED on a private file mapping drops the written copies: the pages read the file again
# (zeros past its end in the last page), also for a mapping made writable after being read-only, in
# a forked child, and for a range starting inside a page run
cat > dontneedfile.c <<'C'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>
#include <fcntl.h>

static void show(const char *what, const unsigned char *p, size_t len) {
    size_t a = 0, b = 0, z = 0, other = 0;
    for (size_t i = 0; i < len; i++) {
        if (p[i] == 'a') a++;
        else if (p[i] == 'b') b++;
        else if (p[i] == 0) z++;
        else other++;
    }
    printf("%s: a=%zu b=%zu zero=%zu other=%zu\n", what, a, b, z, other);
}

int main(void) {
    // 3 pages and a bit: the last page holds 100 bytes of the file
    size_t flen = 3 * 4096 + 100, mlen = 4 * 4096;
    int fd = open("dnfile", O_RDWR | O_CREAT | O_TRUNC, 0644);
    unsigned char *buf = malloc(flen);
    memset(buf, 'a', flen);
    if (write(fd, buf, flen) != (ssize_t)flen) return 1;
    // Old enough that a read-only mapping may share a cached host mapping
    sleep(3);

    unsigned char *p = mmap(NULL, mlen, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    memset(p, 'b', mlen);
    show("written", p, mlen);
    printf("dontneed %d\n", madvise(p, mlen, MADV_DONTNEED));
    show("after", p, mlen);
    // Only a middle part
    memset(p, 'b', mlen);
    printf("dontneed middle %d\n", madvise(p + 4096, 4096, MADV_DONTNEED));
    show("page 0", p, 4096);
    show("page 1", p + 4096, 4096);
    show("page 2", p + 8192, 4096);

    // Read-only first, then writable
    unsigned char *q = mmap(NULL, mlen, PROT_READ, MAP_PRIVATE, fd, 0);
    show("read-only", q, mlen);
    mprotect(q, mlen, PROT_READ | PROT_WRITE);
    memset(q, 'b', mlen);
    printf("dontneed %d\n", madvise(q, mlen, MADV_DONTNEED));
    show("made writable", q, mlen);

    // In a forked child, the child's copies go and the parent's stay
    memset(p, 'b', mlen);
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        printf("child dontneed %d\n", madvise(p, mlen, MADV_DONTNEED));
        show("child", p, mlen);
        fflush(stdout);
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    show("parent", p, mlen);

    // The file changed: the dropped pages show the new contents
    memset(p, 'b', mlen);
    pwrite(fd, "bbbb", 4, 0);
    madvise(p, 4096, MADV_DONTNEED);
    printf("first bytes %.6s\n", p);
    return 0;
}
C
gcc -O1 -o dontneedfile dontneedfile.c && ./dontneedfile
# A signal whose frame cannot be written (its alternate stack is read-only) forces a SIGSEGV
# (SI_KERNEL) that a handler may catch; one for SIGSEGV itself ends the process with SIGSEGV. A bad
# rt_sigreturn frame returns 0 and forces a SIGSEGV at the stack pointer (SEGV_ACCERR below a
# mapping)
cat > badframe.c <<'C'
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

static sigjmp_buf back;
static volatile int usr1;

static void on_segv(int sig, siginfo_t *si, void *uc) {
    (void)uc;
    printf("SIGSEGV handler: sig %d code %d addr %s\n", sig, si->si_code,
           si->si_code == SI_KERNEL ? "-" : "set");
    fflush(stdout);
    siglongjmp(back, 1);
}

static void on_usr1(int sig) {
    (void)sig;
    usr1 = 1;
}

static void bad_sigreturn(void *sp) {
    __asm__ volatile("mov x1, sp\n\tmov sp, %0\n\tmov x8, #139\n\tsvc #0\n\tmov sp, x1"
                     :
                     : "r"(sp)
                     : "x0", "x1", "x8", "memory");
}

static void *ro_stack(size_t len) {
    return mmap(NULL, len, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
}

int main(void) {
    size_t len = 64 * 1024;
    stack_t ss = {.ss_sp = ro_stack(len), .ss_size = len};
    sigaltstack(&ss, NULL);
    struct sigaction sa = {0};
    sa.sa_handler = on_usr1;
    sa.sa_flags = SA_ONSTACK;
    sigaction(SIGUSR1, &sa, NULL);
    struct sigaction segv = {0};
    segv.sa_sigaction = on_segv;
    segv.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &segv, NULL);
    if (sigsetjmp(back, 1) == 0) {
        raise(SIGUSR1);
        printf("not reached\n");
    }
    printf("after the forced SIGSEGV: usr1 ran %d\n", usr1);

    // A bad frame at rt_sigreturn: SIGSEGV handled on a good alternate stack
    stack_t good = {.ss_sp = malloc(len), .ss_size = len};
    sigaltstack(&good, NULL);
    segv.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigaction(SIGSEGV, &segv, NULL);
    void *none = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (sigsetjmp(back, 1) == 0) {
        bad_sigreturn(none);
        printf("not reached\n");
    }
    printf("after the bad sigreturn\n");
    fflush(stdout);

    // SIGSEGV's own frame cannot be written either: the process ends with SIGSEGV
    pid_t pid = fork();
    if (pid == 0) {
        struct rlimit no = {0, 0};
        setrlimit(RLIMIT_CORE, &no);
        stack_t bad = {.ss_sp = ro_stack(len), .ss_size = len};
        sigaltstack(&bad, NULL);
        raise(SIGUSR1);
        printf("child not reached\n");
        _exit(0);
    }
    int st;
    waitpid(pid, &st, 0);
    printf("child: signaled %d sig %d\n", WIFSIGNALED(st), WIFSIGNALED(st) ? WTERMSIG(st) : 0);
    return 0;
}
C
gcc -O1 -o badframe badframe.c && ./badframe
# Cutting a file while another thread reads its mapped tail: the reader gets SIGBUS, never a dead
# process (the pages past the new end are unmapped before the file shrinks)
cat > truncrace.c <<'C'
#include <fcntl.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static __thread sigjmp_buf back;
static volatile unsigned char *tail;
static volatile int started;

static void on_bus(int sig) {
    (void)sig;
    siglongjmp(back, 1);
}

static void *reader(void *arg) {
    (void)arg;
    unsigned sum = 0;
    if (sigsetjmp(back, 1) == 0) {
        started = 1;
        for (;;) sum += tail[0];
    }
    return (void *)1;
}

int main(void) {
    struct sigaction sa = {0};
    sa.sa_handler = on_bus;
    sigaction(SIGBUS, &sa, NULL);
    int bus = 0;
    for (int round = 0; round < 200; round++) {
        int fd = open("truncfile", O_RDWR | O_CREAT | O_TRUNC, 0644);
        ftruncate(fd, 2 * 4096);
        unsigned char *p = mmap(NULL, 2 * 4096, PROT_READ, round % 2 ? MAP_PRIVATE : MAP_SHARED, fd, 0);
        tail = p + 4096;
        started = 0;
        pthread_t t;
        pthread_create(&t, NULL, reader, NULL);
        while (!started) {
        }
        ftruncate(fd, 4096);
        void *r;
        pthread_join(t, &r);
        bus += r == (void *)1;
        munmap(p, 2 * 4096);
        close(fd);
    }
    printf("readers ended by SIGBUS: %d of 200\n", bus);
    return 0;
}
C
gcc -O1 -pthread -o truncrace truncrace.c && ./truncrace
# Waiting: only children reporting with SIGCHLD without __WCLONE, only the others with it, all with
# __WALL; a wait for stops and continues alone is ECHILD once every child has exited. Five nested
# #! scripts run, a sixth is ELOOP
cat > waitkinds.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int quit(void *arg) {
    (void)arg;
    return 3;
}

static pid_t spawn(int exit_signal) {
    char *stack = malloc(65536);
    return clone(quit, stack + 65536, exit_signal, NULL);
}

static const char *res(pid_t r, pid_t want) {
    if (r == want) return "reaped";
    if (r == 0) return "none";
    return strerror(errno);
}

static void script(int n, int depth) {
    char name[32], next[40];
    for (int i = 1; i <= depth; i++) {
        snprintf(name, sizeof name, "/tmp/nest%d-%d", n, i);
        FILE *f = fopen(name, "w");
        if (i < depth) snprintf(next, sizeof next, "#!/tmp/nest%d-%d\n", n, i + 1);
        else snprintf(next, sizeof next, "#!/bin/sh\necho ran\n");
        fputs(next, f);
        fclose(f);
        chmod(name, 0755);
    }
    snprintf(name, sizeof name, "/tmp/nest%d-1", n);
    pid_t pid = fork();
    if (pid == 0) {
        execl(name, name, (char *)NULL);
        printf("%d scripts: %s\n", depth, strerror(errno));
        fflush(stdout);
        _exit(1);
    }
    waitpid(pid, NULL, 0);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    signal(SIGUSR2, SIG_IGN);
    pid_t quiet = spawn(0), usr = spawn(SIGUSR2), chld = spawn(SIGCHLD);
    usleep(200000);
    pid_t r = waitpid(quiet, NULL, 0);
    printf("waitpid(signal 0 child): %s\n", res(r, quiet));
    r = waitpid(quiet, NULL, __WCLONE);
    printf("waitpid(signal 0 child, __WCLONE): %s\n", res(r, quiet));
    r = waitpid(chld, NULL, __WCLONE);
    printf("waitpid(SIGCHLD child, __WCLONE): %s\n", res(r, chld));
    r = waitpid(-1, NULL, 0);
    printf("waitpid(-1): %s\n", res(r, chld));
    r = waitpid(-1, NULL, WNOHANG);
    printf("waitpid(-1) again: %s\n", r == 0 ? "none" : strerror(errno));
    r = waitpid(-1, NULL, __WALL);
    printf("waitpid(-1, __WALL): %s\n", r == usr ? "reaped" : strerror(errno));

    pid_t z = fork();
    if (z == 0) _exit(0);
    usleep(200000);
    siginfo_t si;
    int w = waitid(P_ALL, 0, &si, WSTOPPED | WCONTINUED | WNOHANG);
    printf("waitid(stops) with only a zombie: %s\n", w == 0 ? "0" : strerror(errno));
    // A kernel that waits here for good is ended by the alarm rather than holding the case
    alarm(5);
    w = waitid(P_ALL, 0, &si, WSTOPPED);
    alarm(0);
    printf("waitid(stops) blocking: %s\n", w == 0 ? "0" : strerror(errno));
    w = waitid(P_PID, z, &si, WEXITED);
    printf("waitid(exits): %d %d\n", w, si.si_pid == z);

    script(1, 5);
    script(2, 6);
    return 0;
}
C
gcc -O1 -o waitkinds waitkinds.c && ./waitkinds
# rt_sigqueueinfo to a child that has exited but not been reaped succeeds, as kill does
cat > sigqueuezombie.c <<'C'
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    pid_t pid = fork();
    if (pid == 0) _exit(0);
    usleep(200000);
    union sigval v = {.sival_int = 1};
    printf("sigqueue to a zombie: %s\n", sigqueue(pid, SIGUSR1, v) == 0 ? "0" : strerror(errno));
    printf("sigqueue 0 to a zombie: %s\n", sigqueue(pid, 0, v) == 0 ? "0" : strerror(errno));
    printf("kill 0 to a zombie: %s\n", kill(pid, 0) == 0 ? "0" : strerror(errno));
    waitpid(pid, NULL, 0);
    printf("sigqueue after reaping: %s\n", sigqueue(pid, 0, v) == 0 ? "0" : strerror(errno));
    return 0;
}
C
gcc -O1 -o sigqueuezombie sigqueuezombie.c && ./sigqueuezombie
# A blocked mq_timedreceive or mq_timedsend restarts after a handler with SA_RESTART and returns
# EINTR without it
cat > mqrestart.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static mqd_t q;
static volatile int handled;
static void on_usr1(int sig) { (void)sig; handled++; }

static void *poke(void *arg) {
    pthread_t main_thread = *(pthread_t *)arg;
    usleep(200000);
    pthread_kill(main_thread, SIGUSR1);
    usleep(200000);
    mq_send(q, "x", 1, 0);
    return NULL;
}

static void receive(int flags, const char *what) {
    struct sigaction sa = {0};
    sa.sa_handler = on_usr1;
    sa.sa_flags = flags;
    sigaction(SIGUSR1, &sa, NULL);
    pthread_t self = pthread_self(), t;
    pthread_create(&t, NULL, poke, &self);
    char buf[16];
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    ts.tv_sec += 5;
    ssize_t n = mq_timedreceive(q, buf, sizeof buf, NULL, &ts);
    printf("%s: %zd %s, handled %d\n", what, n, n < 0 ? strerror(errno) : "", handled);
    pthread_join(t, NULL);
    // Without SA_RESTART the message sent after the signal is still queued: take it
    struct mq_attr a;
    mq_getattr(q, &a);
    if (a.mq_curmsgs) mq_receive(q, buf, sizeof buf, NULL);
}

int main(void) {
    struct mq_attr attr = {.mq_maxmsg = 1, .mq_msgsize = 16};
    mq_unlink("/restart");
    q = mq_open("/restart", O_CREAT | O_RDWR, 0600, &attr);
    if (q == (mqd_t)-1) {
        printf("mq_open: %s\n", strerror(errno));
        return 0;
    }
    receive(SA_RESTART, "receive with SA_RESTART");
    handled = 0;
    receive(0, "receive without SA_RESTART");
    mq_unlink("/restart");
    return 0;
}
C
gcc -O1 -pthread -o mqrestart mqrestart.c && ./mqrestart
# New files' modes: in a setgid directory of another group, a group-executable setgid file made by a
# user outside the group loses the bit before the umask applies; mkdir keeps only permissions and
# the sticky bit, plus the setgid bit of a setgid parent
cat > newmodes.c <<'C'
#include <fcntl.h>
#include <grp.h>
#include <stdio.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static void show(const char *path) {
    struct stat st;
    if (stat(path, &st) == 0) printf("%s %04o gid %u\n", path, st.st_mode & 07777, st.st_gid);
    else perror(path);
}

int main(void) {
    mkdir("/tmp/sgid", 0);
    chown("/tmp/sgid", 0, 1234);
    chmod("/tmp/sgid", 02777);
    mkdir("/tmp/plain", 0777);
    chmod("/tmp/plain", 0777);
    pid_t pid = fork();
    if (pid == 0) {
        setgroups(0, NULL);
        if (setgid(4321) || setuid(4321)) return 1;
        umask(022);
        close(open("/tmp/sgid/f022", O_CREAT | O_WRONLY, 02755));
        umask(010);
        close(open("/tmp/sgid/f010", O_CREAT | O_WRONLY, 02755));
        umask(077);
        close(open("/tmp/sgid/f077", O_CREAT | O_WRONLY, 02755));
        mknod("/tmp/sgid/fifo", S_IFIFO | 02775, 0);
        umask(0);
        close(open("/tmp/sgid/f2745", O_CREAT | O_WRONLY, 02745));
        mkdir("/tmp/sgid/d", 06777);
        mkdir("/tmp/plain/d", 07777);
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    const char *paths[] = {"/tmp/sgid/f022", "/tmp/sgid/f010", "/tmp/sgid/f077", "/tmp/sgid/fifo",
                           "/tmp/sgid/f2745", "/tmp/sgid/d", "/tmp/plain/d"};
    for (unsigned i = 0; i < sizeof paths / sizeof *paths; i++) show(paths[i]);
    return 0;
}
C
rm -rf /tmp/sgid /tmp/plain
gcc -O1 -o newmodes newmodes.c && ./newmodes
# Other groups run later in the same Tuxport store (compat's file-capability case copies a program
# to /tmp/plain)
rm -rf /tmp/sgid /tmp/plain
# accept4: no free fd is EMFILE with the connection still queued; a bad address drops the
# connection without leaving an fd; accepted TCP sockets keep the listener's timeouts and AF_UNIX
# ones its SO_PASSCRED. Socket timeouts: microseconds outside a second are EDOM, a short value
# EINVAL, a negative time means not waiting
cat > acceptorder.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

static double now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static int client(int port) {
    int c = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    connect(c, (struct sockaddr *)&a, sizeof a);
    return c;
}

int main(void) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET};
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    bind(l, (struct sockaddr *)&a, sizeof a);
    socklen_t len = sizeof a;
    getsockname(l, (struct sockaddr *)&a, &len);
    int port = ntohs(a.sin_port);
    listen(l, 8);

    struct timeval tv = {0, 1000000};
    printf("usec 1000000: %s\n", setsockopt(l, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv) ? strerror(errno) : "ok");
    tv.tv_usec = -1;
    printf("usec -1: %s\n", setsockopt(l, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv) ? strerror(errno) : "ok");
    printf("short value: %s\n", setsockopt(l, SOL_SOCKET, SO_RCVTIMEO, &tv, 8) ? strerror(errno) : "ok");
    tv.tv_sec = 0;
    tv.tv_usec = 300000;
    setsockopt(l, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);

    // No free fd: EMFILE, and the connection waits in the queue
    int c1 = client(port);
    struct rlimit rl;
    getrlimit(RLIMIT_NOFILE, &rl);
    struct rlimit tight = {c1 + 1, rl.rlim_max};
    setrlimit(RLIMIT_NOFILE, &tight);
    int r = accept(l, NULL, NULL);
    printf("accept without a free fd: %s\n", r < 0 ? strerror(errno) : "fd");
    setrlimit(RLIMIT_NOFILE, &rl);
    int s1 = accept(l, NULL, NULL);
    printf("accept afterwards: %s\n", s1 >= 0 ? "the queued connection" : strerror(errno));

    // The accepted socket keeps the listener's receive timeout
    struct timeval got;
    socklen_t gl = sizeof got;
    getsockopt(s1, SOL_SOCKET, SO_RCVTIMEO, &got, &gl);
    printf("accepted SO_RCVTIMEO: %ld.%06ld\n", (long)got.tv_sec, (long)got.tv_usec);
    char b[8];
    double t0 = now();
    ssize_t n = recv(s1, b, sizeof b, 0);
    printf("recv: %zd %s after %s\n", n, n < 0 ? strerror(errno) : "", now() - t0 > 0.2 ? "the timeout" : "less");

    // A negative timeout: no waiting
    tv.tv_sec = -1;
    tv.tv_usec = 0;
    setsockopt(s1, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    t0 = now();
    n = recv(s1, b, sizeof b, 0);
    printf("recv with a negative timeout: %zd %s %s\n", n, n < 0 ? strerror(errno) : "", now() - t0 < 0.2 ? "at once" : "late");

    // A bad address pointer: EFAULT, no fd left behind, the connection dropped
    int c2 = client(port);
    usleep(100000);
    int before = dup(0);
    close(before);
    len = sizeof a;
    r = accept(l, (struct sockaddr *)8, &len);
    printf("accept with a bad address: %s\n", r < 0 ? strerror(errno) : "fd");
    int after = dup(0);
    printf("lowest free fd unchanged: %s\n", after == before ? "yes" : "no");
    close(after);
    tv.tv_sec = 2;
    setsockopt(c2, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    n = recv(c2, b, sizeof b, 0);
    printf("client of the dropped connection: %s\n", n == 0 ? "end of file" : n < 0 ? strerror(errno) : "data");

    // AF_UNIX: SO_PASSCRED on the listener carries over to accepted sockets
    int ul = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un ua = {.sun_family = AF_UNIX};
    strcpy(ua.sun_path + 1, "passcred-accept");
    socklen_t ulen = offsetof(struct sockaddr_un, sun_path) + 1 + strlen("passcred-accept");
    bind(ul, (struct sockaddr *)&ua, ulen);
    listen(ul, 1);
    int on = 1;
    setsockopt(ul, SOL_SOCKET, SO_PASSCRED, &on, sizeof on);
    int uc = socket(AF_UNIX, SOCK_STREAM, 0);
    connect(uc, (struct sockaddr *)&ua, ulen);
    int us = accept(ul, NULL, NULL);
    int pc = 0;
    socklen_t pcl = sizeof pc;
    getsockopt(us, SOL_SOCKET, SO_PASSCRED, &pc, &pcl);
    printf("accepted unix socket SO_PASSCRED: %d\n", pc);
    write(uc, "x", 1);
    char cbuf[CMSG_SPACE(sizeof(struct ucred))];
    struct iovec iov = {b, 1};
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf};
    recvmsg(us, &m, 0);
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    printf("credentials received: %s\n", c && c->cmsg_type == SCM_CREDENTIALS ? "yes" : "no");
    return 0;
}
C
gcc -O1 -o acceptorder acceptorder.c && ./acceptorder
# A main thread that has exited while another runs has no address space or namespaces: pagemap
# and mem are ESRCH to open, mounts and mountinfo EINVAL
cat > zombiefiles.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>
static void *sleeper(void *a) { (void)a; sleep(3); return NULL; }
int main(void) {
    pid_t pid = fork();
    if (pid == 0) { pthread_t t; pthread_create(&t, NULL, sleeper, NULL); pthread_exit(NULL); }
    usleep(300000);
    char path[64]; char buf[64];
    const char *files[] = {"pagemap", "mounts", "mountinfo", "mem"};
    for (int i = 0; i < 4; i++) {
        snprintf(path, sizeof path, "/proc/%d/%s", pid, files[i]);
        int fd = open(path, O_RDONLY);
        if (fd < 0) { printf("%s: open %s\n", files[i], strerror(errno)); continue; }
        ssize_t n = read(fd, buf, sizeof buf);
        printf("%s: read %zd %s\n", files[i], n, n < 0 ? strerror(errno) : "");
        close(fd);
    }
    waitpid(pid, NULL, 0);
    return 0;
}
C
gcc -O1 -pthread -o zombiefiles zombiefiles.c && ./zombiefiles
