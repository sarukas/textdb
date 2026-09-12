# ADR 0005 — What the POC could not run in this environment

Status: recorded (Stage 3b)

| Item | Status | Reason |
|---|---|---|
| Dolt harness (`bench/dolt`) | N/A | No Dolt binary reachable (GitHub releases blocked); schema and runner design kept in `bench/dolt/README.md` |
| DU-01..03 crash tests | N/A | Killing a writer or the server mid-write needs an out-of-process supervisor and `dm-flakey`; the POC harness runs all backends in one process |
| Cold-cache measurements | warm only | `/proc/sys/vm/drop_caches` is not writable in the container; the `cache` column says `warm` |
| Spec-scale runs (50k files, 1 GiB files, 10 min per N) | `poc` profile | Run time; every TOML case carries both `spec` and `poc` parameter sets so the full matrix can be replayed with `--profile spec` |
| Real corpus (RT-06/07 on ≥ 10k real markdown files) | synthetic | No corpus available offline; `corpora/import.sh` documents the procedure |
| pgrx 0.19 | pgrx 0.18.1 | 0.19 needs rustc ≥ 1.96; the toolchain here is 1.94 |
