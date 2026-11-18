package io.github.sajadxdp0.tuxport

import android.app.ActivityManager
import android.app.Application
import android.app.ApplicationExitInfo
import android.content.ClipData
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Process
import android.system.Os
import android.system.OsConstants
import android.util.Log
import androidx.core.content.FileProvider
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.time.Instant
import java.util.zip.ZipEntry
import java.util.zip.ZipOutputStream
import kotlin.concurrent.thread

/** Bounded app-only diagnostics, independent of the service worker and the guest kernel lock. */
internal object Diagnostics {
    private const val TAG = "TuxportDiagnostics"
    private const val PART_BYTES = 256 * 1024
    private const val REPORT_BYTES = 64 * 1024
    private const val TRACE_BYTES = 1024 * 1024
    @Volatile private var files: DiagnosticFiles? = null
    @Volatile private var captureStatus = "Log capture has not started."

    fun install(app: Application) {
        // Save authorization is process-local; discard archives abandoned by a dead process.
        File(app.cacheDir, "diagnostic-saves").listFiles()?.forEach {
            if (!it.delete()) Log.w(TAG, "cannot remove abandoned diagnostic save")
        }
        try {
            files = DiagnosticFiles(File(app.noBackupFilesDir, "diagnostics"), PART_BYTES).also { it.startRun() }
        } catch (e: IOException) {
            captureStatus = "Cannot retain logs: ${e.javaClass.simpleName}"
            Log.w(TAG, "cannot initialize diagnostic storage", e)
            return
        }
        record("Process ${Process.myPid()} started at ${Instant.now()}\n${environment(app)}")
        val previous = Thread.getDefaultUncaughtExceptionHandler()
        if (previous != null) {
            Thread.setDefaultUncaughtExceptionHandler { t, error ->
                // Persist before Android's handler kills the process, then preserve its behavior.
                try {
                    record("Uncaught exception on ${t.name}:\n${Log.getStackTraceString(error).take(REPORT_BYTES)}")
                } finally {
                    previous.uncaughtException(t, error)
                }
            }
        }
        thread(name = "tuxport-diagnostics", isDaemon = true) {
            var reader: java.lang.Process? = null
            try {
                reader = ProcessBuilder(
                    "logcat", "--pid=${Process.myPid()}", "-b", "main", "-b", "crash",
                    "-v", "threadtime", "-T", "1", "tuxport:I", "Rootfs:I", "TerminalService:I", "*:W",
                ).redirectErrorStream(true).start()
                captureStatus = "App process logs: warnings/errors, native diagnostics and setup events."
                reader.inputStream.use { input ->
                    val buffer = ByteArray(8192)
                    while (true) {
                        val count = input.read(buffer)
                        if (count < 0) break
                        files?.append("logcat", buffer.copyOf(count))
                    }
                }
                captureStatus = "Log capture stopped (exit ${reader.waitFor()}); retained logs follow."
            } catch (e: Exception) {
                captureStatus = "Log capture unavailable: ${e.javaClass.simpleName}; retained reports follow."
            } finally {
                reader?.destroy()
            }
        }
    }

    /** Reports survive notification dismissal and process restarts, even if logcat is unavailable. */
    fun record(report: String) {
        try {
            files?.append("reports", "\n--- ${Instant.now()} ---\n${report.take(REPORT_BYTES)}\n".toByteArray())
        } catch (e: IOException) {
            Log.w(TAG, "cannot retain diagnostic report", e)
        }
    }

    fun environment(context: Context): String = buildString {
        appendLine("Tuxport ${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE}, ${BuildConfig.BUILD_TYPE})")
        appendLine("Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT}), ${Build.MANUFACTURER} ${Build.MODEL}, " +
            "${Build.SUPPORTED_ABIS.firstOrNull()}, page size ${Os.sysconf(OsConstants._SC_PAGESIZE)}")
        appendLine("${Rootfs.installed(context)?.label ?: "unknown distribution"}, Tier ${Settings.tier(context)}")
    }

    /** Called off the main thread, only after the settings page has authorized this export. */
    @Synchronized
    fun export(context: Context, forSave: Boolean = false): File {
        val directory = File(context.cacheDir, if (forSave) "diagnostic-saves" else "diagnostic-exports")
        if (!directory.isDirectory && !directory.mkdirs()) throw IOException("Cannot create export directory")
        // Keep a few completed shares alive for receivers; never expose the retention directory.
        // Pending saves are owned by their picker/transfer and must not be pruned by other exports.
        if (!forSave) {
            directory.listFiles()?.sortedByDescending { it.lastModified() }?.drop(2)?.forEach {
                if (!it.delete()) throw IOException("Cannot remove old diagnostic export")
            }
        }
        val archive = File.createTempFile("tuxport-diagnostics-", ".zip", directory)
        try {
            ZipOutputStream(archive.outputStream().buffered()).use { zip ->
                zip.entry("summary.txt", buildString {
                    append(environment(context))
                    appendLine("Exported at ${Instant.now()}")
                    appendLine(captureStatus)
                    appendLine("Current and previous app runs; oldest bytes are discarded when the size limit is reached.")
                    appendLine("No terminal transcript, command history, environment dump or guest files are collected.")
                    appendLine("Diagnostics can contain process names, paths and error messages. Review before sharing.")
                    appendLine("A sudden process death may lose the final logcat messages.")
                }.toByteArray())
                files?.snapshot()?.forEach { (name, bytes) -> zip.entry(name, bytes) }
                zip.writeExitInfo(context)
            }
            return archive
        } catch (e: Exception) {
            archive.delete()
            throw e
        }
    }

    private fun ZipOutputStream.writeExitInfo(context: Context) {
        val exits = runCatching {
            context.getSystemService(ActivityManager::class.java)
                .getHistoricalProcessExitReasons(context.packageName, 0, 5)
        }
        entry("process-exits.txt", buildString {
            exits.fold(onSuccess = { records ->
                if (records.isEmpty()) appendLine("No historical process exit records available.")
                records.forEach { exit ->
                    appendLine("${Instant.ofEpochMilli(exit.timestamp)} pid=${exit.pid} reason=${exit.reason} status=${exit.status}")
                    appendLine(exit.description.orEmpty())
                }
            }, onFailure = { appendLine("Exit records unavailable: ${it.javaClass.simpleName}") })
        }.toByteArray())
        exits.getOrNull()?.firstOrNull {
            it.reason == ApplicationExitInfo.REASON_ANR || it.reason == ApplicationExitInfo.REASON_CRASH_NATIVE
        }?.let { writeTrace(it) }
    }

    private fun ZipOutputStream.writeTrace(exit: ApplicationExitInfo) {
        val trace = runCatching { exit.traceInputStream?.use { it.readLimited(TRACE_BYTES + 1) } }
        val bytes = trace.getOrNull()
        val status = when {
            trace.isFailure -> "System trace unavailable: ${trace.exceptionOrNull()?.javaClass?.simpleName}"
            bytes == null -> "System trace unavailable or expired."
            bytes.size > TRACE_BYTES -> "System trace omitted: exceeds $TRACE_BYTES bytes."
            else -> {
                // API 31 adds protobuf native tombstones. API 30 may retain an earlier ANR's text.
                val native = Build.VERSION.SDK_INT >= 31 && exit.reason == ApplicationExitInfo.REASON_CRASH_NATIVE
                entry(if (native) "native-tombstone.pb" else "anr-trace.txt", bytes)
                "System trace for pid ${exit.pid} at ${Instant.ofEpochMilli(exit.timestamp)}."
            }
        }
        entry("trace-status.txt", status.toByteArray())
    }

    private fun ZipOutputStream.entry(name: String, bytes: ByteArray) {
        putNextEntry(ZipEntry(name))
        write(bytes)
        closeEntry()
    }

    fun shareIntent(context: Context, archive: File): Intent {
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.diagnostics", archive)
        return Intent(Intent.ACTION_SEND).apply {
            type = "application/zip"
            putExtra(Intent.EXTRA_STREAM, uri)
            clipData = ClipData.newRawUri("Tuxport diagnostics", uri)
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
    }
}

/** Two rolling parts per stream, with one previous run. All reads/writes use the same lock. */
internal class DiagnosticFiles(private val directory: File, private val limit: Int) {
    private val streams = listOf("logcat", "reports")
    private fun file(run: String, stream: String, part: Int) = File(directory, "$run-$stream-$part.txt")

    @Synchronized
    fun startRun() {
        if (!directory.isDirectory && !directory.mkdirs()) throw IOException("Cannot create diagnostic directory")
        for (stream in streams) for (part in 0..1) {
            val previous = file("previous", stream, part)
            if (previous.exists() && !previous.delete()) throw IOException("Cannot remove old diagnostic log")
            val current = file("current", stream, part)
            if (current.exists() && !current.renameTo(previous)) throw IOException("Cannot retain previous diagnostic log")
        }
    }

    @Synchronized
    fun append(stream: String, bytes: ByteArray) {
        require(stream in streams)
        val current = file("current", stream, 0)
        val tail = if (bytes.size > limit) bytes.copyOfRange(bytes.size - limit, bytes.size) else bytes
        if (current.length() + tail.size > limit) {
            val older = file("current", stream, 1)
            if (older.exists() && !older.delete()) throw IOException("Cannot rotate diagnostic log")
            if (current.exists() && !current.renameTo(older)) throw IOException("Cannot rotate diagnostic log")
        }
        current.appendBytes(tail)
    }

    @Synchronized
    fun snapshot(): Map<String, ByteArray> = buildMap {
        for (run in listOf("previous", "current")) for (stream in streams) {
            val parts = listOf(1, 0).map { file(run, stream, it) }.filter { it.isFile }
            if (parts.isNotEmpty()) {
                put("$run-$stream.txt", parts.fold(ByteArray(0)) { result, part ->
                    result + part.inputStream().use { it.readLimited(limit) }
                })
            }
        }
    }
}

private fun InputStream.readLimited(limit: Int): ByteArray {
    val bytes = ByteArray(limit)
    var count = 0
    while (count < limit) {
        val read = read(bytes, count, limit - count)
        if (read < 0) break
        count += read
    }
    return bytes.copyOf(count)
}

/** Only generated archives are grantable; retained logs and the guest filesystem are not roots. */
class DiagnosticProvider : FileProvider()
