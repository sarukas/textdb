# Assets — design

Status: accepted, in implementation (stages below). Decisions from the discussion of 2026-09-14/15.

## Problem

A knowledge base is 85–90 % text (notes, front matter, links) and 10–15 % binaries: images,
diagrams, PDFs, Office documents, recordings. textdb versions, merges, searches and links the
text. Binaries are skipped today, which causes drift:

- a move, rename or delete in the store leaves attachments behind on disk;
- links to images and PDFs can only be `not-in-store`, so a present file and a missing one look
  the same;
- a fresh clone or another teammate's vault gets the notes without their attachments;
- binaries committed to git bloat the repository, and users keep private copies elsewhere.

Loading binaries into the database is wrong: they do not merge, chunk or diff, and they multiply
database and backup size.

## Model

```
 GitHub (git)                 shared Postgres store (textdb)          shared drive (asset store)
 notes *.md                   notes, versions, links                  accounts/acme/arch.png
 accounts/acme/arch.png.tdbasset   ◄──── pointers are documents ────►  (mirrors the vault layout,
 .gitattributes, .gitignore   asset stores, what each vault saw        addressed by item id)
                    ▲                         ▲                                   ▲
                    └────────── a user's vault on disk: real files in place ──────┘
                                accounts/acme/arch.png   (git-ignored, pulled from the drive)
                                accounts/acme/arch.png.tdbasset
```

- **Text stays in git and in textdb.** Binaries leave git and live in a shared *asset store*
  (Google shared drive, SharePoint document library, or a plain folder) whose layout mirrors the
  vault, so people can browse it.
- **Every asset has a pointer**: a small text file next to the real file, `<name>.tdbasset`,
  committed to git and held in textdb as an ordinary document. Everything textdb does for
  documents — paths, versions, moves with history, the change feed, base-version conflicts,
  sync, the shared Postgres store — applies to pointers unchanged.
- **The working directory stays ordinary.** The real file sits at its real path, so Obsidian,
  Explorer and Finder work with textdb uninstalled. No symlinks, placeholders or git filters.
- **textdb is the authority on names and moves**; the asset store holds bytes; a vault holds a
  materialised copy of the assets its user pulled.

Principles carried from the research: path ≠ identity (asset id), hash ≠ availability (a pointer
says what the bytes are, not that this vault has them), extraction ≠ version; upload and verify
bytes before publishing a pointer; deletes go to a trash, never a bare unlink; binaries in
conflict are kept both, visibly.

## Pointer files

`accounts/acme/diagrams/arch.png.tdbasset`:

```
textdb-asset: 1
id: 01926d1e-7b4a-7c1e-9d2f-3a5b6c7d8e9f
sha256: 9f2c…(64 hex)
size: 184233
type: image/png
store: team-drive
item: 1AbCdEf…
```

- One `key: value` per line, LF line ends, keys in this order; readers accept CRLF and keep
  unknown keys (a later version may add some). `textdb-asset` is the format version.
- `id`: UUIDv7, assigned at first push, never reused; survives rename, move and content change.
- `sha256`: of the raw bytes. `size`: bytes. `type`: media type from the extension.
- `store`: the name of an asset store configured in the textdb store. `item`: the provider's
  stable item id; for a local-folder store, the store path the bytes were put at, so a pointer
  moved in the store still finds them until a push or stage 2's sync moves them too.
- Later stages add provider metadata lines (`provider-hash`, `provider-version`) for cheap
  change detection; they are not part of identity.
- The real file is the pointer's path without `.tdbasset`. The pointer changes only when the
  bytes (or where they are kept) change, so its version history is the asset's history, in
  textdb and in git.

States of an asset in a vault. What the directory last had of each asset (the sha256 it pushed,
pulled or found matching the pointer) is remembered per vault, and tells a file changed here
from a pointer changed elsewhere:

| Pointer | Real file | State |
|---|---|---|
| yes | same sha256 | `ok` |
| yes | missing | `not-pulled` (pull fetches it) |
| yes | changed since this directory had the pointer's bytes | `modified` (push publishes it) |
| yes | what this directory last had; the pointer names newer bytes | `outdated` (pull replaces it, the old copy kept in `.textdb/trash/`) |
| yes | other bytes, not what this directory last had | `conflict` (`push --force` replaces the store's copy; or move it aside and pull) |
| no | present, classified asset | `new` (push publishes it) |

A file whose name differs from a pointer's only in case is that asset (on Windows and macOS it
is the same file); a second file differing only in case is a `conflict`. A pointer whose path
cannot be a file on every system is `invalid-path`.

## Which files are documents, assets or ignored

Git's attribute syntax, in the vault's `.gitattributes`, with a `textdb` attribute:

```
*.md        textdb=document
*.png       textdb=asset
*.pdf       textdb=asset
*.log       textdb=ignore
```

- Built-in defaults: `md markdown mdx txt canvas base` are documents; common binary formats
  (images, PDF, Office, archives, audio, video, fonts, drawio/excalidraw exports) are assets;
  the store's own files (`kb.db*`, `.textdb/`) and `.git/` are always ignored. The vault's
  `.gitattributes` overrides the defaults (last matching line wins, as in git).
- A file no rule mentions is sniffed as git does: a NUL byte in the first 8 000 bytes makes it an
  asset; otherwise it is left to git and textdb ignores it (JSON, code, HTML) unless a rule says
  `document`.
- `sync --ext` keeps working as a list of document extensions for a command.
- The rules (with a hash of `.gitattributes`) are recorded with each sync base, so a change is
  reported before it takes in or pushes files unnoticed.
- `textdb assets gitignore` keeps a managed block in `.gitignore` that ignores the asset patterns
  and the vault's `.textdb/trash/`, and never `*.tdbasset`. Where the patterns get a file wrong (a
  document under a `binary` rule, an asset only its bytes show) the block names that file; run
  it again as files come and go. Push notes pushed assets git does not ignore.

## Asset stores

Declared in the textdb store (shared by the team through Postgres), bound per machine:

- Store table `asset_store(name, driver, root, options)`: `driver` ∈ `local`, `rclone`; `root` is
  the store-side identity (a folder, or an rclone remote path such as `teamdrive:textdb`).
- Machine binding, because the same shared drive is mounted or configured differently per user:
  environment `TEXTDB_ASSET_STORE_<NAME>` (name upper case, other characters `_`), else
  `asset-stores.json` in the config directory (`TEXTDB_CONFIG_DIR`, else `%APPDATA%\textdb`,
  `$XDG_CONFIG_HOME/textdb` or `~/.config/textdb`), written by
  `textdb assets stores --bind NAME=G:\Shared drives\Team\textdb`. A local store without a
  binding uses its root when that is an absolute path. `textdb assets stores` shows each binding
  and whether the store is reachable.
- Layout: the asset at store path `/accounts/acme/arch.png` is kept at `<root>/accounts/acme/arch.png`.
- **local driver**: plain filesystem operations (works for a NAS, a USB disk, and Google Drive for
  Desktop / OneDrive clients in mirror mode). Item = path. Copies go through a hidden partial file
  (`.NAME.PID-NANOS.tdbpart`), flushed and renamed into place, and are hashed after the copy; bytes
  a push replaces move to `<root>/.textdb-trash/<yyyymmdd-HHMMSS>-<nanos>-<pid>-<n>/…`. A push
  replaces only the asset's own bytes where they are kept (its path, or the item its pointer
  names), and only when no other pointer names them (locations compared without case); anything
  else is kept and the upload goes to the asset's path, next to what is there, as
  `NAME (<first 8 of its sha256>).ext`, which the pointer's item records. Pushes of the same path
  take turns (a lock file in `<root>/.textdb-trash/locks/`, naming the process and host that
  holds it, waited on for up to ten minutes), holding it from reading which pointers name the
  path until their own pointer is committed, so a push reusing bytes and one replacing them never
  cross;
  what a push replaces is hard-linked into the trash and checked before the new copy replaces it
  in one step, so the asset is never missing from its path, and bytes put there some other way
  are kept. Within one push, what it has placed counts as in use for the assets after it. An
  asset store folder inside the vault is left out of the vault's assets; a vault inside an asset
  store's folder is refused.
- Hashes of local files are cached per computer and directory in the local cache directory
  (`TEXTDB_CONFIG_DIR/cache`, else `%LOCALAPPDATA%\textdb`, `$XDG_CACHE_HOME/textdb` or
  `~/.cache/textdb`; `assets-*.json`, written atomically), by size and modification time, and only
  for files not modified in the last two seconds; `verify` never uses the cache.
- **rclone driver** (stage 3): runs `rclone` (a single native executable on Windows, macOS and
  Linux) for copy, move, delete (to the provider's trash) and listing with hashes and item ids.
  Google shared drives report SHA-256; SharePoint only its QuickXorHash and rewrites Office files
  on upload, so for those textdb tracks the provider's version tag instead of comparing hashes.

What a vault last had of each asset (to tell a local edit from a remote one) is kept in the same
cache, keyed by host and directory and never roaming with a profile (a cache an older build kept
in the config directory is taken over; processes saving at once merge what each learnt). Losing it is safe: a file
that differs from its pointer is then a `conflict` until it is pulled or pushed with `--force`.
Push records the pointers it wrote in this directory's sync base for the vault's folder only (the
base's time and other files stay), refuses a pointer the store deleted since that sync, and checks
the pointer's version before uploading as well as before committing.

## Commands

```
textdb assets stores [--add NAME [--driver local|rclone] --root ROOT] [--remove NAME] [--bind NAME=LOCATION]
textdb assets status [PATH] [--dir DIR]            ok / new / modified / not-pulled / invalid-pointer
textdb assets push [PATH…] [--dir DIR] [--to NAME] [-m MSG] [--dry-run]   upload, verify, then commit pointers
textdb assets pull [PATH…] [--linked-from PATH] [--dir DIR] [--dry-run]   download, check the hash, put in place
textdb assets verify [PATH] [--dir DIR]            hash local files and the asset store's copies (exit 1 on problems)
textdb assets gitignore [PATH] [--dir DIR] [--dry-run]   write the managed .gitignore block
textdb assets migrate-from-git [--dir DIR] [--dry-run]   push git-tracked binaries, git rm --cached, commit (stage 2)
```

`DIR` defaults to the directory a store folder was last synced with; with `--dir`, `PATH` must be
in the folder that directory was synced with, and only a directory never synced takes `PATH` as
the store folder it holds. `push` publishes `new` and `modified` assets (`conflict` ones too with
`--force`), refuses an asset whose pointer on disk differs from the store's (sync first) or whose
pointer changed in the store during the push, exits 3 when something was left for a conflict,
keeps an asset's id when its bytes change, and records the pointers it wrote in the directory's
sync base, as sync would. One asset failing does not stop the others. `pull` fetches `not-pulled`
and `outdated` assets, leaves `modified` and `conflict` files alone, and never puts bytes whose
hash differs from the pointer in place. `verify` counts an asset store it cannot reach as a
problem.

Which files are candidates at all: besides the rules above, textdb's own and system files are
never assets — `.git`, `.textdb`, `.trash`, `.textdb-trash`, `.obsidian`, `node_modules`, OS
folders, `kb.db*`, `.DS_Store`, `._*` AppleDouble files, `~$*` and `.~lock.*` lock files, and
partial downloads (`.crdownload`, `.part`, `.tmp`, …). Git's `binary` marks a file nothing else
decides as an asset; `-text` does not. The managed `.gitignore` block is written first in the
file, so the user's own lines after it take precedence, and it keeps directories matched by asset
rules visible so the pointers inside them stay in git. Store-side namespace
operations take the asset's real path: `textdb mv /a/arch.png /b/arch.png` moves the pointer (and
records the intent); the next sync or push moves the real file on disk and the item in the asset
store. `textdb rm` deletes the pointer; sync moves the real file to `.textdb/trash/`, push moves the
item to the provider's trash.

## Sync

Pointers sync like any document. Around that, sync pairs pointers with real files:

1. A pointer moved in the store → the real file moves with it on disk.
2. A pointer deleted in the store → the real file goes to `.textdb/trash/<timestamp>/…`.
3. A real file moved on disk without its pointer (renamed in Obsidian) → found by sha256 among
   pointers whose real file is missing, 1:1 only; the pointer is moved in the store (links in notes
   were already rewritten by Obsidian or are rewritten by `link_updates`).
4. New or modified real files are listed (`assets new`, `assets modified`); with an asset store
   bound and `--push` (or the setting `asset_sync = push`) they are pushed as in `assets push`.
5. Pointers without a real file are listed as not pulled; `--pull` (or `asset_sync = pull|both`,
   with `asset_pull = linked|all`) downloads them.
6. Both sides changed an asset (pointer sha256 changed in the store, real file changed on disk):
   the store's bytes are pulled to the real path and the local bytes are kept next to it as
   `name (conflict <host> <yyyy-mm-dd>).ext`, reported, never pushed automatically. A name with
   that suffix never starts another conflict.

The folder-move carry and `left behind` report stay for files textdb ignores.

## Links

A link target that is not a document resolves to `<target>.tdbasset` when that pointer exists:
status `ok` (or `ambiguous`), `resolved` = the real path and `asset: true` (the `links` SQL view
has the same `resolved` and an `asset` column); the kind stays as written, and an anchor
(`#page=3`) is not checked as a heading. `backlinks` takes the asset's path. `links --broken --dir`
also reports `not-pulled` for assets whose real file is missing. Link rewriting on moves covers
assets and writes the asset's name, never the pointer's.

Until stage 2 pairs them, sync carries pointer files like any document (they are eligible whatever
`--ext` says, and are not part of the recorded include rules), and leaves real files alone.

## Stages

Each stage: implement with tests, adversarial review by independent agents (correctness and data
loss; Windows paths and filesystem; Postgres), fix findings, docs and skills, commit, push to the
stage branch, CI green, then fast-forward `main` and push.

### Stage 0 — Postgres parity

Bring the Postgres extension and the CLI's Postgres store level with SQLite before anything is
built on top, and make CI run Postgres for real (until now CI only built the extension).

- **Shared link rules.** Resolution (Obsidian's rules, statuses, heading anchors), candidate
  choice and the rewrite of a link's target on a move move out of the SQLite binding into
  `textdb-md`, behind a small lookup trait each backend implements, so SQLite and Postgres
  cannot drift apart.
- **Schema.** `kb.link` gains `id, kind, anchor, alias, external, target_name, resolved_id,
  status`; `kb.commit` / `kb.change` gain `batch`; `kb.sync` gains `rules`; the missing indexes.
  The extension has no upgrade scripts (INSTALL.md: re-create), and no Postgres store has been
  deployed, so the schema changes in place; upgrade scripts come with the first deployment.
- **Links.** Link rows written with their parts; resolved at commit; re-resolved on create, move
  and delete; `link_updates` (off/report/rewrite) accepted by `kb.set_setting`; `kb.move` reports
  or rewrites links in the same transaction; `links`, `backlinks` and `mv --update-links` work.
- **Writes.** Messages on edit, append, line replacement, move and delete inside the write's
  transaction; several line ranges in one commit; `kb.replace` / `kb.replace_many`; batches
  (session setting `textdb.batch`) on commits and changes, and `revert-batch`; search snippets
  from the best line.
- **CLI Postgres store.** No stubs left for the above; `sql` binds `:author`, reports the batch
  and dry-run diffs; the `links` and `commits` views have the SQLite columns; `history` reports
  sizes.
- **Tests.** `#[pg_test]` tests in the extension, run by `cargo pgrx test` in CI, and the CLI's
  Postgres scenarios (links, moves, meta, line ranges, SQL batches, sync) run against a Postgres
  16 with the extension installed.
- Deferred: trash, purge and `textdb_entry` (used only by the SQLite web app).

### Stage 1 — Assets core

Classification rules and `.gitattributes` parsing; pointer format (parse, write, validate); asset
store table and machine binding; local driver; `assets stores | status | push | pull | verify |
gitignore`; links resolve to pointers; SQLite and Postgres.

### Stage 2 — Assets in sync and namespace operations

Sync pairing (moves, deletes to trash, Obsidian renames, keep-both conflicts), `--push` / `--pull`
and the `asset_sync` settings; `mv`/`rm` by real path; recorded rules include `.gitattributes`;
`assets migrate-from-git`.

### Stage 3 — rclone driver

Google shared drive and SharePoint through rclone: item ids, provider hashes and version tags,
the Office-file exception, provider trash on delete, remote renames by item id, detection of
changes made directly in the drive; rclone shipped next to `textdb.exe` in a vault's `.textdb/bin`.

### Stage 4 — Web app

Assets in the folder view (real name, type, state), image and PDF previews and downloads served by
the server from the vault or through the driver, pull and push actions, asset history from pointer
versions; docs and skills.

## Out of scope for now

Extracted text from PDF/Office for search; thumbnails; placeholder files or mounts; binary
deltas; distributed copy-count enforcement; advisory locks; history rewrite of existing git
repositories (documented as a separate, explicit step).
