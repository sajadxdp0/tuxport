/*
 * Every kind of system call that can wait in the kernel really waits once (the kernel/process
 * map): those with a timeout use a very short one, and for the others another thread or a child
 * process makes them return a little later. The kernel layer of debug builds checks, every time
 * the BKL is released, that no kernel object is borrowed (crate::cell), so cargo test running this
 * program checks all these wait paths without having to hit concurrency. Run in the store (it
 * needs /tmp). Each item that passes prints a line "ok <name>"; a failure prints the reason and
 * exits with 1; when all pass it prints "all ok" at the end.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/file.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/msg.h>
#include <sys/prctl.h>
#include <sys/select.h>
#include <sys/sem.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            printf("FAIL: ");                                                                      \
            printf(__VA_ARGS__);                                                                   \
            printf(" (errno %d)\n", errno);                                                        \
            exit(1);                                                                               \
        }                                                                                          \
    } while (0)

static void ok(const char *name)
{
    printf("ok %s\n", name);
}

static const struct timespec short_wait = {0, 5000000};

/* Calls f(arg) a little later in another thread: by then the main thread is already waiting */
struct later {
    pthread_t thread;
    void (*f)(long);
    long arg;
};

static void *later_main(void *p)
{
    struct later *l = p;
    usleep(30000);
    l->f(l->arg);
    return NULL;
}

static void after(struct later *l, void (*f)(long), long arg)
{
    l->f = f;
    l->arg = arg;
    CHECK(pthread_create(&l->thread, NULL, later_main, l) == 0, "pthread_create");
}

static void join(struct later *l)
{
    pthread_join(l->thread, NULL);
}

static void write_byte(long fd)
{
    write((int)fd, "x\n", 2);
}

static void write_eventfd(long fd)
{
    uint64_t one = 1;
    write((int)fd, &one, sizeof one);
}

static void unlock_flock(long fd)
{
    flock((int)fd, LOCK_UN);
}

static void unlock_ofd(long fd)
{
    struct flock fl = {.l_type = F_UNLCK, .l_whence = SEEK_SET};
    fcntl((int)fd, F_OFD_SETLK, &fl);
}

static void create_file(long unused)
{
    (void)unused;
    close(open("/tmp/waits-dir/new", O_CREAT | O_WRONLY, 0644));
}

static pid_t main_tid;

static void send_usr1(long unused)
{
    (void)unused;
    syscall(SYS_tgkill, getpid(), main_tid, SIGUSR1);
}

static void send_alrm(long unused)
{
    (void)unused;
    syscall(SYS_tgkill, getpid(), main_tid, SIGALRM);
}

static void on_alrm(int sig)
{
    (void)sig;
}

static void write_kmsg(long unused)
{
    (void)unused;
    int fd = open("/dev/kmsg", O_WRONLY);
    write(fd, "waits\n", 6);
    close(fd);
}

static int msq;

static void send_msg(long unused)
{
    (void)unused;
    struct {
        long type;
        char text[4];
    } m = {1, "hi"};
    msgsnd(msq, &m, sizeof m.text, 0);
}

/* seccomp's notification fd: waits in NOTIF_RECV and answers 42 */
#define SECCOMP_SET_MODE_FILTER 1
#define FLAG_NEW_LISTENER 8
#define RET_USER_NOTIF 0x7fc00000U
#define RET_ALLOW 0x7fff0000U
#define NOTIF_RECV 0xc0502100UL
#define NOTIF_SEND 0xc0182101UL

struct notif {
    uint64_t id;
    uint32_t pid, flags;
    int nr;
    uint32_t arch;
    uint64_t ip, args[6];
};

struct notif_resp {
    uint64_t id;
    int64_t val;
    int32_t error;
    uint32_t flags;
};

struct filter {
    uint16_t code;
    uint8_t jt, jf;
    uint32_t k;
};

struct fprog {
    unsigned short len;
    struct filter *filter;
};

static void *answer(void *arg)
{
    int listener = (int)(long)arg;
    struct notif n;
    memset(&n, 0, sizeof n);
    if (ioctl(listener, NOTIF_RECV, &n) != 0)
        return (void *)1;
    struct notif_resp r = {n.id, 42, 0, 0};
    return (void *)(long)(ioctl(listener, NOTIF_SEND, &r) != 0);
}

/* Lease breaking: when the child opens for writing, it waits for the parent to give up its read
 * lease */
static void lease_break(void)
{
    signal(SIGIO, SIG_IGN); /* the signal that notifies the holder of a break */
    int fd = open("/tmp/waits-lease", O_CREAT | O_RDONLY, 0644);
    CHECK(fd >= 0 && fcntl(fd, F_SETLEASE, F_RDLCK) == 0, "F_SETLEASE");
    pid_t pid = fork();
    if (pid == 0)
        _exit(open("/tmp/waits-lease", O_WRONLY) >= 0 ? 0 : 2);
    /* While breaking, F_GETLEASE reports the type it is being lowered to: the child is already
     * waiting in open */
    for (int i = 0; i < 5000 && fcntl(fd, F_GETLEASE) != F_UNLCK; i++)
        usleep(1000);
    CHECK(fcntl(fd, F_GETLEASE) == F_UNLCK, "the lease is not being broken");
    CHECK(fcntl(fd, F_SETLEASE, F_UNLCK) == 0, "F_SETLEASE F_UNLCK");
    int st;
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) && WEXITSTATUS(st) == 0,
          "open breaking the lease: status %#x", st);
    close(fd);
    signal(SIGIO, SIG_DFL);
    ok("lease break");
}

/* File position lock (f_pos_lock): two threads at once read 2 bytes each from the same open file,
 * and the positions read neither repeat nor skip. While one thread reads a host file without the
 * BKL, the other's read waits for it to finish; with this many reads they always meet */
#define POS_SLOTS 4000
static int pos_fd;
static int pos_seen[POS_SLOTS];

static void *read_slots(void *arg)
{
    (void)arg;
    uint16_t slot;
    while (read(pos_fd, &slot, sizeof slot) == sizeof slot)
        if (slot < POS_SLOTS)
            __atomic_fetch_add(&pos_seen[slot], 1, __ATOMIC_RELAXED);
    return NULL;
}

static void file_position_lock(void)
{
    uint16_t slots[POS_SLOTS];
    for (int i = 0; i < POS_SLOTS; i++)
        slots[i] = i;
    int fd = open("/tmp/waits-pos", O_CREAT | O_TRUNC | O_RDWR, 0644);
    CHECK(fd >= 0 && write(fd, slots, sizeof slots) == sizeof slots, "write the slots");
    pos_fd = open("/tmp/waits-pos", O_RDONLY);
    CHECK(pos_fd >= 0, "open the slots");
    pthread_t t;
    CHECK(pthread_create(&t, NULL, read_slots, NULL) == 0, "pthread_create");
    read_slots(NULL);
    pthread_join(t, NULL);
    for (int i = 0; i < POS_SLOTS; i++)
        CHECK(pos_seen[i] == 1, "slot %d read %d times", i, pos_seen[i]);
    close(pos_fd);
    close(fd);
    ok("file position lock");
}

/* AIO: IOCB_CMD_POLL on a pipe is completed by another thread writing the pipe, and io_getevents
 * without a timeout waits until then */
struct iocb {
    uint64_t data;
    uint32_t key, rw_flags;
    uint16_t opcode;
    int16_t reqprio;
    uint32_t fildes;
    uint64_t buf, nbytes;
    int64_t offset;
    uint64_t reserved2;
    uint32_t flags, resfd;
};

struct io_event {
    uint64_t data, obj;
    int64_t res, res2;
};

#define IOCB_CMD_POLL 5

static void aio_completion(void)
{
    int p[2];
    CHECK(pipe(p) == 0, "pipe");
    unsigned long ctx = 0;
    CHECK(syscall(SYS_io_setup, 1, &ctx) == 0, "io_setup");
    struct iocb cb = {.data = 7, .opcode = IOCB_CMD_POLL, .fildes = p[0], .buf = POLLIN};
    struct iocb *list[] = {&cb};
    CHECK(syscall(SYS_io_submit, ctx, 1, list) == 1, "io_submit");
    struct later l;
    after(&l, write_byte, p[1]);
    struct io_event e;
    CHECK(syscall(SYS_io_getevents, ctx, 1, 1, &e, NULL) == 1 && e.data == 7 &&
              (e.res & POLLIN),
          "io_getevents: res %lld", (long long)e.res);
    join(&l);
    syscall(SYS_io_destroy, ctx);
    close(p[0]);
    close(p[1]);
    ok("AIO poll completion");
}

static void seccomp_listener(void)
{
    pid_t pid = fork();
    if (pid == 0) {
        struct filter f[] = {
            {0x20, 0, 0, 0}, /* ld [nr] */
            {0x15, 0, 1, SYS_getppid},
            {0x06, 0, 0, RET_USER_NOTIF},
            {0x06, 0, 0, RET_ALLOW},
        };
        struct fprog p = {4, f};
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        int listener = syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, FLAG_NEW_LISTENER, &p);
        if (listener < 0)
            _exit(2);
        pthread_t t;
        pthread_create(&t, NULL, answer, (void *)(long)listener);
        usleep(30000);
        long r = syscall(SYS_getppid);
        void *failed;
        pthread_join(t, &failed);
        _exit(r == 42 && !failed ? 0 : 3);
    }
    int st;
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) && WEXITSTATUS(st) == 0,
          "seccomp listener: status %#x", st);
    ok("seccomp NOTIF_RECV");
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    main_tid = gettid();
    struct later l;
    int p[2];

    /* Those that only wait for a timeout */
    int ep = epoll_create1(0);
    struct epoll_event ev;
    CHECK(epoll_wait(ep, &ev, 1, 5) == 0, "epoll_wait");
    ok("epoll_wait timeout");
    CHECK(pipe(p) == 0, "pipe");
    struct pollfd pf = {p[0], POLLIN, 0};
    CHECK(ppoll(&pf, 1, &short_wait, NULL) == 0, "ppoll");
    fd_set rd;
    FD_ZERO(&rd);
    FD_SET(p[0], &rd);
    CHECK(pselect(p[0] + 1, &rd, NULL, NULL, &short_wait, NULL) == 0, "pselect");
    ok("ppoll and pselect timeouts");
    sigset_t usr;
    sigemptyset(&usr);
    sigaddset(&usr, SIGUSR1);
    sigaddset(&usr, SIGUSR2);
    sigprocmask(SIG_BLOCK, &usr, NULL);
    sigset_t usr2;
    sigemptyset(&usr2);
    sigaddset(&usr2, SIGUSR2);
    CHECK(sigtimedwait(&usr2, NULL, &short_wait) < 0 && errno == EAGAIN, "sigtimedwait");
    ok("sigtimedwait timeout");
    struct mq_attr attr = {.mq_maxmsg = 4, .mq_msgsize = 16};
    mqd_t mq = mq_open("/tuxport-waits", O_CREAT | O_RDWR, 0600, &attr);
    CHECK(mq != (mqd_t)-1, "mq_open");
    struct timespec now;
    clock_gettime(CLOCK_REALTIME, &now);
    now.tv_nsec += 5000000;
    if (now.tv_nsec >= 1000000000) {
        now.tv_sec++;
        now.tv_nsec -= 1000000000;
    }
    char buf[64];
    CHECK(mq_timedreceive(mq, buf, sizeof buf, NULL, &now) < 0 && errno == ETIMEDOUT,
          "mq_timedreceive");
    mq_close(mq);
    mq_unlink("/tuxport-waits");
    ok("mq_timedreceive timeout");
    unsigned long ctx = 0;
    CHECK(syscall(SYS_io_setup, 1, &ctx) == 0, "io_setup");
    char events[32];
    CHECK(syscall(SYS_io_getevents, ctx, 1, 1, events, &short_wait) == 0, "io_getevents");
    syscall(SYS_io_destroy, ctx);
    ok("io_getevents timeout");
    int sem = semget(IPC_PRIVATE, 1, 0600);
    struct sembuf down = {0, -1, 0};
    CHECK(semtimedop(sem, &down, 1, &short_wait) < 0 && errno == EAGAIN, "semtimedop");
    semctl(sem, 0, IPC_RMID);
    ok("semtimedop timeout");
    /* A futex in a writable shared file mapping takes the path that holds the lock */
    int mfd = memfd_create("waits", 0);
    CHECK(mfd >= 0 && ftruncate(mfd, 4096) == 0, "memfd");
    int *word = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    CHECK(word != MAP_FAILED, "mmap");
    CHECK(syscall(SYS_futex, word, 0 /* FUTEX_WAIT */, 0, &short_wait, NULL, 0) < 0 &&
              errno == ETIMEDOUT,
          "futex in a shared file mapping");
    ok("futex timeout");
    /* When no answer is readable, netlink waits for SO_RCVTIMEO */
    int nl = socket(AF_NETLINK, SOCK_RAW, 0 /* NETLINK_ROUTE */);
    struct timeval nl_wait = {0, 5000};
    CHECK(nl >= 0 && setsockopt(nl, SOL_SOCKET, SO_RCVTIMEO, &nl_wait, sizeof nl_wait) == 0,
          "netlink socket");
    CHECK(recv(nl, buf, sizeof buf, 0) < 0 && errno == EAGAIN, "netlink recv");
    close(nl);
    ok("netlink recv timeout");

    /* Wait for another thread */
    after(&l, write_byte, p[1]);
    CHECK(read(p[0], buf, sizeof buf) == 2, "pipe read");
    join(&l);
    ok("pipe read");
    int efd = eventfd(0, 0);
    after(&l, write_eventfd, efd);
    uint64_t count;
    CHECK(read(efd, &count, sizeof count) == 8 && count == 1, "eventfd read");
    join(&l);
    ok("eventfd read");
    int sv[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "socketpair");
    after(&l, write_byte, sv[1]);
    CHECK(recv(sv[0], buf, sizeof buf, 0) == 2, "recv");
    join(&l);
    ok("unix recv");
    int tfd = timerfd_create(CLOCK_MONOTONIC, 0);
    struct itimerspec its = {.it_value = {0, 10000000}};
    timerfd_settime(tfd, 0, &its, NULL);
    CHECK(read(tfd, &count, sizeof count) == 8 && count == 1, "timerfd read");
    ok("timerfd read");
    sigset_t usr1;
    sigemptyset(&usr1);
    sigaddset(&usr1, SIGUSR1);
    int sfd = signalfd(-1, &usr1, 0);
    after(&l, send_usr1, 0);
    struct signalfd_siginfo si;
    CHECK(read(sfd, &si, sizeof si) == sizeof si && si.ssi_signo == SIGUSR1, "signalfd read");
    join(&l);
    ok("signalfd read");
    msq = msgget(IPC_PRIVATE, 0600);
    after(&l, send_msg, 0);
    struct {
        long type;
        char text[4];
    } m;
    CHECK(msgrcv(msq, &m, sizeof m.text, 0, 0) == 4, "msgrcv");
    join(&l);
    msgctl(msq, IPC_RMID, NULL);
    ok("msgrcv");
    mkdir("/tmp/waits-dir", 0755);
    unlink("/tmp/waits-dir/new");
    int ifd = inotify_init();
    inotify_add_watch(ifd, "/tmp/waits-dir", IN_CREATE);
    after(&l, create_file, 0);
    CHECK(read(ifd, buf, sizeof buf) > 0, "inotify read");
    join(&l);
    ok("inotify read");
    int a = open("/tmp/waits-lock", O_CREAT | O_RDWR, 0644);
    int b = open("/tmp/waits-lock", O_RDWR);
    CHECK(flock(a, LOCK_EX) == 0, "flock");
    after(&l, unlock_flock, a);
    CHECK(flock(b, LOCK_EX) == 0, "flock waits");
    join(&l);
    flock(b, LOCK_UN);
    ok("flock");
    struct flock fl = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
    CHECK(fcntl(a, F_OFD_SETLK, &fl) == 0, "OFD lock");
    after(&l, unlock_ofd, a);
    CHECK(fcntl(b, F_OFD_SETLKW, &fl) == 0, "OFD lock waits");
    join(&l);
    ok("F_OFD_SETLKW");
    lease_break();
    file_position_lock();
    aio_completion();
    int master = posix_openpt(O_RDWR | O_NOCTTY);
    CHECK(master >= 0 && grantpt(master) == 0 && unlockpt(master) == 0, "posix_openpt");
    int slave = open(ptsname(master), O_RDWR | O_NOCTTY);
    CHECK(slave >= 0, "open the slave");
    after(&l, write_byte, master);
    CHECK(read(slave, buf, sizeof buf) == 2, "pty read");
    join(&l);
    ok("pty read");
    pid_t child = fork();
    if (child == 0) {
        usleep(30000);
        _exit(5);
    }
    int st;
    CHECK(waitpid(child, &st, 0) == child && WEXITSTATUS(st) == 5, "waitpid");
    ok("waitpid");
    seccomp_listener();
    /* syslog reads: the log of a newly started Tuxport is empty, so wait until a signal arrives
     * (the handler has no SA_RESTART: EINTR); then wait until someone writes /dev/kmsg */
    struct sigaction sa = {.sa_handler = on_alrm};
    sigaction(SIGALRM, &sa, NULL);
    after(&l, send_alrm, 0);
    CHECK(syscall(SYS_syslog, 2, buf, (int)sizeof buf) == -1 && errno == EINTR, "syslog read");
    join(&l);
    after(&l, write_kmsg, 0);
    CHECK(syscall(SYS_syslog, 2, buf, (int)sizeof buf) > 0, "syslog read woken by a record");
    join(&l);
    ok("syslog read");
    /* /proc/kmsg shares its position with syslog reads: the reads above took everything, so wait
     * for the next message */
    int pk = open("/proc/kmsg", O_RDONLY);
    CHECK(pk >= 0, "open /proc/kmsg");
    after(&l, write_kmsg, 0);
    CHECK(read(pk, buf, sizeof buf) > 0, "/proc/kmsg read");
    join(&l);
    close(pk);
    ok("/proc/kmsg read");
    /* /dev/kmsg: each open file has its own position; wait starting from the end */
    int dk = open("/dev/kmsg", O_RDONLY);
    CHECK(dk >= 0 && lseek(dk, 0, SEEK_END) == 0, "open /dev/kmsg");
    after(&l, write_kmsg, 0);
    CHECK(read(dk, buf, sizeof buf) > 0, "/dev/kmsg read");
    join(&l);
    close(dk);
    ok("/dev/kmsg read");
    printf("all ok\n");
    return 0;
}
