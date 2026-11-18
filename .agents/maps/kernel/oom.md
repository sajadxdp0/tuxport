# kernel/oom Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/oom.md` for the paired decision history (decisions from before the split are D26, D29, D32, D35 and D44 in `.agents/decisions/kernel/mm.md`).
>
> Verified: 2026-11-11 (split out of the kernel/mm full mapping of 2026-11-10 and its later maintenance; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-14 (targeted verification only since the full mapping; latest change: references follow the kernel/time and kernel/boot split from kernel/process)

## Responsibilities

`kernel/oom` measures guest memory and keeps the app under its memory limit (6.1's
`mm/oom_kill.c`, and the RSS figures of `fs/proc/task_mmu.c`):

- resident memory of an address space for procfs (status, statm, smaps, pagemap), rusage peaks,
  OOM victim scores and the kill log, and of several address spaces together for child cgroups;
- the OOM killer: the app-wide and cgroup limits, polling, victim choice, kills, `memory.events`
  and `memory.oom.group`, and `/proc/<pid>/oom_score`;
- `process_mrelease`, freeing an exiting process's private memory before its threads are gone;
- the periodic release of Tier 1 code written through the writable view.

Root: `core/kernel/src/oom.rs`; in `mm.rs` the resident-memory measures (`resident_in`,
`cow_shared`, `pagemap`, `stack_bytes`, `resident_pages`, `peak_resident_pages`, `resident_split`,
`Usage`) and `Mm::release`; process_mrelease's syscall in `syscall/mem.rs`.

Outside the module: VMAs, host backing and copy-on-write, which the measures read (`kernel/mm`);
the timer thread that runs the check and the stop/resume of periodic checks (`kernel/time`);
the cgroup tree, its memory files and controller (`kernel/cgroup`); the procfs files that print the
measures (`kernel/procfs`); group exit and kill reports (`kernel/signal`, `kernel/process`).

## Key types

- `RssPlan`, `FileResidency` — one RSS sample plan, and a process-wide cache of file-mapping
  residency (1 s, 1024 entries, emptied when full).
- `Usage` — the resident memory of several address spaces together (a child cgroup's): their host
  spans and the file pieces already counted.
- `Oom` (`Kernel::oom`) — the app-wide limit (`u64::MAX` is none), the root's peak, the next check
  deadline (`None`: at the next wake, at start, after a `memory.max` write, or while every task
  sleeps), the last victim and when it was picked, the victims that outlived their second
  (`stuck`), and the next code-view release.
- `Events` — a cgroup's `memory.events` counters (`max`, `oom`, `oom_kill`, `oom_group_kill`),
  kept hierarchical and local.
- Intervals: 20 ms, 5 ms above 3/4 of a limit, at least 20 times the measurement's own time; 1 s
  victim timeout and code-view interval.

## Public entry points

- **Measures**: `resident_in`, `cow_shared`, `pagemap`, `stack_bytes`, `resident_pages`,
  `resident_split`, `peak_resident_pages`; `Usage::add` and `pages` (child cgroups).
- **OOM killer**: `oom::check` and `oom::unmap_code_views` (timer thread), `out_of_memory` (also
  from a lowered cgroup `memory.max`), `Oom::{measure, recheck}`, `default_limit`, `app_memory`;
  `Shared::resume_checks` from the run loop; `oom::proc_score` for `/proc/<pid>/oom_score`.
- **process_mrelease**: the syscall, and `Mm::release` on another process's address space.

## Data flow / lifecycle

### Resident memory

- Residency comes from host pointers, never from guest addresses: host `mincore`, except that
  `resident_in` (smaps) counts file mappings by host pagemap presence or swap (falling back to
  `mincore`; readahead fills the page cache around touched pages) and a borrowed page only when the
  owner's host page is resident. `resident_pages` and `resident_split` count every borrowed page.
  Reserved VMAs and `[vvar]` never count.
- `stack_bytes` (VmStk) sums the grows-down VMAs; `cow_shared` counts pages with more than one user
  for smaps' shared lines and divides them for Pss.
- `resident_split` (status's `VmRSS` as their sum, and `RssAnon`/`RssFile`/`RssShmem`) measures each
  VMA as `resident_pages` does and sorts it: private anonymous memory, borrowed pages and written
  private file mappings as anonymous (all of a written file mapping, where Linux counts only its
  copied pages), shared anonymous memory, SysV shm, memfds and deleted files as shmem
  (`shmem_like`, also used by core dumps' `ANON_SHARED`), other file mappings as file.
- `resident_pages` (statm, stat, OOM victim choice) skips `[vvar]` and private anonymous VMAs never
  writable nor written by the kernel, measures private anonymous VMAs over 1 MB only over windows
  with entries, and uses the 1 s file-residency cache for unwritten private file mappings and all
  shared mappings with a file identity (so they can lag by a second). `resident_in` (smaps, OOM
  kill log) uses the same windows but no skip and no cache.
- `Usage` (child cgroups' usage) measures address spaces as `resident_pages` does, but together:
  each file piece measured from the page cache counts once (device, inode, offset and length, so
  overlapping but different pieces of one file count separately), and the other spans, with every
  borrowed page as the owner's host page, are united before one mincore pass, so shared anonymous
  memory, SysV shm, `FileMaps` mappings and pages lent for copy-on-write, which are the same host
  pages in every address space, count once, and a borrowed page counts only while resident.
- `peak_resident_pages` feeds `ru_maxrss` and VmHWM, assuming spans totalling at most 256 pages
  are resident instead of measuring them; it is sampled at last-thread exit, before execve (cheaply
  for a process younger than a tick that has not execed), on getrusage (at most every 10 ms, not
  for `RUSAGE_CHILDREN`) and on process_mrelease.
- `pagemap` marks resident private anonymous pages present and exclusive, borrowed pages present
  only, every other mapping's pages (shared anonymous, shm, memfd, written private file pages
  included) present by `mincore` and file-backed, and reports no frame numbers.

### OOM killer

- All guest memory is host memory of the one app process, and Android's low-memory killer would end
  the whole app, so the OOM killer keeps the app under its own limit by ending guest processes.
  Guest accesses to host-backed pages never reach the kernel layer, so it polls.
- The limit is set once at start from `MachineOptions.memory_limit`: none given (the Android host,
  and the CLI's default) means half of the host's total RAM, or no limit if that reads 0; the CLI's
  `--memory-limit none` means no limit. The guest cannot change it (the root's `memory.max` is
  `EPERM`); the kernel turns it off for good if the usage cannot be read or the check panics (child
  cgroups' limits stay).
- The root (whole-app) usage is the app process's host RssAnon + RssShmem + VmSwap (VmRSS and no
  shmem on kernels without RssAnon), minus Tier 1 code written through the writable view and not
  yet unmapped: it includes translations and kernel-layer metadata and excludes host file pages.
  A child cgroup with the memory controller enabled and a numeric `memory.max` uses `Usage` over
  its processes' address spaces, each address space once (a vfork child and its parent,
  `CLONE_VM` processes).
- `check` runs under the BKL each time the timer thread wakes. With no root limit and no cgroup
  limit it does nothing; otherwise it measures every 20 ms, every 5 ms once a scope is above 3/4 of
  its limit, and never more often than 20 times the measurement's own time. The root is measured
  first and cgroups stop at the first scope over its limit, so one scope is handled per round. A
  `memory.max` write forces an immediate round.
- When every task is sleeping (a thread in `Cx::unlocked` counts as sleeping), `check` stops
  through `Shared::stop_checks`: it sets `checks_idle` and then looks at the tasks again, clearing
  the flag and going on if one woke meanwhile. Every guest thread clears its `sleeping` flag and
  then calls `resume_checks` at the top of each run-loop iteration, which signals the timer thread
  if the flag was set; with all these accesses SeqCst a waking thread is never missed (the
  code-view release and load-average sampling stop the same way).
- An over-limit round still measures but is skipped while `UNMAPPING` (`kernel/mm`'s pending host
  releases) is nonzero or the last victim, picked under 1 s ago, still has threads; otherwise it
  records a `max` event and calls `out_of_memory`. Lowering a child's `memory.max` below its usage
  calls `out_of_memory` once at once, without a `max` event or the `UNMAPPING` check.
- `select` considers every process except tgid 1 (the built-in init), `oom_score_adj` -1000,
  processes outside the over-limit cgroup, zombies, a vfork child whose parent thread still waits
  for it with the same address space (Linux's `in_vfork`: ending it frees nothing, and with its
  parent's memory and a higher tgid it would win), and every victim that outlived its second and
  still has threads (kept in a list until it exits). The score (`badness`, Linux's `oom_badness`)
  is `resident_pages + adj * (total / 1000)`, unclamped, with `total` the larger of RAM and the
  limit for the root, the cgroup's limit otherwise; on equal scores the later (higher tgid) wins,
  as `oom_evaluate_task` keeps the last one not below the best. With no eligible process nothing
  is killed or logged.
- `/proc/<pid>/oom_score` (`proc_score`, Linux's `proc_oom_score`) is the same badness with
  `total` the host's RAM plus swap, as `(1000 + badness * 1000 / total) * 2 / 3` (0 to 2000), and
  0 for the processes `select` never takes (init, -1000, no live thread, a vfork child).
- The kill records `oom` on the domain and its ancestors; a victim already exiting is only waited
  for. Otherwise the group gets `SIGKILL` (`group_exit`), its cgroup and ancestors count
  `oom_kill`, host stderr gets a line, the kernel log gets Linux's memcg lines (`memory: usage …`,
  `oom-kill:constraint=CONSTRAINT_MEMCG,…`, `Memory cgroup out of memory: Killed process …`; the
  app-wide limit appears as a memcg OOM; the log's anonymous, file and shmem split goes by
  mapping kind, so memfds show as file), and the app gets a `KillReason::OutOfMemory` report. With
  `memory.oom.group` on the victim's cgroup or an ancestor up to the domain, every other live
  process of the topmost such cgroup except init and -1000 ones is killed too (stuck victims
  included); the victim is logged and counted twice, as Linux's `mem_cgroup_scan_tasks` does.
- `unmap_code_views` runs on the timer thread whether or not there is a limit: at once after a
  resume from idle, then once a second while any task runs, and once more when everything is idle,
  it calls `tuxport_engine::unmap_written_code`, because translation-cache pages written through the
  writable W^X view count twice in RssShmem. It is timed rather than continuous because each unmap
  makes the next write to every such page fault again.

### process_mrelease

- The syscall (Linux 6.1's `mm/oom_kill.c`): any flag is `EINVAL`; the fd must be a pidfd (a
  `/proc/<pid>` directory is `EBADF`, unlike pidfd_send_signal), whose process has not been
  reaped and still has a thread in the task table (`ESRCH` otherwise: zombies, Linux's
  `find_lock_task_mm`); that process must be exiting (`ThreadGroup::exiting` set: exit_group, a
  fatal signal taken or SIGKILL sent, an OOM or host kill, or every thread in the exit path) and
  not writing a core file, and so must every other process whose first live thread uses the same
  address space (`task_will_free_mem`), else `EINVAL`. It succeeds again on a second call. The
  process's peak resident memory is sampled first, so its `ru_maxrss` keeps what it used; if its
  address space is borrowed elsewhere at that moment nothing is released and 0 is returned.
- `Mm::release` (Linux's `__oom_reap_task_mm` over anonymous and private VMAs) leaves the VMAs, and
  so maps, unchanged and skips executable mappings (their translations, the vDSO, and pages they
  borrowed) and `PROT_NONE` reservations. Borrowed pages are re-pointed at the VMA's own page and
  returned without copying (one shootdown, the owners getting write permission back); lent pages
  stay for their borrowers; the other pages of written private anonymous VMAs go back to the host
  (reading zero); written private file VMAs go back to the file in whole host pages of a host file
  mapping (partial host pages kept) or, copied into anonymous memory, to zeros. A thread of the
  process still running reads what is left; on Linux any fault there is `SIGBUS` (`MMF_UNSTABLE`).

## Dependencies (inbound / outbound)

### Inbound

- `kernel/time`: the timer thread (`check`, `unmap_code_views`) and its idle handshake;
  `kernel/process`: the run loop calling `resume_checks`.
- `kernel/procfs`: status, statm, stat, smaps, pagemap and `oom_score`.
- `kernel/cgroup`: the cgroup files (`measure`, `limit`, peak, `app_memory`, `Events`,
  `out_of_memory` for a lowered `memory.max`).
- `kernel/files`: the write path's `recheck` after writing cgroup `memory.max`.
- rusage and exit (`peak_resident_pages`), `Machine` (the memory limit at start), the
  process_mrelease syscall.

### Outbound

- `kernel/mm`: VMAs, CoW sets and loans, `FileMaps`, `UNMAPPING`, `shmem_like`; `shootdown`
  (`kernel/mmu`) for borrowed pages a release re-points.
- `tuxport_hal` (`memory_usage` from `/proc/self/status`, `memory_info`, `mincore` and pagemap
  reads), `tuxport_engine` (`written_code_bytes`, `unmap_written_code`), `kernel/cgroup` (`memory_limits`,
  `measure_memory`, `memory_event`, `oom_group`), `signal::group_exit`, the kernel log (`printk`)
  and kill reports (`bug::record_kill`).

## Invariants and gotchas

- The OOM killer uses two measures: the root counts the whole app process (translations, metadata,
  swap, no file pages), while cgroups and victim scores count guest resident pages (file pages
  included, swap excluded; shared pages once per cgroup, but once per process in victim scores).
- The OOM killer polls, so a process that allocates and exits within one interval is never seen,
  and under Tier 1 a guest can touch gigabytes between polls: the limit needs headroom.
- The OOM killer acts on no measurement while `UNMAPPING` is nonzero.
- `failcnt` in the OOM log is the domain's hierarchical `max` count, including its descendants'.
- The measures rely on `kernel/mm`'s rule that every private anonymous page with content has a
  page-table entry: windows without entries are not measured.

## Confirmed bugs / technical debt

- **OOM scores omit swap and page tables:** a mostly swapped-out process ranks low although the
  root's usage counts VmSwap (Linux adds swap entries and page-table pages to `oom_badness`).
- **pagemap reports lent pages as exclusive:** they are mapped by several address spaces, contrary
  to Linux and the function's own description of the bit.
- **process_mrelease judges "exiting" by `ThreadGroup::exiting`:** a fatal signal other than
  SIGKILL sets it only when a thread takes the signal (Linux's `complete_signal` starts the group
  exit when it is sent), so a release right after kill with SIGTERM is `EINVAL`; and when a main
  thread has exited on its own, the last thread calling exit(2) counts as exiting, where Linux,
  still seeing the zombie leader in the thread list, answers `EINVAL`. The OOM killer does not
  release its victims' memory itself (Linux's oom_reaper does).

## Open questions

- `statm` reports data = rss while `status` reports VmData = VmSize; is the mismatch intentional?
- A kill frees no Tier 1 translations kept in the translator's pool or shared through code groups,
  and the OOM killer never trims the pool; is that acceptable when translations dominate the root
  usage?

## To verify

- None beyond the full mapping's: the Linux 6.1 comparisons here were checked from knowledge of the
  kernel, not against its source.
