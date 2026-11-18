/*
 * Guest test program for the kernel log's reports of unhandled exceptions (arm64 Linux's
 * arm64_show_signal): build with musl-gcc -static and run it as root, copied into the store under
 * the name crashlog (the reports name the program's file).
 *
 * debug.exception-trace starts at 0 and nothing is reported; after writing 1, children crash in
 * each way the kernel layer reports, and each report read back from /dev/kmsg must be the line
 * Linux 6.1 writes (comm[pid], exception class and ESR, what happened, the file mapped at or above
 * the pc) followed by the registers. Children with a handler or a tracer are not reported, and
 * after 10 reports within 5 seconds the next is dropped. The ESRs are those Tuxport synthesizes:
 * Linux reports the translation-table level the walk stopped at (often 2 for an address with no
 * page tables), and a never-touched page as a translation fault even when its mapping forbids
 * the access.
 *
 * Each item that passes prints "ok <name>"; a failure prints the reason and exits with 1; when all
 * pass it prints "all ok" at the end.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            printf("FAIL %s:%d: ", __func__, __LINE__);                                            \
            printf(__VA_ARGS__);                                                                   \
            printf(" (errno %d)\n", errno);                                                        \
            exit(1);                                                                               \
        }                                                                                          \
    } while (0)

#define TRACE "/proc/sys/debug/exception-trace"
#define RECORDS 64

static int kmsg;
/* An unmapped address, hidden from the compiler's bounds checks */
static volatile int *volatile unmapped = (volatile int *)16;

static void ok(const char *name)
{
    printf("ok %s\n", name);
    fflush(stdout);
}

/* One /dev/kmsg record: its level and text */
struct record {
    int level;
    char text[1024];
};

/* Reads the records logged since the last call, at most RECORDS */
static int read_records(struct record *out)
{
    int n = 0;
    char buf[2048];
    for (;;) {
        ssize_t len = read(kmsg, buf, sizeof buf - 1);
        if (len < 0 && errno == EAGAIN)
            return n;
        CHECK(len > 0, "read /dev/kmsg");
        buf[len] = 0;
        char *text = strchr(buf, ';');
        CHECK(text != NULL, "record without text: %s", buf);
        *strchrnul(text, '\n') = 0;
        CHECK(n < RECORDS, "too many records");
        out[n].level = atoi(buf) & 7;
        snprintf(out[n].text, sizeof out[n].text, "%s", text + 1);
        n++;
    }
}

static void write_trace(const char *value)
{
    int fd = open(TRACE, O_WRONLY);
    CHECK(fd >= 0, "open " TRACE);
    CHECK(write(fd, value, strlen(value)) == (ssize_t)strlen(value), "write " TRACE);
    close(fd);
}

static void on_segv(int sig)
{
    (void)sig;
    _exit(3);
}

enum crash {
    WRITE_UNMAPPED,
    WRITE_READONLY,
    UNDEFINED,
    BRK,
    PC_ALIGNMENT,
    PAST_EOF,
    EXEC_UNMAPPED,
    IGNORED,
    SP_ALIGNMENT,
    BAD_SIGRETURN,
    HANDLED,
    TRACED,
};

static void crash(enum crash what)
{
    switch (what) {
    case IGNORED:
        signal(SIGSEGV, SIG_IGN);
        /* fall through */
    case WRITE_UNMAPPED:
        *unmapped = 1;
        break;
    case WRITE_READONLY: {
        volatile char *m = mmap(0, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        m[0] = 1;
        break;
    }
    case UNDEFINED:
        __asm__ volatile(".inst 0x00000000");
        break;
    case BRK:
        __asm__ volatile("brk #0x3e8");
        break;
    case PC_ALIGNMENT:
        ((void (*)(void))(uintptr_t)0x1002)();
        break;
    case PAST_EOF: {
        int fd = open("/tmp/crashlog-empty", O_RDWR | O_CREAT | O_TRUNC, 0600);
        volatile char *m = mmap(0, 8192, PROT_READ, MAP_SHARED, fd, 0);
        (void)m[0];
        break;
    }
    case EXEC_UNMAPPED:
        ((void (*)(void))(uintptr_t)0x10000)();
        break;
    case SP_ALIGNMENT:
        __asm__ volatile("mov x9, sp\n sub x9, x9, #8\n mov sp, x9\n ldr x0, [sp]"
                         :
                         :
                         : "x9", "x0", "memory");
        break;
    case BAD_SIGRETURN:
        /* The stack holds no signal frame */
        __asm__ volatile("mov x8, #139\n svc #0" : : : "x8", "x0", "memory");
        break;
    case HANDLED:
        signal(SIGSEGV, on_segv);
        *unmapped = 1;
        break;
    case TRACED:
        CHECK(ptrace(PTRACE_TRACEME, 0, 0, 0) == 0, "PTRACE_TRACEME");
        *unmapped = 1;
        break;
    }
    _exit(0);
}

/* Runs a child that crashes; returns its pid after it has been reaped */
static pid_t run(enum crash what, int signal)
{
    pid_t pid = fork();
    CHECK(pid >= 0, "fork");
    if (pid == 0)
        crash(what);
    int status;
    CHECK(waitpid(pid, &status, 0) == pid, "waitpid");
    if (what == TRACED) {
        /* Stopped at the fault: let the signal kill it */
        CHECK(WIFSTOPPED(status) && WSTOPSIG(status) == SIGSEGV, "status %#x", status);
        CHECK(ptrace(PTRACE_CONT, pid, 0, SIGSEGV) == 0, "PTRACE_CONT");
        CHECK(waitpid(pid, &status, 0) == pid, "waitpid");
    }
    if (what == HANDLED)
        CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 3, "status %#x", status);
    else
        CHECK(WIFSIGNALED(status) && WTERMSIG(status) == signal, "status %#x", status);
    return pid;
}

/* A child crashes and nothing is reported */
static void unreported(const char *name, enum crash what, int signal)
{
    struct record r[RECORDS];
    run(what, signal);
    int n = read_records(r);
    for (int i = 0; i < n; i++)
        CHECK(strstr(r[i].text, "unhandled exception") == NULL, "%s: %s", name, r[i].text);
    ok(name);
}

/*
 * A child crashes and is reported: "crashlog[<pid>]: unhandled exception: <what>", then " in
 * crashlog[<start>+<length>]" when the pc is at or below the program's mappings, then the CPU
 * line, pstate, pc (`pc`, unless 0), lr, sp and ten lines of registers
 */
static void reported(const char *name, enum crash what, int signal, const char *text, int in,
                     unsigned long pc)
{
    struct record r[RECORDS];
    pid_t pid = run(what, signal);
    int n = read_records(r);
    char head[256];
    snprintf(head, sizeof head, "crashlog[%d]: unhandled exception: %s", pid, text);
    int i = 0;
    while (i < n && strncmp(r[i].text, head, strlen(head)) != 0)
        i++;
    CHECK(i < n, "%s: no report starting with %s among %d records", name, head, n);
    CHECK(n - i == 16, "%s: %d records from the report", name, n - i);
    CHECK(r[i].level == 6, "%s: level %d", name, r[i].level);
    const char *rest = r[i].text + strlen(head);
    unsigned long start, len;
    char end;
    if (in)
        CHECK(sscanf(rest, " in crashlog[%lx+%lx%c", &start, &len, &end) == 3 && end == ']' &&
                  len > 0,
              "%s: %s", name, r[i].text);
    else
        CHECK(*rest == 0, "%s: %s", name, r[i].text);

    int cpu, line_pid, used = 0;
    CHECK(sscanf(r[i + 1].text, "CPU: %d PID: %d Comm: crashlog Not tainted 6.1.0-tuxport #1%n",
                 &cpu, &line_pid, &used) == 2 &&
              line_pid == pid && r[i + 1].text[used] == 0,
          "%s: %s", name, r[i + 1].text);
    CHECK(strncmp(r[i + 2].text, "pstate: ", 8) == 0, "%s: %s", name, r[i + 2].text);
    char line[64];
    if (pc) {
        snprintf(line, sizeof line, "pc : %016lx", pc);
        CHECK(strcmp(r[i + 3].text, line) == 0, "%s: %s", name, r[i + 3].text);
    }
    CHECK(strncmp(r[i + 4].text, "lr : ", 5) == 0, "%s: %s", name, r[i + 4].text);
    CHECK(strncmp(r[i + 5].text, "sp : ", 5) == 0, "%s: %s", name, r[i + 5].text);
    for (int j = 0; j < 10; j++) {
        int top = 29 - 3 * j;
        snprintf(line, sizeof line, "x%-2d: ", top);
        CHECK(strncmp(r[i + 6 + j].text, line, 5) == 0 && strlen(r[i + 6 + j].text) == 65,
              "%s: %s", name, r[i + 6 + j].text);
    }
    for (int j = i + 1; j < n; j++)
        CHECK(r[j].level == 4, "%s: level %d: %s", name, r[j].level, r[j].text);
    ok(name);
}

int main(void)
{
    char value[16] = {0};
    int fd = open(TRACE, O_RDONLY);
    CHECK(fd >= 0 && read(fd, value, sizeof value - 1) > 0, "read " TRACE);
    close(fd);
    CHECK(strcmp(value, "0\n") == 0, "exception-trace starts at %s", value);
    kmsg = open("/dev/kmsg", O_RDONLY | O_NONBLOCK);
    CHECK(kmsg >= 0, "open /dev/kmsg");
    CHECK(lseek(kmsg, 0, SEEK_END) >= 0, "lseek");
    unreported("off", WRITE_UNMAPPED, SIGSEGV);

    write_trace("1\n");
    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    reported("write-unmapped", WRITE_UNMAPPED, SIGSEGV,
             "DABT (lower EL), ESR 0x0000000092000047, level 3 translation fault", 1, 0);
    reported("write-readonly", WRITE_READONLY, SIGSEGV,
             "DABT (lower EL), ESR 0x000000009200004f, level 3 permission fault", 1, 0);
    reported("undefined", UNDEFINED, SIGILL, "undefined instruction", 1, 0);
    reported("brk", BRK, SIGTRAP, "User debug trap", 1, 0);
    /* The pc is below every mapping: Linux's find_vma names the first one above it */
    reported("pc-alignment", PC_ALIGNMENT, SIGBUS,
             "PC Alignment, ESR 0x000000008a000000, SP/PC alignment exception", 1, 0x1002);
    reported("past-eof", PAST_EOF, SIGBUS,
             "DABT (lower EL), ESR 0x0000000092000007, level 3 translation fault", 1, 0);
    reported("exec-unmapped", EXEC_UNMAPPED, SIGSEGV,
             "IABT (lower EL), ESR 0x0000000082000007, level 3 translation fault", 1, 0x10000);
    reported("ignored", IGNORED, SIGSEGV,
             "DABT (lower EL), ESR 0x0000000092000047, level 3 translation fault", 1, 0);
    reported("sp-alignment", SP_ALIGNMENT, SIGBUS,
             "SP Alignment, ESR 0x000000009a000000, SP/PC alignment exception", 1, 0);
    /* Tuxport checks the frame before restoring anything, so the registers are the caller's */
    reported("bad-sigreturn", BAD_SIGRETURN, SIGSEGV, "illegal memory access", 1, 0);
    unreported("handled", HANDLED, 0);
    unreported("traced", TRACED, SIGSEGV);
    /* Ten reports passed within 5 seconds (unless this machine was too slow to tell) */
    clock_gettime(CLOCK_MONOTONIC, &t1);
    if (t1.tv_sec - t0.tv_sec < 4) {
        unreported("ratelimited", WRITE_UNMAPPED, SIGSEGV);
    } else {
        ok("ratelimited (skipped: too slow)");
    }

    write_trace("0\n");
    printf("all ok\n");
    return 0;
}
