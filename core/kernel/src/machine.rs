//! The guest kernel as the host sees it (the kernel/boot, kernel/process and kernel/time maps,
//! and android/service).
//!
//! A [`Machine`] holds one kernel instance: it can start several processes one after another
//! ([`Machine::spawn`]), and each process can have its own console (an in-process PTY whose master
//! the host holds, [`Machine::open_console`]). The Android foreground service opens one console
//! and one login shell per terminal tab; `tuxport-run` starts only one process ([`run`]). Methods
//! may be called on any host thread, and blocking methods use the same unified wait mechanism as
//! guest threads. Interval timers are handled by one host thread ([`crate::itimer`]).
//!
//! When kernel code in a method hits a kernel-layer bug (a panic), only that call fails
//! ([`crate::bug::host_call`]): it returns EIO or [`RunError::Internal`], and when the failing call
//! is on a console it also hangs up that console, so the terminal tab does not get stuck on a
//! broken console. Methods that only read one table ([`Machine::wait`] and the like) do not need
//! this.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::thread::JoinHandle;

use tuxport_engine::{Cpu, Dbt, DbtStats};
use tuxport_hal::Event;

use crate::cell::RefCell;
use crate::cred::Cred;
use crate::errno::Errno;
use crate::fd::FdTable;
use crate::kernel::{INIT, Kernel, Shared, block, block_for, request_wake, wake};
use crate::loader::{self, Args, Executable};
use crate::mm::Mm;
use crate::process::{
    ExitStatus, RunError, RunOptions, RunReport, Signal, ThreadGroup, Tier, comm_of,
    read_executable,
};
use crate::signal::{self, SigHandlers};
use crate::task::{self, FsContext, Task, TaskState};
use crate::terminal;
use crate::tty::Winsize;
use crate::vfs::{Node, Vfs};
use crate::watchdog::Watchdog;

/// Options of a kernel instance.
#[derive(Debug, Clone, Default)]
pub struct MachineOptions {
    /// The execution engine.
    pub tier: Tier,
    /// The host directory of the inode store. When set, program paths are resolved as guest paths
    /// in the store; otherwise they are read as host paths and the guest sees no filesystem at all.
    pub rootfs: Option<PathBuf>,
    /// The limit on the app process's anonymous memory (bytes): when exceeded a guest process is
    /// ended, so that Android's LMK does not kill the whole app (design doc §12 item 4). None means
    /// half the host's physical memory, and `u64::MAX` means no limit.
    pub memory_limit: Option<u64>,
    /// The offset of the host ports that ports below 1024 are changed to ([`crate::portmap`]):
    /// None is the default 10000, and 0 means no mapping (on Android binding most low ports is
    /// EACCES).
    pub low_port_offset: Option<u16>,
    /// Change low ports even when the host lets them be bound as they are (by default they are
    /// bound as they are): for testing the mapping on hosts that can bind low ports (running as
    /// root, in a container).
    pub always_map_low_ports: bool,
    /// Start with `debug.exception-trace` at 1 rather than arm64 Linux's 0, so that unhandled
    /// exceptions are reported in the kernel log (the kernel/signal map) from the first process
    /// on; the guest can still change it. The app does, so that guest crashes reach `dmesg` and
    /// the bug reports carrying the kernel log.
    pub exception_trace: bool,
}

/// stdin/stdout/stderr of a new process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdio {
    /// The host process's own 0, 1 and 2.
    Host,
    /// The slave of a console (by PTY number): the process becomes the leader of a new session,
    /// and the console is its controlling terminal.
    Console(u32),
}

/// Running kernels that use a store: when the host opens a store separately and changes something
/// ([`crate::browse_changing`]), they are found to report inotify events ([`store_changed`]). Store
/// directories are compared by canonical path; when a kernel stops its weak reference expires and
/// is cleared at the next registration or lookup.
static RUNNING: std::sync::Mutex<Vec<(PathBuf, std::sync::Weak<Shared>)>> =
    std::sync::Mutex::new(Vec::new());

/// A guest kernel instance. Dropping it ends every guest process.
pub struct Machine {
    shared: Arc<Shared>,
    timers: Option<JoinHandle<()>>,
    watchdog: Option<Watchdog>,
}

/// How long the shutdown commands in inittab may take in total when [`run`] shuts down (OpenRC
/// waits up to 15 s when stopping PostgreSQL).
pub const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

thread_local! {
    /// The event a host thread waits on in the Machine's blocking methods.
    static EVENT: Arc<Event> = Arc::new(Event::new().expect("cannot create an eventfd"));
}

impl Machine {
    /// Creates a kernel instance: opens the inode store and creates the built-in init (pid 1).
    pub fn start(options: &MachineOptions) -> Result<Machine, RunError> {
        crate::bug::host_call("Machine::start", || Self::start_kernel(options))
            .unwrap_or(Err(RunError::Internal))
    }

    fn start_kernel(options: &MachineOptions) -> Result<Machine, RunError> {
        crate::bug::install_panic_hook();
        if options.tier == Tier::Dbt && Dbt::new().is_none() {
            return Err(RunError::TierUnavailable);
        }
        let vfs = match &options.rootfs {
            Some(dir) => Some(Rc::new(Vfs::open(dir).map_err(RunError::Rootfs)?)),
            None => None,
        };
        tuxport_hal::raise_fd_limit();
        // An engine bug ends only the guest process that hits it (crate::bug)
        if crate::bug::contain() {
            tuxport_engine::install_fault_guard();
        }
        let shared = Arc::new(Shared::new(Kernel::new(options.tier)));
        if let Some(vfs) = &vfs {
            // Deferred write transactions are committed by the timer thread when due
            let timers = shared.timers.clone();
            vfs.store().set_waker(Box::new(move || timers.signal()));
        }
        {
            let mut k = shared.lock();
            if let Some(vfs) = &vfs {
                vfs.store().guard_with_bkl(shared.id());
            }
            k.vfs = vfs;
            k.oom.limit = options
                .memory_limit
                .unwrap_or_else(crate::oom::default_limit);
            {
                let mut portmap = k.portmap.borrow_mut();
                portmap.offset = options
                    .low_port_offset
                    .unwrap_or(crate::portmap::DEFAULT_OFFSET);
                portmap.always_map = options.always_map_low_ports;
            }
            if options.exception_trace {
                k.sysctls.insert(signal::EXCEPTION_TRACE, 1);
            }
            let init = ThreadGroup::new(INIT, 0, Rc::new(RefCell::new(SigHandlers::default())));
            k.groups.insert(INIT, Rc::new(RefCell::new(init)));
        }
        if let Some(dir) = &options.rootfs {
            let dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            let mut running = RUNNING
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            running.retain(|(_, w)| w.strong_count() > 0);
            running.push((dir, Arc::downgrade(&shared)));
        }
        let timer_shared = shared.clone();
        let timers = std::thread::Builder::new()
            .name("timers".into())
            .spawn(move || timers(timer_shared))
            .map_err(RunError::Io)?;
        let watchdog = Watchdog::start(&shared);
        Ok(Machine {
            shared,
            timers: Some(timers),
            watchdog,
        })
    }

    /// Boots from `/etc/inittab` ([`crate::inittab`]): runs in order sysinit (waited for), boot and
    /// bootwait together in file order (only bootwait waited for), and the default runlevel's
    /// wait (waited for) and once (not waited for), with output written to
    /// [`crate::inittab::LOG`]. On return the boot and once commands may still be running. Does
    /// nothing without an inittab or a store. From the start of boot the built-in init accepts
    /// busybox's reboot, halt and poweroff requests ([`Machine::take_power`]).
    pub fn boot(&self) {
        self.shared.lock().init_handlers = true;
        let entries = self.inittab();
        let mut first = true;
        for (process, wait) in crate::inittab::boot_plan(&entries) {
            if self.shared.lock().stopping {
                break;
            }
            let Some(pid) = self.run_init_process(&process, first) else {
                continue;
            };
            first = false;
            if wait {
                self.wait(pid);
            }
        }
        // No command ran at all (OpenRC not installed): the previous boot's output is not kept, so
        // it is not taken for this boot's (the app looks in it for services that failed to start)
        if first {
            crate::bug::host_call("Machine::boot", || {
                let k = self.shared.lock();
                let Some(vfs) = &k.vfs else { return };
                let log = crate::inittab::LOG.as_bytes();
                if crate::vfs::read_file(vfs.store(), log).is_ok_and(|t| !t.is_empty()) {
                    let _ = crate::vfs::write_file(vfs.store(), log, b"", 0o644, true);
                }
            });
        }
    }

    /// Starts a new process (a child of init) and returns its pid. With a store, `program` is a
    /// path in the guest filesystem; otherwise it is a host path.
    pub fn spawn(
        &self,
        program: &[u8],
        argv: &[Vec<u8>],
        envp: &[Vec<u8>],
        stdio: Stdio,
    ) -> Result<u32, RunError> {
        crate::bug::host_call("Machine::spawn", || {
            self.spawn_process(program, argv, envp, stdio)
        })
        .unwrap_or(Err(RunError::Internal))
    }

    fn spawn_process(
        &self,
        program: &[u8],
        argv: &[Vec<u8>],
        envp: &[Vec<u8>],
        stdio: Stdio,
    ) -> Result<u32, RunError> {
        let io_err = |e: Errno| RunError::Io(io::Error::from_raw_os_error(e.0));
        let mut k = self.shared.lock();
        let vfs = k.vfs.clone();
        let offline = crate::pseudo::Offline;
        let file = match &vfs {
            Some(vfs) => read_executable(
                vfs,
                &offline,
                &Cred::root(),
                &Node::ROOT,
                &Node::ROOT,
                program,
            )
            .map_err(RunError::Exec)?,
            None => {
                let f = std::fs::File::open(OsStr::from_bytes(program)).map_err(RunError::Io)?;
                Executable::open(f).map_err(|e| RunError::Io(io::Error::from_raw_os_error(e.0)))?
            }
        };
        let args = Args {
            argv,
            envp,
            execfn: program,
            ids: [0; 4],
            secure: false,
            stack_limit: loader::STACK_SIZE,
        };
        let mut mm = Mm::new();
        mm.share_files(&k.file_maps);
        let mut read_interp = |p: &[u8]| match &vfs {
            Some(vfs) => read_executable(vfs, &offline, &Cred::root(), &Node::ROOT, &Node::ROOT, p),
            None => Err(Errno::ENOENT),
        };
        let image =
            loader::load(&mut mm, &file, &args, &mut read_interp).map_err(RunError::Load)?;
        let walked = vfs.as_ref().and_then(|vfs| {
            vfs.walk(
                &offline,
                &Cred::root(),
                &Node::ROOT,
                &Node::ROOT,
                program,
                true,
            )
            .ok()
        });
        let real = match (&vfs, &walked) {
            (Some(vfs), Some((node, found))) => vfs
                .found_path(&offline, &Node::ROOT, node, found.as_ref())
                .ok(),
            _ => None,
        };
        let exe: Rc<[u8]> =
            Rc::from(real.unwrap_or_else(|| crate::vfs::normalize(&[b"/", program].concat())));
        let ino_of = |p: &[u8]| {
            vfs.as_ref()
                .and_then(|vfs| {
                    vfs.resolve(&offline, &Cred::root(), &Node::ROOT, &Node::ROOT, p, true)
                        .ok()
                })
                .and_then(|n| n.ino())
                .unwrap_or(0)
        };
        let root = Node::ROOT;
        let interp = crate::syscall::interp_label(
            vfs.as_deref(),
            &offline,
            &Cred::root(),
            &root,
            &root,
            &image,
        );
        crate::syscall::label_image(&mut mm, &image, (&exe, ino_of(program)), interp);
        // Record the program file as execve does (ETXTBSY, the key of pooled translations, the
        // group sharing translations)
        mm.exe = Some(ino_of(program)).filter(|&ino| ino != 0);

        let tid = k.alloc_tid().map_err(io_err)?;
        let mut group = ThreadGroup::new(tid, INIT, Rc::new(RefCell::new(SigHandlers::default())));
        group.exe = Some(Rc::new(crate::process::ExeFile::new(
            exe.clone(),
            walked,
            None,
            None,
            mm.exe,
            &file,
            None,
        )));
        // Whether the console had a session before this one, for giving it back on failure
        let mut console = None;
        let fds = match stdio {
            Stdio::Host => FdTable::with_host_stdio(),
            Stdio::Console(id) => {
                let attached = k.consoles.get(&id).is_some_and(|c| c.attached);
                let (slave, tty) = terminal::attach(&mut k, id, tid).map_err(RunError::Console)?;
                console = Some((id, attached));
                group.ctty = Some(tty);
                let mut table = FdTable::new();
                for fd in 0..3 {
                    table.install_at(fd, slave.clone(), false).unwrap();
                }
                table
            }
        };
        let mm = mm.into_shared();
        // SAFETY: the Mm is held by the task's TaskState and is not released before the thread
        // exits or execs
        let mut cpu = unsafe { Cpu::new(mm.borrow().mmu()) };
        cpu.state_mut().pc = image.entry;
        cpu.state_mut().sp = image.sp;
        let group = Rc::new(RefCell::new(group));
        let state = TaskState {
            mm,
            fds: Rc::new(RefCell::new(fds)),
            fs: Rc::new(RefCell::new(FsContext {
                root: Node::ROOT,
                cwd: Node::ROOT,
                umask: 0o022,
                mnt_ns: vfs.as_ref().map_or_else(
                    || {
                        Rc::new(crate::vfs::MntNs {
                            id: crate::vfs::INIT_MNT_NS,
                            owner: crate::cred::UserNs::init(),
                            anon: false,
                        })
                    },
                    |v| v.init_ns(),
                ),
            })),
            group: group.clone(),
            vfs,
            altstack: Default::default(),
            clear_child_tid: 0,
            vfork_done: None,
            restart: None,
            fast_sleep: None,
        };
        let ns = k.init_ns.clone();
        let started = Task::new(tid, tid, &cpu, state, ns, 0, comm_of(program))
            .and_then(|task| task::start(&self.shared, &mut k, task, cpu));
        if let Err(e) = started {
            // Nothing refers to the process yet, but the console has it as its session: give the
            // console back, so a later spawn can attach to it
            if let Some((id, attached)) = console {
                terminal::detach(&mut k, id, tid, attached);
            }
            return Err(io_err(e));
        }
        // Registered only once its thread has started, as clone does: a process left without
        // threads would keep its pid and its watched slot and never finish
        k.groups.insert(tid, group);
        k.groups[&INIT].borrow_mut().children.push(tid);
        k.forks += 1;
        k.watched.insert(tid, None);
        Ok(tid as u32)
    }

    /// Creates a console and returns its PTY number (`/dev/pts/<number>`).
    pub fn open_console(&self, rows: u16, cols: u16) -> Result<u32, Errno> {
        self.open_console_with(Winsize {
            rows,
            cols,
            ..Winsize::default()
        })
    }

    fn open_console_with(&self, size: Winsize) -> Result<u32, Errno> {
        crate::bug::host_call("Machine::open_console", || {
            terminal::open(&mut self.shared.lock(), size)
        })
        .ok_or(Errno::EIO)
    }

    /// Reads the console's output, waiting when there is none. Returns 0 when the session has
    /// ended: every slave is closed and the output has all been read.
    pub fn read_console(&self, id: u32, buf: &mut [u8]) -> Result<usize, Errno> {
        let objects = terminal::wake_objects(&self.shared.lock(), id);
        let out = self.wait_objects(wake::FILE, objects.as_deref(), |k| {
            on_console(k, id, "Machine::read_console", |k| {
                terminal::take_output(k, id, buf.len())
            })
            .transpose()
        })?;
        buf[..out.len()].copy_from_slice(&out);
        Ok(out.len())
    }

    /// As [`Machine::read_console`], but reads only up to where the output's writer changes
    /// between the console's foreground process group and another, and returns whether another
    /// wrote these bytes: a job the shell left in the background, or a process of another session.
    /// What the line discipline echoes counts as the foreground's.
    pub fn read_console_run(&self, id: u32, buf: &mut [u8]) -> Result<(usize, bool), Errno> {
        let objects = terminal::wake_objects(&self.shared.lock(), id);
        let (out, background) = self.wait_objects(wake::FILE, objects.as_deref(), |k| {
            on_console(k, id, "Machine::read_console_run", |k| {
                terminal::take_output_run(k, id, buf.len())
            })
            .transpose()
        })?;
        buf[..out.len()].copy_from_slice(&out);
        Ok((out.len(), background))
    }

    /// Writes input to the console (through the line discipline), waiting while the input queue is
    /// full. Returns the number of bytes written; EIO if the session has already ended.
    pub fn write_console(&self, id: u32, data: &[u8]) -> Result<usize, Errno> {
        if data.is_empty() {
            return Ok(0);
        }
        let objects = terminal::wake_objects(&self.shared.lock(), id);
        self.wait_objects(wake::FILE, objects.as_deref(), |k| {
            on_console(k, id, "Machine::write_console", |k| {
                let room = terminal::input_room(k, id)?;
                if room > 0 {
                    let n = room.min(data.len());
                    return terminal::feed(k, id, &data[..n]).map(|()| Some(n));
                }
                // The input queue is full, but no process will read it any more
                match terminal::take_output(k, id, 0) {
                    Ok(Some(_)) => Err(Errno::EIO),
                    _ => Ok(None),
                }
            })
            .transpose()
        })
    }

    /// The console's window size changed: the foreground process group gets SIGWINCH.
    pub fn resize_console(&self, id: u32, rows: u16, cols: u16) -> Result<(), Errno> {
        let w = Winsize {
            rows,
            cols,
            ..Winsize::default()
        };
        on_console(
            &mut self.shared.lock(),
            id,
            "Machine::resize_console",
            |k| terminal::resize(k, id, w),
        )
    }

    /// Closes the console (the terminal tab closed): the session leader gets SIGHUP.
    pub fn close_console(&self, id: u32) -> Result<(), Errno> {
        crate::bug::host_call("Machine::close_console", || {
            terminal::close(&mut self.shared.lock(), id)
        })
        .unwrap_or(Err(Errno::EIO))
    }

    /// The process group in the foreground of console `id` (the app clears what a program reported
    /// about its progress once another group takes the terminal; the android/vt map). ESRCH
    /// if there is none, as after the shell has exited.
    pub fn console_foreground(&self, id: u32) -> Result<i32, Errno> {
        let k = self.shared.lock();
        crate::bug::host_call("Machine::console_foreground", || terminal::fg_pgrp(&k, id))
            .unwrap_or(Err(Errno::EIO))
    }

    /// The current directory of console `id`'s foreground process group (the shells of tabs the
    /// app restores after being killed start here, the android/service map, and so do those of new
    /// tabs, the android/terminal map). ESRCH if the shell has already exited.
    pub fn console_cwd(&self, id: u32) -> Result<Vec<u8>, Errno> {
        let k = self.shared.lock();
        crate::bug::host_call("Machine::console_cwd", || terminal::fg_cwd(&k, id))
            .unwrap_or(Err(Errno::EIO))
    }

    /// Whether console `id` reads lines without echoing them (ICANON on, ECHO off), as programs
    /// set it to read a password; the app draws the cursor as a lock then (the android/vtview
    /// map).
    pub fn console_password_input(&self, id: u32) -> Result<bool, Errno> {
        let k = self.shared.lock();
        crate::bug::host_call("Machine::console_password_input", || {
            terminal::password_input(&k, id)
        })
        .unwrap_or(Err(Errno::EIO))
    }

    /// Waits for a process started by [`Machine::spawn`] to end.
    pub fn wait(&self, pid: u32) -> ExitStatus {
        // This needs waking only when a process the host waits for ends; other processes ending
        // and files closing need not wake it
        self.wait_on(wake::WATCHED, |k| {
            k.watched.get(&(pid as i32)).copied().flatten()
        })
    }

    /// Returns how a process started by [`Machine::spawn`] ended, if it has.
    pub fn try_wait(&self, pid: u32) -> Option<ExitStatus> {
        self.shared
            .lock()
            .watched
            .get(&(pid as i32))
            .copied()
            .flatten()
    }

    /// Waits for a process started by [`Machine::spawn`] to end, or for the guest to request
    /// shutdown ([`Machine::take_power`]; the request is not taken).
    pub fn wait_or_power(&self, pid: u32) -> Result<ExitStatus, crate::Power> {
        self.wait_on(wake::WATCHED, |k| match k.watched.get(&(pid as i32)) {
            Some(Some(status)) => Some(Ok(*status)),
            _ => k.power.map(Err),
        })
    }

    /// Host memory is tight and the app's UI has been away for a while (the android/service map):
    /// drops caches that can be rebuilt, that is the Tier 1 translations kept in the pool, the
    /// shared read-only file mappings and the dentry/inode caches (those in use by running
    /// processes are left alone). The app is most likely in the background then and may be killed
    /// at any time, so deferred write transactions are committed too.
    pub fn trim_memory(&self) {
        crate::debug::log(format_args!("trimming memory"));
        tuxport_engine::trim_dbt_pool();
        crate::bug::host_call("Machine::trim_memory", || {
            let k = self.shared.lock();
            k.file_maps.clear();
            if let Some(vfs) = &k.vfs {
                vfs.trim();
            }
        });
        self.sync();
    }

    /// The host's network changed (the kernel/net map): the routes in the main table become
    /// `routes`, which the guest's `ip route`, `/proc/net/route` and so on then see
    /// ([`crate::route`]). On Android the app cannot read the host routing table, so it takes the
    /// routes from the default network's `LinkProperties` and passes them in; None switches back to
    /// reading the host's, synthesised from the interface addresses when that cannot be read.
    pub fn set_routes(&self, routes: Option<Vec<crate::HostRoute>>) {
        crate::bug::host_call("Machine::set_routes", || {
            self.shared.lock().host_routes = routes;
        });
    }

    /// Commits the inode store's deferred write transactions (when the app goes to the background,
    /// [`crate::vfs::store`]).
    pub fn sync(&self) {
        crate::bug::host_call("Machine::sync", || {
            if let Some(vfs) = &self.shared.lock().vfs {
                let _ = vfs.store().commit();
            }
        });
    }

    /// Shuts down (busybox init's `run_shutdown_and_kill_processes`): if it has booted, first runs
    /// the shutdown commands in inittab in order ([`crate::inittab::shutdown_plan`]); then sends
    /// SIGTERM to every process, waits up to 1 second, and ends the rest ([`Machine::kill_all`]).
    /// `timeout` limits how long the shutdown commands may take in total; once it is reached they
    /// are no longer waited for.
    pub fn shutdown(&self, power: crate::Power, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        let init = {
            let mut k = self.shared.lock();
            k.stopping = true;
            k.init_handlers
        };
        if init {
            for process in crate::inittab::shutdown_plan(&self.inittab(), power) {
                if let Some(pid) = self.run_init_process(&process, false) {
                    let exited = |k: &mut Kernel| k.watched.get(&(pid as i32)).copied().flatten();
                    if self
                        .wait_until(wake::WATCHED, Some(deadline), exited)
                        .is_none()
                    {
                        break;
                    }
                }
            }
        }
        crate::bug::host_call("Machine::shutdown", || {
            let k = self.shared.lock();
            let groups: Vec<_> = k.groups.values().cloned().collect();
            for g in groups.iter().filter(|g| !g.borrow().threads.is_empty()) {
                let tgid = g.borrow().tgid;
                let info = crate::signal::SigInfo::kill(crate::signal::code::SI_USER, INIT, 0);
                signal::send_to_group(&k, tgid, Signal::TERM, info);
            }
        });
        let second = std::time::Instant::now() + std::time::Duration::from_secs(1);
        self.wait_until(wake::ALL, Some(second), |k| {
            k.tasks.is_empty().then_some(())
        });
        self.kill_all();
    }

    /// Reads `/etc/inittab` (following symlinks); an empty table if there is none. Read with the
    /// BKL held: guest processes are still running at shutdown, and the store (the SQLite
    /// connection, various caches) may only be used under the BKL.
    fn inittab(&self) -> Vec<crate::inittab::Entry> {
        let text = crate::bug::host_call("Machine::inittab", || {
            let k = self.shared.lock();
            let vfs = k.vfs.as_ref()?;
            let offline = crate::pseudo::Offline;
            let root = Cred::root();
            match vfs.resolve(
                &offline,
                &root,
                &Node::ROOT,
                &Node::ROOT,
                b"/etc/inittab",
                true,
            ) {
                Ok(Node::Store(_, ino)) => {
                    let mut text = Vec::new();
                    let f = vfs.store().open_object(ino, false).ok()?;
                    io::Read::read_to_end(&mut &f, &mut text).ok()?;
                    Some(text)
                }
                _ => None,
            }
        });
        text.flatten()
            .map(|t| crate::inittab::parse(&t))
            .unwrap_or_default()
    }

    /// Runs an inittab command by `/bin/sh -c` ([`crate::inittab::script`]) and returns the pid.
    /// Returns None when the command's program is an absolute path that does not exist (OpenRC not
    /// installed, say) or when it cannot start.
    fn run_init_process(&self, process: &[u8], first: bool) -> Option<u32> {
        if let Some(program) = crate::inittab::program(process) {
            // Checked with the BKL held: earlier once commands may already be running (see
            // [`Machine::inittab`])
            let offline = crate::pseudo::Offline;
            let found = crate::bug::host_call("Machine::boot", || {
                let k = self.shared.lock();
                k.vfs.as_ref().is_some_and(|vfs| {
                    vfs.resolve(
                        &offline,
                        &Cred::root(),
                        &Node::ROOT,
                        &Node::ROOT,
                        program,
                        true,
                    )
                    .is_ok()
                })
            });
            if found != Some(true) {
                return None;
            }
        }
        let argv = [
            b"/bin/sh".to_vec(),
            b"-c".to_vec(),
            crate::inittab::script(process, first),
        ];
        let envp: Vec<Vec<u8>> = crate::inittab::ENV.iter().map(|e| e.to_vec()).collect();
        match self.spawn(b"/bin/sh", &argv, &envp, Stdio::Host) {
            Ok(pid) => Some(pid),
            Err(e) => {
                crate::debug::log(format_args!("cannot run an inittab entry: {e}"));
                None
            }
        }
    }

    /// Sends signal `sig` to guest process `pid` from the host (with `SI_KERNEL`, as the kernel's
    /// own signals): ESRCH when there is no such process, EINVAL for a bad signal number.
    pub fn signal(&self, pid: u32, sig: i32) -> Result<(), Errno> {
        let sig = Signal::new(sig).ok_or(Errno::EINVAL)?;
        crate::bug::host_call("Machine::signal", || {
            let k = self.shared.lock();
            let pid = pid as crate::kernel::Tid;
            if !k.groups.contains_key(&pid) {
                return Err(Errno::ESRCH);
            }
            signal::send_to_group(&k, pid, sig, crate::signal::SigInfo::kernel());
            Ok(())
        })
        .unwrap_or(Err(Errno::EIO))
    }

    /// Ends every guest process and waits for every guest thread to exit.
    pub fn kill_all(&self) {
        crate::bug::host_call("Machine::kill_all", || {
            let mut k = self.shared.lock();
            k.stopping = true;
            let groups: Vec<_> = k.groups.values().cloned().collect();
            for g in groups.iter().filter(|g| !g.borrow().threads.is_empty()) {
                signal::group_exit(
                    &k,
                    g,
                    ExitStatus::Signaled {
                        signal: Signal::KILL,
                        pc: None,
                        fault_addr: None,
                        core_dumped: false,
                    },
                );
            }
        });
        self.wait_for(|k| k.tasks.is_empty().then_some(()));
    }

    /// Takes the shutdown request the guest sent to the built-in init (busybox's reboot, halt,
    /// poweroff). Only present after [`Machine::boot`].
    pub fn take_power(&self) -> Option<crate::Power> {
        self.shared.lock().power.take()
    }

    /// The number of guest processes still running (not counting the built-in init and zombies):
    /// after the last tab is closed the app uses it to decide whether to stay in the background or
    /// exit (the android/service map).
    pub fn processes(&self) -> usize {
        let k = self.shared.lock();
        k.groups
            .values()
            .filter(|g| {
                let g = g.borrow();
                g.tgid != INIT && !g.threads.is_empty()
            })
            .count()
    }

    /// CPU nanoseconds used by guest threads, including those already exited. Host UI, polling
    /// and timer threads are excluded, so the host can detect guest activity without keeping
    /// itself awake through its own sampling. Takes the BKL.
    pub fn cpu_time_ns(&self) -> u64 {
        let k = self.shared.lock();
        k.tasks.values().fold(k.cpu.total_ns, |sum, task| {
            sum.saturating_add(task.cpu_ns())
        })
    }

    /// Takes the ports the guest first listened on at non-loopback addresses (design doc §12 item
    /// 7): the app posts a notification from them, telling the user which port other devices
    /// should connect to (low ports are mapped elsewhere, [`crate::portmap`]).
    pub fn take_listens(&self) -> Vec<crate::ListenEvent> {
        crate::bug::host_call("Machine::take_listens", || {
            self.shared.lock().portmap.borrow_mut().take_listens()
        })
        .unwrap_or_default()
    }

    /// Takes the guest processes the kernel layer ended (kernel bug, engine host fault, OOM
    /// killer): the app explains the reason in their tabs, and posts a notification for those not
    /// in a tab (the android/service map).
    pub fn take_kills(&self) -> Vec<crate::KillReport> {
        crate::bug::host_call("Machine::take_kills", || {
            std::mem::take(&mut *self.shared.lock().kills.borrow_mut())
        })
        .unwrap_or_default()
    }

    /// Takes the store paths of directories whose entries the guest changed (created, deleted,
    /// renamed): the app has the Files app refresh them (the android/documents map). Recording
    /// starts only at the first call; deleted directories and those in tmpfs are not reported.
    pub fn take_changed_dirs(&self) -> Vec<Vec<u8>> {
        crate::bug::host_call("Machine::take_changed_dirs", || {
            let k = self.shared.lock();
            let Some(vfs) = &k.vfs else {
                return Vec::new();
            };
            let store = vfs.store();
            store
                .take_changed_dirs()
                .into_iter()
                .filter_map(|d| crate::vfs::browse::dir_path(store, d).ok())
                .collect()
        })
        .unwrap_or_default()
    }

    /// The latest hold of the BKL that lasted longer than the watchdog's threshold
    /// ([`crate::watchdog`]), ended or not. Does not take the BKL, so it answers while that hold
    /// goes on: the app calls it outside the thread its other calls run on, which waits too.
    pub fn stall(&self) -> Option<crate::Stall> {
        self.shared
            .stall
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The summed statistics of exited threads when using Tier 1.
    pub fn dbt_stats(&self) -> Option<DbtStats> {
        self.shared.lock().dbt_stats
    }

    /// Checks `cond` repeatedly with the BKL held until it gives a result.
    fn wait_for<T>(&self, cond: impl FnMut(&mut Kernel) -> Option<T>) -> T {
        self.wait_on(wake::ALL, cond)
    }

    /// The same as [`Machine::wait_for`], but woken only by the channels in `interest` ([`wake`]).
    fn wait_on<T>(&self, interest: u32, cond: impl FnMut(&mut Kernel) -> Option<T>) -> T {
        self.wait_objects(interest, None, cond)
    }

    /// The same as [`Machine::wait_on`]; when `objects` is not None, changes that affect only other
    /// objects do not wake it ([`crate::kernel::Cx::wait_objects`]).
    fn wait_objects<T>(
        &self,
        interest: u32,
        objects: Option<&[wake::Object]>,
        cond: impl FnMut(&mut Kernel) -> Option<T>,
    ) -> T {
        self.wait_deadline(interest, objects, None, cond)
            .expect("waited without a deadline")
    }

    /// The same as [`Machine::wait_on`], returning None if nothing comes by `deadline`.
    fn wait_until<T>(
        &self,
        interest: u32,
        deadline: Option<std::time::Instant>,
        cond: impl FnMut(&mut Kernel) -> Option<T>,
    ) -> Option<T> {
        self.wait_deadline(interest, None, deadline, cond)
    }

    fn wait_deadline<T>(
        &self,
        interest: u32,
        objects: Option<&[wake::Object]>,
        deadline: Option<std::time::Instant>,
        mut cond: impl FnMut(&mut Kernel) -> Option<T>,
    ) -> Option<T> {
        EVENT.with(|event| {
            let mut k = self.shared.lock();
            loop {
                if let Some(v) = cond(&mut k) {
                    return Some(v);
                }
                if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                    return None;
                }
                let (k2, _) =
                    block_for(&self.shared, k, event, deadline, &mut [], interest, objects);
                k = k2;
            }
        })
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.kill_all();
        self.sync();
        {
            let mut k = self.shared.lock();
            k.shutting_down = true;
            request_wake();
        }
        if let Some(t) = self.timers.take() {
            let _ = t.join();
        }
        if let Some(w) = self.watchdog.take() {
            w.stop();
        }
    }
}

/// The timer thread: waits until the earliest deadline and sends the signals of due interval
/// timers, POSIX timers and CPU-time limits ([`crate::itimer::expire`]); checks signal-driven
/// I/O, `EPOLLEXCLUSIVE` gates, AIO polls that signal eventfds and raw ICMP taps; while guest
/// threads are running, periodically
/// checks the memory limit, unmaps the pages the code cache wrote through its writable views
/// ([`crate::oom`]) and samples the load average ([`crate::loadavg`]); commits the inode store's
/// deferred write transactions when due; until the Machine shuts down.
///
/// It keeps running when it hits a kernel-layer bug ([`crate::bug`]): an error while handling one
/// process's timers ends that process; the memory-limit check and signal-driven I/O cannot tell
/// which process is at fault, so an error turns that feature off.
fn timers(shared: Arc<Shared>) {
    let event = shared.timers.clone();
    // Jumps of the host's wall clock (set, or the device resuming): wall-clock deadlines on the
    // realtime and boottime bases move, so every waiter must look again
    let watch = tuxport_hal::ClockWatch::new();
    let mut k = shared.lock();
    while !k.shutting_down {
        if watch.as_ref().is_some_and(tuxport_hal::ClockWatch::take) {
            crate::itimer::clock_jumped(&k);
        }
        let mut next = crate::itimer::expire(&k);
        match crate::bug::catch(|| crate::oom::check(&mut k, &shared)) {
            Some(Some(check)) => next = Some(next.map_or(check, |n| n.min(check))),
            Some(None) => {}
            None => {
                crate::bug::report(format_args!(
                    "kernel panic in the OOM killer; the memory limit is off"
                ));
                k.oom.limit = u64::MAX;
            }
        }
        // The OOM killer's memory events (memory.events) for watchers of cgroup files
        if crate::bug::catch(|| crate::cgroup::notify_changes(&mut k)).is_none() {
            crate::bug::report(format_args!("kernel panic while notifying cgroup changes"));
        }
        match crate::bug::catch(|| crate::oom::unmap_code_views(&mut k, &shared)) {
            Some(Some(due)) => next = Some(next.map_or(due, |n| n.min(due))),
            Some(None) => {}
            None => crate::bug::report(format_args!(
                "kernel panic while unmapping the written code views"
            )),
        }
        match crate::bug::catch(|| crate::loadavg::sample(&mut k, &shared)) {
            Some(Some(due)) => next = Some(next.map_or(due, |n| n.min(due))),
            Some(None) => {}
            None => crate::bug::report(format_args!("kernel panic in the load average")),
        }
        // The inode store's deferred write transactions: deferring starts by waking this thread
        if let Some(vfs) = k.vfs.clone() {
            match crate::bug::catch(|| vfs.store().commit_due()) {
                Some(Some(due)) => next = Some(next.map_or(due, |n| n.min(due))),
                Some(None) => {}
                None => crate::bug::report(format_args!(
                    "kernel panic while committing the inode store"
                )),
            }
        }
        // Signal-driven I/O: every change wakes this thread, which checks along the way; host
        // sockets are waited on together
        let fasync = crate::bug::catch(|| crate::syscall::check_fasync(&mut k));
        let mut host = fasync.unwrap_or_else(|| {
            crate::bug::report(format_args!(
                "kernel panic in signal-driven I/O; no more SIGIO for the files that used it"
            ));
            k.fasync.clear();
            Vec::new()
        });
        // EPOLLEXCLUSIVE exclusive gates: this thread watches the files for the waiting threads,
        // and a new edge wakes only one
        match crate::bug::catch(|| crate::syscall::check_exclusive(&mut k)) {
            Some((fds, due)) => {
                host.extend(fds);
                if let Some(due) = due {
                    next = Some(next.map_or(due, |n| n.min(due)));
                }
            }
            None => {
                crate::bug::report(format_args!(
                    "kernel panic in EPOLLEXCLUSIVE wakeups; waking every exclusive waiter"
                ));
                for gate in std::mem::take(&mut k.ex_gates) {
                    gate.wake_all();
                }
            }
        }
        // AIO polls that signal an eventfd: completed once ready, as Linux does from the file's
        // wake-up, since the program may wait on the eventfd rather than in io_getevents
        match crate::bug::catch(|| crate::syscall::check_aio_polls(&mut k)) {
            Some((fds, due)) => {
                host.extend(fds);
                if let Some(due) = due {
                    next = Some(next.map_or(due, |n| n.min(due)));
                }
            }
            None => {
                crate::bug::report(format_args!(
                    "kernel panic in AIO polls; their eventfds are signalled only by io_getevents"
                ));
                k.aio_polls.clear();
            }
        }
        // DNS stub answers resolved by host threads: queue them on their sockets
        if crate::bug::catch(|| crate::syscall::deliver_dns(&mut k, &shared.dns_answers)).is_none()
        {
            crate::bug::report(format_args!(
                "kernel panic while delivering DNS stub answers"
            ));
        }
        // Collection point of raw ICMP sockets: takes arrived packets for poll and epoll waiters
        match crate::bug::catch(|| crate::rawicmp::check_taps(&mut k)) {
            Some(fds) => host.extend(fds),
            None => crate::bug::report(format_args!("kernel panic in the raw ICMP sockets")),
        }
        if let Some(w) = &watch {
            host.push((w.fd(), 1, 0));
        }
        // Setting a timer is not a change on an open file; every change needs watching only while
        // signal-driven I/O, exclusive gates or AIO polls with eventfds exist
        let interest = if k.fasync.is_empty() && k.ex_gates.is_empty() && k.aio_polls.is_empty() {
            wake::OTHER
        } else {
            wake::ALL
        };
        let (k2, _) = block(&shared, k, &event, next, &mut host, interest);
        k = k2;
    }
}

/// Runs a host call on console `id`. On a kernel-layer panic the console is closed as if the
/// terminal were hung up (the session leader gets SIGHUP), and EIO is returned.
fn on_console<T>(
    k: &mut Kernel,
    id: u32,
    what: &str,
    f: impl FnOnce(&mut Kernel) -> Result<T, Errno>,
) -> Result<T, Errno> {
    crate::bug::host_call(what, || f(k)).unwrap_or_else(|| {
        let _ = crate::bug::host_call("Machine::close_console", || terminal::close(k, id));
        Err(Errno::EIO)
    })
}

/// Loads a guest program and runs it until it ends, then ends the other processes
/// (`tuxport-run`).
///
/// With [`RunOptions::rootfs`] set, `program` is a path in the guest filesystem;
/// otherwise it is a host path.
pub fn run(
    program: &OsStr,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    options: &RunOptions,
) -> Result<RunReport, RunError> {
    let machine = Machine::start(&MachineOptions {
        tier: options.tier,
        rootfs: options.rootfs.clone(),
        memory_limit: options.memory_limit,
        low_port_offset: options.low_port_offset,
        always_map_low_ports: options.always_map_low_ports,
        exception_trace: false,
    })?;
    // With a terminal, the process is a session leader, and the console connected to the host
    // terminal is its controlling terminal
    let console = options
        .terminal
        .map(|term| {
            let id = machine.open_console_with(terminal::initial_size(&term));
            id.map(|id| (term, id)).map_err(RunError::Console)
        })
        .transpose()?;
    let stdio = console.map_or(Stdio::Host, |(_, id)| Stdio::Console(id));
    if options.init {
        machine.boot();
    }
    let pid = machine.spawn(program.as_bytes(), argv, envp, stdio)?;
    let bridge = console.map(|(term, id)| {
        let shared = machine.shared.clone();
        std::thread::Builder::new()
            .name("terminal".into())
            .spawn(move || terminal::bridge(shared, term, id, pid as i32))
            .expect("cannot start the terminal thread")
    });
    let done =
        tuxport_hal::Event::new().map_err(|e| RunError::Io(io::Error::from_raw_os_error(e)))?;
    let status = std::thread::scope(|scope| {
        // Stop requests from the host (RunOptions::stop): the first signal goes to the program,
        // a later one ends everything; either way the program's end comes back below
        if let Some(stop) = options.stop {
            let (machine, done) = (&machine, &done);
            scope.spawn(move || {
                // `timeout` and terminals signal the whole process group too, so the same request
                // can arrive twice at once: only a signal at least a second after the one passed on
                // ends everything
                const AGAIN: std::time::Duration = std::time::Duration::from_secs(1);
                let mut asked: Option<std::time::Instant> = None;
                loop {
                    const POLLIN: i16 = 1;
                    let mut fds = [(stop, POLLIN, 0), (done.fd(), POLLIN, 0)];
                    if tuxport_hal::poll(&mut fds, None).is_err() {
                        continue;
                    }
                    if fds[1].2 != 0 {
                        return;
                    }
                    let mut sig = [0u8; 1];
                    if tuxport_hal::read(stop, &mut sig) != Ok(1) {
                        continue;
                    }
                    match asked {
                        Some(at) if at.elapsed() >= AGAIN => machine.kill_all(),
                        Some(_) => {}
                        None => {
                            asked = Some(std::time::Instant::now());
                            let _ = machine.signal(pid, i32::from(sig[0]));
                        }
                    }
                }
            });
        }
        let status = if options.init {
            // The program ending means shutdown; a guest request to reboot is also only a
            // shutdown, ending tuxport-run
            let power = machine.wait_or_power(pid).err();
            machine.shutdown(power.unwrap_or(crate::Power::PowerOff), SHUTDOWN_TIMEOUT);
            machine.wait(pid)
        } else {
            let status = machine.wait(pid);
            machine.kill_all();
            status
        };
        done.signal();
        status
    });
    // The bridge thread exits only after writing out the remaining output
    if let Some(b) = bridge {
        let _ = b.join();
    }
    Ok(RunReport {
        status,
        dbt_stats: machine.dbt_stats(),
    })
}

/// The host changed `changes` in store `store`: hands them to the running kernels that use it,
/// each of which reports inotify events as the matching syscall would, with the BKL held
/// ([`crate::syscall::host_change`]).
pub(crate) fn store_changed(store: &std::path::Path, changes: &[crate::vfs::browse::Change]) {
    if changes.is_empty() {
        return;
    }
    let dir = store.canonicalize().unwrap_or_else(|_| store.to_path_buf());
    let kernels: Vec<Arc<Shared>> = {
        let mut running = RUNNING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        running.retain(|(_, w)| w.strong_count() > 0);
        running
            .iter()
            .filter(|(d, _)| *d == dir)
            .filter_map(|(_, w)| w.upgrade())
            .collect()
    };
    for shared in kernels {
        let k = shared.lock();
        for c in changes {
            crate::syscall::host_change(&k, c);
        }
    }
}
