/*
 * Guest test program for sockets and I/O multiplexing (M4): build with musl-gcc -static and run
 * directly in tuxport-run (no rootfs and no outside network needed). Each check that passes prints
 * a line "ok <name>"; a failure prints the reason and exits with 1; when all pass it prints
 * "all ok" at the end.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            printf("FAIL %s:%d: ", __func__, __LINE__);                                            \
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

static volatile sig_atomic_t sigpipes, alarms;

static void on_sigpipe(int sig)
{
    (void)sig;
    sigpipes++;
}

static void on_alarm(int sig)
{
    (void)sig;
    alarms++;
}

static void handle(int sig, void (*fn)(int))
{
    struct sigaction sa = {0};
    sa.sa_handler = fn;
    sigemptyset(&sa.sa_mask);
    CHECK(sigaction(sig, &sa, NULL) == 0, "sigaction %d", sig);
}

static int wait_child(pid_t pid)
{
    int status;
    CHECK(waitpid(pid, &status, 0) == pid, "waitpid");
    return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
}

/* ---- AF_UNIX socketpair ---- */

static void test_stream_pair(void)
{
    int sv[2];
    char buf[16];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "socketpair");
    CHECK(write(sv[0], "hello", 5) == 5, "write");
    CHECK(write(sv[0], "world", 5) == 5, "write");
    int avail = 0;
    CHECK(ioctl(sv[1], FIONREAD, &avail) == 0 && avail == 10, "FIONREAD %d", avail);
    CHECK(recv(sv[1], buf, 3, MSG_PEEK) == 3 && memcmp(buf, "hel", 3) == 0, "peek");
    /* A stream socket reads two writes back merged */
    CHECK(read(sv[1], buf, sizeof buf) == 10 && memcmp(buf, "helloworld", 10) == 0, "read");
    CHECK(shutdown(sv[0], SHUT_WR) == 0, "shutdown");
    CHECK(read(sv[1], buf, sizeof buf) == 0, "EOF after SHUT_WR");
    CHECK(write(sv[1], "back", 4) == 4, "other direction still open");
    CHECK(read(sv[0], buf, sizeof buf) == 4, "read back");

    /* After the peer closes: with MSG_NOSIGNAL only EPIPE is returned, otherwise there is also
     * SIGPIPE */
    close(sv[1]);
    handle(SIGPIPE, on_sigpipe);
    CHECK(send(sv[0], "x", 1, MSG_NOSIGNAL) == -1 && errno == EPIPE, "EPIPE");
    CHECK(sigpipes == 0, "no SIGPIPE with MSG_NOSIGNAL");
    CHECK(write(sv[0], "x", 1) == -1 && errno == EPIPE, "EPIPE");
    CHECK(sigpipes == 1, "SIGPIPE %d", (int)sigpipes);
    struct pollfd p = {sv[0], POLLIN | POLLOUT, 0};
    CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLHUP), "POLLHUP 0x%x", p.revents);
    close(sv[0]);
    ok("unix stream socketpair");
}

static void test_message_pairs(void)
{
    int types[] = {SOCK_DGRAM, SOCK_SEQPACKET};
    for (int i = 0; i < 2; i++) {
        int sv[2];
        char buf[16];
        CHECK(socketpair(AF_UNIX, types[i] | SOCK_CLOEXEC, 0, sv) == 0, "socketpair %d", types[i]);
        CHECK(fcntl(sv[0], F_GETFD) == FD_CLOEXEC, "SOCK_CLOEXEC");
        CHECK(send(sv[0], "first", 5, 0) == 5 && send(sv[0], "second", 6, 0) == 6, "send");
        /* Message boundaries are kept, and the part not read is discarded */
        struct iovec iov = {buf, 3};
        struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1};
        CHECK(recvmsg(sv[1], &msg, 0) == 3 && (msg.msg_flags & MSG_TRUNC), "truncated");
        CHECK(recv(sv[1], buf, sizeof buf, 0) == 6 && memcmp(buf, "second", 6) == 0, "boundary");
        int type = 0;
        socklen_t len = sizeof type;
        CHECK(getsockopt(sv[0], SOL_SOCKET, SO_TYPE, &type, &len) == 0 && type == types[i], "SO_TYPE");
        CHECK(recv(sv[1], buf, sizeof buf, MSG_DONTWAIT) == -1 && errno == EAGAIN, "EAGAIN");
        close(sv[0]);
        close(sv[1]);
    }
    ok("unix datagram and seqpacket socketpairs");
}

/* Passes a pipe's write end to a child through SCM_RIGHTS; the child writes to it once received */
static void test_scm_rights(void)
{
    int sv[2], pfd[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "socketpair");
    CHECK(pipe(pfd) == 0, "pipe");
    pid_t pid = fork();
    if (pid == 0) {
        char data[4], control[CMSG_SPACE(sizeof(int))];
        struct iovec iov = {data, sizeof data};
        struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = control,
                             .msg_controllen = sizeof control};
        if (recvmsg(sv[1], &msg, MSG_CMSG_CLOEXEC) != 2)
            _exit(2);
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
        if (!c || c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_RIGHTS)
            _exit(3);
        int fd;
        memcpy(&fd, CMSG_DATA(c), sizeof fd);
        if (fcntl(fd, F_GETFD) != FD_CLOEXEC || write(fd, "passed", 6) != 6)
            _exit(4);
        _exit(0);
    }
    char control[CMSG_SPACE(sizeof(int))] = {0};
    struct iovec iov = {"fd", 2};
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = control,
                         .msg_controllen = sizeof control};
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &pfd[1], sizeof(int));
    CHECK(sendmsg(sv[0], &msg, 0) == 2, "sendmsg");
    close(pfd[1]);
    CHECK(wait_child(pid) == 0, "child failed");
    char buf[8];
    CHECK(read(pfd[0], buf, sizeof buf) == 6 && memcmp(buf, "passed", 6) == 0, "data via passed fd");
    CHECK(read(pfd[0], buf, sizeof buf) == 0, "pipe closed everywhere");
    close(pfd[0]);
    close(sv[0]);
    close(sv[1]);
    ok("SCM_RIGHTS");
}

/* listen/connect/accept on an abstract address, and SO_PEERCRED on both sides */
static void test_unix_listen(void)
{
    struct sockaddr_un addr = {.sun_family = AF_UNIX};
    memcpy(addr.sun_path, "\0tuxport-test", 12);
    socklen_t alen = offsetof(struct sockaddr_un, sun_path) + 12;
    int srv = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    CHECK(srv >= 0, "socket");
    CHECK(bind(srv, (struct sockaddr *)&addr, alen) == 0, "bind");
    int dup = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(bind(dup, (struct sockaddr *)&addr, alen) == -1 && errno == EADDRINUSE, "EADDRINUSE");
    close(dup);
    CHECK(listen(srv, 4) == 0, "listen");
    CHECK(accept(srv, NULL, NULL) == -1 && errno == EAGAIN, "nonblocking accept");

    pid_t pid = fork();
    if (pid == 0) {
        int c = socket(AF_UNIX, SOCK_STREAM, 0);
        if (connect(c, (struct sockaddr *)&addr, alen) != 0)
            _exit(2);
        struct ucred cred;
        socklen_t len = sizeof cred;
        if (getsockopt(c, SOL_SOCKET, SO_PEERCRED, &cred, &len) != 0 || cred.pid != getppid())
            _exit(3);
        char buf[8];
        if (write(c, "ping", 4) != 4 || read(c, buf, sizeof buf) != 4 || memcmp(buf, "pong", 4))
            _exit(4);
        _exit(0);
    }
    struct pollfd p = {srv, POLLIN, 0};
    CHECK(poll(&p, 1, 5000) == 1 && (p.revents & POLLIN), "listener readable");
    int conn = accept4(srv, NULL, NULL, SOCK_CLOEXEC);
    CHECK(conn >= 0, "accept");
    struct ucred cred;
    socklen_t len = sizeof cred;
    CHECK(getsockopt(conn, SOL_SOCKET, SO_PEERCRED, &cred, &len) == 0 && cred.pid == pid,
          "peer pid %d", cred.pid);
    struct sockaddr_un name;
    len = sizeof name;
    CHECK(getsockname(conn, (struct sockaddr *)&name, &len) == 0 && len == alen &&
              memcmp(name.sun_path, addr.sun_path, 12) == 0,
          "accepted socket has the listener's name");
    char buf[8];
    CHECK(read(conn, buf, sizeof buf) == 4 && memcmp(buf, "ping", 4) == 0, "ping");
    CHECK(write(conn, "pong", 4) == 4, "pong");
    CHECK(wait_child(pid) == 0, "child failed");
    close(conn);
    close(srv);
    /* The address is released when the socket closes */
    int again = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(bind(again, (struct sockaddr *)&addr, alen) == 0, "rebind");
    close(again);
    ok("unix listen, connect and SO_PEERCRED");
}

/* ---- TCP and UDP loopback (host sockets) ---- */

static void test_tcp_loopback(void)
{
    struct sockaddr_in addr = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t len = sizeof addr;
    int srv = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(srv >= 0, "socket");
    int one = 1;
    CHECK(setsockopt(srv, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one) == 0, "SO_REUSEADDR");
    CHECK(bind(srv, (struct sockaddr *)&addr, sizeof addr) == 0, "bind");
    CHECK(getsockname(srv, (struct sockaddr *)&addr, &len) == 0 && addr.sin_port != 0, "port");
    CHECK(listen(srv, 4) == 0, "listen");

    int c = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    int rc = connect(c, (struct sockaddr *)&addr, sizeof addr);
    CHECK(rc == 0 || errno == EINPROGRESS, "connect");
    struct pollfd p = {c, POLLOUT, 0};
    CHECK(poll(&p, 1, 5000) == 1 && (p.revents & POLLOUT), "connected");
    int err = -1;
    len = sizeof err;
    CHECK(getsockopt(c, SOL_SOCKET, SO_ERROR, &err, &len) == 0 && err == 0, "SO_ERROR %d", err);
    int conn = accept(srv, NULL, NULL);
    CHECK(conn >= 0, "accept");
    fcntl(c, F_SETFL, 0);

    /* A blocking recv waits for the peer's data; a large chunk of data arrives in several parts */
    static char big[256 * 1024], got[256 * 1024];
    memset(big, 'z', sizeof big);
    pid_t pid = fork();
    if (pid == 0) {
        _exit(send(conn, big, sizeof big, 0) == (ssize_t)sizeof big ? 0 : 1);
    }
    size_t total = 0;
    while (total < sizeof got) {
        ssize_t n = recv(c, got + total, sizeof got - total, 0);
        CHECK(n > 0, "recv %zd", n);
        total += n;
    }
    CHECK(memcmp(big, got, sizeof big) == 0, "data");
    CHECK(wait_child(pid) == 0, "sender failed");
    CHECK(shutdown(conn, SHUT_WR) == 0, "shutdown");
    CHECK(recv(c, got, 1, 0) == 0, "EOF");
    close(conn);
    close(c);
    close(srv);
    ok("tcp loopback");
}

static void test_udp_loopback(void)
{
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    struct sockaddr_in from;
    socklen_t len = sizeof a;
    int s1 = socket(AF_INET, SOCK_DGRAM, 0), s2 = socket(AF_INET, SOCK_DGRAM, 0);
    CHECK(bind(s1, (struct sockaddr *)&a, sizeof a) == 0, "bind");
    CHECK(getsockname(s1, (struct sockaddr *)&a, &len) == 0, "getsockname");
    CHECK(sendto(s2, "datagram", 8, 0, (struct sockaddr *)&a, sizeof a) == 8, "sendto");
    char buf[16];
    len = sizeof from;
    CHECK(recvfrom(s1, buf, sizeof buf, 0, (struct sockaddr *)&from, &len) == 8, "recvfrom");
    CHECK(from.sin_addr.s_addr == htonl(INADDR_LOOPBACK) && from.sin_port != 0, "source address");
    close(s1);
    close(s2);
    ok("udp loopback");
}

/*
 * The replies of the DNS stub (127.0.0.53:53) are in Tuxport's own queue: FIONREAD must report the
 * length of the next reply. glibc's resolver sends both the A and the AAAA query at once with
 * sendmmsg and sizes the buffer for the second reply by FIONREAD; when that reports 0 it cannot
 * take the AAAA reply and resolution fails (Debian's apt). Without outside network the stub replies
 * SERVFAIL, still a readable reply, so this does not look at the replies' contents, only that the
 * lengths match.
 */
static void test_dns_stub_fionread(void)
{
    static const unsigned char query[] = {
        0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0,  /* header: RD, one question */
        9, 'l', 'o', 'c', 'a', 'l', 'h', 'o', 's', 't', 0, /* localhost */
        0, 1, 0, 1,                                         /* A, IN */
    };
    unsigned char q2[sizeof query];
    memcpy(q2, query, sizeof query);
    q2[0] = 0x56;
    q2[sizeof q2 - 3] = 28; /* AAAA */
    struct sockaddr_in ns = {.sin_family = AF_INET, .sin_port = htons(53)};
    inet_pton(AF_INET, "127.0.0.53", &ns.sin_addr);
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    CHECK(connect(fd, (struct sockaddr *)&ns, sizeof ns) == 0, "connect to the stub");
    struct iovec iov[2] = {{(void *)query, sizeof query}, {q2, sizeof q2}};
    struct mmsghdr msgs[2] = {{.msg_hdr = {.msg_iov = &iov[0], .msg_iovlen = 1}},
                              {.msg_hdr = {.msg_iov = &iov[1], .msg_iovlen = 1}}};
    CHECK(sendmmsg(fd, msgs, 2, 0) == 2, "sendmmsg");
    /* The queries are resolved in parallel, so the answers may come in either order */
    int seen = 0;
    for (int i = 0; i < 2; i++) {
        struct pollfd p = {fd, POLLIN, 0};
        CHECK(poll(&p, 1, 30000) == 1, "answer %d", i);
        int avail = -1;
        CHECK(ioctl(fd, FIONREAD, &avail) == 0, "FIONREAD");
        unsigned char buf[4096];
        ssize_t n = recv(fd, buf, sizeof buf, 0);
        CHECK(n >= 12 && avail == n, "answer %d: FIONREAD %d, recv %zd", i, avail, n);
        CHECK((buf[0] == 0x12 || buf[0] == 0x56) && buf[1] == 0x34, "answer %d id", i);
        seen |= buf[0] == 0x12 ? 1 : 2;
    }
    CHECK(seen == 3, "both answers");
    int avail = -1;
    CHECK(ioctl(fd, FIONREAD, &avail) == 0 && avail == 0, "FIONREAD when empty: %d", avail);
    close(fd);
    ok("dns stub fionread");
}

/*
 * The DNS stub's answers queue on the socket only up to its SO_RCVBUF, as UDP datagrams do: with
 * the smallest receive buffer, 400 queries the stub answers at once (shorter than a DNS header, so
 * they fail without the network) leave at most a buffer's worth of answers, plus the one that
 * crossed it. The socket is bound on the host by its first send, so getsockname shows a port.
 */
static void test_dns_stub_queue(void)
{
    struct sockaddr_in ns = {.sin_family = AF_INET, .sin_port = htons(53)};
    inet_pton(AF_INET, "127.0.0.53", &ns.sin_addr);
    int fd = socket(AF_INET, SOCK_DGRAM, 0), one = 1, rcvbuf = 0;
    socklen_t len = sizeof rcvbuf;
    CHECK(setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &one, sizeof one) == 0, "SO_RCVBUF");
    CHECK(getsockopt(fd, SOL_SOCKET, SO_RCVBUF, &rcvbuf, &len) == 0 && rcvbuf > 0, "SO_RCVBUF");
    static const unsigned char q[11] = {0x12, 0x34, 1, 0};
    for (int i = 0; i < 400; i++)
        CHECK(sendto(fd, q, sizeof q, 0, (struct sockaddr *)&ns, sizeof ns) == sizeof q, "send");
    struct sockaddr_in me;
    socklen_t ml = sizeof me;
    CHECK(getsockname(fd, (struct sockaddr *)&me, &ml) == 0 && me.sin_port != 0, "autobound");
    usleep(1500 * 1000);
    int count = 0, bytes = 0;
    unsigned char buf[64];
    ssize_t n;
    while ((n = recv(fd, buf, sizeof buf, MSG_DONTWAIT)) > 0) {
        count++;
        bytes += n;
    }
    CHECK(count > 0 && count < 400 && bytes <= rcvbuf + (int)sizeof q,
          "%d answers, %d bytes, SO_RCVBUF %d", count, bytes, rcvbuf);
    close(fd);
    ok("dns stub queue");
}

/* getifaddrs lists interfaces and addresses through NETLINK_ROUTE */
static void test_netlink(void)
{
    struct ifaddrs *list;
    CHECK(getifaddrs(&list) == 0, "getifaddrs");
    int found = 0;
    for (struct ifaddrs *i = list; i; i = i->ifa_next) {
        if (strcmp(i->ifa_name, "lo") == 0 && i->ifa_addr && i->ifa_addr->sa_family == AF_INET &&
            ((struct sockaddr_in *)i->ifa_addr)->sin_addr.s_addr == htonl(INADDR_LOOPBACK) &&
            (i->ifa_flags & IFF_LOOPBACK))
            found = 1;
    }
    freeifaddrs(list);
    CHECK(found, "lo with 127.0.0.1 not found");
    CHECK(if_nametoindex("lo") > 0, "if_nametoindex");
    ok("netlink interfaces");

    /* Unimplemented protocols cannot be created, like subsystems not compiled into the kernel:
     * from this libaudit takes auditing to be absent and does not wait for an ACK that will never
     * come. sock_diag refuses every query as without its family modules (ENOENT), so ss falls
     * back to reading /proc/net quietly. uevent only listens, so it can be created; generic has
     * one family, nlctrl (the replies' contents are compared by the differential test compat) */
    CHECK(socket(AF_NETLINK, SOCK_RAW, 9 /* NETLINK_AUDIT */) < 0 && errno == EPROTONOSUPPORT,
          "audit netlink socket");
    int sd = socket(AF_NETLINK, SOCK_RAW, 4 /* NETLINK_SOCK_DIAG */);
    CHECK(sd >= 0, "sock_diag netlink socket");
    /* struct nlmsghdr (musl-gcc has no kernel headers), then a sock_diag_req for AF_UNIX; the
     * answer is an NLMSG_ERROR (2) whose payload starts with the negative errno */
    struct {
        uint32_t len;
        uint16_t type, flags;
        uint32_t seq, pid;
        unsigned char family, protocol;
    } q = {sizeof q, 20 /* SOCK_DIAG_BY_FAMILY */, 1 | 0x300 /* NLM_F_REQUEST | NLM_F_DUMP */,
           1, 0, AF_UNIX, 0};
    CHECK(send(sd, &q, sizeof q, 0) == (ssize_t)sizeof q, "sock_diag send");
    union {
        char b[256];
        uint32_t w[64];
    } ans;
    ssize_t n = recv(sd, ans.b, sizeof ans, 0);
    CHECK(n >= 20 && (ans.w[1] & 0xffff) == 2 && (int32_t)ans.w[4] == -ENOENT,
          "sock_diag query refused with ENOENT");
    close(sd);
    int ue = socket(AF_NETLINK, SOCK_DGRAM, 15 /* NETLINK_KOBJECT_UEVENT */);
    CHECK(ue >= 0, "uevent netlink socket");
    close(ue);
    int ge = socket(AF_NETLINK, SOCK_RAW, 16 /* NETLINK_GENERIC */);
    CHECK(ge >= 0, "generic netlink socket");
    close(ge);
    ok("netlink protocols");
}

/* ---- epoll, eventfd, select ---- */

static void test_epoll(void)
{
    int ep = epoll_create1(EPOLL_CLOEXEC);
    int ev = eventfd(0, EFD_NONBLOCK);
    int sv[2];
    CHECK(ep >= 0 && ev >= 0 && socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "create");
    struct epoll_event e = {.events = EPOLLIN, .data.u32 = 1};
    CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, ev, &e) == 0, "add eventfd");
    e.data.u32 = 2;
    CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, sv[1], &e) == 0, "add socket");
    struct epoll_event out[4];
    CHECK(epoll_wait(ep, out, 4, 0) == 0, "nothing ready");

    /* Another process writes the socket: epoll_wait is woken */
    pid_t pid = fork();
    if (pid == 0) {
        usleep(20000);
        _exit(write(sv[0], "x", 1) == 1 ? 0 : 1);
    }
    CHECK(epoll_wait(ep, out, 4, 5000) == 1 && out[0].data.u32 == 2, "socket readable");
    CHECK(wait_child(pid) == 0, "writer failed");
    uint64_t one = 1;
    CHECK(write(ev, &one, 8) == 8, "eventfd write");
    CHECK(epoll_wait(ep, out, 4, 0) == 2, "both ready");
    uint64_t v;
    CHECK(read(ev, &v, 8) == 8 && v == 1, "eventfd read");
    CHECK(read(ev, &v, 8) == -1 && errno == EAGAIN, "eventfd empty");

    fd_set rd;
    FD_ZERO(&rd);
    FD_SET(sv[1], &rd);
    struct timeval tv = {0, 0};
    CHECK(select(sv[1] + 1, &rd, NULL, NULL, &tv) == 1 && FD_ISSET(sv[1], &rd), "select");
    close(ep);
    close(ev);
    close(sv[0]);
    close(sv[1]);
    ok("epoll, eventfd and select");
}

/* ---- Interval timers ---- */

static void test_itimer(void)
{
    handle(SIGALRM, on_alarm);
    struct itimerval t = {{0, 20000}, {0, 20000}}, cur;
    CHECK(setitimer(ITIMER_REAL, &t, NULL) == 0, "setitimer");
    CHECK(getitimer(ITIMER_REAL, &cur) == 0 && cur.it_interval.tv_usec == 20000, "getitimer");
    sigset_t none;
    sigemptyset(&none);
    while (alarms < 3)
        sigsuspend(&none);
    struct itimerval off = {0};
    CHECK(setitimer(ITIMER_REAL, &off, &cur) == 0 && cur.it_interval.tv_usec == 20000, "disarm");
    CHECK(getitimer(ITIMER_REAL, &cur) == 0 && cur.it_value.tv_sec == 0 && cur.it_value.tv_usec == 0,
          "disarmed");
    /* alarm(2) is implemented through setitimer */
    alarms = 0;
    CHECK(alarm(1) == 0, "alarm");
    CHECK(alarm(0) >= 1, "alarm remaining");
    ok("interval timers");
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    test_stream_pair();
    test_message_pairs();
    test_scm_rights();
    test_unix_listen();
    test_tcp_loopback();
    test_udp_loopback();
    test_dns_stub_fionread();
    test_dns_stub_queue();
    test_netlink();
    test_epoll();
    test_itimer();
    printf("all ok\n");
    return 0;
}
