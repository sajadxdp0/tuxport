//! System V IPC syscalls: the msgget family, the semget family and the shmget family (for the
//! objects see [`crate::ipc`]).
//!
//! Structs follow asm-generic's 64-bit layout (`msqid64_ds` etc.). musl adds `IPC_64` to cmd, and
//! as on Linux it is simply dropped. Waits (msgsnd on a full queue, msgrcv with no message, a
//! semop that cannot complete at once) go through the unified wait mechanism: every change
//! requests a wake and waiters check again; when the object is removed during the wait, EIDRM is
//! returned.

use tuxport_engine::{PAGE_SIZE, prot};

use super::{SysResult, get, put, read_timespec};
use crate::cred::{Cred, UserNs, cap};
use crate::errno::Errno;
use crate::ipc::{
    IPC_NOWAIT, Ipc, Msg, MsgQueue, Perm, SHM_DEST, SHM_LOCKED, SemOp, SemSet, ShmSeg, limits,
    nattch, now, reap_shm,
};
use crate::kernel::{Cx, Kernel, Tid};
use crate::mm::SharedMemory;
use crate::uaccess::UserSlice;

const IPC_RMID: i32 = 0;
const IPC_SET: i32 = 1;
const IPC_STAT: i32 = 2;
const IPC_INFO: i32 = 3;
const IPC_64: i32 = 0x100;

const MSG_STAT: i32 = 11;
const MSG_INFO: i32 = 12;
const MSG_STAT_ANY: i32 = 13;
const MSG_NOERROR: i32 = 0o10000;
const MSG_EXCEPT: i32 = 0o20000;
const MSG_COPY: i32 = 0o40000;

const GETPID: i32 = 11;
const GETVAL: i32 = 12;
const GETALL: i32 = 13;
const GETNCNT: i32 = 14;
const GETZCNT: i32 = 15;
const SETVAL: i32 = 16;
const SETALL: i32 = 17;
const SEM_STAT: i32 = 18;
const SEM_INFO: i32 = 19;
const SEM_STAT_ANY: i32 = 20;

const SHM_LOCK: i32 = 11;
const SHM_UNLOCK: i32 = 12;
const SHM_STAT: i32 = 13;
const SHM_INFO: i32 = 14;
const SHM_STAT_ANY: i32 = 15;
const SHM_HUGETLB: i32 = 0o4000;
const SHM_RDONLY: i32 = 0o10000;
const SHM_RND: i32 = 0o20000;
const SHM_REMAP: i32 = 0o40000;
const SHM_EXEC: i32 = 0o100000;

/// Read and write permissions (`S_IRUGO`, `S_IWUGO`).
const READ: u32 = 0o444;
const WRITE: u32 = 0o222;

// ---- Message queues ----

pub(super) fn msgget(cx: &mut Cx<'_>, key: i32, flags: i32) -> SysResult {
    let cred = cx.cred();
    let owner = ipc_owner(cx);
    let ipc = &mut cx.ipc_mut();
    let t = ipc.tunables;
    ipc.msg
        .get_or_create(
            key,
            flags,
            (&cred, &owner),
            t.msgmni,
            |_| Ok(()),
            || Ok(MsgQueue::new(t.msgmnb)),
        )
        .map(|id| id as u64)
}

pub(super) fn msgsnd(cx: &mut Cx<'_>, id: i32, msgp: u64, size: u64, flags: i32) -> SysResult {
    let msgmax = cx.ipc().tunables.msgmax;
    let len = usize::try_from(size as i64)
        .ok()
        .filter(|&n| n as u64 <= msgmax && id >= 0)
        .ok_or(Errno::EINVAL)?;
    let mtype = i64::from_le_bytes(get(cx, msgp)?);
    if mtype < 1 {
        return Err(Errno::EINVAL);
    }
    let mut text = crate::ipc::zeroed(len)?;
    cx.mm().read(UserSlice::new(msgp + 8, len), &mut text)?;
    let cred = cx.cred();
    let owner = ipc_owner(cx);
    let tgid = cx.tgid();
    let mut waited = false;
    loop {
        let (perm, q) = match cx.ipc_mut().msg.get_mut(id) {
            Ok(e) => e,
            Err(_) if waited => return Err(Errno::EIDRM),
            Err(e) => return Err(e),
        };
        perm.check(&cred, &owner, WRITE)?;
        if q.fits(len) {
            q.cbytes += len as u64;
            q.msgs.push_back(Msg { mtype, text });
            q.lspid = tgid;
            q.stime = now();
            crate::kernel::request_wake();
            return Ok(0);
        }
        if flags & IPC_NOWAIT != 0 {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTNOHAND);
        }
        waited = true;
        cx.wait(None, &mut [])?;
    }
}

pub(super) fn msgrcv(
    cx: &mut Cx<'_>,
    id: i32,
    msgp: u64,
    size: u64,
    mtype: i64,
    flags: i32,
) -> SysResult {
    let max = usize::try_from(size as i64)
        .ok()
        .filter(|_| id >= 0)
        .ok_or(Errno::EINVAL)?;
    // MSG_COPY requires CONFIG_CHECKPOINT_RESTORE
    if flags & MSG_COPY != 0 {
        return Err(Errno::ENOSYS);
    }
    let cred = cx.cred();
    let owner = ipc_owner(cx);
    let tgid = cx.tgid();
    let mut waited = false;
    let msg = loop {
        let (perm, q) = match cx.ipc_mut().msg.get_mut(id) {
            Ok(e) => e,
            Err(_) if waited => return Err(Errno::EIDRM),
            Err(e) => return Err(e),
        };
        perm.check(&cred, &owner, READ)?;
        if let Some(i) = q.select(mtype, flags & MSG_EXCEPT != 0) {
            if q.msgs[i].text.len() > max && flags & MSG_NOERROR == 0 {
                return Err(Errno::E2BIG);
            }
            let m = q.msgs.remove(i).unwrap();
            q.cbytes -= m.text.len() as u64;
            q.lrpid = tgid;
            q.rtime = now();
            crate::kernel::request_wake();
            break m;
        }
        if flags & IPC_NOWAIT != 0 {
            return Err(Errno::ENOMSG);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTNOHAND);
        }
        waited = true;
        cx.wait(None, &mut [])?;
    };
    let n = msg.text.len().min(max);
    put(cx, msgp, msg.mtype.to_le_bytes())?;
    cx.mm().write(UserSlice::new(msgp + 8, n), &msg.text[..n])?;
    Ok(n as u64)
}

pub(super) fn msgctl(cx: &mut Cx<'_>, id: i32, cmd: i32, buf: u64) -> SysResult {
    let cmd = cmd & !IPC_64;
    let cred = cx.cred();
    let owner = ipc_owner(cx);
    match cmd {
        IPC_INFO | MSG_INFO => {
            let tun = cx.ipc().tunables;
            let t = &cx.ipc().msg;
            let (pool, map, tql) = if cmd == IPC_INFO {
                (tun.msgmni * tun.msgmnb / 1024, tun.msgmnb, tun.msgmnb)
            } else {
                let msgs = t.iter().map(|(_, _, q)| q.msgs.len() as u64).sum();
                let bytes = t.iter().map(|(_, _, q)| q.cbytes).sum();
                (t.len() as u64, msgs, bytes)
            };
            // struct msginfo: 7 ints and an unsigned short
            let mut b = [0u8; 32];
            words(
                &mut b,
                0,
                &[pool, map, tun.msgmax, tun.msgmnb, tun.msgmni, 16, tql],
                4,
            );
            b[28..30].copy_from_slice(&0xffffu16.to_le_bytes());
            let max = t.max_index();
            put(cx, buf, b)?;
            Ok(max as u64)
        }
        IPC_STAT | MSG_STAT | MSG_STAT_ANY => {
            let ns = cx.pid_ns();
            let t = &cx.ipc().msg;
            let (full, (perm, q)) =
                stat_target(t, (&cred, &owner), id, cmd != IPC_STAT, cmd == MSG_STAT_ANY)?;
            let b = encode_msqid(perm, &cred, q, |p| ns.vnr(p).unwrap_or(0));
            put(cx, buf, b)?;
            Ok(if cmd == IPC_STAT { 0 } else { full as u64 })
        }
        IPC_SET => {
            let b: [u8; 120] = get(cx, buf)?;
            let qbytes = u64::from_le_bytes(b[88..96].try_into().unwrap());
            let msgmnb = cx.ipc().tunables.msgmnb;
            let (perm, q) = cx.ipc_mut().msg.get_mut(id)?;
            perm.may_change(&cred, &owner)?;
            if qbytes > msgmnb && !cred.capable(cap::SYS_RESOURCE) {
                return Err(Errno::EPERM);
            }
            perm.set(&b[..48], &cred)?;
            q.qbytes = qbytes;
            q.ctime = now();
            crate::kernel::request_wake();
            Ok(0)
        }
        IPC_RMID => {
            let t = &mut cx.ipc_mut().msg;
            t.get(id)?.0.may_change(&cred, &owner)?;
            t.remove(id);
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `struct msqid64_ds` (120 bytes).
/// `pid` translates a recorded pid into the one the caller sees.
fn encode_msqid(perm: &Perm, cred: &Cred, q: &MsgQueue, pid: impl Fn(Tid) -> Tid) -> [u8; 120] {
    let mut b = [0u8; 120];
    b[..48].copy_from_slice(&perm.encode(cred));
    words(
        &mut b,
        48,
        &[
            q.stime as u64,
            q.rtime as u64,
            q.ctime as u64,
            q.cbytes,
            q.msgs.len() as u64,
            q.qbytes,
        ],
        8,
    );
    words(&mut b, 96, &[pid(q.lspid) as u64, pid(q.lrpid) as u64], 4);
    b
}

// ---- Semaphores ----

pub(super) fn semget(cx: &mut Cx<'_>, key: i32, nsems: i32, flags: i32) -> SysResult {
    let [semmsl, _, _, semmni] = cx.ipc().tunables.sem;
    let n = usize::try_from(nsems)
        .ok()
        .filter(|&n| n as u64 <= semmsl)
        .ok_or(Errno::EINVAL)?;
    let cred = cx.cred();
    let owner = ipc_owner(cx);
    cx.ipc_mut()
        .sem
        .get_or_create(
            key,
            flags,
            (&cred, &owner),
            semmni,
            |s| {
                if n > s.vals.len() {
                    Err(Errno::EINVAL)
                } else {
                    Ok(())
                }
            },
            || {
                if n == 0 {
                    Err(Errno::EINVAL)
                } else {
                    SemSet::new(n)
                }
            },
        )
        .map(|id| id as u64)
}

pub(super) fn semctl(cx: &mut Cx<'_>, id: i32, num: i32, cmd: i32, arg: u64) -> SysResult {
    let cmd = cmd & !IPC_64;
    let cred = cx.cred();
    let owner = ipc_owner(cx);
    let tgid = cx.tgid();
    match cmd {
        IPC_INFO | SEM_INFO => {
            let [semmsl, semmns, semopm, semmni] = cx.ipc().tunables.sem;
            let t = &cx.ipc().sem;
            let (usz, aem) = if cmd == IPC_INFO {
                (20, limits::SEMAEM as u64)
            } else {
                let sems = t.iter().map(|(_, _, s)| s.vals.len() as u64).sum();
                (t.len() as u64, sems)
            };
            // struct seminfo: 10 ints
            let mut b = [0u8; 40];
            words(
                &mut b,
                0,
                &[
                    semmns,
                    semmni,
                    semmns,
                    semmns,
                    semmsl,
                    semopm,
                    semopm,
                    usz,
                    limits::SEMVMX as u64,
                    aem,
                ],
                4,
            );
            let max = t.max_index();
            put(cx, arg, b)?;
            Ok(max as u64)
        }
        IPC_STAT | SEM_STAT | SEM_STAT_ANY => {
            let t = &cx.ipc().sem;
            let (full, (perm, s)) =
                stat_target(t, (&cred, &owner), id, cmd != IPC_STAT, cmd == SEM_STAT_ANY)?;
            let b = encode_semid(perm, &cred, s);
            put(cx, arg, b)?;
            Ok(if cmd == IPC_STAT { 0 } else { full as u64 })
        }
        IPC_SET => {
            let b: [u8; 88] = get(cx, arg)?;
            let (perm, s) = cx.ipc_mut().sem.get_mut(id)?;
            perm.may_change(&cred, &owner)?;
            perm.set(&b[..48], &cred)?;
            s.ctime = now();
            Ok(0)
        }
        IPC_RMID => {
            let ipc = &mut cx.ipc_mut();
            ipc.sem.get(id)?.0.may_change(&cred, &owner)?;
            ipc.remove_sem(id);
            Ok(0)
        }
        GETVAL | GETPID | GETNCNT | GETZCNT => {
            let ns = cx.pid_ns();
            let (perm, s) = cx.ipc().sem.get(id)?;
            perm.check(&cred, &owner, READ)?;
            let i = usize::try_from(num)
                .ok()
                .filter(|&i| i < s.vals.len())
                .ok_or(Errno::EINVAL)?;
            Ok(match cmd {
                GETVAL => s.vals[i] as u64,
                GETPID => ns.vnr(s.pids[i]).unwrap_or(0) as u64,
                GETNCNT => s.ncnt[i] as u64,
                _ => s.zcnt[i] as u64,
            })
        }
        GETALL => {
            let (perm, s) = cx.ipc().sem.get(id)?;
            perm.check(&cred, &owner, READ)?;
            let b: Vec<u8> = s
                .vals
                .iter()
                .flat_map(|&v| (v as u16).to_le_bytes())
                .collect();
            cx.mm().write(UserSlice::new(arg, b.len()), &b)?;
            Ok(0)
        }
        SETVAL => {
            let val = arg as i32;
            if !(0..=limits::SEMVMX).contains(&val) {
                return Err(Errno::ERANGE);
            }
            let ipc = &mut cx.ipc_mut();
            let (perm, s) = ipc.sem.get_mut(id)?;
            let i = usize::try_from(num)
                .ok()
                .filter(|&i| i < s.vals.len())
                .ok_or(Errno::EINVAL)?;
            perm.check(&cred, &owner, WRITE)?;
            s.vals[i] = val;
            s.pids[i] = tgid;
            s.ctime = now();
            ipc.clear_undo(id, Some(i));
            crate::kernel::request_wake();
            Ok(0)
        }
        SETALL => {
            let n = {
                let (perm, s) = cx.ipc().sem.get(id)?;
                perm.check(&cred, &owner, WRITE)?;
                s.vals.len()
            };
            let mut b = vec![0u8; n * 2];
            cx.mm().read(UserSlice::new(arg, b.len()), &mut b)?;
            let vals: Vec<i32> = b
                .chunks(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]) as i32)
                .collect();
            if vals.iter().any(|&v| v > limits::SEMVMX) {
                return Err(Errno::ERANGE);
            }
            let ipc = &mut cx.ipc_mut();
            let s = &mut ipc.sem.get_mut(id)?.1;
            s.vals = vals;
            s.pids.fill(tgid);
            s.ctime = now();
            ipc.clear_undo(id, None);
            crate::kernel::request_wake();
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `struct semid64_ds` (88 bytes).
fn encode_semid(perm: &Perm, cred: &Cred, s: &SemSet) -> [u8; 88] {
    let mut b = [0u8; 88];
    b[..48].copy_from_slice(&perm.encode(cred));
    words(
        &mut b,
        48,
        &[s.otime as u64, s.ctime as u64, s.vals.len() as u64],
        8,
    );
    b
}

pub(super) fn semtimedop(
    cx: &mut Cx<'_>,
    id: i32,
    sops: u64,
    nsops: u64,
    timeout: u64,
) -> SysResult {
    // An unsigned int in Linux's prototype
    let nsops = nsops as u32;
    if nsops < 1 || id < 0 {
        return Err(Errno::EINVAL);
    }
    let n = nsops as usize;
    if n as u64 > cx.ipc().tunables.sem[2] {
        return Err(Errno::E2BIG);
    }
    let mut raw = crate::ipc::zeroed(n * 6)?;
    cx.mm().read(UserSlice::new(sops, raw.len()), &mut raw)?;
    let ops: Vec<SemOp> = raw
        .chunks(6)
        .map(|c| SemOp {
            num: u16::from_le_bytes([c[0], c[1]]),
            op: i16::from_le_bytes([c[2], c[3]]),
            flg: i16::from_le_bytes([c[4], c[5]]),
        })
        .collect();
    let deadline = if timeout != 0 {
        Some(std::time::Instant::now() + read_timespec(cx, timeout)?)
    } else {
        None
    };
    let cred = cx.cred();
    let owner = ipc_owner(cx);
    let tgid = cx.tgid();
    let alter = ops.iter().any(|o| o.op != 0);
    let mut waited = false;
    loop {
        let Ipc { sem, undo, .. } = &mut cx.ipc_mut();
        let (perm, set) = match sem.get_mut(id) {
            Ok(e) => e,
            Err(_) if waited => return Err(Errno::EIDRM),
            Err(e) => return Err(e),
        };
        if ops.iter().any(|o| o.num as usize >= set.vals.len()) {
            return Err(Errno::EFBIG);
        }
        perm.check(&cred, &owner, if alter { WRITE } else { READ })?;
        let adj = undo.entry((tgid, id)).or_default();
        let blocked = set.apply(&ops, adj, tgid)?;
        if adj.iter().all(|&a| a == 0) {
            undo.remove(&(tgid, id));
        }
        let Some(i) = blocked else {
            return Ok(0);
        };
        if ops[i].flg & IPC_NOWAIT as i16 != 0
            || deadline.is_some_and(|d| std::time::Instant::now() >= d)
        {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            // semop is not restarted by SA_RESTART
            return Err(Errno::EINTR);
        }
        // While waiting, count in semncnt or semzcnt
        let (num, zero) = (ops[i].num as usize, ops[i].op == 0);
        let ns = cx.task.ns.borrow().ipc.id;
        let count = |k: &mut Kernel, d: i32| {
            if let Ok((_, s)) = k.ipc_mut(ns).sem.get_mut(id) {
                let c = if zero {
                    &mut s.zcnt[num]
                } else {
                    &mut s.ncnt[num]
                };
                *c = c.saturating_add_signed(d);
            }
        };
        count(cx.k(), 1);
        waited = true;
        let r = cx.wait(deadline, &mut []);
        count(cx.k(), -1);
        r?;
    }
}

// ---- Shared memory ----

pub(super) fn shmget(cx: &mut Cx<'_>, key: i32, size: u64, flags: i32) -> SysResult {
    let cred = cx.cred();
    let owner = ipc_owner(cx);
    let tgid = cx.tgid();
    reap_shm(cx.k());
    let t = cx.ipc().tunables;
    cx.ipc_mut()
        .shm
        .get_or_create(
            key,
            flags,
            (&cred, &owner),
            t.shmmni,
            |s| {
                if size > s.size {
                    Err(Errno::EINVAL)
                } else {
                    Ok(())
                }
            },
            || {
                if !(limits::SHMMIN..=t.shmmax).contains(&size) {
                    return Err(Errno::EINVAL);
                }
                // No huge pages: EINVAL, as on a Linux without hugetlbfs compiled in
                // (hstate_sizelog in newseg is NULL). This used to be created as an ordinary
                // segment
                if flags & SHM_HUGETLB != 0 {
                    return Err(Errno::EINVAL);
                }
                Ok(ShmSeg {
                    mem: SharedMemory::new(size)?,
                    size,
                    atime: 0,
                    dtime: 0,
                    ctime: now(),
                    cpid: tgid,
                    lpid: 0,
                })
            },
        )
        .map(|id| id as u64)
}

pub(super) fn shmat(cx: &mut Cx<'_>, id: i32, addr: u64, flags: i32) -> SysResult {
    let mut addr = addr;
    if !addr.is_multiple_of(PAGE_SIZE) {
        if flags & SHM_RND == 0 {
            return Err(Errno::EINVAL);
        }
        addr &= !(PAGE_SIZE - 1);
        if addr == 0 && flags & SHM_REMAP != 0 {
            return Err(Errno::EINVAL);
        }
    }
    if addr == 0 && flags & SHM_REMAP != 0 {
        return Err(Errno::EINVAL);
    }
    let (mut want, mut prot) = if flags & SHM_RDONLY != 0 {
        (READ, prot::READ)
    } else {
        (READ | WRITE, prot::READ | prot::WRITE)
    };
    if flags & SHM_EXEC != 0 {
        want |= 0o111;
        prot |= prot::EXEC;
    }
    let cred = cx.cred();
    let owner = ipc_owner(cx);
    let mem = {
        let (perm, seg) = cx.ipc().shm.get(id)?;
        perm.check(&cred, &owner, want)?;
        seg.mem.clone()
    };
    let ns = cx.task.ns.borrow().ipc.id;
    let at = cx.mm_mut().attach_shm(
        (addr != 0).then_some(addr),
        &mem,
        prot,
        flags & SHM_REMAP != 0,
        (ns, id),
    )?;
    let tgid = cx.tgid();
    if let Ok((_, seg)) = cx.ipc_mut().shm.get_mut(id) {
        seg.atime = now();
        seg.lpid = tgid;
    }
    Ok(at)
}

pub(super) fn shmdt(cx: &mut Cx<'_>, addr: u64) -> SysResult {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    // The segment's namespace is the one at shmat time (there may have been an unshare since)
    let (ns, id) = cx.mm_mut().detach_shm(addr)?;
    let tgid = cx.tgid();
    let k = cx.k();
    if let Some((_, ipc)) = k.ipcs.get_mut(&ns)
        && let Ok((_, seg)) = ipc.shm.get_mut(id)
    {
        seg.dtime = now();
        seg.lpid = tgid;
    }
    reap_shm(k);
    Ok(0)
}

pub(super) fn shmctl(cx: &mut Cx<'_>, id: i32, cmd: i32, buf: u64) -> SysResult {
    let cmd = cmd & !IPC_64;
    let cred = cx.cred();
    let owner = ipc_owner(cx);
    reap_shm(cx.k());
    match cmd {
        IPC_INFO => {
            // struct shminfo64: 9 unsigned longs
            let t = cx.ipc().tunables;
            let mut b = [0u8; 72];
            words(
                &mut b,
                0,
                &[t.shmmax, limits::SHMMIN, t.shmmni, t.shmmni, t.shmall],
                8,
            );
            let max = cx.ipc().shm.max_index();
            put(cx, buf, b)?;
            Ok(max as u64)
        }
        SHM_INFO => {
            let t = &cx.ipc().shm;
            let pages: u64 = t.iter().map(|(_, _, s)| s.mem.len() / PAGE_SIZE).sum();
            // struct shm_info: used_ids, then shm_tot, shm_rss, shm_swp etc. (unsigned long)
            let mut b = [0u8; 48];
            words(&mut b, 0, &[t.len() as u64], 4);
            words(&mut b, 8, &[pages, pages], 8);
            let max = t.max_index();
            put(cx, buf, b)?;
            Ok(max as u64)
        }
        IPC_STAT | SHM_STAT | SHM_STAT_ANY => {
            let ns = cx.pid_ns();
            let ipc_ns = cx.task.ns.borrow().ipc.id;
            let k = cx.kr();
            let (full, (perm, s)) = stat_target(
                &k.ipc(ipc_ns).shm,
                (&cred, &owner),
                id,
                cmd != IPC_STAT,
                cmd == SHM_STAT_ANY,
            )?;
            let attached = nattch(k, (ipc_ns, full));
            let b = encode_shmid(perm, &cred, s, attached, |p| ns.vnr(p).unwrap_or(0));
            put(cx, buf, b)?;
            Ok(if cmd == IPC_STAT { 0 } else { full as u64 })
        }
        IPC_SET => {
            let b: [u8; 112] = get(cx, buf)?;
            let (perm, s) = cx.ipc_mut().shm.get_mut(id)?;
            perm.may_change(&cred, &owner)?;
            perm.set(&b[..48], &cred)?;
            s.ctime = now();
            Ok(0)
        }
        IPC_RMID => {
            let ns = cx.task.ns.borrow().ipc.id;
            let k = cx.k();
            k.ipc(ns).shm.get(id)?.0.may_change(&cred, &owner)?;
            // While mappings remain, mark it first, the key becomes private, and free it after the
            // last mapping goes away
            let attached = nattch(k, (ns, id));
            let (perm, _) = k.ipc_mut(ns).shm.get_mut(id)?;
            perm.mode |= SHM_DEST;
            perm.key = crate::ipc::IPC_PRIVATE;
            if attached == 0 {
                k.ipc_mut(ns).shm.remove(id);
            }
            Ok(0)
        }
        SHM_LOCK | SHM_UNLOCK => {
            let (perm, _) = cx.ipc_mut().shm.get_mut(id)?;
            let is_owner = cred.euid == perm.uid || cred.euid == perm.cuid;
            if !is_owner && !cred.ns_capable(&owner, cap::IPC_LOCK) {
                return Err(Errno::EPERM);
            }
            if cmd == SHM_LOCK {
                perm.mode |= SHM_LOCKED;
            } else {
                perm.mode &= !SHM_LOCKED;
            }
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `struct shmid64_ds` (112 bytes).
/// `pid` translates a recorded pid into the one the caller sees.
fn encode_shmid(
    perm: &Perm,
    cred: &Cred,
    s: &ShmSeg,
    nattch: u64,
    pid: impl Fn(Tid) -> Tid,
) -> [u8; 112] {
    let mut b = [0u8; 112];
    b[..48].copy_from_slice(&perm.encode(cred));
    words(
        &mut b,
        48,
        &[s.size, s.atime as u64, s.dtime as u64, s.ctime as u64],
        8,
    );
    words(&mut b, 80, &[pid(s.cpid) as u64, pid(s.lpid) as u64], 4);
    words(&mut b, 88, &[nattch], 8);
    b
}

// ---- Shared by msgctl, semctl and shmctl ----

/// `*_STAT` and `IPC_STAT`: finds the object by index or by id, without checking read permission
/// when `any`. Returns the id.
fn stat_target<'t, T>(
    table: &'t crate::ipc::Table<T>,
    (cred, owner): (&Cred, &UserNs),
    id: i32,
    by_index: bool,
    any: bool,
) -> Result<(i32, &'t (Perm, T)), Errno> {
    let (id, entry) = if by_index {
        table.by_index(id)?
    } else {
        (id, table.get(id)?)
    };
    if !any {
        entry.0.check(cred, owner, READ)?;
    }
    Ok((id, entry))
}

/// Concatenates a list of integers in little-endian (to encode `*_ds`, `*info`).
fn words(out: &mut [u8], at: usize, values: &[u64], width: usize) {
    for (i, v) in values.iter().enumerate() {
        let off = at + i * width;
        out[off..off + width].copy_from_slice(&v.to_le_bytes()[..width]);
    }
}

/// The user namespace owning the caller's IPC namespace, over which the SysV capabilities count
/// (`CAP_IPC_OWNER`, `CAP_SYS_ADMIN`, `CAP_IPC_LOCK`; Linux checks them with `ns_capable` on
/// `ns->user_ns`).
fn ipc_owner(cx: &Cx<'_>) -> std::rc::Rc<UserNs> {
    cx.task.ns.borrow().ipc.owner.clone()
}
