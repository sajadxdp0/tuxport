# website Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/website.md` for the paired decision history (not yet created).
>
> Verified: 2026-11-17 (three source-research concerns and nine independently checked claims; static checks passed, external release and visual checks remain below)
>
> Maintained: 2026-11-18 (targeted verification only since the full mapping; latest change: documented the Settings About policy link and removed the missing-entry debt)

## Responsibilities

`website/` owns Tuxport's minimal homepage, English privacy-policy page and maintenance
and local-preview instructions. Android modules own the behavior described by the
policy; changing this page does not change permission enforcement or data handling.

## Key types

No runtime classes or application state. `dist/index.html` contains the homepage and
`dist/privacy.html` contains the policy, each with inline CSS; `README.md` explains
preview, content maintenance and publication. `dist/favicon.svg` is a directly maintained
asset adapted from the Android launcher foreground and colors.

## Public entry points

- `dist/index.html`, opened directly or served as the static site's index; its hero
  shows `dist/terminal.png`, a capture of the running app.
- `dist/privacy.html`, reached through the homepage's relative Privacy Policy link or
  opened directly.
- Both pages reference the local `favicon.svg` with an SVG icon link and share the
  app's dark palette: an ink-dark base with one cyan accent and quiet hairlines.
- The site is published to GitHub Pages by the Website workflow, served at
  `https://sajadxdp0.github.io/tuxport/`.
- Android Settings About opens `https://sajadxdp0.github.io/tuxport/privacy.html`
  through its shared Custom Tabs link action; the entry is implemented in `android/setup`.
- The privacy page names Sajad as the maintainer and points support requests to
  `https://github.com/sajadxdp0/tuxport/issues`.

## Data flow / lifecycle

The browser renders each document with a local SVG favicon, without JavaScript,
forms, remote assets or font downloads. The homepage contains the app name, a one-sentence description and
the policy link. Content changes require editing the corresponding HTML. Preview
uses Python's standard HTTP server bound to loopback with `website/dist/` as its
document root; production hosting is outside this module. The public files live
directly in the Git-tracked `dist/` directory, with no build or copy step. Pages
Direct Upload takes that directory; Git integration uses root `website`, output
`dist` and build command `exit 0`. The maintenance README stays outside the site root.
The current project uses Direct Upload via Wrangler with production branch `main`;
local edits require another deployment and are not automatically published from Git.

The favicon retains the Android foreground paths, exterior clip and colors, with
no background. The body's two facial
cutout contours are also drawn in solid black over the white even-odd body. A
fixed `26 25 56 56` viewBox removes adaptive-icon padding. Maintainers edit the SVG
directly; serving and deployment need neither Python nor Android.

The ten policy sections cover local processing, diagnostics and support, network
destinations, Android permissions, security, retention/deletion, choices, changes
and contact. The policy distinguishes automatic mirror probes, local diagnostics,
user-directed sharing and network traffic from guest programs.

## Dependencies (inbound / outbound)

### Inbound

- `android/build`: source/merged permissions, dependencies and backup exclusions.
- `android/setup`: mirrors, system downloads, repository protocols and Settings.
- `android/service`: diagnostics, terminal restoration and background execution.
- `android/privacy`, `android/documents`, `android/file-import`, `android/terminal`:
  authentication, file grants, selected imports and clipboard interactions.
- Maintainer-supplied developer identity/contact and actual support-mail handling.
- `android/terminal`: launcher foreground vector and `values/colors.xml` are the
  design sources for favicon geometry and colors, with no automatic synchronization.

### Outbound

- Browser HTML/CSS support and system fonts; mail handler on contact-link activation.
- A separately configured static host determines public availability and server logs.
- Google Play's User Data and Data safety requirements inform publication; the README
  links the official guidance reviewed for this version.

## Invariants and gotchas

- Keep the English language attribute and one main heading per page, and the policy's
  section label references. Both pages use a 42rem reading column, narrow-screen
  spacing, long-word wrapping and visible link focus; the policy has print adjustments.
- Keep `dist/` in Git: it contains maintained source files, not generated output.
- Preserve the `email_off` comments around the public contact link; Cloudflare's
  default email obfuscation otherwise rewrites it and injects a decoding script.
- Review `dist/favicon.svg` after Android launcher changes and update it directly as
  needed; check the fixed crop and appearance at small sizes when geometry changes.
- Review dependency-added permissions in the merged release manifest, including
  biometric permissions. Do not infer the complete list from the source manifest.
- Local diagnostic capture is automatic; external diagnostic sharing is user-directed.
  Local terminal restoration data is separate from diagnostic exports.
- Normal service shutdown clears saved terminal state unless restoration from an earlier
  killed run has not succeeded; the policy preserves this retention exception.
- Built-in mirror probes and image downloads use HTTPS, but the current default Debian
  package sources use HTTP. Do not promise encryption for all guest traffic.
- Support retention is purpose-based policy wording, not a code-enforced timer:
  resolve the request and related follow-up, then delete unless legally required.
- A policy page does not establish public hosting, Play Console declarations or
  Google Play approval. Recheck behavior and dates against the final release.

## Confirmed bugs / technical debt

None currently recorded.

## Open questions

- What request logging and retention apply to the Cloudflare-hosted site?
- What are the mirror operators' retention and IP-address processing practices for the
  release's Data safety declarations?

## To verify

- Production HTTP access without sign-in was checked from the deployment environment;
  accessibility from other regions, the Play Console policy URL and final Data safety
  declarations have not been verified.
- The existing merged release manifest was inspected, not rebuilt. Verify the final
  release artifact and its SDK behavior against the policy before submission.
- HTML structure, permission coverage and local HTTP delivery passed checks. Favicon
  source geometry checks passed; the transparent icon and black
  facial details were inspected at 16, 32 and 128px on light, dark and checkerboard
  backgrounds in desktop Chrome. Other browsers' favicon rendering and mobile,
  keyboard-focus and print layouts remain unverified.
