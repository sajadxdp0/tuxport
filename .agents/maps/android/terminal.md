# android/terminal Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/terminal.md` for the paired decision history (it still holds the pre-split decisions for `android/vt`, `android/vtcore`, `android/vtview`, `android/vtrender`, `android/licenses`, `android/tabbar`, `android/keys` and `android/file-import`; pre-split privacy decisions and newer license decisions remain in their original android/terminal commits until distilled).
>
> Verified: 2026-11-14 (full mapping: 5 research concerns; 18 claims checked by 3 independent verifiers — 15 confirmed and 3 partial, corrected below)
>
> Maintained: 2026-11-17 (targeted verification only since the full mapping; latest change: stronger contrast between selected and ordinary terminal controls)

## Responsibilities

`android/terminal` coordinates the terminal screen around sessions (`android/vt`), native
terminals (`android/vtcore`), interaction (`android/vtview`) and rendering (`android/vtrender`).
It owns activity lifecycle, choosing/closing tabs, shortcuts, search UI, link routing, clipboard
questions, insets and IME animation, plus shared UI helpers, strings and themes.

Root: `MainActivity.kt`, `TerminalLayout.kt`, `TerminalLinks.kt`, `NotificationPermission.kt`, `BatteryOptimization.kt`, `SearchBarView.kt` and `Ui.kt`
in `android/app/src/main/java/io/github/sajadxdp0/tuxport/`, and `android/app/src/main/res/` except
android/privacy's `layout/privacy_shield.xml`, `drawable/privacy_emblem.xml` and `drawable/ic_lock.xml`,
and android/licenses' `layout/list_item.xml` and `values/ids.xml`.

Controls are `android/tabbar` and `android/keys`; retained file-copy work is `android/file-import`.
All MainActivity adapters, picker/dialog handling, shared strings and themes remain here.
`Theme.Tuxport` is Material 3 DayNight; `Theme.Tuxport.Terminal` is always dark with black window
and system bars. Shared resources include `Widget.Tuxport.MorePopup`,
`ThemeOverlay.Tuxport.CheckPopup` (framework checkbox without its own ripple), `page.xml`
and `preference_switch.xml` (MaterialSwitch at AndroidX's `switchWidget` ID). Settings uses
AndroidX category/row layouts.

Session ownership, saving, reports and event routing are `android/service`; build and notice
assets are `android/build`; authentication is `android/privacy`, notice UI `android/licenses`.

## Key types

- `MainActivity` — a `TerminalService.Listener` with a `TerminalLayout` and `TerminalLinks`;
  adapters `Host : VtView.Host`,
  `Bar : TabBarView.Target`, `Tabs : TabListView.Target`, `Find : SearchBarView.Target` and `Keys : ExtraKeysView.Target`; the
  current session and its index, from which its successor is taken; a tab requested by a
  notification (`EXTRA_TAB`); AndroidX Back's callback while the search bar is open (`closeSearchOnBack`);
  the open question whether a
  program may read the clipboard (`clipboardQuestion`).
- `TerminalLayout` — builds the views with the activity's adapters after dynamic colours are
  applied: a black column of one bar's place (`TabBarView`, or `SearchBarView` while searching),
  a shared overlay frame containing a column of optional permission banners (notifications, then battery usage),
  the terminal body (`VtView`, status text and `ProgressLine` along its top) and `ExtraKeysView`,
  with `TabListView` above that column;
  exposes the root and interactive views, keeps the status view private behind `showStatus` and
  `showTerminal`, and owns the IME animation callback `keysFollowIme`.
- `NotificationPermission` — shared notification availability, request history and system-settings
  navigation for MainActivity and SettingsFragment; callers own their result launchers and visibility.
- `BatteryOptimization` — checks the package’s Doze exemption, requests it from the terminal
  and opens system app details from Settings.
- `TerminalLinks` — activity-scoped link routing through `open`, the guest-file lookup thread and
  `startViewer`; keeps the activity and its toast callback, with no service or session state.
- `SearchBarView` — the search bar, a row as tall as the tab bar: the text to find (an `EditText`
  without suggestions, its IME action Search), the count ("selected/total"), `↑`, `↓` and `×`; it
  shows the count it is given (`setCount`) and sends text changes, steps and closing to a
  `Target` (`search`, `step`, `close`).
- `Ui.kt` — what the screens and views share (`VtView` too): `LINE_DP` and `line()` (how thick
  the indicators and the progress line are: 1 dp, at least a pixel), `dp`, `themeColor`
  (white when the theme lacks the colour), `themeDrawable` and `touchFeedback` (the theme's
  `selectableItemBackground`); `showPrivate` registers dialog security with android/privacy before showing it.
  `terminalControlColor` separates states on black: ordinary labels blend on-surface-variant
  with 40% black, selected labels and indicators blend primary with 25% white. Tab controls
  and extra keys share it; progress/status marks keep their own theme colours.
  `monospaceMessage` substitutes a styled value into a localized `^1` template with TextUtils,
  leaving the explanation in the dialog's normal font.

## Public entry points

- Launcher activity `MainActivity` (`singleTask`, `adjustResize`, `Theme.Tuxport.Terminal`, handles
  most configuration changes itself, the dark mode included, but not density, locale or font
  scale, which recreate it), started with
  `EXTRA_TAB` (a `VtSession.id`) by a tab's notification; listener callbacks `onProgress`,
  `onMachineReady`, `onSessionsChanged`, `onBell`, `onClipboardRead`, `onTabStatus`,
  `onSettingsChanged`,
  `onNotice`, `onRestarting`, `onExitAll`, `onOpenUrl`; `onPrivacyUnlocked` from the privacy gate,
  and the `OpenMultipleDocuments` activity-result callback for import.

## Data flow / lifecycle

### Binding and choosing a tab

- `res/xml/locale_config.xml` lists English (`en`, default strings), Simplified Chinese
  (`zh-CN`, `values-zh-rCN`), Traditional Chinese (`zh-Hant`, `values-b+zh+Hant`), Korean
  (`ko`, `values-ko`) and Japanese (`ja`, `values-ja`) for the manifest's Android 13+ system
  language picker. Traditional Chinese uses Taiwan terminology with a script-wide qualifier.
  All translated string files follow the default file's sections and resource order, omit
  non-translatable resources and use only `other` for plurals. There is no in-app picker;
  Android 11–12 keep following the system language. Locale changes use
  normal activity recreation and the existing service-current tab fallback below.
- `attachBaseContext` applies an override containing only `UI_MODE_NIGHT_YES`, before the window,
  dynamic colours or views read resources. The terminal's night-qualified resources therefore
  match its fixed dark theme; other configuration fields and other activities remain independent.
- `onCreate` applies dynamic colours, reads `EXTRA_TAB` (only when the activity is not being
  recreated and was not opened from Recents: such an activity still has the intent that first
  started it), reads the font size (kept in dp as `font_size_dp`, defaulting to 14 dp, so it
  scales with a display size
  change; a pixel size an earlier version kept as `font_size` is converted at the current density
  and removed), applies the saved Keep screen on choice (off by default), builds `TerminalLayout`
  (the status line says "Starting…", insets and IME animation are wired there), installs its root
  edge-to-edge and updates the permission banners without opening system dialogs. `onStart` applies the wide-cell
  and tab-list visibility settings through `onSettingsChanged`, starts and binds the service; `onServiceConnected` registers the listener and replays
  the start's result or progress and pending notices; `onStop` unregisters and unbinds.
- When the kernel is ready the tab shown is, in order: one a notification asked for (`EXTRA_TAB`,
  from `onCreate` or `onNewIntent`; a tab closed meanwhile is skipped), the activity's own current
  tab, the service's `current` (the tab the last activity showed, for a new one: recreated, or
  opened again after Back finished the last on Android 11), the service's `focus` (the restored
  current tab), each if it still exists, else the last tab; with no
  sessions a new one opens. A failed start hides the terminal and shows "Cannot start: …";
  `onRestarting` detaches and shows "Restarting…". `onServiceConnected` calls `onMachineReady` on
  every bind (each `onStart`) when the kernel runs or failed to start (while it starts, the
  progress, and `onMachineReady` later from the service), so coming back to the activity (from
  Settings, a link's app, the background) shows the current tab again, which closes the search
  bar.
- Showing a tab sets it as the service's `current` (which takes its notification away), attaches
  it to the view (`VtView.attach`) and renders the bar; `onRestarting` and `onDestroy` detach it,
  so sessions never keep a destroyed Activity's view.
- Closing a tab hangs it up at once; when it was the current one, the tab after it, now in its
  place, is shown (the one before when it was the last). Closing the last tab while the kernel runs
  finishes the activity (the service stays).

### Tab controls and activity policy

- `android/tabbar` owns bar/list rendering, animations and progress marks. `MainActivity`
  implements their targets, validates session identity and privacy, opens/closes the list,
  coordinates focus, Back and accessibility, and performs session/menu actions.
- Numbered-tab titles use native tooltips owned by `TabBarView`; the activity's shared toast
  helper remains for extra-key notices, link failures and file-import feedback.
- The list closes on tab switching, search, other bar actions, extra-key input, pause and restart;
  list close buttons retain it while using the existing busy confirmation.
- The list starts directly below the top bar and overlays both permission banners, terminal
  and extra keys; its rows may cover the keys in short viewports. Taps outside its rows dismiss
  and are consumed, including over the keys. The whole content column is hidden
  from accessibility while the list is open, and restored when it closes.

- The list’s close buttons, `×` and the menu's "Close other tabs" and "Close all tabs" close at once when none of the tabs
  they close is busy (its shell has ended, has not started, as in a restored tab never shown, or
  waits at its prompt as the prompt marks tell, `VtSession.busy`, android/vt's Shell integration),
  else after a confirmation dialog ("Close this tab?" for one tab, else "Close N tabs?", counting
  all of them); a shell without prompt marks always counts as busy. Several tabs are closed with the
  current one last, so no other tab is shown (and its shell started) on the way, and closing all
  ends like closing the last tab. Ctrl+Shift+W closes the current tab without asking, even while its
  shell runs, and in an ended tab any input with a line break (Enter) closes it.
- The close dialog's button closes only the tabs still open, and dismissing it or the clipboard
  question gives the terminal the focus.
- Its dialogs (close, notices, the clipboard question, the paste confirmation) and its menu (⋮, a
  `PopupMenu` placed like a
  toolbar's overflow menu) are Material 3 in the theme's dark colours, from the wallpaper on
  Android 12 and later (`DynamicColors`). The tab bar, the extra keys and what lies under the
  system bars are black like the terminal; tab/list controls and extra keys use
  `terminalControlColor` for stronger state contrast while preserving wallpaper hues.
  Status text, permission banners and search controls retain their original theme colours.
- "Keep screen on" is available even without a current tab and applies to the whole terminal
  window. Its right-side checkbox reflects the activity's choice, persisted as `keep_screen_on`
  in the activity's private preferences, so recreating or reopening the activity keeps it.
  The choice sets or clears `FLAG_KEEP_SCREEN_ON`: only the visible terminal window keeps the
  screen lit; opening Settings or leaving the app allows the system's normal screen timeout.
  It is independent of the service's screen-off run policy and partial wake lock. The popup's
  context uses `ThemeOverlay.Tuxport.CheckPopup`, removing the checkbox's own background while
  retaining the menu row's press feedback and the overflow menu's placement.

### Permission banners

- The notification and battery banners share a 32 dp row design below the top bar, including
  during search. Each localized 14 dp message is start-aligned with
  10 dp horizontal padding and fallback line spacing disabled. It ellipsizes in narrow windows
  and fills the clickable space before a separate 32 dp close button.
  Both buttons use background touch feedback with tint cleared over the container's surface colour,
  muted text and tap haptics. The close button has a localized accessibility description.
- The notification banner appears while notifications are disabled. Closing it persists `dismissed_notification_prompt` in MainActivity preferences;
  recreating or relaunching the activity does not show it again. Closing does not request
  permission or alter authorization history. The tab list hides both buttons from accessibility
  while covering the banner through the existing container-level policy.
- `NotificationPermission` shares the authorization flow between the banner and Settings' Android
  notification action (android/setup). It retains `requested_notifications` in the original
  `MainActivity` preference file, so requests from either page and prior versions share history.
  Startup and privacy unlock never request permission. Tapping either entry checks the privacy gate.
  On Android 13+, a first request uses the caller's activity-result launcher; a prior attempt
  or the system's denial rationale state routes to app notification settings. Android 11–12 use
  settings when notifications are off. An updated installation without request history is treated
  as already requested, preserving older versions' possible permanent denial. A dismissed
  permission dialog also counts as an attempt; only another tap opens settings.
- Navigation uses `ACTION_APP_NOTIFICATION_SETTINGS` with `EXTRA_APP_PACKAGE`, falling back to
  `ACTION_APPLICATION_DETAILS_SETTINGS` if the first activity is missing. Permission results and
  `onResume` refresh each entry's visibility. Granting removes both entries; revoking restores the
  Settings entry and restores the banner only if it was never dismissed. Individual disabled
  channels do not show either entry.
- The battery banner appears while `PowerManager.isIgnoringBatteryOptimizations` is false and
  disappears on dismissal or grant. Its separate `dismissed_battery_prompt` preference leaves
  the notification banner and Settings action available under their own visibility rules.
  A tap requests the exemption with `ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` and the package
  URI. If that activity is missing, it opens app details, the same destination as the Settings
  action. Both paths check privacy and the live exemption before launch. Resume rereads the
  system state. The guest screen-off policy remains a separate preference.

### Extra-key integration

- `android/keys` supplies `ExtraKeysView` and modifier state. `MainActivity.Keys` closes the
  tab list and forwards keys/text to `VtView`; `Host` exposes `ctrl`, `alt`, `shift` and
  `modifiersUsed`. Search focus does not redirect extra keys into the search field.
- `TerminalLayout` owns insets and IME translation; the key view chooses its own row layout.

### File-import integration

- `android/file-import` owns retained selection, copying and results. The activity launches
  `OpenMultipleDocuments`, enables Import from `occupied`, resumes copies only when unlocked,
  and presents/copies paths through a protected dialog. Recreation must not consume a result.

### Hardware-keyboard shortcuts

- Taken by the activity (`dispatchKeyEvent`) before the terminal sees them, together with the
  matching key release, when the key event (from a hardware keyboard, or an IME sending key events)
  has Ctrl without Alt or Meta: Ctrl+Shift+T opens a tab, Ctrl+Shift+W closes the current one,
  Ctrl+Shift+F opens the search bar (these once per press; repeats are swallowed), Ctrl+Tab and
  Ctrl+Shift+Tab show the next and the previous one, going round and repeating while held (and
  swallowed when there is nothing to switch to), and Ctrl+Shift+↑ and ↓ scroll to the previous and
  the next prompt, repeating while held (Ghostty's `jump_to_prompt` keys on Linux; on the alternate
  screen, which has no prompts, and without a terminal they reach the program). They also work with
  the search bar's text focused, and are listed in the system's keyboard shortcuts screen (Meta+/;
  the tab ones under Tabs, Find and the two prompt jumps under Terminal). The extra keys' modifiers
  do not make shortcuts: with them these keys reach the program.

### The volume keys

- While Settings has them switch tabs (`Settings.volumeKeysSwitchTabs`, on by default, read at
  each press), the activity (`dispatchKeyEvent`) takes a volume key's press when there is another
  tab to show: volume up shows the previous tab and volume down the next, going round as Ctrl+Tab
  does, once per press; the key's repeats and release are swallowed with it, so holding it neither
  switches again nor changes the volume. With a single tab (or none, as while the kernel starts) or
  the setting off, the press, its repeats and its release go to the system, which changes the
  volume; programs never get the volume keys either way (`VtView` leaves system keys alone). Open
  dialogs and the ⋮ menu are windows of their own, where the volume keys change the volume.
- A successful volume-key switch requests `KEYBOARD_TAP` feedback through the terminal view,
  matching the bars. Repeats, releases and presses left to system volume control do not request it.

### Finding text

- "Find" (⋮) or Ctrl+Shift+F, with a current tab, puts the search bar in the tab bar's place
  (the tab bar is gone, the place keeps its 32 dp, so the terminal keeps its rows) and brings the
  keyboard up on its text, which is kept from the last search and selected, so typing replaces it;
  that text is searched for again at once. Every change of the text searches anew in the tab shown
  (`VtView.search`; query, steps and scroll-bar marks are android/vtview's Search, frame progress
  and cell highlights android/vtrender's). Empty text searches for nothing and clears the count.
- No match is selected until a step: `↑`, Enter or the keyboard's Search key selects the next match
  above (towards older output; the first step selects the newest), `↓` or Shift+Enter the one
  below, going round at either end (`VtView.searchStep`). The count reads "selected/total" as the
  view reports it (`Host.onSearchCount`), the selected one counted from the newest, 0 before a step; text that is not found reads "0/0".
- `×`, Esc in the text, or Back (after Back has hidden the keyboard) closes the bar: the search
  ends and its marks go (`VtView.endSearch`), the tab bar comes back, the terminal takes the focus
  and the viewport stays where the search left it. Showing a tab (another one, Ctrl+Tab or a volume
  key while searching, or the same one again when the activity comes back, see Binding and choosing a tab) or
  a restart closes it too, as a search belongs to the tab it was opened in.
- Tapping the terminal (or pressing a mouse on it) while the bar is open gives the keyboard back to
  the terminal and leaves the bar and its marks; the extra keys always go to the terminal, the
  bar's text focused or not.
- The search bar's `↑`, `↓` and `×` click listeners request the same `KEYBOARD_TAP` feedback
  as the tab bar and extra keys; Enter, Escape and the IME's Search action bypass those listeners.

### Insets and the IME

- The content is edge-to-edge with the root padded by system-bar, display-cutout and IME insets
  (from Android 15 the window reaches into the cutout, which in landscape is at a side). As the IME
  shows or hides, the root is laid out for the end at once (the terminal regrids once, at the
  start) and the extra keys are drawn moving from their old place to the new one along the IME
  animation's curve (`TerminalLayout.keysFollowIme`), rising through the IME's room or sinking over the
  terminal's new rows; an animation that interrupts another starts from where the keys are drawn.

### Links and the clipboard question

- `onOpenUrl` receives a validated guest web URL from the service's kernel-wide browser bridge.
  With window focus it calls `TerminalLinks.openBrowser`; otherwise it returns false. Its Boolean
  acknowledges Android accepting the intent; the guest receives a failure when it is refused.
- The view reports a link to open (`Host.onLink`: a tap or Ctrl+click on one, or the menu's Open
  link; which links count and which schemes open are android/vtview's). The activity delegates to
  `TerminalLinks.open`, which opens HTTP and HTTPS links with AndroidX Custom Tabs, without binding
  a browser package or session, and other
  non-file schemes (such as mail) with plain `ACTION_VIEW`; both carry `CATEGORY_BROWSABLE`.
  `startViewer` passes the Custom Tabs launch options and returns false with a toast if no app handles the link.
  A `file:` link (`ls --hyperlink`) is looked up off
  the main thread through the Files app's provider (`Documents.fileLink`) and opened as its
  document, typed by it and with read access granted: a directory in the Files app, a file in the
  app the system picks. A link from another host, to an existing file while file access is off,
  or to nothing in the store gets a toast instead, as does a link no app takes. The disabled-file
  hint names the localized "Allow other apps to access files" setting in all five locales,
  matching Settings' consent dialog.
  The lookup result returns on the UI thread and is ignored if its activity is destroyed or locked.
- A tab's clipboard read reaches the activity through the service (`onClipboardRead`; the queue and
  the answers are `VtSession`'s, android/vt). `MainActivity` reads the clipboard's first item as
  text and, when there is some, asks ("Let the program read the clipboard?", up to 300 characters
  shown, Allow and Deny), one question at a time, whose answer covers every read queued meanwhile;
  an empty clipboard, or one without text, is answered at once (`VtSession.answerClipboardRead`).

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService`: listener callbacks (`onProgress`, `onMachineReady`, `onSessionsChanged`,
  `onBell`, `onClipboardRead`, `onTabStatus`, `onSettingsChanged`, `onNotice`, `onRestarting`,
  `onExitAll`) and the notification intent with `EXTRA_TAB`.
- android/vtview: `VtView.Host` (`onGrid`, `onInput`, `onZoom`, the extra keys' `ctrl`, `alt`,
  `shift` and `modifiersUsed`, `onLink`, `onSearchCount`).
- android/setup and android/licenses use the shared page frame, themes and view helpers;
  Settings also uses the preference switch widget, NotificationPermission and BatteryOptimization. android/privacy uses the shared theme and helpers.
- android/privacy: `onPrivacyUnlocked` resumes deferred terminal UI work.

### Outbound

- android/vtview: `VtView` (`attach`, `setFontSize`, `setFillWide`, `press` and `type` from the
  extra keys, `search`, `searchStep`, `endSearch`, `jumpToPrompt`, `bell`, `altScreen`).
- android/vt: `VtSession` (`busy`, `cwd()`, `title`, `progress`, `status`, `describe`, `clear`,
  `reset`, `answerClipboardRead`); `ProgressLine` draws the tab's `Progress`.
- `TerminalService` (`createSession`, with the current tab's directory for a new tab,
  `removeSession`, `current`, `focus`, `sessions`), `Settings.fillWide`,
  `volumeKeysSwitchTabs` and `showTabList`; `Documents.fileLink`
  (android/documents).
- `android/file-import`: retained `TerminalFiles` request/copy/result and shell-escaped paths.
- `android/tabbar`: tab bar/list and `ProgressLine`; `android/keys`: extra keys and modifiers.
- android/privacy: `Privacy.locked` guards input, clipboard, imports, browser launches and
  toasts; `Ui.showPrivate` delegates dialog-window security to `Privacy.protect`.
- Material Components 1.14.0 with AppCompat and androidx.activity (the shared themes and page
  layouts, `MainActivity`'s dialogs and menus); androidx.core's insets and animation compat;
  androidx.browser's Custom Tabs for web links; the app the system picks for a link.

## Invariants and gotchas

- Programs never receive Ctrl+Shift+T, Ctrl+Shift+W, Ctrl+Shift+F, Ctrl+Tab or Ctrl+Shift+Tab
  typed on a hardware keyboard with Ctrl held but neither Alt nor Meta (the tab shortcuts), nor
  Ctrl+Shift+↑ and ↓ on the primary screen
  (the prompt jumps, taken even when there are no prompts); the release of the shortcut's key is
  swallowed too, so the Kitty protocol sees no release without a press. Other releases are never
  matched to presses: Ctrl+Shift+C/V's, a Shift+arrow's (or Page Up or Down, Home or End) that
  moved a selection, or Esc's when the IME took the press, reach the program alone.
- A taken shortcut or volume key is remembered (`shortcutKey`) until its release, which is
  swallowed; a volume key's repeats are taken only while it is the remembered key, so a press
  left to the system keeps its repeats and release there and the volume changes as usual.
- Android honours at most 200 dp of back-gesture exclusion on each edge of a window; the tab
  bar's 32 dp, the scrolling extra keys' 32 or 64 dp and a selection handle at the edge leave room
  under it.
- `MainActivity` is a `FragmentActivity` on a Material theme: Material dialogs and AppCompat's
  `PopupMenu` take the theme, but the views it builds or inflates from XML are the framework's (no
  AppCompat inflater), so AppCompat view attributes such as `app:tint` do nothing there. The text
  selection handles `VtView` draws read framework attributes, which AppCompat forwards to the
  Material 3 colours (the handles are the primary colour). The system floating text menu has its
  own theme and resources: on S23 Android 16 its surface/text follow configuration night mode
  while its ripple follows the theme. A fixed dark theme alone does not force `uiMode` to night;
  MainActivity's early configuration override aligns them and survives configuration updates.
  Keep the override limited to night mode so locale, font scale, density and window size still update.
  Theme.Material3 does not set the system bars' colours, which Android applies up to version 14,
  so `Theme.Tuxport.Terminal` sets them black, as the framework theme had them.
- `TerminalLayout` must be built after dynamic colours are applied and assigned before attaching
  its root to the window: its view adapters access the activity's layout once input, grid and
  search callbacks begin. The activity still detaches the session on restart and destruction.
- The system lays the window out with the IME's end insets as its animation starts, when it hides
  as well as when it shows, so without a `WindowInsetsAnimation` callback the extra keys jump
  there at once. The keys rising with the IME are drawn inside the root's bottom padding, which
  already holds the IME's room, so the root must not clip to its padding (`clipToPadding` off) or
  they stay hidden until they reach the terminal. The content column and shared overlay frame
  also disable `clipChildren` so translated keys can pass their bottom edges; the tab list is
  drawn after the column and stays above the keys wherever they overlap.
- No activity handles `density`: every view is sized in dp when built. Settings and the license
  pages also recreate for dark mode (android/setup and android/licenses).
- The privacy gate is android/privacy's. Keep checks at the final input and clipboard sinks,
  including delayed IME/paste callbacks; pausing cancels activity toasts and denies the open clipboard
  question. Dialogs use `showPrivate`, and deferred import results require a resumed unlocked page.
- An activity the system recreates (a configuration change it does not handle, or after killing
  the app) or that is opened from Recents gets the intent that first started it, so `EXTRA_TAB`
  is read in `onCreate` only without a saved state and without
  `FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY`; otherwise a notification's tab would come back to the
  front. A recreated `MainActivity` drops ordinary dialogs, the search bar and the extra keys'
  modifiers/page; a retained import result is shown again after the page resumes unlocked.
- Back uses `OnBackPressedDispatcher` with `closeSearchOnBack` enabled only during search and
  `closeTabListOnBack` only while the tab list is open. List callbacks carry the session, never
  a position; the activity checks it still exists before selecting or confirming its close. Dismissing
  the list clears its data and recycled rows so it does not retain sessions closed through other controls.

## Open questions

- Should Ctrl+Shift+W ask before closing a running shell, as `×` does, and should "Close other
  tabs" with one other tab name it rather than ask "Close this tab?"?
- Should a clipboard read from a tab not in front wait until the tab is shown rather than get an
  empty clipboard, and should the question offer to remember the answer for the tab (Ghostty asks
  every time)?
- Should a clipboard question left open when the activity is destroyed be answered then? Until
  the tab's queued reads are answered it asks for no later read.
- Should typing in the search bar select the newest match at once, as Chrome's find bar does,
  rather than wait for a step, as Ghostty does? And should the extra keys act on the search text
  while it has the keyboard, rather than on the terminal?

## To verify

- Control contrast passes debug assembly and lint. Device checks remain for wallpaper palettes,
  small-label readability and selected/ordinary/locked states; indicator geometry is unchanged.

- System per-app language selection: verify all five languages and returning to the system
  default on Android 13+, including retaining the current terminal tab across recreation.
  Resource compilation, lint and translation key/placeholder checks passed for the three added
  locales. Native-speaker review, device layout checks, language switching and vendor settings
  UI remain unverified.

- Tab-list coverage of the notification banner passed API 34 checks with notifications disabled
  and enabled: the first row stays flush below the top bar, taps over the banner select a row
  without opening settings, and dismissal restores the banner's action and accessibility node.
  The covered banner is excluded from the compressed accessibility tree; TalkBack speech remains
  unverified. Debug assembly and lint passed.

- Notification permission and the dismissible banner: debug assembly and lint passed.
  API 34 covered start-aligned layout, separate accessible close action, persisted dismissal
  after relaunch/revocation, Settings first request and denial, system-settings grants and return,
  shared banner routing, direct dialog grants and tab-list suppression of both banner actions
  in the compressed accessibility tree. Held-touch screenshots confirmed feedback on each button.
  Vendor settings screens, the missing-handler fallback, Android 11–12 and TalkBack remain
  unverified for this flow.

- Tab-list coverage and hit routing over the extra keys passed API 34 checks in a short viewport;
  a tap on exposed keys below a shorter list dismisses without activating the key. Opening the
  list with the IME visible and then hiding the IME retained the terminal input connection.
  Debug assembly and lint passed; frame-by-frame IME animation clipping and TalkBack traversal
  after the hierarchy change remain unverified.

- Bar/list clicks and volume-key tab switches use the extra keys' feedback constant; their physical
  feel and device touch-feedback settings still need real-device verification.

- The terminal's night override compiles and passes lint; the system selection menu's surface,
  text and ripple on S23, including after switching system dark mode while the activity is alive,
  still need device verification. Settings and licenses should continue following the system.
- Guest launcher → Chrome CustomTabActivity → Back, plus background refusal with exit 1, were
  checked on API 34; a complete Codex OAuth login and its callback remain unverified.
- Everything here was tried only on emulators (a Pixel 4a API 34, and the outer display of a Pixel
  10 Pro Fold API 37 for the cutout and the IME), with Gboard in English and input from adb; not on
  real phones, tablets or foldables. Unverified beyond that:
  - App Links, browsers without Custom Tabs and missing
    handlers; links with other mail apps, viewers and file managers, and `mailto` at all;
  - the cutout insets and the keys following the IME with other IMEs;
  - the search bar's Back callback on Android 16;
  - the volume keys as real buttons (tried with adb key events: presses, a long press, the
    setting off and on, with three tabs; not with a single tab) and under vendor volume handling;
  - Back finishing the activity on Android 11 (a new activity's tab was tried after removing the
    task, a font scale change and a display size change on API 34).
