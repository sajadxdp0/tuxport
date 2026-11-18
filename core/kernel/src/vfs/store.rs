//! The inode store (the kernel/store map).
//!
//! SQLite (WAL) holds the whole namespace: dentries, inodes, symlink targets; regular files' data
//! is stored as host files `objects/<ino>`. A regular file's size and mtime are taken from the
//! host object file, and the other metadata (mode, uid/gid, nlink, rdev, ctime) is stored in
//! SQLite. A regular file's ctime is also taken from the object: write, truncate and utimensat
//! change the object, and when the database changes its metadata (permissions, owner, link count,
//! rename, xattrs) the object is chmodded as well, so that the host updates its ctime. The clock
//! the database column uses does not necessarily agree with the one the host filesystem stamps
//! times with (tens of milliseconds apart on virtiofs), and taking the later of the two would
//! miss changes.
//! A hard link is simply several dentries pointing at the same ino. Only paths of the form
//! `objects/<number>` ever appear on the host side, never built from guest input (§12 item 3).
//!
//! When the last link is removed, a regular file's object is deleted after this change is
//! committed (host files already open remain usable), and the inode row stays with nlink = 0 so
//! that fds that have it open can still fstat it; it is cleaned up the next time the store is
//! opened.
//!
//! Several regular files can share one object ([`Store::share`]: FICLONE, a whole-file
//! copy_file_range, an overlay copy-up), the reflink of filesystems such as btrfs and XFS at the
//! granularity of whole files. The `shared` table records each file whose data is not simply its
//! own `objects/<ino>`: the object's number, and while the object is or was shared since the file
//! was last written, the file's access time (its mtime and ctime are then the row's), since the
//! object's host times belong to no one file. Such a file is held: writing, truncating or
//! mapping it shared for write first gives it an object of its own ([`Store::unshare`]: a copy
//! when others still share the object), and only then do its times come from the object again.
//! A number some row uses as its object keeps its inode row at boot, so it is never given to a new
//! file while the object lives.
//!
//! Write transactions are committed late ([`COMMIT_DELAY`]): every commit writes the changed
//! pages into the WAL, ten times as expensive as the change itself, so metadata operations within
//! a few milliseconds are committed together; each operation is a savepoint in the transaction
//! and on error rolls back only itself. When the app is killed, at most the last few milliseconds
//! of changes are lost, and the store is still the complete state of some moment (Linux loses
//! more on power loss). While other Tuxport processes are also using the store, every operation
//! commits immediately. Checkpoints (writing the WAL back into the database file, which needs
//! fsync) are done by a background thread ([`wal_hook`]), not in commits, which mostly hold the
//! BKL.
//!
//! When the host loses power (or crashes), everything in the page cache is lost, and the database
//! and the objects on disk each keep only the parts that were written back. Commits write without
//! flushing (`synchronous = NORMAL`), and the host writes the WAL and the objects back
//! independently, with no guarantee of order. Three things are guaranteed
//! ([`Meta::sync_metadata`]): data and metadata the guest asked to be made durable (fsync, sync,
//! ...) really are on disk; when a rename replaces another file, the new file's data reaches the
//! disk before the rename does ([`Store::flush_replaced`], native ext4's auto_da_alloc); an object
//! is deleted only after the commit that removed its last link is on disk ([`reap`]), so when the
//! database returns to an earlier state after power loss, the object is still there.
//!
//! Path resolution and stat go through an in-memory metadata cache (dentries, including names that
//! do not exist; inode rows; symlink targets) instead of querying the database level by level.
//! Our own writes only remove the affected entries. Several Tuxport processes can share one store,
//! so the first 8 bytes of the `lock` file are a counter that all processes map shared: it is
//! bumped after every commit, and when the cache finds that it changed other than by its own
//! commit, the whole cache is invalidated.
//!
//! The cache can also be read without the BKL ([`Meta`]): parallel lstats (`git status`, build
//! tools) need not queue up.

use std::cell::{Cell, UnsafeCell};
use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_int, c_void};
use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock, RwLockReadGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, ffi, params};

use crate::cell::RefCell;
use crate::cred::XATTR_NAME_CAPS;
use crate::errno::Errno;

/// Inode number.
pub type Ino = u64;

/// The root directory's inode.
pub const ROOT: Ino = 1;

/// The longest a write transaction's commit is deferred (module docs).
pub const COMMIT_DELAY: Duration = Duration::from_millis(5);

/// When this many pages not yet written back have accumulated in the WAL, the background thread
/// is asked for a checkpoint (the same as SQLite's default auto-checkpoint).
const CHECKPOINT_PAGES: c_int = 1000;

/// When the WAL grows to this many pages, the committing thread checkpoints in place so the WAL
/// is reused from the start ([`wal_hook`]).
const INLINE_CHECKPOINT_PAGES: c_int = 10 * CHECKPOINT_PAGES;

/// After writes have stopped for this long, the background thread writes back the frames left in
/// the WAL ([`reap`]).
const CHECKPOINT_QUIET: Duration = Duration::from_millis(100);

/// The length the WAL is truncated to when reused from the start (`journal_size_limit`), so it
/// does not keep for long the space it grew to during sustained writes.
const WAL_SIZE_LIMIT: i64 = 8 << 20;

/// Permission bits of object files on the host: only the app itself reads and writes them; the
/// permissions the guest sees are recorded in the database.
const OBJECT_MODE: u32 = 0o600;

/// The most directories recorded between two [`Store::take_changed_dirs`] calls; beyond that no
/// more are recorded (the Files app refreshes a few fewer).
const MAX_CHANGED_DIRS: usize = 256;

/// When at most this many regular files in one commit were renamed over other files, each is
/// fdatasynced; beyond that the whole filesystem is syncfsed once ([`Store::flush_replaced`]).
const FLUSH_EACH: usize = 16;

/// Objects the host side (the Files app) has handed out for writing ([`Store::host_opened`]): the
/// store they are in (its `objects` directory's device and inode), the object, and the path the
/// file was opened by. The other side writes such an object directly, so no other file may come
/// to share it ([`Store::host_writing`]). An open goes when the close is reported by the same path
/// ([`Store::host_closed`]), whatever happened to the file meanwhile; one whose close is never
/// reported (the close could not be watched) keeps its object out of sharing, which costs only
/// copies.
static HOST_WRITERS: Mutex<Vec<HostWriter>> = Mutex::new(Vec::new());

/// One entry of [`HOST_WRITERS`]: the store's identity, the object, the path.
type HostWriter = ((u64, u64), Ino, Vec<u8>);

/// The times [`Store::set_times`] sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTime {
    /// Now. For regular files the host takes it, with the same clock as the host filesystem's own
    /// timestamps (see the module docs).
    Now,
    /// A given time (nanoseconds).
    At(i64),
}

impl FileTime {
    /// (seconds, nanoseconds) to hand to the host.
    fn host(self) -> (i64, i64) {
        match self {
            FileTime::Now => tuxport_hal::TIME_NOW,
            FileTime::At(ns) => split_ns(ns),
        }
    }
}

/// File type bits (the high bits of `st_mode`).
pub mod mode {
    /// Type bit mask.
    pub const S_IFMT: u32 = 0o170000;
    /// Directory.
    pub const S_IFDIR: u32 = 0o040000;
    /// Regular file.
    pub const S_IFREG: u32 = 0o100000;
    /// Symbolic link.
    pub const S_IFLNK: u32 = 0o120000;
    /// Character device.
    pub const S_IFCHR: u32 = 0o020000;
    /// Block device.
    pub const S_IFBLK: u32 = 0o060000;
    /// FIFO.
    pub const S_IFIFO: u32 = 0o010000;
    /// Socket.
    pub const S_IFSOCK: u32 = 0o140000;
    /// The setuid bit.
    pub const S_ISUID: u32 = 0o4000;
    /// The setgid bit.
    pub const S_ISGID: u32 = 0o2000;
    /// The sticky bit.
    pub const S_ISVTX: u32 = 0o1000;
}

/// The metadata of an inode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stat {
    /// Inode number.
    pub ino: Ino,
    /// Type and permission bits.
    pub mode: u32,
    /// Number of hard links.
    pub nlink: u32,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Device number (device nodes).
    pub rdev: u64,
    /// Size in bytes.
    pub size: u64,
    /// Access time (nanoseconds). Recorded separately only for regular files (the host object's
    /// atime); for the others it equals mtime.
    pub atime_ns: i64,
    /// Modification time (nanoseconds).
    pub mtime_ns: i64,
    /// Status change time (nanoseconds).
    pub ctime_ns: i64,
    /// Storage a regular file takes in 512-byte blocks, as the host reports it for the object
    /// (holes take none); `None` where there is no object, and the size stands in for it.
    pub blocks: Option<u64>,
}

impl Stat {
    /// File type bits.
    pub fn kind(&self) -> u32 {
        self.mode & mode::S_IFMT
    }

    /// Whether it is a directory.
    pub fn is_dir(&self) -> bool {
        self.kind() == mode::S_IFDIR
    }

    /// Whether it is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.kind() == mode::S_IFLNK
    }
}

/// An entry in a directory (not including `.` and `..`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Name.
    pub name: Vec<u8>,
    /// Inode number.
    pub ino: Ino,
    /// Type and permission bits.
    pub mode: u32,
}

/// A held file getting an object of its own while others still share its old one
/// ([`Store::unshare`]): the caller copies `len` bytes of `from` into `to` (holes kept,
/// [`copy_object`]), with the BKL released if it likes, and then calls [`Store::finish_unshare`],
/// or [`Store::abandon_unshare`] when the copy fails.
pub struct Unshare {
    /// The file.
    pub ino: Ino,
    /// The shared object, open for reading.
    pub from: File,
    /// Its new object, empty, open for writing.
    pub to: File,
    /// How much of the data to copy (all of it, or what a truncation keeps).
    pub len: u64,
    /// The numbers of the two objects.
    objects: (Ino, Ino),
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS inodes (
        ino      INTEGER PRIMARY KEY,
        mode     INTEGER NOT NULL,
        uid      INTEGER NOT NULL,
        gid      INTEGER NOT NULL,
        nlink    INTEGER NOT NULL,
        rdev     INTEGER NOT NULL DEFAULT 0,
        parent   INTEGER NOT NULL DEFAULT 0, -- a directory's parent directory (..)
        mtime_ns INTEGER NOT NULL DEFAULT 0, -- used only by inodes without an object
        ctime_ns INTEGER NOT NULL DEFAULT 0,
        target   BLOB                        -- the symlink's target
    );
    CREATE TABLE IF NOT EXISTS dentries (
        parent INTEGER NOT NULL,
        name   BLOB NOT NULL,
        ino    INTEGER NOT NULL,
        PRIMARY KEY (parent, name)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS xattrs (
        ino   INTEGER NOT NULL,
        name  BLOB NOT NULL,
        value BLOB NOT NULL,
        PRIMARY KEY (ino, name)
    ) WITHOUT ROWID;
    -- Directories not under the root directory (tmpfs roots): deleted at boot along with
    -- everything in them
    CREATE TABLE IF NOT EXISTS detached_roots (
        ino INTEGER PRIMARY KEY
    );
    -- Regular files whose data is in objects/<object> rather than their own objects/<ino>, or
    -- whose object other files share (module docs); atime_ns is set while the file is held: its
    -- times are then the database's (mtime and ctime in inodes)
    CREATE TABLE IF NOT EXISTS shared (
        ino      INTEGER PRIMARY KEY,
        object   INTEGER NOT NULL,
        atime_ns INTEGER
    );
    CREATE INDEX IF NOT EXISTS shared_object ON shared (object);
";

/// An inode store.
///
/// Not Sync: only one thread uses it at a time. The kernel's store relies on the BKL for this
/// (`Kernel` implements Send by hand), and checked builds check it with
/// [`Store::guard_with_bkl`].
pub struct Store {
    /// The database connection, used through [`Store::db`]. Must be dropped first (the WAL hook of
    /// [`Store::connect`]).
    conn: Connection,
    /// The running kernel it belongs to ([`crate::kernel::Shared::id`]); it may be used only while
    /// holding that kernel's BKL ([`Store::guard_with_bkl`]); 0 means it belongs to none.
    #[cfg(any(debug_assertions, feature = "borrow-check"))]
    owner: std::sync::atomic::AtomicUsize,
    objects: PathBuf,
    /// The `lock` file in the store directory. Every process using the store holds a shared lock
    /// on it, and deleted inodes are cleaned up only under exclusive use: other processes may still
    /// have them open.
    lock: File,
    /// The metadata cache, the commit counter and the `objects` directory.
    meta: Arc<Meta>,
    /// Inside an operation ([`Store::transaction`]): what is queried may be content its rollback
    /// would undo, so it is not filled into the cache. The cache is still read: entries this
    /// operation changed were removed when they were changed ([`Store::touch_name`]), and what
    /// remains agrees with the database.
    writing: Cell<bool>,
    /// Inside [`Store::snapshot`]: Some(None) before the read transaction has started, and
    /// Some(Some(n)) with the commit count at its start.
    snapshot_at: Cell<Option<Option<u64>>>,
    /// When the deferred write transaction started; None when no write transaction is open.
    pending: Cell<Option<Instant>>,
    /// The longest a write transaction is deferred ([`COMMIT_DELAY`]). Longer in tests: on a slow
    /// machine one operation of a debug build can take over 5 ms and be committed on the spot,
    /// hiding the deferred state.
    delay: Duration,
    /// Objects whose last link has been removed, to be deleted after the write transaction
    /// commits.
    doomed: RefCell<Vec<Ino>>,
    /// Regular files in the write transaction that were renamed over other files: their data is
    /// made durable before the commit ([`Store::flush_replaced`]).
    replaced: RefCell<Vec<Ino>>,
    /// Directories whose dentries changed ([`Store::take_changed_dirs`]); not recorded when None.
    changed_dirs: RefCell<Option<std::collections::BTreeSet<Ino>>>,
    /// Called when a commit starts being deferred: wakes the thread that commits when it is due
    /// ([`Store::commit_due`]).
    waker: RefCell<Option<Box<dyn Fn()>>>,
    /// The background thread that deletes objects ([`reap`]).
    reaper: Option<JoinHandle<()>>,
    /// What goes into the cache after this operation succeeds ([`Fill`]).
    fills: RefCell<Vec<Fill>>,
}

impl Store {
    // ---- Opening and boot cleanup ----

    /// Creates a new store, containing only the root directory, in empty (or nonexistent)
    /// directory `dir`.
    pub fn create(dir: &Path) -> Result<Store, Errno> {
        fs::create_dir_all(dir.join("objects")).map_err(io_err)?;
        if dir.join("meta.db").exists() {
            return Err(Errno::EEXIST);
        }
        let store = Self::connect(dir)?;
        let now = now_ns();
        store
            .db()
            .execute(
                "INSERT INTO inodes (ino, mode, uid, gid, nlink, parent, mtime_ns, ctime_ns)
                 VALUES (?1, ?2, 0, 0, 2, ?1, ?3, ?3)",
                params![ROOT, mode::S_IFDIR | 0o755, now],
            )
            .map_err(db_err)?;
        store.meta.commits.users().fetch_add(1, Ordering::SeqCst);
        Ok(store)
    }

    /// Opens an existing store. When no other process is using it (the equivalent of boot),
    /// empties the directories in `volatile` (paths relative to the root directory, such as
    /// `dev/shm`: on Linux they are tmpfs) and cleans up deleted inodes left by the last run.
    pub fn open(dir: &Path, volatile: &[&[u8]]) -> Result<Store, Errno> {
        if !dir.join("meta.db").exists() {
            return Err(Errno::ENOENT);
        }
        let store = Self::connect(dir)?;
        let fd = store.lock.as_raw_fd();
        if tuxport_hal::flock(fd, true, false).is_ok() {
            store.meta.commits.users().store(0, Ordering::SeqCst);
            store.remove_lost_objects()?;
            for path in volatile {
                store.empty_dir(path)?;
            }
            store.remove_detached_dirs()?;
            // The last run may have been killed, and its last commit may still be only in the
            // host's page cache: make it durable before deleting objects according to the
            // database, or with power lost right afterwards the database returns to its earlier
            // state but the objects are gone ([`reap`])
            store.meta.sync_metadata()?;
            store.transaction(|s| s.remove_orphans())?;
            store.commit()?;
        }
        tuxport_hal::flock(fd, false, true).map_err(Errno)?;
        // Another process is also using the store: wait for it to commit its deferred write
        // transaction (after seeing this count it no longer defers); only then are its changes
        // visible
        if store.meta.commits.users().fetch_add(1, Ordering::SeqCst) > 0 {
            store.exec("BEGIN IMMEDIATE")?;
            store.exec("COMMIT")?;
        }
        Ok(store)
    }

    fn connect(dir: &Path) -> Result<Store, Errno> {
        let db = Connection::open(dir.join("meta.db")).map_err(db_err)?;
        db.pragma_update(None, "journal_mode", "WAL")
            .map_err(db_err)?;
        db.pragma_update(None, "synchronous", "NORMAL")
            .map_err(db_err)?;
        db.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT)
            .map_err(db_err)?;
        // Several Tuxport processes can use the same store at once: wait for others' write
        // transactions to end instead of failing at once
        db.busy_timeout(Duration::from_secs(10)).map_err(db_err)?;
        // Metadata operations are very frequent: every statement is compiled only once
        db.set_prepared_statement_cache_capacity(64);
        db.execute_batch(SCHEMA).map_err(db_err)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("lock"))
            .map_err(io_err)?;
        tuxport_hal::flock(lock.as_raw_fd(), false, true).map_err(Errno)?;
        let commits = Commits::open(&lock)?;
        let objects_dir = tuxport_hal::open_dir(&dir.join("objects")).map_err(Errno)?;
        let meta = Arc::new(Meta {
            lock: RwLock::new(()),
            cache: UnsafeCell::new(Cache {
                generation: commits.get(),
                ..Cache::default()
            }),
            commits,
            objects_dir,
            reaper: Reaper::default(),
            fs: lock.try_clone().map_err(io_err)?,
            wal: dir.join("meta.db-wal"),
            durable: AtomicU64::new(0),
            #[cfg(test)]
            syncs: AtomicU64::new(0),
            #[cfg(test)]
            flushes: AtomicU64::new(0),
        });
        let reaper = {
            let (meta, db) = (meta.clone(), dir.join("meta.db"));
            std::thread::Builder::new()
                .name("objects-reaper".into())
                .spawn(move || reap(meta, db))
                .ok()
        };
        if reaper.is_some() {
            // SAFETY: the Meta the hook gets is held by both the Store and the background thread;
            // conn is the first of the Store's fields to be dropped, and the hook is never called
            // after the connection closes. Without a background thread, SQLite's own
            // auto-checkpoint is kept
            unsafe {
                ffi::sqlite3_wal_hook(
                    db.handle(),
                    Some(wal_hook),
                    Arc::as_ptr(&meta).cast_mut().cast(),
                );
            }
        }
        Ok(Store {
            conn: db,
            #[cfg(any(debug_assertions, feature = "borrow-check"))]
            owner: std::sync::atomic::AtomicUsize::new(0),
            objects: dir.join("objects"),
            lock,
            meta,
            reaper,
            writing: Cell::new(false),
            snapshot_at: Cell::new(None),
            pending: Cell::new(None),
            delay: COMMIT_DELAY,
            doomed: RefCell::default(),
            replaced: RefCell::default(),
            changed_dirs: RefCell::new(None),
            waker: RefCell::new(None),
            fills: RefCell::default(),
        })
    }

    /// Deletes the objects left by inodes lost last time before they could be committed
    /// ([`Commits::fresh`]).
    fn remove_lost_objects(&self) -> Result<(), Errno> {
        let (lo, hi) = self.meta.commits.fresh();
        let range = lo.load(Ordering::SeqCst)..=hi.load(Ordering::SeqCst);
        if *range.start() != 0 {
            for ino in range {
                if self.query_row(ino) == Err(Errno::ENOENT) {
                    self.remove_object(ino)?;
                }
            }
            lo.store(0, Ordering::SeqCst);
            hi.store(0, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Empties the contents of directory `path`, keeping the directory itself. Does nothing if it
    /// does not exist or is not a directory (a symlink, for example).
    fn empty_dir(&self, path: &[u8]) -> Result<(), Errno> {
        let mut dir = ROOT;
        for name in path.split(|&b| b == b'/') {
            match self.lookup(dir, name)? {
                Some(ino) => dir = ino,
                None => return Ok(()),
            }
        }
        if !self.stat_row(dir)?.is_dir() {
            return Ok(());
        }
        self.remove_contents(dir)
    }

    fn remove_contents(&self, dir: Ino) -> Result<(), Errno> {
        for e in self.readdir(dir)? {
            if e.mode & mode::S_IFMT == mode::S_IFDIR {
                self.remove_contents(e.ino)?;
                self.rmdir(dir, &e.name)?;
            } else {
                self.unlink(dir, &e.name)?;
            }
        }
        Ok(())
    }

    /// Deletes the directories left by the last run that do not hang under the root directory
    /// ([`Store::create_detached_dir`]).
    fn remove_detached_dirs(&self) -> Result<(), Errno> {
        let roots: Vec<Ino> = {
            let mut stmt = self
                .db()
                .prepare("SELECT ino FROM detached_roots")
                .map_err(db_err)?;
            let rows = stmt.query_map([], |r| r.get(0)).map_err(db_err)?;
            rows.collect::<Result<_, _>>().map_err(db_err)?
        };
        for ino in roots {
            self.remove_detached_dir(ino)?;
        }
        Ok(())
    }

    /// Deletes the inodes left with no links by the last run, and their objects. Deleted files
    /// first stop sharing objects (none is open now); the rows whose numbers other files still use
    /// as their objects stay, so the numbers are not given to new files, and the objects nothing
    /// uses any more go.
    fn remove_orphans(&self) -> Result<(), Errno> {
        let query = |sql: &str| -> Result<Vec<(Ino, u32)>, Errno> {
            let mut stmt = self.db().prepare(sql).map_err(db_err)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(db_err)?;
            rows.collect::<Result<_, _>>().map_err(db_err)
        };
        let released = query(
            "SELECT s.object, 0 FROM shared s LEFT JOIN inodes i ON i.ino = s.ino
             WHERE i.nlink IS NULL OR i.nlink = 0",
        )?;
        self.exec("DELETE FROM shared WHERE ino NOT IN (SELECT ino FROM inodes WHERE nlink > 0)")?;
        if !released.is_empty() {
            self.update(|c| c.clear(c.generation));
        }
        let orphans = query(
            "SELECT ino, mode FROM inodes
             WHERE nlink = 0 AND ino NOT IN (SELECT object FROM shared)",
        )?;
        // Emptying the volatile directories and detached trees just before queued objects for the
        // background thread; those deleted here are gone now, and their numbers are free for new
        // files
        let mut removed: HashSet<Ino> = orphans.iter().map(|&(ino, _)| ino).collect();
        for &(object, _) in &released {
            if removed.contains(&object) || self.object_used(object)? {
                continue;
            }
            // An object no file uses any more: an orphan's goes with it below; a live file whose
            // number it is uses an object of its own (a number becomes a shared object only along
            // with a row for its own file, so it is never a plain file's data)
            let garbage = match self.query_row(object) {
                Ok((st, d)) => st.nlink > 0 && d.listed(object),
                Err(Errno::ENOENT) => true,
                Err(e) => return Err(e),
            };
            if garbage {
                self.remove_object(object)?;
                removed.insert(object);
            }
        }
        for &(ino, m) in &orphans {
            // Not handed to the background thread: once the row is deleted, its number may be
            // given to a newly created file, and a background thread deleting a bit later would
            // delete the new file's object. Deleting before the commit does no harm: no dentry has
            // pointed at these rows for a long time
            if m & mode::S_IFMT == mode::S_IFREG {
                self.remove_object(ino)?;
            }
            self.db()
                .prepare_cached("DELETE FROM inodes WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![ino]))
                .map_err(db_err)?;
            self.touch_ino(ino);
            self.db()
                .prepare_cached("DELETE FROM xattrs WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![ino]))
                .map_err(db_err)?;
            self.touch_file_caps();
        }
        self.doomed
            .borrow_mut()
            .retain(|ino| !removed.contains(ino));
        Ok(())
    }

    /// From now on this store belongs to running kernel `kernel` ([`crate::kernel::Shared::id`])
    /// and may be used only while holding its BKL. In checked builds (debug builds and the
    /// `borrow-check` feature, the same as for [`crate::cell`]), using the database connection or
    /// the metadata cache without holding this BKL is treated as a kernel bug: two threads using
    /// one connection at once at best make rusqlite's statement cache panic, and at worst corrupt
    /// the SQLite connection state (rusqlite has SQLite run without locking). This catches it
    /// without having to hit the race. In release builds it does nothing.
    pub(crate) fn guard_with_bkl(&self, kernel: usize) {
        #[cfg(any(debug_assertions, feature = "borrow-check"))]
        self.owner.store(kernel, Ordering::Relaxed);
        #[cfg(not(any(debug_assertions, feature = "borrow-check")))]
        let _ = kernel;
    }

    // ---- Reads ----

    /// Runs `f` in one read transaction, whose queries see the same snapshot and share one round
    /// of locking: in WAL mode, every statement opening and closing its own read transaction takes
    /// and releases fcntl locks and checks the file length, which costs more than the query itself
    /// when a path is resolved level by level. The read transaction starts only at the first cache
    /// miss, and when everything hits the database is not touched. Runs `f` directly when already
    /// in a transaction.
    pub fn snapshot<T>(&self, f: impl FnOnce(&Store) -> Result<T, Errno>) -> Result<T, Errno> {
        if !self.db().is_autocommit() || self.snapshot_at.get().is_some() {
            return f(self);
        }
        self.snapshot_at.set(Some(None));
        let r = f(self);
        if self.snapshot_at.get().flatten().is_some() {
            // Committing a read-only transaction only releases the read lock
            let _ = self.exec(if r.is_ok() { "COMMIT" } else { "ROLLBACK" });
        }
        self.snapshot_at.set(None);
        r
    }

    /// Looks up `name` in directory `parent`.
    pub fn lookup(&self, parent: Ino, name: &[u8]) -> Result<Option<Ino>, Errno> {
        let generation = self.cache_generation();
        if let Some(hit) = self.cached(|c| c.dentries.get(&parent)?.get(name).copied()) {
            return Ok(hit);
        }
        self.begin_read()?;
        let found = self
            .db()
            .prepare_cached("SELECT ino FROM dentries WHERE parent = ?1 AND name = ?2")
            .and_then(|mut st| st.query_row(params![parent, name], |row| row.get(0)))
            .optional()
            .map_err(db_err)?;
        if generation.is_some_and(|g| self.may_fill(g)) {
            self.update(|c| {
                c.entries += 1;
                c.dentries
                    .entry(parent)
                    .or_default()
                    .insert(name.to_vec(), found);
            });
        }
        Ok(found)
    }

    /// One name in directory `parent` pointing at `ino` (for getcwd; opening a relative path also
    /// needs the absolute path, see `display_path` in `syscall::fs`, hence the caching).
    pub fn name_of(&self, parent: Ino, ino: Ino) -> Result<Option<Vec<u8>>, Errno> {
        let generation = self.cache_generation();
        if let Some(hit) = self.cached(|c| c.names.get(&parent)?.get(&ino).cloned()) {
            return Ok(hit);
        }
        self.begin_read()?;
        let found: Option<Vec<u8>> = self
            .db()
            .prepare_cached(
                "SELECT name FROM dentries WHERE parent = ?1 AND ino = ?2 ORDER BY name LIMIT 1",
            )
            .and_then(|mut st| st.query_row(params![parent, ino], |row| row.get(0)))
            .optional()
            .map_err(db_err)?;
        if generation.is_some_and(|g| self.may_fill(g)) {
            self.update(|c| {
                c.entries += 1;
                c.names
                    .entry(parent)
                    .or_default()
                    .insert(ino, found.clone());
            });
        }
        Ok(found)
    }

    /// Reads metadata. A regular file's size and mtime are taken from the host object file (its
    /// times from the database while it is held).
    pub fn stat(&self, ino: Ino) -> Result<Stat, Errno> {
        let (mut st, data) = self.row(ino)?;
        if st.kind() == mode::S_IFREG {
            fill_from_host(&mut st, &self.meta.objects_dir, data)?;
        }
        Ok(st)
    }

    /// Metadata of an open regular file: size and mtime are taken from the open object, which also
    /// works after the file is deleted.
    pub fn stat_open(&self, ino: Ino, file: &File) -> Result<Stat, Errno> {
        let (mut st, data) = self.row(ino)?;
        fill_from_object(&mut st, &file.metadata().map_err(io_err)?, data.held);
        Ok(st)
    }

    /// Whether regular file `ino` is held: it shares its object, or did since it was last
    /// written, so it must get an object of its own before it is changed ([`Store::unshare`]).
    pub fn held(&self, ino: Ino) -> Result<bool, Errno> {
        Ok(self.row(ino)?.1.held.is_some())
    }

    /// The host handed regular file `ino`'s object out for writing, opened by `path`
    /// ([`HOST_WRITERS`]).
    pub fn host_opened(&self, ino: Ino, path: &[u8]) -> Result<(), Errno> {
        let entry = (self.id()?, self.data(ino)?.object, path.to_vec());
        HOST_WRITERS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry);
        Ok(())
    }

    /// The host closed a file it had opened for writing by `path`.
    pub fn host_closed(&self, path: &[u8]) {
        let Ok(id) = self.id() else {
            return;
        };
        let mut writers = HOST_WRITERS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(i) = writers.iter().position(|(s, _, p)| *s == id && p == path) {
            writers.remove(i);
        }
    }

    /// Whether the host may be writing regular file `ino`'s object, which then must not be shared
    /// with another file in either direction (the writes would reach both).
    pub fn host_writing(&self, ino: Ino) -> Result<bool, Errno> {
        if HOST_WRITERS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
        {
            return Ok(false);
        }
        let (id, object) = (self.id()?, self.data(ino)?.object);
        Ok(HOST_WRITERS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|(s, o, _)| *s == id && *o == object))
    }

    /// The store's identity across connections: the device and inode of its `objects` directory.
    fn id(&self) -> Result<(u64, u64), Errno> {
        let st = tuxport_hal::fstat(self.meta.objects_dir.as_raw_fd()).map_err(Errno)?;
        Ok((st.st_dev, st.st_ino))
    }

    /// Metadata in the database (without touching the host; a regular file's size and times are
    /// not filled in): for permission checks when opening a file.
    pub fn attrs(&self, ino: Ino) -> Result<Stat, Errno> {
        self.stat_row(ino)
    }

    /// File type bits (no need to read the object; also works for files still open after
    /// deletion).
    pub fn kind(&self, ino: Ino) -> Result<u32, Errno> {
        Ok(self.stat_row(ino)?.kind())
    }

    /// Metadata in the database (a regular file's size and mtime are not filled in yet).
    fn stat_row(&self, ino: Ino) -> Result<Stat, Errno> {
        Ok(self.row(ino)?.0)
    }

    /// Where regular file `ino`'s data and times are.
    fn data(&self, ino: Ino) -> Result<Data, Errno> {
        Ok(self.row(ino)?.1)
    }

    /// The database row of inode `ino` ([`Store::stat_row`], [`Store::data`]).
    fn row(&self, ino: Ino) -> Result<(Stat, Data), Errno> {
        let generation = self.cache_generation();
        if let Some(hit) = self.cached(|c| c.rows.get(&ino).copied()) {
            return Ok(hit);
        }
        self.begin_read()?;
        let row = self.query_row(ino)?;
        if generation.is_some_and(|g| self.may_fill(g)) {
            self.update(|c| {
                c.entries += 1;
                c.rows.insert(ino, row);
            });
        }
        Ok(row)
    }

    fn query_row(&self, ino: Ino) -> Result<(Stat, Data), Errno> {
        let row = self
            .db()
            .prepare_cached(
                "SELECT i.mode, i.nlink, i.uid, i.gid, i.rdev, i.mtime_ns, i.ctime_ns,
                        length(i.target), s.object, s.atime_ns
                 FROM inodes i LEFT JOIN shared s ON s.ino = i.ino WHERE i.ino = ?1",
            )
            .and_then(|mut st| {
                st.query_row(params![ino], |r| {
                    Ok((
                        (
                            r.get::<_, u32>(0)?,
                            r.get::<_, u32>(1)?,
                            r.get::<_, u32>(2)?,
                            r.get::<_, u32>(3)?,
                            r.get::<_, u64>(4)?,
                        ),
                        (
                            r.get::<_, i64>(5)?,
                            r.get::<_, i64>(6)?,
                            r.get::<_, Option<u64>>(7)?,
                        ),
                        (r.get::<_, Option<Ino>>(8)?, r.get::<_, Option<i64>>(9)?),
                    ))
                })
            })
            .optional()
            .map_err(db_err)?
            .ok_or(Errno::ENOENT)?;
        let ((mode, nlink, uid, gid, rdev), (mtime_ns, ctime_ns, target_len), (object, held)) = row;
        let size = target_len.unwrap_or(0);
        let st = Stat {
            ino,
            mode,
            nlink,
            uid,
            gid,
            rdev,
            size,
            atime_ns: held.unwrap_or(mtime_ns),
            mtime_ns,
            ctime_ns,
            blocks: None,
        };
        let data = Data {
            object: object.unwrap_or(ino),
            held,
        };
        Ok((st, data))
    }

    /// Reads a symlink's target.
    pub fn readlink(&self, ino: Ino) -> Result<Vec<u8>, Errno> {
        let generation = self.cache_generation();
        if let Some(hit) = self.cached(|c| c.targets.get(&ino).cloned()) {
            return Ok(hit);
        }
        self.begin_read()?;
        let target = self
            .db()
            .prepare_cached("SELECT target FROM inodes WHERE ino = ?1")
            .and_then(|mut st| st.query_row(params![ino], |r| r.get::<_, Option<Vec<u8>>>(0)))
            .optional()
            .map_err(db_err)?
            .ok_or(Errno::ENOENT)?
            .ok_or(Errno::EINVAL)?;
        if generation.is_some_and(|g| self.may_fill(g)) {
            self.update(|c| {
                c.entries += 1;
                c.targets.insert(ino, target.clone());
            });
        }
        Ok(target)
    }

    /// A directory's parent directory; the root directory's parent is itself.
    pub fn parent(&self, dir: Ino) -> Result<Ino, Errno> {
        let generation = self.cache_generation();
        if let Some(hit) = self.cached(|c| c.parents.get(&dir).copied()) {
            return Ok(hit);
        }
        self.begin_read()?;
        let parent = self
            .db()
            .prepare_cached("SELECT parent FROM inodes WHERE ino = ?1")
            .and_then(|mut st| st.query_row(params![dir], |r| r.get(0)))
            .optional()
            .map_err(db_err)?
            .ok_or(Errno::ENOENT)?;
        if generation.is_some_and(|g| self.may_fill(g)) {
            self.update(|c| {
                c.entries += 1;
                c.parents.insert(dir, parent);
            });
        }
        Ok(parent)
    }

    /// Lists a directory's contents, sorted by name, without `.` and `..`.
    pub fn readdir(&self, dir: Ino) -> Result<Vec<DirEntry>, Errno> {
        let generation = self.cache_generation();
        if let Some(hit) = self.cached(|c| c.listings.get(&dir).cloned()) {
            return Ok(hit);
        }
        let mut stmt = self
            .db()
            .prepare_cached(
                "SELECT d.name, d.ino, i.mode FROM dentries d JOIN inodes i ON i.ino = d.ino
                 WHERE d.parent = ?1 ORDER BY d.name",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![dir], |r| {
                Ok(DirEntry {
                    name: r.get(0)?,
                    ino: r.get(1)?,
                    mode: r.get(2)?,
                })
            })
            .map_err(db_err)?;
        let list: Vec<DirEntry> = rows.collect::<Result<_, _>>().map_err(db_err)?;
        if generation.is_some_and(|g| self.may_fill(g)) {
            self.update(|c| {
                c.entries += list.len() + 1;
                c.listings.insert(dir, list.clone());
            });
        }
        Ok(list)
    }

    /// The capacity of the host filesystem the store is on (reported to the guest by statfs).
    pub fn fs_stats(&self) -> Result<tuxport_hal::FsStats, Errno> {
        tuxport_hal::statvfs(&self.objects).map_err(Errno)
    }

    // ---- Changes ----

    /// Creates a directory.
    pub fn mkdir(
        &self,
        parent: Ino,
        name: &[u8],
        perm: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Ino, Errno> {
        self.transaction(|s| {
            if s.lookup(parent, name)?.is_some() {
                return Err(Errno::EEXIST);
            }
            let ino = s.new_inode(mode::S_IFDIR | (perm & 0o7777), uid, gid, 2, parent)?;
            s.add_dentry(parent, name, ino)?;
            s.db()
                .prepare_cached("UPDATE inodes SET nlink = nlink + 1 WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![parent]))
                .map_err(db_err)?;
            s.touch_ino(parent);
            Ok(ino)
        })
    }

    /// Creates an empty regular file, returning its object open for reading and writing.
    pub fn create_file(
        &self,
        parent: Ino,
        name: &[u8],
        perm: u32,
        uid: u32,
        gid: u32,
    ) -> Result<(Ino, File), Errno> {
        self.transaction(|s| {
            if s.lookup(parent, name)?.is_some() {
                return Err(Errno::EEXIST);
            }
            let ino = s.new_inode(mode::S_IFREG | (perm & 0o7777), uid, gid, 1, 0)?;
            s.add_dentry(parent, name, ino)?;
            // Create the object last: on failure the whole operation rolls back, leaving no inode
            // without an object
            let file = s.new_object(ino)?;
            Ok((ino, file))
        })
    }

    /// Creates a directory that does not hang under the root directory (a tmpfs root): its parent
    /// is itself, it is recorded in `detached_roots`, and [`Store::remove_detached_dir`] or the
    /// next boot deletes it along with everything in it.
    pub fn create_detached_dir(&self, perm: u32, uid: u32, gid: u32) -> Result<Ino, Errno> {
        self.transaction(|s| {
            let ino = s.new_inode(mode::S_IFDIR | (perm & 0o7777), uid, gid, 2, 0)?;
            s.db()
                .prepare_cached("UPDATE inodes SET parent = ?1 WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![ino]))
                .map_err(db_err)?;
            s.db()
                .prepare_cached("INSERT INTO detached_roots (ino) VALUES (?1)")
                .and_then(|mut st| st.execute(params![ino]))
                .map_err(db_err)?;
            s.touch_ino(ino);
            Ok(ino)
        })
    }

    /// Deletes directory `dir`, which does not hang under the root directory, and everything in it
    /// (the tmpfs was unmounted). As with unlink, files still open or mapped keep working as usual,
    /// and the inode rows stay until the next boot.
    pub fn remove_detached_dir(&self, dir: Ino) -> Result<(), Errno> {
        self.transaction(|s| {
            if s.stat_row(dir).is_ok_and(|st| st.nlink > 0) {
                s.remove_contents(dir)?;
                s.db()
                    .prepare_cached("UPDATE inodes SET nlink = 0 WHERE ino = ?1")
                    .and_then(|mut st| st.execute(params![dir]))
                    .map_err(db_err)?;
                s.touch_ino(dir);
            }
            s.db()
                .prepare_cached("DELETE FROM detached_roots WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![dir]))
                .map_err(db_err)?;
            Ok(())
        })
    }

    /// Creates a regular file without a name (memfd_create): its link count is 0 and its object is
    /// deleted from the start, so the data goes away once it is closed; the inode is cleaned up
    /// the next time the store is opened. The owner is the creator's (filesystem) ids.
    pub fn create_anonymous(&self, perm: u32, uid: u32, gid: u32) -> Result<(Ino, File), Errno> {
        self.transaction(|s| {
            let ino = s.new_inode(mode::S_IFREG | (perm & 0o7777), uid, gid, 0, 0)?;
            let file = s.new_object(ino)?;
            s.remove_object(ino)?;
            Ok((ino, file))
        })
    }

    /// Creates a symbolic link.
    pub fn symlink(
        &self,
        parent: Ino,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
    ) -> Result<Ino, Errno> {
        self.transaction(|s| {
            if s.lookup(parent, name)?.is_some() {
                return Err(Errno::EEXIST);
            }
            let ino = s.new_inode(mode::S_IFLNK | 0o777, uid, gid, 1, 0)?;
            s.db()
                .prepare_cached("UPDATE inodes SET target = ?1 WHERE ino = ?2")
                .and_then(|mut st| st.execute(params![target, ino]))
                .map_err(db_err)?;
            s.touch_ino(ino);
            s.add_dentry(parent, name, ino)?;
            Ok(ino)
        })
    }

    /// Creates a device node, FIFO or socket file; these exist only in the database.
    pub fn mknod(
        &self,
        parent: Ino,
        name: &[u8],
        mode: u32,
        rdev: u64,
        uid: u32,
        gid: u32,
    ) -> Result<Ino, Errno> {
        self.transaction(|s| {
            if s.lookup(parent, name)?.is_some() {
                return Err(Errno::EEXIST);
            }
            let ino = s.new_inode(mode, uid, gid, 1, 0)?;
            s.db()
                .prepare_cached("UPDATE inodes SET rdev = ?1 WHERE ino = ?2")
                .and_then(|mut st| st.execute(params![rdev, ino]))
                .map_err(db_err)?;
            s.touch_ino(ino);
            s.add_dentry(parent, name, ino)?;
            Ok(ino)
        })
    }

    /// Adds a hard link to an existing non-directory inode.
    pub fn link(&self, parent: Ino, name: &[u8], ino: Ino) -> Result<(), Errno> {
        self.transaction(|s| {
            let row = s.stat_row(ino)?;
            if row.is_dir() {
                return Err(Errno::EPERM);
            }
            // Linux's vfs_link: an inode whose last name is gone (a deleted file still open, a
            // memfd) gets no new one; its object is doomed and would be deleted under the new name
            if row.nlink == 0 {
                return Err(Errno::ENOENT);
            }
            s.add_dentry(parent, name, ino)?;
            s.db()
                .prepare_cached("UPDATE inodes SET nlink = nlink + 1, ctime_ns = ?2 WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![ino, now_ns()]))
                .map_err(db_err)?;
            s.touch_ino(ino);
            s.object_changed(ino);
            Ok(())
        })
    }

    fn new_inode(
        &self,
        mode: u32,
        uid: u32,
        gid: u32,
        nlink: u32,
        parent: Ino,
    ) -> Result<Ino, Errno> {
        let now = now_ns();
        self.db()
            .prepare_cached(
                "INSERT INTO inodes (mode, uid, gid, nlink, parent, mtime_ns, ctime_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            )
            .and_then(|mut st| st.execute(params![mode, uid, gid, nlink, parent, now]))
            .map_err(db_err)?;
        let ino = self.db().last_insert_rowid() as Ino;
        self.touch_ino(ino);
        self.fill(Fill::Row(Stat {
            ino,
            mode,
            nlink,
            uid,
            gid,
            rdev: 0,
            size: 0,
            atime_ns: now,
            mtime_ns: now,
            ctime_ns: now,
            blocks: None,
        }));
        let (lo, hi) = self.meta.commits.fresh();
        if lo.load(Ordering::Relaxed) == 0 {
            lo.store(ino, Ordering::Relaxed);
        }
        hi.fetch_max(ino, Ordering::Relaxed);
        Ok(ino)
    }

    fn add_dentry(&self, parent: Ino, name: &[u8], ino: Ino) -> Result<(), Errno> {
        if self.lookup(parent, name)?.is_some() {
            return Err(Errno::EEXIST);
        }
        let dir = self.stat_row(parent)?;
        if !dir.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        // Nothing can be created in a deleted directory (still someone's cwd, or still open):
        // Linux's may_create returns ENOENT on seeing IS_DEADDIR
        if dir.nlink == 0 {
            return Err(Errno::ENOENT);
        }
        self.db()
            .prepare_cached("INSERT INTO dentries (parent, name, ino) VALUES (?1, ?2, ?3)")
            .and_then(|mut st| st.execute(params![parent, name, ino]))
            .map_err(db_err)?;
        self.touch_name(parent, name);
        self.fill(Fill::Name(parent, name.to_vec(), ino));
        self.dir_changed(parent)
    }

    /// Creates an empty object for a new inode. An existing one was left by an inode lost before
    /// it could be committed (the inode number was allocated again), and is emptied and reused.
    fn new_object(&self, ino: Ino) -> Result<File, Errno> {
        self.open_number(ino, libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC)
    }

    /// Changes the permission bits (keeping the type bits).
    pub fn set_mode(&self, ino: Ino, perm: u32) -> Result<(), Errno> {
        self.statement(|s| {
            let n = s
                .db()
                .prepare_cached(
                    "UPDATE inodes SET mode = (mode & ?2) | ?3, ctime_ns = ?4 WHERE ino = ?1",
                )
                .and_then(|mut st| st.execute(params![ino, mode::S_IFMT, perm & 0o7777, now_ns()]))
                .map_err(db_err)?;
            s.touch_ino(ino);
            if n == 0 {
                return Err(Errno::ENOENT);
            }
            s.object_changed(ino);
            Ok(())
        })
    }

    /// Changes the owner and group; `None` means unchanged. For non-directories the setuid bit is
    /// cleared as well (Linux does this for root too), and the setgid bit when the file is
    /// group-executable or `keep_sgid` is false: the caller is neither in the file's group nor has
    /// `CAP_FSETID` (Linux's `setattr_should_drop_sgid`, since 6.2).
    pub fn set_owner(
        &self,
        ino: Ino,
        uid: Option<u32>,
        gid: Option<u32>,
        keep_sgid: bool,
    ) -> Result<(), Errno> {
        let st = self.stat_row(ino)?;
        let mut perm = st.mode & 0o7777;
        if !st.is_dir() {
            perm &= !mode::S_ISUID;
            if perm & 0o010 != 0 || !keep_sgid {
                perm &= !mode::S_ISGID;
            }
        }
        self.transaction(|s| {
            // As in Linux's chown_common, non-directories also lose their file capabilities
            // (ATTR_KILL_PRIV)
            if !st.is_dir() && s.has_file_caps(ino)? {
                s.remove_xattr(ino, XATTR_NAME_CAPS)?;
            }
            s.set_owner_mode(ino, perm, uid.unwrap_or(st.uid), gid.unwrap_or(st.gid))
        })
    }

    /// Removes a regular file's privileges before it is written or truncated (Linux's
    /// `file_remove_privs`): without `CAP_FSETID` (`keep_setid` false), removes the setuid bit,
    /// and the setgid bit when the group execute bit is set; file capabilities are removed
    /// whoever writes. Does not touch the database when there is nothing to remove.
    pub fn remove_privs(&self, ino: Ino, keep_setid: bool) -> Result<(), Errno> {
        let st = self.stat_row(ino)?;
        if st.kind() != mode::S_IFREG {
            return Ok(());
        }
        let mut perm = st.mode & 0o7777;
        if !keep_setid {
            perm &= !mode::S_ISUID;
            if perm & 0o010 != 0 {
                perm &= !mode::S_ISGID;
            }
        }
        let caps = self.has_file_caps(ino)?;
        if !caps && perm == st.mode & 0o7777 {
            return Ok(());
        }
        self.transaction(|s| {
            if caps {
                s.remove_xattr(ino, XATTR_NAME_CAPS)?;
            }
            s.set_owner_mode(ino, perm, st.uid, st.gid)
        })
    }

    /// Whether inode `ino` has file capabilities (`security.capability`): looked up in the whole
    /// cached set.
    pub fn has_file_caps(&self, ino: Ino) -> Result<bool, Errno> {
        let generation = self.cache_generation();
        if let Some(hit) = self.cached(|c| c.file_caps.as_ref().map(|m| m.contains_key(&ino))) {
            return Ok(hit);
        }
        self.begin_read()?;
        let all: Map<Ino, ()> = {
            let mut stmt = self
                .db()
                .prepare_cached("SELECT ino FROM xattrs WHERE name = ?1")
                .map_err(db_err)?;
            let rows = stmt
                .query_map(params![XATTR_NAME_CAPS], |r| r.get(0))
                .map_err(db_err)?;
            rows.map(|r| r.map(|ino| (ino, ())))
                .collect::<Result<_, _>>()
                .map_err(db_err)?
        };
        let hit = all.contains_key(&ino);
        if generation.is_some_and(|g| self.may_fill(g)) {
            self.update(|c| c.file_caps = Some(all));
        }
        Ok(hit)
    }

    /// Changes the permission bits, owner and group (keeping the type bits).
    pub fn set_owner_mode(&self, ino: Ino, perm: u32, uid: u32, gid: u32) -> Result<(), Errno> {
        self.statement(|s| {
            s.db().prepare_cached("UPDATE inodes SET mode = (mode & ?2) | ?3, uid = ?4, gid = ?5, ctime_ns = ?6 WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![ino, mode::S_IFMT, perm & 0o7777, uid, gid, now_ns()]))
                .map_err(db_err)?;
            s.touch_ino(ino);
            s.object_changed(ino);
            Ok(())
        })
    }

    /// unlink(2): removes a dentry of a non-directory.
    pub fn unlink(&self, parent: Ino, name: &[u8]) -> Result<(), Errno> {
        let ino = self.lookup(parent, name)?.ok_or(Errno::ENOENT)?;
        if self.stat_row(ino)?.is_dir() {
            return Err(Errno::EISDIR);
        }
        self.transaction(|s| s.remove_link(parent, name, ino))
    }

    /// rmdir(2): removes an empty directory.
    pub fn rmdir(&self, parent: Ino, name: &[u8]) -> Result<(), Errno> {
        let ino = self.lookup(parent, name)?.ok_or(Errno::ENOENT)?;
        if !self.stat_row(ino)?.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        if ino == ROOT {
            return Err(Errno::EBUSY);
        }
        self.transaction(|s| s.remove_dir(parent, name, ino))
    }

    /// rename(2)/renameat2(2): `noreplace` is `RENAME_NOREPLACE`, `exchange` is
    /// `RENAME_EXCHANGE`. The whole operation is done in one transaction.
    pub fn rename(
        &self,
        old_dir: Ino,
        old_name: &[u8],
        new_dir: Ino,
        new_name: &[u8],
        noreplace: bool,
        exchange: bool,
    ) -> Result<(), Errno> {
        self.transaction(|s| {
            let src = s.lookup(old_dir, old_name)?.ok_or(Errno::ENOENT)?;
            let dst = s.lookup(new_dir, new_name)?;
            let src_dir = s.stat_row(src)?.is_dir();
            if exchange {
                let dst = dst.ok_or(Errno::ENOENT)?;
                if src == dst {
                    return Ok(());
                }
                let dst_dir = s.stat_row(dst)?.is_dir();
                if (src_dir && s.is_ancestor(src, new_dir)?)
                    || (dst_dir && s.is_ancestor(dst, old_dir)?)
                {
                    return Err(Errno::EINVAL);
                }
                s.set_dentry(old_dir, old_name, dst)?;
                s.set_dentry(new_dir, new_name, src)?;
                if src_dir {
                    s.reparent_dir(src, old_dir, new_dir)?;
                }
                if dst_dir {
                    s.reparent_dir(dst, new_dir, old_dir)?;
                }
                s.dirs_changed(old_dir, new_dir)?;
                s.touch_ctime(src)?;
                return s.touch_ctime(dst);
            }
            if let Some(d) = dst {
                if noreplace {
                    return Err(Errno::EEXIST);
                }
                // The two names point at the same inode (hard links): do nothing
                if d == src {
                    return Ok(());
                }
                let dst_dir = s.stat_row(d)?.is_dir();
                match (src_dir, dst_dir) {
                    (true, false) => return Err(Errno::ENOTDIR),
                    (false, true) => return Err(Errno::EISDIR),
                    _ => {}
                }
            }
            if src_dir && s.is_ancestor(src, new_dir)? {
                return Err(Errno::EINVAL);
            }
            if dst.is_none() && s.stat_row(new_dir)?.nlink == 0 {
                return Err(Errno::ENOENT);
            }
            if let Some(d) = dst {
                if src_dir {
                    s.remove_dir(new_dir, new_name, d)?;
                } else {
                    s.remove_link(new_dir, new_name, d)?;
                }
                // A regular file renamed over another file: make its data durable before the
                // commit
                if s.stat_row(src)?.kind() == mode::S_IFREG {
                    s.replaced.borrow_mut().push(src);
                }
            }
            s.db()
                .prepare_cached("DELETE FROM dentries WHERE parent = ?1 AND name = ?2")
                .and_then(|mut st| st.execute(params![old_dir, old_name]))
                .map_err(db_err)?;
            s.touch_name(old_dir, old_name);
            s.set_dentry(new_dir, new_name, src)?;
            if src_dir {
                s.reparent_dir(src, old_dir, new_dir)?;
            }
            s.dirs_changed(old_dir, new_dir)?;
            s.touch_ctime(src)
        })
    }

    /// Whether `ancestor` is `dir` itself or one of its ancestor directories. The parent of the
    /// root directory and of directories not under the root directory is itself.
    fn is_ancestor(&self, ancestor: Ino, mut dir: Ino) -> Result<bool, Errno> {
        loop {
            if dir == ancestor {
                return Ok(true);
            }
            let up = self.parent(dir)?;
            if up == dir {
                return Ok(false);
            }
            dir = up;
        }
    }

    /// Removes a dentry pointing at a non-directory; when the last link goes, a regular file's
    /// object is deleted along with it.
    fn remove_link(&self, parent: Ino, name: &[u8], ino: Ino) -> Result<(), Errno> {
        self.db()
            .prepare_cached("DELETE FROM dentries WHERE parent = ?1 AND name = ?2")
            .and_then(|mut st| st.execute(params![parent, name]))
            .map_err(db_err)?;
        self.db()
            .prepare_cached("UPDATE inodes SET nlink = nlink - 1, ctime_ns = ?2 WHERE ino = ?1")
            .and_then(|mut st| st.execute(params![ino, now_ns()]))
            .map_err(db_err)?;
        self.touch_name(parent, name);
        self.touch_ino(ino);
        self.dir_changed(parent)?;
        let (st, data) = self.row(ino)?;
        if st.nlink == 0 && st.kind() == mode::S_IFREG {
            // Deleted after the commit: if the app is killed before the commit, the dentry is
            // still there, so the object must be too. A held file keeps sharing its object while
            // it may still be open: its descriptors still hold the shared object, and a write
            // through them must find what to copy ([`Store::release`])
            if data.held.is_none() {
                self.drop_object(ino, data)?;
            }
        } else {
            self.object_changed(ino);
        }
        Ok(())
    }

    /// Removes the dentry of an empty directory.
    fn remove_dir(&self, parent: Ino, name: &[u8], ino: Ino) -> Result<(), Errno> {
        if !self.is_empty_dir(ino)? {
            return Err(Errno::ENOTEMPTY);
        }
        self.db()
            .prepare_cached("DELETE FROM dentries WHERE parent = ?1 AND name = ?2")
            .and_then(|mut st| st.execute(params![parent, name]))
            .map_err(db_err)?;
        self.db()
            .prepare_cached("UPDATE inodes SET nlink = 0 WHERE ino = ?1")
            .and_then(|mut st| st.execute(params![ino]))
            .map_err(db_err)?;
        self.db()
            .prepare_cached("UPDATE inodes SET nlink = nlink - 1 WHERE ino = ?1")
            .and_then(|mut st| st.execute(params![parent]))
            .map_err(db_err)?;
        self.touch_name(parent, name);
        self.touch_ino(ino);
        self.touch_ino(parent);
        self.dir_changed(parent)
    }

    /// Whether a directory is empty.
    fn is_empty_dir(&self, dir: Ino) -> Result<bool, Errno> {
        let n: i64 = self
            .db()
            .prepare_cached("SELECT count(*) FROM dentries WHERE parent = ?1")
            .and_then(|mut st| st.query_row(params![dir], |r| r.get(0)))
            .map_err(db_err)?;
        Ok(n == 0)
    }

    fn set_dentry(&self, parent: Ino, name: &[u8], ino: Ino) -> Result<(), Errno> {
        self.db()
            .prepare_cached(
                "INSERT OR REPLACE INTO dentries (parent, name, ino) VALUES (?1, ?2, ?3)",
            )
            .and_then(|mut st| st.execute(params![parent, name, ino]))
            .map_err(db_err)?;
        self.touch_name(parent, name);
        self.fill(Fill::Name(parent, name.to_vec(), ino));
        Ok(())
    }

    /// Updates `..` and the link counts on both sides when directory `dir` moves from under `from`
    /// to under `to`.
    fn reparent_dir(&self, dir: Ino, from: Ino, to: Ino) -> Result<(), Errno> {
        if from == to {
            return Ok(());
        }
        for (ino, delta) in [(from, -1i64), (to, 1)] {
            self.db()
                .prepare_cached("UPDATE inodes SET nlink = nlink + ?2 WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![ino, delta]))
                .map_err(db_err)?;
        }
        self.db()
            .prepare_cached("UPDATE inodes SET parent = ?2 WHERE ino = ?1")
            .and_then(|mut st| st.execute(params![dir, to]))
            .map_err(db_err)?;
        for ino in [dir, from, to] {
            self.touch_ino(ino);
        }
        Ok(())
    }

    /// A rename changed two directories ([`Store::dir_changed`]); the same directory is changed
    /// only once.
    fn dirs_changed(&self, a: Ino, b: Ino) -> Result<(), Errno> {
        self.dir_changed(a)?;
        if b != a { self.dir_changed(b) } else { Ok(()) }
    }

    /// The names in directory `dir` changed: its mtime and ctime become now, as with Linux's
    /// create, delete and rename (Python's import, Maildir and git detect changes by directory
    /// mtime). The new row goes into the cache: path resolution reads the directory's row at every
    /// level.
    fn dir_changed(&self, dir: Ino) -> Result<(), Errno> {
        if let Some(dirs) = self.changed_dirs.borrow_mut().as_mut()
            && dirs.len() < MAX_CHANGED_DIRS
        {
            dirs.insert(dir);
        }
        let mut st = self.stat_row(dir)?;
        let now = now_ns();
        self.db()
            .prepare_cached("UPDATE inodes SET mtime_ns = ?2, ctime_ns = ?2 WHERE ino = ?1")
            .and_then(|mut st| st.execute(params![dir, now]))
            .map_err(db_err)?;
        self.touch_ino(dir);
        (st.atime_ns, st.mtime_ns, st.ctime_ns) = (now, now, now);
        self.fill(Fill::Row(st));
        Ok(())
    }

    /// Sets an extended attribute. With `create` (`XATTR_CREATE`) it fails if the attribute
    /// exists, with `replace` (`XATTR_REPLACE`) if it does not.
    pub fn set_xattr(
        &self,
        ino: Ino,
        name: &[u8],
        value: &[u8],
        create: bool,
        replace: bool,
    ) -> Result<(), Errno> {
        self.transaction(|s| {
            let exists = s.get_xattr(ino, name).is_ok();
            if create && exists {
                return Err(Errno::EEXIST);
            }
            if replace && !exists {
                return Err(Errno::ENODATA);
            }
            s.db()
                .prepare_cached(
                    "INSERT OR REPLACE INTO xattrs (ino, name, value) VALUES (?1, ?2, ?3)",
                )
                .and_then(|mut st| st.execute(params![ino, name, value]))
                .map_err(db_err)?;
            if name == XATTR_NAME_CAPS {
                s.touch_file_caps();
            }
            s.touch_ctime(ino)
        })
    }

    /// Reads an extended attribute; ENODATA if there is none.
    pub fn get_xattr(&self, ino: Ino, name: &[u8]) -> Result<Vec<u8>, Errno> {
        self.db()
            .prepare_cached("SELECT value FROM xattrs WHERE ino = ?1 AND name = ?2")
            .and_then(|mut st| st.query_row(params![ino, name], |r| r.get(0)))
            .optional()
            .map_err(db_err)?
            .ok_or(Errno::ENODATA)
    }

    /// The names of all extended attributes.
    pub fn list_xattr(&self, ino: Ino) -> Result<Vec<Vec<u8>>, Errno> {
        let mut stmt = self
            .db()
            .prepare_cached("SELECT name FROM xattrs WHERE ino = ?1 ORDER BY name")
            .map_err(db_err)?;
        let rows = stmt.query_map(params![ino], |r| r.get(0)).map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)
    }

    /// Removes an extended attribute; ENODATA if there is none.
    pub fn remove_xattr(&self, ino: Ino, name: &[u8]) -> Result<(), Errno> {
        self.transaction(|s| {
            let n = s
                .db()
                .prepare_cached("DELETE FROM xattrs WHERE ino = ?1 AND name = ?2")
                .and_then(|mut st| st.execute(params![ino, name]))
                .map_err(db_err)?;
            if n == 0 {
                return Err(Errno::ENODATA);
            }
            if name == XATTR_NAME_CAPS {
                s.touch_file_caps();
            }
            s.touch_ctime(ino)
        })
    }

    fn touch_ctime(&self, ino: Ino) -> Result<(), Errno> {
        self.db()
            .prepare_cached("UPDATE inodes SET ctime_ns = ?2 WHERE ino = ?1")
            .and_then(|mut st| st.execute(params![ino, now_ns()]))
            .map_err(db_err)?;
        self.touch_ino(ino);
        self.object_changed(ino);
        Ok(())
    }

    /// Sets the modification time: the host object's mtime for regular files, the database for
    /// the rest.
    pub fn set_mtime(&self, ino: Ino, mtime_ns: i64) -> Result<(), Errno> {
        self.set_times(ino, None, Some(FileTime::At(mtime_ns)))
    }

    /// Sets the access and modification times (None means unchanged). For regular files the host
    /// object's times change; the rest record only the modification time, and the access time
    /// follows it. ctime always becomes now (for regular files, the object's).
    pub fn set_times(
        &self,
        ino: Ino,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> Result<(), Errno> {
        let (st, data) = self.row(ino)?;
        if st.kind() == mode::S_IFREG && data.held.is_some() {
            return self.set_held_times(ino, atime, mtime);
        }
        if st.kind() == mode::S_IFREG {
            let mut name = [0u8; 21];
            return tuxport_hal::set_times_at(
                self.meta.objects_dir.as_raw_fd(),
                object_name(data.object, &mut name),
                atime.map(FileTime::host),
                mtime.map(FileTime::host),
            )
            .map_err(|e| object_err(ino, e));
        }
        let now = now_ns();
        let mtime_ns = mtime.map(|t| match t {
            FileTime::Now => now,
            FileTime::At(ns) => ns,
        });
        self.statement(|s| {
            s.db().prepare_cached(
                "UPDATE inodes SET mtime_ns = coalesce(?2, mtime_ns), ctime_ns = ?3 WHERE ino = ?1",
            )
            .and_then(|mut st| st.execute(params![ino, mtime_ns, now]))
            .map_err(db_err)?;
            s.touch_ino(ino);
            Ok(())
        })
    }

    /// Sets the access and modification times of regular file `ino` through its open object: also
    /// works after the file is deleted (when the object has already been deleted).
    pub fn set_times_open(
        &self,
        ino: Ino,
        file: &File,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> Result<(), Errno> {
        if self.held(ino)? {
            return self.set_held_times(ino, atime, mtime);
        }
        tuxport_hal::set_times_fd(
            file.as_raw_fd(),
            atime.map(FileTime::host),
            mtime.map(FileTime::host),
        )
        .map_err(|e| object_err(ino, e))
    }

    /// The times of held regular file `ino` are the database's; ctime becomes now.
    fn set_held_times(
        &self,
        ino: Ino,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> Result<(), Errno> {
        let now = now_ns();
        let ns = |t: Option<FileTime>| {
            t.map(|t| match t {
                FileTime::Now => now,
                FileTime::At(ns) => ns,
            })
        };
        let (atime, mtime) = (ns(atime), ns(mtime));
        self.transaction(|s| {
            s.db()
                .prepare_cached(
                    "UPDATE inodes SET mtime_ns = coalesce(?2, mtime_ns), ctime_ns = ?3
                     WHERE ino = ?1",
                )
                .and_then(|mut st| st.execute(params![ino, mtime, now]))
                .map_err(db_err)?;
            s.db()
                .prepare_cached(
                    "UPDATE shared SET atime_ns = coalesce(?2, atime_ns) WHERE ino = ?1",
                )
                .and_then(|mut st| st.execute(params![ino, atime]))
                .map_err(db_err)?;
            s.touch_ino(ino);
            Ok(())
        })
    }

    // ---- Transactions and deferred commits ----

    /// Runs `f` as one metadata operation: either all of it takes effect or (when `f` fails or
    /// panics) all of it is rolled back. The operation is a savepoint in the deferred write
    /// transaction (module docs); the write transaction takes the write lock right at its start
    /// (`IMMEDIATE`): a transaction that reads first and writes later cannot be upgraded to a
    /// write transaction after another process has committed, and busy_timeout does not help
    /// then. When already inside an operation (importing a rootfs is one whole operation), it
    /// joins the enclosing one.
    pub fn transaction<T>(&self, f: impl FnOnce(&Store) -> Result<T, Errno>) -> Result<T, Errno> {
        self.operation(true, f)
    }

    /// An operation with only one write statement (changing permissions, owner, times): the
    /// statement itself is atomic, and on failure SQLite undoes only it, so no savepoint is
    /// needed.
    fn statement<T>(&self, f: impl FnOnce(&Store) -> Result<T, Errno>) -> Result<T, Errno> {
        self.operation(false, f)
    }

    fn operation<T>(
        &self,
        savepoint: bool,
        f: impl FnOnce(&Store) -> Result<T, Errno>,
    ) -> Result<T, Errno> {
        // Inside a read transaction ([`Store::snapshot`]) it also runs directly, writing inside the
        // read transaction as before
        if self.writing.get() || (self.pending.get().is_none() && !self.db().is_autocommit()) {
            return f(self);
        }
        if self.pending.get().is_none() {
            self.exec("BEGIN IMMEDIATE")?;
            self.pending.set(Some(Instant::now()));
            if self.defers()
                && let Some(wake) = &*self.waker.borrow()
            {
                wake();
            }
        }
        let r = if savepoint {
            self.exec("SAVEPOINT op")
        } else {
            Ok(())
        };
        let r = r.and_then(|()| {
            let op = Op {
                store: self,
                savepoint,
                doomed: self.doomed.borrow().len(),
                done: false,
            };
            self.writing.set(true);
            let r = f(self);
            let r = match r {
                Ok(v) if savepoint => self.exec("RELEASE op").map(|()| v),
                r => r,
            };
            op.finish(r.is_ok());
            r
        });
        // Commit immediately when another process is also using the store, so that it does not
        // miss the changes; also commit once deferred long enough
        let overdue = self
            .pending
            .get()
            .is_some_and(|t| t.elapsed() >= self.delay);
        if overdue || !self.defers() {
            self.commit()?;
        }
        r
    }

    /// Write transactions may be deferred: no other connection is open on the store.
    fn defers(&self) -> bool {
        self.meta.commits.users().load(Ordering::Relaxed) <= 1
    }

    /// Takes the directories whose dentries changed (created, removed, renamed) since the last
    /// call; recording starts at the first call (the app has the Files app refresh them, the
    /// android/documents map). Directories are recorded as of the operation, so ones whose changes
    /// were later rolled back are included too.
    pub fn take_changed_dirs(&self) -> Vec<Ino> {
        let mut dirs = self.changed_dirs.borrow_mut();
        match dirs.as_mut() {
            Some(set) => std::mem::take(set).into_iter().collect(),
            None => {
                *dirs = Some(Default::default());
                Vec::new()
            }
        }
    }

    /// Wakes `wake` when a commit is deferred ([`Store::commit_due`]).
    pub fn set_waker(&self, wake: Box<dyn Fn()>) {
        *self.waker.borrow_mut() = Some(wake);
    }

    /// Commits the deferred write transaction if it is due; if not yet, returns when it will be
    /// due.
    pub fn commit_due(&self) -> Option<Instant> {
        let due = self.pending.get()? + self.delay;
        if Instant::now() < due {
            return Some(due);
        }
        let _ = self.commit();
        None
    }

    /// Commits the deferred write transaction (on fsync, sync, close) and then deletes the objects
    /// waiting for it. If the commit fails, all deferred changes are lost: the database returns to
    /// the state of the last commit, and the whole cache is cleared. Only writes into the WAL,
    /// without waiting for it to be durable ([`Meta::sync_metadata`]).
    pub fn commit(&self) -> Result<(), Errno> {
        if self.pending.take().is_none() {
            return Ok(());
        }
        self.flush_replaced();
        let r = self.exec("COMMIT");
        let doomed = std::mem::take(&mut *self.doomed.borrow_mut());
        if r.is_err() {
            if !self.db().is_autocommit() {
                let _ = self.exec("ROLLBACK");
            }
            self.update(|c| c.clear(c.generation));
            let _ = self.remove_lost_objects();
            return r;
        }
        let (lo, hi) = self.meta.commits.fresh();
        lo.store(0, Ordering::SeqCst);
        hi.store(0, Ordering::SeqCst);
        // The counter must be bumped after the commit: if bumped before, another process could
        // reread the old content before the commit completes and record it under the new count.
        // If someone else changed the counter before this, our own cache is invalidated as a
        // whole too. It must also be bumped before objects are handed to the background thread
        // for deletion: the thread judges by the counter whether this commit is durable
        // ([`Meta::sync_metadata`])
        self.update(|c| {
            let before = self.meta.commits.bump();
            if before == c.generation {
                c.generation = before + 1;
            } else {
                c.clear(before + 1);
            }
        });
        self.reap_later(doomed);
        Ok(())
    }

    /// Makes the data of regular files in the write transaction that were renamed over other
    /// files ([`Store::rename`]) durable before the commit: after the commit the host may write
    /// the WAL back to disk at any time, and by then the new file's content must already be on
    /// disk, so that after power loss the name does not hold an empty or truncated file. Native
    /// ext4 does the same on a replacing rename (auto_da_alloc), and apk installing files and
    /// editors saving rely on it without calling fsync; Tuxport's renames are only in the
    /// database, invisible to the host. A few files are fdatasynced one by one (writeback is
    /// started for all of them first, which ext4 merges into one journal commit); for more, the
    /// whole filesystem is syncfsed once. This holds the BKL, so other processes wait during the
    /// flush. Failures to flush are only logged.
    fn flush_replaced(&self) {
        let mut inos = std::mem::take(&mut *self.replaced.borrow_mut());
        if inos.is_empty() {
            return;
        }
        inos.sort_unstable();
        inos.dedup();
        let r = if inos.len() > FLUSH_EACH {
            tuxport_hal::syncfs(self.meta.fs.as_raw_fd())
        } else {
            // Files deleted after the rename cannot be opened and need no attention
            let files: Vec<File> = inos
                .iter()
                .filter_map(|&ino| self.open_object(ino, false).ok())
                .collect();
            for f in &files {
                let _ = tuxport_hal::sync_file_range(
                    f.as_raw_fd(),
                    0,
                    0,
                    tuxport_hal::SYNC_FILE_RANGE_WRITE,
                );
            }
            files
                .iter()
                .try_for_each(|f| tuxport_hal::fsync(f.as_raw_fd(), true))
        };
        if let Err(e) = r {
            crate::debug::log(format_args!("inode store: flushing replaced files: {e}"));
        }
        #[cfg(test)]
        self.meta
            .flushes
            .fetch_add(inos.len() as u64, Ordering::Relaxed);
    }

    /// Hands objects to the background thread for deletion; deletes them itself when the thread
    /// was not started.
    fn reap_later(&self, inos: Vec<Ino>) {
        if inos.is_empty() {
            return;
        }
        if self.reaper.is_none() {
            let _ = self.meta.sync_metadata();
            for ino in inos {
                let _ = self.remove_object(ino);
            }
            return;
        }
        let r = &self.meta.reaper;
        let mut st = r.state.lock().unwrap_or_else(PoisonError::into_inner);
        #[cfg(test)]
        {
            st.handed += inos.len();
        }
        st.queue.extend(inos);
        drop(st);
        r.work.notify_one();
    }

    // ---- Metadata cache ----

    /// Called before reading the cache: clears it first if it is stale. Returns the current commit
    /// count when query results may be filled into the cache (not inside an operation).
    fn cache_generation(&self) -> Option<u64> {
        let now = self.meta.commits.get();
        if self.cached(|c| c.generation != now || c.entries > Cache::MAX_ENTRIES) {
            self.update(|c| c.clear(now));
        }
        (!self.writing.get()).then_some(now)
    }

    /// Called on a cache miss, before querying the database: inside [`Store::snapshot`], the
    /// first query starts the read transaction.
    fn begin_read(&self) -> Result<(), Errno> {
        if self.snapshot_at.get() == Some(None) {
            self.snapshot_at.set(Some(Some(self.meta.commits.get())));
            self.exec("BEGIN")?;
        }
        Ok(())
    }

    /// Whether a result just queried may go into the cache: nobody has committed since the read
    /// transaction began, nor during this query (otherwise the result may be older than
    /// `generation`).
    fn may_fill(&self, generation: u64) -> bool {
        self.snapshot_at
            .get()
            .flatten()
            .is_none_or(|s| s == generation)
            && self.meta.commits.get() == generation
    }

    /// Name `name` in directory `dir` changed: removes the affected entries from our own cache.
    /// Other processes' caches are invalidated at commit ([`Store::commit`]).
    fn touch_name(&self, dir: Ino, name: &[u8]) {
        self.fills
            .borrow_mut()
            .retain(|f| !matches!(f, Fill::Name(d, n, _) if *d == dir && n == name));
        self.update(|c| {
            if let Some(d) = c.dentries.get_mut(&dir) {
                d.remove(name);
            }
            c.listings.remove(&dir);
            c.names.remove(&dir);
        });
    }

    /// The row of inode `ino` changed (as above).
    fn touch_ino(&self, ino: Ino) {
        self.fills
            .borrow_mut()
            .retain(|f| !matches!(f, Fill::Row(st) if st.ino == ino));
        self.update(|c| {
            c.rows.remove(&ino);
            c.targets.remove(&ino);
            c.parents.remove(&ino);
        });
    }

    /// File capabilities changed: invalidates the cached set.
    fn touch_file_caps(&self) {
        self.update(|c| c.file_caps = None);
    }

    /// Puts `fill` into the cache after the operation succeeds.
    fn fill(&self, fill: Fill) {
        let mut fills = self.fills.borrow_mut();
        if fills.len() < Fill::MAX {
            fills.push(fill);
        }
    }

    /// The cache is current (stale content was cleared before the cache was read): used to fill in
    /// what an operation created after it succeeds.
    fn cache_generation_any(&self) -> bool {
        self.cached(|c| c.generation) == self.meta.commits.get()
    }

    /// Reads the cache. Only Store's methods change the cache, so they need no lock to read it
    /// themselves ([`Meta`]).
    fn cached<R>(&self, f: impl FnOnce(&Cache) -> R) -> R {
        self.check_owner();
        // SAFETY: Store is not Sync, so no other thread modifies it at the same time; f does not
        // call back into Store's methods, so the borrow does not overlap with update's
        f(unsafe { &*self.meta.cache.get() })
    }

    /// Changes the cache: holds the write lock, waiting for threads reading the cache without the
    /// BKL to finish.
    fn update<R>(&self, f: impl FnOnce(&mut Cache) -> R) -> R {
        self.check_owner();
        let _guard = self
            .meta
            .lock
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        // SAFETY: the write lock is held, so no other thread is reading; f does not call back into
        // Store's methods
        f(unsafe { &mut *self.meta.cache.get() })
    }

    /// The part needed to read the cache without the BKL ([`Meta`]).
    pub(crate) fn meta(&self) -> &Arc<Meta> {
        &self.meta
    }

    // ---- Objects ----

    /// Opens a regular file's object. Opening a held file for writing gives it an object of its own
    /// first ([`Store::make_private`]); the kernel does that itself beforehand, moving the file's
    /// other open descriptions along, so this is for the host's own writes.
    pub fn open_object(&self, ino: Ino, write: bool) -> Result<File, Errno> {
        if write {
            self.make_private(ino, None)?;
        }
        self.object(ino, if write { libc::O_RDWR } else { libc::O_RDONLY })
    }

    /// Opens a regular file's object for reading and writing, with `O_APPEND` under `append` (for
    /// writes when the host browses the store).
    pub fn open_object_append(&self, ino: Ino, append: bool) -> Result<File, Errno> {
        self.make_private(ino, None)?;
        let append = if append { libc::O_APPEND } else { 0 };
        self.object(ino, libc::O_RDWR | append)
    }

    /// Truncates a regular file.
    pub fn truncate(&self, ino: Ino, len: u64) -> Result<(), Errno> {
        self.make_private(ino, Some(len))?;
        self.open_object(ino, true)?.set_len(len).map_err(io_err)
    }

    /// Opens regular file `ino`'s object (its own, or the one it shares); `flags` are host open
    /// flags.
    fn object(&self, ino: Ino, flags: i32) -> Result<File, Errno> {
        self.open_number(self.data(ino)?.object, flags)
    }

    /// Opens object `object` (relative to the `objects` directory, without resolving the whole
    /// path); `flags` are host open flags.
    fn open_number(&self, object: Ino, flags: i32) -> Result<File, Errno> {
        let mut name = [0u8; 21];
        let dir = self.meta.objects_dir.as_raw_fd();
        tuxport_hal::open_at(dir, object_name(object, &mut name), flags, OBJECT_MODE)
            .map_err(|e| object_err(object, e))
    }

    /// The database changed the metadata of inode `ino`: for a regular file, chmod the object so
    /// that the host updates its ctime (see the module docs); a held file's ctime is the
    /// database's, which the change set already. Ignored if the object is gone (still open after
    /// deletion) or cannot be changed; the ctime is then just not updated.
    fn object_changed(&self, ino: Ino) {
        if let Ok((st, data)) = self.row(ino)
            && st.kind() == mode::S_IFREG
            && data.held.is_none()
        {
            let mut name = [0u8; 21];
            let dir = self.meta.objects_dir.as_raw_fd();
            let _ = tuxport_hal::chmod_at(dir, object_name(data.object, &mut name), OBJECT_MODE);
        }
    }

    /// Makes regular file `dst`'s data the same object as `src`'s (FICLONE, a whole-file
    /// copy_file_range, an overlay copy-up): both are held from then on, `dst`'s modification and
    /// change times become now, and its old object goes once no other file uses it. The caller
    /// makes sure nothing writes `src`'s object in place meanwhile (no descriptor open for
    /// writing it, no writable shared mapping of it) and that nothing maps `dst`. ENOENT for a
    /// deleted `src`, whose object may already be on its way to the reaper.
    pub fn share(&self, src: Ino, dst: Ino) -> Result<(), Errno> {
        if src == dst {
            return Err(Errno::EINVAL);
        }
        self.transaction(|s| {
            let (sst, sdata) = s.row(src)?;
            let (dst_st, ddata) = s.row(dst)?;
            if sst.kind() != mode::S_IFREG || dst_st.kind() != mode::S_IFREG {
                return Err(Errno::EINVAL);
            }
            // A deleted source's object may already be on its way to the reaper
            if sst.nlink == 0 {
                return Err(Errno::ENOENT);
            }
            // The source's times move into the database as they are
            if sdata.held.is_none() {
                let o = s.object_stat(sdata.object)?;
                s.hold(src, sdata.object, (o.atime_ns, o.mtime_ns, o.ctime_ns))?;
            }
            // The destination keeps its access time (Linux's file_modified changes the other two)
            let atime = match ddata.held {
                Some(atime) => atime,
                None => s.object_stat(ddata.object).map_or(now_ns(), |o| o.atime_ns),
            };
            s.drop_object(dst, ddata)?;
            let now = now_ns();
            s.hold(dst, sdata.object, (atime, now, now))
        })
    }

    /// Gives held file `ino` an object of its own before it is changed. When no other file shares
    /// its object any more, it simply takes the object's times back ([`Store::settle`]) and None is
    /// returned. Otherwise a new object is made, numbered by an inode row with no links that only
    /// holds it, and returned for the caller to copy into ([`Unshare`]); `keep` is the length a
    /// truncation is about to leave (None: all of it). Does nothing to a file that is not held.
    pub fn unshare(&self, ino: Ino, keep: Option<u64>) -> Result<Option<Unshare>, Errno> {
        let (st, data) = self.row(ino)?;
        let Some(atime) = data.held else {
            return Ok(None);
        };
        if self.users_of(data.object)? <= 1 {
            self.transaction(|s| s.settle(ino, data.object, (atime, st.mtime_ns)))?;
            return Ok(None);
        }
        let from = self.open_number(data.object, libc::O_RDONLY)?;
        let len = from.metadata().map_err(io_err)?.len();
        let (number, to) = self.transaction(|s| {
            let number = s.new_inode(mode::S_IFREG, 0, 0, 0, 0)?;
            Ok((number, s.new_object(number)?))
        })?;
        Ok(Some(Unshare {
            ino,
            from,
            to,
            len: keep.map_or(len, |k| k.min(len)),
            objects: (data.object, number),
        }))
    }

    /// The copy of [`Store::unshare`] is done: the file uses its new object (with the times it had
    /// in the database) and leaves the shared one to the others. The new object's data reaches
    /// disk before the commit that records it, as a replacing rename's does
    /// ([`Store::flush_replaced`]). EAGAIN when the file no longer uses the object it was copied
    /// from (it changed while the copy was made without the BKL): the new object goes, and the
    /// caller starts over.
    pub fn finish_unshare(&self, job: Unshare) -> Result<(), Errno> {
        let (from, to) = job.objects;
        let r = self.transaction(|s| {
            let (st, data) = s.row(job.ino)?;
            let (Some(atime), true) = (data.held, data.object == from) else {
                return Err(Errno::EAGAIN);
            };
            s.settle(job.ino, to, (atime, st.mtime_ns))?;
            if !s.object_used(from)? {
                s.doomed.borrow_mut().push(from);
            }
            s.replaced.borrow_mut().push(job.ino);
            Ok(())
        });
        r.inspect_err(|_| self.abandon_unshare(job))
    }

    /// The copy of [`Store::unshare`] failed: the new object goes (its row, which nothing uses,
    /// goes at the next boot).
    pub fn abandon_unshare(&self, job: Unshare) {
        let _ = self.remove_object(job.objects.1);
    }

    /// Gives held file `ino` an object of its own ([`Store::unshare`]), copying the data with the
    /// BKL held. `keep` as for unshare.
    pub fn make_private(&self, ino: Ino, keep: Option<u64>) -> Result<(), Errno> {
        loop {
            let Some(job) = self.unshare(ino, keep)? else {
                return Ok(());
            };
            if let Err(e) = copy_object(&job.from, &job.to, job.len) {
                self.abandon_unshare(job);
                return Err(e);
            }
            match self.finish_unshare(job) {
                Err(Errno::EAGAIN) => continue,
                r => return r,
            }
        }
    }

    /// Deleted regular file `ino` is not open any more: if it was held, it stops sharing its
    /// object, which goes when no other file uses it (otherwise the next boot does this).
    pub fn release(&self, ino: Ino) -> Result<(), Errno> {
        let (st, data) = self.row(ino)?;
        if st.nlink != 0 || st.kind() != mode::S_IFREG || !data.listed(ino) {
            return Ok(());
        }
        self.transaction(|s| s.drop_object(ino, data))
    }

    /// Regular file `ino` becomes held, using `object`, with times (atime, mtime, ctime).
    fn hold(
        &self,
        ino: Ino,
        object: Ino,
        (atime, mtime, ctime): (i64, i64, i64),
    ) -> Result<(), Errno> {
        self.db()
            .prepare_cached("UPDATE inodes SET mtime_ns = ?2, ctime_ns = ?3 WHERE ino = ?1")
            .and_then(|mut st| st.execute(params![ino, mtime, ctime]))
            .map_err(db_err)?;
        self.db()
            .prepare_cached(
                "INSERT OR REPLACE INTO shared (ino, object, atime_ns) VALUES (?1, ?2, ?3)",
            )
            .and_then(|mut st| st.execute(params![ino, object, atime]))
            .map_err(db_err)?;
        self.touch_ino(ino);
        Ok(())
    }

    /// Held file `ino`, from now on the only user of `object`, takes its times back: they are set
    /// on the object, whose ctime becomes now (the file is about to change anyway), and its row in
    /// `shared` goes when the object is the file's own.
    fn settle(&self, ino: Ino, object: Ino, (atime, mtime): (i64, i64)) -> Result<(), Errno> {
        let mut name = [0u8; 21];
        tuxport_hal::set_times_at(
            self.meta.objects_dir.as_raw_fd(),
            object_name(object, &mut name),
            Some(FileTime::At(atime).host()),
            Some(FileTime::At(mtime).host()),
        )
        .map_err(|e| object_err(object, e))?;
        let r = if object == ino {
            self.db()
                .prepare_cached("DELETE FROM shared WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![ino]))
        } else {
            self.db()
                .prepare_cached("UPDATE shared SET object = ?2, atime_ns = NULL WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![ino, object]))
        };
        r.map_err(db_err)?;
        self.touch_ino(ino);
        Ok(())
    }

    /// Regular file `ino` (described by `data`) stops using its object, which is deleted after the
    /// commit when no other file uses it.
    fn drop_object(&self, ino: Ino, data: Data) -> Result<(), Errno> {
        // A plain file's object is its own: no row uses its number (module invariant), and
        // deleting files need not query for it
        if data.listed(ino) {
            self.db()
                .prepare_cached("DELETE FROM shared WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![ino]))
                .map_err(db_err)?;
            self.touch_ino(ino);
            if self.object_used(data.object)? {
                return Ok(());
            }
        }
        self.doomed.borrow_mut().push(data.object);
        Ok(())
    }

    /// Whether some file in the `shared` table uses object `object`.
    fn object_used(&self, object: Ino) -> Result<bool, Errno> {
        Ok(self.users_of(object)? > 0)
    }

    /// How many files in the `shared` table use object `object`.
    fn users_of(&self, object: Ino) -> Result<u64, Errno> {
        self.db()
            .prepare_cached("SELECT count(*) FROM shared WHERE object = ?1")
            .and_then(|mut st| st.query_row(params![object], |r| r.get(0)))
            .map_err(db_err)
    }

    /// The host's times of object `object`.
    fn object_stat(&self, object: Ino) -> Result<Stat, Errno> {
        let mut st = Stat::default();
        fill_from_host(&mut st, &self.meta.objects_dir, Data { object, held: None })?;
        Ok(st)
    }

    /// Deletes an object; not an error if it is already gone.
    fn remove_object(&self, ino: Ino) -> Result<(), Errno> {
        remove_object(&self.meta.objects_dir, ino)
    }

    // ---- Database connection ----

    /// The database connection.
    #[inline]
    #[track_caller]
    fn db(&self) -> &Connection {
        self.check_owner();
        &self.conn
    }

    /// Executes a statement without parameters (BEGIN, COMMIT, savepoints), compiled only once.
    fn exec(&self, sql: &str) -> Result<(), Errno> {
        self.db()
            .prepare_cached(sql)
            .and_then(|mut st| st.execute([]))
            .map(drop)
            .map_err(db_err)
    }

    /// In checked builds: when it belongs to a running kernel, the current thread must hold that
    /// kernel's BKL ([`Store::guard_with_bkl`]).
    #[inline]
    #[track_caller]
    fn check_owner(&self) {
        #[cfg(any(debug_assertions, feature = "borrow-check"))]
        {
            let owner = self.owner.load(Ordering::Relaxed);
            assert!(
                owner == 0 || owner == crate::kernel::bkl_holder(),
                "the inode store of a running kernel is used without holding its BKL"
            );
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // The kernel is already gone (the last Arc<Shared> was dropped), so no other thread can
        // use it
        #[cfg(any(debug_assertions, feature = "borrow-check"))]
        self.owner.store(0, Ordering::Relaxed);
        if let Err(e) = self.commit() {
            crate::debug::log(format_args!("inode store: final commit failed: {e:?}"));
        }
        if let Some(t) = self.reaper.take() {
            let r = &self.meta.reaper;
            r.state.lock().unwrap_or_else(PoisonError::into_inner).quit = true;
            r.work.notify_one();
            let _ = t.join();
        }
        self.meta.commits.users().fetch_sub(1, Ordering::SeqCst);
    }
}

// ---- Operations ----

/// An operation in [`Store::transaction`]: when it does not finish normally (an error or a panic),
/// it rolls back to its savepoint and cancels the object deletions it queued. One without a
/// savepoint ([`Store::statement`]) only clears "inside an operation".
struct Op<'a> {
    store: &'a Store,
    savepoint: bool,
    doomed: usize,
    done: bool,
}

impl Op<'_> {
    fn finish(mut self, ok: bool) {
        self.done = ok;
    }
}

impl Drop for Op<'_> {
    fn drop(&mut self) {
        let s = self.store;
        s.writing.set(false);
        let fills = std::mem::take(&mut *s.fills.borrow_mut());
        if !self.done {
            if self.savepoint {
                let _ = s.exec("ROLLBACK TO op");
                let _ = s.exec("RELEASE op");
                s.doomed.borrow_mut().truncate(self.doomed);
            }
            return;
        }
        if !fills.is_empty() && s.cache_generation_any() {
            s.update(|c| {
                for f in fills {
                    c.entries += 1;
                    match f {
                        Fill::Name(dir, name, ino) => {
                            c.dentries.entry(dir).or_default().insert(name, Some(ino));
                        }
                        Fill::Row(st) => {
                            c.rows.insert(st.ino, (st, Data::own(st.ino)));
                        }
                    }
                }
            });
        }
    }
}

/// Dentries and inode rows created in an operation: their contents are known within the
/// operation, and they go straight into the cache after it succeeds, so chmod, chown or stat by
/// path right after creation need not query the database again. If the same operation changes
/// them later, they are invalidated when changed ([`Store::touch_name`]).
enum Fill {
    Name(Ino, Vec<u8>, Ino),
    Row(Stat),
}

impl Fill {
    /// At most this many per operation: importing a rootfs is one whole operation, and comparing
    /// them one by one on invalidation would become quadratic.
    const MAX: usize = 64;
}

// ---- State shared with readers and the reaper ----

/// The metadata cache, and what reading it without the BKL (`syscall::fast`) needs.
///
/// Only [`Store`]'s methods modify the cache, holding the write lock while they do; they read it
/// without locking, because other threads only read: Store is not Sync, so only one thread (the
/// one holding the BKL) calls its methods at a time. Threads without the BKL read only through
/// [`Meta::reader`], holding the read lock.
pub(crate) struct Meta {
    lock: RwLock<()>,
    cache: UnsafeCell<Cache>,
    /// The commit counter at the start of the `lock` file (module docs).
    commits: Commits,
    /// The `objects` directory (a host `O_PATH`), so stat-ing an object by inode number need not
    /// resolve the whole path.
    objects_dir: OwnedFd,
    /// Objects waiting for the background thread to delete them, and checkpoints to do
    /// ([`reap`]).
    reaper: Reaper,
    /// Another fd of the `lock` file in the store directory: syncfs needs an ordinary fd, and
    /// `objects_dir` is `O_PATH`.
    fs: File,
    /// The path of the WAL (`meta.db-wal`).
    wal: PathBuf,
    /// The commit counter's value before the WAL was last made durable, plus one: with no new
    /// commits since, there is no need to flush again ([`Meta::sync_metadata`]). 0 means it has
    /// never been flushed.
    durable: AtomicU64,
    /// The counts of making the WAL durable and of flushing renamed-over files before commits
    /// (for tests).
    #[cfg(test)]
    syncs: AtomicU64,
    #[cfg(test)]
    flushes: AtomicU64,
}

impl Meta {
    /// Makes committed metadata durable: commits only write frames into the WAL without flushing
    /// (`synchronous = NORMAL`), and this waits until the WAL is on disk. Frames a checkpoint
    /// wrote back are in the database file, which the checkpoint has already flushed; when the WAL
    /// is gone (written back and deleted when the last connection closed) there is nothing to
    /// flush either. May be called with the BKL released.
    pub(crate) fn sync_metadata(&self) -> Result<(), Errno> {
        let seen = self.commits.get();
        if self.durable.load(Ordering::Acquire) > seen {
            return Ok(());
        }
        match File::open(&self.wal) {
            Ok(f) => f.sync_data().map_err(io_err)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(e)),
        }
        #[cfg(test)]
        self.syncs.fetch_add(1, Ordering::Relaxed);
        self.durable.fetch_max(seen + 1, Ordering::AcqRel);
        Ok(())
    }

    /// Writes the whole host filesystem the store is on to disk (the guest's sync, syncfs): object
    /// data, the WAL and the database file together. May be called with the BKL released.
    pub(crate) fn sync_all(&self) -> Result<(), Errno> {
        let seen = self.commits.get();
        tuxport_hal::syncfs(self.fs.as_raw_fd()).map_err(Errno)?;
        self.durable.fetch_max(seen + 1, Ordering::AcqRel);
        Ok(())
    }
}

// SAFETY: the cache is synchronized by the convention above; the commit counter is an atomic in a
// shared mapping, and the mapping stays valid for as long as Meta lives
unsafe impl Send for Meta {}
unsafe impl Sync for Meta {}

impl Meta {
    /// Reads the cache without the BKL. When the cache is current (the commit counter has not
    /// changed), returns a reader, which holds the read lock until it is dropped.
    pub(crate) fn reader(&self) -> Option<Reader<'_>> {
        let guard = self.lock.read().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: the read lock is held, and a thread modifying the cache waits for its release
        let cache = unsafe { &*self.cache.get() };
        (cache.generation == self.commits.get()).then_some(Reader {
            _guard: guard,
            cache,
            objects_dir: &self.objects_dir,
        })
    }
}

/// A reader of the cache without the BKL ([`Meta::reader`]). A query that misses returns None, and
/// the caller then queries normally with the BKL held (which fills the cache).
pub(crate) struct Reader<'a> {
    _guard: RwLockReadGuard<'a, ()>,
    cache: &'a Cache,
    objects_dir: &'a OwnedFd,
}

impl Reader<'_> {
    /// [`Store::lookup`].
    pub fn lookup(&self, parent: Ino, name: &[u8]) -> Option<Option<Ino>> {
        self.cache.dentries.get(&parent)?.get(name).copied()
    }

    /// [`Store::kind`].
    pub fn kind(&self, ino: Ino) -> Option<u32> {
        Some(self.cache.rows.get(&ino)?.0.kind())
    }

    /// [`Store::readlink`]: None if it is not a symlink.
    pub fn readlink(&self, ino: Ino) -> Option<&[u8]> {
        self.cache.targets.get(&ino).map(Vec::as_slice)
    }

    /// [`Store::parent`].
    pub fn parent(&self, dir: Ino) -> Option<Ino> {
        self.cache.parents.get(&dir).copied()
    }

    /// [`Store::stat`]. Stats the object while holding the read lock: if the inode number is
    /// reused after deletion, the result is not half from the old file and half from the new one.
    /// None on a host error.
    pub fn stat(&self, ino: Ino) -> Option<Stat> {
        let (mut st, data) = *self.cache.rows.get(&ino)?;
        if st.kind() == mode::S_IFREG {
            fill_from_host(&mut st, self.objects_dir, data).ok()?;
        }
        Some(st)
    }
}

/// A regular file's size and times are taken from the host object file (for ctime see the module
/// docs), except that a held file keeps the times of its row.
fn fill_from_host(st: &mut Stat, objects_dir: &OwnedFd, data: Data) -> Result<(), Errno> {
    let mut name = [0u8; 21];
    let o = tuxport_hal::stat_at(objects_dir.as_raw_fd(), object_name(data.object, &mut name))
        .map_err(Errno)?;
    st.size = o.st_size as u64;
    st.blocks = Some(o.st_blocks as u64);
    if data.held.is_none() {
        st.mtime_ns = o.st_mtime * 1_000_000_000 + o.st_mtime_nsec;
        st.atime_ns = o.st_atime * 1_000_000_000 + o.st_atime_nsec;
        st.ctime_ns = o.st_ctime * 1_000_000_000 + o.st_ctime_nsec;
    }
    Ok(())
}

// ---- Cache entries and hashing ----

/// The cache's hasher: keys are inode numbers and file names, with no need to resist hash
/// collision attacks, so FxHash (as rustc does). The default SipHash took 4–5% of the stat path
/// of `ls` and `git status`.
#[derive(Default, Clone, Copy)]
struct FxHasher {
    hash: u64,
}

impl FxHasher {
    fn add(&mut self, word: u64) {
        const SEED: u64 = 0x517c_c1b7_2722_0a95;
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl std::hash::Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = [0u8; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word));
        }
    }

    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}

type Map<K, V> = HashMap<K, V, std::hash::BuildHasherDefault<FxHasher>>;

/// Where a regular file's data and times are (module docs): `object` is the number of the object
/// holding its data, its own unless it uses another's, and `held` its access time while it is held
/// (its other times are then the row's too).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Data {
    object: Ino,
    held: Option<i64>,
}

impl Data {
    /// Of a file in no row of `shared`, and of every other kind of inode.
    fn own(ino: Ino) -> Data {
        Data {
            object: ino,
            held: None,
        }
    }

    /// Whether inode `ino`, so described, has a row in `shared` (one that would only repeat
    /// [`Data::own`] is never kept).
    fn listed(&self, ino: Ino) -> bool {
        *self != Data::own(ino)
    }
}

/// The metadata cache. Its entries correspond to the database contents at commit count
/// `generation`.
#[derive(Default)]
struct Cache {
    generation: u64,
    entries: usize,
    /// Directory → name → inode (None means it does not exist).
    dentries: Map<Ino, Map<Vec<u8>, Option<Ino>>>,
    rows: Map<Ino, (Stat, Data)>,
    targets: Map<Ino, Vec<u8>>,
    parents: Map<Ino, Ino>,
    /// Directory → inode → one name in the directory pointing at it ([`Store::name_of`]; None
    /// means there is none).
    names: Map<Ino, Map<Ino, Option<Vec<u8>>>>,
    /// Complete listings of directories (readdir).
    listings: Map<Ino, Vec<DirEntry>>,
    /// Inodes with file capabilities (`security.capability`) ([`Store::has_file_caps`]); None
    /// means not looked up yet. Writing a file has to check it, so the whole set is cached
    /// together and invalidated with the commit count.
    file_caps: Option<Map<Ino, ()>>,
}

impl Cache {
    /// The limit on the number of entries: beyond it the whole cache is cleared.
    const MAX_ENTRIES: usize = 1 << 17;

    fn clear(&mut self, generation: u64) {
        *self = Cache {
            generation,
            ..Cache::default()
        };
    }
}

// ---- Counters in the `lock` file ----

/// A few words at the start of the `lock` file that all processes map shared: the number of
/// commits, the number of connections open on the store, and the range of inode numbers newly
/// allocated by write transactions not yet committed.
struct Commits {
    map: tuxport_hal::FileMapping,
}

impl Commits {
    fn open(lock: &File) -> Result<Commits, Errno> {
        if lock.metadata().map_err(io_err)?.len() < 32 {
            lock.set_len(32).map_err(io_err)?;
        }
        let map = tuxport_hal::map_file(lock.as_raw_fd(), 0, 32, true, true).map_err(Errno)?;
        Ok(Commits { map })
    }

    fn word(&self, i: usize) -> &AtomicU64 {
        // SAFETY: the mapping is page-aligned, at least 32 bytes, and valid for its whole
        // lifetime
        unsafe { AtomicU64::from_ptr(self.map.ptr.as_ptr().cast::<u64>().add(i)) }
    }

    fn get(&self) -> u64 {
        self.word(0).load(Ordering::SeqCst)
    }

    /// Adds one, returning the old value.
    fn bump(&self) -> u64 {
        self.word(0).fetch_add(1, Ordering::SeqCst)
    }

    /// The number of [`Store`] connections open on the store (one process may hold several: each
    /// host call opens its own besides the kernel's). A process that exited abnormally has not
    /// subtracted its connections, so it may be too high (which only makes other connections stop
    /// deferring commits); it is reset to zero when the store is opened exclusively (the
    /// equivalent of boot).
    fn users(&self) -> &AtomicU64 {
        self.word(1)
    }

    /// The minimum and maximum inode numbers newly allocated by write transactions not yet
    /// committed (0 means none). When a process is killed before committing, these inodes are lost
    /// with the transaction, but their objects have already been created; the next boot deletes
    /// them by this range. Otherwise, once the numbers were allocated again to inodes without
    /// objects, such as directories, the objects would stay forever.
    fn fresh(&self) -> (&AtomicU64, &AtomicU64) {
        (self.word(2), self.word(3))
    }
}

impl Drop for Commits {
    fn drop(&mut self) {
        // SAFETY: the mapping came from map_file and is not accessed afterwards
        unsafe { tuxport_hal::unmap(self.map.base, self.map.len) }
    }
}

// ---- Reaper and checkpoints ----

/// Objects to delete after a commit are handed to the background thread ([`reap`]): a host
/// unlink takes a few microseconds each and also takes the `objects` directory's lock; during
/// `rm -rf` it took over a tenth of the guest thread's time, and it was done holding the BKL. The
/// app being killed before the deletion does no harm: the inode row stays with nlink = 0 and is
/// cleaned up at boot. It also does the checkpoints ([`wal_hook`]).
#[derive(Default)]
struct Reaper {
    state: Mutex<Reaping>,
    /// There is work to do, or it is time to quit.
    work: Condvar,
    /// The queue is empty and nothing is being deleted (tests wait for this).
    idle: Condvar,
}

#[derive(Default)]
struct Reaping {
    queue: Vec<Ino>,
    /// The committing thread asks it for a checkpoint.
    checkpoint: bool,
    /// The number of pages in the WAL at the last checkpoint request: once done, the checkpoint
    /// has written back at least up to here.
    requested: c_int,
    /// The number of pages in the WAL after the latest commit, and how many commits there have
    /// been in total.
    pages: c_int,
    commits: u64,
    /// The WAL frame the last checkpoint wrote back up to.
    backfilled: c_int,
    busy: bool,
    quit: bool,
    /// How many have been handed to it in total (for tests).
    #[cfg(test)]
    handed: usize,
    /// How many checkpoints have been done (for tests).
    #[cfg(test)]
    checkpoints: usize,
    /// No checkpoints, simulating a background thread that cannot keep up (for tests).
    #[cfg(test)]
    paused: bool,
}

impl Reaper {
    /// After a commit the WAL has `pages` pages ([`wal_hook`]): once that is [`CHECKPOINT_PAGES`]
    /// pages more than at the last request, asks the background thread for a checkpoint.
    fn committed(&self, pages: c_int) {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        st.pages = pages;
        st.commits += 1;
        // Fewer than before: the WAL has been reused from the start
        if pages < st.requested {
            st.requested = 0;
        }
        if pages < st.backfilled {
            st.backfilled = 0;
        }
        if pages - st.requested >= CHECKPOINT_PAGES {
            st.requested = pages;
            st.checkpoint = true;
            self.work.notify_one();
        }
    }
}

/// The store's background thread, until the Store closes: deletes objects (finishing the queue
/// before closing) and does checkpoints.
fn reap(meta: Arc<Meta>, db: PathBuf) {
    let r = &meta.reaper;
    // Checkpoints use their own connection, opened the first time one is needed
    let mut conn = None;
    let mut st = r.state.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        if !st.queue.is_empty() {
            let inos = std::mem::take(&mut st.queue);
            st.busy = true;
            drop(st);
            // The commit that removed their last links goes to disk first: otherwise after power
            // loss the database returns to its earlier state, and the names are still there but
            // the objects are gone. Objects handed over during the flush accumulate for the next
            // round
            if let Err(e) = meta.sync_metadata() {
                crate::debug::log(format_args!("inode store: syncing the WAL: {e:?}"));
            }
            for ino in inos {
                let _ = remove_object(&meta.objects_dir, ino);
            }
            st = r.state.lock().unwrap_or_else(PoisonError::into_inner);
            st.busy = false;
            continue;
        }
        #[cfg(test)]
        let paused = st.paused;
        #[cfg(not(test))]
        let paused = false;
        if st.checkpoint && !paused {
            st.checkpoint = false;
            st.busy = true;
            drop(st);
            let backfilled = checkpoint(&mut conn, &db);
            st = r.state.lock().unwrap_or_else(PoisonError::into_inner);
            st.busy = false;
            if let Some(n) = backfilled {
                st.backfilled = n;
            }
            #[cfg(test)]
            {
                st.checkpoints += 1;
            }
            continue;
        }
        r.idle.notify_all();
        if st.quit {
            return;
        }
        // The WAL has grown past one checkpoint and has frames not written back: once writes
        // stop, write back the rest, so the next write transaction reuses the WAL from the start
        // ([`wal_hook`]). Sporadic small writes never get this long, and no fsync is spent on them
        if st.pages >= CHECKPOINT_PAGES && st.pages > st.backfilled {
            let seen = st.commits;
            let (g, wait) = r
                .work
                .wait_timeout(st, CHECKPOINT_QUIET)
                .unwrap_or_else(PoisonError::into_inner);
            st = g;
            if wait.timed_out() && st.commits == seen {
                st.checkpoint = true;
            }
            continue;
        }
        st = r.work.wait(st).unwrap_or_else(PoisonError::into_inner);
    }
}

/// Deletes an object in the `objects` directory; not an error if it is already gone.
fn remove_object(objects_dir: &OwnedFd, ino: Ino) -> Result<(), Errno> {
    let mut name = [0u8; 21];
    match tuxport_hal::unlink_at(objects_dir.as_raw_fd(), object_name(ino, &mut name)) {
        Err(e) if e != libc::ENOENT => Err(object_err(ino, e)),
        _ => Ok(()),
    }
}

/// A PASSIVE checkpoint: waits for no readers and blocks no writers; frames that cannot be written
/// back now are left for next time. Returns the frame it wrote back up to. Failures are only
/// logged; the WAL keeps growing, and at [`INLINE_CHECKPOINT_PAGES`] the committing thread
/// checkpoints in place.
fn checkpoint(conn: &mut Option<Connection>, db: &Path) -> Option<c_int> {
    if conn.is_none() {
        match Connection::open(db) {
            Ok(c) => *conn = Some(c),
            Err(e) => crate::debug::log(format_args!("inode store: checkpoint: {e}")),
        }
    }
    // The result is (whether it was blocked, the number of WAL frames, the frame written back up
    // to)
    conn.as_ref()?
        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get(2))
        .inspect_err(|e| crate::debug::log(format_args!("inode store: checkpoint: {e}")))
        .ok()
}

/// Replaces SQLite's default auto-checkpoint (`sqlite3_wal_hook`): called by the committing
/// thread after every commit, with `pages` the current number of pages in the WAL. A checkpoint
/// writes the WAL back into the database file and fsyncs twice, while commits mostly hold the BKL
/// (deferred commits that come due run in the timer thread); on a phone one fsync takes from a
/// few to tens of milliseconds, during which every guest thread has to wait, so the checkpoint is
/// handed to the background thread ([`reap`]).
///
/// The background checkpoint blocks no writers, but the WAL is reused from the start only by a
/// write transaction that begins after everything has been written back: with commits coming
/// all the time, the background thread is always the last few frames behind and the WAL keeps
/// growing. When writes stop (as they mostly do in interactive use) the background thread catches
/// up; when writes go on all the way to [`INLINE_CHECKPOINT_PAGES`] pages, the committing thread
/// finishes the job in place, by which time the background thread has written back most frames
/// and only those within the last thousand pages are left.
unsafe extern "C" fn wal_hook(
    meta: *mut c_void,
    db: *mut ffi::sqlite3,
    name: *const c_char,
    pages: c_int,
) -> c_int {
    // SAFETY: registration passed the Meta of the Store this connection belongs to, which
    // outlives the connection (Store::connect)
    unsafe { &*meta.cast::<Meta>() }.reaper.committed(pages);
    if pages >= INLINE_CHECKPOINT_PAGES {
        // SAFETY: SQLite passes the connection that just committed and the database's name
        unsafe { ffi::sqlite3_wal_checkpoint(db, name) };
    }
    ffi::SQLITE_OK
}

// ---- Errors, times and object names ----

fn db_err(e: rusqlite::Error) -> Errno {
    crate::debug::log(format_args!("inode store: {e}"));
    Errno::EIO
}

fn io_err(e: std::io::Error) -> Errno {
    crate::debug::log(format_args!("inode store: {e}"));
    e.raw_os_error().map(Errno).unwrap_or(Errno::EIO)
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// [`fill_from_host`] from an open object; `held` is the file's access time if it is held.
fn fill_from_object(st: &mut Stat, meta: &fs::Metadata, held: Option<i64>) {
    st.size = meta.len();
    st.blocks = Some(meta.blocks());
    if held.is_none() {
        st.mtime_ns = meta.modified().map(to_ns).unwrap_or(st.mtime_ns);
        st.atime_ns = meta.accessed().map(to_ns).unwrap_or(st.mtime_ns);
        st.ctime_ns = meta.ctime() * 1_000_000_000 + meta.ctime_nsec();
    }
}

/// Copies `len` bytes of object `from` into the empty object `to`, leaving holes where the host
/// reports them, and makes `to` that long ([`Unshare`], overlay copy-up).
pub(crate) fn copy_object(from: &File, to: &File, len: u64) -> Result<(), Errno> {
    const SEEK_DATA: i32 = 3;
    const SEEK_HOLE: i32 = 4;
    let (i, o) = (from.as_raw_fd(), to.as_raw_fd());
    let mut pos = 0;
    while pos < len {
        let data = match tuxport_hal::lseek(i, pos as i64, SEEK_DATA) {
            Ok(data) => data.min(len),
            Err(e) if e == Errno::ENXIO.0 => break,
            // A host that cannot tell has data everywhere
            Err(_) => pos,
        };
        let hole = tuxport_hal::lseek(i, data as i64, SEEK_HOLE)
            .unwrap_or(len)
            .min(len);
        let mut at = data;
        while at < hole {
            let chunk = (hole - at).min(1 << 30) as usize;
            match tuxport_hal::copy_file(i, at, o, at, chunk).map_err(Errno)? {
                0 => break,
                n => at += n as u64,
            }
        }
        pos = hole.max(data + 1);
    }
    to.set_len(len).map_err(io_err)
}

fn to_ns(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

/// An error operating on an object on the host (a host error code).
fn object_err(ino: Ino, e: i32) -> Errno {
    crate::debug::log(format_args!("inode store: object {ino}: errno {e}"));
    Errno(e)
}

/// An object's file name (the inode number in decimal), without allocating.
fn object_name(mut ino: Ino, buf: &mut [u8; 21]) -> &std::ffi::CStr {
    let mut i = buf.len() - 1;
    buf[i] = 0;
    loop {
        i -= 1;
        buf[i] = b'0' + (ino % 10) as u8;
        ino /= 10;
        if ino == 0 {
            return std::ffi::CStr::from_bytes_with_nul(&buf[i..]).unwrap();
        }
    }
}

/// Splits nanoseconds into (seconds, nanoseconds).
fn split_ns(ns: i64) -> (i64, i64) {
    (ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    impl Store {
        /// Waits for the background thread to finish deleting the objects handed to it.
        fn reaped(&self) {
            let r = &self.meta.reaper;
            let mut st = r.state.lock().unwrap();
            while !st.queue.is_empty() || st.busy {
                st = r.idle.wait(st).unwrap();
            }
        }

        /// Simulates the process being killed before committing: the deferred changes are lost,
        /// the objects waiting for deletion stay, and so does the range of newly allocated inode
        /// numbers in `lock`.
        fn crash(self) {
            let _ = self.exec("ROLLBACK");
            self.pending.set(None);
            self.doomed.borrow_mut().clear();
            self.replaced.borrow_mut().clear();
        }
    }

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tuxport-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store::create(&dir).unwrap();
        // Deferred write transactions are committed only when the test commits them itself,
        // regardless of how fast the machine is
        s.delay = Duration::from_secs(3600);
        (s, dir)
    }

    /// A store belonging to a running kernel ([`Store::guard_with_bkl`]) may be used only while
    /// holding that kernel's BKL: using it without holding it, or while holding another kernel's,
    /// is treated as a bug, caught without having to hit the race (the statement-cache panic of
    /// round 193).
    #[test]
    fn a_running_kernels_store_needs_its_bkl() {
        use crate::kernel::{Kernel, Shared};
        let (s, dir) = temp_store("bkl");
        let a = Shared::new(Kernel::new(crate::process::Tier::Interpreter));
        let b = Shared::new(Kernel::new(crate::process::Tier::Interpreter));
        let used = |s: &Store| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                s.lookup(ROOT, b"f").unwrap();
                s.readdir(ROOT).unwrap();
            }))
            .is_ok()
        };
        // Before it belongs to a kernel (before boot, or opened separately by the Files app),
        // anyone may use it
        assert!(used(&s));
        s.guard_with_bkl(a.id());
        assert!(!used(&s));
        {
            let _k = b.lock();
            assert!(!used(&s));
        }
        {
            let _k = a.lock();
            assert!(used(&s));
        }
        // No check on drop: the kernel is already gone, so no other thread can use it
        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_follows_own_and_other_processes_writes() {
        let (a, dir) = temp_store("cache");
        // A second connection stands in for another Tuxport process
        let b = Store::open(&dir, &[]).unwrap();
        let names = |s: &Store| -> Vec<Vec<u8>> {
            s.readdir(ROOT)
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect()
        };
        // Our own writes: "does not exist", attributes and directory listings in the cache are
        // updated accordingly
        assert_eq!(a.lookup(ROOT, b"f").unwrap(), None);
        assert!(names(&a).is_empty());
        let (f, _) = a.create_file(ROOT, b"f", 0o644, 0, 0).unwrap();
        assert_eq!(a.lookup(ROOT, b"f").unwrap(), Some(f));
        assert_eq!(names(&a), [b"f".to_vec()]);
        assert_eq!(a.stat(f).unwrap().mode & 0o777, 0o644);
        a.set_mode(f, 0o600).unwrap();
        assert_eq!(a.stat(f).unwrap().mode & 0o777, 0o600);
        a.rename(ROOT, b"f", ROOT, b"g", false, false).unwrap();
        assert_eq!(a.lookup(ROOT, b"f").unwrap(), None);
        assert_eq!(a.lookup(ROOT, b"g").unwrap(), Some(f));
        // Another process's writes: the counter changed, so the whole cache is invalidated
        assert_eq!(b.lookup(ROOT, b"g").unwrap(), Some(f));
        assert_eq!(b.stat(f).unwrap().nlink, 1);
        a.link(ROOT, b"h", f).unwrap();
        a.unlink(ROOT, b"g").unwrap();
        assert_eq!(b.lookup(ROOT, b"g").unwrap(), None);
        assert_eq!(b.lookup(ROOT, b"h").unwrap(), Some(f));
        assert_eq!(names(&b), [b"h".to_vec()]);
        let d = b.mkdir(ROOT, b"d", 0o755, 0, 0).unwrap();
        assert_eq!(a.lookup(ROOT, b"d").unwrap(), Some(d));
        assert_eq!(a.stat(ROOT).unwrap().nlink, 3);
        // Queries in a read transaction go through the cache too, and writes outside the read
        // transaction become visible afterwards
        a.snapshot(|s| {
            assert_eq!(s.lookup(ROOT, b"h")?, Some(f));
            assert_eq!(s.parent(d)?, ROOT);
            Ok(())
        })
        .unwrap();
        b.rmdir(ROOT, b"d").unwrap();
        a.snapshot(|s| {
            assert_eq!(s.lookup(ROOT, b"d")?, None);
            Ok(())
        })
        .unwrap();
        drop((a, b));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn namespace_round_trip() {
        let (s, dir) = temp_store("ns");
        let bin = s.mkdir(ROOT, b"bin", 0o755, 0, 0).unwrap();
        let (busybox, mut f) = s.create_file(bin, b"busybox", 0o4755, 0, 0).unwrap();
        f.write_all(b"\x7fELF").unwrap();
        let ls = s.symlink(bin, b"ls", b"/bin/busybox", 0, 0).unwrap();
        s.link(bin, b"bb", busybox).unwrap();

        assert_eq!(s.lookup(ROOT, b"bin").unwrap(), Some(bin));
        assert_eq!(s.lookup(bin, b"missing").unwrap(), None);
        assert_eq!(s.parent(bin).unwrap(), ROOT);
        let st = s.stat(busybox).unwrap();
        assert_eq!((st.mode, st.size, st.nlink), (mode::S_IFREG | 0o4755, 4, 2));
        assert!(s.stat(ls).unwrap().is_symlink());
        assert_eq!(s.readlink(ls).unwrap(), b"/bin/busybox");
        assert_eq!(s.stat(ROOT).unwrap().nlink, 3);
        let names: Vec<Vec<u8>> = s
            .readdir(bin)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            names,
            vec![b"bb".to_vec(), b"busybox".to_vec(), b"ls".to_vec()]
        );
        assert_eq!(s.mkdir(ROOT, b"bin", 0o755, 0, 0), Err(Errno::EEXIST));

        drop(s);
        let s = Store::open(&dir, &[]).unwrap();
        assert_eq!(s.lookup(bin, b"busybox").unwrap(), Some(busybox));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unlink_rmdir_and_rename() {
        let (s, dir) = temp_store("rename");
        let a = s.mkdir(ROOT, b"a", 0o755, 0, 0).unwrap();
        let b = s.mkdir(a, b"b", 0o755, 0, 0).unwrap();
        let (f, mut file) = s.create_file(a, b"f", 0o644, 0, 0).unwrap();
        file.write_all(b"data").unwrap();
        s.link(ROOT, b"g", f).unwrap();

        // A directory cannot be moved into its own subdirectory; a non-directory cannot replace a
        // directory
        assert_eq!(
            s.rename(ROOT, b"a", b, b"x", false, false),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            s.rename(ROOT, b"g", a, b"b", false, false),
            Err(Errno::EISDIR)
        );
        assert_eq!(
            s.rename(ROOT, b"g", a, b"f", true, false),
            Err(Errno::EEXIST)
        );
        // rename between two names of the same inode does nothing
        s.rename(ROOT, b"g", a, b"f", false, false).unwrap();
        assert_eq!(s.stat(f).unwrap().nlink, 2);

        // A directory gets a new parent: .. and the link counts on both sides must be updated
        s.rename(a, b"b", ROOT, b"b", false, false).unwrap();
        assert_eq!(s.parent(b).unwrap(), ROOT);
        assert_eq!(
            (s.stat(a).unwrap().nlink, s.stat(ROOT).unwrap().nlink),
            (2, 4)
        );
        s.rename(ROOT, b"b", a, b"f", false, true).unwrap();
        assert_eq!(s.lookup(a, b"f").unwrap(), Some(b));
        assert_eq!(s.lookup(ROOT, b"b").unwrap(), Some(f));

        // Removing the last link: the open file can still be read and fstat-ed; it is really
        // cleaned up only after the store is reopened
        assert_eq!(s.rmdir(ROOT, b"a"), Err(Errno::ENOTEMPTY));
        s.unlink(ROOT, b"g").unwrap();
        s.unlink(ROOT, b"b").unwrap();
        assert_eq!(s.lookup(ROOT, b"b").unwrap(), None);
        assert_eq!(s.stat_open(f, &file).unwrap().size, 4);
        s.rmdir(a, b"f").unwrap();
        s.rmdir(ROOT, b"a").unwrap();
        assert_eq!(s.stat(ROOT).unwrap().nlink, 2);
        assert_eq!(s.unlink(ROOT, b"a"), Err(Errno::ENOENT));
        drop(s);
        let s = Store::open(&dir, &[]).unwrap();
        assert_eq!(s.stat(f), Err(Errno::ENOENT));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_and_change_times_follow_modifications() {
        let (s, dir) = temp_store("times");
        let times = |ino: Ino| {
            let st = s.stat(ino).unwrap();
            (st.mtime_ns, st.ctime_ns)
        };
        // Change things with a short pause in between: the host's file times use a coarse clock
        let tick = || std::thread::sleep(Duration::from_millis(20));
        let newer = |ino: Ino, (m, c): (i64, i64), op: &mut dyn FnMut()| {
            tick();
            op();
            let (m2, c2) = times(ino);
            (m2 > m, c2 > c)
        };
        let a = s.mkdir(ROOT, b"a", 0o755, 0, 0).unwrap();
        let b = s.mkdir(ROOT, b"b", 0o755, 0, 0).unwrap();
        // Creating, renaming and deleting in a directory all change its mtime and ctime; a rename
        // changes the directories on both sides
        let mut created = None;
        let op = &mut || created = Some(s.create_file(a, b"f", 0o644, 0, 0).unwrap());
        assert_eq!(newer(a, times(a), op), (true, true));
        let (f, mut file) = created.unwrap();
        let tb = times(b);
        let op = &mut || s.rename(a, b"f", b, b"g", false, false).unwrap();
        assert_eq!(newer(a, times(a), op), (true, true));
        assert!(times(b).0 > tb.0 && times(b).1 > tb.1);
        let op = &mut || s.link(b, b"h", f).unwrap();
        assert_eq!(newer(b, times(b), op), (true, true));
        let op = &mut || s.unlink(b, b"h").unwrap();
        assert_eq!(newer(b, times(b), op), (true, true));
        let op = &mut || s.rmdir(ROOT, b"a").unwrap();
        assert_eq!(newer(ROOT, times(ROOT), op), (true, true));
        // Regular file: write and truncate change the object, and the ctime follows; chmod changes
        // only the ctime
        let op = &mut || file.write_all(b"data").unwrap();
        assert_eq!(newer(f, times(f), op), (true, true));
        let op = &mut || s.truncate(f, 1).unwrap();
        assert_eq!(newer(f, times(f), op), (true, true));
        let op = &mut || s.set_mode(f, 0o600).unwrap();
        assert_eq!(newer(f, times(f), op), (false, true));
        let op = &mut || s.rename(b, b"g", b, b"g2", false, false).unwrap();
        assert_eq!(newer(f, times(f), op), (false, true));
        let op = &mut || s.link(b, b"h", f).unwrap();
        assert_eq!(newer(f, times(f), op), (false, true));
        // utimensat sets an earlier time, and the ctime is now; for a directory, giving only the
        // access time also changes the ctime
        let before = times(f);
        tick();
        s.set_times(f, Some(FileTime::At(1)), Some(FileTime::At(2_000_000_000)))
            .unwrap();
        assert_eq!(times(f).0, 2_000_000_000);
        assert!(times(f).1 > before.1);
        let before = times(b);
        tick();
        s.set_times(b, None, Some(FileTime::At(3_000_000_000)))
            .unwrap();
        assert_eq!(times(b).0, 3_000_000_000);
        assert!(times(b).1 > before.1);
        let op = &mut || s.set_times(b, Some(FileTime::Now), None).unwrap();
        assert_eq!(newer(b, times(b), op), (false, true));
        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleted_directories_take_no_new_entries() {
        let (s, dir) = temp_store("deaddir");
        let d = s.mkdir(ROOT, b"d", 0o755, 0, 0).unwrap();
        let (f, _) = s.create_file(ROOT, b"f", 0o644, 0, 0).unwrap();
        s.rmdir(ROOT, b"d").unwrap();
        // After deletion it may still be a cwd: creating, linking and renaming into it are all
        // ENOENT
        assert_eq!(
            s.create_file(d, b"x", 0o644, 0, 0).err(),
            Some(Errno::ENOENT)
        );
        assert_eq!(s.mkdir(d, b"x", 0o755, 0, 0), Err(Errno::ENOENT));
        assert_eq!(s.symlink(d, b"x", b"f", 0, 0), Err(Errno::ENOENT));
        assert_eq!(s.link(d, b"x", f), Err(Errno::ENOENT));
        assert_eq!(
            s.rename(ROOT, b"f", d, b"x", false, false),
            Err(Errno::ENOENT)
        );
        assert_eq!(s.lookup(ROOT, b"f").unwrap(), Some(f));
        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_and_chown_remove_privileges() {
        let (s, dir) = temp_store("privs");
        let caps = [0x0200_0001u32, 1 << 13, 0, 0, 0];
        let caps: Vec<u8> = caps.iter().flat_map(|w| w.to_le_bytes()).collect();
        let (f, _) = s.create_file(ROOT, b"ping", 0o6755, 0, 0).unwrap();
        assert!(!s.has_file_caps(f).unwrap());
        s.set_xattr(f, XATTR_NAME_CAPS, &caps, false, false)
            .unwrap();
        // What another process sets is visible too: the cache is invalidated with the commit count
        s.commit().unwrap();
        let other = Store::open(&dir, &[]).unwrap();
        assert!(other.has_file_caps(f).unwrap());
        // A write with CAP_FSETID: only the file capabilities are removed
        other.remove_privs(f, true).unwrap();
        other.commit().unwrap();
        assert!(!s.has_file_caps(f).unwrap());
        assert_eq!(s.attrs(f).unwrap().mode & 0o7777, 0o6755);
        // Without CAP_FSETID: setuid and (when group-executable) setgid are removed
        s.remove_privs(f, false).unwrap();
        assert_eq!(s.attrs(f).unwrap().mode & 0o7777, 0o755);
        let (g, _) = s.create_file(ROOT, b"lock", 0o2644, 0, 0).unwrap();
        s.remove_privs(g, false).unwrap();
        assert_eq!(s.attrs(g).unwrap().mode & 0o7777, 0o2644);
        // chown also removes file capabilities
        s.set_xattr(f, XATTR_NAME_CAPS, &caps, false, false)
            .unwrap();
        assert!(s.has_file_caps(f).unwrap());
        s.set_owner(f, None, Some(0), true).unwrap();
        assert!(!s.has_file_caps(f).unwrap());
        assert_eq!(s.get_xattr(f, XATTR_NAME_CAPS), Err(Errno::ENODATA));
        drop((other, s));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn xattrs_and_anonymous_files() {
        let (s, dir) = temp_store("xattr");
        let (f, _) = s.create_file(ROOT, b"f", 0o644, 0, 0).unwrap();
        assert_eq!(s.get_xattr(f, b"user.a"), Err(Errno::ENODATA));
        s.set_xattr(f, b"user.b", b"2", false, false).unwrap();
        s.set_xattr(f, b"user.a", b"1", true, false).unwrap();
        assert_eq!(
            s.set_xattr(f, b"user.a", b"x", true, false),
            Err(Errno::EEXIST)
        );
        assert_eq!(
            s.set_xattr(f, b"user.c", b"x", false, true),
            Err(Errno::ENODATA)
        );
        s.set_xattr(f, b"user.a", b"one", false, true).unwrap();
        assert_eq!(s.get_xattr(f, b"user.a").unwrap(), b"one");
        assert_eq!(
            s.list_xattr(f).unwrap(),
            vec![b"user.a".to_vec(), b"user.b".to_vec()]
        );
        s.remove_xattr(f, b"user.b").unwrap();
        assert_eq!(s.remove_xattr(f, b"user.b"), Err(Errno::ENODATA));

        let (anon, mut file) = s.create_anonymous(0o777, 1000, 1001).unwrap();
        file.write_all(b"data").unwrap();
        let st = s.stat_open(anon, &file).unwrap();
        assert_eq!((st.nlink, st.size, st.uid, st.gid), (0, 4, 1000, 1001));
        // An inode without names gets none again (its object is gone)
        assert_eq!(s.link(ROOT, b"anon", anon), Err(Errno::ENOENT));
        s.set_xattr(anon, b"user.x", b"1", false, false).unwrap();
        // The timer thread commits when it is due; without the commit, opening the store below
        // would wait until the timeout
        s.commit().unwrap();
        // No cleanup while another process is still using the store: it may still have this inode
        // open
        let other = Store::open(&dir, &[]).unwrap();
        assert!(other.stat_row(anon).is_ok());
        drop(other);
        // Opening the store exclusively cleans up the nameless inode and its extended attributes
        drop((file, s));
        let s = Store::open(&dir, &[]).unwrap();
        assert!(s.stat_row(anon).is_err());
        assert_eq!(s.list_xattr(anon).unwrap(), Vec::<Vec<u8>>::new());
        assert_eq!(s.get_xattr(f, b"user.a").unwrap(), b"one");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn volatile_directories_are_emptied_on_first_open() {
        let (s, dir) = temp_store("volatile");
        let dev = s.mkdir(ROOT, b"dev", 0o755, 0, 0).unwrap();
        let shm = s.mkdir(dev, b"shm", 0o1777, 0, 0).unwrap();
        s.create_file(shm, b"seg", 0o600, 0, 0).unwrap();
        let run = s.mkdir(ROOT, b"run", 0o755, 0, 0).unwrap();
        let sub = s.mkdir(run, b"sub", 0o755, 0, 0).unwrap();
        s.create_file(sub, b"pid", 0o644, 0, 0).unwrap();
        s.symlink(ROOT, b"tmp", b"run", 0, 0).unwrap();
        let volatile: &[&[u8]] = &[b"dev/shm", b"run", b"tmp", b"missing/dir"];
        s.commit().unwrap();
        // Another process is still using the store: not a boot, so nothing is deleted
        let other = Store::open(&dir, volatile).unwrap();
        assert!(other.lookup(shm, b"seg").unwrap().is_some());
        drop((other, s));
        let s = Store::open(&dir, volatile).unwrap();
        assert!(s.readdir(shm).unwrap().is_empty());
        assert!(s.readdir(run).unwrap().is_empty());
        assert_eq!(s.stat(shm).unwrap().mode & 0o7777, 0o1777);
        assert!(s.lookup(ROOT, b"tmp").unwrap().is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detached_directories_are_removed() {
        let (s, dir) = temp_store("detached");
        // Not under the root directory: its parent is itself and it is invisible from the root
        // directory; files and directories are created in it as usual
        let a = s.create_detached_dir(0o1777, 0, 0).unwrap();
        assert_eq!(s.parent(a).unwrap(), a);
        assert_eq!(s.stat(a).unwrap().mode, mode::S_IFDIR | 0o1777);
        assert!(s.readdir(ROOT).unwrap().is_empty());
        let sub = s.mkdir(a, b"sub", 0o755, 0, 0).unwrap();
        let (f, _) = s.create_file(sub, b"f", 0o644, 0, 0).unwrap();
        // Moved under another subdirectory: the ancestor check stops at it
        let other = s.mkdir(a, b"other", 0o755, 0, 0).unwrap();
        s.rename(a, b"sub", other, b"sub", false, false).unwrap();
        assert_eq!(
            s.rename(other, b"sub", sub, b"x", false, false),
            Err(Errno::EINVAL)
        );
        // Unmounted: deleted along with everything in it
        s.remove_detached_dir(a).unwrap();
        assert_eq!(s.stat(f).unwrap().nlink, 0);
        assert_eq!(s.stat(a).unwrap().nlink, 0);
        // The next boot deletes what was left last time
        let b = s.create_detached_dir(0o755, 0, 0).unwrap();
        let (g, _) = s.create_file(b, b"g", 0o644, 0, 0).unwrap();
        s.commit().unwrap();
        drop(s);
        let s = Store::open(&dir, &[]).unwrap();
        assert_eq!(s.stat(b), Err(Errno::ENOENT));
        assert_eq!(s.stat(g), Err(Errno::ENOENT));
        assert_eq!(s.stat(f), Err(Errno::ENOENT));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_connections_wait_for_each_other() {
        // Several Tuxport processes using one store at once: writes wait for others' transactions
        // to end and do not fail
        let (s, dir) = temp_store("concurrent");
        let dirs: Vec<Ino> = (0..4)
            .map(|i| {
                s.mkdir(ROOT, format!("d{i}").as_bytes(), 0o755, 0, 0)
                    .unwrap()
            })
            .collect();
        drop(s);
        let workers: Vec<_> = dirs
            .into_iter()
            .map(|d| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let s = Store::open(&dir, &[]).unwrap();
                    for i in 0..100 {
                        let name = format!("f{i}");
                        s.create_file(d, name.as_bytes(), 0o644, 0, 0).unwrap();
                        s.rename(d, name.as_bytes(), d, b"g", false, false).unwrap();
                        s.unlink(d, b"g").unwrap();
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deferred_commits() {
        let (s, dir) = temp_store("defer");
        let (f, _) = s.create_file(ROOT, b"f", 0o644, 0, 0).unwrap();
        // Other connections (reading the database directly) do not see it before the commit
        let raw = Connection::open(dir.join("meta.db")).unwrap();
        let count = |name: &[u8]| -> i64 {
            raw.query_row(
                "SELECT count(*) FROM dentries WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(count(b"f"), 0);
        // A failed operation rolls back only itself: creating a symlink under a regular file
        // finds out that the parent is not a directory only after inserting the inode
        assert_eq!(s.symlink(f, b"x", b"t", 0, 0), Err(Errno::ENOTDIR));
        let max: Ino = s
            .db()
            .query_row("SELECT max(ino) FROM inodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(max, f);
        assert_eq!(s.lookup(ROOT, b"f").unwrap(), Some(f));
        s.commit().unwrap();
        assert_eq!(count(b"f"), 1);
        // After the last link is removed, the object is deleted only at commit
        let object = dir.join("objects").join(f.to_string());
        s.unlink(ROOT, b"f").unwrap();
        assert!(object.exists());
        s.commit().unwrap();
        s.reaped();
        assert!(!object.exists());
        assert_eq!(count(b"f"), 0);
        drop((raw, s));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn inode_numbers_reused_after_boot_keep_their_objects() {
        let (s, dir) = temp_store("reuse");
        // Deleted last time, still in a volatile directory, and left in a tmpfs tree that was
        // never unmounted: all three kinds are deleted by the next boot
        let tmpfs = s.create_detached_dir(0o1777, 0, 0).unwrap();
        let run = s.mkdir(ROOT, b"run", 0o755, 0, 0).unwrap();
        let mut inos = Vec::new();
        for i in 0..30 {
            let name = format!("f{i}");
            let (ino, _) = s.create_file(ROOT, name.as_bytes(), 0o644, 0, 0).unwrap();
            s.unlink(ROOT, name.as_bytes()).unwrap();
            inos.push(ino);
            let (ino, _) = s.create_file(run, name.as_bytes(), 0o644, 0, 0).unwrap();
            inos.push(ino);
            let (ino, _) = s.create_file(tmpfs, name.as_bytes(), 0o644, 0, 0).unwrap();
            inos.push(ino);
        }
        s.commit().unwrap();
        drop(s);
        // Boot cleanup deletes these inode rows, and the numbers are allocated to newly created
        // files again. Cleanup deletes the objects itself instead of handing them to the
        // background thread: if that deleted them a bit later, it would delete the new files'
        let s = Store::open(&dir, &[b"run"]).unwrap();
        assert_eq!(s.meta.reaper.state.lock().unwrap().handed, 0);
        for i in 0..inos.len() {
            let (g, _) = s
                .create_file(ROOT, format!("g{i}").as_bytes(), 0o644, 0, 0)
                .unwrap();
            s.open_object(g, true).unwrap().write_all(b"data").unwrap();
        }
        s.commit().unwrap();
        s.reaped();
        for i in 0..inos.len() {
            let ino = s.lookup(ROOT, format!("g{i}").as_bytes()).unwrap().unwrap();
            assert!(inos.contains(&ino));
            let object = dir.join("objects").join(ino.to_string());
            assert_eq!(fs::read(object).unwrap(), b"data");
        }
        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoints_run_in_the_background() {
        let (s, dir) = temp_store("checkpoint");
        let state = |s: &Store| {
            let st = s.meta.reaper.state.lock().unwrap();
            (st.checkpoints, st.pages, st.backfilled)
        };
        // Each operation commits on its own, until the WAL has accumulated enough pages for a
        // checkpoint. If the committing thread did it itself (SQLite's default auto-checkpoint),
        // the background thread would never have done one
        let mut i = 0;
        while state(&s).0 == 0 {
            assert!(i < 5000, "no checkpoint after {i} commits");
            s.create_file(ROOT, format!("f{i}").as_bytes(), 0o644, 0, 0)
                .unwrap();
            s.commit().unwrap();
            i += 1;
        }
        // Once writes stop, the background thread writes back the remaining frames: the next
        // write transaction reuses the WAL from the start. Waiting only for the second checkpoint
        // is not enough: on a slow machine the second may be the one requested during the last
        // few commits, which cannot write those back; the one after writes stop is needed
        let start = Instant::now();
        while {
            let (_, pages, backfilled) = state(&s);
            backfilled < pages
        } {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "no checkpoint after the writes stopped"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        s.reaped();
        let f = s.lookup(ROOT, b"f0").unwrap().unwrap();
        s.set_mode(f, 0o600).unwrap();
        s.commit().unwrap();
        assert!(
            state(&s).1 < CHECKPOINT_PAGES,
            "WAL has {} pages",
            state(&s).1
        );
        // Writing all the time, with the background thread unable to keep up: at
        // INLINE_CHECKPOINT_PAGES pages the committing thread catches up, and the WAL is reused
        // from the start afterwards as well
        s.meta.reaper.state.lock().unwrap().paused = true;
        let (mut most, mut restarted) = (0, false);
        for k in 0..1500 {
            let value = vec![k as u8; 60 << 10];
            s.set_xattr(f, b"user.big", &value, false, false).unwrap();
            s.commit().unwrap();
            let pages = state(&s).1;
            restarted |= most >= INLINE_CHECKPOINT_PAGES && pages < CHECKPOINT_PAGES;
            most = most.max(pages);
        }
        assert!(restarted, "WAL grew to {most} pages and never restarted");
        assert!(
            most < INLINE_CHECKPOINT_PAGES + 100,
            "WAL grew to {most} pages"
        );
        s.meta.reaper.state.lock().unwrap().paused = false;
        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn killed_before_commit() {
        let (s, dir) = temp_store("killed");
        let (g, _) = s.create_file(ROOT, b"g", 0o644, 0, 0).unwrap();
        s.commit().unwrap();
        // Killed without committing: the deletion did not take effect, the newly created file is
        // lost with it, and its object is deleted at boot
        s.unlink(ROOT, b"g").unwrap();
        let (f, _) = s.create_file(ROOT, b"f", 0o644, 0, 0).unwrap();
        let object = |ino: Ino| dir.join("objects").join(ino.to_string());
        assert!(object(f).exists());
        s.crash();
        let s = Store::open(&dir, &[]).unwrap();
        assert_eq!(s.lookup(ROOT, b"g").unwrap(), Some(g));
        assert!(object(g).exists());
        assert_eq!(s.lookup(ROOT, b"f").unwrap(), None);
        assert!(!object(f).exists());
        // The number is allocated again
        assert_eq!(s.mkdir(ROOT, b"d", 0o755, 0, 0).unwrap(), f);
        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Ordering for power loss (module docs): a regular file renamed over another file is flushed
    /// before the commit, while renames to a new name and symlinks are not; the WAL goes to disk
    /// before objects are deleted, at boot cleanup too; with no new commits the WAL is not flushed
    /// again.
    #[test]
    fn power_loss_ordering() {
        let (s, dir) = temp_store("ordering");
        let flushes = || s.meta.flushes.load(Ordering::Relaxed);
        let syncs = || s.meta.syncs.load(Ordering::Relaxed);
        let (a, mut fa) = s.create_file(ROOT, b"a", 0o644, 0, 0).unwrap();
        fa.write_all(b"old").unwrap();
        let (b, mut fb) = s.create_file(ROOT, b"b.tmp", 0o644, 0, 0).unwrap();
        fb.write_all(b"new").unwrap();
        s.rename(ROOT, b"b.tmp", ROOT, b"c", false, false).unwrap();
        s.symlink(ROOT, b"l", b"c", 0, 0).unwrap();
        s.rename(ROOT, b"l", ROOT, b"c2", false, false).unwrap();
        s.commit().unwrap();
        assert_eq!(flushes(), 0);
        // Replacing a: flushed only at commit
        s.rename(ROOT, b"c", ROOT, b"a", false, false).unwrap();
        assert_eq!(flushes(), 0);
        assert_eq!(*s.replaced.borrow(), [b]);
        // A symlink replacing a regular file is not flushed
        s.create_file(ROOT, b"d", 0o644, 0, 0).unwrap();
        s.rename(ROOT, b"c2", ROOT, b"d", false, false).unwrap();
        s.commit().unwrap();
        assert_eq!(flushes(), 1);
        assert!(s.replaced.borrow().is_empty());
        // The object of the replaced a is deleted only after the WAL is on disk
        s.reaped();
        assert!(!dir.join("objects").join(a.to_string()).exists());
        let after_reap = syncs();
        assert!(after_reap >= 1);
        // No new commits: no further flush
        s.meta.sync_metadata().unwrap();
        assert_eq!(syncs(), after_reap);
        s.set_mode(b, 0o600).unwrap();
        s.commit().unwrap();
        s.meta.sync_metadata().unwrap();
        assert_eq!(syncs(), after_reap + 1);
        drop((fa, fb, s));
        // At boot it also flushes before deleting objects according to the database (a's row
        // still stays with nlink = 0)
        let s = Store::open(&dir, &[]).unwrap();
        assert_eq!(s.meta.syncs.load(Ordering::Relaxed), 1);
        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Files sharing an object ([`Store::share`]) read the same data from it and keep their own
    /// times in the database; the first change of one gives it an object of its own with those
    /// times (a copy while others still share it, flushed before the commit; only what a
    /// truncation keeps), and the last one takes the object back as its own.
    #[test]
    fn shared_objects_until_written() {
        use std::io::Read;
        use std::os::unix::fs::FileExt;
        let (s, dir) = temp_store("share");
        let objects = || fs::read_dir(dir.join("objects")).unwrap().count();
        let read = |s: &Store, ino| {
            let mut v = Vec::new();
            s.open_object(ino, false)
                .unwrap()
                .read_to_end(&mut v)
                .unwrap();
            v
        };
        let at = FileTime::At;
        let (a, mut fa) = s.create_file(ROOT, b"a", 0o644, 0, 0).unwrap();
        fa.write_all(b"shared data").unwrap();
        s.set_times(a, Some(at(1_000_000_000)), Some(at(2_000_000_000)))
            .unwrap();
        let (b, _) = s.create_file(ROOT, b"b", 0o600, 0, 0).unwrap();
        let (c, _) = s.create_file(ROOT, b"c", 0o600, 0, 0).unwrap();
        s.share(a, b).unwrap();
        s.share(b, c).unwrap();
        s.commit().unwrap();
        s.reaped();
        // b's and c's own objects are gone, and all three read a's
        assert_eq!(objects(), 1);
        for ino in [a, b, c] {
            assert!(s.held(ino).unwrap());
            assert_eq!(read(&s, ino), b"shared data");
            assert_eq!(s.stat(ino).unwrap().size, 11);
        }
        // a keeps its times; a clone's modification time is now
        let st = s.stat(a).unwrap();
        assert_eq!((st.atime_ns, st.mtime_ns), (1_000_000_000, 2_000_000_000));
        assert!(s.stat(b).unwrap().mtime_ns > 2_000_000_000);
        s.set_times(b, Some(at(5_000_000_000)), Some(at(6_000_000_000)))
            .unwrap();
        let st = s.stat(b).unwrap();
        assert_eq!((st.atime_ns, st.mtime_ns), (5_000_000_000, 6_000_000_000));
        assert_eq!(s.stat(a).unwrap().mtime_ns, 2_000_000_000);
        // Writing b: a copy of its own, with its times, on disk before the commit recording it
        let flushes = s.meta.flushes.load(Ordering::Relaxed);
        s.make_private(b, None).unwrap();
        assert!(!s.held(b).unwrap());
        let st = s.stat(b).unwrap();
        assert_eq!(
            (st.atime_ns, st.mtime_ns, st.size),
            (5_000_000_000, 6_000_000_000, 11)
        );
        s.open_object(b, true)
            .unwrap()
            .write_all_at(b"B", 0)
            .unwrap();
        assert_eq!(read(&s, b), b"Bhared data");
        assert_eq!(read(&s, a), b"shared data");
        s.commit().unwrap();
        assert_eq!(s.meta.flushes.load(Ordering::Relaxed), flushes + 1);
        // Truncating c copies only what it keeps
        s.truncate(c, 6).unwrap();
        assert_eq!(read(&s, c), b"shared");
        assert_eq!(read(&s, a), b"shared data");
        // a, the last one, takes the object back with its times, without a copy
        s.make_private(a, None).unwrap();
        assert_eq!(s.data(a).unwrap(), Data::own(a));
        let st = s.stat(a).unwrap();
        assert_eq!((st.atime_ns, st.mtime_ns), (1_000_000_000, 2_000_000_000));
        s.commit().unwrap();
        s.reaped();
        assert_eq!(objects(), 3);
        drop((fa, s));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A deleted file that shared an object keeps using it while it may still be open, until it
    /// is released or the next boot; numbers other files use as objects keep their rows at boot,
    /// so no new file gets them, and objects nothing uses go.
    #[test]
    fn shared_objects_outlive_their_first_file() {
        let (s, dir) = temp_store("share-boot");
        let (b, _) = s.create_file(ROOT, b"b", 0o644, 0, 0).unwrap();
        let (c, _) = s.create_file(ROOT, b"c", 0o644, 0, 0).unwrap();
        let (a, mut fa) = s.create_file(ROOT, b"a", 0o644, 0, 0).unwrap();
        fa.write_all(b"data").unwrap();
        s.share(a, b).unwrap();
        s.share(a, c).unwrap();
        s.unlink(ROOT, b"a").unwrap();
        s.release(a).unwrap();
        // c is deleted but not released: still open somewhere
        s.unlink(ROOT, b"c").unwrap();
        s.commit().unwrap();
        s.reaped();
        let object = dir.join("objects").join(a.to_string());
        assert_eq!(fs::read(&object).unwrap(), b"data");
        drop((fa, s));
        let s = Store::open(&dir, &[]).unwrap();
        assert_eq!(fs::read(&object).unwrap(), b"data");
        assert!(s.query_row(a).is_ok());
        assert_eq!(s.query_row(c).err(), Some(Errno::ENOENT));
        let (d, _) = s.create_file(ROOT, b"d", 0o644, 0, 0).unwrap();
        assert_ne!(d, a);
        // b, the last user, goes: so does the object, and a's row at the next boot
        s.unlink(ROOT, b"b").unwrap();
        s.release(b).unwrap();
        s.commit().unwrap();
        s.reaped();
        assert!(!object.exists());
        drop(s);
        let s = Store::open(&dir, &[]).unwrap();
        assert_eq!(s.query_row(a).err(), Some(Errno::ENOENT));
        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }

    /// An object copied for a file that stops sharing is lost with an uncommitted transaction
    /// like any new object: the next boot deletes it, and the file still reads the shared one.
    #[test]
    fn unshared_copies_lost_before_commit_are_deleted() {
        let (s, dir) = temp_store("share-crash");
        let (a, mut fa) = s.create_file(ROOT, b"a", 0o644, 0, 0).unwrap();
        fa.write_all(b"data").unwrap();
        let (b, _) = s.create_file(ROOT, b"b", 0o644, 0, 0).unwrap();
        s.share(a, b).unwrap();
        s.commit().unwrap();
        s.make_private(b, None).unwrap();
        let copy = s.data(b).unwrap().object;
        assert_ne!(copy, a);
        assert!(dir.join("objects").join(copy.to_string()).exists());
        s.crash();
        drop(fa);
        let s = Store::open(&dir, &[]).unwrap();
        assert!(!dir.join("objects").join(copy.to_string()).exists());
        assert!(s.held(b).unwrap());
        assert_eq!(s.data(b).unwrap().object, a);
        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }
}
