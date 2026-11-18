package io.github.sajadxdp0.tuxport

import android.graphics.Bitmap
import android.graphics.Canvas
import android.os.SystemClock
import android.view.ContextThemeWrapper
import androidx.core.graphics.createBitmap
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.lang.reflect.Proxy

/** Real frames and view pixels with controlled output chunks, without a shell or IME timing. */
@RunWith(AndroidJUnit4::class)
class TerminalResizeTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()

    private class Host : VtView.Host {
        override fun onInput(data: ByteArray) {}
        override fun onGrid(cols: Int, rows: Int, cellWidth: Int, cellHeight: Int) {}
        override fun onZoom(fontPx: Int) {}
        override val ctrl = false
        override val alt = false
        override val shift = false
        override fun modifiersUsed() {}
        override fun onLink(uri: String) {}
        override fun onSearchCount(total: Int, selected: Int) {}
    }

    private class Fixture(val view: VtView, val tab: Long) {
        private val bitmaps = mutableListOf<Bitmap>()
        fun draw(): Bitmap = createBitmap(view.width, view.height).also {
            bitmaps.add(it)
            view.draw(Canvas(it))
        }
        fun previewMatches(before: Bitmap): Boolean {
            val cropped = Bitmap.createBitmap(before, 0, 0, view.width, view.height)
            return try { cropped.sameAs(draw()) } finally { cropped.recycle() }
        }
        fun feed(text: String) = VtNative.print(tab, text.toByteArray())
        fun pending() = VtFrame(checkNotNull(VtNative.frame(tab, true))).outputPending
        fun close() { bitmaps.forEach(Bitmap::recycle) }
    }

    private fun withView(test: Fixture.() -> Unit) {
        val context = instrumentation.targetContext
        val root = File(context.cacheDir, "terminal-resize-${System.nanoTime()}").apply { mkdirs() }
        val tar = File(root, "rootfs.tgz")
        context.assets.open("rootfs.tgz").use { input -> tar.outputStream().use(input::copyTo) }
        val store = File(root, "store")
        Native.importRootfs(store.path, tar.path)
        val machine = Native.start(store.path, 1)
        val console = Native.openConsole(machine, 24, 80)
        val tab = VtNative.create(machine, console, 80, 24, 8, 16)
        try {
            instrumentation.runOnMainSync {
                val events = Proxy.newProxyInstance(
                    VtSession.Events::class.java.classLoader, arrayOf(VtSession.Events::class.java),
                ) { _, _, _ -> null } as VtSession.Events
                val session = VtSession(1, ConsolePty(machine, "", "root"), machine, events)
                // Supply the native terminal without starting the session's shell threads.
                VtSession::class.java.getDeclaredField("tab").apply { isAccessible = true }.setLong(session, tab)
                val view = VtView(ContextThemeWrapper(context, R.style.Theme_Tuxport_Terminal), Host())
                view.attach(session)
                view.layout(0, 0, 480, 800)
                val fixture = Fixture(view, tab)
                try {
                    fixture.feed("\u001b[?25l" + (0..99).joinToString("\r\n") { "old row $it" })
                    fixture.test()
                } finally {
                    view.attach(null)
                    fixture.close()
                }
            }
        } finally {
            VtNative.destroy(tab)
            Native.closeConsole(machine, console)
            Native.stop(machine)
            root.deleteRecursively()
        }
    }

    @Test fun queuedFullFrameStillPreservesPaintedContent() = withView {
        val before = draw()
        view.setFillWide(false)
        view.layout(0, 0, 480, 400)
        assertTrue("A full-frame request must not discard the painted preview", previewMatches(before))
    }

    @Test fun subrowShrinkKeepsThePreview() = withView {
        val before = draw()
        val renderer = VtView::class.java.getDeclaredField("renderer").apply { isAccessible = true }.get(view) as VtRenderer
        val h = renderer.cellHeight * 8 + 2
        view.layout(0, 0, 480, h)
        assertTrue(previewMatches(before))
        view.layout(0, 0, 480, h - 1)
        assertTrue("A second layout within the same row must keep the preview", previewMatches(before))
    }

    @Test fun splitSynchronizedOutputStaysPending() = withView {
        VtNative.resize(tab, 40, 12, 8, 16)
        feed("\u001b[?20")
        assertTrue("An unfinished sync prefix is not a redraw", pending())
        SystemClock.sleep(40)
        assertTrue("Quiet output inside a control sequence is still incomplete", pending())
        feed("26h\u001b[Hnew")
        assertTrue(pending())
        feed("\u001b[?2026l")
        SystemClock.sleep(40)
        assertFalse(pending())
    }

    @Test fun ordinaryOutputWaitsForTheBurstToSettle() = withView {
        VtNative.resize(tab, 40, 12, 8, 16)
        feed("\u001b[?25l")
        assertTrue("A cursor update can precede the actual redraw", pending())
        feed("\u001b[Hnew content")
        assertTrue(pending())
        SystemClock.sleep(40)
        assertFalse(pending())
    }

    @Test fun silentProgramDoesNotKeepThePreviewForever() = withView {
        val before = draw()
        view.layout(0, 0, 480, 400)
        assertTrue(previewMatches(before))
        SystemClock.sleep(250)
        assertFalse("The timeout must reveal the resized terminal without output", previewMatches(before))
    }

    @Test fun completedRedrawReplacesAllPreviewRows() = withView {
        val before = draw()
        view.layout(0, 0, 480, 400)
        feed("\u001b[?2026h\u001b[H\u001b[2Jnew frame")
        assertTrue(previewMatches(before))
        feed("\u001b[?2026l")
        SystemClock.sleep(40)
        assertFalse("Completed output must replace the preview before the timeout", previewMatches(before))
        val accepted = draw()
        view.onStarted()
        assertTrue("Accepted pixels must match a fresh full frame", accepted.sameAs(draw()))
    }

    @Test fun detachingDropsThePreview() = withView {
        val before = draw()
        view.layout(0, 0, 480, 400)
        assertTrue(previewMatches(before))
        view.attach(null)
        assertFalse("A detached session must not keep its resize preview", previewMatches(before))
    }
}
