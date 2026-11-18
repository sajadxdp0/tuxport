# docker-run-options: --privileged
# difftest-files: gtest
# Fuchsia Starnix's syscall tests on files, pipes and mounts: file I/O and metadata, fcntl locks
# and flags, fallocate, renameat2, symlinks, xattrs, tmpfs, sync, sendfile, splice and vmsplice,
# pipes, AIO, inotify, mount and its flags, loop devices. Run by tests/diff/gtest/run.sh, one
# process per test. Tests that differ for a reason recorded in a map, or that cannot be compared,
# are skipped, with the reason above them.
# kernel/vfs: AIO contexts belong to the thread group, not the address space
SKIP="AioWriteTest.WriteCompletesAfterSubmittingProcessExits"
# kernel/vfs: F_SETFL takes O_NOATIME with no ownership check; no inode flags (FS_IOC_SETFLAGS is
# ENOTTY)
SKIP="$SKIP:FcntlTest.NoatimePermission"
SKIP="$SKIP:TmpfsTest.CasefoldDisabledByDefault"
# kernel/procfs: /sys/block/loop<n> entries are directories, not links into /sys/devices
SKIP="$SKIP:LoopTest.SysfsDeviceDirectoryRemovalAndReplacement"
# kernel/vfs: umount ignores mappings
SKIP="$SKIP:MountTest.BusyWithMmap"
# kernel/procfs: procfs mount options (hidepid, gid) are ignored
SKIP="$SKIP:ProcMountsTest.FilesystemMountOptions"
# kernel/vfs: O_NOTIFICATION_PIPE is EINVAL; pipe I/O with faulting buffers ignores Linux's
# per-buffer rules; splice-family argument checks differ; splice into a file stops quietly at
# RLIMIT_FSIZE; vmsplice copies the data at the call
SKIP="$SKIP:PipeTest.NotificationPipeUnsupported:PipeTest.PartialEfaultRollbackAndShortCounts"
SKIP="$SKIP:PipeTest.SpliceValidationAndRlimitFsize"
SKIP="$SKIP:VmspliceTest.FileInPipe:VmspliceTest.ForkWithBufferInPipe:VmspliceTest.ModifyBufferInPipe"
SKIP="$SKIP:VmspliceTest.RemapSharedBufferInPipeThenModify:VmspliceTest.UnmapBufferInPipe"
SKIP="$SKIP:VmspliceTest.UnmapBufferInPipeThenMapInPlace"
SKIP="$SKIP:VmspliceTest/VmspliceUnmapBufferAfterVmsplicingRemappedBufferTest.*"
# kernel/poll: pipe POLLOUT counts bytes, not buffers
SKIP="$SKIP:PipeTest.PolloutFragmentation"
# fs_test's tests on an overlay run in the starnix-overlay case (natively they need a tmpfs)
SKIP="$SKIP:OverlayFs/*"
# Fail on the native side, which runs in docker: its pid namespace turns a dead lock owner's pid
# into 0; its device cgroup refuses mknod of a character device; its newer kernel answers ENOENT
# where the test expects EINVAL for operations on a lazily unmounted mount
SKIP="$SKIP:FcntlLockTest.CloneFiles:MknodTest.MknodChrZeroDoesNotRequireCapMknod"
SKIP="$SKIP:DetachedMountTest.BindFails:DetachedMountTest.MoveFails:DetachedMountTest.PivotRootFails"
SKIP="$SKIP:DetachedMountTest.SubmountFails:DetachedMountTest.UnsharePreservesDetachedState"
# kernel/vfs: no O_TMPFILE (EISDIR; docker's overlay root on 6.8 refuses it too, CI's newer kernel
# supports it)
SKIP="$SKIP:OtmpfileTest.TmpFileLinkIntoAfter:OtmpfileTest.TmpFileWithOExclShouldFailLinkInto"
# Cannot be compared on CI's newer kernel: tmpfs casefolding (6.13) runs there and skips itself on
# 6.8 and Tuxport; the newer kernel refuses changing a detached mount's propagation; musl's pwrite
# asks for RWF_NOAPPEND (6.9), which 6.8 lacks (EOPNOTSUPP), and the O_APPEND writes the test
# expects happen natively on CI on docker's overlay root, while Tuxport writes at the offset
SKIP="$SKIP:FsCasefold/*/tmpfs:DetachedMountTest.ChangePropagationSucceeds"
SKIP="$SKIP:ReadWriteTest.PwriteOAppendAtomicity"
sh /tmp/difftest-files/gtest/run.sh starnix "$SKIP" fs_test fcntl_test fallocate_test read_write_test \
    renameat2_exchange_sticky_test symlink_test xattr_test tmpfs_test sync_test sendfile_test \
    vmsplice_test pipe_test aio_test inotify_test mount_test loop_test
