# docker-run-options: --privileged
# The eleventh map-bug triage's socket items: MSG_TRUNC on a TCP receive discards the data without
# touching the buffer (a NULL one included) and returns its length, with MSG_PEEK and MSG_WAITALL
# too; a blocking TCP send or write that meets a shutdown after part of its data returns that count
# without SIGPIPE, and the next call gets EPIPE and SIGPIPE; SIOCADDRT and SIOCDELRT for IPv4 and
# IPv6; accept4's error order, fd reservation, signals, addresses and IPV6_V6ONLY; and which
# sockets the DNS stub takes and the local address its UDP sockets get
set -e
cd /tmp
cat > trunc.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t pipes;

static void on_pipe(int sig)
{
    (void)sig;
    pipes++;
}

/* A connected TCP pair over loopback: *c the client, *s the accepted end */
static void pair(int *c, int *s)
{
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t al = sizeof a;
    if (bind(l, (struct sockaddr *)&a, sizeof a) || getsockname(l, (struct sockaddr *)&a, &al) ||
        listen(l, 1)) {
        perror("listener");
        exit(1);
    }
    *c = socket(AF_INET, SOCK_STREAM, 0);
    if (connect(*c, (struct sockaddr *)&a, sizeof a)) {
        perror("connect");
        exit(1);
    }
    *s = accept(l, NULL, NULL);
    close(l);
}

static void untouched(const char *what, ssize_t n, const char *buf)
{
    int same = 1;
    for (int i = 0; i < 16; i++)
        same &= buf[i] == 'x';
    if (n < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %zd, buffer untouched %d\n", what, n, same);
}

static int victim;

static void *shut_later(void *a)
{
    struct timespec t = {0, 300 * 1000000};
    nanosleep(&t, NULL);
    shutdown(victim, SHUT_WR);
    return a;
}

/* A blocking send (or write) of `len` bytes to a peer that reads nothing, shut down for writing
 * by another thread once it blocks: the count is partial and no SIGPIPE comes; the next send is
 * EPIPE with SIGPIPE */
static void broken(const char *what, int use_write)
{
    int c, s;
    pair(&c, &s);
    size_t len = 64 << 20;
    char *big = calloc(1, len);
    victim = c;
    pthread_t t;
    pthread_create(&t, NULL, shut_later, NULL);
    pipes = 0;
    ssize_t n = use_write ? write(c, big, len) : send(c, big, len, 0);
    pthread_join(t, NULL);
    printf("%s: partial %d (%s), SIGPIPE %d\n", what, n > 0 && (size_t)n < len,
           n < 0 ? strerror(errno) : "a count", (int)pipes);
    n = use_write ? write(c, "x", 1) : send(c, "x", 1, 0);
    printf("%s again: %s, SIGPIPE %d\n", what, n < 0 ? strerror(errno) : "sent", (int)pipes);
    free(big);
    close(c);
    close(s);
}

int main(void)
{
    signal(SIGPIPE, on_pipe);
    int c, s;
    pair(&c, &s);
    char buf[16], all[16];
    if (send(c, "hello world", 11, 0) != 11)
        return 1;
    /* Wait until all of it is there */
    recv(s, all, 11, MSG_PEEK | MSG_WAITALL);
    memset(buf, 'x', sizeof buf);
    untouched("trunc peek 4", recv(s, buf, 4, MSG_PEEK | MSG_TRUNC), buf);
    untouched("trunc 5", recv(s, buf, 5, MSG_TRUNC), buf);
    untouched("trunc NULL 3", recv(s, NULL, 3, MSG_TRUNC), buf);
    struct iovec iov = {buf, 1};
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1};
    untouched("recvmsg trunc 1", recvmsg(s, &m, MSG_TRUNC), buf);
    ssize_t n = recv(s, buf, sizeof buf, 0);
    printf("rest: %.*s\n", (int)n, buf);
    char block[3000];
    memset(block, 'y', sizeof block);
    send(c, block, sizeof block, 0);
    memset(buf, 'x', sizeof buf);
    untouched("trunc waitall 3000", recv(s, buf, 3000, MSG_TRUNC | MSG_WAITALL), buf);
    close(c);
    close(s);
    broken("send", 0);
    broken("write", 1);
    return 0;
}
C
gcc -O1 -pthread -o trunc trunc.c
./trunc
rm -f trunc trunc.c

# SIOCADDRT and SIOCDELRT (busybox route, and the cases route itself never sends) in a new network
# namespace with a dummy device: added and deleted routes, metrics, an address label's source,
# and the errors of rtentry_to_fib_config and fib_table_delete
cat > rt.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <net/route.h>
#include <netinet/in.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

static void in4(struct sockaddr *sa, const char *a)
{
    struct sockaddr_in *s = (struct sockaddr_in *)sa;
    memset(s, 0, sizeof *s);
    s->sin_family = AF_INET;
    inet_pton(AF_INET, a, &s->sin_addr);
}

static void try(int fd, const char *what, unsigned long req, struct rtentry *rt)
{
    int r = ioctl(fd, req, rt);
    printf("%s: %s\n", what, r ? strerror(errno) : "ok");
}

/* A route to dst/mask, through gw when given, on dev when given */
static struct rtentry route(const char *dst, const char *mask, const char *gw, char *dev, int flags)
{
    struct rtentry rt;
    memset(&rt, 0, sizeof rt);
    in4(&rt.rt_dst, dst);
    in4(&rt.rt_genmask, mask);
    if (gw)
        in4(&rt.rt_gateway, gw);
    rt.rt_dev = dev;
    rt.rt_flags = RTF_UP | flags;
    return rt;
}

int main(void)
{
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    char d0[] = "d0", label[] = "d0:1", nolabel[] = "d0:9", nosuch[] = "nosuch";
    struct rtentry rt = route("10.5.0.0", "255.255.0.0", NULL, label, 0);
    try(fd, "label source", SIOCADDRT, &rt);
    rt = route("10.5.0.0", "255.255.0.0", NULL, label, 0);
    try(fd, "same again", SIOCADDRT, &rt);
    rt = route("10.4.0.0", "255.255.0.0", NULL, nolabel, 0);
    try(fd, "unknown label", SIOCADDRT, &rt);
    rt = route("10.4.0.0", "255.255.0.0", NULL, nosuch, 0);
    try(fd, "unknown device", SIOCADDRT, &rt);
    rt = route("10.4.0.0", "255.255.0.0", NULL, (char *)8, 0);
    try(fd, "bad device pointer", SIOCADDRT, &rt);
    rt = route("10.4.0.1", "255.255.0.0", NULL, d0, 0);
    try(fd, "host bits set", SIOCADDRT, &rt);
    rt = route("10.4.0.0", "255.0.255.0", NULL, d0, 0);
    try(fd, "mask with a hole", SIOCADDRT, &rt);
    rt = route("10.4.0.0", "255.255.0.0", NULL, d0, 0);
    rt.rt_genmask.sa_family = AF_INET6;
    try(fd, "mask family", SIOCADDRT, &rt);
    rt = route("10.4.0.0", "255.255.0.0", NULL, d0, 0);
    rt.rt_dst.sa_family = AF_UNIX;
    try(fd, "destination family", SIOCADDRT, &rt);
    rt = route("10.4.0.0", "255.255.0.0", NULL, d0, RTF_GATEWAY);
    try(fd, "gateway flag without one", SIOCADDRT, &rt);
    rt = route("10.4.0.7", "0.0.0.0", "10.1.0.254", NULL, RTF_HOST | RTF_GATEWAY);
    rt.rt_metric = 8;
    try(fd, "host through a gateway, metric 8", SIOCADDRT, &rt);
    rt = route("10.3.0.0", "255.255.0.0", NULL, NULL, 0);
    try(fd, "delete a missing route", SIOCDELRT, &rt);
    rt = route("10.5.0.0", "255.255.0.0", NULL, d0, 0);
    try(fd, "delete", SIOCDELRT, &rt);
    try(fd, "bad structure", SIOCADDRT, (struct rtentry *)8);
    int u = socket(AF_UNIX, SOCK_DGRAM, 0);
    rt = route("10.4.0.0", "255.255.0.0", NULL, d0, 0);
    try(u, "AF_UNIX socket", SIOCADDRT, &rt);
    /* Without CAP_NET_ADMIN: EPERM before the structure is looked at */
    struct {
        uint32_t version;
        int pid;
    } h = {0x20080522, 0};
    struct {
        uint32_t eff, perm, inh;
    } d[2];
    syscall(SYS_capget, &h, d);
    d[0].eff &= ~(1u << 12);
    syscall(SYS_capset, &h, d);
    rt = route("10.4.0.1", "255.255.0.0", NULL, nosuch, 0);
    try(fd, "without CAP_NET_ADMIN", SIOCADDRT, &rt);
    try(fd, "without CAP_NET_ADMIN, bad structure", SIOCADDRT, (struct rtentry *)8);
    return 0;
}
C
gcc -O1 -o rt rt.c
unshare -n sh -c '
ip link add d0 type dummy
ip addr add 10.1.0.1/24 dev d0
ip addr add 10.1.0.2/24 dev d0 label d0:1
ip link set d0 up
route add -net 10.9.0.0 netmask 255.255.0.0 gw 10.1.0.254 && echo "route add -net gw"
route add -host 10.8.0.1 dev d0 && echo "route add -host dev"
route add -net 10.6.0.0 netmask 255.255.0.0 dev d0 metric 5 && echo "route add metric 5"
route add default gw 10.1.0.254 && echo "route add default"
route add -net 10.7.0.0 netmask 255.255.0.0 gw 10.2.0.1 || echo "unreachable gateway refused"
ip route show
route del -net 10.9.0.0 netmask 255.255.0.0 && echo "route del"
route del -net 10.9.0.0 netmask 255.255.0.0 || echo "deleted twice refused"
route del default && echo "route del default"
/tmp/rt
ip route show
echo 0 > /proc/sys/net/ipv6/conf/d0/accept_dad
ip -6 addr add 2001:db8:1::1/64 dev d0
route -A inet6 add 2001:db8:9::/48 gw 2001:db8:1::fe && echo "route -A inet6 add gw"
route -A inet6 add 2001:db8:8::/48 dev d0 && echo "route -A inet6 add dev"
ip -6 route show | grep 2001:db8:[89]
route -A inet6 del 2001:db8:9::/48 gw 2001:db8:1::fe && echo "route -A inet6 del"
route -A inet6 del 2001:db8:9::/48 gw 2001:db8:1::fe || echo "inet6 deleted twice refused"
ip -6 route show | grep 2001:db8:[89]
' 2>&1
rm -f rt rt.c

# accept4: EBADF before the flags, EMFILE before ENOTSOCK, the new fd reserved while waiting (a
# file opened meanwhile gets the next one, dup2 onto it is EBUSY), a signal during the wait (EINTR
# with SO_RCVTIMEO even under SA_RESTART, restarted without), a NULL addrlen with an address
# (EFAULT, the connection dropped and the fd given back, as for recvfrom), and IPV6_V6ONLY copied
# to accepted sockets in a new network namespace
cat > acc.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static void pause_ms(int ms)
{
    struct timespec t = {ms / 1000, ms % 1000 * 1000000L};
    nanosleep(&t, NULL);
}

static void report(const char *what, int r)
{
    printf("%s: %s\n", what, r < 0 ? strerror(errno) : "ok");
}

static struct sockaddr_in where;

static int listener(void)
{
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t al = sizeof a;
    bind(l, (struct sockaddr *)&a, sizeof a);
    getsockname(l, (struct sockaddr *)&a, &al);
    listen(l, 4);
    where = a;
    return l;
}

static int dial(void)
{
    int c = socket(AF_INET, SOCK_STREAM, 0);
    connect(c, (struct sockaddr *)&where, sizeof where);
    return c;
}

static int lst, during_open, during_dup, accepted;

static void *acceptor(void *a)
{
    accepted = accept4(lst, NULL, NULL, 0);
    return a;
}

static void on_usr1(int sig) { (void)sig; }

static pthread_t main_thread;

static void *poke(void *a)
{
    pause_ms(200);
    pthread_kill(main_thread, SIGUSR1);
    pause_ms(200);
    close(dial());
    return a;
}

static void v6only(void)
{
    int l = socket(AF_INET6, SOCK_STREAM, 0), on = 1;
    setsockopt(l, IPPROTO_IPV6, IPV6_V6ONLY, &on, sizeof on);
    struct sockaddr_in6 a = {.sin6_family = AF_INET6, .sin6_addr = IN6ADDR_LOOPBACK_INIT};
    socklen_t al = sizeof a;
    if (bind(l, (struct sockaddr *)&a, sizeof a) || getsockname(l, (struct sockaddr *)&a, &al) ||
        listen(l, 1)) {
        perror("v6 listener");
        return;
    }
    int c = socket(AF_INET6, SOCK_STREAM, 0);
    connect(c, (struct sockaddr *)&a, sizeof a);
    int n = accept(l, NULL, NULL), v = -1;
    socklen_t vl = sizeof v;
    getsockopt(n, IPPROTO_IPV6, IPV6_V6ONLY, &v, &vl);
    printf("accepted IPV6_V6ONLY: %d\n", v);
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc > 1) {
        v6only();
        return 0;
    }
    int p[2];
    pipe(p);
    report("bad fd, bad flags", accept4(999, NULL, NULL, 0x7));
    report("pipe, bad flags", accept4(p[0], NULL, NULL, 0x7));
    /* No free fd: EMFILE comes before ENOTSOCK */
    struct rlimit old, low = {16, 16};
    getrlimit(RLIMIT_NOFILE, &old);
    int spare[16], n = 0;
    while ((spare[n] = dup(0)) >= 0 && n < 15)
        n++;
    setrlimit(RLIMIT_NOFILE, &low);
    for (int fd = 0; fd < 16; fd++)
        if (fcntl(fd, F_GETFD) < 0)
            dup2(0, fd);
    report("pipe, no free fd", accept4(p[0], NULL, NULL, 0));
    for (int fd = 3; fd < 16; fd++)
        if (fd != p[0] && fd != p[1])
            close(fd);
    setrlimit(RLIMIT_NOFILE, &old);

    /* The fd accept4 will return is taken while it waits */
    lst = listener();
    int lowest = dup(0);
    close(lowest);
    pthread_t t;
    pthread_create(&t, NULL, acceptor, NULL);
    pause_ms(200);
    during_open = open("/dev/null", O_RDONLY);
    /* The raw call: musl's dup2 retries for as long as it gets EBUSY */
    during_dup = syscall(SYS_dup3, 0, lowest, 0);
    report("dup2 onto the waiting accept's fd", during_dup);
    int c = dial();
    pthread_join(t, NULL);
    printf("accepted at the lowest fd: %d, open meanwhile got the next: %d\n", accepted == lowest,
           during_open == lowest + 1);
    close(c);
    close(accepted);
    close(during_open);

    /* Signals during the wait */
    main_thread = pthread_self();
    struct sigaction sa = {.sa_handler = on_usr1, .sa_flags = SA_RESTART};
    sigaction(SIGUSR1, &sa, NULL);
    struct timeval tv = {5, 0};
    setsockopt(lst, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    pthread_create(&t, NULL, poke, NULL);
    int r = accept(lst, NULL, NULL);
    report("SA_RESTART, SO_RCVTIMEO", r);
    pthread_join(t, NULL);
    if (r < 0)
        close(accept(lst, NULL, NULL));
    else
        close(r);
    tv.tv_sec = 0;
    setsockopt(lst, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    pthread_create(&t, NULL, poke, NULL);
    r = accept(lst, NULL, NULL);
    report("SA_RESTART, no timeout (restarted, then connected)", r);
    pthread_join(t, NULL);
    close(r);

    /* An address without a length: EFAULT, and the connection is gone */
    c = dial();
    pause_ms(100);
    struct sockaddr_in peer;
    int before = dup(0);
    close(before);
    report("address without a length", accept4(lst, (struct sockaddr *)&peer, NULL, 0));
    int after = dup(0);
    printf("fd given back: %d\n", after == before);
    close(after);
    fcntl(lst, F_SETFL, O_NONBLOCK);
    report("connection dropped", accept4(lst, NULL, NULL, 0));
    close(c);
    int u = socket(AF_INET, SOCK_DGRAM, 0), v = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in ua = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t ual = sizeof ua;
    bind(u, (struct sockaddr *)&ua, sizeof ua);
    getsockname(u, (struct sockaddr *)&ua, &ual);
    sendto(v, "x", 1, 0, (struct sockaddr *)&ua, sizeof ua);
    char b[4];
    report("recvfrom, address without a length",
           recvfrom(u, b, sizeof b, 0, (struct sockaddr *)&peer, NULL));
    return 0;
}
C
gcc -O1 -pthread -o acc acc.c
./acc
unshare -n sh -c 'ip link set lo up; /tmp/acc v6only'
rm -f acc acc.c

# The DNS stub at 127.0.0.53:53 takes only UDP sockets: a raw ICMP socket and a ping socket get
# loopback's echo reply from that address; a UDP socket that connected to it reads back 127.0.0.1
# and a port (and /proc/net/udp shows the connection), and one that only sent to it a port
cat > stub.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static struct sockaddr_in stub = {.sin_family = AF_INET, .sin_port = 0x3500};

static void echo(const char *what, int type)
{
    int fd = socket(AF_INET, type, IPPROTO_ICMP);
    if (fd < 0) {
        printf("%s: socket %s\n", what, strerror(errno));
        return;
    }
    /* An echo request: type 8, checksum filled in below, id 0x77, sequence 1 */
    unsigned char req[8] = {8, 0, 0, 0, 0, 0x77, 0, 1};
    unsigned sum = 0;
    for (int i = 0; i < 8; i += 2)
        sum += req[i] << 8 | req[i + 1];
    sum = (sum & 0xffff) + (sum >> 16);
    req[2] = ~sum >> 8;
    req[3] = ~sum;
    if (sendto(fd, req, sizeof req, 0, (struct sockaddr *)&stub, sizeof stub) < 0) {
        printf("%s: sendto %s\n", what, strerror(errno));
        return;
    }
    /* A raw socket's messages start with the IP header, and on loopback Linux's raw socket also
     * sees the request itself, so look at up to three */
    struct pollfd p = {fd, POLLIN, 0};
    unsigned char buf[256];
    int off = type == SOCK_RAW ? 20 : 0, reply = 0;
    for (int i = 0; i < 3 && !reply && poll(&p, 1, 3000) == 1; i++) {
        ssize_t got = recv(fd, buf, sizeof buf, 0);
        reply = got >= off + 8 && buf[off] == 0;
    }
    printf("%s: echo reply %d\n", what, reply);
    close(fd);
}

static void name(const char *what, int fd)
{
    struct sockaddr_in a;
    socklen_t al = sizeof a;
    getsockname(fd, (struct sockaddr *)&a, &al);
    char ip[16];
    inet_ntop(AF_INET, &a.sin_addr, ip, sizeof ip);
    printf("%s: %s, a port %d\n", what, ip, a.sin_port != 0);
}

int main(void)
{
    inet_pton(AF_INET, "127.0.0.53", &stub.sin_addr);
    echo("raw ICMP", SOCK_RAW);
    echo("ping socket", SOCK_DGRAM);
    int c = socket(AF_INET, SOCK_DGRAM, 0);
    connect(c, (struct sockaddr *)&stub, sizeof stub);
    name("UDP connected to the stub", c);
    fflush(stdout);
    system("grep -c ' 3500007F:0035 ' /proc/net/udp");
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    static const unsigned char q[12] = {0x12, 0x34, 1, 0};
    sendto(u, q, sizeof q, 0, (struct sockaddr *)&stub, sizeof stub);
    name("UDP that sent to the stub", u);
    return 0;
}
C
gcc -O1 -o stub stub.c
./stub
rm -f stub stub.c
