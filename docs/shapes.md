# What every surface returns

Two records, and every surface returns one of them unchanged: the **listing record** (`Entry`)
and the **hit row** (`Hit`). The CLI, the SQL functions and views on both engines, the Python
and Node SDKs and the HTTP API all use these names, in this order, with these types.

The rules that hold everywhere:

- Keys are `snake_case`, on the wire and in both SDKs.
- **Every key is always present.** A value that does not apply is `null`, never omitted, so a
  consumer can tell "not applicable" from "this build does not have it".
- `path` is absolute and normalized on output (`/guides/api/index.md`), whatever was typed.
- Timestamps are ISO-8601 UTC with milliseconds and `Z` (`2026-09-16T07:05:05.583Z`), as
  strings, on both backends and in every SDK. Postgres renders them in SQL rather than leaving
  `::text` to the session's time zone.
- Sizes are `nbytes`, a raw integer. Human rendering (`316.7 KB`) is a text-output concern.
- A folder's figures are totals over the live files below it, never null. A folder has no
  `version`.

## The listing record

Twenty-four keys. The first eight are the **minimal tier**: what a caller needs to find a
file, read it, and then edit it safely — `version` above all, because every line-numbered edit
needs one and no listing used to carry it.

| # | Key | Type | File | Folder |
|---|---|---|---|---|
| 1 | `path` | string | absolute | absolute |
| 2 | `name` | string | last segment | last segment |
| 3 | `kind` | `"file"` \| `"folder"` | | |
| 4 | `version` | int \| null | current version | `null` |
| 5 | `nbytes` | int | own size | total below |
| 6 | `nlines` | int | own lines | total below |
| 7 | `updated_at` | string | last commit or move | latest change below |
| 8 | `updated_by` | string \| null | author of the last commit | author of the latest below |
| 9 | `id` | int | | |
| 10 | `dir` | string \| null | parent folder | parent folder; `null` for `/` |
| 11 | `depth` | int | `/a.md` is 1 | `/` is 0 |
| 12 | `ext` | string \| null | lower case, no dot | `null` |
| 13 | `title` | string \| null | front matter `title`, else the first level-1 heading | `null` |
| 14 | `nwords` | int | own | total below |
| 15 | `nsections` | int | headings | total below |
| 16 | `nprops` | int | top-level front-matter keys | total below |
| 17 | `nlinks` | int | links written in it | total below |
| 18 | `nlinks_broken` | int | links that reach no document | total below |
| 19 | `versions` | int | version count | commits below |
| 20 | `created_at` | string | | |
| 21 | `files` | int \| null | `null` | live files below |
| 22 | `folders` | int \| null | `null` | live folders below |
| 23 | `nauthors` | int | distinct authors | distinct authors below |
| 24 | `authors` | array | `[{author, commits, first_ts, last_ts}]`, most commits first | `[]` |

`nsections`, `nprops`, `nlinks`, `nlinks_broken` and `title` are computed once at commit from
what the markdown extractor already returned, stored on the node row and rolled into the
folder totals — the way `nwords` has always been. `nlinks_broken` is the one that moves
without a commit, because a link breaks when its *target* is deleted; the pass that
re-resolves link statuses updates it and the totals above it.

Not in the record on purpose: `content` and `frontmatter` (a listing row stays O(1) per file —
fetch them with `cat`, `meta get`, or the `kb` table), and `score` (a search property).

**Where it comes from:** `textdb ls`, `textdb ls -R`, `textdb tree`, `textdb stat`;
`textdb_ls(path, recursive)` and `textdb_entry(path)` on SQLite; `kb.ls(path, recursive)`,
`kb.entry` and `kb.folder` on Postgres; the `files` and `folders` SQL views; `Corpus.ls()` and
`Corpus.entry()` in Python and Node; `GET /api/ls`, `/api/list`, `/api/entry`, `/api/stat`.

The `kb` table (SQLite) and `kb.file` (Postgres) carry the **minimal tier** plus `id`, `dir`
and `content` — they are the writable surfaces, not listings.

## The hit row

Seven keys, one row per matching **line**, from `search` and `grep` alike.

| # | Key | Type | Meaning |
|---|---|---|---|
| 1 | `path` | string | normalized |
| 2 | `version` | int | the version the line number belongs to; pass it as `--base-version` |
| 3 | `line` | int | 1-based |
| 4 | `text` | string | the matching line, windowed around the match when longer than 300 characters, with the cut ends marked |
| 5 | `section` | string \| null | the heading path the line sits under (`API Guide / Errors`), for `cat --section` |
| 6 | `score` | number \| null | relevance, higher is better, scaled to (0, 1] against the best hit of this query; `null` from `grep` |
| 7 | `more` | int | matching lines in this file held back by `--per-file`; 0 otherwise |

`--limit` counts **rows** on both commands and `--per-file` caps how many come from one
document. `-l` lists the matching documents and `-c` each with its count; both return
`{path, version, matches}` — a flag filters rows, it never changes the row type.

The index works on chunks, so on its own it knows which documents match and only guesses at
the line. Every surface now reads the matching documents and lists the lines that really hold
the terms, which is what makes `line` a fact rather than a hint, and drops a document whose
words only ever appear apart.

## History

One order on both engines and in every SDK, matching the `commits` view:
`version, author, ts, message, kind, base_version, nbytes, nlines, nwords`.

## What this replaced

| Was | Is |
|---|---|
| `snippet` (search), `text` (grep), cut at 200 and 300 | `text`, one cut, windowed around the match |
| `rank`, negative on SQLite and positive on Postgres | `score`, higher is better on both |
| `parent_path` / `dir` / `parent` / `parent_id` | `dir` |
| `valuesN` (Node), `values` (Python), `values_n` (SQL) | `values_n` everywhere — `values` is reserved in SQL |
| `PropertyHit.updatedAt` (Node) | `updated_at` |
| `ls` 15 columns, `stat` 7, `tree` 6, Python 6 | one record, 24 keys |
| a key omitted when absent | `null` |
| `search` limit 100 (SQL), 50 (Node, HTTP), 200 (UI), 50 documents (CLI) | 200 rows everywhere |
| `kb.folder.n_children`, `nbytes_total` | `files + folders`, `nbytes` |
| `tree` folder rows `(N files, SIZE)`; `tree FILE` headed `(0 files, 0 B)` | `(N files, M folders, SIZE)`; `tree FILE` prints the one entry |
