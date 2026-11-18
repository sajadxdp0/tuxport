package io.github.sajadxdp0.tuxport

import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Rect
import android.graphics.RectF
import android.view.View
import android.widget.TextView
import androidx.core.graphics.ColorUtils
import kotlin.math.roundToInt

/** Shared progress and status marks for the tab bar and the tab list's numbered cells. */
internal class TabMarks(private val context: Context) {
    private val density = context.resources.displayMetrics.density
    private val progressColors = ProgressLine.Colors(context)
    private val currentColor = context.themeColor(androidx.appcompat.R.attr.colorPrimary)
    private val errorColor = context.themeColor(androidx.appcompat.R.attr.colorError)
    private val mutedColor = context.themeColor(com.google.android.material.R.attr.colorOnSurfaceVariant)
    private val markPaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val trackPaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val markBounds = RectF()
    private val digitBounds = Rect()

    /** Draw over a direct child number, following its slide and opacity; true while sweeping. */
    fun draw(
        canvas: Canvas, container: View, view: TextView,
        progress: VtSession.Progress?, status: VtSession.Status?,
    ): Boolean {
        val moving = drawProgress(canvas, container, view, progress, status)
        drawDot(canvas, view, status)
        return moving
    }

    private fun drawProgress(
        canvas: Canvas, container: View, view: TextView,
        progress: VtSession.Progress?, status: VtSession.Status?,
    ): Boolean {
        val fraction = when {
            progress != null -> ProgressLine.fractionOf(progress)
            status?.state == VtSession.STATUS_WORKING -> {
                if (status.percent >= 0) status.percent / 100f else -1f
            }
            status?.state == VtSession.STATUS_BLOCKED && status.percent >= 0 -> status.percent / 100f
            else -> return false
        }
        if (!ExtraKeysView.indicatorBounds(container, view, view.translationX, markBounds)) return false
        val color = progressColors.of(progress?.state ?: VtSession.PROGRESS_SET)
        markPaint.color = faded(color, view.alpha)
        trackPaint.color = faded(ColorUtils.setAlphaComponent(color, ProgressLine.TRACK_ALPHA), view.alpha)
        return ProgressLine.draw(canvas, markBounds, fraction, markPaint, trackPaint, rounded = true)
    }

    private fun drawDot(canvas: Canvas, view: TextView, status: VtSession.Status?) {
        val dot = when (status?.state) {
            VtSession.STATUS_BLOCKED -> currentColor
            VtSession.STATUS_ERROR -> errorColor
            VtSession.STATUS_DONE -> mutedColor
            else -> return
        }
        val text = view.text
        if (digitBounds.isEmpty) view.paint.getTextBounds("0", 0, 1, digitBounds)
        val radius = DOT_RADIUS_DP * density
        val textRight = view.left + view.translationX + (view.width + view.paint.measureText(text, 0, text.length)) / 2
        markPaint.color = faded(dot, view.alpha)
        val x = textRight + DOT_GAP_DP * density + radius
        val y = (view.top + view.baseline + digitBounds.top).toFloat()
        canvas.drawCircle(x, y, radius, markPaint)
    }

    private fun faded(color: Int, shown: Float) =
        ColorUtils.setAlphaComponent(color, (Color.alpha(color) * shown).roundToInt())

    /**
     * What TalkBack adds to a tab's title for its program's progress or status: what it says it
     * is doing or needs ([VtSession.describe]), working for progress (failed or paused as
     * reported), with the percentage when there is one; null with nothing to say.
     */
    fun describe(running: Boolean, progress: VtSession.Progress?, report: VtSession.Status?): String? {
        if (!running) return null
        val status = report?.takeIf { it.state != VtSession.STATUS_IDLE }
        val what = when {
            status != null && status.state != VtSession.STATUS_WORKING -> VtSession.describe(context, status)
            progress?.state == VtSession.PROGRESS_ERROR -> context.getString(R.string.status_error)
            progress?.state == VtSession.PROGRESS_PAUSE -> context.getString(R.string.status_paused)
            progress != null || status != null -> context.getString(R.string.status_working)
            else -> return null
        }
        val percent = progress?.takeIf { it.state != VtSession.PROGRESS_INDETERMINATE }?.percent?.takeIf { it >= 0 }
            ?: status?.percent?.takeIf { it >= 0 }
        return if (percent == null) what else context.getString(R.string.status_percent, what, percent)
    }

    companion object {
        private const val DOT_RADIUS_DP = 1.5f
        private const val DOT_GAP_DP = 2f
    }
}
