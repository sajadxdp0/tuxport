# Tuxport website

A single page in `dist/index.html` links to the English privacy policy in
`dist/privacy.html`. The page is a minimal dark one-pager that mirrors the
app's Neon Terminal theme: the flat near-black `#0d1117` base, the app's
own `favicon.svg` as the prompt mark anchoring the hero, a short
headline with two actions, and a highlighted fact panel whose labels
carry the app's magenta secondary and whose cyan primary drives hovers,
focus and selection. Inline CSS, a local SVG favicon and wordmark font,
no JavaScript, no remote assets, no dependencies, no compilation step.
The pages are written in modern plain CSS: custom properties,
`color-mix()` hairlines, grid and flex layout, and `clamp()` fluid type
and spacing. The homepage is one left-aligned fluid column with no layout
breakpoints; the policy keeps a 42rem reading column and print styles.

`dist/` contains the directly maintained website source and is committed to Git.
It is also the directory uploaded for deployment; no generation or copying is needed.

Open `dist/index.html` directly in a browser, or preview from the repository root:

```sh
python3 -m http.server 8000 --bind 127.0.0.1 --directory website/dist
```

Visit <http://127.0.0.1:8000> for the homepage or
<http://127.0.0.1:8000/privacy.html> for the policy. Edit `dist/privacy.html` to update
the policy and its effective date.

For static hosting, GitHub Pages publishes the site through the
[Website workflow](../.github/workflows/pages.yml): it uploads `website/dist` as a
Pages artifact and deploys it. The site is served at
<https://sajadxdp0.github.io/tuxport/> with the policy at
<https://sajadxdp0.github.io/tuxport/privacy.html>, the URL the Android app's
About → Privacy policy link opens. The workflow runs when `website/**` changes
and can also be dispatched by hand.

## Screenshot

`docs/screenshots/terminal.png` is a capture of the running app from the
debug build on an emulator, shown in the project README. When the app's look
changes, replace it with a fresh capture of the same session. The site
itself shows no image: the prompt mark is its only visual.

## Favicon

`dist/favicon.svg` mirrors the Android launcher foreground: the terminal
prompt as solid geometry, white on a `#0d1117` rounded tile. The chevron is
a uniform stroke with a miter tip, the underscore a filled bar, matching the
notification icon's glyph.
A `26 25 56 56` viewBox removes the adaptive-icon padding so the mark
remains legible at favicon sizes.

Maintain the SVG directly. When the Android icon changes, update the favicon as
needed and review its crop and appearance at small sizes. Deployment only copies
the SVG; it does not need Python or the Android source tree.

## Wordmark font

`dist/fonts/tuxport-mono.woff2` is the app's terminal typeface (JetBrains
Mono, `android/app/src/main/assets/fonts/JetBrainsMono.ttf`) subset to the
glyphs these pages use, so the wordmark and terminal-voice text render the
same everywhere without a remote request; `dist/fonts/OFL.txt` is its
licence and must ship beside it. The font stack puts `JetBrainsMono Nerd
Font` first, so machines that have it use their own copy. Regenerate the
subset when the pages' glyphs change:

```sh
pyftsubset android/app/src/main/assets/fonts/JetBrainsMono.ttf \
  --output-file=website/dist/fonts/tuxport-mono.woff2 --flavor=woff2 \
  --unicodes="U+0020-007E,U+00A9,U+00B7,U+2014,U+2197" --layout-features=""
```

Keep the `@font-face` family name distinct from "JetBrains Mono", the
licence's reserved name, and keep the subset's glyph set in step with the
text the pages actually render.

## Content maintenance

Keep the policy's repository link current if the project moves. The privacy page
names the maintainer and points to the project's GitHub issues for support requests.

The policy describes the current Android implementation. Recheck these sources
when behavior changes:

- `AndroidManifest.xml`, the merged release manifest and Gradle dependencies:
  permissions, including AndroidX biometric permissions, and included SDKs.
- `Rootfs.kt`: automatic mirror probes, download destinations and repository protocols.
- `Diagnostics.kt` and `Tabs.kt`: local data, exported diagnostics and retention.
- `Privacy.kt`, `Documents.kt`, `TerminalFiles.kt` and terminal clipboard handlers:
  authentication, grants, imports and clipboard access.
- `res/xml/data_extraction_rules.xml`: backup and transfer exclusions.

These paths are under `android/app/src/main/` unless they refer to build inputs or
generated output. Developer identity and the support link were supplied by the
maintainer. The support-retention wording uses a purpose-based period: resolve the
request and related follow-up, then delete unless legal retention applies. Keep
actual support handling consistent with that policy.

The current Debian repository configuration uses HTTP even though the built-in
mirror probes and image download use HTTPS. The policy discloses this; do not claim
universal transport encryption without reviewing the guest configuration.

## Publishing a release

Build instructions are in `android/README.md`. Upload the signed APK/AAB to
<https://github.com/sajadxdp0/tuxport/releases>; the app's About → Version link opens
that page. Tuxport is not published on Google Play, so keep the privacy policy at a
stable public URL before distributing a release.

Editing this page does not deploy the site or create a GitHub release.
