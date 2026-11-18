# docker-run-options: --privileged
# Change notifications of synthetic files: mounts and mountinfo report POLLPRI|POLLERR to one poll
# after the mount table changes, reading them does not take the change in; which procfs, sysfs
# and cgroup2 files epoll accepts; a sysfs file polls as changed until its first read (kernfs);
# cgroup.events reports a process entering and leaving the cgroup and its freezing to poll, epoll
# (EPOLLPRI) and inotify (IN_MODIFY on the file and on its directory, which also gets the writes
# to cgroup.procs and cgroup.freeze)
set -e
cd /tmp
cat > notify.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/inotify.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int revents(int fd)
{
    struct pollfd p = {.fd = fd, .events = POLLIN | POLLPRI | POLLOUT};
    poll(&p, 1, 0);
    return p.revents;
}

static void reread(int fd)
{
    char buf[65536];
    lseek(fd, 0, SEEK_SET);
    while (read(fd, buf, sizeof buf) > 0)
        ;
}

static int by_name(const void *a, const void *b)
{
    return strcmp(*(char *const *)a, *(char *const *)b);
}

/* The events in name order: Linux queues kernfs's notification from a workqueue, so it may come
 * before or after the event of the write that caused it */
static void show_events(const char *what, int in)
{
    char buf[4096], *names[64];
    char line[64][80];
    int count = 0;
    ssize_t n = read(in, buf, sizeof buf);
    for (char *p = buf; n > 0 && p < buf + n && count < 64;) {
        struct inotify_event *e = (struct inotify_event *)p;
        snprintf(line[count], sizeof line[count], "%s%s%s",
                 e->mask & IN_MODIFY ? "IN_MODIFY" : "other", e->len ? " " : "",
                 e->len ? e->name : "");
        names[count] = line[count];
        count++;
        p += sizeof *e + e->len;
    }
    qsort(names, count, sizeof *names, by_name);
    printf("  %s:", what);
    for (int i = 0; i < count; i++)
        printf(" %s", names[i]);
    printf("%s\n", count ? "" : " none");
}

static void show(const char *what, int fd)
{
    char buf[256] = "";
    pread(fd, buf, sizeof buf - 1, 0);
    for (char *c = buf; *c; c++)
        if (*c == '\n')
            *c = ',';
    printf("%s: %s\n", what, buf);
}

/* Waits for the cgroup.events change through epoll (EPOLLPRI), then for inotify */
static void wait_change(const char *what, int ep, int ev, int dir_in, int file_in)
{
    struct epoll_event e;
    int n = epoll_wait(ep, &e, 1, 2000);
    printf("%s: epoll %d, events %#x\n", what, n, n > 0 ? e.events : 0);
    usleep(100000);
    show_events("directory watch", dir_in);
    show_events("file watch", file_in);
    reread(ev);
    show("  cgroup.events", ev);
}

int main(void)
{
    /* mountinfo */
    int mi = open("/proc/self/mountinfo", O_RDONLY);
    printf("mountinfo: %#x\n", revents(mi));
    mkdir("/tmp/nm", 0755);
    mount("tmpfs", "/tmp/nm", "tmpfs", 0, NULL);
    printf("after a mount: %#x\n", revents(mi));
    printf("polled again: %#x\n", revents(mi));
    umount("/tmp/nm");
    reread(mi);
    printf("after an unmount and a read: %#x\n", revents(mi));
    mount("tmpfs", "/tmp/nm", "tmpfs", 0, NULL);
    int ep = epoll_create1(0);
    struct epoll_event e = {.events = EPOLLPRI, .data.fd = mi};
    printf("epoll_ctl mountinfo: %d\n", epoll_ctl(ep, EPOLL_CTL_ADD, mi, &e));
    umount("/tmp/nm");
    printf("after an unmount: epoll %d\n", epoll_wait(ep, &e, 1, 1000));
    close(ep);

    /* Which files epoll accepts */
    const char *files[] = {"/proc/meminfo", "/proc/self/status", "/proc/sys/kernel/hostname",
                           "/proc/self/net/tcp", "/proc/sysvipc/shm", "/proc/self/mounts",
                           "/sys/devices/system/cpu/online", "/sys/fs/cgroup/cgroup.procs",
                           "/proc/self/ns/net"};
    ep = epoll_create1(0);
    for (unsigned i = 0; i < sizeof files / sizeof *files; i++) {
        int fd = open(files[i], O_RDONLY);
        e.events = EPOLLIN;
        int r = epoll_ctl(ep, EPOLL_CTL_ADD, fd, &e);
        printf("epoll_ctl %s: %s\n", files[i], r ? strerror(errno) : "ok");
        close(fd);
    }
    close(ep);

    /* A sysfs file is changed until first read */
    int on = open("/sys/devices/system/cpu/online", O_RDONLY);
    printf("cpu/online before reading: %#x\n", revents(on));
    reread(on);
    printf("cpu/online after reading: %#x\n", revents(on));

    /* cgroup.events */
    rmdir("/sys/fs/cgroup/n");
    mkdir("/sys/fs/cgroup/n", 0755);
    int ev = open("/sys/fs/cgroup/n/cgroup.events", O_RDONLY);
    reread(ev);
    show("cgroup.events", ev);
    printf("polled after reading: %#x\n", revents(ev));
    int dir_in = inotify_init1(IN_NONBLOCK), file_in = inotify_init1(IN_NONBLOCK);
    inotify_add_watch(dir_in, "/sys/fs/cgroup/n", IN_MODIFY);
    inotify_add_watch(file_in, "/sys/fs/cgroup/n/cgroup.events", IN_MODIFY);
    ep = epoll_create1(0);
    e.events = EPOLLPRI;
    e.data.fd = ev;
    epoll_ctl(ep, EPOLL_CTL_ADD, ev, &e);
    int go[2];
    pipe(go);
    pid_t child = fork();
    if (child == 0) {
        for (;;)
            pause();
    }
    char pid[16];
    int procs = open("/sys/fs/cgroup/n/cgroup.procs", O_WRONLY);
    write(procs, pid, snprintf(pid, sizeof pid, "%d", child));
    close(procs);
    wait_change("a process entered", ep, ev, dir_in, file_in);
    int freeze = open("/sys/fs/cgroup/n/cgroup.freeze", O_WRONLY);
    write(freeze, "1", 1);
    wait_change("frozen", ep, ev, dir_in, file_in);
    write(freeze, "0", 1);
    wait_change("thawed", ep, ev, dir_in, file_in);
    close(freeze);
    kill(child, SIGKILL);
    waitpid(child, NULL, 0);
    wait_change("the process left", ep, ev, dir_in, file_in);
    close(ep);
    close(ev);
    close(dir_in);
    close(file_in);

    /* Inherited freeze on empty children. Each child has only one kind of inotify
     * watch. Keep the directory watch's child dentry instantiated for kernfs;
     * O_PATH does not open the file for poll. */
    const char *dir_child = "/sys/fs/cgroup/n/d", *file_child = "/sys/fs/cgroup/n/f";
    mkdir(dir_child, 0755);
    mkdir(file_child, 0755);
    dir_in = inotify_init1(IN_NONBLOCK);
    file_in = inotify_init1(IN_NONBLOCK);
    int pathfd = open("/sys/fs/cgroup/n/d/cgroup.events", O_PATH);
    inotify_add_watch(dir_in, dir_child, IN_MODIFY);
    inotify_add_watch(file_in, "/sys/fs/cgroup/n/f/cgroup.events", IN_MODIFY);
    freeze = open("/sys/fs/cgroup/n/cgroup.freeze", O_WRONLY);
    write(freeze, "1", 1);
    struct pollfd dw = {.fd = dir_in, .events = POLLIN};
    struct pollfd fw = {.fd = file_in, .events = POLLIN};
    poll(&dw, 1, 2000);
    poll(&fw, 1, 2000);
    show_events("inherited freeze, directory alone", dir_in);
    show_events("inherited freeze, file alone", file_in);
    close(dir_in);
    close(file_in);
    write(freeze, "0", 1);
    close(freeze);
    close(pathfd);
    rmdir(dir_child);
    rmdir(file_child);
    rmdir("/sys/fs/cgroup/n");
    return 0;
}
C
gcc -O1 -o notify notify.c
./notify
rm -rf notify notify.c /tmp/nm
