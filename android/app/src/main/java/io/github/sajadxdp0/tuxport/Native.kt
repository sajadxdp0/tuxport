package io.github.sajadxdp0.tuxport

import java.io.IOException

/**
 * JNI interface of the native library libtuxport.so, implemented in hosts/android/src/lib.rs.
 *
 * `machine` is the handle [start] returns. Using it after [stop] is safe too: reads and writes
 * return -1, [waitFor] returns -9 (the processes were all ended by SIGKILL), opening a console or
 * starting a process throws, and everything else does nothing. Blocking calls ([read], [write],
 * [waitFor]) may only be made on background threads.
 */
internal object Native {
    init {
        System.loadLibrary("tuxport")
        init(BuildConfig.DEBUG)
    }

    /**
     * Sends stderr to logcat; with [verbose], turns on the kernel layer's debug log. Called by the
     * init block.
     */
    @JvmStatic
    private external fun init(verbose: Boolean)

    /** Creates an inode store in the empty directory [store] and imports a rootfs tar.gz. */
    @JvmStatic
    @Throws(IOException::class)
    external fun importRootfs(store: String, tarball: String)

    /**
     * Writes store [store] to disk (with its whole filesystem): after an import and writing the
     * configuration, before renaming it into place.
     */
    @JvmStatic
    @Throws(IOException::class)
    external fun syncRootfs(store: String)

    /** Writes a file in the store; with [overwrite] false, an existing file is left alone. */
    @JvmStatic
    @Throws(IOException::class)
    external fun writeFile(store: String, path: String, data: ByteArray, mode: Int, overwrite: Boolean)

    /** Reads a regular file in the store (null when missing); also works while the kernel runs. */
    @JvmStatic
    @Throws(IOException::class)
    external fun readFile(store: String, path: String): ByteArray?

    // ---- The host browsing the store (DocumentsProvider, the android/documents map) ----
    // Paths are document IDs: the guest's absolute paths, with `%`, control characters and bytes
    // that are not UTF-8 written as `%XX`. Each call opens the store separately, which also works
    // while the kernel runs; the caller holds the read lock of [Rootfs.access]. Metadata is
    // `<d|f>\t<bytes>\t<modification time (ms)>\t<permission bits>`. Only directories and regular
    // files (following symbolic links); anything else, or anything missing, throws
    // FileNotFoundException.

    /** Metadata of the directory or regular file a path points at. */
    @JvmStatic
    @Throws(IOException::class)
    external fun browseStat(store: String, path: String): String

    /** Lists a directory: entries are `<name>\t<metadata>`, with names written like paths. */
    @JvmStatic
    @Throws(IOException::class)
    external fun browseList(store: String, dir: String): Array<String>

    /** Opens a regular file for reading and returns the fd (for ParcelFileDescriptor.adoptFd). */
    @JvmStatic
    @Throws(IOException::class)
    external fun browseOpen(store: String, path: String): Int

    /**
     * Whether the regular files at these paths look like text (no NUL in the first 4 KB, valid
     * UTF-8): used when the extension does not tell the type ([Documents]). At most 1000 are looked
     * at per call; the rest are false.
     */
    @JvmStatic
    @Throws(IOException::class)
    external fun browseSniff(store: String, paths: Array<String>): BooleanArray

    /**
     * Finds entries under directory [dir] whose name contains [query] (case-insensitive), shallow
     * ones first, at most [limit]: `<path>\t<metadata>`.
     */
    @JvmStatic
    @Throws(IOException::class)
    external fun browseSearch(store: String, dir: String, query: String, limit: Int): Array<String>

    // Changes are all confined to [top] (the login account's home directory), judged by real path:
    // leading out through a symbolic link is not allowed.
    // New names come from the user and are used as they are; returned names are written as paths
    // are.

    /**
     * Creates a file or directory in directory [dir], owned by [uid] and [gid]; when the name is
     * taken another one is used (`a (1).txt`), and the actual name is returned.
     */
    @JvmStatic
    @Throws(IOException::class)
    external fun browseCreate(
        store: String, top: String, dir: String, name: String, directory: Boolean, uid: Int, gid: Int,
    ): String

    /** Removes a file, a symbolic link (only the link) or a whole directory tree. */
    @JvmStatic
    @Throws(IOException::class)
    external fun browseRemove(store: String, top: String, path: String)

    /** Renames within the same directory, without replacing; returns the new name. */
    @JvmStatic
    @Throws(IOException::class)
    external fun browseRename(store: String, top: String, path: String, name: String): String

    /** Moves to directory [to], keeping the name, without replacing. */
    @JvmStatic
    @Throws(IOException::class)
    external fun browseMove(store: String, top: String, path: String, to: String)

    /**
     * Opens a regular file for reading and writing, truncating first with [truncate], with O_APPEND
     * with [append]; returns the fd.
     */
    @JvmStatic
    @Throws(IOException::class)
    external fun browseOpenWrite(store: String, top: String, path: String, truncate: Boolean, append: Boolean): Int

    /**
     * A file handed out by [browseOpenWrite] was closed: tells the running kernel it was written
     * (inotify in the guest).
     */
    @JvmStatic
    external fun browseWritten(store: String, top: String, path: String)

    /** Starts the kernel; [tier] 0 is the interpreter, 1 is the DBT. */
    @JvmStatic
    @Throws(IOException::class)
    external fun start(store: String, tier: Int): Long

    /**
     * Boots by `/etc/inittab` (the kernel/process map; OpenRC on Alpine): blocks until the sysinit
     * and wait commands have all finished, with the output in the guest's `/var/log/boot.log`. Not
     * called on the main thread.
     */
    @JvmStatic
    external fun boot(machine: Long)

    /**
     * The host's default network changed: the guest's routing table becomes [routes], with entries
     * `<destination network in hex>/<prefix length> <gateway in hex or -> <interface>`; with null
     * it goes back to being synthesised from the interfaces' addresses.
     */
    @JvmStatic
    external fun setRoutes(machine: Long, routes: Array<String>?)

    /** Creates a console (an in-process PTY) and returns its number. */
    @JvmStatic
    @Throws(IOException::class)
    external fun openConsole(machine: Long, rows: Int, cols: Int): Int

    /**
     * Starts a session leader with the console as its controlling terminal and returns the pid. A
     * negative [console] means no console (the stdio is the app's).
     */
    @JvmStatic
    @Throws(IOException::class)
    external fun spawn(machine: Long, console: Int, program: String, argv: Array<String>, envp: Array<String>): Int

    /**
     * Reads console output: returns the number of bytes, 0 when the session ended, -1 when the
     * console is closed.
     */
    @JvmStatic
    external fun read(machine: Long, console: Int, buffer: ByteArray): Int

    /** Input to a console: returns the number of bytes written, -1 when the session has ended. */
    @JvmStatic
    external fun write(machine: Long, console: Int, buffer: ByteArray, offset: Int, length: Int): Int

    @JvmStatic
    external fun resize(machine: Long, console: Int, rows: Int, cols: Int)

    /** Closes a console: the session leader receives SIGHUP. */
    @JvmStatic
    external fun closeConsole(machine: Long, console: Int)

    /**
     * Waits for a process to end: returns the exit code, or the negated signal number when a signal
     * terminated it.
     */
    @JvmStatic
    external fun waitFor(machine: Long, pid: Int): Int

    /**
     * The current directory of the console's foreground process group (where restored tabs and new
     * tabs start); null once the shell has exited.
     */
    @JvmStatic
    external fun consoleCwd(machine: Long, console: Int): String?

    /**
     * Takes the ports the guest listened on for the first time on a non-loopback address (design
     * doc §12 item 7): each entry is `<process name>\t<port>\t<host port>`. Ports below 1024 that
     * the host refuses have been moved to other ports, and other devices must connect to the one on
     * the host. Takes the BKL; not called on the main thread.
     */
    @JvmStatic
    external fun takeListens(machine: Long): Array<String>?

    /**
     * Takes the processes the kernel layer ended:
     * `<console or -1>\t<pid>\t<process name>\t<bug|oom>\t<a>\t<b>\t<c>`; for bug, a is the report;
     * for oom, a is the cgroup (empty for the app's limit), b and c the usage and the limit
     * (bytes).
     */
    @JvmStatic
    external fun takeKills(machine: Long): Array<String>?

    /**
     * The latest hold of the kernel lock that lasted longer than the kernel's watchdog allows,
     * ended or not: `<id>\t<seconds>\t<report>\t<kernel log record once ended, else empty>`, or
     * null for none. Does not take the BKL: called on the main thread, since the worker waits for
     * a hold that has not ended.
     */
    @JvmStatic
    external fun bklStall(machine: Long): String?

    /** Takes the guest's power request (busybox's reboot, halt, poweroff): [POWER_NONE] etc. */
    @JvmStatic
    external fun takePower(machine: Long): Int

    const val POWER_NONE = 0
    const val POWER_REBOOT = 1
    const val POWER_HALT = 2
    const val POWER_OFF = 3

    /** Number of guest processes still running (not counting init). */
    @JvmStatic
    external fun processes(machine: Long): Int

    /** Guest CPU nanoseconds, including exited threads; -1 when unavailable. Takes the BKL. */
    @JvmStatic
    external fun guestCpuTime(machine: Long): Long

    /**
     * Takes the directories whose entries (create, delete, rename) the guest changed, written as
     * document IDs, so the Files app refreshes them. Recording starts only with the first call.
     * Takes the BKL; not called on the main thread.
     */
    @JvmStatic
    external fun takeChangedDirs(machine: Long): Array<String>?

    /**
     * Drops caches that can be rebuilt (translations kept in the pool and so on) and commits the
     * deferred write transaction. Takes the BKL; not called on the main thread.
     */
    @JvmStatic
    external fun trimMemory(machine: Long)

    /**
     * Shuts down: runs the shutdown commands in inittab (at most 30 seconds), sends SIGTERM to all
     * processes and waits at most 1 second, ends those left, and then releases the handle. Not
     * called on the main thread.
     */
    @JvmStatic
    external fun shutdown(machine: Long)

    /** Ends all guest processes and releases the handle. */
    @JvmStatic
    external fun stop(machine: Long)
}
