# Tuxport website

A single page in `dist/index.html` links to the English privacy policy in
`dist/privacy.html`. The page is a minimal dark one-pager in a small-site
style: the app's own `favicon.svg` as the logo in the header and the hero,
a terminal transcript captured from the running app, and four quiet fact
lines. Inline CSS, a local SVG favicon, no JavaScript, no remote assets,
no dependencies, no compilation step. Both pages share the app's palette,
near-black `#0d1117` with one cyan accent, written in modern plain CSS:
custom properties, grid and flex layout, `clamp()` fluid type and spacing,
and mobile-first responsive breakpoints. The terminal cursor blinks unless
`prefers-reduced-motion` is set.

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
itself shows a terminal transcript instead of an image.

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
