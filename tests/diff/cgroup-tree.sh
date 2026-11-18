# docker-run-options: --privileged
# Writable cgroup v2 (docs/rounds/163-cgroup-tree.md): creating and removing child cgroups,
# migrating processes, /proc/<pid>/cgroup, cgroup.events, cgroup.stat, the rules of
# subtree_control, cgroup.kill, cgroup.max.depth and max.descendants, the format of interface
# files, paths in a cgroup namespace, clone3's CLONE_INTO_CGROUP. The native side is a privileged
# container (cgroups writable). The container's root cgroup is the root of the cgroup namespace:
# it has processes, controllers cannot be enabled there, and writing most of its files is EPERM;
# Tuxport's root is the real root, so only the t created below it is compared. The full list of
# files (newer kernels have PSI and cpu.stat.local) and the sizes are not compared
cd /sys/fs/cgroup
rmdir t/a/x t/a t/b t 2>/dev/null
mkdir t && echo "mkdir t"
mkdir t 2>&1
stat -c '%A %U %n' t t/cgroup.procs t/cgroup.kill t/cgroup.controllers t/cgroup.subtree_control
for f in cgroup.controllers cgroup.events cgroup.freeze cgroup.kill cgroup.max.depth \
    cgroup.max.descendants cgroup.procs cgroup.stat cgroup.subtree_control cgroup.threads \
    cgroup.type cpu.stat memory.max pids.max; do
    [ -e t/$f ] && echo "has $f" || echo "no $f"
done
echo "controllers [$(cat t/cgroup.controllers)] subtree [$(cat t/cgroup.subtree_control)]"
echo "type $(cat t/cgroup.type)"
mkdir t/a t/b
# For cgroup.stat only the two entries 6.1 already has are compared: newer kernels (CI's) add
# nr_subsys_* for each controller
grep -vE '^nr_(dying_)?subsys_' t/cgroup.stat
cat t/cgroup.events
# Migrating processes
sleep 100 &
pid=$!
echo $pid > t/a/cgroup.procs && echo "moved to t/a"
cat /proc/$pid/cgroup
grep -qx $pid t/a/cgroup.procs && echo "t/a/cgroup.procs has it"
grep -qx $pid cgroup.procs || echo "the root no longer has it"
echo "t: $(tr '\n' ' ' < t/cgroup.events)/ t/a: $(tr '\n' ' ' < t/a/cgroup.events)/ t/b: $(tr '\n' ' ' < t/b/cgroup.events)"
rmdir t/a 2>&1
rmdir t 2>&1
echo $pid > t/b/cgroup.procs && cat /proc/$pid/cgroup
# On the namespace root (natively the container's root, in Tuxport the real root) only
# cgroup.procs, cgroup.threads and cgroup.subtree_control can be written
echo 1 > cgroup.max.depth
echo max > memory.max
echo 1 > memory.current
# Wrong values
echo $pid > t/a/cgroup.threads
echo abc > t/a/cgroup.procs
echo 999999 > t/a/cgroup.procs
echo -5 > t/a/cgroup.procs
# A token starting with a multi-byte character (Tuxport's parser once panicked on it)
for v in "+bogus" "memory" "+memory" "-memory" "" "émemory" "+émemory"; do
    echo "$v" > t/cgroup.subtree_control && echo "subtree_control '$v' ok"
done
for f in cgroup.controllers cgroup.events cgroup.stat cpu.stat; do
    echo 1 > t/$f
done
echo 2 > t/cgroup.kill
echo abc > t/cgroup.kill
echo 2 > t/cgroup.freeze
echo 0 > t/cgroup.freeze && echo "freeze 0 ok"
echo x > t/cgroup.type
echo domain > t/cgroup.type
cat t/cgroup.kill
touch t/new
rm -f t/cgroup.procs
mknod t/fifo p
# Limits on depth and count
echo -1 > t/cgroup.max.depth
echo 1 > t/cgroup.max.depth
mkdir t/a/x
echo "depth $(cat t/cgroup.max.depth)"
echo max > t/cgroup.max.depth
mkdir t/a/x && echo "t/a/x after max"
rmdir t/a/x
echo 2 > t/cgroup.max.descendants
mkdir t/c
echo max > t/cgroup.max.descendants
echo "descendants $(cat t/cgroup.max.descendants)"
# clone3's CLONE_INTO_CGROUP (placed before cgroup.kill: on 6.8, once cgroup.kill has been
# written, children put into the subtree through it are killed too; newer kernels changed this)
cat > /tmp/into.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
struct clone_args { unsigned long long flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup; };
static void into(const char *what, int fd)
{
    struct clone_args a = {.flags = 0x200000000ULL, .exit_signal = SIGCHLD, .cgroup = fd};
    fflush(stdout);
    long r = syscall(435, &a, sizeof a);
    if (r == 0) {
        char buf[64] = "";
        int f = open("/proc/self/cgroup", O_RDONLY);
        read(f, buf, sizeof buf - 1);
        printf("%s: child in %s", what, buf);
        fflush(stdout);
        _exit(0);
    }
    if (r < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        waitpid(r, NULL, 0);
}
int main(void)
{
    into("t/b", open("/sys/fs/cgroup/t/b", O_RDONLY | O_DIRECTORY));
    into("O_PATH t/a", open("/sys/fs/cgroup/t/a", O_PATH | O_DIRECTORY));
    into("not a cgroup", open("/tmp", O_RDONLY | O_DIRECTORY));
    into("bad fd", 1000);
    return 0;
}
C
gcc -o /tmp/into /tmp/into.c && /tmp/into
# cgroup.kill: all processes in the subtree end
echo 1 > t/cgroup.kill && echo "killed"
# If the child ends early, ash reaps it elsewhere and does not report Killed
wait $pid 2>/dev/null
echo "status $?"
echo "t/b: $(tr '\n' ' ' < t/b/cgroup.events)"
# Children follow their parent; paths seen in a cgroup namespace
sh -c "echo \$\$ > t/a/cgroup.procs; sleep 0.2; cat /proc/self/cgroup; exec unshare -C sh -c 'cat /proc/self/cgroup; echo 1 > /sys/fs/cgroup/t/a/cgroup.max.depth; sleep 100 & echo \$! > /sys/fs/cgroup/t/b/cgroup.procs; cat /proc/\$!/cgroup; kill \$!'"
# Ordinary users
su nobody -s /bin/sh -c 'mkdir /sys/fs/cgroup/t/x; echo $$ > /sys/fs/cgroup/t/a/cgroup.procs' 2>&1
rmdir t/a t/b t 2>&1
ls -d t 2>&1
# Names that already exist in a synthetic filesystem: mkdir is EEXIST, checked before whether the
# directory can be modified (mkdir -p relies on this)
for d in /sys /sys/fs /sys/fs/cgroup /proc/sys /proc/self /dev/pts; do
    mkdir $d 2>&1
done
mkdir -p /sys/fs/cgroup/t/p/q && echo "mkdir -p ok"
rmdir /sys/fs/cgroup/t/p/q /sys/fs/cgroup/t/p /sys/fs/cgroup/t
