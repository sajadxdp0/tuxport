//! The kernel log (the kernel/signal map): Linux 6.1's printk ring buffer.
//!
//! Records come from the kernel layer itself (the OOM killer ending a process, an internal error
//! ending a process, unhandled exceptions while `debug.exception-trace` is set, see [`printk`])
//! and from guest writes to `/dev/kmsg`. There are three kinds
//! of readers, as on Linux:
//!
//! - `/dev/kmsg`: each open file has its own position (a sequence number) and reads one record at
//!   a time, in the format `prefix,seq,usec,flags;text`, with unprintable characters escaped as
//!   `\xNN` ([`Klog::ext_text`]);
//! - syslog(2)'s read-all (`dmesg`): takes the newest records that fit from those since the last
//!   clear, each line prefixed with `<prefix>[sec.usec] ` ([`Klog::syslog_text`]), and changes no
//!   state;
//! - syslog(2)'s read and `/proc/kmsg`: one position shared by the whole system (`syslog_seq`);
//!   what is read is gone, and a record that does not fit is continued on the next read
//!   (`syslog_partial`).
//!
//! The buffer is 128 KB as with `CONFIG_LOG_BUF_SHIFT=17`, with at most 4096 records
//! (`PRB_AVGBITS`); when full, the oldest are dropped. Timestamps are the host's
//! `CLOCK_MONOTONIC` (Linux's `local_clock`, which excludes suspend). As in Linux 6.1's ring
//! buffer, a record without a trailing newline is not yet finalized (it waits for a `KERN_CONT`
//! continuation): readers cannot see it until the next record arrives.
//!
//! The kernel log lives in [`crate::kernel::Kernel::klog`] and, like other kernel objects, is
//! accessed only while holding the BKL; `/dev/kmsg` open files hold it ([`KmsgFile`]), so poll and
//! close need not find the kernel.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::rc::Rc;

use crate::cell::RefCell;
use crate::kernel::{request_wake_object, wake};

/// `log_buf_len`: the default `CONFIG_LOG_BUF_SHIFT` of 17.
pub(crate) const LOG_BUF_LEN: usize = 1 << 17;
/// The number of descriptors: `log_buf_len` at an average of 32 bytes per record
/// (`PRB_AVGBITS` = 5).
const MAX_RECORDS: usize = LOG_BUF_LEN >> 5;
/// Upper limit of one record's text (`LOG_LINE_MAX`; without `CONFIG_PRINTK_CALLER` the prefix is
/// at most 32). A single `/dev/kmsg` write longer than this is EINVAL; for one that fills it, the
/// stored text is one byte shorter still (`vsnprintf`'s terminating NUL counts too).
pub(crate) const LOG_LINE_MAX: usize = 1024 - 32;
/// Upper limit of one record in syslog format, including each line's prefix (`CONSOLE_LOG_MAX`);
/// lines that do not fit are dropped.
const CONSOLE_LOG_MAX: usize = 1024;
/// Record flags: had a trailing newline (removed when stored), continues the previous record
/// (`KERN_CONT`).
const LOG_NEWLINE: u8 = 2;
const LOG_CONT: u8 = 8;
/// Linux's `CLOCK_MONOTONIC`.
const CLOCK_MONOTONIC: i32 = 1;

/// Log levels (`KERN_*`).
pub(crate) mod level {
    pub const ERR: u8 = 3;
    pub const WARNING: u8 = 4;
    pub const INFO: u8 = 6;
}

/// `/proc/sys/kernel/printk_devkmsg`: whether guest writes to `/dev/kmsg` are logged and
/// ratelimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Devkmsg {
    On,
    Off,
    Ratelimit,
}

impl Devkmsg {
    pub fn name(self) -> &'static str {
        match self {
            Devkmsg::On => "on",
            Devkmsg::Off => "off",
            Devkmsg::Ratelimit => "ratelimit",
        }
    }
}

/// One record.
struct Record {
    seq: u64,
    ts_nsec: u64,
    facility: u8,
    level: u8,
    flags: u8,
    text: Box<[u8]>,
}

impl Record {
    /// Bytes taken in the data ring: the block ID (8 bytes) plus the text, aligned to 8 bytes.
    fn data_size(&self) -> usize {
        (8 + self.text.len()).div_ceil(8) * 8
    }

    /// The prefix of each line in syslog format (`info_print_prefix`).
    fn prefix(&self, syslog: bool, time: bool) -> String {
        let mut s = String::new();
        if syslog {
            write!(s, "<{}>", (self.facility as u32) << 3 | self.level as u32).unwrap();
        }
        if time {
            let (sec, ns) = (self.ts_nsec / 1_000_000_000, self.ts_nsec % 1_000_000_000);
            write!(s, "[{sec:5}.{:06}] ", ns / 1000).unwrap();
        }
        s
    }

    /// Number of lines: newlines in the text plus one (the trailing newline was removed when
    /// stored).
    fn lines(&self) -> usize {
        self.text.iter().filter(|&&b| b == b'\n').count() + 1
    }
}

/// A record read out (copied, and written to the guest after the borrow is released).
pub(crate) struct Read {
    /// The record's sequence number; larger than the one asked for means that one was pushed out.
    pub seq: u64,
    pub text: Vec<u8>,
}

/// The kernel log.
pub(crate) struct Klog {
    records: VecDeque<Record>,
    /// Total bytes the records take in the data ring ([`Record::data_size`]).
    data: usize,
    /// Sequence number of the next record.
    next_seq: u64,
    /// The first record after syslog's clear (`dmesg -c`).
    clear_seq: u64,
    /// The position shared by syslog's read and `/proc/kmsg`, and the bytes of this record already
    /// read.
    syslog_seq: u64,
    syslog_partial: usize,
    /// Whether the start of this record was read with time (`syslog_time`): if the setting changes
    /// halfway through, the original one is still used.
    syslog_time: bool,
    /// `/sys/module/printk/parameters/time` (`CONFIG_PRINTK_TIME=y`).
    pub printk_time: bool,
    pub devkmsg: Devkmsg,
    /// The warning for a process reading the log with only `CAP_SYS_ADMIN` has been logged
    /// (`pr_warn_once`).
    pub sys_admin_warned: bool,
}

impl Default for Klog {
    fn default() -> Klog {
        Klog {
            records: VecDeque::new(),
            data: 0,
            next_seq: 0,
            clear_seq: 0,
            syslog_seq: 0,
            syslog_partial: 0,
            syslog_time: true,
            printk_time: true,
            devkmsg: Devkmsg::Ratelimit,
            sys_admin_warned: false,
        }
    }
}

impl Klog {
    /// Stores a record (`vprintk_store`): the text up to the first NUL (`%s`), at most
    /// `LOG_LINE_MAX - 1` bytes, with one trailing newline removed and recorded as a flag. Does
    /// not wake readers, see [`emit`].
    fn store(&mut self, facility: u8, level: u8, text: &[u8]) {
        let text = &text[..text.iter().position(|&b| b == 0).unwrap_or(text.len())];
        let mut text = &text[..text.len().min(LOG_LINE_MAX - 1)];
        let mut flags = 0;
        if let Some(rest) = text.strip_suffix(b"\n") {
            text = rest;
            flags |= LOG_NEWLINE;
        }
        let ts_nsec = tuxport_hal::clock_gettime(CLOCK_MONOTONIC)
            .map_or(0, |(s, ns)| s as u64 * 1_000_000_000 + ns as u64);
        let record = Record {
            seq: self.next_seq,
            ts_nsec,
            facility,
            level: level & 7,
            flags,
            text: text.into(),
        };
        self.next_seq += 1;
        self.data += record.data_size();
        self.records.push_back(record);
        while self.data > LOG_BUF_LEN || self.records.len() > MAX_RECORDS {
            let old = self.records.pop_front().expect("the newest record fits");
            self.data -= old.data_size();
        }
    }

    /// The format read from `/dev/kmsg` (`info_print_ext_header`, `msg_print_ext_body`): the first
    /// record with a sequence number not below `seq`.
    pub fn ext_text(&self, seq: u64) -> Option<Read> {
        let r = self.record(seq)?;
        let mut text = format!(
            "{},{},{},{};",
            (r.facility as u32) << 3 | r.level as u32,
            r.seq,
            r.ts_nsec / 1000,
            if r.flags & LOG_CONT != 0 { 'c' } else { '-' }
        )
        .into_bytes();
        for &c in &*r.text {
            if !(b' '..127).contains(&c) || c == b'\\' {
                text.extend_from_slice(format!("\\x{c:02x}").as_bytes());
            } else {
                text.push(c);
            }
        }
        text.push(b'\n');
        Some(Read { seq: r.seq, text })
    }

    /// syslog's read-all (`syslog_print_all`): from the records since the clear, takes the newest
    /// ones that fit in `size` bytes. With `clear` (clear after reading), later reads start after
    /// what was read.
    pub fn read_all(&mut self, size: usize, clear: bool) -> Vec<u8> {
        let time = self.printk_time;
        let start = self.clear_seq.max(self.first_seq());
        let from = |seq: u64| self.readable_from(seq);
        let mut len: usize = from(start).map(|r| Self::syslog_size(r, time)).sum();
        let mut seq = self.next_seq();
        for r in from(start) {
            if len <= size {
                seq = r.seq;
                break;
            }
            len -= Self::syslog_size(r, time);
        }
        let mut out = Vec::new();
        let mut end = self.next_seq();
        for r in from(seq) {
            let text = Self::syslog_text(r, time);
            if out.len() + text.len() > size {
                // Linux does seq-- here: when clearing after reading, the clear position stops at
                // the record before the one that did not fit
                end = r.seq.saturating_sub(1);
                break;
            }
            out.extend_from_slice(&text);
        }
        if clear {
            self.clear_seq = end;
        }
        out
    }

    /// syslog's clear (`syslog_clear`).
    pub fn clear(&mut self) {
        self.clear_seq = self.next_seq();
    }

    /// Whether syslog's read and `/proc/kmsg` have a readable record (the wait condition).
    pub fn syslog_readable(&self) -> bool {
        self.readable(self.syslog_seq)
    }

    /// syslog's read (`syslog_print`), with a record already readable: reads the records that fit
    /// in `size` bytes; when the first one does not fit, reads its first part and continues next
    /// time.
    pub fn syslog_read(&mut self, size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < size {
            let Some(seq) = self.record(self.syslog_seq).map(|r| r.seq) else {
                break;
            };
            if seq != self.syslog_seq {
                self.syslog_seq = seq;
                self.syslog_partial = 0;
            }
            if self.syslog_partial == 0 {
                self.syslog_time = self.printk_time;
            }
            let r = self.record(seq).expect("the record was just found");
            let text = Self::syslog_text(r, self.syslog_time);
            let skip = self.syslog_partial;
            let room = size - out.len();
            let n = if text.len() - skip <= room {
                self.syslog_seq = seq + 1;
                self.syslog_partial = 0;
                text.len() - skip
            } else if out.is_empty() {
                self.syslog_partial += room;
                room
            } else {
                0
            };
            if n == 0 {
                break;
            }
            out.extend_from_slice(&text[skip..skip + n]);
        }
        out
    }

    /// syslog's unread byte count (`SYSLOG_ACTION_SIZE_UNREAD`); when `/proc/kmsg` uses it
    /// (`from_proc`), the number of unread records. Records that were pushed out are skipped first.
    pub fn size_unread(&mut self, from_proc: bool) -> u64 {
        let Some(r) = self.record(self.syslog_seq) else {
            return 0;
        };
        if r.seq != self.syslog_seq {
            self.syslog_seq = r.seq;
            self.syslog_partial = 0;
        }
        if from_proc {
            return self.next_seq() - self.syslog_seq;
        }
        let mut time = if self.syslog_partial > 0 {
            self.syslog_time
        } else {
            self.printk_time
        };
        let mut n = 0;
        for r in self.readable_from(self.syslog_seq) {
            n += Self::syslog_size(r, time);
            time = self.printk_time;
        }
        (n - self.syslog_partial) as u64
    }

    /// Readable records with sequence numbers not below `seq`, one entry per line as `dmesg` shows
    /// them, prefixed with `[sec.usec] ` (always with time, regardless of `printk_time`, and
    /// whether or not syslog was cleared). Control characters are escaped as `\xNN` and non-UTF-8
    /// bytes replaced with U+FFFD. Kernel bug reports carry them to the app ([`crate::bug`]).
    pub fn lines_from(&self, seq: u64) -> Vec<String> {
        let mut out = Vec::new();
        for r in self.readable_from(seq) {
            let prefix = r.prefix(false, true);
            for line in r.text.split(|&b| b == b'\n') {
                let mut s = prefix.clone();
                for c in String::from_utf8_lossy(line).chars() {
                    if c.is_control() {
                        write!(s, "\\x{:02x}", c as u32).unwrap();
                    } else {
                        s.push(c);
                    }
                }
                out.push(s);
            }
        }
        out
    }

    /// One record in syslog format (`record_print_text`, with a `CONSOLE_LOG_MAX` buffer): each
    /// line gets the prefix and a trailing newline is added; lines that do not fit with the prefix
    /// are dropped from the end, and a first line that does not fit is truncated.
    fn syslog_text(r: &Record, time: bool) -> Vec<u8> {
        let prefix = r.prefix(true, time);
        let prefix = prefix.as_bytes();
        let mut out = Vec::new();
        let mut text: &[u8] = &r.text;
        let mut truncated = false;
        loop {
            let next = text.iter().position(|&b| b == b'\n');
            if next.is_none() && truncated {
                break;
            }
            let line_len = next.unwrap_or(text.len());
            let mut text_len = text.len();
            if out.len() + prefix.len() + text_len + 2 > CONSOLE_LOG_MAX {
                if out.len() + prefix.len() + line_len + 2 > CONSOLE_LOG_MAX {
                    break;
                }
                text_len = CONSOLE_LOG_MAX - out.len() - prefix.len() - 2;
                truncated = true;
            }
            out.extend_from_slice(prefix);
            out.extend_from_slice(&text[..line_len.min(text_len)]);
            out.push(b'\n');
            if text_len == line_len {
                break;
            }
            text = &text[line_len + 1..text_len];
        }
        out
    }

    /// The length of [`Klog::syslog_text`] when nothing is truncated
    /// (`get_record_print_text_size`).
    fn syslog_size(r: &Record, time: bool) -> usize {
        r.prefix(true, time).len() * r.lines() + r.text.len() + 1
    }

    /// Whether a record with a sequence number not below `seq` exists: what readers wait for.
    pub fn readable(&self, seq: u64) -> bool {
        self.record(seq).is_some()
    }

    /// The first readable record with a sequence number not below `seq` (`prb_read_valid`).
    fn record(&self, seq: u64) -> Option<&Record> {
        let index: usize = seq
            .saturating_sub(self.first_seq())
            .try_into()
            .unwrap_or(usize::MAX);
        self.records.get(index).filter(|_| index < self.visible())
    }

    /// Readable records with sequence numbers not below `seq`.
    fn readable_from(&self, seq: u64) -> impl Iterator<Item = &Record> {
        self.records
            .iter()
            .take(self.visible())
            .filter(move |r| r.seq >= seq)
    }

    /// The number of readable records: when the newest one has no trailing newline it is not yet
    /// finalized (`desc_make_final` waits for the next record) and does not count.
    fn visible(&self) -> usize {
        let open = self
            .records
            .back()
            .is_some_and(|r| r.flags & LOG_NEWLINE == 0);
        self.records.len() - open as usize
    }

    /// Sequence number of the oldest record (`prb_first_valid_seq`); with no records, that of the
    /// next one.
    pub fn first_seq(&self) -> u64 {
        self.records.front().map_or(self.next_seq, |r| r.seq)
    }

    /// The sequence number after the last readable record (`prb_next_seq`): also how many records
    /// have ever been readable; poll's data generation uses it.
    pub fn next_seq(&self) -> u64 {
        self.first_seq() + self.visible() as u64
    }

    /// Sequence number of the next record to be logged (whether or not the newest one is
    /// finalized).
    pub fn upcoming_seq(&self) -> u64 {
        self.next_seq
    }

    /// The first record after syslog's last clear (`SEEK_DATA` on `/dev/kmsg`).
    pub fn clear_seq(&self) -> u64 {
        self.clear_seq
    }
}

/// Logs a message from the kernel layer itself (`printk`, facility 0) and wakes threads waiting
/// for the log. Like messages in the kernel it ends with a newline (readable at once). While the
/// log is borrowed (an error in the log's own code), the message is dropped. The caller holds the
/// BKL.
pub(crate) fn printk(log: &RefCell<Klog>, level: u8, args: fmt::Arguments<'_>) {
    let mut text = fmt::format(args);
    text.push('\n');
    emit(log, 0, level, text.as_bytes());
}

/// Logs a record (`vprintk_emit`) and wakes threads waiting for the log (Linux's `log_wait`).
pub(crate) fn emit(log: &RefCell<Klog>, facility: u8, level: u8, text: &[u8]) {
    if let Ok(mut l) = log.try_borrow_mut() {
        l.store(facility, level, text);
        request_wake_object(wake::of(log));
    }
}

/// One `/dev/kmsg` open file (`struct devkmsg_user`): the read position and the write ratelimit.
pub(crate) struct KmsgFile {
    pub log: Rc<RefCell<Klog>>,
    /// Sequence number of the next read: the oldest record at open.
    pub seq: u64,
    rs: Ratelimit,
}

impl KmsgFile {
    pub fn new(log: Rc<RefCell<Klog>>) -> KmsgFile {
        let seq = log.borrow().first_seq();
        KmsgFile {
            log,
            seq,
            rs: Ratelimit::default(),
        }
    }

    /// Writes a record (`devkmsg_write`): a leading `<digits>` is a syslog prefix whose low three
    /// bits are the level and the rest the facility (0 still counts as `LOG_USER`); without a
    /// prefix, the default message level and `LOG_USER`. Writes dropped by the ratelimit (at most
    /// 10 per 5 seconds when `printk_devkmsg` is `ratelimit`) still count as successful. The
    /// length has already been checked (not over [`LOG_LINE_MAX`]).
    pub fn write(&mut self, data: &[u8], default_level: i32) {
        let devkmsg = self.log.borrow().devkmsg;
        if devkmsg == Devkmsg::Off
            || (devkmsg == Devkmsg::Ratelimit && !self.rs.allow(std::time::Instant::now()))
        {
            return;
        }
        let (mut level, mut facility) = (default_level as u8 & 7, 1);
        let mut line = data;
        if let Some(rest) = data.strip_prefix(b"<") {
            let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
            if rest.get(digits) == Some(&b'>') {
                // simple_strtoul's result truncated to unsigned int
                let u = rest[..digits].iter().fold(0u64, |v, &d| {
                    v.wrapping_mul(10).wrapping_add((d - b'0') as u64)
                }) as u32;
                level = (u & 7) as u8;
                if (u >> 3) & 0xff != 0 {
                    facility = ((u >> 3) & 0xff) as u8;
                }
                line = &rest[digits + 1..];
            }
        }
        emit(&self.log, facility, level, line);
    }
}

impl Drop for KmsgFile {
    /// At close, reports how many writes the ratelimit dropped (`ratelimit_state_exit`, with the
    /// name of the closing process).
    fn drop(&mut self) {
        if self.rs.missed > 0 {
            // Open files are always released while holding the BKL; unit tests have no BKL and no
            // current thread
            let current = crate::kernel::bkl_held()
                .then(crate::task::current)
                .flatten();
            let comm = current.map_or_else(String::new, |(t, _)| {
                crate::pseudo::comm_str(Some(&*t), t.tgid)
            });
            printk(
                &self.log,
                level::WARNING,
                format_args!(
                    "printk: {comm}: {} output lines suppressed due to ratelimiting",
                    self.rs.missed
                ),
            );
        }
    }
}

/// `struct ratelimit_state` with the default interval of 5 seconds and at most 10 times. With
/// `RATELIMIT_MSG_ON_RELEASE` (`/dev/kmsg` writes) the number dropped is reported only at close
/// ([`Ratelimit::allow`]); without it (unhandled exceptions, crate::signal) when the next interval
/// begins ([`Ratelimit::allow_reporting`]).
#[derive(Default)]
pub(crate) struct Ratelimit {
    begin: Option<std::time::Instant>,
    printed: u32,
    missed: u32,
}

impl Ratelimit {
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    const BURST: u32 = 10;

    /// `___ratelimit` with `RATELIMIT_MSG_ON_RELEASE`: whether to let this one through.
    fn allow(&mut self, now: std::time::Instant) -> bool {
        self.check(now, true).0
    }

    /// `___ratelimit` without `RATELIMIT_MSG_ON_RELEASE`: whether to let this one through, and,
    /// when it begins a new interval, how many the earlier ones dropped (Linux then logs
    /// "`<function>`: N callbacks suppressed" before it).
    pub fn allow_reporting(&mut self, now: std::time::Instant) -> (bool, u32) {
        self.check(now, false)
    }

    fn check(&mut self, now: std::time::Instant, on_release: bool) -> (bool, u32) {
        let begin = *self.begin.get_or_insert(now);
        let mut suppressed = 0;
        if now > begin + Self::INTERVAL {
            if !on_release {
                suppressed = std::mem::take(&mut self.missed);
            }
            self.begin = Some(now);
            self.printed = 0;
        }
        if self.printed < Self::BURST {
            self.printed += 1;
            (true, suppressed)
        } else {
            self.missed += 1;
            (false, suppressed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each with a trailing newline, so readable.
    fn log_with(texts: &[&[u8]]) -> Klog {
        let mut l = Klog::default();
        for t in texts {
            l.store(0, 6, &[t, &b"\n"[..]].concat());
        }
        l
    }

    #[test]
    fn records_strip_one_newline_and_stop_at_nul() {
        let mut l = Klog::default();
        for t in [&b"a\n\n"[..], b"b\0c\n", b""] {
            l.store(0, 6, t);
        }
        let texts: Vec<&[u8]> = l.records.iter().map(|r| &*r.text).collect();
        assert_eq!(texts, [&b"a\n"[..], b"b", b""]);
        assert_eq!(l.records[0].flags, LOG_NEWLINE);
        assert_eq!(l.records[1].flags, 0);
    }

    #[test]
    fn a_record_without_a_newline_is_read_after_the_next_one() {
        let mut l = Klog::default();
        l.store(1, 4, b"open");
        assert_eq!((l.first_seq(), l.next_seq()), (0, 0));
        assert!(!l.readable(0) && l.ext_text(0).is_none());
        assert_eq!(l.read_all(4096, false), b"");
        l.store(1, 4, b"closed\n");
        assert_eq!(l.next_seq(), 2);
        assert!(l.ext_text(0).unwrap().text.ends_with(b";open\n"));
    }

    #[test]
    fn ext_text_escapes_and_reports_the_next_valid_record() {
        let l = log_with(&[b"x\\y\x01\xff"]);
        let r = l.ext_text(0).unwrap();
        let s = String::from_utf8(r.text).unwrap();
        assert!(s.starts_with("6,0,"), "{s}");
        assert!(s.ends_with(",-;x\\x5cy\\x01\\xff\n"), "{s}");
        assert!(l.ext_text(1).is_none());
    }

    #[test]
    fn old_records_are_dropped_when_the_buffer_is_full() {
        let mut l = Klog::default();
        let mut line = [b'x'; 900];
        line[899] = b'\n';
        for _ in 0..200 {
            l.store(1, 4, &line);
        }
        assert!(l.data <= LOG_BUF_LEN);
        assert_eq!(l.next_seq(), 200);
        assert!(l.first_seq() > 0);
        // A sequence number that was pushed out reads the oldest record
        assert_eq!(l.ext_text(0).unwrap().seq, l.first_seq());
        for i in 0..5000 {
            l.store(1, 4, format!("{i}\n").as_bytes());
        }
        assert_eq!(l.records.len(), MAX_RECORDS);
    }

    #[test]
    fn syslog_lines_get_prefixes_and_long_records_lose_lines() {
        let mut l = log_with(&[b"one\ntwo"]);
        l.printk_time = false;
        assert_eq!(l.read_all(4096, false), b"<6>one\n<6>two\n");
        // The second line with the prefix does not fit in 1024 bytes, so it is dropped
        let mut text = vec![b'a'; 600];
        text.push(b'\n');
        text.extend(vec![b'b'; 600]);
        let r = Record {
            seq: 0,
            ts_nsec: 0,
            facility: 0,
            level: 6,
            flags: 0,
            text: text.into(),
        };
        let out = Klog::syslog_text(&r, false);
        assert_eq!(out.len(), 3 + 600 + 1);
    }

    #[test]
    fn lines_from_are_dmesg_lines_with_escapes() {
        let mut l = log_with(&[b"old", b"two\nlines", b"tab\there\x1b[0m \xff ok"]);
        l.printk_time = false;
        l.clear();
        l.store(1, 4, b"open");
        assert_eq!(l.upcoming_seq(), 4);
        let lines = l.lines_from(1);
        // Always with time, still there after a clear; the unfinalized record does not count
        let texts: Vec<&str> = lines
            .iter()
            .map(|s| {
                assert!(s.starts_with('[') && s.contains("] "), "{s}");
                &s[s.find("] ").unwrap() + 2..]
            })
            .collect();
        assert_eq!(texts, ["two", "lines", "tab\\x09here\\x1b[0m \u{fffd} ok"]);
        assert_eq!(l.lines_from(0).len(), 4);
        assert!(l.lines_from(l.upcoming_seq()).is_empty());
    }

    #[test]
    fn read_all_keeps_the_newest_records_that_fit_and_clear_moves_on() {
        let mut l = log_with(&[b"first", b"second", b"third"]);
        l.printk_time = false;
        assert_eq!(l.read_all(20, false), b"<6>second\n<6>third\n");
        assert_eq!(l.read_all(4096, true).len(), 9 + 10 + 9);
        assert_eq!(l.read_all(4096, false), b"");
        l.store(0, 3, b"later\n");
        assert_eq!(l.read_all(4096, false), b"<3>later\n");
    }

    #[test]
    fn syslog_read_consumes_and_continues_partial_records() {
        let mut l = log_with(&[b"abcdef", b"gh"]);
        l.printk_time = false;
        assert_eq!(l.size_unread(false), 10 + 6);
        assert_eq!(l.size_unread(true), 2);
        assert_eq!(l.syslog_read(4), b"<6>a");
        assert_eq!(l.size_unread(false), 6 + 6);
        // The rest fits, the next record does not: only the end of this record is read
        assert_eq!(l.syslog_read(10), b"bcdef\n");
        assert_eq!(l.syslog_read(100), b"<6>gh\n");
        assert!(!l.syslog_readable());
        assert_eq!(l.size_unread(false), 0);
    }

    #[test]
    fn devkmsg_prefixes_and_ratelimit() {
        let log = Rc::new(RefCell::new(Klog::default()));
        let mut f = KmsgFile::new(log.clone());
        f.write(b"<11>user err\n", 4);
        f.write(b"<0>kernel facility stays user\n", 4);
        f.write(b"<x>not a prefix\n", 4);
        f.write(b"<>empty prefix\n", 4);
        {
            let l = log.borrow();
            let meta: Vec<(u8, u8, &[u8])> = l
                .records
                .iter()
                .map(|r| (r.facility, r.level, &*r.text))
                .collect();
            assert_eq!(
                meta,
                [
                    (1, 3, &b"user err"[..]),
                    (1, 0, b"kernel facility stays user"),
                    (1, 4, b"<x>not a prefix"),
                    (1, 0, b"empty prefix"),
                ]
            );
        }
        for _ in 0..10 {
            f.write(b"more\n", 4);
        }
        // The first 10 pass, later ones are dropped, and the count dropped is reported at close
        assert_eq!(log.borrow().next_seq(), 10);
        drop(f);
        let l = log.borrow();
        let last = l.records.back().unwrap();
        assert_eq!(last.level, level::WARNING);
        assert!(
            last.text
                .ends_with(b": 4 output lines suppressed due to ratelimiting")
        );
    }

    #[test]
    fn ratelimit_window_restarts() {
        let mut rs = Ratelimit::default();
        let t = std::time::Instant::now();
        assert!((0..10).all(|_| rs.allow(t)));
        assert!(!rs.allow(t));
        assert!(rs.allow(t + Ratelimit::INTERVAL + std::time::Duration::from_millis(1)));
        assert_eq!(rs.missed, 1);
    }

    /// Without `RATELIMIT_MSG_ON_RELEASE`, the first call of a new interval gets the number
    /// dropped, once.
    #[test]
    fn ratelimit_reports_drops_when_the_window_restarts() {
        let mut rs = Ratelimit::default();
        let t = std::time::Instant::now();
        assert!((0..10).all(|_| rs.allow_reporting(t) == (true, 0)));
        assert_eq!(rs.allow_reporting(t), (false, 0));
        assert_eq!(rs.allow_reporting(t), (false, 0));
        let later = t + Ratelimit::INTERVAL + std::time::Duration::from_millis(1);
        assert_eq!(rs.allow_reporting(later), (true, 2));
        assert_eq!(rs.allow_reporting(later), (true, 0));
        assert_eq!(rs.missed, 0);
    }
}
