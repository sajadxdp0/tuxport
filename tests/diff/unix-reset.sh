# AF_UNIX: a connection closed with data its end had not read, or never accepted, is reset (the
# peer's read once its own data is read, SO_ERROR once, POLLERR; a seqpacket read reports it before
# what is queued); abstract names are kept apart by socket type (a stream and a datagram socket
# bind the same name, and a connect to it with the wrong type is refused); recvfrom on a stream or
# seqpacket connection gives the sending socket's bound name, or none
set -e
cd /tmp
cat > ureset.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static void show(const char *what, long r)
{
    printf("%s: %ld %s\n", what, r, r < 0 ? strerror(errno) : "");
}

static int so_error(int s)
{
    int e = -1;
    socklen_t l = sizeof e;
    getsockopt(s, SOL_SOCKET, SO_ERROR, &e, &l);
    return e;
}

static int revents(int s)
{
    struct pollfd p = {s, POLLIN | POLLOUT, 0};
    poll(&p, 1, 0);
    return p.revents;
}

static socklen_t abstract(struct sockaddr_un *a, const char *name)
{
    memset(a, 0, sizeof *a);
    a->sun_family = AF_UNIX;
    strcpy(a->sun_path + 1, name);
    return offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name);
}

static void reset(int type, const char *name)
{
    char buf[16];
    int sv[2];
    printf("-- %s\n", name);
    socketpair(AF_UNIX, type, 0, sv);
    // sv[1] writes to sv[0], which closes without reading it; sv[0] wrote to sv[1] first
    write(sv[0], "mine", 4);
    write(sv[1], "unread", 6);
    close(sv[0]);
    printf("revents %#x\n", revents(sv[1]));
    show("read", read(sv[1], buf, sizeof buf));
    show("read again", read(sv[1], buf, sizeof buf));
    show("read after", read(sv[1], buf, sizeof buf));
    printf("SO_ERROR %d\n", so_error(sv[1]));
    close(sv[1]);

    // Closed with nothing unread: no reset
    socketpair(AF_UNIX, type, 0, sv);
    write(sv[1], "read", 4);
    read(sv[0], buf, sizeof buf);
    close(sv[0]);
    printf("read nothing unread: revents %#x\n", revents(sv[1]));
    show("read", read(sv[1], buf, sizeof buf));
    close(sv[1]);

    // SO_ERROR reports it once, and the read then sees only EOF
    socketpair(AF_UNIX, type, 0, sv);
    write(sv[1], "x", 1);
    close(sv[0]);
    printf("SO_ERROR %d, then %d\n", so_error(sv[1]), so_error(sv[1]));
    show("read", read(sv[1], buf, sizeof buf));
    close(sv[1]);

    // A listener closed with a connection it never accepted resets it
    struct sockaddr_un a;
    socklen_t len = abstract(&a, name);
    int l = socket(AF_UNIX, type, 0);
    bind(l, (struct sockaddr *)&a, len);
    listen(l, 1);
    int c = socket(AF_UNIX, type, 0);
    show("connect", connect(c, (struct sockaddr *)&a, len));
    close(l);
    printf("not accepted: revents %#x\n", revents(c));
    show("read", read(c, buf, sizeof buf));
    show("read again", read(c, buf, sizeof buf));
    close(c);
}

static void sender(int type, const char *name)
{
    struct sockaddr_un a, b, from;
    socklen_t la = abstract(&a, name), lb, fl;
    char bname[32], buf[8];
    snprintf(bname, sizeof bname, "%s-client", name);
    lb = abstract(&b, bname);
    int l = socket(AF_UNIX, type, 0);
    bind(l, (struct sockaddr *)&a, la);
    listen(l, 2);
    // Unbound client: no address
    int c = socket(AF_UNIX, type, 0);
    connect(c, (struct sockaddr *)&a, la);
    int s = accept(l, NULL, NULL);
    write(c, "1", 1);
    fl = sizeof from;
    memset(&from, 0, sizeof from);
    show("unbound sender", recvfrom(s, buf, sizeof buf, 0, (struct sockaddr *)&from, &fl));
    printf("address length %d\n", (int)fl);
    // Bound after accept: its name as bound when it sent
    bind(c, (struct sockaddr *)&b, lb);
    write(c, "2", 1);
    fl = sizeof from;
    show("bound sender", recvfrom(s, buf, sizeof buf, 0, (struct sockaddr *)&from, &fl));
    printf("address length %d, name %s\n", (int)fl, fl > 3 ? from.sun_path + 1 : "-");
    close(c);
    close(s);
    close(l);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    reset(SOCK_STREAM, "ureset-stream");
    reset(SOCK_SEQPACKET, "ureset-seqpacket");

    // A seqpacket reset comes before what is still queued
    char buf[16];
    int sv[2];
    socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv);
    write(sv[0], "queued", 6);
    write(sv[1], "unread", 6);
    close(sv[0]);
    show("seqpacket with data queued, read", read(sv[1], buf, sizeof buf));
    show("then read", read(sv[1], buf, sizeof buf));
    close(sv[1]);

    // Abstract names by type
    struct sockaddr_un a;
    socklen_t len = abstract(&a, "ureset-typed");
    int st = socket(AF_UNIX, SOCK_STREAM, 0), dg = socket(AF_UNIX, SOCK_DGRAM, 0);
    int sp = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    show("stream bind", bind(st, (struct sockaddr *)&a, len));
    show("datagram bind of the same name", bind(dg, (struct sockaddr *)&a, len));
    show("second stream bind", bind(socket(AF_UNIX, SOCK_STREAM, 0), (struct sockaddr *)&a, len));
    listen(st, 1);
    show("seqpacket connect to it", connect(sp, (struct sockaddr *)&a, len));
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    show("stream connect to it", connect(c, (struct sockaddr *)&a, len));
    int d = socket(AF_UNIX, SOCK_DGRAM, 0);
    show("datagram sendto it", sendto(d, "x", 1, 0, (struct sockaddr *)&a, len));
    show("datagram received", recv(dg, buf, sizeof buf, MSG_DONTWAIT));

    sender(SOCK_STREAM, "ureset-from-stream");
    sender(SOCK_SEQPACKET, "ureset-from-seqpacket");
    return 0;
}
C
gcc -O1 -o ureset ureset.c
./ureset
