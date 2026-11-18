# Terminal view (android/vtview) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/vtview"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions on the terminal view made before android/vtview was split from android/terminal keep their `android/terminal` tags and live in `.agents/decisions/android/terminal.md`: D5, D7, D9, D16, D17, D25, D30–D32, D34, D36, D37, D40, D41, D43 and D46–D48.

## Active

### D1: android/vtview owns view lifecycle and interaction
- **What**: android/vtview owns VtView, VtKeys and native input, paste, mouse/focus, selection, links, prompt-tap and search sections, retaining frame-consumer contracts while rendering lives in android/vtrender.
- **Why**: Interaction and rendering had identifiable boundaries but shared an oversized map.
- **Tradeoffs**: VtView orchestration and overlays stay here, sessions in android/vt and JNI, terminal locking and prompt parsing in android/vtcore.
- **Watch out**: Preserve full-frame requests, overscan, image lifetimes and search coordination, and do not move native interaction sections into vtcore.
- **Source**: d42013d (files since added by 49e8b08, D3); 9015b244, 81f60bef; d053be24; 39e55b4b

### D2: The view drops its image bitmaps while its window is hidden
- **What**: `onWindowVisibilityChanged` calls `dropImages` when the window is not visible, and the next frame finds the placements' pictures missing, copies them from libghostty-vt and asks for a full frame, while the backing bitmap keeps showing the rows already painted.
- **Why**: The view's copies of the shown tab's images stayed in memory while the app was in the background, where nothing draws them and libghostty-vt still holds the pixels.
- **Tradeoffs**: Coming back from Settings, another app or the background copies every placed image again (one memcpy each) and repaints the screen once.
- **Watch out**: libghostty-vt's own images are cut only under memory pressure before Android 14 (android/vt D3), so on newer versions they stay until their tab closes.
- **Source**: e9caf75

### D3: Frames, the screen mirror, drawing and images are classes of their own beside `VtView`
- **What**: `VtFrame` (an inline class over the frame buffer with named header fields, placements and row copying, named by vt.c as the frame's reader), `VtScreen` (the cells, selected columns, default colours and cursor), `VtRenderer` (the paints, cell metrics, backing bitmap and all drawing, with the cell constants) and `VtImages` (the placements and bitmaps) own their state, while `VtView` keeps the interaction code (selection, gestures, scrolling), reads cols, rows and cell sizes through getters and writes the cursor and default colours from the frame header into `VtScreen`.
- **Why**: `VtView.kt` held the frame format's offsets, the screen mirror, the paints and every draw call next to the input and gesture code, so neither side could be read on its own.
- **Tradeoffs**: `VtView` is still about 2100 lines, the interaction half, which stays coupled to its own state, and the frame header is read through accessors instead of inline offsets.
- **Watch out**: A frame format change now touches `VtFrame` and vt.c (android/terminal D5), and the drawing path moved with its locals hoisted (screen arrays read once per row), so a slowdown would show in the tuxport-vt frame statistics.
- **Source**: 49e8b08

### D4: Each search change forces a count notification

- **What**: VtView.search resets searchTotal to -1 so the next frame reports even an unchanged zero-match count.
- **Why**: A first unmatched query otherwise left the count blank.
- **Tradeoffs**: There is one extra count callback per query change, with the old count visible until the frame.
- **Watch out**: A frame preceding library search progress can briefly report the prior count before correcting it.
- **Source**: 5e1f851d; 20137e4c; d053be24; 39e55b4b

### D5: View members follow their event flows and frame logging is removed

- **What**: VtView groups metrics, frames, cursor, focus, input/IME, clipboard, selection, touch, mouse and scrolling, shares an inputMethod getter and removes frame statistics while renderer helpers follow drawing order.
- **Why**: Following an event required jumps between unrelated sections.
- **Tradeoffs**: The large file and gesture properties remain, and frame timing now needs a profiler or temporary logging.
- **Watch out**: Kotlin gesture/scaler initializers must remain after the state they read.
- **Source**: aaa1e7d5; 20137e4c; d053be24; 39e55b4b

### D6: Prompt taps cross rows broken by ash

- **What**: With the ash option enabled, non-soft-wrapped continuation rows without prompt cells count as the same input line in either direction.
- **Why**: Taps on later rows of long ash commands previously did nothing.
- **Tradeoffs**: PS2 prompts remain separate lines and other shells need their own declared behavior.
- **Watch out**: Arrow counts were tested with the host program rather than touch input; background-output limitations remain in android/vt D6.
- **Source**: a809c5a8; 20137e4c; d053be24; 39e55b4b

### D7: Clipboard and delayed input obey the shared privacy gate

- **What**: Copy and paste reject locked access, preview dialogs use protected windows and MainActivity checks authorization again at the final input sink.
- **Why**: A visible shield alone cannot prevent queued IME or dialog callbacks from sending input.
- **Tradeoffs**: Operations while locked or without a resumed page are discarded instead of replayed.
- **Watch out**: Every new clipboard/input path and delayed callback must retain the final checks.
- **Source**: f61072f3; d053be24; 39e55b4b

### D8: IME replacement and completed composition use the checked commit path

- **What**: replaceText, finished composition and IME paste actions use commitText checks, treating LF/CR/ESC as paste risks except lone LF as Enter and offering trailing-CR/LF removal only for a nonempty safely encoded remainder.
- **Why**: Replacement and composition paths could delay clipboard delivery or bypass unsafe-paste confirmation.
- **Tradeoffs**: Offsets cannot edit input already sent, safe pastes stay immediate and optional-action validation encodes without sending.
- **Watch out**: Recheck privacy and terminal modes when the user acts; real Gboard suggestions/history, other IMEs and older Android versions still need end-to-end device coverage.
- **Source**: 6f5be4da; d053be24; 39e55b4b

### D9: Paste previews style only the inserted command as monospace

- **What**: The bounded preview uses shared monospaceMessage with span-preserving localized ^1 substitution.
- **Why**: Commands should remain visually distinct from explanatory text, as imported paths do.
- **Tradeoffs**: Templates must use TextUtils placeholders and long lines still wrap.
- **Watch out**: Preserve the placeholder and spans in future translations without changing paste authorization or encoding.
- **Source**: 165b5f99; d053be24; 39e55b4b

### D10: Height shrink briefly preserves the last painted frame

- **What**: Same-width row shrink at the bottom without selection/search preserves a clipped painted bitmap until settled output or 200 ms, retaining subrow previews, requesting full rows after skipped frames and clearing on interaction or lifecycle changes.
- **Why**: Old rows can shift visibly before a CLI redraw arrives.
- **Tradeoffs**: One temporary bitmap delays resized display briefly, while width changes, growth and interactive history render immediately.
- **Watch out**: Track painted state independently of full-frame requests; slow redraws can exceed the deadline and vendor IME interruption coverage remains limited.
- **Source**: 2e73a06a; 39e55b4b

### D11: The system selection menu inherits the terminal night override

- **What**: The existing TYPE_FLOATING selection menu uses MainActivity's pinned night configuration for its surface, text and ripple colors.
- **Why**: Selection needs consistent colors without replacing system interaction.
- **Tradeoffs**: The OS still determines layout and appearance.
- **Watch out**: Verify OEM/version differences and menu reopening after system dark-mode changes.
- **Source**: 4c639565; 39e55b4b

## Superseded
- ~~D1 ownership of frame encoding, mirrors, drawing, images, glyphs and fonts~~ → replaced by **D1** in 9015b244 (2026-11-14)
