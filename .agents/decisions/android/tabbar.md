# Tab controls (android/tabbar) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/tabbar"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions on android/tabbar made before it was split from android/terminal keep their `android/terminal` tags and live in `.agents/decisions/android/terminal.md`: D4, D18–D24, D27, the drawing half of D39, D51, D54 and D63–D65 (D63 also retains later activity-owned integration).

## Active

### D1: Tab rendering and status drawing have a dedicated owner

- **What**: android/tabbar owns tab rendering, progress marks and number animation, while activity integration and session ownership remain outside.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14; 39e55b4b

### D2: The list covers the content column including keys and banner

- **What**: The list retains 32 dp rows and a ten-row cap within the combined terminal, extra-key and notification-banner column, suppressing the covered banner accessibility node until close.
- **Why**: The list must stay directly below the top bar and cover the controls beneath it.
- **Tradeoffs**: Short viewports can use keys/banner height, longer lists scroll, and covered notification authorization is temporarily unavailable.
- **Watch out**: API 34 checks establish placement and touch routing but not TalkBack speech or frame-by-frame IME clipping.
- **Source**: 562f2d78, 4918eabc; 39e55b4b

### D3: Native tooltips label fixed controls and tab titles

- **What**: All five fixed controls and numbered tabs use native tooltipText, with Tab.update refreshing fallback titles and ended markers.
- **Why**: Symbols need explanations and titles should appear near the tab being inspected.
- **Tradeoffs**: Android controls tooltip placement and duration, and both tab views inherit line spacing from the terminal theme.
- **Watch out**: A theme change can alter label height, and long-title wrapping, edge placement, release behavior, IME overlap and privacy transitions need device verification.
- **Source**: 655d37ec; 39e55b4b; b5303dd3

### D4: Selected tabs and the list button share the terminal control palette

- **What**: Apply terminalControlColor to bar labels, the selected indicator, the list button and list rows while keeping progress and status colors.
- **Why**: Current tabs and the open list need the same visible distinction as active extra keys.
- **Tradeoffs**: Selected indicators are brighter than progress marks, and list selection still relies on text color.
- **Watch out**: Check ended-tab fading, wallpaper palettes and small-label readability on devices.
- **Source**: 71e94b55

## Superseded
- ~~Terminal-only bounds inherited from android/terminal 34348eda, then terminal-and-key bounds in 562f2d78~~ → replaced by **D2** in 562f2d78 and 4918eabc (2026-11-15; cross-module first link)
- ~~Tab-title toasts inherited from android/terminal D18~~ → replaced by **D3** in 655d37ec (2026-11-16; cross-module link, other parent constraints retained)
- ~~Direct primary-color selection in android/terminal D20 and D63~~ → replaced by **D4** in 71e94b55 (2026-11-17), retaining layout and animation.
