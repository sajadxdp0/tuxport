//! Device nodes (the kernel/vfs map).
//!
//! At boot a tmpfs is mounted on `/dev` and the standard device nodes are created in it with
//! Linux's device numbers ([`populate`], the equivalent of Linux's devtmpfs); opening a device
//! node finds the implementation here by device number, and nodes with the same numbers that the
//! guest creates with mknod behave the same. The only block devices are the loop devices
//! (`syscall::loopdev`). The store's `/dev` directory is its mount point and
//! gets the same nodes too ([`ensure_dev_nodes`]; they are what is seen after `/dev` is
//! unmounted).

use crate::errno::Errno;
use crate::vfs::store::{Ino, ROOT, Store, mode};

/// Devices implemented in-process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    /// `/dev/null`: reads return EOF, writes are all discarded.
    Null,
    /// `/dev/zero`: reads return zeros, writes are all discarded.
    Zero,
    /// `/dev/full`: reads return zeros, writes fail (ENOSPC).
    Full,
    /// `/dev/random`, `/dev/urandom`: the host's random numbers.
    Random,
    /// `/dev/kmsg`: the kernel log ([`crate::klog`]); each open file has its own read position.
    Kmsg,
    /// `/dev/loop-control`: adds, removes and finds loop devices (`syscall::loopdev`).
    LoopControl,
    /// A loop device, a block device (`syscall::loopdev`; the device itself is in the open
    /// file's `block`).
    Loop,
}

impl Device {
    /// Looks up a character device by device number; ENXIO for devices that are not
    /// implemented.
    pub fn from_rdev(rdev_value: u64) -> Result<Device, Errno> {
        match rdev_value {
            v if v == rdev(1, 3) => Ok(Device::Null),
            v if v == rdev(1, 5) => Ok(Device::Zero),
            v if v == rdev(1, 7) => Ok(Device::Full),
            v if v == rdev(1, 8) || v == rdev(1, 9) => Ok(Device::Random),
            v if v == rdev(1, 11) => Ok(Device::Kmsg),
            LOOP_CONTROL => Ok(Device::LoopControl),
            _ => Err(Errno::ENXIO),
        }
    }
}

/// Linux's device number encoding (`new_encode_dev`).
pub const fn rdev(major: u32, minor: u32) -> u64 {
    ((minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)) as u64
}

/// Splits a device number in Linux's encoding into major and minor (`new_decode_dev`).
pub const fn split_rdev(rdev_value: u64) -> (u32, u32) {
    let r = rdev_value as u32;
    ((r & 0xfff00) >> 8, (r & 0xff) | ((r >> 12) & 0xfff00))
}

/// The block major of loop devices.
pub const LOOP_MAJOR: u32 = 7;
/// `/dev/loop-control`'s device number (misc 10:237).
pub const LOOP_CONTROL: u64 = rdev(10, 237);
/// The loop devices that exist from boot (`CONFIG_BLK_DEV_LOOP_MIN_COUNT`), `loop0` to `loop7`.
pub const LOOP_MIN_COUNT: u32 = 8;
/// The `disk` group (6 in Alpine and Debian), which owns the loop nodes as udev's and mdev's
/// rules set them.
pub const DISK_GID: u32 = 6;

/// The standard character nodes in `/dev`: name, device number, permissions, group. Terminal
/// devices (tty, console, ptmx) are connected to PTYs by the syscall layer by device number.
const NODES: [(&[u8], u64, u32, u32); 11] = [
    (b"null", rdev(1, 3), 0o666, 0),
    (b"zero", rdev(1, 5), 0o666, 0),
    (b"full", rdev(1, 7), 0o666, 0),
    (b"random", rdev(1, 8), 0o666, 0),
    (b"urandom", rdev(1, 9), 0o666, 0),
    (b"kmsg", rdev(1, 11), 0o644, 0),
    (b"tty", rdev(5, 0), 0o666, 0),
    (b"console", rdev(5, 1), 0o600, 0),
    (b"ptmx", rdev(5, 2), 0o666, 0),
    (b"loop-control", LOOP_CONTROL, 0o660, DISK_GID),
    (b"fuse", crate::vfs::fuse::DEV_RDEV, 0o666, 0),
];

/// The standard symlinks in `/dev`, all pointing into procfs.
const LINKS: [(&[u8], &[u8]); 4] = [
    (b"fd", b"/proc/self/fd"),
    (b"stdin", b"/proc/self/fd/0"),
    (b"stdout", b"/proc/self/fd/1"),
    (b"stderr", b"/proc/self/fd/2"),
];

/// The standard directories in `/dev`: the mount points of devpts, the `/dev/shm` tmpfs and
/// mqueue.
const DIRS: [(&[u8], u32); 3] = [(b"pts", 0o755), (b"shm", 0o1777), (b"mqueue", 0o755)];

/// Ensures that `/dev` and the standard device nodes in it exist, along with the mount points
/// `/proc`, `/sys` and `/run` (called after importing a rootfs and at every boot). Existing
/// entries with the same names are left alone.
pub fn ensure_dev_nodes(store: &Store) -> Result<(), Errno> {
    store.transaction(|s| {
        for (name, perm) in [(&b"proc"[..], 0o555), (b"sys", 0o555), (b"run", 0o755)] {
            if s.lookup(ROOT, name)?.is_none() {
                s.mkdir(ROOT, name, perm, 0, 0)?;
            }
        }
        let dev = match s.lookup(ROOT, b"dev")? {
            Some(ino) => ino,
            None => s.mkdir(ROOT, b"dev", 0o755, 0, 0)?,
        };
        if !s.stat(dev)?.is_dir() {
            return Ok(());
        }
        populate(s, dev)
    })
}

/// Creates the standard device nodes, symlinks and directories in directory `dev` (the tmpfs
/// mounted on `/dev` at boot, and the store's `/dev`), with the boot loop devices' nodes.
/// Existing entries with the same names are left alone.
pub fn populate(store: &Store, dev: Ino) -> Result<(), Errno> {
    store.transaction(|s| {
        let missing = |name: &[u8]| -> Result<bool, Errno> { Ok(s.lookup(dev, name)?.is_none()) };
        for (name, rdev_value, perm, gid) in NODES {
            if missing(name)? {
                s.mknod(dev, name, mode::S_IFCHR | perm, rdev_value, 0, gid)?;
            }
        }
        for n in 0..LOOP_MIN_COUNT {
            let name = format!("loop{n}");
            if missing(name.as_bytes())? {
                let node = rdev(LOOP_MAJOR, n);
                s.mknod(
                    dev,
                    name.as_bytes(),
                    mode::S_IFBLK | 0o660,
                    node,
                    0,
                    DISK_GID,
                )?;
            }
        }
        for (name, target) in LINKS {
            if missing(name)? {
                s.symlink(dev, name, target, 0, 0)?;
            }
        }
        for (name, perm) in DIRS {
            if missing(name)? {
                s.mkdir(dev, name, perm, 0, 0)?;
            }
        }
        Ok(())
    })
}

/// The character device for a device node's inode.
pub fn open(store: &Store, ino: Ino) -> Result<Device, Errno> {
    Device::from_rdev(store.stat(ino)?.rdev)
}
