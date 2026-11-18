# kernel/mm Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/mm.md` for the paired decision history (it still holds the decisions on the soft-MMU and the OOM killer made before they were split out into `kernel/mmu` and `kernel/oom`).
>
> Verified: 2026-11-10 (full mapping: 6 research concerns; 18 claims checked by 3 independent verifiers — 13 confirmed and 5 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: repoint dependencies to the split module maps)

## Responsibilities

`kernel/mm` is the guest memory subsystem: it implements Linux mm semantics for every guest
process on top of `kernel/mmu`'s soft-MMU:

- one address space (`Mm`) per guest process: a VMA table plus its software page table;
- host memory behind every VMA, including sharing, recycling and deferred release;
- the memory syscalls, brk, stack growth and the guest page-fault policy;
- `fork` with page-granular copy-on-write implemented only in the soft page tables;
- file mappings at EOF and truncation, files moving to objects of their own, and FUSE mappings
  read on demand;
- the VMA views for `/proc/<pid>/maps` and core dumps.

Root:

- `core/kernel/src/mm.rs` — address space, host backing, CoW, unit tests (except what
  `kernel/mmu` and `kernel/oom` list there: entry population, consistency and code groups;
  resident-memory measures and `Mm::release`).
- `core/kernel/src/syscall/mem.rs` — argument validation for mmap, mremap, remap_file_pages,
  msync, munmap, mprotect, madvise, mlock/mlock2/munlock, mlockall/munlockall and mincore.
- The page-fault branch of the task run loop in `core/kernel/src/task.rs`, and the stack-growth
  helpers on `Cx` in `core/kernel/src/kernel.rs` (with `UserSlice` in `uaccess.rs`).

Outside the module (dependencies only): the page table, TLB, entry population, shootdowns and code
groups (`kernel/mmu`); resident-memory measures, the OOM killer and process_mrelease (`kernel/oom`);
the CPU engine and the Tier 1 translator (`engine`), SysV IPC objects (`kernel/ipc`), futexes
(`kernel/futex`), the ELF loader (`kernel/exec`), the timer thread (`kernel/time`), procfs
(`kernel/procfs`) and the cgroup files (`kernel/cgroup`), core dumps (`kernel/signal`) and the vDSO image (`kernel/vdso`), FUSE reads
(`kernel/fuse`) and the host abstraction layer (`core/hal`).

## Key types

### Address space

- `Mm` — one guest address space. Holds the page table (`mmu`, dropped first), the VMA table
  (`vmas: BTreeMap<start, Vma>`), the `graveyard` of host regions awaiting shootdown, brk and
  layout fields (`brk_start`, `brk`, `mmap_top`, `start_stack`, `stack_low`), its `id` (never
  reused), its users, the consistency counters, CoW state, the code-group membership, the shared
  writable ranges (`shared_maps`), the kernel-wide `FileMaps`, whether it ever had a `Fill`, and
  fields consumed elsewhere (`exe`, `dbt_regs`, `vdso`, `vdso_svc`, `code` and `data` (the
  loader's bounds, for `/proc/<pid>/stat`), `args`, `env`, `auxv`, `membarrier`, `lock_future`,
  `lock_onfault`). Built and loaded at execve as a plain value with the BKL released, then shared
  as `Rc<RefCell<Mm>>` and used only under the BKL.
- `Vma` — one mapping: length, guest prot, `Backing`, `may_write`/`may_exec` (Linux
  `VM_MAYWRITE`/`VM_MAYEXEC`), `Arc<HostRegion>` plus host pointer, `name`/`ino` for maps,
  `file_len` (bytes before EOF), `file_id`/`content` (file identity and version), a dup'd fd for
  shared file mappings, `shm` (segment and attach base), and flags `cow`, `locked`, `written`
  (sticky: it has been writable or the kernel wrote into it), `growsdown`, `dontdump`,
  `wipeonfork`, `dontfork`, `noreserve`, `anon` (the identity of the anon_vma it would have on
  Linux, 0 until a fork), and `fill` for a mapping read on demand.
- `Fill` (`Rc`) — a FUSE file mapping read on demand: a sparse host file (memfd) as long as the
  file was when mapped, holding what was read at the file's own offsets; the open file to read
  from (kept open while mapped); the runs of file pages read; and the read-ahead state. Every VMA
  made from one mmap call (split, forked, moved, grown) holds the same one.
- `same_vma` — whether two adjacent VMAs are one VMA on Linux: same contents source (both
  file-less and not shared, or the same store inode at contiguous offsets; shared anonymous
  never), backing, prot, lock, may_write, growsdown, dump and fork flags, `anon` and name (not
  `may_exec`, `noreserve` or the fill).
- `Backing` — `Anonymous` (private anonymous, private `/dev/zero`), `PrivateFile` (private file
  mapping), `Shared` (shared file, shared anonymous, shared `/dev/zero` or SysV shm; also
  `[vvar]`, whose host memory is shared with forked children).
- `flags` — guest ABI values of the `MAP_*` flags the mm interprets.
- `Refill` — the private-file piece re-mapped at the old address after `MREMAP_DONTUNMAP`;
  `Reread` — a written private-file piece the syscall reads back after `MADV_DONTNEED`.
- `WANTED` (thread-local) — the page of a `Fill` a kernel access found not read, for the syscall
  dispatcher (`take_wanted`).
- `UserSlice` — an unchecked guest range passed to kernel-side accessors.

### Host backing

- `HostRegion` — owns one host range (`base`, `len`). `anonymous: Some(reserve)` marks a range that
  is recycled on drop. `reserved` marks a placeholder with no host memory, whose base is the fake
  address `RESERVED_HOST` and is never dereferenced. `foreign` marks host memory the kernel layer
  does not own and only reads (the host's vvar pages behind `[vvar]`): never unmapped or recycled.
- All anonymous guest memory, private or shared (shared anonymous mappings, SysV shm), is private
  anonymous host memory: guest processes sharing it hold the same `HostRegion` inside the one app
  process.
- `recycled` — process-wide reuse pool of anonymous host ranges keyed by `(len, reserve)`: at most
  256 MB in total and 32 ranges per key, a range kept only when the host discard succeeds.
- `spare` — pre-mapped `MAP_NORESERVE` host pieces that `fork` takes while holding the BKL: two
  per length, at most eight lengths (least recently wanted evicted), falling back to `recycled`
  and then a host mmap.
- `Carved` — small child mappings carved from one spare region at fork.
- `Unmapped` / `UNMAPPED` / `UNMAPPING` — thread-local list of host ranges and page tables whose
  release is deferred while the BKL is held, and a global byte count of what is still pending (page
  tables not counted).
- `FileMaps` — kernel-wide LRU cache (at most 128 entries) of read-only private file mappings,
  shared by all address spaces, keyed by device, inode, offset, length, ctime and size.
- `SharedMemory` — host backing of a SysV shm segment (always commit-accounted).

### Copy-on-write

- `Loan` — one lent host page: its owner (`None` for an orphan) and its borrowers (their count is
  the page's map count for smaps).
- `Loans` — `HashMap<host page address, Loan>` shared by a whole fork family (execve starts a fresh
  one), hashed with a multiplicative `PageHasher`.
- `Cow` — per-mm sets of guest pages it has `lent` and has `borrowed`; `Borrowed` keeps the owner's
  `HostRegion` alive.
- `Runs` — coalesces consecutive pages into runs for PTE updates; a run never crosses a VMA.
- `MmRef` — weak self-handle (`me`) recorded in loans.

### Introspection

- `Region` — one `/proc/<pid>/maps` / `smaps` line.
- `DumpRegion`, `dump_filter` — core-dump segments and the `coredump_filter` bits (default `0x33`).

## Public entry points

- **Syscalls** (`syscall/mem.rs` → `Mm`): `mmap`, `mmap_fill` (a FUSE file), `munmap`, `protect`
  (mprotect), `mremap` (including `MREMAP_DONTUNMAP` via `mremap_dontunmap`), `remap_file_pages`
  (emulated as a shared fixed mmap), `msync`, `madvise`, `mlock`/`mlock2`/`munlock`,
  `mlockall`/`munlockall`, `mincore`. `brk` calls `Mm::brk` directly. `process_madvise` is
  `ENOSYS`; process_mrelease's `release` is `kernel/oom`'s.
- **execve / loader** (`kernel/exec`): `Mm::new`, `share_files`, `into_shared`, `set_layout`, `mmap`
  and `munmap` (segments), `map_anonymous`, `map_vvar`, `find_free` (interpreter and vDSO
  placement), `set_brk`, `map_stack`, `set_start_stack`, `label`, `write_kernel` (segments, `.bss`),
  plus the public layout fields.
- **Time namespaces** (clone, execve, setns, `kernel/ns`): `vdso_syscalls`.
- **Fault path** (task run loop): `cow_break`, `fill_wanted`, `fault_past_eof`, `Cx::grow_stack`
  (→ `grow_down`), `is_mapped`; `mmu().translate` and `fault_in` are `kernel/mmu`'s.
- **Mappings read on demand** (`kernel/fuse`, the dispatcher): `Fill::new`, `window`, `store`,
  `fd`, `take_wanted`, `has_fills`, `first_unread`.
- **Stack growth from the kernel**: `Cx::grow_stack_to` at syscall entry; `Cx::grow_stack` before
  building a signal frame; `grow_for` through `Cx::read_user`, `Cx::write_user`,
  `Cx::write_user_partial` and `/proc/<pid>/mem`.
- **Kernel access to guest memory**: `read`, `write` (breaks CoW per page), `translate` (does not
  break CoW), `read_cstr`, `write_kernel`, `write_force` (ptrace and `/proc/<pid>/mem` writes),
  `can_write` (whether a range lies in writable mappings, by VMA permissions only, for a write that
  must not fail halfway), `populate`, `mapped_above` (sigreturn's forced `SIGSEGV`);
  `check_mapped` (a coverage check, `ENOMEM`, for msync, madvise, mlock and mincore).
- **Threads and processes**: `fork`, `futex_key`, `shared_maps`, `id` (users, `sync`, `quiesce`
  and code spaces are `kernel/mmu`'s).
- **Files**: `file_truncated` (truncate, ftruncate, `O_TRUNC`), `writably_maps` (`F_SEAL_WRITE`,
  and a clone's source), `maps_file` (a clone's destination, a deleted shared file's release),
  `file_moving` and `file_moved` (a store file's data moving to another host object),
  `shared_file_ranges` (msync), `content_version`.
- **SysV shm**: `SharedMemory::new`, `attach_shm`, `detach_shm`, `shm_attachments`.
- **Introspection**: `regions`, `residency`, `size`, `locked_bytes`, `locked_in`, `dump_regions`,
  `dump_page` (the resident-memory measures are `kernel/oom`'s).
- **BKL hooks**: `defer_unmaps` when the BKL is taken; `release_unmapped`, then `spare::refill`
  when it is released.
- **Machine**: `FileMaps::clear` from `Machine::trim_memory`.

## Data flow / lifecycle

### Creation and layout

- execve builds a fresh `Mm` (with a fresh `Loans` table) that shares the kernel-wide `FileMaps`;
  the loader fills it with the BKL released, and after the point of no return it is shared and
  installed.
- `set_layout` sets `mmap_top` from the `RLIMIT_STACK` at execve (capped at 8 MB for a secure
  exec): `STACK_TOP` minus the limit plus the stack guard gap, clamped between 1 GB + 8 MB and 5/6
  of the address space. An unlimited stack still gives a top-down layout with the largest gap.
- `STACK_TOP` is `1 << 47`. A position-independent main program loads at `0x5555_5555_0000`.
  Non-fixed `mmap` allocates top-down from `mmap_top` (`find_free`), skipping the guard gap
  (`stack_guard_gap`, 256 pages) below every grows-down VMA, with a floor of `vm.mmap_min_addr`
  (`MMAP_MIN_ADDR`, 32768 as on distribution and GKI arm64 kernels; at least one page). The sysctl
  is global and writable with `CAP_SYS_RAWIO`.
- `map_stack` maps the arguments and auxv plus 128 KB (capped at the page-aligned limit, never
  below what the arguments use) at the top of one `MAP_NORESERVE` host region of
  `max(min(RLIMIT_STACK, 8 MB), mapped length)`, leaving the rest below as reserved growth room.

### vDSO and `[vvar]`

- The loader maps the vDSO image (`kernel/vdso`) as private anonymous memory written by the
  kernel. When the image is the host's vDSO, `map_vvar` first maps `[vvar]` right below it, as
  long as the host's vvar mappings and at the same distance, since the host's code addresses its
  data relative to itself: a `Shared` VMA, readable only (`may_write` and `may_exec` false, so
  mprotect asking for more is `EACCES`), on a `foreign` `HostRegion` at the host's vvar address.
  Only its first `data` bytes (the clock data the host's code reads) count as contents and get
  entries; the rest behaves like a file mapping past EOF (`SIGBUS`), as the time-namespace page
  does on Linux outside a time namespace.
- `[vvar]` is shared with forked children like any shared mapping, cannot be written by ptrace
  (`write_force` refuses shared mappings without write permission), cannot grow (`EFAULT`, checked
  before looking for room, Linux's `VM_DONTEXPAND`) or be duplicated by mremap (`EINVAL`), counts
  in no resident-memory measure, writes nothing to core files, and shows as private `r--p` in maps.
- `vdso_svc` says whether the vDSO's clock functions are svc stubs: set at execve for the built
  image, and by `vdso_syscalls` once a process using the address space enters a time namespace
  with offsets, which writes the stubs over the host image's clock entries (`write_kernel`) and
  queues the vDSO for code invalidation. It is copied at fork and never cleared.

### mmap and host backing

- The syscall checks, in order: unknown prot bits (`PROT_BTI` and `PROT_MTE` included, `EINVAL`),
  an unaligned offset (`EINVAL`), the map type (`MAP_SHARED_VALIDATE` with unknown flags
  `EOPNOTSUPP`), `MAP_SHARED | MAP_ANONYMOUS | MAP_GROWSDOWN` (`EINVAL`), a fixed address below
  `vm.mmap_min_addr` without `CAP_SYS_RAWIO` (`EPERM`; for an anonymous mapping only with a
  non-zero length; for a file after its `EBADF` and zero-length `EINVAL`), then the file's own
  rules (a shared mapping of a read-write descriptor first gives the store file an object of its
  own; a FUSE file gets its `Fill`; `EACCES`/`EPERM` by file kind), and `MAP_LOCKED` over the
  locked-memory limit (`EAGAIN`). After the mapping: `reap_shm` for `MAP_FIXED`, grows-down for
  private anonymous memory, the label, write and exec denials, locking, and population
  (`MAP_POPULATE`, `MAP_LOCKED`; a FUSE mapping's pages are read then).
- Lengths round up to a page; zero or overflow is `EINVAL`.
- With personality(2)'s `READ_IMPLIES_EXEC` (which every execve clears, as arm64's
  `SET_PERSONALITY` does), a readable mmap is made executable unless it maps a file on a noexec
  mount (do_mmap), mprotect adds execution to each VMA that may execute (`may_exec`, as
  do_mprotect_pkey does per VMA, so one call can leave mixed permissions), and brk's heap is
  executable (arm64's `VM_DATA_DEFAULT_FLAGS`); a heap grown with other permissions than its last
  part becomes a VMA of its own.
- `MAP_FIXED` / `MAP_FIXED_NOREPLACE`: the address must be aligned and the end at most `1 << 48`
  (`EINVAL`, after the syscall's `EPERM`). `NOREPLACE` on an occupied range is `EEXIST`; otherwise
  the old range is unmapped first, before host allocation, the permission check, the fd copy and
  the map-count check, so a later failure leaves a hole.
- A hint is rounded down to a page and raised to `vm.mmap_min_addr` (Linux's `round_hint_to_min`),
  and used if it ends at or below `mmap_top`, is free, and stays clear of the guard gap below a
  grows-down VMA above it.
- Host backing by kind:
  - private anonymous `PROT_NONE` (not grows-down) → a `reserved` `HostRegion` (no host address
    space);
  - other private anonymous, shared anonymous and SysV shm → the `recycled` pool, else a new host
    private anonymous mapping;
  - read-only private file with a trusted content version → a `FileMaps` hit (VMA marked `cow`),
    or a new read-only host mapping inserted into the cache;
  - other file mappings → host `mmap` of the store file's host object (read-write for private
    mappings);
  - a FUSE file → host `mmap` of its `Fill`'s memfd, empty at first (`mmap_fill`): no content
    version, no `FileMaps`, `file_len` 0; never writable when shared (`EACCES`).
- A file has no trusted content version while its ctime is within the last 2 s, or once it has been
  mapped shared from a writably opened descriptor (`WRITABLY_SHARED`, never cleared, recorded even
  when the mmap then fails; sealed memfds count as not writable).
- Shared file mappings map the same host object file with host `MAP_SHARED`, so they stay
  coherent with `read`/`write`, which go to that file through the host page cache.
- On a 16 KB-page host, a file mapping covers the 16 KB-aligned host range around the target and
  points the VMA at the right 4 KB offset inside it.
- Host commit accounting: anonymous memory is requested as accounted only when the guest mapping
  is writable and not `MAP_NORESERVE` (Linux's `shmem_acct_size` also skips `VM_NORESERVE` for
  shared anonymous memory); SysV shm is always accounted. The main stack, stack chunks, the heap
  reservation and fork's spare and carved pieces are always host `MAP_NORESERVE`.
- `insert` enforces `vm.max_map_count` (default 65530, writable through procfs; a new VMA must keep
  the count at or below it), builds page-table entries (below), and sets `locked` from
  `mlockall(MCL_FUTURE)`.
- `brk`: below `brk_start` returns the current break; growth needs the range free; it grows in
  place inside the heap's host region when the last part is anonymous, unnamed, with the heap's
  permissions and lock state and no other VMA on that host range (discarding the host range
  first), else at the first heap page reserves 256 MB (`MAP_NORESERVE`) for growth of at most that,
  else falls back to a separate accounted anonymous mapping. Shrinking unmaps. brk ignores
  `mmap_top`, the guard gap and `vm.mmap_min_addr`, and there is no `RLIMIT_DATA` or `RLIMIT_AS`
  enforcement.

### Mappings read on demand (`Fill`)

- Page-table entries are built eagerly for small and executable mappings and otherwise on first
  access (`kernel/mmu`); `PROT_NONE` mappings get none. A `Fill` VMA is the exception below.
- A `Fill` VMA keeps `file_len` 0, so `map_pages` and every other path sizing by `file_len`
  (mprotect, mremap, fork, populate, truncation) build none of its entries, even when executable;
  only `fault_in` does, for a page already read, and then for the runs read within its 2 MB leaf
  range. Its EOF is the size the file had when mapped (`contents_len`).
- Reading a page (`kernel/fuse`'s `fill`): `Fill::window` picks the pages not read around it
  (normally 32 starting 16 before it; from it, twice the last read when it continues that read or
  twice the run read just before it, between 32 and 512), clipped to the runs already read and
  EOF; the daemon is asked with the BKL released, and `store` writes the pages still missing into
  the memfd with `pwrite` and records the window as read. A page not read has no private copy
  anywhere (no entry, no kernel write), so the host file reaches every mapping of it, shared or
  private.
- A kernel access (`read`, `write`, `read_cstr`, `write_kernel`) meeting a page of a `Fill` mapping
  not yet read fails with the internal `FUSE_RETRY` and records the page in `WANTED`; the syscall
  dispatcher reads it and runs the syscall again (`kernel/process`). `read` and `write` go page by
  page, so a copy can stop partway; `write_kernel` checks the whole range first (any permission,
  `PROT_NONE` fills included), before changing anything, and gives `PROT_NONE` anonymous memory it
  writes prot-0 entries.

### Guest page faults (task run loop, under the BKL, in this order)

1. A write fault where `cow_break` succeeds → re-execute.
2. The page now translates (another thread fixed it while this one waited for the BKL) → re-execute.
3. `fault_in` (`kernel/mmu`) succeeds → re-execute.
4. A page of a `Fill` mapping not yet read and allowing the access (`fill_wanted`) → read it through
   the daemon (above) and re-execute; a signal arriving before the daemon took the read is
   delivered first and the access faults again; a read error → `SIGBUS` / `BUS_ADRERR`.
5. `fault_past_eof`: past EOF in a shared file mapping whose file has grown → map the new part and
   re-execute (not a `Fill` mapping, which keeps its size); otherwise past EOF → `SIGBUS` /
   `BUS_ADRERR`.
6. Stack growth (`grow_down`) succeeds → re-execute.
7. Otherwise `SIGSEGV`: `SEGV_ACCERR` if the address is mapped, else `SEGV_MAPERR`.

"Re-execute" forces no signal and leaves the PC on the faulting instruction (already-pending
signals may still be delivered).

### munmap, mprotect, mremap

- VMAs are only ever split (`split_at` adjusts file offsets and host pointers), never merged;
  `same_vma` tells which pieces Linux would have as one VMA.
- `munmap`: an unaligned address, zero or overflowing length or an end past 2^48 is `EINVAL`;
  `ENOMEM` only when one VMA would split in three at `vm.max_map_count`. It moves removed host
  regions to the `graveyard`, sets `dirty`, records executable ranges in `code_ranges`, returns
  borrowed pages, and the syscall reaps SysV segments left without attachments.
- `mprotect` (`protect`, Linux's `do_mprotect_pkey`): unknown prot bits (`PROT_SEM` included) or
  both grows bits are `EINVAL` at once; an unaligned address is `EINVAL`; then a length of 0
  succeeds and an overflowing one is `ENOMEM`. The VMAs from the first one ending above the
  address are changed in turn; a gap (`ENOMEM`) or a VMA refusing the permissions
  (`may_write`/`may_exec`, `EACCES`) stops it with the VMAs before it already changed. With
  `PROT_GROWSDOWN` the range starts at the start of the Linux VMA holding that first VMA, which
  must grow down (`EINVAL`; `ENOMEM` if it starts at or above the end), so it reaches the parts the
  mapping grew by; `PROT_GROWSUP` is `EINVAL` once the range is found. A `cow` VMA is copied
  (`unshare`, which sets `dirty`; a `Fill` VMA maps its host file privately again, copies the pages
  it changed after reading them and drops its entries) before it becomes writable, and a
  `reserved` region gets real host memory (`back_reserved`) before it becomes accessible.
  Otherwise only a permission downgrade sets `dirty`. A private anonymous VMA holding fork CoW
  pages first gets the new permissions without write, then its CoW pages get read-only entries
  (lent ones at its own host pages, borrowed ones at the owner's, built even where a `PROT_NONE`
  VMA had none), and only then do its other pages get write permission.
- `mremap`: the syscall checks the flags (`MREMAP_DONTUNMAP` needs equal lengths) and gives `EPERM`
  for a fixed target below `vm.mmap_min_addr` before anything else. Then Linux's order: no mapping
  at the old address is `EFAULT`; a `MREMAP_FIXED` target is validated (`EINVAL`) and unmapped and
  a shrunk tail unmapped; then the rest of the old range must lie in one Linux VMA (`EFAULT`,
  `same_vma`, as Linux before 6.17). Shrinking without a target only unmaps the tail. Growing in
  place needs the old end to be a VMA end, free space above, the new end at or below `mmap_top`,
  and room for one more VMA (the grown part is a separate VMA with the same lock, dump, fork,
  `noreserve` and `anon` flags; a `Fill` VMA's maps more of its host file, sharing the fill); a
  locked VMA's grown part is neither charged nor populated. A move re-points the same host memory
  and moves the page-table entries with it (a SysV segment keeps its attach base and the vDSO its
  recorded address).
- A move (growing with `MREMAP_MAYMOVE` and no room in place, `MREMAP_FIXED`,
  `MREMAP_DONTUNMAP`) is `ENOMEM` before anything changes unless the address space stays more
  than three VMAs below `vm.max_map_count` (`move_room`, 6.1's `move_vma`), with two more for a
  target's unmapping (`mremap_to`); the moved pieces go back in without the count check
  (`place_vma`), so a move never fails for room halfway.
- `MREMAP_DONTUNMAP` follows 6.1's order: no mapping at the old address `EFAULT`, the target
  `EINVAL`, the margin `ENOMEM`, the target unmapped, the one-VMA rule `EFAULT`; it splits nothing
  before these, and then also needs room for the splits and for one VMA per piece left at the old
  address. What it leaves there: for anonymous memory a new empty VMA, for a private file a
  `Refill` that the syscall maps from the store object (both without the old piece's dump, fork,
  grows-down, `noreserve` and `anon` flags), and for shared memory and a `Fill` VMA a copy of the
  old VMA (unlocked; a `Fill` one on its host file mapped privately again); that path reaps no SysV
  segments, though unmapping its target can end a segment's last attachment.
- `mremap` with an old length of 0 duplicates (6.1's "duplicate shm area"): the VMA holding the
  address (`EFAULT` if none) must be shared (`EINVAL`; a `MREMAP_FIXED` target is validated and
  unmapped before this check), and without `MREMAP_MAYMOVE` it is `ENOMEM`; no margin is asked. A
  shared file mapping maps the file again from the address's offset for the new length; shared
  anonymous memory and System V segments get a VMA on the same host memory, which may run past the
  source VMA to the end of the region and past that has no contents (`SIGBUS`). A segment's copy is
  a new attach base, so it counts as one more attachment and shmdt at its address detaches it.
- `remap_file_pages`: a non-zero prot is `EINVAL`; start and size are rounded down; the range must
  lie in contiguous shared file pieces of one host file and prot (`EINVAL`; shared anonymous memory
  and segments are refused); it maps the file again with `MAP_SHARED | MAP_FIXED`, keeping the
  label, exec denial and lock, and populates unless `MAP_NONBLOCK`.
- `msync`: flags and alignment `EINVAL`, the range must be fully mapped (`ENOMEM`), `MS_INVALIDATE`
  on locked pages `EBUSY`; `MS_SYNC` syncs the shared file ranges on the host with the BKL
  released. `mincore`: alignment `EINVAL`, fully mapped or `ENOMEM`; reserved pages report 0 and
  borrowed pages 1; the result is written without growing the stack.
- `mlock`/`mlock2`/`munlock` follow `do_mlock`: the length plus the offset in the page is
  page-aligned with wrapping arithmetic; after mlock's `RLIMIT_MEMLOCK` check (`EPERM` with a limit
  of 0, `ENOMEM` above it; skipped with `CAP_IPC_LOCK`) a range that wraps past the end is `EINVAL`
  and an empty one succeeds; the range must be fully mapped (`ENOMEM`) and is populated unless
  `MLOCK_ONFAULT`. `mlockall(MCL_CURRENT)` checks the whole size (`PROT_NONE` reservations
  included) against the limit, locks every VMA and populates unless `MCL_ONFAULT`; `MCL_FUTURE`
  makes new VMAs locked, uncharged.
- `madvise`: alignment and length `EINVAL`, unknown advice `EINVAL` (counted as unimplemented), a
  length of 0 succeeds. Flag advice (`DONTDUMP`/`DODUMP`, `WIPEONFORK`/`KEEPONFORK`,
  `DONTFORK`/`DOFORK`) needs the range fully mapped (`ENOMEM`), splits the VMAs at its ends and
  sets the flag. Whether the range is locked (`MADV_DONTNEED` is `EINVAL`) or all private anonymous
  (`MADV_FREE` and `MADV_WIPEONFORK` otherwise `EINVAL`) is judged over the VMAs overlapping it
  only. Advice not listed here returns 0 and does nothing.
- `MADV_DONTNEED` (`discard`): private anonymous memory goes back to the host and reads zero
  (CoW pages settled first); the written pages of a private file mapping before EOF go back to the
  file's contents: whole host pages of a host private file mapping through host `MADV_DONTNEED`,
  and the rest (partial host pages on 16 KB hosts, or a mapping copied into anonymous memory when it
  became writable) as `Reread`s that the syscall reads from the store object (zeros past EOF, or
  for an object that cannot be opened) and stores with `fill_private`; a `Fill` mapping's partial
  host pages are read back from its host file, so it returns to what was read and its entries
  stay. Executable ranges are queued for code invalidation; shared mappings and unwritten or `cow`
  ones are left alone. The vDSO is refilled from the kernel's image (with the svc stubs when
  `vdso_svc`) after its pages are discarded (Linux's special mapping faults its pages back), so its
  code survives.
- `MADV_REMOVE` (`remove`): the range must be fully mapped; locked pages or private anonymous
  memory `EINVAL`, other private or never-writable mappings `EACCES` (6.7's rule); the shared
  mappings' parts before EOF are freed through the host's `MADV_REMOVE` (a hole in the host file or
  shared memory), with the partial host pages at the ends, or everything when the host refuses,
  zeroed instead; no code invalidation is queued.

### Stack growth

- `grow_down` refuses pages below `vm.mmap_min_addr` and addresses already mapped. It applies when
  the nearest VMA at or above the address is `growsdown`. It refuses if the single nearest VMA
  below is non-growsdown, accessible and closer than 256 pages (`stack_guard_gap`; a `PROT_NONE` or
  growsdown neighbour is not looked past), or if the new stack size exceeds the current
  `RLIMIT_STACK`. Otherwise it extends into the reserved room below, or maps a new chunk in a host
  allocation of `clamp(size, grow, limit − size)` bytes (exactly the growth if that fails) with the
  chunk at its top, so the VMA count grows only logarithmically; the chunk takes the flags of the
  VMA above it and needs room for one more VMA.
- The kernel grows a stack itself at syscall entry when SP is below the main stack (arm64 has no
  red zone), before writing a signal frame, and when a kernel access to unmapped memory fails
  (`grow_for`, as Linux's fault handler does): the shared syscall helpers (`get`/`put`), read,
  write and readv buffers, getdents64, vmsplice, waitid and FUSE's device ioctls and prefault go
  through `Cx::read_user`/`write_user`, and `/proc/<pid>/mem` accesses grow within the accessor's
  `RLIMIT_STACK`. Other direct `Mm::read`/`write` callers do not grow it.
- `MAP_GROWSDOWN` is honoured only for private anonymous mappings (including private `/dev/zero`);
  `MAP_SHARED | MAP_ANONYMOUS` with it is `EINVAL`, and file or shared `/dev/zero` mappings ignore
  it. An mmap'd growsdown VMA has no reserved room, so its first growth maps a new chunk.

### fork and copy-on-write

- The caller quiesces the parent, then `fork` checks commit unless `vm.overcommit_memory` is 1
  (`commit_too_big`: counts private, currently writable, non-`MAP_NORESERVE`, non-dontfork VMAs
  the child gets, merging adjacent ones the way Linux `vma_merge` would; `ENOMEM` when one exceeds
  RAM plus swap; a limit of 0 disables it; mode 2's strict accounting is not emulated).
  `fork` cannot fail after lending starts; the parent's TLB flush waits for its `sync`.
- The child keeps the parent's membarrier registrations (Linux copies `membarrier_state`); execve
  starts a fresh `Mm` without them.
- Each child VMA with contents of its own gets a new `anon` identity, shared by the pieces of one
  Linux VMA (Linux's `anon_vma_fork`), so it no longer joins its neighbours while the parent's
  pieces still do: a written anonymous VMA, or a written private file VMA when a page of its host
  region holds a private copy (host pagemap: present without the file bit, or swapped; unreadable
  pagemap counts as yes).
- Per VMA (`MADV_DONTFORK` VMAs are skipped; the child's VMAs keep `dontdump` and `wipeonfork`):
  - shared mappings, reservations and read-only private file mappings share the parent's region;
    read-only private file VMAs are marked `cow` on both sides;
  - non-growsdown, non-reserved, non-heap anonymous or writable private-file VMAs of at most 1 MB
    are carved from one spare region;
  - the heap (the anonymous VMA at the page-aligned `brk_start`, at most 256 MB) gets a spare region
    of 256 MB; a growsdown anonymous VMA with reserved room below gets a spare region of room plus
    length; everything else gets new anonymous memory;
  - private anonymous VMAs that were ever written are lent, except `MADV_WIPEONFORK` ones, where
    the child keeps its own all-zero memory; never-written ones need nothing;
  - writable private file VMAs are copied page by page up to EOF, skipping all-zero pages; a
    `Fill` one instead maps its host file privately again and copies the pages read that differ
    from it, keeping the same fill (pages either side reads later reach both), and is never
    carved.
- Lending: VMAs of at most 16 pages lend every page. Larger ones read the host
  `/proc/self/pagemap` only over 2 MB leaf windows that have entries or CoW pages (merged up to
  1 GB); untouched pages are known zero and the child keeps its own. If pagemap is unreadable, every
  page in the windows is lent. Pages the parent itself borrowed are re-lent from their owner. Parent
  and child get read-only entries for the lent pages (none for `PROT_NONE` VMAs; a later mprotect
  rebuilds them).
- Borrower write (`cow_break`): only for a writable private anonymous VMA whose page is borrowed or
  lent. A borrowed page is copied into the VMA's own slot, re-mapped with the VMA prot, the
  borrower's threads are shot down, and the loan is returned.
- Owner write, munmap, mremap move or `MADV_DONTNEED` of a lent page (`recall`): every borrower
  copies first (interrupting its guest threads), then the owner gets write permission back.
- Borrower munmap returns borrowed pages without copying; borrower mremap and `MADV_DONTNEED`
  copy first (`cow_settle`).
- When the last borrower returns a page, the owner's entries are restored at once (`repossess`),
  in runs that never cross VMAs, without a flush.
- mprotect over CoW pages never gives a CoW page write permission or a borrowed page an entry at
  the borrower's own host page, not even between steps (see mprotect above), since other threads
  may run guest code meanwhile.
- Futex keys of private memory are the address space's id and the address, so computing a key
  does not copy a borrowed page (writing the word, as PI locks, robust lists and `FUTEX_WAKE_OP`
  do, breaks CoW); a private operation's key needs no mapping at all (Linux checks only that it is
  a user address); shared anonymous memory is keyed by host address and shared file mappings by
  file and offset (`kernel/futex`).
- Kernel writes: `Mm::write` breaks CoW per page; `write_kernel` and `write_force` settle CoW
  (`cow_settle`), copy `cow` VMAs and back reservations, and ignore VMA permissions (but
  `write_force` refuses shared mappings without write permission). No-BKL fast-path writes go
  through the CPU's `translate(WRITE)` and fall back to the BKL path on failure.
- Owner destruction (exit, exec): borrowed pages are returned. If it has lent pages, their loans
  become orphans held by the borrowers' `Arc`, and every non-lent page of its written private
  anonymous VMAs is discarded. Lent or borrowed pages are never written in place, even by a sole
  orphan borrower.

### Release of host memory

- Regions stay in the `graveyard` until `sync` has quiesced the other threads, so guest data races
  never touch memory already returned to the host.
- A `HostRegion` dropped while the BKL is held is queued in `UNMAPPED` and counted in `UNMAPPING`.
  When the BKL is released, page tables are freed first, anonymous ranges go back to `recycled` one
  at a time (kept after host `MADV_DONTNEED` within the caps, otherwise unmapped), file ranges are
  sorted and unmapped in coalesced runs, and then `spare::refill` tops the fork pool back up.
- Under the BKL, fork still calls host mmap when a needed length is missing from both the spare
  and recycled pools (and for private mappings over 1 MB or growsdown VMAs without reserved room
  when `recycled` has no match), and still unmaps inline the pieces of a length evicted from the
  spare pool.
- `Mm` drop leaves its code group, releases CoW state, then the page table (deferred the same way),
  then VMAs and the graveyard.

### Files: EOF and truncation

- Whole pages past EOF have no entries. A shared mapping whose file grew picks up the new content on
  the next fault; private mappings do not.
- truncate, ftruncate and `O_TRUNC` that shorten a file first call `file_truncated` on every
  address space (entries past the new end are removed, executable ranges are queued for
  invalidation, `dirty` is set; `Fill` VMAs are left alone) and sync each one that changed, the
  current one included (one that cannot be borrowed is skipped), and only then shorten the host
  file: no thread can reach the dropped tail through a stale translation, so an access there faults
  into the kernel and gets the guest's `SIGBUS` once the BKL is free. A host truncation that then
  fails leaves the mappings' EOF at the requested length.
- When a store file that shared its object gets one of its own (`kernel/files`' `make_private`),
  its shared mappings follow it, picked by store inode and old host file so other files' mappings
  of the old object stay (nothing moves if an address space cannot be borrowed): `file_moving`
  first removes, in every address space, the entries past the new object's length (as truncation
  does) and each changed one syncs; then `file_moved` maps the new object over each of their host
  regions with `MAP_FIXED` at the same host addresses, so entries and TLBs stay valid, and points
  the VMAs' file identity and fd at it (a failure is only logged). Private mappings stay on the old
  object.

### Introspection

- `regions` labels each VMA (file name, else `[stack]` for the VMA containing `start_stack` and the
  contiguous same-prot growsdown VMAs below it, else `[vdso]`, else `[vvar]` for the foreign VMA,
  else `[heap]` inside `[brk_start, brk)`) and joins adjacent pieces of one Linux VMA (`same_vma`,
  plus the same label). procfs escapes a newline in a file name as `\012`.
- `dump_regions` applies `coredump_filter` and `MADV_DONTDUMP` (the vDSO is dumped whole anyway; a
  written private file VMA follows `ANON_PRIVATE`; ELF headers' first pages follow their own bit)
  and writes nothing of `[vvar]`; the core-dump writer leaves holes where `residency` reports pages
  absent in regions other than private file mappings.
- Resident memory (status, statm, smaps, pagemap, rusage peaks, cgroups) is measured by
  `kernel/oom`.

## Dependencies (inbound / outbound)

### Inbound

- Syscall dispatch: `syscall/mem.rs`, and `syscall/mod.rs` (stack growth at entry, `take_wanted`
  on a retry).
- `kernel/fuse`: `Fill` (`map_fill` makes it, `fill` reads into it, `prefault` reads syscall
  buffers in through `first_unread`).
- Processes: `syscall/proc.rs` (clone/fork, execve), `syscall/ns.rs` (setns, `vdso_syscalls`),
  `task.rs` (fault dispatch), `kernel.rs` (`Cx` stack growth, BKL hooks), `signal.rs` (signal
  frames, `mapped_above`).
- Loader (`loader.rs`), ptrace (`write_force`), futexes (`futex_key`, `translate`, `id` and
  `shared_maps` on the fast path, `cow_break`), SysV IPC, `syscall/fs.rs` (truncation, `ETXTBSY`
  via `exe`, seals, clones and files moving to objects of their own), AF_UNIX (`can_write`).
- Observers: procfs (`pseudo.rs`), core dumps; `kernel/oom` (VMAs, CoW sets, `FileMaps` and
  `UNMAPPING` for the measures; `Mm::release`).
- `kernel/mmu`: VMAs and EOF for `fault_in`; the recorded changes `sync` acts on.
- `Machine` (initial `FileMaps`, `trim_memory`).

### Outbound

- `kernel/mmu`: page-table entries (`map_pages`, `fill_shared`, `map_shared`, `protect_shared`,
  `move_entries`), `sync`, `quiesce`, `shootdown`, code groups.
- `tuxport_hal`: host `mmap`/`munmap`/`madvise` wrappers (private anonymous memory, file mapping
  with host-page alignment, `map_file_over`, discard, remove, populate), `fstat`, `msync`,
  `mincore`-based residency, pagemap reads (`touched_pages`, `has_private_pages`), memfds,
  `ram_and_swap`, `page_size`.
- `kernel/vdso`: the image to refill (`image`, `svc_image`), the clock entries and their
  stubs (`clocks`, `svc_bytes`), the host's vvar, and its identity as a trusted translation source.
- `kernel/fuse` through `Fill::source` (the open file a `Fill` keeps open; reading is the
  caller's).
- vfs (file kinds, seals, store objects for re-reads and `Refill`s), mount flags (`kernel/mount`),
  credentials
  (`CAP_IPC_LOCK`, `CAP_SYS_RAWIO`), rlimits; SysV IPC (`reap_shm` after munmap, mremap and
  `MAP_FIXED` mmap, so a removed segment goes with its last attachment).

## Invariants and gotchas

- Every change must keep `kernel/mmu`'s rules: release host memory only after the other threads
  have flushed, leave entries valid for guest code at each step (a `shootdown` does not hold
  threads back), and record what changed (`dirty`, `graveyard`, `code_ranges`) for `sync`.
- A `Fill` VMA has `file_len` 0 and entries only for pages read; a page not read is never written
  anywhere (guest writes need an entry, kernel writes read it first), so storing it into the host
  file shows it to every VMA of the fill, and copies of a `Fill` VMA (fork, `unshare`, mremap)
  map that host file again rather than copying into anonymous memory.
- Every private anonymous page with content has a page-table entry, except borrowed pages in a
  `PROT_NONE` VMA (which fork's window search adds from the CoW sets). Kernel-side writes, mlock
  and `MAP_POPULATE` fill entries, mremap moves them, and `PROT_NONE` keeps them with prot 0.
  fork's lending and RSS measurement rely on this.
- Lent and borrowed pages are never writable in place. `Mm::translate` does not break CoW, so any
  kernel code that writes guest memory through it must handle CoW pages itself.
- `Mm`, `Loans` and `Cow` use `Rc`/`RefCell` and are correct only under the BKL; in checking
  builds (debug, or the `borrow-check` feature) every release of the BKL in the middle of a wait
  asserts that no such borrow is held. `shared_maps` is shared across threads (the fast path reads
  it).
- CoW never mutably borrows two address spaces at once: borrower-side loan returns run inside the
  borrower's own operation or drop, and owner-side `recall` only takes shared borrows of borrowers.
  The `try_borrow` guard in `return_loans` is therefore defensive (see the latent hazard below).
- The host's `vm.max_map_count` is shared by the whole app process, so the guest limit is enforced
  per address space.
- VMAs never merge; adjacent anonymous VMAs are separate host mappings and are only joined (as
  `same_vma`) for display, mremap's one-VMA rule, `PROT_GROWSDOWN`, fork's `anon` identities and
  (by its own rule) fork's commit estimate. Code creating a VMA that continues another (stack
  chunks, mremap growth) must copy the flags `same_vma` compares.
- Guest pages are always 4 KB (`AT_PAGESZ` 4096) whatever the host page size.
- All guest processes live in the one app process, and every guest mapping but a never-accessible
  `PROT_NONE` reservation and `[vvar]` takes host address space (some more than their size: the
  heap's and stacks' reservations, file mappings rounded to host pages). On hosts with a 39-bit
  user address space (512 GB, every 4 KB-page GKI kernel) they must all fit in it: large writable
  mappings such as ASan's shadow memory cannot be mapped there, and the guest address space is not
  shrunk to match.
- Host `mincore` reports pages swapped to zram as absent, which is why fork's touched-page check
  reads pagemap instead; RSS, mincore(2), pagemap and core-dump holes still use mincore.

## Confirmed bugs / technical debt

- **FUSE mappings read on demand are not Linux's page cache:** a page keeps what was read when first
  touched, so writes through the mount or by another process are not seen and separate mmap calls
  read separately; the size is fixed at mmap (growth not seen, truncation never `SIGBUS`); mlock,
  `MADV_WILLNEED` and core dumps do not read pages (dumps write unread pages as zeros); a kernel
  access to an unread page reruns the whole syscall, so a syscall other than read/write, pread,
  pwrite and the vector forms (whose buffers the dispatcher reads in first) repeats what it did
  before the copy, or returns a short count after a partial transfer.
- **Some madvise advice is ignored:** `MADV_DONTNEED_LOCKED` and `MADV_POPULATE_*` are accepted
  but do nothing, and `MADV_FREE`, `MADV_COLD` and `MADV_PAGEOUT` are not refused on locked ranges.
- **Past-EOF faults ignore permissions:** a `PROT_NONE` or write-to-read-only access past EOF gets
  `SIGBUS` instead of `SIGSEGV`/`SEGV_ACCERR`, and private file mappings never see file growth.
- **Latent CoW leak:** if `return_loans` ever ran while the owner was mutably borrowed, the page
  would stay in the owner's `lent` set without a loan; a fork in that state would give the child a
  loan-less borrowed entry and the owner's next write would show in the child. Unreachable today,
  and the guard's comment cites a path (`recall`) that never reaches it.
- **Linux VMA boundaries are approximated:** anonymous memory that has been writable counts as
  having an anon_vma although Linux creates one only at the first write fault, and anonymous pieces
  join regardless of Linux's page offsets (a VMA moved by mremap joins its new neighbours), so
  `/proc/<pid>/maps` and mremap's one-VMA rule can still differ in such cases; `MREMAP_DONTUNMAP`
  of one Linux VMA in several pieces needs room for a VMA per piece, and growing a VMA in place
  adds one where Linux extends it; `shmat` below `vm.mmap_min_addr` is not refused.
- **mremap moves leave stale addresses:** a moved SysV segment keeps its old attach base (shmdt at
  its new address is `EINVAL`), and a moved vDSO keeps its recorded address (the default signal
  restorer, the vDSO's refill after `MADV_DONTNEED`, its `[vdso]` label and the time-namespace
  stubs use the old one); `MREMAP_DONTUNMAP`'s anonymous and private-file remnants lose the old
  VMA's flags, and a segment it detaches is reaped only by a later munmap, mremap or mmap.
- **mlock never fails on inaccessible ranges:** `mlock`/`mlock2` over a range that includes a
  `PROT_NONE` mapping succeeds and keeps populating the mappings after it, where Linux 6.1 locks the
  whole range but stops populating at the `PROT_NONE` mapping and returns `ENOMEM`.
- **mlock population inflates RSS:** locking read-only private anonymous pages makes them count as
  resident (Linux maps the zero page, which is not counted), and locking written private memory in
  a forked child does not break copy-on-write (Linux 6.1 copies the pages at mlock time) but
  touches the child's own, still unused host pages, so smaps counts the range twice until the
  pages are written.
- **Ranges with gaps fail as a whole:** madvise, msync, mlock and mincore refuse a range that is not
  fully mapped before doing anything (`ENOMEM`), where Linux acts on the mapped parts first.
- **A FUSE mapping's short read is taken as a whole window:** `store` records the whole window as
  read even when the daemon returned less (a short reply, or an error after the first chunk), so
  the rest stays zeros and is never asked for again.
- **A written private file mapping marked `cow` at fork is not reverted by `MADV_DONTNEED`:** fork
  marks every read-only private file VMA `cow`, written or not, and `discard` skips `cow` VMAs, so
  the written data stays in both processes.
## Open questions

- Why is `STACK_TOP` `1 << 47` while the page table covers 48 bits and `MAP_FIXED` may reach
  `1 << 48`?
- Should mappings locked through `mlockall(MCL_FUTURE)` be charged against `RLIMIT_MEMLOCK`?
- Should `WRITABLY_SHARED` ever be cleared (for example when the inode is deleted)?
- Is the window in `mmap` and `shmat` between installing entries and `note_shared_maps` acceptable
  for a concurrent fast-path futex on a writable shared page?

## To verify

- Whether the host kernel commit-accounts the read-write `MAP_PRIVATE` host file mappings used for
  private file VMAs that miss `FileMaps`: depends on host kernel rules, not repository code.
