//! Core dumps (Linux's `do_coredump` and `elf_core_dump`, the kernel/signal map).
//!
//! When a signal whose default action produces a core ([`dumps_core`]) ends a process, the other
//! threads are ended first and waited for: an exiting thread leaves its registers in the thread
//! group ([`ThreadCore`], `task::exit`). Then an ELF core file is written in the process's current
//! directory according to `/proc/sys/kernel/core_pattern`: one `PT_LOAD` per mapping (how much is
//! written, see [`Mm::dump_regions`]), and one `PT_NOTE` holding each thread's registers
//! (`NT_PRSTATUS`, `NT_PRFPREG`, `NT_ARM_TLS` and so on) and the process's `NT_PRPSINFO`,
//! `NT_SIGINFO`, `NT_AUXV` and `NT_FILE`, the same as arm64 Linux writes, so gdb can analyse it
//! afterwards.
//!
//! As on Linux, nothing is written when `RLIMIT_CORE` is less than a page or the process is not
//! dumpable (`PR_SET_DUMPABLE`, setuid programs); once the bytes written (holes not counted)
//! reach `RLIMIT_CORE`, the file is truncated. A pipe `core_pattern` (`|program`) is not
//! supported and nothing is written. For a process whose core file was created (truncated ones
//! count too, as with Linux's `has_dumped`), the wait status has `WCOREDUMP`, and the `si_code` of
//! SIGCHLD and waitid is `CLD_DUMPED`.

use crate::errno::Errno;
use crate::fd::FileRef;
use crate::kernel::{Cx, Tid, wake};
use crate::mm::{DumpRegion, Mm};
use crate::process::{CpuTime, RLIMIT_CORE, Signal, ThreadGroup};
use crate::ptrace::{HwDebug, Regs};
use crate::signal::SigInfo;
use crate::task::Task;
use tuxport_engine::{CpuState, PAGE_SIZE, prot};

/// Maximum length of `core_pattern` (Linux's `CORENAME_MAX_SIZE`).
pub(crate) const PATTERN_MAX: usize = 128;

/// Signals whose default action produces a core (Linux's `SIG_KERNEL_COREDUMP_MASK`): SIGQUIT,
/// SIGILL, SIGTRAP, SIGABRT, SIGBUS, SIGFPE, SIGSEGV, SIGXCPU, SIGXFSZ, SIGSYS.
pub(crate) fn dumps_core(sig: Signal) -> bool {
    matches!(sig.number(), 3..=8 | 11 | 24 | 25 | 31)
}

/// The state a thread leaves for the core file (Linux's `elf_thread_core_info`).
pub(crate) struct ThreadCore {
    tid: Tid,
    regs: Regs,
    /// Hardware breakpoints and watchpoints set by the tracer (`NT_ARM_HW_BREAK`,
    /// `NT_ARM_HW_WATCH`).
    hw: HwDebug,
    /// The thread's own pending signals and signal mask.
    pending: u64,
    blocked: u64,
    cpu: CpuTime,
}

impl ThreadCore {
    /// The current state of thread `task`, with `state` as its registers.
    pub(crate) fn of(task: &Task, state: &CpuState) -> ThreadCore {
        ThreadCore {
            tid: task.tid.get(),
            regs: Regs::save(state),
            hw: task.trace.borrow().hw,
            pending: task.pending.borrow().set(),
            blocked: task.sigmask.get(),
            cpu: task.cpu_self(),
        }
    }
}

/// The fatal signal `sig` (with `info`) is ending the caller's thread group: writes the core
/// file. The caller has already made the thread group exit (`signal::group_exit`); this waits for
/// the other threads to exit and leave their registers, then writes. Returns whether it was
/// written.
pub(crate) fn dump(cx: &mut Cx<'_>, state: &CpuState, sig: Signal, info: &SigInfo) -> bool {
    let group = cx.t.group.clone();
    {
        let g = group.borrow();
        // As on Linux, nothing is written when the limit is below ELF's minimum core (one page)
        if g.rlimits[RLIMIT_CORE as usize].0 < PAGE_SIZE || !g.dumpable {
            return false;
        }
    }
    let k = cx.kr();
    if k.core_pattern.first() == Some(&b'|') {
        crate::debug::log(format_args!(
            "core dump: pipe core_pattern is not supported"
        ));
        return false;
    }
    // Wait for the other threads to exit; on exit they leave their registers in group.core
    group.borrow_mut().core = Some(Vec::new());
    while group.borrow().threads.len() > 1 {
        let _ = cx.wait_on(None, &mut [], wake::EXITED);
    }
    let others = group.borrow_mut().core.take().unwrap_or_default();
    let mut threads = vec![ThreadCore::of(cx.task, state)];
    threads.extend(others);
    let name = core_name(cx, sig);
    match write(cx, &name, &threads, sig, info) {
        Ok(()) => true,
        Err(e) => {
            crate::debug::log(format_args!(
                "core dump: {}: errno {}",
                String::from_utf8_lossy(&name),
                e.0
            ));
            false
        }
    }
}

/// Expands the core file name from `core_pattern` (Linux's `format_corename`).
fn core_name(cx: &Cx<'_>, sig: Signal) -> Vec<u8> {
    let k = cx.kr();
    let g = cx.t.group.borrow();
    let pattern = k.core_pattern.clone();
    let mut out = Vec::new();
    let mut has_pid = false;
    let mut it = pattern.iter().copied();
    while let Some(c) = it.next() {
        if c != b'%' {
            out.push(c);
            continue;
        }
        let spec = match it.next() {
            Some(s) => s,
            None => break,
        };
        // %p and %i are the ids in the process's own pid namespace, %P and %I those in the boot
        // namespace
        let vnr = |id: Tid| g.pid_ns.vnr(id).unwrap_or(0).to_string();
        let text = match spec {
            b'%' => "%".to_owned(),
            b'p' => {
                has_pid = true;
                vnr(g.tgid)
            }
            b'P' => {
                has_pid = true;
                g.tgid.to_string()
            }
            b'i' => vnr(cx.tid()),
            b'I' => cx.tid().to_string(),
            b'u' => cx.cred().ruid.to_string(),
            b'g' => cx.cred().rgid.to_string(),
            b's' => sig.number().to_string(),
            b't' => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
                .to_string(),
            b'h' => {
                String::from_utf8_lossy(&cx.task.ns.borrow().uts.hostname.borrow()).into_owned()
            }
            b'e' => {
                // A / in the name becomes ! (Linux's cn_esc_printf)
                let comm = cx.task.comm.get();
                let len = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
                String::from_utf8_lossy(&comm[..len]).replace('/', "!")
            }
            b'E' => g
                .exe
                .as_ref()
                .map(|e| String::from_utf8_lossy(&e.path).replace('/', "!"))
                .unwrap_or_default(),
            b'c' => g.rlimits[RLIMIT_CORE as usize].0.to_string(),
            // The dumpable mode (Linux's get_dumpable: 1 for a process that may dump)
            b'd' => (g.dumpable as u32).to_string(),
            // Unknown specifiers: output nothing, as on Linux
            _ => String::new(),
        };
        out.extend_from_slice(text.as_bytes());
    }
    if k.core_uses_pid && !has_pid {
        let pid = g.pid_ns.vnr(g.tgid).unwrap_or(0);
        out.extend_from_slice(format!(".{pid}").as_bytes());
    }
    out
}

/// Writes the core file in the process's current directory (or at the name if it is an absolute
/// path). As on Linux, a file with the same name is unlinked first, then a new one is created with
/// 0600 (`O_EXCL | O_NOFOLLOW`); once created it counts as written, and it is truncated when the
/// writes reach `RLIMIT_CORE` or fail.
fn write(
    cx: &mut Cx<'_>,
    name: &[u8],
    threads: &[ThreadCore],
    sig: Signal,
    info: &SigInfo,
) -> Result<(), Errno> {
    let mm = cx.t.mm.clone();
    let (regions, notes) = {
        let mm = mm.borrow();
        let filter = cx.t.group.borrow().coredump_filter;
        let regions = mm.dump_regions(filter);
        let notes = notes(cx, &mm, &regions, threads, sig, info);
        (regions, notes)
    };
    let limit = cx.t.group.borrow().rlimits[RLIMIT_CORE as usize].0;
    let (header, data_offset) = elf_header(&regions, &notes);
    let _ = crate::syscall::unlink_path(cx, name);
    let file = crate::syscall::create_exclusive(cx, name, 0o600)?;
    let mut w = CoreWriter {
        file,
        limit,
        written: 0,
        end: 0,
        stopped: false,
    };
    w.put(cx, 0, &header);
    let mut offset = data_offset;
    let mut page = vec![0u8; PAGE_SIZE as usize];
    for r in regions.iter().filter(|r| r.size > 0) {
        // Untouched pages of anonymous and shared memory are left as holes (Linux's FOLL_DUMP)
        let resident = if r.sparse {
            mm.borrow().residency(r.start, r.size).ok()
        } else {
            None
        };
        let mut addr = r.start;
        while addr < r.start + r.size && !w.stopped {
            let index = ((addr - r.start) / PAGE_SIZE) as usize;
            let touched = resident
                .as_ref()
                .is_none_or(|v| v.get(index) == Some(&true));
            if touched && mm.borrow().dump_page(addr, &mut page) {
                w.put(cx, offset, &page);
            }
            addr += PAGE_SIZE;
            offset += PAGE_SIZE;
        }
    }
    // When it ends in a hole, the file still needs its full length
    if w.end < offset {
        w.put(cx, offset - 1, &[0]);
    }
    Ok(())
}

/// Writes to the core file. As with Linux's `dump_emit`, writing stops once the bytes written
/// (holes not counted) exceed `RLIMIT_CORE`, or after a write error.
struct CoreWriter {
    file: FileRef,
    limit: u64,
    written: u64,
    /// The farthest position written.
    end: u64,
    stopped: bool,
}

impl CoreWriter {
    fn put(&mut self, cx: &mut Cx<'_>, offset: u64, data: &[u8]) {
        if self.stopped || self.written + data.len() as u64 > self.limit {
            self.stopped = true;
            return;
        }
        let mut done = 0;
        while done < data.len() {
            match crate::syscall::write_at(cx, &self.file, &data[done..], offset + done as u64) {
                Ok(n) if n > 0 => done += n,
                _ => {
                    self.stopped = true;
                    return;
                }
            }
        }
        self.written += data.len() as u64;
        self.end = self.end.max(offset + data.len() as u64);
    }
}

// ---- ELF ----

const ET_CORE: u16 = 4;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;
const PN_XNUM: u16 = 0xffff;
const EHDR_SIZE: u64 = 64;
const PHDR_SIZE: u64 = 56;

const NT_PRSTATUS: u32 = 1;
const NT_PRFPREG: u32 = 2;
const NT_PRPSINFO: u32 = 3;
const NT_AUXV: u32 = 6;
const NT_SIGINFO: u32 = 0x5349_4749;
const NT_FILE: u32 = 0x4649_4c45;
const NT_ARM_TLS: u32 = 0x401;
const NT_ARM_HW_BREAK: u32 = 0x402;
const NT_ARM_HW_WATCH: u32 = 0x403;
const NT_ARM_SYSTEM_CALL: u32 = 0x404;

/// All notes, in the order of Linux's `write_note_info`: for each thread `NT_PRSTATUS` first;
/// after the first thread (the one that raised the signal) come the process's `NT_PRPSINFO`,
/// `NT_SIGINFO`, `NT_AUXV` and `NT_FILE`, then that thread's other register sets.
fn notes(
    cx: &Cx<'_>,
    mm: &Mm,
    regions: &[DumpRegion],
    threads: &[ThreadCore],
    sig: Signal,
    info: &SigInfo,
) -> Vec<u8> {
    let g = cx.t.group.borrow();
    // The process's CPU time: exited threads are already added to g.cpu, the thread that raised
    // the signal is not yet
    let process_cpu = g.cpu + threads[0].cpu;
    let mut out = Vec::new();
    for (i, t) in threads.iter().enumerate() {
        note(
            &mut out,
            "CORE",
            NT_PRSTATUS,
            &prstatus(&g, t, sig, process_cpu),
        );
        if i == 0 {
            note(&mut out, "CORE", NT_PRPSINFO, &prpsinfo(cx, &g, mm));
            note(&mut out, "CORE", NT_SIGINFO, &info.encode(sig));
            let auxv: Vec<u8> = mm.auxv.iter().flat_map(|w| w.to_le_bytes()).collect();
            note(&mut out, "CORE", NT_AUXV, &auxv);
            note(&mut out, "CORE", NT_FILE, &files(regions));
        }
        // The other register sets are the same as PTRACE_GETREGSET reads; not in a syscall, so
        // the number is -1
        for (name, nt) in [
            ("CORE", NT_PRFPREG),
            ("LINUX", NT_ARM_TLS),
            ("LINUX", NT_ARM_HW_BREAK),
            ("LINUX", NT_ARM_HW_WATCH),
            ("LINUX", NT_ARM_SYSTEM_CALL),
        ] {
            let desc = crate::syscall::encode_regset(u64::from(nt), &t.regs, -1, &t.hw);
            note(&mut out, name, nt, &desc);
        }
    }
    out
}

/// One note: the name and the contents are each aligned to 4 bytes.
fn note(out: &mut Vec<u8>, name: &str, ty: u32, desc: &[u8]) {
    out.extend_from_slice(&(name.len() as u32 + 1).to_le_bytes());
    out.extend_from_slice(&(desc.len() as u32).to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.resize(out.len().next_multiple_of(4), 0);
    out.extend_from_slice(desc);
    out.resize(out.len().next_multiple_of(4), 0);
}

/// `struct elf_prstatus` (arm64, 392 bytes). As on Linux, the thread group's main thread reports
/// the whole process's CPU time (`process_cpu`), other threads their own.
fn prstatus(g: &ThreadGroup, t: &ThreadCore, sig: Signal, process_cpu: CpuTime) -> Vec<u8> {
    let mut b = vec![0u8; 392];
    b[..4].copy_from_slice(&sig.number().to_le_bytes());
    b[12..14].copy_from_slice(&(sig.number() as u16).to_le_bytes());
    b[16..24].copy_from_slice(&t.pending.to_le_bytes());
    b[24..32].copy_from_slice(&t.blocked.to_le_bytes());
    // pids are the ids in the process's own pid namespace (Linux's fill_prstatus uses
    // task_pid_vnr)
    for (i, &v) in [t.tid, g.ppid, g.pgid, g.sid].iter().enumerate() {
        let v = g.pid_ns.vnr(v).unwrap_or(0);
        b[32 + i * 4..36 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    let cpu = if t.tid == g.tgid { process_cpu } else { t.cpu };
    b[48..64].copy_from_slice(&timeval(cpu.user_ns()));
    b[64..80].copy_from_slice(&timeval(cpu.sys_ns));
    b[80..96].copy_from_slice(&timeval(g.child_cpu.user_ns()));
    b[96..112].copy_from_slice(&timeval(g.child_cpu.sys_ns));
    let r = &t.regs;
    // user_pt_regs: x0–x30, sp, pc, pstate
    for (i, v) in r.x.iter().chain(&[r.sp, r.pc, r.pstate]).enumerate() {
        b[112 + i * 8..120 + i * 8].copy_from_slice(&v.to_le_bytes());
    }
    b[384..388].copy_from_slice(&1i32.to_le_bytes()); // pr_fpvalid
    b
}

fn timeval(ns: u64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
    b[8..].copy_from_slice(&(ns % 1_000_000_000 / 1000).to_le_bytes());
    b
}

/// `struct elf_prpsinfo` (arm64, 136 bytes): state, credentials, pids, comm and the first 79
/// bytes of the command line (NULs between arguments replaced by spaces).
fn prpsinfo(cx: &Cx<'_>, g: &ThreadGroup, mm: &Mm) -> Vec<u8> {
    const PF_FORKNOEXEC: u64 = 0x40;
    const PF_DUMPCORE: u64 = 0x200;
    const PF_SIGNALED: u64 = 0x400;
    let mut b = vec![0u8; 136];
    b[1] = b'R';
    b[3] = cx.task.nice.get() as i8 as u8;
    let flags = PF_DUMPCORE | PF_SIGNALED | if g.execed { 0 } else { PF_FORKNOEXEC };
    b[8..16].copy_from_slice(&flags.to_le_bytes());
    // The real user and group IDs as seen in the process's own user namespace (Linux's
    // fill_psinfo)
    b[16..20].copy_from_slice(&g.cred.uid_out(g.cred.ruid).to_le_bytes());
    b[20..24].copy_from_slice(&g.cred.gid_out(g.cred.rgid).to_le_bytes());
    for (i, &v) in [g.tgid, g.ppid, g.pgid, g.sid].iter().enumerate() {
        let v = g.pid_ns.vnr(v).unwrap_or(0);
        b[24 + i * 4..28 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    b[40..56].copy_from_slice(&cx.task.comm.get());
    let (start, end) = mm.args;
    let len = end.saturating_sub(start).min(79) as usize;
    let mut args = vec![0u8; len];
    if mm
        .read(crate::uaccess::UserSlice::new(start, len), &mut args)
        .is_ok()
    {
        let used = args.iter().rposition(|&c| c != 0).map_or(0, |i| i + 1);
        for (i, &c) in args[..used].iter().enumerate() {
            b[56 + i] = if c == 0 { b' ' } else { c };
        }
    }
    b
}

/// `NT_FILE`: the number of file mappings, the page size, (start, end, file offset in pages) for
/// each, then their paths.
fn files(regions: &[DumpRegion]) -> Vec<u8> {
    let mapped: Vec<_> = regions
        .iter()
        .filter_map(|r| r.file.as_ref().map(|f| (r, f)))
        .collect();
    let mut b = Vec::new();
    b.extend_from_slice(&(mapped.len() as u64).to_le_bytes());
    b.extend_from_slice(&PAGE_SIZE.to_le_bytes());
    for (r, (offset, _)) in &mapped {
        for v in [r.start, r.end, offset / PAGE_SIZE] {
            b.extend_from_slice(&v.to_le_bytes());
        }
    }
    for (_, (_, path)) in &mapped {
        b.extend_from_slice(path);
        b.push(0);
    }
    b
}

/// The ELF header and program headers, and the file position of the first mapping's contents
/// (page-aligned, Linux's `dataoff`).
fn elf_header(regions: &[DumpRegion], notes: &[u8]) -> (Vec<u8>, u64) {
    let segs = regions.len() + 1;
    let phnum = if segs >= PN_XNUM as usize {
        PN_XNUM
    } else {
        segs as u16
    };
    // With more than 65535 program headers, the real count goes in sh_info of section header 0
    // (Linux's PN_XNUM)
    let extnum = phnum == PN_XNUM;
    let notes_at = EHDR_SIZE + PHDR_SIZE * segs as u64;
    let shdr_at = notes_at + notes.len() as u64;
    let end = shdr_at + if extnum { 64 } else { 0 };
    let data_offset = end.next_multiple_of(PAGE_SIZE);

    let mut b = Vec::with_capacity(end as usize);
    b.extend_from_slice(b"\x7fELF\x02\x01\x01");
    b.resize(16, 0);
    b.extend_from_slice(&ET_CORE.to_le_bytes());
    b.extend_from_slice(&EM_AARCH64.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes()); // e_version
    b.extend_from_slice(&0u64.to_le_bytes()); // e_entry
    b.extend_from_slice(&EHDR_SIZE.to_le_bytes()); // e_phoff
    b.extend_from_slice(&(if extnum { shdr_at } else { 0 }).to_le_bytes()); // e_shoff
    b.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    b.extend_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    b.extend_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    b.extend_from_slice(&phnum.to_le_bytes());
    b.extend_from_slice(&(if extnum { 64u16 } else { 0 }).to_le_bytes()); // e_shentsize
    b.extend_from_slice(&(extnum as u16).to_le_bytes()); // e_shnum
    b.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx

    let phdr = |b: &mut Vec<u8>,
                ty: u32,
                flags: u32,
                off: u64,
                vaddr: u64,
                filesz: u64,
                memsz: u64,
                align: u64| {
        b.extend_from_slice(&ty.to_le_bytes());
        b.extend_from_slice(&flags.to_le_bytes());
        for v in [off, vaddr, 0, filesz, memsz, align] {
            b.extend_from_slice(&v.to_le_bytes());
        }
    };
    phdr(&mut b, PT_NOTE, 0, notes_at, 0, notes.len() as u64, 0, 4);
    let mut off = data_offset;
    for r in regions {
        let mut flags = 0;
        if r.prot & prot::READ != 0 {
            flags |= 4;
        }
        if r.prot & prot::WRITE != 0 {
            flags |= 2;
        }
        if r.prot & prot::EXEC != 0 {
            flags |= 1;
        }
        phdr(
            &mut b,
            PT_LOAD,
            flags,
            off,
            r.start,
            r.size,
            r.end - r.start,
            PAGE_SIZE,
        );
        off += r.size;
    }
    b.extend_from_slice(notes);
    if extnum {
        // Section header: only sh_info matters (the real program header count)
        let mut sh = [0u8; 64];
        sh[44..48].copy_from_slice(&(segs as u32).to_le_bytes());
        b.extend_from_slice(&sh);
    }
    (b, data_offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_are_padded_to_four_bytes() {
        let mut out = Vec::new();
        note(&mut out, "CORE", NT_ARM_SYSTEM_CALL, &[1, 2, 3]);
        // namesz 5, descsz 3, type, "CORE\0" padded to 8, contents padded to 4
        assert_eq!(out.len(), 12 + 8 + 4);
        assert_eq!(&out[..4], &5u32.to_le_bytes());
        assert_eq!(&out[4..8], &3u32.to_le_bytes());
        assert_eq!(&out[12..17], b"CORE\0");
        assert_eq!(&out[20..24], &[1, 2, 3, 0]);
    }

    #[test]
    fn elf_header_places_segments_after_the_notes() {
        let regions = vec![
            DumpRegion {
                start: 0x10000,
                end: 0x12000,
                prot: prot::READ | prot::EXEC,
                size: 0x1000,
                sparse: false,
                file: Some((0, std::rc::Rc::from(&b"/bin/x"[..]))),
            },
            DumpRegion {
                start: 0x20000,
                end: 0x23000,
                prot: prot::READ | prot::WRITE,
                size: 0x3000,
                sparse: true,
                file: None,
            },
        ];
        let notes = vec![0u8; 100];
        let (h, data) = elf_header(&regions, &notes);
        assert_eq!(&h[..4], b"\x7fELF");
        assert_eq!(u16::from_le_bytes([h[16], h[17]]), ET_CORE);
        assert_eq!(u16::from_le_bytes([h[18], h[19]]), EM_AARCH64);
        assert_eq!(u16::from_le_bytes([h[56], h[57]]), 3);
        assert_eq!(data, PAGE_SIZE);
        assert_eq!(h.len() as u64, 64 + 3 * 56 + 100);
        // The second PT_LOAD follows the first one's contents directly, with flags RW
        let ph = |i: usize, off: usize| {
            let at = 64 + i * 56 + off;
            u64::from_le_bytes(h[at..at + 8].try_into().unwrap())
        };
        assert_eq!(ph(1, 8), PAGE_SIZE);
        assert_eq!(ph(2, 8), PAGE_SIZE + 0x1000);
        assert_eq!(ph(2, 16), 0x20000);
        assert_eq!(ph(2, 40), 0x3000);
        assert_eq!(h[64 + 2 * 56 + 4], 6);
        let file = files(&regions);
        assert_eq!(&file[..8], &1u64.to_le_bytes());
        assert!(file.ends_with(b"/bin/x\0"));
    }
}
