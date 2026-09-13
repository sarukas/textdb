# Live corpus app — contract

This is the contract between the store, the server and the web UI. To build, start and
configure the app, and for what it does, see [`demo-app.md`](demo-app.md).

The exercise: ingest a large markdown corpus into a textdb SQLite store, browse it as a file
tree, view and edit a document in the browser, and watch an agent edit the same document from
the command line — with the agent's changes appearing in the open viewer or editor as they
land, attributed, and with every version browsable and diffable.

```
 agent ──► textdb CLI (Rust) ──┐
                               ├──► kb.db (SQLite, WAL) ◄── node server ──SSE/HTTP──► web UI
 human ──► web UI ──HTTP───────┘        change feed            (watcher)
```

Nothing talks to anything but the store. The CLI does not need the server running; the server
learns about the CLI's commits from the store's change feed.

## What the store provides (SQLite binding)

| SQL | Returns |
|---|---|
| `textdb_feed(since[, lim])` | `seq, ts, op, path, old_path, node_kind, version, base_version, commit_kind, author, message` for every change with `seq > since`, oldest first. `op` ∈ `create, commit, mkdir, move, delete, purge`; `node_kind` ∈ `file, folder`; `commit_kind` ∈ `direct, rebased, merged` |
| `textdb_last_seq()` | newest `seq` (0 when empty) |
| `textdb_hunks(path[, v1[, v2]])` | `old_from, old_count, new_from, new_count, old_text, new_text` — line hunks turning `v1` into `v2`, 1-based lines, a zero count is an insertion/deletion in front of that line. Defaults: `v2` = HEAD, `v1` = `v2 - 1`. Version 0 is the empty document |
| `textdb_chunks(path[, version])` | `ord, hash, byte_from, nbytes, line_from, nlines` — the document's chunks in order; unchanged content keeps its hash across versions |
| `textdb_history(path)` | `version, author, ts, message, nbytes, kind, base_version` |
| `textdb_write(path, content[, base_version[, author[, message]]])` | JSON `{"version": n, "kind": "direct"\|"rebased"\|"merged"\|"noop"}`; creates the file if missing |
| `textdb_replace_lines(path, from, to, text[, base_version[, author]])` | same JSON; lines refer to `base_version` (HEAD if NULL); `to = from - 1` inserts |
| `textdb_move(from, to[, author])` | `1`; moves or renames a file, or a folder with everything below it (missing parent folders are created). History moves with the files. TX003 when `from` is missing, TX004 when `to` exists, is inside `from`, or either is the root |
| `textdb_delete(path[, author])` | `1`; deletes a file, or a folder with everything below it. History stays in the store. TX003 when missing, TX004 for the root |
| `textdb_path_history(path[, node_id])` | `id, ts, op, old_path, new_path, via, version, author` — the renames, moves and deletes that touched the file or folder at `path` (live, else the one most recently deleted there), or with `textdb_path_history(NULL, id)` a trash entry; oldest first. One row per node an operation touched: a folder's rename gives each file inside a row with `via` = the folder. `op` ∈ `rename` (same folder), `move`, `delete`; `version` is a file's version at the time. Path events are not versions |
| `textdb_setting(key[, value])` | a store setting, NULL at its default; `textdb_setting(key, value)` sets it, `textdb_setting(key, NULL)` clears it. `path_history` (`on`/`off`, default on) decides whether renames, moves and deletes are recorded |
| `textdb_trash([parent_id])` | JSON array of trash entries `{id, name, kind, path, version, nbytes, nlines, files, updated_at, updated_by, deleted_at, deleted_by}`: without an argument the trash items (one per delete, newest first), with a trashed folder's id what was deleted inside it. `path` is where the entry was when deleted; a folder's `files`/`nbytes` total what was deleted with it |
| `textdb_trash_entry(id)` | one entry as JSON |
| `textdb_trash_content(id[, version])` | a trashed file's content, at the version it was deleted with by default |
| `textdb_trash_history(id)` | JSON array of its commits, as `textdb_history` |
| `textdb_purge(id[, author])` | JSON `{items, files, folders, versions, chunks, tree_nodes, bytes}`: removes the entry and everything deleted with it inside for good, then the chunks and tree nodes nothing remaining (any version of any file, HEAD, checkpoint) still reaches. Records a `purge` change |
| `textdb_empty_trash([author])` | the same for every trash item; one `purge` change per item |
| `textdb_ls(dir[, recursive])` | `name, kind, nbytes, nlines, updated_at, path, nwords, versions, created_at, updated_by, nauthors, authors, files, folders, id` — the folder's own entries by name, or with `recursive = 1` everything below it by path. A file's `nwords` counts words as `wc -w` does; `authors` is a JSON array `[{author, commits, first_ts, last_ts}]`, most commits first. For a folder, `nbytes`, `nlines`, `nwords` and `versions` total every live file below it, `files`/`folders` count what is below it, and `updated_at` is the latest change anywhere inside. The figures are kept current by every commit, mkdir, move and delete, so listing costs the rows listed, not the subtree |
| `textdb_entry(path)` | one entry as JSON with the same fields (`authors` as an array); works for the root |
| `textdb_migrate()` | brings a store written by an older build up to date; call once after `CREATE VIRTUAL TABLE IF NOT EXISTS kb USING textdb(store='kb_')` |

plus the existing `kb` table, `textdb_content`, `textdb_search`, `textdb_diff`,
`textdb_edit`, `textdb_append`. Errors are `TX001 conflict: {json}`, `TX002 …`, `TX003 …`,
`TX004 …` in the SQLite error message.

**Watching for changes from another process.** SQLite has no cross-process notification. A
watcher keeps one connection, polls `PRAGMA data_version` (a cheap, connection-local counter that
moves only when *another* connection commits) every ~100 ms, and when it moves reads
`textdb_feed(last_seq)`. Every change row is written in the same transaction as the change, so
the feed is never ahead of or behind the data. Versions of a file are consecutive, so the hunks
for a `commit` row at version `v` are `textdb_hunks(path, v - 1, v)`.

## HTTP API (node server)

Base URL `http://localhost:4317`. JSON bodies, UTF-8. Errors are
`{ "code": "TX00n", "message": "…", "conflict"?: {…} }` with status 409 (TX001), 503 (TX002),
404 (TX003), 400 (TX004), 500 (other). A malformed request (missing parameter, non-integer
version, body that is not JSON) is a 400 with code TX004.

Configuration by environment: `TEXTDB_DB` (path of the SQLite store, default `./kb.db`),
`TEXTDB_SQLITE_EXT` (path of the loadable extension; otherwise found under
`crates/textdb-sqlite-ext/target/release`), `PORT` (default 4317), `HOST` (default
`127.0.0.1` — the API has no authentication, so it listens on loopback unless told otherwise).

| Method & path | Request | Response |
|---|---|---|
| `GET /api/info` | | `{ db, files, last_seq }` |
| `GET /api/ls?path=/a` | | `[entry]` — one folder level, folders first then files, by name. An entry is `{ id, name, path, kind, nbytes, nlines, nwords, versions, updated_at, updated_by, created_at, files, folders, nauthors, authors }`, see `textdb_ls` |
| `GET /api/list?path=/a[&sort=name][&order=asc][&offset=0][&limit=200][&recursive=1][&name=…][&author=…][&type=md][&kind=file]` | | `{ path, total, offset, entries: [entry] }` — one page, sorted and filtered in the store. `sort` ∈ `name, type, size, lines, words, versions, created, updated, authors`; folders come first except when `recursive`. `name` matches names containing it, or as a glob with `*`/`?`; `author` keeps files that author committed to (`''` for commits without one); `type` is an extension. `limit` ≤ 1000; `total` counts every match |
| `GET /api/entry?path=…` | | one `entry`, the root included |
| `POST /api/bulk` | `{ op: "move" \| "delete", paths: [...], to?, author? }` (at most 10000 paths) | `{ op, to, done, skipped }` — one transaction: all move (into folder `to`, keeping names) or delete, or none do. A path inside another listed folder goes with it and is `skipped`, as is a move to where a path already is |
| `GET /api/export/files?path=/a` | | `{ path, files: [{ rel, nbytes, updated_at }] }` — every file below the folder (the whole store without `path`), by path, `rel` relative to it |
| `POST /api/export/hashes` | `{ paths: [...] }` (at most 1000) | `{ hashes: [{ path, sha256 }] }` — SHA-256 of each file's stored bytes, so a client can compare with a file on disk without downloading it |
| `GET /api/export/file?path=/a/b.md` | | the file's stored bytes, unchanged (`application/octet-stream`): line endings and byte-order mark as imported |
| `GET /api/export/zip?path=/a` | | a zip of every file below the folder, entries named relative to it, streamed as it is built |
| `GET /api/file?path=/a/b.md[&version=n]` | | `{ path, version, head_version, content, nbytes, nlines, updated_at, updated_by }` |
| `GET /api/chunks?path=…[&version=n]` | | `[{ ord, hash, byte_from, nbytes, line_from, nlines }]` |
| `GET /api/history?path=…` | | `[{ version, author, ts, message, nbytes, kind, base_version }]` oldest first |
| `GET /api/hunks?path=…&from=v1&to=v2` | | `[{ old_from, old_count, new_from, new_count, old_text, new_text }]` |
| `GET /api/diff?path=…&from=v1&to=v2` | | `{ diff }` unified text |
| `GET /api/search?q=…[&prefix=/][&limit=50]` | | `[{ path, line, snippet, rank }]` |
| `PUT /api/file` | `{ path, content, base_version?, author?, message? }` | `{ version, kind }` — rebased over concurrent commits; 409 with `conflict` when the same lines changed |
| `POST /api/replace-lines` | `{ path, from, to, text, base_version?, author? }` | `{ version, kind }` |
| `GET /api/stat?path=…` | | `{ path, kind, files, folders, nbytes }` — for a folder, everything below it (`folders` does not count the folder itself) |
| `POST /api/move` | `{ from, to, author? }` | `{ from, to }` — a file or a whole folder; one `move` change for the moved node |
| `POST /api/delete` | `{ path, author? }` | `{ path }` — a file or a whole folder; one `delete` change for the deleted node |
| `GET /api/path-history?path=…` or `?id=…` | | `[{ id, ts, op, old_path, new_path, via, version, author }]`, see `textdb_path_history` |
| `GET /api/setting?key=…` | | `{ key, value }` (`value` null at the default) |
| `PUT /api/setting` | `{ key, value }` (`value` null clears) | `{ key, value }` |
| `GET /api/trash[?parent=id]` | | trash entries, see `textdb_trash` |
| `GET /api/trash/file?id=…[&version=n]` | | `{ entry, version, content }` |
| `GET /api/trash/history?id=…` | | `[{ version, author, ts, message, nbytes, kind, base_version }]` |
| `POST /api/trash/purge` | `{ id, author? }` | `{ items, files, folders, versions, chunks, tree_nodes, bytes }` |
| `POST /api/trash/empty` | `{ author? }` | the same, for the whole trash |
| `POST /api/import` | `{ files: [{ path, content }], author? }` (at most 5000 files) | `{ created, updated, unchanged, failed, failures: [{ path, code, message }] }` — one transaction, message `import`; unchanged files make no version; a refused file is listed and the rest still land |
| `GET /api/events[?since=seq]` | `Last-Event-ID` honoured | Server-sent events, see below |

### `GET /api/events`

`text/event-stream`. Without `since` (and without `Last-Event-ID`) the stream starts at the
current `last_seq`. When both are present `Last-Event-ID` wins, because a reconnecting
`EventSource` repeats its original URL. Each change is one event:

```
id: 1042
event: change
data: {"seq":1042,"ts":"2026-09-13T09:12:01.123Z","op":"commit","path":"/guides/intro.md",
       "old_path":null,"node_kind":"file","version":7,"base_version":6,"commit_kind":"direct",
       "author":"agent-7","message":"replace-lines",
       "hunks":[{"old_from":12,"old_count":1,"new_from":12,"new_count":2,"old_text":"…","new_text":"…"}]}
```

- `hunks` is present for `op = "commit"` when the hunk text totals ≤ 256 KiB; otherwise the
  field is absent and the client fetches `/api/hunks` or the whole file.
- For `create` the client may fetch the file; `mkdir`, `move`, `delete` refresh the tree.
- A comment line `: ping` is sent every 15 s.

## Client semantics (web UI)

- **Base version.** An open document holds `base_version` (the version its text corresponds to)
  and, in the editor, the local unsaved changes relative to that text.
- **Remote commit on the open file** (`event.path` equals the open path):
  - if `event.version == base_version + 1` apply `event.hunks`; otherwise fetch
    `/api/hunks?from=base_version&to=event.version` and apply those;
  - in the editor, the remote hunks become a CodeMirror change set applied with a `remote`
    annotation; unsaved local changes are mapped over it (they survive, shifted), and the
    changed lines are decorated with the author's colour and a label
    (`agent-7 · v7 · rebased`) that fades after a few seconds;
  - in the preview, the markdown blocks whose source lines intersect the new line ranges are
    re-rendered and flashed with the same label; untouched blocks keep their DOM and the
    scroll position is preserved;
  - `base_version` becomes `event.version`.
- **Own commits** also come back through the feed; the client recognises them by the version
  its save returned and does not flash them.
- **Save** sends the full text with `base_version`; the store rebases over anything that
  landed in between, and the response `kind` is shown (`saved v8 · merged`). A 409 opens a
  conflict view with `base`, `theirs` (current text of the region) and `ours`.
- **History** lists versions with author, time and kind; any version opens read-only; any two
  versions open in a diff view.
