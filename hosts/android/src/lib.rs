//! Tuxport's Android native library (design doc §3, the android/native map): the Kotlin shell uses
//! the kernel layer through JNI.
//!
//! The matching Kotlin declarations are in
//! `android/app/src/main/java/io/github/sajadxdp0/tuxport/Native.kt`.
//! [`Machine`]s are registered in a table, and Kotlin gets a number (a `long` handle). Each call
//! takes a temporary reference, so `stop` is safe even concurrently with a read blocked on another
//! thread: the Machine is dropped only when the last reference goes away. `stop` removes it from
//! the table, and later calls with the handle (such as `closeConsole`, cleaned up only after the
//! session ends) do not find it and treat the console as closed.
//! Errors throw `java.io.IOException`. Blocking calls (reading a console, waiting for a process to
//! end) are made by Kotlin on background threads. Kernel-layer bugs (panics) are meant to be caught
//! inside the [`Machine`] methods and become errors of these calls, so that they do not cross the
//! JNI boundary and abort the app process. Only the methods that wrap their work in the kernel's
//! `host_call` do so; a panic in an unwrapped one (such as [`Machine::wait`] or the wait loops of
//! console reads) or in this crate's own code is caught by every JNI function ([`guarded`]) and
//! fails that call instead.
//!
//! Compiled only for Android; on other hosts it is an empty library.
#![cfg(target_os = "android")]

use std::collections::BTreeMap;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Once, PoisonError};

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JObjectArray, JString};
use jni::sys::{jboolean, jbooleanArray, jbyteArray, jint, jlong, jobjectArray, jstring};
use tuxport_kernel::vfs::browse;
use tuxport_kernel::{ExitStatus, HostRoute, Machine, MachineOptions, Power, Stdio, Tier};

/// Runs the body of a JNI function. A panic in it (a kernel-layer bug outside the calls the kernel
/// wraps itself, or one in this crate) would unwind out of `extern "system"` and abort the app
/// with every session in it; it is caught instead, logged to logcat after the panic message the
/// panic hook writes to stderr, and the call returns `fallback`, which Kotlin takes as a failure of
/// that call: an `IOException` with `throws env` (the functions Kotlin declares `@Throws`), an
/// ended console or process, or an empty answer.
macro_rules! guarded {
    ($function:literal, $fallback:expr, $body:block) => {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || $body)) {
            Ok(r) => r,
            Err(_) => {
                crate::panicked($function, None);
                $fallback
            }
        }
    };
    ($function:literal, throws $env:ident, $fallback:expr, $body:block) => {{
        let raw = $env.get_raw();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || $body)) {
            Ok(r) => r,
            Err(_) => {
                crate::panicked($function, Some(raw));
                $fallback
            }
        }
    }};
}

mod vt;

/// A JNI function's body panicked ([`guarded`]): logs it, and when Kotlin expects an exception
/// from the call, throws one unless one is already pending.
fn panicked(function: &str, env: Option<*mut jni::sys::JNIEnv>) {
    eprintln!("tuxport: internal error in {function} (a panic, logged above); the call failed");
    // SAFETY: the pointer is the JNIEnv this call was made with, on this thread
    if let Some(Ok(mut env)) = env.map(|raw| unsafe { JNIEnv::from_raw(raw) })
        && !env.exception_check().unwrap_or(true)
    {
        throw(&mut env, format!("internal error in {function}"));
    }
}

/// Called before any other function: stderr goes to logcat, and with `verbose` the kernel layer's
/// debug log is turned on.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_init<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    verbose: jboolean,
) {
    guarded!("Native.init", (), {
        redirect_stderr();
        if verbose != 0 {
            // SAFETY: set before the kernel layer is used; no other thread reads the environment yet
            unsafe { std::env::set_var("TUXPORT_LOG", "1") };
        }
    })
}

/// Sends stderr (kernel-layer logs, internal errors) to logcat.
fn redirect_stderr() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        #[link(name = "log")]
        unsafe extern "C" {
            fn __android_log_write(
                prio: i32,
                tag: *const libc::c_char,
                text: *const libc::c_char,
            ) -> i32;
        }
        const ANDROID_LOG_INFO: i32 = 4;
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 || unsafe { libc::dup2(fds[1], 2) } < 0 {
            return;
        }
        unsafe { libc::close(fds[1]) };
        let read_end = fds[0];
        let _ = std::thread::Builder::new()
            .name("stderr-log".into())
            .spawn(move || {
                use std::io::{BufRead, BufReader};
                use std::os::fd::FromRawFd;
                let input = unsafe { std::fs::File::from_raw_fd(read_end) };
                let tag = c"tuxport";
                for line in BufReader::new(input).split(b'\n').map_while(Result::ok) {
                    if let Ok(text) = CString::new(line) {
                        unsafe {
                            __android_log_write(ANDROID_LOG_INFO, tag.as_ptr(), text.as_ptr())
                        };
                    }
                }
            });
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_importRootfs<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    tarball: JString<'l>,
) {
    guarded!("Native.importRootfs", throws env, (), {
        let (Some(store), Some(tarball)) = (string(&mut env, &store), string(&mut env, &tarball))
        else {
            return;
        };
        if let Err(e) = tuxport_kernel::import_rootfs(store.as_ref(), tarball.as_ref()) {
            throw(&mut env, format!("cannot import the rootfs: {e}"));
        }
    })
}

/// Writes the store to disk (after an import and writing the configuration, before renaming it into
/// place).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_syncRootfs<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
) {
    guarded!("Native.syncRootfs", throws env, (), {
        let Some(store) = string(&mut env, &store) else {
            return;
        };
        if let Err(e) = tuxport_kernel::sync_store(store.as_ref()) {
            throw(&mut env, format!("cannot sync the rootfs: {e}"));
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_writeFile<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    path: JString<'l>,
    data: JByteArray<'l>,
    mode: jint,
    overwrite: jboolean,
) {
    guarded!("Native.writeFile", throws env, (), {
        let (Some(store), Some(path)) = (string(&mut env, &store), string(&mut env, &path)) else {
            return;
        };
        let Ok(data) = env.convert_byte_array(&data) else {
            return;
        };
        let r = tuxport_kernel::write_file(store.as_ref(), &path, &data, mode as u32, overwrite != 0);
        if let Err(e) = r {
            throw(&mut env, format!("cannot write {path}: {e}"));
        }
    })
}

/// Reads a regular file in the store (for the host to look at configuration files, such as listing
/// accounts in Settings): returns null when it does not exist and throws IOException for other
/// errors. Also works while the kernel runs: when the store is opened separately, the kernel's
/// deferred write transaction commits first.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_readFile<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    path: JString<'l>,
) -> jbyteArray {
    guarded!("Native.readFile", throws env, std::ptr::null_mut(), {
        let (Some(store), Some(path)) = (string(&mut env, &store), string(&mut env, &path)) else {
            return std::ptr::null_mut();
        };
        match tuxport_kernel::read_file(store.as_ref(), &path) {
            Ok(data) => env
                .byte_array_from_slice(&data)
                .map(JByteArray::into_raw)
                .unwrap_or(std::ptr::null_mut()),
            Err(tuxport_kernel::errno::Errno::ENOENT) => std::ptr::null_mut(),
            Err(e) => {
                throw(&mut env, format!("cannot read {path}: {e}"));
                std::ptr::null_mut()
            }
        }
    })
}

// ---- The host browsing the store (DocumentsProvider, the android/documents map) ----
//
// On the Java side paths are strings (document IDs): guest file names are arbitrary bytes; the
// valid UTF-8 parts stay as they are, while `%`, control characters (each line of a list is
// separated by tabs) and bytes that are not UTF-8 are written as `%XX`, so they convert back
// exactly.
// Each entry is `<type d|f>\t<bytes>\t<modification time (ms)>\t<permission bits>`; list and search
// results are also prefixed with the name or path and a tab. Errors throw FileNotFoundException (a
// subclass of IOException).

/// The most entries (directory entries) one search looks at: it does not search forever in a big
/// tree like /usr.
const SEARCH_BUDGET: usize = 50_000;

/// The directory or regular file a path (document ID) points at (following symbolic links).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_browseStat<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    path: JString<'l>,
) -> jstring {
    guarded!("Native.browseStat", throws env, std::ptr::null_mut(), {
        let (Some(store), Some(path)) = (string(&mut env, &store), string(&mut env, &path)) else {
            return std::ptr::null_mut();
        };
        let r = tuxport_kernel::browse(store.as_ref(), |s| {
            browse::stat(s, &browse::decode_path(&path))
        });
        match r {
            Ok(st) => env
                .new_string(stat_fields(&st))
                .map(|s| s.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            Err(e) => {
                not_found(&mut env, "find", &path, e);
                std::ptr::null_mut()
            }
        }
    })
}

/// Lists a directory: each entry is `<name>\t<type>\t…`, with the name written by
/// [`browse::encode_path`].
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_browseList<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    dir: JString<'l>,
) -> jobjectArray {
    guarded!("Native.browseList", throws env, std::ptr::null_mut(), {
        let (Some(store), Some(dir)) = (string(&mut env, &store), string(&mut env, &dir)) else {
            return std::ptr::null_mut();
        };
        let r = tuxport_kernel::browse(store.as_ref(), |s| {
            browse::list(s, &browse::decode_path(&dir))
        });
        match r {
            Ok(entries) => {
                let lines: Vec<String> = entries
                    .iter()
                    .map(|e| format!("{}\t{}", browse::encode_path(&e.name), stat_fields(&e.stat)))
                    .collect();
                string_array(&mut env, &lines)
            }
            Err(e) => {
                not_found(&mut env, "list", &dir, e);
                std::ptr::null_mut()
            }
        }
    })
}

/// Opens a regular file for reading and returns the host fd (handed to
/// ParcelFileDescriptor.adoptFd, which closes it).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_browseOpen<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    path: JString<'l>,
) -> jint {
    guarded!("Native.browseOpen", throws env, -1, {
        use std::os::fd::IntoRawFd;
        let (Some(store), Some(path)) = (string(&mut env, &store), string(&mut env, &path)) else {
            return -1;
        };
        let r = tuxport_kernel::browse(store.as_ref(), |s| {
            browse::open(s, &browse::decode_path(&path))
        });
        match r {
            Ok(file) => file.into_raw_fd(),
            Err(e) => {
                not_found(&mut env, "open", &path, e);
                -1
            }
        }
    })
}

/// Whether the regular files at these paths look like text ([`browse::looks_like_text`]), all
/// checked with one opening of the store. Only the first 1000 are looked at; the rest count as not
/// text.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_browseSniff<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    paths: JObjectArray<'l>,
) -> jbooleanArray {
    guarded!("Native.browseSniff", throws env, std::ptr::null_mut(), {
        /// The most files looked at in one call: each has to be opened on the host and 4 KB read, so a
        /// big directory cannot go on forever.
        const SNIFF_MAX: usize = 1000;
        let (Some(store), Some(paths)) = (string(&mut env, &store), strings(&mut env, &paths)) else {
            return std::ptr::null_mut();
        };
        let r = tuxport_kernel::browse(store.as_ref(), |s| {
            Ok(paths
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let p = browse::decode_path(&String::from_utf8_lossy(p));
                    jboolean::from(i < SNIFF_MAX && browse::looks_like_text(s, &p))
                })
                .collect::<Vec<jboolean>>())
        });
        let texts = match r {
            Ok(t) => t,
            Err(e) => {
                not_found(&mut env, "open", &store, e);
                return std::ptr::null_mut();
            }
        };
        let Ok(array) = env.new_boolean_array(texts.len() as jint) else {
            return std::ptr::null_mut();
        };
        if env.set_boolean_array_region(&array, 0, &texts).is_err() {
            return std::ptr::null_mut();
        }
        array.into_raw()
    })
}

/// Searches by name under a directory (case-insensitive): each entry is `<path>\t<type>\t…`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_browseSearch<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    dir: JString<'l>,
    query: JString<'l>,
    limit: jint,
) -> jobjectArray {
    guarded!("Native.browseSearch", throws env, std::ptr::null_mut(), {
        let (Some(store), Some(dir), Some(query)) = (
            string(&mut env, &store),
            string(&mut env, &dir),
            string(&mut env, &query),
        ) else {
            return std::ptr::null_mut();
        };
        let r = tuxport_kernel::browse(store.as_ref(), |s| {
            browse::search(
                s,
                &browse::decode_path(&dir),
                &query,
                limit.max(0) as usize,
                SEARCH_BUDGET,
            )
        });
        match r {
            Ok(found) => {
                let lines: Vec<String> = found
                    .iter()
                    .map(|(p, st)| format!("{}\t{}", browse::encode_path(p), stat_fields(st)))
                    .collect();
                string_array(&mut env, &lines)
            }
            Err(e) => {
                not_found(&mut env, "search", &dir, e);
                std::ptr::null_mut()
            }
        }
    })
}

// Changes: all confined to `top` (the login account's home directory), judged by real path (see
// [`browse`]). New names come from the user and are used as UTF-8 as they are, without decoding
// `%XX`; returned names are written by [`browse::encode_path`].

/// Creates a file or directory ([directory]) in directory [dir], owned by [uid] and [gid]; when the
/// name is taken another one is used, and the name actually used is returned.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_browseCreate<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    top: JString<'l>,
    dir: JString<'l>,
    name: JString<'l>,
    directory: jboolean,
    uid: jint,
    gid: jint,
) -> jstring {
    guarded!("Native.browseCreate", throws env, std::ptr::null_mut(), {
        let (Some(store), Some(top), Some(dir), Some(name)) = (
            string(&mut env, &store),
            string(&mut env, &top),
            string(&mut env, &dir),
            string(&mut env, &name),
        ) else {
            return std::ptr::null_mut();
        };
        let r = tuxport_kernel::browse_changing(store.as_ref(), |s, changes| {
            browse::create(
                s,
                &browse::decode_path(&top),
                &browse::decode_path(&dir),
                name.as_bytes(),
                directory != 0,
                (uid as u32, gid as u32),
                changes,
            )
        });
        match r {
            Ok(name) => env
                .new_string(browse::encode_path(&name))
                .map(|s| s.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            Err(e) => {
                not_found(&mut env, "create in", &dir, e);
                std::ptr::null_mut()
            }
        }
    })
}

/// Removes a file, a symbolic link or a whole directory tree.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_browseRemove<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    top: JString<'l>,
    path: JString<'l>,
) {
    guarded!("Native.browseRemove", throws env, (), {
        let (Some(store), Some(top), Some(path)) = (
            string(&mut env, &store),
            string(&mut env, &top),
            string(&mut env, &path),
        ) else {
            return;
        };
        let r = tuxport_kernel::browse_changing(store.as_ref(), |s, changes| {
            browse::remove(
                s,
                &browse::decode_path(&top),
                &browse::decode_path(&path),
                changes,
            )
        });
        if let Err(e) = r {
            not_found(&mut env, "delete", &path, e);
        }
    })
}

/// Renames within the same directory ([name] is the new name), without replacing.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_browseRename<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    top: JString<'l>,
    path: JString<'l>,
    name: JString<'l>,
) -> jstring {
    guarded!("Native.browseRename", throws env, std::ptr::null_mut(), {
        let (Some(store), Some(top), Some(path), Some(name)) = (
            string(&mut env, &store),
            string(&mut env, &top),
            string(&mut env, &path),
            string(&mut env, &name),
        ) else {
            return std::ptr::null_mut();
        };
        let r = tuxport_kernel::browse_changing(store.as_ref(), |s, changes| {
            browse::rename(
                s,
                &browse::decode_path(&top),
                &browse::decode_path(&path),
                name.as_bytes(),
                changes,
            )
        });
        match r {
            Ok(()) => env
                .new_string(browse::encode_path(name.as_bytes()))
                .map(|s| s.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            Err(e) => {
                not_found(&mut env, "rename", &path, e);
                std::ptr::null_mut()
            }
        }
    })
}

/// Moves to another directory [to], keeping the name, without replacing.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_browseMove<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    top: JString<'l>,
    path: JString<'l>,
    to: JString<'l>,
) {
    guarded!("Native.browseMove", throws env, (), {
        let (Some(store), Some(top), Some(path), Some(to)) = (
            string(&mut env, &store),
            string(&mut env, &top),
            string(&mut env, &path),
            string(&mut env, &to),
        ) else {
            return;
        };
        let r = tuxport_kernel::browse_changing(store.as_ref(), |s, changes| {
            browse::move_to(
                s,
                &browse::decode_path(&top),
                &browse::decode_path(&path),
                &browse::decode_path(&to),
                changes,
            )
        });
        if let Err(e) = r {
            not_found(&mut env, "move", &path, e);
        }
    })
}

/// Opens a regular file for reading and writing (truncating first with [truncate], opening with
/// O_APPEND with [append]) and returns the host fd.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_browseOpenWrite<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    top: JString<'l>,
    path: JString<'l>,
    truncate: jboolean,
    append: jboolean,
) -> jint {
    guarded!("Native.browseOpenWrite", throws env, -1, {
        use std::os::fd::IntoRawFd;
        let (Some(store), Some(top), Some(path)) = (
            string(&mut env, &store),
            string(&mut env, &top),
            string(&mut env, &path),
        ) else {
            return -1;
        };
        let r = tuxport_kernel::browse_changing(store.as_ref(), |s, changes| {
            browse::open_write(
                s,
                &browse::decode_path(&top),
                &browse::decode_path(&path),
                truncate != 0,
                append != 0,
                changes,
            )
        });
        match r {
            Ok(file) => file.into_raw_fd(),
            Err(e) => {
                not_found(&mut env, "open", &path, e);
                -1
            }
        }
    })
}

/// A file handed out by [browseOpenWrite] was closed: tells the running kernel it was written
/// (programs watching in the guest receive `IN_MODIFY` and `IN_CLOSE_WRITE`). No error if the file
/// disappeared meanwhile.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_browseWritten<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    top: JString<'l>,
    path: JString<'l>,
) {
    guarded!("Native.browseWritten", (), {
        let (Some(store), Some(top), Some(path)) = (
            string(&mut env, &store),
            string(&mut env, &top),
            string(&mut env, &path),
        ) else {
            return;
        };
        let _ = tuxport_kernel::browse_changing(store.as_ref(), |s, changes| {
            browse::written(
                s,
                &browse::decode_path(&top),
                &browse::decode_path(&path),
                changes,
            )
        });
    })
}

fn stat_fields(st: &tuxport_kernel::vfs::store::Stat) -> String {
    let kind = if st.is_dir() { 'd' } else { 'f' };
    let perm = st.mode & 0o7777;
    format!(
        "{kind}\t{}\t{}\t{perm}",
        st.size,
        st.mtime_ns.div_euclid(1_000_000)
    )
}

fn not_found(env: &mut JNIEnv<'_>, what: &str, path: &str, e: tuxport_kernel::errno::Errno) {
    let _ = env.throw_new(
        "java/io/FileNotFoundException",
        format!("cannot {what} {path}: {e}"),
    );
}

// ---- The kernel ----

/// Handle → Machine (module docs).
static MACHINES: Mutex<BTreeMap<jlong, Arc<Machine>>> = Mutex::new(BTreeMap::new());
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_start<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    tier: jint,
) -> jlong {
    guarded!("Native.start", throws env, 0, {
        let Some(store) = string(&mut env, &store) else {
            return 0;
        };
        let options = MachineOptions {
            tier: if tier == 1 {
                Tier::Dbt
            } else {
                Tier::Interpreter
            },
            rootfs: Some(PathBuf::from(store)),
            memory_limit: None,
            low_port_offset: None,
            always_map_low_ports: false,
            // Guest crashes are reported in the kernel log, which the app's bug reports carry
            exception_trace: true,
        };
        match Machine::start(&options) {
            Ok(m) => {
                let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
                machines().insert(handle, Arc::new(m));
                handle
            }
            Err(e) => {
                throw(&mut env, format!("cannot start: {e}"));
                0
            }
        }
    })
}

/// Boots by `/etc/inittab` (the kernel/boot map): blocks until the sysinit and wait commands
/// have all finished.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_boot<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    guarded!("Native.boot", (), {
        if let Some(m) = machine(handle) {
            m.boot();
        }
    })
}

/// The host's default network changed: the routing table the guest sees becomes `routes` (each
/// entry as in [`parse_route`]); with null it goes back to being synthesised from the interfaces'
/// addresses.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_setRoutes<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    routes: JObject<'l>,
) {
    guarded!("Native.setRoutes", (), {
        let routes = if routes.is_null() {
            None
        } else {
            let Some(lines) = strings(&mut env, &JObjectArray::from(routes)) else {
                return;
            };
            Some(
                lines
                    .iter()
                    .filter_map(|l| parse_route(std::str::from_utf8(l).ok()?))
                    .collect(),
            )
        };
        if let Some(m) = machine(handle) {
            m.set_routes(routes);
        }
    })
}

/// One route passed in from `Native.kt`:
/// `<destination network in hex>/<prefix length> <gateway in hex or -> <interface>`. The metric is
/// the same as for the same route on Linux: 0 for IPv4; for IPv6, 1024 for the default route (RA)
/// and 256 for others (the networks the kernel adds for addresses).
fn parse_route(line: &str) -> Option<HostRoute> {
    // Slicing by bytes needs ASCII: a multi-byte character would split and panic, which `guarded!`
    // would turn into a failed setRoutes call setting none of the routes
    let hex = |s: &str| -> Option<Vec<u8>> {
        (s.is_ascii() && s.len().is_multiple_of(2))
            .then(|| {
                (0..s.len() / 2)
                    .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
                    .collect()
            })
            .flatten()
    };
    let mut f = line.split_whitespace();
    let (dst, prefix) = f.next()?.split_once('/')?;
    let (dst, prefix): (Vec<u8>, u8) = (hex(dst)?, prefix.parse().ok()?);
    let gateway = match f.next()? {
        "-" => None,
        g => Some(hex(g)?),
    };
    let iface = f.next()?.to_owned();
    let (family, metric) = match dst.len() {
        4 => (libc::AF_INET, 0),
        16 if prefix == 0 => (libc::AF_INET6, 1024),
        16 => (libc::AF_INET6, 256),
        _ => return None,
    };
    if gateway.as_ref().is_some_and(|g| g.len() != dst.len()) {
        return None;
    }
    Some(HostRoute {
        family,
        dst,
        prefix,
        gateway,
        iface,
        metric,
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_openConsole<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    rows: jint,
    cols: jint,
) -> jint {
    guarded!("Native.openConsole", throws env, -1, {
        let (rows, cols) = (rows.clamp(1, 0xffff) as u16, cols.clamp(1, 0xffff) as u16);
        let Some(m) = machine(handle) else {
            throw(&mut env, "the machine is stopped");
            return -1;
        };
        match m.open_console(rows, cols) {
            Ok(id) => id as jint,
            Err(e) => {
                throw(&mut env, format!("cannot open a console: {e}"));
                -1
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_spawn<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
    program: JString<'l>,
    argv: JObjectArray<'l>,
    envp: JObjectArray<'l>,
) -> jint {
    guarded!("Native.spawn", throws env, -1, {
        let Some(program) = string(&mut env, &program) else {
            return -1;
        };
        let (Some(argv), Some(envp)) = (strings(&mut env, &argv), strings(&mut env, &envp)) else {
            return -1;
        };
        // Negative: no console, use the host's stdin/stdout/stderr (for the app, /dev/null and logcat)
        let stdio = u32::try_from(console).map_or(Stdio::Host, Stdio::Console);
        let Some(m) = machine(handle) else {
            throw(&mut env, "the machine is stopped");
            return -1;
        };
        match m.spawn(program.as_bytes(), &argv, &envp, stdio) {
            Ok(pid) => pid as jint,
            Err(e) => {
                throw(&mut env, format!("{program}: {e}"));
                -1
            }
        }
    })
}

/// Reads console output: returns the number of bytes read, 0 when the session ended, -1 when the
/// console is closed.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_read<'l>(
    env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
    buffer: JByteArray<'l>,
) -> jint {
    guarded!("Native.read", -1, {
        let Ok(len) = env.get_array_length(&buffer) else {
            return -1;
        };
        let Some(m) = machine(handle) else {
            return -1;
        };
        let mut buf = vec![0u8; len as usize];
        match m.read_console(console as u32, &mut buf) {
            Ok(n) => {
                // SAFETY: u8 and i8 have the same layout
                let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<i8>(), n) };
                match env.set_byte_array_region(&buffer, 0, bytes) {
                    Ok(()) => n as jint,
                    Err(_) => -1,
                }
            }
            Err(_) => -1,
        }
    })
}

/// Input to a console: returns the number of bytes written, -1 when the session ended or the
/// console is closed.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_write<'l>(
    env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
    buffer: JByteArray<'l>,
    offset: jint,
    length: jint,
) -> jint {
    guarded!("Native.write", -1, {
        let mut data = vec![0i8; length.max(0) as usize];
        if env
            .get_byte_array_region(&buffer, offset, &mut data)
            .is_err()
        {
            return -1;
        }
        // SAFETY: u8 and i8 have the same layout
        let data = unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), data.len()) };
        let Some(m) = machine(handle) else {
            return -1;
        };
        let mut done = 0;
        while done < data.len() {
            match m.write_console(console as u32, &data[done..]) {
                Ok(n) => done += n,
                Err(_) => return -1,
            }
        }
        done as jint
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_resize<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
    rows: jint,
    cols: jint,
) {
    guarded!("Native.resize", (), {
        if let Some(m) = machine(handle) {
            let _ = m.resize_console(
                console as u32,
                rows.clamp(1, 0xffff) as u16,
                cols.clamp(1, 0xffff) as u16,
            );
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_closeConsole<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
) {
    guarded!("Native.closeConsole", (), {
        if let Some(m) = machine(handle) {
            let _ = m.close_console(console as u32);
        }
    })
}

/// Waits for a process to end: returns the exit code, or the negated signal number when a signal
/// terminated it (the same convention as Termux). Once the kernel is stopped, all processes have
/// been ended by SIGKILL.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_waitFor<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    pid: jint,
) -> jint {
    guarded!("Native.waitFor", -9, {
        let Some(m) = machine(handle) else {
            return -9;
        };
        match m.wait(pid as u32) {
            ExitStatus::Exited(code) => code,
            ExitStatus::Signaled { signal, .. } => -signal.number(),
        }
    })
}

/// The current directory of the console's foreground process group (where restored tabs and new
/// tabs start): null when it cannot be had (the shell has exited, the handle is invalid). A
/// path that is not UTF-8 is converted with UTF-8 replacement rules.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_consoleCwd<'l>(
    env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
) -> jstring {
    guarded!("Native.consoleCwd", std::ptr::null_mut(), {
        let Some(path) = machine(handle).and_then(|m| m.console_cwd(console as u32).ok()) else {
            return std::ptr::null_mut();
        };
        env.new_string(String::from_utf8_lossy(&path))
            .map(JString::into_raw)
            .unwrap_or(std::ptr::null_mut())
    })
}

/// Takes the ports the guest listened on for the first time on a non-loopback address (design doc
/// §12 item 7): each entry is `<process name>\t<port>\t<host port>`; an empty array when there are
/// none.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_takeListens<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jobjectArray {
    guarded!("Native.takeListens", std::ptr::null_mut(), {
        let events = machine(handle)
            .map(|m| m.take_listens())
            .unwrap_or_default();
        let Ok(array) =
            env.new_object_array(events.len() as jint, "java/lang/String", JObject::null())
        else {
            return std::ptr::null_mut();
        };
        for (i, e) in events.iter().enumerate() {
            let line = format!("{}\t{}\t{}", e.comm, e.port, e.host_port);
            let Ok(s) = env.new_string(line) else {
                return std::ptr::null_mut();
            };
            if env.set_object_array_element(&array, i as jint, s).is_err() {
                return std::ptr::null_mut();
            }
        }
        array.into_raw()
    })
}

/// Takes the guest processes the kernel layer ended (kernel bug, engine fault, OOM killer): each
/// entry is `<console or -1>\t<pid>\t<process name>\t<bug|oom>\t<a>\t<b>\t<c>`; for bug, a is the
/// report, b the last few lines of the kernel log and c the unimplemented paths the guest reached
/// most (both separated by U+001E); for oom, a is the cgroup (empty for the app's limit), b and c
/// the usage and the limit (bytes). Newlines, tabs and U+001E inside the fields are replaced by
/// spaces.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_takeKills<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jobjectArray {
    guarded!("Native.takeKills", std::ptr::null_mut(), {
        let kills = machine(handle).map(|m| m.take_kills()).unwrap_or_default();
        let Ok(array) =
            env.new_object_array(kills.len() as jint, "java/lang/String", JObject::null())
        else {
            return std::ptr::null_mut();
        };
        let clean = |s: &str| s.replace(['\t', '\n', '\r', '\u{1e}'], " ");
        for (i, k) in kills.iter().enumerate() {
            let console = k.console.map_or(-1, i64::from);
            let tail = match &k.reason {
                tuxport_kernel::KillReason::Bug { report, log, stubs } => {
                    let join = |lines: &[String]| {
                        lines
                            .iter()
                            .map(|l| clean(l))
                            .collect::<Vec<_>>()
                            .join("\u{1e}")
                    };
                    format!("bug\t{}\t{}\t{}", clean(report), join(log), join(stubs))
                }
                tuxport_kernel::KillReason::OutOfMemory {
                    cgroup,
                    used,
                    limit,
                } => format!(
                    "oom\t{}\t{used}\t{limit}",
                    clean(cgroup.as_deref().unwrap_or(""))
                ),
            };
            let line = format!("{console}\t{}\t{}\t{tail}", k.pid, clean(&k.comm));
            let Ok(s) = env.new_string(line) else {
                return std::ptr::null_mut();
            };
            if env.set_object_array_element(&array, i as jint, s).is_err() {
                return std::ptr::null_mut();
            }
        }
        array.into_raw()
    })
}

/// The latest hold of the BKL that lasted longer than the watchdog's threshold, ended or not:
/// `<id>\t<seconds>\t<report>\t<kernel log record once ended, else empty>`, or null for none.
/// Does not take the BKL, so the app can call it on the main thread while the worker waits for
/// that hold to end. Tabs and newlines inside the fields are replaced by spaces.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_bklStall<'l>(
    env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jstring {
    guarded!("Native.bklStall", std::ptr::null_mut(), {
        let Some(s) = machine(handle).and_then(|m| m.stall()) else {
            return std::ptr::null_mut();
        };
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        let ended = s.ended.as_deref().map(clean).unwrap_or_default();
        let line = format!("{}\t{}\t{}\t{ended}", s.id, s.secs, clean(&s.report));
        env.new_string(line)
            .map(JString::into_raw)
            .unwrap_or(std::ptr::null_mut())
    })
}

/// Takes the guest's power request (busybox's reboot, halt, poweroff): 0 none, 1 reboot, 2 halt,
/// 3 power off.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_takePower<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jint {
    guarded!("Native.takePower", 0, {
        match machine(handle).and_then(|m| m.take_power()) {
            None => 0,
            Some(Power::Reboot) => 1,
            Some(Power::Halt) => 2,
            Some(Power::PowerOff) => 3,
        }
    })
}

/// Number of guest processes still running (not counting init).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_processes<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jint {
    guarded!("Native.processes", 0, {
        machine(handle).map_or(0, |m| m.processes() as jint)
    })
}

/// Guest CPU nanoseconds, including exited threads; unavailable samples must not imply idleness.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_guestCpuTime<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jlong {
    guarded!("Native.guestCpuTime", -1, {
        machine(handle).map_or(-1, |m| m.cpu_time_ns().min(jlong::MAX as u64) as jlong)
    })
}

/// Takes the directories whose entries the guest changed (written as by [`browse::encode_path`],
/// that is, document IDs): the Files app refreshes them.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_takeChangedDirs<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jobjectArray {
    guarded!("Native.takeChangedDirs", std::ptr::null_mut(), {
        let dirs: Vec<String> = machine(handle)
            .map(|m| m.take_changed_dirs())
            .unwrap_or_default()
            .iter()
            .map(|d| browse::encode_path(d))
            .collect();
        string_array(&mut env, &dirs)
    })
}

/// The host is short of memory: drop caches that can be rebuilt (translations kept in the pool and
/// so on) and commit the deferred write transaction.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_trimMemory<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    guarded!("Native.trimMemory", (), {
        if let Some(m) = machine(handle) {
            m.trim_memory();
        }
    })
}

/// Shuts down ([`Machine::shutdown`]): runs the shutdown commands in inittab, sends SIGTERM to all
/// processes, ends those left, and then removes the handle from the table. Other calls keep working
/// during the shutdown commands.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_shutdown<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    guarded!("Native.shutdown", (), {
        if let Some(m) = machine(handle) {
            m.shutdown(Power::PowerOff, tuxport_kernel::SHUTDOWN_TIMEOUT);
        }
        // Dropped only after the table's lock is released: when this is the last reference, Machine's
        // drop syncs the store and joins threads, and every other call would wait for it
        let m = machines().remove(&handle);
        drop(m);
    })
}

/// Ends all guest processes and removes the handle from the table.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_sajadxdp0_tuxport_Native_stop<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    guarded!("Native.stop", (), {
        // Remove first, then kill: kill_all waits, so the table's lock must not be held
        let m = machines().remove(&handle);
        if let Some(m) = m {
            m.kill_all();
        }
    })
}

/// The Machine of a handle, holding a reference for the duration of the call; None once stopped.
fn machine(handle: jlong) -> Option<Arc<Machine>> {
    machines().get(&handle).cloned()
}

fn machines() -> std::sync::MutexGuard<'static, BTreeMap<jlong, Arc<Machine>>> {
    MACHINES.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- Strings and errors ----

/// Throws `java.io.IOException`; Kotlin sees it after the JNI call returns.
fn throw(env: &mut JNIEnv<'_>, message: impl AsRef<str>) {
    let _ = env.throw_new("java/io/IOException", message.as_ref());
}

fn string(env: &mut JNIEnv<'_>, s: &JString<'_>) -> Option<String> {
    env.get_string(s).ok().map(Into::into)
}

fn strings(env: &mut JNIEnv<'_>, array: &JObjectArray<'_>) -> Option<Vec<Vec<u8>>> {
    let n = env.get_array_length(array).ok()?;
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let item = JString::from(env.get_object_array_element(array, i).ok()?);
        out.push(string(env, &item)?.into_bytes());
    }
    Some(out)
}

fn string_array(env: &mut JNIEnv<'_>, items: &[String]) -> jobjectArray {
    let Ok(array) = env.new_object_array(items.len() as jint, "java/lang/String", JObject::null())
    else {
        return std::ptr::null_mut();
    };
    for (i, item) in items.iter().enumerate() {
        let Ok(s) = env.new_string(item) else {
            return std::ptr::null_mut();
        };
        if env.set_object_array_element(&array, i as jint, s).is_err() {
            return std::ptr::null_mut();
        }
    }
    array.into_raw()
}
