# After fork the memory of parent and child does not affect each other (copy-on-write): several
# children and the parent writing at once; a multithreaded parent forking while other threads keep
# writing, with the child seeing a snapshot of one instant; lent memory being unmapped, moved and
# discarded; lent pages returning to their own permissions after the child ends; shared pages in
# smaps; locks and condition variables in the child; fork nested level upon level; python's
# os.fork; the commit estimated over merged VMAs at fork; a 1 TB MAP_NORESERVE mapping with only
# a few pages touched
set -e
cd /tmp
cat > cow.c <<'C'
#define _GNU_SOURCE
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE 4096
#define PAGES 2048 /* 8 MB */

static unsigned char *mem;

static int check(unsigned char *p, size_t pages, unsigned char want)
{
    for (size_t i = 0; i < pages * PAGE; i += 61)
        if (p[i] != want)
            return 1;
    return 0;
}

static int wait_all(int n)
{
    int bad = 0, st;
    for (int i = 0; i < n; i++) {
        wait(&st);
        bad += !WIFEXITED(st) || WEXITSTATUS(st) != 0;
    }
    return bad;
}

/* Several children and the parent write at once: each sees only its own changes */
static int concurrent_writers(void)
{
    memset(mem, 'p', PAGES * PAGE);
    int fds[2];
    pipe(fds);
    for (int c = 0; c < 4; c++) {
        if (fork() == 0) {
            char go;
            read(fds[0], &go, 1);
            /* Write half of the pages; the other half must still hold what they had at fork */
            for (int i = c % 2; i < PAGES; i += 2)
                mem[(size_t)i * PAGE + c] = 'a' + c;
            int bad = 0;
            for (int i = 0; i < PAGES; i++) {
                unsigned char want = (i % 2 == c % 2) ? 'a' + c : 'p';
                bad += mem[(size_t)i * PAGE + c] != want;
                bad += mem[(size_t)i * PAGE + 100] != 'p';
            }
            _exit(bad != 0);
        }
    }
    for (int c = 0; c < 4; c++)
        write(fds[1], "g", 1);
    /* The parent writes while the children check */
    for (int i = 0; i < PAGES; i += 3)
        mem[(size_t)i * PAGE + 100] = 'q';
    int bad = wait_all(4);
    for (int i = 0; i < PAGES; i++)
        bad += mem[(size_t)i * PAGE + 100] != (i % 3 ? 'p' : 'q') || mem[(size_t)i * PAGE] != 'p';
    close(fds[0]);
    close(fds[1]);
    return bad;
}

/* Threads write the count into each page in order, then add one: at any instant the first few
 * pages hold v and the rest v - 1 */
static atomic_int stop;
static unsigned *bufs[3];
#define TPAGES 64

static void *scribbler(void *arg)
{
    unsigned *b = arg;
    for (unsigned v = 1; !atomic_load(&stop); v++)
        for (int i = 0; i < TPAGES; i++)
            b[i * (PAGE / 4)] = v;
    return NULL;
}

static int consistent(unsigned *b)
{
    unsigned first = b[0];
    int i = 1;
    while (i < TPAGES && b[i * (PAGE / 4)] == first)
        i++;
    while (i < TPAGES && b[i * (PAGE / 4)] == first - 1)
        i++;
    return i == TPAGES;
}

static int threads_and_fork(void)
{
    pthread_t t[3];
    for (int i = 0; i < 3; i++) {
        bufs[i] = calloc(TPAGES, PAGE);
        pthread_create(&t[i], NULL, scribbler, bufs[i]);
    }
    int bad = 0;
    for (int round = 0; round < 40; round++) {
        pid_t pid = fork();
        if (pid == 0) {
            int ok = 1;
            for (int i = 0; i < 3; i++) {
                ok &= consistent(bufs[i]);
                /* The child writes for itself: its snapshot is not affected by the parent's
                 * threads */
                unsigned snap = bufs[i][0];
                usleep(2000);
                ok &= bufs[i][0] == snap;
                bufs[i][0] = 0;
            }
            _exit(!ok);
        }
        int st;
        waitpid(pid, &st, 0);
        bad += !WIFEXITED(st) || WEXITSTATUS(st) != 0;
    }
    atomic_store(&stop, 1);
    for (int i = 0; i < 3; i++) {
        pthread_join(t[i], NULL);
        bad += !consistent(bufs[i]);
    }
    return bad;
}

/* Lent memory is unmapped by the parent, moved away by mremap, or MADV_DONTNEED'd: the child's
 * contents do not change */
static int unmap_move_discard(void)
{
    size_t len = 64 * PAGE;
    unsigned char *a = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    unsigned char *b = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    unsigned char *c = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memset(a, 'a', len);
    memset(b, 'b', len);
    memset(c, 'c', len);
    int fds[2], back[2];
    pipe(fds);
    pipe(back);
    pid_t pid = fork();
    if (pid == 0) {
        char x;
        read(fds[0], &x, 1);
        int bad = check(a, 64, 'a') + check(b, 64, 'b') + check(c, 64, 'c');
        /* The child changes things too: the parent side is not affected */
        mprotect(c, len, PROT_READ);
        mprotect(c, len, PROT_READ | PROT_WRITE);
        memset(c, 'C', len / 2);
        madvise(b, len / 2, MADV_DONTNEED);
        bad += b[0] != 0 || b[len - 1] != 'b';
        write(back[1], "d", 1);
        _exit(bad != 0);
    }
    munmap(a, len / 2);
    unsigned char *moved = mremap(b, len, 2 * len, MREMAP_MAYMOVE);
    madvise(c + len / 2, len / 2, MADV_DONTNEED);
    /* Map again where it was unmapped, and write other contents */
    unsigned char *again =
        mmap(a, len / 2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    memset(again, 'z', len / 2);
    memset(moved, 'm', len);
    write(fds[1], "g", 1);
    char x;
    read(back[0], &x, 1);
    int bad = wait_all(1);
    bad += check(c, 32, 'c') + (c[len - 1] != 0) + check(moved, 64, 'm');
    return bad;
}

/* After the child ends and returns the borrowed pages, the parent's pages get back the
 * permissions of the mapping each is in. The pieces of host memory that one mmap was split into
 * by mprotect are contiguous, so they cannot all be restored with the first piece's permissions:
 * a guard page's PROT_NONE would make all the following pages inaccessible, and a writable piece
 * would make a following read-only piece writable too */
static sigjmp_buf probe_jmp;

static void probe_fault(int sig)
{
    (void)sig;
    siglongjmp(probe_jmp, 1);
}

static int probe(volatile unsigned char *p, int write)
{
    if (sigsetjmp(probe_jmp, 1))
        return 0;
    if (write)
        *p = 'w';
    else
        (void)*p;
    return 1;
}

static void returned_loans(void)
{
    /* A guard page, two writable pages, one read-only; one writable, one read-only; finally, like
     * a thread's signal stack, a mapping that starts with a never-written guard page, placed
     * right below the previous one */
    int flags = MAP_PRIVATE | MAP_ANONYMOUS;
    unsigned char *m = mmap(NULL, 4 * PAGE, PROT_READ | PROT_WRITE, flags, -1, 0);
    unsigned char *n = mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE, flags, -1, 0);
    unsigned char *s = mmap(NULL, 4 * PAGE, PROT_READ | PROT_WRITE, flags | MAP_STACK, -1, 0);
    memset(m, 'r', 4 * PAGE);
    memset(n, 'r', 2 * PAGE);
    mprotect(m, PAGE, PROT_NONE);
    mprotect(m + 3 * PAGE, PAGE, PROT_READ);
    mprotect(n + PAGE, PAGE, PROT_READ);
    mprotect(s, PAGE, PROT_NONE);
    memset(s + PAGE, 's', 3 * PAGE);
    pid_t pid = fork();
    if (pid == 0)
        _exit(0);
    wait_all(1);
    struct sigaction sa = {.sa_handler = probe_fault}, old;
    sigaction(SIGSEGV, &sa, &old);
    printf("returned loans: guard read %d, rw write %d %d, ro read %d write %d; rw write %d, ro write %d; "
           "stack guard read %d, stack write %d\n",
           probe(m, 0), probe(m + PAGE, 1), probe(m + 2 * PAGE, 1), probe(m + 3 * PAGE, 0),
           probe(m + 3 * PAGE, 1), probe(n, 1), probe(n + PAGE, 1), probe(s, 0), probe(s + PAGE, 1));
    sigaction(SIGSEGV, &old, NULL);
}

/* After fork both sides share the written pages: smaps counts them as shared dirty pages, and
 * each side's Pss counts half; after one side writes, each has its own. redis and valkey check
 * the kernel this way when starting on arm64 (write a page, MADV_FREE, write again, then after
 * fork the child looks at Shared_Dirty), and refuse to start if it is 0 */
static void smaps_of(const char *who, unsigned char *addr)
{
    FILE *f = fopen("/proc/self/smaps", "r");
    char line[256];
    int in = 0;
    long rss = -1, pss = -1, shared = -1, private = -1;
    while (fgets(line, sizeof line, f)) {
        unsigned long lo, hi;
        if (sscanf(line, "%lx-%lx ", &lo, &hi) == 2) {
            in = (unsigned long)addr >= lo && (unsigned long)addr < hi;
            continue;
        }
        if (in) {
            sscanf(line, "Rss: %ld", &rss);
            sscanf(line, "Pss: %ld", &pss);
            sscanf(line, "Shared_Dirty: %ld", &shared);
            sscanf(line, "Private_Dirty: %ld", &private);
        }
    }
    fclose(f);
    printf("%s: Rss %ld, Pss %ld, Shared_Dirty %ld, Private_Dirty %ld\n", who, rss, pss, shared, private);
    fflush(stdout);
}

static void shared_in_smaps(void)
{
    unsigned char *p = mmap(NULL, 3 * PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    unsigned char *q = p + PAGE;
    mprotect(q, PAGE, PROT_READ | PROT_WRITE);
    *q = 1;
    madvise(q, PAGE, MADV_FREE);
    *q = 2;
    int go[2], back[2];
    char c = 0;
    pipe(go);
    pipe(back);
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        smaps_of("child after fork", q);
        write(back[1], &c, 1);
        read(go[0], &c, 1);
        *q = 3;
        smaps_of("child after writing", q);
        write(back[1], &c, 1);
        _exit(0);
    }
    read(back[0], &c, 1);
    smaps_of("parent after fork", q);
    write(go[1], &c, 1);
    read(back[0], &c, 1);
    smaps_of("parent after the child wrote", q);
    wait_all(1);
}

/* Locks and condition variables in the child (the futexes are on pages borrowed at fork) */
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int turn;

static void *pingpong(void *arg)
{
    int me = (int)(long)arg;
    for (int i = 0; i < 2000; i++) {
        pthread_mutex_lock(&lock);
        while (turn % 2 != me)
            pthread_cond_wait(&cond, &lock);
        turn++;
        pthread_cond_broadcast(&cond);
        pthread_mutex_unlock(&lock);
    }
    return NULL;
}

static int locks_in_child(void)
{
    pthread_mutex_lock(&lock);
    turn = 0;
    pthread_mutex_unlock(&lock);
    pid_t pid = fork();
    if (pid == 0) {
        pthread_t t[2];
        for (long i = 0; i < 2; i++)
            pthread_create(&t[i], NULL, pingpong, (void *)i);
        for (int i = 0; i < 2; i++)
            pthread_join(t[i], NULL);
        _exit(turn != 4000);
    }
    return wait_all(1);
}

/* Level upon level: each level changes one page, then looks at the contents of the levels
 * above */
static int nested(int depth)
{
    mem[(size_t)depth * PAGE] = 'n';
    if (depth == 6) {
        for (int i = 0; i <= 6; i++)
            if (mem[(size_t)i * PAGE] != 'n')
                return 1;
        return mem[7 * PAGE] != 'p';
    }
    pid_t pid = fork();
    if (pid == 0)
        _exit(nested(depth + 1));
    mem[(size_t)(depth + 1) * PAGE] = 'x';
    int st;
    waitpid(pid, &st, 0);
    return !WIFEXITED(st) || WEXITSTATUS(st) != 0 || mem[(size_t)depth * PAGE] != 'n';
}

int main(void)
{
    mem = malloc(PAGES * PAGE);
    printf("concurrent writers: %d bad\n", concurrent_writers());
    printf("threads and fork: %d bad\n", threads_and_fork());
    printf("unmap, move, discard: %d bad\n", unmap_move_discard());
    returned_loans();
    shared_in_smaps();
    printf("locks in child: %d bad\n", locks_in_child());
    memset(mem, 'p', PAGES * PAGE);
    printf("nested: %d bad\n", nested(0));
    return 0;
}
C
gcc -O2 -o cow cow.c -pthread
./cow
# Commit estimate at fork (vm.overcommit_memory is 0, Linux's dup_mmap): writable private
# mappings are counted by merged VMA, and fork is ENOMEM when that exceeds the total of memory
# and swap. Two pieces of a little over half each, adjoining end to start (neither exceeds it on
# its own); with a gap, or with the second piece MAP_NORESERVE or read-only, they are not counted
# merged
cat > commit.c <<'C'
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sysinfo.h>
#include <sys/wait.h>
#include <unistd.h>

static unsigned long half;
static char *base;

static const char *try_fork(void)
{
    pid_t p = fork();
    if (p < 0)
        return strerror(errno);
    if (p == 0)
        _exit(0);
    waitpid(p, NULL, 0);
    return "ok";
}

static void case_(const char *name, unsigned long gap, int prot, int flags)
{
    char *a = mmap(base, half, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    char *b = mmap(base + half + gap, half, prot,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED | flags, -1, 0);
    if (a == MAP_FAILED || b == MAP_FAILED)
        printf("%s: mmap %s\n", name, strerror(errno));
    else
        printf("%s: fork %s\n", name, try_fork());
    munmap(base, 2 * half + gap);
}

int main(void)
{
    struct sysinfo si;
    sysinfo(&si);
    unsigned long limit = (si.totalram + si.totalswap) * (unsigned long)si.mem_unit;
    half = (limit / 2 / 4096 + 1) * 4096;
    base = mmap(NULL, 2 * half + (1 << 20), PROT_NONE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    munmap(base, 2 * half + (1 << 20));
    char *a = mmap(base, half, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    printf("one half: %s, fork %s\n", a == MAP_FAILED ? strerror(errno) : "mapped", try_fork());
    munmap(base, half);
    case_("adjacent", 0, PROT_READ | PROT_WRITE, 0);
    case_("with a gap", 1 << 20, PROT_READ | PROT_WRITE, 0);
    case_("second MAP_NORESERVE", 0, PROT_READ | PROT_WRITE, MAP_NORESERVE);
    case_("second read-only", 0, PROT_READ, 0);
    /* The old limit: a single mapping over the total is ENOMEM (the host refuses it) */
    char *big = mmap(NULL, limit + (1 << 20), PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    printf("one mapping over the limit: %s\n", big == MAP_FAILED ? strerror(errno) : "mapped");
    return 0;
}
C
gcc -O2 -o commit commit.c
./commit
# A 1 TB MAP_NORESERVE mapping with only a few pages touched (ASan's shadow memory works like
# this): fork succeeds, the child reads the contents, writes its own copy, and untouched pages
# are zero; after PROT_NONE and back, and after an mremap move, the contents are still there and
# a further fork sees them too; resident memory is only the few touched pages. Tuxport builds
# page tables on demand, and fork lends pages only where there are entries (round 179); fork
# used to ignore MAP_NORESERVE when requesting host memory for the child, so exceeding the total
# memory was ENOMEM
cat > sparse.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define LEN (1ul << 40)
static const unsigned long at[] = {0, LEN / 3 + 123, LEN / 2 + (5ul << 20), LEN - 1};

static int check(const char *p)
{
    for (int i = 0; i < 4; i++)
        if (p[at[i]] != 'a' + i)
            return 0;
    return 1;
}

static const char *fork_and_check(char *p)
{
    pid_t pid = fork();
    if (pid < 0)
        return strerror(errno);
    if (pid == 0) {
        int ok = check(p) && p[LEN / 4] == 0;
        p[at[1]] = 'z';
        _exit(ok ? 0 : 1);
    }
    int st = -1;
    waitpid(pid, &st, 0);
    return WIFEXITED(st) && WEXITSTATUS(st) == 0 && check(p) ? "child saw the pages" : "wrong";
}

static long rss_kb(void)
{
    FILE *f = fopen("/proc/self/status", "r");
    char line[256];
    long kb = -1;
    while (fgets(line, sizeof line, f))
        if (sscanf(line, "VmRSS: %ld", &kb) == 1)
            break;
    fclose(f);
    return kb;
}

int main(void)
{
    char *p = mmap(NULL, LEN, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    if (p == MAP_FAILED) {
        printf("mmap: %s\n", strerror(errno));
        return 0;
    }
    for (int i = 0; i < 4; i++)
        p[at[i]] = 'a' + i;
    printf("fork: %s\n", fork_and_check(p));
    mprotect(p, LEN, PROT_NONE);
    mprotect(p, LEN, PROT_READ | PROT_WRITE);
    printf("after PROT_NONE: %s\n", check(p) ? "kept" : "lost");
    char *dst = mmap(NULL, LEN, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    char *q = mremap(p, LEN, LEN, MREMAP_MAYMOVE | MREMAP_FIXED, dst);
    printf("after mremap: %s, fork: %s\n", q == dst && check(q) ? "kept" : "lost",
           fork_and_check(q));
    printf("rss under 64 MB: %s\n", rss_kb() < (64 << 10) ? "yes" : "no");
    return 0;
}
C
gcc -O2 -o sparse sparse.c
./sparse
python3 -c '
import os
data = bytearray(b"p" * (1 << 22))
r, w = os.pipe()
pid = os.fork()
if pid == 0:
    os.read(r, 1)
    ok = data[::4096] == b"p" * 1024
    data[0:10] = b"c" * 10
    os._exit(0 if ok else 1)
data[:] = b"q" * (1 << 22)
os.write(w, b"g")
_, st = os.waitpid(pid, 0)
print("python fork:", os.WEXITSTATUS(st), bytes(data[:4]))
'
# Fork-related madvise: MADV_WIPEONFORK gives a child zeros instead of a copy (BoringSSL reseeds its
# random number generator this way) and the child keeps the flag, MADV_KEEPONFORK undoes it;
# MADV_DONTFORK leaves the mapping out of the child, MADV_DOFORK undoes it; wiping applies only to
# private anonymous memory; a neighbouring file mapping or locked mapping below the range does not
# make the advice EINVAL; growing a mapping with mremap keeps its flags; smaps shows wf, dc and dd
cat > advice.c <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE 4096

/* Prints what a forked child reads at p[0], or the signal that ended it */
static void reads(const char *label, volatile char *p)
{
    pid_t pid = fork();
    if (pid == 0) {
        printf(" %s=%d", label, p[0]);
        _exit(0);
    }
    int st;
    waitpid(pid, &st, 0);
    if (WIFSIGNALED(st))
        printf(" %s=%s", label, WTERMSIG(st) == SIGSEGV ? "SIGSEGV" : "signal");
}

/* Whether the VmFlags of the mapping starting at p list `flag` */
static int smaps_has(void *p, const char *flag)
{
    FILE *f = fopen("/proc/self/smaps", "r");
    char line[512], start[32], want[16];
    snprintf(start, sizeof start, "%lx-", (unsigned long)p);
    snprintf(want, sizeof want, " %s ", flag);
    int in = 0, found = 0;
    while (fgets(line, sizeof line, f)) {
        if (strncmp(line, start, strlen(start)) == 0) {
            in = 1;
        } else if (in && strncmp(line, "VmFlags:", 8) == 0) {
            found = strstr(line, want) != NULL;
            break;
        }
    }
    fclose(f);
    return found;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    setrlimit(RLIMIT_CORE, &(struct rlimit){0, 0});
    char *p = mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    p[0] = 'A';
    p[PAGE] = 'B';
    printf("wipeonfork %d dontdump %d:", madvise(p, PAGE, MADV_WIPEONFORK),
           madvise(p, PAGE, MADV_DONTDUMP));
    reads("wiped", p);
    reads("other page", p + PAGE);
    printf(" wf %d %d\n", smaps_has(p, "wf"), smaps_has(p + PAGE, "wf"));
    pid_t pid = fork();
    if (pid == 0) {
        p[0] = 'C';
        printf("child: wf %d dd %d", smaps_has(p, "wf"), smaps_has(p, "dd"));
        reads("grandchild", p);
        printf("\n");
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    printf("keeponfork %d:", madvise(p, PAGE, MADV_KEEPONFORK));
    reads("kept", p);
    printf(" wf %d\n", smaps_has(p, "wf"));

    printf("dontfork %d:", madvise(p + PAGE, PAGE, MADV_DONTFORK));
    reads("left out", p + PAGE);
    reads("other page", p);
    printf(" dc %d\n", smaps_has(p + PAGE, "dc"));
    printf("dofork %d:", madvise(p + PAGE, PAGE, MADV_DOFORK));
    reads("back", p + PAGE);
    printf(" dc %d\n", smaps_has(p + PAGE, "dc"));

    /* A file mapping, then anonymous memory right above it; a locked page, then anonymous memory
     * right above it */
    int fd = open("advice-file", O_RDWR | O_CREAT | O_TRUNC, 0644);
    ftruncate(fd, PAGE);
    char *area = mmap(NULL, 4 * PAGE, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *file = mmap(area, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_FIXED, fd, 0);
    char *anon = mmap(area + PAGE, PAGE, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    char *locked = mmap(area + 2 * PAGE, PAGE, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    char *above = mmap(area + 3 * PAGE, PAGE, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    mlock(locked, PAGE);
    printf("above a file mapping: free %d wipeonfork %d keeponfork %d\n",
           madvise(anon, PAGE, MADV_FREE), madvise(anon, PAGE, MADV_WIPEONFORK),
           madvise(anon, PAGE, MADV_KEEPONFORK));
    printf("above a locked page: dontneed %d\n", madvise(above, PAGE, MADV_DONTNEED));
    printf("on a file mapping: wipeonfork %d", madvise(file, PAGE, MADV_WIPEONFORK));
    printf(" keeponfork %d\n", madvise(file, PAGE, MADV_KEEPONFORK));

    /* Growing keeps the flags: the new part is wiped too */
    char *q = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    q[0] = 'D';
    madvise(q, PAGE, MADV_WIPEONFORK);
    madvise(q, PAGE, MADV_DONTDUMP);
    q = mremap(q, PAGE, 2 * PAGE, MREMAP_MAYMOVE);
    q[PAGE] = 'E';
    printf("grown:");
    reads("start", q);
    reads("new part", q + PAGE);
    printf(" wf %d dd %d\n", smaps_has(q, "wf"), smaps_has(q, "dd"));
    return 0;
}
C
gcc -O2 -o advice advice.c
./advice
