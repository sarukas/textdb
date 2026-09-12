# textdb-bench

Runner for the comparative test suite in [`docs/test-suite.md`](../../docs/test-suite.md).

```sh
cargo build --release -p textdb-bench
./target/release/textdb-bench run  [--profile poc|spec] [--size xs|s|m|l] [--mode fast|durable]
                                   [--backends a,b,…] [--filter RT,XL-01] [--seed N] [--pg URL]
                                   [--out DIR] [--work DIR] [--drop-caches] [-v]
./target/release/textdb-bench report [--out DIR]          # rebuild report.md from results.jsonl
python3 bench/scripts/verdict.py bench/out/results.jsonl   # claim verdicts (test spec §10)
python3 bench/scripts/key-metrics.py bench/out/results.jsonl
```

## Run size

`--size` scales how much work the matrix does, orthogonally to `--profile`. `--profile`
picks *which* parameter set to read (`poc` or `spec`); `--size` scales the volume
parameters inside it.

| size | scale | use |
|---|---|---|
| `xs` | 0.1x | smoke test — is the harness working |
| `s`  | 0.3x | a full matrix on a laptop |
| `m`  | 1.0x | **default**; identical to the previous behaviour |
| `l`  | 3.0x | closer to `spec` volumes without the full run |

Only volume parameters scale: `n_files`, `file_size`, `size`, `n_edits`,
`sequential_edits`, `ops_per_writer`, `duration_s`, `n_queries`,
`edits_before_search`, `n_versions`, `descendants`, `edit_lines`, and the
`checkpoint_every` / `footprint_every` cadences (which scale with the edit count they
sample, so the number of checkpoints stays constant). Each has a floor so a shrunk test
stays meaningful.

Semantic axes are never scaled: `sizes` straddles the chunker's min/max boundaries on
purpose, `writers` and `readers` are the independent variable of the concurrency
families, and `pattern` / `variant` / `depth` select what is being tested rather than how
much of it. Shrinking `ops_per_writer` makes a concurrency run cheaper without collapsing
its x-axis.

Where scaling a parameter would be wrong, a test declares the value explicitly in a
`[test.<size>]` table, which is taken verbatim and never scaled. XL does this: its
document sizes are deliberate points (10 MiB / 100 MiB / 1 GiB), so each size selects
points rather than shrinking them.

Size matters most for the full-copy-history baselines. `sql-text-sqlite` stores every
version whole, so ME-05 (1 MiB x 3000 edits) alone writes gigabytes — the run's disk
footprint is dominated by it and by XL. `textdb-*` share chunks between versions and stay
far smaller; that gap is a result the suite is measuring, not overhead to tune away.

- `tests/*.toml` — the matrix as data. Each `[[test]]` has `kind` (which suite implementation
  runs it), `spec` parameters (issue #1 scale) and `poc` parameters (scaled for this POC).
- `src/backend.rs` — the uniform `Backend` trait; `src/backends/*` — the six implementations.
- `src/gen.rs` — seeded content generator; `src/reference.rs` — the oracle model.
- `src/suites/*` — one module per family (RT/XL/LL/ME via `edit_sequence`, CW, CR, SR, NS, FP, DU).
- Output: `results.jsonl` (one row per test × backend × mode × cache × rep × case × metric),
  `manifest.json`, `report.md`.

Outcome vocabulary for writes: `committed_direct` (no other commit between the writer's base
and its write), `committed_rebased` (base had moved; textdb rebased or merged), `absorbed_identical`
(textdb: an identical concurrent change already produced that content; no new version),
`conflict`, `contention`, `error`. `lost_updates` is computed against the final content:
for counters, `committed − final counter`; for markers, last committed marker per line missing.
`fs` has no concurrency control, so its lost updates are expected and are the measurement.
