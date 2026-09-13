# Optimisation candidates — a review of the 2026-09-12 `s` optimisation pass

Companion to [`RESULTS.md`](RESULTS.md). That log records what the matrix measured and
what the last pass changed; this file records what the numbers look like when the
remaining gaps are taken apart, which of the last pass's conclusions survive that, and
what is worth trying next.

Every figure below is reproducible with

```sh
cargo build --release --workspace
./target/release/textdb-probe all        # ops, statements, prefix, writepath, diff
./target/release/textdb-probe diff-big   # allocates several GiB; can be OOM-killed
```

The probes compare `textdb-sqlite` against a copy of the `sql-text-sqlite` schema in the
same process, at three document sizes, with distinct content for every document — a probe
that writes the same body twice measures chunk sharing, not the write path. Absolute
numbers are host-specific (8 CPUs, Linux container, SQLite 3.53.2 bundled); the ratios
inside one run are the part to trust.

## What the decomposition changes about the last pass's conclusions

**`read` is not uniformly 1.62x — the gap is fixed cost per call, not cost per byte.**

| document | textdb read | baseline | ratio |
|---|---|---|---|
| 8 KiB | 14.0 us | 5.0 us | **2.81x** |
| 100 KiB | 25.8 us | 29.2 us | **0.88x** |
| 1 MiB | 266 us | 199 us | **1.34x** |

At 100 KiB textdb already wins; at 8 KiB it loses by nearly 3x. The matrix's single
aggregate figure averages these, and the 1.06M-call block that dominates the run comes
from the CR/CW/SR families, which use 8 KiB documents — so the published `read` number is
mostly a measurement of per-call overhead, and that is where the remaining work is.
`create` tells the same story: 2.13x at 8 KiB, 1.68x at 100 KiB, 1.02x at 1 MiB.

**The statement-compilation hypothesis in `RESULTS.md` is right, and bigger than it reads.**
It was recorded as "the remaining measurable overhead", with the fix reverted. Measured at
8 KiB, the node-row lookup the virtual table's `filter` performs costs 9.2 us compiled
fresh against 2.2 us from the statement cache — **74% of the whole 9.6 us read gap**.
Across the 1.06M reads in the published run that is on the order of seven seconds.

**Nothing published separates the markdown structure sidecar, and every document in the
matrix is `.md`.** `record_commit` extracts sections, links and frontmatter for
`.md`/`.markdown` only. Writing the same body as `.txt` instead isolates it: a one-line
replace in a 1 MiB document costs 21.5 ms as `.md` against 11.0 ms as `.txt`. Half of the
write is derived-index maintenance, and it is inside every write number in the matrix.

**`replace` beats the baseline by more than the matrix shows, and for a structural reason.**
One line changed in a 1 MiB document: textdb 21.5 ms (`.md`) / 11.0 ms (`.txt`) against
53.2 ms for the full-copy baseline, which must rewrite the body, delete it from FTS and
re-index it. This is the design working, and it widens with document size.

---

## Candidates, ranked by expected payoff

### 1. `myers` sizes its working arrays by the input, not by the distance bound

**The strongest finding here, and the only one that is also a robustness bug.**

`myers()` allocates `v` of width `2 * (n + m) + 3` and pushes a **full-width clone of `v`
into `trace` once per diagonal**. Only diagonals in `[-d, d]` can be live at depth `d`, so
both the array and each trace row need to span the distance bound, not the inputs. As
written, memory and memory traffic are `O(D * (N + M))` even when `D` is tiny.

`diff_seq` trims the common prefix and suffix first, so a single contiguous edit is cheap.
Scattered edits are not: they defeat the trim, and the cost grows with the *square* of the
number of changed lines.

Measured, `byte_edits` over a document with scattered one-line changes:

| document | changed lines | time | peak RSS |
|---|---|---|---|
| 1 MiB | 8 | 3.7 ms | – |
| 1 MiB | 143 | 18 ms | – |
| 1 MiB | 569 | **560 ms** | +254 MiB |
| 8 MiB | 237 | **7.8 s** | +1.2 GiB |
| 8 MiB | 1182 | **42 s** | +6.9 GiB |
| 8 MiB | ~5900 | **process SIGKILLed** | – |

The `max_d` fallback does not save it: the fallback only triggers *after* `max_d` trace
rows have been allocated, so at `max_d = 4096` on an 8 MiB document the transient
allocation is on the order of gigabytes. `byte_edits` runs on every `UPDATE kb SET
content = …` — the primary write path through the virtual table, and the path the Python
library and the CLI use — so an agent that rewrites a large document with many scattered
changes can take the process down.

**Change:** band the V array and each trace row to `min(max_d, n + m)`, indexing relative
to the band. Roughly ten lines in `myers()`; nothing else moves.

**Verified.** A banded implementation was written against the same inputs: identical hunk
sets, every `apply(a, edits) == b` round-trip correct, and

| case | current | banded | |
|---|---|---|---|
| 1 MiB, 143 changes | 111 ms | 3.2 ms | 35x |
| 1 MiB, 569 changes | 2243 ms | 6.4 ms | **350x** |
| 8 MiB, 1182 changes | 42 s | 40.7 ms | **1000x** |
| 8 MiB, ~4700 changes | OOM-killed | 116 ms (bounded fallback) | — |

**Blast radius:** `diff_seq` is also used by `diff3` (every rebase and merge, so the whole
CW family) and by `changed_runs` over leaf-hash sequences. The hunks it returns must stay
byte-identical, which the property tests P1–P6 and the round-trip check above cover.

### 2. The virtual table compiles its statement on every `filter`

`KbCursor::filter` builds one of three fixed SQL strings and calls `conn.prepare`. Worth
7.0 us per read, 74% of the 8 KiB read gap (§above).

`RESULTS.md` records the attempt that was reverted: caching the statement kept the SQLite
handle alive past its owner's `close`, so the file could not be released and vtab teardown
never ran — `close` is what triggers `xDisconnect`. The cause is that the cache lived on a
`Connection` that outlived the owner, not that caching is unworkable. Two ways round it:

- Own the prepared statements in `KbTab` and let `xDisconnect` drop them. `KbTab` is
  destroyed by teardown rather than holding it up, which inverts the lifetime that broke
  last time.
- Keep raw `*mut sqlite3_stmt` handles prepared with `SQLITE_PREPARE_PERSISTENT` and
  `sqlite3_finalize` them in `xDisconnect`/`xDestroy`.

Either needs a test that a `Connection` carrying a `textdb` table still closes and that
the database file can be deleted afterwards — that is the regression the first attempt hit,
and it is not covered today.

### 3. Prefix queries cannot use the path index

Every prefix query is written `substr(path, 1, length(?1) + 1) = ?1 || '/'`, which no index
can serve, so each one is a full scan of `{p}node`. The equivalent range predicate uses the
existing `{p}node_path` index:

```sql
path >= ?1 || '/' AND path < ?1 || '0'     -- '0' is the successor of '/'
```

| prefix | substr scan | indexed range | |
|---|---|---|---|
| `/wide/d00` (2000 files) | 797 us | 14 us | **58x** |
| depth-1000 path | 1463 us | 10 us | **147x** |

This is almost certainly the whole of `list` at 12.58x and NS-02's `list` at 353x. The same
predicate appears in `list_files`, `delete`'s tombstone update, `export`, `search`'s prefix
filter and the harness's own `list` query, so one fix moves several cells.

Worth confirming the collation assumption before landing it: `path` is `TEXT` with default
`BINARY` collation, so `'0'` (0x30) is the immediate successor of `'/'` (0x2F) and the range
is exact. A store created with a different collation would need the bound recomputed.

### 4. The markdown structure sidecar runs inside every write

Half of a 1 MiB markdown replace (21.5 ms against 11.0 ms for the same bytes as `.txt`).
Instrumenting `record_commit` on a 1 MiB create splits it as: re-materialise the document
plus a copy 1.4 ms, `pulldown-cmark` parse 4.7 ms, delete and re-insert 339 section rows
0.5 ms. Three separate things to fix, cheapest first:

- **Stop re-materialising content the caller already holds.** `record_commit` calls
  `st.document(&c.root)` on a root that was just built, which misses the document cache and
  walks the whole tree, then `.clone()`s the result. `create` and `update_content` both have
  the full new content as a `&[u8]` in hand; pass it through.
- **Do not rewrite rows that did not change.** A one-line body edit leaves all 339 sections
  identical. Extract, compare against the stored rows, write the difference. Better still,
  skip extraction when the committed byte range cannot affect structure.
- **Batch the inserts.** One multi-`VALUES` statement instead of N: 0.30 ms against 0.42 ms
  for 339 rows, so worth having but not the main cost.

Beyond that: structure rows are derived data, already HEAD-only by
[ADR 0007](../docs/decisions/0007-structure-rows-head-only.md) and explicitly recomputable.
That makes them a candidate for leaving the commit transaction entirely — rebuilt lazily on
first structural query, or in `maintenance`. That is a semantic change (a `textdb_section`
call straight after a write would have to trigger the rebuild), so it needs a decision
rather than just a patch.

### 5. Fixed per-operation cost at small documents

The regime that holds the run's call volume, and where textdb is 2.8x–3.7x. Each item is
small; together they are the gap.

- `textdb_content` **re-validates UTF-8 that `document()` already decided**.
  `SqliteStorage::document` returns `(Arc<Vec<u8>>, bool)` where the flag is cached because
  it is a property of the content — and then `db.read`/`db.read_version` return `Vec<u8>`,
  dropping it, and `functions::text_or_blob` runs `String::from_utf8` over the whole body
  again. `vtab::column` gets this right; the scalar functions do not. `read_version` is
  3.70x at 8 KiB and 1.67x at 1 MiB, the worst ratio in the table.
- **One avoidable whole-document copy per read, on both paths.** `TextDb::read` does
  `(*st.document(&root)?.0).clone()`, and `vtab::column` does `bytes.to_vec()` before
  handing the bytes to rusqlite, which copies again into SQLite. The cached `Arc` could be
  handed to `sqlite3_result_text64`/`blob64` with a destructor that drops the `Arc` clone,
  making the handoff zero-copy.
- `TextDb::attach` allocates a boxed `MarkdownExtractor` and a prefix `String` on **every**
  scalar-function call and every vtab operation.
- `normalize_path` allocates a `Vec<&str>` and a `join` for every call even when the path is
  already normalised, which it is on every internal call.
- The three content-addressed caches key a `HashMap` by `[u8; 32]`, so every lookup runs
  SipHash over 32 bytes. BLAKE3 output is already uniform: a `BuildHasher` that takes the
  first eight bytes is free and removes the hashing from every chunk, node and document
  access.

### 6. Per-chunk SQL on the write path

A 1 MiB document is ~650 chunks, and the write costs, measured separately:

| | per MiB |
|---|---|
| chunk + hash + tree build (no SQL) | 2.3 ms |
| `chunk` inserts | 3.1 ms |
| `fts` inserts | **12.9 ms** |
| `chunk_ref` (hash → rowid, then insert) | 2.3 ms |

- FTS at chunk granularity is the largest single item and it is a deliberate design choice
  — it is what makes the index insert-only and shareable. Worth asking whether it has to be
  synchronous: the chunk rows are append-only, so indexing could be deferred to
  `maintenance` with search falling back to a scan for not-yet-indexed chunks. That is a
  semantic change and needs the recall/precision checks to confirm it is invisible.
- `chunk_ref` resolves each hash back to a rowid with `SELECT id FROM {p}chunk WHERE
  hash = ?1` — one statement per chunk, when `put_chunk` already saw
  `last_insert_rowid()` for every chunk it inserted. Thread the ids out of `put_chunk`, or
  key `chunk_ref` by hash (it is `WITHOUT ROWID` already) and drop the lookup; the second
  costs 24 bytes a row, which FP-01/02 can price.
- `put_chunk` **recounts newlines that `build_with_chunks` already counted** for the same
  bytes, and copies every chunk into the cache on write (`Arc::new(b.to_vec())`) even when
  the chunk already existed.

### 7. Node access clones the node

`Storage::get_node` returns `Node` by value, so `SqliteStorage` clones a `Vec<Child>` of up
to `MAX_FANOUT` = 512 entries (~25 KiB) out of the cache on **every** node access. The last
pass added `chunk_shared` to avoid exactly this for chunk bytes; the node equivalent was
never added. A `node_shared() -> Arc<Node>` alongside it, with `Lru<Arc<Node>>` behind it,
is the same argument one level up.

The same clone shows up inside the edit path, which is the part of the design that matters
most:

- `tree::Frame` holds a `Node` by value, so `Cursor::clone()` deep-copies every node on the
  path. `edit::apply_one` clones a whole `Cursor` **per suffix leaf** it pulls into the
  re-chunk window (`old_bounds.push_back((buf.len(), after.clone()))`), and
  `Cursor::parent()` deep-copies the path again on every level of `rebuild_level`.
  `Frame { node: Arc<Node> }` makes all of these pointer bumps.
- `diff::common_prefix` and `common_suffix` clone the children vector at each level and
  `pop` from the clone.

Unmeasured — this one is read off the code, not off a probe, and it should be measured
before it is believed. `replace` already beats the baseline, so the payoff is in the CW
family and in `create`, not in an obviously broken cell.

### 8. Search

8.54x in the matrix, ~4.35x in the controlled A/B the last pass recorded. Two hypotheses are
already tested and rejected there (`json_each` batching, moving `ORDER BY rank` into Rust);
do not repeat them. Three that have not been tried:

- **The `limit * 50` hit window is fixed.** For `limit = 20` that ranks and joins up to 1000
  chunk hits per term to produce at most 20 files. Start at `limit * 4` and widen only when
  the intersection comes up short — a pure win whenever a term matches many chunks in few
  files, which is the common case for a knowledge base.
- **Two statements per candidate.** Each surviving file costs a `node` lookup and a `chunk`
  lookup. The node columns could come from the FTS join that already joins `node`; only the
  chosen chunk's bytes need a second fetch.
- **`find_leaf` walks the tree per hit** to turn a chunk into a line number. `chunk_ref`
  already stores `(chunk_id, file_id, version)`; adding the chunk's line offset in that
  version would make the resolution O(1). The complication is that a chunk can occur twice
  in a document and the table is append-only, so the stored offset is only valid for the
  version that wrote it — the current `find_leaf` call doubles as the "is this chunk still in
  HEAD" check, and anything replacing it has to keep that.

### 9. The write critical section spans work that does not need the lock

CW-06 at 3.78x and CW-07 at 4.56x are the largest write gaps in the matrix, and the 8 KiB
single-writer `replace` figure is 1.47x — so most of it is per-operation cost rather than
contention, but not all.

`TextDb::tx` opens `BEGIN IMMEDIATE` and holds the write lock across the whole operation:
materialising the base document, `byte_edits`, chunking, every `chunk` and `fts` insert,
`chunk_ref`, the commit row and the structure sidecar. The baseline's equivalent is one
statement. But chunk and node writes are append-only, idempotent and content-addressed —
they are safe to perform *outside* the transaction that does the CAS. Only the `node.root`
CAS, the commit row and `chunk_ref` need the lock.

A chunk written by an attempt that then loses its CAS becomes unreferenced garbage, which
`maintenance` already has to handle; and FTS rows for such chunks cannot cause a false
search hit because `chunk_ref`, written under the lock, is what maps a chunk to a file.

**How to test it:** `--filter CW-04,CW-06,CW-07 --backends sql-text-sqlite,textdb-sqlite` at
`--size s`, before and after, at N=20 and N=50. If the gap is lock-hold time, per-op write
latency should fall with N; if it is per-operation cost, it will not move and this candidate
should be dropped in favour of §5 and §6.

### 10. Deep paths cost a round trip per component

`ensure_folder` walks up one `node_by_path` at a time and then inserts one row per missing
component: 21 ms to create a file at depth 1000, against 224 us for the baseline, which
stores no folder rows at all (NS-02 `create`, 218x). Both loops are iterative since the
stack-overflow fix, so this is a round-trip count, not a correctness risk. One recursive CTE
to find the deepest existing ancestor plus one batched insert would collapse it. Low value —
NS-02 is a one-cell pathological case — but it is cheap and the cell is the worst ratio in
the whole matrix, which invites misreading.

---

## Two build defects found while probing

Neither is a performance matter, but both block measuring the surfaces they affect, and CI
does not see either because it builds only the workspace.

- **`textdb-sqlite` did not build on its own.** It takes rusqlite with
  `default-features = false` and never re-enables `cache`, which rusqlite 0.40 has as a
  default feature, so every `prepare_cached` call failed to resolve. Inside the workspace
  the harness pulls rusqlite with defaults and feature unification hid it. Fixed here by
  naming `cache` explicitly in `crates/textdb-sqlite/Cargo.toml`; the workspace build and
  all tests are unaffected.
- **`crates/textdb-sqlite-ext` does not build at all.** It is written against a pre-0.40
  rusqlite entry point: `Connection::extension_init2` now takes four arguments, and
  `ffi::sqlite3_mprintf` is no longer exposed. This is the artefact `docs/INSTALL.md` and
  the Python SQLite backend depend on, so the Python library's SQLite path cannot currently
  be built from this tree. Not fixed here — the new `extension_init2` signature wants a
  decision about how the extension reports errors, not a mechanical patch. Adding
  `cd crates/textdb-sqlite-ext && cargo build --release` to CI would stop it drifting again.

---

## Suggested order of work

1. §1 `myers` banding — largest win, bounded change, fixes a crash.
2. §3 prefix ranges — one predicate, several cells, no semantic change.
3. §2 vtab statement cache — the known residual on the highest-volume operation; needs the
   teardown test the first attempt lacked.
4. §4 first two bullets and §5 — the fixed-cost items, all local.
5. §9 measurement — cheap to run, and it decides whether the concurrency work is real.
6. §6, §7, §8 — each needs a measurement before a patch.

Re-run the matrix at `--size s` against `sql-text-sqlite` after each of 1–4 and add an entry
to [`RESULTS.md`](RESULTS.md). This host's variance is ±40% on single cells, so the same
discipline applies as last time: anything that is not a million-sample operation needs a
repeated A/B at the same size and filter, not one run.
