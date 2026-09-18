# Working in this repository

Start with [`README.md`](README.md); [`docs/`](docs/) is thorough and current, and
[`docs/shapes.md`](docs/shapes.md) is the contract every listing, hit, link and history row
answers to on all six surfaces. This file is only for what those do not tell you: the traps that
cost an afternoon if you meet them cold.

## Running the tests

```sh
cargo build --release -p textdb-cli
cd crates/textdb-sqlite-ext && cargo build --release && cd ../..   # its own crate, its own target dir
export TEXTDB_SQLITE_EXT="$PWD/crates/textdb-sqlite-ext/target/release/libtextdb_sqlite_ext.so"

cargo test --release --workspace                      # Rust, SQLite only
TEXTDB_TEST_PG=postgres://postgres@localhost:54329/postgres \
  cargo test --release -p textdb-cli --test cli_pg    # skips itself when the variable is unset
cd python && python3 -m pytest -q                     # both backends when TEXTDB_TEST_PG is set
cd node/packages/textdb && npm test                   # and node/apps/server, node/apps/web
cargo test --release -p textdb-cli --test cli -- --ignored   # the two real-process sync races
```

The Postgres tests need a server: `bench/scripts/pg-start.sh` brings up a throwaway PG16 cluster
on port 54329 (as user `pgbench` when you are root — `pg_ctl` refuses to run as root). It is worth
knowing the script is idempotent, because that cluster dies under load and the symptom downstream
is a wall of `ConnectionRefused` that looks like a code failure.

## Traps

**Installing the Postgres extension does not update databases that already have it.** `cargo pgrx
install` replaces the `.so` and the SQL file, but a database that already ran `CREATE EXTENSION`
keeps the old function signatures. The new library then answers the old signature: at best
`function kb.x(...) does not exist`, at worst a backend segfault that takes the whole cluster down
with it and leaves you reading a crash trace. After every install:

```sh
psql "$URL" -c 'DROP EXTENSION textdb_pg CASCADE; CREATE EXTENSION textdb_pg;'
```

`cli_pg` creates a fresh database per test and is immune. The Python suite connects to whichever
database `TEXTDB_TEST_PG` names and is not. [`docs/USAGE.md`](docs/USAGE.md) says the same thing to
users as a release note; the part worth remembering here is that it applies after *every* install
during a dev loop, not only across releases.

**A new SQLite column goes in `create_sql` *and* `ADDED_COLUMNS`.** Putting it only in the
migration list means it is missing from every *fresh* store too, so every store takes the
migration path — which opens a savepoint, and a savepoint cannot be opened while `CREATE VIRTUAL
TABLE` is running. `textdb init` then fails on a brand-new store with `cannot open savepoint - SQL
statements in progress`, which reads like anything but a schema mistake.
`crates/textdb-sqlite/tests/schema_invariants.rs` asserts a new store needs no migration.

**pgrx reads `#[pg_extern]` signatures literally.** A return tuple cannot hide behind a type alias;
the macro rejects it. Spell the tuple out in each function and keep the alias for the helper they
share.

**Postgres infers a bare parameter's type from its context.** `VALUES (..., $10 + 1)` resolves
`$10` as `integer`, and rust-postgres then rejects the `i64` with `error serializing parameter 9`.
Cast at the use site: `$10::bigint + 1`.

A single cast is not enough when the parameter is *not* the target type. `$3::timestamptz`
declares `$3` a `timestamptz`, so sending the store's timestamp — which is text — fails the same
way. Two casts pin it: `$3::text::timestamptz` says "this parameter is text, now make it a
timestamp". The error message names a zero-based index, so `parameter 2` is `$3`.

**An aggregate with no `GROUP BY` returns one row over an empty set.** `SELECT sum(x) … WHERE
false` is one row of NULLs, not none. A view built that way — `kb.root_entry`, which adds an
account's shares up — therefore returned a phantom row to the owner its `WHERE` was meant to
exclude, and `stat /` came back with two rows and `query returned an unexpected number of rows`.
`HAVING count(*) > 0` filters the empty group.

**`now()` is transaction time.** Every row a commit writes shares a timestamp, so "the newest row"
is a tie and `ORDER BY ts DESC` picks arbitrarily among them. Where that matters — the folder
journal's author — filter to the rows that carry a value rather than trusting the order.

**A new table goes in `drop_sql` as well.** `DROP TABLE kb` removes the shadow tables by the
list in `schema.rs::drop_sql`; a table added to `create_sql` and forgotten there survives the
drop. Children first — `grant` and `token` reference `account` and `node`.
`sql_surface.rs::dropping_a_kb_table_removes_the_shadow_tables` counts what is left rather than
naming what it expected, so it catches the next one too.

**`kb.entry` speaks the caller's paths; `kb.node` speaks the store's.** Since #12 the view
translates, so its `path` and `dir` are the account's own. Anything that looks a path up must
pick the right one and not mix them: `kb.ls` compared `e.dir` against a *node's* `path` and
silently returned nothing once an account saw the store under an alias, and `PgStore::read`
reached past the extension to `kb.node` with a caller's path and found nothing. The rule for the
CLI's pg module is the one its own header states — every operation is a call into `kb.*`, and
reaching around it to a table is how a leak or an empty answer gets in.

**A path is translated exactly once, at a public entry point.** With delegated access (#12) a
caller's path is not a store path, and `TextDb`'s public methods turn one into the other on the
way in. Several of them used to call each other — `upsert` calls `create`, `write` calls
`update_content`, `create` calls `ensure_folder` — which translated a second time, and an
account's own `/legal/contracts/x.md` fed back in names an alias it does not have. Hence the
`*_at` variants (`create_at`, `update_content_at`, `commit_edits_at`, `ensure_folder_at`): they
take a path that is already a store path and never translate. Public methods translate, `*_at`
never does, and an internal caller uses `*_at`. The same rule holds on the way out — a row's
`path` is translated once, where the row is built.

**Root discovery walks up from the working directory.** A synced directory anywhere above a test
pairs it with a store the test knows nothing about, and a test that syncs the current directory
syncs *this repository*. The harnesses pin `TEXTDB_CEILING_DIRECTORIES` to the working directory
for that reason; keep it on anything new that runs the CLI.

## Judging a benchmark result

`bench/` has two backends, `fs` and `sql-text-sqlite`, that no change to textdb touches. Whatever
they move between two runs is the host, not the code. On this container that has been a median
around 1.03× with outliers past 80× on sub-millisecond concurrent work, so read a textdb median
inside that band as noise and reach for a per-cell comparison before claiming a win or a
regression. `bench/results/2026-09-16-shape-after/COMPARISON.md` is the worked example.
