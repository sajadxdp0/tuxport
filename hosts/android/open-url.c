// Guest browser launcher (android/native). Built as a static Linux executable, for either distro.
// The endpoint is read for each invocation so surviving tmux/screen shells never cache tab state.
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#define CONFIG_PATH "/etc/tuxport/browser"
#define MAX_URL_BYTES 8192

static int fail(const char *message) {
    fprintf(stderr, "xdg-open: %s\n", message);
    return 1;
}

static int64_t now_ms(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t) != 0) return -1;
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

static int wait_fd(int fd, short events, int64_t deadline) {
    for (;;) {
        int64_t now = now_ms();
        if (now < 0 || now >= deadline) return 0;
        struct pollfd p = {.fd = fd, .events = events};
        int result = poll(&p, 1, (int)(deadline - now));
        if (result > 0) return 1;
        if (result == 0 || errno != EINTR) return 0;
    }
}

int main(int argc, char **argv) {
    if (argc != 2) return fail("usage: xdg-open URL");
    const char *url = argv[1];
    size_t length = strlen(url);
    if (length > MAX_URL_BYTES ||
        (strncmp(url, "http://", 7) != 0 && strncmp(url, "https://", 8) != 0))
        return fail("expected an http or https URL of at most 8192 bytes");
    for (size_t i = 0; i < length; i++) {
        unsigned char c = (unsigned char)url[i];
        if (c <= 0x20 || c == 0x7f) return fail("invalid web URL");
    }

    FILE *config = fopen(CONFIG_PATH, "r");
    if (config == NULL) return fail("Tuxport browser bridge is unavailable");
    char port_line[16], token[66];
    int valid = fgets(port_line, sizeof(port_line), config) != NULL &&
                fgets(token, sizeof(token), config) != NULL && fgetc(config) == EOF;
    fclose(config);
    if (!valid) return fail("invalid Tuxport browser configuration");
    char *end;
    errno = 0;
    unsigned long port = strtoul(port_line, &end, 10);
    size_t token_length = strlen(token);
    if (errno != 0 || end == port_line || strcmp(end, "\n") != 0 || port == 0 || port > 65535 ||
        token_length != 37 || token[36] != '\n')
        return fail("invalid Tuxport browser configuration");
    for (size_t i = 0; i < 36; i++) {
        char c = token[i];
        if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || c == '-'))
            return fail("invalid Tuxport browser configuration");
    }

    int fd = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    if (fd < 0) return fail("cannot create browser connection");
    struct sockaddr_in address = {
        .sin_family = AF_INET,
        .sin_port = htons((uint16_t)port),
        .sin_addr = {.s_addr = htonl(INADDR_LOOPBACK)},
    };
    int64_t now = now_ms();
    int64_t deadline = now + 5000;
    if (now < 0) { close(fd); return fail("cannot read the clock"); }
    int connected = connect(fd, (struct sockaddr *)&address, sizeof(address));
    if (connected != 0 && (errno != EINPROGRESS || !wait_fd(fd, POLLOUT, deadline))) {
        close(fd);
        return fail("cannot connect to Tuxport browser bridge");
    }
    int error = 0;
    socklen_t error_length = sizeof(error);
    if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &error_length) != 0 || error != 0) {
        close(fd);
        return fail("Tuxport browser bridge is unavailable");
    }
    char request[66 + MAX_URL_BYTES + 1];
    memcpy(request, token, token_length);
    memcpy(request + token_length, url, length);
    request[token_length + length] = '\n';
    size_t total = token_length + length + 1;
    for (size_t sent = 0; sent < total;) {
        if (!wait_fd(fd, POLLOUT, deadline)) goto timeout;
        ssize_t n = send(fd, request + sent, total - sent, MSG_NOSIGNAL);
        if (n > 0) sent += (size_t)n;
        else if (n < 0 && (errno == EAGAIN || errno == EINTR)) continue;
        else { close(fd); return fail("cannot send browser request"); }
    }
    shutdown(fd, SHUT_WR);
    char reply[160];
    size_t used = 0;
    while (used < sizeof(reply) - 1) {
        if (!wait_fd(fd, POLLIN, deadline)) goto timeout;
        ssize_t n = recv(fd, reply + used, sizeof(reply) - 1 - used, 0);
        if (n < 0 && (errno == EAGAIN || errno == EINTR)) continue;
        if (n <= 0) break;
        used += (size_t)n;
        if (memchr(reply, '\n', used) != NULL) break;
    }
    close(fd);
    reply[used] = 0;
    if (strcmp(reply, "OK\n") == 0) return 0;
    if (used > 7 && strncmp(reply, "ERROR ", 6) == 0 && reply[used - 1] == '\n') {
        reply[used - 1] = 0;
        // Never let an unexpected peer inject terminal controls into the error message.
        for (size_t i = 6; i < used - 1; i++)
            if ((unsigned char)reply[i] < 0x20 || (unsigned char)reply[i] > 0x7e)
                return fail("invalid browser response");
        return fail(reply + 6);
    }
    return fail("browser request was not acknowledged");

timeout:
    close(fd);
    return fail("browser request timed out");
}
