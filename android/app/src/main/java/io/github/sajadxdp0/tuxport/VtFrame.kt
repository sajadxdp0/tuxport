package io.github.sajadxdp0.tuxport

import android.graphics.Rect
import java.nio.ByteBuffer
import java.nio.ByteOrder
import kotlin.math.min

/**
 * A frame from [VtNative.frame] (the android/vtrender map), read in place: its header, its image
 * placements and its rows, in the format `hosts/android/src/vt.c` documents and writes (the two
 * change together). Valid until the next frame is taken.
 */
@JvmInline
internal value class VtFrame(private val b: ByteBuffer) {
    init {
        b.order(ByteOrder.LITTLE_ENDIAN)
    }

    val cols: Int get() = u16(0)
    val rows: Int get() = u16(2)

    /** Whether rows changed: [DIRTY_NONE], 1 for some, [DIRTY_FULL] for every one. */
    val dirty: Int get() = b.get(4).toInt()
    val cursorShown: Boolean get() = b.get(5).toInt() != 0

    /** `GhosttyCursorStyle`, or vt.c's lock ([VtNative.CURSOR_LOCK]). */
    val cursorStyle: Int get() = b.get(6).toInt()
    val cursorBlinking: Boolean get() = b.get(7).toInt() != 0
    val cursorX: Int get() = u16(8)
    val cursorY: Int get() = u16(10)
    val defaultFg: Int get() = b.getInt(12)
    val defaultBg: Int get() = b.getInt(16)

    /** 0 when the program did not set one (OSC 12). */
    val cursorColor: Int get() = b.getInt(20)

    /** The rows that follow ([takeRows]). */
    val rowCount: Int get() = u16(24)

    /** The viewport shows the active area (it is not scrolled back). */
    val atBottom: Boolean get() = b.get(26).toInt() != 0
    private val flags: Int get() = b.get(27).toInt()
    val mouseTracking: Boolean get() = flags and FLAG_MOUSE != 0
    val altScreen: Boolean get() = flags and FLAG_ALT_SCREEN != 0
    val altScroll: Boolean get() = flags and FLAG_ALT_SCROLL != 0
    val selection: Boolean get() = flags and FLAG_SELECTION != 0
    val rectangle: Boolean get() = flags and FLAG_RECTANGLE != 0

    /** The search has not caught up with the terminal yet: draw again to go on. */
    val searchPending: Boolean get() = flags and FLAG_SEARCH_PENDING != 0

    /** Output after resize has not settled at parser ground, or synchronized output is held. */
    val outputPending: Boolean get() = flags and FLAG_OUTPUT_PENDING != 0

    // The selection's first and last cell in the viewport, -1 when outside it
    val selStartX: Int get() = coordinate(28)
    val selStartY: Int get() = coordinate(30)
    val selEndX: Int get() = coordinate(32)
    val selEndY: Int get() = coordinate(34)

    // The scrollable area in rows, the viewport's first row in it and the rows it shows
    val scrollTotal: Int get() = b.getInt(36)
    val scrollOffset: Int get() = b.getInt(40)
    val scrollLen: Int get() = b.getInt(44)

    /** The search's matches so far, and the selected one's place from the newest (0 for none). */
    val searchTotal: Int get() = b.getInt(48)
    val searchSelected: Int get() = b.getInt(52)
    val imageCount: Int get() = u16(56)

    /** The row after the viewport comes too (it is scrolled back). */
    val overscan: Boolean get() = b.get(58).toInt() != 0

    /** The mouse pointer's shape the program asked for (OSC 22, a `GhosttyMouseShape`). */
    val pointerShape: Int get() = b.get(59).toInt()

    /** When to draw again for an animated image's next frame, 0 when none plays. */
    val animationMs: Int get() = b.getInt(60)

    /** Reads image placement [k] into [p]. */
    fun placement(k: Int, p: VtImages.Placement) {
        val o = HEADER_BYTES + k * IMAGE_BYTES
        p.id = b.getInt(o)
        p.layer = b.get(o + 4).toInt()
        p.generation = b.getLong(o + 8)
        rect(p.dst, b.getInt(o + 16), b.getInt(o + 20), b.getInt(o + 24), b.getInt(o + 28))
        rect(p.src, b.getInt(o + 32), b.getInt(o + 36), b.getInt(o + 40), b.getInt(o + 44))
        p.imageWidth = b.getInt(o + 48)
        p.imageHeight = b.getInt(o + 52)
    }

    /**
     * Sets [r] to [w] × [h] pixels at ([x], [y]), the sizes read as unsigned (vt.c's u32) and the
     * far edges kept within an Int.
     */
    private fun rect(r: Rect, x: Int, y: Int, w: Int, h: Int) {
        val right = min(x + (w.toLong() and 0xffffffffL), Int.MAX_VALUE.toLong())
        val bottom = min(y + (h.toLong() and 0xffffffffL), Int.MAX_VALUE.toLong())
        r.set(x, y, right.toInt(), bottom.toInt())
    }

    /**
     * Copies the rows the frame brings into [screen]'s cells, up to the overscan row after the
     * viewport (y == rows), and calls [painted] with each one's y once copied.
     */
    fun takeRows(screen: VtScreen, painted: (Int) -> Unit) {
        val cols = screen.cols
        var pos = HEADER_BYTES + imageCount * IMAGE_BYTES
        repeat(rowCount) {
            val y = u16(pos)
            val n = u16(pos + 2)
            val extra = b.getInt(pos + 4)
            val from = coordinate(pos + 8)
            val to = coordinate(pos + 10)
            pos += ROW_HEADER_BYTES
            var e = pos + n * CELL_BYTES
            if (y <= screen.rows) {
                screen.selFrom[y] = if (to >= 0) from else -1
                screen.selTo[y] = to
                val base = y * cols
                for (x in 0 until cols) {
                    val i = base + x
                    if (x >= n) {
                        screen.cp[i] = 0
                        screen.fg[i] = 0
                        screen.bg[i] = 0
                        screen.attrs[i] = 0
                        screen.clusters[i] = null
                        continue
                    }
                    val o = pos + x * CELL_BYTES
                    screen.cp[i] = b.getInt(o)
                    screen.fg[i] = b.getInt(o + 4)
                    screen.bg[i] = b.getInt(o + 8)
                    screen.attrs[i] = u16(o + 12)
                    val more = u16(o + 14)
                    screen.clusters[i] = if (more == 0) {
                        null
                    } else {
                        StringBuilder().apply {
                            appendCodePoint(screen.cp[i])
                            repeat(more) {
                                appendCodePoint(b.getInt(e))
                                e += 4
                            }
                        }.toString()
                    }
                }
                painted(y)
            }
            pos += n * CELL_BYTES + extra * 4
        }
    }

    private fun u16(at: Int): Int = b.getShort(at).toInt() and 0xffff

    /** A u16 coordinate, -1 for 0xffff (none). */
    private fun coordinate(at: Int): Int = u16(at).let { if (it == 0xffff) -1 else it }

    companion object {
        const val DIRTY_NONE = 0
        const val DIRTY_FULL = 2

        private const val HEADER_BYTES = 64
        private const val IMAGE_BYTES = 56
        private const val ROW_HEADER_BYTES = 12
        private const val CELL_BYTES = 16
        private const val FLAG_MOUSE = 1
        private const val FLAG_ALT_SCREEN = 1 shl 1
        private const val FLAG_ALT_SCROLL = 1 shl 2
        private const val FLAG_SELECTION = 1 shl 4
        private const val FLAG_RECTANGLE = 1 shl 5
        private const val FLAG_SEARCH_PENDING = 1 shl 6
        private const val FLAG_OUTPUT_PENDING = 1 shl 7
    }
}
