package io.github.sajadxdp0.tuxport

import android.content.Context

/**
 * The PTY of one terminal tab ([VtSession]): a console in the kernel with a login shell running on
 * it; the tab reads, writes and resizes the console itself, in the native library. [login] is
 * the login account from Settings (`auto`, `root` or an account name), from which the assets'
 * `login.sh`, running as root, decides whom to log in (the android/setup map); [cwd] is the
 * directory to start in (tabs restored after the app was killed, the android/service map), null for
 * the home directory.
 */
internal class ConsolePty(
    private val machine: Long,
    private val script: String,
    val login: String,
    private val cwd: String? = null,
) {
    /** The console number in the kernel, -1 before the shell starts. */
    @Volatile
    var console = -1
        private set
    private var pid = -1

    /**
     * Opens a console of [columns] × [rows] and spawns the login script on it; returns its pid.
     * Throws `IOException` when either fails (the console may then be open: [close] it).
     */
    fun start(columns: Int, rows: Int): Int {
        console = Native.openConsole(machine, rows, columns)
        pid = Native.spawn(machine, console, "/bin/sh", argv(script, login, cwd), ENVIRONMENT)
        return pid
    }

    /** Waits for the login script's process (the shell) to exit; its status. */
    fun waitFor(): Int = Native.waitFor(machine, pid)

    /** Closes the console: the session leader gets SIGHUP. */
    fun close() {
        if (console >= 0) Native.closeConsole(machine, console)
    }

    companion object {
        /**
         * Login account in Settings: automatic (the first regular user created, or root when there
         * is none).
         */
        const val LOGIN_AUTO = "auto"
        const val LOGIN_ROOT = "root"

        @Volatile
        private var cached: String? = null

        /**
         * The login script (the assets' `login.sh`): chooses the account; when a regular user first
         * becomes the login account, adds it to the admin group and installs doas or sudo. A
         * regular user is switched to with `su` (`--session-command` when offered, else `-c`)
         * running `exec "$SHELL" -l` in the start directory or home; root logs in with the shell in
         * `/etc/passwd` (also after chsh changed it).
         */
        fun script(context: Context): String = cached
            ?: context.assets.open("login.sh").bufferedReader().readText().also { cached = it }

        /**
         * The argv for running the login script as root
         * (`sh -c script tuxport-login account directory`).
         */
        fun argv(script: String, login: String, cwd: String? = null) =
            arrayOf("sh", "-c", script, "tuxport-login", login, cwd.orEmpty())

        val ENVIRONMENT = arrayOf(
            "HOME=/root",
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            "TERM=xterm-256color",
            "LANG=C.UTF-8",
            "SHELL=/bin/sh",
            "USER=root",
            "LOGNAME=root",
            "BROWSER=/usr/local/bin/xdg-open",
            // Run as root, nmap does scans that need raw sockets by default, which the app cannot
            // create (like root without CAP_NET_RAW), so nmap just exits; with this set it uses
            // connect scans instead
            "NMAP_UNPRIVILEGED=1",
        )
    }
}
