//! Processes and threads (the kernel/process map): clone, exit, wait4, process groups and
//! sessions; execve (the kernel/exec map) and unshare (the kernel/ns map).

use std::cell::Cell;
use std::rc::Rc;

use tuxport_engine::Cpu;

use super::{Flow, SysResult, put, read_path};
use crate::cell::RefCell;
use crate::cred::{Cred, FileCaps, XATTR_NAME_CAPS, cap};
use crate::errno::Errno;
use crate::fd::{FileKind, FileRef, OpenFile, Pidfd, oflags};
use crate::kernel::{Cx, Tid};
use crate::loader::{self, Args, LoadError};
use crate::mm::Mm;
use crate::nsproxy::Counted;
use crate::process::{self, ExitStatus, Signal, ThreadGroup};
use crate::signal::{self, AltStack, SigInfo, code};
use crate::task::{self, FsContext, Task, TaskState};
use crate::uaccess::UserSlice;
use crate::vfs::store::{Ino, mode};
use crate::vfs::{Node, Pseudo, Vfs, normalize};

const CLONE_VM: u64 = 0x100;
const CLONE_FS: u64 = 0x200;
const CLONE_FILES: u64 = 0x400;
const CLONE_SIGHAND: u64 = 0x800;
const CLONE_PIDFD: u64 = 0x1000;
const CLONE_VFORK: u64 = 0x4000;
const CLONE_PARENT: u64 = 0x8000;
const CLONE_THREAD: u64 = 0x10000;
const CLONE_SETTLS: u64 = 0x80000;
const CLONE_PARENT_SETTID: u64 = 0x100000;
const CLONE_CHILD_CLEARTID: u64 = 0x200000;
const CLONE_DETACHED: u64 = 0x400000;
const CLONE_CHILD_SETTID: u64 = 0x1000000;
const CLONE_SYSVSEM: u64 = 0x40000;
const CLONE_NEWNS: u64 = 0x0002_0000;
const CLONE_NEWUSER: u64 = 0x1000_0000;
const CLONE_NEWPID: u64 = 0x2000_0000;
const CLONE_NEWUTS: u64 = 0x0400_0000;
const CLONE_NEWIPC: u64 = 0x0800_0000;
const CLONE_NEWNET: u64 = 0x4000_0000;
const CLONE_NEWCGROUP: u64 = 0x0200_0000;
/// Among the exit-signal bits of clone, only unshare and clone3 can use it.
const CLONE_NEWTIME: u64 = 0x80;
/// All the `CLONE_NEW*` flags: mount, user, pid, UTS, IPC, network, cgroup and time namespaces
/// (`syscall::ns`).
pub(super) const CLONE_NEW_MASK: u64 = CLONE_NEWTIME | CLONE_NEWNS | 0x7e00_0000;
/// The supported `CLONE_NEW*` flags.
const CLONE_NEW_DONE: u64 = CLONE_NEWNS
    | CLONE_NEWUSER
    | CLONE_NEWPID
    | CLONE_NEWUTS
    | CLONE_NEWIPC
    | CLONE_NEWNET
    | CLONE_NEWCGROUP
    | CLONE_NEWTIME;
/// The `CLONE_NEW*` flags that need `CAP_SYS_ADMIN` in the new process's user namespace (Linux's
/// copy_namespaces).
const CLONE_NEW_ADMIN: u64 = CLONE_NEWNS
    | CLONE_NEWPID
    | CLONE_NEWUTS
    | CLONE_NEWIPC
    | CLONE_NEWNET
    | CLONE_NEWCGROUP
    | CLONE_NEWTIME;
/// clone3: the child's signal handlers are reset to default.
const CLONE_CLEAR_SIGHAND: u64 = 0x1_0000_0000;
/// clone3: the child is put into the cgroup given by `cgroup`.
const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;
/// Low 8 bits: clone's exit signal.
const CSIGNAL: u64 = 0xff;

const WNOHANG: u32 = 1;
const WUNTRACED: u32 = 2;
const WEXITED: u32 = 4;
const WCONTINUED: u32 = 8;
const WNOWAIT: u32 = 0x0100_0000;
const WNOTHREAD: u32 = 0x2000_0000;
const WALL: u32 = 0x4000_0000;
const WCLONE: u32 = 0x8000_0000;

const P_ALL: u32 = 0;
const P_PID: u32 = 1;
const P_PGID: u32 = 2;
const P_PIDFD: u32 = 3;

const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
const AT_EMPTY_PATH: u32 = 0x1000;

/// The maximum length of one argument (Linux's `MAX_ARG_STRLEN`).
const MAX_ARG_STRLEN: usize = 32 * 4096;
/// `#!` interpreters nest at most this many levels (five scripts, then the binary, as Linux
/// 6.1's `exec_binprm` allows); more is ELOOP.
const MAX_INTERP_DEPTH: usize = 5;
/// `#!` lines are read up to this many bytes (Linux's `BINPRM_BUF_SIZE`).
const SHEBANG_MAX: usize = 256;

// ---- clone ----

/// Arguments of clone and clone3 (Linux's `struct kernel_clone_args`).
struct CloneArgs {
    flags: u64,
    /// The signal sent to the parent when the child exits; 0 means none.
    exit_signal: i32,
    /// The child thread's stack top; 0 means keep the parent thread's sp.
    stack: u64,
    ptid: u64,
    tls: u64,
    ctid: u64,
    /// `CLONE_PIDFD`: where the new process's pidfd is written.
    pidfd: u64,
    /// The new process's numbers in each pid-namespace level given by clone3, from its own level
    /// outwards.
    set_tid: Vec<Tid>,
    /// clone3's `CLONE_INTO_CGROUP`: the cgroup the new process is put into.
    cgroup: Option<u64>,
}

pub(super) fn clone(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    flags: u64,
    stack: u64,
    ptid: u64,
    tls: u64,
    ctid: u64,
) -> SysResult {
    // Only the low 32 bits count; the pidfd is written at parent_tid, so CLONE_PARENT_SETTID
    // cannot be requested as well
    let flags = flags & 0xffff_ffff;
    if flags & CLONE_PIDFD != 0 && flags & CLONE_PARENT_SETTID != 0 {
        return Err(Errno::EINVAL);
    }
    let args = CloneArgs {
        flags: flags & !CSIGNAL,
        exit_signal: (flags & CSIGNAL) as i32,
        stack,
        ptid,
        tls,
        ctid,
        pidfd: ptid,
        set_tid: Vec::new(),
        cgroup: None,
    };
    do_clone(cx, cpu, args)
}

/// clone3: the arguments are in a `struct clone_args`, which adds the pidfd, the stack size and
/// set_tid to clone's.
pub(super) fn clone3(cx: &mut Cx<'_>, cpu: &mut Cpu, uargs: u64, size: usize) -> SysResult {
    // CLONE_ARGS_SIZE_VER0 and the largest known size (VER2)
    const MIN: usize = 64;
    const KNOWN: usize = 88;
    if size < MIN {
        return Err(Errno::EINVAL);
    }
    if size > tuxport_engine::PAGE_SIZE as usize {
        return Err(Errno::E2BIG);
    }
    let mut b = vec![0u8; size];
    cx.mm().read(UserSlice::new(uargs, size), &mut b)?;
    // Programs built for newer kernels pass a longer structure: the extra part must be zero
    if b.get(KNOWN..)
        .is_some_and(|rest| rest.iter().any(|&x| x != 0))
    {
        return Err(Errno::E2BIG);
    }
    b.resize(KNOWN, 0);
    let w = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    let (flags, pidfd, ctid, ptid, exit_signal) = (w(0), w(1), w(2), w(3), w(4));
    let (stack, stack_size, tls, set_tid, set_tid_size) = (w(5), w(6), w(7), w(8), w(9));
    let known = 0xffff_ffff | CLONE_CLEAR_SIGHAND | CLONE_INTO_CGROUP;
    let invalid = flags & !known != 0
        || flags & (CLONE_DETACHED | (CSIGNAL & !0x80)) != 0
        || flags & CLONE_SIGHAND != 0 && flags & CLONE_CLEAR_SIGHAND != 0
        || flags & (CLONE_THREAD | CLONE_PARENT) != 0 && exit_signal != 0
        || exit_signal > Signal::MAX as u64
        || (stack == 0) != (stack_size == 0)
        || set_tid_size > crate::pidns::MAX_PID_NS_LEVEL as u64
        || (set_tid == 0) != (set_tid_size == 0);
    if invalid {
        return Err(Errno::EINVAL);
    }
    // CLONE_INTO_CGROUP: the cgroup field is an fd of a cgroup2 directory (O_PATH is fine too),
    // and the structure must contain that field
    let cgroup = if flags & CLONE_INTO_CGROUP != 0 {
        if size < KNOWN {
            return Err(Errno::EINVAL);
        }
        let file = cx.fds().get(w(10) as i32).map_err(|_| Errno::EBADF)?;
        let node = file.borrow().node();
        match node {
            Some(Node::Pseudo(_, crate::pseudo::PseudoNode::Cgroup(id, b""))) => Some(id),
            _ => return Err(Errno::EBADF),
        }
    } else {
        None
    };
    // Pids given for each level (used by CRIU; whether they may be given is checked in
    // Kernel::choose_pid)
    let mut buf = vec![0u8; set_tid_size as usize * 4];
    cx.mm().read(UserSlice::new(set_tid, buf.len()), &mut buf)?;
    let set_tid: Vec<Tid> = buf
        .chunks_exact(4)
        .map(|b| Tid::from_le_bytes(b.try_into().unwrap()))
        .collect();
    if flags & CLONE_PIDFD != 0 && flags & CLONE_PARENT_SETTID != 0 && pidfd == ptid {
        return Err(Errno::EINVAL);
    }
    let args = CloneArgs {
        flags,
        exit_signal: exit_signal as i32,
        stack: if stack != 0 {
            stack.wrapping_add(stack_size)
        } else {
            0
        },
        ptid,
        tls,
        ctid,
        pidfd,
        set_tid,
        cgroup,
    };
    do_clone(cx, cpu, args)
}

fn do_clone(cx: &mut Cx<'_>, cpu: &mut Cpu, args: CloneArgs) -> SysResult {
    let CloneArgs {
        flags,
        stack,
        ptid,
        tls,
        ctid,
        ..
    } = args;
    let both = |a: u64, b: u64| flags & (a | b) == a | b;
    if both(CLONE_NEWNS, CLONE_FS)
        || both(CLONE_NEWUSER, CLONE_FS)
        || both(CLONE_NEWUSER, CLONE_THREAD)
        || (flags & CLONE_THREAD != 0 && flags & CLONE_SIGHAND == 0)
        || (flags & CLONE_SIGHAND != 0 && flags & CLONE_VM == 0)
    {
        return Err(Errno::EINVAL);
    }
    // A pid namespace's process 1 cannot create siblings of itself; a thread must be in the same
    // pid namespace as its creator
    let active = cx.pid_ns();
    let for_children = cx.pid_ns_children();
    let thread = flags & CLONE_THREAD != 0;
    if flags & CLONE_PARENT != 0 && active.reaper.get() == Some(cx.tgid())
        || thread && (flags & CLONE_NEWPID != 0 || for_children.id != active.id)
        || (flags & CLONE_PIDFD != 0 && flags & (CLONE_DETACHED | CLONE_THREAD) != 0)
    {
        return Err(Errno::EINVAL);
    }
    if flags & CLONE_NEW_MASK & !CLONE_NEW_DONE != 0 {
        return Err(Errno::EPERM);
    }
    // The new user namespace is created first; the other namespaces belong to it and are checked
    // against its capabilities (Linux's copy_creds comes before copy_namespaces)
    let mut child_cred = cx.cred();
    let euid = child_cred.euid;
    if flags & CLONE_NEWUSER != 0 {
        let chrooted = super::ns::chrooted(cx);
        super::ns::new_user_ns(cx.k(), &mut child_cred, chrooted)?;
    }
    if flags & CLONE_NEW_ADMIN != 0 && !child_cred.capable_here(cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    super::ns::check_counts(cx.k(), &counted(flags), &child_cred.user_ns, euid)?;
    // A new IPC namespace cannot keep sharing the old SEM_UNDO adjustments
    if both(CLONE_NEWIPC, CLONE_SYSVSEM) {
        return Err(Errno::EINVAL);
    }
    // CLONE_INTO_CGROUP (Linux's cgroup_css_set_fork): the destination is checked as for a write
    // to cgroup.procs; a thread cannot be put alone into another domain cgroup (EOPNOTSUPP)
    if let Some(dst) = args.cgroup {
        let src = cx.t.group.borrow().cgroup;
        let ns_root = cx.ns().cgroup.root;
        crate::cgroup::may_enter(cx.kr(), &cx.cred(), (src, dst), ns_root)?;
        if thread && dst != src {
            return Err(Errno::EOPNOTSUPP);
        }
    }
    let pid_ns = if flags & CLONE_NEWPID != 0 {
        let ns = super::ns::new_pid_ns(&active, &for_children, child_cred.user_ns.clone())?;
        super::ns::count(cx.k(), Counted::Pid, &child_cred.user_ns, euid, ns.clone());
        ns
    } else {
        for_children
    };
    // The new UTS and IPC namespaces belong to the new process's user namespace
    let child_cred_owner = child_cred.user_ns.clone();
    // Thread-count limits (Linux's copy_process): RLIMIT_NPROC counts threads per real user ID,
    // and root and holders of CAP_SYS_RESOURCE or CAP_SYS_ADMIN are exempt; threads-max applies to
    // everyone
    {
        let cred = cx.cred();
        let nproc = cx.t.group.borrow().rlimits[process::RLIMIT_NPROC as usize].0;
        let k = cx.kr();
        if cred.ruid != 0
            && !cred.capable(cap::SYS_RESOURCE)
            && !cred.capable(cap::SYS_ADMIN)
            && user_threads(k, cred.ruid) >= nproc
        {
            return Err(Errno::EAGAIN);
        }
        if k.tasks.len() >= k.threads_max {
            return Err(Errno::EAGAIN);
        }
    }
    // The cgroup's pids.max (Linux's cgroup_can_fork): threads count too
    let dst = args.cgroup.unwrap_or(cx.t.group.borrow().cgroup);
    crate::cgroup::pids_charge(cx.k(), dst)?;
    // A child put into a frozen cgroup stops from the start (Linux's cgroup_post_fork)
    let frozen = cx.kr().cgroups.freezing(dst);
    let cred = cx.cred();
    let tid = cx.k().choose_pid(&pid_ns, &cred, &args.set_tid)?;

    let mm = if flags & CLONE_VM != 0 {
        cx.t.mm.clone()
    } else {
        // First stop the other threads in the same address space, so the copy is a snapshot of
        // the moment of fork
        cx.mm().quiesce(&cx.task.sync);
        cx.mm_mut().fork()?
    };
    let fds = if flags & CLONE_FILES != 0 {
        cx.t.fds.clone()
    } else {
        Rc::new(RefCell::new(cx.fds().clone()))
    };
    let fs = if flags & CLONE_FS != 0 {
        cx.t.fs.clone()
    } else {
        let mut copy = {
            let f = cx.t.fs.borrow();
            FsContext {
                root: f.root.clone(),
                cwd: f.cwd.clone(),
                umask: f.umask,
                mnt_ns: f.mnt_ns.clone(),
            }
        };
        if flags & CLONE_NEWNS != 0 {
            let owner = child_cred.user_ns.clone();
            super::ns::new_mnt_ns(cx.t.vfs.as_deref(), &mut copy, owner);
            let ns = copy.mnt_ns.clone();
            super::ns::count(cx.k(), Counted::Mnt, &child_cred.user_ns, euid, ns);
        }
        Rc::new(RefCell::new(copy))
    };
    let group = if thread {
        cx.t.group.clone()
    } else {
        let parent = cx.t.group.borrow();
        let sighand = if flags & CLONE_SIGHAND != 0 {
            parent.sighand.clone()
        } else {
            let mut h = parent.sighand.borrow().clone();
            // As with execve: handlers that are installed are reset to default, ignored signals
            // stay ignored
            if flags & CLONE_CLEAR_SIGHAND != 0 {
                h.reset_on_exec();
            }
            Rc::new(RefCell::new(h))
        };
        let ppid = if flags & CLONE_PARENT != 0 {
            parent.ppid
        } else {
            parent.tgid
        };
        let mut g = ThreadGroup::new(tid, ppid, sighand);
        g.parent_thread = if flags & CLONE_PARENT != 0 {
            parent.parent_thread
        } else {
            cx.tid()
        };
        g.pgid = parent.pgid;
        g.sid = parent.sid;
        g.rlimits = parent.rlimits;
        g.dumpable = parent.dumpable;
        g.mm_user_ns = parent.mm_user_ns.clone();
        g.no_new_privs = parent.no_new_privs;
        g.oom_score_adj = parent.oom_score_adj;
        g.oom_score_adj_min = parent.oom_score_adj_min;
        g.cred = child_cred.clone();
        g.personality = parent.personality;
        g.thp_disable = parent.thp_disable;
        g.coredump_filter = parent.coredump_filter;
        g.ctty = parent.ctty.clone();
        g.exe = parent.exe.clone();
        // As in Linux's copy_process, a CLONE_PARENT child signals the parent it shares with the
        // caller as the caller's process does (clone3 even requires an exit signal of 0 for it)
        g.exit_signal = if flags & CLONE_PARENT != 0 {
            parent.exit_signal
        } else {
            args.exit_signal
        };
        g.pid_ns = pid_ns.clone();
        g.cgroup = dst;
        g.frozen = frozen;
        Rc::new(RefCell::new(g))
    };

    // The child thread starts at clone's return with x0 = 0; threads (CLONE_VM but not vfork) do
    // not inherit the alternate stack
    // SAFETY: mm is held by the child thread's TaskState
    let mut child_cpu = unsafe { Cpu::new(mm.borrow().mmu()) };
    {
        let (p, c) = (cpu.state(), child_cpu.state_mut());
        c.x = p.x;
        c.x[0] = 0;
        c.sp = if stack != 0 { stack } else { p.sp };
        c.pc = p.pc;
        c.nzcv = p.nzcv;
        c.dit = p.dit;
        c.fpcr = p.fpcr;
        c.fpsr = p.fpsr;
        c.v = p.v;
        c.tpidr_el0 = if flags & CLONE_SETTLS != 0 {
            tls
        } else {
            p.tpidr_el0
        };
    }
    let altstack = if flags & (CLONE_VM | CLONE_VFORK) == CLONE_VM {
        AltStack::default()
    } else {
        cx.t.altstack
    };
    let vfork_done = (flags & CLONE_VFORK != 0).then(|| Rc::new(Cell::new(false)));
    let state = TaskState {
        mm: mm.clone(),
        fds,
        fs,
        group: group.clone(),
        vfs: cx.t.vfs.clone(),
        altstack,
        clear_child_tid: if flags & CLONE_CHILD_CLEARTID != 0 {
            ctid
        } else {
            0
        },
        vfork_done: vfork_done.clone().map(|d| (d, cx.task.event.clone())),
        restart: None,
        fast_sleep: None,
    };
    let tgid = if thread { cx.tgid() } else { tid };
    let mut ns = cx.ns();
    let owner = &child_cred_owner;
    if flags & CLONE_NEWUTS != 0 {
        ns.uts = ns.uts.copy(owner.clone());
        super::ns::count(cx.k(), Counted::Uts, owner, euid, ns.uts.clone());
    }
    if flags & CLONE_NEWIPC != 0 {
        let cred = cx.cred();
        ns.ipc = cx.k().new_ipc_ns(owner.clone(), (cred.fsuid, cred.fsgid));
        super::ns::count(cx.k(), Counted::Ipc, owner, euid, ns.ipc.clone());
    }
    if flags & CLONE_NEWNET != 0 {
        ns.net = cx.k().new_net_ns(owner.clone())?;
        super::ns::count(cx.k(), Counted::Net, owner, euid, ns.net.clone());
    }
    if flags & CLONE_NEWCGROUP != 0 {
        let root = cx.t.group.borrow().cgroup;
        ns.cgroup = super::ns::new_cgroup_ns(owner.clone(), root);
        super::ns::count(cx.k(), Counted::Cgroup, owner, euid, ns.cgroup.clone());
    }
    // Time namespaces (Linux's copy_namespaces and timens_on_fork, since 6.2): clone3's
    // CLONE_NEWTIME creates the one for children; a child that does not share the address space
    // enters the one for children, whose offsets can then no longer change; threads and vfork
    // children stay in the parent thread's and enter it when execve replaces the address space
    // (6.1 gives EINVAL; see the kernel/signal map)
    if flags & CLONE_NEWTIME != 0 {
        ns.time_for_children = ns.time_for_children.copy(owner.clone());
        let time = ns.time_for_children.clone();
        super::ns::count(cx.k(), Counted::Time, owner, euid, time);
    }
    if flags & CLONE_VM == 0 && ns.time.id != ns.time_for_children.id {
        ns.time = ns.time_for_children.clone();
        ns.time.frozen.set(true);
        // With offsets, the child's vDSO clock functions must enter the kernel layer, which adds
        // them (the kernel/vdso map)
        if ns.time.offsets_ns() != [0, 0] {
            mm.borrow_mut().vdso_syscalls()?;
        }
    }
    let task = Task::new(
        tid,
        tgid,
        &child_cpu,
        state,
        ns,
        cx.task.sigmask.get(),
        cx.task.comm.get(),
    )?;
    // A new thread starts with its creator's credentials, which may differ from the process's
    *task.cred.borrow_mut() = child_cred;
    task.affinity.set(cx.task.affinity.get());
    let (sched, nice) = super::sched::inherit(cx.task.sched.get(), cx.task.nice.get());
    task.sched.set(sched);
    task.nice.set(nice);
    *task.ioprio.borrow_mut() = super::sched::inherit_ioprio(&cx.task.ioprio.borrow(), flags);
    task.deadline.set(cx.task.deadline.get());
    // uclamp_fork: copied, back to the defaults with SCHED_RESET_ON_FORK
    task.uclamp.set(
        if cx.task.sched.get().0 & super::sched::SCHED_RESET_ON_FORK != 0 {
            (0, 1024)
        } else {
            cx.task.uclamp.get()
        },
    );
    let slack = cx.task.timerslack.get().0;
    task.timerslack.set((slack, slack));
    // The seccomp mode and filter chain are inherited as on Linux
    let seccomp = cx.task.seccomp.borrow().clone();
    *task.seccomp.borrow_mut() = seccomp;
    crate::seccomp::publish(&task);
    // Threads cloned by a traced thread may be traced too (crate::ptrace)
    let traced = crate::ptrace::clone(cx.task, &task, flags, args.exit_signal);

    // Allocate numbers in every pid-namespace level: the parent sees the one in its level, and
    // the child thread sees the one in its own
    cx.k().register_pid(&pid_ns, tid, &args.set_tid)?;
    let (vtid, child_vtid) = (cx.pid_out(tid), pid_ns.vnr(tid).unwrap_or(0));
    // Before writing tids, make sure the thread can start, so failure leaves no trace
    let started = (|| {
        if flags & CLONE_PARENT_SETTID != 0 {
            put(cx, ptid, vtid.to_le_bytes())?;
        }
        if flags & CLONE_CHILD_SETTID != 0 {
            mm.borrow()
                .write(UserSlice::new(ctid, 4), &child_vtid.to_le_bytes())?;
        }
        // CLONE_PIDFD: open the new process's pidfd in the parent (without CLONE_FILES the child
        // does not see it)
        let pidfd = if flags & CLONE_PIDFD != 0 {
            let fd = cx.fds_mut().install(new_pidfd(tid, &group, 0), true, 0)?;
            if let Err(e) = put(cx, args.pidfd, fd.to_le_bytes()) {
                let _ = cx.fds_mut().close(fd);
                return Err(e);
            }
            Some(fd)
        } else {
            None
        };
        let shared = cx.shared;
        if let Err(e) = task::start(shared, cx.k(), task, child_cpu) {
            if let Some(fd) = pidfd {
                let _ = cx.fds_mut().close(fd);
            }
            return Err(e);
        }
        Ok(())
    })();
    if let Err(e) = started {
        cx.k().release_pid(tid);
        return Err(e);
    }
    // It has not run yet: after the kick it enters the kernel on its first run and stops before
    // returning to the guest
    if group.borrow().frozen
        && let Some(t) = cx.kr().tasks.get(&tid)
    {
        crate::signal::wake(t);
    }
    // Every new task counts, threads too (Linux's total_forks)
    cx.k().forks += 1;
    if !thread {
        let ppid = group.borrow().ppid;
        let k = cx.k();
        k.groups.insert(tid, group);
        if let Some(parent) = k.groups.get(&ppid) {
            parent.borrow_mut().children.push(tid);
        }
    }

    // The tracer enabled this event: report the new thread (PTRACE_EVENT_FORK, VFORK, CLONE), with
    // the number the tracer sees (Linux's ptrace_event_pid)
    if let Some(e) = traced
        && !crate::ptrace::event(cx, cpu, e, crate::ptrace::tracer_pid_of(cx, tid) as u64)
    {
        return Ok(vtid as u64);
    }
    // vfork: wait for the child to execve or exit; only fatal signals can interrupt
    // (Linux's TASK_KILLABLE). Both wake this thread directly (task::release_mm,
    // signal::group_exit)
    if let Some(done) = vfork_done {
        // The wait counts as frozen for cgroup.events, as Linux's wait_for_vfork_done enters the
        // frozen state for it: a child frozen before its execve would otherwise keep a freezing
        // cgroup from ever reporting frozen (runc pause waits for that). While the cgroup is
        // freezing the thread stays counted until it stops on its way out (cgroup_leave_frozen)
        crate::cgroup::enter_frozen(cx);
        let waited = loop {
            if done.get() || cx.task.killed.get() {
                break Ok(());
            }
            if let Err(e) = cx.wait_alone(None) {
                break Err(e);
            }
        };
        if !cx.t.group.borrow().frozen {
            cx.task.frozen.set(false);
        }
        waited?;
        let pid = crate::ptrace::tracer_pid_of(cx, tid);
        crate::ptrace::event(cx, cpu, crate::ptrace::event::VFORK_DONE, pid as u64);
    }
    Ok(vtid as u64)
}

/// The number of threads whose real user ID is `uid` (`RLIMIT_NPROC`).
fn user_threads(k: &crate::kernel::Kernel, uid: u32) -> u64 {
    k.tasks
        .values()
        .filter(|t| t.cred.borrow().ruid == uid)
        .count() as u64
}

// ---- execve ----

pub(super) fn execve(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    path: u64,
    argv: u64,
    envp: u64,
) -> Result<(), Errno> {
    let path = read_path(cx, path)?;
    exec(cx, cpu, AT_FDCWD, path, argv, envp, 0)
}

/// execveat: the path is relative to `dirfd`; with `AT_EMPTY_PATH` and an empty path, `dirfd`
/// itself is executed (fexecve).
pub(super) fn execveat(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    dirfd: i32,
    path: u64,
    (argv, envp): (u64, u64),
    flags: u32,
) -> Result<(), Errno> {
    if flags & !(AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW) != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    exec(cx, cpu, dirfd, path, argv, envp, flags)
}

fn exec(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    dirfd: i32,
    path: Vec<u8>,
    argv: u64,
    envp: u64,
    flags: u32,
) -> Result<(), Errno> {
    let mut stack_limit = cx.t.group.borrow().rlimits[process::RLIMIT_STACK as usize].0;
    let limit = loader::arg_limit(stack_limit);
    let mut total = 0;
    let mut argv = read_strings(cx, argv, &mut total, limit)?;
    let envp = read_strings(cx, envp, &mut total, limit)?;
    // The same as Linux since 5.18: an empty argv gets one empty string, so the new program's
    // argc is at least 1
    if argv.is_empty() {
        argv.push(Vec::new());
    }
    let vfs = cx.vfs()?;
    let files = cx.k().file_maps.clone();
    let cwd = cx.cwd();
    let view = cx.view(&vfs);
    let cred = cx.cred();

    // The path the new program sees (AT_EXECFN, comm, the script interpreter's argument): when the
    // path does not start from the current directory or the root, it is /dev/fd/<dirfd>[/<path>]
    // as on Linux
    let by_fd = dirfd != AT_FDCWD && !path.starts_with(b"/");
    let filename = match (by_fd, path.is_empty()) {
        (false, _) => path.clone(),
        (true, true) => format!("/dev/fd/{dirfd}").into_bytes(),
        (true, false) => [format!("/dev/fd/{dirfd}/").as_bytes(), &path].concat(),
    };
    // Where the path of /proc/<pid>/exe is computed from: (start directory, path relative to it)
    let mut exe_at = (cwd.clone(), path.clone());
    // comm is the last component of the path; when the fd itself is executed it is the file name,
    // not the fd number
    let mut comm = filename.clone();
    let root = cx.root();
    // Executing through an open regular file (fexecve, /proc/<pid>/fd/<n>): use its host file
    // ([`process::open_open_file`]); deleted files and memfds cannot be opened by inode
    let mut by_file: Option<FileRef> = None;
    let node = if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        if dirfd == AT_FDCWD {
            cwd.clone()
        } else {
            let file = cx.fds().get(dirfd)?;
            let f = file.borrow();
            // A memfd's path carries ` (deleted)`; /proc/<pid>/exe adds it again by nlink when
            // read
            let shown = f.path.as_deref().unwrap_or_default();
            exe_at.1 = shown.strip_suffix(b" (deleted)").unwrap_or(shown).to_vec();
            if !exe_at.1.is_empty() {
                comm.clone_from(&exe_at.1);
            }
            let node = f.node().ok_or(Errno::EACCES)?;
            if matches!(f.kind, FileKind::Regular { .. }) {
                by_file = Some(file.clone());
            }
            node
        }
    } else {
        let start = super::fs::start_dir(cx, dirfd, &path)?;
        exe_at.0 = start.clone();
        let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
        match vfs.resolve(&view, &cred, &root, &start, &path, follow)? {
            // An open file not in the store that /proc/<pid>/fd/<n> leads to: deleted files and
            // memfds can be executed, pipes and the like are EACCES
            Node::Pseudo(_, crate::pseudo::PseudoNode::OpenFd(pid, fd)) => {
                let file = view.open_file(pid, fd).ok_or(Errno::ENOENT)?;
                let f = file.borrow();
                let node = f.node().ok_or(Errno::EACCES)?;
                let shown = f.path.as_deref().unwrap_or_default();
                exe_at.1 = shown.strip_suffix(b" (deleted)").unwrap_or(shown).to_vec();
                if matches!(f.kind, FileKind::Regular { .. }) {
                    by_file = Some(file.clone());
                }
                node
            }
            node => node,
        }
    };
    // Executing a deleted program through /proc/<pid>/exe (re-execing oneself): open the host
    // file that process recorded
    let from = match &node {
        Node::Pseudo(_, crate::pseudo::PseudoNode::OpenExe(pid)) => {
            Some(super::fs::exe_file(cx, *pid)?)
        }
        _ => None,
    };
    let (mut file, mut st, mut nosuid) = match (&from, &by_file) {
        (Some(exe), _) => process::open_exe_file(&vfs, &cred, exe)?,
        (None, Some(f)) => process::open_open_file(&vfs, &cred, &f.borrow())?,
        (None, None) => process::open_node(&vfs, &cred, &node)?,
    };
    let script = parse_shebang(file.head()).is_some();
    // A file open for writing cannot be executed, a script included (do_open_execat's
    // deny_write_access, before its interpreter is looked at)
    if script && super::fs::open_for_write(cx, st.ino) {
        return Err(Errno::ETXTBSY);
    }

    // `#!` scripts: switch to the interpreter, and argv becomes [interpreter, argument, script
    // path, original argv[1..]]
    let mut file_path = filename.clone();
    for depth in 0.. {
        let Some((interp, arg)) = parse_shebang(file.head()) else {
            break;
        };
        if depth == MAX_INTERP_DEPTH {
            return Err(Errno::ELOOP);
        }
        // The interpreter must open the script by path, and a close-on-exec dirfd will be closed
        // by then
        if depth == 0 && by_fd && cx.fds().cloexec(dirfd)? {
            return Err(Errno::ENOENT);
        }
        let mut new_argv = vec![interp.clone()];
        new_argv.extend(arg);
        new_argv.push(file_path);
        new_argv.extend(argv.into_iter().skip(1));
        argv = new_argv;
        (file, st, nosuid) = process::open_executable(&vfs, &view, &cred, &root, &cwd, &interp)?;
        exe_at = (cwd.clone(), interp.clone());
        file_path = interp;
    }
    // A program open for writing cannot be executed
    if super::fs::open_for_write(cx, st.ino) {
        return Err(Errno::ETXTBSY);
    }
    // The absolute path shown in /proc/<pid>/exe and maps, with every symlink resolved: musl
    // expands the main program's `$ORIGIN` from it, and programs started through links such as
    // /usr/bin/java rely on it to find their libraries
    let (exe_dir, exe_path) = &exe_at;
    let walked = vfs.walk(&view, &cred, &root, exe_dir, exe_path, true).ok();
    let real = match &walked {
        Some((n, found)) => vfs.found_path(&view, &root, n, found.as_ref()),
        None => Err(Errno::ENOENT),
    };
    let exe: Rc<[u8]> = match real {
        Ok(p) => Rc::from(p),
        Err(_) if exe_path.starts_with(b"/") => Rc::from(normalize(exe_path)),
        Err(_) => {
            let mut p = vfs.path_of(&view, &root, exe_dir)?;
            p.push(b'/');
            p.extend_from_slice(exe_path);
            Rc::from(normalize(&p))
        }
    };

    // setuid and setgid programs (no effect after `PR_SET_NO_NEW_PRIVS` or on a nosuid mount) and
    // the program file's capabilities; for scripts the interpreter counts. No new privileges are
    // gained while traced by a tracer without CAP_SYS_PTRACE
    let no_new_privs = cx.t.group.borrow().no_new_privs;
    // Also no effect when the file's owner or group has no mapping in one's own user namespace
    // (bprm_fill_uid)
    let mapped = cred.user_ns.uid_map.borrow().up(st.uid).is_some()
        && cred.user_ns.gid_map.borrow().up(st.gid).is_some();
    let setid = |bit: u32| st.mode & bit == bit && !no_new_privs && !nosuid && mapped;
    let fcaps = if nosuid {
        None
    } else {
        file_caps(&vfs, st.ino, &cred)?
    };
    let ptraced = {
        let t = cx.task.trace.borrow();
        t.tracer.is_some() && !t.tracer_capable(&cred.user_ns)
    };
    // Another process shares the fs context (clone with CLONE_FS but not CLONE_THREAD) and could
    // change the new program's root, cwd or umask, so it gains no privileges either (Linux's
    // LSM_UNSAFE_SHARE from check_unsafe_exec). Each thread holds two references (TaskState and
    // TaskRes), so with no more than that nothing else can
    let shared_fs = Rc::strong_count(&cx.t.fs) > 2
        && cx.kr().tasks.values().any(|t| {
            t.tgid != cx.tgid()
                && t.res
                    .try_borrow()
                    .is_ok_and(|r| Rc::ptr_eq(&r.fs, &cx.t.fs))
        });
    let mut new_cred = cred.clone();
    let (setuid, setgid) = (
        setid(mode::S_ISUID).then_some(st.uid),
        setid(mode::S_ISGID | 0o010).then_some(st.gid),
    );
    let secure = new_cred.exec(setuid, setgid, fcaps, no_new_privs, ptraced || shared_fs)?;
    // Linux's bprm->per_clear: a setuid or setgid program, or one whose file capabilities grant
    // more than the caller had (counted before no_new_privs or an unsafe execution takes the gain
    // away), drops the personality flags that weaken a program's protections
    let clear_personality = setuid.is_some() || setgid.is_some() || {
        let mut c = cred.clone();
        c.exec(setuid, setgid, fcaps, false, false).is_ok()
            && c.cap_permitted & !cred.cap_permitted != 0
    };
    // As on Linux, a privileged exec's stack size limit is at most the default, so the caller
    // cannot use it to influence the new program's address-space layout; the new value is written
    // back after execve succeeds
    if secure {
        stack_limit = stack_limit.min(loader::STACK_SIZE);
    }
    let args = Args {
        argv: &argv,
        envp: &envp,
        execfn: &filename,
        // AT_UID and the like are values in one's own user namespace (Linux's create_elf_tables)
        ids: [
            new_cred.uid_out(new_cred.ruid),
            new_cred.uid_out(new_cred.euid),
            new_cred.gid_out(new_cred.rgid),
            new_cred.gid_out(new_cred.egid),
        ],
        secure,
        stack_limit,
    };
    // An interpreter the caller cannot read makes the process non-dumpable too (would_dump): its
    // owner and group
    let interp_unreadable = std::cell::Cell::new(None);
    let mut read_interp = |p: &[u8]| {
        let (exe, st, _) = process::open_executable(&vfs, &view, &cred, &root, &cwd, p)?;
        if cred.permission(&st, crate::cred::MAY_READ).is_err() {
            interp_unreadable.set(Some((st.uid, st.gid)));
        }
        Ok(exe)
    };
    let prepared = loader::prepare(&file, &mut read_interp).map_err(load_error)?;
    // The new address space is built with the BKL released: mapping the program's segments and
    // building the initial stack took most of execve's time under the lock, and processes at both
    // ends of a pipe or exec'ing at once in a parallel build used to queue. Meanwhile only the new
    // address space and the read-only file mappings shared by every address space (FileMaps,
    // which has its own lock) are touched
    let built = cx.unlocked(|| {
        let mut mm = Mm::new();
        mm.share_files(&files);
        loader::load_prepared(&mut mm, &prepared, &args).map(|image| (mm, image))
    });
    // Ended while waiting for the BKL (another thread's exit_group or execve, or a fatal signal)
    if cx.task.killed.get() {
        return Err(Errno::EINTR);
    }
    let (mut mm, image) = built.map_err(load_error)?;
    let view = cx.view(&vfs);
    let interp = interp_label(Some(&*vfs), &view, &cred, &root, &cwd, &image);
    label_image(&mut mm, &image, (&exe, st.ino), interp);
    mm.exe = Some(st.ino);
    // The process runs in the time namespace for children from here on (entered below); with
    // offsets, the vDSO's clock functions must enter the kernel layer, which adds them
    if cx.task.ns.borrow().time_for_children.offsets_ns() != [0, 0] {
        mm.vdso_syscalls()?;
    }

    // From here on nothing may fail: end the other threads in the thread group, then replace the
    // address space
    kill_other_threads(cx)?;
    // The address space was replaced: after an unshare of CLONE_NEWTIME, enter the one for
    // children (Linux's exec_task_namespaces, since 6.2), whose offsets can then no longer change
    {
        let mut p = cx.task.ns.borrow_mut();
        if p.time.id != p.time_for_children.id {
            p.time = p.time_for_children.clone();
            p.time.frozen.set(true);
            drop(p);
            cx.shared.fast_changed();
        }
    }
    // Copy the fd table only when another process shares it (CLONE_FILES; Linux's
    // unshare_files): this thread's TaskState and TaskRes hold the only two references once the
    // other threads are gone. A copy is a new owner, so the POSIX record locks this process holds
    // would be lost (crate::locks)
    if Rc::strong_count(&cx.t.fds) > 2 {
        let copy = cx.fds().clone();
        cx.set_fds(Rc::new(RefCell::new(copy)));
    }
    let closed = cx.fds_mut().close_on_exec();
    let fds = cx.t.fds.clone();
    crate::locks::closed(cx.k(), &fds, &closed);
    super::mqueue_closed(cx.tgid(), &closed);
    super::fuse::flush_all(cx, &closed);
    crate::unix::released(cx.k(), closed);
    {
        let group = cx.t.group.clone();
        let mut g = group.borrow_mut();
        // The peak resident memory is kept across execve: sample once before replacing the
        // address space (before marking execed)
        g.sample_rss_at_exec(&cx.mm());
        // A program found through an overlay (by the walk at execution, or the node or open file
        // it was run through, or as the removed overlay program of the process whose exe it was
        // run through) is recorded by its overlay node
        let ovl = {
            let overlays = vfs.overlay();
            let ovl_of = |n: &Node| match *n {
                Node::Overlay(m, o) if overlays.node(o).real() == st.ino => Some((m, o)),
                _ => None,
            };
            walked
                .as_ref()
                .and_then(|(n, _)| ovl_of(n))
                .or_else(|| ovl_of(&node).filter(|_| !script))
                .or_else(|| {
                    let (m, o, _) = from.as_deref().filter(|_| !script)?.ovl.as_ref()?;
                    Some((*m, *o))
                })
        };
        g.exe = Some(Rc::new(process::ExeFile::new(
            exe.clone(),
            walked,
            (!script).then_some(&node),
            from.as_deref().filter(|_| !script),
            Some(st.ino),
            &file,
            ovl,
        )));
        // As with Linux's commit_creds, changing the effective IDs or gaining new capabilities
        // makes the process non-dumpable, and the parent's exit no longer signals it; a
        // privileged exec likewise no longer signals it. Other execs make it dumpable again
        // (begin_new_exec), except when the program file or its interpreter is not readable by
        // the caller (would_dump)
        let ids = |c: &Cred| (c.euid, c.egid, c.fsuid, c.fsgid);
        let gained = new_cred.cap_permitted & !cred.cap_permitted != 0;
        if (new_cred.euid, new_cred.egid) != (new_cred.ruid, new_cred.rgid)
            || ids(&new_cred) != ids(&cred)
            || gained
        {
            g.dumpable = false;
            cx.task.pdeathsig.set(0);
        } else {
            g.dumpable = cred.permission(&st, crate::cred::MAY_READ).is_ok()
                && interp_unreadable.get().is_none();
        }
        // The new address space belongs to the caller's user namespace, or to the nearest
        // ancestor where a program file it could not read is owned by mapped IDs (would_dump's
        // mm->user_ns), so tracers privileged only in a namespace below cannot reach it
        let mut mm_ns = cred.user_ns.clone();
        if cred.permission(&st, crate::cred::MAY_READ).is_err() {
            mm_ns = mm_ns.holding(st.uid, st.gid);
        }
        if let Some((uid, gid)) = interp_unreadable.get() {
            mm_ns = mm_ns.holding(uid, gid);
        }
        g.mm_user_ns = Some(mm_ns);
        if secure {
            cx.task.pdeathsig.set(0);
        }
        // arm64's SET_PERSONALITY drops READ_IMPLIES_EXEC for every 64-bit program, so the flag
        // lasts only until the next execve
        g.personality &= !process::READ_IMPLIES_EXEC;
        if clear_personality {
            g.personality &= !process::PER_CLEAR_ON_SETID;
        }
        // The thread is the main thread now (kill_other_threads)
        g.cred = new_cred.clone();
        *cx.task.cred.borrow_mut() = new_cred;
        cx.shared.fast_changed();
        g.rlimits[process::RLIMIT_STACK as usize].0 = stack_limit;
        g.posix_timers.clear();
        // The AIO contexts' rings are in the old address space and go away with it
        let contexts = std::mem::take(&mut g.aio);
        drop(g);
        super::aio::released(cx.k(), contexts);
        let mut g = group.borrow_mut();
        g.execed = true;
        if Rc::strong_count(&g.sighand) > 1 {
            let copy = g.sighand.borrow().clone();
            g.sighand = Rc::new(RefCell::new(copy));
        }
        g.sighand.borrow_mut().reset_on_exec();
    }
    let task = cx.task.clone();
    task::release_mm(cx.shared, &task, cx.t);
    let mm = mm.into_shared();
    mm.borrow_mut().add_user(task.sync.clone());
    // SAFETY: the new address space is held by the TaskState; the old one may be released by this
    // assignment, and cpu no longer uses it
    unsafe { cpu.reset(mm.borrow().mmu()) };
    cx.set_mm(mm);
    // The old address space's attachments are gone: a segment marked for removal goes with its
    // last one
    crate::ipc::reap_shm(cx.k());
    cx.t.altstack = AltStack::default();
    cx.task.saved_sigmask.set(None);
    cx.task.comm.set(process::comm_of(&comm));
    let st = cpu.state_mut();
    st.pc = image.entry;
    st.sp = image.sp;
    // What Linux fails to map after the point of no return kills the process (force_fatal_sig)
    if image.doomed {
        signal::force_fatal(cx, Signal::SEGV, SigInfo::kernel());
    }
    // Handle already pending signals before the new program starts
    if cx.interrupted() {
        // SAFETY: cpu belongs to the current thread, which is running
        unsafe { cx.task.sync.kicker.kick() };
    }
    Ok(())
}

/// Reads execve's argv or envp: a NULL-terminated array of string pointers. Returns E2BIG when the
/// strings and pointers together exceed `limit` bytes ([`loader::arg_limit`]).
fn read_strings(
    cx: &Cx<'_>,
    addr: u64,
    total: &mut usize,
    limit: usize,
) -> Result<Vec<Vec<u8>>, Errno> {
    let mut out = Vec::new();
    if addr == 0 {
        return Ok(out);
    }
    for i in 0.. {
        let p = u64::from_le_bytes(super::get(cx, addr.wrapping_add(i * 8))?);
        if p == 0 {
            break;
        }
        let s = cx.mm().read_cstr(p, MAX_ARG_STRLEN - 1).map_err(|e| {
            if e == Errno::ENAMETOOLONG {
                Errno::E2BIG
            } else {
                e
            }
        })?;
        *total += s.len() + 1 + 8;
        if *total > limit {
            return Err(Errno::E2BIG);
        }
        out.push(s);
    }
    Ok(out)
}

/// `#!` line: (interpreter, optional single argument). None when not a script.
fn parse_shebang(file: &[u8]) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    let head = file.strip_prefix(b"#!")?;
    let line = &head[..head.len().min(SHEBANG_MAX - 2)];
    let line = line.split(|&b| b == b'\n').next().unwrap_or(line);
    let is_space = |b: &u8| *b == b' ' || *b == b'\t';
    let line = line.trim_ascii();
    let end = line.iter().position(is_space).unwrap_or(line.len());
    let interp = &line[..end];
    if interp.is_empty() {
        return None;
    }
    let rest = line[end..].trim_ascii();
    Some((interp.to_vec(), (!rest.is_empty()).then(|| rest.to_vec())))
}

/// The program file's capabilities (`security.capability`) for `cred`; None when absent or
/// belonging to a root that does not own `cred`'s namespace, and execve fails (EINVAL) when
/// malformed.
fn file_caps(vfs: &Vfs, ino: Ino, cred: &Cred) -> Result<Option<FileCaps>, Errno> {
    let store = vfs.store();
    if !store.has_file_caps(ino)? {
        return Ok(None);
    }
    match store.get_xattr(ino, XATTR_NAME_CAPS) {
        Ok(v) => FileCaps::parse(&v, cred),
        Err(Errno::ENODATA) => Ok(None),
        Err(e) => Err(e),
    }
}

fn load_error(e: LoadError) -> Errno {
    match e {
        LoadError::NotElf | LoadError::Unsupported(_) | LoadError::Malformed(_) => Errno::ENOEXEC,
        LoadError::BadInterpreter(_) => Errno::ELIBBAD,
        LoadError::ArgumentsTooLong => Errno::E2BIG,
        LoadError::Memory(e) | LoadError::Read(e) | LoadError::Interpreter(_, e) => e,
    }
}

/// The interpreter's name and inode in maps: the name has its symlinks resolved like the main
/// program's (Debian's `/lib/ld-linux-aarch64.so.1` is a link); the path from PT_INTERP as is when
/// not found.
pub(crate) fn interp_label(
    vfs: Option<&Vfs>,
    ps: &dyn Pseudo,
    cred: &Cred,
    root: &Node,
    cwd: &Node,
    image: &loader::Image,
) -> Option<(Rc<[u8]>, u64)> {
    let (path, ..) = image.interp.as_ref()?;
    let walked = vfs.and_then(|vfs| Some((vfs, vfs.walk(ps, cred, root, cwd, path, true).ok()?)));
    let Some((vfs, (node, found))) = walked else {
        return Some((Rc::from(path.as_slice()), 0));
    };
    let name = vfs
        .found_path(ps, root, &node, found.as_ref())
        .unwrap_or_else(|_| path.clone());
    let ino = node.ino().unwrap_or(0);
    Some((Rc::from(name), ino))
}

/// Names the program's and interpreter's mappings (`/proc/<pid>/maps`). `exe` and `interp` are
/// the names and the inodes in the store (0 when unknown); for the interpreter's see
/// [`interp_label`].
pub(crate) fn label_image(
    mm: &mut Mm,
    image: &loader::Image,
    exe: (&Rc<[u8]>, u64),
    interp: Option<(Rc<[u8]>, u64)>,
) {
    if image.main.1 <= image.main.0 {
        return;
    }
    mm.label(
        image.main.0,
        image.main.1 - image.main.0,
        Some(exe.0.clone()),
        exe.1,
    );
    if let (Some((_, start, end)), Some((name, ino))) = (&image.interp, interp) {
        mm.label(*start, end - start, Some(name), ino);
    }
}

/// execve's de_thread: ends the other threads in the thread group and waits for them to exit.
/// When execve is called by a thread other than the main thread, it takes over the main thread's
/// tid.
fn kill_other_threads(cx: &mut Cx<'_>) -> Result<(), Errno> {
    let me = cx.tid();
    let others: Vec<Tid> =
        cx.t.group
            .borrow()
            .threads
            .iter()
            .copied()
            .filter(|&t| t != me)
            .collect();
    if others.is_empty() {
        return Ok(());
    }
    cx.kr().cgroups.task_changed(cx.t.group.borrow().cgroup);
    for tid in &others {
        if let Some(t) = cx.kr().tasks.get(tid) {
            t.killed.set(true);
            signal::wake(t);
        }
    }
    while cx.t.group.borrow().threads.len() > 1 {
        if cx.task.killed.get() {
            // Another thread is ending the whole process at the same time
            return Err(Errno::EINTR);
        }
        cx.wait(None, &mut [])?;
    }
    let tgid = cx.tgid();
    if me != tgid {
        let k = cx.k();
        let task = k.tasks.remove(&me).unwrap();
        task.tid.set(tgid);
        k.tasks.insert(tgid, task);
        k.release_pid(me);
        let children = {
            let mut g = cx.t.group.borrow_mut();
            g.threads = vec![tgid];
            // The original main thread has been reaped (release_task in Linux's de_thread)
            g.leader_seccomp = None;
            g.leader_exit = None;
            g.children.clone()
        };
        // Children created by other threads were handed to this thread when they exited, and
        // now this thread's number has changed
        for c in children {
            if let Some(cg) = cx.kr().groups.get(&c) {
                let mut cg = cg.borrow_mut();
                if cg.parent_thread == me {
                    cg.parent_thread = tgid;
                }
            }
        }
    }
    Ok(())
}

// ---- Exit and reaping ----

pub(super) fn set_tid_address(cx: &mut Cx<'_>, addr: u64) -> SysResult {
    cx.t.clear_child_tid = addr;
    Ok(cx.tid() as u64)
}

pub(super) fn exit(cx: &mut Cx<'_>, code: i32) -> Flow {
    let status = ExitStatus::Exited(code & 0xff);
    cx.task.exit_status.set(status);
    if cx.tid() == cx.tgid() {
        cx.t.group.borrow_mut().leader_status = status;
    }
    Flow::Exit
}

pub(super) fn exit_group(cx: &mut Cx<'_>, code: i32) -> Flow {
    let group = cx.t.group.clone();
    signal::group_exit(cx.k(), &group, ExitStatus::Exited(code & 0xff));
    Flow::Exit
}

/// What to wait for: a pid, any child, or a process group.
#[derive(Clone, Copy)]
enum Who {
    Pid(Tid),
    Any,
    Pgid(Tid),
}

/// A state change of a child (or of a traced thread).
#[derive(Clone, Copy)]
enum ChildEvent {
    Exited(ExitStatus),
    Stopped(Signal),
    Continued,
    /// The traced thread is in a ptrace-stop, with the status reported to the tracer
    /// (`crate::ptrace`).
    Trapped(i32),
}

impl ChildEvent {
    /// wait4's status word.
    fn wait_status(self) -> i32 {
        match self {
            ChildEvent::Exited(s) => s.wait_status(),
            ChildEvent::Stopped(sig) => sig.number() << 8 | 0x7f,
            ChildEvent::Continued => 0xffff,
            ChildEvent::Trapped(code) => code << 8 | 0x7f,
        }
    }

    /// waitid's siginfo: `uid` is the child's real user ID as seen in the caller's namespace.
    fn siginfo(self, pid: Tid, uid: u32) -> SigInfo {
        match self {
            ChildEvent::Exited(s) => SigInfo::child(pid, uid, s),
            ChildEvent::Stopped(sig) => {
                SigInfo::child_event(pid, uid, code::CLD_STOPPED, sig.number())
            }
            ChildEvent::Continued => {
                SigInfo::child_event(pid, uid, code::CLD_CONTINUED, Signal::CONT.number())
            }
            ChildEvent::Trapped(status) => {
                SigInfo::child_event(pid, uid, code::CLD_TRAPPED, status)
            }
        }
    }
}

/// Which state changes to wait for (`WEXITED`, `WUNTRACED`/`WSTOPPED`, `WCONTINUED`), whether to
/// consume the event, and whether to wait only for children created by the calling thread
/// (`__WNOTHREAD`).
#[derive(Clone, Copy)]
struct Wanted {
    exited: bool,
    stopped: bool,
    continued: bool,
    consume: bool,
    own_thread: bool,
    /// Which children by the signal they report their exit with (Linux's `eligible_child`): all
    /// with `__WALL` (None); with `__WCLONE` only those reporting with another signal than
    /// `SIGCHLD` (Some(true)); otherwise only those reporting with `SIGCHLD` (Some(false)). A
    /// child the caller traces is always eligible.
    clones: Option<bool>,
}

/// [`Wanted::clones`] from wait4's or waitid's options.
fn clones(options: u32) -> Option<bool> {
    (options & WALL == 0).then_some(options & WCLONE != 0)
}

/// A child state change found: the child (its number as the caller sees it), its state change,
/// its resource usage (including CPU time used by the children it reaped), and its real user ID
/// (a kernel ID, waitid's si_uid).
type Found = (Tid, ChildEvent, process::Rusage, u32);

pub(super) fn wait4(
    cx: &mut Cx<'_>,
    pid: i32,
    wstatus: u64,
    options: u32,
    rusage: u64,
) -> SysResult {
    if options & !(WNOHANG | WUNTRACED | WCONTINUED | WNOTHREAD | WALL | WCLONE) != 0 {
        return Err(Errno::EINVAL);
    }
    // -INT_MIN cannot be represented as a process group ID
    if pid == i32::MIN {
        return Err(Errno::ESRCH);
    }
    let who = match pid {
        -1 => Who::Any,
        0 => Who::Pgid(cx.t.group.borrow().pgid),
        p if p < 0 => Who::Pgid(-p),
        p => Who::Pid(p),
    };
    let want = Wanted {
        exited: true,
        stopped: options & WUNTRACED != 0,
        continued: options & WCONTINUED != 0,
        consume: true,
        own_thread: options & WNOTHREAD != 0,
        clones: clones(options),
    };
    let Some((child, event, usage, _)) = wait_child(cx, who, options & WNOHANG != 0, want)? else {
        return Ok(0);
    };
    if wstatus != 0 {
        put(cx, wstatus, event.wait_status().to_le_bytes())?;
    }
    if rusage != 0 {
        put(cx, rusage, usage.encode())?;
    }
    Ok(child as u64)
}

pub(super) fn waitid(
    cx: &mut Cx<'_>,
    idtype: u32,
    id: i32,
    infop: u64,
    options: u32,
    rusage: u64,
) -> SysResult {
    if options & !(WNOHANG | WUNTRACED | WEXITED | WCONTINUED | WNOWAIT | WNOTHREAD | WALL | WCLONE)
        != 0
        || options & (WEXITED | WUNTRACED | WCONTINUED) == 0
    {
        return Err(Errno::EINVAL);
    }
    let mut nonblock = false;
    let who = match idtype {
        P_ALL => Who::Any,
        P_PID if id > 0 => Who::Pid(id),
        P_PGID if id > 0 => Who::Pgid(id),
        P_PGID if id == 0 => Who::Pgid(cx.t.group.borrow().pgid),
        // When the process the pidfd refers to has been reaped and its pid given to another, it
        // cannot be waited for, as on Linux
        P_PIDFD if id >= 0 => {
            let file = cx.fds().get(id)?;
            let f = file.borrow();
            let FileKind::Pidfd(p) = &f.kind else {
                return Err(Errno::EBADF);
            };
            nonblock = f.nonblocking();
            match p.group(cx.kr()) {
                Some(_) => Who::Pid(p.tgid),
                None => return Err(Errno::ECHILD),
            }
        }
        _ => return Err(Errno::EINVAL),
    };
    let want = Wanted {
        exited: options & WEXITED != 0,
        stopped: options & WUNTRACED != 0,
        continued: options & WCONTINUED != 0,
        consume: options & WNOWAIT == 0,
        own_thread: options & WNOTHREAD != 0,
        clones: clones(options),
    };
    let nohang = options & WNOHANG != 0;
    let found = wait_child(cx, who, nohang || nonblock, want)?;
    // A non-blocking pidfd: EAGAIN when there is no event, unless the caller asked for WNOHANG
    if found.is_none() && nonblock && !nohang {
        return Err(Errno::EAGAIN);
    }
    if infop != 0 {
        let info = match found {
            Some((child, event, _, uid)) => event
                .siginfo(child, cx.cred().uid_out(uid))
                .encode(Signal::CHLD),
            None => [0; 128],
        };
        cx.write_user(UserSlice::new(infop, 128), &info)?;
    }
    if rusage != 0 {
        let usage = found.map_or(process::Rusage::default(), |f| f.2);
        put(cx, rusage, usage.encode())?;
    }
    Ok(0)
}

/// The part common to wait4 and waitid: waits until a child's state changes.
fn wait_child(
    cx: &mut Cx<'_>,
    who: Who,
    nohang: bool,
    want: Wanted,
) -> Result<Option<Found>, Errno> {
    loop {
        if let Some(found) = find_event(cx, who, want)? {
            return Ok(Some(found));
        }
        if nohang {
            return Ok(None);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        // Only child state changes (and directly notified signals) can make the condition here
        // true (crate::kernel::wake)
        cx.wait_on(None, &mut [], crate::kernel::wake::CHILD)?;
    }
}

/// Finds a matching child state change, consuming it when `consume` (an exited child is reaped at
/// the same time). ptrace-stops and exits of traced threads (not necessarily children) count too.
/// Returns ECHILD when no child or traced thread matches.
fn find_event(cx: &mut Cx<'_>, who: Who, want: Wanted) -> Result<Option<Found>, Errno> {
    let children = cx.t.group.borrow().children.clone();
    let me = cx.tgid();
    let matches = |pid: Tid, pgid: Tid| match who {
        Who::Pid(p) => p == pid,
        Who::Any => true,
        Who::Pgid(g) => g == pgid,
    };
    // As on Linux, a child's usage includes the children it reaped. Computed only when an event
    // is found: a live thread's kernel-mode time requires reading the host's /proc, and wait
    // rescans the children every time it is woken
    let usage = |g: &ThreadGroup, k: &crate::kernel::Kernel| process::Rusage {
        cpu: g.cpu(k) + g.child_cpu,
        maxrss_kb: g.maxrss_kb.max(g.child_maxrss_kb),
    };
    let mut any = false;
    let tid = cx.tid();
    for c in children {
        let Some(child) = cx.kr().groups.get(&c).cloned() else {
            continue;
        };
        if !matches(c, child.borrow().pgid) {
            continue;
        }
        // __WNOTHREAD: children created by other threads are invisible (Linux's do_wait looks
        // only at this thread's children)
        if want.own_thread && child.borrow().parent_thread != tid {
            continue;
        }
        // The main thread is traced by the caller: stops and continues are reported as
        // ptrace-stops (below), and as on Linux not reported twice
        let traced = cx
            .kr()
            .tasks
            .get(&c)
            .is_some_and(|t| t.trace.borrow().tracer == Some(me));
        let clone = child.borrow().exit_signal != Signal::CHLD.number();
        if !traced && want.clones.is_some_and(|c| c != clone) {
            continue;
        }
        // A zombie counts only for a wait that takes exits (Linux's wait_task_zombie returns
        // before clearing notask_error): waiting only for stops and continues of children that
        // have all exited is ECHILD
        if !want.exited && {
            let g = child.borrow();
            g.zombie.is_some() && g.exit_tracer.is_none()
        } {
            continue;
        }
        any = true;
        let mut g = child.borrow_mut();
        // Exited but not yet collected by the tracer: only the tracer can see it (trace_exits
        // below), and the parent keeps waiting
        if g.exit_tracer.is_some() {
            continue;
        }
        // Translate before reaping: after reaping the number is no longer in the pid namespace
        let vpid = cx.pid_out(c);
        let uid = g.cred.ruid;
        if let (true, Some(status)) = (want.exited, g.zombie) {
            let usage = usage(&g, cx.kr());
            if want.consume {
                drop(g);
                cx.k().groups.remove(&c);
                cx.k().release_pid(c);
                let mut me = cx.t.group.borrow_mut();
                me.children.retain(|&x| x != c);
                me.child_cpu += usage.cpu;
                me.child_maxrss_kb = me.child_maxrss_kb.max(usage.maxrss_kb);
            }
            return Ok(Some((vpid, ChildEvent::Exited(status), usage, uid)));
        }
        if traced {
            continue;
        }
        if let (true, Some(sig)) = (want.stopped, g.stop_report) {
            if want.consume {
                g.stop_report = None;
            }
            return Ok(Some((
                vpid,
                ChildEvent::Stopped(sig),
                usage(&g, cx.kr()),
                uid,
            )));
        }
        if want.continued && g.cont_report {
            if want.consume {
                g.cont_report = false;
            }
            return Ok(Some((vpid, ChildEvent::Continued, usage(&g, cx.kr()), uid)));
        }
    }
    // Traced threads: ptrace-stops do not require WUNTRACED (as on Linux, nor __WALL)
    let tracees: Vec<_> = cx
        .kr()
        .tasks
        .values()
        .filter(|t| t.trace.borrow().tracer == Some(me))
        .cloned()
        .collect();
    for t in tracees {
        let pgid = cx.kr().groups.get(&t.tgid).map_or(0, |g| g.borrow().pgid);
        if !matches(t.tid.get(), pgid) {
            continue;
        }
        any = true;
        let mut trace = t.trace.borrow_mut();
        if let (true, false, Some(code)) = (trace.stopped(), trace.reported, trace.exit_code()) {
            if want.consume {
                trace.reported = true;
            }
            // As on Linux, report the usage of its process (strace -c uses it to time syscalls)
            let (usage, uid) = cx.kr().groups.get(&t.tgid).map_or_else(
                || (process::Rusage::default(), 0),
                |g| (usage(&g.borrow(), cx.kr()), g.borrow().cred.ruid),
            );
            return Ok(Some((
                cx.pid_out(t.tid.get()),
                ChildEvent::Trapped(code),
                usage,
                uid,
            )));
        }
    }
    // A traced thread exited
    let exits = &cx.kr().trace_exits;
    if let Some(i) = exits
        .iter()
        .position(|e| e.tracer == me && matches(e.pid, e.pgid))
    {
        any = true;
        if want.exited {
            let (pid, seen, status) = (exits[i].pid, exits[i].seen, exits[i].status);
            let uid = exits[i].cred.ruid;
            // The whole process ended: as on Linux, report its usage (it still exists, waiting to
            // be handed to its parent)
            let usage = match cx.kr().groups.get(&pid) {
                Some(g) if exits[i].process => usage(&g.borrow(), cx.kr()),
                _ => process::Rusage::default(),
            };
            if want.consume {
                let e = cx.k().trace_exits.remove(i);
                crate::ptrace::released(cx.k(), &e);
            }
            return Ok(Some((seen, ChildEvent::Exited(status), usage, uid)));
        }
    }
    if any { Ok(None) } else { Err(Errno::ECHILD) }
}

// ---- pidfds and kcmp ----

/// pidfd_open: `flags` may only be `PIDFD_NONBLOCK` (`PIDFD_THREAD`, added in 6.9, is not
/// supported), and pid must be a process, not a thread. The pidfd is always close-on-exec.
pub(super) fn pidfd_open(cx: &mut Cx<'_>, pid: i32, flags: u32) -> SysResult {
    if pid <= 0 || flags & !oflags::O_NONBLOCK != 0 {
        return Err(Errno::EINVAL);
    }
    let Some(group) = cx.kr().groups.get(&pid).cloned() else {
        let thread = cx.kr().tasks.contains_key(&pid);
        return Err(if thread { Errno::EINVAL } else { Errno::ESRCH });
    };
    let file = new_pidfd(pid, &group, flags);
    cx.fds_mut().install(file, true, 0).map(|fd| fd as u64)
}

/// A new pidfd referring to thread group `group` (with pid `tgid`).
fn new_pidfd(tgid: Tid, group: &Rc<RefCell<ThreadGroup>>, flags: u32) -> FileRef {
    let p = Pidfd {
        tgid,
        group: Rc::downgrade(group),
    };
    OpenFile::new(FileKind::Pidfd(p), oflags::O_RDWR | flags)
}

/// pidfd_getfd: duplicates `target` of the process the pidfd refers to into the caller, with
/// close-on-exec.
pub(super) fn pidfd_getfd(cx: &mut Cx<'_>, pidfd: i32, target: i32, flags: u32) -> SysResult {
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let file = cx.fds().get(pidfd)?;
    let group = match &file.borrow().kind {
        FileKind::Pidfd(p) => p.group(cx.kr()).ok_or(Errno::ESRCH)?,
        _ => return Err(Errno::EBADF),
    };
    let leader = group
        .borrow()
        .threads
        .first()
        .copied()
        .ok_or(Errno::ESRCH)?;
    let tgid = group.borrow().tgid;
    if !may_ptrace(cx, tgid) {
        return Err(Errno::EPERM);
    }
    let task = cx.kr().tasks.get(&leader).cloned().ok_or(Errno::ESRCH)?;
    let fds = task.res.borrow().fds.clone();
    let remote = fds.borrow().get(target)?;
    cx.fds_mut().install(remote, true, 0).map(|fd| fd as u64)
}

/// kcmp: whether some resource of two threads is the same one. Returns 0 when the same, otherwise
/// 1 or 2 by object address (Linux's addresses are scrambled, guaranteeing only a consistent
/// order).
pub(super) fn kcmp(
    cx: &mut Cx<'_>,
    pid1: i32,
    pid2: i32,
    ty: i32,
    idx1: u64,
    idx2: u64,
) -> SysResult {
    const KCMP_FILE: i32 = 0;
    const KCMP_VM: i32 = 1;
    const KCMP_FILES: i32 = 2;
    const KCMP_FS: i32 = 3;
    const KCMP_SIGHAND: i32 = 4;
    const KCMP_IO: i32 = 5;
    const KCMP_SYSVSEM: i32 = 6;
    const KCMP_EPOLL_TFD: i32 = 7;
    let task = |pid: i32| cx.kr().tasks.get(&pid).cloned().ok_or(Errno::ESRCH);
    let (t1, t2) = (task(pid1)?, task(pid2)?);
    for t in [&t1, &t2] {
        if !may_ptrace(cx, t.tid.get()) {
            return Err(Errno::EPERM);
        }
    }
    let file = |t: &Task, fd: u64| -> Result<usize, Errno> {
        let fds = t.res.borrow().fds.clone();
        let f = fds
            .borrow()
            .get(i32::try_from(fd).map_err(|_| Errno::EBADF)?)?;
        Ok(Rc::as_ptr(&f) as usize)
    };
    let sighand = |t: &Task| -> usize {
        cx.kr()
            .groups
            .get(&t.tgid)
            .map_or(0, |g| Rc::as_ptr(&g.borrow().sighand) as usize)
    };
    let (a, b) = match ty {
        KCMP_FILE => (file(&t1, idx1)?, file(&t2, idx2)?),
        KCMP_VM => {
            let vm = |t: &Task| Rc::as_ptr(&t.res.borrow().mm) as usize;
            (vm(&t1), vm(&t2))
        }
        KCMP_FILES => {
            let files = |t: &Task| Rc::as_ptr(&t.res.borrow().fds) as usize;
            (files(&t1), files(&t2))
        }
        KCMP_FS => {
            let fs = |t: &Task| Rc::as_ptr(&t.res.borrow().fs) as usize;
            (fs(&t1), fs(&t2))
        }
        KCMP_SIGHAND => (sighand(&t1), sighand(&t2)),
        // The I/O context (`Task::ioprio`, shared under CLONE_IO); two threads without one compare
        // equal, as Linux's NULL io_contexts do
        KCMP_IO => {
            let io = |t: &Task| {
                t.ioprio
                    .borrow()
                    .as_ref()
                    .map_or(0, |c| Rc::as_ptr(c) as usize)
            };
            (io(&t1), io(&t2))
        }
        // SysV semaphore undo tables are not shared between processes, so both sides count as
        // empty
        KCMP_SYSVSEM => (0, 0),
        // idx2 points at a `struct kcmp_epoll_slot { efd, tfd, toff }`: the toff-th interest for
        // fd tfd in t2's epoll
        KCMP_EPOLL_TFD => {
            let slot: [u8; 12] = super::get(cx, idx2)?;
            let word = |i: usize| u32::from_le_bytes(slot[i * 4..i * 4 + 4].try_into().unwrap());
            let fds = t2.res.borrow().fds.clone();
            let ep = fds.borrow().get(word(0) as i32)?;
            let target = match &ep.borrow().kind {
                FileKind::Epoll(e) => e.borrow().target(word(1) as i32, word(2) as usize),
                _ => return Err(Errno::EINVAL),
            };
            let target = target.ok_or(Errno::ENOENT)?;
            (file(&t1, idx1)?, Rc::as_ptr(&target) as usize)
        }
        _ => return Err(Errno::EINVAL),
    };
    Ok(match a.cmp(&b) {
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Less => 1,
        std::cmp::Ordering::Greater => 2,
    })
}

/// Whether thread `tid` (or a process, by its main thread's pid) may be accessed as with ptrace
/// (`PTRACE_MODE_ATTACH_REALCREDS`): it is in the caller's process, or [`Cred::may_ptrace`] allows
/// it with the thread's credentials ([`crate::kernel::Kernel::cred_of`]).
pub(super) fn may_ptrace(cx: &Cx<'_>, tid: Tid) -> bool {
    ptrace_access(cx, tid, false)
}

/// The same with the filesystem credentials (`PTRACE_MODE_*_FSCREDS`), as for opening a process's
/// memory views in procfs (Linux's `proc_mem_open`).
pub(super) fn may_ptrace_fs(cx: &Cx<'_>, tid: Tid) -> bool {
    ptrace_access(cx, tid, true)
}

fn ptrace_access(cx: &Cx<'_>, tid: Tid, fscreds: bool) -> bool {
    let k = cx.kr();
    let Some(group) = crate::pseudo::group_of(k, tid) else {
        return false;
    };
    let g = group.borrow();
    g.tgid == cx.tgid()
        || k.cred_of(tid)
            .is_some_and(|c| cx.cred().may_ptrace(&c, g.dump_scope(), fscreds))
}

// ---- unshare ----

/// unshare: stops sharing some resources with other threads and processes. Every namespace type
/// is supported (mount, user, pid, UTS, IPC, network, cgroup, time); the caller does not enter a
/// new pid namespace, and enters a new time namespace only at its next execve. As on Linux,
/// `CLONE_THREAD`, `CLONE_SIGHAND` and `CLONE_VM` succeed only when nothing was shared in the
/// first place (there is nothing to do).
pub(super) fn unshare(cx: &mut Cx<'_>, flags: u64) -> SysResult {
    let known = CLONE_THREAD
        | CLONE_FS
        | CLONE_SIGHAND
        | CLONE_VM
        | CLONE_FILES
        | CLONE_SYSVSEM
        | CLONE_NEW_MASK;
    let mut flags = flags;
    if flags & CLONE_NEWUSER != 0 {
        flags |= CLONE_THREAD | CLONE_FS;
    }
    if flags & CLONE_VM != 0 {
        flags |= CLONE_SIGHAND;
    }
    if flags & CLONE_SIGHAND != 0 {
        flags |= CLONE_THREAD;
    }
    if flags & CLONE_NEWNS != 0 {
        flags |= CLONE_FS;
    }
    if flags & !known != 0 {
        return Err(Errno::EINVAL);
    }
    let group = cx.t.group.clone();
    let shared = (flags & (CLONE_THREAD | CLONE_SIGHAND | CLONE_VM) != 0
        && group.borrow().threads.len() > 1)
        || (flags & (CLONE_SIGHAND | CLONE_VM) != 0
            && Rc::strong_count(&group.borrow().sighand) > 1)
        || (flags & CLONE_VM != 0 && cx.mm().shared_with_others());
    if shared {
        return Err(Errno::EINVAL);
    }
    if flags & CLONE_NEW_MASK & !CLONE_NEW_DONE != 0 {
        return Err(Errno::EPERM);
    }
    // Compute the new credentials and every check first, and change things only when all pass
    // (Linux's ksys_unshare commits only at the end)
    let mut new_cred = cx.cred();
    let euid = new_cred.euid;
    if flags & CLONE_NEWUSER != 0 {
        let chrooted = super::ns::chrooted(cx);
        super::ns::new_user_ns(cx.k(), &mut new_cred, chrooted)?;
    }
    if flags & CLONE_NEW_ADMIN != 0 && !new_cred.capable_here(cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    super::ns::check_counts(cx.k(), &counted(flags), &new_cred.user_ns, euid)?;
    // The new pid namespace is only for later children (the caller does not enter it); it cannot
    // be unshared again before it is entered
    let pid_ns = if flags & CLONE_NEWPID != 0 {
        let owner = new_cred.user_ns.clone();
        let ns = super::ns::new_pid_ns(&cx.pid_ns(), &cx.pid_ns_children(), owner)?;
        super::ns::count(cx.k(), Counted::Pid, &new_cred.user_ns, euid, ns.clone());
        Some(ns)
    } else {
        None
    };
    // Each thread's TaskState and TaskRes each hold one reference
    let others = |count: usize| count > 2;
    if flags & CLONE_FILES != 0 && others(Rc::strong_count(&cx.t.fds)) {
        // As with Linux's dup_fd: copying fails when the highest open fd (rounded up to 64)
        // exceeds nr_open
        let highest = cx.fds().open_fds().last().map_or(0, |&fd| fd as usize + 1);
        if highest.next_multiple_of(64) > cx.kr().nr_open {
            return Err(Errno::EMFILE);
        }
        let copy = cx.fds().clone();
        cx.set_fds(Rc::new(RefCell::new(copy)));
    }
    if flags & CLONE_FS != 0 && others(Rc::strong_count(&cx.t.fs)) {
        let copy = {
            let f = cx.t.fs.borrow();
            FsContext {
                root: f.root.clone(),
                cwd: f.cwd.clone(),
                umask: f.umask,
                mnt_ns: f.mnt_ns.clone(),
            }
        };
        cx.set_fs(Rc::new(RefCell::new(copy)));
    }
    if flags & CLONE_NEWUSER != 0 {
        cx.set_cred(new_cred.clone());
    }
    if flags & CLONE_NEWNS != 0 {
        let owner = new_cred.user_ns.clone();
        super::ns::new_mnt_ns(cx.t.vfs.as_deref(), &mut cx.t.fs.borrow_mut(), owner);
        let ns = cx.t.fs.borrow().mnt_ns.clone();
        super::ns::count(cx.k(), Counted::Mnt, &new_cred.user_ns, euid, ns);
        cx.shared.fast_changed();
    }
    if pid_ns.is_some() {
        *cx.task.pid_ns_children.borrow_mut() = pid_ns;
    }
    // As with Linux's exit_sem, apply and drop the SysV semaphore undos; this is also needed when
    // switching IPC namespaces (the old namespace's semaphores can no longer be reached)
    if flags & (CLONE_SYSVSEM | CLONE_NEWIPC) != 0 {
        let tgid = cx.tgid();
        cx.ipc_mut().exit(tgid);
    }
    // A new UTS namespace copies the old names, a new IPC namespace is empty, and a new network
    // namespace has only lo, down; a new time namespace is only for later children, copying the
    // offsets of the old one for children
    if flags & (CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWNET | CLONE_NEWCGROUP | CLONE_NEWTIME) != 0 {
        let owner = &new_cred.user_ns;
        let mut ns = cx.ns();
        if flags & CLONE_NEWTIME != 0 {
            ns.time_for_children = ns.time_for_children.copy(owner.clone());
            let time = ns.time_for_children.clone();
            super::ns::count(cx.k(), Counted::Time, owner, euid, time);
        }
        if flags & CLONE_NEWCGROUP != 0 {
            let root = cx.t.group.borrow().cgroup;
            ns.cgroup = super::ns::new_cgroup_ns(owner.clone(), root);
            super::ns::count(cx.k(), Counted::Cgroup, owner, euid, ns.cgroup.clone());
        }
        if flags & CLONE_NEWNET != 0 {
            ns.net = cx.k().new_net_ns(owner.clone())?;
            super::ns::count(cx.k(), Counted::Net, owner, euid, ns.net.clone());
        }
        if flags & CLONE_NEWUTS != 0 {
            ns.uts = ns.uts.copy(owner.clone());
            super::ns::count(cx.k(), Counted::Uts, owner, euid, ns.uts.clone());
        }
        if flags & CLONE_NEWIPC != 0 {
            let cred = cx.cred();
            ns.ipc = cx.k().new_ipc_ns(owner.clone(), (cred.fsuid, cred.fsgid));
            super::ns::count(cx.k(), Counted::Ipc, owner, euid, ns.ipc.clone());
        }
        *cx.task.ns.borrow_mut() = ns;
        cx.k().collect_ipc();
    }
    Ok(0)
}

/// The namespaces other than user namespaces that `CLONE_NEW*` flags create, as counted against
/// `/proc/sys/user/max_*_namespaces` ([`crate::nsproxy::Ucounts`]).
fn counted(flags: u64) -> Vec<Counted> {
    [
        (CLONE_NEWNS, Counted::Mnt),
        (CLONE_NEWPID, Counted::Pid),
        (CLONE_NEWUTS, Counted::Uts),
        (CLONE_NEWIPC, Counted::Ipc),
        (CLONE_NEWNET, Counted::Net),
        (CLONE_NEWCGROUP, Counted::Cgroup),
        (CLONE_NEWTIME, Counted::Time),
    ]
    .into_iter()
    .filter(|&(f, _)| flags & f != 0)
    .map(|(_, kind)| kind)
    .collect()
}

// ---- Process groups and sessions ----

pub(super) fn getpgid(cx: &mut Cx<'_>, pid: i32) -> SysResult {
    Ok(group_of(cx, pid)?.borrow().pgid as u64)
}

pub(super) fn getsid(cx: &mut Cx<'_>, pid: i32) -> SysResult {
    Ok(group_of(cx, pid)?.borrow().sid as u64)
}

pub(super) fn setsid(cx: &mut Cx<'_>) -> SysResult {
    let tgid = cx.tgid();
    // A process-group leader cannot create a new session
    if cx.kr().groups.values().any(|g| g.borrow().pgid == tgid) {
        return Err(Errno::EPERM);
    }
    let mut g = cx.t.group.borrow_mut();
    g.ctty = None;
    g.sid = tgid;
    g.pgid = tgid;
    Ok(tgid as u64)
}

pub(super) fn setpgid(cx: &mut Cx<'_>, pid: i32, pgid: i32) -> SysResult {
    if pgid < 0 {
        return Err(Errno::EINVAL);
    }
    let me = cx.tgid();
    let target = group_of(cx, pid)?;
    let (tgid, ppid, sid, execed) = {
        let g = target.borrow();
        (g.tgid, g.ppid, g.sid, g.execed)
    };
    let my_sid = cx.t.group.borrow().sid;
    if tgid != me && ppid != me {
        return Err(Errno::ESRCH);
    }
    if sid != my_sid {
        return Err(Errno::EPERM);
    }
    // After the child has execed, the parent can no longer change its process group
    if tgid != me && execed {
        return Err(Errno::EACCES);
    }
    // A session leader cannot change its process group
    if sid == tgid {
        return Err(Errno::EPERM);
    }
    let pgid = if pgid == 0 { tgid } else { pgid };
    // Only an existing process group in the same session can be joined, or a new one created with
    // oneself as leader
    if pgid != tgid
        && !cx
            .kr()
            .groups
            .values()
            .any(|g| g.borrow().pgid == pgid && g.borrow().sid == my_sid)
    {
        return Err(Errno::EPERM);
    }
    target.borrow_mut().pgid = pgid;
    Ok(0)
}

/// A pid argument of 0 means the caller itself.
fn group_of(cx: &Cx<'_>, pid: Tid) -> Result<Rc<RefCell<ThreadGroup>>, Errno> {
    if pid == 0 {
        return Ok(cx.t.group.clone());
    }
    cx.kr().groups.get(&pid).cloned().ok_or(Errno::ESRCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shebang_lines() {
        assert_eq!(
            parse_shebang(b"#!/bin/sh\necho"),
            Some((b"/bin/sh".to_vec(), None))
        );
        assert_eq!(
            parse_shebang(b"#! /usr/bin/env  python3 -u \nx"),
            Some((b"/usr/bin/env".to_vec(), Some(b"python3 -u".to_vec())))
        );
        assert_eq!(parse_shebang(b"#!\n"), None);
        assert_eq!(parse_shebang(b"\x7fELF"), None);
    }

    #[test]
    fn wait_status_encoding() {
        assert_eq!(ExitStatus::Exited(3).wait_status(), 0x300);
        let killed = ExitStatus::Signaled {
            signal: Signal::TERM,
            pc: None,
            fault_addr: None,
            core_dumped: false,
        };
        assert_eq!(killed.wait_status(), 15);
    }
}
