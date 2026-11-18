package io.github.sajadxdp0.tuxport

import android.content.Context
import android.os.Handler
import android.os.Looper
import android.util.Base64
import android.util.Log
import io.github.sajadxdp0.tuxport.VtSession.Companion.MAX_STATUS_RECORDS
import io.github.sajadxdp0.tuxport.VtSession.Companion.PROGRESS_TIMEOUT_MS
import java.io.IOException
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

/**
 * A terminal tab (the android/vt map), owned by [TerminalService]: a login shell on a kernel
 * console ([ConsolePty]) and its libghostty-vt terminal. The terminal lives in the native library
 * and takes the console's output on a pump thread whether or not a [VtView] shows it ([view]); the
 * shell starts when a view first knows the grid ([start]), and until then a restored tab's saved
 * terminal and messages wait. Input goes out on the tab's own writer thread, so a program that
 * stops reading blocks only its tab. Calls are on the main thread, except [console],
 * [resolvedLogin], [takeSnapshot] and [savedSnapshot], which saving uses on the service's worker.
 */
internal class VtSession(
    /** Names the tab in a notification's intent (from the service, never reused in a run). */
    val id: Int,
    private val pty: ConsolePty,
    private val machine: Long,
    private val events: Events,
) {

    /** What the service hears about (main thread). */
    interface Events {
        fun onTitleChanged(session: VtSession)

        /** A program rang the bell (BEL); several in one read of the console count once. */
        fun onBell(session: VtSession)

        /** A program wrote the clipboard (OSC 52): [text], or null to clear it. */
        fun onClipboard(session: VtSession, text: String?)

        /** A program asked for a notification (OSC 9 body, OSC 777 title and body). */
        fun onNotification(session: VtSession, title: String, body: String)

        /**
         * A program asked to read the clipboard (OSC 52); the answer goes through
         * [answerClipboardRead], which also answers the reads that come meanwhile.
         */
        fun onClipboardRead(session: VtSession)

        /** The tab's [progress] (OSC 9;4) or [status] (OSC 7501) changed. */
        fun onStatusChanged(session: VtSession)

        /**
         * The shell's prompt marks (OSC 133) saw a command finish after [millis] milliseconds, with
         * [exitCode] when the shell reported one.
         */
        fun onCommandFinished(session: VtSession, exitCode: Int?, millis: Long)

        /**
         * tmux's control mode (tmux -CC), which the terminal cannot show and detached as it
         * started, ended; its session goes on without the tab.
         */
        fun onTmuxDetached(session: VtSession)

        fun onFinished(session: VtSession)
    }

    /**
     * A progress report (OSC 9;4): [state] is one of the `PROGRESS_` constants (not
     * [PROGRESS_REMOVE]), [percent] 0–100, or -1 when the program gave none.
     */
    data class Progress(val state: Int, val percent: Int)

    /**
     * A program status record (OSC 7501): what a program says about itself ([id] empty) or one of
     * its parts. [state] is one of the `STATUS_` constants, [kind] what a blocked program needs
     * (the `KIND_` constants), [percent] 0–100 or -1; [title] and [message] are the program's text
     * for people, without text direction controls, so that shown outside the terminal (a
     * notification) it cannot pass for text from somewhere else.
     */
    data class Status(val id: String, val state: Int, val kind: Int, val percent: Int, val title: String, val message: String)

    /**
     * The kernel console, -1 before the shell starts; kept after the console is closed, when the
     * kernel may give the number to another tab's console ([consoleOpen]).
     */
    val console: Int get() = pty.console

    /** The console is open: started and not closed yet. */
    val consoleOpen: Boolean get() = pty.console >= 0 && !hungUp.get()

    /** The login account setting the tab was opened with ([ConsolePty.login]). */
    val login: String get() = pty.login

    /**
     * The account an `auto` login resolved to, found when the tab is first saved and kept, so a
     * restored tab logs in as the same user even if the regular accounts change (only the
     * service's worker thread uses it).
     */
    @Volatile
    var resolvedLogin: String? = null

    /**
     * The file of the terminal's last saved snapshot (`Tabs`), kept while the terminal does not
     * change ([takeSnapshot] gives nothing new); only the service's worker thread uses it.
     */
    @Volatile
    var savedSnapshot: String? = null

    /** The terminal ([VtNative]), 0 until [start] created it. */
    @Volatile
    var tab = 0L
        private set

    /** The view showing this tab, if any; read on the pump thread. */
    @Volatile
    var view: VtView? = null

    private val main = Handler(Looper.getMainLooper())
    private var started = false
    private var released = false
    private var exit: Int? = null
    private val hungUp = AtomicBoolean()
    private var writer: ExecutorService? = null

    /**
     * A restored tab's terminal as it was saved: its snapshot (read when the shell starts), else
     * its transcript (a terminal byte stream). Both are dropped once the terminal exists.
     */
    private var restoredSnapshot: (() -> ByteArray?)? = null
    private var restoredText: ByteArray? = null

    /** A restored tab's notice and messages to show before the shell's output. */
    private var pending: ByteArray? = null

    /**
     * The cursor's default shape ([VtNative.CURSOR_BLOCK], [VtNative.CURSOR_BAR] or
     * [VtNative.CURSOR_UNDERLINE]) and blinking, from Settings ([setCursor]).
     */
    private var cursorStyle = VtNative.CURSOR_BLOCK
    private var cursorBlink = true

    // The grid the terminal and the console have
    private var cols = 0
    private var rows = 0
    private var cellWidth = 0
    private var cellHeight = 0

    /**
     * The last transcript of the primary screen (as much as [transcriptText] returns), kept for
     * saves while a full-screen program runs or nothing changed the screen.
     */
    private var lastTranscript: String? = null

    /** The title a program set (OSC 0/2), null when none (the tab bar then shows `sh`). */
    val title: String? get() = if (tab == 0L) null else VtNative.title(tab)

    /**
     * The current directory of the console's foreground process group (the shell's at its
     * prompt), for a new tab to start in; null before the shell starts and once it has ended. Takes
     * the kernel's BKL on the calling thread, as opening a console does.
     */
    fun cwd(): String? = if (isRunning && consoleOpen) Native.consoleCwd(machine, pty.console) else null

    /** True until the shell has exited (also before it starts). */
    val isRunning: Boolean get() = exit == null

    /**
     * Closing the tab would end something the user may be waiting for: the shell runs and is not
     * at its prompt, as its prompt marks (OSC 133, sent from `/etc/profile.d/tuxport.sh`) tell, so a
     * command runs; always so for a shell without the marks. A shell not started yet (a restored
     * tab never shown) has nothing to end.
     */
    val busy: Boolean get() = isRunning && tab != 0L && !VtNative.atPrompt(tab)

    /**
     * The last progress report (OSC 9;4); null when there is none, it was removed, none came for
     * [PROGRESS_TIMEOUT_MS] (as Ghostty drops it, against programs that never remove it), the
     * program gave the terminal back ([onForeground]) or the shell exited.
     */
    var progress: Progress? = null
        private set

    /**
     * The program status record most in need of the user (OSC 7501), null when there is none:
     * blocked first, then error, done, working and idle, the most recently updated among equals.
     */
    var status: Status? = null
        private set

    /**
     * The program status records by id, the least recently updated first: the specification keeps
     * one per id, at most [MAX_STATUS_RECORDS], dropping the one updated longest ago.
     */
    private val records = LinkedHashMap<String, Status>()

    /** Status reports taken on the pump thread and not applied yet ([onStatus]). */
    private val reports = ArrayList<Status>()

    /**
     * Clipboard reads (OSC 52) waiting for the user's answer: the clipboard each named, `c`, `p`
     * or `s`, which the answer names too.
     */
    private val clipboardReads = ArrayList<Char>()

    private val dropProgress = Runnable { setProgress(null) }

    /**
     * Plain text of the primary screen and its scrollback for saving the tab, as much of its end as
     * saving keeps ([Tabs.MAX_TRANSCRIPT]); null while the shell has not started (a restored tab
     * not shown yet).
     */
    val transcriptText: String?
        get() {
            if (tab == 0L) return null
            VtNative.transcript(tab)?.let {
                // Kept for later saves: the rest of a full scrollback would sit in the heap unused
                lastTranscript = it.takeLast(Tabs.MAX_TRANSCRIPT)
                return lastTranscript
            }
            // Unchanged since the last one, which formatting again would only make the scrollback
            // decompress for; or the alternate screen (vim, less): what the tab showed before it,
            // not the program's screen without scrollback
            return lastTranscript.orEmpty()
        }

    /**
     * A snapshot of the whole terminal for saving the tab ([VtNative.snapshot]), on the service's
     * worker: null when nothing changed since the last one ([savedSnapshot] is still the
     * terminal's) or the shell has not started, empty when none can be taken.
     */
    fun takeSnapshot(): ByteArray? {
        val t = tab
        return if (t == 0L) null else VtNative.snapshot(t)
    }

    /**
     * The cursor's default [style] and whether it [blink]s, the settings' (the service gives them
     * to every tab): shown at once unless a program chose its own cursor, and given to the terminal
     * when it is made, a restored one included.
     */
    fun setCursor(style: Int, blink: Boolean) {
        cursorStyle = style
        cursorBlink = blink
        if (tab == 0L) return
        VtNative.setCursor(tab, style, blink)
        view?.invalidate()
    }

    /**
     * A restored tab: its terminal comes back from the [snapshot] the loader reads when the shell
     * starts, or, without one that decodes, shows [text] (a terminal byte stream, the transcript);
     * then the [notice] and the shell's output follow.
     */
    fun setRestored(text: ByteArray, snapshot: (() -> ByteArray?)?, notice: ByteArray) {
        restoredText = text
        restoredSnapshot = snapshot
        pending = notice
    }

    /**
     * Shows a yellow line of app notice above the cursor's line (a kill report); kept until the
     * terminal exists; false in a full-screen program, where nothing is shown.
     */
    fun showMessage(text: String): Boolean {
        if (tab == 0L) {
            if (started) return false
            pending = (pending ?: ByteArray(0)) + "\u001b[33m$text\u001b[0m\r\n".toByteArray()
            return true
        }
        val shown = VtNative.message(tab, text)
        if (shown) view?.invalidate()
        return shown
    }

    /**
     * Opens the console with a grid of [cols] × [rows], creates the terminal, shows what was
     * pending and starts the shell with its pump, writer and waiter threads. Only the first call
     * does anything.
     */
    fun start(cols: Int, rows: Int, cellWidth: Int, cellHeight: Int) {
        if (started || released || hungUp.get()) return
        started = true
        val error = try {
            pty.start(cols, rows)
            null
        } catch (e: IOException) {
            e.message ?: e.toString()
        }
        val t = try {
            createTerminal(cols, rows, cellWidth, cellHeight)
        } catch (e: IOException) {
            0L
        }
        if (t == 0L) {
            // The kernel is gone: the service is about to release the tab
            hangUp()
            exited(127)
            return
        }
        tab = t
        VtNative.setCursor(t, cursorStyle, cursorBlink)
        this.cols = cols
        this.rows = rows
        this.cellWidth = cellWidth
        this.cellHeight = cellHeight
        pending?.let { VtNative.print(t, it) }
        pending = null
        if (error != null) {
            // Show why and treat the tab as ended. The console opened before spawning failed is
            // closed here
            VtNative.print(t, "\r\n[$error]\r\n".toByteArray())
            hangUp()
            exit = 127
            view?.onStarted()
            events.onFinished(this)
            return
        }
        writer = Executors.newSingleThreadExecutor { Thread(it, "vt-writer") }
        val pump = thread(name = "vt-pump") { VtNative.pump(t, this) }
        thread(name = "vt-waiter") {
            val code = pty.waitFor()
            // Output from before the exit may not have been taken yet: wait a little for the pump
            // (a background job may keep the console open, so not without limit)
            pump.join(500)
            main.post { exited(code) }
        }
        view?.onStarted()
    }

    /**
     * The tab's terminal: a restored tab's from its snapshot when that decodes, else a new one,
     * which shows a restored tab's transcript. Throws when the kernel is gone.
     */
    private fun createTerminal(cols: Int, rows: Int, cellWidth: Int, cellHeight: Int): Long {
        val snapshot = restoredSnapshot?.invoke()
        val text = restoredText
        restoredSnapshot = null
        restoredText = null
        if (snapshot != null) {
            val t = VtNative.restore(machine, pty.console, snapshot, cols, rows, cellWidth, cellHeight)
            if (t != 0L) return t
            Log.w(TAG, "cannot restore a tab's snapshot; showing its transcript")
        }
        val t = VtNative.create(machine, pty.console, cols, rows, cellWidth, cellHeight)
        if (t != 0L && text != null) VtNative.print(t, text)
        return t
    }

    /** From vt.rs, on the pump thread: the screen changed since the last frame. */
    @Suppress("unused")
    fun onOutput() {
        view?.postInvalidateOnAnimation()
    }

    /** From vt.rs, on the pump thread: a program set the title. */
    @Suppress("unused")
    fun onTitle() {
        main.post { if (!released) events.onTitleChanged(this) }
    }

    /** From vt.rs, on the pump thread: a program rang the bell. */
    @Suppress("unused")
    fun onBell() {
        main.post { if (!released) events.onBell(this) }
    }

    /** From vt.rs, on the pump thread: a program wrote the clipboard (null clears it). */
    @Suppress("unused")
    fun onClipboard(text: String?) {
        main.post { if (!released) events.onClipboard(this, text) }
    }

    /** From vt.rs, on the pump thread: a program asked for a notification. */
    @Suppress("unused")
    fun onNotification(title: String, body: String) {
        main.post { if (!released) events.onNotification(this, title, body) }
    }

    /**
     * From vt.rs, on the pump thread: programs asked to read the clipboard (OSC 52 "?") [count]
     * times, the last naming clipboard [kind] (`c`, `p` or `s`). They wait for the answer: the
     * service has the user asked, or answers with an empty clipboard ([answerClipboardRead]).
     */
    @Suppress("unused")
    fun onClipboardRead(kind: Int, count: Int) {
        main.post {
            if (released) return@post
            val first = clipboardReads.isEmpty()
            repeat(count) { clipboardReads.add(kind.toChar()) }
            if (first) events.onClipboardRead(this)
        }
    }

    /**
     * Answers the clipboard reads that wait with [text], or, denied, with an empty clipboard
     * (as libghostty-vt answers a read it refuses), so programs do not wait for a timeout. The
     * answer names the clipboard asked for, ends with ST and carries the text in base64, as
     * Ghostty's does.
     */
    fun answerClipboardRead(text: String?) {
        if (clipboardReads.isEmpty()) return
        val data = Base64.encodeToString(text.orEmpty().toByteArray(), Base64.NO_WRAP)
        val answer = clipboardReads.joinToString("") { "\u001b]52;$it;$data\u001b\\" }
        clipboardReads.clear()
        write(answer.toByteArray())
    }

    /**
     * From vt.rs, on the pump thread: a program reported its progress (OSC 9;4), [state] being
     * libghostty-vt's `GhosttyTerminalProgressState` and [percent] 0–100 or -1.
     */
    @Suppress("unused")
    fun onProgress(state: Int, percent: Int) {
        main.post {
            if (!released && isRunning) setProgress(if (state == PROGRESS_REMOVE) null else Progress(state, percent))
        }
    }

    /** Takes a progress report, or its removal, and waits [PROGRESS_TIMEOUT_MS] for the next. */
    private fun setProgress(p: Progress?) {
        main.removeCallbacks(dropProgress)
        if (p != null) main.postDelayed(dropProgress, PROGRESS_TIMEOUT_MS)
        if (p == progress) return
        progress = p
        events.onStatusChanged(this)
    }

    /**
     * From vt.rs, on the pump thread: a program status report (OSC 7501), the states and kinds
     * being libghostty-vt's. The reports of one read are usually applied together, so that a
     * passing state between two of them notifies no one.
     */
    @Suppress("unused")
    fun onStatus(state: Int, kind: Int, percent: Int, id: String, title: String, message: String) {
        val report = Status(id, state, kind, percent, withoutDirectionControls(title), withoutDirectionControls(message))
        val first = synchronized(reports) {
            reports.add(report)
            reports.size == 1
        }
        if (first) main.post(::applyReports)
    }

    /**
     * Applies the waiting reports as the specification says: a report replaces its record, a clear
     * removes the record and those beneath it (`build` takes `build/test` along; an empty id takes
     * every record, as after a full reset).
     */
    private fun applyReports() {
        val taken = synchronized(reports) { reports.toList().also { reports.clear() } }
        if (released || !isRunning) return
        for (r in taken) {
            if (r.state == STATUS_CLEAR) {
                if (r.id.isEmpty()) records.clear() else records.keys.removeAll { it == r.id || it.startsWith(r.id + "/") }
                continue
            }
            records.remove(r.id)
            records[r.id] = r
            if (records.size > MAX_STATUS_RECORDS) records.remove(records.keys.first())
        }
        updateStatus()
    }

    /** Sets [status] from the records, telling the service when it changed. */
    private fun updateStatus() {
        var top: Status? = null
        // Oldest first, so the most recently updated wins among equals
        for (r in records.values) {
            if (top == null || urgency(r.state) >= urgency(top.state)) top = r
        }
        if (top == status) return
        status = top
        events.onStatusChanged(this)
    }

    /**
     * The user has seen the tab: its done and error records go, which the specification keeps
     * only until then.
     */
    fun statusSeen() {
        if (records.values.removeAll { it.state == STATUS_DONE || it.state == STATUS_ERROR }) updateStatus()
    }

    /**
     * From vt.rs, on the pump thread: the process group in the foreground when progress or a
     * status was last reported no longer is. What it said about its work goes, as the
     * specification has a terminal do when the program exits or the shell's next prompt starts:
     * the progress and the working, blocked and idle records; done and error ones stay until seen.
     */
    @Suppress("unused")
    fun onForeground() {
        main.post {
            if (released) return@post
            setProgress(null)
            if (records.values.removeAll { it.state != STATUS_DONE && it.state != STATUS_ERROR }) updateStatus()
        }
    }

    /**
     * From vt.rs, on the pump thread: the shell's prompt marks (OSC 133) saw a command finish after
     * [millis] milliseconds, with exit code [exit] when [hasExit]: from the line feed that ended
     * its input (or the output's mark) to the next prompt's command end.
     */
    @Suppress("unused")
    fun onCommandFinished(hasExit: Boolean, exit: Int, millis: Long) {
        main.post { if (!released) events.onCommandFinished(this, exit.takeIf { hasExit }, millis) }
    }

    /**
     * From vt.rs, on the pump thread: tmux's control mode ended, which vt.c detached with an empty
     * line as it started (the tab would show nothing of it).
     */
    @Suppress("unused")
    fun onTmuxDetached() {
        main.post { if (!released) events.onTmuxDetached(this) }
    }

    /**
     * The grid changed: the terminal reflows, the console's foreground group gets SIGWINCH, and a
     * program that asked for in-band size reports (mode 2048) gets one.
     */
    fun resize(cols: Int, rows: Int, cellWidth: Int, cellHeight: Int) {
        if (tab == 0L) return
        if (cols == this.cols && rows == this.rows && cellWidth == this.cellWidth && cellHeight == this.cellHeight) return
        this.cols = cols
        this.rows = rows
        this.cellWidth = cellWidth
        this.cellHeight = cellHeight
        // Replies the resize made (the size report of a program using mode 2048) go out at once,
        // not after the program's next output
        val replies = VtNative.resize(tab, cols, rows, cellWidth, cellHeight)
        if (replies != null && replies.isNotEmpty()) write(replies)
    }

    /** Input for the program, written on the tab's writer thread; dropped once the shell exited. */
    fun write(data: ByteArray) {
        val w = writer ?: return
        if (!isRunning) return
        val console = pty.console
        try {
            w.execute { Native.write(machine, console, data, 0, data.size) }
        } catch (_: RejectedExecutionException) {
            // Released meanwhile
        }
    }

    /**
     * Clears the screen and the scrollback as Ghostty's clear_screen does (⋮); a shell at its prompt
     * gets a form feed (^L) to draw the prompt again on the empty screen. Nothing on the alternate
     * screen.
     */
    fun clear() {
        if (tab == 0L) return
        when (VtNative.clear(tab)) {
            -1 -> return
            1 -> write(FORM_FEED)
        }
        view?.invalidate()
    }

    /**
     * Resets the terminal as Ghostty's reset does (⋮); a shell at its prompt gets a form feed (^L)
     * to draw the prompt again on the empty screen.
     */
    fun reset() {
        if (tab == 0L) return
        if (VtNative.reset(tab)) write(FORM_FEED)
        view?.invalidate()
        // The title a program set is gone
        events.onTitleChanged(this)
    }

    private fun exited(code: Int) {
        if (exit != null) return
        exit = code
        hangUp()
        writer?.shutdown()
        main.removeCallbacks(dropProgress)
        if (released) return
        // Nothing runs any more to work, wait or have finished: an ended tab shows none of it
        if (progress != null || status != null) {
            progress = null
            records.clear()
            status = null
            events.onStatusChanged(this)
        }
        if (tab != 0L) {
            val reason = when {
                code > 0 -> " (code $code)"
                code < 0 -> " (signal ${-code})"
                else -> ""
            }
            VtNative.print(tab, "\r\n[Process completed$reason - press Enter]".toByteArray())
            view?.invalidate()
        }
        events.onFinished(this)
    }

    /** The tab is closed or its kernel goes away: hang up and free the terminal. */
    fun release() {
        if (released) return
        released = true
        finishIfRunning()
        writer?.shutdown()
        main.removeCallbacks(dropProgress)
        // A running pump keeps the terminal until it returns
        if (tab != 0L) VtNative.destroy(tab)
    }

    /** Hangs up the console when the shell still runs: it gets SIGHUP. */
    fun finishIfRunning() {
        if (isRunning) hangUp()
    }

    /** Closes the console once, so a later close cannot hang up a tab that reused its number. */
    private fun hangUp() {
        if (hungUp.compareAndSet(false, true)) pty.close()
    }

    companion object {
        private const val TAG = "VtSession"

        // libghostty-vt's GhosttyTerminalProgressState (OSC 9;4)
        const val PROGRESS_REMOVE = 0
        const val PROGRESS_SET = 1
        const val PROGRESS_ERROR = 2
        const val PROGRESS_INDETERMINATE = 3
        const val PROGRESS_PAUSE = 4

        // libghostty-vt's GhosttyProgramStatusState (OSC 7501)
        const val STATUS_IDLE = 0
        const val STATUS_WORKING = 1
        const val STATUS_DONE = 2
        const val STATUS_BLOCKED = 3
        const val STATUS_ERROR = 4
        const val STATUS_CLEAR = 5

        // libghostty-vt's GhosttyProgramStatusKind: what a blocked program needs
        const val KIND_NONE = 0
        const val KIND_PERMISSION = 1
        const val KIND_QUESTION = 2
        const val KIND_AUTH = 3

        /** Ctrl+L, which shells take as a request to draw their prompt again. */
        private val FORM_FEED = byteArrayOf(0x0c)

        /** How long a progress report lasts without the next one (Ghostty's 15 s). */
        private const val PROGRESS_TIMEOUT_MS = 15_000L

        /** The most status records a tab keeps (the specification allows 64 to 256). */
        private const val MAX_STATUS_RECORDS = 64

        /**
         * What [status] says the program is doing or needs, in a few words for people (a
         * notification, TalkBack).
         */
        fun describe(context: Context, status: Status): String = context.getString(
            when (status.state) {
                STATUS_BLOCKED -> when (status.kind) {
                    KIND_PERMISSION -> R.string.status_permission
                    KIND_QUESTION -> R.string.status_question
                    KIND_AUTH -> R.string.status_auth
                    else -> R.string.status_blocked
                }
                STATUS_DONE -> R.string.status_done
                STATUS_ERROR -> R.string.status_error
                STATUS_WORKING -> R.string.status_working
                else -> R.string.status_idle
            },
        )

        /** How much a record needs the user: [status] is the one that needs them most. */
        private fun urgency(state: Int) = when (state) {
            STATUS_BLOCKED -> 4
            STATUS_ERROR -> 3
            STATUS_DONE -> 2
            STATUS_WORKING -> 1
            else -> 0
        }

        /**
         * [text] without Unicode's text direction controls (embeddings, overrides, isolates and
         * marks), which could make it read otherwise outside the terminal.
         */
        private fun withoutDirectionControls(text: String) = text.filterNot {
            it in '‪'..'‮' || it in '⁦'..'⁩' || it == '‎' || it == '‏' || it == '؜'
        }
    }
}
