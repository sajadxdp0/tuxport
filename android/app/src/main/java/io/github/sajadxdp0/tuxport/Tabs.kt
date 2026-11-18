package io.github.sajadxdp0.tuxport

import android.content.Context
import android.os.Process
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.IOException
import java.security.MessageDigest
import java.util.zip.Deflater
import java.util.zip.DeflaterOutputStream
import java.util.zip.InflaterInputStream

/**
 * Restoring tabs after the app was killed (the android/service map). While the kernel runs a
 * "running" marker naming the process is kept, and the tab layout and each tab's login account,
 * current directory, transcript and terminal snapshot are saved periodically in `files/tabs/`. A
 * normal exit deletes them together with the marker; if the marker is still there the next time the
 * service starts, the system killed the app last time (or it crashed), and the tabs are restored
 * from what was saved: each tab starts a new shell in its original directory below its terminal as
 * it was (from the snapshot, else the plain transcript) and a line of notice, which says why that
 * process ended as far as Android recorded it. Processes are not restored.
 */
internal object Tabs {
    private const val TAG = "Tabs"

    /**
     * The most characters saved per transcript (the last part is kept): a 2000-line transcript
     * usually fits.
     */
    internal const val MAX_TRANSCRIPT = 256 * 1024

    /**
     * The most bytes a snapshot inflates to when read (vt.c keeps none larger than 16 MiB); a file
     * that gives more is not one this app wrote.
     */
    private const val MAX_SNAPSHOT = 16 shl 20

    /**
     * A tab: the login account ([ConsolePty]'s `login`), the foreground process's current directory
     * (null when unavailable), the transcript, and the file of its terminal's snapshot
     * ([writeSnapshot]; null when there is none, or it is from another libghostty-vt).
     */
    data class Tab(val login: String, val cwd: String?, val transcript: String, val snapshot: String? = null)

    /** The saved tabs, which one is current, and when they were saved (ms). */
    data class Saved(val tabs: List<Tab>, val current: Int, val time: Long)

    /**
     * A run that did not exit normally: the pid it ran as (0 when the marker does not say, as one
     * left by an earlier version does not) and its saved tabs (null when there are none or they
     * cannot be read).
     */
    data class Killed(val pid: Int, val saved: Saved?)

    private fun dir(context: Context) = File(context.filesDir, "tabs")

    private fun marker(context: Context) = File(dir(context), "running")

    /**
     * The hash of the index contents last written, null with none: an unchanged index is not
     * written again (saved every minute, so as not to keep writing flash; transcripts are skipped
     * by their names).
     */
    private var writtenLayout: Int? = null

    /**
     * Called once when the service starts: returns the killed run when the last run did not exit
     * normally (the marker is still there), otherwise deletes the old tabs and returns null. The
     * marker and the tabs stay until they are replaced by the restored run's own marker and saves or
     * cleared by a normal exit: killed again before the kernel is up, the next start still finds
     * them.
     */
    fun takeKilled(context: Context): Killed? {
        val marker = marker(context)
        if (!marker.exists()) {
            clear(context)
            return null
        }
        val pid = runCatching { marker.readText().trim().toInt() }.getOrDefault(0)
        val saved = runCatching { load(context) }
            .onFailure { Log.w(TAG, "cannot read the saved tabs", it) }
            .getOrNull()
        return Killed(pid, saved)
    }

    private fun load(context: Context): Saved? {
        val index = File(dir(context), "index.json").takeIf { it.isFile } ?: return null
        val json = JSONObject(index.readText())
        val list = json.getJSONArray("tabs")
        // Snapshots written by another libghostty-vt (an update of the app) may not decode, or not
        // as they were written: those tabs come back from their transcripts
        val readable = json.optString("vt") == version
        val tabs = (0 until list.length()).map { i ->
            val t = list.getJSONObject(i)
            val transcript = File(dir(context), t.getString("transcript")).takeIf { it.isFile }?.readText().orEmpty()
            val snapshot = t.optString("snapshot").takeIf { readable && it.isNotEmpty() && File(dir(context), it).isFile }
            // A "name" key, from versions that could rename tabs, is ignored
            Tab(t.getString("login"), t.optString("cwd").takeIf { it.isNotEmpty() }, transcript, snapshot)
        }
        return Saved(tabs, json.optInt("current"), json.optLong("time"))
    }

    /** The version snapshots are written with ([VtNative.version]), recorded in the index. */
    private val version: String by lazy { VtNative.version().orEmpty() }

    /**
     * The snapshot a [Tab] names, inflated, for [VtNative.restore]; null when it cannot be read.
     * Called on the main thread when the tab's shell starts (a few milliseconds).
     */
    fun readSnapshot(context: Context, name: String): ByteArray? = try {
        InflaterInputStream(File(dir(context), name).inputStream()).use { input ->
            val out = ByteArrayOutputStream()
            val buf = ByteArray(64 * 1024)
            while (true) {
                val n = input.read(buf)
                if (n < 0) break
                out.write(buf, 0, n)
                if (out.size() > MAX_SNAPSHOT) throw IOException("$name is larger than a snapshot")
            }
            out.toByteArray()
        }
    } catch (e: IOException) {
        Log.w(TAG, "cannot read a tab's snapshot", e)
        null
    }

    /**
     * The kernel is up: leave the "running" marker with this process's pid, by which the next run
     * finds how this one ended.
     */
    fun markRunning(context: Context) {
        runCatching {
            dir(context).mkdirs()
            marker(context).writeText(Process.myPid().toString())
        }.onFailure { Log.w(TAG, "cannot mark the tabs as running", it) }
    }

    /**
     * Writes a terminal's [snapshot] ([VtNative.snapshot], called on a background thread) as a
     * file named by its content, deflated (a full scrollback of coloured output, about 1.5 MB, goes
     * to a tenth of that); returns its name for a [Tab], null when it cannot be written. The file
     * stays while an index names it ([save]).
     */
    fun writeSnapshot(context: Context, snapshot: ByteArray): String? = synchronized(this) {
        try {
            val dir = dir(context).apply { mkdirs() }
            val name = "s-" + digest(snapshot) + ".snap"
            val file = File(dir, name)
            if (!file.isFile) {
                val tmp = File(file.path + ".tmp")
                val deflater = Deflater(Deflater.BEST_SPEED)
                try {
                    DeflaterOutputStream(tmp.outputStream(), deflater).use { it.write(snapshot) }
                } finally {
                    deflater.end()
                }
                if (!tmp.renameTo(file)) throw IOException("cannot move ${tmp.name} into place")
            }
            name
        } catch (e: IOException) {
            Log.w(TAG, "cannot save a tab's snapshot", e)
            null
        }
    }

    /**
     * Saves the tabs (called on a background thread). Each transcript is a file named by its
     * content, written to a temporary file and then renamed, as their snapshots were
     * ([writeSnapshot]); the index naming them is replaced last, and only then are transcripts and
     * snapshots no index names any more deleted. A kill at any point leaves a complete index whose
     * files all exist and belong to its own tabs.
     */
    fun save(context: Context, tabs: List<Tab>, current: Int) = synchronized(this) {
        try {
            val dir = dir(context).apply { mkdirs() }
            val entries = JSONArray()
            val names = HashSet<String>()
            tabs.forEach { tab ->
                val text = tab.transcript.takeLast(MAX_TRANSCRIPT)
                val name = transcriptName(text)
                // Unchanged since the last save: the file with this content is already there
                if (name !in names && !File(dir, name).isFile) replace(File(dir, name), text)
                names.add(name)
                val entry = JSONObject().put("login", tab.login).put("cwd", tab.cwd ?: "").put("transcript", name)
                tab.snapshot?.let {
                    entry.put("snapshot", it)
                    names.add(it)
                }
                entries.put(entry)
            }
            // The index carries the save time: not rewritten when the transcripts, the layout and
            // the current directories are all unchanged
            val layout = (entries.toString() + current).hashCode()
            val index = File(dir, "index.json")
            if (writtenLayout != layout || !index.isFile) {
                val json = JSONObject()
                    .put("time", System.currentTimeMillis())
                    .put("current", current)
                    .put("vt", version)
                    .put("tabs", entries)
                replace(index, json.toString())
                writtenLayout = layout
            }
            // Transcripts and snapshots of closed tabs and old contents, and leftovers of a save
            // that was killed (including the t<i>.txt names of earlier versions)
            dir.listFiles { f -> (f.name.matches(TRANSCRIPT) || f.name.matches(SNAPSHOT)) && f.name !in names }
                ?.forEach { it.delete() }
        } catch (e: IOException) {
            Log.w(TAG, "cannot save the tabs", e)
        }
    }

    /** Transcript files (current and earlier names) and their temporary files. */
    private val TRANSCRIPT = Regex("t[-0-9a-f]+\\.txt(\\.tmp)?")

    /** Snapshot files and their temporary files. */
    private val SNAPSHOT = Regex("s-[0-9a-f]+\\.snap(\\.tmp)?")

    /** The file name of a transcript: a digest of [text], so equal content keeps its file. */
    private fun transcriptName(text: String) = "t-" + digest(text.toByteArray()) + ".txt"

    /** The first 128 bits of [data]'s SHA-256, in hex. */
    private fun digest(data: ByteArray): String =
        MessageDigest.getInstance("SHA-256").digest(data).take(16).joinToString("") { "%02x".format(it) }

    private fun replace(file: File, text: String) {
        val tmp = File(file.path + ".tmp")
        tmp.writeText(text)
        if (!tmp.renameTo(file)) throw IOException("cannot move ${tmp.name} into place")
    }

    /** Normal exit: delete the marker and the saved tabs. */
    fun clear(context: Context) {
        synchronized(this) {
            writtenLayout = null
            dir(context).deleteRecursively()
        }
    }
}
