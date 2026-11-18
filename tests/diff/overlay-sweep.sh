# docker-run-options: --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# overlayfs nodes of removed names: after thousands of files were made and removed through an
# overlay (enough for Tuxport to drop the nodes behind them), what still uses a removed name works
# as before: a file open after its unlink, an O_PATH descriptor and a cwd of removed directories,
# /proc/<pid>/exe of a program whose directory was removed (read, and run again), and of one
# renamed while it runs (opened), and an open of a FIFO that waits for a writer while its name is
# removed and the writer comes through another link. Not compared: opening the removed program
# through exe, which Linux allows and Tuxport refuses (ENOENT, kernel/vfs map)
cd /tmp
umount -R /tmp/ovs 2>/dev/null
rm -rf ovs
mkdir ovs
mount -t tmpfs tmpfs ovs
cd ovs
mkdir l u w m
echo lower > l/keep
cat > sweep.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

/* Makes and removes `n` files */
static void churn(int n)
{
    char name[32];
    for (int i = 0; i < n; i++) {
        snprintf(name, sizeof name, "tmp%d", i);
        close(open(name, O_CREAT | O_WRONLY, 0644));
        unlink(name);
    }
}

static const char *links(int dirfd, const char *path, int flags)
{
    static char buf[32];
    struct stat st;
    if (fstatat(dirfd, path, &st, flags))
        return strerror(errno);
    snprintf(buf, sizeof buf, "%lu links", (unsigned long)st.st_nlink);
    return buf;
}

static void *reader(void *arg)
{
    int fd = open("fifo", O_RDONLY);
    char buf[16] = "";
    if (fd < 0) {
        printf("fifo open: %s\n", strerror(errno));
        return arg;
    }
    printf("fifo open: %s\n", links(fd, "", AT_EMPTY_PATH));
    struct stat st;
    printf("fifo fchmod: %s", fchmod(fd, 0600) ? strerror(errno) : "ok");
    fstat(fd, &st);
    printf(", mode %o\n", st.st_mode & 07777);
    read(fd, buf, sizeof buf - 1);
    printf("fifo read: %s", buf);
    close(fd);
    return arg;
}

int main(int argc, char **argv)
{
    if (argc > 1 && strcmp(argv[1], "again") == 0) {
        /* Run again through /proc/<pid>/exe */
        printf("run again: %s\n", argv[0]);
        return 0;
    }
    if (argc > 1) {
        /* The program run from a directory removed later: waits for its parent */
        char c;
        read(0, &c, 1);
        return 0;
    }
    setvbuf(stdout, NULL, _IONBF, 0);
    int held = open("held", O_CREAT | O_RDWR, 0644);
    write(held, "held\n", 5);
    unlink("held");
    mkdir("gone", 0755);
    int opath = open("gone", O_PATH | O_DIRECTORY);
    rmdir("gone");
    /* A child in a directory then removed, and one running a program from another */
    int cwd_pipe[2], exe_pipe[2], done[2];
    pipe(cwd_pipe);
    pipe(exe_pipe);
    pipe(done);
    mkdir("cwd", 0755);
    pid_t cwd_child = fork();
    if (cwd_child == 0) {
        chdir("cwd");
        close(cwd_pipe[0]);
        close(cwd_pipe[1]);
        char c;
        read(done[0], &c, 1);
        char buf[256];
        printf("child cwd: %s, %s\n", getcwd(buf, sizeof buf) ? buf : strerror(errno),
               links(AT_FDCWD, ".", 0));
        return 0;
    }
    mkdir("bin", 0755);
    int in = open("/proc/self/exe", O_RDONLY), out = open("bin/prog", O_CREAT | O_WRONLY, 0755);
    char buf[65536];
    ssize_t n;
    while ((n = read(in, buf, sizeof buf)) > 0)
        write(out, buf, n);
    close(in);
    close(out);
    char path[64];
    pid_t exe_child = fork();
    if (exe_child == 0) {
        dup2(exe_pipe[0], 0);
        execl("bin/prog", "prog", "wait", (char *)NULL);
        _exit(127);
    }
    close(exe_pipe[0]);
    /* One renamed while it runs: its exe still opens the program */
    link("bin/prog", "bin/ren");
    int ren_pipe[2];
    pipe(ren_pipe);
    pid_t ren_child = fork();
    if (ren_child == 0) {
        dup2(ren_pipe[0], 0);
        execl("bin/ren", "ren", "wait", (char *)NULL);
        _exit(127);
    }
    close(ren_pipe[0]);
    usleep(200000);
    rename("bin/ren", "bin/ren2");
    snprintf(path, sizeof path, "/proc/%d/exe", ren_child);
    int exe_fd = open(path, O_RDONLY);
    char magic[4] = "";
    if (exe_fd >= 0)
        read(exe_fd, magic, 4);
    printf("renamed program opens: %s\n",
           exe_fd < 0 ? strerror(errno) : memcmp(magic, "\177ELF", 4) ? "not ELF" : "ELF");
    close(exe_fd);
    write(ren_pipe[1], "x", 1);
    waitpid(ren_child, NULL, 0);
    unlink("bin/ren2");
    unlink("bin/prog");
    rmdir("bin");
    rmdir("cwd");
    churn(3000);
    char target[256] = "";
    snprintf(path, sizeof path, "/proc/%d/exe", exe_child);
    n = readlink(path, target, sizeof target - 1);
    printf("exe: %s\n", n < 0 ? strerror(errno) : target);
    pid_t again = fork();
    if (again == 0) {
        execl(path, "prog", "again", (char *)NULL);
        printf("run again: %s\n", strerror(errno));
        _exit(1);
    }
    waitpid(again, NULL, 0);
    char data[8] = "";
    pread(held, data, 5, 0);
    printf("held: %s, %s", links(held, "", AT_EMPTY_PATH), data);
    printf("O_PATH: %s\n", links(opath, "", AT_EMPTY_PATH));
    write(done[1], "x", 1);
    write(exe_pipe[1], "x", 1);
    waitpid(cwd_child, NULL, 0);
    waitpid(exe_child, NULL, 0);
    /* A reader waits in open while its name goes; the writer comes through another link */
    mkfifo("fifo", 0644);
    link("fifo", "fifo2");
    pthread_t t;
    pthread_create(&t, NULL, reader, NULL);
    usleep(200000);
    unlink("fifo");
    churn(3000);
    int w = open("fifo2", O_WRONLY);
    write(w, "fifo\n", 5);
    close(w);
    pthread_join(t, NULL);
    unlink("fifo2");
    printf("names left: ");
    fflush(stdout);
    return 0;
}
C
gcc -O1 -pthread -o sweep sweep.c
mount -t overlay overlay -o lowerdir=l,upperdir=u,workdir=w m
cd m
../sweep
ls
cd ..
umount m
cd /tmp
umount ovs
rm -rf ovs
