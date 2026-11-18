package io.github.sajadxdp0.tuxport

import android.annotation.SuppressLint
import android.content.Context
import android.text.InputType
import android.util.TypedValue
import android.view.Gravity
import android.view.HapticFeedbackConstants
import android.view.KeyEvent
import android.view.View
import android.view.ViewGroup
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputMethodManager
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import androidx.annotation.StringRes
import androidx.core.view.isVisible
import androidx.core.widget.doAfterTextChanged

/**
 * The search bar, in the tab bar's place and of its height while a search is open, so the terminal
 * keeps its rows: the text to find, how many matches there are ("3/17": the selected one counted
 * from the newest, 0 before one is selected), `↑` and `↓` to the match above (older output) and
 * below, going round, and `×` to close. Enter (the keyboard's search key) goes up, as Ghostty's
 * Enter does, Shift+Enter down, and Esc closes.
 *
 * It shows the count it is given ([setCount]) and passes the rest to a [Target]: the search itself
 * is the terminal's ([VtView.search]), which the activity connects it to. The android/terminal map.
 */
@SuppressLint("ViewConstructor") // Created only in code, never in layout files
internal class SearchBarView(context: Context, private val target: Target) : LinearLayout(context) {

    /** Where the bar's input goes: the activity, which owns the terminal view. */
    interface Target {
        /** The text to find changed; empty finds nothing. */
        fun search(needle: String)

        /** `↑` or Enter: the match above ([older]); `↓` or Shift+Enter: the one below. */
        fun step(older: Boolean)

        /** `×` or Esc. */
        fun close()
    }

    // On black like the tab bar it stands in for, in the theme's colours
    private val mutedColor = themeColor(com.google.android.material.R.attr.colorOnSurfaceVariant)
    private val textColor = themeColor(com.google.android.material.R.attr.colorOnSurface)

    /** The text to find: one line, without suggestions (commands and paths, not words). */
    private val field = EditText(context).apply {
        background = null
        setPadding(dp(BAR_PADDING_DP), 0, dp(BAR_PADDING_DP), 0)
        gravity = Gravity.CENTER_VERTICAL
        setTextSize(TypedValue.COMPLEX_UNIT_DIP, TabBarView.BAR_LABEL_DP)
        setTextColor(textColor)
        setHintTextColor(mutedColor)
        setHint(R.string.find)
        isSingleLine = true
        inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        imeOptions = EditorInfo.IME_ACTION_SEARCH or EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_FLAG_NO_FULLSCREEN
        doAfterTextChanged {
            val needle = it?.toString().orEmpty()
            if (needle.isEmpty()) count.text = ""
            target.search(needle)
        }
        setOnEditorActionListener { _, action, _ ->
            if (action != EditorInfo.IME_ACTION_SEARCH) return@setOnEditorActionListener false
            target.step(older = true)
            true
        }
        setOnKeyListener { _, _, event -> key(event) }
    }

    private val count = TextView(context).apply {
        setTextColor(mutedColor)
        setTextSize(TypedValue.COMPLEX_UNIT_DIP, TabBarView.BAR_LABEL_DP)
        gravity = Gravity.CENTER_VERTICAL
        setPadding(dp(BAR_PADDING_DP), 0, dp(BAR_PADDING_DP), 0)
        // Digits of one width, so the count does not jiggle as it grows
        fontFeatureSettings = "'tnum'"
    }

    init {
        orientation = HORIZONTAL
        setBackgroundColor(TERMINAL_BG)
        addView(field, LayoutParams(0, ViewGroup.LayoutParams.MATCH_PARENT, 1f))
        addView(count, LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.MATCH_PARENT))
        addView(button("↑", R.string.find_previous) { target.step(older = true) }, item())
        addView(button("↓", R.string.find_next) { target.step(older = false) }, item())
        addView(button("×", R.string.find_close) { target.close() }, item())
    }

    /** As tall as the tab bar ([TabBarView.BAR_DP]), whatever the parent offers. */
    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        super.onMeasure(widthMeasureSpec, MeasureSpec.makeMeasureSpec(dp(TabBarView.BAR_DP), MeasureSpec.EXACTLY))
    }

    /**
     * Shows the bar with the keyboard on the text, which is kept from the last search and
     * selected, so typing replaces it; searching for it starts again at once.
     */
    fun open() {
        isVisible = true
        field.requestFocus()
        field.selectAll()
        // Once the field is laid out and focused, or the keyboard may not come up
        field.post {
            context.getSystemService(InputMethodManager::class.java).showSoftInput(field, 0)
        }
        target.search(field.text.toString())
    }

    /** The count of [total] matches, [selected] the selected one's place (0 for none). */
    fun setCount(total: Int, selected: Int) {
        count.text = if (field.text.isEmpty()) "" else "%d/%d".format(selected, total)
    }

    /**
     * A hardware key in the field: Enter goes up (Shift+Enter down) and Esc closes, on the press;
     * their releases are taken too, so the field does not act on them again.
     */
    private fun key(event: KeyEvent): Boolean {
        when (event.keyCode) {
            KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> {
                if (event.action == KeyEvent.ACTION_DOWN) target.step(older = !event.isShiftPressed)
            }
            KeyEvent.KEYCODE_ESCAPE -> if (event.action == KeyEvent.ACTION_DOWN) target.close()
            else -> return false
        }
        return true
    }

    /** A button as the tab bar's: a square as tall as the bar with its label centred. */
    private fun button(label: String, @StringRes description: Int, onClick: (View) -> Unit) = TextView(context).apply {
        text = label
        contentDescription = context.getString(description)
        setTextColor(mutedColor)
        setTextSize(TypedValue.COMPLEX_UNIT_DIP, TabBarView.BAR_LABEL_DP)
        gravity = Gravity.CENTER
        background = touchFeedback()
        setOnClickListener {
            it.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
            onClick(it)
        }
    }

    private fun item() = LayoutParams(dp(TabBarView.BAR_DP), ViewGroup.LayoutParams.MATCH_PARENT)

    private companion object {
        /** The room before the text and around the count (dp). */
        const val BAR_PADDING_DP = 12
    }
}
