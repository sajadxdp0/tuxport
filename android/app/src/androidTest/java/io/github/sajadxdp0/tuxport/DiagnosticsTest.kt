package io.github.sajadxdp0.tuxport

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.os.Process
import android.os.SystemClock
import android.util.Log
import androidx.core.content.FileProvider
import androidx.core.content.edit
import androidx.lifecycle.ViewModelProvider
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.util.UUID
import java.util.zip.ZipFile

@RunWith(AndroidJUnit4::class)
class DiagnosticsTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context = instrumentation.targetContext

    @Test
    fun pendingSaveSurvivesRecreationButCanOnlyBeConsumedOnce() {
        val prefs = context.getSharedPreferences("settings", Context.MODE_PRIVATE)
        val privacy = Settings.privacyMode(context)
        val archive = File.createTempFile("pending-save-", ".zip", context.cacheDir)
        val abandoned = File.createTempFile("abandoned-save-", ".zip", context.cacheDir)
        prefs.edit(commit = true) { putBoolean("privacy_mode", false) }
        try {
            ActivityScenario.launch<SettingsActivity>(Intent(context, SettingsActivity::class.java)).use { scenario ->
                fun state(activity: SettingsActivity): PendingLogSave {
                    val fragment = activity.supportFragmentManager.findFragmentById(R.id.content)!!
                    return ViewModelProvider(fragment)[PendingLogSave::class.java]
                }
                scenario.onActivity { activity ->
                    state(activity).apply {
                        saveRequested = true
                        stage(archive)
                    }
                }
                scenario.recreate()
                scenario.onActivity { activity ->
                    state(activity).apply {
                        assertTrue(saveRequested)
                        assertTrue(pending)
                        assertEquals(archive, take())
                        assertFalse(pending)
                        assertEquals(null, take())
                        assertTrue(archive.exists())
                        // Cancellation consumes and deletes exactly the prepared archive.
                        archive.delete()
                        stage(abandoned)
                    }
                }
            }
            assertFalse(archive.exists())
            assertFalse("Finishing the owner must discard a pending archive", abandoned.exists())
        } finally {
            archive.delete()
            abandoned.delete()
            prefs.edit(commit = true) { putBoolean("privacy_mode", privacy) }
        }
    }

    @Test
    fun laterSharesCannotPruneOrExposeAPendingSave() {
        val pending = Diagnostics.export(context, forSave = true)
        val shares = mutableListOf<File>()
        try {
            assertTrue(pending.setLastModified(1))
            repeat(4) { shares.add(Diagnostics.export(context)) }
            assertTrue("A pending save must outlive share-cache pruning", pending.exists())
            assertEquals(3, shares.count { it.exists() })
            ZipFile(pending).use { assertTrue(it.getEntry("summary.txt") != null) }
            try {
                FileProvider.getUriForFile(context, "${context.packageName}.diagnostics", pending)
                fail("Pending save files must not be exposed by the sharing provider")
            } catch (_: IllegalArgumentException) {
                // Expected: the save directory has no FileProvider root.
            }
        } finally {
            pending.delete()
            shares.forEach { it.delete() }
        }
    }

    @Test
    fun retentionKeepsRecentBytesAndOnlyOnePreviousRun() {
        val directory = File(context.cacheDir, "diagnostic-retention-${UUID.randomUUID()}")
        try {
            val logs = DiagnosticFiles(directory, 16)
            logs.startRun()
            logs.append("logcat", "oldest-record--\n".toByteArray())
            logs.append("logcat", "newer-record---\n".toByteArray())
            assertEquals("oldest-record--\nnewer-record---\n", logs.snapshot().getValue("current-logcat.txt").decodeToString())
            logs.append("logcat", "latest-record--\n".toByteArray())
            assertEquals("newer-record---\nlatest-record--\n", logs.snapshot().getValue("current-logcat.txt").decodeToString())
            logs.append("reports", "x".repeat(1000).toByteArray())
            assertEquals(16, logs.snapshot().getValue("current-reports.txt").size)
            // Reopen as a new app process, retaining the previous run on disk.
            val next = DiagnosticFiles(directory, 16)
            next.startRun()
            assertEquals("newer-record---\nlatest-record--\n", next.snapshot().getValue("previous-logcat.txt").decodeToString())
            assertFalse(next.snapshot().containsKey("current-logcat.txt"))
            next.append("logcat", "third run".toByteArray())
            next.startRun()
            assertEquals("third run", next.snapshot().getValue("previous-logcat.txt").decodeToString())
            assertFalse(next.snapshot().containsKey("previous-reports.txt"))
            assertTrue(directory.listFiles()!!.all { it.length() <= 16 })
            assertTrue(directory.listFiles()!!.size <= 8)
        } finally {
            directory.deleteRecursively()
        }
    }

    @Test
    fun archiveContainsReportsAndOwnLogsWithoutGuestFiles() {
        val marker = "diagnostic-test-${UUID.randomUUID()}"
        val unrelated = "other-process-${UUID.randomUUID()}"
        Diagnostics.record(marker)
        Log.w("DiagnosticTest", "own process ${Process.myPid()} $marker")
        // A shell process writes the same priority/tag, but its PID must not enter the archive.
        instrumentation.uiAutomation.executeShellCommand("log -p w -t DiagnosticTest $unrelated").close()
        var captured = false
        val deadline = SystemClock.uptimeMillis() + 5000
        while (!captured && SystemClock.uptimeMillis() < deadline) {
            val archive = Diagnostics.export(context)
            try {
                ZipFile(archive).use { zip ->
                    val names = zip.entries().asSequence().map { it.name }.toSet()
                    assertTrue(names.containsAll(listOf("summary.txt", "process-exits.txt", "current-reports.txt")))
                    assertTrue(names.all { it in setOf(
                        "summary.txt", "process-exits.txt", "current-reports.txt", "previous-reports.txt",
                        "current-logcat.txt", "previous-logcat.txt", "trace-status.txt", "native-tombstone.pb", "anr-trace.txt",
                    ) })
                    val reports = zip.getInputStream(zip.getEntry("current-reports.txt")).bufferedReader().readText()
                    assertTrue(reports.contains(marker))
                    val logs = zip.getEntry("current-logcat.txt")?.let { zip.getInputStream(it).bufferedReader().readText() }.orEmpty()
                    assertFalse(logs.contains(unrelated))
                    captured = logs.contains(marker)
                    assertTrue("Archive must stay bounded", archive.length() < 4 * 1024 * 1024)
                }
            } finally {
                archive.delete()
            }
            if (!captured) SystemClock.sleep(50)
        }
        assertTrue("The live log reader must capture this process's warning", captured)
    }

    @Test
    fun shareGrantsOnlyReadAccessToGeneratedArchives() {
        val archive = Diagnostics.export(context)
        try {
            val send = Diagnostics.shareIntent(context, archive)
            assertEquals(Intent.ACTION_SEND, send.action)
            assertEquals("application/zip", send.type)
            assertTrue(send.flags and Intent.FLAG_GRANT_READ_URI_PERMISSION != 0)
            assertEquals(0, send.flags and Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
            val uri = send.clipData!!.getItemAt(0).uri
            context.contentResolver.openInputStream(uri)!!.use { input ->
                assertEquals('P'.code, input.read())
                assertEquals('K'.code, input.read())
            }
            @Suppress("DEPRECATION")
            val provider = context.packageManager.getProviderInfo(ComponentName(context, DiagnosticProvider::class.java), 0)
            assertFalse(provider.exported)
            for (outside in listOf(
                File(context.filesDir, "rootfs/meta.db"),
                File(context.noBackupFilesDir, "diagnostics/current-reports-0.txt"),
                File(context.cacheDir, "diagnostic-exports/../private.txt"),
            )) {
                try {
                    FileProvider.getUriForFile(context, "${context.packageName}.diagnostics", outside)
                    fail("A non-export file must not be grantable: $outside")
                } catch (_: IllegalArgumentException) {
                    // Expected: the provider exposes only its dedicated archive directory.
                }
            }
        } finally {
            archive.delete()
        }
    }

    @Test
    fun lockedAndStaleCallbacksCannotStartAnExport() {
        val prefs = context.getSharedPreferences("settings", Context.MODE_PRIVATE)
        val privacy = Settings.privacyMode(context)
        val directory = File(context.cacheDir, "diagnostic-exports")
        prefs.edit(commit = true) { putBoolean("privacy_mode", true) }
        try {
            ActivityScenario.launch<SettingsActivity>(Intent(context, SettingsActivity::class.java)).use { scenario ->
                val before = directory.listFiles()?.map { it.name }?.toSet().orEmpty()
                scenario.onActivity { activity ->
                    assertTrue(Privacy.locked(activity))
                    val stale = Privacy.ExportAuthorization(-1, true)
                    assertFalse(Privacy.canExport(activity, stale))
                    activity.onLogExportAuthorized(stale)
                    activity.onPrivacyUnlocked()
                }
                instrumentation.waitForIdleSync()
                assertEquals(before, directory.listFiles()?.map { it.name }?.toSet().orEmpty())
                scenario.recreate()
                scenario.onActivity { activity ->
                    assertTrue(Privacy.locked(activity))
                    assertFalse(Privacy.canExport(activity, Privacy.ExportAuthorization(-1, true)))
                }
                assertEquals(before, directory.listFiles()?.map { it.name }?.toSet().orEmpty())
            }
        } finally {
            prefs.edit(commit = true) { putBoolean("privacy_mode", privacy) }
        }
    }
}
