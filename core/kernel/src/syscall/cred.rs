//! Credential syscalls: user and group IDs, supplementary groups, capabilities (rules in
//! [`crate::cred`]). IDs are translated through the caller's user namespace: reads return the
//! mapped ones (65534 when unmapped), and an unmapped ID passed in is EINVAL.

use super::{SysResult, get, put};
use crate::cred::{Cred, NGROUPS_MAX};
use crate::errno::Errno;
use crate::kernel::Cx;

// ---- Id syscalls ----

pub(super) fn getuid(cx: &mut Cx<'_>, effective: bool) -> SysResult {
    let c = cx.cred();
    Ok(c.uid_out(if effective { c.euid } else { c.ruid }) as u64)
}

pub(super) fn getgid(cx: &mut Cx<'_>, effective: bool) -> SysResult {
    let c = cx.cred();
    Ok(c.gid_out(if effective { c.egid } else { c.rgid }) as u64)
}

pub(super) fn setuid(cx: &mut Cx<'_>, uid: u64) -> SysResult {
    let uid = uid_arg(cx, uid)?.ok_or(Errno::EINVAL)?;
    update(cx, |c| c.setuid(uid)).map(|()| 0)
}

pub(super) fn setgid(cx: &mut Cx<'_>, gid: u64) -> SysResult {
    let gid = gid_arg(cx, gid)?.ok_or(Errno::EINVAL)?;
    update(cx, |c| c.setgid(gid)).map(|()| 0)
}

pub(super) fn setreuid(cx: &mut Cx<'_>, r: u64, e: u64) -> SysResult {
    let (r, e) = (uid_arg(cx, r)?, uid_arg(cx, e)?);
    update(cx, |c| c.setreuid(r, e)).map(|()| 0)
}

pub(super) fn setregid(cx: &mut Cx<'_>, r: u64, e: u64) -> SysResult {
    let (r, e) = (gid_arg(cx, r)?, gid_arg(cx, e)?);
    update(cx, |c| c.setregid(r, e)).map(|()| 0)
}

pub(super) fn setresuid(cx: &mut Cx<'_>, r: u64, e: u64, s: u64) -> SysResult {
    let (r, e, s) = (uid_arg(cx, r)?, uid_arg(cx, e)?, uid_arg(cx, s)?);
    update(cx, |c| c.setresuid(r, e, s)).map(|()| 0)
}

pub(super) fn setresgid(cx: &mut Cx<'_>, r: u64, e: u64, s: u64) -> SysResult {
    let (r, e, s) = (gid_arg(cx, r)?, gid_arg(cx, e)?, gid_arg(cx, s)?);
    update(cx, |c| c.setresgid(r, e, s)).map(|()| 0)
}

/// getresuid, getresgid: the three IDs are written to three addresses.
pub(super) fn getres(cx: &mut Cx<'_>, gid: bool, addrs: [u64; 3]) -> SysResult {
    let c = cx.cred();
    let ids = if gid {
        [c.rgid, c.egid, c.sgid].map(|g| c.gid_out(g))
    } else {
        [c.ruid, c.euid, c.suid].map(|u| c.uid_out(u))
    };
    for (addr, id) in addrs.into_iter().zip(ids) {
        put(cx, addr, id.to_le_bytes())?;
    }
    Ok(0)
}

/// setfsuid: an unmapped ID is silently ignored like a disallowed one; returns the old fsuid
/// (mapped).
pub(super) fn setfsuid(cx: &mut Cx<'_>, uid: u64) -> SysResult {
    let uid = uid_arg(cx, uid).ok().flatten();
    update(cx, |c| {
        let old = c.setfsuid(uid);
        Ok(c.uid_out(old) as u64)
    })
}

pub(super) fn setfsgid(cx: &mut Cx<'_>, gid: u64) -> SysResult {
    let gid = gid_arg(cx, gid).ok().flatten();
    update(cx, |c| {
        let old = c.setfsgid(gid);
        Ok(c.gid_out(old) as u64)
    })
}

pub(super) fn getgroups(cx: &mut Cx<'_>, size: i32, list: u64) -> SysResult {
    let cred = cx.cred();
    let groups: Vec<u32> = cred.groups.iter().map(|&g| cred.gid_out(g)).collect();
    let size = usize::try_from(size).map_err(|_| Errno::EINVAL)?;
    if size == 0 {
        return Ok(groups.len() as u64);
    }
    if size < groups.len() {
        return Err(Errno::EINVAL);
    }
    for (i, gid) in groups.iter().enumerate() {
        put(cx, list + 4 * i as u64, gid.to_le_bytes())?;
    }
    Ok(groups.len() as u64)
}

pub(super) fn setgroups(cx: &mut Cx<'_>, size: i32, list: u64) -> SysResult {
    let size = usize::try_from(size)
        .ok()
        .filter(|&n| n <= NGROUPS_MAX)
        .ok_or(Errno::EINVAL)?;
    let mut groups = Vec::with_capacity(size);
    for i in 0..size {
        let gid = u32::from_le_bytes(get(cx, list + 4 * i as u64)?);
        groups.push(gid_arg(cx, gid as u64)?.ok_or(Errno::EINVAL)?);
    }
    update(cx, |c| c.setgroups(groups)).map(|()| 0)
}

/// Converts a user ID argument to a kernel ID: -1 is `None`, unmapped is EINVAL (`uid_valid`
/// after `make_kuid`).
fn uid_arg(cx: &Cx<'_>, id: u64) -> Result<Option<u32>, Errno> {
    id_arg(id)
        .map(|id| cx.cred().uid_in(id).ok_or(Errno::EINVAL))
        .transpose()
}

/// Converts a group ID argument to a kernel ID, with the same rules as [`uid_arg`].
fn gid_arg(cx: &Cx<'_>, id: u64) -> Result<Option<u32>, Errno> {
    id_arg(id)
        .map(|id| cx.cred().gid_in(id).ok_or(Errno::EINVAL))
        .transpose()
}

/// An ID argument meaning "unchanged" (-1) is `None`.
fn id_arg(id: u64) -> Option<u32> {
    (id as u32 != u32::MAX).then_some(id as u32)
}

// ---- Capabilities ----

/// Versions for capget and capset: v1 has only the low 32 bits, v2 and v3 have two 32-bit sets.
const CAPABILITY_VERSION_1: u32 = 0x1998_0330;
const CAPABILITY_VERSION_2: u32 = 0x2007_1026;
const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

pub(super) fn capget(cx: &mut Cx<'_>, hdr: u64, data: u64) -> SysResult {
    let header = cap_header(cx, hdr);
    // A null data pointer only probes the version: an unknown version also returns 0 after the
    // kernel's version is written back, and the pid is ignored (Linux's capget;
    // moby/sys/capability gets the version this way, and podman and runc both use it)
    if data == 0 {
        return match header {
            Ok(_) | Err(Errno::EINVAL) => Ok(0),
            Err(e) => Err(e),
        };
    }
    let (words, pid) = header?;
    if pid < 0 {
        return Err(Errno::EINVAL);
    }
    let cred = if pid == 0 {
        cx.cred()
    } else {
        // The thread's own, or a main thread's that has ended and is still a zombie
        cx.kr().cred_of(cx.pid_in(pid)).ok_or(Errno::ESRCH)?
    };
    for i in 0..words {
        let mut b = [0u8; 12];
        for (j, set) in [cred.cap_effective, cred.cap_permitted, cred.cap_inheritable]
            .into_iter()
            .enumerate()
        {
            b[j * 4..j * 4 + 4].copy_from_slice(&((set >> (32 * i)) as u32).to_le_bytes());
        }
        put(cx, data + 12 * i as u64, b)?;
    }
    Ok(0)
}

pub(super) fn capset(cx: &mut Cx<'_>, hdr: u64, data: u64) -> SysResult {
    let (words, pid) = cap_header(cx, hdr)?;
    if pid != 0 && cx.pid_in(pid) != cx.tid() {
        return Err(Errno::EPERM);
    }
    let mut sets = [0u64; 3];
    for i in 0..words {
        let b: [u8; 12] = get(cx, data + 12 * i as u64)?;
        for (j, set) in sets.iter_mut().enumerate() {
            let word = u32::from_le_bytes(b[j * 4..j * 4 + 4].try_into().unwrap());
            *set |= (word as u64) << (32 * i);
        }
    }
    let [effective, permitted, inheritable] = sets;
    update(cx, |c| c.capset(effective, permitted, inheritable)).map(|()| 0)
}

/// Header of capget and capset: checks the version (when unknown, writes back the kernel's
/// version and returns EINVAL), and returns the number of data sets and the pid.
fn cap_header(cx: &Cx<'_>, hdr: u64) -> Result<(usize, i32), Errno> {
    let h: [u8; 8] = get(cx, hdr)?;
    let version = u32::from_le_bytes(h[..4].try_into().unwrap());
    let pid = i32::from_le_bytes(h[4..].try_into().unwrap());
    match version {
        CAPABILITY_VERSION_1 => Ok((1, pid)),
        CAPABILITY_VERSION_2 | CAPABILITY_VERSION_3 => Ok((2, pid)),
        _ => {
            put(cx, hdr, CAPABILITY_VERSION_3.to_le_bytes())?;
            Err(Errno::EINVAL)
        }
    }
}

// ---- Committing credentials ----

/// Modifies the current thread's credentials: if `f` fails, the credentials stay unchanged. As
/// with Linux's `commit_creds`, when the effective or filesystem user or group changes, or new
/// capabilities are gained, the process is no longer dumpable (the files in `/proc/<pid>` then
/// belong to root), and the thread's children no longer get a signal when it exits.
fn update<T>(cx: &Cx<'_>, f: impl FnOnce(&mut Cred) -> Result<T, Errno>) -> Result<T, Errno> {
    let mut cred = cx.cred();
    let old = cred.clone();
    let r = f(&mut cred)?;
    let ids = |c: &Cred| (c.euid, c.egid, c.fsuid, c.fsgid);
    if ids(&cred) != ids(&old) || cred.cap_permitted & !old.cap_permitted != 0 {
        cx.t.group.borrow_mut().dumpable = false;
        cx.task.pdeathsig.set(0);
    }
    cx.set_cred(cred);
    Ok(r)
}
