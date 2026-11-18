# The sequel to compat.sh (difftest passes the whole script as one argument of sh -c, which must
# not exceed 128 KB): raw ICMP sockets, PR_SET_CHILD_SUBREAPER, positional writes on O_APPEND
# files, readlink with an empty path, O_LARGEFILE in F_GETFL, /proc/<pid>/task/<tid>/children,
# PR_SET_PDEATHSIG and __WNOTHREAD, short symlinks, /proc/<pid>/exe pointing at the program file
# itself, DIT across signals, fork, execve and ptrace, a fixed btime in /proc/stat, POSIX locks
# across execve and close_range, O_SYNC and O_DSYNC kept at open, chown(-1, -1) of set-id files,
# /proc/<tid> of a thread that is not the main one, FIONREAD on regular files
set -e
cd /tmp
# raw ICMP sockets (Tuxport emulates them with ping sockets on the host): data received on IPv4
# includes the IP header, an echo reply carries the id we sent, and the checksum is right;
# ICMP_FILTER, ICMPV6_FILTER, IPV6_CHECKSUM, the control messages of IP_RECVTTL and IP_PKTINFO
# (truncated when they do not fit), MSG_TRUNC. On loopback Linux's raw socket also receives the
# echo requests sent, so the test cases all first use a filter to keep only the replies. The id,
# TOS and flags of the IP header are not compared
cat > rawicmp.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/icmp6.h>
#include <netinet/in.h>
#include <netinet/ip.h>
#include <netinet/ip_icmp.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#define ICMP_FILTER 1

static unsigned fold(unsigned long acc)
{
    while (acc >> 16)
        acc = (acc & 0xffff) + (acc >> 16);
    return acc;
}

static unsigned long add(unsigned long acc, const void *p, int n)
{
    const unsigned char *b = p;
    for (int i = 0; i + 1 < n; i += 2)
        acc += b[i] << 8 | b[i + 1];
    if (n & 1)
        acc += b[n - 1] << 8;
    return acc;
}

static unsigned short cksum(const void *p, int n)
{
    return htons(~fold(add(0, p, n)) & 0xffff);
}

static void echo(int s, int seq, int len)
{
    struct sockaddr_in to = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    unsigned char pkt[256] = {0};
    struct icmphdr *h = (void *)pkt;
    h->type = ICMP_ECHO;
    h->un.echo.id = htons(0x1234);
    h->un.echo.sequence = htons(seq);
    for (int i = 8; i < len; i++)
        pkt[i] = i;
    h->checksum = cksum(pkt, len);
    if (sendto(s, pkt, len, 0, (void *)&to, sizeof to) != len)
        printf("sendto: %s\n", strerror(errno));
}

static void show(const unsigned char *buf, int n)
{
    const struct iphdr *ip = (const void *)buf;
    struct in_addr a;
    printf("  %d bytes: version %d ihl %d tot_len %d ttl %d protocol %d ip checksum %s", n,
           ip->version, ip->ihl, ntohs(ip->tot_len), ip->ttl, ip->protocol,
           fold(add(0, ip, 20)) == 0xffff ? "ok" : "bad");
    a.s_addr = ip->saddr;
    printf(" src %s", inet_ntoa(a));
    a.s_addr = ip->daddr;
    printf(" dst %s\n", inet_ntoa(a));
    const struct icmphdr *h = (const void *)(buf + ip->ihl * 4);
    int len = n - ip->ihl * 4, same = 1;
    for (int i = 8; i < len; i++)
        same &= buf[ip->ihl * 4 + i] == (unsigned char)i;
    printf("  icmp type %d code %d id %#x seq %d checksum %s payload %s\n", h->type, h->code,
           ntohs(h->un.echo.id), ntohs(h->un.echo.sequence),
           fold(add(0, h, len)) == 0xffff ? "ok" : "bad", same ? "same" : "differs");
}

static void control(struct msghdr *m)
{
    printf("  msg_flags%s controllen %zu\n", m->msg_flags & MSG_CTRUNC ? " MSG_CTRUNC" : "",
           (size_t)m->msg_controllen);
    for (struct cmsghdr *c = CMSG_FIRSTHDR(m); c; c = CMSG_NXTHDR(m, c)) {
        printf("  cmsg level %d type %d len %zu", c->cmsg_level, c->cmsg_type, (size_t)c->cmsg_len);
        if (c->cmsg_type == IP_TTL && c->cmsg_len == CMSG_LEN(sizeof(int)))
            printf(" ttl %d", *(int *)CMSG_DATA(c));
        if (c->cmsg_type == IP_PKTINFO && c->cmsg_len == CMSG_LEN(sizeof(struct in_pktinfo))) {
            struct in_pktinfo *p = (void *)CMSG_DATA(c);
            printf(" ifindex %d spec_dst %s", p->ipi_ifindex, inet_ntoa(p->ipi_spec_dst));
            printf(" addr %s", inet_ntoa(p->ipi_addr));
        }
        printf("\n");
    }
}

static void v4(void)
{
    int s = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP), v;
    unsigned filt;
    socklen_t l = sizeof v;
    getsockopt(s, SOL_SOCKET, SO_TYPE, &v, &l);
    printf("SO_TYPE %d\n", v);
    l = sizeof filt;
    getsockopt(s, SOL_RAW, ICMP_FILTER, &filt, &l);
    printf("ICMP_FILTER %#x len %d\n", filt, (int)l);
    filt = ~(1u << ICMP_ECHOREPLY);
    setsockopt(s, SOL_RAW, ICMP_FILTER, &filt, sizeof filt);
    filt = 0;
    getsockopt(s, SOL_RAW, ICMP_FILTER, &filt, &l);
    printf("ICMP_FILTER %#x\n", filt);
    l = sizeof v;
    getsockopt(s, IPPROTO_IP, IP_RECVTTL, &v, &l);
    printf("IP_RECVTTL %d\n", v);
    printf("SOL_RAW 99: %s\n", setsockopt(s, SOL_RAW, 99, &v, sizeof v) ? strerror(errno) : "ok");

    unsigned char buf[512];
    struct sockaddr_in from;
    l = sizeof from;
    echo(s, 1, 64);
    int n = recvfrom(s, buf, sizeof buf, 0, (void *)&from, &l);
    printf("recvfrom %d from %s\n", n, inet_ntoa(from.sin_addr));
    show(buf, n);

    /* Control messages: PKTINFO comes before TTL; both fit, only the first fits, only part of the
     * second fits */
    v = 1;
    setsockopt(s, IPPROTO_IP, IP_RECVTTL, &v, sizeof v);
    setsockopt(s, IPPROTO_IP, IP_PKTINFO, &v, sizeof v);
    size_t rooms[] = {256, CMSG_SPACE(sizeof(struct in_pktinfo)),
                      CMSG_SPACE(sizeof(struct in_pktinfo)) + 20, 8};
    for (int i = 0; i < 4; i++) {
        union {
            struct cmsghdr h;
            char b[256];
        } cbuf;
        memset(&cbuf, 0, sizeof cbuf);
        struct iovec iov = {buf, sizeof buf};
        struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = &cbuf,
                           .msg_controllen = rooms[i]};
        echo(s, 2 + i, 64);
        n = recvmsg(s, &m, 0);
        printf("recvmsg %d with %zu bytes of control\n", n, rooms[i]);
        control(&m);
    }
    v = 0;
    setsockopt(s, IPPROTO_IP, IP_RECVTTL, &v, sizeof v);
    setsockopt(s, IPPROTO_IP, IP_PKTINFO, &v, sizeof v);

    /* Truncation: MSG_TRUNC asks for the full length, and what is received is the start of the IP
     * header */
    echo(s, 6, 200);
    struct iovec iov = {buf, 10};
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1};
    n = recvmsg(s, &m, MSG_TRUNC);
    printf("truncated: %d, MSG_TRUNC %d, first byte %#x\n", n, !!(m.msg_flags & MSG_TRUNC), buf[0]);

    /* Replies the filter blocks are not received, also when peeking */
    filt = ~0u;
    setsockopt(s, SOL_RAW, ICMP_FILTER, &filt, sizeof filt);
    for (int peek = 0; peek < 2; peek++) {
        echo(s, 7 + peek, 64);
        usleep(200000);
        n = recv(s, buf, sizeof buf, MSG_DONTWAIT | (peek ? MSG_PEEK : 0));
        printf("all blocked%s: %d %s\n", peek ? " (peek)" : "", n, n < 0 ? strerror(errno) : "");
    }
    filt = ~(1u << ICMP_ECHOREPLY);
    setsockopt(s, SOL_RAW, ICMP_FILTER, &filt, sizeof filt);
    n = recv(s, buf, sizeof buf, MSG_DONTWAIT);
    printf("after unblocking: %d %s\n", n, n < 0 ? strerror(errno) : "");
    echo(s, 9, 64);
    n = recv(s, buf, sizeof buf, 0);
    printf("next reply:\n");
    show(buf, n);
    close(s);
}

static void v6(void)
{
    int s = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6), v;
    socklen_t l = sizeof v;
    getsockopt(s, SOL_RAW, IPV6_CHECKSUM, &v, &l);
    printf("IPV6_CHECKSUM %d\n", v);
    v = 2;
    printf("IPV6_CHECKSUM at IPPROTO_IPV6: %s\n",
           setsockopt(s, IPPROTO_IPV6, IPV6_CHECKSUM, &v, sizeof v) ? strerror(errno) : "ok");
    v = 3;
    printf("IPV6_CHECKSUM 3: %s\n",
           setsockopt(s, SOL_RAW, IPV6_CHECKSUM, &v, sizeof v) ? strerror(errno) : "ok");
    v = -5;
    setsockopt(s, SOL_RAW, IPV6_CHECKSUM, &v, sizeof v);
    getsockopt(s, IPPROTO_IPV6, IPV6_CHECKSUM, &v, &l);
    printf("IPV6_CHECKSUM after -5: %d\n", v);
    v = 2;
    setsockopt(s, SOL_RAW, IPV6_CHECKSUM, &v, sizeof v);

    struct icmp6_filter f;
    l = sizeof f;
    getsockopt(s, IPPROTO_ICMPV6, ICMP6_FILTER, &f, &l);
    printf("ICMP6_FILTER %#x %#x len %d\n", f.icmp6_filt[0], f.icmp6_filt[4], (int)l);
    ICMP6_FILTER_SETBLOCKALL(&f);
    ICMP6_FILTER_SETPASS(ICMP6_ECHO_REPLY, &f);
    setsockopt(s, IPPROTO_ICMPV6, ICMP6_FILTER, &f, sizeof f);
    memset(&f, 0, sizeof f);
    getsockopt(s, IPPROTO_ICMPV6, ICMP6_FILTER, &f, &l);
    printf("ICMP6_FILTER %#x %#x\n", f.icmp6_filt[0], f.icmp6_filt[4]);

    struct sockaddr_in6 to = {.sin6_family = AF_INET6, .sin6_addr = IN6ADDR_LOOPBACK_INIT};
    unsigned char pkt[64] = {0}, buf[256];
    struct icmp6_hdr *h = (void *)pkt;
    h->icmp6_type = ICMP6_ECHO_REQUEST;
    h->icmp6_id = htons(0x4321);
    h->icmp6_seq = htons(1);
    for (int i = 8; i < 64; i++)
        pkt[i] = i;
    if (sendto(s, pkt, 64, 0, (void *)&to, sizeof to) != 64)
        printf("sendto: %s\n", strerror(errno));
    struct sockaddr_in6 from;
    l = sizeof from;
    int n = recvfrom(s, buf, sizeof buf, 0, (void *)&from, &l);
    char a[64];
    printf("recvfrom %d from %s\n", n, inet_ntop(AF_INET6, &from.sin6_addr, a, sizeof a));
    /* Pseudo-header: source and destination addresses, length, next header */
    unsigned char pseudo[40] = {0};
    memcpy(pseudo, &from.sin6_addr, 16);
    memcpy(pseudo + 16, &to.sin6_addr, 16);
    pseudo[35] = n;
    pseudo[39] = IPPROTO_ICMPV6;
    h = (void *)buf;
    printf("  icmp6 type %d id %#x seq %d checksum %s\n", h->icmp6_type, ntohs(h->icmp6_id),
           ntohs(h->icmp6_seq), fold(add(add(0, pseudo, 40), buf, n)) == 0xffff ? "ok" : "bad");
    close(s);
}

/* Without CAP_NET_RAW raw sockets cannot be created; a wrong protocol number is reported first */
static void unprivileged(void)
{
    fflush(stdout);
    if (fork() == 0) {
        if (setgid(65534) || setuid(65534))
            perror("setuid");
        int cases[][3] = {{AF_INET, SOCK_RAW, IPPROTO_ICMP}, {AF_INET6, SOCK_RAW, IPPROTO_ICMPV6},
                          {AF_INET, SOCK_RAW, IPPROTO_UDP}, {AF_INET, SOCK_RAW, 0},
                          {AF_INET, SOCK_RAW, 300}, {AF_INET, SOCK_DGRAM, IPPROTO_ICMP}};
        for (int i = 0; i < 6; i++) {
            int s = socket(cases[i][0], cases[i][1], cases[i][2]);
            printf("nobody socket(%d, %d, %d): %s\n", cases[i][0], cases[i][1], cases[i][2],
                   s < 0 ? strerror(errno) : "ok");
            close(s);
        }
        fflush(stdout);
        _exit(0);
    }
    wait(NULL);
}

int main(void)
{
    v4();
    v6();
    unprivileged();
    return 0;
}
C
gcc -O1 -o rawicmp rawicmp.c
./rawicmp

# busybox's ping and ping6 use raw sockets when run as root, and switch to ping sockets when an
# ordinary user gets EPERM creating a raw socket
ping -c 2 -s 100 127.0.0.1 | sed 's/ time=.*//; /round-trip/d'
ping6 -c 1 ::1 | sed 's/ time=.*//; /round-trip/d'
su -s /bin/sh nobody -c 'ping -c 1 127.0.0.1' | sed 's/ time=.*//; /round-trip/d'

# PR_SET_CHILD_SUBREAPER: orphans are reparented to the nearest subreaper among their ancestors,
# which also reaps the ones already zombies; after it exits its orphans go to the next one up;
# fork does not inherit it, execve keeps it; without a subreaper they go to process 1
cat > subreaper.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>

static int get(void)
{
    int v = -1;
    prctl(PR_GET_CHILD_SUBREAPER, &v);
    return v;
}

static void wait_byte(int fd)
{
    char c;
    if (read(fd, &c, 1) != 1)
        perror("read");
}

int main(int argc, char **argv)
{
    if (argc > 1) {
        printf("after execve: %d\n", get());
        return 0;
    }
    pid_t me = getpid(), r;
    int st;
    /* Line buffering: forked processes do not output again what the parent has buffered */
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("default: %d\n", get());
    printf("set 5: %d, now %d\n", prctl(PR_SET_CHILD_SUBREAPER, 5), get());
    printf("set 0: %d, now %d\n", prctl(PR_SET_CHILD_SUBREAPER, 0), get());
    errno = 0;
    printf("get into a bad pointer: %d %s\n", prctl(PR_GET_CHILD_SUBREAPER, (int *)8),
           strerror(errno));
    prctl(PR_SET_CHILD_SUBREAPER, 1);
    fflush(stdout);

    if (fork() == 0) {
        printf("child: %d\n", get());
        prctl(PR_SET_CHILD_SUBREAPER, 1);
        fflush(stdout);
        execl("/proc/self/exe", argv[0], "again", (char *)NULL);
        _exit(1);
    }
    wait(NULL);

    /* The grandchild is reparented here after its parent exits, and reaped here */
    int p[2];
    pipe(p);
    pid_t a = fork();
    if (a == 0) {
        if (fork() == 0) {
            wait_byte(p[0]);
            printf("orphan's parent is the subreaper: %s\n", getppid() == me ? "yes" : "no");
            fflush(stdout);
            _exit(3);
        }
        _exit(0);
    }
    waitpid(a, NULL, 0);
    write(p[1], "x", 1);
    r = waitpid(-1, &st, 0);
    printf("reaped the orphan: %s, status %d\n", r > 0 && r != a ? "yes" : "no", WEXITSTATUS(st));

    /* A grandchild that is already a zombie when its parent exits */
    a = fork();
    if (a == 0) {
        pid_t g = fork();
        if (g == 0)
            _exit(5);
        siginfo_t si;
        waitid(P_PID, g, &si, WEXITED | WNOWAIT);
        _exit(0);
    }
    waitpid(a, NULL, 0);
    r = waitpid(-1, &st, 0);
    printf("reaped the zombie: %s, status %d\n", r > 0 ? "yes" : "no", WEXITSTATUS(st));

    /* The nearest subreaper; after it exits they go to the next one up */
    int q[2], back[2];
    pipe(q);
    pipe(back);
    pid_t b = fork();
    if (b == 0) {
        prctl(PR_SET_CHILD_SUBREAPER, 1);
        pid_t bpid = getpid();
        pid_t c = fork();
        if (c == 0) {
            if (fork() == 0) {
                wait_byte(q[0]);
                printf("adopted by the nearest subreaper: %s\n", getppid() == bpid ? "yes" : "no");
                fflush(stdout);
                write(back[1], "x", 1);
                wait_byte(q[0]);
                printf("then by the next one up: %s\n", getppid() == me ? "yes" : "no");
                fflush(stdout);
                _exit(0);
            }
            _exit(0);
        }
        waitpid(c, NULL, 0);
        write(q[1], "x", 1);
        wait_byte(back[0]);
        _exit(0);
    }
    waitpid(b, NULL, 0);
    write(q[1], "x", 1);
    r = waitpid(-1, &st, 0);
    printf("reaped it: %s\n", r > 0 && r != b ? "yes" : "no");

    /* After clearing it, orphans go to process 1 */
    prctl(PR_SET_CHILD_SUBREAPER, 0);
    a = fork();
    if (a == 0) {
        if (fork() == 0) {
            wait_byte(p[0]);
            printf("without a subreaper: adopted by pid %d\n", getppid());
            fflush(stdout);
            write(back[1], "x", 1);
            _exit(0);
        }
        _exit(0);
    }
    waitpid(a, NULL, 0);
    write(p[1], "x", 1);
    wait_byte(back[0]);
    return 0;
}
C
gcc -O1 -o subreaper subreaper.c && ./subreaper

# Positional writes on an O_APPEND file still write at the end and leave the file position
# unchanged (BUGS in pwrite(2); musl's pwrite uses RWF_NOAPPEND of pwritev2 instead, so the
# system call is called directly here); readlink with an empty path reads dirfd itself, and when
# that is not a symlink it is ENOENT
cat > positional.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

static void show(const char *what, int fd, long n)
{
    struct stat st;
    fstat(fd, &st);
    printf("%s: %ld, size %ld, position %ld\n", what, n, (long)st.st_size,
           (long)lseek(fd, 0, SEEK_CUR));
}

static void readlink_empty(const char *what, int dirfd)
{
    char buf[64] = {0};
    errno = 0;
    long n = syscall(SYS_readlinkat, dirfd, "", buf, sizeof buf);
    printf("readlinkat(%s, \"\"): %ld %s\n", what, n, n < 0 ? strerror(errno) : buf);
}

int main(void)
{
    int fd = open("/tmp/append", O_CREAT | O_TRUNC | O_RDWR, 0644);
    write(fd, "0123456789", 10);
    close(fd);
    fd = open("/tmp/append", O_RDWR | O_APPEND);
    lseek(fd, 3, SEEK_SET);
    show("pwrite64 on O_APPEND", fd, syscall(SYS_pwrite64, fd, "ab", 2, 0));
    struct iovec v = {"cd", 2};
    show("pwritev on O_APPEND", fd, syscall(SYS_pwritev, fd, &v, 1, 0, 0));
    int plain = open("/tmp/append", O_RDWR);
    show("pwrite64 without O_APPEND", plain, syscall(SYS_pwrite64, plain, "XY", 2, 0));
    char all[32] = {0};
    pread(plain, all, sizeof all - 1, 0);
    printf("contents %s\n", all);

    unlink("/tmp/rl-link");
    symlink("target", "/tmp/rl-link");
    int link = open("/tmp/rl-link", O_PATH | O_NOFOLLOW);
    int dir = open("/tmp", O_PATH | O_DIRECTORY);
    int p[2];
    pipe(p);
    readlink_empty("AT_FDCWD", AT_FDCWD);
    readlink_empty("symlink", link);
    readlink_empty("directory", dir);
    readlink_empty("regular file", fd);
    readlink_empty("pipe", p[0]);
    readlink_empty("bad fd", 999);
    return 0;
}
C
gcc -O1 -o positional positional.c && ./positional

# A raw ICMP socket receives all arriving ICMP (through the aggregation point in Tuxport): port
# unreachables caused by packets other UDP sockets send (quoting the original IP header, UDP
# header and payload), replies to echoes other raw sockets send; an unconnected UDP socket sees
# no error, a connected one sees only hard errors that match its connection (ECONNREFUSED); once
# a raw socket turns on IP_RECVERR, time exceeded reported by the gateway (TTL 1 to an external
# address) goes into its error queue. The TTL and id of the quoted IP header are not compared
cat > icmperr.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <netinet/ip.h>
#include <netinet/ip_icmp.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define ICMP_FILTER 1

/* linux/errqueue.h (the store has no kernel headers) */
struct sock_extended_err {
    unsigned ee_errno;
    unsigned char ee_origin, ee_type, ee_code, ee_pad;
    unsigned ee_info, ee_data;
};
#define SO_EE_OFFENDER(ee) ((struct sockaddr *)((ee) + 1))

static unsigned long add(unsigned long acc, const unsigned char *b, int n)
{
    for (int i = 0; i + 1 < n; i += 2)
        acc += b[i] << 8 | b[i + 1];
    if (n & 1)
        acc += b[n - 1] << 8;
    return acc;
}

static int sum_ok(const void *p, int n)
{
    unsigned long acc = add(0, p, n);
    while (acc >> 16)
        acc = (acc & 0xffff) + (acc >> 16);
    return acc == 0xffff;
}

static int raw(unsigned pass)
{
    int s = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    unsigned filt = ~pass;
    setsockopt(s, SOL_RAW, ICMP_FILTER, &filt, sizeof filt);
    return s;
}

static int udp(void)
{
    int u = socket(AF_INET, SOCK_DGRAM, 0), ttl = 64;
    setsockopt(u, IPPROTO_IP, IP_TTL, &ttl, sizeof ttl);
    return u;
}

static struct sockaddr_in lo(int port)
{
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port),
                            .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    return a;
}

static const char *err(long n)
{
    return n < 0 ? strerror(errno) : "ok";
}

/* Wait for the raw socket to receive a packet, and print the outer and the quoted UDP */
static void unreachable(int r, int port)
{
    unsigned char b[512];
    struct pollfd p = {r, POLLIN, 0};
    if (poll(&p, 1, 3000) != 1) {
        printf("  nothing\n");
        return;
    }
    int n = recv(r, b, sizeof b, 0);
    struct ip *o = (void *)b;
    struct icmp *ic = (void *)(b + o->ip_hl * 4);
    struct ip *in = &ic->icmp_ip;
    unsigned char *u = (unsigned char *)in + in->ip_hl * 4;
    printf("  %d bytes from %s:", n, inet_ntoa(o->ip_src));
    printf(" type %d code %d checksum %s; quoted proto %d src %s", ic->icmp_type, ic->icmp_code,
           sum_ok(ic, n - o->ip_hl * 4) ? "ok" : "bad", in->ip_p, inet_ntoa(in->ip_src));
    printf(" dst %s len %d header %s dport %s udp len %d payload %.4s\n", inet_ntoa(in->ip_dst),
           ntohs(in->ip_len), sum_ok(in, 20) ? "ok" : "bad",
           u[2] << 8 == (port & 0xff00) && u[3] == (port & 0xff) ? "matches" : "differs",
           u[4] << 8 | u[5], (char *)u + 8);
}

int main(void)
{
    int r = raw(1u << ICMP_DEST_UNREACH), closed = 33999;
    struct sockaddr_in to = lo(closed);
    char buf[64];

    printf("unconnected UDP to a closed port:\n");
    int u = udp();
    sendto(u, "abcd", 4, 0, (void *)&to, sizeof to);
    unreachable(r, closed);
    printf("  UDP sendto again: %s\n", err(sendto(u, "abcd", 4, 0, (void *)&to, sizeof to)));
    unreachable(r, closed);
    usleep(100000);
    printf("  recv: %s\n", err(recv(u, buf, sizeof buf, MSG_DONTWAIT)));
    int e = -1;
    socklen_t l = sizeof e;
    getsockopt(u, SOL_SOCKET, SO_ERROR, &e, &l);
    printf("  SO_ERROR %d, error queue: %s\n", e, err(recv(u, buf, sizeof buf, MSG_ERRQUEUE)));

    printf("connected UDP to a closed port:\n");
    int c = udp();
    connect(c, (void *)&to, sizeof to);
    send(c, "abcd", 4, 0);
    unreachable(r, closed);
    printf("  recv: %s\n", err(recv(c, buf, sizeof buf, MSG_DONTWAIT)));
    printf("  recv again: %s\n", err(recv(c, buf, sizeof buf, MSG_DONTWAIT)));

    printf("connected UDP, sendto another closed port:\n");
    struct sockaddr_in other = lo(closed + 1);
    int c2 = udp();
    connect(c2, (void *)&to, sizeof to);
    sendto(c2, "abcd", 4, 0, (void *)&other, sizeof other);
    unreachable(r, closed + 1);
    printf("  recv: %s\n", err(recv(c2, buf, sizeof buf, MSG_DONTWAIT)));
    struct sockaddr_in peer;
    l = sizeof peer;
    getpeername(c2, (void *)&peer, &l);
    printf("  peer port %d\n", ntohs(peer.sin_port));

    printf("echo from another raw socket:\n");
    int s = raw(0), r2 = raw(1u << ICMP_ECHOREPLY);
    unsigned char e1[16] = {ICMP_ECHO, 0, 0, 0, 0x12, 0x34, 0, 1};
    unsigned long acc = add(0, e1, sizeof e1);
    while (acc >> 16)
        acc = (acc & 0xffff) + (acc >> 16);
    e1[2] = ~acc >> 8;
    e1[3] = ~acc;
    struct sockaddr_in any = lo(0);
    sendto(s, e1, sizeof e1, 0, (void *)&any, sizeof any);
    struct pollfd p = {r2, POLLIN, 0};
    if (poll(&p, 1, 3000) == 1) {
        unsigned char b[128];
        int n = recv(r2, b, sizeof b, 0);
        struct icmp *ic = (void *)(b + 20);
        printf("  %d bytes: type %d id %#x seq %d\n", n, ic->icmp_type, ntohs(ic->icmp_id),
               ntohs(ic->icmp_seq));
    }

    printf("IP_RECVERR on a raw socket, TTL 1 to an outside address:\n");
    int q = raw(~0u), on = 1, one = 1;
    setsockopt(q, IPPROTO_IP, IP_RECVERR, &on, sizeof on);
    struct sockaddr_in out = {.sin_family = AF_INET, .sin_addr.s_addr = inet_addr("1.1.1.1")};
    connect(q, (void *)&out, sizeof out);
    setsockopt(q, IPPROTO_IP, IP_TTL, &one, sizeof one);
    unsigned char e2[16] = {ICMP_ECHO, 0, 0, 0, 0x43, 0x21, 0, 1};
    send(q, e2, sizeof e2, 0);
    p.fd = q;
    p.events = 0;
    printf("  poll %d revents%s\n", poll(&p, 1, 3000), p.revents & POLLERR ? " POLLERR" : "");
    unsigned char data[64], ctl[256];
    struct sockaddr_in from;
    struct iovec iov = {data, sizeof data};
    struct msghdr m = {.msg_name = &from, .msg_namelen = sizeof from, .msg_iov = &iov,
                       .msg_iovlen = 1, .msg_control = ctl, .msg_controllen = sizeof ctl};
    int n = recvmsg(q, &m, MSG_ERRQUEUE);
    printf("  error queue: %d, flags %s, original type %d id %#x seq %d, to %s\n", n,
           m.msg_flags & MSG_ERRQUEUE ? "MSG_ERRQUEUE" : "-", data[0], data[4] << 8 | data[5],
           data[6] << 8 | data[7], inet_ntoa(from.sin_addr));
    for (struct cmsghdr *h = CMSG_FIRSTHDR(&m); h; h = CMSG_NXTHDR(&m, h)) {
        if (h->cmsg_level != IPPROTO_IP || h->cmsg_type != IP_RECVERR)
            continue;
        struct sock_extended_err *ee = (void *)CMSG_DATA(h);
        struct sockaddr_in *off = (void *)SO_EE_OFFENDER(ee);
        printf("  errno %s origin %d type %d code %d offender %s\n", strerror(ee->ee_errno),
               ee->ee_origin, ee->ee_type, ee->ee_code, inet_ntoa(off->sin_addr));
    }
    l = sizeof e;
    getsockopt(q, SOL_SOCKET, SO_ERROR, &e, &l);
    printf("  then SO_ERROR %d, error queue: %s\n", e, err(recv(q, buf, sizeof buf, MSG_ERRQUEUE)));
    return 0;
}
C
gcc -O1 -o icmperr icmperr.c && ./icmperr

# busybox's traceroute: UDP probes (a different port for each) and -I echoes, both received with
# another raw socket. Only to a local address: the gateway rate-limits ICMP (one per second), so
# with several probes sent in a row the answers come and go even natively
traceroute -n -q 2 -w 2 127.0.0.1 2>&1 | sed 's/ [0-9.]* ms//g'
traceroute -I -n -q 2 -w 2 127.0.0.1 2>&1 | sed 's/ [0-9.]* ms//g'
traceroute6 -n -q 2 -w 2 ::1 2>&1 | sed 's/ [0-9.]* ms//g'

# Flags of F_GETFL and fdinfo: files opened by open have O_LARGEFILE (O_PATH ones do not), and
# pipes, sockets, eventfds, epolls, memfds and the like follow their own system calls
python3 - <<'PY'
import os, socket, select, fcntl
def show(what, fd):
    with open("/proc/self/fdinfo/%d" % fd) as f:
        info = [l.split()[1] for l in f if l.startswith("flags:")][0]
    print(what, oct(fcntl.fcntl(fd, fcntl.F_GETFL)), info)
show("open", os.open("/tmp", os.O_RDONLY))
show("open wronly", os.open("/tmp/largefile", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600))
show("o_path", os.open("/tmp", os.O_PATH))
r, w = os.pipe()
show("pipe r", r)
show("pipe w", w)
keep = [socket.socket(), socket.socketpair(), select.epoll()]
show("socket", keep[0].fileno())
show("unix pair", keep[1][0].fileno())
show("eventfd", os.eventfd(0, os.EFD_NONBLOCK))
show("epoll", keep[2].fileno())
show("memfd", os.memfd_create("x"))
show("reopened pipe", os.open("/proc/self/fd/%d" % r, os.O_RDONLY))
PY

# /proc/<pid>/task/<tid>/children: the children a thread created (zombies included), not
# counting threads; after the creating thread exits they go to the end of a thread still alive in
# the same process; those created with CLONE_PARENT count for the caller's parent thread; orphans
# adopted by a subreaper; after a non-main thread execs they all belong to it (it takes over the
# main thread's ID). pids are replaced with names for comparison
cat > children.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t pids[32];
static const char *names[32];
static int n;
static int fds[2];
static pthread_barrier_t bar;
static pid_t thread_tid;

static void add(pid_t p, const char *name)
{
    pids[n] = p;
    names[n++] = name;
}

static const char *name_of(pid_t p)
{
    for (int i = 0; i < n; i++)
        if (pids[i] == p)
            return names[i];
    return "?";
}

static void show(const char *what, pid_t tid)
{
    char path[64], buf[1024];
    snprintf(path, sizeof path, "/proc/self/task/%d/children", tid);
    FILE *f = fopen(path, "r");
    if (!f) {
        printf("%s: %s\n", what, strerror(errno));
        return;
    }
    size_t len = fread(buf, 1, sizeof buf - 1, f);
    fclose(f);
    buf[len] = 0;
    int tail = len == 0 || buf[len - 1] == ' ';
    printf("%s:", what);
    char *save;
    for (char *t = strtok_r(buf, " ", &save); t; t = strtok_r(NULL, " ", &save))
        printf(" %s", name_of(atoi(t)));
    printf("%s\n", tail ? "" : " (no trailing space)");
}

static pid_t spawn(const char *name)
{
    pid_t p = fork();
    if (p == 0) {
        pause();
        _exit(0);
    }
    add(p, name);
    return p;
}

/* When pthread_join returns the thread may not have finished exiting (Linux wakes the joining
 * thread before handing over its children), so wait until it is gone from /proc/self/task */
static void wait_gone(pid_t tid)
{
    char path[64];
    snprintf(path, sizeof path, "/proc/self/task/%d", tid);
    while (access(path, F_OK) == 0)
        usleep(1000);
}

static void *worker(void *arg)
{
    thread_tid = gettid();
    spawn("t1");
    pthread_barrier_wait(&bar);
    pthread_barrier_wait(&bar);
    return NULL;
}

static void *execer(void *arg)
{
    static char pairs[32][32];
    char *argv[40];
    int a = 0;
    spawn("f");
    argv[a++] = "children";
    argv[a++] = "after-exec";
    for (int i = 0; i < n; i++) {
        snprintf(pairs[i], sizeof pairs[i], "%s=%d", names[i], pids[i]);
        argv[a++] = pairs[i];
    }
    argv[a] = NULL;
    execv("/proc/self/exe", argv);
    perror("execv");
    _exit(1);
}

static void wait_zombie(pid_t p)
{
    char path[64], buf[512];
    snprintf(path, sizeof path, "/proc/%d/stat", p);
    for (;;) {
        FILE *f = fopen(path, "r");
        size_t len = f ? fread(buf, 1, sizeof buf - 1, f) : 0;
        if (f)
            fclose(f);
        buf[len] = 0;
        char *s = strrchr(buf, ')');
        if (s && s[1] && s[2] == 'Z')
            return;
        usleep(1000);
    }
}

int main(int argc, char **argv)
{
    /* What is still unwritten in the buffer when a thread execs is lost */
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1 && strcmp(argv[1], "after-exec") == 0) {
        for (int i = 2; i < argc; i++) {
            char *eq = strchr(argv[i], '=');
            *eq = 0;
            add(atoi(eq + 1), argv[i]);
        }
        printf("after exec, tid is the pid: %d\n", gettid() == getpid());
        show("after exec", getpid());
        for (int i = 0; i < n; i++)
            kill(pids[i], SIGKILL);
        while (wait(NULL) > 0)
            ;
        return 0;
    }
    pid_t me = getpid();
    spawn("a");
    pid_t b = spawn("b");
    pthread_t t;
    pthread_barrier_init(&bar, NULL, 2);
    pthread_create(&t, NULL, worker, NULL);
    pthread_barrier_wait(&bar);
    show("main", me);
    show("thread", thread_tid);
    printf("/proc/self/children: %s\n", access("/proc/self/children", F_OK) ? strerror(errno) : "exists");
    pthread_barrier_wait(&bar);
    pthread_join(t, NULL);
    wait_gone(thread_tid);
    show("after the thread exited", me);
    kill(b, SIGKILL);
    wait_zombie(b);
    show("b is a zombie", me);
    waitpid(b, NULL, 0);
    show("b reaped", me);

    pipe(fds);
    pid_t c = fork();
    if (c == 0) {
        long gc = syscall(SYS_clone, CLONE_PARENT, 0, 0, 0, 0);
        if (gc == 0) {
            pause();
            _exit(0);
        }
        write(fds[1], &gc, sizeof gc);
        pause();
        _exit(0);
    }
    add(c, "c");
    long gc;
    read(fds[0], &gc, sizeof gc);
    add(gc, "gc");
    show("CLONE_PARENT", me);

    prctl(PR_SET_CHILD_SUBREAPER, 1);
    pid_t d = fork();
    if (d == 0) {
        long e = fork();
        if (e == 0) {
            pause();
            _exit(0);
        }
        write(fds[1], &e, sizeof e);
        _exit(0);
    }
    add(d, "d");
    long e;
    read(fds[0], &e, sizeof e);
    add(e, "e");
    waitpid(d, NULL, 0);
    show("orphan taken by the subreaper", me);

    pthread_create(&t, NULL, execer, NULL);
    pthread_join(t, NULL);
    return 1;
}
C
gcc -O1 -pthread -o children children.c && ./children
echo "children in task/<tid>: $(ls /proc/$$/task/$$ | grep -c '^children$'), in /proc/<pid>: $(ls /proc/$$ | grep -c '^children$')"

# PR_SET_PDEATHSIG is per thread: it is sent as soon as the thread that created the child exits
# (even while the process lives on), not when other threads exit; the setting follows the thread
# that set it, new threads and forked children have 0, execve keeps it, and setuid clears it;
# siginfo is SI_USER with the parent as sender. __WNOTHREAD of wait waits only for children
# created by the calling thread
cat > pdeath.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef __WNOTHREAD
#define __WNOTHREAD 0x20000000
#endif

static int fds[2];
static pid_t parent;
static char *self;

/* The child waits for PR_SET_PDEATHSIG signals: those received within ms milliseconds are written
 * into the pipe */
static void await_signals(int ms)
{
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGUSR2);
    char buf[256] = "", one[96];
    siginfo_t si;
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    while (sigtimedwait(&set, &si, &ts) > 0) {
        snprintf(one, sizeof one, " %s code %d from parent %d uid %d",
                 si.si_signo == SIGUSR1 ? "USR1" : "USR2", si.si_code, si.si_pid == parent,
                 (int)si.si_uid);
        strcat(buf, one);
    }
    strcat(buf, buf[0] ? "\n" : " none\n");
    write(fds[1], buf, strlen(buf));
}

static void block(void)
{
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGUSR2);
    sigprocmask(SIG_BLOCK, &set, NULL);
}

static int stay;
static volatile int usr2_set;
static void *set_usr2(void *arg)
{
    prctl(PR_SET_PDEATHSIG, SIGUSR2);
    usr2_set = 1;
    if (stay)
        for (;;)
            pause();
    return NULL;
}

/* how: 0 sets SIGUSR1 only in the main thread; 1 another thread also sets SIGUSR2 and stays
 * alive; 2 exec after setting; 3 another thread sets SIGUSR2 and then exits (the setting goes
 * with it) */
static pid_t spawn(int how, int ms)
{
    pid_t p = fork();
    if (p == 0) {
        block();
        prctl(PR_SET_PDEATHSIG, SIGUSR1);
        if (how == 1 || how == 3) {
            pthread_t t;
            stay = how == 1;
            pthread_create(&t, NULL, set_usr2, NULL);
            if (stay)
                while (!usr2_set)
                    usleep(1000);
            else
                pthread_join(t, NULL);
        }
        if (how == 2) {
            char a[16], b[16], c[16];
            snprintf(a, sizeof a, "%d", fds[1]);
            snprintf(b, sizeof b, "%d", parent);
            snprintf(c, sizeof c, "%d", ms);
            execl(self, self, "pdeath-exec", a, b, c, (char *)NULL);
            _exit(1);
        }
        char ok = 'r';
        write(fds[1], &ok, 1);
        await_signals(ms);
        _exit(0);
    }
    char c;
    read(fds[0], &c, 1);
    return p;
}

static void report(const char *what, pid_t p)
{
    char buf[300];
    waitpid(p, NULL, 0);
    ssize_t n = read(fds[0], buf, sizeof buf - 1);
    buf[n > 0 ? n : 0] = 0;
    printf("%s:%s", what, buf);
}

static pid_t from_thread;
static int how_in_thread;
static void *thread_spawn(void *arg)
{
    from_thread = spawn(how_in_thread, 1000);
    return NULL;
}

static void *nothing(void *arg) { return NULL; }

static void *get_pdeath(void *arg)
{
    int s = -1;
    prctl(PR_GET_PDEATHSIG, &s);
    printf("a new thread sees %d\n", s);
    return NULL;
}

static const char *err(long r) { return r < 0 ? strerror(errno) : "ok"; }

static pid_t a_pid, b_pid, c_pid, waiter_tid;
static void *waiter_thread(void *arg)
{
    int st;
    waiter_tid = gettid();
    b_pid = fork();
    if (b_pid == 0)
        _exit(7);
    pid_t r = waitpid(-1, &st, __WNOTHREAD);
    printf("thread waits any: %s, status %d\n", r == b_pid ? "its own child" : "other", WEXITSTATUS(st));
    r = waitpid(-1, &st, WNOHANG | __WNOTHREAD);
    printf("thread waits any again: %s\n", err(r));
    r = waitpid(a_pid, &st, WNOHANG | __WNOTHREAD);
    printf("thread waits the main thread's child: %s\n", err(r));
    r = waitpid(a_pid, &st, WNOHANG);
    printf("without __WNOTHREAD: %ld\n", (long)r);
    siginfo_t si;
    printf("waitid: %s\n", err(waitid(P_ALL, 0, &si, WEXITED | WNOHANG | __WNOTHREAD)));
    c_pid = fork();
    if (c_pid == 0) {
        pause();
        _exit(0);
    }
    return NULL;
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    self = argv[0];
    if (argc > 1 && strcmp(argv[1], "pdeath-exec") == 0) {
        fds[1] = atoi(argv[2]);
        parent = atoi(argv[3]);
        int s = -1;
        prctl(PR_GET_PDEATHSIG, &s);
        char buf[64];
        snprintf(buf, sizeof buf, "after exec %d,", s);
        write(fds[1], "r", 1);
        write(fds[1], buf, strlen(buf));
        await_signals(atoi(argv[4]));
        _exit(0);
    }
    pipe(fds);
    parent = getpid();
    pthread_t t;

    how_in_thread = 0;
    pthread_create(&t, NULL, thread_spawn, NULL);
    pthread_join(t, NULL);
    report("creating thread exited", from_thread);

    pid_t p = spawn(0, 300);
    pthread_create(&t, NULL, nothing, NULL);
    pthread_join(t, NULL);
    report("another thread exited", p);

    how_in_thread = 1;
    pthread_create(&t, NULL, thread_spawn, NULL);
    pthread_join(t, NULL);
    report("set in two threads", from_thread);

    how_in_thread = 3;
    pthread_create(&t, NULL, thread_spawn, NULL);
    pthread_join(t, NULL);
    report("set in a thread that exited", from_thread);

    how_in_thread = 2;
    pthread_create(&t, NULL, thread_spawn, NULL);
    pthread_join(t, NULL);
    report("set before exec", from_thread);

    prctl(PR_SET_PDEATHSIG, SIGUSR2);
    int s = -1;
    prctl(PR_GET_PDEATHSIG, &s);
    printf("own %d\n", s);
    pthread_create(&t, NULL, get_pdeath, NULL);
    pthread_join(t, NULL);
    p = fork();
    if (p == 0) {
        prctl(PR_GET_PDEATHSIG, &s);
        printf("a forked child sees %d\n", s);
        prctl(PR_SET_PDEATHSIG, SIGUSR1);
        setuid(1000);
        prctl(PR_GET_PDEATHSIG, &s);
        printf("after setuid %d\n", s);
        _exit(0);
    }
    waitpid(p, NULL, 0);
    prctl(PR_SET_PDEATHSIG, 0);


    a_pid = fork();
    if (a_pid == 0) {
        pause();
        _exit(0);
    }
    pthread_create(&t, NULL, waiter_thread, NULL);
    pthread_join(t, NULL);
    /* When pthread_join returns the thread may not have finished exiting, and its children may not
     * have been handed to the main thread yet: wait until it is gone from /proc/self/task */
    char task[64];
    snprintf(task, sizeof task, "/proc/self/task/%d", waiter_tid);
    while (access(task, F_OK) == 0)
        usleep(1000);
    kill(c_pid, SIGKILL);
    printf("main waits the exited thread's child: %s\n", waitpid(c_pid, NULL, __WNOTHREAD) == c_pid ? "ok" : strerror(errno));
    kill(a_pid, SIGKILL);
    printf("main waits any: %s\n", waitpid(-1, NULL, __WNOTHREAD) == a_pid ? "its own child" : strerror(errno));

    /* Placed last: the grandchild becomes an orphan, and on the native side this program adopts it
     * when it is the container's process 1 */
    pid_t mid = fork();
    if (mid == 0) {
        parent = getpid();
        spawn(0, 1000);
        _exit(0);
    }
    waitpid(mid, NULL, 0);
    char buf[300];
    ssize_t n = read(fds[0], buf, sizeof buf - 1);
    buf[n > 0 ? n : 0] = 0;
    printf("parent process exited:%s", buf);
    return 0;
}
C
gcc -O1 -pthread -o pdeath pdeath.c && ./pdeath

# Short symlinks are stored in the inode and take no blocks: on ext4 those whose targets are
# shorter than 60 bytes (ext4_symlink), so the total of ls -l does not count them (tmpfs ones are
# in the tmpfs case)
cd /tmp
rm -f sl59 sl60
ln -s "$(printf %059d 0)" sl59
ln -s "$(printf %060d 0)" sl60
stat -c '%s %b' sl59 sl60
rm -f sl59 sl60

# /proc/<pid>/exe points at the program file itself (Linux's mm->exe_file): after it is deleted or
# replaced by rename, reading it gives " (deleted)", and following it (stat, open, execve) still
# reaches the original file; one that was only renamed is also the original (the path read
# follows the rename on Linux but not in Tuxport, so it is not compared)
cd /tmp
cat > exelink.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static void show(const char *what, const char *link)
{
    char buf[512];
    ssize_t n = readlink(link, buf, sizeof buf - 1);
    buf[n > 0 ? n : 0] = 0;
    printf("%s: %s\n", what, n > 0 ? buf : strerror(errno));
}

/* stat and open through the link: is it the original file */
static void probe(const char *what, const char *link, const struct stat *orig)
{
    struct stat st;
    if (stat(link, &st) != 0) {
        printf("%s stat: %s\n", what, strerror(errno));
        return;
    }
    printf("%s stat: same file %d, nlink %ld, same size %d, reg %d\n", what,
           st.st_dev == orig->st_dev && st.st_ino == orig->st_ino, (long)st.st_nlink,
           st.st_size == orig->st_size, S_ISREG(st.st_mode));
    int fd = open(link, O_RDONLY);
    if (fd < 0) {
        printf("%s open: %s\n", what, strerror(errno));
        return;
    }
    char magic[4] = {0};
    struct stat fs;
    ssize_t n = read(fd, magic, 4);
    fstat(fd, &fs);
    printf("%s open: %s, same file %d, nlink %ld\n", what,
           n == 4 && memcmp(magic, "\177ELF", 4) == 0 ? "ELF" : "not ELF",
           fs.st_dev == orig->st_dev && fs.st_ino == orig->st_ino, (long)fs.st_nlink);
    close(fd);
}

/* The path on the main program's line in maps */
static void maps(const char *what)
{
    char line[600];
    FILE *f = fopen("/proc/self/maps", "r");
    while (fgets(line, sizeof line, f)) {
        char *p = strchr(line, '/');
        if (p && strstr(p, "exe-")) {
            printf("%s maps: %s", what, p);
            break;
        }
    }
    fclose(f);
}

static void copy_self(const char *to)
{
    char buf[65536];
    int in = open("/proc/self/exe", O_RDONLY), out = open(to, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    ssize_t n;
    while ((n = read(in, buf, sizeof buf)) > 0)
        write(out, buf, n);
    close(in);
    close(out);
}

static void run(const char *path, const char *mode)
{
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        execl(path, "exelink", mode, (char *)NULL);
        perror("execl");
        _exit(1);
    }
    int st;
    waitpid(p, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st))
        printf("%s: status %#x\n", mode, st);
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    struct stat orig;
    if (argc > 1 && !strcmp(argv[1], "deleted")) {
        show("before unlink", "/proc/self/exe");
        stat("/tmp/exe-del", &orig);
        unlink("/tmp/exe-del");
        show("after unlink", "/proc/self/exe");
        probe("after unlink", "/proc/self/exe", &orig);
        maps("after unlink");
        fflush(stdout);
        /* Re-execute ourselves: Linux executes the deleted file */
        char ino[64];
        snprintf(ino, sizeof ino, "%lu:%lu", (unsigned long)orig.st_dev, (unsigned long)orig.st_ino);
        execl("/proc/self/exe", "exelink", "again", ino, (char *)NULL);
        printf("re-exec: %s\n", strerror(errno));
        return 1;
    }
    if (argc > 2 && !strcmp(argv[1], "again")) {
        show("re-exec", "/proc/self/exe");
        struct stat st;
        char ino[64];
        stat("/proc/self/exe", &st);
        snprintf(ino, sizeof ino, "%lu:%lu", (unsigned long)st.st_dev, (unsigned long)st.st_ino);
        printf("re-exec runs the deleted file: %d\n", !strcmp(ino, argv[2]));
        maps("re-exec");
        return 0;
    }
    if (argc > 1 && !strcmp(argv[1], "replaced")) {
        stat("/tmp/exe-up", &orig);
        int fd = open("/tmp/exe-up.new", O_WRONLY | O_CREAT | O_TRUNC, 0755);
        write(fd, "#!/bin/sh\necho new\n", 19);
        close(fd);
        rename("/tmp/exe-up.new", "/tmp/exe-up");
        show("after replace", "/proc/self/exe");
        probe("after replace", "/proc/self/exe", &orig);
        return 0;
    }
    if (argc > 1 && !strcmp(argv[1], "renamed")) {
        stat("/tmp/exe-mv", &orig);
        rename("/tmp/exe-mv", "/tmp/exe-mv2");
        probe("after rename", "/proc/self/exe", &orig);
        unlink("/tmp/exe-mv2");
        return 0;
    }
    if (argc > 1 && !strcmp(argv[1], "waiting")) {
        stat("/tmp/exe-other", &orig);
        unlink("/tmp/exe-other");
        char c;
        /* Tell the parent it has been deleted, and wait until it has looked */
        write(10, "x", 1);
        read(11, &c, 1);
        return 0;
    }

    copy_self("/tmp/exe-del");
    run("/tmp/exe-del", "deleted");
    copy_self("/tmp/exe-up");
    run("/tmp/exe-up", "replaced");
    copy_self("/tmp/exe-mv");
    run("/tmp/exe-mv", "renamed");

    /* Another process's exe: after the child deletes its own program file, look through
     * /proc/<pid>/exe */
    copy_self("/tmp/exe-other");
    stat("/tmp/exe-other", &orig);
    int up[2], down[2];
    pipe2(up, O_CLOEXEC);
    pipe2(down, O_CLOEXEC);
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        dup2(up[1], 10);
        dup2(down[0], 11);
        execl("/tmp/exe-other", "exelink", "waiting", (char *)NULL);
        _exit(1);
    }
    close(up[1]);
    close(down[0]);
    char c, link[64];
    read(up[0], &c, 1);
    snprintf(link, sizeof link, "/proc/%d/exe", p);
    show("other process", link);
    probe("other process", link, &orig);
    close(down[1]);
    waitpid(p, NULL, 0);
    unlink("/tmp/exe-up");
    return 0;
}
C
gcc -O1 -o exelink exelink.c && ./exelink

# DIT is thread state (Linux 6.1): a signal handler runs with the DIT it had when interrupted,
# bit 24 of the signal frame's pstate is it, and sigreturn restores it from the frame; fork and
# threads inherit it, execve clears it; ptrace's NT_PRSTATUS has it in bit 24 of pstate, and the
# tracee sees the new value after the tracer changes it. When the host has no DIT both sides
# only print no DIT
cd /tmp
cat > ditstate.c <<'C'
#define _GNU_SOURCE
#include <elf.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/ptrace.h>
#include <sys/uio.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <ucontext.h>
#include <unistd.h>

static int dit(void)
{
    uint64_t v;
    __asm__ volatile("mrs %0, dit" : "=r"(v));
    return (int)(v >> 24 & 1);
}

static void set(int on)
{
    if (on)
        __asm__ volatile("msr dit, #1");
    else
        __asm__ volatile("msr dit, #0");
}

static volatile int in_handler = -1, frame_bit = -1;

static void handler(int sig, siginfo_t *si, void *p)
{
    ucontext_t *uc = p;
    (void)sig, (void)si;
    in_handler = dit();
    frame_bit = (int)(uc->uc_mcontext.pstate >> 24 & 1);
    uc->uc_mcontext.pstate ^= UINT64_C(1) << 24; /* sigreturn restores it from the frame */
    set(!in_handler);                            /* what the handler sets itself does not count */
}

static void *thread(void *a)
{
    (void)a;
    return (void *)(intptr_t)dit();
}

static uint64_t regs_pstate(pid_t p, struct user_regs_struct *r)
{
    struct iovec io = {r, sizeof *r};
    ptrace(PTRACE_GETREGSET, p, (void *)NT_PRSTATUS, &io);
    return r->pstate;
}

int main(int argc, char **argv)
{
    if (!(getauxval(AT_HWCAP) & HWCAP_DIT)) {
        printf("no DIT\n");
        return 0;
    }
    if (argc > 1) {
        printf("after execve: %d\n", dit());
        return 0;
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("start: %d\n", dit());
    struct sigaction sa = {.sa_sigaction = handler, .sa_flags = SA_SIGINFO};
    sigaction(SIGUSR1, &sa, NULL);
    for (int on = 1; on >= 0; on--) {
        set(on);
        raise(SIGUSR1);
        printf("dit %d: handler %d, frame bit %d, after sigreturn %d\n", on, in_handler, frame_bit,
               dit());
    }
    set(1);
    pid_t p = fork();
    if (p == 0) {
        printf("fork child: %d\n", dit());
        _exit(0);
    }
    waitpid(p, NULL, 0);
    pthread_t t;
    void *r;
    pthread_create(&t, NULL, thread, NULL);
    pthread_join(t, &r);
    printf("thread: %d\n", (int)(intptr_t)r);
    p = fork();
    if (p == 0) {
        execl("/proc/self/exe", argv[0], "exec", (char *)NULL);
        _exit(1);
    }
    waitpid(p, NULL, 0);

    p = fork();
    if (p == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        set(1);
        raise(SIGSTOP);
        printf("tracee after the tracer cleared it: %d\n", dit());
        set(0);
        raise(SIGSTOP);
        printf("tracee after the tracer set it: %d\n", dit());
        _exit(0);
    }
    struct user_regs_struct regs;
    int st;
    waitpid(p, &st, 0);
    printf("tracer sees %d\n", (int)(regs_pstate(p, &regs) >> 24 & 1));
    regs.pstate &= ~(UINT64_C(1) << 24);
    struct iovec io = {&regs, sizeof regs};
    ptrace(PTRACE_SETREGSET, p, (void *)NT_PRSTATUS, &io);
    printf("tracer reads back %d\n", (int)(regs_pstate(p, &regs) >> 24 & 1));
    ptrace(PTRACE_CONT, p, 0, 0);
    waitpid(p, &st, 0);
    printf("tracer sees %d\n", (int)(regs_pstate(p, &regs) >> 24 & 1));
    regs.pstate |= UINT64_C(1) << 24;
    ptrace(PTRACE_SETREGSET, p, (void *)NT_PRSTATUS, &io);
    ptrace(PTRACE_CONT, p, 0, 0);
    waitpid(p, &st, 0);
    printf("dit at exit: %d\n", dit());
    return 0;
}
C
gcc -O1 -march=armv8.4-a -pthread -o ditstate ditstate.c && ./ditstate

# /proc/stat's btime is fixed while the clock is not set (Linux: realtime minus boottime in
# nanoseconds, then whole seconds), so ps -o lstart (btime plus the start time in
# /proc/<pid>/stat) does not change between reads; reads over more than a second sweep the
# fraction of the uptime, which made Tuxport's value alternate between two seconds
cat > btime.c <<'C'
#include <stdio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static unsigned long long btime(void)
{
    char line[256];
    unsigned long long b = 0;
    FILE *f = fopen("/proc/stat", "r");
    while (fgets(line, sizeof line, f))
        if (sscanf(line, "btime %llu", &b) == 1)
            break;
    fclose(f);
    return b;
}

int main(void)
{
    unsigned long long first = btime(), start = 0;
    int changed = 0;
    for (int i = 0; i < 60; i++) {
        changed += btime() != first;
        usleep(20000);
    }
    printf("btime changed in %d of 60 reads\n", changed);
    fflush(stdout);
    // A new child, whose start time is now (the shell may exec this program as its last command)
    if (fork() == 0) {
        time_t started = time(NULL);
        FILE *f = fopen("/proc/self/stat", "r");
        fscanf(f, "%*d %*s %*c %*d %*d %*d %*d %*d %*u %*u %*u %*u %*u %*u %*u %*d %*d %*d %*d %*d %*d %llu", &start);
        fclose(f);
        long long lstart = btime() + start / sysconf(_SC_CLK_TCK);
        printf("btime + start time near the real start: %d\n", started - lstart >= 0 && started - lstart <= 2);
        fflush(stdout);
        _exit(0);
    }
    wait(NULL);
    return 0;
}
C
gcc -O1 -o btime btime.c && ./btime

# POSIX record locks belong to the process's fd table: they survive close_range with
# CLOSE_RANGE_UNSHARE (closing other fds) and execve, as checked by another process with F_GETLK.
# open keeps O_SYNC and O_DSYNC in the status flags, and F_SETFL cannot clear them
cat > locks.py <<'PY'
import ctypes, fcntl, os, struct, subprocess, sys

FLOCK = "hh4xqqi4x"

if len(sys.argv) > 2 and sys.argv[1] == "holder":
    fd = os.open(sys.argv[2], os.O_RDWR)
    got = fcntl.fcntl(fd, fcntl.F_GETLK, struct.pack(FLOCK, fcntl.F_WRLCK, 0, 0, 0, 0))
    t, _, _, _, pid = struct.unpack(FLOCK, got)
    print(pid if t != fcntl.F_UNLCK else 0)
    sys.exit(0)


def held_by_me(path):
    out = subprocess.run([sys.executable, __file__, "holder", path], capture_output=True, text=True)
    return out.stdout.strip() == str(os.getpid())


if len(sys.argv) > 1 and sys.argv[1] == "after-exec":
    print("after execve, still held:", held_by_me("/tmp/lock-exec"))
    sys.exit(0)

fd = os.open("/tmp/lock-cr", os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
fcntl.lockf(fd, fcntl.LOCK_EX)
print("locked:", held_by_me("/tmp/lock-cr"))
spare = os.open("/dev/null", os.O_RDONLY)
libc = ctypes.CDLL(None, use_errno=True)
r = libc.syscall(436, spare, spare, 2)  # close_range(spare, spare, CLOSE_RANGE_UNSHARE)
print("close_range with unshare", r, "still held:", held_by_me("/tmp/lock-cr"))

for name, flag in (("O_SYNC", os.O_SYNC), ("O_DSYNC", os.O_DSYNC)):
    f = os.open("/tmp/sync-file", os.O_WRONLY | os.O_CREAT | flag, 0o644)
    fl = fcntl.fcntl(f, fcntl.F_GETFL)
    fcntl.fcntl(f, fcntl.F_SETFL, fl & ~flag)
    after = fcntl.fcntl(f, fcntl.F_GETFL)
    print(name, "in F_GETFL:", fl & flag == flag, "kept after F_SETFL:", after & flag == flag,
          "write:", os.write(f, b"x"))
    os.close(f)

fd2 = os.open("/tmp/lock-exec", os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
fcntl.lockf(fd2, fcntl.LOCK_EX)
os.set_inheritable(fd2, True)
sys.stdout.flush()
os.execv(sys.executable, [sys.executable, __file__, "after-exec"])
PY
python3 locks.py
rm -f locks.py /tmp/lock-cr /tmp/lock-exec /tmp/sync-file

# chown(path, -1, -1) changes no owner but clears the set-id bits of a non-directory: setuid
# always, setgid when group-executable or when the caller is neither in the file's group nor has
# CAP_FSETID. That is a mode change only the owner (or CAP_FOWNER) may make: another user gets EPERM
# on such files and their bits stay; on other files it succeeds. Whether a file capability goes too
# is compared as well. Run once as uid 65534 and once as root
cat > chown.py <<'PY'
import errno, os, struct

caps = struct.pack("<IIIII", 0x02000000, 1 << 13, 0, 0, 0)  # v2, cap_net_raw permitted
cases = (("setuid", 0o4755, 0, 0, False), ("setgid", 0o2755, 0, 0, False),
         ("setgid-noexec", 0o2745, 0, 0, False), ("caps", 0o755, 0, 0, True),
         ("plain", 0o755, 0, 0, False), ("own-setuid", 0o4755, 65534, 65534, False),
         ("own-setgid-noexec", 0o2745, 65534, 65534, False),
         ("own-setgid-noexec-other-group", 0o2745, 65534, 0, False))
for who in ("uid 65534", "root"):
    files = {}
    for name, mode, uid, gid, cap in cases:
        p = "/tmp/chown-" + name
        open(p, "w").close()
        os.chown(p, uid, gid)
        os.chmod(p, mode)
        if cap:
            os.setxattr(p, "security.capability", caps)
        files[name] = p
    r, w = os.pipe()
    if os.fork() == 0:
        os.close(r)
        if who != "root":
            os.setgroups([])
            os.setgid(65534)
            os.setuid(65534)
        out = []
        for name, p in files.items():
            try:
                os.chown(p, -1, -1)
                out.append("%s ok" % name)
            except OSError as e:
                out.append("%s %s" % (name, errno.errorcode[e.errno]))
        os.write(w, "\n".join(out).encode())
        os._exit(0)
    os.close(w)
    results = os.read(r, 4096).decode().split("\n")
    os.close(r)
    os.wait()
    for line in results:
        name = line.split()[0]
        p = files[name]
        try:
            cap = "cap kept" if os.getxattr(p, "security.capability") == caps else "cap changed"
        except OSError:
            cap = "no cap"
        print("chown(-1, -1) by %s:" % who, line, "mode %o" % (os.stat(p).st_mode & 0o7777), cap)
        os.unlink(p)
PY
python3 chown.py
rm -f chown.py

# /proc/<tid> of any thread resolves (Linux's proc_pid_lookup finds any task), though readdir of
# /proc lists only processes: it looks like the process's directory seen through that thread
# (status and comm are the thread's), and goes away when the thread exits
cat > tids.py <<'PY'
import ctypes, os, threading, time

libc = ctypes.CDLL(None, use_errno=True)
PR_SET_NAME = 15
ready, done = threading.Event(), threading.Event()
info = {}


def worker():
    libc.prctl(PR_SET_NAME, b"worker", 0, 0, 0)
    info["tid"] = threading.get_native_id()
    ready.set()
    done.wait()


# A daemon thread, and each check reports its own error: a failing check must not leave the
# script waiting for the thread
t = threading.Thread(target=worker, daemon=True)
t.start()
ready.wait()
pid, tid = os.getpid(), info["tid"]
d = "/proc/%d" % tid


def check(what, f):
    try:
        print(what, f())
    except OSError as e:
        print(what, "failed:", e.strerror)


check("/proc/<tid> is a directory, listed in /proc:",
      lambda: (os.path.isdir(d), str(tid) in os.listdir("/proc")))


def status():
    f = dict(l.split(":\t", 1) for l in open(d + "/status").read().splitlines() if ":\t" in l)
    return (f["Name"].strip(), int(f["Pid"]) == tid, int(f["Tgid"]) == pid,
            int(f["NStgid"]) == pid)


def stat():
    s = open(d + "/stat").read()
    return int(s.split()[0]) == tid, s[s.index("("):s.index(")") + 1]


check("status: Name, Pid is the tid, Tgid and NStgid are the process:", status)
check("stat: pid field is the tid, comm:", stat)
check("comm:", lambda: open(d + "/comm").read().strip())
check("task lists both:", lambda: sorted(os.listdir(d + "/task")) == sorted([str(pid), str(tid)]))
check("task/<tid>/comm:", lambda: open(d + "/task/%d/comm" % tid).read().strip())
check("exe is the process's:", lambda: os.readlink(d + "/exe") == os.readlink("/proc/self/exe"))
check("fd lists the same fds:",
      lambda: sorted(os.listdir(d + "/fd")) == sorted(os.listdir("/proc/%d/fd" % pid)))
done.set()
t.join()
# join returns before the kernel has necessarily reaped the thread, on Linux as here: wait for
# /proc/<tid> to go (at most 5 s) rather than compare a race
for _ in range(500):
    if not os.path.exists(d):
        break
    time.sleep(0.01)
print("after the thread exits:", os.path.exists(d))
PY
python3 tids.py
rm -f tids.py

# FIONREAD on regular files (Linux's do_vfs_ioctl, for every S_IFREG inode): the size less the
# position, negative past the end; procfs files have size 0 and sysfs attributes one page; a
# directory and a character device answer ENOTTY
python3 - <<'PY'
import array, errno, fcntl, os, termios

def fionread(fd):
    n = array.array("i", [0])
    try:
        fcntl.ioctl(fd, termios.FIONREAD, n)
        return n[0]
    except OSError as e:
        return errno.errorcode[e.errno]

with open("/tmp/fionread", "wb") as f:
    f.write(b"x" * 1000)
fd = os.open("/tmp/fionread", os.O_RDONLY)
print("regular file:", fionread(fd))
os.read(fd, 100)
print("after reading 100:", fionread(fd))
os.lseek(fd, 3000, os.SEEK_SET)
print("past the end:", fionread(fd))
os.close(fd)
os.unlink("/tmp/fionread")
fd = os.memfd_create("fionread")
os.write(fd, b"0123456789")
print("memfd at its end:", fionread(fd))
os.lseek(fd, 4, os.SEEK_SET)
print("memfd at 4:", fionread(fd))
os.close(fd)
fd = os.open("/proc/self/status", os.O_RDONLY)
os.read(fd, 10)
print("procfs file after reading 10:", fionread(fd))
os.close(fd)
fd = os.open("/sys/devices/system/cpu/online", os.O_RDONLY)
print("sysfs file:", fionread(fd))
os.close(fd)
for path in ["/tmp", "/dev/null"]:
    fd = os.open(path, os.O_RDONLY)
    print(path, fionread(fd))
    os.close(fd)
PY
