# Mid-implementation validation, size `s`, no XL

Not a comparison run. Its job was to answer one question — *has anything broken?* — at a point
where 66 of the 87 #12 acceptance tests pass and the rest is unbuilt. It is **not comparable**
with `2026-09-16-acl-before/`: different size (`s`, 0.3x) and a different set of families, which
means a different load on the machine as well as different numbers.

| | |
|---|---|
| Commit | `866d508` |
| Profile | `poc`, size `s` (0.3x), mode `fast`, seed `20260912` |
| Families | RT, LL, ME, CR, CW, SR, NS, DU, FP, MD, SY — **XL excluded** |
| Backends | `fs`, `sql-text-sqlite`, `textdb-sqlite`, `textdb-pg` |
| Tests | 47, 15,073 rows |
| Failures | 1 |

## The one failure is the one the baseline had

```
FAIL CW-06 textdb-pg [N=20] version_count_matches_commits: 113 versions for 105 commits
```

`bench/results/2026-09-16-acl-before/` has the same cell failing and nothing else. Pre-existing,
and unrelated to delegated access.

## What this run was really checking

The run before it — the full matrix, mid-implementation — **died**, and left two questions:

1. **`textdb-pg` failed `RT-01` and `RT-03` `read`**: create a 1 MiB mixed-unicode document, read
   it back, bytes differ. The baseline had neither. Those families pass here, and passed again in
   a targeted re-run. That run's cluster had printed `could not start server` before it began and
   was dead by the end, and the same run failed `fs` — a backend no change to textdb can touch —
   so the weight of evidence is environmental. Not closed until the final full run repeats clean.
2. **The harness killed the machine.** Ten failures at XL each printed the contested region as a
   decimal byte array: a **1.9 GB** log, the disk from 15 GB free to 6 GB, and Postgres dead under
   the pressure. `metrics.rs::fail` now cuts a detail to its first line and 400 characters. This
   run's log is **12 KB**.

XL is excluded here for time, not because it is suspect: it is the family where a failure used to
be expensive, and it is where the disk went. The final run puts it back.
