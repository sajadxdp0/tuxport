/*
 * Guest test program for clones and the Files app writing the same files: build with musl-gcc
 * -static and run it copied into the store. "clone SRC DST" clones SRC onto DST (FICLONE); "hold
 * PATH SECONDS" opens PATH for reading, prints "holding", sleeps, then prints what reading it from
 * the start gives (so the host can write the file meanwhile).
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

/* From <linux/fs.h> */
#define FICLONE 0x40049409

int main(int argc, char **argv)
{
    if (argc == 4 && strcmp(argv[1], "clone") == 0) {
        int src = open(argv[2], O_RDONLY);
        int dst = open(argv[3], O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (src < 0 || dst < 0 || ioctl(dst, FICLONE, src) != 0) {
            perror("clone");
            return 1;
        }
        printf("cloned\n");
        return 0;
    }
    if (argc == 4 && strcmp(argv[1], "hold") == 0) {
        int fd = open(argv[2], O_RDONLY);
        if (fd < 0) {
            perror("open");
            return 1;
        }
        printf("holding\n");
        fflush(stdout);
        sleep(atoi(argv[3]));
        char buf[64] = "";
        ssize_t n = pread(fd, buf, sizeof buf - 1, 0);
        printf("read %s", n > 0 ? buf : "nothing\n");
        return 0;
    }
    fprintf(stderr, "usage: hostclone clone SRC DST | hold PATH SECONDS\n");
    return 2;
}
