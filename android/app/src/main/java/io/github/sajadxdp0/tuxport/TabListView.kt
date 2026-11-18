package io.github.sajadxdp0.tuxport

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Canvas
import android.text.TextUtils
import android.util.TypedValue
import android.view.Gravity
import android.view.HapticFeedbackConstants
import android.view.View
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import androidx.core.view.ViewCompat
import androidx.recyclerview.widget.DefaultItemAnimator
import androidx.recyclerview.widget.DiffUtil
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.RecyclerView

/** A flat list over the terminal, flush with the tab bar, without resizing the terminal. */
@SuppressLint("ViewConstructor")
internal class TabListView(context: Context, private val target: Target) : FrameLayout(context) {
    interface Target {
        fun show(session: VtSession)
        fun close(session: VtSession)
        fun dismiss()
    }

    // Snapshot mutable session state before diffing, including positional numbers after a close.
    private data class Entry(
        val session: VtSession,
        val number: Int,
        val title: String,
        val running: Boolean,
        val selected: Boolean,
        val progress: VtSession.Progress?,
        val status: VtSession.Status?,
    )

    private val muted = terminalControlColor(selected = false)
    private val primary = terminalControlColor(selected = true)
    private val marks = TabMarks(context)
    private val rows = Rows()
    private val manager = LinearLayoutManager(context)
    private val list = RecyclerView(context).apply {
        layoutManager = manager
        adapter = rows
        itemAnimator = DefaultItemAnimator()
        setBackgroundColor(TERMINAL_BG)
        isVerticalScrollBarEnabled = true
        isScrollbarFadingEnabled = true
        verticalScrollbarThumbDrawable = context.themeDrawable(android.R.attr.scrollbarThumbVertical)
        isFocusableInTouchMode = false
    }

    init {
        visibility = GONE
        importantForAccessibility = IMPORTANT_FOR_ACCESSIBILITY_NO
        ViewCompat.setAccessibilityPaneTitle(this, context.getString(R.string.tab_list))
        // Consume the dismissing tap so it cannot also move the shell's cursor or open a link.
        setOnClickListener { target.dismiss() }
        addView(list, LayoutParams(LayoutParams.MATCH_PARENT, 0, Gravity.TOP))
    }

    /** Reveal the current tab while preserving the terminal's input focus and IME connection. */
    fun open(sessions: List<VtSession>, current: VtSession?) {
        render(sessions, current)
        visibility = VISIBLE
        val position = rows.items.indexOfFirst { it.selected }
        if (position >= 0) manager.scrollToPositionWithOffset(position, 0)
    }

    fun render(sessions: List<VtSession>, current: VtSession?) {
        val next = sessions.mapIndexed { index, session ->
            val title = session.title?.takeIf { it.isNotBlank() } ?: "sh"
            val running = session.isRunning
            Entry(
                session = session,
                number = index + 1,
                title = if (running) title else context.getString(R.string.tab_ended, title),
                running = running,
                selected = session === current,
                progress = session.progress,
                status = session.status,
            )
        }
        rows.replace(next)
        requestLayout()
    }

    /** Update immutable reports without moving the list or rebuilding unchanged rows. */
    fun refreshStatus() {
        if (visibility != VISIBLE) return
        rows.replace(rows.items.map { entry ->
            entry.copy(progress = entry.session.progress, status = entry.session.status)
        })
    }

    /** Hide the list and drop references to sessions other controls may later close. */
    fun close() {
        visibility = GONE
        list.adapter = null
        rows.replace(emptyList())
        list.recycledViewPool.clear()
        list.adapter = rows
    }

    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        // A short landscape/IME viewport leaves the remaining rows scrollable.
        val contentHeight = rows.itemCount.coerceAtMost(MAX_VISIBLE_ROWS) * dp(TabBarView.BAR_DP)
        list.layoutParams.height = minOf(contentHeight, MeasureSpec.getSize(heightMeasureSpec))
        super.onMeasure(widthMeasureSpec, heightMeasureSpec)
    }

    private inner class Rows : RecyclerView.Adapter<Row>() {
        var items = emptyList<Entry>()
            private set

        init { setHasStableIds(true) }

        fun replace(next: List<Entry>) {
            val previous = items
            val diff = DiffUtil.calculateDiff(object : DiffUtil.Callback() {
                override fun getOldListSize() = previous.size
                override fun getNewListSize() = next.size
                override fun areItemsTheSame(old: Int, new: Int) = previous[old].session === next[new].session
                override fun areContentsTheSame(old: Int, new: Int) = previous[old] == next[new]
                // A non-empty payload lets DefaultItemAnimator reuse the row without a cross-fade.
                override fun getChangePayload(old: Int, new: Int) = Unit
            })
            items = next
            diff.dispatchUpdatesTo(this)
        }

        override fun getItemCount() = items.size
        override fun getItemId(position: Int) = items[position].session.id.toLong()
        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) = Row()
        override fun onBindViewHolder(holder: Row, position: Int) = holder.bind(items[position])
        override fun onBindViewHolder(holder: Row, position: Int, payloads: MutableList<Any>) =
            holder.bind(items[position], partial = payloads.isNotEmpty())
    }

    private inner class Row : RecyclerView.ViewHolder(LinearLayout(context)) {
        private var entry: Entry? = null
        private val number = label(NUMBER_DP).apply { gravity = Gravity.CENTER }
        private val title = label(TabBarView.BAR_LABEL_DP).apply {
            gravity = Gravity.CENTER_VERTICAL
            ellipsize = TextUtils.TruncateAt.END
            setPadding(0, 0, dp(8), 0)
        }
        private val select = object : LinearLayout(context) {
            override fun dispatchDraw(canvas: Canvas) {
                super.dispatchDraw(canvas)
                val shown = entry ?: return
                if (shown.running && marks.draw(canvas, this, number, shown.progress, shown.status)) {
                    postInvalidateOnAnimation()
                }
            }
        }.apply {
            orientation = LinearLayout.HORIZONTAL
            background = touchFeedback()
            isFocusable = true
            addView(number, square())
            addView(title, LinearLayout.LayoutParams(0, LayoutParams.MATCH_PARENT, 1f))
        }
        private val close = label(TabBarView.BAR_LABEL_DP).apply {
            text = "×"
            gravity = Gravity.CENTER
            setTextColor(muted)
            background = touchFeedback()
            importantForAccessibility = IMPORTANT_FOR_ACCESSIBILITY_YES
            isFocusable = true
        }

        init {
            (itemView as LinearLayout).apply {
                orientation = LinearLayout.HORIZONTAL
                layoutParams = RecyclerView.LayoutParams(LayoutParams.MATCH_PARENT, dp(TabBarView.BAR_DP))
                addView(select, LinearLayout.LayoutParams(0, LayoutParams.MATCH_PARENT, 1f))
                addView(close, square())
            }
            // Use the bound session even when adapter positions change before the next bind.
            select.setOnClickListener {
                val session = entry?.session ?: return@setOnClickListener
                it.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                target.show(session)
            }
            close.setOnClickListener {
                val session = entry?.session ?: return@setOnClickListener
                it.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                target.close(session)
            }
        }

        fun bind(entry: Entry, partial: Boolean = false) {
            // Compare with the last bound snapshot so merged payloads use the latest state.
            val previous = if (partial) this.entry else null
            this.entry = entry
            if (previous?.number != entry.number) number.text = "%d".format(entry.number)
            if (previous?.title != entry.title) title.text = entry.title
            if (previous?.selected != entry.selected) {
                val color = if (entry.selected) primary else muted
                number.setTextColor(color)
                title.setTextColor(color)
                select.isSelected = entry.selected
            }
            if (previous?.running != entry.running) select.alpha = if (entry.running) 1f else ENDED_ALPHA
            val status = marks.describe(entry.running, entry.progress, entry.status)
            select.contentDescription = if (status == null) {
                context.getString(R.string.tab_description, entry.number, entry.title)
            } else {
                context.getString(R.string.tab_description_status, entry.number, entry.title, status)
            }
            select.invalidate()
            close.contentDescription = context.getString(R.string.close_tab_description, entry.number, entry.title)
        }
    }

    /** Numbers and close buttons have the same square bounds as the navigation controls. */
    private fun square() = LinearLayout.LayoutParams(dp(TabBarView.BAR_DP), LayoutParams.MATCH_PARENT)

    private fun label(size: Float) = TextView(context).apply {
        setTextSize(TypedValue.COMPLEX_UNIT_DIP, size)
        setSingleLine()
        importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
    }

    companion object {
        private const val MAX_VISIBLE_ROWS = 10
        private const val NUMBER_DP = 14f
        private const val ENDED_ALPHA = 0.38f
    }
}
