# Before and after the results-shape overhaul

`2026-09-16-shape-before` → `2026-09-16-shape-after`, same host, same profile,
run back to back. 1068 p50 latencies are comparable between the two (metrics
under 50 µs are dropped as noise).

## The noise floor, first

`fs` and `sql-text-sqlite` are reference backends. Nothing in this change
touches either of them, so whatever they move is the host, not the code:

| Backend | median | geomean | within 10% | worst outlier |
|---|---|---|---|---|
| `fs` | 1.038× | 0.939× | 62 of 173 | CW-06 write 4.10× (0.114 → 0.469 ms) |
| `sql-text-sqlite` | 1.029× | 1.036× | 131 of 254 | CR-01 read 82× (0.032 → 2.590 ms) |

An 82× swing on a backend that did not change is the measure of how much this
host drifts under concurrency at sub-millisecond scale. Read every single-cell
move below that scale as noise.

## The two that changed

| Backend | median | geomean | within 10% |
|---|---|---|---|
| `textdb-sqlite` | 1.041× | 1.081× | 153 of 306 |
| `textdb-pg` | 1.021× | 1.001× | 172 of 335 |

Both medians sit inside the reference backends' own drift, so the overhaul is
cost-neutral across the suite as a whole — the five new stored counters, the
`title` column and the wider listing rows do not show up in the write path.

## Except search, which is slower by design

| Cell | Before | After | |
|---|---|---|---|
| SR-01/02 `single` | 8.25 ms | 50.81 ms | 6.16× |
| SR-01/02 `prefix` | 22.15 ms | 142.34 ms | 6.43× |
| SR-01/02 `and2` | 5.03 ms | 11.66 ms | 2.32× |
| SR-01/02 `phrase` | 1.57 ms | 2.22 ms | 1.41× |
| SR-04 `single` | 6.61 ms | 36.90 ms | 5.58× |
| CR-05 (100 readers × 2-term AND) | 0.77 s | 1.75 s | 2.28× (pg) / 3.09× (sqlite) |

Two deliberate changes account for this, and both were asked for:

1. **One row per matching line.** The index works on chunks, so on its own it
   knows which documents match and guesses at the line. Every surface now reads
   the matching documents and lists the lines that really hold the terms. `line`
   became a fact instead of a hint, and the cost is the read.
2. **200 rows everywhere**, where the SQL functions returned 100.

So search does roughly twice the rows and verifies each one. What it buys shows
in the same run: `prefix` precision went from 0.99999 to 1.0, and a document
whose words only ever appear apart is no longer returned at all.

One correctness figure moved the wrong way: `phrase` recall fell from 0.9985 to
0.9724. A phrase that straddles a line break matched the document before and
matches no single line now. That is the honest cost of line granularity for
phrase queries, and it is the first thing to look at in a follow-up pass —
along with the 6×, which is a document read per hit and has obvious room
(read once per document rather than per hit, and stop at the row limit).

## Oracles

One failure in each run, the same one:
`CW-06 textdb-pg version_count_matches_commits`. It predates this work.
