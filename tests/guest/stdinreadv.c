/*
 * readv on host stdio (tuxport-run without a PTY, stdin a host pipe) returns what has arrived once
 * an iovec is full, as Linux's pipe read_iter does, rather than waiting for more input to fill the
 * next one. The host sends exactly 8 bytes for an 8-byte and a 64-byte iovec and waits for the
 * answer line before sending more; then 10 bytes, which spread over both iovecs; then it closes
 * stdin. Each answer is one writev of three iovecs.
 */
#include <stdio.h>
#include <string.h>
#include <sys/uio.h>
#include <unistd.h>

static ssize_t take(void) {
    char a[8], b[64], head[32];
    struct iovec in[2] = {{a, sizeof a}, {b, sizeof b}};
    ssize_t n = readv(0, in, 2);
    if (n < 0) {
        perror("readv");
        _exit(1);
    }
    size_t first = n < 8 ? (size_t)n : 8;
    int len = snprintf(head, sizeof head, "got %zd: ", n);
    char rest[80];
    int rlen = snprintf(rest, sizeof rest, "|%.*s\n", (int)(n - first), b);
    struct iovec out[3] = {{head, (size_t)len}, {a, first}, {rest, (size_t)rlen}};
    if (writev(1, out, 3) != (ssize_t)(len + first + rlen)) {
        perror("writev");
        _exit(1);
    }
    return n;
}

int main(void) {
    while (take() > 0) {
    }
    return 0;
}
