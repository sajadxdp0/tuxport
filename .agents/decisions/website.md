# Static website (website) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: website"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)

## Active

### D1: The homepage and English privacy policy ship as static files

- **What**: Maintain both HTML pages and the SVG favicon directly in website/dist, deploy that directory through Cloudflare Pages Direct Upload, and exempt the public contact address from email obfuscation.
- **Why**: The landing page and public policy need a stable location and a simple publishing process.
- **Tradeoffs**: Changes to the English-only policy, icon and production site require manual edits or deployment.
- **Watch out**: Reconcile the policy with Android behavior and release declarations, and check hosting logs, regional access and browser layouts.
- **Source**: 5b213f09
