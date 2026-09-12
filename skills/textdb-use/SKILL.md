---
name: textdb-use
description: Read, search, edit and version markdown/text documents stored in textdb (Postgres schema kb, or the SQLite textdb virtual table) safely alongside other agents — anchored edits, base-version writes, conflict (TX001) handling, history and diffs. Use when working with a knowledge base kept in textdb.
---

# Working with documents in textdb

textdb keeps documents in SQL with full history and compare-and-swap writes. Several agents
can edit the same document at once: edits to different lines both land; edits to the same
lines are merged when possible, otherwise you get a conflict payload with the current text.
Everything below is plain SQL. Postgres form first; the SQLite form is listed at the end.

## Rules

1. **Never rewrite a document you did not read in this session.** Read `content, version`
   first, or use `kb.edit(path, old, new)` which anchors on unique text.
2. **Prefer anchored edits** (`kb.edit`) for one change and **`base_version` writes** for a
   batch. Both let textdb rebase your change over concurrent commits.
3. **Handle `TX001` by rebuilding on `theirs`**, not by re-reading and retrying blindly.
4. **Append with `kb.append`** for logs, journals, changelogs — it never conflicts.
5. **Do not touch tables in schema `kb`** (`kb.node`, `kb.chunk`, …). Use views and functions.
6. Paths are absolute, `/`-separated, any UTF-8 except `/`; folders appear when a file is
   created under them.

## Find and read

```sql
SELECT * FROM kb.ls('/clients');                                -- entries of one folder
SELECT path, nbytes, updated_at FROM kb.file WHERE path LIKE '/clients/acme/%' ORDER BY path;
SELECT path, line, snippet FROM kb.search('pricing renewal', '/clients');  -- AND of terms per document
SELECT path, line FROM kb.search('"quarterly review"');          -- phrase
SELECT path FROM kb.search('renew*', '/clients');                -- prefix
SELECT content, version FROM kb.file WHERE path = '/clients/acme/notes.md';   -- remember version
SELECT kb.lines('/clients/acme/notes.md', 40, 60);              -- 1-based inclusive
SELECT kb.section('/clients/acme/notes.md', 'Open questions');  -- markdown heading (path "A / B" or last component)
```

Read only the part you need: `kb.lines` and `kb.section` cost O(fragment), `content` costs the
whole document.

## Change

Single anchored change (old text must be unique in the document):

```sql
SELECT kb.edit('/clients/acme/notes.md',
               '- renewal: pending',
               '- renewal: signed 2026-09-12', 'agent-7');      -- returns the new version
```

If `old` is not unique you get `TX004`; include more surrounding text as the anchor.

Batch of changes derived from a read:

```sql
UPDATE kb.file
   SET content = $new_body, base_version = $version_you_read, updated_by = 'agent-7'
 WHERE path = '/clients/acme/notes.md';
```

Append:

```sql
SELECT kb.append('/clients/acme/journal.md', E'\n## 2026-09-12\n- call with CFO\n', 'agent-7');
```

Create (parents created; if the path exists the content is updated, identical content makes no
new version):

```sql
INSERT INTO kb.file(path, content, updated_by) VALUES ('/clients/acme/plan.md', $body, 'agent-7');
```

Move / rename / delete:

```sql
UPDATE kb.file   SET path = '/clients/acme/plan-2027.md' WHERE path = '/clients/acme/plan.md';
UPDATE kb.folder SET path = '/archive/acme'               WHERE path = '/clients/acme';
DELETE FROM kb.file WHERE path = '/archive/acme/plan-2027.md';   -- tombstone; history stays readable
```

## When a write fails

| SQLSTATE | Meaning | Do this |
|---|---|---|
| `TX001` | Another agent changed the same lines. `DETAIL` is JSON `{path, region_line_from, region_line_to, base, theirs, ours, current_version}`; `theirs` is the current text of those lines | Recompute your change starting from `theirs`; write again with `base_version = current_version` (or `kb.edit` anchored on text from `theirs`). No extra read is needed |
| `TX002` | Too many concurrent commits on this file right now | Wait 50–200 ms, retry once or twice |
| `TX003` | Path or version not found | List the folder; the file may have been moved |
| `TX004` | Anchor text missing or ambiguous, or bad path | Read the current content, choose a unique anchor |

Never loop on `UPDATE … SET content` without `base_version` to "win" a conflict: it silently
overwrites other agents' lines.

## History and review

```sql
SELECT * FROM kb.history('/clients/acme/notes.md');              -- version, author, ts, message
SELECT kb.content('/clients/acme/notes.md', 3);                  -- any version, also of deleted files
SELECT kb.diff('/clients/acme/notes.md', 3, 7);                  -- unified diff
SELECT version, author, ts FROM kb.file_version WHERE path = '/clients/acme/notes.md' ORDER BY version DESC LIMIT 5;
SELECT kb.checkpoint('before-bulk-rewrite');                      -- name the current roots of all files
```

Identical content committed by two agents produces one version; your `UPDATE` returns success
but `version` does not move — that is expected, not an error.

## SQLite equivalents

`CREATE VIRTUAL TABLE kb USING textdb(store='kb_')` once, then: `SELECT … FROM kb WHERE path = ?`
(columns `content, version, kind, nbytes, nlines`), `INSERT INTO kb(path, content, author)`,
`UPDATE kb SET content = ?, base_version = ?, author = ? WHERE path = ?`,
`UPDATE kb SET path = ? WHERE path = ?`, `DELETE FROM kb WHERE path = ?`,
`textdb_edit(path, old, new, author)`, `textdb_append(path, text, author)`,
`textdb_content(path[, version])`, `textdb_lines(path, from, to)`, `textdb_section(path, heading)`,
`textdb_diff(path, v1, v2)`, `textdb_history(path)`, `textdb_ls(path)`,
`textdb_search(query, prefix, limit)`, `textdb_export(prefix)`, `textdb_checkpoint(name)`.
Errors are messages starting with the same codes (`TX001 conflict: {json}` …).
