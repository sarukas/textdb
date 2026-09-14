---
name: textdb-cli
description: Browse, read, search and edit markdown/text documents kept in a textdb store (SQLite file or Postgres) with the `textdb` command-line tool — tree, cat with line numbers and versions, full-text search, SQL queries over files, front matter, headings, links and commits, line-range and anchored edits that rebase over concurrent writers, conflict handling via exit status 3, history, diffs and following live changes. Use when a corpus lives in a textdb store rather than on disk, or when asked to edit documents other people or agents may be editing at the same time.
---

# Working on a textdb corpus with the `textdb` CLI

The documents are not files on disk: they live in a textdb store, and every change creates a
version that others see immediately (including a person watching in the web UI). Use the
`textdb` binary (`target/release/textdb` in the textdb repository) for everything — never
edit the `.db` file with other tools.

## Setup

```sh
export TEXTDB_STORE=path/to/kb.db       # or postgres://user@host/db
export TEXTDB_AUTHOR=agent-<name>        # your writes are attributed to this name
export MSYS_NO_PATHCONV=1               # Git Bash on Windows only: stops "/a.md" being rewritten
```

With `MSYS_NO_PATHCONV=1`, give local files and directories (the store, `sync`/`export` targets)
as Windows (`C:/Users/me/kb.db`) or relative paths: `/c/Users/...` is no longer translated and the
store will not open.

Run `textdb config` once to check the store, your author name and whether renames, moves and
deletes are recorded in history, and where each setting came from.

Paths look like `/folder/file.md`. Writing them without the leading slash (`folder/file.md`)
also works and is immune to shell rewriting. Add `--json` to any command for machine-readable
output.

## Find your way around

```sh
textdb tree -L 2                         # top of the tree with file counts
textdb tree guides -d                    # folders only under /guides
textdb ls guides/api
textdb ls -l guides                      # + words, versions, last update, authors; folders show totals below them
textdb ls -R -l --sort updated -r guides # everything below /guides, most recently changed first
textdb ls -l --sort words guides --json  # machine-readable, with authors and folder file counts
textdb ls -1 guides                      # only paths, one per line: for loops and xargs
textdb search rate limit -p guides       # path:line: text for each line holding a word; all words must occur
textdb search '"rate limit"' 'retry*'    # a phrase and a prefix
textdb grep -i 'status: (draft|review)' -p guides   # regular expression per line; -F plain text, -l paths only
textdb stat guides/api/index.md          # version, size, lines, last author
textdb export guides ./checkout --dry-run # what writing /guides to disk would change; drop --dry-run to write
```

`search` uses the full-text index (case and accents ignored) and prints nothing on stdout when
nothing matches (`no matches` on stderr, exit 0). Treat its lines as candidates: read the lines
with `cat -n --lines` before editing. Use `grep` for exact case, punctuation or regular
expressions; it reads every file under the folder, so narrow it with `-p`.

`export` writes only new and changed files and deletes nothing. It stops with exit code 6 and
lists the names that cannot exist side by side on this OS (e.g. `README.md` and `readme.md` on
Windows/macOS); rename those in the store, then export again.

To keep a folder reconciled with a git checkout both ways, use `sync` rather than export/import:

```sh
textdb sync guides ~/src/repo/guides --dry-run   # changes each way, merges, conflicts
textdb sync guides ~/src/repo/guides --commit    # apply; commit what changed on disk (Textdb-* trailers)
textdb git-status guides ~/src/repo/guides       # last synced commit; store vs HEAD by blob id
```

Exit code 3 from `sync` means conflict markers were written to files on disk: resolve them there
(or ask the user), then run `sync` again.

## Ask questions with SQL

For anything `ls`, `tree` and `search` do not answer directly — front matter values, who links
where, which files have a heading, who changed what — run one `textdb sql` statement instead of
looping over `cat` or `ls` in the shell. Use a quoted heredoc for the statement and `-p` for values
(`?`, or `?1` to reuse one); add `--json` when you need to parse the rows.

```sh
textdb sql <<'SQL'
SELECT path, json_extract(data, '$.status') AS status
FROM frontmatter WHERE json_extract(data, '$.type') = 'account' ORDER BY path
SQL
textdb sql -p guides/api/index.md <<'SQL'
SELECT path, line FROM links WHERE target = ?1 OR target LIKE '%/' || ?1   -- who links here
SQL
textdb sql -p '%Errors' 'SELECT path, heading, line_from FROM sections WHERE heading LIKE ?'
textdb sql 'SELECT path, version, author, ts, message FROM commits ORDER BY ts DESC LIMIT 20'
textdb --json sql 'SELECT path, nwords FROM files ORDER BY nwords DESC LIMIT 10'
```

| View | Columns |
|---|---|
| `files` | `path, name, version, nbytes, nlines, nwords, created_at, updated_at, updated_by` |
| `folders` | `path, files, folders, nbytes, nwords, versions, updated_at` (totals below) |
| `frontmatter` | `path, data` — YAML front matter as JSON: `json_extract(data, '$.key')`, `json_each(data, '$.list')` |
| `sections` | `path, heading` (`Title / Section`), `level, line_from, line_to` — feed `line_from` to `cat --lines` |
| `links` | `path, target` (as written), `line` |
| `commits` | `path, version, author, ts, message, kind` |
| `authors` | `path, author, commits, first_ts, last_ts` |

Also `textdb_search(query, prefix)`, `textdb_ls(dir, recursive)`, `textdb_content(path)`. Do not
select `content` from `kb` across many files: it reads every document in full.

Statements are read-only unless you pass `--write`. Then change documents only through the textdb
functions, passing `:author` (bound to your author name) so the edits are attributed:

```sh
textdb sql --write <<'SQL'
SELECT path, textdb_edit(path, 'status: draft', 'status: published', :author) AS version
FROM frontmatter WHERE path LIKE '/guides/%' AND json_extract(data, '$.status') = 'draft'
SQL
```

Run the same `SELECT` without the function first to see which files it touches. A `--write`
statement is all or nothing: if one file fails (the old text is missing or not unique), nothing
changes; fix the statement and run it again. `textdb_edit(path, old, new, :author)` needs `old` to
occur exactly once; `textdb_append(path, text, :author)` and
`textdb_write(path, content, base_version, :author, message)` are the others.

## Read before you edit

```sh
textdb cat -n guides/api/index.md                    # header: /guides/api/index.md v12 · lines 1-310 of 310
textdb cat -n guides/api/index.md --lines 120:160    # just a range
textdb cat guides/api/index.md --section 'Errors'    # one markdown section
```

**Remember the version in the header** (`v12`). Line numbers belong to that version.

## Edit

Prefer the smallest edit that does the job:

```sh
# Replace lines 130-134 as numbered in v12. Rebased automatically if others committed meanwhile.
textdb replace-lines guides/api/index.md 130 134 -b 12 <<'EOF'
New text for those lines.
EOF

# Insert before line 40 (TO = FROM - 1), as numbered in v12.
textdb replace-lines guides/api/index.md 40 39 -b 12 --text $'A new line.\n'

# Replace text that occurs exactly once (no version needed).
textdb edit guides/api/index.md --old 'deprecated in 2.0' --new 'removed in 3.0'

# Multi-line anchors without shell quoting trouble:
printf '%s' '{"old": "## Errors\n\nOld intro.", "new": "## Errors\n\nNew intro."}' | textdb edit guides/api/index.md --stdin-json

# Append to a log or journal (never conflicts).
textdb append notes/journal.md '- 2026-09-13: reviewed the API guide'

# Whole-document rewrite derived from v12.
textdb write guides/api/index.md -b 12 -m 'restructure' < new-index.md

# Create a new file; --create fails (exit 6) instead of replacing one that exists.
textdb write --create guides/api/limits.md -m 'new page' <<'EOF'
# Limits
EOF
```

Every editing command takes `-m 'why'`: `write`, `edit`, `replace-lines`, `append`, `mv` and `rm`.
Give one that says what the change is for; history and the web app show it.

Each write prints `path: vN (kind)`: `direct` (nobody else wrote), `rebased` (others changed
other lines; your change was applied on top), `merged` (others touched nearby lines and the
three-way merge was clean), `unchanged` (nothing to do).

## When a write is refused

| Exit status | Meaning | Do this |
|---|---|---|
| 3 | Conflict: someone changed the same lines since your version | Read the payload (stderr, or `--json` stdout): `theirs` is the current text of those lines and `current_version` the version it belongs to. Rebuild your change on `theirs` and retry with `-b <current_version>`. Do not retry the same command blindly |
| 4 | Contention on a very hot file | Wait a moment and retry |
| 5 | Not found | Check the path with `ls` / `tree`; it may have moved (`textdb log`) |
| 6 | Invalid edit: `--old` text missing or not unique, line range outside the file, empty content | Re-read (`cat -n`) and choose a unique anchor or a valid range |
| 2 | Usage error, including a path mangled into `C:/…` by the shell | Drop the leading slash or set `MSYS_NO_PATHCONV=1` |

## History and other people's changes

```sh
textdb history guides/api/index.md       # who changed it, when, how each change landed, and renames/moves/deletes
textdb diff guides/api/index.md 10 12    # unified diff between versions
textdb hunks guides/api/index.md         # what the latest commit changed, line by line
textdb cat guides/api/index.md -v 10     # an old version
textdb log --since 0 --limit 50          # recent changes across the corpus
textdb watch -p guides --json            # follow changes live (runs until stopped)
```

`history` lists versions and, between them, the renames, moves and deletes that touched the
file — also those of a folder it was in (`renamed … (with /old-folder)`). In `--json` each
entry has `"type": "version"` or `"type": "path"` (`op`: `rename`, `move`, `delete`);
`--versions-only` gives versions alone. A deleted file's history is still found at the path it
was deleted from.

## Reorganise

```sh
textdb mv guides/draft.md guides/published/intro.md -m 'publish'  # a file or a whole folder; history moves with it
textdb rm guides/old -m 'superseded by guides/new'                 # a file or a whole folder, recursively
textdb setting                                        # path_history: on (default) or off for this store
textdb --path-history off mv archive/2024 archive/y2024   # keep one bulk reshuffle out of history
```

`rm` is not final: deleted files keep their content and versions in the store's trash, where
people can read and restore-by-copy them in the web app until someone permanently removes
them. Moving or deleting a folder touches everything inside it — check with `tree` first.
Folders a move or delete leaves empty are removed too (printed as `removed empty folder …`);
pass `--keep-empty-folders` to keep them.

## Rules

1. Read with `cat -n` and pass the header's version as `-b` for line-number edits.
2. Keep edits small and local; many agents and people may be editing the same document.
3. On exit status 3, rebuild on `theirs`; never overwrite with a stale whole-document `write`
   that lacks `-b`.
4. Use `append` for journals and logs.
5. Use your own `TEXTDB_AUTHOR`, so the changes you make are attributed to you.
6. Do not `rm` or `mv` folders you were not asked to reorganise; people are browsing them.
7. To find things across many files, write one `textdb sql` query rather than a shell loop over
   `cat`/`ls`; check a `--write` statement's `SELECT` first.
