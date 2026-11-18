package io.github.sajadxdp0.tuxport

import android.animation.ValueAnimator
import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Canvas
import android.graphics.Rect
import android.graphics.drawable.InsetDrawable
import android.util.TypedValue
import android.view.Gravity
import android.view.HapticFeedbackConstants
import android.view.MotionEvent
import android.view.View
import android.view.ViewGroup
import android.view.ViewTreeObserver
import android.view.animation.PathInterpolator
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.TextView
import androidx.core.graphics.drawable.toDrawable
import androidx.core.view.updateLayoutParams
import io.github.sajadxdp0.tuxport.TabBarView.Companion.LEAVE_SCALE

/**
 * The tab bar, one row in any window: a scrolling row of tabs, each a square with its number (a
 * long press shows its title), with an optional `⧉` fixed at the left and `×` (closes the current tab), `+`,
 * Import and `⋮` over its end. Tabs fade into the left part of `×` while more lie beyond. Scrolled away from the
 * current tab, the row brings it back into view a few seconds after it comes to rest. When tabs
 * close, the ones after them slide into their places and then roll to their new numbers. The
 * other tabs show what their programs report: progress under the number, a dot for a program
 * that waits for the user, failed or is done ([drawMarks]).
 *
 * It shows the sessions it is given ([render]) and passes taps on to a [Target]: the sessions,
 * and which of them is current, belong to the activity. The android/tabbar map.
 */
@SuppressLint("ViewConstructor") // Created only in code, never in layout files
internal class TabBarView(context: Context, private val target: Target) : FrameLayout(context) {

    /** Where taps on the bar go: the activity, which owns the sessions. */
    interface Target {
        /** `⧉`: toggle the list of tab titles. */
        fun listTabs()

        /** A tab was tapped. */
        fun show(session: VtSession)

        /** `×`: the current tab is to close. */
        fun closeCurrent()

        /** `+`. */
        fun newTab()

        /** `@`: select files to import. */
        fun importFiles()

        /** `⋮`: its menu, at [anchor]. */
        fun menu(anchor: View)
    }

    /** The current tab's session, as last rendered; null with none. */
    private var current: VtSession? = null

    /** [current]'s number in the row, which the row draws the indicator under; null with none. */
    private var currentTab: TextView? = null

    /** The row's tabs in its order; a closed one leaves the list as it starts fading out. */
    private val rowTabs = mutableListOf<Tab>()

    /**
     * Where each tab's place in the row was on screen before the change that [slideTabs] animates,
     * or null with none waiting for the next layout. Taken at the first change before that layout:
     * closing the current tab shows the next one, which renders the row once more.
     */
    private var slideFrom: Map<Tab, Int>? = null

    /** Where the current tab's place was on screen, taken with [slideFrom]; null with no tab. */
    private var indicatorFrom: Int? = null

    /** How far right of [currentTab]'s place the indicator is drawn while it slides there. */
    private var indicatorShift = 0f
    private var indicatorSlide: ValueAnimator? = null

    /** Whether a finger is on the row, which holds off [returnToCurrentTab]. */
    private var touched = false

    /** Brings the current tab back into view a while after the row was scrolled away from it. */
    private val returnToCurrentTab = Runnable { revealCurrentTab() }

    // On black like the terminal; the theme's colours, from the wallpaper on Android 12 and later,
    // as the extra keys'

    /** The buttons and the other tabs' numbers. */
    private val mutedColor = terminalControlColor(selected = false)

    /** The current tab's number and indicator. */
    private val currentColor = terminalControlColor(selected = true)
    private val indicatorPaint = ExtraKeysView.indicatorPaint(currentColor)

    private val marks = TabMarks(context)

    /**
     * The tabs, with the current one's indicator and the others' marks drawn over them, as the
     * extra keys draw their indicators.
     */
    private val row = object : LinearLayout(context) {
        override fun dispatchDraw(canvas: Canvas) {
            super.dispatchDraw(canvas)
            currentTab?.let { ExtraKeysView.drawIndicator(canvas, this, it, indicatorPaint, indicatorShift) }
            if (drawMarks(canvas)) postInvalidateOnAnimation()
        }
    }.apply { orientation = LinearLayout.HORIZONTAL }

    /**
     * The row, scrolling. It runs under the buttons to the bar's end; its end padding, as wide as
     * the buttons, lets the last tab scroll out from under them. While tabs lie beyond the padding,
     * the right fading edge hints that the row scrolls: a padding offset carries it into the blank
     * left part of ×, so the tabs fade out under the button rather than at its edge. Scrolled away
     * from the current tab, the row brings it back once it has rested for a while.
     */
    private val scroll = object : HorizontalScrollView(context) {
        override fun getLeftFadingEdgeStrength() = 0f
        override fun isPaddingOffsetRequired() = true
        override fun getRightPaddingOffset() = dp(FADE_INTO_CLOSE_DP)

        // A finger on the row, on a tab too, holds the return off; lifting it starts the wait
        override fun dispatchTouchEvent(ev: MotionEvent): Boolean {
            when (ev.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    touched = true
                    removeCallbacks(returnToCurrentTab)
                }
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                    touched = false
                    returnToCurrentTabLater()
                }
            }
            return super.dispatchTouchEvent(ev)
        }

        // Every move without a finger (a fling, TalkBack's scroll actions) starts it again
        override fun onScrollChanged(l: Int, t: Int, oldl: Int, oldt: Int) {
            super.onScrollChanged(l, t, oldl, oldt)
            if (!touched) returnToCurrentTabLater()
        }
    }.apply {
        isHorizontalScrollBarEnabled = false
        isHorizontalFadingEdgeEnabled = true
        setFadingEdgeLength(dp(ExtraKeysView.FADE_DP))
        clipToPadding = false
        // A closed last tab fades out past the end of the shortened row (Tab.leave)
        clipChildren = false
        addView(row, FrameLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.MATCH_PARENT))
    }

    private val listButton = button("⧉") { target.listTabs() }.apply {
        contentDescription = context.getString(R.string.tab_list)
        tooltipText = contentDescription
    }

    /**
     * `×`, hidden while there is no current tab. The multiplication sign, which Roboto draws like
     * its +; U+2715 (✕) is not in Roboto, and the fallback font's is larger and thinner than + and ⋮.
     */
    private val closeButton = button("×") { target.closeCurrent() }.apply {
        contentDescription = context.getString(R.string.close_tab)
        tooltipText = contentDescription
    }

    private val importButton = button("@") { target.importFiles() }.apply {
        contentDescription = context.getString(R.string.import_files)
        tooltipText = contentDescription
    }

    /**
     * `×`, `+`, Import and `⋮` over the row's end. Black but for the left part of × the fade reaches into,
     * so the tabs under the buttons are hidden, and taking every touch there, so a tab under the
     * hidden × cannot be tapped; TalkBack skips the group and reads each button.
     */
    private val buttons = LinearLayout(context).apply {
        orientation = LinearLayout.HORIZONTAL
        background = InsetDrawable(TERMINAL_BG.toDrawable(), dp(FADE_INTO_CLOSE_DP), 0, 0, 0)
        // The inset comes as padding with the background, which would widen the group to the
        // left by as much and keep the black at the left of ×
        setPadding(0, 0, 0, 0)
        isClickable = true
        importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
        addView(closeButton, item())
        addView(button("+") { target.newTab() }.apply {
            contentDescription = context.getString(R.string.new_tab)
            tooltipText = contentDescription
        }, item())
        addView(importButton, item())
        addView(button("⋮") { target.menu(it) }.apply {
            contentDescription = context.getString(R.string.more_options)
            tooltipText = contentDescription
        }, item())
    }

    // With gesture navigation, a slide from the screen's left edge over the tabs scrolls them
    // instead of going back; the buttons, which do not scroll, leave the right edge's gesture as
    // it is. Reused: the framework copies the list and reads the rectangle when it updates, after
    // each set
    private val excluded = Rect()
    private val exclusion = listOf(excluded)

    init {
        setBackgroundColor(TERMINAL_BG)
        scroll.setPadding(0, 0, buttons.childCount * dp(BAR_DP), 0)
        addView(scroll, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT).apply { marginStart = dp(BAR_DP) })
        addView(listButton, LayoutParams(dp(BAR_DP), LayoutParams.MATCH_PARENT, Gravity.START))
        addView(buttons, LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.MATCH_PARENT, Gravity.END))
    }

    /** [BAR_DP] tall, whatever the parent offers. */
    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        super.onMeasure(widthMeasureSpec, MeasureSpec.makeMeasureSpec(dp(BAR_DP), MeasureSpec.EXACTLY))
    }

    override fun onLayout(changed: Boolean, l: Int, t: Int, r: Int, b: Int) {
        super.onLayout(changed, l, t, r, b)
        excluded.set(scroll.left, 0, width - scroll.paddingRight, height)
        systemGestureExclusionRects = exclusion
    }

    /**
     * Shows [sessions], a number each, [current] marked (the indicator sliding to it from the last
     * current one, [moveIndicator]) and scrolled into view. Called whenever tabs, titles or the
     * current tab change.
     *
     * Closed tabs shrink and fade out in place, and the tabs after them slide into the places left,
     * keeping their numbers until they are there ([slideTabs]): closing the current tab shows the
     * next one in its place, under the same number, and it is seen arriving.
     */
    fun render(sessions: List<VtSession>, current: VtSession?) {
        this.current = current
        if (slideFrom == null && row.isLaidOut && rowTabs.isNotEmpty()) {
            slideFrom = rowTabs.associateWith { it.view.left - scroll.scrollX }
            indicatorFrom = currentTab?.let { it.left - scroll.scrollX }
            row.viewTreeObserver.addOnPreDrawListener(slideTabs)
        }
        // Out of the list first, so that they renumber no more. Sessions are added only at the end,
        // so the tabs left keep their order
        val closed = rowTabs.filter { it.session !in sessions }
        rowTabs.removeAll(closed)
        closed.forEach { it.leave() }
        sessions.forEachIndexed { i, session ->
            if (i == rowTabs.size) rowTabs.add(Tab(session, i).also { row.addView(it.view, item()) })
            rowTabs[i].update(i)
        }
        closeButton.visibility = if (current == null) View.INVISIBLE else View.VISIBLE
        listButton.isEnabled = sessions.isNotEmpty()
        listButton.alpha = if (sessions.isEmpty()) 0.38f else 1f

        // Indicate the current tab (the row draws the indicator), and bring it into view once the
        // new row is laid out (this runs after the layout pass that changing the tabs requested).
        // Not through returnToCurrentTab: a scroll in that layout (closing tabs can shorten the row)
        // would put it off
        currentTab = rowTabs.getOrNull(sessions.indexOf(current))?.view
        row.invalidate()
        if (currentTab != null) scroll.post { revealCurrentTab() }
    }

    fun setImportEnabled(enabled: Boolean) {
        importButton.isEnabled = enabled
        importButton.alpha = if (enabled) 1f else 0.38f
    }

    fun setListOpen(open: Boolean) {
        listButton.isSelected = open
        listButton.setTextColor(if (open) currentColor else mutedColor)
    }

    fun setListVisible(visible: Boolean) {
        val visibility = if (visible) View.VISIBLE else View.GONE
        if (listButton.visibility == visibility) return
        listButton.visibility = visibility
        scroll.updateLayoutParams<LayoutParams> { marginStart = if (visible) dp(BAR_DP) else 0 }
        scroll.post { revealCurrentTab() }
    }

    /**
     * Once the row is laid out after a change, slides each tab from where its place was on screen
     * to its new one, then gives it the number of that place ([Tab.renumber]). A closed tab keeps
     * fading out where it was, though the row's scroll moved: a shorter row may scroll less.
     */
    private val slideTabs = object : ViewTreeObserver.OnPreDrawListener {
        override fun onPreDraw(): Boolean {
            row.viewTreeObserver.removeOnPreDrawListener(this)
            val from = slideFrom ?: return true
            slideFrom = null
            for ((tab, x) in from) {
                val shift = x - (tab.view.left - scroll.scrollX)
                tab.view.translationX += shift
                // A tab whose place stays on screen but whose number changes waits as long as
                // those that slide (a 0 to 0 slide); one still sliding renumbers at its end
                if (tab in rowTabs && (shift != 0 || tab.stale && tab.view.translationX == 0f)) tab.slide()
            }
            moveIndicator()
            return true
        }
    }

    /**
     * Moves the indicator to the current tab's new place on screen ([slideTabs]), sliding there
     * from where it was drawn, whether another tab was shown or tabs closed; a change while it
     * slides (a held Ctrl+Tab) slides on from where it is. Closing the current tab of a row that
     * does not scroll leaves its place where it was, so the indicator stays while the next tab
     * slides in above it; a row scrolled to its end scrolls back as it shortens, and the indicator
     * slides on to the next tab. With no place to come from (the row's first layout) it is put
     * there at once.
     */
    private fun moveIndicator() {
        val tab = currentTab
        val from = indicatorFrom
        indicatorFrom = null
        val shift = if (tab == null || from == null) null else from - (tab.left - scroll.scrollX)
        when (shift) {
            0 -> {} // the same place: a slide still running goes on
            null -> {
                indicatorSlide?.cancel()
                indicatorShift = 0f
            }
            else -> {
                indicatorSlide?.cancel()
                indicatorSlide = ValueAnimator.ofFloat(indicatorShift + shift, 0f).apply {
                    duration = SLIDE_MS
                    interpolator = STANDARD
                    addUpdateListener {
                        indicatorShift = it.animatedValue as Float
                        row.invalidate()
                    }
                    start()
                }
            }
        }
    }

    /**
     * Scrolls the row to bring the current tab into view, clear of the fade towards the buttons;
     * nothing when it already is, or when there is none.
     */
    private fun revealCurrentTab() {
        val tab = currentTab ?: return
        val fade = scroll.horizontalFadingEdgeLength
        val shownWidth = scroll.width - scroll.paddingRight
        val end = row.width - shownWidth
        // Where the fade begins, from the row's visible left edge
        val clearWidth = shownWidth + dp(FADE_INTO_CLOSE_DP) - fade
        when {
            tab.left < scroll.scrollX -> scroll.smoothScrollTo(tab.left, 0)
            tab.right > scroll.scrollX + clearWidth -> {
                val x = tab.right - clearWidth
                // Less than a fade's width of tabs left beyond the padding would fade only inside
                // ×, leaving the last tab barely faded against it: go to the row's end, where
                // nothing fades
                scroll.smoothScrollTo(if (end - x < fade) end else x, 0)
            }
        }
    }

    /** Starts, or starts again, the wait before [returnToCurrentTab]. */
    private fun returnToCurrentTabLater() {
        scroll.removeCallbacks(returnToCurrentTab)
        scroll.postDelayed(returnToCurrentTab, RETURN_TO_CURRENT_TAB_MS)
    }

    /**
     * A tab's progress or program status changed: redraws the marks ([drawMarks]) and updates what
     * TalkBack reads. Numbers are left alone, as tabs may be on their way to new ones.
     */
    fun refreshStatus() {
        rowTabs.forEachIndexed { i, tab -> tab.describe(i) }
        row.invalidate()
    }

    /** Background tabs keep their marks separate from the current tab's selection indicator. */
    private fun drawMarks(canvas: Canvas): Boolean {
        var moving = false
        for (tab in rowTabs) {
            val session = tab.session
            if (session === current || !session.isRunning) continue
            moving = marks.draw(canvas, row, tab.view, session.progress, session.status) || moving
        }
        return moving
    }

    /**
     * A tab in the row: its number (from [i]) in a square as tall as the bar, the current one in
     * the primary colour above the row's indicator ([currentTab]), an ended one faded. Tap shows
     * it; a long press shows its title.
     *
     * The indicator goes by the current tab's place in the row ([moveIndicator]), not by where the
     * tab is while it slides, and a changing number rolls inside the view ([TabNumber]), whose
     * opacity the indicator takes, so it leaves the indicator as it is.
     */
    private inner class Tab(val session: VtSession, i: Int) {
        val view = TabNumber(context).apply {
            text = number(i)
            setTextSize(TypedValue.COMPLEX_UNIT_DIP, TAB_NUMBER_DP)
            gravity = Gravity.CENTER
            foreground = touchFeedback()
            setOnClickListener {
                it.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                target.show(session)
            }
        }

        /**
         * Whether it shows another number than that of its place in the row: it is still on its
         * way there.
         */
        val stale get() = rowTabs.indexOf(this).let { it >= 0 && view.text.toString() != number(it) }

        /**
         * Brings it up to date as the [i]th tab; its number waits while a slide is to be worked
         * out ([slideTabs]), the description (TalkBack) does not.
         */
        fun update(i: Int) {
            describe(i)
            view.tooltipText = title(session)
            view.alpha = if (session.isRunning) 1f else ENDED_ALPHA
            if (slideFrom?.containsKey(this) != true) view.text = number(i)
            view.setTextColor(if (session === current) currentColor else mutedColor)
        }

        /** What TalkBack reads for it as the [i]th tab: its number, title and status. */
        fun describe(i: Int) {
            val status = marks.describe(session.isRunning, session.progress, session.status)
            view.contentDescription = if (status == null) {
                context.getString(R.string.tab_description, i + 1, title(session))
            } else {
                context.getString(R.string.tab_description_status, i + 1, title(session), status)
            }
        }

        /** Slides to its place in the row, then takes the number of that place. */
        fun slide() {
            view.animate().translationX(0f).setDuration(SLIDE_MS).setInterpolator(STANDARD).withEndAction { renumber() }
        }

        /**
         * Rolls its number to the number of its place, when they differ. A change while it rolls
         * is taken at its end.
         */
        fun renumber() {
            if (!view.rolling && stale) view.roll(number(rowTabs.indexOf(this))) { renumber() }
        }

        /**
         * Out of the row, into its overlay, where it shrinks and fades out in its place; a roll
         * under way goes on.
         */
        fun leave() {
            // A slide stops where it is
            view.animate().cancel()
            row.overlay.add(view)
            view.animate().alpha(0f).scaleX(LEAVE_SCALE).scaleY(LEAVE_SCALE)
                .setDuration(LEAVE_MS).setInterpolator(ACCELERATE)
                .withEndAction { row.overlay.remove(view) }
        }
    }

    private fun number(i: Int) = "%d".format(i + 1)

    /** The program's title (`sh` when it set none), marked once the shell has ended. */
    private fun title(session: VtSession): String {
        val text = session.title?.takeIf { it.isNotBlank() } ?: "sh"
        return if (session.isRunning) text else context.getString(R.string.tab_ended, text)
    }

    /** A button: a square as tall as the bar ([item]) with its label centred. */
    private fun button(label: String, onClick: (View) -> Unit) = TextView(context).apply {
        text = label
        setTextColor(mutedColor)
        setTextSize(TypedValue.COMPLEX_UNIT_DIP, BAR_LABEL_DP)
        gravity = Gravity.CENTER
        background = touchFeedback()
        setOnClickListener {
            it.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
            onClick(it)
        }
    }

    /** A tab or a button: a square as tall as the bar. */
    private fun item() = LinearLayout.LayoutParams(dp(BAR_DP), ViewGroup.LayoutParams.MATCH_PARENT)

    companion object {
        /** The bar's height, and the side of its tabs and buttons (the search bar's too). */
        const val BAR_DP = 32

        /**
         * The size of the buttons' labels. In dp, not sp: the bar's height is fixed, so its labels
         * ignore the system font scale. The search bar's text and buttons take it too.
         */
        const val BAR_LABEL_DP = 14f

        /** The size of the tabs' numbers (dp, like [BAR_LABEL_DP]). */
        private const val TAB_NUMBER_DP = 14f

        /**
         * How far the fade reaches into `×`, ending in the blank before its glyph; the rest of
         * [ExtraKeysView.FADE_DP] lies at its left.
         */
        private const val FADE_INTO_CLOSE_DP = 12

        /** The opacity of an ended tab's number (Material's disabled content). */
        private const val ENDED_ALPHA = 0.38f

        /**
         * How long the row rests, with no finger on it, before it brings a current tab it was
         * scrolled away from back into view; longer than a tab's title stays up after a long press.
         */
        private const val RETURN_TO_CURRENT_TAB_MS = 3000L

        /** How long tabs take to slide into the places of closed ones (Material's short 4). */
        private const val SLIDE_MS = 200L

        /** How long a closed tab takes to shrink to [LEAVE_SCALE] and fade out. */
        private const val LEAVE_MS = 150L
        private const val LEAVE_SCALE = 0.6f

        /** Material's standard easing, for the slides and the roll ([TabNumber]). */
        val STANDARD = PathInterpolator(0.2f, 0f, 0f, 1f)

        /** Material's standard accelerate easing, for a closed tab on its way out. */
        private val ACCELERATE = PathInterpolator(0.3f, 0f, 1f, 1f)
    }
}
