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
| `kb.file` | `id, path, name, parent_path, content, version, nbytes, nlines, frontmatter, updated_at, updated_by, base_version` | INSERT (upsert), UPDATE `content` / `path` / `updated_by`, DELETE |
| `kb.folder` | `id, path, name, parent_path, n_children, nbytes_total, updated_at` | INSERT (mkdir -p), UPDATE `path`, DELETE |
| `kb.file_version` | `id, path, version, content, parent_version, author, ts, message` | read-only |

```sql
-- create (parents are created; an existing path is updated, identical content = no new version)
INSERT INTO kb.file(path, content, updated_by) VALUES ('/clients/acme/notes.md', E'# Acme\n\n- kickoff done\n', 'agent-7');

-- read
SELECT content, version FROM kb.file WHERE path = '/clients/acme/notes.md';
SELECT kb.lines('/clients/acme/notes.md', 3, 3);              -- 1-based inclusive line range
SELECT kb.section('/clients/acme/notes.md', 'Acme');           -- text of a heading's section (markdown)
SELECT * FROM kb.ls('/clients');                                -- one folder level
SELECT path, nbytes FROM kb.file WHERE path LIKE '/clients/%'; -- subtree

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

-- history
SELECT * FROM kb.history('/clients/acme/notes.md');
SELECT kb.content('/clients/acme/notes.md', 1);
SELECT kb.diff('/clients/acme/notes.md', 1, 3);                -- unified diff between versions
SELECT version, author, ts FROM kb.file_version WHERE path = '/clients/acme/notes.md' ORDER BY version;

-- search: terms are ANDed per document, "quoted phrase", prefix*
SELECT path, line, snippet FROM kb.search('pricing acme', '/clients');
SELECT path, line FROM kb.search('"SOW signed"');
SELECT path FROM kb.search('kick*', '/');

-- namespace
UPDATE kb.folder SET path = '/archive/acme' WHERE path = '/clients/acme';   -- ids and history preserved
UPDATE kb.file   SET path = '/archive/acme/notes-2026.md' WHERE path = '/archive/acme/notes.md';
DELETE FROM kb.folder WHERE path = '/archive';                             -- tombstone; kb.content(path, v) still works
SELECT kb.checkpoint('before-migration');                                    -- record all current roots under a name
SELECT * FROM kb.export('/clients');                                         -- (path, content) rows
```

### Errors

| SQLSTATE | Meaning | What to do |
|---|---|---|
| `TX001` | Conflict: someone changed the same lines since your base version. `DETAIL` is JSON: `{path, region_line_from, region_line_to, base, theirs, ours, current_version}` where `theirs` is the **current** text of the region | Re-derive your change from `theirs` (no extra read needed) and write again with `base_version = current_version` |
| `TX002` | Contention: the retry budget (8 CAS attempts) was exhausted on a very hot file | Back off a few ms and retry |
| `TX003` | Not found (path or version) | Check the path; folders may have been moved |
| `TX004` | Invalid edit: `old` text absent or not unique, invalid path segment | Read the current content and pick a unique anchor |

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
SELECT * FROM textdb_ls('/');
SELECT path, line, snippet FROM textdb_search('beta', '/', 50);
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
