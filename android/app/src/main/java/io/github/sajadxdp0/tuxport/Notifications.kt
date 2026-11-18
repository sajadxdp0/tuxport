package io.github.sajadxdp0.tuxport

import android.app.ApplicationExitInfo
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.icu.text.ListFormatter
import android.icu.text.MeasureFormat
import android.icu.util.Measure
import android.icu.util.MeasureUnit
import android.os.Build
import android.os.LocaleList
import android.system.Os
import android.system.OsConstants
import android.widget.Toast
import io.github.sajadxdp0.tuxport.Notifications.Companion.BELL_NOTIFY_MS
import io.github.sajadxdp0.tuxport.Notifications.Companion.COMMAND_NOTIFY_MS
import io.github.sajadxdp0.tuxport.Notifications.Companion.KILL_NOTIFICATION_BASE
import java.text.DateFormat
import java.time.Instant
import java.util.Date

/**
 * The service's notifications but its ongoing one (the android/service map): the channels; the
 * reports of Tuxport's problems (a kernel stuck on its lock, a crash or ANR of the last run, a
 * process the kernel layer ended) with "Copy report"; services that did not start at boot; ports
 * other devices can reach; and each tab's own notification, for a tab not in front of the user
 * (bells, programs' notifications and statuses, finished commands). [TerminalService] decides
 * what is told; this builds, posts and takes away the notifications.
 */
internal class Notifications(private val service: TerminalService) {

    private val manager by lazy { service.getSystemService(NotificationManager::class.java) }

    /** The app's language, for what the string resources do not format: lists and durations. */
    private val locale get() = service.resources.configuration.locales[0]

    /**
     * The next ended-process notification ([kill]), counting up; it starts from the clock so a new
     * service does not reuse the ids of reports still shown from the last one.
     */
    private var killNotifications = (System.currentTimeMillis() % KILL_NOTIFICATION_COUNT).toInt()

    /** The languages the channels were last named in ([createChannels]). */
    private var channelLocales: LocaleList? = null

    /** When each tab's bell last posted a notification ([bell]). */
    private val bellNotified = HashMap<VtSession, Long>()

    /**
     * The program status each tab's notification shows ([status]), while it shows one: a bell or a
     * program's own notification replacing it takes the tab out.
     */
    private val statusNotified = HashMap<VtSession, VtSession.Status>()

    /**
     * The hold of the kernel lock last notified ([stall]): the kernel it was in, its id, and
     * whether it had ended.
     */
    private var stallNotified = Triple(0L, 0L, false)

    /**
     * Creates the channels, or names them anew: their names are in the app's language. Returns
     * whether that language is another than the last time's (the first time included).
     */
    fun createChannels(): Boolean {
        val locales = service.resources.configuration.locales
        if (locales == channelLocales) return false
        channelLocales = locales
        val channel = NotificationChannel(
            SESSIONS_CHANNEL_ID, service.getString(R.string.channel_sessions), NotificationManager.IMPORTANCE_LOW,
        )
        val services = NotificationChannel(
            NETWORK_CHANNEL_ID, service.getString(R.string.channel_network), NotificationManager.IMPORTANCE_DEFAULT,
        )
        val problems = NotificationChannel(
            PROBLEMS_CHANNEL_ID, service.getString(R.string.channel_problems), NotificationManager.IMPORTANCE_HIGH,
        )
        val terminal = NotificationChannel(
            TERMINAL_CHANNEL_ID, service.getString(R.string.channel_terminal), NotificationManager.IMPORTANCE_HIGH,
        ).apply {
            // Only sets the default for a new channel; existing channels keep the user's settings.
            enableVibration(true)
        }
        manager.createNotificationChannels(listOf(channel, services, problems, terminal))
        return true
    }

    /**
     * The last run ended because Tuxport crashed or stopped responding: posts a notification with
     * "Copy report" ([bugReport]), as Tuxport's bugs in the kernel get ([kill]).
     */
    fun crash(exit: ApplicationExitInfo) {
        val anr = exit.reason == ApplicationExitInfo.REASON_ANR
        val time = DateFormat.getDateTimeInstance(DateFormat.SHORT, DateFormat.SHORT).format(Date(exit.timestamp))
        val text = service.getString(if (anr) R.string.anr_text else R.string.crashed_text, time)
        val what = when (exit.reason) {
            ApplicationExitInfo.REASON_CRASH -> "crashed (Java)"
            ApplicationExitInfo.REASON_CRASH_NATIVE -> "crashed (native, signal ${exit.status})"
            else -> "stopped responding (ANR)"
        }
        // The report is in English, for whoever reads the feedback. Android's description says
        // only "crash" for crashes; for an ANR it says what timed out
        val line = "The app process (pid ${exit.pid}) $what at ${Instant.ofEpochMilli(exit.timestamp)}" +
                (exit.description?.takeIf { anr }?.let { ": $it" } ?: "")
        val title = service.getString(if (anr) R.string.anr_title else R.string.crashed_title)
        val notification = builder(PROBLEMS_CHANNEL_ID, title, text)
            .addAction(copyReportAction(service, CRASH_NOTIFICATION_ID, bugReport(line, "")))
            .build()
        manager.notify(CRASH_NOTIFICATION_ID, notification)
    }

    /**
     * An entry of [Native.bklStall] for kernel [m]: one kernel operation has held the kernel lock
     * longer than the kernel's watchdog allows, and every guest program, and the service's other
     * calls into the kernel, wait for it. A new one posts a notification with "Copy report"
     * ([bugReport]); once it has ended, a notification still showing says how long it lasted,
     * without alerting again.
     */
    fun stall(m: Long, line: String) {
        val f = line.split('\t')
        if (f.size != 4) return
        val id = f[0].toLongOrNull() ?: return
        val ended = f[3].isNotEmpty()
        val seen = Triple(m, id, ended)
        if (seen == stallNotified) return
        val updating = stallNotified.first == m && stallNotified.second == id
        stallNotified = seen
        val report = bugReport(f[2], f[3])
        if (updating && manager.activeNotifications.none { it.id == STALL_NOTIFICATION_ID }) return
        val secs = f[1].toIntOrNull() ?: 0
        val text = service.getString(if (ended) R.string.stall_ended_text else R.string.stall_text, secs)
        val title = service.getString(if (ended) R.string.stall_ended_title else R.string.stall_title)
        val notification = builder(PROBLEMS_CHANNEL_ID, title, text)
            .setOnlyAlertOnce(true)
            // The record of its end is a kernel log line
            .addAction(copyReportAction(service, STALL_NOTIFICATION_ID, report))
            .build()
        manager.notify(STALL_NOTIFICATION_ID, notification)
    }

    /**
     * OpenRC services did not start at boot: posts a notification saying which ones and where the
     * output is, the services listed as the app's language lists things.
     */
    fun bootFailures(services: List<String>) {
        Diagnostics.record("Boot services failed: ${services.joinToString(", ")}")
        val title = service.resources.getQuantityString(R.plurals.boot_failed_title, services.size, services.size)
        val text = service.getString(R.string.boot_failed_text, ListFormatter.getInstance(locale).format(services))
        manager.notify(BOOT_NOTIFICATION_ID, builder(PROBLEMS_CHANNEL_ID, title, text).build())
    }

    /**
     * An entry of [Native.takeListens]: `<process name>\t<port>\t<host port>`. One notification per
     * port.
     */
    fun listen(line: String) {
        val fields = line.split('\t')
        if (fields.size != 3) return
        val comm = fields[0]
        val port = fields[1].toIntOrNull() ?: return
        val hostPort = fields[2].toIntOrNull() ?: return
        val text = if (hostPort != port) {
            service.getString(R.string.listening_mapped, hostPort, port)
        } else {
            service.getString(R.string.listening_direct)
        }
        val title = service.getString(R.string.listening_title, comm, port)
        manager.notify(LISTEN_NOTIFICATION_BASE + port, builder(NETWORK_CHANNEL_ID, title, text).build())
    }

    /**
     * The kernel layer ended process [comm] for the reason [text] says, each report a notification
     * of its own; for Tuxport's own bug, whose kernel report line, log records and unimplemented
     * paths [bug] holds, with "Copy report" ([bugReport]) on it, for the user to paste into
     * feedback.
     */
    fun kill(comm: String, text: String, bug: List<String>?) {
        val id = KILL_NOTIFICATION_BASE + killNotifications++ % KILL_NOTIFICATION_COUNT
        val builder = builder(PROBLEMS_CHANNEL_ID, service.getString(R.string.killed_title, comm), text)
        if (bug != null) builder.addAction(copyReportAction(service, id, bugReport(bug[0], bug[1], bug[2])))
        manager.notify(id, builder.build())
    }

    /**
     * The report copied for the user to paste into feedback, in English whatever the app's
     * language (its reader is whoever gets the feedback): the app's version, the device, the
     * distro and the execution tier, plus the kernel layer's report (process, thread, last guest
     * pc, the panic message and location, the same as the line in `dmesg`), the kernel log
     * entries before it and itself ([log] separated by U+001E, with timestamps as `dmesg` writes
     * them), and the unimplemented paths the guest reached most since boot ([stubs] separated by
     * U+001E: what the workload asked for that Tuxport lacks).
     */
    private fun bugReport(report: String, log: String, stubs: String = ""): String = buildString {
        appendLine("Tuxport ${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE}, ${BuildConfig.BUILD_TYPE})")
        appendLine(
            "Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT}), ${Build.MANUFACTURER} ${Build.MODEL}, " +
                    "${Build.SUPPORTED_ABIS.firstOrNull()}, page size ${Os.sysconf(OsConstants._SC_PAGESIZE)}",
        )
        appendLine("${Rootfs.installed(service)?.label ?: "unknown distribution"}, Tier ${Settings.tier(service)}")
        append(report)
        if (log.isNotEmpty()) {
            append("\n\nKernel log:\n")
            append(log.split('\u001e').joinToString("\n"))
        }
        if (stubs.isNotEmpty()) {
            append("\n\nUnimplemented paths reached:\n")
            append(stubs.split('\u001e').joinToString("\n"))
        }
    }.also(Diagnostics::record)

    /** Legacy service actions use the same privacy check as the receiver and activity. */
    fun copyReport(intent: Intent) = copyReport(service, intent)

    /**
     * A program in [session], not the tab in front, rang the bell: the tab's notification ([tab]),
     * at most one per [BELL_NOTIFY_MS], alerting only when it is not showing already.
     */
    fun bell(session: VtSession) {
        val now = System.currentTimeMillis()
        if (now - (bellNotified[session] ?: 0) < BELL_NOTIFY_MS) return
        bellNotified[session] = now
        statusNotified.remove(session)
        tab(session, "", service.getString(R.string.bell_rang), quiet = true)
    }

    /** A program in [session], not the tab in front, posted a notification (OSC 9, OSC 777). */
    fun program(session: VtSession, title: String, body: String) {
        statusNotified.remove(session)
        tab(session, title, body, quiet = false)
    }

    /**
     * Tells the user when [session]'s program status (OSC 7501) needs them or has news: blocked
     * (waiting for an approval, an answer or credentials), done or failed, as the tab's
     * notification ([tab]) titled with the record's title, or the tab's. Each status notifies
     * once: the same record again posts nothing, while one that changes posts anew. A status that
     * goes without the user (the program was killed, or stopped waiting) takes its notification
     * away, unless something else has replaced it. In the tab [inFront] nothing is posted, and done
     * and error count as seen at once.
     */
    fun status(session: VtSession, inFront: Boolean) {
        val status = session.status?.takeIf {
            it.state == VtSession.STATUS_BLOCKED || it.state == VtSession.STATUS_DONE || it.state == VtSession.STATUS_ERROR
        }
        if (status == null) {
            if (statusNotified.remove(session) != null) cancelTab(session)
            return
        }
        if (inFront) {
            session.statusSeen()
            return
        }
        if (statusNotified[session] == status) return
        statusNotified[session] = status
        val what = VtSession.describe(service, status)
        val text = if (status.message.isEmpty()) what else service.getString(R.string.status_message, what, status.message)
        tab(session, status.title, text, quiet = false)
    }

    /**
     * A command that ran at least [COMMAND_NOTIFY_MS] finished in [session], not the tab in front,
     * as Ghostty's notify-on-command-finish = unfocused tells (it defaults to never, but a phone
     * shows one tab at a time, and none while the app is away): the tab's notification says
     * whether it succeeded, how long it took and the exit code it failed with.
     */
    fun commandFinished(session: VtSession, exitCode: Int?, millis: Long) {
        if (millis < COMMAND_NOTIFY_MS) return
        statusNotified.remove(session)
        val title = service.getString(
            when (exitCode) {
                null -> R.string.command_finished
                0 -> R.string.command_succeeded
                else -> R.string.command_failed
            },
        )
        val took = duration(millis)
        val text = if (exitCode == null || exitCode == 0) {
            service.getString(R.string.command_took, took)
        } else {
            service.getString(R.string.command_took_code, took, exitCode)
        }
        tab(session, title, text, quiet = false)
    }

    /**
     * How long a command took ([millis]) in the app's language: hours and minutes, minutes and
     * seconds, or seconds, leaving out a second part of 0.
     */
    private fun duration(millis: Long): String {
        val s = millis / 1000
        val parts = when {
            s >= 3600 -> listOf(Measure(s / 3600, MeasureUnit.HOUR), Measure(s % 3600 / 60, MeasureUnit.MINUTE))
            s >= 60 -> listOf(Measure(s / 60, MeasureUnit.MINUTE), Measure(s % 60, MeasureUnit.SECOND))
            else -> listOf(Measure(s, MeasureUnit.SECOND))
        }.filterIndexed { i, m -> i == 0 || m.number.toLong() != 0L }
        return MeasureFormat.getInstance(locale, MeasureFormat.FormatWidth.SHORT)
            .formatMeasures(*parts.toTypedArray())
    }

    /**
     * A program in [session], not the tab in front, asked for attention (a bell, OSC 9, OSC 777,
     * a program status), or a command there finished: one notification per tab, replaced by the
     * tab's next one and taken away when the tab is shown ([cancelTab]), opening that tab. It is
     * titled [title], or else the tab's title; a [quiet] one (the bell) alerts only when the tab's
     * notification is not showing already.
     */
    private fun tab(session: VtSession, title: String, text: String, quiet: Boolean) {
        val number = service.sessions.indexOf(session) + 1
        if (number == 0) return
        val id = tabId(session)
        val open = Intent(service, MainActivity::class.java).putExtra(MainActivity.EXTRA_TAB, session.id)
        val pending = PendingIntent.getActivity(
            service, id, open, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val shown = title.ifEmpty { session.title?.takeIf { it.isNotBlank() } ?: "sh" }
        val notification = builder(TERMINAL_CHANNEL_ID, shown, text, pending)
            .setSubText(service.getString(R.string.tab_number, number))
            .setOnlyAlertOnce(quiet)
            .build()
        manager.notify(id, notification)
    }

    /** Takes [session]'s notification away: the tab is shown or closed. */
    fun cancelTab(session: VtSession) {
        bellNotified.remove(session)
        statusNotified.remove(session)
        manager.cancel(tabId(session))
    }

    private fun tabId(session: VtSession) = TAB_NOTIFICATION_BASE + session.id % TAB_NOTIFICATION_COUNT

    /**
     * A notification on [channel] with [title] and all of [text], which a tap takes away, opening
     * [intent] (the terminal, unless given).
     */
    private fun builder(channel: String, title: String, text: String, intent: PendingIntent = openApp()): Notification.Builder {
        val privacyEnabled = Settings.privacyMode(service)
        val shownTitle = if (privacyEnabled) service.getString(R.string.settings_privacy) else title
        val shownText = if (privacyEnabled) service.getString(R.string.privacy_notification) else text
        return Notification.Builder(service, channel)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(shownTitle)
            .setContentText(shownText)
            .setStyle(Notification.BigTextStyle().bigText(shownText))
            .setContentIntent(intent)
            .setAutoCancel(true)
    }

    /** Opens the terminal, for a notification's tap. */
    private fun openApp(): PendingIntent = PendingIntent.getActivity(
        service, 0, Intent(service, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
    )

    /** Copies reports without starting the terminal; stale actions are checked at delivery. */
    class ReportReceiver : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            if (intent.action == ACTION_COPY_REPORT) copyReport(context, intent)
        }
    }

    companion object {
        /** The channel of the service's ongoing notification (id 1). */
        const val SESSIONS_CHANNEL_ID = "sessions"
        private const val NETWORK_CHANNEL_ID = "network"
        private const val PROBLEMS_CHANNEL_ID = "problems"
        private const val TERMINAL_CHANNEL_ID = "terminal"

        /** The boot failure notification. */
        private const val BOOT_NOTIFICATION_ID = 2

        /** The notification that the last run ended in a crash or ANR of Tuxport. */
        private const val CRASH_NOTIFICATION_ID = 3

        /** The notification that a kernel operation held the kernel lock too long ([stall]). */
        private const val STALL_NOTIFICATION_ID = 4

        /** Listen notifications are numbered by port: 1000 + port, below [KILL_NOTIFICATION_BASE]. */
        private const val LISTEN_NOTIFICATION_BASE = 1000

        /**
         * Notifications of ended processes: one id per report, above every listen id, so neither a
         * listen nor a later report replaces one (and with it a bug's "Copy report").
         */
        private const val KILL_NOTIFICATION_BASE = 100_000
        private const val KILL_NOTIFICATION_COUNT = 100_000

        /**
         * Notifications for tabs ([tab]), one per tab, above the kill reports' ids; a tab's
         * [VtSession.id] modulo [TAB_NOTIFICATION_COUNT] picks its own.
         */
        private const val TAB_NOTIFICATION_BASE = 200_000
        const val TAB_NOTIFICATION_COUNT = 100_000

        /** A tab's bell posts at most one notification per this long. */
        private const val BELL_NOTIFY_MS = 1_000L

        /**
         * A command that ran at least this long notifies when it finishes in a tab not in front
         * (Ghostty's notify-on-command-finish-after is 5 s; a phone's shorter commands, a few
         * package installs, are more often watched to the end).
         */
        private const val COMMAND_NOTIFY_MS = 10_000L

        /** The notification action for "Copy report" ([copyReport]). */
        const val ACTION_COPY_REPORT = "io.github.sajadxdp0.tuxport.COPY_REPORT"
        private const val EXTRA_REPORT = "report"
        private const val EXTRA_NOTIFICATION = "notification"

        /** Only report export requires an authentication page while privacy is enabled. */
        fun copyReportAction(context: Context, id: Int, report: String): Notification.Action {
            val authenticate = Settings.privacyMode(context)
            val copy = Intent(context, if (authenticate) NotificationActivity::class.java else ReportReceiver::class.java)
                .setAction(ACTION_COPY_REPORT)
                .putExtra(EXTRA_REPORT, report)
                .putExtra(EXTRA_NOTIFICATION, id)
            val flags = PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
            val pending = if (authenticate) {
                PendingIntent.getActivity(context, id, copy, flags)
            } else {
                PendingIntent.getBroadcast(context, id, copy, flags)
            }
            return Notification.Action.Builder(null, context.getString(R.string.copy_report), pending).build()
        }

        /** Copies only while unlocked, then dismisses the report notification. */
        fun copyReport(context: Context, intent: Intent) {
            if (Privacy.locked(context)) return
            val report = intent.getStringExtra(EXTRA_REPORT).orEmpty()
            if (report.isEmpty()) return
            val clipboard = context.getSystemService(ClipboardManager::class.java)
            clipboard.setPrimaryClip(ClipData.newPlainText(context.getString(R.string.copy_report), report))
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
                Toast.makeText(context, R.string.report_copied, Toast.LENGTH_SHORT).show()
            }
            context.getSystemService(NotificationManager::class.java).cancel(intent.getIntExtra(EXTRA_NOTIFICATION, 0))
        }

        /** Control actions do not export content and remain available while locked. */
        fun serviceAction(context: Context, id: Int, action: String): PendingIntent = PendingIntent.getService(
            context, id, Intent(context, TerminalService::class.java).setAction(action), PendingIntent.FLAG_IMMUTABLE,
        )
    }
}
