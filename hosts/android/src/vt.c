// SPDX-License-Identifier: AGPL-3.0-or-later
//
// The tabs' terminal emulator (android/vtcore map): one libghostty-vt terminal per tab, behind a
// small C ABI that vt.rs calls. The frame-format specification and Frames section belong to
// android/vtrender; Input, Paste, Mouse and focus, Selection, Links, Taps on the prompt and Search
// belong to android/vtview. This file only talks to libghostty-vt; the console, threads and locking
// are vt.rs's.
//
// Threading: libghostty-vt is not thread-safe, and neither is anything here. vt.rs holds one lock
// around every call, so the terminal, the render state and the buffers below are only used by one
// thread at a time.
//
// Frame format (little-endian), written by tuxport_vt_frame and read by VtFrame.kt; the two change
// together:
//
//   header, 64 bytes:
//     u16 cols, u16 rows
//     u8  dirty            0 nothing changed, 1 some rows, 2 every row
//     u8  cursor_visible   1 when visible and inside the viewport
//     u8  cursor_style     0 bar, 1 block, 2 underline, 3 hollow block, 4 lock (the console reads
//                          a password: shown even when the program hid the cursor, never
//                          blinking)
//     u8  cursor_blinking
//     u16 cursor_x, u16 cursor_y
//     u32 foreground       0xAARRGGBB, the default colours (after OSC 10/11 and reverse video)
//     u32 background
//     u32 cursor_color     0 when the program did not set one (OSC 12)
//     u16 row_count        number of rows that follow
//     u8  viewport_bottom  1 when the viewport shows the active area (not scrolled back)
//     u8  flags            bit 0 mouse tracking on, 1 alternate screen, 2 alternate scroll
//                          (mode 1007), 3 focus events (mode 1004), 4 a selection exists,
//                          5 it is rectangular, 6 the search has not caught up with the
//                          terminal yet (draw again to go on), 7 output after resize has not
//                          settled at parser ground or synchronized output is still held
//     u16 selection_start_x, u16 selection_start_y
//                          the selection's first cell (in terminal order; a rectangle's top
//                          left corner) in the viewport, 0xffff for both when there is none or
//                          it is outside the viewport
//     u16 selection_end_x, u16 selection_end_y
//                          its last cell (a rectangle's bottom right corner), likewise
//     u32 scroll_total     the scrollable area in rows (scrollback and active area), as
//                          GhosttyTerminalScrollbar has it
//     u32 scroll_offset    the viewport's first row in that area
//     u32 scroll_len       the rows the viewport shows
//     u32 search_total     the search's matches on the active screen so far, 0 without a search
//     u32 search_selected  the selected match's place counted from the newest (1 for the
//                          newest), 0 when none is selected
//     u16 image_count      number of image placements that follow
//     u8  overscan         rows captured below the viewport: 1 while it is scrolled back (there
//                          is a row after it), else 0
//     u8  pointer          the mouse pointer's shape the program asked for (OSC 22,
//                          GhosttyMouseShape), text (an I-beam) by default
//     u32 animation_ms     when to draw again for the next frame of an animated image, 0 when
//                          none plays
//   image_count image placements (Kitty graphics), in drawing order, 56 bytes each:
//     u32 image_id         the image to draw, whose pixels tuxport_vt_image gives
//     u8  layer            0 below the cell backgrounds, 1 below the text, 2 above it
//                          (GhosttyImageLayer)
//     u8  reserved[3]
//     u64 generation       the image's pixels: a new value whenever they change
//     i32 x, i32 y         the destination's top left corner in pixels from the grid's, which
//                          can be left of or above it
//     u32 width, u32 height
//                          the destination's size in pixels, which can reach past the grid
//     u32 source_x, u32 source_y, u32 source_width, u32 source_height
//                          the part of the image drawn there, in image pixels
//     u32 image_width, u32 image_height
//                          the image's size
//   row_count rows:
//     u16 y                the row in the viewport; y == rows is the overscan row after it,
//                          which the view shows in part while it draws the grid shifted up by
//                          part of a row (scrolling by pixels)
//     u16 cell_count, u32 extra_count
//     u16 selection_from, u16 selection_to
//                          the row's selected columns (inclusive), 0xffff for both when none
//     cell_count cells of 16 bytes:
//       u32 codepoint      first codepoint of the cell's grapheme, 0 for an empty cell
//       u32 fg             0xAARRGGBB, 0 for the default foreground
//       u32 bg             0xAARRGGBB, 0 for the default background
//       u16 attrs          bit 0 bold, 1 italic, 2 faint, 3 blink, 4 inverse, 5 invisible,
//                          6 strikethrough, 7 overline, 8-10 underline (GhosttySgrUnderline),
//                          12-13 width (GhosttyCellWide: 0 narrow, 1 wide, 2 spacer tail,
//                          3 spacer head), 14 in a search match, 15 in the selected match
//       u16 extra          number of further codepoints in the grapheme
//     extra_count u32 codepoints: the further codepoints of the row's cells, in cell order
//
// A row is sent when its cells changed or when its search marks did (a new needle, another
// selected match, a match found), so the view's mirror always carries the marks; the overscan row
// carries none (its matches are marked once it is in the viewport). The view paints
// images into its rows, so every row is sent whenever the placements change and some are or were
// on the screen.

#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <ghostty/vt.h>

// Wuffs, which libghostty-vt links for Kitty graphics, also decodes the PNG images programs send
// (decode_png): the decoder Ghostty itself uses, memory-safe by construction, for data any program
// can print. Only its declarations are compiled here; the definitions are in libghostty-vt.a. The
// modules are those PNG needs of the ones Ghostty compiles (pkg/wuffs/build.zig); build.rs finds
// the header in the source's Zig packages
#define WUFFS_CONFIG__MODULES
#define WUFFS_CONFIG__MODULE__BASE
#define WUFFS_CONFIG__MODULE__ADLER32
#define WUFFS_CONFIG__MODULE__CRC32
#define WUFFS_CONFIG__MODULE__DEFLATE
#define WUFFS_CONFIG__MODULE__PNG
#define WUFFS_CONFIG__MODULE__ZLIB
#include "wuffs-v0.4.c"

#define HEADER_BYTES 64
#define IMAGE_BYTES 56
#define ROW_HEADER_BYTES 12
#define CELL_BYTES 16

#define FLAG_MOUSE 0x01
#define FLAG_ALT_SCREEN 0x02
#define FLAG_ALT_SCROLL 0x04
#define FLAG_FOCUS_EVENTS 0x08
#define FLAG_SELECTION 0x10
#define FLAG_RECTANGLE 0x20
#define FLAG_SEARCH_PENDING 0x40
#define FLAG_OUTPUT_PENDING 0x80

// The frame's cursor style while the console reads a password, after GhosttyCursorStyle's four
#define CURSOR_LOCK 4

// A cell's search mark (TuxportVt.marks), shifted into its attrs at bit 14
#define MARK_NONE 0
#define MARK_MATCH 1
#define MARK_SELECTED 2
#define MARK_SHIFT 14

#define NONE16 0xffff

// How long one frame may spend catching a search up with the terminal; the view draws again until
// it has (FLAG_SEARCH_PENDING), so a search through a long scrollback does not hold up a frame
#define SEARCH_BUDGET_NS (4LL * 1000 * 1000)

// Scrollback kept per terminal: whichever limit is reached first prunes the oldest whole pages
// (about 400 KiB each, libghostty-vt rounding up). A page holds fewer rows the wider the terminal,
// so the line limit applies up to about 120 columns and the byte limit, which bounds a tab's
// memory, beyond. Both must be set: libghostty-vt's C terminal starts with a byte limit of 10,000
// bytes, which it raises to two pages
#define SCROLLBACK_LINES ((size_t)10000)
#define SCROLLBACK_BYTES ((size_t)10 << 20)

// The default colours, those of the render state's built-ins and the terminal's black window.
// Unset, libghostty-vt answers no OSC 10 or 11 query, reverse video (DECSCNM) swaps nothing, and a
// colour set with OSC 10 or 11 alone does not show. The cursor gets none, so it follows the
// foreground, as in Ghostty without cursor-color (and an OSC 12 query answers the foreground)
static const GhosttyColorRgb DEFAULT_FOREGROUND = {.r = 0xff, .g = 0xff, .b = 0xff};
static const GhosttyColorRgb DEFAULT_BACKGROUND = {.r = 0, .g = 0, .b = 0};

// Synchronized output (mode 2026) holds the screen at most this long, so a program that never
// ends its hold cannot freeze the tab
#define HOLD_LIMIT_NS (1000LL * 1000 * 1000)

// A PTY read is not a redraw boundary. Allow adjacent chunks (including a split mode 2026
// prefix) to arrive before the view releases its resize preview.
#define RESIZE_QUIET_NS (32LL * 1000 * 1000)

// The largest clipboard write (OSC 52) taken: Android's clipboard goes through a binder
// transaction, which fails beyond about 1 MB (the text is sent as UTF-16)
#define CLIPBOARD_MAX ((size_t)256 * 1024)

// The most an OSC 5522 write may carry in all its representations: libghostty-vt buffers a write
// whole before handing it over, up to 64 MiB by default (the protocol's minimum), and fails one
// beyond this limit with EFBIG. It leaves room for other types of the same text (text/html), which
// are dropped
#define KITTY_CLIPBOARD_MAX ((size_t)1 << 20)

// What a notification (OSC 9, OSC 777) keeps of its title and body
#define NOTIFICATION_MAX ((size_t)4096)

// How much of an OSC string's start tuxport_vt_feed keeps to find clipboard reads (OSC 52 with "?"
// for data: "52;", the clipboards, ";?", which fit with all eight clipboards named) and how the
// shell moves its cursor (OSC 133;A with its options: Ghostty's bash integration sends
// "133;A;redraw=last;cl=line;aid=" and the shell's pid)
#define OSC_HEAD 64

// tmux's control mode (tmux -CC) wraps everything it prints in a DCS string with this header, for
// terminals that turn it into windows and tabs (iTerm2's). libghostty-vt built for apps leaves
// tmux control mode out with Oniguruma, so it swallows the string as an unknown DCS: the tab would
// show nothing until tmux left control mode, while keys reached tmux as commands. tuxport_vt_feed
// keeps the first DCS_HEAD bytes of a DCS string to find it
#define TMUX_CONTROL "1000p"
#define DCS_HEAD 5

// How a tap on the input typed at the shell's prompt moves its cursor there
// (tuxport_vt_prompt_click), as the last prompt's start (OSC 133;A) declared: not at all; with left
// and right arrows within the input's line (cl=line, and cl=m, v and w, which Ghostty treats as
// line too); or with a mouse press the shell places the cursor by itself (Kitty's click_events=1,
// its row counted from the screen's top, =2 from the prompt's first row)
#define PROMPT_CLICK_NONE 0
#define PROMPT_CLICK_ARROWS 1
#define PROMPT_CLICK_ABSOLUTE 2
#define PROMPT_CLICK_RELATIVE 3

// The option by which a prompt (OSC 133;A or P) says that its shell marks no output start and
// breaks the lines of a long command itself, as busybox ash does (tuxport_vt_feed's line feeds):
// the profile's ash prompts carry it (android/setup's ashrc.sh)
#define PROMPT_LF_OPTION "tuxport_lf"
#define PROMPT_LF_OUTPUT "output"

// The output start (OSC 133;C) tuxport_vt_feed marks for such a shell
#define OUTPUT_START "\x1b]133;C\x1b\\"
// The marks tuxport_vt_feed puts after another writer's output at such a shell's prompt: a
// continuation prompt with no text (OSC 133;P;k=s) and the input's start (OSC 133;B), so what is
// typed from there on is input again
#define INPUT_RESUMED "\x1b]133;P;k=s\x1b\\\x1b]133;B\x1b\\"

// Rows above the cursor searched for the first row of the prompt it is at (tuxport_vt_prompt_click)
#define PROMPT_ROWS 64

// The memory each screen's Kitty images may take, decoded; beyond it libghostty-vt deletes the
// oldest images no placement shows, and refuses an image that does not fit at all. About three
// images the size of a phone's screen; Ghostty's default is 320 MB, libghostty-vt's 10 MB
#define KITTY_STORAGE ((uint64_t)32 << 20)

// What each screen's Kitty images are cut down to when the app trims its memory under pressure, in
// tabs not shown (tuxport_vt_drop_images): about one image the size of a phone's screen
#define KITTY_TRIMMED ((uint64_t)4 << 20)

// The shortest wait between two frames of an animated image (about 120 Hz)
#define ANIMATION_MIN_MS 8

// libghostty-vt's warnings and errors, and the sequences it does not implement, go to stderr (which
// the app sends to logcat): at most this many lines a second across all tabs, so a program
// repeating one cannot flood the log
#define LOG_PER_SECOND 20

// How much of an unimplemented OSC or APC string libghostty-vt keeps for on_unknown_sequence,
// which logs only an OSC's number or an APC's protocol letter, never its data. Within the buffer
// libghostty-vt already has for OSC strings, so they cost no allocation (an APC's always does)
#define UNKNOWN_MAX ((size_t)16)

// The terminfo entry programs run under (ConsolePty sets TERM to it), the answer to XTGETTCAP's
// TN. Unset, libghostty-vt leaves that query unanswered; the other capabilities it answers from
// Ghostty's own entry either way
static const char TERMINFO_NAME[] = "xterm-256color";

// What tuxport_vt_compress_step asks of vt.rs: nothing until the terminal changes again, another
// step soon, or another idle interval first (the terminal changed since the last look)
#define TUXPORT_COMPRESS_DONE 0
#define TUXPORT_COMPRESS_STEP 1
#define TUXPORT_COMPRESS_WAIT 2

// The text buffer (TuxportVt.text) a finished compression pass leaves allocated: a transcript or a
// large copy grows it to their size, and it is not needed again until the next one
#define TEXT_KEEP ((size_t)64 * 1024)

// The most of an unfinished sequence libghostty-vt keeps to replay (continuation tracking): a
// snapshot of a terminal whose parser is inside a sequence needs it, or encoding fails until the
// parser is back at ground. Programs send long strings in chunks (a Kitty image's are 4 KiB), so
// only a save that falls inside a longer one finds none
#define CONTINUATION_MAX ((size_t)64 * 1024)

// The largest snapshot kept for a tab (tuxport_vt_snapshot): a full scrollback of coloured output
// takes about 1.5 MB, so beyond this it is mostly Kitty images' pixels, and the tab comes back from
// its plain transcript instead
#define SNAPSHOT_MAX ((size_t)16 << 20)

// The state of a tab's snapshot (TuxportVt.snapshot): the terminal changed since the last one, the
// last one is still the terminal's, or none could be taken of the terminal as it is
#define SNAPSHOT_STALE 0
#define SNAPSHOT_TAKEN 1
#define SNAPSHOT_NONE 2

// A program status report (OSC 7501) as tuxport_vt_take_effects queues it for vt.rs, which reads the
// same layout: u8 state (GhosttyProgramStatusState), u8 kind (GhosttyProgramStatusKind), i8 progress
// (0-100, -1 for none), u8 id length, u8 title length, u16 message length, then the id, title and
// message bytes. The specification's limits (id 128, title 192, message 2048 bytes) fit the fields
#define STATUS_HEADER_BYTES 7

typedef struct {
  uint8_t *ptr;
  size_t len;
  size_t cap;
} Buf;

static bool buf_reserve(Buf *b, size_t extra) {
  if (b->len + extra <= b->cap) return true;
  size_t cap = b->cap ? b->cap : 4096;
  while (cap < b->len + extra) cap *= 2;
  uint8_t *p = realloc(b->ptr, cap);
  if (p == NULL) return false;
  b->ptr = p;
  b->cap = cap;
  return true;
}

static void put_u16(uint8_t *p, uint16_t v) {
  p[0] = (uint8_t)v;
  p[1] = (uint8_t)(v >> 8);
}

static void put_u32(uint8_t *p, uint32_t v) {
  p[0] = (uint8_t)v;
  p[1] = (uint8_t)(v >> 8);
  p[2] = (uint8_t)(v >> 16);
  p[3] = (uint8_t)(v >> 24);
}

static void put_u64(uint8_t *p, uint64_t v) {
  put_u32(p, (uint32_t)v);
  put_u32(p + 4, (uint32_t)(v >> 32));
}

static uint32_t clamp_u32(uint64_t v) {
  return v > UINT32_MAX ? UINT32_MAX : (uint32_t)v;
}

static int32_t clamp_i32(int64_t v) {
  return v > INT32_MAX ? INT32_MAX : v < INT32_MIN ? INT32_MIN : (int32_t)v;
}

static uint32_t argb(GhosttyColorRgb c) {
  return 0xff000000u | ((uint32_t)c.r << 16) | ((uint32_t)c.g << 8) | c.b;
}

static int64_t now_ns(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (int64_t)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

// What tuxport_vt_take_effects reports (vt.rs's `Effects` has the same layout)
#define TUXPORT_CLIPBOARD_NONE 0
#define TUXPORT_CLIPBOARD_TEXT 1
#define TUXPORT_CLIPBOARD_CLEAR 2

typedef struct {
  uint32_t bells;
  int32_t clipboard;
  const uint8_t *clip;
  size_t clip_len;
  bool notified;
  const uint8_t *title;
  size_t title_len;
  const uint8_t *body;
  size_t body_len;
  // The last progress report (OSC 9;4): its GhosttyTerminalProgressState, -1 for none, and its
  // percentage, -1 when it has none
  int32_t progress_state;
  int32_t progress;
  // Every program status report (OSC 7501), in order, as STATUS_HEADER_BYTES describes
  const uint8_t *statuses;
  size_t statuses_len;
  // Clipboard reads (OSC 52 "?"): how many, and the clipboard the last one named ('c', 'p' or 's')
  uint32_t clipboard_reads;
  uint8_t clipboard_read_kind;
  // The last command the shell's prompt marks (OSC 133) saw finish (on_semantic_prompt): whether
  // one did, whether the shell reported its exit code, the code, and how long it ran
  bool command_finished;
  bool command_has_exit;
  int32_t command_exit;
  uint64_t command_ms;
  // tmux's control mode, which the app detached (tmux_started), ended
  bool tmux_detached;
} TuxportVtEffects;

// One image placement of a frame (see the format above), with what puts it in drawing order and
// joins the rows of a virtual placement (images_changed)
typedef struct {
  uint32_t id;
  uint8_t layer;
  int32_t z;
  uint8_t kind;
  uint32_t placement_id;
  uint64_t generation;
  int32_t x;
  int32_t y;
  uint32_t width;
  uint32_t height;
  uint32_t source_x;
  uint32_t source_y;
  uint32_t source_width;
  uint32_t source_height;
  uint32_t image_width;
  uint32_t image_height;
} Placement;

typedef struct TuxportVt {
  GhosttyTerminal term;
  GhosttyRenderState state;
  GhosttyRenderStateRowIterator rows;
  GhosttyRenderStateRowCells cells;
  GhosttyKeyEncoder encoder;
  GhosttyKeyEvent event;
  GhosttyMouseEncoder mouse;
  GhosttyMouseEvent mouse_event;
  uint16_t cols;
  uint16_t height;
  uint32_t cell_width;
  uint32_t cell_height;
  // Bytes the terminal answers with (DA, DSR, mode reports), for the console's input
  Buf replies;
  // The last frame, see the format above
  Buf frame;
  // Scratch for one cell's grapheme
  uint32_t *grapheme;
  size_t grapheme_cap;
  // Scratch for one row's further codepoints
  Buf extras;
  // The last text handed out (copy, title, transcript), valid until the next such call or the end
  // of a compression pass, which frees it when large (release_text)
  Buf text;
  // A program set the title (OSC 0/2) since tuxport_vt_take_title_changed
  bool title_changed;
  // Effects for the app since tuxport_vt_take_effects: bells rung (BEL), the last clipboard write
  // (OSC 52: text, or a request to clear), the last notification (OSC 9, OSC 777), the last
  // progress report (OSC 9;4; progress_state -1 for none) and the program status reports (OSC 7501)
  uint32_t bells;
  int clipboard;
  Buf clip;
  bool notified;
  Buf note_title;
  Buf note_body;
  int32_t progress_state;
  int32_t progress;
  Buf statuses;
  // Clipboard reads (OSC 52 "?") since tuxport_vt_take_effects, and the clipboard of the last
  uint32_t clipboard_reads;
  uint8_t clipboard_read_kind;
  // Commands as the prompt marks tell (on_semantic_prompt): when the one running started (0 for
  // none), whether the cursor was at the prompt after the last feed, and the last one finished
  // since tuxport_vt_take_effects
  int64_t command_start;
  bool was_at_prompt;
  bool command_finished;
  bool command_has_exit;
  int32_t command_exit;
  uint64_t command_ms;
  // The OSC string the output is in (tuxport_vt_feed): its first OSC_HEAD bytes, and its length so
  // far (which may be larger)
  bool osc_open;
  uint8_t osc_head[OSC_HEAD];
  size_t osc_len;
  // The DCS string the output is in (tuxport_vt_feed): its first DCS_HEAD bytes and its length so
  // far; whether it is tmux's control mode, which the app detached (tmux_started), and whether one
  // ended since tuxport_vt_take_effects
  bool dcs_open;
  uint8_t dcs_head[DCS_HEAD];
  size_t dcs_len;
  bool tmux_control;
  bool tmux_detached;
  // How a tap on the shell's input moves its cursor (PROMPT_CLICK_*), as the primary screen's last
  // prompt start declared it (osc_ended); a full reset forgets it, as libghostty-vt's screen does
  uint8_t prompt_click;
  // The shell's last prompt asked for its line feeds to be read (PROMPT_LF_OPTION): lf_output
  // while that shell's prompts are the last seen (rows it broke itself are one line to taps),
  // lf_armed from such a prompt until the line feed that ends its input, lf_marking while the
  // output start fed there is processed, and lf_returns, the carriage returns that ended the last
  // feed (at most 2)
  bool lf_output;
  bool lf_armed;
  bool lf_marking;
  uint8_t lf_returns;
  // The fixed end of a selection being dragged by a handle (tuxport_vt_select_to): the end opposite
  // the handle, or a whole output selected; tracked, so output does not move it
  GhosttyTrackedGridRef anchor_start;
  GhosttyTrackedGridRef anchor_end;
  // A press that selected a word or a line and the drag after it (tuxport_vt_gesture_*), or a
  // mouse's clicks and drags (tuxport_vt_mouse_*): libghostty-vt's selection gesture, which tracks
  // the pressed cell, and its reusable events (a mouse's presses are timed, a finger's not)
  GhosttySelectionGesture gesture;
  GhosttySelectionGestureEvent gesture_press;
  GhosttySelectionGestureEvent gesture_drag;
  GhosttySelectionGestureEvent mouse_press;
  GhosttySelectionGestureEvent mouse_release;
  // A cell in a command's output that a long press found (tuxport_vt_output_at), for the menu's
  // Select output; tracked, so output does not move it. NULL when none
  GhosttyTrackedGridRef output_at;
  // Synchronized output: the render state was captured when the hold began and is not updated
  // again until it ends (or HOLD_LIMIT_NS passes)
  bool held;
  int64_t held_since;
  // A resize has not yet been followed by settled output. Resize itself dirties rows before
  // the program redraws; a nonempty feed starts the quiet period, not an immediate release.
  bool resize_pending;
  int64_t resize_output_at;
  // The console reads a password (tuxport_vt_set_password_input): frames give the cursor as a lock
  bool password_input;
  // The cell of the last mouse event reported, to report motion once per cell rather than for
  // every pixel a drag or hover moves: the encoder's own deduplication forgets the cell whenever
  // its options are set, which tuxport_vt_mouse does for every event
  int mouse_x;
  int mouse_y;
  // The search (tuxport_vt_search), NULL while none runs. Frames drive it: each one catches it up
  // with the terminal and marks its matches in the viewport
  GhosttySearch search;
  // Scratch for the matches on the viewport's pages
  GhosttySelection *matches;
  size_t matches_cap;
  // Scratch for all the search's matches and the rows they start on (tuxport_vt_search_rows),
  // freed when the search ends
  GhosttySelection *all_matches;
  size_t all_matches_cap;
  uint32_t *match_rows;
  size_t match_rows_cap;
  // The search mark (MARK_*) of each viewport cell, row by row: as of this frame, and as last sent
  // (a row whose marks changed is sent again); marks_cells cells each, the grid's when allocated
  uint8_t *marks;
  uint8_t *marks_sent;
  size_t marks_cells;
  // Scrollback compression (see there): a pass is scheduled (vt.rs runs its steps), the activity
  // token as last looked at, and a read restored compressed pages since the last pass
  bool compress_scheduled;
  uint64_t compress_seen;
  bool compress_touched;
  // The last transcript is still the screen's text, and the state of the last snapshot
  // (SNAPSHOT_*): both go stale with output, a message, a resize, clearing or a reset (changed)
  bool transcribed;
  int snapshot;
  // Kitty graphics: the render state's image list, read into placed (in drawing order) whenever
  // its placements generation changes, and that generation as last read
  GhosttyRenderStateImageIterator images;
  Placement *placed;
  size_t placed_len;
  size_t placed_cap;
  uint64_t images_generation;
} TuxportVt;

// ---- Effects (called inside ghostty_terminal_vt_write, under vt.rs's lock) ----

static void on_write_pty(GhosttyTerminal t, void *userdata, const uint8_t *data, size_t len) {
  (void)t;
  TuxportVt *vt = userdata;
  if (!buf_reserve(&vt->replies, len)) return;
  memcpy(vt->replies.ptr + vt->replies.len, data, len);
  vt->replies.len += len;
}

static bool on_size(GhosttyTerminal t, void *userdata, GhosttySizeReportSize *out) {
  (void)t;
  TuxportVt *vt = userdata;
  out->columns = vt->cols;
  out->rows = vt->height;
  out->cell_width = vt->cell_width;
  out->cell_height = vt->cell_height;
  return true;
}

static bool on_color_scheme(GhosttyTerminal t, void *userdata, GhosttyColorScheme *out) {
  (void)t;
  (void)userdata;
  *out = GHOSTTY_COLOR_SCHEME_DARK;
  return true;
}

// Without an answer to DA, programs that probe for it (vim, tmux) wait for a timeout. Clipboard
// access (52) is listed because clipboard writes are taken (on_clipboard_write), as Ghostty lists
// it unless clipboard-write = deny; programs looking for it treat OSC 52 as unsupported otherwise
static bool on_device_attributes(GhosttyTerminal t, void *userdata, GhosttyDeviceAttributes *out) {
  (void)t;
  (void)userdata;
  out->primary.conformance_level = GHOSTTY_DA_CONFORMANCE_VT220;
  out->primary.features[0] = GHOSTTY_DA_FEATURE_COLUMNS_132;
  out->primary.features[1] = GHOSTTY_DA_FEATURE_SELECTIVE_ERASE;
  out->primary.features[2] = GHOSTTY_DA_FEATURE_ANSI_COLOR;
  out->primary.features[3] = GHOSTTY_DA_FEATURE_CLIPBOARD;
  out->primary.num_features = 4;
  out->secondary.device_type = 1;
  out->secondary.firmware_version = 1;
  out->secondary.rom_cartridge = 0;
  out->tertiary.unit_id = 0;
  return true;
}

static GhosttyString on_xtversion(GhosttyTerminal t, void *userdata) {
  (void)t;
  (void)userdata;
  static const char name[] = "tuxport";
  return (GhosttyString){.ptr = (const uint8_t *)name, .len = sizeof(name) - 1};
}

static void on_title_changed(GhosttyTerminal t, void *userdata) {
  (void)t;
  TuxportVt *vt = userdata;
  vt->title_changed = true;
}

static void on_bell(GhosttyTerminal t, void *userdata) {
  (void)t;
  TuxportVt *vt = userdata;
  vt->bells++;
}

static bool keep(Buf *b, GhosttyString s, size_t max) {
  size_t len = s.len < max ? s.len : max;
  b->len = 0;
  if (!buf_reserve(b, len)) return false;
  memcpy(b->ptr, s.ptr, len);
  b->len = len;
  return true;
}

static bool is_text(GhosttyString mime) {
  static const char plain[] = "text/plain";
  return mime.len >= sizeof(plain) - 1 && memcmp(mime.ptr, plain, sizeof(plain) - 1) == 0;
}

// A program writes the clipboard (OSC 52, OSC 1337, OSC 5522; Ghostty's default
// clipboard-write = allow). Android has one clipboard, so every location goes there; only text is
// taken, up to CLIPBOARD_MAX. Reads have no callback: libghostty-vt asks for the answer to one
// within the callback, while the app asks the user first, so OSC 52 reads are found and answered
// apart (tuxport_vt_feed), and OSC 5522 reads are refused
static void on_clipboard_write(GhosttyTerminal t, void *userdata, const GhosttyClipboardWrite *w) {
  (void)t;
  TuxportVt *vt = userdata;
  GhosttyClipboardWriteReply reply = {.size = sizeof(reply)};
  const GhosttyClipboardContent *text = NULL;
  for (size_t i = 0; i < w->contents_len && text == NULL; i++) {
    if (is_text(w->contents[i].mime)) text = &w->contents[i];
  }
  if (w->contents_len == 0) {
    vt->clipboard = TUXPORT_CLIPBOARD_CLEAR;
    reply.result = GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS;
  } else if (text == NULL) {
    reply.result = GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED;
  } else if (text->data.len > CLIPBOARD_MAX) {
    reply.result = GHOSTTY_CLIPBOARD_WRITE_RESULT_DENIED;
  } else if (!keep(&vt->clip, text->data, CLIPBOARD_MAX)) {
    reply.result = GHOSTTY_CLIPBOARD_WRITE_RESULT_IO_ERROR;
  } else {
    vt->clipboard = TUXPORT_CLIPBOARD_TEXT;
    reply.result = GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS;
  }
  w->reply(w, &reply);
}

// OSC 9 (body only) or OSC 777 notify (title and body)
static void on_notification(GhosttyTerminal t, void *userdata,
                            const GhosttyTerminalDesktopNotification *n) {
  (void)t;
  TuxportVt *vt = userdata;
  if (keep(&vt->note_title, n->title, NOTIFICATION_MAX) &&
      keep(&vt->note_body, n->body, NOTIFICATION_MAX)) {
    vt->notified = true;
  }
}

// OSC 9;4: only the last report of a read matters, as each replaces the one before. A full reset
// reports a removal
static void on_progress(GhosttyTerminal t, void *userdata, const GhosttyTerminalProgressReport *r) {
  (void)t;
  TuxportVt *vt = userdata;
  vt->progress_state = (int32_t)r->state;
  vt->progress = r->progress;
}

static size_t at_most(size_t len, size_t max) {
  return len < max ? len : max;
}

// OSC 7501: every report is queued, since each replaces the record of its id only; the app keeps
// the records (VtSession). A full reset reports a clear of every record
static void on_program_status(GhosttyTerminal t, void *userdata,
                              const GhosttyTerminalProgramStatus *s) {
  (void)t;
  TuxportVt *vt = userdata;
  size_t id = at_most(s->id.len, UINT8_MAX);
  size_t title = at_most(s->title.len, UINT8_MAX);
  size_t message = at_most(s->message.len, UINT16_MAX);
  if (!buf_reserve(&vt->statuses, STATUS_HEADER_BYTES + id + title + message)) return;
  uint8_t *p = vt->statuses.ptr + vt->statuses.len;
  p[0] = (uint8_t)s->state;
  p[1] = (uint8_t)s->kind;
  p[2] = (uint8_t)s->progress;
  p[3] = (uint8_t)id;
  p[4] = (uint8_t)title;
  put_u16(p + 5, (uint16_t)message);
  p += STATUS_HEADER_BYTES;
  if (id > 0) memcpy(p, s->id.ptr, id);
  if (title > 0) memcpy(p + id, s->title.ptr, title);
  if (message > 0) memcpy(p + id + title, s->message.ptr, message);
  vt->statuses.len += STATUS_HEADER_BYTES + id + title + message;
}

// OSC 133: a command starts when the shell marks its output's start (bash's PS0, zsh's preexec)
// or, as busybox ash marks none (its input ends at a line feed, where tuxport_vt_feed marks the
// output's start for it), when a feed ends with the cursor no longer at the prompt
// (tuxport_vt_feed; output of a background job at the prompt ends no input and so starts none,
// except with an edited profile's older marks, whose input ends with its line); it finishes at
// the command end the next prompt reports, with the exit code (every marked shell sends it). Only
// a command that started is reported: a prompt drawn again (after a resize, or Ctrl-C at the
// prompt) reports a command end too, with none running
static void on_semantic_prompt(GhosttyTerminal t, void *userdata,
                               const GhosttyTerminalSemanticPrompt *e) {
  (void)t;
  TuxportVt *vt = userdata;
  switch (e->kind) {
    case GHOSTTY_SEMANTIC_PROMPT_OUTPUT_START:
      // The mark is the command's start, whatever left the prompt before it; one tuxport_vt_feed
      // made at a line feed is not the shell's word that a command runs (the line feed may begin
      // a list of completions or follow Ctrl-C), so the cursor leaving the prompt decides there
      if (!vt->lf_marking) vt->command_start = now_ns();
      break;
    case GHOSTTY_SEMANTIC_PROMPT_COMMAND_END:
      if (vt->command_start == 0) break;
      vt->command_finished = true;
      vt->command_has_exit = e->has_exit_code;
      vt->command_exit = e->has_exit_code ? e->exit_code : 0;
      vt->command_ms = (uint64_t)(now_ns() - vt->command_start) / 1000000;
      vt->command_start = 0;
      break;
    case GHOSTTY_SEMANTIC_PROMPT_PROMPT_START:
      // A new prompt without a command end: what ran is not known to have finished
      if (e->prompt_kind == GHOSTTY_SEMANTIC_PROMPT_PROMPT_PRIMARY) vt->command_start = 0;
      break;
    default:
      break;
  }
}

// A hold began: capture the screen now, which is the frame the program wants to keep showing
static void on_render_hold(GhosttyTerminal t, void *userdata, bool held) {
  TuxportVt *vt = userdata;
  if (held) {
    ghostty_render_state_update(vt->state, t);
    vt->held = true;
    vt->held_since = now_ns();
  } else {
    vt->held = false;
  }
}

// A program's full reset (RIS): the screen forgets how its shell's prompt moves the cursor
static void on_reset(GhosttyTerminal t, void *userdata) {
  (void)t;
  TuxportVt *vt = userdata;
  vt->prompt_click = PROMPT_CLICK_NONE;
  vt->lf_output = false;
  vt->lf_armed = false;
}

// ---- PNG images (Kitty graphics) ----

// Decodes a PNG image a program sent into RGBA (straight alpha) allocated from allocator, as
// libghostty-vt's sys interface asks (GhosttySysDecodePngFn): with Wuffs, as Ghostty does
// (pkg/wuffs/src/png.zig). Called inside ghostty_terminal_vt_write; an image larger than a
// screen's whole storage is refused before its pixels are allocated
static bool decode_png(void *userdata, const GhosttyAllocator *allocator, const uint8_t *data,
                       size_t data_len, GhosttySysImage *out) {
  (void)userdata;
  wuffs_png__decoder *decoder = wuffs_png__decoder__alloc();
  if (decoder == NULL) return false;
  bool ok = false;
  uint8_t *pixels = NULL;
  size_t size = 0;
  uint8_t *work = NULL;
  wuffs_base__io_buffer source = wuffs_base__ptr_u8__reader((uint8_t *)data, data_len, true);
  wuffs_base__image_config config;
  wuffs_base__status status =
      wuffs_png__decoder__decode_image_config(decoder, &config, &source);
  if (!wuffs_base__status__is_ok(&status)) goto done;
  uint32_t width = wuffs_base__pixel_config__width(&config.pixcfg);
  uint32_t height = wuffs_base__pixel_config__height(&config.pixcfg);
  if (width == 0 || height == 0 || (uint64_t)width * height > KITTY_STORAGE / 4) goto done;
  wuffs_base__pixel_config__set(&config.pixcfg, WUFFS_BASE__PIXEL_FORMAT__RGBA_NONPREMUL,
                                WUFFS_BASE__PIXEL_SUBSAMPLING__NONE, width, height);
  size = (size_t)width * height * 4;
  pixels = ghostty_alloc(allocator, size);
  if (pixels == NULL) goto done;
  uint64_t work_len = wuffs_png__decoder__workbuf_len(decoder).max_incl;
  if (work_len > SIZE_MAX) goto done;
  // malloc(0) may give NULL, which Wuffs takes for a missing buffer
  work = malloc(work_len > 0 ? (size_t)work_len : 1);
  if (work == NULL) goto done;
  wuffs_base__pixel_buffer buffer;
  status = wuffs_base__pixel_buffer__set_from_slice(&buffer, &config.pixcfg,
                                                     wuffs_base__make_slice_u8(pixels, size));
  if (!wuffs_base__status__is_ok(&status)) goto done;
  status = wuffs_png__decoder__decode_frame(decoder, &buffer, &source, WUFFS_BASE__PIXEL_BLEND__SRC,
                                            wuffs_base__make_slice_u8(work, (size_t)work_len),
                                            NULL);
  if (!wuffs_base__status__is_ok(&status)) goto done;
  out->width = width;
  out->height = height;
  out->data = pixels;
  out->data_len = size;
  ok = true;
done:
  if (!ok && pixels != NULL) ghostty_free(allocator, pixels, size);
  free(work);
  free(decoder);
  return ok;
}

// ---- Logging ----

// Whether another line may be logged in this second (LOG_PER_SECOND across all threads; a line
// racing the second's turn may slip through)
static bool log_allowed(void) {
  static _Atomic int64_t second;
  static _Atomic uint32_t lines;
  int64_t now = now_ns() / 1000000000LL;
  int64_t seen = atomic_load_explicit(&second, memory_order_relaxed);
  if (seen != now && atomic_compare_exchange_strong_explicit(&second, &seen, now,
                                                             memory_order_relaxed,
                                                             memory_order_relaxed)) {
    atomic_store_explicit(&lines, 0, memory_order_relaxed);
  }
  return atomic_fetch_add_explicit(&lines, 1, memory_order_relaxed) < LOG_PER_SECOND;
}

// libghostty-vt's own log, on any thread: warnings (an unimplemented CSI or ESC sequence or mode,
// a malformed OSC 52) and errors. The info messages a release build also emits are routine
static void on_log(void *userdata, GhosttySysLogLevel level, const uint8_t *scope, size_t scope_len,
                   const uint8_t *message, size_t message_len) {
  (void)userdata;
  if (level > GHOSTTY_SYS_LOG_LEVEL_WARNING || !log_allowed()) return;
  fprintf(stderr, "vt: %s(%.*s): %.*s\n", level == GHOSTTY_SYS_LOG_LEVEL_ERROR ? "error" : "warning",
          (int)scope_len, scope_len > 0 ? (const char *)scope : "", (int)message_len,
          message_len > 0 ? (const char *)message : "");
}

// An OSC or APC string libghostty-vt does not implement (it logs the CSI and ESC ones itself),
// named by an OSC's leading number or an APC's first letter (its protocol) and nothing else: the
// data may be anything the program sent, a file or a password among it, and a malformed string
// (an OSC not starting with a number) may start with it
static void on_unknown_sequence(GhosttyTerminal t, void *userdata,
                                const GhosttyTerminalUnknownSequence *seq) {
  (void)t;
  (void)userdata;
  char name[UNKNOWN_MAX + 1];
  size_t len = 0;
  const char *kind;
  if (seq->tag == GHOSTTY_TERMINAL_UNKNOWN_SEQUENCE_OSC) {
    kind = "OSC";
    GhosttyString s = seq->value.osc.content;
    while (len < s.len && len < UNKNOWN_MAX && s.ptr[len] >= '0' && s.ptr[len] <= '9') {
      name[len] = (char)s.ptr[len];
      len++;
    }
  } else if (seq->tag == GHOSTTY_TERMINAL_UNKNOWN_SEQUENCE_APC) {
    kind = "APC";
    GhosttyString s = seq->value.apc.content;
    uint8_t c = s.len > 0 ? s.ptr[0] : 0;
    if ((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z')) name[len++] = (char)c;
  } else {
    return;
  }
  name[len] = 0;
  if (log_allowed()) fprintf(stderr, "vt: unsupported %s %s\n", kind, len > 0 ? name : "(unnamed)");
}

// libghostty-vt's sys interface is process-wide: the PNG decoder and the log are installed with
// the first terminal
static pthread_once_t sys_once = PTHREAD_ONCE_INIT;

static void install_sys(void) {
  ghostty_sys_set(GHOSTTY_SYS_OPT_DECODE_PNG, (const void *)decode_png);
  ghostty_sys_set(GHOSTTY_SYS_OPT_LOG, (const void *)on_log);
}

// ---- Lifecycle ----

void tuxport_vt_free(TuxportVt *vt);

static TuxportVt *wrap(GhosttyTerminal term, uint16_t cols, uint16_t rows, uint32_t cell_width,
                      uint32_t cell_height);
static void settle(GhosttyTerminal term);

// A new terminal of cols × rows cells, each cell_width × cell_height pixels
TuxportVt *tuxport_vt_new(uint16_t cols, uint16_t rows, uint32_t cell_width, uint32_t cell_height) {
  GhosttyTerminal term = NULL;
  if (ghostty_terminal_new(NULL, &term, cols, rows) != GHOSTTY_SUCCESS) return NULL;
  return wrap(term, cols, rows, cell_width, cell_height);
}

// The terminal a snapshot (tuxport_vt_snapshot) holds, on a grid of cols × rows cells: its screens,
// scrollback (compressed as it is restored), images, colours and title, without what the programs
// that ran in it had set up for themselves (settle). NULL when the snapshot cannot be decoded,
// which a snapshot from another version of libghostty-vt may well be: the format carries no
// compatibility guarantee yet.
TuxportVt *tuxport_vt_restore(const uint8_t *data, size_t len, uint16_t cols, uint16_t rows,
                            uint32_t cell_width, uint32_t cell_height) {
  GhosttySnapshotDecoder decoder = NULL;
  if (ghostty_snapshot_decoder_new_buf(NULL, &decoder, data, len) != GHOSTTY_SUCCESS) return NULL;
  bool compress = true;
  // The snapshot does not carry the limit of the terminal that wrote it
  uint64_t kitty_limit = KITTY_STORAGE;
  GhosttyTerminal term = NULL;
  bool ok = ghostty_snapshot_decoder_set(decoder, GHOSTTY_SNAPSHOT_DECODER_OPT_COMPRESS_HISTORY,
                                         &compress) == GHOSTTY_SUCCESS &&
            ghostty_snapshot_decoder_set(decoder,
                                         GHOSTTY_SNAPSHOT_DECODER_OPT_KITTY_IMAGE_STORAGE_LIMIT,
                                         &kitty_limit) == GHOSTTY_SUCCESS &&
            ghostty_snapshot_decoder_decode(decoder, &term) == GHOSTTY_SUCCESS;
  ghostty_snapshot_decoder_free(decoder);
  if (!ok) return NULL;
  settle(term);
  return wrap(term, cols, rows, cell_width, cell_height);
}

// The cursor's defaults, the user's setting (Ghostty's cursor-style and cursor-style-blink): its
// shape (GhosttyCursorStyle: bar, block or underline) and whether it blinks. Shown at once unless a
// program chose a cursor of its own (DECSCUSR), and what DECSCUSR's default (CSI 0 q), DECSTR and a
// full reset bring back; a full reset keeps the setting.
void tuxport_vt_set_cursor(TuxportVt *vt, int style, bool blink) {
  GhosttyCursorStyle shape = (GhosttyCursorStyle)style;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_STYLE, &shape);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_BLINK, &blink);
}

static void append(Buf *b, const char *s) {
  size_t len = strlen(s);
  if (!buf_reserve(b, len)) return;
  memcpy(b->ptr + b->len, s, len);
  b->len += len;
}

// A restored terminal's programs are gone, and a new shell starts below what they left: what they
// set up for themselves is undone first, before any callback is installed, so it reports nothing
// and its replies go nowhere. A sequence they were in the middle of is cancelled, a full-screen
// program's screen is left for the primary one, and a soft reset (DECSTR) brings back the cursor,
// margins, attributes, cursor and keypad modes and the palette; the modes it leaves alone are
// reset by name (reverse video, mouse tracking, focus events, bracketed paste, synchronized
// output, colour scheme and size reports, Kitty paste events; alternate scroll is set, its
// default), as are the Kitty keyboard flags, an open hyperlink, the title and the mouse pointer's
// shape (OSC 22, which a full reset keeps). The cursor then
// starts a line of its own with what follows marked as output, not as the input of a prompt the
// snapshot was taken at (a resize at a prompt clears it for the shell to redraw), and nothing is
// left below it.
static void settle(GhosttyTerminal term) {
  Buf seq = {0};
  bool ground = true;
  if (ghostty_terminal_get(term, GHOSTTY_TERMINAL_DATA_VT_GROUND, &ground) == GHOSTTY_SUCCESS &&
      !ground) {
    append(&seq, "\x18");
  }
  GhosttyTerminalScreen screen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
  ghostty_terminal_get(term, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen);
  if (screen == GHOSTTY_TERMINAL_SCREEN_ALTERNATE) append(&seq, "\x1b[?1049l");
  append(&seq, "\x1b[!p");
  // (1001, highlight tracking, is not implemented: resetting it would only log a warning)
  append(&seq, "\x1b[?5;9;1000;1002;1003;1004;1005;1006;1015;1016;2004;2026;2031;2048;5522l");
  append(&seq, "\x1b[?1007h\x1b[<99u\x1b]8;;\x1b\\\x1b]0;\x1b\\\x1b]22;\x1b\\");
  if (seq.ptr != NULL) ghostty_terminal_vt_write(term, seq.ptr, seq.len);
  seq.len = 0;
  // Output first: a line feed in a prompt's input marks the next row as the prompt's
  // continuation, and at the start of a row this unmarks it (OSC 133;C)
  append(&seq, "\x1b]133;C\x1b\\");
  if (seq.ptr != NULL) ghostty_terminal_vt_write(term, seq.ptr, seq.len);
  seq.len = 0;
  uint16_t x = 0;
  ghostty_terminal_get(term, GHOSTTY_TERMINAL_DATA_CURSOR_X, &x);
  if (x > 0) append(&seq, "\r\n");
  append(&seq, "\x1b[J");
  if (seq.ptr != NULL) ghostty_terminal_vt_write(term, seq.ptr, seq.len);
  free(seq.ptr);
  ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_SELECTION, NULL);
  GhosttyTerminalScrollViewport bottom = {.tag = GHOSTTY_TERMINAL_SCROLL_VIEWPORT_BOTTOM};
  ghostty_terminal_scroll_viewport(term, bottom);
}

// Makes an TuxportVt around a terminal, new or restored (freed with it on failure), on a grid of
// cols × rows cells, and sets what libghostty-vt asks the app through and the options a snapshot
// does not carry
static TuxportVt *wrap(GhosttyTerminal term, uint16_t cols, uint16_t rows, uint32_t cell_width,
                      uint32_t cell_height) {
  TuxportVt *vt = calloc(1, sizeof(*vt));
  if (vt == NULL) {
    ghostty_terminal_free(term);
    return NULL;
  }
  vt->term = term;
  vt->cols = cols;
  vt->height = rows;
  vt->cell_width = cell_width;
  vt->cell_height = cell_height;
  if (ghostty_render_state_new(NULL, &vt->state) != GHOSTTY_SUCCESS ||
      ghostty_render_state_row_iterator_new(NULL, &vt->rows) != GHOSTTY_SUCCESS ||
      ghostty_render_state_row_cells_new(NULL, &vt->cells) != GHOSTTY_SUCCESS ||
      ghostty_key_encoder_new(NULL, &vt->encoder) != GHOSTTY_SUCCESS ||
      ghostty_key_event_new(NULL, &vt->event) != GHOSTTY_SUCCESS ||
      ghostty_mouse_encoder_new(NULL, &vt->mouse) != GHOSTTY_SUCCESS ||
      ghostty_mouse_event_new(NULL, &vt->mouse_event) != GHOSTTY_SUCCESS ||
      ghostty_render_state_image_iterator_new(NULL, &vt->images) != GHOSTTY_SUCCESS ||
      ghostty_selection_gesture_new(NULL, &vt->gesture) != GHOSTTY_SUCCESS ||
      ghostty_selection_gesture_event_new(NULL, &vt->gesture_press,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_PRESS) !=
          GHOSTTY_SUCCESS ||
      ghostty_selection_gesture_event_new(NULL, &vt->gesture_drag,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_DRAG) !=
          GHOSTTY_SUCCESS ||
      ghostty_selection_gesture_event_new(NULL, &vt->mouse_press,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_PRESS) !=
          GHOSTTY_SUCCESS ||
      ghostty_selection_gesture_event_new(NULL, &vt->mouse_release,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_RELEASE) !=
          GHOSTTY_SUCCESS) {
    tuxport_vt_free(vt);
    return NULL;
  }
  pthread_once(&sys_once, install_sys);
  // The row after the viewport too, which the view needs to draw the grid shifted by part of a row
  GhosttyRenderStateOverscan overscan = {.above = 0, .below = 1};
  ghostty_render_state_set(vt->state, GHOSTTY_RENDER_STATE_OPT_OVERSCAN, &overscan);
  // Cell sizes are only known to libghostty-vt through resize (size reports, images)
  ghostty_terminal_resize(vt->term, cols, rows, cell_width, cell_height);
  vt->mouse_x = -1;
  vt->mouse_y = -1;
  vt->progress_state = -1;

  // The value itself is the userdata (since Ghostty 1.4; read through *(void **)value before)
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_USERDATA, vt);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_WRITE_PTY, (const void *)on_write_pty);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SIZE, (const void *)on_size);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_COLOR_SCHEME, (const void *)on_color_scheme);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES,
                       (const void *)on_device_attributes);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_XTVERSION, (const void *)on_xtversion);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_RENDER_HOLD, (const void *)on_render_hold);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_TITLE_CHANGED,
                       (const void *)on_title_changed);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_BELL, (const void *)on_bell);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
                       (const void *)on_clipboard_write);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION,
                       (const void *)on_notification);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT, (const void *)on_progress);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS,
                       (const void *)on_program_status);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT,
                       (const void *)on_semantic_prompt);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_RESET, (const void *)on_reset);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_UNKNOWN_SEQUENCE,
                       (const void *)on_unknown_sequence);
  size_t unknown = UNKNOWN_MAX;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_UNKNOWN_MAX_BYTES, &unknown);
  GhosttyString terminfo = {.ptr = (const uint8_t *)TERMINFO_NAME,
                            .len = sizeof(TERMINFO_NAME) - 1};
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_TERMINFO_NAME, &terminfo);
  size_t lines = SCROLLBACK_LINES;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES, &lines);
  size_t bytes = SCROLLBACK_BYTES;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES, &bytes);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &DEFAULT_FOREGROUND);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, &DEFAULT_BACKGROUND);
  size_t kitty_clipboard = KITTY_CLIPBOARD_MAX;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE_MAX_BYTES, &kitty_clipboard);
  // Grapheme clustering (mode 2027) by default, as in Ghostty (grapheme-width-method = unicode):
  // emoji sequences (ZWJ, skin tones, flags) take one or two cells instead of one per codepoint.
  // Programs that count widths per codepoint (wcwidth) then disagree on such sequences; they can
  // turn the mode off
  GhosttyTerminalModeConfig graphemes = {.mode = GHOSTTY_MODE_GRAPHEME_CLUSTER, .value = true};
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_MODE_DEFAULT, &graphemes);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_MODE, &graphemes);
  // A blinking block by default, as in Ghostty without cursor-style-blink (libghostty-vt's default
  // is steady), until the app sets the user's (tuxport_vt_set_cursor): DECSCUSR's default
  // (CSI 0 q), DECSTR and RIS bring the blinking back, and programs can still stop it with DECSCUSR
  // or mode 12. Applied at once unless a program chose a style
  bool blink = true;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_BLINK, &blink);
  // Kitty graphics, drawn by the view from the frames' placements; a full reset keeps the limit
  // and the media (the alternate screen takes the primary's)
  uint64_t kitty_limit = KITTY_STORAGE;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT, &kitty_limit);
  // Images sent in the escape sequences only (libghostty-vt's default, set so that it stays so):
  // the file, temporary file and shared memory media name files and objects of the app's host,
  // not of the guest whose program sends them. Programs then fall back to sending the data
  bool host_media = false;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_FILE, &host_media);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_SHARED_MEM, &host_media);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_TEMP_FILE, NULL);
  // Registered glyphs are never drawn (the C API gives no access to them), so the Glyph Protocol
  // is not advertised (libghostty-vt's default since 1.0, set so that it stays so): programs would
  // send glyphs that never appear instead of falling back to text
  bool glyphs = false;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_GLYPH_PROTOCOL, &glyphs);
  // A snapshot can then be taken while output stops inside a sequence (CONTINUATION_MAX)
  size_t continuation = CONTINUATION_MAX;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES, &continuation);
  return vt;
}

static void drop_anchor(TuxportVt *vt);
static void drop_output(TuxportVt *vt);

void tuxport_vt_free(TuxportVt *vt) {
  if (vt == NULL) return;
  drop_anchor(vt);
  drop_output(vt);
  // Before the terminal, so they release what they track there
  if (vt->search) ghostty_search_free(vt->search);
  if (vt->gesture) ghostty_selection_gesture_free(vt->gesture, vt->term);
  if (vt->gesture_press) ghostty_selection_gesture_event_free(vt->gesture_press);
  if (vt->gesture_drag) ghostty_selection_gesture_event_free(vt->gesture_drag);
  if (vt->mouse_press) ghostty_selection_gesture_event_free(vt->mouse_press);
  if (vt->mouse_release) ghostty_selection_gesture_event_free(vt->mouse_release);
  if (vt->images) ghostty_render_state_image_iterator_free(vt->images);
  free(vt->placed);
  free(vt->matches);
  free(vt->all_matches);
  free(vt->match_rows);
  free(vt->marks);
  free(vt->marks_sent);
  if (vt->mouse_event) ghostty_mouse_event_free(vt->mouse_event);
  if (vt->mouse) ghostty_mouse_encoder_free(vt->mouse);
  if (vt->event) ghostty_key_event_free(vt->event);
  if (vt->encoder) ghostty_key_encoder_free(vt->encoder);
  if (vt->cells) ghostty_render_state_row_cells_free(vt->cells);
  if (vt->rows) ghostty_render_state_row_iterator_free(vt->rows);
  if (vt->state) ghostty_render_state_free(vt->state);
  if (vt->term) ghostty_terminal_free(vt->term);
  free(vt->replies.ptr);
  free(vt->frame.ptr);
  free(vt->extras.ptr);
  free(vt->text.ptr);
  free(vt->clip.ptr);
  free(vt->note_title.ptr);
  free(vt->note_body.ptr);
  free(vt->statuses.ptr);
  free(vt->grapheme);
  free(vt);
}

// ---- Output from the console ----

// The terminal changed what a transcript or a snapshot of it holds
static void changed(TuxportVt *vt) {
  vt->transcribed = false;
  vt->snapshot = SNAPSHOT_STALE;
}

static void osc_ended(TuxportVt *vt);
static bool is_alternate(TuxportVt *vt);
bool tuxport_vt_at_prompt(TuxportVt *vt);

static void tmux_started(TuxportVt *vt);

// Feeds console output to the terminal. It stops at the ends of OSC strings to find clipboard reads
// (OSC 52 with "?" for data), which libghostty-vt answers only from a callback that must answer at
// once and so without asking the user, as Ghostty's clipboard-read = ask does: they are counted
// for the app (tuxport_vt_take_effects), which asks and answers itself, as Ghostty's own app does.
// It also stops at the starts and ends of DCS strings to find tmux's control mode (TMUX_CONTROL).
//
// For a shell whose prompt asked for it (PROMPT_LF_OPTION: busybox ash, which runs nothing before
// a command and so marks no output start, and breaks a command longer than the screen's width
// into rows itself), it stops at each line feed from that prompt on, outside OSC and DCS strings:
// one that follows two carriage returns is ash breaking the line (it writes CR LF, and the
// console's ONLCR adds a CR), so the input goes on in the next row, which libghostty-vt marks as
// the prompt's continuation; any other at the prompt ends the input, and the output's start
// (OSC 133;C) is fed after it, where it also takes that mark off the new row. Bytes another
// process group than the console's foreground one wrote (`background`: a job the shell left in
// the background writing while the shell waits at its prompt, which the kernel tells apart) are
// not read so: they end no input. At the prompt they are marked as output themselves, between an
// output start before them and INPUT_RESUMED after them, so the cursor stays at the prompt, what
// is typed after them is still input, and they are selected with the output around them.
void tuxport_vt_feed(TuxportVt *vt, const uint8_t *data, size_t len, bool background) {
  uint32_t want = GHOSTTY_TERMINAL_BOUNDARY_OSC_START | GHOSTTY_TERMINAL_BOUNDARY_OSC_END |
                  GHOSTTY_TERMINAL_BOUNDARY_DCS_START | GHOSTTY_TERMINAL_BOUNDARY_DCS_END;
  size_t off = 0;
  // Strings cut in two by another writer are left alone
  bool aside = background && vt->lf_armed && len > 0 && !vt->osc_open && !vt->dcs_open &&
               tuxport_vt_at_prompt(vt);
  if (aside) {
    vt->lf_marking = true;
    ghostty_terminal_vt_write(vt->term, (const uint8_t *)OUTPUT_START, sizeof(OUTPUT_START) - 1);
    vt->lf_marking = false;
  }
  while (off < len) {
    size_t take = len - off;
    if (vt->lf_armed && !background && !vt->osc_open && !vt->dcs_open) {
      const uint8_t *lf = memchr(data + off, '\n', take);
      if (lf == data + off) {
        size_t returns = 0;
        size_t i = off;
        for (; i > 0 && data[i - 1] == '\r'; i--) returns++;
        if (i == 0) returns += vt->lf_returns;
        bool ends = returns < 2;
        bool input = ends && tuxport_vt_at_prompt(vt);
        ghostty_terminal_vt_write(vt->term, data + off, 1);
        if (input) {
          vt->lf_marking = true;
          ghostty_terminal_vt_write(vt->term, (const uint8_t *)OUTPUT_START,
                                    sizeof(OUTPUT_START) - 1);
          vt->lf_marking = false;
        }
        // Away from the prompt there is no input to end: the next prompt asks again
        if (ends) vt->lf_armed = false;
        off++;
        continue;
      }
      if (lf != NULL) take = (size_t)(lf - (data + off));
    }
    GhosttyTerminalWriteStop stop = GHOSTTY_INIT_SIZED(GhosttyTerminalWriteStop);
    GhosttyResult r = ghostty_terminal_vt_write_until(vt->term, data + off, take, want, &stop);
    if (r != GHOSTTY_SUCCESS && r != GHOSTTY_NO_VALUE) {
      ghostty_terminal_vt_write(vt->term, data + off, len - off);
      vt->osc_open = false;
      vt->dcs_open = false;
      vt->tmux_control = false;
      break;
    }
    uint32_t reached = r == GHOSTTY_SUCCESS ? stop.reached : 0;
    bool ended = reached & GHOSTTY_TERMINAL_BOUNDARY_OSC_END;
    bool dcs_ended = reached & GHOSTTY_TERMINAL_BOUNDARY_DCS_END;
    size_t n = r == GHOSTTY_SUCCESS ? stop.consumed : take;
    if (vt->osc_open) {
      // The string's content: what was consumed but the byte that ended it
      size_t content = ended ? n - 1 : n;
      if (vt->osc_len < OSC_HEAD) {
        size_t keep = OSC_HEAD - vt->osc_len < content ? OSC_HEAD - vt->osc_len : content;
        memcpy(vt->osc_head + vt->osc_len, data + off, keep);
      }
      vt->osc_len += content;
    }
    if (vt->dcs_open) {
      // Its header (parameters, intermediates, final byte) comes first
      size_t content = dcs_ended ? n - 1 : n;
      size_t had = vt->dcs_len;
      if (had < DCS_HEAD) {
        size_t keep = DCS_HEAD - had < content ? DCS_HEAD - had : content;
        memcpy(vt->dcs_head + had, data + off, keep);
      }
      vt->dcs_len += content;
      if (had < DCS_HEAD && vt->dcs_len >= DCS_HEAD &&
          memcmp(vt->dcs_head, TMUX_CONTROL, DCS_HEAD) == 0) {
        tmux_started(vt);
      }
    }
    off += n;
    if (ended) {
      // A cancelled string (CAN, SUB) does not run
      if (vt->osc_open && stop.end != GHOSTTY_TERMINAL_STRING_END_CANCELLED) osc_ended(vt);
      vt->osc_open = false;
    }
    if (dcs_ended) {
      // tmux ends it with ST once detached (libghostty-vt reports ST_ESC at its ESC; past the
      // header, 0x9C is payload); a string cancelled (CAN, SUB) or cut short ended otherwise, as
      // the OSC strings above do not run then
      if (vt->tmux_control && (stop.end == GHOSTTY_TERMINAL_STRING_END_ST_ESC ||
                               stop.end == GHOSTTY_TERMINAL_STRING_END_ST_C1)) {
        vt->tmux_detached = true;
      }
      vt->tmux_control = false;
      vt->dcs_open = false;
    }
    if (reached & GHOSTTY_TERMINAL_BOUNDARY_OSC_START) {
      vt->osc_open = true;
      vt->osc_len = 0;
    }
    if (reached & GHOSTTY_TERMINAL_BOUNDARY_DCS_START) {
      vt->dcs_open = true;
      vt->dcs_len = 0;
    }
  }
  if (aside && !vt->osc_open && !vt->dcs_open) {
    vt->lf_marking = true;
    ghostty_terminal_vt_write(vt->term, (const uint8_t *)INPUT_RESUMED, sizeof(INPUT_RESUMED) - 1);
    vt->lf_marking = false;
  }
  // Another writer's bytes do not come between the shell's carriage returns and its line feed
  if (!background) {
    size_t returns = 0;
    while (returns < len && returns < 2 && data[len - 1 - returns] == '\r') returns++;
    if (returns == len && returns + vt->lf_returns >= 2) returns = 2;
    vt->lf_returns = (uint8_t)returns;
  }
  // A command's start for a shell that does not mark it (on_semantic_prompt): the input ended
  bool at_prompt = tuxport_vt_at_prompt(vt);
  if (vt->was_at_prompt && !at_prompt && vt->command_start == 0) vt->command_start = now_ns();
  vt->was_at_prompt = at_prompt;
  if (len > 0 && vt->resize_pending) vt->resize_output_at = now_ns();
  changed(vt);
}

// tmux started its control mode, which the terminal cannot show: an empty line detaches it at
// once, as the user's Enter would, leaving its session running (tmux attach brings it back in the
// tab), and the string's end tells the app to say so (tmux_detached). The line goes out with the
// replies the pump writes before reading again
static void tmux_started(TuxportVt *vt) {
  vt->tmux_control = true;
  if (!buf_reserve(&vt->replies, 1)) return;
  vt->replies.ptr[vt->replies.len++] = '\r';
}

static void prompt_started(TuxportVt *vt, const uint8_t *options, size_t len);
static void prompt_line_feeds(TuxportVt *vt, const uint8_t *options, size_t len, bool primary);

// An OSC string ended: a prompt's start (OSC 133;A) on the primary screen tells how a tap moves
// the shell's cursor, and it or a prompt (OSC 133;P) whether the shell's line feeds are read
// (PROMPT_LF_OPTION); a clipboard read (OSC 52 ; clipboards ; ?) is counted, with the clipboard it
// names first ('c' for the standard one, also when it names none; 'p' primary, 's' selection), which
// the answer names as Ghostty's does
static void osc_ended(TuxportVt *vt) {
  const uint8_t *s = vt->osc_head;
  size_t n = vt->osc_len;
  if (n > OSC_HEAD) return;
  if (n >= 5 && (memcmp(s, "133;A", 5) == 0 || memcmp(s, "133;P", 5) == 0) &&
      (n == 5 || s[5] == ';')) {
    if (is_alternate(vt)) return;
    prompt_line_feeds(vt, s + 5, n - 5, s[4] == 'A');
    if (s[4] == 'A') prompt_started(vt, s + 5, n - 5);
    return;
  }
  if (n < 5 || memcmp(s, "52;", 3) != 0) return;
  size_t i = 3;
  for (; i < n && s[i] != ';'; i++) {
    bool clipboard = s[i] == 'c' || s[i] == 'p' || s[i] == 'q' || s[i] == 's' ||
                     (s[i] >= '0' && s[i] <= '7');
    if (!clipboard) return;
  }
  if (i + 2 != n || s[i] != ';' || s[i + 1] != '?') return;
  uint8_t first = i > 3 ? s[3] : 'c';
  vt->clipboard_read_kind = first == 'p' || first == 's' ? first : 'c';
  vt->clipboard_reads++;
}

// The value of option key among an OSC 133 command's options (';'-separated key=value pairs), as
// libghostty-vt reads them: the first pair with that key; NULL when there is none
static const uint8_t *prompt_option(const uint8_t *s, size_t n, const char *key, size_t *len) {
  size_t k = strlen(key);
  for (size_t i = 0; i < n;) {
    size_t end = i;
    while (end < n && s[end] != ';') end++;
    if (end - i > k && s[i + k] == '=' && memcmp(s + i, key, k) == 0) {
      *len = end - i - k - 1;
      return s + i + k + 1;
    }
    i = end + 1;
  }
  return NULL;
}

// A prompt started (OSC 133;A, with what follows the A): its options say how a tap moves the
// cursor, read as libghostty-vt reads them: Kitty's click_events first, then cl, a value it does not
// know counting as none; with neither, the way the last prompt declared stays
static void prompt_started(TuxportVt *vt, const uint8_t *options, size_t len) {
  size_t n = 0;
  const uint8_t *v = prompt_option(options, len, "click_events", &n);
  if (v != NULL && n == 1 && (v[0] == '1' || v[0] == '2')) {
    vt->prompt_click = v[0] == '1' ? PROMPT_CLICK_ABSOLUTE : PROMPT_CLICK_RELATIVE;
    return;
  }
  v = prompt_option(options, len, "cl", &n);
  if (v != NULL && ((n == 4 && memcmp(v, "line", 4) == 0) ||
                    (n == 1 && (v[0] == 'm' || v[0] == 'v' || v[0] == 'w')))) {
    vt->prompt_click = PROMPT_CLICK_ARROWS;
  }
}

// A prompt started (OSC 133;A, primary) or one was marked (OSC 133;P, as ash's secondary prompt
// is): with PROMPT_LF_OPTION its line feeds are read from here on (tuxport_vt_feed); a prompt start
// without it is another shell's, whose line feeds are not
static void prompt_line_feeds(TuxportVt *vt, const uint8_t *options, size_t len, bool primary) {
  size_t n = 0;
  const uint8_t *v = prompt_option(options, len, PROMPT_LF_OPTION, &n);
  bool asked = v != NULL && n == sizeof(PROMPT_LF_OUTPUT) - 1 && memcmp(v, PROMPT_LF_OUTPUT, n) == 0;
  if (!asked && !primary) return;
  vt->lf_output = asked;
  vt->lf_armed = asked;
}

// The answers collected since the last call; valid until the next feed
const uint8_t *tuxport_vt_take_replies(TuxportVt *vt, size_t *len) {
  *len = vt->replies.len;
  vt->replies.len = 0;
  return vt->replies.ptr;
}

// Whether the console reads lines without echoing them (ICANON on, ECHO off, which the kernel
// tells), as programs read passwords: frames then give the cursor as a lock, as Ghostty draws it at
// password prompts (the C API cannot set libghostty-vt's own flag for it)
void tuxport_vt_set_password_input(TuxportVt *vt, bool on) { vt->password_input = on; }

static void search_reselect(TuxportVt *vt, size_t index);

// Resizes the terminal. A search keeps its selected match: libghostty-vt drops it on a resize,
// which here comes with every keyboard shown or hidden
void tuxport_vt_resize(TuxportVt *vt, uint16_t cols, uint16_t rows, uint32_t cell_width,
                      uint32_t cell_height) {
  if (cols != vt->cols || rows != vt->height) {
    vt->resize_pending = true;
    vt->resize_output_at = 0;
  }
  vt->cols = cols;
  vt->height = rows;
  vt->cell_width = cell_width;
  vt->cell_height = cell_height;
  size_t selected = 0;
  bool reselect = vt->search != NULL &&
                  ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_SELECTED_INDEX, &selected) ==
                      GHOSTTY_SUCCESS;
  ghostty_terminal_resize(vt->term, cols, rows, cell_width, cell_height);
  changed(vt);
  if (reselect) search_reselect(vt, selected);
}

// Selects the match index places from the newest again, after a resize: the search has to catch
// up first (a resize restarts it, the matches are the same text), then steps from the newest
// without scrolling, but for the last step, which brings the match into view if it is not
static void search_reselect(TuxportVt *vt, size_t index) {
  if (ghostty_search_run(vt->search) != GHOSTTY_SUCCESS) return;
  GhosttySearchScroll quiet = GHOSTTY_SEARCH_SCROLL_NONE;
  ghostty_search_set(vt->search, GHOSTTY_SEARCH_OPT_SELECT_SCROLL, &quiet);
  for (size_t i = 0; i < index; i++) {
    if (ghostty_search_select_next(vt->search) != GHOSTTY_SUCCESS) break;
  }
  ghostty_search_set(vt->search, GHOSTTY_SEARCH_OPT_SELECT_SCROLL, NULL);
  ghostty_search_select_next(vt->search);
}

// Scrolls the viewport by delta rows (negative: back into the scrollback); 0 goes to the bottom
void tuxport_vt_scroll(TuxportVt *vt, int32_t delta) {
  GhosttyTerminalScrollViewport s = {0};
  if (delta == 0) {
    s.tag = GHOSTTY_TERMINAL_SCROLL_VIEWPORT_BOTTOM;
  } else {
    s.tag = GHOSTTY_TERMINAL_SCROLL_VIEWPORT_DELTA;
    s.value.delta = delta;
  }
  ghostty_terminal_scroll_viewport(vt->term, s);
}

// ---- Shell integration ----
//
// The shells' prompt marks (OSC 133, sent for bash, busybox ash and zsh by /etc/profile.d/tuxport.sh
// and the files it has them read) tell libghostty-vt which rows are prompts and which cells are a
// prompt, the input typed at it or a command's output. Without them there are no prompts: the
// cursor is never at one, there is nothing to scroll to, and no output to select.

// Whether the cursor is at the shell's prompt or in the input typed at it, on the primary screen:
// no command runs in the foreground, as far as the marks tell (closing the tab then ends nothing
// the user is waiting for). False without the marks.
bool tuxport_vt_at_prompt(TuxportVt *vt) {
  bool at = false;
  return ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_CURSOR_AT_PROMPT, &at) ==
             GHOSTTY_SUCCESS &&
         at;
}

// Scrolls the viewport to the prompt delta prompts above (negative) or below its top, or as far as
// there are prompts; nothing when there are none
void tuxport_vt_jump_prompt(TuxportVt *vt, int32_t delta) {
  GhosttyTerminalScrollViewport s = {0};
  s.tag = GHOSTTY_TERMINAL_SCROLL_VIEWPORT_DELTA_PROMPT;
  s.value.delta_prompt = delta;
  // Looking for prompts reads the scrollback's pages, which the activity token does not count
  // when the viewport ends up where it was
  vt->compress_touched = true;
  ghostty_terminal_scroll_viewport(vt->term, s);
}

static bool mode_on(TuxportVt *vt, GhosttyMode mode) {
  GhosttyTerminalModeConfig m = {.mode = mode};
  return ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_MODE, &m) == GHOSTTY_SUCCESS &&
         m.value;
}

// ---- Frames ----

// Writes the row the iterator is on as row y of the frame; marks is the row's search marks (NULL
// for none)
static bool put_row(TuxportVt *vt, uint16_t y, const uint8_t *marks) {
  if (ghostty_render_state_row_get(vt->rows, GHOSTTY_RENDER_STATE_ROW_DATA_CELLS, &vt->cells) !=
      GHOSTTY_SUCCESS) {
    return false;
  }
  if (!buf_reserve(&vt->frame, ROW_HEADER_BYTES + (size_t)vt->cols * CELL_BYTES)) return false;
  size_t row_at = vt->frame.len;
  vt->frame.len += ROW_HEADER_BYTES;
  vt->extras.len = 0;
  uint16_t count = 0;
  while (count < vt->cols && ghostty_render_state_row_cells_next(vt->cells)) {
    uint32_t len = 0;
    ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
                                       &len);
    uint32_t codepoint = 0;
    uint16_t extra = 0;
    if (len > 0) {
      if (len > vt->grapheme_cap) {
        uint32_t *p = realloc(vt->grapheme, len * sizeof(uint32_t));
        if (p == NULL) return false;
        vt->grapheme = p;
        vt->grapheme_cap = len;
      }
      GhosttyCodepointBuffer graphemes = {.ptr = vt->grapheme, .cap = vt->grapheme_cap};
      if (ghostty_render_state_row_cells_get(vt->cells,
                                             GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                                             &graphemes) != GHOSTTY_SUCCESS) {
        graphemes.len = 0;
      }
      // Only what was written, which is all of the cell's codepoints: the buffer holds len
      if (graphemes.len < len) len = (uint32_t)graphemes.len;
    }
    if (len > 0) {
      codepoint = vt->grapheme[0];
      extra = (uint16_t)(len - 1 > 0xffff ? 0xffff : len - 1);
      if (extra > 0) {
        if (!buf_reserve(&vt->extras, (size_t)extra * 4)) return false;
        for (uint16_t i = 0; i < extra; i++) {
          put_u32(vt->extras.ptr + vt->extras.len, vt->grapheme[i + 1]);
          vt->extras.len += 4;
        }
      }
    }

    GhosttyCell raw = 0;
    ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW, &raw);
    GhosttyCellWide wide = GHOSTTY_CELL_WIDE_NARROW;
    ghostty_cell_get(raw, GHOSTTY_CELL_DATA_WIDE, &wide);
    uint16_t attrs = (uint16_t)(((uint32_t)wide & 0x3) << 12);
    if (marks != NULL) attrs |= (uint16_t)((uint32_t)marks[count] << MARK_SHIFT);

    // Colours resolve through the palette; no value means the default colour
    GhosttyColorRgb c;
    uint32_t fg = 0, bg = 0;
    if (ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR,
                                           &c) == GHOSTTY_SUCCESS) {
      bg = argb(c);
    }
    bool styled = false;
    ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_HAS_STYLING,
                                       &styled);
    if (styled) {
      if (ghostty_render_state_row_cells_get(vt->cells,
                                             GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR,
                                             &c) == GHOSTTY_SUCCESS) {
        fg = argb(c);
      }
      GhosttyStyle style = {0};
      ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                                         &style);
      if (style.flags & GHOSTTY_STYLE_FLAG_BOLD) attrs |= 1u << 0;
      if (style.flags & GHOSTTY_STYLE_FLAG_ITALIC) attrs |= 1u << 1;
      if (style.flags & GHOSTTY_STYLE_FLAG_FAINT) attrs |= 1u << 2;
      if (style.flags & GHOSTTY_STYLE_FLAG_BLINK) attrs |= 1u << 3;
      if (style.flags & GHOSTTY_STYLE_FLAG_INVERSE) attrs |= 1u << 4;
      if (style.flags & GHOSTTY_STYLE_FLAG_INVISIBLE) attrs |= 1u << 5;
      if (style.flags & GHOSTTY_STYLE_FLAG_STRIKETHROUGH) attrs |= 1u << 6;
      if (style.flags & GHOSTTY_STYLE_FLAG_OVERLINE) attrs |= 1u << 7;
      attrs |= (uint16_t)(((uint32_t)GHOSTTY_STYLE_UNDERLINE(style) & 0x7) << 8);
    }

    uint8_t *p = vt->frame.ptr + vt->frame.len;
    put_u32(p, codepoint);
    put_u32(p + 4, fg);
    put_u32(p + 8, bg);
    put_u16(p + 12, attrs);
    put_u16(p + 14, extra);
    vt->frame.len += CELL_BYTES;
    count++;
  }
  if (!buf_reserve(&vt->frame, vt->extras.len)) return false;
  memcpy(vt->frame.ptr + vt->frame.len, vt->extras.ptr, vt->extras.len);
  vt->frame.len += vt->extras.len;

  GhosttyRenderStateRowSelection sel = GHOSTTY_INIT_SIZED(GhosttyRenderStateRowSelection);
  bool selected = ghostty_render_state_row_get(vt->rows, GHOSTTY_RENDER_STATE_ROW_DATA_SELECTION,
                                               &sel) == GHOSTTY_SUCCESS;

  uint8_t *h = vt->frame.ptr + row_at;
  put_u16(h, y);
  put_u16(h + 2, count);
  put_u32(h + 4, (uint32_t)(vt->extras.len / 4));
  put_u16(h + 8, selected ? sel.start_x : NONE16);
  put_u16(h + 10, selected ? sel.end_x : NONE16);
  return true;
}

// Writes a grid reference's viewport coordinates, or 0xffff for both when it is not in the
// viewport
static void put_viewport(TuxportVt *vt, uint8_t *p, const GhosttyGridRef *ref) {
  GhosttyPointCoordinate c;
  if (ghostty_terminal_point_from_grid_ref(vt->term, ref, GHOSTTY_POINT_TAG_VIEWPORT, &c) ==
          GHOSTTY_SUCCESS &&
      c.y < NONE16) {
    put_u16(p, c.x);
    put_u16(p + 2, (uint16_t)c.y);
  } else {
    put_u16(p, NONE16);
    put_u16(p + 2, NONE16);
  }
}

static bool search_catch_up(TuxportVt *vt);
static bool mark_matches(TuxportVt *vt);
static uint32_t tick_animations(TuxportVt *vt);
static bool images_changed(TuxportVt *vt);

// Captures the screen and writes a frame with the rows that changed since the last frame (all of
// them when full is set); the result is valid until the next call. NULL when out of memory.
const uint8_t *tuxport_vt_frame(TuxportVt *vt, bool full, size_t *len) {
  *len = 0;
  if (vt->held && now_ns() - vt->held_since > HOLD_LIMIT_NS) {
    GhosttyTerminalModeConfig sync = {.mode = GHOSTTY_MODE_SYNCHRONIZED_OUTPUT, .value = false};
    ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_MODE, &sync);
    vt->held = false;
  }
  if (vt->resize_pending && vt->resize_output_at != 0 && !vt->held &&
      now_ns() - vt->resize_output_at >= RESIZE_QUIET_NS) {
    bool ground = false;
    if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_VT_GROUND, &ground) ==
            GHOSTTY_SUCCESS && ground) {
      vt->resize_pending = false;
    }
  }
  // The animation frames due now go into the update that follows
  uint32_t animation_ms = vt->held ? 0 : tick_animations(vt);
  if (!vt->held && ghostty_render_state_update(vt->state, vt->term) != GHOSTTY_SUCCESS) {
    return NULL;
  }
  // The view paints images into its rows, so every row is repainted when they change
  if (images_changed(vt)) full = true;
  bool search_pending = vt->search != NULL && !search_catch_up(vt);
  // Without memory for the marks, rows go without them
  bool marked = mark_matches(vt);
  uint32_t search_total = 0;
  uint32_t search_selected = 0;
  if (vt->search != NULL) {
    size_t total = 0;
    size_t index = 0;
    ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_TOTAL_MATCHES, &total);
    if (ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_SELECTED_INDEX, &index) ==
        GHOSTTY_SUCCESS) {
      search_selected = clamp_u32((uint64_t)index + 1);
    }
    search_total = clamp_u32(total);
  }

  GhosttyRenderStateDirty dirty = GHOSTTY_RENDER_STATE_DIRTY_FALSE;
  ghostty_render_state_get(vt->state, GHOSTTY_RENDER_STATE_DATA_DIRTY, &dirty);
  if (full) dirty = GHOSTTY_RENDER_STATE_DIRTY_FULL;
  GhosttyRenderStateColors colors = GHOSTTY_INIT_SIZED(GhosttyRenderStateColors);
  ghostty_render_state_get(vt->state, GHOSTTY_RENDER_STATE_DATA_COLORS, &colors);
  GhosttyRenderStateCursor cursor = GHOSTTY_INIT_SIZED(GhosttyRenderStateCursor);
  ghostty_render_state_get(vt->state, GHOSTTY_RENDER_STATE_DATA_CURSOR, &cursor);
  bool bottom = true;
  ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_VIEWPORT_ACTIVE, &bottom);
  GhosttyTerminalScrollbar scrollbar = {0};
  ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &scrollbar);

  vt->frame.len = 0;
  if (!buf_reserve(&vt->frame, HEADER_BYTES)) return NULL;
  uint8_t *h = vt->frame.ptr;
  memset(h, 0, HEADER_BYTES);
  put_u16(h, vt->cols);
  put_u16(h + 2, vt->height);
  h[4] = (uint8_t)dirty;
  if (vt->password_input) {
    // As Ghostty draws it: wherever the cursor is in the viewport, whatever the program asked
    h[5] = cursor.viewport_has_value;
    h[6] = CURSOR_LOCK;
    h[7] = false;
  } else {
    h[5] = cursor.visible && cursor.viewport_has_value;
    h[6] = (uint8_t)cursor.visual_style;
    h[7] = cursor.blinking;
  }
  if (cursor.viewport_has_value) {
    put_u16(h + 8, cursor.viewport_x);
    put_u16(h + 10, cursor.viewport_y);
  }
  put_u32(h + 12, argb(colors.foreground));
  put_u32(h + 16, argb(colors.background));
  put_u32(h + 20, colors.cursor_has_value ? argb(colors.cursor) : 0);
  h[26] = bottom;
  uint8_t flags = 0;
  bool mouse = false;
  ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING, &mouse);
  if (mouse) flags |= FLAG_MOUSE;
  GhosttyTerminalScreen screen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
  ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen);
  if (screen == GHOSTTY_TERMINAL_SCREEN_ALTERNATE) flags |= FLAG_ALT_SCREEN;
  if (mode_on(vt, GHOSTTY_MODE_MOUSE_ALTERNATE_SCROLL)) flags |= FLAG_ALT_SCROLL;
  if (mode_on(vt, GHOSTTY_MODE_FOCUS_EVENT)) flags |= FLAG_FOCUS_EVENTS;
  put_u16(h + 28, NONE16);
  put_u16(h + 30, NONE16);
  put_u16(h + 32, NONE16);
  put_u16(h + 34, NONE16);
  GhosttySelection sel = {0};
  GhosttySelection ordered = {0};
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SELECTION, &sel) == GHOSTTY_SUCCESS &&
      ghostty_terminal_selection_ordered(vt->term, &sel, GHOSTTY_SELECTION_ORDER_FORWARD,
                                         &ordered) == GHOSTTY_SUCCESS) {
    flags |= FLAG_SELECTION;
    if (ordered.rectangle) flags |= FLAG_RECTANGLE;
    put_viewport(vt, h + 28, &ordered.start);
    put_viewport(vt, h + 32, &ordered.end);
  }
  if (search_pending) flags |= FLAG_SEARCH_PENDING;
  if (vt->resize_pending || vt->held) flags |= FLAG_OUTPUT_PENDING;
  h[27] = flags;
  put_u32(h + 36, clamp_u32(scrollbar.total));
  put_u32(h + 40, clamp_u32(scrollbar.offset));
  put_u32(h + 44, clamp_u32(scrollbar.len));
  put_u32(h + 48, search_total);
  put_u32(h + 52, search_selected);
  size_t images = vt->placed_len < UINT16_MAX ? vt->placed_len : UINT16_MAX;
  put_u16(h + 56, (uint16_t)images);
  GhosttyRenderStateOverscan captured = {0};
  ghostty_render_state_get(vt->state, GHOSTTY_RENDER_STATE_DATA_OVERSCAN, &captured);
  h[58] = captured.below > 0;
  GhosttyMouseShape pointer = GHOSTTY_MOUSE_SHAPE_TEXT;
  ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_MOUSE_SHAPE, &pointer);
  h[59] = (uint8_t)pointer;
  put_u32(h + 60, animation_ms);
  vt->frame.len = HEADER_BYTES;

  if (!buf_reserve(&vt->frame, images * IMAGE_BYTES)) return NULL;
  for (size_t i = 0; i < images; i++) {
    const Placement *p = &vt->placed[i];
    uint8_t *o = vt->frame.ptr + vt->frame.len;
    memset(o, 0, IMAGE_BYTES);
    put_u32(o, p->id);
    o[4] = p->layer;
    put_u64(o + 8, p->generation);
    put_u32(o + 16, (uint32_t)p->x);
    put_u32(o + 20, (uint32_t)p->y);
    put_u32(o + 24, p->width);
    put_u32(o + 28, p->height);
    put_u32(o + 32, p->source_x);
    put_u32(o + 36, p->source_y);
    put_u32(o + 40, p->source_width);
    put_u32(o + 44, p->source_height);
    put_u32(o + 48, p->image_width);
    put_u32(o + 52, p->image_height);
    vt->frame.len += IMAGE_BYTES;
  }

  uint16_t count = 0;
  if (ghostty_render_state_get(vt->state, GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, &vt->rows) ==
      GHOSTTY_SUCCESS) {
    uint16_t y = 0;
    while (ghostty_render_state_row_iterator_next(vt->rows)) {
      const uint8_t *marks = marked && y < vt->height ? vt->marks + (size_t)y * vt->cols : NULL;
      bool changed = dirty == GHOSTTY_RENDER_STATE_DIRTY_FULL;
      if (!changed && dirty != GHOSTTY_RENDER_STATE_DIRTY_FALSE) {
        ghostty_render_state_row_get(vt->rows, GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY, &changed);
      }
      if (!changed && marks != NULL) {
        changed = memcmp(marks, vt->marks_sent + (size_t)y * vt->cols, vt->cols) != 0;
      }
      if (changed) {
        if (!put_row(vt, y, marks)) return NULL;
        count++;
      }
      y++;
    }
  }
  if (marked) memcpy(vt->marks_sent, vt->marks, vt->marks_cells);
  // put_row may have moved the buffer
  put_u16(vt->frame.ptr + 24, count);
  ghostty_render_state_clean(vt->state);
  *len = vt->frame.len;
  return vt->frame.ptr;
}

// Shows the frames of animated Kitty images that are due now (libghostty-vt keeps no timer of its
// own); the milliseconds until the next one is due, 0 when nothing plays. A program starting an
// animation writes to the terminal, which brings a frame that starts the ticking
static uint32_t tick_animations(TuxportVt *vt) {
  GhosttyKittyGraphics graphics = NULL;
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS, &graphics) !=
          GHOSTTY_SUCCESS ||
      graphics == NULL) {
    return 0;
  }
  uint64_t now = (uint64_t)now_ns() / 1000000;
  uint64_t next = 0;
  if (ghostty_kitty_graphics_animation_tick(graphics, now, &next) != GHOSTTY_SUCCESS) return 0;
  uint64_t delay = next > now ? next - now : 0;
  return clamp_u32(delay < ANIMATION_MIN_MS ? ANIMATION_MIN_MS : delay);
}

static int compare(int64_t a, int64_t b) {
  return a < b ? -1 : a > b;
}

// Drawing order, as Ghostty's: by layer, then z-index, then image; within an image by placement,
// then top to bottom, so that the rows of a virtual placement follow each other
static int placement_order(const void *a, const void *b) {
  const Placement *p = a;
  const Placement *q = b;
  int c = compare(p->layer, q->layer);
  if (c == 0) c = compare(p->z, q->z);
  if (c == 0) c = compare(p->id, q->id);
  if (c == 0) c = compare(p->placement_id, q->placement_id);
  if (c == 0) c = compare(p->x, q->x);
  if (c == 0) c = compare(p->y, q->y);
  return c;
}

// Joins the rows of a virtual placement, one placement per row of placeholder cells, back into
// one rectangle wherever they meet: libghostty-vt gives each row's source in whole image pixels,
// so rows drawn on their own scale by different amounts and leave seams (an 8-pixel tall image
// over three rows gets the source rows 0-3, 3-6 and 5-8 for 2⅔ each), while one rectangle scales
// evenly. Rows meet when they are of the same placement, columns and source columns, one starts
// where the other ends on the screen, and their sources meet within the rounding. Returns the
// placements left.
static size_t join_rows(Placement *placed, size_t len) {
  size_t out = 0;
  for (size_t i = 0; i < len; i++) {
    const Placement *next = &placed[i];
    Placement *last = out > 0 ? &placed[out - 1] : NULL;
    if (last != NULL && last->kind == GHOSTTY_RENDER_STATE_KITTY_PLACEMENT_KIND_VIRTUAL &&
        next->kind == last->kind && next->layer == last->layer && next->z == last->z &&
        next->id == last->id && next->generation == last->generation &&
        next->placement_id == last->placement_id && next->x == last->x &&
        next->width == last->width && next->source_x == last->source_x &&
        next->source_width == last->source_width &&
        (int64_t)last->y + last->height == next->y) {
      int64_t end = (int64_t)last->source_y + last->source_height;
      if (next->source_y >= end - 1 && next->source_y <= end + 1) {
        last->height += next->height;
        last->source_height = next->source_y + next->source_height - last->source_y;
        continue;
      }
    }
    placed[out++] = *next;
  }
  return out;
}

// Reads the render state's images into vt->placed, in drawing order with virtual placements' rows
// joined, when its placements generation changed since the last read (they moved, came, went, or
// an image's pixels changed). Placements of another protocol than Kitty's, or of an image the
// storage no longer has (a held frame's), are left out; without memory the list ends early. True
// when placements changed while some were or are on the screen, so every row has to be painted
// again.
static bool images_changed(TuxportVt *vt) {
  uint64_t generation = 0;
  ghostty_render_state_get(vt->state, GHOSTTY_RENDER_STATE_DATA_IMAGE_PLACEMENTS_GENERATION,
                           &generation);
  if (generation == vt->images_generation) return false;
  vt->images_generation = generation;
  bool had = vt->placed_len > 0;
  vt->placed_len = 0;
  GhosttyKittyGraphics graphics = NULL;
  if (ghostty_render_state_get(vt->state, GHOSTTY_RENDER_STATE_DATA_IMAGE_ITERATOR,
                               &vt->images) != GHOSTTY_SUCCESS ||
      ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS, &graphics) !=
          GHOSTTY_SUCCESS ||
      graphics == NULL) {
    return had;
  }
  static const GhosttyRenderStateImageData keys[] = {
      GHOSTTY_RENDER_STATE_IMAGE_DATA_PROTOCOL,      GHOSTTY_RENDER_STATE_IMAGE_DATA_IMAGE_ID,
      GHOSTTY_RENDER_STATE_IMAGE_DATA_IMAGE_GENERATION, GHOSTTY_RENDER_STATE_IMAGE_DATA_LAYER,
      GHOSTTY_RENDER_STATE_IMAGE_DATA_Z,             GHOSTTY_RENDER_STATE_IMAGE_DATA_VIEWPORT_COL,
      GHOSTTY_RENDER_STATE_IMAGE_DATA_VIEWPORT_ROW,  GHOSTTY_RENDER_STATE_IMAGE_DATA_CELL_OFFSET_X,
      GHOSTTY_RENDER_STATE_IMAGE_DATA_CELL_OFFSET_Y, GHOSTTY_RENDER_STATE_IMAGE_DATA_PIXEL_WIDTH,
      GHOSTTY_RENDER_STATE_IMAGE_DATA_PIXEL_HEIGHT,  GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_X,
      GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_Y,      GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_WIDTH,
      GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_HEIGHT,
      // Kitty's own, which fail for another protocol's placements
      GHOSTTY_RENDER_STATE_IMAGE_DATA_KITTY_PLACEMENT_ID, GHOSTTY_RENDER_STATE_IMAGE_DATA_KITTY_KIND,
  };
  while (ghostty_render_state_image_iterator_next(vt->images)) {
    GhosttyImageProtocol protocol = GHOSTTY_IMAGE_PROTOCOL_KITTY;
    GhosttyImageLayer layer = GHOSTTY_IMAGE_LAYER_ABOVE_TEXT;
    GhosttyRenderStateKittyPlacementKind kind = GHOSTTY_RENDER_STATE_KITTY_PLACEMENT_KIND_FIXED;
    Placement p = {0};
    int32_t col = 0;
    int32_t row = 0;
    uint32_t offset_x = 0;
    uint32_t offset_y = 0;
    void *values[] = {
        &protocol,      &p.id,           &p.generation, &layer,    &p.z,
        &col,           &row,            &offset_x,     &offset_y, &p.width,
        &p.height,      &p.source_x,     &p.source_y,   &p.source_width,
        &p.source_height, &p.placement_id, &kind,
    };
    if (ghostty_render_state_image_get_multi(vt->images, sizeof(keys) / sizeof(keys[0]), keys,
                                             values, NULL) != GHOSTTY_SUCCESS ||
        protocol != GHOSTTY_IMAGE_PROTOCOL_KITTY) {
      continue;
    }
    p.kind = (uint8_t)kind;
    GhosttyKittyGraphicsImage image = ghostty_kitty_graphics_image(graphics, p.id);
    if (image == NULL ||
        ghostty_kitty_graphics_image_get(image, GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_WIDTH,
                                         &p.image_width) != GHOSTTY_SUCCESS ||
        ghostty_kitty_graphics_image_get(image, GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_HEIGHT,
                                         &p.image_height) != GHOSTTY_SUCCESS ||
        p.image_width == 0 || p.image_height == 0 || p.width == 0 || p.height == 0) {
      continue;
    }
    p.layer = (uint8_t)layer;
    p.x = clamp_i32((int64_t)col * vt->cell_width + offset_x);
    p.y = clamp_i32((int64_t)row * vt->cell_height + offset_y);
    if (vt->placed_len == vt->placed_cap) {
      size_t cap = vt->placed_cap ? vt->placed_cap * 2 : 16;
      Placement *grown = realloc(vt->placed, cap * sizeof(*grown));
      if (grown == NULL) break;
      vt->placed = grown;
      vt->placed_cap = cap;
    }
    vt->placed[vt->placed_len++] = p;
  }
  qsort(vt->placed, vt->placed_len, sizeof(*vt->placed), placement_order);
  vt->placed_len = join_rows(vt->placed, vt->placed_len);
  return had || vt->placed_len > 0;
}

// Writes the pixels of Kitty image id into out (width × height RGBA with premultiplied alpha, rows
// stride bytes apart, as an Android bitmap holds them). False when the image is gone, its pixels
// have not all arrived, or it is not of this generation and size any more: the frame that named
// it is stale, and the next one names its new generation.
bool tuxport_vt_image(TuxportVt *vt, uint32_t id, uint64_t generation, uint8_t *out, uint32_t width,
                     uint32_t height, size_t stride) {
  GhosttyKittyGraphics graphics = NULL;
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS, &graphics) !=
          GHOSTTY_SUCCESS ||
      graphics == NULL) {
    return false;
  }
  GhosttyKittyGraphicsImage image = ghostty_kitty_graphics_image(graphics, id);
  uint64_t current = 0;
  uint32_t w = 0;
  uint32_t h = 0;
  GhosttyKittyImageFormat format = GHOSTTY_KITTY_IMAGE_FORMAT_RGBA;
  // GHOSTTY_NO_VALUE until all the image's pixels have arrived
  GhosttyString data = {0};
  if (image == NULL ||
      ghostty_kitty_graphics_image_get(image, GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_GENERATION,
                                       &current) != GHOSTTY_SUCCESS ||
      current != generation ||
      ghostty_kitty_graphics_image_get(image, GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_WIDTH, &w) !=
          GHOSTTY_SUCCESS ||
      ghostty_kitty_graphics_image_get(image, GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_HEIGHT, &h) !=
          GHOSTTY_SUCCESS ||
      w != width || h != height || stride < (size_t)width * 4 ||
      ghostty_kitty_graphics_image_get(image, GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_FORMAT,
                                       &format) != GHOSTTY_SUCCESS ||
      ghostty_kitty_graphics_image_get(image, GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_PIXELS, &data) !=
          GHOSTTY_SUCCESS ||
      data.ptr == NULL) {
    return false;
  }
  const uint8_t *pixels = data.ptr;
  size_t len = data.len;
  size_t bpp;
  switch (format) {
    case GHOSTTY_KITTY_IMAGE_FORMAT_RGB:
      bpp = 3;
      break;
    case GHOSTTY_KITTY_IMAGE_FORMAT_RGBA:
      bpp = 4;
      break;
    case GHOSTTY_KITTY_IMAGE_FORMAT_GRAY_ALPHA:
      bpp = 2;
      break;
    case GHOSTTY_KITTY_IMAGE_FORMAT_GRAY:
      bpp = 1;
      break;
    default:
      return false;
  }
  if (len < (size_t)width * height * bpp) return false;
  for (uint32_t y = 0; y < height; y++) {
    const uint8_t *s = pixels + (size_t)y * width * bpp;
    uint8_t *d = out + (size_t)y * stride;
    for (uint32_t x = 0; x < width; x++, s += bpp, d += 4) {
      uint8_t r, g, b, a;
      switch (bpp) {
        case 3:
          r = s[0], g = s[1], b = s[2], a = 0xff;
          break;
        case 4:
          r = s[0], g = s[1], b = s[2], a = s[3];
          break;
        case 2:
          r = g = b = s[0], a = s[1];
          break;
        default:
          r = g = b = s[0], a = 0xff;
          break;
      }
      if (a != 0xff) {
        r = (uint8_t)((r * a + 127) / 255);
        g = (uint8_t)((g * a + 127) / 255);
        b = (uint8_t)((b * a + 127) / 255);
      }
      d[0] = r;
      d[1] = g;
      d[2] = b;
      d[3] = a;
    }
  }
  return true;
}

// Catches the search up with the terminal: feeds it what changed and works through what it has,
// for at most SEARCH_BUDGET_NS. True once it is caught up, false when it needs more frames.
static bool search_catch_up(TuxportVt *vt) {
  int64_t start = now_ns();
  for (;;) {
    if (ghostty_search_feed(vt->search) != GHOSTTY_SUCCESS) return true;
    GhosttySearchStatus status = GHOSTTY_SEARCH_STATUS_COMPLETE;
    do {
      if (ghostty_search_tick(vt->search, &status) != GHOSTTY_SUCCESS) return true;
    } while (status == GHOSTTY_SEARCH_STATUS_RUNNING && now_ns() - start < SEARCH_BUDGET_NS);
    if (status == GHOSTTY_SEARCH_STATUS_COMPLETE) return true;
    // Running out of time, or waiting for the next part of the scrollback
    if (now_ns() - start >= SEARCH_BUDGET_NS) return false;
  }
}

static bool viewport_ref(TuxportVt *vt, uint16_t x, uint16_t y, GhosttyGridRef *out);
static bool screen_point(TuxportVt *vt, const GhosttyGridRef *ref, GhosttyPointCoordinate *out);

// Marks the viewport cells match covers with mark (over any mark there before); top is the
// viewport's first row in screen coordinates. Screen coordinates, unlike viewport ones, exist for a
// match that begins above the viewport or ends below it, so it is cut to the rows shown.
static void mark_match(TuxportVt *vt, const GhosttySelection *match, uint32_t top, uint8_t mark) {
  GhosttySelection ordered = {0};
  GhosttyPointCoordinate start, end;
  if (ghostty_terminal_selection_ordered(vt->term, match, GHOSTTY_SELECTION_ORDER_FORWARD,
                                         &ordered) != GHOSTTY_SUCCESS ||
      !screen_point(vt, &ordered.start, &start) || !screen_point(vt, &ordered.end, &end) ||
      end.y < top || start.y >= top + vt->height) {
    return;
  }
  for (uint32_t y = start.y < top ? top : start.y; y <= end.y && y < top + vt->height; y++) {
    uint16_t from = y == start.y ? start.x : 0;
    uint16_t to = y == end.y ? end.x : vt->cols - 1;
    if (to >= vt->cols) to = vt->cols - 1;
    uint8_t *row = vt->marks + (size_t)(y - top) * vt->cols;
    for (uint16_t x = from; x <= to; x++) row[x] = mark;
  }
}

// Fills vt->marks for this frame: the search's matches in the viewport and its selected match,
// none without a search. False when the marks cannot be allocated.
static bool mark_matches(TuxportVt *vt) {
  size_t cells = (size_t)vt->cols * vt->height;
  if (cells != vt->marks_cells) {
    // A new grid: its first frame is a full one, and every row is sent
    free(vt->marks);
    free(vt->marks_sent);
    vt->marks = calloc(cells, 1);
    vt->marks_sent = calloc(cells, 1);
    if (vt->marks == NULL || vt->marks_sent == NULL) {
      free(vt->marks);
      free(vt->marks_sent);
      vt->marks = NULL;
      vt->marks_sent = NULL;
      vt->marks_cells = 0;
      return false;
    }
    vt->marks_cells = cells;
  }
  memset(vt->marks, MARK_NONE, cells);
  GhosttyGridRef first;
  GhosttyPointCoordinate top;
  if (vt->search == NULL || !viewport_ref(vt, 0, 0, &first) || !screen_point(vt, &first, &top)) {
    return true;
  }
  GhosttySelectionBuffer found = {.ptr = vt->matches, .cap = vt->matches_cap};
  GhosttyResult r = ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES, &found);
  if (r == GHOSTTY_OUT_OF_SPACE) {
    GhosttySelection *p = realloc(vt->matches, found.len * sizeof(*p));
    if (p == NULL) return true;
    vt->matches = p;
    vt->matches_cap = found.len;
    found = (GhosttySelectionBuffer){.ptr = vt->matches, .cap = vt->matches_cap};
    r = ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES, &found);
  }
  if (r == GHOSTTY_SUCCESS) {
    for (size_t i = 0; i < found.len; i++) mark_match(vt, &vt->matches[i], top.y, MARK_MATCH);
  }
  GhosttySelection selected = {0};
  if (ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_SELECTED_MATCH, &selected) ==
      GHOSTTY_SUCCESS) {
    mark_match(vt, &selected, top.y, MARK_SELECTED);
  }
  return true;
}

// ---- Input ----

// Encodes a key press into out; returns the length, or -1 when it does not fit (or fails). text
// is the character the key types without Ctrl and Alt (NULL for none), unshifted the key's
// character without Shift (0 for none). A key or action libghostty-vt does not know fails too:
// its setters then leave the event as it was, the previous key.
ptrdiff_t tuxport_vt_key(TuxportVt *vt, int key, uint16_t mods, uint16_t consumed, int action,
                        const uint8_t *text, size_t text_len, uint32_t unshifted, uint8_t *out,
                        size_t cap) {
  ghostty_key_encoder_set_from_terminal(vt->encoder, vt->term);
  if (ghostty_key_event_set_action(vt->event, (GhosttyKeyAction)action) != GHOSTTY_SUCCESS ||
      ghostty_key_event_set_key(vt->event, (GhosttyKey)key) != GHOSTTY_SUCCESS) {
    return -1;
  }
  ghostty_key_event_set_mods(vt->event, mods);
  ghostty_key_event_set_consumed_mods(vt->event, consumed);
  ghostty_key_event_set_utf8(vt->event, (GhosttyString){.ptr = text, .len = text_len});
  ghostty_key_event_set_unshifted_codepoint(vt->event, unshifted);
  size_t len = 0;
  if (ghostty_key_encoder_encode(vt->encoder, vt->event, out, cap, &len) != GHOSTTY_SUCCESS) {
    return -1;
  }
  return (ptrdiff_t)len;
}

// ---- Paste ----

typedef struct {
  const uint8_t *ptr;
  size_t len;
} PasteText;

static bool paste_read(void *userdata, GhosttyString mime, GhosttyWriter writer) {
  (void)mime;
  const PasteText *text = userdata;
  return writer.write(writer.userdata, text->ptr, text->len);
}

// Pastes text from the clipboard as the terminal's modes ask (bracketed with mode 2004, else
// newlines as carriage returns; unsafe control bytes become spaces); the bytes for the console
// are left in the replies. Returns 0 when pasted, 1 when refused because the text could run
// commands (a line break without bracketed paste, or the bracketed paste terminator) and
// allow_unsafe is false, -1 on failure. Nothing is written unless it returns 0.
int tuxport_vt_paste(TuxportVt *vt, const uint8_t *data, size_t len, bool allow_unsafe) {
  static const char plain[] = "text/plain";
  GhosttyString mime = {.ptr = (const uint8_t *)plain, .len = sizeof(plain) - 1};
  PasteText text = {.ptr = data, .len = len};
  GhosttyPaste paste = GHOSTTY_INIT_SIZED(GhosttyPaste);
  paste.location = GHOSTTY_CLIPBOARD_LOCATION_STANDARD;
  paste.source = GHOSTTY_PASTE_SOURCE_CLIPBOARD;
  paste.mimes = &mime;
  paste.mimes_len = 1;
  paste.reader = (GhosttyMimeReader){.read = paste_read, .userdata = &text};
  paste.allow_unsafe = allow_unsafe;
  GhosttyResult r = ghostty_terminal_paste(vt->term, &paste, NULL);
  if (r == GHOSTTY_REJECTED) return 1;
  return r == GHOSTTY_SUCCESS ? 0 : -1;
}

// ---- Mouse and focus ----

// Encodes a mouse event at pixel (x, y) of the grid for the program, in its tracking mode and
// format; returns the length, 0 when the program does not take it (no tracking, or a motion it
// did not ask for), -1 when it does not fit. action 0 press, 1 release, 2 motion; button 0 none
// (motion with no button held), 1 left, 2 right, 3 middle, 4 and 5 the wheel up and down, 6 and 7
// left and right.
ptrdiff_t tuxport_vt_mouse(TuxportVt *vt, int action, int button, uint16_t mods, float x, float y,
                          bool pressed, uint8_t *out, size_t cap) {
  // The cell as the encoder takes it (clamped to the grid); SGR-Pixels reports every pixel
  int cx = x < 0 ? 0 : (int)(x / (float)vt->cell_width);
  int cy = y < 0 ? 0 : (int)(y / (float)vt->cell_height);
  if (cx >= vt->cols) cx = vt->cols - 1;
  if (cy >= vt->height) cy = vt->height - 1;
  if (action == GHOSTTY_MOUSE_ACTION_MOTION && cx == vt->mouse_x && cy == vt->mouse_y &&
      !mode_on(vt, GHOSTTY_MODE_MOUSE_FORMAT_SGR_PIXELS)) {
    return 0;
  }
  ghostty_mouse_encoder_set_from_terminal(vt->mouse, vt->term);
  GhosttyMouseEncoderSize size = GHOSTTY_INIT_SIZED(GhosttyMouseEncoderSize);
  size.screen_width = (uint32_t)vt->cols * vt->cell_width;
  size.screen_height = (uint32_t)vt->height * vt->cell_height;
  size.cell_width = vt->cell_width;
  size.cell_height = vt->cell_height;
  ghostty_mouse_encoder_set(vt->mouse, GHOSTTY_MOUSE_ENCODER_OPT_SIZE, &size);
  ghostty_mouse_encoder_set(vt->mouse, GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED, &pressed);
  // An action or button libghostty-vt does not know leaves the event as it was
  if (ghostty_mouse_event_set_action(vt->mouse_event, (GhosttyMouseAction)action) !=
          GHOSTTY_SUCCESS ||
      (button == 0
           ? ghostty_mouse_event_clear_button(vt->mouse_event)
           : ghostty_mouse_event_set_button(vt->mouse_event, (GhosttyMouseButton)button)) !=
          GHOSTTY_SUCCESS) {
    return -1;
  }
  ghostty_mouse_event_set_mods(vt->mouse_event, mods);
  ghostty_mouse_event_set_position(vt->mouse_event, (GhosttyMousePosition){.x = x, .y = y});
  size_t len = 0;
  if (ghostty_mouse_encoder_encode(vt->mouse, vt->mouse_event, out, cap, &len) !=
      GHOSTTY_SUCCESS) {
    return -1;
  }
  if (len > 0) {
    vt->mouse_x = cx;
    vt->mouse_y = cy;
  }
  return (ptrdiff_t)len;
}

// Encodes focus gained or lost when the program asked for focus events (mode 1004); returns the
// length, 0 when it did not ask, -1 when it does not fit.
ptrdiff_t tuxport_vt_focus(TuxportVt *vt, bool gained, uint8_t *out, size_t cap) {
  if (!mode_on(vt, GHOSTTY_MODE_FOCUS_EVENT)) return 0;
  size_t len = 0;
  if (ghostty_focus_encode(gained ? GHOSTTY_FOCUS_GAINED : GHOSTTY_FOCUS_LOST, out, cap, &len) !=
      GHOSTTY_SUCCESS) {
    return -1;
  }
  return (ptrdiff_t)len;
}

// Copies text allocated by libghostty-vt into vt->text and frees it; NULL with len 0 when empty
// or out of memory
static const uint8_t *keep_text(TuxportVt *vt, uint8_t *data, size_t data_len, size_t *len) {
  vt->text.len = 0;
  bool ok = data_len > 0 && buf_reserve(&vt->text, data_len);
  if (ok) {
    memcpy(vt->text.ptr, data, data_len);
    vt->text.len = data_len;
  }
  ghostty_free(NULL, data, data_len);
  *len = vt->text.len;
  return ok ? vt->text.ptr : NULL;
}

// ---- Selection ----
//
// The selection itself is the terminal's (GHOSTTY_TERMINAL_OPT_SELECTION): it follows its cells
// through output and scrolling, and frames report it. Grid references from the API are only
// valid until the terminal changes, so every function takes fresh ones.

static bool viewport_ref(TuxportVt *vt, uint16_t x, uint16_t y, GhosttyGridRef *out) {
  if (x >= vt->cols) x = vt->cols - 1;
  if (y >= vt->height) y = vt->height - 1;
  GhosttyPoint p = {.tag = GHOSTTY_POINT_TAG_VIEWPORT, .value.coordinate = {.x = x, .y = y}};
  *out = (GhosttyGridRef){0};
  return ghostty_terminal_grid_ref(vt->term, p, out) == GHOSTTY_SUCCESS;
}

static void drop_anchor(TuxportVt *vt) {
  if (vt->anchor_start) ghostty_tracked_grid_ref_free(vt->anchor_start);
  if (vt->anchor_end) ghostty_tracked_grid_ref_free(vt->anchor_end);
  vt->anchor_start = NULL;
  vt->anchor_end = NULL;
}

// Tracks the cells at the screen coordinates (start, end) as the anchor
static bool set_anchor(TuxportVt *vt, GhosttyPointCoordinate start, GhosttyPointCoordinate end) {
  drop_anchor(vt);
  GhosttyPoint a = {.tag = GHOSTTY_POINT_TAG_SCREEN, .value.coordinate = start};
  GhosttyPoint b = {.tag = GHOSTTY_POINT_TAG_SCREEN, .value.coordinate = end};
  if (ghostty_terminal_grid_ref_track(vt->term, a, &vt->anchor_start) != GHOSTTY_SUCCESS ||
      ghostty_terminal_grid_ref_track(vt->term, b, &vt->anchor_end) != GHOSTTY_SUCCESS) {
    drop_anchor(vt);
    return false;
  }
  return true;
}

static bool screen_point(TuxportVt *vt, const GhosttyGridRef *ref, GhosttyPointCoordinate *out) {
  return ghostty_terminal_point_from_grid_ref(vt->term, ref, GHOSTTY_POINT_TAG_SCREEN, out) ==
         GHOSTTY_SUCCESS;
}

static bool set_selection(TuxportVt *vt, GhosttyGridRef start, GhosttyGridRef end, bool rectangle) {
  GhosttySelection sel = {0};
  sel.start = start;
  sel.end = end;
  sel.rectangle = rectangle;
  return ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, &sel) == GHOSTTY_SUCCESS;
}

// Installs a selection libghostty-vt derived (a command's output) and anchors later drags to it;
// false when it cannot be anchored.
static bool select_anchored(TuxportVt *vt, const GhosttySelection *derived) {
  GhosttySelection ordered = {0};
  GhosttyPointCoordinate start, end;
  if (ghostty_terminal_selection_ordered(vt->term, derived, GHOSTTY_SELECTION_ORDER_FORWARD,
                                         &ordered) != GHOSTTY_SUCCESS ||
      !screen_point(vt, &ordered.start, &start) || !screen_point(vt, &ordered.end, &end)) {
    return false;
  }
  // Tracking may change the terminal's page list, so it is selected from fresh references
  GhosttyGridRef a = {0};
  GhosttyGridRef b = {0};
  return set_anchor(vt, start, end) &&
         ghostty_tracked_grid_ref_snapshot(vt->anchor_start, &a) == GHOSTTY_SUCCESS &&
         ghostty_tracked_grid_ref_snapshot(vt->anchor_end, &b) == GHOSTTY_SUCCESS &&
         set_selection(vt, a, b, false);
}

// A press that selects a word (a long press, a double tap) or a line (a triple tap), and dragging
// on from it, go through libghostty-vt's selection gesture, Ghostty's own double- and triple-click
// selection: the press selects the word or the line under it, and the drag extends the selection
// by whole words or lines from the pressed cell (which the gesture tracks through output) to the
// one under the finger. The view counts the taps itself, since on a touch screen a press is not
// always a click (a finger drag scrolls), so each press is given to the gesture untimed, which
// makes it a first click, with the behaviour the view asks for as a first click's.

void tuxport_vt_select_clear(TuxportVt *vt);

// Starts a gesture at viewport cell (x, y), at pixel (px, py) from the grid's corner: selects the
// word (behavior GHOSTTY_SELECTION_GESTURE_BEHAVIOR_WORD) or the line (_LINE) there. A line is
// Ghostty's triple-click line: the whole soft-wrapped line with its leading and trailing blanks
// trimmed, stopping where a prompt mark (OSC 133) starts or ends a part of it, so a command is
// selected without its prompt. False when there is none there (blank cells), with any earlier
// selection cleared and the gesture ended.
bool tuxport_vt_gesture_press(TuxportVt *vt, int behavior, uint16_t x, uint16_t y, double px,
                             double py) {
  tuxport_vt_select_clear(vt);
  GhosttySelectionGestureBehavior b = (GhosttySelectionGestureBehavior)behavior;
  GhosttySelectionGestureBehaviors behaviors = {.single_click = b, .double_click = b,
                                                .triple_click = b};
  GhosttySurfacePosition at = {.x = px, .y = py};
  GhosttyGridRef ref;
  GhosttySelection sel = {0};
  if (viewport_ref(vt, x, y, &ref) &&
      ghostty_selection_gesture_event_set(vt->gesture_press, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                                          &ref) == GHOSTTY_SUCCESS &&
      ghostty_selection_gesture_event_set(vt->gesture_press,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                                          &at) == GHOSTTY_SUCCESS &&
      ghostty_selection_gesture_event_set(vt->gesture_press,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_OPT_BEHAVIORS,
                                          &behaviors) == GHOSTTY_SUCCESS &&
      ghostty_selection_gesture_event(vt->gesture, vt->term, vt->gesture_press, &sel) ==
          GHOSTTY_SUCCESS &&
      ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, &sel) == GHOSTTY_SUCCESS) {
    return true;
  }
  ghostty_selection_gesture_reset(vt->gesture, vt->term);
  return false;
}

// Applies a drag of the gesture to viewport cell (x, y), at pixel (px, py) from the grid's corner,
// making the selection a rectangle when asked (a cell drag's only); the selection it makes goes to
// sel, not yet installed. GHOSTTY_NO_VALUE when it makes none.
static GhosttyResult gesture_drag(TuxportVt *vt, uint16_t x, uint16_t y, double px, double py,
                                  bool rectangle, GhosttySelection *sel) {
  GhosttySelectionGestureGeometry geometry = {
      .columns = vt->cols,
      .cell_width = vt->cell_width,
      .padding_left = 0,
      .screen_height = (uint32_t)vt->height * vt->cell_height,
  };
  GhosttySurfacePosition at = {.x = px, .y = py};
  GhosttyGridRef ref;
  if (!viewport_ref(vt, x, y, &ref) ||
      ghostty_selection_gesture_event_set(vt->gesture_drag, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                                          &ref) != GHOSTTY_SUCCESS ||
      ghostty_selection_gesture_event_set(vt->gesture_drag,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                                          &at) != GHOSTTY_SUCCESS ||
      ghostty_selection_gesture_event_set(vt->gesture_drag,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY,
                                          &geometry) != GHOSTTY_SUCCESS ||
      ghostty_selection_gesture_event_set(vt->gesture_drag,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_OPT_RECTANGLE,
                                          &rectangle) != GHOSTTY_SUCCESS) {
    return GHOSTTY_INVALID_VALUE;
  }
  return ghostty_selection_gesture_event(vt->gesture, vt->term, vt->gesture_drag, sel);
}

// Drags the gesture on to viewport cell (x, y), at pixel (px, py) from the grid's corner: the
// selection runs by whole words or lines from the pressed cell's to the cell's (passing over
// blanks and punctuation, a word drag stays on the last word reached; a line drag takes in blank
// lines too). False, leaving the selection as it was, without a gesture, or when there is nothing
// to select towards the cell or the pressed cell has gone (its screen changed).
bool tuxport_vt_gesture_drag(TuxportVt *vt, uint16_t x, uint16_t y, double px, double py) {
  GhosttySelection sel = {0};
  return gesture_drag(vt, x, y, px, py, false, &sel) == GHOSTTY_SUCCESS &&
         ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, &sel) == GHOSTTY_SUCCESS;
}

// Ends the gesture (the finger lifted), keeping its selection and releasing the pressed cell's
// tracking.
void tuxport_vt_gesture_end(TuxportVt *vt) { ghostty_selection_gesture_reset(vt->gesture, vt->term); }

// A mouse's (or touchpad's) left button selects as Ghostty's does, through the same gesture, its
// presses timed so that libghostty-vt counts the clicks: a first click clears the selection and a
// drag from it selects by cells, a cell counting once the drag passes its point 60% of the way
// across (libghostty-vt's threshold; a rectangle while the view says so), a second click selects
// the word and a third the line (the command's output when asked), each dragging on by its unit. A
// release ends the drag but keeps the count, so the next press nearby within the interval is the
// next click; anything else that starts a gesture (a finger's) or clears the selection resets it.

// Presses the left button at viewport cell (x, y), at pixel (px, py) from the grid's corner, at
// time_ns on a monotonic clock, a press within interval_ns of the last and a cell's width of it
// counting as its next click. Installs the click's selection (none after a first click, which
// clears it; a second or third on blanks leaves the selection as it was) and returns the click
// count (1 to 3), or 0 when the press could not be applied (the gesture is then reset).
int tuxport_vt_mouse_press(TuxportVt *vt, uint16_t x, uint16_t y, double px, double py,
                          uint64_t time_ns, uint64_t interval_ns, bool output) {
  drop_anchor(vt);
  drop_output(vt);
  GhosttySelectionGestureBehaviors behaviors = {
      .single_click = GHOSTTY_SELECTION_GESTURE_BEHAVIOR_CELL,
      .double_click = GHOSTTY_SELECTION_GESTURE_BEHAVIOR_WORD,
      .triple_click = output ? GHOSTTY_SELECTION_GESTURE_BEHAVIOR_OUTPUT
                             : GHOSTTY_SELECTION_GESTURE_BEHAVIOR_LINE,
  };
  double distance = vt->cell_width;
  GhosttySurfacePosition at = {.x = px, .y = py};
  GhosttyGridRef ref;
  GhosttySelection sel = {0};
  uint8_t count = 0;
  GhosttyResult r = GHOSTTY_INVALID_VALUE;
  if (viewport_ref(vt, x, y, &ref) &&
      ghostty_selection_gesture_event_set(vt->mouse_press, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                                          &ref) == GHOSTTY_SUCCESS &&
      ghostty_selection_gesture_event_set(vt->mouse_press,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                                          &at) == GHOSTTY_SUCCESS &&
      ghostty_selection_gesture_event_set(vt->mouse_press,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_OPT_BEHAVIORS,
                                          &behaviors) == GHOSTTY_SUCCESS &&
      ghostty_selection_gesture_event_set(vt->mouse_press,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_OPT_TIME_NS,
                                          &time_ns) == GHOSTTY_SUCCESS &&
      ghostty_selection_gesture_event_set(vt->mouse_press,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_INTERVAL_NS,
                                          &interval_ns) == GHOSTTY_SUCCESS &&
      ghostty_selection_gesture_event_set(vt->mouse_press,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_DISTANCE,
                                          &distance) == GHOSTTY_SUCCESS) {
    r = ghostty_selection_gesture_event(vt->gesture, vt->term, vt->mouse_press, &sel);
  }
  if ((r != GHOSTTY_SUCCESS && r != GHOSTTY_NO_VALUE) ||
      ghostty_selection_gesture_get(vt->gesture, vt->term,
                                    GHOSTTY_SELECTION_GESTURE_DATA_CLICK_COUNT,
                                    &count) != GHOSTTY_SUCCESS ||
      count == 0) {
    tuxport_vt_select_clear(vt);
    return 0;
  }
  if (r == GHOSTTY_SUCCESS) {
    // An output's bounds are looked for beyond the viewport
    if (output && count == 3) vt->compress_touched = true;
    ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, &sel);
  } else if (count == 1) {
    ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, NULL);
  }
  return count;
}

// Drags the held left button on to viewport cell (x, y), at pixel (px, py) from the grid's corner
// (outside the grid, the edge cell under it): the selection becomes the one the press's unit makes
// from the pressed cell (a cell drag a rectangle when asked), or none (a cell drag not yet past
// its threshold), as Ghostty installs it. Returns 1 with a selection, 0 without, and -1, changing
// nothing, when no press is under way (none was made, the gesture was reset, or the pressed cell's
// screen has gone).
int tuxport_vt_mouse_drag(TuxportVt *vt, uint16_t x, uint16_t y, double px, double py,
                         bool rectangle) {
  uint8_t count = 0;
  GhosttyGridRef anchor = {0};
  if (ghostty_selection_gesture_get(vt->gesture, vt->term,
                                    GHOSTTY_SELECTION_GESTURE_DATA_CLICK_COUNT,
                                    &count) != GHOSTTY_SUCCESS ||
      count == 0 ||
      ghostty_selection_gesture_get(vt->gesture, vt->term, GHOSTTY_SELECTION_GESTURE_DATA_ANCHOR,
                                    &anchor) != GHOSTTY_SUCCESS) {
    return -1;
  }
  drop_anchor(vt);
  drop_output(vt);
  GhosttySelection sel = {0};
  GhosttyResult r = gesture_drag(vt, x, y, px, py, rectangle, &sel);
  if (r == GHOSTTY_SUCCESS &&
      ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, &sel) == GHOSTTY_SUCCESS) {
    return 1;
  }
  if (r == GHOSTTY_NO_VALUE) {
    ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, NULL);
    return 0;
  }
  return -1;
}

// Releases the left button at viewport cell (x, y), or outside the grid (x or y negative or past
// it, which counts as having moved away). Returns the click count, negated when the press moved
// from its cell or its drag selected (no click then), or 0 without a press under way.
int tuxport_vt_mouse_release(TuxportVt *vt, int x, int y) {
  GhosttyGridRef ref;
  bool inside = x >= 0 && y >= 0 && x < vt->cols && y < vt->height &&
                viewport_ref(vt, (uint16_t)x, (uint16_t)y, &ref);
  uint8_t count = 0;
  bool dragged = true;
  if (ghostty_selection_gesture_event_set(vt->mouse_release,
                                          GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                                          inside ? &ref : NULL) != GHOSTTY_SUCCESS ||
      ghostty_selection_gesture_event(vt->gesture, vt->term, vt->mouse_release, NULL) ==
          GHOSTTY_INVALID_VALUE ||
      ghostty_selection_gesture_get(vt->gesture, vt->term,
                                    GHOSTTY_SELECTION_GESTURE_DATA_CLICK_COUNT,
                                    &count) != GHOSTTY_SUCCESS ||
      ghostty_selection_gesture_get(vt->gesture, vt->term, GHOSTTY_SELECTION_GESTURE_DATA_DRAGGED,
                                    &dragged) != GHOSTTY_SUCCESS) {
    return 0;
  }
  return dragged ? -(int)count : (int)count;
}

// A right-click at viewport cell (x, y), as Ghostty's for its context menu: a selection holding
// the cell stays; otherwise the word there is selected (on a written space, the run of spaces),
// and on empty cells the selection stays as it was. True when there is a selection afterwards.
bool tuxport_vt_select_context(TuxportVt *vt, uint16_t x, uint16_t y) {
  GhosttySelection sel = {0};
  bool has = ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SELECTION, &sel) ==
             GHOSTTY_SUCCESS;
  if (x >= vt->cols) x = vt->cols - 1;
  if (y >= vt->height) y = vt->height - 1;
  GhosttyPoint p = {.tag = GHOSTTY_POINT_TAG_VIEWPORT, .value.coordinate = {.x = x, .y = y}};
  bool inside = false;
  if (has && ghostty_terminal_selection_contains(vt->term, &sel, p, &inside) == GHOSTTY_SUCCESS &&
      inside) {
    return true;
  }
  GhosttyTerminalSelectWordOptions word = GHOSTTY_INIT_SIZED(GhosttyTerminalSelectWordOptions);
  GhosttySelection selected = {0};
  if (!viewport_ref(vt, x, y, &word.ref) ||
      ghostty_terminal_select_word(vt->term, &word, &selected) != GHOSTTY_SUCCESS ||
      ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, &selected) !=
          GHOSTTY_SUCCESS) {
    return has;
  }
  // Only now: untracking may change the page list the references above point into
  drop_anchor(vt);
  drop_output(vt);
  ghostty_selection_gesture_reset(vt->gesture, vt->term);
  return true;
}

// Starts dragging a selection handle: the other end becomes the anchor. end is the handle of the
// selection's last cell, else its first. False when there is no selection.
bool tuxport_vt_select_grab(TuxportVt *vt, bool end) {
  GhosttySelection sel = {0};
  GhosttySelection ordered = {0};
  GhosttyPointCoordinate fixed;
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SELECTION, &sel) != GHOSTTY_SUCCESS ||
      ghostty_terminal_selection_ordered(vt->term, &sel, GHOSTTY_SELECTION_ORDER_FORWARD,
                                         &ordered) != GHOSTTY_SUCCESS ||
      !screen_point(vt, end ? &ordered.start : &ordered.end, &fixed)) {
    return false;
  }
  return set_anchor(vt, fixed, fixed);
}

static bool rectangular(TuxportVt *vt) {
  GhosttySelection sel = {0};
  return ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SELECTION, &sel) ==
             GHOSTTY_SUCCESS &&
         sel.rectangle;
}

// Extends the selection from the anchor to viewport cell (x, y): the anchor stays selected
// whole, and the selection runs from its far side to the cell; a rectangle stays one, with the
// anchor and the cell at opposite corners. False without an anchor.
bool tuxport_vt_select_to(TuxportVt *vt, uint16_t x, uint16_t y) {
  if (vt->anchor_start == NULL || vt->anchor_end == NULL) return false;
  GhosttyGridRef start = {0};
  GhosttyGridRef end = {0};
  GhosttyGridRef cell;
  bool rectangle = rectangular(vt);
  if (ghostty_tracked_grid_ref_snapshot(vt->anchor_start, &start) != GHOSTTY_SUCCESS ||
      ghostty_tracked_grid_ref_snapshot(vt->anchor_end, &end) != GHOSTTY_SUCCESS ||
      !viewport_ref(vt, x, y, &cell)) {
    return false;
  }
  GhosttySelection probe = {0};
  GhosttySelectionOrder order = GHOSTTY_SELECTION_ORDER_FORWARD;
  probe.start = start;
  probe.end = cell;
  ghostty_terminal_selection_order(vt->term, &probe, &order);
  if (order == GHOSTTY_SELECTION_ORDER_REVERSE) return set_selection(vt, end, cell, rectangle);
  probe.start = end;
  ghostty_terminal_selection_order(vt->term, &probe, &order);
  return set_selection(vt, start, order == GHOSTTY_SELECTION_ORDER_REVERSE ? end : cell,
                       rectangle);
}

// Turns the selection into a rectangle (on) with its first and last cells as opposite corners, or
// back into a linear selection from the rectangle's top left to its bottom right corner. False
// when there is no selection.
bool tuxport_vt_select_rectangle(TuxportVt *vt, bool on) {
  drop_anchor(vt);
  GhosttySelection sel = {0};
  GhosttySelection ordered = {0};
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SELECTION, &sel) != GHOSTTY_SUCCESS ||
      ghostty_terminal_selection_ordered(vt->term, &sel, GHOSTTY_SELECTION_ORDER_FORWARD,
                                         &ordered) != GHOSTTY_SUCCESS) {
    return false;
  }
  return set_selection(vt, ordered.start, ordered.end, on);
}

// Selects everything, scrollback included; false when there is nothing to select.
bool tuxport_vt_select_all(TuxportVt *vt) {
  drop_anchor(vt);
  // Finding the first and last non-blank cells reads the oldest and newest pages
  vt->compress_touched = true;
  GhosttySelection sel = {0};
  return ghostty_terminal_select_all(vt->term, &sel) == GHOSTTY_SUCCESS &&
         ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, &sel) == GHOSTTY_SUCCESS;
}

static void drop_output(TuxportVt *vt) {
  if (vt->output_at) ghostty_tracked_grid_ref_free(vt->output_at);
  vt->output_at = NULL;
}

// Whether viewport cell (x, y) is in a command's output, between the prompt marks of the command
// and of the next one (see Shell integration), or above the first prompt; if so, the cell is
// tracked for tuxport_vt_select_output, which the menu after a long press there offers.
bool tuxport_vt_output_at(TuxportVt *vt, uint16_t x, uint16_t y) {
  drop_output(vt);
  // Finding the output's ends reads the pages above and below the cell
  vt->compress_touched = true;
  GhosttyGridRef ref;
  GhosttySelection output = {0};
  GhosttyPointCoordinate cell;
  if (!viewport_ref(vt, x, y, &ref) ||
      ghostty_terminal_select_output(vt->term, &ref, &output) != GHOSTTY_SUCCESS ||
      !screen_point(vt, &ref, &cell)) {
    return false;
  }
  GhosttyPoint p = {.tag = GHOSTTY_POINT_TAG_SCREEN, .value.coordinate = cell};
  if (ghostty_terminal_grid_ref_track(vt->term, p, &vt->output_at) != GHOSTTY_SUCCESS) {
    vt->output_at = NULL;
    return false;
  }
  return true;
}

// Selects the whole output tuxport_vt_output_at found, as it is now (a command still running may
// have added to it), and anchors drags to it; false, leaving the selection alone, when its cell
// has left the scrollback or is no longer output (a full reset).
bool tuxport_vt_select_output(TuxportVt *vt) {
  if (vt->output_at == NULL) return false;
  vt->compress_touched = true;
  GhosttyGridRef ref = {0};
  GhosttySelection output = {0};
  bool selected =
      ghostty_tracked_grid_ref_snapshot(vt->output_at, &ref) == GHOSTTY_SUCCESS &&
      ghostty_terminal_select_output(vt->term, &ref, &output) == GHOSTTY_SUCCESS &&
      select_anchored(vt, &output);
  // Only now: untracking may change the page list the references above point into
  drop_output(vt);
  return selected;
}

void tuxport_vt_select_clear(TuxportVt *vt) {
  drop_anchor(vt);
  drop_output(vt);
  ghostty_selection_gesture_reset(vt->gesture, vt->term);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, NULL);
}

// Moves the selection's end as Ghostty's adjust_selection does (GhosttySelectionAdjust: to the
// next or previous non-blank cell, a row or a page up or down, the screen's start or end) and
// scrolls the viewport to show it, as Ghostty does; false without a selection.
bool tuxport_vt_select_adjust(TuxportVt *vt, int adjustment) {
  GhosttySelection sel = {0};
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SELECTION, &sel) != GHOSTTY_SUCCESS) {
    return false;
  }
  drop_anchor(vt);
  // Moving by pages and to the ends reads beyond the viewport
  vt->compress_touched = true;
  if (ghostty_terminal_selection_adjust(vt->term, &sel, (GhosttySelectionAdjust)adjustment) !=
          GHOSTTY_SUCCESS ||
      ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, &sel) != GHOSTTY_SUCCESS) {
    return false;
  }
  // From the selection as installed: references taken before are stale
  GhosttySelection now = {0};
  GhosttyPointCoordinate end;
  GhosttyTerminalScrollbar bar = {0};
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SELECTION, &now) != GHOSTTY_SUCCESS ||
      !screen_point(vt, &now.end, &end) ||
      ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &bar) != GHOSTTY_SUCCESS) {
    return true;
  }
  // Screen rows are the scroll bar's rows: the end at the viewport's top when above it, at its
  // bottom when below
  GhosttyTerminalScrollViewport s = {.tag = GHOSTTY_TERMINAL_SCROLL_VIEWPORT_ROW};
  if (end.y < bar.offset) {
    s.value.row = (size_t)end.y;
  } else if (end.y >= bar.offset + bar.len) {
    s.value.row = (size_t)(end.y - bar.len + 1);
  } else {
    return true;
  }
  ghostty_terminal_scroll_viewport(vt->term, s);
  return true;
}

// Formats the selection, or the active screen with its scrollback when sel is NULL, as plain text
// with soft-wrapped lines joined and trailing blanks trimmed, as Ghostty copies text: *data is
// allocated by libghostty-vt (ghostty_free). False on failure.
static bool format_plain(TuxportVt *vt, const GhosttySelection *sel, uint8_t **data, size_t *len) {
  GhosttyFormatter formatter = NULL;
  if (ghostty_formatter_terminal_new(NULL, &formatter, vt->term) != GHOSTTY_SUCCESS) return false;
  GhosttyFormatterFormat emit = GHOSTTY_FORMATTER_FORMAT_PLAIN;
  bool on = true;
  GhosttyResult r = ghostty_formatter_set(formatter, GHOSTTY_FORMATTER_OPT_EMIT, &emit);
  if (r == GHOSTTY_SUCCESS) r = ghostty_formatter_set(formatter, GHOSTTY_FORMATTER_OPT_UNWRAP, &on);
  if (r == GHOSTTY_SUCCESS) r = ghostty_formatter_set(formatter, GHOSTTY_FORMATTER_OPT_TRIM, &on);
  if (r == GHOSTTY_SUCCESS && sel != NULL) {
    r = ghostty_formatter_set(formatter, GHOSTTY_FORMATTER_OPT_SELECTION, sel);
  }
  if (r == GHOSTTY_SUCCESS) r = ghostty_formatter_format_alloc(formatter, NULL, data, len);
  ghostty_formatter_free(formatter);
  return r == GHOSTTY_SUCCESS;
}

// The selection as plain text, soft-wrapped lines joined and trailing blanks trimmed (as
// Ghostty copies); NULL with len 0 when there is no selection or it is empty. Valid until the
// next call.
const uint8_t *tuxport_vt_copy(TuxportVt *vt, size_t *len) {
  *len = 0;
  GhosttySelection sel = {0};
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SELECTION, &sel) != GHOSTTY_SUCCESS) {
    return NULL;
  }
  uint8_t *text = NULL;
  size_t text_len = 0;
  // The formatter reads every page the selection spans
  vt->compress_touched = true;
  if (!format_plain(vt, &sel, &text, &text_len)) return NULL;
  return keep_text(vt, text, text_len, len);
}

// ---- Links ----
//
// A tap opens the link under it: the cell's OSC 8 hyperlink, else an http or https URL written as
// text, found in the cell's line with its soft wraps joined (libghostty-vt leaves matching URLs in
// text to the embedder; Ghostty's own renderer does it by regex). The scheme an OSC 8 link may
// have is VtView's to check.

// Rows of a soft-wrapped line read above and below the tapped one, bounding a tap's work
#define LINK_ROWS 64

static bool screen_ref(TuxportVt *vt, uint16_t x, uint32_t y, GhosttyGridRef *out) {
  GhosttyPoint p = {.tag = GHOSTTY_POINT_TAG_SCREEN, .value.coordinate = {.x = x, .y = y}};
  *out = (GhosttyGridRef){0};
  return ghostty_terminal_grid_ref(vt->term, p, out) == GHOSTTY_SUCCESS;
}

// Whether screen row y is soft-wrapped onto the next one
static bool row_wraps(TuxportVt *vt, uint32_t y) {
  GhosttyGridRef ref;
  GhosttyRow row;
  bool wrap = false;
  return screen_ref(vt, 0, y, &ref) && ghostty_grid_ref_row(&ref, &row) == GHOSTTY_SUCCESS &&
         ghostty_row_get(row, GHOSTTY_ROW_DATA_WRAP, &wrap) == GHOSTTY_SUCCESS && wrap;
}

// The characters a URL in text is made of: ASCII letters, digits and the punctuation RFC 3986
// allows, less the quote that usually encloses one (as in Ghostty's URL regex). Text in other
// scripts ends a URL, so one followed by Chinese without a space stops before it.
static bool url_char(uint32_t c) {
  return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
         (c != 0 && c < 0x80 && strchr("-._~:/?#[]@!$&*+,;=%()", (int)c) != NULL);
}

// The length of "http://" or "https://" (any case) at cps[i], else 0
static size_t url_scheme(const uint32_t *cps, size_t count, size_t i) {
  static const char *const schemes[] = {"https://", "http://"};
  for (size_t k = 0; k < sizeof(schemes) / sizeof(schemes[0]); k++) {
    size_t n = strlen(schemes[k]);
    size_t j = 0;
    for (; j < n && i + j < count; j++) {
      uint32_t c = cps[i + j];
      if ((c >= 'A' && c <= 'Z' ? c + ('a' - 'A') : c) != (uint32_t)schemes[k][j]) break;
    }
    if (j == n) return n;
  }
  return 0;
}

// Finds the URL around cps[at] in a line of count characters: from the run's first scheme at or
// before at, over URL characters while brackets balance (an unmatched closing one ends it), less
// the punctuation that ends a sentence. Sets [*start, *end) and returns true when at is inside it.
static bool find_url(const uint32_t *cps, size_t count, size_t at, size_t *start, size_t *end) {
  if (at >= count || !url_char(cps[at])) return false;
  size_t s = at;
  while (s > 0 && url_char(cps[s - 1])) s--;
  size_t rest = 0;
  for (; s <= at; s++) {
    if ((rest = url_scheme(cps, count, s)) != 0) break;
  }
  if (rest == 0) return false;
  size_t e = s + rest;
  int depth = 0;
  for (; e < count && url_char(cps[e]); e++) {
    if (cps[e] == '(' || cps[e] == '[') {
      depth++;
    } else if (cps[e] == ')' || cps[e] == ']') {
      if (depth == 0) break;
      depth--;
    }
  }
  while (e > s + rest && strchr(".,;:!?", (int)cps[e - 1]) != NULL) e--;
  if (e == s + rest || at >= e) return false;
  *start = s;
  *end = e;
  return true;
}

// The link at viewport cell (x, y): its OSC 8 URI, else an http or https URL in the text there;
// NULL with len 0 when there is none. Valid until the next text call.
const uint8_t *tuxport_vt_link(TuxportVt *vt, uint16_t x, uint16_t y, size_t *len) {
  *len = 0;
  vt->text.len = 0;
  GhosttyGridRef ref;
  GhosttyPointCoordinate at;
  if (!viewport_ref(vt, x, y, &ref) || !screen_point(vt, &ref, &at)) return NULL;
  size_t n = 0;
  if (ghostty_grid_ref_hyperlink_uri(&ref, NULL, 0, &n) == GHOSTTY_OUT_OF_SPACE && n > 0 &&
      buf_reserve(&vt->text, n) &&
      ghostty_grid_ref_hyperlink_uri(&ref, vt->text.ptr, n, &n) == GHOSTTY_SUCCESS && n > 0) {
    vt->text.len = n;
    *len = n;
    return vt->text.ptr;
  }

  // The tapped row's line: up while the row above wraps onto it, down while it wraps onward. Rows
  // above the viewport may be on a compressed page, which reading them restores
  vt->compress_touched = true;
  uint32_t top = at.y;
  uint32_t bottom = at.y;
  while (top > 0 && at.y - top < LINK_ROWS && row_wraps(vt, top - 1)) top--;
  while (bottom - at.y < LINK_ROWS && row_wraps(vt, bottom)) bottom++;
  uint32_t *cps = malloc((size_t)(bottom - top + 1) * vt->cols * sizeof(*cps));
  if (cps == NULL) return NULL;
  // One character per cell, wide characters once (the tapped cell may be their second half);
  // the blank a wide character left at the end of a wrapped row is not part of the line
  size_t count = 0;
  size_t tapped = SIZE_MAX;
  for (uint32_t row = top; row <= bottom; row++) {
    for (uint16_t col = 0; col < vt->cols; col++) {
      GhosttyGridRef c;
      GhosttyCell cell;
      if (!screen_ref(vt, col, row, &c) || ghostty_grid_ref_cell(&c, &cell) != GHOSTTY_SUCCESS) {
        row = bottom;
        break;
      }
      bool here = row == at.y && col == at.x;
      GhosttyCellWide wide = GHOSTTY_CELL_WIDE_NARROW;
      ghostty_cell_get(cell, GHOSTTY_CELL_DATA_WIDE, &wide);
      if (wide == GHOSTTY_CELL_WIDE_SPACER_TAIL || wide == GHOSTTY_CELL_WIDE_SPACER_HEAD) {
        if (here && wide == GHOSTTY_CELL_WIDE_SPACER_TAIL && count > 0) tapped = count - 1;
        continue;
      }
      uint32_t cp = 0;
      ghostty_cell_get(cell, GHOSTTY_CELL_DATA_CODEPOINT, &cp);
      if (here) tapped = count;
      cps[count++] = cp;
    }
  }
  size_t start = 0;
  size_t end = 0;
  bool found = tapped != SIZE_MAX && find_url(cps, count, tapped, &start, &end) &&
               buf_reserve(&vt->text, end - start);
  if (found) {
    // URL characters are all ASCII
    for (size_t i = start; i < end; i++) vt->text.ptr[i - start] = (uint8_t)cps[i];
    vt->text.len = end - start;
    *len = vt->text.len;
  }
  free(cps);
  return found ? vt->text.ptr : NULL;
}

// ---- Taps on the prompt ----
//
// A tap on the input typed at the shell's prompt moves the shell's cursor there, as Ghostty's
// cursor-click-to-move does for a prompt that declares how (prompt_click): left or right arrows,
// one for each input cell between the cursor and the tapped cell within the cursor's line (its soft
// wraps joined), or a mouse press, for a shell that places the cursor itself. Only cells the prompt
// marks call input count, so the prompt and blank cells are passed over as the shell's cursor
// passes over them. Unlike Ghostty, which counts both cells of a wide character, a wide character
// takes one arrow, as line editors move by characters.

// Screen row y's prompt state (GHOSTTY_ROW_SEMANTIC_*) and whether it is a soft-wrapped row's
// continuation and wraps onto the next row
static bool prompt_row(TuxportVt *vt, uint32_t y, GhosttyRowSemanticPrompt *prompt, bool *continues,
                       bool *wraps) {
  GhosttyGridRef ref;
  GhosttyRow row;
  *prompt = GHOSTTY_ROW_SEMANTIC_NONE;
  *continues = false;
  *wraps = false;
  return screen_ref(vt, 0, y, &ref) && ghostty_grid_ref_row(&ref, &row) == GHOSTTY_SUCCESS &&
         ghostty_row_get(row, GHOSTTY_ROW_DATA_SEMANTIC_PROMPT, prompt) == GHOSTTY_SUCCESS &&
         ghostty_row_get(row, GHOSTTY_ROW_DATA_WRAP_CONTINUATION, continues) == GHOSTTY_SUCCESS &&
         ghostty_row_get(row, GHOSTTY_ROW_DATA_WRAP, wraps) == GHOSTTY_SUCCESS;
}

// Whether screen cell (x, y) is input typed at a prompt, and its width (*wide)
static bool input_cell(TuxportVt *vt, uint16_t x, uint32_t y, GhosttyCellWide *wide) {
  GhosttyGridRef ref;
  GhosttyCell cell;
  GhosttyCellSemanticContent content = GHOSTTY_CELL_SEMANTIC_OUTPUT;
  *wide = GHOSTTY_CELL_WIDE_NARROW;
  return screen_ref(vt, x, y, &ref) && ghostty_grid_ref_cell(&ref, &cell) == GHOSTTY_SUCCESS &&
         ghostty_cell_get(cell, GHOSTTY_CELL_DATA_WIDE, wide) == GHOSTTY_SUCCESS &&
         ghostty_cell_get(cell, GHOSTTY_CELL_DATA_SEMANTIC_CONTENT, &content) == GHOSTTY_SUCCESS &&
         content == GHOSTTY_CELL_SEMANTIC_INPUT;
}

// Whether the cursor stops at screen cell (x, y) when the shell moves it by one character: input,
// and not the second half of a wide character or the blank one leaves at a row's end
static bool input_stop(TuxportVt *vt, uint16_t x, uint32_t y) {
  GhosttyCellWide wide;
  return input_cell(vt, x, y, &wide) &&
         (wide == GHOSTTY_CELL_WIDE_NARROW || wide == GHOSTTY_CELL_WIDE_WIDE);
}

// Whether screen row y goes on the line of the row above it, which the shell broke there itself
// (lf_output: busybox ash, tuxport_vt_feed): a continuation row of the prompt that is no soft wrap
// and does not start with a secondary prompt
static bool shell_broken(TuxportVt *vt, uint32_t y) {
  GhosttyRowSemanticPrompt prompt;
  bool continues, wraps;
  GhosttyGridRef ref;
  GhosttyCell cell;
  GhosttyCellSemanticContent content = GHOSTTY_CELL_SEMANTIC_OUTPUT;
  return vt->lf_output && prompt_row(vt, y, &prompt, &continues, &wraps) &&
         prompt == GHOSTTY_ROW_SEMANTIC_PROMPT_CONTINUATION && !continues &&
         screen_ref(vt, 0, y, &ref) && ghostty_grid_ref_cell(&ref, &cell) == GHOSTTY_SUCCESS &&
         ghostty_cell_get(cell, GHOSTTY_CELL_DATA_SEMANTIC_CONTENT, &content) == GHOSTTY_SUCCESS &&
         content != GHOSTTY_CELL_SEMANTIC_PROMPT;
}

// The first row of the prompt the cursor's row is in, found as libghostty-vt finds prompts going
// up: the nearest prompt row at or above it, or the first of the continuation rows that lead up to
// it when no prompt row begins them (it left the scrollback). False when there is none within
// PROMPT_ROWS.
static bool prompt_top(TuxportVt *vt, uint32_t from, uint32_t *top) {
  bool found = false;
  for (uint32_t n = 0; n < PROMPT_ROWS && n <= from; n++) {
    uint32_t y = from - n;
    GhosttyRowSemanticPrompt prompt;
    bool continues, wraps;
    if (!prompt_row(vt, y, &prompt, &continues, &wraps)) break;
    if (prompt == GHOSTTY_ROW_SEMANTIC_PROMPT) {
      *top = y;
      return true;
    }
    if (prompt == GHOSTTY_ROW_SEMANTIC_PROMPT_CONTINUATION) {
      *top = y;
      found = true;
    } else if (found) {
      break;
    }
  }
  return found;
}

// Right arrows from the cursor to a cell after it (Ghostty's promptClickLine): the input after the
// cursor up to and including that cell, over the rows that soft-wrap on from the cursor's (or go on
// after the shell's own break, shell_broken) and continue its prompt; a cell past the line's input moves to the end of it (one more arrow when
// the cursor is on input, to the cell after the last).
static size_t moves_right(TuxportVt *vt, GhosttyPointCoordinate cursor,
                          GhosttyPointCoordinate click) {
  size_t count = 0;
  for (uint32_t y = cursor.y; y <= click.y; y++) {
    GhosttyRowSemanticPrompt prompt;
    bool continues, wraps;
    if (!prompt_row(vt, y, &prompt, &continues, &wraps)) break;
    if (y != cursor.y && prompt != GHOSTTY_ROW_SEMANTIC_PROMPT_CONTINUATION) break;
    for (uint32_t x = y == cursor.y ? (uint32_t)cursor.x + 1 : 0; x < vt->cols; x++) {
      if (!input_stop(vt, (uint16_t)x, y)) continue;
      count++;
      if (y == click.y && x == click.x) return count;
    }
    if (!wraps && !shell_broken(vt, y + 1)) {
      GhosttyCellWide wide;
      if (input_cell(vt, cursor.x, cursor.y, &wide)) count++;
      break;
    }
  }
  return count;
}

// Left arrows from the cursor to a cell before it: the input before the cursor down to and
// including that cell, over the rows of the cursor's line above its row; a cell before the line's
// input moves to the start of it.
static size_t moves_left(TuxportVt *vt, GhosttyPointCoordinate cursor,
                         GhosttyPointCoordinate click) {
  size_t count = 0;
  for (uint32_t y = cursor.y;; y--) {
    for (uint32_t x = y == cursor.y ? cursor.x : vt->cols; x-- > 0;) {
      if (!input_stop(vt, (uint16_t)x, y)) continue;
      count++;
      if (y == click.y && x == click.x) return count;
    }
    GhosttyRowSemanticPrompt prompt;
    bool continues, wraps;
    if (y == click.y || y == 0 || !prompt_row(vt, y, &prompt, &continues, &wraps) ||
        !(continues || shell_broken(vt, y))) {
      break;
    }
  }
  return count;
}

// A tap on viewport cell (x, y): when the shell's prompt declared a way to move its cursor, the
// cursor is at the prompt and the cell is not above the prompt's first row, the bytes that move the
// cursor there are left in the replies (none when it is already there or no input lies between)
// and it returns true; otherwise false, and the tap is no prompt's.
bool tuxport_vt_prompt_click(TuxportVt *vt, uint16_t x, uint16_t y) {
  if (vt->prompt_click == PROMPT_CLICK_NONE || !tuxport_vt_at_prompt(vt)) return false;
  if (x >= vt->cols) x = vt->cols - 1;
  if (y >= vt->height) y = vt->height - 1;
  uint16_t cx = 0;
  uint16_t cy = 0;
  GhosttyGridRef ref;
  GhosttyPointCoordinate cursor, click;
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_CURSOR_X, &cx) != GHOSTTY_SUCCESS ||
      ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_CURSOR_Y, &cy) != GHOSTTY_SUCCESS) {
    return false;
  }
  GhosttyPoint at = {.tag = GHOSTTY_POINT_TAG_ACTIVE, .value.coordinate = {.x = cx, .y = cy}};
  if (ghostty_terminal_grid_ref(vt->term, at, &ref) != GHOSTTY_SUCCESS ||
      !screen_point(vt, &ref, &cursor) || !viewport_ref(vt, x, y, &ref) ||
      !screen_point(vt, &ref, &click)) {
    return false;
  }
  // The rows above the cursor may be on a compressed page, which reading them restores
  vt->compress_touched = true;
  uint32_t top = 0;
  if (!prompt_top(vt, cursor.y, &top) || click.y < top) return false;

  if (vt->prompt_click != PROMPT_CLICK_ARROWS) {
    // A left button press (SGR), as Kitty and Ghostty send it, without a release
    uint32_t row = vt->prompt_click == PROMPT_CLICK_ABSOLUTE ? (uint32_t)y + 1 : click.y - top + 1;
    char press[48];
    snprintf(press, sizeof(press), "\x1b[<0;%u;%uM", (unsigned)x + 1, (unsigned)row);
    append(&vt->replies, press);
    return true;
  }
  // The second half of a wide character stands for the character
  GhosttyCellWide wide;
  input_cell(vt, click.x, click.y, &wide);
  if (wide == GHOSTTY_CELL_WIDE_SPACER_TAIL && click.x > 0) click.x--;
  if (click.x == cursor.x && click.y == cursor.y) return true;
  bool right = click.y > cursor.y || (click.y == cursor.y && click.x > cursor.x);
  size_t count = right ? moves_right(vt, cursor, click) : moves_left(vt, cursor, click);
  // The arrows the program's cursor key mode (DECCKM) asks for, as Ghostty sends them
  bool application = mode_on(vt, GHOSTTY_MODE_CURSOR_KEYS);
  const char *arrow = right ? (application ? "\x1bOC" : "\x1b[C")
                            : (application ? "\x1bOD" : "\x1b[D");
  if (count == 0 || !buf_reserve(&vt->replies, count * 3)) return true;
  for (size_t i = 0; i < count; i++) {
    memcpy(vt->replies.ptr + vt->replies.len, arrow, 3);
    vt->replies.len += 3;
  }
  return true;
}

// ---- Search ----
//
// One search per terminal, over the active screen and its scrollback (libghostty-vt keeps the
// primary screen's results while a program uses the alternate one). Frames drive it
// (search_catch_up) and carry its count and marks; the matches themselves stay in libghostty-vt,
// which keeps them on their text through output, resizes and pruned scrollback.

// Searches for needle (UTF-8; ASCII letters match either case, all else exactly), restarting the
// search when it changed; an empty needle leaves nothing found. The next frames find its matches,
// none of them selected. False when the search cannot be made.
bool tuxport_vt_search(TuxportVt *vt, const uint8_t *needle, size_t len) {
  if (vt->search == NULL) {
    if (len == 0) return true;
    if (ghostty_search_new(NULL, &vt->search, vt->term) != GHOSTTY_SUCCESS) {
      vt->search = NULL;
      return false;
    }
  }
  GhosttyString s = {.ptr = needle, .len = len};
  return ghostty_search_set(vt->search, GHOSTTY_SEARCH_OPT_NEEDLE, len > 0 ? &s : NULL) ==
         GHOSTTY_SUCCESS;
}

// Selects the next match towards older output (older; the first call selects the newest match)
// or towards newer output, going round at either end, and scrolls the viewport to it unless it is
// shown already. False when there is no match (or no search).
bool tuxport_vt_search_select(TuxportVt *vt, bool older) {
  if (vt->search == NULL) return false;
  GhosttyResult r =
      older ? ghostty_search_select_next(vt->search) : ghostty_search_select_prev(vt->search);
  return r == GHOSTTY_SUCCESS;
}

// The rows of the scroll bar (screen rows) the search's matches start on, for marks along it:
// each row once, newest first, as many as *len says, and in *selected the selected match's row
// (-1 for none). NULL with *len 0 without a search or a match. Valid until the next call or the
// search's end. Rows are counted per page, whose rows follow one another, so only a page's first
// row is looked up (counting the rows of the pages above it, which reads no compressed page),
// not each match's.
const uint32_t *tuxport_vt_search_rows(TuxportVt *vt, size_t *len, int64_t *selected) {
  *len = 0;
  *selected = -1;
  if (vt->search == NULL) return NULL;
  GhosttySelection chosen = {0};
  GhosttyPointCoordinate at;
  if (ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_SELECTED_MATCH, &chosen) ==
          GHOSTTY_SUCCESS &&
      screen_point(vt, &chosen.start, &at)) {
    *selected = at.y;
  }
  GhosttySelectionBuffer all = {.ptr = vt->all_matches, .cap = vt->all_matches_cap};
  GhosttyResult r = ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_MATCHES, &all);
  if (r == GHOSTTY_OUT_OF_SPACE) {
    GhosttySelection *p = realloc(vt->all_matches, all.len * sizeof(*p));
    if (p == NULL) return NULL;
    vt->all_matches = p;
    vt->all_matches_cap = all.len;
    all = (GhosttySelectionBuffer){.ptr = p, .cap = all.len};
    r = ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_MATCHES, &all);
  }
  if (r != GHOSTTY_SUCCESS || all.len == 0) return NULL;
  if (all.len > vt->match_rows_cap) {
    uint32_t *p = realloc(vt->match_rows, all.len * sizeof(*p));
    if (p == NULL) return NULL;
    vt->match_rows = p;
    vt->match_rows_cap = all.len;
  }
  void *node = NULL;
  bool found = false;
  uint32_t first = 0;
  size_t n = 0;
  for (size_t i = 0; i < all.len; i++) {
    GhosttyGridRef ref = all.ptr[i].start;
    if (ref.node != node) {
      node = ref.node;
      GhosttyGridRef top = ref;
      top.x = 0;
      top.y = 0;
      GhosttyPointCoordinate p;
      found = screen_point(vt, &top, &p);
      first = found ? p.y : 0;
    }
    if (!found) continue;
    uint32_t row = first + ref.y;
    // Newest first: the matches of a row come together
    if (n > 0 && vt->match_rows[n - 1] == row) continue;
    vt->match_rows[n++] = row;
  }
  *len = n;
  return n > 0 ? vt->match_rows : NULL;
}

// Ends the search; the next frame sends the rows its marks were on again.
void tuxport_vt_search_end(TuxportVt *vt) {
  if (vt->search) ghostty_search_free(vt->search);
  vt->search = NULL;
  free(vt->all_matches);
  free(vt->match_rows);
  vt->all_matches = NULL;
  vt->all_matches_cap = 0;
  vt->match_rows = NULL;
  vt->match_rows_cap = 0;
}

// ---- Clearing and resetting ----

static bool is_alternate(TuxportVt *vt);

// Writes the app's own sequences into the terminal as if the console had sent them, after a CAN
// when the parser is inside a sequence a program left unfinished (which would take them for its
// payload); they ask for no answer, so the console's replies stay as they were
static void write_own(TuxportVt *vt, const Buf *seq) {
  bool ground = true;
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_VT_GROUND, &ground) == GHOSTTY_SUCCESS &&
      !ground) {
    static const uint8_t can = 0x18;
    ghostty_terminal_vt_write(vt->term, &can, 1);
    vt->osc_open = false;
    vt->dcs_open = false;
    vt->tmux_control = false;
  }
  size_t replies = vt->replies.len;
  if (seq->ptr != NULL) ghostty_terminal_vt_write(vt->term, seq->ptr, seq->len);
  vt->replies.len = replies;
  changed(vt);
}

// Clears the screen and the scrollback as Ghostty's clear_screen does: at the shell's prompt the
// whole screen, for the shell to draw the prompt again (the caller sends it a form feed, ^L, as
// Ghostty does); elsewhere the rows above the cursor's, which moves to the top with what follows
// it, and the screen's images. Returns 1 when cleared at the prompt, 0 when cleared elsewhere, and
// -1 on the alternate screen, where nothing is done: a full-screen program would go on drawing on a
// screen it no longer knows.
int tuxport_vt_clear(TuxportVt *vt) {
  if (is_alternate(vt)) return -1;
  tuxport_vt_select_clear(vt);
  bool prompt = tuxport_vt_at_prompt(vt);
  Buf seq = {0};
  if (prompt) {
    // At a prompt, erasing the display (ED 2) moves the screen into the scrollback, as for a
    // shell's ^L, so the scrollback goes after it
    append(&seq, "\x1b[2J\x1b[3J");
  } else {
    uint16_t x = 0;
    uint16_t y = 0;
    ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_CURSOR_X, &x);
    ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_CURSOR_Y, &y);
    append(&seq, "\x1b[3J");
    if (y > 0) {
      // Deleting lines from the top moves the cursor's row up to it with the rows below
      char rows[64];
      snprintf(rows, sizeof(rows), "\x1b[H\x1b[%uM\x1b[1;%uH", (unsigned)y, (unsigned)x + 1);
      append(&seq, rows);
    }
    // Every placement on the screen, and the images no other one shows
    append(&seq, "\x1b_Ga=d,d=A,q=2\x1b\\");
  }
  write_own(vt, &seq);
  free(seq.ptr);
  tuxport_vt_scroll(vt, 0);
  return prompt ? 1 : 0;
}

// Resets the terminal as Ghostty's reset does (a full reset, RIS): both screens, the scrollback,
// modes, colours programs set (but OSC 10, 11 and 12's), the title and images; the app's own
// settings (default colours, limits, grapheme clustering) stay. Returns whether the shell was at
// its prompt, which the caller then sends a form feed (^L) to draw it again on the empty screen.
bool tuxport_vt_reset(TuxportVt *vt) {
  bool prompt = tuxport_vt_at_prompt(vt);
  tuxport_vt_select_clear(vt);
  ghostty_terminal_reset(vt->term);
  // A synchronized output hold ends with the modes, and the prompt's way of moving the cursor
  // with the screen's prompt state (on_reset hears only a program's reset)
  vt->held = false;
  vt->prompt_click = PROMPT_CLICK_NONE;
  vt->lf_output = false;
  vt->lf_armed = false;
  changed(vt);
  return prompt;
}

// ---- Title, effects, messages, transcript ----

// Whether a program set the title since the last call
bool tuxport_vt_take_title_changed(TuxportVt *vt) {
  bool changed = vt->title_changed;
  vt->title_changed = false;
  return changed;
}

// The bells, clipboard write, notification, progress and status reports, clipboard reads, the
// last command finished and a detached tmux control mode since the last call (vt.rs's `Effects`);
// the text stays valid until the next feed
void tuxport_vt_take_effects(TuxportVt *vt, TuxportVtEffects *out) {
  out->bells = vt->bells;
  out->clipboard = vt->clipboard;
  out->clip = vt->clip.ptr;
  out->clip_len = vt->clipboard == TUXPORT_CLIPBOARD_TEXT ? vt->clip.len : 0;
  out->notified = vt->notified;
  out->title = vt->note_title.ptr;
  out->title_len = vt->notified ? vt->note_title.len : 0;
  out->body = vt->note_body.ptr;
  out->body_len = vt->notified ? vt->note_body.len : 0;
  out->progress_state = vt->progress_state;
  out->progress = vt->progress;
  out->statuses = vt->statuses.ptr;
  out->statuses_len = vt->statuses.len;
  out->clipboard_reads = vt->clipboard_reads;
  out->clipboard_read_kind = vt->clipboard_read_kind;
  out->command_finished = vt->command_finished;
  out->command_has_exit = vt->command_has_exit;
  out->command_exit = vt->command_exit;
  out->command_ms = vt->command_ms;
  out->tmux_detached = vt->tmux_detached;
  vt->tmux_detached = false;
  vt->command_finished = false;
  vt->clipboard_reads = 0;
  vt->bells = 0;
  vt->clipboard = TUXPORT_CLIPBOARD_NONE;
  vt->notified = false;
  vt->progress_state = -1;
  vt->statuses.len = 0;
}

// The title set by the program (OSC 0/2), empty when none; valid until the next text call
const uint8_t *tuxport_vt_title(TuxportVt *vt, size_t *len) {
  *len = 0;
  GhosttyString title = {0};
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_TITLE, &title) != GHOSTTY_SUCCESS ||
      title.len == 0 || !buf_reserve(&vt->text, title.len)) {
    return NULL;
  }
  memcpy(vt->text.ptr, title.ptr, title.len);
  vt->text.len = title.len;
  *len = title.len;
  return vt->text.ptr;
}

static bool is_alternate(TuxportVt *vt) {
  GhosttyTerminalScreen screen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
  ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen);
  return screen == GHOSTTY_TERMINAL_SCREEN_ALTERNATE;
}

// Display width of UTF-8 text in cells (per codepoint; malformed bytes count as one cell each)
static size_t utf8_width(const uint8_t *s, size_t len) {
  size_t width = 0;
  for (size_t i = 0; i < len;) {
    uint32_t cp = s[i];
    size_t n = cp < 0x80 ? 1 : (cp >> 5) == 0x6 ? 2 : (cp >> 4) == 0xe ? 3 : (cp >> 3) == 0x1e ? 4 : 0;
    if (n == 0 || i + n > len) {
      width++;
      i++;
      continue;
    }
    if (n > 1) cp &= 0x7f >> n;
    for (size_t k = 1; k < n; k++) cp = (cp << 6) | (s[i + k] & 0x3f);
    width += ghostty_unicode_codepoint_width(cp);
    i += n;
  }
  return width;
}

// Shows a yellow line of app notice (a kill report) above the cursor's line: move down and back as
// many lines as it takes (scrolling at the bottom), save the cursor, insert the lines, write,
// restore the cursor and move it down. The prompt and a half-typed command move down with it and
// the shell's line editing is unaffected. False on the alternate screen (a full-screen program),
// where nothing is written.
bool tuxport_vt_message(TuxportVt *vt, const uint8_t *text, size_t len) {
  if (is_alternate(vt)) return false;
  size_t width = utf8_width(text, len);
  size_t rows = (width + vt->cols - 1) / vt->cols;
  if (rows > (size_t)vt->height - 1) rows = (size_t)vt->height - 1;
  if (rows < 1) rows = 1;
  char head[64], tail[32];
  int head_len = snprintf(head, sizeof(head), "\x1b[%zuA\x1b" "7\r\x1b[%zuL\x1b[33m", rows, rows);
  int tail_len = snprintf(tail, sizeof(tail), "\x1b[0m\x1b" "8\x1b[%zuB", rows);
  Buf seq = {0};
  bool ok = head_len > 0 && tail_len > 0 &&
            buf_reserve(&seq, rows + (size_t)head_len + len + (size_t)tail_len);
  if (ok) {
    memset(seq.ptr, '\n', rows);
    seq.len = rows;
    memcpy(seq.ptr + seq.len, head, (size_t)head_len);
    seq.len += (size_t)head_len;
    memcpy(seq.ptr + seq.len, text, len);
    seq.len += len;
    memcpy(seq.ptr + seq.len, tail, (size_t)tail_len);
    seq.len += (size_t)tail_len;
    // The program may have died inside a sequence (an OSC, DCS or APC string): the CAN before it
    // keeps the line feeds that make the report's room from being ignored or taken as payload
    write_own(vt, &seq);
  }
  free(seq.ptr);
  return ok;
}

// The primary screen's text with its scrollback, soft-wrapped lines joined and trailing blanks
// trimmed (saving tabs); valid until the next text call. NULL on the alternate screen, whose text
// is not what the tab should come back with, when the last transcript is still the screen's text
// (the caller keeps it: formatting restores every compressed page), and on failure.
const uint8_t *tuxport_vt_transcript(TuxportVt *vt, size_t *len) {
  *len = 0;
  if (is_alternate(vt) || vt->transcribed) return NULL;
  vt->compress_touched = true;
  uint8_t *data = NULL;
  size_t data_len = 0;
  if (!format_plain(vt, NULL, &data, &data_len)) return NULL;
  if (data_len == 0) {
    // An empty screen: an empty transcript, not a failure
    ghostty_free(NULL, data, 0);
    vt->transcribed = true;
    static const uint8_t empty = 0;
    return &empty;
  }
  const uint8_t *text = keep_text(vt, data, data_len, len);
  vt->transcribed = text != NULL;
  return text;
}

// A snapshot of the whole terminal for saving the tab, which tuxport_vt_restore brings back: 1 with
// its bytes in *out (freed with tuxport_vt_free_snapshot), 0 when nothing changed since the last
// one (the caller keeps that), -1 when there is none of the terminal as it is: libghostty-vt
// failed (output stopped inside a sequence longer than CONTINUATION_MAX), or the snapshot is
// larger than SNAPSHOT_MAX. Not taken again until the terminal changes.
int tuxport_vt_snapshot(TuxportVt *vt, uint8_t **out, size_t *len) {
  *out = NULL;
  *len = 0;
  if (vt->snapshot == SNAPSHOT_TAKEN) return 0;
  if (vt->snapshot == SNAPSHOT_NONE) return -1;
  // Encoding reads every page
  vt->compress_touched = true;
  uint8_t *data = NULL;
  size_t data_len = 0;
  vt->snapshot = SNAPSHOT_NONE;
  if (ghostty_snapshot_encode_alloc(vt->term, NULL, &data, &data_len) != GHOSTTY_SUCCESS) {
    return -1;
  }
  if (data_len > SNAPSHOT_MAX) {
    ghostty_free(NULL, data, data_len);
    return -1;
  }
  vt->snapshot = SNAPSHOT_TAKEN;
  *out = data;
  *len = data_len;
  return 1;
}

void tuxport_vt_free_snapshot(uint8_t *data, size_t len) {
  ghostty_free(NULL, data, len);
}

// libghostty-vt's version, which the tabs' saved snapshots are recorded with: another version may
// not read them
const uint8_t *tuxport_vt_version(size_t *len) {
  GhosttyString version = {0};
  if (ghostty_build_info(GHOSTTY_BUILD_INFO_VERSION_STRING, &version) != GHOSTTY_SUCCESS) {
    *len = 0;
    return NULL;
  }
  *len = version.len;
  return version.ptr;
}

// ---- Scrollback compression ----
//
// libghostty-vt compresses the primary screen's cold pages, those wholly in the scrollback and
// outside the viewport, to about a fifth of their 400 KiB, and restores a page whenever something
// reads it; scheduling is left to the embedder. As in Ghostty, vt.rs runs a pass in short steps
// once the terminal has been idle for a moment, so output does not wait for it. Unlike Ghostty, a
// pass waits for that only a few seconds: a tab whose output never pauses would otherwise never be
// compressed, and the transcripts saved every minute restore all its pages. The activity token
// changes with what can make work (rows moving into the scrollback, resizes, scrolling), but not
// when a read restores pages, so the reads that restore them in bulk (the formatter for
// transcripts and copies, select all, a link's line) mark the terminal touched. Drawing reads only
// the viewport's pages, and searching keeps pages as they are.

static uint64_t compression_activity(TuxportVt *vt) {
  uint64_t activity = 0;
  ghostty_terminal_compression_activity(vt->term, &activity);
  return activity;
}

// Frees the text buffer once a transcript or a large copy has grown it past TEXT_KEEP; whatever
// it held was handed out under an earlier lock and is no longer read
static void release_text(TuxportVt *vt) {
  if (vt->text.cap <= TEXT_KEEP) return;
  free(vt->text.ptr);
  vt->text = (Buf){0};
}

// Whether vt.rs should schedule a pass now: the terminal changed or a read restored pages since
// the last pass, and none is scheduled yet. Marks it scheduled; vt.rs then calls
// tuxport_vt_compress_step once the idle interval has passed.
bool tuxport_vt_compress_wanted(TuxportVt *vt) {
  if (vt->compress_scheduled) return false;
  uint64_t activity = compression_activity(vt);
  if (activity == vt->compress_seen && !vt->compress_touched) return false;
  vt->compress_seen = activity;
  vt->compress_scheduled = true;
  return true;
}

// One step of the scheduled pass (TUXPORT_COMPRESS_*): a bounded amount of work, unless the
// terminal changed since the last look and the pass may still wait (may_wait), which postpones it
// by another idle interval. The last step ends the pass and frees a large text buffer.
int tuxport_vt_compress_step(TuxportVt *vt, bool may_wait) {
  uint64_t activity = compression_activity(vt);
  if (activity != vt->compress_seen) {
    vt->compress_seen = activity;
    if (may_wait) return TUXPORT_COMPRESS_WAIT;
  }
  GhosttyTerminalCompressionResult result = GHOSTTY_TERMINAL_COMPRESSION_RESULT_COMPLETE;
  if (ghostty_terminal_compress(vt->term, GHOSTTY_TERMINAL_COMPRESSION_MODE_INCREMENTAL,
                                &result) == GHOSTTY_SUCCESS &&
      result == GHOSTTY_TERMINAL_COMPRESSION_RESULT_PENDING) {
    return TUXPORT_COMPRESS_STEP;
  }
  vt->compress_scheduled = false;
  vt->compress_touched = false;
  release_text(vt);
  return TUXPORT_COMPRESS_DONE;
}

// Compresses every cold page at once, idle or not (the app trimming its memory), and frees a large
// text buffer. A scheduled pass goes on as it was.
void tuxport_vt_compress_all(TuxportVt *vt) {
  GhosttyTerminalCompressionResult result;
  ghostty_terminal_compress(vt->term, GHOSTTY_TERMINAL_COMPRESSION_MODE_FULL, &result);
  if (!vt->compress_scheduled) {
    vt->compress_seen = compression_activity(vt);
    vt->compress_touched = false;
  }
  release_text(vt);
}

// Deletes Kitty images until each screen's take at most KITTY_TRIMMED (the app trimming its memory
// under pressure, for a tab not shown), then puts the limit back. Lowering the limit is
// libghostty-vt's only way to give image memory back: it deletes whole images with their
// placements, those no placement shows first, then the oldest sent, so images shown in the
// scrollback or on the screen can go. The last snapshot is left as it was (it still has them): a new
// one would read, and so decompress, every page of the scrollback; the next change takes one.
void tuxport_vt_drop_images(TuxportVt *vt) {
  uint64_t limit = KITTY_TRIMMED;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT, &limit);
  limit = KITTY_STORAGE;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT, &limit);
}
