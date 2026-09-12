# bench/

Everything about measuring textdb against the baselines from the test specification
([`docs/test-suite.md`](../docs/test-suite.md), issue #1).

| Path | What |
|---|---|
| [`RESULTS.md`](RESULTS.md) | **The benchmark results** of the POC run: claim verdicts, key metric tables, findings |
| `results/` | Raw artefacts of that run: `results.jsonl`, `manifest.json`, the full generated `report.md` |
| `harness/` | The runner and the six backends ([`harness/README.md`](harness/README.md)) |
| `harness/tests/*.toml` | The test matrix as data (`spec` and `poc` parameter sets) |
| `scripts/pg-start.sh` | Throwaway PostgreSQL 16 cluster for the harness |
| `scripts/verdict.py` | Claim verdicts from `results.jsonl` |
| `scripts/key-metrics.py` | Compact metric tables from `results.jsonl` |
| `dolt/` | Dolt harness design (not run: no binary available, see ADR 0005) |

Reproduce:

```sh
bench/scripts/pg-start.sh
cargo build --release -p textdb-bench
./target/release/textdb-bench run --profile poc --pg postgres://postgres@localhost:54329/postgres --out bench/out --work bench/data
python3 bench/scripts/verdict.py bench/out/results.jsonl
python3 bench/scripts/key-metrics.py bench/out/results.jsonl
```

`--profile spec` runs the matrix at the scale the specification asks for (50k files, 1 GiB
files, minutes per writer count); expect hours.
