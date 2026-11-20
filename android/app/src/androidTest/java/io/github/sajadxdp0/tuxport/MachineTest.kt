package io.github.sajadxdp0.tuxport

import android.net.ConnectivityManager
import android.provider.DocumentsContract
import android.system.Os
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.io.ByteArrayOutputStream
import java.io.DataInputStream
import java.io.File
import java.io.IOException
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.security.MessageDigest
import kotlin.concurrent.thread

/**
 * Checks the native library on a real device or an emulator: importing a rootfs, starting the
 * kernel, running a shell on a console (the Android-specific tests of design doc §13.3). Run them
 * on devices with both 4K and 16K pages: `./gradlew connectedDebugAndroidTest`.
 */
@RunWith(AndroidJUnit4::class)
class MachineTest {
    private val context = InstrumentationRegistry.getInstrumentation().targetContext
    private val machines = mutableListOf<Long>()

    @Before
    fun prepare() {
        Rootfs.prepare(context, Rootfs.Distro.ALPINE)
        // As the app does before starting the kernel, replace the profile an earlier import wrote
        Rootfs.updateProfile(context)
    }

    @After
    fun stop() {
        machines.forEach(Native::stop)
    }

    private fun start(tier: Int) = Native.start(Rootfs.store(context).path, tier).also { machines.add(it) }

    /** Reads the console until the session ends, normalising line breaks to \n. */
    private fun readAll(machine: Long, console: Int): String {
        val out = ByteArrayOutputStream()
        val buf = ByteArray(4096)
        while (true) {
            val n = Native.read(machine, console, buf)
            if (n <= 0) break
            out.write(buf, 0, n)
        }
        return out.toString("UTF-8").replace("\r\n", "\n")
    }

    /** Runs `program` on a new console; returns the output and the result of [Native.waitFor]. */
    private fun run(machine: Long, program: String, vararg argv: String): Pair<String, Int> {
        val console = Native.openConsole(machine, 24, 80)
        val pid = Native.spawn(machine, console, program, arrayOf(*argv), ConsolePty.ENVIRONMENT)
        val out = readAll(machine, console)
        val code = Native.waitFor(machine, pid)
        Native.closeConsole(machine, console)
        return out to code
    }

    /** Runs `sh -c script` on a new console and returns the output and the exit code. */
    private fun sh(machine: Long, script: String) = run(machine, "/bin/sh", "sh", "-c", script)

    /**
     * Runs the login script on a new console as a terminal tab does ([ConsolePty.script], with
     * [account] as the login account), types [input] and returns the output.
     */
    private fun login(
        machine: Long,
        input: String,
        account: String = ConsolePty.LOGIN_AUTO,
        cwd: String? = null,
    ): String {
        val console = Native.openConsole(machine, 24, 200)
        val argv = ConsolePty.argv(ConsolePty.script(context), account, cwd)
        val pid = Native.spawn(machine, console, "/bin/sh", argv, ConsolePty.ENVIRONMENT)
        var out = ""
        val reader = thread { out = readAll(machine, console) }
        val b = input.toByteArray()
        assertEquals(b.size, Native.write(machine, console, b, 0, b.size))
        Native.waitFor(machine, pid)
        reader.join(5000)
        Native.closeConsole(machine, console)
        return out
    }

    /**
     * [out] without control sequences (CSI, ESC 7 and 8, OSC ended by BEL) and carriage returns,
     * so a command's output is on lines of its own: bash puts its prompt marks and bracketed paste
     * right before it.
     */
    private fun plain(out: String) = out
        .replace(Regex("""\u001b\][^\u0007]*\u0007"""), "")
        .replace(Regex("""\u001b\[[0-?]*[ -/]*[@-~]|\u001b[78]"""), "")
        .replace("\r", "")

    /**
     * Runs the login script like [login] (choosing the account automatically), but interactively
     * step by step: each step waits up to two minutes for `wait` to appear in the output (after the
     * point the previous step waited for), then types `send`. Returns all the output.
     */
    private fun loginSteps(machine: Long, vararg steps: Pair<String, String>): String {
        val console = Native.openConsole(machine, 24, 200)
        val argv = ConsolePty.argv(ConsolePty.script(context), ConsolePty.LOGIN_AUTO, null)
        val pid = Native.spawn(machine, console, "/bin/sh", argv, ConsolePty.ENVIRONMENT)
        val bytes = ByteArrayOutputStream()
        val text = { synchronized(bytes) { bytes.toString("UTF-8") } }
        val reader = thread {
            val buf = ByteArray(4096)
            while (true) {
                val n = Native.read(machine, console, buf)
                if (n <= 0) break
                synchronized(bytes) { bytes.write(buf, 0, n) }
            }
        }
        var from = 0
        for ((wait, send) in steps) {
            val deadline = System.currentTimeMillis() + 120_000
            var at = text().indexOf(wait, from)
            while (at < 0 && System.currentTimeMillis() < deadline) {
                Thread.sleep(50)
                at = text().indexOf(wait, from)
            }
            assertTrue("no \"$wait\" in:\n${text()}", at >= 0)
            from = at + wait.length
            if (send.isEmpty()) continue
            val b = send.toByteArray()
            assertEquals(b.size, Native.write(machine, console, b, 0, b.size))
        }
        Native.waitFor(machine, pid)
        reader.join(5000)
        Native.closeConsole(machine, console)
        return text().replace("\r\n", "\n")
    }

    @Test
    fun busyboxRunsUnderBothTiers() {
        for (tier in 0..1) {
            val (out, code) = sh(
                start(tier),
                "echo hello; getconf PAGESIZE; cat /etc/alpine-release; " +
                    "echo x | sha256sum | cut -c1-16; (sleep 0.1 & wait); ls /proc/self/fd | wc -l; tty",
            )
            assertEquals(out, 0, code)
            val lines = out.lines()
            assertEquals(out, "hello", lines[0])
            // The guest's page size does not depend on the host's (which may be 16K)
            assertEquals(out, "4096", lines[1])
            assertTrue(out, lines[2].startsWith("3."))
            assertEquals(out, "73cb3858a687a849", lines[3])
            assertTrue(out, lines[5].startsWith("/dev/pts/"))
        }
    }

    @Test
    fun guestCpuTimeTracksRunningAndExitedThreads() {
        for (tier in 0..1) {
            val machine = start(tier)
            val before = Native.guestCpuTime(machine)
            assertTrue(before >= 0)
            val pid = Native.spawn(machine, -1, "/bin/sh", arrayOf("sh", "-c", "while :; do :; done"), ConsolePty.ENVIRONMENT)
            val deadline = System.nanoTime() + 5_000_000_000L
            var live = before
            while (live <= before && System.nanoTime() < deadline) {
                Thread.sleep(20)
                live = Native.guestCpuTime(machine)
            }
            assertTrue("Running guest CPU must be visible", live > before)
            assertEquals(0, sh(machine, "kill -TERM $pid").second)
            Native.waitFor(machine, pid)
            val exited = Native.guestCpuTime(machine)
            assertTrue("Reaping must retain CPU time", exited >= live)
            repeat(5) { assertEquals(0, sh(machine, "true").second) }
            val pooled = Native.guestCpuTime(machine)
            assertTrue("Short-lived tasks on pooled threads must count", pooled > exited)
            Thread.sleep(100)
            assertEquals("Host polling and timers must not count as guest activity", pooled, Native.guestCpuTime(machine))
            Native.stop(machine)
            assertEquals(-1L, Native.guestCpuTime(machine))
        }
    }

    /**
     * Booting and shutting down by inittab (the kernel/process map, the android/service map): after
     * boot the once commands are running, the guest's `poweroff` is a request to shut down, and
     * [Native.shutdown] runs the shutdown commands, ends all processes and releases the handle.
     * Before the start, an /etc/network/interfaces with only the loopback interface is written.
     */
    @Test
    fun bootsAndShutsDownByInittab() {
        val store = Rootfs.store(context).path
        val probe = start(1)
        val original = sh(probe, "cat /etc/inittab").first.toByteArray()
        Native.stop(probe)
        val inittab = "::sysinit:/bin/sh -c 'echo sysinit > /tmp/inittab-order'\n" +
            // This is what OpenRC writes when a service did not start (eerror); repeats count once
            "::sysinit:/bin/sh -c 'for s in fake fake other; do echo \" * ERROR: \$s failed to start\"; done'\n" +
            "::once:/bin/sleep 30\n" +
            "::shutdown:/bin/sh -c 'echo shutdown >> /tmp/inittab-order'\n"
        Native.writeFile(store, "/etc/inittab", inittab.toByteArray(), 420, true)
        try {
            Rootfs.ensureNetworkConfig(context)
            val machine = start(1)
            Native.boot(machine)
            assertTrue(Native.processes(machine) >= 1)
            assertEquals("sysinit\n" to 0, sh(machine, "cat /tmp/inittab-order"))
            assertEquals(listOf("fake", "other"), Rootfs.failedServices(context))
            assertEquals(Native.POWER_NONE, Native.takePower(machine))
            assertEquals(0, sh(machine, "poweroff").second)
            assertEquals(Native.POWER_OFF, Native.takePower(machine))
            assertEquals(Native.POWER_NONE, Native.takePower(machine))
            val begin = System.nanoTime()
            Native.shutdown(machine)
            assertTrue(System.nanoTime() - begin < 10_000_000_000L)
            assertEquals(0, Native.processes(machine))
            val after = start(1)
            assertEquals("sysinit\nshutdown\n" to 0, sh(after, "cat /tmp/inittab-order"))
            assertTrue(sh(after, "cat /etc/network/interfaces").first.contains("iface lo inet loopback"))
        } finally {
            machines.forEach(Native::stop)
            Native.writeFile(store, "/etc/inittab", original, 420, true)
        }
    }

    /**
     * Before boot, osclock is added to boot ([Rootfs.PREFER_OSCLOCK]): it does nothing when OpenRC
     * is not installed; a fake rc-update shows when it is called, and it is not called when the
     * user put another clock service there. Also checks [Native.spawn] without a console.
     */
    @Test
    fun prefersOsclockOverHwclock() {
        val machine = start(1)
        // The minirootfs has no OpenRC directories: if they exist OpenRC was installed, leave it
        assumeTrue(sh(machine, "test -e /etc/init.d -o -e /etc/runlevels").second != 0)
        val argv = arrayOf("sh", "-c", Rootfs.PREFER_OSCLOCK)
        fun prefer() = Native.waitFor(machine, Native.spawn(machine, -1, "/bin/sh", argv, ConsolePty.ENVIRONMENT))
        assertEquals(0, prefer())
        val fake = "mkdir -p /etc/init.d /etc/runlevels/boot && touch /etc/init.d/osclock && " +
            "printf '#!/bin/sh\\necho \"\$*\" >> /tmp/rc-update-args\\n' > /sbin/rc-update && " +
            "chmod +x /sbin/rc-update && rm -f /tmp/rc-update-args"
        try {
            assertEquals(0, sh(machine, fake).second)
            assertEquals(0, prefer())
            assertEquals("-q add osclock boot\n" to 0, sh(machine, "cat /tmp/rc-update-args"))
            assertEquals(0, sh(machine, "rm /tmp/rc-update-args; ln -s /etc/init.d/hwclock /etc/runlevels/boot/hwclock").second)
            assertEquals(0, prefer())
            assertEquals(1, sh(machine, "test -e /tmp/rc-update-args").second)
        } finally {
            sh(machine, "rm -rf /sbin/rc-update /etc/init.d /etc/runlevels /tmp/rc-update-args")
        }
    }

    @Test
    fun interactiveShellTakesInputAndControlC() {
        val machine = start(1)
        val console = Native.openConsole(machine, 24, 80)
        val pid = Native.spawn(machine, console, "/bin/sh", arrayOf("sh", "-i"), ConsolePty.ENVIRONMENT)
        var output = ""
        val reader = thread { output = readAll(machine, console) }
        fun type(s: String) {
            val b = s.toByteArray()
            assertEquals(b.size, Native.write(machine, console, b, 0, b.size))
        }
        type("sleep 30\n")
        Thread.sleep(500)
        // ^C goes through the line discipline and becomes a SIGINT to the foreground process group
        type("\u0003")
        type("echo status=\$?\n")
        type("exit 7\n")
        assertEquals(7, Native.waitFor(machine, pid))
        reader.join(5000)
        assertTrue(output, output.contains("status=130"))
    }

    @Test
    fun consolesRunConcurrently() {
        val machine = start(1)
        val results = Array(3) { "" }
        val workers = (0 until 3).map { i ->
            thread { results[i] = sh(machine, "for n in 1 2 3; do echo $i-\$n; sleep 0.05; done").first }
        }
        workers.forEach { it.join(10_000) }
        for (i in 0 until 3) assertEquals(results[i], "$i-1\n$i-2\n$i-3\n", results[i])
    }

    /**
     * The current directory of the console's foreground process group follows the shell's cd;
     * after the shell exits there is none.
     */
    @Test
    fun consoleCwdFollowsTheForegroundShell() {
        val machine = start(1)
        val console = Native.openConsole(machine, 24, 80)
        val argv = ConsolePty.argv(ConsolePty.script(context), ConsolePty.LOGIN_ROOT)
        val pid = Native.spawn(machine, console, "/bin/sh", argv, ConsolePty.ENVIRONMENT)
        val reader = thread { readAll(machine, console) }
        fun waitFor(want: String?): String? {
            val deadline = System.currentTimeMillis() + 10_000
            var cwd = Native.consoleCwd(machine, console)
            while (cwd != want && System.currentTimeMillis() < deadline) {
                Thread.sleep(50)
                cwd = Native.consoleCwd(machine, console)
            }
            return cwd
        }
        assertEquals("/root", waitFor("/root"))
        val cd = "cd /usr/lib\n".toByteArray()
        Native.write(machine, console, cd, 0, cd.size)
        assertEquals("/usr/lib", waitFor("/usr/lib"))
        val exit = "exit\n".toByteArray()
        Native.write(machine, console, exit, 0, exit.size)
        Native.waitFor(machine, pid)
        assertEquals(null, waitFor(null))
        reader.join(5000)
        Native.closeConsole(machine, console)
    }

    /**
     * The login shell is root's shell in /etc/passwd, also after chsh changed it; it reads
     * /etc/profile.
     */
    @Test
    fun loginShellComesFromPasswd() {
        val machine = start(1)
        val shell = sh(machine, "awk -F: '\$1 == \"root\" { print \$7 }' /etc/passwd").first.trim()
        val out = login(machine, "echo shell=\$SHELL\nexit\n")
        assertTrue(out, out.contains("shell=$shell\n"))
        assertTrue(out, out.contains("Welcome to Tuxport"))
        assertEquals("" to 0, sh(machine, "sed -i '1s|:[^:]*\$|:/bin/ash|' /etc/passwd"))
        try {
            val changed = login(machine, "echo shell=\$SHELL\nexit\n")
            assertTrue(changed, changed.contains("shell=/bin/ash\n"))
        } finally {
            sh(machine, "sed -i '1s|:[^:]*\$|:$shell|' /etc/passwd")
        }
    }

    /**
     * The profile gives busybox ash as the login shell (root's here, as on the systems imported
     * before bash was the login shell; [newSystemsLogInWithBash] has bash's) its shell integration
     * (the android/vtcore map): each prompt reports the last command's status (OSC 133 D) and starts
     * the prompt (A, whose cursor taps move with the arrows, and which asks the terminal to read
     * ash's line feeds), the typed command starts after it (B), and continuation lines are
     * secondary prompts. The marks come from the file ENV names (the android/setup map).
     */
    @Test
    fun profileMarksPrompts() {
        val machine = start(1)
        val shell = sh(machine, "awk -F: '\$1 == \"root\" { print \$7 }' /etc/passwd").first.trim()
        assertEquals("" to 0, sh(machine, "sed -i '1s|:[^:]*\$|:/bin/sh|' /etc/passwd"))
        try {
            val out = login(machine, "false\nfor i in 1; do\ntrue; done\nexit\n", ConsolePty.LOGIN_ROOT)
            assertTrue(out, out.contains("\u001b]133;D;1\u0007\u001b]133;A;cl=line;tuxport_lf=output\u0007"))
            assertTrue(out, Regex("""\u001b]133;A;cl=line;tuxport_lf=output\u0007.*# \u001b]133;B\u0007""").containsMatchIn(out))
            assertTrue(out, out.contains("\u001b]133;P;k=s;tuxport_lf=output\u0007> \u001b]133;B\u0007"))
            // The loop's status, after its continuation line
            assertTrue(out, out.contains("true; done\n\u001b]133;D;0\u0007"))
            // A prompt that ~/.profile sets gets the marks when ash reads ENV after it, and so does
            // a nested ash, which reads no profile
            try {
                sh(machine, "echo 'PS1=\"mine> \"' > /root/.profile")
                val own = login(machine, "ash\nexit\nexit\n", ConsolePty.LOGIN_ROOT)
                assertTrue(own, own.contains("\u001b]133;A;cl=line;tuxport_lf=output\u0007mine> \u001b]133;B\u0007"))
                assertTrue(own, Regex("""\u001b]133;A;cl=line;tuxport_lf=output\u0007[^\u001b]*# \u001b]133;B\u0007""").containsMatchIn(own))
            } finally {
                sh(machine, "rm -f /root/.profile")
            }
        } finally {
            sh(machine, "sed -i '1s|:[^:]*\$|:$shell|' /etc/passwd")
        }
    }

    /**
     * The startup updates recognize the app's own files by their authorship rather than by exact
     * bytes: a profile or motd that still carries Tuxport's marks is refreshed even when no known
     * text matches it (a store can hold an edited or otherwise diverged copy), while one that no
     * longer looks like Tuxport's is kept as it is.
     */
    @Test
    fun updatesRefreshAuthoredFilesAndKeepCustomizedOnes() {
        val store = Rootfs.store(context).path
        fun read(path: String) = String(Native.readFile(store, path)!!)
        fun write(path: String, text: String) = Native.writeFile(store, path, text.toByteArray(), 420 /* 0644 */, true)
        val profile = "/etc/profile.d/tuxport.sh"

        write(profile, read(profile) + "\n# a local change\n")
        write("/etc/motd", "Welcome to Tuxport!\nAlpine Linux 3.24, running on Android.\n\nAn unknown older copy.\n")
        Rootfs.updateProfile(context)
        Rootfs.updateMotd(context)
        assertTrue(read(profile), "The app's mark" in read(profile))
        assertTrue(read("/etc/motd"), "install the OpenCode CLI" in read("/etc/motd"))

        write(profile, "#!/bin/sh\necho mine\n")
        write("/etc/motd", "My own welcome.\n")
        Rootfs.updateProfile(context)
        Rootfs.updateMotd(context)
        assertEquals("#!/bin/sh\necho mine\n", read(profile))
        assertEquals("My own welcome.\n", read("/etc/motd"))
    }

    /**
     * A system imported from Alpine logs in with bash (the android/setup map): root's shell is
     * /bin/bash, and while the packages the app bundles are missing (moved away here) the login
     * uses /bin/sh; the next login with them installs them offline and deletes them. bash's
     * prompts have its marks (D with the status, then A; P and B around PS1; C from PS0), a nested
     * bash marks its own through Alpine's bashrc and a nested sh has ash's, and an account adduser
     * makes from there gets bash. Imports into a store of its own.
     */
    @Test
    fun newSystemsLogInWithBash() {
        val store = File(context.cacheDir, "store-bash")
        store.deleteRecursively()
        Rootfs.prepare(context, Rootfs.Distro.ALPINE, store = store)
        val machine = Native.start(store.path, 1)
        try {
            assertEquals("" to 0, sh(machine, "mv /var/cache/tuxport/bash /var/cache/tuxport/bash.off"))
            val fallback = plain(login(machine, "echo shell=\$0\nexit\n", ConsolePty.LOGIN_ROOT))
            assertTrue(fallback, fallback.contains("\nshell=/bin/sh\n"))
            assertEquals("" to 0, sh(machine, "mv /var/cache/tuxport/bash.off /var/cache/tuxport/bash"))
            val out = login(
                machine,
                "echo shell=\$0 \$BASH_VERSION\nfalse\nbash\nexit\nsh\nexit\n" +
                    "adduser -D bob; grep ^bob: /etc/passwd\n" +
                    "[ -e /var/cache/tuxport/bash ] || echo bundle-gone; grep -qx bash /etc/apk/world && echo in-world\n" +
                    "exit\n",
                ConsolePty.LOGIN_ROOT,
            )
            val text = plain(out)
            assertTrue(out, text.contains("\nshell=/bin/bash 5."))
            assertTrue(out, out.contains("\u001b]133;D;1\u0007\u001b]133;A;redraw=last;cl=line\u0007"))
            assertTrue(out, Regex("""\u001b]133;P;k=i\u0007.*# \u001b]133;B\u0007""").containsMatchIn(out))
            assertTrue(out, out.contains("\u001b]133;C\u0007"))
            // The nested bash's prompt (Alpine's, without colours) and the nested sh's
            assertTrue(out, Regex("""\u001b]133;P;k=i\u0007[^\u001b]*# \u001b]133;B\u0007""").containsMatchIn(out))
            assertTrue(out, out.contains("\u001b]133;A;cl=line;tuxport_lf=output\u0007"))
            assertTrue(out, text.contains(":/home/bob:/bin/bash\n"))
            assertTrue(out, text.contains("\nbundle-gone\n") && text.contains("\nin-world\n"))
        } finally {
            Native.stop(machine)
            store.deleteRecursively()
        }
    }

    /** Offline procps-ng install is independent of bash, retries failures, and respects removal. */
    @Test
    fun newSystemsInstallProcpsOffline() {
        val store = File(context.cacheDir, "store-procps")
        store.deleteRecursively()
        Rootfs.prepare(context, Rootfs.Distro.ALPINE, store = store)
        val machine = Native.start(store.path, 1)
        try {
            // Keep ash as the login shell and omit a required library for the first attempt.
            assertEquals(0, sh(machine,
                "sed -i 's@/bin/bash$@/bin/sh@' /etc/passwd; " +
                    "mkdir /var/cache/tuxport/procps.off; " +
                    "mv /var/cache/tuxport/procps/libproc2-*.apk /var/cache/tuxport/procps.off/; " +
                    "rm -f /etc/apk/repositories"
            ).second)
            val failed = plain(login(machine, "echo login-ok\nexit\n", ConsolePty.LOGIN_ROOT))
            assertTrue(failed, failed.contains("cannot install procps-ng now"))
            assertTrue(failed, failed.contains("\nlogin-ok\n"))
            assertEquals(0, sh(machine,
                "test -d /var/cache/tuxport/procps && ! apk info -e procps-ng"
            ).second)
            assertEquals(0, sh(machine,
                "mv /var/cache/tuxport/procps.off/*.apk /var/cache/tuxport/procps/"
            ).second)

            // Two tabs retry together: both must reach a working procps-ng after sharing the lock.
            val outputs = arrayOf("", "")
            val logins = (0..1).map { i ->
                thread { outputs[i] = login(machine, "ps --version\nexit\n", ConsolePty.LOGIN_ROOT) }
            }
            logins.forEach { it.join(120_000) }
            assertTrue("first logins did not finish", logins.none { it.isAlive })
            for (out in outputs) assertTrue(out, plain(out).contains("\nps from procps-ng "))
            assertEquals("" to 0, sh(machine,
                "test ! -e /var/cache/tuxport/procps && grep -qx procps-ng /etc/apk/world && " +
                    "test ! -x /bin/bash"
            ))
            val smoke = sh(machine,
                "ps -eo pid,comm && top -b -n 1 && free -m && vmstat && uptime && " +
                    "pgrep -x sh && echo procps-ok"
            )
            assertEquals(smoke.first, 0, smoke.second)
            assertTrue(smoke.first, smoke.first.contains("procps-ok"))

            assertEquals(0, sh(machine, "apk del --no-network procps-ng").second)
            val removed = plain(login(machine, "echo login-ok\nexit\n", ConsolePty.LOGIN_ROOT))
            assertTrue(removed, removed.contains("\nlogin-ok\n"))
            assertEquals(0, sh(machine, "! apk info -e procps-ng && test ! -e /var/cache/tuxport/procps").second)
        } finally {
            Native.stop(machine)
            store.deleteRecursively()
        }
    }

    /** Offline bubblewrap installation retries safely and works for an unprivileged user. */
    @Test
    fun newSystemsInstallBubblewrapOffline() {
        val store = File(context.cacheDir, "store-bubblewrap")
        store.deleteRecursively()
        Rootfs.prepare(context, Rootfs.Distro.ALPINE, store = store)
        val machine = Native.start(store.path, 1)
        try {
            // No repositories or other bundles: bubblewrap must carry its own dependencies.
            assertEquals(0, sh(machine,
                "sed -i 's@/bin/bash$@/bin/sh@' /etc/passwd; " +
                    "rm -rf /var/cache/tuxport/bash /var/cache/tuxport/procps; " +
                    "mkdir /var/cache/tuxport/bubblewrap.off; " +
                    "mv /var/cache/tuxport/bubblewrap/libcap2-*.apk /var/cache/tuxport/bubblewrap.off/; " +
                    "rm -f /etc/apk/repositories"
            ).second)
            val failed = plain(login(machine, "echo login-ok\nexit\n", ConsolePty.LOGIN_ROOT))
            assertTrue(failed, failed.contains("cannot install bubblewrap now"))
            assertTrue(failed, failed.contains("\nlogin-ok\n"))
            assertEquals(0, sh(machine,
                "test -d /var/cache/tuxport/bubblewrap && ! apk info -e bubblewrap"
            ).second)
            assertEquals(0, sh(machine,
                "mv /var/cache/tuxport/bubblewrap.off/*.apk /var/cache/tuxport/bubblewrap/"
            ).second)

            val outputs = arrayOf("", "")
            val logins = (0..1).map { i ->
                thread { outputs[i] = login(machine, "bwrap --version\nexit\n", ConsolePty.LOGIN_ROOT) }
            }
            logins.forEach { it.join(120_000) }
            assertTrue("first logins did not finish", logins.none { it.isAlive })
            for (out in outputs) assertTrue(out, plain(out).contains("\nbubblewrap "))
            assertEquals("" to 0, sh(machine,
                "test ! -e /var/cache/tuxport/bubblewrap && grep -qx bubblewrap /etc/apk/world && " +
                    "test ! -x /bin/bash && ! apk info -e procps-ng"
            ))
            val sandbox = "bwrap --unshare-all --hostname box --ro-bind / / --dev /dev " +
                "--proc /proc --tmpfs /tmp /bin/sh -c " +
                "'test \"\$(hostname)\" = box && touch /tmp/inside && " +
                "test -r /proc/self/status && ! touch /etc/bwrap-readonly 2>/dev/null && echo sandbox-ok'"
            val root = sh(machine, sandbox)
            assertEquals(root.first, 0, root.second)
            assertTrue(root.first, root.first.contains("sandbox-ok"))
            assertEquals(0, sh(machine, "adduser -D -s /bin/sh bwraptest").second)
            val quotedSandbox = "'" + sandbox.replace("'", "'\\''") + "'"
            val unprivileged = sh(machine, "su bwraptest -s /bin/sh -c $quotedSandbox")
            assertEquals(unprivileged.first, 0, unprivileged.second)
            assertTrue(unprivileged.first, unprivileged.first.contains("sandbox-ok"))
            assertEquals(0, sh(machine, "test ! -e /tmp/inside && test ! -e /etc/bwrap-readonly").second)

            assertEquals(0, sh(machine, "apk del --no-network bubblewrap").second)
            val removed = plain(login(machine, "echo login-ok\nexit\n", ConsolePty.LOGIN_ROOT))
            assertTrue(removed, removed.contains("\nlogin-ok\n"))
            assertEquals(0, sh(machine,
                "! apk info -e bubblewrap && test ! -e /var/cache/tuxport/bubblewrap"
            ).second)
        } finally {
            Native.stop(machine)
            store.deleteRecursively()
        }
    }

    /**
     * A restored tab starts in its original directory (the android/service map): root enters it if
     * it can; if the directory is gone, the home directory; a regular user enters as itself, and
     * when it cannot (/root), its own home directory.
     */
    @Test
    fun loginStartsInTheSavedDirectory() {
        Rootfs.ensureLoginTools(context)
        val machine = start(1)
        try {
            assertTrue(plain(login(machine, "pwd; exit\n", ConsolePty.LOGIN_ROOT, "/etc")).contains("\n/etc\n"))
            assertTrue(plain(login(machine, "pwd; exit\n", ConsolePty.LOGIN_ROOT, "/no/such/dir")).contains("\n/root\n"))
            assertTrue(plain(login(machine, "pwd; exit\n", ConsolePty.LOGIN_ROOT)).contains("\n/root\n"))
            assertEquals(0, sh(machine, "adduser -D -s /bin/sh alice").second)
            assertTrue(login(machine, "pwd; exit\n", "alice", "/usr/share").contains("\n/usr/share\n"))
            val denied = login(machine, "pwd; exit\n", "alice", "/root")
            assertTrue(denied, denied.contains("\n/home/alice\n"))
        } finally {
            sh(
                machine,
                "deluser --remove-home alice; apk del -q --no-network --repositories-file /dev/null doas; " +
                    "rm -rf /var/lib/tuxport /etc/doas.d",
            )
        }
    }

    /**
     * The login account (the android/setup map): with only root, root logs in automatically; once a
     * regular user exists it logs in automatically, and on its first login it is added to wheel and
     * the app's bundled doas is installed offline; doas asks no password while none is set, and
     * asks once one is set; choosing root explicitly gives root, and so does a chosen account that
     * no longer exists. Finally removes the account, doas and the records left behind.
     */
    @Test
    fun loginAccountsAndDoas() {
        Rootfs.ensureLoginTools(context)
        val machine = start(1)
        val show = "id -un; id -nG; doas -n id -un; exit\n"
        try {
            assertTrue(plain(login(machine, "id -un; exit\n")).contains("\nroot\n"))
            assertEquals(0, sh(machine, "adduser -D -s /bin/sh alice").second)
            assertEquals(listOf("alice"), Rootfs.regularAccounts(context).map { it.name })
            val first = login(machine, show)
            assertTrue(first, first.contains("alice is in the wheel group and can become root with doas"))
            assertTrue(first, becameAdmin(first, "wheel"))
            assertEquals(0, sh(machine, "echo alice:secret | chpasswd").second)
            val withPassword = login(machine, show)
            assertFalse(withPassword, withPassword.contains("can become root"))
            assertTrue(withPassword, withPassword.contains("doas: Authentication required"))
            assertTrue(plain(login(machine, "id -un; exit\n", ConsolePty.LOGIN_ROOT)).contains("\nroot\n"))
            val missing = plain(login(machine, "id -un; exit\n", "bob"))
            assertTrue(missing, missing.contains("there is no user bob") && missing.contains("\nroot\n"))
        } finally {
            sh(
                machine,
                "deluser --remove-home alice; apk del -q --no-network --repositories-file /dev/null doas; " +
                    "rm -rf /var/lib/tuxport /etc/doas.d",
            )
        }
    }

    /**
     * Two tabs logging in at once as an admin without doas yet both install it (the android/setup
     * map): the login script's installs take turns, as apk fails at once on the lock another apk
     * holds, which left one tab with apk's errors and "not installed yet". While an apk the user
     * runs holds that lock, the login says so, waits for it and then installs.
     */
    @Test
    fun loginsInstallOneAtATime() {
        Rootfs.ensureLoginTools(context)
        val machine = start(1)
        try {
            assertEquals(
                0,
                sh(
                    machine,
                    "adduser -D -s /bin/sh alice && addgroup alice wheel && mkdir -p /var/lib/tuxport && " +
                        "echo alice > /var/lib/tuxport/admins",
                ).second,
            )
            val outs = arrayOfNulls<String>(2)
            val logins = outs.indices.map { i -> thread { outs[i] = plain(login(machine, "command -v doas; exit\n", "alice")) } }
            logins.forEach { it.join() }
            for (out in outs.map { it.orEmpty() }) {
                assertFalse(out, out.contains("Unable to lock") || out.contains("not installed yet"))
                assertTrue(out, out.contains("\n/usr/bin/doas\n"))
            }
            // The user's apk: a process holding apk's lock for 3 s from before the login
            assertEquals(
                0,
                sh(machine, "apk del -q --no-network --repositories-file /dev/null doas && rm -f /var/lib/tuxport/doas-installed").second,
            )
            val holder = Native.spawn(
                machine, -1, "/bin/sh",
                arrayOf("sh", "-c", "flock /lib/apk/db/lock sh -c 'touch /tmp/apk-held; sleep 3'; rm -f /tmp/apk-held"),
                ConsolePty.ENVIRONMENT,
            )
            assertEquals(
                0,
                sh(machine, "i=0; while [ ! -e /tmp/apk-held ] && [ \$i -lt 100 ]; do sleep 0.05; i=\$((i + 1)); done; [ -e /tmp/apk-held ]").second,
            )
            val waited = plain(login(machine, "command -v doas; exit\n", "alice"))
            Native.waitFor(machine, holder)
            assertTrue(waited, waited.contains("another apk is running; waiting for it to finish"))
            assertFalse(waited, waited.contains("Unable to lock") || waited.contains("not installed yet"))
            assertTrue(waited, waited.contains("\n/usr/bin/doas\n"))
        } finally {
            sh(
                machine,
                "deluser --remove-home alice; apk del -q --no-network --repositories-file /dev/null doas; " +
                    "rm -rf /var/lib/tuxport /etc/doas.d",
            )
        }
    }

    /**
     * A password set while the session runs (the android/setup map): until then alice escalates
     * without one; chpasswd replaces `/etc/shadow`, the changed `/etc` (what the service takes with
     * [Native.takeChangedDirs]) makes [Rootfs.dirsChanged] rewrite the rules, and from then on doas
     * asks for the password without a new login. A change elsewhere refreshes nothing.
     */
    @Test
    fun rulesFollowAPasswordSetLater() {
        Rootfs.ensureLoginTools(context)
        val machine = start(1)
        val escalate = "su alice -c 'doas -n id -un' 2>&1"
        try {
            assertEquals(0, sh(machine, "adduser -D -s /bin/sh alice").second)
            val first = login(machine, "id -un; id -nG; doas -n id -un; exit\n")
            assertTrue(first, becameAdmin(first, "wheel"))
            Native.takeChangedDirs(machine)
            assertEquals(-1, Rootfs.dirsChanged(context, machine, arrayOf("/home/alice")))
            assertEquals(0, sh(machine, "echo alice:secret | chpasswd").second)
            val before = sh(machine, escalate).first
            assertTrue(before, before.lines().any { it.trim() == "root" })
            val dirs = Native.takeChangedDirs(machine) ?: emptyArray()
            assertTrue(dirs.joinToString(), "/etc" in dirs)
            val pid = Rootfs.dirsChanged(context, machine, dirs)
            assertTrue(pid > 0)
            Native.waitFor(machine, pid)
            val after = sh(machine, escalate).first
            assertTrue(after, after.contains("doas: Authentication required"))
        } finally {
            sh(
                machine,
                "deluser --remove-home alice; apk del -q --no-network --repositories-file /dev/null doas; " +
                    "rm -rf /var/lib/tuxport /etc/doas.d",
            )
        }
    }

    /**
     * In the login script's output: `id -un` is alice, `id -nG` contains [group],
     * `doas`/`sudo -n id -un` is root (compared by line, without the control sequences and CRs
     * terminal output has mixed in).
     */
    private fun becameAdmin(out: String, group: String): Boolean {
        val lines = plain(out).lines().map { it.trim() }
        val i = lines.indexOf("alice")
        return i >= 0 && lines.getOrNull(i + 1)?.split(' ')?.containsAll(listOf("alice", group)) == true &&
            lines.getOrNull(i + 2) == "root"
    }

    /**
     * Saved tabs (the android/service map): while the marker is still there (killed) they read back
     * unchanged with the pid of the run that left it, also a second time (killed again before the
     * kernel came up); a later save replaces them and leaves only the transcripts its index names;
     * a snapshot reads back as written while an index names it, not when the index was written with
     * another libghostty-vt, and goes with the next save that does not name it; an earlier version's
     * empty marker still counts as killed, with no pid; after a normal exit (clear) nothing is read.
     */
    @Test
    fun tabsComeBackOnlyAfterAKill() {
        Tabs.clear(context)
        val tabs = listOf(
            Tabs.Tab("auto", "/etc", "line 1\nline 2"),
            Tabs.Tab("root", null, ""),
        )
        Tabs.markRunning(context)
        Tabs.save(context, tabs, 1)
        val killed = Tabs.takeKilled(context)
        assertEquals(android.os.Process.myPid(), killed?.pid)
        val saved = killed?.saved
        assertEquals(tabs, saved?.tabs)
        assertEquals(1, saved?.current)
        assertTrue(saved!!.time > 0)
        assertEquals(tabs, Tabs.takeKilled(context)?.saved?.tabs)
        Tabs.save(context, listOf(tabs[0].copy(transcript = "line 3")), 0)
        assertEquals(listOf("line 3"), Tabs.takeKilled(context)?.saved?.tabs?.map { it.transcript })
        val transcripts = File(context.filesDir, "tabs").list()!!.filter { it.endsWith(".txt") }
        assertEquals(transcripts.toString(), 1, transcripts.size)
        val snapshot = ByteArray(100_000) { (it % 7).toByte() }
        val name = Tabs.writeSnapshot(context, snapshot)!!
        Tabs.save(context, listOf(tabs[0].copy(snapshot = name)), 0)
        assertEquals(name, Tabs.takeKilled(context)?.saved?.tabs?.single()?.snapshot)
        assertArrayEquals(snapshot, Tabs.readSnapshot(context, name))
        val index = File(context.filesDir, "tabs/index.json")
        index.writeText(index.readText().replace("\"vt\":", "\"vt\":\"other\",\"was\":"))
        assertEquals(null, Tabs.takeKilled(context)?.saved?.tabs?.single()?.snapshot)
        Tabs.save(context, listOf(tabs[0]), 0)
        assertFalse(File(context.filesDir, "tabs/$name").exists())
        File(context.filesDir, "tabs/running").writeText("")
        assertEquals(0, Tabs.takeKilled(context)?.pid)
        Tabs.clear(context)
        assertEquals(null, Tabs.takeKilled(context))
    }

    /**
     * Downloaded tarballs are checked by length and sha256 and deleted on a mismatch (file: URLs,
     * no network).
     */
    @Test
    fun downloadsAreVerified() {
        val src = File(context.cacheDir, "download-src.tgz")
        context.assets.open("rootfs.tgz").use { input -> src.outputStream().use { input.copyTo(it) } }
        val sha = MessageDigest.getInstance("SHA-256").digest(src.readBytes()).joinToString("") { "%02x".format(it) }
        val dest = File(context.cacheDir, "download-dest.tgz")
        var last = 0L to 0L
        Rootfs.fetch(Rootfs.Download(listOf(src.toURI().toString()), sha, src.length()), dest) { done, total -> last = done to total }
        assertEquals(src.length() to src.length(), last)
        assertTrue(dest.readBytes().contentEquals(src.readBytes()))
        dest.delete()
        // A wrong digest, fewer bytes than expected, more bytes than expected (stopped as soon as it
        // goes past the length)
        for (wrong in listOf(Rootfs.Download(listOf(src.toURI().toString()), "0".repeat(64), src.length()),
            Rootfs.Download(listOf(src.toURI().toString()), sha, src.length() + 1),
            Rootfs.Download(listOf(src.toURI().toString()), sha, src.length() - 1))) {
            assertThrows(IOException::class.java) { Rootfs.fetch(wrong, dest) { _, _ -> } }
            assertFalse(dest.exists() || File(dest.path + ".part").exists())
        }
        src.delete()
    }

    /**
     * A download that broke off midway continues with Range; when the server ignores Range (replies
     * 200) it starts over. The local HTTP server sends only the first third the first time and then
     * drops the connection.
     */
    @Test
    fun downloadsResumeWhereTheyBrokeOff() {
        val data = ByteArray(300_000) { (it * 7 + it / 251).toByte() }
        val sha = MessageDigest.getInstance("SHA-256").digest(data).joinToString("") { "%02x".format(it) }
        val dest = File(context.cacheDir, "download-resumed.tgz")
        for (ranges in listOf(true, false)) {
            val server = ServerSocket(0, 4, InetAddress.getByName("127.0.0.1"))
            val asked = mutableListOf<String>()
            val serving = thread {
                repeat(2) { n ->
                    server.accept().use { c ->
                        val reader = c.getInputStream().bufferedReader()
                        val headers = generateSequence { reader.readLine() }.takeWhile { it.isNotEmpty() }.toList()
                        val range = headers.firstOrNull { it.startsWith("Range:", ignoreCase = true) }
                        asked.add(range ?: "-")
                        val from = range?.substringAfter("bytes=")?.substringBefore('-')?.toInt()?.takeIf { ranges } ?: 0
                        val head = if (from > 0) {
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes $from-${data.size - 1}/${data.size}\r\n"
                        } else {
                            "HTTP/1.1 200 OK\r\n"
                        }
                        val out = c.getOutputStream()
                        out.write((head + "Content-Length: ${data.size - from}\r\nConnection: close\r\n\r\n").toByteArray())
                        out.write(data, from, if (n == 0) data.size / 3 else data.size - from)
                        out.flush()
                    }
                }
            }
            Rootfs.fetch(Rootfs.Download(listOf("http://127.0.0.1:${server.localPort}/rootfs.tgz"), sha, data.size.toLong()), dest) { _, _ -> }
            serving.join(5000)
            server.close()
            assertEquals("ranges=$ranges", listOf("-", "Range: bytes=100000-"), asked)
            assertTrue(dest.readBytes().contentEquals(data))
            dest.delete()
        }
    }

    /**
     * An error status on a resume ends the source at once: the download neither starts over nor
     * asks again. And a download that broke off with every byte received (the server promised more
     * than it sent) is verified without another request, whose Range would start at the end.
     */
    @Test
    fun downloadsHandleResumesWithNothingToGet() {
        val data = ByteArray(300_000) { (it * 7 + it / 251).toByte() }
        val sha = MessageDigest.getInstance("SHA-256").digest(data).joinToString("") { "%02x".format(it) }
        val dest = File(context.cacheDir, "download-resumed.tgz")
        for (complete in listOf(false, true)) {
            val server = ServerSocket(0, 4, InetAddress.getByName("127.0.0.1"))
            val asked = mutableListOf<String>()
            val serving = thread {
                // Closed after the requests expected, so one too many is refused instead of waiting
                server.use {
                    repeat(if (complete) 1 else 2) { n ->
                        server.accept().use { c ->
                            val reader = c.getInputStream().bufferedReader()
                            val headers = generateSequence { reader.readLine() }.takeWhile { it.isNotEmpty() }.toList()
                            asked.add(headers.firstOrNull { it.startsWith("Range:", ignoreCase = true) } ?: "-")
                            val out = c.getOutputStream()
                            if (n == 0) {
                                out.write("HTTP/1.1 200 OK\r\nContent-Length: ${data.size + 10}\r\nConnection: close\r\n\r\n".toByteArray())
                                out.write(data, 0, if (complete) data.size else data.size / 3)
                            } else {
                                out.write("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".toByteArray())
                            }
                            out.flush()
                        }
                    }
                }
            }
            val download = Rootfs.Download(listOf("http://127.0.0.1:${server.localPort}/rootfs.tgz"), sha, data.size.toLong())
            if (complete) {
                Rootfs.fetch(download, dest) { _, _ -> }
                assertTrue(dest.readBytes().contentEquals(data))
                dest.delete()
            } else {
                assertThrows(IOException::class.java) { Rootfs.fetch(download, dest) { _, _ -> } }
                assertFalse(dest.exists())
            }
            serving.join(5000)
            assertEquals("complete=$complete", if (complete) listOf("-") else listOf("-", "Range: bytes=100000-"), asked)
            assertFalse(File(dest.path + ".part").exists())
        }
    }

    /**
     * An aborted download (cancelling a distro switch in Settings) throws [Rootfs.Cancelled] and
     * keeps no partial file. The local HTTP server stalls after a third and waits for the client to
     * cancel and disconnect.
     */
    @Test
    fun downloadsCanBeCancelled() {
        val data = ByteArray(300_000) { (it * 7).toByte() }
        val sha = MessageDigest.getInstance("SHA-256").digest(data).joinToString("") { "%02x".format(it) }
        val dest = File(context.cacheDir, "download-cancelled.tgz")
        val server = ServerSocket(0, 4, InetAddress.getByName("127.0.0.1"))
        val sent = java.util.concurrent.CountDownLatch(1)
        val serving = thread {
            server.accept().use { c ->
                val reader = c.getInputStream().bufferedReader()
                generateSequence { reader.readLine() }.takeWhile { it.isNotEmpty() }.count()
                val out = c.getOutputStream()
                out.write("HTTP/1.1 200 OK\r\nContent-Length: ${data.size}\r\nConnection: close\r\n\r\n".toByteArray())
                out.write(data, 0, data.size / 3)
                out.flush()
                sent.countDown()
                // Stall until the client gives up (the read sees EOF)
                runCatching { c.getInputStream().read() }
            }
        }
        var cancelled = false
        val url = "http://127.0.0.1:${server.localPort}/rootfs.tgz"
        assertThrows(Rootfs.Cancelled::class.java) {
            Rootfs.fetch(Rootfs.Download(listOf(url), sha, data.size.toLong()), dest, cancelled = { cancelled }) { done, _ ->
                if (done >= data.size / 3) {
                    sent.await()
                    cancelled = true
                }
            }
        }
        server.close()
        serving.join(5000)
        assertFalse(dest.exists() || File(dest.path + ".part").exists())
    }

    /**
     * Downloads the Debian rootfs into another store and, in the login shell (bash), looks at the
     * system information and DNS and runs apt-get update (when it cannot fetch the indexes it only
     * warns and still exits 0, so the output is checked); then creates a regular user that logs in
     * automatically: Ctrl-C while sudo installs skips it, Ctrl-C in the shell does not end the
     * session, and the next login installs sudo. It downloads about 50 MB and is skipped by
     * default:
     * `./gradlew connectedDebugAndroidTest -Pandroid.testInstrumentationRunnerArguments.network=true`.
     */
    @Test
    fun downloadedDistributionsRun() {
        assumeTrue(InstrumentationRegistry.getArguments().getString("network") == "true")
        for (distro in listOf(Rootfs.Distro.DEBIAN)) {
            val store = File(context.cacheDir, "store-${distro.name.lowercase()}")
            store.deleteRecursively()
            Rootfs.prepare(context, distro, store = store)
            val machine = Native.start(store.path, 1)
            try {
                val out = login(
                    machine,
                    "grep PRETTY_NAME /etc/os-release; echo shell=\$SHELL; getent hosts localhost | head -n 1; " +
                        "apt-get update 2>&1 | grep -q 'Failed to fetch' || echo apt-ok\nexit\n",
                )
                assertTrue(out, out.contains(distro.label.substringBefore(' ')))
                assertTrue(out, out.contains("shell=/bin/bash\n"))
                assertTrue(out, out.contains("apt-ok\n"))
                // A regular user becomes the login account for the first time: added to the sudo
                // group, sudo installed from the network. Ctrl-C while apt-get update downloads:
                // skipped this time, the login goes on as usual. util-linux's su starts no new
                // session, so the shell has a controlling terminal and job control: Ctrl-C
                // interrupts only the foreground sleep and does not end the whole session (it used
                // to close the tab)
                assertEquals(0, sh(machine, "useradd -m -s /bin/bash alice").second)
                val skipped = loginSteps(
                    machine,
                    "installing sudo" to "",
                    "% [" to "\u0003",
                    "$ " to "tty; echo sl''eeping; sleep 30\n",
                    "sleeping" to "\u0003",
                    "$ " to "echo still-here; exit\n",
                )
                assertTrue(skipped, skipped.contains("tuxport: skipped installing sudo"))
                // Neither appears in the echoed input (bash writes \e[?2004l\r before a command's
                // output, not a newline)
                assertTrue(skipped, skipped.contains("/dev/pts/"))
                assertTrue(skipped, skipped.contains("still-here\n"))
                assertFalse(skipped, skipped.contains("Session terminated"))
                // The next login installs sudo (showing apt's progress this time); no password
                // asked while none is set
                val admin = login(machine, "id -un; id -nG; sudo -n id -un; exit\n")
                assertTrue(admin, admin.contains("Setting up sudo"))
                assertTrue(admin, admin.contains("alice is in the sudo group and can become root with sudo"))
                assertTrue(admin, becameAdmin(admin, "sudo"))
            } finally {
                Native.stop(machine)
                store.deleteRecursively()
            }
        }
    }

    /**
     * The fastest mirror goes into the sources only while they are still exactly what the import
     * wrote: also while the kernel runs, and the guest sees it at once. Sources already switched,
     * edited by the user or deleted are left alone.
     */
    @Test
    fun mirrorIsWrittenOnlyIntoUntouchedSources() {
        val machine = start(1)
        val distro = Rootfs.Distro.ALPINE
        val path = "/etc/apk/repositories"
        val before = sh(machine, "cat $path").first
        val (official, tuna, ustc) = distro.mirrors
        val branch = BuildConfig.ALPINE_BRANCH
        val sources = { mirror: String -> "$mirror/$branch/main\n$mirror/$branch/community\n" }
        val write = { text: String -> Native.writeFile(Rootfs.store(context).path, path, text.toByteArray(), 420, true) }
        try {
            // The app's speed probe may already have switched the mirror: start as at import
            write(sources(official))
            // The official mirror being the fastest writes nothing
            assertTrue(Rootfs.useMirror(context, distro, official))
            assertEquals(sources(official), sh(machine, "cat $path").first)
            assertTrue(Rootfs.useMirror(context, distro, ustc))
            assertEquals(sources(ustc), sh(machine, "cat $path").first)
            // Once switched, never again
            assertFalse(Rootfs.useMirror(context, distro, tuna))
            assertEquals(sources(ustc), sh(machine, "cat $path").first)
            // A line the user added, even with the official mirror still there
            val edited = sources(official) + "@testing $official/edge/testing\n"
            write(edited)
            assertFalse(Rootfs.useMirror(context, distro, ustc))
            assertEquals(edited, sh(machine, "cat $path").first)
            // The sources file is gone: not written again
            sh(machine, "rm $path")
            assertFalse(Rootfs.useMirror(context, distro, ustc))
            assertEquals(null, Native.readFile(Rootfs.store(context).path, path))
        } finally {
            // Restore the original sources: other test cases and the app itself use this store
            write(before)
        }
    }

    /**
     * The guest's routing table (the kernel/net map): the app cannot read the host's
     * `/proc/net/route`; until the default network's `LinkProperties` are handed in, the table is
     * synthesised from the interfaces' addresses (the default route has no gateway), and afterwards
     * it has a gateway; the host chooses the source address of `ip route get`.
     */
    @Test
    fun routesComeFromLinkProperties() {
        val machine = start(1)
        val cm = context.getSystemService(ConnectivityManager::class.java)
        val lp = cm.getLinkProperties(cm.activeNetwork)
        assertNotNull("no default network", lp)
        val default = lp!!.routes.first { it.isDefaultRoute && it.hasGateway() && it.gateway!!.address.size == 4 }
        val (before, _) = sh(machine, "ip route; ip route get 1.1.1.1")
        assertTrue(before, before.contains("default dev ${default.`interface`} scope link"))
        assertTrue(before, before.contains("1.1.1.1 dev ${default.`interface`}  src "))
        Native.setRoutes(machine, TerminalService.routes(lp))
        val (after, code) = sh(machine, "ip route; ip route get 1.1.1.1; route -n")
        assertEquals(after, 0, code)
        val gateway = default.gateway!!.hostAddress
        assertTrue(after, after.contains("default via $gateway dev ${default.`interface`}"))
        assertTrue(after, after.contains("1.1.1.1 via $gateway dev ${default.`interface`}  src "))
        assertTrue(after, after.lines().any { it.startsWith("0.0.0.0 ") && it.contains(gateway!!) })
        Native.setRoutes(machine, null)
        assertTrue(sh(machine, "ip route").first.contains("default dev "))
    }

    /**
     * Ports below 1024 that the host refuses (the kernel/net map): the app has no
     * CAP_NET_BIND_SERVICE, and 79 is not among the few Android opened up; when the guest's nc
     * binds 79 the host uses 10079 instead, and the guest still sees 79. The app learns the
     * external port through [Native.takeListens] (the notification of §12 item 7 uses it), a
     * connection from the app process to 10079 receives the data the guest sends, and the kernel
     * log records where the port moved.
     */
    @Test
    fun lowPortsTheHostRefusesMoveToHighPorts() {
        assertTrue("the app itself could bind port 79", runCatching { ServerSocket(79).close() }.isFailure)
        val out = ncListen(79, 10079)
        assertTrue(out, out.contains("tuxport: port 79/tcp is 10079 on the host"))
    }

    /**
     * Low ports the host allows: Google Play system updates since May 2026 let apps bind 22, 80 and
     * 443 (Android 13 and later, kernel 5.15 and later; present on the API 37 emulator), so when
     * the guest's nc binds 22 it is the host's 22, not remapped and not in the kernel log. Skipped
     * on systems without that update.
     */
    @Test
    fun lowPortsTheHostAllowsAreBoundAsTheyAre() {
        assumeTrue("apps cannot bind port 22 here", runCatching { ServerSocket(22).close() }.isSuccess)
        val out = ncListen(22, 22)
        assertTrue(out, out.contains("no log"))
    }

    /**
     * The guest's nc listens on [port]: the reported listen event is [hostPort] on the host, and a
     * connection to it from the app process receives the data the guest sends. Returns the console
     * output (followed by the `dmesg` lines about this port, or "no log" when there are none).
     */
    private fun ncListen(port: Int, hostPort: Int): String {
        val machine = start(1)
        val console = Native.openConsole(machine, 24, 80)
        val script = "echo hello | nc -l -p $port; dmesg | grep 'port $port/' || echo no log"
        val pid = Native.spawn(machine, console, "/bin/sh", arrayOf("sh", "-c", script), ConsolePty.ENVIRONMENT)
        var out = ""
        val reader = thread { out = readAll(machine, console) }
        var listens: Array<String>? = null
        val deadline = System.currentTimeMillis() + 30_000
        while (System.currentTimeMillis() < deadline) {
            listens = Native.takeListens(machine)
            if (!listens.isNullOrEmpty()) break
            Thread.sleep(50)
        }
        assertEquals("nc\t$port\t$hostPort", listens?.singleOrNull())
        // nc does not close the connection after reading all of stdin: close it once read, so nc
        // exits
        Socket("127.0.0.1", hostPort).use { s ->
            val got = ByteArray(6)
            DataInputStream(s.getInputStream()).readFully(got)
            assertEquals("hello\n", String(got))
        }
        reader.join(10_000)
        assertEquals(out, 0, Native.waitFor(machine, pid))
        assertEquals(0, Native.takeListens(machine)?.size)
        return out
    }

    /**
     * File access is opt-in for both the login account's home and `/`. Disabling refuses old URIs
     * for queries, reads and changes. Listings follow symbolic links and leave out broken links
     * and FIFOs; files outside the home cannot be written; a tree grant cannot walk out with `..`;
     * search by name. The files were just written by the running kernel: when the store is opened
     * separately, the kernel commits first, so they are visible.
     */
    @Test
    fun documentsShowTheHomeDirectory() {
        val prefs = context.getSharedPreferences("settings", android.content.Context.MODE_PRIVATE)
        prefs.edit().putString("login", ConsolePty.LOGIN_ROOT).remove("file_access").commit()
        val machine = start(1)
        val setup = "rm -rf /root/dt && mkdir -p /root/dt/sub && printf hi > /root/dt/a.txt && " +
            "ln -s /etc/alpine-release /root/dt/release && ln -s /nowhere /root/dt/broken && " +
            "mkfifo /root/dt/fifo && printf x > '/root/dt/100% 中文.txt' && " +
            "printf 'export A=1\\n' > /root/dt/.bashrc && printf 'a\\0b' > /root/dt/blob"
        assertEquals(0, sh(machine, setup).second)
        val resolver = context.contentResolver
        val authority = Documents.authority(context)
        fun rows(uri: android.net.Uri, vararg columns: String, args: android.os.Bundle? = null): List<List<String?>>? =
            resolver.query(uri, null, args, null)?.use { c ->
                val out = mutableListOf<List<String?>>()
                while (c.moveToNext()) out.add(columns.map { c.getString(c.getColumnIndexOrThrow(it)) })
                out
            }
        val id = DocumentsContract.Document.COLUMN_DOCUMENT_ID
        val name = DocumentsContract.Document.COLUMN_DISPLAY_NAME
        val mime = DocumentsContract.Document.COLUMN_MIME_TYPE
        val size = DocumentsContract.Document.COLUMN_SIZE
        fun roots() = rows(DocumentsContract.buildRootsUri(authority), DocumentsContract.Root.COLUMN_ROOT_ID, id)
        fun document(doc: String) = DocumentsContract.buildDocumentUri(authority, doc)
        fun read(doc: String) = resolver.openInputStream(document(doc))!!.use { String(it.readBytes()) }
        fun assertDisabled() {
            assertEquals(emptyList<List<String?>>(), roots())
            assertEquals(null, rows(document("/root/dt/a.txt"), id))
            assertEquals(null, rows(document("/etc/motd"), id))
            assertThrows(java.io.FileNotFoundException::class.java) { read("/root/dt/a.txt") }
            assertThrows(java.io.FileNotFoundException::class.java) { read("/etc/motd") }
            assertEquals(Documents.FileLink.Hidden, Documents.fileLink(context, android.net.Uri.parse("file:///root/dt/a.txt")))
            resolver.acquireContentProviderClient(authority)!!.use { client ->
                val provider = client.localContentProvider as Documents
                assertFalse(provider.isChildDocument("/root/dt", "/root/dt/a.txt"))
                val refused: List<() -> Unit> = listOf(
                    { provider.queryChildDocuments("/root/dt", null, sortOrder = null).close() },
                    { provider.querySearchDocuments(Documents.ROOT_HOME, "a.txt", null).close() },
                    { provider.querySearchDocuments(Documents.ROOT_SYSTEM, "motd", null).close() },
                    { provider.findDocumentPath(null, "/root/dt/a.txt") },
                    { provider.openDocument("/root/dt/a.txt", "wt", null).close() },
                    { provider.createDocument("/root/dt", "text/plain", "denied.txt") },
                    { provider.renameDocument("/root/dt/a.txt", "renamed.txt") },
                    { provider.moveDocument("/root/dt/a.txt", "/root/dt", "/root/dt/sub") },
                    { provider.deleteDocument("/root/dt/a.txt") },
                )
                for (call in refused) assertThrows(java.io.FileNotFoundException::class.java) { call() }
            }
        }
        try {
            assertFalse(Settings.fileAccess(context))
            assertDisabled()
            // The old visibility setting does not bypass the new explicit consent.
            val old = prefs.all["system_documents"] as? Boolean
            try {
                prefs.edit().putBoolean("system_documents", true).commit()
                assertDisabled()
            } finally {
                prefs.edit().apply {
                    if (old == null) remove("system_documents") else putBoolean("system_documents", old)
                }.commit()
            }
            Settings.setFileAccess(context, true)
            assertEquals(listOf(listOf("home", "/root"), listOf("system", "/")), roots())
            assertEquals(
                // What the extension cannot tell goes by content: text is text/plain (dot files, a
                // linked-to version number); anything with a NUL is not
                listOf(
                    listOf("/root/dt/.bashrc", ".bashrc", "text/plain", "11"),
                    listOf("/root/dt/100%25 中文.txt", "100% 中文.txt", "text/plain", "1"),
                    listOf("/root/dt/a.txt", "a.txt", "text/plain", "2"),
                    listOf("/root/dt/blob", "blob", "application/octet-stream", "3"),
                    listOf("/root/dt/release", "release", "text/plain", null),
                    listOf("/root/dt/sub", "sub", DocumentsContract.Document.MIME_TYPE_DIR, null),
                ),
                rows(DocumentsContract.buildChildDocumentsUri(authority, "/root/dt"), id, name, mime, size)
                    ?.map { if (it[0] == "/root/dt/release") it.take(3) + listOf(null) else it },
            )
            assertEquals("hi", read("/root/dt/a.txt"))
            assertEquals("x", read("/root/dt/100%25 中文.txt"))
            assertTrue(read("/root/dt/release").startsWith("3."))
            // Non-canonical IDs count as nonexistent (the query returns null).
            assertEquals(listOf(listOf("text/plain")), rows(document("/root/dt/release"), mime))
            assertEquals(null, rows(document("/root/dt/../../etc/motd"), id))
            assertEquals(null, rows(document("/root//dt"), id))
            // Tree grants: what is inside works, walking out with `..` does not
            val tree = DocumentsContract.buildTreeDocumentUri(authority, "/root/dt")
            assertEquals(
                listOf(listOf("/root/dt/sub")),
                rows(DocumentsContract.buildDocumentUriUsingTree(tree, "/root/dt/sub"), id),
            )
            assertThrows(SecurityException::class.java) {
                rows(DocumentsContract.buildDocumentUriUsingTree(tree, "/root/dt/../../etc/motd"), id)
            }
            // The Files app passes the name in the arguments when it searches
            val search = DocumentsContract.buildSearchDocumentsUri(authority, Documents.ROOT_HOME, "A.TXT")
            val args = android.os.Bundle().apply { putString(DocumentsContract.QUERY_ARG_DISPLAY_NAME, "A.TXT") }
            val found = rows(search, id, args = args)
            assertEquals(listOf(listOf("/root/dt/a.txt")), found)
            assertEquals(listOf(listOf("motd")), rows(document("/etc/motd"), name))
            // Outside the home directory is read-only
            assertThrows(Exception::class.java) { resolver.openOutputStream(document("/etc/motd"))!!.close() }
            assertTrue(rows(DocumentsContract.buildChildDocumentsUri(authority, "/"), name)!!.contains(listOf("etc")))
            Settings.setFileAccess(context, false)
            assertDisabled()
            Settings.setFileAccess(context, true)
            assertEquals("hi", read("/root/dt/a.txt"))
            val privacy = Settings.privacyMode(context)
            try {
                Settings.setPrivacyMode(context, true)
                // Even a stale true preference and previously issued URIs cannot bypass privacy.
                prefs.edit().putBoolean("file_access", true).commit()
                assertDisabled()
            } finally {
                Settings.setPrivacyMode(context, privacy)
            }
        } finally {
            prefs.edit().remove("login").remove("file_access").commit()
            sh(machine, "rm -rf /root/dt")
        }
    }

    /**
     * Without a usable home directory (the login account has none, or its home is `/`) there is no
     * home root, while the system root still appears with file access on, and stays read-only: a home
     * of `/` must not make every file writable.
     */
    @Test
    fun documentsWithoutAHomeDirectory() {
        val prefs = context.getSharedPreferences("settings", android.content.Context.MODE_PRIVATE)
        val machine = start(1)
        val setup = "deluser carol 2>/dev/null; deluser dave 2>/dev/null; " +
            "adduser -D -H -h / -u 1601 carol && adduser -D -H -h /home/dave -u 1602 dave"
        assertEquals(0, sh(machine, setup).second)
        val resolver = context.contentResolver
        val authority = Documents.authority(context)
        fun roots() = resolver.query(DocumentsContract.buildRootsUri(authority), null, null, null)?.use { c ->
            val out = mutableListOf<String>()
            while (c.moveToNext()) {
                out.add(c.getString(c.getColumnIndexOrThrow(DocumentsContract.Root.COLUMN_ROOT_ID)))
            }
            out
        }
        fun document(doc: String) = DocumentsContract.buildDocumentUri(authority, doc)
        try {
            // Each account is its own login setting, so the provider's cached home is not reused
            for (account in listOf("carol", "dave")) {
                prefs.edit().putString("login", account).putBoolean("file_access", false).commit()
                assertEquals(account, emptyList<String>(), roots())
                prefs.edit().putBoolean("file_access", true).commit()
                assertEquals(account, listOf("system"), roots())
                assertThrows(account, Exception::class.java) { resolver.openOutputStream(document("/etc/motd"))!!.close() }
                val created = runCatching { DocumentsContract.createDocument(resolver, document("/"), "text/plain", "x") }
                assertEquals(account, null, created.getOrNull())
            }
            assertEquals(1, sh(machine, "test -e /x").second)
        } finally {
            prefs.edit().remove("login").remove("file_access").commit()
            sh(machine, "deluser carol; deluser dave")
        }
    }

    /**
     * Changing the home directory from the Files app ([Documents]): creating (another name when the
     * name is taken, an extension added from the type), writing, appending, truncating, renaming
     * (without replacing), moving, deleting a whole directory; new files belong to the login
     * account. The running kernel sees the changes at once. Nothing outside the home directory, or
     * leading out through a symbolic link, is allowed, and the home directory itself cannot be
     * deleted.
     */
    @Test
    fun documentsWriteInTheHomeDirectory() {
        val prefs = context.getSharedPreferences("settings", android.content.Context.MODE_PRIVATE)
        val machine = start(1)
        val setup = "deluser --remove-home bob 2>/dev/null; adduser -D -u 1500 bob && mkdir /home/bob/dw && " +
            "ln -s /etc /home/bob/dw/etc-link && chown -h bob:bob /home/bob/dw /home/bob/dw/etc-link"
        assertEquals(0, sh(machine, setup).second)
        prefs.edit().putString("login", "bob").putBoolean("file_access", true).commit()
        val resolver = context.contentResolver
        val authority = Documents.authority(context)
        fun uri(doc: String) = DocumentsContract.buildDocumentUri(authority, doc)
        fun id(uri: android.net.Uri?) = uri?.let { DocumentsContract.getDocumentId(it) }
        fun create(dir: String, mime: String, name: String) =
            id(DocumentsContract.createDocument(resolver, uri(dir), mime, name))
        fun write(doc: String, mode: String, text: String) =
            resolver.openOutputStream(uri(doc), mode)!!.use { it.write(text.toByteArray()) }
        fun flags(doc: String) = resolver.query(uri(doc), null, null, null, null)!!.use {
            it.moveToFirst()
            it.getInt(it.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_FLAGS))
        }
        val dw = "/home/bob/dw"
        try {
            assertEquals("$dw/notes.txt", create(dw, "text/plain", "notes"))
            assertEquals("$dw/notes (1).txt", create(dw, "text/plain", "notes.txt"))
            assertEquals("$dw/sub", create(dw, DocumentsContract.Document.MIME_TYPE_DIR, "sub"))
            assertEquals("$dw/a_b", create(dw, "application/octet-stream", "a/b"))
            write("$dw/notes.txt", "wt", "hello from Files")
            write("$dw/notes.txt", "wa", " again")
            assertEquals("hello from Files again" to 0, sh(machine, "cat $dw/notes.txt"))
            write("$dw/notes (1).txt", "w", "x")
            write("$dw/notes (1).txt", "wt", "y")
            assertEquals("y" to 0, sh(machine, "cat '$dw/notes (1).txt'"))
            val owners = "stat -c '%U %G %a %n' $dw/notes.txt $dw/sub"
            assertEquals("bob bob 644 $dw/notes.txt\nbob bob 755 $dw/sub\n" to 0, sh(machine, owners))
            // Document flags: inside the home directory, write, delete, rename and move; the home
            // directory itself only allows creating inside it
            val home = DocumentsContract.Document.FLAG_DIR_SUPPORTS_CREATE
            assertEquals(home, flags("/home/bob"))
            assertTrue(flags("$dw/notes.txt") and DocumentsContract.Document.FLAG_SUPPORTS_WRITE != 0)
            assertTrue(flags("$dw/sub") and DocumentsContract.Document.FLAG_SUPPORTS_DELETE != 0)
            // Renaming does not replace; moving; deleting a whole directory
            assertThrows(Exception::class.java) {
                DocumentsContract.renameDocument(resolver, uri("$dw/notes (1).txt"), "notes.txt")
            }
            val renamed = DocumentsContract.renameDocument(resolver, uri("$dw/notes (1).txt"), "renamed.txt")
            assertEquals("$dw/renamed.txt", id(renamed))
            val moved = DocumentsContract.moveDocument(resolver, renamed!!, uri(dw), uri("$dw/sub"))
            assertEquals("$dw/sub/renamed.txt", id(moved))
            assertEquals("y" to 0, sh(machine, "cat $dw/sub/renamed.txt"))
            assertTrue(DocumentsContract.deleteDocument(resolver, uri("$dw/sub")))
            assertEquals(1, sh(machine, "test -e $dw/sub").second)
            // Outside the home directory, leading out through a symbolic link, the home directory
            // itself
            val motd = sh(machine, "cat /etc/motd").first
            assertThrows(Exception::class.java) { create("/etc", "text/plain", "x") }
            assertThrows(Exception::class.java) { create("$dw/etc-link", "text/plain", "x") }
            assertThrows(Exception::class.java) { write("$dw/etc-link/motd", "wt", "gone") }
            assertThrows(Exception::class.java) { DocumentsContract.deleteDocument(resolver, uri("/home/bob")) }
            assertEquals(motd to 0, sh(machine, "cat /etc/motd"))
            assertEquals(1, sh(machine, "test -e /etc/x").second)
            // Deleting a symbolic link deletes only the link
            assertTrue(DocumentsContract.deleteDocument(resolver, uri("$dw/etc-link")))
            assertEquals(0, sh(machine, "test ! -e $dw/etc-link && test -d /etc").second)
        } finally {
            prefs.edit().remove("login").remove("file_access").commit()
            sh(machine, "deluser --remove-home bob")
        }
    }

    /** Disabling file access revokes grants across document and tree URI paths, also after re-enabling. */
    @Test
    fun documentsRevokeAccess() {
        val recipient = InstrumentationRegistry.getInstrumentation().context
        val uid = recipient.applicationInfo.uid
        assertTrue(uid != android.os.Process.myUid())
        val mode = android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION or
            android.content.Intent.FLAG_GRANT_WRITE_URI_PERMISSION
        val authority = Documents.authority(context)
        val uris = listOf(
            DocumentsContract.buildDocumentUri(authority, "/root/a.txt"),
            DocumentsContract.buildDocumentUri(authority, "/etc/motd"),
            DocumentsContract.buildTreeDocumentUri(authority, "/root"),
            DocumentsContract.buildTreeDocumentUri(authority, "/"),
        )
        val prefs = context.getSharedPreferences("settings", android.content.Context.MODE_PRIVATE)
        try {
            Settings.setFileAccess(context, true)
            for (uri in uris) {
                context.grantUriPermission(recipient.packageName, uri, mode)
                assertEquals(android.content.pm.PackageManager.PERMISSION_GRANTED, context.checkUriPermission(uri, -1, uid, mode))
            }
            Settings.setFileAccess(context, false)
            Documents.revokeAccess(context)
            Settings.setFileAccess(context, true)
            for (uri in uris) {
                assertEquals(android.content.pm.PackageManager.PERMISSION_DENIED, context.checkUriPermission(uri, -1, uid, mode))
            }
        } finally {
            Documents.revokeAccess(context)
            prefs.edit().remove("file_access").commit()
        }
    }

    /**
     * The persisted grants a deleted, renamed or moved directory takes along (the android/documents
     * map): those on the directory and below it, as document or tree URIs (a document reached
     * through a tree counts as its tree), once each; not a sibling sharing the name's prefix,
     * another provider's documents or IDs that are not canonical.
     */
    @Test
    fun documentsGrantsBelowADirectory() {
        val authority = Documents.authority(context)
        fun doc(id: String) = DocumentsContract.buildDocumentUri(authority, id)
        fun tree(id: String) = DocumentsContract.buildTreeDocumentUri(authority, id)
        val uris = listOf(
            doc("/home/a/dir"),
            tree("/home/a/dir/sub"),
            doc("/home/a/dir/sub/f.txt"),
            DocumentsContract.buildDocumentUriUsingTree(tree("/home/a/dir"), "/home/a/dir/g"),
            doc("/home/a/dirx"),
            tree("/home/a"),
            doc("/home/a/dir/../x"),
            DocumentsContract.buildDocumentUri("other.documents", "/home/a/dir/f"),
        )
        assertEquals(
            listOf("/home/a/dir", "/home/a/dir/sub", "/home/a/dir/sub/f.txt"),
            Documents.grantedUnder(uris, authority, "/home/a/dir"),
        )
        assertEquals(
            listOf("/home/a/dir", "/home/a/dir/sub", "/home/a/dir/sub/f.txt", "/home/a/dirx", "/home/a"),
            Documents.grantedUnder(uris, authority, "/"),
        )
        assertEquals(emptyList<String>(), Documents.grantedUnder(uris, authority, "/home/b"))
    }

    /**
     * Changes made in the Files app go to the running kernel, and a program watching in the guest
     * receives inotify events: a written file is reported only when the Files app side closes it
     * ([Documents.openDocument] hands the fd out with a close callback). The other way round,
     * directories whose entries the guest changed are taken by [Native.takeChangedDirs], and
     * [Documents.dirsChanged] tells the Files app to refresh.
     */
    @Test
    fun documentsChangesReachTheKernelAndBack() {
        val prefs = context.getSharedPreferences("settings", android.content.Context.MODE_PRIVATE)
        val machine = start(1)
        val dw = "/home/bob/dw"
        val setup = "deluser --remove-home bob 2>/dev/null; adduser -D -u 1500 bob && mkdir $dw && chown bob:bob $dw"
        assertEquals(0, sh(machine, setup).second)
        prefs.edit().putString("login", "bob").putBoolean("file_access", true).commit()
        val resolver = context.contentResolver
        val authority = Documents.authority(context)
        fun uri(doc: String) = DocumentsContract.buildDocumentUri(authority, doc)
        try {
            // busybox's inotifyd prints "letter\tdirectory\tname": n created, c modified, r opened,
            // w closed after writing, m and y moved out and in, d deleted. It runs on its own
            // console, read until it exits (timeout)
            val console = Native.openConsole(machine, 24, 200)
            val argv = arrayOf("sh", "-c", "timeout 6 inotifyd - $dw")
            val pid = Native.spawn(machine, console, "/bin/sh", argv, ConsolePty.ENVIRONMENT)
            var out = ""
            val reader = thread { out = readAll(machine, console) }
            Thread.sleep(1500)
            val created = DocumentsContract.createDocument(resolver, uri(dw), "text/plain", "w.txt")!!
            resolver.openOutputStream(created, "wt")!!.use { it.write("from Files".toByteArray()) }
            val renamed = DocumentsContract.renameDocument(resolver, created, "v.txt")!!
            assertTrue(DocumentsContract.deleteDocument(resolver, renamed))
            Native.waitFor(machine, pid)
            reader.join(5000)
            Native.closeConsole(machine, console)
            val log = out.lines().joinToString("\n") { it.split('\t').let { f -> "${f[0]}\t${f.getOrElse(2) { "" }}" } }
            val want = listOf("n\tw.txt", "w\tw.txt", "m\tw.txt", "y\tv.txt", "d\tv.txt")
            assertTrue(out, want.all { log.lines().contains(it) })
            val lines = log.lines()
            assertTrue(log, lines.indexOf("w\tw.txt") > lines.indexOf("n\tw.txt"))
            // Directories whose entries the guest changed: recorded only from the first take on;
            // after the notification the Files app's observer gets it
            Native.takeChangedDirs(machine)
            assertEquals(0, sh(machine, "touch $dw/from-guest").second)
            val dirs = Native.takeChangedDirs(machine)!!
            assertTrue(dirs.joinToString(), dw in dirs)
            val latch = java.util.concurrent.CountDownLatch(1)
            val observer = object : android.database.ContentObserver(null) {
                override fun onChange(selfChange: Boolean) = latch.countDown()
            }
            resolver.registerContentObserver(DocumentsContract.buildChildDocumentsUri(authority, dw), false, observer)
            try {
                Documents.dirsChanged(context, dirs)
                assertTrue(latch.await(5, java.util.concurrent.TimeUnit.SECONDS))
            } finally {
                resolver.unregisterContentObserver(observer)
            }
        } finally {
            prefs.edit().remove("login").remove("file_access").commit()
            sh(machine, "deluser --remove-home bob")
        }
    }

    /**
     * A kernel-layer or engine bug ends only the guest process that hit it (the engine map, design
     * doc §12 item 5), simulated with the test hooks of the debug app. In the app process, when the
     * engine faults on the host the SIGSEGV first goes through ART's libsigchain: only when ART
     * does not claim it does it reach the engine's handler, which siglongjmps back. When the kernel
     * layer panics in a host call (JNI), that call fails and the console is hung up. Afterwards ART
     * and the Machine both keep working.
     */
    @Test
    fun bugsKillOnlyTheGuestProcess() {
        Os.setenv("TUXPORT_TEST_HOOKS", "1", true)
        val store = Rootfs.store(context).path
        for ((name, code) in GUESTS) Native.writeFile(store, "/tmp/$name", elf(code), 0x1ed, true)
        for (tier in 0..1) {
            val machine = start(tier)
            assertEquals("tier $tier", -SIGSEGV, run(machine, "/tmp/engine-fault", "engine-fault").second)
            assertEquals("tier $tier", -SIGSEGV, run(machine, "/tmp/kernel-panic", "kernel-panic").second)
            // The kernel layer panics while the console is read: the read gets -1, the session
            // leader gets SIGHUP
            assertEquals("tier $tier", "" to -SIGHUP, run(machine, "/tmp/host-call", "host-call"))
            // ART keeps working: GC, and a stack overflow becomes a StackOverflowError
            Runtime.getRuntime().gc()
            assertTrue(runCatching { depth(0) }.exceptionOrNull() is StackOverflowError)
            assertEquals("ok\n" to 0, sh(machine, "echo ok"))
        }
    }

    private fun depth(n: Int): Int = if (n < 0) 0 else depth(n + 1) + 1

    /**
     * A process the kernel layer ended leaves a report for the app ([Native.takeKills]): console,
     * pid, process name, reason (for a bug, also the last few kernel log entries); once taken, it
     * is gone.
     * Kernel bugs are simulated with the test hooks, OOM with cgroup's memory.max: awk builds a
     * 64 MB string and then keeps running for a while (the OOM killer measures usage periodically
     * and cannot catch a process that uses it all up and exits at once).
     */
    @Test
    fun killsAreReportedToTheApp() {
        Os.setenv("TUXPORT_TEST_HOOKS", "1", true)
        val store = Rootfs.store(context).path
        Native.writeFile(store, "/tmp/kernel-panic", elf(GUESTS.getValue("kernel-panic")), 0x1ed, true)
        val machine = start(1)
        assertEquals(0, Native.takeKills(machine)?.size)
        assertEquals("" to 0, sh(machine, "echo before the bug > /dev/kmsg"))
        assertEquals(-SIGSEGV, run(machine, "/tmp/kernel-panic", "kernel-panic").second)
        val bug = Native.takeKills(machine)!!.single().split('\t')
        assertEquals(listOf("kernel-panic", "bug"), bug.subList(2, 4))
        assertTrue(bug[0], bug[0].toInt() >= 0)
        assertTrue(bug[4], bug[4].contains("pid ${bug[1]} (kernel-panic)"))
        // The report has the panic message and location (what "Copy report" on the notification
        // copies)
        assertTrue(bug[4], bug[4].contains(": panic injected by the test hook at core/kernel/src/bug.rs:"))
        // And the kernel log entries before it (the one the guest wrote) and itself, with
        // timestamps
        val log = bug[5].split('\u001e')
        assertTrue(bug[5], log.all { it.startsWith("[") } && log.any { it.endsWith("] before the bug") })
        assertTrue(bug[5], log.last().endsWith(bug[4]))
        val script = "cd /sys/fs/cgroup && echo +memory > cgroup.subtree_control && mkdir -p oomt && " +
            "echo 16M > oomt/memory.max && " +
            "sh -c 'echo \$\$ > oomt/cgroup.procs; exec awk \"BEGIN { s = \\\"x\\\"; " +
            "for (i = 0; i < 26; i++) s = s s; for (j = 0; j < 50000000; j++) n++ }\"'; echo \$?"
        val (out, code) = sh(machine, script)
        assertTrue(out, out.endsWith("137\n") && code == 0)
        val oom = Native.takeKills(machine)!!.single().split('\t')
        assertEquals(listOf("awk", "oom", "/oomt", "16777216"), oom.subList(2, 5) + oom[6])
        assertTrue(oom[5], oom[5].toLong() > 16777216)
        assertEquals(0, Native.takeKills(machine)?.size)
    }

    /**
     * The app starts its kernel with debug.exception-trace at 1 (arm64 Linux starts at 0), so a
     * guest program that dies of an unhandled exception is reported in the kernel log, which bug
     * reports carry: here an undefined instruction, named after the program's file and mapping.
     */
    @Test
    fun guestCrashesReachTheKernelLog() {
        val store = Rootfs.store(context).path
        Native.writeFile(store, "/tmp/undefined", elf(intArrayOf(0 /* udf #0 */)), 0x1ed, true)
        val machine = start(1)
        assertEquals("1\n" to 0, sh(machine, "cat /proc/sys/debug/exception-trace"))
        assertEquals(-SIGILL, run(machine, "/tmp/undefined", "undefined").second)
        val (log, code) = sh(machine, "dmesg | grep 'unhandled exception'")
        val report = Regex("""\] undefined\[\d+]: unhandled exception: undefined instruction in undefined\[400000\+1000]""")
        assertTrue(log, code == 0 && report.containsMatchIn(log))
    }

    /** Releasing caches (onTrimMemory) leaves running processes alone; new ones start as usual. */
    @Test
    fun trimMemoryKeepsTheMachineWorking() {
        val machine = start(1)
        assertEquals("ok\n" to 0, sh(machine, "echo ok"))
        var result = "" to -1
        val running = thread {
            result = sh(machine, "for n in 1 2 3 4 5; do sha256sum /bin/busybox >/dev/null; sleep 0.05; done; echo done")
        }
        repeat(5) {
            Native.trimMemory(machine)
            Thread.sleep(50)
        }
        running.join(30_000)
        assertEquals("done\n" to 0, result)
        Native.trimMemory(machine)
        assertEquals("ok\n" to 0, sh(machine, "echo ok"))
    }

    /**
     * Using a kernel's handle after it stopped is safe: after a session ends, TerminalSession
     * cleans up the console only later on the main thread, and by then the service may have stopped
     * the kernel (rootfs reset, service destroyed).
     */
    @Test
    fun callsAfterStopAreHarmless() {
        val machine = start(1)
        val console = Native.openConsole(machine, 24, 80)
        val pid = Native.spawn(machine, console, "/bin/sh", arrayOf("sh", "-c", "sleep 30"), ConsolePty.ENVIRONMENT)
        Native.stop(machine)
        Native.closeConsole(machine, console)
        Native.resize(machine, console, 30, 100)
        Native.trimMemory(machine)
        Native.setRoutes(machine, null)
        assertEquals(-1, Native.read(machine, console, ByteArray(16)))
        assertEquals(-1, Native.write(machine, console, ByteArray(1), 0, 1))
        assertEquals(-9, Native.waitFor(machine, pid))
        assertThrows(IOException::class.java) { Native.openConsole(machine, 24, 80) }
        Native.stop(machine)
    }

    private companion object {
        const val SIGHUP = 1
        const val SIGILL = 4
        const val SIGSEGV = 11

        /** Syscall number of the test hook (core/kernel/src/bug.rs); x0 is the bug to simulate. */
        const val HOOK = 0xa991
        val SVC = 0xd4000001L.toInt()

        /** `movz x<rd>, #imm`. */
        fun movz(rd: Int, imm: Int) = (0xd2800000L or (imm.toLong() shl 5) or rd.toLong()).toInt()

        /** `exit(x0)`. */
        val EXIT = intArrayOf(movz(8, 93), SVC)

        val GUESTS = mapOf(
            // Point the stack's page in the page table at inaccessible host memory, then write it:
            // the engine faults on the host
            "engine-fault" to intArrayOf(
                movz(0, 2), 0x910003e1L.toInt() /* mov x1, sp */, movz(8, HOOK), SVC,
                0xf9000020L.toInt() /* str x0, [x1] */, movz(0, 4),
            ) + EXIT,
            "kernel-panic" to intArrayOf(movz(0, 1), movz(8, HOOK), SVC, movz(0, 7)) + EXIT,
            // Plant a bug for the host's next call to a Machine method, then read stdin
            "host-call" to intArrayOf(
                movz(0, 6), movz(8, HOOK), SVC,
                movz(0, 0), 0xd10043e1L.toInt() /* sub x1, sp, #16 */, movz(2, 1), movz(8, 63), SVC,
                movz(0, 0),
            ) + EXIT,
        )

        /**
         * A static aarch64 ELF with a single code segment: loaded at 0x400000, runs from the first
         * instruction.
         */
        fun elf(code: IntArray): ByteArray {
            val base = 0x400000L
            val headers = 64 + 56
            val size = (headers + 4 * code.size).toLong()
            val b = ByteBuffer.allocate(size.toInt()).order(ByteOrder.LITTLE_ENDIAN)
            // 64-bit, little-endian, version 1
            b.put(byteArrayOf(0x7f, 'E'.code.toByte(), 'L'.code.toByte(), 'F'.code.toByte(), 2, 1, 1, 0))
            b.put(ByteArray(8))
            // ET_EXEC, EM_AARCH64, version, entry, program and section header offsets, flags
            b.putShort(2).putShort(183).putInt(1).putLong(base + headers).putLong(64).putLong(0).putInt(0)
            // Sizes of the file header and program header, one program header, no section headers
            b.putShort(64).putShort(56).putShort(1).putShort(64).putShort(0).putShort(0)
            // PT_LOAD: readable and executable, the whole file
            b.putInt(1).putInt(5).putLong(0).putLong(base).putLong(base).putLong(size).putLong(size).putLong(0x1000)
            code.forEach { b.putInt(it) }
            return b.array()
        }
    }
}
