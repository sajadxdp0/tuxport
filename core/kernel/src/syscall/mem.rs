//! Memory syscalls (the kernel/mm map; process_mrelease: the kernel/oom map).

use std::os::fd::AsRawFd;

use tuxport_engine::{PAGE_SIZE, prot};

use super::SysResult;
use crate::cred::cap;
use crate::errno::Errno;
use crate::fd::{FileKind, OpenFile};
use crate::kernel::Cx;
use crate::mm::flags;
use crate::process::RLIMIT_MEMLOCK;
use crate::vfs::dev::Device;
use crate::vfs::ms;

const PROT_MASK: u32 = prot::READ | prot::WRITE | prot::EXEC;
/// `PROT_GROWSDOWN`, `PROT_GROWSUP`: mmap ignores them as Linux does; mprotect with
/// `PROT_GROWSDOWN` extends the range to the bottom of a grows-down mapping ([`Mm::protect`]).
///
/// [`Mm::protect`]: crate::mm::Mm::protect
const PROT_GROWS: u32 = 0x0100_0000 | 0x0200_0000;
/// Mapping type bits: `MAP_SHARED`, `MAP_PRIVATE`, `MAP_SHARED_VALIDATE`.
const MAP_TYPE: u32 = 0x0f;
const MAP_SHARED_VALIDATE: u32 = 0x03;
/// Flags MAP_SHARED_VALIDATE recognises (Linux's `LEGACY_MAP_MASK` plus `MAP_FIXED_NOREPLACE`,
/// `MAP_UNINITIALIZED`, and the page-size bits of MAP_HUGETLB).
const MAP_VALIDATE_KNOWN: u32 = MAP_TYPE
    | flags::MAP_FIXED
    | flags::MAP_ANONYMOUS
    | 0x0100 // MAP_GROWSDOWN
    | 0x0800 // MAP_DENYWRITE
    | 0x1000 // MAP_EXECUTABLE
    | 0x2000 // MAP_LOCKED
    | 0x4000 // MAP_NORESERVE
    | 0x8000 // MAP_POPULATE
    | 0x1_0000 // MAP_NONBLOCK
    | 0x2_0000 // MAP_STACK
    | 0x4_0000 // MAP_HUGETLB
    | flags::MAP_FIXED_NOREPLACE
    | 0x400_0000 // MAP_UNINITIALIZED
    | 0xfc00_0000; // page-size bits above MAP_HUGE_SHIFT
const F_SEAL_WRITE: u32 = 0x8;
const F_SEAL_FUTURE_WRITE: u32 = 0x10;
const MADV_DONTNEED: i32 = 4;
const MADV_REMOVE: i32 = 9;
const MADV_DONTFORK: i32 = 10;
const MADV_DOFORK: i32 = 11;
const MADV_FREE: i32 = 8;
const MADV_DONTDUMP: i32 = 16;
const MADV_DODUMP: i32 = 17;
const MADV_WIPEONFORK: i32 = 18;
const MADV_KEEPONFORK: i32 = 19;
const MCL_CURRENT: u32 = 1;
const MCL_FUTURE: u32 = 2;
const MCL_ONFAULT: u32 = 4;

pub(super) fn mmap(
    cx: &mut Cx<'_>,
    addr: u64,
    len: u64,
    prot: u32,
    flags: u32,
    fd: i32,
    offset: u64,
) -> SysResult {
    // arm64's PROT_BTI and PROT_MTE are rejected here too: the guest does not see those features
    if prot & !(PROT_MASK | PROT_GROWS) != 0 {
        return Err(Errno::EINVAL);
    }
    let mut prot = prot & PROT_MASK;
    // READ_IMPLIES_EXEC: a readable mapping is executable too, unless it is a file on a noexec
    // mount (do_mmap)
    let implies_exec = prot & prot::READ != 0 && read_implies_exec(cx);
    if !offset.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    let kind = match flags & MAP_TYPE {
        // MAP_SHARED_VALIDATE requires every flag to be recognised (MAP_SYNC works only on DAX
        // files)
        MAP_SHARED_VALIDATE if flags & !MAP_VALIDATE_KNOWN != 0 => {
            return Err(Errno::EOPNOTSUPP);
        }
        MAP_SHARED_VALIDATE => flags::MAP_SHARED,
        k @ (flags::MAP_SHARED | flags::MAP_PRIVATE) => k,
        _ => return Err(Errno::EINVAL),
    };
    let mut flags = (flags & !MAP_TYPE) | kind;
    // Shared anonymous memory cannot grow down
    const MAP_GROWSDOWN: u32 = 0x0100;
    let growsdown = flags & MAP_GROWSDOWN != 0;
    if growsdown && flags & flags::MAP_ANONYMOUS != 0 && kind == flags::MAP_SHARED {
        return Err(Errno::EINVAL);
    }
    // A shared mapping of /dev/zero opened read-only: no write permission, now or later
    let mut deny_write = false;
    // The path and inode shown for a file mapping in /proc/<pid>/maps
    let mut label = None;
    let mut deny_exec = false;
    let low =
        flags & (flags::MAP_FIXED | flags::MAP_FIXED_NOREPLACE) != 0 && below_min_addr(cx, addr);
    // A FUSE file has no page cache to map: the mapping reads its pages into a host file as they
    // are touched (crate::mm::Fill)
    let mut fuse_fill = None;
    let file = if flags & flags::MAP_ANONYMOUS != 0 {
        if low && len != 0 {
            return Err(Errno::EPERM);
        }
        if implies_exec {
            prot |= prot::EXEC;
        }
        None
    } else {
        let file = cx.fds().get(fd)?;
        // Linux checks the fd first (EBADF), then the length (EINVAL), the address (EPERM), and
        // the file's permissions (EACCES) last
        if len == 0 {
            return Err(Errno::EINVAL);
        }
        if low {
            return Err(Errno::EPERM);
        }
        if implies_exec && !on_noexec(cx, &file.borrow()) {
            prot |= prot::EXEC;
        }
        // A shared mapping that can be made writable writes the file's object in place, and a
        // mapping does not follow a file to another object: a held file gets its own first
        if kind == flags::MAP_SHARED && file.borrow().readable() && file.borrow().writable() {
            super::fs::prepare_write(cx, &file, None)?;
        }
        // A shared writable mapping, or an executable one on a noexec mount, is refused below
        let refused = kind == flags::MAP_SHARED && prot & prot::WRITE != 0
            || on_noexec(cx, &file.borrow()) && prot & prot::EXEC != 0;
        fuse_fill = super::fuse::map_fill(cx, &file, kind == flags::MAP_SHARED, refused)?;
        let f = file.borrow();
        let shared_write = kind == flags::MAP_SHARED && prot & prot::WRITE != 0;
        match &f.kind {
            FileKind::Regular {
                file: host, ino, ..
            } => {
                if !f.readable() {
                    return Err(Errno::EACCES);
                }
                label = Some((f.path.clone(), *ino));
                if shared_write && !f.writable() {
                    return Err(Errno::EACCES);
                }
                // Files on a noexec mount cannot be mapped executable, now or later (Linux's
                // do_mmap clears VM_MAYEXEC)
                if on_noexec(cx, &f) {
                    if prot & prot::EXEC != 0 {
                        return Err(Errno::EPERM);
                    }
                    deny_exec = true;
                }
                // A memfd sealed against writes cannot have a writable shared mapping, now or later
                let sealed = cx
                    .kr()
                    .seals
                    .get(ino)
                    .is_some_and(|s| s & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0);
                if sealed && kind == flags::MAP_SHARED {
                    if prot & prot::WRITE != 0 {
                        return Err(Errno::EPERM);
                    }
                    Some((host.as_raw_fd(), offset, false))
                } else {
                    Some((host.as_raw_fd(), offset, f.writable()))
                }
            }
            // /dev/zero: a private mapping is anonymous memory, a shared one is shared anonymous
            // memory
            FileKind::Device {
                dev: Device::Zero, ..
            } => {
                if !f.readable() {
                    return Err(Errno::EACCES);
                }
                if kind == flags::MAP_SHARED && !f.writable() {
                    if prot & prot::WRITE != 0 {
                        return Err(Errno::EACCES);
                    }
                    deny_write = true;
                }
                flags |= flags::MAP_ANONYMOUS;
                None
            }
            // Read on demand into a host file: a shared mapping can never be written (its writes
            // would not reach the file), and noexec mounts refuse execution as for store files
            // In do_mmap's order: not readable, then shared writable (EACCES), then executable on
            // a noexec mount (EPERM); the fill was made only for a mapping passing them
            FileKind::Fuse(h) => {
                if !f.readable() || shared_write {
                    return Err(Errno::EACCES);
                }
                if on_noexec(cx, &f) {
                    if prot & prot::EXEC != 0 {
                        return Err(Errno::EPERM);
                    }
                    deny_exec = true;
                }
                let Some(fill) = &fuse_fill else {
                    return Err(Errno::EIO);
                };
                let ino = cx.vfs()?.fuse().get(h.id).map_or(0, |n| n.attr.ino);
                label = Some((f.path.clone(), ino));
                Some((fill.fd(), offset, false))
            }
            FileKind::Path(_) => return Err(Errno::EBADF),
            _ => return Err(Errno::ENODEV),
        }
    };
    const MAP_LOCKED: u32 = 0x2000;
    if flags & MAP_LOCKED != 0 {
        check_memlock(cx, len.next_multiple_of(PAGE_SIZE)).map_err(|_| Errno::EAGAIN)?;
    }
    let fill = fuse_fill;
    let start = match &fill {
        Some(fill) => cx
            .mm_mut()
            .mmap_fill(addr, len, prot, flags, (fill.clone(), offset))?,
        None => cx.mm_mut().mmap(addr, len, prot, flags, file)?,
    };
    // MAP_FIXED may have replaced a segment's last attachment
    if flags & flags::MAP_FIXED != 0 {
        crate::ipc::reap_shm(cx.k());
    }
    // MAP_GROWSDOWN on a file mapping does not grow (on Linux the grown part also maps the file,
    // and no program uses it that way)
    if growsdown && file.is_none() && kind == flags::MAP_PRIVATE {
        cx.mm_mut().set_growsdown(start);
    }
    if let Some((name, ino)) = label {
        cx.mm_mut().label(start, len, name, ino);
    }
    if deny_write {
        cx.mm_mut().deny_write(start, len);
    }
    if deny_exec {
        cx.mm_mut().deny_exec(start, len);
    }
    if flags & MAP_LOCKED != 0 {
        cx.mm_mut().set_locked(start, len, true)?;
    }
    // MAP_POPULATE (not with MAP_NONBLOCK), MAP_LOCKED and mlockall(MCL_FUTURE) all bring the
    // pages into memory
    const MAP_POPULATE: u32 = 0x8000;
    const MAP_NONBLOCK: u32 = 0x10000;
    let populate = flags & (MAP_POPULATE | MAP_NONBLOCK) == MAP_POPULATE
        || flags & MAP_LOCKED != 0
        || cx.mm().lock_future && !cx.mm().lock_onfault;
    if populate {
        cx.mm().populate(start, len);
        // A FUSE file's pages are read now, the BKL released (errors are not reported, as on
        // Linux)
        if fill.is_some() && prot != 0 {
            loop {
                let Some(w) = cx.mm().first_unread(start, len, 0) else {
                    break;
                };
                if super::fuse::fill(cx, w).is_err() {
                    break;
                }
            }
        }
    }
    Ok(start)
}

pub(super) fn mremap(
    cx: &mut Cx<'_>,
    old: u64,
    old_len: u64,
    new_len: u64,
    flags: u32,
    new_addr: u64,
) -> SysResult {
    let r = remap(cx, old, old_len, new_len, flags, new_addr);
    // What was moved or grown may be writable shared file mappings: the no-BKL futex must know
    // where they are now
    cx.mm().note_shared_maps(true);
    r
}

fn remap(
    cx: &mut Cx<'_>,
    old: u64,
    old_len: u64,
    new_len: u64,
    flags: u32,
    new_addr: u64,
) -> SysResult {
    const MREMAP_MAYMOVE: u32 = 1;
    const MREMAP_FIXED: u32 = 2;
    const MREMAP_DONTUNMAP: u32 = 4;
    if flags & !(MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP) != 0
        || flags & MREMAP_FIXED != 0 && flags & MREMAP_MAYMOVE == 0
        || flags & MREMAP_DONTUNMAP != 0 && (flags & MREMAP_MAYMOVE == 0 || old_len != new_len)
    {
        return Err(Errno::EINVAL);
    }
    let fixed = (flags & MREMAP_FIXED != 0).then_some(new_addr);
    if fixed.is_some_and(|a| below_min_addr(cx, a)) {
        return Err(Errno::EPERM);
    }
    if flags & MREMAP_DONTUNMAP != 0 {
        let (new, refill) = cx.mm_mut().mremap_dontunmap(old, old_len, fixed)?;
        // Map the store file again at the old address of a private file mapping; if it cannot be
        // opened (for example deleted and reclaimed), empty anonymous memory is all that is left
        let vfs = cx.vfs()?;
        for r in refill {
            let file = (r.ino != 0)
                .then(|| vfs.store().open_object(r.ino, false).ok())
                .flatten();
            let mut mm = cx.mm_mut();
            match &file {
                Some(f) => {
                    let flags = flags::MAP_PRIVATE | flags::MAP_FIXED;
                    let src = Some((f.as_raw_fd(), r.offset, false));
                    mm.mmap(r.start, r.len, r.prot, flags, src)?;
                    mm.label(r.start, r.len, r.name, r.ino);
                }
                None => mm.map_anonymous(r.start, r.len, r.prot)?,
            }
            mm.set_locked(r.start, r.len, false)?;
        }
        return Ok(new);
    }
    // Growing a private file mapping maps the rest of that store file, so open it first
    let ino = (new_len > old_len)
        .then(|| cx.mm().private_file_ino(old + old_len.max(1) - 1))
        .flatten();
    let file = match ino {
        Some(ino) => Some(cx.vfs()?.store().open_object(ino, false)?),
        None => None,
    };
    let r = cx.mm_mut().mremap(
        old,
        old_len,
        new_len,
        flags & MREMAP_MAYMOVE != 0,
        fixed,
        file.as_ref().map(|f| f.as_raw_fd()),
    );
    // A shrunk or replaced segment attachment may have been the last one
    crate::ipc::reap_shm(cx.k());
    r
}

/// Reads pieces of private file mappings from their files again after `MADV_DONTNEED`
/// ([`Mm::discard`](crate::mm::Mm::discard)): zeros past the end of the file, and zeros for all of
/// a piece whose file can no longer be opened (deleted and reclaimed).
fn reread(cx: &mut Cx<'_>, rereads: Vec<crate::mm::Reread>) -> Result<(), Errno> {
    if rereads.is_empty() {
        return Ok(());
    }
    let vfs = cx.vfs()?;
    for r in rereads {
        let mut buf = vec![0u8; r.len as usize];
        if let Some(f) = (r.ino != 0)
            .then(|| vfs.store().open_object(r.ino, false).ok())
            .flatten()
        {
            let mut at = 0;
            while at < buf.len() {
                match tuxport_hal::pread(f.as_raw_fd(), &mut buf[at..], r.offset + at as u64) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => at += n,
                }
            }
        }
        cx.mm_mut().fill_private(r.addr, &buf);
    }
    Ok(())
}

/// remap_file_pages: deprecated, and emulated with mmap as on Linux since 4.0
/// ([`Mm::remap_file_pages`]). `prot` must be 0, and of `flags` only `MAP_NONBLOCK` is looked
/// at.
///
/// [`Mm::remap_file_pages`]: crate::mm::Mm::remap_file_pages
pub(super) fn remap_file_pages(
    cx: &mut Cx<'_>,
    start: u64,
    size: u64,
    prot: u64,
    pgoff: u64,
    flags: u32,
) -> SysResult {
    const MAP_NONBLOCK: u32 = 0x10000;
    if prot != 0 {
        return Err(Errno::EINVAL);
    }
    let start = start & !(PAGE_SIZE - 1);
    let size = size & !(PAGE_SIZE - 1);
    let offset = pgoff
        .checked_add(size / PAGE_SIZE)
        .and(pgoff.checked_mul(PAGE_SIZE))
        .ok_or(Errno::EINVAL)?;
    cx.mm_mut().remap_file_pages(start, size, offset)?;
    if flags & MAP_NONBLOCK == 0 {
        cx.mm().populate(start, size);
    }
    Ok(0)
}

pub(super) fn mprotect(cx: &mut Cx<'_>, addr: u64, len: u64, prot: u32) -> SysResult {
    if prot & !(PROT_MASK | PROT_GROWS) != 0 || prot & PROT_GROWS == PROT_GROWS {
        return Err(Errno::EINVAL);
    }
    let implies_exec = read_implies_exec(cx);
    cx.mm_mut()
        .protect(addr, len, prot & PROT_MASK, prot & PROT_GROWS, implies_exec)
        .map(|()| 0)
}

/// The calling process has personality(2)'s `READ_IMPLIES_EXEC`: readable memory it maps
/// (mmap, mprotect, brk) is executable too where it may be.
pub(super) fn read_implies_exec(cx: &Cx<'_>) -> bool {
    cx.t.group.borrow().personality & crate::process::READ_IMPLIES_EXEC != 0
}

/// The open file is on a noexec mount (Linux's `path_noexec`): it can never be mapped
/// executable.
fn on_noexec(cx: &Cx<'_>, f: &OpenFile) -> bool {
    f.node()
        .and_then(|n| cx.vfs().ok()?.mount_flags(&n))
        .is_some_and(|m| m & ms::NOEXEC != 0)
}

/// Whether mapping at `addr` is refused: below `vm.mmap_min_addr` without `CAP_SYS_RAWIO`
/// (Linux's cap_mmap_addr, EPERM).
fn below_min_addr(cx: &Cx<'_>, addr: u64) -> bool {
    use std::sync::atomic::Ordering;
    addr < crate::mm::MMAP_MIN_ADDR.load(Ordering::Relaxed) && !cx.cred().capable(cap::SYS_RAWIO)
}

pub(super) fn madvise(cx: &mut Cx<'_>, addr: u64, len: u64, advice: i32) -> SysResult {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    let len = len
        .checked_next_multiple_of(PAGE_SIZE)
        .filter(|&l| addr.checked_add(l).is_some())
        .ok_or(Errno::EINVAL)?;
    // No KSM, transparent huge pages or memory fault injection (MERGEABLE, HUGEPAGE, COLLAPSE,
    // HWPOISON and so on)
    if !matches!(advice, 0..=4 | 8..=11 | 16..=24) {
        crate::stub::hit(cx, crate::stub::Stub::Madvise(advice));
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    if advice == MADV_REMOVE {
        return cx.mm_mut().remove(addr, len).map(|()| 0);
    }
    let (locked, private_anon) = cx.mm().advice_range(addr, len)?;
    match advice {
        MADV_DONTNEED if locked => Err(Errno::EINVAL),
        // The other advice only affects performance and can be ignored; after DONTNEED anonymous
        // memory must read back as 0, and private file mappings as the file
        MADV_DONTNEED => {
            let rereads = cx.mm_mut().discard(addr, len)?;
            reread(cx, rereads)?;
            Ok(0)
        }
        MADV_FREE | MADV_WIPEONFORK if !private_anon => Err(Errno::EINVAL),
        MADV_WIPEONFORK | MADV_KEEPONFORK => cx
            .mm_mut()
            .set_wipeonfork(addr, len, advice == MADV_WIPEONFORK)
            .map(|()| 0),
        MADV_DONTFORK | MADV_DOFORK => cx
            .mm_mut()
            .set_dontfork(addr, len, advice == MADV_DONTFORK)
            .map(|()| 0),
        // Whether it is written to core files (crate::coredump)
        MADV_DONTDUMP | MADV_DODUMP => cx
            .mm_mut()
            .set_dontdump(addr, len, advice == MADV_DONTDUMP)
            .map(|()| 0),
        _ => Ok(0),
    }
}

/// mlock, munlock, mlock2: records which mappings are locked (host memory is never swapped out
/// anywhere the guest could notice) and checks the range and `RLIMIT_MEMLOCK`. As on Linux,
/// locking brings the pages into memory, except with `MLOCK_ONFAULT`.
pub(super) fn mlock(cx: &mut Cx<'_>, addr: u64, len: u64, flags: u32, lock: bool) -> SysResult {
    const MLOCK_ONFAULT: u32 = 1;
    if flags & !MLOCK_ONFAULT != 0 {
        return Err(Errno::EINVAL);
    }
    // Linux's do_mlock and munlock: the length is page-aligned with wrapping arithmetic; after the
    // RLIMIT_MEMLOCK check, a range that wraps past the end of the address space is EINVAL and an
    // empty one succeeds
    let start = addr & !(PAGE_SIZE - 1);
    let span = len
        .wrapping_add(addr & (PAGE_SIZE - 1))
        .wrapping_add(PAGE_SIZE - 1)
        & !(PAGE_SIZE - 1);
    if lock {
        let already = cx.mm().locked_in(start, span);
        check_memlock(cx, span - already)?;
    }
    let end = start.wrapping_add(span);
    if end < start {
        return Err(Errno::EINVAL);
    }
    if end == start {
        return Ok(0);
    }
    cx.mm_mut().set_locked(start, span, lock)?;
    if lock && flags & MLOCK_ONFAULT == 0 {
        cx.mm().populate(start, span);
    }
    Ok(0)
}

pub(super) fn mlockall(cx: &mut Cx<'_>, flags: u32) -> SysResult {
    if flags == 0
        || flags & !(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT) != 0
        || flags & (MCL_CURRENT | MCL_FUTURE) == 0
    {
        return Err(Errno::EINVAL);
    }
    if flags & MCL_CURRENT != 0 {
        let mm = cx.mm();
        let more = mm.size() - mm.locked_bytes();
        drop(mm);
        check_memlock(cx, more)?;
        cx.mm_mut().set_all_locked(true);
        if flags & MCL_ONFAULT == 0 {
            cx.mm().populate(0, u64::MAX);
        }
    } else {
        check_memlock(cx, 0)?;
    }
    let mut mm = cx.mm_mut();
    mm.lock_future = flags & MCL_FUTURE != 0;
    mm.lock_onfault = flags & MCL_ONFAULT != 0;
    Ok(0)
}

pub(super) fn munlockall(cx: &mut Cx<'_>) -> SysResult {
    let mut mm = cx.mm_mut();
    mm.set_all_locked(false);
    mm.lock_future = false;
    mm.lock_onfault = false;
    Ok(0)
}

/// Without `CAP_IPC_LOCK`, locked memory may not exceed `RLIMIT_MEMLOCK` (nothing can be locked
/// when it is 0). `more` is the bytes newly locked by this call.
fn check_memlock(cx: &Cx<'_>, more: u64) -> Result<(), Errno> {
    if cx.cred().capable(cap::IPC_LOCK) {
        return Ok(());
    }
    let limit = cx.t.group.borrow().rlimits[RLIMIT_MEMLOCK as usize].0;
    if limit == 0 {
        return Err(Errno::EPERM);
    }
    if cx.mm().locked_bytes() + more > limit {
        return Err(Errno::ENOMEM);
    }
    Ok(())
}

/// msync: shared mappings are host file mappings themselves and need no write-back, so only the
/// arguments are checked.
pub(super) fn msync(cx: &mut Cx<'_>, addr: u64, len: u64, flags: u32) -> SysResult {
    const MS_ASYNC: u32 = 1;
    const MS_INVALIDATE: u32 = 2;
    const MS_SYNC: u32 = 4;
    if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0
        || flags & (MS_ASYNC | MS_SYNC) == MS_ASYNC | MS_SYNC
        || !addr.is_multiple_of(PAGE_SIZE)
    {
        return Err(Errno::EINVAL);
    }
    cx.mm().check_mapped(addr, len)?;
    // Locked pages cannot be invalidated
    if flags & MS_INVALIDATE != 0 && cx.mm().locked_in(addr, len) > 0 {
        return Err(Errno::EBUSY);
    }
    // A shared file mapping is the host's shared mapping of the object itself: MS_SYNC leaves the
    // write-back to the host and waits with the BKL released. If another thread tears down the
    // mapping meanwhile the host reports ENOMEM, which is ignored (unmapping already handed the
    // data to the host page cache)
    if flags & MS_SYNC != 0 {
        let ranges = cx.mm().shared_file_ranges(addr, len);
        if !ranges.is_empty() {
            cx.unlocked(move || {
                ranges
                    .into_iter()
                    .try_for_each(|(a, n)| match tuxport_hal::msync(a, n) {
                        Err(libc::ENOMEM) => Ok(()),
                        r => r,
                    })
            })
            .map_err(Errno)?;
        }
    }
    Ok(0)
}

pub(super) fn munmap(cx: &mut Cx<'_>, addr: u64, len: u64) -> SysResult {
    cx.mm_mut().munmap(addr, len)?;
    // A segment marked for removal goes with its last attachment, whatever detaches it
    crate::ipc::reap_shm(cx.k());
    Ok(0)
}

/// mincore: reports whether each page is resident, by host memory.
pub(super) fn mincore(cx: &mut Cx<'_>, addr: u64, len: u64, vec: u64) -> SysResult {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    let out: Vec<u8> = cx
        .mm()
        .residency(addr, len)?
        .into_iter()
        .map(u8::from)
        .collect();
    cx.mm()
        .write(crate::uaccess::UserSlice::new(vec, out.len()), &out)?;
    Ok(0)
}

/// process_mrelease (Linux 6.1's `mm/oom_kill.c`): frees the private memory of a process that is
/// exiting before its threads have all gone (lmkd and earlyoom call it right after SIGKILL). Flags
/// are `EINVAL`; anything but a pidfd is `EBADF` (a `/proc/<pid>` directory too, unlike
/// pidfd_send_signal); a process with no thread left holding its address space (a zombie, or
/// reaped) is `ESRCH` (`find_lock_task_mm`); unless it is exiting as a whole (exit_group, a fatal
/// signal, or its last thread's exit) and writing no core file, and so is every other process
/// sharing its address space (`task_will_free_mem`), `EINVAL`.
pub(super) fn process_mrelease(cx: &mut Cx<'_>, pidfd: i32, flags: u32) -> SysResult {
    use std::rc::Rc;
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let file = cx.fds().get(pidfd)?;
    let group = match &file.borrow().kind {
        FileKind::Pidfd(p) => p.group(cx.kr()).ok_or(Errno::ESRCH)?,
        _ => return Err(Errno::EBADF),
    };
    let k = cx.kr();
    let mm_of = |g: &crate::process::ThreadGroup| {
        g.threads
            .iter()
            .find_map(|t| k.tasks.get(t))
            .map(|t| t.res.borrow().mm.clone())
    };
    let mm = mm_of(&group.borrow()).ok_or(Errno::ESRCH)?;
    let dying = |g: &crate::process::ThreadGroup| g.exiting.is_some() && g.core.is_none();
    let sharers_dying = k
        .groups
        .values()
        .filter(|g| !Rc::ptr_eq(g, &group))
        .all(|g| {
            let g = g.borrow();
            mm_of(&g).is_none_or(|m| !Rc::ptr_eq(&m, &mm)) || dying(&g)
        });
    if !dying(&group.borrow()) || !sharers_dying {
        return Err(Errno::EINVAL);
    }
    // The peak resident memory (ru_maxrss of the process's wait) is taken before the memory goes
    group.borrow_mut().sample_rss(&mm.borrow());
    // No thread holds an address space borrowed across a BKL release, and the caller's own (the
    // target when another of its threads called exit_group) is not borrowed here: checked rather
    // than assumed
    if let Ok(mut m) = mm.try_borrow_mut() {
        m.release();
    }
    Ok(0)
}
