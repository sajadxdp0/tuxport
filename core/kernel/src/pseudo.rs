//! Synthetic filesystems (the kernel/procfs map): procfs, devpts, sysfs (CPU topology, network
//! interfaces) and cgroup2, mounted at boot on `/proc`, `/dev/pts`, `/sys` and `/sys/fs/cgroup`.
//!
//! Nodes are [`PseudoNode`]s, and the directory structure implements [`Pseudo`] through [`View`]
//! (a read-only view of kernel state taken while holding the BKL). A procfs file generates its
//! complete contents when opened ([`content`]), and later reads all work on that snapshot, with
//! the same effect as a Linux seq_file read in one go.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use tuxport_engine::PAGE_SIZE;

use crate::cell::RefCell;
use crate::cred::{Cred, cap};
use crate::errno::Errno;
use crate::fd::{FileKind, OpenFile, oflags};
use crate::ipc;
use crate::kernel::{INIT, Kernel, Tid};
use crate::mm::Region;
use crate::net::{AF_INET, AF_INET6, Table};
use crate::pidns::PidNs;
use crate::process::{Signal, ThreadGroup};
use crate::signal::SigAction;
use crate::syscall::SocketRef;
use crate::task::Task;
use crate::tty::{PTS_MAJOR, Tty};
use crate::uaccess::UserSlice;
use crate::vfs::dev::rdev;
use crate::vfs::store::{DirEntry, Ino, Stat, Store, mode};
use crate::vfs::{Fs, MountId, Node, Pseudo, Vfs};

/// Global files under `/proc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Global {
    Cpuinfo,
    Meminfo,
    Stat,
    Uptime,
    Loadavg,
    Version,
    Filesystems,
    Cmdline,
    /// A symlink to `self/mounts`.
    Mounts,
    /// A symlink to `self/net`.
    Net,
    /// cgroup controllers ([`crate::cgroup`]).
    Cgroups,
    /// Memory counters, the host's when it can be read.
    Vmstat,
    /// Swap areas: the host's when it can be read, else none.
    Swaps,
    /// File locks and leases ([`crate::locks::proc_locks`]).
    Locks,
    /// syslog reading ([`crate::klog`]): the contents are not generated at open but taken at read
    /// time.
    Kmsg,
}

const GLOBALS: [(&[u8], Global); 15] = [
    (b"cpuinfo", Global::Cpuinfo),
    (b"meminfo", Global::Meminfo),
    (b"stat", Global::Stat),
    (b"uptime", Global::Uptime),
    (b"loadavg", Global::Loadavg),
    (b"version", Global::Version),
    (b"filesystems", Global::Filesystems),
    (b"cmdline", Global::Cmdline),
    (b"mounts", Global::Mounts),
    (b"net", Global::Net),
    (b"cgroups", Global::Cgroups),
    (b"kmsg", Global::Kmsg),
    (b"vmstat", Global::Vmstat),
    (b"swaps", Global::Swaps),
    (b"locks", Global::Locks),
];

/// Entries under `/proc/<pid>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Entry {
    Auxv,
    Stat,
    Status,
    Statm,
    Cmdline,
    Comm,
    Environ,
    Limits,
    /// Child processes the thread created (only under `task/<tid>/`, Linux's
    /// `CONFIG_PROC_CHILDREN`).
    Children,
    Maps,
    Mem,
    Smaps,
    SmapsRollup,
    Pagemap,
    Mounts,
    MountInfo,
    Cwd,
    Exe,
    Root,
    Fd,
    FdInfo,
    Task,
    /// Files of the process's network namespace ([`PROC_NET`]).
    Net,
    /// The OOM killer's score ([`crate::oom::proc_score`]).
    OomScore,
    OomScoreAdj,
    CoredumpFilter,
    /// The cgroup it is in ([`crate::cgroup`]).
    Cgroup,
    /// The namespace directory ([`NsKind`]).
    Ns,
    /// The user namespace's ID maps (user_namespaces(7)): `uid_map`, `gid_map`, `projid_map`,
    /// and `setgroups`.
    UidMap,
    GidMap,
    ProjidMap,
    Setgroups,
    /// The time namespace offsets for children ([`crate::nsproxy::TimeNs`]), only under
    /// `/proc/<pid>`.
    TimensOffsets,
    /// The syscall the thread is waiting in ([`crate::task::KernelEntry`]).
    Syscall,
}

/// Namespace kinds (the entries in `/proc/<pid>/ns`); every kind can be created by clone or
/// unshare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum NsKind {
    Cgroup,
    Ipc,
    Mnt,
    Net,
    Pid,
    PidForChildren,
    Time,
    TimeForChildren,
    User,
    Uts,
}

impl NsKind {
    /// Sorted by name (the listing of `/proc/<pid>/ns`).
    pub(crate) const ALL: [NsKind; 10] = [
        NsKind::Cgroup,
        NsKind::Ipc,
        NsKind::Mnt,
        NsKind::Net,
        NsKind::Pid,
        NsKind::PidForChildren,
        NsKind::Time,
        NsKind::TimeForChildren,
        NsKind::User,
        NsKind::Uts,
    ];

    /// The name in `/proc/<pid>/ns`.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            NsKind::Cgroup => "cgroup",
            NsKind::Ipc => "ipc",
            NsKind::Mnt => "mnt",
            NsKind::Net => "net",
            NsKind::Pid => "pid",
            NsKind::PidForChildren => "pid_for_children",
            NsKind::Time => "time",
            NsKind::TimeForChildren => "time_for_children",
            NsKind::User => "user",
            NsKind::Uts => "uts",
        }
    }

    /// The type in the link contents (`*_for_children` is the same as its counterpart).
    pub(crate) const fn type_name(self) -> &'static str {
        match self {
            NsKind::PidForChildren => "pid",
            NsKind::TimeForChildren => "time",
            k => k.name(),
        }
    }

    /// setns's `nstype` (`CLONE_NEW*`).
    pub(crate) const fn clone_flag(self) -> u64 {
        match self {
            NsKind::Cgroup => 0x0200_0000,
            NsKind::Ipc => 0x0800_0000,
            NsKind::Mnt => 0x0002_0000,
            NsKind::Net => 0x4000_0000,
            NsKind::Pid | NsKind::PidForChildren => 0x2000_0000,
            NsKind::Time | NsKind::TimeForChildren => 0x80,
            NsKind::User => 0x1000_0000,
            NsKind::Uts => 0x0400_0000,
        }
    }

    /// The id (inode number) of this kind of namespace of thread `task`: for mount namespaces by
    /// its `FsContext`, for user namespaces by its credentials, for pid namespaces by its process
    /// and thread, and for the others by its [`crate::nsproxy::NsProxy`]. None when there is none
    /// (ENOENT).
    pub(crate) fn id_of(self, k: &Kernel, task: &Task) -> Option<u64> {
        let group = k.groups.get(&task.tgid);
        Some(match self {
            NsKind::Mnt => task.res.borrow().fs.borrow().mnt_ns.id,
            NsKind::User => group.map_or(crate::cred::INIT_USER_NS, |g| g.borrow().cred.user_ns.id),
            NsKind::Cgroup => task.ns.borrow().cgroup.id,
            NsKind::Ipc => task.ns.borrow().ipc.id,
            NsKind::Net => task.ns.borrow().net.id,
            NsKind::Pid => k.pid_ns_of(task.tgid).id,
            // None after unshare until a child is created (Linux's pidns_for_children_get looks
            // at child_reaper)
            NsKind::PidForChildren => {
                let ns = task.pid_ns_children.borrow().clone();
                let ns = ns.unwrap_or_else(|| k.pid_ns_of(task.tgid));
                if ns.level > 0 && ns.reaper.get().is_none() {
                    return None;
                }
                ns.id
            }
            NsKind::Time => task.ns.borrow().time.id,
            NsKind::TimeForChildren => task.ns.borrow().time_for_children.id,
            NsKind::Uts => task.ns.borrow().uts.id,
        })
    }
}

const ENTRIES: [(&[u8], Entry); 34] = [
    (b"auxv", Entry::Auxv),
    (b"stat", Entry::Stat),
    (b"status", Entry::Status),
    (b"statm", Entry::Statm),
    (b"cmdline", Entry::Cmdline),
    (b"comm", Entry::Comm),
    (b"syscall", Entry::Syscall),
    (b"environ", Entry::Environ),
    (b"limits", Entry::Limits),
    (b"children", Entry::Children),
    (b"maps", Entry::Maps),
    (b"mem", Entry::Mem),
    (b"smaps", Entry::Smaps),
    (b"smaps_rollup", Entry::SmapsRollup),
    (b"pagemap", Entry::Pagemap),
    (b"mounts", Entry::Mounts),
    (b"mountinfo", Entry::MountInfo),
    (b"cwd", Entry::Cwd),
    (b"exe", Entry::Exe),
    (b"root", Entry::Root),
    (b"fd", Entry::Fd),
    (b"fdinfo", Entry::FdInfo),
    (b"task", Entry::Task),
    (b"net", Entry::Net),
    (b"oom_score", Entry::OomScore),
    (b"oom_score_adj", Entry::OomScoreAdj),
    (b"coredump_filter", Entry::CoredumpFilter),
    (b"cgroup", Entry::Cgroup),
    (b"ns", Entry::Ns),
    (b"uid_map", Entry::UidMap),
    (b"gid_map", Entry::GidMap),
    (b"projid_map", Entry::ProjidMap),
    (b"setgroups", Entry::Setgroups),
    (b"timens_offsets", Entry::TimensOffsets),
];

/// The files under `/proc/<pid>/net`. As on Linux, `/proc/net` is a link to `self/net`.
const PROC_NET: [&[u8]; 18] = [
    b"arp",
    b"dev",
    b"icmp",
    b"icmp6",
    b"if_inet6",
    b"ipv6_route",
    b"netstat",
    b"raw",
    b"raw6",
    b"route",
    b"snmp",
    b"sockstat",
    b"sockstat6",
    b"tcp",
    b"tcp6",
    b"udp",
    b"udp6",
    b"unix",
];

/// The fixed directories and files under `/proc/sys` and `/proc/sysvipc` (paths relative to
/// `/proc`, directories ending in `/`). Those under `sys/net` follow the reader's network namespace
/// (as on Linux), and the directories under `ipv4/conf` and `ipv6/conf` are
/// [`PseudoNode::NetConf`].
const STATIC: [&[u8]; 129] = [
    b"sys/",
    b"sys/debug/",
    b"sys/debug/exception-trace",
    b"sys/fs/",
    b"sys/fs/aio-max-nr",
    b"sys/fs/aio-nr",
    b"sys/fs/file-max",
    b"sys/fs/inotify/",
    b"sys/fs/inotify/max_queued_events",
    b"sys/fs/inotify/max_user_instances",
    b"sys/fs/inotify/max_user_watches",
    b"sys/fs/lease-break-time",
    b"sys/fs/mount-max",
    b"sys/fs/mqueue/",
    b"sys/fs/mqueue/msg_default",
    b"sys/fs/mqueue/msg_max",
    b"sys/fs/mqueue/msgsize_default",
    b"sys/fs/mqueue/msgsize_max",
    b"sys/fs/mqueue/queues_max",
    b"sys/fs/nr_open",
    b"sys/fs/overflowgid",
    b"sys/fs/overflowuid",
    b"sys/fs/pipe-max-size",
    b"sys/fs/pipe-user-pages-hard",
    b"sys/fs/pipe-user-pages-soft",
    b"sys/fs/protected_hardlinks",
    b"sys/fs/protected_symlinks",
    b"sys/fs/suid_dumpable",
    b"sys/kernel/",
    b"sys/kernel/cap_last_cap",
    b"sys/kernel/core_pattern",
    b"sys/kernel/core_pipe_limit",
    b"sys/kernel/core_uses_pid",
    b"sys/kernel/dmesg_restrict",
    b"sys/kernel/domainname",
    b"sys/kernel/hostname",
    b"sys/kernel/hung_task_all_cpu_backtrace",
    b"sys/kernel/hung_task_check_count",
    b"sys/kernel/hung_task_check_interval_secs",
    b"sys/kernel/hung_task_panic",
    b"sys/kernel/hung_task_timeout_secs",
    b"sys/kernel/hung_task_warnings",
    b"sys/kernel/kptr_restrict",
    b"sys/kernel/modprobe",
    b"sys/kernel/modules_disabled",
    b"sys/kernel/msgmax",
    b"sys/kernel/msgmnb",
    b"sys/kernel/msgmni",
    b"sys/kernel/ngroups_max",
    b"sys/kernel/ns_last_pid",
    b"sys/kernel/osrelease",
    b"sys/kernel/ostype",
    b"sys/kernel/overflowgid",
    b"sys/kernel/overflowuid",
    b"sys/kernel/panic",
    b"sys/kernel/panic_on_oops",
    b"sys/kernel/perf_cpu_time_max_percent",
    b"sys/kernel/perf_event_max_contexts_per_stack",
    b"sys/kernel/perf_event_max_sample_rate",
    b"sys/kernel/perf_event_max_stack",
    b"sys/kernel/perf_event_mlock_kb",
    b"sys/kernel/perf_event_paranoid",
    b"sys/kernel/pid_max",
    b"sys/kernel/printk",
    b"sys/kernel/printk_devkmsg",
    b"sys/kernel/random/",
    b"sys/kernel/random/boot_id",
    b"sys/kernel/random/entropy_avail",
    b"sys/kernel/random/poolsize",
    b"sys/kernel/random/uuid",
    b"sys/kernel/randomize_va_space",
    b"sys/kernel/sched_rr_timeslice_ms",
    b"sys/kernel/sched_rt_period_us",
    b"sys/kernel/sched_rt_runtime_us",
    b"sys/kernel/sched_schedstats",
    b"sys/kernel/seccomp/",
    b"sys/kernel/seccomp/actions_avail",
    b"sys/kernel/seccomp/actions_logged",
    b"sys/kernel/sem",
    b"sys/kernel/shmall",
    b"sys/kernel/shmmax",
    b"sys/kernel/shmmni",
    b"sys/kernel/sysrq",
    b"sys/kernel/tainted",
    b"sys/kernel/threads-max",
    b"sys/kernel/unprivileged_bpf_disabled",
    b"sys/kernel/version",
    b"sys/net/",
    b"sys/net/core/",
    b"sys/net/core/somaxconn",
    b"sys/net/ipv4/",
    b"sys/net/ipv4/conf/",
    b"sys/net/ipv4/ip_forward",
    b"sys/net/ipv4/ip_local_port_range",
    b"sys/net/ipv4/ip_unprivileged_port_start",
    b"sys/net/ipv4/ping_group_range",
    b"sys/net/ipv6/",
    b"sys/net/ipv6/conf/",
    b"sys/net/unix/",
    b"sys/user/",
    b"sys/user/max_cgroup_namespaces",
    b"sys/user/max_ipc_namespaces",
    b"sys/user/max_mnt_namespaces",
    b"sys/user/max_net_namespaces",
    b"sys/user/max_pid_namespaces",
    b"sys/user/max_time_namespaces",
    b"sys/user/max_user_namespaces",
    b"sys/user/max_uts_namespaces",
    b"sys/vm/",
    b"sys/vm/dirty_background_bytes",
    b"sys/vm/dirty_background_ratio",
    b"sys/vm/dirty_bytes",
    b"sys/vm/dirty_expire_centisecs",
    b"sys/vm/dirty_ratio",
    b"sys/vm/dirty_writeback_centisecs",
    b"sys/vm/drop_caches",
    b"sys/vm/max_map_count",
    b"sys/vm/mmap_min_addr",
    b"sys/vm/mmap_rnd_bits",
    b"sys/vm/mmap_rnd_compat_bits",
    b"sys/vm/overcommit_memory",
    b"sys/vm/overcommit_ratio",
    b"sys/vm/page-cluster",
    b"sys/vm/swtuxportss",
    b"sys/vm/watermark_scale_factor",
    b"sysvipc/",
    b"sysvipc/msg",
    b"sysvipc/sem",
    b"sysvipc/shm",
];

/// The sysctls procfs only records, so that `sysctl -p` setups, benchmark scripts and checkers find
/// them: (path relative to `/proc`, mode, default, lowest and highest value accepted), the range
/// being that of Linux 6.1's handler (any int for `proc_dointvec`). A write keeps the value in
/// `Kernel::sysctls` and changes nothing else, except `debug.exception-trace`, which
/// crate::signal reads ([`recorded_value`]); `kernel.randomize_va_space` starts at 0 because
/// Tuxport places mappings at fixed addresses, and `debug.exception-trace` at arm64's 0
/// (`show_unhandled_signals`; x86 starts at 1).
pub(crate) const RECORDED: [(&[u8], u32, i64, i64, i64); 40] = {
    const INT_MIN: i64 = i32::MIN as i64;
    const INT_MAX: i64 = i32::MAX as i64;
    // LONG_MAX / HZ for the hung-task times (HZ 250, as on distribution arm64 kernels)
    const HUNG_MAX: i64 = i64::MAX / 250;
    [
        (b"sys/debug/exception-trace", 0o644, 0, INT_MIN, INT_MAX),
        (b"sys/fs/overflowgid", 0o644, 65534, 0, 65535),
        (b"sys/fs/overflowuid", 0o644, 65534, 0, 65535),
        (b"sys/fs/suid_dumpable", 0o644, 0, 0, 2),
        (b"sys/kernel/core_pipe_limit", 0o644, 0, INT_MIN, INT_MAX),
        (b"sys/kernel/hung_task_all_cpu_backtrace", 0o644, 0, 0, 1),
        (
            b"sys/kernel/hung_task_check_count",
            0o644,
            4194304,
            0,
            4194304,
        ),
        (
            b"sys/kernel/hung_task_check_interval_secs",
            0o644,
            0,
            0,
            HUNG_MAX,
        ),
        (b"sys/kernel/hung_task_panic", 0o644, 0, 0, 1),
        (
            b"sys/kernel/hung_task_timeout_secs",
            0o644,
            120,
            0,
            HUNG_MAX,
        ),
        (b"sys/kernel/hung_task_warnings", 0o644, 10, -1, INT_MAX),
        // Writing also needs CAP_SYS_ADMIN (proc_dointvec_minmax_sysadmin)
        (b"sys/kernel/kptr_restrict", 0o644, 0, 0, 2),
        // Only the change from 0 to 1 can be written
        (b"sys/kernel/modules_disabled", 0o644, 0, 1, 1),
        (b"sys/kernel/overflowgid", 0o644, 65534, 0, 65535),
        (b"sys/kernel/overflowuid", 0o644, 65534, 0, 65535),
        (b"sys/kernel/panic_on_oops", 0o644, 0, INT_MIN, INT_MAX),
        (b"sys/kernel/perf_cpu_time_max_percent", 0o644, 25, 0, 100),
        (
            b"sys/kernel/perf_event_max_contexts_per_stack",
            0o644,
            8,
            0,
            65535,
        ),
        (
            b"sys/kernel/perf_event_max_sample_rate",
            0o644,
            100000,
            1,
            INT_MAX,
        ),
        (
            b"sys/kernel/perf_event_max_stack",
            0o644,
            127,
            0,
            640 * 1024,
        ),
        (
            b"sys/kernel/perf_event_mlock_kb",
            0o644,
            516,
            INT_MIN,
            INT_MAX,
        ),
        (
            b"sys/kernel/perf_event_paranoid",
            0o644,
            2,
            INT_MIN,
            INT_MAX,
        ),
        (b"sys/kernel/randomize_va_space", 0o644, 0, INT_MIN, INT_MAX),
        (b"sys/kernel/sched_rt_period_us", 0o644, 1000000, 1, INT_MAX),
        (
            b"sys/kernel/sched_rt_runtime_us",
            0o644,
            950000,
            -1,
            INT_MAX,
        ),
        (b"sys/kernel/sched_schedstats", 0o644, 0, 0, 1),
        (b"sys/kernel/sysrq", 0o644, 1, INT_MIN, INT_MAX),
        (b"sys/vm/dirty_background_bytes", 0o644, 0, 1, i64::MAX),
        (b"sys/vm/dirty_background_ratio", 0o644, 10, 0, 100),
        (b"sys/vm/dirty_bytes", 0o644, 0, 8192, i64::MAX),
        (b"sys/vm/dirty_expire_centisecs", 0o644, 3000, 0, INT_MAX),
        (b"sys/vm/dirty_ratio", 0o644, 20, 0, 100),
        (
            b"sys/vm/dirty_writeback_centisecs",
            0o644,
            500,
            INT_MIN,
            INT_MAX,
        ),
        // Write-only; the host keeps the page cache, and FUSE nodes are forgotten (write)
        (b"sys/vm/drop_caches", 0o200, 0, 1, 4),
        (b"sys/vm/mmap_rnd_bits", 0o600, 18, 18, 33),
        (b"sys/vm/mmap_rnd_compat_bits", 0o600, 11, 11, 16),
        (b"sys/vm/overcommit_ratio", 0o644, 50, INT_MIN, INT_MAX),
        (b"sys/vm/page-cluster", 0o644, 3, 0, 31),
        (b"sys/vm/swtuxportss", 0o644, 60, 0, 200),
        (b"sys/vm/watermark_scale_factor", 0o644, 10, 1, 3000),
    ]
};

/// The [`RECORDED`] entry of a path.
fn recorded(path: &[u8]) -> Option<&'static (&'static [u8], u32, i64, i64, i64)> {
    RECORDED.iter().find(|e| e.0 == path)
}

/// The current value of a [`RECORDED`] sysctl: the one written last, or its default.
pub(crate) fn recorded_value(k: &Kernel, path: &[u8]) -> i64 {
    let (path, _, default, ..) = *recorded(path).expect("a recorded sysctl");
    k.sysctls.get(path).copied().unwrap_or(default)
}

/// The fixed directories and files under `/sys` (paths relative to `/sys`, directories ending in
/// `/`). CPU topology: busybox's `nproc --all`, lscpu and glibc's get_nprocs read them, and each
/// `cpu<n>` is a [`PseudoNode::SysCpu`]; network interfaces (btop reads the traffic counters),
/// each interface a [`PseudoNode::SysNet`]; overlayfs's module parameters (containerd adds
/// `index=off` when `index` exists, BuildKit `redirect_dir=off` when `redirect_dir` does).
const SYS_STATIC: [&[u8]; 23] = [
    b"block/",
    b"class/",
    b"class/net/",
    b"devices/",
    b"devices/system/",
    b"devices/system/cpu/",
    b"devices/system/cpu/offline",
    b"devices/system/cpu/online",
    b"devices/system/cpu/possible",
    b"devices/system/cpu/present",
    b"fs/",
    b"fs/cgroup/",
    b"module/",
    b"module/overlay/",
    b"module/overlay/parameters/",
    b"module/overlay/parameters/check_copy_up",
    b"module/overlay/parameters/index",
    b"module/overlay/parameters/metacopy",
    b"module/overlay/parameters/nfs_export",
    b"module/overlay/parameters/redirect_always_follow",
    b"module/overlay/parameters/redirect_dir",
    b"module/overlay/parameters/redirect_max",
    b"module/overlay/parameters/xino_auto",
];

/// `/sys/class/net`: the interfaces are under it.
const SYS_NET_DIR: &[u8] = b"class/net/";

/// `/sys/block`: the loop devices are under it.
const SYS_BLOCK_DIR: &[u8] = b"block/";

/// The directories and files under `/sys/block/loop<n>` (paths relative to the device directory),
/// with values from [`crate::syscall::Loops::sysfs_read`]; `loop/` exists only while the device is
/// bound. On Linux the device directories are symlinks into `/sys/devices/virtual/block`; here
/// they are directories, as for `/sys/class/net`.
const BLOCK_FILES: [&[u8]; 22] = [
    b"dev",
    b"removable",
    b"ro",
    b"size",
    b"loop/",
    b"loop/autoclear",
    b"loop/backing_file",
    b"loop/dio",
    b"loop/offset",
    b"loop/partscan",
    b"loop/sizelimit",
    b"queue/",
    b"queue/dma_alignment",
    b"queue/hw_sector_size",
    b"queue/logical_block_size",
    b"queue/max_sectors_kb",
    b"queue/minimum_io_size",
    b"queue/optimal_io_size",
    b"queue/physical_block_size",
    b"queue/read_ahead_kb",
    b"queue/rotational",
    b"queue/rq_affinity",
];

/// The directories and files under `/sys/class/net/<interface>` (paths relative to the interface
/// directory), with values from [`net_file`]. On Linux the interface directories are symlinks into
/// `/sys/devices`; here they are directories: the device paths on the host are not available.
const NET_FILES: [&[u8]; 36] = [
    b"addr_len",
    b"address",
    b"broadcast",
    b"carrier",
    b"flags",
    b"ifindex",
    b"iflink",
    b"mtu",
    b"operstate",
    b"tx_queue_len",
    b"type",
    b"statistics/",
    b"statistics/collisions",
    b"statistics/multicast",
    b"statistics/rx_bytes",
    b"statistics/rx_compressed",
    b"statistics/rx_crc_errors",
    b"statistics/rx_dropped",
    b"statistics/rx_errors",
    b"statistics/rx_fifo_errors",
    b"statistics/rx_frame_errors",
    b"statistics/rx_length_errors",
    b"statistics/rx_missed_errors",
    b"statistics/rx_nohandler",
    b"statistics/rx_over_errors",
    b"statistics/rx_packets",
    b"statistics/tx_aborted_errors",
    b"statistics/tx_bytes",
    b"statistics/tx_carrier_errors",
    b"statistics/tx_compressed",
    b"statistics/tx_dropped",
    b"statistics/tx_errors",
    b"statistics/tx_fifo_errors",
    b"statistics/tx_heartbeat_errors",
    b"statistics/tx_packets",
    b"statistics/tx_window_errors",
];

/// The extra ones under a bridge's `/sys/class/net/<interface>` (`brif/<port>` is a
/// [`PseudoNode::SysBrif`]). dockerd reads `bridge/stp_state` and the ports' `state`, waiting for
/// the ports to reach the forwarding state.
const BRIDGE_FILES: [&[u8]; 10] = [
    b"bridge/",
    b"bridge/ageing_time",
    b"bridge/bridge_id",
    b"bridge/forward_delay",
    b"bridge/hello_time",
    b"bridge/max_age",
    b"bridge/priority",
    b"bridge/root_id",
    b"bridge/stp_state",
    b"brif/",
];

/// The extra ones under a bridge port's `/sys/class/net/<interface>`.
const PORT_FILES: [&[u8]; 4] = [
    b"brport/",
    b"brport/path_cost",
    b"brport/priority",
    b"brport/state",
];

/// `/sys/devices/system/cpu`: each `cpu<n>` is under it.
const SYS_CPU_DIR: &[u8] = b"devices/system/cpu/";

/// The directories and files under `/sys/devices/system/cpu/cpu<n>` (paths relative to
/// `cpu<n>`).
const CPU_FILES: [&[u8]; 19] = [
    b"online",
    b"regs/",
    b"regs/identification/",
    b"regs/identification/midr_el1",
    b"regs/identification/revidr_el1",
    b"topology/",
    b"topology/cluster_cpus",
    b"topology/cluster_cpus_list",
    b"topology/cluster_id",
    b"topology/core_cpus",
    b"topology/core_cpus_list",
    b"topology/core_id",
    b"topology/core_siblings",
    b"topology/core_siblings_list",
    b"topology/package_cpus",
    b"topology/package_cpus_list",
    b"topology/physical_package_id",
    b"topology/thread_siblings",
    b"topology/thread_siblings_list",
];

/// The directories and files under `/sys/devices/system/cpu/cpu<n>` copied from the host: caches
/// (`cache/`) and frequencies (`cpufreq/`). They exist only when the corresponding host CPU has
/// them ([`host_cpu_has`]); the contents are the host's too, with the CPU numbers inside changed
/// to the guest's. lscpu, hwloc and OpenBLAS read the cache sizes, and Node.js's `os.cpus()`
/// reads `cpufreq/scaling_cur_freq`; on hosts without cpufreq (virtual machines) they are absent,
/// as on Linux.
const HOST_CPU_FILES: [&[u8]; 68] = [
    b"cache/",
    b"cache/index0/",
    b"cache/index0/allocation_policy",
    b"cache/index0/coherency_line_size",
    b"cache/index0/id",
    b"cache/index0/level",
    b"cache/index0/number_of_sets",
    b"cache/index0/physical_line_partition",
    b"cache/index0/shared_cpu_list",
    b"cache/index0/shared_cpu_map",
    b"cache/index0/size",
    b"cache/index0/type",
    b"cache/index0/ways_of_associativity",
    b"cache/index0/write_policy",
    b"cache/index1/",
    b"cache/index1/allocation_policy",
    b"cache/index1/coherency_line_size",
    b"cache/index1/id",
    b"cache/index1/level",
    b"cache/index1/number_of_sets",
    b"cache/index1/physical_line_partition",
    b"cache/index1/shared_cpu_list",
    b"cache/index1/shared_cpu_map",
    b"cache/index1/size",
    b"cache/index1/type",
    b"cache/index1/ways_of_associativity",
    b"cache/index1/write_policy",
    b"cache/index2/",
    b"cache/index2/allocation_policy",
    b"cache/index2/coherency_line_size",
    b"cache/index2/id",
    b"cache/index2/level",
    b"cache/index2/number_of_sets",
    b"cache/index2/physical_line_partition",
    b"cache/index2/shared_cpu_list",
    b"cache/index2/shared_cpu_map",
    b"cache/index2/size",
    b"cache/index2/type",
    b"cache/index2/ways_of_associativity",
    b"cache/index2/write_policy",
    b"cache/index3/",
    b"cache/index3/allocation_policy",
    b"cache/index3/coherency_line_size",
    b"cache/index3/id",
    b"cache/index3/level",
    b"cache/index3/number_of_sets",
    b"cache/index3/physical_line_partition",
    b"cache/index3/shared_cpu_list",
    b"cache/index3/shared_cpu_map",
    b"cache/index3/size",
    b"cache/index3/type",
    b"cache/index3/ways_of_associativity",
    b"cache/index3/write_policy",
    b"cpufreq/",
    b"cpufreq/affected_cpus",
    b"cpufreq/cpuinfo_cur_freq",
    b"cpufreq/cpuinfo_max_freq",
    b"cpufreq/cpuinfo_min_freq",
    b"cpufreq/cpuinfo_transition_latency",
    b"cpufreq/related_cpus",
    b"cpufreq/scaling_available_frequencies",
    b"cpufreq/scaling_available_governors",
    b"cpufreq/scaling_cur_freq",
    b"cpufreq/scaling_driver",
    b"cpufreq/scaling_governor",
    b"cpufreq/scaling_max_freq",
    b"cpufreq/scaling_min_freq",
    b"cpufreq/scaling_setspeed",
];

/// A node in a synthetic filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum PseudoNode {
    /// The root directory of a devpts instance (its device number, [`crate::tty::PtsOptions`]),
    /// `/dev/pts`.
    PtsRoot(u64),
    /// A terminal in the instance, `/dev/pts/<n>`.
    Pts(u64, u32),
    /// The instance's ptmx, `/dev/pts/ptmx`.
    PtsPtmx(u64),
    /// `/proc`.
    ProcRoot,
    /// A global file under `/proc`.
    Global(Global),
    /// `/proc/self`.
    SelfLink,
    /// `/proc/thread-self`.
    ThreadSelf,
    /// A directory or file under `/proc/sys` or `/proc/sysvipc`, with values from [`STATIC`].
    Static(&'static [u8]),
    /// `/proc/<pid>`; with `Some(tid)` as the second item it is `/proc/<pid>/task/<tid>`.
    Pid(Tid, Option<Tid>),
    /// `/proc/<pid>/<entry>` (including those in the task subdirectories).
    Entry(Tid, Option<Tid>, Entry),
    /// `/proc/<pid>/net/<name>` (including those in the task subdirectories), with names from
    /// [`PROC_NET`].
    Net(Tid, Option<Tid>, &'static [u8]),
    /// `/proc/<pid>/fd/<n>`.
    Fd(Tid, i32),
    /// The open file reached by following `/proc/<pid>/fd/<n>` (for those not in the store:
    /// pipes, host fds).
    OpenFd(Tid, i32),
    /// The program file reached by following `/proc/<pid>/exe`, when it has been deleted and
    /// cannot be opened by inode ([`crate::process::ExeFile`]).
    OpenExe(Tid),
    /// `/proc/<pid>/fdinfo/<n>`.
    FdInfo(Tid, i32),
    /// `/sys`.
    SysRoot,
    /// A directory or file under `/sys`, with values from [`SYS_STATIC`].
    Sys(&'static [u8]),
    /// `/sys/devices/system/cpu/cpu<n>` (empty path), or a directory or file under it (paths from
    /// [`CPU_FILES`]).
    SysCpu(u32, &'static [u8]),
    /// `/sys/class/net/<interface>` (empty path), or a directory or file under it (paths from
    /// [`NET_FILES`]); interfaces are recorded by their host interface index.
    SysNet(u32, &'static [u8]),
    /// `/sys/class/net/<bridge>/brif/<port>`: a link to `../../<port>/brport` (the interface
    /// indexes of the bridge and the port).
    SysBrif(u32, u32),
    /// `/sys/block/loop<n>` (empty path), or a directory or file under it (paths from
    /// [`BLOCK_FILES`]).
    SysBlock(u32, &'static [u8]),
    /// A cgroup directory (empty name), or a file in it (names from [`crate::cgroup::FILES`]): the
    /// cgroup id and the file name.
    Cgroup(u64, &'static [u8]),
    /// `/proc/<pid>/ns/<kind>` (including those in the task subdirectories): a magic link to a
    /// namespace.
    Ns(Tid, Option<Tid>, NsKind),
    /// The namespace reached by following `/proc/<pid>/ns/<kind>` (a file in Linux's nsfs), by id.
    NsFile(NsKind, u64),
    /// The root directory of an mqueue filesystem: the POSIX message queues of an IPC namespace
    /// (by id) ([`crate::syscall::MessageQueue`]).
    MqRoot(u64),
    /// A queue in an mqueue filesystem: the IPC namespace and the queue's inode number.
    Mq(u64, u64),
    /// `/proc/sys/net/ipv4/conf/<dir>` (first item false) or `ipv6/conf/<dir>` (empty name), or an
    /// entry under it (names from the [`crate::netconf`] tables); follows the reader's network
    /// namespace ([`crate::netdev::conf_read`]).
    NetConf(bool, crate::netdev::ConfDir, &'static [u8]),
    /// A sysctl under `/proc/sys/net/` (one in [`crate::netconf::SYSCTLS`], with paths like
    /// `ipv4/tcp_keepalive_time`), following the reader's network namespace.
    NetSysctl(&'static str),
}

/// The cgroup2 mount point `fs/cgroup` in sysfs (an empty directory when nothing is mounted).
pub(crate) const CGROUP_POINT: PseudoNode = PseudoNode::Sys(b"fs/cgroup/");

/// The `ipv4/conf` and `ipv6/conf` directories.
const NET_CONF4: &[u8] = b"sys/net/ipv4/conf/";
const NET_CONF6: &[u8] = b"sys/net/ipv6/conf/";

/// The entries in `/proc/<pid>/task/<tid>`: the thread's own, and those the same as the
/// process's (debuggers read a thread's maps and mem; `oom_score_adj` there sets the process's, as
/// Linux's tid_base_stuff has it).
const TASK_ENTRIES: [Entry; 30] = [
    Entry::Stat,
    Entry::Status,
    Entry::Statm,
    Entry::Comm,
    Entry::Syscall,
    Entry::Cmdline,
    Entry::Environ,
    Entry::Limits,
    Entry::Children,
    Entry::Auxv,
    Entry::Mem,
    Entry::Maps,
    Entry::Smaps,
    Entry::SmapsRollup,
    Entry::Mounts,
    Entry::MountInfo,
    Entry::Cwd,
    Entry::Exe,
    Entry::Root,
    Entry::Fd,
    Entry::FdInfo,
    Entry::Cgroup,
    Entry::OomScore,
    Entry::OomScoreAdj,
    Entry::Net,
    Entry::Ns,
    Entry::UidMap,
    Entry::GidMap,
    Entry::ProjidMap,
    Entry::Setgroups,
];

// ---- Lookup ----

/// A read-only view of kernel state taken while holding the BKL; `tid`/`tgid` is the thread
/// `self` points to.
pub(crate) struct View<'a> {
    pub k: &'a Kernel,
    pub vfs: &'a Vfs,
    pub tid: Tid,
    pub tgid: Tid,
    /// Credentials to read with instead of the process's: an open file's opener's, when its
    /// content is generated again ([`crate::fd::FileKind::Proc`]).
    pub cred: Option<Rc<Cred>>,
}

/// The view used before the kernel has started (resolving the first program's path): the
/// synthetic filesystems have only empty root directories.
pub(crate) struct Offline;

impl Pseudo for Offline {
    fn parent(&self, _: &PseudoNode) -> Option<PseudoNode> {
        None
    }
    fn name(&self, _: MountId, _: &PseudoNode) -> Vec<u8> {
        Vec::new()
    }
    fn lookup(&self, _: MountId, _: &PseudoNode, _: &[u8]) -> Result<PseudoNode, Errno> {
        Err(Errno::ENOENT)
    }
    fn kind(&self, _: &PseudoNode) -> Result<u32, Errno> {
        Ok(mode::S_IFDIR)
    }
    fn readlink(&self, _: MountId, _: &PseudoNode) -> Result<Vec<u8>, Errno> {
        Err(Errno::EINVAL)
    }
    fn follow(&self, _: MountId, _: &PseudoNode) -> Option<Result<Node, Errno>> {
        None
    }
}

impl<'a> View<'a> {
    /// The reader's root directory (the thread `self` points to): paths in procfs are shown
    /// relative to it.
    fn root(&self) -> Node {
        self.k
            .tasks
            .get(&self.tid)
            .map_or(Node::ROOT, |t| t.res.borrow().fs.borrow().root.clone())
    }

    fn group(&self, pid: Tid) -> Option<Rc<RefCell<ThreadGroup>>> {
        group_of(self.k, pid).cloned()
    }

    /// The thread a pid directory stands for: the thread itself when `pid` is a thread id, so the
    /// main thread for a process, and none once that has exited (procfs then shows it as a
    /// zombie).
    fn leader(&self, pid: Tid) -> Option<Rc<Task>> {
        let g = self.group(pid)?;
        let threads = g.borrow().threads.clone();
        threads
            .iter()
            .find(|&&t| t == pid)
            .and_then(|t| self.k.tasks.get(t).cloned())
    }

    /// The pid namespace of mount `mnt` (an instance of procfs): pids in directory names, links
    /// and files are counted in it (Linux's `proc_pid_ns`).
    pub(crate) fn pid_ns(&self, mnt: MountId) -> Rc<PidNs> {
        self.vfs.proc_pid_ns(mnt).unwrap_or_else(PidNs::init)
    }

    /// The last number allocated in the reader's pid namespace (`ns_last_pid`, `loadavg`; Linux
    /// uses the reader's namespace, not the mount's).
    fn last_pid(&self) -> Tid {
        let ns = self.k.pid_ns_of(self.tgid);
        if ns.level == 0 {
            self.k.last_pid()
        } else {
            ns.last_pid()
        }
    }

    /// The reader's UTS and IPC namespaces (of the thread `self` points to):
    /// `/proc/sys/kernel/hostname`, the System V IPC limits and `/proc/sysvipc` follow them.
    pub(crate) fn ns(&self) -> crate::nsproxy::NsProxy {
        self.k
            .tasks
            .get(&self.tid)
            .map_or_else(|| self.k.init_ns.clone(), |t| t.ns.borrow().clone())
    }

    /// The interface with index `index` in the reader's network namespace (the host's or lo, and
    /// virtual devices).
    fn interface(&self, index: u32) -> Option<tuxport_hal::net::Interface> {
        crate::netdev::interfaces(&self.ns().net)
            .into_iter()
            .find(|i| i.index == index)
    }

    /// Whether the interface with index `index` in the reader's network namespace is a bridge,
    /// and whether it is a bridge port.
    fn bridge_roles(&self, index: u32) -> (bool, bool) {
        crate::netdev::views(&self.ns().net)
            .into_iter()
            .find(|v| v.index == index)
            .map_or((false, false), |v| {
                (v.kind == crate::netdev::Kind::Bridge, v.master.is_some())
            })
    }

    /// The directory under `/proc/sys/net/ipv*/conf` still exists (the interface was not
    /// removed).
    fn conf_dir_exists(&self, dir: crate::netdev::ConfDir) -> bool {
        match dir {
            crate::netdev::ConfDir::Dev(i) => self.interface(i).is_some(),
            _ => true,
        }
    }

    /// The root of the reader's cgroup namespace (a cgroup id).
    pub(crate) fn cgroup_root(&self) -> u64 {
        self.k
            .tasks
            .get(&self.tid)
            .map_or(crate::cgroup::ROOT, |t| t.ns.borrow().cgroup.root)
    }

    /// Nanoseconds since boot `ns` plus the boottime offset of the reader's time namespace
    /// (Linux's `timens_add_boottime_ns`: `/proc/uptime`, process start times; `/proc/stat`'s
    /// `btime` subtracts it), no less than 0.
    fn with_boot_offset(&self, ns: u64) -> u64 {
        let offset = self
            .k
            .tasks
            .get(&self.tid)
            .map_or(0, |t| t.ns.borrow().time.offset(7));
        (ns as i128 + offset as i128).clamp(0, u64::MAX as i128) as u64
    }

    /// The reader's credentials (of the thread `self` points to, or the opener's given with the
    /// view): IDs in procfs are translated through its user namespace.
    pub(crate) fn reader(&self) -> Cred {
        if let Some(c) = &self.cred {
            return (**c).clone();
        }
        self.k.cred_of(self.tid).unwrap_or_else(Cred::root)
    }

    /// `/proc/<pid>/task/<tid>` uses the given thread, otherwise the representative thread.
    fn task(&self, pid: Tid, tid: Option<Tid>) -> Option<Rc<Task>> {
        match tid {
            Some(t) => {
                let tgid = self.group(pid)?.borrow().tgid;
                self.k.tasks.get(&t).cloned().filter(|t| t.tgid == tgid)
            }
            None => self.leader(pid),
        }
    }

    fn pid_exists(&self, pid: Tid, tid: Option<Tid>) -> bool {
        match (self.group(pid), tid) {
            (None, _) => false,
            (Some(_), None) => true,
            // An exited main thread stays, as a zombie, until the process is reaped
            (Some(g), Some(t)) => {
                let g = g.borrow();
                g.threads.contains(&t) || (t == g.tgid && g.leader_exit.is_some())
            }
        }
    }

    /// An open file of some process.
    pub(crate) fn open_file(&self, pid: Tid, fd: i32) -> Option<crate::fd::FileRef> {
        let task = self.leader(pid)?;
        let res = task.res.borrow();
        res.fds.borrow().get(fd).ok()
    }

    fn pty(&self, fs: u64, n: u32) -> Option<Rc<RefCell<Tty>>> {
        self.k.ptys.get(&(fs, n)).and_then(|w| w.upgrade())
    }
}

impl Pseudo for View<'_> {
    fn parent(&self, node: &PseudoNode) -> Option<PseudoNode> {
        use PseudoNode::*;
        Some(match node {
            PtsRoot(_) | ProcRoot | SysRoot | MqRoot(_) => return None,
            // A child cgroup's parent is its parent cgroup; the root has none
            Cgroup(id, b"") => Cgroup(self.k.cgroups.get(*id)?.parent?, b""),
            Mq(ns, _) => MqRoot(*ns),
            Pts(fs, _) | PtsPtmx(fs) => PtsRoot(*fs),
            Global(_) | SelfLink | ThreadSelf | Pid(_, None) => ProcRoot,
            Static(path) => tree_parent(&STATIC, path).map_or(ProcRoot, Static),
            Sys(path) => tree_parent(&SYS_STATIC, path).map_or(SysRoot, Sys),
            SysCpu(_, b"") => Sys(SYS_CPU_DIR),
            SysCpu(n, path) => tree_parent(&CPU_FILES, path)
                .or_else(|| tree_parent(&HOST_CPU_FILES, path))
                .map_or(SysCpu(*n, b""), |p| SysCpu(*n, p)),
            SysNet(_, b"") => Sys(SYS_NET_DIR),
            SysNet(n, path) => tree_parent(&NET_FILES, path)
                .or_else(|| tree_parent(&BRIDGE_FILES, path))
                .or_else(|| tree_parent(&PORT_FILES, path))
                .map_or(SysNet(*n, b""), |p| SysNet(*n, p)),
            SysBrif(n, _) => SysNet(*n, b"brif/"),
            SysBlock(_, b"") => Sys(SYS_BLOCK_DIR),
            SysBlock(n, path) => {
                tree_parent(&BLOCK_FILES, path).map_or(SysBlock(*n, b""), |p| SysBlock(*n, p))
            }
            Cgroup(id, _) => Cgroup(*id, b""),
            NetConf(v6, _, b"") => Static(if *v6 { NET_CONF6 } else { NET_CONF4 }),
            NetSysctl(path) => {
                let dir = &path[..=path.find('/').unwrap_or(0)];
                tree_lookup(&STATIC, b"sys/net/", dir.trim_end_matches('/').as_bytes())
                    .map_or(ProcRoot, Static)
            }
            NetConf(v6, dir, _) => NetConf(*v6, *dir, b""),
            Pid(pid, Some(_)) => Entry(*pid, None, self::Entry::Task),
            Entry(pid, tid, _) => Pid(*pid, *tid),
            Net(pid, tid, _) => Entry(*pid, *tid, self::Entry::Net),
            Fd(pid, _) | OpenFd(pid, _) => Entry(*pid, None, self::Entry::Fd),
            OpenExe(pid) => Pid(*pid, None),
            FdInfo(pid, _) => Entry(*pid, None, self::Entry::FdInfo),
            Ns(pid, tid, _) => Entry(*pid, *tid, self::Entry::Ns),
            NsFile(..) => return None,
        })
    }

    fn name(&self, mnt: MountId, node: &PseudoNode) -> Vec<u8> {
        use PseudoNode::*;
        match node {
            PtsRoot(_) => b"pts".to_vec(),
            ProcRoot => b"proc".to_vec(),
            Pts(_, n) => n.to_string().into_bytes(),
            PtsPtmx(_) => b"ptmx".to_vec(),
            Global(g) => GLOBALS.iter().find(|(_, x)| x == g).unwrap().0.to_vec(),
            SelfLink => b"self".to_vec(),
            ThreadSelf => b"thread-self".to_vec(),
            Static(path) | Sys(path) => tree_name(path).to_vec(),
            SysRoot => b"sys".to_vec(),
            MqRoot(_) => b"mqueue".to_vec(),
            Mq(ns, ino) => self
                .k
                .ipcs
                .get(ns)
                .and_then(|(_, ipc)| crate::syscall::mqueue_find(ipc, *ino))
                .map_or_else(|| ino.to_string().into_bytes(), |(n, _)| n.to_vec()),
            SysCpu(n, b"") => format!("cpu{n}").into_bytes(),
            SysCpu(_, path) => tree_name(path).to_vec(),
            SysNet(n, b"") => self
                .interface(*n)
                .map_or_else(|| n.to_string(), |i| i.name)
                .into_bytes(),
            NetConf(_, dir, b"") => {
                crate::netdev::conf_dir_name(&self.ns().net, *dir).unwrap_or_else(|| b"?".to_vec())
            }
            NetConf(_, _, name) => name.to_vec(),
            NetSysctl(path) => tree_name(path.as_bytes()).to_vec(),
            SysNet(_, path) => tree_name(path).to_vec(),
            SysBlock(n, b"") => format!("loop{n}").into_bytes(),
            SysBlock(_, path) => tree_name(path).to_vec(),
            SysBrif(_, p) => self
                .interface(*p)
                .map_or_else(|| p.to_string(), |i| i.name)
                .into_bytes(),
            Cgroup(crate::cgroup::ROOT, b"") => b"cgroup".to_vec(),
            Cgroup(id, b"") => self
                .k
                .cgroups
                .get(*id)
                .map_or_else(|| id.to_string().into_bytes(), |g| g.name.clone()),
            Cgroup(_, name) => name.to_vec(),
            Pid(pid, None) => self
                .pid_ns(mnt)
                .vnr(*pid)
                .unwrap_or(0)
                .to_string()
                .into_bytes(),
            Pid(_, Some(tid)) => self
                .pid_ns(mnt)
                .vnr(*tid)
                .unwrap_or(0)
                .to_string()
                .into_bytes(),
            Entry(_, _, e) => ENTRIES.iter().find(|(_, x)| x == e).unwrap().0.to_vec(),
            Net(_, _, name) => name.to_vec(),
            Fd(_, fd) | OpenFd(_, fd) | FdInfo(_, fd) => fd.to_string().into_bytes(),
            OpenExe(_) => b"exe".to_vec(),
            Ns(_, _, k) => k.name().as_bytes().to_vec(),
            NsFile(k, id) => format!("{}:[{id}]", k.type_name()).into_bytes(),
        }
    }

    fn lookup(&self, mnt: MountId, dir: &PseudoNode, name: &[u8]) -> Result<PseudoNode, Errno> {
        use PseudoNode::*;
        // fd and fdinfo of another process are open only to readers that pass the ptrace-read
        // check (Linux's 0500 directories with proc_fd_permission, and proc_fd_access_allowed)
        if let Entry(pid, _, self::Entry::Fd | self::Entry::FdInfo) = dir
            && !self.may_read(*pid)
        {
            return Err(Errno::EACCES);
        }
        // The pid in the name is a number in the mount's pid namespace
        let global = |name: &[u8]| {
            let n = Tid::try_from(parse_num(name)?).ok()?;
            self.pid_ns(mnt).find(n)
        };
        let found = match dir {
            MqRoot(ns) => self
                .k
                .ipcs
                .get(ns)
                .and_then(|(_, ipc)| ipc.mqueues.get(name))
                .map(|q| Mq(*ns, q.borrow().ino)),
            PtsRoot(fs) => {
                if name == b"ptmx" {
                    Some(PtsPtmx(*fs))
                } else {
                    parse_num(name)
                        .and_then(|n| u32::try_from(n).ok())
                        .filter(|&n| self.pty(*fs, n).is_some())
                        .map(|n| Pts(*fs, n))
                }
            }
            ProcRoot => match name {
                b"self" => Some(SelfLink),
                b"thread-self" => Some(ThreadSelf),
                _ if tree_lookup(&STATIC, b"", name).is_some() => {
                    tree_lookup(&STATIC, b"", name).map(Static)
                }
                _ => GLOBALS
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, g)| Global(*g))
                    .or_else(|| {
                        let pid = global(name)?;
                        self.pid_exists(pid, None).then_some(Pid(pid, None))
                    }),
            },
            Static(path) if *path == NET_CONF4 || *path == NET_CONF6 => {
                crate::netdev::conf_dirs(&self.ns().net)
                    .into_iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, d)| NetConf(*path == NET_CONF6, d, b""))
            }
            Static(path) => tree_lookup(&STATIC, path, name).map(Static).or_else(|| {
                let rest = path.strip_prefix(b"sys/net/")?;
                let full = [rest, name].concat();
                let init = self.ns().net.host_net.is_none();
                crate::netconf::sysctl(&full, init).map(|e| NetSysctl(e.0))
            }),
            NetConf(v6, dir, b"") => crate::netconf::table(*v6)
                .iter()
                .find(|(n, ..)| n.as_bytes() == name)
                .filter(|_| self.conf_dir_exists(*dir))
                .map(|(n, ..)| NetConf(*v6, *dir, n.as_bytes())),
            SysRoot => tree_lookup(&SYS_STATIC, b"", name).map(Sys),
            Sys(path) => tree_lookup(&SYS_STATIC, path, name).map(Sys).or_else(|| {
                if *path == SYS_NET_DIR {
                    return crate::netdev::interfaces(&self.ns().net)
                        .into_iter()
                        .find(|i| i.name.as_bytes() == name)
                        .map(|i| SysNet(i.index, b""));
                }
                if *path == SYS_BLOCK_DIR {
                    let n = name.strip_prefix(b"loop").and_then(parse_num)?;
                    let n = u32::try_from(n).ok()?;
                    return self
                        .k
                        .loops
                        .sysfs_exists(n, b"")
                        .then_some(SysBlock(n, b""));
                }
                let n = name.strip_prefix(b"cpu").and_then(parse_num)?;
                let n = u32::try_from(n).ok().filter(|&n| (n as usize) < cpus())?;
                (*path == SYS_CPU_DIR).then_some(SysCpu(n, b""))
            }),
            // A child cgroup, or a file this cgroup has
            Cgroup(id, b"") => self.k.cgroups.get(*id).and_then(|g| {
                g.children.get(name).map(|&c| Cgroup(c, b"")).or_else(|| {
                    self.k
                        .cgroups
                        .files(*id)
                        .find(|f| *f == name)
                        .map(|f| Cgroup(*id, f))
                })
            }),
            SysCpu(n, path) => tree_lookup(&CPU_FILES, path, name)
                .or_else(|| {
                    tree_lookup(&HOST_CPU_FILES, path, name).filter(|p| host_cpu_has(*n, p))
                })
                .map(|p| SysCpu(*n, p)),
            SysNet(n, b"brif/") => crate::netdev::views(&self.ns().net)
                .into_iter()
                .find(|v| v.master == Some(*n) && v.name == name)
                .map(|v| SysBrif(*n, v.index)),
            SysBlock(n, path) => tree_lookup(&BLOCK_FILES, path, name)
                .filter(|p| self.k.loops.sysfs_exists(*n, p))
                .map(|p| SysBlock(*n, p)),
            SysNet(n, path) => tree_lookup(&NET_FILES, path, name)
                .filter(|_| self.interface(*n).is_some())
                .or_else(|| {
                    let (bridge, port) = self.bridge_roles(*n);
                    tree_lookup(&BRIDGE_FILES, path, name)
                        .filter(|_| bridge)
                        .or_else(|| tree_lookup(&PORT_FILES, path, name).filter(|_| port))
                })
                .map(|p| SysNet(*n, p)),
            Pid(pid, tid) => {
                let allowed = |e: &self::Entry| in_pid_dir(*tid, e);
                ENTRIES
                    .iter()
                    .find(|(n, e)| *n == name && allowed(e))
                    .map(|(_, e)| Entry(*pid, *tid, *e))
                    .filter(|_| self.pid_exists(*pid, *tid))
            }
            // fd and fdinfo in a thread directory are the process's open files too (the fd table
            // is kept per process)
            Entry(pid, _, e @ (self::Entry::Fd | self::Entry::FdInfo)) => parse_num(name)
                .map(|fd| fd as i32)
                .filter(|&fd| self.open_file(*pid, fd).is_some())
                .map(|fd| match e {
                    self::Entry::Fd => Fd(*pid, fd),
                    _ => FdInfo(*pid, fd),
                }),
            Entry(pid, None, self::Entry::Task) => global(name)
                .filter(|&t| self.pid_exists(*pid, Some(t)))
                .map(|t| Pid(*pid, Some(t))),
            Entry(pid, tid, self::Entry::Net) => PROC_NET
                .iter()
                .find(|n| **n == name)
                .filter(|_| self.pid_exists(*pid, *tid))
                .map(|n| Net(*pid, *tid, n)),
            Entry(pid, tid, self::Entry::Ns) => NsKind::ALL
                .into_iter()
                .find(|k| k.name().as_bytes() == name)
                .filter(|_| self.pid_exists(*pid, *tid))
                .map(|k| Ns(*pid, *tid, k)),
            _ => return Err(Errno::ENOTDIR),
        };
        found.ok_or(Errno::ENOENT)
    }

    fn missing(&self, dir: &Node, name: &[u8]) {
        if let Some(path) = self.missing_path(dir, name) {
            let task = self.k.tasks.get(&self.tid).map(|t| &**t);
            crate::stub::record(self.k, task, self.tgid, crate::stub::Stub::Missing(path));
        }
    }

    fn kind(&self, node: &PseudoNode) -> Result<u32, Errno> {
        use PseudoNode::*;
        Ok(match node {
            PtsRoot(_) | ProcRoot | Pid(..) | SysRoot | MqRoot(_) => mode::S_IFDIR,
            Mq(..) => mode::S_IFREG,
            Static(p) | Sys(p) if p.ends_with(b"/") => mode::S_IFDIR,
            SysCpu(_, p) | SysNet(_, p) | SysBlock(_, p) if p.is_empty() || p.ends_with(b"/") => {
                mode::S_IFDIR
            }
            Cgroup(_, b"") => mode::S_IFDIR,
            NetSysctl(_) => mode::S_IFREG,
            NetConf(_, dir, name) => {
                if !self.conf_dir_exists(*dir) {
                    return Err(Errno::ENOENT);
                }
                if name.is_empty() {
                    mode::S_IFDIR
                } else {
                    mode::S_IFREG
                }
            }
            Entry(
                _,
                _,
                self::Entry::Fd
                | self::Entry::FdInfo
                | self::Entry::Task
                | self::Entry::Net
                | self::Entry::Ns,
            ) => mode::S_IFDIR,
            Pts(..) | PtsPtmx(_) => mode::S_IFCHR,
            SelfLink | ThreadSelf | Fd(..) | Ns(..) | SysBrif(..) => mode::S_IFLNK,
            Global(self::Global::Mounts | self::Global::Net) => mode::S_IFLNK,
            Entry(_, _, self::Entry::Cwd | self::Entry::Exe | self::Entry::Root) => mode::S_IFLNK,
            OpenFd(pid, fd) => {
                let file = self.open_file(*pid, *fd).ok_or(Errno::ENOENT)?;
                let f = file.borrow();
                match &f.kind {
                    FileKind::Pipe(_) => mode::S_IFIFO,
                    FileKind::Socket(_) => mode::S_IFSOCK,
                    FileKind::Host(fd) => {
                        tuxport_hal::fstat(*fd).map_err(Errno)?.st_mode as u32 & mode::S_IFMT
                    }
                    _ => mode::S_IFREG,
                }
            }
            _ => mode::S_IFREG,
        })
    }

    fn readlink(&self, mnt: MountId, node: &PseudoNode) -> Result<Vec<u8>, Errno> {
        use PseudoNode::*;
        // Readers not visible in the mount's pid namespace have no self or thread-self (ENOENT)
        let ns = || self.pid_ns(mnt);
        match node {
            SelfLink => Ok(ns()
                .vnr(self.tgid)
                .ok_or(Errno::ENOENT)?
                .to_string()
                .into_bytes()),
            ThreadSelf => {
                let ns = ns();
                let (tgid, tid) = (ns.vnr(self.tgid), ns.vnr(self.tid));
                let (Some(tgid), Some(tid)) = (tgid, tid) else {
                    return Err(Errno::ENOENT);
                };
                Ok(format!("{tgid}/task/{tid}").into_bytes())
            }
            Global(self::Global::Mounts) => Ok(b"self/mounts".to_vec()),
            Global(self::Global::Net) => Ok(b"self/net".to_vec()),
            Entry(pid, tid, self::Entry::Root) => {
                let task = self.task(*pid, *tid).ok_or(Errno::ENOENT)?;
                if !self.may_read(*pid) {
                    return Err(Errno::EACCES);
                }
                let root = task.res.borrow().fs.borrow().root.clone();
                self.vfs.path_of(self, &self.root(), &root)
            }
            // As with root and cwd, the reader must be able to see that process (Linux's
            // proc_pid_readlink and proc_pid_get_link both check proc_fd_access_allowed first).
            // After the program file is deleted it gets ` (deleted)`, as with fds
            Entry(pid, tid, self::Entry::Exe) => {
                let g = self.group(*pid).ok_or(Errno::ENOENT)?;
                if !self.may_read(*pid) {
                    return Err(Errno::EACCES);
                }
                // The program is the thread's address space's: an exited thread (the main thread
                // of a process living on, a zombie) has none
                if self.task(*pid, *tid).is_none() {
                    return Err(Errno::ENOENT);
                }
                let exe = g.borrow().exe.clone().ok_or(Errno::ENOENT)?;
                let path = exe.path.to_vec();
                Ok(match (exe.node, &exe.ovl) {
                    (Some((_, ino)), _) => mark_deleted(self.vfs.store(), ino, path),
                    // An overlay program whose name was removed
                    (None, Some((_, o, _))) if !self.vfs.overlay().linked(*o) => with_deleted(path),
                    _ => path,
                })
            }
            Entry(pid, tid, self::Entry::Cwd) => {
                let task = self.task(*pid, *tid).ok_or(Errno::ENOENT)?;
                if !self.may_read(*pid) {
                    return Err(Errno::EACCES);
                }
                let cwd = task.res.borrow().fs.borrow().cwd.clone();
                self.vfs.path_of(self, &self.root(), &cwd)
            }
            Fd(pid, fd) => {
                let file = self.open_file(*pid, *fd).ok_or(Errno::ENOENT)?;
                if !self.may_read(*pid) {
                    return Err(Errno::EACCES);
                }
                let f = file.borrow();
                let name = describe(&f);
                Ok(match &f.kind {
                    FileKind::Regular { ino, .. } => mark_deleted(self.vfs.store(), *ino, name),
                    _ => name,
                })
            }
            Ns(pid, tid, k) => {
                let task = self.task(*pid, *tid).ok_or(Errno::ENOENT)?;
                if !self.may_read(*pid) {
                    return Err(Errno::EACCES);
                }
                let id = k.id_of(self.k, &task).ok_or(Errno::ENOENT)?;
                Ok(self.name(mnt, &NsFile(*k, id)))
            }
            SysBrif(_, p) => {
                let i = self.interface(*p).ok_or(Errno::ENOENT)?;
                Ok(format!("../../{}/brport", i.name).into_bytes())
            }
            _ => Err(Errno::EINVAL),
        }
    }

    fn follow_dentry(&self, node: &PseudoNode) -> Option<crate::vfs::Dentry> {
        if let PseudoNode::Entry(pid, _, Entry::Exe) = node {
            return self.group(*pid)?.borrow().exe.as_ref()?.dentry.clone();
        }
        let PseudoNode::Fd(pid, fd) = node else {
            return None;
        };
        let file = self.open_file(*pid, *fd)?;
        let f = file.borrow();
        // Only files that are not directories want it (a directory's path is derived from its
        // parent)
        let dir = match &f.kind {
            FileKind::Path(n) => self.vfs.is_dir(self, n).unwrap_or(true),
            FileKind::Directory { .. } => true,
            _ => false,
        };
        f.dentry.clone().filter(|_| !dir)
    }

    /// `/proc/<pid>/fd/<n>`, `ns/*`, `root`, `cwd`, `exe` (Linux's proc_pid_get_link and
    /// proc_ns_get_link both go through nd_jump_link). `/proc/self` and `thread-self` are ordinary
    /// links.
    fn magic(&self, node: &PseudoNode) -> bool {
        matches!(
            node,
            PseudoNode::Fd(..)
                | PseudoNode::Ns(..)
                | PseudoNode::Entry(_, _, Entry::Root | Entry::Cwd | Entry::Exe)
        )
    }

    fn protected_symlinks(&self) -> bool {
        self.k.protected_symlinks
    }

    /// A cgroup directory is searched by its mode, as kernfs checks it (mkdir gives it one).
    fn searched(&self, node: &PseudoNode) -> Option<Stat> {
        matches!(node, PseudoNode::Cgroup(_, b"")).then(|| stat(self, node).ok())?
    }

    fn follow(&self, mnt: MountId, node: &PseudoNode) -> Option<Result<Node, Errno>> {
        match node {
            PseudoNode::Ns(pid, tid, k) => {
                let task = self.task(*pid, *tid)?;
                if !self.may_read(*pid) {
                    return Some(Err(Errno::EACCES));
                }
                let Some(id) = k.id_of(self.k, &task) else {
                    return Some(Err(Errno::ENOENT));
                };
                return Some(Ok(Node::Pseudo(mnt, PseudoNode::NsFile(*k, id))));
            }
            // The root and current directories point straight at that process's nodes (Linux's
            // proc_root_link, proc_cwd_link): through them one can get into other mount
            // namespaces and outside a chroot
            PseudoNode::Entry(pid, tid, e @ (Entry::Root | Entry::Cwd)) => {
                let task = self.task(*pid, *tid)?;
                if !self.may_read(*pid) {
                    return Some(Err(Errno::EACCES));
                }
                let res = task.res.borrow();
                let fs = res.fs.borrow();
                return Some(Ok(match e {
                    Entry::Root => fs.root.clone(),
                    _ => fs.cwd.clone(),
                }));
            }
            // The program file itself (Linux's proc_exe_link): still the original after being
            // renamed, deleted or replaced. A deleted one's object in the store is deleted too,
            // so it is opened through the recorded host file (OpenExe). Without a store (host
            // files for development) it is followed by the path read back
            PseudoNode::Entry(pid, tid, Entry::Exe) => {
                let g = self.group(*pid)?;
                if !self.may_read(*pid) {
                    return Some(Err(Errno::EACCES));
                }
                let exe = g.borrow().exe.clone();
                let Some(exe) = exe.filter(|_| self.task(*pid, *tid).is_some()) else {
                    return Some(Err(Errno::ENOENT));
                };
                // A program found through an overlay: its node while its name lasts. Once removed
                // it can still be run through the recorded host file (OpenExe), but not opened
                // (reopen_exe): a descriptor of the store inode behind it would change it behind
                // the overlay's back
                if let Some((m, o, _)) = &exe.ovl {
                    return Some(if self.vfs.overlay().linked(*o) {
                        Ok(Node::Overlay(*m, *o))
                    } else if exe.host.is_some() {
                        Ok(Node::Pseudo(mnt, PseudoNode::OpenExe(*pid)))
                    } else {
                        Err(Errno::ENOENT)
                    });
                }
                let (m, ino) = exe.node?;
                let deleted = self.vfs.store().attrs(ino).map_or(true, |st| st.nlink == 0);
                return Some(Ok(if deleted && exe.host.is_some() {
                    Node::Pseudo(mnt, PseudoNode::OpenExe(*pid))
                } else {
                    Node::Store(m, ino)
                }));
            }
            PseudoNode::Fd(pid, _) if !self.may_read(*pid) => return Some(Err(Errno::EACCES)),
            _ => {}
        }
        self.follow_fd(mnt, node).map(Ok)
    }
}

impl View<'_> {
    /// The path counted as unimplemented when path resolution finds no `name` in `dir`
    /// ([`Pseudo::missing`], Starnix's `not_found`): names Linux would have in the fixed parts of
    /// procfs and sysfs and in the `/dev` mounted at boot, with the numbers of processes, threads,
    /// CPUs, interfaces and loop devices replaced by placeholders. None where the absence is a
    /// fact rather than a gap: a process, thread, fd, terminal, message queue, cgroup, interface,
    /// CPU or loop device that does not exist, the files of a process that has gone, anything
    /// under a cgroup (the root and the other cgroups have different files) or outside these
    /// filesystems.
    fn missing_path(&self, dir: &Node, name: &[u8]) -> Option<String> {
        use PseudoNode::*;
        let numbered = |prefix: &[u8]| {
            name.strip_prefix(prefix)
                .is_some_and(|n| !n.is_empty() && n.iter().all(u8::is_ascii_digit))
        };
        let pid_dir = |tid: Option<Tid>| match tid {
            Some(_) => "/proc/<pid>/task/<tid>",
            None => "/proc/<pid>",
        };
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let name_text = text(name);
        let node = match dir {
            Node::Store(mnt, _) => {
                let dev = *mnt == crate::vfs::DEV_MOUNT
                    && self.vfs.mount_root(dir) == Some(crate::vfs::DEV_MOUNT);
                return dev.then(|| format!("/dev/{name_text}"));
            }
            Node::Pseudo(_, p) => p,
            Node::Overlay(..) | Node::Fuse(..) => return None,
        };
        Some(match node {
            ProcRoot if numbered(b"") => return None,
            ProcRoot => format!("/proc/{name_text}"),
            // Interfaces
            Static(path) if *path == NET_CONF4 || *path == NET_CONF6 => return None,
            Static(path) => format!("/proc/{}{name_text}", text(path)),
            NetConf(v6, _, b"") => {
                let v = if *v6 { 6 } else { 4 };
                format!("/proc/sys/net/ipv{v}/conf/<interface>/{name_text}")
            }
            Pid(pid, tid) if self.pid_exists(*pid, *tid) => {
                format!("{}/{name_text}", pid_dir(*tid))
            }
            Entry(pid, tid, self::Entry::Net) if self.pid_exists(*pid, *tid) => {
                format!("{}/net/{name_text}", pid_dir(*tid))
            }
            Entry(pid, tid, self::Entry::Ns) if self.pid_exists(*pid, *tid) => {
                format!("{}/ns/{name_text}", pid_dir(*tid))
            }
            SysRoot => format!("/sys/{name_text}"),
            // Interfaces, loop devices, CPUs
            Sys(path) if *path == SYS_NET_DIR || *path == SYS_BLOCK_DIR => return None,
            Sys(path) if *path == SYS_CPU_DIR && numbered(b"cpu") => return None,
            Sys(path) => format!("/sys/{}{name_text}", text(path)),
            SysCpu(_, path) => {
                format!("/sys/devices/system/cpu/cpu<n>/{}{name_text}", text(path))
            }
            // Bridge ports
            SysNet(_, b"brif/") => return None,
            SysNet(_, path) => format!("/sys/class/net/<interface>/{}{name_text}", text(path)),
            SysBlock(_, path) => format!("/sys/block/loop<n>/{}{name_text}", text(path)),
            _ => return None,
        })
    }

    /// Whether the reader may look at the root directory, current directory, program file and
    /// namespaces of thread or process `pid` (Linux's `ptrace_may_access(PTRACE_MODE_READ_FSCREDS)`
    /// on the thread, or the main thread).
    fn may_read(&self, pid: Tid) -> bool {
        self.may_read_as(pid, &self.reader())
    }

    /// [`View::may_read`] with the credentials `reader`.
    fn may_read_as(&self, pid: Tid, reader: &Cred) -> bool {
        self.group(pid).is_some_and(|g| {
            let g = g.borrow();
            g.tgid == self.tgid
                || self
                    .k
                    .cred_of(pid)
                    .is_some_and(|c| reader.may_ptrace(&c, g.dump_scope(), true))
        })
    }

    /// The node reached by following `/proc/<pid>/fd/<n>` ([`Pseudo::follow`]).
    fn follow_fd(&self, mnt: MountId, node: &PseudoNode) -> Option<Node> {
        let PseudoNode::Fd(pid, fd) = node else {
            return None;
        };
        let file = self.open_file(*pid, *fd)?;
        let f = file.borrow();
        // A file opened through an overlay leads back to its overlay node, never to the store
        // inode behind it (which may be in a lower layer)
        if let Some(o) = f.ovl {
            return Some(Node::Overlay(f.mnt, o));
        }
        Some(match &f.kind {
            // A deleted file cannot be reopened by inode (its object in the store is already
            // deleted): point at the open file itself, and copy the host file on reopen
            FileKind::Regular { ino, file, .. }
                if self
                    .vfs
                    .store()
                    .stat_open(*ino, file)
                    .is_ok_and(|st| st.nlink == 0) =>
            {
                Node::Pseudo(mnt, PseudoNode::OpenFd(*pid, *fd))
            }
            FileKind::Regular { ino, .. } | FileKind::Device { ino, .. } => {
                Node::Store(f.mnt, *ino)
            }
            FileKind::Directory { node, .. } | FileKind::Path(node) => node.clone(),
            FileKind::Proc { mnt, node, .. } => Node::Pseudo(*mnt, node.clone()),
            // A slave device: its path in the devpts mount it was opened through
            FileKind::Pty(end) if !end.master => {
                let t = end.tty.borrow();
                Node::Pseudo(end.mnt, PseudoNode::Pts(t.fs, t.index))
            }
            _ => Node::Pseudo(mnt, PseudoNode::OpenFd(*pid, *fd)),
        })
    }
}

/// A number naming a pid or fd directory entry (Linux's `name_to_int`): decimal digits only, no
/// sign and no leading zero; anything above `i32::MAX` names nothing (no pid or fd is that large,
/// and it must not wrap to a small one).
fn parse_num(name: &[u8]) -> Option<i64> {
    if name.is_empty()
        || name.len() > 10
        || (name.len() > 1 && name[0] == b'0')
        || !name.iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let n: i64 = std::str::from_utf8(name).ok()?.parse().ok()?;
    (n <= i64::from(i32::MAX)).then_some(n)
}

/// The target `/proc/<pid>/fd/<n>` shows: a path, or a description like `pipe:[ino]`.
pub(crate) fn describe(f: &OpenFile) -> Vec<u8> {
    if let Some(p) = &f.path {
        return p.to_vec();
    }
    match &f.kind {
        FileKind::Pipe(end) => format!("pipe:[{}]", end.pipe.borrow().ino).into_bytes(),
        FileKind::Pty(end) if end.master => b"/dev/ptmx".to_vec(),
        FileKind::Pty(end) => format!("/dev/pts/{}", end.tty.borrow().index).into_bytes(),
        FileKind::Socket(s) => format!("socket:[{}]", socket_ino(s)).into_bytes(),
        FileKind::Epoll(_) => b"anon_inode:[eventpoll]".to_vec(),
        FileKind::EventFd(_) => b"anon_inode:[eventfd]".to_vec(),
        FileKind::Inotify(_) => b"anon_inode:inotify".to_vec(),
        FileKind::Pidfd(_) => b"anon_inode:[pidfd]".to_vec(),
        FileKind::SeccompNotify(_) => b"anon_inode:seccomp notify".to_vec(),
        FileKind::Bpf(_) => b"anon_inode:bpf-prog".to_vec(),
        FileKind::TimerFd(_) => b"anon_inode:[timerfd]".to_vec(),
        FileKind::SignalFd(_) => b"anon_inode:[signalfd]".to_vec(),
        FileKind::Host(fd) => std::fs::read_link(format!("/proc/self/fd/{fd}"))
            .map(|p| {
                use std::os::unix::ffi::OsStringExt;
                p.into_os_string().into_vec()
            })
            .unwrap_or_else(|_| format!("host:[{fd}]").into_bytes()),
        _ => b"anon_inode:[unknown]".to_vec(),
    }
}

// ---- stat and readdir ----

/// The metadata of a synthetic node.
pub(crate) fn stat(ps: &View<'_>, node: &PseudoNode) -> Result<Stat, Errno> {
    // mqueue filesystem: owners, permissions and times are kept on the queues and the namespace
    match node {
        PseudoNode::MqRoot(ns) => {
            let (_, ipc) = ps.k.ipcs.get(ns).ok_or(Errno::ENOENT)?;
            let mut st = ipc.mq_dir.borrow().stat(ipc.mqueues.len());
            st.ino = ino_of(node);
            return Ok(st);
        }
        PseudoNode::Mq(ns, ino) => {
            let (_, ipc) = ps.k.ipcs.get(ns).ok_or(Errno::ENOENT)?;
            let (_, q) = crate::syscall::mqueue_find(ipc, *ino).ok_or(Errno::ENOENT)?;
            return Ok(q.borrow().stat());
        }
        _ => {}
    }
    let kind = ps.kind(node)?;
    let sysfs = matches!(fs_of(node), Fs::Sys | Fs::Cgroup);
    let perm = match (kind, node) {
        // A cgroup directory has the mode mkdir gave it
        (mode::S_IFDIR, PseudoNode::Cgroup(id, b"")) => {
            ps.k.cgroups.get(*id).ok_or(Errno::ENOENT)?.mode
        }
        (mode::S_IFDIR, _) if sysfs => 0o755,
        (_, PseudoNode::Cgroup(_, name)) if !name.is_empty() => {
            crate::cgroup::file_mode(name).unwrap_or(0o444)
        }
        (mode::S_IFDIR, _) => 0o555,
        // proc_fd_instantiate: read and search bits for a readable file, write and search for a
        // writable one (none for O_PATH)
        (_, PseudoNode::Fd(pid, fd)) => ps.open_file(*pid, *fd).map_or(0, |f| {
            let f = f.borrow();
            (if f.readable() { 0o500 } else { 0 }) | if f.writable() { 0o300 } else { 0 }
        }),
        (mode::S_IFLNK, _) => 0o777,
        (_, PseudoNode::Pts(fs, n)) => pty(ps.k, *fs, *n).map_or(0o620, |t| t.borrow().mode),
        // ptmx's permission bits follow the instance's ptmxmode
        (_, PseudoNode::PtsPtmx(fs)) => {
            ps.k.vfs
                .as_ref()
                .map_or(0o666, |v| v.devpts_options(*fs).ptmxmode)
        }
        (_, PseudoNode::Entry(_, _, Entry::Mem)) => 0o600,
        (_, PseudoNode::NetSysctl(path)) => {
            u32::from(crate::netconf::sysctl(path.as_bytes(), true).map_or(0o444, |e| e.2))
        }
        (_, PseudoNode::Static(p)) if recorded(p).is_some() => recorded(p).map_or(0o444, |e| e.1),
        (_, n) if writable(n) => 0o644,
        (_, PseudoNode::Entry(_, _, Entry::Pagemap | Entry::Auxv | Entry::Syscall)) => 0o400,
        (_, PseudoNode::Global(Global::Kmsg)) => 0o400,
        _ => 0o444,
    };
    let rdev_value = match node {
        PseudoNode::Pts(_, n) => rdev(PTS_MAJOR, *n),
        PseudoNode::PtsPtmx(_) => rdev(5, 2),
        _ => 0,
    };
    let now = crate::tty::wall_ns();
    let times = match node {
        PseudoNode::Pts(fs, n) => pty(ps.k, *fs, *n).map(|t| {
            let t = t.borrow();
            (t.atime_ns, t.mtime_ns, t.ctime_ns)
        }),
        _ => None,
    };
    let (atime_ns, mtime_ns, ctime_ns) = times.unwrap_or((now, now, now));
    // net.* sysctls belong to root of the user namespace owning the network namespace, when it
    // maps one (net_ctl_set_ownership)
    let (uid, gid) = if is_net_sysctl(node) {
        let owner = ps.ns().net.owner.clone();
        let uid = owner.uid_map.borrow().down(0).unwrap_or(0);
        let gid = owner.gid_map.borrow().down(0).unwrap_or(0);
        (uid, gid)
    } else {
        owner_of(ps.k, node)
    };
    // As in Linux's proc_task_getattr, the task directory's link count adds the number of
    // threads: Chromium's sandbox uses it to tell whether it is single-threaded, and the zygote
    // checks before fork that it is at least 3, crashing otherwise
    let nlink = match node {
        PseudoNode::Entry(pid, None, Entry::Task) => {
            2 + ps
                .group(*pid)
                .map_or(0, |g| thread_count(&g.borrow()) as u32)
        }
        _ if kind == mode::S_IFDIR => 2,
        _ => 1,
    };
    Ok(Stat {
        ino: node_ino(ps.k, node),
        mode: kind | perm,
        nlink,
        uid,
        gid,
        rdev: rdev_value,
        // sysfs files report one page, as on Linux
        size: if sysfs && kind == mode::S_IFREG {
            4096
        } else {
            0
        },
        atime_ns,
        mtime_ns,
        ctime_ns,
        blocks: None,
    })
}

/// The filesystem a node belongs to.
pub(crate) fn fs_of(node: &PseudoNode) -> Fs {
    match node {
        PseudoNode::PtsRoot(_) | PseudoNode::Pts(..) | PseudoNode::PtsPtmx(_) => Fs::Pts,
        PseudoNode::SysRoot
        | PseudoNode::Sys(_)
        | PseudoNode::SysCpu(..)
        | PseudoNode::SysNet(..)
        | PseudoNode::SysBrif(..)
        | PseudoNode::SysBlock(..) => Fs::Sys,
        PseudoNode::Cgroup(..) => Fs::Cgroup,
        PseudoNode::NsFile(..) => Fs::Nsfs,
        PseudoNode::MqRoot(_) | PseudoNode::Mq(..) => Fs::Mqueue,
        _ => Fs::Proc,
    }
}

/// For a node under `/proc/sys`, whether it is a `net.*` one of the reader's network namespace;
/// `None` for the rest of procfs. Opening one follows `proc_sys_permission`'s rules.
pub(crate) fn sysctl_kind(node: &PseudoNode) -> Option<bool> {
    match node {
        PseudoNode::Static(p) if p.starts_with(b"sys/") => Some(is_net_sysctl(node)),
        PseudoNode::NetSysctl(_) | PseudoNode::NetConf(..) => Some(true),
        _ => None,
    }
}

/// A `net.*` sysctl (or directory) of the reader's network namespace.
fn is_net_sysctl(node: &PseudoNode) -> bool {
    match node {
        PseudoNode::Static(p) => p.starts_with(b"sys/net/"),
        PseudoNode::NetSysctl(_) | PseudoNode::NetConf(..) => true,
        _ => false,
    }
}

/// A node's owner and group: under `/proc/<pid>` the effective user and group IDs of the thread
/// (`/proc/<pid>/task/<tid>`) or the main thread, for `/dev/pts/<n>` what is recorded on the
/// terminal ([`Tty::owner`]), and root for the rest.
fn owner_of(k: &Kernel, node: &PseudoNode) -> (u32, u32) {
    match node {
        // Files of a non-dumpable process belong to root in its user namespace (Linux's
        // task_dump_owner)
        PseudoNode::Pid(pid, tid) | PseudoNode::Entry(pid, tid, _) => {
            dump_owner(k, tid.unwrap_or(*pid))
        }
        PseudoNode::Fd(pid, _) => dump_owner(k, *pid),
        PseudoNode::Pts(fs, n) => pty(k, *fs, *n).map_or((0, 0), |t| t.borrow().owner),
        PseudoNode::Cgroup(id, _) => k.cgroups.get(*id).map_or((0, 0), |g| (g.uid, g.gid)),
        _ => (0, 0),
    }
}

/// The owner of thread `tid`'s procfs files (Linux's `task_dump_owner`): its effective user and
/// group IDs, root in its user namespace when the process is not dumpable.
fn dump_owner(k: &Kernel, tid: Tid) -> (u32, u32) {
    match (group_of(k, tid), k.cred_of(tid)) {
        (Some(g), Some(c)) if g.borrow().dumpable => (c.euid, c.egid),
        (Some(_), Some(c)) => (c.uid_in(0).unwrap_or(0), c.gid_in(0).unwrap_or(0)),
        _ => (0, 0),
    }
}

/// The terminal of `/dev/pts/<n>`; None once it is closed.
pub(crate) fn pty(k: &Kernel, fs: u64, n: u32) -> Option<Rc<RefCell<Tty>>> {
    k.ptys.get(&(fs, n)).and_then(std::rc::Weak::upgrade)
}

/// The contents of a directory (without `.` and `..`).
pub(crate) fn readdir(
    ps: &View<'_>,
    mnt: MountId,
    dir: &PseudoNode,
) -> Result<Vec<DirEntry>, Errno> {
    use PseudoNode::*;
    let mut nodes: Vec<PseudoNode> = Vec::new();
    let ns = ps.pid_ns(mnt);
    match dir {
        MqRoot(ipc) => {
            if let Some((_, ipc_objects)) = ps.k.ipcs.get(ipc) {
                nodes.extend(
                    ipc_objects
                        .mqueues
                        .values()
                        .map(|q| Mq(*ipc, q.borrow().ino)),
                );
            }
        }
        PtsRoot(fs) => {
            nodes.push(PtsPtmx(*fs));
            nodes.extend(
                ps.k.ptys
                    .range((*fs, 0)..=(*fs, u32::MAX))
                    .filter(|(_, w)| w.upgrade().is_some())
                    .map(|((_, n), _)| Pts(*fs, *n)),
            );
        }
        ProcRoot => {
            nodes.extend([SelfLink, ThreadSelf]);
            nodes.extend(tree_children(&STATIC, b"").map(Static));
            nodes.extend(GLOBALS.iter().map(|(_, g)| Global(*g)));
            // Processes visible in the mount's pid namespace, sorted by their numbers there
            let mut pids: Vec<(Tid, Tid)> =
                ps.k.groups
                    .keys()
                    .filter_map(|&pid| Some((ns.vnr(pid)?, pid)))
                    .collect();
            pids.sort_unstable();
            nodes.extend(pids.into_iter().map(|(_, pid)| Pid(pid, None)));
        }
        Static(path) if *path == NET_CONF4 || *path == NET_CONF6 => {
            let v6 = *path == NET_CONF6;
            nodes.extend(
                crate::netdev::conf_dirs(&ps.ns().net)
                    .into_iter()
                    .map(|(_, d)| NetConf(v6, d, b"")),
            );
        }
        Static(path) => {
            nodes.extend(tree_children(&STATIC, path).map(Static));
            // The sysctls under /proc/sys/net/core, ipv4, ipv6 and unix
            if let Some(dir) = path.strip_prefix(b"sys/net/") {
                let init = ps.ns().net.host_net.is_none();
                nodes.extend(
                    crate::netconf::SYSCTLS
                        .iter()
                        .filter(|(p, _, _, only)| {
                            (init || !only)
                                && p.as_bytes().starts_with(dir)
                                && !p.as_bytes()[dir.len()..].contains(&b'/')
                        })
                        .map(|e| NetSysctl(e.0)),
                );
            }
        }
        NetConf(v6, dir, b"") => nodes.extend(
            crate::netconf::table(*v6)
                .iter()
                .map(|(n, ..)| NetConf(*v6, *dir, n.as_bytes())),
        ),
        SysRoot => nodes.extend(tree_children(&SYS_STATIC, b"").map(Sys)),
        Sys(path) => {
            nodes.extend(tree_children(&SYS_STATIC, path).map(Sys));
            if *path == SYS_CPU_DIR {
                nodes.extend((0..cpus() as u32).map(|n| SysCpu(n, b"")));
            }
            if *path == SYS_NET_DIR {
                nodes.extend(
                    crate::netdev::interfaces(&ps.ns().net)
                        .iter()
                        .map(|i| SysNet(i.index, b"")),
                );
            }
            if *path == SYS_BLOCK_DIR {
                nodes.extend(ps.k.loops.numbers().into_iter().map(|n| SysBlock(n, b"")));
            }
        }
        SysBlock(n, path) => nodes.extend(
            tree_children(&BLOCK_FILES, path)
                .filter(|p| ps.k.loops.sysfs_exists(*n, p))
                .map(|p| SysBlock(*n, p)),
        ),
        SysNet(n, b"brif/") => nodes.extend(
            crate::netdev::views(&ps.ns().net)
                .into_iter()
                .filter(|v| v.master == Some(*n))
                .map(|v| SysBrif(*n, v.index)),
        ),
        SysNet(n, path) => {
            nodes.extend(tree_children(&NET_FILES, path).map(|p| SysNet(*n, p)));
            let (bridge, port) = ps.bridge_roles(*n);
            if bridge {
                nodes.extend(tree_children(&BRIDGE_FILES, path).map(|p| SysNet(*n, p)));
            }
            if port {
                nodes.extend(tree_children(&PORT_FILES, path).map(|p| SysNet(*n, p)));
            }
        }
        Cgroup(id, b"") => {
            nodes.extend(ps.k.cgroups.files(*id).map(|f| Cgroup(*id, f)));
            if let Some(g) = ps.k.cgroups.get(*id) {
                nodes.extend(g.children.values().map(|&c| Cgroup(c, b"")));
            }
        }
        SysCpu(n, path) => {
            nodes.extend(tree_children(&CPU_FILES, path).map(|p| SysCpu(*n, p)));
            nodes.extend(
                tree_children(&HOST_CPU_FILES, path)
                    .filter(|p| host_cpu_has(*n, p))
                    .map(|p| SysCpu(*n, p)),
            );
        }
        Entry(pid, tid, self::Entry::Net) => {
            nodes.extend(PROC_NET.iter().map(|n| Net(*pid, *tid, n)));
        }
        Entry(pid, tid, self::Entry::Ns) => {
            nodes.extend(NsKind::ALL.iter().map(|k| Ns(*pid, *tid, *k)));
        }
        Pid(pid, tid) => nodes.extend(
            ENTRIES
                .iter()
                .filter(|(_, e)| in_pid_dir(*tid, e))
                .map(|(_, e)| Entry(*pid, *tid, *e)),
        ),
        Entry(pid, _, e @ (self::Entry::Fd | self::Entry::FdInfo)) => {
            if ps.group(*pid).is_some() && !ps.may_read(*pid) {
                return Err(Errno::EACCES);
            }
            if let Some(task) = ps.leader(*pid) {
                let fds = task.res.borrow().fds.borrow().open_fds();
                nodes.extend(fds.into_iter().map(|fd| match e {
                    self::Entry::Fd => Fd(*pid, fd),
                    _ => FdInfo(*pid, fd),
                }));
            }
        }
        Entry(pid, None, self::Entry::Task) => {
            if let Some(g) = ps.group(*pid) {
                let g = g.borrow();
                let zombie = g.leader_exit.is_some().then_some(g.tgid);
                nodes.extend(
                    zombie
                        .into_iter()
                        .chain(g.threads.iter().copied())
                        .map(|t| Pid(*pid, Some(t))),
                );
            }
        }
        _ => return Err(Errno::ENOTDIR),
    }
    nodes
        .into_iter()
        .map(|n| {
            Ok(DirEntry {
                name: ps.name(mnt, &n),
                ino: node_ino(ps.k, &n),
                mode: ps.kind(&n).unwrap_or(mode::S_IFREG),
            })
        })
        .collect()
}

/// Whether `/proc/<pid>` (`tid` is None) or `/proc/<pid>/task/<tid>` has entry `e`: `children`
/// is only in the latter.
fn in_pid_dir(tid: Option<Tid>, e: &Entry) -> bool {
    match tid {
        None => *e != Entry::Children,
        Some(_) => TASK_ENTRIES.contains(e),
    }
}

/// The inode given by stat and readdir: for `/proc/<pid>` and the entries under it it also
/// depends on which process it is, so as on Linux the inode differs after a new process reuses
/// the pid.
pub(crate) fn node_ino(k: &Kernel, node: &PseudoNode) -> u64 {
    let pid = match node {
        // A namespace's inode number is its id
        PseudoNode::NsFile(_, id) => return *id,
        PseudoNode::Pid(pid, _)
        | PseudoNode::Entry(pid, ..)
        | PseudoNode::Net(pid, ..)
        | PseudoNode::Fd(pid, _)
        | PseudoNode::OpenFd(pid, _)
        | PseudoNode::OpenExe(pid)
        | PseudoNode::FdInfo(pid, _) => *pid,
        _ => return ino_of(node),
    };
    let Some(g) = group_of(k, pid) else {
        return ino_of(node);
    };
    let mut h = std::hash::DefaultHasher::new();
    node.hash(&mut h);
    g.borrow().serial.hash(&mut h);
    h.finish() | 1
}

/// The inode number of a synthetic node: a hash of the node (stable within one run), avoiding 0.
/// Each filesystem's root directory is 1, as on Linux (procfs's `PROC_ROOT_INO`, the kernfs root,
/// the devpts root): runc checks it after opening `/proc` (pathrs-lite's `verifyProcRoot`) and
/// treats anything other than 1 as a replaced procfs. devpts follows Linux too: ptmx is 2, and
/// terminal n is n + 3.
pub(crate) fn ino_of(node: &PseudoNode) -> u64 {
    if matches!(
        node,
        PseudoNode::ProcRoot
            | PseudoNode::SysRoot
            | PseudoNode::PtsRoot(_)
            | PseudoNode::Cgroup(crate::cgroup::ROOT, b"")
    ) {
        return 1;
    }
    match node {
        PseudoNode::PtsPtmx(_) => return 2,
        PseudoNode::Pts(_, n) => return *n as u64 + 3,
        _ => {}
    }
    // A queue uses its own number (it is also what fstat on an fd from mq_open shows)
    if let PseudoNode::Mq(_, ino) = node {
        return *ino;
    }
    let mut h = std::hash::DefaultHasher::new();
    node.hash(&mut h);
    h.finish() | 1
}

// ---- File contents ----

/// Clock ticks (`sysconf(_SC_CLK_TCK)`).
const HZ: u64 = crate::loader::CLOCK_TICKS;

/// The procfs files that can be written.
pub(crate) fn writable(node: &PseudoNode) -> bool {
    if let PseudoNode::Cgroup(_, name) = node {
        return crate::cgroup::file_mode(name).is_some_and(|m| m & 0o200 != 0);
    }
    if let PseudoNode::NetConf(_, _, name) = node {
        return !name.is_empty() && !crate::netconf::read_only(name);
    }
    if let PseudoNode::NetSysctl(path) = node {
        return crate::netconf::sysctl(path.as_bytes(), true).is_some_and(|e| e.2 & 0o200 != 0);
    }
    matches!(
        node,
        PseudoNode::Entry(_, None, Entry::CoredumpFilter | Entry::TimensOffsets)
            | PseudoNode::Entry(_, _, Entry::OomScoreAdj)
            | PseudoNode::Entry(_, _, Entry::Mem)
            | PseudoNode::Entry(_, _, Entry::UidMap | Entry::GidMap | Entry::Setgroups)
            | PseudoNode::Static(
                b"sys/kernel/hostname"
                    | b"sys/kernel/domainname"
                    | b"sys/kernel/pid_max"
                    | b"sys/kernel/threads-max"
                    | b"sys/kernel/ns_last_pid"
                    | b"sys/kernel/core_pattern"
                    | b"sys/kernel/seccomp/actions_logged"
                    | b"sys/kernel/core_uses_pid"
                    | b"sys/kernel/dmesg_restrict"
                    | b"sys/kernel/panic"
                    | b"sys/kernel/unprivileged_bpf_disabled"
                    | b"sys/fs/protected_hardlinks"
                    | b"sys/fs/protected_symlinks"
                    | b"sys/kernel/printk"
                    | b"sys/kernel/printk_devkmsg"
                    | b"sys/kernel/msgmax"
                    | b"sys/kernel/msgmnb"
                    | b"sys/kernel/msgmni"
                    | b"sys/kernel/sem"
                    | b"sys/kernel/shmall"
                    | b"sys/kernel/shmmax"
                    | b"sys/kernel/shmmni"
                    | b"sys/fs/lease-break-time"
                    | b"sys/fs/aio-max-nr"
                    | b"sys/fs/mqueue/msg_default"
                    | b"sys/fs/mqueue/msg_max"
                    | b"sys/fs/mqueue/msgsize_default"
                    | b"sys/fs/mqueue/msgsize_max"
                    | b"sys/fs/mqueue/queues_max"
                    | b"sys/fs/mount-max"
                    | b"sys/fs/nr_open"
                    | b"sys/fs/pipe-max-size"
                    | b"sys/fs/inotify/max_queued_events"
                    | b"sys/fs/inotify/max_user_instances"
                    | b"sys/fs/inotify/max_user_watches"
                    | b"sys/vm/max_map_count"
                    | b"sys/vm/mmap_min_addr"
                    | b"sys/net/core/somaxconn"
                    | b"sys/net/ipv4/ip_forward"
                    | b"sys/net/ipv4/ip_local_port_range"
                    | b"sys/net/ipv4/ip_unprivileged_port_start"
                    | b"sys/net/ipv4/ping_group_range"
                    | b"sys/kernel/tainted"
                    | b"sys/kernel/modprobe"
                    | b"sys/vm/overcommit_memory"
            )
    ) || matches!(node, PseudoNode::Static(p)
        if p.starts_with(b"sys/user/max_") || recorded(p).is_some())
}

/// For a file that is not [`writable`] but that whoever passes its permission check (root, through
/// `CAP_DAC_OVERRIDE`) may still open for writing, the error each write then returns: procfs files
/// outside `/proc/sys`, as Linux's open checks only the mode there. Files of proc entries
/// (`/proc/cpuinfo`, `/proc/kmsg`, `/proc/sysvipc/*`, the single-record files of
/// `/proc/<pid>/net`) go through `proc_reg_write`, `EIO` without a write handler; per-process files
/// have no write operation, so `vfs_write` answers `EINVAL`. `None`: the open is refused
/// ([`open_unwritable`]).
pub(crate) fn unwritable_error(node: &PseudoNode) -> Option<Errno> {
    match node {
        PseudoNode::Global(_) => Some(Errno::EIO),
        PseudoNode::Net(_, _, name)
            if matches!(*name, b"netstat" | b"snmp" | b"sockstat" | b"sockstat6") =>
        {
            Some(Errno::EIO)
        }
        PseudoNode::Static(p) if p.starts_with(b"sysvipc/") => Some(Errno::EIO),
        PseudoNode::Entry(..) | PseudoNode::FdInfo(..) => Some(Errno::EINVAL),
        _ => None,
    }
}

/// An open asking for write permission (`for_write`: an access mode that writes, rather than
/// `O_TRUNC` alone) of a node that is not [`writable`], once its permission check has passed: Ok
/// opens it ([`unwritable_error`] says what its writes get), Err refuses it. `/proc/sys` decides by
/// the mode alone (`proc_sys_permission`, which refuses `O_TRUNC` too); sysfs (`kernfs_fop_open`)
/// and the other `/proc/<pid>/net` files (`seq_open_net`) refuse only an open for writing.
pub(crate) fn open_unwritable(node: &PseudoNode, for_write: bool) -> Result<(), Errno> {
    match node {
        PseudoNode::Static(p) if p.starts_with(b"sys/") => Err(Errno::EACCES),
        PseudoNode::NetConf(..) | PseudoNode::NetSysctl(_) => Err(Errno::EACCES),
        _ if unwritable_error(node).is_some() || !for_write => Ok(()),
        _ => Err(Errno::EACCES),
    }
}

/// Files with a poll operation, which epoll accepts: procfs's global files, `/proc/sys`,
/// `/proc/sysvipc` and `/proc/<pid>/net` (`proc_reg_poll`, `proc_sys_poll`), mounts and mountinfo
/// (`mounts_poll`), and every sysfs and cgroup2 file (`kernfs_fop_poll`). The other per-process
/// files and nsfs have none (epoll_ctl's EPERM).
pub(crate) fn pollable(node: &PseudoNode) -> bool {
    use PseudoNode::*;
    matches!(
        node,
        Global(_)
            | Static(_)
            | Net(..)
            | NetConf(..)
            | NetSysctl(_)
            | Entry(_, _, self::Entry::Mounts | self::Entry::MountInfo)
            | Sys(_)
            | SysCpu(..)
            | SysNet(..)
            | SysBlock(..)
            | Cgroup(..)
    )
}

/// Files that cannot be read: write-only cgroup files (`cgroup.kill`); as on Linux this is not
/// checked at open, and reading is EINVAL (`cgroup_seqfile_show` has no read callback).
pub(crate) fn unreadable(node: &PseudoNode) -> bool {
    matches!(node, PseudoNode::Cgroup(_, name)
        if crate::cgroup::file_mode(name).is_some_and(|m| m & 0o444 == 0))
        || matches!(node, PseudoNode::Static(b"sys/vm/drop_caches"))
}

/// The contents generated when a procfs file is opened.
pub(crate) fn content(ps: &View<'_>, mnt: MountId, node: &PseudoNode) -> Result<Vec<u8>, Errno> {
    use PseudoNode::*;
    let ns = ps.pid_ns(mnt);
    let s = match node {
        Global(g) => match g {
            self::Global::Cpuinfo => cpuinfo(),
            self::Global::Meminfo => meminfo(),
            self::Global::Vmstat => {
                vmstat(tuxport_hal::host_vmstat(), &tuxport_hal::host_meminfo())
            }
            // Like meminfo's swap lines, the host's (a container sees its host's); the guest cannot
            // swap on anything itself. Linux's swap_show header alone when the host hides it
            self::Global::Swaps => tuxport_hal::host_swaps().unwrap_or_else(|| {
                "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n".to_owned()
            }),
            self::Global::Locks => crate::locks::proc_locks(ps.k, ps.vfs, &ns),
            self::Global::Stat => global_stat(ps),
            // Truncated to hundredths of a second, as in Linux's uptime_proc_show (rounding would
            // write x.996 seconds as the next second, more than sysinfo reports right after)
            self::Global::Uptime => {
                let hundredths =
                    |ns: u64| format!("{}.{:02}", ns / 1_000_000_000, ns / 10_000_000 % 100);
                // Idle time is not affected by the time namespace (Linux's uptime_proc_show)
                let idle = uptime_ns() * cpus() as u64;
                let up = ps.with_boot_offset(uptime_ns());
                format!("{} {}\n", hundredths(up), hundredths(idle))
            }
            self::Global::Loadavg => crate::loadavg::proc_loadavg(ps.k, ps.last_pid()),
            self::Global::Version => {
                "Linux version 6.1.0-tuxport (tuxport@localhost) #1 SMP PREEMPT tuxport\n".to_owned()
            }
            self::Global::Filesystems => {
                "nodev\tsysfs\nnodev\ttmpfs\n\text4\nnodev\tproc\nnodev\tdevpts\nnodev\tcgroup2\nnodev\tmqueue\nnodev\toverlay\nnodev\tfuse\n"
                    .to_owned()
            }
            self::Global::Cmdline => "tuxport\n".to_owned(),
            self::Global::Cgroups => crate::cgroup::proc_cgroups(ps.k),
            self::Global::Kmsg => String::new(),
            self::Global::Mounts | self::Global::Net => return Err(Errno::EINVAL),
        },
        // Those of the network namespace that process is in: the interfaces are the host's (lo in
        // a new one) plus virtual devices (crate::netdev), and only sockets created in it are
        // listed; in a new one the traffic and protocol counters are all 0 (crate::netns)
        Net(pid, tid, name) => {
            let full = ps
                .task(*pid, *tid)
                .map_or_else(|| ps.k.init_ns.net.clone(), |t| t.ns.borrow().net.clone());
            let net = Some(full.clone()).filter(|n| n.host_net.is_some());
            let id = net.as_ref().map_or(crate::nsproxy::INIT_NET_NS, |n| n.id);
            // The uid column is translated through the reader's user namespace (Linux's
            // seq_user_ns)
            let reader = ps.reader();
            let socks: Vec<(SocketRef, u32)> = sockets(ps.k)
                .into_iter()
                .filter(|(s, _)| socket_net(&s.borrow()) == id)
                .map(|(s, uid)| (s, reader.uid_out(uid)))
                .collect();
            let ifaces = crate::netdev::interfaces(&full);
            let world = ps.k.world();
            let quiet = crate::traffic::Protocols::default();
            let protocols = if net.is_some() {
                &quiet
            } else {
                &ps.k.traffic.protocols
            };
            let tables = || {
                let host = net.is_none().then(|| crate::route::host_routes(ps.k)).flatten();
                crate::netdev::tables(&full, host)
            };
            match &name[..] {
                b"arp" => crate::net::PROC_ARP.to_owned(),
                b"dev" => net_dev(ps.k, &ifaces, net.is_none()),
                b"netstat" => crate::net::proc_netstat(protocols),
                b"snmp" => crate::net::proc_snmp(&socks, protocols),
                b"if_inet6" => net_if_inet6(&ifaces),
                b"route" => crate::route::proc_route(&tables()),
                b"ipv6_route" => crate::route::proc_ipv6_route(&tables()),
                b"sockstat" => crate::net::sockstat(&socks, false),
                b"sockstat6" => crate::net::sockstat(&socks, true),
                b"tcp" => crate::net::proc_listing(&socks, AF_INET, Table::Tcp, &ps.k.portmap, &world),
                b"tcp6" => crate::net::proc_listing(&socks, AF_INET6, Table::Tcp, &ps.k.portmap, &world),
                b"udp" => crate::net::proc_listing(&socks, AF_INET, Table::Udp, &ps.k.portmap, &world),
                b"udp6" => crate::net::proc_listing(&socks, AF_INET6, Table::Udp, &ps.k.portmap, &world),
                b"icmp" => crate::net::proc_listing(&socks, AF_INET, Table::Icmp, &ps.k.portmap, &world),
                b"icmp6" => crate::net::proc_listing(&socks, AF_INET6, Table::Icmp, &ps.k.portmap, &world),
                b"raw" => crate::net::proc_listing(&socks, AF_INET, Table::Raw, &ps.k.portmap, &world),
                b"raw6" => crate::net::proc_listing(&socks, AF_INET6, Table::Raw, &ps.k.portmap, &world),
                b"unix" => crate::unix::proc_listing(&socks),
                _ => return Err(Errno::ENOENT),
            }
        }
        Static(path) => match &path[..] {
            b"sys/fs/file-max" => "9223372036854775807\n".to_owned(),
            b"sys/fs/lease-break-time" => format!("{}\n", ps.k.lease_break_time),
            b"sys/fs/aio-max-nr" => format!("{}\n", ps.k.aio_max_nr),
            b"sys/fs/aio-nr" => format!("{}\n", ps.k.aio_nr),
            p if p.starts_with(b"sys/fs/mqueue/") => {
                let ns = ps.ns().ipc.id;
                format!("{}\n", ps.k.ipc(ns).mq_limits.get(&p[14..]).unwrap_or(0))
            }
            b"sys/fs/inotify/max_queued_events" => {
                format!("{}\n", ps.k.inotify.borrow().max_queued)
            }
            b"sys/fs/inotify/max_user_instances" => {
                format!("{}\n", ps.k.inotify.borrow().max_instances)
            }
            b"sys/fs/inotify/max_user_watches" => {
                format!("{}\n", ps.k.inotify.borrow().max_watches)
            }
            b"sys/fs/nr_open" => format!("{}\n", ps.k.nr_open),
            b"sys/fs/mount-max" => format!("{}\n", ps.k.mount_max),
            b"sys/fs/pipe-max-size" => format!("{}\n", ps.k.pipe_max_size),
            b"sys/fs/pipe-user-pages-hard" => "0\n".to_owned(),
            b"sys/fs/pipe-user-pages-soft" => "16384\n".to_owned(),
            b"sys/kernel/cap_last_cap" => "40\n".to_owned(),
            b"sys/net/core/somaxconn" => {
                format!("{}\n", ps.ns().net.devs.borrow().conf.somaxconn)
            }
            b"sys/net/ipv4/ip_forward" => format!("{}\n", crate::netdev::ip_forward(&ps.ns().net)),
            b"sys/net/ipv4/ip_local_port_range" => {
                let (lo, hi) = ps.ns().net.devs.borrow().conf.port_range;
                format!("{lo}\t{hi}\n")
            }
            b"sys/net/ipv4/ip_unprivileged_port_start" => {
                format!("{}\n", ps.ns().net.devs.borrow().conf.prot_sock)
            }
            b"sys/net/ipv4/ping_group_range" => {
                // Kernel IDs, shown in the reader's namespace (65534 when unmapped)
                let (lo, hi) = ps.ns().net.devs.borrow().conf.ping_group;
                let r = ps.reader();
                format!("{}\t{}\n", r.gid_out(lo as u32), r.gid_out(hi as u32))
            }
            b"sys/kernel/core_pattern" => {
                format!("{}\n", String::from_utf8_lossy(&ps.k.core_pattern))
            }
            b"sys/kernel/core_uses_pid" => format!("{}\n", ps.k.core_uses_pid as u8),
            b"sys/kernel/domainname" => {
                format!(
                    "{}\n",
                    String::from_utf8_lossy(&ps.ns().uts.domainname.borrow())
                )
            }
            b"sys/kernel/hostname" => {
                format!(
                    "{}\n",
                    String::from_utf8_lossy(&ps.ns().uts.hostname.borrow())
                )
            }
            b"sys/kernel/sched_rr_timeslice_ms" => "100\n".to_owned(),
            b"sys/kernel/msgmax" | b"sys/kernel/msgmnb" | b"sys/kernel/msgmni"
            | b"sys/kernel/sem" | b"sys/kernel/shmall" | b"sys/kernel/shmmax"
            | b"sys/kernel/shmmni" => {
                let ns = ps.ns().ipc.id;
                ps.k.ipc(ns).tunables.read(&path[11..]).unwrap()
            }
            b"sysvipc/msg" => ipc::proc_listing(ps.k, ps.ns().ipc.id, b"msg", &ps.reader()),
            b"sysvipc/sem" => ipc::proc_listing(ps.k, ps.ns().ipc.id, b"sem", &ps.reader()),
            b"sysvipc/shm" => ipc::proc_listing(ps.k, ps.ns().ipc.id, b"shm", &ps.reader()),
            b"sys/kernel/tainted" => format!("{}\n", ps.k.tainted),
            b"sys/kernel/modprobe" => format!("{}\n", String::from_utf8_lossy(&ps.k.modprobe)),
            // Write-only (reading is EINVAL, `unreadable`)
            b"sys/vm/drop_caches" => String::new(),
            b"sys/kernel/threads-max" => format!("{}\n", ps.k.threads_max),
            b"sys/vm/max_map_count" => {
                format!(
                    "{}\n",
                    crate::mm::MAX_MAP_COUNT.load(std::sync::atomic::Ordering::Relaxed)
                )
            }
            b"sys/vm/mmap_min_addr" => {
                format!(
                    "{}\n",
                    crate::mm::MMAP_MIN_ADDR.load(std::sync::atomic::Ordering::Relaxed)
                )
            }
            b"sys/kernel/ngroups_max" => "65536\n".to_owned(),
            b"sys/kernel/ns_last_pid" => format!("{}\n", ps.last_pid()),
            b"sys/kernel/seccomp/actions_avail" => crate::seccomp::actions_avail(),
            b"sys/kernel/seccomp/actions_logged" => crate::seccomp::actions_logged(ps.k),
            b"sys/kernel/osrelease" => "6.1.0-tuxport\n".to_owned(),
            b"sys/kernel/ostype" => "Linux\n".to_owned(),
            b"sys/kernel/pid_max" => "4194304\n".to_owned(),
            b"sys/kernel/dmesg_restrict" => format!("{}\n", ps.k.dmesg_restrict as u8),
            b"sys/kernel/panic" => format!("{}\n", ps.k.panic),
            b"sys/kernel/unprivileged_bpf_disabled" => {
                format!("{}\n", ps.k.unprivileged_bpf_disabled)
            }
            b"sys/fs/protected_hardlinks" => format!("{}\n", ps.k.protected_hardlinks as u8),
            b"sys/fs/protected_symlinks" => format!("{}\n", ps.k.protected_symlinks as u8),
            b"sys/kernel/printk" => {
                let [a, b, c, d] = ps.k.printk;
                format!("{a}\t{b}\t{c}\t{d}\n")
            }
            b"sys/kernel/printk_devkmsg" => format!("{}\n", ps.k.klog.borrow().devkmsg.name()),
            // Limits on the namespaces of each kind created in the reader's user namespace and
            // below it (crate::nsproxy::Ucounts): half of threads-max in the initial one and
            // INT_MAX in the others unless written
            p if p.starts_with(b"sys/user/max_") => {
                let kind = crate::nsproxy::Counted::from_sysctl(&p[b"sys/user/".len()..])
                    .ok_or(Errno::ENOENT)?;
                let init = crate::nsproxy::default_ns_limit(ps.k.threads_max);
                let v = ps.k.ucounts.limit(ps.reader().user_ns.id, kind, init);
                format!("{v}\n")
            }
            b"sys/kernel/random/boot_id" => format!("{}\n", format_uuid(&ps.k.boot_id)),
            b"sys/kernel/random/entropy_avail" | b"sys/kernel/random/poolsize" => {
                format!("{}\n", crate::syscall::ENTROPY)
            }
            b"sys/kernel/random/uuid" => format!("{}\n", random_uuid()),
            b"sys/kernel/version" => "#1 SMP PREEMPT tuxport\n".to_owned(),
            b"sys/vm/overcommit_memory" => {
                format!(
                    "{}\n",
                    crate::mm::OVERCOMMIT_MEMORY.load(std::sync::atomic::Ordering::Relaxed)
                )
            }
            // The ID unmapped user and group IDs are shown as (Linux's default, 65534; bubblewrap
            // reads it at startup) and the other recorded sysctls
            p if recorded(p).is_some() => format!("{}\n", recorded_value(ps.k, p)),
            _ => return Err(Errno::EISDIR),
        },
        Entry(pid, tid, e) => {
            let task = ps.task(*pid, *tid);
            match e {
                // These carry the thread's name as its bytes, which need not be UTF-8
                self::Entry::Stat => return pid_stat(ps, &ns, *pid, *tid),
                self::Entry::Status => return pid_status(ps, &ns, *pid, *tid),
                self::Entry::Statm => {
                    let (vsize, rss) = mem_of(task.as_deref());
                    format!("{} {rss} 0 0 0 {rss} 0\n", vsize / 4096)
                }
                self::Entry::Comm => {
                    let g = ps.group(*pid).ok_or(Errno::ENOENT)?;
                    let mut name = comm_of(&g.borrow(), task.as_deref(), *pid, *tid);
                    name.push(b'\n');
                    return Ok(name);
                }
                // The thread's child processes not yet reaped, each followed by a space, with no
                // newline (Linux's children_seq_show)
                self::Entry::Children => {
                    let g = ps.group(*pid).ok_or(Errno::ENOENT)?;
                    let me = tid.unwrap_or(*pid);
                    let mut s = String::new();
                    for c in &g.borrow().children {
                        if ps
                            .k
                            .groups
                            .get(c)
                            .is_some_and(|g| g.borrow().parent_thread == me)
                        {
                            write!(s, "{} ", ns.vnr(*c).unwrap_or(0)).unwrap();
                        }
                    }
                    s
                }
                // Relative to the root of the reader's cgroup namespace
                self::Entry::Cgroup => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    let cgroup = g.borrow().cgroup;
                    crate::cgroup::proc_cgroup(ps.k, cgroup, ps.cgroup_root())
                }
                self::Entry::Cmdline => {
                    return Ok(task.map_or(Vec::new(), |t| guest_bytes(&t, |m| m.args)));
                }
                self::Entry::Environ => {
                    return Ok(task.map_or(Vec::new(), |t| guest_bytes(&t, |m| m.env)));
                }
                self::Entry::Limits => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    limits(&g.borrow().rlimits)
                }
                self::Entry::Maps => task.map_or(String::new(), |t| maps(ps.vfs.store(), &t)),
                self::Entry::Smaps => {
                    task.map_or(String::new(), |t| smaps(ps.vfs.store(), &t, false))
                }
                self::Entry::SmapsRollup => {
                    task.map_or(String::new(), |t| smaps(ps.vfs.store(), &t, true))
                }
                // Binary, computed from the read or write position (`read_pagemap`, `read_mem`,
                // `write_mem`)
                self::Entry::Pagemap | self::Entry::Mem => String::new(),
                self::Entry::Auxv => {
                    let auxv =
                        task.map_or_else(Vec::new, |t| t.res.borrow().mm.borrow().auxv.to_vec());
                    return Ok(auxv.iter().flat_map(|w| w.to_le_bytes()).collect());
                }
                self::Entry::Mounts | self::Entry::MountInfo => {
                    // An exited thread (a zombie, a main thread whose process lives on) has no
                    // namespaces left: EINVAL at open (mounts_open_common)
                    let t = task.ok_or(Errno::EINVAL)?;
                    let (root, ns) = {
                        let res = t.res.borrow();
                        let fs = res.fs.borrow();
                        (fs.root.clone(), fs.mnt_ns.id)
                    };
                    return Ok(mounts(ps, &root, ns, *e == self::Entry::MountInfo));
                }
                // Linux's proc_pid_syscall: "running" for a thread running guest code or kernel
                // code, and for one waiting in the kernel (or the reader itself) the syscall
                // number with x0 to x5, sp and pc, or a negative number with sp and pc outside a
                // syscall. An exited main thread (a zombie task on Linux) has no stack left: -1
                // with zeros; another thread that has gone is released at once (ESRCH). Like
                // lock_trace, it needs ptrace access to the thread itself, checked when read (open
                // generates nothing) with the reader's own credentials rather than the opener's
                self::Entry::Syscall => {
                    let me = ps.k.cred_of(ps.tid).unwrap_or_else(Cred::root);
                    let thread = tid.unwrap_or(*pid);
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    if task.is_none() && thread != g.borrow().tgid {
                        return Err(Errno::ESRCH);
                    }
                    if !ps.may_read_as(thread, &me) {
                        return Err(Errno::EPERM);
                    }
                    let own = thread == ps.tid;
                    let running = task.is_some()
                        && !own
                        && state_of(ps, &g.borrow(), *pid, *tid).0 == 'R';
                    match task.as_deref().and_then(|t| t.sync.entry.get()) {
                        _ if running => "running\n".to_owned(),
                        Some([nr, a0, a1, a2, a3, a4, a5, sp, pc]) => {
                            // The number is the 32-bit w8, as arm64's syscallno is
                            let nr = nr as u32 as i32;
                            if nr < 0 {
                                format!("{nr} 0x{sp:x} 0x{pc:x}\n")
                            } else {
                                format!(
                                    "{nr} 0x{a0:x} 0x{a1:x} 0x{a2:x} 0x{a3:x} 0x{a4:x} 0x{a5:x} \
                                     0x{sp:x} 0x{pc:x}\n"
                                )
                            }
                        }
                        None => "-1 0x0 0x0\n".to_owned(),
                    }
                }
                self::Entry::OomScore => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    format!("{}\n", crate::oom::proc_score(ps.k, &g.borrow()))
                }
                self::Entry::OomScoreAdj => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    format!("{}\n", g.borrow().oom_score_adj)
                }
                // Linux's proc_timens_show_offsets: the one for children
                self::Entry::TimensOffsets => {
                    let t = ps.k.tasks.get(pid).ok_or(Errno::ESRCH)?;
                    let [mono, boot] = t.ns.borrow().time_for_children.offsets.get();
                    format!(
                        "{:<10} {:>10} {:>9}\n{:<10} {:>10} {:>9}\n",
                        "monotonic", mono.0, mono.1, "boottime", boot.0, boot.1
                    )
                }
                e @ (self::Entry::UidMap | self::Entry::GidMap | self::Entry::ProjidMap) => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    let ns = g.borrow().cred.user_ns.clone();
                    id_map(&ps.reader(), &ns, *e)
                }
                self::Entry::Setgroups => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    let allow = g.borrow().cred.user_ns.setgroups.get();
                    format!("{}\n", if allow { "allow" } else { "deny" })
                }
                self::Entry::CoredumpFilter => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    format!("{:08x}\n", g.borrow().coredump_filter)
                }
                _ => return Err(Errno::EINVAL),
            }
        }
        FdInfo(pid, fd) => fdinfo(ps, &ns, *pid, *fd)?,
        Sys(path) => match &path[..] {
            b"devices/system/cpu/online"
            | b"devices/system/cpu/possible"
            | b"devices/system/cpu/present" => cpu_list(0..cpus()),
            b"devices/system/cpu/offline" => "\n".to_owned(),
            // The defaults of 6.1's Kconfig, which are what the overlay does: no index, redirects,
            // metacopy, NFS export or xino
            b"module/overlay/parameters/redirect_always_follow" => "Y\n".to_owned(),
            b"module/overlay/parameters/redirect_max" => "256\n".to_owned(),
            p if p.starts_with(b"module/overlay/parameters/") => "N\n".to_owned(),
            _ => return Err(Errno::EISDIR),
        },
        SysCpu(n, path) => cpu_file(*n as usize, path)?,
        SysNet(n, path) => net_file(ps.k, &ps.ns().net, *n, path)?,
        SysBlock(n, path) => ps.k.loops.sysfs_read(*n, path).ok_or(Errno::EISDIR)?,
        NetConf(v6, dir, name) => {
            crate::netdev::conf_read(&ps.ns().net, *v6, *dir, name)?
        }
        NetSysctl(path) => crate::netconf::sysctl_read(&ps.ns().net.devs.borrow().conf, path),
        // Write-only ones (cgroup.kill) are EINVAL to read
        Cgroup(_, b"") => return Err(Errno::EISDIR),
        Cgroup(id, name) => {
            // Ids as the reader's pid namespace sees them (task_pid_vnr: current's, not the
            // mount's)
            let ns = ps.k.pid_ns_of(ps.tgid);
            let vnr = |t| ns.vnr(t).unwrap_or(0);
            crate::cgroup::file(ps.k, *id, name, &vnr).ok_or(Errno::EINVAL)?
        }
        // A namespace is opened only for setns; nothing can be read
        NsFile(..) => String::new(),
        _ => return Err(Errno::EINVAL),
    };
    Ok(s.into_bytes())
}

// ---- Per-process files ----

fn pid_stat(ps: &View<'_>, ns: &PidNs, pid: Tid, tid: Option<Tid>) -> Result<Vec<u8>, Errno> {
    let g = ps.group(pid).ok_or(Errno::ENOENT)?;
    let g = g.borrow();
    let task = ps.task(pid, tid);
    let nice = task.as_ref().map_or(0, |t| t.nice.get());
    let (state, _) = state_of(ps, &g, pid, tid);
    let (tty_nr, tpgid) = ctty_info(ns, &g);
    let v = |p: Tid| ns.vnr(p).unwrap_or(0);
    let cpu = match (tid, &task, g.leader_exit) {
        (Some(_), Some(t), _) => t.cpu(),
        // The exited main thread's own time
        (Some(t), None, Some((_, cpu))) if t == g.tgid => cpu,
        _ => g.cpu(ps.k),
    };
    let (vsize, rss) = mem_of(task.as_deref());
    let (sigign, sigcgt) = sig_dispositions(&g);
    let (pending, blocked) = task.as_ref().map_or((0, 0), |t| {
        (t.pending.borrow().set() | g.pending.set(), t.sigmask.get())
    });
    // The layout goes only to readers passing the ptrace read check (6.1's do_task_stat):
    // others see 1 for the code's bounds and 0 for the rest
    let permitted = ps.may_read(pid);
    let layout = task.as_ref().map(|t| {
        let res = t.res.borrow();
        let mm = res.mm.borrow();
        let (code, data) = (mm.code, mm.data);
        let rest = [mm.brk_start(), mm.args.0, mm.args.1, mm.env.0, mm.env.1];
        (code, mm.start_stack(), data, rest)
    });
    let (code, stack, tail) = match layout {
        Some((code, stack, data, rest)) if permitted => {
            let tail = [data.0, data.1, rest[0], rest[1], rest[2], rest[3], rest[4]];
            (code, stack, tail)
        }
        Some(_) => ((1, 1), 0, [0; 7]),
        None => ((0, 0), 0, [0; 7]),
    };
    // The name as raw bytes, as Linux's proc_task_name without escaping
    let mut out = format!("{} (", v(tid.unwrap_or(pid))).into_bytes();
    out.extend(comm_of(&g, task.as_deref(), pid, tid));
    let mut s = String::new();
    write!(
        s,
        ") {} {} {} {} {} {} 4194304 0 0 0 0 {} {} {} {} {} {} {} 0 {} {} {} 18446744073709551615",
        state,
        v(g.ppid),
        v(g.pgid),
        v(g.sid),
        tty_nr,
        tpgid,
        ticks(cpu.user_ns()),
        ticks(cpu.sys_ns),
        ticks(g.child_cpu.user_ns()),
        ticks(g.child_cpu.sys_ns),
        20 + nice,
        nice,
        thread_count(&g).max(1),
        ps.with_boot_offset(g.start_ticks * (1_000_000_000 / HZ)) / (1_000_000_000 / HZ),
        vsize,
        rss,
    )
    .unwrap();
    let tail = tail.map(|v| v.to_string()).join(" ");
    writeln!(
        s,
        " {} {} {stack} 0 0 {pending} {blocked} {sigign} {sigcgt} 0 0 0 {} 0 0 0 0 0 0 {tail} 0",
        code.0, code.1, g.exit_signal
    )
    .unwrap();
    out.extend(s.into_bytes());
    Ok(out)
}

/// The controlling terminal's device number and foreground process group (its number in pid
/// namespace `ns`).
fn ctty_info(ns: &PidNs, g: &ThreadGroup) -> (u64, i64) {
    match &g.ctty {
        Some(tty) => {
            let t = tty.borrow();
            let fg = t.fg.map_or(-1, |p| ns.vnr(p).unwrap_or(0) as i64);
            (rdev(PTS_MAJOR, t.index), fg)
        }
        None => (0, -1),
    }
}

fn pid_status(ps: &View<'_>, ns: &PidNs, pid: Tid, tid: Option<Tid>) -> Result<Vec<u8>, Errno> {
    let g = ps.group(pid).ok_or(Errno::ENOENT)?;
    let g = g.borrow();
    let task = ps.task(pid, tid);
    let (state, state_name) = state_of(ps, &g, pid, tid);
    // The size only: the resident pages are measured below, split by kind
    let vsize = task
        .as_ref()
        .map_or(0, |t| t.res.borrow().mm.borrow().size());
    let (sigign, sigcgt) = sig_dispositions(&g);
    let (umask, fdsize) = task.as_ref().map_or((0o022, 0), |t| {
        let res = t.res.borrow();
        let fds = res.fds.borrow().open_fds().len();
        (res.fs.borrow().umask, fds.next_multiple_of(64))
    });
    let (pending, blocked) = task
        .as_ref()
        .map_or((0, 0), |t| (t.pending.borrow().set(), t.sigmask.get()));
    // The name as its bytes with only a newline and a backslash escaped (Linux's
    // proc_task_name with seq_escape_str(.., "\n\\"))
    let mut out = b"Name:\t".to_vec();
    for b in comm_of(&g, task.as_deref(), pid, tid) {
        match b {
            b'\n' => out.extend(b"\\n"),
            b'\\' => out.extend(b"\\\\"),
            b => out.push(b),
        }
    }
    out.push(b'\n');
    let mut s = String::new();
    writeln!(s, "Umask:\t{umask:04o}").unwrap();
    writeln!(s, "State:\t{state} ({state_name})").unwrap();
    let v = |p: Tid| ns.vnr(p).unwrap_or(0);
    let tid = tid.unwrap_or(pid);
    writeln!(s, "Tgid:\t{}\nNgid:\t0\nPid:\t{}", v(g.tgid), v(tid)).unwrap();
    let tracer = task.as_ref().and_then(|t| t.trace.borrow().tracer);
    writeln!(
        s,
        "PPid:\t{}\nTracerPid:\t{}",
        v(g.ppid),
        tracer.map_or(0, v)
    )
    .unwrap();
    // The thread's credentials, or the main thread's; IDs are translated through the reader's
    // user namespace (Linux's seq_user_ns)
    let c = &ps.k.cred_of(tid).unwrap_or_else(|| g.cred.clone());
    let r = ps.reader();
    let u = |k: u32| r.uid_out(k);
    let gi = |k: u32| r.gid_out(k);
    writeln!(
        s,
        "Uid:\t{}\t{}\t{}\t{}",
        u(c.ruid),
        u(c.euid),
        u(c.suid),
        u(c.fsuid)
    )
    .unwrap();
    writeln!(
        s,
        "Gid:\t{}\t{}\t{}\t{}",
        gi(c.rgid),
        gi(c.egid),
        gi(c.sgid),
        gi(c.fsgid)
    )
    .unwrap();
    let groups: Vec<String> = c.groups.iter().map(|&g| gi(g).to_string()).collect();
    writeln!(s, "FDSize:\t{fdsize}\nGroups:\t{}", groups.join(" ")).unwrap();
    // One column per level, from the mount's pid namespace down to the process's own
    let levels = g.pid_ns.levels_from(ns.level);
    let col = |p: Tid| -> String {
        levels
            .iter()
            .map(|n| format!("\t{}", n.vnr(p).unwrap_or(0)))
            .collect()
    };
    writeln!(
        s,
        "NStgid:{}\nNSpid:{}\nNSpgid:{}\nNSsid:{}",
        col(g.tgid),
        col(tid),
        col(g.pgid),
        col(g.sid)
    )
    .unwrap();
    if task.is_some() {
        let kb = vsize / 1024;
        writeln!(s, "VmPeak:\t{kb:>8} kB\nVmSize:\t{kb:>8} kB").unwrap();
        let locked = task
            .as_ref()
            .map_or(0, |t| t.res.borrow().mm.borrow().locked_bytes());
        writeln!(s, "VmLck:\t{:>8} kB\nVmPin:\t{:>8} kB", locked / 1024, 0).unwrap();
        // VmRSS is the sum of the three kinds, as on Linux
        let (anon, file, shmem) = task
            .as_ref()
            .map_or((0, 0, 0), |t| t.res.borrow().mm.borrow().resident_split());
        let rss = anon + file + shmem;
        let hwm = g.maxrss_kb.max(rss * 4);
        writeln!(s, "VmHWM:\t{hwm:>8} kB\nVmRSS:\t{:>8} kB", rss * 4).unwrap();
        writeln!(
            s,
            "RssAnon:\t{:>8} kB\nRssFile:\t{:>8} kB\nRssShmem:\t{:>8} kB",
            anon * 4,
            file * 4,
            shmem * 4
        )
        .unwrap();
        // Grows-down mappings are the stack (Linux's stack_vm); data and code are not told
        // apart; no swap
        let stack = task
            .as_ref()
            .map_or(0, |t| t.res.borrow().mm.borrow().stack_bytes());
        writeln!(s, "VmData:\t{kb:>8} kB\nVmStk:\t{:>8} kB", stack / 1024).unwrap();
        writeln!(s, "VmExe:\t{:>8} kB\nVmLib:\t{:>8} kB", 0, 0).unwrap();
        writeln!(s, "VmPTE:\t{:>8} kB\nVmSwap:\t{:>8} kB", 0, 0).unwrap();
    }
    writeln!(s, "Threads:\t{}", thread_count(&g).max(1)).unwrap();
    let limit = g.rlimits[crate::process::RLIMIT_SIGPENDING as usize].0;
    writeln!(s, "SigQ:\t{}/{limit}", crate::signal::queued(ps.k, &g)).unwrap();
    writeln!(
        s,
        "SigPnd:\t{pending:016x}\nShdPnd:\t{:016x}",
        g.pending.set()
    )
    .unwrap();
    writeln!(
        s,
        "SigBlk:\t{blocked:016x}\nSigIgn:\t{sigign:016x}\nSigCgt:\t{sigcgt:016x}"
    )
    .unwrap();
    writeln!(s, "CapInh:\t{:016x}", c.cap_inheritable).unwrap();
    writeln!(s, "CapPrm:\t{:016x}", c.cap_permitted).unwrap();
    writeln!(s, "CapEff:\t{:016x}", c.cap_effective).unwrap();
    writeln!(s, "CapBnd:\t{:016x}", c.cap_bounding).unwrap();
    writeln!(s, "CapAmb:\t{:016x}", c.cap_ambient).unwrap();
    let (seccomp, filters) = task.as_ref().map_or((0, 0), |t| {
        let s = t.seccomp.borrow();
        (s.mode, s.filters())
    });
    writeln!(
        s,
        "NoNewPrivs:\t{}\nSeccomp:\t{seccomp}\nSeccomp_filters:\t{filters}",
        g.no_new_privs as u8
    )
    .unwrap();
    // The thread's affinity (the main thread's for the process), within the online CPUs
    let online = u64::MAX >> (64 - cpus().clamp(1, 64));
    let allowed = task.as_ref().map_or(u64::MAX, |t| t.affinity.get()) & online;
    let allowed: Vec<usize> = (0..64).filter(|b| allowed >> b & 1 != 0).collect();
    write!(s, "Cpus_allowed_list:\t{}", cpu_list_of(&allowed)).unwrap();
    // Each time a host thread goes to sleep (waits) is one voluntary switch: a thread that has
    // not been woken does not change
    let (vol, nonvol) = task.as_ref().map_or((0, 0), |t| {
        let (v, n) = tuxport_hal::thread_ctxt_switches(t.host_tid.get()).unwrap_or((0, 0));
        let (bv, bn) = t.ctxt_base.get();
        (v.saturating_sub(bv), n.saturating_sub(bn))
    });
    writeln!(
        s,
        "voluntary_ctxt_switches:\t{vol}\nnonvoluntary_ctxt_switches:\t{nonvol}"
    )
    .unwrap();
    out.extend(s.into_bytes());
    Ok(out)
}

/// The process state letter and name: Z zombie, t in a ptrace-stop, T stopped, S waiting in the
/// kernel, and R otherwise (running guest code or handling a syscall).
fn state_of(ps: &View<'_>, g: &ThreadGroup, pid: Tid, tid: Option<Tid>) -> (char, &'static str) {
    // The whole process, or its main thread once that has exited (Linux keeps it as a zombie
    // task, and the process directory shows it)
    if g.zombie.is_some() || (tid.unwrap_or(pid) == g.tgid && g.leader_exit.is_some()) {
        return ('Z', "zombie");
    }
    // Stopped in a ptrace-stop (also under PTRACE_LISTEN, as on Linux)
    if let Some(t) = ps.task(pid, tid)
        && t.trace.borrow().stop.is_some()
    {
        return ('t', "tracing stop");
    }
    if g.stopped {
        return ('T', "stopped");
    }
    // As on Linux, the state is the thread's (the main thread's for the process directory):
    // waiting in the kernel is S, and everything else (running guest code, handling a syscall,
    // just created and not yet run, woken but not yet running) is R
    if ps.task(pid, tid).is_some_and(|t| t.asleep()) {
        ('S', "sleeping")
    } else {
        ('R', "running")
    }
}

/// The threads of a process, counting an exited main thread until the process is reaped (Linux's
/// `nr_threads`).
fn thread_count(g: &ThreadGroup) -> usize {
    g.threads.len() + usize::from(g.leader_exit.is_some())
}

/// The name of the thread viewed (the main thread for the process directory): an exited main
/// thread keeps its name, as Linux's zombie task does.
fn comm_of(g: &ThreadGroup, task: Option<&Task>, pid: Tid, tid: Option<Tid>) -> Vec<u8> {
    let comm = match (task, g.leader_exit) {
        (Some(t), _) => t.comm.get(),
        (None, Some((comm, _))) if tid.unwrap_or(pid) == g.tgid => comm,
        _ => return comm_str(None, pid).into_bytes(),
    };
    let n = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
    comm[..n].to_vec()
}

/// An address space's (total size, resident pages).
fn mem_of(task: Option<&Task>) -> (u64, u64) {
    let Some(t) = task else {
        return (0, 0);
    };
    let res = t.res.borrow();
    let mm = res.mm.borrow();
    (mm.size(), mm.resident_pages())
}

/// Signal disposition bitmaps: (ignored, caught).
fn sig_dispositions(g: &ThreadGroup) -> (u64, u64) {
    let sh = g.sighand.borrow();
    let (mut ign, mut cgt) = (0, 0);
    for n in 1..=Signal::MAX {
        let sig = Signal::new(n).unwrap();
        match sh.get(sig).handler {
            SigAction::IGNORE => ign |= sig.bit(),
            SigAction::DEFAULT => {}
            _ => cgt |= sig.bit(),
        }
    }
    (ign, cgt)
}

pub(crate) fn comm_str(task: Option<&Task>, pid: Tid) -> String {
    match task {
        Some(t) => {
            let c = t.comm.get();
            let n = c.iter().position(|&b| b == 0).unwrap_or(c.len());
            String::from_utf8_lossy(&c[..n]).into_owned()
        }
        None if pid == INIT => "init".to_owned(),
        None => String::new(),
    }
}

/// A range of guest memory (cmdline, environ); empty when it cannot be read.
fn guest_bytes(task: &Task, range: fn(&crate::mm::Mm) -> (u64, u64)) -> Vec<u8> {
    let res = task.res.borrow();
    let mm = res.mm.borrow();
    let (start, end) = range(&mm);
    if end <= start || end - start > 1 << 20 {
        return Vec::new();
    }
    let mut buf = vec![0u8; (end - start) as usize];
    match mm.read(crate::uaccess::UserSlice::new(start, buf.len()), &mut buf) {
        Ok(()) => buf,
        Err(_) => Vec::new(),
    }
}

/// The names and units of the lines of `/proc/<pid>/limits`, in resource-number order (Linux's
/// `lnames`).
const LIMIT_NAMES: [(&str, &str); crate::process::RLIM_NLIMITS] = [
    ("Max cpu time", "seconds"),
    ("Max file size", "bytes"),
    ("Max data size", "bytes"),
    ("Max stack size", "bytes"),
    ("Max core file size", "bytes"),
    ("Max resident set", "bytes"),
    ("Max processes", "processes"),
    ("Max open files", "files"),
    ("Max locked memory", "bytes"),
    ("Max address space", "bytes"),
    ("Max file locks", "locks"),
    ("Max pending signals", "signals"),
    ("Max msgqueue size", "bytes"),
    ("Max nice priority", ""),
    ("Max realtime priority", ""),
    ("Max realtime timeout", "us"),
];

/// `/proc/<pid>/limits`: the same format as Linux's `proc_pid_limits`; lines without a unit still
/// keep the spaces after the hard limit at the end.
fn limits(rlimits: &[(u64, u64); crate::process::RLIM_NLIMITS]) -> String {
    let value = |v: u64| match v {
        u64::MAX => "unlimited".to_owned(),
        v => v.to_string(),
    };
    let mut s = format!(
        "{:<25} {:<20} {:<20} {:<10}\n",
        "Limit", "Soft Limit", "Hard Limit", "Units"
    );
    for ((name, unit), &(soft, hard)) in LIMIT_NAMES.iter().zip(rlimits) {
        write!(s, "{name:<25} {:<20} {:<20} ", value(soft), value(hard)).unwrap();
        if !unit.is_empty() {
            write!(s, "{unit:<10}").unwrap();
        }
        s.push('\n');
    }
    s
}

fn maps(store: &Store, task: &Task) -> String {
    let res = task.res.borrow();
    let mm = res.mm.borrow();
    mm.regions().iter().map(|r| map_line(store, r)).collect()
}

/// smaps counts, in kB. The host cannot tell which pages have been written: resident pages of
/// anonymous memory all count as dirty, and those of file mappings as clean. Pages of anonymous
/// memory shared with other address spaces after fork (copy-on-write) count as shared, with Pss
/// divided by the number of sharers ([`crate::mm::Mm::cow_shared`]); everything else counts as
/// private.
#[derive(Default)]
struct Usage {
    rss: u64,
    anon: u64,
    /// The part of `anon` shared with other address spaces.
    shared: u64,
    /// The bytes of `shared` after dividing.
    shared_pss: u64,
    file: u64,
    shmem: u64,
    locked: u64,
}

impl Usage {
    fn add(&mut self, o: &Usage) {
        self.rss += o.rss;
        self.anon += o.anon;
        self.shared += o.shared;
        self.shared_pss += o.shared_pss;
        self.file += o.file;
        self.shmem += o.shmem;
        self.locked += o.locked;
    }

    /// Linux's `__show_smap`: labels take 16 columns and numbers 8. As on Linux, Pss is summed in
    /// bytes first and converted to kB only at the end.
    fn write(&self, s: &mut String, rollup: bool) {
        let private = self.anon - self.shared;
        let pss_anon = private * 1024 + self.shared_pss;
        let pss_shmem = self.shmem * 1024;
        let dirty = private + self.shmem;
        // The Private_Hugetlb label takes one more column and the number one less, keeping the
        // total width
        let mut put = |label: &str, kb: u64| {
            let w = 24 - label.len().max(16);
            writeln!(s, "{label:<16}{kb:>w$} kB").unwrap()
        };
        put("Rss:", self.rss);
        put("Pss:", (pss_anon + pss_shmem) / 1024 + self.file);
        put("Pss_Dirty:", (pss_anon + pss_shmem) / 1024);
        if rollup {
            put("Pss_Anon:", pss_anon / 1024);
            put("Pss_File:", self.file);
            put("Pss_Shmem:", self.shmem);
        }
        put("Shared_Clean:", 0);
        put("Shared_Dirty:", self.shared);
        put("Private_Clean:", self.file);
        put("Private_Dirty:", dirty);
        put("Referenced:", self.rss);
        put("Anonymous:", self.anon);
        for label in [
            "LazyFree:",
            "AnonHugePages:",
            "ShmemPmdMapped:",
            "FilePmdMapped:",
        ] {
            put(label, 0);
        }
        put("Shared_Hugetlb:", 0);
        put("Private_Hugetlb:", 0);
        put("Swap:", 0);
        put("SwapPss:", 0);
        put("Locked:", self.locked);
    }
}

/// `/proc/<pid>/smaps`, or `smaps_rollup` with `rollup` (the total over all mappings). Resident
/// pages are counted in host memory (mincore).
fn smaps(store: &Store, task: &Task, rollup: bool) -> String {
    let res = task.res.borrow();
    let mm = res.mm.borrow();
    let regions = mm.regions();
    let mut s = String::new();
    let mut total = Usage::default();
    for r in &regions {
        let rss = mm.resident_in(r.start, r.end - r.start) * 4;
        let mut u = Usage {
            rss,
            locked: if r.locked { rss } else { 0 },
            ..Usage::default()
        };
        match (r.file, r.shared) {
            (Some(_), _) => u.file = rss,
            (None, true) => u.shmem = rss,
            (None, false) => {
                let (pages, pss) = mm.cow_shared(r.start, r.end);
                u.anon = rss;
                // Lent pages are usually resident; those swapped out to zram are not resident,
                // and so not shared either
                u.shared = (pages * 4).min(rss);
                u.shared_pss = pss.min(u.shared * 1024);
            }
        }
        total.add(&u);
        if rollup {
            continue;
        }
        s += &map_line(store, r);
        let kb = (r.end - r.start) / 1024;
        writeln!(s, "{:<16}{kb:>8} kB", "Size:").unwrap();
        writeln!(s, "{:<16}{:>8} kB", "KernelPageSize:", 4).unwrap();
        writeln!(s, "{:<16}{:>8} kB", "MMUPageSize:", 4).unwrap();
        u.write(&mut s, false);
        writeln!(s, "THPeligible:    0").unwrap();
        s += &vm_flags(r);
    }
    if rollup && let (Some(first), Some(last)) = (regions.first(), regions.last()) {
        let line = format!(
            "{:08x}-{:08x} ---p 00000000 00:00 0 ",
            first.start, last.end
        );
        writeln!(s, "{line:<73}[rollup]").unwrap();
        total.write(&mut s, true);
    }
    s
}

/// The VmFlags line of smaps, in Linux's flag-bit order.
fn vm_flags(r: &Region) -> String {
    let flags = [
        (r.prot & 1 != 0, "rd"),
        (r.prot & 2 != 0, "wr"),
        (r.prot & 4 != 0, "ex"),
        (r.shared, "sh"),
        (true, "mr"),
        (r.may_write, "mw"),
        (r.may_exec, "me"),
        (r.shared, "ms"),
        (r.name.as_deref() == Some(b"[stack]"), "gd"),
        (r.locked, "lo"),
        (r.dontfork, "dc"),
        // Commit-accounted: writable private anonymous mappings
        (r.file.is_none() && !r.shared && r.prot & 2 != 0, "ac"),
        (r.wipeonfork, "wf"),
        (r.dontdump, "dd"),
    ];
    let mut s = String::from("VmFlags: ");
    for (on, name) in flags {
        if on {
            s += name;
            s.push(' ');
        }
    }
    s.push('\n');
    s
}

/// One line of maps. As on Linux, lines without a name also end in a space, the name starts at
/// column 74, and a newline in a file's path is escaped as `\012` (seq_file_path).
fn map_line(store: &Store, r: &Region) -> String {
    let perms = format!(
        "{}{}{}{}",
        if r.prot & 1 != 0 { 'r' } else { '-' },
        if r.prot & 2 != 0 { 'w' } else { '-' },
        if r.prot & 4 != 0 { 'x' } else { '-' },
        if r.shared { 's' } else { 'p' },
    );
    // The device number of file mappings is the root filesystem's 8:1 (the same as stat's
    // st_dev)
    let (offset, dev, ino) = match r.file {
        Some((offset, ino)) => (offset, "08:01", ino),
        None => (0, "00:00", 0),
    };
    let line = format!(
        "{:08x}-{:08x} {perms} {offset:08x} {dev} {ino} ",
        r.start, r.end
    );
    match &r.name {
        Some(name) => {
            let name = match r.file {
                Some((_, ino)) => {
                    let path = mark_deleted(store, ino, name.to_vec());
                    let mut escaped = Vec::with_capacity(path.len());
                    for b in path {
                        match b {
                            b'\n' => escaped.extend_from_slice(b"\\012"),
                            b => escaped.push(b),
                        }
                    }
                    escaped
                }
                None => name.to_vec(),
            };
            format!("{line:<73}{}\n", String::from_utf8_lossy(&name))
        }
        None => format!("{line}\n"),
    }
}

/// As with Linux's `d_path`, deleted files (still open or mapped) get ` (deleted)` appended to
/// their path: `/proc/<pid>/fd/<n>`, maps, smaps. After the last name is removed in the store,
/// the inode's row stays with nlink = 0 until the store is next opened exclusively, so the check
/// uses nlink. Linux looks at the name used at open and also appends it when only that name was
/// removed while other hard links remain; here it does not. When apt upgrades, dpkg replaces old
/// libraries with rename, and needrestart finds the processes to restart by the ` (deleted)` in
/// maps.
pub(crate) fn mark_deleted(store: &Store, ino: Ino, name: Vec<u8>) -> Vec<u8> {
    if store.attrs(ino).is_ok_and(|st| st.nlink == 0) {
        with_deleted(name)
    } else {
        name
    }
}

/// `name` with ` (deleted)` appended, unless it already ends with it.
fn with_deleted(mut name: Vec<u8>) -> Vec<u8> {
    const DELETED: &[u8] = b" (deleted)";
    if !name.ends_with(DELETED) {
        name.extend_from_slice(DELETED);
    }
    name
}

/// Reads `/proc/<pid>/pagemap`: 8 bytes per page; the position and length must both be
/// multiples of 8.
pub(crate) fn read_pagemap(k: &Kernel, tid: Tid, buf: &mut [u8], pos: u64) -> Result<usize, Errno> {
    if !pos.is_multiple_of(8) || !buf.len().is_multiple_of(8) {
        return Err(Errno::EINVAL);
    }
    // The thread exited after the open (which refuses one already gone, ESRCH): its address space
    // is gone, end of file, rather than another thread's (a reader going to the end would walk
    // all 48 bits)
    let Some(task) = k.tasks.get(&tid).cloned() else {
        return Ok(0);
    };
    // The user address space is 48 bits
    let pages_total = 1u64 << (48 - 12);
    let first = pos / 8;
    let n = (buf.len() as u64 / 8).min(pages_total.saturating_sub(first)) as usize;
    let entries = task.res.borrow().mm.borrow().pagemap(first * 4096, n);
    for (i, e) in entries.iter().enumerate() {
        buf[i * 8..i * 8 + 8].copy_from_slice(&e.to_le_bytes());
    }
    Ok(n * 8)
}

/// Reads `/proc/<pid>/mem` (`mm` is the address space at open; once it is released reads get
/// EOF): the position is the address. Reads page by page and stops at a page that cannot be read;
/// when not a single byte was read, it is EIO (Linux's `mem_rw`). As Linux's access_remote_vm,
/// a page below a grows-down mapping grows it first, within the reader's `RLIMIT_STACK`
/// (`limit`).
pub(crate) fn read_mem(
    mm: Option<Rc<RefCell<crate::mm::Mm>>>,
    buf: &mut [u8],
    pos: u64,
    limit: u64,
) -> Result<usize, Errno> {
    let Some(mm) = mm else {
        return Ok(0);
    };
    let mut done = 0;
    while done < buf.len() {
        let Some(addr) = pos.checked_add(done as u64) else {
            break;
        };
        let n = ((PAGE_SIZE - addr % PAGE_SIZE) as usize).min(buf.len() - done);
        let mut read =
            |mm: &crate::mm::Mm| mm.read(UserSlice::new(addr, n), &mut buf[done..done + n]);
        if read(&mm.borrow()).is_err()
            && !(mm.borrow_mut().grow_for(addr, n, limit) && read(&mm.borrow()).is_ok())
        {
            break;
        }
        done += n;
    }
    if done == 0 && !buf.is_empty() {
        return Err(Errno::EIO);
    }
    Ok(done)
}

/// Writes `/proc/<pid>/mem`: a forced write like PTRACE_POKETEXT ([`Mm::write_force`]), page by
/// page, stopping at a page that cannot be written; grows grows-down mappings as [`read_mem`]
/// does. `current` is the writing thread.
///
/// [`Mm::write_force`]: crate::mm::Mm::write_force
pub(crate) fn write_mem(
    mm: Option<Rc<RefCell<crate::mm::Mm>>>,
    data: &[u8],
    pos: u64,
    current: &Arc<crate::task::TaskSync>,
    limit: u64,
) -> Result<usize, Errno> {
    let Some(mm) = mm else {
        return Ok(0);
    };
    let mut mm = mm.borrow_mut();
    let mut done = 0;
    while done < data.len() {
        let Some(addr) = pos.checked_add(done as u64) else {
            break;
        };
        let n = ((PAGE_SIZE - addr % PAGE_SIZE) as usize).min(data.len() - done);
        let page = &data[done..done + n];
        if mm.write_force(addr, page, current).is_err()
            && !(mm.grow_for(addr, n, limit) && mm.write_force(addr, page, current).is_ok())
        {
            break;
        }
        done += n;
    }
    if done == 0 && !data.is_empty() {
        return Err(Errno::EIO);
    }
    Ok(done)
}

/// `/proc/<pid>/mounts` (`info` false) or `mountinfo`: as with Linux's `show_vfsmnt` and
/// `show_mountinfo`, lists the mounts in that process's mount namespace `ns` in mount order, with
/// mount points relative to its root directory `root`; those outside the root directory are not
/// listed (after chroot the mounts outside are invisible, the root filesystem included). Mount ids
/// match fdinfo's `mnt_id`, and device numbers match stat's `st_dev`.
fn mounts(ps: &View<'_>, root: &Node, ns: u64, info: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for (m, point) in ps.vfs.mounts_from(ps, root, ns) {
        let Some(point) = point else {
            continue;
        };
        MountLine {
            id: m.id,
            // The root of the mount tree sits on the invisible rootfs (on Linux its id is not in
            // the list), so write 0
            parent: m.parent().unwrap_or(0),
            dev: m.dev,
            fs_root: ps.vfs.root_path(ps, &m),
            point,
            flags: m.flags,
            fstype: ps.vfs.type_of(&m),
            source: if m.source.is_empty() {
                b"none"
            } else {
                &m.source
            },
            options: ps.vfs.sb_options(&m),
            sb_flags: ps.vfs.sb_flags(&m),
            prop: m.prop,
        }
        .write(&mut out, info);
    }
    out
}

/// One line of mounts or mountinfo.
struct MountLine<'a> {
    id: crate::vfs::MountId,
    parent: crate::vfs::MountId,
    dev: u64,
    /// The path within the filesystem of the part the mount sees.
    fs_root: Vec<u8>,
    point: Vec<u8>,
    flags: u64,
    fstype: String,
    source: &'a [u8],
    /// Superblock options ([`crate::vfs::Vfs::sb_options`]).
    options: String,
    /// The filesystem's own flags ([`crate::vfs::Vfs::sb_flags`]): mountinfo shows them in the
    /// superblock options, mounts with the mount's.
    sb_flags: u64,
    /// Shown in mountinfo's optional fields.
    prop: crate::vfs::Propagation,
}

impl MountLine<'_> {
    fn write(&self, out: &mut Vec<u8>, info: bool) {
        use crate::vfs::ms;
        let (fstype, options) = (&self.fstype, &self.options);
        let sb_ro = self.sb_flags & ms::RDONLY != 0;
        let sb = if sb_ro { "ro" } else { "rw" };
        let sb_opts = sb_options(self.sb_flags);
        if info {
            let (id, parent) = (self.id, self.parent);
            let (major, minor) = (self.dev >> 8, self.dev & 0xff);
            out.extend_from_slice(format!("{id} {parent} {major}:{minor} ").as_bytes());
            out.extend(mangle(&self.fs_root));
            out.push(b' ');
            out.extend(mangle(&self.point));
            let opts = mount_options(self.flags);
            out.extend_from_slice(format!(" {opts}").as_bytes());
            // The optional fields (show_mountinfo): peer group, master, unbindable
            let p = self.prop;
            if p.group != 0 {
                out.extend_from_slice(format!(" shared:{}", p.group).as_bytes());
            }
            if p.master != 0 {
                out.extend_from_slice(format!(" master:{}", p.master).as_bytes());
            }
            if p.unbindable {
                out.extend_from_slice(b" unbindable");
            }
            out.extend_from_slice(format!(" - {fstype} ").as_bytes());
            out.extend(mangle(self.source));
            out.extend_from_slice(format!(" {sb}{sb_opts}{options}\n").as_bytes());
        } else {
            // show_vfsmnt: read-only if the mount or the filesystem is, the superblock's flags,
            // then the mount's
            let ro = if sb_ro { ms::RDONLY } else { 0 };
            let opts = mount_options(self.flags | ro);
            let (rw, mnt) = opts.split_at(2);
            out.extend(mangle(self.source));
            out.push(b' ');
            out.extend(mangle(&self.point));
            out.extend_from_slice(
                format!(" {fstype} {rw}{sb_opts}{mnt}{options} 0 0\n").as_bytes(),
            );
        }
    }
}

/// Mount options (the sixth column of mountinfo, Linux's `show_mnt_opts`).
fn mount_options(flags: u64) -> String {
    use crate::vfs::ms;
    let mut s = String::from(if flags & ms::RDONLY != 0 { "ro" } else { "rw" });
    for (bit, name) in [
        (ms::NOSUID, ",nosuid"),
        (ms::NODEV, ",nodev"),
        (ms::NOEXEC, ",noexec"),
        (ms::NOATIME, ",noatime"),
        (ms::NODIRATIME, ",nodiratime"),
        (ms::RELATIME, ",relatime"),
        (ms::NOSYMFOLLOW, ",nosymfollow"),
    ] {
        if flags & bit != 0 {
            s += name;
        }
    }
    s
}

/// The superblock's own options (Linux's `show_sb_opts`).
fn sb_options(sb: u64) -> String {
    use crate::vfs::ms;
    let mut s = String::new();
    for (bit, name) in [
        (ms::SYNCHRONOUS, ",sync"),
        (ms::DIRSYNC, ",dirsync"),
        (ms::MANDLOCK, ",mand"),
        (ms::LAZYTIME, ",lazytime"),
    ] {
        if sb & bit != 0 {
            s += name;
        }
    }
    s
}

/// As with Linux's `mangle`, writes spaces, tabs, newlines and backslashes as octal escapes.
fn mangle(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for &b in s {
        match b {
            b' ' | b'\t' | b'\n' | b'\\' => out.extend_from_slice(format!("\\{b:03o}").as_bytes()),
            _ => out.push(b),
        }
    }
    out
}

/// `/proc/<pid>/uid_map`, `gid_map`, `projid_map` (Linux's `uid_m_show`): each extent is "first ID
/// in the namespace, first ID in the parent, count", 10 columns each. The second column is
/// translated through the reader's namespace; when the reader is in this namespace itself,
/// through the parent's. Unmapped values are written as 4294967295. Only the boot-time namespace
/// has a projid_map.
fn id_map(reader: &Cred, ns: &Rc<crate::cred::UserNs>, e: Entry) -> String {
    let lower = match &ns.parent {
        Some(parent) if reader.user_ns.id == ns.id => parent.clone(),
        _ => reader.user_ns.clone(),
    };
    let (map, lower_map) = match e {
        Entry::GidMap => (ns.gid_map.borrow().clone(), lower.gid_map.borrow().clone()),
        Entry::UidMap => (ns.uid_map.borrow().clone(), lower.uid_map.borrow().clone()),
        _ if ns.level == 0 => return format!("{:>10} {:>10} {:>10}\n", 0, 0, u32::MAX),
        _ => return String::new(),
    };
    map.extents()
        .iter()
        .map(|&(first, k, count)| {
            let shown = lower_map.up(k).unwrap_or(u32::MAX);
            format!("{first:>10} {shown:>10} {count:>10}\n")
        })
        .collect()
}

/// `/proc/<pid>/fdinfo/<n>`: position, flags (plus close-on-exec; files opened with open carry
/// `O_LARGEFILE`), mount id, inode, the locks held through it, and the extra lines of eventfd,
/// inotify, pidfd, epoll, timerfd and signalfd.
fn fdinfo(ps: &View<'_>, ns: &PidNs, pid: Tid, fd: i32) -> Result<String, Errno> {
    let task = ps.leader(pid).ok_or(Errno::ENOENT)?;
    let res = task.res.borrow();
    let fds = res.fds.borrow();
    let file = fds.get(fd).map_err(|_| Errno::ENOENT)?;
    let cloexec = if fds.cloexec(fd)? {
        oflags::O_CLOEXEC
    } else {
        0
    };
    let f = file.borrow();
    let mnt_id = f.node().map_or(crate::vfs::ANON_MOUNT, |n| n.mnt());
    let mut s = format!(
        "pos:\t{}\nflags:\t0{:o}\nmnt_id:\t{mnt_id}\nino:\t{}\n",
        f.pos,
        f.flags | cloexec,
        f.ino().unwrap_or(0)
    );
    // Only files and tables that have taken locks can hold any (most fds have none)
    if f.holds_locks || fds.holds_locks {
        s += &crate::locks::fd_locks(ps.k, ps.vfs, ns, &res.fds, &file);
    }
    match &f.kind {
        FileKind::EventFd(e) => s += &format!("eventfd-count: {:16x}\n", e.borrow().count),
        FileKind::Inotify(i) => s += &i.borrow().fdinfo(),
        // The number in the mount's pid namespace (0 when not visible); NSpid goes on to list the
        // lower levels
        FileKind::Pidfd(p) => {
            let nr = if p.exited() {
                -1
            } else {
                ns.vnr(p.tgid).unwrap_or(0)
            };
            s += &format!("Pid:\t{nr}\nNSpid:\t{nr}");
            if nr > 0 {
                let levels = ps.k.pid_ns_of(p.tgid).levels_from(ns.level + 1);
                for n in levels {
                    s += &format!("\t{}", n.vnr(p.tgid).unwrap_or(0));
                }
            }
            s.push('\n');
        }
        FileKind::Epoll(e) => s += &e.borrow().fdinfo(),
        FileKind::TimerFd(t) => s += &t.borrow_mut().fdinfo(),
        FileKind::SignalFd(sfd) => s += &format!("sigmask:\t{:016x}\n", sfd.borrow().mask),
        _ => {}
    }
    Ok(s)
}

// ---- System-wide files ----

/// The names of the HWCAP bits in `/proc/cpuinfo` (Linux 6.1's `hwcap_str`), in bit order.
const HWCAP_NAMES: [&str; 30] = [
    "fp", "asimd", "evtstrm", "aes", "pmull", "sha1", "sha2", "crc32", "atomics", "fphp",
    "asimdhp", "cpuid", "asimdrdm", "jscvt", "fcma", "lrcpc", "dcpop", "sha3", "sm3", "sm4",
    "asimddp", "sha512", "sve", "asimdfhm", "dit", "uscat", "ilrcpc", "flagm", "ssbs", "sb",
];

/// Likewise, the names of the HWCAP2 bits, which come after HWCAP's.
const HWCAP2_NAMES: [&str; 20] = [
    "dcpodp",
    "sve2",
    "sveaes",
    "svepmull",
    "svebitperm",
    "svesha3",
    "svesm4",
    "flagm2",
    "frint",
    "svei8mm",
    "svef32mm",
    "svef64mm",
    "svebf16",
    "i8mm",
    "bf16",
    "dgh",
    "rng",
    "bti",
    "mte",
    "ecv",
];

fn cpuinfo() -> String {
    let named = |names: &'static [&'static str], caps: u64| {
        names
            .iter()
            .enumerate()
            .filter(move |(i, _)| caps & (1 << i) != 0)
            .map(|(_, n)| *n)
    };
    let features: Vec<&str> = named(&HWCAP_NAMES, tuxport_engine::guest_hwcap())
        .chain(named(&HWCAP2_NAMES, tuxport_engine::guest_hwcap2()))
        .collect();
    let midr = tuxport_engine::guest_midr();
    let mut s = String::new();
    for cpu in 0..cpus() {
        writeln!(s, "processor\t: {cpu}\nBogoMIPS\t: 100.00").unwrap();
        writeln!(s, "Features\t: {}", features.join(" ")).unwrap();
        writeln!(s, "CPU implementer\t: {:#04x}", (midr >> 24) & 0xff).unwrap();
        writeln!(s, "CPU architecture: 8").unwrap();
        writeln!(s, "CPU variant\t: {:#x}", (midr >> 20) & 0xf).unwrap();
        writeln!(s, "CPU part\t: {:#05x}", (midr >> 4) & 0xfff).unwrap();
        writeln!(s, "CPU revision\t: {}\n", midr & 0xf).unwrap();
    }
    s
}

/// `/proc/meminfo`: copied from the host (as in a docker container); for the limit the guest can
/// use, see the cgroup's `memory.max`. It used to be computed from sysinfo, with `MemAvailable`
/// equal to `MemFree` and all page cache counted as used.
fn meminfo() -> String {
    let m = tuxport_hal::host_meminfo();
    let mut s = String::new();
    for (name, bytes) in [
        ("MemTotal", m.total),
        ("MemFree", m.free),
        ("MemAvailable", m.available),
        ("Buffers", m.buffers),
        ("Cached", m.cached),
        ("SwapCached", m.swap_cached),
        ("Active", m.active),
        ("Inactive", m.inactive),
        ("SwapTotal", m.swap_total),
        ("SwapFree", m.swap_free),
        ("Dirty", m.dirty),
        ("AnonPages", m.anon_pages),
        ("Mapped", m.mapped),
        ("Shmem", m.shmem),
        ("Slab", m.slab),
        ("SReclaimable", m.sreclaimable),
    ] {
        writeln!(s, "{:<16}{:>8} kB", format!("{name}:"), bytes / 1024).unwrap();
    }
    s
}

/// `/proc/vmstat`: like `meminfo`, the host's figures (as a container sees its host's) when the
/// host lets the app read them; otherwise the page counts `meminfo` gives and the event counters
/// at 0, in Linux's order, so procps' vmstat and memory monitors still find the file.
fn vmstat(host: Option<String>, m: &tuxport_hal::MemInfo) -> String {
    if let Some(text) = host {
        return text;
    }
    let pages = |bytes: u64| bytes / 4096;
    let mut s = String::new();
    for (name, value) in [
        ("nr_free_pages", pages(m.free)),
        ("nr_slab_reclaimable", pages(m.sreclaimable)),
        (
            "nr_slab_unreclaimable",
            pages(m.slab.saturating_sub(m.sreclaimable)),
        ),
        ("nr_anon_pages", pages(m.anon_pages)),
        ("nr_mapped", pages(m.mapped)),
        ("nr_file_pages", pages(m.cached + m.buffers + m.swap_cached)),
        ("nr_dirty", pages(m.dirty)),
        ("nr_writeback", 0),
        ("nr_shmem", pages(m.shmem)),
        ("pgpgin", 0),
        ("pgpgout", 0),
        ("pswpin", 0),
        ("pswpout", 0),
        ("pgfault", 0),
        ("pgmajfault", 0),
        ("oom_kill", 0),
    ] {
        writeln!(s, "{name} {value}").unwrap();
    }
    s
}

fn global_stat(ps: &View<'_>) -> String {
    let n = cpus() as u64;
    let up = ticks(uptime_ns());
    let cpu = ps.k.tasks.values().fold(ps.k.cpu, |sum, t| sum + t.cpu());
    let (user, system) = (ticks(cpu.user_ns()), ticks(cpu.sys_ns));
    let idle = (up * n).saturating_sub(user + system);
    let running =
        ps.k.tasks
            .values()
            .filter(|t| t.sync.in_guest.load(std::sync::atomic::Ordering::Relaxed))
            .count();
    // Linux's getboottime64 (realtime minus boottime, subtracted in nanoseconds) moved back by the
    // reader's time-namespace boottime offset (timens_sub_boottime), so btime plus a start time
    // from /proc/<pid>/stat is the wall-clock start in any namespace (ps -o lstart). Truncating
    // both clocks to seconds before subtracting made btime alternate between two seconds.
    let now =
        tuxport_hal::clock_gettime(0).map_or(0, |(s, ns)| s as i128 * 1_000_000_000 + ns as i128);
    let btime = ((now - ps.with_boot_offset(uptime_ns()) as i128) / 1_000_000_000).max(0);
    let mut s = String::new();
    writeln!(s, "cpu  {user} 0 {system} {idle} 0 0 0 0 0 0").unwrap();
    for i in 0..n {
        writeln!(
            s,
            "cpu{i} {} 0 {} {} 0 0 0 0 0 0",
            user / n,
            system / n,
            idle / n
        )
        .unwrap();
    }
    writeln!(s, "intr 0\nctxt 0\nbtime {btime}\nprocesses {}", ps.k.forks).unwrap();
    // softirq is the total and then one count for each of Linux's 10 softirq types (show_stat);
    // parsers such as prometheus/procfs, which BuildKit reads it with, refuse fewer
    writeln!(
        s,
        "procs_running {}\nprocs_blocked 0\nsoftirq 0{}",
        running.max(1),
        " 0".repeat(10)
    )
    .unwrap();
    s
}

fn ticks(ns: u64) -> u64 {
    ns / (1_000_000_000 / HZ)
}

/// Nanoseconds since boot (CLOCK_BOOTTIME).
pub(crate) fn uptime_ns() -> u64 {
    tuxport_hal::clock_gettime(tuxport_hal::CLOCK_BOOTTIME)
        .map(|(s, ns)| s as u64 * 1_000_000_000 + ns as u64)
        .unwrap_or(0)
}

fn random_uuid() -> String {
    let mut b = [0u8; 16];
    let _ = tuxport_hal::fill_random(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format_uuid(&b)
}

pub(crate) fn format_uuid(b: &[u8; 16]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

// ---- /proc/<pid>/net ----

/// All open sockets (each counted once), sorted by inode, with the fsuid of the first thread
/// found holding each in its fd table (Linux shows the creator's), or 0 for sockets that are only
/// in SCM_RIGHTS messages. Sockets waiting to be accepted do not count.
pub(crate) fn sockets(k: &Kernel) -> Vec<(SocketRef, u32)> {
    let mut tables = HashSet::new();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for task in k.tasks.values() {
        let fds = task.res.borrow().fds.clone();
        if !tables.insert(Rc::as_ptr(&fds)) {
            continue;
        }
        let uid = task.cred.borrow().fsuid;
        let fds = fds.borrow();
        for file in fds.open_fds().into_iter().filter_map(|fd| fds.get(fd).ok()) {
            if let FileKind::Socket(s) = &file.borrow().kind
                && seen.insert(Rc::as_ptr(s))
            {
                out.push((s.clone(), uid));
            }
        }
    }
    // Those only in SCM_RIGHTS messages
    for s in crate::unix::inflight_sockets(k) {
        if seen.insert(Rc::as_ptr(&s)) {
            out.push((s, 0));
        }
    }
    out.sort_by_key(|(s, _)| socket_ino(s));
    out
}

/// A socket's inode number (fstat, the `socket:[n]` of `/proc/<pid>/fd`).
pub(crate) fn socket_ino(s: &SocketRef) -> u64 {
    Rc::as_ptr(s) as u64 >> 4
}

/// The id of the network namespace a socket is in.
fn socket_net(s: &crate::net::Socket) -> u64 {
    use crate::net::Socket;
    let net = match s {
        Socket::Inet(i) => i.net.as_ref(),
        Socket::Netlink(n) => n.net.as_ref(),
        Socket::Unix(u) => u.net.as_ref(),
    };
    net.map_or(crate::nsproxy::INIT_NET_NS, |n| n.id)
}

/// `/proc/net/dev`: interfaces `ifaces` with counters for the guest's own traffic
/// ([`crate::traffic`]; all 0 when `counted` is false), in the same format as Linux's
/// `dev_seq_printf_stats`.
fn net_dev(k: &Kernel, ifaces: &[tuxport_hal::net::Interface], counted: bool) -> String {
    let mut s = String::from(
        "Inter-|   Receive                                                |  Transmit\n \
         face |bytes    packets errs drop fifo frame compressed multicast|\
         bytes    packets errs drop fifo colls carrier compressed\n",
    );
    for i in ifaces {
        let st = if counted {
            k.traffic.get(&i.name)
        } else {
            Default::default()
        };
        writeln!(
            s,
            "{:>6}: {:>7} {:>7} {:>4} {:>4} {:>4} {:>5} {:>10} {:>9} {:>8} {:>7} {:>4} {:>4} {:>4} \
             {:>5} {:>7} {:>10}",
            i.name,
            st.rx_bytes,
            st.rx_packets,
            0,
            0,
            0,
            0,
            0,
            0,
            st.tx_bytes,
            st.tx_packets,
            0,
            0,
            0,
            0,
            0,
            0
        )
        .unwrap();
    }
    s
}

/// `/proc/net/if_inet6`: the IPv6 addresses on the host's interfaces, in the same format as
/// Linux's `if6_seq_show`. OpenJDK uses it to decide whether IPv6 exists (`IPv6_supported`), and
/// turns IPv6 off entirely when the file is missing or empty. The scope comes from the address,
/// as in Linux's `ipv6_addr_scope`; the flags are `IFA_F_PERMANENT`, as in the netlink replies
/// (getifaddrs cannot tell temporary addresses apart).
fn net_if_inet6(ifaces: &[tuxport_hal::net::Interface]) -> String {
    let mut s = String::new();
    for i in ifaces {
        for (_, addr, prefix) in i.addrs.iter().filter(|(f, ..)| *f == AF_INET6) {
            let scope = match addr[..] {
                [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1] => 0x10,
                [0xfe, b, ..] if b & 0xc0 == 0x80 => 0x20,
                [0xfe, b, ..] if b & 0xc0 == 0xc0 => 0x40,
                _ => 0,
            };
            let hex: String = addr.iter().map(|b| format!("{b:02x}")).collect();
            writeln!(
                s,
                "{hex} {:02x} {prefix:02x} {scope:02x} 80 {:>8}",
                i.index, i.name
            )
            .unwrap();
        }
    }
    s
}

// ---- sysfs ----

/// The files under `/sys/class/net/<interface>`, with interfaces numbered as in the reader's
/// network namespace (unlike Linux, which looks at the namespace of the process that mounted
/// sysfs). As in the netlink replies, interface types are only loopback and Ethernet; `flags` is
/// `dev->flags` as on Linux, without state-derived bits such as `IFF_RUNNING`. For virtual devices
/// ([`crate::netdev`]) `iflink` is the veth peer, and `operstate` can be lowerlayerdown.
fn net_file(
    k: &Kernel,
    ns: &crate::nsproxy::NetNs,
    index: u32,
    path: &[u8],
) -> Result<String, Errno> {
    const IFF_UP: u32 = 0x1;
    const IFF_LOOPBACK: u32 = 0x8;
    const IFF_RUNNING: u32 = 0x40;
    // IFF_RUNNING, IFF_LOWER_UP, IFF_DORMANT, IFF_ECHO (only dev_get_flags adds them)
    const STATE_FLAGS: u32 = 0x40 | 0x1_0000 | 0x2_0000 | 0x4_0000;
    let virt = crate::netdev::views(ns)
        .into_iter()
        .find(|v| v.index == index);
    if let Some(v) = virt {
        let mac = |m: [u8; 6]| {
            let hex: Vec<String> = m.iter().map(|b| format!("{b:02x}")).collect();
            format!("{}\n", hex.join(":"))
        };
        return Ok(match path {
            b"addr_len" => "6\n".to_owned(),
            b"address" => mac(v.mac),
            b"broadcast" => mac([0xff; 6]),
            b"carrier" if v.flags & IFF_UP == 0 => return Err(Errno::EINVAL),
            b"carrier" => format!("{}\n", u8::from(v.carrier)),
            b"flags" => format!("{:#x}\n", v.flags & !STATE_FLAGS),
            b"ifindex" => format!("{index}\n"),
            b"iflink" => format!("{}\n", v.link.as_ref().map_or(index, |l| l.0)),
            b"mtu" => format!("{}\n", v.mtu),
            b"operstate" => match v.operstate {
                0 => "unknown\n",
                3 => "lowerlayerdown\n",
                6 => "up\n",
                _ => "down\n",
            }
            .to_owned(),
            b"tx_queue_len" => format!("{}\n", v.txqlen),
            b"type" => "1\n".to_owned(),
            // Bridge parameters are all defaults, with STP off (crate::netdev::bridge_info)
            b"bridge/ageing_time" => "30000\n".to_owned(),
            b"bridge/forward_delay" => "1500\n".to_owned(),
            b"bridge/hello_time" => "200\n".to_owned(),
            b"bridge/max_age" => "2000\n".to_owned(),
            b"bridge/priority" => "32768\n".to_owned(),
            b"bridge/stp_state" => "0\n".to_owned(),
            b"bridge/bridge_id" | b"bridge/root_id" => {
                let hex: String = v.mac.iter().map(|b| format!("{b:02x}")).collect();
                format!("8000.{hex}\n")
            }
            b"brport/state" => format!("{}\n", v.port_state.unwrap_or(0)),
            b"brport/priority" => "32\n".to_owned(),
            b"brport/path_cost" => "2\n".to_owned(),
            p if p.starts_with(b"statistics/") && !p.ends_with(b"/") => "0\n".to_owned(),
            _ => return Err(Errno::EISDIR),
        });
    }
    // The interface is gone: on Linux the directory disappears with it, and files already open
    // read ENODEV
    let i = crate::netdev::base(ns)
        .into_iter()
        .find(|i| i.index == index)
        .ok_or(Errno::ENODEV)?;
    let loopback = i.flags & IFF_LOOPBACK != 0;
    let mac = |m: [u8; 6]| {
        let hex: Vec<String> = m.iter().map(|b| format!("{b:02x}")).collect();
        format!("{}\n", hex.join(":"))
    };
    let st = k.traffic.get(&i.name);
    Ok(match path {
        b"addr_len" => "6\n".to_owned(),
        b"address" => mac(i.mac.unwrap_or([0; 6])),
        b"broadcast" => mac(if loopback { [0; 6] } else { [0xff; 6] }),
        // On Linux reading carrier of a down interface is EINVAL
        b"carrier" if i.flags & IFF_UP == 0 => return Err(Errno::EINVAL),
        b"carrier" => format!("{}\n", u8::from(i.flags & IFF_RUNNING != 0)),
        b"flags" => format!("{:#x}\n", i.flags & !STATE_FLAGS),
        b"ifindex" | b"iflink" => format!("{index}\n"),
        b"mtu" => format!("{}\n", i.mtu),
        b"operstate" => match crate::net::operstate(&i) {
            0 => "unknown\n",
            6 => "up\n",
            _ => "down\n",
        }
        .to_owned(),
        b"tx_queue_len" => format!(
            "{}\n",
            tuxport_hal::net::tx_queue_len(&i.name).unwrap_or(1000)
        ),
        b"type" => format!("{}\n", if loopback { 772 } else { 1 }),
        b"statistics/rx_bytes" => format!("{}\n", st.rx_bytes),
        b"statistics/rx_packets" => format!("{}\n", st.rx_packets),
        b"statistics/tx_bytes" => format!("{}\n", st.tx_bytes),
        b"statistics/tx_packets" => format!("{}\n", st.tx_packets),
        p if p.starts_with(b"statistics/") && !p.ends_with(b"/") => "0\n".to_owned(),
        _ => return Err(Errno::EISDIR),
    })
}

/// The files under `/sys/devices/system/cpu/cpu<n>`. All CPUs are the same (in `/proc/cpuinfo`
/// too), each its own core, with no hyperthreading, all in one package.
fn cpu_file(n: usize, path: &[u8]) -> Result<String, Errno> {
    let all = 0..cpus();
    let me = n..n + 1;
    Ok(match path {
        b"online" => "1\n".to_owned(),
        b"regs/identification/midr_el1" => format!("0x{:016x}\n", tuxport_engine::guest_midr()),
        b"regs/identification/revidr_el1" => format!("0x{:016x}\n", 0),
        b"topology/cluster_id" => "-1\n".to_owned(),
        b"topology/core_id" => format!("{n}\n"),
        b"topology/physical_package_id" => "0\n".to_owned(),
        b"topology/cluster_cpus" | b"topology/core_cpus" | b"topology/thread_siblings" => {
            cpu_mask(me)
        }
        b"topology/cluster_cpus_list"
        | b"topology/core_cpus_list"
        | b"topology/thread_siblings_list" => cpu_list(me),
        b"topology/core_siblings" | b"topology/package_cpus" => cpu_mask(all),
        b"topology/core_siblings_list" | b"topology/package_cpus_list" => cpu_list(all),
        _ if HOST_CPU_FILES.contains(&path) => return host_cpu_file(n, path),
        _ => return Err(Errno::EISDIR),
    })
}

/// CPU list form (`%*pbl`): `0-3`, or `2` when there is only one.
fn cpu_list(set: Range<usize>) -> String {
    cpu_list_of(&set.collect::<Vec<_>>())
}

/// CPU mask form (`%*pb`): hexadecimal, comma-separated groups of 32 bits, with the highest group
/// zero-padded to enough digits for the total CPU count (`f` for 4 CPUs; with 8, cpu0 is `01`).
fn cpu_mask(set: Range<usize>) -> String {
    cpu_mask_of(&set.collect::<Vec<_>>())
}

/// Cache and frequency files copied from the host: host CPU numbers in CPU lists and masks are
/// changed to the guest's (CPUs the guest cannot use are removed).
fn host_cpu_file(n: usize, path: &[u8]) -> Result<String, Errno> {
    let rel = String::from_utf8_lossy(path).into_owned();
    let raw = host_cpu(n as u32)
        .and_then(|h| tuxport_hal::cpu_sysfs(h, &rel))
        .ok_or(Errno::ENOENT)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let guest = |host: Vec<usize>| -> Vec<usize> {
        let cpus = tuxport_hal::host_cpus();
        (0..cpus.len())
            .filter(|&i| host.contains(&cpus[i]))
            .collect()
    };
    Ok(match tree_name(path) {
        b"shared_cpu_list" => cpu_list_of(&guest(parse_cpu_list(text.trim()))),
        b"shared_cpu_map" => cpu_mask_of(&guest(parse_cpu_mask(text.trim()))),
        b"affected_cpus" | b"related_cpus" => {
            let host = text
                .split_whitespace()
                .filter_map(|w| w.parse().ok())
                .collect();
            let cpus: Vec<String> = guest(host).iter().map(usize::to_string).collect();
            format!("{}\n", cpus.join(" "))
        }
        _ => text,
    })
}

/// The CPUs in a CPU list (`0-3,5`).
fn parse_cpu_list(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.split(',').filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
            out.extend(a..=b.min(a + 4096));
        }
    }
    out
}

/// The CPUs in a CPU mask (hexadecimal, comma-separated groups of 32 bits, high first).
fn parse_cpu_mask(s: &str) -> Vec<usize> {
    let words: Vec<u32> = s
        .split(',')
        .filter_map(|w| u32::from_str_radix(w, 16).ok())
        .collect();
    let mut out = Vec::new();
    for (i, w) in words.iter().rev().enumerate() {
        out.extend((0..32).filter(|b| w >> b & 1 != 0).map(|b| i * 32 + b));
    }
    out
}

/// Writes a set of CPUs (in increasing order) in CPU list form ([`cpu_list`], `0-1,3`).
fn cpu_list_of(cpus: &[usize]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < cpus.len() {
        let mut j = i;
        while j + 1 < cpus.len() && cpus[j + 1] == cpus[j] + 1 {
            j += 1;
        }
        parts.push(if i == j {
            cpus[i].to_string()
        } else {
            format!("{}-{}", cpus[i], cpus[j])
        });
        i = j + 1;
    }
    format!("{}\n", parts.join(","))
}

/// Writes a set of CPUs in CPU mask form ([`cpu_mask`]).
fn cpu_mask_of(cpus: &[usize]) -> String {
    let bits = self::cpus();
    let groups = bits.div_ceil(32);
    let mut s = String::new();
    for g in (0..groups).rev() {
        let width = if g == groups - 1 {
            (bits - g * 32).div_ceil(4)
        } else {
            8
        };
        let word = cpus
            .iter()
            .filter(|&&c| c / 32 == g)
            .fold(0u32, |w, &c| w | 1 << (c % 32));
        if g != groups - 1 {
            s.push(',');
        }
        write!(s, "{word:0width$x}").unwrap();
    }
    s.push('\n');
    s
}

/// Whether the host CPU corresponding to `n` has `path` from [`HOST_CPU_FILES`] (relative to
/// `cpu<n>`, directories ending in `/`).
fn host_cpu_has(n: u32, path: &[u8]) -> bool {
    let rel = String::from_utf8_lossy(path.strip_suffix(b"/").unwrap_or(path)).into_owned();
    host_cpu(n).is_some_and(|h| tuxport_hal::cpu_sysfs(h, &rel).is_some())
}

/// The host CPU corresponding to guest CPU `n`.
fn host_cpu(n: u32) -> Option<usize> {
    tuxport_hal::host_cpus().get(n as usize).copied()
}

// ---- Writing ----

/// Writes a synthetic file: cgroup files go to [`crate::cgroup::write`], the id maps,
/// `setgroups` and `timens_offsets` to their own writers, and the other writable files check the
/// value and record it (namespaced sysctls in the writer's namespaces); some recorded values have
/// no effect, and `kernel/pid_max` accepts writes and ignores them. `cred`, `ns` and `pid_ns` are
/// the writer's, `opener` the credentials the file was opened with.
pub(crate) fn write(
    k: &mut Kernel,
    (tid, cred, opener): (Tid, &Cred, &Cred),
    (ns, pid_ns): (&crate::nsproxy::NsProxy, &PidNs),
    node: &PseudoNode,
    data: &[u8],
) -> Result<(), Errno> {
    // Opened for writing by root although not writable ([`unwritable_error`])
    if !writable(node) && !matches!(node, PseudoNode::Cgroup(..)) {
        return Err(unwritable_error(node).unwrap_or(Errno::EINVAL));
    }
    match node {
        PseudoNode::Cgroup(id, name) => {
            let writer = crate::cgroup::Writer {
                tid,
                pid_ns,
                ns_root: ns.cgroup.root,
                opener,
            };
            return crate::cgroup::write(k, &writer, (*id, name), data);
        }
        PseudoNode::Entry(pid, _, e @ (Entry::UidMap | Entry::GidMap)) => {
            return write_id_map(k, (cred, opener), *pid, *e == Entry::GidMap, data);
        }
        PseudoNode::Entry(pid, _, Entry::Setgroups) => {
            return write_setgroups(k, opener, *pid, data);
        }
        PseudoNode::Entry(pid, None, Entry::TimensOffsets) => {
            return write_timens_offsets(k, opener, *pid, data);
        }
        // Network namespace sysctls: the writer's namespace (crate::netconf)
        PseudoNode::NetConf(v6, dir, name) => {
            return crate::netdev::conf_write(&ns.net, *v6, *dir, name, data);
        }
        PseudoNode::NetSysctl(path) => {
            return crate::netconf::sysctl_write(&mut ns.net.devs.borrow_mut().conf, path, data);
        }
        PseudoNode::Static(b"sys/net/ipv4/ip_forward") => {
            return crate::netdev::conf_write(
                &ns.net,
                false,
                crate::netdev::ConfDir::All,
                b"forwarding",
                data,
            );
        }
        PseudoNode::Static(b"sys/net/core/somaxconn") => {
            let v = crate::netconf::parse_int(data)?;
            if !(0..=i32::MAX as i64).contains(&v) {
                return Err(Errno::EINVAL);
            }
            ns.net.devs.borrow_mut().conf.somaxconn = v;
            return Ok(());
        }
        PseudoNode::Static(b"sys/net/ipv4/ip_local_port_range") => {
            let mut d = ns.net.devs.borrow_mut();
            d.conf.port_range = crate::netconf::parse_port_range(data, d.conf.prot_sock)?;
            return Ok(());
        }
        PseudoNode::Static(b"sys/net/ipv4/ip_unprivileged_port_start") => {
            let mut d = ns.net.devs.borrow_mut();
            d.conf.prot_sock = crate::netconf::parse_prot_sock(data, d.conf.port_range.0)?;
            return Ok(());
        }
        PseudoNode::Static(b"sys/net/ipv4/ping_group_range") => {
            let r = crate::netconf::parse_ping_group(data, cred)?;
            ns.net.devs.borrow_mut().conf.ping_group = r;
            return Ok(());
        }
        _ => {}
    }
    if let PseudoNode::Static(path) = node
        && let Some(name) = path.strip_prefix(b"sys/kernel/")
        && k.ipc(ns.ipc.id).tunables.read(name).is_some()
    {
        return k.ipc_mut(ns.ipc.id).tunables.write(name, data);
    }
    if let PseudoNode::Static(path @ (b"sys/kernel/hostname" | b"sys/kernel/domainname")) = node {
        let name = data.strip_suffix(b"\n").unwrap_or(data);
        if name.len() > crate::kernel::UTS_LEN {
            return Err(Errno::EINVAL);
        }
        let mut slot = if *path == b"sys/kernel/hostname" {
            ns.uts.hostname.borrow_mut()
        } else {
            ns.uts.domainname.borrow_mut()
        };
        *slot = name.to_vec();
    }
    // mmap_min_addr_handler: writing needs CAP_SYS_RAWIO
    if let PseudoNode::Static(b"sys/vm/mmap_min_addr") = node {
        if !cred.capable(cap::SYS_RAWIO) {
            return Err(Errno::EPERM);
        }
        let n: u64 = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or(Errno::EINVAL)?;
        crate::mm::MMAP_MIN_ADDR.store(n, std::sync::atomic::Ordering::Relaxed);
    }
    // Dropping dentries and inodes (2, 3) forgets the FUSE nodes nothing uses; the host keeps
    // the page cache
    if let PseudoNode::Static(b"sys/vm/drop_caches") = node
        && let Some(v) = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
        && (1..=4).contains(&v)
    {
        crate::vfs::fuse::drop_caches(v);
    }
    if let PseudoNode::Static(b"sys/vm/max_map_count") = node {
        let n: usize = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&n| n <= i32::MAX as usize)
            .ok_or(Errno::EINVAL)?;
        crate::mm::MAX_MAP_COUNT.store(n, std::sync::atomic::Ordering::Relaxed);
    }
    if let PseudoNode::Static(path) = node
        && let Some(name) = path.strip_prefix(b"sys/fs/mqueue/")
        && let Some(slot) = k.ipc_mut(ns.ipc.id).mq_limits.slot(name)
    {
        *slot = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&v: &u64| v > 0)
            .ok_or(Errno::EINVAL)?;
    }
    // Between 0 and INT_MAX, as on Linux; the writer's user namespace's own limit
    if let PseudoNode::Static(path) = node
        && let Some(kind) = path
            .strip_prefix(b"sys/user/")
            .and_then(crate::nsproxy::Counted::from_sysctl)
    {
        let n: u32 = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&n| n <= i32::MAX as u32)
            .ok_or(Errno::EINVAL)?;
        k.ucounts.set_limit(cred.user_ns.id, kind, n);
    }
    if let PseudoNode::Static(b"sys/fs/aio-max-nr") = node {
        k.aio_max_nr = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or(Errno::EINVAL)?;
    }
    if let PseudoNode::Static(path) = node
        && let Some(name) = path.strip_prefix(b"sys/fs/inotify/")
    {
        // Negative numbers are not accepted, as on Linux
        let n: usize = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&n| n <= i32::MAX as usize)
            .ok_or(Errno::EINVAL)?;
        let mut r = k.inotify.borrow_mut();
        match name {
            b"max_queued_events" => r.max_queued = n,
            b"max_user_instances" => r.max_instances = n,
            _ => r.max_watches = n,
        }
    }
    // Between 0 and pid_max, as on Linux (checkpoint/restore uses it to give a new process a
    // chosen pid); it changes the writer's pid namespace
    if let PseudoNode::Static(b"sys/kernel/ns_last_pid") = node {
        let pid: i32 = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&n| (0..=crate::kernel::PID_MAX).contains(&n))
            .ok_or(Errno::EINVAL)?;
        if pid_ns.level == 0 {
            k.set_last_pid(pid);
        } else {
            pid_ns.set_last_pid(pid);
        }
    }
    // Between MIN_THREADS and FUTEX_TID_MASK, as on Linux; RLIMIT_NPROC values already set are
    // unchanged
    if let PseudoNode::Static(b"sys/kernel/threads-max") = node {
        k.threads_max = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|n| (crate::process::MIN_THREADS..=0x3fff_ffff).contains(n))
            .ok_or(Errno::EINVAL)?;
    }
    if let PseudoNode::Static(b"sys/fs/lease-break-time") = node {
        k.lease_break_time = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or(Errno::EINVAL)?;
    }
    // At least 1, at most INT_MAX (proc_dointvec_minmax with SYSCTL_ONE)
    if let PseudoNode::Static(b"sys/fs/mount-max") = node {
        k.mount_max = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|n| (1..=i32::MAX as usize).contains(n))
            .ok_or(Errno::EINVAL)?;
    }
    // The fd table's capacity is fixed: nr_open can be lowered but cannot exceed it
    if let PseudoNode::Static(b"sys/fs/nr_open") = node {
        k.nr_open = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|n| (64..=crate::fd::MAX_FDS).contains(n))
            .ok_or(Errno::EINVAL)?;
    }
    // As on Linux, rounded up to a power of two in pages; above 2^31 is EINVAL
    if let PseudoNode::Static(b"sys/fs/pipe-max-size") = node {
        let size: u64 = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&n| n <= 1 << 31)
            .ok_or(Errno::EINVAL)?;
        k.pipe_max_size = (size as usize)
            .max(crate::pipe::PIPE_BUF)
            .next_power_of_two();
    }
    if let PseudoNode::Entry(pid, _, Entry::OomScoreAdj) = node {
        let value = kstrto(proc_number(data)?, true)?;
        if !(-1000..=1000).contains(&value) {
            return Err(Errno::EINVAL);
        }
        let value = value as i32;
        let group = group_of(k, *pid).ok_or(Errno::ESRCH)?;
        let mut g = group.borrow_mut();
        // As on Linux: going below the minimum needs CAP_SYS_RESOURCE, and with it the value
        // written also becomes the new minimum
        let privileged = cred.capable(cap::SYS_RESOURCE);
        if value < g.oom_score_adj_min && !privileged {
            return Err(Errno::EACCES);
        }
        g.oom_score_adj = value;
        if privileged {
            g.oom_score_adj_min = value;
        }
    }
    if let PseudoNode::Static(b"sys/kernel/seccomp/actions_logged") = node {
        crate::seccomp::set_actions_logged(k, cred, data)?;
    }
    // As with Linux's proc_dostring, an over-long value is truncated (core_pattern is at most
    // 127 bytes)
    if let PseudoNode::Static(b"sys/kernel/core_pattern") = node {
        let pattern = data.strip_suffix(b"\n").unwrap_or(data);
        let len = pattern.len().min(crate::coredump::PATTERN_MAX - 1);
        k.core_pattern = pattern[..len].to_vec();
    }
    // proc_dointvec_minmax_sysadmin: writing needs CAP_SYS_ADMIN in the boot-time namespace, and
    // only 0 or 1
    if let PseudoNode::Static(b"sys/kernel/dmesg_restrict") = node {
        if !cred.capable(cap::SYS_ADMIN) {
            return Err(Errno::EPERM);
        }
        k.dmesg_restrict = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .filter(|v| (0..=1).contains(v))
            .ok_or(Errno::EINVAL)?
            == 1;
    }
    // proc_dointvec_minmax for one int (kernel.panic is a proc_dointvec with no range)
    let int_in = |range: std::ops::RangeInclusive<i64>| {
        crate::netconf::parse_int(data)
            .ok()
            .filter(|v| range.contains(v))
            .ok_or(Errno::EINVAL)
    };
    match node {
        PseudoNode::Static(b"sys/kernel/panic") => {
            k.panic = int_in(i32::MIN as i64..=i32::MAX as i64)? as i32;
        }
        PseudoNode::Static(b"sys/fs/protected_symlinks") => {
            k.protected_symlinks = int_in(0..=1)? == 1
        }
        // overcommit_policy_handler: 0 to 2; 1 turns off fork's commit check (crate::mm)
        PseudoNode::Static(b"sys/vm/overcommit_memory") => {
            let v = int_in(0..=2)? as u8;
            crate::mm::OVERCOMMIT_MEMORY.store(v, std::sync::atomic::Ordering::Relaxed);
        }
        // proc_taint: needs CAP_SYS_ADMIN, and the flags written (of the 19 defined) are added
        // to those already set, never cleared
        PseudoNode::Static(b"sys/kernel/tainted") => {
            if !cred.capable(cap::SYS_ADMIN) {
                return Err(Errno::EPERM);
            }
            let v: u64 = std::str::from_utf8(data)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .ok_or(Errno::EINVAL)?;
            k.tainted |= v & ((1 << 19) - 1);
        }
        // proc_dostring: up to a newline, cut to KMOD_PATH_LEN - 1 bytes
        PseudoNode::Static(b"sys/kernel/modprobe") => {
            let end = data
                .iter()
                .position(|&b| b == b'\n' || b == 0)
                .unwrap_or(data.len());
            k.modprobe = data[..end.min(255)].to_vec();
        }
        PseudoNode::Static(p) if recorded(p).is_some() => {
            let (path, _, _, lo, hi) = *recorded(p).unwrap();
            if path == b"sys/kernel/kptr_restrict" && !cred.capable(cap::SYS_ADMIN) {
                return Err(Errno::EPERM);
            }
            let v = int_in(lo..=hi)?;
            k.sysctls.insert(path, v);
        }
        PseudoNode::Static(b"sys/fs/protected_hardlinks") => {
            k.protected_hardlinks = int_in(0..=1)? == 1;
        }
        // bpf_unpriv_handler: needs CAP_SYS_ADMIN, 0 to 2, and once set to 1 it cannot be changed
        // back (EPERM)
        PseudoNode::Static(b"sys/kernel/unprivileged_bpf_disabled") => {
            if !cred.capable(cap::SYS_ADMIN) {
                return Err(Errno::EPERM);
            }
            let v = int_in(0..=2)? as u8;
            if k.unprivileged_bpf_disabled == 1 && v != 1 {
                return Err(Errno::EPERM);
            }
            k.unprivileged_bpf_disabled = v;
        }
        _ => {}
    }
    // devkmsg_sysctl_set_loglvl: proc_dostring takes up to a newline or the 9th byte; the write
    // must be exactly one of on, off, ratelimit plus one byte (echo's newline), otherwise EINVAL
    // and unchanged
    if let PseudoNode::Static(b"sys/kernel/printk_devkmsg") = node {
        let value = &data[..data.len().min(9)];
        let value = &value[..value
            .iter()
            .position(|&b| b == 0 || b == b'\n')
            .unwrap_or(value.len())];
        let (mode, len) = if value.starts_with(b"on") {
            (crate::klog::Devkmsg::On, 2)
        } else if value.starts_with(b"off") {
            (crate::klog::Devkmsg::Off, 3)
        } else if value.starts_with(b"ratelimit") {
            (crate::klog::Devkmsg::Ratelimit, 9)
        } else {
            return Err(Errno::EINVAL);
        };
        if len + 1 != data.len() {
            return Err(Errno::EINVAL);
        }
        k.klog.borrow_mut().devkmsg = mode;
    }
    // proc_dointvec: writes the first few in order, at most four; EINVAL when the first is not a
    // number
    if let PseudoNode::Static(b"sys/kernel/printk") = node {
        let text = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
        let values: Vec<i32> = text
            .split_ascii_whitespace()
            .map_while(|t| t.parse().ok())
            .take(4)
            .collect();
        if values.is_empty() {
            return Err(Errno::EINVAL);
        }
        k.printk[..values.len()].copy_from_slice(&values);
    }
    if let PseudoNode::Static(b"sys/kernel/core_uses_pid") = node {
        let v: i64 = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or(Errno::EINVAL)?;
        k.core_uses_pid = v != 0;
    }
    // Parsed the C way, as on Linux (0x prefix is hexadecimal, 0 prefix octal), keeping only 9
    // bits
    if let PseudoNode::Entry(pid, _, Entry::CoredumpFilter) = node {
        let value = kstrto(proc_number(data)?, false)?;
        let group = group_of(k, *pid).ok_or(Errno::ESRCH)?;
        group.borrow_mut().coredump_filter = value as u32 & 0x1ff;
    }
    Ok(())
}

/// Writes `/proc/<pid>/uid_map` or `gid_map` (Linux's `proc_uid_map_write` and `map_write`), with
/// the opener's credentials where Linux uses the file's: the initial namespace's maps cannot be
/// written, and the opener must be in that namespace or its parent (EPERM); a map can be written
/// only once and the opener needs `CAP_SYS_ADMIN` in that namespace (EPERM); bad contents are
/// EINVAL. Then `new_idmap_permitted`: mapping the parent's uid 0 needs `CAP_SETFCAP`, that the
/// creator had when the namespace was made if the opener is inside it, or that the opener has in
/// the parent otherwise (`verify_root_map`); the opener, owning the namespace, maps its own
/// effective ID as one extent of one (for gid_map, setgroups must first be written as deny), or
/// both writer and opener have `CAP_SETUID`/`CAP_SETGID` in the parent namespace; otherwise
/// EPERM. A second column unmapped in the parent is EPERM too. So a map file opened by an
/// unprivileged user and handed to a setuid program grants no more than the user could write.
fn write_id_map(
    k: &Kernel,
    (writer, opener): (&Cred, &Cred),
    pid: Tid,
    gid: bool,
    data: &[u8],
) -> Result<(), Errno> {
    let g = group_of(k, pid).ok_or(Errno::ESRCH)?;
    let ns = g.borrow().cred.user_ns.clone();
    let parent = ns.parent.clone().ok_or(Errno::EPERM)?;
    if opener.user_ns.id != ns.id && opener.user_ns.id != parent.id {
        return Err(Errno::EPERM);
    }
    let map = if gid { &ns.gid_map } else { &ns.uid_map };
    if !map.borrow().extents().is_empty() || !opener.ns_capable(&ns, cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    let new = crate::cred::IdMap::parse(data)?;
    let parent_map = if gid {
        parent.gid_map.borrow().clone()
    } else {
        parent.uid_map.borrow().clone()
    };
    if !gid && new.extents().iter().any(|&(_, lower, _)| lower == 0) {
        let setfcap = if opener.user_ns.id == ns.id {
            ns.parent_could_setfcap
        } else {
            opener.ns_capable(&parent, cap::SETFCAP)
        };
        if !setfcap {
            return Err(Errno::EPERM);
        }
    }
    let own = match new.extents() {
        &[(_, lower, 1)] if ns.owner == opener.euid => {
            if gid {
                !ns.setgroups.get() && parent_map.down(lower) == Some(opener.egid)
            } else {
                parent_map.down(lower) == Some(opener.euid)
            }
        }
        _ => false,
    };
    let setid = if gid { cap::SETGID } else { cap::SETUID };
    let capable = writer.ns_capable(&parent, setid) && opener.ns_capable(&parent, setid);
    if !own && !capable {
        return Err(Errno::EPERM);
    }
    let kernel = new.lower_to_kernel(&parent_map)?;
    *map.borrow_mut() = kernel;
    Ok(())
}

/// Writes `/proc/<pid>/setgroups` (Linux's `proc_setgroups_write`): "allow" or "deny", followed
/// only by whitespace (EINVAL); the opener needs `CAP_SYS_ADMIN` in that user namespace (checked
/// at open as Linux's `proc_setgroups_open` does, `EACCES`, and again here in case the process has
/// moved to another namespace since, EPERM). Once denied it cannot be allowed again, and it cannot
/// be denied after gid_map has been written (EPERM).
fn write_setgroups(k: &Kernel, opener: &Cred, pid: Tid, data: &[u8]) -> Result<(), Errno> {
    let g = group_of(k, pid).ok_or(Errno::ESRCH)?;
    let ns = g.borrow().cred.user_ns.clone();
    if !opener.ns_capable(&ns, cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    let (allow, rest) = if let Some(rest) = data.strip_prefix(b"allow") {
        (true, rest)
    } else if let Some(rest) = data.strip_prefix(b"deny") {
        (false, rest)
    } else {
        return Err(Errno::EINVAL);
    };
    if !rest.iter().all(u8::is_ascii_whitespace) {
        return Err(Errno::EINVAL);
    }
    if allow {
        if !ns.setgroups.get() {
            return Err(Errno::EPERM);
        }
    } else {
        if !ns.gid_map.borrow().extents().is_empty() {
            return Err(Errno::EPERM);
        }
        ns.setgroups.set(false);
    }
    Ok(())
}

/// Writes `/proc/<pid>/timens_offsets` (Linux 6.1's `timens_offsets_write`,
/// `proc_timens_set_offset`): one or two lines of `<clock> <seconds> <nanoseconds>`, where the
/// clock is `monotonic`, `boottime` or their numbers 1 and 7. Writes into the time namespace that
/// process uses for children: the opener needs `CAP_SYS_TIME` in its owner (EPERM; Linux's
/// `file_ns_capable`), the clocks with the offsets added must be within [0, KTIME_SEC_MAX / 2]
/// (ERANGE), and once a process has entered it is EACCES.
fn write_timens_offsets(k: &Kernel, cred: &Cred, pid: Tid, data: &[u8]) -> Result<(), Errno> {
    const KTIME_SEC_MAX: i64 = i64::MAX / 1_000_000_000;
    if data.len() >= 4096 {
        return Err(Errno::EINVAL);
    }
    let text = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
    let mut offsets = Vec::new();
    for line in text.split_terminator('\n') {
        let mut words = line.split_ascii_whitespace();
        let (Some(clock), Some(sec), Some(nsec)) = (words.next(), words.next(), words.next())
        else {
            return Err(Errno::EINVAL);
        };
        let sec: i64 = sec.parse().map_err(|_| Errno::EINVAL)?;
        let nsec: u64 = nsec.parse().map_err(|_| Errno::EINVAL)?;
        if nsec >= 1_000_000_000 {
            return Err(Errno::EINVAL);
        }
        let index = match clock {
            "monotonic" | "1" => 0,
            "boottime" | "7" => 1,
            _ => return Err(Errno::EINVAL),
        };
        offsets.push((index, (sec, nsec as i64)));
        if offsets.len() == 2 {
            break;
        }
    }
    let task = k.tasks.get(&pid).ok_or(Errno::ESRCH)?;
    let ns = task.ns.borrow().time_for_children.clone();
    if !cred.ns_capable(&ns.owner, cap::SYS_TIME) {
        return Err(Errno::EPERM);
    }
    for &(index, (sec, nsec)) in &offsets {
        let clock = [1, tuxport_hal::CLOCK_BOOTTIME][index];
        let (now, now_ns) = tuxport_hal::clock_gettime(clock).map_err(Errno)?;
        if !(-KTIME_SEC_MAX..=KTIME_SEC_MAX).contains(&sec) {
            return Err(Errno::ERANGE);
        }
        let total = now + sec + (now_ns + nsec) / 1_000_000_000;
        if !(0..=KTIME_SEC_MAX / 2).contains(&total) {
            return Err(Errno::ERANGE);
        }
    }
    if ns.frozen.get() {
        return Err(Errno::EACCES);
    }
    let mut all = ns.offsets.get();
    for (index, value) in offsets {
        all[index] = value;
    }
    ns.offsets.set(all);
    Ok(())
}

/// The buffer of the number-taking files in `/proc/<pid>` (oom_score_adj, coredump_filter): as
/// with Linux's `PROC_NUMBUF`, only the first 12 bytes up to a NUL are looked at (values runc
/// writes end in a NUL), with whitespace stripped from both ends (`strstrip`).
fn proc_number(data: &[u8]) -> Result<&str, Errno> {
    let data = &data[..data.len().min(12)];
    let data = data.split(|&b| b == 0).next().unwrap_or(data);
    Ok(std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?.trim())
}

/// Base 0 of Linux's `kstrtoint` (`signed`) and `kstrtouint`: a `0x` prefix is hexadecimal, a `0`
/// prefix octal, `+` is allowed, and signed ones also allow `-`. Non-numbers are EINVAL, values
/// beyond 32 bits ERANGE.
fn kstrto(s: &str, signed: bool) -> Result<i64, Errno> {
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) if signed => (true, rest),
        _ => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (digits, radix) = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) if h.starts_with(|c: char| c.is_ascii_hexdigit()) => (h, 16),
        _ if s.len() > 1 && s.starts_with('0') => (&s[1..], 8),
        _ => (s, 10),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(Errno::EINVAL);
    }
    let v = u64::from_str_radix(digits, radix).map_err(|_| Errno::ERANGE)?;
    match (signed, neg) {
        (true, true) if v <= 1 << 31 => Ok(-(v as i64)),
        (true, false) if v <= i32::MAX as u64 => Ok(v as i64),
        (false, _) if v <= u32::MAX as u64 => Ok(v as i64),
        _ => Err(Errno::ERANGE),
    }
}

/// The number of bytes a write to `node` consumes: `/proc/<pid>` files taking numbers accept only
/// the 12 bytes [`proc_number`] looks at (Linux truncates count and returns it), everything else
/// accepts all.
pub(crate) fn written(node: &PseudoNode, len: usize) -> usize {
    match node {
        PseudoNode::Entry(_, _, Entry::OomScoreAdj | Entry::CoredumpFilter) => len.min(12),
        _ => len,
    }
}

// ---- Shared helpers ----

/// The thread group a pid directory names. `/proc/<pid>` resolves any thread id, not only
/// thread group ids (Linux's `proc_pid_lookup` finds any task; readdir lists only the groups):
/// such a directory looks like its process's, seen through that thread (`status` and `comm` are
/// the thread's, `stat` adds up the whole group).
pub(crate) fn group_of(k: &Kernel, pid: Tid) -> Option<&Rc<RefCell<ThreadGroup>>> {
    k.groups
        .get(&pid)
        .or_else(|| k.groups.get(&k.tasks.get(&pid)?.tgid))
}

fn cpus() -> usize {
    tuxport_hal::host_cpus().len()
}

/// The entry named `name` under directory `dir` (empty for the tree root) in a fixed directory
/// tree (paths relative to the tree root like [`STATIC`], directories ending in `/`).
fn tree_lookup(tree: &[&'static [u8]], dir: &[u8], name: &[u8]) -> Option<&'static [u8]> {
    tree.iter().copied().find(|p| {
        let p = p.strip_suffix(b"/").unwrap_or(p);
        p.len() == dir.len() + name.len() && p.starts_with(dir) && p.ends_with(name)
    })
}

/// The direct entries under directory `dir` in a fixed directory tree.
fn tree_children<'t>(
    tree: &'t [&'static [u8]],
    dir: &'t [u8],
) -> impl Iterator<Item = &'static [u8]> + 't {
    tree.iter().copied().filter(move |p| {
        let p = p.strip_suffix(b"/").unwrap_or(p);
        p.len() > dir.len() && p.starts_with(dir) && !p[dir.len()..].contains(&b'/')
    })
}

/// The parent directory in a fixed directory tree; None directly under the tree root.
fn tree_parent(tree: &[&'static [u8]], path: &[u8]) -> Option<&'static [u8]> {
    let p = path.strip_suffix(b"/").unwrap_or(path);
    let i = p.iter().rposition(|&b| b == b'/')?;
    tree.iter().copied().find(|q| *q == &p[..=i])
}

/// The last component of a path.
fn tree_name(path: &[u8]) -> &[u8] {
    let p = path.strip_suffix(b"/").unwrap_or(path);
    &p[p.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1)..]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Number-taking /proc files: up to a NUL, only the first 12 bytes, base 0, beyond 32 bits is
    /// ERANGE.
    #[test]
    fn vmstat_without_the_host_file() {
        let m = tuxport_hal::MemInfo {
            free: 8192 * 4096,
            slab: 30 * 4096,
            sreclaimable: 10 * 4096,
            ..Default::default()
        };
        let text = vmstat(None, &m);
        assert!(text.starts_with("nr_free_pages 8192\n"));
        assert!(text.contains("\nnr_slab_unreclaimable 20\n"));
        for key in [
            "pgpgin",
            "pgpgout",
            "pswpin",
            "pswpout",
            "pgfault",
            "pgmajfault",
        ] {
            assert!(text.contains(&format!("\n{key} 0\n")), "{key}");
        }
        assert_eq!(vmstat(Some("pgpgin 5\n".into()), &m), "pgpgin 5\n");
    }

    #[test]
    fn proc_numbers() {
        let num = |d: &[u8], signed| kstrto(proc_number(d)?, signed);
        assert_eq!(num(b"0\0", true), Ok(0));
        assert_eq!(num(b"-500\0junk", true), Ok(-500));
        assert_eq!(num(b" 12\n", true), Ok(12));
        assert_eq!(num(b"+0x1f", false), Ok(31));
        assert_eq!(num(b"017", false), Ok(15));
        assert_eq!(num(b"0", false), Ok(0));
        assert_eq!(num(b"-1", false), Err(Errno::EINVAL));
        assert_eq!(num(b"-2147483648", true), Ok(-(1 << 31)));
        assert_eq!(num(b"2147483648", true), Err(Errno::ERANGE));
        assert_eq!(num(b"4294967296", false), Err(Errno::ERANGE));
        assert_eq!(num(b"08", false), Err(Errno::EINVAL));
        assert_eq!(num(b"0x", false), Err(Errno::EINVAL));
        assert_eq!(num(b"", true), Err(Errno::EINVAL));
        assert_eq!(num(b"1 2", true), Err(Errno::EINVAL));
        // Bytes from the 13th on are ignored
        assert_eq!(num(b"000000000001x", false), Ok(1));
    }

    #[test]
    fn cpu_lists_and_masks_round_trip() {
        assert_eq!(parse_cpu_list("0-2,5"), vec![0, 1, 2, 5]);
        assert_eq!(parse_cpu_list("7"), vec![7]);
        assert_eq!(parse_cpu_mask("00000001,0000000f"), vec![0, 1, 2, 3, 32]);
        assert_eq!(cpu_list_of(&[0, 1, 2, 5, 7, 8]), "0-2,5,7-8\n");
        assert_eq!(cpu_list_of(&[3]), "3\n");
        assert_eq!(cpu_list_of(&[]), "\n");
    }
}
