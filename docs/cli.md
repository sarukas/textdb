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
| `import DIR [--prefix /p] [--ext md,markdown,mdx,txt] [--batch 500]` | Load matching files; unchanged files make no new version, and binary files (a NUL byte in their first 8000 bytes) are skipped with a note. `.git`, `.textdb`, `.trash` and `node_modules` directories are skipped; other hidden directories (`.claude`, `.github`) are read, as by `sync` |
| `export PREFIX DIR [--dry-run]` | Write the files under a folder to disk, byte for byte (line endings, BOM). Only new and changed files are written and nothing on disk is deleted, so exporting over a git checkout shows only real changes; an existing file is overwritten in place and keeps its permissions, a symbolic link is left alone. Names that cannot coexist on this computer (differing only in letter case on Windows and macOS, or in Unicode normalization on macOS; Windows reserved names, forbidden characters, trailing dot or space; clashes with what is on disk) stop the export before anything is written, with exit code 6 and the list; problems only on other systems are warnings. `--dry-run` lists what would be written. `--json` gives `{ new, changed, unchanged, skipped, problems, stopped, written, bytes }` |
| `sync [PREFIX] [DIR] [--force] [-q] [--dry-run] [--commit] [--base REV] [--ext md,markdown,mdx,txt] [--lock-timeout SECONDS]` | Reconcile a folder with a directory both ways against what both held at the last sync (recorded in the store): changes, new files, deletes and moves on either side are carried across; edits on both sides are merged line by line, and where they overlap the file on disk gets `<<<<<<< textdb` / `>>>>>>> disk` markers (exit code 3) and the store keeps its version until they are resolved. Files never synced are left alone. In a git checkout, changes that came from git are committed to the store under their git author and subject; `--commit` commits what sync wrote to disk with `Textdb-*` trailers. See [Syncing with a git checkout](#syncing-with-a-git-checkout). Each sync records its include rules (extensions, skipped folders, `.textdbignore`); when they changed and would take in files the last sync left out, sync lists them and stops (exit 6) unless `--accept-rules`. `.textdbignore` in the directory (`.gitignore` syntax, in any letter case on Windows and macOS) leaves files out both ways: they are not taken in, and a store file it matches is never written, moved or deleted on disk (listed as skipped); as in git, a file inside a folder it leaves out stays out whatever a `!` line says. It is the directory's own: sync never writes anything under that name from the store, and stops if the file is there but cannot be read. The first sync of a folder with a directory that goes ahead puts the lines `**/.obsidian/plugins`, `**/.obsidian/snippets` and `**/.obsidian/themes` in its `.textdbignore` (creating the file, or adding those for folders it has no `**/` or `!` line for yet; with `--commit` the file is committed too), so the code and styles Obsidian loads are never written from the store; delete those lines to sync them, and later syncs of that folder do not add them again. `.textdbignore` must be UTF-8. A `.textdbignore` loosened so that files it left out would be taken in, written, merged or deleted stops the sync like other rule changes, until `--accept-rules`. Binary files (a NUL byte in their first 8000 bytes) are never taken in or merged, new or changed: sync lists each as skipped, suggesting `.textdbignore` or an asset rule (or saving UTF-16 text as UTF-8), and both sides keep what they have. When the store moved a whole folder, files textdb does not track (images, JSON, `.base`) move on disk with it (`disk carried`); folders whose text files left but that still hold such files are listed as `left behind`. Directories holding no files are listed, and removed with `--prune-empty-dirs`. Asset pointers are paired with their files: a pointer moved in the store moves its file, one deleted there sends it to `.textdb/trash/`, a file renamed on disk takes its pointer along, and a file changed both here and in the store is kept as `NAME (conflict HOST DATE).ext` while the store's bytes are pulled. `--push` and `--pull` push and pull assets after the documents (else the `asset_sync` setting decides); assets that fail exit 1, assets left for a conflict exit 3; a changed `.gitattributes` stops automatic pushes until `--accept-rules`; see [Assets](#assets-binaries-next-to-the-text). **One sync of a directory at a time:** a sync takes an exclusive lock on `DIR/.textdb/lock` for its whole run, because two at once would each compute both sides from the same base and land one edit twice — which is what a turn-end hook in one agent and a turn-start hook in another produce. A second sync exits 4 at once, naming the holder: a collision is worth seeing rather than absorbing, and exit 4 already means retry shortly. `--lock-timeout SECONDS` queues behind the first instead, for a caller that would rather wait than retry. The lock is advisory (`flock`), so a killed sync releases it without leaving anything to clean up, and its staging files are cleared by the next sync that takes the lock; `--dry-run` neither takes the lock nor waits for it. The sync base is saved with a compare-and-swap as well: a sync that another machine sharing the folder overtook exits 4 without recording its base, so the next sync reconciles against the base that machine left rather than taking the overtaken run's view as the agreed state. The base is written last, so this does not undo what the run already wrote — it is in the store, versioned, and on disk; run sync again. Files are written to `DIR/.textdb/tmp` and renamed into place, so a reader never sees half a note and a sync killed mid-write leaves the old content. `DIR/.textdb` carries a `.gitignore` of `*`, so nothing textdb keeps beside a directory shows up as untracked. **The directory remembers what it is paired with:** a sync that goes ahead writes `DIR/.textdb/config` naming the store, the folder and an id of its own, and every later `textdb sync` finds it by walking up from the current directory — so `textdb sync` with no arguments at all syncs the whole tree from anywhere inside it, with the store it was paired with. `TEXTDB_DIR` names a directory outright and `TEXTDB_CEILING_DIRECTORIES` (`:`-separated) stops the walk, as their git counterparts do; the walk also stops at a filesystem boundary. A folder or store that contradicts the pairing is refused naming what the directory is already paired with, rather than importing the whole tree again under a second name — `--force` pairs it anew. With one argument that argument is the folder in the store, so a lone directory is refused rather than taken as a folder. The store's own file is never synced when it lies inside the directory, `-wal` and `-shm` included, and is reported once. `-q` prints the summary line and what went wrong, not the file-by-file list; a line names what was left out and why (`left out 2 files by extension (app.json, data.csv); --ext to include them`). The Obsidian `.textdbignore` lines are written only where Obsidian is — in the directory or in the folder being synced into it — and directories under an ignored or skipped path are neither counted as empty nor offered to `--prune-empty-dirs`. The sync base follows the directory's id rather than its path, so moving or renaming a directory keeps it — the sync continues where it left off instead of treating every file as new — and the move is reported once |
| `sql [STATEMENT \| -f FILE] [-p VALUE]… [--write [--dry-run]] [--format table\|tsv\|lines\|json] [--full]` | One SQL statement (argument, `-f FILE` or stdin) against the store, printed as a table, TSV, one value per line or, with `--json`, `{ columns, rows, row_count, store_changes, batch }`. `--write --dry-run` runs it, prints each file's diff and the moves and deletes, and undoes it all; a real write prints the batch id that `revert-batch` undoes. Views `files`, `folders`, `frontmatter`, `properties`, `sections`, `links`, `commits`, `authors` besides `kb` and the `textdb_*` functions. Read-only unless `--write`; see [Querying with SQL](#querying-with-sql) |
| `revert-batch BATCH [--skip-changed] [--dry-run]` | Undo what one `sql --write` run changed: files get their content from before the batch back (as a new version), files it created are deleted, moves are undone, and files it deleted are created again (new files; the deleted ones keep their history in the trash). When anything in the batch changed since, nothing is reverted (exit 6) unless `--skip-changed`, which reverts the rest and lists what it left. The revert is a batch itself. Both backends |
| `git-status PREFIX DIR [--rev REV]` | When the folder was synced and with which commit, what changed in the store since, and how it compares with a commit (`HEAD` by default) by git blob id: same (CRLF-only differences noted), differ, only in textdb, only in git |
| `ls [PATH] [-l \| -1] [-S KEY] [-r] [-R]` | One folder: folders first, then files with size and line count. `-1` (`--paths`) prints only the paths, one per line, for scripts; a folder keeps its trailing `/` so a script can tell it from a file with no extension. `ls FILE` lists that one file. `-l` adds words, versions, last update, and a file's authors (commits each) or a folder's contents; a folder's size, lines, words and versions are totals of everything below it. `--sort` by `name`, `type`, `size`, `lines`, `words`, `versions`, `created`, `updated` or `authors`; `-r` reverses; `-R` lists everything below the folder by path |
| `tree [PATH] [-L DEPTH] [-d]` | The folder tree. Every folder row says what it holds all the way down (`guide/  (2 files, 1 folder, 394 B)`), whatever `-L` lets through; `tree FILE` prints the one entry, as `ls FILE` does. `--json` gives a flat, path-sorted list of the same full records `ls --json` returns |
| `stat PATH` | Everything the store knows about one path, one key per line: the full listing record, the same twenty-four keys `ls --json` returns |
| `cat PATH [-n] [--lines A:B] [--version V] [--section HEADING]` | Content; `-n` numbers lines under a header `PATH vN · lines A-B of T` |
| `search WORD… [-p PREFIX] [-l] [-c] [--limit N] [--per-file N]` | Full text: every word must occur in the document, `"phrases"`, `prefix*`, case and accents ignored. Prints `path:line: text` for each line holding a word, checked against the text; a document is listed only when its lines hold every word. `--limit` counts rows (200) and `--per-file` lines from one document (10), with the rest reported as `more`. `-l` lists the matching documents, `-c` each with its count. No match: nothing on stdout, a note on stderr, exit 0 |
| `grep PATTERN [-p PREFIX] [-i] [-F] [-l] [-c] [--limit N] [--per-file N]` | Regular expression per line over every file under a folder (case-sensitive unless `-i`; `-F` plain text). Reads each file, so slower than `search` on large folders. Same rows, same flags and the same meanings of `--limit` and `--per-file` as `search`; `score` is null |
| `links [PATH] [--broken [--dir DIR]]` | The links written in a file or every file below a folder: `path:line: [[target]] -> /resolved/path` with a status — `ok`, `ambiguous` (several files match; the nearest is taken), `anchor-missing`, `broken`, `not-in-store` (PDFs, images and other files a text store does not hold) or `external` (URLs, emails, `?tab=` queries, numbered references). Resolved by Obsidian's rules: markdown links relative to the note, `[[a/b]]` from the vault root, `[[name]]` by file name anywhere, `.md` optional, `#heading` checked (block `^ids` are not). `--broken` lists only what does not resolve; with `--dir`, links to files the store does not hold are looked for on disk. Both backends |
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
| `properties` | `path, key` (dotted: `project.name`), `value, number` (the value as a number when it is one), `ord` (position in a list). One row per value, so a list is one row per element |
| `sections` | `path, heading` (`Title / Section / Subsection`), `level, line_from, line_to, title` (the last component alone), `nwords` (the section's own lines), `nwords_total` (plus everything nested under it), and the document's `nbytes, nlines, file_nwords, version, updated_at, updated_by` |
| `links` | `path, version, line, kind` (`wiki`, `embed`, `md`, `image`), `target` (without `#anchor` or `\|alias`; markdown links decoded), `anchor, alias, status` (`ok`, `ambiguous`, `anchor-missing`, `broken`, `not-in-store`, `external`), `resolved` (the path it points to), `asset` (it resolves to an asset). The same ten columns as `textdb_links` / `kb.links`. Both backends |
| `commits` | `path, version, author, ts, message, kind, base_version, nbytes, nlines, nwords, batch` (`batch` in SQLite: the `sql --write` run that made it) |
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
textdb sql 'SELECT path, nwords FROM files ORDER BY nwords DESC LIMIT 10'
-- Headings are indexed folded, so prefer textdb_outline over LIKE over the view:
textdb sql "SELECT path, line_from FROM textdb_outline('/', 'Next steps')"
textdb sql "SELECT heading, nwords_total FROM textdb_outline('/plan.md') ORDER BY nwords_total DESC"
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

**Full-text search in SQL.** `textdb_search(query, prefix, limit, per_file)` returns one row per
matching *line* — `path, version, line, text, section, score, more` — the same rows the `search`
command prints (terms are ANDed; `"a phrase"`, `prefix*`; a term with punctuation such as
`teo-group` or `2026-02` is matched as the phrase of its words, no quoting needed). The index works
on chunks, so the function reads the matching documents and lists the lines that really hold the
terms: `line` is a fact rather than a guess, and a document whose words only ever appear apart is
dropped. `SELECT DISTINCT path` gives the documents, and `more` says how many lines `per_file` held
back.

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

## Property queries

`meta find` takes the same query language everywhere — CLI, SQL, both SDKs and the web app —
so a query written once means the same thing wherever it is run. Full detail, including what
the index costs, is in [`docs/properties.md`](properties.md).

| Written | Means |
|---|---|
| `status:draft` | equals, ignoring case |
| `tags:telco` | a list contains it; lists are indexed one row per element, so this is the same comparison |
| `project.name:atlas` | nested properties are dotted |
| `title:"quarterly review"` | quote a value with spaces; a quoted value is never read as an operator |
| `priority:>3`, `due:<=2026-10-01` | compare — numerically when both sides are numbers, as text otherwise, so ISO dates sort correctly |
| `has:budget`, `budget:*` | the property is present, whatever it holds |
| `name:atl*`, `note:~telco` | starts with, contains |
| `status:!=draft` | **has** the property, but not with that value |
| `status:draft tags:telco` | a space means AND |
| `a:1 OR b:2` | OR, which binds looser than AND |
| `-status:archived`, `NOT status:archived` | either spelling of NOT |
| `(a OR b) AND c` | parentheses regroup |

`!=` is worth stating plainly: a note with no `status` at all is not a note whose status is
not draft, so `status:draft` and `status:!=draft` partition the corpus exactly rather than
overlapping or leaving a gap.

```sh
textdb meta keys                      # what this vault uses
textdb meta values status             # what that property holds
textdb meta find "status:draft tags:telco" --show status,tags
textdb meta find "priority:>3 -status:archived" --folder /notes
```

An invalid query exits 6 and names the offset it went wrong at, so an editor can point at it.

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
textdb assets status /handbook                     # ok, new, modified, outdated, conflict, not-pulled, conflict-copy, orphan, invalid-path, invalid-pointer
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
| 4 | `TX002` contention: retry budget exhausted on a very hot file, or another sync holds the directory | Retry shortly |
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
| `ls`, `tree` | `[{"path", "name", "kind", "version", "nbytes", "nlines", "updated_at", "updated_by", "id", "dir", "depth", "ext", "title", "nwords", "nsections", "nprops", "nlinks", "nlinks_broken", "versions", "created_at", "files", "folders", "nauthors", "authors": [{"author", "commits", "last_ts"}]}]` — the full listing record, the same twenty-four keys in this order from every listing surface and both backends. Every key is always present; one that does not apply is `null`. `version`, `ext` and `authors` are null or empty for a folder; `files`/`folders` for a file. A folder's figures are totals over everything below it |
| `stat` | one object of the same twenty-four keys |
| `history` | time-ordered `[{"type": "version", "version", "author", "ts", "message", "kind", "base_version", "nbytes", "nlines", "nwords"} \| {"type": "path", "id", "ts", "op", "old_path", "new_path", "via", "version", "author"}]`; `op` is `rename`, `move` or `delete`, `via` the folder the operation named when the file went along with it, `version` the file's version at the time. With `--versions-only`, the version objects without `type` |
| `setting` | `{"path_history": {"value": "on" \| "off" \| null, "effective": true \| false}}` |
| `hunks` | `{"path", "from", "to", "hunks": [{"old_from", "old_count", "new_from", "new_count", "old_text", "new_text"}]}`. The text form uses `@@` headers but is **not** a unified diff — no context lines, no `---`/`+++` — so it cannot be fed to `patch`; `diff` can |
| `log`, `watch` | `{"seq", "ts", "op", "path", "old_path", "node_kind", "version", "base_version", "commit_kind", "author", "message"}` |
| `search`, `grep` | `[{"path", "version", "line", "text", "section", "score", "more"}]`, one row per matching line. `version` is the version the line number belongs to — pass it to `--base-version`. `section` is the heading path the line sits under, for `cat --section`. `score` is relevance, higher is better, scaled to (0, 1]; `null` from `grep`, which ranks nothing. `more` counts matching lines in that file held back by `--per-file`, so truncation is visible. With `-l` or `-c`, `[{"path", "version", "matches"}]` — a flag filters rows, it does not change the row type |
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
