# docker-run-options: --security-opt apparmor=unconfined
# How an inet bind treats the address family, checked before anything else (Linux 6.1's inet_bind,
# ping_check_bind_addr and inet6_bind): an IPv4 socket takes AF_UNSPEC with the wildcard address
# as AF_INET and refuses it with another address (EAFNOSUPPORT), an address shorter than a
# sockaddr_in is EINVAL even when the family is wrong, a bound socket refuses a second bind
# (EINVAL), and an IPv6 socket takes only AF_INET6. Raw sockets (raw_bind, rawv6_bind): an IPv4
# one ignores the family, an IPv6 one refuses another family with EINVAL. The native side runs
# without AppArmor: under docker's default profile an AF_UNSPEC bind oopses Ubuntu's 6.8 kernel in
# aa_inet_bind_perm.
set -e
cd /tmp
cat > inetbind.c <<'C'
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

/* Binds a new socket, so that each check sees an unbound one */
static void try(const char *what, int domain, int type, int proto,
                const struct sockaddr_storage *a, socklen_t len)
{
    int fd = socket(domain, type, proto);
    int r = bind(fd, (const struct sockaddr *)a, len);
    printf("%s: %s\n", what, r == 0 ? "ok" : strerror(errno));
    close(fd);
}

static void v4(const char *name, int type, int proto, int raw)
{
    struct sockaddr_storage ss;
    struct sockaddr_in *in = (struct sockaddr_in *)&ss;
    int fd = socket(AF_INET, type, proto);
    if (fd < 0) {
        printf("%s: socket: %s\n", name, strerror(errno));
        return;
    }
    printf("== %s\n", name);
    memset(&ss, 0, sizeof ss);
    ss.ss_family = AF_UNSPEC;
    try("unspec, short", AF_INET, type, proto, &ss, 8);
    in->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    try("unspec, loopback", AF_INET, type, proto, &ss, sizeof *in);
    ss.ss_family = AF_INET6;
    in->sin_addr.s_addr = htonl(INADDR_ANY);
    try("inet6 family", AF_INET, type, proto, &ss, sizeof *in);
    ss.ss_family = AF_UNSPEC;
    int r = bind(fd, (const struct sockaddr *)&ss, sizeof ss);
    printf("unspec, any: %s\n", r == 0 ? "ok" : strerror(errno));
    struct sockaddr_in got;
    socklen_t len = sizeof got;
    if (getsockname(fd, (struct sockaddr *)&got, &len) == 0)
        printf("bound: family %d, address %s, port %s\n", got.sin_family,
               inet_ntoa(got.sin_addr), got.sin_port ? "set" : "0");
    /* A raw socket can be bound again on Linux; on Tuxport it cannot (the kernel/net map) */
    if (!raw) {
        r = bind(fd, (const struct sockaddr *)&ss, sizeof *in);
        printf("unspec again: %s\n", r == 0 ? "ok" : strerror(errno));
        ss.ss_family = AF_INET;
        r = bind(fd, (const struct sockaddr *)&ss, sizeof *in);
        printf("inet again: %s\n", r == 0 ? "ok" : strerror(errno));
    }
    close(fd);
}

static void v6(const char *name, int type, int proto)
{
    struct sockaddr_storage ss;
    int fd = socket(AF_INET6, type, proto);
    if (fd < 0) {
        printf("%s: socket: %s\n", name, strerror(errno));
        return;
    }
    close(fd);
    printf("== %s\n", name);
    memset(&ss, 0, sizeof ss);
    ss.ss_family = AF_UNSPEC;
    try("unspec, short", AF_INET6, type, proto, &ss, 8);
    try("unspec", AF_INET6, type, proto, &ss, sizeof(struct sockaddr_in6));
    ss.ss_family = AF_INET;
    try("inet family", AF_INET6, type, proto, &ss, sizeof(struct sockaddr_in6));
    ss.ss_family = AF_INET6;
    try("inet6, any", AF_INET6, type, proto, &ss, sizeof(struct sockaddr_in6));
}

int main(void)
{
    v4("udp", SOCK_DGRAM, 0, 0);
    v4("tcp", SOCK_STREAM, 0, 0);
    v4("ping", SOCK_DGRAM, IPPROTO_ICMP, 0);
    v4("raw", SOCK_RAW, IPPROTO_ICMP, 1);
    v6("udp6", SOCK_DGRAM, 0);
    v6("raw6", SOCK_RAW, IPPROTO_ICMPV6);
    return 0;
}
C
gcc -O2 -o inetbind inetbind.c
./inetbind
rm -f inetbind inetbind.c
