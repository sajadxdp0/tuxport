package io.github.sajadxdp0.tuxport

import android.graphics.Color

/**
 * The screen as of the last frame (the android/vtrender map): a mirror of the grid's cells, one row
 * taller than the grid (the frame's overscan row after the viewport, which shows in part while the
 * grid is drawn shifted up by [VtView]'s pixel scrolling), each row's selected columns, the
 * default colours and the cursor. [VtFrame.takeRows] fills the cells, [VtView] the rest, and
 * [VtRenderer] draws them.
 */
internal class VtScreen {
    var cols = 0
        private set
    var rows = 0
        private set

    // Each cell's first codepoint, colours (0 for the default ones) and attributes (vt.c's)
    var cp = IntArray(0)
        private set
    var fg = IntArray(0)
        private set
    var bg = IntArray(0)
        private set
    var attrs = IntArray(0)
        private set

    /** Whole graphemes of cells with more than one codepoint, else null. */
    var clusters = arrayOfNulls<String>(0)
        private set

    /** Each row's selected columns (inclusive), -1 when none. */
    var selFrom = IntArray(0)
        private set
    var selTo = IntArray(0)
        private set

    var defaultFg = Color.WHITE
    // Opaque black, matching the palette's default so pre-frame and post-frame draws are
    // pixel-identical (TerminalResizeTest compares them); the chrome keeps the brand base.
    var defaultBg = Color.BLACK
    var cursorColor = 0
    var cursorShown = false
    var cursorStyle = VtNative.CURSOR_BLOCK
    var cursorBlinking = false
    var cursorX = 0
    var cursorY = 0

    /** Holds [cols] × [rows] cells, and the overscan row; the cells are blank until a frame fills them. */
    fun resize(cols: Int, rows: Int) {
        this.cols = cols
        this.rows = rows
        val n = cols * (rows + 1)
        cp = IntArray(n)
        fg = IntArray(n)
        bg = IntArray(n)
        attrs = IntArray(n)
        clusters = arrayOfNulls(n)
        selFrom = IntArray(rows + 1) { -1 }
        selTo = IntArray(rows + 1) { -1 }
    }

    /** How many cells the character in cell [i] takes: 2 when it is wide, else 1. */
    fun span(i: Int): Int = if ((attrs[i] shr 12) and 3 == VtRenderer.WIDE) 2 else 1
}
