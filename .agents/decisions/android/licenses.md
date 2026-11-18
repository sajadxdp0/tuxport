# Notices UI (android/licenses) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/licenses"`
>
> Last distilled: 2026-11-18 (HEAD = ec9ad94b)
>
> Decisions made before android/licenses was split from android/terminal retain their `android/terminal` tags in `.agents/decisions/android/terminal.md`: D3, D4, D13, the license-page part of D50, D55 and D58 (bd580227 Decision 1).

## Active

### D1: Catalog navigation and offline reading form a notices UI module

- **What**: android/licenses owns LicenseCatalog, LicensesActivity, LicenseActivity, list_item.xml and license IDs, with shared UI resources in android/terminal and packaged notices in android/build.
- **Why**: Reading and catalog navigation change independently of terminal operation.
- **Tradeoffs**: Reader changes can require both the shared UI and packaging maps, and pre-split runtime decisions remain in the parent; packaging and instrumentation belong to android/build and android/tests respectively.
- **Watch out**: The split inherits renderer-recovery, zoom, large-font and tablet verification gaps.
- **Source**: 9c6c51a7; d053be24; 60790c14; 39e55b4b
