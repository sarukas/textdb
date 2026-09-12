# textdb

Versioned, chunk-shared text documents inside an SQL database, with compare-and-swap
commits and automatic rebase — so many humans and AI agents can read, search and edit a
markdown knowledge base concurrently without git push/pull.

This repository is the proof of concept described in [`docs/spec.md`](docs/spec.md)
(issue #2) together with the comparative test suite from [`docs/test-suite.md`](docs/test-suite.md)
(issue #1).

- **Install:** [`docs/INSTALL.md`](docs/INSTALL.md) · **Use:** [`docs/USAGE.md`](docs/USAGE.md)
- **Benchmark results:** [`bench/RESULTS.md`](bench/RESULTS.md) (raw data in `bench/results/`)
- **Python library:** [`python/`](python/README.md) — `Corpus.open("sqlite:///kb.db" | "postgresql://…")`,
  file/folder loaders, anchored edits, conflict handling, CLI.
- **Skills for AI agents:** [`skills/textdb-install`](skills/textdb-install/SKILL.md),
  [`skills/textdb-use-postgres`](skills/textdb-use-postgres/SKILL.md),
  [`skills/textdb-use-sqlite`](skills/textdb-use-sqlite/SKILL.md) — copy a folder into `.claude/skills/`
  (project) or `~/.claude/skills/` (user) to make it available as a slash command.

## Layout

| Path | What |
|---|---|
| `crates/textdb-core` | Engine-agnostic algorithms: FastCDC chunker with newline snap, BLAKE3 prolly tree, `Storage` trait, materialize/locate, localised edit, tree diff, diff3, commit-with-rebase |
| `crates/textdb-md` | Markdown `StructureExtractor` (sections, wikilinks, frontmatter) |
| `crates/textdb-sqlite` | SQLite binding: shadow tables, `CREATE VIRTUAL TABLE kb USING textdb(...)`, table-valued and scalar functions, FTS5 on chunks |
| `crates/textdb-sqlite-ext` | Loadable SQLite extension (`libtextdb_sqlite_ext.so`) for Python, the `sqlite3` shell, any language |
| `crates/textdb-pg` | Postgres 16 extension (pgrx): schema `kb`, updatable views `kb.file`/`kb.folder`/`kb.file_version`, functions, SQLSTATEs `TX001`/`TX002` |
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
UPDATE kb SET path = '/archive/notes' WHERE path = '/notes';                              -- move a folder
DELETE FROM kb WHERE path = '/archive';                                                   -- tombstone
```

Postgres:

```sql
INSERT INTO kb.file(path, content) VALUES ('/notes/a.md', E'# A\nalpha\n');
UPDATE kb.file SET content = replace(content, 'alpha', 'ALPHA') WHERE path = '/notes/a.md';
SELECT kb.edit(f, 'ALPHA', 'beta') FROM kb.file f WHERE f.path = '/notes/a.md';
SELECT * FROM kb.search('beta', '/notes');
SELECT version, author FROM kb.history('/notes/a.md');
SELECT content FROM kb.file_version WHERE path = '/notes/a.md' AND version = 1;
```

A conflicting concurrent edit raises SQLSTATE `TX001` whose `DETAIL` is a JSON payload
with `base`, `theirs` (current text) and `ours` for the overlapping lines; `TX002` is
retry-budget exhaustion.

## Running the comparative suite

```sh
bench/scripts/pg-start.sh                       # throwaway PG16 cluster, prints the URL
cargo build --release -p textdb-bench
./target/release/textdb-bench run --profile poc --pg postgres://postgres@localhost:54329/postgres \
    --out bench/out --work bench/data [--backends fs,textdb-sqlite] [--filter CW,ME-06] [--mode durable]
./target/release/textdb-bench report --out bench/out     # regenerate report.md from results.jsonl
```

`--profile spec` replays the matrix at the scale issue #1 asks for (50k files, 1 GiB files,
minutes per N); `poc` is the scaled-down profile used for `bench/RESULTS.md`.
