package io.github.sajadxdp0.tuxport

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.graphics.Color
import android.graphics.drawable.Drawable
import android.net.Uri
import android.text.SpannableString
import android.text.Spanned
import android.text.TextUtils
import android.text.style.TypefaceSpan
import android.view.View
import androidx.annotation.StringRes
import androidx.appcompat.app.AlertDialog
import androidx.core.graphics.ColorUtils
import com.google.android.material.color.MaterialColors
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import kotlin.math.max

// Small helpers the app's screens and views share (the android/terminal map): sizes in dp and
// the theme's colours and drawables

/**
 * How thick the app's thin lines are (dp): the indicator under the tab bar's and the extra keys'
 * labels, and the terminal's progress line.
 */
internal const val LINE_DP = 1

/** Tuxport's Neon Terminal base, shared by the terminal's chrome and fallback cells. */
internal const val TERMINAL_BG = 0xFF0D1117.toInt()

/** [v] dp in whole pixels, rounded down. */
internal fun Context.dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()

internal fun View.dp(v: Int): Int = context.dp(v)

/** [LINE_DP] in whole pixels, at least one. */
internal fun Context.line(): Int = max(1, dp(LINE_DP))

internal fun View.line(): Int = context.line()

/** The theme's colour [attr], white when the theme has none. */
internal fun Context.themeColor(attr: Int): Int = MaterialColors.getColor(this, attr, Color.WHITE)

internal fun View.themeColor(attr: Int): Int = context.themeColor(attr)

/** Separate control states on black; the dark theme's primary and muted labels share a tone. */
internal fun View.terminalControlColor(selected: Boolean): Int = if (selected) {
    ColorUtils.blendARGB(themeColor(androidx.appcompat.R.attr.colorPrimary), Color.WHITE, 0.25f)
} else {
    ColorUtils.blendARGB(themeColor(com.google.android.material.R.attr.colorOnSurfaceVariant), Color.BLACK, 0.4f)
}

/** The theme's drawable [attr], null when the theme has none. */
internal fun Context.themeDrawable(attr: Int): Drawable? {
    val a = obtainStyledAttributes(intArrayOf(attr))
    try {
        return a.getDrawable(0)
    } finally {
        a.recycle()
    }
}

/** The theme's ripple for something that can be tapped. */
internal fun View.touchFeedback(): Drawable? = context.themeDrawable(android.R.attr.selectableItemBackground)

/** Opens a system settings action that takes the app's package URI. */
internal fun Activity.openPackageSettings(action: String) {
    startActivity(Intent(action, Uri.fromParts("package", packageName, null)))
}

/** Inserts a monospace value at the localized message's ^1 placeholder, preserving its span. */
internal fun Context.monospaceMessage(@StringRes template: Int, value: String): CharSequence {
    val content = SpannableString(value).apply {
        setSpan(TypefaceSpan("monospace"), 0, length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
    }
    return TextUtils.expandTemplate(getText(template), content)
}

/** Dialog windows do not inherit FLAG_SECURE from their activity. */
internal fun MaterialAlertDialogBuilder.showPrivate(): AlertDialog = create().also {
    Privacy.protect(it)
    it.show()
}
