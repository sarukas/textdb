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
| no | a copy a keep-both conflict left, `NAME (conflict HOST DATE).ext` | `conflict-copy` (never pushed or paired; compare it, then delete or rename it) |
| no | where this directory had an asset whose pointer was moved or deleted in the store | `orphan` (sync moves it after its pointer or trashes it; never pushed but with `push --force`) |

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
  (`.NAME.HOST-PID-NANOS.tdbpart`; one a process of this computer that is no longer running left
  is removed by the next push of that path), flushed and renamed into place, and are hashed after the copy; bytes
  a push replaces move to `<root>/.textdb-trash/<yyyymmdd-HHMMSS>-<nanos>-<pid>-<n>/…`. A push
  replaces only the asset's own bytes where they are kept (its path, or the item its pointer
  names), and only when no other pointer names them (locations compared without case); anything
  else is kept and the upload goes to the asset's path, next to what is there, as
  `NAME (<first 8 of its sha256>).ext`, which the pointer's item records. Pushes of the same path
  take turns (a lock file in `<root>/.textdb-trash/locks/`, naming the process and host that
  holds it, waited on for up to ten minutes, and removed at once when that process was of this
  computer and is not running any more), holding it from reading which pointers name the
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
- **rclone driver**: runs `rclone` (a single native executable on Windows, macOS and Linux;
  `TEXTDB_RCLONE`, else the one next to `textdb` such as a vault's `.textdb/bin`, else the PATH)
  with the rclone configuration of the person running it. The store's root, shared with everyone
  using the store, must be `REMOTE:path` naming a remote of each person's own configuration
  (`teamdrive:textdb`): a connection string or options (some rclone options run programs) or a
  leading `-` or a one-letter name (a Windows drive) is refused, and a computer that needs one binds
  it locally, as it binds another remote name for the same folder. rclone runs only to push, pull
  or verify assets (`assets` commands, or a sync that pushes or pulls them) and for `assets
  stores`, which reports a root rclone cannot reach within about 30 seconds or that is not a
  folder; sync's check for store folders inside a vault looks at local stores only. rclone flags
  set through `RCLONE_*` environment variables, in any letter case, are not passed on (its
  `RCLONE_CONFIG*` settings are), and paths follow `--`. Layout, item (= path), trash, partial copies and the `beside` names are the
  local driver's, kept on the remote. An upload goes to a partial name next to its path and is
  hashed there (the provider's SHA-256 where it keeps one, such as Google Drive; else rclone reads
  it back) before it is moved into place; bytes a push replaces are first copied to the remote's
  `.textdb-trash/…` and checked. rclone removes what is at a path before a server-side move onto
  it, so the asset is briefly missing while it is replaced: the path is hashed after the move, and
  when it holds other bytes or none the push fails and what was there is copied back from the
  trash (when the provider cannot be asked, after retries, a move rclone reported done counts).
  Every copy and move passes `--ignore-times`: rclone otherwise skips a copy onto a file of the
  same size and time, and a move then deletes the new bytes and keeps the old. A pull refuses a
  folder where the asset's file should be. rclone cannot create a file only when none is there, so
  pushes take turns through lock files (`<hash of the path>-MILLIS-HOST-PID-N.lock`) in
  `.textdb-trash/locks/`: a push writes its own only when a listing shows no other of that path,
  and holds the lock when two listings a moment apart show its file alone; where several wrote at
  once, the push that started first keeps its file and the others remove theirs, and a push waits,
  longer each time, while another file is listed. This relies on the provider listing what was
  just written. Lock files, partial copies and failed trash copies are deleted outright, not into the
  provider's trash; a lock of a process of this computer that is not running any more is removed,
  and one that could not be removed is reported. A Google document (no bytes) is never an asset.
  The local and rclone drivers' locks do not see each other: one team should reach a store
  through one driver. Still to come: provider item ids, SharePoint's rewriting of Office files on upload, and
  changes made directly in the drive (see stage 3 below).

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
textdb assets status [PATH] [--dir DIR]            ok / new / modified / outdated / conflict / not-pulled / conflict-copy / invalid-pointer / invalid-path
textdb assets push [PATH…] [--dir DIR] [--to NAME] [-m MSG] [--dry-run]   upload, verify, then commit pointers
textdb assets pull [PATH…] [--linked-from PATH] [--dir DIR] [--dry-run]   download, check the hash, put in place
textdb assets verify [PATH] [--dir DIR]            hash local files and the asset store's copies (exit 1 on problems)
textdb assets gitignore [PATH] [--dir DIR] [--dry-run]   write the managed .gitignore block
textdb assets migrate-from-git [PATH] [--dir DIR] [--to NAME] [-m MSG] [--dry-run]   move the binaries git tracks to the asset store
textdb sync PREFIX DIR [--push] [--pull]           documents, then pointers paired with their files, then assets pushed / pulled
```

`migrate-from-git` refuses to start while anything is staged in git (exit 6), pushes the vault's
assets whose files git tracks, writes the `.gitignore` block, removes from git's index those now
in the asset store (the files stay on disk), and commits their pointers and `.gitignore` in one
commit; the vault may be a subfolder of the checkout. What stays in git: an asset whose pointer
the user's own `.gitignore` lines ignore (`pointers_ignored`), one that fails to push (exit 1),
and one in any state but `new`, `modified` or `ok` (`blocked`: a conflict, outdated, orphan,
conflict copy or invalid name; exit 3). When the commit itself fails (a hook, say), what was
staged for it is unstaged again and the pushed assets stay pushed, so a later run starts over.
Git's history keeps the old blobs. `--json` gives `{ dry_run, tracked_assets, bytes, to_push,
in_store_already, blocked }` for a dry run and `{ dry_run, tracked_assets, pushed, migrated, bytes,
pointers_ignored, blocked, conflicts, failed, gitignore_changed, commit }` otherwise.

`DIR` defaults to the directory a store folder was last synced with; with `--dir`, `PATH` must be
in the folder that directory was synced with, and only a directory never synced takes `PATH` as
the store folder it holds. `push` publishes `new` and `modified` assets (`conflict` ones too with
`--force`), refuses an asset whose pointer on disk differs from the store's (sync first) or whose
pointer changed in the store during the push, exits 3 when something was left for a conflict,
keeps an asset's id when its bytes change, and records the pointers it wrote in the directory's
sync base, as sync would. One asset failing does not stop the others. `pull` fetches `not-pulled`
and `outdated` assets, leaves `modified` and `conflict` files alone, and never puts bytes whose
hash differs from the pointer in place. A pointer is never pulled into `.git`, `.textdb`,
`node_modules` or another place the rules leave out (it is `invalid-path`), and a pointer that only
names a file that is no asset (a document, a file the rules leave out) is a `conflict` whose file is
never hashed or described: anyone who can write to the store must not be able to plant a git hook
or learn about other files through a pointer, and `push --force` never uploads a file a pointer
only names. For the same reason sync never writes a store file inside `.git`, `.textdb`,
`node_modules`, `.trash`, `.textdb-trash`, `__pycache__` or the system folders textdb leaves out
(`.obsidian` excepted: its settings sync on purpose), whatever the name's letter case, trailing
dots or spaces, stream, or 8.3 short name (`GIT~1`) — such a file is reported as skipped. These
checks take a name as Windows would resolve it, so `.git.` and `GIT~1` are `.git`. `verify` counts an asset store it cannot reach as a
problem. `status --json` gives `{ prefix, dir, assets, counts }`, each asset with `path`, `state`,
`type` (the pointer's media type, or one from the name), and where known `size`, `store`,
`sha256` (the pointer's), `version` (the pointer's in the store), `file` (its name on disk,
relative to the directory) and `note`.

Which files are candidates at all: besides the rules above, textdb's own and system files are
never assets — `.git`, `.textdb`, `.trash`, `.textdb-trash`, `.obsidian`, `node_modules`, OS
folders, `kb.db*`, `.DS_Store`, `._*` AppleDouble files, `~$*` and `.~lock.*` lock files, and
partial downloads (`.crdownload`, `.part`, `.tmp`, …). Git's `binary` marks a file nothing else
decides as an asset; `-text` does not. The managed `.gitignore` block is written first in the
file, so the user's own lines after it take precedence, and it keeps directories matched by asset
rules visible so the pointers inside them stay in git. Store-side namespace
operations take the asset's real path: `textdb mv /a/arch.png /b/arch.png` moves the pointer, and
the next sync moves the real file on disk of every directory synced with the folder; the asset
store keeps the bytes where they were put, which the pointer's item still names. `textdb rm
/a/arch.png` deletes the pointer, and the next sync moves the real file to `.textdb/trash/`.
Moving and trashing items in the asset store itself is still to come (stage 3, second part).

## Sync

Pointers sync like any document: they are taken in whatever `--ext` says, and files the rules or
their bytes make assets never are, whatever `--ext` says. A file (document or pointer) deleted or
moved away in the store since the last sync and made again at the same path is told apart by its
content, not its version number. Around that, sync pairs pointers with their real files:

1. A pointer moved in the store (matched by its id) → the real file moves with it on disk. When a
   file is already at the new place, the real file stays and is reported (`orphan` until
   resolved).
2. A pointer deleted in the store → the real file goes to
   `.textdb/trash/<yyyymmdd-HHMMSS>-<nanos>/…` when it holds the bytes the pointer named, and
   the pointer leaves disk only once the file is there, so a move that fails is tried again by
   the next sync; a file with other bytes is kept and reported, as a new asset; a file that
   cannot be read keeps its pointer until a later sync can read it.
3. A real file renamed on disk without its pointer (in Obsidian, say) → an asset file with no
   pointer, matched by size and sha256, one to one, with a pointer unchanged on both sides whose
   file is missing. What the directory last had decides: it had the pointer's file (not the
   other), so the pointer is moved in the store and on disk; it had the other file (not the
   pointer's), so the pointer moved in the store earlier and the file is moved after it now; a
   copy it had neither of, or one already there at the last sync, is left alone, and a pointer
   whose file was already missing at the last sync is never renamed. A possible new name that
   cannot be read is looked at again by the next sync. Real files are
   found in any letter case on Windows and macOS, and what a directory last had follows its
   pointers' moves and deletes, made on disk or in the store. Asset store folders inside the directory are never
   candidates. Links are left to what renamed the file, as with the other moves sync makes.
   A name changed only in letter case is the same asset on Windows and macOS: nothing is paired,
   and a case-only rename in the store renames the files in place.
4. Both sides changed an asset (the store's pointer names new bytes, and the file here is neither
   those nor the bytes the pointer on disk named) → the file is renamed
   `name (conflict <host> <yyyy-mm-dd>).ext` and the store's bytes are pulled to its path. The
   copy is `conflict-copy`: never pushed, never paired, never the start of another conflict.
5. After the documents, assets are pushed with `--push` and pulled with `--pull`. Without either,
   the store's setting `asset_sync` decides (`off` by default, `push`, `pull` or `both`), and
   `asset_pull` says which are pulled: `linked` (the default: what the notes in the folder link
   to) or `all`. A push takes no assets while the `.gitattributes` files differ from the ones the
   base recorded (an id of them is kept with its rules), sync after sync, and says so, until a
   sync with `--accept-rules` records them. The store's bytes of an asset a conflict copy stands
   next to are pulled by every sync until they are in place.
6. The report (`assets` in `--json`): `mode`; `counts` by state; `trashed` and `trash` (the
   folder); `renamed` and `conflict_copies` (`{from, to}`); `pushed`, `pulled` and `conflicts`;
   `failed`; `notes`. `trashed`, `renamed` and `conflict_copies` are paths relative to the
   directory, `pushed` and `pulled` store paths. It is there when the folder or directory has
   pointers or an asset store is declared. After the sync, assets that failed to push or pull
   exit 1, and assets left for a conflict exit 3 (conflict markers in documents come first).

The folder-move carry and the `left behind` report stay for the other files textdb ignores.

## Links

A link target that is not a document resolves to `<target>.tdbasset` when that pointer exists:
status `ok` (or `ambiguous`), `resolved` = the real path and `asset: true` (the `links` SQL view
has the same `resolved` and an `asset` column); the kind stays as written, and an anchor
(`#page=3`) is not checked as a heading. `backlinks` takes the asset's path. `links --broken --dir`
also reports `not-pulled` for assets whose real file is missing. Link rewriting on moves covers
assets and writes the asset's name, never the pointer's.


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

First part (done): the `rclone` driver: push, pull and verify through any rclone remote, with
the local driver's layout, trash, partial copies and names beside, provider SHA-256 where there is
one (else read back), locks by lock files and listing, rclone found through `TEXTDB_RCLONE`, next
to `textdb` (a vault's `.textdb/bin`) or the PATH, and CI running the tests against rclone's local
backend.

Second part: provider item ids and remote renames by id, SharePoint's rewriting of Office files
(tracking the provider's version tag instead of comparing hashes), moving and trashing items in
the asset store when their pointers move or go, and detection of changes made directly in the
drive. These need real Google Drive and SharePoint accounts to build and test against.

### Stage 4 — Web app

For the folders the web server syncs (`TEXTDB_SYNC`), through the CLI as for sync:

- The folder view lists a pointer `NAME.tdbasset` as `NAME`, with its type, the asset's size and a
  state badge (ok, changed, outdated, conflict, not pulled, …), refreshed when a pointer in the
  folder changes or something in it moves or goes. A file with no pointer yet has no row: `textdb
  assets push` or a sync that pushes gives it one. Opening it shows the asset: its state, type, size and asset store; a preview of an image,
  PDF, audio or video file; a download of anything else; Pull or Push when its state calls for
  one; Rename and Delete (of the pointer; the next sync moves or trashes the file); and the
  pointer's versions with their authors and messages.
- The sync dialog lists what a sync did with assets: pushed, pulled, renamed, trashed, set aside,
  and its conflicts, failures and notes.
- Server: `GET /api/assets?prefix=&path=` (the CLI's status), `POST /api/assets/pull` and
  `POST /api/assets/push` (`{ prefix, paths?, message?, author? }`, taking the folder's turn with
  its syncs), `GET /api/assets/file?prefix=&path=[&download=1]`: the asset's file from inside the
  folder's directory only, `nosniff`, sandboxed unless a PDF, and only images, PDF, audio and video
  inline (SVG and anything else as a download). All of them only once the folder has been synced
  (before that the CLI would take a path inside it for the folder). A file is sent only when it is
  the asset's own: in state ok, modified or outdated (the pointer's bytes, or bytes this directory
  had as that asset), or new, orphan or a conflict copy (a file the rules make an asset); never a
  file a pointer merely names, since anyone who can write a pointer to the store could otherwise
  read any file of the directory through it. Writes are JSON only (`Content-Type:
  application/json`), so another site's page cannot trigger them; at most four CLI runs go at once. An asset not pulled is pulled before it can be
  seen: the server never reads the asset store for a preview.

## Out of scope for now

Extracted text from PDF/Office for search; thumbnails; placeholder files or mounts; binary
deltas; distributed copy-count enforcement; advisory locks; history rewrite of existing git
repositories (documented as a separate, explicit step).
