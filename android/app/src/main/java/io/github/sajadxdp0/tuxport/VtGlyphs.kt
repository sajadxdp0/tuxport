package io.github.sajadxdp0.tuxport

import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Path
import android.graphics.RectF

/**
 * Characters drawn as shapes instead of from a font (the terminal view, the android/vtrender
 * map): box drawing (U+2500–U+257F), block elements (U+2580–U+259F) and the Powerline triangles
 * (U+E0B0–U+E0B3). Fonts draw these with their own heights and stroke widths, which leave gaps
 * between rows and misalign with the cell grid; drawn here, every line is pixel-aligned and runs to
 * the cell edge, so lines join across cells and rows.
 */
internal class VtGlyphs {
    private val fill = Paint().apply { style = Paint.Style.FILL }
    private val smooth = Paint(Paint.ANTI_ALIAS_FLAG)
    private val path = Path()
    private val oval = RectF()

    fun covers(cp: Int): Boolean = cp in 0x2500..0x259F || cp in 0xE0B0..0xE0B3

    /**
     * Draws [cp] (one of [covers]) in the cell at ([x], [y]) of [w] × [h] pixels; [light] is the
     * width of a light line in pixels.
     */
    fun draw(c: Canvas, cp: Int, x: Int, y: Int, w: Int, h: Int, light: Int, color: Int) {
        fill.color = color
        smooth.color = color
        when (cp) {
            in 0x2504..0x250B -> dashes(c, cp - 0x2504, x, y, w, h, light)
            in 0x254C..0x254F -> dashes(c, cp - 0x254C + 8, x, y, w, h, light)
            in 0x256D..0x2570 -> arc(c, cp, x, y, w, h, light)
            in 0x2571..0x2573 -> diagonal(c, cp, x, y, w, h, light)
            in 0x2500..0x257F -> lines(c, ARMS[cp - 0x2500], x, y, w, h, light)
            in 0x2580..0x259F -> block(c, cp, x, y, w, h)
            else -> powerline(c, cp, x, y, w, h, light)
        }
    }

    /**
     * Straight lines from the cell edges to the centre: [arms] holds the weight of the up, right,
     * down and left arms (0 none, 1 light, 2 heavy, 3 double) as four digits.
     */
    private fun lines(c: Canvas, arms: Int, x: Int, y: Int, w: Int, h: Int, light: Int) {
        val up = arms / 1000
        val right = arms / 100 % 10
        val down = arms / 10 % 10
        val left = arms % 10
        fun width(weight: Int) = if (weight == 2) light * 2 else light
        // Single (light or heavy) lines are centred; T is the widest one, and single arms reach
        // across the centre square of T × T, so joins are filled
        val t = maxOf(
            if (up in 1..2) width(up) else 0, if (down in 1..2) width(down) else 0,
            if (left in 1..2) width(left) else 0, if (right in 1..2) width(right) else 0,
        )
        val cx = x + (w - t) / 2
        val cy = y + (h - t) / 2
        // A double line is two light lines a light width apart: x1, x2 (vertical) and y1, y2
        // (horizontal) are their left and top edges
        val x1 = x + (w - 3 * light) / 2
        val x2 = x1 + 2 * light
        val y1 = y + (h - 3 * light) / 2
        val y2 = y1 + 2 * light
        val midX = x + w / 2
        val midY = y + h / 2

        // Single arms; where doubles cross them, they stop at the double line they meet
        if (up in 1..2) {
            val lx = x + (w - width(up)) / 2
            val end = when {
                left == 3 && right == 3 -> if (down != 0) cy + t else y1 + light
                left == 3 || right == 3 -> y2 + light
                else -> cy + t
            }
            rect(c, lx, y, lx + width(up), end)
        }
        if (down in 1..2) {
            val lx = x + (w - width(down)) / 2
            val start = when {
                left == 3 && right == 3 -> if (up != 0) cy else y2
                left == 3 || right == 3 -> y1
                else -> cy
            }
            rect(c, lx, start, lx + width(down), y + h)
        }
        if (left in 1..2) {
            val ty = y + (h - width(left)) / 2
            val end = when {
                up == 3 && down == 3 -> if (right != 0) cx + t else x1 + light
                up == 3 || down == 3 -> x2 + light
                else -> cx + t
            }
            rect(c, x, ty, end, ty + width(left))
        }
        if (right in 1..2) {
            val ty = y + (h - width(right)) / 2
            val start = when {
                up == 3 && down == 3 -> if (left != 0) cx else x2
                up == 3 || down == 3 -> x1
                else -> cx
            }
            rect(c, start, ty, x + w, ty + width(right))
        }

        // Double arms: each of the two lines stops where it meets the line on its own side (the
        // inner corner), or else the single line, or else the far double line (the outer corner)
        fun single(weight: Int) = weight == 1 || weight == 2
        if (right == 3) {
            fun start(side: Int, other: Int, inner: Int, outer: Int) = when {
                side == 3 -> inner
                single(side) -> x + (w - width(side)) / 2
                other == 3 -> outer
                single(other) -> x + (w - width(other)) / 2
                else -> midX
            }
            rect(c, start(up, down, x2, x1), y1, x + w, y1 + light)
            rect(c, start(down, up, x2, x1), y2, x + w, y2 + light)
        }
        if (left == 3) {
            fun end(side: Int, other: Int, inner: Int, outer: Int) = when {
                side == 3 -> inner
                single(side) -> x + (w - width(side)) / 2 + width(side)
                other == 3 -> outer
                single(other) -> x + (w - width(other)) / 2 + width(other)
                else -> midX
            }
            rect(c, x, y1, end(up, down, x1 + light, x2 + light), y1 + light)
            rect(c, x, y2, end(down, up, x1 + light, x2 + light), y2 + light)
        }
        if (down == 3) {
            fun start(side: Int, other: Int, inner: Int, outer: Int) = when {
                side == 3 -> inner
                single(side) -> y + (h - width(side)) / 2
                other == 3 -> outer
                single(other) -> y + (h - width(other)) / 2
                else -> midY
            }
            rect(c, x1, start(left, right, y2, y1), x1 + light, y + h)
            rect(c, x2, start(right, left, y2, y1), x2 + light, y + h)
        }
        if (up == 3) {
            fun end(side: Int, other: Int, inner: Int, outer: Int) = when {
                side == 3 -> inner
                single(side) -> y + (h - width(side)) / 2 + width(side)
                other == 3 -> outer
                single(other) -> y + (h - width(other)) / 2 + width(other)
                else -> midY
            }
            rect(c, x1, y, x1 + light, end(left, right, y1 + light, y2 + light))
            rect(c, x2, y, x2 + light, end(right, left, y1 + light, y2 + light))
        }
    }

    private fun rect(c: Canvas, l: Int, t: Int, r: Int, b: Int) {
        if (r > l && b > t) c.drawRect(l.toFloat(), t.toFloat(), r.toFloat(), b.toFloat(), fill)
    }

    /**
     * Dashed lines: [kind] 0–7 are U+2504–U+250B (triple and quadruple dashes: horizontal light,
     * horizontal heavy, vertical light, vertical heavy), 8–11 U+254C–U+254F (double dashes).
     */
    private fun dashes(c: Canvas, kind: Int, x: Int, y: Int, w: Int, h: Int, light: Int) {
        val n = when (kind) {
            in 0..3 -> 3
            in 4..7 -> 4
            else -> 2
        }
        val heavy = kind % 2 == 1
        val vertical = kind % 4 >= 2
        val t = if (heavy) light * 2 else light
        val length = if (vertical) h else w
        // Gaps are split at both ends, so dashes keep their rhythm across neighbouring cells
        val gap = maxOf(1, length / (n * 3))
        for (i in 0 until n) {
            val a = i * length / n + gap / 2
            val b = (i + 1) * length / n - (gap - gap / 2)
            if (vertical) {
                val lx = x + (w - t) / 2
                rect(c, lx, y + a, lx + t, y + b)
            } else {
                val ty = y + (h - t) / 2
                rect(c, x + a, ty, x + b, ty + t)
            }
        }
    }

    /** U+256D–U+2570: rounded corners joining the light lines of the two edges they touch. */
    private fun arc(c: Canvas, cp: Int, x: Int, y: Int, w: Int, h: Int, light: Int) {
        smooth.style = Paint.Style.STROKE
        smooth.strokeWidth = light.toFloat()
        val vx = x + (w - light) / 2 + light / 2f
        val hy = y + (h - light) / 2 + light / 2f
        val r = minOf(w, h) / 2f
        path.rewind()
        when (cp) {
            0x256D -> { // ╭ down and right
                path.moveTo(vx, (y + h).toFloat())
                path.lineTo(vx, hy + r)
                oval.set(vx, hy, vx + 2 * r, hy + 2 * r)
                path.arcTo(oval, 180f, 90f)
                path.lineTo((x + w).toFloat(), hy)
            }
            0x256E -> { // ╮ down and left
                path.moveTo(vx, (y + h).toFloat())
                path.lineTo(vx, hy + r)
                oval.set(vx - 2 * r, hy, vx, hy + 2 * r)
                path.arcTo(oval, 0f, -90f)
                path.lineTo(x.toFloat(), hy)
            }
            0x256F -> { // ╯ up and left
                path.moveTo(vx, y.toFloat())
                path.lineTo(vx, hy - r)
                oval.set(vx - 2 * r, hy - 2 * r, vx, hy)
                path.arcTo(oval, 0f, 90f)
                path.lineTo(x.toFloat(), hy)
            }
            else -> { // ╰ up and right
                path.moveTo(vx, y.toFloat())
                path.lineTo(vx, hy - r)
                oval.set(vx, hy - 2 * r, vx + 2 * r, hy)
                path.arcTo(oval, 180f, -90f)
                path.lineTo((x + w).toFloat(), hy)
            }
        }
        c.drawPath(path, smooth)
    }

    /** U+2571–U+2573: diagonals from corner to corner. */
    private fun diagonal(c: Canvas, cp: Int, x: Int, y: Int, w: Int, h: Int, light: Int) {
        smooth.style = Paint.Style.STROKE
        smooth.strokeWidth = light.toFloat()
        val l = x.toFloat()
        val t = y.toFloat()
        val r = (x + w).toFloat()
        val b = (y + h).toFloat()
        if (cp != 0x2572) c.drawLine(r, t, l, b, smooth) // ╱
        if (cp != 0x2571) c.drawLine(l, t, r, b, smooth) // ╲
    }

    /** U+2580–U+259F: halves, eighths, quadrants and shades. */
    private fun block(c: Canvas, cp: Int, x: Int, y: Int, w: Int, h: Int) {
        val r = x + w
        val b = y + h
        val mx = x + w / 2
        val my = y + h / 2
        fun lower(eighths: Int) = rect(c, x, b - h * eighths / 8, r, b)
        fun leftPart(eighths: Int) = rect(c, x, y, x + w * eighths / 8, b)
        when (cp) {
            0x2580 -> rect(c, x, y, r, my)
            in 0x2581..0x2588 -> lower(cp - 0x2580)
            in 0x2589..0x258F -> leftPart(0x2590 - cp)
            0x2590 -> rect(c, mx, y, r, b)
            in 0x2591..0x2593 -> {
                val alpha = (cp - 0x2590) * 64
                fill.alpha = alpha
                rect(c, x, y, r, b)
                fill.alpha = 255
            }
            0x2594 -> rect(c, x, y, r, y + h / 8)
            0x2595 -> rect(c, r - w / 8, y, r, b)
            else -> {
                // Quadrants, as bits: 1 upper left, 2 upper right, 4 lower left, 8 lower right
                val q = QUADRANTS[cp - 0x2596]
                if (q and 1 != 0) rect(c, x, y, mx, my)
                if (q and 2 != 0) rect(c, mx, y, r, my)
                if (q and 4 != 0) rect(c, x, my, mx, b)
                if (q and 8 != 0) rect(c, mx, my, r, b)
            }
        }
    }

    /** U+E0B0–U+E0B3: solid and outlined triangles pointing right and left. */
    private fun powerline(c: Canvas, cp: Int, x: Int, y: Int, w: Int, h: Int, light: Int) {
        val l = x.toFloat()
        val t = y.toFloat()
        val r = (x + w).toFloat()
        val b = (y + h).toFloat()
        val m = y + h / 2f
        path.rewind()
        when (cp) {
            0xE0B0, 0xE0B1 -> {
                path.moveTo(l, t)
                path.lineTo(r, m)
                path.lineTo(l, b)
            }
            else -> {
                path.moveTo(r, t)
                path.lineTo(l, m)
                path.lineTo(r, b)
            }
        }
        if (cp == 0xE0B0 || cp == 0xE0B2) {
            path.close()
            smooth.style = Paint.Style.FILL
        } else {
            smooth.style = Paint.Style.STROKE
            smooth.strokeWidth = light.toFloat()
        }
        c.drawPath(path, smooth)
    }

    private companion object {
        /**
         * U+2500–U+257F: up, right, down and left weights as four digits (0 none, 1 light,
         * 2 heavy, 3 double); dashes, arcs and diagonals are drawn by their own functions and
         * have 0 here.
         */
        val ARMS = intArrayOf(
            // 2500
            101, 202, 1010, 2020, 0, 0, 0, 0, 0, 0, 0, 0, 110, 210, 120, 220,
            // 2510
            11, 12, 21, 22, 1100, 1200, 2100, 2200, 1001, 1002, 2001, 2002, 1110, 1210, 2110, 1120,
            // 2520
            2120, 2210, 1220, 2220, 1011, 1012, 2011, 1021, 2021, 2012, 1022, 2022, 111, 112, 211, 212,
            // 2530
            121, 122, 221, 222, 1101, 1102, 1201, 1202, 2101, 2102, 2201, 2202, 1111, 1112, 1211, 1212,
            // 2540
            2111, 1121, 2121, 2112, 2211, 1122, 1221, 2212, 1222, 2122, 2221, 2222, 0, 0, 0, 0,
            // 2550
            303, 3030, 310, 130, 330, 13, 31, 33, 1300, 3100, 3300, 1003, 3001, 3003, 1310, 3130,
            // 2560
            3330, 1013, 3031, 3033, 313, 131, 333, 1303, 3101, 3303, 1313, 3131, 3333, 0, 0, 0,
            // 2570
            0, 0, 0, 0, 1, 1000, 100, 10, 2, 2000, 200, 20, 201, 1020, 102, 2010,
        )

        /** U+2596–U+259F as quadrant bits (1 upper left, 2 upper right, 4 lower left, 8 lower right). */
        val QUADRANTS = intArrayOf(4, 8, 1, 1 or 4 or 8, 1 or 8, 1 or 2 or 4, 1 or 2 or 8, 2, 2 or 4, 2 or 4 or 8)
    }
}
