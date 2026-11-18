# docker-run-options: --privileged
# Superblock flags: read-only, sync, dirsync and lazytime given to a new tmpfs belong to the
# instance (mountinfo's superblock options, /proc/mounts after ro/rw, statfs's f_flags) and are
# shared by its binds; a bind remount changes only its mount, so a read-only instance stays
# read-only through it; a remount without MS_BIND changes the instance (but not dirsync), and once
# it is read-only again a bind with rw flags cannot open its files for writing either
set -e
cd /tmp
cat > sb.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/statvfs.h>
#include <unistd.h>

#ifndef MS_LAZYTIME
#define MS_LAZYTIME (1 << 25)
#endif

static void show(const char *dir)
{
    char line[1024];
    FILE *f = fopen("/proc/self/mountinfo", "r");
    while (fgets(line, sizeof line, f)) {
        char point[256], opts[256], super[256];
        if (sscanf(line, "%*d %*d %*s %*s %255s %255s", point, opts) == 2 && !strcmp(point, dir)) {
            char *dash = strstr(line, " - ");
            sscanf(dash, " - %*s %*s %255s", super);
            printf("%s mountinfo: %s | %s\n", dir, opts, super);
        }
    }
    fclose(f);
    f = fopen("/proc/self/mounts", "r");
    while (fgets(line, sizeof line, f)) {
        char point[256], opts[256];
        if (sscanf(line, "%*s %255s %*s %255s", point, opts) == 2 && !strcmp(point, dir))
            printf("%s mounts: %s\n", dir, opts);
    }
    fclose(f);
    struct statvfs sv;
    statvfs(dir, &sv);
    printf("%s statvfs: rdonly %d sync %d mand %d\n", dir, !!(sv.f_flag & ST_RDONLY),
           !!(sv.f_flag & ST_SYNCHRONOUS), !!(sv.f_flag & ST_MANDLOCK));
}

static void touch(const char *path)
{
    int fd = open(path, O_CREAT | O_WRONLY, 0644);
    printf("create %s: %s\n", path, fd >= 0 ? "ok" : strerror(errno));
    if (fd >= 0)
        close(fd);
}

static void reopen(const char *path, int flags)
{
    int fd = open(path, flags);
    printf("open %s 0%o: %s\n", path, flags, fd >= 0 ? "ok" : strerror(errno));
    if (fd >= 0)
        close(fd);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    mkdir("/tmp/sba", 0755);
    mkdir("/tmp/sbb", 0755);
    mount("none", "/tmp/sba", "tmpfs", MS_RDONLY | MS_SYNCHRONOUS | MS_DIRSYNC | MS_LAZYTIME,
          "size=1m");
    mount("/tmp/sba", "/tmp/sbb", NULL, MS_BIND, NULL);
    show("/tmp/sba");
    show("/tmp/sbb");
    // A bind remount read-write: the mount turns rw, the instance stays read-only
    printf("bind remount rw: %d\n", mount(NULL, "/tmp/sbb", NULL, MS_REMOUNT | MS_BIND, NULL));
    show("/tmp/sbb");
    touch("/tmp/sbb/x");
    // A remount without MS_BIND: the instance turns read-write and loses sync and lazytime
    // (dirsync stays), the other bind keeps its own read-only flag
    printf("remount rw: %d\n", mount(NULL, "/tmp/sba", NULL, MS_REMOUNT, "size=1m"));
    show("/tmp/sba");
    show("/tmp/sbb");
    touch("/tmp/sba/y");
    touch("/tmp/sbb/z");
    // The instance read-only again without MS_BIND: the bind keeps its own rw flag, yet its
    // files cannot be opened for writing or truncated (mnt_want_write checks both)
    printf("remount ro: %d\n", mount(NULL, "/tmp/sba", NULL, MS_REMOUNT | MS_RDONLY, "size=1m"));
    show("/tmp/sbb");
    reopen("/tmp/sbb/y", O_WRONLY);
    reopen("/tmp/sbb/y", O_RDONLY | O_TRUNC);
    reopen("/tmp/sbb/y", O_RDONLY);
    umount("/tmp/sbb");
    umount("/tmp/sba");
    return 0;
}
C
gcc -O1 -o sb sb.c
./sb
