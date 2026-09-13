---
name: textdb-cli
description: Browse, read, search and edit markdown/text documents kept in a textdb store (SQLite file or Postgres) with the `textdb` command-line tool — tree, cat with line numbers and versions, full-text search, line-range and anchored edits that rebase over concurrent writers, conflict handling via exit status 3, history, diffs and following live changes. Use when a corpus lives in a textdb store rather than on disk, or when asked to edit documents other people or agents may be editing at the same time.
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

Paths look like `/folder/file.md`. Writing them without the leading slash (`folder/file.md`)
also works and is immune to shell rewriting. Add `--json` to any command for machine-readable
output.

## Find your way around

```sh
textdb tree -L 2                         # top of the tree with file counts
textdb tree guides -d                    # folders only under /guides
textdb ls guides/api
textdb search 'rate limit' -p guides     # path:line: snippet — terms ANDed, "phrase", prefix*
textdb stat guides/api/index.md          # version, size, lines, last author
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

# Replace text that occurs exactly once (no version needed).
textdb edit guides/api/index.md --old 'deprecated in 2.0' --new 'removed in 3.0'

# Multi-line anchors without shell quoting trouble:
printf '%s' '{"old": "## Errors\n\nOld intro.", "new": "## Errors\n\nNew intro."}' | textdb edit guides/api/index.md --stdin-json

# Append to a log or journal (never conflicts).
textdb append notes/journal.md '- 2026-09-13: reviewed the API guide'

# Whole-document rewrite derived from v12.
textdb write guides/api/index.md -b 12 -m 'restructure' < new-index.md

# Create a new file.
textdb write guides/api/limits.md -m 'new page' <<'EOF'
# Limits
EOF
```

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
textdb history guides/api/index.md       # who changed it, when, and how each change landed
textdb diff guides/api/index.md 10 12    # unified diff between versions
textdb hunks guides/api/index.md         # what the latest commit changed, line by line
textdb cat guides/api/index.md -v 10     # an old version
textdb log --since 0 --limit 50          # recent changes across the corpus
textdb watch -p guides --json            # follow changes live (runs until stopped)
```

## Rules

1. Read with `cat -n` and pass the header's version as `-b` for line-number edits.
2. Keep edits small and local; many agents and people may be editing the same document.
3. On exit status 3, rebuild on `theirs`; never overwrite with a stale whole-document `write`
   that lacks `-b`.
4. Use `append` for journals and logs.
5. Use your own `TEXTDB_AUTHOR`, so the changes you make are attributed to you.
