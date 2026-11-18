# android/licenses Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/licenses.md` for the paired decision history (not yet created; pre-split decisions D3, D4, D13, the license-page part of D50 and D55 stay in `.agents/decisions/android/terminal.md`; the newer offline-reader decision remains in bd580227 Decision 1 with its android/terminal tag until distilled).
>
> Verified: 2026-11-14 (split out of the android/terminal full mapping of 2026-11-14; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: repoint dependencies to the split module maps)

## Responsibilities

`android/licenses` presents the checked-in component notices as a list and an offline HTML
reader, resolves catalog IDs and local document links, and restores reading position.

Root: `LicensesActivity.kt`, `LicenseActivity.kt` and `LicenseCatalog.kt` in
`android/app/src/main/java/io/github/sajadxdp0/tuxport/`, and `layout/list_item.xml` and `values/ids.xml`
(the license list and reader IDs) under `android/app/src/main/res/`.

Outside the module: the About entry is android/setup's; the shared strings, themes, `page.xml`
and `Ui.kt` are android/terminal's; authentication is android/privacy's; the notice assets,
their CSS, reference license texts, dependency inventory, build and manifest remain android/build's;
instrumentation tests belong to android/tests. This module consumes those assets without owning their upkeep.

## Key types

- `LicenseCatalog` — reads `assets/notices/index.json`: stable ID, name, version, chosen license,
  HTML file, optional package count and standalone-license kind; formats localized summaries.
- `LicensesActivity` — the Material 3 list, one `list_item.xml` per component entry (excluding
  standalone license texts), sorted by name ignoring case, with a stable scroll-view ID.
- `LicenseActivity` — a separate non-exported Activity, opened with a catalog ID; an offline
  WebView reader on the Material 3 page, with relative scroll position saved across recreation.

## Public entry points

- `LicensesActivity`, opened from Settings; `LicenseActivity`, opened from the list or a local
  license link with `EXTRA_ID` (unknown IDs finish without loading a URL).
- `LicenseCatalog.read` reads the bundled index; entries expose the display title and localized
  subtitle to the list and reader.

## Data flow / lifecycle

### Catalog, navigation and reading

- Settings' About group has a separate localized Project website action (android/setup); the
  license summary names GNU AGPL 3.0 or later, with copyright and warranty details in the Tuxport notice.
  Its final Version action is labeled "Version" / "版本号"; android/setup supplies the build
  version summary and Google Play navigation.
- `notices/` is checked in (android/build): Tuxport, Fuchsia, libghostty-vt and its linked
  components, JetBrains Mono, Alpine and the bundled doas/bash/procps/bubblewrap packages with libcap2, Android
  libraries, Rust crates, SQLite and standalone license texts. Subtitles show the chosen
  license; Alpine shows a localized package count and "Multiple licenses", and SQLite "Public
  domain". Standalone texts stay in the catalog and assets for links from Android libraries and
  Alpine, but do not appear in the main list. Upstream alternatives remain in the body.
- The page (`page.xml`): a `MaterialToolbar` (title, back arrow) in an `AppBarLayout` that lifts
  while the content below it is scrolled, over a `FrameLayout` with the app bar's scrolling
  behaviour, containing the list's `NestedScrollView` or the reader's WebView; edge to edge (wallpaper
  colours from Android 12 on through `DynamicColors`), with the bar padded under the status bar
  and content clear of the navigation bar. The reader's toolbar names the entry, Back and Up
  finish it, and the list's scroll view retains its place, including across recreation.
- The reader uses AndroidX `WebViewAssetLoader` on `appassets.androidplatform.net`; only the
  selected HTML file, shared CSS and an in-memory theme stylesheet can load. JavaScript, file,
  content and network access are disabled. HTTP(S) links open Custom Tabs; local links to other
  catalog documents open another reader, and same-document anchors stay in the current reader.
  A renderer exit destroys the WebView and shows a localized message to reopen the page.
- Shared CSS gives selectable, wrapping 16 px body text, 1.65 line height, 24 px side padding
  and a 48 rem maximum reading width. WebView text zoom follows system font scale; pinch zoom
  is available. Material colours are resolved before background asset requests.
- The reader recreates for configuration changes, loads its ID again and restores its relative
  scroll position after visual layout; the list handles orientation in place. Both recreate
  for dark mode, density, locale and font-scale changes.

## Dependencies (inbound / outbound)

### Inbound

- android/setup: Settings' About action opens the list without requiring a running kernel.
- android/build: bundled catalog and HTML assets and manifest registration; android/tests: `LicenseTest`.
- android/privacy: automatic lifecycle protection installed on both activities.

### Outbound

- android/terminal: shared `Theme.Tuxport`, localized strings, `page.xml`, `dp` and `themeColor`.
- android/build: `assets/notices/index.json`, the selected HTML and shared CSS.
- AndroidX WebKit's asset loader, WebView, Custom Tabs for external HTTP(S) links, AppCompat,
  Material Components, insets helpers and the system-selected browser.

## Invariants and gotchas

- Open the reader by a catalog ID, not an arbitrary URL. Requests are restricted to the selected
  HTML, shared CSS and generated theme CSS; local links resolve through the catalog.
- Standalone license texts stay addressable by local links but do not appear in the main list.
- Resolve Material colors on the UI thread before asset requests on WebView's worker threads.
- The licenses and Settings leave `uiMode` out of `configChanges`: dark-mode changes recreate
  them, since an AppCompat activity handling the change would retain its existing views' colors.
  Neither handles `density`; every view is sized in dp when built. Locale and font scale also
  recreate the pages. The shared frame remains android/terminal's.
- Both pages use android/privacy's automatic gate. Keeping them separate activities must not
  bypass authentication or the secure-window policy.

## Open questions

None recorded in the parent mapping for this concern.

## To verify

- The offline license reader's renderer-exit fallback, pinch zoom, very large system fonts and
  tablet layouts remain untried. API 34 emulator checks cover offline rendering, no horizontal
  overflow, scroll restoration on recreation, list navigation and light/dark Material colours.
