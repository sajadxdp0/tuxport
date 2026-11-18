# docker-run-options: --privileged
# cgroup device filtering (eBPF's BPF_PROG_TYPE_CGROUP_DEVICE, docs/rounds/166-bpf-devices.md):
# it is what runc and crun attach to a container's cgroup. Loading, attaching, querying, getting by
# ID, getting info, detaching; opening device nodes and mknod are allowed or refused (EPERM) by
# the programs in the cgroup, and child cgroups inherit them; programs the verifier rejects. The
# native side is a privileged container (docker attaches its own program to the container's
# root), so only what is under t is compared, and for the effective program count only how many
# more there are than for t
cd /sys/fs/cgroup
mkdir t t/d t/d/e
cat > /tmp/bpfdev.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <unistd.h>

enum { MAP_CREATE = 0, PROG_LOAD = 5, PROG_ATTACH = 8, PROG_DETACH = 9, GET_FD_BY_ID = 13,
       GET_INFO = 15, PROG_QUERY = 16 };
enum { TYPE_CGROUP_DEVICE = 15, ATTACH_CGROUP_DEVICE = 6, ALLOW_OVERRIDE = 1, ALLOW_MULTI = 2,
       REPLACE = 4, QUERY_EFFECTIVE = 1 };

struct insn { uint8_t code, regs; int16_t off; int32_t imm; };
#define I(c, d, s, o, i) ((struct insn){c, (d) | (s) << 4, o, i})

static long bpf(int cmd, void *attr, unsigned size) { return syscall(SYS_bpf, cmd, attr, size); }
static const char *err(long r) { return r < 0 ? strerror(errno) : "ok"; }

static char logbuf[4096];

static int load(const struct insn *p, int n, const char *name)
{
    unsigned char a[128] = {0};
    *(uint32_t *)(a + 0) = TYPE_CGROUP_DEVICE;
    *(uint32_t *)(a + 4) = n;
    *(uint64_t *)(a + 8) = (uintptr_t)p;
    *(uint64_t *)(a + 16) = (uintptr_t)"GPL";
    *(uint32_t *)(a + 24) = 1;
    *(uint32_t *)(a + 28) = sizeof logbuf;
    *(uint64_t *)(a + 32) = (uintptr_t)logbuf;
    if (name)
        strncpy((char *)a + 48, name, 15);
    return bpf(PROG_LOAD, a, sizeof a);
}

static long attach(int cmd, int cg, int prog, unsigned flags, int replace)
{
    unsigned char a[32] = {0};
    *(uint32_t *)(a + 0) = cg;
    *(uint32_t *)(a + 4) = prog;
    *(uint32_t *)(a + 8) = ATTACH_CGROUP_DEVICE;
    *(uint32_t *)(a + 12) = flags;
    *(uint32_t *)(a + 16) = replace;
    return bpf(cmd, a, sizeof a);
}

static uint32_t ids[64];

static long query(int cg, unsigned qflags, unsigned cnt, unsigned *flags, unsigned *total)
{
    unsigned char a[40] = {0};
    *(uint32_t *)(a + 0) = cg;
    *(uint32_t *)(a + 4) = ATTACH_CGROUP_DEVICE;
    *(uint32_t *)(a + 8) = qflags;
    *(uint64_t *)(a + 16) = (uintptr_t)ids;
    *(uint32_t *)(a + 24) = cnt;
    long r = bpf(PROG_QUERY, a, sizeof a);
    *flags = *(uint32_t *)(a + 12);
    *total = *(uint32_t *)(a + 24);
    return r;
}

static int cgfd(const char *name)
{
    char path[64];
    snprintf(path, sizeof path, "/sys/fs/cgroup/t/%s", name);
    return open(path, O_RDONLY | O_DIRECTORY);
}

static void into(const char *name)
{
    char path[64], b[16];
    snprintf(path, sizeof path, "/sys/fs/cgroup/t/%s/cgroup.procs", name);
    int fd = open(path, O_WRONLY);
    snprintf(b, sizeof b, "%d", getpid());
    write(fd, b, strlen(b));
    close(fd);
}

// Open devices and mknod them in cgroup t/<name>
static void devices(const char *name)
{
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        into(name);
        const char *files[] = {"/dev/null", "/dev/zero", "/dev/full", "/dev/ptmx", "/dev/urandom"};
        for (int i = 0; i < 5; i++) {
            int r = open(files[i], O_RDONLY);
            int w = open(files[i], O_RDWR);
            printf("  %s: read %s, write %s\n", files[i], err(r), err(w));
            close(r);
            close(w);
        }
        struct { mode_t mode; int major, minor; } nodes[] = {
            {S_IFCHR, 1, 3}, {S_IFCHR, 1, 5}, {S_IFCHR, 1, 7}, {S_IFBLK, 1, 3}};
        for (int i = 0; i < 4; i++) {
            unlink("/tmp/node");
            long r = mknod("/tmp/node", nodes[i].mode | 0600, makedev(nodes[i].major, nodes[i].minor));
            printf("  mknod %c %d:%d: %s\n", nodes[i].mode == S_IFCHR ? 'c' : 'b', nodes[i].major,
                   nodes[i].minor, err(r));
        }
        unlink("/tmp/node");
        fflush(stdout);
        _exit(0);
    }
    waitpid(c, NULL, 0);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    // Character device 1:3 may do anything, 1:5 may not be written, nothing else is allowed
    struct insn prog[] = {
        I(0x61, 2, 1, 0, 0),    // r2 = *(u32 *)(r1 + 0)   access_type
        I(0xbc, 3, 2, 0, 0),    // w3 = w2
        I(0x54, 3, 0, 0, 0xffff), // w3 &= 0xffff          device type
        I(0x74, 2, 0, 0, 16),   // w2 >>= 16               access
        I(0x61, 4, 1, 4, 0),    // r4 = major
        I(0x61, 5, 1, 8, 0),    // r5 = minor
        I(0x55, 3, 0, 6, 2),    // if r3 != 2 goto deny
        I(0x55, 4, 0, 5, 1),    // if r4 != 1 goto deny
        I(0x15, 5, 0, 2, 3),    // if r5 == 3 goto allow
        I(0x55, 5, 0, 3, 5),    // if r5 != 5 goto deny
        I(0x45, 2, 0, 2, 4),    // if r2 & 4 goto deny
        I(0xb7, 0, 0, 0, 1),    // allow: r0 = 1
        I(0x95, 0, 0, 0, 0),    // exit
        I(0xb7, 0, 0, 0, 0),    // deny: r0 = 0
        I(0x95, 0, 0, 0, 0),    // exit
    };
    int n = sizeof prog / sizeof prog[0];
    int p1 = load(prog, n, "allow_null");
    int p2 = load(prog, n, "second");
    printf("load: %s %s\n", err(p1), err(p2));
    int d = cgfd("d"), e = cgfd("d/e"), t = cgfd("");
    unsigned flags, total, base_flags, base;
    printf("query d: %s", err(query(d, 0, 64, &flags, &total)));
    printf(" flags %u cnt %u\n", flags, total);
    query(t, QUERY_EFFECTIVE, 64, &base_flags, &base);
    printf("devices before attaching:\n");
    devices("d");

    // Attach: ALLOW_MULTI
    printf("attach multi: %s\n", err(attach(PROG_ATTACH, d, p1, ALLOW_MULTI, 0)));
    printf("attach the same again: %s\n", err(attach(PROG_ATTACH, d, p1, ALLOW_MULTI, 0)));
    printf("attach without multi on top: %s\n", err(attach(PROG_ATTACH, d, p2, 0, 0)));
    printf("override and multi: %s\n", err(attach(PROG_ATTACH, d, p2, ALLOW_MULTI | ALLOW_OVERRIDE, 0)));
    printf("replace without multi: %s\n", err(attach(PROG_ATTACH, d, p2, REPLACE, p1)));
    printf("replace a missing one: %s\n", err(attach(PROG_ATTACH, d, p1, ALLOW_MULTI | REPLACE, p2)));
    printf("not a cgroup: %s\n", err(attach(PROG_ATTACH, 0, p1, ALLOW_MULTI, 0)));
    printf("not a program: %s\n", err(attach(PROG_ATTACH, d, d, ALLOW_MULTI, 0)));
    long r = query(d, 0, 64, &flags, &total);
    printf("query d: %s flags %u cnt %u\n", err(r), flags, total);
    uint32_t id1 = ids[0];
    r = query(d, 0, 0, &flags, &total);
    printf("query d with no room: %s cnt %u\n", err(r), total);
    printf("query e: %s", err(query(e, 0, 64, &flags, &total)));
    printf(" cnt %u\n", total);
    r = query(e, QUERY_EFFECTIVE, 64, &flags, &total);
    printf("query e effective: %s flags %u cnt over t %d\n", err(r), flags, (int)(total - base));
    printf("query bad flags: %s\n", err(query(d, 2, 64, &flags, &total)));
    printf("devices in d:\n");
    devices("d");
    printf("devices in d/e:\n");
    devices("d/e");

    // Get by ID, get info
    unsigned char a[16] = {0};
    *(uint32_t *)a = id1;
    int byid = bpf(GET_FD_BY_ID, a, sizeof a);
    printf("get fd by id: %s\n", err(byid));
    *(uint32_t *)a = 0x7ffffff0;
    printf("get fd by a missing id: %s\n", err(bpf(GET_FD_BY_ID, a, sizeof a)));
    unsigned char info[256] = {0};
    struct insn xl[32];
    *(uint32_t *)(info + 20) = sizeof xl;
    *(uint64_t *)(info + 32) = (uintptr_t)xl;
    unsigned char ia[16] = {0};
    *(uint32_t *)(ia + 0) = byid;
    *(uint32_t *)(ia + 4) = sizeof info;
    *(uint64_t *)(ia + 8) = (uintptr_t)info;
    r = bpf(GET_INFO, ia, sizeof ia);
    // Only whether there is a tag is compared: on 6.1 and 6.8 it is the first 8 bytes of the
    // program's SHA-1, while newer kernels (CI's) changed the algorithm
    int tagged = 0;
    for (int i = 0; i < 8; i++)
        tagged |= info[8 + i] != 0;
    printf("info: %s type %u same id %d name %s gpl %u xlated %u insns same %d tag set %d\n",
           err(r), *(uint32_t *)info, *(uint32_t *)(info + 4) == id1, (char *)info + 64,
           *(uint32_t *)(info + 84) & 1, *(uint32_t *)(info + 20),
           memcmp(xl, prog, sizeof prog) == 0, tagged);
    *(uint32_t *)(ia + 0) = d;
    printf("info of a cgroup fd: %s\n", err(bpf(GET_INFO, ia, sizeof ia)));
    char link[64] = "";
    char path[32];
    snprintf(path, sizeof path, "/proc/self/fd/%d", p1);
    readlink(path, link, sizeof link - 1);
    printf("fd link: %s\n", link);

    // Replace, detach
    printf("replace p1 with p2: %s\n", err(attach(PROG_ATTACH, d, p2, ALLOW_MULTI | REPLACE, p1)));
    query(d, 0, 64, &flags, &total);
    printf("after replace cnt %u\n", total);
    printf("detach p1: %s\n", err(attach(PROG_DETACH, d, p1, 0, 0)));
    printf("detach without fd: %s\n", err(attach(PROG_DETACH, d, -1, 0, 0)));
    printf("detach p2: %s\n", err(attach(PROG_DETACH, d, p2, 0, 0)));
    query(d, 0, 64, &flags, &total);
    printf("after detach flags %u cnt %u\n", flags, total);
    printf("detach again: %s\n", err(attach(PROG_DETACH, d, p2, 0, 0)));
    // Attached on d without multi: child cgroups can then no longer attach; with override they can
    printf("attach plain: %s\n", err(attach(PROG_ATTACH, d, p1, 0, 0)));
    printf("attach to the child: %s\n", err(attach(PROG_ATTACH, e, p2, ALLOW_MULTI, 0)));
    printf("attach plain again replaces: %s\n", err(attach(PROG_ATTACH, d, p2, 0, 0)));
    query(d, 0, 64, &flags, &total);
    printf("plain cnt %u\n", total);
    printf("detach plain without fd: %s\n", err(attach(PROG_DETACH, d, -1, 0, 0)));
    printf("attach override: %s\n", err(attach(PROG_ATTACH, d, p1, ALLOW_OVERRIDE, 0)));
    printf("attach to the child: %s\n", err(attach(PROG_ATTACH, e, p2, ALLOW_MULTI, 0)));
    printf("devices in d/e with its own program:\n");
    devices("d/e");
    attach(PROG_DETACH, e, p2, 0, 0);
    attach(PROG_DETACH, d, p1, 0, 0);
    printf("devices after detaching:\n");
    devices("d/e");

    // Ones the verifier rejects
    struct { const char *what; struct insn p[4]; int n; } bad[] = {
        {"unknown opcode", {I(0xff, 0, 0, 0, 0), I(0x95, 0, 0, 0, 0)}, 2},
        {"no exit", {I(0xb7, 0, 0, 0, 1)}, 1},
        {"back edge", {I(0xb7, 0, 0, 0, 1), I(0x05, 0, 0, -2, 0), I(0x95, 0, 0, 0, 0)}, 3},
        {"jump out of range", {I(0xb7, 0, 0, 0, 1), I(0x05, 0, 0, 5, 0), I(0x95, 0, 0, 0, 0)}, 3},
        {"r0 not set", {I(0x95, 0, 0, 0, 0)}, 1},
        {"reads r2 unset", {I(0xbf, 0, 2, 0, 0), I(0x95, 0, 0, 0, 0)}, 2},
        {"ctx out of range", {I(0x61, 0, 1, 12, 0), I(0x95, 0, 0, 0, 0)}, 2},
        {"ctx write", {I(0x62, 1, 0, 0, 1), I(0xb7, 0, 0, 0, 1), I(0x95, 0, 0, 0, 0)}, 3},
        {"narrow major", {I(0x69, 0, 1, 4, 0), I(0x95, 0, 0, 0, 0)}, 2},
        {"narrow access_type", {I(0x69, 0, 1, 0, 0), I(0x57, 0, 0, 0, 1), I(0x95, 0, 0, 0, 0)}, 3},
        {"returns 2", {I(0xb7, 0, 0, 0, 2), I(0x95, 0, 0, 0, 0)}, 2},
        {"returns a pointer", {I(0xbf, 0, 1, 0, 0), I(0x95, 0, 0, 0, 0)}, 2},
        {"writes r10", {I(0xb7, 10, 0, 0, 0), I(0xb7, 0, 0, 0, 1), I(0x95, 0, 0, 0, 0)}, 3},
        {"stack", {I(0x62, 10, 0, -4, 1), I(0x61, 0, 10, -4, 0), I(0x57, 0, 0, 0, 1), I(0x95, 0, 0, 0, 0)}, 4},
        {"stack unset", {I(0x61, 0, 10, -8, 0), I(0x57, 0, 0, 0, 1), I(0x95, 0, 0, 0, 0)}, 3},
        {"call ktime_get_ns", {I(0x85, 0, 0, 0, 5), I(0xb7, 0, 0, 0, 1), I(0x95, 0, 0, 0, 0)}, 3},
        {"call an unknown helper", {I(0x85, 0, 0, 0, 100000), I(0xb7, 0, 0, 0, 1), I(0x95, 0, 0, 0, 0)}, 3},
        {"r1 after a call", {I(0x85, 0, 0, 0, 5), I(0x61, 0, 1, 0, 0), I(0x57, 0, 0, 0, 1), I(0x95, 0, 0, 0, 0)}, 4},
        {"div by zero", {I(0xb7, 0, 0, 0, 1), I(0x37, 0, 0, 0, 0), I(0x95, 0, 0, 0, 0)}, 3},
    };
    for (unsigned i = 0; i < sizeof bad / sizeof bad[0]; i++) {
        int fd = load(bad[i].p, bad[i].n, NULL);
        printf("load %s: %s\n", bad[i].what, err(fd));
        close(fd);
    }
    printf("empty: %s\n", err(load(prog, 0, NULL)));
    printf("bad name: %s\n", err(load(prog, n, "a-b")));
    unsigned char la[128] = {0};
    *(uint32_t *)la = 999;
    *(uint32_t *)(la + 4) = n;
    *(uint64_t *)(la + 8) = (uintptr_t)prog;
    *(uint64_t *)(la + 16) = (uintptr_t)"GPL";
    printf("unknown type: %s\n", err(bpf(PROG_LOAD, la, sizeof la)));
    // The part longer than the kernel's bpf_attr must be 0 (6.1 and newer kernels have different
    // lengths, so only a byte far beyond them is compared)
    unsigned char big[512] = {0};
    memcpy(big, la, sizeof la);
    *(uint32_t *)big = TYPE_CGROUP_DEVICE;
    big[400] = 1;
    printf("nonzero tail: %s\n", err(bpf(PROG_LOAD, big, sizeof big)));
    printf("unknown command: %s\n", err(bpf(1000, la, sizeof la)));
    return 0;
}
C
gcc -o /tmp/bpfdev /tmp/bpfdev.c && /tmp/bpfdev
