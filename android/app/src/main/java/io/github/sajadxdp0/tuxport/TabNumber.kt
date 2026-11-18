package io.github.sajadxdp0.tuxport

import android.animation.ValueAnimator
import android.annotation.SuppressLint
import android.content.Context
import android.graphics.BlurMaskFilter
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.MaskFilter
import android.widget.TextView
import androidx.core.animation.doOnEnd
import androidx.core.graphics.ColorUtils
import kotlin.math.roundToInt

/**
 * A tab's number in the tab bar ([TabBarView], the android/tabbar map), which rolls to a new
 * one ([roll]) like SwiftUI's numeric text: each digit that changes moves out, fading and
 * blurring, as the new one moves in from the other side and sharpens; a lower number comes in
 * from above, a higher one from below. Digits that stay the same in place stay still, and a
 * number of another length rolls whole.
 *
 * Drawn by hand digit by digit, rolling or not, centred as by its gravity, so that a digit
 * that stays does not shift by a fraction of a pixel as a roll starts or ends.
 */
@SuppressLint("AppCompatCustomView") // The activity is a framework one, like the views it builds
internal class TabNumber(context: Context) : TextView(context) {
    /** The number rolling out; null while none rolls. */
    private var old: String? = null

    /** How far the roll has come, eased: from 0, the old number in place, to 1. */
    private var progress = 0f

    /** 1 when the new number comes in from above, -1 from below. */
    private var direction = 1f

    /** The blur of the digits moving out and of those moving in, null when too slight. */
    private var outBlur: MaskFilter? = null
    private var inBlur: MaskFilter? = null

    val rolling get() = old != null

    /** Rolls to [number] (when not [rolling]); [onEnd] runs once it shows alone. */
    fun roll(number: String, onEnd: () -> Unit) {
        val from = text.toString()
        old = from
        text = number
        direction = if ((number.toIntOrNull() ?: 0) < (from.toIntOrNull() ?: 0)) 1f else -1f
        ValueAnimator.ofFloat(0f, 1f).apply {
            duration = RENUMBER_MS
            interpolator = TabBarView.STANDARD
            addUpdateListener {
                progress = it.animatedValue as Float
                outBlur = blur(progress)
                inBlur = blur(1f - progress)
                invalidate()
            }
            doOnEnd {
                old = null
                outBlur = null
                inBlur = null
                invalidate()
                onEnd()
            }
            start()
        }
    }

    /** A blur by [f] of the most, made here as onDraw must not allocate. */
    private fun blur(f: Float): MaskFilter? {
        val radius = f * textSize * ROLL_BLUR
        return if (radius < 0.5f) null else BlurMaskFilter(radius, BlurMaskFilter.Blur.NORMAL)
    }

    override fun onDraw(canvas: Canvas) {
        val new = text.toString()
        val old = old
        val color = currentTextColor
        val y = baseline.toFloat()
        val shift = textSize * ROLL_DISTANCE * direction
        val newStart = start(new)
        for (i in new.indices) {
            val x = newStart + paint.measureText(new, 0, i)
            if (old == null || stays(old, new, i)) {
                drawDigit(canvas, new, i, x, y, color, 1f, null)
            } else {
                drawDigit(canvas, new, i, x, y - shift * (1f - progress), color, progress, inBlur)
            }
        }
        if (old != null) {
            val oldStart = start(old)
            for (i in old.indices) {
                if (stays(old, new, i)) continue
                val x = oldStart + paint.measureText(old, 0, i)
                drawDigit(canvas, old, i, x, y + shift * progress, color, 1f - progress, outBlur)
            }
        }
        paint.maskFilter = null
    }

    /** Whether [old]'s [i]th digit stays as [new]'s, in its place. */
    private fun stays(old: String, new: String, i: Int) = old.length == new.length && old[i] == new[i]

    /** Where [number] starts when centred between the paddings. */
    private fun start(number: String) =
        paddingLeft + (width - paddingLeft - paddingRight - paint.measureText(number)) / 2

    /** Draws [number]'s [i]th digit at [x], [y] in [color], [shown] opaque. */
    private fun drawDigit(canvas: Canvas, number: String, i: Int, x: Float, y: Float, color: Int, shown: Float, blur: MaskFilter?) {
        paint.color = ColorUtils.setAlphaComponent(color, (Color.alpha(color) * shown).roundToInt())
        paint.maskFilter = blur
        canvas.drawText(number, i, i + 1, x, y, paint)
    }

    private companion object {
        /** How long a tab's number takes to roll to the new one (Material's medium 2). */
        const val RENUMBER_MS = 300L

        /** How far a rolling digit moves, and how much it blurs at most, in its text size. */
        const val ROLL_DISTANCE = 0.7f
        const val ROLL_BLUR = 0.12f
    }
}
