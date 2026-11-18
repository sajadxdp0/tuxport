# docker-run-options: --privileged
# difftest-files: gtest
# Fuchsia Starnix's syscall tests on signals, the vDSO, credentials, seccomp and the kernel log:
# signalfd, the vDSO and vvar mappings, seccomp filters, capabilities and file capabilities,
# set-id execution, syslog(2), /dev/kmsg and /proc/kmsg. Run by tests/diff/gtest/run.sh, one
# process per test. Tests that differ for a reason recorded in a map, or that cannot be compared,
# are skipped, with the reason above them.
# kernel/cred: capset does not mask bits above CAP_LAST_CAP
SKIP="CapsTest.CapsetMasksReservedBits"
# kernel/vfs: security.capability writes check the magic under a mask and accept any v2 or v3
# size, so malformed values are stored
SKIP="$SKIP:FileCapTest.MalformedAndRevision1FileCapabilitiesRejected"
# Cannot be compared: the VM's AppArmor (apparmor_restrict_unprivileged_userns) denies capabilities
# in a user namespace an unprivileged process creates, so setting file capabilities there fails
# natively
SKIP="$SKIP:FileCapTest.FileCapabilityCoercionInsideUserNamespace"
# Fail on the native side at times: docker's own messages keep the VM's kernel log wrapping, so a
# read from its start can find the record gone
SKIP="$SKIP:SyslogTest.Read:SyslogTest.ReadDevKmsg:SyslogTest.DevKmsgSeekSet:SyslogTest.DevKmsgSeekEnd"
# Cannot be compared on CI: 6.9 dropped syslog's CAP_SYS_ADMIN fallback, which 6.1 (Tuxport) and
# 6.8 still have (the tests expect CAP_SYSLOG alone to count); reading /proc/kmsg hangs natively
# on CI until the 60 s timeout
S=SyslogAccessTestAll/SyslogAccessTest
SKIP="$SKIP:$S.SyslogAction*/*_SysAdmin_NoSyslog_*:$S.ProcKmsg*/*_SysAdmin_NoSyslog_DacOverride_*"
SKIP="$SKIP:$S.ProcKmsgRead/Root_SysAdmin_NoSyslog_*:$S.DevKmsgRead/*_SysAdmin_NoSyslog*_Restrict"
SKIP="$SKIP:SyslogTest.ReadProcKmsg:SyslogTest.ProcKmsgPoll"
sh /tmp/difftest-files/gtest/run.sh starnix "$SKIP" signalfd_test vdso_test seccomp_test \
    capabilities_test suid_test syslog_test
