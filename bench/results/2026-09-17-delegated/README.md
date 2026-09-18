# The matrix as a delegated account

The first run of the whole suite where a backend holds a bearer token. Everything before this
measured the **owner's** path only, which is what `#14` item 2 asked to change.

| | |
|---|---|
| Commit | `a98e4ec` |
| Profile | `poc`, size `s` (×0.3), mode `fast`, seed `20260912` |
| Backends | `fs`, `sql-text-sqlite`, `textdb-sqlite`(+`@account`), `textdb-pg`(+`@account`) |
| Postgres | PostgreSQL 16.15 on port 54329 (`bench/scripts/pg-start.sh`) |
| Tests | 48, 23,475 result rows |
| Failures | `CW-06` on both `textdb-pg` columns; `MD-04`/`MD-05` on `textdb-sqlite` |

Reproduce with:

```sh
bench/scripts/pg-start.sh
./target/release/textdb-bench run --size s --as-account \
  --backends fs,sql-text-sqlite,textdb-sqlite,textdb-pg \
  --pg postgres://postgres@localhost:54329/postgres --out bench/out
```

See [`COMPARISON.md`](COMPARISON.md) for what the account costs and what is left.

## What `@account` means here

`textdb-sqlite@account` and `textdb-pg@account` are the same engines, opened by an account whose
**root is one folder**. The path the suite writes is the path the account writes; the store holds
it one level deeper and every operation crosses the view on the way. No path is rewritten in the
harness, so **every oracle applies unchanged** — a twin that stopped filtering fails the same
checks as any other backend. `bench/harness/src/backends/delegate.rs` has the reasoning.

## The failures, and whose they are

**`CW-06` on both Postgres columns** is the flaky concurrency check the previous run already
retired as not textdb's: `version_count_matches_commits` also failed on `sql-text-sqlite`, a
reference backend this feature cannot reach. It fails for the owner and the account alike, which
is the point — it is not about delegation.

**`MD-04` and `MD-05` on `textdb-sqlite`** fail for the **owner** and not the account, and they
fail at size `s` and `xs` but not at `m`: the oracles expect a corpus these sizes do not produce.
They are recorded rather than explained away, but they are not this feature's. The account is
silent on them because `frontmatter` and `sections` are N/A for the twin — see below.

**No check fails for an account that does not also fail for its owner.** One did in the previous
run (`MD-08` on `textdb-sqlite@account`) and it was a real defect, not a flake; `94fbeed` is the
fix and this run is how it was confirmed.

## What this run still cannot measure

- **The structure sidecar, for an account.** `links`, `backlinks`, `frontmatter` and `sections`
  are read by both backends from the sidecar *tables*, which speak store paths and carry link
  targets as recorded rather than as an account is shown them. The twin records N/A with that
  reason rather than translating around it and publishing a number for a query no account can
  run. That neither engine has a view-aware SQL surface for the sidecar is itself the result.
- **The aliased namespace.** The twin is single-root; the other shape differs by one `format!`
  inside `to_store` and everything expensive runs the same either way.
- **Row-level security.** `kb.node`'s policy is deliberately not `FORCE`d and the harness
  connects as the role that owns the extension, so a delegated Postgres run measures the `kb.*`
  surface and not the layer beneath it. Unchanged from `#14` item 5.
