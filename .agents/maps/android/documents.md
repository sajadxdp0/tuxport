# android/documents Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/documents.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 1 research concern; 5 claims checked by 1 independent verifier — 5 confirmed; bugs and debts recorded)
>
> Maintained: 2026-11-14 (targeted verification only since the full mapping; latest change: privacy mode overrides file access for every provider operation)

## Responsibilities

`android/documents` is the `DocumentsProvider` that shows the guest's files in the system Files
app and in other apps' pickers:

- the roots, both behind default-off file access: the login account's home (writable) and the
  whole system (read-only outside the home);
- document IDs, columns, flags and MIME types;
- reading, writing, creating, deleting, renaming, moving and searching through the store's browse
  API, without the kernel;
- change notifications in both directions;
- the documents that the terminal's `file:` links name.

Root: `android/app/src/main/java/io/github/sajadxdp0/tuxport/Documents.kt` (and its manifest entry).

Outside the module: the store-side browse implementation, real-path checks, naming and ownership
of new files (`kernel/store`); the poll that delivers guest changes (`android/service`); account
resolution and the store lock (`android/setup`); the JNI calls (`android/native`).

## Key types

- `Documents : DocumentsProvider` — roots `home` and `system`; search limit 200.
- `Home(account, uid, gid, path)` — the resolved home root, cached 5 s per login setting.

## Public entry points

- Framework: `queryRoots`, `queryDocument`, `queryChildDocuments`, `openDocument`,
  `createDocument`, `deleteDocument`, `renameDocument`, `moveDocument`, `isChildDocument`,
  `findDocumentPath`, `querySearchDocuments` (no `copyDocument`, `removeDocument`, recents or
  thumbnails).
- Static, for the service: `rootsChanged`, `dirsChanged`, `revokeAccess`; for the terminal: `fileLink`, with its
  result `FileLink` (`Found` with the document's URI and MIME type, `Remote`, `Hidden`, `Missing`).

## Data flow / lifecycle

### Roots and IDs

- With file access enabled and a ready rootfs, the home comes from the login setting through `Rootfs.loginAccount`
  (`/etc/passwd` read from the store; root with `/root` when unreadable) and is shown only if it
  exists and is not `/` (which would make the whole system writable). Home root: summary
  "account · distro", flags local-only, is-child, search, create (making it a "Save to" target),
  free space from `getAllocatableBytes`. System root `/`: same without create, with or without
  a home root. `Settings.fileAccess` uses `file_access`, default false, and always refuses access during privacy mode; old `system_documents` is ignored. Enabling privacy clears file access and revokes grants; disabling privacy restores neither.
- Document IDs are absolute guest paths with `%`, control characters and non-UTF-8 bytes written
  as `%XX`; only canonical IDs (no empty, `.` or `..` components) count. With file access off,
  neither root nor any document is accessible; IDs under the home are writable when enabled, the home itself only
  as a create or move target.
- Columns: ID, display name (decoded, U+FFFD for bad bytes), MIME type, size (files), modified
  time, flags from the literal path (home: create; under home: delete, rename, move, plus write or
  create; elsewhere none).
- MIME: by extension (`MimeTypeMap`); files without a known extension are sniffed in one batch
  (first 4 KB without NUL and valid UTF-8 → `text/plain`, at most 1000 per call); otherwise
  `application/octet-stream`.
- Document paths (`findDocumentPath`, which the Files app needs to open a directory it is sent
  to): the IDs from the home root's document down to a visible document under it, else from the
  system root's `/`, or from a given parent; a document not under the parent is not found.

### Operations

- Every call holds `Rootfs.access`'s read lock and opens the store afresh per JNI call; errors
  surface as `FileNotFoundException`.
- Read: the host object file opened read-only and adopted as the returned fd.
- Disabling gates queries, search, ancestry/path lookup, opens and changes even for old URIs.
  The service calls `revokeAccess` on the authority URI to revoke persisted and transient document/tree
  grants, then refreshes roots. Already-open fds and copied data cannot be recalled; calls already
  past the access check may finish. Re-enabling does not restore revoked grants.
- Write modes: the native side checks the real path against the home, truncates if asked, opens
  with `O_APPEND` for append modes and returns an fd; the provider hands that fd out wrapped with a
  close listener (`ParcelFileDescriptor.wrap`, run on a `documents-close` thread) that reports the
  write, so the kernel sees `IN_MODIFY` and `IN_CLOSE_WRITE`; data goes directly into the host
  object.
- Create: names sanitised (`/` and NUL to `_`; empty, `.`, `..` to `_`), an extension added from
  the MIME type for files; owned by the home account, 0644/0755, numbered on collision. Delete,
  rename (no overwrite, no extension added) and move are limited to the home by real path; each
  revokes grants on the source ID and, among the persisted grants other apps hold on this
  provider (`getOutgoingPersistedUriPermissions`), those on documents below it (`grantedUnder`: a
  tree URI by its tree's document), and notifies the affected directories. The framework revokes
  a URI with those below it by path segment, and an ID is a single segment, hence the listing.
- Search: by name under the root, breadth first (200 results, 50,000 entries).

### Links from the terminal

- `fileLink` (called off the main thread) takes a `file:` URI from a program's OSC 8 link. Its host
  must be empty, `localhost` or the name in the store's `/etc/hostname` (any case), else `Remote`.
  The path's `%XX` escapes become bytes, written back as an ID the way the native side writes
  paths, empty and `.` components dropped; a relative path or one with `..` is `Missing`. The
  document's type comes from the provider itself (`getType`, so what is shown now applies and
  extensionless text is sniffed): `Found`; otherwise `Hidden` when the store has a directory or
  regular file there, else `Missing`.

### Notifications

- Provider to kernel: the store's change path replays the changes under each running kernel's lock
  (guest inotify events).
- Kernel to Files app: the service's 2 s poll passes directories the guest changed to
  `dirsChanged`, which notifies their child-document URIs in one call; `rootsChanged` follows
  login and setting changes and each successful kernel start.
- With no rootfs `queryRoots` returns nothing and other calls fail; during a rootfs replacement the
  write lock holds provider calls until the store is gone.

## Dependencies (inbound / outbound)

### Inbound

- The framework (manifest provider: exported, grant URI permissions, `MANAGE_DOCUMENTS`);
  `TerminalService` (`rootsChanged`, `dirsChanged`); `MainActivity` (`fileLink`, granting the
  viewer read access to a `Found` document); `MachineTest`.

### Outbound

- `Native.browse*`; `Rootfs` (`isReady`, `installed`, `access`, `loginAccount`); `Settings`
  (`login`, `fileAccess`; android/service).

## Invariants and gotchas

- When enabled, writes are limited by real path; reads follow
  symlinks wherever they point, so an app holding a grant on the home tree can read outside it
  through a link the guest user made (an accepted limitation: no guest permission checks apply).
- Flags shown in the Files app are a hint; the real write check is in the native code.
- `fileLink` writes IDs in Kotlin by the same rules as the native `encode_path` (`%`, Unicode
  control characters and bytes outside well-formed UTF-8 as `%XX`); the two change together. A
  differing ID would still reach the file (the native side decodes any `%XX`) but not compare
  equal to the home's ID for write flags and path lookup.
- A link's host is matched against `/etc/hostname` in the store, not the running system's name:
  after `hostname` alone, without the file, the machine's own links count as `Remote`.
- If wrapping a write fd fails (the socket pair that carries the close cannot be created), the
  plain fd is handed out and the guest gets no `IN_MODIFY`/`IN_CLOSE_WRITE` for that write (the
  data still lands). `wrap` detaches the fd before creating that pair, so after a failure the fd
  is still open and is adopted again by number.

## Confirmed bugs / technical debt

- **Transient grants outlive moved or deleted directories.** Grants given for an activity (opening
  a file in another app) cannot be listed, so those on a moved or deleted directory's descendants
  last until that activity ends, and since IDs are paths they cover a file created at the same
  path meanwhile; persisted grants are revoked.

## Open questions

- Should `/etc/passwd` changes (an `adduser` while the login is "auto") refresh the roots?

## To verify

- Persisted and picker-issued grant revocation on a device; explicit document/tree grants were
  revoked on API 34, with all six provider tests passing, including disabled queries, reads and changes.
- `fileLink` and `findDocumentPath` on real devices and other file managers; tried only on a Pixel
  4a API 34 emulator with Google's Files app, HTML Viewer and Chrome.
