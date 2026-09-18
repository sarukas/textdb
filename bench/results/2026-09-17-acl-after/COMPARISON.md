# Before and after folder-scoped delegated access (#12)

`2026-09-16-acl-before` → `2026-09-17-acl-after`, same host, same profile, same seed. 1030 p50
latencies are comparable between the two; cells under 50 µs are dropped, because below that the
host's own drift is the whole signal.

## A note on the three runs

There are three benchmark directories for #12 and only two of them are a comparison.
`2026-09-16-acl-mid-s` was a mid-implementation check — *has anything broken?* — at size `s` with
the XL family excluded. A different size and a different set of families is a different load on the
machine as well as different numbers, so it is a checkpoint, not a data point on this axis. It
answered its question (one failure, the same one the baseline had) and that is all it is cited for.

## The noise floor, first

`fs` and `sql-text-sqlite` are reference backends. Nothing in this change touches either, so
whatever they move is the host:

| Backend | median | geomean | within 10% | worst outlier |
|---|---|---|---|---|
| `fs` | 0.928× | 0.976× | 33 of 145 | XL 100MiB create 10.29× (36.8 → 378.5 ms) |
| `sql-text-sqlite` | 1.070× | 1.132× | 117 of 239 | CR-04 read_lines 4.97× (725.7 → 3608.5 ms) |

A 10× swing on a backend that did not change is the measure of this host. Read any single-cell move
below that scale as noise, and reach for the split below before calling anything a regression.

## The two that changed

| Backend | median | geomean | within 10% |
|---|---|---|---|
| `textdb-sqlite` | 1.105× | 1.143× | 103 of 290 |
| `textdb-pg` | 1.054× | 1.249× | 173 of 356 |

`textdb-sqlite` sits inside `sql-text-sqlite`'s own drift (1.132×) on both figures: the access model
costs it nothing measurable. `textdb-pg`'s median is *below* the reference backend's, but its
geomean is not, and that shape — a good median with a bad geomean — is what a fixed per-operation
cost looks like. Splitting by how long the operation took before says so outright:

| Backend | cells under 2 ms | cells over 2 ms |
|---|---|---|
| `fs` | 0.92× | 1.06× |
| `sql-text-sqlite` | 1.17× | 1.09× |
| `textdb-sqlite` | 1.25× | 1.05× |
| **`textdb-pg`** | **1.84×** | **1.08×** |

Anything that already did real work absorbs it. Small operations are where it shows: `NS-02 list`
5.18× (0.35 → 1.83 ms), `RT-02` 0-byte reads 3.5× (0.33 → 1.18 ms). `textdb-sqlite`'s 1.25× against
`sql-text-sqlite`'s 1.17× on the same split is the two backends drifting together.

## What the run caught, and what was done about it

Three owner-path regressions, all introduced by this feature, all found by this benchmark and not
by the 87 acceptance tests — which is the whole reason the baseline was captured before a line of
it was written.

### 1. The property surface, 10× to 94× — fixed

`kb.prop_keys`, `kb.prop_values` and `kb.prop_find` for the owner, on a store that delegates
nothing. Two causes:

- `account_id_now()` ran `SELECT kb.current_account()` through SPI, and it is the first question
  every per-row helper asks. One round trip per row to learn there is no account. It reads the
  `textdb.token` GUC directly now; `kb.current_account()` returns NULL for an empty GUC by
  construction, so the shortcut is the same answer.
- The predicate. `WHERE ((SELECT kb.current_account()) IS NULL OR kb.visible(n.path)) AND ($1 = ''
  OR (r.key_lc >= $2 AND r.key_lc < $3))` reads as the InitPlan pattern the listing views use, but
  a subquery in the `WHERE` of a *parameterised* statement costs the other conjunct its custom
  plan: the range over `property_kv` stopped being an index scan and the prefixed calls — the
  autosuggest ones, which run per keystroke — went 40–60× slower. Where a statement has parameters
  the predicate is appended only when there is a token. In a view, which has none, the InitPlan
  form is right and stays.

MD-07 is 1.01× geomean now, every cell between 0.86× and 1.03×.

**Ruled out by measurement:** the row-level security policy added in the same batch. It was the
obvious suspect. The policy was removed, the family re-run, and the numbers were identical.

### 2. `kb.current_account()` probing the token table — fixed

It hashed a NULL and probed `kb.token` on every call, and it is an InitPlan of every listing, every
read and every view. The gate is now first and on its own, so with no token the body is a
constant-false qual and the table is never touched.

### 3. `kb.file` reads, +0.45 ms flat — **open, and the hotspot**

`kb.file` is the hot read surface: `SELECT content FROM kb.file WHERE path = $1` is what a client
writes. Giving it a `path` column computed as `CASE … THEN n.path ELSE kb.to_view(n.path) END` made
that equality a function of the column, so it could no longer use `node_path` and every read became
a sequential scan — the same trap `kb.ls` hit on the SQLite side, which CLAUDE.md records at 58× on
2000 files. It is now two branches under a `UNION ALL` gated on an InitPlan, so the owner's branch
keeps its index scan and the account's is a one-time filter that never runs.

That recovered part of it and not all of it. Measured directly, same store, back to back, with
`plan_cache_mode = force_generic_plan` so re-planning is out of the picture:

| `SELECT content, version FROM kb.file WHERE path = $1` | per call |
|---|---|
| the view as it was before #12 | 0.38–0.42 ms |
| the view as it is now | 0.85–0.90 ms |

It is **execution, not planning**, and it is flat in the size of the document. Both shapes of a
two-namespace view cost something: the `CASE` loses the index, the `UNION ALL` pays for a branch
that never runs. Picking the third way is next-iteration work rather than a tweak, and it is the
first item of the hotspot list.

## The one that was not a regression

`textdb-sqlite`'s slowest cells are reads: `NS-02 read` 2.44×, `LL-01..03 read_lines` 2.15×,
`LL-04 history` 2.00×. None of them is this change. In a targeted re-run of the ME family,
`sql-text-sqlite` — which nothing here touches — moved `ME-05 read_version` **2.72×** while
`textdb-sqlite` moved the same cell 1.08×, and `textdb-sqlite`'s ME geomean came out at **0.911×**,
faster than the baseline. A reference backend moving further than the target on the same cell is
the cleanest evidence the host is the cause.

One pointless per-row allocation was removed from the virtual table's owner path anyway — it
translated rows that were already what they would be — but as tidying, not as a fix.
