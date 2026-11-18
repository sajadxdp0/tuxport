package io.github.sajadxdp0.tuxport

import android.content.Context
import android.graphics.Typeface
import android.text.TextUtils
import android.util.TypedValue
import android.view.Gravity
import android.view.HapticFeedbackConstants
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import androidx.annotation.StringRes
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsAnimationCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.isVisible

/**
 * Builds the terminal screen after the activity applies its theme. Owns the view hierarchy,
 * system insets and the extra keys' IME animation; the activity connects input and session state.
 */
internal class TerminalLayout(
    context: Context,
    fontSize: Float,
    host: VtView.Host,
    bar: TabBarView.Target,
    tabs: TabListView.Target,
    find: SearchBarView.Target,
    extraKeys: ExtraKeysView.Target,
    enableNotifications: () -> Unit,
    dismissNotificationPrompt: () -> Unit,
    requestBatteryExemption: () -> Unit,
    dismissBatteryPrompt: () -> Unit,
) {
    val screen = VtView(context, host).apply {
        setFontSize(fontSize)
        setBackgroundColor(TERMINAL_BG)
    }
    private val status = TextView(context).apply {
        // In the muted colour of the bars' labels, read once onCreate has applied the
        // wallpaper's colours (Android 12 and later)
        setTextColor(context.themeColor(com.google.android.material.R.attr.colorOnSurfaceVariant))
        gravity = Gravity.CENTER
        setText(R.string.starting)
    }
    /** The current tab's progress (OSC 9;4), along the top of the terminal. */
    val progressLine = ProgressLine(context)
    val tabBar = TabBarView(context, bar)
    val tabList = TabListView(context, tabs)

    /** In the tab bar's place while the current tab is searched. */
    val searchBar = SearchBarView(context, find).apply { isVisible = false }
    val keys = ExtraKeysView(context, extraKeys)

    val notificationPrompt = permissionPrompt(
        context, R.string.notification_prompt, R.string.notification_prompt_dismiss,
        enableNotifications, dismissNotificationPrompt,
    )
    val batteryPrompt = permissionPrompt(
        context, R.string.battery_prompt, R.string.battery_prompt_dismiss,
        requestBatteryExemption, dismissBatteryPrompt,
    )

    private fun permissionPrompt(
        context: Context,
        @StringRes message: Int,
        @StringRes dismissLabel: Int,
        enable: () -> Unit,
        dismiss: () -> Unit,
    ) = LinearLayout(context).apply {
        val rowHeight = context.dp(32)
        layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, rowHeight)
        isVisible = false
        orientation = LinearLayout.HORIZONTAL
        setBackgroundColor(context.themeColor(com.google.android.material.R.attr.colorSurfaceContainer))
        fun button(action: () -> Unit) = Button(context).apply {
            setTextColor(context.themeColor(com.google.android.material.R.attr.colorOnSurfaceVariant))
            setTextSize(TypedValue.COMPLEX_UNIT_DIP, 14f)
            typeface = Typeface.DEFAULT
            isAllCaps = false
            isSingleLine = true
            gravity = Gravity.CENTER
            setPadding(0, 0, 0, 0)
            minWidth = 0
            minimumWidth = 0
            minHeight = 0
            minimumHeight = 0
            backgroundTintList = null
            background = touchFeedback()
            setOnClickListener {
                it.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                action()
            }
        }
        addView(button(enable).apply {
            setText(message)
            ellipsize = TextUtils.TruncateAt.END
            gravity = Gravity.START or Gravity.CENTER_VERTICAL
            setPadding(context.dp(10), 0, context.dp(10), 0)
        }, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.MATCH_PARENT, 1f))
        addView(button(dismiss).apply {
            text = "×"
            contentDescription = context.getString(dismissLabel)
        }, LinearLayout.LayoutParams(rowHeight, ViewGroup.LayoutParams.MATCH_PARENT))
    }

    private val content: LinearLayout
    val root: LinearLayout

    /**
     * Moves the extra keys with the IME as it shows or hides. The system lays the root out for the
     * end as the animation starts (so the terminal regrids once, at the start), and the keys are
     * drawn from where they were on screen to their new place along the IME's own curve: as it
     * shows, they rise through the IME's room (black) to the terminal's new bottom; as it hides,
     * they sink over the terminal's new rows, which they hide while passing, being black.
     */
    private val keysFollowIme = object : WindowInsetsAnimationCompat.Callback(DISPATCH_MODE_STOP) {
        /** The IME animation being followed; an earlier one ending late leaves the keys alone. */
        private var following: WindowInsetsAnimationCompat? = null

        /** The keys' bottom before the change, relative to the content column. */
        private var startBottom = 0f

        /** How far from their place the keys start: below it as the IME shows, above as it hides. */
        private var shift = 0f

        override fun onPrepare(animation: WindowInsetsAnimationCompat) {
            if (!isIme(animation)) return
            following = animation
            // Where they are drawn, which is not their place while an earlier animation ran
            startBottom = keys.bottom + keys.translationY
        }

        override fun onStart(
            animation: WindowInsetsAnimationCompat,
            bounds: WindowInsetsAnimationCompat.BoundsCompat,
        ): WindowInsetsAnimationCompat.BoundsCompat {
            if (animation === following) {
                shift = startBottom - keys.bottom
                keys.translationY = shift
            }
            return bounds
        }

        override fun onProgress(
            insets: WindowInsetsCompat,
            runningAnimations: List<WindowInsetsAnimationCompat>,
        ): WindowInsetsCompat {
            following?.takeIf { it in runningAnimations }?.let {
                keys.translationY = shift * (1 - it.interpolatedFraction)
            }
            return insets
        }

        override fun onEnd(animation: WindowInsetsAnimationCompat) {
            if (animation !== following) return
            following = null
            keys.translationY = 0f
        }

        private fun isIme(animation: WindowInsetsAnimationCompat) =
            animation.typeMask and WindowInsetsCompat.Type.ime() != 0
    }

    init {
        // One bar's place: the search bar, as tall as the tab bar, stands in for it
        val bars = FrameLayout(context).apply {
            addView(tabBar, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
            addView(searchBar, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
        }
        val body = FrameLayout(context).apply {
            setBackgroundColor(TERMINAL_BG)
            addView(screen)
            addView(status)
            addView(progressLine, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, context.line(), Gravity.TOP))
        }
        content = LinearLayout(context).apply {
            orientation = LinearLayout.VERTICAL
            // Let the keys follow the IME beyond this column's bottom edge.
            clipChildren = false
            addView(notificationPrompt)
            addView(batteryPrompt)
            addView(body, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
            addView(keys)
        }
        val overlay = FrameLayout(context).apply {
            clipChildren = false
            addView(content, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
            // The list covers the banners, terminal and keys, including during IME animation.
            addView(tabList, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
        }
        root = LinearLayout(context).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(TERMINAL_BG)
            // The extra keys are drawn in the IME's room while they follow it up (keysFollowIme)
            clipToPadding = false
            addView(bars, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
            addView(overlay, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
        }
        // Leave room for the system bars, the display cutout and the IME. In landscape the
        // cutout can be at a side, over the terminal's first or last columns.
        ViewCompat.setOnApplyWindowInsetsListener(root) { v, insets ->
            val room = insets.getInsets(
                WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout() or
                    WindowInsetsCompat.Type.ime(),
            )
            v.setPadding(room.left, room.top, room.right, room.bottom)
            WindowInsetsCompat.CONSUMED
        }
        ViewCompat.setWindowInsetsAnimationCallback(root, keysFollowIme)
    }

    fun updateTabListControls() {
        val open = tabList.isVisible
        content.importantForAccessibility = if (open) {
            View.IMPORTANT_FOR_ACCESSIBILITY_NO_HIDE_DESCENDANTS
        } else {
            View.IMPORTANT_FOR_ACCESSIBILITY_AUTO
        }
        tabBar.setListOpen(open)
    }

    fun showStatus(text: String) {
        screen.visibility = View.INVISIBLE
        status.visibility = View.VISIBLE
        status.text = text
    }

    fun showTerminal() {
        status.visibility = View.GONE
        screen.visibility = View.VISIBLE
    }
}
