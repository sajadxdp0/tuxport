# kernel/aio Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/aio.md` for the paired decision history (decisions from before the split are D42 in `.agents/decisions/kernel/vfs.md`).
>
> Verified: 2026-11-11 (split out of the kernel/vfs full mapping of 2026-11-10, whose AIO claims its independent verifiers checked; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-14 (targeted verification only since the full mapping; latest change: references follow the kernel/time and kernel/boot split from kernel/process)

## Responsibilities

`kernel/aio` is Linux's native AIO (`io_setup` to `io_pgetevents`, 6.1's `fs/aio.c`):

- contexts per thread group, their `fs.aio-nr` charge and their release;
- io_submit's checks, with reads, writes and fsyncs run synchronously at submission through the
  regular file paths and their results queued at once;
- `IOCB_CMD_POLL` requests that stay pending, and their completion when looked at: by
  io_getevents, by io_cancel, and by the timer thread for polls that signal an eventfd;
- io_getevents and io_pgetevents waiting as ppoll would, and io_cancel.

Root: `core/kernel/src/syscall/aio.rs`.

Outside the module: the read, write, vectored and fsync paths and their file checks
(`kernel/files`); readiness, the ppoll-style wait, temporary signal masks and eventfds
(`kernel/poll`); thread groups and exit (`kernel/process`); the timer thread (`kernel/time`); execve
(`kernel/exec`); the context page's mapping (`kernel/mm`); `/proc/sys/fs/aio-nr` and `aio-max-nr`
(`kernel/procfs`).

## Key types

- `AioContext` — one io_setup context: its capacity `nr`, completed `io_event`s, pending polls
  (each holding its open file and the eventfd `aio_resfd` named) and the `dead` mark set when it
  is released; held by the thread group (`ThreadGroup::aio`), keyed by its id.
- `Kernel::aio_nr` / `aio_max_nr` — back `fs.aio-nr` and `fs.aio-max-nr`.
- `Kernel::aio_polls` — weak references to the contexts whose pending polls signal eventfds, for
  the timer thread.

## Public entry points

- Syscalls `io_setup`, `io_destroy`, `io_submit`, `io_cancel`, `io_getevents`, `io_pgetevents`
  (its temporary sigmask through `sigmask_arg`).
- `released` (`aio_released`: execve and the last thread's exit), `check_polls`
  (`check_aio_polls`, the timer thread).

## Data flow / lifecycle

### Contexts

- A context's id is the guest address of a 4096-byte read-only anonymous page that io_setup maps
  whatever `nr` is. The page never holds a ring header, so libaio's user-space ring check fails and
  it always makes the syscall. Contexts belong to the thread group: a non-thread child (fork,
  vfork, `CLONE_VM` without `CLONE_THREAD`) starts with none; execve and the last thread's exit
  release them.
- io_setup: `*ctxp` must be 0 and `nr` from 1 to `0x1000_0000 / 32` (`EINVAL`); each context
  charges `nr` to `aio-nr` against `aio-max-nr` (65536; `EAGAIN`).
- Releasing a context (io_destroy, execve, the process's exit; 6.1's `kill_ioctx`) marks it dead,
  drops its pending polls without events that could be taken but signals their eventfds, releases
  the charge and wakes waiters, which take what is queued; io_destroy (an unknown context
  `EINVAL`) then unmaps the page. It does not wait for a submit still blocked in I/O.

### Submission

- io_submit: an unknown context `EINVAL`, a negative count `EINVAL`, an unreadable or null iocb
  pointer `EFAULT`; then per iocb, in 6.1's order (io_submit_one, then aio_read/aio_write,
  aio_fsync, aio_poll): `aio_reserved2` must be 0 and `aio_nbytes` not negative (`EINVAL`); a
  context whose queued events and pending polls reach `nr` is `EAGAIN`; the fd must exist and not
  be `O_PATH` (`EBADF`), and with `IOCB_FLAG_RESFD` the resfd must be an open fd (`EBADF`) and an
  eventfd (`EINVAL`), which is taken then (closing or reusing the fd number later changes
  nothing); `aio_key` is written; unknown opcodes are `EINVAL`.
- Reads and writes: unknown `aio_rw_flags` bits `EOPNOTSUPP`, then the open mode (`EBADF`), then a
  file without Linux's `read_iter`/`write_iter` `EINVAL` (`has_iter`: directories, epoll, timerfd,
  signalfd, inotify, pidfd, message queue, seccomp and bpf fds, `/dev/loop-control`, reads of
  `/dev/kmsg`, writes of `/dev/full` and of eventfds), then a negative `aio_offset` `EINVAL`, also
  on a file that ignores the offset (`rw_verify_area`). Fsyncs: a nonzero `aio_buf`,
  `aio_offset`, `aio_nbytes` or `aio_rw_flags`, or a file fsync refuses (`has_fsync`), is
  `EINVAL`. Polls: events beyond 16 bits or a nonzero `aio_offset`, `aio_nbytes` or
  `aio_rw_flags` is `EINVAL`. The first failure is returned when nothing was submitted, otherwise
  the count so far.
- PREAD, PWRITE, PREADV, PWRITEV, FSYNC and FDSYNC run synchronously inside io_submit through
  `kernel/files`' read, write, vectored and fsync paths (blocking pipe or socket I/O blocks
  io_submit), and their result or negative errno is queued at once (an interrupted one as
  `-EINTR`, as `aio_rw_done` turns restart codes into it). `aio_offset` is used only for
  positional files; `RWF_NOWAIT` and `RWF_APPEND` are set temporarily as `O_NONBLOCK`/`O_APPEND`
  on the shared open file. A write to an `O_APPEND` regular file, or with `RWF_APPEND`, goes to the
  end whatever the offset (`append_offset`), leaving the file position alone, and
  `RWF_DSYNC`/`RWF_SYNC` sync the file after a write as pwritev2 does (`write_sync`).

### Polls and completion

- `IOCB_CMD_POLL` completes at once when ready (reporting the requested bits plus `POLLERR`,
  `POLLHUP` and `POLLNVAL`); otherwise it stays pending (one-shot, reporting every ready bit when
  it completes later). Linux completes it from the file's wake-up; here it completes when it is
  looked at: by io_getevents and io_pgetevents on that context, by io_cancel, and, when it signals
  an eventfd, by the timer thread, which watches its context (every wake channel, the pending
  polls' host fds and timerfd expirations, through `kernel/poll`'s `watch_aio_poll`) until no
  such poll is left. If that check panics, the timer thread forgets every watched context, and
  only io_getevents and io_cancel complete those polls from then on.
- io_getevents and io_pgetevents (`min_nr <= nr`, relative timeout, optional temporary sigmask):
  complete ready polls, then return once `max(min_nr, 1)` events are queued, `min_nr` is 0 or the
  deadline has passed; a destroyed context returns what is queued, else `EINVAL`; a signal is
  `EINTR`; otherwise wait as ppoll would on the pending polls' files (their in-process objects,
  host fds and timerfd expirations) and on the context, which submission, completion,
  cancellation and release wake (`wait_aio_polls`). Events whose copy out faults stay queued
  (`EFAULT`).
- io_cancel reads the iocb's key (`EFAULT`) before looking up the context, then first completes
  ready polls, so it cancels only a poll still pending (an event with result 0 that signals its
  eventfd, then `EINPROGRESS`); everything else has already completed (`EINVAL`).

## Dependencies (inbound / outbound)

### Inbound

- The syscall dispatcher.
- `kernel/process`: release at the thread group's exit.
- `kernel/time`: the timer thread's `check_aio_polls`.
- `kernel/exec`: release at execve.
- `kernel/procfs`: `fs.aio-nr` and `fs.aio-max-nr`.

### Outbound

- `kernel/files`: fd lookup, `read_file`, `write_file`, `vectored`, `fsync`, `has_fsync`,
  `seekable`, `append_offset`, `write_sync`; `FileKind` for `has_iter`; `kernel/vfs`: `Device`
  for `has_iter`.
- `kernel/poll`: `poll_once`, `wait_aio_polls`, `watch_aio_poll`, `set_temp_mask`/`restore_mask`,
  the resfd eventfds it signals.
- `kernel/mm`: mapping and unmapping the context page; `UserSlice` copies.
- `kernel/process`: wake objects, and `signal_later` to wake the timer thread when a watched
  context appears.

## Invariants and gotchas

- Contexts belong to the thread group, not the address space, so a `CLONE_VM` child cannot use its
  parent's contexts (Linux keeps them per mm).
- The capacity check runs before I/O that may release the BKL, so concurrent submitters can exceed
  `nr`.
- A pending poll that signals an eventfd is completed by the timer thread, so the eventfd is
  signalled shortly after the file becomes ready rather than inside the write that readied it, as
  Linux's wake-up completes it: a nonblocking read of the eventfd right after that write may still
  find 0.
- io_getevents sleeps on the context's wake object, so a path that queues an event must wake it.
- `has_iter` and `has_fsync` must follow new `FileKind`s and devices added in `kernel/files` and
  `kernel/vfs`.

## Confirmed bugs / technical debt

- **Events lost to signals:** io_getevents returns `EINTR` on a signal even when some events
  (fewer than `min_nr`) are queued, leaving them queued, where Linux returns them.
- **Per-request flags leak:** `RWF_NOWAIT` and `RWF_APPEND` are set as `O_NONBLOCK`/`O_APPEND` on
  the shared open file for the duration of the I/O, so other users of the description see them
  (and an `F_SETFL` of those bits made meanwhile is undone), where Linux keeps them per request.
- **Synthetic files all take reads and writes:** they are taken to have `read_iter` and
  `write_iter`, which holds for sysfs, cgroup2 and `/proc/sys` but not for every procfs file
  (`/proc/<pid>/mem`, `pagemap`), which Linux refuses with `EINVAL`.

## To verify

- The Linux 6.1 behaviour above comes from knowledge of `fs/aio.c`; not observed on a native
  kernel: `aio_rw_done`'s `EINTR`, the sync flags, `nr` charged to `aio-nr`, and io_submit's
  refusals for `/dev/kmsg`, `/dev/loop-control`, seccomp and bpf fds.
