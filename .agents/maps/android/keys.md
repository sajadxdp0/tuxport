# android/keys Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/keys.md` for the paired decision history (not yet created; pre-split decisions D4, D12, D14, D15 and D19–D23 stay in `.agents/decisions/android/terminal.md`).
>
> Verified: 2026-11-15 (split out of the android/terminal full mapping of 2026-11-14; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-17 (targeted verification only since the full mapping; latest change: shared terminal control colours with stronger state contrast)

## Responsibilities

`android/keys` owns `ExtraKeysView.kt` in `android/app/src/main/java/io/github/sajadxdp0/tuxport/`: adaptive
rows, pages, key touch/repeat handling, one-shot/locked modifiers and shared indicator helpers.
`MainActivity.Keys`/`Host`, routing to the terminal, insets/IME translation and shared resources
remain `android/terminal`; terminal key encoding remains `android/vtview`.

## Key types

- `ExtraKeysView` — adaptive extra keys, normally Termux's default keys widened to two rows of
  eight (ESC, `/`, `-`, HOME, ↑, END,
  `|`, SHIFT; TAB, CTRL, ALT, ←, ↓, →, `~`, SYM), with a second page of symbols for every key but
  SYM (`` ` ``, `$`, `*`, `=`, `^`, `\`, `&`, `%`; `[`, `]`, `{`, `}`, `<`, `>`, `;`: shell
  symbols absent from the first page, with bracket pairs together and `&` above `;` in the
  last scrolling column) and a third of function keys for every key but SHIFT and SYM (F1–F6,
  PGUP; F7–F12, PGDN): a `Page` keeps its original rows, `singleRowKeys` order, grid and
  `upperKey` (the key above SYM in two-row mode).
  The grid shown is in a `HorizontalScrollView`, with fixed keys over its end: SHIFT or `%`
  above SYM in two-row mode, CTRL/ALT/SHIFT/SYM beside it on the main page in single-row mode,
  with only SYM fixed on that layout's symbols/function pages and `%` following `&` before the bracket pairs.
  It keeps CTRL, ALT and SHIFT (one-shot or locked; `shift` reads
  false while the symbols are shown) and the page shown (`KEYS`, `SYMBOLS` or `FUNCTIONS`), exposes
  them with `modifiersUsed`, and sends keys, text and notices (a modifier locked) to a `Target`; a
  `Key` may have a `longPress` (the modifiers' lock, SYM's function keys); `Press` handles the
  touch on one key. Its companion's `FADE_DP`, `indicatorPaint`, `drawIndicator` (the indicator
  under a label) and `indicatorBounds` (where it lies, where the tab row also draws a tab's
  progress) serve the tab row too.

## Public entry points

- `Target.key`, `text`, `notice`; `ctrl`, `alt`, `shift`, `modifiersUsed`.
- Shared `FADE_DP`, `indicatorPaint`, `drawIndicator`, `indicatorBounds` serve `android/tabbar`.

## Data flow / lifecycle

### Extra keys

- Rows of 32 dp with 12 dp labels (not sp, so the system font scale leaves them as they are).
  Below 600 dp available width, portrait windows retain two rows, as do landscape windows with
  configuration height above 480 dp. In that layout, the
  eight columns share the width, each at least 45 dp (the last one takes the pixels left over), so
  they all fit from 360 dp on. Narrower, the first seven columns of both rows scroll sideways
  together under the finger (turning the page keeps the scroll position) and under the last, which
  stays at the right edge; while more keys lie beyond, they fade out over 32 dp ending 6 dp inside
  the last column, in the blank before SHIFT, as the tab row's into `×`, and a touch there goes to
  the last column's key. A sideways slide past the touch slop then scrolls and cancels the key it
  started on, unless that key is already held, and with gesture navigation the scrolling part is
  excluded from the system's back gesture. With nothing to scroll the keys keep every touch and the
  edge its gesture. No lines are drawn between or around the keys.
- An available width of at least 600 dp in either orientation, or landscape with configuration
  `screenHeightDp <= 480`, uses one 32 dp row. `onMeasure` chooses the mode from its width
  constraint after the root's system-bar/cutout/IME side padding; keyboard height does not choose
  the mode. Configuration changes request measurement rather than choosing from old view bounds.
  CTRL, ALT, SHIFT and SYM stay fixed at the right on the main page;
  the symbols and function pages fix only SYM, hiding the modifiers without clearing their state.
  SHIFT still waits on the symbols page but applies on the function page.
  The main scrolling row is ESC, TAB, `/`, `-`, ↑, ↓, ←, →,
  HOME, END, `|`, `~`; the symbols are the original scrolling top row followed by `%` and the
  bottom row, placing `%` after `&` and ending with `;` at SYM's left edge,
  and the function page is F1–F12 followed by PGUP/PGDN. All keys remain available, each at least
  45 dp wide; excess keys scroll under the fixed group with the same fade and gesture exclusion.
  Every single-row page shares the width over 16 columns, with rounding remainder in SYM, so
  key widths and SYM's bounds stay unchanged across pages. At 600–719 dp the main and symbol
  pages still scroll because keys retain their 45 dp minimum. When 16 columns fit, the function page
  leaves one column empty before SYM. Mode changes during measurement reparent existing key views, retaining the
  selected page and modifier states, cancelling pending presses/repeats and resetting horizontal
  scroll to the start when switching layouts. They do not recreate the terminal or its IME connection.
- A key is shaded while pressed and acts when the finger lifts (with a key-click haptic), not at
  all if the finger left it by more than the touch slop. Held past the long-press timeout, the
  arrows, PGUP and PGDN repeat every 80 ms until the finger lifts or leaves the key (no act on
  release then); the modifiers and SYM act on their long press then, with a long-press haptic, and
  not again on release; other keys still act once on release. No key has a swipe or popup. Keys
  with a long press also answer an accessibility service's long click (the touch never reaches
  the view's own long-press detection).
- Labels use Ui.kt's `terminalControlColor`: ordinary labels blend on-surface-variant with 40%
  black, while active labels and indicators blend primary with 25% white, retaining wallpaper hues.
- CTRL, ALT and SHIFT: a tap turns them on for the next key (active colour only) or off, a locked
  one too; a long press locks them on (active colour and the
  indicator) until tapped, one a tap turned on included, with a toast naming the key ("CTRL
  locked"), or turns a locked one off. Keys go to `VtView.press` and `type`, which read the
  modifiers through the host and report when a key took them; that clears one-shot modifiers and
  leaves locked ones on.
- A tap on SYM shows the symbols, and from either other page shows the first again; a long press
  shows the function keys, and from them the first page again. Two-row mode keeps `%` above SYM
  with symbols and SHIFT above SYM with functions; single-row mode fixes only SYM on either page.
  A page stays, for any number
  of keys, until SYM turns it; SYM's label takes the active colour and the indicator while another
  page than the first is shown. The function keys and PGUP/PGDN go to `VtView.press` as their key
  codes, so the modifiers apply as on a keyboard (SHIFT, F5 sends `CSI 15;2~`). Turning the page
  leaves the modifiers as they are: CTRL and ALT apply to symbols and function keys too (CTRL,
  SYM, `]` sends Ctrl+]); CTRL/ALT are hidden on both alternate pages, SHIFT also on the single-row
  function page. SHIFT waits on the symbols page: it applies to no key, the symbols or the IME's,
  and a one-shot SHIFT is not used
  up, so it applies again once another page is back. A key still held on the page going away is
  cancelled. The page lives as long as the view: a new activity starts on the first.

## Dependencies (inbound / outbound)

### Inbound

- `android/terminal` builds the view, reads modifiers and reports their consumption through its
  VtView host adapter; `android/tabbar` calls the shared drawing helpers.

### Outbound

- Target callbacks go to `android/terminal`, which forwards input to `android/vtview`.
- `android/terminal` supplies Ui.kt sizing/theme helpers, strings and themes; Android touch,
  accessibility and haptic APIs drive keys and long clicks.

## Invariants and gotchas

- A `HorizontalScrollView` takes any sideways slide past the touch slop even when it has nothing
  to scroll, cancelling the child's touch; the extra keys' scroll view intercepts only when it can
  scroll, and a held key asks its parents not to intercept.
- The column width is set on the keys' layout params during `ExtraKeysView.onMeasure`; the rows
  and the grids inside the scroll view are measured without a width limit, so they are forced to
  measure again whenever the column width changes, the pages not shown too (each keeps the flag
  until it is added back).
- SHIFT's view is the `upperKey` of both the first page and the two-row function page (a view has
  one parent, and a `Modifier` one view), so turning between those pages in two-row mode leaves
  it in place and a SHIFT held meanwhile is not cancelled. In single-row mode both alternate
  pages detach all modifier views while keeping their state. `show` selects an ordered list of
  fixed keys and only removes/adds differing views, keeping SYM attached through its long press.
  Layout changes first detach every
  page's rows and the fixed group before reparenting shared keys; they cancel all presses but retain modifier state.
- The tab row and the extra keys' grid draw into their end padding (`clipToPadding` off).
  `HorizontalScrollView` leaves the padding out of its scroll range and fade strength, and `View`
  draws the fading edge at the padding; a right padding offset (`isPaddingOffsetRequired`,
  `getRightPaddingOffset`) carries the edge into `×` or the last column without changing either.
  `canScrollHorizontally` counts the end padding as shown (from 315 dp on the keys would find
  nothing to scroll), so the extra keys compare the grid's width with the room between the
  paddings. How far a fade reaches in (12 dp into `×` at 14 dp, 6 dp into SHIFT at 12 dp) fits
  the blank before the glyph in Roboto; another label size or font can put the fade over it.
- The indicators are drawn by the containers (the tab row, each page's grid, the fixed column) in
  `dispatchDraw`, over their labels, so a change of state must invalidate the container, not the
  label: `refresh` invalidates the shown grid and the fixed column (turning pages refreshes the
  newly attached grid), and `TabBarView.render` the tab
  row. The label's place is the sum of `left` and `top` up to the container:
  `offsetDescendantRectToMyCoords` subtracts each view's scroll, and a single-line `TextView` (the
  extra keys' labels) scrolls far sideways to centre its text, which also draws a foreground
  drawable it is given off the label.
- Shared indicator/fade helpers also serve `android/tabbar`; changes require checking both maps.
- MainActivity owns final privacy checks and routing; modifier state is consumed only after
  VtView reports use, not when the extra-key callback fires.

## Open questions

- Search focus still sends extra keys to the terminal; the policy question is in `android/terminal`.

## To verify

- The shared control palette passes debug assembly and lint; device checks remain for one-shot,
  locked and SYM states across wallpaper palettes. State transitions and indicator geometry are unchanged.

- Single-row extra keys passed debug assembly/lint and API 34 checks at 360 × 640 dp: three pages,
  scrolling to PGDN, input with Shift+F5 and locked Ctrl+], and page/modifier retention through
  portrait/landscape changes, with and without the IME. The symbols page reclaimed 90 dp from hidden CTRL/ALT, kept
  locked CTRL and one-shot ALT across rotation, sent Ctrl+] with CTRL hidden, and restored both
  controls on the main page. Before the symbol reorder, its scrolling `%` reached SYM's left edge and typed `%`;
  rotation restored the fixed portrait placement and the scrolling landscape placement.
  The single-row function page hid all three modifiers, preserved a preselected SHIFT across
  rotation for Shift+F5, scrolled PGDN to SYM's edge and restored modifiers on the main page.
  At 840 × 400 dp, all three pages had equal ordinary-key widths above the 45 dp minimum and
  identical SYM bounds; the function page left exactly one key width before SYM.
  After the layout cleanup, API 34 checks confirmed two rows at 599 dp and one at 600 dp, retaining
  the symbols/function page across repeated resizing and preselected SHIFT for Shift+F5.
  At 560 × 360 dp, short landscape used one row with left/right/up/down arrows; at 840 × 400 dp,
  all three pages again shared key widths and SYM bounds, with the one-column gap on the function page.
  A 600 dp-wide tall landscape display retained two rows after its side cutout reduced available
  width; before the symbol reorder, the narrow single-row symbols/function pages scrolled `%`/PGDN to SYM's left edge.
  The new `%` position after `&` and trailing `;` still need device verification.
  Hiding the IME retained the short-landscape mode.
  Physical touch, TalkBack and rotation during a held repeat remain
  unverified on real devices.

- Extra-key long presses through TalkBack and function keys in real programs such as mc/htop
  (rather than `cat -v`) remain unverified.
