# Dolt harness (spec §7.3) — not run in this POC

No Dolt binary was reachable from the build environment (GitHub releases are blocked), so
the Stage 2 Dolt comparison is recorded as N/A in `docs/results.md` (ADR 0005).

Design kept for when a binary is available:

```sql
CREATE TABLE chunks (
  file_id     BIGINT      NOT NULL,
  ordinal_key VARCHAR(64) NOT NULL,   -- fractional ordinal key, e.g. 'a', 'am', 'b'
  hash        BINARY(32)  NOT NULL,
  text        LONGTEXT    NOT NULL,
  PRIMARY KEY (file_id, ordinal_key)
);
```

Runner outline (`run.sh`): import the corpus by running `textdb-core`'s chunker over each
file and inserting one row per chunk; for each simulated agent `dolt checkout -b agent-N`,
apply the CW-01..03 workloads as row updates/inserts on `chunks`, `dolt commit`, then
`dolt merge` into `main` and count merges that report cell-level conflicts. Report the
same outcome histogram as the harness for the same workload seed.
