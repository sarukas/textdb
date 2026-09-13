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
| `HOST` | `127.0.0.1` | Interface to listen on. The API has **no authentication**: only listen beyond loopback on a network you trust |

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
- **No authentication and no multi-user permissions**: this is a local demo.
