# docker-run-options: --privileged
# difftest-files: gtest
# gVisor's syscall tests on files, terminals and mounts: PTYs and job control, pivot_root and
# chroot, inotify, fcntl and flock, open and openat2, rename, getdents, close_range, chown and
# pipes. Run by tests/diff/gtest/run.sh, one process per test. Tests that differ for a reason
# recorded in a map, or that cannot be compared, are skipped, with the reason above them.
# kernel/tty: TIOCSPGRP error codes; kernel/procfs fidelity gaps: the devpts root is 0555
SKIP="JobControlTest.SetForegroundProcessGroupEmptyProcessGroup:SetModeTest.SetMode/*"
# kernel/procfs: synthetic files other than cgroup2's report no inotify events (procfs opens and
# reads); kernel/vfs: IN_EXCL_UNLINK filters the wrong events; executing a file reports no
# inotify events
SKIP="$SKIP:Inotify.KernfsBasic:Inotify.KernfsDynamicFileAcrossSave"
SKIP="$SKIP:Inotify.ExcludeUnlink*:Inotify.Exec"
# kernel/vfs: F_GETFL never shows O_DIRECT; open with O_PATH keeps other status flags; locks on
# inode-less files (pipes) are no-ops
SKIP="$SKIP:FcntlTest.GetAllFlags:FcntlTest.GetOpathFlag"
SKIP="$SKIP:FlockTestNoFixture.FlockPipe"
# kernel/vfs: scoped openat2 never returns EAGAIN (the rename variants loop for minutes)
SKIP="$SKIP:Openat2Test.BeneathRaceWithMount:Openat2Test.InRootRaceWithMount"
SKIP="$SKIP:Openat2Test.BeneathRaceWithRename:Openat2Test.InRootRaceWithRename"
# kernel/vfs: anonymous pipes and sockets cannot change owner
SKIP="$SKIP:ChownTest.FchownPipeFileSucceeds:ChownTest.FchownPipeFileFails"
M=MountTest
# kernel/procfs: sysfs has no /sys/kernel
SKIP="$SKIP:$M.LockedMountStopsOverlayOfSysfs"
SKIP="$SKIP:$M.UserNamespaceSysfsMountRevealingLockedMount"
# kernel/vfs D7: fsopen and fsmount are ENOSYS (these skip themselves on Tuxport); kernel/vfs: atime
# ignores the guest's settings; a fault on a mapping of a full tmpfs never raises SIGBUS
SKIP="$SKIP:$M.UserNamespaceProcFsmount*:$M.MountNoAtime:$M.TmpfsSizeMmap"
# Slow by design (it sleeps 30 s to show that shared locks do not block), left out for the time it
# would add to every run
SKIP="$SKIP:FlockTest.BlockingLockNoBlockingForSharedLocks"
# Cannot be compared: Linux 6.4 made O_CREAT|O_DIRECTORY EINVAL, 6.1 (and Tuxport) answer ENOTDIR;
# docker's /tmp is an overlay, so the native side skips the link-count test
SKIP="$SKIP:Openat2Test.DirectoryWithCreate:RenameTest.DirectoryOverwritesDirectoryLinkCount"
sh /tmp/difftest-files/gtest/run.sh gvisor "$SKIP" pty_test pivot_root_test chroot_test \
    mount_test inotify_test fcntl_test flock_test open_test openat2_test rename_test getdents_test \
    close_range_test chown_test pipe_test
