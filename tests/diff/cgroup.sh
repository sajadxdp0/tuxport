# docker-run-options: --privileged
# The cgroup v2 view: /proc/<pid>/cgroup, /proc/cgroups, the mount, the permissions and contents
# of the files in /sys/fs/cgroup, and that the memory limits the JVM and Node.js see agree with
# memory.max. The native side is a privileged docker container (cgroup namespace, writable
# mount, no --memory); for memory.max and the others that depend on host values, only whether
# they are self-consistent is compared. Writes and child cgroups are in cgroup-tree
cd /sys/fs/cgroup
echo "self: $(cat /proc/self/cgroup)"
echo "shell: $(cat /proc/$$/cgroup)"
# Which controllers /proc/cgroups lists depends on the kernel configuration (newer kernels do not
# list cpuset and memory without v1 support), so only that all are on the v2 hierarchy (0) and
# that cpu is there is compared
head -1 /proc/cgroups
awk 'NR > 1 && $2 != 0 { v1 = 1 } $1 == "cpu" { cpu = 1 }
    END { print (v1 ? "a v1 hierarchy" : "all on the v2 hierarchy"), (cpu ? "with cpu" : "without cpu") }' \
    /proc/cgroups
grep ' /sys/fs/cgroup ' /proc/self/mountinfo | cut -d ' ' -f 4-
grep ' /sys/fs/cgroup ' /proc/mounts
echo "fs type: $(stat -f -c %t /sys/fs/cgroup)"
stat -c '%A %n' /sys/fs/cgroup
for c in cpuset cpu memory pids; do
    grep -qw $c cgroup.controllers && echo "controller $c"
done
# Files whose contents do not depend on the host. Tuxport reports 6.1, and newer kernels have extra
# entries in cgroup.stat, memory.events, cpu.stat and pids.events (nr_subsys_*, sock_throttled,
# nice_usec...), so for these only the entries 6.1 already has are compared
for f in cgroup.events cgroup.freeze cgroup.max.depth cgroup.max.descendants \
    cgroup.subtree_control cgroup.type cpu.idle cpu.max cpu.max.burst cpu.weight \
    cpu.weight.nice cpuset.cpus cpuset.cpus.effective cpuset.cpus.partition cpuset.mems \
    cpuset.mems.effective memory.high memory.low memory.min memory.oom.group \
    memory.swap.current memory.swap.high; do
    echo "$(stat -c %A $f) $f: $(tr '\n' '|' < $f)"
done
echo "$(stat -c %A cgroup.stat) cgroup.stat: $(grep -E '^nr_(dying_)?descendants ' cgroup.stat | tr '\n' '|')"
for f in memory.events memory.events.local; do
    echo "$(stat -c %A $f) $f: $(grep -E '^(low|high|max|oom|oom_kill|oom_group_kill) ' $f | tr '\n' '|')"
done
echo "$(stat -c %A memory.swap.events) memory.swap.events: $(grep -E '^(high|max|fail) ' memory.swap.events | tr '\n' '|')"
echo "$(stat -c %A pids.events) pids.events: $(grep -E '^max ' pids.events | tr '\n' '|')"
for k in usage_usec user_usec system_usec nr_periods nr_throttled throttled_usec nr_bursts \
    burst_usec; do
    grep -q "^$k " cpu.stat && printf '%s ' $k
done
echo
# memory.stat: newer kernels added entries, and whether swapcached, *_thp and zswap are present
# also depends on the kernel configuration, so only the order of the entries that 6.1 already has
# and that do not depend on the configuration is compared, and that the values are all numbers
echo "$(stat -c %A memory.stat) memory.stat: $(grep -oE '^[a-z_]+' memory.stat | grep -xE \
    'anon|file|kernel|kernel_stack|pagetables|sec_pagetables|percpu|sock|vmalloc|shmem|file_mapped|file_dirty|file_writeback|inactive_anon|active_anon|inactive_file|active_file|unevictable|slab_reclaimable|slab_unreclaimable|slab|workingset_[a-z_]+|pgscan|pgsteal|pgscan_kswapd|pgscan_direct|pgsteal_kswapd|pgsteal_direct|pgfault|pgmajfault|pgrefill|pgactivate|pgdeactivate|pglazyfree|pglazyfreed' |
    tr '\n' ' ')"
grep -vqE '^[a-z_]+ [0-9]+$' memory.stat || echo "memory.stat has a line that is not a name and a number"
# Those that depend on the host and the state at the time: only the format is compared (from 6.12
# on memory.peak is writable, so its permissions are not compared; the container's pids.max may
# have a limit)
for f in cgroup.controllers cgroup.procs cgroup.threads memory.current memory.max \
    memory.swap.max pids.current pids.max cpu.stat; do
    echo "$(stat -c %A $f) $f"
done
for f in memory.current memory.peak pids.current; do
    grep -qxE '[0-9]+' $f && echo "$f is a number"
done
for f in memory.max pids.max; do
    grep -qxE '[0-9]+|max' $f && echo "$f is a number or max"
done
grep -qx $$ cgroup.procs && echo "the shell is in cgroup.procs"
grep -qx $$ cgroup.threads && echo "the shell is in cgroup.threads"
# Writable mount (without writing the root's cgroup.freeze and the like: on the native side that
# would freeze the whole container): read-only files cannot be written, files cannot be created
# and interface files cannot be deleted, child cgroups can be created and removed
echo 1 > memory.current
touch new
rm -f memory.max
mkdir sub && rmdir sub && echo "mkdir and rmdir sub"
# The memory limit runtimes see: when memory.max is max it is the physical memory
limit=$(cat memory.max)
total=$(($(awk '/^MemTotal:/ { print $2 }' /proc/meminfo) * 1024))
cat > /tmp/Mem.java <<'J'
public class Mem {
    public static void main(String[] args) {
        var os = (com.sun.management.OperatingSystemMXBean)
            java.lang.management.ManagementFactory.getOperatingSystemMXBean();
        System.out.println(os.getTotalMemorySize());
    }
}
J
jvm=$(java /tmp/Mem.java)
if [ "$limit" = max ]; then want=$total; else want=$limit; fi
[ "$jvm" = "$want" ] && echo "the JVM sees the limit" || echo "the JVM sees $jvm, not $want"
# With a writable mount the JVM considers itself in a container only when there is a CPU or memory
# limit (Tuxport's memory.max is the OOM killer's limit, which the native side usually lacks), and
# in a container there is no CPU quota
s=$(java -XshowSettings:system -version 2>&1)
echo "$s" | grep Provider
if echo "$s" | grep -q 'not containerized'; then jvm=max; else jvm=$limit; fi
[ "$jvm" = "$limit" ] && echo "the JVM is in a container exactly when there is a limit"
echo "$s" | grep -E 'CPU (Period|Quota|Shares)' | grep -vE ': (-1|100000us)$'
node -e '
const m = require("fs").readFileSync("/sys/fs/cgroup/memory.max", "utf8").trim();
const c = process.constrainedMemory();
const ok = m === "max" ? c >= 2 ** 64 - 1 : c === Number(m);
console.log(ok ? "node sees the limit" : `node sees ${c}, memory.max is ${m}`);
'
rm -f /tmp/Mem.java
