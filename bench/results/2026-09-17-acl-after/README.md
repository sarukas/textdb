# After folder-scoped delegated access (#12)

The after-side of the comparison begun in `2026-09-16-acl-before/`. Same host, same profile, same
seed, run on an idle host — nothing was compiled or tested while it ran.

| | |
|---|---|
| Commit | `38d9134` (the #12 acceptance suite, 87 of 87 green on both engines) |
| Profile | `poc`, size `m` (×1), mode `fast`, seed `20260912` |
| Backends | `fs`, `sql-text-sqlite`, `textdb-sqlite`, `textdb-pg` |
| Postgres | PostgreSQL 16.15 on port 54329 (`bench/scripts/pg-start.sh`) |
| Tests | 48, 15,858 result rows |
| Failures | `CW-06` on `textdb-pg` **and** on `sql-text-sqlite` |

See [`COMPARISON.md`](COMPARISON.md) for what moved and why.

## What this run can and cannot prove

The same limit as the baseline, and it has not changed: **it measures the owner's path only.** No
backend in `bench/harness/` presents a bearer token, so nothing here exercises an authenticated
session. That is still the right thing to measure, because it is the claim most likely to go wrong
quietly — *adding the access model costs nothing to callers who do not use it* — and this run shows
it went wrong in three places and was fixed in two.

The **token path remains unmeasured**. Nothing in this directory supports a claim about the cost of
a filtered read, a translated listing or a projected document. A harness backend that holds a
bearer is the only thing that would change that, and it does not exist yet.

## The CW-06 failure is not textdb's

`CW-06`'s `version_count_matches_commits` check failed on `textdb-pg` here, as it did in the
baseline. It also failed on **`sql-text-sqlite`**, a reference backend that nothing in this feature
touches and that has no version machinery of its own beyond what the test drives. A check that
fails on a backend the change cannot reach is a check that is flaky under concurrency, not a defect
it found — which retires the open question the baseline left about that cell.
