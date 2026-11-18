package io.github.sajadxdp0.tuxport

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import androidx.browser.customtabs.CustomTabsIntent
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.ByteArrayOutputStream
import java.io.File
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.concurrent.CountDownLatch
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

@RunWith(AndroidJUnit4::class)
class BrowserTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context = instrumentation.targetContext

    private fun withGuest(test: (Long, BrowserBridge, LinkedBlockingQueue<String>, AtomicBoolean) -> Unit) {
        val root = File(context.cacheDir, "browser-test").apply { mkdirs() }
        val store = File(root, "store").path
        val tar = File(root, "rootfs.tgz")
        context.assets.open("rootfs.tgz").use { input -> tar.outputStream().use(input::copyTo) }
        val urls = LinkedBlockingQueue<String>()
        val accepting = AtomicBoolean(true)
        val bridge = BrowserBridge { url -> accepting.get().also { if (it) urls.add(url) } }
        var machine = 0L
        try {
            Native.importRootfs(store, tar.path)
            Rootfs.ensureBrowser(context, bridge.configuration, store)
            val original = Native.readFile(store, "/usr/local/bin/xdg-open")!!
            Native.writeFile(store, "/usr/local/bin/xdg-open", "custom".toByteArray(), 493, true)
            Rootfs.ensureBrowser(context, bridge.configuration, store)
            assertEquals("custom", Native.readFile(store, "/usr/local/bin/xdg-open")!!.toString(Charsets.UTF_8))
            Native.writeFile(store, "/usr/local/bin/xdg-open", original, 493, true)
            machine = Native.start(store, 1)
            test(machine, bridge, urls, accepting)
            ok(machine, "rm /usr/local/bin/xdg-open; ln -s /bin/true /usr/local/bin/xdg-open")
            Native.stop(machine)
            machine = 0
            Rootfs.ensureBrowser(context, bridge.configuration, store)
            machine = Native.start(store, 1)
            ok(machine, "test \"$(readlink /usr/local/bin/xdg-open)\" = /bin/true")
        } finally {
            bridge.close()
            if (machine != 0L) Native.stop(machine)
            root.deleteRecursively()
        }
    }

    private fun spawn(machine: Long, console: Int, script: String) =
        Native.spawn(machine, console, "/bin/sh", arrayOf("sh", "-c", script), ConsolePty.ENVIRONMENT)

    private fun sh(machine: Long, script: String): Pair<String, Int> {
        val console = Native.openConsole(machine, 24, 100)
        try {
            val pid = spawn(machine, console, script)
            val bytes = ByteArrayOutputStream()
            val buffer = ByteArray(4096)
            while (true) {
                val n = Native.read(machine, console, buffer)
                if (n <= 0) break
                bytes.write(buffer, 0, n)
            }
            return bytes.toString("UTF-8") to Native.waitFor(machine, pid)
        } finally {
            Native.closeConsole(machine, console)
        }
    }

    private fun ok(machine: Long, script: String) {
        val (out, code) = sh(machine, "set -e\n$script")
        assertEquals(out, 0, code)
    }

    private fun configure(machine: Long, configuration: String) =
        ok(machine, "printf '%s' '$configuration' > ${BrowserBridge.CONFIG_PATH}")

    @Test(timeout = 60000)
    fun launcherWorksWithoutATtyAndReloadsItsEndpoint() = withGuest { machine, bridge, urls, accepting ->
        val url = "https://example.com/auth?state=a%2Bb&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"
        val pid = spawn(machine, -1, "\"${'$'}BROWSER\" '$url' >/dev/null 2>&1")
        assertEquals(0, Native.waitFor(machine, pid))
        assertEquals(url, urls.poll(5, TimeUnit.SECONDS))
        ok(machine, "su nobody -s /bin/sh -c 'xdg-open https://example.com/regular >/dev/null 2>&1'")
        assertEquals("https://example.com/regular", urls.poll(5, TimeUnit.SECONDS))

        accepting.set(false)
        val refused = sh(machine, "xdg-open https://example.com/refused")
        assertEquals(1, refused.second)
        assertTrue(refused.first, refused.first.contains("no focused Tuxport terminal or browser available"))
        assertTrue(urls.isEmpty())

        // This shell predates the replacement endpoint and retains an obsolete tab token.
        val survivor = spawn(machine, -1, """
            export TUXPORT_BROWSER_TOKEN=obsolete
            while [ ! -e /tmp/browser-ready ]; do sleep 0.05; done
            xdg-open https://example.com/reattached >/dev/null 2>&1
        """.trimIndent())
        bridge.close()
        val unavailable = sh(machine, "xdg-open https://example.com/closed")
        assertEquals(1, unavailable.second)
        BrowserBridge { urls.add(it); true }.use { replacement ->
            configure(machine, replacement.configuration)
            ok(machine, "touch /tmp/browser-ready")
            assertEquals(0, Native.waitFor(machine, survivor))
            assertEquals("https://example.com/reattached", urls.poll(5, TimeUnit.SECONDS))
        }
    }

    @Test(timeout = 60000)
    fun requestsAreAuthenticatedValidatedAndAcknowledged() = withGuest { machine, bridge, urls, _ ->
        val port = bridge.configuration.lineSequence().first().toInt()
        fun raw(request: ByteArray): String = Socket("127.0.0.1", port).use { socket ->
            socket.soTimeout = 5000
            socket.getOutputStream().write(request)
            socket.shutdownOutput()
            socket.getInputStream().bufferedReader().readText()
        }
        val token = bridge.configuration.lineSequence().drop(1).first()
        assertTrue(raw("stale\nhttps://example.com/\n".toByteArray()).contains("authentication failed"))
        assertTrue(raw("$token\nfile:///etc/passwd\n".toByteArray()).contains("invalid web URL"))
        assertTrue(raw("$token\nhttps://example.com/".toByteArray() + byteArrayOf(0xff.toByte(), 10)).contains("invalid web URL"))
        for (url in listOf("file:///etc/passwd", "intent://example", "https:///missing-host",
            "https://example.com/has space", "https://example.com:99999/",
            "https://example.com/" + "a".repeat(8192))) {
            assertFalse(url, BrowserBridge.isWebUrl(url))
            assertEquals(url, 1, sh(machine, "xdg-open '$url'").second)
        }
        assertTrue(urls.isEmpty())
        // A peer that never acknowledges must not leave the launcher blocked indefinitely.
        ServerSocket(0, 1, InetAddress.getByName("127.0.0.1")).use { silent ->
            configure(machine, "${silent.localPort}\n$token\n")
            val started = System.nanoTime()
            val result = sh(machine, "xdg-open https://example.com/timeout")
            val elapsed = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - started)
            assertEquals(result.first, 1, result.second)
            assertTrue(result.first, result.first.contains("timed out"))
            assertTrue("Timeout took $elapsed ms", elapsed in 4000..10000)
        }
    }

    @Test(timeout = 180000)
    fun tmuxAndScreenSurviveReattachAndNesting() {
        assumeTrue(InstrumentationRegistry.getArguments().getString("network") == "true")
        withGuest { machine, _, urls, _ ->
            ok(machine, """
                printf 'nameserver 127.0.0.53\n' > /etc/resolv.conf
                timeout 60 apk add --no-cache tmux screen
            """.trimIndent())
            fun attach(command: String, action: () -> Unit) {
                val console = Native.openConsole(machine, 24, 100)
                val pid = spawn(machine, console, command)
                val output = ByteArrayOutputStream()
                val reader = thread {
                    val buffer = ByteArray(4096)
                    while (true) {
                        val n = Native.read(machine, console, buffer)
                        if (n <= 0) break
                        synchronized(output) { output.write(buffer, 0, n) }
                    }
                }
                try {
                    action()
                } catch (e: AssertionError) {
                    throw AssertionError("$command: " + synchronized(output) { output.toString("UTF-8").map { if (it.isISOControl()) ' ' else it }.joinToString("") }, e)
                } finally {
                    Native.closeConsole(machine, console)
                    Native.waitFor(machine, pid)
                    reader.join(5000)
                }
            }
            ok(machine, "tmux -f /dev/null new-session -d -s browser")
            repeat(2) { pass ->
                attach("tmux attach-session -t browser") {
                    ok(machine, """
                        timeout 5 sh -c 'until [ -n "$(tmux list-clients)" ]; do sleep 0.05; done'
                        tmux send-keys -t browser 'xdg-open https://example.com/tmux-$pass' Enter
                    """.trimIndent())
                    assertEquals("https://example.com/tmux-$pass", urls.poll(10, TimeUnit.SECONDS))
                    ok(machine, "tmux detach-client -s browser")
                }
            }
            ok(machine, "mkfifo /tmp/browser-screen /tmp/browser-inner")
            val readUrls = "exec 3<>/tmp/browser-screen; while read -r url <&3; do xdg-open \"${'$'}url\"; done"
            spawn(machine, -1, "screen -DmS browser /bin/sh -c '$readUrls'")
            ok(machine, "timeout 5 sh -c 'until screen -ls | grep -q browser; do sleep 0.05; done'")
            repeat(2) { pass ->
                attach("screen -r browser") {
                    ok(machine, """
                        timeout 5 sh -c 'until screen -ls | grep -q Attached; do sleep 0.05; done'
                        printf 'https://example.com/screen-$pass\n' > /tmp/browser-screen
                    """.trimIndent())
                    assertEquals("https://example.com/screen-$pass", urls.poll(10, TimeUnit.SECONDS))
                    ok(machine, "screen -S browser -d")
                }
            }
            ok(machine, "printf '%s' '$readUrls' | sed s/browser-screen/browser-inner/ > /tmp/browser-inner.sh")
            ok(machine, "tmux new-session -d -s nested 'screen -m -S inner /bin/sh /tmp/browser-inner.sh'")
            attach("tmux attach-session -t nested") {
                ok(machine, """
                    timeout 5 sh -c 'until screen -ls | grep -q inner; do sleep 0.05; done'
                    printf 'https://example.com/nested\n' > /tmp/browser-inner
                """.trimIndent())
                assertEquals("https://example.com/nested", urls.poll(10, TimeUnit.SECONDS))
                ok(machine, "tmux detach-client -s nested")
            }
        }
    }

    @Test(timeout = 10000)
    fun expiredQueuedRequestsDoNotLaunchLater() {
        val entered = CountDownLatch(1)
        val release = CountDownLatch(1)
        val called = AtomicBoolean()
        BrowserBridge { called.set(true); true }.use { bridge ->
            Handler(Looper.getMainLooper()).post {
                entered.countDown()
                release.await(5, TimeUnit.SECONDS)
            }
            try {
                assertTrue(entered.await(2, TimeUnit.SECONDS))
                val lines = bridge.configuration.lines()
                Socket("127.0.0.1", lines[0].toInt()).use { socket ->
                    socket.soTimeout = 5000
                    socket.getOutputStream().write("${lines[1]}\nhttps://example.com/expired\n".toByteArray())
                    socket.shutdownOutput()
                    assertTrue(socket.getInputStream().bufferedReader().readLine().startsWith("ERROR "))
                }
            } finally {
                release.countDown()
            }
            instrumentation.waitForIdleSync()
            assertFalse(called.get())
        }
    }

    @Test
    fun webLinksUseABrowsableCustomTabIntent() {
        instrumentation.runOnMainSync {
            var launched: Intent? = null
            val activity = object : Activity() {
                override fun startActivity(intent: Intent, options: Bundle?) { launched = intent }
            }
            assertTrue(TerminalLinks(activity) { _, _ -> error("Unexpected link failure") }
                .openBrowser("https://example.com/auth?state=a%2Bb&code=test"))
            val intent = launched!!
            assertEquals(Intent.ACTION_VIEW, intent.action)
            assertEquals("https://example.com/auth?state=a%2Bb&code=test", intent.dataString)
            assertTrue(intent.hasCategory(Intent.CATEGORY_BROWSABLE))
            assertTrue(intent.hasExtra(CustomTabsIntent.EXTRA_SESSION))
            val unavailable = object : Activity() {
                init { attachBaseContext(context) }
                override fun startActivity(intent: Intent, options: Bundle?) {
                    throw ActivityNotFoundException()
                }
            }
            var notified = false
            assertFalse(TerminalLinks(unavailable) { _, _ -> notified = true }.openBrowser("https://example.com/"))
            assertTrue(notified)
        }
    }
}
