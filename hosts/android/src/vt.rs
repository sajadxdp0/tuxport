//! The libghostty-vt terminals of the app's tabs (the android/vtcore map): each
//! `VtSession` has a terminal here, fed straight from its kernel console, so console output never
//! crosses JNI as bytes.
//!
//! The matching Kotlin declarations are in
//! `android/app/src/main/java/io/github/sajadxdp0/tuxport/VtNative.kt`. Like [`Machine`]s in the crate
//! root, tabs are registered in a table and Kotlin holds a handle.
//!
//! A tab names its kernel by the crate root's handle too and looks it up for each call, as the
//! `Native` functions do, instead of holding an `Arc<Machine>`: `Native.stop` and `shutdown` only
//! remove the handle and end the processes, and the store is closed when the last reference is
//! dropped. A tab outliving its kernel would keep the old store open after a reboot or a distro
//! switch, and the next kernel would then start without the boot cleanup.
//!
//! Threads: Kotlin's pump thread runs [`pump`]'s loop (read the console, feed the terminal, write
//! the terminal's replies back) until the session ends; the main thread takes frames and the
//! pixels of the images they place, resizes, scrolls, encodes keys, mouse and focus events and
//! pastes, selects, searches and copies, and reads the title and transcript; the service's worker
//! takes snapshots for saving tabs and compresses every tab's scrollback when memory is trimmed;
//! the `vt-compress` thread compresses idle tabs' scrollback ([`compressor`]). The terminal is not
//! thread-safe, so every call into vt.c holds the tab's lock; the frame buffer it returns is only
//! written by `frame`, which only the main thread calls, so the main thread may read it after the
//! lock is released.
//!
//! [`pump`]: Java_io_github_sajadxdp0_tuxport_VtNative_pump

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jbyteArray, jfloat, jint, jintArray, jlong, jobject, jstring};
use tuxport_kernel::Machine;

use super::{machine, string, throw};

#[repr(C)]
struct TuxportVt {
    _opaque: [u8; 0],
}

/// What `tuxport_vt_take_effects` reports (vt.c's `TuxportVtEffects`).
#[repr(C)]
struct RawEffects {
    bells: u32,
    clipboard: i32,
    clip: *const u8,
    clip_len: usize,
    notified: bool,
    title: *const u8,
    title_len: usize,
    body: *const u8,
    body_len: usize,
    progress_state: i32,
    progress: i32,
    statuses: *const u8,
    statuses_len: usize,
    clipboard_reads: u32,
    clipboard_read_kind: u8,
    command_finished: bool,
    command_has_exit: bool,
    command_exit: i32,
    command_ms: u64,
    tmux_detached: bool,
}

/// vt.c's `TUXPORT_CLIPBOARD_TEXT` and `TUXPORT_CLIPBOARD_CLEAR`.
const CLIPBOARD_TEXT: i32 = 1;
const CLIPBOARD_CLEAR: i32 = 2;

/// vt.c's `STATUS_HEADER_BYTES`: the fixed part of a queued program status report.
const STATUS_HEADER_BYTES: usize = 7;

/// vt.c's `TUXPORT_COMPRESS_STEP` and `TUXPORT_COMPRESS_WAIT` (`TUXPORT_COMPRESS_DONE` is any other).
const COMPRESS_STEP: i32 = 1;
const COMPRESS_WAIT: i32 = 2;

/// How long a tab's terminal must stay unchanged before its scrollback is compressed, and the
/// pause between two steps of a pass, in which the pump and the main thread get the lock: Ghostty's
/// own intervals.
const COMPRESS_IDLE: Duration = Duration::from_millis(250);
const COMPRESS_STEP_GAP: Duration = Duration::from_millis(1);

/// How long a scheduled pass waits for the terminal to be idle before it runs anyway (Ghostty
/// waits for good): output that never pauses would otherwise keep a tab's scrollback, and what a
/// transcript restored of it, uncompressed.
const COMPRESS_MAX_WAIT: Duration = Duration::from_secs(5);

// vt.c
unsafe extern "C" {
    fn tuxport_vt_new(cols: u16, rows: u16, cell_width: u32, cell_height: u32) -> *mut TuxportVt;
    fn tuxport_vt_restore(
        data: *const u8,
        len: usize,
        cols: u16,
        rows: u16,
        cell_width: u32,
        cell_height: u32,
    ) -> *mut TuxportVt;
    fn tuxport_vt_set_cursor(vt: *mut TuxportVt, style: i32, blink: bool);
    fn tuxport_vt_free(vt: *mut TuxportVt);
    fn tuxport_vt_feed(vt: *mut TuxportVt, data: *const u8, len: usize, background: bool);
    fn tuxport_vt_take_replies(vt: *mut TuxportVt, len: *mut usize) -> *const u8;
    fn tuxport_vt_set_password_input(vt: *mut TuxportVt, on: bool);
    fn tuxport_vt_resize(
        vt: *mut TuxportVt,
        cols: u16,
        rows: u16,
        cell_width: u32,
        cell_height: u32,
    );
    fn tuxport_vt_scroll(vt: *mut TuxportVt, delta: i32);
    fn tuxport_vt_at_prompt(vt: *mut TuxportVt) -> bool;
    fn tuxport_vt_jump_prompt(vt: *mut TuxportVt, delta: i32);
    fn tuxport_vt_frame(vt: *mut TuxportVt, full: bool, len: *mut usize) -> *const u8;
    fn tuxport_vt_image(
        vt: *mut TuxportVt,
        id: u32,
        generation: u64,
        out: *mut u8,
        width: u32,
        height: u32,
        stride: usize,
    ) -> bool;
    #[allow(clippy::too_many_arguments)]
    fn tuxport_vt_key(
        vt: *mut TuxportVt,
        key: i32,
        mods: u16,
        consumed: u16,
        action: i32,
        text: *const u8,
        text_len: usize,
        unshifted: u32,
        out: *mut u8,
        cap: usize,
    ) -> isize;
    fn tuxport_vt_paste(vt: *mut TuxportVt, data: *const u8, len: usize, allow_unsafe: bool)
    -> i32;
    #[allow(clippy::too_many_arguments)]
    fn tuxport_vt_mouse(
        vt: *mut TuxportVt,
        action: i32,
        button: i32,
        mods: u16,
        x: f32,
        y: f32,
        pressed: bool,
        out: *mut u8,
        cap: usize,
    ) -> isize;
    fn tuxport_vt_focus(vt: *mut TuxportVt, gained: bool, out: *mut u8, cap: usize) -> isize;
    fn tuxport_vt_gesture_press(
        vt: *mut TuxportVt,
        behavior: i32,
        x: u16,
        y: u16,
        px: f64,
        py: f64,
    ) -> bool;
    fn tuxport_vt_gesture_drag(vt: *mut TuxportVt, x: u16, y: u16, px: f64, py: f64) -> bool;
    fn tuxport_vt_gesture_end(vt: *mut TuxportVt);
    fn tuxport_vt_mouse_press(
        vt: *mut TuxportVt,
        x: u16,
        y: u16,
        px: f64,
        py: f64,
        time_ns: u64,
        interval_ns: u64,
        output: bool,
    ) -> i32;
    fn tuxport_vt_mouse_drag(
        vt: *mut TuxportVt,
        x: u16,
        y: u16,
        px: f64,
        py: f64,
        rectangle: bool,
    ) -> i32;
    fn tuxport_vt_mouse_release(vt: *mut TuxportVt, x: i32, y: i32) -> i32;
    fn tuxport_vt_select_context(vt: *mut TuxportVt, x: u16, y: u16) -> bool;
    fn tuxport_vt_select_grab(vt: *mut TuxportVt, end: bool) -> bool;
    fn tuxport_vt_select_to(vt: *mut TuxportVt, x: u16, y: u16) -> bool;
    fn tuxport_vt_select_adjust(vt: *mut TuxportVt, adjustment: i32) -> bool;
    fn tuxport_vt_select_rectangle(vt: *mut TuxportVt, on: bool) -> bool;
    fn tuxport_vt_select_all(vt: *mut TuxportVt) -> bool;
    fn tuxport_vt_output_at(vt: *mut TuxportVt, x: u16, y: u16) -> bool;
    fn tuxport_vt_select_output(vt: *mut TuxportVt) -> bool;
    fn tuxport_vt_select_clear(vt: *mut TuxportVt);
    fn tuxport_vt_copy(vt: *mut TuxportVt, len: *mut usize) -> *const u8;
    fn tuxport_vt_link(vt: *mut TuxportVt, x: u16, y: u16, len: *mut usize) -> *const u8;
    fn tuxport_vt_prompt_click(vt: *mut TuxportVt, x: u16, y: u16) -> bool;
    fn tuxport_vt_search(vt: *mut TuxportVt, needle: *const u8, len: usize) -> bool;
    fn tuxport_vt_search_select(vt: *mut TuxportVt, older: bool) -> bool;
    fn tuxport_vt_search_rows(
        vt: *mut TuxportVt,
        len: *mut usize,
        selected: *mut i64,
    ) -> *const u32;
    fn tuxport_vt_search_end(vt: *mut TuxportVt);
    fn tuxport_vt_clear(vt: *mut TuxportVt) -> i32;
    fn tuxport_vt_reset(vt: *mut TuxportVt) -> bool;
    fn tuxport_vt_take_title_changed(vt: *mut TuxportVt) -> bool;
    fn tuxport_vt_take_effects(vt: *mut TuxportVt, out: *mut RawEffects);
    fn tuxport_vt_title(vt: *mut TuxportVt, len: *mut usize) -> *const u8;
    fn tuxport_vt_message(vt: *mut TuxportVt, text: *const u8, len: usize) -> bool;
    fn tuxport_vt_transcript(vt: *mut TuxportVt, len: *mut usize) -> *const u8;
    fn tuxport_vt_snapshot(vt: *mut TuxportVt, out: *mut *mut u8, len: *mut usize) -> i32;
    fn tuxport_vt_free_snapshot(data: *mut u8, len: usize);
    fn tuxport_vt_version(len: *mut usize) -> *const u8;
    fn tuxport_vt_compress_wanted(vt: *mut TuxportVt) -> bool;
    fn tuxport_vt_compress_step(vt: *mut TuxportVt, may_wait: bool) -> i32;
    fn tuxport_vt_compress_all(vt: *mut TuxportVt);
    fn tuxport_vt_drop_images(vt: *mut TuxportVt);
}

/// `AndroidBitmapInfo` (`<android/bitmap.h>`).
#[repr(C)]
struct BitmapInfo {
    width: u32,
    height: u32,
    stride: u32,
    format: i32,
    flags: u32,
}

/// `ANDROID_BITMAP_FORMAT_RGBA_8888`, a `Bitmap.Config.ARGB_8888` bitmap's pixels: R, G, B, A
/// bytes, premultiplied.
const BITMAP_RGBA_8888: i32 = 1;

// The NDK's bitmap access (libjnigraphics, a system library on every version)
#[link(name = "jnigraphics")]
unsafe extern "C" {
    fn AndroidBitmap_getInfo(
        env: *mut jni::sys::JNIEnv,
        bitmap: jobject,
        info: *mut BitmapInfo,
    ) -> i32;
    fn AndroidBitmap_lockPixels(
        env: *mut jni::sys::JNIEnv,
        bitmap: jobject,
        pixels: *mut *mut std::ffi::c_void,
    ) -> i32;
    fn AndroidBitmap_unlockPixels(env: *mut jni::sys::JNIEnv, bitmap: jobject) -> i32;
}

/// Owns one vt.c terminal.
struct Term(*mut TuxportVt);

// SAFETY: the terminal is only used under the tab's lock (module docs)
unsafe impl Send for Term {}

impl Term {
    /// Feeds bytes and returns the replies they produced. `background`: the console's foreground
    /// process group did not write them (`Machine::read_console_run`).
    fn feed(&mut self, data: &[u8], background: bool) -> Vec<u8> {
        unsafe { tuxport_vt_feed(self.0, data.as_ptr(), data.len(), background) };
        self.take_replies()
    }

    /// The bytes the terminal wrote for the console since the last call (replies, a paste).
    fn take_replies(&mut self) -> Vec<u8> {
        let mut len = 0;
        let p = unsafe { tuxport_vt_take_replies(self.0, &mut len) };
        if len == 0 {
            return Vec::new();
        }
        // SAFETY: vt.c returns len valid bytes, kept until the next feed
        unsafe { std::slice::from_raw_parts(p, len) }.to_vec()
    }

    /// The bells, clipboard write, notification, progress and status reports and clipboard reads
    /// programs sent, the last command the shell's prompt marks saw finish, and whether tmux's
    /// control mode, which vt.c detached, ended, since the last call.
    fn take_effects(&mut self) -> Effects {
        let mut raw = RawEffects {
            bells: 0,
            clipboard: 0,
            clip: std::ptr::null(),
            clip_len: 0,
            notified: false,
            title: std::ptr::null(),
            title_len: 0,
            body: std::ptr::null(),
            body_len: 0,
            progress_state: -1,
            progress: -1,
            statuses: std::ptr::null(),
            statuses_len: 0,
            clipboard_reads: 0,
            clipboard_read_kind: 0,
            command_finished: false,
            command_has_exit: false,
            command_exit: 0,
            command_ms: 0,
            tmux_detached: false,
        };
        unsafe { tuxport_vt_take_effects(self.0, &mut raw) };
        /// The `len` bytes vt.c returned at `p`, which it keeps until the next feed.
        fn bytes<'a>(p: *const u8, len: usize) -> &'a [u8] {
            if p.is_null() || len == 0 {
                return &[];
            }
            // SAFETY: vt.c keeps len bytes at p until the next feed, which needs the lock the
            // caller holds while it reads them
            unsafe { std::slice::from_raw_parts(p, len) }
        }
        let text = |p: *const u8, len: usize| String::from_utf8_lossy(bytes(p, len)).into_owned();
        Effects {
            bell: raw.bells > 0,
            clipboard: match raw.clipboard {
                CLIPBOARD_TEXT => Some(Some(text(raw.clip, raw.clip_len))),
                CLIPBOARD_CLEAR => Some(None),
                _ => None,
            },
            notification: raw
                .notified
                .then(|| (text(raw.title, raw.title_len), text(raw.body, raw.body_len))),
            progress: (raw.progress_state >= 0).then_some((raw.progress_state, raw.progress)),
            statuses: StatusReport::decode(bytes(raw.statuses, raw.statuses_len)),
            clipboard_reads: (raw.clipboard_reads > 0)
                .then_some((raw.clipboard_reads, raw.clipboard_read_kind)),
            command: raw.command_finished.then_some((
                raw.command_has_exit.then_some(raw.command_exit),
                raw.command_ms,
            )),
            tmux_detached: raw.tmux_detached,
        }
    }

    /// Text vt.c returned (`len` bytes at `p`, kept until its next text call) as a Java string;
    /// null for a null `p`. Called with the lock held.
    fn text(&self, env: &mut JNIEnv<'_>, p: *const u8, len: usize) -> jstring {
        if p.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: vt.c keeps len bytes at p until its next text call, which needs the lock
        let text = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(p, len) });
        env.new_string(text)
            .map_or(std::ptr::null_mut(), JString::into_raw)
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        unsafe { tuxport_vt_free(self.0) };
    }
}

/// What programs asked of the app in one read of the console: the bell (any number of BELs
/// counts once), a clipboard write (`Some(None)` clears it), a notification (title, body), the
/// last progress report (OSC 9;4: libghostty-vt's state and the percentage, -1 for none), the
/// program status reports (OSC 7501), in order, clipboard reads (OSC 52 "?": how many, and the
/// clipboard the last named, `c`, `p` or `s`), which the app answers once the user agrees, the
/// last command the shell's prompt marks (OSC 133) saw finish (its exit code when the shell
/// reported one, and how long it ran in milliseconds), and the end of tmux's control mode, which
/// vt.c detached as it started.
struct Effects {
    bell: bool,
    clipboard: Option<Option<String>>,
    notification: Option<(String, String)>,
    progress: Option<(i32, i32)>,
    statuses: Vec<StatusReport>,
    clipboard_reads: Option<(u32, u8)>,
    command: Option<(Option<i32>, u64)>,
    tmux_detached: bool,
}

/// A program status report (OSC 7501), as vt.c queues it: libghostty-vt's state and kind, the
/// percentage (-1 for none), and the record's id, title and message.
struct StatusReport {
    state: i32,
    kind: i32,
    progress: i32,
    id: String,
    title: String,
    message: String,
}

impl StatusReport {
    /// The reports vt.c queued (`STATUS_HEADER_BYTES` in vt.c has the layout).
    fn decode(mut bytes: &[u8]) -> Vec<StatusReport> {
        let mut reports = Vec::new();
        while bytes.len() >= STATUS_HEADER_BYTES {
            let id = usize::from(bytes[3]);
            let title = usize::from(bytes[4]);
            let message = usize::from(u16::from_le_bytes([bytes[5], bytes[6]]));
            let end = STATUS_HEADER_BYTES + id + title + message;
            let Some(text) = bytes.get(STATUS_HEADER_BYTES..end) else {
                break;
            };
            let string = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
            reports.push(StatusReport {
                state: i32::from(bytes[0]),
                kind: i32::from(bytes[1]),
                progress: i32::from(bytes[2] as i8),
                id: string(&text[..id]),
                title: string(&text[id..id + title]),
                message: string(&text[id + title..]),
            });
            bytes = &bytes[end..];
        }
        reports
    }
}

impl Effects {
    /// Whether a program reported its progress or status, which lasts while its process group
    /// keeps the foreground ([`pump`]).
    fn reported(&self) -> bool {
        self.progress.is_some() || !self.statuses.is_empty()
    }

    /// Hands them to the tab's `VtSession` (`onBell`, `onClipboard`, `onNotification`,
    /// `onProgress`, `onStatus` for each status report, `onClipboardRead`, `onCommandFinished`,
    /// `onTmuxDetached`);
    /// false when a call threw. The strings are
    /// local references, deleted after each call: the pump stays in native code for the tab's
    /// whole life, so they would otherwise pile up.
    fn deliver(self, env: &mut JNIEnv<'_>, session: &JObject<'_>) -> bool {
        if self.bell && env.call_method(session, "onBell", "()V", &[]).is_err() {
            return false;
        }
        if let Some(clip) = self.clipboard {
            let text = match clip.map(|t| env.new_string(t)) {
                Some(Ok(s)) => JObject::from(s),
                Some(Err(_)) => return false,
                None => JObject::null(),
            };
            let sig = "(Ljava/lang/String;)V";
            let called = env.call_method(session, "onClipboard", sig, &[JValue::Object(&text)]);
            let _ = env.delete_local_ref(text);
            if called.is_err() {
                return false;
            }
        }
        if let Some((title, body)) = self.notification {
            let (Ok(title), Ok(body)) = (env.new_string(title), env.new_string(body)) else {
                return false;
            };
            let sig = "(Ljava/lang/String;Ljava/lang/String;)V";
            let args = [JValue::Object(&title), JValue::Object(&body)];
            let called = env.call_method(session, "onNotification", sig, &args);
            let _ = env.delete_local_ref(title);
            let _ = env.delete_local_ref(body);
            if called.is_err() {
                return false;
            }
        }
        if let Some((state, percent)) = self.progress {
            let args = [JValue::Int(state), JValue::Int(percent)];
            if env
                .call_method(session, "onProgress", "(II)V", &args)
                .is_err()
            {
                return false;
            }
        }
        for report in self.statuses {
            let (Ok(id), Ok(title), Ok(message)) = (
                env.new_string(report.id),
                env.new_string(report.title),
                env.new_string(report.message),
            ) else {
                return false;
            };
            let sig = "(IIILjava/lang/String;Ljava/lang/String;Ljava/lang/String;)V";
            let args = [
                JValue::Int(report.state),
                JValue::Int(report.kind),
                JValue::Int(report.progress),
                JValue::Object(&id),
                JValue::Object(&title),
                JValue::Object(&message),
            ];
            let called = env.call_method(session, "onStatus", sig, &args);
            let _ = env.delete_local_ref(id);
            let _ = env.delete_local_ref(title);
            let _ = env.delete_local_ref(message);
            if called.is_err() {
                return false;
            }
        }
        if let Some((count, kind)) = self.clipboard_reads {
            let args = [JValue::Int(i32::from(kind)), JValue::Int(count as i32)];
            if env
                .call_method(session, "onClipboardRead", "(II)V", &args)
                .is_err()
            {
                return false;
            }
        }
        if let Some((exit, ms)) = self.command {
            let args = [
                JValue::Bool(u8::from(exit.is_some())),
                JValue::Int(exit.unwrap_or(0)),
                JValue::Long(i64::try_from(ms).unwrap_or(i64::MAX)),
            ];
            if env
                .call_method(session, "onCommandFinished", "(ZIJ)V", &args)
                .is_err()
            {
                return false;
            }
        }
        if self.tmux_detached
            && env
                .call_method(session, "onTmuxDetached", "()V", &[])
                .is_err()
        {
            return false;
        }
        true
    }
}

struct Tab {
    /// This tab's handle in [`TABS`], by which a compression pass is scheduled.
    id: jlong,
    /// The kernel's handle in the crate root's table ([`machine`]).
    machine: jlong,
    console: u32,
    term: Mutex<Term>,
    /// Output changed the terminal since the last frame. The pump tells the view only when it sets
    /// this, so a burst of output asks for one frame; `frame` clears it before capturing.
    dirty: AtomicBool,
}

impl Tab {
    /// Writes input to the console of `m` (this tab's kernel) until all of it is taken; false once
    /// the console is gone.
    fn write(&self, m: &Machine, mut data: &[u8]) -> bool {
        while !data.is_empty() {
            match m.write_console(self.console, data) {
                Ok(n) => data = &data[n..],
                Err(_) => return false,
            }
        }
        true
    }

    /// The terminal, locked until the guard goes; letting it go schedules a compression pass when
    /// the call left work for one.
    fn term(&self) -> TermGuard<'_> {
        TermGuard {
            tab: self,
            term: self.term.lock().unwrap_or_else(PoisonError::into_inner),
        }
    }

    /// The kernel, for the duration of one call; None once it is stopped.
    fn machine(&self) -> Option<Arc<Machine>> {
        machine(self.machine)
    }

    /// One step of this tab's scheduled compression pass, which `may_wait` for the terminal to be
    /// idle: the delay before the next one, None once the pass is over.
    fn compress_step(&self, may_wait: bool) -> Option<Duration> {
        // Not through term(), whose guard would schedule this very pass
        let term = self.term.lock().unwrap_or_else(PoisonError::into_inner);
        match unsafe { tuxport_vt_compress_step(term.0, may_wait) } {
            COMPRESS_STEP => Some(COMPRESS_STEP_GAP),
            COMPRESS_WAIT => Some(COMPRESS_IDLE),
            _ => None,
        }
    }
}

/// A tab's locked terminal ([`Tab::term`]). Every call into vt.c goes through one, so when the
/// lock goes, vt.c can tell whether the call made compression work (output moved rows into the
/// scrollback, a resize, a read that restored compressed pages) and the pass is scheduled then.
struct TermGuard<'a> {
    tab: &'a Tab,
    term: MutexGuard<'a, Term>,
}

impl Deref for TermGuard<'_> {
    type Target = Term;

    fn deref(&self) -> &Term {
        &self.term
    }
}

impl DerefMut for TermGuard<'_> {
    fn deref_mut(&mut self) -> &mut Term {
        &mut self.term
    }
}

impl Drop for TermGuard<'_> {
    fn drop(&mut self) {
        // Still under the tab's lock; the schedule's lock is only ever taken after it
        if unsafe { tuxport_vt_compress_wanted(self.term.0) } {
            compress_later(self.tab.id);
        }
    }
}

static TABS: Mutex<BTreeMap<jlong, Arc<Tab>>> = Mutex::new(BTreeMap::new());
static NEXT_TAB: AtomicI64 = AtomicI64::new(1);

/// Creates the terminal of an open console; returns the tab handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_create<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
    cols: jint,
    rows: jint,
    cell_width: jint,
    cell_height: jint,
) -> jlong {
    guarded!("VtNative.create", 0, {
        if machine(handle).is_none() {
            throw(&mut env, "the machine is stopped");
            return 0;
        }
        let (cols, rows, cw, ch) = size(cols, rows, cell_width, cell_height);
        let raw = unsafe { tuxport_vt_new(cols, rows, cw, ch) };
        if raw.is_null() {
            throw(&mut env, "cannot create a terminal");
            return 0;
        }
        register(handle, console, raw)
    })
}

/// Creates the terminal of an open console from a tab's saved `snapshot` ([`snapshot`]), settled
/// for a new shell (vt.c's `tuxport_vt_restore`); returns the tab handle, 0 when the snapshot
/// cannot be decoded.
///
/// [`snapshot`]: Java_io_github_sajadxdp0_tuxport_VtNative_snapshot
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_restore<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
    snapshot: JByteArray<'l>,
    cols: jint,
    rows: jint,
    cell_width: jint,
    cell_height: jint,
) -> jlong {
    guarded!("VtNative.restore", 0, {
        if machine(handle).is_none() {
            throw(&mut env, "the machine is stopped");
            return 0;
        }
        let Ok(data) = env.convert_byte_array(&snapshot) else {
            return 0;
        };
        let (cols, rows, cw, ch) = size(cols, rows, cell_width, cell_height);
        let raw = unsafe { tuxport_vt_restore(data.as_ptr(), data.len(), cols, rows, cw, ch) };
        if raw.is_null() {
            return 0;
        }
        register(handle, console, raw)
    })
}

/// Registers a new terminal of kernel `machine`'s `console` in [`TABS`]; returns its handle.
fn register(machine: jlong, console: jint, raw: *mut TuxportVt) -> jlong {
    let id = NEXT_TAB.fetch_add(1, Ordering::Relaxed);
    let tab = Tab {
        id,
        machine,
        console: console as u32,
        term: Mutex::new(Term(raw)),
        dirty: AtomicBool::new(false),
    };
    TABS.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, Arc::new(tab));
    id
}

/// Sets the cursor's default shape (`style`, libghostty-vt's `GhosttyCursorStyle`) and whether it
/// blinks, the user's setting; shown at once unless a program chose its own cursor.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_setCursor<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    style: jint,
    blink: jboolean,
) {
    guarded!("VtNative.setCursor", (), {
        if let Some(tab) = tab(handle) {
            unsafe { tuxport_vt_set_cursor(tab.term().0, style, blink != 0) };
        }
    })
}

/// Shows bytes from the app itself (not the program) as if the console had printed them; replies
/// are dropped.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_print<'l>(
    env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    data: JByteArray<'l>,
) {
    guarded!("VtNative.print", (), {
        let Some(tab) = tab(handle) else { return };
        let Ok(data) = env.convert_byte_array(&data) else {
            return;
        };
        let mut term = tab.term();
        term.feed(&data, false);
        // The app's own text asks nothing of it
        term.take_effects();
    })
}

/// Moves the console's output into the terminal until the session ends or the console is closed;
/// calls `session.onOutput()` whenever the screen changed after the last frame was taken,
/// `session.onTitle()` when a program set the title, `onBell`, `onClipboard`, `onNotification`,
/// `onProgress`, `onStatus`, `onClipboardRead`, `onCommandFinished` and `onTmuxDetached` for what
/// programs asked of the app and what the shell's prompt marks told ([`Effects`]), and
/// `onForeground()` once the process group that was in the foreground when progress or a status
/// was last reported no longer is.
///
/// That stands in for the shell's next prompt (OSC 133), which only some shells mark (the guest's
/// login shells, bash, busybox ash and zsh, android/setup): the program status specification drops a
/// program's `working` and `blocked` records when the next prompt starts or the program exits.
/// Without it, a program that exits without clearing them
/// (killed, or interrupted with Ctrl-C) would leave its tab working forever. The group is the one
/// in the foreground after the read that carried the report, so a report from a command that has
/// already exited by then lasts until the next command takes the foreground.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_pump<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    session: JObject<'l>,
) {
    guarded!("VtNative.pump", (), {
        let Some(tab) = tab(handle) else { return };
        let mut buf = vec![0u8; 64 * 1024];
        // The foreground process group when progress or a status was last reported, while that
        // report lasts
        let mut reporter = None;
        loop {
            // Looked up again for every read, so the kernel is not kept once the service stops it
            let Some(m) = tab.machine() else { break };
            let (n, background) = match m.read_console_run(tab.console, &mut buf) {
                Ok((0, _)) | Err(_) => break,
                Ok(read) => read,
            };
            // Whether a password is read now, for the cursor: programs turn echo off before they
            // print their prompt, so it is known by the time this output shows. Asked before the
            // tab's lock is taken, so the kernel's lock is not held under it
            let password = m.console_password_input(tab.console).unwrap_or(false);
            let (replies, titled, effects) = {
                let mut term = tab.term();
                unsafe { tuxport_vt_set_password_input(term.0, password) };
                let replies = term.feed(&buf[..n], background);
                let titled = unsafe { tuxport_vt_take_title_changed(term.0) };
                (replies, titled, term.take_effects())
            };
            // A program waiting for an answer (DA, DSR) does not read further until it comes
            if !replies.is_empty() && !tab.write(&m, &replies) {
                break;
            }
            let mut gave_back = false;
            if effects.reported() {
                reporter = m.console_foreground(tab.console).ok();
            } else if let Some(group) = reporter
                && m.console_foreground(tab.console).ok() != Some(group)
            {
                reporter = None;
                gave_back = true;
            }
            // Not kept across the calls into Java either
            drop(m);
            let output = !tab.dirty.swap(true, Ordering::AcqRel);
            if (output && env.call_method(&session, "onOutput", "()V", &[]).is_err())
                || (titled && env.call_method(&session, "onTitle", "()V", &[]).is_err())
                || !effects.deliver(&mut env, &session)
                || (gave_back
                    && env
                        .call_method(&session, "onForeground", "()V", &[])
                        .is_err())
            {
                let _ = env.exception_clear();
                break;
            }
        }
    })
}

/// The title a program set (OSC 0/2); null when none.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_title<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jstring {
    guarded!("VtNative.title", std::ptr::null_mut(), {
        let Some(tab) = tab(handle) else {
            return std::ptr::null_mut();
        };
        let term = tab.term();
        let mut len = 0;
        let p = unsafe { tuxport_vt_title(term.0, &mut len) };
        term.text(&mut env, p, len)
    })
}

/// Whether the shell is at its prompt (or the input typed at it), as its prompt marks (OSC 133)
/// tell; false without them, on the alternate screen, or when the tab is gone.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_atPrompt<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jboolean {
    guarded!("VtNative.atPrompt", 0, {
        tab(handle).is_some_and(|tab| unsafe { tuxport_vt_at_prompt(tab.term().0) }) as jboolean
    })
}

/// Captures a frame (vt.c's format) of the rows that changed, or of all rows with `full`. The
/// buffer stays valid until the next call, which only the main thread makes.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_frame<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    full: jboolean,
) -> jobject {
    guarded!("VtNative.frame", std::ptr::null_mut(), {
        let Some(tab) = tab(handle) else {
            return std::ptr::null_mut();
        };
        tab.dirty.store(false, Ordering::Release);
        let mut len = 0;
        let p = unsafe { tuxport_vt_frame(tab.term().0, full != 0, &mut len) };
        if p.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: vt.c keeps the frame until the next tuxport_vt_frame call (above)
        match unsafe { env.new_direct_byte_buffer(p as *mut u8, len) } {
            Ok(b) => b.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// Copies the pixels of the Kitty image `id` at `generation`, as a frame named it, into `bitmap`
/// (mutable, `ARGB_8888`, the image's size); false when the image is no longer that generation or
/// size (a later frame names the new one), its pixels have not all arrived, or the bitmap does not
/// fit.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_image<'l>(
    env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    id: jint,
    generation: jlong,
    bitmap: JObject<'l>,
) -> jboolean {
    guarded!("VtNative.image", 0, {
        let Some(tab) = tab(handle) else {
            return 0;
        };
        let raw = env.get_raw();
        let bitmap = bitmap.as_raw();
        let mut info = BitmapInfo {
            width: 0,
            height: 0,
            stride: 0,
            format: 0,
            flags: 0,
        };
        // SAFETY: a live JNIEnv of this thread and a Bitmap reference Kotlin passed
        if unsafe { AndroidBitmap_getInfo(raw, bitmap, &mut info) } != 0
            || info.format != BITMAP_RGBA_8888
        {
            return 0;
        }
        let mut pixels = std::ptr::null_mut();
        // SAFETY: as above; the pixels stay put until they are unlocked below
        if unsafe { AndroidBitmap_lockPixels(raw, bitmap, &mut pixels) } != 0 || pixels.is_null() {
            return 0;
        }
        let copied = unsafe {
            tuxport_vt_image(
                tab.term().0,
                id as u32,
                generation as u64,
                pixels.cast(),
                info.width,
                info.height,
                info.stride as usize,
            )
        };
        // SAFETY: locked above; unlocking also tells the bitmap its pixels changed
        unsafe { AndroidBitmap_unlockPixels(raw, bitmap) };
        copied as jboolean
    })
}

/// Whether the console reads a password now (lines without echo), noted for the frames' cursor;
/// the pump notes it at every read, and the view asks again while it shows the lock, since a
/// program may turn echo back on without printing anything after (sudo prints its line break
/// first). False when the tab or its console is gone.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_passwordInput<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jboolean {
    guarded!("VtNative.passwordInput", 0, {
        let Some(tab) = tab(handle) else {
            return 0;
        };
        let password = tab
            .machine()
            .is_some_and(|m| m.console_password_input(tab.console).unwrap_or(false));
        unsafe { tuxport_vt_set_password_input(tab.term().0, password) };
        password as jboolean
    })
}

/// Resizes the terminal and then the console (whose foreground group gets SIGWINCH); returns the
/// replies the terminal has for the console, such as the in-band size report a program asked for
/// with mode 2048 (empty when there are none, null when the tab is gone), for the caller to write
/// after the resize: the pump would send them only after its next read.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_resize<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    cols: jint,
    rows: jint,
    cell_width: jint,
    cell_height: jint,
) -> jbyteArray {
    guarded!("VtNative.resize", std::ptr::null_mut(), {
        let Some(tab) = tab(handle) else {
            return std::ptr::null_mut();
        };
        let (cols, rows, cw, ch) = size(cols, rows, cell_width, cell_height);
        let replies = {
            let mut term = tab.term();
            unsafe { tuxport_vt_resize(term.0, cols, rows, cw, ch) };
            term.take_replies()
        };
        if let Some(m) = tab.machine() {
            let _ = m.resize_console(tab.console, rows, cols);
        }
        bytes_array(&mut env, &replies)
    })
}

fn size(cols: jint, rows: jint, cell_width: jint, cell_height: jint) -> (u16, u16, u32, u32) {
    (
        cols.clamp(1, 0xffff) as u16,
        rows.clamp(1, 0xffff) as u16,
        cell_width.max(1) as u32,
        cell_height.max(1) as u32,
    )
}

/// Encodes a key for the program in the terminal's current modes (cursor keys, keypad, Kitty
/// keyboard protocol); null when it fails. `text` is what the key types without Ctrl and Alt.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_key<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    key: jint,
    mods: jint,
    consumed: jint,
    action: jint,
    text: JString<'l>,
    unshifted: jint,
) -> jbyteArray {
    guarded!("VtNative.key", std::ptr::null_mut(), {
        let Some(tab) = tab(handle) else {
            return std::ptr::null_mut();
        };
        let text = if text.is_null() {
            String::new()
        } else {
            string(&mut env, &text).unwrap_or_default()
        };
        let mut out = [0u8; 128];
        let n = unsafe {
            tuxport_vt_key(
                tab.term().0,
                key,
                mods as u16,
                consumed as u16,
                action,
                if text.is_empty() {
                    std::ptr::null()
                } else {
                    text.as_ptr()
                },
                text.len(),
                unshifted as u32,
                out.as_mut_ptr(),
                out.len(),
            )
        };
        encoded(&mut env, &out, n)
    })
}

/// Pastes clipboard text as the terminal's modes ask (bracketed paste, line breaks, unsafe
/// control bytes); returns the bytes for the console, or null when the text could run commands
/// and `allow_unsafe` is false (the view asks first and tries again). An empty array when it
/// failed.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_paste<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    text: JString<'l>,
    allow_unsafe: jboolean,
) -> jbyteArray {
    guarded!("VtNative.paste", std::ptr::null_mut(), {
        let Some(tab) = tab(handle) else {
            return std::ptr::null_mut();
        };
        let Some(text) = string(&mut env, &text) else {
            return std::ptr::null_mut();
        };
        let bytes = {
            let mut term = tab.term();
            match unsafe { tuxport_vt_paste(term.0, text.as_ptr(), text.len(), allow_unsafe != 0) }
            {
                0 => term.take_replies(),
                1 => return std::ptr::null_mut(),
                _ => Vec::new(),
            }
        };
        bytes_array(&mut env, &bytes)
    })
}

/// Encodes a mouse event at pixel (`x`, `y`) of the grid; empty when the program does not take
/// it, null when encoding failed. `action`: 0 press, 1 release, 2 motion; `button`: 0 none,
/// 1 left, 2 right, 3 middle, 4 and 5 the wheel up and down, 6 and 7 left and right.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_mouse<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    action: jint,
    button: jint,
    mods: jint,
    x: jfloat,
    y: jfloat,
    pressed: jboolean,
) -> jbyteArray {
    guarded!("VtNative.mouse", std::ptr::null_mut(), {
        let Some(tab) = tab(handle) else {
            return std::ptr::null_mut();
        };
        let mut out = [0u8; 64];
        let n = unsafe {
            tuxport_vt_mouse(
                tab.term().0,
                action,
                button,
                mods as u16,
                x,
                y,
                pressed != 0,
                out.as_mut_ptr(),
                out.len(),
            )
        };
        encoded(&mut env, &out, n)
    })
}

/// Encodes focus gained or lost; empty when the program did not ask for focus events.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_focus<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    gained: jboolean,
) -> jbyteArray {
    guarded!("VtNative.focus", std::ptr::null_mut(), {
        let Some(tab) = tab(handle) else {
            return std::ptr::null_mut();
        };
        let mut out = [0u8; 16];
        let n = unsafe { tuxport_vt_focus(tab.term().0, gained != 0, out.as_mut_ptr(), out.len()) };
        encoded(&mut env, &out, n)
    })
}

/// The first `n` bytes of an encoder's output, null when it failed (`n` < 0).
fn encoded(env: &mut JNIEnv<'_>, out: &[u8], n: isize) -> jbyteArray {
    match usize::try_from(n).ok().and_then(|n| out.get(..n)) {
        Some(bytes) => bytes_array(env, bytes),
        None => std::ptr::null_mut(),
    }
}

fn bytes_array(env: &mut JNIEnv<'_>, bytes: &[u8]) -> jbyteArray {
    env.byte_array_from_slice(bytes)
        .map_or(std::ptr::null_mut(), JByteArray::into_raw)
}

/// Scrolls the viewport by `delta` rows (negative: back); 0 returns to the bottom.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_scroll<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    delta: jint,
) {
    guarded!("VtNative.scroll", (), {
        if let Some(tab) = tab(handle) {
            unsafe { tuxport_vt_scroll(tab.term().0, delta) };
        }
    })
}

/// Scrolls the viewport by `delta` prompts (negative: back), as the shell's prompt marks
/// (OSC 133) find them; nothing without them.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_jumpPrompt<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    delta: jint,
) {
    guarded!("VtNative.jumpPrompt", (), {
        if let Some(tab) = tab(handle) {
            unsafe { tuxport_vt_jump_prompt(tab.term().0, delta) };
        }
    })
}

/// Starts a selection gesture at viewport cell (`x`, `y`), pixel (`px`, `py`) from the grid's
/// corner: selects the word or the line there (`behavior`, libghostty-vt's
/// `GhosttySelectionGestureBehavior`); false when there is none (the selection is then cleared).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_gesturePress<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    behavior: jint,
    x: jint,
    y: jint,
    px: jfloat,
    py: jfloat,
) -> jboolean {
    guarded!("VtNative.gesturePress", 0, {
        tab(handle).is_some_and(|tab| unsafe {
            tuxport_vt_gesture_press(
                tab.term().0,
                behavior,
                cell(x),
                cell(y),
                px.into(),
                py.into(),
            )
        }) as jboolean
    })
}

/// Drags the selection gesture on to viewport cell (`x`, `y`), pixel (`px`, `py`), extending the
/// selection by words or lines; false when the selection stays as it was.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_gestureDrag<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    x: jint,
    y: jint,
    px: jfloat,
    py: jfloat,
) -> jboolean {
    guarded!("VtNative.gestureDrag", 0, {
        tab(handle).is_some_and(|tab| unsafe {
            tuxport_vt_gesture_drag(tab.term().0, cell(x), cell(y), px.into(), py.into())
        }) as jboolean
    })
}

/// Ends the selection gesture, keeping its selection.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_gestureEnd<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    guarded!("VtNative.gestureEnd", (), {
        if let Some(tab) = tab(handle) {
            unsafe { tuxport_vt_gesture_end(tab.term().0) };
        }
    })
}

/// A mouse's left button pressed at viewport cell (`x`, `y`), pixel (`px`, `py`), at `time_ns`:
/// a timed press of the selection gesture, which counts the clicks (`interval_ns` apart at most).
/// Returns the click count, 0 when it failed.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_mousePress<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    x: jint,
    y: jint,
    px: jfloat,
    py: jfloat,
    time_ns: jlong,
    interval_ns: jlong,
    output: jboolean,
) -> jint {
    guarded!("VtNative.mousePress", 0, {
        tab(handle).map_or(0, |tab| unsafe {
            tuxport_vt_mouse_press(
                tab.term().0,
                cell(x),
                cell(y),
                px.into(),
                py.into(),
                time_ns.max(0) as u64,
                interval_ns.max(0) as u64,
                output != 0,
            )
        })
    })
}

/// The held left button dragged on to viewport cell (`x`, `y`), pixel (`px`, `py`): 1 with a
/// selection, 0 without, -1 when no press is under way.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_mouseDrag<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    x: jint,
    y: jint,
    px: jfloat,
    py: jfloat,
    rectangle: jboolean,
) -> jint {
    guarded!("VtNative.mouseDrag", -1, {
        tab(handle).map_or(-1, |tab| unsafe {
            tuxport_vt_mouse_drag(
                tab.term().0,
                cell(x),
                cell(y),
                px.into(),
                py.into(),
                rectangle != 0,
            )
        })
    })
}

/// The left button released at viewport cell (`x`, `y`) (out of the grid: moved away): the click
/// count, negated after a drag, 0 without a press.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_mouseRelease<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    x: jint,
    y: jint,
) -> jint {
    guarded!("VtNative.mouseRelease", 0, {
        tab(handle).map_or(0, |tab| unsafe {
            tuxport_vt_mouse_release(tab.term().0, x, y)
        })
    })
}

/// A right-click at viewport cell (`x`, `y`): keeps a selection holding it, else selects the word
/// there; true when there is a selection afterwards.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_selectContext<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    x: jint,
    y: jint,
) -> jboolean {
    guarded!("VtNative.selectContext", 0, {
        tab(handle)
            .is_some_and(|tab| unsafe { tuxport_vt_select_context(tab.term().0, cell(x), cell(y)) })
            as jboolean
    })
}

/// Starts dragging the handle of the selection's last cell (`end`) or first cell: the other
/// end becomes the anchor. False when there is no selection.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_selectGrab<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    end: jboolean,
) -> jboolean {
    guarded!("VtNative.selectGrab", 0, {
        tab(handle).is_some_and(|tab| unsafe { tuxport_vt_select_grab(tab.term().0, end != 0) })
            as jboolean
    })
}

/// Extends the selection from its anchor to viewport cell (`x`, `y`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_selectTo<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    x: jint,
    y: jint,
) -> jboolean {
    guarded!("VtNative.selectTo", 0, {
        tab(handle)
            .is_some_and(|tab| unsafe { tuxport_vt_select_to(tab.term().0, cell(x), cell(y)) })
            as jboolean
    })
}

/// Moves the selection's end by `adjustment` (libghostty-vt's `GhosttySelectionAdjust`) and
/// scrolls the viewport to show it; false without a selection.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_selectAdjust<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    adjustment: jint,
) -> jboolean {
    guarded!("VtNative.selectAdjust", 0, {
        tab(handle).is_some_and(|tab| unsafe { tuxport_vt_select_adjust(tab.term().0, adjustment) })
            as jboolean
    })
}

/// A viewport coordinate from Kotlin, clamped to the grid's range (vt.c clamps to the grid).
fn cell(v: jint) -> u16 {
    v.clamp(0, 0xffff) as u16
}

/// Makes the selection a rectangle (`on`) with its ends at opposite corners, or linear again;
/// false when there is no selection.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_selectRectangle<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    on: jboolean,
) -> jboolean {
    guarded!("VtNative.selectRectangle", 0, {
        tab(handle).is_some_and(|tab| unsafe { tuxport_vt_select_rectangle(tab.term().0, on != 0) })
            as jboolean
    })
}

/// Selects everything, scrollback included.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_selectAll<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jboolean {
    guarded!("VtNative.selectAll", 0, {
        tab(handle).is_some_and(|tab| unsafe { tuxport_vt_select_all(tab.term().0) }) as jboolean
    })
}

/// Whether viewport cell (`x`, `y`) is in a command's output, as the shell's prompt marks
/// (OSC 133) delimit it; if so, [`selectOutput`] selects that output.
///
/// [`selectOutput`]: Java_io_github_sajadxdp0_tuxport_VtNative_selectOutput
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_outputAt<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    x: jint,
    y: jint,
) -> jboolean {
    guarded!("VtNative.outputAt", 0, {
        tab(handle)
            .is_some_and(|tab| unsafe { tuxport_vt_output_at(tab.term().0, cell(x), cell(y)) })
            as jboolean
    })
}

/// Selects the whole output the last [`outputAt`] found and anchors drags to it; false when it is
/// gone.
///
/// [`outputAt`]: Java_io_github_sajadxdp0_tuxport_VtNative_outputAt
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_selectOutput<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jboolean {
    guarded!("VtNative.selectOutput", 0, {
        tab(handle).is_some_and(|tab| unsafe { tuxport_vt_select_output(tab.term().0) }) as jboolean
    })
}

/// Clears the selection.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_selectClear<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    guarded!("VtNative.selectClear", (), {
        if let Some(tab) = tab(handle) {
            unsafe { tuxport_vt_select_clear(tab.term().0) };
        }
    })
}

/// The selection as plain text (soft wraps joined, trailing blanks trimmed); null when there is
/// no selection or it is empty.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_copy<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jstring {
    guarded!("VtNative.copy", std::ptr::null_mut(), {
        let Some(tab) = tab(handle) else {
            return std::ptr::null_mut();
        };
        let term = tab.term();
        let mut len = 0;
        let p = unsafe { tuxport_vt_copy(term.0, &mut len) };
        term.text(&mut env, p, len)
    })
}

/// The link at viewport cell (`x`, `y`): its OSC 8 URI, else an http or https URL in the text
/// there; null when there is none.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_link<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    x: jint,
    y: jint,
) -> jstring {
    guarded!("VtNative.link", std::ptr::null_mut(), {
        let Some(tab) = tab(handle) else {
            return std::ptr::null_mut();
        };
        let term = tab.term();
        let mut len = 0;
        let p = unsafe { tuxport_vt_link(term.0, cell(x), cell(y), &mut len) };
        term.text(&mut env, p, len)
    })
}

/// A tap on viewport cell (`x`, `y`) at the shell's prompt: the bytes that move the shell's cursor
/// there (arrow keys, or a mouse press for a shell that places it itself), empty when it need not
/// move; null when the tap is no prompt's (the prompt declared no way to move the cursor, the
/// cursor is not at the prompt, the cell is above it) or the tab is gone.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_promptClick<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    x: jint,
    y: jint,
) -> jbyteArray {
    guarded!("VtNative.promptClick", std::ptr::null_mut(), {
        let Some(tab) = tab(handle) else {
            return std::ptr::null_mut();
        };
        let bytes = {
            let mut term = tab.term();
            if !unsafe { tuxport_vt_prompt_click(term.0, cell(x), cell(y)) } {
                return std::ptr::null_mut();
            }
            term.take_replies()
        };
        bytes_array(&mut env, &bytes)
    })
}

/// Searches the screen and its scrollback for `needle` (ASCII letters in either case), restarting
/// when it changed; empty finds nothing. The frames that follow find the matches and carry their
/// count and marks. False when the search cannot be made.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_search<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    needle: JString<'l>,
) -> jboolean {
    guarded!("VtNative.search", 0, {
        let (Some(tab), Some(needle)) = (tab(handle), string(&mut env, &needle)) else {
            return 0;
        };
        (unsafe { tuxport_vt_search(tab.term().0, needle.as_ptr(), needle.len()) }) as jboolean
    })
}

/// Selects the next match towards older output (`older`) or newer, going round, and scrolls the
/// viewport to it when needed; false when there is none.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_searchSelect<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    older: jboolean,
) -> jboolean {
    guarded!("VtNative.searchSelect", 0, {
        tab(handle).is_some_and(|tab| unsafe { tuxport_vt_search_select(tab.term().0, older != 0) })
            as jboolean
    })
}

/// The scroll bar rows the search's matches start on, for marks along it: the selected match's
/// first (-1 for none), then each row a match starts on once, newest first (none without a
/// search); null without the tab.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_searchRows<'l>(
    env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jintArray {
    guarded!("VtNative.searchRows", std::ptr::null_mut(), {
        let Some(tab) = tab(handle) else {
            return std::ptr::null_mut();
        };
        let term = tab.term();
        let mut len = 0usize;
        let mut selected = -1i64;
        let p = unsafe { tuxport_vt_search_rows(term.0, &mut len, &mut selected) };
        let mut rows = Vec::with_capacity(len + 1);
        rows.push(i32::try_from(selected).unwrap_or(-1));
        if !p.is_null() {
            // SAFETY: vt.c keeps len rows at p until its next call, which needs the lock held here
            let found = unsafe { std::slice::from_raw_parts(p, len) };
            rows.extend(found.iter().map(|&r| i32::try_from(r).unwrap_or(i32::MAX)));
        }
        drop(term);
        let Ok(size) = i32::try_from(rows.len()) else {
            return std::ptr::null_mut();
        };
        let Ok(array) = env.new_int_array(size) else {
            return std::ptr::null_mut();
        };
        if env.set_int_array_region(&array, 0, &rows).is_err() {
            return std::ptr::null_mut();
        }
        array.into_raw()
    })
}

/// Ends the search; the next frame takes its marks away.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_searchEnd<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    guarded!("VtNative.searchEnd", (), {
        if let Some(tab) = tab(handle) {
            unsafe { tuxport_vt_search_end(tab.term().0) };
        }
    })
}

/// Clears the screen and the scrollback as Ghostty's clear_screen does: 1 when it cleared at the
/// shell's prompt (the caller sends the shell a form feed to draw it again), 0 when it cleared
/// elsewhere, -1 on the alternate screen or without the tab, where nothing changes.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_clear<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jint {
    guarded!("VtNative.clear", -1, {
        tab(handle).map_or(-1, |tab| unsafe { tuxport_vt_clear(tab.term().0) })
    })
}

/// Resets the terminal (a full reset, RIS); true when the shell was at its prompt (the caller
/// sends it a form feed to draw it again).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_reset<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jboolean {
    guarded!("VtNative.reset", 0, {
        tab(handle).is_some_and(|tab| unsafe { tuxport_vt_reset(tab.term().0) }) as jboolean
    })
}

/// Shows a yellow line of app notice above the cursor's line; false on the alternate screen.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_message<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    text: JString<'l>,
) -> jboolean {
    guarded!("VtNative.message", 0, {
        let (Some(tab), Some(text)) = (tab(handle), string(&mut env, &text)) else {
            return 0;
        };
        (unsafe { tuxport_vt_message(tab.term().0, text.as_ptr(), text.len()) }) as jboolean
    })
}

/// The primary screen's text with its scrollback (soft wraps joined, trailing blanks trimmed);
/// null on the alternate screen, when nothing changed the screen since the last transcript (whose
/// text the caller keeps), or on failure.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_transcript<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jstring {
    guarded!("VtNative.transcript", std::ptr::null_mut(), {
        let Some(tab) = tab(handle) else {
            return std::ptr::null_mut();
        };
        let term = tab.term();
        let mut len = 0;
        let p = unsafe { tuxport_vt_transcript(term.0, &mut len) };
        term.text(&mut env, p, len)
    })
}

/// A snapshot of the whole terminal for saving the tab, which [`restore`] brings back; null when
/// nothing changed since the last one (whose bytes the caller keeps) or the tab is gone, empty
/// when none can be taken of the terminal as it is (vt.c's `tuxport_vt_snapshot`).
///
/// [`restore`]: Java_io_github_sajadxdp0_tuxport_VtNative_restore
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_snapshot<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jbyteArray {
    guarded!("VtNative.snapshot", std::ptr::null_mut(), {
        let Some(tab) = tab(handle) else {
            return std::ptr::null_mut();
        };
        let mut data = std::ptr::null_mut();
        let mut len = 0;
        let taken = unsafe { tuxport_vt_snapshot(tab.term().0, &mut data, &mut len) };
        match taken {
            0 => std::ptr::null_mut(),
            1 => {
                // SAFETY: vt.c returned len bytes at data, ours until freed below
                let array = bytes_array(&mut env, unsafe { std::slice::from_raw_parts(data, len) });
                unsafe { tuxport_vt_free_snapshot(data, len) };
                array
            }
            _ => bytes_array(&mut env, &[]),
        }
    })
}

/// The version saved snapshots are recorded with: libghostty-vt's. A snapshot from another version
/// is not decoded: the snapshot format is versioned apart from the library and is not covered by
/// its ABI promise.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_version<'l>(
    env: JNIEnv<'l>,
    _: JClass<'l>,
) -> jstring {
    guarded!("VtNative.version", std::ptr::null_mut(), {
        let mut len = 0;
        let p = unsafe { tuxport_vt_version(&mut len) };
        let lib = if p.is_null() {
            String::new()
        } else {
            // SAFETY: vt.c returns len bytes of libghostty-vt's static version string
            String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(p, len) }).into_owned()
        };
        env.new_string(lib)
            .map_or(std::ptr::null_mut(), JString::into_raw)
    })
}

/// Compresses the cold scrollback of every tab at once, idle or not (the app trimming its memory),
/// so a tab whose output never pauses is compressed too.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_compressAll<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
) {
    guarded!("VtNative.compressAll", (), {
        let tabs: Vec<Arc<Tab>> = TABS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for tab in tabs {
            // Not through term(): the pass leaves nothing to schedule
            let term = tab.term.lock().unwrap_or_else(PoisonError::into_inner);
            unsafe { tuxport_vt_compress_all(term.0) };
        }
    })
}

/// Cuts a tab's Kitty images down to a few MiB per screen, the oldest going first (the app trimming
/// its memory under pressure, for a tab not shown).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_dropImages<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    guarded!("VtNative.dropImages", (), {
        if let Some(tab) = tab(handle) {
            unsafe { tuxport_vt_drop_images(tab.term().0) };
        }
    })
}

/// Forgets a tab; its terminal is freed once a running pump has returned too.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_VtNative_destroy<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    guarded!("VtNative.destroy", (), {
        let removed = TABS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&handle);
        drop(removed);
    })
}

fn tab(handle: jlong) -> Option<Arc<Tab>> {
    TABS.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&handle)
        .cloned()
}

/// Tabs with a compression pass scheduled, by handle.
static COMPRESS: Mutex<BTreeMap<jlong, Due>> = Mutex::new(BTreeMap::new());
static COMPRESS_WAKE: Condvar = Condvar::new();
static COMPRESSOR: Once = Once::new();

/// A scheduled compression pass: when [`compressor`] looks at it next, and when it was scheduled.
#[derive(Clone, Copy)]
struct Due {
    at: Instant,
    since: Instant,
}

/// Schedules tab `id`'s compression pass an idle interval from now; the first call starts the
/// `vt-compress` thread. vt.c has marked the pass scheduled, so this comes once per pass.
fn compress_later(id: jlong) {
    COMPRESSOR.call_once(|| {
        if let Err(e) = thread::Builder::new()
            .name("vt-compress".into())
            .spawn(compressor)
        {
            eprintln!("tuxport: cannot start the scrollback compressor: {e}");
        }
    });
    let now = Instant::now();
    let due = Due {
        at: now + COMPRESS_IDLE,
        since: now,
    };
    COMPRESS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, due);
    COMPRESS_WAKE.notify_one();
}

/// The `vt-compress` thread (vt.c's "Scrollback compression"): takes the scheduled tab due first
/// and runs one step of its pass, which waits for the terminal to stay unchanged for
/// [`COMPRESS_IDLE`] (for at most [`COMPRESS_MAX_WAIT`]) and then compresses in bounded steps
/// [`COMPRESS_STEP_GAP`] apart, releasing the tab's lock between them. The schedule's lock is not
/// held during a step, so scheduling, which happens under a tab's lock, never waits for one.
fn compressor() {
    let mut due = COMPRESS.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        let Some((id, next)) = due
            .iter()
            .min_by_key(|&(_, next)| next.at)
            .map(|(&id, &next)| (id, next))
        else {
            due = COMPRESS_WAKE
                .wait(due)
                .unwrap_or_else(PoisonError::into_inner);
            continue;
        };
        let now = Instant::now();
        if next.at > now {
            due = COMPRESS_WAKE
                .wait_timeout(due, next.at - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            continue;
        }
        due.remove(&id);
        drop(due);
        let may_wait = now - next.since < COMPRESS_MAX_WAIT;
        // A tab closed meanwhile is dropped from the schedule
        let again = tab(id).and_then(|tab| tab.compress_step(may_wait));
        due = COMPRESS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(delay) = again {
            let at = Instant::now() + delay;
            due.insert(id, Due { at, ..next });
        }
    }
}
