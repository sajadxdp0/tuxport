//! Imports a rootfs tar.gz into the inode store (the android/setup map).
//!
//! The whole import is done in one transaction, keeping permission bits (setuid included),
//! owners, mtimes, symlinks and hardlinks. Paths are resolved only in the store's namespace, so
//! `..`, absolute paths and symlinks pointing outside in the tar never touch the host filesystem
//! (§12 item 3).

use std::io::{self, Read};

use flate2::read::GzDecoder;

use crate::errno::Errno;
use crate::vfs::store::{Ino, ROOT, Store, mode};

/// Import statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Number of directories.
    pub dirs: u64,
    /// Number of regular files.
    pub files: u64,
    /// Number of symlinks.
    pub symlinks: u64,
    /// Number of hardlinks.
    pub hardlinks: u64,
    /// Number of device nodes and FIFOs.
    pub nodes: u64,
}

/// Imports from a gzip-compressed tar stream.
pub fn import_tar_gz(store: &Store, reader: impl Read) -> Result<ImportReport, Errno> {
    store.transaction(|store| {
        let mut archive = tar::Archive::new(GzDecoder::new(reader));
        let mut report = ImportReport::default();
        // Directory mtimes are set last: creating entries in a directory later would set its mtime
        // to now (GNU tar does the same)
        let mut dir_times = Vec::new();
        let entries = archive.entries().map_err(tar_err)?;
        for entry in entries {
            let mut entry = entry.map_err(tar_err)?;
            let path = entry.path_bytes().into_owned();
            let comps = components(&path)?;
            let header = entry.header();
            let perm = header.mode().map_err(tar_err)? & 0o7777;
            let uid = header.uid().map_err(tar_err)? as u32;
            let gid = header.gid().map_err(tar_err)? as u32;
            let mtime_ns = header.mtime().map_err(tar_err)? as i64 * 1_000_000_000;
            let kind = header.entry_type();
            let Some((name, parents)) = comps.split_last() else {
                // The root directory in the tar: only update its attributes
                store.set_owner_mode(ROOT, perm, uid, gid)?;
                dir_times.push((ROOT, mtime_ns));
                continue;
            };
            let parent = ensure_dir(store, parents, &mut report)?;
            let existing = store.lookup(parent, name)?;

            let ino = if kind.is_dir() {
                let ino = match existing {
                    Some(ino) => ino,
                    None => {
                        report.dirs += 1;
                        store.mkdir(parent, name, perm, uid, gid)?
                    }
                };
                store.set_owner_mode(ino, perm, uid, gid)?;
                dir_times.push((ino, mtime_ns));
                continue;
            } else if existing.is_some() {
                return Err(Errno::EEXIST);
            } else if kind.is_file() {
                report.files += 1;
                let (ino, mut file) = store.create_file(parent, name, perm, uid, gid)?;
                io::copy(&mut entry, &mut file).map_err(tar_err)?;
                ino
            } else if kind.is_symlink() {
                report.symlinks += 1;
                let target = entry.link_name_bytes().ok_or(Errno::EINVAL)?.into_owned();
                store.symlink(parent, name, &target, uid, gid)?
            } else if kind.is_hard_link() {
                report.hardlinks += 1;
                let target = entry.link_name_bytes().ok_or(Errno::EINVAL)?.into_owned();
                let target_comps = components(&target)?;
                let mut ino = ROOT;
                for c in &target_comps {
                    ino = store.lookup(ino, c)?.ok_or(Errno::ENOENT)?;
                }
                store.link(parent, name, ino)?;
                continue;
            } else if kind.is_character_special() || kind.is_block_special() || kind.is_fifo() {
                report.nodes += 1;
                let ty = if kind.is_character_special() {
                    mode::S_IFCHR
                } else if kind.is_block_special() {
                    mode::S_IFBLK
                } else {
                    mode::S_IFIFO
                };
                let major = header.device_major().ok().flatten().unwrap_or(0) as u64;
                let minor = header.device_minor().ok().flatten().unwrap_or(0) as u64;
                let rdev = (major << 8 & 0xfff00) | (minor & 0xff) | (minor << 12 & !0xfffff);
                store.mknod(parent, name, ty | perm, rdev, uid, gid)?
            } else {
                continue; // metadata entries such as pax headers
            };
            store.set_mtime(ino, mtime_ns)?;
        }
        for (ino, mtime_ns) in dir_times {
            store.set_mtime(ino, mtime_ns)?;
        }
        Ok(report)
    })
}

fn tar_err(e: io::Error) -> Errno {
    crate::debug::log(format_args!("rootfs import: {e}"));
    Errno::EIO
}

/// Splits a path into components, dropping empty components, `.` and the leading `/`; EINVAL on
/// `..`.
fn components(path: &[u8]) -> Result<Vec<&[u8]>, Errno> {
    let mut out = Vec::new();
    for c in path.split(|&b| b == b'/') {
        match c {
            b"" | b"." => {}
            b".." => return Err(Errno::EINVAL),
            c => out.push(c),
        }
    }
    Ok(out)
}

/// Finds (creating it if needed) the directory for a path, without following symlinks.
fn ensure_dir(store: &Store, comps: &[&[u8]], report: &mut ImportReport) -> Result<Ino, Errno> {
    let mut dir = ROOT;
    for c in comps {
        dir = match store.lookup(dir, c)? {
            Some(ino) => ino,
            None => {
                report.dirs += 1;
                store.mkdir(dir, c, 0o755, 0, 0)?
            }
        };
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_gz(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        build(&mut builder);
        let raw = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &raw).unwrap();
        gz.finish().unwrap()
    }

    fn header(kind: tar::EntryType, mode: u32, size: u64) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_mode(mode);
        h.set_size(size);
        h.set_mtime(1_700_000_000);
        h.set_uid(0);
        h.set_gid(0);
        h
    }

    #[test]
    fn imports_files_links_and_modes() {
        let data = tar_gz(|b| {
            b.append_data(
                &mut header(tar::EntryType::Directory, 0o755, 0),
                "./bin/",
                &[][..],
            )
            .unwrap();
            b.append_data(
                &mut header(tar::EntryType::Regular, 0o4755, 5),
                "./bin/busybox",
                &b"hello"[..],
            )
            .unwrap();
            let mut l = header(tar::EntryType::Symlink, 0o777, 0);
            b.append_link(&mut l, "./bin/ls", "/bin/busybox").unwrap();
            let mut h = header(tar::EntryType::Link, 0o755, 0);
            b.append_link(&mut h, "./sbin/bb", "./bin/busybox").unwrap();
        });
        let dir = std::env::temp_dir().join(format!("tuxport-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::create(&dir).unwrap();
        let report = import_tar_gz(&store, &data[..]).unwrap();
        assert_eq!((report.files, report.symlinks, report.hardlinks), (1, 1, 1));

        let bin = store.lookup(ROOT, b"bin").unwrap().unwrap();
        let busybox = store.lookup(bin, b"busybox").unwrap().unwrap();
        let st = store.stat(busybox).unwrap();
        assert_eq!((st.mode, st.size, st.nlink), (mode::S_IFREG | 0o4755, 5, 2));
        assert_eq!(st.mtime_ns, 1_700_000_000 * 1_000_000_000);
        // The directory's mtime is the one from the tar, not the time entries were created in it
        assert_eq!(
            store.stat(bin).unwrap().mtime_ns,
            1_700_000_000 * 1_000_000_000
        );
        let ls = store.lookup(bin, b"ls").unwrap().unwrap();
        assert_eq!(store.readlink(ls).unwrap(), b"/bin/busybox");
        let sbin = store.lookup(ROOT, b"sbin").unwrap().unwrap();
        assert_eq!(store.lookup(sbin, b"bb").unwrap(), Some(busybox));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_parent_components() {
        assert_eq!(components(b"./a/../b"), Err(Errno::EINVAL));
        assert_eq!(components(b"/a//b/.").unwrap(), vec![&b"a"[..], &b"b"[..]]);
    }
}
