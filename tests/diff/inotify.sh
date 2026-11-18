# inotify: events on directories and files and their order, rename cookies, overwriting and
# exchanging, one-shot watches, IN_IGNORED, error codes, FIONREAD, poll and blocking reads, which
# directory fd-based events are reported to; Java's WatchService and Node.js's fs.watch are built
# on it
set -e
mkdir -p /tmp/diff-inotify
cd /tmp/diff-inotify
cat > ino.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int in, wd_dir, wd_file, wd_once, wd_src = -2, wd_target = -2;
static unsigned last_cookie;

static const char *who(int wd)
{
    return wd == wd_dir      ? "dir"
           : wd == wd_file   ? "file"
           : wd == wd_once   ? "once"
           : wd == wd_src    ? "src"
           : wd == wd_target ? "target"
                             : "?";
}

static void print_mask(unsigned m)
{
    static const char *names[] = {"ACCESS", "MODIFY", "ATTRIB", "CLOSE_WRITE", "CLOSE_NOWRITE",
                                  "OPEN", "MOVED_FROM", "MOVED_TO", "CREATE", "DELETE",
                                  "DELETE_SELF", "MOVE_SELF"};
    for (int i = 0; i < 12; i++)
        if (m & (1u << i))
            printf(" %s", names[i]);
    if (m & IN_IGNORED)
        printf(" IGNORED");
    if (m & IN_ISDIR)
        printf(" ISDIR");
    if (m & IN_Q_OVERFLOW)
        printf(" OVERFLOW");
}

/* Read all events queued now; for cookies print only whether each equals the last MOVED_FROM's */
static void drain(const char *step)
{
    char buf[4096] __attribute__((aligned(8)));
    printf("%s:", step);
    for (;;) {
        ssize_t n = read(in, buf, sizeof buf);
        if (n < 0) {
            printf(errno == EAGAIN ? "\n" : " read error %d\n", errno);
            return;
        }
        for (char *p = buf; p < buf + n;) {
            struct inotify_event *e = (struct inotify_event *)p;
            printf(" [%s", who(e->wd));
            print_mask(e->mask);
            if (e->len)
                printf(" %s/%u", e->name, e->len);
            if (e->mask & IN_MOVED_FROM)
                last_cookie = e->cookie;
            if (e->mask & IN_MOVED_TO)
                printf(" cookie%s", e->cookie && e->cookie == last_cookie ? "=" : "!=");
            printf("]");
            p += sizeof *e + e->len;
        }
    }
}

int main(void)
{
    char buf[16];
    mkdir("d", 0755);
    in = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    wd_dir = inotify_add_watch(in, "d", IN_ALL_EVENTS);

    int fd = open("d/f", O_CREAT | O_WRONLY, 0644);
    drain("create");
    wd_file = inotify_add_watch(in, "d/f", IN_ALL_EVENTS);
    write(fd, "hello", 5);
    close(fd);
    drain("write+close");
    fd = open("d/f", O_RDONLY);
    read(fd, buf, sizeof buf);
    close(fd);
    drain("read");
    chmod("d/f", 0600);
    utimensat(AT_FDCWD, "d/f", NULL, 0);
    truncate("d/f", 2);
    drain("chmod+utimens+truncate");
    link("d/f", "d/g");
    rename("d/g", "d/h");
    unlink("d/h");
    drain("link+rename+unlink");
    mkdir("d/sub", 0755);
    rmdir("d/sub");
    symlink("f", "d/s");
    mkfifo("d/p", 0644);
    drain("mkdir+rmdir+symlink+mkfifo");
    rename("d/f", "d/f2");
    syscall(SYS_renameat2, AT_FDCWD, "d/s", AT_FDCWD, "d/p", 2 /* RENAME_EXCHANGE */);
    drain("rename+exchange");
    /* The moved name is watched too: fsnotify_move reports the replaced one's link count before
     * the moved one's MOVE_SELF, and the replaced one's deletion after it */
    wd_src = inotify_add_watch(in, "d/p", IN_ALL_EVENTS | IN_DONT_FOLLOW);
    rename("d/p", "d/f2");
    drain("rename over the watched file");
    mkdir("d/x", 0755);
    mkdir("d/y", 0755);
    wd_src = inotify_add_watch(in, "d/x", IN_ALL_EVENTS);
    wd_target = inotify_add_watch(in, "d/y", IN_ALL_EVENTS);
    rename("d/x", "d/y");
    rmdir("d/y");
    drain("rename over the watched directory");
    unlink("d/f2");
    unlink("d/s");
    drain("unlink");

    close(open("d/o", O_CREAT | O_WRONLY, 0644));
    wd_once = inotify_add_watch(in, "d/o", IN_OPEN | IN_ONESHOT);
    close(open("d/o", O_RDONLY));
    close(open("d/o", O_RDONLY));
    drain("oneshot");

    printf("onlydir %d %d\n", inotify_add_watch(in, "d/o", IN_MODIFY | IN_ONLYDIR), errno);
    printf("empty mask %d %d\n", inotify_add_watch(in, "d", 0), errno);
    printf("add+create %d %d\n", inotify_add_watch(in, "d", IN_MODIFY | IN_MASK_ADD | IN_MASK_CREATE), errno);
    printf("mask create %d %d\n", inotify_add_watch(in, "d", IN_MODIFY | IN_MASK_CREATE), errno);
    printf("same wd %d\n", inotify_add_watch(in, "d", IN_CREATE | IN_DELETE | IN_MASK_ADD) == wd_dir);
    printf("not inotify %d %d\n", inotify_add_watch(1, "d", IN_MODIFY), errno);
    printf("missing %d %d\n", inotify_add_watch(in, "d/missing", IN_MODIFY), errno);
    printf("bad wd %d %d\n", inotify_rm_watch(in, 12345), errno);

    unlink("d/o");
    int avail = 0;
    ioctl(in, FIONREAD, &avail);
    printf("fionread %d short read %zd %d\n", avail, read(in, buf, 8), errno);
    drain("after errors");

    /* Blocking read: the child creates a directory after a while (only one event, so the result
     * does not depend on when the parent wakes up) */
    fcntl(in, F_SETFL, 0);
    pid_t pid = fork();
    if (pid == 0) {
        usleep(100000);
        mkdir("d/late", 0755);
        _exit(0);
    }
    struct pollfd p = {in, POLLIN, 0};
    printf("poll %d %d\n", poll(&p, 1, 5000), p.revents);
    char ev[256] __attribute__((aligned(8)));
    ssize_t n = read(in, ev, sizeof ev);
    struct inotify_event *e = (struct inotify_event *)ev;
    printf("blocking read %s", n > 0 ? e->name : "?");
    print_mask(e->mask);
    printf("\n");
    waitpid(pid, NULL, 0);
    fcntl(in, F_SETFL, O_NONBLOCK);
    rmdir("d/late");
    inotify_rm_watch(in, wd_dir);
    drain("rm_watch");
    return 0;
}
C
gcc -O2 -o ino ino.c
./ino

# fd-based events follow the directory entry the file was opened through: a file opened before
# the watch was set up, renamed after opening, still open when deleted (IN_DELETE_SELF waits for
# the close), hard links, an open directory removed with rmdir
cat > fdev.c <<'C'
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <sys/inotify.h>
#include <sys/stat.h>

static int in;
static int wa, wb, wf;
static void drain(const char *tag) {
    char buf[4096];
    printf("[%s]", tag);
    for (;;) {
        ssize_t n = read(in, buf, sizeof buf);
        if (n <= 0) break;
        for (char *p = buf; p < buf + n;) {
            struct inotify_event *e = (void *)p;
            const char *w = e->wd == wa ? "a" : e->wd == wb ? "b" : e->wd == wf ? "f" : "?";
            printf(" %s:%x%s%s", w, e->mask, e->len ? ":" : "", e->len ? e->name : "");
            p += sizeof *e + e->len;
        }
    }
    printf("\n");
}

int main(void) {
    mkdir("/tmp/diff-inotify/fd", 0755); mkdir("/tmp/diff-inotify/fd/a", 0755); mkdir("/tmp/diff-inotify/fd/b", 0755);
    in = inotify_init1(IN_NONBLOCK);
    /* 1. A file opened before the watch is set up */
    int fd = open("/tmp/diff-inotify/fd/a/x", O_CREAT | O_WRONLY, 0644);
    wa = inotify_add_watch(in, "/tmp/diff-inotify/fd/a", IN_ALL_EVENTS);
    wb = inotify_add_watch(in, "/tmp/diff-inotify/fd/b", IN_ALL_EVENTS);
    write(fd, "1", 1);
    close(fd);
    drain("pre-open close");
    /* 2. Renamed into another directory after opening */
    fd = open("/tmp/diff-inotify/fd/a/x", O_WRONLY);
    rename("/tmp/diff-inotify/fd/a/x", "/tmp/diff-inotify/fd/b/y");
    write(fd, "2", 1);
    close(fd);
    drain("rename then write");
    /* 3. Delete the last link while open */
    wf = inotify_add_watch(in, "/tmp/diff-inotify/fd/b/y", IN_ALL_EVENTS);
    fd = open("/tmp/diff-inotify/fd/b/y", O_RDONLY);
    unlink("/tmp/diff-inotify/fd/b/y");
    drain("unlink while open");
    close(fd);
    drain("close after unlink");
    /* 4. Two opens */
    int f1 = open("/tmp/diff-inotify/fd/b/z", O_CREAT | O_RDWR, 0644);
    int f2 = open("/tmp/diff-inotify/fd/b/z", O_RDONLY);
    wf = inotify_add_watch(in, "/tmp/diff-inotify/fd/b/z", IN_ALL_EVENTS);
    drain("two opens");
    unlink("/tmp/diff-inotify/fd/b/z");
    close(f1);
    drain("close one");
    close(f2);
    drain("close two");
    /* 5. Hard links */
    fd = open("/tmp/diff-inotify/fd/a/h", O_CREAT | O_RDWR, 0644);
    link("/tmp/diff-inotify/fd/a/h", "/tmp/diff-inotify/fd/b/h");
    wf = inotify_add_watch(in, "/tmp/diff-inotify/fd/a/h", IN_ALL_EVENTS);
    unlink("/tmp/diff-inotify/fd/a/h");
    drain("unlink one link");
    unlink("/tmp/diff-inotify/fd/b/h");
    drain("unlink other link");
    close(fd);
    drain("close hardlinked");
    /* 6. Delete while not open */
    close(open("/tmp/diff-inotify/fd/a/q", O_CREAT | O_RDWR, 0644));
    wf = inotify_add_watch(in, "/tmp/diff-inotify/fd/a/q", IN_ALL_EVENTS);
    unlink("/tmp/diff-inotify/fd/a/q");
    drain("unlink closed");
    /* 7. rmdir of an open directory */
    mkdir("/tmp/diff-inotify/fd/a/d", 0755);
    wf = inotify_add_watch(in, "/tmp/diff-inotify/fd/a/d", IN_ALL_EVENTS);
    fd = open("/tmp/diff-inotify/fd/a/d", O_RDONLY | O_DIRECTORY);
    rmdir("/tmp/diff-inotify/fd/a/d");
    drain("rmdir open dir");
    close(fd);
    drain("close dir");
    return 0;
}
C
gcc -O2 -o fdev fdev.c
./fdev

cat > Watch.java <<'JAVA'
import java.nio.file.*;
import static java.nio.file.StandardWatchEventKinds.*;

public class Watch {
    public static void main(String[] args) throws Exception {
        Path dir = Files.createDirectories(Path.of("/tmp/diff-inotify/jw"));
        WatchService ws = FileSystems.getDefault().newWatchService();
        dir.register(ws, ENTRY_CREATE, ENTRY_MODIFY, ENTRY_DELETE);
        Path f = dir.resolve("a.txt");
        Files.writeString(f, "x");
        Files.delete(f);
        boolean deleted = false;
        while (!deleted) {
            WatchKey key = ws.take();
            for (WatchEvent<?> e : key.pollEvents()) {
                System.out.println("java " + e.kind() + " " + e.context());
                deleted |= e.kind() == ENTRY_DELETE;
            }
            key.reset();
        }
    }
}
JAVA
javac -d . Watch.java
java -cp . Watch

cat > watch.js <<'JS'
const fs = require('fs');
fs.mkdirSync('/tmp/diff-inotify/nw', { recursive: true });
const seen = [];
const w = fs.watch('/tmp/diff-inotify/nw', (event, name) => {
  seen.push(`${event} ${name}`);
  if (seen.length === 3) {
    console.log('node', seen.join(', '));
    w.close();
  }
});
fs.writeFileSync('/tmp/diff-inotify/nw/a.txt', 'x');
fs.unlinkSync('/tmp/diff-inotify/nw/a.txt');
JS
node watch.js
