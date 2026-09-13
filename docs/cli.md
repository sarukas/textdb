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

## Commands

| Command | What it does |
|---|---|
| `init` | Create the store, or upgrade one an older build wrote; prints the last change number |
| `config` | Show settings and their sources |
| `import DIR [--prefix /p] [--ext md,markdown,mdx,txt] [--batch 500]` | Load matching files; unchanged files make no new version. Hidden directories and `node_modules` are skipped |
| `export PREFIX DIR` | Write every file under a folder to disk |
| `ls [PATH]` | One folder: folders first, then files with size and line count |
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
| `ls`, `tree` | `[{"path", "name", "kind", "nbytes", "nlines", "updated_at"}]` |
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
