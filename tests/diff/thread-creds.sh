# docker-run-options: --privileged
# Credentials belong to each thread: libc's seteuid changes every thread's, but a thread's raw
# setfsuid and setresuid change only its own: the owner of the files it creates, its permission
# checks, its /proc/<pid>/task/<tid> status and owner, capget, SO_PEERCRED of its connection,
# PRIO_USER, who may signal it and read its maps or kcmp it, while the process (its main thread)
# keeps root's; a thread it creates starts with its credentials, and when it calls execve they
# become the process's. Then PTRACE_TRACEME records the caller's own credentials as the tracer's
# (Linux's ptrace_link since the fix for CVE-2019-13272), so an unprivileged child traced by a root
# parent gains nothing from a setuid program
set -e
rm -rf /opt/tc
mkdir -p /opt/tc
chmod 1777 /opt/tc
cd /opt/tc

cat > threads.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static pthread_barrier_t barrier;
static volatile pid_t worker_tid;
static int pipefd[2];
static const char NAME[] = "thread-creds";

static void step(void)
{
    pthread_barrier_wait(&barrier);
}

static const char *result(long r)
{
    return r < 0 ? strerror(errno) : "ok";
}

static socklen_t address(struct sockaddr_un *a)
{
    memset(a, 0, sizeof *a);
    a->sun_family = AF_UNIX;
    memcpy(a->sun_path + 1, NAME, sizeof NAME - 1);
    return offsetof(struct sockaddr_un, sun_path) + 1 + sizeof NAME - 1;
}

/* The Uid and Gid lines of a status file, tabs as spaces */
static void status_ids(const char *what, const char *path)
{
    FILE *f = fopen(path, "r");
    char line[256];
    while (f && fgets(line, sizeof line, f)) {
        if (strncmp(line, "Uid:", 4) && strncmp(line, "Gid:", 4))
            continue;
        for (char *c = line; *c; c++)
            if (*c == '\t')
                *c = ' ';
        printf("%s %s", what, line);
    }
    if (f)
        fclose(f);
}

static void *other(void *arg)
{
    char c;
    read(pipefd[0], &c, 1);
    printf("other thread after main's seteuid: euid %d\n", (int)geteuid());
    return arg;
}

static void *created(void *arg)
{
    printf("thread created by the worker: uid %d\n", (int)getuid());
    return arg;
}

/* In a process of its own: a thread with other IDs calls execve */
static void *execs(void *arg)
{
    syscall(SYS_setresgid, 4244, 4244, 4244);
    syscall(SYS_setresuid, 4244, 4244, 4244);
    execl("/opt/tc/plainid", "plainid", (char *)0);
    return arg;
}

static void *worker(void *arg)
{
    worker_tid = syscall(SYS_gettid);
    /* Only this thread's filesystem IDs */
    syscall(SYS_setfsgid, 4242);
    syscall(SYS_setfsuid, 4242);
    close(open("/opt/tc/by-worker", O_CREAT | O_WRONLY, 0644));
    step();
    step();
    /* Every ID of this thread, by raw system calls */
    syscall(SYS_setresgid, 4242, 4242, 4242);
    syscall(SYS_setresuid, 4242, 4242, 4242);
    printf("worker: uid %d euid %d\n", (int)getuid(), (int)geteuid());
    pthread_t t;
    pthread_create(&t, NULL, created, NULL);
    pthread_join(t, NULL);
    int fd = open("/opt/tc/secret", O_RDONLY);
    printf("worker opens a root-only file: %s\n", result(fd));
    if (fd >= 0)
        close(fd);
    struct sockaddr_un a;
    socklen_t len = address(&a);
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    printf("worker connects: %s\n", result(connect(s, (struct sockaddr *)&a, len)));
    step();
    step();
    close(s);
    return arg;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);

    /* libc's set*id functions change every thread */
    pthread_t t;
    pipe(pipefd);
    pthread_create(&t, NULL, other, NULL);
    printf("seteuid: %s\n", result(seteuid(4243)));
    write(pipefd[1], "x", 1);
    pthread_join(t, NULL);
    printf("seteuid back: %s, euid %d\n", result(seteuid(0)), (int)geteuid());

    close(open("/opt/tc/secret", O_CREAT | O_WRONLY, 0600));
    struct sockaddr_un a;
    socklen_t len = address(&a);
    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    bind(listener, (struct sockaddr *)&a, len);
    listen(listener, 1);
    pthread_barrier_init(&barrier, NULL, 2);
    pthread_create(&t, NULL, worker, NULL);

    step();
    close(open("/opt/tc/by-main", O_CREAT | O_WRONLY, 0644));
    struct stat st;
    stat("/opt/tc/by-worker", &st);
    printf("created by the worker: uid %d gid %d\n", (int)st.st_uid, (int)st.st_gid);
    stat("/opt/tc/by-main", &st);
    printf("created by main: uid %d gid %d\n", (int)st.st_uid, (int)st.st_gid);
    char task[64], status[80];
    snprintf(task, sizeof task, "/proc/self/task/%d", (int)worker_tid);
    snprintf(status, sizeof status, "%s/status", task);
    status_ids("process", "/proc/self/status");
    status_ids("worker", status);
    step();

    step();
    printf("main: uid %d euid %d\n", (int)getuid(), (int)geteuid());
    int fd = open("/opt/tc/secret", O_RDONLY);
    printf("main opens the root-only file: %s\n", result(fd));
    close(fd);
    status_ids("process", "/proc/self/status");
    status_ids("worker", status);
    /* The ID changes set the process's dumpable flag to fs.suid_dumpable (0 by default, 2 on CI's
     * runner); dumpable, its files belong to the threads */
    int suid_dumpable = -1;
    FILE *sd = fopen("/proc/sys/fs/suid_dumpable", "r");
    if (sd) {
        if (fscanf(sd, "%d", &suid_dumpable) != 1)
            suid_dumpable = -1;
        fclose(sd);
    }
    printf("dumpable as fs.suid_dumpable says: %d\n", prctl(PR_GET_DUMPABLE) == suid_dumpable);
    prctl(PR_SET_DUMPABLE, 1);
    stat(task, &st);
    printf("worker's task directory: uid %d\n", (int)st.st_uid);
    stat("/proc/self/", &st);
    printf("process directory: uid %d\n", (int)st.st_uid);

    struct {
        uint32_t version;
        int pid;
    } header = {0x20080522, worker_tid};
    struct {
        uint32_t effective, permitted, inheritable;
    } data[2];
    syscall(SYS_capget, &header, data);
    printf("worker's capabilities: %#x %#x\n", data[0].effective, data[0].permitted);
    header.pid = 0;
    syscall(SYS_capget, &header, data);
    printf("main has capabilities: %s\n", data[0].effective ? "yes" : "no");

    int c = accept(listener, NULL, NULL);
    struct ucred u;
    socklen_t ulen = sizeof u;
    getsockopt(c, SOL_SOCKET, SO_PEERCRED, &u, &ulen);
    printf("peer of the worker's connection: uid %d gid %d\n", (int)u.uid, (int)u.gid);
    close(c);

    printf("PRIO_USER: %s\n", result(setpriority(PRIO_USER, 4242, 7)));
    printf("nice: worker %d, main %d\n", getpriority(PRIO_PROCESS, worker_tid),
           getpriority(PRIO_PROCESS, 0));

    /* Another process of the worker's user may signal the worker, not the process */
    pid_t me = getpid();
    pid_t child = fork();
    if (child == 0) {
        setresgid(4242, 4242, 4242);
        setresuid(4242, 4242, 4242);
        printf("tgkill to the worker: %s\n", result(syscall(SYS_tgkill, me, worker_tid, 0)));
        printf("tgkill to main: %s\n", result(syscall(SYS_tgkill, me, me, 0)));
        printf("kill to the process: %s\n", result(kill(me, 0)));
        char path[80];
        snprintf(path, sizeof path, "/proc/%d/task/%d/maps", (int)me, (int)worker_tid);
        int maps = open(path, O_RDONLY);
        printf("worker's maps: %s\n", result(maps));
        snprintf(path, sizeof path, "/proc/%d/maps", (int)me);
        printf("process's maps: %s\n", result(open(path, O_RDONLY)));
        /* KCMP_VM */
        printf("kcmp of the worker: %s\n", result(syscall(SYS_kcmp, worker_tid, worker_tid, 1, 0, 0)));
        printf("kcmp of main: %s\n", result(syscall(SYS_kcmp, me, me, 1, 0, 0)));
        _exit(0);
    }
    waitpid(child, NULL, 0);
    step();
    pthread_join(t, NULL);

    child = fork();
    if (child == 0) {
        pthread_create(&t, NULL, execs, NULL);
        pthread_join(t, NULL);
        _exit(1);
    }
    waitpid(child, NULL, 0);
    return 0;
}
C

cat > showid.c <<'C'
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv)
{
    if (strstr(argv[0], "plainid")) {
        printf("after execve from the thread: uid %d euid %d, ", (int)getuid(), (int)geteuid());
        FILE *f = fopen("/proc/self/status", "r");
        char line[256];
        while (fgets(line, sizeof line, f))
            if (strncmp(line, "Uid:", 4) == 0) {
                for (char *c = line; *c; c++)
                    if (*c == '\t')
                        *c = ' ';
                printf("%s", line);
            }
        return 0;
    }
    printf("euid %d\n", (int)geteuid());
    return 0;
}
C

cat > traceme.c <<'C'
#define _GNU_SOURCE
#include <stdio.h>
#include <sys/ptrace.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char **argv)
{
    int traced = argc > 1;
    printf("%s: ", traced ? "traced by its root parent" : "untraced");
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        setresgid(65534, 65534, 65534);
        setresuid(65534, 65534, 65534);
        if (traced)
            ptrace(PTRACE_TRACEME, 0, 0, 0);
        execl("/opt/tc/showid", "showid", (char *)0);
        _exit(127);
    }
    int st;
    waitpid(c, &st, 0);
    if (WIFSTOPPED(st)) {
        ptrace(PTRACE_DETACH, c, 0, 0);
        waitpid(c, &st, 0);
    }
    return 0;
}
C

gcc -O1 -pthread -o threads threads.c
gcc -O1 -o showid showid.c
cp showid plainid
gcc -O1 -o traceme traceme.c
chmod 4755 showid
./threads
./traceme
./traceme traced
cd /
rm -rf /opt/tc
