# 2026-09-15 — MD family, five backends, size s

First run of the MD family (the structure sidecar: markdown links, front matter, sections,
change feed). `fs`, `sql-text-sqlite` and `sql-text-pg` record 14 N/A rows each and no
timings — they have no such index, which is the expected result, not a gap in the run.

Both textdb bindings pass every oracle and **agree exactly** on the generated graph
(5760 links recorded, 360 of them correctly reported broken), which is a useful
cross-check: the SQLite and Postgres resolvers are separate implementations.
