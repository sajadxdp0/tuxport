package io.github.sajadxdp0.tuxport

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.graphics.Bitmap
import android.os.Build
import android.os.SystemClock
import android.view.ContextThemeWrapper
import android.view.View
import android.view.ViewGroup
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inspector.WindowInspector
import android.widget.TextView
import androidx.test.core.app.ActivityScenario
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
import java.lang.reflect.Proxy

/** Exercises the real input connection and paste encoder without sending input to a shell. */
@RunWith(AndroidJUnit4::class)
class TerminalInputTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context = instrumentation.targetContext

    private fun onMain(action: () -> Unit) {
        instrumentation.runOnMainSync(action)
        instrumentation.waitForIdleSync()
    }

    private class Host : VtView.Host {
        val bytes = ByteArrayOutputStream()
        override fun onInput(data: ByteArray) { bytes.write(data) }
        override fun onGrid(cols: Int, rows: Int, cellWidth: Int, cellHeight: Int) {}
        override fun onZoom(fontPx: Int) {}
        override val ctrl = false
        override val alt = false
        override val shift = false
        override fun modifiersUsed() {}
        override fun onLink(uri: String) {}
        override fun onSearchCount(total: Int, selected: Int) {}
        fun text() = bytes.toString("UTF-8")
    }

    private fun withConnection(test: (InputConnection, Host, Long) -> Unit) {
        val root = File(context.cacheDir, "terminal-input-test").apply { mkdirs() }
        val tar = File(root, "rootfs.tgz")
        context.assets.open("rootfs.tgz").use { input -> tar.outputStream().use(input::copyTo) }
        Native.importRootfs(File(root, "store").path, tar.path)
        val machine = Native.start(File(root, "store").path, 1)
        val console = Native.openConsole(machine, 24, 80)
        val tab = VtNative.create(machine, console, 80, 24, 8, 16)
        try {
            ActivityScenario.launch<LicensesActivity>(Intent(context, LicensesActivity::class.java)).use { scenario ->
                lateinit var view: VtView
                lateinit var connection: InputConnection
                val host = Host()
                scenario.onActivity { activity ->
                    assertFalse("Tests require an unlocked page", Privacy.locked(activity))
                    val events = Proxy.newProxyInstance(
                        VtSession.Events::class.java.classLoader, arrayOf(VtSession.Events::class.java),
                    ) { _, _, _ -> null } as VtSession.Events
                    val session = VtSession(1, ConsolePty(machine, "", "root"), machine, events)
                    // Supply only the terminal; starting a session would also start shell threads.
                    VtSession::class.java.getDeclaredField("tab").apply { isAccessible = true }.setLong(session, tab)
                    view = VtView(ContextThemeWrapper(activity, R.style.Theme_Tuxport_Terminal), host)
                    view.attach(session)
                    connection = view.onCreateInputConnection(EditorInfo())
                }
                try {
                    // Clipboard reads require window focus, which can follow ActivityScenario's resume.
                    val deadline = SystemClock.uptimeMillis() + 5000
                    var focused = false
                    while (!focused && SystemClock.uptimeMillis() < deadline) {
                        scenario.onActivity { focused = it.hasWindowFocus() }
                        if (!focused) SystemClock.sleep(20)
                    }
                    assertTrue("The test page must have window focus", focused)
                    test(connection, host, tab)
                } finally {
                    onMain {
                        button(android.R.id.button2)?.performClick()
                        view.attach(null)
                    }
                }
            }
        } finally {
            VtNative.destroy(tab)
            Native.closeConsole(machine, console)
            Native.stop(machine)
            root.deleteRecursively()
        }
    }

    private fun button(id: Int): View? = WindowInspector.getGlobalWindowViews()
        .firstNotNullOfOrNull { it.findViewById(id) }

    private fun hasPreview(): Boolean {
        fun contains(view: View): Boolean =
            (view is TextView && view.text == context.getString(R.string.paste_unsafe_title)) ||
                (view is ViewGroup && (0 until view.childCount).any { contains(view.getChildAt(it)) })
        return WindowInspector.getGlobalWindowViews().any(::contains)
    }

    @Test
    fun removingTrailingNewlinesPreservesOtherWhitespaceAndDoesNotReplay() {
        withConnection { connection, host, _ ->
            onMain {
                connection.commitText("  printf hello \t\r\n\r\n", 1)
                assertTrue(hasPreview())
                assertEquals("", host.text())
                assertTrue("A safe alternative must be offered", button(android.R.id.button3)?.visibility == View.VISIBLE)
            }
            if (InstrumentationRegistry.getArguments().getString("screenshots") == "true") {
                SystemClock.sleep(500) // Let the dialog's window animation finish before visual inspection.
                val screenshot = instrumentation.uiAutomation.takeScreenshot()!!
                File(context.cacheDir, "paste-preview.png").outputStream().use {
                    screenshot.compress(Bitmap.CompressFormat.PNG, 100, it)
                }
                screenshot.recycle()
            }
            onMain { button(android.R.id.button3)!!.performClick() }
            onMain {
                connection.finishComposingText()
                assertEquals("  printf hello \t", host.text())
                assertFalse(hasPreview())
            }
        }
    }

    @Test
    fun removingTrailingNewlinesIsNotOfferedForUnsafeOrEmptyRemainders() {
        withConnection { connection, host, _ ->
            for (text in listOf("first\nsecond\n", "first\u001b[201~\n", "\r\n\r\n")) {
                onMain {
                    connection.commitText(text, 1)
                    assertTrue(hasPreview())
                    assertFalse("Unsafe or empty text must not be offered", button(android.R.id.button3)?.visibility == View.VISIBLE)
                    assertEquals("", host.text())
                    button(android.R.id.button2)!!.performClick()
                }
            }
        }
    }

    @Test
    fun removingTrailingNewlinesUsesCurrentPasteMode() {
        withConnection { connection, host, tab ->
            onMain {
                connection.commitText("hello\n", 1)
                assertTrue(hasPreview())
                assertTrue(button(android.R.id.button3)?.visibility == View.VISIBLE)
                VtNative.print(tab, "\u001b[?2004h".toByteArray())
                button(android.R.id.button3)!!.performClick()
            }
            assertEquals("\u001b[200~hello\u001b[201~", host.text())
        }
    }

    @Test
    fun replacementIsDeliveredImmediatelyAndNotReplayedOnFinish() {
        assumeTrue(Build.VERSION.SDK_INT >= 34)
        withConnection { connection, host, _ ->
            onMain {
                assertTrue(connection.replaceText(0, 0, "first", 1, null))
                assertEquals("first", host.text())
                assertEquals("", connection.getTextBeforeCursor(100, 0).toString())
                connection.finishComposingText()
                assertEquals("first", host.text())
                connection.replaceText(0, 0, "second", 1, null)
                connection.finishComposingText()
                assertEquals("firstsecond", host.text())
            }
        }
    }

    @Test
    fun replacementRequiresConfirmationBeforeSendingLineBreaks() {
        assumeTrue(Build.VERSION.SDK_INT >= 34)
        withConnection { connection, host, _ ->
            onMain {
                connection.replaceText(0, 0, "first\nsecond", 1, null)
                assertTrue("Replacement must show the paste preview", hasPreview())
                assertEquals("", host.text())
                connection.finishComposingText()
                assertEquals("", host.text())
                button(android.R.id.button1)!!.performClick()
            }
            assertEquals("first\rsecond", host.text())
        }
    }

    @Test
    fun finishingCompositionUsesPasteSafetyAndCancellationDiscardsIt() {
        withConnection { connection, host, _ ->
            onMain {
                connection.setComposingText("first\nsecond", 1)
                connection.finishComposingText()
                assertTrue("Finishing composition must show the paste preview", hasPreview())
                assertEquals("", host.text())
                button(android.R.id.button2)!!.performClick()
            }
            onMain {
                connection.finishComposingText()
                connection.commitText("ok", 1)
                assertEquals("ok", host.text())
            }
        }
    }

    @Test
    fun imePasteMenuUsesClipboardSafety() {
        withConnection { connection, host, _ ->
            val clipboard = context.getSystemService(ClipboardManager::class.java)
            val previous = clipboard.primaryClip
            try {
                onMain {
                    clipboard.setPrimaryClip(ClipData.newPlainText("Test", "first\nsecond"))
                    assertTrue(connection.performContextMenuAction(android.R.id.paste))
                    assertTrue("IME paste must show the paste preview", hasPreview())
                    assertEquals("", host.text())
                    button(android.R.id.button2)!!.performClick()
                }
                onMain {
                    connection.finishComposingText()
                    assertEquals("", host.text())
                }
            } finally {
                if (previous == null) clipboard.clearPrimaryClip() else clipboard.setPrimaryClip(previous)
            }
        }
    }

    @Test
    fun ordinaryCompositionAndEnterStillTypeOnce() {
        withConnection { connection, host, _ ->
            onMain {
                connection.setComposingText("ni", 1)
                assertEquals("", host.text())
                connection.commitText("你", 1)
                connection.finishComposingText()
                connection.setComposingText("好", 1)
                connection.finishComposingText()
                connection.finishComposingText()
                connection.commitText("\n", 1)
                assertEquals("你好\r", host.text())
                assertFalse(hasPreview())
            }
        }
    }

    @Test
    fun bracketedPasteAndItsTerminatorUseTheSameChecksForComposition() {
        withConnection { connection, host, tab ->
            onMain {
                VtNative.print(tab, "\u001b[?2004h".toByteArray())
                connection.setComposingText("first\nsecond", 1)
                connection.finishComposingText()
                assertEquals("\u001b[200~first\nsecond\u001b[201~", host.text())
                assertFalse(hasPreview())
                host.bytes.reset()
                connection.setComposingText("\u001b[201~", 1)
                connection.finishComposingText()
                assertTrue("The bracketed paste terminator must require confirmation", hasPreview())
                assertEquals("", host.text())
            }
        }
    }
}
