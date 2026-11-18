# Several processes of the same program share Tier 1 translations
# (docs/rounds/108-shared-translation.md). Sharing assumes they map the same code at the same
# addresses; in the cases below two processes put different code at the same address, and each
# must execute its own:
#   files    parent and child map two different files at the same address
#   jit      parent and child each write their own code at the same address (writable and
#            executable anonymous mappings)
#   rewrite  parent and child first map the same file and execute it together, after which the
#            child rewrites its own copy
#   exec     the child execs the same program and maps another file at the same address
#   dlopen   several processes dlopen two libraries in different orders, so the same address
#            holds different libraries
#   alone    the child maps a file at an address, executes it and exits, leaving only the parent
#            in the group, which maps another file at the same address (each case in its own
#            process tree: while the main process is there, the group has more than just it)
# A file is trusted (so translations can be shared) only once its ctime is more than two seconds
# in the past, so wait a while after compiling and writing the files
set -e
cd /tmp
mkdir -p shared && cd shared
cat > shared.c <<'C'
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

typedef int (*fn)(void);

/* movz w0, #n; ret */
static void code(uint32_t *p, int n)
{
    p[0] = 0x52800000u | (uint32_t)n << 5;
    p[1] = 0xd65f03c0u;
}

static void *at(int i)
{
    return (void *)(uintptr_t)(0x500000000000ull + (uint64_t)i * 0x100000);
}

static fn map_file(const char *path, int i)
{
    int fd = open(path, O_RDONLY);
    void *p = mmap(at(i), 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_FIXED, fd, 0);
    close(fd);
    return p == MAP_FAILED ? NULL : (fn)p;
}

static fn map_jit(int i, int n)
{
    void *p = mmap(at(i), 4096, PROT_READ | PROT_WRITE | PROT_EXEC,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (p == MAP_FAILED)
        return NULL;
    code(p, n);
    __builtin___clear_cache(p, (char *)p + 8);
    return (fn)p;
}

/* Call several times and return the result (when it is the same every time), -1 when it differs */
static int run(fn f)
{
    int first = f();
    for (int i = 0; i < 200000; i++)
        if (f() != first)
            return -1;
    return first;
}

/* Parent and child each get ready and wait until the other is ready too, then execute together */
static int pa[2], pb[2];
static void meet(int child)
{
    char c = 0;
    if (child) {
        write(pa[1], &c, 1);
        read(pb[0], &c, 1);
    } else {
        read(pa[0], &c, 1);
        write(pb[1], &c, 1);
    }
}

static int both(const char *what, fn (*prepare)(int child))
{
    pipe(pa);
    pipe(pb);
    pid_t pid = fork();
    if (pid == 0) {
        fn f = prepare(1);
        meet(1);
        _exit(f ? run(f) : 100);
    }
    fn f = prepare(0);
    meet(0);
    int mine = f ? run(f) : 100, status;
    waitpid(pid, &status, 0);
    printf("%s: child %d parent %d\n", what, WEXITSTATUS(status), mine);
    return 0;
}

static fn files(int child)
{
    return map_file(child ? "f1" : "f2", 0);
}

static fn jit(int child)
{
    return map_jit(1, child ? 3 : 4);
}

/* Each case runs in a process forked from the main process: the main process maps no other code
 * itself and can share translations all along, and the forked processes all join its group */
static void in_child(void (*body)(void))
{
    pid_t pid = fork();
    if (pid == 0) {
        body();
        _exit(0);
    }
    waitpid(pid, NULL, 0);
}

static void case_files(void)
{
    both("files", files);
}

static void case_jit(void)
{
    both("jit", jit);
}

static char *self;

/* rewrite: first execute the same file together, then the child rewrites its own copy and
 * executes it */
static void case_rewrite(void)
{
    fn f = map_file("f1", 2);
    printf("rewrite: before %d\n", run(f));
    pipe(pa);
    pipe(pb);
    pid_t pid = fork();
    if (pid == 0) {
        int before = run(f);
        mprotect((void *)f, 4096, PROT_READ | PROT_WRITE | PROT_EXEC);
        code((uint32_t *)(void *)f, 9);
        __builtin___clear_cache((char *)f, (char *)f + 8);
        meet(1);
        _exit(before * 10 + run(f));
    }
    meet(0);
    int status, mine = run(f);
    waitpid(pid, &status, 0);
    printf("rewrite: child %d parent %d\n", WEXITSTATUS(status), mine);
}

/* exec: the new address space of the same program maps f2 at the address where the parent maps
 * f1 */
static void case_exec(void)
{
    fn f = map_file("f1", 3);
    pid_t pid = fork();
    if (pid == 0) {
        execl(self, self, "f2", "3", (char *)NULL);
        _exit(101);
    }
    int status, mine = run(f);
    waitpid(pid, &status, 0);
    printf("exec: child %d parent %d\n", WEXITSTATUS(status), mine);
}

/* dlopen: two processes load two libraries in different orders, so the same address holds
 * different libraries */
static void case_dlopen(void)
{
    int results[2], status;
    for (int order = 0; order < 2; order++) {
        pid_t pid = fork();
        if (pid == 0) {
            void *a = dlopen(order ? "./libb.so" : "./liba.so", RTLD_NOW);
            void *b = dlopen(order ? "./liba.so" : "./libb.so", RTLD_NOW);
            fn va = a ? (fn)dlsym(a, "v") : NULL, vb = b ? (fn)dlsym(b, "v") : NULL;
            _exit(va && vb ? run(va) * 10 + run(vb) : 100);
        }
        waitpid(pid, &status, 0);
        results[order] = WEXITSTATUS(status);
    }
    printf("dlopen: %d %d\n", results[0], results[1]);
}

/* alone: after the child maps f1, executes it and exits, only the parent that opened the group is
 * left in it; it maps f2 in the same slot and, on leaving the group, keeps the group's
 * translations: that slot still has the blocks of f1 that the child translated */
static void case_alone(void)
{
    pid_t pid = fork();
    if (pid == 0) {
        fn f = map_file("f1", 4);
        _exit(f ? run(f) : 100);
    }
    int status;
    waitpid(pid, &status, 0);
    fn f = map_file("f2", 4);
    printf("alone: child %d parent %d\n", WEXITSTATUS(status), f ? run(f) : 100);
}

int main(int argc, char **argv)
{
    if (argc == 2 && !strcmp(argv[1], "alone")) {
        setvbuf(stdout, NULL, _IONBF, 0);
        case_alone();
        return 0;
    }
    if (argc == 3) { /* the exec'd one: map argv[1] in the slot of argv[2] */
        fn f = map_file(argv[1], atoi(argv[2]));
        return f ? run(f) : 100;
    }
    self = argv[0];
    setvbuf(stdout, NULL, _IONBF, 0);
    for (int round = 0; round < 3; round++) {
        in_child(case_files);
        in_child(case_jit);
        in_child(case_rewrite);
        in_child(case_exec);
        in_child(case_dlopen);
    }
    return 0;
}
C
gcc -O2 -o shared shared.c -ldl
printf 'int v(void) { return 1; }\n' > a.c
printf 'int v(void) { return 2; }\n' > b.c
gcc -O2 -shared -fPIC -o liba.so a.c
gcc -O2 -shared -fPIC -o libb.so b.c
# Two files with only one page of code: movz w0, #1; ret and movz w0, #2; ret
printf '\040\000\200\122\300\003\137\326' > f1
printf '\100\000\200\122\300\003\137\326' > f2
truncate -s 4096 f1 f2
sleep 3
for i in 1 2 3; do ./shared alone; done
./shared
