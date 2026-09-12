---
name: textdb-use-sqlite
description: Read, search, edit and version markdown/text documents kept in a textdb SQLite store (the `textdb` virtual table and textdb_* functions) — anchored edits, base-version writes, TX001 conflict handling, history, diffs, and the SQL equivalents of everyday file-system commands (ls, cat, grep, sed, awk, mv, rm, git log). Use when a knowledge base lives in an SQLite file with the textdb module loaded.
---

# Working with documents in textdb (SQLite)

textdb for SQLite is a virtual-table module plus SQL functions. Load it in one of three ways:

- **Python**: `pip install -e python/` from the repo, then `from textdb import Corpus; kb = Corpus.open("sqlite:///kb.db")`
  (loads `libtextdb_sqlite_ext.so`; set `TEXTDB_SQLITE_EXT=/path/to/libtextdb_sqlite_ext.so` if it is not next to the package or in `target/release`).
- **sqlite3 shell**: `.load /path/to/libtextdb_sqlite_ext` then `CREATE VIRTUAL TABLE IF NOT EXISTS kb USING textdb(store='kb_');`
- **Rust**: `textdb_sqlite::open("kb.db")`.

SQLite is single-writer: writes serialize, but the rebase/conflict logic still applies when
you write with a `base_version` older than the current one. Same content model as the
Postgres extension: absolute `/` paths, folders on demand, tombstone deletes, full history.

## Rules

1. Read `content, version` before rewriting; or use `textdb_edit(path, old, new)` anchored on unique text.
2. Batch changes derived from a read go through `UPDATE kb SET content = ?, base_version = ? WHERE path = ?`.
3. On `TX001` rebuild your change on `theirs` from the error payload; do not blindly retry.
4. Journals/logs: `textdb_append` — never conflicts.
5. Never modify the shadow tables (`kb_node`, `kb_chunk`, …) directly.

## File-system equivalents

| Shell habit | textdb SQL (SQLite) |
|---|---|
| `ls /clients` | `SELECT * FROM textdb_ls('/clients');` |
| `find /clients -name '*.md'` | `SELECT path FROM kb WHERE kind = 'file' AND path LIKE '/clients/%' AND path LIKE '%.md';` |
| `du -sb /clients` | `SELECT sum(nbytes) FROM kb WHERE kind = 'file' AND path LIKE '/clients/%';` |
| `cat notes.md` | `SELECT content FROM kb WHERE path = '/clients/acme/notes.md';` or `SELECT textdb_content('/clients/acme/notes.md');` |
| `sed -n '40,60p' notes.md` | `SELECT textdb_lines('/clients/acme/notes.md', 40, 60);` |
| `head -20` / `tail -20` | `SELECT textdb_lines(p, 1, 20)` / `SELECT textdb_lines(path, nlines - 19, nlines) FROM kb WHERE path = p` |
| `wc -l` / `wc -c` | `SELECT nlines, nbytes FROM kb WHERE path = '/clients/acme/notes.md';` |
| `awk '/^## Open questions/,/^## /'` (a section) | `SELECT textdb_section('/clients/acme/notes.md', 'Open questions');` |
| `grep -rn -w pricing /clients` | `SELECT path, line, snippet FROM textdb_search('pricing', '/clients');` |
| `grep -rl pricing \| xargs grep -l renewal` | `SELECT path FROM textdb_search('pricing renewal', '/clients');` |
| `grep -rn '"quarterly review"'` | `SELECT path, line FROM textdb_search('"quarterly review"', '/');` |
| `grep -rn 'renew'` (prefix) | `SELECT path, line FROM textdb_search('renew*', '/');` |
| `sed -i 's/pending/signed/' notes.md` (unique occurrence) | `SELECT textdb_edit('/clients/acme/notes.md', 'pending', 'signed', 'me');` |
| `sed -i 's/foo/bar/g' notes.md` (all, last writer wins) | `UPDATE kb SET content = replace(content, 'foo', 'bar') WHERE path = '/clients/acme/notes.md';` |
| `echo "- done" >> journal.md` | `SELECT textdb_append('/clients/acme/journal.md', '- done' \|\| char(10), 'me');` |
| `cat > new.md` (create/overwrite) | `INSERT INTO kb(path, content, author) VALUES ('/clients/acme/new.md', ?, 'me');` |
| `mkdir -p /clients/acme/2027` | `INSERT INTO kb(path, kind) VALUES ('/clients/acme/2027', 'folder');` |
| `mv notes.md notes-2026.md` | `UPDATE kb SET path = '/clients/acme/notes-2026.md' WHERE path = '/clients/acme/notes.md';` |
| `mv /clients/acme /archive/acme` | `UPDATE kb SET path = '/archive/acme' WHERE path = '/clients/acme';` |
| `rm notes.md` / `rm -r /archive` | `DELETE FROM kb WHERE path = '/clients/acme/notes.md';` / `DELETE FROM kb WHERE path = '/archive';` |
| `git log notes.md` | `SELECT * FROM textdb_history('/clients/acme/notes.md');` |
| `git show v3:notes.md` | `SELECT textdb_content('/clients/acme/notes.md', 3);` |
| `git diff v1 v2 -- notes.md` | `SELECT textdb_diff('/clients/acme/notes.md', 1, 2);` |
| `git tag before-migration` | `SELECT textdb_checkpoint('before-migration');` |
| `cp -r /clients ./export` | `SELECT * FROM textdb_export('/clients');` or `textdb-corpus export kb.db ./export` |
| `rsync ./notes/ /clients/` (import; unchanged files make no version) | `textdb-corpus import ./notes kb.db` or Python `Corpus.load_folder` |

## Read

```sql
SELECT id, path, kind, version, nbytes, nlines FROM kb WHERE path LIKE '/clients/%';
SELECT content, version FROM kb WHERE path = '/clients/acme/notes.md';
SELECT textdb_lines('/clients/acme/notes.md', 40, 60);
SELECT textdb_section('/clients/acme/notes.md', 'Open questions');
SELECT path, line, snippet FROM textdb_search('pricing renewal', '/clients', 50);
```

## Change

```sql
SELECT textdb_edit('/clients/acme/notes.md', '- renewal: pending', '- renewal: signed', 'agent-7');  -- new version
UPDATE kb SET content = ?, base_version = ?, author = 'agent-7' WHERE path = '/clients/acme/notes.md';
SELECT textdb_append('/clients/acme/journal.md', '- call with CFO' || char(10), 'agent-7');
INSERT INTO kb(path, content, author) VALUES ('/clients/acme/plan.md', ?, 'agent-7');   -- upsert
UPDATE kb SET path = '/archive/acme' WHERE path = '/clients/acme';
DELETE FROM kb WHERE path = '/archive/acme/plan.md';
```

## When a write fails

Errors are SQLite error messages beginning with a code:

| Message starts with | Meaning | Do this |
|---|---|---|
| `TX001 conflict: {json}` | Same lines changed concurrently; JSON has `base`, `theirs` (current text), `ours`, `region_line_from/to`, `current_version` | Rebuild on `theirs`, write again with `base_version = current_version` |
| `TX002` | Retry budget exhausted | Wait briefly, retry |
| `TX003` | Path/version not found | List the folder |
| `TX004` | `old` missing or not unique / bad path | Read and choose a unique anchor |

## History

```sql
SELECT * FROM textdb_history('/clients/acme/notes.md');
SELECT textdb_content('/clients/acme/notes.md', 3);
SELECT textdb_diff('/clients/acme/notes.md', 3, 7);
SELECT textdb_checkpoint('before-bulk-rewrite');
```
