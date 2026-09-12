# ADR 0006 — Postgres: buffer chunk/node inserts and flush in hash order before the CAS

Status: accepted (Stage 3a)

## Context

In the first concurrency runs against `textdb-pg`, hot-file tests (ME-06, CW-03) reported
`error` outcomes. Recording the error text showed they were conflicts whose custom SQLSTATE
had been lost (pgrx re-raises an error caught inside SPI as `XX000`; fixed by raising from a
PL/pgSQL wrapper, `kb._check`). Reviewing the write path for that bug exposed a real, if not
yet observed, hazard: with identical concurrent edits (two agents both turning `count: 5`
into `count: 6`) both transactions produce the same new chunk. `INSERT … ON CONFLICT DO
NOTHING` on a unique index must wait for the other, uncommitted, insert of the same key, so
two transactions inserting the same pair of chunks in opposite orders would wait on each
other and Postgres would abort one with a deadlock error.

## Decision

`SpiStorage` buffers `put_chunk`/`put_node` in `BTreeMap`s keyed by hash and flushes them in
ascending hash order immediately before the `cas_root` UPDATE (and on drop). Reads consult
the buffer first, so the algorithms are unchanged. Lock acquisition is therefore totally
ordered across transactions and cannot cycle; the only remaining wait is on the file's
`node` row, which is acquired last and released at commit.

## Consequences

- The deadlock scenario cannot occur; a lost CAS race simply retries with a rebase.
- Fewer SPI round trips (one insert per distinct hash instead of per algorithm step).
- The SQLite binding does not need this: it is single-writer.
