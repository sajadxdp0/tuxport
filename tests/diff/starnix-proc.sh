# docker-run-options: --privileged
# difftest-files: gtest
# Fuchsia Starnix's syscall tests on procfs, sysfs, sysctls and cgroups: per-process and system
# files, their permissions, /proc/sys, /sys/kernel, cpuinfo, pressure files, the cgroup v2 tree and
# its freezer. Run by tests/diff/gtest/run.sh, one process per test. Tests that differ for a
# reason recorded in a map, or that cannot be compared, are skipped, with the reason above them.
# kernel/procfs fidelity gaps: chown delegation of cgroups is impossible
SKIP="CgroupTest.ChownDirectoryDoesNotPropagate:CgroupTest.NewDirectoryOwnedByCreator"
# kernel/procfs: /proc/zoneinfo, /proc/pressure/* and /proc/kallsyms are absent; per-process
# timerslack_ns is absent; Yama is absent; no tracefs
SKIP="$SKIP:ProcfsTest.ZoneInfo:PressureTest/*:ProcKallsymsTest.*:ProcTaskDirTest.TimerslackNs*"
SKIP="$SKIP:SysctlTest/SysctlNodeTest.*/kernel_yama_ptrace_scope:TracefsTest.BufferSizeKbReadWrite"
# kernel/procfs: /proc/<pid>/fd and fdinfo are open to everyone
SKIP="$SKIP:ProcfsAccessTest/ProcfsAccessTest.AccessDeniedToNonOwner/fd"
SKIP="$SKIP:ProcfsAccessTest/ProcfsAccessTest.AccessDeniedToNonOwner/fdinfo"
SKIP="$SKIP:ProcfsAccessTest/ProcfsAccessTest.AccessDeniedToNonOwnerWithoutCapPtrace/fdinfo"
# kernel/procfs fidelity gaps: /proc/<pid>/attr is absent (the test stats it); procfs mount
# options are ignored
SKIP="$SKIP:ProcfsBlksizeTest.StatBlksize:ProcHidePidTest.HidePidAndGidMountOptions"
# kernel/procfs: a reaped process's held directory behaves like a live or missing one
SKIP="$SKIP:ProcTaskDirTest.PidDirFdListingAndReadsFailAfterExit"
SKIP="$SKIP:ProcTaskDirTest.PidDirFdLookupsFailAfterExit"
# No perf_event_open (ENOSYS, kernel/signal D10) and no tun device (kernel/net D1)
SKIP="$SKIP:ProcSelfFdTest.PerfEventFdName:SysctlTest.DisableIpv6Default"
# The io_uring sysctls are 6.6's, and the contract is 6.1
SKIP="$SKIP:SysctlTest/SysctlNodeTest.*/kernel_io_uring_*"
# Cannot be compared: the native side runs in docker's network namespace, which has no
# net.core.rmem_max and wmem_max (Linux registers them for the initial namespace only)
SKIP="$SKIP:SysctlTest/SysctlNodeTest.*/net_core_rmem_max:SysctlTest/SysctlNodeTest.*/net_core_wmem_max"
SKIP="$SKIP:SysctlTest/SysctlTestReadBack.ReadBack/core_rmem_max_*"
SKIP="$SKIP:SysctlTest/SysctlTestReadBack.ReadBack/core_wmem_max_*"
# Cannot be compared: pid 2 is kthreadd only on a whole system; docker's pid namespace has no pid 2
# and Tuxport gives it to an ordinary process
SKIP="$SKIP:ProcTaskDirTest.Kthread*"
# kernel/vfs: no O_TMPFILE (EISDIR; docker's overlay root on 6.8 refuses it too, CI's newer kernel
# supports it)
SKIP="$SKIP:ProcSelfFdTest.TmpFileFdName:ProcSelfFdTest.TmpFileLinkIntoAfterFdName"
# Cannot be compared on CI: the test fails natively on CI's CPU, which has SVE
SKIP="$SKIP:CpuinfoTest.SveFunctionMultiVersioning"
sh /tmp/difftest-files/gtest/run.sh starnix "$SKIP" procfs_test sysctl_test sysfs_kernel_test \
    cpuinfo_test pressure_test cgroup_test cgroup_freezer_test
