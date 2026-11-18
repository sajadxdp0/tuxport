//! A subset of eBPF (the kernel/cgroup map): cgroup device filter programs
//! (`BPF_PROG_TYPE_CGROUP_DEVICE`).
//!
//! cgroup v2 has no v1 `devices.allow`; runc and crun attach an eBPF program to each container's
//! cgroup, and the kernel uses it to decide whether to allow a process to open a device node or
//! mknod. This is the only program type here: the verifier ([`verify`]) checks programs as Linux
//! 6.1's `do_check` does, with the same error codes, and the interpreter ([`run`]) executes them;
//! they are attached to cgroups ([`crate::cgroup`]), and opening a device or mknod runs the
//! effective programs of that cgroup ([`check_device`]).
//!
//! The verifier is simpler than Linux's: only forward jumps are allowed (no loops, so programs
//! always terminate), paths are not told apart, and a register whose types differ where paths
//! join counts as uninitialized; scalars track only an unsigned range, enough to decide whether
//! the return value is 0 or 1. No maps, BPF-to-BPF calls or atomics, and the stack holds only
//! scalars. The programs runc and crun generate use only ALU, conditional jumps and context reads.

use std::collections::BTreeMap;
use std::rc::{Rc, Weak};

use crate::errno::Errno;
use crate::kernel::Kernel;

/// `BPF_PROG_TYPE_CGROUP_DEVICE`.
pub(crate) const PROG_TYPE_CGROUP_DEVICE: u32 = 15;
/// `BPF_CGROUP_DEVICE` (the attach type).
pub(crate) const ATTACH_CGROUP_DEVICE: u32 = 6;
/// Attach flags: child cgroups may attach their own (overriding it), several may be attached, one
/// of them is replaced.
pub(crate) const ALLOW_OVERRIDE: u32 = 1;
pub(crate) const ALLOW_MULTI: u32 = 2;
pub(crate) const REPLACE: u32 = 4;
/// The most programs one cgroup can have attached (`BPF_CGROUP_MAX_PROGS`).
pub(crate) const MAX_PROGS: usize = 64;

/// Device types and accesses (`BPF_DEVCG_DEV_*`, `BPF_DEVCG_ACC_*`).
pub(crate) const DEV_BLOCK: u32 = 1;
pub(crate) const DEV_CHAR: u32 = 2;
pub(crate) const ACC_MKNOD: u32 = 1;
pub(crate) const ACC_READ: u32 = 2;
pub(crate) const ACC_WRITE: u32 = 4;

/// The most instructions a program may have (privileged, `BPF_COMPLEXITY_LIMIT_INSNS`).
pub(crate) const MAX_INSNS: usize = 1_000_000;
/// Stack size.
const STACK: i64 = 512;
/// The size of `struct bpf_cgroup_dev_ctx`: access_type, major, minor.
const CTX_SIZE: i64 = 12;

// ---- Programs ----

/// One instruction (`struct bpf_insn`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Insn {
    pub code: u8,
    pub dst: u8,
    pub src: u8,
    pub off: i16,
    pub imm: i32,
}

impl Insn {
    pub fn decode(b: &[u8]) -> Insn {
        Insn {
            code: b[0],
            dst: b[1] & 0xf,
            src: b[1] >> 4,
            off: i16::from_le_bytes([b[2], b[3]]),
            imm: i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        }
    }

    pub fn encode(&self) -> [u8; 8] {
        let mut b = [0; 8];
        b[0] = self.code;
        b[1] = self.dst | self.src << 4;
        b[2..4].copy_from_slice(&self.off.to_le_bytes());
        b[4..].copy_from_slice(&self.imm.to_le_bytes());
        b
    }
}

/// A loaded program. Open fds and the cgroups it is attached to each hold a reference; the
/// [`Registry`] records it by id (`BPF_PROG_GET_FD_BY_ID`).
pub(crate) struct Prog {
    pub id: u32,
    pub insns: Vec<Insn>,
    pub name: [u8; 16],
    /// The first 8 bytes of the SHA-1 of the instructions (Linux's `bpf_prog_calc_tag`).
    pub tag: [u8; 8],
    /// The license is GPL-compatible.
    pub gpl: bool,
    /// Load time (nanoseconds since boot).
    pub load_time: u64,
    /// The loader's real uid (a kernel ID).
    pub uid: u32,
    /// Number of instructions the verifier processed.
    pub verified: u32,
}

impl Prog {
    /// The bytes as given (`xlated_prog_insns`: instructions are not rewritten here).
    pub fn bytes(&self) -> Vec<u8> {
        self.insns.iter().flat_map(|i| i.encode()).collect()
    }
}

/// The program id table. Ids count up from 1 and are never reused.
#[derive(Default)]
pub(crate) struct Registry {
    progs: BTreeMap<u32, Weak<Prog>>,
    next: u32,
}

impl Registry {
    /// The next id.
    pub fn next_id(&mut self) -> u32 {
        self.progs.retain(|_, p| p.strong_count() > 0);
        self.next += 1;
        self.next
    }

    pub fn insert(&mut self, prog: &Rc<Prog>) {
        self.progs.insert(prog.id, Rc::downgrade(prog));
    }

    pub fn get(&self, id: u32) -> Option<Rc<Prog>> {
        self.progs.get(&id)?.upgrade()
    }
}

/// The license is GPL-compatible (Linux's `license_is_gpl_compatible`).
pub(crate) fn gpl_compatible(license: &[u8]) -> bool {
    matches!(
        license,
        b"GPL"
            | b"GPL v2"
            | b"GPL and additional rights"
            | b"Dual BSD/GPL"
            | b"Dual MIT/GPL"
            | b"Dual MPL/GPL"
    )
}

/// SHA-1 (FIPS 180-1), for program tags.
pub(crate) fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (t, word) in block.chunks_exact(4).enumerate() {
            w[t] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for t in 16..80 {
            w[t] = (w[t - 3] ^ w[t - 8] ^ w[t - 14] ^ w[t - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (t, &wt) in w.iter().enumerate() {
            let (f, k) = match t {
                0..20 => ((b & c) | (!b & d), 0x5a827999),
                20..40 => (b ^ c ^ d, 0x6ed9eba1),
                40..60 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wt);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (k, v) in h.iter().enumerate() {
        out[k * 4..k * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

// ---- Verifier ----

/// The verifier rejects a program: the error code and the explanation written to the log.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Reject {
    pub errno: Errno,
    pub msg: String,
}

fn reject<T>(errno: Errno, msg: String) -> Result<T, Reject> {
    Err(Reject { errno, msg })
}

/// Register types (for the verifier).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
    /// Not initialized, or different on the paths that join.
    Uninit,
    /// A scalar with a value in [min, max] (unsigned).
    Scalar { min: u64, max: u64 },
    /// A context pointer plus `off`.
    Ctx { off: i64 },
    /// A stack pointer: an offset from the stack top (r10).
    Stack { off: i64 },
}

const UNKNOWN: Ty = Ty::Scalar {
    min: 0,
    max: u64::MAX,
};
const UNKNOWN32: Ty = Ty::Scalar {
    min: 0,
    max: u32::MAX as u64,
};

fn constant(v: u64) -> Ty {
    Ty::Scalar { min: v, max: v }
}

impl Ty {
    fn is_pointer(self) -> bool {
        matches!(self, Ty::Ctx { .. } | Ty::Stack { .. })
    }

    fn known(self) -> Option<u64> {
        match self {
            Ty::Scalar { min, max } if min == max => Some(min),
            _ => None,
        }
    }

    fn merge(self, other: Ty) -> Ty {
        match (self, other) {
            (a, b) if a == b => a,
            (Ty::Scalar { min: a, max: b }, Ty::Scalar { min: c, max: d }) => Ty::Scalar {
                min: a.min(c),
                max: b.max(d),
            },
            _ => Ty::Uninit,
        }
    }
}

type Regs = [Ty; 11];

/// Instruction classes, operations and sources (`BPF_CLASS` and so on).
mod op {
    pub const LD: u8 = 0x00;
    pub const LDX: u8 = 0x01;
    pub const ST: u8 = 0x02;
    pub const STX: u8 = 0x03;
    pub const ALU: u8 = 0x04;
    pub const JMP: u8 = 0x05;
    pub const JMP32: u8 = 0x06;
    pub const ALU64: u8 = 0x07;
    pub const X: u8 = 0x08;
    pub const MEM: u8 = 0x60;
    pub const LD_IMM64: u8 = 0x18;
    pub const JA: u8 = 0x00;
    pub const CALL: u8 = 0x80;
    pub const EXIT: u8 = 0x90;
    pub const NEG: u8 = 0x80;
    pub const MOV: u8 = 0xb0;
    pub const END: u8 = 0xd0;
}

/// Helpers device programs may use (those in Linux 6.1's `cgroup_dev_func_proto` that need no
/// map): ktime_get_ns, get_prandom_u32, get_smp_processor_id, get_current_uid_gid,
/// get_numa_node_id, get_current_cgroup_id, ktime_get_boot_ns.
const HELPERS: [i32; 7] = [5, 7, 8, 15, 42, 80, 125];

/// Verifies a program (Linux's `bpf_check`: first the control flow, then each instruction in
/// program order). Returns the number of instructions processed.
pub(crate) fn verify(insns: &[Insn]) -> Result<u32, Reject> {
    let n = insns.len();
    // LD_IMM64 takes two slots, and the second one is not an instruction
    let mut second = vec![false; n];
    let mut pc = 0;
    while pc < n {
        let i = &insns[pc];
        if i.dst > 10 || i.src > 10 {
            return reject(Errno::EINVAL, format!("R{} is invalid", i.dst.max(i.src)));
        }
        if i.code == op::LD_IMM64 {
            let next = insns.get(pc + 1);
            if next.is_none_or(|j| j.code != 0 || j.dst != 0 || j.src != 0 || j.off != 0) {
                return reject(Errno::EINVAL, "invalid bpf_ld_imm64 insn".into());
            }
            second[pc + 1] = true;
            pc += 2;
        } else {
            pc += 1;
        }
    }
    // Control flow (check_subprogs, check_cfg): jumps stay in bounds, never land in the second
    // half of an LD_IMM64 and never go backwards; execution must not run past the last
    // instruction, and every instruction must be reachable
    let mut reachable = vec![false; n];
    reachable[0] = true;
    for pc in 0..n {
        if second[pc] {
            continue;
        }
        let i = &insns[pc];
        let class = i.code & 7;
        let o = i.code & 0xf0;
        if let Some(t) = jump_target(pc, i) {
            if t < 0 || t >= n as i64 || second[t as usize] {
                return reject(
                    Errno::EINVAL,
                    format!("jump out of range from insn {pc} to {t}"),
                );
            }
            if t <= pc as i64 {
                return reject(Errno::EINVAL, format!("back-edge from insn {pc} to {t}"));
            }
            if reachable[pc] {
                reachable[t as usize] = true;
            }
        }
        let ends = class == op::JMP && (o == op::EXIT || o == op::JA) && i.code & op::X == 0;
        let len = if i.code == op::LD_IMM64 { 2 } else { 1 };
        if !ends {
            if pc + len >= n {
                return reject(Errno::EINVAL, "last insn is not an exit or jmp".into());
            }
            if reachable[pc] {
                reachable[pc + len] = true;
            }
        }
    }
    if let Some(pc) = (0..n).find(|&pc| !second[pc] && !reachable[pc]) {
        return reject(Errno::EINVAL, format!("unreachable insn {pc}"));
    }
    // Check in order (do_check): each instruction's state is the join of the paths reaching it
    let mut states: Vec<Option<Regs>> = vec![None; n];
    let mut init = [Ty::Uninit; 11];
    init[1] = Ty::Ctx { off: 0 };
    init[10] = Ty::Stack { off: 0 };
    states[0] = Some(init);
    let mut processed = 0u32;
    for pc in 0..n {
        if second[pc] {
            continue;
        }
        // Instructions skipped by known conditions (dead code) are not checked
        let Some(mut r) = states[pc] else {
            continue;
        };
        processed += 1;
        let i = insns[pc];
        let mut flow = |to: usize, regs: Regs| {
            states[to] = Some(match states[to] {
                Some(s) => std::array::from_fn(|k| s[k].merge(regs[k])),
                None => regs,
            });
        };
        match i.code & 7 {
            op::ALU | op::ALU64 => {
                alu(&mut r, &i)?;
                flow(pc + 1, r);
            }
            op::LDX => {
                ldx(&mut r, &i)?;
                flow(pc + 1, r);
            }
            op::ST | op::STX => {
                store(&r, &i)?;
                flow(pc + 1, r);
            }
            op::LD if i.code == op::LD_IMM64 => {
                if i.src != 0 || i.off != 0 {
                    return reject(Errno::EINVAL, "unsupported ld_imm64 (maps)".into());
                }
                write_reg(
                    &mut r,
                    i.dst,
                    constant(i.imm as u32 as u64 | (insns[pc + 1].imm as u32 as u64) << 32),
                )?;
                flow(pc + 2, r);
            }
            op::LD => {
                return reject(
                    Errno::EINVAL,
                    "BPF_LD_[ABS|IND] instructions not allowed for this program type".into(),
                );
            }
            _ => {
                for (to, regs) in jmp(r, pc, &i)? {
                    flow(to, regs);
                }
            }
        }
    }
    Ok(processed)
}

/// Whether this instruction is a jump (JA and conditional jumps), and if so, its target.
fn jump_target(pc: usize, i: &Insn) -> Option<i64> {
    let class = i.code & 7;
    let o = i.code & 0xf0;
    let is_jump = (class == op::JMP && o != op::CALL && o != op::EXIT) || class == op::JMP32;
    is_jump.then(|| pc as i64 + i.off as i64 + 1)
}

fn alu(r: &mut Regs, i: &Insn) -> Result<(), Reject> {
    let is64 = i.code & 7 == op::ALU64;
    let x = i.code & op::X != 0;
    let o = i.code & 0xf0;
    let reserved = || reject(Errno::EINVAL, "BPF_ALU uses reserved fields".into());
    match o {
        op::END => {
            if is64 || i.src != 0 || i.off != 0 || !matches!(i.imm, 16 | 32 | 64) {
                return reject(Errno::EINVAL, "BPF_END uses reserved fields".into());
            }
            let t = read_reg(r, i.dst)?;
            if t.is_pointer() {
                return reject(
                    Errno::EACCES,
                    format!("R{} pointer arithmetic prohibited", i.dst),
                );
            }
            let max = if i.imm == 64 {
                u64::MAX
            } else {
                (1 << i.imm) - 1
            };
            return write_reg(r, i.dst, Ty::Scalar { min: 0, max });
        }
        op::NEG => {
            if x || i.src != 0 || i.off != 0 || i.imm != 0 {
                return reject(Errno::EINVAL, "BPF_NEG uses reserved fields".into());
            }
            let t = read_reg(r, i.dst)?;
            if t.is_pointer() {
                return reject(
                    Errno::EACCES,
                    format!("R{} pointer arithmetic prohibited", i.dst),
                );
            }
            return write_reg(r, i.dst, if is64 { UNKNOWN } else { UNKNOWN32 });
        }
        op::MOV => {
            if i.off != 0 || (x && i.imm != 0) || (!x && i.src != 0) {
                return reject(Errno::EINVAL, "BPF_MOV uses reserved fields".into());
            }
            let t = if x {
                read_reg(r, i.src)?
            } else if is64 {
                constant(i.imm as i64 as u64)
            } else {
                constant(i.imm as u32 as u64)
            };
            let t = match t {
                t if is64 => t,
                t if t.is_pointer() => {
                    return reject(Errno::EACCES, format!("R{} partial copy of pointer", i.src));
                }
                Ty::Scalar { min, max } if max <= u32::MAX as u64 => Ty::Scalar { min, max },
                _ => UNKNOWN32,
            };
            return write_reg(r, i.dst, t);
        }
        0x00 | 0x10 | 0x20 | 0x30 | 0x40 | 0x50 | 0x60 | 0x70 | 0x90 | 0xa0 | 0xc0 => {}
        _ => return reject(Errno::EINVAL, format!("unknown opcode {:02x}", i.code)),
    }
    if i.off != 0 || (x && i.imm != 0) || (!x && i.src != 0) {
        return reserved();
    }
    let src = if x {
        read_reg(r, i.src)?
    } else if is64 {
        constant(i.imm as i64 as u64)
    } else {
        constant(i.imm as u32 as u64)
    };
    let dst = read_reg(r, i.dst)?;
    if !x && matches!(o, 0x30 | 0x90) && i.imm == 0 {
        return reject(Errno::EINVAL, "div by zero".into());
    }
    let bits = if is64 { 64 } else { 32 };
    if !x && matches!(o, 0x60 | 0x70 | 0xc0) && !(0..bits).contains(&i.imm) {
        return reject(Errno::EINVAL, format!("invalid shift {}", i.imm));
    }
    // Pointers may only add or subtract constants (64-bit)
    if dst.is_pointer() || src.is_pointer() {
        if !is64 {
            return reject(
                Errno::EACCES,
                format!("R{} 32-bit pointer arithmetic prohibited", i.dst),
            );
        }
        let moved = |t: Ty, by: i64| match t {
            Ty::Ctx { off } => Ty::Ctx { off: off + by },
            Ty::Stack { off } => Ty::Stack { off: off + by },
            t => t,
        };
        let t = match (dst, src.known(), o) {
            (d, Some(v), 0x00) if d.is_pointer() => moved(d, v as i64),
            (d, Some(v), 0x10) if d.is_pointer() => moved(d, (v as i64).wrapping_neg()),
            (d, _, 0x00) if src.is_pointer() && d.known().is_some() => {
                moved(src, d.known().unwrap() as i64)
            }
            _ => {
                return reject(
                    Errno::EACCES,
                    format!("R{} pointer arithmetic prohibited", i.dst),
                );
            }
        };
        return write_reg(r, i.dst, t);
    }
    let mask = if is64 { u64::MAX } else { u32::MAX as u64 };
    let t = match (dst, src) {
        (Ty::Scalar { min: a, max: b }, Ty::Scalar { min: c, max: d }) => {
            if a == b && c == d {
                constant(alu_value(o, is64, a, c))
            } else if o == 0x50 {
                // and: no more than either side's upper bound
                Ty::Scalar {
                    min: 0,
                    max: b.min(d).min(mask),
                }
            } else if o == 0x70 && c == d && b <= mask {
                // Right shift by a constant
                Ty::Scalar {
                    min: a >> c,
                    max: b >> c,
                }
            } else {
                Ty::Scalar { min: 0, max: mask }
            }
        }
        _ => Ty::Scalar { min: 0, max: mask },
    };
    write_reg(r, i.dst, t)
}

fn ldx(r: &mut Regs, i: &Insn) -> Result<(), Reject> {
    if i.code & 0xe0 != op::MEM {
        return reject(Errno::EINVAL, format!("unknown opcode {:02x}", i.code));
    }
    if i.imm != 0 {
        return reject(Errno::EINVAL, "BPF_LDX uses reserved fields".into());
    }
    let size = size_of(i.code);
    let base = read_reg(r, i.src)?;
    let max = if size == 8 {
        u64::MAX
    } else {
        (1 << (size * 8)) - 1
    };
    match base {
        Ty::Ctx { off: 0 } if ctx_access(i.off as i64, size) => {}
        Ty::Ctx { off: 0 } => {
            return reject(
                Errno::EACCES,
                format!("invalid bpf_context access off={} size={size}", i.off),
            );
        }
        Ty::Ctx { off } => {
            return reject(
                Errno::EACCES,
                format!(
                    "dereference of modified ctx ptr R{} off={off} disallowed",
                    i.src
                ),
            );
        }
        // Privileged programs may read stack that was never written (allow_uninit_stack), and get
        // an unknown value
        Ty::Stack { off } => stack_access(off + i.off as i64, size, false)?,
        _ => {
            return reject(
                Errno::EACCES,
                format!("R{} invalid mem access 'scalar'", i.src),
            );
        }
    }
    write_reg(r, i.dst, Ty::Scalar { min: 0, max })
}

/// Context access (Linux's `cgroup_dev_is_valid_access`): read-only; in bounds and aligned to the
/// size; access_type may be read in part, major and minor only whole.
fn ctx_access(off: i64, size: i64) -> bool {
    off >= 0 && off + size <= CTX_SIZE && off % size == 0 && (size == 4 || off < 4)
}

fn store(r: &Regs, i: &Insn) -> Result<(), Reject> {
    let stx = i.code & 7 == op::STX;
    if i.code & 0xe0 != op::MEM {
        return reject(Errno::EINVAL, format!("unknown opcode {:02x}", i.code));
    }
    if (stx && i.imm != 0) || (!stx && i.src != 0) {
        return reject(Errno::EINVAL, "BPF_ST uses reserved fields".into());
    }
    let size = size_of(i.code);
    let base = read_reg(r, i.dst)?;
    if stx && read_reg(r, i.src)?.is_pointer() {
        return reject(
            Errno::EACCES,
            format!("R{} spilling a pointer is not supported", i.src),
        );
    }
    match base {
        Ty::Stack { off } => stack_access(off + i.off as i64, size, true),
        Ty::Ctx { .. } => reject(
            Errno::EACCES,
            format!("invalid bpf_context access off={} size={size}", i.off),
        ),
        _ => reject(
            Errno::EACCES,
            format!("R{} invalid mem access 'scalar'", i.dst),
        ),
    }
}

/// Stack access: within [-512, 0) and aligned to the size.
fn stack_access(off: i64, size: i64, write: bool) -> Result<(), Reject> {
    if off < -STACK || off + size > 0 {
        let what = if write { "write to" } else { "read from" };
        return reject(
            Errno::EACCES,
            format!("invalid {what} stack off={off} size={size}"),
        );
    }
    if off % size != 0 {
        return reject(
            Errno::EACCES,
            format!("misaligned stack access off {off} size {size}"),
        );
    }
    Ok(())
}

/// Jump instructions: returns the successors with their registers (conditional jumps narrow
/// scalar ranges by the condition).
fn jmp(mut r: Regs, pc: usize, i: &Insn) -> Result<Vec<(usize, Regs)>, Reject> {
    let jmp32 = i.code & 7 == op::JMP32;
    let x = i.code & op::X != 0;
    let o = i.code & 0xf0;
    match o {
        op::JA if !jmp32 => {
            if x || i.src != 0 || i.imm != 0 || i.dst != 0 {
                return reject(Errno::EINVAL, "BPF_JA uses reserved fields".into());
            }
            return Ok(vec![((pc as i64 + i.off as i64 + 1) as usize, r)]);
        }
        op::CALL if !jmp32 => {
            if x || i.off != 0 || i.dst != 0 {
                return reject(Errno::EINVAL, "BPF_CALL uses reserved fields".into());
            }
            if i.src != 0 {
                return reject(Errno::EINVAL, "function calls are not supported".into());
            }
            if !HELPERS.contains(&i.imm) {
                return reject(Errno::EINVAL, format!("invalid func unknown#{}", i.imm));
            }
            // After a call r1–r5 can no longer be read, and r0 is the return value
            r[1..=5].fill(Ty::Uninit);
            r[0] = UNKNOWN;
            return Ok(vec![(pc + 1, r)]);
        }
        op::EXIT if !jmp32 => {
            if x || i.src != 0 || i.imm != 0 || i.off != 0 || i.dst != 0 {
                return reject(Errno::EINVAL, "BPF_EXIT uses reserved fields".into());
            }
            // The return value can only be 0 or 1 (check_return_code)
            return match read_reg(&r, 0)? {
                Ty::Scalar { max, .. } if max <= 1 => Ok(Vec::new()),
                Ty::Scalar { min, max } => reject(
                    Errno::EINVAL,
                    format!(
                        "At program exit the register R0 has value ({min:#x}; {max:#x}) should have been in (0x0; 0x1)"
                    ),
                ),
                _ => reject(
                    Errno::EINVAL,
                    "At program exit the register R0 is not a known value".into(),
                ),
            };
        }
        0x10 | 0x20 | 0x30 | 0x40 | 0x50 | 0x60 | 0x70 | 0xa0 | 0xb0 | 0xc0 | 0xd0 => {}
        _ => return reject(Errno::EINVAL, format!("unknown opcode {:02x}", i.code)),
    }
    if (x && i.imm != 0) || (!x && i.src != 0) {
        return reject(Errno::EINVAL, "BPF_JMP/JMP32 uses reserved fields".into());
    }
    let dst = read_reg(&r, i.dst)?;
    if x {
        read_reg(&r, i.src)?;
    }
    let target = (pc as i64 + i.off as i64 + 1) as usize;
    // Comparisons with a constant (the unsigned kinds) narrow the scalar's range; a side narrowed
    // to empty is unreachable
    let mut taken = r;
    let mut fall = r;
    let mut take = true;
    let mut through = true;
    if let (false, Ty::Scalar { min, max }) = (x, dst)
        && (!jmp32 || max <= u32::MAX as u64)
    {
        let v = if jmp32 {
            i.imm as u32 as u64
        } else {
            i.imm as i64 as u64
        };
        let range = |lo: u64, hi: u64| (lo <= hi).then_some(Ty::Scalar { min: lo, max: hi });
        let (t, f) = match o {
            0x10 => (range(min.max(v), max.min(v)), Some(dst)),
            0x50 => (Some(dst), range(min.max(v), max.min(v))),
            0x20 => (
                v.checked_add(1).and_then(|w| range(min.max(w), max)),
                range(min, max.min(v)),
            ),
            0x30 => (
                range(min.max(v), max),
                v.checked_sub(1).and_then(|w| range(min, max.min(w))),
            ),
            0xa0 => (
                v.checked_sub(1).and_then(|w| range(min, max.min(w))),
                range(min.max(v), max),
            ),
            0xb0 => (
                range(min, max.min(v)),
                v.checked_add(1).and_then(|w| range(min.max(w), max)),
            ),
            _ => (Some(dst), Some(dst)),
        };
        match t {
            Some(t) => taken[i.dst as usize] = t,
            None => take = false,
        }
        match f {
            Some(f) => fall[i.dst as usize] = f,
            None => through = false,
        }
    }
    let mut out = Vec::new();
    if through {
        out.push((pc + 1, fall));
    }
    if take {
        out.push((target, taken));
    }
    Ok(out)
}

fn read_reg(r: &Regs, reg: u8) -> Result<Ty, Reject> {
    match r[reg as usize] {
        Ty::Uninit => reject(Errno::EACCES, format!("R{reg} !read_ok")),
        t => Ok(t),
    }
}

fn write_reg(r: &mut Regs, reg: u8, t: Ty) -> Result<(), Reject> {
    if reg == 10 {
        return reject(Errno::EACCES, "frame pointer is read only".into());
    }
    r[reg as usize] = t;
    Ok(())
}

// ---- Interpreter ----

/// The caller's information while a program runs (for helpers).
pub(crate) struct Env {
    pub uid: u32,
    pub gid: u32,
    pub cgroup: u64,
}

/// The "addresses" of the context and the stack in the interpreter: pointers can only be derived
/// from them by adding or subtracting constants (the verifier guarantees this), and a memory
/// access finds the context or the stack by which range it falls in.
const CTX_BASE: u64 = 0x1000_0000_0000_0000;
const STACK_TOP: u64 = 0x2000_0000_0000_0000;

/// Before a process opens a device node or calls mknod (Linux's `devcgroup_check_permission`):
/// runs the effective programs of its cgroup; if any one denies (the lowest bit of its return
/// value is 0), the result is EPERM. `dev_type` is [`DEV_CHAR`] or [`DEV_BLOCK`], and `access` is
/// a combination of [`ACC_MKNOD`] and the others.
pub(crate) fn check_device(
    k: &Kernel,
    env: &Env,
    (dev_type, major, minor): (u32, u32, u32),
    access: u32,
) -> Result<(), Errno> {
    let progs = crate::cgroup::effective_device_progs(k, env.cgroup);
    let ctx = [access << 16 | dev_type, major, minor];
    if progs.iter().all(|p| run(p, ctx, env) & 1 == 1) {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}

/// Interprets a verified program (Linux's `___bpf_prog_run`); `ctx` is access_type, major, minor.
/// Returns r0.
pub(crate) fn run(prog: &Prog, ctx: [u32; 3], env: &Env) -> u64 {
    let mut reg = [0u64; 11];
    reg[1] = CTX_BASE;
    reg[10] = STACK_TOP;
    let mut stack = [0u8; STACK as usize];
    let mut ctx_bytes = [0u8; CTX_SIZE as usize];
    for (k, v) in ctx.iter().enumerate() {
        ctx_bytes[k * 4..k * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    let insns = &prog.insns;
    let mut pc = 0;
    // Jumps only go forward, so the instruction count bounds the number of steps
    for _ in 0..insns.len() {
        let Some(&i) = insns.get(pc) else {
            return 0;
        };
        let (d, s) = (i.dst as usize, i.src as usize);
        let class = i.code & 7;
        let o = i.code & 0xf0;
        let x = i.code & op::X != 0;
        pc += 1;
        match class {
            op::ALU | op::ALU64 => {
                let is64 = class == op::ALU64;
                let src = if x {
                    reg[s]
                } else if is64 {
                    i.imm as i64 as u64
                } else {
                    i.imm as u32 as u64
                };
                reg[d] = match o {
                    op::END => {
                        let v = reg[d];
                        // to_le only truncates on little endian; to_be swaps bytes
                        match (x, i.imm) {
                            (false, 16) => v & 0xffff,
                            (false, 32) => v & 0xffff_ffff,
                            (false, _) => v,
                            (true, 16) => (v as u16).swap_bytes() as u64,
                            (true, 32) => (v as u32).swap_bytes() as u64,
                            (true, _) => v.swap_bytes(),
                        }
                    }
                    _ => alu_value(o, is64, reg[d], src),
                };
            }
            op::JMP | op::JMP32 => {
                match o {
                    op::JA => {
                        pc = (pc as i64 + i.off as i64) as usize;
                        continue;
                    }
                    op::EXIT => return reg[0],
                    op::CALL => {
                        reg[0] = helper(i.imm, env);
                        continue;
                    }
                    _ => {}
                }
                let src = if x { reg[s] } else { i.imm as i64 as u64 };
                let (a, b) = if class == op::JMP32 {
                    (reg[d] as u32 as u64, src as u32 as u64)
                } else {
                    (reg[d], src)
                };
                let (sa, sb) = if class == op::JMP32 {
                    (a as u32 as i32 as i64, b as u32 as i32 as i64)
                } else {
                    (a as i64, b as i64)
                };
                let cond = match o {
                    0x10 => a == b,
                    0x20 => a > b,
                    0x30 => a >= b,
                    0x40 => a & b != 0,
                    0x50 => a != b,
                    0x60 => sa > sb,
                    0x70 => sa >= sb,
                    0xa0 => a < b,
                    0xb0 => a <= b,
                    0xc0 => sa < sb,
                    0xd0 => sa <= sb,
                    _ => false,
                };
                if cond {
                    pc = (pc as i64 + i.off as i64) as usize;
                }
            }
            op::LDX => {
                let size = size_of(i.code) as usize;
                let addr = reg[s].wrapping_add(i.off as i64 as u64);
                let Some(bytes) = memory(&ctx_bytes, &stack, addr, size) else {
                    return 0;
                };
                let mut b = [0u8; 8];
                b[..size].copy_from_slice(bytes);
                reg[d] = u64::from_le_bytes(b);
            }
            op::ST | op::STX => {
                let size = size_of(i.code) as usize;
                let v = if class == op::STX {
                    reg[s]
                } else {
                    i.imm as i64 as u64
                };
                let addr = reg[d].wrapping_add(i.off as i64 as u64);
                let start = addr.wrapping_sub(STACK_TOP - STACK as u64) as usize;
                let Some(slot) = stack.get_mut(start..start.wrapping_add(size)) else {
                    return 0;
                };
                slot.copy_from_slice(&v.to_le_bytes()[..size]);
            }
            _ if i.code == op::LD_IMM64 => {
                let hi = insns.get(pc).map_or(0, |n| n.imm as u32 as u64);
                reg[d] = i.imm as u32 as u64 | hi << 32;
                pc += 1;
            }
            _ => return 0,
        }
    }
    0
}

/// Returns those bytes when the access falls in the context or the stack.
fn memory<'a>(ctx: &'a [u8], stack: &'a [u8], addr: u64, size: usize) -> Option<&'a [u8]> {
    if let Some(off) = addr.checked_sub(CTX_BASE)
        && off < CTX_SIZE as u64
    {
        return ctx.get(off as usize..off as usize + size);
    }
    let start = addr.checked_sub(STACK_TOP - STACK as u64)? as usize;
    stack.get(start..start.checked_add(size)?)
}

/// Helpers ([`HELPERS`]).
fn helper(id: i32, env: &Env) -> u64 {
    match id {
        // CLOCK_MONOTONIC
        5 => tuxport_hal::clock_gettime(1).map_or(0, |(s, ns)| s as u64 * 1_000_000_000 + ns as u64),
        7 => {
            let mut b = [0u8; 4];
            let _ = tuxport_hal::fill_random(&mut b);
            u32::from_le_bytes(b) as u64
        }
        15 => (env.gid as u64) << 32 | env.uid as u64,
        80 => env.cgroup,
        125 => crate::pseudo::uptime_ns(),
        _ => 0,
    }
}

/// ALU operations (the ALU part of `___bpf_prog_run`): 32-bit ones work on the low 32 bits and
/// clear the high bits; shift amounts are taken modulo the width; division by 0 gives 0, and
/// modulo 0 leaves the value unchanged.
fn alu_value(o: u8, is64: bool, a: u64, b: u64) -> u64 {
    if is64 {
        match o {
            0x00 => a.wrapping_add(b),
            0x10 => a.wrapping_sub(b),
            0x20 => a.wrapping_mul(b),
            0x30 => a.checked_div(b).unwrap_or(0),
            0x40 => a | b,
            0x50 => a & b,
            0x60 => a << (b & 63),
            0x70 => a >> (b & 63),
            0x80 => a.wrapping_neg(),
            0x90 => a.checked_rem(b).unwrap_or(a),
            0xa0 => a ^ b,
            0xb0 => b,
            0xc0 => ((a as i64) >> (b & 63)) as u64,
            _ => 0,
        }
    } else {
        let (a, b) = (a as u32, b as u32);
        (match o {
            0x00 => a.wrapping_add(b),
            0x10 => a.wrapping_sub(b),
            0x20 => a.wrapping_mul(b),
            0x30 => a.checked_div(b).unwrap_or(0),
            0x40 => a | b,
            0x50 => a & b,
            0x60 => a << (b & 31),
            0x70 => a >> (b & 31),
            0x80 => a.wrapping_neg(),
            0x90 => a.checked_rem(b).unwrap_or(a),
            0xa0 => a ^ b,
            0xb0 => b,
            0xc0 => ((a as i32) >> (b & 31)) as u32,
            _ => 0,
        }) as u64
    }
}

/// Bytes accessed (`BPF_SIZE`).
fn size_of(code: u8) -> i64 {
    match code & 0x18 {
        0x00 => 4,
        0x08 => 2,
        0x10 => 1,
        _ => 8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insn(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> Insn {
        Insn {
            code,
            dst,
            src,
            off,
            imm,
        }
    }

    /// A program like runc's: character device 1:3 allows anything, 1:5 cannot be written.
    fn device_prog() -> Vec<Insn> {
        vec![
            insn(0x61, 2, 1, 0, 0),
            insn(0xbc, 3, 2, 0, 0),
            insn(0x54, 3, 0, 0, 0xffff),
            insn(0x74, 2, 0, 0, 16),
            insn(0x61, 4, 1, 4, 0),
            insn(0x61, 5, 1, 8, 0),
            insn(0x55, 3, 0, 6, 2),
            insn(0x55, 4, 0, 5, 1),
            insn(0x15, 5, 0, 2, 3),
            insn(0x55, 5, 0, 3, 5),
            insn(0x45, 2, 0, 2, 4),
            insn(0xb7, 0, 0, 0, 1),
            insn(0x95, 0, 0, 0, 0),
            insn(0xb7, 0, 0, 0, 0),
            insn(0x95, 0, 0, 0, 0),
        ]
    }

    fn prog(insns: Vec<Insn>) -> Prog {
        Prog {
            id: 1,
            insns,
            name: [0; 16],
            tag: [0; 8],
            gpl: true,
            load_time: 0,
            uid: 0,
            verified: 0,
        }
    }

    const ENV: Env = Env {
        uid: 0,
        gid: 0,
        cgroup: 1,
    };

    #[test]
    fn runs_a_device_filter() {
        let insns = device_prog();
        assert!(verify(&insns).is_ok());
        let p = prog(insns);
        let ask = |ty: u32, acc: u32, major, minor| run(&p, [acc << 16 | ty, major, minor], &ENV);
        assert_eq!(ask(DEV_CHAR, ACC_READ | ACC_WRITE, 1, 3), 1);
        assert_eq!(ask(DEV_CHAR, ACC_READ, 1, 5), 1);
        assert_eq!(ask(DEV_CHAR, ACC_WRITE, 1, 5), 0);
        assert_eq!(ask(DEV_CHAR, ACC_READ, 1, 7), 0);
        assert_eq!(ask(DEV_BLOCK, ACC_MKNOD, 1, 3), 0);
    }

    #[test]
    fn rejects_like_linux() {
        let errno = |insns: Vec<Insn>| verify(&insns).err().map(|r| r.errno);
        let exit = insn(0x95, 0, 0, 0, 0);
        let mov1 = insn(0xb7, 0, 0, 0, 1);
        assert_eq!(
            errno(vec![insn(0xff, 0, 0, 0, 0), exit]),
            Some(Errno::EINVAL)
        );
        assert_eq!(errno(vec![mov1]), Some(Errno::EINVAL));
        assert_eq!(
            errno(vec![mov1, insn(0x05, 0, 0, -2, 0), exit]),
            Some(Errno::EINVAL)
        );
        assert_eq!(errno(vec![exit]), Some(Errno::EACCES));
        assert_eq!(
            errno(vec![insn(0x61, 0, 1, 12, 0), exit]),
            Some(Errno::EACCES)
        );
        assert_eq!(
            errno(vec![insn(0x69, 0, 1, 4, 0), exit]),
            Some(Errno::EACCES)
        );
        assert_eq!(
            errno(vec![insn(0x69, 0, 1, 0, 0), insn(0x57, 0, 0, 0, 1), exit]),
            None
        );
        assert_eq!(
            errno(vec![insn(0xb7, 0, 0, 0, 2), exit]),
            Some(Errno::EINVAL)
        );
        assert_eq!(
            errno(vec![insn(0xbf, 0, 1, 0, 0), exit]),
            Some(Errno::EINVAL)
        );
        assert_eq!(
            errno(vec![insn(0xb7, 10, 0, 0, 0), mov1, exit]),
            Some(Errno::EACCES)
        );
        assert_eq!(
            errno(vec![mov1, insn(0x37, 0, 0, 0, 0), exit]),
            Some(Errno::EINVAL)
        );
        // The side setting r0 = 2 is skipped: after narrowing, the return value is still 0 or 1
        assert_eq!(
            errno(vec![
                insn(0x61, 0, 1, 4, 0),
                insn(0x25, 0, 0, 1, 1),
                exit,
                mov1,
                exit
            ]),
            None
        );
    }

    #[test]
    fn stack_and_helpers() {
        let insns = vec![
            insn(0x62, 10, 0, -4, 7),
            insn(0x61, 0, 10, -4, 0),
            insn(0x85, 0, 0, 0, 15),
            insn(0x61, 2, 10, -4, 0),
            insn(0xbf, 0, 2, 0, 0),
            insn(0x57, 0, 0, 0, 1),
            insn(0x95, 0, 0, 0, 0),
        ];
        assert!(verify(&insns).is_ok());
        assert_eq!(run(&prog(insns), [0; 3], &ENV), 1);
        // After a call r1 can no longer be read
        let bad = vec![
            insn(0x85, 0, 0, 0, 7),
            insn(0x61, 0, 1, 0, 0),
            insn(0x95, 0, 0, 0, 0),
        ];
        assert_eq!(verify(&bad).err().map(|r| r.errno), Some(Errno::EACCES));
    }

    #[test]
    fn sha1_known_answers() {
        let hex = |d: [u8; 20]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(
            hex(sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(hex(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }
}
