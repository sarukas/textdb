# ADR 0003 — Transaction handling in the SQLite binding

Status: accepted (Stage 1c)

## Context

Scalar functions such as `textdb_edit()` run inside a `SELECT`. In autocommit mode every
nested write statement issued from the function commits on its own, which cost ~1.2 MB of
WAL writes per small edit in the first benchmark run (vs ~100 KB when batched).

## Decision

`TextDb::tx` opens `BEGIN IMMEDIATE … COMMIT` when the connection is in autocommit mode
(SQLite allows this from inside a scalar function while only read statements are active),
uses a `SAVEPOINT` inside an explicit transaction, and runs inline inside virtual-table
`xUpdate` callbacks, where the enclosing INSERT/UPDATE/DELETE statement already provides the
transaction and SQLite forbids savepoints while a write statement is in progress.

## Consequences

- One transaction per logical operation on every path.
- `append`/`edit` through SQL functions cost the same as through the virtual table.
