# android/vtrender Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/vtrender.md` for the paired decision history (not yet created; pre-split decisions remain D1–D3 and the rendering part of D5 in `.agents/decisions/android/vtview.md`, and D5, D9, D16, D17, D32 and the frame/rendering parts of D7, D30, D36, D41 and D46–D48 in `.agents/decisions/android/terminal.md`).
>
> Verified: 2026-11-14 (split out of android/vtview, inherited from the android/terminal full mapping of 2026-11-11; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: output-pending flag covers unsettled or incomplete resize output and synchronized holds)

## Responsibilities

`android/vtrender` defines and paints a terminal frame: the native frame format and image pixels,
`VtFrame`'s decoder, `VtScreen`'s mirror, `VtRenderer`'s cell metrics and drawing, `VtImages`' picture
cache and `VtGlyphs`' shapes, with the bundled JetBrains Mono font.

Root: `VtFrame.kt`, `VtScreen.kt`, `VtRenderer.kt`, `VtImages.kt` and `VtGlyphs.kt` in
`android/app/src/main/java/io/github/sajadxdp0/tuxport/`, `android/app/src/main/assets/fonts/`, and the
frame-format specification and Frames section of `hosts/android/src/vt.c` (including frame-driven
search progress, viewport match encoding and Kitty image extraction).

Outside the module: all of `VtView`, its frame orchestration, redraw and cursor/password timers,
interaction overlays, input, selection, scrolling and search requests (`android/vtview`); terminal
creation, state ownership, PNG decoding, synchronized-output hold lifecycle, cursor defaults,
resize, the JNI functions and locking (`android/vtcore`). The flow below describes the view's duties
as a caller as well as rendering; it does not transfer those lifecycle methods here.

## Key types

- `VtFrame` — a frame read in place (an inline class over the direct buffer): the header's
  fields, each image placement, and copying the rows into a `VtScreen`.
- `VtScreen` — the screen as of the last frame: a mirror of the cells (codepoint, colours,
  attributes with search marks, grapheme clusters, each row's selected columns), one row taller
  than the grid (the overscan row), the default colours and the cursor.
- `VtRenderer` — four paints (regular/bold × upright/italic) and the cell metrics from them, the
  backing bitmap (one row taller than the grid, like the mirror) and the drawing: rows into the
  bitmap, the bitmap, the cursor (or the lock) and the composing text into the view's canvas; the
  cell attribute, underline, cursor and search colour constants.
- `VtImages` — the last frame's image placements (`Placement`, pooled) and the bitmaps they show
  by id and generation (`Picture`), copied, drawn per layer into a row and dropped.
- `VtGlyphs` — box drawing (U+2500–U+257F), block elements (U+2580–U+259F) and the Powerline
  triangles (U+E0B0–U+E0B3) drawn as shapes.

## Public entry points

- Native: `tuxport_vt_frame` and `tuxport_vt_image`, reached through android/vtcore's `VtNative.frame`
  and `VtNative.image` with its tab lock and JNI buffer/bitmap handling.
- `VtFrame`: header accessors, `placement` and `takeRows`; the format is shared with vt.c.
- `VtScreen`: `resize`, `span`, cell arrays, default colours and cursor state.
- `VtRenderer`: `setFont`, cell metrics, `resize`, `hasGrid`, `takeRows`, `drawGrid`, `drawCursor`,
  `advance` and `drawPreedit`; `VtImages`: `take`, `draw`, `drop`; `VtGlyphs`: `covers`, `draw`.

## Data flow / lifecycle

### Frames and drawing

- `onOutput` posts an invalidate on animation to the attached view; `onDraw` takes a frame
  (`VtNative.frame`, main thread only) and applies it: `frame` clears `dirty`, shows the animation
  frames due (unless a synchronized-output hold is active), updates the render state (likewise),
  reads the image placements again when their generation changed, catches a running search up
  with the terminal for at most 4 ms and marks its matches in the viewport, and writes a 64-byte
  header, the image placements (56 bytes each) and the dirty rows and the rows whose search marks
  changed since the last frame (all rows when the view asks for a full frame, or when the
  placements changed while some were or are on the screen) in the format documented in `vt.c`
  (12-byte row headers, 16-byte cells, a grapheme's extra codepoints after its row); the direct
  `ByteBuffer` is valid until the next `frame`. A search not yet caught up sets a header flag, and
  the view draws again on the next animation frame to go on.
- Overscan: while the viewport is scrolled back the render state captures the row after it too,
  the header says so (byte 58) and that row comes as row `rows` (vt.c numbers rows by the
  iterator, which is right while nothing above the viewport is asked for), with the same cells,
  dirty flag and selection columns as the others but no search marks (they come once it is in the
  viewport). The view then draws the grid up to a row higher (`scrollPx`, android/vtview's Gestures
  and selection), so as much of that row shows at the bottom as is hidden of the first.
- Images (the Kitty graphics protocol): libghostty-vt stores what programs send (PNG decoded at
  once with Wuffs, which libghostty-vt.a links, into straight-alpha RGBA; larger than the storage
  refused) and its render state lists what to draw: per placement the image, a source rectangle,
  a destination in pixels from the grid's corner (which may start above or left of it), a layer
  (below the backgrounds, below the text, above it) and a z-index. `frame` sorts them as Ghostty
  does (layer, z-index, image), then by placement and position, and joins the rows of a virtual
  placement (one per row of placeholder cells) back into one rectangle where they meet, since
  each row's source comes in whole image pixels and rows scaled apart leave seams. Every frame
  carries the whole list, and the header the milliseconds to the next animation frame (at least
  8, 0 when none plays), for which the view draws again.
- The view keeps a bitmap per image shown (`VtImages`), by id with the generation of its pixels:
  a placement of another generation has them copied (`VtNative.image`, through libjnigraphics' bitmap
  locking, premultiplied as `ARGB_8888` holds them; gray and RGB images widened), reusing the
  bitmap when the size is the same; a copy fails when the terminal has moved on to another
  generation, and the next frame names that. Bitmaps no placement shows are recycled; attaching
  another tab drops them all, and so does the window being hidden (the activity stopped), after
  which the next frame copies them again from libghostty-vt while the backing bitmap still holds
  the rows painted with them. Pixels that arrive in a frame that does not repaint every row ask
  for a full frame.
- A hold (mode 2026) captures the screen when it begins; it ends with the program, a resize or a
  full reset, or by force once a frame is taken more than 1 s after it began. During a hold the
  rows, cursor and colours come from the capture, and so does the overscan byte, while the
  header's modes, selection and scroll position are read live.
- The view asks for a full frame after a regrid, a new bitmap, new default colours (OSC 10 or 11,
  reverse video), a change of the wide-cell setting, attaching another tab or the attached tab's
  shell starting; a frame captured at an old size is dropped and a full one requested. Attaching
  also resizes the tab's terminal and console to the view's grid when it differs.
- The header also carries mode flags (mouse tracking, alternate screen, alternate scroll 1007,
  focus events 1004, a selection exists, it is a rectangle, the search is still catching up), plus
  output pending (flags bit 7: resize output has not settled at parser ground, or a synchronized-output hold), the
  selection's first and last cells in the viewport (a rectangle's top left and bottom right
  corners), the scroll position libghostty-vt reports for the scroll bar (the scrollable area in
  rows, the viewport's first row in it and the rows it shows), the search's count (matches so
  far, the selected one's place from the newest) and the mouse pointer's shape the program asked
  for (OSC 22, byte 59); each row carries its selected columns (a
  rectangle's columns on each of its rows) and each cell its search mark (attributes bits 14 and
  15). `VtView` reads the dirty byte only to tell whether late image pixels need a full frame and,
  while searching, whether to take the matches' rows again, the focus-events flag not at all, and
  reports the count to the activity when it changes.
- Cell metrics: the width is the font's advance rounded to whole pixels and the text size is
  adjusted so the advance equals it; the height is the ceiling of ascent+descent, with the baseline
  centred; the light line width is the font's underline thickness. The wide scale is two cells over
  the advance of "水" (the system's CJK fallback, one em, so 1.2 with JetBrains Mono; a font's own
  ideograph when it has one), at least 1, or 1 with the setting off.
- `onDraw` fills the view with the default background, applies the frame, draws the bitmap
  clipped to the grid's whole cells (the margin below keeps the background) and shifted up by
  `scrollPx`, then the cursor and the composing text translated by it, then the handles (whose
  places subtract it). Applying a frame repaints each row it brings into the bitmap under a
  clip: default background,
  the images below the backgrounds (showing through cells of the default background), runs of
  other backgrounds (a search match's colour over a cell's own), the images below the text, the
  selection's translucent highlight, text (black in a search match), then the images above the
  text; images are scaled from their source to their destination with bilinear filtering, as
  Ghostty samples them, and only the part in the row is drawn. Plain ASCII runs of
  one paint, colour and decoration go out in one draw call; other glyphs are centred in their one
  or two cells, a wide cell's glyph enlarged evenly by the wide scale about the cell's vertical
  middle, and any glyph still wider than its cells shrunk evenly to fit (never stretched); single
  codepoints `VtGlyphs` covers are shapes across their one or two cells; Kitty placeholder cells
  (U+10EEEE, with or without their diacritics) have no glyph drawn, as in Ghostty; four paints
  (regular/bold × upright/italic) from the variable font with ligatures off. Underlines (single,
  double, curly, dotted, dashed), strikethrough and overline are drawn; inverse and faint apply;
  invisible hides the glyph but not its decorations; blink is ignored.
- The cursor (bar and underline two light lines thick, block with the glyph redrawn in the
  background colour, underline, hollow; a block is drawn hollow while the view lacks focus; all
  but the bar cover two cells on a wide cell; in the OSC 12 colour, else the default foreground;
  a lock while the console reads a password, below), the IME's composing text (shifted
  left at the right edge) and the theme's text selection handles are drawn over the bitmap each
  draw. During android/vtview's bounded height-shrink preview, that view can reject a frame whose
  output-pending bit is set and draw its own capture of the prior grid, cursor and preedit instead.
  It requests all rows again until it accepts a frame; the renderer's bitmap still resizes at once.
- While the console reads a password the frame gives the cursor as a lock (style 4), as Ghostty
  shows one at password prompts: wherever the cursor is in the viewport, even when the program
  hid it, never blinking, with or without the focus; `VtRenderer` draws a padlock about as tall as a
  capital letter on the baseline (Ghostty draws its Nerd Font's U+F023, which JetBrains Mono
  lacks), and a block instead while the IME composes. The pump notes the state at every read;
  while the lock shows in a visible window, `VtView` asks the kernel again every 200 ms (Ghostty
  polls termios as often, all the time), since a program can turn echo back on without printing
  anything (sudo writes its line break before restoring the terminal), and stops once it ends.
  While the lock shows, the IME is told a password is typed (android/vtview's Input), as Ghostty
  turns macOS's secure input on at password prompts.
- The cursor blinks (shown and hidden every 500 ms, as Android's text fields) while the frame
  header says it blinks (the setting, on by default; a program changes it with DECSCUSR or mode
  12) and the view has the focus in a focused window; otherwise no timer runs. Input and a cursor
  move show it again for a whole period.
- The cursor setting (Settings' shape, block, bar or underline, and blinking; android/service
  gives it to every tab, restored ones not shown yet included, as `VtSession.setCursor`) is
  libghostty-vt's default cursor: a change shows at once unless a program chose its own cursor
  (DECSCUSR with a nonzero style), and is kept for DECSCUSR 0, DECSTR, a program's RIS and the
  app's Reset terminal, which bring it back; while the cursor follows its defaults, a change also
  overrides a program's mode 12. No full frame is needed: every frame's header carries the shape
  and blinking. A restored tab shows the current setting whatever its snapshot recorded (`settle`'s
  DECSTR, then the session's setting). The lock ignores it while it shows. Changing it does not
  make the last snapshot stale.

## Dependencies (inbound / outbound)

### Inbound

- android/vtview's `VtView`: asks for frames, applies the header to `VtScreen` and interaction
  state, copies images, repaints rows and calls the renderer; owns font/grid settings, full-frame
  requests, clipping/scroll offset, animation and cursor timers, and dropping view images on tab
  changes or hiding the window. Handles and scroll-bar search marks remain view overlays.
- android/vtcore's JNI frame and image functions: enter the native Frames section under the tab lock.

### Outbound

- android/vtcore: terminal and render-state ownership, `VtNative.frame`/`image`, the frame buffer's
  lifetime, cursor-style constants, PNG decoding and image storage/trimming; libjnigraphics copies
  pixels into Android bitmaps through that module's JNI.
- android/vtview: query, selection and viewport changes are reflected in frames. Frame capture
  advances its search and calls the Selection section's `viewport_ref` and `screen_point` helpers
  to encode highlights; the view consumes count/pending fields and schedules further frames.
- libghostty-vt: render state, overscan, rows/cells, Kitty placements and animation, viewport search
  matches; Android Canvas/Bitmap and fonts. JetBrains Mono 2.304 (variable, upright and italic) is
  bundled, with system fallback for other glyphs (Noto Sans CJK on AOSP images).

## Invariants and gotchas

- A cursor shape set while a full-screen program shows the alternate screen reaches only that
  screen: libghostty-vt keeps the shape per screen, and leaving the alternate screen with mode 1049
  restores no shape (its saved cursor carries none), so the primary keeps the old one until the
  next DECSCUSR 0, DECSTR, RIS or setting change; leaving with mode 47 or 1047 copies the whole
  cursor, shape included, back (from libghostty-vt's source, not tried).
- The frame format is defined in `vt.c` and read by `VtFrame`; the two change together.
- The render state's source rectangles are whole image pixels, so a virtual placement's rows,
  drawn one by one, overlap or gap by a pixel each and show seams (Ghostty's renderer draws them
  so); vt.c joins rows that meet. Rows only partly in the viewport keep the rounding at their top.
- The view paints images into its row bitmap, so a changed placement list must repaint every row:
  vt.c makes such a frame a full one, and the view asks for one when pixels arrive late. An image
  read for a frame during a synchronized-output hold may be gone or replaced by then; its copy
  fails (by generation) and the frame after the hold names the new one.
- libghostty-vt's render state keeps search highlights internally, but the C API does not expose
  them: vt.c marks the matches itself, from the viewport's matches in screen coordinates (viewport
  ones fail for a match that starts above the viewport), and resends a row whose marks differ from
  the last frame's, since the render state does not count them as a change.
- Capture and consume frames on the main thread. android/vtcore holds the tab lock during native
  capture; the direct buffer remains valid until the next frame call, so it is read after unlocking.
- `VtView` remains the owner of full-frame requests and repaint scheduling. Changing the frame
  format or its view consumers requires checking both this map and android/vtview; search queries,
  selection and scrolling use the same terminal state through android/vtcore's lock.

## Confirmed bugs / technical debt

- **libghostty-vt's drawing features left unused**: the default palette option (the colour scheme
  reply is fixed to dark); overscan above the viewport; `row_iterator_next_dirty`; the two-phase render update: `frame` holds the tab's
  lock while it encodes every row (about 95% of its time on a macOS build), where only
  libghostty-vt's deferred style work would leave it, and taking the encoding out would need the
  render state kept apart from the terminal's lock; `ghostty_render_state_row_cells_get_multi`
  in place of the four to six reads `frame` makes per cell (it stops at the first failing key, so
  the colours would go last; gain unmeasured); the colour utilities (`palette_generate`, contrast
  and luminance), of use only with themes.
- **libghostty-vt's images are trimmed only under pressure before Android 14**: each screen of
  each tab keeps up to 32 MiB of decoded Kitty images, and while a tab is shown the view holds a
  second copy of those placed on its screen (dropped while the window is hidden). Only the
  `RUNNING_LOW` and higher trim levels, which Android 14 and later no longer send, cut the tabs
  not shown down to 4 MiB a screen (android/vtcore), deleting the oldest images outright; the routine
  trim after the UI leaves keeps them all. A tab restored from its snapshot gets its images back
  (libghostty-vt's snapshot encoder writes them when built with Kitty graphics, though a comment
  in its `snapshot/terminal.zig` says otherwise); one restored from its plain transcript has text
  only.

## Open questions

- Should a Nerd Font or a CJK fallback font be bundled?
- Should invisible cells keep their decorations (as now) or hide them as Ghostty's renderer does?
- Should a cursor shape changed while a full-screen program shows reach the primary screen too?

## To verify

- Tried only on a Pixel 4a API 34 emulator (4 KB pages, Gboard in English, input from adb,
  `sendevent` and a `uinput` virtual mouse) and through vt.c's harnesses on a macOS build, where
  the search and frame times come from; not on real or 16 KB-page devices or Android 11–13.
  Unverified beyond that:
  - drawing: Kitty graphics with yazi, timg, `kitten icat`, tmux passthrough and the 32 MiB storage
    filled; the cursor setting in Chinese or while a full-screen program
    shows; the lock cursor with other IMEs composing at it; the search's marks while rows leave a
    full scrollback.
