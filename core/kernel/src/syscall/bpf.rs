//! A subset of bpf(2) ([`crate::bpf`]): load cgroup device filter programs, attach them to
//! cgroups, detach and query them, get an fd by id, and get a program's info. As in Linux 6.1's
//! `kernel/bpf/syscall.c` and `kernel/bpf/cgroup.c`. Other commands and program types (maps,
//! socket filters, tracing...) are EINVAL, as when they are not built into the kernel.

use std::rc::Rc;

use super::SysResult;
use crate::bpf::{self, Insn, Prog};
use crate::cred::cap;
use crate::errno::Errno;
use crate::fd::{FileKind, OpenFile, oflags};
use crate::kernel::Cx;
use crate::pseudo::PseudoNode;
use crate::uaccess::UserSlice;
use crate::vfs::Node;

/// Commands (`enum bpf_cmd`).
mod cmd {
    pub const PROG_LOAD: i32 = 5;
    pub const PROG_ATTACH: i32 = 8;
    pub const PROG_DETACH: i32 = 9;
    pub const PROG_GET_FD_BY_ID: i32 = 13;
    pub const OBJ_GET_INFO_BY_FD: i32 = 15;
    pub const PROG_QUERY: i32 = 16;
}

/// The size of Linux 6.1's `union bpf_attr` (the largest member is the one for `BPF_PROG_LOAD`).
const ATTR_SIZE: usize = 144;
/// The size of `struct bpf_prog_info` (6.1).
const INFO_SIZE: usize = 232;
const PAGE_SIZE: usize = 4096;
/// Program load flags (`BPF_F_STRICT_ALIGNMENT` to `BPF_F_XDP_HAS_FRAGS`).
const PROG_FLAGS: u32 = 0x3f;
/// `BPF_F_SLEEPABLE`: device programs cannot sleep.
const SLEEPABLE: u32 = 0x10;
/// `BPF_F_QUERY_EFFECTIVE`.
const QUERY_EFFECTIVE: u32 = 1;
/// The two program types that processes without capabilities may also load
/// (`BPF_PROG_TYPE_SOCKET_FILTER`, `BPF_PROG_TYPE_CGROUP_SKB`).
const PROG_TYPE_SOCKET_FILTER: u32 = 1;
const PROG_TYPE_CGROUP_SKB: u32 = 8;
/// Log levels (`BPF_LOG_MASK`).
const LOG_MASK: u32 = 7;

pub(super) fn bpf(cx: &mut Cx<'_>, command: i32, uattr: u64, size: u32) -> SysResult {
    // When kernel.unprivileged_bpf_disabled (default 2) is not 0, every command needs a
    // capability; when it is 0, each command checks on its own
    if cx.kr().unprivileged_bpf_disabled != 0 && !bpf_capable(cx) {
        return Err(Errno::EPERM);
    }
    let size = tail_zero(cx, uattr, ATTR_SIZE, size as usize)?;
    let mut attr = [0u8; ATTR_SIZE];
    cx.mm()
        .read(UserSlice::new(uattr, size), &mut attr[..size])?;
    match command {
        cmd::PROG_LOAD => prog_load(cx, &attr),
        cmd::PROG_ATTACH => prog_attach(cx, &attr),
        cmd::PROG_DETACH => prog_detach(cx, &attr),
        cmd::PROG_QUERY => prog_query(cx, &attr, uattr),
        cmd::PROG_GET_FD_BY_ID => prog_get_fd_by_id(cx, &attr),
        cmd::OBJ_GET_INFO_BY_FD => obj_get_info_by_fd(cx, &attr, uattr),
        _ => Err(Errno::EINVAL),
    }
}

/// BPF_PROG_LOAD (Linux's `bpf_prog_load`).
fn prog_load(cx: &mut Cx<'_>, attr: &[u8]) -> SysResult {
    // The last field is core_relo_rec_size
    check_attr(attr, 140)?;
    let prog_type = u32_at(attr, 0);
    let insn_cnt = u32_at(attr, 4) as usize;
    let flags = u32_at(attr, 44);
    if flags & !PROG_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    // License: at most 127 bytes
    let license = match cx.mm().read_cstr(u64_at(attr, 16), 127) {
        Ok(l) => l,
        Err(Errno::ENAMETOOLONG) => {
            let mut b = vec![0u8; 127];
            cx.mm()
                .read(UserSlice::new(u64_at(attr, 16), 127), &mut b)?;
            b
        }
        Err(e) => return Err(e),
    };
    if insn_cnt == 0 || insn_cnt > bpf::MAX_INSNS {
        return Err(Errno::E2BIG);
    }
    // Without capabilities only socket filter and cgroup skb programs may be loaded (Tuxport has
    // neither, so they are EINVAL below)
    if prog_type != PROG_TYPE_SOCKET_FILTER && prog_type != PROG_TYPE_CGROUP_SKB && !bpf_capable(cx)
    {
        return Err(Errno::EPERM);
    }
    // Device programs are among the networking types that also need CAP_NET_ADMIN or
    // CAP_SYS_ADMIN (is_net_admin_prog_type)
    if prog_type == bpf::PROG_TYPE_CGROUP_DEVICE
        && !cx.cred().capable(cap::NET_ADMIN)
        && !cx.cred().capable(cap::SYS_ADMIN)
    {
        return Err(Errno::EPERM);
    }
    // No types other than device programs; offloading to a NIC (prog_ifindex) and sleepable ones
    // are not allowed either
    if prog_type != bpf::PROG_TYPE_CGROUP_DEVICE || u32_at(attr, 64) != 0 || flags & SLEEPABLE != 0
    {
        return Err(Errno::EINVAL);
    }
    let mut raw = vec![0u8; insn_cnt * 8];
    cx.mm()
        .read(UserSlice::new(u64_at(attr, 8), raw.len()), &mut raw)?;
    let insns: Vec<Insn> = raw.chunks_exact(8).map(Insn::decode).collect();
    // Name: NUL-terminated, only letters, digits, `_` and `.` (bpf_obj_name_cpy)
    let mut name = [0u8; 16];
    name.copy_from_slice(&attr[48..64]);
    let len = name.iter().position(|&b| b == 0).ok_or(Errno::EINVAL)?;
    if !name[..len]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
    {
        return Err(Errno::EINVAL);
    }
    // Verifier log: if given, it must be sane (bpf_verifier_log_attr_valid)
    let (log_level, log_size, log_buf) = (u32_at(attr, 24), u32_at(attr, 28), u64_at(attr, 32));
    if (log_level != 0 || log_buf != 0 || log_size != 0)
        && (!(128..=u32::MAX >> 2).contains(&log_size)
            || log_level == 0
            || log_buf == 0
            || log_level & !LOG_MASK != 0)
    {
        return Err(Errno::EINVAL);
    }
    let verified = match bpf::verify(&insns) {
        Ok(n) => n,
        Err(r) => {
            if log_level != 0 {
                let mut msg = r.msg.into_bytes();
                msg.push(b'\n');
                msg.truncate(log_size as usize - 1);
                msg.push(0);
                cx.mm().write(UserSlice::new(log_buf, msg.len()), &msg)?;
            }
            return Err(r.errno);
        }
    };
    let digest = bpf::sha1(&raw);
    let id = cx.k().bpf.next_id();
    let prog = Rc::new(Prog {
        id,
        insns,
        name,
        tag: digest[..8].try_into().unwrap(),
        gpl: bpf::gpl_compatible(&license),
        load_time: crate::pseudo::uptime_ns(),
        uid: cx.cred().ruid,
        verified,
    });
    cx.k().bpf.insert(&prog);
    install(cx, prog)
}

/// BPF_PROG_ATTACH (`bpf_prog_attach`, `__cgroup_bpf_attach`).
fn prog_attach(cx: &mut Cx<'_>, attr: &[u8]) -> SysResult {
    // The last field is replace_bpf_fd
    check_attr(attr, 20)?;
    let flags = u32_at(attr, 12);
    if flags & !(bpf::ALLOW_OVERRIDE | bpf::ALLOW_MULTI | bpf::REPLACE) != 0 {
        return Err(Errno::EINVAL);
    }
    if u32_at(attr, 8) != bpf::ATTACH_CGROUP_DEVICE {
        return Err(Errno::EINVAL);
    }
    let prog = prog_of_fd(cx, u32_at(attr, 4))?;
    let id = cgroup_of_fd(cx, u32_at(attr, 0))?;
    let multi = flags & bpf::ALLOW_MULTI != 0;
    let replace = if multi && flags & bpf::REPLACE != 0 {
        Some(prog_of_fd(cx, u32_at(attr, 16))?)
    } else {
        None
    };
    // OVERRIDE and MULTI cannot be combined; REPLACE needs MULTI
    if (flags & bpf::ALLOW_OVERRIDE != 0 && multi) || (flags & bpf::REPLACE != 0 && !multi) {
        return Err(Errno::EINVAL);
    }
    let k = cx.k();
    if !crate::cgroup::hierarchy_allows_attach(k, id) {
        return Err(Errno::EPERM);
    }
    let g = k.cgroups.map.get_mut(&id).ok_or(Errno::EBADF)?;
    let saved = flags & (bpf::ALLOW_OVERRIDE | bpf::ALLOW_MULTI);
    // The programs already attached have different flags from this one
    if !g.device_progs.is_empty() && g.device_flags != saved {
        return Err(Errno::EPERM);
    }
    if g.device_progs.len() >= bpf::MAX_PROGS {
        return Err(Errno::E2BIG);
    }
    // find_attach_entry: without MULTI, the existing one is replaced; with it, the same program
    // cannot be attached twice, and the one being replaced must already be attached
    let slot = if !multi {
        (!g.device_progs.is_empty()).then_some(0)
    } else {
        let replacing = |p: &Rc<Prog>| replace.as_ref().is_some_and(|r| Rc::ptr_eq(r, p));
        if g.device_progs
            .iter()
            .any(|p| Rc::ptr_eq(p, &prog) && !replacing(&prog))
        {
            return Err(Errno::EINVAL);
        }
        match &replace {
            Some(r) => Some(
                g.device_progs
                    .iter()
                    .position(|p| Rc::ptr_eq(p, r))
                    .ok_or(Errno::ENOENT)?,
            ),
            None => None,
        }
    };
    match slot {
        Some(i) => g.device_progs[i] = prog,
        None => g.device_progs.push(prog),
    }
    g.device_flags = saved;
    Ok(0)
}

/// BPF_PROG_DETACH (`__cgroup_bpf_detach`): an fd that is not a program counts as not given;
/// programs attached with MULTI must name the attached one, otherwise the only one is detached.
fn prog_detach(cx: &mut Cx<'_>, attr: &[u8]) -> SysResult {
    // The last field is attach_type
    check_attr(attr, 12)?;
    if u32_at(attr, 8) != bpf::ATTACH_CGROUP_DEVICE {
        return Err(Errno::EINVAL);
    }
    let id = cgroup_of_fd(cx, u32_at(attr, 0))?;
    let prog = prog_of_fd(cx, u32_at(attr, 4)).ok();
    let g = cx.k().cgroups.map.get_mut(&id).ok_or(Errno::EBADF)?;
    let i = if g.device_flags & bpf::ALLOW_MULTI == 0 {
        if g.device_progs.is_empty() {
            return Err(Errno::ENOENT);
        }
        0
    } else {
        let prog = prog.ok_or(Errno::EINVAL)?;
        g.device_progs
            .iter()
            .position(|p| Rc::ptr_eq(p, &prog))
            .ok_or(Errno::ENOENT)?
    };
    g.device_progs.remove(i);
    if g.device_progs.is_empty() {
        g.device_flags = 0;
    }
    Ok(0)
}

/// The program on an fd (`bpf_prog_get_type`): a missing fd is EBADF, a non-program EINVAL.
fn prog_of_fd(cx: &Cx<'_>, fd: u32) -> Result<Rc<Prog>, Errno> {
    let file = cx.fds().get(fd as i32).map_err(|_| Errno::EBADF)?;
    let prog = match &file.borrow().kind {
        FileKind::Bpf(p) => p.clone(),
        _ => return Err(Errno::EINVAL),
    };
    Ok(prog)
}

/// BPF_PROG_QUERY (`bpf_prog_query`, `__cgroup_bpf_query`): needs `CAP_NET_ADMIN`. Writes back
/// attach_flags (0 when querying only effective ones) and the program count; when the space given
/// is too small, writes only the ones that fit and returns ENOSPC.
fn prog_query(cx: &mut Cx<'_>, attr: &[u8], uattr: u64) -> SysResult {
    if !cx.cred().capable(cap::NET_ADMIN) {
        return Err(Errno::EPERM);
    }
    // The last field is prog_attach_flags
    check_attr(attr, 40)?;
    let query_flags = u32_at(attr, 8);
    if query_flags & !QUERY_EFFECTIVE != 0 || u32_at(attr, 4) != bpf::ATTACH_CGROUP_DEVICE {
        return Err(Errno::EINVAL);
    }
    let id = cgroup_of_fd(cx, u32_at(attr, 0))?;
    let effective = query_flags & QUERY_EFFECTIVE != 0;
    let prog_attach_flags = u64_at(attr, 32);
    if effective && prog_attach_flags != 0 {
        return Err(Errno::EINVAL);
    }
    let (progs, flags) = if effective {
        (crate::cgroup::effective_device_progs(cx.kr(), id), 0)
    } else {
        let g = cx.kr().cgroups.get(id).ok_or(Errno::EBADF)?;
        (g.device_progs.clone(), g.device_flags)
    };
    super::put(cx, uattr + 12, flags.to_le_bytes())?;
    super::put(cx, uattr + 24, (progs.len() as u32).to_le_bytes())?;
    let (room, ids) = (u32_at(attr, 24) as usize, u64_at(attr, 16));
    if room == 0 || ids == 0 || progs.is_empty() {
        return Ok(0);
    }
    let n = progs.len().min(room);
    let out: Vec<u8> = progs[..n].iter().flat_map(|p| p.id.to_le_bytes()).collect();
    cx.mm().write(UserSlice::new(ids, out.len()), &out)?;
    if prog_attach_flags != 0 {
        let out: Vec<u8> = (0..n).flat_map(|_| flags.to_le_bytes()).collect();
        cx.mm()
            .write(UserSlice::new(prog_attach_flags, out.len()), &out)?;
    }
    if n < progs.len() {
        return Err(Errno::ENOSPC);
    }
    Ok(0)
}

/// The cgroup2 directory an fd has open (`cgroup_get_from_fd`, O_PATH ones too); anything else
/// is EBADF.
fn cgroup_of_fd(cx: &Cx<'_>, fd: u32) -> Result<u64, Errno> {
    let file = cx.fds().get(fd as i32).map_err(|_| Errno::EBADF)?;
    let node = file.borrow().node();
    match node {
        Some(Node::Pseudo(_, PseudoNode::Cgroup(id, b""))) if cx.kr().cgroups.get(id).is_some() => {
            Ok(id)
        }
        _ => Err(Errno::EBADF),
    }
}

/// BPF_PROG_GET_FD_BY_ID: needs `CAP_SYS_ADMIN`; no program with this id is ENOENT.
fn prog_get_fd_by_id(cx: &mut Cx<'_>, attr: &[u8]) -> SysResult {
    // The last field is prog_id
    check_attr(attr, 4)?;
    if !cx.cred().capable(cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    let prog = cx.kr().bpf.get(u32_at(attr, 0)).ok_or(Errno::ENOENT)?;
    install(cx, prog)
}

/// New fds are all close-on-exec (`bpf_prog_new_fd`).
fn install(cx: &mut Cx<'_>, prog: Rc<Prog>) -> SysResult {
    let file = OpenFile::new(FileKind::Bpf(prog), oflags::O_RDWR);
    Ok(cx.fds_mut().install(file, true, 0)? as u64)
}

/// BPF_OBJ_GET_INFO_BY_FD (`bpf_prog_get_info_by_fd`): first reads the guest's `bpf_prog_info`
/// (which holds the places and lengths for instructions and tags), fills it in and writes back
/// the same length. A missing fd is EBADFD, a non-program EINVAL.
fn obj_get_info_by_fd(cx: &mut Cx<'_>, attr: &[u8], uattr: u64) -> SysResult {
    // The last field is info.info
    check_attr(attr, 16)?;
    let file = cx
        .fds()
        .get(u32_at(attr, 0) as i32)
        .map_err(|_| Errno::EBADFD)?;
    let prog = match &file.borrow().kind {
        FileKind::Bpf(p) => p.clone(),
        _ => return Err(Errno::EINVAL),
    };
    let uinfo = u64_at(attr, 8);
    let len = tail_zero(cx, uinfo, INFO_SIZE, u32_at(attr, 4) as usize)?;
    let mut info = [0u8; INFO_SIZE];
    cx.mm().read(UserSlice::new(uinfo, len), &mut info[..len])?;
    let set32 = |info: &mut [u8; INFO_SIZE], off: usize, v: u32| {
        info[off..off + 4].copy_from_slice(&v.to_le_bytes());
    };
    let set64 = |info: &mut [u8; INFO_SIZE], off: usize, v: u64| {
        info[off..off + 8].copy_from_slice(&v.to_le_bytes());
    };
    // Record sizes, if given, must be right
    let rec = |nr: usize, size: usize, want: u32| {
        (u32_at(&info, nr) != 0 || u32_at(&info, size) != 0) && u32_at(&info, size) != want
    };
    if rec(144, 132, 8) || rec(148, 172, 16) || rec(168, 176, 8) {
        return Err(Errno::EINVAL);
    }
    set32(&mut info, 0, bpf::PROG_TYPE_CGROUP_DEVICE);
    set32(&mut info, 4, prog.id);
    info[8..16].copy_from_slice(&prog.tag);
    set64(&mut info, 40, prog.load_time);
    set32(&mut info, 48, cx.cred().uid_out(prog.uid));
    set32(&mut info, 52, 0);
    info[64..80].copy_from_slice(&prog.name);
    set32(&mut info, 84, prog.gpl as u32);
    set32(&mut info, 216, prog.verified);
    // Without capabilities, no instructions and no record counts (the goto done in
    // bpf_prog_get_info_by_fd)
    if !bpf_capable(cx) {
        for off in [16, 20, 104, 108, 144, 148, 168] {
            set32(&mut info, off, 0);
        }
        cx.mm().write(UserSlice::new(uinfo, len), &info[..len])?;
        super::put(cx, uattr + 4, (len as u32).to_le_bytes())?;
        return Ok(0);
    }
    // Instructions (not rewritten), no JIT
    let bytes = prog.bytes();
    let (ulen, uinsns) = (u32_at(&info, 20) as usize, u64_at(&info, 32));
    set32(&mut info, 20, bytes.len() as u32);
    if ulen != 0 {
        let n = ulen.min(bytes.len());
        cx.mm().write(UserSlice::new(uinsns, n), &bytes[..n])?;
    }
    set32(&mut info, 16, 0);
    set32(&mut info, 104, 0);
    set32(&mut info, 108, 0);
    set32(&mut info, 144, 0);
    set32(&mut info, 148, 0);
    set32(&mut info, 168, 0);
    set32(&mut info, 132, 8);
    set32(&mut info, 172, 16);
    set32(&mut info, 176, 8);
    // Only one function: one tag
    let (ntags, utags) = (u32_at(&info, 180), u64_at(&info, 184));
    set32(&mut info, 180, 1);
    if ntags != 0 {
        cx.mm().write(UserSlice::new(utags, 8), &prog.tag)?;
    }
    set32(&mut info, 216, prog.verified);
    cx.mm().write(UserSlice::new(uinfo, len), &info[..len])?;
    super::put(cx, uattr + 4, (len as u32).to_le_bytes())?;
    Ok(0)
}

/// `bpf_capable()`: `CAP_BPF` or `CAP_SYS_ADMIN` in the boot-time user namespace.
fn bpf_capable(cx: &Cx<'_>) -> bool {
    let cred = cx.cred();
    cred.capable(cap::BPF) || cred.capable(cap::SYS_ADMIN)
}

/// When the guest's struct is longer than the kernel's, the extra part must be 0
/// (`bpf_check_uarg_tail_zero`); more than one page is E2BIG. Returns the length to read.
fn tail_zero(cx: &Cx<'_>, addr: u64, expected: usize, actual: usize) -> Result<usize, Errno> {
    if actual > PAGE_SIZE {
        return Err(Errno::E2BIG);
    }
    if actual > expected {
        let mut tail = vec![0u8; actual - expected];
        cx.mm().read(
            UserSlice::new(addr + expected as u64, tail.len()),
            &mut tail,
        )?;
        if tail.iter().any(|&b| b != 0) {
            return Err(Errno::E2BIG);
        }
    }
    Ok(actual.min(expected))
}

/// `CHECK_ATTR`: everything after the last field this command uses must be 0.
fn check_attr(attr: &[u8], end: usize) -> Result<(), Errno> {
    if attr[end..].iter().any(|&b| b != 0) {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

fn u32_at(a: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(a[off..off + 4].try_into().unwrap())
}

fn u64_at(a: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(a[off..off + 8].try_into().unwrap())
}

/// Runs the device filter programs of the caller's cgroup before opening a device node or mknod
/// ([`bpf::check_device`]). `kind` is `S_IFCHR` or `S_IFBLK`, `rdev` is in Linux's encoding.
pub(crate) fn check_device(cx: &Cx<'_>, kind: u32, rdev: u64, access: u32) -> Result<(), Errno> {
    let dev_type = if kind == crate::vfs::store::mode::S_IFBLK {
        bpf::DEV_BLOCK
    } else {
        bpf::DEV_CHAR
    };
    let r = rdev as u32;
    let (major, minor) = ((r & 0xfff00) >> 8, (r & 0xff) | ((r >> 12) & 0xfff00));
    let cred = cx.cred();
    let env = bpf::Env {
        uid: cred.ruid,
        gid: cred.rgid,
        cgroup: cx.t.group.borrow().cgroup,
    };
    bpf::check_device(cx.kr(), &env, (dev_type, major, minor), access)
}
