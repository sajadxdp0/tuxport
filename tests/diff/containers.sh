# docker-run-options: --cap-add SYS_ADMIN --cap-add NET_ADMIN --cap-add SYS_TIME --security-opt apparmor=unconfined
# Kernel interfaces used by container runtimes (podman, crun, docker, runc), from
# docs/rounds/157-container-survey.md on: capget version probing, bringing up an already-up lo in
# the boot-time namespace, inode numbers of synthetic filesystem roots, fds in thread directories;
# mounting cgroup2 without read-only; the mqueue filesystem; executing memfds and deleted files
# through fds; time namespaces (the native side needs CAP_SYS_TIME to write timens_offsets, and
# the test case does not change the host's time); /proc files that take numbers being written
# with a trailing NUL
set -e
cd /tmp

# capget: with a null data pointer it only probes the version; an unknown version also returns 0
# and writes back the kernel's version, without looking at pid (moby/sys/capability gets the
# version this way); only with a data pointer are the version and pid checked
cat > capget.c <<'C'
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

// The store has no kernel headers, so define them as in linux/capability.h
#define _LINUX_CAPABILITY_VERSION_3 0x20080522
struct __user_cap_header_struct {
    unsigned version;
    int pid;
};
struct __user_cap_data_struct {
    unsigned effective, permitted, inheritable;
};

static void probe(const char *what, unsigned version, int pid, int with_data)
{
    struct __user_cap_header_struct h = {version, pid};
    struct __user_cap_data_struct d[2];
    long r = syscall(SYS_capget, &h, with_data ? d : NULL);
    printf("%-28s ret=%ld %-6s version=%#x\n", what, r, r ? strerror(errno) : "", h.version);
}

int main(void)
{
    probe("probe", 0, 0, 0);
    probe("probe pid -1", 0, -1, 0);
    probe("probe unknown version", 0x12345678, 0, 0);
    probe("v3 no data pid -1", _LINUX_CAPABILITY_VERSION_3, -1, 0);
    probe("v3 no data no such pid", _LINUX_CAPABILITY_VERSION_3, 999999, 0);
    probe("unknown version with data", 0, 0, 1);
    probe("v3 pid -1 with data", _LINUX_CAPABILITY_VERSION_3, -1, 1);
    probe("v3 no such pid with data", _LINUX_CAPABILITY_VERSION_3, 999999, 1);
    probe("v3 self with data", _LINUX_CAPABILITY_VERSION_3, 0, 1);
    return 0;
}
C
gcc -D_GNU_SOURCE -o capget capget.c && ./capget

# Changing lo in the boot-time network namespace: success when nothing is changed (dockerd brings
# up lo for the default network sandbox), and error codes as in Linux 6.1's rtnl_setlink and
# __rtnl_newlink. Requests that really change something would change the container's lo
# natively, so they are not compared
cat > nlset.c <<'C'
#include <errno.h>
#include <net/if.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

// Defined as in linux/netlink.h and linux/rtnetlink.h
#define NETLINK_ROUTE 0
#define NLM_F_REQUEST 1
#define NLM_F_ACK 4
#define NLM_F_REPLACE 0x100
#define NLM_F_EXCL 0x200
#define NLM_F_CREATE 0x400
#define NLMSG_ERROR 2
#define RTM_NEWLINK 16
#define RTM_SETLINK 19
#define IFLA_IFNAME 3
#define IFLA_MTU 4
struct nlmsghdr {
    unsigned nlmsg_len;
    unsigned short nlmsg_type, nlmsg_flags;
    unsigned nlmsg_seq, nlmsg_pid;
};
struct nlmsgerr {
    int error;
    struct nlmsghdr msg;
};
struct ifinfomsg {
    unsigned char ifi_family, pad;
    unsigned short ifi_type;
    int ifi_index;
    unsigned ifi_flags, ifi_change;
};
struct rtattr {
    unsigned short rta_len, rta_type;
};
#define NLMSG_LENGTH(n) ((n) + sizeof(struct nlmsghdr))
#define NLMSG_DATA(h) ((void *)((char *)(h) + sizeof(struct nlmsghdr)))
#define RTA_ALIGN(n) (((n) + 3) & ~3)
#define RTA_LENGTH(n) (sizeof(struct rtattr) + (n))
#define RTA_DATA(a) ((void *)((char *)(a) + sizeof(struct rtattr)))

static int sock;

static void ask(const char *what, int type, int flags, int index, unsigned ifi_flags, unsigned change,
                const char *name, int mtu)
{
    struct {
        struct nlmsghdr h;
        struct ifinfomsg i;
        char attrs[64];
    } m;
    memset(&m, 0, sizeof m);
    m.h.nlmsg_type = type;
    m.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    m.h.nlmsg_seq = 1;
    m.i.ifi_index = index;
    m.i.ifi_flags = ifi_flags;
    m.i.ifi_change = change;
    int len = NLMSG_LENGTH(sizeof m.i);
    if (name) {
        struct rtattr *a = (struct rtattr *)((char *)&m + len);
        a->rta_type = IFLA_IFNAME;
        a->rta_len = RTA_LENGTH(strlen(name) + 1);
        strcpy(RTA_DATA(a), name);
        len += RTA_ALIGN(a->rta_len);
    }
    if (mtu) {
        struct rtattr *a = (struct rtattr *)((char *)&m + len);
        a->rta_type = IFLA_MTU;
        a->rta_len = RTA_LENGTH(4);
        memcpy(RTA_DATA(a), &mtu, 4);
        len += RTA_ALIGN(a->rta_len);
    }
    m.h.nlmsg_len = len;
    if (send(sock, &m, len, 0) < 0) {
        printf("%-30s send: %s\n", what, strerror(errno));
        return;
    }
    char buf[4096];
    int n = recv(sock, buf, sizeof buf, 0);
    struct nlmsghdr *h = (struct nlmsghdr *)buf;
    if (n < 0 || h->nlmsg_type != NLMSG_ERROR) {
        printf("%-30s no ack\n", what);
        return;
    }
    int e = -((struct nlmsgerr *)NLMSG_DATA(h))->error;
    printf("%-30s %s\n", what, e ? strerror(e) : "ok");
}

int main(void)
{
    sock = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    int lo = if_nametoindex("lo");
    ask("newlink up", RTM_NEWLINK, 0, lo, IFF_UP, IFF_UP, NULL, 0);
    ask("setlink up", RTM_SETLINK, 0, lo, IFF_UP, IFF_UP, NULL, 0);
    ask("newlink up by name", RTM_NEWLINK, 0, 0, IFF_UP, IFF_UP, "lo", 0);
    ask("setlink up by name", RTM_SETLINK, 0, 0, IFF_UP, IFF_UP, "lo", 0);
    ask("no flags", RTM_NEWLINK, 0, lo, 0, 0, NULL, 0);
    ask("whole flags", RTM_NEWLINK, 0, lo, IFF_UP | IFF_LOOPBACK | IFF_RUNNING, 0, NULL, 0);
    ask("same name and mtu", RTM_NEWLINK, NLM_F_CREATE, lo, IFF_UP, IFF_UP, "lo", 65536);
    ask("no such index", RTM_NEWLINK, 0, 9999, IFF_UP, IFF_UP, NULL, 0);
    ask("no such name", RTM_SETLINK, 0, 0, IFF_UP, IFF_UP, "nosuch0", 0);
    ask("create unknown type", RTM_NEWLINK, NLM_F_CREATE, 0, 0, 0, "nosuch0", 0);
    ask("excl", RTM_NEWLINK, NLM_F_EXCL, lo, IFF_UP, IFF_UP, NULL, 0);
    ask("replace", RTM_NEWLINK, NLM_F_REPLACE, lo, IFF_UP, IFF_UP, NULL, 0);
    ask("newlink nothing named", RTM_NEWLINK, 0, 0, IFF_UP, IFF_UP, NULL, 0);
    ask("setlink nothing named", RTM_SETLINK, 0, 0, IFF_UP, IFF_UP, NULL, 0);
    ask("negative index", RTM_NEWLINK, 0, -1, IFF_UP, IFF_UP, NULL, 0);
    if (setuid(65534) == 0)
        ask("nobody", RTM_NEWLINK, 0, lo, IFF_UP, IFF_UP, NULL, 0);
    return 0;
}
C
gcc -D_GNU_SOURCE -o nlset nlset.c && ./nlset
ip link set lo up && echo "ip link set lo up: ok"
ip link set dev lo mtu 65536 && echo "ip link set lo mtu 65536: ok"
ip link set nosuch0 up 2>&1 || true

# The inode number of a synthetic filesystem's root directory is 1 (runc checks it after opening
# /proc). docker's /sys/fs/cgroup is a subdirectory in a cgroup namespace, so it is not compared
stat -c '%n %i' /proc /sys /dev/pts
mkdir -p mnt/proc mnt/sys
mount -t proc proc mnt/proc && mount -t sysfs sysfs mnt/sys
stat -c '%n %i' mnt/proc mnt/sys
umount mnt/proc mnt/sys

# fds and fdinfo in thread directories: runc, following pathrs-lite, opens thread-self/fd/ from
# /proc through openat2 (not following magic links, not crossing mounts), then reads the links
# inside. The main thread, another thread, and in a new pid namespace (as runc init does, which
# still sees the outer procfs)
cat > threadfd.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

// Defined as in linux/openat2.h
struct open_how {
    unsigned long long flags, mode, resolve;
};
#define RESOLVE_NO_XDEV 0x01
#define RESOLVE_NO_MAGICLINKS 0x02
#define RESOLVE_BENEATH 0x08
#ifndef SYS_openat2
#define SYS_openat2 437
#endif

static void check(const char *who)
{
    int proc = open("/proc", O_PATH | O_DIRECTORY | O_CLOEXEC);
    struct open_how how = {
        .flags = O_PATH | O_NOFOLLOW | O_CLOEXEC,
        .resolve = RESOLVE_BENEATH | RESOLVE_NO_XDEV | RESOLVE_NO_MAGICLINKS,
    };
    int dir = syscall(SYS_openat2, proc, "thread-self/fd/", &how, sizeof how);
    if (dir < 0) {
        printf("%s: openat2 thread-self/fd/: %s\n", who, strerror(errno));
        return;
    }
    char a[256] = "", b[256] = "";
    readlinkat(dir, "2", a, sizeof a - 1);
    readlink("/proc/self/fd/2", b, sizeof b - 1);
    struct stat st;
    fstatat(dir, "", &st, AT_EMPTY_PATH);
    int info = syscall(SYS_openat2, proc, "thread-self/fdinfo/2", &how, sizeof how);
    printf("%s: dir=%d same link=%d fdinfo=%d\n", who, S_ISDIR(st.st_mode), strcmp(a, b) == 0, info >= 0);
    char path[64];
    snprintf(path, sizeof path, "/proc/self/task/%ld/fd", syscall(SYS_gettid));
    printf("%s: %s exists=%d\n", who, "/proc/self/task/<tid>/fd", access(path, F_OK) == 0);
}

static void *thread(void *arg)
{
    check("thread");
    return arg;
}

int main(void)
{
    check("main");
    pthread_t t;
    pthread_create(&t, NULL, thread, NULL);
    pthread_join(t, NULL);
    fflush(stdout);
    if (unshare(CLONE_NEWPID) == 0) {
        pid_t c = fork();
        if (c == 0) {
            check("in a new pid namespace");
            return 0;
        }
        waitpid(c, NULL, 0);
    }
    return 0;
}
C
gcc -D_GNU_SOURCE -pthread -o threadfd threadfd.c && ./threadfd
ls /proc/$$/task/$$/ | grep -x 'fd\|fdinfo'

# cgroup2 can be mounted without read-only (as crun mounts /sys/fs/cgroup for containers), and
# then remounted read-only or writable with bind. Writing files and creating child cgroups would
# really change things natively, so they are not compared; a remount without bind changes the
# superblock, which natively would change the cgroup docker gives the container, so it is not
# done either
mkdir -p mnt/cg
mount -t cgroup2 cgroup2 mnt/cg && echo "cgroup2 rw: ok"
opts() { awk -v p="$PWD/$1" '$5 == p {split($6, o, ","); print $5, o[1]}' /proc/self/mountinfo; }
opts mnt/cg
python3 -c 'import os; print("ST_RDONLY", os.statvfs("mnt/cg").f_flag & 1)'
[ -f mnt/cg/cgroup.procs ] && echo "has cgroup.procs"
mount -o remount,bind,ro mnt/cg && opts mnt/cg
python3 -c 'import os; print("ST_RDONLY", os.statvfs("mnt/cg").f_flag & 1)'
mount -o remount,bind,rw mnt/cg && opts mnt/cg
umount mnt/cg

# The mqueue filesystem (crun mounts /dev/mqueue for each container): it lists the POSIX message
# queues in the IPC namespace of the process that mounted it. The root directory is 01777, its
# size counts the queues; reading a queue file gives the status line; what is created inside is a
# queue (default size), and its fd can be used as an mq descriptor; unlink removes the queue
# (sticky bit: someone else's is EPERM, which libc's mq_unlink turns into EACCES); mkdir, mknod,
# symlink and rename are EPERM; mounting again on its own root in the same namespace is EBUSY,
# and in a new IPC namespace it is another instance
mkdir -p mnt/mq mnt/mq2
mount -t mqueue mqueue mnt/mq && echo "mqueue: ok"
awk -v p="$PWD/mnt/mq" '$5 == p {for (i = 7; $i != "-"; i++) {}; split($6, o, ","); print "mountinfo", o[1], $(i + 1), $(i + 2)}' /proc/self/mountinfo
stat -c '%A %h %s %u' mnt/mq
stat -f -c 'type %t' mnt/mq
mount -t mqueue mqueue mnt/mq 2>&1 || true
cat > mqfs.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <dirent.h>
#include <unistd.h>

#define D "/tmp/mnt/mq/"

static void show(const char *what, long r)
{
    printf("%-34s %s\n", what, r < 0 ? strerror(errno) : "ok");
}

static void ls(const char *dir)
{
    DIR *d = opendir(dir);
    struct dirent *e;
    printf("ls %s:", dir);
    while (d && (e = readdir(d)))
        if (e->d_name[0] != '.')
            printf(" %s(%s)", e->d_name, e->d_type == DT_REG ? "reg" : "?");
    printf("\n");
    if (d)
        closedir(d);
}

int main(void)
{
    umask(022);
    struct mq_attr a = {.mq_maxmsg = 4, .mq_msgsize = 64};
    mqd_t q1 = mq_open("/q1", O_CREAT | O_RDWR, 0640, &a);
    show("mq_open /q1", q1);
    show("mq_send", mq_send(q1, "hello", 5, 3));
    struct stat st, fst;
    stat(D "q1", &st);
    fstat(q1, &fst);
    printf("q1: mode %o nlink %d size %lld uid %d same ino %d same dev %d\n", st.st_mode, (int)st.st_nlink,
           (long long)st.st_size, st.st_uid, st.st_ino == fst.st_ino, st.st_dev == fst.st_dev);
    struct stat dst;
    stat(D, &dst);
    printf("dir: size %lld, queue on the same device %d\n", (long long)dst.st_size, dst.st_dev == st.st_dev);
    char buf[128] = "";
    int fd = open(D "q1", O_RDONLY);
    int n = read(fd, buf, sizeof buf - 1);
    printf("read %d: %s", n, buf);
    close(fd);
    ls(D);

    // What is created in the filesystem is a queue
    int q2 = open(D "q2", O_CREAT | O_RDWR, 0666);
    show("open O_CREAT q2", q2);
    stat(D "q2", &st);
    printf("q2: mode %o\n", st.st_mode);
    struct mq_attr ga;
    show("mq_getattr on the open fd", mq_getattr(q2, &ga));
    printf("q2 attr: maxmsg %ld msgsize %ld\n", ga.mq_maxmsg, ga.mq_msgsize);
    show("mq_send on the open fd", mq_send(q2, "x", 1, 0));
    mqd_t q2b = mq_open("/q2", O_RDONLY | O_NONBLOCK);
    char m[8192];
    show("mq_receive via mq_open", mq_receive(q2b, m, sizeof m, NULL));
    show("write to the queue fd", write(q2, "x", 1));
    struct statfs sf;
    fstatfs(q2, &sf);
    printf("fstatfs type %lx\n", (unsigned long)sf.f_type);
    show("open O_CREAT|O_EXCL existing", open(D "q2", O_CREAT | O_EXCL | O_RDWR, 0600));
    show("open O_DIRECTORY on a queue", open(D "q2", O_RDONLY | O_DIRECTORY));
    stat(D, &dst);
    printf("dir: size %lld\n", (long long)dst.st_size);

    // Metadata
    show("chmod q1 0600", chmod(D "q1", 0600));
    fstat(q1, &fst);
    printf("q1 mode via mq fd: %o\n", fst.st_mode);
    show("fchmod on the mq fd 0640", fchmod(q1, 0640));
    stat(D "q1", &st);
    printf("q1 mode: %o\n", st.st_mode);
    show("chown q1 1000:1000", chown(D "q1", 1000, 1000));
    stat(D "q1", &st);
    printf("q1 owner: %d %d\n", st.st_uid, st.st_gid);
    struct timespec ts[2] = {{1000, 0}, {2000, 0}};
    show("utimensat q1", utimensat(AT_FDCWD, D "q1", ts, 0));
    stat(D "q1", &st);
    printf("q1 times: %lld %lld\n", (long long)st.st_atime, (long long)st.st_mtime);

    // Unsupported operations
    show("mkdir", mkdir(D "d", 0755));
    show("mknod fifo", mknod(D "f", S_IFIFO | 0644, 0));
    show("symlink", symlink("q1", D "l"));
    show("rename", rename(D "q1", D "q3"));
    show("rmdir a queue", rmdir(D "q1"));
    show("unlink q1/", unlink(D "q1/"));
    show("unlink missing", unlink(D "nosuch"));

    // Someone else's queue: the sticky bit
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        setgid(65534);
        setuid(65534);
        show("nobody unlink q1", unlink(D "q1"));
        show("nobody raw mq_unlink q1", syscall(SYS_mq_unlink, "q1"));
        show("nobody libc mq_unlink q1", mq_unlink("/q1"));
        int fd = open(D "q4", O_CREAT | O_RDWR, 0600);
        show("nobody creates q4", fd);
        struct stat st;
        stat(D "q4", &st);
        printf("q4 owner %d\n", st.st_uid);
        show("nobody unlinks q4", unlink(D "q4"));
        fflush(stdout);
        _exit(0);
    }
    waitpid(c, NULL, 0);

    show("unlink q2", unlink(D "q2"));
    show("mq_open /q2 after unlink", mq_open("/q2", O_RDONLY));
    ls(D);

    // Another mount in a new IPC namespace: it is empty, with a different device number
    fflush(stdout);
    c = fork();
    if (c == 0) {
        show("unshare CLONE_NEWIPC", unshare(CLONE_NEWIPC));
        show("mount in the new ns", mount("mqueue", "/tmp/mnt/mq2", "mqueue", 0, NULL));
        ls("/tmp/mnt/mq2");
        struct stat a, b;
        stat("/tmp/mnt/mq", &a);
        stat("/tmp/mnt/mq2", &b);
        printf("other instance has its own device: %d\n", a.st_dev != b.st_dev);
        fflush(stdout);
        _exit(0);
    }
    waitpid(c, NULL, 0);
    ls(D);
    mq_unlink("/q1");
    return 0;
}
C
gcc -D_GNU_SOURCE -o mqfs mqfs.c && ./mqfs
umount mnt/mq2 2>/dev/null; umount mnt/mq

# Executing memfds and deleted files through fds (runc copies itself into a memfd and executes it
# through /proc/self/fd/<n>, buildah's chroot isolation uses fexecve): fexecve (execveat's
# AT_EMPTY_PATH), execve("/proc/self/fd/<n>"); the memfd's own fd does not count as a writer
# (Linux's alloc_file_pseudo), but reopening it for writing through /proc does (ETXTBSY);
# MFD_NOEXEC_SEAL ones cannot be executed. comm is not compared: when executing an fd Linux 6.8
# uses the fd number, and newer kernels changed it to the file name
cat > fdexec-hello.c <<'C'
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>
#include <unistd.h>
int main(int argc, char **argv)
{
    char exe[256] = "";
    readlink("/proc/self/exe", exe, sizeof exe - 1);
    char maps[512] = "";
    FILE *f = fopen("/proc/self/maps", "r");
    fgets(maps, sizeof maps, f);
    char *path = strchr(maps, '/');
    printf("  ran %s: exe=%s execfn=%s map=%s", argv[1], exe, (char *)getauxval(AT_EXECFN), path ? path : "?\n");
    return 0;
}
C
gcc -O1 -o fdexec-hello fdexec-hello.c
cat > fdexec.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sendfile.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef MFD_NOEXEC_SEAL
#define MFD_NOEXEC_SEAL 8
#endif

extern char **environ;

static int copy_to(int fd)
{
    int src = open("/tmp/fdexec-hello", O_RDONLY);
    struct stat st;
    fstat(src, &st);
    off_t off = 0;
    int n = sendfile(fd, src, &off, st.st_size);
    close(src);
    return n;
}

// Execute in a child process and report execve's error or the program's output
static void run(const char *what, int how, int fd, const char *path)
{
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        char *argv[] = {"hello", (char *)what, NULL};
        if (how == 0)
            fexecve(fd, argv, environ);
        else if (how == 1)
            execve(path, argv, environ);
        else
            syscall(SYS_execveat, fd, "", argv, environ, AT_EMPTY_PATH);
        printf("%-34s %s\n", what, strerror(errno));
        fflush(stdout);
        _exit(1);
    }
    int status;
    waitpid(c, &status, 0);
}

int main(void)
{
    char path[64];
    int m = memfd_create("hello", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    copy_to(m);
    run("memfd fexecve", 0, m, NULL);
    snprintf(path, sizeof path, "/proc/self/fd/%d", m);
    run("memfd /proc/self/fd", 1, 0, path);
    run("memfd execveat AT_EMPTY_PATH", 2, m, NULL);
    // Still executable after sealing (runc does this)
    fcntl(m, F_ADD_SEALS, F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE);
    run("sealed memfd", 0, m, NULL);
    // Reopening for writing through /proc counts as a writer
    int m2 = memfd_create("hello2", MFD_CLOEXEC);
    copy_to(m2);
    snprintf(path, sizeof path, "/proc/self/fd/%d", m2);
    int w = open(path, O_RDWR | O_CLOEXEC);
    printf("reopen memfd for write: %s\n", w >= 0 ? "ok" : strerror(errno));
    run("memfd reopened for write", 0, m2, NULL);
    close(w);
    run("memfd after closing the reopen", 0, m2, NULL);
    // MFD_NOEXEC_SEAL: no execute permission
    int m3 = memfd_create("hello3", MFD_CLOEXEC | MFD_NOEXEC_SEAL);
    copy_to(m3);
    run("memfd MFD_NOEXEC_SEAL", 0, m3, NULL);

    // A regular file already deleted
    int fd = open("/tmp/fdexec-copy", O_CREAT | O_TRUNC | O_RDWR, 0755);
    copy_to(fd);
    close(fd);
    fd = open("/tmp/fdexec-copy", O_RDONLY | O_CLOEXEC);
    unlink("/tmp/fdexec-copy");
    run("deleted file fexecve", 0, fd, NULL);
    snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
    run("deleted file /proc/self/fd", 1, 0, path);
    // A deleted file still open for writing: ETXTBSY
    int wf = open("/tmp/fdexec-copy2", O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0755);
    copy_to(wf);
    unlink("/tmp/fdexec-copy2");
    run("deleted file open for write", 0, wf, NULL);
    // An O_PATH fd, a file not yet deleted
    int p = open("/tmp/fdexec-hello", O_PATH | O_CLOEXEC);
    run("O_PATH fd", 2, p, NULL);
    int r = open("/tmp/fdexec-hello", O_RDONLY | O_CLOEXEC);
    run("regular file fexecve", 0, r, NULL);
    // Not a regular file
    int pipefd[2];
    pipe2(pipefd, O_CLOEXEC);
    run("pipe", 2, pipefd[0], NULL);
    return 0;
}
C
gcc -D_GNU_SOURCE -o fdexec fdexec.c && ./fdexec

# Time namespaces (dockerd has runc create one when it sees /proc/self/ns/time):
# unshare(CLONE_NEWTIME) creates only the one for children, with the offsets copied and set
# through /proc/<pid>/timens_offsets, which can no longer be changed once a forked child has
# entered it; the monotonic clock, boottime, /proc/uptime, sysinfo and start times the child sees
# all include the offset (/proc/stat's btime moves back by it), and waits for absolute times are
# converted by the offset; setns into it; threads and vfork stay in the original one and enter the
# new one after execve (since Linux 6.2). Only whether things differ by the offset and whether
# they are the same namespace is compared, not the exact times
cat > timens.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysinfo.h>
#include <sys/timerfd.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef CLONE_NEWTIME
#define CLONE_NEWTIME 0x80
#endif

static double now(clockid_t c)
{
    struct timespec t;
    clock_gettime(c, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static double uptime(void)
{
    double u = 0;
    FILE *f = fopen("/proc/uptime", "r");
    fscanf(f, "%lf", &u);
    fclose(f);
    return u;
}

static unsigned long long btime(void)
{
    char line[256];
    unsigned long long b = 0;
    FILE *f = fopen("/proc/stat", "r");
    while (fgets(line, sizeof line, f))
        if (sscanf(line, "btime %llu", &b) == 1)
            break;
    fclose(f);
    return b;
}

static ino_t ns_ino(const char *path)
{
    struct stat st;
    return stat(path, &st) == 0 ? st.st_ino : 0;
}

static void offsets(const char *what)
{
    char buf[256] = "";
    int fd = open("/proc/self/timens_offsets", O_RDONLY);
    int n = read(fd, buf, sizeof buf - 1);
    close(fd);
    printf("%s:\n%.*s", what, n > 0 ? n : 0, buf);
}

static void set(const char *what, const char *text)
{
    int fd = open("/proc/self/timens_offsets", O_WRONLY);
    int n = write(fd, text, strlen(text));
    printf("%-28s %s\n", what, n < 0 ? strerror(errno) : "ok");
    close(fd);
}

static int near(double a, double b) { return a - b < 1.0 && b - a < 1.0; }

static void *thread(void *arg)
{
    printf("thread stays in the old ns: %d\n", ns_ino("/proc/thread-self/ns/time") == *(ino_t *)arg);
    return NULL;
}

static int gate[2];

static void *blocked(void *arg)
{
    char x;
    read(gate[0], &x, 1);
    return arg;
}

int main(int argc, char **argv)
{
    if (argc == 3) {
        // The vfork child after execve: it has entered the one for children
        printf("after execve in the new ns: %d\n", ns_ino("/proc/self/ns/time") == (ino_t)atoll(argv[2]));
        return 0;
    }
    setvbuf(stdout, NULL, _IONBF, 0);
    ino_t old = ns_ino("/proc/self/ns/time");
    printf("same at start: %d\n", old == ns_ino("/proc/self/ns/time_for_children"));
    offsets("init offsets");
    printf("unshare: %s\n", unshare(CLONE_NEWTIME) ? strerror(errno) : "ok");
    ino_t fresh = ns_ino("/proc/self/ns/time_for_children");
    printf("self unchanged: %d, for children new: %d\n", ns_ino("/proc/self/ns/time") == old, fresh != old);
    offsets("copied offsets");
    set("bad clock", "realtime 1 0\n");
    set("bad nsec", "monotonic 1 1000000000\n");
    set("missing field", "monotonic 1\n");
    set("before boot", "boottime -100000000 0\n");
    set("set", "monotonic 86400 0\nboottime 3600 500000000\n");
    offsets("new offsets");
    set("one line", "7 3600 500000000");
    offsets("after one line");

    // nobody has no CAP_SYS_TIME
    pid_t c = fork();
    if (c == 0) {
        setuid(65534);
        set("nobody writes", "monotonic 1 0\n");
        _exit(0);
    }
    waitpid(c, NULL, 0);

    double m0 = now(CLOCK_MONOTONIC), b0 = now(CLOCK_BOOTTIME), r0 = now(CLOCK_REALTIME), u0 = uptime();
    unsigned long long bt0 = btime();
    struct sysinfo si0;
    sysinfo(&si0);
    int ready[2];
    pipe(ready);
    c = fork();
    if (c == 0) {
        printf("child in the new ns: %d\n", ns_ino("/proc/self/ns/time") == fresh);
        printf("monotonic +86400: %d\n", near(now(CLOCK_MONOTONIC) - m0, 86400));
        printf("monotonic raw, coarse +86400: %d %d\n", near(now(CLOCK_MONOTONIC_RAW) - m0, 86400),
               near(now(CLOCK_MONOTONIC_COARSE) - m0, 86400));
        printf("boottime +3600.5: %d\n", near(now(CLOCK_BOOTTIME) - b0, 3600.5));
        printf("realtime unchanged: %d\n", near(now(CLOCK_REALTIME), r0));
        printf("uptime +3600.5: %d\n", near(uptime() - u0, 3600.5));
        struct sysinfo si;
        sysinfo(&si);
        printf("sysinfo uptime +3600: %d\n", si.uptime - si0.uptime >= 3600 && si.uptime - si0.uptime <= 3602);
        // The start time (clock ticks) also follows the boottime offset
        unsigned long long start = 0;
        FILE *f = fopen("/proc/self/stat", "r");
        fscanf(f, "%*d %*s %*c %*d %*d %*d %*d %*d %*u %*u %*u %*u %*u %*u %*u %*d %*d %*d %*d %*d %*d %llu", &start);
        fclose(f);
        printf("start time near uptime: %d\n", near(start / (double)sysconf(_SC_CLK_TCK), uptime()));
        // btime moves back by the offset (whole seconds of boot time minus 3600.5), so btime plus
        // the start time is still the wall-clock start (ps -o lstart)
        unsigned long long bt = btime();
        printf("btime -3600.5: %d\n", bt0 - bt == 3600 || bt0 - bt == 3601);
        double lag = now(CLOCK_REALTIME) - (double)(bt + start / sysconf(_SC_CLK_TCK));
        printf("btime + start time near realtime: %d\n", lag > -1 && lag < 3);
        // Waits for absolute times are converted by the offset, and do not wait an extra day
        struct timespec t;
        clock_gettime(CLOCK_MONOTONIC, &t);
        t.tv_nsec += 100000000;
        if (t.tv_nsec >= 1000000000) {
            t.tv_sec++;
            t.tv_nsec -= 1000000000;
        }
        double s = now(CLOCK_REALTIME);
        clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &t, NULL);
        printf("clock_nanosleep abs: %d\n", now(CLOCK_REALTIME) - s < 2);
        int tfd = timerfd_create(CLOCK_BOOTTIME, 0);
        struct itimerspec its = {0};
        clock_gettime(CLOCK_BOOTTIME, &its.it_value);
        its.it_value.tv_sec += 1;
        timerfd_settime(tfd, TFD_TIMER_ABSTIME, &its, NULL);
        unsigned long long n;
        s = now(CLOCK_REALTIME);
        read(tfd, &n, sizeof n);
        printf("timerfd abs: %d\n", now(CLOCK_REALTIME) - s < 3);
        timer_t tm;
        struct sigevent ev = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGUSR1};
        sigset_t set;
        sigemptyset(&set);
        sigaddset(&set, SIGUSR1);
        sigprocmask(SIG_BLOCK, &set, NULL);
        timer_create(CLOCK_MONOTONIC, &ev, &tm);
        clock_gettime(CLOCK_MONOTONIC, &its.it_value);
        its.it_value.tv_sec += 1;
        timer_settime(tm, TIMER_ABSTIME, &its, NULL);
        s = now(CLOCK_REALTIME);
        int sig;
        sigwait(&set, &sig);
        printf("timer_settime abs: %d\n", now(CLOCK_REALTIME) - s < 3);
        int word = 0;
        clock_gettime(CLOCK_MONOTONIC, &t);
        t.tv_sec += 1;
        s = now(CLOCK_REALTIME);
        long r = syscall(SYS_futex, &word, 9 /* FUTEX_WAIT_BITSET */ | 128, 0, &t, NULL, ~0u);
        printf("futex abs: %s %d\n", r ? strerror(errno) : "woken", now(CLOCK_REALTIME) - s < 3);
        char x = 1;
        write(ready[1], &x, 1);
        pause();
        _exit(0);
    }
    char x;
    read(ready[0], &x, 1);
    set("after a child entered", "monotonic 1 0\n");
    printf("parent unchanged: %d\n", now(CLOCK_MONOTONIC) - m0 < 60);

    // Threads stay in the original one; a vfork child enters the new one only after execve
    pthread_t th;
    pthread_create(&th, NULL, thread, &old);
    pthread_join(th, NULL);
    char arg[32];
    snprintf(arg, sizeof arg, "%llu", (unsigned long long)fresh);
    pid_t v = vfork();
    if (v == 0) {
        execl("/proc/self/exe", "timens", "show", arg, (char *)NULL);
        _exit(1);
    }
    waitpid(v, NULL, 0);

    // setns into the one the child is in
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/ns/time", c);
    int nsfd = open(path, O_RDONLY);
    pipe(gate);
    pthread_create(&th, NULL, blocked, NULL);
    printf("setns while threaded: %s\n", setns(nsfd, CLONE_NEWTIME) ? strerror(errno) : "ok");
    write(gate[1], "x", 1);
    pthread_join(th, NULL);
    printf("setns: %s\n", setns(nsfd, CLONE_NEWTIME) ? strerror(errno) : "ok");
    printf("now in it: %d %d\n", ns_ino("/proc/self/ns/time") == fresh, ns_ino("/proc/self/ns/time_for_children") == fresh);
    printf("monotonic after setns +86400: %d\n", near(now(CLOCK_MONOTONIC) - m0, 86400) || now(CLOCK_MONOTONIC) - m0 > 86400);
    kill(c, SIGKILL);
    waitpid(c, NULL, 0);
    return 0;
}
C
gcc -D_GNU_SOURCE -pthread -o timens timens.c && ./timens

# /proc/<pid> files that take numbers (oom_score_adj, coredump_filter): the oom_score_adj value
# runc writes ends with a NUL (a string from a netlink message), and Linux copies it into a
# 12-byte buffer up to the NUL and parses it with kstrtoint base 0; beyond 32 bits it is ERANGE
cat > procnum.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static void put(const char *file, const char *what, const char *data, int len)
{
    char path[64], buf[32] = "";
    snprintf(path, sizeof path, "/proc/self/%s", file);
    int fd = open(path, O_WRONLY);
    int n = write(fd, data, len);
    close(fd);
    fd = open(path, O_RDONLY);
    read(fd, buf, sizeof buf - 1);
    close(fd);
    buf[strcspn(buf, "\n")] = 0;
    printf("%s %-16s %-24s now %s\n", file, what, n < 0 ? strerror(errno) : n == len ? "ok" : "short", buf);
}

int main(void)
{
    put("oom_score_adj", "nul", "100\0", 4);
    put("oom_score_adj", "nul then junk", "500\0junk", 8);
    put("oom_score_adj", "hex", "0x10", 4);
    put("oom_score_adj", "octal", "010", 3);
    put("oom_score_adj", "spaces", " 7 \n", 4);
    put("oom_score_adj", "too big", "1001", 4);
    put("oom_score_adj", "overflow", "2147483648", 10);
    put("oom_score_adj", "not a number", "abc", 3);
    put("oom_score_adj", "13th byte", "0000000000001x", 14);
    put("oom_score_adj", "empty", "\0", 1);
    put("coredump_filter", "nul", "0x23\0", 5);
    put("coredump_filter", "negative", "-1", 2);
    put("coredump_filter", "overflow", "4294967296", 10);
    put("coredump_filter", "plus octal", "+017", 4);
    return 0;
}
C
gcc -o procnum procnum.c && ./procnum
