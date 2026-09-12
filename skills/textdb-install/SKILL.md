---
name: textdb-install
description: Install the textdb Postgres extension (textdb_pg) or the textdb SQLite crate on a machine, verify the installation, and troubleshoot pgrx/libclang/header problems. Use when asked to install, build, upgrade or set up textdb.
---

# Installing textdb

You are installing **textdb**: a Postgres 16 extension (`textdb_pg`, schema `kb`) and/or the
Rust crate `textdb-sqlite`. Source: https://github.com/sarukas/textdb. Follow the steps in
order and verify each one; do not skip the verification queries.

## 1. Decide the target

- Postgres available and the user wants SQL access for many agents → **extension** (steps 2–5).
- Embedded / single process / no Postgres → **SQLite crate** (step 6).

## 2. Check prerequisites (extension)

Run and read the output of each:

```sh
rustc --version                 # need stable ≥ 1.85; install via https://rustup.rs if missing
pg_config --version             # need PostgreSQL 16.x
ls "$(pg_config --includedir-server)/postgres.h"   # server headers; if missing install postgresql-server-dev-16
ls /usr/lib/llvm-*/lib/libclang*.so* 2>/dev/null | head -1   # libclang for bindgen; if missing install libclang-dev
cargo pgrx --version 2>/dev/null                              # need exactly 0.18.x
```

Fix what is missing before continuing. On Debian/Ubuntu:
`sudo apt-get install -y build-essential clang libclang-dev postgresql-16 postgresql-server-dev-16`.
Never uninstall or replace an existing Postgres major version; if it is not 16, stop and tell
the user the POC extension targets PG16 (pgrx 0.18 supports 13–17; other majors need a
`pgXX` feature added to `crates/textdb-pg/Cargo.toml` and `cargo pgrx init --pgXX`).

## 3. Build and install the extension

```sh
git clone https://github.com/sarukas/textdb.git && cd textdb
cargo install cargo-pgrx --version 0.18.1 --locked        # 0.19 needs rustc ≥ 1.96; keep 0.18.1
cargo pgrx init --pg16 "$(which pg_config)"
cd crates/textdb-pg
export LIBCLANG_PATH="$(dirname "$(ls /usr/lib/llvm-*/lib/libclang.so* | head -1)")"
cargo pgrx install --release --pg-config "$(which pg_config)"
```

`cargo pgrx install` writes into `$(pg_config --pkglibdir)` and `$(pg_config --sharedir)/extension`;
if it fails with permission denied, re-run with `sudo -E` (keep the environment) or as the
Postgres owner. A full release build takes a few minutes the first time.

If the Postgres server was already running, restart it so new backends load the new library
(`pg_ctl restart` or `systemctl restart postgresql`). Make sure the server environment does
not contain `RUST_BACKTRACE=1` (it would put Rust backtraces into SQL error messages).

## 4. Enable it in a database

```sql
CREATE EXTENSION textdb_pg;
SELECT kb.textdb_version();
```

`CREATE EXTENSION` needs a superuser. To let a normal role use it afterwards:

```sql
GRANT USAGE ON SCHEMA kb TO agent_role;
GRANT SELECT, INSERT, UPDATE, DELETE ON kb.file, kb.folder TO agent_role;
GRANT SELECT ON kb.file_version TO agent_role;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA kb TO agent_role;
```

## 5. Verify

```sql
INSERT INTO kb.file(path, content) VALUES ('/_install_check.md', E'# check\nline\n');
SELECT kb.edit('/_install_check.md', 'line', 'edited');          -- returns 2
SELECT version, content FROM kb.file WHERE path = '/_install_check.md';   -- version 2
SELECT * FROM kb.search('edited', '/');                         -- one hit, line 2
SELECT kb.diff('/_install_check.md', 1, 2);
DELETE FROM kb.file WHERE path = '/_install_check.md';
```

All five must succeed. Report the versions of Postgres, rustc and cargo-pgrx you used.

## 6. SQLite crate instead

Add to the project's `Cargo.toml`:

```toml
textdb-sqlite = { git = "https://github.com/sarukas/textdb", package = "textdb-sqlite" }
rusqlite = { version = "0.40", features = ["bundled"] }
```

and open a store with `textdb_sqlite::open("kb.db")` followed by
`CREATE VIRTUAL TABLE IF NOT EXISTS kb USING textdb(store='kb_')`. Verify with
`INSERT INTO kb(path, content) …` and `SELECT textdb_content('/…')`. No system SQLite and no
Postgres are needed. There is no standalone `.so` for the `sqlite3` shell.

## Troubleshooting map

| Error text | Fix |
|---|---|
| `bindgen failed for pg16` / `No such file or directory` | server headers missing → install `postgresql-server-dev-16`, check `pg_config --includedir-server` |
| `Unable to find libclang` | set `LIBCLANG_PATH` to the directory containing `libclang.so` |
| `requires rustc 1.96 or newer` (cargo-pgrx) | use `--version 0.18.1` |
| `could not open extension control file` | installed against a different `pg_config` than the running server; pass `--pg-config` of the running server |
| `extension "textdb_pg" already exists` when upgrading | `DROP EXTENSION textdb_pg CASCADE` first — this deletes the `kb` data; export with `kb.export('/')` beforehand |

Do not modify tables in schema `kb` directly; everything goes through the views and functions.
