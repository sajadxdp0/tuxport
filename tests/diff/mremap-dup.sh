# mremap with an old length of 0 (Linux's "duplicate shm area"): a shared mapping (anonymous, a
# file, a System V segment) is mapped again from the old address's offset at a new address while
# the original stays, both seeing the same memory; past the end of a shared anonymous object the
# copy raises SIGBUS; MREMAP_FIXED places the copy; private mappings are EINVAL, no MREMAP_MAYMOVE
# is ENOMEM and an unmapped address EFAULT; a segment's copy counts as one more attachment.
cd /tmp
cat > /tmp/mdup.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/shm.h>
#include <unistd.h>

static sigjmp_buf env;

static void on_signal(int sig)
{
    siglongjmp(env, sig);
}

/* Reads one byte, or reports the signal the access raised */
static void touch(const char *what, volatile char *p)
{
    int sig = sigsetjmp(env, 1);
    if (sig == 0) {
        char c = *p;
        printf("%s: '%c'\n", what, c >= ' ' && c < 127 ? c : '?');
    }
    else
        printf("%s: %s\n", what, strsignal(sig));
}

/* Writes one byte; a mapping that is not there reports the signal instead of looping */
static void poke(volatile char *p, char c)
{
    if (sigsetjmp(env, 1) == 0)
        *p = c;
    else
        printf("  (write faulted)\n");
}

static void result(const char *what, void *p)
{
    if (p == MAP_FAILED)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: mapped\n", what);
}

int main(void)
{
    signal(SIGBUS, on_signal);
    signal(SIGSEGV, on_signal);
    long pg = sysconf(_SC_PAGESIZE);

    char *p = mmap(NULL, 2 * pg, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    p[0] = 'A';
    p[pg] = 'B';
    char *q = mremap(p, 0, pg, MREMAP_MAYMOVE);
    result("anonymous copy", q);
    touch("  copy[0]", q);
    poke(q, 'Z');
    touch("  original[0] after writing the copy", p);
    char *r = mremap(p + pg, 0, 2 * pg, MREMAP_MAYMOVE);
    result("copy of the second page, two pages long", r);
    touch("  copy[0]", r);
    touch("  copy[page] past the object", r + pg);
    touch("  original still mapped", p + pg);

    /* MREMAP_FIXED, and a target overlapping the old address */
    char *target = mmap(NULL, pg, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *f = mremap(p, 0, pg, MREMAP_MAYMOVE | MREMAP_FIXED, target);
    printf("fixed copy: %s\n", f == target ? "at the target" : strerror(errno));
    touch("  target[0]", target);
    result("fixed over the old address", mremap(p + pg, 0, 2 * pg, MREMAP_MAYMOVE | MREMAP_FIXED, p));

    /* Errors */
    result("without MREMAP_MAYMOVE", mremap(p, 0, pg, 0));
    char *priv = mmap(NULL, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    result("private", mremap(priv, 0, pg, MREMAP_MAYMOVE));
    munmap(priv, pg);
    result("unmapped address", mremap(priv, 0, pg, MREMAP_MAYMOVE));
    result("zero new length", mremap(p, 0, 0, MREMAP_MAYMOVE));
    result("dontunmap", mremap(p, 0, 0, MREMAP_MAYMOVE | MREMAP_DONTUNMAP));

    /* A shared file mapping of one page: the copy maps two pages of the file */
    int fd = open("/tmp/mdup.dat", O_RDWR | O_CREAT | O_TRUNC, 0600);
    for (int i = 0; i < 3; i++) {
        char buf[4096];
        memset(buf, 'a' + i, sizeof buf);
        write(fd, buf, pg);
    }
    char *m = mmap(NULL, pg, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    char *d = mremap(m, 0, 2 * pg, MREMAP_MAYMOVE);
    result("file copy", d);
    touch("  copy[0]", d);
    touch("  copy[page]", d + pg);
    poke(d + 1, 'X');
    touch("  original[1] after writing the copy", m + 1);
    char c;
    pread(fd, &c, 1, 1);
    printf("  file[1]: '%c'\n", c);
    close(fd);
    unlink("/tmp/mdup.dat");

    /* A System V segment: the copy is one more attachment */
    int id = shmget(IPC_PRIVATE, 2 * pg, IPC_CREAT | 0600);
    char *s = shmat(id, NULL, 0);
    s[0] = 'S';
    struct shmid_ds ds;
    shmctl(id, IPC_STAT, &ds);
    printf("segment attachments: %lu\n", (unsigned long)ds.shm_nattch);
    char *sc = mremap(s, 0, 2 * pg, MREMAP_MAYMOVE);
    result("segment copy", sc);
    touch("  copy[0]", sc);
    shmctl(id, IPC_STAT, &ds);
    printf("  attachments: %lu\n", (unsigned long)ds.shm_nattch);
    printf("  shmdt copy: %s\n", shmdt(sc) ? strerror(errno) : "ok");
    shmctl(id, IPC_STAT, &ds);
    printf("  attachments: %lu\n", (unsigned long)ds.shm_nattch);
    shmdt(s);
    shmctl(id, IPC_RMID, NULL);
    return 0;
}
C
gcc -O1 -o /tmp/mdup /tmp/mdup.c && /tmp/mdup
rm -f /tmp/mdup /tmp/mdup.c
