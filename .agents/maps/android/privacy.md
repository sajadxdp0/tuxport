# android/privacy Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/privacy.md` for the paired decision history (not yet created; the pre-split boundary is D4 in `.agents/decisions/android/terminal.md`; privacy decisions not yet distilled into that snapshot remain in f61072f3 Decision 1 and c7309c0c Decision 3 with their android/terminal tags).
>
> Verified: 2026-11-14 (split out of the android/terminal full mapping of 2026-11-14; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: repoint dependencies to the split module maps)

## Responsibilities

`android/privacy` owns the app-wide authentication gate, process-local unlock state, secure
activity and dialog windows, the locked-page shield, fresh authorization for diagnostic exports,
and the activity that authenticates private notification report copies.

Root: `Privacy.kt` and `NotificationActivity.kt` in
`android/app/src/main/java/io/github/sajadxdp0/tuxport/`, and `layout/privacy_shield.xml`,
`drawable/privacy_emblem.xml` and `drawable/ic_lock.xml` under `android/app/src/main/res/`.

Outside the module: shared strings, themes and `Ui.kt` (including the `showPrivate` wrapper)
remain android/terminal's; application startup is android/native's; manifest and authentication dependency are
android/build's, instrumentation tests belong to android/tests. Settings' controls and export handoff are
android/setup's; preferences, notification redaction/routing and diagnostic archives are
android/service's; provider denial and grant revocation are android/documents'. Call sites keep
their own last-moment input, clipboard, browser and export checks.

## Key types

- `Privacy` — process-local authentication and epoch, with a `Gate` per FragmentActivity and a
  `Request` ViewModel holding that activity's authentication purpose, epoch snapshot and completion.
  `ExportAuthorization` scopes fresh log-export approval; `NotificationActivity` authenticates
  private report-copy actions before finishing.
- `Purpose` — separates ordinary unlock, enabling, disabling and fresh diagnostic-export
  authentication; ordinary page unlock never delivers an `ExportAuthorization`.

## Public entry points

- `Privacy.install` registers the process lifecycle gate and screen-off receiver from
  `MainApplication`; `locked` is the shared predicate used by protected operations.
- `requestMode`, `requestLogExport`, `canExport` and `deferNextUnlock` serve Settings' mode and
  export actions; `Listener.onPrivacyUnlocked` and `ExportListener.onLogExportAuthorized`
  return control to their activity.
- `protect` registers dialog windows; `addListener` and `removeListener` notify mode consumers.
- Non-exported `NotificationActivity` receives private report-copy intents, delegates the copy
  to `Notifications.copyReport` after unlock, clears its intent and finishes.

## Data flow / lifecycle

### Authentication, protected operations and exports

- `Privacy.requestLogExport` always requests a fresh `EXPORT_LOGS` authentication when privacy
  is enabled, even if the page was already unlocked. Only success on a resumed, available
  device with the matching epoch delivers `ExportAuthorization` to `ExportListener`.
  Settings checks that epoch, the current privacy setting and foreground/device availability
  before generation and again before sharing or handing a prepared save to the document picker.
  Save's process-local pending file reference authorizes only that archive's transfer when the
  picker returns, even though the page locks while backgrounded; it cannot generate another ZIP.
  Its result may call `deferNextUnlock`: while the gate is paused this suppresses automatic unlock
  on its next resume without revealing protected content. This flag does not survive gate recreation.
  Ordinary page unlock never starts an export. Authentication purpose survives configuration
  recreation in the existing ViewModel, not process death. Cancellation, failure or screen-off
  invalidation before the handoff cannot export; an intentional handoff may complete while locked.
- `MainApplication` (`android/native`) installs `Privacy` on every FragmentActivity, including Settings, licenses and the non-exported `NotificationActivity`. Default-off `privacy_mode` accepts AndroidX Biometric 1.1.0 strong biometrics or device credentials; enabling and disabling both authenticate, enabling after a confirmation.
- Authentication lives only in memory. The last stopped activity locks and covers pages unless recreating; overlapping internal navigation preserves it. Screen-off invalidates pending authentication and locks immediately. Pausing alone leaves visible content in place; resumed locked pages authenticate or offer Unlock/Leave after cancellation or unavailability.
- Each activity's ViewModel retains the prompt's purpose/generation through recreation, never through process death. Successful callbacks complete only when resumed with an interactive, unlocked device; report-copy/clipboard/input checks also refuse access while no page is resumed.
- Enabled pages and all app dialogs use `FLAG_SECURE`; `showPrivate`/`Privacy.protect` track dialog windows weakly and update existing ones on changes. The shield also covers restored dialog windows. Terminal input, clipboard answers, browser launches and title toasts check the gate; pausing cancels title toasts and denies an open clipboard question.
- `privacy_shield.xml` presents a decorative lock, heading, authentication hint, filled Unlock button and text Leave button using the covered page's theme. Its scrollable content centers within system insets, has 24 dp horizontal/32 dp vertical padding and caps the inner column at 360 dp as the window resizes; text and buttons can grow with the font scale.
- Only report-copy actions created during privacy mode enter `NotificationActivity`, never the exported launcher. It copies after the gate opens and then finishes. Normal report copying uses a non-exported receiver; exit, download cancellation and running-policy actions run directly in the service even while locked. Background programs, saved terminals and rootfs data are retained; this is access protection, not encryption or history removal.

## Dependencies (inbound / outbound)

### Inbound

- android/native: `MainApplication` installs the gate; android/build registers the action activity.
- android/terminal: `MainActivity` and `TerminalLinks` check the gate; `Ui.showPrivate` registers
  dialogs before showing them; the activity resumes deferred work through `onPrivacyUnlocked`.
- android/setup: Settings requests mode changes and fresh export authorization, rechecks it
  before collection/handoff and protects restored dialogs.
- android/vtview: copy/paste access and preview dialogs. android/licenses' activities receive
  the same automatic lifecycle protection as every other FragmentActivity.
- android/service: mode listeners, clipboard guards and private report-copy action intents.

### Outbound

- android/service: `Settings.privacyMode`/`setPrivacyMode` and `Notifications.copyReport`;
  android/documents: `revokeAccess` and `rootsChanged` when mode changes.
- android/terminal: shared localized strings, themes, `dp` and `themeColor` helpers.
- AndroidX Biometric 1.1.0, FragmentActivity/ViewModel lifecycle, Android's device-lock and
  interactive-state checks, secure window flags and notification manager.

## Invariants and gotchas

- Unlock state is process-local; saved state or preferences cannot unlock a page. Retained
  authentication purpose and epoch survive activity recreation only, never process death.
- With privacy enabled, a visible paused page need not be covered yet, but protected operations
  require a resumed page. The last stop and screen-off cover pages; screen-off invalidates pending authentication.
- Dialogs have separate windows: register them with `Privacy.protect` through `showPrivate`
  (or directly for restored dialogs), rather than relying on the activity's secure flag.
- Ordinary unlock must not replay an export. An authorization is checked before collection and
  handoff; only the already-prepared save may finish across the external picker while locked.
- Enabling privacy clears external file access and revokes grants; disabling does not restore
  them. Background programs and stored data are retained, not encrypted or erased.

## Open questions

None recorded in the parent mapping for this concern.

## To verify

- Fresh diagnostic-export credential authentication, cancellation, wrong PIN and screen-off
  invalidation were checked on API 34; ordinary unlock did not replay an export. Real biometric
  hardware and vendor prompt behavior remain unverified.
- Privacy credential enable/disable, background return, cancellation/retry, screen-off on Settings and notification actions were checked on API 34; cold/recreated locked pages and dialog flags have instrumentation coverage. Real biometrics, concurrent windows and vendor authentication/window behavior remain unverified.
