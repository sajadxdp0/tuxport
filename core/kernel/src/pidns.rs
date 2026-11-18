//! Pid namespaces (pid_namespaces(7), the kernel/ns map). Pids and tids inside the kernel are
//! always global (the numbers in the boot namespace), and every newly created namespace keeps a
//! table from global numbers to its own level's numbers: a process has one number in its own
//! namespace and one in each ancestor namespace. At the syscall and procfs boundaries numbers are
//! translated by the caller's namespace (for procfs, the mounting process's), and processes that
//! cannot be seen are ESRCH or 0. The first process in a namespace is numbered 1 and is its reaper:
//! orphans in the namespace are reparented to it, and when it exits every process in the namespace
//! is ended and no more processes can be created there.

use std::collections::HashMap;
use std::rc::Rc;

use crate::cell::RefCell;
use crate::cred::UserNs;
use crate::errno::Errno;
use crate::kernel::{PID_MAX, Tid};

/// The number of the boot pid namespace (`/proc/<pid>/ns/pid`).
pub(crate) const INIT_PID_NS: u64 = 4_026_531_836;
/// Pid namespaces nested deeper than this cannot be created (Linux's `MAX_PID_NS_LEVEL`).
pub(crate) const MAX_PID_NS_LEVEL: u32 = 32;
/// After numbers reach `PID_MAX`, the search starts again from here (Linux's `RESERVED_PIDS`).
pub(crate) const RESERVED_PIDS: Tid = 300;

/// One pid namespace. The boot namespace keeps no table: global numbers are its numbers.
#[derive(Debug)]
pub(crate) struct PidNs {
    pub id: u64,
    pub parent: Option<Rc<PidNs>>,
    /// Nesting level; the boot namespace's is 0.
    pub level: u32,
    /// Owner: the user namespace it was created in (setns needs `CAP_SYS_ADMIN` here).
    pub owner: Rc<UserNs>,
    /// Global numbers to this level's numbers, and the reverse.
    local: RefCell<HashMap<Tid, Tid>>,
    global: RefCell<HashMap<Tid, Tid>>,
    /// The next local number to try.
    next: std::cell::Cell<Tid>,
    /// The process numbered 1 (global pid).
    pub reaper: std::cell::Cell<Option<Tid>>,
    /// The reaper has exited: no more processes can be created in it (ENOMEM).
    pub dead: std::cell::Cell<bool>,
}

impl PidNs {
    /// The boot pid namespace (a new copy each time, compared by number; it keeps no table).
    pub fn init() -> Rc<PidNs> {
        Rc::new(PidNs {
            id: INIT_PID_NS,
            parent: None,
            level: 0,
            owner: UserNs::init(),
            local: Default::default(),
            global: Default::default(),
            next: std::cell::Cell::new(1),
            reaper: std::cell::Cell::new(None),
            dead: std::cell::Cell::new(false),
        })
    }

    /// A new namespace under `parent`, owned by user namespace `owner`.
    pub fn child(parent: &Rc<PidNs>, owner: Rc<UserNs>) -> Rc<PidNs> {
        Rc::new(PidNs {
            id: crate::vfs::new_ns_id(),
            parent: Some(parent.clone()),
            level: parent.level + 1,
            owner,
            local: Default::default(),
            global: Default::default(),
            next: std::cell::Cell::new(1),
            reaper: std::cell::Cell::new(None),
            dead: std::cell::Cell::new(false),
        })
    }

    /// A new thread or process `g` in namespace `ns`: allocates a number in it and in every
    /// ancestor (except the boot namespace); `set_tid` is what clone3 specified, from the level of
    /// `ns` outwards (already checked, see [`crate::kernel::Kernel::choose_pid`]). The process
    /// numbered 1 is the namespace's reaper; after the reaper exits this is ENOMEM.
    pub fn register(ns: &Rc<PidNs>, g: Tid, set_tid: &[Tid]) -> Result<(), Errno> {
        if ns.dead.get() {
            return Err(Errno::ENOMEM);
        }
        let mut done: Vec<&PidNs> = Vec::new();
        let mut cur = Some(&**ns);
        let mut want = set_tid.iter().copied();
        while let Some(n) = cur.filter(|n| n.level > 0) {
            match n.alloc(g, want.next()) {
                Ok(1) if n.reaper.get().is_none() => {
                    n.reaper.set(Some(g));
                    done.push(n);
                }
                Ok(_) => done.push(n),
                Err(e) => {
                    for d in done {
                        d.release(g);
                    }
                    return Err(e);
                }
            }
            cur = n.parent.as_deref();
        }
        Ok(())
    }

    /// Allocates a local number for global number `g`: `want` if given (EEXIST if already in use),
    /// otherwise an unused one after the last, wrapping to 300 at `PID_MAX` (Linux's
    /// `RESERVED_PIDS`, `alloc_pid`).
    fn alloc(&self, g: Tid, want: Option<Tid>) -> Result<Tid, Errno> {
        let mut global = self.global.borrow_mut();
        if let Some(l) = want {
            if global.contains_key(&l) {
                return Err(Errno::EEXIST);
            }
            global.insert(l, g);
            self.local.borrow_mut().insert(g, l);
            return Ok(l);
        }
        let mut l = self.next.get();
        for _ in 0..PID_MAX {
            if l >= PID_MAX {
                l = RESERVED_PIDS;
            }
            if let std::collections::hash_map::Entry::Vacant(e) = global.entry(l) {
                e.insert(g);
                self.local.borrow_mut().insert(g, l);
                self.next.set(l + 1);
                return Ok(l);
            }
            l += 1;
        }
        Err(Errno::EAGAIN)
    }

    /// Global number `g` is no longer used (both thread and zombie are gone): removes it from `ns`
    /// and its ancestors.
    pub fn unregister(ns: &PidNs, g: Tid) {
        let mut cur = Some(ns);
        while let Some(n) = cur.filter(|n| n.level > 0) {
            n.release(g);
            cur = n.parent.as_deref();
        }
    }

    fn release(&self, g: Tid) {
        if let Some(l) = self.local.borrow_mut().remove(&g) {
            self.global.borrow_mut().remove(&l);
        }
    }

    /// The number of global number `g` in this namespace; None if not visible here (`pid_nr_ns`).
    pub fn vnr(&self, g: Tid) -> Option<Tid> {
        if self.level == 0 {
            return Some(g);
        }
        self.local.borrow().get(&g).copied()
    }

    /// The global number for number `l` in this namespace (`find_vpid`, without checking that the
    /// process exists).
    pub fn find(&self, l: Tid) -> Option<Tid> {
        if self.level == 0 {
            return Some(l);
        }
        self.global.borrow().get(&l).copied()
    }

    /// The process with global number `g` is visible here (it is in this namespace or a
    /// descendant).
    pub fn sees(&self, g: Tid) -> bool {
        self.vnr(g).is_some()
    }

    /// Every visible global number in this namespace.
    pub fn members(&self) -> Vec<Tid> {
        self.local.borrow().keys().copied().collect()
    }

    /// `self` is `other` or one of its ancestors.
    pub fn is_ancestor_of(&self, other: &PidNs) -> bool {
        let mut cur = Some(other);
        while let Some(ns) = cur {
            if ns.id == self.id {
                return true;
            }
            cur = ns.parent.as_deref();
        }
        false
    }

    /// The namespaces from level `from` to this one, in increasing level order (one column per
    /// level, as in `NSpid` of `/proc/<pid>/status`).
    pub fn levels_from(self: &Rc<PidNs>, from: u32) -> Vec<Rc<PidNs>> {
        let mut out = Vec::new();
        let mut cur = Some(self.clone());
        while let Some(ns) = cur.filter(|ns| ns.level >= from) {
            cur = ns.parent.clone();
            out.push(ns);
        }
        out.reverse();
        out
    }

    /// Sets the last allocated local number (a write to `ns_last_pid`).
    pub fn set_last_pid(&self, pid: Tid) {
        self.next.set(pid + 1);
    }

    /// The last allocated local number (`/proc/sys/kernel/ns_last_pid`).
    pub fn last_pid(&self) -> Tid {
        self.next.get() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_namespaces() {
        let init = PidNs::init();
        let a = PidNs::child(&init, UserNs::init());
        // The first process is number 1 and becomes the reaper; the ancestor (boot) namespace uses
        // global numbers
        PidNs::register(&a, 100, &[]).unwrap();
        PidNs::register(&a, 101, &[]).unwrap();
        assert_eq!(a.reaper.get(), Some(100));
        assert_eq!((a.vnr(100), a.vnr(101), a.vnr(7)), (Some(1), Some(2), None));
        assert_eq!((a.find(2), a.find(3)), (Some(101), None));
        assert_eq!(init.vnr(101), Some(101));
        assert_eq!(a.last_pid(), 2);
        // A process in nested namespaces has one number at each level
        let b = PidNs::child(&a, UserNs::init());
        PidNs::register(&b, 102, &[]).unwrap();
        assert_eq!((b.vnr(102), a.vnr(102)), (Some(1), Some(3)));
        assert!(!b.sees(101));
        let levels: Vec<u32> = b.levels_from(1).iter().map(|n| n.level).collect();
        assert_eq!(levels, [1, 2]);
        assert_eq!(b.levels_from(0).len(), 3);
        assert!(b.levels_from(3).is_empty());
        assert!(a.is_ancestor_of(&b) && init.is_ancestor_of(&b) && !b.is_ancestor_of(&a));
        // The number is no longer used: removed at every level, and reallocation does not reuse
        // the one just freed
        PidNs::unregister(&b, 102);
        assert_eq!((b.vnr(102), a.vnr(102)), (None, None));
        PidNs::register(&a, 103, &[]).unwrap();
        assert_eq!(a.vnr(103), Some(4));
        // After a write to ns_last_pid, allocation starts after it; after pid_max, from 300
        a.set_last_pid(41);
        PidNs::register(&a, 104, &[]).unwrap();
        assert_eq!(a.vnr(104), Some(42));
        a.set_last_pid(PID_MAX);
        PidNs::register(&a, 105, &[]).unwrap();
        assert_eq!(a.vnr(105), Some(RESERVED_PIDS));
        // Numbers specified by clone3: from the inner level outwards; ones in use are EEXIST
        let c = PidNs::child(&a, UserNs::init());
        PidNs::register(&c, 107, &[1, 77]).unwrap();
        assert_eq!(
            (c.vnr(107), a.vnr(107), c.reaper.get()),
            (Some(1), Some(77), Some(107))
        );
        assert_eq!(PidNs::register(&c, 108, &[5, 77]), Err(Errno::EEXIST));
        assert_eq!((c.vnr(108), a.vnr(108)), (None, None));
        // After the reaper exits no more processes can be created in it
        a.dead.set(true);
        assert_eq!(PidNs::register(&a, 106, &[]), Err(Errno::ENOMEM));
        assert_eq!(a.vnr(106), None);
    }
}
