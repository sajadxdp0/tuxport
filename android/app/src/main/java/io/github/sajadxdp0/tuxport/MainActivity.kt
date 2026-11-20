package io.github.sajadxdp0.tuxport

import android.annotation.SuppressLint
import android.content.ActivityNotFoundException
import android.content.ClipData
import android.content.ClipboardManager
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.res.Configuration
import android.os.Bundle
import android.os.IBinder
import android.text.SpannableStringBuilder
import android.view.Gravity
import android.view.HapticFeedbackConstants
import android.view.KeyEvent
import android.view.KeyboardShortcutGroup
import android.view.KeyboardShortcutInfo
import android.view.Menu
import android.view.View
import android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON
import android.widget.Toast
import androidx.activity.OnBackPressedCallback
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.view.ContextThemeWrapper
import androidx.appcompat.widget.PopupMenu
import androidx.core.content.edit
import androidx.core.view.WindowCompat
import androidx.core.view.isVisible
import androidx.fragment.app.FragmentActivity
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.ViewModelProvider
import com.google.android.material.color.DynamicColors
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import io.github.sajadxdp0.tuxport.MainActivity.Companion.PREF_FONT_SIZE

/**
 * The terminal screen: the tab bar ([TabBarView]) on top, the terminal ([VtView]) in the middle,
 * and below it the keys the soft keyboard lacks ([ExtraKeysView]: Esc, Ctrl, arrows and so on).
 * Sessions belong to [TerminalService]; this only handles display and input: the bar shows every
 * tab, the view the current tab's [VtSession]. While the current tab is searched, the search bar
 * ([SearchBarView]) takes the tab bar's place. [TerminalLayout] builds the views and handles
 * insets and the IME animation; [TerminalLinks] opens links from the terminal.
 *
 * Its dialogs and menus are Material 3 (`Theme.Tuxport.Terminal`, always dark); the tab bar and the
 * extra keys are black like the terminal, with labels in the theme's colours. The activity itself
 * uses FragmentActivity for AndroidX authentication, retaining framework view inflation.
 */
class MainActivity : FragmentActivity(), TerminalService.Listener, Privacy.Listener {

    private lateinit var layout: TerminalLayout
    private val links = TerminalLinks(this, ::showToast)
    private val files by lazy { ViewModelProvider(this)[TerminalFiles::class.java] }
    private var importDialog: AlertDialog? = null
    private val pickFiles = registerForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        files.selected(uris)
    }
    private val requestNotifications = registerForActivityResult(ActivityResultContracts.RequestPermission()) {
        updatePermissionPrompts()
    }

    private var service: TerminalService? = null
    private var current: VtSession? = null

    /** [current]'s position when the tabs were last shown, where its successor is taken from. */
    private var currentIndex = 0

    /**
     * The tab a notification asked to show ([EXTRA_TAB], a [VtSession.id]), -1 for none; shown
     * once the service is bound and the kernel ready ([showRequestedTab]).
     */
    private var requestedTab = -1

    /** The last toast ([showToast]), cancelled by the next. */
    private var toast: Toast? = null

    /**
     * The key of the shortcut or volume key last taken ([dispatchKeyEvent]), whose release is
     * taken too.
     */
    private var shortcutKey = 0

    /** The saved menu choice; Android honours the flag only while this window is visible. */
    private var keepScreenOn = false
        set(value) {
            field = value
            window.setFlags(if (value) FLAG_KEEP_SCREEN_ON else 0, FLAG_KEEP_SCREEN_ON)
        }

    private val closeSearchOnBack = object : OnBackPressedCallback(false) {
        override fun handleOnBackPressed() { closeSearch() }
    }

    private val closeTabListOnBack = object : OnBackPressedCallback(false) {
        override fun handleOnBackPressed() { closeTabList() }
    }

    /**
     * The question whether a program may read the clipboard ([onClipboardRead]) while it is open,
     * which answers the reads that come meanwhile too.
     */
    private var clipboardQuestion: AlertDialog? = null

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            val s = (binder as TerminalService.LocalBinder).service
            service = s
            s.addListener(this@MainActivity)
            when {
                s.machine != 0L || s.startError != null -> onMachineReady(s.startError)
                s.progress != null -> onProgress(s.progress!!)
            }
            onNotice()
        }

        override fun onServiceDisconnected(name: ComponentName) {
            service = null
        }
    }

    override fun attachBaseContext(newBase: Context) {
        super.attachBaseContext(newBase)
        // System selection menus use night-qualified colours as well as the theme's dark palette.
        // Override only night mode, before the window or dynamic colours access resources.
        applyOverrideConfiguration(Configuration().apply { uiMode = Configuration.UI_MODE_NIGHT_YES })
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        DynamicColors.applyToActivityIfAvailable(this)
        super.onCreate(savedInstanceState)
        onBackPressedDispatcher.addCallback(this, closeSearchOnBack)
        onBackPressedDispatcher.addCallback(this, closeTabListOnBack)
        NotificationPermission.initialize(this)
        keepScreenOn = getPreferences(MODE_PRIVATE).getBoolean(PREF_KEEP_SCREEN_ON, false)
        // A notification's tab only when it starts this activity: one recreated (a configuration
        // change, or the system bringing it back after killing the app) or opened from Recents
        // still has the intent that first started it, whose tab the user has left since
        if (savedInstanceState == null && intent.flags and Intent.FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY == 0) {
            requestedTab = intent.getIntExtra(EXTRA_TAB, -1)
        }
        layout = TerminalLayout(
            this, savedFontSize(), Host(), Bar(), Tabs(), Find(), Keys(),
            enableNotifications = ::enableNotifications,
            dismissNotificationPrompt = { dismissPrompt(PREF_DISMISSED_NOTIFICATION_PROMPT) },
            requestBatteryExemption = { BatteryOptimization.requestExemption(this) },
            dismissBatteryPrompt = { dismissPrompt(PREF_DISMISSED_BATTERY_PROMPT) },
        )
        updatePermissionPrompts()
        renderTabs()
        WindowCompat.setDecorFitsSystemWindows(window, false)
        setContentView(layout.root)
        files.changes.observe(this) { updateImport() }
    }

    /** A notification of a tab was tapped while this activity exists (it is `singleTask`). */
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        requestedTab = intent.getIntExtra(EXTRA_TAB, -1)
        showRequestedTab()
    }

    override fun onStart() {
        super.onStart()
        // Settings may have changed while this activity was stopped
        onSettingsChanged()
        val intent = Intent(this, TerminalService::class.java)
        startForegroundService(intent)
        bindService(intent, connection, BIND_AUTO_CREATE)
    }

    override fun onResume() {
        super.onResume()
        updatePermissionPrompts()
    }

    override fun onPause() {
        closeTabList()
        if (Settings.privacyMode(this)) {
            toast?.cancel()
            clipboardQuestion?.dismiss()
        }
        super.onPause()
    }

    override fun onStop() {
        service?.removeListener(this)
        unbindService(connection)
        service = null
        super.onStop()
    }

    override fun onDestroy() {
        importDialog?.setOnDismissListener(null)
        importDialog?.dismiss()
        // Sessions outlive the Activity: do not leave them pointing at this view
        layout.screen.attach(null)
        super.onDestroy()
    }

    // ---- Privacy.Listener ----

    /** Called by the shared gate only when this resumed page can be used. */
    override fun onPrivacyUnlocked() {
        updateImport()
        onNotice()
    }

    // ---- Permission prompts ----

    private fun updatePermissionPrompts() {
        val preferences = getPreferences(MODE_PRIVATE)
        layout.notificationPrompt.isVisible = !NotificationPermission.enabled(this) &&
            !preferences.getBoolean(PREF_DISMISSED_NOTIFICATION_PROMPT, false)
        layout.batteryPrompt.isVisible = !BatteryOptimization.isExempt(this) &&
            !preferences.getBoolean(PREF_DISMISSED_BATTERY_PROMPT, false)
    }

    private fun dismissPrompt(key: String) {
        if (Privacy.locked(this)) return
        getPreferences(MODE_PRIVATE).edit { putBoolean(key, true) }
        updatePermissionPrompts()
    }

    private fun enableNotifications() {
        if (Privacy.locked(this)) return
        closeTabList()
        NotificationPermission.enable(this, requestNotifications)
    }

    // ---- TerminalService.Listener ----

    override fun onProgress(text: String) {
        layout.showStatus(text)
    }

    override fun onMachineReady(error: String?) {
        val s = service ?: return
        if (error != null) {
            layout.showStatus(getString(R.string.start_failed, error))
            return
        }
        layout.showTerminal()
        if (showRequestedTab()) return
        if (s.sessions.isEmpty()) {
            newSession()
        } else {
            // The tab this activity shows; for a new one (recreated, or opened again after Back
            // finished the last one), the tab the last one showed; after tabs were restored from
            // a kill, the one that was being viewed then; else the last
            show(
                current?.takeIf { it in s.sessions } ?: s.current?.takeIf { it in s.sessions }
                    ?: s.focus?.takeIf { it in s.sessions } ?: s.sessions.last(),
            )
        }
    }

    override fun onSessionsChanged() {
        val s = service ?: return
        if (current != null && current !in s.sessions) {
            // The current tab was closed: show the one after it, now in its place (the one before
            // when it was the last)
            current = null
            s.sessions.getOrNull(currentIndex.coerceAtMost(s.sessions.lastIndex))?.let { show(it, keepTabList = true) }
        }
        if (s.sessions.isEmpty() && s.machine != 0L) {
            // The last tab was closed: the UI exits. The service stays in the background while it
            // still has guest processes (services started by OpenRC and so on), and exits by itself
            // when there are none (the android/service map)
            finish()
            return
        }
        renderTabs()
    }

    /** The program in the tab shown rang the bell (the service sends only those). */
    override fun onBell() = layout.screen.bell()

    override fun onOpenUrl(url: String): Boolean = !Privacy.locked(this) && hasWindowFocus() && links.openBrowser(url)

    /**
     * A program in the tab shown asks to read the clipboard (OSC 52): the user is asked, with what
     * it holds shown, as Ghostty's clipboard-read = ask does; nothing to read (an empty clipboard,
     * or no text) is answered at once. Denying, or leaving the question unanswered, gives the
     * program an empty clipboard.
     */
    override fun onClipboardRead() {
        val session = current ?: return
        if (Privacy.locked(this)) {
            session.answerClipboardRead(null)
            return
        }
        if (clipboardQuestion != null) return
        val clip = getSystemService(ClipboardManager::class.java).primaryClip?.takeIf { it.itemCount > 0 }
        val text = clip?.getItemAt(0)?.coerceToText(this)?.toString().orEmpty()
        if (text.isEmpty()) {
            session.answerClipboardRead("")
            return
        }
        confirmClipboardRead(session, text)
    }

    /** A tab's reports changed: refresh the bar, the open list and the progress line. */
    override fun onTabStatus() {
        layout.tabBar.refreshStatus()
        layout.tabList.refreshStatus()
        layout.progressLine.show(current?.progress, jump = false)
    }

    /** Settings changed while this activity is visible too (split screen). */
    override fun onSettingsChanged() {
        layout.screen.setFillWide(Settings.fillWide(this))
        val showTabList = Settings.showTabList(this)
        if (!showTabList) closeTabList()
        layout.tabBar.setListVisible(showTabList)
    }

    /**
     * A distro switch failed (the download failed, or the import failed and fell back to Alpine).
     */
    override fun onNotice() {
        if (Privacy.locked(this)) return
        val text = service?.takeNotice() ?: return
        MaterialAlertDialogBuilder(this)
            .setMessage(text)
            .setPositiveButton(android.R.string.ok, null)
            .showPrivate()
    }

    override fun onRestarting() {
        closeTabList()
        closeSearch()
        current = null
        layout.progressLine.show(null, jump = true)
        layout.screen.attach(null)
        layout.showStatus(getString(R.string.restarting))
    }

    override fun onExitAll() {
        finish()
    }

    // ---- Clipboard and notices ----

    /** Answers the requesting session even if another tab becomes current while the dialog is open. */
    private fun confirmClipboardRead(session: VtSession, text: String) {
        val preview = if (text.length > CLIPBOARD_PREVIEW) text.take(CLIPBOARD_PREVIEW) + "…" else text
        var allowed = false
        clipboardQuestion = MaterialAlertDialogBuilder(this)
            .setTitle(R.string.clipboard_read_title)
            .setMessage(getString(R.string.clipboard_read, preview))
            .setPositiveButton(R.string.allow) { _, _ -> allowed = true }
            .setNegativeButton(R.string.deny, null)
            .setOnDismissListener {
                clipboardQuestion = null
                session.answerClipboardRead(if (allowed && !Privacy.locked(this)) text else null)
                // The terminal takes the keyboard again
                layout.screen.requestFocus()
            }
            .showPrivate()
    }

    /**
     * A notice from the extra keys, links or file import, replacing the previous toast;
     * [long] for text that takes a while to read.
     */
    private fun showToast(text: String, long: Boolean = false) {
        if (Privacy.locked(this)) return
        toast?.cancel()
        toast = Toast.makeText(this, text, if (long) Toast.LENGTH_LONG else Toast.LENGTH_SHORT).apply { show() }
    }

    // ---- Importing files ----

    private fun importFiles() {
        val s = service ?: return
        if (Privacy.locked(this) || s.machine == 0L) return
        if (!files.begin(Settings.login(this))) return
        try {
            pickFiles.launch(arrayOf("*/*"))
        } catch (_: ActivityNotFoundException) {
            files.selected(emptyList())
            showToast(getString(R.string.import_picker_missing), long = true)
        }
    }

    private fun updateImport() {
        layout.tabBar.setImportEnabled(!files.occupied && (service?.machine ?: 0L) != 0L)
        if (!lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED) || Privacy.locked(this)) return
        if (files.importSelected()) showToast(getString(R.string.importing_files))
        val result = files.result ?: return
        if (importDialog == null) showImportResult(result)
    }

    private fun showImportResult(result: TerminalFiles.Result) {
        val message = SpannableStringBuilder().apply {
            if (result.paths.isNotEmpty()) {
                append(monospaceMessage(R.string.import_files_saved, result.paths.joinToString("\n")))
            }
            if (result.failures.isNotEmpty()) {
                if (isNotEmpty()) append("\n\n")
                append(result.failures.joinToString("\n"))
            }
        }
        val dialog = MaterialAlertDialogBuilder(this)
            .setTitle(R.string.import_files)
            .setMessage(message)
            .setNegativeButton(R.string.close, null)
            .setOnDismissListener {
                importDialog = null
                files.dismissResult()
                layout.screen.requestFocus()
            }
        if (result.paths.isNotEmpty()) {
            dialog.setPositiveButton(R.string.copy_paths) { _, _ ->
                if (!Privacy.locked(this)) {
                    getSystemService(ClipboardManager::class.java).setPrimaryClip(
                        ClipData.newPlainText(getString(R.string.import_files), TerminalFiles.clipboardPaths(result.paths)),
                    )
                }
            }
        }
        importDialog = dialog.showPrivate()
    }

    // ---- Sessions ----

    /**
     * Opens a tab in the current tab's directory (that of the program in its foreground, the shell
     * at its prompt), as Ghostty's new tabs inherit the working directory; with no current tab, or
     * one whose shell has ended, in the home directory.
     */
    private fun newSession() {
        val s = service ?: return
        if (s.machine == 0L) return
        show(s.createSession(cwd = current?.cwd()))
    }

    /**
     * Shows the tab a notification named ([requestedTab]) once the kernel is ready; false when
     * there is none, or it has closed.
     */
    private fun showRequestedTab(): Boolean {
        val s = service ?: return false
        if (requestedTab < 0 || s.machine == 0L) return false
        val session = s.sessions.firstOrNull { it.id == requestedTab }
        requestedTab = -1
        session?.let { show(it) }
        return session != null
    }

    private fun show(session: VtSession, keepTabList: Boolean = false) {
        if (!keepTabList) closeTabList()
        // A search belongs to the tab it was opened in
        closeSearch()
        current = session
        layout.progressLine.show(session.progress, jump = true)
        service?.current = session
        // Starts the shell once the view knows the grid (Host.onGrid)
        layout.screen.attach(session)
        layout.screen.requestFocus()
        renderTabs()
    }

    /**
     * Shows the tabs in the bar, the current one marked ([TabBarView.render]). Called whenever
     * tabs, titles or the current tab change.
     */
    private fun renderTabs() {
        val sessions = service?.sessions.orEmpty().toList()
        val shown = current?.takeIf { it in sessions }
        if (shown != null) currentIndex = sessions.indexOf(shown)
        layout.tabBar.render(sessions, shown)
        if (layout.tabList.isVisible) {
            if (sessions.isEmpty()) closeTabList() else layout.tabList.render(sessions, shown)
        }
        updateImport()
    }

    private fun toggleTabList() {
        if (layout.tabList.isVisible) {
            closeTabList()
            return
        }
        if (Privacy.locked(this)) return
        val sessions = service?.sessions.orEmpty().toList()
        if (sessions.isEmpty()) return
        layout.tabList.open(sessions, current)
        layout.updateTabListControls()
        closeTabListOnBack.isEnabled = true
    }

    private fun closeTabList() {
        if (!layout.tabList.isVisible) return
        val hadFocus = layout.tabList.hasFocus()
        layout.tabList.close()
        layout.updateTabListControls()
        closeTabListOnBack.isEnabled = false
        if (hadFocus) layout.screen.requestFocus()
    }

    /**
     * Shows the tab [delta] places after the current one, going round from the last to the first;
     * false when there is no other tab to show.
     */
    private fun switchBy(delta: Int): Boolean {
        val sessions = service?.sessions ?: return false
        val i = sessions.indexOf(current)
        if (i < 0 || sessions.size < 2) return false
        show(sessions[Math.floorMod(i + delta, sessions.size)])
        return true
    }

    // ---- The menu and closing tabs ----

    /**
     * `⋮`: finding text in the current tab (none: disabled), clearing its screen (also disabled on
     * the alternate screen, where Ghostty's clear_screen does nothing) or resetting its terminal
     * (disabled before its shell starts), closing the other tabs (none other: disabled) or all of
     * them, keeping the screen on, and Settings.
     */
    private fun showMenu(anchor: View) {
        closeTabList()
        val sessions = service?.sessions.orEmpty().toList()
        val others = sessions.filter { it != current }
        val context = ContextThemeWrapper(this, R.style.ThemeOverlay_Tuxport_CheckPopup)
        PopupMenu(context, anchor, Gravity.END, 0, R.style.Widget_Tuxport_MorePopup).apply {
            menu.add(R.string.find).apply { isEnabled = current != null }.setOnMenuItemClickListener {
                openSearch()
                true
            }
            val terminal = current?.takeIf { it.tab != 0L }
            menu.add(R.string.clear_screen).apply { isEnabled = terminal != null && !layout.screen.altScreen }
                .setOnMenuItemClickListener {
                    terminal?.clear()
                    true
                }
            menu.add(R.string.reset_terminal).apply { isEnabled = terminal != null }.setOnMenuItemClickListener {
                terminal?.reset()
                true
            }
            menu.add(R.string.close_other_tabs).apply { isEnabled = others.isNotEmpty() }.setOnMenuItemClickListener {
                confirmClose(others)
                true
            }
            menu.add(R.string.close_all_tabs).apply { isEnabled = sessions.isNotEmpty() }.setOnMenuItemClickListener {
                confirmClose(sessions)
                true
            }
            menu.add(R.string.keep_screen_on).apply {
                isCheckable = true
                isChecked = keepScreenOn
            }.setOnMenuItemClickListener { item ->
                keepScreenOn = !item.isChecked
                item.isChecked = keepScreenOn
                getPreferences(MODE_PRIVATE).edit { putBoolean(PREF_KEEP_SCREEN_ON, keepScreenOn) }
                true
            }
            menu.add(R.string.menu_settings).setOnMenuItemClickListener {
                startActivity(Intent(this@MainActivity, SettingsActivity::class.java))
                true
            }
            show()
        }
    }

    /**
     * `×` and the menu: closes [targets] at once when none is [busy][VtSession.busy] (their shells
     * have ended or wait at their prompts), else after asking, as closing hangs a shell up and with
     * it, usually, the programs it runs. A shell without prompt marks always counts as busy.
     */
    private fun confirmClose(targets: List<VtSession>) {
        if (targets.none { it.busy }) {
            close(targets)
            return
        }
        val one = targets.size == 1
        MaterialAlertDialogBuilder(this)
            .setTitle(
                if (one) getString(R.string.close_tab_title)
                else resources.getQuantityString(R.plurals.close_tabs_title, targets.size, targets.size)
            )
            .setMessage(if (one) R.string.close_tab_message else R.string.close_tabs_message)
            .setPositiveButton(R.string.close) { _, _ ->
                // Some may have been closed meanwhile (their kernel restarting)
                close(targets.filter { service?.sessions?.contains(it) == true })
            }
            .setNegativeButton(android.R.string.cancel, null)
            // The terminal takes the keyboard again
            .setOnDismissListener { layout.screen.requestFocus() }
            .showPrivate()
    }

    /**
     * Closes [targets], the current tab last: closed earlier, it would have another tab shown (and
     * its shell started) only to close that one too.
     */
    private fun close(targets: List<VtSession>) {
        targets.sortedBy { it == current }.forEach { close(it) }
    }

    private fun close(session: VtSession) {
        service?.removeSession(session)
    }

    // ---- Finding text ----

    /**
     * Puts the search bar in the tab bar's place, with the keyboard on it, to search the current
     * tab (the same height, so the terminal keeps its rows); Back closes it.
     */
    private fun openSearch() {
        if (current == null) return
        closeTabList()
        layout.tabBar.isVisible = false
        layout.searchBar.open()
        closeSearchOnBack.isEnabled = true
    }

    /** Ends the search and brings the tab bar back; the terminal takes the keyboard again. */
    private fun closeSearch() {
        if (!layout.searchBar.isVisible) return
        layout.searchBar.isVisible = false
        layout.tabBar.isVisible = true
        layout.screen.endSearch()
        layout.screen.requestFocus()
        closeSearchOnBack.isEnabled = false
    }

    // ---- Keyboard shortcuts ----

    // FragmentActivity inherits this public framework override from the restricted core base.
    @SuppressLint("RestrictedApi")
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (Privacy.locked(this)) return true
        if (event.keyCode == KeyEvent.KEYCODE_ESCAPE && (layout.tabList.isVisible || shortcutKey == KeyEvent.KEYCODE_ESCAPE)) {
            if (event.action == KeyEvent.ACTION_DOWN) {
                closeTabList()
                shortcutKey = event.keyCode
            } else if (event.action == KeyEvent.ACTION_UP) shortcutKey = 0
            return true
        }
        if (event.action == KeyEvent.ACTION_DOWN && (shortcut(event) || volumeKey(event))) {
            shortcutKey = event.keyCode
            return true
        }
        // Its release too, or the terminal would send a release (Kitty protocol) for a key it never
        // saw pressed
        if (event.action == KeyEvent.ACTION_UP && event.keyCode == shortcutKey) {
            shortcutKey = 0
            return true
        }
        return super.dispatchKeyEvent(event)
    }

    /** Lists the shortcuts in the system's keyboard shortcuts screen (Meta+/). */
    override fun onProvideKeyboardShortcuts(data: MutableList<KeyboardShortcutGroup>, menu: Menu?, deviceId: Int) {
        val ctrl = KeyEvent.META_CTRL_ON
        val ctrlShift = KeyEvent.META_CTRL_ON or KeyEvent.META_SHIFT_ON
        data.add(
            KeyboardShortcutGroup(
                getString(R.string.shortcuts_tabs),
                listOf(
                    KeyboardShortcutInfo(getString(R.string.new_tab), KeyEvent.KEYCODE_T, ctrlShift),
                    KeyboardShortcutInfo(getString(R.string.close_tab), KeyEvent.KEYCODE_W, ctrlShift),
                    KeyboardShortcutInfo(getString(R.string.next_tab), KeyEvent.KEYCODE_TAB, ctrl),
                    KeyboardShortcutInfo(getString(R.string.previous_tab), KeyEvent.KEYCODE_TAB, ctrlShift),
                ),
            ),
        )
        data.add(
            KeyboardShortcutGroup(
                getString(R.string.shortcuts_terminal),
                listOf(
                    KeyboardShortcutInfo(getString(R.string.find), KeyEvent.KEYCODE_F, ctrlShift),
                    KeyboardShortcutInfo(getString(R.string.previous_prompt), KeyEvent.KEYCODE_DPAD_UP, ctrlShift),
                    KeyboardShortcutInfo(getString(R.string.next_prompt), KeyEvent.KEYCODE_DPAD_DOWN, ctrlShift),
                ),
            ),
        )
    }

    /**
     * The shortcuts of a hardware keyboard, as in desktop terminals: Ctrl+Shift+T opens a tab,
     * Ctrl+Shift+W closes the current one, Ctrl+Tab and Ctrl+Shift+Tab show the next and the
     * previous one (going round), Ctrl+Shift+F finds text in it and Ctrl+Shift+↑ and ↓ scroll to
     * the previous and the next prompt (as in Ghostty on Linux; on the alternate screen, which has
     * no prompts, they go to the program). They are taken before the terminal sees them; false for
     * any other key.
     */
    private fun shortcut(event: KeyEvent): Boolean {
        if (!event.isCtrlPressed || event.isAltPressed || event.isMetaPressed) return false
        // Holding Ctrl+Tab or a jump keeps going; holding the others acts once
        val first = event.repeatCount == 0
        when {
            event.keyCode == KeyEvent.KEYCODE_TAB -> switchBy(if (event.isShiftPressed) -1 else 1)
            !event.isShiftPressed -> return false
            event.keyCode == KeyEvent.KEYCODE_T -> if (first) newSession()
            event.keyCode == KeyEvent.KEYCODE_W -> if (first) current?.let { close(it) }
            event.keyCode == KeyEvent.KEYCODE_F -> if (first) openSearch()
            event.keyCode == KeyEvent.KEYCODE_DPAD_UP -> return layout.screen.jumpToPrompt(-1)
            event.keyCode == KeyEvent.KEYCODE_DPAD_DOWN -> return layout.screen.jumpToPrompt(1)
            else -> return false
        }
        return true
    }

    /**
     * The volume keys switch tabs while Settings has them do so ([Settings.volumeKeysSwitchTabs], on
     * by default): up shows the previous tab and down the next, going round, once per press (a held
     * key's repeats are swallowed). With no other tab to show, or with the setting off, they are
     * left to the system, which changes the volume. False for any other key.
     */
    private fun volumeKey(event: KeyEvent): Boolean {
        val delta = when (event.keyCode) {
            KeyEvent.KEYCODE_VOLUME_UP -> -1
            KeyEvent.KEYCODE_VOLUME_DOWN -> 1
            else -> return false
        }
        // Repeats follow the press: swallowed after a switch, else the volume keeps changing
        if (event.repeatCount > 0) return shortcutKey == event.keyCode
        if (!Settings.volumeKeysSwitchTabs(this) || !switchBy(delta)) return false
        layout.screen.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
        return true
    }

    // ---- Font preferences ----

    /**
     * The terminal's font size in pixels. It is kept in dp, so that after a display size change
     * (which recreates this activity) the text keeps its size relative to the rest; a size kept in
     * pixels by earlier versions is taken at the current density and kept in dp from then on.
     */
    private fun savedFontSize(): Float {
        val prefs = getPreferences(MODE_PRIVATE)
        val density = resources.displayMetrics.density
        if (prefs.contains(PREF_FONT_SIZE_PX)) {
            val dp = prefs.getInt(PREF_FONT_SIZE_PX, 0) / density
            prefs.edit {
                putFloat(PREF_FONT_SIZE, dp)
                remove(PREF_FONT_SIZE_PX)
            }
        }
        return prefs.getFloat(PREF_FONT_SIZE, DEFAULT_FONT_DP) * density
    }

    // ---- The views' targets ----

    /** The [VtView]'s side: starting the shell, input, zoom and the extra keys' modifiers. */
    private inner class Host : VtView.Host {
        override fun onGrid(cols: Int, rows: Int, cellWidth: Int, cellHeight: Int) {
            current?.start(cols, rows, cellWidth, cellHeight)
        }

        override fun onInput(data: ByteArray) {
            if (Privacy.locked(this@MainActivity)) return
            val session = current ?: return
            if (session.isRunning) {
                session.write(data)
            } else if (data.any { it == '\r'.code.toByte() || it == '\n'.code.toByte() }) {
                // When the session has ended, Enter (from a key or the IME) closes it
                close(session)
            }
        }

        override fun onZoom(fontPx: Int) {
            getPreferences(MODE_PRIVATE).edit { putFloat(PREF_FONT_SIZE, fontPx / resources.displayMetrics.density) }
        }

        override val ctrl: Boolean get() = layout.keys.ctrl
        override val alt: Boolean get() = layout.keys.alt
        override val shift: Boolean get() = layout.keys.shift

        override fun modifiersUsed() = layout.keys.modifiersUsed()

        override fun onSearchCount(total: Int, selected: Int) = layout.searchBar.setCount(total, selected)

        override fun onLink(uri: String) = links.open(uri)
    }

    /** Taps on the tab bar: a tab, `⧉`, `×`, `+`, Import and `⋮`. */
    private inner class Bar : TabBarView.Target {
        override fun listTabs() = toggleTabList()

        override fun show(session: VtSession) = this@MainActivity.show(session)

        override fun closeCurrent() {
            closeTabList()
            current?.let { confirmClose(listOf(it)) }
        }

        override fun newTab() = newSession()

        override fun importFiles() {
            closeTabList()
            this@MainActivity.importFiles()
        }

        override fun menu(anchor: View) = showMenu(anchor)
    }

    /** List rows retain session identity while updates can change their positions. */
    private inner class Tabs : TabListView.Target {
        override fun show(session: VtSession) {
            if (canUse(session)) this@MainActivity.show(session)
        }

        override fun close(session: VtSession) {
            if (canUse(session)) confirmClose(listOf(session))
        }

        override fun dismiss() = closeTabList()

        private fun canUse(session: VtSession) =
            !Privacy.locked(this@MainActivity) && service?.sessions?.contains(session) == true
    }

    /** The search bar's text and buttons go to the view, which searches the current tab. */
    private inner class Find : SearchBarView.Target {
        override fun search(needle: String) = layout.screen.search(needle)

        override fun step(older: Boolean) = layout.screen.searchStep(older)

        override fun close() = closeSearch()
    }

    /** The extra keys go to the view, which reads the one-shot modifiers through [Host]. */
    private inner class Keys : ExtraKeysView.Target {
        override fun key(keyCode: Int) {
            closeTabList()
            layout.screen.press(keyCode)
        }

        override fun text(text: String) {
            closeTabList()
            layout.screen.type(text)
        }

        override fun notice(text: String) = showToast(text)
    }

    companion object {
        /** A [VtSession.id]: the tab to show (a tab's notification, from the service). */
        const val EXTRA_TAB = "tab"

        /** The terminal's font size in dp (a float). */
        private const val PREF_FONT_SIZE = "font_size_dp"

        /** The font size in pixels, as versions before [PREF_FONT_SIZE] kept it (an int). */
        private const val PREF_FONT_SIZE_PX = "font_size"

        private const val PREF_KEEP_SCREEN_ON = "keep_screen_on"
        private const val PREF_DISMISSED_BATTERY_PROMPT = "dismissed_battery_prompt"
        private const val PREF_DISMISSED_NOTIFICATION_PROMPT = "dismissed_notification_prompt"

        private const val DEFAULT_FONT_DP = 14f

        /**
         * Characters of the clipboard shown when asking whether a program may read it, as many as
         * of a paste (VtView).
         */
        private const val CLIPBOARD_PREVIEW = 300
    }
}
