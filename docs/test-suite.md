# textdb — Stress & Comparative Test Suite Specification

Version 0.1 · Companion to `spec.md` §9 · 2026-09-12 · Source: [issue #1](https://github.com/sarukas/textdb/issues/1)

> Mirrors issue #1. The implementation lives in `bench/harness`; test cases are the TOML
> files in `bench/harness/tests/`. Deviations are listed at the end of this file.

## 1. Goal

Every test in this suite runs the same operations against every storage format under identical conditions, so that each cell of the results matrix answers one question: *for this operation, at this scale, what does each format cost, and does it stay correct?* No test is allowed to exist for only one backend. Where a backend cannot perform an operation natively, the suite either emulates it in the most idiomatic way for that backend (and says so) or records `N/A` — never silently skips.

## 2. Storage formats under test

| ID | Format | Versioning | Concurrency control | Search | Purpose |
|---|---|---|---|---|---|
| `fs` | Plain files, one per document | None | None (last writer wins, `rename(2)` for atomic replace) | `grep -r` / `ripgrep` | What everyone starts with |
| `fs-git` | Files + `git commit` per write | Full | None at write; merge at pull | `git grep` | The status quo the project replaces |
| `sql-text-pg` | Postgres `doc(id, path, body TEXT, version INT)` + `doc_rev` full-copy history | Full copy per version | OCC on `version` | `tsvector` GIN on `body` (recomputed per edit) | The naive database answer |
| `sql-text-sqlite` | Same schema in SQLite + FTS5 external-content | Full copy | Single writer | FTS5 | Embedded naive answer |
| `textdb-sqlite` | Spec §7.1 | Chunk-shared | Single writer, CAS + rebase | FTS5 on chunks | Algorithm under test, embedded |
| `textdb-pg` | Spec §7.2 | Chunk-shared | MVCC, CAS + rebase | GIN on chunks | Algorithm under test, server |

`fs-git` is included because "remove git push/pull" is the stated motivation; the comparison is incomplete without the incumbent.

## 3. Fairness rules

1. **Same machine, same run.** All backends execute the full matrix in one session on one host; the host does nothing else. Hardware, kernel, filesystem, and Postgres/SQLite versions are recorded in the report header (`manifest.json`).
2. **Durability parity.** Two modes, both reported: `durable` (fs: `fsync` after every write; Postgres: `synchronous_commit = on`; SQLite: `synchronous = FULL`) and `fast` (fs: no fsync; Postgres: `synchronous_commit = off`; SQLite: `synchronous = NORMAL`, WAL). Never compare across modes.
3. **Cache parity.** Each test runs cold (drop page cache, restart DB, first access) and warm (third repetition). Both reported.
4. **Same bytes.** Content is generated once per test from a fixed seed and fed identically to every backend. Byte-exactness is verified against the generator's copy, not against another backend.
5. **Idiomatic emulation only.** `replace` on `fs` is read → splice in memory → write temp → `rename`. Not `sed -i` and not mmap tricks. Emulations are listed in §5.
6. **No backend-specific tuning beyond documented defaults** except: Postgres `shared_buffers` = 25% RAM, `work_mem` = 64 MB; SQLite `cache_size` = 256 MB, `mmap_size` = 1 GB. Applied equally to `sql-text-*` and `textdb-*`.
7. **Client overhead excluded from latency**, measured from request dispatch to response receipt on a local socket. Content generation and verification happen outside the timed region.

## 4. Uniform backend interface

See `bench/harness/src/backend.rs` — `Backend` trait with `create/delete/rename/list`, `read/read_lines/read_version`, `overwrite/replace/append`, `search/history`, and the measurement hooks `storage_bytes`, `bytes_written_since_reset`, `reset_counters`. `WriteOutcome` is `Committed { version, direct }`, `Conflict { current_region }` or `Contention`.

`storage_bytes` / `bytes_written`: fs via directory walk and `/proc/self/io` (`wchar`) plus children `ru_oublock` for git; Postgres via `pg_total_relation_size` and `pg_stat_io` + `pg_stat_wal` (PG16); SQLite via file sizes and `/proc/self/io`.

## 5. Emulation table

| Op | `fs` | `fs-git` | `sql-text-*` | `textdb-*` |
|---|---|---|---|---|
| `replace` | read, splice, write-temp, rename | as `fs` + `git add && git commit -q` | `UPDATE … SET body = replace(body,$1,$2), version = version+1 WHERE path=$3 AND version=$4 AND position($1 in body) > 0`; 0 rows → `Conflict` | native (spec §6.6): `edit()` or `UPDATE … SET content, base_version` |
| `read_lines` | read whole file, slice | as `fs` | `SELECT body` then slice client-side | native `lines()` |
| `read_version` | `N/A` | `git show <sha>:<path>` | `SELECT body FROM doc_rev WHERE …` | native |
| `history` | `N/A` | `git log --follow` | `SELECT version FROM doc_rev` | native |
| `search` | `rg -l -i -w -F` per term, file sets intersected | `git grep -l -i --all-match` | `WHERE tsv @@ to_tsquery($1)` / FTS5 `MATCH` | native (per-term chunk hits, file sets intersected) |
| `rename` folder | `rename(2)` (O(1)) | `git mv` | `UPDATE doc SET path = replace(path,$1,$2) WHERE path LIKE $1||'/%'` | native |
| Concurrency guard | none — lost updates are *expected and counted* | none | OCC | CAS + rebase |

## 6. Content generator

Deterministic, seeded (`bench/harness/src/gen.rs`): log-normal line lengths (median 80 B, p99 ≈ 400 B), headings 0.5/KB, paragraphs, lists, fenced code, tables, wikilinks, YAML frontmatter; charsets ASCII / mixed Unicode (Lithuanian, CJK, emoji) / random bytes; line endings `\n`, `\r\n`, none at EOF. Also single-line, minified-JSON-like and fixed-length-line generators for LL tests.

## 7. Test matrix

The families RT, XL, LL, ME, CR, CW, SR, NS, DU, FP and their cases are defined in `bench/harness/tests/*.toml` exactly as in issue #1 §7.1–§7.10, each with a `spec` parameter set (the scale the issue asks for) and a `poc` parameter set (what this POC ran). Repetitions: as in the TOML (`reps`), medians and p99 across repetitions.

## 8. Metrics

latency (p50/p95/p99/max), throughput, bytes_written, write_amplification, footprint, outcomes {committed_direct, committed_rebased, conflict, contention, error} plus `absorbed_identical`, lost_updates, torn_reads, corruption (`identical` oracles), recall/precision, plus textdb structural counters (chunks, tree depth, leaves changed per edit).

## 9. Runner

`bench/harness` (`textdb-bench run … | report`). Test cases are TOML data. Reference model: in-memory `Vec<u8>` per document plus version log. One OS thread per agent, own connection per thread. Output: one JSON line per (test, backend, mode, cache_state, repetition, case, metric) in `results.jsonl` (Parquet was replaced by JSONL to avoid an Arrow dependency), `manifest.json`, and a markdown matrix per family plus a ratio summary normalised to `fs` in `report.md`. Oracle failures mark the cell **FAIL** and do not abort the run.

## 10. What a decisive result looks like

| Claim (spec §1) | Deciding cells | Pass condition |
|---|---|---|
| 1 — O(edit) writes | XL-04, LL-02, ME-04 | `textdb` write_amplification ≤ 10 and flat across sizes; others grow linearly |
| 2 — conflict rate | ME-06, CW-01..03 | `textdb` conflict ≤ 0.1 × `sql-text` conflict at N=20 on disjoint and same-section tests; zero lost updates |
| 3 — insert-only index | SR-03, SR-04 | `textdb` index growth per edit ≈ chunk size; `sql-text-pg` ≈ document size |
| 4 — SQL surface | RT-06, NS-03, NS-04 | all pass on `textdb-pg` through the view/trigger path, not direct table access |
| Not worse where it shouldn't be | XL-02, CR-01, CR-04 | `textdb` full read ≤ 2 × `fs` warm; fragment read ≤ 1 × `fs` |

## Deviations in this POC

- Search ground truth is an in-process reference tokenizer (word-boundary, case-insensitive), equivalent to `rg -w -i -F`, computed from the reference model rather than by shelling out to `rg`.
- `bytes_written` for in-process backends is `/proc/self/io wchar` (bytes handed to `write(2)`); for `fs-git` the children's `ru_oublock` is added; for Postgres `pg_stat_io` writes plus WAL bytes.
- DU family recorded as N/A (see ADR 0005). SR-03 staleness is not measured separately: every backend here indexes synchronously in the writing transaction.
- Results are JSONL rather than Parquet.
