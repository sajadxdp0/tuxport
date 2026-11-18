/*
 * Classic BPF filters on TCP sockets when the host refuses them. A host with Linux 5d39580f68e6
 * lets only CAP_NET_ADMIN attach SO_ATTACH_FILTER to a TCP socket, and the app never has it, so
 * Tuxport keeps the program itself; a test hook of debug builds (tuxport-run must set
 * TUXPORT_TEST_HOOKS=1) makes any host act that way. With the hook on, a guest with CAP_NET_ADMIN
 * attaches, reads back, replaces and detaches the program as on Linux, sockets accepted from a
 * listener inherit it, a locked filter cannot be changed, a bad program is still refused, and
 * without CAP_NET_ADMIN the attach is EPERM. Prints "all ok" when every check passes.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <netinet/in.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define TEST_SYSCALL 0xa991
#define HOOK_REFUSE_TCP_FILTERS 14

/* linux/filter.h and linux/capability.h, which musl-gcc does not see */
struct sock_filter {
    uint16_t code;
    uint8_t jt, jf;
    uint32_t k;
};
struct sock_fprog {
    unsigned short len;
    struct sock_filter *filter;
};
#define BPF_LD 0x00
#define BPF_W 0x00
#define BPF_LEN 0x80
#define BPF_RET 0x06
#define BPF_K 0x00
#ifndef SO_ATTACH_FILTER
#define SO_ATTACH_FILTER 26
#endif
#ifndef SO_DETACH_FILTER
#define SO_DETACH_FILTER 27
#endif
#ifndef SO_GET_FILTER
#define SO_GET_FILTER SO_ATTACH_FILTER
#endif
#ifndef SO_LOCK_FILTER
#define SO_LOCK_FILTER 44
#endif
#define CAP_VERSION_3 0x20080522
#define CAP_NET_ADMIN 12
struct cap_header {
    uint32_t version;
    int pid;
};
struct cap_data {
    uint32_t effective, permitted, inheritable;
};

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            printf("FAIL line %d: ", __LINE__);                                                    \
            printf(__VA_ARGS__);                                                                   \
            printf(" (errno %d)\n", errno);                                                        \
            exit(1);                                                                               \
        }                                                                                          \
    } while (0)

static struct sock_filter drop[1] = {{BPF_RET | BPF_K, 0, 0, 0}};
static struct sock_filter pass[2] = {{BPF_LD | BPF_W | BPF_LEN, 0, 0, 0},
                                     {BPF_RET | BPF_K, 0, 0, 0xffffffff}};

static int attach(int fd, struct sock_filter *insns, unsigned short n, socklen_t len)
{
    struct sock_fprog prog = {n, insns};
    return setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, &prog, len);
}

/* The program's count by SO_GET_FILTER with room for `room` instructions, copied into `out`;
 * -1 with errno on failure */
static int get(int fd, struct sock_filter *out, socklen_t room)
{
    socklen_t len = room;
    if (getsockopt(fd, SOL_SOCKET, SO_GET_FILTER, out, &len) != 0)
        return -1;
    return (int)len;
}

static int tcp(void)
{
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(fd >= 0, "socket");
    return fd;
}

static void hook(int on)
{
    CHECK(syscall(TEST_SYSCALL, HOOK_REFUSE_TCP_FILTERS, on) == 0, "hook %d", on);
}

int main(void)
{
    struct sock_filter got[4];
    if (getuid() != 0) {
        printf("skipped: not root\n");
        return 0;
    }
    hook(1);

    /* Attach, read back, replace */
    int s = tcp();
    CHECK(attach(s, drop, 1, sizeof(struct sock_fprog)) == 0, "attach drop");
    CHECK(get(s, NULL, 0) == 1, "count");
    memset(got, 0, sizeof got);
    CHECK(get(s, got, 1) == 1 && memcmp(got, drop, sizeof drop) == 0, "read back");
    CHECK(attach(s, pass, 2, sizeof(struct sock_fprog)) == 0, "replace");
    CHECK(get(s, got, 1) == -1 && errno == EINVAL, "short room");
    CHECK(get(s, got, 4) == 2 && memcmp(got, pass, sizeof pass) == 0, "read replaced");
    printf("ok attach\n");

    /* Refused attaches leave the program alone */
    struct sock_filter bad[1] = {{0xffff, 0, 0, 0}};
    CHECK(attach(s, drop, 1, 15) == -1 && errno == EINVAL, "optlen 15");
    CHECK(attach(s, drop, 1, 3) == -1 && errno == EINVAL, "optlen 3");
    CHECK(attach(s, drop, 0, sizeof(struct sock_fprog)) == -1 && errno == EINVAL, "count 0");
    CHECK(attach(s, NULL, 1, sizeof(struct sock_fprog)) == -1 && errno == EINVAL, "no program");
    CHECK(attach(s, bad, 1, sizeof(struct sock_fprog)) == -1 && errno == EINVAL, "bad opcode");
    CHECK(attach(s, (struct sock_filter *)8, 1, sizeof(struct sock_fprog)) == -1 &&
              errno == EFAULT,
          "bad pointer");
    CHECK(get(s, got, 4) == 2, "kept after refusals");
    printf("ok refused\n");

    /* Detach */
    CHECK(setsockopt(s, SOL_SOCKET, SO_DETACH_FILTER, &(int){0}, sizeof(int)) == 0, "detach");
    CHECK(get(s, got, 4) == 0, "none after detach");
    CHECK(setsockopt(s, SOL_SOCKET, SO_DETACH_FILTER, &(int){0}, sizeof(int)) == -1 &&
              errno == ENOENT,
          "detach twice");
    printf("ok detach\n");

    /* Accepted sockets inherit the listener's program */
    int l = tcp();
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t al = sizeof a;
    CHECK(bind(l, (struct sockaddr *)&a, sizeof a) == 0, "bind");
    CHECK(getsockname(l, (struct sockaddr *)&a, &al) == 0, "getsockname");
    CHECK(attach(l, pass, 2, sizeof(struct sock_fprog)) == 0, "attach listener");
    CHECK(listen(l, 1) == 0, "listen");
    int c = tcp();
    CHECK(connect(c, (struct sockaddr *)&a, sizeof a) == 0, "connect");
    int n = accept(l, NULL, NULL);
    CHECK(n >= 0, "accept");
    CHECK(get(n, got, 4) == 2 && memcmp(got, pass, sizeof pass) == 0, "inherited");
    CHECK(get(c, got, 4) == 0, "client has none");
    printf("ok accept\n");

    /* A locked filter can be neither replaced nor detached */
    CHECK(setsockopt(n, SOL_SOCKET, SO_LOCK_FILTER, &(int){1}, sizeof(int)) == 0, "lock");
    CHECK(attach(n, drop, 1, sizeof(struct sock_fprog)) == -1 && errno == EPERM, "attach locked");
    CHECK(setsockopt(n, SOL_SOCKET, SO_DETACH_FILTER, &(int){0}, sizeof(int)) == -1 &&
              errno == EPERM,
          "detach locked");
    CHECK(get(n, got, 4) == 2, "still there");
    printf("ok lock\n");

    /* Without CAP_NET_ADMIN a TCP filter is EPERM before anything else; UDP needs nothing */
    pid_t pid = fork();
    CHECK(pid >= 0, "fork");
    if (pid == 0) {
        struct cap_header h = {CAP_VERSION_3, 0};
        struct cap_data d[2];
        CHECK(syscall(SYS_capget, &h, d) == 0, "capget");
        d[CAP_NET_ADMIN / 32].effective &= ~(1u << (CAP_NET_ADMIN % 32));
        CHECK(syscall(SYS_capset, &h, d) == 0, "capset");
        int t = tcp();
        CHECK(attach(t, drop, 1, sizeof(struct sock_fprog)) == -1 && errno == EPERM, "no cap");
        CHECK(attach(t, drop, 1, 15) == -1 && errno == EPERM, "no cap, bad optlen");
        int u = socket(AF_INET, SOCK_DGRAM, 0);
        CHECK(attach(u, drop, 1, sizeof(struct sock_fprog)) == 0, "udp without the cap");
        exit(0);
    }
    int status;
    CHECK(waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0,
          "child");
    printf("ok capability\n");

    hook(0);
    printf("all ok\n");
    return 0;
}
