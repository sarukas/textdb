# Installing textdb

textdb ships as four Rust crates and one Postgres extension. Pick what you need:

| You want | Install |
|---|---|
| Versioned documents inside **PostgreSQL 16** with an SQL surface (`kb.file`, `kb.search`, …) | [Postgres extension](#postgres-extension-textdb_pg) |
| The same inside an **embedded SQLite** database from a Rust program | [SQLite crate](#sqlite-crate-textdb-sqlite) |
| Only the algorithms (chunker, prolly tree, edit, rebase, diff) | [Core crate](#core-crate-textdb-core) |
| To use it **from Python** (either backend) | [Python library](#python-library) |
| To load the SQLite module in the `sqlite3` shell or any language | [Loadable SQLite extension](#loadable-sqlite-extension) |
| To reproduce the benchmarks | [Benchmark harness](#benchmark-harness) |

## Prerequisites

| Component | Requirement | Check |
|---|---|---|
| Rust toolchain | stable, 1.85 or newer (1.94 was used for the POC) | `rustc --version` |
| C compiler | gcc or clang (bundled SQLite) | `cc --version` |
| Postgres extension only | PostgreSQL 16 server **and** server headers (`postgresql-server-dev-16` on Debian/Ubuntu, `postgresql16-devel` on RHEL-likes) | `pg_config --includedir-server` must list `postgres.h` |
| Postgres extension only | libclang (bindgen) — `libclang-dev` / `clang-devel` | `ls /usr/lib/llvm-*/lib/libclang*.so*` |
| Postgres extension only | `cargo-pgrx` **0.18.1** (0.19 needs rustc ≥ 1.96) | `cargo pgrx --version` |
| Benchmarks | `git`, `ripgrep` (`rg`), a Postgres cluster you may create | `rg --version` |

Ubuntu 24.04 one-liner for everything:

```sh
sudo apt-get update && sudo apt-get install -y build-essential clang libclang-dev git ripgrep \
     postgresql-16 postgresql-server-dev-16
curl https://sh.rustup.rs -sSf | sh -s -- -y && source ~/.cargo/env
```

## Get the source

```sh
git clone https://github.com/sarukas/textdb.git
cd textdb
cargo test --workspace --release      # core property tests P1–P6, markdown extractor, SQLite SQL surface
```

## Postgres extension (`textdb_pg`)

```sh
cargo install cargo-pgrx --version 0.18.1 --locked
cargo pgrx init --pg16 "$(which pg_config)"            # registers your system Postgres with pgrx
cd crates/textdb-pg
export LIBCLANG_PATH=/usr/lib/llvm-18/lib               # only if bindgen cannot find libclang
cargo pgrx install --release --pg-config "$(which pg_config)"
```

`cargo pgrx install` copies `textdb_pg.so` into `$(pg_config --pkglibdir)` and the control
and SQL files into `$(pg_config --sharedir)/extension`. This needs write access to those
directories (run as root or the Postgres owner). Then, in every database that should have it:

```sql
CREATE EXTENSION textdb_pg;          -- creates schema kb with tables, views, functions
SELECT kb.textdb_version();
```

Superuser is required for `CREATE EXTENSION` (the control file says `superuser = true`
because the extension is a compiled library). After that, ordinary roles can be granted
`USAGE` on schema `kb` and privileges on `kb.file`, `kb.folder`, `kb.file_version` and the
functions — the tables themselves never need to be touched directly.

Upgrading after a rebuild: `cargo pgrx install --release …` again, then in each database
`DROP EXTENSION textdb_pg CASCADE; CREATE EXTENSION textdb_pg;` — **this drops the data**.
Version-to-version upgrade scripts are out of the POC's scope; export with `kb.export('/')`
first if the content matters.

Verify:

```sql
INSERT INTO kb.file(path, content) VALUES ('/hello.md', E'# Hello\nworld\n');
SELECT path, version, nbytes FROM kb.file;
SELECT kb.edit('/hello.md', 'world', 'textdb');
SELECT * FROM kb.history('/hello.md');
```

### Configuration notes

- The extension needs no `shared_preload_libraries` entry and no GUCs.
- `READ COMMITTED` is sufficient; correctness rests on the compare-and-swap of the file root.
- For write-heavy hot files raise `max_connections` for your agent fleet; each concurrent
  writer holds one connection while its transaction runs (commits are short: a few ms for
  100 KB documents).
- Full-text search uses the `simple` dictionary on a generated `tsvector` column with a GIN
  index; edits only add rows (never update the index), so `autovacuum` has little to do on
  `kb.chunk`. Consider `ALTER INDEX kb.chunk_tsv SET (fastupdate = off)` if search latency
  spikes under insert load.

## SQLite crate (`textdb-sqlite`)

textdb for SQLite is a Rust crate that registers a virtual-table module and SQL functions on
a `rusqlite` connection (SQLite is bundled; no system SQLite needed):

```toml
[dependencies]
textdb-sqlite = { git = "https://github.com/sarukas/textdb", package = "textdb-sqlite" }
rusqlite = { version = "0.40", features = ["bundled"] }
```

```rust
let conn = textdb_sqlite::open("kb.db")?;                // WAL, busy_timeout, module registered
conn.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS kb USING textdb(store='kb_');")?;
conn.execute("INSERT INTO kb(path, content) VALUES (?1, ?2)", ("/hello.md", "# Hello\n"))?;
```

or, on a connection you already own, `textdb_sqlite::register(&conn, "kb_")?`. There is also a
Rust API (`textdb_sqlite::TextDb`) with the same operations for callers that do not want SQL,
and a small CLI, `textdb-corpus`, that imports a directory tree and exports it back:

```sh
cargo build --release -p textdb-bench                     # builds target/release/textdb-corpus too
./target/release/textdb-corpus import ./my-notes kb.db
./target/release/textdb-corpus export kb.db ./roundtrip && diff -r ./my-notes ./roundtrip
```

## Loadable SQLite extension

For Python, the `sqlite3` shell, or any non-Rust language:

```sh
cd crates/textdb-sqlite-ext && cargo build --release        # → target/release/libtextdb_sqlite_ext.so
sqlite3 kb.db ".load $PWD/target/release/libtextdb_sqlite_ext" \
        "CREATE VIRTUAL TABLE IF NOT EXISTS kb USING textdb(store='kb_');" \
        "INSERT INTO kb(path, content) VALUES ('/hello.md', 'hi');" "SELECT * FROM textdb_history('/hello.md');"
```

The crate is outside the main workspace because rusqlite's `loadable_extension` feature and
the `bundled` SQLite cannot be built together. Entry points: `sqlite3_textdbsqliteext_init`
(auto-derived from the file name) and `sqlite3_extension_init`. Install by copying the `.so`
anywhere and referencing it (Python: `TEXTDB_SQLITE_EXT`).

## Python library

```sh
pip install -e python/                # SQLite only (stdlib sqlite3 + the loadable extension above)
pip install -e 'python/[postgres]'    # + psycopg2-binary for postgresql:// URLs
python -m textdb sqlite:///kb.db load ./notes /notes
```

See [`python/README.md`](../python/README.md) and the examples in `python/examples/`.

## Core crate (`textdb-core`)

```toml
[dependencies]
textdb-core = { git = "https://github.com/sarukas/textdb", package = "textdb-core" }
```

Implement `textdb_core::Storage` for your store and use `build`, `materialize`, `apply_edits`,
`commit`, `changed_runs`, `unified_diff`. `MemStorage` is the in-memory reference.

## Benchmark harness

```sh
bench/scripts/pg-start.sh                                  # throwaway PG16 cluster on port 54329 (runs as user pgbench when root)
cargo build --release -p textdb-bench
./target/release/textdb-bench run --profile poc --pg postgres://postgres@localhost:54329/postgres \
     --out bench/out --work bench/data
python3 bench/scripts/verdict.py bench/out/results.jsonl
```

See [`bench/README.md`](../bench/README.md) and [`bench/RESULTS.md`](../bench/RESULTS.md).

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `bindgen failed for pg16 … No such file or directory` | Server headers missing: install `postgresql-server-dev-16`; check `pg_config --includedir-server` |
| `Unable to find libclang` | `export LIBCLANG_PATH=/usr/lib/llvm-18/lib` (adjust version) |
| `cargo-pgrx … requires rustc 1.96` | Pin `--version 0.18.1` |
| `CREATE EXTENSION` says file not found | `cargo pgrx install` targeted a different `pg_config` than the running server; pass `--pg-config` explicitly |
| Rust backtraces in Postgres error output | The server inherited `RUST_BACKTRACE=1`; restart it without that variable |
| SQLite: `database is locked` | Every connection needs `PRAGMA busy_timeout`; `textdb_sqlite::open` sets 30 s |
