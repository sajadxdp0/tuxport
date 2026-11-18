/*
 * Traffic counters of network interfaces and protocols (core/kernel/src/traffic.rs): after sending
 * and receiving on the loopback, the lo counters in three places, /proc/net/dev,
 * /sys/class/net/lo/statistics and getifaddrs' ifa_data (netlink's IFLA_STATS), are equal, the
 * bytes added are exactly the payload sent and received, and peeking (MSG_PEEK) does not count;
 * the IP packet and byte counts in /proc/net/snmp and netstat equal lo's, and TCP connections, UDP
 * datagrams and ping echoes are counted correctly too. The packet and byte counts hold only in
 * Tuxport: Linux also counts protocol headers and ACKs. Prints "all ok" when it passes; a failure
 * prints the reason and exits with 1.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <ifaddrs.h>
#include <netinet/in.h>
#include <netinet/ip_icmp.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
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

struct counts {
    unsigned long long rx_bytes, rx_packets, tx_bytes, tx_packets;
};

static struct counts from_proc(void)
{
    FILE *f = fopen("/proc/net/dev", "r");
    CHECK(f, "open /proc/net/dev");
    char line[512];
    struct counts c;
    int found = 0;
    while (fgets(line, sizeof line, f)) {
        char *colon = strchr(line, ':');
        if (!colon)
            continue;
        *colon = 0;
        char *name = line + strspn(line, " ");
        if (strcmp(name, "lo") != 0)
            continue;
        unsigned long long v[16];
        CHECK(sscanf(colon + 1, "%llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu "
                                "%llu %llu",
                     &v[0], &v[1], &v[2], &v[3], &v[4], &v[5], &v[6], &v[7], &v[8], &v[9], &v[10],
                     &v[11], &v[12], &v[13], &v[14], &v[15]) == 16,
              "the lo line has 16 counters");
        c = (struct counts){v[0], v[1], v[8], v[9]};
        found = 1;
    }
    fclose(f);
    CHECK(found, "no lo in /proc/net/dev");
    return c;
}

static unsigned long long sys_value(const char *file)
{
    char path[128];
    snprintf(path, sizeof path, "/sys/class/net/lo/statistics/%s", file);
    FILE *f = fopen(path, "r");
    CHECK(f, "open %s", path);
    unsigned long long v;
    CHECK(fscanf(f, "%llu", &v) == 1, "read %s", path);
    fclose(f);
    return v;
}

static struct counts from_sys(void)
{
    return (struct counts){sys_value("rx_bytes"), sys_value("rx_packets"), sys_value("tx_bytes"),
                           sys_value("tx_packets")};
}

/* The first four fields of struct rtnl_link_stats */
struct link_stats {
    uint32_t rx_packets, tx_packets, rx_bytes, tx_bytes;
};

static struct counts from_ifaddrs(void)
{
    struct ifaddrs *list;
    CHECK(getifaddrs(&list) == 0, "getifaddrs");
    struct counts c;
    int found = 0;
    for (struct ifaddrs *i = list; i; i = i->ifa_next) {
        if (strcmp(i->ifa_name, "lo") != 0 || !i->ifa_addr || i->ifa_addr->sa_family != AF_PACKET)
            continue;
        CHECK(i->ifa_data, "lo has no ifa_data");
        struct link_stats *s = i->ifa_data;
        c = (struct counts){s->rx_bytes, s->rx_packets, s->tx_bytes, s->tx_packets};
        found = 1;
    }
    freeifaddrs(list);
    CHECK(found, "no AF_PACKET entry for lo");
    return c;
}

/* The counts in the three places are equal */
static struct counts counters(void)
{
    struct counts p = from_proc(), s = from_sys(), a = from_ifaddrs();
    CHECK(memcmp(&p, &s, sizeof p) == 0 && memcmp(&p, &a, sizeof p) == 0,
          "/proc/net/dev %llu/%llu %llu/%llu, sysfs %llu/%llu %llu/%llu, ifaddrs %llu/%llu %llu/%llu",
          p.rx_bytes, p.rx_packets, p.tx_bytes, p.tx_packets, s.rx_bytes, s.rx_packets, s.tx_bytes,
          s.tx_packets, a.rx_bytes, a.rx_packets, a.tx_bytes, a.tx_packets);
    return p;
}

/*
 * The field `field` of group `group` in /proc/net/snmp or netstat. Each group is a line of field
 * names and a line of values, with the same group name and the same count; returns -1 when there
 * is no such entry (IcmpMsg lists only the types that are not 0).
 */
static long long snmp(const char *file, const char *group, const char *field)
{
    char path[64];
    snprintf(path, sizeof path, "/proc/net/%s", file);
    FILE *f = fopen(path, "r");
    CHECK(f, "open %s", path);
    static char names[8192], values[8192];
    long long found = -1;
    while (fgets(names, sizeof names, f)) {
        CHECK(fgets(values, sizeof values, f), "%s: no values after %s", path, names);
        char *n = strchr(names, ':'), *v = strchr(values, ':');
        CHECK(n && v && n - names == v - values && memcmp(names, values, n - names) == 0,
              "%s: names %s and values %s", path, names, values);
        *n++ = 0;
        v++;
        int same = strcmp(names, group) == 0;
        char *ns, *vs;
        char *name = strtok_r(n, " \n", &ns), *value = strtok_r(v, " \n", &vs);
        for (; name && value; name = strtok_r(NULL, " \n", &ns), value = strtok_r(NULL, " \n", &vs))
            if (same && strcmp(name, field) == 0)
                found = atoll(value);
        CHECK(!name && !value, "%s: %s has as many names as values", path, names);
    }
    fclose(f);
    return found;
}

static void read_line(const char *path, const char *want)
{
    FILE *f = fopen(path, "r");
    CHECK(f, "open %s", path);
    char got[64] = "";
    CHECK(fgets(got, sizeof got, f), "read %s", path);
    fclose(f);
    CHECK(strcmp(got, want) == 0, "%s is %s", path, got);
}

#define STREAM_BYTES 100000
#define DATAGRAMS 10
#define DATAGRAM_BYTES 100
#define UNREAD_BYTES 50

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    DIR *d = opendir("/sys/class/net");
    CHECK(d, "opendir /sys/class/net");
    int listed = 0;
    for (struct dirent *e; (e = readdir(d));)
        listed |= strcmp(e->d_name, "lo") == 0;
    closedir(d);
    CHECK(listed, "lo is not listed in /sys/class/net");
    read_line("/sys/class/net/lo/type", "772\n");
    read_line("/sys/class/net/lo/operstate", "unknown\n");
    read_line("/sys/class/net/lo/statistics/rx_errors", "0\n");
    printf("ok sysfs\n");

    struct counts before = counters();
    long long opens = snmp("snmp", "Tcp", "ActiveOpens"),
              passive = snmp("snmp", "Tcp", "PassiveOpens"),
              estab = snmp("snmp", "Tcp", "CurrEstab"), in_segs = snmp("snmp", "Tcp", "InSegs"),
              out_segs = snmp("snmp", "Tcp", "OutSegs"),
              udp_in = snmp("snmp", "Udp", "InDatagrams"),
              udp_out = snmp("snmp", "Udp", "OutDatagrams"),
              ip_in = snmp("snmp", "Ip", "InReceives"), ip_out = snmp("snmp", "Ip", "OutRequests"),
              octets_in = snmp("netstat", "IpExt", "InOctets"),
              octets_out = snmp("netstat", "IpExt", "OutOctets");
    CHECK(snmp("snmp", "Ip", "DefaultTTL") == 64 && snmp("snmp", "Tcp", "RtoMax") == 120000 &&
              snmp("snmp", "Tcp", "MaxConn") == -1 && snmp("snmp", "UdpLite", "InDatagrams") == 0,
          "the constant fields of /proc/net/snmp");
    CHECK(snmp("netstat", "TcpExt", "TCPMigrateReqFailure") == 0 &&
              snmp("netstat", "MPTcpExt", "RcvWndConflict") == 0,
          "the last fields of TcpExt and MPTcpExt");

    /* A TCP connection on the loopback */
    struct sockaddr_in addr = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t len = sizeof addr;
    int l = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(l >= 0 && bind(l, (void *)&addr, len) == 0 && listen(l, 1) == 0, "listen");
    CHECK(getsockname(l, (void *)&addr, &len) == 0, "getsockname");
    int c = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(c >= 0 && connect(c, (void *)&addr, len) == 0, "connect");
    int s = accept(l, NULL, NULL);
    CHECK(s >= 0, "accept");
    CHECK(snmp("snmp", "Tcp", "ActiveOpens") == opens + 1 &&
              snmp("snmp", "Tcp", "PassiveOpens") == passive + 1 &&
              snmp("snmp", "Tcp", "CurrEstab") == estab + 2,
          "connections: %lld active, %lld passive, %lld established",
          snmp("snmp", "Tcp", "ActiveOpens") - opens, snmp("snmp", "Tcp", "PassiveOpens") - passive,
          snmp("snmp", "Tcp", "CurrEstab") - estab);
    static char buf[STREAM_BYTES];
    memset(buf, 'x', sizeof buf);
    if (fork() == 0) {
        size_t sent = 0;
        while (sent < sizeof buf) {
            ssize_t n = write(c, buf + sent, sizeof buf - sent);
            if (n <= 0)
                _exit(1);
            sent += n;
        }
        _exit(0);
    }
    close(c);
    size_t got = 0;
    for (ssize_t n; (n = read(s, buf, sizeof buf)) > 0;)
        got += n;
    CHECK(got == STREAM_BYTES, "received %zu bytes", got);
    close(s);
    close(l);

    /* UDP without connect: the interface is determined by the peer; peek once, which does not
     * count */
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    addr.sin_port = 0;
    len = sizeof addr;
    CHECK(u >= 0 && bind(u, (void *)&addr, len) == 0 && getsockname(u, (void *)&addr, &len) == 0,
          "bind udp");
    int v = socket(AF_INET, SOCK_DGRAM, 0);
    for (int i = 0; i < DATAGRAMS; i++)
        CHECK(sendto(v, buf, DATAGRAM_BYTES, 0, (void *)&addr, len) == DATAGRAM_BYTES, "sendto");
    CHECK(recv(u, buf, sizeof buf, MSG_PEEK) == DATAGRAM_BYTES, "peek");
    for (int i = 0; i < DATAGRAMS; i++)
        CHECK(recv(u, buf, sizeof buf, 0) == DATAGRAM_BYTES, "recv");
    /* Sent but never read: counted only as sent, so the counts of the two directions differ */
    CHECK(sendto(v, buf, UNREAD_BYTES, 0, (void *)&addr, len) == UNREAD_BYTES, "sendto");
    close(u);
    close(v);

    struct counts after = counters();
    unsigned long long bytes = STREAM_BYTES + DATAGRAMS * DATAGRAM_BYTES;
    CHECK(after.tx_bytes - before.tx_bytes == bytes + UNREAD_BYTES &&
              after.rx_bytes - before.rx_bytes == bytes,
          "bytes: sent %llu, received %llu", after.tx_bytes - before.tx_bytes,
          after.rx_bytes - before.rx_bytes);
    /* At least 2 for TCP (the loopback MSS is about 64 KB), 1 per datagram */
    CHECK(after.tx_packets - before.tx_packets >= DATAGRAMS + 3 &&
              after.rx_packets - before.rx_packets >= DATAGRAMS + 2,
          "packets: sent %llu, received %llu", after.tx_packets - before.tx_packets,
          after.rx_packets - before.rx_packets);
    printf("ok counters\n");

    /* IP packet and byte counts equal lo's (all IPv4); TCP segments are the rest after the
     * datagrams, peeked datagrams do not count, and unread ones count only as sent */
    long long rx = after.rx_packets - before.rx_packets, tx = after.tx_packets - before.tx_packets;
    CHECK(snmp("snmp", "Ip", "InReceives") - ip_in == rx &&
              snmp("snmp", "Ip", "OutRequests") - ip_out == tx &&
              snmp("netstat", "IpExt", "InOctets") - octets_in == (long long)bytes &&
              snmp("netstat", "IpExt", "OutOctets") - octets_out == (long long)bytes + UNREAD_BYTES,
          "ip: %lld in, %lld out, %lld octets in, %lld octets out",
          snmp("snmp", "Ip", "InReceives") - ip_in, snmp("snmp", "Ip", "OutRequests") - ip_out,
          snmp("netstat", "IpExt", "InOctets") - octets_in,
          snmp("netstat", "IpExt", "OutOctets") - octets_out);
    CHECK(snmp("snmp", "Tcp", "InSegs") - in_segs == rx - DATAGRAMS &&
              snmp("snmp", "Tcp", "OutSegs") - out_segs == tx - DATAGRAMS - 1 &&
              snmp("snmp", "Udp", "InDatagrams") - udp_in == DATAGRAMS &&
              snmp("snmp", "Udp", "OutDatagrams") - udp_out == DATAGRAMS + 1 &&
              snmp("snmp", "Tcp", "CurrEstab") == estab,
          "tcp: %lld segments in, %lld out, %lld established; udp: %lld in, %lld out",
          snmp("snmp", "Tcp", "InSegs") - in_segs, snmp("snmp", "Tcp", "OutSegs") - out_segs,
          snmp("snmp", "Tcp", "CurrEstab") - estab, snmp("snmp", "Udp", "InDatagrams") - udp_in,
          snmp("snmp", "Udp", "OutDatagrams") - udp_out);

    /* Connection refused: a blocking connect gets ECONNREFUSED directly, a non-blocking one learns
     * it from SO_ERROR, each counted once */
    long long fails = snmp("snmp", "Tcp", "AttemptFails");
    opens = snmp("snmp", "Tcp", "ActiveOpens");
    l = socket(AF_INET, SOCK_STREAM, 0);
    addr.sin_port = 0;
    len = sizeof addr;
    CHECK(l >= 0 && bind(l, (void *)&addr, len) == 0 && getsockname(l, (void *)&addr, &len) == 0,
          "bind a port nobody listens on");
    c = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(connect(c, (void *)&addr, len) < 0 && errno == ECONNREFUSED, "blocking connect");
    close(c);
    c = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    if (connect(c, (void *)&addr, len) < 0 && errno == EINPROGRESS) {
        struct pollfd p = {.fd = c, .events = POLLOUT};
        CHECK(poll(&p, 1, 5000) == 1, "poll the refused connection");
        int err = 0;
        socklen_t elen = sizeof err;
        CHECK(getsockopt(c, SOL_SOCKET, SO_ERROR, &err, &elen) == 0 && err == ECONNREFUSED,
              "SO_ERROR is %d", err);
        CHECK(getsockopt(c, SOL_SOCKET, SO_ERROR, &err, &elen) == 0 && err == 0,
              "SO_ERROR is cleared");
    } else {
        CHECK(errno == ECONNREFUSED, "non-blocking connect");
    }
    close(c);
    close(l);
    CHECK(snmp("snmp", "Tcp", "ActiveOpens") - opens == 2 &&
              snmp("snmp", "Tcp", "AttemptFails") - fails == 2,
          "refused: %lld active, %lld failed", snmp("snmp", "Tcp", "ActiveOpens") - opens,
          snmp("snmp", "Tcp", "AttemptFails") - fails);

    /* Ping socket echoes: skipped when the host does not allow creating a ping socket */
    int ping = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    if (ping >= 0) {
        long long echos = snmp("snmp", "Icmp", "OutEchos"),
                  replies = snmp("snmp", "Icmp", "InEchoReps");
        struct icmphdr echo = {.type = ICMP_ECHO, .un.echo.sequence = htons(1)};
        addr.sin_port = 0;
        CHECK(sendto(ping, &echo, sizeof echo, 0, (void *)&addr, sizeof addr) == sizeof echo,
              "send an echo request");
        struct pollfd p = {.fd = ping, .events = POLLIN};
        CHECK(poll(&p, 1, 5000) == 1 && recv(ping, buf, sizeof buf, 0) == sizeof echo,
              "receive the echo reply");
        close(ping);
        CHECK(snmp("snmp", "Icmp", "OutEchos") - echos == 1 &&
                  snmp("snmp", "Icmp", "InEchoReps") - replies == 1 &&
                  snmp("snmp", "Icmp", "OutMsgs") >= 1 &&
                  snmp("snmp", "IcmpMsg", "OutType8") >= 1 &&
                  snmp("snmp", "IcmpMsg", "InType0") >= 1,
              "icmp: %lld echoes, %lld replies", snmp("snmp", "Icmp", "OutEchos") - echos,
              snmp("snmp", "Icmp", "InEchoReps") - replies);
    } else {
        printf("skip icmp (errno %d)\n", errno);
    }
    printf("ok snmp\n");
    printf("all ok\n");
    return 0;
}
