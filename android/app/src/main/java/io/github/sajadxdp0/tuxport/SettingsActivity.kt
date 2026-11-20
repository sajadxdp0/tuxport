package io.github.sajadxdp0.tuxport

import android.app.Dialog
import android.content.ActivityNotFoundException
import android.content.ComponentName
import android.content.Context
import android.content.DialogInterface
import android.content.Intent
import android.content.ServiceConnection
import android.net.Uri
import android.os.Bundle
import android.os.IBinder
import android.view.View
import android.widget.Toast
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.browser.customtabs.CustomTabsIntent
import androidx.core.net.toUri
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.updatePadding
import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelProvider
import androidx.preference.ListPreference
import androidx.preference.ListPreferenceDialogFragmentCompat
import androidx.preference.Preference
import androidx.preference.PreferenceCategory
import androidx.preference.PreferenceFragmentCompat
import androidx.preference.PreferenceScreen
import androidx.preference.SwitchPreferenceCompat
import androidx.recyclerview.widget.RecyclerView
import com.google.android.material.appbar.AppBarLayout
import com.google.android.material.appbar.MaterialToolbar
import com.google.android.material.color.DynamicColors
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import java.io.File
import java.io.IOException
import kotlin.concurrent.thread

/** Material 3 frame for the standard AndroidX settings list. */
internal class SettingsActivity : AppCompatActivity(), Privacy.Listener, Privacy.ExportListener {
    override fun onCreate(savedInstanceState: Bundle?) {
        DynamicColors.applyToActivityIfAvailable(this)
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        setContentView(R.layout.page)
        findViewById<MaterialToolbar>(R.id.toolbar).apply {
            setTitle(R.string.settings)
            setNavigationOnClickListener { finish() }
        }
        if (savedInstanceState == null) {
            supportFragmentManager.beginTransaction()
                .replace(R.id.content, SettingsFragment())
                .commitNow()
        }
        val appBar = findViewById<AppBarLayout>(R.id.app_bar)
        // The list is inside the fragment's container; layout and scroll callbacks must use
        // the same scrolling view when deciding whether to lift the app bar.
        appBar.liftOnScrollTargetViewId = androidx.preference.R.id.recycler_view
        ViewCompat.setOnApplyWindowInsetsListener(findViewById(R.id.root)) { v, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout())
            v.setPadding(bars.left, 0, bars.right, 0)
            appBar.updatePadding(top = bars.top)
            // The fragment applies the bottom inset to its RecyclerView.
            insets
        }
    }

    override fun onPrivacyUnlocked() {
        (supportFragmentManager.findFragmentById(R.id.content) as? SettingsFragment)?.onNotice()
    }

    override fun onLogExportAuthorized(authorization: Privacy.ExportAuthorization) {
        (supportFragmentManager.findFragmentById(R.id.content) as? SettingsFragment)?.exportLogs(authorization)
    }
}

/**
 * Displays service state and forwards changes to TerminalService or the authenticated Privacy
 * controller. Preferences never persist values themselves: service setters also update wake
 * locks, document roots and running terminals. Destructive operations still ask first and
 * return to the terminal when replacement or restart begins.
 */
class SettingsFragment : PreferenceFragmentCompat(), TerminalService.Listener {

    private var service: TerminalService? = null
    private var insetsApplied = false
    private lateinit var distro: Preference
    private lateinit var reset: Preference
    private lateinit var account: ListPreference

    /**
     * The regular users in `/etc/passwd` ([Rootfs.regularAccounts]), read in the background each
     * time the settings page is shown.
     */
    private var accounts: List<Rootfs.Account> = emptyList()
    private var accountLoad = 0

    private lateinit var tier: Preference
    private lateinit var export: Preference
    private var exporting = false
    private var exportGeneration = 0
    private val logSave by lazy { ViewModelProvider(this)[PendingLogSave::class.java] }
    private val createLogDocument = registerForActivityResult(ActivityResultContracts.CreateDocument("application/zip")) { uri ->
        // The process-local archive reference authorizes this result once, never a new export.
        val archive = logSave.take() ?: return@registerForActivityResult
        Privacy.deferNextUnlock(requireActivity())
        if (uri == null) {
            archive.delete()
            setExportProgress()
        } else {
            saveLogs(archive, uri)
        }
    }
    private lateinit var batteryUsage: Preference
    private lateinit var notifications: Preference
    private val requestNotifications = registerForActivityResult(ActivityResultContracts.RequestPermission()) {
        updatePermissionActions()
    }
    private lateinit var runPolicy: ListPreference
    private lateinit var fileAccess: SwitchPreferenceCompat
    private lateinit var privacy: SwitchPreferenceCompat
    private lateinit var fillWide: SwitchPreferenceCompat
    private lateinit var cursorStyle: ListPreference
    private lateinit var cursorBlink: SwitchPreferenceCompat
    private lateinit var volumeKeys: SwitchPreferenceCompat
    private lateinit var tabList: SwitchPreferenceCompat
    private lateinit var licenses: Preference
    private lateinit var update: Preference

    /** The last release offered, for the cancel question while it downloads. */
    private var updateVersion = ""

    /** The session of the last update check or download: a result from an older one is dropped. */
    private var updateSession = 0
    private var updateDownloading = false

    @Volatile
    private var updateCancelled = false

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            val s = (binder as TerminalService.LocalBinder).service
            service = s
            s.addListener(this@SettingsFragment)
            render()
            onNotice()
            loadAccounts()
        }

        override fun onServiceDisconnected(name: ComponentName) {
            service = null
            render()
        }
    }

    override fun onCreatePreferences(savedInstanceState: Bundle?, rootKey: String?) {
        preferenceScreen = preferenceManager.createPreferenceScreen(requireContext())
        distro = action(R.string.settings_distro) { onDistro() }
        reset = action(R.string.settings_reset) { onReset() }
        account = choice(R.string.settings_login) { s, value ->
            s.setLogin(value)
            val who = if (value == ConsolePty.LOGIN_AUTO) {
                accounts.firstOrNull()?.name ?: ConsolePty.LOGIN_ROOT
            } else {
                value
            }
            Toast.makeText(requireContext(), getString(R.string.login_changed, who), Toast.LENGTH_SHORT).show()
        }
        tier = action(R.string.settings_tier) { onTier() }
        export = action(R.string.settings_export_logs) {
            chooseLogExport()
        }.apply {
            setSummary(R.string.export_logs_summary)
            isEnabled = !logSave.pending
        }
        NotificationPermission.initialize(requireContext())
        notifications = action(R.string.settings_notifications) {
            NotificationPermission.enable(requireActivity(), requestNotifications)
        }.apply { setSummary(R.string.settings_notifications_summary) }
        batteryUsage = action(R.string.settings_battery_usage) {
            BatteryOptimization.openAppSettings(requireActivity())
        }.apply { setSummary(R.string.battery_prompt) }
        updatePermissionActions()
        runPolicy = choice(R.string.settings_run_policy) { s, value ->
            s.setRunPolicy(RunPolicy.entries.first { it.value == value })
        }.apply {
            entries = RunPolicy.entries.map { getString(it.label) }.toTypedArray()
            entryValues = RunPolicy.entries.map { it.value }.toTypedArray()
        }
        fileAccess = toggle(R.string.settings_file_access, TerminalService::setFileAccess).apply {
            setOnPreferenceChangeListener { _, value ->
                if (service == null) return@setOnPreferenceChangeListener false
                if (value as Boolean) confirmFileAccess() else service?.setFileAccess(false)
                // Only the service's render changes the switch, after confirmation when enabling.
                false
            }
        }
        privacy = SwitchPreferenceCompat(requireContext()).configure(R.string.settings_privacy).apply {
            widgetLayoutResource = R.layout.preference_switch
            setOnPreferenceChangeListener { _, value ->
                onPrivacyMode(value as Boolean)
                false
            }
        }
        fillWide = toggle(R.string.settings_fill_wide, TerminalService::setFillWide)
        cursorStyle = choice(R.string.settings_cursor_style) { s, value -> s.setCursorStyle(value.toInt()) }.apply {
            entries = CURSOR_LABELS.map { getString(it) }.toTypedArray()
            entryValues = CURSOR_STYLES.map { it.toString() }.toTypedArray()
        }
        cursorBlink = toggle(R.string.settings_cursor_blink, TerminalService::setCursorBlink)
        volumeKeys = toggle(R.string.settings_volume_keys, TerminalService::setVolumeKeysSwitchTabs)
        tabList = toggle(R.string.settings_show_tab_list, TerminalService::setShowTabList)
        // The license page includes the full text, copyright and warranty disclaimer.
        licenses = action(R.string.settings_licenses) {
            startActivity(Intent(requireContext(), LicensesActivity::class.java))
        }
        licenses.setSummary(R.string.licenses_summary)
        update = action(R.string.settings_update) { onUpdate() }.apply {
            setSummary(R.string.update_summary)
        }
        val support = link(R.string.settings_support, SUPPORT_URL)
        val project = link(R.string.settings_project, PROJECT_URL)
        val privacyPolicy = link(R.string.settings_privacy_policy, PRIVACY_POLICY_URL)
        val version = action(R.string.settings_version) { onVersion() }.apply {
            summary = BuildConfig.VERSION_NAME
        }

        group(R.string.settings_group_system, distro, account, reset)
        group(R.string.settings_group_android, notifications, batteryUsage, runPolicy, privacy, fileAccess)
        group(R.string.settings_group_terminal, fillWide, cursorStyle, cursorBlink, tabList, volumeKeys)
        group(R.string.settings_group_debug, tier, export)
        group(R.string.settings_group_about, support, project, privacyPolicy, licenses, update, version)
        render()
    }

    override fun onViewCreated(view: View, savedInstanceState: Bundle?) {
        insetsApplied = false
        super.onViewCreated(view, savedInstanceState)
        listView.itemAnimator = null
        listView.clipToPadding = false
        ViewCompat.setOnApplyWindowInsetsListener(view) { _, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout())
            listView.updatePadding(bottom = bars.bottom)
            insetsApplied = true
            allowStateRestoration()
            WindowInsetsCompat.CONSUMED
        }
        ViewCompat.requestApplyInsets(view)
    }

    override fun onCreateAdapter(preferenceScreen: PreferenceScreen): RecyclerView.Adapter<*> =
        super.onCreateAdapter(preferenceScreen).apply {
            // Empty summaries make rows shorter before binding; restoring now would clamp the
            // saved scroll position to that temporary, shorter list. The bottom inset must also
            // be applied before restoring a position anchored at the list's end.
            stateRestorationPolicy = if (service == null || !insetsApplied) {
                RecyclerView.Adapter.StateRestorationPolicy.PREVENT
            } else {
                RecyclerView.Adapter.StateRestorationPolicy.ALLOW
            }
        }

    override fun onStart() {
        super.onStart()
        val context = requireContext()
        val intent = Intent(context, TerminalService::class.java)
        context.startForegroundService(intent)
        context.bindService(intent, connection, Context.BIND_AUTO_CREATE)
    }

    override fun onResume() {
        super.onResume()
        updatePermissionActions()
    }

    private fun updatePermissionActions() {
        notifications.isVisible = !NotificationPermission.enabled(requireContext())
        batteryUsage.isVisible = !BatteryOptimization.isExempt(requireContext())
    }

    private fun chooseLogExport() {
        if (exporting || logSave.pending || Privacy.locked(requireContext())) return
        MaterialAlertDialogBuilder(requireContext())
            .setTitle(R.string.settings_export_logs)
            .setItems(arrayOf(getString(R.string.export_logs_save), getString(R.string.export_logs_share))) { _, which ->
                logSave.saveRequested = which == 0
                Privacy.requestLogExport(requireActivity())
            }
            .showPrivate()
    }

    override fun onStop() {
        exportGeneration++
        service?.removeListener(this)
        requireContext().unbindService(connection)
        service = null
        accountLoad++
        updateSession++
        render()
        super.onStop()
    }

    /** Never queue an export behind the service worker: the kernel may be stuck on its lock. */
    internal fun exportLogs(authorization: Privacy.ExportAuthorization) {
        if (exporting || logSave.pending || !isResumed || !Privacy.canExport(requireContext(), authorization)) return
        setExportProgress(R.string.export_logs_preparing)
        val owner = requireActivity()
        val context = owner.applicationContext
        val generation = exportGeneration
        val save = logSave.saveRequested
        logSave.saveRequested = false
        thread(name = "tuxport-export") {
            val result = runCatching { Diagnostics.export(context, forSave = save) }
            owner.runOnUiThread {
                setExportProgress()
                // A detached/replaced page or a lock must never replay a delayed export.
                if (!isAdded || !isResumed || generation != exportGeneration || !Privacy.canExport(context, authorization)) {
                    result.getOrNull()?.delete()
                    return@runOnUiThread
                }
                result.fold(onSuccess = { archive ->
                    if (save) chooseLogDestination(archive) else shareLogs(archive)
                }, onFailure = {
                    Toast.makeText(context, R.string.export_logs_failed, Toast.LENGTH_LONG).show()
                })
            }
        }
    }

    private fun chooseLogDestination(archive: File) {
        // Authorize only this prepared archive before leaving for the picker.
        logSave.stage(archive)
        setExportProgress()
        try {
            createLogDocument.launch("tuxport-diagnostics-${System.currentTimeMillis()}.zip")
        } catch (_: ActivityNotFoundException) {
            logSave.take()?.delete()
            setExportProgress()
            Toast.makeText(requireContext(), R.string.export_logs_no_picker, Toast.LENGTH_LONG).show()
        }
    }

    private fun shareLogs(archive: File) {
        try {
            val intent = Diagnostics.shareIntent(requireContext(), archive)
            startActivity(Intent.createChooser(intent, getString(R.string.settings_export_logs)))
        } catch (_: ActivityNotFoundException) {
            archive.delete()
            Toast.makeText(requireContext(), R.string.export_logs_no_app, Toast.LENGTH_LONG).show()
        }
    }

    private fun setExportProgress(summary: Int? = null) {
        exporting = summary != null
        if (isAdded) {
            export.isEnabled = !exporting && !logSave.pending
            export.setSummary(summary ?: R.string.export_logs_summary)
        }
    }

    private fun saveLogs(archive: File, destination: Uri) {
        setExportProgress(R.string.export_logs_saving)
        val owner = requireActivity()
        val context = owner.applicationContext
        // Like an intentional share, this already-authorized transfer can finish while locked.
        thread(name = "tuxport-save-logs") {
            val saved = runCatching {
                archive.inputStream().use { input ->
                    val output = context.contentResolver.openOutputStream(destination, "wt")
                        ?: throw IOException("Document provider returned no output stream")
                    output.use { input.copyTo(it) }
                }
            }
            archive.delete()
            owner.runOnUiThread {
                setExportProgress()
                if (isAdded) {
                    Toast.makeText(context, if (saved.isSuccess) R.string.export_logs_saved else R.string.export_logs_failed, Toast.LENGTH_LONG).show()
                }
            }
        }
    }

    /** Stable keys let AndroidX restore list/dialog state without owning the setting's storage. */
    private fun <T : Preference> T.configure(title: Int): T = apply {
        key = resources.getResourceEntryName(title)
        setTitle(title)
        isPersistent = false
        isIconSpaceReserved = false
        isSingleLineTitle = false
    }

    private fun action(title: Int, onClick: () -> Unit): Preference =
        Preference(requireContext()).configure(title).apply {
            setOnPreferenceClickListener {
                onClick()
                true
            }
        }

    private fun link(title: Int, url: String): Preference = action(title) {
        try {
            CustomTabsIntent.Builder().build().launchUrl(requireContext(), url.toUri())
        } catch (e: ActivityNotFoundException) {
            Toast.makeText(requireContext(), R.string.link_no_app, Toast.LENGTH_SHORT).show()
        }
    }.apply { summary = url }

    private fun toggle(title: Int, change: (TerminalService, Boolean) -> Unit): SwitchPreferenceCompat =
        SwitchPreferenceCompat(requireContext()).configure(title).apply {
            widgetLayoutResource = R.layout.preference_switch
            setOnPreferenceChangeListener { _, value ->
                val s = service ?: return@setOnPreferenceChangeListener false
                change(s, value as Boolean)
                true
            }
        }

    private fun choice(title: Int, change: (TerminalService, String) -> Unit): ListPreference =
        ListPreference(requireContext()).configure(title).apply {
            setDialogTitle(title)
            setOnPreferenceChangeListener { _, value ->
                val s = service ?: return@setOnPreferenceChangeListener false
                if (!isEnabled) return@setOnPreferenceChangeListener false
                change(s, value as String)
                true
            }
        }

    private fun group(heading: Int, vararg rows: Preference) {
        val category = PreferenceCategory(requireContext()).configure(heading)
        preferenceScreen.addPreference(category)
        for (row in rows) category.addPreference(row)
    }

    override fun onDisplayPreferenceDialog(preference: Preference) {
        if (preference !is ListPreference) {
            super.onDisplayPreferenceDialog(preference)
            return
        }
        if (parentFragmentManager.findFragmentByTag(CHOICE_DIALOG) != null) return
        ChoiceDialog.newInstance(preference.key).apply {
            // Preference's dialog contract still uses the target fragment to find its preference.
            @Suppress("DEPRECATION")
            setTargetFragment(this@SettingsFragment, 0)
        }.show(parentFragmentManager, CHOICE_DIALOG)
    }

    /** AndroidX owns selection and saved state; only the builder changes for Material 3. */
    class ChoiceDialog : ListPreferenceDialogFragmentCompat() {
        override fun onCreateDialog(savedInstanceState: Bundle?): Dialog {
            val builder = MaterialAlertDialogBuilder(requireContext())
                .setTitle(preference.dialogTitle)
                .setNegativeButton(preference.negativeButtonText, this)
            onPrepareDialogBuilder(builder)
            return builder.create().also {
                Privacy.protect(it)
                // Back, Cancel and an outside tap must never commit the current selection.
                onClick(it, DialogInterface.BUTTON_NEGATIVE)
            }
        }

        companion object {
            fun newInstance(key: String) = ChoiceDialog().apply {
                arguments = Bundle().apply { putString(ARG_KEY, key) }
            }
        }
    }

    // ---- TerminalService.Listener ----

    /**
     * Replacing the store (importing a new distro) begins: go back to the terminal to see the
     * progress.
     */
    override fun onProgress(text: String) = requireActivity().finish()

    override fun onMachineReady(error: String?) {
        render()
        loadAccounts()
    }

    override fun onSettingsChanged() = render()

    /** The download for a distro switch failed. */
    override fun onNotice() {
        if (Privacy.locked(requireContext())) return
        val text = service?.takeNotice() ?: return
        MaterialAlertDialogBuilder(requireContext())
            .setMessage(text)
            .setPositiveButton(android.R.string.ok, null)
            .showPrivate()
    }

    override fun onRestarting() = requireActivity().finish()

    override fun onExitAll() = requireActivity().finish()

    // ---- Display ----

    private fun render() {
        val s = service
        if (s == null) {
            for (row in listOf(distro, reset, account, tier, runPolicy, fileAccess, privacy,
                fillWide, cursorStyle, cursorBlink, volumeKeys, tabList)) row.isEnabled = false
            return
        }
        val running = s.machine != 0L
        val sw = s.switching
        val installed = Rootfs.installed(requireContext())
        val idle = running && sw == null
        distro.set(
            when {
                sw != null -> getString(R.string.switch_downloading, sw.distro.label, sw.done shr 20, sw.total shr 20)
                running || s.startError != null -> installed?.label.orEmpty()
                else -> s.progress ?: getString(R.string.starting)
            },
            sw != null || s.canSwitch,
        )
        // Reset is possible even when the start failed: it is the only way to recover a broken
        // store
        reset.set(getString(R.string.reset_summary), s.canSwitch)
        tier.set(getString(if (s.tier == 0) R.string.tier_0_summary else R.string.tier_1_summary), idle)
        account.entries = (listOf(getString(R.string.login_auto_choice), ConsolePty.LOGIN_ROOT) + accounts.map { it.name }).toTypedArray()
        account.entryValues = (listOf(ConsolePty.LOGIN_AUTO, ConsolePty.LOGIN_ROOT) + accounts.map { it.name }).toTypedArray()
        account.value = s.login
        account.set(loginSummary(s.login), installed != null && sw == null)
        // Can be changed at any time: while the kernel is not running it is only recorded, and the
        // lock is held accordingly once it is up
        runPolicy.value = s.runPolicy.value
        runPolicy.set(getString(s.runPolicy.summary), true)
        val privacyEnabled = Settings.privacyMode(requireContext())
        privacy.set(getString(R.string.privacy_summary), true, privacyEnabled)
        fileAccess.set(getString(if (privacyEnabled) R.string.privacy_file_access else R.string.file_access_summary), !privacyEnabled, s.fileAccess)
        fillWide.set(getString(if (s.fillWide) R.string.fill_wide_on else R.string.fill_wide_off), true, s.fillWide)
        cursorStyle.value = s.cursorStyle.toString()
        cursorStyle.set(getString(CURSOR_LABELS[CURSOR_STYLES.indexOf(s.cursorStyle).coerceAtLeast(0)]), true)
        cursorBlink.set(getString(if (s.cursorBlink) R.string.cursor_blink_on else R.string.cursor_blink_off), true, s.cursorBlink)
        volumeKeys.set(
            getString(if (s.volumeKeysSwitchTabs) R.string.volume_keys_on else R.string.volume_keys_off),
            true,
            s.volumeKeysSwitchTabs,
        )
        tabList.set(getString(if (s.showTabList) R.string.tab_list_on else R.string.tab_list_off), true, s.showTabList)
        allowStateRestoration()
    }

    private fun allowStateRestoration() {
        if (service != null && insetsApplied && view != null) {
            listView.adapter?.stateRestorationPolicy = RecyclerView.Adapter.StateRestorationPolicy.ALLOW
        }
    }

    private fun Preference.set(text: CharSequence, enabled: Boolean) {
        summary = text
        isEnabled = enabled
    }

    private fun SwitchPreferenceCompat.set(text: CharSequence, enabled: Boolean, checked: Boolean) {
        set(text, enabled)
        isChecked = checked
    }

    private fun loginSummary(login: String): String = when (login) {
        ConsolePty.LOGIN_AUTO -> getString(R.string.login_auto, accounts.firstOrNull()?.name ?: ConsolePty.LOGIN_ROOT)
        ConsolePty.LOGIN_ROOT -> login
        else -> if (accounts.any { it.name == login }) login else getString(R.string.login_missing, login)
    }

    private fun loadAccounts() {
        val context = requireContext().applicationContext
        val activity = requireActivity()
        val generation = ++accountLoad
        thread(name = "tuxport-accounts") {
            val list = Rootfs.regularAccounts(context)
            activity.runOnUiThread {
                if (generation == accountLoad && service != null) {
                    accounts = list
                    render()
                }
            }
        }
    }

    // ---- Actions ----

    private fun onPrivacyMode(enabled: Boolean) {
        if (enabled) {
            MaterialAlertDialogBuilder(requireContext())
                .setTitle(R.string.settings_privacy)
                .setMessage(R.string.privacy_enable_message)
                .setPositiveButton(R.string.file_access_enable) { _, _ -> Privacy.requestMode(requireActivity(), true) }
                .setNegativeButton(android.R.string.cancel, null)
                .showPrivate()
        } else {
            Privacy.requestMode(requireActivity(), false)
        }
    }

    private fun confirmFileAccess() {
        MaterialAlertDialogBuilder(requireContext())
            .setTitle(R.string.file_access_title)
            .setMessage(R.string.file_access_message)
            .setPositiveButton(R.string.file_access_enable) { _, _ -> service?.setFileAccess(true) }
            .setNegativeButton(android.R.string.cancel, null)
            .showPrivate()
    }

    private fun onDistro() {
        val s = service ?: return
        s.switching?.let { sw ->
            MaterialAlertDialogBuilder(requireContext())
                .setTitle(getString(R.string.cancel_download_title, sw.distro.label))
                .setPositiveButton(R.string.cancel_download) { _, _ -> s.cancelSwitch() }
                .setNegativeButton(R.string.keep_downloading, null)
                .showPrivate()
            return
        }
        val distros = Rootfs.Distro.entries
        val installed = Rootfs.installed(requireContext())
        val labels = distros.map { it.label }
        MaterialAlertDialogBuilder(requireContext())
            .setTitle(R.string.settings_distro)
            .setSingleChoiceItems(labels.toTypedArray(), distros.indexOf(installed)) { dialog, i ->
                dialog.dismiss()
                if (distros[i] != installed) confirmSwitch(distros[i], getString(R.string.switch_title, distros[i].label))
            }
            .setNegativeButton(android.R.string.cancel, null)
            .showPrivate()
    }

    /** When the very first import failed (no store), resets to Alpine, as on the first run. */
    private fun onReset() {
        val installed = Rootfs.installed(requireContext()) ?: Rootfs.Distro.ALPINE
        confirmSwitch(installed, getString(R.string.reset_title))
    }

    /**
     * Confirms before switching to [distro] (a reset switches to the installed one): every file in
     * the rootfs is deleted.
     */
    private fun confirmSwitch(distro: Rootfs.Distro, title: String) {
        val download = distro.download
        val message = if (download != null) {
            getString(R.string.switch_message_download, distro.label, download.size shr 20)
        } else {
            getString(R.string.switch_message, distro.label)
        }
        MaterialAlertDialogBuilder(requireContext())
            .setTitle(title)
            .setMessage(message)
            .setPositiveButton(R.string.switch_confirm) { _, _ -> service?.switchTo(distro) }
            .setNegativeButton(android.R.string.cancel, null)
            .showPrivate()
    }

    private fun onTier() {
        val s = service ?: return
        val tiers = intArrayOf(1, 0)
        val labels = arrayOf(getString(R.string.tier_1), getString(R.string.tier_0))
        MaterialAlertDialogBuilder(requireContext())
            .setTitle(R.string.settings_tier)
            .setSingleChoiceItems(labels, tiers.indexOf(s.tier)) { dialog, i ->
                dialog.dismiss()
                if (tiers[i] == s.tier) return@setSingleChoiceItems
                MaterialAlertDialogBuilder(requireContext())
                    .setTitle(R.string.restart_title)
                    .setMessage(R.string.restart_message)
                    .setPositiveButton(R.string.restart) { _, _ -> service?.setTier(tiers[i]) }
                    .setNegativeButton(android.R.string.cancel, null)
                    .showPrivate()
            }
            .setNegativeButton(android.R.string.cancel, null)
            .showPrivate()
    }

    private fun onVersion() {
        val intent = Intent(Intent.ACTION_VIEW, RELEASES_URL.toUri())
        try {
            startActivity(intent)
        } catch (e: ActivityNotFoundException) {
            Toast.makeText(requireContext(), R.string.link_no_app, Toast.LENGTH_SHORT).show()
        }
    }

    /** While a package downloads, the row asks whether to stop it; otherwise a check begins. */
    private fun onUpdate() {
        if (updateDownloading) {
            MaterialAlertDialogBuilder(requireContext())
                .setTitle(getString(R.string.cancel_download_title, updateVersion))
                .setPositiveButton(R.string.cancel_download) { _, _ -> updateCancelled = true }
                .setNegativeButton(R.string.keep_downloading, null)
                .showPrivate()
            return
        }
        val owner = requireActivity()
        val session = ++updateSession
        update.isEnabled = false
        update.setSummary(R.string.update_checking)
        thread(name = "tuxport-update") {
            val result = runCatching { Updater.latest() }
            owner.runOnUiThread {
                if (!isAdded || session != updateSession) return@runOnUiThread
                update.isEnabled = true
                result.fold(onSuccess = { release -> showUpdate(release) }, onFailure = {
                    update.setSummary(R.string.update_summary)
                    Toast.makeText(owner, R.string.update_check_failed, Toast.LENGTH_LONG).show()
                })
            }
        }
    }

    /** A release newer than the running one is offered; any other answers "up to date". */
    private fun showUpdate(release: Updater.Release) {
        if (!Updater.newer(release.version, BuildConfig.VERSION_NAME)) {
            update.setSummary(R.string.update_up_to_date)
            return
        }
        updateVersion = release.version
        update.setSummary(getString(R.string.update_available, release.version))
        MaterialAlertDialogBuilder(requireContext())
            .setTitle(getString(R.string.update_available, release.version))
            .setMessage(getString(R.string.update_available_message, release.download.size shr 20))
            .setPositiveButton(R.string.update_download) { _, _ -> downloadUpdate(release) }
            .setNegativeButton(android.R.string.cancel, null)
            .showPrivate()
    }

    /**
     * Downloads the package with [Rootfs.fetch] and moves it into the Downloads folder. The row
     * keeps the download's progress and stays enabled, so tapping it offers to stop the download.
     */
    private fun downloadUpdate(release: Updater.Release) {
        val owner = requireActivity()
        val context = owner.applicationContext
        val session = updateSession
        updateDownloading = true
        updateCancelled = false
        val packageFile = File(context.cacheDir, release.name)
        thread(name = "tuxport-update") {
            val result = runCatching {
                Rootfs.fetch(
                    release.download, packageFile, { updateCancelled },
                ) { done, total ->
                    val text = owner.getString(R.string.switch_downloading, release.version, done shr 20, total shr 20)
                    owner.runOnUiThread {
                        if (isAdded && session == updateSession) update.setSummary(text)
                    }
                }
                Updater.intoDownloads(context, packageFile, release.name)
            }
            owner.runOnUiThread {
                updateDownloading = false
                if (updateCancelled) {
                    if (isAdded && session == updateSession) update.setSummary(R.string.update_summary)
                    return@runOnUiThread
                }
                // The package reaches the Downloads folder even when this page is gone; say so
                // wherever the user now is.
                result.fold(onSuccess = { uri ->
                    if (isAdded && session == updateSession) offerPackage(uri, release.version)
                    else Toast.makeText(context, R.string.update_downloaded_message, Toast.LENGTH_LONG).show()
                }, onFailure = {
                    if (isAdded && session == updateSession) update.setSummary(R.string.update_summary)
                    Toast.makeText(context, R.string.update_download_failed, Toast.LENGTH_LONG).show()
                })
            }
        }
    }

    /** Points the installer at the downloaded package; Android then asks for the app to be allowed. */
    private fun offerPackage(uri: Uri, version: String) {
        MaterialAlertDialogBuilder(requireContext())
            .setTitle(getString(R.string.update_downloaded_title, version))
            .setMessage(R.string.update_downloaded_message)
            .setPositiveButton(R.string.update_open) { _, _ ->
                try {
                    startActivity(Intent(Intent.ACTION_VIEW).apply {
                        setDataAndType(uri, "application/vnd.android.package-archive")
                        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                    })
                } catch (_: ActivityNotFoundException) {
                    Toast.makeText(requireContext(), R.string.update_no_installer, Toast.LENGTH_SHORT).show()
                }
            }
            .setNegativeButton(android.R.string.ok, null)
            .showPrivate()
    }

    companion object {
        private const val SUPPORT_URL = "https://github.com/sajadxdp0/tuxport/issues"
        private const val PROJECT_URL = "https://github.com/sajadxdp0/tuxport"
        private const val PRIVACY_POLICY_URL =
            "https://sajadxdp0.github.io/tuxport/privacy.html"
        private const val RELEASES_URL = "https://github.com/sajadxdp0/tuxport/releases"
        private const val CHOICE_DIALOG = "settings.choice"

        /** The cursor shapes offered, in order, and their names. */
        private val CURSOR_STYLES = intArrayOf(VtNative.CURSOR_BLOCK, VtNative.CURSOR_BAR, VtNative.CURSOR_UNDERLINE)
        private val CURSOR_LABELS = intArrayOf(R.string.cursor_block, R.string.cursor_bar, R.string.cursor_underline)
    }
}

/** Retain one authorized archive across picker recreation, never across process death. */
internal class PendingLogSave : ViewModel() {
    var saveRequested = false
    private var archive: File? = null
    val pending: Boolean get() = archive != null

    fun stage(file: File) {
        check(archive == null) { "A log save is already pending" }
        archive = file
    }

    fun take(): File? = archive.also { archive = null }

    override fun onCleared() {
        take()?.delete()
    }
}
