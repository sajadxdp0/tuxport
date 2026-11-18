# docker-run-options: --cap-add SYS_ADMIN --cap-add SYS_TIME --security-opt apparmor=unconfined
# The vDSO (the kernel/vdso map): its clock functions agree with the syscalls clock by clock (a
# clock read alternately both ways never goes back, resolutions and gettimeofday's timezone are
# the same); its four symbols; [vvar] right below [vdso], readable, and neither writable,
# executable nor growable; tracers and seccomp see only the clocks Linux's vDSO leaves to the
# kernel; in a time namespace with offsets (the native side needs CAP_SYS_ADMIN to create it and
# CAP_SYS_TIME to write timens_offsets) a program exec'd from a fork or a vfork reads its clocks
# with the offsets
set -e
cd /tmp
cat > vdso.c <<'C'
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef CLONE_NEWTIME
#define CLONE_NEWTIME 0x80
#endif

/* No linux-headers: define them ourselves */
struct sock_filter {
    uint16_t code;
    uint8_t jt, jf;
    uint32_t k;
};
struct sock_fprog {
    unsigned short len;
    struct sock_filter *filter;
};
#define BPF_STMT(code, k) {(unsigned short)(code), 0, 0, k}
#define BPF_JUMP(code, k, jt, jf) {(unsigned short)(code), jt, jf, k}
#define BPF_LD 0x00
#define BPF_W 0x00
#define BPF_ABS 0x20
#define BPF_JMP 0x05
#define BPF_JEQ 0x10
#define BPF_K 0x00
#define BPF_RET 0x06
#define SECCOMP_MODE_FILTER 2
#define SECCOMP_RET_ALLOW 0x7fff0000U
#define SECCOMP_RET_ERRNO 0x00050000U

/* Finds a vDSO symbol as musl's __vdsosym does (without the version check) */
static void *vdso_sym(const char *name)
{
    unsigned long base = getauxval(AT_SYSINFO_EHDR), load = 0;
    Elf64_Ehdr *eh = (void *)base;
    Elf64_Dyn *dyn = NULL;
    for (int i = 0; i < eh->e_phnum; i++) {
        Elf64_Phdr *ph = (void *)(base + eh->e_phoff + i * eh->e_phentsize);
        if (ph->p_type == PT_LOAD)
            load = base + ph->p_offset - ph->p_vaddr;
        else if (ph->p_type == PT_DYNAMIC)
            dyn = (void *)(base + ph->p_offset);
    }
    char *strings = NULL;
    Elf64_Sym *syms = NULL;
    Elf32_Word *hash = NULL;
    for (; dyn->d_tag; dyn++) {
        if (dyn->d_tag == DT_STRTAB)
            strings = (char *)(load + dyn->d_un.d_ptr);
        else if (dyn->d_tag == DT_SYMTAB)
            syms = (void *)(load + dyn->d_un.d_ptr);
        else if (dyn->d_tag == DT_HASH)
            hash = (void *)(load + dyn->d_un.d_ptr);
    }
    for (Elf32_Word i = 0; i < hash[1]; i++)
        if (syms[i].st_shndx && !strcmp(strings + syms[i].st_name, name))
            return (void *)(load + syms[i].st_value);
    return NULL;
}

static int (*vdso_gettime)(clockid_t, struct timespec *);
static int (*vdso_getres)(clockid_t, struct timespec *);
static int (*vdso_gettimeofday)(struct timeval *, struct timezone *);

static long long ns(struct timespec t) { return t.tv_sec * 1000000000LL + t.tv_nsec; }

static double secs(clockid_t c)
{
    struct timespec t;
    vdso_gettime(c, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static int near(double a, double b) { return a - b < 1.0 && b - a < 1.0; }

/* The mappings named [vvar] and [vdso] */
static void find(const char *name, unsigned long *start, unsigned long *end, char perms[5])
{
    char line[256];
    FILE *f = fopen("/proc/self/maps", "r");
    *start = *end = 0;
    while (fgets(line, sizeof line, f))
        if (strstr(line, name))
            sscanf(line, "%lx-%lx %4s", start, end, perms);
    fclose(f);
}

/* A program exec'd in the new time namespace: its clocks include the offsets */
static int show(const char *how, double m0, double b0, double r0)
{
    printf("%s: monotonic +86400 %d, coarse %d, boottime +3600 %d, realtime unchanged %d\n", how,
           near(secs(CLOCK_MONOTONIC) - m0, 86400), near(secs(CLOCK_MONOTONIC_COARSE) - m0, 86400),
           near(secs(CLOCK_BOOTTIME) - b0, 3600), near(secs(CLOCK_REALTIME), r0));
    return 0;
}

static void calls(void)
{
    struct timespec t;
    struct timeval tv;
    clock_gettime(CLOCK_MONOTONIC, &t);
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &t);
    clock_getres(CLOCK_REALTIME, &t);
    vdso_gettimeofday(&tv, NULL);
}

int main(int argc, char **argv)
{
    vdso_gettime = vdso_sym("__kernel_clock_gettime");
    vdso_getres = vdso_sym("__kernel_clock_getres");
    vdso_gettimeofday = vdso_sym("__kernel_gettimeofday");
    if (argc == 5)
        return show(argv[1], atof(argv[2]), atof(argv[3]), atof(argv[4]));
    if (argc == 2) { /* traced by strace */
        calls();
        return 0;
    }
    setvbuf(stdout, NULL, _IONBF, 0);
    const char *names[] = {"__kernel_rt_sigreturn", "__kernel_clock_gettime",
                           "__kernel_gettimeofday", "__kernel_clock_getres"};
    for (int i = 0; i < 4; i++)
        printf("%s: %s\n", names[i], vdso_sym(names[i]) ? "yes" : "no");

    // Read alternately through the vDSO and the syscall, a clock never goes back; resolutions
    // are the same both ways
    clockid_t clocks[] = {CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW,
                          CLOCK_REALTIME_COARSE, CLOCK_MONOTONIC_COARSE, CLOCK_BOOTTIME, CLOCK_TAI};
    for (int c = 0; c < 7; c++) {
        long long last = 0;
        int back = 0;
        for (int i = 0; i < 20000; i++) {
            struct timespec t;
            if (i & 1)
                syscall(SYS_clock_gettime, clocks[c], &t);
            else
                vdso_gettime(clocks[c], &t);
            back += ns(t) < last;
            last = ns(t);
        }
        struct timespec a, b;
        vdso_getres(clocks[c], &a);
        syscall(SYS_clock_getres, clocks[c], &b);
        printf("clock %d: back %d, same resolution %d\n", clocks[c], back, ns(a) == ns(b));
    }
    struct timeval tv1, tv2;
    struct timezone tz1 = {-1, -1}, tz2 = {-2, -2};
    vdso_gettimeofday(&tv1, &tz1);
    syscall(SYS_gettimeofday, &tv2, &tz2);
    printf("gettimeofday: in order %d, same timezone %d\n",
           tv1.tv_sec < tv2.tv_sec || (tv1.tv_sec == tv2.tv_sec && tv1.tv_usec <= tv2.tv_usec),
           tz1.tz_minuteswest == tz2.tz_minuteswest && tz1.tz_dsttime == tz2.tz_dsttime);
    struct timespec bad = {0};
    printf("bad clock: %d %d\n", vdso_gettime(16, &bad), clock_getres(16, &bad) ? errno : 0);

    // [vvar] lies right below [vdso], with the host kernel's size
    unsigned long vs, ve, ds, de;
    char vp[5] = "", dp[5] = "";
    find("[vvar]", &vs, &ve, vp);
    find("[vdso]", &ds, &de, dp);
    printf("[vvar] %s right below [vdso] %d, [vdso] %s at AT_SYSINFO_EHDR %d\n", vp, ve == ds, dp,
           ds == getauxval(AT_SYSINFO_EHDR));
    printf("[vvar] size %lu, [vdso] size %lu\n", ve - vs, de - ds);
    volatile char first = *(char *)vs;
    (void)first;
    printf("mprotect write: %s\n", mprotect((void *)vs, ve - vs, PROT_READ | PROT_WRITE) ? strerror(errno) : "ok");
    printf("mprotect exec: %s\n", mprotect((void *)vs, ve - vs, PROT_READ | PROT_EXEC) ? strerror(errno) : "ok");
    void *r = mremap((void *)vs, ve - vs, 2 * (ve - vs), 0);
    printf("grow: %s\n", r == MAP_FAILED ? strerror(errno) : "ok");
    r = mremap((void *)vs, ve - vs, 2 * (ve - vs), MREMAP_MAYMOVE);
    printf("grow by moving: %s\n", r == MAP_FAILED ? strerror(errno) : "ok");
    r = mremap((void *)vs, 0, ve - vs, MREMAP_MAYMOVE);
    printf("duplicate: %s\n", r == MAP_FAILED ? strerror(errno) : "ok");
    printf("still reads: %d\n", near(secs(CLOCK_REALTIME), time(NULL)));

    // seccomp refuses the clock syscalls: what Linux's vDSO reads in user space still works, the
    // CPU-time clock it leaves to the kernel does not
    pid_t c = fork();
    if (c == 0) {
        struct sock_filter f[] = {
            BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 0),
            BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_clock_gettime, 2, 0),
            BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_gettimeofday, 1, 0),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        };
        struct sock_fprog prog = {sizeof f / sizeof f[0], f};
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog);
        struct timespec t;
        struct timeval tv;
        printf("seccomp: monotonic %d, cputime %d, gettimeofday %d, syscall %d\n",
               vdso_gettime(CLOCK_MONOTONIC, &t), vdso_gettime(CLOCK_PROCESS_CPUTIME_ID, &t),
               vdso_gettimeofday(&tv, NULL), (int)syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &t));
        _exit(0);
    }
    waitpid(c, NULL, 0);

    // A time namespace with offsets: programs exec'd from a fork and from a vfork (which enters it
    // only at execve) read their clocks with the offsets
    printf("unshare: %s\n", unshare(CLONE_NEWTIME) ? strerror(errno) : "ok");
    int fd = open("/proc/self/timens_offsets", O_WRONLY);
    const char *offsets = "monotonic 86400 0\nboottime 3600 0\n";
    printf("offsets: %s\n", write(fd, offsets, strlen(offsets)) < 0 ? strerror(errno) : "ok");
    close(fd);
    char m0[32], b0[32], r0[32];
    snprintf(m0, sizeof m0, "%f", secs(CLOCK_MONOTONIC));
    snprintf(b0, sizeof b0, "%f", secs(CLOCK_BOOTTIME));
    snprintf(r0, sizeof r0, "%f", secs(CLOCK_REALTIME));
    c = fork();
    if (c == 0) {
        execl("/proc/self/exe", "vdso", "fork", m0, b0, r0, (char *)NULL);
        _exit(1);
    }
    waitpid(c, NULL, 0);
    c = vfork();
    if (c == 0) {
        execl("/proc/self/exe", "vdso", "vfork", m0, b0, r0, (char *)NULL);
        _exit(1);
    }
    waitpid(c, NULL, 0);
    printf("parent unchanged: %d\n", near(secs(CLOCK_MONOTONIC), atof(m0)));
    return 0;
}
C
gcc -D_GNU_SOURCE -O1 -o vdso vdso.c && ./vdso
# strace sees the CPU-time clock the vDSO leaves to the kernel, not the clocks it reads itself
strace -qq -e trace=clock_gettime,clock_getres,gettimeofday -o vdso.trace ./vdso trace
sed 's/,.*//' vdso.trace
