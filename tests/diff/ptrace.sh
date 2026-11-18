# ptrace and process_vm_readv/writev: a small tracer checks stops, continues, events and registers
# one by one, then strace traces a few programs
set -e
cd /tmp
cat > tracer.c <<'C'
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef NT_ARM_SYSTEM_CALL
#define NT_ARM_SYSTEM_CALL 0x404
#endif
#ifndef NT_ARM_HW_BREAK
#define NT_ARM_HW_BREAK 0x402
#endif

/* aarch64's user_pt_regs */
struct regs {
    unsigned long long x[31], sp, pc, pstate;
};

static const char *describe(int st)
{
    static char b[64];
    if (WIFEXITED(st))
        snprintf(b, sizeof b, "exited %d", WEXITSTATUS(st));
    else if (WIFSIGNALED(st))
        snprintf(b, sizeof b, "killed by %d", WTERMSIG(st));
    else if (WIFSTOPPED(st))
        snprintf(b, sizeof b, "stopped %#x event %d", WSTOPSIG(st), st >> 16);
    else
        snprintf(b, sizeof b, "status %#x", st);
    return b;
}

static int wait_for(pid_t pid)
{
    int st;
    if (waitpid(pid, &st, __WALL) != pid) {
        perror("waitpid");
        exit(1);
    }
    return st;
}

static void regs(pid_t pid, struct regs *r, int set)
{
    struct iovec iov = {r, sizeof *r};
    if (ptrace(set ? PTRACE_SETREGSET : PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov) || iov.iov_len != sizeof *r)
        perror("regset");
}

/* State in /proc/<pid>/status, and whether its TracerPid is us; the state of a running process
   depends on scheduling, so it is not printed */
static void status(pid_t pid, int show_state)
{
    char path[64], line[128], state[64] = "";
    int tracer = -1;
    snprintf(path, sizeof path, "/proc/%d/status", pid);
    FILE *f = fopen(path, "r");
    while (fgets(line, sizeof line, f)) {
        sscanf(line, "State: %63[^\n]", state);
        sscanf(line, "TracerPid: %d", &tracer);
    }
    fclose(f);
    printf("  state %s, traced by %s\n", show_state ? state : "-",
           tracer == getpid() ? "me" : tracer == 0 ? "nobody" : "other");
}

/* System call entry and exit: GET_SYSCALL_INFO, x7, changing the return value, skipping the
   system call, and PTRACE_DETACH after changing the return value */
static void syscalls(void)
{
    printf("== syscall stops\n");
    pid_t pid = fork();
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        raise(SIGSTOP);
        long a = syscall(SYS_getppid);
        long b = syscall(SYS_kill, 0, 0);
        long c = syscall(SYS_getppid);
        long d = syscall(SYS_getppid);
        printf("  child sees getppid %ld, kill %ld, then getppid %s, after detach %ld\n", a, b,
               c == getppid() ? "real" : "changed", d);
        _exit(5);
    }
    int st = wait_for(pid);
    printf("first stop: %s\n", describe(st));
    siginfo_t si;
    long r = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    printf("  siginfo %ld signo %d code %d from child %d\n", r, si.si_signo, si.si_code, si.si_pid == pid);
    status(pid, 1);
    ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACESYSGOOD);
    struct __ptrace_syscall_info info;
    r = ptrace(PTRACE_GET_SYSCALL_INFO, pid, sizeof info, &info);
    printf("  info in a signal stop: size %ld op %d arch %#x\n", r, info.op, info.arch);
    ptrace(PTRACE_SYSCALL, pid, 0, 0);
    long last = -2;
    int getppids = 0;
    for (;;) {
        st = wait_for(pid);
        if (!WIFSTOPPED(st)) {
            printf("end: %s\n", describe(st));
            break;
        }
        if (WSTOPSIG(st) != (SIGTRAP | 0x80)) {
            printf("unexpected: %s\n", describe(st));
            ptrace(PTRACE_SYSCALL, pid, 0, 0);
            continue;
        }
        r = ptrace(PTRACE_GET_SYSCALL_INFO, pid, sizeof info, &info);
        unsigned long msg;
        ptrace(PTRACE_GETEVENTMSG, pid, 0, &msg);
        struct regs g;
        regs(pid, &g, 0);
        if (info.op == PTRACE_SYSCALL_INFO_ENTRY) {
            last = info.entry.nr;
            if (last == SYS_kill) {
                printf("enter kill(%lld, %lld): size %ld message %lu x7 %llu sp %s\n",
                       (long long)info.entry.args[0], (long long)info.entry.args[1], r, msg, g.x[7],
                       info.stack_pointer == g.sp ? "ok" : "differs");
                /* Skip it: the number changed to -1, return value 77 */
                int no = -1;
                struct iovec iov = {&no, sizeof no};
                ptrace(PTRACE_SETREGSET, pid, NT_ARM_SYSTEM_CALL, &iov);
                g.x[0] = 77;
                regs(pid, &g, 1);
            }
        } else if (info.op == PTRACE_SYSCALL_INFO_EXIT) {
            if (last == SYS_getppid && getppids++ == 0) {
                printf("exit getppid: size %ld message %lu x7 %llu rval is parent %d error %d\n", r, msg, g.x[7],
                       info.exit.rval == getpid(), info.exit.is_error);
                g.x[0] = 4242;
                g.x[7] = 99; /* the tracer cannot change x7 */
                regs(pid, &g, 1);
            } else if (last == SYS_getppid && getppids == 3) {
                g.x[0] = 1234;
                regs(pid, &g, 1);
                /* The child prints its own line as soon as it is released: printing only after it
                 * exits keeps the order of the two lines fixed */
                r = ptrace(PTRACE_DETACH, pid, 0, 0);
                st = wait_for(pid);
                printf("detach %ld\n", r);
                printf("end: %s\n", describe(st));
                break;
            } else if (last == SYS_kill) {
                printf("exit of the skipped kill: rval %lld error %d\n", (long long)info.exit.rval, info.exit.is_error);
            }
        }
        ptrace(PTRACE_SYSCALL, pid, 0, 0);
    }
}

/* PTRACE_O_TRACEEXEC, and the SIGTRAP after execve without it */
static void exec_events(void)
{
    printf("== exec\n");
    for (int opt = 0; opt < 2; opt++) {
        pid_t pid = fork();
        if (pid == 0) {
            ptrace(PTRACE_TRACEME, 0, 0, 0);
            raise(SIGSTOP);
            execl("/bin/true", "true", (char *)0);
            _exit(1);
        }
        wait_for(pid);
        ptrace(PTRACE_SETOPTIONS, pid, 0, opt ? PTRACE_O_TRACEEXEC : 0);
        ptrace(PTRACE_CONT, pid, 0, 0);
        int st = wait_for(pid);
        unsigned long msg = 1;
        ptrace(PTRACE_GETEVENTMSG, pid, 0, &msg);
        siginfo_t si;
        long r = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
        printf("%s: %s, message %s, siginfo %ld code %#x\n", opt ? "TRACEEXEC" : "plain", describe(st),
               msg == (unsigned long)pid ? "pid" : msg ? "other" : "0", r, si.si_code);
        ptrace(PTRACE_CONT, pid, 0, 0);
        printf("  then %s\n", describe(wait_for(pid)));
    }
}

/* PTRACE_SEIZE, PTRACE_INTERRUPT, following the new process after fork, PTRACE_EVENT_EXIT */
static void seize_fork(void)
{
    printf("== seize and fork\n");
    int go[2];
    pipe(go);
    pid_t pid = fork();
    if (pid == 0) {
        char c;
        read(go[0], &c, 1);
        pid_t g = fork();
        if (g == 0)
            _exit(7);
        int st;
        waitpid(g, &st, 0);
        printf("  child: grandchild %s\n", describe(st));
        exit(9);
    }
    printf("seize %ld\n", ptrace(PTRACE_SEIZE, pid, 0, PTRACE_O_TRACEFORK | PTRACE_O_TRACEEXIT));
    ptrace(PTRACE_INTERRUPT, pid, 0, 0);
    int st = wait_for(pid);
    printf("interrupt: %s\n", describe(st));
    ptrace(PTRACE_CONT, pid, 0, 0);
    write(go[1], "x", 1);
    st = wait_for(pid);
    unsigned long g = 0;
    ptrace(PTRACE_GETEVENTMSG, pid, 0, &g);
    printf("fork: %s\n", describe(st));
    st = wait_for(g);
    printf("grandchild starts: %s\n", describe(st));
    status(g, 1);
    ptrace(PTRACE_CONT, g, 0, 0);
    st = wait_for(g);
    unsigned long msg = 0;
    ptrace(PTRACE_GETEVENTMSG, g, 0, &msg);
    printf("grandchild exiting: %s", describe(st));
    printf(", status %s\n", describe(msg));
    ptrace(PTRACE_CONT, g, 0, 0);
    printf("grandchild: %s\n", describe(wait_for(g)));
    /* The grandchild exited and the child gets SIGCHLD: the traced thread stops first for the
     * tracer to see */
    ptrace(PTRACE_CONT, pid, 0, 0);
    while (st = wait_for(pid), WIFSTOPPED(st) && st >> 16 == 0) {
        printf("child gets a signal: %s\n", describe(st));
        ptrace(PTRACE_CONT, pid, 0, WSTOPSIG(st));
    }
    ptrace(PTRACE_GETEVENTMSG, pid, 0, &msg);
    printf("child exiting: %s", describe(st));
    printf(", status %s\n", describe(msg));
    ptrace(PTRACE_CONT, pid, 0, 0);
    printf("child: %s\n", describe(wait_for(pid)));
}

/* Continue with PTRACE_CONT, and on a fork event stopped in the middle of a system call switch to
   PTRACE_SYSCALL: as on Linux it stops at the exit of this system call (clone), even though the
   entry never stopped; NT_ARM_SYSTEM_CALL gives clone's number both times */
static int syscall_number(pid_t pid)
{
    int nr = -2;
    struct iovec iov = {&nr, sizeof nr};
    if (ptrace(PTRACE_GETREGSET, pid, NT_ARM_SYSTEM_CALL, &iov))
        perror("NT_ARM_SYSTEM_CALL");
    return nr;
}

static void event_then_syscall(void)
{
    printf("== syscall exit after an event stop\n");
    pid_t pid = fork();
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        raise(SIGSTOP);
        pid_t g = fork();
        if (g == 0)
            _exit(3);
        int st;
        waitpid(g, &st, 0);
        _exit(WIFEXITED(st) ? WEXITSTATUS(st) + 10 : 1);
    }
    int st = wait_for(pid);
    printf("first stop: %s\n", describe(st));
    ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACEFORK | PTRACE_O_TRACESYSGOOD);
    ptrace(PTRACE_CONT, pid, 0, 0);
    st = wait_for(pid);
    unsigned long g = 0;
    ptrace(PTRACE_GETEVENTMSG, pid, 0, &g);
    printf("fork: %s, syscall %d\n", describe(st), syscall_number(pid));
    printf("grandchild starts: %s\n", describe(wait_for(g)));
    ptrace(PTRACE_DETACH, g, 0, 0);
    ptrace(PTRACE_SYSCALL, pid, 0, 0);
    st = wait_for(pid);
    struct __ptrace_syscall_info info;
    memset(&info, 0, sizeof info);
    ptrace(PTRACE_GET_SYSCALL_INFO, pid, sizeof info, &info);
    printf("next: %s, op %d, syscall %d, returns the grandchild %d\n", describe(st), info.op,
           syscall_number(pid), info.op == PTRACE_SYSCALL_INFO_EXIT && info.exit.rval == (long)g);
    ptrace(PTRACE_CONT, pid, 0, 0);
    while (st = wait_for(pid), WIFSTOPPED(st)) {
        printf("child gets a signal: %s\n", describe(st));
        ptrace(PTRACE_CONT, pid, 0, WSTOPSIG(st));
    }
    printf("child: %s\n", describe(st));
}

/* group-stop: reported as PTRACE_EVENT_STOP with SEIZE, and PTRACE_LISTEN waits for SIGCONT;
   with ATTACH group-stop has no siginfo */
static void group_stop(void)
{
    printf("== group stop\n");
    int go[2];
    pipe(go);
    pid_t pid = fork();
    if (pid == 0) {
        char c;
        read(go[0], &c, 1);
        _exit(3);
    }
    ptrace(PTRACE_SEIZE, pid, 0, 0);
    kill(pid, SIGSTOP);
    int st = wait_for(pid);
    printf("signal: %s\n", describe(st));
    ptrace(PTRACE_CONT, pid, 0, SIGSTOP);
    st = wait_for(pid);
    siginfo_t si;
    long r = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    printf("group stop: %s, siginfo %ld\n", describe(st), r);
    printf("listen %ld\n", ptrace(PTRACE_LISTEN, pid, 0, 0));
    status(pid, 1);
    errno = 0;
    r = ptrace(PTRACE_CONT, pid, 0, 0);
    printf("cont while listening %ld %s\n", r, strerror(errno));
    kill(pid, SIGCONT);
    st = wait_for(pid);
    printf("after SIGCONT: %s\n", describe(st));
    ptrace(PTRACE_CONT, pid, 0, 0);
    st = wait_for(pid);
    printf("then: %s\n", describe(st));
    ptrace(PTRACE_CONT, pid, 0, SIGCONT);
    write(go[1], "x", 1);
    printf("end: %s\n", describe(wait_for(pid)));

    pid = fork();
    if (pid == 0) {
        char c;
        read(go[0], &c, 1);
        _exit(4);
    }
    printf("attach %ld\n", ptrace(PTRACE_ATTACH, pid, 0, 0));
    st = wait_for(pid);
    r = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    printf("attach stop: %s, siginfo %ld signo %d\n", describe(st), r, si.si_signo);
    ptrace(PTRACE_CONT, pid, 0, SIGSTOP);
    st = wait_for(pid);
    errno = 0;
    r = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    printf("group stop: %s, siginfo %ld %s\n", describe(st), r, strerror(errno));
    ptrace(PTRACE_CONT, pid, 0, 0);
    write(go[1], "x", 1);
    printf("end: %s\n", describe(wait_for(pid)));
}

/* Tracing an already stopped process: the signal that stopped it (SIGTSTP here) is reported,
   also when the parent has already collected the stop event with wait */
static void stopped_attach(void)
{
    printf("== attach to a stopped process\n");
    for (int seize = 0; seize < 2; seize++) {
        pid_t pid = fork();
        if (pid == 0) {
            /* A process group of its own: the parent is in another process group of the same
               session, so it is not an orphaned process group, and SIGTSTP is not discarded */
            setpgid(0, 0);
            raise(SIGTSTP);
            _exit(6);
        }
        int st;
        waitpid(pid, &st, WUNTRACED);
        printf("%s; the parent took %s\n", seize ? "seize" : "attach", describe(st));
        long r = ptrace(seize ? PTRACE_SEIZE : PTRACE_ATTACH, pid, 0, 0);
        st = wait_for(pid);
        siginfo_t si;
        errno = 0;
        long got = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
        printf("  %ld, first stop: %s, siginfo %ld", r, describe(st), got);
        if (got == 0)
            printf(" signo %d code %#x\n", si.si_signo, si.si_code);
        else
            printf(" %s\n", strerror(errno));
        if (seize) {
            ptrace(PTRACE_LISTEN, pid, 0, 0);
            kill(pid, SIGCONT);
            printf("  after SIGCONT: %s\n", describe(wait_for(pid)));
            ptrace(PTRACE_CONT, pid, 0, 0);
            printf("  then: %s\n", describe(wait_for(pid)));
        } else {
            /* Then the SIGSTOP sent by ATTACH; it is not delivered, and although the thread group
             * is stopped, the traced thread runs as usual */
            ptrace(PTRACE_CONT, pid, 0, 0);
            printf("  then: %s\n", describe(wait_for(pid)));
        }
        ptrace(PTRACE_CONT, pid, 0, 0);
        printf("  end: %s\n", describe(wait_for(pid)));
    }
}

/* The tracer is not the parent (it is the grandparent, which Yama also allows): after the tracee
   ends it is first reported only to the tracer, and until the tracer collects it with wait the
   parent cannot wait for it and gets no SIGCHLD; only after that can the parent reap it; if the
   tracer exits without collecting it, it is handed to the parent */
static void foreign_tracer(void)
{
    printf("== tracer that is not the parent\n");
    int up[2], go[2], chk[2], say[2];
    pipe(up);
    pipe(go);
    pipe(chk);
    pipe(say);
    char ch;
    pid_t parent = fork();
    if (parent == 0) {
        sigset_t chld;
        sigemptyset(&chld);
        sigaddset(&chld, SIGCHLD);
        sigprocmask(SIG_BLOCK, &chld, NULL);
        pid_t c = fork();
        if (c == 0) {
            read(go[0], &ch, 1);
            _exit(7);
        }
        write(up[1], &c, sizeof c);
        read(chk[0], &ch, 1);
        int st = -1;
        sigset_t pending;
        sigpending(&pending);
        pid_t r = waitpid(c, &st, WNOHANG);
        printf("  parent before the tracer reaps: waitpid %s, SIGCHLD %s\n",
               r == 0 ? "0" : r == c ? describe(st) : "error",
               sigismember(&pending, SIGCHLD) ? "pending" : "not pending");
        write(up[1], "x", 1);
        r = waitpid(c, &st, 0);
        sigpending(&pending);
        read(say[0], &ch, 1);
        printf("  parent then reaps: %s, SIGCHLD %s\n", r == c ? describe(st) : "error",
               sigismember(&pending, SIGCHLD) ? "pending" : "not pending");
        _exit(0);
    }
    pid_t c;
    read(up[0], &c, sizeof c);
    printf("seize %ld\n", ptrace(PTRACE_SEIZE, c, 0, 0));
    write(go[1], "x", 1);
    siginfo_t si = {0};
    waitid(P_PID, c, &si, WEXITED | WNOWAIT);
    printf("  tracer sees: code %d status %d\n", si.si_code, si.si_status);
    write(chk[1], "x", 1);
    read(up[0], &ch, 1);
    int st;
    printf("  tracer reaps: %s\n", waitpid(c, &st, 0) == c ? describe(st) : "error");
    write(say[1], "x", 1);
    waitpid(parent, &st, 0);
    printf("parent: %s\n", describe(st));

    /* The tracer (the grandparent) exited without collecting it */
    int done[2];
    pipe(done);
    pid_t tracer = fork();
    if (tracer == 0) {
        if (fork() == 0) {
            pid_t c = fork();
            if (c == 0) {
                read(go[0], &ch, 1);
                _exit(8);
            }
            write(up[1], &c, sizeof c);
            read(chk[0], &ch, 1);
            int st = -1;
            printf("  parent before the tracer exits: waitpid %d\n", waitpid(c, &st, WNOHANG));
            write(up[1], "x", 1);
            pid_t r = waitpid(c, &st, 0);
            printf("  parent after the tracer exits: %s\n", r == c ? describe(st) : "error");
            write(done[1], "x", 1);
            _exit(0);
        }
        pid_t c;
        read(up[0], &c, sizeof c);
        printf("seize %ld\n", ptrace(PTRACE_SEIZE, c, 0, 0));
        write(go[1], "x", 1);
        siginfo_t si = {0};
        waitid(P_PID, c, &si, WEXITED | WNOWAIT);
        printf("  tracer sees: code %d status %d\n", si.si_code, si.si_status);
        write(chk[1], "x", 1);
        read(up[0], &ch, 1);
        _exit(0);
    }
    read(done[0], &ch, 1);
    waitpid(tracer, &st, 0);
    printf("tracer: %s\n", describe(st));
}

/* Signals the tracee ignores are also left pending, and the tracer sees them */
static void ignored(void)
{
    printf("== ignored signal\n");
    int go[2], ready[2];
    pipe(go);
    pipe(ready);
    pid_t pid = fork();
    if (pid == 0) {
        signal(SIGUSR1, SIG_IGN);
        write(ready[1], "r", 1);
        char c;
        read(go[0], &c, 1);
        _exit(6);
    }
    char c;
    read(ready[0], &c, 1);
    ptrace(PTRACE_SEIZE, pid, 0, 0);
    kill(pid, SIGUSR1);
    printf("ignored SIGUSR1: %s\n", describe(wait_for(pid)));
    ptrace(PTRACE_CONT, pid, 0, SIGUSR1);
    write(go[1], "x", 1);
    printf("end: %s\n", describe(wait_for(pid)));
}

static void *thread_main(void *arg)
{
    (void)arg;
    return 0;
}

/* PTRACE_O_TRACECLONE: a new thread starts with SIGSTOP pending, and its exit is reported
 * separately */
static void threads(void)
{
    printf("== threads\n");
    pid_t pid = fork();
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        raise(SIGSTOP);
        pthread_t t;
        pthread_create(&t, 0, thread_main, 0);
        pthread_join(t, 0);
        _exit(0);
    }
    wait_for(pid);
    ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACECLONE);
    ptrace(PTRACE_CONT, pid, 0, 0);
    int st = wait_for(pid);
    unsigned long tid = 0;
    ptrace(PTRACE_GETEVENTMSG, pid, 0, &tid);
    printf("clone: %s, new thread is %s\n", describe(st), tid > (unsigned long)pid ? "after the process" : "?");
    st = wait_for(tid);
    printf("thread starts: %s\n", describe(st));
    ptrace(PTRACE_CONT, tid, 0, 0);
    ptrace(PTRACE_CONT, pid, 0, 0);
    printf("thread: %s\n", describe(wait_for(tid)));
    printf("process: %s\n", describe(wait_for(pid)));
}

/* PTRACE_O_EXITKILL: the tracee is killed when the tracer exits */
static void exitkill(void)
{
    printf("== exitkill\n");
    int alive[2];
    pipe(alive);
    pid_t tracer = fork();
    if (tracer == 0) {
        close(alive[0]);
        pid_t victim = fork();
        if (victim == 0) {
            pause();
            _exit(0);
        }
        close(alive[1]);
        ptrace(PTRACE_SEIZE, victim, 0, PTRACE_O_EXITKILL);
        _exit(0);
    }
    close(alive[1]);
    waitpid(tracer, 0, 0);
    struct pollfd p = {alive[0], POLLIN, 0};
    char c;
    int n = poll(&p, 1, 5000);
    printf("victim %s\n", n == 1 && read(alive[0], &c, 1) == 0 ? "killed" : "still running");
}

static char message[32] = "hello from the child";

/* process_vm_readv/writev and PTRACE_PEEKDATA: the same address in the child after fork */
static void memory(void)
{
    printf("== memory\n");
    int go[2], back[2];
    pipe(go);
    pipe(back);
    pid_t pid = fork();
    if (pid == 0) {
        char c;
        read(go[0], &c, 1);
        write(back[1], message, sizeof message);
        _exit(0);
    }
    char buf[32] = {0};
    struct iovec local = {buf, 5}, remote = {message + 6, 5};
    printf("readv %zd '%.5s'\n", process_vm_readv(pid, &local, 1, &remote, 1, 0), buf);
    struct iovec from = {"HELLO", 5}, to = {message, 5};
    printf("writev %zd\n", process_vm_writev(pid, &from, 1, &to, 1, 0));
    struct iovec two[2] = {{message, 4}, {(void *)16, 4}}, eight = {buf, 8};
    printf("second piece unmapped %zd\n", process_vm_readv(pid, &eight, 1, two, 2, 0));
    errno = 0;
    printf("unmapped %zd %s\n", process_vm_readv(pid, &eight, 1, &two[1], 1, 0), strerror(errno));
    errno = 0;
    printf("flags %zd %s\n", process_vm_readv(pid, &eight, 1, two, 1, 1), strerror(errno));
    ptrace(PTRACE_SEIZE, pid, 0, 0);
    ptrace(PTRACE_INTERRUPT, pid, 0, 0);
    wait_for(pid);
    errno = 0;
    long word = ptrace(PTRACE_PEEKDATA, pid, message, 0);
    printf("peek '%.8s' %s\n", (char *)&word, strerror(errno));
    errno = 0;
    ptrace(PTRACE_PEEKDATA, pid, (void *)16, 0);
    printf("peek unmapped %s\n", strerror(errno));
    status(pid, 1);
    ptrace(PTRACE_DETACH, pid, 0, 0);
    status(pid, 0);
    write(go[1], "x", 1);
    read(back[0], buf, sizeof buf);
    printf("child now says '%s'\n", buf);
    waitpid(pid, 0, 0);
}

static void __attribute__((noinline)) steps(void)
{
    asm volatile("nop\n nop\n mov x9, #5\n add x9, x9, #1\n" ::: "x9");
}

static long __attribute__((noinline)) do_svc(void)
{
    register long x0 asm("x0");
    asm volatile("mov x8, #172\n svc #0\n nop\n" : "=r"(x0)::"x8", "memory");
    return x0;
}

/* Single-step to the next SIGTRAP, and print pc's position relative to `base` and the siginfo */
static struct regs step(pid_t pid, void *base)
{
    struct regs r;
    ptrace(PTRACE_SINGLESTEP, pid, 0, 0);
    int st = wait_for(pid);
    regs(pid, &r, 0);
    siginfo_t si;
    ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    printf("  step: %s, pc %+lld, code %d, addr is pc %d\n", describe(st), (long long)(r.pc - (unsigned long)base),
           si.si_code, si.si_addr == (void *)r.pc);
    return r;
}

/* Breakpoints (brk written with PTRACE_POKETEXT and through /proc/<pid>/mem), PTRACE_SINGLESTEP
   (ordinary instructions and svc), /proc/<pid>/auxv, and read-only shared mappings that cannot
   be force-written */
static void breakpoints(void)
{
    printf("== breakpoints and single step\n");
    int fd = open("shared.tmp", O_RDWR | O_CREAT | O_TRUNC, 0600);
    ftruncate(fd, 4096);
    char *shared = mmap(0, 4096, PROT_READ, MAP_SHARED, fd, 0);
    pid_t pid = fork();
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        /* Executed first: Tier 1 has already translated it, and writing the breakpoint must
         * invalidate that */
        for (int i = 0; i < 100; i++)
            steps();
        raise(SIGSTOP);
        steps();
        long r = do_svc();
        _exit(r == getpid() ? 0 : 1);
    }
    wait_for(pid);
    const unsigned brk = 0xd4200000;
    errno = 0;
    long word = ptrace(PTRACE_PEEKTEXT, pid, steps, 0);
    long patched = (word & ~0xffffffffL) | brk;
    printf("poke code %ld\n", ptrace(PTRACE_POKETEXT, pid, steps, patched));
    ptrace(PTRACE_CONT, pid, 0, 0);
    int st = wait_for(pid);
    struct regs r;
    regs(pid, &r, 0);
    siginfo_t si;
    ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    printf("breakpoint: %s, at steps %d, code %d, addr %d\n", describe(st), r.pc == (unsigned long)steps,
           si.si_code, si.si_addr == (void *)steps);
    ptrace(PTRACE_POKETEXT, pid, steps, word);
    for (int i = 0; i < 4; i++)
        r = step(pid, steps);
    printf("  x9 %llu\n", r.x[9]);

    /* Set a breakpoint at do_svc through /proc/<pid>/mem */
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/mem", pid);
    int mem = open(path, O_RDWR);
    unsigned insn = 0, back = 0;
    printf("mem read %zd\n", pread(mem, &insn, 4, (off_t)(unsigned long)do_svc));
    printf("mem write %zd\n", pwrite(mem, &brk, 4, (off_t)(unsigned long)do_svc));
    pread(mem, &back, 4, (off_t)(unsigned long)do_svc);
    printf("  reads back brk %d\n", back == brk);
    errno = 0;
    printf("mem unmapped %zd %s\n", pread(mem, &back, 4, 16), strerror(errno));
    ptrace(PTRACE_CONT, pid, 0, 0);
    st = wait_for(pid);
    regs(pid, &r, 0);
    printf("breakpoint: %s, at do_svc %d\n", describe(st), r.pc == (unsigned long)do_svc);
    pwrite(mem, &insn, 4, (off_t)(unsigned long)do_svc);
    for (int i = 0; i < 3; i++)
        r = step(pid, do_svc);
    printf("  x0 is the pid %d\n", r.x[0] == (unsigned long long)pid);

    /* A read-only shared mapping cannot be force-written */
    errno = 0;
    printf("poke read-only shared %ld %s\n", ptrace(PTRACE_POKEDATA, pid, shared, 0L), strerror(errno));
    errno = 0;
    printf("mem write read-only shared %zd %s\n", pwrite(mem, "x", 1, (off_t)(unsigned long)shared),
           strerror(errno));
    close(mem);

    snprintf(path, sizeof path, "/proc/%d/auxv", pid);
    int f = open(path, O_RDONLY);
    unsigned long aux[128] = {0};
    ssize_t n = read(f, aux, sizeof aux);
    close(f);
    int entry = 0, pagesz = 0, last = 0;
    for (ssize_t i = 0; i + 1 < n / 8; i += 2) {
        if (aux[i] == AT_ENTRY)
            entry = aux[i + 1] == getauxval(AT_ENTRY);
        if (aux[i] == AT_PAGESZ)
            pagesz = aux[i + 1] == 4096;
        last = aux[i] == AT_NULL;
    }
    printf("auxv: entry %d, page size %d, ends with AT_NULL %d\n", entry, pagesz, last);
    ptrace(PTRACE_CONT, pid, 0, 0);
    printf("end: %s\n", describe(wait_for(pid)));
}

static void errors(void)
{
    printf("== errors\n");
    errno = 0;
    ptrace(PTRACE_CONT, getpid(), 0, 0);
    printf("cont self: %s\n", strerror(errno));
    errno = 0;
    ptrace(PTRACE_ATTACH, getpid(), 0, 0);
    printf("attach self: %s\n", strerror(errno));
    errno = 0;
    ptrace(PTRACE_ATTACH, 999999, 0, 0);
    printf("attach missing: %s\n", strerror(errno));
    int go[2];
    pipe(go);
    pid_t pid = fork();
    if (pid == 0) {
        char c;
        read(go[0], &c, 1);
        _exit(0);
    }
    errno = 0;
    ptrace(PTRACE_SEIZE, pid, 0, 1 << 30);
    printf("seize with a bad option: %s\n", strerror(errno));
    ptrace(PTRACE_SEIZE, pid, 0, 0);
    errno = 0;
    ptrace(PTRACE_SEIZE, pid, 0, 0);
    printf("seize twice: %s\n", strerror(errno));
    errno = 0;
    ptrace(PTRACE_CONT, pid, 0, 0);
    printf("cont while running: %s\n", strerror(errno));
    ptrace(PTRACE_INTERRUPT, pid, 0, 0);
    wait_for(pid);
    errno = 0;
    ptrace(PTRACE_SETOPTIONS, pid, 0, 1 << 30);
    printf("bad option: %s\n", strerror(errno));
    errno = 0;
    ptrace(PTRACE_CONT, pid, 0, 99);
    printf("bad signal: %s\n", strerror(errno));
    errno = 0;
    ptrace(PTRACE_PEEKUSER, pid, 0, 0);
    printf("peekuser: %s\n", strerror(errno));
    unsigned long long b[40];
    struct iovec iov = {b, 7};
    errno = 0;
    ptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov);
    printf("regset of 7 bytes: %s\n", strerror(errno));
    iov.iov_len = 16;
    printf("regset of 16 bytes: %ld, got %zu\n", ptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov), iov.iov_len);
    iov.iov_len = sizeof b;
    printf("whole regset: got %zu\n", (ptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov), iov.iov_len));
    iov.iov_len = sizeof b;
    printf("fp regset: got %zu\n", (ptrace(PTRACE_GETREGSET, pid, NT_PRFPREG, &iov), iov.iov_len));
    iov.iov_len = sizeof b;
    printf("hw breakpoints: got %zu\n", (ptrace(PTRACE_GETREGSET, pid, NT_ARM_HW_BREAK, &iov), iov.iov_len));
    errno = 0;
    ptrace(PTRACE_GETREGSET, pid, 0x7777, &iov);
    printf("unknown regset: %s\n", strerror(errno));
    errno = 0;
    ptrace(PTRACE_GETSIGMASK, pid, 4, b);
    printf("sigmask of 4 bytes: %s\n", strerror(errno));
    ptrace(PTRACE_KILL, pid, 0, 0);
    printf("kill: %s\n", describe(wait_for(pid)));
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    /* Do not hang forever when an expected stop never comes */
    alarm(60);
    syscalls();
    exec_events();
    seize_fork();
    event_then_syscall();
    group_stop();
    stopped_attach();
    foreign_tracer();
    ignored();
    threads();
    exitkill();
    memory();
    breakpoints();
    errors();
    return 0;
}
C
gcc -O1 -o tracer tracer.c -lpthread
./tracer

# strace: addresses, pids and the number of environment variables differ between the two sides,
# so the outputs are compared after normalization
norm() {
    sed -E 's/0x[0-9a-f]+/ADDR/g; s/\/\* [0-9]+ vars \*\//VARS/; s/(si_pid|si_uid)=[0-9]+/\1=N/g' |
        sed -E 's/^(getpid|getppid|gettid|set_tid_address|clone|wait4)\((.*)\) += [0-9]+$/\1(\2) = PID/' |
        sed -E 's/^tkill\([0-9]+/tkill(TID/'
}
cat > sig.c <<'C'
#include <signal.h>
#include <stdlib.h>
#include <unistd.h>

static void handler(int sig)
{
    (void)sig;
    write(1, "handled\n", 8);
}

int main(void)
{
    signal(SIGUSR1, handler);
    raise(SIGUSR1);
    raise(SIGUSR2);
    return 0;
}
C
gcc -O1 -o sig sig.c
echo "some text" > text
env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin strace -o st1 cat text
norm < st1
echo "strace sig: $(env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin strace -o st2 ./sig; echo $?)"
norm < st2
# -f: one file per process, in pid order
rm -rf st3 && mkdir st3
env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin strace -ff -e trace=process -e signal=none -o st3/t sh -c 'true; ls /dev/null >/dev/null; exit 2' || echo "strace -f: $?"
for f in $(ls st3 | sort -t. -k2 -n); do
    echo "-- process"
    norm < st3/$f
done

# Hardware watchpoints and breakpoints (NT_ARM_HW_WATCH, NT_ARM_HW_BREAK): slot counts, invalid
# control words, reading back canonicalized control words; write, read and unaligned one-byte
# watchpoints stop before the access (the data is not written yet), with si_code TRAP_HWBKPT and
# si_addr the accessed address; after disabling, a single step completes the access; a
# breakpoint stops at the instruction
cat > hw.c <<'C'
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

struct dbg { uint32_t info, pad; struct { uint64_t addr; uint32_t ctrl, pad; } regs[16]; };

static volatile long data[4];
static volatile char bytes[16];

__attribute__((noinline)) static void store(volatile long *p, long v) { *p = v; }
__attribute__((noinline)) static long load(volatile long *p) { return *p; }
__attribute__((noinline)) static void store_byte(volatile char *p) { *p = 'x'; }
__attribute__((noinline)) static int target(int x) { return x + 1; }

static int setdbg(pid_t pid, int nt, int n, const uint64_t *addr, const uint32_t *ctrl)
{
    struct dbg d;
    memset(&d, 0, sizeof d);
    for (int i = 0; i < n; i++) { d.regs[i].addr = addr[i]; d.regs[i].ctrl = ctrl[i]; }
    struct iovec iov = {&d, 8 + 16 * (n ? n : 1)};
    return ptrace(PTRACE_SETREGSET, pid, nt, &iov) < 0 ? -errno : 0;
}

static uint32_t ctrl(uint32_t bas, uint32_t type) { return bas << 5 | type << 3 | 2 << 1 | 1; }

static void report(pid_t pid, const char *what)
{
    int st;
    waitpid(pid, &st, 0);
    if (!WIFSTOPPED(st)) { printf("%s: not stopped %#x\n", what, st); return; }
    siginfo_t si;
    ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    struct { uint64_t x[31], sp, pc, pstate; } r;
    struct iovec iov = {&r, sizeof r};
    ptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov);
    long cur = ptrace(PTRACE_PEEKDATA, pid, &data[1], 0);
    printf("%s: sig %d code %d addr %s pc %s data[1] %ld\n", what, WSTOPSIG(st), si.si_code,
           si.si_addr == (void *)&data[1] ? "data[1]" : si.si_addr == (void *)&bytes[3] ? "bytes[3]" :
           si.si_addr == (void *)(uintptr_t)target ? "target" : "other",
           r.pc == (uintptr_t)target ? "target" : "other", cur);
}

int main(void)
{
    pid_t pid = fork();
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        raise(SIGSTOP);
        store(&data[1], 5);
        long v = load(&data[1]);
        store_byte(&bytes[3]);
        v += target(1);
        _exit(v == 7 ? 0 : 1);
    }
    int st;
    waitpid(pid, &st, 0);
    struct dbg d;
    struct iovec iov = {&d, sizeof d};
    ptrace(PTRACE_GETREGSET, pid, NT_ARM_HW_WATCH, &iov);
    printf("watch slots >= 2: %d, len %zu\n", (d.info & 0xff) >= 2, iov.iov_len);
    iov.iov_len = sizeof d;
    ptrace(PTRACE_GETREGSET, pid, NT_ARM_HW_BREAK, &iov);
    printf("break slots >= 2: %d\n", (d.info & 0xff) >= 2);
    uint64_t a = (uint64_t)&data[1];
    uint32_t bad[] = {ctrl(0xff, 0), ctrl(0x5, 2), 0x1};
    for (int i = 0; i < 3; i++)
        printf("bad ctrl %#x: %d\n", bad[i], setdbg(pid, NT_ARM_HW_WATCH, 1, &a, &bad[i]));
    uint32_t c = ctrl(0xff, 2); /* write */
    printf("set store watch: %d\n", setdbg(pid, NT_ARM_HW_WATCH, 1, &a, &c));
    iov.iov_len = sizeof d;
    ptrace(PTRACE_GETREGSET, pid, NT_ARM_HW_WATCH, &iov);
    printf("read back: addr ok %d ctrl %#x\n", d.regs[0].addr == a, d.regs[0].ctrl);
    ptrace(PTRACE_CONT, pid, 0, 0);
    report(pid, "store watch");
    c = 0;
    setdbg(pid, NT_ARM_HW_WATCH, 1, &a, &c);
    ptrace(PTRACE_SINGLESTEP, pid, 0, 0);
    report(pid, "stepped");
    c = ctrl(0xff, 1); /* read */
    setdbg(pid, NT_ARM_HW_WATCH, 1, &a, &c);
    ptrace(PTRACE_CONT, pid, 0, 0);
    report(pid, "load watch");
    /* An unaligned single byte: the address is aligned to 8, and BAS selects byte 3 */
    uint64_t b8 = (uint64_t)&bytes[3] & ~7ul;
    uint32_t cb = ctrl(1u << ((uint64_t)&bytes[3] & 7), 2);
    printf("set byte watch: %d\n", setdbg(pid, NT_ARM_HW_WATCH, 1, &b8, &cb));
    c = 0;
    ptrace(PTRACE_CONT, pid, 0, 0);
    report(pid, "byte watch");
    setdbg(pid, NT_ARM_HW_WATCH, 1, &a, &c);
    uint64_t t = (uint64_t)target;
    uint32_t cbk = ctrl(0xf, 0);
    printf("set breakpoint: %d\n", setdbg(pid, NT_ARM_HW_BREAK, 1, &t, &cbk));
    uint64_t tu = t + 2;
    printf("unaligned breakpoint: %d\n", setdbg(pid, NT_ARM_HW_BREAK, 1, &tu, &cbk));
    setdbg(pid, NT_ARM_HW_BREAK, 1, &t, &cbk);
    ptrace(PTRACE_CONT, pid, 0, 0);
    report(pid, "breakpoint");
    cbk = 0;
    setdbg(pid, NT_ARM_HW_BREAK, 1, &t, &cbk);
    ptrace(PTRACE_CONT, pid, 0, 0);
    waitpid(pid, &st, 0);
    printf("exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    return 0;
}
C
gcc -O1 -o hw hw.c && ./hw
# Atomic read-modify-writes (LSE) also count as reads; a 16-byte SIMD store that touches the
# second half reports the start of the access
cat > hw3.c <<'C'
#define _GNU_SOURCE
#include <elf.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

struct dbg { uint32_t info, pad; struct { uint64_t addr; uint32_t ctrl, pad; } regs[16]; };
static volatile int atom;
static volatile uint64_t vec[4] __attribute__((aligned(16)));

static void watch(pid_t pid, uint64_t addr, uint32_t bas, uint32_t type)
{
    struct dbg d;
    memset(&d, 0, sizeof d);
    d.regs[0].addr = addr;
    d.regs[0].ctrl = type ? bas << 5 | type << 3 | 2 << 1 | 1 : 0;
    struct iovec iov = {&d, 8 + 16};
    if (ptrace(PTRACE_SETREGSET, pid, NT_ARM_HW_WATCH, &iov) < 0)
        perror("SETREGSET");
}

/* si_addr must fall within [start, start + len) of this instruction's access. Which address is
 * reported when the access crosses out of the watched range is up to the core: Apple cores report
 * the start of the access, the Neoverse N2 on CI reports one that is not the start */
static void report(pid_t pid, const char *what, volatile void *start, size_t len)
{
    int st;
    waitpid(pid, &st, 0);
    siginfo_t si;
    ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    uintptr_t a = (uintptr_t)si.si_addr, s = (uintptr_t)start;
    printf("%s: sig %d code %d addr %s\n", what, WSTOPSIG(st), si.si_code,
           a >= s && a < s + len ? "ok" : "other");
}

int main(void)
{
    pid_t pid = fork();
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        raise(SIGSTOP);
        __atomic_fetch_add(&atom, 1, __ATOMIC_SEQ_CST);
        __asm__ volatile("movi v0.16b, #7\n\tstr q0, [%0]" : : "r"(&vec[1]) : "v0", "memory");
        _exit(atom == 1 && vec[1] == 0x0707070707070707ull ? 0 : 1);
    }
    int st;
    waitpid(pid, &st, 0);
    /* A watchpoint for reads only: atomic read-modify-writes also count as reads */
    watch(pid, (uint64_t)&atom & ~7ull, 0xf << ((uint64_t)&atom & 7), 1);
    ptrace(PTRACE_CONT, pid, 0, 0);
    report(pid, "atomic add, load watch", &atom, sizeof atom);
    watch(pid, 0, 0, 0);
    /* A 16-byte SIMD store that touches the second half */
    watch(pid, (uint64_t)&vec[2], 0xff, 2);
    ptrace(PTRACE_CONT, pid, 0, 0);
    report(pid, "simd store", &vec[1], 16);
    watch(pid, 0, 0, 0);
    ptrace(PTRACE_CONT, pid, 0, 0);
    waitpid(pid, &st, 0);
    printf("exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    return 0;
}
C
gcc -O1 -o hw3 hw3.c && ./hw3
# Hardware watchpoints outlive PTRACE_DETACH on arm64 (only execve flushes them): one set at the
# stop the tracer detaches from still stops the next write with SIGTRAP, and a tracer attaching
# later reads the armed slot from NT_ARM_HW_WATCH
cat > hw4.c <<'C'
#define _GNU_SOURCE
#include <elf.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

struct dbg { uint32_t info, pad; struct { uint64_t addr; uint32_t ctrl, pad; } regs[16]; };
static volatile long word;

static void watch(pid_t pid, int on)
{
    struct dbg d;
    memset(&d, 0, sizeof d);
    d.regs[0].addr = (uint64_t)&word;
    d.regs[0].ctrl = on ? 0xff << 5 | 2 << 3 | 2 << 1 | 1 : 0;
    struct iovec iov = {&d, 8 + 16};
    if (ptrace(PTRACE_SETREGSET, pid, NT_ARM_HW_WATCH, &iov) < 0)
        perror("SETREGSET");
}

static void status(const char *what, int st)
{
    if (WIFEXITED(st))
        printf("%s: exit %d\n", what, WEXITSTATUS(st));
    else
        printf("%s: signal %d\n", what, WIFSIGNALED(st) ? WTERMSIG(st) : -1);
}

int main(void)
{
    int st;
    pid_t pid = fork();
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        raise(SIGSTOP);
        word = 1;
        _exit(0);
    }
    waitpid(pid, &st, 0);
    watch(pid, 1);
    ptrace(PTRACE_DETACH, pid, 0, 0);
    waitpid(pid, &st, 0);
    status("write after PTRACE_DETACH", st);
    int p[2];
    pipe(p);
    pid = fork();
    if (pid == 0) {
        char b;
        close(p[1]);
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        raise(SIGSTOP);
        read(p[0], &b, 1);
        word = 2;
        _exit(0);
    }
    close(p[0]);
    waitpid(pid, &st, 0);
    watch(pid, 1);
    ptrace(PTRACE_DETACH, pid, 0, 0);
    usleep(100000);
    ptrace(PTRACE_ATTACH, pid, 0, 0);
    waitpid(pid, &st, 0);
    struct dbg d;
    struct iovec iov = {&d, sizeof d};
    memset(&d, 0, sizeof d);
    ptrace(PTRACE_GETREGSET, pid, NT_ARM_HW_WATCH, &iov);
    printf("a later tracer reads the slot: addr %d enabled %d\n",
           d.regs[0].addr == (uint64_t)&word, d.regs[0].ctrl & 1);
    watch(pid, 0);
    ptrace(PTRACE_DETACH, pid, 0, 0);
    write(p[1], "x", 1);
    waitpid(pid, &st, 0);
    status("write after clearing it", st);
    return 0;
}
C
gcc -O1 -o hw4 hw4.c && ./hw4

# Single-stepping with a signal: stops at the first instruction of the handler (ptrace_notify's
# SIGTRAP, with si_code also SIGTRAP), and only the next single step executes it; when stopped in
# sigsuspend, GETSIGMASK reads the original mask, and after SETSIGMASK the handler's return no
# longer restores the original one
cat > sigstep.c <<'C'
#define _GNU_SOURCE
#include <elf.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile int hits;
static void handler(int sig) { (void)sig; hits++; }

struct regs { uint64_t x[31], sp, pc, pstate; };

static uint64_t pc_of(pid_t pid)
{
    struct regs r;
    struct iovec iov = {&r, sizeof r};
    ptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov);
    return r.pc;
}

static int wait_stop(pid_t pid, siginfo_t *si)
{
    int st;
    waitpid(pid, &st, 0);
    if (!WIFSTOPPED(st))
        return -1;
    ptrace(PTRACE_GETSIGINFO, pid, 0, si);
    return WSTOPSIG(st);
}

static const char *where(uint64_t pc)
{
    uint64_t h = (uint64_t)handler;
    if (pc == h) return "handler";
    if (pc == h + 4) return "handler+4";
    return "elsewhere";
}

int main(void)
{
    pid_t pid = fork();
    if (pid == 0) {
        signal(SIGUSR1, handler);
        sigset_t m;
        sigemptyset(&m);
        sigaddset(&m, SIGUSR2);
        sigprocmask(SIG_BLOCK, &m, NULL);
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        raise(SIGSTOP);
        raise(SIGUSR1);
        /* sigsuspend: wait with only SIGINT blocked (the mask is replaced, so SIGUSR2 is not
         * blocked during the call) */
        sigset_t s;
        sigemptyset(&s);
        sigaddset(&s, SIGINT);
        sigsuspend(&s);
        sigset_t after;
        sigprocmask(SIG_BLOCK, NULL, &after);
        _exit((sigismember(&after, SIGUSR2) ? 1 : 0) | (sigismember(&after, SIGQUIT) ? 2 : 0) |
              (hits == 2 ? 4 : 0));
    }
    siginfo_t si;
    wait_stop(pid, &si);
    ptrace(PTRACE_CONT, pid, 0, 0);
    int sig = wait_stop(pid, &si); /* the signal-delivery stop of raise(SIGUSR1) */
    printf("stop %d\n", sig);
    /* Single-stepping with a signal: does it stop at the handler's first instruction (not yet
     * executed), or after executing it */
    ptrace(PTRACE_SINGLESTEP, pid, 0, SIGUSR1);
    sig = wait_stop(pid, &si);
    printf("step into handler: sig %d code %d at %s\n", sig, si.si_code, where(pc_of(pid)));
    ptrace(PTRACE_SINGLESTEP, pid, 0, 0);
    sig = wait_stop(pid, &si);
    printf("next step: sig %d code %d at %s\n", sig, si.si_code, where(pc_of(pid)));
    ptrace(PTRACE_CONT, pid, 0, 0);
    /* The child waits in sigsuspend; interrupt it with SIGUSR1 and look at the mask in the
     * signal-delivery stop */
    usleep(100000);
    kill(pid, SIGUSR1);
    sig = wait_stop(pid, &si);
    uint64_t mask;
    ptrace(PTRACE_GETSIGMASK, pid, 8, &mask);
    printf("in sigsuspend: stop %d, mask has INT %d USR2 %d\n", sig, !!(mask & (1ull << (SIGINT - 1))),
           !!(mask & (1ull << (SIGUSR2 - 1))));
    /* The tracer changed the mask: after the handler returns that mask is in effect, and the one
     * from before sigsuspend is no longer restored */
    mask = 1ull << (SIGQUIT - 1);
    ptrace(PTRACE_SETSIGMASK, pid, 8, &mask);
    ptrace(PTRACE_CONT, pid, 0, SIGUSR1);
    int st;
    waitpid(pid, &st, 0);
    printf("exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    return 0;
}
C
gcc -O1 -o sigstep sigstep.c && ./sigstep

# /proc/<pid>/mem holds the address space from when it was opened, so after execve the old fd
# reads EOF; opening auxv, environ, mem and maps requires being able to trace that process (same
# user; a non-dumpable one cannot be opened)
cat > procmem.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile char secret[] = "before exec";

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    /* /proc/<pid>/mem holds the address space from when it was opened: after the child execves,
     * the old fd reads EOF */
    int go[2];
    pipe(go);
    pid_t pid = fork();
    if (pid == 0) {
        char c;
        read(go[0], &c, 1);
        execl("/bin/sleep", "sleep", "1", (char *)0);
        _exit(127);
    }
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/mem", pid);
    int fd = open(path, O_RDONLY);
    char buf[16] = {0};
    ssize_t n = pread(fd, buf, 11, (off_t)(unsigned long)secret);
    printf("before exec: %zd %s\n", n, buf);
    write(go[1], "x", 1);
    usleep(300000);
    memset(buf, 0, sizeof buf);
    n = pread(fd, buf, 11, (off_t)(unsigned long)secret);
    printf("after exec: %zd\n", n);
    int fd2 = open(path, O_RDONLY);
    printf("reopen after exec: %d\n", fd2 >= 0);
    waitpid(pid, NULL, 0);

    /* auxv, environ: cannot be opened when a process of the same user is not dumpable (it must be
     * possible to trace it in read mode) */
    int ready[2];
    pipe(ready);
    pid = fork();
    if (pid == 0) {
        setgid(1000);
        setuid(1000);
        prctl(PR_SET_DUMPABLE, 0);
        write(ready[1], "x", 1);
        pause();
        _exit(0);
    }
    char c;
    read(ready[0], &c, 1);
    pid_t peer = fork();
    if (peer == 0) {
        setgid(1000);
        setuid(1000);
        const char *names[] = {"auxv", "environ", "mem", "maps"};
        for (int i = 0; i < 4; i++) {
            snprintf(path, sizeof path, "/proc/%d/%s", pid, names[i]);
            int f = open(path, O_RDONLY);
            printf("non-dumpable %s: %s\n", names[i], f >= 0 ? "opened" : strerror(errno));
        }
        _exit(0);
    }
    waitpid(peer, NULL, 0);
    kill(pid, SIGKILL);
    waitpid(pid, NULL, 0);
    return 0;
}
C
gcc -O1 -o procmem procmem.c && ./procmem

# The tracer exits without detaching: if the stop was already collected by wait, the signal in a
# signal-delivery stop is discarded and the process keeps running (delve exits after attach
# without detaching); one not yet collected is delivered as usual; one in group-stop stays stopped
cat > implicit.c <<'C'
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/wait.h>
#include <unistd.h>

static void on_usr1(int sig)
{
    (void)sig;
    write(1, "  child: handler ran\n", 21);
}

static char state(pid_t p)
{
    char path[64], buf[256], c = '?';
    snprintf(path, sizeof path, "/proc/%d/stat", p);
    FILE *f = fopen(path, "r");
    if (f && fgets(buf, sizeof buf, f))
        sscanf(buf, "%*d %*s %c", &c);
    if (f)
        fclose(f);
    return c;
}

/* sig: the tracer first lets it continue, then sends sig, and exits after the signal-delivery
 * stop for sig (when unwaited, it does not wait, and exits after 0.1 seconds); 0 means stopping
 * at the SIGSTOP of attach; -1 means injecting the SIGSTOP back and exiting after it enters
 * group-stop */
static void run(const char *what, int sig, int unwaited)
{
    pid_t child = fork();
    if (child == 0) {
        /* When Yama's ptrace_scope is 1, a sibling process must be allowed before it can attach */
        prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY);
        signal(SIGUSR1, on_usr1);
        for (;;)
            usleep(1000);
    }
    usleep(100000);
    pid_t tracer = fork();
    if (tracer == 0) {
        int st = 0;
        ptrace(PTRACE_ATTACH, child, 0, 0);
        waitpid(child, &st, 0);
        if (sig > 0) {
            ptrace(PTRACE_CONT, child, 0, 0);
            kill(child, sig);
            if (unwaited) {
                usleep(100000);
                printf("%s: tracer exits without waiting\n", what);
                fflush(stdout);
                _exit(0);
            }
            waitpid(child, &st, 0);
        } else if (sig < 0) {
            ptrace(PTRACE_CONT, child, 0, SIGSTOP);
            waitpid(child, &st, 0);
        }
        printf("%s: tracee stopped by %s\n", what, WIFSTOPPED(st) ? strsignal(WSTOPSIG(st)) : "?");
        fflush(stdout);
        _exit(0);
    }
    waitpid(tracer, NULL, 0);
    usleep(200000);
    int st;
    if (waitpid(child, &st, WNOHANG) == child && WIFSIGNALED(st))
        printf("  after tracer exit: killed by %s\n", strsignal(WTERMSIG(st)));
    else {
        /* It is in a usleep loop, so whether R or S is read depends on timing: only check whether
         * it is stopped */
        char c = state(child);
        printf("  after tracer exit: %s\n", c == 'T' || c == 't' ? "stopped" : c == 'R' || c == 'S' ? "running" : "?");
    }
    fflush(stdout);
    kill(child, SIGKILL);
    waitpid(child, NULL, 0);
}

int main(void)
{
    run("attach stop", 0, 0);
    run("SIGUSR1 with handler", SIGUSR1, 0);
    run("SIGTERM default", SIGTERM, 0);
    run("group stop", -1, 0);
    run("SIGUSR1 not waited for", SIGUSR1, 1);
    run("SIGTERM not waited for", SIGTERM, 1);
    return 0;
}
C
gcc -O1 -o implicit implicit.c && ./implicit

# Detaching from a thread stopped at a syscall's entry lets it run that syscall: here a write of
# 3 bytes and a getppid
cat > detachentry.c <<'C'
#include <elf.h>
#include <stdio.h>
#include <sys/ptrace.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

/* Stops the child at the entry of syscall `nr` and detaches there */
static void detach_at(pid_t child, unsigned long nr)
{
    int st;
    ptrace(PTRACE_SEIZE, child, 0, 0);
    ptrace(PTRACE_INTERRUPT, child, 0, 0);
    waitpid(child, &st, __WALL);
    for (int i = 0; i < 100; i++) {
        ptrace(PTRACE_SYSCALL, child, 0, 0);
        waitpid(child, &st, __WALL);
        unsigned long long r[34];
        struct iovec io = {r, sizeof r};
        ptrace(PTRACE_GETREGSET, child, NT_PRSTATUS, &io);
        if (r[8] == nr && r[7] == 0)
            break;
    }
    ptrace(PTRACE_DETACH, child, 0, 0);
}

int main(void)
{
    int go[2], out[2];
    pipe(go);
    pipe(out);
    pid_t child = fork();
    if (child == 0) {
        char x;
        read(go[0], &x, 1);
        long w = write(out[1], "abc", 3);
        long p = getppid();
        char line[64];
        int n = snprintf(line, sizeof line, "write %ld, getppid is the parent: %d\n", w, p == getppid());
        write(out[1], line, n);
        _exit(0);
    }
    /* The child waits in read until the parent, its tracer, has seized it; a helper then lets it
     * go, and the first syscall after the read is the write */
    if (fork() == 0) {
        usleep(200000);
        write(go[1], "g", 1);
        _exit(0);
    }
    detach_at(child, 64);
    int st;
    waitpid(child, &st, 0);
    wait(&st);
    char buf[128];
    int n = read(out[0], buf, sizeof buf - 1);
    buf[n > 0 ? n : 0] = 0;
    printf("%s", buf);
    return 0;
}
C
gcc -O1 -o detachentry detachentry.c && ./detachentry
