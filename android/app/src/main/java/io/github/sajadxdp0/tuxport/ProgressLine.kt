package io.github.sajadxdp0.tuxport

import android.animation.ValueAnimator
import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.RectF
import android.os.SystemClock
import android.view.View
import android.view.animation.PathInterpolator
import androidx.core.graphics.ColorUtils
import com.google.android.material.color.MaterialColors
import io.github.sajadxdp0.tuxport.ProgressLine.Companion.SWEEP_MS

/**
 * The line along the top of the terminal that shows the current tab's progress report (OSC 9;4)
 * as Ghostty shows it, but as thin as the tab bar's indicator ([LINE_DP], half of Ghostty's 2
 * points): over the terminal's first row, filled from the left to the percentage in the primary
 * colour, the error colour for an error, the muted colour while paused (full when a pause gives
 * no percentage); with no percentage, a quarter of its width sweeps across and back over a faint
 * track. It fades in and out, and a new percentage slides there. It takes no touches. The
 * android/tabbar map.
 *
 * The companion draws progress into any rectangle, for the tab bar's marks too.
 */
@SuppressLint("ViewConstructor") // Created only in code, never in layout files
internal class ProgressLine(context: Context) : View(context) {

    /** The theme's colours for progress, on black like the terminal. */
    class Colors(context: Context) {
        private val primary = MaterialColors.getColor(context, androidx.appcompat.R.attr.colorPrimary, Color.WHITE)
        private val error = MaterialColors.getColor(context, androidx.appcompat.R.attr.colorError, Color.RED)
        private val muted = MaterialColors.getColor(context, com.google.android.material.R.attr.colorOnSurfaceVariant, Color.GRAY)

        /** The colour of progress in [state] (a `VtSession.PROGRESS_` constant). */
        fun of(state: Int) = when (state) {
            VtSession.PROGRESS_ERROR -> error
            VtSession.PROGRESS_PAUSE -> muted
            else -> primary
        }
    }

    /** The report shown, null when none (the line may still be fading out with [drawn]). */
    private var progress: VtSession.Progress? = null

    /** The report drawn: [progress], or the last one while the line fades out. */
    private var drawn: VtSession.Progress? = null

    /** The fraction filled, sliding to that of [progress]; negative for a sweep. */
    private var fraction = 0f
    private var slide: ValueAnimator? = null

    private val colors = Colors(context)
    private val fill = Paint(Paint.ANTI_ALIAS_FLAG)
    private val track = Paint(Paint.ANTI_ALIAS_FLAG)
    private val bounds = RectF()

    init {
        alpha = 0f
        importantForAccessibility = IMPORTANT_FOR_ACCESSIBILITY_NO
    }

    /**
     * Shows [p], or hides the line for null; a percentage slides from the one shown, unless
     * [jump] (another tab is shown).
     */
    fun show(p: VtSession.Progress?, jump: Boolean) {
        // Another tab's equal report still ends the slide and the fade in of the one before
        if (p == progress && (!jump || p == null)) return
        progress = p
        slide?.cancel()
        if (p == null) {
            animate().alpha(0f).setDuration(FADE_MS).withEndAction { if (progress == null) drawn = null }
            return
        }
        val to = fractionOf(p)
        // A percentage slides only from another one on the screen
        if (!jump && drawn != null && alpha > 0f && to >= 0f && fraction >= 0f) {
            slide = ValueAnimator.ofFloat(fraction, to).apply {
                duration = SLIDE_MS
                interpolator = EASE_IN_OUT
                addUpdateListener {
                    fraction = it.animatedValue as Float
                    invalidate()
                }
                start()
            }
        } else {
            fraction = to
        }
        drawn = p
        animate().alpha(1f).setDuration(if (jump) 0L else FADE_MS)
        invalidate()
    }

    override fun onDraw(canvas: Canvas) {
        val p = drawn ?: return
        fill.color = colors.of(p.state)
        track.color = ColorUtils.setAlphaComponent(fill.color, TRACK_ALPHA)
        bounds.set(0f, 0f, width.toFloat(), height.toFloat())
        // A faint track only under the sweep, as Ghostty draws it
        if (draw(canvas, bounds, fraction, fill, if (fraction < 0f) track else null, rounded = false)) {
            postInvalidateOnAnimation()
        }
    }

    companion object {
        /** How opaque the track under a sweep is, as Ghostty's 30%. */
        const val TRACK_ALPHA = 77

        /** How long the line fades in or out (Material's short 3). */
        private const val FADE_MS = 150L

        /** How long a new percentage takes to slide in (Ghostty's 0.2 s). */
        private const val SLIDE_MS = 200L

        /** How long a sweep takes one way (Ghostty's 1.2 s), and the part of the width it covers. */
        private const val SWEEP_MS = 1200L
        private const val SWEEP_WIDTH = 0.25f

        /** SwiftUI's ease-in-out, Ghostty's curve for the sweep and the slide. */
        private val EASE_IN_OUT = PathInterpolator(0.42f, 0f, 0.58f, 1f)

        /**
         * The fraction [p] fills: its percentage, all of it for a pause without one, or -1 for a
         * sweep (no percentage, or indeterminate whatever the percentage).
         */
        fun fractionOf(p: VtSession.Progress) = when {
            p.state == VtSession.PROGRESS_INDETERMINATE -> -1f
            p.percent >= 0 -> p.percent / 100f
            p.state == VtSession.PROGRESS_PAUSE -> 1f
            else -> -1f
        }

        /**
         * Draws progress into [r]: [fraction] of it filled with [fill] from the left, or, for a
         * negative [fraction], a quarter of it sweeping across and back (one way per [SWEEP_MS]),
         * timed by the clock so that every sweep drawn moves together. [track], when given, fills
         * the rest first; [rounded] rounds the ends as the tab bar's indicator. Returns whether
         * the drawing moves, so the caller draws again on the next frame.
         */
        fun draw(canvas: Canvas, r: RectF, fraction: Float, fill: Paint, track: Paint?, rounded: Boolean): Boolean {
            val radius = if (rounded) r.height() / 2 else 0f
            if (track != null) canvas.drawRoundRect(r, radius, radius, track)
            val left: Float
            val right: Float
            if (fraction < 0f) {
                val t = (SystemClock.uptimeMillis() % (2 * SWEEP_MS)).toFloat() / SWEEP_MS
                val at = EASE_IN_OUT.getInterpolation(if (t <= 1f) t else 2f - t)
                val width = r.width() * SWEEP_WIDTH
                left = r.left + at * (r.width() - width)
                right = left + width
            } else {
                left = r.left
                right = r.left + r.width() * fraction.coerceAtMost(1f)
            }
            if (right > left) canvas.drawRoundRect(left, r.top, right, r.bottom, radius, radius, fill)
            return fraction < 0f
        }
    }
}
