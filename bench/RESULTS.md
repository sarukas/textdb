# Benchmark results log

A **manually maintained** log of benchmark runs. Newest first. The harness generates
`report.md`; this file is where a human records which runs are worth keeping, what they
showed, and what changed since the last one.

Each entry links to the run's artefacts under [`results/`](results/). Add an entry only
for a run whose numbers you would quote — a run that crashed, or one taken on a machine
with a known measurement problem, is worth recording precisely because someone will
otherwise re-derive its conclusions by accident.

## How to add an entry

```sh
./target/release/textdb-bench run --size s --backends fs,sql-text-sqlite,textdb-sqlite \
    --out bench/out --work bench/data

ID=$(date +%Y-%m-%d)-s              # date + size, plus a suffix if the day has several
mkdir -p bench/results/$ID
cp bench/out/results.jsonl bench/out/manifest.json bench/out/report.md bench/results/$ID/
```

Then copy the template below to the top of the log and fill it in. Keep the raw
`results.jsonl` — it is the only artefact from which the report can be rebuilt
(`textdb-bench report --out <dir>`), and it is written incrementally, so it survives an
interrupted run.

---

## Template

```markdown
## YYYY-MM-DD — <size>, <backends>

**Artefacts:** [`results/<id>/`](results/<id>/) · **Manifest:** size, profile, mode, seed, host
**Status:** complete | interrupted at <test>

| check | outcome |
|---|---|
| accuracy checks | N pass / N fail |
| timings voided | N cells |
| oracle failures | N (+ N expected) |

Findings — what changed, what is new, what is still open.
```

---

## 2026-09-12 — s, optimisation pass (sql-text-sqlite vs textdb-sqlite)

**Artefacts:** [`results/2026-09-12-s-optimised/`](results/2026-09-12-s-optimised/) — includes `hotspots.md`
**Manifest:** size `s` (scale 0.3) · profile `poc` · mode `fast` · seed 20260912 · 8 CPUs, Windows
**Status:** complete — 39 tests, **508 accuracy checks pass, 0 fail, 0 timings voided**

Goal for this pass: no operation slower than the plain-text SQLite baseline. Not reached
— six operations remain above it — but the two largest gaps closed substantially and the
correctness gap closed completely.

### Against `sql-text-sqlite` (ratio, lower is better; 1.00 is parity)

| operation | before | after | |
|---|---|---|---|
| `read` | 8.56x | **1.62x** | 212k→1.06M calls, the largest block of time in the suite |
| `read_version` | 4.25x | **1.98x** | |
| `create` | 2.04x | **1.58x** | |
| `search` | 4.78x | 8.54x here, ~1020us→650us in isolation | see caveat |
| `history` | 3.28x | 2.78x | 9 calls |
| `list` | 15.81x | 12.58x | 2 calls |
| `replace` | 0.82x | **0.86x** | already beats the baseline |
| `append` | 0.56x | **0.48x** | |
| `rename` | 1.21x | **0.22x** | |
| `maintenance` | 0.88x | **0.64x** | |
| `delete` | 0.10x | **0.05x** | |
| `read_lines` | 0.03x | **0.00x** | 200x+ faster; the fragment read chunking exists for |

Caveat on `search`: the full-matrix figure moved the wrong way, but a controlled A/B of the
two commits at the same size and filter has it going from ~1020us to ~650us median. The
matrix figure is not comparable across runs — different call counts, cache state and
machine load. Trust the A/B.

### What changed

Reading was the dominant cost and was doing one SQL round trip per leaf chunk. Chunks,
tree nodes and whole documents are now cached by BLAKE3 hash, which is sound without
invalidation because the hash *is* the content and nothing in the schema deletes either.
Search resolved every FTS chunk hit to its files one statement at a time — up to
`limit * 50` per term — and walked a file's entire tree per hit to find a line; both are
now single passes.

### What is left, and why it is hard

`read` at 1.62x is the honest floor for this design without deeper work: the baseline
stores a document as one column and returns it in one row fetch, while textdb resolves a
path through a virtual table, then reassembles the document. The remaining measurable
overhead is statement compilation — the virtual table's `filter` calls `prepare`, not
`prepare_cached`, on every read. Caching those statements is the obvious fix and was
tried: it keeps the SQLite handle open past its owner's `close`, so the database file
cannot be released, and vtab teardown cannot run because `close` is what triggers it. It
was reverted. Doing this properly needs the statement cache to live with the virtual table
and be torn down on `xDisconnect`.

Two further hypotheses for `search` were tested and rejected — batching hit resolution
through `json_each` (worse: the table-valued scan costs more than the few queries it
saves when a query matches few files) and moving `ORDER BY rank` out of SQL into Rust
(worse). Neither is in the tree.

Beyond that, closing the last 60% on `read` needs a profiler rather than hypotheses. This
host has roughly +/-40% run-to-run variance, which is wider than the remaining gaps, so
single measurements here are not decisive: every claim above that is not from a 1M-sample
operation came from repeated A/B runs at the same size and filter.

### Correctness

All 508 accuracy checks pass and no cell had its timings voided, so every number above
describes work that was actually done and verified against the oracle.

---

## 2026-09-12 — xs, fs + sql-text-sqlite + textdb-sqlite

**Artefacts:** [`results/2026-09-12-xs/`](results/2026-09-12-xs/) — `results.jsonl`,
`manifest.json`, `report.md`, `run-xs.log`
**Manifest:** size `xs` (scale 0.1) · profile `poc` · mode `fast` · seed 20260912 ·
harness `a749802` · 8 CPUs, Windows · SQLite 3.53.2
**Status:** complete — all 39 tests, all three local backends

| check | outcome |
|---|---|
| accuracy checks | **762 pass, 0 fail** (6 N/A) |
| timings voided | 1 cell (CW-03 / textdb-sqlite) |
| oracle failures | 1 real, 18 expected |

Postgres is not installed on this host, so `sql-text-pg` and `textdb-pg` are recorded
`N/A` throughout rather than skipped. ripgrep is not installed either, so `fs` search is
`N/A` for the SR family — `fs` has no index of its own and searches by shelling out to
`rg`.

This is the first run after the harness gained untimed accuracy checks, named operations
and run sizes, and after four defects found along the way were fixed. **Treat every
earlier number in this repository as unverified**: the runs that produced them were
affected by at least one of the defects below.

`xs` is a smoke scale. It is enough to establish that every test runs, every accuracy
check passes and the concurrency oracles behave — it is not enough to quote performance
or footprint from. FP reports footprint-over-raw of 32.3 for `sql-text-sqlite` against
33.5 for `textdb-sqlite`, which says nothing useful: at 0.1× there are too few edits for
chunk sharing to pay for its own metadata. Re-run at `s` or `m` before drawing any
footprint conclusion.

### Defects fixed before this run

| # | Where | Effect on results |
|---|---|---|
| 1 | Leaked SQLite connections (`sql_text_sqlite.rs`, `textdb_sqlite.rs`) | The two Postgres backends evicted their cached connection on drop; the SQLite ones never did. |
| 2 | Silent reset failure (`backends/mod.rs`) | Combined with 1, the per-repetition wipe failed and repetition 2+ ran on repetition 1's data. Invisible on Linux, where `unlink` succeeds on an open file. **Every multi-repetition SQLite result before this was measuring the wrong state.** |
| 3 | `length()` counts characters (`sql_text_sqlite.rs`, `sql_text_pg.rs`) | `list` under-reported sizes for non-ASCII documents — 97267 bytes against an actual 102400. Found by the new accuracy checks on their first run. |
| 4 | `duration_s` floor applied to a default | The run-size floor turned an absent `duration_s` (meaning "run `ops_per_writer` operations") into 2.0, silently converting every ops-bounded concurrency test into a two-second timed one. Attempts ran 15–35× over the intended count and varied by backend. |

Two crashes on deep paths were fixed as well, all from recursion one frame per path
component, which overflows sooner on Windows (1 MiB main stack) than on Linux (8 MiB):
`walk` and `du` in the harness, and — in the engine itself —
`TextDb::ensure_folder`, which meant **any sufficiently deep path crashed textdb**, not
just the benchmark. `create_dir_all` was replaced with an iterative equivalent for the
same reason. All are now iterative, and NS-02 (folder depth 1000) passes on all three
local backends.

### Open finding: textdb-sqlite loses counter updates at high concurrency

CW-03 has N writers increment one counter line; the oracle is that no update is lost among
those committed. Now that defect 4 is fixed the runs are ops-bounded, so every backend
performs the same number of attempts and the counts are directly comparable.

| backend | guard | N=5 | N=20 | N=50 |
|---|---|---|---|---|
| `fs` | none | 25 committed → 5 | 87 → 6 | 232 → 7 (expected; this is the measurement) |
| `sql-text-sqlite` | OCC on version | 21 → **21** | 77 → **77** | 146 → **146** |
| `textdb-sqlite` | CAS + rebase | 21 → **21** | 81 → **81** | 217 → **13** |

Read as `commits → final counter`; for a correct store the two are equal.

`sql-text-sqlite` is exact at every N — it rejects a stale write outright. `textdb-sqlite`
is **exact at N=5 and N=20 and fails only at N=50**, where it records 217 commits and the
counter reaches 13. That it is correct at low concurrency and wrong at high concurrency
points at a race in the CAS/rebase path rather than a plain logic error, which also
explains why it is intermittent: an earlier `xs` run failed at all three N, this one only
at N=50.

A correct answer is plainly achievable on this workload — the OCC baseline gets it every
time — so this is not a limitation of the test. Either the rebase resolves a three-way
conflict in favour of the stale side instead of conflicting, or it creates versions for
content identical to the head that the `absorbed_identical` path should have caught. In
an earlier ops-bounded run only 8 of 185 such writes were classified absorbed. Not yet
diagnosed; the cell's timings are voided, so no CW-03 latency is published for it.

Reproduce:

```sh
./target/release/textdb-bench run --size xs --backends sql-text-sqlite,textdb-sqlite \
    --filter CW-03,ME-06 --out bench/out-cw --work bench/data-cw
```

### Measurement caveat for this host

Absolute latencies from this machine are noisy: the same RT-01 / `fs` cell measured 11.9 s
in one run and 1.2 s in another, almost certainly Windows Defender scanning the freshly
written corpus. Cross-backend ratios within a single run are the trustworthy part. Set a
Defender exclusion on the `--work` directory before a run whose absolute numbers you mean
to publish.
