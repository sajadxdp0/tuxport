//! Loop devices (the kernel/loop map): block devices whose contents are a file, after Linux 6.1's
//! `drivers/block/loop.c` together with the block-device parts of `block/fops.c` and
//! `block/ioctl.c`.
//!
//! `/dev/loop-control` (10:237) hands out devices and `/dev/loop<N>` (7:N) are the devices; eight
//! exist from boot (`CONFIG_BLK_DEV_LOOP_MIN_COUNT`) and more are added on demand, each getting a
//! node in the boot `/dev` as devtmpfs would give it. `LOOP_SET_FD` or `LOOP_CONFIGURE` binds a
//! device to an open file; reads and writes then go to that file at the device's offset, within a
//! size fixed when it is bound (`LOOP_SET_CAPACITY` takes the file's new length). There is no page
//! cache in between, so the file and the device always agree, and no block layer above it: no
//! partitions, discard or direct I/O are offered, and nothing can be mounted from a device, since
//! Tuxport has no disk filesystems. What it is for: LTP acquires its test device this way (and runs
//! on tmpfs over it), `losetup` works, and tools such as mkfs can write images through it.

use std::collections::BTreeMap;
use std::rc::Rc;

use super::{IO_CHUNK, SysResult, get, put};
use crate::cell::RefCell;
use crate::cred::cap;
use crate::errno::Errno;
use crate::fd::{FileKind, FileRef, oflags};
use crate::kernel::Cx;
use crate::vfs::dev::{DISK_GID, Device, LOOP_MAJOR, LOOP_MIN_COUNT, rdev, split_rdev as split};
use crate::vfs::store::{Ino, mode};
use crate::vfs::{DEV_MOUNT, Node};

/// Device numbers stop at `MINORBITS` (20 bits).
const MAX_DEVICES: u32 = 1 << 20;

const LO_NAME_SIZE: usize = 64;
const LO_KEY_SIZE: u32 = 32;
const LO_FLAGS_READ_ONLY: u32 = 1;
const LO_FLAGS_AUTOCLEAR: u32 = 4;
const LO_FLAGS_PARTSCAN: u32 = 8;
const LO_FLAGS_DIRECT_IO: u32 = 16;
const SET_STATUS_SETTABLE: u32 = LO_FLAGS_AUTOCLEAR | LO_FLAGS_PARTSCAN;
const SET_STATUS_CLEARABLE: u32 = LO_FLAGS_AUTOCLEAR;
const CONFIGURE_SETTABLE: u32 =
    LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR | LO_FLAGS_PARTSCAN | LO_FLAGS_DIRECT_IO;

const LOOP_SET_FD: u32 = 0x4c00;
const LOOP_CLR_FD: u32 = 0x4c01;
const LOOP_SET_STATUS: u32 = 0x4c02;
const LOOP_GET_STATUS: u32 = 0x4c03;
const LOOP_SET_STATUS64: u32 = 0x4c04;
const LOOP_GET_STATUS64: u32 = 0x4c05;
const LOOP_CHANGE_FD: u32 = 0x4c06;
const LOOP_SET_CAPACITY: u32 = 0x4c07;
const LOOP_SET_DIRECT_IO: u32 = 0x4c08;
const LOOP_SET_BLOCK_SIZE: u32 = 0x4c09;
const LOOP_CONFIGURE: u32 = 0x4c0a;
const LOOP_CTL_ADD: u32 = 0x4c80;
const LOOP_CTL_REMOVE: u32 = 0x4c81;
const LOOP_CTL_GET_FREE: u32 = 0x4c82;

const HDIO_GETGEO: u32 = 0x0301;
const BLKROSET: u32 = 0x125d;
const BLKROGET: u32 = 0x125e;
const BLKRRPART: u32 = 0x125f;
const BLKGETSIZE: u32 = 0x1260;
const BLKFLSBUF: u32 = 0x1261;
const BLKRASET: u32 = 0x1262;
const BLKRAGET: u32 = 0x1263;
const BLKFRASET: u32 = 0x1264;
const BLKFRAGET: u32 = 0x1265;
const BLKSECTGET: u32 = 0x1267;
const BLKSSZGET: u32 = 0x1268;
const BLKPG: u32 = 0x1269;
const BLKBSZGET: u32 = 0x8008_1270;
const BLKBSZSET: u32 = 0x4008_1271;
const BLKGETSIZE64: u32 = 0x8008_1272;
const BLKDISCARD: u32 = 0x1277;
const BLKIOMIN: u32 = 0x1278;
const BLKIOOPT: u32 = 0x1279;
const BLKALIGNOFF: u32 = 0x127a;
const BLKPBSZGET: u32 = 0x127b;
const BLKDISCARDZEROES: u32 = 0x127c;
const BLKSECDISCARD: u32 = 0x127d;
const BLKROTATIONAL: u32 = 0x127e;
const BLKZEROOUT: u32 = 0x127f;
const BLKGETDISKSEQ: u32 = 0x8008_1280;

/// The page size, the largest logical block size.
const PAGE: u32 = 4096;
/// `BLK_DEF_MAX_SECTORS` (6.1), the loop queue's largest request (`BLKSECTGET`).
const MAX_SECTORS: u16 = 2560;
/// The default read-ahead, 128 KiB (`VM_READAHEAD_PAGES`), in pages.
const READ_AHEAD: u64 = 32;

const SEEK_SET: u32 = 0;
const SEEK_CUR: u32 = 1;
const SEEK_END: u32 = 2;

// ---- Opening ----

/// Opens a block device node (Linux's `blkdev_open`). Only loop devices exist; a loop device
/// that does not exist yet is created, as Linux's legacy autoload probe does. An `O_EXCL` open
/// claims the device and is EBUSY while another open holds it.
pub(super) fn open(
    cx: &mut Cx<'_>,
    ino: Ino,
    rdev_value: u64,
    flags: u32,
) -> Result<FileKind, Errno> {
    let (major, minor) = split(rdev_value);
    // Block devices other than loop devices have no driver (Linux's `blkdev_get_no_open`)
    if major != LOOP_MAJOR {
        crate::stub::hit(cx, crate::stub::Stub::Device(true, major, minor));
        return Err(Errno::ENXIO);
    }
    if minor >= MAX_DEVICES {
        return Err(Errno::ENXIO);
    }
    let dev = match cx.kr().loops.devices.get(&minor) {
        Some(d) => d.clone(),
        None => add(cx, minor)?,
    };
    let excl = flags & oflags::O_EXCL != 0;
    {
        let mut l = dev.borrow_mut();
        if excl && l.claimed {
            return Err(Errno::EBUSY);
        }
        // The first opener sets the soft block size (`set_init_blocksize`): the largest power
        // of two up to a page that divides the size, starting from the logical block size
        if l.opens == 0 {
            let mut bsize = l.block_size;
            while bsize < PAGE && l.size & u64::from(bsize) == 0 {
                bsize <<= 1;
            }
            l.soft_block = bsize;
        }
        l.opens += 1;
        l.claimed |= excl;
    }
    Ok(FileKind::Device {
        ino,
        dev: Device::Loop,
        kmsg: None,
        block: Some(LoopOpen { dev, excl }),
    })
}

// ---- Reading and writing ----

/// Reads from a loop device (Linux's `blkdev_read_iter`): nothing at or past the end of the
/// device (an unbound device has size 0), otherwise from the backing file at the device offset.
/// Past the end of a backing file that has shrunk since the device was bound, reads return zeros,
/// as `lo_read_simple` does. With `offset` None the file position is used and advanced (block
/// devices have no position lock).
pub(super) fn read(
    cx: &mut Cx<'_>,
    file: &FileRef,
    dev: &LoopRef,
    buf: &mut [u8],
    offset: Option<u64>,
) -> Result<usize, Errno> {
    let pos = offset.unwrap_or_else(|| file.borrow().pos);
    let (backing, at, want) = {
        let l = dev.borrow();
        let Some(backing) = l
            .backing
            .clone()
            .filter(|_| pos < l.size && !buf.is_empty())
        else {
            return Ok(0);
        };
        let want = buf
            .len()
            .min(usize::try_from(l.size - pos).unwrap_or(usize::MAX));
        (backing, l.offset + pos, want)
    };
    let mut done = 0;
    while done < want {
        let n = super::fs::read_bytes(cx, &backing, &mut buf[done..want], Some(at + done as u64))
            .map_err(|_| Errno::EIO)?;
        if n == 0 {
            buf[done..want].fill(0);
            done = want;
        }
        done += n;
    }
    if offset.is_none() {
        file.borrow_mut().pos = pos + want as u64;
    }
    Ok(want)
}

/// Writes to a loop device (Linux's `blkdev_write_iter`): EPERM on a read-only device, ENOSPC at
/// or past its end (an unbound device has size 0), a short write at the end; `O_APPEND` is
/// ignored, as for every block device. With `offset` None the file position is used and
/// advanced.
pub(super) fn write(
    cx: &mut Cx<'_>,
    file: &FileRef,
    dev: &LoopRef,
    data: &[u8],
    offset: Option<u64>,
) -> Result<usize, Errno> {
    let pos = offset.unwrap_or_else(|| file.borrow().pos);
    let (backing, at, want) = {
        let l = dev.borrow();
        if l.read_only() {
            return Err(Errno::EPERM);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let Some(backing) = l.backing.clone().filter(|_| pos < l.size) else {
            return Err(Errno::ENOSPC);
        };
        let want = data
            .len()
            .min(usize::try_from(l.size - pos).unwrap_or(usize::MAX));
        (backing, l.offset + pos, want)
    };
    let mut done = 0;
    while done < want {
        let n = super::fs::write_bytes(cx, &backing, &data[done..want], Some(at + done as u64))
            .map_err(|_| Errno::EIO)?;
        if n == 0 {
            return Err(Errno::EIO);
        }
        done += n;
    }
    if offset.is_none() {
        file.borrow_mut().pos = pos + want as u64;
    }
    Ok(want)
}

/// lseek on a loop device (`blkdev_llseek`, `fixed_size_llseek`): positions run from 0 to the
/// device size, and only SEEK_SET, SEEK_CUR and SEEK_END are accepted.
pub(super) fn seek(dev: &LoopRef, pos: &mut u64, offset: i64, whence: u32) -> SysResult {
    let size = dev.borrow().size as i64;
    let new = match whence {
        SEEK_SET => offset,
        SEEK_CUR if offset == 0 => return Ok(*pos),
        SEEK_CUR => (*pos as i64).checked_add(offset).ok_or(Errno::EINVAL)?,
        SEEK_END => size.checked_add(offset).ok_or(Errno::EINVAL)?,
        _ => return Err(Errno::EINVAL),
    };
    if new < 0 || new > size {
        return Err(Errno::EINVAL);
    }
    *pos = new as u64;
    Ok(new as u64)
}

/// The backing file fsync on a loop device syncs (`blkdev_fsync` flushes the device, and the
/// loop driver turns the flush into an fsync of the backing file). None when there is nothing to
/// flush: only a bound, writable device has a write cache.
pub(super) fn flush_target(dev: &LoopRef) -> Option<FileRef> {
    let l = dev.borrow();
    l.backing
        .clone()
        .filter(|_| l.flags & LO_FLAGS_READ_ONLY == 0)
}

// ---- ioctl ----

/// ioctl on a loop device: the block-device ioctls first (`blkdev_ioctl`), then the loop
/// driver's (`lo_ioctl`); anything else is EINVAL, as the loop driver answers. `writable` is the
/// open file's write mode (Linux's `FMODE_WRITE`) and `excl` whether it was opened `O_EXCL`.
pub(super) fn ioctl(cx: &mut Cx<'_>, file: &FileRef, request: u32, arg: u64) -> SysResult {
    let (dev, writable, excl) = {
        let f = file.borrow();
        let FileKind::Device {
            block: Some(open), ..
        } = &f.kind
        else {
            return Err(Errno::ENOTTY);
        };
        (open.dev.clone(), f.writable(), open.excl)
    };
    let admin = cx.cred().capable(cap::SYS_ADMIN);
    let done = |r: Result<(), Errno>| r.map(|()| 0);
    match request {
        // The loop driver has no geometry
        HDIO_GETGEO if arg == 0 => Err(Errno::EINVAL),
        HDIO_GETGEO => Err(Errno::ENOTTY),
        // Partitions are disabled on loop devices (`GENHD_FL_NO_PART`); struct blkpg_ioctl_arg is
        // read first
        BLKPG => {
            let _: [u8; 24] = get(cx, arg)?;
            Err(if admin { Errno::EINVAL } else { Errno::EACCES })
        }
        BLKRAGET | BLKFRAGET => {
            if arg == 0 {
                return Err(Errno::EINVAL);
            }
            // In sectors, from whole pages
            let sectors = dev.borrow().read_ahead.wrapping_mul(u64::from(PAGE)) / 512;
            done(put(cx, arg, sectors.to_le_bytes()))
        }
        BLKGETSIZE => done(put(cx, arg, (dev.borrow().size >> 9).to_le_bytes())),
        BLKBSZGET => done(put(cx, arg, dev.borrow().soft_block.to_le_bytes())),
        BLKBSZSET => {
            if !admin {
                return Err(Errno::EACCES);
            }
            if arg == 0 {
                return Err(Errno::EINVAL);
            }
            let n = u32::from_le_bytes(get(cx, arg)?);
            let mut l = dev.borrow_mut();
            // Without O_EXCL the device is claimed for the call (EBUSY when someone holds it)
            if !excl && l.claimed {
                return Err(Errno::EBUSY);
            }
            if valid_block_size(n).is_err() || n < l.block_size {
                return Err(Errno::EINVAL);
            }
            l.soft_block = n;
            Ok(0)
        }
        BLKGETSIZE64 => done(put(cx, arg, dev.borrow().size.to_le_bytes())),
        // fsync_bdev and invalidate_bdev: there is no cache to write back or drop
        BLKFLSBUF => {
            if !admin {
                return Err(Errno::EACCES);
            }
            Ok(0)
        }
        BLKROSET => {
            if !admin {
                return Err(Errno::EACCES);
            }
            let n = i32::from_le_bytes(get(cx, arg)?);
            dev.borrow_mut().read_only = n != 0;
            Ok(0)
        }
        // No discard: the device advertises none, as the loop driver does over a backing file
        // without fallocate
        BLKDISCARD | BLKSECDISCARD => {
            if !writable {
                return Err(Errno::EBADF);
            }
            Err(Errno::EOPNOTSUPP)
        }
        BLKZEROOUT => {
            if !writable {
                return Err(Errno::EBADF);
            }
            let range: [u8; 16] = get(cx, arg)?;
            let start = u64::from_le_bytes(range[..8].try_into().unwrap());
            let len = u64::from_le_bytes(range[8..].try_into().unwrap());
            zero_out(cx, &dev, start, len)
        }
        BLKGETDISKSEQ => done(put(cx, arg, dev.borrow().seq.to_le_bytes())),
        BLKROGET => done(put(
            cx,
            arg,
            i32::from(dev.borrow().read_only()).to_le_bytes(),
        )),
        BLKSSZGET => done(put(cx, arg, dev.borrow().block_size.to_le_bytes())),
        BLKPBSZGET | BLKIOMIN => done(put(cx, arg, dev.borrow().block_size.to_le_bytes())),
        BLKIOOPT | BLKALIGNOFF | BLKDISCARDZEROES => done(put(cx, arg, 0u32.to_le_bytes())),
        BLKSECTGET => done(put(cx, arg, MAX_SECTORS.to_le_bytes())),
        BLKROTATIONAL => done(put(
            cx,
            arg,
            u16::from(dev.borrow().rotational).to_le_bytes(),
        )),
        BLKRASET | BLKFRASET => {
            if !admin {
                return Err(Errno::EACCES);
            }
            // Kept in whole pages, as `ra_pages`
            dev.borrow_mut().read_ahead = arg.wrapping_mul(512) / u64::from(PAGE);
            Ok(0)
        }
        BLKRRPART => Err(if admin { Errno::EINVAL } else { Errno::EACCES }),
        LOOP_SET_FD => configure(cx, &dev, (writable, excl), arg as i32, 0, Info::default()),
        LOOP_CONFIGURE => {
            let config: [u8; CONFIG_SIZE] = get(cx, arg)?;
            let fd = u32::from_le_bytes(config[..4].try_into().unwrap()) as i32;
            let block_size = u32::from_le_bytes(config[4..8].try_into().unwrap());
            let info = Info::decode64(&config[8..8 + INFO64_SIZE]);
            configure(cx, &dev, (writable, excl), fd, block_size, info)
        }
        LOOP_CHANGE_FD => change_fd(cx, &dev, arg as i32),
        LOOP_CLR_FD => {
            let backing = {
                let mut l = dev.borrow_mut();
                if l.backing.is_none() {
                    return Err(Errno::ENXIO);
                }
                // Others still have it open: it is cleared when the last one closes
                if l.opens > 1 {
                    l.flags |= LO_FLAGS_AUTOCLEAR;
                    return Ok(0);
                }
                l.clear()
            };
            drop(backing);
            Ok(0)
        }
        LOOP_SET_STATUS | LOOP_SET_STATUS64 => {
            if !writable && !admin {
                return Err(Errno::EPERM);
            }
            let info = if request == LOOP_SET_STATUS {
                let raw: [u8; INFO_SIZE] = get(cx, arg)?;
                Info::decode_old(&raw)
            } else {
                let raw: [u8; INFO64_SIZE] = get(cx, arg)?;
                Info::decode64(&raw)
            };
            set_status(&dev, &info)
        }
        LOOP_GET_STATUS | LOOP_GET_STATUS64 => {
            if arg == 0 {
                return Err(Errno::EINVAL);
            }
            let info = get_status(cx, &dev)?;
            if request == LOOP_GET_STATUS {
                let raw = info.encode_old()?;
                done(put(cx, arg, raw))
            } else {
                done(put(cx, arg, info.encode64()))
            }
        }
        LOOP_SET_CAPACITY | LOOP_SET_DIRECT_IO | LOOP_SET_BLOCK_SIZE if !writable && !admin => {
            Err(Errno::EPERM)
        }
        LOOP_SET_CAPACITY => {
            let backing = dev.borrow().backing.clone().ok_or(Errno::ENXIO)?;
            let len = backing_len(&backing)?;
            let mut l = dev.borrow_mut();
            l.size = device_size(len, l.offset, l.sizelimit);
            Ok(0)
        }
        // Direct I/O is never possible (`__loop_update_dio`): store objects sit behind the host's
        // page cache, so asking for it is EINVAL and turning it off succeeds
        LOOP_SET_DIRECT_IO => {
            if dev.borrow().backing.is_none() {
                return Err(Errno::ENXIO);
            }
            if arg != 0 {
                return Err(Errno::EINVAL);
            }
            Ok(0)
        }
        LOOP_SET_BLOCK_SIZE => {
            let mut l = dev.borrow_mut();
            if l.backing.is_none() {
                return Err(Errno::ENXIO);
            }
            let size = u32::try_from(arg).map_err(|_| Errno::EINVAL)?;
            valid_block_size(size)?;
            l.block_size = size;
            Ok(0)
        }
        // Everything else, including the zone, tracing and persistent-reservation ioctls of the
        // block layer, which Linux answers differently
        _ => Err(Errno::EINVAL),
    }
}

/// `LOOP_SET_FD` and `LOOP_CONFIGURE` (Linux's `loop_configure`): binds an unbound device to the
/// open file `fd`. The device is read-only when the file or the device's own open file is not
/// writable, or when asked; direct I/O is dropped (see `LOOP_SET_DIRECT_IO`), and so is any
/// partition scan, since loop devices have no partitions here.
fn configure(
    cx: &mut Cx<'_>,
    dev: &LoopRef,
    (writable, excl): (bool, bool),
    fd: i32,
    block_size: u32,
    info: Info,
) -> SysResult {
    let file = backing_fd(cx, fd)?;
    // Without O_EXCL, the device is claimed for the call (`bd_prepare_to_claim`)
    if !excl && dev.borrow().claimed {
        return Err(Errno::EBUSY);
    }
    if dev.borrow().backing.is_some() {
        return Err(Errno::EBUSY);
    }
    validate(dev, &file)?;
    if info.flags & !CONFIGURE_SETTABLE != 0 {
        return Err(Errno::EINVAL);
    }
    if block_size != 0 {
        valid_block_size(block_size)?;
    }
    info.check()?;
    let len = backing_len(&file)?;
    let file_writable = file.borrow().writable();
    let seq = cx.k().loops.next_seq();
    let mut l = dev.borrow_mut();
    l.offset = info.offset;
    l.sizelimit = info.sizelimit;
    l.file_name = info.file_name;
    l.flags = info.flags & !LO_FLAGS_DIRECT_IO;
    if !file_writable || !writable {
        l.flags |= LO_FLAGS_READ_ONLY;
    }
    l.disk_ro = l.flags & LO_FLAGS_READ_ONLY != 0;
    l.block_size = if block_size != 0 { block_size } else { 512 };
    l.rotational = false;
    l.size = device_size(len, l.offset, l.sizelimit);
    l.seq = seq;
    l.backing = Some(file);
    Ok(0)
}

/// `LOOP_CHANGE_FD` (`loop_change_fd`): swaps the backing file of a bound read-only device for
/// another of the same size.
fn change_fd(cx: &mut Cx<'_>, dev: &LoopRef, fd: i32) -> SysResult {
    let file = backing_fd(cx, fd)?;
    let old = {
        let l = dev.borrow();
        let Some(old) = l.backing.clone() else {
            return Err(Errno::ENXIO);
        };
        if l.flags & LO_FLAGS_READ_ONLY == 0 {
            return Err(Errno::EINVAL);
        }
        old
    };
    validate(dev, &file)?;
    let (offset, sizelimit) = {
        let l = dev.borrow();
        (l.offset, l.sizelimit)
    };
    if device_size(backing_len(&file)?, offset, sizelimit)
        != device_size(backing_len(&old)?, offset, sizelimit)
    {
        return Err(Errno::EINVAL);
    }
    dev.borrow_mut().backing = Some(file);
    drop(old);
    Ok(0)
}

/// `LOOP_SET_STATUS` and `LOOP_SET_STATUS64` (`loop_set_status`): offset, size limit, name and
/// the flags that may change (autoclear can be set and cleared, partscan only set).
fn set_status(dev: &LoopRef, info: &Info) -> SysResult {
    let backing = dev.borrow().backing.clone().ok_or(Errno::ENXIO)?;
    info.check()?;
    let len = backing_len(&backing)?;
    let mut l = dev.borrow_mut();
    let size_changed = l.offset != info.offset || l.sizelimit != info.sizelimit;
    let prev = l.flags;
    l.offset = info.offset;
    l.sizelimit = info.sizelimit;
    l.file_name = info.file_name;
    l.flags = info.flags & SET_STATUS_SETTABLE
        | prev & !SET_STATUS_SETTABLE
        | prev & !SET_STATUS_CLEARABLE;
    if size_changed {
        l.size = device_size(len, l.offset, l.sizelimit);
    }
    Ok(0)
}

/// `LOOP_GET_STATUS` and `LOOP_GET_STATUS64` (`loop_get_status`): the device's settings, and the
/// device, inode and device number of the backing file as stat shows them.
fn get_status(cx: &Cx<'_>, dev: &LoopRef) -> Result<Info, Errno> {
    let (mut info, backing) = {
        let l = dev.borrow();
        let Some(backing) = l.backing.clone() else {
            return Err(Errno::ENXIO);
        };
        let info = Info {
            number: l.number,
            offset: l.offset,
            sizelimit: l.sizelimit,
            flags: l.flags,
            file_name: l.file_name,
            ..Info::default()
        };
        (info, backing)
    };
    let st = super::fs::file_stat(cx, &backing)?;
    info.device = u64::from_le_bytes(st[..8].try_into().unwrap());
    info.inode = u64::from_le_bytes(st[8..16].try_into().unwrap());
    info.rdevice = u64::from_le_bytes(st[32..40].try_into().unwrap());
    Ok(info)
}

/// `BLKZEROOUT` (`blk_ioctl_zeroout`): writes zeros over a range of whole sectors inside the
/// device.
fn zero_out(cx: &mut Cx<'_>, dev: &LoopRef, start: u64, len: u64) -> SysResult {
    let size = dev.borrow().size;
    let end = start.wrapping_add(len).wrapping_sub(1);
    if start & 511 != 0 || len & 511 != 0 || end >= size || end < start {
        return Err(Errno::EINVAL);
    }
    let backing = dev.borrow().backing.clone().ok_or(Errno::EIO)?;
    let base = dev.borrow().offset;
    let zeros = vec![0u8; IO_CHUNK];
    let mut pos = start;
    while pos <= end {
        let n = (end + 1 - pos).min(zeros.len() as u64) as usize;
        let k = super::fs::write_bytes(cx, &backing, &zeros[..n], Some(base + pos))
            .map_err(|_| Errno::EIO)?;
        if k == 0 {
            return Err(Errno::EIO);
        }
        pos += k as u64;
    }
    Ok(0)
}

/// ioctl on `/dev/loop-control` (`loop_control_ioctl`): add a device (a given number, or the
/// lowest free one for a negative number), remove an unbound device nobody has open, or find a
/// free device (adding one when all are bound). Other requests are ENOSYS.
pub(super) fn control_ioctl(cx: &mut Cx<'_>, request: u32, arg: u64) -> SysResult {
    // The argument is an unsigned long that the driver takes as an int
    let index = arg as i32;
    match request {
        LOOP_CTL_ADD => {
            let n = match u32::try_from(index) {
                Ok(n) if cx.kr().loops.devices.contains_key(&n) => return Err(Errno::EEXIST),
                Ok(n) if n >= MAX_DEVICES => return Err(Errno::EINVAL),
                Ok(n) => n,
                Err(_) => cx.kr().loops.lowest_unused(),
            };
            add(cx, n).map(|_| u64::from(n))
        }
        LOOP_CTL_REMOVE => {
            let n = u32::try_from(index).map_err(|_| Errno::EINVAL)?;
            let dev = cx
                .kr()
                .loops
                .devices
                .get(&n)
                .cloned()
                .ok_or(Errno::ENODEV)?;
            {
                let l = dev.borrow();
                if l.backing.is_some() || l.opens > 0 {
                    return Err(Errno::EBUSY);
                }
            }
            cx.k().loops.devices.remove(&n);
            remove_node(cx, n);
            Ok(0)
        }
        LOOP_CTL_GET_FREE => {
            let free = cx
                .kr()
                .loops
                .devices
                .iter()
                .find(|(_, d)| d.borrow().backing.is_none())
                .map(|(&n, _)| n);
            match free {
                Some(n) => Ok(u64::from(n)),
                None => {
                    let n = cx.kr().loops.lowest_unused();
                    add(cx, n).map(|_| u64::from(n))
                }
            }
        }
        _ => Err(Errno::ENOSYS),
    }
}

// ---- Devices ----

/// The loop devices of a kernel, by number.
pub(crate) struct Loops {
    devices: BTreeMap<u32, LoopRef>,
    /// The last disk sequence number handed out (Linux's global `diskseq`, which also advances
    /// each time a device is bound).
    seq: u64,
}

impl Loops {
    /// The open files the bound devices read and write.
    pub fn backing_files(&self) -> Vec<FileRef> {
        self.devices
            .values()
            .filter_map(|l| l.borrow().backing.clone())
            .collect()
    }

    /// The devices that exist from boot, `loop0` to `loop7`.
    pub(crate) fn new() -> Loops {
        let mut loops = Loops {
            devices: BTreeMap::new(),
            seq: 0,
        };
        for n in 0..LOOP_MIN_COUNT {
            loops.insert(n);
        }
        loops
    }

    fn insert(&mut self, n: u32) -> LoopRef {
        let seq = self.next_seq();
        let dev = Rc::new(RefCell::new(Loop::new(n, seq)));
        self.devices.insert(n, dev.clone());
        dev
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// The device numbers, in order (the directories of `/sys/block`).
    pub(crate) fn numbers(&self) -> Vec<u32> {
        self.devices.keys().copied().collect()
    }

    /// Whether device `n` exists and, for the files of the `loop` directory, is bound: as in
    /// Linux, that directory is added when a device is bound and removed when it is cleared.
    pub(crate) fn sysfs_exists(&self, n: u32, path: &[u8]) -> bool {
        self.devices
            .get(&n)
            .is_some_and(|d| !path.starts_with(b"loop/") || d.borrow().backing.is_some())
    }

    /// The content of a file under `/sys/block/loop<n>` (paths as in the procfs map's sysfs
    /// table), as the block layer and the loop driver show them; None for a directory or a file
    /// that does not exist.
    pub(crate) fn sysfs_read(&self, n: u32, path: &[u8]) -> Option<String> {
        if !self.sysfs_exists(n, path) {
            return None;
        }
        let l = self.devices.get(&n)?.borrow();
        let bs = l.block_size;
        Some(match path {
            b"dev" => format!("{LOOP_MAJOR}:{n}\n"),
            b"ro" => format!("{}\n", u8::from(l.read_only())),
            b"size" => format!("{}\n", l.size >> 9),
            b"removable" => "0\n".to_owned(),
            b"queue/logical_block_size"
            | b"queue/physical_block_size"
            | b"queue/minimum_io_size"
            | b"queue/hw_sector_size" => format!("{bs}\n"),
            b"queue/optimal_io_size" => "0\n".to_owned(),
            b"queue/dma_alignment" => "511\n".to_owned(),
            b"queue/rotational" => format!("{}\n", u8::from(l.rotational)),
            b"queue/read_ahead_kb" => format!("{}\n", l.read_ahead * u64::from(PAGE) / 1024),
            b"queue/max_sectors_kb" => format!("{}\n", MAX_SECTORS / 2),
            b"queue/rq_affinity" => "1\n".to_owned(),
            // The path the backing file was opened by, as d_path shows it
            b"loop/backing_file" => {
                let backing = l.backing.as_ref()?.borrow();
                let p = backing.path.as_deref().unwrap_or(b"");
                format!("{}\n", String::from_utf8_lossy(p))
            }
            b"loop/offset" => format!("{}\n", l.offset),
            b"loop/sizelimit" => format!("{}\n", l.sizelimit),
            b"loop/autoclear" => format!("{}\n", u8::from(l.flags & LO_FLAGS_AUTOCLEAR != 0)),
            b"loop/partscan" => format!("{}\n", u8::from(l.flags & LO_FLAGS_PARTSCAN != 0)),
            b"loop/dio" => "0\n".to_owned(),
            _ => return None,
        })
    }

    /// The lowest number without a device (`idr_alloc` from 0).
    fn lowest_unused(&self) -> u32 {
        (0..MAX_DEVICES)
            .find(|n| !self.devices.contains_key(n))
            .unwrap_or(MAX_DEVICES)
    }
}

/// Adds loop device `n` and its node `/dev/loop<n>`.
fn add(cx: &mut Cx<'_>, n: u32) -> Result<LoopRef, Errno> {
    if n >= MAX_DEVICES {
        return Err(Errno::ENOSPC);
    }
    let dev = cx.k().loops.insert(n);
    add_node(cx, n);
    Ok(dev)
}

/// Creates `/dev/loop<n>` in the boot `/dev` for a new device, as devtmpfs does (0660, group
/// disk, like udev's and mdev's rules); nothing when `/dev` is not mounted or the name is taken.
fn add_node(cx: &Cx<'_>, n: u32) {
    let Ok(vfs) = cx.vfs() else {
        return;
    };
    let Some(Node::Store(_, dir)) = vfs.mount(DEV_MOUNT).map(|m| m.root) else {
        return;
    };
    let name = format!("loop{n}");
    let _ = vfs.store().mknod(
        dir,
        name.as_bytes(),
        mode::S_IFBLK | 0o660,
        rdev(LOOP_MAJOR, n),
        0,
        DISK_GID,
    );
}

/// Removes `/dev/loop<n>` from the boot `/dev` when it is that device's node (devtmpfs removes
/// only the node it created).
fn remove_node(cx: &Cx<'_>, n: u32) {
    let Ok(vfs) = cx.vfs() else {
        return;
    };
    let Some(Node::Store(_, dir)) = vfs.mount(DEV_MOUNT).map(|m| m.root) else {
        return;
    };
    let name = format!("loop{n}");
    let store = vfs.store();
    let ours = store
        .lookup(dir, name.as_bytes())
        .ok()
        .flatten()
        .and_then(|ino| store.stat(ino).ok())
        .is_some_and(|st| st.kind() == mode::S_IFBLK && st.rdev == rdev(LOOP_MAJOR, n));
    if ours {
        let _ = store.unlink(dir, name.as_bytes());
    }
}

pub(crate) type LoopRef = Rc<RefCell<Loop>>;

/// A loop device (Linux's `struct loop_device` with the state of its disk).
pub(crate) struct Loop {
    number: u32,
    /// Bound to this open file (`Lo_bound`), or unbound.
    backing: Option<FileRef>,
    offset: u64,
    /// 0 is no limit.
    sizelimit: u64,
    /// The device size in bytes, a multiple of 512.
    size: u64,
    flags: u32,
    file_name: [u8; LO_NAME_SIZE],
    /// The logical block size, which is also the physical block size and the minimum I/O.
    block_size: u32,
    /// The soft block size (`BLKBSZGET`, the bdev inode's block size).
    soft_block: u32,
    /// `BLKROSET`.
    read_only: bool,
    /// The disk is read-only (`set_disk_ro`): set by each binding and, as in Linux, kept after the
    /// device is unbound.
    disk_ro: bool,
    /// Read-ahead in pages (`ra_pages`).
    read_ahead: u64,
    /// Rotational until first bound: the backing files are on the phone's flash.
    rotational: bool,
    /// Disk sequence number (`BLKGETDISKSEQ`).
    seq: u64,
    /// Open files of the device (`disk_openers`).
    opens: u32,
    /// An `O_EXCL` open holds the device.
    claimed: bool,
}

impl Loop {
    fn new(number: u32, seq: u64) -> Loop {
        Loop {
            number,
            backing: None,
            offset: 0,
            sizelimit: 0,
            size: 0,
            flags: 0,
            file_name: [0; LO_NAME_SIZE],
            block_size: 512,
            soft_block: 512,
            read_only: false,
            disk_ro: false,
            read_ahead: READ_AHEAD,
            rotational: true,
            seq,
            opens: 0,
            claimed: false,
        }
    }

    /// `bdev_read_only`: `BLKROSET` or a read-only disk.
    fn read_only(&self) -> bool {
        self.read_only || self.disk_ro
    }

    /// Unbinds the device (`__loop_clr_fd`) and returns the backing file, to be dropped once the
    /// device is no longer borrowed.
    fn clear(&mut self) -> Option<FileRef> {
        self.offset = 0;
        self.sizelimit = 0;
        self.size = 0;
        self.flags = 0;
        self.file_name = [0; LO_NAME_SIZE];
        self.block_size = 512;
        self.backing.take()
    }
}

/// An open of a loop device node, kept in its [`FileKind::Device`]: one opener while it lives, and
/// with `O_EXCL` the holder of the device. Its drop is Linux's `lo_release`: when the last opener
/// goes, a device marked `LO_FLAGS_AUTOCLEAR` is unbound.
pub(crate) struct LoopOpen {
    pub(crate) dev: LoopRef,
    excl: bool,
}

impl Drop for LoopOpen {
    fn drop(&mut self) {
        let backing = {
            let mut l = self.dev.borrow_mut();
            l.opens -= 1;
            if self.excl {
                l.claimed = false;
            }
            if l.opens == 0 && l.backing.is_some() && l.flags & LO_FLAGS_AUTOCLEAR != 0 {
                l.clear()
            } else {
                None
            }
        };
        drop(backing);
    }
}

// ---- Helpers ----

/// The open file to bind (`fget`, which does not accept `O_PATH` files).
fn backing_fd(cx: &Cx<'_>, fd: i32) -> Result<FileRef, Errno> {
    let file = cx.fds().get(fd)?;
    if file.borrow().flags & oflags::O_PATH != 0 {
        return Err(Errno::EBADF);
    }
    Ok(file)
}

/// `loop_validate_file`: the backing file must be a regular file or a bound loop device that does
/// not lead back to this device.
fn validate(dev: &LoopRef, file: &FileRef) -> Result<(), Errno> {
    let mut f = file.clone();
    loop {
        let next = match &f.borrow().kind {
            FileKind::Regular { .. } => return Ok(()),
            FileKind::Device {
                block: Some(open), ..
            } => {
                if Rc::ptr_eq(&open.dev, dev) {
                    return Err(Errno::EBADF);
                }
                open.dev.borrow().backing.clone().ok_or(Errno::EINVAL)?
            }
            _ => return Err(Errno::EINVAL),
        };
        f = next;
    }
}

/// The length of a backing file (`i_size_read` of its inode): a store file's length, or a loop
/// device's size.
fn backing_len(file: &FileRef) -> Result<u64, Errno> {
    match &file.borrow().kind {
        FileKind::Regular { file, .. } => Ok(file.metadata().map_err(|_| Errno::EIO)?.len()),
        FileKind::Device {
            block: Some(open), ..
        } => Ok(open.dev.borrow().size),
        _ => Err(Errno::EINVAL),
    }
}

/// The device size (`get_size`): the backing file's length past the offset, limited by the size
/// limit, in whole sectors.
fn device_size(len: u64, offset: u64, sizelimit: u64) -> u64 {
    let mut size = len.saturating_sub(offset);
    if sizelimit > 0 && sizelimit < size {
        size = sizelimit;
    }
    size & !511
}

/// `blk_validate_block_size`: a power of two from 512 to a page.
fn valid_block_size(size: u32) -> Result<(), Errno> {
    if (512..=PAGE).contains(&size) && size.is_power_of_two() {
        Ok(())
    } else {
        Err(Errno::EINVAL)
    }
}

/// `struct loop_info` (old) and `struct loop_info64` on arm64.
const INFO_SIZE: usize = 160;
const INFO64_SIZE: usize = 232;
const CONFIG_SIZE: usize = 304;

/// The settings `LOOP_SET_STATUS`, `LOOP_GET_STATUS` and `LOOP_CONFIGURE` carry, as
/// `struct loop_info64`; the old `struct loop_info` converts to and from it.
#[derive(Clone, Copy)]
struct Info {
    device: u64,
    inode: u64,
    rdevice: u64,
    offset: u64,
    sizelimit: u64,
    number: u32,
    encrypt_type: u32,
    encrypt_key_size: u32,
    flags: u32,
    file_name: [u8; LO_NAME_SIZE],
}

impl Default for Info {
    fn default() -> Info {
        Info {
            device: 0,
            inode: 0,
            rdevice: 0,
            offset: 0,
            sizelimit: 0,
            number: 0,
            encrypt_type: 0,
            encrypt_key_size: 0,
            flags: 0,
            file_name: [0; LO_NAME_SIZE],
        }
    }
}

impl Info {
    fn decode64(b: &[u8]) -> Info {
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let mut file_name = [0; LO_NAME_SIZE];
        file_name.copy_from_slice(&b[56..56 + LO_NAME_SIZE]);
        file_name[LO_NAME_SIZE - 1] = 0;
        Info {
            device: u64_at(0),
            inode: u64_at(8),
            rdevice: u64_at(16),
            offset: u64_at(24),
            sizelimit: u64_at(32),
            number: u32_at(40),
            encrypt_type: u32_at(44),
            encrypt_key_size: u32_at(48),
            flags: u32_at(52),
            file_name,
        }
    }

    /// `loop_info64_from_old`: the old structure carries no size limit, and its encryption fields
    /// are not taken.
    fn decode_old(b: &[u8]) -> Info {
        let i32_at = |o: usize| i32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let mut file_name = [0; LO_NAME_SIZE];
        file_name.copy_from_slice(&b[36..36 + LO_NAME_SIZE]);
        file_name[LO_NAME_SIZE - 1] = 0;
        Info {
            device: u64::from(u32_at(4)),
            inode: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            rdevice: u64::from(u32_at(16)),
            // A negative int offset becomes a huge one, refused by check
            offset: i64::from(i32_at(20)) as u64,
            sizelimit: 0,
            number: i32_at(0) as u32,
            encrypt_type: 0,
            encrypt_key_size: 0,
            flags: i32_at(32) as u32,
            file_name,
        }
    }

    /// `loop_set_status_from_info`'s checks: no encryption, and offsets that fit a signed 64-bit
    /// file offset.
    fn check(&self) -> Result<(), Errno> {
        if self.encrypt_key_size > LO_KEY_SIZE || self.encrypt_type != 0 {
            return Err(Errno::EINVAL);
        }
        if self.offset > i64::MAX as u64 || self.sizelimit > i64::MAX as u64 {
            return Err(Errno::EOVERFLOW);
        }
        Ok(())
    }

    fn encode64(&self) -> [u8; INFO64_SIZE] {
        let mut b = [0u8; INFO64_SIZE];
        b[0..8].copy_from_slice(&self.device.to_le_bytes());
        b[8..16].copy_from_slice(&self.inode.to_le_bytes());
        b[16..24].copy_from_slice(&self.rdevice.to_le_bytes());
        b[24..32].copy_from_slice(&self.offset.to_le_bytes());
        b[32..40].copy_from_slice(&self.sizelimit.to_le_bytes());
        b[40..44].copy_from_slice(&self.number.to_le_bytes());
        b[52..56].copy_from_slice(&self.flags.to_le_bytes());
        b[56..56 + LO_NAME_SIZE].copy_from_slice(&self.file_name);
        b
    }

    /// `loop_info64_to_old`: EOVERFLOW when a field does not fit the old structure.
    fn encode_old(&self) -> Result<[u8; INFO_SIZE], Errno> {
        let device = u32::try_from(self.device).map_err(|_| Errno::EOVERFLOW)?;
        let rdevice = u32::try_from(self.rdevice).map_err(|_| Errno::EOVERFLOW)?;
        let offset = i32::try_from(self.offset).map_err(|_| Errno::EOVERFLOW)?;
        let mut b = [0u8; INFO_SIZE];
        b[0..4].copy_from_slice(&self.number.to_le_bytes());
        b[4..8].copy_from_slice(&device.to_le_bytes());
        b[8..16].copy_from_slice(&self.inode.to_le_bytes());
        b[16..20].copy_from_slice(&rdevice.to_le_bytes());
        b[20..24].copy_from_slice(&offset.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..36 + LO_NAME_SIZE].copy_from_slice(&self.file_name);
        Ok(b)
    }
}
