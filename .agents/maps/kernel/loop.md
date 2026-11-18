# kernel/loop Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/loop.md` for the paired decision history (decisions from before the split are D21 in `.agents/decisions/kernel/vfs.md`).
>
> Verified: 2026-11-11 (split out of the kernel/vfs full mapping of 2026-11-10, whose loop-device claims its independent verifiers checked; the boundary was checked against the source at the split)

## Responsibilities

`kernel/loop` is the loop devices, the only block devices, after Linux 6.1's
`drivers/block/loop.c` and the block-device parts of `block/fops.c` and `block/ioctl.c`:

- `/dev/loop-control` (10:237), which adds, removes and hands out devices, and `/dev/loop<N>`
  (block 7:N), eight from boot and more on demand, each with a node in the boot `/dev`;
- opening, the openers and the exclusive claim, and the release that unbinds an autoclear device;
- binding a device to an open file and the status, capacity, block-size and read-only requests;
- reads and writes forwarded to the backing file at the device offset, with no page cache, and the
  block ioctls;
- the `/sys/block/loop<N>/` values `kernel/procfs` shows.

What it is for: LTP acquires its test device this way (and runs on tmpfs over it), `losetup`
works, and tools such as mkfs can write images through it. Nothing can be mounted from a device:
Tuxport has no disk filesystems.

Root: `core/kernel/src/syscall/loopdev.rs`.

Outside the module: device-number dispatch at open, nodev and the cgroup device filter, fd tables,
the read/write paths of the backing file and the `IoPath` routing (`kernel/files`); the `Device`
kinds and the boot `/dev` nodes (`kernel/vfs`); the `/sys/block` directory itself (`kernel/procfs`);
remount's writer check over backing files (`kernel/mount`); epoll's refusal of the device
(`kernel/poll`).

## Key types

- `Loops` (`Kernel::loops`) — devices by number and a disk-sequence counter.
- `Loop` (`LoopRef = Rc<RefCell<Loop>>`) — number, backing open file (`None` unbound), offset,
  size limit and size in bytes, flags, name, logical and soft block sizes, `BLKROSET` and disk
  read-only state, read-ahead, rotational, disk sequence, opener count and exclusive claim.
- `LoopOpen` — carried by a loop device's open file (`Device::Loop`'s `block`): the device and
  whether it was opened `O_EXCL`; its drop is the device's release (6.1's `lo_release`).

## Public entry points

- From `kernel/files`: `open` (a block node of major 7), `read`, `write`, `seek`, `flush_target`
  (fsync), `ioctl` (the loop and block requests), `control_ioctl` (`LOOP_CTL_*` on
  `/dev/loop-control`).
- `Loops::numbers`, `sysfs_exists`, `sysfs_read` (`kernel/procfs`'s `/sys/block` view);
  `Loops::backing_files` (remount's writer check, `kernel/files`' `opens_of`).

## Data flow / lifecycle

### Devices and loop-control

- Eight devices exist from boot. `LOOP_CTL_ADD` (a number below 2^20, else `EINVAL`, or the lowest
  unused for a negative one; `EEXIST`), `LOOP_CTL_REMOVE` (`EINVAL` negative, `ENODEV` missing,
  `EBUSY` bound or open) and `LOOP_CTL_GET_FREE` (the lowest unbound, else a new device) also
  create or delete `/dev/loop<N>` in the boot `/dev`; other requests are `ENOSYS`, and read and
  write on loop-control `EINVAL`. Opening block 7:N of a missing device creates it (Linux's
  legacy autoload).
- Open counts an opener; `O_EXCL` claims the device (`EBUSY` while another open holds it); the
  first opener sets the soft block size (the largest power of two up to a page dividing the size).

### Binding and status

- `LOOP_SET_FD`/`LOOP_CONFIGURE` bind an unbound device (`EBUSY` otherwise, or when another open
  holds the claim and this one is not the `O_EXCL` holder) to a store regular file
  (`FileKind::Regular`, so not a FUSE file) or a bound loop device that does not lead back to it
  (`EBADF` for itself or `O_PATH`, `EINVAL` otherwise); configure flags outside
  RO/AUTOCLEAR/PARTSCAN/DIRECT_IO, bad block sizes and any encryption are `EINVAL`, offsets over
  `i64::MAX` `EOVERFLOW`. The device is read-only when the file or the device's own open file is
  not writable; direct I/O is always dropped; partscan is kept but nothing is scanned. Size is the
  file length past the offset, capped by the size limit, in whole sectors, fixed until
  `LOOP_SET_CAPACITY` or a `LOOP_SET_STATUS` that changes offset or limit.
- `LOOP_CLR_FD` with other openers only sets autoclear; otherwise it unbinds at once (6.1; later
  kernels defer it to the close). The last opener's release unbinds an autoclear device.
  Unbinding resets offset, limit, size, flags, name and block size but keeps the disk read-only
  state, as Linux does.
- `LOOP_SET_STATUS(64)` needs write mode or `CAP_SYS_ADMIN` (`EPERM`), may set and clear
  autoclear and only set partscan; `LOOP_GET_STATUS(64)` reports the backing file's device, inode
  and rdev as stat shows them (`EOVERFLOW` when the old structure cannot hold them or the offset;
  a null argument `EINVAL`). `LOOP_CHANGE_FD` swaps the file of a read-only device for one of the
  same size. `LOOP_SET_CAPACITY`, `LOOP_SET_DIRECT_IO` and `LOOP_SET_BLOCK_SIZE` also need write
  mode or `CAP_SYS_ADMIN`; `LOOP_SET_DIRECT_IO` is `EINVAL` to enable; `LOOP_SET_BLOCK_SIZE` takes
  a power of two from 512 to 4096 and keeps the size. Requests that need a bound device
  (`LOOP_CLR_FD`, the status requests, `LOOP_CHANGE_FD`, `LOOP_SET_CAPACITY`, direct I/O, block
  size) are `ENXIO` on an unbound one.

### I/O and block ioctls

- Reads past the end return 0 (an unbound device has size 0), zeros past a shrunk backing file;
  writes on a read-only device are `EPERM`, at or past the end `ENOSPC` (`O_APPEND` ignored). Data
  goes through the backing open file's own read and write paths at the device offset (regular
  files without the BKL), with no cache in between; backing errors, and a backing write of 0
  bytes, are `EIO`. lseek accepts only SET/CUR/END within the size; fsync syncs the backing file of
  a bound writable device; epoll refuses the device (`EPERM`); poll reports it ready.
- Block ioctls: sizes, logical/physical/minimum block size, read-only get/set (`BLKROSET` needs
  `CAP_SYS_ADMIN`), read-ahead (setting it `EACCES` without `CAP_SYS_ADMIN`), `BLKBSZSET`
  (`EACCES` without it, `EBUSY` while another open holds the claim, `EINVAL` below the logical
  block size), `BLKZEROOUT` (writes zeros), `BLKFLSBUF` (`EACCES` unprivileged), `BLKSECTGET`,
  `BLKROTATIONAL`, `BLKGETDISKSEQ`, and 0 for `BLKIOOPT`, `BLKALIGNOFF` and `BLKDISCARDZEROES`;
  `BLKDISCARD` and `BLKSECDISCARD` are `EOPNOTSUPP` (no discard), the discard and zero-out requests
  `EBADF` without write mode, `HDIO_GETGEO` `ENOTTY` (`EINVAL` with a null argument, as are the
  read-ahead gets), `BLKRRPART` and `BLKPG` `EINVAL` (no partitions) or `EACCES` (`BLKPG` reads
  its argument first, `EFAULT`); anything else `EINVAL`, as the loop driver answers.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/files`: open of block nodes, the read, write, lseek, fsync and ioctl dispatch
  (`IoPath::Loop`, `kernel/vfs`'s `Device::Loop` and `Device::LoopControl`), `rehome` and
  `opens_of` over backing files.
- `kernel/procfs`: `/sys/block/loop<N>/`.
- `kernel/mount`: `backing_files` for the read-only remount's writer check.

### Outbound

- `kernel/files`: the backing file's read and write paths, fd lookup; `kernel/vfs`: the `Device`
  numbers and `/dev` node creation in the boot `/dev`, stat of the backing file.
- Credentials: `CAP_SYS_ADMIN` checks.

## Invariants and gotchas

- A bound device holds its backing open file, and a loop over a loop holds the lower device's open
  file, so the lower device keeps that opener until the upper one is unbound. I/O takes the backing
  file out of the device before reading or writing it (the BKL may be released), and the device's
  file position has no lock, as block devices have none in Linux.
- The opener count lives in `LoopOpen`, dropped with the open file: anything that keeps an open
  file alive (a dup, a fork, an `SCM_RIGHTS` message, a loop binding) keeps the device open, and
  autoclear runs only when the last such reference goes.
- Native kernels after 6.1 defer `LOOP_CLR_FD`'s unbinding to the close, so differential cases
  must wait for the autoclear device to be unbound before checking it.

## Confirmed bugs / technical debt

- **Backing files:** only store regular files and loop devices can back one, so a FUSE file is
  `EINVAL` (Linux takes any regular file or block device).
- **No block layer:** no partitions (partscan is only a flag); no discard, and `fallocate` on the
  device is `ENODEV` (Linux punches or zeroes); mmap is `ENODEV` (Linux maps block devices);
  `O_DIRECT` alignment is not enforced.
- **Error and sequence gaps:** zone, tracing and persistent-reservation ioctls are `EINVAL` where
  Linux answers `ENOTTY` or `EOPNOTSUPP`; `BLKGETDISKSEQ` advances only when a device is created
  or bound, not when it is unbound.
