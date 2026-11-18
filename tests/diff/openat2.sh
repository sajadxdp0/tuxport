# openat2 (fs/open.c and fs/namei.c of Linux 6.1): the checks on struct open_how and on the flags
# (unlike openat, anything unknown is EINVAL), the path-resolution restrictions of
# RESOLVE_BENEATH, IN_ROOT, NO_SYMLINKS, NO_MAGICLINKS and NO_XDEV, which also apply when O_CREAT
# creates a file through a dangling symlink. Everything is done in /tmp and /proc: in the
# container /etc/hosts and the like are bind mounts, in Tuxport they are not. From 6.4 on
# O_DIRECTORY|O_CREAT is EINVAL (not in 6.1); in 6.1 RESOLVE_CACHED with O_DIRECTORY is EAGAIN
# (the O_TMPFILE macro includes O_DIRECTORY), in later kernels it is not; 6.1 and 6.8 strip
# FMODE_NONOTIFY (0x4000000) as a bit that should not be given, while the newer kernels on CI
# treat it as an unknown bit, EINVAL: these three are not compared. RESOLVE_CACHED natively
# depends on whether the dentry is in the cache, so only "success or EAGAIN" is compared
set -e
cd /tmp
cat > openat2.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_openat2
#define SYS_openat2 437
#endif
#define RESOLVE_NO_XDEV 0x01
#define RESOLVE_NO_MAGICLINKS 0x02
#define RESOLVE_NO_SYMLINKS 0x04
#define RESOLVE_BENEATH 0x08
#define RESOLVE_IN_ROOT 0x10
#define RESOLVE_CACHED 0x20
#ifndef O_TMPFILE
#define O_TMPFILE (020000000 | O_DIRECTORY)
#endif

struct how {
    uint64_t flags, mode, resolve;
};

static long o2(int dirfd, const char *path, uint64_t flags, uint64_t mode, uint64_t resolve)
{
    struct how h = {flags, mode, resolve};
    return syscall(SYS_openat2, dirfd, path, &h, sizeof h);
}

static const char *ename(int e)
{
    switch (e) {
    case EINVAL: return "EINVAL";
    case E2BIG: return "E2BIG";
    case EFAULT: return "EFAULT";
    case EXDEV: return "EXDEV";
    case ELOOP: return "ELOOP";
    case ENOENT: return "ENOENT";
    case ENOTDIR: return "ENOTDIR";
    case EAGAIN: return "EAGAIN";
    case EEXIST: return "EEXIST";
    case EISDIR: return "EISDIR";
    case EBADF: return "EBADF";
    case EACCES: return "EACCES";
    default: return strerror(e);
    }
}

static struct stat root_st;

/* Print the result; on success say what was opened (the root directory, a file), then close it */
static void show(const char *what, long fd)
{
    if (fd < 0) {
        printf("%-44s %s\n", what, ename(errno));
        return;
    }
    struct stat st;
    fstat((int)fd, &st);
    const char *kind = S_ISDIR(st.st_mode) ? "dir" : S_ISREG(st.st_mode) ? "file" : "other";
    int is_root = st.st_dev == root_st.st_dev && st.st_ino == root_st.st_ino;
    printf("%-44s ok %s%s size %lld\n", what, kind, is_root ? " (the root)" : "",
           S_ISREG(st.st_mode) ? (long long)st.st_size : 0LL);
    close((int)fd);
}

static void write_file(const char *path, const char *text)
{
    FILE *f = fopen(path, "w");
    fputs(text, f);
    fclose(f);
}

int main(void)
{
    system("rm -rf /tmp/oa2");
    mkdir("/tmp/oa2", 0755);
    mkdir("/tmp/oa2/root", 0755);
    mkdir("/tmp/oa2/root/a", 0755);
    mkdir("/tmp/oa2/root/a/b", 0755);
    write_file("/tmp/oa2/root/a/b/f", "inside\n");
    write_file("/tmp/oa2/outside", "outside!\n");
    symlink("a/b/f", "/tmp/oa2/root/rel");
    symlink("/tmp/oa2/root/a/b/f", "/tmp/oa2/root/abs");
    symlink("../outside", "/tmp/oa2/root/up");
    symlink("/a/b/f", "/tmp/oa2/root/inroot");
    symlink("a", "/tmp/oa2/root/dirlink");
    symlink("new1", "/tmp/oa2/root/dangling");
    symlink("/new2", "/tmp/oa2/root/dangling_abs");
    symlink("loop", "/tmp/oa2/root/loop");
    symlink("/proc/self", "/tmp/oa2/root/procself");
    int root = open("/tmp/oa2/root", O_RDONLY | O_DIRECTORY);
    fstat(root, &root_st);
    int file = open("/tmp/oa2/outside", O_RDONLY);
    struct how h = {O_RDONLY, 0, 0};
    char big[64] = {0};
    memcpy(big, &h, sizeof h);

    puts("struct open_how");
    show("size 0", syscall(SYS_openat2, root, "a", &h, 0));
    show("size 23", syscall(SYS_openat2, root, "a", &h, 23));
    show("size 24", syscall(SYS_openat2, root, "a", &h, 24));
    show("size 64, zero tail", syscall(SYS_openat2, root, "a", big, 64));
    big[40] = 1;
    show("size 64, nonzero tail", syscall(SYS_openat2, root, "a", big, 64));
    show("size 4097", syscall(SYS_openat2, root, "a", big, 4097));
    show("bad pointer", syscall(SYS_openat2, root, "a", (void *)8, 24));

    puts("flags");
    show("unknown flag bit", o2(root, "a", 1u << 30, 0, 0));
    show("bit 32", o2(root, "a", 1ull << 32, 0, 0));
    show("unknown resolve bit", o2(root, "a", 0, 0, 0x40));
    show("BENEATH | IN_ROOT", o2(root, "a", 0, 0, RESOLVE_BENEATH | RESOLVE_IN_ROOT));
    show("mode without O_CREAT", o2(root, "a", 0, 0644, 0));
    show("mode with S_IFREG", o2(root, "m", O_CREAT | O_WRONLY, 0100644, 0));
    show("O_TMPFILE read-only", o2(root, "a", O_TMPFILE | O_RDONLY, 0600, 0));
    show("__O_TMPFILE without O_DIRECTORY", o2(root, "a", 020000000 | O_RDWR, 0600, 0));
    show("O_PATH | O_RDWR", o2(root, "a", O_PATH | O_RDWR, 0, 0));
    show("O_PATH | O_LARGEFILE", o2(root, "a", O_PATH | 0400000, 0, 0));
    show("O_PATH | O_NOFOLLOW | O_CLOEXEC", o2(root, "rel", O_PATH | O_NOFOLLOW | O_CLOEXEC, 0, 0));
    show("bad flags before a bad path", o2(root, (const char *)8, 1u << 30, 0, 0));
    show("CACHED with O_CREAT", o2(root, "c", O_CREAT | O_WRONLY, 0644, RESOLVE_CACHED));
    long c = o2(root, "a/b/f", 0, 0, RESOLVE_CACHED);
    printf("%-44s %d\n", "CACHED opens or says EAGAIN", c >= 0 || errno == EAGAIN);
    if (c >= 0)
        close((int)c);
    long fd = o2(root, "a/b/f", O_RDONLY | O_CLOEXEC, 0, 0);
    printf("%-44s cloexec %d largefile %d\n", "fd flags", fcntl((int)fd, F_GETFD) & FD_CLOEXEC,
           !!(fcntl((int)fd, F_GETFL) & 0400000));
    close((int)fd);

    puts("RESOLVE_BENEATH");
    uint64_t B = RESOLVE_BENEATH;
    show("a/b/f", o2(root, "a/b/f", 0, 0, B));
    show("a/b/../b/f", o2(root, "a/b/../b/f", 0, 0, B));
    show("a/..", o2(root, "a/..", 0, 0, B));
    show(".", o2(root, ".", 0, 0, B));
    show("..", o2(root, "..", 0, 0, B));
    show("a/../..", o2(root, "a/../..", 0, 0, B));
    show("/tmp", o2(root, "/tmp", 0, 0, B));
    show("/tmp with a bad dirfd", o2(-1, "/tmp", 0, 0, B));
    show("rel (relative symlink)", o2(root, "rel", 0, 0, B));
    show("abs (absolute symlink)", o2(root, "abs", 0, 0, B));
    show("abs with O_NOFOLLOW | O_PATH", o2(root, "abs", O_PATH | O_NOFOLLOW, 0, B));
    show("up (symlink to ../outside)", o2(root, "up", 0, 0, B));
    show("dirlink/b/f", o2(root, "dirlink/b/f", 0, 0, B));
    show("procself", o2(root, "procself", 0, 0, B));
    show("loop", o2(root, "loop", 0, 0, B));
    show("missing", o2(root, "missing", 0, 0, B));

    puts("RESOLVE_IN_ROOT");
    uint64_t R = RESOLVE_IN_ROOT;
    show("/a/b/f", o2(root, "/a/b/f", 0, 0, R));
    show("/", o2(root, "/", 0, 0, R));
    show("..", o2(root, "..", 0, 0, R));
    show("../../a/b/f", o2(root, "../../a/b/f", 0, 0, R));
    show("inroot (symlink to /a/b/f)", o2(root, "inroot", 0, 0, R));
    show("abs (symlink to the real path)", o2(root, "abs", 0, 0, R));
    show("up (symlink to ../outside)", o2(root, "up", 0, 0, R));
    show("procself", o2(root, "procself", 0, 0, R));
    show("/tmp with a bad dirfd", o2(-1, "/tmp", 0, 0, R));
    show("a relative path from a file", o2(file, "x", 0, 0, R));

    puts("RESOLVE_NO_SYMLINKS");
    uint64_t S = RESOLVE_NO_SYMLINKS;
    show("a/b/f", o2(root, "a/b/f", 0, 0, S));
    show("rel", o2(root, "rel", 0, 0, S));
    show("rel with O_NOFOLLOW", o2(root, "rel", O_NOFOLLOW, 0, S));
    show("rel with O_NOFOLLOW | O_PATH", o2(root, "rel", O_NOFOLLOW | O_PATH, 0, S));
    show("dirlink/b/f", o2(root, "dirlink/b/f", 0, 0, S));
    show("/proc/self/status", o2(root, "/proc/self/status", 0, 0, S));
    show("/proc/1/status", o2(root, "/proc/1/status", 0, 0, S));

    puts("RESOLVE_NO_MAGICLINKS");
    uint64_t M = RESOLVE_NO_MAGICLINKS;
    char fdpath[64];
    snprintf(fdpath, sizeof fdpath, "/proc/self/fd/%d", file);
    show("/proc/self/fd/<n>", o2(root, fdpath, 0, 0, M));
    show("/proc/self/fd/<n> with O_NOFOLLOW | O_PATH", o2(root, fdpath, O_PATH | O_NOFOLLOW, 0, M));
    show("/proc/self/cwd", o2(root, "/proc/self/cwd", 0, 0, M));
    show("/proc/self/root", o2(root, "/proc/self/root", 0, 0, M));
    show("/proc/self/exe", o2(root, "/proc/self/exe", 0, 0, M));
    show("/proc/self/ns/mnt", o2(root, "/proc/self/ns/mnt", 0, 0, M));
    show("/proc/self (a plain symlink)", o2(root, "/proc/self", 0, 0, M));
    show("rel", o2(root, "rel", 0, 0, M));
    show("/proc/self/fd/<n> without flags", o2(root, fdpath, 0, 0, 0));

    puts("RESOLVE_NO_XDEV");
    uint64_t X = RESOLVE_NO_XDEV;
    int proc = open("/proc", O_RDONLY | O_DIRECTORY);
    show("/tmp/oa2/root/a/b/f", o2(root, "/tmp/oa2/root/a/b/f", 0, 0, X));
    show("/proc", o2(root, "/proc", 0, 0, X));
    show("procself", o2(root, "procself", 0, 0, X));
    show("abs", o2(root, "abs", 0, 0, X));
    show("a/../abs (.. takes the root first)", o2(root, "a/../abs", 0, 0, X));
    show("/tmp/oa2/root/abs", o2(root, "/tmp/oa2/root/abs", 0, 0, X));
    show("from /proc: self/status", o2(proc, "self/status", 0, 0, X));
    show("from /proc: ..", o2(proc, "..", 0, 0, X));
    show("from /proc: self/cwd", o2(proc, "self/cwd", 0, 0, X));
    show("from /proc: self/fd/<n>", o2(proc, fdpath + 6, 0, 0, X));
    show("from /proc: thread-self/..", o2(proc, "thread-self/..", 0, 0, X));
    show("from /proc, BENEATH: self/status", o2(proc, "self/status", 0, 0, B));
    show("from /proc, BENEATH: self/cwd", o2(proc, "self/cwd", 0, 0, B));
    show("from /proc, IN_ROOT: self/fd/<n>", o2(proc, fdpath + 6, 0, 0, R));
    show("from /proc, IN_ROOT: /self/status", o2(proc, "/self/status", 0, 0, R));

    puts("O_CREAT");
    int W = O_CREAT | O_WRONLY;
    show("BENEATH a/new", o2(root, "a/new", W, 0644, B));
    show("BENEATH dangling (to new1)", o2(root, "dangling", W, 0644, B));
    printf("%-44s %d\n", "new1 exists", faccessat(root, "new1", F_OK, 0) == 0);
    show("BENEATH dangling_abs (to /new2)", o2(root, "dangling_abs", W, 0644, B));
    show("NO_SYMLINKS dangling_abs", o2(root, "dangling_abs", W, 0644, S));
    show("IN_ROOT dangling_abs", o2(root, "dangling_abs", W, 0644, R));
    printf("%-44s %d\n", "new2 exists in the root", faccessat(root, "new2", F_OK, 0) == 0);
    printf("%-44s %d\n", "/new2 exists", access("/new2", F_OK) == 0);
    show("NO_SYMLINKS | O_EXCL dangling", o2(root, "dangling", W | O_EXCL, 0644, S));
    show("NO_XDEV dangling_abs", o2(root, "dangling_abs", W, 0644, X));
    show("BENEATH ../escape", o2(root, "../escape", W, 0644, B));
    show("IN_ROOT ../escape", o2(root, "../escape", W, 0644, R));
    printf("%-44s %d %d\n", "escape inside, outside", faccessat(root, "escape", F_OK, 0) == 0,
           access("/tmp/oa2/escape", F_OK) == 0);
    show("BENEATH a directory with O_CREAT", o2(root, "a", W, 0644, B));
    show("IN_ROOT / with O_CREAT", o2(root, "/", W, 0644, R));
    show("BENEATH / with O_CREAT", o2(root, "/", W, 0644, B));
    show("ENOTDIR: a relative path from a file", o2(file, "x", 0, 0, 0));
    show("an absolute path from a file", o2(file, "/tmp", 0, 0, 0));
    return 0;
}
C
gcc -O2 -Wall -o openat2 openat2.c && ./openat2
