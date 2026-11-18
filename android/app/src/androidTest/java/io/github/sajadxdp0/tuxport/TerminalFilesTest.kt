package io.github.sajadxdp0.tuxport

import android.content.Context
import android.content.Intent
import android.net.Uri
import androidx.core.content.edit
import androidx.lifecycle.ViewModelProvider
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.io.ByteArrayInputStream
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.util.UUID

/** Real store copies, including failures, with no dependency on the app's running rootfs. */
@RunWith(AndroidJUnit4::class)
class TerminalFilesTest {
    private val context = InstrumentationRegistry.getInstrumentation().targetContext
    private val root = File(context.cacheDir, "file-import-test-${UUID.randomUUID()}")
    private val store = File(root, "store").path
    private val account = Rootfs.Account("root", 0, 0, "/root")

    @Before
    fun prepare() {
        root.mkdirs()
        val tar = File(root, "rootfs.tgz")
        context.assets.open("rootfs.tgz").use { input -> tar.outputStream().use(input::copyTo) }
        Native.importRootfs(store, tar.path)
    }

    @After
    fun cleanUp() { root.deleteRecursively() }

    @Test
    fun copiesBinaryFilesAndNumbersCollisionsWithoutOverwriting() {
        val bytes = ByteArray(256 * 1024 + 37) { it.toByte() }
        val first = TerminalFiles.copyIntoHome(store, account, "资料 100%.bin", bytes.inputStream())
        val second = TerminalFiles.copyIntoHome(store, account, "资料 100%.bin", byteArrayOf(9).inputStream())
        assertEquals("/root/资料 100%.bin", first)
        assertEquals("/root/资料 100% (1).bin", second)
        assertArrayEquals(bytes, Native.readFile(store, first))
        assertArrayEquals(byteArrayOf(9), Native.readFile(store, second))
    }

    @Test
    fun failedStreamsRemoveThePartialCopyAndKeepExistingFiles() {
        Native.writeFile(store, "/root/keep.txt", "original".toByteArray(), 420, true)
        val input = object : InputStream() {
            var reads = 0
            override fun read(): Int = throw IOException("Source disconnected")
            override fun read(bytes: ByteArray, offset: Int, length: Int): Int {
                if (reads++ > 0) throw IOException("Source disconnected")
                bytes[offset] = 42
                return 1
            }
        }
        assertThrows(IOException::class.java) {
            TerminalFiles.copyIntoHome(store, account, "keep.txt", input)
        }
        assertArrayEquals("original".toByteArray(), Native.readFile(store, "/root/keep.txt"))
        assertNull(Native.readFile(store, "/root/keep (1).txt"))
    }

    @Test
    fun failedSourceCloseAlsoRemovesTheCopy() {
        val input = object : ByteArrayInputStream("data".toByteArray()) {
            override fun close() { throw IOException("Source close failed") }
        }
        assertThrows(IOException::class.java) {
            TerminalFiles.copyIntoHome(store, account, "close-failed.txt", input)
        }
        assertNull(Native.readFile(store, "/root/close-failed.txt"))
    }

    @Test
    fun sanitizesNamesAndSupportsEmptyFilesAndEscapedHomePaths() {
        Native.browseCreate(store, "/root", "/root", "100% home", true, 1000, 1000)
        val user = Rootfs.Account("user", 1000, 1000, "/root/100% home")
        val path = TerminalFiles.copyIntoHome(store, user, "../bad\nname\u001b.txt", byteArrayOf().inputStream())
        assertEquals("/root/100% home/.._bad_name_.txt", path)
        assertArrayEquals(byteArrayOf(), Native.readFile(store, path))
        assertFalse(path.contains('\n'))
    }

    @Test
    fun refusesMissingOrWholeSystemHomes() {
        for (home in listOf("/", "/missing-home")) {
            assertThrows(IOException::class.java) {
                TerminalFiles.copyIntoHome(store, account.copy(home = home), "file.txt", byteArrayOf(1).inputStream())
            }
        }
        assertNull(Native.readFile(store, "/file.txt"))
    }

    @Test
    fun guestCanReadImportedFilesAndClipboardPathsAreLiteralShellArguments() {
        assertEquals("/root/photo.png /root/中文.txt", TerminalFiles.clipboardPaths(listOf("/root/photo.png", "/root/中文.txt")))
        assertEquals("/root/my\\ photo.png", TerminalFiles.clipboardPaths(listOf("/root/my photo.png")))
        val machine = Native.start(store, 1)
        try {
            val name = "quote' and \$dollar.txt"
            val path = TerminalFiles.copyIntoHome(store, account, name, "imported".toByteArray().inputStream())
            val arguments = listOf(
                path, "/root/\$(touch /root/injected)", "/root/back`tick`", "/root/中文",
                "/root/*?[ab]{one,two}!(x)~#^&|;<>\\\"", "/root/tab\tname", "/root/trailing\\",
            )
            val command = "cat ${TerminalFiles.clipboardPaths(listOf(path))} > /root/content; " +
                "printf '%s\\n' ${TerminalFiles.clipboardPaths(arguments)} > /root/arguments"
            val pid = Native.spawn(machine, -1, "/bin/sh", arrayOf("sh", "-c", command), arrayOf("PATH=/bin:/usr/bin"))
            assertEquals(0, Native.waitFor(machine, pid))
            assertArrayEquals("imported".toByteArray(), Native.readFile(store, "/root/content"))
            assertArrayEquals((arguments.joinToString("\n") + "\n").toByteArray(), Native.readFile(store, "/root/arguments"))
            assertNull(Native.readFile(store, "/root/injected"))
        } finally {
            Native.stop(machine)
        }
    }

    @Test
    fun pickerResultWaitsForUnlockAndSurvivesRecreation() {
        val prefs = context.getSharedPreferences("settings", Context.MODE_PRIVATE)
        val privacy = Settings.privacyMode(context)
        val fileAccess = prefs.getBoolean("file_access", false)
        Settings.setPrivacyMode(context, true)
        try {
            ActivityScenario.launch<MainActivity>(Intent(context, MainActivity::class.java)).use { scenario ->
                lateinit var files: TerminalFiles
                scenario.onActivity { activity ->
                    assertTrue(Privacy.locked(activity))
                    files = ViewModelProvider(activity)[TerminalFiles::class.java]
                    assertTrue(files.begin("root"))
                    files.selected(listOf(Uri.parse("content://unavailable/import-test")))
                }
                scenario.recreate()
                scenario.onActivity { activity ->
                    assertTrue(Privacy.locked(activity))
                    assertSame(files, ViewModelProvider(activity)[TerminalFiles::class.java])
                    assertTrue(files.occupied)
                    assertFalse(files.copying)
                    assertNull(files.result)
                }
            }
        } finally {
            prefs.edit(commit = true) {
                putBoolean("privacy_mode", privacy)
                putBoolean("file_access", fileAccess)
            }
        }
    }
}
