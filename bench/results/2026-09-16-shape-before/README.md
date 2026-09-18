# 2026-09-16 — before the results-shape pass

The full matrix at `poc` on `fs`, `sql-text-sqlite`, `textdb-sqlite` and `textdb-pg`, taken as
the baseline for the listing/search consistency work (`docs/shapes.md`). Its pair is
`2026-09-16-shape-after`.

## Read this before comparing

**The `textdb-pg` SR-01/02, SR-04 and SR-05 cells are void.** The Postgres extension was
rebuilt and reinstalled while this run was in flight, so those cells ran a new extension —
where `kb.search` no longer has a `snippet` column — against the old harness binary. The
`column "snippet" does not exist` failures and the SR-01/02 deadlock are that, not anything
about the store. Every other cell, and every `fs`, `sql-text-sqlite` and `textdb-sqlite` cell,
is a clean measurement: none of them touches the Postgres cluster.

Two failures are real and worth keeping:

- **`CR-04 textdb-sqlite` — `too many SQL variables`.** A genuine bug this run found, in code
  committed before it. The section insert batched 4,000 rows per statement, which was under
  SQLite's 32,766-parameter limit at six columns per row and over it at ten once the heading
  and word-count columns were added, so a heading-dense document could not be committed. Fixed
  in the pass this run precedes; the batch size is derived from the column count now and a
  test writes 5,000 headings.
- **`CW-06 textdb-pg` — 200 versions for 167 commits.** Pre-existing and unrelated, recorded
  in earlier runs too.

## What the pass changed

Shapes, not algorithms: one twenty-four-key listing record and one seven-key hit row on every
surface, five new counters on the node row (`title`, `nsections`, `nprops`, `nlinks`,
`nlinks_broken`) maintained the way `nwords` already was, and search returning one row per
matching *line* rather than one per document with a guessed line.

The counters are the only write-path cost, and the line expansion the only read-path one. The
after run is what says how much.
