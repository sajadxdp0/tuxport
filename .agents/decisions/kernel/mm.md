# Guest memory (kernel/mm) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/mm"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b). D1–D17 were seeded by hand from the retired design
> doc §5 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Constraint index (compressed active decisions)

> Full text of D1, D2, D4, D6–D9, D11–D17, D19–D21, D23–D26: `git show 248d79f637c8f05dadc6ef9fdb0733c568c6a468:.claude/decisions/kernel/mm.md`

- **D1** — Guest memory uses a software radix page table per address space and a software TLB per thread, never emulated ARM translation hardware, and the TLB layout is a contract with the Tier 1 translator, which inlines the check (design review 2026-10-24, round 23)
- **D2** — Unmapped host memory is released only after the other threads are quiesced (the graveyard) and then after the BKL drops (a thread-local list), so any new path freeing host memory under the BKL must use the same deferral, and the OOM killer skips a round while releases are pending (rounds 04, 29, 36, 109)
- **D4** — New `PROT_NONE` mappings get no page-table entries, so mprotect away from `PROT_NONE` must rebuild them, and `MADV_DONTNEED` hands whole host pages to the host's madvise while zeroing partial ones on 16 KB hosts (round 12)
- **D6** — fork finds the pages the host touched through `/proc/self/pagemap`, never `mincore` (which reports zram-swapped pages as absent), and lends every page of mappings of at most 16 pages or when pagemap is unreadable (rounds 13, 26, 179)
- **D7** — VMAs are never merged and each keeps its own host memory, so `same_vma` treats adjacent pieces as one Linux VMA (D22) and must compare every flag shown in maps and smaps, and the guest's `max_map_count` is enforced per address space (rounds 17, 18; e03c0cc)
- **D8** — The first brk growth reserves 256 MB of `MAP_NORESERVE` host address space that later growth lengthens as one VMA and fork recreates, with growth beyond it falling back to separate mappings (round 17)
- **D9** — execve reserves host memory below the main stack down to `RLIMIT_STACK` (at most 8 MB), and the stack grows into it and then into chunks about as large again, but only at syscall entry, for signal frames and on failed accesses through the user-copy helpers and `/proc/<pid>/mem`, never through direct `Mm::read`/`write` (round 22; e03c0cc)
- **D11** — When the last borrower returns a lent page, the owner's entries get the VMA permission back at once instead of waiting for a write fault (round 26)
- **D12** — Read-only private mappings of files with a trusted content version share one kernel-wide read-only host mapping, so a VMA must copy before it becomes writable, and a missed copy faults instead of corrupting other processes' code (round 28)
- **D13** — Address spaces of the same executable with consistent executable layouts share one Tier 1 translation space, a diverging member leaves for good with a shootdown, and the last member to leave must not leave other members' translations behind (rounds 108, 181)
- **D14** — Private anonymous host ranges are discarded with `MADV_DONTNEED` and reused for the next request of the same size and accounting class (up to 32 ranges per size, 256 MB in all), and must be discarded before reuse so guests never see stale data (round 109)
- **D15** — fork carves the child's small mappings, heap and stack from a pool of pre-mapped host memory refilled after the BKL drops, a carved region is returned only when all its pieces are unmapped, and pool misses and mappings over 1 MB still mmap under the BKL (round 109)
- **D16** — Mappings over 1 MB (except executable ones) get page-table entries on first access in 2 MB leaf tables, so every private anonymous page with content must have an entry and any kernel path writing guest memory directly must fill it, since fork and RSS look only where entries exist (round 179)
- **D17** — A private anonymous mapping that has been `PROT_NONE` since creation holds no host memory until it becomes accessible or the kernel writes into it, and its fake host base must never be dereferenced (round 199)
- **D19** — `MADV_WIPEONFORK` and `MADV_DONTFORK` are VMA flags that fork honours (dontfork VMAs are skipped, in the commit check too, and wipeonfork ones are never lent, so the child gets zeroes), and every path that copies a VMA must carry them along with dontdump (e020977)
- **D20** — mm.rs, syscall/mem.rs and oom.rs read in the order their code runs (mm.rs in the sections Address space, Consistency and code, Copy-on-write, Introspection types and Host backing), so new code goes where it runs rather than at the end of `impl Mm` or of the file (80d41b5)
- **D21** — mremap with an old length of 0 duplicates shared mappings in 6.1's error order, mapping shared files again and cloning shared anonymous and SysV VMAs onto the same host memory, so a copy's `file_len` is capped at its host region's end and need not equal its length, and SysV copies count as attachments per kernel/ipc D2 (5dcf7db)
- **D23** — mlock, mlock2 and munlock validate and page-align their range in the syscall with `do_mlock`'s arithmetic before calling `Mm::set_locked` (09188ec)
- **D24** — `vm.overcommit_memory=1` makes fork skip `commit_too_big` (kernel/procfs D16), while mode 2's strict accounting is not emulated and behaves as 0 (42d2ffe)
- **D25** — `Mm::fork` copies the membarrier registrations, as Linux's `dup_mm` copies `membarrier_state` (cbf7267)
- **D26** — `resident_split` sorts whole VMAs into anonymous, file and shared pages for status's Rss lines (kernel/procfs D20), counting a written private file mapping as anonymous, and shares `shmem_like` with core dumps (566da95)

## Active (full detail)

### D3: Host commit accounting follows the guest's mapping arguments
- **What**: Host memory is requested as accounted or `MAP_NORESERVE` according to the guest's prot and flags under Linux's rules (shared anonymous `MAP_NORESERVE` memory included), not according to the always-read-write host mapping.
- **Why**: Guest permissions live only in the page table; accounting every mapping as writable private memory made V8's 8 GB `PROT_NONE` wasm reservations fail.
- **Tradeoffs**: The host and guest views of commit can differ (fork's spare pieces are host `MAP_NORESERVE` but counted by the guest check).
- **Watch out**: None known.
- **Source**: rounds 10 (`10-m8`), 179 (`179-demand-page-tables`), 199 (`199-reserved-mappings`); 5d23297

### D5: Truncation removes entries past the new end in every address space
- **What**: truncate, ftruncate and `O_TRUNC` remove page-table entries past the new EOF from every address space that maps the file and sync each that changed, the caller's included, before the host file shrinks (kernel/vfs D33).
- **Why**: A guest access past the new end used to raise a host `SIGBUS` that took down every session, and with the host file shortened first a concurrent access through a stale translation still did; the guest should get its own `SIGBUS` as on Linux.
- **Tradeoffs**: Truncation walks every address space, and the caller's own is quiesced during the syscall rather than at its exit.
- **Watch out**: If the host truncation fails after the entries are gone, the mappings keep the requested end of file and accesses past it raise `SIGBUS` until remapped.
- **Source**: round 12 (`12-stability-memory`); 400817d

### D10: Page-granular copy-on-write in the soft page tables
- **What**: fork lends written private anonymous pages: owner and borrowers map the owner's host page read-only, borrowers copy on write, and the owner recalls copies before writing.
- **Why**: fork copied every touched page while holding the BKL (21 ms versus 0.6 ms native at 100 MB) and doubled memory use.
- **Tradeoffs**: Per-page loan bookkeeping (made cheap with a multiplicative hash, merged traversal and no allocation for a single borrower), and every kernel write path must handle CoW pages.
- **Watch out**: `translate` does not break CoW, so kernel code writing through it must break CoW or fault the page in first, as the robust-futex death path now does.
- **Source**: round 26 (`26-fork-cow`); e4d3386

### D18: kernel/mm is guest memory above the soft-MMU; the soft-MMU and the OOM killer are modules of their own
- **What**: kernel/mm covers `mm.rs` (except the soft-MMU's consistency and entry-filling methods and the resident-memory measures), `syscall/mem.rs` (except process_mrelease), `uaccess.rs` (`UserSlice`) and the run loop's page-fault branch, while the page table, software TLB, shootdowns and code groups are kernel/mmu's and the measures, OOM killer and process_mrelease kernel/oom's, so the engine module stops at instruction execution.
- **Why**: The retired design doc's §5 was the only current-state description of guest memory and had drifted from the code, and by 2026-11-11 the map (69.6 KB) could not be read in one Read while the soft-MMU and the OOM killer sit below and above the VMA code most mm changes do not touch.
- **Tradeoffs**: Copy-on-write and FUSE mappings stay because they run through mprotect, mremap, madvise and fork; the fault order stays here though its `fault_in` step is kernel/mmu's; boundaries inside `mm.rs` exist only in the registry's wording; decisions made before the split keep their kernel/mm tags here (listed in the kernel/mmu and kernel/oom preambles), cgroup files belong to kernel/cgroup, procfs views to kernel/procfs and the timer worker to kernel/time; vDSO image construction belongs to kernel/vdso while memory mapping remains here.
- **Watch out**: Bugs still open in the map include ignored `MADV_DONTNEED_LOCKED` and `MADV_POPULATE_*`, past-EOF faults ignoring permissions, approximated Linux VMA boundaries, mlock succeeding on inaccessible ranges, ranges with gaps failing as a whole, stale addresses after mremap moves a SysV segment or the vDSO and FUSE short reads taken as whole windows, and a VMA change must still record what kernel/mmu's `sync` acts on; module documentation points to `.agents/maps/kernel/mm.md`, so future directory moves must update it.
- **Source**: cd53f35, 44a21b7 (scope since extended by 3f3db11 and d2b1dc3; bug list since updated by 3f3db11, 1459292, 4e64e74, 400817d, 5d23297, d9e1f0c and 5760479); 36083476; 20137e4c; cf029f9a; 8a17e4a9; d053be24; 60790c14; 39e55b4b

### D22: Linux VMA boundaries are computed over unmerged pieces
- **What**: `same_vma` decides which adjacent pieces form one Linux VMA (contents source, backing, protection, flags, grows-down and an `anon` identity that fork renews per parent VMA) for maps, mremap's one-VMA rule and `PROT_GROWSDOWN`; mprotect follows `do_mprotect_pkey`; a guard gap below grows-down VMAs and `MMAP_MIN_ADDR` (32768) bound placement; and failed kernel accesses grow stacks.
- **Why**: With VMAs never merged (D7), maps, mremap, `PROT_GROWSDOWN`, merging after fork, stack growth on kernel accesses, partial mprotect and mmap_min_addr all differed from Linux, as Starnix's mmap tests showed.
- **Tradeoffs**: fork reads pagemap once per written private file mapping's host region (hal D12), `anon_vma` is still approximated, and direct `Mm::read`/`write` callers do not grow stacks.
- **Watch out**: mremap now refuses ranges spanning differently flagged pieces with `EFAULT` (as Linux before 6.17), partial mprotect leaves earlier VMAs changed on error, and the fork pagemap read may show in fork-heavy benchmarks.
- **Source**: 1459292, e03c0cc

### D27: `MADV_DONTNEED` reverts written private file mappings to the file
- **What**: For written private file VMAs, `discard` gives the whole host pages of a host private file mapping back with the host's `MADV_DONTNEED` (`discard_file_host`, shared with the anonymous path) and returns `Reread`s for partial host pages (16 KB hosts) and VMAs copied into anonymous memory by `unshare`, which madvise reads from the store object (zeros past EOF or when it cannot be opened) and stores with `Mm::fill_private`, queueing executable ranges for invalidation.
- **Why**: `discard` only gave private anonymous memory back to the host, so a program resetting a private file mapping with `MADV_DONTNEED` kept its own changes where Linux reads the file again.
- **Tradeoffs**: Re-read pieces are private copies of the file at that moment, so later writes to the file do not show in them as they would on Linux.
- **Watch out**: The 16 KB partial-page path runs only on 16 KB devices and no differential test covers it, and an inode reused after its object was reclaimed would be read in place of the old file.
- **Source**: 8127cfe

### D28: `Mm::mapped_above` answers find_vma's question for forced SIGSEGVs
- **What**: `Mm::mapped_above` is true when the last VMA ends above an address, which decides the code of the SIGSEGV forced when a signal frame cannot be used (kernel/signal D18).
- **Why**: That code depends on whether Linux's find_vma finds a mapping ending above the address, and `is_mapped` is wrong for addresses in a gap below a mapping.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 41bd6ec

### D29: The timer thread's checks stop only through `stop_checks`
- **What**: The OOM check, the code-view release and the load-average sampler stop through `Shared::stop_checks`, which sets `checks_idle` and then looks at the threads again, with every `sleeping` flag and both sides' `checks_idle` accesses SeqCst so a thread waking from a fast-path sleep either sees the flag or is seen running.
- **Why**: Reading "every task sleeping" and setting `checks_idle` in two steps let a waking thread miss the store, so the checks stayed off until the next code-view round (up to a second).
- **Tradeoffs**: Every sleep and wake now does SeqCst stores on its flag.
- **Watch out**: None known.
- **Source**: 5d23297

### D30: File mapping identities use `st_dev` and `st_ino` as they are
- **What**: A file mapping's identity takes the host's `st_dev` and `st_ino` without casts, since both are `u64` on the aarch64 Linux and Android hosts that remain (hal D1).
- **Why**: The casts existed only because `st_dev` is `i32` on macOS, which is no longer a host.
- **Tradeoffs**: A 32-bit target or another libc would need the casts back.
- **Watch out**: Only a compile error on such a target.
- **Source**: 7c5e636

### D31: Private futexes are keyed by address space and address
- **What**: `Mm::id` (never reused) names the address space for private futex keys, which need only a user address and no mapping, and `shared_maps` (renamed from `shared_files`) lists every writable shared mapping, file, anonymous or SysV, recounted when one is created (mmap, shmat), moved or forked, while `futex_key` no longer copies a borrowed page first (kernel/process D32).
- **Why**: Keys of private memory moved from host addresses to the address space and address, so a waiter on a page replaced by `MAP_FIXED` is found, and gVisor's WakeOp tests pass NULL to private futexes.
- **Tradeoffs**: Futexes in shared anonymous memory (process-shared mutexes) now take the BKL path.
- **Watch out**: Stale ranges only slow the fast path, but a shared mapping not yet noted (the window inside mmap and shmat) could be keyed as private by a concurrent fast-path futex.
- **Source**: f6a82cc, 5d7b936

### D32: The OOM killer skips stuck victims and scores as `oom_badness`
- **What**: `Oom::stuck` keeps every victim that outlived its second while it has threads and `select` skips them, and `badness` scores are unclamped with the adjustment times total/1000, equal scores going to the later process and a vfork child that shares its parent thread's address space being skipped (Linux's `in_vfork`), while `proc_score` scales the score against the host's RAM plus swap for `oom_score` (kernel/procfs D30).
- **Why**: A stuck victim was skipped only while it stayed the recorded victim, so picked again later it was only waited for and the killer stalled a second without killing, scores differed from 6.1's `oom_badness`, a vfork child won the choice over its parent with the same memory and a higher tgid, and the guest could not see the score.
- **Tradeoffs**: Scores still omit swap and page tables.
- **Watch out**: A stuck victim stays listed until its last thread ends, and anything creating vfork children must set `Task::vfork` through `vfork_done`, which `release_mm` must keep clearing.
- **Source**: b43ff37, 39ea702

### D33: `MADV_REMOVE` frees storage through the host, and discarding keeps the vDSO
- **What**: `MADV_REMOVE` uses the host's `MADV_REMOVE` for whole host pages and zeroes the edges or what the host refuses, and the vDSO is refilled from its image after a discard.
- **Why**: `MADV_REMOVE` zeroed instead of freeing storage, and `MADV_DONTNEED` zeroed the vDSO, so the next clock call raised `SIGILL`.
- **Tradeoffs**: Breakpoints a debugger wrote into the vDSO are lost by `MADV_DONTNEED`, as on Linux.
- **Watch out**: tmpfs accounting does not see `MADV_REMOVE` until a recount.
- **Source**: 5d7b936

### D34: `Mm::can_write` checks a buffer before a kernel write, and unmapping reaps destroyed segments
- **What**: `Mm::can_write` tells whether a range lies in writable mappings, for a kernel write that must not fail halfway (AF_UNIX recvmsg, before it takes queued data), and munmap, mremap and `MAP_FIXED` call `reap_shm` (kernel/ipc D7).
- **Why**: recvmsg needed a writability check so that a bad buffer fails with `EFAULT` before data is consumed, and shm segments marked for destruction outlived their last mapping when it went away through these calls.
- **Tradeoffs**: `can_write` looks only at VMA permissions, not at mapped pages or stack growth.
- **Watch out**: None known.
- **Source**: 5d7b936

### D35: process_mrelease releases a dying process's memory; process_madvise stays `ENOSYS`
- **What**: `Mm::release` re-points borrowed pages at the VMA's own page without copying, leaves lent pages, discards the other written private anonymous pages and the written pages of private file mappings, and leaves VMAs, executable mappings and reservations alone; process_mrelease calls it after 6.1's flag and pidfd checks once every process sharing the address space is exiting without a core dump, while process_madvise stays `ENOSYS`.
- **Why**: While process_mrelease was `ENOSYS`, earlyoom warned after each kill and Starnix's test failed, whereas process_madvise carries only hints Tuxport ignores or refuses, has no Linux userspace caller, and is `ENOSYS` in Starnix and `EPERM` under docker's default seccomp.
- **Tradeoffs**: "Exiting" is `ThreadGroup::exiting`, so a release right after a fatal signal other than SIGKILL is `EINVAL` until a thread takes it; executable private memory is never released, and the OOM killer does not release its own victims' memory.
- **Watch out**: A dying thread still running guest code reads zeros where Linux would fault, and the release must not reuse `discard`, which makes borrowers copy lent pages and settles borrowed ones by copying.
- **Source**: b141001, 39ea702, 983fc3d

### D36: Refused madvise advice is counted as unimplemented
- **What**: madvise counts every advice value its up-front check refuses (KSM, transparent huge pages, hardware poisoning) among the unimplemented paths (kernel/signal D24).
- **Why**: Advice refused up front went unrecorded.
- **Tradeoffs**: Invalid advice from tests is counted too.
- **Watch out**: None known.
- **Source**: 99b4216

### D37: Shared mappings follow a file to its own store object
- **What**: When a file sharing its store object (kernel/store D17) gets one of its own, `file_moving` removes its shared mappings' entries past the new object's length in every address space as truncation does, the callers sync, and `file_moved` maps the new object over each host region with `MAP_FIXED` (hal D17) and points the file id and descriptor at it, picking mappings by store inode and old host file, while `maps_file` answers the clone checks.
- **Why**: Shared mappings made through read-only descriptions kept showing the shared object after the file got its own, silently (LMDB's `MDB_RDONLY` readers, read-only SQLite with mmap).
- **Tradeoffs**: Private mappings are not moved, and a host region shared across address spaces is re-mapped once for each that still points at the old file.
- **Watch out**: Clipping must finish in every address space before any region is re-mapped, so when one is busy all mappings stay on the old object, and a Files app write that unshares a file does not move the running kernel's shared mappings (kernel/store D17).
- **Source**: df6ee70

### D38: The host's vvar pages are mapped as a foreign Shared VMA
- **What**: `Mm::map_vvar` maps the host's live vvar pages below the vDSO as a read-only Shared VMA on a `HostRegion::foreign` (never unmapped or recycled) whose `file_len` is the readable part, named `[vvar]` and shown private, left out of RSS, smaps and core files, and refusing writes, execution, growth (`EFAULT`) and duplication (`EINVAL`), while `Mm::vdso_syscalls` writes svc stubs over the host vDSO's clock entries once a process using the address space enters a time namespace with offsets, and a discarded vDSO comes back with them (kernel/signal D4, hal D18).
- **Why**: The host's vDSO code addresses its data at a fixed distance below itself, so each address space needs the host's live vvar pages there, which Tuxport does not own.
- **Tradeoffs**: ptrace and process_vm_readv can read the vvar data where Linux refuses `VM_PFNMAP` pages, core files carry no `[vvar]` contents, and madvise on `[vvar]` succeeds where Linux refuses.
- **Watch out**: Any new path that writes, unmaps or discards a Shared VMA's host memory must skip foreign regions.
- **Source**: 16364c0

### D39: Shootdowns kick users out of guest code but do not hold them back
- **What**: A shootdown's wait loop kicks each user it still finds in guest code and yields, and since only quiesce keeps threads out of guest code afterwards, mprotect orders its updates per VMA without a shootdown first: entries lose write, copy-on-write pages get read-only entries at the right host pages, and the other pages then regain write in runs around them.
- **Why**: A user entering guest code after the kicks was never kicked, so a shootdown spinning on it while it waited for the BKL stopped the whole kernel (LTP shmctl05), and mprotect assumed its shootdown kept threads out, so an unkicked thread could write a lent or borrowed page in place and the other process saw the write.
- **Tradeoffs**: A late entrant takes an extra engine exit, and a VMA holding copy-on-write pages is walked twice with its other pages briefly lacking write permission, so a thread writing one faults and waits for the BKL.
- **Watch out**: A user entering after the wait loop has passed it runs on with a flushed TLB, and any other page-table change under the BKL that relies on threads staying out of guest code after a shootdown has the hole mprotect had.
- **Source**: f51f688, 04163e7

### D40: FUSE mappings read pages on demand into a per-mmap memfd
- **What**: A FUSE file mapping gets a `Fill` at mmap, shared by all VMAs from that mmap, that holds a sparse memfd as long as the file (hal D19), the open file, the runs of pages read and read-ahead state; its VMAs keep `file_len` 0 and get entries only for pages read, guest faults read a window (32 pages, doubling up to 512 for sequential access) with the BKL released, kernel copies fail with `FUSE_RETRY` so the dispatcher reads the page and reruns the syscall (kernel/fuse D2), fork, unshare and mremap map the memfd again, `MADV_DONTNEED` returns pages to what was read, and shared writable mappings are refused with `EACCES`.
- **Why**: FUSE files have no host object to map, and copying the whole range at mmap read a whole 1 GB file to touch a few pages and hid the file behind a store inode number that mremap and `MADV_DONTNEED` used to open unrelated store objects.
- **Tradeoffs**: There is no page cache, so a page keeps what was read when first touched, separate mmap calls read separately, the size is fixed at mmap, a sequential scan costs one round trip per window, and mlock, `MADV_WILLNEED` and core dumps do not read pages.
- **Watch out**: A syscall other than read, write and their vector and positional forms that touches an unread page after side effects repeats them on the retry or returns a short count, a new path sizing entries by contents rather than `file_len` would map unread pages, and a short read is taken as a whole window (a map bug).
- **Source**: c008749, 3957a03

### D41: The address space records the main program's code and data bounds
- **What**: `Mm::code` and `Mm::data` hold start_code, end_code, start_data and end_data, set by the loader from the main program's segments as 6.1's `load_elf_binary` computes them and copied at fork, with `brk_start` and `start_stack` getters, for `/proc/<pid>/stat` (kernel/procfs D34).
- **Why**: `/proc/<pid>/stat` needs Linux's start_code, end_code, start_data and end_data, which nothing recorded.
- **Tradeoffs**: Two more fields per address space.
- **Watch out**: None known.
- **Source**: 6d9a9e8

### D42: mremap checks room for a move before changing anything
- **What**: The `MREMAP_MAYMOVE`, `MREMAP_FIXED` and `MREMAP_DONTUNMAP` paths check `move_room` (6.1's `move_vma` margin plus two for `mremap_to`'s unmapping) before any change, `place_vma` re-inserts moved pieces without the map-count check, mremap and `MREMAP_DONTUNMAP` share `check_target`, and `MREMAP_DONTUNMAP` checks in 6.1's order before splitting and then reserves room for the splits and one VMA per piece left at the old address.
- **Why**: A move near `vm.max_map_count` failed halfway with the VMAs gone, stale entries at the destination and `ENOMEM` returned, and `MREMAP_DONTUNMAP` kept the splits it made before refusing a bad target.
- **Tradeoffs**: VMAs are never merged (D7), so the margin counts unmerged pieces and a move can be refused a little earlier than on Linux, and growing in place adds a VMA where Linux extends one.
- **Watch out**: A host page-table allocation failure inside `move_entries` still leaves a move half done, and a new caller of `move_range` must check `move_room` first.
- **Source**: 004f5be

### D43: `READ_IMPLIES_EXEC` adds `PROT_EXEC` to readable mappings
- **What**: Under the personality flag, mmap adds `PROT_EXEC` to a readable request unless it maps a file on a noexec mount, `Mm::protect` adds it per VMA that may execute (changing mixed ranges piece by piece), and `Mm::brk` makes the heap readable, writable and executable, a heap part with other permissions becoming a VMA of its own.
- **Why**: The flag was only recorded, so mmap and mprotect never added `PROT_EXEC` as Linux 6.1 does, as Starnix's personality tests showed.
- **Tradeoffs**: Every execve clears the flag, so it affects only code that sets it in its own process, and mprotect now calls `set_protection` once per run of equal permissions.
- **Watch out**: Another path creating readable mappings for the process (none known) would not apply the flag.
- **Source**: 5760479

### D44: Cgroup memory usage counts each shared host page once
- **What**: `mm::Usage` collects a cgroup member's pages through `Mm::plan` (shared with `rss_plan`), listing borrowed pages as runs of the owner's host pages and skipping file pieces already counted, and `Usage::pages` merges overlapping host ranges and measures them in one mincore pass, for a child cgroup's `memory.current` (kernel/procfs D5).
- **Why**: Linux charges each page to one cgroup, while a child cgroup's usage was the sum of its processes' `resident_pages`, counting every shared page once per process.
- **Tradeoffs**: A borrowed page counts only while the owner's page is resident, overlapping but different pieces of one file count separately, and a page lent to a process in another cgroup counts in the borrower's cgroup too, where Linux charges the owner's.
- **Watch out**: The OOM check now sorts every member's spans each round, bounded by its 20-times-the-measurement interval, and a new kind of shared host memory with distinct host addresses per process would count once per process again.
- **Source**: 1d3d559

## Superseded

- ~~Adjacent VMAs joined only for display in maps and in fork's commit estimate (D7 in part, rounds 17, 18)~~ → replaced by **D22** in e03c0cc (2026-11-07)
- ~~The kernel grows the stack only at syscall entry and for signal frames (D9 in part, round 22)~~ → replaced by **D22** in e03c0cc (2026-11-07)
- ~~process_mrelease stays `ENOSYS` until earlyoom and oom_score make it worth doing (b141001 in part)~~ → replaced by **D35** in 39ea702 (2026-11-08)
- ~~FUSE mappings copy the whole range into a memfd at mmap (c008749)~~ → replaced by **D40** in 3957a03 (2026-11-09)
- ~~Guest memory, with the page table, software TLB and OOM killer, is one kernel/mm module (D18, cd53f35)~~ → replaced by **D18** in 44a21b7 (2026-11-11)
