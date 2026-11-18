package io.github.sajadxdp0.tuxport

import android.annotation.SuppressLint
import android.content.Context
import android.content.res.Configuration
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Rect
import android.graphics.RectF
import android.graphics.drawable.InsetDrawable
import android.util.TypedValue
import android.view.Gravity
import android.view.HapticFeedbackConstants
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.View
import android.view.ViewConfiguration
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.TextView
import androidx.core.graphics.drawable.toDrawable
import io.github.sajadxdp0.tuxport.ExtraKeysView.Companion.COLUMNS
import io.github.sajadxdp0.tuxport.ExtraKeysView.Companion.FUNCTIONS
import io.github.sajadxdp0.tuxport.ExtraKeysView.Companion.INDICATOR_GAP_DP
import io.github.sajadxdp0.tuxport.ExtraKeysView.Companion.KEYS
import io.github.sajadxdp0.tuxport.ExtraKeysView.Companion.MIN_COLUMN_DP
import io.github.sajadxdp0.tuxport.ExtraKeysView.Companion.REPEAT_MS
import io.github.sajadxdp0.tuxport.ExtraKeysView.Companion.SYMBOLS
import kotlin.math.max
import kotlin.math.roundToInt

/**
 * The keys the soft keyboard lacks: Termux's default two rows with the arrows as an inverted T,
 * widened to eight columns, with Shift and SYM in the last. Until it is tapped again, SYM puts
 * shell symbols in place of every other key, without duplicating the first page's symbols.
 * A long press on SYM puts the function keys there instead, with Page Up and Down beside them
 * as on fn's layer of a laptop and Shift kept in two-row mode,
 * until SYM is tapped or long-pressed again. SYM is under Shift as the symbol key of a phone
 * keyboard is and fn on a laptop, and on the edge next to the IME, where a stray tap turns the
 * whole page rather than silently shifting the IME's next letter.
 *
 * ```
 * ESC  /   -  HOME  ↑  END  |  │ SHIFT
 * TAB CTRL ALT  ←   ↓   →   ~  │ SYM
 *
 *  `   $   *   =   ^   \   &   │  %
 *  [   ]   {   }   <   >   ;   │ SYM
 *
 *  F1  F2  F3  F4  F5  F6 PGUP │ SHIFT
 *  F7  F8  F9 F10 F11 F12 PGDN │ SYM
 * ```
 *
 * The columns share the width but are never narrower than [MIN_COLUMN_DP], so all eight fit from
 * 360 dp on. In a narrower view the last column stays at the right edge and the first seven scroll
 * sideways under it, fading out into the blank left of SHIFT while more of them lie beyond, as the
 * tab row fades out into ×.
 * Wide windows and short landscape windows use one row, with ESC, TAB, / and - before navigation and
 * CTRL, ALT, SHIFT and SYM fixed at the right. The symbols and function pages fix only SYM there, retaining
 * the hidden modifiers' state; % follows & before the bracket pairs. Cells never shrink below the minimum.
 *
 * A key acts when the finger lifts, and not at all if the finger left it first. Holding an arrow,
 * Page Up or Page Down repeats it. Ctrl, Alt and Shift are one-shot modifiers: tapped, they apply
 * to the next key typed, which reads them ([ctrl], [alt], [shift]) and reports taking them
 * ([modifiersUsed]); held, they stay on until tapped again. SYM is no modifier: turning the page
 * leaves them as they are, and Ctrl and Alt apply to a symbol or a function key too, while Shift,
 * not shown with the symbols, waits there for another page. The android/keys map.
 */
@SuppressLint("ViewConstructor") // Created only in code, never in layout files
internal class ExtraKeysView(context: Context, private val target: Target) : FrameLayout(context) {

    /** Where the keys go: the terminal view of the tab shown. */
    interface Target {
        /** A special key (Esc, Tab, arrows); the one-shot modifiers are still to be read. */
        fun key(keyCode: Int)

        /** Plain characters; the one-shot modifiers are still to be read. */
        fun text(text: String)

        /** A short message about the keys, such as which modifier a long press locked. */
        fun notice(text: String)
    }

    /** Ctrl, Alt or Shift: on for the next key after a tap, [locked] on after a long press. */
    private class Modifier {
        var on = false
        var locked = false
        lateinit var view: TextView
    }

    /**
     * One key: its label, what it sends, whether holding it repeats, the modifier it toggles, or
     * what a long press does instead of repeating (a modifier locked, SYM's function keys).
     */
    private class Key(
        val label: String,
        val repeats: Boolean = false,
        val modifier: Modifier? = null,
        val longPress: (() -> Unit)? = null,
        val send: () -> Unit = {},
    )

    /**
     * A page's original rows, their single-row order, and the key above SYM in two-row mode.
     * Only the page shown has its grid attached to the scroll view.
     */
    private class Page(val grid: LinearLayout, val rows: List<List<View>>, val upperKey: View) {
        var singleRowKeys = rows.flatten()
    }

    private val ctrlKey = Modifier()
    private val altKey = Modifier()
    private val shiftKey = Modifier()
    private val modifiers = arrayOf(ctrlKey, altKey, shiftKey)

    /** The keys, the symbols SYM shows, and the function keys a long press on it shows. */
    private val pages = mutableListOf<Page>()

    /** The keys in the pages' grids (sized in [onMeasure]). */
    private val gridKeys = mutableListOf<View>()
    private val presses = mutableListOf<Press>()
    private var singleRow = false

    /** The page shown: [KEYS], [SYMBOLS] or [FUNCTIONS]. */
    private var shown = KEYS
    private val symView: TextView

    private val density = resources.displayMetrics.density

    /**
     * How far the fade reaches into the last column, in whole pixels, rounded down as
     * TabBarView's dp.
     */
    private val fadeIntoLast = (FADE_INTO_LAST_DP * density).toInt()

    /**
     * The page shown, scrolled sideways when its keys do not fit. It runs under the fixed keys
     * to the view's end; its end padding lets the grid's last key scroll out from under them.
     * While keys lie beyond the padding, the right fading edge hints that they scroll:
     * a padding offset carries it into the blank left of the first fixed key,
     * as the tab row's into ×.
     */
    private val scroll = object : HorizontalScrollView(context) {
        // It would take any sideways slide past the touch slop, cancelling the key under the
        // finger, even with nothing to scroll
        override fun onInterceptTouchEvent(e: MotionEvent) = scrolls() && super.onInterceptTouchEvent(e)

        override fun getLeftFadingEdgeStrength() = 0f
        override fun isPaddingOffsetRequired() = true
        override fun getRightPaddingOffset() = fadeIntoLast

        // With gesture navigation, a slide from the screen's left edge over keys that scroll
        // scrolls them instead of going back; with nothing to scroll, the edge keeps its gesture.
        // Reused: the framework copies the list and reads the rectangle when it updates, after
        // each set
        private val excluded = Rect()
        private val exclusion = listOf(excluded)

        override fun onLayout(changed: Boolean, l: Int, t: Int, r: Int, b: Int) {
            super.onLayout(changed, l, t, r, b)
            // The part beside the fixed keys: under them, the right edge keeps its gesture
            excluded.set(0, 0, width - paddingRight, height)
            systemGestureExclusionRects = if (scrolls()) exclusion else emptyList()
        }

        // Not canScrollHorizontally, which counts the end padding as shown, so from 315 dp on it
        // would find nothing to scroll
        private fun scrolls() = getChildAt(0).let { it != null && it.width > width - paddingLeft - paddingRight }
    }.apply {
        isHorizontalScrollBarEnabled = false
        isHorizontalFadingEdgeEnabled = true
        setFadingEdgeLength((FADE_DP * density).toInt())
        clipToPadding = false
    }

    /**
     * The fixed keys at the right edge over the grid's end. Black but for their left part the
     * fade reaches into, so the keys under it are hidden; its keys fill it and take every touch on
     * it, a key showing through the fade too.
     */
    private val fixed = object : LinearLayout(context) {
        override fun dispatchDraw(canvas: Canvas) {
            super.dispatchDraw(canvas)
            indicateKeys(canvas, this)
        }
    }.apply {
        orientation = LinearLayout.VERTICAL
        background = InsetDrawable(TERMINAL_BG.toDrawable(), fadeIntoLast, 0, 0, 0)
        // The inset comes as padding with the background, which would narrow the keys and move
        // them right by as much
        setPadding(0, 0, 0, 0)
    }

    /** The width of a column in the grid, in pixels, as last measured; 0 before that. */
    private var column = 0

    private val slop = ViewConfiguration.get(context).scaledTouchSlop

    // On black like the terminal and the tab bar; the theme's colours, as the tab bar's, for the
    // labels, a key that is on and its indicator, and a key while pressed
    private val labelColor = terminalControlColor(selected = false)
    private val onColor = terminalControlColor(selected = true)
    private val indicatorPaint = indicatorPaint(onColor)
    private val pressedColor = themeColor(com.google.android.material.R.attr.colorSurfaceContainerHighest)

    /** The height of a row, in pixels. */
    private val rowHeight = (ROW_DP * density).roundToInt()

    init {
        setBackgroundColor(TERMINAL_BG)
        page(
            arrayOf(
                special("ESC", KeyEvent.KEYCODE_ESCAPE),
                char("/"),
                char("-"),
                special("HOME", KeyEvent.KEYCODE_MOVE_HOME),
                special("↑", KeyEvent.KEYCODE_DPAD_UP, repeats = true),
                special("END", KeyEvent.KEYCODE_MOVE_END),
                char("|"),
            ),
            arrayOf(
                special("TAB", KeyEvent.KEYCODE_TAB),
                modifier("CTRL", ctrlKey),
                modifier("ALT", altKey),
                special("←", KeyEvent.KEYCODE_DPAD_LEFT, repeats = true),
                special("↓", KeyEvent.KEYCODE_DPAD_DOWN, repeats = true),
                special("→", KeyEvent.KEYCODE_DPAD_RIGHT, repeats = true),
                char("~"),
            ),
            upperKey = fixedView(modifier("SHIFT", shiftKey)),
        )
        // Put ESC, TAB, / and - first, then up/down followed by left/right and Home/End, before | and ~.
        pages[KEYS].let { page ->
            val (top, bottom) = page.rows
            page.singleRowKeys = listOf(
                top[0], bottom[0], top[1], top[2], // ESC TAB / -
                top[4], bottom[4], bottom[3], bottom[5], top[3], top[5], // ↑ ↓ ← → HOME END
                top[6], bottom[6], // | ~
            )
        }
        // Keep bracket pairs together, $ and * on the left, and the shell control operators
        // & and ; in the rightmost grid column. The other symbols retain their positions.
        page(chars("`$*=^\\&"), chars("[]{}<>;"), upperKey = fixedView(char("%")))
        pages[SYMBOLS].let { page ->
            val (top, bottom) = page.rows
            page.singleRowKeys = top + page.upperKey + bottom
            gridKeys.add(page.upperKey)
        }
        // F1 to F12 in order, six to a row, then Page Up and Down, which fn's arrows type on a
        // laptop and have no other place. Two-row mode keeps SHIFT for shifted function keys
        // (mc's Shift+F4 and Shift+F6); single-row mode uses the main page's selected modifiers.
        page(
            Array(6) { function(it) } + special("PGUP", KeyEvent.KEYCODE_PAGE_UP, repeats = true),
            Array(6) { function(it + 6) } + special("PGDN", KeyEvent.KEYCODE_PAGE_DOWN, repeats = true),
            upperKey = pages[KEYS].upperKey,
        )
        pages[FUNCTIONS].let { page ->
            val (top, bottom) = page.rows
            page.singleRowKeys = top.take(6) + bottom.take(6) + listOf(top.last(), bottom.last())
        }
        symView = fixedView(
            Key("SYM", longPress = { turnTo(if (shown == FUNCTIONS) KEYS else FUNCTIONS) }) {
                turnTo(if (shown == KEYS) SYMBOLS else KEYS)
            },
        )
        arrangePages()
        turnTo(KEYS)
        // Their widths and the scroll view's end padding are set in onMeasure.
        addView(scroll, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.WRAP_CONTENT))
        addView(fixed, LayoutParams(0, LayoutParams.WRAP_CONTENT, Gravity.END))
    }

    private fun special(label: String, keyCode: Int, repeats: Boolean = false) =
        Key(label, repeats = repeats) { target.key(keyCode) }

    /** Ctrl, Alt or Shift: a tap toggles [m], a long press locks it ([hold]). */
    private fun modifier(label: String, m: Modifier) = Key(label, modifier = m, longPress = { hold(m) })

    private fun char(text: String) = Key(text) { target.text(text) }

    /** A key for each character of [text]. */
    private fun chars(text: String) = Array(text.length) { char(text[it].toString()) }

    /** The function key F1 to F12 at [index] from 0. */
    private fun function(index: Int) = special("F${index + 1}", KeyEvent.KEYCODE_F1 + index)

    /**
     * A page: a row of the grid for each of [rows], of [COLUMNS] − 1 keys each, and [upperKey],
     * the view of the key above SYM in two-row mode.
     */
    private fun page(vararg rows: Array<Key>, upperKey: View) {
        val grid = object : LinearLayout(context) {
            // Drawn here, inside the scroll view, the indicators scroll with the keys
            override fun dispatchDraw(canvas: Canvas) {
                super.dispatchDraw(canvas)
                indicateKeys(canvas, this)
            }
        }.apply { orientation = LinearLayout.VERTICAL }
        val views = rows.map { keys ->
            check(keys.size == COLUMNS - 1)
            keys.map { key -> keyView(key).also { gridKeys.add(it) } }
        }
        pages.add(Page(grid, views, upperKey))
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        // Choose the mode during measurement, with the new inset-adjusted width, not the old bounds.
        requestLayout()
    }

    private fun updateLayoutMode(width: Int) {
        val config = resources.configuration
        val useSingleRow = width >= (SINGLE_ROW_WIDTH_DP * density).roundToInt() ||
            (config.orientation == Configuration.ORIENTATION_LANDSCAPE && config.screenHeightDp <= COMPACT_HEIGHT_DP)
        if (useSingleRow == singleRow) return
        // Reuse the keys so the selected page and one-shot/locked modifiers survive resizing.
        // Cancel delayed repeats before their keys move underneath a held finger.
        presses.forEach { it.cancel() }
        singleRow = useSingleRow
        arrangePages()
        show(pages[shown])
        scroll.scrollTo(0, 0)
        refresh()
    }

    /** Reparents existing keys only when the window changes between one and two rows. */
    private fun arrangePages() {
        fixed.removeAllViews()
        for (page in pages) {
            for (i in 0 until page.grid.childCount) (page.grid.getChildAt(i) as ViewGroup).removeAllViews()
            page.grid.removeAllViews()
        }
        for (page in pages) {
            for (keys in if (singleRow) listOf(page.singleRowKeys) else page.rows) {
                val row = LinearLayout(context).apply { orientation = LinearLayout.HORIZONTAL }
                for (key in keys) row.addView(key, LinearLayout.LayoutParams(0, rowHeight))
                page.grid.addView(row, LinearLayout.LayoutParams(LayoutParams.WRAP_CONTENT, rowHeight))
            }
        }
        fixed.orientation = if (singleRow) LinearLayout.HORIZONTAL else LinearLayout.VERTICAL
        column = 0
    }

    @SuppressLint("ClickableViewAccessibility") // Press calls performClick on release
    private fun keyView(key: Key) = label(key.label).also { view ->
        key.modifier?.view = view
        view.setOnClickListener { tap(key) }
        // For accessibility services only: Press takes every touch, so the view's own long-press
        // detection never runs and Press acts on a held finger itself
        key.longPress?.let { longPress ->
            view.setOnLongClickListener {
                longPress()
                true
            }
        }
        view.setOnTouchListener(Press(key, view).also { presses.add(it) })
    }

    private fun label(text: String) = TextView(context).apply {
        this.text = text
        gravity = Gravity.CENTER
        setTextColor(labelColor)
        setTextSize(TypedValue.COMPLEX_UNIT_DIP, LABEL_DP)
        isSingleLine = true
    }

    /** A key for the fixed column, laid out to fill a cell of it. */
    private fun fixedView(key: Key) =
        keyView(key).apply { layoutParams = LinearLayout.LayoutParams(LayoutParams.MATCH_PARENT, rowHeight) }

    /**
     * Puts [page] in the view in place of the one shown, with its fixed keys. Removing a key's
     * view cancels its touch, so a key still held on the page going away stops repeating. In
     * two-row mode, SHIFT stays in place between the first page and the function keys' page.
     */
    private fun show(page: Page) {
        scroll.removeAllViews()
        scroll.addView(page.grid)
        val fixedKeys = when {
            !singleRow -> listOf(page.upperKey, symView)
            shown == KEYS -> listOf(ctrlKey.view, altKey.view, shiftKey.view, symView)
            else -> listOf(symView)
        }
        // Keep shared keys attached, especially SYM while its long press changes the page.
        for (i in fixed.childCount - 1 downTo 0) {
            if (fixed.getChildAt(i) !in fixedKeys) fixed.removeViewAt(i)
        }
        for ((index, key) in fixedKeys.withIndex()) {
            if (key.parent !== fixed) fixed.addView(key, index)
        }
    }

    private fun refresh() {
        symView.setTextColor(if (shown != KEYS) onColor else labelColor)
        for (m in modifiers) m.view.setTextColor(if (m.on) onColor else labelColor)
        // Their indicators; only the page shown is in the view
        pages[shown].grid.invalidate()
        fixed.invalidate()
    }

    /**
     * Draws the indicator, as under the current tab, under the keys in [container] that stay on
     * until tapped (a locked modifier, and SYM with another page than the first shown; a modifier
     * on for the next key only shows the colour), over the keys so that a pressed key's shading
     * does not hide it.
     */
    private fun indicateKeys(canvas: Canvas, container: View) {
        for (m in modifiers) if (m.locked) drawIndicator(canvas, container, m.view, indicatorPaint)
        if (shown != KEYS) drawIndicator(canvas, container, symView, indicatorPaint)
    }

    /**
     * Uses eight columns in two-row mode and sixteen on every single-row page, each at least
     * [MIN_COLUMN_DP] wide. SYM takes the rounding remainder; the shorter function page leaves
     * room before it. When the minimum wins, the grid scrolls under the fixed keys.
     */
    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        val width = MeasureSpec.getSize(widthMeasureSpec)
        updateLayoutMode(width)
        val min = (MIN_COLUMN_DP * density).roundToInt()
        val columns = if (singleRow) 2 * COLUMNS else COLUMNS
        val c = max(min, width / columns)
        if (c != column) {
            column = c
            for (v in gridKeys) v.layoutParams.width = c
            // The rows and the grids are measured without a width limit, which need not change
            // with the columns, so their last measurement could be reused; the page not shown
            // keeps the flag until it is shown
            for (page in pages) {
                page.grid.forceLayout()
                for (i in 0 until page.grid.childCount) page.grid.getChildAt(i).forceLayout()
            }
        }
        val last = max(min, width - (columns - 1) * c)
        val fixedWidth = if (singleRow) c * (fixed.childCount - 1) + last else last
        for (i in 0 until fixed.childCount) {
            fixed.getChildAt(i).layoutParams.apply {
                this.width = when {
                    !singleRow -> LayoutParams.MATCH_PARENT
                    i == fixed.childCount - 1 -> last
                    else -> c
                }
                height = rowHeight
            }
        }
        fixed.layoutParams.width = fixedWidth
        scroll.setPadding(0, 0, fixedWidth, 0)
        super.onMeasure(widthMeasureSpec, heightMeasureSpec)
    }

    /**
     * The touch on one key: highlights it while pressed, taps it on release (through
     * [View.performClick]), and while held repeats it or does its [Key.longPress] once. A sideways
     * slide on a key that is not yet held scrolls the grid, if it can scroll, and cancels the key.
     */
    private inner class Press(private val key: Key, private val view: View) : OnTouchListener, Runnable {
        /** The long-press timeout passed with the finger still on the key. */
        private var held = false
        private var cancelled = false

        override fun onTouch(v: View, e: MotionEvent): Boolean {
            when (e.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    held = false
                    cancelled = false
                    view.setBackgroundColor(pressedColor)
                    if (key.repeats || key.longPress != null) {
                        view.postDelayed(this, ViewConfiguration.getLongPressTimeout().toLong())
                    }
                }
                MotionEvent.ACTION_MOVE -> if (!cancelled && outside(e)) cancel()
                MotionEvent.ACTION_UP -> {
                    val tapped = !cancelled && !held
                    cancel()
                    if (tapped) {
                        view.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                        view.performClick()
                    }
                }
                MotionEvent.ACTION_CANCEL -> cancel()
            }
            return true
        }

        /** The long-press timeout passed, then every [REPEAT_MS] while a repeating key is held. */
        override fun run() {
            if (cancelled) return
            if (!held) {
                held = true
                keepTouch()
            }
            val longPress = key.longPress
            if (longPress != null) {
                view.performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
                longPress()
            } else {
                key.send()
                view.postDelayed(this, REPEAT_MS)
            }
        }

        /** From now on the grid does not take the finger to scroll. */
        private fun keepTouch() = view.parent.requestDisallowInterceptTouchEvent(true)

        private fun outside(e: MotionEvent) =
            e.x < -slop || e.y < -slop || e.x > view.width + slop || e.y > view.height + slop

        fun cancel() {
            cancelled = true
            view.removeCallbacks(this)
            view.background = null
        }
    }

    /** A tap, also what accessibility services perform as a click. */
    private fun tap(key: Key) {
        val m = key.modifier
        if (m == null) {
            key.send()
        } else {
            m.on = !m.on
            m.locked = false
            refresh()
        }
    }

    /**
     * A long press on Ctrl, Alt or Shift (or an accessibility service's): locks it on, also when a
     * tap had turned it on for the next key, saying which one it locked, or turns it off when
     * locked.
     */
    private fun hold(m: Modifier) {
        m.locked = !m.locked
        m.on = m.locked
        refresh()
        if (m.locked) target.notice(context.getString(R.string.modifier_locked, m.view.text))
    }

    /**
     * SYM: shows page [index], the symbols from the first page on a tap or the function keys on a
     * long press, or the first page again.
     */
    private fun turnTo(index: Int) {
        shown = index
        show(pages[index])
        refresh()
    }

    val ctrl get() = ctrlKey.on
    val alt get() = altKey.on
    val shift get() = shiftKey.on && !waits(shiftKey)

    /**
     * The next key or character took the one-shot modifiers; locked ones stay on, and so does
     * Shift while it waits with the symbols shown.
     */
    fun modifiersUsed() {
        var changed = false
        for (m in modifiers) {
            if (m.on && !m.locked && !waits(m)) {
                m.on = false
                changed = true
            }
        }
        if (changed) post { refresh() }
    }

    /** Whether [m] waits for another page: Shift with the symbols shown, where it has no key. */
    private fun waits(m: Modifier) = m === shiftKey && shown == SYMBOLS

    companion object {
        /** The pages, as indices into [pages]: the keys, the symbols and the function keys. */
        private const val KEYS = 0
        private const val SYMBOLS = 1
        private const val FUNCTIONS = 2

        /** The number of columns: seven in the grid and the fixed one. */
        private const val COLUMNS = 8

        /** Use one row from this inset-adjusted width; keys scroll until all sixteen fit. */
        private const val SINGLE_ROW_WIDTH_DP = 600

        /** Narrow landscape windows also use one row when short. IME height does not choose the mode. */
        private const val COMPACT_HEIGHT_DP = 480

        /** The narrowest a column gets, so eight fit in 360 dp. */
        private const val MIN_COLUMN_DP = 45f

        /** The height of a row. */
        private const val ROW_DP = 32f

        /**
         * The size of the labels; HOME, the widest, takes about 34 dp of a 45 dp column. In dp, not
         * sp, so the system font scale cannot outgrow the fixed cells.
         */
        private const val LABEL_DP = 12f

        /**
         * The width over which a row that scrolls fades out under what is fixed at its end: the
         * keys under the last column, and the tab bar's tabs under its buttons.
         */
        const val FADE_DP = 32

        /**
         * How far the fade reaches into the last column, ending in the blank before SHIFT, its
         * widest label (about 33 dp of a 45 dp column in Roboto); the rest of [FADE_DP] lies at its
         * left.
         */
        private const val FADE_INTO_LAST_DP = 6

        /** How often a held key repeats after the long-press timeout, as in Termux. */
        private const val REPEAT_MS = 80L

        /** The indicator's width (dp); it is [LINE_DP] high. */
        private const val INDICATOR_WIDTH_DP = 16

        /** How far the indicator lies below its label's baseline (dp). */
        private const val INDICATOR_GAP_DP = 4

        /** The paint for [drawIndicator], in [color]. */
        fun indicatorPaint(color: Int) = Paint(Paint.ANTI_ALIAS_FLAG).apply { this.color = color }

        /**
         * Draws the indicator under [label], which lies in [container] (directly or deeper), for a
         * state that stays until changed: the current tab, a locked modifier, SYM with the symbols
         * or the function keys shown. As in Material's tabs, a short rounded bar, the same width
         * under any label, and a shape, so it shows without telling colours apart. It hangs
         * [INDICATOR_GAP_DP] below the label's baseline, where its letters and digits end, so it
         * stays with the label whatever the height of the bar or the size of the text; it fades
         * with the label. Nothing is drawn when [label] is not in [container]. [dx] moves it right
         * of the label's place (the tab row slides it there after tabs close).
         */
        fun drawIndicator(canvas: Canvas, container: View, label: TextView, paint: Paint, dx: Float = 0f) {
            val r = indicator
            if (!indicatorBounds(container, label, dx, r)) return
            val alpha = paint.alpha
            paint.alpha = (alpha * label.alpha).roundToInt()
            canvas.drawRoundRect(r, r.height() / 2, r.height() / 2, paint)
            paint.alpha = alpha
        }

        /** [drawIndicator]'s bounds, reused: drawing must not allocate. Main thread only. */
        private val indicator = RectF()

        /**
         * Puts where the indicator under [label] lies in [container] ([drawIndicator]) into
         * [out]; false when [label] is not in [container]. The tab row draws a background tab's
         * progress there too.
         */
        fun indicatorBounds(container: View, label: TextView, dx: Float, out: RectF): Boolean {
            // The label's place in the container: its left and top, not
            // offsetDescendantRectToMyCoords, which subtracts each view's scroll, and a single-line
            // label scrolls its text far sideways to centre it
            var x = 0
            var y = 0
            var v: View = label
            while (v !== container) {
                x += v.left
                y += v.top
                v = v.parent as? View ?: return false
            }
            // In whole pixels, rounded down as TabBarView's dp
            val density = label.resources.displayMetrics.density
            val width = (INDICATOR_WIDTH_DP * density).toInt()
            val height = label.line()
            val left = x + (label.width - width) / 2f + dx
            val top = (y + label.baseline + (INDICATOR_GAP_DP * density).toInt()).toFloat()
            out.set(left, top, left + width, top + height)
            return true
        }
    }
}
