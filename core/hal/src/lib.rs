//! Host abstraction layer (design doc §3): keeps host system differences out of the kernel layer.
//!
//! The production host is Android (bionic) and the development host is Linux (glibc); both run
//! the Linux kernel, so there is only one implementation here, and no other host is supported.
//! The error codes returned to the kernel layer are host errnos, which match the guest's
//! numbering and are passed to the guest unchanged.

#[cfg(not(any(target_os = "linux", target_os = "android")))]
compile_error!(
    "tuxport runs only on Linux and Android hosts; on macOS, build and test in scripts/dev"
);

pub mod net;

use std::io;
use std::ptr::NonNull;
use std::time::Duration;

// ---- Memory ----

/// Allocates `len` bytes of zeroed, readable and writable anonymous host memory. With `reserve`
/// false it is not counted in the host's memory commit (`MAP_NORESERVE`), so overcommitting fails
/// on access rather than now.
pub fn map_anonymous(len: usize, reserve: bool) -> Result<NonNull<u8>, i32> {
    let noreserve = if reserve { 0 } else { libc::MAP_NORESERVE };
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | noreserve,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(last_errno());
    }
    Ok(NonNull::new(ptr.cast()).expect("mmap returned NULL"))
}

/// Allocates `len` bytes of inaccessible (`PROT_NONE`) host address space: accessing it raises
/// SIGSEGV on the host.
pub fn map_inaccessible(len: usize) -> Result<NonNull<u8>, i32> {
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(last_errno());
    }
    Ok(NonNull::new(ptr.cast()).expect("mmap returned NULL"))
}

/// Releases memory allocated by [`map_anonymous`].
///
/// # Safety
///
/// `ptr` and `len` must match the return value and argument of some [`map_anonymous`] call, and
/// the memory must not be accessed afterwards.
pub unsafe fn unmap(ptr: NonNull<u8>, len: usize) {
    unsafe { libc::munmap(ptr.as_ptr().cast(), len) };
}

/// Discards the contents of a piece of private anonymous memory (`MADV_DONTNEED`): the host
/// reclaims the pages, and later reads return zeros.
///
/// # Safety
///
/// `ptr` and `len` are aligned to host pages and lie within memory allocated by
/// [`map_anonymous`] and not yet released.
pub unsafe fn discard(ptr: NonNull<u8>, len: usize) -> Result<(), i32> {
    if unsafe { libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_DONTNEED) } != 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// Frees the storage behind a piece of a shared mapping (`MADV_REMOVE`): the host punches a hole
/// in the file or shared memory behind it, and later reads return zeros.
///
/// # Safety
///
/// `ptr` and `len` are aligned to host pages and lie within a writable shared mapping not yet
/// released.
pub unsafe fn remove(ptr: NonNull<u8>, len: usize) -> Result<(), i32> {
    if unsafe { libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_REMOVE) } != 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// A host file mapping: `ptr` points at the requested file offset, and `base`/`len` are the range
/// actually mapped, passed to [`unmap`] on release.
pub struct FileMapping {
    /// Where the requested file offset is in host memory.
    pub ptr: NonNull<u8>,
    /// Start of the actual mapping (aligned to the host page).
    pub base: NonNull<u8>,
    /// Length of the actual mapping.
    pub len: usize,
}

/// Maps `[offset, offset + len)` of host file `fd`. Host pages may be larger than the guest's 4K
/// pages, so the start is aligned down to a host page. With `shared` false it is a copy-on-write
/// private mapping.
pub fn map_file(
    fd: i32,
    offset: u64,
    len: usize,
    shared: bool,
    writable: bool,
) -> Result<FileMapping, i32> {
    let prot = if !shared || writable {
        libc::PROT_READ | libc::PROT_WRITE
    } else {
        libc::PROT_READ
    };
    let flags = if shared {
        libc::MAP_SHARED
    } else {
        libc::MAP_PRIVATE
    };
    map_file_with(fd, offset, len, prot, flags)
}

/// Maps `[offset, offset + len)` of host file `fd` shared over the mapping at `base` (`offset`
/// aligned to the host page), in one step: `MAP_FIXED` replaces the old mapping atomically, so a
/// thread touching the range meanwhile sees the old file or the new one, never a hole.
///
/// # Safety
///
/// `[base, base + len)` is a mapping of this process that nothing uses as other memory.
pub unsafe fn map_file_over(
    base: NonNull<u8>,
    len: usize,
    fd: i32,
    offset: u64,
    writable: bool,
) -> Result<(), i32> {
    let prot = if writable {
        libc::PROT_READ | libc::PROT_WRITE
    } else {
        libc::PROT_READ
    };
    let ptr = unsafe {
        libc::mmap(
            base.as_ptr().cast(),
            len,
            prot,
            libc::MAP_SHARED | libc::MAP_FIXED,
            fd,
            offset as libc::off_t,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(last_errno());
    }
    Ok(())
}

/// A read-only private mapping: shared by several address spaces and writable by none
/// ([`map_file`]).
pub fn map_file_read_only(fd: i32, offset: u64, len: usize) -> Result<FileMapping, i32> {
    map_file_with(fd, offset, len, libc::PROT_READ, libc::MAP_PRIVATE)
}

fn map_file_with(
    fd: i32,
    offset: u64,
    len: usize,
    prot: i32,
    flags: i32,
) -> Result<FileMapping, i32> {
    let page = page_size() as u64;
    let aligned = offset & !(page - 1);
    let delta = (offset - aligned) as usize;
    let map_len = (len + delta).div_ceil(page as usize) * page as usize;
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            map_len,
            prot,
            flags,
            fd,
            aligned as libc::off_t,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(last_errno());
    }
    let base = NonNull::new(ptr.cast::<u8>()).expect("mmap returned NULL");
    Ok(FileMapping {
        ptr: unsafe { base.add(delta) },
        base,
        len: map_len,
    })
}

/// Brings the pages of `[ptr, ptr + len)` into memory by reading one byte from each host page.
/// Never-written pages of anonymous memory map to the zero page and take no extra memory.
///
/// # Safety
///
/// `[ptr, ptr + len)` is readable memory, and reading it cannot raise SIGBUS (file mappings must
/// not extend past the end of the file).
pub unsafe fn populate(ptr: *const u8, len: usize) {
    let page = page_size();
    let mut at = ptr as usize & !(page - 1);
    let end = ptr as usize + len;
    while at < end {
        // SAFETY: guaranteed by the caller; the read is at the larger of at and ptr, which is in
        // the range
        unsafe { std::ptr::read_volatile(at.max(ptr as usize) as *const u8) };
        at += page;
    }
}

/// The page cache state of host file `fd` in `[off, off + len)` (to the end of the file when `len`
/// is 0), counted in 4K pages (the five fields of `struct cachestat`). Cached pages are counted
/// with mincore: mincore on a file mapping queries the page cache itself, independent of the host
/// kernel version and filesystem (the host's cachestat needs 6.5, and on overlayfs it cannot count
/// the underlying file's pages either). The host does not provide the dirty, writeback and
/// evicted fields, so they are 0.
pub fn cachestat(fd: i32, off: u64, len: u64) -> Result<[u64; 5], i32> {
    let size = fstat(fd)?.st_size as u64;
    // 4K pages in the range and within the end of the file
    let first = off / 4096;
    let last = match len {
        0 => u64::MAX,
        n => off.saturating_add(n - 1) / 4096,
    }
    .min(size.saturating_sub(1) / 4096);
    let pages = if size == 0 || last < first {
        0
    } else {
        last - first + 1
    };
    Ok([resident_in_file(fd, first, pages), 0, 0, 0, 0])
}

/// How many of the `pages` pages of file `fd` from 4K page `first` on are in the host's page
/// cache. Mapped in chunks so large files do not take too much address space; parts that cannot be
/// mapped (for example when opened write-only) count as not cached.
fn resident_in_file(fd: i32, first: u64, pages: u64) -> u64 {
    const WINDOW: u64 = 1 << 16;
    let mut total = 0;
    let mut at = first;
    while at < first + pages {
        let n = (first + pages - at).min(WINDOW);
        let span = (n * 4096) as usize;
        let Ok(m) = map_file(fd, at * 4096, span, true, false) else {
            break;
        };
        total += resident_pages(m.ptr.as_ptr(), span) as u64;
        unsafe { unmap(m.base, m.len) };
        at += n;
    }
    total
}

/// The number of 4K pages of `[ptr, ptr + len)` resident in memory (mincore). Measured in chunks
/// so the result array never exceeds 64 KB; chunks that cannot be measured count as 0.
pub fn resident_pages(ptr: *const u8, len: usize) -> usize {
    const WINDOW_PAGES: usize = 1 << 16;
    let page = page_size();
    let start = ptr as usize & !(page - 1);
    let span = (ptr as usize + len).next_multiple_of(page) - start;
    let mut vec = vec![0u8; (span / page).min(WINDOW_PAGES)];
    let mut host_pages = 0;
    let mut at = start;
    while at < start + span {
        let n = ((start + span - at) / page).min(WINDOW_PAGES);
        let rc =
            unsafe { libc::mincore(at as *mut libc::c_void, n * page, vec.as_mut_ptr().cast()) };
        if rc == 0 {
            host_pages += vec[..n].iter().filter(|&&b| b & 1 != 0).count();
        }
        at += n * page;
    }
    (host_pages * page / 4096).min(len.div_ceil(4096))
}

/// Whether each 4K page of `[ptr, ptr + len)` is in host memory (mincore); when host pages are
/// larger, the 4K pages within one host page share a result. On error all are reported as absent.
pub fn page_residency(ptr: *const u8, len: usize) -> Vec<bool> {
    let page = page_size();
    let start = ptr as usize & !(page - 1);
    let span = (ptr as usize + len).next_multiple_of(page) - start;
    let mut vec = vec![0u8; span / page];
    let rc = unsafe { libc::mincore(start as *mut libc::c_void, span, vec.as_mut_ptr().cast()) };
    (0..len.div_ceil(4096))
        .map(|i| rc == 0 && vec[(ptr as usize + i * 4096 - start) / page] & 1 != 0)
        .collect()
}

/// Whether each 4K page of `[ptr, ptr + len)` has been touched (the "present" or "swapped" bit of
/// `/proc/self/pagemap`); when host pages are larger, the 4K pages within one host page share a
/// result. Returns None when pagemap cannot be read. mincore cannot be used: it reports pages
/// swapped out (zram on Android) as "absent" too.
pub fn touched_pages(ptr: *const u8, len: usize) -> Option<Vec<bool>> {
    use std::os::unix::fs::FileExt;
    let pagemap = pagemap()?;
    let page = page_size();
    let first = ptr as usize / page;
    let last = (ptr as usize + len).div_ceil(page);
    let mut raw = vec![0u8; (last - first) * 8];
    pagemap.read_exact_at(&mut raw, first as u64 * 8).ok()?;
    let touched = |host: usize| {
        let i = (host - first) * 8;
        let entry = u64::from_le_bytes(raw[i..i + 8].try_into().unwrap());
        entry & (3 << 62) != 0
    };
    Some(
        (0..len.div_ceil(4096))
            .map(|i| touched((ptr as usize + i * 4096) / page))
            .collect(),
    )
}

/// Whether any host page of `[ptr, ptr + len)` in a private file mapping holds a private copy
/// (written since it was mapped, so no longer the file's page): `/proc/self/pagemap` shows it
/// present without the "file page" bit, or swapped. Returns None when pagemap cannot be read.
pub fn has_private_pages(ptr: *const u8, len: usize) -> Option<bool> {
    use std::os::unix::fs::FileExt;
    if len == 0 {
        return Some(false);
    }
    let pagemap = pagemap()?;
    let page = page_size();
    let first = ptr as usize / page;
    let last = (ptr as usize + len).div_ceil(page);
    let mut raw = vec![0u8; (last - first) * 8];
    pagemap.read_exact_at(&mut raw, first as u64 * 8).ok()?;
    Some(raw.as_chunks::<8>().0.iter().any(|e| {
        let entry = u64::from_le_bytes(*e);
        let (present, swapped, file) = (entry >> 63 & 1, entry >> 62 & 1, entry >> 61 & 1);
        present == 1 && file == 0 || swapped == 1
    }))
}

/// `/proc/self/pagemap`, opened once.
fn pagemap() -> Option<&'static std::fs::File> {
    static PAGEMAP: std::sync::OnceLock<Option<std::fs::File>> = std::sync::OnceLock::new();
    PAGEMAP
        .get_or_init(|| std::fs::File::open("/proc/self/pagemap").ok())
        .as_ref()
}

/// Host page size.
pub fn page_size() -> usize {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

// ---- Files and storage ----

/// Opens a host directory (`O_PATH`) used only as the starting point of `*at` calls
/// ([`open_at`], [`stat_at`], [`chmod_at`], [`set_times_at`], [`unlink_at`]).
pub fn open_dir(path: &std::path::Path) -> Result<std::os::fd::OwnedFd, i32> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| libc::EINVAL)?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(last_errno());
    }
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// Opens the file named `name` in directory `dir` (openat, always with `O_CLOEXEC`); a new file
/// gets permissions `mode` (minus the umask).
pub fn open_at(
    dir: i32,
    name: &std::ffi::CStr,
    flags: i32,
    mode: u32,
) -> Result<std::fs::File, i32> {
    use std::os::fd::FromRawFd;
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(last_errno());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Reads from a host fd, retrying automatically on `EINTR`.
pub fn read(fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

/// Writes to a host fd, retrying automatically on `EINTR`.
pub fn write(fd: i32, buf: &[u8]) -> Result<usize, i32> {
    loop {
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

/// Reads at `offset` of a host fd (pread), retrying automatically on `EINTR`.
pub fn pread(fd: i32, buf: &mut [u8], offset: u64) -> Result<usize, i32> {
    loop {
        let n = unsafe {
            libc::pread(
                fd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                offset as libc::off_t,
            )
        };
        if n >= 0 {
            return Ok(n as usize);
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

/// Writes at `offset` of a host fd (pwrite), retrying automatically on `EINTR`.
pub fn pwrite(fd: i32, buf: &[u8], offset: u64) -> Result<usize, i32> {
    loop {
        let n = unsafe { libc::pwrite(fd, buf.as_ptr().cast(), buf.len(), offset as libc::off_t) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

/// Allocates, punches or zeroes a range of a host file (`mode` takes the Linux `FALLOC_FL_*`
/// bits, the same on both hosts). Retries automatically on `EINTR`; a host filesystem without
/// the mode answers `EOPNOTSUPP`.
pub fn fallocate(fd: i32, mode: i32, offset: u64, len: u64) -> Result<(), i32> {
    loop {
        let rc = unsafe { libc::fallocate(fd, mode, offset as libc::off_t, len as libc::off_t) };
        if rc == 0 {
            return Ok(());
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

/// Moves a host fd's file position.
pub fn lseek(fd: i32, offset: i64, whence: i32) -> Result<u64, i32> {
    let rc = unsafe { libc::lseek(fd, offset as libc::off_t, whence) };
    if rc < 0 {
        Err(last_errno())
    } else {
        Ok(rc as u64)
    }
}

/// Copies up to `len` bytes of host file `fd_in` starting at `off_in` to `off_out` of `fd_out`
/// without the data passing through user space, and returns the number of bytes copied. Uses
/// sendfile: before Android 14 the app's seccomp does not allow copy_file_range (it gets
/// SIGSYS). sendfile writes at the output fd's position, so that is moved to `off_out` first, and
/// the caller must ensure nobody else uses this fd's position meanwhile. Retries automatically on
/// `EINTR`.
pub fn copy_file(
    fd_in: i32,
    off_in: u64,
    fd_out: i32,
    off_out: u64,
    len: usize,
) -> Result<usize, i32> {
    if unsafe { libc::lseek(fd_out, off_out as libc::off_t, libc::SEEK_SET) } < 0 {
        return Err(last_errno());
    }
    loop {
        let mut off = off_in as libc::off_t;
        let n = unsafe { libc::sendfile(fd_out, fd_in, &mut off, len) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

/// Writes host file `fd` to disk: fdatasync with `data_only` (the data and the metadata needed to
/// read it back, such as the length), otherwise fsync.
pub fn fsync(fd: i32, data_only: bool) -> Result<(), i32> {
    let r = unsafe {
        if data_only {
            libc::fdatasync(fd)
        } else {
            libc::fsync(fd)
        }
    };
    if r < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// Flag for [`sync_file_range`]: start writeback (without waiting). The Android part of the libc
/// crate lacks this constant.
pub const SYNC_FILE_RANGE_WRITE: u32 = 2;

/// sync_file_range(2): starts writeback of, and waits for, the dirty pages in a range of host file
/// `fd` (`flags` is a combination of `SYNC_FILE_RANGE_WAIT_BEFORE`, `WRITE` and `WAIT_AFTER`),
/// without flushing metadata or the disk cache. The Android part of the libc crate has no binding
/// for it (bionic does), so it is called by number; on arm64 the arguments are in this order (it
/// is not sync_file_range2).
pub fn sync_file_range(fd: i32, offset: i64, len: i64, flags: u32) -> Result<(), i32> {
    if unsafe { libc::syscall(libc::SYS_sync_file_range, fd, offset, len, flags) } < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// Writes the whole filesystem holding host fd `fd` to disk (syncfs).
pub fn syncfs(fd: i32) -> Result<(), i32> {
    if unsafe { libc::syncfs(fd) } < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// msync(2) with `MS_SYNC`: writes the modified pages of the shared file mappings in
/// `[addr, addr + len)` on the host back to their files and waits for completion. `addr` must be
/// aligned to the host page.
pub fn msync(addr: usize, len: usize) -> Result<(), i32> {
    if unsafe { libc::msync(addr as *mut libc::c_void, len, libc::MS_SYNC) } < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// flock(2): takes an exclusive lock with `exclusive`, otherwise a shared one; with `wait` false
/// it returns `EWOULDBLOCK` at once when the lock is unavailable. An existing lock on the same fd
/// is converted. (The standard library's `File::lock` is not implemented on Android.)
pub fn flock(fd: i32, exclusive: bool, wait: bool) -> Result<(), i32> {
    let mut op = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    if !wait {
        op |= libc::LOCK_NB;
    }
    loop {
        if unsafe { libc::flock(fd, op) } == 0 {
            return Ok(());
        }
        match last_errno() {
            libc::EINTR => continue,
            e => return Err(e),
        }
    }
}

/// Reads a host fd's metadata.
pub fn fstat(fd: i32) -> Result<libc::stat, i32> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } < 0 {
        return Err(last_errno());
    }
    Ok(st)
}

/// Reads the metadata of the file named `name` in directory `dir` (following symlinks).
pub fn stat_at(dir: i32, name: &std::ffi::CStr) -> Result<libc::stat, i32> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, 0) } < 0 {
        return Err(last_errno());
    }
    Ok(st)
}

/// When a time for [`set_times_at`] or [`set_times_fd`] takes this value, the host uses the
/// current time (UTIME_NOW).
pub const TIME_NOW: (i64, i64) = (0, libc::UTIME_NOW);

/// Sets the access and modification times of the file named `name` in directory `dir`
/// (utimensat, following symlinks); times are (seconds, nanoseconds), None meaning unchanged and
/// [`TIME_NOW`] meaning now.
pub fn set_times_at(
    dir: i32,
    name: &std::ffi::CStr,
    atime: Option<(i64, i64)>,
    mtime: Option<(i64, i64)>,
) -> Result<(), i32> {
    let times = utime_spec(atime, mtime);
    if unsafe { libc::utimensat(dir, name.as_ptr(), times.as_ptr(), 0) } < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// Sets the access and modification times of open file `fd` (futimens), with the same arguments
/// as [`set_times_at`].
pub fn set_times_fd(
    fd: i32,
    atime: Option<(i64, i64)>,
    mtime: Option<(i64, i64)>,
) -> Result<(), i32> {
    let times = utime_spec(atime, mtime);
    if unsafe { libc::futimens(fd, times.as_ptr()) } < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// The time argument of utimensat: (seconds, nanoseconds), None meaning unchanged.
fn utime_spec(atime: Option<(i64, i64)>, mtime: Option<(i64, i64)>) -> [libc::timespec; 2] {
    let ts = |t: Option<(i64, i64)>| match t {
        Some((sec, nsec)) => libc::timespec {
            tv_sec: sec as libc::time_t,
            tv_nsec: nsec as _,
        },
        None => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
    };
    [ts(atime), ts(mtime)]
}

/// Changes the permission bits of the file named `name` in directory `dir` (fchmodat, following
/// symlinks).
pub fn chmod_at(dir: i32, name: &std::ffi::CStr, mode: u32) -> Result<(), i32> {
    if unsafe { libc::fchmodat(dir, name.as_ptr(), mode as libc::mode_t, 0) } < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// Removes the file named `name` in directory `dir` (unlinkat).
pub fn unlink_at(dir: i32, name: &std::ffi::CStr) -> Result<(), i32> {
    if unsafe { libc::unlinkat(dir, name.as_ptr(), 0) } < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// Issues an ioctl on a host fd with `arg` as the argument buffer (the caller ensures it is large
/// enough).
pub fn ioctl(fd: i32, request: u64, arg: &mut [u8]) -> Result<i32, i32> {
    let rc = unsafe { libc::ioctl(fd, request as _, arg.as_mut_ptr()) };
    if rc < 0 { Err(last_errno()) } else { Ok(rc) }
}

/// Duplicates a host fd (close-on-exec).
pub fn dup(fd: i32) -> Result<i32, i32> {
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if new < 0 { Err(last_errno()) } else { Ok(new) }
}

/// A new empty host file in memory (memfd_create, close-on-exec) named `name`.
pub fn memfd(name: &std::ffi::CStr) -> Result<std::fs::File, i32> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(last_errno());
    }
    // SAFETY: fd was just created and is owned by nobody else
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// The capacity of a host filesystem (statvfs).
#[derive(Debug, Clone, Copy, Default)]
pub struct FsStats {
    /// Block size.
    pub block_size: u64,
    /// Total blocks.
    pub blocks: u64,
    /// Free blocks.
    pub free: u64,
    /// Blocks available to unprivileged users.
    pub available: u64,
    /// Total inodes.
    pub files: u64,
    /// Free inodes.
    pub files_free: u64,
}

/// Reads the capacity of the host filesystem holding `path`.
#[allow(clippy::unnecessary_cast)] // fsblkcnt_t and similar types differ in width across hosts
pub fn statvfs(path: &std::path::Path) -> Result<FsStats, i32> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| libc::EINVAL)?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } < 0 {
        return Err(last_errno());
    }
    let frsize = if st.f_frsize != 0 {
        st.f_frsize as u64
    } else {
        st.f_bsize as u64
    };
    Ok(FsStats {
        block_size: frsize,
        blocks: st.f_blocks as u64,
        free: st.f_bfree as u64,
        available: st.f_bavail as u64,
        files: st.f_files as u64,
        files_free: st.f_ffree as u64,
    })
}

/// Raises the host process's soft fd limit to the hard limit and returns the new soft limit. All
/// guest processes share the host's fds: every open store file and socket takes one, and the
/// default 1024 is soon used up.
#[allow(clippy::unnecessary_cast)] // rlim_t is 32 bits on 32-bit hosts
pub fn raise_fd_limit() -> u64 {
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } < 0 {
        return 0;
    }
    let hard = rl.rlim_max;
    if rl.rlim_cur < hard {
        rl.rlim_cur = hard;
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rl) } < 0 {
            let _ = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) };
        }
    }
    rl.rlim_cur as u64
}

/// The host process's soft fd limit.
#[allow(clippy::unnecessary_cast)] // rlim_t is 32 bits on 32-bit hosts
pub fn fd_limit() -> u64 {
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } < 0 {
        return 1024;
    }
    if rl.rlim_cur == libc::RLIM_INFINITY {
        u64::MAX
    } else {
        rl.rlim_cur as u64
    }
}

/// Fills `buf` from the host's random source: `getrandom`, which needs no fd (all guest processes
/// share the app's fd table, and opening `/dev/urandom` failed with `EMFILE` once the guest had
/// used it up).
pub fn fill_random(buf: &mut [u8]) -> Result<(), i32> {
    let mut done = 0;
    while done < buf.len() {
        let rest = &mut buf[done..];
        // SAFETY: the host writes at most rest.len() bytes into rest
        let n = unsafe { libc::getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) };
        if n < 0 {
            let e = last_errno();
            if e == libc::EINTR {
                continue;
            }
            return Err(e);
        }
        done += n as usize;
    }
    Ok(())
}

// ---- Host statistics ----

/// Anonymous memory used by the host process (bytes), itemized: RssAnon, RssShmem and VmSwap from
/// `/proc/self/status`, counting what is swapped out (zram on Android); file pages can be
/// reclaimed and do not count.
#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryUsage {
    /// RssAnon: resident private anonymous memory. On old kernels without RssAnon (before 4.5)
    /// it is VmRSS.
    pub anon: u64,
    /// RssShmem: resident shared anonymous memory (memfd, `MAP_SHARED|MAP_ANONYMOUS`).
    pub shmem: u64,
    /// VmSwap: what is swapped out.
    pub swap: u64,
}

/// The host process's anonymous memory ([`MemoryUsage`]). Returns None when it cannot be read.
pub fn memory_usage() -> Option<MemoryUsage> {
    use std::os::unix::fs::FileExt;
    static STATUS: std::sync::OnceLock<Option<std::fs::File>> = std::sync::OnceLock::new();
    let status = STATUS
        .get_or_init(|| std::fs::File::open("/proc/self/status").ok())
        .as_ref()?;
    // Reading from the start makes the kernel regenerate the whole file
    let mut buf = [0u8; 4096];
    let n = status.read_at(&mut buf, 0).ok()?;
    let field = |name: &[u8]| -> Option<u64> {
        let line = buf[..n]
            .split(|&b| b == b'\n')
            .find(|l| l.starts_with(name))?;
        let digits = line[name.len()..].trim_ascii().strip_suffix(b"kB")?;
        std::str::from_utf8(digits).ok()?.trim().parse().ok()
    };
    let (anon, shmem) = match field(b"RssAnon:") {
        Some(anon) => (anon, field(b"RssShmem:").unwrap_or(0)),
        None => (field(b"VmRSS:")?, 0),
    };
    Some(MemoryUsage {
        anon: anon * 1024,
        shmem: shmem * 1024,
        swap: field(b"VmSwap:").unwrap_or(0) * 1024,
    })
}

/// The host's total and free physical memory (bytes).
#[allow(clippy::unnecessary_cast)] // unsigned long is 32 bits on 32-bit hosts
pub fn memory_info() -> (u64, u64) {
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::sysinfo(&mut info) } < 0 {
        return (0, 0);
    }
    let unit = info.mem_unit.max(1) as u64;
    (info.totalram as u64 * unit, info.freeram as u64 * unit)
}

/// The host's total physical memory plus swap (bytes): the limit on a single allocation when
/// Linux estimates the commit (`vm.overcommit_memory` is 0). 0 when it cannot be read.
#[allow(clippy::unnecessary_cast)] // unsigned long is 32 bits on 32-bit hosts
pub fn ram_and_swap() -> u64 {
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::sysinfo(&mut info) } < 0 {
        return 0;
    }
    let unit = info.mem_unit.max(1) as u64;
    (info.totalram as u64 + info.totalswap as u64) * unit
}

/// Some fields of the host's `/proc/meminfo` (bytes); the guest's `/proc/meminfo` and sysinfo
/// copy them, the same as what a docker container sees.
#[derive(Debug, Clone, Copy, Default)]
pub struct MemInfo {
    pub total: u64,
    pub free: u64,
    pub available: u64,
    pub buffers: u64,
    pub cached: u64,
    pub swap_cached: u64,
    pub active: u64,
    pub inactive: u64,
    pub swap_total: u64,
    pub swap_free: u64,
    pub dirty: u64,
    pub anon_pages: u64,
    pub mapped: u64,
    pub shmem: u64,
    pub slab: u64,
    pub sreclaimable: u64,
}

/// The host's `/proc/vmstat`, when the host lets the app read it.
pub fn host_vmstat() -> Option<String> {
    std::fs::read_to_string("/proc/vmstat")
        .ok()
        .filter(|t| !t.is_empty())
}

/// The host's `/proc/swaps`, when the host lets the app read it.
pub fn host_swaps() -> Option<String> {
    std::fs::read_to_string("/proc/swaps")
        .ok()
        .filter(|t| !t.is_empty())
}

/// The host's memory state, read from `/proc/meminfo`. When it cannot be read (the host does not
/// allow it), only the few fields sysinfo can give, with the available amount estimated as free
/// plus buffers.
#[allow(clippy::unnecessary_cast)] // unsigned long is 32 bits on 32-bit hosts
pub fn host_meminfo() -> MemInfo {
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        let field = |name: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
                .and_then(|v| v.trim().strip_suffix("kB"))
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map_or(0, |kb| kb * 1024)
        };
        let total = field("MemTotal");
        if total != 0 {
            return MemInfo {
                total,
                free: field("MemFree"),
                available: field("MemAvailable"),
                buffers: field("Buffers"),
                cached: field("Cached"),
                swap_cached: field("SwapCached"),
                active: field("Active"),
                inactive: field("Inactive"),
                swap_total: field("SwapTotal"),
                swap_free: field("SwapFree"),
                dirty: field("Dirty"),
                anon_pages: field("AnonPages"),
                mapped: field("Mapped"),
                shmem: field("Shmem"),
                slab: field("Slab"),
                sreclaimable: field("SReclaimable"),
            };
        }
    }
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::sysinfo(&mut info) } < 0 {
        return MemInfo::default();
    }
    let unit = info.mem_unit.max(1) as u64;
    let free = info.freeram as u64 * unit;
    let buffers = info.bufferram as u64 * unit;
    MemInfo {
        total: info.totalram as u64 * unit,
        free,
        available: free + buffers,
        buffers,
        swap_total: info.totalswap as u64 * unit,
        swap_free: info.freeswap as u64 * unit,
        shmem: info.sharedram as u64 * unit,
        ..MemInfo::default()
    }
}

// ---- Waiting ----

/// An event for waking across threads: an eventfd.
///
/// After [`Event::signal`], its fd is readable until [`Event::clear`].
pub struct Event {
    fd: i32,
}

impl Event {
    /// Creates an event that is not signalled.
    pub fn new() -> Result<Event, i32> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(last_errno());
        }
        Ok(Event { fd })
    }

    /// Signals the event; can be called from any thread.
    pub fn signal(&self) {
        let one = 1u64.to_ne_bytes();
        // The write fails when the counter is full, but then the event is already signalled
        unsafe { libc::write(self.fd, one.as_ptr().cast(), one.len()) };
    }

    /// Whether the event is signalled (without clearing it).
    pub fn is_signaled(&self) -> bool {
        const POLLIN: i16 = 1;
        let mut fds = [(self.fd, POLLIN, 0)];
        matches!(poll(&mut fds, Some(Duration::ZERO)), Ok(n) if n > 0)
    }

    /// Clears the signalled state.
    pub fn clear(&self) {
        let mut buf = [0u8; 64];
        while unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
    }

    /// The fd to poll (waiting for `POLLIN`).
    pub fn fd(&self) -> i32 {
        self.fd
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// New events on a host fd: an edge-triggered host epoll watching only this one fd. A host poll
/// can only see whether the fd is ready now, not that more data arrived while it was already
/// readable; the host epoll's edge triggering reports once each time data arrives (each wakeup),
/// which is exactly what the guest's `EPOLLET` needs.
pub struct Edges {
    epfd: std::os::fd::OwnedFd,
}

impl Edges {
    /// Starts recording events on `fd`. If the fd is already ready when this is created, the first
    /// [`Edges::take`] reports it.
    pub fn new(fd: i32) -> Result<Edges, i32> {
        use std::os::fd::FromRawFd;
        let raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if raw < 0 {
            return Err(last_errno());
        }
        let epfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLRDHUP | libc::EPOLLET) as u32,
            u64: 0,
        };
        if unsafe { libc::epoll_ctl(raw, libc::EPOLL_CTL_ADD, fd, &mut ev) } < 0 {
            return Err(last_errno());
        }
        Ok(Edges { epfd })
    }

    /// Takes the events since the last call: whether there are new ones in the (read, write)
    /// directions. Errors and hangups count as the read direction.
    pub fn take(&self) -> (bool, bool) {
        let mut ev = libc::epoll_event { events: 0, u64: 0 };
        if unsafe { libc::epoll_wait(self.fd(), &mut ev, 1, 0) } != 1 {
            return (false, false);
        }
        let out = (libc::EPOLLOUT | libc::EPOLLWRNORM | libc::EPOLLWRBAND) as u32;
        (ev.events & !out != 0, ev.events & out != 0)
    }

    /// The fd of the host epoll itself: readable when there are new events, and can be waited on
    /// in [`poll`].
    pub fn fd(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.epfd.as_raw_fd()
    }
}

/// A set of host fds: it is readable itself when any of them is ready (level-triggered), and can
/// be waited on together in [`poll`].
pub struct Watch {
    epfd: std::os::fd::OwnedFd,
}

impl Watch {
    pub fn new() -> Result<Watch, i32> {
        use std::os::fd::FromRawFd;
        let raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if raw < 0 {
            return Err(last_errno());
        }
        Ok(Watch {
            epfd: unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) },
        })
    }

    /// Starts watching `fd`: for readability with `readable`, otherwise only for errors and hangups
    /// (the two that are always reported).
    pub fn add(&self, fd: i32, readable: bool) -> Result<(), i32> {
        let mut ev = libc::epoll_event {
            events: if readable { libc::EPOLLIN as u32 } else { 0 },
            u64: fd as u64,
        };
        if unsafe { libc::epoll_ctl(self.fd(), libc::EPOLL_CTL_ADD, fd, &mut ev) } < 0 {
            return Err(last_errno());
        }
        Ok(())
    }

    /// Stops watching `fd` (the host removes closed fds by itself).
    pub fn remove(&self, fd: i32) {
        unsafe {
            libc::epoll_ctl(self.fd(), libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut());
        }
    }

    /// The fd of the host epoll itself.
    pub fn fd(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.epfd.as_raw_fd()
    }
}

/// Waits for events on host fds: each item of `fds` is (fd, events, revents), and a `timeout` of
/// None waits forever. Returns the number of ready fds; returns `EINTR` when interrupted by a host
/// signal.
pub fn poll(fds: &mut [(i32, i16, i16)], timeout: Option<Duration>) -> Result<usize, i32> {
    let mut raw: Vec<libc::pollfd> = fds
        .iter()
        .map(|&(fd, events, _)| libc::pollfd {
            fd,
            events,
            revents: 0,
        })
        .collect();
    let rc = poll_raw(&mut raw, timeout);
    if rc < 0 {
        return Err(last_errno());
    }
    for (slot, r) in fds.iter_mut().zip(&raw) {
        slot.2 = r.revents;
    }
    Ok(rc as usize)
}

fn poll_raw(raw: &mut [libc::pollfd], timeout: Option<Duration>) -> i32 {
    let ts = timeout.map(|t| libc::timespec {
        tv_sec: t.as_secs().min(i32::MAX as u64) as libc::time_t,
        tv_nsec: t.subsec_nanos() as _,
    });
    let ts_ptr = ts
        .as_ref()
        .map_or(std::ptr::null(), |t| t as *const libc::timespec);
    unsafe {
        libc::ppoll(
            raw.as_mut_ptr(),
            raw.len() as libc::nfds_t,
            ts_ptr,
            std::ptr::null(),
        )
    }
}

// ---- Threads, CPUs and time ----

/// The numbers of the host CPUs this process may use (at most 64), a snapshot taken at the first
/// call: the guest's CPU `i` is the `i`-th of them. Later changes to host threads' affinity (the
/// guest's sched_setaffinity) do not affect it.
pub fn host_cpus() -> &'static [usize] {
    static CPUS: std::sync::OnceLock<Vec<usize>> = std::sync::OnceLock::new();
    CPUS.get_or_init(|| {
        // Also takes the cgroup's CPU quota into account
        let n = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(64);
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) == 0 {
                // CPU_SETSIZE is i32 on Linux and usize on Android
                #[allow(clippy::unnecessary_cast)]
                let cpus: Vec<usize> = (0..libc::CPU_SETSIZE as usize)
                    .filter(|&i| libc::CPU_ISSET(i, &set))
                    .take(n)
                    .collect();
                if !cpus.is_empty() {
                    return cpus;
                }
            }
        }
        (0..n).collect()
    })
}

/// `path` under host CPU `cpu`'s sysfs directory (`/sys/devices/system/cpu/cpu<n>`): returns the
/// contents for a file and empty contents for a directory; None when it is missing or cannot be
/// read (Android's SELinux may not let the app read cpufreq).
pub fn cpu_sysfs(cpu: usize, path: &str) -> Option<Vec<u8>> {
    let full = format!("/sys/devices/system/cpu/cpu{cpu}/{path}");
    let meta = std::fs::metadata(&full).ok()?;
    if meta.is_dir() {
        return Some(Vec::new());
    }
    std::fs::read(&full).ok()
}

/// The current host thread's tid.
pub fn gettid() -> i32 {
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

/// Sets the calling thread's name (`/proc/self/task/<tid>/comm`, what debuggers show), truncated
/// beyond 15 bytes.
pub fn set_thread_name(name: &str) {
    let mut buf = [0u8; 16];
    let n = name.len().min(15);
    buf[..n].copy_from_slice(&name.as_bytes()[..n]);
    unsafe { libc::prctl(libc::PR_SET_NAME, buf.as_ptr()) };
}

/// The current thread's CPU-time clock; other threads can read it with [`clock_gettime`] too
/// (invalid after the thread exits).
pub fn thread_cpu_clock() -> Option<i32> {
    let mut id: libc::clockid_t = 0;
    (unsafe { libc::pthread_getcpuclockid(libc::pthread_self(), &mut id) } == 0).then_some(id)
}

/// Time the calling thread has spent in the host kernel so far (the stime of
/// `getrusage(RUSAGE_THREAD)`).
pub fn thread_system_ns_self() -> u64 {
    // Linux's UAPI constant; the libc crate does not define it on Android
    const RUSAGE_THREAD: libc::c_int = 1;
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(RUSAGE_THREAD, &mut ru) } == 0 {
        let t = ru.ru_stime;
        return t.tv_sec as u64 * 1_000_000_000 + t.tv_usec as u64 * 1000;
    }
    0
}

/// Time host thread `tid` of this process has spent in the host kernel (the stime of
/// `/proc/self/task/<tid>/stat`, with a resolution of one clock tick). Returns None when the
/// thread has exited or the file cannot be read.
pub fn thread_system_ns(tid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/self/task/{tid}/stat")).ok()?;
    // Column 2 is the thread name in parentheses and may contain spaces; counting from column 3
    // (the state) after it, stime is column 15
    let rest = &stat[stat.rfind(')')? + 1..];
    let ticks: u64 = rest.split_ascii_whitespace().nth(12)?.parse().ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    Some(ticks * (1_000_000_000 / hz))
}

/// The calling thread's (voluntary, involuntary) context switch counts, the same counts as
/// [`thread_ctxt_switches`] but with a single getrusage: the baseline is taken while holding the
/// BKL when a thread starts, and reading the `/proc` status takes about ten microseconds each
/// time.
pub fn thread_ctxt_switches_self() -> Option<(u64, u64)> {
    const RUSAGE_THREAD: libc::c_int = 1;
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    (unsafe { libc::getrusage(RUSAGE_THREAD, &mut ru) } == 0)
        .then_some((ru.ru_nvcsw as u64, ru.ru_nivcsw as u64))
}

/// The voluntary and involuntary context switch counts of host thread `tid` of this process
/// (`/proc/self/task/<tid>/status`).
pub fn thread_ctxt_switches(tid: i32) -> Option<(u64, u64)> {
    let status = std::fs::read_to_string(format!("/proc/self/task/{tid}/status")).ok()?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().parse().ok())
    };
    Some((
        field("voluntary_ctxt_switches:")?,
        field("nonvoluntary_ctxt_switches:")?,
    ))
}

/// The name of host thread `tid` of this process and what it is doing, for reports of a thread
/// stuck somewhere: `running`, `in host syscall <n>` (`/proc/self/task/<tid>/syscall`, readable
/// only by the process itself), or else its scheduler state from `stat`. None when the thread has
/// exited.
pub fn thread_activity(tid: i32) -> Option<(String, String)> {
    let task = format!("/proc/self/task/{tid}");
    let name = std::fs::read_to_string(format!("{task}/comm")).ok()?;
    // "running", "<nr> <args...> <sp> <pc>" in a syscall, "-1 <sp> <pc>" blocked outside one
    let syscall = std::fs::read_to_string(format!("{task}/syscall")).unwrap_or_default();
    let doing = match syscall.split_ascii_whitespace().next() {
        Some("running") => "running".to_owned(),
        Some(nr) if nr.parse::<u32>().is_ok() => format!("in host syscall {nr}"),
        _ => {
            let stat = std::fs::read_to_string(format!("{task}/stat")).ok()?;
            match stat[stat.rfind(')')? + 1..]
                .split_ascii_whitespace()
                .next()?
            {
                "R" => "running".to_owned(),
                "S" => "sleeping".to_owned(),
                "D" => "in uninterruptible sleep".to_owned(),
                state => format!("in state {state}"),
            }
        }
    };
    Some((name.trim_end().to_owned(), doing))
}

/// Restricts host thread `tid` of this process to the CPUs of [`host_cpus`] selected by `mask`
/// (bit `i` is the `i`-th). Left unchanged on failure.
pub fn set_thread_affinity(tid: i32, mask: u64) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for (i, &cpu) in host_cpus().iter().enumerate() {
            if mask & (1 << i) != 0 {
                libc::CPU_SET(cpu, &mut set);
            }
        }
        libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

/// Sets the nice value of host thread `tid` of this process. Left unchanged on failure (no
/// permission to raise priority).
pub fn set_thread_nice(tid: i32, nice: i32) {
    unsafe {
        // On Linux PRIO_PROCESS applies to a single thread
        libc::setpriority(libc::PRIO_PROCESS as _, tid as libc::id_t, nice);
    }
}

/// Sets the IO priority of host thread `tid` (`ioprio_set(IOPRIO_WHO_PROCESS, tid, ioprio)`, with
/// the value in Linux's encoding: the class in bits 13–15). Android's app seccomp allows it. Left
/// unchanged without permission.
pub fn set_thread_ioprio(tid: i32, ioprio: u16) {
    unsafe {
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            tid as libc::c_long,
            ioprio as libc::c_long,
        );
    }
}

/// The IO priority of host thread `tid` (`ioprio_get(IOPRIO_WHO_PROCESS, tid)`).
pub fn thread_ioprio(tid: i32) -> Option<u16> {
    const IOPRIO_WHO_PROCESS: libc::c_long = 1;
    let r = unsafe {
        libc::syscall(
            libc::SYS_ioprio_get,
            IOPRIO_WHO_PROCESS,
            tid as libc::c_long,
        )
    };
    (r >= 0).then_some(r as u16)
}

/// Linux's `CLOCK_BOOTTIME`.
pub const CLOCK_BOOTTIME: i32 = 7;

/// Reads a host clock, with clock ids as defined by Linux: (seconds, nanoseconds).
pub fn clock_gettime(clock: i32) -> Result<(i64, i64), i32> {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(clock as libc::clockid_t, &mut ts) } < 0 {
        return Err(last_errno());
    }
    Ok(timespec_pair(&ts))
}

/// Watches the host's wall clock for jumps against the monotonic clock: a timerfd on
/// `CLOCK_REALTIME` armed far in the future with `TFD_TIMER_CANCEL_ON_SET`, which Linux cancels
/// (the fd turns readable and reads `ECANCELED`) whenever the wall clock is set and when the device
/// resumes from sleep (`timerfd_resume`): the two moments the realtime and boottime clocks jump.
pub struct ClockWatch {
    fd: i32,
}

impl ClockWatch {
    /// None where timerfds are unavailable (a refusing seccomp policy); jumps then go unnoticed.
    pub fn new() -> Option<ClockWatch> {
        let fd = unsafe {
            libc::timerfd_create(libc::CLOCK_REALTIME, libc::O_NONBLOCK | libc::O_CLOEXEC)
        };
        if fd < 0 {
            return None;
        }
        let watch = ClockWatch { fd };
        watch.arm().then_some(watch)
    }

    /// Arms the timer at a time that never comes (2^40 seconds, which the kernel saturates).
    fn arm(&self) -> bool {
        const TFD_TIMER_ABSTIME: i32 = 1;
        const TFD_TIMER_CANCEL_ON_SET: i32 = 2;
        let mut spec: libc::itimerspec = unsafe { std::mem::zeroed() };
        spec.it_value.tv_sec = 1 << 40;
        let flags = TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET;
        unsafe { libc::timerfd_settime(self.fd, flags, &spec, std::ptr::null_mut()) == 0 }
    }

    /// The fd to poll for readability.
    pub fn fd(&self) -> i32 {
        self.fd
    }

    /// Whether the clock jumped since the last call; the watch is armed again after a jump.
    pub fn take(&self) -> bool {
        let mut buf = [0u8; 8];
        let r = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
        if r < 0 && last_errno() == libc::ECANCELED {
            self.arm();
            return true;
        }
        false
    }
}

impl Drop for ClockWatch {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// The resolution of a host clock: (seconds, nanoseconds).
pub fn clock_getres(clock: i32) -> Result<(i64, i64), i32> {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_getres(clock as libc::clockid_t, &mut ts) } < 0 {
        return Err(last_errno());
    }
    Ok(timespec_pair(&ts))
}

// time_t and long are 32 bits on 32-bit hosts; on 64-bit hosts these conversions are identities
#[allow(clippy::unnecessary_cast)]
fn timespec_pair(ts: &libc::timespec) -> (i64, i64) {
    (ts.tv_sec as i64, ts.tv_nsec as i64)
}

/// The host kernel's timezone (`sys_tz`, which gettimeofday reports): (minutes west of
/// Greenwich, DST type). Android sets it to the device's time zone; Linux distributions leave it
/// at 0 unless the RTC keeps local time.
pub fn timezone() -> (i32, i32) {
    // struct timezone (the libc crate leaves it opaque on glibc); the syscall itself, since
    // glibc's gettimeofday zeroes it before asking the kernel
    let mut tz = [0i32; 2];
    let tv: *mut libc::timeval = std::ptr::null_mut();
    unsafe { libc::syscall(libc::SYS_gettimeofday, tv, tz.as_mut_ptr()) };
    (tz[0], tz[1])
}

/// Where the host kernel mapped its vDSO into this process, and the vvar pages right below it.
pub struct HostVdso {
    /// The vDSO's mapping (`[vdso]`, starting at `AT_SYSINFO_EHDR`).
    pub vdso: NonNull<u8>,
    pub vdso_len: usize,
    /// The start of the vvar mappings right below it (`[vvar]`, and on x86 since 6.13 also
    /// `[vvar_vclock]`): the vDSO's code finds its data at fixed distances below itself.
    pub vvar: NonNull<u8>,
}

/// The host's vDSO and vvar mappings in this process, from `AT_SYSINFO_EHDR` and
/// `/proc/self/maps`; None when either is missing.
pub fn host_vdso() -> Option<HostVdso> {
    let base = unsafe { libc::getauxval(libc::AT_SYSINFO_EHDR) } as usize;
    if base == 0 {
        return None;
    }
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    // (start, end, name) of each mapping, in address order
    let mappings: Vec<(usize, usize, &str)> = maps
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (start, end) = fields.next()?.split_once('-')?;
            let name = fields.nth(4).unwrap_or("");
            let start = usize::from_str_radix(start, 16).ok()?;
            Some((start, usize::from_str_radix(end, 16).ok()?, name))
        })
        .collect();
    let at = mappings
        .iter()
        .position(|&(start, _, name)| start == base && name == "[vdso]")?;
    let mut vvar = base;
    for &(start, end, name) in mappings[..at].iter().rev() {
        if end != vvar || !name.starts_with("[vvar") {
            break;
        }
        vvar = start;
    }
    (vvar < base).then(|| HostVdso {
        vdso: NonNull::new(base as *mut u8).unwrap(),
        vdso_len: mappings[at].1 - base,
        vvar: NonNull::new(vvar as *mut u8).unwrap(),
    })
}

/// The size of `struct __kernel_timex` (the same on arm64 and x86_64 Linux).
pub const TIMEX_SIZE: usize = 208;

/// The NTP state of the host clock (a read-only adjtimex): returns adjtimex's return value (the
/// clock state, such as `TIME_OK` or `TIME_ERROR`) and the filled-in `struct __kernel_timex`. With
/// `adjtime` true it queries with `ADJ_OFFSET_SS_READ` (offset is the adjustment adjtime(3) has
/// left), otherwise with modes = 0.
///
/// Android's app seccomp forbids adjtimex (`SECCOMP_BLOCKLIST_APP.TXT`; calling it gets the whole
/// app SIGSYS), so the result is synthesized from the kernel's actual state on Android: Android
/// sets the time with settimeofday and does not run the kernel's NTP adjustment, so the clock is
/// always unsynchronized (`STA_UNSYNC`), and the fields have their initial values after
/// `ntp_clear`.
pub fn adjtimex_query(adjtime: bool) -> Result<(i32, [u8; TIMEX_SIZE]), i32> {
    let mut buf = [0u8; TIMEX_SIZE];
    #[cfg(target_os = "linux")]
    {
        const ADJ_OFFSET_SS_READ: u32 = 0xa001;
        let modes: u32 = if adjtime { ADJ_OFFSET_SS_READ } else { 0 };
        buf[..4].copy_from_slice(&modes.to_le_bytes());
        let r = unsafe { libc::syscall(libc::SYS_adjtimex, buf.as_mut_ptr()) };
        if r < 0 {
            return Err(last_errno());
        }
        Ok((r as i32, buf))
    }
    #[cfg(target_os = "android")]
    {
        let _ = (adjtime, &mut buf);
        unsynchronized_timex()
    }
}

/// The state of a clock without the kernel's NTP adjustment running (the initial values after
/// `ntp_clear`): `STA_UNSYNC`, error `NTP_PHASE_LIMIT`, time_constant = 2, precision 1, tolerance
/// `MAXFREQ_SCALED / PPM_SCALE` (500 ppm shifted left by 16), tick 1000000 / USER_HZ, returning
/// `TIME_ERROR`.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn unsynchronized_timex() -> Result<(i32, [u8; TIMEX_SIZE]), i32> {
    const TIME_ERROR: i32 = 5;
    let mut buf = [0u8; TIMEX_SIZE];
    let mut put = |off: usize, v: i64| buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    put(24, 16_000_000); // maxerror
    put(32, 16_000_000); // esterror
    put(48, 2); // constant
    put(56, 1); // precision
    put(64, 500 << 16); // tolerance
    let (sec, nsec) = clock_gettime(0)?; // CLOCK_REALTIME
    put(72, sec);
    put(80, nsec / 1000); // no STA_NANO: microseconds
    put(88, 10_000); // tick
    buf[40..44].copy_from_slice(&0x40i32.to_le_bytes()); // status = STA_UNSYNC
    Ok((TIME_ERROR, buf))
}

// ---- Terminal helpers for the CLI ----

/// Whether the fd is a terminal.
pub fn isatty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

/// Switches the host terminal to raw mode, restoring the old settings on drop.
pub struct RawMode {
    fd: i32,
    saved: libc::termios,
}

impl RawMode {
    /// Switches the terminal of `fd`: turns off echo, line editing and signals so each keystroke
    /// arrives as is.
    pub fn enter(fd: i32) -> Result<RawMode, i32> {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut t) } < 0 {
            return Err(last_errno());
        }
        let saved = t;
        unsafe { libc::cfmakeraw(&mut t) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) } < 0 {
            return Err(last_errno());
        }
        Ok(RawMode { fd, saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSADRAIN, &self.saved) };
    }
}

static WINCH_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// An fd (nonblocking) that becomes readable when the host receives SIGWINCH (terminal window
/// size change); can be called only once.
pub fn sigwinch_notifier() -> Result<i32, i32> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(last_errno());
    }
    for fd in fds {
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
        }
    }
    WINCH_FD.store(fds[1], std::sync::atomic::Ordering::Relaxed);
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = on_sigwinch as extern "C" fn(libc::c_int) as usize;
    sa.sa_flags = libc::SA_RESTART;
    if unsafe { libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut()) } < 0 {
        let e = last_errno();
        WINCH_FD.store(-1, std::sync::atomic::Ordering::Relaxed);
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        return Err(e);
    }
    Ok(fds[0])
}

extern "C" fn on_sigwinch(_: libc::c_int) {
    let fd = WINCH_FD.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        // Call only the async-signal-safe write; a full pipe means a notification is already
        // waiting. A failing write (EAGAIN) must not leave its errno to the interrupted code
        let saved = errno_location();
        // SAFETY: errno_location points at this thread's errno
        let errno = unsafe { *saved };
        unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) };
        unsafe { *saved = errno };
    }
}

/// This thread's errno.
fn errno_location() -> *mut libc::c_int {
    #[cfg(target_os = "android")]
    // SAFETY: always valid for the calling thread
    unsafe {
        libc::__errno()
    }
    #[cfg(not(target_os = "android"))]
    // SAFETY: always valid for the calling thread
    unsafe {
        libc::__errno_location()
    }
}

static STOP_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// An fd (nonblocking) from which each byte read is the number of a SIGINT, SIGTERM or SIGHUP the
/// host received (the CLI asks the guest to stop with them instead of dying at once); can be
/// called only once. On failure nothing is left installed.
pub fn stop_notifier() -> Result<i32, i32> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(last_errno());
    }
    for fd in fds {
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
        }
    }
    STOP_FD.store(fds[1], std::sync::atomic::Ordering::Relaxed);
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = on_stop_signal as extern "C" fn(libc::c_int) as usize;
    sa.sa_flags = libc::SA_RESTART;
    let signals = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];
    for (i, &sig) in signals.iter().enumerate() {
        if unsafe { libc::sigaction(sig, &sa, std::ptr::null_mut()) } < 0 {
            let e = last_errno();
            let mut default: libc::sigaction = unsafe { std::mem::zeroed() };
            default.sa_sigaction = libc::SIG_DFL;
            for &done in &signals[..i] {
                unsafe { libc::sigaction(done, &default, std::ptr::null_mut()) };
            }
            STOP_FD.store(-1, std::sync::atomic::Ordering::Relaxed);
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(e);
        }
    }
    Ok(fds[0])
}

extern "C" fn on_stop_signal(sig: libc::c_int) {
    let fd = STOP_FD.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        // Only the async-signal-safe write, keeping the interrupted code's errno
        let saved = errno_location();
        // SAFETY: errno_location points at this thread's errno
        let errno = unsafe { *saved };
        unsafe { libc::write(fd, [sig as u8].as_ptr().cast(), 1) };
        unsafe { *saved = errno };
    }
}

/// Drains the data currently in a nonblocking fd (a notification pipe).
pub fn drain(fd: i32) {
    let mut buf = [0u8; 64];
    while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

// ---- Errors ----

fn last_errno() -> i32 {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::AsRawFd;

    /// Random bytes need no fd: a child process with its fd limit at 0 (like the app once the
    /// guest has used up the shared fd table) still gets them, also more than one getrandom call
    /// returns at a time.
    #[test]
    fn random_bytes_need_no_fd() {
        let mut big = vec![0u8; 1 << 20];
        fill_random(&mut big).unwrap();
        assert!(big.iter().filter(|&&b| b == 0).count() < big.len() / 64);
        // SAFETY: the child only makes syscalls and leaves with _exit
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let mut lim = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            let mut buf = [0u8; 64];
            // SAFETY: plain syscalls on local memory
            let ok = unsafe {
                libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 && {
                    lim.rlim_cur = 0;
                    libc::setrlimit(libc::RLIMIT_NOFILE, &lim) == 0
                }
            } && fill_random(&mut buf).is_ok()
                && buf.iter().any(|&b| b != 0);
            // SAFETY: ends the child without running the parent's destructors
            unsafe { libc::_exit(if ok { 0 } else { 1 }) };
        }
        let mut status = 0;
        // SAFETY: waits for the child forked above
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    }

    #[test]
    fn synthesized_clock_state_matches_a_kernel_without_ntp() {
        // What Android synthesizes (unsynchronized_timex) matches the host kernel's read-only
        // query in the fields that do not change with NTP state (precision, tolerance); the
        // synthesized one is unsynchronized, TIME_ERROR, with the current time. tick is not
        // compared: daemons like chrony change it with ADJ_TICK when the frequency error is large
        // (9999 has been seen on a CI runner); the synthesized one is 10000 for USER_HZ 100, and
        // the host's only has to be within the ±10% the kernel allows
        let field = |b: &[u8; TIMEX_SIZE], off: usize| {
            i64::from_le_bytes(b[off..off + 8].try_into().unwrap())
        };
        let (state, fake) = unsynchronized_timex().unwrap();
        assert_eq!(state, 5);
        assert_eq!(i32::from_le_bytes(fake[40..44].try_into().unwrap()), 0x40);
        assert_eq!(
            (field(&fake, 24), field(&fake, 32)),
            (16_000_000, 16_000_000)
        );
        let now = clock_gettime(0).unwrap().0;
        assert!((now - field(&fake, 72)).abs() <= 1);
        #[cfg(target_os = "linux")]
        {
            let (_, host) = adjtimex_query(false).unwrap();
            for off in [56, 64] {
                assert_eq!(field(&fake, off), field(&host, off), "offset {off}");
            }
            assert_eq!(field(&fake, 88), 10_000);
            assert!((9_000..=11_000).contains(&field(&host, 88)));
        }
    }

    #[test]
    fn checking_an_event_does_not_clear_it() {
        let e = Event::new().unwrap();
        assert!(!e.is_signaled());
        e.signal();
        assert!(e.is_signaled());
        assert!(e.is_signaled());
        e.clear();
        assert!(!e.is_signaled());
    }

    #[test]
    fn counts_written_pages_as_cached() {
        let path = std::env::temp_dir().join(format!("tuxport-cachestat-{}", std::process::id()));
        let mut f = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.write_all(&[7u8; 3 * 4096 + 100]).unwrap();
        let fd = f.as_raw_fd();
        // Freshly written pages are all in the page cache; the part of a range beyond the end of
        // the file does not count
        assert_eq!(cachestat(fd, 0, 0).unwrap()[0], 4);
        assert_eq!(cachestat(fd, 4096, 4096).unwrap()[0], 1);
        assert_eq!(cachestat(fd, 1 << 20, 0).unwrap(), [0; 5]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn finds_the_host_vdso_and_its_vvar() {
        let v = host_vdso().unwrap();
        let page = page_size();
        assert!(v.vdso_len >= page && v.vdso_len.is_multiple_of(page));
        let vvar_len = v.vdso.as_ptr() as usize - v.vvar.as_ptr() as usize;
        assert!(vvar_len >= page && vvar_len.is_multiple_of(page));
        // SAFETY: the vDSO mapping is readable
        let magic = unsafe { std::slice::from_raw_parts(v.vdso.as_ptr(), 4) };
        assert_eq!(magic, b"\x7fELF");
    }
}
