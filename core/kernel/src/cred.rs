//! Process credentials (credentials(7), capabilities(7)): user and group IDs, supplementary
//! groups, capability sets, and the permission checks for file access (DAC).
//!
//! On Linux credentials belong to threads, but libc's setuid family has every thread make the
//! call, so in effect the whole process shares one set; that is why credentials live in the
//! thread group here. Capabilities change with user IDs by Linux's rules: the effective set is
//! cleared when the effective user ID becomes nonzero, the permitted set when all three user IDs
//! become nonzero, and the filesystem-related capabilities follow fsuid; capset can drop some of
//! them, and `PR_CAPBSET_DROP` can shrink the bounding set. execve transforms capabilities as
//! Linux 6.1's `cap_bprm_creds_from_file` does, taking the program file's capabilities
//! (`security.capability`, [`FileCaps`]) and the ambient capabilities into account; the
//! securebits and their locks are the same as on Linux. v3 file capabilities only take effect
//! when rootid is 0.
//!
//! User namespaces ([`UserNs`]): the IDs in credentials are always kernel IDs (those of the
//! boot-time namespace) and are translated for the outside through the uid_map and gid_map of the
//! caller's namespace, with unmapped ones shown as 65534. Capabilities apply only in their own
//! namespace and its descendants ([`Cred::ns_capable`]): [`Cred::capable`] is Linux's
//! `capable()`, which looks only at the boot-time namespace; file DAC checks use
//! [`Cred::capable_wrt`] (Linux's `capable_wrt_inode_uidgid`), and the setuid family, capset and
//! the like look at the caller's own namespace ([`Cred::capable_here`]).

use std::cell::Cell;
use std::rc::Rc;

use crate::cell::RefCell;
use crate::errno::Errno;
use crate::vfs::store::{Stat, mode};

/// Capability numbers (`linux/capability.h`).
pub(crate) mod cap {
    pub const CHOWN: u32 = 0;
    pub const DAC_OVERRIDE: u32 = 1;
    pub const DAC_READ_SEARCH: u32 = 2;
    pub const FOWNER: u32 = 3;
    pub const FSETID: u32 = 4;
    pub const KILL: u32 = 5;
    pub const SETGID: u32 = 6;
    pub const SETUID: u32 = 7;
    pub const SETPCAP: u32 = 8;
    pub const LINUX_IMMUTABLE: u32 = 9;
    pub const NET_BIND_SERVICE: u32 = 10;
    pub const NET_BROADCAST: u32 = 11;
    pub const NET_ADMIN: u32 = 12;
    pub const NET_RAW: u32 = 13;
    pub const IPC_LOCK: u32 = 14;
    pub const IPC_OWNER: u32 = 15;
    pub const SYS_RAWIO: u32 = 17;
    pub const SYS_CHROOT: u32 = 18;
    pub const SYS_PTRACE: u32 = 19;
    pub const SYS_ADMIN: u32 = 21;
    pub const SYS_NICE: u32 = 23;
    pub const SYS_RESOURCE: u32 = 24;
    pub const SYS_TIME: u32 = 25;
    pub const SYS_TTY_CONFIG: u32 = 26;
    pub const MKNOD: u32 = 27;
    pub const LEASE: u32 = 28;
    pub const SETFCAP: u32 = 31;
    pub const MAC_OVERRIDE: u32 = 32;
    pub const SYSLOG: u32 = 34;
    pub const WAKE_ALARM: u32 = 35;
    pub const BPF: u32 = 39;
    pub const CHECKPOINT_RESTORE: u32 = 40;
    /// The highest capability number.
    pub const LAST: u32 = CHECKPOINT_RESTORE;
}

/// Every capability (also the default bounding set).
pub(crate) const CAP_FULL: u64 = (1 << (cap::LAST + 1)) - 1;

/// securebits (`linux/securebits.h`): each bit is followed by the bit that locks it.
pub(crate) mod secbit {
    /// execve does not grant capabilities by root's rules.
    pub const NOROOT: u32 = 1 << 0;
    /// Capabilities are not adjusted when user IDs change.
    pub const NO_SETUID_FIXUP: u32 = 1 << 2;
    /// Keep the permitted set when all user IDs become nonzero (this is what `PR_SET_KEEPCAPS`
    /// sets); cleared on execve.
    pub const KEEP_CAPS: u32 = 1 << 4;
    pub const KEEP_CAPS_LOCKED: u32 = 1 << 5;
    /// Ambient capabilities cannot be raised.
    pub const NO_CAP_AMBIENT_RAISE: u32 = 1 << 6;
    pub const ALL_BITS: u32 = 0x55;
    pub const ALL_LOCKS: u32 = 0xaa;
}

/// The filesystem capabilities that follow fsuid (`CAP_FS_MASK`).
const CAP_FS_MASK: u64 = 1 << cap::CHOWN
    | 1 << cap::DAC_OVERRIDE
    | 1 << cap::DAC_READ_SEARCH
    | 1 << cap::FOWNER
    | 1 << cap::FSETID
    | 1 << cap::LINUX_IMMUTABLE
    | 1 << cap::MKNOD
    | 1 << cap::MAC_OVERRIDE;

/// The access modes a permission check asks for.
pub(crate) const MAY_EXEC: u32 = 1;
pub(crate) const MAY_WRITE: u32 = 2;
pub(crate) const MAY_READ: u32 = 4;

/// The most supplementary groups setgroups accepts (`NGROUPS_MAX`).
pub(crate) const NGROUPS_MAX: usize = 65536;

/// The id of the boot-time user namespace (`/proc/<pid>/ns/user`).
pub(crate) const INIT_USER_NS: u64 = 4_026_531_837;
/// Unmapped IDs are shown as this (`/proc/sys/kernel/overflowuid`, `overflowgid`).
pub(crate) const OVERFLOW_ID: u32 = 65534;
/// No namespace can be created inside a user namespace whose level exceeds this (Linux's 32; the
/// deepest is level 33).
pub(crate) const MAX_USER_NS_LEVEL: u32 = 32;
/// The most extents one ID map may have (Linux's `UID_GID_MAP_MAX_EXTENTS`).
const MAX_EXTENTS: usize = 340;

// ---- Credentials ----

/// A process's credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cred {
    pub ruid: u32,
    pub euid: u32,
    pub suid: u32,
    pub fsuid: u32,
    pub rgid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub fsgid: u32,
    /// Supplementary groups, ascending.
    pub groups: Rc<[u32]>,
    pub cap_effective: u64,
    pub cap_permitted: u64,
    pub cap_inheritable: u64,
    /// Bounding set: the permitted set after execve never exceeds it.
    pub cap_bounding: u64,
    /// Ambient capabilities: always in the permitted and inheritable sets; kept on execve of a
    /// non-privileged program, going into the permitted and effective sets.
    pub cap_ambient: u64,
    /// `PR_SET_SECUREBITS` ([`secbit`]).
    pub securebits: u32,
    /// The user namespace it is in.
    pub user_ns: Rc<UserNs>,
}

impl Cred {
    /// root: all IDs 0, with every capability.
    pub fn root() -> Cred {
        Cred {
            ruid: 0,
            euid: 0,
            suid: 0,
            fsuid: 0,
            rgid: 0,
            egid: 0,
            sgid: 0,
            fsgid: 0,
            groups: Rc::from([]),
            cap_effective: CAP_FULL,
            cap_permitted: CAP_FULL,
            cap_inheritable: 0,
            cap_bounding: CAP_FULL,
            cap_ambient: 0,
            securebits: 0,
            user_ns: UserNs::init(),
        }
    }

    /// Enters user namespace `ns` (creating it or setns into it): every capability there, and
    /// securebits back to default (Linux's `set_cred_user_ns`). IDs are unchanged, and unmapped
    /// ones show as 65534 inside.
    pub fn enter_user_ns(&mut self, ns: Rc<UserNs>) {
        self.securebits = 0;
        self.cap_inheritable = 0;
        self.cap_permitted = CAP_FULL;
        self.cap_effective = CAP_FULL;
        self.cap_ambient = 0;
        self.cap_bounding = CAP_FULL;
        self.user_ns = ns;
    }

    /// setuid(2).
    pub fn setuid(&mut self, uid: u32) -> Result<(), Errno> {
        let old = self.clone();
        if self.capable_here(cap::SETUID) {
            self.ruid = uid;
            self.suid = uid;
        } else if uid != self.ruid && uid != self.suid {
            return Err(Errno::EPERM);
        }
        self.euid = uid;
        self.fsuid = uid;
        self.fix_caps(&old);
        Ok(())
    }

    /// setreuid(2): `None` means unchanged.
    pub fn setreuid(&mut self, r: Option<u32>, e: Option<u32>) -> Result<(), Errno> {
        let old = self.clone();
        let (ruid, euid) = re_ids(
            (old.ruid, old.euid, old.suid),
            r,
            e,
            self.capable_here(cap::SETUID),
        )?;
        self.ruid = ruid;
        self.euid = euid;
        if r.is_some() || e.is_some_and(|e| e != old.ruid) {
            self.suid = euid;
        }
        self.fsuid = euid;
        self.fix_caps(&old);
        Ok(())
    }

    /// setresuid(2): `None` means unchanged.
    pub fn setresuid(
        &mut self,
        r: Option<u32>,
        e: Option<u32>,
        s: Option<u32>,
    ) -> Result<(), Errno> {
        let old = self.clone();
        let known = |id: u32| id == old.ruid || id == old.euid || id == old.suid;
        if [r, e, s].into_iter().flatten().any(|id| !known(id)) && !self.capable_here(cap::SETUID) {
            return Err(Errno::EPERM);
        }
        self.ruid = r.unwrap_or(old.ruid);
        self.euid = e.unwrap_or(old.euid);
        self.suid = s.unwrap_or(old.suid);
        self.fsuid = self.euid;
        self.fix_caps(&old);
        Ok(())
    }

    /// Adjusts capabilities by Linux's rules after user IDs change (`cap_emulate_setxuid`), with
    /// root meaning root in our own namespace; no adjustment with `SECBIT_NO_SETUID_FIXUP`.
    fn fix_caps(&mut self, old: &Cred) {
        if self.securebits & secbit::NO_SETUID_FIXUP != 0 {
            return;
        }
        let root = old.root_uid();
        let is_root = |id: u32| Some(id) == root;
        let had_root = is_root(old.ruid) || is_root(old.euid) || is_root(old.suid);
        let has_root = is_root(self.ruid) || is_root(self.euid) || is_root(self.suid);
        if had_root && !has_root {
            if !self.keep_caps() {
                self.cap_permitted = 0;
                self.cap_effective = 0;
            }
            // Legacy programs have no capabilities left after setresuid to non-root and execve;
            // ambient capabilities must not defeat that
            self.cap_ambient = 0;
        }
        if is_root(old.euid) && !is_root(self.euid) {
            self.cap_effective = 0;
        }
        if !is_root(old.euid) && is_root(self.euid) {
            self.cap_effective = self.cap_permitted;
        }
    }

    /// setfsuid(2): returns the old fsuid; disallowed changes are silently ignored.
    pub fn setfsuid(&mut self, uid: Option<u32>) -> u32 {
        let old = self.fsuid;
        if let Some(uid) = uid
            && (uid == self.ruid
                || uid == self.euid
                || uid == self.suid
                || uid == self.fsuid
                || self.capable_here(cap::SETUID))
        {
            self.fsuid = uid;
            self.fix_fs_caps(old);
        }
        old
    }

    /// Adjusts the filesystem capabilities after fsuid changes; no adjustment with
    /// `SECBIT_NO_SETUID_FIXUP`.
    fn fix_fs_caps(&mut self, old_fsuid: u32) {
        if self.securebits & secbit::NO_SETUID_FIXUP != 0 {
            return;
        }
        let root = self.root_uid();
        if Some(old_fsuid) == root && Some(self.fsuid) != root {
            self.cap_effective &= !CAP_FS_MASK;
        }
        if Some(old_fsuid) != root && Some(self.fsuid) == root {
            self.cap_effective |= self.cap_permitted & CAP_FS_MASK;
        }
    }

    /// setgid(2).
    pub fn setgid(&mut self, gid: u32) -> Result<(), Errno> {
        if self.capable_here(cap::SETGID) {
            self.rgid = gid;
            self.sgid = gid;
        } else if gid != self.rgid && gid != self.sgid {
            return Err(Errno::EPERM);
        }
        self.egid = gid;
        self.fsgid = gid;
        Ok(())
    }

    /// setregid(2): `None` means unchanged.
    pub fn setregid(&mut self, r: Option<u32>, e: Option<u32>) -> Result<(), Errno> {
        let old_rgid = self.rgid;
        let (rgid, egid) = re_ids(
            (self.rgid, self.egid, self.sgid),
            r,
            e,
            self.capable_here(cap::SETGID),
        )?;
        self.rgid = rgid;
        self.egid = egid;
        if r.is_some() || e.is_some_and(|e| e != old_rgid) {
            self.sgid = egid;
        }
        self.fsgid = egid;
        Ok(())
    }

    /// setresgid(2): `None` means unchanged.
    pub fn setresgid(
        &mut self,
        r: Option<u32>,
        e: Option<u32>,
        s: Option<u32>,
    ) -> Result<(), Errno> {
        let known = |id: u32| id == self.rgid || id == self.egid || id == self.sgid;
        if [r, e, s].into_iter().flatten().any(|id| !known(id)) && !self.capable_here(cap::SETGID) {
            return Err(Errno::EPERM);
        }
        self.rgid = r.unwrap_or(self.rgid);
        self.egid = e.unwrap_or(self.egid);
        self.sgid = s.unwrap_or(self.sgid);
        self.fsgid = self.egid;
        Ok(())
    }

    /// setfsgid(2): returns the old fsgid; disallowed changes are silently ignored.
    pub fn setfsgid(&mut self, gid: Option<u32>) -> u32 {
        let old = self.fsgid;
        if let Some(gid) = gid
            && (gid == self.rgid
                || gid == self.egid
                || gid == self.sgid
                || gid == self.fsgid
                || self.capable_here(cap::SETGID))
        {
            self.fsgid = gid;
        }
        old
    }

    /// setgroups(2): allowed only when the user namespace's gid_map has been written and
    /// setgroups is not denied (`userns_may_setgroups`).
    pub fn setgroups(&mut self, mut groups: Vec<u32>) -> Result<(), Errno> {
        let ns = &self.user_ns;
        let allowed = ns.setgroups.get() && !ns.gid_map.borrow().extents().is_empty();
        if !self.capable_here(cap::SETGID) || !allowed {
            return Err(Errno::EPERM);
        }
        groups.sort_unstable();
        self.groups = Rc::from(groups);
        Ok(())
    }

    /// capset(2): the permitted set can only shrink, the effective set cannot exceed the
    /// permitted set, and the inheritable set cannot exceed the old inheritable and permitted sets
    /// (the bounding set with `CAP_SETPCAP`). Ambient capabilities not in the new permitted and
    /// inheritable sets are dropped along with them.
    pub fn capset(
        &mut self,
        effective: u64,
        permitted: u64,
        inheritable: u64,
    ) -> Result<(), Errno> {
        let inh_limit = self.cap_inheritable
            | if self.capable_here(cap::SETPCAP) {
                CAP_FULL
            } else {
                self.cap_permitted
            };
        if inheritable & !inh_limit != 0
            || inheritable & !(self.cap_inheritable | self.cap_bounding) != 0
            || permitted & !self.cap_permitted != 0
            || effective & !permitted != 0
        {
            return Err(Errno::EPERM);
        }
        self.cap_effective = effective;
        self.cap_permitted = permitted;
        self.cap_inheritable = inheritable;
        self.cap_ambient &= permitted & inheritable;
        Ok(())
    }

    /// `PR_CAP_AMBIENT_RAISE`: the capability must be in both the permitted and the inheritable
    /// set, without `SECBIT_NO_CAP_AMBIENT_RAISE`.
    pub fn raise_ambient(&mut self, c: u32) -> Result<(), Errno> {
        let bit = 1 << c;
        if self.cap_permitted & self.cap_inheritable & bit == 0
            || self.securebits & secbit::NO_CAP_AMBIENT_RAISE != 0
        {
            return Err(Errno::EPERM);
        }
        self.cap_ambient |= bit;
        Ok(())
    }

    /// `PR_SET_SECUREBITS` (`cap_task_prctl`): needs `CAP_SETPCAP`; locked bits cannot change,
    /// locks cannot be released, and only the bits in [`secbit`] are recognized.
    pub fn set_securebits(&mut self, bits: u32) -> Result<(), Errno> {
        let old = self.securebits;
        let locked = (old & secbit::ALL_LOCKS) >> 1;
        if locked & (old ^ bits) != 0
            || old & secbit::ALL_LOCKS & !bits != 0
            || bits & !(secbit::ALL_LOCKS | secbit::ALL_BITS) != 0
            || !self.capable_here(cap::SETPCAP)
        {
            return Err(Errno::EPERM);
        }
        self.securebits = bits;
        Ok(())
    }

    /// The credentials after execve (Linux's `bprm_fill_uid`, `cap_bprm_creds_from_file`).
    /// `setuid` and `setgid` are the program file's setuid owner and setgid group (the caller
    /// passes neither under `no_new_privs`), and `fcaps` its file capabilities; under
    /// `no_new_privs`, or when the execution is unsafe (`unsafe_exec`, Linux's `bprm->unsafe`:
    /// traced by a tracer without `CAP_SYS_PTRACE`, or sharing the fs context with another
    /// process), no capabilities that were not held before can be gained, and the set-id
    /// effective IDs are kept only with `CAP_SETUID` (never under `no_new_privs`).
    ///
    /// - The permitted set is (bounding & file permitted) | (inheritable & file inheritable) |
    ///   ambient; when the effective or real user ID is 0 (without `SECBIT_NOROOT`) both file sets
    ///   count as all ones, and when the effective user ID is 0 the effective bit is set too. A
    ///   setuid-root program with file capabilities run by non-root does not get the root rules.
    /// - The effective set equals the permitted set when the file's effective bit is set,
    ///   otherwise the ambient capabilities.
    /// - File capabilities or setuid/setgid clear the ambient capabilities.
    ///
    /// Returns EPERM when the file asks for the effective bit and its permitted set but cannot get
    /// all of it (a program that cannot get the capabilities it wants had better not run). On
    /// success, returns whether this is a privileged execution (`AT_SECURE`): setuid, setgid, or
    /// non-root gaining capabilities beyond the ambient ones.
    pub fn exec(
        &mut self,
        setuid: Option<u32>,
        setgid: Option<u32>,
        fcaps: Option<FileCaps>,
        no_new_privs: bool,
        unsafe_exec: bool,
    ) -> Result<bool, Errno> {
        let old = self.clone();
        let mut new = self.clone();
        if let Some(uid) = setuid {
            new.euid = uid;
        }
        if let Some(gid) = setgid {
            new.egid = gid;
        }
        let mut effective = false;
        new.cap_permitted = 0;
        if let Some(f) = fcaps {
            effective = f.effective;
            new.cap_permitted =
                old.cap_bounding & f.permitted | old.cap_inheritable & f.inheritable;
            if effective && f.permitted & !new.cap_permitted != 0 {
                return Err(Errno::EPERM);
            }
        }
        // root (in our own namespace): the file's permitted and inheritable sets count as all
        // ones (handle_privileged_root)
        let root = old.root_uid();
        let is_root = |id: u32| Some(id) == root;
        let setuid_root = !is_root(new.ruid) && is_root(new.euid);
        if old.securebits & secbit::NOROOT == 0 && !(fcaps.is_some() && setuid_root) {
            if is_root(new.euid) || is_root(new.ruid) {
                new.cap_permitted = old.cap_bounding | old.cap_inheritable;
            }
            if is_root(new.euid) {
                effective = true;
            }
        }
        // As in Linux's __is_setuid, the new effective IDs are compared with the old real IDs
        let setid = new.euid != old.ruid || new.egid != old.rgid;
        let gained = new.cap_permitted & !old.cap_permitted != 0;
        if (setid || gained) && (no_new_privs || unsafe_exec) {
            if !old.capable_here(cap::SETUID) || no_new_privs {
                new.euid = new.ruid;
                new.egid = new.rgid;
            }
            new.cap_permitted &= old.cap_permitted;
        }
        new.suid = new.euid;
        new.fsuid = new.euid;
        new.sgid = new.egid;
        new.fsgid = new.egid;
        if fcaps.is_some() || setid {
            new.cap_ambient = 0;
        }
        new.cap_permitted |= new.cap_ambient;
        new.cap_effective = if effective {
            new.cap_permitted
        } else {
            new.cap_ambient
        };
        new.securebits &= !secbit::KEEP_CAPS;
        let secure =
            setid || !is_root(new.ruid) && (effective || new.cap_permitted & !new.cap_ambient != 0);
        *self = new;
        Ok(secure)
    }

    /// Is the file's owner, or has `CAP_FOWNER` with the owner mapped in our namespace (chmod,
    /// setting timestamps and so on: Linux's `inode_owner_or_capable`, which unlike
    /// [`capable_wrt`](Self::capable_wrt) does not need the group mapped).
    pub fn owns(&self, st: &Stat) -> bool {
        self.fsuid == st.uid
            || self.capable_here(cap::FOWNER) && self.user_ns.uid_map.borrow().up(st.uid).is_some()
    }

    /// Permission check for file access (`generic_permission`): `mask` is a combination of
    /// `MAY_*`.
    pub fn permission(&self, st: &Stat, mask: u32) -> Result<(), Errno> {
        let class = if self.fsuid == st.uid {
            st.mode >> 6
        } else if self.in_group(st.gid) {
            st.mode >> 3
        } else {
            st.mode
        };
        if mask & !class & 7 == 0 {
            return Ok(());
        }
        if st.kind() == mode::S_IFDIR {
            // Directory permissions can always be overridden by capabilities
            if mask & MAY_WRITE == 0 && self.capable_wrt(st, cap::DAC_READ_SEARCH)
                || self.capable_wrt(st, cap::DAC_OVERRIDE)
            {
                return Ok(());
            }
        } else {
            if mask == MAY_READ && self.capable_wrt(st, cap::DAC_READ_SEARCH) {
                return Ok(());
            }
            // Execute permission can only be overridden when at least one x bit is set
            if (mask & MAY_EXEC == 0 || st.mode & 0o111 != 0)
                && self.capable_wrt(st, cap::DAC_OVERRIDE)
            {
                return Ok(());
            }
        }
        Err(Errno::EACCES)
    }

    /// Whether the search permission checks of every directory can be bypassed (the
    /// path-resolution fast path). In other user namespaces only directories whose owner and group
    /// are mapped can be bypassed, so that does not count.
    pub fn searches_all(&self) -> bool {
        self.capable(cap::DAC_READ_SEARCH) || self.capable(cap::DAC_OVERRIDE)
    }

    /// Deleting or renaming `victim` in directory `dir`: the directory must be writable and
    /// searchable, and the sticky bit is honoured.
    pub fn may_delete(&self, dir: &Stat, victim: &Stat) -> Result<(), Errno> {
        self.permission(dir, MAY_WRITE | MAY_EXEC)?;
        if dir.mode & mode::S_ISVTX != 0
            && self.fsuid != victim.uid
            && self.fsuid != dir.uid
            && !self.capable_wrt(victim, cap::FOWNER)
        {
            return Err(Errno::EPERM);
        }
        Ok(())
    }

    /// The credentials access(2) uses: checks with the real user and group IDs, and capabilities
    /// depend on whether the real user ID is root (in our own namespace).
    pub fn for_access(&self) -> Cred {
        let mut c = self.clone();
        c.fsuid = self.ruid;
        c.fsgid = self.rgid;
        c.cap_effective = if Some(self.ruid) == self.root_uid() {
            self.cap_permitted
        } else {
            0
        };
        c
    }

    /// The owner, group and permission bits of a new file made with `mode` under `umask` (Linux
    /// 6.1's `vfs_prepare_mode` and `inode_init_owner`). In a setgid directory the group is the
    /// directory's. A caller outside that group without `CAP_FSETID` cannot make a group-executable
    /// setgid file: the bit goes before the umask is applied (`mode_strip_sgid`), so a umask that
    /// removes group execute does not save it. A directory keeps only its permissions and the sticky
    /// bit, and inherits the setgid bit of a setgid parent.
    pub fn new_owner(&self, dir: &Stat, mode: u32, umask: u32, is_dir: bool) -> (u32, u32, u32) {
        let mut perm = mode & 0o7777;
        let setgid_dir = dir.mode & mode::S_ISGID != 0;
        let gid = if setgid_dir { dir.gid } else { self.fsgid };
        if !is_dir
            && setgid_dir
            && perm & (mode::S_ISGID | 0o010) == mode::S_ISGID | 0o010
            && !self.in_group(dir.gid)
            && !self.capable_wrt(dir, cap::FSETID)
        {
            perm &= !mode::S_ISGID;
        }
        perm &= !umask;
        if is_dir {
            perm &= 0o1777;
            if setgid_dir {
                perm |= mode::S_ISGID;
            }
        }
        (self.fsuid, gid, perm)
    }

    /// Whether `gid` is the fsgid or among the supplementary groups.
    pub fn in_group(&self, gid: u32) -> bool {
        self.fsgid == gid || self.groups.binary_search(&gid).is_ok()
    }

    /// Whether another process with credentials `target` may be accessed the way ptrace does
    /// (Linux's `__ptrace_may_access` plus `cap_ptrace_access_check`): the real user and group
    /// (the filesystem ones with `fscreds`) equal all of the target's users and groups and the
    /// target is dumpable, or there is `CAP_SYS_PTRACE` in the target's user namespace; then both
    /// are in the same user namespace and the target's permitted set does not exceed our own
    /// permitted set (our effective set with `fscreds`), or there is that `CAP_SYS_PTRACE`.
    ///
    /// A target that is not dumpable is reachable only with `CAP_SYS_PTRACE` in `mm_ns`, the
    /// user namespace of its address space (Linux's `mm->user_ns`; the target's own when None).
    pub fn may_ptrace(
        &self,
        target: &Cred,
        (dumpable, mm_ns): (bool, Option<&UserNs>),
        fscreds: bool,
    ) -> bool {
        let (uid, gid, caps) = if fscreds {
            (self.fsuid, self.fsgid, self.cap_effective)
        } else {
            (self.ruid, self.rgid, self.cap_permitted)
        };
        let same = [target.ruid, target.euid, target.suid]
            .iter()
            .all(|&u| u == uid)
            && [target.rgid, target.egid, target.sgid]
                .iter()
                .all(|&g| g == gid);
        let privileged = self.ns_capable(&target.user_ns, cap::SYS_PTRACE);
        let subset = self.user_ns.id == target.user_ns.id && target.cap_permitted & !caps == 0;
        // __ptrace_may_access: the IDs or the capability, then dumpable or the capability over the
        // address space's namespace, then cap_ptrace_access_check
        let reachable =
            dumpable || self.ns_capable(mm_ns.unwrap_or(&target.user_ns), cap::SYS_PTRACE);
        (same || privileged) && reachable && (subset || privileged)
    }

    /// Whether a process with credentials `target` may be signalled (`kill_ok_by_cred`): the
    /// sender's real or effective user ID equals the target's real or saved user ID, or there is
    /// `CAP_KILL` in the target's user namespace.
    pub fn may_signal(&self, target: &Cred) -> bool {
        [self.euid, self.ruid]
            .iter()
            .any(|&u| u == target.suid || u == target.ruid)
            || self.ns_capable(&target.user_ns, cap::KILL)
    }

    /// The user ID of kernel ID `k` in our own namespace, 65534 when unmapped
    /// (`from_kuid_munged`).
    pub fn uid_out(&self, k: u32) -> u32 {
        self.user_ns.uid_map.borrow().up(k).unwrap_or(OVERFLOW_ID)
    }

    /// The group ID of kernel ID `k` in our own namespace, 65534 when unmapped.
    pub fn gid_out(&self, k: u32) -> u32 {
        self.user_ns.gid_map.borrow().up(k).unwrap_or(OVERFLOW_ID)
    }

    /// The kernel ID for user ID `id` in our own namespace (`make_kuid`); None when unmapped.
    pub fn uid_in(&self, id: u32) -> Option<u32> {
        self.user_ns.uid_map.borrow().down(id)
    }

    /// The kernel ID for group ID `id` in our own namespace; None when unmapped.
    pub fn gid_in(&self, id: u32) -> Option<u32> {
        self.user_ns.gid_map.borrow().down(id)
    }

    /// The kernel ID of root (user ID 0) in our own namespace; None when unmapped.
    fn root_uid(&self) -> Option<u32> {
        self.uid_in(0)
    }

    /// `SECBIT_KEEP_CAPS`.
    pub fn keep_caps(&self) -> bool {
        self.securebits & secbit::KEEP_CAPS != 0
    }

    /// Whether there is capability `c` in the boot-time user namespace (Linux's `capable()`):
    /// processes in other namespaces never have it.
    pub fn capable(&self, c: u32) -> bool {
        self.user_ns.level == 0 && self.capable_here(c)
    }

    /// Whether there is capability `c` in our own user namespace (whether it is in the effective
    /// set, `ns_capable(current_user_ns(), c)`).
    pub fn capable_here(&self, c: u32) -> bool {
        self.cap_effective & (1 << c) != 0
    }

    /// Whether there is capability `c` in user namespace `ns` (Linux's `cap_capable`): when `ns`
    /// is our own namespace, the effective set decides; when it is a descendant of our own
    /// namespace, a process that created it (or one of its ancestors) and whose effective user ID
    /// equals its owner has every capability; otherwise there is none.
    pub fn ns_capable(&self, ns: &UserNs, c: u32) -> bool {
        let mut ns = ns;
        loop {
            if ns.id == self.user_ns.id {
                return self.capable_here(c);
            }
            if ns.level <= self.user_ns.level {
                return false;
            }
            let Some(parent) = &ns.parent else {
                return false;
            };
            if parent.id == self.user_ns.id && ns.owner == self.euid {
                return true;
            }
            ns = parent;
        }
    }

    /// Whether kernel user ID `root` is the root of our user namespace or of one of its ancestors
    /// (Linux's `rootid_owns_currentns`): file capabilities with that root apply to us.
    pub fn root_owns(&self, root: u32) -> bool {
        let mut ns = Some(&self.user_ns);
        while let Some(n) = ns {
            if n.uid_map.borrow().up(root) == Some(0) {
                return true;
            }
            ns = n.parent.as_ref();
        }
        false
    }

    /// Whether there is capability `c` over a file whose owner and group are `st`'s (Linux's
    /// `capable_wrt_inode_uidgid`): it is held in our own namespace, and the owner and group are
    /// both mapped in it.
    pub fn capable_wrt(&self, st: &Stat, c: u32) -> bool {
        self.capable_here(c)
            && self.user_ns.uid_map.borrow().up(st.uid).is_some()
            && self.user_ns.gid_map.borrow().up(st.gid).is_some()
    }
}

/// The new real and effective IDs for setreuid and setregid: without privilege the real ID can
/// only change to the old real or effective ID, and the effective ID only to one of the old three.
fn re_ids(
    (real, eff, saved): (u32, u32, u32),
    r: Option<u32>,
    e: Option<u32>,
    privileged: bool,
) -> Result<(u32, u32), Errno> {
    let r_ok = r.is_none_or(|r| r == real || r == eff);
    let e_ok = e.is_none_or(|e| e == real || e == eff || e == saved);
    if !(privileged || r_ok && e_ok) {
        return Err(Errno::EPERM);
    }
    Ok((r.unwrap_or(real), e.unwrap_or(eff)))
}

// ---- File capabilities ----

/// The extended attribute name of program file capabilities.
pub(crate) const XATTR_NAME_CAPS: &[u8] = b"security.capability";

const VFS_CAP_REVISION_MASK: u32 = 0xff00_0000;
const VFS_CAP_REVISION_1: u32 = 0x0100_0000;
const VFS_CAP_REVISION_2: u32 = 0x0200_0000;
const VFS_CAP_REVISION_3: u32 = 0x0300_0000;
const VFS_CAP_FLAGS_EFFECTIVE: u32 = 1;
/// The lengths of the three versions: after magic_etc come one (v1) or two pairs of permitted and
/// inheritable, and v3 adds rootid.
const XATTR_CAPS_SZ_1: usize = 12;
const XATTR_CAPS_SZ_2: usize = 20;
const XATTR_CAPS_SZ_3: usize = 24;

/// A program file's capabilities (the `struct vfs_cap_data` in `security.capability`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileCaps {
    pub permitted: u64,
    pub inheritable: u64,
    /// The effective bit: after execve the effective set equals the permitted set.
    pub effective: bool,
}

impl FileCaps {
    /// Parses the extended attribute of the program being execve'd by `cred` as Linux's
    /// `get_vfs_caps_from_disk` does: a length or version mismatch is EINVAL (execve fails), and
    /// capabilities whose root (kernel uid 0 for v1 and v2, the rootid for v3) does not own the
    /// caller's user namespace ([`Cred::root_owns`]) give none. Bits beyond [`CAP_FULL`] are
    /// dropped.
    pub fn parse(v: &[u8], cred: &Cred) -> Result<Option<FileCaps>, Errno> {
        if v.len() < 4 {
            return Err(Errno::EINVAL);
        }
        let magic = le32(v, 0);
        let (words, root) = match (magic & VFS_CAP_REVISION_MASK, v.len()) {
            (VFS_CAP_REVISION_1, XATTR_CAPS_SZ_1) => (1, 0),
            (VFS_CAP_REVISION_2, XATTR_CAPS_SZ_2) => (2, 0),
            (VFS_CAP_REVISION_3, XATTR_CAPS_SZ_3) => (2, le32(v, 20)),
            _ => return Err(Errno::EINVAL),
        };
        if !cred.root_owns(root) {
            return Ok(None);
        }
        let mut caps = FileCaps {
            effective: magic & VFS_CAP_FLAGS_EFFECTIVE != 0,
            ..FileCaps::default()
        };
        for i in 0..words {
            caps.permitted |= u64::from(le32(v, 4 + 8 * i)) << (32 * i);
            caps.inheritable |= u64::from(le32(v, 8 + 8 * i)) << (32 * i);
        }
        caps.permitted &= CAP_FULL;
        caps.inheritable &= CAP_FULL;
        Ok(Some(caps))
    }

    /// Whether setxattr may write this value (`validheader` in `cap_convert_nscap`): only v2 and
    /// v3 are accepted.
    pub fn valid_xattr(v: &[u8]) -> bool {
        let revision = |r| v.len() >= 4 && le32(v, 0) & VFS_CAP_REVISION_MASK == r;
        v.len() == XATTR_CAPS_SZ_2 && revision(VFS_CAP_REVISION_2)
            || v.len() == XATTR_CAPS_SZ_3 && revision(VFS_CAP_REVISION_3)
    }

    /// The value setxattr stores for a valid `v` ([`FileCaps::valid_xattr`]) written by `cred`
    /// (Linux's `cap_convert_nscap`, after its `CAP_SETFCAP` check): a v2 written by a holder of
    /// `CAP_SETFCAP` in the initial namespace (the store's) is kept; anything else becomes a v3
    /// whose rootid is the kernel ID of the root the value names in the writer's namespace (uid 0
    /// for v2, its rootid for v3), so the capabilities apply only in namespaces that root owns.
    /// EINVAL when that root is not mapped.
    pub fn xattr_to_store(v: &[u8], cred: &Cred) -> Result<Vec<u8>, Errno> {
        if v.len() == XATTR_CAPS_SZ_2 && cred.capable(cap::SETFCAP) {
            return Ok(v.to_vec());
        }
        let root = if v.len() == XATTR_CAPS_SZ_3 {
            le32(v, 20)
        } else {
            0
        };
        let root = cred.uid_in(root).ok_or(Errno::EINVAL)?;
        Ok(Self::v3(v, root))
    }

    /// The value getxattr reads (`cap_inode_getsecurity`) for `cred`: when the capabilities' root
    /// (uid 0 for v2, the rootid for v3) is mapped in the reader's namespace to an ID other than 0,
    /// a v3 with that rootid; otherwise, when that root owns the reader's namespace, a v2; else
    /// EOVERFLOW. A stored value that is neither v2 nor v3 (a zero-length value can be written
    /// without checks) is EINVAL.
    pub fn xattr_for_reader(v: &[u8], cred: &Cred) -> Result<Vec<u8>, Errno> {
        let revision = if v.len() >= 4 {
            le32(v, 0) & VFS_CAP_REVISION_MASK
        } else {
            0
        };
        let root = match (revision, v.len()) {
            (VFS_CAP_REVISION_2, XATTR_CAPS_SZ_2) => 0,
            (VFS_CAP_REVISION_3, XATTR_CAPS_SZ_3) => le32(v, 20),
            _ => return Err(Errno::EINVAL),
        };
        match cred.user_ns.uid_map.borrow().up(root) {
            Some(mapped) if mapped != 0 => return Ok(Self::v3(v, mapped)),
            _ => {}
        }
        if !cred.root_owns(root) {
            return Err(Errno::EOVERFLOW);
        }
        let magic = VFS_CAP_REVISION_2 | le32(v, 0) & VFS_CAP_FLAGS_EFFECTIVE;
        let mut out = magic.to_le_bytes().to_vec();
        out.extend_from_slice(&v[4..XATTR_CAPS_SZ_2]);
        Ok(out)
    }

    /// `v` (v2 or v3) as a v3 with rootid `root`, keeping the effective flag and the sets.
    fn v3(v: &[u8], root: u32) -> Vec<u8> {
        let magic = VFS_CAP_REVISION_3 | le32(v, 0) & VFS_CAP_FLAGS_EFFECTIVE;
        let mut out = magic.to_le_bytes().to_vec();
        out.extend_from_slice(&v[4..XATTR_CAPS_SZ_2]);
        out.extend_from_slice(&root.to_le_bytes());
        out
    }
}

fn le32(v: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(v[at..at + 4].try_into().unwrap())
}

// ---- User namespaces ----

/// An ID map (uid_map, gid_map): each extent is (first ID in the namespace, first kernel ID,
/// count).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct IdMap(Vec<(u32, u32, u32)>);

impl IdMap {
    /// The boot-time namespace: every ID maps to itself (Linux's `0 0 4294967295`).
    fn identity() -> IdMap {
        IdMap(vec![(0, 0, u32::MAX)])
    }

    /// Parses what is written to uid_map or gid_map (the parsing part of Linux's `map_write`):
    /// each line is "first ID in the namespace, first ID in the parent, count"; the count cannot
    /// be 0, extents cannot overflow or overlap, and there are at most 340 of them. The second
    /// column is still an ID in the parent namespace; the caller converts it to a kernel ID.
    pub fn parse(data: &[u8]) -> Result<IdMap, Errno> {
        let text = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
        let mut extents: Vec<(u32, u32, u32)> = Vec::new();
        for line in text.split('\n').filter(|l| !l.is_empty()) {
            let nums: Vec<u32> = line
                .split_ascii_whitespace()
                .map(|n| n.parse().map_err(|_| Errno::EINVAL))
                .collect::<Result<_, _>>()?;
            let &[first, lower, count] = nums.as_slice() else {
                return Err(Errno::EINVAL);
            };
            let fits = |start: u32| {
                count > 0 && start.checked_add(count - 1).is_some_and(|e| e != u32::MAX)
            };
            if !fits(first) || !fits(lower) {
                return Err(Errno::EINVAL);
            }
            let overlaps = |&(f, l, c): &(u32, u32, u32)| {
                first < f + c && f < first + count || lower < l + c && l < lower + count
            };
            if extents.iter().any(overlaps) || extents.len() == MAX_EXTENTS {
                return Err(Errno::EINVAL);
            }
            extents.push((first, lower, count));
        }
        if extents.is_empty() {
            return Err(Errno::EINVAL);
        }
        Ok(IdMap(extents))
    }

    /// Converts each extent's second column from an ID in the parent namespace (whose map is
    /// `parent`) to a kernel ID; any unmapped one is EPERM. One extent may correspond to several
    /// in the parent, and is split along the parent's extents.
    pub fn lower_to_kernel(&self, parent: &IdMap) -> Result<IdMap, Errno> {
        let mut out = Vec::new();
        for &(first, lower, count) in &self.0 {
            let mut done = 0;
            while done < count {
                let id = lower + done;
                let &(pf, pl, pc) = parent
                    .0
                    .iter()
                    .find(|&&(f, _, c)| id >= f && id - f < c)
                    .ok_or(Errno::EPERM)?;
                let n = (count - done).min(pc - (id - pf));
                out.push((first + done, pl + (id - pf), n));
                done += n;
            }
        }
        Ok(IdMap(out))
    }

    /// The kernel ID for `id` in the namespace (`make_kuid`).
    pub fn down(&self, id: u32) -> Option<u32> {
        self.0
            .iter()
            .find(|&&(first, _, count)| id >= first && id - first < count)
            .map(|&(first, lower, _)| lower + (id - first))
    }

    /// The ID in the namespace of kernel ID `k` (`from_kuid`).
    pub fn up(&self, k: u32) -> Option<u32> {
        self.0
            .iter()
            .find(|&&(_, lower, count)| k >= lower && k - lower < count)
            .map(|&(first, lower, _)| first + (k - lower))
    }

    /// The extents.
    pub fn extents(&self) -> &[(u32, u32, u32)] {
        &self.0
    }
}

/// A user namespace (user_namespaces(7)). In the boot-time one every ID maps to itself; a new one
/// starts with no mappings, which `/proc/<pid>/uid_map` and `gid_map` write once.
#[derive(Debug)]
pub(crate) struct UserNs {
    /// The id (the inode number in `/proc/<pid>/ns/user`).
    pub id: u64,
    pub parent: Option<Rc<UserNs>>,
    /// Nesting level, 0 for the boot-time namespace.
    pub level: u32,
    /// The creator's effective user ID (a kernel ID): it has every capability over this namespace
    /// in the parent namespace.
    pub owner: u32,
    pub uid_map: RefCell<IdMap>,
    pub gid_map: RefCell<IdMap>,
    /// `/proc/<pid>/setgroups` is "allow": after it is written as "deny", setgroups is not allowed
    /// in this namespace.
    pub setgroups: Cell<bool>,
    /// The creator had `CAP_SETFCAP` in its effective set (Linux's `parent_could_setfcap`): only
    /// then may a process inside map the parent's uid 0 into it.
    pub parent_could_setfcap: bool,
}

impl UserNs {
    /// This namespace or its nearest ancestor where `uid` and `gid` (kernel IDs) are both mapped
    /// (Linux's `privileged_wrt_inode_uidgid` walk in `would_dump`); the initial namespace at the
    /// latest.
    pub fn holding(self: &Rc<UserNs>, uid: u32, gid: u32) -> Rc<UserNs> {
        let mut ns = self.clone();
        while let Some(parent) = ns.parent.clone() {
            if ns.uid_map.borrow().up(uid).is_some() && ns.gid_map.borrow().up(gid).is_some() {
                break;
            }
            ns = parent;
        }
        ns
    }

    /// The boot-time user namespace (a new copy each time, compared by id).
    pub fn init() -> Rc<UserNs> {
        Rc::new(UserNs {
            id: INIT_USER_NS,
            parent: None,
            level: 0,
            owner: 0,
            uid_map: RefCell::new(IdMap::identity()),
            gid_map: RefCell::new(IdMap::identity()),
            setgroups: Cell::new(true),
            parent_could_setfcap: true,
        })
    }

    /// A new namespace under `parent` whose creator's effective user ID is `owner` and who had
    /// `CAP_SETFCAP` in its effective set or not (`could_setfcap`). The setgroups state is
    /// inherited from the parent.
    pub fn child(parent: &Rc<UserNs>, owner: u32, could_setfcap: bool) -> Rc<UserNs> {
        Rc::new(UserNs {
            id: crate::vfs::new_ns_id(),
            parent: Some(parent.clone()),
            level: parent.level + 1,
            owner,
            uid_map: RefCell::new(IdMap::default()),
            gid_map: RefCell::new(IdMap::default()),
            setgroups: Cell::new(parent.setgroups.get()),
            parent_could_setfcap: could_setfcap,
        })
    }

    /// `self` is `other` or one of its ancestors (Linux's `in_userns(self, other)`).
    pub fn is_ancestor_of(&self, other: &UserNs) -> bool {
        let mut ns = other;
        while ns.level > self.level {
            match &ns.parent {
                Some(p) => ns = p,
                None => return false,
            }
        }
        ns.id == self.id
    }
}

impl PartialEq for UserNs {
    fn eq(&self, other: &UserNs) -> bool {
        self.id == other.id
    }
}

impl Eq for UserNs {}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(perm: u32, uid: u32, gid: u32) -> Stat {
        Stat {
            ino: 1,
            mode: mode::S_IFREG | perm,
            nlink: 1,
            uid,
            gid,
            rdev: 0,
            size: 0,
            atime_ns: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            blocks: None,
        }
    }

    fn user(uid: u32, gid: u32) -> Cred {
        let mut c = Cred::root();
        c.setresgid(Some(gid), Some(gid), Some(gid)).unwrap();
        c.setresuid(Some(uid), Some(uid), Some(uid)).unwrap();
        c
    }

    #[test]
    fn user_namespaces() {
        // Parsing maps: three columns, nonzero counts, no overlap, no overflow
        assert_eq!(
            IdMap::parse(b"0 1000 1\n").unwrap().extents(),
            &[(0, 1000, 1)]
        );
        assert_eq!(
            IdMap::parse(b"0 0 1\n1 100000 65536").unwrap().extents(),
            &[(0, 0, 1), (1, 100_000, 65536)]
        );
        for bad in [
            &b""[..],
            b"0 0",
            b"0 0 0",
            b"0 0 1\n0 5 1",
            b"x 0 1",
            b"4294967295 0 1",
        ] {
            assert_eq!(IdMap::parse(bad), Err(Errno::EINVAL), "{bad:?}");
        }
        // The second column is converted to kernel IDs through the parent's map, split across
        // extents
        let parent = IdMap(vec![(0, 100_000, 10), (10, 200_000, 10)]);
        let m = IdMap::parse(b"0 5 10")
            .unwrap()
            .lower_to_kernel(&parent)
            .unwrap();
        assert_eq!(m.extents(), &[(0, 100_005, 5), (5, 200_000, 5)]);
        assert_eq!(m.down(6), Some(200_001));
        assert_eq!(m.up(100_007), Some(2));
        assert_eq!(
            IdMap::parse(b"0 15 10").unwrap().lower_to_kernel(&parent),
            Err(Errno::EPERM)
        );

        // A namespace created by uid 1000: every capability inside, none in the boot-time
        // namespace; IDs show as 65534 before the maps are written
        let user = Cred {
            ruid: 1000,
            euid: 1000,
            suid: 1000,
            fsuid: 1000,
            cap_effective: 0,
            cap_permitted: 0,
            ..Cred::root()
        };
        let init = user.user_ns.clone();
        let mut inside = user.clone();
        inside.enter_user_ns(UserNs::child(&init, 1000, false));
        let ns = inside.user_ns.clone();
        assert!(inside.capable_here(cap::SYS_ADMIN));
        assert!(!inside.capable(cap::SYS_ADMIN));
        assert!(inside.ns_capable(&ns, cap::SYS_ADMIN));
        assert!(!inside.ns_capable(&init, cap::SYS_ADMIN));
        assert_eq!(inside.uid_out(1000), OVERFLOW_ID);
        // The creator (same effective user ID) has every capability over it in the parent, and
        // so does root through its capabilities in the parent; other users have none
        assert!(user.ns_capable(&ns, cap::SYS_ADMIN));
        assert!(Cred::root().ns_capable(&ns, cap::SYS_ADMIN));
        let other = Cred {
            euid: 2000,
            ..user.clone()
        };
        assert!(!other.ns_capable(&ns, cap::SYS_ADMIN));
        // Map 0 -> 1000 written: root inside; DAC capabilities apply only to mapped files
        *ns.uid_map.borrow_mut() = IdMap(vec![(0, 1000, 1)]);
        *ns.gid_map.borrow_mut() = IdMap(vec![(0, 0, 1)]);
        assert_eq!(inside.uid_out(1000), 0);
        assert_eq!(inside.uid_in(0), Some(1000));
        assert_eq!(inside.uid_in(1), None);
        assert!(inside.permission(&file(0o600, 1000, 0), MAY_READ).is_ok());
        assert_eq!(
            inside.permission(&file(0o600, 0, 0), MAY_READ),
            Err(Errno::EACCES)
        );
        // After switching to an unmapped user (the syscall layer translates setuid's arguments)
        // it is no longer root, and capabilities are cleared accordingly
        let mut c = inside.clone();
        c.setresuid(None, Some(5000), None).unwrap();
        assert!(!c.capable_here(cap::SYS_ADMIN));
        c.setresuid(None, Some(1000), None).unwrap();
        assert!(c.capable_here(cap::SYS_ADMIN));
        // Executing a program: root inside keeps its capabilities
        let mut e = inside.clone();
        e.exec(None, None, None, false, false).unwrap();
        assert_eq!(e.cap_effective, CAP_FULL);
    }

    #[test]
    fn permission_uses_the_owner_group_or_other_class() {
        let u = user(1000, 100);
        assert_eq!(u.cap_effective, 0);
        assert_eq!(u.cap_permitted, 0);
        // The owner looks only at the owner bits, even when the other bits allow
        assert_eq!(
            u.permission(&file(0o077, 1000, 0), MAY_READ),
            Err(Errno::EACCES)
        );
        assert!(u.permission(&file(0o640, 0, 100), MAY_READ).is_ok());
        assert_eq!(
            u.permission(&file(0o640, 0, 100), MAY_WRITE),
            Err(Errno::EACCES)
        );
        assert!(u.permission(&file(0o604, 0, 0), MAY_READ).is_ok());
        let root = Cred::root();
        assert!(
            root.permission(&file(0o000, 1000, 100), MAY_READ | MAY_WRITE)
                .is_ok()
        );
        // root also needs at least one x bit to execute a file
        assert_eq!(
            root.permission(&file(0o666, 0, 0), MAY_EXEC),
            Err(Errno::EACCES)
        );
        assert!(root.permission(&file(0o100, 1000, 0), MAY_EXEC).is_ok());
        let mut dir = file(0o000, 1000, 100);
        dir.mode = mode::S_IFDIR;
        assert!(root.permission(&dir, MAY_EXEC | MAY_WRITE).is_ok());
    }

    #[test]
    fn dropping_root_follows_the_capability_rules() {
        // seteuid to an ordinary user: the effective set is cleared, the permitted set kept, and
        // it can become root again
        let mut c = Cred::root();
        c.setreuid(None, Some(1000)).unwrap();
        assert_eq!((c.ruid, c.euid, c.suid, c.fsuid), (0, 1000, 1000, 1000));
        assert!(!c.capable(cap::DAC_OVERRIDE));
        assert_eq!(c.cap_permitted, CAP_FULL);
        c.setresuid(None, Some(0), None).unwrap();
        assert!(c.capable(cap::SETUID));
        // After setuid all three IDs are the ordinary user, and there is no way back
        c.setuid(1000).unwrap();
        assert_eq!((c.ruid, c.euid, c.suid), (1000, 1000, 1000));
        assert_eq!(c.cap_permitted, 0);
        assert_eq!(c.setuid(0), Err(Errno::EPERM));
        assert_eq!(c.setresuid(Some(0), None, None), Err(Errno::EPERM));
        // Repeating the same change always succeeds (libc has every thread make the call)
        c.setuid(1000).unwrap();
        // fsuid can only change to one of the existing IDs
        assert_eq!(c.setfsuid(Some(0)), 1000);
        assert_eq!(c.fsuid, 1000);
        assert_eq!(c.setfsuid(None), 1000);
    }

    #[test]
    fn fsuid_controls_the_filesystem_capabilities() {
        let mut c = Cred::root();
        assert_eq!(c.setfsuid(Some(1000)), 0);
        assert!(!c.capable(cap::DAC_OVERRIDE));
        assert!(c.capable(cap::KILL));
        c.setfsuid(Some(0));
        assert!(c.capable(cap::DAC_OVERRIDE));
    }

    #[test]
    fn exec_recomputes_capabilities() {
        let mut c = user(1000, 100);
        assert_eq!(c.exec(Some(0), None, None, false, false), Ok(true));
        assert_eq!((c.ruid, c.euid, c.suid), (1000, 0, 0));
        assert_eq!(c.cap_effective, CAP_FULL);
        let mut c = user(1000, 100);
        assert_eq!(c.exec(None, None, None, false, false), Ok(false));
        assert_eq!(c.cap_permitted, 0);
        let mut c = Cred::root();
        assert_eq!(c.exec(None, None, None, false, false), Ok(false));
        assert_eq!(c.cap_effective, CAP_FULL);
        let mut c = Cred::root();
        c.setreuid(None, Some(1000)).unwrap();
        assert_eq!(c.exec(None, None, None, false, false), Ok(true));
        assert_eq!(c.cap_permitted, CAP_FULL);
        assert_eq!(c.cap_effective, 0);
    }

    const NET_RAW: u64 = 1 << 13;
    const NET_BIND: u64 = 1 << cap::NET_BIND_SERVICE;

    fn caps(permitted: u64, inheritable: u64, effective: bool) -> Option<FileCaps> {
        Some(FileCaps {
            permitted,
            inheritable,
            effective,
        })
    }

    #[test]
    fn file_capabilities_grant_to_other_users() {
        // cap_net_raw+ep: in both the permitted and effective sets, a privileged execution
        let mut c = user(1000, 100);
        let r = c.exec(None, None, caps(NET_RAW, 0, true), false, false);
        assert_eq!(r, Ok(true));
        assert_eq!((c.cap_permitted, c.cap_effective), (NET_RAW, NET_RAW));
        // cap_net_raw+p: only into the permitted set, still a privileged execution
        let mut c = user(1000, 100);
        assert_eq!(
            c.exec(None, None, caps(NET_RAW, 0, false), false, false),
            Ok(true)
        );
        assert_eq!((c.cap_permitted, c.cap_effective), (NET_RAW, 0));
        // The file's inheritable set grants only what is in the process's inheritable set
        let mut c = user(1000, 100);
        c.cap_inheritable = NET_BIND;
        c.exec(None, None, caps(0, NET_BIND | NET_RAW, true), false, false)
            .unwrap();
        assert_eq!(c.cap_permitted, NET_BIND);
        // Not in the bounding set but the effective bit is required: EPERM, credentials unchanged
        let mut c = user(1000, 100);
        c.cap_bounding &= !NET_RAW;
        let before = c.clone();
        assert_eq!(
            c.exec(None, None, caps(NET_RAW, 0, true), false, false),
            Err(Errno::EPERM)
        );
        assert_eq!(c, before);
        // no_new_privs, an unsafe execution (an unprivileged tracer, a shared fs context): no new
        // capabilities, and a setuid program keeps the caller's effective ID
        for (nnp, unsafe_exec) in [(true, false), (false, true)] {
            let mut c = user(1000, 100);
            c.exec(None, None, caps(NET_RAW, 0, true), nnp, unsafe_exec)
                .unwrap();
            assert_eq!(c.cap_permitted, 0);
            let mut c = user(1000, 100);
            c.exec(Some(0), None, None, nnp, unsafe_exec).unwrap();
            assert_eq!((c.euid, c.cap_permitted), (1000, 0));
        }
        // root still gets every capability; a setuid-root program with file capabilities run by
        // non-root gets only the file's
        let mut c = Cred::root();
        c.exec(None, None, caps(NET_RAW, 0, true), false, false)
            .unwrap();
        assert_eq!(c.cap_permitted, CAP_FULL);
        let mut c = user(1000, 100);
        c.exec(Some(0), None, caps(NET_RAW, 0, true), false, false)
            .unwrap();
        assert_eq!((c.euid, c.cap_permitted), (0, NET_RAW));
        // SECBIT_NOROOT: root too gets only the file's
        let mut c = Cred::root();
        c.set_securebits(secbit::NOROOT).unwrap();
        c.exec(None, None, None, false, false).unwrap();
        assert_eq!(c.cap_permitted, 0);
    }

    #[test]
    fn ambient_capabilities_survive_exec_of_ordinary_programs() {
        let mut c = Cred::root();
        c.set_securebits(secbit::KEEP_CAPS).unwrap();
        c.setresuid(Some(1000), Some(1000), Some(1000)).unwrap();
        c.capset(0, NET_RAW | NET_BIND, NET_RAW | NET_BIND).unwrap();
        assert_eq!(c.raise_ambient(cap::NET_ADMIN), Err(Errno::EPERM));
        c.raise_ambient(13).unwrap();
        let mut plain = c.clone();
        assert_eq!(plain.exec(None, None, None, false, false), Ok(false));
        assert_eq!(
            (plain.cap_permitted, plain.cap_effective, plain.cap_ambient),
            (NET_RAW, NET_RAW, NET_RAW)
        );
        // File capabilities and setuid clear the ambient capabilities
        let mut f = c.clone();
        f.exec(None, None, caps(NET_BIND, 0, true), false, false)
            .unwrap();
        assert_eq!((f.cap_permitted, f.cap_ambient), (NET_BIND, 0));
        let mut s = c.clone();
        s.exec(None, Some(50), None, false, false).unwrap();
        assert_eq!(s.cap_ambient, 0);
        // capset dropping them from the inheritable set drops the ambient ones too; cleared when
        // all user IDs become nonzero
        let mut d = c.clone();
        d.capset(0, NET_RAW | NET_BIND, NET_BIND).unwrap();
        assert_eq!(d.cap_ambient, 0);
        let mut r = Cred::root();
        r.cap_inheritable = NET_RAW;
        r.raise_ambient(13).unwrap();
        r.set_securebits(secbit::KEEP_CAPS).unwrap();
        r.setuid(1000).unwrap();
        assert_eq!((r.cap_permitted, r.cap_ambient), (CAP_FULL, 0));
        // SECBIT_NO_CAP_AMBIENT_RAISE
        let mut n = c.clone();
        n.securebits |= secbit::NO_CAP_AMBIENT_RAISE;
        assert_eq!(n.raise_ambient(13), Err(Errno::EPERM));
    }

    #[test]
    fn securebits_locks_cannot_be_undone() {
        let mut c = Cred::root();
        c.set_securebits(secbit::NOROOT | secbit::KEEP_CAPS_LOCKED)
            .unwrap();
        // Locked bits cannot change, locks cannot be released, unknown bits are refused
        assert_eq!(
            c.set_securebits(secbit::NOROOT | secbit::KEEP_CAPS | secbit::KEEP_CAPS_LOCKED),
            Err(Errno::EPERM)
        );
        assert_eq!(c.set_securebits(secbit::NOROOT), Err(Errno::EPERM));
        assert_eq!(
            c.set_securebits(0x100 | secbit::KEEP_CAPS_LOCKED),
            Err(Errno::EPERM)
        );
        c.set_securebits(secbit::KEEP_CAPS_LOCKED).unwrap();
        let mut u = user(1000, 100);
        assert_eq!(u.set_securebits(0), Err(Errno::EPERM));
        // SECBIT_NO_SETUID_FIXUP: capabilities are kept even after becoming non-root
        let mut f = Cred::root();
        f.set_securebits(secbit::NO_SETUID_FIXUP).unwrap();
        f.setresuid(Some(1000), Some(1000), Some(1000)).unwrap();
        assert_eq!(f.cap_effective, CAP_FULL);
    }

    #[test]
    fn file_capability_xattrs() {
        let v2 = |magic: u32, p: u32, i: u32| -> Vec<u8> {
            [magic, p, i, 1, 0]
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect()
        };
        let root = Cred::root();
        let parsed = FileCaps::parse(&v2(0x0200_0001, 1 << 13, 0), &root)
            .unwrap()
            .unwrap();
        assert_eq!(parsed.permitted, NET_RAW | 1 << 32);
        assert!(parsed.effective);
        // v1 has only the low 32 bits; a wrong length or version is EINVAL; a v3 whose rootid is
        // not the root of the caller's namespace (or an ancestor's) gives no capabilities
        let v1: Vec<u8> = [0x0100_0000u32, 1, 0]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        assert_eq!(FileCaps::parse(&v1, &root).unwrap().unwrap().permitted, 1);
        assert_eq!(FileCaps::parse(&v1[..8], &root), Err(Errno::EINVAL));
        assert_eq!(
            FileCaps::parse(&v2(0x0400_0000, 0, 0), &root),
            Err(Errno::EINVAL)
        );
        let mut v3 = v2(0x0300_0001, 1, 0);
        v3.extend_from_slice(&1000u32.to_le_bytes());
        assert_eq!(FileCaps::parse(&v3, &root), Ok(None));
        assert!(FileCaps::valid_xattr(&v3) && !FileCaps::valid_xattr(&v1));
        // Read from the initial namespace, rootid 1000 is mapped there to itself: v3 as stored
        assert_eq!(FileCaps::xattr_for_reader(&v3, &root).unwrap(), v3);
        // A v3 with rootid 0 reads back as v2
        v3[20..].copy_from_slice(&0u32.to_le_bytes());
        let one = FileCaps {
            permitted: 1 | 1 << 32,
            inheritable: 0,
            effective: true,
        };
        assert_eq!(FileCaps::parse(&v3, &root), Ok(Some(one)));
        assert_eq!(
            FileCaps::xattr_for_reader(&v3, &root).unwrap(),
            v2(0x0200_0001, 1, 0)
        );
        assert_eq!(FileCaps::xattr_for_reader(&[], &root), Err(Errno::EINVAL));
        // Written by root of the initial namespace, a v2 stays v2
        assert_eq!(
            FileCaps::xattr_to_store(&v2(0x0200_0001, 1, 0), &root).unwrap(),
            v2(0x0200_0001, 1, 0)
        );
    }

    #[test]
    fn sticky_directories_protect_other_users_files() {
        let u = user(1000, 100);
        let mut tmp = file(0o1777, 0, 0);
        tmp.mode = mode::S_IFDIR | 0o1777;
        assert!(u.may_delete(&tmp, &file(0o644, 1000, 100)).is_ok());
        assert_eq!(
            u.may_delete(&tmp, &file(0o666, 2000, 100)),
            Err(Errno::EPERM)
        );
        assert!(
            Cred::root()
                .may_delete(&tmp, &file(0o644, 2000, 100))
                .is_ok()
        );
    }

    #[test]
    fn new_files_inherit_the_group_of_setgid_directories() {
        let u = user(1000, 100);
        let mut dir = file(0o2775, 0, 50);
        dir.mode = mode::S_IFDIR | 0o2775;
        assert_eq!(u.new_owner(&dir, 0o755, 0o022, true), (1000, 50, 0o2755));
        // Not in group 50: a new setgid executable loses the setgid bit, even when the umask then
        // removes group execute
        assert_eq!(u.new_owner(&dir, 0o2755, 0o022, false), (1000, 50, 0o755));
        assert_eq!(u.new_owner(&dir, 0o2755, 0o077, false), (1000, 50, 0o700));
        assert_eq!(u.new_owner(&dir, 0o2755, 0o010, false), (1000, 50, 0o745));
        assert_eq!(
            u.new_owner(&file(0o755, 0, 0), 0o644, 0o022, false),
            (1000, 100, 0o644)
        );
        // mkdir keeps only the permissions and the sticky bit
        assert_eq!(
            u.new_owner(&file(0o755, 0, 0), 0o7777, 0o022, true),
            (1000, 100, 0o1755)
        );
        assert_eq!(u.new_owner(&dir, 0o6777, 0, true), (1000, 50, 0o2777));
    }
}
