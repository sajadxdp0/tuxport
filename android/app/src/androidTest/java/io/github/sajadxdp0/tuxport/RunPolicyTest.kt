package io.github.sajadxdp0.tuxport

import android.content.Context
import android.content.ContextWrapper
import androidx.core.content.edit
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class RunPolicyTest {
    // Keep these tests independent of the user's settings and running kernel.
    private val context = object : ContextWrapper(InstrumentationRegistry.getInstrumentation().targetContext) {
        override fun getSharedPreferences(name: String, mode: Int) =
            super.getSharedPreferences("run-policy-test-$name", mode)
    }
    private val prefs get() = context.getSharedPreferences("settings", Context.MODE_PRIVATE)

    @Before
    @After
    fun clear() { prefs.edit(commit = true) { clear() } }

    @Test
    fun oldSwitchRetainsItsBehaviour() {
        assertEquals(RunPolicy.SLEEP, Settings.runPolicy(context))
        assertEquals(RunPolicy.ALWAYS, Settings.nextRunPolicy(context))
        prefs.edit { putBoolean("keep_awake", true) }
        assertEquals(RunPolicy.ALWAYS, Settings.runPolicy(context))
        assertEquals(RunPolicy.SLEEP, Settings.nextRunPolicy(context))
        Settings.setRunPolicy(context, RunPolicy.BUSY)
        assertFalse(prefs.contains("keep_awake"))
        assertEquals(RunPolicy.BUSY, Settings.runPolicy(context))
    }

    @Test
    fun notificationRemembersTheLastNonAlwaysChoice() {
        Settings.setRunPolicy(context, RunPolicy.BUSY)
        repeat(2) {
            Settings.setRunPolicy(context, Settings.nextRunPolicy(context))
            assertEquals(RunPolicy.ALWAYS, Settings.runPolicy(context))
            assertEquals(RunPolicy.BUSY, Settings.nextRunPolicy(context))
            Settings.setRunPolicy(context, Settings.nextRunPolicy(context))
            assertEquals(RunPolicy.BUSY, Settings.runPolicy(context))
        }
        Settings.setRunPolicy(context, RunPolicy.SLEEP)
        Settings.setRunPolicy(context, RunPolicy.ALWAYS)
        assertEquals(RunPolicy.SLEEP, Settings.nextRunPolicy(context))
    }

    @Test
    fun idleGraceHandlesActivityFailuresAndStaleResults() {
        val activity = GuestActivity()
        activity.reset(0)
        activity.sample(10, 0)
        activity.sample(10, GuestActivity.IDLE_MS - 1)
        assertTrue(activity.isBusy(GuestActivity.IDLE_MS - 1))
        assertFalse(activity.isBusy(GuestActivity.IDLE_MS))
        activity.sample(11, 40_000)
        assertTrue(activity.isBusy(40_000))
        assertFalse(activity.isBusy(70_000))
        activity.sample(-1, 70_000)
        assertTrue(activity.isBusy(70_000))
        activity.reset(100_000)
        activity.sample(12, 80_000)
        assertTrue(activity.isBusy(129_999))
        assertFalse(activity.isBusy(130_000))
        activity.sample(0, 130_000)
        assertTrue(activity.isBusy(130_000))
        activity.sample(1, 131_000, 180_000)
        assertTrue(activity.isBusy(180_000))
        assertFalse(activity.isBusy(210_000))
    }
}
