# 2026-09-16 — outlines, snippets and the Postgres planner

The MD and SR families after closing the two coverage gaps recorded in
[`OPTIMISATION-CANDIDATES.md`](../OPTIMISATION-CANDIDATES.md): headings above one document had
no surface but raw SQL and no benchmark at all, and search snippets were never compared
against the query.

`poc` profile, one host, `textdb-sqlite`, `textdb-pg`, `fs` and `sql-text-sqlite`.

## New in this run

- **MD-08** — headings above one document: a whole vault in one call, a heading query as
  `exact` / `prefix` / `contains`, the level filter, and the per-keystroke autosuggest.
- **Snippet checking in SR** — every hit's snippet must contain a term that was searched for,
  folded the way the index folds and word by word so a quoted phrase is not looked for with
  its spaces intact. Only true positives are judged; `snippet_coverage` records what share of
  hits carried a snippet at all.

## What the snippet oracle found

Three ways of showing the wrong text for a right answer, all in what is now
`textdb_core::snippet` (it existed twice and the copies had drifted):

| | |
|---|---|
| raw comparison against a diacritic-folding index | `facade` found the document holding `façade` and showed line 1 |
| a quoted phrase looked for with its spaces intact | `"draft false"` never matched `draft: false` |
| a 200-character *prefix* of the line, not a window | a match past character 200 was off the end of the snippet |

Over the SR corpus: single-term, 2-term and prefix queries went from thousands of wrong
snippets to none; phrases from 3,223 of 6,804 to a handful.

## What the pass changed

| | before | after | |
|---|---|---|---|
| pg `outline_match_exact` | 74.40 ms | 7.70 ms | **9.66x** |
| pg `heading_names_prefix` | 6.74 ms | 3.89 ms | 1.73x |
| pg `outline_one_doc` | 764 us | 447 us | 1.71x |
| pg `outline_level_1` | 11.42 ms | 8.03 ms | 1.42x |
| pg `outline_vault` | 42.35 ms | 36.32 ms | 1.17x |
| SQLite MD-05 `create` (32 headings) | 1.745 ms | 1.637 ms | 1.07x |

Most of the Postgres win is planner statistics. A store built in one burst keeps whatever
autovacuum worked out while its tables were nearly empty, and the heading query then plans as
a nested loop rescanning `section_heading` once per document. `ANALYZE` alone, same query and
same bound parameters, took 74.4 ms to 2.1 ms. `kb.analyze_store()` does it, `textdb import`
and `textdb sync` call it, and `Backend::settle()` lets the harness give every backend the
same chance before anything is timed.

The rest was a scope predicate written as `path = $1 OR (path >= $2 AND path < $3)`, which the
planner estimates at one row; a prefix is now resolved to a file id or a subtree range before
the query is built.

## Where it stops

Row transport, on both engines, and the same measurement says so twice: Postgres executes the
prefix query server-side in 4.1 ms of 15.7 ms measured, and SQLite runs the vault query
aggregated in 3.5 ms against 20.8 ms through the virtual table. Both want a streaming cursor
rather than a materialised result, which is a different piece of work.

## Reading this run

One cell failed, `SR-05 textdb-pg`, with a deadlock. It was self-inflicted: a `textdb import`
was run by hand against the same database while the benchmark was writing to it. Re-run alone
on a quiet cluster it passes, and nothing else in the run touched it. Postgres cells need the
store to themselves — the SQLite backends get a fresh directory under `bench/data`, the
Postgres ones share whatever cluster `--pg` points at.
