//! The Tuxport kernel layer: the Linux user-space ABI implemented in-process (design doc §3 and
//! each module's map).
//!
//! Every guest thread runs on its own host thread, and the kernel layer is protected by one big
//! kernel lock; through [`Machine`] the host starts processes and opens consoles (the Android
//! terminal tabs, `tuxport-run`). The root filesystem is the inode store.

mod bpf;
mod bug;
mod cell;
mod cgroup;
mod coredump;
mod cred;
mod debug;
pub mod errno;
mod fd;
mod forward;
mod futex;
mod inittab;
mod ipc;
mod itimer;
mod kernel;
mod klog;
mod loadavg;
mod loader;
mod locks;
mod machine;
mod mm;
mod net;
mod netconf;
mod netdev;
mod netns;
mod nft;
mod nsproxy;
mod oom;
mod origdst;
mod pidns;
mod pipe;
mod portmap;
mod process;
mod pseudo;
mod ptrace;
mod rawicmp;
mod route;
mod seccomp;
mod signal;
mod stub;
mod syscall;
mod task;
mod terminal;
mod traffic;
mod tty;
mod uaccess;
mod unix;
mod vdso;
pub mod vfs;
mod watchdog;

pub use bug::{KillReason, KillReport};
pub use inittab::Power;
pub use loader::LoadError;
pub use machine::{Machine, MachineOptions, SHUTDOWN_TIMEOUT, Stdio, run};
pub use portmap::ListenEvent;
pub use process::{ExitStatus, RunError, RunOptions, RunReport, Signal, Tier};
pub use terminal::Terminal;
pub use tuxport_engine::DbtStats;
pub use tuxport_hal::net::Route as HostRoute;
pub use watchdog::Stall;

use std::path::Path;

use errno::Errno;
use vfs::import::ImportReport;
use vfs::store::Store;

/// Creates a new inode store in host directory `store` and imports a rootfs tar.gz into it (the
/// android/setup map).
///
/// Returns `EEXIST` when the directory already holds a store. If the import fails the store stays
/// empty.
pub fn import_rootfs(store: &Path, tarball: &Path) -> Result<ImportReport, Errno> {
    bug::host_call("import_rootfs", || {
        let file = std::fs::File::open(tarball)
            .map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))?;
        let store = Store::create(store)?;
        let report = vfs::import::import_tar_gz(&store, std::io::BufReader::new(file))?;
        vfs::dev::ensure_dev_nodes(&store)?;
        Ok(report)
    })
    .unwrap_or(Err(Errno::EIO))
}

/// Writes the inode store in host directory `store` (along with the whole host filesystem it is on)
/// to disk: after importing the rootfs and writing the configuration, before renaming it into place
/// (the android/setup map). Otherwise after a power loss the store could be there while its objects
/// are still empty.
pub fn sync_store(store: &Path) -> Result<(), Errno> {
    let dir =
        std::fs::File::open(store).map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))?;
    tuxport_hal::syncfs(std::os::fd::AsRawFd::as_raw_fd(&dir)).map_err(Errno)
}

/// Writes a file in the inode store (for the host to prepare configuration files, such as
/// `/etc/profile.d/tuxport.sh` on first start); `path` is a guest path. See [`vfs::write_file`].
pub fn write_file(
    store: &Path,
    path: &str,
    data: &[u8],
    mode: u32,
    overwrite: bool,
) -> Result<(), Errno> {
    bug::host_call("write_file", || {
        let store = Store::open(store, &[])?;
        vfs::write_file(&store, path.as_bytes(), data, mode, overwrite)
    })
    .unwrap_or(Err(Errno::EIO))
}

/// Reads a regular file in the inode store (for the host to read configuration files); `path` is
/// a guest path. See [`vfs::read_file`].
pub fn read_file(store: &Path, path: &str) -> Result<Vec<u8>, Errno> {
    bug::host_call("read_file", || {
        let store = Store::open(store, &[])?;
        vfs::read_file(&store, path.as_bytes())
    })
    .unwrap_or(Err(Errno::EIO))
}

/// The host browses the store (the app's DocumentsProvider, the android/documents map): opens the
/// store a second time and calls `f` on it (the functions in [`vfs::browse`]). This works while the
/// kernel is running too: when the store is opened separately the kernel first commits its deferred
/// write transaction, so what is seen is up to date; while this connection is open the kernel no
/// longer defers commits, so it is closed after every use rather than kept open.
pub fn browse<R>(store: &Path, f: impl FnOnce(&Store) -> Result<R, Errno>) -> Result<R, Errno> {
    bug::host_call("browse", || {
        let store = Store::open(store, &[])?;
        f(&store)
    })
    .unwrap_or(Err(Errno::EIO))
}

/// The host modifies the store (the app's DocumentsProvider creating, deleting, renaming, moving,
/// writing): opens the store a second time like [`browse`], and `f` records what it changed in
/// `changes` ([`vfs::browse::Change`]). After the store is closed (committed), they are handed to
/// the running kernel that uses the same store, which reports inotify events
/// ([`machine::store_changed`]): programs watching in the guest learn that the Files app changed
/// something. On an error midway, what was already done is handed over all the same.
pub fn browse_changing<R>(
    store: &Path,
    f: impl FnOnce(&Store, &mut Vec<vfs::browse::Change>) -> Result<R, Errno>,
) -> Result<R, Errno> {
    bug::host_call("browse", || {
        let mut changes = Vec::new();
        let r = Store::open(store, &[]).and_then(|s| f(&s, &mut changes));
        machine::store_changed(store, &changes);
        r
    })
    .unwrap_or(Err(Errno::EIO))
}
