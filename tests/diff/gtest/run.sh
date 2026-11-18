# Runs a prebuilt suite of gtest syscall tests for the starnix-* and gvisor-* cases:
#
#   sh run.sh <suite> <skip> <program>...
#
# <suite> is starnix (Fuchsia Starnix's tests, built by scripts/build-starnix-tests) or gvisor
# (gVisor's, built by scripts/build-gvisor-tests); scripts/difftest installs each at
# /usr/lib/<suite>-tests. Each test runs in its own process (one crash does not take the rest with
# it) under a 60 s timeout (SIGKILL), and only "<test> ok|failed|skipped|exit <code>" is printed.
# A test killed by the timeout is followed by "  hang:" lines, one per thread of its processes
# (pid, thread, name, state and /proc/<pid>/task/<tid>/syscall, the syscall it waits in), taken
# about 50 s into it; a test that ends by itself drops them, so a slow but passing test still
# prints the same as on the native side. The native side's syscall files are often unreadable
# (Yama lets only an ancestor read them, and the watcher is not one).
# <skip> is a gtest filter of tests not to run, ':'-separated (empty for none); the cases keep the
# reason beside each name. The environment variable ONLY, when set, is a gtest filter of the only
# tests to run. Without the installed suite the case prints that it is missing.
#
# overlayfs_test, and fs_test's OverlayFs tests, run in the starnix-overlay case, with their
# layers on a tmpfs.
#
# Built Starnix programs in no case, and why:
# - they act on the development VM itself with the native side's privileges: reboot_test,
#   sysfs_power_test (suspends), time_test and timerfd_mutable_test (set the clock);
# - Tuxport does not implement what they test: bpf_test (bpf(2) has only cgroup device programs,
#   which the bpf-devices case covers), perf_event_open_test and userfaultfd_test (ENOSYS),
#   tun_test;
# - devices neither side has: input_test, uinput_test, udev_test, touch_standby_test,
#   custom_artifact_test, ext4_write_test;
# - nothing to compare: cache_flush_test has no tests on arm64, and uio_test's one test is
#   process_vm_readv's recorded deviation (the kernel/ptrace map).
suite=$1
skip=$2
shift 2
T=/usr/lib/$suite-tests
if [ ! -d "$T" ]; then
    echo "$suite tests not installed: run scripts/build-$suite-tests, then scripts/difftest"
    exit 0
fi
cd /tmp
# gVisor's ExecTest.ExecuteOnlySpecialFiles names its special files by pid and leaves them behind
# when it fails; Tuxport's store keeps /tmp, and the same pid in the lane's next run fails on them
rm -f /tmp/exec-only-*
# Core files would change the shell's report of a crash on one side only
ulimit -c 0

# The threads of the processes whose command line runs test $1 (the test program, and timeout
# with it when timeout is not busybox's), and of every process under them
dump() {
    roots=
    for p in /proc/[0-9]*; do
        case $(tr '\0' ' ' <"$p/cmdline" 2>/dev/null) in
        *" --gtest_filter=$1 "*) roots="$roots ${p#/proc/}" ;;
        esac
    done
    [ -n "$roots" ] || return
    cat /proc/[0-9]*/stat 2>/dev/null | awk -v roots="$roots" '
        {
            i = index($0, " ("); j = length($0)
            while (j > 0 && substr($0, j, 1) != ")") j--
            if (!i || !j) next
            split(substr($0, j + 2), f, " ")
            parent[substr($0, 1, i - 1)] = f[2]
        }
        END {
            n = split(roots, r, " ")
            for (k = 1; k <= n; k++) tree[r[k]] = 1
            do {
                more = 0
                for (p in parent)
                    if (!(p in tree) && (parent[p] in tree)) { tree[p] = 1; more = 1 }
            } while (more)
            for (p in tree) print p
        }' | sort -n | while read -r p; do
        for t in /proc/"$p"/task/*; do
            s=$(cat "$t/stat" 2>/dev/null) || continue
            comm=${s#* (}
            comm=${comm%) *}
            state=${s##*) }
            state=${state%% *}
            echo "hang: $p ${t##*/} $comm $state $(cat "$t/syscall" 2>&1)"
        done
    done
}

# Every 5 s: the test named in gtest.cur that has been running for 45 s since first seen here
# (45 to 50 s in all) has its threads written to hang.out, its name on the first line. It stops
# when gtest.cur goes
watch() {
    last= since=0 dumped=
    while [ -e gtest.cur ]; do
        sleep 5
        read -r cur <gtest.cur 2>/dev/null || continue
        now=$(date +%s)
        if [ "$cur" != "$last" ]; then
            last=$cur since=$now
        elif [ "$cur" != "$dumped" ] && [ $((now - since)) -ge 45 ]; then
            { echo "$cur"; dump "$cur"; } >hang.tmp 2>&1
            mv hang.tmp hang.out
            dumped=$cur
        fi
    done
}
echo >gtest.cur
watch >/dev/null 2>&1 &
watcher=$!

for bin in "$@"; do
    # Disabled tests do not run even when named in a filter
    $T/bin/$bin --gtest_list_tests --gtest_filter="${ONLY:-*}-$skip" |
        awk '/^[^ ]/ { s = $1 } /^  / { print s $1 }' | grep -v DISABLED_ | while read -r t; do
        # SIGKILL, and the output in a file: a test that ignores SIGTERM, or a child of it that
        # keeps the output open, would otherwise hold the case (and its difftest lane) forever.
        # stdin is not the loop's list of tests, which a test reading its terminal would eat. The
        # test runs in the foreground: the shell would have a background one ignore SIGINT and
        # SIGQUIT
        echo "$t" >gtest.cur
        timeout -s KILL 60 $T/bin/$bin --gtest_filter="$t" --gtest_print_time=0 \
            </dev/null >gtest.out 2>&1
        code=$?
        out=$(cat gtest.out)
        case $out in
        *"[       OK ] $t"*) r=ok ;;
        *"[  SKIPPED ] $t"*) r=skipped ;;
        *"[  FAILED  ] $t"*) r=failed ;;
        *) r="exit $code" ;;
        esac
        echo "$t $r"
        if [ -e hang.out ]; then
            if [ "$code" = 137 ] && [ "$(head -n 1 hang.out)" = "$t" ]; then
                tail -n +2 hang.out | sed 's/^/  /'
            fi
            rm -f hang.out
        fi
    done
# The shell's own crash reports are filtered out (where busybox prints them varies)
done 2>&1 | grep -v -E '^(Segmentation fault|Aborted|Bus error|Illegal instruction|Killed)'
rm -f gtest.out gtest.cur
# Its sleep ends early, and then it sees gtest.cur gone
pkill -P "$watcher" sleep 2>/dev/null
wait "$watcher"
rm -f hang.out hang.tmp
# Some gVisor fs tests change the mode of their test directory, /tmp itself: the native side's
# container ends with the case, but Tuxport's store is reused by the next cases of the lane, which
# expect the sticky, world-writable /tmp (protected_symlinks)
chmod 1777 /tmp
