# Ports below 1024 (docs/rounds/156-low-ports.md): Tuxport binds them to other ports on the host,
# and on the guest side this must be indistinguishable from native (root in the container binds
# them directly): getsockname, getpeername, the peer from accept, the source from recvfrom, the
# ports in /proc/net; connecting to these local ports (loopback, IPv6 dual stack, wildcard
# address); using a low port as the source port; TCP and UDP on the same port, SO_REUSEPORT;
# conflicts are EADDRINUSE as usual, and so is TIME_WAIT after a connection is closed (but not
# with SO_REUSEADDR); ECONNREFUSED when no program is listening; after the listener is closed,
# established connections still report the original port. Ordinary users binding low ports is not
# compared: in docker containers ip_unprivileged_port_start is 0
# In the container the host allows binding the original ports (208-low-ports-direct.md), so the
# port conversion can only be tested by always mapping
# tuxport-options: --always-map-low-ports
set -e
cd /tmp
cat > lowports.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static int port_of(const struct sockaddr_storage *a)
{
    return a->ss_family == AF_INET ? ntohs(((struct sockaddr_in *)a)->sin_port)
                                   : ntohs(((struct sockaddr_in6 *)a)->sin6_port);
}

static void names(const char *what, int fd)
{
    struct sockaddr_storage a;
    socklen_t n = sizeof a;
    int local = getsockname(fd, (void *)&a, &n) ? -1 : port_of(&a);
    n = sizeof a;
    int peer = getpeername(fd, (void *)&a, &n) ? -errno : port_of(&a);
    /* Ephemeral ports differ every time; only report whether they are low ports */
    printf("%s: local %d, peer %d\n", what, local < 1024 ? local : 1024,
           peer < 0 ? peer : peer < 1024 ? peer : 1024);
}

static struct sockaddr_in v4(const char *ip, int port)
{
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
    inet_pton(AF_INET, ip, &a.sin_addr);
    return a;
}

static int server(int type, const char *ip, int port, int reuse)
{
    int s = socket(AF_INET, type, 0), one = 1;
    if (reuse)
        setsockopt(s, SOL_SOCKET, SO_REUSEPORT, &one, sizeof one);
    if (reuse != 2)
        setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a = v4(ip, port);
    if (bind(s, (void *)&a, sizeof a)) {
        printf("bind %s:%d: %s\n", ip, port, strerror(errno));
        close(s);
        return -1;
    }
    if (type == SOCK_STREAM)
        listen(s, 8);
    return s;
}

/* Lines whose local end is this port: TCP counts only listeners (Tuxport does not list connections
 * not yet accepted), UDP counts all */
static void proc_net(const char *file, int port, int tcp)
{
    char line[512], want[16];
    snprintf(want, sizeof want, ":%04X ", port);
    FILE *f = fopen(file, "r");
    int n = 0;
    while (fgets(line, sizeof line, f)) {
        char *p = strstr(line, want);
        if (p && p < line + 60 && (!tcp || strstr(line, " 0A ")))
            n++;
    }
    fclose(f);
    printf("%s lines with local port %d: %d\n", file, port, n);
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    /* TCP: listen on the wildcard address, connect through loopback and through IPv6's
     * ::ffff:127.0.0.1 */
    int l = server(SOCK_STREAM, "0.0.0.0", 79, 0);
    names("listener", l);
    proc_net("/proc/net/tcp", 79, 1);
    struct sockaddr_in to = v4("127.0.0.1", 79);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    printf("connect 127.0.0.1:79: %d\n", connect(c, (void *)&to, sizeof to));
    names("client", c);
    struct sockaddr_storage peer;
    socklen_t pn = sizeof peer;
    int a = accept(l, (void *)&peer, &pn);
    printf("accepted peer is low: %d\n", port_of(&peer) < 1024);
    names("accepted", a);
    write(c, "ping", 4);
    char buf[16] = {0};
    read(a, buf, 4);
    printf("server read %s\n", buf);
    close(l);
    names("accepted after the listener closed", a);
    close(a);
    close(c);

    int l6 = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 any6 = {.sin6_family = AF_INET6, .sin6_port = htons(113)};
    printf("bind [::]:113: %d\n", bind(l6, (void *)&any6, sizeof any6));
    listen(l6, 8);
    to = v4("127.0.0.1", 113);
    c = socket(AF_INET, SOCK_STREAM, 0);
    printf("ipv4 connect to the dual-stack listener: %d\n", connect(c, (void *)&to, sizeof to));
    names("client of [::]:113", c);
    struct sockaddr_in6 to6 = {.sin6_family = AF_INET6, .sin6_port = htons(113)};
    inet_pton(AF_INET6, "::1", &to6.sin6_addr);
    int c6 = socket(AF_INET6, SOCK_STREAM, 0);
    printf("connect [::1]:113: %d\n", connect(c6, (void *)&to6, sizeof to6));
    names("ipv6 client", c6);
    proc_net("/proc/net/tcp6", 113, 1);
    close(c6);
    close(c);
    close(l6);

    /* A low port as the source port: the peer the server accepts is 513 */
    l = server(SOCK_STREAM, "127.0.0.1", 79, 0);
    c = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in src = v4("127.0.0.1", 513);
    printf("bind source 513: %d\n", bind(c, (void *)&src, sizeof src));
    to = v4("127.0.0.1", 79);
    connect(c, (void *)&to, sizeof to);
    pn = sizeof peer;
    a = accept(l, (void *)&peer, &pn);
    printf("accepted peer port %d\n", port_of(&peer));
    names("accepted from 513", a);
    close(a);
    close(c);
    close(l);

    /* Conflicts, no listener */
    l = server(SOCK_STREAM, "127.0.0.1", 79, 0);
    int dup = server(SOCK_STREAM, "127.0.0.1", 79, 0);
    printf("second bind while listening: %s\n", dup < 0 ? "failed" : "ok");
    close(l);
    c = socket(AF_INET, SOCK_STREAM, 0);
    to = v4("127.0.0.1", 79);
    printf("connect with nobody listening: %d %s\n", connect(c, (void *)&to, sizeof to),
           strerror(errno));
    close(c);

    /* TIME_WAIT: the server closes the connection first, and the port stays in TIME_WAIT */
    l = server(SOCK_STREAM, "127.0.0.1", 81, 0);
    c = socket(AF_INET, SOCK_STREAM, 0);
    to = v4("127.0.0.1", 81);
    connect(c, (void *)&to, sizeof to);
    a = accept(l, NULL, NULL);
    close(a);
    close(l);
    usleep(100000);
    close(c);
    l = server(SOCK_STREAM, "127.0.0.1", 81, 2);
    printf("rebind in TIME_WAIT without SO_REUSEADDR: %s\n", l < 0 ? "failed" : "ok");
    if (l >= 0)
        close(l);
    l = server(SOCK_STREAM, "127.0.0.1", 81, 0);
    printf("rebind in TIME_WAIT with SO_REUSEADDR: %s\n", l < 0 ? "failed" : "ok");
    names("rebound", l);
    close(l);

    /* UDP: TCP and UDP on the same port; SO_REUSEPORT; source ports; connected UDP */
    int t53 = server(SOCK_STREAM, "127.0.0.1", 53, 0);
    int u1 = server(SOCK_DGRAM, "127.0.0.1", 53, 1), u2 = server(SOCK_DGRAM, "127.0.0.1", 53, 1);
    printf("tcp and two reuseport udp on 53: %d %d %d\n", t53 >= 0, u1 >= 0, u2 >= 0);
    proc_net("/proc/net/udp", 53, 0);
    close(u2);
    int uc = socket(AF_INET, SOCK_DGRAM, 0);
    to = v4("127.0.0.1", 53);
    sendto(uc, "q", 1, 0, (void *)&to, sizeof to);
    pn = sizeof peer;
    recvfrom(u1, buf, sizeof buf, 0, (void *)&peer, &pn);
    printf("udp server got it, peer low %d\n", port_of(&peer) < 1024);
    sendto(u1, "a", 1, 0, (void *)&peer, pn);
    pn = sizeof peer;
    recvfrom(uc, buf, sizeof buf, 0, (void *)&peer, &pn);
    printf("udp reply from port %d\n", port_of(&peer));
    printf("connect udp: %d\n", connect(uc, (void *)&to, sizeof to));
    names("connected udp", uc);
    send(uc, "r", 1, 0);
    recv(u1, buf, sizeof buf, 0);
    printf("connected send arrived\n");
    close(uc);
    close(u1);
    close(t53);
    return 0;
}
C
gcc -O1 -o lowports lowports.c && ./lowports
