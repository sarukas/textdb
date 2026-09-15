# bench/

Everything about measuring textdb against the baselines from the test specification
([`docs/test-suite.md`](../docs/test-suite.md), issue #1).

| Path | What |
|---|---|
| [`RESULTS.md`](RESULTS.md) | **The benchmark results** of the POC run: claim verdicts, key metric tables, findings |
| [`OPTIMISATION-CANDIDATES.md`](OPTIMISATION-CANDIDATES.md) | Where the remaining gaps come from when taken apart, and what to try next |
| `results/` | Raw artefacts of that run: `results.jsonl`, `manifest.json`, the full generated `report.md` |
| `harness/` | The runner and the six backends ([`harness/README.md`](harness/README.md)) |
| `harness/tests/*.toml` | The test matrix as data (`spec` and `poc` parameter sets) |
| `scripts/pg-start.sh` | Throwaway PostgreSQL 16 cluster for the harness |
| `scripts/verdict.py` | Claim verdicts from `results.jsonl` |
| `scripts/key-metrics.py` | Compact metric tables from `results.jsonl` |
| `harness/src/bin/textdb-probe.rs` | Micro-probes behind `OPTIMISATION-CANDIDATES.md`: one cost at a time, against the baseline schema in-process |
| `dolt/` | Dolt harness design (not run: no binary available, see ADR 0005) |

## Quick start

```sh
cargo build --release -p textdb-bench

# A few minutes, a few hundred MB: enough to see the shape of every result.
./target/release/textdb-bench run --size s --backends fs,sql-text-sqlite,textdb-sqlite \
    --out bench/out --work bench/data

# Add the Postgres backends (without --pg they are recorded as N/A, never skipped silently).
bench/scripts/pg-start.sh
./target/release/textdb-bench run --size s --pg postgres://postgres@localhost:54329/postgres \
    --out bench/out --work bench/data
```

`run` writes `results.jsonl` as it goes and `report.md` at the end, so a run that is
interrupted still leaves usable raw data. Rebuild the report from it at any time:

```sh
./target/release/textdb-bench report --out bench/out
python3 bench/scripts/verdict.py bench/out/results.jsonl      # claim verdicts (spec §10)
python3 bench/scripts/key-metrics.py bench/out/results.jsonl  # compact metric tables
```

`bench/out/report.md` is generated in full by the harness. `bench/RESULTS.md` is the
curated write-up, assembled separately:

```sh
bench/scripts/make-results.py bench/out bench/RESULTS.md bench/scripts/results-narrative.md
```

## Knobs

| Flag | Default | What it does |
|---|---|---|
| `--size xs\|s\|m\|l` | `m` | How *much* work each test does. See [Run size](#run-size). |
| `--profile poc\|spec` | `poc` | *Which* parameter set to read. `spec` is the scale issue #1 asks for (50k files, 1 GiB documents, minutes per writer count) and takes hours. |
| `--backends a,b,…` | all six | `fs`, `fs-git`, `sql-text-sqlite`, `sql-text-pg`, `textdb-sqlite`, `textdb-pg`. |
| `--filter RT,XL-01` | all | Family or test-id prefixes, comma separated. |
| `--mode fast\|durable` | `fast` | `durable` means `synchronous=FULL` / `fsync` per commit for every backend alike (fairness rule 2). |
| `--pg URL` | — | Postgres for the two `*-pg` backends. Absent, those cells are `N/A`, not skipped. |
| `--seed N` | `20260912` | Seeds the content generator; a rep adds its index, so reps differ but a run is reproducible. |
| `--out DIR` | `bench/out` | `results.jsonl`, `manifest.json`, `report.md`. |
| `--work DIR` | `bench/data` | Where backends put their data. Wiped per backend per rep. |
| `--drop-caches` | off | Drop the page cache before rep 1 for a cold-cache number. Linux only; elsewhere every rep is recorded `warm`. |
| `--tests DIR` | `bench/harness/tests` | The matrix TOML. |
| `-v` | off | Verbose. |

`results.jsonl` appends, so point a second run at a fresh `--out` unless you mean to
combine them (which is how `fs-git` gets added to a run for one test only).

## Run size

`--size` scales how much work the matrix does. It is orthogonal to `--profile`:
`--profile` picks which parameter set to read, `--size` scales the volume parameters
inside it.

| size | scale | roughly |
|---|---|---|
| `xs` | 0.1× | smoke test — is the harness working |
| `s` | 0.3× | a full matrix on a laptop |
| `m` | 1.0× | **default**; identical to the previous behaviour |
| `l` | 3.0× | closer to `spec` volumes without the full run |

Size matters most for the full-copy-history baselines. `sql-text-*` store every version
whole, so a single test like ME-05 (1 MiB × 3000 edits) writes gigabytes. On one filtered
run the `sql-text-sqlite` database was 2.0 GB at `m` and 25 MB at `xs`, against 17 MB and
5.4 MB for `textdb-sqlite`. That gap is a result the suite is measuring — chunk sharing
against full copies — not overhead to tune away.

Only *volume* parameters scale: `n_files`, `file_size`, `size`, `n_edits`,
`sequential_edits`, `ops_per_writer`, `duration_s`, `n_queries`, `edits_before_search`,
`n_versions`, `descendants`, `edit_lines`, and the `checkpoint_every` / `footprint_every`
cadences — which scale with the edit count they sample, so the number of checkpoints
stays the same at every size. Each has a floor, so a shrunk test does not degenerate.

Semantic axes are never scaled: `sizes` straddles the chunker's min/max boundaries on
purpose, `writers` and `readers` are the independent variable of the concurrency
families, and `pattern` / `variant` / `depth` select what is being tested rather than how
much of it. Scaling `ops_per_writer` makes a concurrency run cheaper without collapsing
its x-axis.

Where scaling would be the wrong move, a test names the value outright in a
`[test.<size>]` table, taken verbatim and never scaled. XL does this: its document sizes
are deliberate points (10 MiB / 100 MiB / 1 GiB), so each size *selects* points rather
than shrinking them.

## The test matrix

Eleven families, defined as data in `harness/tests/*.toml`. `reps` repetitions per cell; the
report takes the median.

### RT — round-trip accuracy

| Test | What it measures |
|---|---|
| RT-01 | Sizes straddling chunk min/max; create → read; oracle bytes identical |
| RT-02 | RT-01 sizes × line endings {lf, crlf, no trailing newline} |
| RT-03 | 1 MiB × {ascii, mixed unicode, random bytes}; random bytes may be N/A where TEXT rejects invalid UTF-8 |
| RT-04 | 100 KiB, random replace ops, read after each equals the reference |
| RT-05 | As RT-04, then `read_version(v)` for every v equals the reference history |
| RT-06 | Corpus import, exported via list+read and compared to the reference; import time reported |

### XL — very large documents

| Test | What it measures |
|---|---|
| XL-01..06 | Create, full read (MB/s), `read_lines` at 0/50/100 %, replace 3 lines at 0/50/100 % (write amplification), sequential replaces (footprint growth per edit), history + `read_version(v1)` |

### LL — long lines

| Test | What it measures |
|---|---|
| LL-01..03 | Single line of {1 MiB, 100 MiB} ASCII: create → read identical; replace 10 B mid-line; `read_lines(1,1)` returns the whole line |
| LL-04 | 10 MiB minified JSON (no newlines, high entropy); 100 replaces; textdb leaf stability (unchanged leaves per edit, target ≥ 0.99) |
| LL-05 | 10 MiB as 1000 lines of 10 KiB; replace in line 500 (compare with XL-04 at the same size) |
| LL-06 | Single line, mixed Unicode with 4-byte code points; replace across a multibyte boundary; no backend may corrupt UTF-8 |

### ME — micro-edits

| Test | What it measures |
|---|---|
| ME-01/02 | 100 KiB; sequential counter increments on one line; footprint after; `read_version` at v1, v(n/2), v(n) |
| ME-03 | 100 KiB; sequential appends; latency trend (last decile / first decile), footprint |
| ME-04 | 100 KiB; edits alternating start/end of file; textdb leaves changed per edit ≤ 4 |
| ME-05 | 1 MiB; Zipf-distributed line edits; footprint vs raw; history latency at end |
| ME-06 | 20 concurrent writers incrementing one counter line, base = last seen version; outcome histogram + lost updates |

### CR — concurrent reads

| Test | What it measures |
|---|---|
| CR-01 | One 100 KiB file; N readers in a closed loop; throughput, p50/p99 |
| CR-02 | Corpus of files, N readers, Zipf file choice |
| CR-03 | CR-01 with one writer replacing every 10 ms; reader p99 degradation and torn reads (every read must equal some committed version) |
| CR-04 | N readers of `read_lines` (50 lines) on a 100 MiB file — isolates fragment-read cost |
| CR-05 | 100 readers × 2-term AND search over the corpus |
| CR-06 | N readers × `read_version` of a random historical version; file with 1000 versions |

### CW — concurrent writes

| Test | What it measures |
|---|---|
| CW-01 | N writers, one file, disjoint sections; oracle: all last edits present |
| CW-02 | N writers, one file, same section, distinct lines |
| CW-03 | N writers, one file, same line (counter); conflicts expected; oracle: no lost updates among committed |
| CW-04 | N writers, random files (Zipf s=1.0) from a corpus |
| CW-05 | N writers append to one file; oracle: every committed append present |
| CW-06 | 20 writers under a folder while another thread renames the folder away and back; writes must not fail on the rename race |
| CW-07 | CW-04 sustained; `fs-git` `index.lock` failures counted as contention |

Write outcomes: `committed_direct` (no other commit between the writer's base and its
write), `committed_rebased` (base had moved; textdb rebased or merged),
`absorbed_identical` (an identical concurrent change already produced that content),
`conflict`, `contention`, `error`. `lost_updates` is computed against the final content.

A backend that declares no write guard is *expected* to lose updates — `fs` has none, and
that count is precisely what CW is there to report. Those violations are recorded as
`EXPECTED` and, unlike a real oracle failure, do not void the cell's timings.

### SR — search

| Test | What it measures |
|---|---|
| SR-01/02 | Index build / corpus import; single-term, 2-term AND, phrase and prefix queries: p50/p99, recall/precision |
| SR-04 | Search after N edits: index growth per edit and maintenance (VACUUM / optimize / gc) time |
| SR-05 | Search restricted to a path prefix — prefix + FTS combination |

Ground truth is the reference tokenizer (word boundary, case-insensitive), equivalent to
`rg -w -i -F`. AND is document-level for every backend.

### NS — namespace

| Test | What it measures |
|---|---|
| NS-01 | Many files in one folder: list latency; create p99 at the end |
| NS-02 | Folder depth 1000 (`/a/a/a/…`): create, read, list |
| NS-03 | Rename a folder with {1k, 10k, 50k} descendants: latency; all paths updated, contents unchanged, versions preserved |
| NS-04 | Delete a folder with 10k descendants, then read a deleted file's last version |
| NS-05 | Path characters: spaces, dots, Unicode, 255-byte name, 4 KiB path, `%` and `_` |

### MD — the structure sidecar

Markdown links, front matter, sections and the change feed: the operations textdb has and
no baseline does. `fs` keeps no index of what a document links to, and the `sql-text-*`
stores keep text and nothing derived from it, so **every cell here is `N/A` for them**,
recorded with its reason. The family is not asking who is faster; it establishes what these
operations cost, and — in MD-01 — what maintaining them costs the write path that every
other family measures.

| Test | What it measures |
|---|---|
| MD-01 | The same bytes written as `.md` (the extractor runs) and `.txt` (it does not), at 0/8/64 links per document: `create_overhead_pct` and `replace_overhead_pct` are what the sidecar costs a write |
| MD-02 | A known link graph with planted dangling targets: outbound links per document, the whole subtree in one query, backlinks, and broken-link validation |
| MD-03 | Rename a file that N others link to, under `link_updates` = off / report / rewrite: latency and documents rewritten per move |
| MD-04 | Front matter: reading the parsed block per document, and setting one key with the rest of the document byte-identical |
| MD-05 | Documents with 32 headings: listing sections, and fetching one section's body by heading |
| MD-06 | Change feed: a watcher polling after every write, and one catching up from zero |

Every case is generated with a **known** link graph, front matter and heading tree, so the
oracle is what the generator wrote rather than whatever the backend returns. A backend that
indexes nothing and answers instantly fails the check and publishes no timings. Links are
written as relative `[text](./f00042.md)`, not wiki links, because a wiki link resolves by
name across the whole store and its expected status would depend on what else the corpus
happens to hold.

Two things MD-01 controls for, because both would swamp the effect it measures:

- The `.md` and `.txt` writes are **interleaved, alternating which goes first**. Writing all
  of one kind and then all of the other hands the second a warmed page cache and a larger
  store.
- `links0` is not "no structure": those documents still have front matter and eight
  headings, so its overhead is the cost of parsing and recording those. The *link* cost is
  the difference between `links0` and the denser cases.

### DU — durability

| Test | What it measures |
|---|---|
| DU-01..03 | `kill -9` / `dm-flakey` crash tests. These need an out-of-process supervisor per backend; this harness runs every backend in one process, so the cells are recorded `N/A` with the reason rather than silently skipped. |

### FP — footprint

| Test | What it measures |
|---|---|
| FP-01/02 | Corpus; Zipf edits; footprint at every checkpoint; then maintenance (`git gc --aggressive` / `VACUUM FULL` / FTS optimize) |

## What makes a result trustworthy

Every cell is bracketed by accuracy checks that run **outside all timing**, because
timings alone cannot tell work from the appearance of work — a backend that silently does
nothing, or that starts a repetition on the previous one's data, posts excellent numbers.

Before a cell: the store is empty, and a canary document survives create → read → delete.
After it: everything listed is readable, reported sizes match the bytes read, storage is
non-zero, and — where the suite keeps a reference model — every document matches the
oracle byte for byte. The canary is skipped for FP and DU, where even a deleted document
leaves a tombstone that would bias a footprint measurement, and the skip is recorded.

**A cell that fails a check publishes no timings at all.** Its latency rows are withheld
and replaced by a `timings_voided` row, so a broken backend cannot show a fast time. The
report's *Accuracy checks* table summarises this per backend, and *Operations* gives
latency per named operation across the whole run.
