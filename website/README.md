# Tuxport website

A minimal homepage in `dist/index.html` links to the English privacy policy in
`dist/privacy.html`. Both use inline CSS and a local SVG favicon with no JavaScript,
remote assets, dependencies or compilation step. The pages use Tuxport's Neon Terminal
palette: a `#0d1117` base with cyan and magenta accents.

`dist/` contains the directly maintained website source and is committed to Git.
It is also the directory uploaded for deployment; no generation or copying is needed.

Open `dist/index.html` directly in a browser, or preview from the repository root:

```sh
python3 -m http.server 8000 --bind 127.0.0.1 --directory website/dist
```

Visit <http://127.0.0.1:8000> for the homepage or
<http://127.0.0.1:8000/privacy.html> for the policy. Edit `dist/privacy.html` to update
the policy and its effective date.

For static hosting, use `website/dist/` as the site root; no build step is needed.
GitHub Pages can publish it with an Actions workflow that uploads `website/dist` as a
Pages artifact. Until that is configured, the Android app's About links point to the
policy file in this repository, which is reachable as soon as the repository is public.

## Favicon

`dist/favicon.svg` is adapted from the Android launcher foreground vector and
`res/values/colors.xml`. It retains the light body, neon-cyan outer stroke and clipping,
uses a transparent background, and fills the two facial cutouts with the dark base.
A `26 25 56 56` viewBox removes the adaptive-icon padding so the symbol remains legible
at favicon sizes.

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
