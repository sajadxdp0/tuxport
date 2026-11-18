# docker-run-options: --privileged
# difftest-files: gtest
# Fuchsia Starnix's syscall tests on memory: mmap and mremap corner cases, address-space bounds,
# anonymous inodes, mlock, memfd, membarrier, /proc/<pid>/mem and pagemap, personality,
# process_mrelease. Run by
# tests/diff/gtest/run.sh, one process per test. Tests that differ for a reason recorded in a
# map, or that cannot be compared, are skipped, with the reason above them.
# Fail on the native side, depending on its newer kernel (VMA merging of file mappings)
SKIP="MMapProcTest.AdjacentFileMappings"
SKIP="$SKIP:MMapProcTest.PrivateFileMappingMremapShrinkAndMmapRetainsSeparateMappings"
# No ASLR (the kernel/process map's open question): the stack ends at the top of the address space,
# so the page the test maps there is taken and it skips itself
SKIP="$SKIP:HighestAddress/AspaceTest.RangeFault/*"
# bpf(2) has only cgroup device programs, so no map fd (kernel/procfs); userfaultfd stays ENOSYS
# (docs/starnix.md)
SKIP="$SKIP:All/AnonInodeTest.*/BpfMap:All/AnonInodeTest.*/Userfaultfd"
# kernel/mm: mlock never fails on inaccessible ranges; mlock population inflates RSS and does not
# break copy-on-write
SKIP="$SKIP:All/MlockProtTest.MlockApplied/None_*:MlockProcTest.MlockStopsPopulatingAtProtNoneMapping"
SKIP="$SKIP:MlockProcTest.MlockAfterForkCopiesPrivatePages"
SKIP="$SKIP:MlockProcTest.MlockPrefaultsOnlyWritablePrivateAnonymousMappings"
# io_uring is ENOSYS (docs/starnix.md), so this skips itself on Tuxport
SKIP="$SKIP:PersonalityTest.ReadImpliesExecMmapIoUring"
# kernel/mm: pagemap reports no frame numbers, not even to CAP_SYS_ADMIN (after that check fails,
# the fallback test crashes in gtest's own allocation)
SKIP="$SKIP:ProcTestBase.ProcSelfPagemapMappedAndUnmapped:ProcTestBase.ProcSelfPagemapMultiChunkBoundary"
SKIP="$SKIP:ProcTestBase.ProcSelfPagemapMultipleMappingsAndHoles"
SKIP="$SKIP:ProcTestBase.ProcSelfPagemapFallbackBoundary"
# Cannot be compared on CI: PER_LINUX32 needs AArch32 at EL0, which CI's CPU has and Tuxport and the
# development Macs do not; the mremap test skips itself on kernels 6.17 and newer
SKIP="$SKIP:PersonalityTest.UnameMachineName:Mremap.RemapMayMoveSpanningMappings"
sh /tmp/difftest-files/gtest/run.sh starnix "$SKIP" mmap_test aspace_test anon_test mlock_test \
    memfd_test membarrier_test procfs_mem_test personality_test process_mrelease_test
