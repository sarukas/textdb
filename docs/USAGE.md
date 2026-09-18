# Using textdb

textdb stores text documents as content-defined chunks in a Merkle (prolly) tree. Every
document version is a root hash; versions share unchanged chunks. Writes are compare-and-swap
on the document root with automatic rebase: two agents editing different parts of the same
document both succeed; overlapping edits are merged line by line when possible and otherwise
rejected with a payload that carries the current text.

Both bindings expose the same model. Paths are `/folder/sub/file.md`; folders are created on
demand and moved or deleted as subtrees; deletes are tombstones (history stays readable).

## Postgres (`kb` schema)

### Views

| View | Columns | Writable |
|---|---|---|
| `kb.file` | the minimal listing tier — `path, name, kind, version, nbytes, nlines, updated_at, updated_by` — plus `id, dir, content, frontmatter, base_version` | INSERT (upsert), UPDATE `content` / `path` / `updated_by`, DELETE |
| `kb.folder` | the full listing record, folders only (`kb.entry` filtered); `n_children` and `nbytes_total` are now `files + folders` and `nbytes` | INSERT (mkdir -p), UPDATE `path`, DELETE |
| `kb.file_version` | `id, path, version, content, parent_version, author, ts, message` | read-only |

```sql
-- create (parents are created; an existing path is updated, identical content = no new version)
INSERT INTO kb.file(path, content, updated_by) VALUES ('/clients/acme/notes.md', E'# Acme\n\n- kickoff done\n', 'agent-7');

-- read
SELECT content, version FROM kb.file WHERE path = '/clients/acme/notes.md';
SELECT kb.lines('/clients/acme/notes.md', 3, 3);              -- 1-based inclusive line range
SELECT kb.section('/clients/acme/notes.md', 'Acme');           -- text of a heading's section (markdown)
SELECT * FROM kb.ls('/clients');                                -- one folder level, with words, versions, authors, folder totals
SELECT path, nwords FROM kb.ls('/clients', true) WHERE kind = 'file' ORDER BY nwords DESC LIMIT 10;  -- everything below, largest first
SELECT path, nbytes FROM kb.file WHERE path LIKE '/clients/%'; -- subtree (see the note below)

-- write whole content (the trigger diffs OLD → NEW and commits the edit set)
UPDATE kb.file SET content = replace(content, 'kickoff done', 'kickoff done, SOW sent'), updated_by = 'agent-7'
 WHERE path = '/clients/acme/notes.md';

-- write against the version you read (enables rebase when others wrote in between)
UPDATE kb.file SET content = $new_body, base_version = $version_i_read, updated_by = 'agent-7'
 WHERE path = '/clients/acme/notes.md';

-- targeted edit: `old` must occur exactly once; never re-sends the document
SELECT kb.edit('/clients/acme/notes.md', '- kickoff done', '- kickoff done ✔', 'agent-7');
SELECT kb.edit(f, 'SOW sent', 'SOW signed') FROM kb.file f WHERE f.path = '/clients/acme/notes.md';
SELECT kb.append('/clients/acme/notes.md', E'- next: pricing\n', 'agent-7');   -- never conflicts

-- history: version, author, ts, message, kind (direct|rebased|merged), base_version
SELECT * FROM kb.history('/clients/acme/notes.md');
SELECT kb.content('/clients/acme/notes.md', 1);
SELECT kb.diff('/clients/acme/notes.md', 1, 3);                -- unified diff between versions
SELECT version, author, ts FROM kb.file_version WHERE path = '/clients/acme/notes.md' ORDER BY version;

-- search: terms are ANDed per document, "quoted phrase", prefix*
SELECT path, version, line, text FROM kb.search('pricing acme', '/clients');
SELECT path, line FROM kb.search('"SOW signed"');
SELECT path FROM kb.search('kick*', '/');

-- namespace
UPDATE kb.folder SET path = '/archive/acme' WHERE path = '/clients/acme';   -- ids and history preserved
UPDATE kb.file   SET path = '/archive/acme/notes-2026.md' WHERE path = '/archive/acme/notes.md';
DELETE FROM kb.folder WHERE path = '/archive';                             -- tombstone; kb.content(path, v) still works
SELECT kb.checkpoint('before-migration');                                    -- record all current roots under a name
SELECT * FROM kb.export('/clients');                                         -- (path, content) rows
```

### Live clients: change feed, hunks, attributed writes

These mirror the SQLite binding's `textdb_feed`, `textdb_hunks`, … (see
[`live-app.md`](live-app.md)); names and semantics are the same.

| Function | Returns |
|---|---|
| `kb.write(path text, content text, base_version bigint DEFAULT NULL, author text DEFAULT NULL, message text DEFAULT NULL)` | `jsonb` `{"version": n, "kind": "direct"\|"rebased"\|"merged"\|"noop"}`. Creates the file if missing (`kind` `direct`, version 1); otherwise diffs against `base_version` (HEAD when NULL) and commits with rebase |
| `kb.replace_lines(path text, l_from bigint, l_to bigint, body text, base_version bigint DEFAULT NULL, author text DEFAULT NULL, message text DEFAULT NULL)` | same `jsonb`. Lines are 1-based inclusive and refer to `base_version` (HEAD when NULL); `l_to = l_from - 1` inserts in front of `l_from`; `l_from` one past the last line appends (a newline is supplied after an unterminated last line); out of range is `TX004`. Committed against `base_version`, so it rebases over commits that landed meanwhile |
| `kb.replace_ranges(path text, ranges jsonb, base_version bigint DEFAULT NULL, author text DEFAULT NULL, message text DEFAULT NULL)` | same `jsonb`. `ranges` is `[{"from": n, "to": n, "text": "…"}, …]`, each as in `kb.replace_lines`, all numbered as in `base_version`, in any order but not overlapping; one commit |
| `kb.edit(path, old, new, author, message DEFAULT NULL)`, `kb.append(path, tail, author, message DEFAULT NULL)` | `bigint` version; `message` defaults to `edit` / `append` |
| `kb.replace(path text, old text, new text, expected_count bigint DEFAULT NULL, author text DEFAULT NULL, message text DEFAULT NULL)` | `bigint` version — every occurrence of `old` becomes `new`, as one version. With `expected_count` the file must hold exactly that many, else at least one; otherwise `TX004` and nothing changes |
| `kb.replace_many(path text, replacements jsonb, author text DEFAULT NULL, message text DEFAULT NULL)` | `bigint` version — replacements applied in order as one version: `[[old, new], [old, new, expected_count], {"old", "new", "count"}, …]` |
| `kb.batch()` | the batch this transaction's writes are recorded under: the session setting `textdb.batch` (`SELECT set_config('textdb.batch', 'my-batch', true)`), NULL when unset. `kb.commit` and `kb.change` rows carry it |
| `kb.changes_after(seq bigint)`, `kb.batch_changes(batch text)` | `jsonb` `[{op, path, old_path, from_version, to_version, diff}]` — what changed after change number `seq` (a preview before `ROLLBACK`), or under `batch`; a file's commits fold into one item with a unified diff |
| `kb.revert_batch(batch text, author text DEFAULT NULL, skip_changed boolean DEFAULT false)` | `jsonb` `{restored: [{path, version}], removed, moved_back: [{from, to}], recreated, skipped}` — files go back to their content before the batch, files it created are deleted, moves undone, deletes recreated. Anything changed since is skipped, and unless `skip_changed` that is `TX004` with nothing changed; an unknown batch is `TX003` |
| `kb.hunks(path text, v1 bigint DEFAULT NULL, v2 bigint DEFAULT NULL)` | `TABLE(old_from, old_count, new_from, new_count, old_text, new_text)` — line hunks turning `v1` into `v2`, 1-based lines, a zero count is an insertion/deletion in front of that line. Defaults: `v2` = HEAD, `v1` = `v2 - 1`. Version 0 is the empty document |
| `kb.chunks(path text, version bigint DEFAULT NULL)` | `TABLE(ord, hash, byte_from, nbytes, line_from, nlines)` — the document's chunks in order (`ord` from 0, `line_from` 1-based, `hash` hex); unchanged content keeps its hash across versions |
| `kb.links(path text DEFAULT '/', status text DEFAULT '', lim bigint DEFAULT 10000)` | `TABLE(path, version, line, kind, target, anchor, alias, status, resolved, asset)` — the links written in a document or in everything below a folder. `status` keeps one kind (`ok`, `ambiguous`, `anchor-missing`, `broken`, `not-in-store`, `external`); anything else is an error, not an empty result. A link to an asset reports the asset, not its `.tdbasset` pointer |
| `kb.backlinks(path text DEFAULT '/', status text DEFAULT '', lim bigint DEFAULT 10000)` | The same row for the links pointing *at* `path`. An asset is found by its own path |
| `kb.feed(since bigint DEFAULT 0, lim bigint DEFAULT 10000)` | `TABLE(seq, ts, op, path, old_path, node_kind, version, base_version, commit_kind, author, message)` for every change with `seq > since`, ordered by `seq`. `op` ∈ `create, commit, mkdir, move, delete`; `node_kind` ∈ `file, folder` |
| `kb.last_seq()` | newest `seq`, 0 when the feed is empty |
| `kb.move(from_path text, to_path text, author text DEFAULT NULL, message text DEFAULT NULL)` | `void` — `_rename` with the move attributed in the feed. Links that pointed at what moved are rewritten when the store's `link_updates` setting is `rewrite` |
| `kb.move_links(from_path text, to_path text, author text DEFAULT NULL, message text DEFAULT NULL, links text DEFAULT NULL)` | `jsonb` `{"links": [{path, line, kind, target, now_at, version}]}` — a move that lists the links that pointed at what moved and no longer reach it. `links` is `off`, `report` or `rewrite` (each linking file committed once, `version` set, message `links: FROM -> TO`), NULL for the `link_updates` setting |
| `kb.remove(path text, author text DEFAULT NULL, message text DEFAULT NULL)` | `void` — `_delete` (tombstone) attributed in the feed |
| `kb.path_history(path text)` | `TABLE(id, ts, op, old_path, new_path, via, version, author)` — the renames, moves and deletes that touched the file or folder at `path` (live, else the one most recently deleted there), oldest first. One `kb.path_event` row per node an operation touched: a folder's move gives every file inside a row with `via` = the folder. `op` ∈ `rename` (same folder), `move`, `delete`; `version` is a file's version at the time. They are not versions |
| `kb.path_history_enabled()` | `boolean` — whether renames, moves and deletes are recorded: the session's `textdb.path_history` (`SET textdb.path_history = off`, or `ALTER ROLE … SET` / `ALTER DATABASE … SET` for a default), else the store setting, else on |
| `kb.setting(k text)`, `kb.set_setting(k text, v text)` | a store setting (`path_history` = `on` / `off`; `link_updates` = `off` / `report` / `rewrite`; `asset_sync` = `off` / `push` / `pull` / `both`, what `textdb sync` does with assets without `--push`/`--pull`; `asset_pull` = `linked` / `all`, which assets it pulls), NULL at its default; `kb.set_setting(k, NULL)` clears it; an unknown key or value is `TX004` |
| `kb.link` | one row per link: `file_id, line, kind` (`wiki`, `embed`, `md`, `image`), `target_path` (without anchor or alias), `anchor, alias, external, resolved_id` and `status` — `ok`, `ambiguous`, `anchor-missing`, `folder`, `broken`, `not-in-store`, `external` — by Obsidian's rules, kept current as files are created, moved and deleted |

```sql
SELECT kb.write('/guides/intro.md', $body, 6, 'agent-7', 'rewrite intro');   -- {"version": 7, "kind": "rebased"}
SELECT kb.replace_lines('/guides/intro.md', 12, 12, E'new line 12\nand 13\n', 7, 'agent-7');
SELECT * FROM kb.hunks('/guides/intro.md', 7, 8);
SELECT * FROM kb.feed(kb.last_seq() - 100);
SELECT kb.move('/drafts/x.md', '/guides/x.md', 'agent-7');
SELECT * FROM kb.path_history('/guides/x.md');                              -- move /drafts/x.md -> /guides/x.md by agent-7
SET textdb.path_history = off;                                              -- this session records no path history
SELECT kb.set_setting('path_history', 'off');                               -- nor, by default, does anyone else
```

Every create, commit, mkdir, move and delete writes one `kb.change` row **in the same
transaction** as the change, and sends `pg_notify('textdb_change', seq::text)`. A watcher
therefore needs no polling:

```sql
LISTEN textdb_change;       -- payload = the new row's seq (as text), delivered at COMMIT
-- on each notification (or on reconnect): SELECT * FROM kb.feed(:last_seen_seq);
```

Notifications are coalesced per transaction and are lost while disconnected, so treat the
payload as a hint and always read `kb.feed(last_seen_seq)`; the feed itself is the source of
truth. Versions of a file are consecutive, so the hunks for a `commit` row at version `v` are
`kb.hunks(path, v - 1, v)`. The `kb.file` / `kb.folder` view triggers go through `kb.move` and
`kb.remove`; a file rename via `UPDATE kb.file SET path = …` is attributed to `NEW.updated_by`.

> **Fresh install required.** This release adds `kb.change`, `kb.path_event`, `kb.setting` and
> the `kind` / `base_version` columns of `kb.commit`. There is no upgrade script: `DROP EXTENSION textdb_pg CASCADE;
> CREATE EXTENSION textdb_pg;` into an empty `kb` schema (reload content with `kb.write` or the
> Python loader).

### Errors

| SQLSTATE | Meaning | What to do |
|---|---|---|
| `TX001` | Conflict: someone changed the same lines since your base version. `DETAIL` is JSON: `{path, region_line_from, region_line_to, base, theirs, ours, current_version}` where `theirs` is the **current** text of the region | Re-derive your change from `theirs` (no extra read needed) and write again with `base_version = current_version` |
| `TX002` | Contention: the retry budget (8 CAS attempts) was exhausted on a very hot file | Back off a few ms and retry |
| `TX003` | Not found (path or version) | Check the path; folders may have been moved |
| `TX004` | Invalid edit: `old` text absent or not unique, invalid path segment, `kb.replace_lines` range outside the document | Read the current content and pick a unique anchor |

On the subtree listing above: `LIKE '/clients/%'` is fine with a literal prefix — `node_path`
is a `text_pattern_ops` index, so the planner extracts the prefix and seeks. When the prefix
is a **parameter**, escape it or use `kb._subtree_like($1)`, because a folder whose name
contains `%` or `_` would otherwise match siblings as well.

Plain `UPDATE kb.file SET content = …` without `base_version` diffs against the current
version and therefore never conflicts — it is "last writer wins" at line level, exactly like
editing a file. Pass `base_version` when the new content was derived from an earlier read.

### Concurrency semantics

- The only mutable state per file is `(root, version)`; a write succeeds iff its CAS succeeds
  and appends exactly one `kb.commit` row.
- Disjoint edits to the same file (different lines) both succeed without coordination.
- Overlapping edits succeed if a line-level three-way merge is clean; identical concurrent
  edits are absorbed (no new version); otherwise `TX001`.
- Moving a folder and editing a file under it never conflict.
- Cross-file atomicity is your enclosing transaction.

## SQLite (`textdb` virtual table)

```sql
CREATE VIRTUAL TABLE kb USING textdb(store='kb_');     -- shadow tables kb_node, kb_chunk, kb_tree_node, kb_commit, kb_section, kb_link, kb_frontmatter, kb_fts

INSERT INTO kb(path, content, author) VALUES ('/a.md', '# A' || char(10) || 'alpha' || char(10), 'me');
SELECT id, path, kind, version, nbytes, nlines, content FROM kb WHERE path = '/a.md';
UPDATE kb SET content = replace(content, 'alpha', 'ALPHA') WHERE path = '/a.md';
UPDATE kb SET content = ?, base_version = ?, author = 'me' WHERE path = '/a.md';   -- rebase against a stale read
SELECT textdb_edit('/a.md', 'ALPHA', 'beta', 'me');        -- returns the new version
SELECT textdb_append('/a.md', 'tail' || char(10));
SELECT textdb_content('/a.md'), textdb_content('/a.md', 1);
SELECT textdb_lines('/a.md', 2, 2), textdb_section('/a.md', 'A'), textdb_diff('/a.md', 1, 2);
SELECT * FROM textdb_history('/a.md');
SELECT * FROM textdb_ls('/');                              -- words, versions, authors (JSON), folder totals
SELECT path, nwords FROM textdb_ls('/', 1) WHERE kind = 'file' ORDER BY nwords DESC LIMIT 10;
SELECT textdb_entry('/notes');                             -- one folder's totals as JSON
-- A whole subtree: give `kb` a range on `path` and it seeks the index. Write it as a range
-- rather than `substr(path, 1, length(?) + 1) = ? || '/'` or `path LIKE ? || '/%'`: those are
-- functions of the column, so they cost a full scan of the store. `'0'` is the byte after
-- `'/'`, which makes `prefix || '0'` the exclusive end of the subtree.
SELECT path, kind, nbytes FROM kb WHERE path >= '/notes/' AND path < '/notes0' ORDER BY path;
SELECT path, version, line, text FROM textdb_search('beta', '/', 50);
SELECT * FROM textdb_export('/');
UPDATE kb SET path = '/archive/a.md' WHERE path = '/a.md';  -- also works for folders (subtree move)
DELETE FROM kb WHERE path = '/archive';
SELECT textdb_checkpoint('cp1');
```

Errors carry the same codes as text: `TX001 conflict: {json}`, `TX002 …`, `TX003 …`, `TX004 …`.
SQLite is single-writer; the rebase path is exercised when `base_version` is older than the
current version, and `BEGIN IMMEDIATE` inside the functions keeps each operation atomic.

## Python

```python
from textdb import Corpus, Conflict
with Corpus.open("postgresql://user@host/db", author="agent-7") as kb:   # or sqlite:///kb.db
    kb.load_folder("./notes", "/notes")
    text, v = kb.read_versioned("/notes/index.md")
    kb.edit("/notes/index.md", "TODO", "DONE")
    try:
        kb.update("/notes/index.md", text.replace("a", "b"), base_version=v)
    except Conflict as c:
        new = kb.read("/notes/index.md").replace("a", "b")          # rebuild on the current text (c.theirs has the region)
        kb.update("/notes/index.md", new, base_version=c.current_version)
```

Full API in [`python/README.md`](../python/README.md); the `python -m textdb` CLI covers
load/ls/cat/search/edit/append/history/diff/mv/rm/export.

## Recommended agent workflow

1. **Read** `content, version` once (or `kb.section` / `kb.lines` for the part you need).
2. **Edit locally**, then write with `kb.edit(path, old, new)` for a single anchored change,
   or `UPDATE kb.file SET content = …, base_version = <read version>` for a batch of changes.
3. **On `TX001`**, parse `DETAIL`, rebuild your change on `theirs`, and write again with
   `base_version = current_version`. Do not re-read the whole document.
4. **Use `kb.append`** for logs and journals; it never conflicts.
5. **Search** with `kb.search(query, prefix)` restricted to the folder you work in; results
   are `(path, line)` so a follow-up `kb.lines(path, line-5, line+5)` gives context cheaply.
6. **History** is free: `kb.history`, `kb.content(path, version)`, `kb.diff(path, v1, v2)`.

## Delegated access: accounts, tokens and shares

One store can hold every vault and still hand each person or agent only the folders they need.
An **account** is given whole folders — a share is a folder and everything below it — and sees
each one at its own root under an **alias** that belongs to the grant.

```sql
-- Postgres, as the owner.
SELECT kb.account_create('accounts-agent', 'agent');
SELECT * FROM kb.access_grant('accounts-agent', '/legal/contracts', 'rw', 'contracts');
SELECT * FROM kb.access_grant('accounts-agent', '/products', 'ro');
SELECT * FROM kb.token_create('accounts-agent', 'claude session');   -- the bearer, once
```

```sql
-- As the account, on the connection it works over.
SET textdb.token = 'tdb_…';        -- or SELECT kb.auth('tdb_…')
SELECT path FROM kb.ls('/');       -- contracts/  products/
SELECT kb.content('/contracts/acme.md');
```

SQLite is the same model through `textdb_auth`:

```sql
SELECT textdb_auth('tdb_…');       -- the account name, or an error
SELECT path FROM textdb_ls('/');
SELECT textdb_content('/contracts/acme.md');
```

Everything after that point speaks the account's paths — listings, `stat`, search, grep,
history, the change feed, links, `export` and `sync` — and shows nothing outside its shares.

What is worth knowing before you build on it:

- **A store that delegates nothing is unchanged.** With no token the connection is the owner and
  sees store paths, which is what opening the SQLite file or connecting to the database already
  means. The filter and the translation cost nothing on that path by construction: Postgres
  resolves the account once per statement as an InitPlan, and SQLite skips its session lookup on
  an atomic that is never set until something authenticates.
- **`forbidden` (`TX005`, exit 7) is not `not found` (`TX003`, exit 5).** A path under a share
  you hold but may not use — read-only, revoked, or its folder in the trash — is forbidden; a
  path under no share of yours is not found and is indistinguishable from one that never existed.
  `sync` deletes from disk what the store no longer has and leaves alone what it may not touch,
  so the difference is what stops a permission change emptying somebody's checkout.
- **Paths are per view; ids are not.** The same document is `/contracts/acme.md` to one account
  and `/legal/contracts/acme.md` to the owner. Every listing row carries the store's `id`, and
  every path-taking command accepts `id:1234` — that is the reference to pass between agents.
- **Links are rewritten at the boundary.** The store holds one canonical text whose root-absolute
  links use store paths; each reader sees its own. A link to a document the reader cannot see
  becomes `textdb:<id>`, which is not a path, cannot collide with one, and renders as an
  unresolved link. Relative links and name-only wiki links are byte-identical everywhere. Line
  numbers are the same in every view, so `cat -n`, `replace-lines` and search line numbers mean
  one thing.
- **Writes are checked before anything commits**, the author of a write is the account, and
  `--author` naming someone else is refused. Reverting a batch is settled over the *whole* batch
  first: one file you may not write refuses all of it rather than undoing half.
- **A root path that names no share of yours is a broken link, not a way in.** `[[hr/salaries]]`
  written by an account with no `hr` keeps its bytes — the text is the author's — and is recorded
  as broken for everyone, rather than resolving against the store's root.
- **The share is a node, not a path.** The owner moving or renaming a shared folder changes
  nothing for its holders. Moving or deleting the share *root* is refused to the holder: it is a
  folder they work inside and do not own. A renamed alias reaches a checkout as a directory move,
  so the next `sync` renames it and keeps whatever else was in it.
- **The SQL surface answers in the account's paths too**, and the store's own tables do not: the
  `files`, `folders`, `commits`, `links`, `properties`, `sections`, `authors` and `frontmatter`
  views, the `kb`/`kb.file` writable relation and the `textdb_*`/`kb.*` functions are the surface,
  and a token session naming `kb_node` (SQLite) or `kb.node` (Postgres) is refused.
- **An `admin`-kind account** (`account create ops --kind admin`) sees the whole store in the
  store's own paths. It is what a hosted deployment needs, where nobody opens the file or connects
  as the owner and every caller arrives with a bearer. It holds no shares, so it takes no root,
  and an unknown or revoked bearer is still refused rather than falling back to it.

### The trust boundary

On SQLite anyone who can open the file can read the raw `kb_*` tables or skip `textdb_auth`; on
Postgres the owner of the tables and any superuser can. Both mean "you own the store", which is
the trust boundary either way. The model is real where a server holds the store and clients hold
tokens.

On Postgres that shape has a second layer: `kb.node` carries a row-level security policy, so a
connection whose role owns nothing is filtered by the same rule every surface uses even on a
hand-written query against the table. The policy is deliberately not `FORCE`d — the table's owner
is the extension's own bookkeeping, and a policy over that would filter the store's maintenance —
so it binds exactly the role a hosted deployment hands a caller. Reading under such a role is
complete; **writing is not yet**, because the functions that write reach the tables as the caller
and need `SECURITY DEFINER` to do their bookkeeping. Until then, run the writing connection as the
owner and rely on the token for the filtering, which is what the CLI does.

**An HTTP server in front of it** is a third case, and the demo server
([`live-app.md`](live-app.md#who-is-asking)) is the worked example. A bearer belongs to a
connection, so it keeps one corpus per token rather than re-authenticating a shared one between
requests. A request with no bearer is the owner, which is right for a server someone runs beside
their own store and wrong for anything else: `TEXTDB_REQUIRE_TOKEN=1` makes a request without one
a 401. What that server adds on its own account -- syncing a directory, reading and writing an
asset store, binding one to its machine -- is the **owner's alone**, and a token session is
refused it: those act on the machine the server runs on, with its credentials, not the account's.
Everything else it passes through: the delegation commands run as the request's own session, so
the store refuses whoever may not run them.
