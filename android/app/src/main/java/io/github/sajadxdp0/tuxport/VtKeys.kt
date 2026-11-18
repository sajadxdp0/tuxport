package io.github.sajadxdp0.tuxport

import android.view.KeyEvent

/**
 * Android key codes as libghostty-vt's physical keys (`GhosttyKey` in `ghostty/vt/key/event.h`,
 * whose numbers its stable ABI keeps since 1.0), and its modifier bits.
 */
internal object VtKeys {
    const val ACTION_RELEASE = 0
    const val ACTION_PRESS = 1
    const val ACTION_REPEAT = 2

    const val MOD_SHIFT = 1 shl 0
    const val MOD_CTRL = 1 shl 1
    const val MOD_ALT = 1 shl 2
    const val MOD_SUPER = 1 shl 3

    private val keys = HashMap<Int, Int>().apply {
        for (i in 0 until 26) put(KeyEvent.KEYCODE_A + i, 20 + i)
        for (i in 0 until 10) put(KeyEvent.KEYCODE_0 + i, 6 + i)
        for (i in 0 until 10) put(KeyEvent.KEYCODE_NUMPAD_0 + i, 80 + i)
        for (i in 0 until 12) put(KeyEvent.KEYCODE_F1 + i, 121 + i)
        put(KeyEvent.KEYCODE_GRAVE, 1)
        put(KeyEvent.KEYCODE_BACKSLASH, 2)
        put(KeyEvent.KEYCODE_LEFT_BRACKET, 3)
        put(KeyEvent.KEYCODE_RIGHT_BRACKET, 4)
        put(KeyEvent.KEYCODE_COMMA, 5)
        put(KeyEvent.KEYCODE_EQUALS, 16)
        put(KeyEvent.KEYCODE_MINUS, 46)
        put(KeyEvent.KEYCODE_PERIOD, 47)
        put(KeyEvent.KEYCODE_APOSTROPHE, 48)
        put(KeyEvent.KEYCODE_SEMICOLON, 49)
        put(KeyEvent.KEYCODE_SLASH, 50)
        put(KeyEvent.KEYCODE_DEL, 53) // Backspace
        put(KeyEvent.KEYCODE_ENTER, 58)
        put(KeyEvent.KEYCODE_SPACE, 63)
        put(KeyEvent.KEYCODE_TAB, 64)
        put(KeyEvent.KEYCODE_FORWARD_DEL, 68)
        put(KeyEvent.KEYCODE_MOVE_END, 69)
        put(KeyEvent.KEYCODE_MOVE_HOME, 71)
        put(KeyEvent.KEYCODE_INSERT, 72)
        put(KeyEvent.KEYCODE_PAGE_DOWN, 73)
        put(KeyEvent.KEYCODE_PAGE_UP, 74)
        put(KeyEvent.KEYCODE_DPAD_DOWN, 75)
        put(KeyEvent.KEYCODE_DPAD_LEFT, 76)
        put(KeyEvent.KEYCODE_DPAD_RIGHT, 77)
        put(KeyEvent.KEYCODE_DPAD_UP, 78)
        put(KeyEvent.KEYCODE_NUMPAD_ADD, 90)
        put(KeyEvent.KEYCODE_NUMPAD_COMMA, 94)
        put(KeyEvent.KEYCODE_NUMPAD_DOT, 95)
        put(KeyEvent.KEYCODE_NUMPAD_DIVIDE, 96)
        put(KeyEvent.KEYCODE_NUMPAD_ENTER, 97)
        put(KeyEvent.KEYCODE_NUMPAD_EQUALS, 98)
        put(KeyEvent.KEYCODE_NUMPAD_MULTIPLY, 104)
        put(KeyEvent.KEYCODE_NUMPAD_SUBTRACT, 107)
        put(KeyEvent.KEYCODE_ESCAPE, 120)
        // Modifiers and locks: the encoder reports them only with the Kitty keyboard protocol's
        // "report all keys as escape codes"
        put(KeyEvent.KEYCODE_ALT_LEFT, 51)
        put(KeyEvent.KEYCODE_ALT_RIGHT, 52)
        put(KeyEvent.KEYCODE_CAPS_LOCK, 54)
        put(KeyEvent.KEYCODE_MENU, 55) // Context menu
        put(KeyEvent.KEYCODE_CTRL_LEFT, 56)
        put(KeyEvent.KEYCODE_CTRL_RIGHT, 57)
        put(KeyEvent.KEYCODE_META_LEFT, 59)
        put(KeyEvent.KEYCODE_META_RIGHT, 60)
        put(KeyEvent.KEYCODE_SHIFT_LEFT, 61)
        put(KeyEvent.KEYCODE_SHIFT_RIGHT, 62)
        put(KeyEvent.KEYCODE_NUM_LOCK, 79)
        put(KeyEvent.KEYCODE_SYSRQ, 148) // Print Screen
        put(KeyEvent.KEYCODE_SCROLL_LOCK, 149)
        put(KeyEvent.KEYCODE_BREAK, 150) // Pause
    }

    private val modifiers = setOf(
        KeyEvent.KEYCODE_ALT_LEFT, KeyEvent.KEYCODE_ALT_RIGHT, KeyEvent.KEYCODE_CTRL_LEFT,
        KeyEvent.KEYCODE_CTRL_RIGHT, KeyEvent.KEYCODE_META_LEFT, KeyEvent.KEYCODE_META_RIGHT,
        KeyEvent.KEYCODE_SHIFT_LEFT, KeyEvent.KEYCODE_SHIFT_RIGHT, KeyEvent.KEYCODE_CAPS_LOCK,
        KeyEvent.KEYCODE_NUM_LOCK, KeyEvent.KEYCODE_SCROLL_LOCK,
    )

    /** The physical key, 0 (unidentified) for keys libghostty-vt has no number for. */
    fun of(keyCode: Int): Int = keys[keyCode] ?: 0

    /** Modifier and lock keys, which programs normally never see on their own. */
    fun isModifier(keyCode: Int) = keyCode in modifiers

    /** libghostty-vt's modifier bits of a key event's meta state. */
    fun mods(metaState: Int): Int {
        var m = 0
        if (metaState and KeyEvent.META_SHIFT_ON != 0) m = m or MOD_SHIFT
        if (metaState and KeyEvent.META_CTRL_ON != 0) m = m or MOD_CTRL
        if (metaState and KeyEvent.META_ALT_ON != 0) m = m or MOD_ALT
        if (metaState and KeyEvent.META_META_ON != 0) m = m or MOD_SUPER
        return m
    }
}
