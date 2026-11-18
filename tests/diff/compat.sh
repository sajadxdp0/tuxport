# Less common interfaces that programs depend on: queueing of real-time signals, pidfd, execveat,
# the owner and permissions of terminals, and so on
set -e
cd /tmp
cat > rtsig.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <unistd.h>

static int got[16], n;

static void handler(int sig, siginfo_t *si, void *ctx)
{
    (void)ctx;
    if (n < 16)
        got[n++] = (sig - SIGRTMIN) * 100 + si->si_value.sival_int;
}

int main(void)
{
    sigset_t set, old;
    sigemptyset(&set);
    sigaddset(&set, SIGRTMIN);
    sigaddset(&set, SIGRTMIN + 1);
    sigaddset(&set, SIGUSR1);
    sigprocmask(SIG_BLOCK, &set, &old);
    /* Queue the same real-time signal three times, each with its own value; a standard signal is
     * pending only once */
    for (int i = 1; i <= 3; i++)
        sigqueue(getpid(), SIGRTMIN + 1, (union sigval){.sival_int = i});
    for (int i = 4; i <= 5; i++)
        sigqueue(getpid(), SIGRTMIN, (union sigval){.sival_int = i});
    kill(getpid(), SIGUSR1);
    kill(getpid(), SIGUSR1);
    siginfo_t si;
    int sig = sigwaitinfo(&set, &si);
    printf("first %s\n", sig == SIGUSR1 ? "SIGUSR1" : "other");
    sigdelset(&set, SIGUSR1);
    struct timespec zero = {0, 0};
    printf("usr1 again %d\n", sigtimedwait(&(sigset_t){0}, NULL, &zero) == -1 && errno == EAGAIN);
    sig = sigwaitinfo(&set, &si);
    printf("wait %d value %d\n", sig - SIGRTMIN, si.si_value.sival_int);
    /* The rest goes to the handler: lower numbers first, the same signal in queueing order */
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGRTMIN, &sa, NULL);
    sigaction(SIGRTMIN + 1, &sa, NULL);
    sigprocmask(SIG_SETMASK, &old, NULL);
    printf("delivered");
    for (int i = 0; i < n; i++)
        printf(" %d", got[i]);
    printf("\n");
    /* Queue full: sigqueue returns EAGAIN, kill still leaves it pending. Linux counts per user,
       including the user's other processes, so the limit is set to 0 */
    struct rlimit rl = {0, 0};
    setrlimit(RLIMIT_SIGPENDING, &rl);
    sigprocmask(SIG_BLOCK, &set, NULL);
    int ok = 0;
    while (sigqueue(getpid(), SIGRTMIN, (union sigval){.sival_int = ok}) == 0 && ok < 100)
        ok++;
    printf("queued %d errno %s\n", ok, errno == EAGAIN ? "EAGAIN" : strerror(errno));
    printf("kill %d\n", kill(getpid(), SIGRTMIN + 1));
    int left = 0;
    while (sigtimedwait(&set, &si, &zero) > 0)
        left++;
    printf("left %d\n", left);
    return 0;
}
C
gcc -O1 -o rtsig rtsig.c
./rtsig

# pidfd: readable when the process ends, waitid(P_PIDFD) waits for it, pidfd_send_signal sends
# signals, pidfd_getfd copies its fds; CLONE_PIDFD of clone and clone3. Only the parts where 6.1
# and newer kernels behave the same are tested (from 6.9 on there is POLLHUP after reaping, and
# CLONE_PIDFD can be used together with CLONE_THREAD)
cat > pidfd.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* The image has no linux-headers */
#define P_PIDFD 3
#define PIDFD_NONBLOCK O_NONBLOCK
#define CLONE_CLEAR_SIGHAND 0x100000000ULL
struct clone_args {
    uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls;
};

static int pidfd_open(pid_t pid, unsigned flags) { return syscall(SYS_pidfd_open, pid, flags); }
static int pidfd_send_signal(int fd, int sig, siginfo_t *info, unsigned flags)
{
    return syscall(SYS_pidfd_send_signal, fd, sig, info, flags);
}
static int pidfd_getfd(int fd, int target, unsigned flags) { return syscall(SYS_pidfd_getfd, fd, target, flags); }

static int readable(int fd, int ms)
{
    struct pollfd p = {fd, POLLIN, 0};
    return poll(&p, 1, ms) == 1 ? p.revents : 0;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    int pipefd[2];
    pipe(pipefd);
    pid_t pid = fork();
    if (pid == 0) {
        char c;
        int fd = open("/tmp/pidfd-target", O_CREAT | O_RDWR | O_TRUNC, 0644);
        dprintf(fd, "hello from child\n");
        dup2(fd, 42);
        read(pipefd[0], &c, 1);
        _exit(7);
    }
    int fd = pidfd_open(pid, 0);
    printf("pidfd cloexec %d\n", (fcntl(fd, F_GETFD) & FD_CLOEXEC) != 0);
    char link[64] = {0}, path[64];
    snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
    readlink(path, link, sizeof link - 1);
    printf("link %s\n", link);
    printf("readable before %d\n", readable(fd, 50));
    printf("read %zd %s\n", read(fd, path, 8), strerror(errno));
    /* Its fd can be taken only after the child opens the file: poll until fd 42 is valid */
    int copy = -1;
    for (int i = 0; i < 200 && copy < 0; i++) {
        copy = pidfd_getfd(fd, 42, 0);
        if (copy < 0)
            usleep(10000);
    }
    char buf[64] = {0};
    pread(copy, buf, sizeof buf - 1, 0);
    printf("getfd %s", buf);
    printf("getfd bad %d %s\n", pidfd_getfd(fd, 1000, 0), strerror(errno));
    printf("getfd flags %d %s\n", pidfd_getfd(fd, 42, 1), strerror(errno));
    printf("signal 0 %d\n", pidfd_send_signal(fd, 0, NULL, 0));
    int nb = pidfd_open(pid, PIDFD_NONBLOCK);
    siginfo_t si;
    printf("waitid nonblock %d %s\n", waitid(P_PIDFD, nb, &si, WEXITED), strerror(errno));
    write(pipefd[1], "x", 1);
    printf("readable after %d\n", readable(fd, 5000) & POLLIN);
    memset(&si, 0, sizeof si);
    printf("waitid %d pid ok %d code %d status %d\n", waitid(P_PIDFD, fd, &si, WEXITED),
           si.si_pid == pid, si.si_code, si.si_status);
    printf("after reap: send %d %s\n", pidfd_send_signal(fd, SIGTERM, NULL, 0), strerror(errno));
    printf("after reap: waitid %d %s\n", waitid(P_PIDFD, fd, &si, WEXITED), strerror(errno));
    printf("after reap: getfd %d %s\n", pidfd_getfd(fd, 42, 0), strerror(errno));
    printf("open gone %d %s\n", pidfd_open(pid, 0), strerror(errno));
    printf("open bad flags %d %s\n", pidfd_open(getpid(), 1), strerror(errno));
    close(fd);
    close(nb);

    /* Sending to ourselves: both a pidfd and the /proc/self directory work, and with a siginfo
     * si_signo must match */
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigprocmask(SIG_BLOCK, &set, NULL);
    int self = open("/proc/self", O_DIRECTORY | O_CLOEXEC);
    siginfo_t info;
    memset(&info, 0, sizeof info);
    info.si_signo = SIGUSR1;
    info.si_code = SI_QUEUE;
    info.si_value.sival_int = 99;
    printf("send self %d\n", pidfd_send_signal(self, SIGUSR1, &info, 0));
    sigwaitinfo(&set, &si);
    printf("got code %d value %d\n", si.si_code, si.si_value.sival_int);
    printf("mismatch %d %s\n", pidfd_send_signal(self, SIGUSR2, &info, 0), strerror(errno));
    printf("bad flags %d %s\n", pidfd_send_signal(self, SIGUSR1, NULL, 1 << 8), strerror(errno));
    printf("not pidfd %d %s\n", pidfd_send_signal(1, SIGUSR1, NULL, 0), strerror(errno));

    /* clone(CLONE_PIDFD) writes to parent_tid, and cannot be used together with
     * CLONE_PARENT_SETTID */
    int cfd = -1;
    pid = syscall(SYS_clone, CLONE_PIDFD | SIGCHLD, 0, &cfd, 0, 0);
    if (pid == 0)
        _exit(3);
    printf("clone pidfd %d\n", cfd >= 0);
    waitid(P_PIDFD, cfd, &si, WEXITED);
    printf("clone child %d\n", si.si_status);
    printf("clone settid %ld %s\n", syscall(SYS_clone, CLONE_PIDFD | CLONE_PARENT_SETTID | SIGCHLD, 0, &cfd, 0, 0),
           strerror(errno));

    /* clone3: the pidfd is a field of its own, and so is exit_signal */
    struct clone_args args;
    memset(&args, 0, sizeof args);
    int c3 = -1;
    args.flags = CLONE_PIDFD;
    args.pidfd = (uintptr_t)&c3;
    args.exit_signal = SIGCHLD;
    pid = syscall(SYS_clone3, &args, sizeof args);
    if (pid == 0)
        _exit(5);
    waitid(P_PIDFD, c3, &si, WEXITED);
    printf("clone3 pidfd %d child %d\n", c3 >= 0, si.si_status);
    args.flags = 0;
    args.exit_signal = 0x100;
    printf("clone3 bad signal %ld %s\n", syscall(SYS_clone3, &args, sizeof args), strerror(errno));
    args.exit_signal = SIGCHLD;
    printf("clone3 short %ld %s\n", syscall(SYS_clone3, &args, 63), strerror(errno));
    char big[sizeof args + 8];
    memset(big, 0, sizeof big);
    memcpy(big, &args, sizeof args);
    big[sizeof args] = 1;
    printf("clone3 nonzero tail %ld %s\n", syscall(SYS_clone3, big, sizeof big), strerror(errno));
    args.flags = CLONE_FS | CLONE_NEWNS;
    printf("clone3 fs newns %ld %s\n", syscall(SYS_clone3, &args, sizeof args), strerror(errno));
    args.flags = 0;
    args.stack = (uintptr_t)big;
    printf("clone3 stack no size %ld %s\n", syscall(SYS_clone3, &args, sizeof args), strerror(errno));
    /* CLONE_CLEAR_SIGHAND: handlers are reset to default, ignored signals stay ignored */
    signal(SIGUSR2, SIG_IGN);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = (void (*)(int))exit;
    sigaction(SIGHUP, &sa, NULL);
    memset(&args, 0, sizeof args);
    args.flags = CLONE_CLEAR_SIGHAND;
    args.exit_signal = SIGCHLD;
    pid = syscall(SYS_clone3, &args, sizeof args);
    if (pid == 0) {
        struct sigaction a, b;
        sigaction(SIGHUP, NULL, &a);
        sigaction(SIGUSR2, NULL, &b);
        _exit((a.sa_handler == SIG_DFL) * 2 + (b.sa_handler == SIG_IGN));
    }
    int status;
    waitpid(pid, &status, 0);
    printf("clear sighand %d\n", WEXITSTATUS(status));
    return 0;
}
C
gcc -O1 -o pidfd pidfd.c
./pidfd

# execveat: paths relative to a directory fd, AT_EMPTY_PATH (fexecve), AT_SYMLINK_NOFOLLOW; the
# AT_EXECFN and comm the new program sees are /dev/fd/<n>[/<path>]
mkdir -p /tmp/xat
cat > /tmp/xat/show.c <<'C'
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>
#include <unistd.h>
int main(int argc, char **argv)
{
    char exe[256] = {0}, comm[32] = {0};
    readlink("/proc/self/exe", exe, sizeof exe - 1);
    FILE *f = fopen("/proc/self/comm", "r");
    fgets(comm, sizeof comm, f);
    comm[strcspn(comm, "\n")] = 0;
    printf("argv0 %s execfn %s comm %s exe %s\n", argv[0], (char *)getauxval(AT_EXECFN), comm, exe);
    return 0;
}
C
cat > xat.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static void run(const char *what, int dirfd, const char *path, int flags)
{
    char *argv[] = {"show", NULL};
    pid_t pid = fork();
    if (pid == 0) {
        syscall(SYS_execveat, dirfd, path, argv, environ, flags);
        printf("%s: %s\n", what, strerror(errno));
        _exit(1);
    }
    waitpid(pid, NULL, 0);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    int dir = open("/tmp/xat", O_DIRECTORY | O_RDONLY);
    int opath = open("/tmp/xat/show", O_PATH);
    int file = open("/tmp/xat/show", O_RDONLY | O_CLOEXEC);
    symlink("show", "/tmp/xat/link");
    run("relative", dir, "show", 0);
    run("absolute", dir, "/tmp/xat/show", 0);
    run("cwd", AT_FDCWD, "xat/show", 0);
    run("empty path O_PATH", opath, "", AT_EMPTY_PATH);
    run("empty path cloexec", file, "", AT_EMPTY_PATH);
    run("symlink", dir, "link", 0);
    run("symlink nofollow", dir, "link", AT_SYMLINK_NOFOLLOW);
    run("bad fd", -1, "", AT_EMPTY_PATH);
    run("bad flags", dir, "show", -1);
    run("not a dir", opath, "show", 0);
    run("empty without flag", dir, "", 0);
    /* Script: a close-on-exec fd is closed before the interpreter opens it */
    int s = open("/tmp/xat/script", O_CREAT | O_WRONLY | O_TRUNC, 0755);
    dprintf(s, "#!/bin/sh\necho script $0\n");
    close(s);
    int sc = open("/tmp/xat/script", O_RDONLY | O_CLOEXEC);
    run("script cloexec", sc, "", AT_EMPTY_PATH);
    run("script relative cloexec", dir, "script", 0);
    int sk = open("/tmp/xat/script", O_RDONLY);
    run("script", sk, "", AT_EMPTY_PATH);
    return 0;
}
C
gcc -O1 -o /tmp/xat/show /tmp/xat/show.c
gcc -O1 -o xatest xat.c
./xatest

# /proc/net/{tcp,tcp6,udp,udp6,icmp,icmp6,raw,unix}: find our own sockets by fstat's inode and
# look only at the columns that do not depend on the environment (ports are allocated
# ephemerally, so only whether they agree with getsockname is compared)
cat > procnet.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <netinet/in.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

static unsigned long ino(int fd)
{
    struct stat st;
    fstat(fd, &st);
    return st.st_ino;
}

static unsigned port(int fd, int peer)
{
    struct sockaddr_storage ss;
    socklen_t len = sizeof ss;
    (peer ? getpeername : getsockname)(fd, (struct sockaddr *)&ss, &len);
    return ntohs(((struct sockaddr_in *)&ss)->sin_port);
}

/* Find the line whose inode is fd's in /proc/net/<file>, and print the addresses, ports and
 * state */
static void inet(const char *file, const char *what, int fd)
{
    char path[64], line[512];
    snprintf(path, sizeof path, "/proc/net/%s", file);
    FILE *f = fopen(path, "r");
    fgets(line, sizeof line, f);
    int width = strlen(line);
    while (fgets(line, sizeof line, f)) {
        char local[64], remote[64];
        unsigned st, lport, rport;
        unsigned long inode;
        if (sscanf(line, "%*d: %63[0-9A-F]:%X %63[0-9A-F]:%X %X %*s %*s %*s %*s %*s %lu", local, &lport,
                   remote, &rport, &st, &inode) != 6 || inode != ino(fd))
            continue;
        printf("%s %s: local %s port %s remote %s port %s st %02X width %s\n", file, what, local,
               lport == port(fd, 0) ? "ok" : "bad", remote,
               rport == 0 ? "0" : rport == port(fd, 1) ? "ok" : "bad", st,
               (int)strlen(line) == width ? "same" : "differs");
        fclose(f);
        return;
    }
    printf("%s %s: missing\n", file, what);
    fclose(f);
}

/* A raw socket's line: the local "port" is the protocol number, the remote port is always 0, and
 * the end of the line is not padded with spaces */
static void raw_line(const char *file, const char *what, int fd)
{
    char path[64], line[512];
    snprintf(path, sizeof path, "/proc/net/%s", file);
    FILE *f = fopen(path, "r");
    fgets(line, sizeof line, f);
    printf("%s header %s", file, line);
    while (fgets(line, sizeof line, f)) {
        char local[64], remote[64];
        unsigned st, lport, rport;
        unsigned long inode;
        if (sscanf(line, "%*d: %63[0-9A-F]:%X %63[0-9A-F]:%X %X %*s %*s %*s %*s %*s %lu", local, &lport,
                   remote, &rport, &st, &inode) != 6 || inode != ino(fd))
            continue;
        printf("%s %s: local %s port %u remote %s port %u st %02X tail '%c'\n", file, what, local, lport,
               remote, rport, st, line[strlen(line) - 2]);
        fclose(f);
        return;
    }
    printf("%s %s: missing\n", file, what);
    fclose(f);
}

static void unix_line(const char *what, int fd)
{
    char line[512];
    FILE *f = fopen("/proc/net/unix", "r");
    fgets(line, sizeof line, f);
    printf("unix header %s", line);
    while (fgets(line, sizeof line, f)) {
        unsigned ref, proto, flags, type, st;
        unsigned long inode;
        int n = 0;
        if (sscanf(line, "%*x: %X %X %X %X %X %lu%n", &ref, &proto, &flags, &type, &st, &inode, &n) != 6 ||
            inode != ino(fd))
            continue;
        printf("unix %s: flags %08X type %04X st %02X path%s", what, flags, type, st, line + n);
        fclose(f);
        return;
    }
    printf("unix %s: missing\n", what);
    fclose(f);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    int lis = socket(AF_INET, SOCK_STREAM, 0);
    bind(lis, (struct sockaddr *)&a, sizeof a);
    listen(lis, 4);
    socklen_t len = sizeof a;
    getsockname(lis, (struct sockaddr *)&a, &len);
    int cli = socket(AF_INET, SOCK_STREAM, 0);
    connect(cli, (struct sockaddr *)&a, sizeof a);
    int srv = accept(lis, NULL, NULL);
    inet("tcp", "listen", lis);
    inet("tcp", "client", cli);
    inet("tcp", "server", srv);
    int udp = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in u = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    bind(udp, (struct sockaddr *)&u, sizeof u);
    inet("udp", "bound", udp);
    connect(udp, (struct sockaddr *)&a, sizeof a);
    inet("udp", "connected", udp);
    struct sockaddr_in6 a6 = {.sin6_family = AF_INET6, .sin6_addr = IN6ADDR_LOOPBACK_INIT};
    int lis6 = socket(AF_INET6, SOCK_STREAM, 0);
    bind(lis6, (struct sockaddr *)&a6, sizeof a6);
    listen(lis6, 4);
    inet("tcp6", "listen", lis6);
    int udp6 = socket(AF_INET6, SOCK_DGRAM, 0);
    bind(udp6, (struct sockaddr *)&a6, sizeof a6);
    inet("udp6", "bound", udp6);
    inet("tcp", "tcp6 socket", lis6);
    /* Ping sockets are listed in icmp, raw ICMP sockets in raw */
    struct sockaddr_in p = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    int ping = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    bind(ping, (struct sockaddr *)&p, sizeof p);
    inet("icmp", "ping", ping);
    connect(ping, (struct sockaddr *)&p, sizeof p);
    inet("icmp", "ping connected", ping);
    int raw = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    int type;
    socklen_t tl = sizeof type;
    getsockopt(raw, SOL_SOCKET, SO_TYPE, &type, &tl);
    printf("raw SO_TYPE %d\n", type);
    raw_line("raw", "icmp", raw);
    inet("icmp", "raw socket", raw);
    int ping6 = socket(AF_INET6, SOCK_DGRAM, IPPROTO_ICMPV6);
    bind(ping6, (struct sockaddr *)&a6, sizeof a6);
    inet("icmp6", "ping", ping6);

    struct sockaddr_un un = {.sun_family = AF_UNIX};
    strcpy(un.sun_path, "/tmp/procnet.sock");
    unlink(un.sun_path);
    int ul = socket(AF_UNIX, SOCK_STREAM, 0);
    bind(ul, (struct sockaddr *)&un, sizeof un);
    listen(ul, 4);
    unix_line("listen", ul);
    int uc = socket(AF_UNIX, SOCK_STREAM, 0);
    connect(uc, (struct sockaddr *)&un, sizeof un);
    unix_line("client", uc);
    struct sockaddr_un ab = {.sun_family = AF_UNIX};
    memcpy(ab.sun_path, "\0tuxport\0test", 12);
    int ua = socket(AF_UNIX, SOCK_DGRAM, 0);
    bind(ua, (struct sockaddr *)&ab, offsetof(struct sockaddr_un, sun_path) + 12);
    unix_line("abstract", ua);
    int sp[2];
    socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sp);
    unix_line("pair", sp[0]);
    return 0;
}
C
gcc -O1 -o procnet procnet.c
./procnet

# /proc/net/if_inet6: the loopback line (addresses of other interfaces depend on the container's
# network). Without IPv6 in the environment neither side has this line
grep ' lo$' /proc/net/if_inet6 || echo "if_inet6: no lo"

# Interface traffic counters: the header of /proc/net/dev and the format of the lo line, the
# attributes of /sys/class/net/lo and the files under statistics; after sending and receiving on
# loopback lo's counters grow by at least the payload bytes (Linux also counts protocol headers,
# Tuxport counts only the payload), and both places read the same. Other interfaces depend on the
# container's network
head -n 2 /proc/net/dev
grep '^ *lo:' /proc/net/dev | awk '{ print "lo fields:", NF }'
for f in addr_len address broadcast carrier flags ifindex iflink mtu operstate type; do
    echo "lo $f: $(cat /sys/class/net/lo/$f)"
done
ls /sys/class/net/lo/statistics | tr '\n' ' '
echo
python3 - <<'PY'
import socket


def lo():
    v = {}
    for f in ("rx_bytes", "tx_bytes", "rx_packets", "tx_packets"):
        with open("/sys/class/net/lo/statistics/" + f) as s:
            v[f] = int(s.read())
    with open("/proc/net/dev") as d:
        for line in d:
            name, _, rest = line.partition(":")
            if name.strip() == "lo":
                n = rest.split()
                v["proc"] = (int(n[0]), int(n[1]), int(n[8]), int(n[9]))
    return v


a = lo()
listener = socket.socket()
listener.bind(("127.0.0.1", 0))
listener.listen()
c = socket.create_connection(listener.getsockname())
s, _ = listener.accept()
c.sendall(b"x" * 100000)
c.close()
got = 0
while d := s.recv(65536):
    got += len(d)
b = lo()
print("received", got)
for k in ("rx_bytes", "tx_bytes"):
    print("lo", k, "grew" if b[k] - a[k] >= 100000 else "did not grow")
grew = b["rx_packets"] > a["rx_packets"] and b["tx_packets"] > a["tx_packets"]
print("lo packets", "grew" if grew else "did not grow")
same = b["proc"] == (b["rx_bytes"], b["rx_packets"], b["tx_bytes"], b["tx_packets"])
print("/proc/net/dev and sysfs", "agree" if same else "disagree")
PY

# NETLINK_GENERIC: nlctrl's GETFAMILY (by name, by ID, dump) and GETPOLICY, querying a family
# that does not exist, sending to a family ID that does not exist, commands nlctrl does not know;
# the attributes of the replies, error codes, ack flags and the length attached to error
# messages. In the dump only nlctrl is looked at; other families depend on the kernel
# configuration
cat > genl.c <<'C'
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

/* From linux/netlink.h and linux/genetlink.h (the store has no kernel headers) */
#define NETLINK_GENERIC 16
#define NLM_F_REQUEST 1
#define NLM_F_MULTI 2
#define NLM_F_ACK 4
#define NLM_F_DUMP 0x300
#define NLMSG_ERROR 2
#define NLMSG_DONE 3
#define GENL_ID_CTRL 0x10
#define CTRL_CMD_GETFAMILY 3
#define CTRL_ATTR_FAMILY_ID 1
#define CTRL_ATTR_FAMILY_NAME 2
#define CTRL_ATTR_OPS 6
#define CTRL_ATTR_MCAST_GROUPS 7
#define NLA_TYPE_MASK 0x3fff
struct nlmsghdr {
    uint32_t nlmsg_len;
    uint16_t nlmsg_type, nlmsg_flags;
    uint32_t nlmsg_seq, nlmsg_pid;
};
struct nlmsgerr {
    int error;
    struct nlmsghdr msg;
};
struct genlmsghdr {
    uint8_t cmd, version;
    uint16_t reserved;
};
struct nlattr {
    uint16_t nla_len, nla_type;
};
#define NLMSG_ALIGN(n) (((n) + 3) & ~3)
#define NLMSG_LENGTH(n) ((n) + sizeof(struct nlmsghdr))
#define NLMSG_DATA(h) ((void *)((char *)(h) + sizeof(struct nlmsghdr)))
#define NLMSG_OK(h, n) ((n) >= (int)sizeof(struct nlmsghdr) && (h)->nlmsg_len >= sizeof(struct nlmsghdr) && (int)(h)->nlmsg_len <= (n))
#define NLMSG_NEXT(h, n) ((n) -= NLMSG_ALIGN((h)->nlmsg_len), (struct nlmsghdr *)((char *)(h) + NLMSG_ALIGN((h)->nlmsg_len)))
#define GENL_HDRLEN 4
#define NLA_HDRLEN 4
#define NLA_ALIGN(n) (((n) + 3) & ~3)

static int fd;
static unsigned seq;

static void request(unsigned short type, unsigned short flags, unsigned char cmd, unsigned short attr,
                    const void *val, int len)
{
    char buf[256];
    memset(buf, 0, sizeof buf);
    struct nlmsghdr *h = (void *)buf;
    struct genlmsghdr *g = NLMSG_DATA(h);
    g->cmd = cmd;
    g->version = 1;
    struct nlattr *a = (void *)(g + 1);
    int total = NLMSG_LENGTH(GENL_HDRLEN);
    if (attr) {
        a->nla_type = attr;
        a->nla_len = NLA_HDRLEN + len;
        memcpy((char *)a + NLA_HDRLEN, val, len);
        total += NLA_ALIGN(a->nla_len);
    }
    h->nlmsg_len = total;
    h->nlmsg_type = type;
    h->nlmsg_flags = NLM_F_REQUEST | flags;
    h->nlmsg_seq = ++seq;
    send(fd, buf, total, 0);
}

static void attrs(struct nlattr *a, int len, int depth)
{
    for (; len >= NLA_HDRLEN && a->nla_len >= NLA_HDRLEN && a->nla_len <= len;
         len -= NLA_ALIGN(a->nla_len), a = (void *)((char *)a + NLA_ALIGN(a->nla_len))) {
        int type = a->nla_type & NLA_TYPE_MASK, n = a->nla_len - NLA_HDRLEN;
        void *d = (char *)a + NLA_HDRLEN;
        printf("%*sattr %d", depth * 2, "", type);
        if (depth == 0 && (type == CTRL_ATTR_OPS || type == CTRL_ATTR_MCAST_GROUPS)) {
            printf(" nested\n");
            attrs(d, n, 1);
        } else if (depth == 1) {
            printf(" nested\n");
            attrs(d, n, 2);
        } else if (n == 2)
            printf(" u16 %u\n", *(unsigned short *)d);
        else if (n == 4)
            printf(" u32 %u\n", *(unsigned *)d);
        else
            printf(" \"%.*s\"\n", n, (char *)d);
    }
}

/* Read replies until DONE, ERROR or nothing more */
static void replies(const char *what)
{
    printf("== %s\n", what);
    char buf[16384];
    for (;;) {
        int n = recv(fd, buf, sizeof buf, 0);
        if (n < 0) {
            printf("recv errno %d\n", errno);
            return;
        }
        for (struct nlmsghdr *h = (void *)buf; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
            if (h->nlmsg_type == NLMSG_ERROR) {
                struct nlmsgerr *e = NLMSG_DATA(h);
                printf("error %d, flags %#x, echoed %u bytes, type %u\n", e->error, h->nlmsg_flags,
                       (unsigned)(h->nlmsg_len - NLMSG_LENGTH(sizeof(int))), e->msg.nlmsg_type);
                return;
            }
            if (h->nlmsg_type == NLMSG_DONE) {
                printf("done\n");
                return;
            }
            struct genlmsghdr *g = NLMSG_DATA(h);
            if (!(h->nlmsg_flags & NLM_F_MULTI))
                printf("type %u flags %#x cmd %u version %u\n", h->nlmsg_type, h->nlmsg_flags,
                       g->cmd, g->version);
            struct nlattr *a = (void *)((char *)g + GENL_HDRLEN);
            int len = h->nlmsg_len - NLMSG_LENGTH(GENL_HDRLEN);
            /* In the dump only nlctrl is looked at; other families depend on the kernel
             * configuration */
            if (h->nlmsg_flags & NLM_F_MULTI) {
                char *name = NULL;
                for (int l = len; l >= NLA_HDRLEN; ) {
                    if (a->nla_type == CTRL_ATTR_FAMILY_NAME) name = (char *)a + NLA_HDRLEN;
                    l -= NLA_ALIGN(a->nla_len);
                    a = (void *)((char *)a + NLA_ALIGN(a->nla_len));
                }
                if (name && strcmp(name, "nlctrl") == 0)
                    printf("  family nlctrl\n");
                continue;
            }
            attrs(a, len, 0);
            if (!(h->nlmsg_flags & NLM_F_MULTI))
                return;
        }
    }
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    fd = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_GENERIC);
    if (fd < 0) {
        printf("socket errno %d\n", errno);
        return 0;
    }
    struct timeval tv = {2, 0};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    request(GENL_ID_CTRL, 0, CTRL_CMD_GETFAMILY, CTRL_ATTR_FAMILY_NAME, "nlctrl", 7);
    replies("GETFAMILY nlctrl");
    unsigned short id = GENL_ID_CTRL;
    request(GENL_ID_CTRL, NLM_F_ACK, CTRL_CMD_GETFAMILY, CTRL_ATTR_FAMILY_ID, &id, 2);
    replies("GETFAMILY id 16, with ACK");
    replies("ACK");
    request(GENL_ID_CTRL, 0, CTRL_CMD_GETFAMILY, CTRL_ATTR_FAMILY_NAME, "tuxport-none", 12);
    replies("GETFAMILY tuxport-none");
    request(GENL_ID_CTRL, 0, CTRL_CMD_GETFAMILY, 0, NULL, 0);
    replies("GETFAMILY without attributes");
    request(0x7ff0, 0, 1, 0, NULL, 0);
    replies("unknown family id");
    request(GENL_ID_CTRL, 0, 99, 0, NULL, 0);
    replies("unknown nlctrl command");
    request(GENL_ID_CTRL, NLM_F_DUMP, CTRL_CMD_GETFAMILY, 0, NULL, 0);
    replies("dump (only nlctrl shown)");
    request(GENL_ID_CTRL, 0, 10 /* CTRL_CMD_GETPOLICY */, CTRL_ATTR_FAMILY_NAME, "tuxport-none", 12);
    replies("GETPOLICY without NLM_F_DUMP");
    request(GENL_ID_CTRL, NLM_F_DUMP, 10, CTRL_ATTR_FAMILY_NAME, "tuxport-none", 12);
    replies("GETPOLICY tuxport-none");
    request(GENL_ID_CTRL, NLM_F_DUMP, 10, 0, NULL, 0);
    replies("GETPOLICY without attributes");
    struct timeval short_wait = {0, 100000};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &short_wait, sizeof short_wait);
    replies("nothing left to read (SO_RCVTIMEO)");
    return 0;
}
C
gcc -O1 -o genl genl.c
./genl

# Routing tables: RTM_GETROUTE dumps and single lookups (through a gateway, the same subnet, a
# local address, the loopback subnet, subnet broadcast, multicast, ::1, no IPv6 route), the
# RTM_GETRULE dump, compared byte by byte; the RTM_GETNEIGH dump ending with NLMSG_DONE. How a
# dump is split into datagrams varies with the kernel version (from 6.10 on some dumps merge
# NLMSG_DONE into the last datagram), so only the messages are looked at. Local addresses are
# printed as SELF and interface indexes as names; the reference count and last-use time of route
# cache entries in rta_cacheinfo are not deterministic, so only the error code is compared
cat > route.c <<'C'
#include <arpa/inet.h>
#include <errno.h>
#include <net/if.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

/* From linux/netlink.h and linux/rtnetlink.h (the store has no kernel headers) */
#define NETLINK_ROUTE 0
#define NLM_F_REQUEST 1
#define NLM_F_MULTI 2
#define NLM_F_DUMP 0x300
#define NLMSG_ERROR 2
#define NLMSG_DONE 3
#define RTM_GETROUTE 26
#define RTM_GETNEIGH 30
#define RTM_GETRULE 34
#define RTA_DST 1
#define RTA_OIF 4
#define RTA_CACHEINFO 12
struct nlmsghdr {
    uint32_t nlmsg_len;
    uint16_t nlmsg_type, nlmsg_flags;
    uint32_t nlmsg_seq, nlmsg_pid;
};
struct rtattr {
    uint16_t rta_len, rta_type;
};

static int fd;
static unsigned char self[4];

static void request(int type, int flags, int family, const void *dst, int len)
{
    unsigned char buf[64] = {0};
    struct nlmsghdr *h = (void *)buf;
    unsigned char *r = buf + 16; /* struct rtmsg */
    h->nlmsg_len = 16 + 12;
    h->nlmsg_type = type;
    h->nlmsg_flags = NLM_F_REQUEST | flags;
    h->nlmsg_seq = 1;
    r[0] = family;
    if (dst) {
        struct rtattr *a = (void *)(buf + 28);
        a->rta_len = 4 + len;
        a->rta_type = RTA_DST;
        memcpy(a + 1, dst, len);
        h->nlmsg_len += (a->rta_len + 3) & ~3;
        r[1] = len * 8;
    }
    send(fd, buf, h->nlmsg_len, 0);
}

static void attribute(struct rtattr *a)
{
    unsigned char *d = (void *)(a + 1);
    int n = a->rta_len - 4;
    printf("  %d:", a->rta_type);
    if (a->rta_type == RTA_OIF && n == 4) {
        char name[IF_NAMESIZE] = "?";
        if_indextoname(*(unsigned *)d, name);
        printf(" %s\n", name);
        return;
    }
    if (n == 4 && !memcmp(d, self, 4)) {
        printf(" SELF\n");
        return;
    }
    if (a->rta_type == RTA_CACHEINFO && n == 32) {
        printf(" error %d\n", ((int *)d)[3]);
        return;
    }
    for (int i = 0; i < n; i++)
        printf(" %02x", d[i]);
    printf("\n");
}

/* Read replies until NLMSG_DONE, an error, or a message that is not NLM_F_MULTI */
static void replies(const char *what)
{
    unsigned char buf[16384];
    printf("%s\n", what);
    for (;;) {
        int n = recv(fd, buf, sizeof buf, 0);
        if (n < 0) {
            printf(" recv errno %d\n", errno);
            return;
        }
        for (struct nlmsghdr *h = (void *)buf; n >= 16 && h->nlmsg_len <= (unsigned)n;) {
            printf(" type %d flags %#x\n", h->nlmsg_type, h->nlmsg_flags);
            if (h->nlmsg_type == NLMSG_DONE)
                return;
            if (h->nlmsg_type == NLMSG_ERROR) {
                printf("  error %d, %d bytes\n", *(int *)(h + 1), h->nlmsg_len);
                return;
            }
            unsigned char *p = (void *)(h + 1);
            printf("  hdr");
            for (int i = 0; i < 12; i++)
                printf(" %02x", p[i]);
            printf("\n");
            for (unsigned off = 16 + 12; off + 4 <= h->nlmsg_len;) {
                struct rtattr *a = (void *)((char *)h + off);
                attribute(a);
                off += (a->rta_len + 3) & ~3;
            }
            if (!(h->nlmsg_flags & NLM_F_MULTI))
                return;
            int len = (h->nlmsg_len + 3) & ~3;
            n -= len;
            h = (void *)((char *)h + len);
        }
    }
}

static void get4(const char *what, uint32_t addr)
{
    request(RTM_GETROUTE, 0, AF_INET, &addr, 4);
    replies(what);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    struct ifreq ifr = {0};
    strcpy(ifr.ifr_name, "eth0");
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    ioctl(s, SIOCGIFADDR, &ifr);
    uint32_t addr = ((struct sockaddr_in *)&ifr.ifr_addr)->sin_addr.s_addr;
    ioctl(s, SIOCGIFNETMASK, &ifr);
    uint32_t mask = ((struct sockaddr_in *)&ifr.ifr_netmask)->sin_addr.s_addr;
    memcpy(self, &addr, 4);

    fd = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    struct timeval tv = {2, 0};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    request(RTM_GETROUTE, NLM_F_DUMP, AF_UNSPEC, NULL, 0);
    replies("dump: all families");
    request(RTM_GETROUTE, NLM_F_DUMP, AF_INET6, NULL, 0);
    replies("dump: only IPv6");
    get4("get 1.1.1.1", inet_addr("1.1.1.1"));
    get4("get the gateway's subnet", (addr & mask) | htonl(1));
    get4("get SELF", addr);
    get4("get 127.0.0.9", inet_addr("127.0.0.9"));
    get4("get the subnet broadcast", addr | ~mask);
    get4("get 224.0.0.1", inet_addr("224.0.0.1"));
    get4("get 239.1.2.3", inet_addr("239.1.2.3"));
    struct in6_addr one = IN6ADDR_LOOPBACK_INIT, doc;
    inet_pton(AF_INET6, "2001:db8::1", &doc);
    request(RTM_GETROUTE, 0, AF_INET6, &one, 16);
    replies("get ::1");
    request(RTM_GETROUTE, 0, AF_INET6, &doc, 16);
    replies("get 2001:db8::1");
    request(RTM_GETRULE, NLM_F_DUMP, AF_INET, NULL, 0);
    replies("rules: IPv4");
    request(RTM_GETRULE, NLM_F_DUMP, AF_UNSPEC, NULL, 0);
    replies("rules: all families");
    /* For the neighbor table only check that the dump ends with NLMSG_DONE: on Linux earlier
     * test cases left entries on lo */
    request(RTM_GETNEIGH, NLM_F_DUMP, AF_UNSPEC, NULL, 0);
    for (;;) {
        unsigned char buf[16384];
        int n = recv(fd, buf, sizeof buf, 0);
        struct nlmsghdr *h = (void *)buf, *last = h;
        for (; n >= 16 && h->nlmsg_len >= 16 && h->nlmsg_len <= (unsigned)n;
             n -= (h->nlmsg_len + 3) & ~3, h = (void *)((char *)h + ((h->nlmsg_len + 3) & ~3)))
            last = h;
        if (n < 0 || last->nlmsg_type == NLMSG_DONE || last->nlmsg_type == NLMSG_ERROR) {
            printf("neighbours: ends with type %d\n", n < 0 ? -1 : last->nlmsg_type);
            break;
        }
    }
    return 0;
}
C
gcc -O1 -o route route.c
./route

# Interfaces and addresses: RTM_GETADDR dumps (IPv4, IPv6) compared byte by byte (not looking at
# how they are split into datagrams, as above), without the creation and update times in
# ifa_cacheinfo; RTM_GETLINK dumps compare ifinfomsg and the few attributes ip link uses (Linux
# also has some forty attributes such as device parameters, which are not here); RTM_GETLINK
# without NLM_F_DUMP looks up by index and by name, not found, no criteria given, ack requested.
# Local addresses are printed as SELF, interface indexes as names, and for MAC addresses only the
# length is looked at
cat > links.c <<'C'
#include <errno.h>
#include <net/if.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <netinet/in.h>
#include <unistd.h>

struct nlmsghdr {
    uint32_t nlmsg_len;
    uint16_t nlmsg_type, nlmsg_flags;
    uint32_t nlmsg_seq, nlmsg_pid;
};

static int fd;
static unsigned char self[4];

static void request(int type, int flags, int family, int index, const char *name)
{
    unsigned char req[64] = {0};
    struct nlmsghdr *h = (void *)req;
    int hl = type == 18 ? 16 : 8;
    h->nlmsg_len = 16 + hl;
    h->nlmsg_type = type;
    h->nlmsg_flags = 1 | flags;
    h->nlmsg_seq = 1;
    req[16] = family;
    if (type == 18)
        memcpy(req + 20, &index, 4);
    if (name) {
        int n = strlen(name) + 1;
        uint16_t len = 4 + n, ty = 3;
        memcpy(req + h->nlmsg_len, &len, 2);
        memcpy(req + h->nlmsg_len + 2, &ty, 2);
        memcpy(req + h->nlmsg_len + 4, name, n);
        h->nlmsg_len += (len + 3) & ~3;
    }
    send(fd, req, h->nlmsg_len, 0);
}

static void index_name(const unsigned char *p)
{
    int index;
    char name[IF_NAMESIZE] = "?";
    memcpy(&index, p, 4);
    if_indextoname(index, name);
    printf(" index=%s", name);
}

/* The attributes compared in link messages: name, txqlen, operstate, linkmode, mtu, group,
 * promiscuity, carrier, qdisc, proto_down; for addresses only the length. The numbers of TX and
 * RX queues are not compared: docker's veth allocates them by the number of CPUs */
static int shown_link(int t)
{
    return t == 3 || t == 13 || t == 16 || t == 17 || t == 4 || t == 27 || t == 30 || t == 33 ||
           t == 6 || t == 39;
}

/* Attributes of link messages are sorted by type before printing: their order varies with the
 * kernel version (around 6.10 IFLA_QDISC moved further back); address messages keep the
 * original order */
static void attribute(struct nlmsghdr *h, int link, unsigned off)
{
    uint16_t len, t;
    memcpy(&len, (char *)h + off, 2);
    memcpy(&t, (char *)h + off + 2, 2);
    t &= 0x3fff;
    unsigned char *d = (unsigned char *)h + off + 4;
    int n = len - 4;
    if (link && (t == 1 || t == 2)) {
        printf("  %d: %d bytes\n", t, n);
        return;
    }
    printf("  %d:", t);
    if (n == 4 && !memcmp(d, self, 4))
        printf(" SELF");
    else if (!link && t == 6 && n == 16)
        for (int i = 0; i < 8; i++)
            printf(" %02x", d[i]);
    else
        for (int i = 0; i < n; i++)
            printf(" %02x", d[i]);
    printf("\n");
}

static void message(struct nlmsghdr *h)
{
    unsigned char *p = (void *)(h + 1);
    int link = h->nlmsg_type == 16, hl = link ? 16 : 8;
    printf(" type %d flags %#x\n  hdr", h->nlmsg_type, h->nlmsg_flags);
    for (int i = 0; i < hl; i++)
        if (!(link ? i >= 4 && i < 8 : i >= 4))
            printf(" %02x", p[i]);
    index_name(p + 4);
    printf("\n");
    for (int t = link ? 0 : -1; t < (link ? 64 : 0); t++)
        for (unsigned off = 16 + hl; off + 4 <= h->nlmsg_len;) {
            uint16_t len, ty;
            memcpy(&len, (char *)h + off, 2);
            memcpy(&ty, (char *)h + off + 2, 2);
            ty &= 0x3fff;
            if (!link || (ty == t && (shown_link(ty) || ty == 1 || ty == 2)))
                attribute(h, link, off);
            off += (len + 3) & ~3;
        }
}

static void replies(const char *what)
{
    unsigned char buf[65536];
    printf("%s\n", what);
    for (;;) {
        int n = recv(fd, buf, sizeof buf, 0);
        if (n < 0) {
            printf(" recv errno %d\n", errno);
            return;
        }
        for (struct nlmsghdr *h = (void *)buf; n >= 16 && h->nlmsg_len <= (unsigned)n;) {
            if (h->nlmsg_type == 3) {
                printf(" done\n");
                return;
            }
            if (h->nlmsg_type == 2) {
                printf(" error %d flags %#x, %u bytes\n", *(int *)(h + 1), h->nlmsg_flags, h->nlmsg_len);
                return;
            }
            message(h);
            if (!(h->nlmsg_flags & 2))
                return;
            int len = (h->nlmsg_len + 3) & ~3;
            n -= len;
            h = (void *)((char *)h + len);
        }
    }
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    struct ifreq ifr = {0};
    strcpy(ifr.ifr_name, "eth0");
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    ioctl(s, SIOCGIFADDR, &ifr);
    memcpy(self, &((struct sockaddr_in *)&ifr.ifr_addr)->sin_addr, 4);
    fd = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, 0);
    struct timeval tv = {2, 0};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    request(22, 0x300, AF_INET, 0, NULL);
    replies("addresses: IPv4");
    request(22, 0x300, AF_INET6, 0, NULL);
    replies("addresses: IPv6");
    request(18, 0x300, 0, 0, NULL);
    replies("links");
    request(18, 0, 0, 1, NULL);
    replies("link 1");
    request(18, 0, 0, 0, "eth0");
    replies("link eth0");
    request(18, 0, 0, 0, "nosuch0");
    replies("link nosuch0");
    request(18, 0, 0, 0, NULL);
    replies("link without index or name");
    request(18, 4, 0, 1, NULL);
    replies("link 1 with ACK");
    replies("ACK");
    return 0;
}
C
gcc -O1 -o links links.c
./links
# busybox's ip (the packages include iproute2, and /sbin/ip is iproute2's): MAC addresses and
# docker's veth-specific @ifN and M-DOWN are not compared
self=$(busybox ip -4 addr show eth0 | awk '/inet / { split($2, a, "/"); print a[1] }')
norm() { sed -e "s/$self/SELF/g" -e 's/@if[0-9]*//' -e 's/,M-DOWN//' -e 's|link/ether [0-9a-f:]*|link/ether MAC|'; }
busybox ip addr | norm
busybox ip link | norm
cat /sys/class/net/lo/tx_queue_len /sys/class/net/lo/operstate

# The same tables read through busybox's ip, route and netstat and through /proc/net; local
# addresses are replaced with SELF, and the reference counts of ipv6_route are not compared
self=$(busybox ip -4 addr show eth0 | awk '/inet / { split($2, a, "/"); print a[1] }')
busybox ip route show table all | sed "s/$self/SELF/g"
busybox ip -6 route show table all
busybox ip rule
busybox ip -6 rule
route -n
netstat -rn
busybox ip route get 1.1.1.1 | sed "s/$self/SELF/g"
cat /proc/net/route
awk '{ $7 = "R"; print }' /proc/net/ipv6_route

# /proc/net/sockstat, sockstat6: the change of each item before and after opening a few sockets
# (the absolute numbers include other sockets in the namespace, and TCP's alloc is system-wide)
python3 - <<'PY'
import socket
import time


def counts():
    out = {}
    for name in ("sockstat", "sockstat6"):
        with open("/proc/net/" + name) as f:
            for line in f:
                proto, rest = line.split(":")
                fields = rest.split()
                for k, v in zip(fields[::2], fields[1::2]):
                    out[proto + " " + k] = int(v)
    return out


def settled():
    """Sockets closed by earlier test cases are freed by the kernel a little later, so "sockets
    used" keeps dropping: wait until it stays unchanged for 0.1 seconds in a row (at most 3
    seconds) before reading. It used not to wait, and on Linux hosts the native side came up two
    or more short about once in several runs."""
    last = counts()
    stable = 0
    for _ in range(60):
        time.sleep(0.05)
        cur = counts()
        stable = stable + 1 if cur["sockets used"] == last["sockets used"] else 0
        last = cur
        if stable >= 2:
            break
    return last


before = settled()
keep = []
for family, host in ((socket.AF_INET, "127.0.0.1"), (socket.AF_INET6, "::1")):
    listener = socket.socket(family)
    listener.bind((host, 0))
    listener.listen()
    client = socket.create_connection(listener.getsockname()[:2])
    server, _ = listener.accept()
    bound = socket.socket(family, socket.SOCK_DGRAM)
    bound.bind((host, 0))
    unbound = socket.socket(family, socket.SOCK_DGRAM)
    closed = socket.socket(family)
    keep += [listener, client, server, bound, unbound, closed]
keep.append(socket.socket(socket.AF_INET, socket.SOCK_RAW, socket.IPPROTO_ICMP))
after = counts()
for k in sorted(before):
    if k not in ("TCP alloc", "TCP mem", "UDP mem", "sockets used"):
        print(k, after[k] - before[k])
# The total in the namespace: on Linux it is one lower when the sockets of earlier test cases are
# freed a little late
print("sockets used grew by at least 12:", after["sockets used"] - before["sockets used"] >= 12)
PY

# The IDs shown for user and group IDs that cannot be mapped: bubblewrap reads these two under
# kernel at startup
cat /proc/sys/kernel/overflowuid /proc/sys/kernel/overflowgid /proc/sys/fs/overflowuid \
    /proc/sys/fs/overflowgid

# /proc/net/snmp, netstat: one line of field names and one line of values per group; fields grow
# with kernel versions (Icmp's are even inserted in the middle), so only the few items every
# version has are looked at. The changes of a few counters before and after connecting, failing
# to connect, and sending and receiving datagrams on loopback; IP packet counts and TCP segments,
# for which Linux also counts handshakes and ACKs, are not compared
python3 - <<'PY'
import errno
import select
import socket


def groups(name):
    with open("/proc/net/" + name) as f:
        lines = f.read().splitlines()
    out = {}
    for names, values in zip(lines[::2], lines[1::2]):
        group, names = names.split(":")
        same, values = values.split(":")
        names, values = names.split(), values.split()
        if same != group or len(names) != len(values):
            print(name, group, "is malformed")
        out[group] = dict(zip(names, map(int, values)))
    return out


snmp, netstat = groups("snmp"), groups("netstat")
print("snmp groups:", [g for g in snmp if g != "IcmpMsg"])
print("netstat groups:", list(netstat))
print("Ip DefaultTTL", snmp["Ip"]["DefaultTTL"])
for k in ("RtoAlgorithm", "RtoMin", "RtoMax", "MaxConn"):
    print("Tcp", k, snmp["Tcp"][k])
print("IpExt has InOctets:", "InOctets" in netstat["IpExt"])
print("TcpExt has TCPMigrateReqFailure:", "TCPMigrateReqFailure" in netstat["TcpExt"])

WATCH = [("Tcp", f) for f in ("ActiveOpens", "PassiveOpens", "AttemptFails", "CurrEstab")]
WATCH += [("Udp", f) for f in ("InDatagrams", "OutDatagrams")]


def counts():
    s = groups("snmp")
    return {g + " " + f: s[g][f] for g, f in WATCH}


def changes(what, before):
    after = counts()
    print(what + ":", " ".join(f"{k}={after[k] - before[k]}" for k in after if after[k] != before[k]))


before = counts()
keep = []
for family, host in ((socket.AF_INET, "127.0.0.1"), (socket.AF_INET6, "::1")):
    listener = socket.socket(family)
    listener.bind((host, 0))
    listener.listen()
    client = socket.create_connection(listener.getsockname()[:2])
    server, _ = listener.accept()
    keep += [listener, client, server]
changes("two connections", before)
before = counts()
for s in keep:
    s.close()
changes("closed", before)

before = counts()
closed = socket.socket()
closed.bind(("127.0.0.1", 0))
port = closed.getsockname()[1]
blocking = socket.socket()
print("blocking connect:", errno.errorcode[blocking.connect_ex(("127.0.0.1", port))])
blocking.close()
nonblocking = socket.socket()
nonblocking.setblocking(False)
r = nonblocking.connect_ex(("127.0.0.1", port))
if r == errno.EINPROGRESS:
    select.select([], [nonblocking], [], 5)
    r = nonblocking.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR)
print("non-blocking connect:", errno.errorcode[r])
nonblocking.close()
closed.close()
changes("refused twice", before)

# IPv4 and IPv4-mapped addresses count as Udp, IPv6 ones do not
before = counts()
v4 = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
v4.bind(("127.0.0.1", 0))
v6 = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
v6.bind(("::1", 0))
mapped = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
for i in range(3):
    v4.sendto(b"x", v4.getsockname())
mapped.sendto(b"x", ("::ffff:127.0.0.1", v4.getsockname()[1]))
v6.sendto(b"x", v6.getsockname()[:2])
for i in range(4):
    v4.recvfrom(10)
v6.recvfrom(10)
for s in (v4, v6, mapped):
    s.close()
changes("datagrams", before)
PY

# File capabilities (security.capability) and ambient capabilities: run programs with different
# capabilities as nobody and others, and look at the capability sets, AT_SECURE and the effective
# user ID; the permissions to set, read and remove the extended attribute; writing, truncating
# and chown removing file capabilities and the setuid and setgid bits; securebits locks. Only the
# few capabilities used are printed, and a set equal to the bounding set prints as all (docker's
# default bounding set is smaller than Tuxport's)
cat > showcaps.c <<'C'
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>
#include <unistd.h>

static unsigned long long field(const char *name)
{
    char line[256];
    unsigned long long v = 0;
    FILE *f = fopen("/proc/self/status", "r");
    while (fgets(line, sizeof line, f))
        if (!strncmp(line, name, strlen(name)))
            sscanf(line + strlen(name), "%llx", &v);
    fclose(f);
    return v;
}

static void set(const char *label, unsigned long long v, unsigned long long bnd)
{
    static const struct { int bit; const char *name; } caps[] = {
        {0, "chown"}, {5, "kill"}, {10, "net_bind_service"}, {13, "net_raw"},
    };
    printf(" %s=", label);
    if (v && v == bnd) {
        printf("all");
        return;
    }
    int any = 0;
    for (unsigned i = 0; i < sizeof caps / sizeof *caps; i++)
        if (v & 1ULL << caps[i].bit)
            printf("%s%s", any++ ? "," : "", caps[i].name);
    if (!any)
        printf("-");
}

int main(void)
{
    unsigned long long bnd = field("CapBnd:");
    printf("  euid=%d", (int)geteuid());
    set("prm", field("CapPrm:"), bnd);
    set("eff", field("CapEff:"), bnd);
    set("inh", field("CapInh:"), bnd);
    set("amb", field("CapAmb:"), bnd);
    printf(" secure=%lu\n", getauxval(AT_SECURE));
    return 0;
}
C
gcc -O1 -o showcaps showcaps.c
cat > fcaps.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <grp.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

/* From linux/capability.h (the store has no kernel headers) */
struct cap_header { uint32_t version; int pid; };
struct cap_data { uint32_t effective, permitted, inheritable; };
#define CAP_V3 0x20080522
#define CHOWN (1u << 0)
#define NET_BIND (1u << 10)
#define NET_RAW (1u << 13)
#define NOBODY 65534

static const char *showcaps;

/* struct vfs_cap_data: the version (plus the effective bit), two sets of permitted and
 * inheritable, and for v3 also rootid */
static int setcaps(const char *path, uint32_t magic, uint32_t p, uint32_t i, int v3, uint32_t rootid)
{
    uint32_t v[6] = {magic, p, i, 0, 0, rootid};
    return setxattr(path, "security.capability", v, v3 ? 24 : 20, 0);
}

static void copy(const char *to, int mode, int uid)
{
    char cmd[256];
    snprintf(cmd, sizeof cmd, "cp %s %s", showcaps, to);
    system(cmd);
    chown(to, uid, uid);
    chmod(to, mode);
}

static void result(const char *what, int r)
{
    printf("%s: %s\n", what, r < 0 ? strerror(errno) : "ok");
}

static int caps_xattr_size(const char *path)
{
    char v[64];
    int n = getxattr(path, "security.capability", v, sizeof v);
    return n < 0 ? -errno : n;
}

enum { NNP = 1, KEEP_INHERITABLE = 2, AMBIENT = 4, DROP_CHOWN = 8, NOROOT = 16 };

/* Run path as uid; with KEEP_INHERITABLE keep net_raw and net_bind_service in the inheritable
 * set, and with AMBIENT also raise net_raw to an ambient capability */
static void run(const char *label, const char *path, int uid, int opts)
{
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        if (opts & DROP_CHOWN)
            prctl(PR_CAPBSET_DROP, 0, 0, 0, 0);
        if (opts & NOROOT)
            prctl(PR_SET_SECUREBITS, 1, 0, 0, 0);
        if (opts & (KEEP_INHERITABLE | AMBIENT))
            prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0);
        if (uid) {
            setgroups(0, NULL);
            setresgid(uid, uid, uid);
            setresuid(uid, uid, uid);
        }
        if (opts & (KEEP_INHERITABLE | AMBIENT)) {
            struct cap_header h = {CAP_V3, 0};
            struct cap_data d[2] = {{0, NET_RAW | NET_BIND, NET_RAW | NET_BIND}};
            if (syscall(SYS_capset, &h, d) < 0)
                printf("capset: %s\n", strerror(errno));
        }
        if (opts & AMBIENT) {
            if (prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, 13, 0, 0) < 0)
                printf("ambient raise: %s\n", strerror(errno));
            printf("ambient is set: %d\n", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, 13, 0, 0));
            fflush(stdout);
        }
        if (opts & NNP)
            prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        printf("%s\n", label);
        fflush(stdout);
        char *argv[] = {(char *)path, NULL};
        execv(path, argv);
        printf("  execve: %s\n", strerror(errno));
        _exit(1);
    }
    waitpid(pid, NULL, 0);
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    showcaps = argv[1];
    mkdir("/tmp/fcaps", 0755);
    chdir("/tmp/fcaps");
    copy("plain", 0755, 0);
    copy("ep", 0755, 0);
    result("set net_raw+ep", setcaps("ep", 0x02000001, NET_RAW, 0, 0, 0));
    copy("p", 0755, 0);
    result("set net_raw+p", setcaps("p", 0x02000000, NET_RAW, 0, 0, 0));
    copy("i", 0755, 0);
    result("set net_bind_service+ie", setcaps("i", 0x02000001, 0, NET_BIND, 0, 0));
    copy("v3root", 0755, 0);
    result("set v3 rootid 0", setcaps("v3root", 0x03000001, NET_RAW, 0, 1, 0));
    printf("v3 rootid 0 reads back as %d bytes\n", caps_xattr_size("v3root"));
    copy("v3other", 0755, 0);
    result("set v3 rootid 1000", setcaps("v3other", 0x03000001, NET_RAW, 0, 1, 1000));
    printf("v3 rootid 1000 reads back as %d bytes\n", caps_xattr_size("v3other"));
    copy("suid", 04755, 0);
    result("set net_raw+ep on setuid root", setcaps("suid", 0x02000001, NET_RAW, 0, 0, 0));
    copy("chown", 0755, 0);
    result("set chown+ep", setcaps("chown", 0x02000001, CHOWN, 0, 0, 0));
    uint32_t v1[3] = {0x01000000, NET_RAW, 0};
    result("set v1", setxattr("plain", "security.capability", v1, sizeof v1, 0));
    result("set unknown revision", setcaps("plain", 0x04000000, NET_RAW, 0, 0, 0));
    result("set the wrong length", setxattr("plain", "security.capability", v1, 16, 0));

    run("nobody runs plain", "./plain", NOBODY, 0);
    run("nobody runs net_raw+ep", "./ep", NOBODY, 0);
    run("nobody runs net_raw+p", "./p", NOBODY, 0);
    run("nobody runs v3 rootid 0", "./v3root", NOBODY, 0);
    run("nobody runs v3 rootid 1000", "./v3other", NOBODY, 0);
    run("nobody with inheritable caps runs net_bind_service+ie", "./i", NOBODY, KEEP_INHERITABLE);
    run("nobody with ambient net_raw runs plain", "./plain", NOBODY, AMBIENT);
    run("nobody with ambient net_raw runs net_raw+ep", "./ep", NOBODY, AMBIENT);
    run("nobody runs setuid root with net_raw+ep", "./suid", NOBODY, 0);
    run("nobody with no_new_privs runs net_raw+ep", "./ep", NOBODY, NNP);
    run("nobody without chown in the bounding set runs chown+ep", "./chown", NOBODY, DROP_CHOWN);
    run("root runs net_raw+ep", "./ep", 0, 0);
    run("root with SECBIT_NOROOT runs plain", "./plain", 0, NOROOT);
    run("root with SECBIT_NOROOT runs net_raw+ep", "./ep", 0, NOROOT);

    /* Permissions of the extended attribute */
    copy("own", 0755, NOBODY);
    fflush(stdout);
    if (fork() == 0) {
        setgroups(0, NULL);
        setresgid(NOBODY, NOBODY, NOBODY);
        setresuid(NOBODY, NOBODY, NOBODY);
        result("nobody sets security.capability on its file", setcaps("own", 0x02000001, NET_RAW, 0, 0, 0));
        printf("nobody reads security.capability: %d bytes\n", caps_xattr_size("ep"));
        result("nobody removes security.capability", removexattr("ep", "security.capability"));
        result("nobody sets security.tuxport", setxattr("own", "security.tuxport", "x", 1, 0));
        result("nobody sets trusted.tuxport", setxattr("own", "trusted.tuxport", "x", 1, 0));
        char v[8];
        result("nobody reads trusted.tuxport", getxattr("own", "trusted.tuxport", v, sizeof v));
        result("nobody sets user.tuxport on its file", setxattr("own", "user.tuxport", "x", 1, 0));
        result("nobody sets user.tuxport on root's file", setxattr("plain", "user.tuxport", "x", 1, 0));
        _exit(0);
    }
    wait(NULL);

    /* Writing, truncating and chown remove privileges */
    int fd = open("ep", O_WRONLY | O_APPEND);
    write(fd, "", 0);
    printf("after writing nothing: %d\n", caps_xattr_size("ep"));
    write(fd, "x", 1);
    close(fd);
    printf("after root writes: %d\n", caps_xattr_size("ep"));
    truncate("p", 100000);
    printf("after truncate: %d\n", caps_xattr_size("p"));
    close(open("i", O_WRONLY | O_TRUNC));
    printf("after O_TRUNC: %d\n", caps_xattr_size("i"));
    chown("suid", -1, -1);
    struct stat st;
    stat("suid", &st);
    printf("after chown(-1, -1): %d, mode %o\n", caps_xattr_size("suid"), st.st_mode & 07777);
    copy("rootsuid", 04755, 0);
    fd = open("rootsuid", O_WRONLY | O_APPEND);
    write(fd, "x", 1);
    close(fd);
    stat("rootsuid", &st);
    printf("root writes a setuid file: mode %o\n", st.st_mode & 07777);
    copy("nobodysuid", 06755, NOBODY);
    copy("nobodylock", 02644, NOBODY);
    fflush(stdout);
    if (fork() == 0) {
        setgroups(0, NULL);
        setresgid(NOBODY, NOBODY, NOBODY);
        setresuid(NOBODY, NOBODY, NOBODY);
        const char *files[] = {"nobodysuid", "nobodylock"};
        for (int i = 0; i < 2; i++) {
            int f = open(files[i], O_WRONLY | O_APPEND);
            write(f, "x", 1);
            close(f);
            stat(files[i], &st);
            printf("nobody writes %s: mode %o\n", files[i], st.st_mode & 07777);
        }
        _exit(0);
    }
    wait(NULL);

    /* securebits locks and the prctl of ambient capabilities */
    fflush(stdout);
    if (fork() == 0) {
        printf("lock keepcaps: %d\n", prctl(PR_SET_SECUREBITS, 1 << 5, 0, 0, 0));
        /* From 6.14 on bits 8 and 10 are defined (SECBIT_EXEC_*), so use bit 20, which no version
         * defines */
        printf("then set keepcaps: %d %s\n", prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0), strerror(errno));
        printf("then unlock: %d %s\n", prctl(PR_SET_SECUREBITS, 0, 0, 0, 0), strerror(errno));
        printf("unknown bit: %d %s\n", prctl(PR_SET_SECUREBITS, 1 << 5 | 1 << 20, 0, 0, 0), strerror(errno));
        printf("securebits: %d\n", prctl(PR_GET_SECUREBITS, 0, 0, 0, 0));
        printf("raise without inheritable: %d %s\n", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, 13, 0, 0), strerror(errno));
        printf("clear all with an argument: %d %s\n", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 1, 0, 0), strerror(errno));
        printf("lower: %d\n", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_LOWER, 13, 0, 0));
        _exit(0);
    }
    wait(NULL);
    return 0;
}
C
gcc -O1 -o fcaps fcaps.c
./fcaps "$PWD/showcaps"
rm -rf /tmp/fcaps

# The epoll-, timerfd- and signalfd-specific lines in fdinfo; device numbers in mountinfo agree
# with stat, and the mnt_id of fdinfo can be found in mountinfo
cat > fdinfo.c <<'C'
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/signalfd.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/timerfd.h>
#include <unistd.h>

static void show(int fd)
{
    unsigned tfds[8] = {0};
    unsigned long long datas[8];
    char path[64], line[256];
    snprintf(path, sizeof path, "/proc/self/fdinfo/%d", fd);
    FILE *f = fopen(path, "r");
    while (fgets(line, sizeof line, f)) {
        unsigned tfd, events;
        unsigned long long data, sec, nsec;
        /* Linux orders the interests by its internal tree, so the order is not fixed: sort by fd */
        if (sscanf(line, "tfd: %u events: %x data: %llx", &tfd, &events, &data) == 3)
            tfds[tfd < 8 ? tfd : 7] = events, datas[tfd < 8 ? tfd : 7] = data;
        else if (sscanf(line, "it_value: (%llu, %llu)", &sec, &nsec) == 2)
            printf("  it_value %llu\n", sec);
        else if (!strncmp(line, "clockid", 7) || !strncmp(line, "ticks", 5) || !strncmp(line, "settime", 7) ||
                 !strncmp(line, "it_interval", 11) || !strncmp(line, "sigmask", 7))
            printf("  %s", line);
    }
    fclose(f);
    for (int i = 0; i < 8; i++)
        if (tfds[i])
            printf("  tfd %d events %x data %llx\n", i, tfds[i], datas[i]);
}

static int mnt_id(int fd)
{
    char path[64], line[256];
    int id = -1;
    snprintf(path, sizeof path, "/proc/self/fdinfo/%d", fd);
    FILE *f = fopen(path, "r");
    while (fgets(line, sizeof line, f))
        sscanf(line, "mnt_id: %d", &id);
    fclose(f);
    return id;
}

/* The line in mountinfo whose mount point is `where`: whether the device number agrees with stat,
 * and whether the ID agrees with the fd's mnt_id */
static void mount(const char *where, int fd)
{
    char line[512], point[256], type[64];
    int id, parent;
    unsigned maj, min;
    struct stat st;
    stat(where, &st);
    FILE *f = fopen("/proc/self/mountinfo", "r");
    while (fgets(line, sizeof line, f)) {
        char *dash = strstr(line, " - ");
        if (sscanf(line, "%d %d %u:%u %*s %255s", &id, &parent, &maj, &min, point) != 5 || strcmp(point, where) ||
            !dash || sscanf(dash, " - %63s", type) != 1)
            continue;
        /* The container's root is overlay, here it is ext4: only the type of /proc is looked at */
        printf("%s: type %s dev %s mnt_id %s\n", where, strcmp(where, "/") ? type : "-",
               maj == major(st.st_dev) && min == minor(st.st_dev) ? "same" : "differs",
               id == mnt_id(fd) ? "same" : "differs");
    }
    fclose(f);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    int ep = epoll_create1(0);
    int p[2];
    pipe(p);
    struct epoll_event ev = {.events = EPOLLIN | EPOLLET, .data.u64 = 0x1234};
    epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev);
    ev.events = EPOLLOUT;
    ev.data.u64 = 7;
    epoll_ctl(ep, EPOLL_CTL_ADD, p[1], &ev);
    printf("epoll\n");
    show(ep);
    int t = timerfd_create(CLOCK_MONOTONIC, 0);
    struct itimerspec its = {.it_interval = {3, 0}, .it_value = {100, 0}};
    timerfd_settime(t, 0, &its, NULL);
    printf("timerfd\n");
    show(t);
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    its.it_value.tv_sec = now.tv_sec + 50;
    timerfd_settime(t, TFD_TIMER_ABSTIME, &its, NULL);
    printf("timerfd abstime\n");
    show(t);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGRTMIN);
    printf("signalfd\n");
    show(signalfd(-1, &set, 0));
    int root = open("/", O_RDONLY);
    int proc = open("/proc/self/stat", O_RDONLY);
    int sys = open("/sys/devices/system/cpu/online", O_RDONLY);
    mount("/", root);
    mount("/proc", proc);
    mount("/sys", sys);
    return 0;
}
C
gcc -O1 -o fdinfo fdinfo.c
./fdinfo

# Odds and ends: old-style termio ioctls, RNDGETENTCNT, writing with an offset to the read end of
# a pipe, MADV_REMOVE, MREMAP_DONTUNMAP
cat > misc.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pty.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/uio.h>
#include <termios.h>
#include <unistd.h>

#ifndef MREMAP_DONTUNMAP
#define MREMAP_DONTUNMAP 4
#endif
#define RNDGETENTCNT _IOR('R', 0x00, int)

struct termio_ {
    unsigned short iflag, oflag, cflag, lflag;
    unsigned char line, cc[8];
};

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    int m, s;
    openpty(&m, &s, NULL, NULL, NULL);
    struct termio_ t;
    struct termios ts;
    tcgetattr(s, &ts);
    printf("TCGETA %d lflag same %d cc same %d\n", ioctl(s, TCGETA, &t), t.lflag == (ts.c_lflag & 0xffff),
           !memcmp(t.cc, ts.c_cc, 8));
    t.lflag &= ~ECHO;
    t.cc[VMIN] = 3;
    printf("TCSETA %d\n", ioctl(s, TCSETA, &t));
    tcgetattr(s, &ts);
    printf("echo off %d vmin %d\n", !(ts.c_lflag & ECHO), ts.c_cc[VMIN]);
    printf("TCGETA bad %d %s\n", ioctl(s, TCGETA, (void *)8), strerror(errno));

    int r = open("/dev/urandom", O_RDONLY), cnt = -1;
    printf("RNDGETENTCNT %d %d\n", ioctl(r, RNDGETENTCNT, &cnt), cnt);

    int p[2];
    pipe(p);
    struct iovec iov = {"x", 1};
    printf("pwritev2 read end %zd %s\n", pwritev2(p[0], &iov, 1, 0, 0), strerror(errno));
    printf("pwritev2 read end at -1 %zd %s\n", pwritev2(p[0], &iov, 1, -1, 0), strerror(errno));
    printf("pread write end %zd %s\n", pread(p[1], &cnt, 1, 0), strerror(errno));

    long pg = sysconf(_SC_PAGESIZE);
    char *priv = mmap(NULL, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *shared = mmap(NULL, pg, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    char *ro = mmap(NULL, pg, PROT_READ, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    strcpy(shared, "data");
    printf("remove private %d %s\n", madvise(priv, pg, MADV_REMOVE), strerror(errno));
    printf("remove read-only %d %s\n", madvise(ro, pg, MADV_REMOVE), strerror(errno));
    printf("remove shared %d left %d\n", madvise(shared, pg, MADV_REMOVE), shared[0]);
    mlock(shared, pg);
    printf("remove locked %d %s\n", madvise(shared, pg, MADV_REMOVE), strerror(errno));

    strcpy(priv, "moved");
    char *moved = mremap(priv, pg, pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    printf("dontunmap moved %d content %s old %d\n", moved != priv, moved, priv[0]);
    priv[0] = 1;
    printf("old still writable %d\n", priv[0]);
    printf("dontunmap no maymove %p %s\n", mremap(moved, pg, pg, MREMAP_DONTUNMAP), strerror(errno));
    printf("dontunmap grow %p %s\n", mremap(moved, pg, 2 * pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP),
           strerror(errno));
    return 0;
}
C
gcc -O1 -o misc misc.c
./misc

# smaps, smaps_rollup, pagemap; mlock and MAP_POPULATE bring pages into memory
cat > smaps.c <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

/* The mapping in smaps starting at addr: print a few counters and VmFlags */
static void smaps(const char *what, void *addr)
{
    char line[256];
    int in = 0;
    FILE *f = fopen("/proc/self/smaps", "r");
    printf("%s:", what);
    while (fgets(line, sizeof line, f)) {
        unsigned long start, end, kb;
        char name[32];
        if (sscanf(line, "%lx-%lx", &start, &end) == 2 && strchr(line, ' ') - line == (long)strcspn(line, " ")) {
            if (in)
                break;
            in = start == (unsigned long)addr;
            continue;
        }
        if (!in)
            continue;
        if (sscanf(line, "%31[^:]: %lu kB", name, &kb) == 2 &&
            (!strcmp(name, "Size") || !strcmp(name, "Rss") || !strcmp(name, "Locked") || !strcmp(name, "Anonymous")))
            printf(" %s %lu", name, kb);
        if (!strncmp(line, "VmFlags:", 8))
            printf(" |%s", line + 8);
    }
    fclose(f);
}

static int present(void *addr, int pages)
{
    int fd = open("/proc/self/pagemap", O_RDONLY), n = 0;
    for (int i = 0; i < pages; i++) {
        uint64_t e;
        pread(fd, &e, 8, ((uintptr_t)addr / 4096 + i) * 8);
        n += e >> 63;
    }
    close(fd);
    return n;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    long pg = 4096;
    char *a = mmap(NULL, 16 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    unsigned char vec[16];
    mincore(a, 16 * pg, vec);
    int n = 0;
    for (int i = 0; i < 16; i++)
        n += vec[i] & 1;
    printf("before mlock: resident %d present %d\n", n, present(a, 16));
    mlock(a, 4 * pg);
    mincore(a, 16 * pg, vec);
    n = 0;
    for (int i = 0; i < 16; i++)
        n += vec[i] & 1;
    printf("after mlock: resident %d present %d\n", n, present(a, 16));
    smaps("locked part", a);
    smaps("rest", a + 4 * pg);
    char *b = mmap(NULL, 4 * pg, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    smaps("read-only", b);
    int fd = open("/tmp/smaps-file", O_RDWR | O_CREAT | O_TRUNC, 0644);
    ftruncate(fd, 8 * pg);
    char *c = mmap(NULL, 8 * pg, PROT_READ, MAP_PRIVATE | MAP_POPULATE, fd, 0);
    printf("populated file: present %d\n", present(c, 8));
    int pm = open("/proc/self/pagemap", O_RDONLY);
    uint64_t e;
    printf("unaligned read %zd\n", pread(pm, &e, 4, 0));
    FILE *r = fopen("/proc/self/smaps_rollup", "r");
    char line[256];
    fgets(line, sizeof line, r);
    printf("rollup header ends with [rollup]: %d at column %ld\n", strstr(line, "[rollup]") != NULL,
           strstr(line, "[rollup]") - line);
    /* From 6.6 on there is an extra KSM line: only check whether a few fields are present */
    int fields = 0;
    while (fgets(line, sizeof line, r))
        fields += !strncmp(line, "Rss:", 4) || !strncmp(line, "Pss_Anon:", 9) || !strncmp(line, "Locked:", 7);
    printf("rollup fields %d\n", fields);
    /* Lines in maps without a name end with a space */
    FILE *m = fopen("/proc/self/maps", "r");
    int unnamed = 0, trailing = 0;
    while (fgets(line, sizeof line, m)) {
        char *nl = strchr(line, '\n');
        if (nl && nl[-1] == ' ' && nl[-2] == '0')
            trailing++;
        int cols = 0;
        for (char *t = strtok(line, " \n"); t; t = strtok(NULL, " \n"))
            cols++;
        unnamed += cols == 5;
    }
    printf("unnamed lines all end with a space: %d\n", unnamed == trailing && unnamed > 0);
    return 0;
}
C
gcc -O1 -o smaps smaps.c
./smaps

# Reopening memfds and files still open after deletion through /proc/self/fd; O_NOFOLLOW stopping
# at the link is ELOOP
cat > mfd.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
static void try(const char *what, const char *path, int flags)
{
    int fd = open(path, flags, 0600);
    char buf[16] = {0};
    if (fd < 0) { printf("%s: %s\n", what, strerror(errno)); return; }
    pread(fd, buf, 4, 0);
    printf("%s: ok [%s] flags %o\n", what, buf, fcntl(fd, F_GETFL) & O_ACCMODE);
    close(fd);
}
int main(void)
{
    int m = memfd_create("t", 0);
    write(m, "data", 4);
    char p[64];
    snprintf(p, sizeof p, "/proc/self/fd/%d", m);
    try("rdonly", p, O_RDONLY);
    try("rdwr", p, O_RDWR);
    try("creat", p, O_RDWR | O_CREAT);
    try("creat excl", p, O_RDWR | O_CREAT | O_EXCL);
    try("creat trunc", p, O_RDWR | O_CREAT | O_TRUNC);
    try("nofollow", p, O_RDONLY | O_NOFOLLOW);
    int t = open("/tmp/unlinked", O_RDWR | O_CREAT | O_TRUNC, 0600);
    write(t, "gone", 4);
    unlink("/tmp/unlinked");
    snprintf(p, sizeof p, "/proc/self/fd/%d", t);
    try("unlinked", p, O_RDONLY);
    int pp[2];
    pipe(pp);
    snprintf(p, sizeof p, "/proc/self/fd/%d", pp[1]);
    try("pipe creat", p, O_WRONLY | O_CREAT);
    return 0;
}
C
gcc -O1 -o mfd mfd.c
./mfd

# unshare, ru_maxrss, tkill failing to queue, negative futex arguments, already connected sockets,
# interface ioctls, write seals while writable mappings exist. The cap on timer overrun counts is
# tested by LTP's timer_settime03: a timer with a 1 ns interval causes a signal storm on the
# native side, and the result depends on timing
cat > misc2.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define FUTEX_CMP_REQUEUE 4

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    /* unshare(CLONE_FILES): fds closed afterwards do not affect the process that used to share
     * the fd table */
    int fd = open("/dev/null", O_RDONLY);
    pid_t pid = syscall(SYS_clone, CLONE_FILES | SIGCHLD, 0, 0, 0, 0);
    if (pid == 0) {
        printf("unshare files %d\n", unshare(CLONE_FILES));
        close(fd);
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    printf("fd still open in parent %d\n", fcntl(fd, F_GETFD) != -1);
    pid = syscall(SYS_clone, CLONE_FS | SIGCHLD, 0, 0, 0, 0);
    if (pid == 0) {
        unshare(CLONE_FS);
        chdir("/tmp");
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    char cwd[64];
    printf("cwd after child chdir %s\n", getcwd(cwd, sizeof cwd));
    printf("unshare bad %d %s\n", unshare(-1), strerror(errno));
    printf("unshare vm single-threaded %d\n", unshare(CLONE_VM));

    /* The child used 64 MB: ru_maxrss is at least half of that (under memory pressure some pages
     * may already be swapped out) */
    pid = fork();
    if (pid == 0) {
        char *p = malloc(64 << 20);
        memset(p, 1, 64 << 20);
        _exit(p[100] - 1);
    }
    waitpid(pid, NULL, 0);
    struct rusage ru;
    getrusage(RUSAGE_CHILDREN, &ru);
    printf("children maxrss >= 32MB %d\n", ru.ru_maxrss >= 32 * 1024);

    /* A real-time signal from tkill that cannot be queued is EAGAIN */
    struct rlimit zero = {0, 0}, old;
    getrlimit(RLIMIT_SIGPENDING, &old);
    setrlimit(RLIMIT_SIGPENDING, &zero);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGRTMIN);
    sigprocmask(SIG_BLOCK, &set, NULL);
    printf("tkill %ld %s\n", syscall(SYS_tkill, gettid(), SIGRTMIN), strerror(errno));
    setrlimit(RLIMIT_SIGPENDING, &old);

    int f1 = 0, f2 = 0;
    printf("requeue negative %ld %s\n",
           syscall(SYS_futex, &f1, FUTEX_CMP_REQUEUE, 1, (long)-1, &f2, 0), strerror(errno));

    /* Connecting again after a blocking connect has returned is EISCONN */
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    int l = socket(AF_INET, SOCK_STREAM, 0);
    bind(l, (void *)&a, sizeof a);
    listen(l, 1);
    socklen_t len = sizeof a;
    getsockname(l, (void *)&a, &len);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    connect(c, (void *)&a, sizeof a);
    printf("connect again %d %s\n", connect(c, (void *)&a, sizeof a), strerror(errno));
    printf("SIOCATMARK null %d %s\n", ioctl(c, SIOCATMARK, NULL), strerror(errno));
    int u = socket(AF_INET, SOCK_DGRAM, 0), mark;
    printf("SIOCATMARK udp %d %s\n", ioctl(u, SIOCATMARK, &mark), strerror(errno));
    printf("SIOCSIFFLAGS bad %d %s\n", ioctl(u, SIOCSIFFLAGS, (void *)8), strerror(errno));

    /* A memfd with a writable shared mapping cannot be sealed against writes */
    int m = memfd_create("seal", MFD_ALLOW_SEALING);
    ftruncate(m, 4096);
    void *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, m, 0);
    printf("seal write mapped %d %s\n", fcntl(m, F_ADD_SEALS, F_SEAL_WRITE), strerror(errno));
    munmap(p, 4096);
    printf("seal write unmapped %d\n", fcntl(m, F_ADD_SEALS, F_SEAL_WRITE));
    return 0;
}
C
gcc -O1 -o misc2 misc2.c
./misc2

# Signal-driven I/O (O_ASYNC, F_SETSIG, FIOSETOWN) and file leases. Leases use a file in
# /dev/shm: the native side's root is overlayfs, on which Linux does not allow write leases
cat > async.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void wait_sig(const char *what, sigset_t *set)
{
    siginfo_t si;
    struct timespec t = {5, 0};
    int sig = sigtimedwait(set, &si, &t);
    if (sig < 0) {
        printf("%s: %s\n", what, strerror(errno));
        return;
    }
    printf("%s: sig %s code %d", what, sig == SIGIO ? "SIGIO" : sig == SIGUSR1 ? "SIGUSR1" : "?", si.si_code);
    if (sig == SIGUSR1)
        printf(" fd %d band %lx", si.si_fd, (long)si.si_band);
    printf("\n");
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGIO);
    sigaddset(&set, SIGUSR1);
    sigprocmask(SIG_BLOCK, &set, NULL);
    int p[2];
    pipe(p);
    fcntl(p[0], F_SETOWN, getpid());
    fcntl(p[0], F_SETSIG, SIGUSR1);
    printf("setfl %d async %d\n", fcntl(p[0], F_SETFL, O_ASYNC | O_NONBLOCK),
           (fcntl(p[0], F_GETFL) & O_ASYNC) != 0);
    if (fork() == 0) {
        usleep(100000);
        write(p[1], "x", 1);
        _exit(0);
    }
    wait_sig("pipe", &set);
    char c;
    read(p[0], &c, 1);
    wait(NULL);
    /* The default SIGIO */
    fcntl(p[0], F_SETSIG, 0);
    write(p[1], "y", 1);
    wait_sig("pipe default", &set);
    read(p[0], &c, 1);
    /* No owner: nothing is sent */
    fcntl(p[0], F_SETOWN, 0);
    write(p[1], "z", 1);
    struct timespec zero = {0, 200000000};
    printf("no owner: %d\n", sigtimedwait(&set, NULL, &zero));
    read(p[0], &c, 1);
    /* TCP: the data comes from another process */
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    int l = socket(AF_INET, SOCK_STREAM, 0);
    bind(l, (void *)&a, sizeof a);
    listen(l, 1);
    socklen_t len = sizeof a;
    getsockname(l, (void *)&a, &len);
    if (fork() == 0) {
        int c2 = socket(AF_INET, SOCK_STREAM, 0);
        connect(c2, (void *)&a, sizeof a);
        usleep(200000);
        write(c2, "hello", 5);
        usleep(200000);
        _exit(0);
    }
    int s = accept(l, NULL, NULL);
    int pid = getpid(), on = 1, owner = 0;
    ioctl(s, FIOSETOWN, &pid);
    ioctl(s, FIOGETOWN, &owner);
    printf("socket owner ok %d\n", owner == pid);
    fcntl(s, F_SETSIG, SIGUSR1);
    ioctl(s, FIOASYNC, &on);
    while (sigtimedwait(&set, NULL, &(struct timespec){0, 0}) > 0)
        ;
    wait_sig("tcp data", &set);
    char buf[16];
    printf("read %zd\n", read(s, buf, sizeof buf));
    wait(NULL);
    return 0;
}
C
gcc -O1 -o /tmp/async async.c && /tmp/async
cat > lease.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define FILE "/dev/shm/tuxport-lease"

static const char *name(int t) { return t == F_RDLCK ? "RDLCK" : t == F_WRLCK ? "WRLCK" : "UNLCK"; }

/* The child opens the file with flags (breaking the lease); its exit code is errno */
static pid_t opener(int flags)
{
    pid_t pid = fork();
    if (pid == 0)
        _exit(open(FILE, flags) >= 0 ? 0 : errno);
    return pid;
}

/* Wait for the child and print the result of its open */
static void reap(const char *what, pid_t pid)
{
    int status;
    waitpid(pid, &status, 0);
    printf("%s: %s\n", what, WEXITSTATUS(status) ? strerror(WEXITSTATUS(status)) : "opened");
}

static void wait_break(sigset_t *set, int fd)
{
    siginfo_t si;
    struct timespec t = {5, 0};
    int sig = sigtimedwait(set, &si, &t);
    printf("  signal %s code %d fd ok %d, lease now %s\n", sig == SIGUSR1 ? "SIGUSR1" : strerror(errno),
           si.si_code, si.si_fd == fd, name(fcntl(fd, F_GETLEASE)));
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    close(open(FILE, O_CREAT | O_RDWR | O_TRUNC, 0644));
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGIO);
    sigprocmask(SIG_BLOCK, &set, NULL);
    int w = open(FILE, O_RDWR);
    printf("read lease on a writable fd %d %s\n", fcntl(w, F_SETLEASE, F_RDLCK), strerror(errno));
    int r = open(FILE, O_RDONLY);
    printf("write lease with another open %d %s\n", fcntl(r, F_SETLEASE, F_WRLCK), strerror(errno));
    close(w);
    printf("unlock without a lease %d %s\n", fcntl(r, F_SETLEASE, F_UNLCK), strerror(errno));
    fcntl(r, F_SETSIG, SIGUSR1);
    printf("write lease %d, lease %s\n", fcntl(r, F_SETLEASE, F_WRLCK), name(fcntl(r, F_GETLEASE)));
    /* Nonblocking open: the holder is notified, and it returns EWOULDBLOCK at once */
    reap("nonblocking read open", opener(O_RDONLY | O_NONBLOCK));
    wait_break(&set, r);
    /* Open for reading: the open completes after downgrading to a read lease */
    pid_t pid = opener(O_RDONLY);
    usleep(200000);
    printf("  downgrade %d, lease %s\n", fcntl(r, F_SETLEASE, F_RDLCK), name(fcntl(r, F_GETLEASE)));
    reap("read open", pid);
    /* Open for writing: the lease must be given up; downgrading to a read lease is not enough */
    pid = opener(O_WRONLY);
    wait_break(&set, r);
    printf("  downgrade %d, unlock %d\n", fcntl(r, F_SETLEASE, F_RDLCK), fcntl(r, F_SETLEASE, F_UNLCK));
    reap("write open", pid);
    unlink(FILE);
    return 0;
}
C
gcc -O1 -o lease lease.c
./lease

# POSIX message queues: priorities, nonblocking and timeouts, the status line, poll, mq_notify
# (signals and musl's thread)
cat > mq.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile int notified;
static void on_notify(union sigval v) { notified = v.sival_int; }

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    mq_unlink("/tuxport-mq");
    struct mq_attr attr = {.mq_maxmsg = 4, .mq_msgsize = 64};
    mqd_t q = mq_open("/tuxport-mq", O_CREAT | O_RDWR | O_EXCL, 0600, &attr);
    printf("open %d excl again %d %s\n", q >= 0, mq_open("/tuxport-mq", O_CREAT | O_EXCL | O_RDWR, 0600, &attr),
           strerror(errno));
    printf("bad attr %d %s\n", mq_open("/tuxport-mq2", O_CREAT | O_RDWR, 0600, &(struct mq_attr){.mq_maxmsg = 0}),
           strerror(errno));
    mq_send(q, "low", 3, 1);
    mq_send(q, "high", 4, 9);
    mq_send(q, "low2", 4, 1);
    struct mq_attr got;
    mq_getattr(q, &got);
    printf("attr maxmsg %ld msgsize %ld curmsgs %ld\n", got.mq_maxmsg, got.mq_msgsize, got.mq_curmsgs);
    char status[128] = {0};
    pread(q, status, 16, 0);
    printf("status %.16s\n", status);
    struct pollfd p = {q, POLLIN | POLLOUT, 0};
    poll(&p, 1, 0);
    printf("poll %x\n", p.revents);
    printf("too long %d %s\n", mq_send(q, status, 65, 0), strerror(errno));
    printf("bad prio %d %s\n", mq_send(q, "x", 1, 40000), strerror(errno));
    char buf[64];
    unsigned prio;
    printf("small buffer %zd %s\n", mq_receive(q, buf, 10, &prio), strerror(errno));
    for (int i = 0; i < 3; i++) {
        ssize_t n = mq_receive(q, buf, sizeof buf, &prio);
        printf("got %.*s prio %u\n", (int)n, buf, prio);
    }
    struct timespec t;
    clock_gettime(CLOCK_REALTIME, &t);
    t.tv_nsec += 50000000;
    if (t.tv_nsec >= 1000000000) t.tv_sec++, t.tv_nsec -= 1000000000;
    printf("timed out %zd %s\n", mq_timedreceive(q, buf, sizeof buf, &prio, &t), strerror(errno));
    struct mq_attr nb = {.mq_flags = O_NONBLOCK};
    mq_setattr(q, &nb, NULL);
    printf("nonblocking %zd %s\n", mq_receive(q, buf, sizeof buf, &prio), strerror(errno));

    /* Signal notification: a message arrives on an empty queue */
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigprocmask(SIG_BLOCK, &set, NULL);
    struct sigevent sev = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGUSR1, .sigev_value.sival_int = 42};
    printf("notify %d\n", mq_notify(q, &sev));
    pid_t pid = fork();
    if (pid == 0) {
        printf("child notify %d %s\n", mq_notify(q, &sev), strerror(errno));
        mq_send(q, "wake", 4, 0);
        _exit(0);
    }
    siginfo_t si;
    sigwaitinfo(&set, &si);
    printf("signal code %d value %d from child %d\n", si.si_code, si.si_value.sival_int, si.si_pid == pid);
    waitpid(pid, NULL, 0);
    mq_receive(q, buf, sizeof buf, &prio);
    /* musl's SIGEV_THREAD: a thread is notified through a netlink socket */
    struct sigevent th = {.sigev_notify = SIGEV_THREAD, .sigev_notify_function = on_notify,
                          .sigev_value.sival_int = 7};
    printf("thread notify %d\n", mq_notify(q, &th));
    mq_send(q, "go", 2, 0);
    for (int i = 0; i < 200 && !notified; i++)
        usleep(10000);
    printf("thread notified %d\n", notified);
    mq_close(q);
    printf("unlink %d reopen %d %s\n", mq_unlink("/tuxport-mq"), mq_open("/tuxport-mq", O_RDWR), strerror(errno));
    return 0;
}
C
gcc -O1 -o mq mq.c
./mq

# Native AIO (issuing the syscalls directly, without libaio): reads and writes, fsync, poll,
# eventfd notification, cancellation
cat > aio.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/* The image has no linux-headers: definitions from linux/aio_abi.h */
typedef unsigned long aio_context_t;
enum { IOCB_CMD_PREAD = 0, IOCB_CMD_PWRITE = 1, IOCB_CMD_FSYNC = 2, IOCB_CMD_POLL = 5 };
#define IOCB_FLAG_RESFD 1
struct iocb {
    uint64_t aio_data;
    uint32_t aio_key, aio_rw_flags;
    uint16_t aio_lio_opcode;
    int16_t aio_reqprio;
    uint32_t aio_fildes;
    uint64_t aio_buf, aio_nbytes;
    int64_t aio_offset;
    uint64_t aio_reserved2;
    uint32_t aio_flags, aio_resfd;
};
struct io_event {
    uint64_t data, obj;
    int64_t res, res2;
};

static struct iocb cb(int fd, int op, void *buf, size_t n, long long off, uint64_t data)
{
    struct iocb c;
    memset(&c, 0, sizeof c);
    c.aio_fildes = fd;
    c.aio_lio_opcode = op;
    c.aio_buf = (uintptr_t)buf;
    c.aio_nbytes = n;
    c.aio_offset = off;
    c.aio_data = data;
    return c;
}

static void events(aio_context_t ctx, long min, const char *what)
{
    struct io_event ev[8];
    struct timespec t = {0, 200000000};
    long n = syscall(SYS_io_getevents, ctx, min, 8, ev, &t);
    printf("%s: %ld", what, n);
    for (long i = 0; i < n; i++)
        printf(" [data %llu res %lld]", (unsigned long long)ev[i].data, (long long)ev[i].res);
    printf("\n");
}

/* Wait in io_getevents indefinitely (no time limit) */
struct waiter {
    aio_context_t ctx;
    long min, n;
    int err;
};

static void *wait_events(void *arg)
{
    struct waiter *w = arg;
    struct io_event ev[8];
    w->n = syscall(SYS_io_getevents, w->ctx, w->min, 8, ev, NULL);
    w->err = errno;
    return NULL;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    aio_context_t ctx = 0;
    printf("setup %ld\n", syscall(SYS_io_setup, 8, &ctx));
    int fd = open("/tmp/aio-file", O_RDWR | O_CREAT | O_TRUNC, 0644);
    char out[] = "hello aio", in[16] = {0};
    struct iocb w = cb(fd, IOCB_CMD_PWRITE, out, 9, 100, 1);
    struct iocb s = cb(fd, IOCB_CMD_FSYNC, NULL, 0, 0, 2);
    struct iocb *list[] = {&w, &s};
    printf("submit %ld\n", syscall(SYS_io_submit, ctx, 2, list));
    events(ctx, 2, "write+fsync");
    struct iocb r = cb(fd, IOCB_CMD_PREAD, in, 9, 100, 3);
    int efd = eventfd(0, 0);
    r.aio_flags = IOCB_FLAG_RESFD;
    r.aio_resfd = efd;
    struct iocb *one[] = {&r};
    syscall(SYS_io_submit, ctx, 1, one);
    uint64_t count;
    read(efd, &count, 8);
    events(ctx, 1, "read");
    printf("read back %s eventfd %llu\n", in, (unsigned long long)count);
    int p[2];
    pipe(p);
    struct iocb pl = cb(p[0], IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 0, 0, 4);
    one[0] = &pl;
    syscall(SYS_io_submit, ctx, 1, one);
    events(ctx, 0, "poll before data");
    write(p[1], "x", 1);
    events(ctx, 1, "poll after data");
    struct iocb pl2 = cb(p[1], IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 0, 0, 5);
    one[0] = &pl2;
    syscall(SYS_io_submit, ctx, 1, one);
    struct io_event res;
    printf("cancel %ld %s\n", syscall(SYS_io_cancel, ctx, &pl2, &res), strerror(errno));
    events(ctx, 1, "after cancel");
    struct iocb bad = cb(fd, 99, in, 1, 0, 6);
    one[0] = &bad;
    printf("bad opcode %ld %s\n", syscall(SYS_io_submit, ctx, 1, one), strerror(errno));
    printf("min > nr %ld %s\n", syscall(SYS_io_getevents, ctx, 2, 1, &res, NULL), strerror(errno));
    printf("destroy %ld again %ld %s\n", syscall(SYS_io_destroy, ctx), syscall(SYS_io_destroy, ctx),
           strerror(errno));
    /* Destroying the context while another thread waits in io_getevents: it wakes and returns,
     * EINVAL if there is not a single event, and completed ones are still collected. mariadb
     * ends its event-waiting threads this way on shutdown */
    for (int done = 0; done <= 1; done++) {
        struct waiter wt = {0, done ? 2 : 1, 0, 0};
        syscall(SYS_io_setup, 8, &wt.ctx);
        struct iocb w2 = cb(fd, IOCB_CMD_PWRITE, out, 9, 0, 7);
        one[0] = &w2;
        if (done)
            syscall(SYS_io_submit, wt.ctx, 1, one);
        pthread_t t;
        pthread_create(&t, NULL, wait_events, &wt);
        usleep(100000);
        printf("destroy while waiting (%d done) %ld", done, syscall(SYS_io_destroy, wt.ctx));
        struct timespec limit;
        clock_gettime(CLOCK_REALTIME, &limit);
        limit.tv_sec += 5;
        if (pthread_timedjoin_np(t, NULL, &limit) != 0) {
            printf(", the waiter is still waiting\n");
            return 1;
        }
        printf(", the waiter got %ld %s\n", wt.n, wt.n < 0 ? strerror(wt.err) : "");
    }
    return 0;
}
C
gcc -O1 -o aio aio.c -pthread
./aio

# What the original address shows after MREMAP_DONTUNMAP on various mappings; remap_file_pages
cat > dun.c <<'C'
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <sys/mman.h>
#ifndef MREMAP_DONTUNMAP
#define MREMAP_DONTUNMAP 4
#endif
static void show(const char *tag, char *old, char *new) {
    printf("%s: old=%.4s new=%.4s\n", tag, old[0] ? old : "(0)", new);
}
int main(void) {
    long pg = 4096;
    int fd = open("dun.dat", O_RDWR | O_CREAT | O_TRUNC, 0644);
    char buf[8192];
    memset(buf, 'f', sizeof buf);
    write(fd, buf, sizeof buf);
    /* Shared file mapping */
    char *a = mmap(0, 2 * pg, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    memcpy(a, "AAAA", 4);
    char *b = mremap(a, 2 * pg, 2 * pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    if (b == MAP_FAILED) { printf("shared file: %s\n", strerror(errno)); return 1; }
    show("shared file", a, b);
    memcpy(b, "BBBB", 4);
    show("shared file after write new", a, b);
    memcpy(a + pg, "CCCC", 4);
    show("shared file second page", a + pg, b + pg);
    pread(fd, buf, 4, 0);
    printf("file now %.4s\n", buf);
    /* Private file mapping */
    char *p = mmap(0, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    memcpy(p, "PPPP", 4);
    char *q = mremap(p, pg, pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    if (q == MAP_FAILED) { printf("private file: %s\n", strerror(errno)); return 1; }
    show("private file", p, q);
    memcpy(p, "OOOO", 4);
    pread(fd, buf, 4, 0);
    printf("private write old -> file %.4s\n", buf);
    /* Shared anonymous mapping */
    char *s = mmap(0, pg, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    memcpy(s, "SSSS", 4);
    char *t = mremap(s, pg, pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    show("shared anon", s, t);
    memcpy(t, "TTTT", 4);
    show("shared anon after write new", s, t);
    /* Private anonymous mapping */
    char *x = mmap(0, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memcpy(x, "XXXX", 4);
    char *y = mremap(x, pg, pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    show("private anon", x, y);
    /* Read-only shared file mapping */
    char *r = mmap(0, pg, PROT_READ, MAP_SHARED, fd, pg);
    char *r2 = mremap(r, pg, pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    show("readonly shared", r, r2);
    /* remap_file_pages */
    char *w = mmap(0, 2 * pg, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    printf("remap %d", remap_file_pages(w, pg, 0, 1, 0));
    printf(" -> %.4s %.4s\n", w, w + pg);
    printf("remap prot %d %s\n", remap_file_pages(w, pg, PROT_READ, 1, 0), strerror(errno));
    printf("remap private %d %s\n", remap_file_pages(p, pg, 0, 1, 0), strerror(errno));
    return 0;
}
C
gcc -O1 -o dun dun.c
./dun

# CPU topology in sysfs (busybox's nproc --all counts the cpu<n> directories); possible and
# present exceed online on some hosts, so they are not compared
n=$(grep -c ^processor /proc/cpuinfo)
[ "$(nproc --all)" = "$n" ] && echo "nproc --all matches cpuinfo"
[ "$(ls -d /sys/devices/system/cpu/cpu[0-9]* | wc -l)" = "$n" ] && echo "cpu directories match cpuinfo"
for f in online cpu0/online cpu0/topology/thread_siblings_list; do
    echo "$f: $(cat /sys/devices/system/cpu/$f)"
done
test -r /sys/devices/system/cpu/possible && test -r /sys/devices/system/cpu/present && echo "possible present"
stat -c 'size %s blocks %b' /sys/devices/system/cpu/online
stat -f -c 'type %T' /sys
# The format of /proc/loadavg (the values depend on the host)
sed -E 's/[0-9]+\.[0-9][0-9]/L/g; s/[0-9]+\/[0-9]+ [0-9]+$/R\/T P/' /proc/loadavg

# /proc/<pid>/limits agrees with getrlimit, with the same column widths as Linux; futimens works
# on files still open after deletion
cat > limits.c <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <unistd.h>

static void show(char *out, rlim_t v)
{
    if (v == RLIM_INFINITY)
        strcpy(out, "unlimited");
    else
        sprintf(out, "%llu", (unsigned long long)v);
}

int main(void)
{
    struct rlimit rl = {777, 777};
    setrlimit(RLIMIT_NOFILE, &rl);
    FILE *f = fopen("/proc/self/limits", "r");
    char line[256];
    int n = 0, same = 0;
    fgets(line, sizeof line, f);
    printf("[%.*s]\n", (int)strcspn(line, "\n"), line);
    while (fgets(line, sizeof line, f)) {
        char soft[32], hard[32], want_soft[32], want_hard[32];
        struct rlimit r;
        getrlimit(n++, &r);
        show(want_soft, r.rlim_cur);
        show(want_hard, r.rlim_max);
        if (sscanf(line + 26, "%31s %31s", soft, hard) == 2 && !strcmp(soft, want_soft) &&
            !strcmp(hard, want_hard))
            same++;
        if (!strncmp(line, "Max open files", 14) || !strncmp(line, "Max nice", 8))
            printf("[%.*s]\n", (int)strcspn(line, "\n"), line);
    }
    printf("%d limits, %d same as getrlimit\n", n, same);

    int fd = open("futimens.tmp", O_CREAT | O_RDWR | O_TRUNC, 0644);
    unlink("futimens.tmp");
    usleep(100000);
    struct timespec ts[2] = {{1000000000, 0}, {1234567890, 5}};
    printf("futimens %d\n", futimens(fd, ts));
    struct stat st;
    fstat(fd, &st);
    printf("atime %ld mtime %ld.%09ld\n", (long)st.st_atime, (long)st.st_mtim.tv_sec, st.st_mtim.tv_nsec);
    ts[1].tv_sec = 1500000000;
    printf("empty path %d\n", utimensat(fd, "", ts, AT_EMPTY_PATH));
    fstat(fd, &st);
    printf("mtime %ld nlink %ld\n", (long)st.st_mtime, (long)st.st_nlink);
    return 0;
}
C
gcc -O1 -o limits limits.c
./limits

# mremap growing a private file mapping: what follows is the file's contents, private writes do
# not affect the file, and pages past the end of the file are SIGBUS
cat > mrp.c <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static sigjmp_buf env;
static void on_bus(int sig) { siglongjmp(env, sig); }

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    long ps = 4096;
    int fd = open("/tmp/mrp.dat", O_RDWR | O_CREAT | O_TRUNC, 0600);
    char page[4096];
    for (int i = 0; i < 3; i++) {
        memset(page, 'a' + i, sizeof page);
        write(fd, page, sizeof page);
    }
    /* Map only the first page, then grow to three and five pages: what follows is the file's
     * contents, and past the end of the file SIGBUS */
    int prots[] = {PROT_READ, PROT_READ | PROT_WRITE};
    for (int k = 0; k < 2; k++) {
        int prot = prots[k];
        char *p = mmap(NULL, ps, prot, MAP_PRIVATE, fd, 0);
        char *q = mremap(p, ps, 3 * ps, MREMAP_MAYMOVE);
        printf("prot %d: grew %s: %c %c %c\n", prot, q == MAP_FAILED ? "failed" : "ok",
               q == MAP_FAILED ? '-' : q[0], q == MAP_FAILED ? '-' : q[ps], q == MAP_FAILED ? '-' : q[2 * ps]);
        if (q == MAP_FAILED)
            continue;
        if (prot & PROT_WRITE) {
            q[2 * ps] = 'X';
            char c;
            pread(fd, &c, 1, 2 * ps);
            printf("private write: mapping %c, file %c\n", q[2 * ps], c);
        }
        char *r = mremap(q, 3 * ps, 5 * ps, MREMAP_MAYMOVE);
        signal(SIGBUS, on_bus);
        int sig = sigsetjmp(env, 1);
        if (!sig)
            printf("past the end: %c\n", r[3 * ps]);
        else
            printf("past the end: signal %d\n", sig);
        munmap(r, 5 * ps);
    }
    return 0;
}
C
gcc -O1 -o mrp mrp.c && ./mrp

# /sys/devices/system/cpu/cpu<n>/cache copies the host's (with CPU numbers turned into the
# guest's), and only the known files are looked at; when the host has no cpufreq it is absent too
cd /sys/devices/system/cpu
for c in cpu*/; do
    for d in "$c"cache/index*; do
        [ -d "$d" ] || continue
        for f in level type size coherency_line_size ways_of_associativity number_of_sets \
            shared_cpu_list shared_cpu_map id; do
            [ -e "$d/$f" ] && echo "$d/$f: $(cat "$d/$f")"
        done
    done
done
ls cpu0 | grep -c '^cache$' || true
[ -d cpu0/cpufreq ] && echo cpufreq || echo no cpufreq
cd /tmp

# A signalfd belongs to no thread: poll, epoll and read look at the signals pending for the
# calling thread and its process. Signals sent to another thread are not seen, a thread blocked
# in poll wakes when it receives a signal sent to it, a forked child looks at its own, and it
# stays usable after the thread that created it exits
cat > sfd.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/signalfd.h>
#include <sys/wait.h>
#include <unistd.h>

static int sfd, cmd[2], res[2], ep_b;
static sigset_t set;

static int pollnow(int fd)
{
    struct pollfd p = {fd, POLLIN, 0};
    return poll(&p, 1, 0) == 1 && p.revents & POLLIN;
}

/* The signal read; 0 when there is none (EAGAIN) */
static int readsig(int fd)
{
    struct signalfd_siginfo si;
    ssize_t n = read(fd, &si, sizeof si);
    return n == sizeof si ? (int)si.ssi_signo : errno == EAGAIN ? 0 : -errno;
}

/* Thread B carries out commands: p poll, e epoll (B's own epoll), r read, w blocking poll for at
 * most 2 seconds */
static void *thread_b(void *arg)
{
    (void)arg;
    ep_b = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN};
    epoll_ctl(ep_b, EPOLL_CTL_ADD, sfd, &ev);
    char c;
    while (read(cmd[0], &c, 1) == 1 && c != 'q') {
        struct pollfd p = {sfd, POLLIN, 0};
        int r = 0;
        if (c == 'p')
            r = pollnow(sfd);
        else if (c == 'e')
            r = epoll_wait(ep_b, &ev, 1, 0);
        else if (c == 'r')
            r = readsig(sfd);
        else if (c == 'w')
            r = poll(&p, 1, 2000);
        write(res[1], &r, sizeof r);
    }
    return NULL;
}

static int in_b(char c)
{
    int r;
    write(cmd[1], &c, 1);
    read(res[0], &r, sizeof r);
    return r;
}

static void *create(void *arg)
{
    (void)arg;
    return (void *)(long)signalfd(-1, &set, SFD_NONBLOCK);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGUSR2);
    sigprocmask(SIG_BLOCK, &set, NULL);
    sfd = signalfd(-1, &set, SFD_NONBLOCK);
    pipe(cmd);
    pipe(res);
    pthread_t b;
    pthread_create(&b, NULL, thread_b, NULL);
    int r[5];

    pthread_kill(pthread_self(), SIGUSR1);
    r[0] = pollnow(sfd), r[1] = in_b('p'), r[2] = in_b('e'), r[3] = in_b('r'), r[4] = readsig(sfd);
    printf("to A: A poll %d, B poll %d, B epoll %d, B read %d, A read %d\n", r[0], r[1], r[2], r[3], r[4]);

    pthread_kill(b, SIGUSR2);
    r[0] = pollnow(sfd), r[1] = in_b('p'), r[2] = in_b('e'), r[3] = readsig(sfd), r[4] = in_b('r');
    printf("to B: A poll %d, B poll %d, B epoll %d, A read %d, B read %d\n", r[0], r[1], r[2], r[3], r[4]);

    kill(getpid(), SIGUSR1);
    r[0] = pollnow(sfd), r[1] = in_b('p'), r[2] = in_b('r'), r[3] = pollnow(sfd), r[4] = in_b('p');
    printf("to the process: A poll %d, B poll %d, B read %d, then A poll %d, B poll %d\n", r[0], r[1],
           r[2], r[3], r[4]);

    char c = 'w';
    write(cmd[1], &c, 1);
    usleep(100000);
    pthread_kill(b, SIGUSR2);
    read(res[0], &r[0], sizeof r[0]);
    r[1] = in_b('r');
    printf("B blocked in poll, signal to B: poll %d, read %d\n", r[0], r[1]);

    pid_t child = fork();
    if (child == 0) {
        struct pollfd p = {sfd, POLLIN, 0};
        int polled = poll(&p, 1, 2000);
        printf("fork child: poll %d, read %d\n", polled, readsig(sfd));
        _exit(0);
    }
    usleep(100000);
    kill(child, SIGUSR2);
    waitpid(child, NULL, 0);
    printf("parent after the child's signal: poll %d\n", pollnow(sfd));

    pthread_t creator;
    void *fd2;
    pthread_create(&creator, NULL, create, NULL);
    pthread_join(creator, &fd2);
    pthread_kill(pthread_self(), SIGUSR1);
    r[0] = pollnow((int)(long)fd2), r[1] = readsig((int)(long)fd2);
    printf("creator thread gone: poll %d, read %d\n", r[0], r[1]);

    c = 'q';
    write(cmd[1], &c, 1);
    pthread_join(b, NULL);
    return 0;
}
C
gcc -O1 -o sfd sfd.c -lpthread && ./sfd

# Owner, group and permissions of /dev/pts/<n> (devpts mounted with gid=5, mode=620): sshd chowns
# the terminal to the logged-in user and the tty group; ordinary users can change only their own
# terminals
cat > ptyown.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static void show(const char *what, const char *path, int fd)
{
    struct stat a, b;
    stat(path, &a);
    fstat(fd, &b);
    printf("%s: path %u:%u %o, fd %u:%u %o\n", what, a.st_uid, a.st_gid, a.st_mode & 07777, b.st_uid,
           b.st_gid, b.st_mode & 07777);
}

static int open_pty(char *name, size_t size, int *slave)
{
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    grantpt(m);
    unlockpt(m);
    ptsname_r(m, name, size);
    *slave = open(name, O_RDWR | O_NOCTTY);
    return m;
}

int main(void)
{
    char name[64];
    int s;
    open_pty(name, sizeof name, &s);
    show("new", name, s);
    printf("chown to 1000:5 %d, chmod 0600 %d\n", chown(name, 1000, 5), chmod(name, 0600));
    show("changed", name, s);
    printf("fchmod 0620 %d, fchown 0:5 %d\n", fchmod(s, 0620), fchown(s, 0, 5));
    show("by fd", name, s);
    chown(name, 1000, 5);
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        if (setgid(1000) || setuid(1000))
            _exit(1);
        int own = chmod(name, 0600);
        int give = chown(name, 0, -1);
        int e = errno;
        printf("uid 1000: chmod its own %d, chown to root %d errno %d\n", own, give, e);
        char n2[64];
        int s2;
        open_pty(n2, sizeof n2, &s2);
        show("opened by uid 1000", n2, s2);
        fflush(stdout);
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    show("after uid 1000", name, s);
    return 0;
}
C
gcc -O1 -o ptyown ptyown.c && ./ptyown

# /proc/meminfo and sysinfo: copied from the host (both sides are the same host), with the
# available amount counting the page cache
awk '/^MemTotal:/ { t = $2 } /^MemFree:/ { f = $2 } /^MemAvailable:/ { a = $2 } /^Cached:/ { c = $2 }
    END { print "meminfo: total", t, "available >= free", (a >= f), "cached", (c > 0) }' /proc/meminfo
cat > si.c <<'C'
#include <stdio.h>
#include <string.h>
#include <sys/sysinfo.h>

static unsigned long long field(const char *name)
{
    char line[128];
    unsigned long long kb = 0;
    FILE *f = fopen("/proc/meminfo", "r");
    while (fgets(line, sizeof line, f))
        if (!strncmp(line, name, strlen(name)) && line[strlen(name)] == ':')
            sscanf(line + strlen(name) + 1, "%llu", &kb);
    fclose(f);
    return kb * 1024;
}

int main(void)
{
    struct sysinfo si;
    sysinfo(&si);
    unsigned long long u = si.mem_unit;
    printf("sysinfo: totalram %d, totalswap %d, bufferram %d, sharedram %d\n", si.totalram * u == field("MemTotal"),
           si.totalswap * u == field("SwapTotal"), si.bufferram > 0, si.sharedram > 0);
    return 0;
}
C
gcc -O1 -o si si.c && ./si

# TCFLSH and TCXONC on PTYs: on the master TCOFLUSH does not discard data the slave wrote, and
# TCIFLUSH does; TCOOFF blocks writes on this end until TCOON (^Q cannot resume it), and what was
# written before stopping can still be read; TCIOFF sends the STOP character
python3 - <<'PY'
import os, select, termios, threading
m, s = os.openpty()
attrs = termios.tcgetattr(s)
attrs[3] &= ~termios.ECHO
termios.tcsetattr(s, termios.TCSANOW, attrs)
def readable(fd):
    return bool(select.select([fd], [], [], 0.2)[0])
def writable(fd):
    return bool(select.select([], [fd], [], 0)[1])
def blocked_write(fd, data):
    done = threading.Event()
    threading.Thread(target=lambda: (os.write(fd, data), done.set())).start()
    return done
os.write(s, b"abcdef")
print("pty read", os.read(m, 2))
termios.tcflush(m, termios.TCOFLUSH)
print("pty after master TCOFLUSH", os.read(m, 2))
termios.tcflush(m, termios.TCIFLUSH)
print("pty after master TCIFLUSH readable", readable(m))
os.write(s, b"ABC")
print("pty read", os.read(m, 1024))
os.write(m, b"xy\n")
termios.tcflush(s, termios.TCIFLUSH)
print("pty after slave TCIFLUSH readable", readable(s))
termios.tcflow(s, termios.TCOOFF)
print("pty slave writable while stopped", writable(s))
done = blocked_write(s, b"def")
print("pty slave write blocked", not done.wait(0.3))
os.write(m, b"\x11")
print("pty still blocked after ^Q", not done.wait(0.3))
termios.tcflow(s, termios.TCOON)
print("pty resumed", done.wait(2), os.read(m, 1024))
os.write(s, b"before")
os.write(m, b"\x13")
print("pty written before ^S", os.read(m, 16), "slave writable", writable(s))
os.write(m, b"\x11")
termios.tcflow(s, termios.TCIOFF)
termios.tcflow(s, termios.TCION)
# The two tcflow calls each write one character, which Linux hands to the master asynchronously
# through the flip buffer, so one read may get only the first
xchars = os.read(m, 16)
while len(xchars) < 2 and readable(m):
    xchars += os.read(m, 16)
print("pty xchars", xchars)
termios.tcflow(m, termios.TCOOFF)
done = blocked_write(m, b"z\n")
print("pty master write blocked", not done.wait(0.3))
termios.tcflow(m, termios.TCOON)
print("pty master resumed", done.wait(2), os.read(s, 16))
PY

# The read end of a FIFO opened with O_NONBLOCK while there is no writer: poll does not report
# POLLHUP before a writer has opened it
python3 - <<'PY'
import os, select, tempfile
fifo = os.path.join(tempfile.mkdtemp(), "fifo")
os.mkfifo(fifo)
def events(fd):
    p = select.poll()
    p.register(fd, select.POLLIN)
    return [e for _, e in p.poll(0)]
r = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)
print("fifo before writer", events(r), os.read(r, 1))
w = os.open(fifo, os.O_WRONLY)
print("fifo with writer", events(r))
os.write(w, b"x")
print("fifo data", events(r))
os.close(w)
print("fifo writer gone", events(r), os.read(r, 2), events(r))
r2 = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)
print("fifo new reader", events(r2))
PY

# Nothing can be created in a deleted current directory (ENOENT); fds of sockets and pipes can be
# fchmod'ed (libuv's uv_pipe_chmod)
python3 - <<'PY'
import errno, os, socket
def err(f):
    try:
        f()
        return "ok"
    except OSError as e:
        return errno.errorcode[e.errno]
os.mkdir("/tmp/deadcwd")
os.chdir("/tmp/deadcwd")
os.rmdir("/tmp/deadcwd")
print("dead cwd mkdir", err(lambda: os.mkdir("x")))
print("dead cwd create", err(lambda: open("x", "w")))
print("dead cwd symlink", err(lambda: os.symlink("a", "x")))
os.chdir("/tmp")
s = socket.socket(socket.AF_UNIX)
print("fchmod socket", err(lambda: os.fchmod(s.fileno(), 0o777)))
print("fchmod pipe", err(lambda: os.fchmod(os.pipe()[0], 0o600)))
PY

# Terminal times: utimensat can change them, reading data changes the access time and writing
# data the modification time (only when they differ by more than 8 seconds, so here they go from
# long ago to now), chmod and utimensat change the status change time
python3 - <<'PY'
import os, time
m, s = os.openpty()
path = os.ttyname(s)
os.utime(path, (1000, 2000))
st = os.stat(path)
print("pty utime", st.st_atime, st.st_mtime, st.st_ctime > 2000)
ctime = os.stat(path).st_ctime_ns
time.sleep(0.05)
os.chmod(path, 0o600)
print("pty chmod ctime", os.stat(path).st_ctime_ns > ctime)
os.write(s, b"out\n")
os.write(m, b"in\n")
os.read(s, 16)
st = os.stat(path)
now = time.time()
print("pty after io", now - 10 < st.st_atime <= now + 1, now - 10 < st.st_mtime <= now + 1)
print("pty fstat", os.fstat(s).st_mtime == st.st_mtime)
PY

# Closing a PTY's master hangs up the slave (Linux's pty_close → tty_vhangup): the session leader
# receives SIGHUP and SIGCONT, other processes in the session receive neither but all lose the
# controlling terminal, and the foreground process group receives SIGHUP only when the leader
# exits (tty_old_pgrp); an open slave reads EOF (unread input is gone too), writes and ioctls
# are EIO, tcsetpgrp is ENOTTY, poll is always ready; /dev/pts/<n> is removed
python3 - <<'PY'
import errno, os, select, signal, time


def err(f):
    try:
        return repr(f())
    except OSError as e:
        return errno.errorcode[e.errno]


r, w = os.pipe()
ready_r, ready_w = os.pipe()
pg_r, pg_w = os.pipe()
m, s = os.openpty()
name = os.ttyname(s)
os.close(s)
if os.fork() == 0:
    os.close(m)
    got = []
    signal.signal(signal.SIGHUP, lambda n, f: got.append("HUP"))
    signal.signal(signal.SIGCONT, lambda n, f: got.append("CONT"))
    os.setsid()
    fd = os.open(name, os.O_RDWR)
    # Another process in the leader's process group (the foreground process group): it receives
    # nothing at the hangup, and SIGHUP only when the leader exits
    leader = os.getpid()
    if os.fork() == 0:
        hups = []
        signal.signal(signal.SIGHUP, lambda n, f: hups.append(os.getppid() != leader))
        for _ in range(1000):
            if hups:
                break
            time.sleep(0.01)
        os.write(pg_w, f"foreground member: SIGHUP after the leader exited {hups}".encode())
        os._exit(0)
    kid_r, kid_w = os.pipe()
    kid = os.fork()
    if kid == 0:
        kid_got = []
        signal.signal(signal.SIGHUP, lambda n, f: kid_got.append("HUP"))
        os.setpgid(0, 0)
        os.write(kid_w, b"x")
        time.sleep(1)
        tty = err(lambda: os.open("/dev/tty", os.O_RDWR) and "opened")
        os.write(kid_w, f"member: signals {kid_got}, /dev/tty {tty}".encode())
        os._exit(0)
    os.read(kid_r, 1)
    os.write(ready_w, b"x")
    for _ in range(500):
        if got:
            break
        time.sleep(0.01)
    time.sleep(0.2)
    out = [f"leader: signals {got}"]
    p = select.poll()
    p.register(fd, select.POLLIN | select.POLLOUT)
    out.append(f"poll {[hex(ev) for _, ev in p.poll(100)]}")
    out.append(f"read {err(lambda: os.read(fd, 16))}")
    out.append(f"write {err(lambda: os.write(fd, b'x'))}")
    out.append(f"tcgetpgrp {err(lambda: os.tcgetpgrp(fd) == os.getpgrp())}")
    out.append(f"tcsetpgrp {err(lambda: os.tcsetpgrp(fd, os.getpgrp()))}")
    out.append(f"isatty {os.isatty(fd)}")
    out.append(f"/dev/tty {err(lambda: os.open('/dev/tty', os.O_RDWR) and 'opened')}")
    # The node is gone too (Linux's devpts_pty_kill), even while the slave is still open
    listed = os.path.basename(name) in os.listdir("/dev/pts")
    out.append(f"{name.startswith('/dev/pts/')} listed {listed}, "
               f"open {err(lambda: os.open(name, os.O_RDWR | os.O_NOCTTY) and 'opened')}")
    os.waitpid(kid, 0)
    out.append(os.read(kid_r, 200).decode())
    os.write(w, "\n".join(out).encode())
    os._exit(0)
os.close(w)
os.close(pg_w)
os.read(ready_r, 1)
os.write(m, b"unread\n")
os.close(m)
os.wait()
print(os.read(r, 4096).decode())
print(os.read(pg_r, 200).decode())
PY

# A process woken by a signal that has not run yet is R in /proc (Linux's try_to_wake_up changes
# the state on the spot): LTP's TST_PROCESS_STATE_WAIT sends a signal to a child stopped in
# pause(), waits for its state to return to S, and takes that as the child having done its next
# step (shmctl01)
cat > woken.c <<'C'
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>
#define N 8
static void on_usr1(int sig) { (void)sig; }
static char state(pid_t pid)
{
    char path[64], buf[512];
    snprintf(path, sizeof path, "/proc/%d/stat", pid);
    FILE *f = fopen(path, "r");
    size_t n = f ? fread(buf, 1, sizeof buf - 1, f) : 0;
    if (f)
        fclose(f);
    buf[n] = 0;
    char *p = strrchr(buf, ')');
    return p && p[1] ? p[2] : 0;
}
static int wait_sleeping(pid_t pid)
{
    for (int i = 0; i < 10000; i++) {
        if (state(pid) == 'S')
            return 1;
        usleep(1000);
    }
    return 0;
}
int main(void)
{
    volatile int *done = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, NULL);
    pid_t pids[N];
    for (int i = 0; i < N; i++) {
        if ((pids[i] = fork()) == 0)
            for (;;) {
                pause();
                done[i]++;
            }
    }
    int stuck = 0, early = 0;
    for (int round = 1; round <= 20; round++) {
        for (int i = 0; i < N; i++)
            stuck += !wait_sleeping(pids[i]);
        for (int i = 0; i < N; i++)
            kill(pids[i], SIGUSR1);
        for (int i = 0; i < N; i++)
            stuck += !wait_sleeping(pids[i]);
        for (int i = 0; i < N; i++)
            early += done[i] != round;
    }
    printf("woken children: never sleeping %d, sleeping again before running %d\n", stuck, early);
    for (int i = 0; i < N; i++) {
        kill(pids[i], SIGKILL);
        waitpid(pids[i], NULL, 0);
    }
    return 0;
}
C
gcc -O1 -o woken woken.c
./woken

# Paths in /proc/self/fd and maps have symlinks fully resolved, as with d_path: a link as the
# last component, directories through links, .. after a link (going back by the actual
# directory), O_PATH|O_NOFOLLOW stopping at the link itself, files created through dangling
# links, relative paths under a current directory entered through a link; a file mmapped through
# a link, and the interpreter's mapping in a program whose PT_INTERP points at a link (valgrind
# reopens files by these names with O_NOFOLLOW)
rm -rf /tmp/rp; mkdir -p /tmp/rp/real/sub; ln -s real /tmp/rp/link; echo x > /tmp/rp/real/f
ln -s f /tmp/rp/real/flink; ln -s new /tmp/rp/dangling; ln -s /lib/ld-musl-aarch64.so.1 /tmp/rp/ld
cat > rp.c <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static void show(const char *what, int fd)
{
    char p[64], link[256] = {0};
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    if (fd < 0 || readlink(p, link, sizeof link - 1) < 0)
        strcpy(link, "error");
    printf("%s %s\n", what, link);
}

/* The path in the first line of maps that contains `key` */
static void mapped(const char *what, const char *key)
{
    char line[512];
    FILE *f = fopen("/proc/self/maps", "r");
    while (fgets(line, sizeof line, f))
        if (strstr(line, key)) {
            printf("%s %s", what, strchr(line, '/'));
            break;
        }
    fclose(f);
}

int main(void)
{
    show("last component", open("/tmp/rp/link/flink", O_RDONLY));
    show("dot dot", open("/tmp/rp/link/sub/../f", O_RDONLY));
    show("nofollow path", open("/tmp/rp/link/flink", O_PATH | O_NOFOLLOW));
    show("created", open("/tmp/rp/dangling", O_CREAT | O_WRONLY, 0644));
    show("directory", open("/tmp/rp/link/", O_RDONLY | O_DIRECTORY));
    chdir("/tmp/rp/link/sub");
    show("relative", open("../flink", O_RDONLY));
    show("relative dir", open("..", O_RDONLY | O_DIRECTORY));
    int fd = open("/tmp/rp/link/flink", O_RDONLY);
    mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
    mapped("mmap", "/tmp/rp/");
    mapped("interpreter", "ld-musl");
    return 0;
}
C
gcc -O1 -Wl,--dynamic-linker=/tmp/rp/ld -o rpaths rp.c
./rpaths

# The link count of /proc/<pid>/task is 2 plus the number of threads: Chromium's zygote uses it to
# tell whether it is single-threaded, and checks that it is at least 3 before fork
python3 - <<'PY'
import os, threading
print("task links, 1 thread", os.stat("/proc/self/task").st_nlink)
ev = threading.Event()
ts = [threading.Thread(target=ev.wait) for _ in range(3)]
for t in ts:
    t.start()
print("task links, 4 threads", os.stat("/proc/self/task").st_nlink)
print("thread's own task dir", os.stat("/proc/thread-self/..").st_nlink)
ev.set()
for t in ts:
    t.join()
PY

# /proc/net is a link to self/net: the files are under each process's (and thread's) directory,
# and files opened through /proc/net show as /proc/<pid>/net/... in /proc/self/fd; the directory
# belongs to the process's user, the files to root
ls -ld /proc/net | cut -c1; readlink /proc/net
for f in dev if_inet6 tcp tcp6 udp udp6 unix raw icmp; do test -r /proc/self/net/$f && echo "net/$f"; done
ls /proc/self/task/*/net/ | grep -cx unix
exec 7</proc/net/dev; readlink /proc/self/fd/7 | sed 's/[0-9][0-9]*/N/'; exec 7<&-
stat -c '%A %U' /proc/self/net /proc/self/net/dev
head -n 2 /proc/net/dev | cmp - /dev/fd/3 3<<HEAD && echo "same header"
$(head -n 2 /proc/self/net/dev)
HEAD

# For files deleted but still open or mapped, the paths in fd links and maps end with
# " (deleted)": deleted, or overwritten by rename (how dpkg upgrades libraries; needrestart uses
# it to find processes to restart); a memfd has only one
cat > deleted.c <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static void link_of(const char *what, int fd)
{
    char p[64], l[256] = {0};
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    readlink(p, l, sizeof l - 1);
    printf("%s: %s\n", what, l);
}

static void mapped(const char *what, const char *key)
{
    char line[512];
    FILE *f = fopen("/proc/self/maps", "r");
    while (fgets(line, sizeof line, f))
        if (strstr(line, key)) {
            printf("%s: %s", what, strchr(line, '/'));
            break;
        }
    fclose(f);
}

int main(void)
{
    int fd = open("/tmp/del-open", O_CREAT | O_RDWR | O_TRUNC, 0644);
    link_of("open", fd);
    unlink("/tmp/del-open");
    link_of("open, unlinked", fd);
    int m = open("/tmp/del-map", O_CREAT | O_RDWR | O_TRUNC, 0644);
    ftruncate(m, 4096);
    mmap(NULL, 4096, PROT_READ, MAP_SHARED, m, 0);
    close(m);
    mapped("mapped", "/tmp/del-map");
    unlink("/tmp/del-map");
    mapped("mapped, unlinked", "/tmp/del-map");
    int r = open("/tmp/del-lib", O_CREAT | O_RDWR | O_TRUNC, 0644);
    ftruncate(r, 4096);
    mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, r, 0);
    close(r);
    int n = open("/tmp/del-lib.new", O_CREAT | O_RDWR | O_TRUNC, 0644);
    close(n);
    rename("/tmp/del-lib.new", "/tmp/del-lib");
    mapped("mapped, replaced by rename", "/tmp/del-lib");
    int mf = memfd_create("x", 0);
    link_of("memfd", mf);
    unlink("/tmp/del-lib");
    return 0;
}
C
gcc -O1 -o deleted deleted.c && ./deleted
