package io.github.sajadxdp0.tuxport

import android.content.Context
import androidx.core.content.edit

/** Persisted screen-off policy; the notification remembers the last non-ALWAYS choice. */
internal enum class RunPolicy(val value: String, val label: Int, val summary: Int) {
    SLEEP("sleep", R.string.allow_sleep, R.string.run_sleep_summary),
    BUSY("busy", R.string.run_busy, R.string.run_busy_summary),
    ALWAYS("always", R.string.run_always, R.string.run_always_summary),
}

/**
 * The settings kept in the `settings` preferences (the android/service map). Settings shows and
 * changes them through [TerminalService], except for authenticated changes owned by [Privacy].
 * [Documents] and [MainActivity] (before it binds the service) read them here.
 */
internal object Settings {
    private const val PREFS = "settings"
    private const val KEY_TIER = "tier"
    private const val KEY_LOGIN = "login"
    private const val KEY_KEEP_AWAKE = "keep_awake"
    private const val KEY_RUN_POLICY = "run_policy"
    private const val KEY_IDLE_POLICY = "idle_policy"
    private const val KEY_FILE_ACCESS = "file_access"
    private const val KEY_PRIVACY_MODE = "privacy_mode"
    private const val KEY_FILL_WIDE = "fill_wide"
    private const val KEY_CURSOR_STYLE = "cursor_style"
    private const val KEY_CURSOR_BLINK = "cursor_blink"
    private const val KEY_VOLUME_KEYS_SWITCH_TABS = "volume_keys_switch_tabs"
    private const val KEY_SHOW_TAB_LIST = "show_tab_list"

    // The cursor shapes as recorded, Ghostty's cursor-style values
    private const val CURSOR_BLOCK = "block"
    private const val CURSOR_BAR = "bar"
    private const val CURSOR_UNDERLINE = "underline"

    private fun prefs(context: Context) = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /**
     * The execution tier (see the engine map): 1 is Tier 1 (the default), 0 is the interpreter.
     * Both debug and release builds use the saved choice, effective at the next kernel start.
     */
    fun tier(context: Context): Int = prefs(context).getInt(KEY_TIER, 1)

    fun setTier(context: Context, value: Int) = prefs(context).edit { putInt(KEY_TIER, value) }

    /**
     * The login account of new tabs (the android/setup map): [ConsolePty.LOGIN_AUTO],
     * [ConsolePty.LOGIN_ROOT] or an account name. Goes back to automatic after a distro switch or
     * a rootfs reset ([clearLogin]).
     */
    fun login(context: Context): String = prefs(context).getString(KEY_LOGIN, null) ?: ConsolePty.LOGIN_AUTO

    fun setLogin(context: Context, value: String) = prefs(context).edit { putString(KEY_LOGIN, value) }

    /** Back to automatic: the accounts went with the store. */
    fun clearLogin(context: Context) = prefs(context).edit { remove(KEY_LOGIN) }

    /** Old boolean preferences keep their behaviour until the first policy change. */
    fun runPolicy(context: Context): RunPolicy {
        val p = prefs(context)
        return RunPolicy.entries.firstOrNull { it.value == p.getString(KEY_RUN_POLICY, null) }
            ?: if (p.getBoolean(KEY_KEEP_AWAKE, false)) RunPolicy.ALWAYS else RunPolicy.SLEEP
    }

    /** The other end of the notification's ALWAYS toggle, SLEEP until explicitly chosen. */
    fun idlePolicy(context: Context): RunPolicy =
        if (prefs(context).getString(KEY_IDLE_POLICY, null) == RunPolicy.BUSY.value) RunPolicy.BUSY else RunPolicy.SLEEP

    fun setRunPolicy(context: Context, policy: RunPolicy) = prefs(context).edit {
        putString(KEY_RUN_POLICY, policy.value)
        if (policy != RunPolicy.ALWAYS) putString(KEY_IDLE_POLICY, policy.value)
        remove(KEY_KEEP_AWAKE)
    }

    fun nextRunPolicy(context: Context): RunPolicy =
        if (runPolicy(context) == RunPolicy.ALWAYS) idlePolicy(context) else RunPolicy.ALWAYS

    /**
     * Whether other apps may access documents, including the home directory (off by default,
     * the android/documents map). The old system_documents preference is intentionally ignored:
     * enabling file access now requires confirmation of the sensitive-file warning.
     */
    fun fileAccess(context: Context): Boolean = !privacyMode(context) && prefs(context).getBoolean(KEY_FILE_ACCESS, false)

    fun privacyMode(context: Context): Boolean = prefs(context).getBoolean(KEY_PRIVACY_MODE, false)

    /** Enabling privacy also forgets file access; disabling never restores old grants. */
    fun setPrivacyMode(context: Context, on: Boolean) = prefs(context).edit {
        putBoolean(KEY_PRIVACY_MODE, on)
        if (on) putBoolean(KEY_FILE_ACCESS, false)
    }

    fun setFileAccess(context: Context, on: Boolean) = prefs(context).edit { putBoolean(KEY_FILE_ACCESS, on) }

    /**
     * Whether the terminal enlarges CJK characters to fill their two cells (a switch in Settings,
     * off by default, the android/vtview map); [MainActivity] reads it whenever it starts.
     */
    fun fillWide(context: Context): Boolean = prefs(context).getBoolean(KEY_FILL_WIDE, false)

    fun setFillWide(context: Context, on: Boolean) = prefs(context).edit { putBoolean(KEY_FILL_WIDE, on) }

    /**
     * The cursor's default shape, as Ghostty's cursor-style (a choice in Settings): a block by
     * default ([VtNative.CURSOR_BLOCK]), a bar or an underline. Every tab takes it at once, unless
     * a program chose its own cursor (DECSCUSR), which the program's reset brings back to it.
     */
    fun cursorStyle(context: Context): Int = when (prefs(context).getString(KEY_CURSOR_STYLE, null)) {
        CURSOR_BAR -> VtNative.CURSOR_BAR
        CURSOR_UNDERLINE -> VtNative.CURSOR_UNDERLINE
        else -> VtNative.CURSOR_BLOCK
    }

    fun setCursorStyle(context: Context, style: Int) {
        val name = when (style) {
            VtNative.CURSOR_BAR -> CURSOR_BAR
            VtNative.CURSOR_UNDERLINE -> CURSOR_UNDERLINE
            else -> CURSOR_BLOCK
        }
        prefs(context).edit { putString(KEY_CURSOR_STYLE, name) }
    }

    /**
     * Whether the cursor blinks by default, as Ghostty's cursor-style-blink (a switch in Settings,
     * on by default); programs may still choose otherwise.
     */
    fun cursorBlink(context: Context): Boolean = prefs(context).getBoolean(KEY_CURSOR_BLINK, true)

    fun setCursorBlink(context: Context, on: Boolean) = prefs(context).edit { putBoolean(KEY_CURSOR_BLINK, on) }

    /**
     * Whether the volume keys switch tabs in the terminal, up to the previous and down to the next
     * (a switch in Settings, on by default, the android/terminal map); off, they change the volume.
     * [MainActivity] reads it at each press.
     */
    fun volumeKeysSwitchTabs(context: Context): Boolean = prefs(context).getBoolean(KEY_VOLUME_KEYS_SWITCH_TABS, true)

    fun setVolumeKeysSwitchTabs(context: Context, on: Boolean) =
        prefs(context).edit { putBoolean(KEY_VOLUME_KEYS_SWITCH_TABS, on) }

    /** Whether the tab bar shows its leftmost list button (on by default). */
    fun showTabList(context: Context): Boolean = prefs(context).getBoolean(KEY_SHOW_TAB_LIST, true)

    fun setShowTabList(context: Context, on: Boolean) =
        prefs(context).edit { putBoolean(KEY_SHOW_TAB_LIST, on) }
}
