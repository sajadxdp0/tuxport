//! Namespaces (the kernel/ns map): `clone` and `unshare` create mount, user, pid
//! (`crate::pidns`), UTS, IPC, network, cgroup and time namespaces (`crate::nsproxy`; a new time
//! namespace is the one for children, and of the clone calls only clone3 can ask for it);
//! `/proc/<pid>/ns/*` (`crate::pseudo::NsKind`); ioctls on namespace files; setns(2) through a
//! namespace file or a pidfd. Namespace files that are open or bind-mounted elsewhere keep their
//! namespace alive ([`NsRef`]).

use std::rc::Rc;

use super::SysResult;
use crate::cred::{Cred, MAX_USER_NS_LEVEL, UserNs, cap};
use crate::errno::Errno;
use crate::fd::{FileKind, Pidfd};
use crate::kernel::{Cx, Kernel};
use crate::nsproxy::{CgroupNs, Counted, IpcNs, NetNs, NsRef, TimeNs, UtsNs};
use crate::pidns::{MAX_PID_NS_LEVEL, PidNs};
use crate::pseudo::{NsKind, PseudoNode};
use crate::task::FsContext;
use crate::vfs::{MntNs, MountId, Node, Vfs, new_ns_id};

// ---- Creating namespaces (clone, unshare) ----

/// Creates a user namespace (Linux 6.1's `create_user_ns`) and moves `cred` into it: nesting goes
/// up to level 33 at most and the user namespaces counted under the parent stay within their limits
/// (ENOSPC; older kernels give EUSERS for the level), a chrooted process may not (EPERM), and the
/// creator's effective user ID and group ID must be mapped in the old namespace (EPERM).
/// `chrooted` means the root directory is not the mount namespace's root.
pub(super) fn new_user_ns(k: &mut Kernel, cred: &mut Cred, chrooted: bool) -> Result<(), Errno> {
    let parent = cred.user_ns.clone();
    if parent.level > MAX_USER_NS_LEVEL {
        return Err(Errno::ENOSPC);
    }
    check_counts(k, &[Counted::User], &parent, cred.euid)?;
    if chrooted {
        return Err(Errno::EPERM);
    }
    if parent.uid_map.borrow().up(cred.euid).is_none()
        || parent.gid_map.borrow().up(cred.egid).is_none()
    {
        return Err(Errno::EPERM);
    }
    let setfcap = cred.cap_effective & 1 << cap::SETFCAP != 0;
    let ns = UserNs::child(&parent, cred.euid, setfcap);
    k.ucounts
        .add(Counted::User, &parent, cred.euid, ns.clone(), Some(ns.id));
    cred.enter_user_ns(ns);
    Ok(())
}

/// Whether namespaces of `kinds` that effective user `euid` creates in user namespace `owner` stay
/// within the limits of `/proc/sys/user/max_*_namespaces` ([`crate::nsproxy::Ucounts`]): ENOSPC
/// if not, before anything is created (Linux's `inc_ucount` failing in `copy_namespaces`).
pub(super) fn check_counts(
    k: &mut Kernel,
    kinds: &[Counted],
    owner: &UserNs,
    euid: u32,
) -> Result<(), Errno> {
    let init = crate::nsproxy::default_ns_limit(k.threads_max);
    for &kind in kinds {
        k.ucounts.check(kind, owner, euid, init)?;
    }
    Ok(())
}

/// Counts namespace `ns` of `kind`, created by `euid` in user namespace `owner` after
/// [`check_counts`], for as long as it lives.
pub(super) fn count(
    k: &mut Kernel,
    kind: Counted,
    owner: &UserNs,
    euid: u32,
    ns: Rc<dyn std::any::Any>,
) {
    k.ucounts.add(kind, owner, euid, ns, None);
}

/// The root directory is not the mount namespace's root (chrooted; Linux's `current_chrooted`).
pub(super) fn chrooted(cx: &Cx<'_>) -> bool {
    let Ok(vfs) = cx.vfs() else {
        return false;
    };
    vfs.ns_root(cx.mnt_ns())
        .is_some_and(|root| root != cx.root())
}

/// Creates a pid namespace (Linux 6.1's `copy_pid_ns`, `create_pid_namespace`) under `parent` (the
/// one new children are in), owned by user namespace `owner`: `parent` must be the caller's own
/// namespace (EINVAL after unshare but before entering it), `owner` must be `parent`'s owner or
/// one of its descendants (EPERM), and nesting goes up to 32 levels (ENOSPC).
pub(super) fn new_pid_ns(
    active: &PidNs,
    parent: &Rc<PidNs>,
    owner: Rc<UserNs>,
) -> Result<Rc<PidNs>, Errno> {
    if parent.id != active.id {
        return Err(Errno::EINVAL);
    }
    if !parent.owner.is_ancestor_of(&owner) {
        return Err(Errno::EPERM);
    }
    if parent.level + 1 > MAX_PID_NS_LEVEL {
        return Err(Errno::ENOSPC);
    }
    Ok(PidNs::child(parent, owner))
}

/// Gives `fs` a new mount namespace owned by user namespace `owner`: copies every mount in the old
/// one and moves the root and the current directory onto the copied mounts (Linux's
/// `copy_mnt_ns`). Without a rootfs only the number changes.
pub(super) fn new_mnt_ns(vfs: Option<&Vfs>, fs: &mut FsContext, owner: Rc<UserNs>) {
    let Some(vfs) = vfs else {
        fs.mnt_ns = Rc::new(MntNs {
            id: new_ns_id(),
            owner,
            anon: false,
        });
        return;
    };
    let (ns, map) = vfs.copy_ns(fs.mnt_ns.id, owner);
    let remap = |n: &Node| match map.get(&n.mnt()) {
        Some(&id) => n.clone().in_mount(id),
        None => n.clone(),
    };
    let (root, cwd) = (remap(&fs.root), remap(&fs.cwd));
    fs.set_root(root);
    fs.set_cwd(cwd);
    fs.mnt_ns = ns;
}

/// A new cgroup namespace owned by user namespace `owner`, rooted at the caller's cgroup `root`.
pub(super) fn new_cgroup_ns(owner: Rc<UserNs>, root: u64) -> Rc<crate::nsproxy::CgroupNs> {
    Rc::new(crate::nsproxy::CgroupNs {
        id: new_ns_id(),
        root,
        owner,
    })
}

// ---- Opening namespace files ----

/// The namespace of kind `kind` numbered `id` (when opening a namespace file): one that still has
/// threads in it (or below it), or one kept by a namespace file that is open or bind-mounted
/// elsewhere.
pub(crate) fn find(k: &Kernel, vfs: Option<&Vfs>, kind: NsKind, id: u64) -> Option<NsRef> {
    let live = match kind {
        NsKind::Mnt => vfs.and_then(|v| v.find_ns(id)).map(NsRef::Mnt),
        NsKind::User => find_user_ns(k, id).map(NsRef::User),
        NsKind::Pid | NsKind::PidForChildren => find_pid_ns(k, id).map(NsRef::Pid),
        NsKind::Uts => find_nsproxy(k, |p| (p.uts.id == id).then(|| NsRef::Uts(p.uts.clone()))),
        NsKind::Ipc => find_nsproxy(k, |p| (p.ipc.id == id).then(|| NsRef::Ipc(p.ipc.clone()))),
        NsKind::Net => find_nsproxy(k, |p| (p.net.id == id).then(|| NsRef::Net(p.net.clone()))),
        NsKind::Cgroup => find_nsproxy(k, |p| {
            (p.cgroup.id == id).then(|| NsRef::Cgroup(p.cgroup.clone()))
        }),
        NsKind::Time | NsKind::TimeForChildren => find_nsproxy(k, |p| {
            [&p.time, &p.time_for_children]
                .into_iter()
                .find(|n| n.id == id)
                .map(|n| NsRef::Time(n.clone()))
        }),
    };
    live.or_else(|| vfs.and_then(|v| v.pinned_ns(id)))
        .or_else(|| {
            // Open namespace files
            k.tasks.values().find_map(|t| {
                let res = t.res.borrow();
                let fds = res.fds.borrow();
                fds.open_fds()
                    .into_iter()
                    .filter_map(|fd| fds.get(fd).ok())
                    .find_map(|f| match &f.borrow().kind {
                        FileKind::Proc { ns: Some(r), .. } if r.id() == id => Some(r.clone()),
                        _ => None,
                    })
            })
        })
}

/// The user namespace numbered `id` that still has processes in it (or below it).
fn find_user_ns(k: &Kernel, id: u64) -> Option<Rc<UserNs>> {
    k.groups.values().find_map(|g| {
        let mut ns = Some(g.borrow().cred.user_ns.clone());
        while let Some(n) = ns {
            if n.id == id {
                return Some(n);
            }
            ns = n.parent.clone();
        }
        None
    })
}

/// The pid namespace numbered `id` that still has processes in it (or below it), or that some
/// thread uses for its children.
fn find_pid_ns(k: &Kernel, id: u64) -> Option<Rc<PidNs>> {
    let up = |ns: Rc<PidNs>| {
        let mut ns = Some(ns);
        while let Some(n) = ns {
            if n.id == id {
                return Some(n);
            }
            ns = n.parent.clone();
        }
        None
    };
    k.groups
        .values()
        .find_map(|g| up(g.borrow().pid_ns.clone()))
        .or_else(|| {
            k.tasks
                .values()
                .find_map(|t| t.pid_ns_children.borrow().clone().and_then(up))
        })
}

/// The UTS, IPC, network or cgroup namespace picked by `f` among those that still have threads.
fn find_nsproxy<T>(k: &Kernel, f: impl Fn(&crate::nsproxy::NsProxy) -> Option<T>) -> Option<T> {
    k.tasks.values().find_map(|t| f(&t.ns.borrow()))
}

// ---- setns ----

/// The namespaces setns switches to (Linux's `struct nsset`): only the one kind for a namespace
/// file, the requested kinds of that process's for a pidfd.
#[derive(Default)]
struct NsSet {
    user: Option<Rc<UserNs>>,
    mnt: Option<Rc<MntNs>>,
    uts: Option<Rc<UtsNs>>,
    ipc: Option<Rc<IpcNs>>,
    pid: Option<Rc<PidNs>>,
    cgroup: Option<Rc<CgroupNs>>,
    net: Option<Rc<NetNs>>,
    time: Option<Rc<TimeNs>>,
}

impl NsSet {
    fn one(ns: NsRef) -> NsSet {
        let mut set = NsSet::default();
        match ns {
            NsRef::Mnt(n) => set.mnt = Some(n),
            NsRef::User(n) => set.user = Some(n),
            NsRef::Pid(n) => set.pid = Some(n),
            NsRef::Uts(n) => set.uts = Some(n),
            NsRef::Ipc(n) => set.ipc = Some(n),
            NsRef::Net(n) => set.net = Some(n),
            NsRef::Cgroup(n) => set.cgroup = Some(n),
            NsRef::Time(n) => set.time = Some(n),
        }
        set
    }

    /// The kinds of namespaces in `flags` of the process the pidfd refers to (the snapshot taken at
    /// the start of Linux's `validate_nsset`): the pid namespace is the one it is in, not the one
    /// for its children. ESRCH if the process has been reaped or its main thread has exited, EPERM
    /// if it may not be read as with ptrace (`PTRACE_MODE_READ_REALCREDS`).
    fn of(cx: &Cx<'_>, p: &Pidfd, flags: u64) -> Result<NsSet, Errno> {
        let k = cx.kr();
        let group = p.group(k).ok_or(Errno::ESRCH)?;
        let task = k.tasks.get(&p.tgid).cloned().ok_or(Errno::ESRCH)?;
        let g = group.borrow();
        if !super::proc::may_ptrace(cx, g.tgid) {
            return Err(Errno::EPERM);
        }
        let want = |kind: NsKind| flags & kind.clone_flag() != 0;
        let ns = task.ns.borrow();
        Ok(NsSet {
            user: want(NsKind::User).then(|| g.cred.user_ns.clone()),
            mnt: want(NsKind::Mnt).then(|| task.res.borrow().fs.borrow().mnt_ns.clone()),
            uts: want(NsKind::Uts).then(|| ns.uts.clone()),
            ipc: want(NsKind::Ipc).then(|| ns.ipc.clone()),
            pid: want(NsKind::Pid).then(|| g.pid_ns.clone()),
            cgroup: want(NsKind::Cgroup).then(|| ns.cgroup.clone()),
            net: want(NsKind::Net).then(|| ns.net.clone()),
            time: want(NsKind::Time).then(|| ns.time.clone()),
        })
    }
}

/// setns(2) (Linux 6.1): `fd` is an open namespace file (`/proc/<pid>/ns/<kind>`, or one
/// bind-mounted elsewhere), and `nstype` is 0 or its matching `CLONE_NEW*`; or `fd` is a pidfd and
/// `nstype` is the kinds of that process's namespaces to switch to (not 0), all at once. An open
/// namespace file keeps its namespace alive, so it can be entered even after all its processes
/// have exited.
pub(super) fn setns(cx: &mut Cx<'_>, fd: i32, nstype: i32) -> SysResult {
    let file = cx.fds().get(fd)?;
    let nstype = nstype as u32 as u64;
    let (flags, set) = match &file.borrow().kind {
        FileKind::Proc {
            node: PseudoNode::NsFile(kind, _),
            ns: Some(ns),
            ..
        } => {
            if nstype != 0 && nstype != kind.clone_flag() {
                return Err(Errno::EINVAL);
            }
            (kind.clone_flag(), NsSet::one(ns.clone()))
        }
        FileKind::Pidfd(p) => {
            if nstype == 0 || nstype & !super::proc::CLONE_NEW_MASK != 0 {
                return Err(Errno::EINVAL);
            }
            (nstype, NsSet::of(cx, p, nstype)?)
        }
        _ => return Err(Errno::EINVAL),
    };
    let root = validate(cx, &set, flags)?;
    commit(cx, set, root);
    Ok(0)
}

/// Checks, in Linux's order (user, mount, UTS, IPC, pid, cgroup, network, time), whether each kind
/// in `set` may be entered (each namespace type's `install`); if any may not, none is switched.
/// `flags` is the `CLONE_NEW*` kinds to switch. Returns the mount namespace's root directory.
///
/// When a user namespace is entered at the same time, the "capability in one's own user
/// namespace" that the later kinds require is judged in the namespace being entered (Linux's
/// `nsset->cred`), while the capabilities still come from the original credentials: the user who
/// created that user namespace can enter it and its other namespaces at once without
/// `CAP_SYS_ADMIN`.
fn validate(cx: &Cx<'_>, set: &NsSet, flags: u64) -> Result<Option<Node>, Errno> {
    let cred = cx.cred();
    // Each thread's TaskState and TaskRes each hold one reference
    let fs_shared = Rc::strong_count(&cx.t.fs) > 2;
    let threaded = cx.t.group.borrow().threads.len() > 1;
    let here = set.user.clone().unwrap_or_else(|| cred.user_ns.clone());
    // Linux's utsns_install and the like: CAP_SYS_ADMIN is needed both in its owner and in one's
    // own user namespace
    let may_enter = |owner: &UserNs| {
        if cred.ns_capable(owner, cap::SYS_ADMIN) && cred.ns_capable(&here, cap::SYS_ADMIN) {
            Ok(())
        } else {
            Err(Errno::EPERM)
        }
    };
    // Linux's userns_install: one's own namespace cannot be re-entered (to regain capabilities
    // that way), not allowed for a multithreaded process or a shared fs, and CAP_SYS_ADMIN is
    // needed in the target namespace
    if let Some(ns) = &set.user {
        if ns.id == cred.user_ns.id || threaded || fs_shared {
            return Err(Errno::EINVAL);
        }
        if !cred.ns_capable(ns, cap::SYS_ADMIN) {
            return Err(Errno::EPERM);
        }
    }
    // Linux's mntns_install: first the capabilities, then whether the fs is shared with other
    // threads or processes. When only the mount namespace is switched, one's own fs is checked;
    // when others are switched too, Linux checks the temporary copy (prepare_nsset), which only
    // the caller ever uses
    let mut root = None;
    if let Some(ns) = &set.mnt {
        let vfs = cx.vfs()?;
        if !cred.ns_capable(&ns.owner, cap::SYS_ADMIN)
            || !cred.ns_capable(&here, cap::SYS_CHROOT)
            || !cred.ns_capable(&here, cap::SYS_ADMIN)
        {
            return Err(Errno::EPERM);
        }
        if fs_shared && flags == NsKind::Mnt.clone_flag() {
            return Err(Errno::EINVAL);
        }
        root = Some(vfs.ns_root(ns.id).ok_or(Errno::EINVAL)?);
    }
    if let Some(ns) = &set.uts {
        may_enter(&ns.owner)?;
    }
    if let Some(ns) = &set.ipc {
        may_enter(&ns.owner)?;
    }
    // Linux's pidns_install: only one's own namespace or a descendant of it
    if let Some(ns) = &set.pid {
        may_enter(&ns.owner)?;
        if !cx.pid_ns().is_ancestor_of(ns) {
            return Err(Errno::EINVAL);
        }
    }
    if let Some(ns) = &set.cgroup {
        may_enter(&ns.owner)?;
    }
    if let Some(ns) = &set.net {
        may_enter(&ns.owner)?;
    }
    // Linux's timens_install: not allowed for a multithreaded process (EUSERS), then capabilities
    // are checked like the others
    if let Some(ns) = &set.time {
        if threaded {
            return Err(Errno::EUSERS);
        }
        may_enter(&ns.owner)?;
    }
    Ok(root)
}

/// Switches to the namespaces in `set` (Linux's `commit_nsset`), with `root` the mount namespace's
/// root directory: the root and current directory move there; before switching the IPC namespace,
/// SEM_UNDO adjustments are applied and dropped (`exit_sem`); the pid namespace is the one later
/// children are created in (one whose process 1 has exited cannot get any, ENOMEM). Both the time
/// namespace of the caller and the one for its children become it, and its offsets can no longer
/// be changed (Linux's `timens_commit`).
fn commit(cx: &mut Cx<'_>, set: NsSet, root: Option<Node>) {
    if let Some(ns) = set.user {
        let mut cred = cx.cred();
        cred.enter_user_ns(ns);
        cx.set_cred(cred);
    }
    if let (Some(ns), Some(root)) = (set.mnt, root) {
        let mut fs = cx.t.fs.borrow_mut();
        fs.set_root(root.clone());
        fs.set_cwd(root);
        fs.mnt_ns = ns;
        drop(fs);
        cx.shared.fast_changed();
    }
    if set.ipc.is_some() {
        let tgid = cx.tgid();
        cx.ipc_mut().exit(tgid);
    }
    {
        let mut p = cx.task.ns.borrow_mut();
        if let Some(ns) = set.uts {
            p.uts = ns;
        }
        if let Some(ns) = set.cgroup {
            p.cgroup = ns;
        }
        if let Some(ns) = set.net {
            p.net = ns;
        }
    }
    if let Some(ns) = set.ipc {
        cx.task.ns.borrow_mut().ipc = ns;
        cx.k().collect_ipc();
    }
    if let Some(ns) = set.pid {
        *cx.task.pid_ns_children.borrow_mut() = Some(ns);
    }
    if let Some(ns) = set.time {
        ns.frozen.set(true);
        // With offsets, the vDSO's clock functions must enter the kernel layer, which adds them
        // (writing into the vDSO mapped at execve cannot fail)
        if ns.offsets_ns() != [0, 0] {
            let _ = cx.mm_mut().vdso_syscalls();
        }
        let mut p = cx.task.ns.borrow_mut();
        p.time = ns.clone();
        p.time_for_children = ns;
        drop(p);
        // The no-BKL clock-reading path caches the offsets
        cx.shared.fast_changed();
    }
}

// ---- ioctls on namespace files ----

const NS_GET_USERNS: u32 = 0xb701;
const NS_GET_PARENT: u32 = 0xb702;
const NS_GET_NSTYPE: u32 = 0xb703;
const NS_GET_OWNER_UID: u32 = 0xb704;

/// ioctls on namespace files (Linux 6.1's `ns_ioctl`): `NS_GET_USERNS` and `NS_GET_PARENT` open the
/// owner and the parent namespace, giving a new fd (with `FD_CLOEXEC`); `NS_GET_NSTYPE` is its
/// `CLONE_NEW*`; `NS_GET_OWNER_UID` is the user ID of the user namespace's creator in the caller's
/// user namespace. Anything else is ENOTTY (so are `NS_GET_MNTNS_ID` and the others added in 6.9:
/// Tuxport reports kernel 6.1). `mnt` is the mount the file was reached through, and `kind` is its
/// kind.
pub(super) fn ioctl(
    cx: &mut Cx<'_>,
    mnt: MountId,
    kind: NsKind,
    ns: &NsRef,
    request: u32,
    arg: u64,
) -> SysResult {
    let related = match request {
        NS_GET_NSTYPE => return Ok(kind.clone_flag()),
        NS_GET_OWNER_UID => {
            let NsRef::User(user) = ns else {
                return Err(Errno::EINVAL);
            };
            let uid = cx.cred().uid_out(user.owner);
            return super::put(cx, arg, uid.to_le_bytes()).map(|()| 0);
        }
        // A user namespace's parent is its owner
        NS_GET_USERNS | NS_GET_PARENT if matches!(ns, NsRef::User(_)) => {
            (NsKind::User, NsRef::User(visible_owner(cx, ns)?))
        }
        NS_GET_USERNS => (NsKind::User, NsRef::User(visible_owner(cx, ns)?)),
        // The parent must be the caller's pid namespace or below it (Linux's pidns_get_parent)
        NS_GET_PARENT => match ns {
            NsRef::Pid(p) => {
                let parent = p.parent.clone().ok_or(Errno::EPERM)?;
                if !cx.pid_ns().is_ancestor_of(&parent) {
                    return Err(Errno::EPERM);
                }
                (NsKind::Pid, NsRef::Pid(parent))
            }
            _ => return Err(Errno::EINVAL),
        },
        _ => return Err(Errno::ENOTTY),
    };
    let (kind, related) = related;
    let id = related.id();
    let name = format!("{}:[{id}]", kind.type_name());
    // Opened read-only as in Linux's open_related_ns (without the O_LARGEFILE that open adds)
    let file = crate::fd::OpenFile::with_path(
        FileKind::Proc {
            mnt,
            node: PseudoNode::NsFile(kind, id),
            data: Vec::new(),
            fresh: false,
            mm: None,
            ns: Some(related),
            klog: None,
            opener: Rc::new(cx.cred()),
            event: None,
        },
        crate::fd::oflags::O_RDONLY,
        Some(Rc::from(name.into_bytes())),
    );
    cx.fds_mut().install(file, true, 0).map(|fd| fd as u64)
}

/// The owner of namespace `ns`, which must be the caller's user namespace or below it (Linux's
/// `ns_get_owner`); otherwise (including the boot user namespace, which has no owner) EPERM.
fn visible_owner(cx: &Cx<'_>, ns: &NsRef) -> Result<Rc<UserNs>, Errno> {
    let owner = ns.owner().ok_or(Errno::EPERM)?;
    if !cx.cred().user_ns.is_ancestor_of(&owner) {
        return Err(Errno::EPERM);
    }
    Ok(owner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_namespace_limits() {
        let init = PidNs::init();
        let user = UserNs::init();
        // At most 32 levels: creation still works in level 31, and is ENOSPC in level 32
        let mut ns = init.clone();
        for level in 1..=MAX_PID_NS_LEVEL {
            ns = new_pid_ns(&ns, &ns, user.clone()).unwrap();
            assert_eq!(ns.level, level);
        }
        assert_eq!(
            new_pid_ns(&ns, &ns, user.clone()).unwrap_err(),
            Errno::ENOSPC
        );
        // After unshare but before entering: EINVAL when the namespace for children differs from
        // one's own
        let child = new_pid_ns(&init, &init, user.clone()).unwrap();
        assert_eq!(
            new_pid_ns(&init, &child, user.clone()).unwrap_err(),
            Errno::EINVAL
        );
        // The owner must be the parent namespace's owner or a descendant of it
        let other = UserNs::child(&user, 1000, false);
        let owned = new_pid_ns(&init, &init, other.clone()).unwrap();
        assert_eq!(new_pid_ns(&owned, &owned, user).unwrap_err(), Errno::EPERM);
        assert!(new_pid_ns(&owned, &owned, other).is_ok());
    }
}
