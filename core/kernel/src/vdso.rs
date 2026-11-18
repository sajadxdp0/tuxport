//! vDSO (the kernel/vdso map). Every process maps a private copy of one image, chosen once per
//! host process ([`get`]):
//!
//! - The host kernel's own vDSO, when it can be used ([`from_host`]). Its clock functions read the
//!   counter (`CNTVCT_EL0`, which the engine reads natively) and the host's vvar data page, mapped
//!   read-only below the copy at the distance the code expects (`[vvar]`, [`Vvar`]), so
//!   clock_gettime, gettimeofday and clock_getres stay in guest code, as on Linux, and return
//!   exactly what the host's syscalls return: the guest's clocks are the host's, and timers and the
//!   syscall path run on them. Clocks it does not handle fall back to a real syscall, as on Linux.
//!   (Starnix fills a vvar page of its own from Zircon's clock transformations, which are exact
//!   there; parameters of our own would drift from the host's NTP-adjusted clocks.) The copy is
//!   changed only where Tuxport's contract needs it: the `Linux` version note says 6.1.0, and
//!   symbols other than the four below (newer kernels' `__kernel_getrandom`, whose data is not
//!   mapped) are hidden. Before it is used, its clock functions run once in the interpreter and
//!   must return what the host returns ([`host_works`]).
//! - Otherwise a minimal ELF shared object assembled here byte by byte ([`build`]), whose clock
//!   functions are svc stubs answered by the kernel layer.
//!
//! Both export four symbols of version `LINUX_2.6.39`, as arm64 Linux does:
//!
//! - `__kernel_rt_sigreturn` is exactly `mov x8, #139; svc #0` (libgcc's unwinder recognises
//!   signal frames by matching these two instructions byte for byte; Linux's vDSO also puts a nop
//!   in front of it);
//! - `__kernel_clock_gettime`, `__kernel_gettimeofday` and `__kernel_clock_getres`.
//!
//! The dynamic segment provides `DT_HASH`, `DT_SYMTAB`, `DT_STRTAB`, `DT_VERSYM` and `DT_VERDEF`,
//! which satisfies the vDSO parsers of musl, glibc and the Go runtime.
//!
//! In a time namespace with offsets the host's code would return the host's clocks: a process
//! there gets the clock functions' entries replaced by the same svc stubs ([`svc_image`],
//! `Mm::vdso_syscalls`), and the kernel layer adds the offsets, where Linux's vDSO reads them from
//! a namespace page.

use std::sync::OnceLock;

use tuxport_engine::PAGE_SIZE;

/// The image every process maps.
struct Image {
    bytes: Vec<u8>,
    /// The same with the clock functions' entries replaced by svc stubs ([`svc_entry`]); equal to
    /// `bytes` for the built image.
    svc: Vec<u8>,
    /// The offset of `__kernel_rt_sigreturn`.
    sigreturn: u64,
    /// The offsets of the clock functions and the syscalls their stubs make.
    clocks: [(u64, u32); 3],
    /// Present for the host's vDSO.
    vvar: Option<Vvar>,
}

/// The host's vvar pages as the host's vDSO code expects them below itself.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Vvar {
    /// The length of the guest's `[vvar]`: the distance from its start to the vDSO, as in the host.
    pub len: u64,
    /// The host address of its start. The guest can read only the first `data` bytes, the clocks'
    /// data the host's code reads: one host page, two when the host itself runs in a time
    /// namespace (the namespace's page, then the real data). The rest (the time-namespace page,
    /// newer kernels' random-number state) gets SIGBUS, as on Linux outside a time namespace.
    pub host: usize,
    pub data: u64,
}

const VERSION: &[u8] = b"LINUX_2.6.39";
const SONAME: &[u8] = b"linux-vdso.so.1";

const NOP: u32 = 0xd503_201f;
const SVC0: u32 = 0xd400_0001;
const RET: u32 = 0xd65f_03c0;

const CLOCK_GETTIME: u32 = 113;
const CLOCK_GETRES: u32 = 114;
const GETTIMEOFDAY: u32 = 169;
const RT_SIGRETURN: u32 = 139;

/// The exported symbols: sigreturn first, then the clock functions with their syscalls.
const SIGRETURN: &[u8] = b"__kernel_rt_sigreturn";
const CLOCKS: [(&[u8], u32); 3] = [
    (b"__kernel_clock_gettime", CLOCK_GETTIME),
    (b"__kernel_gettimeofday", GETTIMEOFDAY),
    (b"__kernel_clock_getres", CLOCK_GETRES),
];

/// `movz x8, #nr`.
const fn mov_x8(nr: u32) -> u32 {
    0xd280_0008 | (nr << 5)
}

/// The svc stub of a clock function: `mov x8, #nr; svc #0; ret`. The svc is at offset 4.
fn svc_entry(nr: u32) -> [u32; 3] {
    [mov_x8(nr), SVC0, RET]
}

/// The vDSO image every process maps at execve (at most a few pages).
pub fn image() -> &'static [u8] {
    &get().bytes
}

/// The image of a process whose clock functions must enter the kernel layer (a time namespace
/// with offsets): the same as [`image`] with the clock functions' entries replaced by svc stubs.
pub fn svc_image() -> &'static [u8] {
    &get().svc
}

/// Whether [`image`] is the host's vDSO, whose clock functions do not enter the kernel layer.
pub fn host() -> bool {
    get().vvar.is_some()
}

/// The host's vvar pages to map below the vDSO; None for the built image.
pub(crate) fn vvar() -> Option<Vvar> {
    get().vvar
}

/// The offsets of the clock functions and the syscalls of their svc stubs.
pub(crate) fn clocks() -> [(u64, u32); 3] {
    get().clocks
}

/// The svc stub of a clock function as bytes ([`svc_entry`]).
pub(crate) fn svc_bytes(nr: u32) -> [u8; 12] {
    let mut b = [0u8; 12];
    for (i, insn) in svc_entry(nr).into_iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&insn.to_le_bytes());
    }
    b
}

/// Offset of `__kernel_rt_sigreturn` from the start of the vDSO.
pub fn sigreturn_offset() -> u64 {
    get().sigreturn
}

fn get() -> &'static Image {
    static IMAGE: OnceLock<Image> = OnceLock::new();
    IMAGE.get_or_init(|| from_host().unwrap_or_else(build))
}

/// Whether the syscall with number `nr` (and first argument `clock`), whose pc after the svc is
/// `pc`, comes from a vDSO clock function's svc stub in place of a read Linux's vDSO makes in user
/// space: tracers do not see it and seccomp does not check it, as on Linux. The syscalls the
/// host's vDSO code makes are real ones, as on Linux, and so are the stubs' for clocks Linux's
/// vDSO leaves to the kernel (CPU-time clocks, alarm clocks, invalid ones).
pub(crate) fn clock_stub(cx: &crate::kernel::Cx<'_>, cpu: &tuxport_engine::Cpu, nr: u64) -> bool {
    let mm = cx.mm();
    let s = cpu.state();
    clock_stub_at((mm.vdso, mm.vdso_svc), s.pc, nr, s.orig_x0)
}

/// Same as [`clock_stub`], with the vDSO mapped at `vdso.0` and `vdso.1` telling whether its clock
/// functions are svc stubs (recorded, for use without the BKL), and `clock` the first argument.
pub(crate) fn clock_stub_at((vdso, svc): (u64, bool), pc: u64, nr: u64, clock: u64) -> bool {
    svc && vdso != 0
        && clocks()
            .iter()
            .any(|&(off, n)| pc == vdso + off + 8 && nr == u64::from(n))
        && (nr == u64::from(GETTIMEOFDAY) || linux_vdso_clock(clock as i32))
}

/// The clocks arm64 Linux's vDSO reads in user space (`VDSO_HRES`, `VDSO_COARSE`, `VDSO_RAW`):
/// realtime, monotonic, monotonic raw, the two coarse clocks, boottime and TAI.
fn linux_vdso_clock(clock: i32) -> bool {
    (0..16).contains(&clock) && 0x8f3 >> clock & 1 != 0
}

// ---- The host's vDSO ----

/// The host's vDSO, prepared for guests; None when it is missing or cannot be used, with the
/// reason in the debug log.
#[cfg(target_arch = "aarch64")]
fn from_host() -> Option<Image> {
    let host = tuxport_hal::host_vdso()?;
    let reject = |why: &str| {
        crate::debug::log(format_args!("host vDSO not used: {why}"));
        None
    };
    let page = tuxport_hal::page_size() as u64;
    let vvar_len = (host.vdso.as_ptr() as usize - host.vvar.as_ptr() as usize) as u64;
    if host.vdso_len > 16 * PAGE_SIZE as usize || vvar_len < page || !vvar_len.is_multiple_of(page)
    {
        return reject("unexpected layout");
    }
    // SAFETY: the host's vDSO mapping is readable for its whole length
    let mut bytes =
        unsafe { std::slice::from_raw_parts(host.vdso.as_ptr(), host.vdso_len) }.to_vec();
    let Some(sigreturn) = lookup(&bytes, SIGRETURN).map(|(at, _)| at) else {
        return reject("no __kernel_rt_sigreturn");
    };
    let at = sigreturn as usize;
    if u32_at(&bytes, at) != Some(mov_x8(RT_SIGRETURN)) || u32_at(&bytes, at + 4) != Some(SVC0) {
        return reject("__kernel_rt_sigreturn is not mov x8, #139; svc #0");
    }
    let mut clocks = [(0, 0); 3];
    for (slot, (name, nr)) in clocks.iter_mut().zip(CLOCKS) {
        // The svc stub must fit inside the function
        match lookup(&bytes, name) {
            Some((at, size)) if size >= 12 => *slot = (at, nr),
            _ => return reject("a clock function is missing or too short"),
        }
    }
    hide_others(&mut bytes);
    set_version_note(&mut bytes);
    // In a time namespace (the development container's; never an Android app's) the first vvar
    // page is the namespace's, whose clock mode (the second word of Linux's `vdso_data` since
    // 5.6) is VDSO_CLOCKMODE_TIMENS, and the code reads the real data from the second page, which
    // is then readable too (Linux's `vvar_fault`)
    // SAFETY: the first vvar page is readable: the host's own vDSO reads it
    let mode = unsafe { host.vvar.as_ptr().add(4).cast::<u32>().read_volatile() };
    let data = if mode == i32::MAX as u32 {
        2 * page
    } else {
        page
    };
    if vvar_len < data {
        return reject("unexpected layout");
    }
    let vvar = Vvar {
        len: vvar_len,
        host: host.vvar.as_ptr() as usize,
        data,
    };
    if !host_works(&bytes, vvar, &clocks) {
        return reject("its clock functions failed the check");
    }
    let mut svc = bytes.clone();
    for &(at, nr) in &clocks {
        svc[at as usize..at as usize + 12].copy_from_slice(&svc_bytes(nr));
    }
    Some(Image {
        bytes,
        svc,
        sigreturn,
        clocks,
        vvar: Some(vvar),
    })
}

#[cfg(not(target_arch = "aarch64"))]
fn from_host() -> Option<Image> {
    None
}

/// Hides every defined symbol but the four exported ones (`__kernel_getrandom` on newer kernels
/// reads vvar pages the guest does not get), by making them undefined: musl, glibc and Go skip
/// undefined symbols.
fn hide_others(b: &mut [u8]) {
    let Some(t) = Tables::of(b) else { return };
    for i in 1..t.nsyms {
        let sym = t.syms + i * 24;
        let (Some(name), Some(shndx)) = (u32_at(b, sym), u16_at(b, sym + 6)) else {
            return;
        };
        // SHN_UNDEF, and reserved sections such as SHN_ABS (the version name's symbol)
        if shndx == 0 || shndx >= 0xff00 {
            continue;
        }
        let keep = cstr(b, t.strings + name as usize)
            .is_some_and(|n| n == SIGRETURN || CLOCKS.iter().any(|&(c, _)| c == n));
        if !keep {
            b[sym + 6..sym + 8].copy_from_slice(&0u16.to_le_bytes());
        }
    }
}

/// Makes the `Linux` note (the version of the kernel the vDSO was built with,
/// `LINUX_VERSION_CODE`) say 6.1.0, the version `uname` reports (glibc's
/// `_dl_discover_osversion` read the kernel version from it).
fn set_version_note(b: &mut [u8]) {
    const VERSION_CODE: u32 = 6 << 16 | 1 << 8;
    let (Some(phoff), Some(phentsize), Some(phnum)) = (u64_at(b, 32), u16_at(b, 54), u16_at(b, 56))
    else {
        return;
    };
    for i in 0..phnum as usize {
        let ph = phoff as usize + i * phentsize as usize;
        // PT_NOTE
        if u32_at(b, ph) != Some(4) {
            continue;
        }
        let (Some(start), Some(size)) = (u64_at(b, ph + 8), u64_at(b, ph + 32)) else {
            return;
        };
        let end = start.saturating_add(size).min(b.len() as u64) as usize;
        let mut at = start as usize;
        // Each note: namesz, descsz, type, then the name and the description, each padded to 4
        while at.saturating_add(12) <= end {
            let (namesz, descsz, kind) = (
                u32_at(b, at).unwrap() as usize,
                u32_at(b, at + 4).unwrap() as usize,
                u32_at(b, at + 8).unwrap(),
            );
            let desc = (at + 12).saturating_add(namesz.next_multiple_of(4));
            let linux = b.get(at + 12..at + 12 + namesz.min(8)) == Some(b"Linux\0");
            if let Some(d) = b
                .get_mut(desc..desc + 4)
                .filter(|_| linux && kind == 0 && descsz == 4)
            {
                d.copy_from_slice(&VERSION_CODE.to_le_bytes());
            }
            at = desc.saturating_add(descsz.next_multiple_of(4));
        }
    }
}

/// Runs the clock functions of `image` (the host's vDSO code), with the host's vvar data mapped
/// below it as `vvar` says, in the interpreter of a private address space, and compares the
/// results with the host's own calls: each reading must lie between host readings taken just
/// before and after, and resolutions and the timezone must be equal. Where the code falls back to
/// a syscall, the host answers it. Any other exit (an instruction the engine does not execute, a
/// fault) or a run of more than 100000 instructions fails it.
#[cfg(target_arch = "aarch64")]
fn host_works(image: &[u8], vvar: Vvar, clocks: &[(u64, u32); 3]) -> bool {
    use std::ptr::NonNull;

    use tuxport_engine::{Cpu, Exit, Mmu, prot};

    /// Host memory of the check: a page whose svc marks the return, a stack page, then the code.
    struct Pages(NonNull<u8>, usize);
    impl Drop for Pages {
        fn drop(&mut self) {
            // SAFETY: mapped by tuxport_hal::map_anonymous below and no longer used
            unsafe { tuxport_hal::unmap(self.0, self.1) }
        }
    }
    const DONE: u64 = 0x1000_0000;
    const STACK: u64 = 0x2000_0000;
    const TEXT: u64 = 0x4000_0000;
    let page = tuxport_hal::page_size();
    let text_len = image.len().next_multiple_of(page);
    let Ok(base) = tuxport_hal::map_anonymous(2 * page + text_len, false) else {
        return false;
    };
    let _pages = Pages(base, 2 * page + text_len);
    // SAFETY: the three pieces lie inside the mapping just made, which is readable and writable
    let (done, stack, text) = unsafe {
        let (done, text) = (base, base.add(2 * page));
        done.cast::<u32>().write(SVC0);
        std::ptr::copy_nonoverlapping(image.as_ptr(), text.as_ptr(), image.len());
        (done, base.add(page), text)
    };
    let mut mmu = Mmu::new();
    let vvar_host = NonNull::new(vvar.host as *mut u8).unwrap();
    let rx = prot::READ | prot::EXEC;
    // SAFETY: each piece of host memory outlives the Mmu (declared after `_pages`, dropped
    // before it); the vvar page is only read
    let mapped = unsafe {
        mmu.map(DONE, done, page as u64, rx).is_ok()
            && mmu
                .map(STACK, stack, page as u64, prot::READ | prot::WRITE)
                .is_ok()
            && mmu.map(TEXT, text, text_len as u64, rx).is_ok()
            && mmu
                .map(TEXT - vvar.len, vvar_host, vvar.data, prot::READ)
                .is_ok()
    };
    if !mapped {
        return false;
    }
    // SAFETY: mmu outlives cpu (declared later, dropped first)
    let mut cpu = unsafe { Cpu::new(&mmu) };
    let buf = STACK + page as u64 - 64;

    // Calls the function at `entry` with x0, x1 = `args`; returns x0
    let call = |cpu: &mut Cpu, entry: u64, args: [u64; 2]| -> Option<u64> {
        let s = cpu.state_mut();
        s.x = [0; 31];
        (s.x[0], s.x[1], s.x[30]) = (args[0], args[1], DONE);
        (s.sp, s.pc) = (STACK + page as u64, TEXT + entry);
        for _ in 0..100_000 {
            match cpu.step() {
                None => {}
                Some(Exit::Svc { .. }) if cpu.state().pc == DONE + 4 => {
                    return Some(cpu.state().x[0]);
                }
                Some(Exit::Svc { .. }) => fall_back(cpu)?,
                Some(_) => return None,
            }
        }
        None
    };
    let read = |cpu: &Cpu, at: u64| -> Option<(i64, i64)> {
        let mut b = [0u8; 16];
        guest_copy(cpu, at, &mut b, false)?;
        Some(pair_of(&b))
    };
    let [gettime, gettimeofday, getres] = clocks.map(|(at, _)| at);
    // Realtime, monotonic, monotonic raw, the coarse ones, boottime and TAI
    for clock in [0, 1, 4, 5, 6, 7, 11] {
        let Ok(before) = tuxport_hal::clock_gettime(clock) else {
            continue;
        };
        let r = call(&mut cpu, gettime, [clock as u64, buf]);
        let Ok(after) = tuxport_hal::clock_gettime(clock) else {
            return false;
        };
        let got = read(&cpu, buf);
        if r != Some(0) || !got.is_some_and(|t| before <= t && t <= after && t.1 < 1_000_000_000) {
            return false;
        }
    }
    // gettimeofday: microseconds, and the host's timezone as two 32-bit fields
    let micros = |(sec, nsec): (i64, i64)| (sec, nsec / 1000);
    let (Ok(before), r, Ok(after)) = (
        tuxport_hal::clock_gettime(0).map(micros),
        call(&mut cpu, gettimeofday, [buf, buf + 16]),
        tuxport_hal::clock_gettime(0).map(micros),
    ) else {
        return false;
    };
    let (tv, tz) = (read(&cpu, buf), read(&cpu, buf + 16));
    let (west, dst) = tuxport_hal::timezone();
    let tz_ok = tz.is_some_and(|(t, _)| t as i32 == west && (t >> 32) as i32 == dst);
    if r != Some(0) || !tv.is_some_and(|t| before <= t && t <= after) || !tz_ok {
        return false;
    }
    for clock in [1, 5] {
        let r = call(&mut cpu, getres, [clock as u64, buf]);
        if r != Some(0) || read(&cpu, buf) != tuxport_hal::clock_getres(clock).ok() {
            return false;
        }
    }
    true
}

/// Answers a syscall the host's vDSO code made during [`host_works`] (a clock it leaves to the
/// kernel) with the host's own call; None for anything else.
#[cfg(target_arch = "aarch64")]
fn fall_back(cpu: &mut tuxport_engine::Cpu) -> Option<()> {
    let s = cpu.state();
    let (a0, a1) = (s.x[0], s.x[1]);
    let both = |(a, b): (i64, i64)| {
        let mut out = [0u8; 16];
        out[..8].copy_from_slice(&a.to_le_bytes());
        out[8..].copy_from_slice(&b.to_le_bytes());
        out.to_vec()
    };
    // (where, what) to write
    let writes = match s.x[8] as u32 {
        CLOCK_GETTIME => tuxport_hal::clock_gettime(a0 as i32).map(|t| vec![(a1, both(t))]),
        CLOCK_GETRES => tuxport_hal::clock_getres(a0 as i32).map(|t| vec![(a1, both(t))]),
        GETTIMEOFDAY => tuxport_hal::clock_gettime(0).map(|(sec, nsec)| {
            let (west, dst) = tuxport_hal::timezone();
            let tz = [west.to_le_bytes(), dst.to_le_bytes()].concat();
            vec![(a0, both((sec, nsec / 1000))), (a1, tz)]
        }),
        _ => return None,
    };
    let r = match writes {
        Ok(writes) => {
            for (at, mut bytes) in writes.into_iter().filter(|&(at, _)| at != 0) {
                guest_copy(cpu, at, &mut bytes, true)?;
            }
            0
        }
        Err(e) => -i64::from(e) as u64,
    };
    cpu.state_mut().x[0] = r;
    Some(())
}

/// Copies between `buf` and guest memory at `at` (into guest memory with `write`) during
/// [`host_works`], within one page.
#[cfg(target_arch = "aarch64")]
fn guest_copy(cpu: &tuxport_engine::Cpu, at: u64, buf: &mut [u8], write: bool) -> Option<()> {
    use tuxport_engine::prot;
    let end = at.checked_add(buf.len() as u64)?;
    if (end - 1) / PAGE_SIZE != at / PAGE_SIZE {
        return None;
    }
    let host = cpu.translate(at, if write { prot::WRITE } else { prot::READ })?;
    // SAFETY: `buf.len()` bytes within one mapped page, writable when written
    unsafe {
        if write {
            std::ptr::copy_nonoverlapping(buf.as_ptr(), host.as_ptr(), buf.len());
        } else {
            std::ptr::copy_nonoverlapping(host.as_ptr(), buf.as_mut_ptr(), buf.len());
        }
    }
    Some(())
}

/// A `struct timespec` or `struct timeval` as (seconds, fraction).
#[cfg(target_arch = "aarch64")]
fn pair_of(b: &[u8; 16]) -> (i64, i64) {
    let half = |at: usize| i64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    (half(0), half(8))
}

// ---- The built image ----

/// The minimal image: a real ELF shared object (`ET_DYN`, a loadable segment and a dynamic
/// segment), whose clock functions are svc stubs.
fn build() -> Image {
    // Code: the nop before sigreturn belongs to no symbol
    let sigreturn: &[u32] = &[mov_x8(RT_SIGRETURN), SVC0];
    let stubs = CLOCKS.map(|(_, nr)| svc_entry(nr));
    let functions: [(&[u8], &[u32]); 4] = [
        (SIGRETURN, sigreturn),
        (CLOCKS[0].0, &stubs[0]),
        (CLOCKS[1].0, &stubs[1]),
        (CLOCKS[2].0, &stubs[2]),
    ];
    let nsyms = functions.len() + 1;

    // String table
    let mut strtab = vec![0u8];
    let mut add_str = |s: &[u8]| {
        let off = strtab.len() as u32;
        strtab.extend_from_slice(s);
        strtab.push(0);
        off
    };
    let name_offs: Vec<u32> = functions.iter().map(|(n, _)| add_str(n)).collect();
    let version_name = add_str(VERSION);
    let soname = add_str(SONAME);

    // Fixed header: the ELF header and two program headers (PT_LOAD, PT_DYNAMIC)
    const EHDR: usize = 64;
    const PHDR: usize = 56;
    let mut b = vec![0u8; EHDR + 2 * PHDR];

    // .hash: one bucket, with all symbols on one chain
    let hash = b.len();
    put32(&mut b, 1);
    put32(&mut b, nsyms as u32);
    put32(&mut b, 1);
    for i in 0..nsyms {
        let next = if i == 0 || i + 1 == nsyms { 0 } else { i + 1 };
        put32(&mut b, next as u32);
    }

    // .dynsym: placeholders first; the code addresses are filled in last
    align(&mut b, 8);
    let symtab = b.len();
    b.resize(symtab + nsyms * 24, 0);

    // .gnu.version: symbol 0 is local, all others belong to version 2 (LINUX_2.6.39)
    let versym = b.len();
    put16(&mut b, 0);
    for _ in &functions {
        put16(&mut b, 2);
    }

    // .gnu.version_d: version 1 is the soname (VER_FLG_BASE), version 2 is LINUX_2.6.39
    align(&mut b, 8);
    let verdef = b.len();
    for (ndx, flags, name, last) in [
        (1u16, 1u16, (SONAME, soname), false),
        (2, 0, (VERSION, version_name), true),
    ] {
        put16(&mut b, 1); // vd_version
        put16(&mut b, flags);
        put16(&mut b, ndx);
        put16(&mut b, 1); // vd_cnt
        put32(&mut b, elf_hash(name.0));
        put32(&mut b, 20); // vd_aux: right after the Verdef
        put32(&mut b, if last { 0 } else { 28 }); // vd_next
        put32(&mut b, name.1); // vda_name
        put32(&mut b, 0); // vda_next
    }

    let strtab_off = b.len();
    b.extend_from_slice(&strtab);

    align(&mut b, 8);
    let dynamic = b.len();
    let dyn_entries: [(u64, u64); 10] = [
        (4, hash as u64),             // DT_HASH
        (5, strtab_off as u64),       // DT_STRTAB
        (6, symtab as u64),           // DT_SYMTAB
        (10, strtab.len() as u64),    // DT_STRSZ
        (11, 24),                     // DT_SYMENT
        (14, soname as u64),          // DT_SONAME
        (0x6fff_fff0, versym as u64), // DT_VERSYM
        (0x6fff_fffc, verdef as u64), // DT_VERDEF
        (0x6fff_fffd, 2),             // DT_VERDEFNUM
        (0, 0),                       // DT_NULL
    ];
    for (tag, val) in dyn_entries {
        put64(&mut b, tag);
        put64(&mut b, val);
    }
    let dynamic_len = b.len() - dynamic;

    // .text
    align(&mut b, 16);
    put32(&mut b, NOP);
    let mut addrs = Vec::new();
    for (_, code) in &functions {
        addrs.push((b.len() as u64, code.len() as u64 * 4));
        for &insn in *code {
            put32(&mut b, insn);
        }
    }
    let size = b.len() as u64;
    assert!(size <= PAGE_SIZE, "vDSO does not fit in one page");

    // Fill in the symbol table
    for (i, (&name, &(value, len))) in name_offs.iter().zip(&addrs).enumerate() {
        let at = symtab + (i + 1) * 24;
        let mut sym = Vec::with_capacity(24);
        put32(&mut sym, name);
        sym.push((1 << 4) | 2); // STB_GLOBAL, STT_FUNC
        sym.push(0);
        // No section header table; parsers only require st_shndx not to be SHN_UNDEF
        put16(&mut sym, 1);
        put64(&mut sym, value);
        put64(&mut sym, len);
        b[at..at + 24].copy_from_slice(&sym);
    }

    // ELF header
    let mut h = Vec::with_capacity(EHDR + 2 * PHDR);
    h.extend_from_slice(b"\x7fELF\x02\x01\x01");
    h.resize(16, 0);
    put16(&mut h, 3); // ET_DYN
    put16(&mut h, 183); // EM_AARCH64
    put32(&mut h, 1);
    put64(&mut h, 0); // e_entry
    put64(&mut h, EHDR as u64); // e_phoff
    put64(&mut h, 0); // e_shoff
    put32(&mut h, 0); // e_flags
    put16(&mut h, EHDR as u16);
    put16(&mut h, PHDR as u16);
    put16(&mut h, 2); // e_phnum
    put16(&mut h, 64); // e_shentsize
    put16(&mut h, 0); // e_shnum
    put16(&mut h, 0); // e_shstrndx
    // PT_LOAD: the whole image, readable and executable
    put32(&mut h, 1);
    put32(&mut h, 5);
    for v in [0, 0, 0, size, size, PAGE_SIZE] {
        put64(&mut h, v);
    }
    // PT_DYNAMIC
    put32(&mut h, 2);
    put32(&mut h, 4);
    for v in [dynamic as u64, dynamic as u64, dynamic as u64] {
        put64(&mut h, v);
    }
    put64(&mut h, dynamic_len as u64);
    put64(&mut h, dynamic_len as u64);
    put64(&mut h, 8);
    b[..h.len()].copy_from_slice(&h);

    let clocks = [1, 2, 3].map(|i| (addrs[i].0, CLOCKS[i - 1].1));
    Image {
        svc: b.clone(),
        bytes: b,
        sigreturn: addrs[0].0,
        clocks,
        vvar: None,
    }
}

/// The System V ABI ELF hash.
fn elf_hash(name: &[u8]) -> u32 {
    let mut h: u32 = 0;
    for &c in name {
        h = (h << 4).wrapping_add(c as u32);
        let g = h & 0xf000_0000;
        if g != 0 {
            h ^= g >> 24;
        }
        h &= !g;
    }
    h
}

fn put16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn align(b: &mut Vec<u8>, to: usize) {
    b.resize(b.len().next_multiple_of(to), 0);
}

// ---- Reading an image ----

/// The dynamic tables of a vDSO image, as offsets into it.
struct Tables {
    strings: usize,
    syms: usize,
    nsyms: usize,
    versym: Option<usize>,
    verdef: Option<usize>,
}

impl Tables {
    /// Finds the tables as musl's `__vdsosym` does: the load bias from `PT_LOAD`, the dynamic
    /// entries from `PT_DYNAMIC`, the symbol count from `DT_HASH`. None when something is missing
    /// or outside the image.
    fn of(b: &[u8]) -> Option<Tables> {
        let (phoff, phentsize) = (u64_at(b, 32)? as usize, u16_at(b, 54)? as usize);
        let (mut dynv, mut bias) = (None, None);
        for i in 0..u16_at(b, 56)? as usize {
            let ph = phoff.checked_add(i.checked_mul(phentsize)?)?;
            match u32_at(b, ph)? {
                1 => bias = Some(u64_at(b, ph + 8)?.wrapping_sub(u64_at(b, ph + 16)?)),
                2 => dynv = Some(u64_at(b, ph + 8)? as usize),
                _ => {}
            }
        }
        let (dynv, bias) = (dynv?, bias?);
        let at =
            |v: u64| Some(usize::try_from(v.wrapping_add(bias)).ok()?).filter(|&a| a < b.len());
        let (mut strings, mut syms, mut hash, mut versym, mut verdef) =
            (None, None, None, None, None);
        let mut d = dynv;
        loop {
            let tag = u64_at(b, d)?;
            if tag == 0 {
                break;
            }
            let val = u64_at(b, d + 8)?;
            match tag {
                4 => hash = at(val),
                5 => strings = at(val),
                6 => syms = at(val),
                0x6fff_fff0 => versym = at(val),
                0x6fff_fffc => verdef = at(val),
                _ => {}
            }
            d += 16;
        }
        let (strings, syms) = (strings?, syms?);
        let nsyms = u32_at(b, hash? + 4)? as usize;
        (syms.checked_add(nsyms.checked_mul(24)?)? <= b.len()).then_some(Tables {
            strings,
            syms,
            nsyms,
            // musl ignores the versions unless both tables are present
            versym: versym.filter(|_| verdef.is_some()),
            verdef,
        })
    }
}

/// Looks `name` of version `LINUX_2.6.39` up as musl's `__vdsosym` does (src/internal/vdso.c), the
/// strictest of the vDSO parsers: its offset in the image and its size.
fn lookup(b: &[u8], name: &[u8]) -> Option<(u64, u64)> {
    // STT_NOTYPE, STT_OBJECT, STT_FUNC, STT_COMMON; STB_GLOBAL, STB_WEAK, STB_GNU_UNIQUE
    const OK_TYPES: u32 = 1 | 1 << 1 | 1 << 2 | 1 << 5;
    const OK_BINDS: u32 = 1 << 1 | 1 << 2 | 1 << 10;
    let t = Tables::of(b)?;
    (0..t.nsyms).find_map(|i| {
        let sym = t.syms + i * 24;
        let info = u32::from(*b.get(sym + 4)?);
        if 1 << (info & 0xf) & OK_TYPES == 0 || 1 << (info >> 4) & OK_BINDS == 0 {
            return None;
        }
        if u16_at(b, sym + 6)? == 0 || cstr(b, t.strings + u32_at(b, sym)? as usize)? != name {
            return None;
        }
        if let Some(versym) = t.versym {
            let vsym = u16_at(b, versym + 2 * i)?;
            if !has_version(b, &t, vsym)? {
                return None;
            }
        }
        Some((u64_at(b, sym + 8)?, u64_at(b, sym + 16)?))
    })
}

/// musl's `checkver`: whether version index `vsym` is `LINUX_2.6.39`.
fn has_version(b: &[u8], t: &Tables, vsym: u16) -> Option<bool> {
    let mut def = t.verdef?;
    for _ in 0..64 {
        // vd_flags & VER_FLG_BASE, vd_ndx
        if u16_at(b, def + 2)? & 1 == 0 && u16_at(b, def + 4)? & 0x7fff == vsym & 0x7fff {
            let aux = def + u32_at(b, def + 12)? as usize;
            return Some(cstr(b, t.strings + u32_at(b, aux)? as usize)? == VERSION);
        }
        match u32_at(b, def + 16)? {
            0 => return Some(false),
            next => def += next as usize,
        }
    }
    None
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

fn cstr(b: &[u8], at: usize) -> Option<&[u8]> {
    let rest = b.get(at..)?;
    Some(&rest[..rest.iter().position(|&c| c == 0)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checks what both kinds of image must provide, looking symbols up the way musl does.
    fn check_exports(b: &[u8], sigreturn: u64) {
        let at = lookup(b, SIGRETURN).unwrap().0;
        assert_eq!(at, sigreturn);
        assert_eq!(u32_at(b, at as usize - 4), Some(NOP));
        assert_eq!(u32_at(b, at as usize), Some(0xd280_1168)); // mov x8, #139
        assert_eq!(u32_at(b, at as usize + 4), Some(SVC0));
        for (name, _) in CLOCKS {
            assert!(lookup(b, name).is_some_and(|(_, size)| size >= 12));
        }
        assert!(lookup(b, b"__kernel_time").is_none());
        assert!(lookup(b, b"__kernel_getrandom").is_none());
    }

    #[test]
    fn the_built_image_exports_svc_stubs() {
        let image = build();
        check_exports(&image.bytes, image.sigreturn);
        assert_eq!(image.svc, image.bytes);
        let gettime = image.clocks[0].0 as usize;
        assert_eq!(u32_at(&image.bytes, gettime), Some(mov_x8(CLOCK_GETTIME)));
        assert_eq!(image.bytes[gettime..gettime + 12], svc_bytes(CLOCK_GETTIME));
        // musl also checks the version name
        let mut renamed = image.bytes.clone();
        let at = renamed
            .windows(VERSION.len())
            .position(|w| w == VERSION)
            .unwrap();
        renamed[at + VERSION.len() - 1] = b'8';
        assert!(lookup(&renamed, CLOCKS[0].0).is_none());
    }

    /// The development and CI hosts are arm64 Linux: their own vDSO is used, with its notes and
    /// symbols changed only as Tuxport's contract needs.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn the_host_vdso_is_used() {
        assert!(host());
        let b = image();
        check_exports(b, sigreturn_offset());
        let vvar = vvar().unwrap();
        assert!(vvar.len >= vvar.data && vvar.len.is_multiple_of(PAGE_SIZE));
        // The version notes (name "Linux", type 0, a 4-byte description) say 6.1.0
        for at in (4..b.len() - 12).filter(|&at| &b[at..at + 6] == b"Linux\0") {
            if u32_at(b, at - 4) == Some(0) && u32_at(b, at - 8) == Some(4) {
                assert_eq!(u32_at(b, at + 8), Some(0x0006_0100));
            }
        }
        // Only the clock functions' entries differ in the svc image
        let svc = svc_image();
        assert_eq!(svc.len(), b.len());
        for (at, nr) in clocks() {
            let at = at as usize;
            assert_eq!(svc[at..at + 12], svc_bytes(nr));
        }
        let differing = b.iter().zip(svc).filter(|(x, y)| x != y).count();
        assert!(differing <= 36);
    }

    #[test]
    fn stubs_count_only_clocks_linux_reads_in_user_space() {
        let vdso = 0x7000_0000;
        let [gettime, gettimeofday, _] = clocks().map(|(at, _)| vdso + at + 8);
        let monotonic = 1;
        assert!(clock_stub_at((vdso, true), gettime, 113, monotonic));
        // The process CPU-time clock goes to the kernel on Linux too
        assert!(!clock_stub_at((vdso, true), gettime, 113, 2));
        assert!(clock_stub_at((vdso, true), gettimeofday, 169, 0));
        // Not at a stub, or the host's code making a real syscall
        assert!(!clock_stub_at((vdso, true), gettime + 4, 113, monotonic));
        assert!(!clock_stub_at((vdso, true), gettime, 169, monotonic));
        assert!(!clock_stub_at((vdso, false), gettime, 113, monotonic));
    }
}
