# The demo app: a live corpus browser and editor

A local web app over one textdb SQLite store. People browse, read, edit, import, reorganise
and clean up a markdown knowledge base in the browser while agents change the same store
from the command line; every change from anyone appears in every open browser as it lands,
attributed and versioned.

It is a Node server (`node/apps/server`, HTTP + server-sent events over
[`@textdb/node`](../node/packages/textdb)) and a React + CodeMirror UI (`node/apps/web`).
The HTTP API and the client semantics are specified in [`live-app.md`](live-app.md).

## What it does

| Area | What you get |
|---|---|
| **Tree** | Folders and files, loaded lazily and virtualised; a dot marks what just changed and who changed it. Clicking a folder opens it in the folder view (its arrow only expands). Right-click, the row's `⋯` button or the menu key opens Open, Download, Replace with a file, Rename or move (`F2`) and Delete (`Del`) |
| **Folder view** | The centre shows the open folder GitHub-style: a path bar (breadcrumbs; click beside them or press `F4` to type a path, with completion — text that is not a path filters the folder), and a table of its files and subfolders that scrolls through any number of entries, loading 200 at a time. Sort by name, type, size, lines, words, versions, created, updated or authors; choose columns. A folder's figures are totals of everything below it. The filter takes a name or glob plus `author:`, `type:` and `is:file`/`is:folder`; *Subfolders* lists everything below; *Contents* searches the text of the folder's files. Select with the checkboxes, `Space`, `Shift`/`Ctrl`-click or `Ctrl+A`, then move or delete the selection in one transaction. Changes from others update rows in place; new, removed or reordered rows wait behind a *N changes · Refresh* button. Keys: arrows, `Enter` opens, `Backspace` or `Alt+↑` goes up, `F2`, `Del`, `Shift+F10` |
| **Document** | *Preview* (rendered markdown; *Chunks* shows the content-defined chunk boundaries), *Edit* (CodeMirror; `Ctrl/Cmd+S` saves against the version you started from, so concurrent commits are rebased or merged, and a real conflict opens a panel with base, theirs and yours), *History* (versions with author, time and how each landed, the renames, moves and deletes between them, any version read-only, any two diffed unified or split). Header buttons: Download, Replace…, Rename…, Delete… |
| **Live changes** | A remote commit to the open document is applied in place — in the editor on top of your unsaved changes — and flashed with the author's colour and `agent-7 · v12 · rebased`. A moved document follows its file; a deleted one says so |
| **Activity** | Every change in the store, newest first; *Others only* hides your own; an import collapses to one row |
| **Search** | `/` focuses it: terms are ANDed per document, `"phrases"` and `prefix*` work; results jump to the line |
| **Import folder** | Pick a folder (Chrome and Edge read it lazily with the directory picker; other browsers take a whole-folder file input), choose the destination folder, tick the file types to import from those actually found in the folder (with file counts and sizes, likely-binary types flagged; or type extensions), and watch progress. Unchanged files make no new version; binary, non-UTF-8, unreadable (for example online-only cloud files) and oversized files are skipped with the reason |
| **Export** | *Export…* in the folder view, or *Export to disk…* on a folder's menu. Chrome and Edge write into a folder you pick: a plan first lists what is new, changed and already identical on disk (same size, then SHA-256), and only new and changed files are written, byte for byte, with progress and *Stop*. Nothing on disk is deleted, so exporting over a git checkout that was imported shows only the real changes. Names that cannot coexist on your system — differing only in letter case (Windows, macOS) or Unicode normalization (macOS), Windows reserved names or characters, clashes with what is already on disk — stop the export and are listed; problems only on other systems are warnings. Other browsers download a zip, after the same name check |
| **Sync** | For folders the server is set up to sync (`TEXTDB_SYNC`), *Sync…* in the folder view and a line saying when it was last synced, at which git commit, how many files changed here since and any conflicts. The dialog plans first — what goes to disk, what comes into textdb, merges, conflicts, name problems — then syncs, optionally committing what landed on disk to git. It runs `textdb sync` on the server, so it behaves exactly as the [CLI](cli.md#syncing-with-a-git-checkout): git authors credited, conflict markers written to disk. Each conflicted file can be viewed and resolved by keeping the textdb or the disk side of its conflicts (merged edits around them stay), which syncs again; or edit the file and sync. On a first sync of a git checkout, the commit the store came from can be given so changes merge instead of conflicting |
| **Assets** | In synced folders, binaries kept in an [asset store](assets.md) show under their own names (the pointer `NAME.tdbasset` without its suffix) with their type, size and a state badge: ok, changed, outdated, conflict, not pulled (a file with no pointer yet has no row until it is pushed). Opening one shows its state and asset store, a preview of an image, PDF, audio or video file (anything else downloads), *Pull* or *Push* when its state calls for one, *Rename…* and *Delete…* of its pointer (the next sync moves or trashes the file), and the versions of its pointer. The sync dialog lists what a sync pushed, pulled, renamed, trashed or set aside. It runs `textdb assets` on the server, with that user's asset store bindings |
| **Asset stores** | *Assets…* in the header (the owner's). The stores this textdb store declares -- name, driver and the root everyone knows them by -- kept apart from the **binding**, which is where *this server's machine* reaches each one and is written to this server's own config file. Declare, remove (refused while any pointer names its files, with the reason) and bind, with whether this machine can reach each store. Then the assets of a synced folder: state, size, type, store, and *Pull all*, *Push all*, *Relocate* and *Verify* over the folder. *Where?* on one asset shows the whole chain -- its path here, what its pointer says (hash, size, type, version), the store, that store's root, this machine's binding, the item inside it (a path, or the id a drive keeps its file under) and the file on the server's disk |
| **Headings and links** | With a document open, the right-hand column carries two zones above the activity feed: **Outline**, its heading tree with each section's own words and the total under it, and **Links**, what the document links to -- with each link's status and where it resolves (an asset shown as the asset) -- and what links to it, grouped by the document it comes from. Every row opens the document at that line, and either zone folds away. Outside a document, *Headings* is the fourth way of looking inside a folder, beside *Names*, *Contents* and *Properties*: every document's outline one after another, an index of the headings in use on the left (who else has *Next steps*, by prefix or by a `contains` that says it scans), and a *Links* tab listing what reaches nothing -- `broken`, `anchor-missing`, `ambiguous` -- grouped by the document to fix it in. See [outlines.md](outlines.md) |
| **Access** | The chip in the header is who this session is: the owner, or the account whose token it presented, with the shares it holds and the rights it holds them by. Paste a token to work as an account -- every path is then that account's own, and a read-only share is marked `ro` in the listings. *Access…* is accounts, shares and tokens (the owner's, and an `admin`-kind token's — the store decides each command, not this app): create an account, grant a folder `ro` or `rw` under an alias, rename or revoke a share, mint a token (its bearer is shown once) and revoke one. The rules are the store's, so what an account may not do arrives as the store's refusal rather than a hidden button. See [permissions.md](permissions.md) |
| **Rename, move, delete** | Files and whole folders. A delete shows what it takes and, past 50 files, asks for the folder's name |
| **Trash** | A system folder at the bottom of the tree. Deleted files and folders stay browsable, a deleted file readable at any of its versions and downloadable; *Permanently remove* an entry or *Permanently clean trash*, both confirmed. Purging frees content no other file or version shares |
| **Download / replace** | Download a file (the version shown). Replace a file with one from your computer: the dialog shows how many lines it adds and removes, then commits it as the next version |
| **Author** | The name in the header is recorded on your saves, imports, moves and deletes |

Deep links: `#/guides/` opens a folder (`#/` the root), `#/guides/intro.md` a file,
`#/guides/intro.md:42` scrolls to line 42, `#trash:8144` opens a trashed file. The browser's
Back and Forward buttons move between them. Sort order and chosen columns are remembered per
browser.

## Requirements

- **Rust** (stable) to build the SQLite loadable extension (and the `textdb` CLI).
- **Node.js 24 or newer**: the server uses `node:sqlite` with extension loading.
- A browser; folder import is best in a Chromium browser (Chrome, Edge).

## Build

From the repository root:

```sh
(cd crates/textdb-sqlite-ext && cargo build --release)   # the extension, found automatically by the server
(cd node/packages/textdb && npm install)
(cd node/apps/server && npm install)
(cd node/apps/web && npm install && npm run build)       # node/apps/web/dist, served by the server
cargo build --release -p textdb-cli                       # optional: the CLI for agents (target/release/textdb)
```

Build the extension inside `crates/textdb-sqlite-ext`: that crate has its own target
directory (`crates/textdb-sqlite-ext/target/release/`), which is where the server looks.

## Start

### On a directory you synced

`textdb sync` pairs a directory with a folder of a store and records it in its `.textdb/config`, so
everything the server needs is already on disk. One command reads that pairing and starts on it:

```sh
cd node/apps/server
npm run open -- /path/to/vault          # or `npm run open -- .` from inside it
```

It prints the directory, the folder and store it is paired with, and the URL of that folder's page,
and opens a browser there (`--no-browser` to skip, `PORT=…` for another port). The pairing comes
from `textdb config --json`, which reports it for the directory it is run in — nothing here parses
`.textdb/config`, which is the CLI's file to change.

### On a store

```sh
cd node/apps/server
TEXTDB_DB=/data/kb.db npm start          # textdb server listening on http://127.0.0.1:4317 (store /data/kb.db)
```

Windows `cmd`:

```bat
cd node\apps\server
set TEXTDB_DB=C:\data\kb.db
npm start
```

PowerShell: `$env:TEXTDB_DB = 'C:\data\kb.db'; npm start`. Open <http://127.0.0.1:4317>.

The store is created if it does not exist and upgraded in place if an older build wrote it.
Stop the server with `Ctrl+C`.

### Configuration

The server takes no command-line flags; it is configured by environment variables.

| Variable | Default | Meaning |
|---|---|---|
| `TEXTDB_DB` | `./kb.db` (relative to where you start it) | The SQLite store |
| `TEXTDB_SQLITE_EXT` | `crates/textdb-sqlite-ext/target/release/textdb_sqlite_ext.dll`, `libtextdb_sqlite_ext.so` or `.dylib` | The loadable extension, if it is somewhere else |
| `PORT` | `4317` | HTTP port |
| `HOST` | `127.0.0.1` | Interface to listen on |
| `TEXTDB_REQUIRE_TOKEN` | unset, and **on whenever `HOST` is not loopback** | `1` to answer nothing without a bearer. Without it, a request with no token is **the owner** — which is what anyone able to open the store file is anyway. A server listening beyond loopback turns it on for you, so that deployment needs an account per person and, for anyone who is to sync or configure asset stores, an `admin`-kind one: there is no bearer that means "the owner" otherwise |
| `TEXTDB_SYNC` | none | Folders the web UI may sync with directories on the server's machine: `/folder=directory` pairs separated by `;`, e.g. `/handbook=C:\src\handbook;/notes=/home/me/notes`. Requests never name a directory; only these can be synced. Sync and every asset route are **the owner's**: a token session is refused them, because they work on this machine's directories and drives. So anyone who can reach the API as the owner can sync (and, with *Commit*, commit to) them, see their asset files (only files that are an asset's own, never another file a pointer names) and push them to the asset store |
| `TEXTDB_CLI` | `target/release/textdb` (or `target/debug`) in the repository | The `textdb` CLI that runs the syncs and the asset pulls and pushes (`cargo build --release -p textdb-cli`); it uses the asset store bindings and rclone configuration of the user running the server |

Settings that belong to the store, not the server, apply to every client of that store —
the web app, the CLI, SQL:

| Store setting | Default | Change it with |
|---|---|---|
| `path_history` — record renames, moves and deletes in the history of everything they touch | `on` | `textdb setting path_history off` · `SELECT textdb_setting('path_history', 'off')` · `PUT /api/setting {"key": "path_history", "value": "off"}` |

### Server scripts (`node/apps/server`)

| Command | What it does |
|---|---|
| `npm start` | Run the server |
| `npm run dev` | Run it with `--watch`: restarts when the server's source changes |
| `npm run open -- DIR [--no-browser]` | Start on a synced directory, working out its store and folder from its own `.textdb/config` (via `textdb config --json`), and open the browser on that folder |
| `npm run seed -- --db kb.db --files 2000 [--seed 1] [--batch 250]` | Write a deterministic synthetic markdown corpus (areas like `guides/`, `runbooks/`, `teams/`; frontmatter, headings, lists, code) in transactions of `--batch` files |
| `npm run agent-sim -- --db kb.db --path /guides/intro.md [--interval 1500] [--author agent-sim]` | A simulated agent in a separate process: every `--interval` ms it reads the file and edits a line against the version it read, so you can watch remote edits land |
| `npm test` · `npm run typecheck` | Tests (a real server on a temporary store) · type check |

`--path` must start with `/`; in Git Bash set `MSYS_NO_PATHCONV=1` first.

### Web development (`node/apps/web`)

| Command | What it does |
|---|---|
| `npm run dev` | Vite on <http://localhost:5173> with hot reload, proxying `/api` to `TEXTDB_API` (default `http://localhost:4317`) — start the server separately |
| `npm run build` | Type check and build `dist/`, which the server serves |
| `npm test` · `npm run typecheck` | Unit tests (vitest) · type check |

## A five-minute tour

```sh
cd node/apps/server
npm run seed -- --db ../../kb.db --files 2000
TEXTDB_DB=../../kb.db npm start
```

1. Open <http://127.0.0.1:4317>, set your name in the header. The root folder is listed in the
   centre: sort by *Words*, tick *Subfolders* and scroll through every file; type
   `author:seed type:md` in the filter. Open a file from the tree or the table.
2. In a second terminal, start an agent on that file — the simulator, or a real one with the
   CLI (see [Working with an external agent](cli.md#working-with-an-external-agent)):

   ```sh
   export TEXTDB_STORE=kb.db TEXTDB_AUTHOR=agent-7 MSYS_NO_PATHCONV=1
   textdb cat -n guides/some-file.md --lines 1:20          # note the version in the header
   textdb replace-lines guides/some-file.md 12 12 -b 3 --text $'Changed by the agent.\n'
   ```

   Windows `cmd` (from the repository root, where `seed` wrote `kb.db`):

   ```bat
   set TEXTDB_STORE=%CD%\kb.db
   set TEXTDB_AUTHOR=agent-7
   target\release\textdb cat -n /guides/some-file.md --lines 1:20
   target\release\textdb edit /guides/some-file.md --old "some unique text" --new "changed by the agent"
   ```

3. Switch to *Edit*, type something without saving, and let the agent edit again: its change
   appears in place and yours survives. Save; the result says how it landed.
4. *History*: pick a version, tick two to diff. Rename the file from the tree (`F2`) and see
   the rename appear between the versions.
5. Delete a folder, open *Trash*, read a file in it, then *Permanently remove* it.
6. *Import folder…* in the header: pick a folder of markdown and watch the progress.
7. Edit one of the imported files, then *Export…* in that folder's view into the original
   folder: the plan shows one changed file and the rest identical; `git status` there shows
   just that file.

## Limits and behaviour worth knowing

- **Import** reads files in the browser and sends them in batches of at most 4 MiB or 500
  files; eight files are read at a time, a file not read within 60 s or larger than 64 MiB is
  skipped, and the server accepts at most 5000 files per request. Nothing is sent until you
  confirm the destination.
- **Export** round-trips bytes exactly: textdb stores what the import read, CRLF and BOM
  included, and never converts. Git settings that convert line endings on checkout
  (`core.autocrlf`, `.gitattributes` `eol=`) apply as for any edit made on disk. Files deleted or
  renamed in textdb are not removed from the target folder, and the executable bit and symbolic
  links are not something textdb stores: overwritten files keep their permissions.
- **Deletes are soft** until purged: a deleted file keeps its content and every version in the
  trash. Purging removes them for good and frees the content no remaining file or version
  shares; the SQLite file does not shrink, its free pages are reused.
- **Renames, moves and deletes are not versions.** A version is a change of content; path
  events are listed between versions and never renumber them.
- **One server, one store.** Other processes (the CLI, Python, `sqlite3` with the extension)
  may write the same store at any time; the server notices within about 100 ms and pushes the
  changes to every browser. A browser that loses its connection replays what it missed.
- **Rebuilding the extension on Windows** needs the server stopped: a loaded DLL cannot be
  overwritten. Reload the browser tab after rebuilding the web app.
- **A request with no bearer is the owner.** That is deliberate for a local app -- anyone who can
  open the store file is its owner anyway -- and it is why the server listens on loopback by
  default. For anything else, set `TEXTDB_REQUIRE_TOKEN=1` and give each person an account and a
  token ([permissions.md](permissions.md)); accounts see their own shares and nothing else, and a
  folder never granted is a 404 rather than a 403.
- **Sync, assets and the asset stores are the owner's** — an `admin`-kind token included, since
  that kind is what a deployment where nobody connects as the owner has instead. Whatever an
  ordinary account's rights are, these act on directories and drives of the server's own machine,
  and rclone runs with the server's credentials rather than the account's
  ([permissions.md](permissions.md) §4).
- **Trash and the one-batch undo are SQLite-only.** Against a central Postgres store the server
  says so through `GET /api/info`'s capabilities rather than offering what would fail.
