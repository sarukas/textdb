---
name: textdb-use-postgres
description: Read, search, edit and version markdown/text documents kept in the textdb Postgres extension (schema kb) safely alongside other agents — anchored edits, base-version writes, TX001 conflict handling, history, diffs, and the SQL equivalents of everyday file-system commands (ls, cat, grep, sed, awk, mv, rm, git log). Use when a knowledge base lives in Postgres with textdb installed.
---

# Working with documents in textdb (Postgres)

textdb keeps documents in Postgres with full history and compare-and-swap writes. Several
agents can edit the same document at once: edits to different lines both land; edits to the
same lines are merged when possible, otherwise you get a conflict payload with the current
text. Everything is plain SQL against schema `kb`; connect with your usual client (`psql`,
psycopg, JDBC, …). Python: `pip install -e python/` from the repo gives `textdb.Corpus`.

## Rules

1. **Never rewrite a document you did not read in this session.** Read `content, version`
   first, or use `kb.edit(path, old, new)` which anchors on unique text.
2. **Prefer anchored edits** (`kb.edit`) for one change and **`base_version` writes** for a
   batch. Both let textdb rebase your change over concurrent commits.
3. **Handle `TX001` by rebuilding on `theirs`**, not by re-reading and retrying blindly.
4. **Append with `kb.append`** for logs, journals, changelogs — it never conflicts.
5. **Do not touch tables in schema `kb`** (`kb.node`, `kb.chunk`, …). Use views and functions.
6. Paths are absolute, `/`-separated, any UTF-8 except `/`; folders appear when a file is
   created under them and vanish when empty folders are deleted.

## File-system equivalents

| Shell habit | textdb SQL |
|---|---|
| `ls /clients` | `SELECT * FROM kb.ls('/clients');` (a folder row's `nbytes`, `nlines`, `nwords`, `versions`, `files`, `folders` total everything below it; a file's `authors` is jsonb, most commits first) |
| `ls -lt` / `ls -R` | `SELECT path, updated_at FROM kb.ls('/clients') ORDER BY updated_at DESC;` / `SELECT path FROM kb.ls('/clients', true);` |
| `find /clients -name '*.md'` | `SELECT path FROM kb.file WHERE path LIKE '/clients/%' AND path LIKE '%.md' ORDER BY path;` |
| `du -sh /clients` | `SELECT nbytes FROM kb.entry WHERE path = '/clients';` (kept current; no subtree scan) |
| `wc -w notes.md` | `SELECT nwords FROM kb.entry WHERE path = '/clients/acme/notes.md';` |
| `cat notes.md` | `SELECT content FROM kb.file WHERE path = '/clients/acme/notes.md';` |
| `sed -n '40,60p' notes.md` | `SELECT kb.lines('/clients/acme/notes.md', 40, 60);` |
| `head -20 notes.md` / `tail -20 notes.md` | `SELECT kb.lines(p, 1, 20)` / `SELECT kb.lines(p, nlines - 19, nlines) FROM kb.file WHERE path = p` |
| `wc -l notes.md` / `wc -c` | `SELECT nlines, nbytes FROM kb.file WHERE path = '/clients/acme/notes.md';` |
| `awk '/^## Open questions/,/^## /' notes.md` (a section) | `SELECT kb.section('/clients/acme/notes.md', 'Open questions');` |
| `grep -rn -w pricing /clients` | `SELECT path, line, snippet FROM kb.search('pricing', '/clients');` |
| `grep -rl pricing /clients \| xargs grep -l renewal` (both words in a file) | `SELECT path FROM kb.search('pricing renewal', '/clients');` |
| `grep -rn '"quarterly review"'` | `SELECT path, line FROM kb.search('"quarterly review"');` |
| `grep -rn 'renew' --include='*'` (prefix) | `SELECT path, line FROM kb.search('renew*');` |
| `sed -i 's/pending/signed/' notes.md` (one unique occurrence) | `SELECT kb.edit('/clients/acme/notes.md', 'pending', 'signed', 'me');` |
| `sed -i 's/foo/bar/g' notes.md` (all occurrences, one version) | `SELECT kb.replace('/clients/acme/notes.md', 'foo', 'bar', NULL, 'me');` (4th argument: the exact count expected, else at least one; `kb.replace_many(path, '[["a","b"],["c","d",2]]', 'me')` for several) |
| `sed -i '12,14c…' notes.md` (line ranges, one commit) | `SELECT kb.replace_ranges(p, '[{"from": 12, "to": 14, "text": "…\n"}, {"from": 40, "to": 39, "text": "inserted\n"}]', $version_you_read, 'me', 'message');` |
| `echo "- done" >> journal.md` | `SELECT kb.append('/clients/acme/journal.md', E'- done\n', 'me');` |
| `cat > new.md <<EOF …` (create or overwrite) | `INSERT INTO kb.file(path, content, updated_by) VALUES ('/clients/acme/new.md', $body, 'me');` |
| `mkdir -p /clients/acme/2027` | `INSERT INTO kb.folder(path) VALUES ('/clients/acme/2027');` (also implicit on file create) |
| `mv notes.md notes-2026.md` | `UPDATE kb.file SET path = '/clients/acme/notes-2026.md' WHERE path = '/clients/acme/notes.md';` |
| `mv /clients/acme /archive/acme` | `UPDATE kb.folder SET path = '/archive/acme' WHERE path = '/clients/acme';` |
| `rm notes.md` / `rm -r /archive` | `DELETE FROM kb.file WHERE path = …;` / `DELETE FROM kb.folder WHERE path = '/archive';` (tombstones; history stays) |
| `mv` with attribution | `SELECT kb.move('/clients/acme', '/archive/acme', 'me', 'why');` · `SELECT kb.remove('/archive/old', 'me', 'why');` |
| `mv` and fix the links to it | `SELECT kb.move_links('/notes/Plan.md', '/archive/Plan.md', 'me', NULL, 'rewrite');` (`'report'` lists them without changing anything) |
| which notes link here / broken links | `SELECT n.path, l.line, l.target_path FROM kb.link l JOIN kb.node n ON n.id = l.file_id AND n.deleted_at IS NULL WHERE l.resolved_id = kb._node_id('/notes/Plan.md');` · `… WHERE l.status IN ('broken', 'anchor-missing', 'ambiguous')` |
| `git log notes.md` | `SELECT * FROM kb.history('/clients/acme/notes.md');` and `SELECT * FROM kb.path_history('/clients/acme/notes.md');` (renames, moves, deletes) |
| `git show HEAD~3:notes.md` | `SELECT kb.content('/clients/acme/notes.md', version - 3) FROM kb.file WHERE path = …;` |
| `git diff v1 v2 -- notes.md` | `SELECT kb.diff('/clients/acme/notes.md', 1, 2);` |
| `git tag before-migration` | `SELECT kb.checkpoint('before-migration');` |
| `cp -r /clients ./export` | `SELECT * FROM kb.export('/clients');` (or `textdb-corpus export`, or Python `Corpus.export_folder`) |
| `rsync ./notes/ /clients/` (import, unchanged files make no new version) | `INSERT INTO kb.file(path, content) VALUES …` per file (Python `Corpus.load_folder`) |

Differences to remember: `sed -i`/`echo >>` on files are last-writer-wins and unversioned;
the SQL forms are transactional, versioned, and (with `kb.edit`/`base_version`) rebased over
concurrent writes. `grep` is line-based; `kb.search` is document-based for AND and reports
the first matching line.

## Find and read

```sql
SELECT * FROM kb.ls('/clients');
SELECT path, nbytes, updated_at FROM kb.file WHERE path LIKE '/clients/acme/%' ORDER BY path;
SELECT path, line, snippet FROM kb.search('pricing renewal', '/clients');   -- AND of terms per document
SELECT content, version FROM kb.file WHERE path = '/clients/acme/notes.md';  -- remember version
SELECT kb.lines('/clients/acme/notes.md', 40, 60);                           -- 1-based inclusive
SELECT kb.section('/clients/acme/notes.md', 'Open questions');               -- heading path "A / B" or last component
```

`kb.lines` and `kb.section` cost O(fragment); `content` costs the whole document.

Folder totals are written as insert-only rows in `kb.folder_delta` so concurrent commits never
wait on a shared parent folder; `kb.entry` adds them in. A maintenance job can fold them with
`SELECT kb.compact_folder_totals();`; `SELECT kb.rebuild_folder_totals();` recomputes every
folder from its files if a move raced a commit inside the moved folder.

## Change

```sql
SELECT kb.edit('/clients/acme/notes.md', '- renewal: pending', '- renewal: signed 2026-09-12', 'agent-7');
UPDATE kb.file SET content = $new_body, base_version = $version_you_read, updated_by = 'agent-7'
 WHERE path = '/clients/acme/notes.md';
SELECT kb.append('/clients/acme/journal.md', E'\n## 2026-09-12\n- call with CFO\n', 'agent-7');
INSERT INTO kb.file(path, content, updated_by) VALUES ('/clients/acme/plan.md', $body, 'agent-7');
UPDATE kb.folder SET path = '/archive/acme' WHERE path = '/clients/acme';
DELETE FROM kb.file WHERE path = '/archive/acme/plan.md';
```

`kb.edit` needs `old` to be unique in the document (`TX004` otherwise — widen the anchor).
`INSERT` on an existing path updates it; identical content creates no version.

## When a write fails

| SQLSTATE | Meaning | Do this |
|---|---|---|
| `TX001` | Another agent changed the same lines. `DETAIL` is JSON `{path, region_line_from, region_line_to, base, theirs, ours, current_version}`; `theirs` is the current text of those lines | Recompute your change from `theirs`; write again with `base_version = current_version`, or `kb.edit` anchored on text from `theirs`. No extra read is needed |
| `TX002` | Too many concurrent commits on this file right now | Wait 50–200 ms, retry once or twice |
| `TX003` | Path or version not found | List the folder; the file may have been moved |
| `TX004` | Anchor text missing or ambiguous, or bad path | Read the current content, choose a unique anchor |

Never loop on `UPDATE … SET content` without `base_version` to "win" a conflict: it silently
overwrites other agents' lines. A successful write whose `version` did not move means an
identical change was already there — expected, not an error.

## History and review

```sql
SELECT * FROM kb.history('/clients/acme/notes.md');
SELECT kb.content('/clients/acme/notes.md', 3);                 -- any version, also of deleted files
SELECT kb.diff('/clients/acme/notes.md', 3, 7);
SELECT version, author, ts FROM kb.file_version WHERE path = '/clients/acme/notes.md' ORDER BY version DESC LIMIT 5;
SELECT kb.checkpoint('before-bulk-rewrite');
```

### Renames, moves and deletes

Every rename, move and delete is recorded for each node it touched (a folder's move gives
each file inside an entry with `via` = the folder), unless path history is off. They are not
versions.

```sql
SELECT op, old_path, new_path, via, version, author, ts FROM kb.path_history('/clients/acme/notes.md');
SELECT kb.path_history_enabled();                 -- session setting, else store setting, else true
SET textdb.path_history = off;                    -- this session only, e.g. for a scripted reorganisation
SELECT kb.set_setting('path_history', 'off');     -- the store default for everyone; NULL restores on
SELECT kb.set_setting('asset_sync', 'both');      -- what `textdb sync` does with assets: off (default), push, pull, both
SELECT kb.set_setting('asset_pull', 'all');       -- which it pulls: linked (default: what the notes link to) or all
```

Postgres has no trash functions yet: a deleted file's versions stay readable with
`kb.content(path, version)` and `kb.history(path)`.

## Bulk changes: preview, batch, revert

Name a batch for the transaction, preview with the diffs, then commit or roll back; a batch can
be undone later as a whole:

```sql
BEGIN;
SELECT set_config('textdb.batch', 'acme-rename-2026-09-15', true);
SELECT kb.last_seq();                                            -- remember it as :seq
SELECT kb.replace(path, 'Acme Corp', 'Acme', NULL, 'agent-7') FROM kb.file
 WHERE path LIKE '/accounts/%' AND strpos(content, 'Acme Corp') > 0;
SELECT kb.changes_after(:seq);                                   -- [{op, path, from_version, to_version, diff}]
COMMIT;                                                          -- or ROLLBACK
SELECT kb.revert_batch('acme-rename-2026-09-15', 'agent-7');     -- TX004 if anything changed since; true as 3rd argument skips those
```
