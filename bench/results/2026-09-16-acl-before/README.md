# Baseline before folder-scoped delegated access (#12)

The before-side of the comparison for issue #12. Run at `cc341d2`, on an idle host: nothing was
compiled or tested while it ran, because this host shows outliers past 80× on sub-millisecond
concurrent work and a `cargo build` on four cores lands squarely inside that.

| | |
|---|---|
| Commit | `cc341d2` (the #12 acceptance suite, all red, feature-gated) |
| Profile | `poc`, size `m` (×1), mode `fast`, seed `20260912` |
| Backends | `fs`, `sql-text-sqlite`, `textdb-sqlite`, `textdb-pg` |
| Postgres | PostgreSQL 16.15 on port 54329 (`bench/scripts/pg-start.sh`) |
| Tests | 48, 15,867 result rows |
| Wall clock | ~48 min |

## What this baseline can and cannot prove

It measures the **owner's path only**. No backend in `bench/harness/` presents a bearer token, so
nothing here exercises an authenticated session. What it is therefore good for is the thing most
likely to go wrong quietly: that adding the access model costs nothing to callers who do not use
it. Two decisions in the implementation exist for exactly that, and this is what checks them:

- Postgres asks for the connection's account as `(SELECT kb.current_account())` rather than
  `kb.current_account()`, so it becomes an InitPlan evaluated once per statement instead of a
  STABLE call per row in every listing.
- SQLite guards its per-connection session map with a relaxed atomic that is never set until
  something authenticates, so `textdb_content` on a store that delegates nothing takes no lock.

The **token path is unmeasured** and stays that way until the harness grows a backend that holds
a bearer. Until then, no claim about the cost of a filtered read is supported by this directory.

## Reading the after-side

`fs` and `sql-text-sqlite` are untouched by anything in #12, so whatever they move between the
two runs is the host rather than the code — that difference is the noise floor, and a textdb
median inside it is noise. `bench/results/2026-09-16-shape-after/COMPARISON.md` is the worked
example of that method.
