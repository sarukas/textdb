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
SELECT path, line, kind, target FROM links WHERE resolved = ?1   -- who links here (as `backlinks`)
SQL
textdb sql -p '%Errors' 'SELECT path, heading, line_from FROM sections WHERE heading LIKE ?'
textdb sql 'SELECT path, version, author, ts, message FROM commits ORDER BY ts DESC LIMIT 20'
textdb --json sql 'SELECT path, nwords FROM files ORDER BY nwords DESC LIMIT 10'
```

| View | Columns |
|---|---|
| `files` | `path, name, dir, depth, ext, version, nbytes, nlines, nwords, created_at, updated_at, updated_by` |
| `folders` | `path, name, parent, depth, files, folders, nbytes, nwords, versions, updated_at` (totals below) |
| `frontmatter` | `path, data` — YAML front matter as JSON: `json_extract(data, '$.key')`, `json_each(data, '$.list')` |
| `sections` | `path, heading` (`Title / Section`), `level, line_from, line_to` — feed `line_from` to `cat --lines` |
| `links` | `path, target` (without `#anchor`/`|alias`), `line, kind` (`wiki`, `embed`, `md`, `image`), `anchor, alias, status` (`ok`, `ambiguous`, `anchor-missing`, `broken`, `not-in-store`, `external`), `resolved` (the file it points to; for an asset, the asset's path), `asset` (it resolves to an asset's pointer) |
| `commits` | `path, version, author, ts, message, kind, batch` |
| `authors` | `path, author, commits, first_ts, last_ts` |

Also `textdb_search(query, prefix)`, `textdb_ls(dir, recursive)`, `textdb_content(path)`. Do not
select `content` from `kb` across many files: it reads every document in full.

- **Output for scripts:** `--format lines` prints one value per line (one column), `--format tsv`
  whole values tab-separated; the table cuts values at 60 characters (`--full` shows them). Long
  statements: `textdb sql -f query.sql`.
- **Paths:** use `dir`, `depth`, `ext` (files) and `parent`, `depth` (folders) instead of
  `substr`/`instr` arithmetic: `WHERE dir = '/accounts/acme'`, `WHERE depth = 2`.
- **Patterns:** in `LIKE`, `_` and `%` are wildcards (`'/work_files/%'` matches `/workXfiles/`; add
  `ESCAPE '\'` and write `\_`); `[0-9]`-style classes work only with `GLOB`, which is case-sensitive.
  A wrong "0 rows" is often this.
- **`textdb_search`** gives one row per document holding every term (hyphenated terms such as
  `teo-group` need no quotes). Its `line`/`snippet` come from one chunk and may hold only some of the
  terms: check with `textdb_lines(path, line, line)`, or use the `search` command, which lists each
  matching line.

Statements are read-only unless you pass `--write`. Then change documents only through the textdb
functions, passing `:author` (bound to your author name) so the edits are attributed:

```sh
textdb sql --write <<'SQL'
SELECT path, textdb_edit(path, 'status: draft', 'status: published', :author) AS version
FROM frontmatter WHERE path LIKE '/guides/%' AND json_extract(data, '$.status') = 'draft'
SQL
```

Run it with `--write --dry-run` first: it prints each file's diff and the moves and deletes, then
undoes everything. A `--write` statement is all or nothing: if one file fails (old text missing or
not unique, a count that does not match), nothing changes; fix the statement and run it again.

| Function | Use |
|---|---|
| `textdb_edit(path, old, new, :author)` | `old` must occur exactly once |
| `textdb_replace(path, old, new, expected_count, :author)` | every occurrence, one version; pass the count you expect (or NULL for "at least one") |
| `textdb_replace_many(path, '[["old","new"],["old2","new2",1]]', :author)` | several replacements in one file, applied in order, one version |
| `textdb_move(from, to, :author)`, `textdb_delete(path, :author)` | move or delete files or folders atomically with the rest of the statement — no shell loops |
| `textdb_append(path, text, :author)`, `textdb_write(path, content, base_version, :author, message)` | as the commands |

After a real write, `textdb sql` prints `batch <id>`; `textdb revert-batch <id>` undoes that whole
run (files back to their earlier content, moves undone, deletes restored as new files) and refuses if
something changed since — `--dry-run` to preview, `--skip-changed` to revert the rest.

```sh
textdb sql --write --dry-run <<'SQL'
SELECT path, textdb_replace_many(path, '[["Acme Corp", "Acme"], ["status: draft", "status: published", 1]]', :author)
FROM files WHERE dir = '/accounts/acme' AND ext = 'md' AND instr(textdb_content(path), 'Acme Corp') > 0
SQL
```

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

# One logical change in several places: all ranges numbered as in v12, one version, one message.
textdb replace-lines guides/api/index.md -b 12 --stdin-json -m 'rename Foo to Bar' <<'EOF'
[{"from": 3, "to": 3, "text": "title: Bar\n"},
 {"from": 88, "to": 90, "text": "Bar replaces Foo.\n"},
 {"from": 200, "to": 199, "text": "- 2026-09-14: renamed to Bar\n"}]
EOF

# Front matter, one key at a time; nothing else in the file changes.
textdb meta get guides/api/index.md status            # value; list items one per line; --json for JSON
textdb meta set guides/api/index.md status published
textdb meta set guides/api/index.md tags api limits    # several values (or --list) make a list
textdb meta set guides/api/index.md related '[[Limits]]'   # quoted in YAML as needed
textdb meta unset guides/api/index.md draft_notes

# Replace text that occurs exactly once (no version needed).
textdb edit guides/api/index.md --old 'deprecated in 2.0' --new 'removed in 3.0'
textdb edit notes/todo.md --old '- call Ana' --new '- [x] call Ana'   # values starting with - work (or --old='- …')

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

## Links

Links are resolved in the store (Obsidian's rules: markdown links relative to the note, `[[a/b]]`
from the root, `[[name]]` by file name, `.md` optional, `#heading` checked) and kept current as files
come and go.

```sh
textdb links guides/api/index.md          # path:line: [[target]] -> /resolved.md, or (broken) etc.
textdb backlinks guides/api/index.md      # who links here — check before moving or deleting it
textdb links --broken guides              # broken, anchor-missing, not-in-store (add --dir DIR to check files on disk)
textdb mv guides/a.md guides/b.md --update-links   # also rewrite the links that pointed at a.md
```

Without `--update-links`, `mv` lists the links it leaves pointing at the old place (unless the
store's `link_updates` setting is `rewrite` or `off`); `rm` lists the links it breaks.

Binaries (images, PDFs, office files) are assets: their bytes are in an asset store and a pointer
document `NAME.tdbasset` sits where the file belongs. Links to them resolve to the pointer
(`asset: true`, `resolved` is the asset's own path). Work with them through `assets`, never by
editing a `.tdbasset` file:

```sh
textdb assets status guides                 # ok / new / modified / outdated / conflict / not-pulled here
textdb assets push guides -m "diagrams"     # upload new and changed files, then commit their pointers
textdb assets pull --linked-from guides/api # fetch the files those notes link to
textdb assets verify guides                 # exit 1 if a hash does not match here or in the asset store
textdb sync guides ~/vault --push --pull    # documents, then assets: moved, trashed and renamed files follow their pointers
textdb assets migrate-from-git guides       # binaries git tracks go to the asset store; one git commit
textdb mv guides/img/arch.png guides/diagrams/arch.png   # an asset by its own path: its pointer moves, sync moves the file
```

A file named `NAME (conflict HOST DATE).ext` is a copy sync kept when an asset changed both here and
in the store; it is never pushed. Compare it with the asset, then delete it or rename it.

Exit 3 from `assets push` means something was left for a conflict: a pointer changed or was deleted
in the store since this directory synced (`sync`, then push again), or the file here is neither the
pointer's bytes nor what this directory last had (`conflict`: move it aside and `pull` to compare;
`push --force` only when this copy should replace the asset store's). A push never overwrites
bytes another pointer still names: it puts the new ones next to them.

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
