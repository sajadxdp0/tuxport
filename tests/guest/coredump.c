/*
 * Guest test program for core dumps: build with musl-gcc -static and run it copied into the store
 * (it needs a filesystem). After a child crashes, the parent checks the wait status and the core
 * file's contents: the ELF header, written and unwritten mappings, each thread's NT_PRSTATUS,
 * NT_PRPSINFO, NT_SIGINFO, NT_FILE; then it checks core_pattern and truncation by RLIMIT_CORE.
 * Each item that passes prints "ok <name>"; a failure prints the reason and exits with 1; when all
 * pass it prints "all ok" at the end.
 */
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/wait.h>
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

#define DIR "/tmp/coredump-test"

static void ok(const char *name)
{
    printf("ok %s\n", name);
    fflush(stdout);
}

static void *idle(void *arg)
{
    (void)arg;
    for (;;)
        pause();
    return NULL;
}

/* Child: writes markers into two pieces of anonymous memory, the second MADV_DONTDUMP, starts
 * another thread, then accesses a null pointer. The two addresses are passed to the parent
 * through a pipe */
static void crash(int out, rlim_t limit)
{
    struct rlimit rl = {limit, RLIM_INFINITY};
    CHECK(setrlimit(RLIMIT_CORE, &rl) == 0, "setrlimit");
    char *a = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *b = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(a != MAP_FAILED && b != MAP_FAILED, "mmap");
    strcpy(a + 4096, "MARKER-A");
    strcpy(b, "MARKER-B");
    CHECK(madvise(b, 8192, MADV_DONTDUMP) == 0, "madvise");
    uint64_t addrs[2] = {(uint64_t)a, (uint64_t)b};
    CHECK(write(out, addrs, sizeof addrs) == sizeof addrs, "write");
    pthread_t t;
    CHECK(pthread_create(&t, NULL, idle, NULL) == 0, "pthread_create");
    usleep(50000);
    *(volatile int *)0 = 1;
}

/* Forks a child that crashes; returns its pid, the wait status and the addresses of the two
 * pieces */
static pid_t run_crash(rlim_t limit, int *status, uint64_t addrs[2])
{
    int p[2];
    CHECK(pipe(p) == 0, "pipe");
    pid_t pid = fork();
    CHECK(pid >= 0, "fork");
    if (pid == 0) {
        close(p[0]);
        crash(p[1], limit);
        _exit(0);
    }
    close(p[1]);
    CHECK(read(p[0], addrs, 16) == 16, "read addresses");
    close(p[0]);
    siginfo_t si;
    CHECK(waitid(P_PID, pid, &si, WEXITED | WNOWAIT) == 0, "waitid");
    CHECK(si.si_signo == SIGCHLD && si.si_status == SIGSEGV, "si_status %d", si.si_status);
    int code = si.si_code;
    CHECK(waitpid(pid, status, 0) == pid, "waitpid");
    CHECK(WIFSIGNALED(*status) && WTERMSIG(*status) == SIGSEGV, "status %#x", *status);
    CHECK((code == CLD_DUMPED) == !!WCOREDUMP(*status), "si_code %d, status %#x", code, *status);
    return pid;
}

static char *read_file(const char *path, size_t *len)
{
    int fd = open(path, O_RDONLY);
    CHECK(fd >= 0, "open %s", path);
    struct stat st;
    CHECK(fstat(fd, &st) == 0, "fstat");
    char *buf = malloc(st.st_size + 1);
    size_t done = 0;
    while (done < (size_t)st.st_size) {
        ssize_t n = read(fd, buf + done, st.st_size - done);
        CHECK(n > 0, "read core");
        done += n;
    }
    close(fd);
    *len = done;
    return buf;
}

/* Counts the notes of type `type` in PT_NOTE and returns the contents of the nth one */
static const char *find_note(const char *core, const Elf64_Phdr *note, uint32_t type, int nth,
                             int *count, uint32_t *size)
{
    const char *p = core + note->p_offset, *end = p + note->p_filesz, *found = NULL;
    *count = 0;
    while (p + 12 <= end) {
        const Elf64_Nhdr *n = (const Elf64_Nhdr *)p;
        const char *desc = p + 12 + ((n->n_namesz + 3) & ~3u);
        if (n->n_type == type) {
            if (*count == nth) {
                found = desc;
                *size = n->n_descsz;
            }
            ++*count;
        }
        p = desc + ((n->n_descsz + 3) & ~3u);
    }
    return found;
}

static void test_layout(void)
{
    int status;
    uint64_t addrs[2];
    pid_t pid = run_crash(RLIM_INFINITY, &status, addrs);
    CHECK(WCOREDUMP(status), "no core dumped: status %#x", status);
    size_t len;
    char *core = read_file(DIR "/core", &len);
    const Elf64_Ehdr *eh = (const Elf64_Ehdr *)core;
    CHECK(memcmp(eh->e_ident, ELFMAG, SELFMAG) == 0 && eh->e_ident[EI_CLASS] == ELFCLASS64,
          "ELF magic");
    CHECK(eh->e_type == ET_CORE && eh->e_machine == EM_AARCH64, "type %d, machine %d",
          eh->e_type, eh->e_machine);
    const Elf64_Phdr *ph = (const Elf64_Phdr *)(core + eh->e_phoff);
    CHECK(ph[0].p_type == PT_NOTE, "first program header %u", ph[0].p_type);
    int found_a = 0, found_b = 0;
    for (int i = 1; i < eh->e_phnum; i++) {
        CHECK(ph[i].p_type == PT_LOAD, "program header %d is %u", i, ph[i].p_type);
        CHECK(ph[i].p_filesz == 0 || ph[i].p_offset + ph[i].p_filesz <= len, "segment %d past the end", i);
        if (ph[i].p_vaddr == addrs[0]) {
            CHECK(ph[i].p_filesz == 8192 && ph[i].p_memsz == 8192 && ph[i].p_flags == (PF_R | PF_W),
                  "dumped mapping: filesz %lu, flags %u", (unsigned long)ph[i].p_filesz,
                  ph[i].p_flags);
            CHECK(strcmp(core + ph[i].p_offset + 4096, "MARKER-A") == 0, "contents of the mapping");
            found_a = 1;
        }
        if (ph[i].p_vaddr == addrs[1]) {
            CHECK(ph[i].p_filesz == 0 && ph[i].p_memsz == 8192, "MADV_DONTDUMP mapping: filesz %lu",
                  (unsigned long)ph[i].p_filesz);
            found_b = 1;
        }
    }
    CHECK(found_a && found_b, "mappings not found");
    ok("core program headers");

    int count;
    uint32_t size;
    const char *pr = find_note(core, &ph[0], NT_PRSTATUS, 0, &count, &size);
    CHECK(count == 2 && size == 392, "NT_PRSTATUS: %d notes, %u bytes", count, size);
    int32_t pid_note, signo;
    uint16_t cursig;
    memcpy(&signo, pr, 4);
    memcpy(&cursig, pr + 12, 2);
    memcpy(&pid_note, pr + 32, 4);
    CHECK(pid_note == pid && signo == SIGSEGV && cursig == SIGSEGV, "pid %d, signal %d/%d",
          pid_note, signo, cursig);
    const char *pr2 = find_note(core, &ph[0], NT_PRSTATUS, 1, &count, &size);
    memcpy(&pid_note, pr2 + 32, 4);
    CHECK(pid_note != pid && pid_note > 0, "second thread %d", pid_note);
    const char *ps = find_note(core, &ph[0], NT_PRPSINFO, 0, &count, &size);
    CHECK(count == 1 && size == 136 && strcmp(ps + 40, "coredump") == 0, "NT_PRPSINFO %d %u %.16s",
          count, size, ps + 40);
    const siginfo_t *si = (const siginfo_t *)find_note(core, &ph[0], NT_SIGINFO, 0, &count, &size);
    CHECK(count == 1 && size == 128 && si->si_signo == SIGSEGV && si->si_code == SEGV_MAPERR &&
              si->si_addr == NULL,
          "NT_SIGINFO %d/%d", si->si_signo, si->si_code);
    find_note(core, &ph[0], NT_PRFPREG, 0, &count, &size);
    CHECK(count == 2 && size == 528, "NT_PRFPREG %d %u", count, size);
    find_note(core, &ph[0], NT_AUXV, 0, &count, &size);
    CHECK(count == 1 && size > 0 && size % 16 == 0, "NT_AUXV");
    const char *files = find_note(core, &ph[0], NT_FILE, 0, &count, &size);
    CHECK(count == 1 && memmem(files, size, "/tmp/coredump", 14), "NT_FILE");
    ok("core notes");
    free(core);
    unlink(DIR "/core");
}

static void write_file(const char *path, const char *s)
{
    int fd = open(path, O_WRONLY | O_TRUNC);
    CHECK(fd >= 0 && write(fd, s, strlen(s)) == (ssize_t)strlen(s), "write %s", path);
    close(fd);
}

static void test_pattern_and_limit(void)
{
    int status;
    uint64_t addrs[2];
    write_file("/proc/sys/kernel/core_pattern", "core.%p.%s\n");
    pid_t pid = run_crash(RLIM_INFINITY, &status, addrs);
    char name[64];
    snprintf(name, sizeof name, DIR "/core.%d.11", pid);
    struct stat st;
    CHECK(WCOREDUMP(status) && stat(name, &st) == 0, "%s", name);
    unlink(name);
    write_file("/proc/sys/kernel/core_pattern", "core");
    ok("core_pattern");

    /* A limit under one page: nothing is written; one page: truncated still counts as written */
    run_crash(100, &status, addrs);
    CHECK(!WCOREDUMP(status) && stat(DIR "/core", &st) < 0, "core under one page");
    run_crash(4096, &status, addrs);
    CHECK(WCOREDUMP(status) && stat(DIR "/core", &st) == 0 && st.st_size <= 4096,
          "truncated core: %ld bytes", (long)st.st_size);
    unlink(DIR "/core");
    ok("RLIMIT_CORE");
}

int main(void)
{
    mkdir(DIR, 0755);
    CHECK(chdir(DIR) == 0, "chdir");
    unlink(DIR "/core");
    test_layout();
    test_pattern_and_limit();
    printf("all ok\n");
    return 0;
}
