# kernel/ipc Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/ipc.md` for the paired decision history.
>
> Verified: 2026-11-06 (full mapping: 3 research concerns; 11 claims checked by 2 independent verifiers — 10 confirmed and 1 partial, corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-11 (targeted verification only since the full mapping; latest change: mq_notify's netlink sockets attributed to kernel/netlink, split out of kernel/net)

## Responsibilities

`kernel/ipc` holds the System V IPC objects and POSIX message queues of every IPC namespace, and
the syscalls on them:

- the per-namespace object tables (`Ipc`): SysV message queues, semaphore sets with their
  `SEM_UNDO` records, shared memory segments, the POSIX message-queue names, and the namespace's
  IPC limits;
- SysV ids, `ipc_perm` permission checks and the 64-bit `*_ds`/`*info` layouts;
- the 12 SysV syscalls, including blocking, undo at exit and the destruction of shm segments;
- the six `mq_*` syscalls and the `MessageQueue` object (priority order, blocking, notification,
  attributes), which the mqueue filesystem's operations reuse;
- the contents of `/proc/sysvipc/*` and the values behind `kernel.msg*`, `kernel.sem`,
  `kernel.shm*` and `fs.mqueue.*`.

Root: `core/kernel/src/ipc.rs` (object model), `syscall/ipc.rs` (SysV syscalls),
`syscall/mqueue.rs` (POSIX message queues).

Outside the module: creating, entering and collecting IPC namespaces (`IpcNs`, and `Kernel::ipcs`,
`new_ipc_ns` and `collect_ipc` in `kernel.rs`) and clone/unshare/setns (`kernel/process`); host
shared memory and shm VMAs (`SharedMemory`, `attach_shm`, `detach_shm`, `shm_attachments`)
(`kernel/mm`); the mqueue filesystem's node views, the `/proc/sysvipc` files and sysctl plumbing
(`kernel/procfs`); mqueue mounts and the generic file syscalls that dispatch to queues
(`kernel/vfs`, `kernel/files`); readiness and epoll of queue fds (`kernel/poll`); signal delivery
(`kernel/signal`); the netlink sockets used by `SIGEV_THREAD` (`kernel/netlink`); credentials
(`kernel/cred`).

## Key types

### SysV objects

- `Ipc` — one IPC namespace's objects: the `msg`, `sem` and `shm` tables; `undo`, the `SEM_UNDO`
  adjustments keyed by `(tgid, semid)`; `tunables`; and the POSIX side: `mqueues` (name to queue),
  `mq_limits`, `mq_dir`. `Kernel::ipcs` keeps one per namespace id, next to a weak `IpcNs`.
- `Table<T>` — slots of `(Perm, T)` with a sequence number per slot; an id is
  `seq * 32768 + index` (IPCMNI 32768).
- `Perm` — `ipc_perm`: key, uid/gid, cuid/cgid, mode (permission bits plus `SHM_DEST` and
  `SHM_LOCKED`), seq.
- `Tunables` — `msgmax`, `msgmnb`, `msgmni`, `sem` (semmsl, semmns, semopm, semmni), `shmmax`,
  `shmall`, `shmmni`, with Linux's defaults; writes take non-negative integers, at most `INT_MAX`
  for Linux's int sysctls (`msgmax`, `msgmnb`, the `sem` values) and IPCMNI for the `*mni` values,
  any u64 for `shmmax` and `shmall` (unsigned longs on Linux); `limits` holds the fixed `SEMVMX`
  and `SEMAEM` (32767) and `SHMMIN` (1).
- `MsgQueue` / `Msg` — the messages, capacity `qbytes`, `cbytes`, times, last sender and receiver.
- `SemSet` / `SemOp` — values, last pids, `ncnt`/`zcnt`, times; one `sembuf`.
- `ShmSeg` — `SharedMemory` host backing, size, times, creator and last pid; the attach count is
  not stored. `ShmKey = (namespace id, shmid)` names a segment from a VMA.

### POSIX message queues

- `MessageQueue` — inode number (from a global counter, never reused), namespace id, owner, mode,
  times, `maxmsg`, `msgsize`, messages as `(priority, bytes)`, at most one `Notify` registration,
  the count of blocked receivers, and `sent`/`received` counters for edge-triggered polling. Its
  fd kind is `FileKind::MessageQueue`.
- `Notify` — `Signal { tgid, sig, value }`, `None { tgid }` (`SIGEV_NONE`) or
  `Socket { tgid, netlink socket, cookie }` (`SIGEV_THREAD` as musl and glibc implement it).
- `Limits` (`MqLimits`) — `queues_max`, `msg_max`, `msgsize_max`, `msg_default`,
  `msgsize_default` (defaults 256, 10, 8192, 10, 8192).
- `MqDir` — a namespace's mqueue root: owner (the namespace's creator), mode 01777, times.

## Public entry points

- **SysV syscalls** (all under the BKL, none on the fast path): `msgget`, `msgctl`, `msgrcv`,
  `msgsnd`, `semget`, `semctl`, `semtimedop`, `semop` (`semtimedop` without a timeout), `shmget`,
  `shmctl`, `shmat`, `shmdt`.
- **mqueue syscalls**: `mq_open`, `mq_unlink`, `mq_timedsend`, `mq_timedreceive`, `mq_notify`,
  `mq_getsetattr`.
- **For the mqueue filesystem** (`syscall/fs.rs`, procfs): `new_queue`, `insert_queue`,
  `remove_queue`, `find` (`mqueue_find`); `MessageQueue::{status, stat, touch_read, set_mode,
  set_owner, set_times}`; `MqDir::stat`.
- **For poll and epoll**: `MessageQueue::{readiness, arrivals}`; the queue is the wake object.
- **Process and namespace hooks**: `ipc::exit` (group exit), `Ipc::exit` (unshare, setns),
  `reap_shm`, `nattch`.
- **procfs**: `Tunables::{read, write}`, `Limits::{get, slot}`, `proc_listing`.

## Data flow / lifecycle

### Namespaces

- Boot creates the initial namespace's `Ipc`; clone or unshare with `CLONE_NEWIPC` creates a fresh
  one (default limits, empty tables, a root directory owned by the creator's fsuid and fsgid).
  clone with both `CLONE_NEWIPC` and `CLONE_SYSVSEM` is `EINVAL`.
- Every syscall uses the calling task's namespace. An `Ipc` is dropped once its `IpcNs` has no
  strong reference (tasks, mqueue mounts and nsfs files hold one), but `collect_ipc` runs only
  after a thread exits, after setns and after unshare. Shm mappings keep their host memory, and
  open queue fds their queue, after the namespace is gone.

### Ids and permissions

- `get_or_create` follows `ipcget`: a non-private key is looked up first; an existing object with
  `IPC_CREAT|IPC_EXCL` is `EEXIST`, then the object's own check (semget's nsems, shmget's size),
  then permission (`EACCES`); a missing key without `IPC_CREAT` is `ENOENT`. Creating checks the
  object count against `msgmni`, `semmni` or `shmmni` (`ENOSPC`) before the object's own
  validation; the caller becomes owner and creator, with mode `flags & 0o777`.
- `insert` takes the lowest free slot and advances that slot's sequence, so a stale id fails with
  `EINVAL`. A waiting loop that has slept once turns a failed lookup into `EIDRM`.
- `Perm::check` is `ipcperms` (owner, group through fsgid and supplementary groups, other;
  `CAP_IPC_OWNER` bypasses it). `may_change` (IPC_SET, IPC_RMID) needs the owner, the creator or
  `CAP_SYS_ADMIN`. These and `CAP_IPC_LOCK` for SHM_LOCK count over `IpcNs.owner`, the user
  namespace owning the IPC namespace (Linux's `ns_capable(ns->user_ns, …)`), so root of a
  container's user namespace has them over its own IPC namespace; raising `qbytes` above `msgmnb`
  needs `CAP_SYS_RESOURCE` in the initial namespace.
- The `*ctl` calls strip `IPC_64` from cmd. `IPC_STAT` and the `*_STAT`/`*_STAT_ANY` forms encode
  the 64-bit layouts with ids mapped into the caller's user namespace and pids into its pid
  namespace; the `*_STAT` forms take an index and return the full id, and `*_STAT_ANY` skips the
  read check. `IPC_INFO` and `*_INFO` return the highest used index.

### SysV message queues

- msgsnd: size up to `msgmax` (`EINVAL`) and `mtype >= 1`; the text is copied into a buffer
  allocated fallibly (`ENOMEM` when the host cannot); the message fits
  when `cbytes + len <= qbytes` and the count is below `qbytes`; otherwise `IPC_NOWAIT` is
  `EAGAIN`, else the caller waits.
- msgrcv: `mtype` 0 takes the first message, a positive one the first of that type (with
  `MSG_EXCEPT` the first of another type), a negative one the lowest type up to its absolute value;
  a longer message is `E2BIG` unless `MSG_NOERROR` truncates it; no match is `ENOMSG` with
  `IPC_NOWAIT`, else the caller waits. The message is dequeued before it is copied out. `MSG_COPY`
  is `ENOSYS`.
- msgctl IPC_SET raising `qbytes` above `msgmnb` needs `CAP_SYS_RESOURCE`.
- Both waits return `ERESTARTNOHAND` on a signal.

### Semaphores

- semget: nsems at most `semmsl` and nonzero for a new set (allocated fallibly, `ENOMEM`); an
  existing set with fewer semaphores is `EINVAL`. `semmns` is only reported (IPC_INFO, SEM_INFO, the sysctl), never enforced.
- semtimedop: nsops (an unsigned int, as in Linux's prototype) 1 to `semopm` (`E2BIG` above),
  read into a fallibly allocated buffer; a semaphore number out of range is `EFBIG`;
  write permission if any op is nonzero, else read. `SemSet::apply` works on copies of the values
  and undo adjustments and commits only if every op succeeds (`ERANGE` above `SEMVMX`, or when an
  adjustment leaves ±`SEMAEM`); a blocking op leaves everything unchanged, and its `ncnt` or `zcnt`
  counts the waiter while it sleeps. `IPC_NOWAIT` on the blocking op or an expired timeout is
  `EAGAIN`; a signal is `EINTR` (never restarted). The relative timeout becomes a deadline once.
- semctl: GETVAL, GETPID, GETNCNT, GETZCNT and GETALL need read permission; SETVAL and SETALL need
  write permission, check `SEMVMX` and clear that semaphore's (or the set's) undo adjustments in
  every process; IPC_RMID also drops the set's undo records.
- `SEM_UNDO` adjustments are created on demand and dropped when they are all zero; a forked child
  has none.

### Shared memory

- shmget: destroyed segments are reaped first; a new segment needs a size from `SHMMIN` to
  `shmmax` and no `SHM_HUGETLB`, and gets page-rounded host shared memory; an existing smaller
  segment is `EINVAL`. `shmall` is only reported, never enforced.
- shmat: alignment (`SHM_RND` rounds down to 4096), `SHM_RDONLY`, `SHM_EXEC`, `SHM_REMAP`, then
  permission, then `Mm::attach_shm`, which tags the VMA with the `ShmKey` and its base. shmdt needs
  a shm VMA starting at the address, and updates the segment in the namespace recorded at attach
  time if it still exists.
- `nattch` is computed: distinct attach bases per distinct address space, over all tasks; an
  mremap duplicate (old length 0) of a segment is its own base, so it counts, as on Linux.
- shmctl IPC_RMID sets `SHM_DEST`, makes the key private and removes the segment only when nothing
  is attached; otherwise `reap_shm` removes destroyed segments with no attachments, in every
  namespace. It runs from shmget, shmdt and shmctl (any command), and after anything else that can
  end an attachment: munmap, mremap and `MAP_FIXED` mmap, execve dropping the old address space,
  and a thread leaving the task table at exit; so a segment goes with its last attachment, as on
  Linux. shmat does not check `SHM_DEST` (a segment still attached can be attached again, as on
  Linux). SHM_LOCK and SHM_UNLOCK (owner, creator or `CAP_IPC_LOCK`) only toggle
  `SHM_LOCKED`.

### SysV wake-ups

- Every SysV change that can unblock a waiter (send, receive, IPC_SET on a queue, removal, a
  successful semop, SETVAL, SETALL, undo at exit) calls `request_wake()`, which wakes waiters on
  every channel; the waiting loops call `Cx::wait` with no wake objects, and each re-checks its own
  condition. There is no handoff to a particular waiter and no order among waiters.

### Process exit and namespace changes

- At group exit, `ipc::exit` applies the process's undo adjustments in every namespace (values
  clamped to 0 to `SEMVMX`) and removes them.
- unshare with `CLONE_SYSVSEM` or `CLONE_NEWIPC`, and setns into an IPC namespace, first apply and
  drop the whole process's undo records in the old namespace. clone otherwise ignores
  `CLONE_SYSVSEM`: undo records are per process whatever the flag says.

### POSIX message queues

- mq_open: the name (one leading `/` stripped) must be 1 to 255 bytes without `/`, `.` or `..`;
  access mode 3 is `EINVAL`. An existing queue is `EEXIST` with `O_CREAT|O_EXCL`, else permission
  by access mode. A new one: attr's `mq_maxmsg` and `mq_msgsize` must be positive and within
  `msg_max` and `msgsize_max` (the hard limits 65536 and 16 MiB with `CAP_SYS_RESOURCE`), the
  defaults capped by `msg_max` and `msgsize_max` without attr (`mqueue_get_inode`); at most
  `queues_max` named queues (`ENOSPC`, unless `CAP_SYS_RESOURCE`); owner fsuid/fsgid and mode
  `perm & ~umask & 0o777`. The fd (always close-on-exec) is installed
  before the name is entered, so a failed install leaves nothing; opening an existing queue also
  touches the root directory's times. Neither mq_open nor mq_unlink checks permission on the root
  directory.
- Through a mounted mqueue filesystem the same objects are created by open with `O_CREAT`, removed
  by unlink (both check write and search permission on the root directory), read as a status line
  (`QSIZE`, `NOTIFY`, `SIGNO`, `NOTIFY_PID`) and changed by chmod, chown and utimensat; write is
  `EINVAL`.
- mq_timedsend: priority below 32768, length up to `msgsize` (`EMSGSIZE`); a full queue is `EAGAIN`
  with `O_NONBLOCK`, `ETIMEDOUT` after the deadline, `ERESTARTSYS` when interrupted (restarted after
  a handler with `SA_RESTART`, a stop, a freeze or a ptrace trap, keeping the absolute deadline;
  `EINTR` otherwise), else the sender waits on the queue object. The message goes before
  the first one of lower priority. If the queue was empty and no receiver is blocked, the
  registration fires once: a signal with `SI_MESGQ` and the sender's pid and uid as the registrant
  sees them, a `NOTIFY_WOKENUP` cookie queued on the registrant's netlink socket, or nothing
  (`SIGEV_NONE`).
- mq_timedreceive: the buffer must hold `msgsize`; the front message is taken; an empty queue is
  `EAGAIN`, `ETIMEDOUT` or interrupted as mq_timedsend is, else the receiver waits, counted in
  `receivers`. Timeouts are
  absolute `CLOCK_REALTIME` (`itimer::Expiry` on the realtime base), so they move with the host's
  wall clock.
- Senders, receivers and poll/epoll wait on the queue object; both directions wake it.
- mq_notify validates the sigevent before the fd; NULL cancels the caller's own registration;
  `SIGEV_THREAD` takes a netlink socket fd and a 32-byte cookie. While the registrant's process is
  alive, any new registration is `EBUSY`, the registrant's own included; a registration ends when
  it fires, when its registrant cancels it, when the registrant's process closes any descriptor of
  the queue (close, dup3 over it, close_range, close-on-exec, seccomp addfd replacing it, or its fd
  table going with its last thread; Linux's `mqueue_flush_file`), or when another process registers
  after the registrant is gone or a zombie. A `SIGEV_THREAD` registration that ends without firing
  queues its cookie marked `NOTIFY_REMOVED`, which ends musl's helper thread. mq_getsetattr changes
  only the fd's `O_NONBLOCK`.
- mq_unlink and filesystem unlink remove only the name (the queue's owner, the root directory's
  owner, or `CAP_FOWNER`); open fds keep the queue.

## Dependencies (inbound / outbound)

### Inbound

- The syscall dispatcher.
- `kernel/process`: group exit (`ipc::exit`).
- `kernel/ns`: unshare and setns (`Ipc::exit`), namespace creation (`MqDir::new`, default
  `Tunables` and `Limits`).
- `kernel/vfs` and `kernel/files`: open, create, unlink, read, lseek, fstat, fstatfs, chmod, chown
  and utimensat on the mqueue filesystem (`new_queue`, `insert_queue`, `remove_queue`, `find`, the
  `MessageQueue` accessors).
- `kernel/procfs`: `/proc/sysvipc/*` (`proc_listing`), the IPC and mqueue sysctls, mqueue node
  views (`find`, `MqDir`, `MessageQueue::stat`).
- `kernel/poll`: readiness, arrivals and the wake object of queue fds.

### Outbound

- `kernel/mm`: `SharedMemory::new`, `Mm::attach_shm`, `detach_shm`, `shm_attachments`, guest
  memory reads and writes.
- `kernel/process`: `Cx::ipc`/`ipc_mut`, `Cx::wait`/`wait_objects`, `request_wake` and
  `request_wake_object`, `interrupted`, pid namespaces (`vnr`, `pid_seen_by`).
- `kernel/cred`: `capable`, `capable_wrt`, `permission`, id mapping; the umask of the fs context.
- `kernel/signal`: `send_to_group` with `SigInfo` for notifications.
- `kernel/netlink`: netlink socket queues for `SIGEV_THREAD`.
- `kernel/files`: fd tables and `OpenFile`; `kernel/vfs`: `Vfs::mq_dev` for fstat.
- `tuxport_hal`: the realtime clock for SysV times.

## Invariants and gotchas

- A removed object's id names nothing until its slot's sequence wraps (65535 reuses). Slots are
  reused lowest first, so ids differ from Linux's cyclic allocation: create, remove, create gives
  32768 where Linux gives 1.
- A namespace's `Ipc` is found by id; looking up a collected namespace panics, which is safe only
  because the calling task holds its `IpcNs`.
- SysV waiters wake on any kernel change and redo the lookup and permission checks, so an IPC_SET
  during a semop wait can end it with `EACCES`, and waiters are served in no particular order.
- msgrcv and mq_timedreceive dequeue before copying out, so a fault loses the message (as on
  Linux).
- `shmmni` counts destroyed segments that are still attached.
- An unlinked queue that is still open no longer counts toward `queues_max` and still reports
  nlink 1.
- mqueue `find` scans the namespace's names linearly by inode.
- An mqueue deadline does not follow realtime clock changes made during the wait.

## Confirmed bugs / technical debt

- **unshare and setns drop sibling threads' undo:** undo records are per process, so one thread's
  `unshare(CLONE_SYSVSEM|CLONE_NEWIPC)` or IPC setns applies and discards the `SEM_UNDO`
  adjustments of every thread in the process (Linux applies a shared undo list only when its last
  sharer leaves, and threads share one only with `CLONE_SYSVSEM`).
- **`/proc/sysvipc` leaks global pids:** the listings print stored global pids without translating
  them into the reader's pid namespace (the `*_STAT` syscalls do translate), and the shm listing
  masks the mode with `0o1777`, hiding `SHM_LOCKED`.
- **`semmns` and `shmall` are not enforced:** semget and shmget never fail with `ENOSPC` for the
  total number of semaphores or shm pages; both values are only reported.
- **mqueue registrations are keyed by tgid alone:** one whose process exited while its fd table
  lives on in another process (`CLONE_FILES`) stays registered, and a reused tgid looks alive and
  would get the signal (Linux holds the registrant's `struct pid`).
- **mqueue limits and root permissions are partly missing:** `RLIMIT_MSGQUEUE` is not enforced; mq_open and mq_unlink skip the root directory's permission check that the filesystem
  paths make (it matters once the 01777 root is chmod'ed), and unlink applies the sticky rule even
  when the root has lost `S_ISVTX`.

## Open questions

- Is the `wake::ALL` broadcast for SysV waits meant to stay?
- Should `collect_ipc` also run when the last nsfs file or mount holding an IPC namespace goes?

## To verify

- The Linux 6.1 behaviour the bugs and gotchas above compare against (cyclic id allocation,
  `ns_capable` in the IPC checks, `semmns`/`shmall` `ENOSPC`, destruction at the last detach,
  shared undo lists, pid translation in `/proc/sysvipc`, `mqueue_flush_file`, `NOTIFY_REMOVED`,
  restarting `wq_sleep`, capped default sizes) comes from upstream knowledge: no Linux source is in
  the repository.
