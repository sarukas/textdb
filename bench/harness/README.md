# textdb-bench

Runner for the comparative test suite in [`docs/test-suite.md`](../../docs/test-suite.md).

```sh
cargo build --release -p textdb-bench
./target/release/textdb-bench run  [--profile poc|spec] [--mode fast|durable] [--backends a,b,…]
                                   [--filter RT,XL-01] [--seed N] [--pg URL] [--out DIR] [--work DIR]
                                   [--drop-caches] [-v]
./target/release/textdb-bench report [--out DIR]          # rebuild report.md from results.jsonl
python3 bench/scripts/verdict.py bench/out/results.jsonl   # claim verdicts (test spec §10)
python3 bench/scripts/key-metrics.py bench/out/results.jsonl
```

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
