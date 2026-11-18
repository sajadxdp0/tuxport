//! In-process pipes (the kernel/pipe map): pipe2's anonymous pipes and the store's named pipes
//! (FIFOs; all opens of one inode share one pipe object).
//!
//! The buffer size defaults to 64 KiB as on Linux and can be changed with `F_SETPIPE_SZ`; writes
//! of at most `PIPE_BUF` are atomic.
//! The read and write ends each count their open files: after the last write end closes, the read
//! end reads EOF; after the last read end closes, writes fail and raise SIGPIPE. Blocking and
//! waking go through the common wait mechanism of [`crate::kernel`].

use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cell::RefCell;
use crate::kernel::request_wake_object;
use crate::vfs::store::Ino;

/// The default capacity of a pipe.
pub const PIPE_CAPACITY: usize = 64 * 1024;
/// The default of `/proc/sys/fs/pipe-max-size`: the maximum pipe capacity without
/// `CAP_SYS_RESOURCE`.
pub const PIPE_MAX_SIZE: usize = 1024 * 1024;
/// Writes up to this length are never interleaved with other writes.
pub const PIPE_BUF: usize = 4096;

/// A pipe.
pub struct Pipe {
    /// Buffered data.
    pub buf: VecDeque<u8>,
    /// Capacity.
    pub capacity: usize,
    /// Number of open read ends.
    pub readers: usize,
    /// Number of open write ends.
    pub writers: usize,
    /// The inode number fstat reports.
    pub ino: u64,
    /// A named pipe's inode in the store; None for anonymous pipes.
    pub fifo: Option<Ino>,
    /// How many times the read and write ends have been opened: opening a FIFO waits for the other
    /// end, and these show that the other end has come (even if it has closed again).
    pub reader_opens: u64,
    pub writer_opens: u64,
    /// The least number of bytes the write ends waiting for space need (0 means none): they are
    /// woken only when the readers free that much ([`Pipe::drained`]).
    pub writer_need: usize,
    /// Number of writes of data: as on Linux, every write makes the read end's `EPOLLET` fire
    /// again.
    pub writes: u64,
    /// Number of reads that made the pipe writable again (at least `PIPE_BUF` free, where poll
    /// reports `POLLOUT`): each makes the write end's `EPOLLET` fire again, as Linux's pipe_read
    /// wakes the writers when it frees a full pipe, so a writer that wrote until a short count
    /// learns of the room.
    pub room: u64,
    /// A splice is writing out data it takes from the pipe only once written: every other
    /// consumer waits until it is done, as Linux's hold the pipe's lock across the write, so the
    /// bytes it then removes are still the ones it wrote.
    pub splicing: bool,
}

impl Pipe {
    fn new(ino: u64, fifo: Option<Ino>, capacity: usize) -> Rc<RefCell<Pipe>> {
        Rc::new(RefCell::new(Pipe {
            buf: VecDeque::new(),
            capacity,
            readers: 0,
            writers: 0,
            ino,
            fifo,
            reader_opens: 0,
            writer_opens: 0,
            writer_need: 0,
            writes: 0,
            room: 0,
            splicing: false,
        }))
    }

    /// The pipe object of named pipe `ino`, with no ends yet.
    pub fn fifo(ino: Ino, capacity: usize) -> Rc<RefCell<Pipe>> {
        Pipe::new(ino, Some(ino), capacity)
    }

    /// A write end waits for at least `need` bytes of space.
    pub fn want_space(&mut self, need: usize) {
        self.writer_need = match self.writer_need {
            0 => need,
            n => n.min(need),
        };
    }

    /// Data was read, with `before` the space before the read: returns whether to wake the waiting
    /// write ends. As on Linux, they are not woken on every small read: only when the space the
    /// waiting write ends need is available, or the space crosses `PIPE_BUF` (the threshold at
    /// which poll reports writable). With a reader using stdio reading 1 KB at a time, the writer
    /// used to be woken on every read.
    pub fn drained(&mut self, before: usize) -> bool {
        let now = self.space();
        let writable = before < PIPE_BUF && now >= PIPE_BUF;
        if writable {
            self.room += 1;
        }
        let wake = writable || (self.writer_need != 0 && now >= self.writer_need);
        if wake {
            self.writer_need = 0;
        }
        wake
    }

    /// Remaining space.
    pub fn space(&self) -> usize {
        self.capacity.saturating_sub(self.buf.len())
    }
}

/// One end of a pipe, held by an open file. A FIFO opened read-write is both a read end and a
/// write end.
pub struct PipeEnd {
    /// The pipe.
    pub pipe: Rc<RefCell<Pipe>>,
    /// Whether this is a read end.
    pub read: bool,
    /// Whether this is a write end.
    pub write: bool,
    /// When a FIFO is opened read-only with `O_NONBLOCK` and there is no write end yet: records
    /// how many times write ends had been opened, and POLLHUP is reported for no write end only
    /// after some write end has opened since (Linux's `f_version` of the read end, `pipe_poll`).
    pub hup_after: Option<u64>,
}

impl PipeEnd {
    /// Creates an anonymous pipe of capacity `capacity` and returns (read end, write end).
    pub fn pair(capacity: usize) -> (PipeEnd, PipeEnd) {
        static NEXT_INO: AtomicU64 = AtomicU64::new(1);
        let pipe = Pipe::new(NEXT_INO.fetch_add(1, Ordering::Relaxed), None, capacity);
        (
            PipeEnd::open(&pipe, true, false),
            PipeEnd::open(&pipe, false, true),
        )
    }

    /// Opens one end of a pipe (a FIFO's open, or a reopen through `/proc/<pid>/fd/<n>`).
    pub fn open(pipe: &Rc<RefCell<Pipe>>, read: bool, write: bool) -> PipeEnd {
        {
            let mut p = pipe.borrow_mut();
            if read {
                p.readers += 1;
                p.reader_opens += 1;
            }
            if write {
                p.writers += 1;
                p.writer_opens += 1;
            }
        }
        // Threads waiting for the other end while opening the FIFO can go on
        request_wake_object(object(pipe));
        PipeEnd {
            pipe: pipe.clone(),
            read,
            write,
            hup_after: None,
        }
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        let mut p = self.pipe.borrow_mut();
        if self.read {
            p.readers -= 1;
        }
        if self.write {
            p.writers -= 1;
        }
        // Threads waiting at the other end must see EOF or EPIPE
        request_wake_object(object(&self.pipe));
    }
}

/// The object that threads waiting for this pipe register ([`crate::kernel::Cx::wait_object`]).
pub(crate) fn object(pipe: &Rc<RefCell<Pipe>>) -> crate::kernel::wake::Object {
    crate::kernel::wake::of(pipe)
}
