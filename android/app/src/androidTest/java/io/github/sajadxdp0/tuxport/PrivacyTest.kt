package io.github.sajadxdp0.tuxport

import android.content.ComponentName
import android.content.Context
import android.content.ContextWrapper
import android.content.Intent
import android.view.View
import android.view.WindowManager.LayoutParams.FLAG_SECURE
import androidx.core.content.edit
import androidx.lifecycle.Lifecycle
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/** Access checks use real Android lifecycle callbacks, without a kernel or an authentication bypass. */
@RunWith(AndroidJUnit4::class)
class PrivacyTest {
    private val context = InstrumentationRegistry.getInstrumentation().targetContext

    @Test
    fun privacyDisablesFileAccessEvenWithAnOldGrantSetting() {
        val isolated = object : ContextWrapper(context) {
            override fun getSharedPreferences(name: String, mode: Int) =
                super.getSharedPreferences("privacy-test-$name", mode)
        }
        val prefs = isolated.getSharedPreferences("settings", Context.MODE_PRIVATE)
        prefs.edit(commit = true) { clear() }
        try {
            assertFalse(Settings.privacyMode(isolated))
            Settings.setFileAccess(isolated, true)
            assertTrue(Settings.fileAccess(isolated))
            Settings.setPrivacyMode(isolated, true)
            assertFalse(Settings.fileAccess(isolated))
            prefs.edit { putBoolean("file_access", true) }
            assertFalse(Settings.fileAccess(isolated))
            Settings.setPrivacyMode(isolated, true)
            Settings.setPrivacyMode(isolated, false)
            assertFalse("Disabling privacy must not restore external access", Settings.fileAccess(isolated))
        } finally {
            prefs.edit(commit = true) { clear() }
        }
    }

    @Test
    fun coldAndRecreatedPagesStayHiddenUntilAuthentication() {
        val prefs = context.getSharedPreferences("settings", Context.MODE_PRIVATE)
        val privacy = Settings.privacyMode(context)
        val files = prefs.getBoolean("file_access", false)
        Settings.setPrivacyMode(context, true)
        try {
            ActivityScenario.launch<LicensesActivity>(Intent(context, LicensesActivity::class.java)).use { scenario ->
                fun assertLocked() = scenario.onActivity { activity ->
                    assertTrue(Privacy.locked(activity))
                    assertEquals(View.INVISIBLE, activity.findViewById<View>(android.R.id.content).visibility)
                    assertTrue(activity.window.attributes.flags and FLAG_SECURE != 0)
                    val dialog = com.google.android.material.dialog.MaterialAlertDialogBuilder(activity)
                        .setMessage("Private preview").create()
                    Privacy.protect(dialog)
                    assertTrue(dialog.window!!.attributes.flags and FLAG_SECURE != 0)
                }
                assertLocked()
                scenario.recreate()
                assertLocked()
                scenario.moveToState(Lifecycle.State.CREATED)
                scenario.moveToState(Lifecycle.State.RESUMED)
                assertLocked()
            }
        } finally {
            prefs.edit(commit = true) {
                putBoolean("privacy_mode", privacy)
                putBoolean("file_access", files)
            }
        }
    }

    @Test
    fun deferringTheReturnPromptDoesNotUnlockThePage() {
        val prefs = context.getSharedPreferences("settings", Context.MODE_PRIVATE)
        val privacy = Settings.privacyMode(context)
        prefs.edit(commit = true) { putBoolean("privacy_mode", false) }
        try {
            ActivityScenario.launch<LicensesActivity>(Intent(context, LicensesActivity::class.java)).use { scenario ->
                scenario.onActivity { prefs.edit(commit = true) { putBoolean("privacy_mode", true) } }
                scenario.moveToState(Lifecycle.State.CREATED)
                scenario.onActivity { Privacy.deferNextUnlock(it) }
                scenario.moveToState(Lifecycle.State.RESUMED)
                scenario.onActivity { activity ->
                    assertTrue(Privacy.locked(activity))
                    assertEquals(View.INVISIBLE, activity.findViewById<View>(android.R.id.content).visibility)
                    assertTrue(activity.window.attributes.flags and FLAG_SECURE != 0)
                    assertFalse(Privacy.canExport(activity, Privacy.ExportAuthorization(-1, true)))
                }
            }
        } finally {
            prefs.edit(commit = true) { putBoolean("privacy_mode", privacy) }
        }
    }

    @Test
    fun pausedPagesStayVisibleUntilTheLastPageStops() {
        val prefs = context.getSharedPreferences("settings", Context.MODE_PRIVATE)
        val privacy = Settings.privacyMode(context)
        prefs.edit(commit = true) { putBoolean("privacy_mode", false) }
        try {
            ActivityScenario.launch<LicensesActivity>(Intent(context, LicensesActivity::class.java)).use { scenario ->
                scenario.onActivity { activity ->
                    assertEquals(View.VISIBLE, activity.findViewById<View>(android.R.id.content).visibility)
                    // Arm the lifecycle checks without changing the process's authentication state.
                    prefs.edit(commit = true) { putBoolean("privacy_mode", true) }
                }
                scenario.moveToState(Lifecycle.State.STARTED)
                scenario.onActivity { activity ->
                    assertTrue("Paused content must still reject protected operations", Privacy.locked(activity))
                    assertEquals(View.VISIBLE, activity.findViewById<View>(android.R.id.content).visibility)
                }
                scenario.moveToState(Lifecycle.State.CREATED)
                scenario.onActivity { activity ->
                    assertEquals(View.INVISIBLE, activity.findViewById<View>(android.R.id.content).visibility)
                }
                scenario.moveToState(Lifecycle.State.RESUMED)
                scenario.onActivity { activity ->
                    assertTrue(Privacy.locked(activity))
                    assertEquals(View.INVISIBLE, activity.findViewById<View>(android.R.id.content).visibility)
                }
            }
        } finally {
            prefs.edit(commit = true) { putBoolean("privacy_mode", privacy) }
        }
    }

    @Test
    fun notificationActionsCannotBeLaunchedByOtherApps() {
        @Suppress("DEPRECATION")
        val info = context.packageManager.getActivityInfo(ComponentName(context, NotificationActivity::class.java), 0)
        assertFalse(info.exported)
        @Suppress("DEPRECATION")
        val receiver = context.packageManager.getReceiverInfo(ComponentName(context, Notifications.ReportReceiver::class.java), 0)
        assertFalse(receiver.exported)
    }

    @Test
    fun onlyPrivateReportActionsOpenAnActivity() {
        val prefs = context.getSharedPreferences("settings", Context.MODE_PRIVATE)
        val privacy = Settings.privacyMode(context)
        try {
            for (enabled in listOf(false, true)) {
                prefs.edit(commit = true) { putBoolean("privacy_mode", enabled) }
                for ((index, action) in listOf("EXIT_ALL", "CANCEL_SWITCH", "TOGGLE_KEEP_AWAKE").withIndex()) {
                    val pending = Notifications.serviceAction(context, 900 + index, "io.github.sajadxdp0.tuxport.$action")
                    try {
                        assertTrue("Control actions must stay in the service", pending.isService)
                    } finally {
                        pending.cancel()
                    }
                }
                val copy = Notifications.copyReportAction(context, 903, "Private report").actionIntent
                try {
                    assertEquals(enabled, copy.isActivity)
                    assertEquals(!enabled, copy.isBroadcast)
                } finally {
                    copy.cancel()
                }
            }
        } finally {
            prefs.edit(commit = true) { putBoolean("privacy_mode", privacy) }
        }
    }

    @Test
    fun staleReportActionsCannotAccessTheClipboardWhileLocked() {
        val prefs = context.getSharedPreferences("settings", Context.MODE_PRIVATE)
        val privacy = Settings.privacyMode(context)
        val guarded = object : ContextWrapper(context) {
            override fun getSystemService(name: String): Any? {
                check(name != Context.CLIPBOARD_SERVICE) { "Locked reports must not access the clipboard" }
                return super.getSystemService(name)
            }
        }
        try {
            prefs.edit(commit = true) { putBoolean("privacy_mode", true) }
            InstrumentationRegistry.getInstrumentation().runOnMainSync {
                assertTrue(Privacy.locked(guarded))
                val report = Intent(Notifications.ACTION_COPY_REPORT).putExtra("report", "Private report")
                Notifications.ReportReceiver().onReceive(guarded, report)
                // Older releases used a service PendingIntent; its handler shares this check.
                Notifications.copyReport(guarded, report)
            }
        } finally {
            prefs.edit(commit = true) { putBoolean("privacy_mode", privacy) }
        }
    }
}
