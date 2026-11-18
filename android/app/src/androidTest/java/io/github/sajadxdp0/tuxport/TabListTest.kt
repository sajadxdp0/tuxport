package io.github.sajadxdp0.tuxport

import android.view.ContextThemeWrapper
import android.view.View
import android.view.ViewGroup
import android.widget.EditText
import android.widget.FrameLayout
import androidx.recyclerview.widget.RecyclerView
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.lang.reflect.Proxy

/** Exercises recycled row identity and bounded dropdown layout without starting guest shells. */
@RunWith(AndroidJUnit4::class)
class TabListTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()

    private class Target : TabListView.Target {
        var shown: VtSession? = null
        var closed: VtSession? = null
        override fun show(session: VtSession) { shown = session }
        override fun close(session: VtSession) { closed = session }
        override fun dismiss() {}
    }

    private fun sessions(count: Int): List<VtSession> {
        val events = Proxy.newProxyInstance(
            VtSession.Events::class.java.classLoader, arrayOf(VtSession.Events::class.java),
        ) { _, _, _ -> null } as VtSession.Events
        return List(count) { VtSession(it + 1, ConsolePty(0, "", "root"), 0, events) }
    }

    private fun layout(view: View, height: Int = view.dp(480)) {
        val width = view.dp(360)
        view.measure(View.MeasureSpec.makeMeasureSpec(width, View.MeasureSpec.EXACTLY),
            View.MeasureSpec.makeMeasureSpec(height, View.MeasureSpec.EXACTLY))
        view.layout(0, 0, width, height)
    }

    @Test fun closeAndSelectKeepTheirSessionAcrossRemoval() = instrumentation.runOnMainSync {
        val context = ContextThemeWrapper(instrumentation.targetContext, R.style.Theme_Tuxport_Terminal)
        val target = Target()
        val view = TabListView(context, target)
        val sessions = sessions(3)
        view.open(sessions, sessions[1])
        layout(view)
        val list = view.getChildAt(0) as RecyclerView
        val secondRow = list.findViewHolderForAdapterPosition(1)!!.itemView as ViewGroup
        val close = secondRow.getChildAt(1)
        // A callback can run after data changed but before RecyclerView rebinds its visible rows.
        view.render(sessions.drop(1), sessions[1])
        close.performClick()
        assertSame(sessions[1], target.closed)
        layout(view)
        val firstRow = list.findViewHolderForAdapterPosition(0)!!.itemView as ViewGroup
        firstRow.getChildAt(0).performClick()
        assertSame(sessions[1], target.shown)
        assertTrue(firstRow.getChildAt(0).isSelected)
        assertEquals(context.getString(R.string.tab_description, 1, "sh"), firstRow.getChildAt(0).contentDescription)
    }

    @Test fun listFitsBelowTheBarAndRevealsTheCurrentTab() = instrumentation.runOnMainSync {
        val context = ContextThemeWrapper(instrumentation.targetContext, R.style.Theme_Tuxport_Terminal)
        val view = TabListView(context, Target())
        val sessions = sessions(50)
        view.open(sessions, sessions.last())
        layout(view, view.dp(100))
        val list = view.getChildAt(0) as RecyclerView
        assertEquals(0, list.top)
        assertEquals(view.width, list.width)
        assertEquals(view.height, list.height)
        assertTrue(list.canScrollVertically(-1))
        val current = list.findViewHolderForAdapterPosition(49)!!.itemView
        assertEquals(view.dp(32), current.height)
        assertTrue(current.bottom <= list.height)
        layout(view, view.dp(480))
        assertEquals(view.dp(320), list.height)
    }

    @Test fun openingAndScrollingPreserveTheEditorsFocus() = instrumentation.runOnMainSync {
        val context = ContextThemeWrapper(instrumentation.targetContext, R.style.Theme_Tuxport_Terminal)
        val editor = EditText(context)
        val view = TabListView(context, Target())
        val root = FrameLayout(context).apply {
            addView(editor)
            addView(view)
        }
        layout(root)
        assertTrue(editor.requestFocus())
        val sessions = sessions(50)
        view.open(sessions, sessions.last())
        layout(root)
        assertSame("Opening the list must keep the editor as the IME target", editor, root.findFocus())
        (view.getChildAt(0) as RecyclerView).scrollBy(0, -view.dp(32))
        assertSame(editor, root.findFocus())
        view.close()
        assertSame("Dismissing the list must not require reconnecting the editor", editor, root.findFocus())
    }
}
