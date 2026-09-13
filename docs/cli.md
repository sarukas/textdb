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
| `import DIR [--prefix /p] [--ext md,markdown,mdx,txt] [--batch 500]` | Load matching files; unchanged files make no new version. Hidden directories and `node_modules` are skipped |
| `export PREFIX DIR [--dry-run]` | Write the files under a folder to disk, byte for byte (line endings, BOM). Only new and changed files are written and nothing on disk is deleted, so exporting over a git checkout shows only real changes; an existing file is overwritten in place and keeps its permissions, a symbolic link is left alone. Names that cannot coexist on this computer (differing only in letter case on Windows and macOS, or in Unicode normalization on macOS; Windows reserved names, forbidden characters, trailing dot or space; clashes with what is on disk) stop the export before anything is written, with exit code 6 and the list; problems only on other systems are warnings. `--dry-run` lists what would be written. `--json` gives `{ new, changed, unchanged, skipped, problems, stopped, written, bytes }` |
| `ls [PATH] [-l] [-S KEY] [-r] [-R]` | One folder: folders first, then files with size and line count. `-l` adds words, versions, last update, and a file's authors (commits each) or a folder's contents; a folder's size, lines, words and versions are totals of everything below it. `--sort` by `name`, `type`, `size`, `lines`, `words`, `versions`, `created`, `updated` or `authors`; `-r` reverses; `-R` lists everything below the folder by path |
| `tree [PATH] [-L DEPTH] [-d]` | The folder tree with file counts and sizes; `--json` gives a flat, path-sorted list |
| `stat PATH` | Kind, version, size, lines, last update and author |
| `cat PATH [-n] [--lines A:B] [--version V] [--section HEADING]` | Content; `-n` numbers lines under a header `PATH vN · lines A-B of T` |
| `search QUERY [-p PREFIX] [--limit N]` | Full text: terms ANDed per document, `"phrases"`, `prefix*`; prints `path:line: snippet` |
| `write PATH [-b V] [-f FILE] [-m MSG] [--allow-empty]` | Create or replace from `--file` or stdin; with `-b`, concurrent commits are rebased |
| `edit PATH --old TEXT --new TEXT` | Replace the one occurrence of `old`. Also `--old-file`/`--new-file`, or `--stdin-json` reading `{"old": …, "new": …}` |
| `replace-lines PATH FROM TO [-b V] [--text T \| -f FILE \| stdin]` | Replace lines `FROM..TO` (1-based, inclusive) as numbered in version `V`; `TO = FROM-1` inserts before `FROM` |
| `append PATH [TEXT]` | Append the argument (as a line) or stdin; never conflicts |
| `history PATH [--versions-only]` | Versions — time, author, how each landed (`direct`, `rebased`, `merged`) and its base — and, between them, the renames, moves and deletes that touched the file, including those of a folder it was in. A deleted file is found at the path it was deleted from |
| `diff PATH V1 [V2]` | Unified diff; `V2` defaults to the current version |
| `hunks PATH [V1 [V2]]` | Line hunks; defaults to the latest commit |
| `chunks PATH [--version V]` | The content-defined chunks the file is stored as |
| `mv FROM TO`, `rm PATH` | Move/rename and delete files or folders. History stays readable, and each file or folder touched gets a `rename`, `move` or `delete` entry in its history while path history is on |
| `setting [KEY [VALUE]]` | Show or change a store setting. `path_history` is `on` (default) or `off`; `default` clears it. `--path-history` overrides it for one command |
| `log [--since SEQ] [--limit N]` | The change log: every create, commit, mkdir, move and delete, in order |
| `watch [--since SEQ] [-p PREFIX]` | Follow the change log live — one line per change, JSON lines with `--json` |

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
| `search` | `[{"path", "line", "snippet", "rank"}]` |
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
