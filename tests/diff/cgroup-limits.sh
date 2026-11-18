# docker-run-options: --privileged
# cgroup limits (docs/rounds/164-cgroup-limits.md): pids.max stops fork and thread creation,
# pids.current counts zombies, where pids.events records events, migration ignores the limit,
# disabling and re-enabling the controller returns to the defaults; exceeding memory.max kills
# processes, memory.events (accumulated upwards) and .local, lowering memory.max kills those over
# it, memory.oom.group kills the whole group, memory several processes share counts once
# against memory.max, and cpu.max keeps its period when only a quota is written. The native side
# is a privileged container whose
# root is not the real root, and controllers cannot be enabled while it has processes, so first
# move all processes into init as docker-in-docker does (Tuxport's root is the real root, and
# does the same). For memory only the event counts are compared (max is the number of times the
# limit was hit, which Linux counts on every allocation, so the exact numbers are not compared)
cd /sys/fs/cgroup
mkdir init
for p in $(cat cgroup.procs); do echo $p > init/cgroup.procs 2>/dev/null; done
echo "+pids +memory" > cgroup.subtree_control && echo "root subtree: $(cat cgroup.subtree_control)"

mkdir t
echo "t controllers: $(cat t/cgroup.controllers)"
echo "+pids +memory" > t/cgroup.subtree_control
mkdir t/p t/p/q
# pids.events: newer kernels (CI's) changed it to accumulate upwards, and the meaning it had on
# 6.1 and 6.8 moved to pids.events.local
pev() { cat "$1/pids.events.local" 2>/dev/null || cat "$1/pids.events"; }
echo "p: max $(cat t/p/pids.max) current $(cat t/p/pids.current) $(pev t/p)"
[ -e t/p/q/pids.max ] || echo "q has no pids.max"
for v in -1 4194305 abc 4194304 max 3; do
    echo $v > t/p/pids.max && echo "pids.max $v ok"
done
# Zombies count, threads count, and over the limit pthread_create and fork are EAGAIN; those in q
# count against p
cat > /tmp/pids.c <<'C'
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void *idle(void *a) { pause(); return a; }

static void current(const char *what)
{
    char b[32] = "";
    FILE *f = fopen("/sys/fs/cgroup/t/p/pids.current", "r");
    fgets(b, sizeof b, f);
    fclose(f);
    printf("%s: %s", what, b);
    fflush(stdout);
}

int main(void)
{
    current("start");
    pid_t c = fork();
    if (c == 0)
        _exit(0);
    usleep(200000);
    current("with a zombie");
    waitpid(c, NULL, 0);
    current("reaped");
    pthread_t t[3];
    for (int i = 0; i < 3; i++) {
        int e = pthread_create(&t[i], NULL, idle, NULL);
        printf("thread %d: %s\n", i, e ? strerror(e) : "ok");
    }
    current("threads");
    c = fork();
    printf("fork: %s\n", c < 0 ? strerror(errno) : "ok");
    if (c == 0)
        _exit(0);
    return 0;
}
C
gcc -pthread -o /tmp/pids /tmp/pids.c
sh -c 'echo $$ > /sys/fs/cgroup/t/p/q/cgroup.procs; /tmp/pids'
echo "events p: $(pev t/p), t: $(pev t)"
echo "p current after: $(cat t/p/pids.current)"
# Migrating in ignores the limit
sleep 100 & s1=$!
sleep 100 & s2=$!
sleep 100 & s3=$!
sleep 100 & s4=$!
for s in $s1 $s2 $s3 $s4; do echo $s > t/p/cgroup.procs; done
echo "after migration: $(cat t/p/pids.current) max $(cat t/p/pids.max)"
kill $s1 $s2 $s3 $s4
wait 2>/dev/null
# fork fails in the shell: ash reports an error and exits, leaving two sleeps behind
sh -c 'echo $$ > /sys/fs/cgroup/t/p/cgroup.procs; for i in 1 2 3; do sleep 100 & done; echo unreachable'
echo "shell status $?"
echo "events p: $(pev t/p)"
kill $(cat t/p/cgroup.procs)
# Disable and re-enable pids: the limit and events return to the defaults
echo "-pids" > t/cgroup.subtree_control && echo "+pids" > t/cgroup.subtree_control
echo "p after re-enable: max $(cat t/p/pids.max) $(pev t/p)"

# Keep one watch on the child across controller enable, disable and re-enable.
# O_PATH pins the new pids.events dentry without opening it for poll.
mkdir watch watch/child
python3 - <<'PY' || exit 1
import ctypes, errno, os, select, struct, time
from pathlib import Path

IN_MODIFY = 0x2
event_header = struct.Struct('iIII')

def hit_pids_max():
    try:
        pid = os.fork()
    except OSError as e:
        if e.errno == errno.EAGAIN:
            return
        raise
    if pid == 0:
        os._exit(0)
    os.waitpid(pid, 0)
    raise AssertionError('pids.max allowed the fork')

def wait_for_modify(fd, name):
    deadline = time.monotonic() + 2
    while select.select([fd], [], [], max(0, deadline - time.monotonic()))[0]:
        events = os.read(fd, 4096)
        offset = 0
        while offset < len(events):
            _, mask, _, size = event_header.unpack_from(events, offset)
            offset += event_header.size
            if mask & IN_MODIFY and events[offset:offset + size].rstrip(b'\0') == name:
                return
            offset += size
    raise TimeoutError(f'no IN_MODIFY for {name!r}')

libc = ctypes.CDLL(None, use_errno=True)
child = Path('watch/child')
control = child.parent / 'cgroup.subtree_control'
fd = libc.inotify_init1(os.O_NONBLOCK)
if fd < 0 or libc.inotify_add_watch(fd, os.fsencode(child), IN_MODIFY) < 0:
    raise OSError(ctypes.get_errno(), 'cannot watch the child cgroup')
for phase in ('enable', 're-enable'):
    control.write_text('+pids')
    pin = os.open(child / 'pids.events', os.O_PATH)
    (child / 'pids.max').write_text('1')
    (child / 'cgroup.procs').write_text('0')
    hit_pids_max()
    wait_for_modify(fd, b'pids.events')
    print(phase, 'pids.events directory notification received')
    os.close(pin)
    Path('init/cgroup.procs').write_text('0')
    control.write_text('-pids')
PY
rmdir watch/child watch

# memory.max: exceeding it kills the process (swap.max is set to 0, so nothing is swapped out even
# on hosts with swap)
cat > /tmp/eat.c <<'C'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv)
{
    int mb = argc > 1 ? atoi(argv[1]) : 512;
    for (int i = 0; i < mb; i++)
        memset(malloc(1 << 20), 1, 1 << 20);
    // With an argument it waits to be killed after allocating, at most 10 seconds (so the test
    // case does not hang if it is not killed)
    if (argc > 1) {
        puts("allocated");
        fflush(stdout);
        sleep(10);
        puts("not killed");
        return 0;
    }
    puts("survived");
    return 0;
}
C
gcc -o /tmp/eat /tmp/eat.c
# When the second argument is exact, compare the oom count, otherwise only whether it is greater
# than 0 (when memory.max is lowered, Linux records it repeatedly while waiting for the process
# to exit)
events() {
    awk -v exact="${2:-}" '{ v[$1] = $2 } END {
        oom = exact ? v["oom"] : (v["oom"] > 0 ? ">0" : "0")
        printf "max>0 %d oom %s oom_kill %d oom_group_kill %d\n", (v["max"] > 0), oom, v["oom_kill"], v["oom_group_kill"]
    }' $1
}
mkdir t/m
echo "m: max $(cat t/m/memory.max) $(events t/m/memory.events exact)"
echo 32M > t/m/memory.max && echo 0 > t/m/memory.swap.max && echo "m max $(cat t/m/memory.max)"
sh -c 'echo $$ > /sys/fs/cgroup/t/m/cgroup.procs; exec /tmp/eat'
echo "eat status $?"
echo "m events: $(events t/m/memory.events exact)"
echo "m local: $(events t/m/memory.events.local exact)"
echo "t events: $(events t/memory.events)"
echo "t local: $(events t/memory.events.local)"
peak=$(cat t/m/memory.peak)
echo "m peak over 16M: $((peak > 16 * 1024 * 1024))"
# Lowering memory.max: a process already over it is killed
mkdir t/w
echo 0 > t/w/memory.swap.max
rm -f /tmp/ready
mkfifo /tmp/ready
sh -c 'echo $$ > /sys/fs/cgroup/t/w/cgroup.procs; exec /tmp/eat 48' > /tmp/ready &
w=$!
read line < /tmp/ready
echo "w: $line"
echo 16M > t/w/memory.max && echo "lowered"
# Linux kills it during the write, Tuxport a little later: whether ash prints Killed depends on
# whether it has ended when ash waits for it
wait $w 2>/dev/null
echo "w status $?"
echo "w events: $(events t/w/memory.events)"
rm /tmp/ready
# memory.oom.group: the whole group is killed, and the chosen one is counted twice in oom_kill
mkdir t/g
echo 32M > t/g/memory.max
echo 0 > t/g/memory.swap.max
echo 1 > t/g/memory.oom.group
sh -c 'echo $$ > /sys/fs/cgroup/t/g/cgroup.procs; sleep 10 & /tmp/eat; echo "eat in group $?"; wait; echo "sleep $?"'
echo "group shell status $?"
echo "g events: $(events t/g/memory.events exact)"
# Memory several processes share counts once, as Linux charges each page to one cgroup: 64 MiB of
# shared memory and 32 MiB of private memory written by one process and read by four forked
# children (which borrow the private pages until they write them) stay below a 150 MiB memory.max
mkdir t/s
echo 150M > t/s/memory.max
echo 0 > t/s/memory.swap.max
cat > /tmp/share.c <<'C'
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void)
{
    size_t len = 64 << 20;
    char *shared = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    char *private = mmap(NULL, len / 2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memset(shared, 1, len);
    memset(private, 2, len / 2);
    for (int i = 0; i < 4; i++)
        if (fork() == 0) {
            long sum = 0;
            for (size_t o = 0; o < len; o += 4096)
                sum += shared[o];
            for (size_t o = 0; o < len / 2; o += 4096)
                sum += private[o];
            // Long enough for the OOM killer, which polls, to look
            sleep(1);
            _exit(sum == (long)(len / 4096 + len / 4096) ? 0 : 1);
        }
    int done = 0, status;
    while (wait(&status) > 0)
        done += WIFEXITED(status) && WEXITSTATUS(status) == 0;
    printf("children done: %d\n", done);
    return 0;
}
C
gcc -O1 -o /tmp/share /tmp/share.c
sh -c 'echo $$ > /sys/fs/cgroup/t/s/cgroup.procs; exec /tmp/share'
echo "share status $?"
echo "s events: $(events t/s/memory.events exact)"
echo "t events: $(events t/memory.events)"
echo "root events: $(events memory.events)"

# cpu.max (recorded only): a quota alone keeps the period already set, as cpu_max_write starts
# from the current one
echo "+cpu" > cgroup.subtree_control && echo "+cpu" > t/cgroup.subtree_control && mkdir t/c
for v in "50000 200000" 30000 max "max 100000" 20000 500 "max 999"; do
    echo "$v" > t/c/cpu.max && echo "cpu.max '$v': $(cat t/c/cpu.max)"
done
