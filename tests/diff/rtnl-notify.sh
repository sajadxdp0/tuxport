# docker-run-options: --privileged
# rtnetlink notifications in a new network namespace: sockets that joined the link, address and
# route groups receive RTM_NEW*/RTM_DEL* for what ip and interface ioctls (ifconfig) change, with
# the group in the source address and NETLINK_PKTINFO; a replaced route is only notified as new,
# a socket only gets the groups it joined, one in another namespace gets nothing, and a socket
# whose queue overflowed reads ENOBUFS once (not with NETLINK_NO_ENOBUFS).
#
# - Each step prints the notifications it caused sorted and without repeats: Linux makes some of
#   them later (IPv6 addresses from a work queue) and some more than once, and the order of the
#   routes an address brings differs. Links are never brought up (linkwatch notifies up to a
#   second late).
# - Not compared: the sequence number and port (Linux copies the request's into address and route
#   notifications), and how many notifications fit in a queue (Linux counts bytes).
set -e
cd /tmp
cat > rtmon.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <net/if.h>
#include <poll.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

/* From linux/netlink.h and linux/rtnetlink.h (the image has no kernel headers) */
#define NETLINK_ROUTE 0
#ifndef SOL_NETLINK
#define SOL_NETLINK 270
#endif
#define NETLINK_PKTINFO 3
#define NETLINK_NO_ENOBUFS 5
#define RTMGRP_LINK 0x1
#define RTMGRP_IPV4_IFADDR 0x10
#define RTMGRP_IPV4_ROUTE 0x40
#define RTMGRP_IPV6_IFADDR 0x100
#define RTMGRP_IPV6_ROUTE 0x400
#define RTM_NEWLINK 16
#define RTM_DELLINK 17
#define RTM_NEWADDR 20
#define RTM_DELADDR 21
#define RTM_NEWROUTE 24
#define RTM_DELROUTE 25
#define IFLA_IFNAME 3
#define IFLA_MTU 4
#define IFA_ADDRESS 1
#define RTA_DST 1
#define RTA_OIF 4
#define RTA_TABLE 15
struct sockaddr_nl {
    sa_family_t nl_family;
    unsigned short nl_pad;
    uint32_t nl_pid, nl_groups;
};
struct nlmsghdr {
    uint32_t nlmsg_len;
    uint16_t nlmsg_type, nlmsg_flags;
    uint32_t nlmsg_seq, nlmsg_pid;
};
struct rtattr {
    uint16_t rta_len, rta_type;
};
struct ifinfomsg {
    unsigned char ifi_family, ifi_pad;
    unsigned short ifi_type;
    int ifi_index;
    unsigned ifi_flags, ifi_change;
};
struct ifaddrmsg {
    uint8_t ifa_family, ifa_prefixlen, ifa_flags, ifa_scope;
    uint32_t ifa_index;
};
struct rtmsg {
    unsigned char rtm_family, rtm_dst_len, rtm_src_len, rtm_tos, rtm_table, rtm_protocol,
        rtm_scope, rtm_type;
    unsigned rtm_flags;
};
struct nl_pktinfo {
    uint32_t group;
};
#define ALIGN4(n) (((n) + 3) & ~3)
/* The attributes of message h after its fixed header of hdr bytes */
#define FOR_ATTRS(a, h, hdr)                                                                   \
    for (struct rtattr *a = (void *)((char *)(h) + 16 + (hdr));                                \
         (char *)a + 4 <= (char *)(h) + (h)->nlmsg_len && a->rta_len >= 4;                     \
         a = (void *)((char *)a + ALIGN4(a->rta_len)))
#define DATA(a) ((void *)((a) + 1))

#define ALL (RTMGRP_LINK | RTMGRP_IPV4_IFADDR | RTMGRP_IPV4_ROUTE | RTMGRP_IPV6_IFADDR | \
             RTMGRP_IPV6_ROUTE)

static int sub(unsigned groups)
{
    int fd = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    struct sockaddr_nl a = {.nl_family = AF_NETLINK, .nl_groups = groups};
    if (bind(fd, (void *)&a, sizeof a))
        perror("bind");
    int one = 1;
    setsockopt(fd, SOL_NETLINK, NETLINK_PKTINFO, &one, sizeof one);
    return fd;
}

static char lines[8192][160];
static int nlines;

static int cmp(const void *a, const void *b)
{
    return strcmp(a, b);
}

static void ntop(int family, const void *a, char *out)
{
    inet_ntop(family, a, out, INET6_ADDRSTRLEN);
}

static void describe(struct nlmsghdr *h, unsigned group, unsigned mask)
{
    char *l = lines[nlines < 8191 ? nlines++ : nlines];
    const char *new = h->nlmsg_type % 4 == 0 ? "new" : "del";
    if (h->nlmsg_type == RTM_NEWLINK || h->nlmsg_type == RTM_DELLINK) {
        struct ifinfomsg *i = (void *)(h + 1);
        char name[IFNAMSIZ] = "?";
        unsigned mtu = 0;
        FOR_ATTRS(a, h, sizeof *i) {
            if (a->rta_type == IFLA_IFNAME)
                snprintf(name, sizeof name, "%s", (char *)DATA(a));
            if (a->rta_type == IFLA_MTU)
                mtu = *(unsigned *)DATA(a);
        }
        snprintf(l, 160, "%slink %s index %d mtu %u (group %u, %#x)", new, name, i->ifi_index, mtu,
                 group, mask);
    } else if (h->nlmsg_type == RTM_NEWADDR || h->nlmsg_type == RTM_DELADDR) {
        struct ifaddrmsg *i = (void *)(h + 1);
        char addr[INET6_ADDRSTRLEN] = "?";
        FOR_ATTRS(a, h, sizeof *i)
            if (a->rta_type == IFA_ADDRESS)
                ntop(i->ifa_family, DATA(a), addr);
        snprintf(l, 160, "%saddr %s/%d index %d (group %u, %#x)", new, addr, i->ifa_prefixlen,
                 i->ifa_index, group, mask);
    } else if (h->nlmsg_type == RTM_NEWROUTE || h->nlmsg_type == RTM_DELROUTE) {
        struct rtmsg *r = (void *)(h + 1);
        char dst[INET6_ADDRSTRLEN] = "default";
        unsigned table = r->rtm_table;
        int oif = 0;
        FOR_ATTRS(a, h, sizeof *r) {
            if (a->rta_type == RTA_DST)
                ntop(r->rtm_family, DATA(a), dst);
            if (a->rta_type == RTA_TABLE)
                table = *(unsigned *)DATA(a);
            if (a->rta_type == RTA_OIF)
                oif = *(int *)DATA(a);
        }
        // Multicast routes Linux adds for IPv6 are not modelled by every kernel the same way
        if (!strncmp(dst, "ff00::", 6)) {
            nlines--;
            return;
        }
        snprintf(l, 160, "%sroute %s/%d table %u type %d proto %d oif %d (group %u, %#x)", new,
                 dst, r->rtm_dst_len, table, r->rtm_type, r->rtm_protocol, oif, group, mask);
    } else {
        snprintf(l, 160, "type %d", h->nlmsg_type);
    }
}

// Reads one datagram: 1 when one was read, 0 when none came in `ms`, -errno on an error
static int one(int fd, int ms, int print)
{
    struct pollfd p = {fd, POLLIN};
    if (poll(&p, 1, ms) <= 0)
        return 0;
    char buf[65536], ctl[64];
    struct sockaddr_nl from;
    struct iovec v = {buf, sizeof buf};
    struct msghdr m = {.msg_name = &from, .msg_namelen = sizeof from, .msg_iov = &v,
                       .msg_iovlen = 1, .msg_control = ctl, .msg_controllen = sizeof ctl};
    int n = recvmsg(fd, &m, MSG_DONTWAIT);
    if (n < 0)
        return errno == EAGAIN ? 0 : -errno;
    unsigned group = 0;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c))
        /* struct nl_pktinfo */
        if (c->cmsg_level == SOL_NETLINK && c->cmsg_type == NETLINK_PKTINFO)
            group = ((struct nl_pktinfo *)CMSG_DATA(c))->group;
    for (char *p = buf; print && p + 16 <= buf + n;) {
        struct nlmsghdr *h = (void *)p;
        if (h->nlmsg_len < 16 || p + h->nlmsg_len > buf + n)
            break;
        describe(h, group, from.nl_groups);
        p += ALIGN4(h->nlmsg_len);
    }
    return 1;
}

// Prints what came in until nothing more does for 400 ms
static void drain(int fd)
{
    nlines = 0;
    while (one(fd, 400, 1) > 0)
        ;
    qsort(lines, nlines, sizeof lines[0], cmp);
    for (int i = 0; i < nlines; i++)
        if (!i || strcmp(lines[i], lines[i - 1]))
            printf("  %s\n", lines[i]);
}

static int count(int fd, int link_only)
{
    nlines = 0;
    int n = 0;
    while (one(fd, 100, 1) > 0)
        ;
    for (int i = 0; i < nlines; i++)
        n += !link_only || !strstr(lines[i], "link ");
    return n;
}

static void step(int fd, const char *cmd)
{
    printf("== %s\n", cmd);
    fflush(stdout);
    if (system(cmd))
        printf("  failed\n");
    drain(fd);
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    int outer = sub(ALL);
    if (unshare(CLONE_NEWNET)) {
        perror("unshare");
        return 1;
    }
    // A device that is up, set up before listening (linkwatch, and IPv6 without DAD)
    system("echo 0 > /proc/sys/net/ipv6/conf/default/accept_dad; ip link set lo up; "
           "ip link add d0 type dummy; ip link set d0 up");
    sleep(1);
    int fd = sub(ALL);
    int links = sub(RTMGRP_LINK);
    int none = sub(0);
    step(fd, "ip addr add 10.9.0.1/24 dev d0");
    step(fd, "ip route add 10.10.0.0/16 dev d0");
    step(fd, "ip route replace 10.10.0.0/16 dev d0 proto static");
    step(fd, "ip route del 10.10.0.0/16 dev d0");
    step(fd, "ip addr del 10.9.0.1/24 dev d0");
    step(fd, "ip -6 addr add fd00::1/64 dev d0");
    step(fd, "ip -6 route add fd01::/64 dev d0");
    step(fd, "ip -6 route del fd01::/64 dev d0");
    step(fd, "ip -6 addr del fd00::1/64 dev d0");
    step(fd, "ip link set d0 mtu 1400");
    step(fd, "ifconfig d0 mtu 1300");
    step(fd, "ip link add d2 type dummy");
    step(fd, "ip addr add 10.11.0.1/24 dev d2");
    step(fd, "ip link add v0 type veth peer name v1");
    step(fd, "ip link del v0");
    step(fd, "ip link del d2");
    printf("socket in the outer namespace: %d notifications\n", count(outer, 0));
    printf("socket in the link group: %d others\n", count(links, 1));
    printf("socket in no group: %d notifications\n", count(none, 0));
    close(fd);

    // Overflow: 1100 addresses (one RTM_NEWADDR each) without reading
    int lossy = sub(RTMGRP_IPV4_IFADDR), quiet = sub(RTMGRP_IPV4_IFADDR), one_ = 1;
    setsockopt(quiet, SOL_NETLINK, NETLINK_NO_ENOBUFS, &one_, sizeof one_);
    FILE *b = fopen("/tmp/rtmon.batch", "w");
    fprintf(b, "link add d1 type dummy\n");
    for (int i = 0; i < 1100; i++)
        fprintf(b, "addr add 10.20.%d.%d/32 dev d1\n", i / 200, i % 200 + 1);
    fclose(b);
    system("ip -batch /tmp/rtmon.batch");
    for (int i = 0; i < 2; i++) {
        int r = one(lossy, 100, 0);
        printf("default socket, read %d: %s\n", i + 1, r < 0 ? strerror(-r) : r ? "a message" : "nothing");
    }
    int err = 0;
    socklen_t len = sizeof err;
    getsockopt(lossy, SOL_SOCKET, SO_ERROR, &err, &len);
    printf("default socket, SO_ERROR after: %d\n", err);
    int r = one(quiet, 100, 0);
    printf("NETLINK_NO_ENOBUFS socket, read 1: %s\n", r < 0 ? strerror(-r) : r ? "a message" : "nothing");
    // A second overflow is reported through SO_ERROR too
    while (one(lossy, 0, 0) > 0)
        ;
    system("ip addr flush dev d1");
    getsockopt(lossy, SOL_SOCKET, SO_ERROR, &err, &len);
    printf("default socket, SO_ERROR after the flush: %s\n", strerror(err));
    r = one(lossy, 100, 0);
    printf("default socket, then: %s\n", r < 0 ? strerror(-r) : r ? "a message" : "nothing");
    return 0;
}
C
gcc -O1 -o rtmon rtmon.c
./rtmon
