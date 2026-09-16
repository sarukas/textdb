# After the results-shape overhaul

The same 48 tests, four backends, `poc` profile, size `m`, `fast` mode as
`../2026-09-16-shape-before`, on the same host, run back to back with it.

One oracle failure, the same one the baseline shows:
`CW-06 textdb-pg version_count_matches_commits: 290 versions for 263 commits`.
It predates this work and is unrelated to row shapes — CW-06 renames a folder
away and back under 20 concurrent writers, and the count it compares is taken
across the rename.

The pg extension was reinstalled once during this run, as it was during the
baseline. Here the change was a SQL string inside a function body with no
signature change, and the family it affects had not run yet, so no cell is
voided by it. The baseline's voided cells are listed in that run's README.
