# System V IPC and message queues (kernel/ipc) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/ipc"`
>
> Last distilled: 2026-11-08 (HEAD = fffe1c1)

## Active

### D1: SysV IPC and POSIX message queues are one kernel/ipc module
- **What**: kernel/ipc owns the per-namespace object model, the 12 SysV syscalls and the six `mq_*` syscalls with `MessageQueue`, while the IPC namespace lifecycle stays in kernel/process, shm host memory and VMAs in kernel/mm, mqueue node views and sysctls in kernel/procfs, and mqueue mounts and file-syscall dispatch in kernel/vfs.
- **Why**: About 2300 lines in `ipc.rs`, `syscall/ipc.rs` and `syscall/mqueue.rs` had no module, so no map described them and their bugs had nowhere to be recorded.
- **Tradeoffs**: Message-queue behaviour is spread over four maps (readiness in kernel/poll too), and the map's Linux comparisons come from recalled upstream source.
- **Watch out**: Bugs recorded at mapping or since stay open in the map, among them unshare and setns dropping sibling threads' undo records, global pids in `/proc/sysvipc`, unenforced semmns and shmall, mqueue registrations keyed by tgid alone and an unenforced `RLIMIT_MSGQUEUE`.
- **Source**: 4140c75 (bug list since updated by 5d23297, b43ff37, 5d7b936 and 2aca260)

### D2: mremap duplicates of shm segments count as attachments
- **What**: A copy made by mremap with an old length of 0 tags its shm segment with its own start, so nattch counts it and shmdt at its address detaches it (kernel/mm D21).
- **Why**: Linux counts a duplicated segment VMA as another attachment through its `vm_ops` open.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 5dcf7db

### D3: Message-queue timeouts stay on the realtime clock
- **What**: mq_timedsend and mq_timedreceive keep their absolute `CLOCK_REALTIME` timeout as an `Expiry` on the realtime base (kernel/process D19).
- **Why**: Their timeouts are absolute realtime times, which fired late once turned into monotonic instants.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 63b9e59

### D4: IPC limits stay within int, and large allocations fail with ENOMEM
- **What**: msgmax, msgmnb and the four sem values refuse values above `INT_MAX` (shmmax and shmall stay unsigned longs), msgsnd, semaphore sets and semtimedop allocate through `ipc::zeroed`, which returns `ENOMEM` when the host cannot, and semtimedop takes nsops as an unsigned int.
- **Why**: After root raised these limits, any process in the namespace could abort the app with one msgsnd, semget or semtimedop allocating by them.
- **Tradeoffs**: `zeroed` touches a large message's pages before the copy, which would touch them anyway.
- **Watch out**: Allocations the host overcommits still succeed and may later be killed by the host's low-memory killer rather than fail with `ENOMEM`.
- **Source**: 09188ec

### D5: Interrupted message-queue waits restart
- **What**: mq_timedsend and mq_timedreceive return `ERESTARTSYS` when a signal interrupts their wait, so `SA_RESTART` handlers and stops resume them with the same absolute deadline (D3).
- **Why**: They returned `EINTR`, so `SA_RESTART` handlers and even stops ended the wait.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 5d23297

### D6: Closing a queue descriptor ends its process's notification registration
- **What**: `mqueue_closed(tgid, files)` runs at every close site (close, close_range, dup3, close-on-exec, seccomp addfd) and when a thread exit drops the last reference to its fd table, as Linux's `mqueue_flush_file` does, and `remove_notification` sends the `NOTIFY_REMOVED` cookie for `SIGEV_THREAD`, also when a stale registration is replaced.
- **Why**: A registration outlived its process's queue descriptors, so another process got `EBUSY`, and ending a `SIGEV_THREAD` registration left musl's helper thread blocked forever.
- **Tradeoffs**: Registrations stay keyed by tgid (recorded in the map).
- **Watch out**: A close path added later must call `mqueue_closed` too.
- **Source**: b43ff37

### D7: Removed shm segments are reaped where attachments end, and mqueue defaults follow the sysctls
- **What**: `reap_shm` also runs after munmap, mremap, `MAP_FIXED` mmap, execve and thread exit, and a new queue's default limits are capped by `msg_max` and `msgsize_max` as `mqueue_get_inode` does.
- **Why**: Removed shm segments stayed until the next shm call, and mqueue defaults ignored `msg_max` and `msgsize_max`.
- **Tradeoffs**: Each such call scans the marked segments once more.
- **Watch out**: None known.
- **Source**: 5d7b936

### D8: SysV capability checks go over the IPC namespace's owner
- **What**: `Ids::check` and `may_change` take the IPC namespace's owning user namespace and check `IPC_OWNER` and `SYS_ADMIN` with `ns_capable` over it, `SHM_LOCK` checks `IPC_LOCK` over it, and every SysV syscall passes the owner.
- **Why**: Root of a container could not stat or remove another user's queue in its IPC namespace, where Linux's `ipcperms` and `ipcctl_obtain_check` check those capabilities over the namespace's owner.
- **Tradeoffs**: Every call reads the owner from the task's namespace set (a cheap `Rc` clone).
- **Watch out**: Objects created before an IPC namespace switch are still checked against the namespace they are looked up in.
- **Source**: 2aca260
