# Benchmark results log

A **manually maintained** log of benchmark runs. Newest first. The harness generates
`report.md`; this file is where a human records which runs are worth keeping, what they
showed, and what changed since the last one.

[`OPTIMISATION-CANDIDATES.md`](OPTIMISATION-CANDIDATES.md) takes the remaining gaps from the
newest entry apart operation by operation and ranks what to try next; it also corrects two
of that entry's conclusions.

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

## 2026-09-13 — s, Windows, after the live-app work (trash, path history)

**Artefacts:** [`results/2026-09-13-s-windows/`](results/2026-09-13-s-windows/) — `results.jsonl`,
`manifest.json`, `report.md`, `run.log`
**Manifest:** size `s` (scale 0.3) · profile `poc` · mode `fast` · seed 20260912 · 8 CPUs,
Windows 11, NTFS, desktop with other work open · SQLite 3.53.2 · rustc 1.93.0 · harness
`b6adf31`
**Status:** complete — 39 tests, `fs` + `sql-text-sqlite` + `textdb-sqlite`, 513 s wall clock

| check | outcome |
|---|---|
| accuracy checks | **all pass** (43 per check per backend; canary 2 n/a) |
| timings voided | **0 cells** |

The first run after the store gained the change feed, the trash and path history: every
delete and rename now also writes change and path-event rows. The suite still passes every
oracle, and the two operations that grew work are still far ahead of the baseline.

### Against `sql-text-sqlite` (p50 ratio, lower is better; same run, same machine)

| operation | calls (textdb) | this run | Linux run above | |
|---|---|---|---|---|
| `read` | 1.31M | 1.39x | 1.52x | 96 µs against 69 µs |
| `create` | 4904 | 2.31x | 2.96x | |
| `replace` | 8561 | **0.78x** | 1.30x | p99 72 ms against 250 ms |
| `append` | 1275 | **0.42x** | 0.36x | |
| `read_lines` | 41.0k | **0.22x** | 1.10x | |
| `read_version` | 14.9k | **0.82x** | 0.35x | |
| `search` | 1462 | 2.49x | 2.80x | still the largest gap |
| `history` | 9 | 2.54x | 1.69x | 9 calls |
| `list` | 2 | 2.88x | 3.36x | 2 calls |
| `delete` | 1 | 0.17x | 0.04x | one sample, now with path events and change row |
| `rename` | 1 | 0.27x | 0.19x | one sample, now with path events and change row |
| `maintenance` | 1 | 0.62x | 0.83x | |

**Do not compare absolute numbers with the Linux run.** This is a Windows desktop: `fs` pays
NTFS and on-access scanning on every file operation (`create` p50 11.6 ms here against
306 µs on Linux, `delete` of a large folder 2.56 s), and several families are
duration-bounded, so a faster engine does more operations and changes cache pressure (`read`
calls: 1.31M here, 4.16M on Linux). Ratios within one run are the comparable quantity.

### Claims (`bench/scripts/verdict.py`)

| claim | this run | Linux run above |
|---|---|---|
| 1 — O(edit) writes: XL-04 write amplification | **not measurable here**: every backend reports 0.000, including `fs` — the harness cannot read the storage footprint on this host | textdb 441, `fs` 43 150, `sql-text` 199 585 |
| 1 — ME-04 leaves changed per edit | max 1.00 (target ≤ 4) | max 1.00 |
| 2 — conflict rate at N=20 | CW-01 0.000 vs 0.250; CW-02 **0.028 vs 0.200** (0.14x; the claim asks ≤ 0.1x); CW-03 0.143 vs 0.200; zero lost updates | CW-02 0.044 vs 0.372 |
| 3 — insert-only index: SR-04 growth per edit | 3 444 B vs 3 607 B | 3 221 B vs 3 607 B |
| 4 — SQL surface through the Postgres views | not run (no Postgres) | not run |
| not worse: XL-02 full read vs `fs` | 3.42x (target ≤ 2x) | 3.71x |

The verdict script prints FAIL for every claim on both runs: claim 1 needs the footprint the
harness cannot measure on Windows, claim 2's CW-02 ratio is 0.14x against a 0.1x target, and
claim 4 needs Postgres. Those are the open items, unchanged by this work.

---

## 2026-09-13 — s, Linux, optimisation pass 2 (+ the Postgres and Python surfaces made buildable)

**Artefacts:** [`results/2026-09-13-s-linux/`](results/2026-09-13-s-linux/) — `results.jsonl`
and `results-before.jsonl`, `hotspots.md` and `hotspots-before.md`, `probe-after.txt` and
`probe-before.txt`, `manifest.json`, `report.md`, `run.log`
**Manifest:** size `s` (scale 0.3) · profile `poc` · mode `fast` · seed 20260912 ·
4 CPUs, 15.7 GiB, Linux container, ext4 · SQLite 3.53.2 · ripgrep 14.1.0 · PostgreSQL 16
**Status:** complete. Two sets of runs. A **before/after pair** — 39 tests,
`fs` + `sql-text-sqlite` + `textdb-sqlite`, run back to back on a quiet machine with the
binaries from `ba598a5` and `182a64b` — and a **five-backend run** adding `sql-text-pg` and
`textdb-pg` ([`results/2026-09-13-s-five/`](results/2026-09-13-s-five/)). `fs-git` is
excluded and not measurable on this host; see that section for why.

| check | before/after pair (identical in both) | five-backend run |
|---|---|---|
| accuracy checks | **834 pass, 0 fail** (96 n/a) | **1397 pass, 0 fail** (161 n/a) |
| timings voided | **0 cells** | **0 cells** |

First run on Linux rather than Windows, and the first where every surface in the repository
could actually be built — see *What was broken* below.

CW-03, the lost-update failure recorded against `textdb-sqlite` on the Windows `xs` run, did
not reproduce in any run here: `lost_updates` is 0 in every CW cell, for both bindings, across
five runs at size `s`. Nothing in this pass touches the CAS or rebase path, so that is absence
of evidence on a different host, not a fix. **Treat it as open** until it is either reproduced
deliberately or traced.

### Against `sql-text-sqlite` (ratio, lower is better; 1.00 is parity)

| operation | calls | before | after | |
|---|---|---|---|---|
| `read` | 2.2M → 4.2M | 1.12x | **0.61x** | 16 us → 9 us; now beats the baseline |
| `create` | 4849 | 1.71x | **1.55x** | |
| `replace` | 10.5k | 3.74x | **3.12x** | dominated by the CW family, see below |
| `append` | 1275 | 0.49x | **0.38x** | |
| `list` | 2 | 6.76x | **3.35x** | not comparable: the harness query changed too |
| `read_version` | 17–19k | 2.95x | 2.97x | unchanged here, 4x better at 8 KiB — see below |
| `search` | ~1.9k | 11.24x | 11.41x | untouched; the largest gap left |
| `history` | 9 | 2.71x | 2.83x | 9 calls |
| `read_lines` | 52–60k | 0.08x | **0.07x** | |
| `delete` | 1 | 0.04x | 0.04x | |
| `rename` | 1 | 0.13x | 0.19x | one sample; not a signal |
| `maintenance` | 1 | 0.73x | 0.83x | |

**Read the matrix numbers with care this time.** Several tests are duration-bounded, so a
faster engine performs *more* operations: `read` went from 2.2M to 4.2M calls between the two
runs. That changes cache pressure and makes cross-run `p50` comparison weak for anything
whose cost depends on the caches — which is exactly why `read_version` looks flat here while
the controlled probe has it 4x better at 8 KiB. The matrix's `read_version` p50 is 5.7 ms,
i.e. the XL and LL cells, where the cost is materialising megabytes rather than per-call
overhead.

### The controlled A/B, per document size

`textdb-probe ops`, same process, same work, distinct content per document. This is the
measurement to trust for per-operation cost; the matrix is the measurement to trust for
"does the whole suite still pass".

| operation | 8 KiB | 100 KiB | 1 MiB |
|---|---|---|---|
| `read` (warm) | 2.87x → **1.15x** | 0.84x → **0.63x** | 1.30x → **1.23x** |
| `read_version` | 3.83x → **0.94x** | 1.54x → **0.61x** | 1.46x → **1.25x** |
| `replace` (one line) | 1.64x → 1.77x | 0.56x → **0.45x** | 0.37x → **0.34x** |
| `create` | 2.30x → 2.38x | 1.72x → **1.47x** | 1.68x → **1.57x** |

The two 8 KiB write cells moved the wrong way in the archived pair, which was taken while
another matrix run had the machine; an earlier quiet pair had 8 KiB `replace` 1.55x → 1.23x
and `create` 2.16x → 1.92x. Sub-millisecond cells on a 4-CPU container are not decisive
either way, and saying so is cheaper than pretending the pair is clean.

### What changed, and what it was worth

Five things, each measured before it was believed; full detail and the rejected alternatives
are in [`OPTIMISATION-CANDIDATES.md`](OPTIMISATION-CANDIDATES.md).

1. **`myers` sized its working arrays by the input instead of the distance bound** — the only
   one that was a crash rather than a slowdown. A 1 MiB document with 569 scattered one-line
   changes took 1539 ms and 254 MiB of resident memory; 8 MiB with 1182 changes took 42
   seconds and 6.9 GiB; a little more than that was **killed by the OOM reaper**, because the
   `max_d` fallback only fires after `max_d` full-width rows are already allocated. This runs
   on every `UPDATE kb SET content = …`. Now 7.2 ms, 41 ms, and a bounded fallback; the trace
   is O(D²) whatever the document size, and the hunks are unchanged.
2. **Subtree queries could not use the path index** — 55x–143x against an equivalent range,
   62x–159x through the virtual table, which now accepts bounds on `path`.
3. **The virtual tables recompiled their statements on every call** — 8 KiB read through `kb`
   14.1 us → 7.1 us. This is the optimisation the last pass reverted; what sank it was where
   the cache lived, not caching.
4. **The scalar functions could not reuse a statement cache at all** —
   `textdb_content(path, 1)` on 8 KiB, 29.5 us → 9.9 us.
5. **The markdown sidecar rebuilt what the write already knew** — it re-materialised the
   document from the root just built, and rewrote every structure row on every commit even
   when a body edit had not touched a heading.

One idea in the middle of (4) was wrong and is recorded rather than buried: folding
`read_version`'s two lookups into one statement made it **ten times slower**, because
`{p}node_path` is a partial index over live rows and a historical read must find tombstoned
files too.

### Footprint, unchanged by this pass

The design's clearest win, and none of the above touches it.

| test | textdb-sqlite | sql-text-sqlite |
|---|---|---|
| LL-04 (many versions of one file) | 14.6 MB, **4.66x raw** | 84.4 MB, 26.83x raw |
| LL-05 | 12.2 MB, **4.10x raw** | 83.0 MB, 27.85x raw |
| ME-05 (1 MiB, Zipf edits) | 9.4 MB, **31.9x raw** | 186.7 MB, 629.7x raw |
| FP-01/02 after maintenance | 10.3 MB, **7.14x raw** | 16.8 MB, 11.71x raw |
| XL-01..06 growth per edit | **0 B** | 10.5 MB |

### What was broken before any of this could be measured

Three defects stood between a fresh clone and a working install; all three were invisible to
CI, which built `--workspace` and nothing else.

| # | Where | Effect |
|---|---|---|
| 1 | `crates/textdb-sqlite` | Took rusqlite with `default-features = false` and never re-enabled `cache`, so every `prepare_cached` call failed to resolve: 46 errors. The workspace unified the feature from a sibling, hiding it. |
| 2 | `crates/textdb-sqlite-ext` | Written against a pre-0.40 rusqlite entry point (`extension_init2` signature, `ffi::sqlite3_mprintf`). **The loadable extension — what INSTALL.md and the Python library use — could not be built at all.** |
| 3 | `python/textdb/backends/{sqlite,postgres}.py` | Raised a bare `TextdbError` with `code = "TX003"` where they meant `NotFound`, so `except NotFound` never matched a missing path. Caught by the Python suite the moment the extension could be loaded. |

And two in the Postgres binding, found while replacing its prefix predicates: `p || '/%'`
treats a path's own `%` and `_` as wildcards, so `kb.folder.nbytes_total` for `/100%_done`
counted `/100XXXdone`'s files, and **`kb.search('y', '/100%_done')` returned a document from
`/100XXXdone`** — scoping a search to a folder did not actually scope it.

CI now builds both SQLite crates on their own, runs the Python suite, and runs a new
`load_extension_smoke.py` that loads the `.so` into a stock `sqlite3` and checks every
surface answers. Verified by hand here as well: the Postgres extension installs and answers,
all four Python examples run, the CLI loads and searches a folder, and the `sqlite3` shell
invocation in INSTALL.md works as written.

### The full matrix, five backends

A second run at the same size adding `sql-text-pg` and `textdb-pg`, once the Postgres
extension could be built at all. **Artefacts:**
[`results/2026-09-13-s-five/`](results/2026-09-13-s-five/) — `results.jsonl`,
`manifest.json`, `report.md`, `verdict.md`, `key-metrics.md`, `hotspots-sqlite.md`,
`hotspots-pg.md`, `run.log`.

| backend | accuracy checks | timings voided | operation errors |
|---|---|---|---|
| `fs` | 280 pass, 0 fail | 0 | 0 |
| `sql-text-sqlite` | 280 pass, 0 fail | 0 | 0 |
| `sql-text-pg` | 277 pass, 0 fail | 0 | 2 (see LL-04 below) |
| **`textdb-sqlite`** | **280 pass, 0 fail** | **0** | **0** |
| **`textdb-pg`** | **280 pass, 0 fail** | **0** | **0** |

1397 checks pass, none fail, no cell has its timings voided. **`fs-git` is excluded and not
measurable on this host:** an unrelated process held roughly 19,800 of the ~20,000 available
file descriptors for most of the session, so every `git` invocation failed through the
commit-signing helper. Three six-backend runs were attempted; `fs-git` accumulated 22
operation errors and 65 voided cells in the worst of them, and the textdb results were
identical in all three. The harness did the right thing — it failed those cells and withheld
their latencies — but `fs-git` numbers from this machine are not publishable.

#### `sql-text-pg` cannot store a large document at all

```
LL-04 sql-text-pg create: db error 54000:
  string is too long for tsvector (1515944 bytes, max 1048575 bytes)
```

PostgreSQL caps one `tsvector` at 1 MB, so a backend that indexes whole documents has a hard
ceiling on document size. textdb indexes chunks of about a kilobyte, so `textdb-pg` stored
the same corpus without trouble. This is claim 3 showing up as the baseline *failing* rather
than merely being slower, and it is a structural limit, not a tuning question.

#### Footprint across backends

| test | `fs` | `sql-text-sqlite` | `sql-text-pg` | **`textdb-sqlite`** | **`textdb-pg`** |
|---|---|---|---|---|---|
| LL-04, many versions of one ~3 MB file | 3.1 MB (no history) | 84.4 MB, 26.8x | *could not store it* | **14.6 MB, 4.66x** | **18.5 MB, 5.89x** |
| LL-04 growth across versions | — | 67.5 MB | — | **0 B** | **0.23 MB** |
| ME-05, 1 MiB with Zipf line edits | 0.3 MB (no history) | 186.7 MB, 630x | 217.2 MB, 732x | **9.6 MB, 32.4x** | **20.5 MB, 69.2x** |
| FP-01/02 after maintenance | — | 16.8 MB, 11.7x | 7.3 MB, 5.07x | **10.3 MB, 7.14x** | 11.4 MB, 7.95x |

`sql-text-pg` is the smaller store after `VACUUM FULL` on FP-01/02, which is worth saying
plainly: at that corpus and edit count, full-copy history in Postgres compacts better than
chunk sharing plus its metadata. The picture inverts as versions accumulate — ME-05 is the
same comparison after many more edits, and there the gap is 36x.

#### Claims

| claim | verdict | reading |
|---|---|---|
| 1 — O(edit) writes | FAIL | On the threshold, not the behaviour. `leaves_changed_max` is **1.00** for both bindings and `leaves_unchanged_frac_min` **0.968**: exactly one leaf is rewritten per edit. But the pass condition is `write_amplification <= 10`, a ratio, and a three-line replace must write at least one ~1 KB chunk plus tree nodes — so the floor for any chunked store is tens. Measured 441 (`textdb-sqlite`) and 81.8 (`textdb-pg`) against 199,585 for `sql-text-sqlite`. The "flat across sizes" half is not evaluable at size `s`, where XL runs a single size. **The criterion needs a decision; it was not relaxed here.** |
| 2 — conflict rate | FAIL | Narrowly, and passing the case that matters most. CW-01 (disjoint sections) at N=20: **0.000** conflict for both bindings against 0.300 for `sql-text-sqlite` and 0.906 for `sql-text-pg`. CW-02 (same section, distinct lines) is 0.050 against 0.350 and 0.117 against 0.900 — 0.14x and 0.13x, just outside the 0.1x bar. **Zero lost updates in every cell, including CW-03.** |
| 3 — insert-only index | FAIL | Growth per edit on SR-04 is 3,221 B (`textdb-sqlite`) against 3,607 B (`sql-text-sqlite`) — the criterion wants under half, and at this corpus a 100 KiB document is only a few chunks, so "chunk-sized" and "document-sized" are not far apart. LL-04 and ME-05 above are where the claim is visible. |
| 4 — SQL surface | **PASS** | RT-06, NS-03 and NS-04 all clean on `textdb-pg` through the `kb.file` / `kb.folder` view and trigger path. **First time this claim could be evaluated at all**, because the Postgres extension did not build before this pass. |

#### Two harness defects the Postgres backends exposed

Both were latent behind the build break and affected `sql-text-pg` and `textdb-pg` equally,
so both were fixed symmetrically.

| # | Where | Effect |
|---|---|---|
| 1 | `maintenance` on both PG backends | Ran `VACUUM FULL` through `batch_execute`, which PostgreSQL wraps in an implicit transaction: "25001: VACUUM cannot run inside a transaction block". SR-04 and FP-01/02 reported an error instead of a maintenance time, and every footprint-after-maintenance figure for either backend was missing. |
| 2 | `leaf_set` in the ME suite | Implemented for `textdb-sqlite` only, so ME-04's `leaves_changed` counters were never recorded for `textdb-pg` — and claim 1 is judged on them, so it could never pass for that binding whatever it did. `kb.leaf_hashes(path)` is the hook it needed. |

One measurement gap found and **not** fixed: `write_amplification` reads 0.0 for `textdb-pg`,
because `bytes_written_since_reset` reports nothing for the Postgres backends on this host.
That is why claim 1's `textdb-pg` figure above (81.8) comes from XL-04 rather than ME-04.

### What is left

`search` at 11.4x is the largest remaining gap and was not touched. `replace` at 3.1x in the
matrix against 0.34x in the single-writer probe is the CW family: the baseline rejects a stale
write outright where textdb diffs and rebases, which is the feature working — but the write
transaction also holds the lock across chunk and FTS inserts that are append-only and
content-addressed and need not be inside the CAS. Both, with the measurement that would
decide the second, are in `OPTIMISATION-CANDIDATES.md`.

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
