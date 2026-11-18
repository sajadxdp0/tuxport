# docker-run-options: --privileged
# The sequel to compat4.sh (difftest passes the whole script as one argument of sh -c, which must
# not exceed 128 KB): write opens of read-only procfs files; who signal-driven I/O may reach;
# ptrace memory requests on processes made non-dumpable; id maps written through another
# process's open file; procfs content regenerated through the opener's credentials; futexes on
# replaced memory, private and shared operations, and shared memory after fork; message-queue
# registrations ended by closing a descriptor; CLONE_INTO_CGROUP permissions; onlink routes;
# nftables limit and quota; O_DIRECT, getdents64, copy_file_range, datagram shutdowns, KCMP_IO,
# sigaction flags and raw rebinds
set -e
cd /tmp
# Root may open read-only procfs files for writing (only the write fails: EIO for the files of proc
# entries, EINVAL for per-process and namespace files), but not /proc/sys or sysfs ones; O_TRUNC
# asks for write permission, so another user cannot use it on a read-only file
cat > procwrite.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void try(const char *path, int flags, const char *how) {
    int fd = open(path, flags);
    if (fd < 0) {
        printf("%s %s: open %s\n", path, how, strerror(errno));
        return;
    }
    if ((flags & O_ACCMODE) == O_RDONLY) {
        printf("%s %s: open ok\n", path, how);
    } else {
        ssize_t n = write(fd, "1\n", 2);
        printf("%s %s: open ok, write %s\n", path, how, n < 0 ? strerror(errno) : "ok");
    }
    close(fd);
}

static void all(void) {
    const char *paths[] = {"/proc/cpuinfo", "/proc/self/status", "/proc/self/net/dev",
                           "/proc/sysvipc/shm", "/proc/self/fdinfo/0", "/proc/self/ns/net",
                           "/proc/sys/kernel/cap_last_cap", "/sys/devices/system/cpu/online"};
    for (unsigned i = 0; i < sizeof paths / sizeof *paths; i++) {
        try(paths[i], O_WRONLY, "write-only");
        try(paths[i], O_RDWR, "read-write");
        try(paths[i], O_RDONLY | O_TRUNC, "truncating");
    }
}

int main(void) {
    printf("root:\n");
    all();
    FILE *f = fopen("/proc/self/status", "r+");
    char line[64] = "";
    if (f && fgets(line, sizeof line, f)) printf("r+ status: %.5s\n", line);
    else printf("r+ status: %s\n", strerror(errno));
    if (f) fclose(f);
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        if (setgid(4321) || setuid(4321)) return 1;
        printf("another user:\n");
        all();
        return 0;
    }
    waitpid(pid, NULL, 0);
    return 0;
}
C
gcc -O1 -o procwrite procwrite.c && ./procwrite
# Signal-driven I/O reaches a process only when whoever set the owner is root or shares a real or
# saved user ID with it (sigio_perm): another user's owner signals nothing, root's and the same
# user's do (SIGIO kills). An owner no pid has is ESRCH (F_SETOWN, a process group, F_SETOWN_EX);
# 0 removes it
cat > sigioperm.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t target(int uid) {
    pid_t t = fork();
    if (t == 0) {
        if (uid && (setgid(uid) || setuid(uid))) _exit(1);
        for (;;) pause();
    }
    return t;
}

// The pipe stays open a while: Linux signals at the write, Tuxport shortly after
static void signal_from(pid_t t, int uid) {
    pid_t p = fork();
    if (p == 0) {
        if (uid && (setgid(uid) || setuid(uid))) _exit(1);
        int fds[2];
        if (pipe(fds)) _exit(1);
        if (fcntl(fds[0], F_SETOWN, t)) {
            printf("F_SETOWN: %s\n", strerror(errno));
            _exit(1);
        }
        fcntl(fds[0], F_SETFL, O_ASYNC);
        write(fds[1], "x", 1);
        usleep(500000);
        _exit(0);
    }
    waitpid(p, NULL, 0);
}

static void report(const char *what, pid_t t) {
    int st;
    usleep(100000);
    if (waitpid(t, &st, WNOHANG) == 0) printf("%s: alive\n", what);
    else if (WIFSIGNALED(st)) printf("%s: killed by %d\n", what, WTERMSIG(st));
    else printf("%s: exited %d\n", what, WEXITSTATUS(st));
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    pid_t t = target(0);
    usleep(100000);
    signal_from(t, 4321);
    report("root target, another user's owner", t);
    signal_from(t, 0);
    report("root target, root's owner", t);
    t = target(4321);
    usleep(100000);
    signal_from(t, 4321);
    report("target of the same user", t);

    pid_t gone = fork();
    if (gone == 0) _exit(0);
    waitpid(gone, NULL, 0);
    int fds[2];
    pipe(fds);
    printf("F_SETOWN of a reaped pid: %s\n",
           fcntl(fds[0], F_SETOWN, gone) ? strerror(errno) : "ok");
    printf("F_SETOWN of its process group: %s\n",
           fcntl(fds[0], F_SETOWN, -gone) ? strerror(errno) : "ok");
    struct f_owner_ex ex = {F_OWNER_TID, gone};
    printf("F_SETOWN_EX of the tid: %s\n", fcntl(fds[0], F_SETOWN_EX, &ex) ? strerror(errno) : "ok");
    printf("F_SETOWN 0: %s\n", fcntl(fds[0], F_SETOWN, 0) ? strerror(errno) : "ok");
    printf("F_SETOWN self: %s, F_GETOWN is self: %d\n",
           fcntl(fds[0], F_SETOWN, getpid()) ? strerror(errno) : "ok",
           fcntl(fds[0], F_GETOWN) == getpid());
    return 0;
}
C
gcc -O1 -o sigioperm sigioperm.c && ./sigioperm
# PTRACE_PEEKDATA/POKEDATA on a tracee that made itself non-dumpable are EIO for a tracer without
# CAP_SYS_PTRACE at attach (ptrace_access_vm), and work for a dumpable one or a privileged tracer
# (the tracee sets its dumpable state itself: leaving root made the other user's processes
# non-dumpable)
cat > accessvm.c <<'C'
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile long secret = 0x1234;

static void trace(const char *who) {
    for (int dumpable = 1; dumpable >= 0; dumpable--) {
        pid_t c = fork();
        if (c == 0) {
            ptrace(PTRACE_TRACEME, 0, 0, 0);
            prctl(PR_SET_DUMPABLE, dumpable);
            raise(SIGSTOP);
            _exit(0);
        }
        int st;
        waitpid(c, &st, 0);
        errno = 0;
        long v = ptrace(PTRACE_PEEKDATA, c, &secret, 0);
        printf("%s, dumpable %d: peek %s", who, dumpable,
               errno ? strerror(errno) : v == 0x1234 ? "ok" : "wrong value");
        long r = ptrace(PTRACE_POKEDATA, c, &secret, 5);
        printf(", poke %s\n", r ? strerror(errno) : "ok");
        fflush(stdout);
        kill(c, SIGKILL);
        waitpid(c, &st, 0);
    }
}

int main(void) {
    trace("root");
    pid_t u = fork();
    if (u == 0) {
        if (setgid(4321) || setuid(4321)) return 1;
        trace("another user");
        return 0;
    }
    waitpid(u, NULL, 0);
    return 0;
}
C
gcc -O1 -o accessvm accessvm.c && ./accessvm
# uid_map written by root through a descriptor an unprivileged user opened is judged by the opener
# too: mapping uid 0 needs the opener's CAP_SETFCAP, more than its own ID the opener's CAP_SETUID;
# its own ID alone works. Contents read through a descriptor follow its opener's user namespace,
# not the reader's
cat > idmapopener.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static void send_fd(int s, int fd) {
    char c[CMSG_SPACE(sizeof(int))] = {0};
    char b = 'f';
    struct iovec iov = {&b, 1};
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = c,
                       .msg_controllen = sizeof c};
    struct cmsghdr *h = CMSG_FIRSTHDR(&m);
    h->cmsg_level = SOL_SOCKET;
    h->cmsg_type = SCM_RIGHTS;
    h->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(h), &fd, sizeof fd);
    sendmsg(s, &m, 0);
}

static int recv_fd(int s) {
    char c[CMSG_SPACE(sizeof(int))] = {0};
    char b;
    struct iovec iov = {&b, 1};
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = c,
                       .msg_controllen = sizeof c};
    if (recvmsg(s, &m, 0) <= 0 || !CMSG_FIRSTHDR(&m)) return -1;
    int fd;
    memcpy(&fd, CMSG_DATA(CMSG_FIRSTHDR(&m)), sizeof fd);
    return fd;
}

static const char *w(int fd, const char *s) {
    return write(fd, s, strlen(s)) < 0 ? strerror(errno) : "ok";
}

static void line(int fd, const char *what, const char *prefix) {
    char buf[4096];
    lseek(fd, 0, SEEK_SET);
    ssize_t n = read(fd, buf, sizeof buf - 1);
    buf[n > 0 ? n : 0] = 0;
    char *p = strstr(buf, prefix);
    if (p) *strchrnul(p, '\n') = 0;
    printf("%s: %s\n", what, p ? p : "(none)");
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pid_t u = fork();
    if (u == 0) {
        close(sv[0]);
        setgroups(0, NULL);
        if (setgid(4321) || setuid(4321)) return 1;
        // Leaving root made it non-dumpable, and the child's /proc files would belong to root
        prctl(PR_SET_DUMPABLE, 1);
        int ready[2], hold[2];
        pipe(ready);
        pipe(hold);
        pid_t c = fork();
        char x;
        if (c == 0) {
            if (unshare(CLONE_NEWUSER)) printf("unshare: %s\n", strerror(errno));
            write(ready[1], "r", 1);
            read(hold[0], &x, 1);
            printf("inside: uid %d\n", (int)getuid());
            _exit(0);
        }
        read(ready[0], &x, 1);
        char path[64];
        snprintf(path, sizeof path, "/proc/%d/uid_map", c);
        int fd = open(path, O_WRONLY);
        if (fd < 0) {
            printf("open %s\n", strerror(errno));
            write(sv[1], "n", 1);
        } else {
            send_fd(sv[1], fd);
        }
        read(sv[1], &x, 1);
        write(hold[1], "g", 1);
        waitpid(c, NULL, 0);
        _exit(0);
    }
    close(sv[1]);
    int fd = recv_fd(sv[0]);
    if (fd >= 0) {
        printf("root writes 0 0 1: %s\n", w(fd, "0 0 1\n"));
        printf("root writes 0 4321 2: %s\n", w(fd, "0 4321 2\n"));
        printf("root writes 0 4321 1: %s\n", w(fd, "0 4321 1\n"));
    }
    write(sv[0], "d", 1);
    waitpid(u, NULL, 0);

    // Opened here, read again by a child in a new user namespace with no map
    int status = open("/proc/self/status", O_RDONLY);
    int map = open("/proc/self/uid_map", O_RDONLY);
    pid_t c = fork();
    if (c == 0) {
        if (unshare(CLONE_NEWUSER)) printf("unshare: %s\n", strerror(errno));
        line(status, "status read in the child", "Uid:");
        line(map, "uid_map read in the child", " ");
        _exit(0);
    }
    waitpid(c, NULL, 0);
    return 0;
}
C
gcc -O1 -o idmapopener idmapopener.c && ./idmapopener
# A futex waiter is found at its address after the memory there was replaced (MAP_FIXED); private
# and shared operations on one private address do not meet; shared anonymous memory still meets
# across fork
cat > futexkeys.c <<'C'
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

// The store has no kernel headers
#define FUTEX_WAIT 0
#define FUTEX_WAKE 1
#define FUTEX_WAIT_PRIVATE 128
#define FUTEX_WAKE_PRIVATE 129

static long futex(int *a, int op, int val, const struct timespec *t) {
    return syscall(SYS_futex, a, op, val, t, NULL, 0);
}

static int *page;
static int waited_op;
static const char *result;

static void *waiter(void *arg) {
    (void)arg;
    struct timespec t = {3, 0};
    result = futex(page, waited_op, 0, &t) == 0 ? "woken" : strerror(errno);
    return NULL;
}

// Wakes until somebody is woken, for at most 2 s
static long wake_loop(int op) {
    for (int i = 0; i < 200; i++) {
        long n = futex(page, op, 1, NULL);
        if (n) return n;
        usleep(10000);
    }
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    pthread_t th;
    waited_op = FUTEX_WAIT_PRIVATE;
    pthread_create(&th, NULL, waiter, NULL);
    usleep(300000);
    mmap(page, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    printf("wake after MAP_FIXED: %ld\n", wake_loop(FUTEX_WAKE_PRIVATE));
    pthread_join(th, NULL);
    printf("waiter: %s\n", result);

    pthread_create(&th, NULL, waiter, NULL);
    usleep(300000);
    printf("shared wake of a private waiter: %ld\n", futex(page, FUTEX_WAKE, 1, NULL));
    printf("private wake: %ld\n", wake_loop(FUTEX_WAKE_PRIVATE));
    pthread_join(th, NULL);
    printf("waiter: %s\n", result);
    waited_op = FUTEX_WAIT;
    pthread_create(&th, NULL, waiter, NULL);
    usleep(300000);
    printf("private wake of a shared waiter: %ld\n", futex(page, FUTEX_WAKE_PRIVATE, 1, NULL));
    printf("shared wake: %ld\n", wake_loop(FUTEX_WAKE));
    pthread_join(th, NULL);
    printf("waiter: %s\n", result);

    page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    pid_t c = fork();
    if (c == 0) {
        struct timespec t = {3, 0};
        printf("child waiting on shared memory: %s\n",
               futex(page, FUTEX_WAIT, 0, &t) == 0 ? "woken" : strerror(errno));
        _exit(0);
    }
    long n = wake_loop(FUTEX_WAKE);
    waitpid(c, NULL, 0);
    printf("parent's shared wake: %ld\n", n);
    return 0;
}
C
gcc -O1 -pthread -o futexkeys futexkeys.c && ./futexkeys
# A message-queue registration ends when its process closes any descriptor of the queue, so
# another process may register; cancelling a SIGEV_THREAD registration ends musl's helper thread
# (the cookie comes marked removed)
cat > mqflush.c <<'C'
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static int threads(void) {
    int n = 0;
    DIR *d = opendir("/proc/self/task");
    for (struct dirent *e; (e = readdir(d));)
        n += e->d_name[0] != '.';
    closedir(d);
    return n;
}

static void try_register(const char *what) {
    pid_t p = fork();
    if (p == 0) {
        mqd_t q = mq_open("/flush", O_RDWR);
        struct sigevent ev = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGUSR1};
        printf("%s: %s\n", what, mq_notify(q, &ev) ? strerror(errno) : "ok");
        mq_notify(q, NULL);
        _exit(0);
    }
    waitpid(p, NULL, 0);
}

static void thread_fn(union sigval v) {
    (void)v;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    mq_unlink("/flush");
    mqd_t q = mq_open("/flush", O_CREAT | O_RDWR, 0600, NULL);
    int other = dup(q);
    struct sigevent ev = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGUSR1};
    printf("register: %s\n", mq_notify(q, &ev) ? strerror(errno) : "ok");
    try_register("another process while registered");
    close(other);
    try_register("another process after closing a duplicate");

    struct sigevent th = {.sigev_notify = SIGEV_THREAD, .sigev_notify_function = thread_fn};
    printf("SIGEV_THREAD: %s, threads %d\n", mq_notify(q, &th) ? strerror(errno) : "ok",
           threads());
    mq_notify(q, NULL);
    for (int i = 0; i < 100 && threads() > 1; i++)
        usleep(10000);
    printf("after cancelling: threads %d\n", threads());
    mq_close(q);
    mq_unlink("/flush");
    return 0;
}
C
gcc -O1 -pthread -o mqflush mqflush.c && ./mqflush
# clone3's CLONE_INTO_CGROUP checks write permission on the destination's cgroup.procs and on the
# common ancestor's, as a migration would: another user cannot place a child into root's cgroups
cat > intocgroup.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

struct clone_args {
    uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid,
        set_tid_size, cgroup;
};
#define CLONE_INTO_CGROUP 0x200000000ULL

static const char *into(int dirfd) {
    struct clone_args a = {.flags = CLONE_INTO_CGROUP, .exit_signal = 17, .cgroup = dirfd};
    long pid = syscall(435, &a, sizeof a);
    if (pid == 0) _exit(0);
    if (pid < 0) return strerror(errno);
    waitpid(pid, NULL, 0);
    return "ok";
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    rmdir("/sys/fs/cgroup/t5a");
    rmdir("/sys/fs/cgroup/t5b");
    if (mkdir("/sys/fs/cgroup/t5a", 0755) || mkdir("/sys/fs/cgroup/t5b", 0755)) {
        printf("mkdir: %s\n", strerror(errno));
        return 0;
    }
    pid_t p = fork();
    if (p == 0) {
        int procs = open("/sys/fs/cgroup/t5a/cgroup.procs", O_WRONLY);
        write(procs, "0", 1);
        close(procs);
        int b = open("/sys/fs/cgroup/t5b", O_PATH | O_DIRECTORY);
        printf("root into t5b: %s\n", into(b));
        if (setgid(4321) || setuid(4321)) _exit(1);
        printf("another user into t5b: %s\n", into(b));
        int a = open("/sys/fs/cgroup/t5a", O_PATH | O_DIRECTORY);
        printf("another user into its own t5a: %s\n", into(a));
        _exit(0);
    }
    waitpid(p, NULL, 0);
    rmdir("/sys/fs/cgroup/t5a");
    rmdir("/sys/fs/cgroup/t5b");
    return 0;
}
C
gcc -O1 -o intocgroup intocgroup.c && ./intocgroup
# Routes through a gateway outside every subnet are accepted with onlink, which needs a device that
# is up and, for IPv4, a scope below link and a unicast gateway; dumps show the flag
unshare -n sh -c '
ip link set lo up
ip link add d0 type dummy
ip link set d0 up
ip addr add 10.10.0.2/24 dev d0
ip route add default via 10.20.0.1 2>&1 | sed "s/^/without onlink: /"
ip route add default via 10.20.0.1 dev d0 onlink && echo "onlink: ok"
ip route add 10.30.0.0/16 via 10.20.0.2 onlink 2>&1 | sed "s/^/without a device: /"
ip route add 10.31.0.0/16 via 10.20.0.2 dev d0 onlink scope link 2>&1 | sed "s/^/scope link: /"
ip route add 10.32.0.0/16 via 224.0.0.9 dev d0 onlink 2>&1 | sed "s/^/multicast gateway: /"
ip link add d1 type dummy
ip route add 10.33.0.0/16 via 10.20.0.3 dev d1 onlink 2>&1 | sed "s/^/device down: /"
ip route
ip route get 10.40.1.1
'
# limit and quota keep state: "limit rate over" drops beyond the rate (after a burst of 5),
# "limit rate" accepts within it, "quota over" drops once the bytes reach the quota (headers only:
# empty datagrams of 28 bytes), and iptables' -m limit matches within its rate
cat > ratesend.c <<'C'
#include <arpa/inet.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>

int main(int argc, char **argv) {
    int port = atoi(argv[1]);
    int r = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    bind(r, (struct sockaddr *)&a, sizeof a);
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    int sent = 0, refused = 0, err = 0;
    for (int i = 0; i < 10; i++) {
        if (sendto(s, "", 0, 0, (struct sockaddr *)&a, sizeof a) == 0)
            sent++;
        else {
            refused++;
            err = errno;
        }
    }
    printf("port %d: sent %d, refused %d%s%s\n", port, sent, refused, refused ? ": " : "",
           refused ? strerror(err) : "");
    return 0;
}
C
gcc -O1 -o ratesend ratesend.c
unshare -n sh -c '
ip link set lo up
nft add table ip t
nft add chain ip t out "{ type filter hook output priority 0; }"
nft add rule ip t out udp dport 9001 limit rate over 2/second drop
nft add rule ip t out udp dport 9002 limit rate 2/second accept
nft add rule ip t out udp dport 9002 drop
nft add rule ip t out udp dport 9003 quota over 100 bytes drop
iptables --wait -A OUTPUT -p udp --dport 9004 -m limit --limit 2/second --limit-burst 3 -j ACCEPT
iptables --wait -A OUTPUT -p udp --dport 9004 -j DROP
for p in 9001 9002 9003 9004 9005; do ./ratesend $p; done
'
# Small checks: O_DIRECT opens (store files only; not directories, devices or procfs);
# getdents64 on a removed directory and with a count above INT_MAX; copy_file_range's order of
# checks and across filesystems; datagram sends to a socket shut down for reading (EPIPE, no
# SIGPIPE) and after SHUT_WR, and POLLHUP after both; kcmp's KCMP_IO; sigaction clearing unknown
# flags; a raw socket bound twice to one address
cat > smallchecks.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *res(long r) {
    return r < 0 ? strerror(errno) : "ok";
}

static void direct(const char *path, int flags) {
    int fd = open(path, flags | O_DIRECT, 0600);
    printf("O_DIRECT %s: %s\n", path, res(fd));
    if (fd >= 0) close(fd);
}

static void sigpipe(int s) {
    (void)s;
    printf("SIGPIPE\n");
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    // tmpfs takes O_DIRECT from 6.6 on, where 6.1 refuses it: not compared
    direct("/tmp/direct", O_RDWR | O_CREAT);
    direct("/tmp", O_RDONLY);
    direct("/tmp", O_WRONLY);
    direct("/dev/null", O_RDONLY);
    direct("/proc/self/status", O_RDONLY);
    unlink("/tmp/direct");

    mkdir("/tmp/gone", 0755);
    int dir = open("/tmp/gone", O_RDONLY | O_DIRECTORY);
    rmdir("/tmp/gone");
    char buf[4096];
    printf("getdents64 of a removed directory: %s\n",
           res(syscall(SYS_getdents64, dir, buf, sizeof buf)));
    close(dir);
    dir = open("/tmp", O_RDONLY | O_DIRECTORY);
    printf("getdents64 with a count above INT_MAX: %s\n",
           res(syscall(SYS_getdents64, dir, buf, 0x80000000UL)));

    int in = open("/tmp/cfr-in", O_RDWR | O_CREAT | O_TRUNC, 0600);
    write(in, "0123456789abcdef", 16);
    int out = open("/tmp/cfr-out", O_RDWR | O_CREAT | O_TRUNC, 0600);
    int shm = open("/dev/shm/cfr", O_RDWR | O_CREAT | O_TRUNC, 0600);
    int pipes[2];
    pipe(pipes);
    loff_t a = 0, b = 0;
    printf("copy_file_range to a directory: %s\n", res(copy_file_range(in, &a, dir, &b, 4, 0)));
    printf("copy_file_range to a pipe's read end: %s\n",
           res(copy_file_range(in, &a, pipes[0], NULL, 4, 0)));
    b = 4;
    printf("copy_file_range overlapping itself: %s\n", res(copy_file_range(in, &a, in, &b, 8, 0)));
    b = 16;
    printf("copy_file_range past its end onto itself: %ld\n",
           (long)copy_file_range(in, &a, in, &b, 8, 0));
    a = 0;
    printf("copy_file_range to tmpfs: %s\n", res(copy_file_range(in, &a, shm, NULL, 4, 0)));
    a = 0, b = 0x7fffffffffffffffLL;
    printf("copy_file_range at the largest offset: %s\n",
           res(copy_file_range(in, &a, out, &b, 4, 0)));
    unlink("/tmp/cfr-in");
    unlink("/tmp/cfr-out");
    unlink("/dev/shm/cfr");

    signal(SIGPIPE, sigpipe);
    int s[2];
    socketpair(AF_UNIX, SOCK_DGRAM, 0, s);
    shutdown(s[1], SHUT_RD);
    printf("datagram to a socket shut down for reading: %s\n", res(send(s[0], "x", 1, 0)));
    shutdown(s[0], SHUT_WR);
    printf("datagram after SHUT_WR: %s\n", res(send(s[0], "x", 1, 0)));
    shutdown(s[0], SHUT_RD);
    struct pollfd p = {s[0], POLLIN | POLLOUT};
    poll(&p, 1, 0);
    printf("POLLHUP after both: %d\n", !!(p.revents & POLLHUP));

    pid_t me = getpid();
    printf("KCMP_IO with itself: %ld\n", (long)syscall(SYS_kcmp, me, me, 5, 0, 0));

    struct sigaction sa = {.sa_handler = SIG_DFL, .sa_flags = 0x400 | SA_RESTART};
    struct sigaction old;
    sigaction(SIGUSR1, &sa, NULL);
    sigaction(SIGUSR1, NULL, &old);
    printf("SA_UNSUPPORTED kept: %d, SA_RESTART kept: %d\n", !!(old.sa_flags & 0x400),
           !!(old.sa_flags & SA_RESTART));

    int raw = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    if (raw < 0) {
        printf("raw socket: %s\n", strerror(errno));
    } else {
        struct sockaddr_in lo = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
        printf("raw bind: %s\n", res(bind(raw, (struct sockaddr *)&lo, sizeof lo)));
        printf("raw bind again: %s\n", res(bind(raw, (struct sockaddr *)&lo, sizeof lo)));
    }
    return 0;
}
C
gcc -O1 -o smallchecks smallchecks.c && ./smallchecks
