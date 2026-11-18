package io.github.sajadxdp0.tuxport

import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.DashPathEffect
import android.graphics.Paint
import android.graphics.Path
import android.graphics.Typeface
import androidx.core.graphics.createBitmap
import androidx.core.graphics.withClip
import androidx.core.graphics.withScale
import io.github.sajadxdp0.tuxport.VtRenderer.Companion.MARK_MATCH
import io.github.sajadxdp0.tuxport.VtRenderer.Companion.MARK_NONE
import io.github.sajadxdp0.tuxport.VtRenderer.Companion.MARK_SELECTED
import kotlin.math.ceil
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * How [VtView] draws its [VtScreen] (the android/vtrender map): with JetBrains Mono on a fixed cell
 * grid, each row a frame brings painted into a bitmap the size of the grid (and the overscan row),
 * and the cursor and the input method's composing text drawn over it.
 *
 * Glyphs are placed cell by cell: runs of ASCII share one draw call (the font size is chosen so
 * their advance is exactly one cell), every other glyph is centred in its one or two cells,
 * enlarged evenly to fill two cells when it is wide (the setting, for CJK fallback glyphs narrower
 * than two cells) or shrunk evenly when it is wider, never stretched. Box drawing, block elements
 * and the Powerline triangles are drawn as shapes ([VtGlyphs]). Images programs show
 * ([VtImages]) are painted into the rows they reach, below the backgrounds, below the text or above
 * it as the frame places them.
 */
internal class VtRenderer(
    private val context: Context,
    private val screen: VtScreen,
    private val images: VtImages,
) {
    private val paints = arrayOf(
        textPaint(font("fonts/JetBrainsMono.ttf", 400)),
        textPaint(font("fonts/JetBrainsMono.ttf", 700)),
        textPaint(font("fonts/JetBrainsMono-Italic.ttf", 400)),
        textPaint(font("fonts/JetBrainsMono-Italic.ttf", 700)),
    )
    private val fill = Paint()
    private val stroke = Paint(Paint.ANTI_ALIAS_FLAG).apply { style = Paint.Style.STROKE }
    private val curl = Path()
    private val glyphs = VtGlyphs()

    /** The lock drawn for the cursor while a password is read ([drawLock]). */
    private val lockPaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val shackle = Path()

    var cellWidth = 1
        private set
    var cellHeight = 1
        private set
    var baseline = 0
        private set
    private var light = 1
    private var underlineY = 0
    private var strikeY = 0
    private var dotted = DashPathEffect(floatArrayOf(1f, 1f), 0f)
    private var dashed = DashPathEffect(floatArrayOf(3f, 2f), 0f)

    /**
     * How much glyphs in wide cells are enlarged: so that an ideograph's advance fills two cells
     * when the setting asks for it ([setFont]), else 1. The system's CJK fallback draws ideographs
     * one em wide, short of two cells of JetBrains Mono (1.2 em), which leaves wide gaps between
     * them; measured, so a font with its own ideographs two cells wide gets 1.
     */
    private var wideScale = 1f

    /** Advances of single codepoints, per paint (index << 32 | codepoint). */
    private val widths = HashMap<Long, Float>()

    /** The rows painted so far, one more than the grid (the overscan row). */
    private var bitmap: Bitmap? = null
    private var canvas: Canvas? = null
    private val runChars = CharArray(4096)

    private fun font(path: String, weight: Int): Typeface =
        Typeface.Builder(context.assets, path).setFontVariationSettings("'wght' $weight").build()

    private fun textPaint(typeface: Typeface) =
        Paint(Paint.ANTI_ALIAS_FLAG or Paint.SUBPIXEL_TEXT_FLAG).apply {
            this.typeface = typeface
            // Ligatures would join cells
            fontFeatureSettings = "'calt' 0, 'liga' 0"
        }

    /**
     * Cell size for a font of [px] pixels: the width is the font's advance rounded to whole pixels,
     * and the text size is then adjusted so the advance is exactly that width (ASCII runs stay on
     * the grid). With [fillWide], glyphs in wide cells are enlarged to fill them.
     */
    fun setFont(px: Float, fillWide: Boolean) {
        for (p in paints) p.textSize = px
        val advance = paints[0].measureText("M")
        cellWidth = max(1, advance.roundToInt())
        val size = px * cellWidth / advance
        for (p in paints) p.textSize = size
        val fm = paints[0].fontMetrics
        val height = fm.descent - fm.ascent
        cellHeight = max(1, ceil(height).toInt())
        baseline = (-fm.ascent + (cellHeight - height) / 2).roundToInt()
        light = max(1, paints[0].underlineThickness.roundToInt())
        underlineY = baseline + max(1, paints[0].underlinePosition.roundToInt())
        strikeY = baseline + paints[0].strikeThruPosition.roundToInt()
        dotted = DashPathEffect(floatArrayOf(light.toFloat(), light.toFloat()), 0f)
        dashed = DashPathEffect(floatArrayOf(light * 3f, light * 2f), 0f)
        stroke.strokeWidth = light.toFloat()
        val ideograph = paints[0].measureText("水")
        wideScale = if (fillWide && ideograph > 0f) max(1f, 2 * cellWidth / ideograph) else 1f
        widths.clear()
    }

    /**
     * Fits the screen and the bitmap to a grid of [cols] × [rows] cells, and the overscan row;
     * false when they fit it already.
     */
    fun resize(cols: Int, rows: Int): Boolean {
        val bw = cols * cellWidth
        val bh = (rows + 1) * cellHeight
        if (cols == screen.cols && rows == screen.rows && bitmap?.width == bw && bitmap?.height == bh) return false
        screen.resize(cols, rows)
        bitmap?.recycle()
        bitmap = createBitmap(bw, bh).also { canvas = Canvas(it) }
        return true
    }

    /** Whether there is a grid to draw ([resize]). */
    val hasGrid: Boolean get() = bitmap != null

    /** Takes [frame]'s rows into the screen and paints them. */
    fun takeRows(frame: VtFrame) {
        val target = canvas ?: return
        frame.takeRows(screen) { y -> drawRow(target, y) }
    }

    private fun drawRow(c: Canvas, y: Int) {
        val cols = screen.cols
        val cp = screen.cp
        val attrs = screen.attrs
        val clusters = screen.clusters
        val top = y * cellHeight
        val base = y * cols
        c.withClip(0, top, cols * cellWidth, top + cellHeight) {
            fill.color = screen.defaultBg
            c.drawRect(0f, top.toFloat(), (cols * cellWidth).toFloat(), (top + cellHeight).toFloat(), fill)
            // Images below the backgrounds show through cells of the default one
            images.draw(c, VtImages.LAYER_BELOW_BACKGROUND, top, cellHeight)
            var x = 0
            while (x < cols) {
                val color = bgOf(base + x)
                var end = x + 1
                while (end < cols && bgOf(base + end) == color) end++
                if (color != screen.defaultBg) {
                    fill.color = color
                    c.drawRect((x * cellWidth).toFloat(), top.toFloat(), (end * cellWidth).toFloat(), (top + cellHeight).toFloat(), fill)
                }
                x = end
            }
            images.draw(c, VtImages.LAYER_BELOW_TEXT, top, cellHeight)
            val from = screen.selFrom[y]
            if (from >= 0) {
                // The highlight covers a wide character whose first cell ends the selection
                val to = screen.selTo[y] + screen.span(base + screen.selTo[y])
                fill.color = SELECTION
                c.drawRect((from * cellWidth).toFloat(), top.toFloat(), (min(to, cols) * cellWidth).toFloat(), (top + cellHeight).toFloat(), fill)
            }

            // Runs of plain ASCII in one style go out in one call; other cells one at a time
            var runStart = -1
            var runLength = 0
            var runPaint: Paint? = null
            var runColor = 0
            var runDecor = 0
            fun flush() {
                if (runLength > 0) {
                    runPaint!!.color = runColor
                    c.drawText(runChars, 0, runLength, (runStart * cellWidth).toFloat(), (top + baseline).toFloat(), runPaint!!)
                    decorate(c, runDecor, runStart, runStart + runLength, top, runColor)
                }
                runStart = -1
                runLength = 0
            }
            x = 0
            while (x < cols) {
                val i = base + x
                val a = attrs[i]
                val wide = (a shr 12) and 3
                if (wide == SPACER_TAIL || wide == SPACER_HEAD) {
                    x++
                    continue
                }
                val code = cp[i]
                val color = fgOf(i)
                val paint = paintOf(i)
                val decor = a and DECORATIONS
                val plain = clusters[i] == null && wide == NARROW && a and INVISIBLE == 0 &&
                    (code == 0 || code in 0x20..0x7e)
                if (plain) {
                    if (runLength > 0 && (paint !== runPaint || color != runColor || decor != runDecor)) flush()
                    if (runLength == 0) {
                        runStart = x
                        runPaint = paint
                        runColor = color
                        runDecor = decor
                    }
                    runChars[runLength++] = if (code == 0) ' ' else code.toChar()
                    x++
                    continue
                }
                flush()
                val span = if (wide == WIDE) 2 else 1
                if (a and INVISIBLE == 0) drawGlyph(c, i, x, span, top, color, paint)
                decorate(c, decor, x, x + span, top, color)
                x += span
            }
            flush()
            images.draw(c, VtImages.LAYER_ABOVE_TEXT, top, cellHeight)
        }
    }

    /** A cell's background: a search match's colour, else its own (inverse swaps it with the text's). */
    private fun bgOf(i: Int): Int = when (markOf(i)) {
        MARK_MATCH -> SEARCH_MATCH
        MARK_SELECTED -> SEARCH_SELECTED
        else -> {
            val s = screen
            if (s.attrs[i] and INVERSE != 0) s.fg[i].takeIf { it != 0 } ?: s.defaultFg else s.bg[i].takeIf { it != 0 } ?: s.defaultBg
        }
    }

    /** A cell's text colour: black on a search match, as in Ghostty, else its own. */
    private fun fgOf(i: Int): Int {
        if (markOf(i) != MARK_NONE) return SEARCH_TEXT
        val s = screen
        val a = s.attrs[i]
        val color = if (a and INVERSE != 0) s.bg[i].takeIf { it != 0 } ?: s.defaultBg else s.fg[i].takeIf { it != 0 } ?: s.defaultFg
        return if (a and FAINT != 0) (color and 0x00ffffff) or 0x80000000.toInt() else color
    }

    /**
     * A cell's search mark (vt.c's attrs bits 14 and 15): [MARK_MATCH], [MARK_SELECTED] or
     * [MARK_NONE]. The second cell of a wide character takes the first's, so a match ending on one
     * covers the whole character.
     */
    private fun markOf(i: Int): Int {
        val attrs = screen.attrs
        val a = attrs[i]
        val own = (a shr 14) and 3
        if (own != MARK_NONE || (a shr 12) and 3 != SPACER_TAIL || i % screen.cols == 0) return own
        return (attrs[i - 1] shr 14) and 3
    }

    private fun paintOf(i: Int): Paint = paints[screen.attrs[i] and (BOLD or ITALIC)]

    /**
     * One cell's glyph, centred in its [span] cells: a wide one enlarged by [wideScale], and any
     * shrunk evenly to fit them.
     */
    private fun drawGlyph(c: Canvas, i: Int, x: Int, span: Int, top: Int, color: Int, paint: Paint) {
        val code = screen.cp[i]
        // With or without the diacritics that say which part of the image the cell shows
        if (code == KITTY_PLACEHOLDER) return
        val cluster = screen.clusters[i]
        val left = x * cellWidth
        val w = span * cellWidth
        if (cluster == null && glyphs.covers(code)) {
            glyphs.draw(c, code, left, top, w, cellHeight, light, color)
            return
        }
        if (cluster == null && (code == 0 || code == ' '.code)) return
        val text = cluster ?: String(Character.toChars(code))
        val advance = if (cluster != null) {
            paint.measureText(cluster)
        } else {
            val key = (paints.indexOf(paint).toLong() shl 32) or code.toLong()
            widths.getOrPut(key) { paint.measureText(text) }
        }
        paint.color = color
        var s = if (span == 2) wideScale else 1f
        if (advance * s > w + 0.5f) s = w / advance
        val l = left + (w - advance * s) / 2
        if (s == 1f) {
            c.drawText(text, l, (top + baseline).toFloat(), paint)
        } else {
            c.withScale(s, s, l, top + cellHeight / 2f) { c.drawText(text, l, (top + baseline).toFloat(), paint) }
        }
    }

    private fun decorate(c: Canvas, decor: Int, from: Int, to: Int, top: Int, color: Int) {
        if (decor == 0) return
        val l = (from * cellWidth).toFloat()
        val r = (to * cellWidth).toFloat()
        fill.color = color
        stroke.color = color
        val underline = (decor shr 8) and 7
        val uy = (top + underlineY).toFloat()
        when (underline) {
            0 -> {}
            UNDERLINE_DOUBLE -> {
                c.drawRect(l, uy, r, uy + light, fill)
                c.drawRect(l, uy + 2 * light, r, uy + 3 * light, fill)
            }
            UNDERLINE_CURLY -> {
                curl.rewind()
                val amp = light * 1.5f
                val wave = max(4f, cellWidth / 2f)
                val mid = uy + amp
                curl.moveTo(l, mid)
                var px = l
                var up = true
                while (px < r) {
                    val nx = minOf(r, px + wave)
                    curl.quadTo((px + nx) / 2, if (up) mid - 2 * amp else mid + 2 * amp, nx, mid)
                    px = nx
                    up = !up
                }
                c.drawPath(curl, stroke)
            }
            UNDERLINE_DOTTED, UNDERLINE_DASHED -> {
                stroke.pathEffect = if (underline == UNDERLINE_DOTTED) dotted else dashed
                c.drawLine(l, uy + light / 2f, r, uy + light / 2f, stroke)
                stroke.pathEffect = null
            }
            else -> c.drawRect(l, uy, r, uy + light, fill)
        }
        if (decor and STRIKE != 0) {
            val sy = (top + strikeY).toFloat()
            c.drawRect(l, sy, r, sy + light, fill)
        }
        if (decor and OVERLINE != 0) c.drawRect(l, top.toFloat(), r, (top + light).toFloat(), fill)
    }

    /** The rows as last painted, [scrollPx] above their place, clipped to the grid's whole cells. */
    fun drawGrid(c: Canvas, scrollPx: Int) {
        val bmp = bitmap ?: return
        c.withClip(0, 0, screen.cols * cellWidth, screen.rows * cellHeight) {
            c.drawBitmap(bmp, 0f, -scrollPx.toFloat(), null)
        }
    }

    /**
     * The cursor in its shape (a block hollow without the view's [focused] focus), or the lock
     * while a password is read, unless the IME is [composing]; nothing when it is hidden or outside
     * the grid.
     */
    fun drawCursor(c: Canvas, focused: Boolean, composing: Boolean) {
        val s = screen
        if (!s.cursorShown || s.cursorX >= s.cols || s.cursorY >= s.rows) return
        val i = s.cursorY * s.cols + s.cursorX
        val span = s.span(i)
        val l = s.cursorX * cellWidth
        val t = s.cursorY * cellHeight
        val r = l + span * cellWidth
        val b = t + cellHeight
        val color = if (s.cursorColor != 0) s.cursorColor else s.defaultFg
        fill.color = color
        stroke.color = color
        when {
            // Composing text shows a block, as in Ghostty
            s.cursorStyle == VtNative.CURSOR_LOCK && !composing -> drawLock(c, l, t, r, b, color)
            s.cursorStyle == VtNative.CURSOR_BAR -> c.drawRect(l.toFloat(), t.toFloat(), (l + 2 * light).toFloat(), b.toFloat(), fill)
            s.cursorStyle == VtNative.CURSOR_UNDERLINE -> c.drawRect(l.toFloat(), (b - 2 * light).toFloat(), r.toFloat(), b.toFloat(), fill)
            s.cursorStyle == VtNative.CURSOR_HOLLOW || !focused ->
                c.drawRect(l + light / 2f, t + light / 2f, r - light / 2f, b - light / 2f, stroke)
            else -> {
                // A block: the cell's glyph in the background colour on top
                c.withClip(l, t, r, b) {
                    c.drawRect(l.toFloat(), t.toFloat(), r.toFloat(), b.toFloat(), fill)
                    if (s.attrs[i] and INVISIBLE == 0) drawGlyph(c, i, s.cursorX, span, t, bgOf(i), paintOf(i))
                }
            }
        }
    }

    /**
     * The cursor while the console reads a password: a padlock in the cursor's colour, about as
     * tall as a capital letter, standing on the baseline in the middle of its cell (or two), where
     * Ghostty draws a lock glyph (U+F023 from its Nerd Font, which the bundled font lacks); not
     * blinking, and shown without the focus too.
     */
    private fun drawLock(c: Canvas, l: Int, t: Int, r: Int, b: Int, color: Int) {
        val w = min(cellWidth, b - t) * 0.85f
        val h = w * 1.15f
        val cx = (l + r) / 2f
        val top = t + baseline - h
        val bodyTop = top + h * 0.42f
        lockPaint.color = color
        lockPaint.style = Paint.Style.FILL
        c.drawRoundRect(cx - w / 2f, bodyTop, cx + w / 2f, top + h, w * 0.12f, w * 0.12f, lockPaint)
        // The shackle: an arch from the body's top, inset from its sides
        val sw = max(light.toFloat(), w * 0.16f)
        val radius = (w * 0.68f - sw) / 2f
        shackle.rewind()
        shackle.moveTo(cx - radius, bodyTop)
        shackle.lineTo(cx - radius, top + sw / 2f + radius)
        shackle.arcTo(cx - radius, top + sw / 2f, cx + radius, top + sw / 2f + 2 * radius, 180f, 180f, false)
        shackle.lineTo(cx + radius, bodyTop)
        lockPaint.style = Paint.Style.STROKE
        lockPaint.strokeWidth = sw
        c.drawPath(shackle, lockPaint)
    }

    /** How wide [text] is in the regular paint (the composing text). */
    fun advance(text: String): Float = paints[0].measureText(text)

    /** The input method's composing [text] at the cursor, underlined. */
    fun drawPreedit(c: Canvas, text: String) {
        val s = screen
        if (text.isEmpty() || s.cursorY >= s.rows) return
        val p = paints[0]
        val w = p.measureText(text)
        // Shifted left when it would run past the right edge
        val l = min((s.cursorX * cellWidth).toFloat(), s.cols * cellWidth - w).coerceAtLeast(0f)
        val t = (s.cursorY * cellHeight).toFloat()
        fill.color = s.defaultBg
        c.drawRect(l, t, l + w, t + cellHeight, fill)
        p.color = s.defaultFg
        c.drawText(text, l, t + baseline, p)
        fill.color = s.defaultFg
        c.drawRect(l, t + underlineY, l + w, t + underlineY + light, fill)
    }

    companion object {
        // Cell attributes (vt.c)
        private const val BOLD = 1
        private const val ITALIC = 2
        private const val FAINT = 1 shl 2
        private const val INVERSE = 1 shl 4
        private const val INVISIBLE = 1 shl 5
        private const val STRIKE = 1 shl 6
        private const val OVERLINE = 1 shl 7
        private const val DECORATIONS = STRIKE or OVERLINE or (7 shl 8)
        private const val NARROW = 0
        const val WIDE = 1
        private const val SPACER_TAIL = 2
        private const val SPACER_HEAD = 3

        // GhosttySgrUnderline
        private const val UNDERLINE_DOUBLE = 2
        private const val UNDERLINE_CURLY = 3
        private const val UNDERLINE_DOTTED = 4
        private const val UNDERLINE_DASHED = 5

        /**
         * The Kitty graphics placeholder (U+10EEEE): its cells show part of an image, which the
         * frame places, and are drawn blank, as Ghostty draws them.
         */
        private const val KITTY_PLACEHOLDER = 0x10EEEE

        // A cell's search mark (attrs bits 14 and 15)
        private const val MARK_NONE = 0
        private const val MARK_MATCH = 1
        private const val MARK_SELECTED = 2

        /** The selection's highlight, over cell backgrounds and under text. */
        private const val SELECTION = 0x6000E5FF

        /**
         * Search matches in Tuxport's Neon Terminal palette: black text on cyan, the
         * selected match on magenta.
         */
        const val SEARCH_MATCH = 0xFF00E5FF.toInt()
        const val SEARCH_SELECTED = 0xFFFF4081.toInt()
        private const val SEARCH_TEXT = 0xFF000000.toInt()
    }
}
