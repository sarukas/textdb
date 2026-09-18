# textdb

Versioned, chunk-shared text documents inside an SQL database, with compare-and-swap
commits and automatic rebase — so many humans and AI agents can read, search and edit a
markdown knowledge base concurrently without git push/pull.

This repository is the proof of concept described in [`docs/spec.md`](docs/spec.md)
(issue #2) together with the comparative test suite from [`docs/test-suite.md`](docs/test-suite.md)
(issue #1).

- **Install:** [`docs/INSTALL.md`](docs/INSTALL.md) · **Use:** [`docs/USAGE.md`](docs/USAGE.md)
- **Benchmark results:** [`bench/RESULTS.md`](bench/RESULTS.md) (raw data in `bench/results/`)
- **Command line:** [`docs/cli.md`](docs/cli.md) — `textdb` for SQLite and Postgres stores: `ls -l` sorted by
  size, words, versions, update time or authors, tree, cat, search, line-range and anchored edits that rebase
  over concurrent writers, history with renames/moves/deletes, hunks, `watch` for live changes, `sql` for queries over
  files, front matter, sections, links and commits, and `sync` to
  reconcile a folder with a git checkout both ways (three-way merge, conflict markers, git authors, commit trailers).
  [Working with an external agent](docs/cli.md#working-with-an-external-agent).
- **Delegated access:** [`docs/cli.md`](docs/cli.md#delegating-folders-to-accounts) — one central store, with
  folders shared to **accounts** that hold bearer tokens. A share is a folder and everything below it, and the
  account sees it at its own root under an alias of its own: `/legal/contracts` is `/contracts/` to whoever
  holds it, so a local checkout is a working vault rather than a slice of someone else's tree. Rights are `ro`
  or `rw`, links are rewritten into each reader's paths and hidden targets become `textdb:<id>` references that
  disclose nothing, and a revoked share answers `forbidden` rather than `not found` so nobody's checkout is
  emptied by a permission change. The same model on both engines: `textdb_auth()` on SQLite, `SET textdb.token`
  on Postgres.
- **Demo app:** [`docs/demo-app.md`](docs/demo-app.md) — build, start and configure the live corpus app: a Node
  server and web UI where an agent's edits appear in the open viewer or editor as they land, attributed, with
  history and diffs. A GitHub-style folder view lists any folder with infinite scroll, sortable by name, type,
  size, lines, words, versions, created, updated and authors, with filters, content search and bulk move/delete;
  plus folder import, export back to disk (only changed files, so a git checkout shows real changes; names that
  clash on Windows or macOS are caught first), rename/move/delete, a trash, download and replace. HTTP API and client semantics:
  [`docs/live-app.md`](docs/live-app.md).
- **Assets (binaries beside the documents):** [`docs/assets.md`](docs/assets.md) — images, PDFs and
  other binaries kept in an asset store (a folder, a NAS, an rclone remote, Google Drive) with a
  small `NAME.tdbasset` pointer in the store beside the documents that link to them: push, pull,
  status, verify, relocate, what a sync does with a binary dropped into a vault, and what a file
  moved, replaced or trashed in the store makes of the asset.
- **Who may do what:** [`docs/permissions.md`](docs/permissions.md) — what textdb enforces today
  (nothing: no principals, roles or ACLs, and `--author` is self-asserted), what the asset stores
  add to the question (store rows nobody owns, per-person provider credentials, what a pointer
  tells its readers, and a denial that looks like a failure), and the options for each.
- **Python library:** [`python/`](python/README.md) — `Corpus.open("sqlite:///kb.db" | "postgresql://…")`,
  file/folder loaders, anchored edits, conflict handling, CLI.
- **Node SDK:** [`node/packages/textdb`](node/packages/textdb) — `connect({ store })` over SQLite or
  Postgres behind one async interface, with a bearer per corpus; `Access` and `AssetStores` for the
  configuration commands, which run the CLI so the store decides who may.
- **Skills for AI agents:** [`skills/textdb-install`](skills/textdb-install/SKILL.md),
  [`skills/textdb-use-postgres`](skills/textdb-use-postgres/SKILL.md),
  [`skills/textdb-use-sqlite`](skills/textdb-use-sqlite/SKILL.md),
  [`skills/textdb-cli`](skills/textdb-cli/SKILL.md) — copy a folder into `.claude/skills/`
  (project) or `~/.claude/skills/` (user) to make it available as a slash command.

## Layout

| Path | What |
|---|---|
| `crates/textdb-core` | Engine-agnostic algorithms: FastCDC chunker with newline snap, BLAKE3 prolly tree, `Storage` trait, materialize/locate, localised edit, tree diff, diff3, commit-with-rebase, and the access model (shares, path translation, link projection) both bindings share |
| `crates/textdb-md` | Markdown `StructureExtractor` (sections, wikilinks, frontmatter) |
| `crates/textdb-sqlite` | SQLite binding: shadow tables, `CREATE VIRTUAL TABLE kb USING textdb(...)`, table-valued and scalar functions, FTS5 on chunks |
| `crates/textdb-sqlite-ext` | Loadable SQLite extension (`libtextdb_sqlite_ext.so`) for Python, the `sqlite3` shell, any language |
| `crates/textdb-pg` | Postgres 16 extension (pgrx): schema `kb`, updatable views `kb.file`/`kb.folder`/`kb.file_version`, functions, SQLSTATEs `TX001`/`TX002`/`TX005`, accounts and grants with `kb.auth` |
| `crates/textdb-cli` | The `textdb` command line, over SQLite (compiled in) or Postgres |
| `node/` | Live corpus app: `packages/textdb` (client over `node:sqlite`), `apps/server` (HTTP + server-sent events), `apps/web` (React + CodeMirror UI) |
| `python/` | Python library `textdb` with swappable Postgres/SQLite backends, loaders, CLI |
| `bench/harness` | The test suite runner and six backends (`fs`, `fs-git`, `sql-text-sqlite`, `sql-text-pg`, `textdb-sqlite`, `textdb-pg`) |
| `bench/harness/tests/*.toml` | The test matrix as data |
| `docs/decisions` | ADRs recorded during the POC |
| `skills/` | Agent skills: installing and using textdb |

## Build and test

```sh
cargo test --workspace --release          # core property tests P1–P6, md, sqlite SQL-surface tests
```

Postgres extension (needs `postgresql-server-dev-16`, `libclang`, `cargo-pgrx 0.18`):

```sh
cargo install cargo-pgrx --version 0.18.1 --locked
cargo pgrx init --pg16 $(which pg_config)
cd crates/textdb-pg && cargo pgrx install --release --pg-config $(which pg_config)
psql -c 'CREATE EXTENSION textdb_pg'
```

## Using it

SQLite:

```sql
CREATE VIRTUAL TABLE kb USING textdb(store='kb_');
INSERT INTO kb(path, content, author) VALUES ('/notes/a.md', '# A' || char(10) || 'alpha', 'alice');
UPDATE kb SET content = replace(content, 'alpha', 'ALPHA') WHERE path = '/notes/a.md';   -- diff → edit → commit
UPDATE kb SET content = ?, base_version = 1 WHERE path = '/notes/a.md';                  -- rebase against a stale read
SELECT textdb_edit('/notes/a.md', 'ALPHA', 'beta');                                       -- strict replace
SELECT * FROM textdb_search('beta', '/notes');
SELECT * FROM textdb_history('/notes/a.md');
SELECT textdb_content('/notes/a.md', 1), textdb_diff('/notes/a.md', 1, 2);
SELECT name, kind, nbytes, nwords, versions, authors FROM textdb_ls('/notes');            -- words, authors; folders total their subtree
SELECT path, nwords FROM textdb_ls('/', 1) WHERE kind = 'file' ORDER BY nwords DESC LIMIT 10;  -- recursive
SELECT textdb_move('/notes', '/archive/notes', 'alice');                                  -- move a folder, recorded in path history
SELECT * FROM textdb_path_history('/archive/notes/a.md');                                 -- renames, moves, deletes
SELECT textdb_delete('/archive', 'alice');                                                -- to the trash; textdb_trash(), textdb_purge(id)
```

Postgres:

```sql
INSERT INTO kb.file(path, content) VALUES ('/notes/a.md', E'# A\nalpha\n');
UPDATE kb.file SET content = replace(content, 'alpha', 'ALPHA') WHERE path = '/notes/a.md';
SELECT kb.edit(f, 'ALPHA', 'beta') FROM kb.file f WHERE f.path = '/notes/a.md';
SELECT * FROM kb.search('beta', '/notes');
SELECT version, author FROM kb.history('/notes/a.md');
SELECT content FROM kb.file_version WHERE path = '/notes/a.md' AND version = 1;
SELECT name, kind, nbytes, nwords, versions, authors FROM kb.ls('/notes');   -- folders total their subtree
SELECT kb.move('/notes', '/archive/notes', 'alice');
SELECT * FROM kb.path_history('/archive/notes/a.md');
```

A conflicting concurrent edit raises SQLSTATE `TX001` whose `DETAIL` is a JSON payload
with `base`, `theirs` (current text) and `ours` for the overlapping lines; `TX002` is
retry-budget exhaustion.

## Running the comparative suite

```sh
bench/scripts/pg-start.sh                       # throwaway PG16 cluster, prints the URL
cargo build --release -p textdb-bench
./target/release/textdb-bench run --size s --pg postgres://postgres@localhost:54329/postgres \
    --out bench/out --work bench/data [--backends fs,textdb-sqlite] [--filter CW,ME-06] [--mode durable]
./target/release/textdb-bench report --out bench/out     # regenerate report.md from results.jsonl
```

`--size xs|s|m|l` scales how much work the matrix does without changing what it tests:
`s` fits a full matrix on a laptop, `m` is the default. The full-copy-history baselines
dominate the disk footprint, so size is the knob that decides whether a run costs
megabytes or gigabytes.

`--profile spec` replays the matrix at the scale issue #1 asks for (50k files, 1 GiB files,
minutes per N); `poc` is the scaled-down profile used for `bench/RESULTS.md`.

Every cell is bracketed by untimed accuracy checks — the store starts empty, a canary
survives a create/read/delete round-trip, and afterwards every document is compared
against a reference model. A cell that fails one publishes no timings at all, so a broken
backend cannot post a fast number. See [`bench/README.md`](bench/README.md) for the knobs,
the ten test families and what each measures.

Front-matter property search is documented in [`docs/properties.md`](docs/properties.md), and
markdown heading outlines in [`docs/outlines.md`](docs/outlines.md). What every listing and
search surface returns, key for key, is [`docs/shapes.md`](docs/shapes.md).
