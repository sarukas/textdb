# textdb CLI

`textdb` reads, searches and edits a textdb store from the command line. One binary works
against a SQLite file and against a Postgres database with the `textdb_pg` extension, and it
is built to be driven by agents as much as by people:

- every command can answer in JSON (`--json`), errors included;
- a failed write exits with a status that says why;
- a conflict carries the current text of the contested lines, so the caller can rebuild its
  change without reading the file again;
- line-number edits are made against the version that was read, and rebased over whatever
  landed since.

```sh
cargo build --release -p textdb-cli        # target/release/textdb (textdb.exe on Windows)
```

SQLite is compiled in; nothing else needs to be installed. Postgres needs the `textdb_pg`
extension built from this repository (see [INSTALL.md](INSTALL.md)).

## Choosing a store

| Setting | Flag | Environment | Default |
|---|---|---|---|
| Store | `--store`, `-s` | `TEXTDB_STORE` | `kb.db` |
| Author of writes | `--author`, `-a` | `TEXTDB_AUTHOR` | `cli` |
| JSON output | `--json` | | off |
| Record renames, moves and deletes | `--path-history on\|off` | `TEXTDB_PATH_HISTORY` | the store's `path_history` setting, which is on unless changed with `textdb setting path_history off` |

A store is a SQLite path (`kb.db`, `C:\data\kb.db`, `sqlite:kb.db`, `sqlite:///kb.db` relative,
`sqlite:////srv/kb.db` absolute) or a Postgres URL (`postgres://user@host:5432/db`).
`textdb config` prints each setting and where it came from, with passwords hidden.

An agent is best given its own author name, so its changes are attributed in history, in the
change log and in the live web UI:

```sh
export TEXTDB_STORE=/srv/corpus/kb.db TEXTDB_AUTHOR=agent-7
```

**Git Bash on Windows** rewrites arguments that start with `/` into Windows paths before the
program sees them (`/guides/a.md` becomes `C:/Program Files/Git/guides/a.md`). Store paths
can be written without the leading slash (`guides/a.md` means `/guides/a.md`), or set
`MSYS_NO_PATHCONV=1`. `textdb` refuses a drive-letter path with this explanation rather than
reporting it as not found.

## Working with an external agent

The CLI is all an agent needs: no server, no daemon, no SDK. Give each agent its own shell
environment and its own author name, and the instructions in
[`skills/textdb-cli/SKILL.md`](../skills/textdb-cli/SKILL.md).

1. **Build once:** `cargo build --release -p textdb-cli` gives `target/release/textdb`
   (`textdb.exe` on Windows). On Windows a running binary is locked, so if you keep
   rebuilding while agents work, point the agents at a copy.
2. **Set the agent's environment** in the shell its commands run in:

   ```sh
   export TEXTDB_STORE=/srv/corpus/kb.db           # a SQLite file, or postgres://user@host/db
   export TEXTDB_AUTHOR=agent-7                    # one name per agent: history, the log and the web UI show it
   export MSYS_NO_PATHCONV=1                       # Git Bash on Windows only
   export PATH=/path/to/textdb/target/release:$PATH
   textdb config                                   # prints store, author and path history, and where each came from
   ```

   Windows `cmd` (a `set` value takes no quotes; the setting lasts for that window):

   ```bat
   set TEXTDB_STORE=C:\data\kb.db
   set TEXTDB_AUTHOR=agent-7
   set PATH=C:\path\to\textdb\target\release;%PATH%
   textdb config
   ```

   PowerShell: `$env:TEXTDB_STORE = 'C:\data\kb.db'; $env:TEXTDB_AUTHOR = 'agent-7'`. In `cmd`
   and PowerShell a store path such as `/guides/a.md` reaches the program unchanged — only Git
   Bash rewrites it. For multi-line text in `cmd`, put it in a file and pass `-f FILE`
   (`echo … |` would add CRLF line endings).

3. **Give it the instructions.** For Claude Code, copy `skills/textdb-cli` into the project's
   `.claude/skills/` (or `~/.claude/skills/`); any other agent gets the contents of `SKILL.md`
   in its prompt. A one-line brief that works:

   > The documents are in a textdb store; use only the `textdb` command, as described in
   > SKILL.md. Read with `textdb cat -n`, edit with `replace-lines -b <version>` or
   > `edit --old/--new`, and on exit status 3 rebuild your change on the `theirs` text from
   > the error and retry with the new version.

4. **The loop it should follow:** find (`tree`, `ls`, `search`) → read (`cat -n`, remember
   `vN` from the header) → change (`replace-lines PATH FROM TO -b N`, `edit --old … --new …`,
   `append` for journals) → check (`hunks PATH`, `history PATH`). Add `--json` to any command
   for machine-readable output; a refused write exits with 3 (conflict, payload has `theirs`
   and `current_version`), 4 (store busy: retry), 5 (not found), 6 (invalid edit).
5. **Follow what others do:** `textdb watch --json -p /some/folder` prints one JSON line per
   change as it commits (SQLite: within ~100 ms; Postgres: on `NOTIFY`), and
   `textdb log --since SEQ` reads the change log from a known point.
6. **Alongside the web app:** the agent and the [demo app](demo-app.md) share the store file
   directly; the agent's commits appear in open browsers attributed to `TEXTDB_AUTHOR`. The
   server does not need to be running for the CLI to work.
7. **Reorganising in bulk:** `--path-history off` (or `TEXTDB_PATH_HISTORY=off`) keeps a large
   scripted reshuffle out of every file's history for that command.

## Commands

| Command | What it does |
|---|---|
| `init` | Create the store, or upgrade one an older build wrote; prints the last change number |
| `config` | Show settings and their sources |
| `import DIR [--prefix /p] [--ext md,markdown,mdx,txt] [--batch 500]` | Load matching files; unchanged files make no new version. `.git`, `.textdb`, `.trash` and `node_modules` directories are skipped; other hidden directories (`.claude`, `.github`) are read, as by `sync` |
| `export PREFIX DIR [--dry-run]` | Write the files under a folder to disk, byte for byte (line endings, BOM). Only new and changed files are written and nothing on disk is deleted, so exporting over a git checkout shows only real changes; an existing file is overwritten in place and keeps its permissions, a symbolic link is left alone. Names that cannot coexist on this computer (differing only in letter case on Windows and macOS, or in Unicode normalization on macOS; Windows reserved names, forbidden characters, trailing dot or space; clashes with what is on disk) stop the export before anything is written, with exit code 6 and the list; problems only on other systems are warnings. `--dry-run` lists what would be written. `--json` gives `{ new, changed, unchanged, skipped, problems, stopped, written, bytes }` |
| `sync PREFIX DIR [--dry-run] [--commit] [--base REV] [--ext md,markdown,mdx,txt]` | Reconcile a folder with a directory both ways against what both held at the last sync (recorded in the store): changes, new files, deletes and moves on either side are carried across; edits on both sides are merged line by line, and where they overlap the file on disk gets `<<<<<<< textdb` / `>>>>>>> disk` markers (exit code 3) and the store keeps its version until they are resolved. Files never synced are left alone. In a git checkout, changes that came from git are committed to the store under their git author and subject; `--commit` commits what sync wrote to disk with `Textdb-*` trailers. See [Syncing with a git checkout](#syncing-with-a-git-checkout). Each sync records its include rules (extensions, skipped folders, `.textdbignore`); when they changed and would take in files the last sync left out, sync lists them and stops (exit 6) unless `--accept-rules`. `.textdbignore` in the directory (`.gitignore` syntax, in any letter case on Windows and macOS) leaves files out both ways: they are not taken in, and a store file it matches is never written, moved or deleted on disk (listed as skipped). It is the directory's own: sync never writes it from the store. An Obsidian vault (a directory with `.obsidian`) that never had one gets one at its next sync leaving out `.obsidian/plugins/`, `snippets/` and `themes/`, the code and styles Obsidian loads; delete those lines to sync them. When the store moved a whole folder, files textdb does not track (images, JSON, `.base`) move on disk with it (`disk carried`); folders whose text files left but that still hold such files are listed as `left behind`. Directories holding no files are listed, and removed with `--prune-empty-dirs`. Asset pointers are paired with their files: a pointer moved in the store moves its file, one deleted there sends it to `.textdb/trash/`, a file renamed on disk takes its pointer along, and a file changed both here and in the store is kept as `NAME (conflict HOST DATE).ext` while the store's bytes are pulled. `--push` and `--pull` push and pull assets after the documents (else the `asset_sync` setting decides); assets that fail exit 1, assets left for a conflict exit 3; a changed `.gitattributes` stops automatic pushes until `--accept-rules`; see [Assets](#assets-binaries-next-to-the-text) |
| `sql [STATEMENT \| -f FILE] [-p VALUE]… [--write [--dry-run]] [--format table\|tsv\|lines\|json] [--full]` | One SQL statement (argument, `-f FILE` or stdin) against the store, printed as a table, TSV, one value per line or, with `--json`, `{ columns, rows, row_count, store_changes, batch }`. `--write --dry-run` runs it, prints each file's diff and the moves and deletes, and undoes it all; a real write prints the batch id that `revert-batch` undoes. Views `files`, `folders`, `frontmatter`, `sections`, `links`, `commits`, `authors` besides `kb` and the `textdb_*` functions. Read-only unless `--write`; see [Querying with SQL](#querying-with-sql) |
| `revert-batch BATCH [--skip-changed] [--dry-run]` | Undo what one `sql --write` run changed: files get their content from before the batch back (as a new version), files it created are deleted, moves are undone, and files it deleted are created again (new files; the deleted ones keep their history in the trash). When anything in the batch changed since, nothing is reverted (exit 6) unless `--skip-changed`, which reverts the rest and lists what it left. The revert is a batch itself. SQLite stores |
| `git-status PREFIX DIR [--rev REV]` | When the folder was synced and with which commit, what changed in the store since, and how it compares with a commit (`HEAD` by default) by git blob id: same (CRLF-only differences noted), differ, only in textdb, only in git |
| `ls [PATH] [-l \| -1] [-S KEY] [-r] [-R]` | One folder: folders first, then files with size and line count. `-1` (`--paths`) prints only the paths, one per line, for scripts. `-l` adds words, versions, last update, and a file's authors (commits each) or a folder's contents; a folder's size, lines, words and versions are totals of everything below it. `--sort` by `name`, `type`, `size`, `lines`, `words`, `versions`, `created`, `updated` or `authors`; `-r` reverses; `-R` lists everything below the folder by path |
| `tree [PATH] [-L DEPTH] [-d]` | The folder tree with file counts and sizes; `--json` gives a flat, path-sorted list |
| `stat PATH` | Kind, version, size, lines, last update and author |
| `cat PATH [-n] [--lines A:B] [--version V] [--section HEADING]` | Content; `-n` numbers lines under a header `PATH vN · lines A-B of T` |
| `search WORD… [-p PREFIX] [--limit N] [--per-file N]` | Full text: every word must occur in the document, `"phrases"`, `prefix*`, case and accents ignored. Prints `path:line: text` for each line holding a word (up to `--per-file`, default 10), checked against the text; a document is listed only when its lines hold every word. No match: nothing on stdout, `no matches` on stderr, exit 0 |
| `grep PATTERN [-p PREFIX] [-i] [-F] [-l] [--limit N]` | Regular expression per line over every file under a folder (case-sensitive unless `-i`; `-F` plain text; `-l` paths only). Reads each file, so slower than `search` on large folders; stops after `--limit` lines (500) |
| `write PATH [-b V] [-f FILE] [-m MSG] [--allow-empty] [--create]` | Create or replace from `--file` or stdin; with `-b`, concurrent commits are rebased. `--create` refuses (exit 6) when the file exists |
| `edit PATH --old TEXT --new TEXT [-m MSG]` | Replace the one occurrence of `old`. Also `--old-file`/`--new-file`, or `--stdin-json` reading `{"old": …, "new": …}` |
| `replace-lines PATH FROM TO [-b V] [--text T \| -f FILE \| stdin] [-m MSG]` | Replace lines `FROM..TO` (1-based, inclusive) as numbered in version `V`; `TO = FROM-1` inserts before `FROM` |
| `replace-lines PATH --stdin-json [-b V] [-m MSG]` | Several ranges in one commit, from stdin: `[{"from": N, "to": N, "text": "…"}, …]`, all numbered as in version `V`, in any order. Overlapping ranges, or ranges outside the file, change nothing (exit 6) |
| `meta get PATH [KEY]` | A front matter value: a string as it is, list items one per line; `--json` gives the JSON value. Without `KEY`, the front matter as written (`--json`: the object). A missing key exits 5 |
| `meta set PATH KEY VALUE… [--list] [--raw] [-m MSG]` | Set a top-level key, replacing only its lines, or add it before the closing `---` (front matter is created when the file has none). One value is a string, quoted only when YAML needs it; several values or `--list` a block list indented like the file's other lists; `--raw` writes YAML as given. Line endings, comments and other keys are untouched. Committed against the version read, so other edits to the file rebase. Values starting with `-` go after `--` |
| `meta unset PATH KEY [-m MSG]` | Remove a top-level key and its lines; nothing to remove makes no version |
| `append PATH [TEXT] [-m MSG]` | Append the argument (as a line) or stdin; never conflicts |
| `history PATH [--versions-only]` | Versions — time, author, how each landed (`direct`, `rebased`, `merged`) and its base — and, between them, the renames, moves and deletes that touched the file, including those of a folder it was in. A deleted file is found at the path it was deleted from |
| `diff PATH V1 [V2]` | Unified diff; `V2` defaults to the current version |
| `hunks PATH [V1 [V2]]` | Line hunks; defaults to the latest commit |
| `chunks PATH [--version V]` | The content-defined chunks the file is stored as |
| `mv FROM TO [-m MSG]`, `rm PATH [-m MSG]` | Move/rename and delete files or folders. History stays readable, and each file or folder touched gets a `rename`, `move` or `delete` entry in its history while path history is on. Folders left empty are removed (listed as `removed empty folder …`) unless `--keep-empty-folders`; `-m` is recorded in the change log. Links that pointed at what moved and no longer reach it are listed, or rewritten with `--update-links` (see `link_updates`); `rm` lists the links it leaves broken |
| `links [PATH] [--broken [--dir DIR]]` | The links written in a file or every file below a folder: `path:line: [[target]] -> /resolved/path` with a status — `ok`, `ambiguous` (several files match; the nearest is taken), `anchor-missing`, `broken`, `not-in-store` (PDFs, images and other files a text store does not hold) or `external` (URLs, emails, `?tab=` queries, numbered references). Resolved by Obsidian's rules: markdown links relative to the note, `[[a/b]]` from the vault root, `[[name]]` by file name anywhere, `.md` optional, `#heading` checked (block `^ids` are not). `--broken` lists only what does not resolve; with `--dir`, links to files the store does not hold are looked for on disk. SQLite stores |
| `backlinks PATH` | The links in any file that resolve to a file, or to a file below a folder |
| `setting [KEY [VALUE]]` | Show or change a store setting. `path_history` is `on` (default) or `off`; `default` clears it. `--path-history` overrides it for one command. `link_updates` is what a move does to links that pointed at what moved: `report` (default: list them), `rewrite` (rewrite them, one commit per linking file) or `off`; moves made by `sync` never rewrite links. `asset_sync` is what `sync` does with assets without `--push`/`--pull`: `off` (default: list them), `push`, `pull` or `both`; `asset_pull` is which it pulls: `linked` (default: what the folder's notes link to) or `all` |
| `log [--since SEQ] [--limit N]` | The change log: every create, commit, mkdir, move and delete, in order |
| `watch [--since SEQ] [-p PREFIX]` | Follow the change log live — one line per change, JSON lines with `--json` |

## Querying with SQL

`textdb sql` runs one statement and prints its rows: a table (values cut at 60 characters, and
the row count says so when any were; `--full` for all of them), `--format tsv` (a header, then
tab-separated rows with tabs, newlines and backslashes escaped as `\t`, `\n`, `\\`, NULL empty),
`--format lines` (one value per line, for a single column: made for `while read` loops), or with
`--json` `{ columns, rows: [{…}], row_count, store_changes, batch }`. With tsv and lines only the rows
go to stdout; the change summary goes to stderr. The statement comes from the argument, from a file
with `-f FILE`, or, when both are omitted, stdin — a file or a quoted heredoc avoids shell quoting
trouble. `-p VALUE` binds the next `?` (`?1` can be reused); in Postgres `$1`, `$2`, …
receive text, so cast where needed (`$1::int`).

Besides `kb`, the `textdb_*` table-valued functions (`textdb_ls(dir, recursive)`,
`textdb_search(query, prefix)`, `textdb_history(path)`, …) and scalar functions
(`textdb_content(path)`, `textdb_section(path, heading)`, …), these views describe the live store
by path, deleted files left out:

| View | Columns |
|---|---|
| `files` | `id, path, name, dir` (`/accounts/acme`), `depth` (`/a.md` is 1), `ext` (lower case, `''` without one), `version, nbytes, nlines, nwords, nauthors, created_at, updated_at, updated_by` |
| `folders` | `id, path, name, parent, depth, files, folders, nbytes, nlines, nwords, versions, updated_at` — totals of everything below |
| `frontmatter` | `path, data` — a document's YAML front matter as JSON (text in SQLite: `json_extract`, `json_each`; `jsonb` in Postgres) |
| `sections` | `path, heading` (`Title / Section / Subsection`), `level, line_from, line_to` |
| `links` | `path, target` (without `#anchor` or `\|alias`; markdown links decoded), `line, kind` (`wiki`, `embed`, `md`, `image`), `anchor, alias, status` (`ok`, `ambiguous`, `anchor-missing`, `broken`, `not-in-store`, `external`), `resolved` (the path it points to). SQLite stores; Postgres has `path, target, line` |
| `commits` | `path, version, author, ts, message, kind, base_version, nbytes, nlines, batch` (`batch` in SQLite: the `sql --write` run that made it) |
| `authors` | `path, author, commits, first_ts, last_ts` |

```sh
textdb sql <<'SQL'
SELECT path, json_extract(data, '$.status') AS status
FROM frontmatter WHERE json_extract(data, '$.type') = 'account' ORDER BY path
SQL
textdb sql -p Acme <<'SQL'
SELECT f.path FROM frontmatter f, json_each(f.data, '$.related_accounts') r WHERE r.value = ?
SQL
textdb sql -p guides/intro.md <<'SQL'
SELECT path, line FROM links WHERE target = ?1 OR target LIKE '%/' || ?1
SQL
textdb sql -p '%/ Next steps' 'SELECT path, line_from FROM sections WHERE heading LIKE ?'
textdb sql 'SELECT path, nwords FROM files ORDER BY nwords DESC LIMIT 10'
```

Avoid `SELECT content FROM kb` over many files: it reads every document in full. Use the views, or
`textdb_content(path)` for the few files you need.

**Patterns.** In `LIKE`, `_` and `%` are wildcards, so `path LIKE '/work_files/%'` also matches
`/workXfiles/`; escape them (`LIKE '/work\_files/%' ESCAPE '\'`) or compare with `instr`/`substr`, or
use the `dir`, `depth` and `ext` columns. Character classes such as `[0-9]` only work with `GLOB`
(case-sensitive, `*` and `?`), never with `LIKE`.

```sh
textdb sql --format lines "SELECT path FROM files WHERE dir = '/accounts/acme' AND ext = 'md'"
textdb sql "SELECT substr(dir, 11) AS account, count(*) FROM files WHERE depth = 3 AND path GLOB '/accounts/*' GROUP BY account"
```

**Full-text search in SQL.** `textdb_search(query, prefix)` returns one row per document that holds
every term (terms are ANDed per document; `"a phrase"`, `prefix*`; a term with punctuation such as
`teo-group` or `2026-02` is matched as the phrase of its words, no quoting needed). `line` and
`snippet` come from one chunk of the document, the best-ranked one for the first term: the line in
that chunk holding the most terms. The document holds all the terms, but that line may hold only some
of them, so confirm with `textdb_lines(path, line, line)` before relying on it. The `search` command
instead checks every line and lists each one that holds a term.

**Changing documents.** Statements are read-only unless `--write`. With it, change the store
through `kb` (`INSERT`, `UPDATE`, `DELETE`) and the functions `textdb_write`, `textdb_edit`,
`textdb_append`, `textdb_replace_lines`, `textdb_move` and `textdb_delete`: each makes versions,
history and change-feed entries like any other edit. In SQLite `:author` is bound to `--author`, so
pass it on. A `--write` statement is all or nothing: if any row fails (an `--old` text that is not
found, a conflict), every change the statement made is undone. Statements that name the internal
tables (`kb_*` in SQLite, `kb.node` and friends in Postgres) are refused with `--write`.

```sh
textdb --author agent-7 sql --write <<'SQL'
SELECT path, textdb_edit(path, 'status: draft', 'status: published', :author) AS version
FROM frontmatter WHERE path LIKE '/guides/%' AND json_extract(data, '$.status') = 'draft'
SQL
```

| Function (SQLite) | Does |
|---|---|
| `textdb_edit(path, old, new[, author])` | Replaces `old`, which must occur exactly once |
| `textdb_replace(path, old, new[, expected_count[, author[, message]]])` | Replaces every occurrence, as one version. With `expected_count` the file must hold exactly that many, otherwise at least one; a mismatch fails the statement |
| `textdb_replace_many(path, replacements[, author[, message]])` | Several replacements applied in order, as one version: `replacements` is a JSON array of `["old", "new"]`, `["old", "new", count]` or `{"old", "new", "count"}` |
| `textdb_append(path, text[, author])`, `textdb_write(path, content[, base_version[, author[, message]]])`, `textdb_replace_lines(path, from, to, text[, base_version[, author]])` | As the commands of the same name |
| `textdb_move(from, to[, author])`, `textdb_delete(path[, author])` | Move or delete a file or a whole folder, inside the statement's transaction |

```sh
textdb --author agent-7 sql --write --dry-run <<'SQL'
SELECT path, textdb_replace_many(path, '[["Acme Corp", "Acme"], ["- [ ] call", "- [x] call", 1]]', :author) AS version
FROM files WHERE dir = '/accounts/acme' AND instr(textdb_content(path), 'Acme Corp') > 0
SQL
textdb --author agent-7 sql --write "SELECT textdb_delete(path, :author) FROM files WHERE path GLOB '/drafts/*' AND nwords = 0"
```

`--dry-run` runs the statement in its transaction, prints every file's unified diff (and the moves
and deletes), and rolls it all back; `--json` gives the same as `changes: [{op, path, old_path,
from_version, to_version, diff}]`. A real `--write` records its commits, moves and deletes under a
batch id, printed after the statement (`batch 20260914-211500-3f9a: …`) and kept in the `commits`
view's `batch` column. `textdb revert-batch ID` undoes the batch; `--dry-run` shows what it would do.
Another client can record its own batch with `SELECT textdb_batch('id')` before writing and
`textdb_batch(NULL)` after.

In a Postgres store the same statements use the extension's functions: `kb.replace`,
`kb.replace_many`, `kb.write`, `kb.append`, `kb.edit`, `kb.replace_lines`, `kb.move`, `kb.remove`
and `kb.content` (see [USAGE.md](USAGE.md)); positional parameters are `$1`, `$2`, … and
`:author` is bound as in SQLite. Batches, `--dry-run` diffs and `revert-batch` work the same; a
client records its own batch with `SELECT set_config('textdb.batch', 'id', true)` in its
transaction.

## Syncing with a git checkout

`sync` keeps a folder in the store and a directory reconciled, typically a git checkout that
others change too:

```sh
git -C ~/src/handbook pull                     # the checkout moves on: files added, edited, removed
textdb sync /handbook ~/src/handbook --dry-run # what would change on each side
textdb sync /handbook ~/src/handbook --commit  # carry changes both ways, commit what landed on disk
git -C ~/src/handbook push
```

- **The sync base.** After each sync the store records, per folder and directory, every file's
  version and git blob id, plus the checkout's commit, branch and remote. The next sync compares
  both sides with it, so it knows which side changed a file, and that a file missing on one side
  was deleted there rather than added on the other.
- **Both sides changed a file:** the edits are merged line by line (the base content comes from
  the store's history). Overlapping edits get conflict markers in the file on disk; the store
  keeps its version. Resolve the file and sync again; while markers remain it is reported as
  `unresolved` and not taken in. Deleting the marked file writes the store's version back.
- **Deleted on one side:** deleted on the other, unless it changed there, in which case the
  changed file is kept. A delete and an identical new file on disk become a move in the store,
  keeping the file's history. Only files the base or the store knows can be deleted: images,
  other types and anything `.gitignore` excludes are never touched.
- **Git authors.** Changes that came in with commits since the last sync are committed to the
  store as their git author, with a message like `git 1a2b3c4: Fix the intro`.
- **`--commit`** stages and commits only the files sync wrote or deleted on disk, leaving your
  other uncommitted work alone. The message says who changed them in textdb and ends with
  trailers: `Textdb-Store`, `Textdb-Prefix`, `Textdb-Seq` (the store's change number) and one
  `Textdb-Author` per author.
- **The first sync** of a store that was imported earlier has no base, so a file that differs on
  the two sides is a conflict. Pass `--base REV`, the commit the import was made from, and git
  supplies the base instead: `textdb sync /handbook ~/src/handbook --base 3f9c2e1`.
- **Line endings.** Content stays byte-exact. With `core.autocrlf` the checkout has CRLF while git
  stores LF; `git-status` counts such files as the same and says how many differ only by line
  endings.
- **Names** that cannot exist side by side on this computer stop the sync before anything is
  written, as for `export`.

`textdb git-status /handbook ~/src/handbook` shows when the folder was last synced (commit,
branch, clean or not), what changed in the store since, and how the store compares with `HEAD`
or `--rev REV`.

## Assets: binaries next to the text

Images, PDFs, office files and other binaries stay out of git and out of the store's text: their
bytes live in an asset store (a shared folder, or a Google shared drive or SharePoint library
through rclone), and each has a small pointer document `NAME.tdbasset` next to where the file
belongs, versioned in the store and in git like any document. The design is in
[assets.md](assets.md).

```sh
textdb assets stores --add team --root 'G:\Shared drives\Team\textdb'   # declared once, for everyone
textdb assets stores --bind team='/Volumes/GoogleDrive/Shared drives/Team/textdb'   # where this computer reaches it
textdb assets stores --add drive --driver rclone --root teamdrive:textdb   # or through an rclone remote
textdb sync /handbook ~/src/handbook               # the vault: a directory synced with a folder
textdb assets status /handbook                     # ok, new, modified, outdated, conflict, not-pulled, conflict-copy
textdb assets push /handbook -m "diagrams"         # upload and check the bytes, then commit the pointers
textdb assets pull --linked-from /handbook/guides  # only what those notes link to
textdb assets verify /handbook                     # every hash, here and in the asset store (exit 1 on problems)
textdb assets gitignore /handbook                  # the managed .gitignore block: assets out, pointers in
textdb assets migrate-from-git /handbook           # binaries git tracks: pushed, out of git's index, one commit
textdb sync /handbook ~/src/handbook --push --pull # documents, then assets both ways
textdb setting asset_sync both                     # make that what every sync of this store does
```

- **Which files are assets:** images, PDF, office documents, archives, audio, video, fonts and
  diagram files by default; the vault's `.gitattributes` decides with a `textdb` attribute
  (`*.dat textdb=asset`, `*.svg textdb=document`, `*.log textdb=ignore`, `!textdb` for the
  default), with git's rules for patterns and precedence. A file nothing decides is an asset when
  it has a NUL byte in its first 8000 bytes.
- **Asset stores:** `local` is a folder this computer reaches (a NAS, a USB disk, a cloud drive
  synced to a folder); `rclone` is an rclone remote path, used with the rclone configuration of
  whoever runs textdb. rclone is `TEXTDB_RCLONE`, else the one next to `textdb` (a vault's
  `.textdb/bin`), else on the PATH; `--bind NAME=REMOTE:PATH` names another remote for the same
  folder on one computer. `assets stores` tells whether each store is reachable.
- **The directory** is the one the store folder was last synced with; `--dir DIR` names it.
- **Links** to an asset resolve to its pointer: `links` shows the asset's path with `asset: true`,
  `backlinks` takes the asset's path, `links --broken --dir DIR` lists assets not pulled into DIR,
  and `mv --update-links` of a pointer rewrites the links to the asset.
- **Moves and deletes:** `mv` and `rm` take an asset's own path and act on its pointer (a pointer
  never moves onto a folder or an existing file, in any letter case, and keeps its `.tdbasset`
  suffix; nor does a document move onto an asset's path); the next `sync` moves the file on disk,
  or moves it to `.textdb/trash/`. A file renamed on disk takes its pointer along at the next sync.
- **Safety:** a push replaces the asset store's copy only when it holds the asset's own bytes and
  no other pointer names them, keeping the copy in the store's `.textdb-trash`; anything else
  there is kept and the upload goes next to it. Pushes of the same file take turns. A pointer is
  committed only once the uploaded copy's hash matches; a pull puts a file in place only after
  its hash matches, and never replaces a file changed locally. A file changed both here and in the
  store is kept as a conflict copy, never overwritten or pushed.

## Exit status

| Status | Meaning | What to do |
|---|---|---|
| 0 | Success | |
| 1 | Other error (store unreachable, I/O) | Read the message |
| 2 | Usage error | Check the arguments |
| 3 | `TX001` conflict: the same lines changed since your base version | Rebuild on `theirs`, retry with `-b current_version` |
| 4 | `TX002` contention: retry budget exhausted on a very hot file | Retry shortly |
| 5 | `TX003` not found | Check the path (`ls`, `tree`) |
| 6 | `TX004` invalid edit: `old` missing or ambiguous, line range outside the file, empty content | Re-read and adjust |

With `--json` an error is printed on stdout as
`{"error": {"code": "TX001", "message": "…", "conflict": {"path", "region_line_from", "region_line_to", "base", "theirs", "ours", "current_version"}}}`.

## Editing safely

```sh
textdb cat -n guides/intro.md --lines 40:60        # header says: /guides/intro.md v12 · lines 40-60 of 310
textdb replace-lines guides/intro.md 44 46 -b 12 <<'EOF'
The new text for what were lines 44 to 46.
EOF
```

- Line numbers mean what they meant in the version you read. If someone else committed to the
  file in the meantime, the edit is rebased onto their version; if they changed the same
  lines, it fails with status 3 and nothing is written.
- `edit --old/--new` needs no version: the anchor text is found in the current content and
  must occur exactly once.
- `write -b V` sends a whole document derived from version `V`; the store diffs it and
  rebases the difference.
- `append` is for logs and journals; it never conflicts.
- A write that changes nothing reports `unchanged` (`kind: noop`) and creates no version.

## Output shapes (`--json`)

| Command | JSON |
|---|---|
| writes | `{"path", "version", "kind"}` |
| `cat` | `{"path", "version", "nlines", "from", "to", "content"}` |
| `ls` | `[{"path", "name", "kind", "nbytes", "nlines", "updated_at", "nwords", "versions", "created_at", "updated_by", "files", "folders", "authors": [{"author", "commits", "last_ts"}]}]`; `files`/`folders` only for folders, `authors` only for files |
| `tree` | `[{"path", "name", "kind", "nbytes", "nlines", "updated_at"}]` |
| `stat` | `{"path", "kind", "version", "nbytes", "nlines", "updated_at", "updated_by"}` |
| `history` | time-ordered `[{"type": "version", "version", "author", "ts", "message", "nbytes", "kind", "base_version"} \| {"type": "path", "id", "ts", "op", "old_path", "new_path", "via", "version", "author"}]`; `op` is `rename`, `move` or `delete`, `via` the folder the operation named when the file went along with it, `version` the file's version at the time. With `--versions-only`, the version objects without `type` |
| `setting` | `{"path_history": {"value": "on" \| "off" \| null, "effective": true \| false}}` |
| `hunks` | `{"path", "from", "to", "hunks": [{"old_from", "old_count", "new_from", "new_count", "old_text", "new_text"}]}` |
| `log`, `watch` | `{"seq", "ts", "op", "path", "old_path", "node_kind", "version", "base_version", "commit_kind", "author", "message"}` |
| `search` | `[{"path", "line", "snippet", "rank"}]`, one per matching line |
| `grep` | `[{"path", "line", "text"}]`; with `-l`, `["path", …]` |
| `import` | `{"dir", "prefix", "stats": {"files", "created", "updated", "unchanged", "failed", "bytes"}, "seconds"}` |

## How `watch` learns about changes

Every change is recorded in a change log in the same transaction as the change itself
(`textdb_feed` in SQLite, `kb.feed` in Postgres), so the log is never ahead of or behind the
data. On SQLite, which cannot notify another process, `watch` polls `PRAGMA data_version` — a
counter that moves only when another connection commits — every 100 ms and reads the log when
it moves. On Postgres it `LISTEN`s on `textdb_change`, which the extension notifies on commit.

## Configuration: today and next

Today a setting is a flag, else an environment variable, else the default. That is enough for
one store per shell and keeps secrets out of files. The layers below are designed but not
built; they slot in between the environment and the defaults, so no command changes.

**Profiles.** A `--profile NAME` flag / `TEXTDB_PROFILE` selects a named block from, in order,
the nearest `.textdb.toml` found walking up from the working directory (a project's corpus)
and the user file (`%APPDATA%\textdb\config.toml`, `~/.config/textdb/config.toml`):

```toml
[profile.docs]
store  = "sqlite:///C:/corpora/docs.db"
author = "agent-docs"

[profile.shared]
store  = "postgres://kb@db.internal:5432/kb?sslmode=verify-full"
author = "agent-7"
credentials = "azure-entra"        # which credential provider supplies the password
prefix = "/team"                   # default folder for ls/tree/search/watch
read_only = true                   # refuse writes, for review agents
```

**Credentials never live in the file.** For Postgres, in order of preference:

1. Short-lived tokens used as the password, obtained per connection by a *credential
   provider*: built-in ones for cloud IAM logins (Azure Entra ID, AWS RDS IAM, GCP Cloud SQL
   IAM), which already have OAuth device-code or workload-identity flows, and a generic
   `credential_command = ["az", "account", "get-access-token", …]` whose stdout is the token.
2. The libpq conventions agents' environments already carry: `PGPASSWORD`, `PGPASSFILE` /
   `pgpass`, `PGSSLROOTCERT`.
3. A password in `TEXTDB_STORE` (hidden by `textdb config`), for local development.

TLS (`sslmode`, root certificates) is required for any provider that sends a token; today the
CLI connects without TLS, which is fine for a local database only.

**A hosted store** (the live app's server, or a future service in front of Postgres) would be a
third store scheme, `https://…`, speaking the HTTP API with a bearer token from the same
credential providers or from an OAuth device-code login whose refresh token is kept in the OS
keychain. Agents would then need no database network access at all, and the server could
enforce per-author permissions that a direct database connection cannot.
