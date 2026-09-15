# Optimisation candidates

Companion to [`RESULTS.md`](RESULTS.md). That log records what the matrix measured;
this file records where the time goes when a gap is taken apart, what has been done about
it, and what is left — including the things that were tried and turned out not to work, so
nobody pays for them twice.

Reproduce every figure here with

```sh
cargo build --release --workspace
./target/release/textdb-probe all        # ops, statements, scalar, prefix, writepath, tree, diff
./target/release/textdb-probe diff-big   # allocates several GiB; can be OOM-killed
```

The probes compare `textdb-sqlite` against a copy of the `sql-text-sqlite` schema in the
same process, at three document sizes, with distinct content for every document — a probe
that writes the same body twice measures chunk sharing, not the write path. The matrix
runner measures whole operations, which is the right unit for "is textdb competitive" and
the wrong one for "where does the time go": one cell mixes the engine, the SQL surface, the
structure sidecar and the index writes into a single number.

Absolute numbers are host-specific. Ratios inside one run are the part to trust, and a
before/after pair should come from one quiet machine, back to back.

## Postgres: one SPI query per chunk is the whole story (2026-09-15)

`textdb-pg` is 4-50x its SQLite sibling across the board, and loses to its own baseline
`sql-text-pg` on the operations that matter most: `read` 6.45x, `create` 6.86x, `replace`
2.96x, `search` 2.69x. It wins where chunk sharing pays — `append` 0.48x, `read_lines`
0.36x, `read_version` 0.67x, `history` 0.68x — so the design is sound and the
implementation is not.

Nearly all of the read gap is one line. `SpiStorage::get_chunk` issues
`Spi::get_one_with_args("SELECT bytes FROM kb.chunk WHERE hash = $1")` **once per chunk**,
with no read cache (`pending_chunks` only serves writes) and no prepared plan, so every
leaf costs a fresh parse, plan and execute.

Measured on this host against a 982 KB document with 596 leaves at depth 2, with transport
excluded (`SELECT length(content)` returns four bytes, so only server work is timed):

| document | leaves | server-side read | per leaf |
|---|---|---|---|
| 62 KB | 41 | 1.73 ms | 42.2 us |
| 244 KB | 154 | 3.66 ms | 23.8 us |
| 983 KB | 596 | 13.8 ms | 23.1 us |
| 1.98 MB | 1225 | 26.7 ms | 21.8 us |

Linear in *leaves*, flat per leaf at ~22 us — per-chunk work, not per-byte. The SQLite
binding materialises the same shape at roughly 0.6 us per leaf, in process and through a
cached statement.

The ceiling is easy to establish. Fetching all 596 chunks in **one** query, the tree walk
to find them included, costs 2.3 ms warm against 17.5 ms for the per-chunk path:

```sql
SELECT sum(length(c.bytes)) FROM kb.chunk c
 WHERE c.hash IN (SELECT hash FROM kb.leaf_hashes('/s14000.md'));   -- 2.3 ms
SELECT length(content) FROM kb.file WHERE path = '/s14000.md';      -- 17.5 ms
```

So about 15 ms of a 17.5 ms read is per-query overhead rather than reaching the data.

### Done: batched, ordered chunk fetch (2.6x on a 983 KB read)

`materialize_all` now walks the tree a level at a time and fetches the leaves with
`unnest($1::bytea[]) WITH ORDINALITY`, which returns the rows **in the order they were
asked for**, so the bytes are appended as they arrive and each chunk is copied once. Server
side, a 983 KB / 596-leaf read went 13.8 ms -> 5.27 ms. Across the matrix `read` is a median
0.86x with a best of 0.35x and `replace` a median 0.73x with a best of 0.38x; the 10 MiB XL
cells are all about 0.37x. `create`, `search` and `history` are unchanged, as they should
be. 120 byte-exact oracle checks pass and the 8 Postgres CLI tests pass.

Three things only showed up by measuring, and are why the final shape is not the obvious
one:

- **A cache plus a walk still handles every chunk twice.** Batching into a `HashMap` and
  then walking the tree to copy back out gave only 2.0x and left ~10 us a leaf. Ordering the
  fetch so the second pass is unnecessary is what took the rest.
- **Batching makes small documents worse.** The first cut sped 10 MiB reads 2.6x while
  making 512 B reads 1.76x *slower*: `unnest ... WITH ORDINALITY` joined against `kb.chunk`
  costs more to plan than `WHERE hash = $1`, and on a one-leaf document that planning is the
  whole read. Small documents are also what the RT, CR and SR families issue most. Hence
  `BATCH_FLOOR`: under 8 leaves, stay on the single-row path.
- **Dropping the node cache cost a query.** Nodes are asked for twice per read — once to
  decide whether batching is worth it, once by the walk that assembles — so without a cache
  a one-leaf document paid three queries where it had paid two. A node-only cache (never
  chunks, which are asked for once each) restored it and helped large reads too, since the
  assembling walk reuses what the level scan read. Small-document reads ended at 30 us
  against 41 us without it.

The fast path declines rather than guesses: unflushed pending writes, a tree it cannot walk,
or a short result all fall back to the ordinary walk.

**Now the limit:** at ~10 us a leaf what remains is pgrx turning each `bytea` datum into a
`Vec<u8>`, not query overhead. That is inherent to reading bytes into Rust, so going further
needs a different approach — assembling in Postgres, or avoiding the copy — not more
batching.

**Still to do — prepare once.** Where a per-row query has to stay, `Spi::prepare` a plan and
reuse it rather than re-planning per call. Same fix that took `textdb-sqlite`'s scalar
functions from 29.5 us to 9.9 us, one layer down.

The client round trip is *not* the problem and should not be optimised first: this Postgres
answers a trivial statement in 82-117 us, which bounds how much of a 648 us small-document
read transport can explain.

Not settled here: whether `textdb-pg` also carries the write regression the SQLite binding
was shown to have. The old-versus-new A/B needs the previous commit's extension installed
into a cluster, which this pass did not do; the cross-host figures suggest it does, and the
rename path is shared logic, but that is inference and not a measurement.

## The feature work since 2026-09-13 cost the write path (2026-09-15)

Re-running the matrix on current `main` turned up a broad write regression. It is not the
host, and it is not noise: the prior run's commit (`c93a7b7`) was built in a worktree and
the two binaries were run **back to back on one machine, twice each**, over RT-01, NS-03 and
NS-04.

| cell | old | new | ratio |
|---|---|---|---|
| NS-03 folder rename, 1000 files | 3,185 us | 18,257 us | **5.73x** |
| NS-04 folder delete | 1,338 us | 6,012 us | **4.49x** |
| RT-01 create, 512 B | 163 us | 592 us | **3.63x** |
| RT-01 create, 4 KiB | 400 us | 957 us | 2.39x |
| RT-01 create, 1 MiB | 44,244 us | 74,932 us | 1.69x |
| RT-01 read, 1 MiB | 275 us | 329 us | 1.20x |

The control is `fs`, which is identical code in both trees. Its best-sampled cell, NS-04
delete, moves 1.12x across the same pair of runs while textdb moves 4.49x on that same cell
in that same run. (`fs`'s RT-01 create cells are too thinly sampled to use — one reads
0.17x and another 1.60x. Quote the NS-04 control, not those.)

**The shape says fixed cost per write, not cost per byte.** The regression is worst at the
smallest documents (3.6x at 512 B) and fades as they grow (1.7x at 1 MiB), which is what
per-commit bookkeeping looks like — sidecar rows, the change feed, path events and folder
totals are all paid once per commit regardless of size. Reads are untouched.

Two things are identified; the rest is not, and this entry does not pretend otherwise.

**Rename re-resolves the link graph, and did so even with no links in the store.**
`git bisect` over the 85 commits, running NS-03 on `textdb-sqlite` at each step, names
`59f6ec3` "Links: resolved in the store" as the first commit to cross the threshold — though
the readings climb (897, 1423, 2827, 3681 us), so it is cumulative rather than one cliff.
That commit added two full subtree listings and a `relink` to every move and delete.

An earlier draft of this entry said `relink` was where the 5.7x lived, on the strength of
the `link_updates` settings recovering only ~20 %. That was wrong, and the measurement that
corrected it is simple: a **1000-file folder rename in a store with no links at all took
24 ms**, against 29 ms with one link per file. The links were never the bulk of it — the
bookkeeping around them was, and it ran whether or not there was anything to book.

Four things fixed, in order of what they were worth:

1. **Skip the bookkeeping when the store records no links.** `has_links()` is one indexed
   probe; without it a plain-text store listed the whole moved subtree and queried the empty
   link table to discover there was nothing to do. 1000-file rename 24 ms -> 14 ms.
2. **Derive the post-move listing instead of re-querying it.** `after` is `before` with the
   path prefix substituted — exactly what the `UPDATE` did — so the second full subtree scan
   was asking the database for what the caller already knew.
3. **Split the `OR`.** `l.resolved_id IN (...) OR l.file_id IN (...)` let SQLite use neither
   the `link_resolved` nor the `link_file` index, turning two seeks into a scan of the link
   table on every move. Two statements, one per column.
4. **Skip no-op link updates, and cache the statement.** Re-resolving usually confirms what
   the row already said (a folder rename moves a file and the siblings its relative links
   point at together), yet every row was rewritten regardless — an `UPDATE` and a WAL record
   each. `relink_where` also used `prepare` rather than `prepare_cached`, recompiling a
   500-placeholder statement per chunk.

Measured on current `main` after all four: NS-03 folder rename 19,633 -> 14,770 us (0.75x),
NS-04 delete 5,957 -> 4,756 us (0.80x), creates unchanged. 131 workspace tests and the 8
Postgres CLI tests pass, and the links still resolve to their new paths after the move.

Not recovered: the old code did NS-03 in 588 us at `xs` against about 3,200 us now. What is
left is `resolve_link` running once per link row in the moved subtree, which for the NS-03
corpus is every file. Removing that needs a sound argument about when a resolution *cannot*
change — for a folder rename, a relative link between two files that both moved keeps its
target — and that is a correctness question, not a tuning one. Left alone deliberately.

**The sidecar is now measured rather than inferred.** MD-01 writes the same bytes as `.md`
and `.txt` and reports the difference: 34 % on create for front matter and eight headings
alone, 87 % at 8 links per document, 257 % at 64. That is a real cost but it is not 3x, so
the sidecar is part of the small-document create regression and not all of it. The change
feed, path events and folder totals are the untested remainder — the next step is a probe
that switches each off in turn, which `textdb-probe writepath` is the right place for.

## Link rewriting on a move is within 1.45x of its floor (2026-09-15)

`link_updates = rewrite` keeps the corpus correct when a file moves: every document that
pointed at it is rewritten to point at the new path, one commit each, in the same
transaction as the move. It handles all four link kinds and keeps each one's syntax and
alias — `[x](./target.md)` -> `[x](./renamed.md)`, `[[target]]` -> `[[renamed]]`,
`![[target]]`, `![alt](./target.md)`.

It is the most expensive thing in the MD family, so it is worth saying what the cost *is*
rather than only that it is large. Measured on this host, one file moved with N documents
pointing at it:

| fan-in | total | per rewritten document |
|---|---|---|
| 50 | 39 ms | 780 us |
| 200 | 124 ms | 620 us |
| 500 | 334 ms | 668 us |

Linear in fan-in, flat per document — no quadratic blow-up from re-resolving the graph.

The floor is "one ordinary edit per affected document", because a versioned store cannot
change 500 documents without committing 500 versions. Measured on the same store, 500
ordinary `textdb_edit` calls in one transaction cost **230 ms, 460 us each**. So the link
rewrite runs at 668 us against a 460 us floor: **1.45x**, with about 208 us per document of
genuinely link-specific work (finding what points here, rewriting the target text,
re-resolving).

The consequence for tuning is the useful part: two thirds of this is ordinary commit cost,
so the lever is the per-commit fixed cost recorded above, not the link logic. Rewriting the
link code could at best recover the 208 us.

## Sync rewrote its whole base on every run, including a no-op (2026-09-15)

SY-01 put a number on sync for the first time, and the no-op case — the one a save hook or
a watcher runs constantly — cost 6.4 % of a full import.

The change detection itself is already right: `t_changed` compares versions and `d_changed`
compares size and mtime (with git's racy-clean window handled), so an unchanged file is
never read. What cost the time was downstream. `save_sync_base` ended every run with
`DELETE FROM sync_file WHERE sync_id = ?` followed by an `INSERT` per file, so a sync whose
answer was "nothing to do" still wrote the entire base back — hundreds of statements and
their WAL records.

It now reads the stored rows and skips the rewrite when they already say the same thing: one
indexed scan against two statements per file. Measured directly on 300 files:

| corpus | before | after |
|---|---|---|
| 300 files, 1 MB total | 15 ms (52 us/file) | 13 ms (45 us/file) |
| 300 files, 16 MB total | 25 ms (84 us/file) | 13 ms (46 us/file) |

The interesting part is the second row. A no-op sync used to get slower as the *content*
grew, which looked like it was re-reading documents; it was not — the bigger store simply
made the pointless rewrite more expensive. The cost is now flat in content size, which is
what a no-op should be. Both store backends got the fix; 131 workspace tests, 36 CLI tests
and 8 Postgres CLI tests pass.

**The SY family at `--size s` does not show this**, and that is worth knowing before reading
its numbers: 120 files of about 1.2 KB is too small a corpus for the rewrite to dominate, so
the cell moves 0.97x, inside the noise. The effect needs either more files or more bytes
than the POC parameters use.

Left alone: ~45 us per file per no-op sync, which is a `metadata` syscall, a store row
lookup and the map work. For a 10,000-file vault that is about half a second to establish
that nothing changed.

## Why the matrix alone was misleading

Three things the published `2026-09-12` entry concluded do not survive decomposition.

**`read` was never uniformly 1.62x — the gap was fixed cost per call, not cost per byte.**
Measured per document size it was 2.9x at 8 KiB, 0.85x at 100 KiB and 1.3x at 1 MiB. At
100 KiB textdb already won. The matrix averages those into one figure, and the million-call
block that dominates the run comes from the CR/CW/SR families, which use 8 KiB documents —
so the published number was mostly a measurement of per-call overhead. `create` told the
same story: 2.3x at 8 KiB, 1.7x at 100 KiB, 1.0x at 1 MiB. **Always read a ratio here next
to the document size it came from.**

**The statement-compilation hypothesis was right and bigger than it read.** It was recorded
as "the remaining measurable overhead" with the fix reverted. At 8 KiB, compiling the
virtual table's node lookup cost 9.7 us against 2.1 us from the cache — 82% of the whole
read gap.

**Nothing published separated the markdown structure sidecar, and every document the matrix
generates is `.md`.** `record_commit` extracts sections, links and frontmatter for
`.md`/`.markdown` only, so writing the same body as `.txt` isolates it: a one-line replace
in a 1 MiB document cost 21.5 ms as `.md` against 11.0 ms as `.txt`. Half the write was
derived-index maintenance, inside every published write number.

---

## Landed

Measured with `textdb-probe`, before and after, on one host back to back. Artefacts for the
matrix pair are in [`results/2026-09-13-s-linux/`](results/2026-09-13-s-linux/).

### 1. `myers` sized its working arrays by the input, not by the distance bound

The one that was also a crash. `myers` allocated its furthest-reaching array at
`2 * (n + m) + 3` and pushed a full-width clone of it into the trace once per diagonal,
where only diagonals in `[-d, d]` can have been reached — so memory and memory traffic were
`O(D * (N + M))` even when `D` was tiny next to the inputs. `diff_seq` trims the common
prefix and suffix first, so one contiguous edit stayed cheap; scattered edits defeat the
trim and paid the full width per diagonal.

`byte_edits` runs on every `UPDATE kb SET content = …`, the main write path through the
virtual table and the one the Python library and the CLI use, so an agent rewriting a large
page with many scattered changes could take the process down.

| document | changed lines | before | after |
|---|---|---|---|
| 1 MiB | 143 | 20 ms | 3.0 ms |
| 1 MiB | 569 | 1539 ms, +254 MiB RSS | 7.2 ms, no measurable RSS |
| 8 MiB | 1182 | 42 s, +6.9 GiB RSS | 41 ms |
| 8 MiB | ~5900 | **OOM-killed** | 116 ms (bounded fallback) |

The `max_d` fallback never saved it: it fires only *after* `max_d` full-width rows have been
allocated. Banded, the trace is `O(D^2)` whatever the document size — 134 MB at the
`max_d = 4096` ceiling, independent of input length. Hunks are unchanged.

### 2. Subtree queries scanned instead of seeking

Every prefix test was a function of the column — `substr(path, 1, length(?1) + 1) = ?1 || '/'`
in SQLite, `left(path, length($1) + 1) = $1 || '/'` in Postgres — so no index could serve it.
55x–143x against an equivalent range over 2000 files and at path depth 1000, and 62x–159x
for the same listing through the virtual table, which now accepts bounds on `path` in
`best_index`.

Two Postgres bugs fell out of the same review: `p || '/%'` treats a path's own `%` and `_`
as wildcards, so `kb.folder.nbytes_total` for `/100%_done` counted `/100XXXdone`'s files, and
`kb.search('y', '/100%_done')` returned a document from `/100XXXdone` — a search scoped to a
folder did not actually scope it.

### 3. The virtual tables recompiled their statements on every call

`KbCursor::filter` called `prepare` per read, and `FnCursor::filter` built a whole
`Connection` per call and handed it to `TextDb`, whose every statement goes through
`prepare_cached` — so `textdb_search` recompiled all of its statements every time and the
cache never hit at all. Both tables now hold the handle themselves. 8 KiB read through `kb`:
14.1 us → 7.1 us.

This is the optimisation that was previously reverted for leaving the database file
unreleasable. What sank it was *where* the cache lived. `Connection::from_handle` marks the
handle not owned, so dropping it never calls `sqlite3_close`; and the cached statements are
finalized when the table struct is dropped, which is `xDisconnect` — and `sqlite3_close`
calls `disconnectAllVtab` before it checks for unfinalized statements, with a comment in
SQLite's own source saying why: "the v-table implementation may be storing some prepared
statements internally".

### 4. The scalar functions could not reuse a statement cache at all

A scalar SQL function gets a fresh `Connection` per call, and cannot safely keep one: a
`Connection` captured by a function closure is released only *after* `sqlite3_close` checks
for unfinalized statements, so its cached statements would make `close` return SQLITE_BUSY.
`storage::shared` is a thread-local registry keyed by SQLite handle — the virtual tables
register when they connect and release when they disconnect, and the scalar functions borrow
the handle when one is registered, falling back to a per-call handle when none is.

`textdb_content(path, 1)` on an 8 KiB document: 29.5 us → 9.9 us. `read_shared` and
`read_version_shared` also hand back the cached buffer and the UTF-8 flag computed with it,
so the functions stop re-validating a megabyte to learn what the cache already recorded and
stop copying the document an extra time on the way out.

### 5. The markdown sidecar rebuilt what the write already knew

It materialised the document from the root it had just built — a full tree walk and
concatenation of chunks written moments earlier — then cloned the result to pass a slice.
Writes that hold the whole new content now call `remember_document`. Structure rows were
deleted and re-inserted one statement per row on every commit, 339 inserts for a 1 MiB
document whose headings a one-line body edit had not touched; they are now compared first
and rewritten only on a difference, and a rewrite batches its inserts.

### Net effect on the probe, per document size

`.md` against the plain-text baseline, lower is better:

| operation | 8 KiB | 100 KiB | 1 MiB |
|---|---|---|---|
| read (warm) | 2.87x → **1.15x** | 0.84x → **0.63x** | 1.30x → **1.23x** |
| read_version | 3.83x → **0.94x** | 1.54x → **0.61x** | 1.46x → **1.25x** |
| replace (one line) | 1.64x → 1.77x* | 0.56x → **0.45x** | 0.37x → **0.34x** |
| create | 2.30x → 2.38x* | 1.72x → **1.47x** | 1.68x → **1.57x** |

\* The 8 KiB write cells moved the wrong way in this pair, which was taken while another
matrix run had the machine; an earlier quiet pair had 8 KiB replace at 1.55x → 1.23x and
create at 2.16x → 1.92x. Neither pair is decisive for a sub-millisecond cell on a 4-CPU
container — see the caveat at the top.

---

## What is left, ranked

### 1. `search` — 11.4x against the plain-text baseline, the largest gap remaining

Untouched by this pass: the statement-cache fix helped its compilation but its p50 is 19.5 ms,
dominated by the FTS query and the per-candidate work. Two hypotheses are already tested and
rejected (below); three have not been tried.

- **The `limit * 50` hit window is fixed.** For `limit = 20` that ranks and joins up to 1000
  chunk hits per term to produce at most 20 files. Start at `limit * 4` and widen only when
  the intersection comes up short — a pure win whenever a term matches many chunks in few
  files, which is the common case for a knowledge base.
- **Two statements per candidate.** Each surviving file costs a `node` lookup and a `chunk`
  lookup. The node columns could come from the FTS join that already joins `node`; only the
  chosen chunk's bytes need a second fetch.
- **`find_leaf` walks the tree per hit** to turn a chunk into a line number. `chunk_ref`
  already stores `(chunk_id, file_id, version)`; adding the chunk's line offset in that
  version would make it O(1). The complication: a chunk can occur twice in a document and
  the table is append-only, so a stored offset is only valid for the version that wrote it —
  and the current `find_leaf` call doubles as the "is this chunk still in HEAD" check, which
  anything replacing it has to keep.

### 2. The concurrent write path — `replace` 3.1x in the matrix, where the probe says 0.34x

The single-writer probe has `replace` beating the baseline at every size above 8 KiB, and the
matrix has it at 3.1x. The difference is the CW family: the baseline rejects a stale write
outright, where textdb diffs and rebases. Some of that is the feature working as designed.
The part that may not be:

`TextDb::tx` opens `BEGIN IMMEDIATE` and holds the write lock across the whole operation —
materialising the base document, `byte_edits`, chunking, every `chunk` and `fts` insert,
`chunk_ref`, the commit row and the structure sidecar. But chunk and node writes are
append-only, idempotent and content-addressed: they are safe to perform *outside* the
transaction that does the CAS. Only the `node.root` CAS, the commit row and `chunk_ref` need
the lock. A chunk written by an attempt that then loses its CAS becomes unreferenced garbage,
which `maintenance` already has to handle, and FTS rows for such chunks cannot cause a false
hit because `chunk_ref` — written under the lock — is what maps a chunk to a file.

**How to test it before building it:** `--filter CW-04,CW-06,CW-07 --backends
sql-text-sqlite,textdb-sqlite` at `--size s`, at N=20 and N=50. If the gap is lock-hold time,
per-op write latency should fall with N. If it does not move, this candidate is wrong and the
cost is per-operation work, which the probe is the better tool for.

### 3. The CommonMark parse in the write path — about 4.7 ms per MiB

What is left of the sidecar after candidate 5 above. Structure rows are derived and
recomputable by [ADR 0007](../docs/decisions/0007-structure-rows-head-only.md), so the parse
could move out of the commit transaction entirely — rebuilt lazily on the first structural
query, or in `maintenance`. That is a semantic change (a `textdb_section` call straight after
a write would have to trigger the rebuild), so it needs a decision rather than a patch.
Making the parse incremental is the other option and the harder one: a heading's
`line_from`/`line_to` shift whenever the line count before it changes, so any edit that adds
or removes a line changes most rows regardless of what it touched.

### 4. Per-chunk SQL on the write path

A 1 MiB document is ~650 chunks:

| | per MiB |
|---|---|
| chunk + hash + tree build (no SQL) | 2.4 ms |
| `chunk` inserts | 3.1–4.1 ms |
| `fts` inserts | **11.6–12.0 ms** |
| `chunk_ref` (hash → rowid, then insert) | 2.2–2.3 ms |

FTS at chunk granularity is the largest item and a deliberate design choice — it is what
makes the index insert-only and shareable. Worth asking whether it has to be synchronous:
chunk rows are append-only, so indexing could defer to `maintenance` with search falling
back to a scan for not-yet-indexed chunks. That is a semantic change and needs the
recall/precision checks to confirm it is invisible.

`chunk_ref` resolves each hash back to a rowid with one statement per chunk, when `put_chunk`
already saw `last_insert_rowid()` for every chunk it inserted. Thread the ids out of
`put_chunk`, or key `chunk_ref` by hash (it is `WITHOUT ROWID` already) and drop the lookup;
the second costs 24 bytes a row, which FP-01/02 can price. `put_chunk` also recounts newlines
that `build_with_chunks` already counted for the same bytes, and copies every chunk into the
cache on write even when the chunk already existed.

### 5. Deep paths cost a round trip per component

`ensure_folder` walks up one lookup at a time and inserts one row per missing component:
~17 ms to create a file at depth 1000, against 224 us for the baseline, which stores no
folder rows at all. Both loops are iterative since the stack-overflow fix, so this is a
round-trip count, not a correctness risk. One recursive CTE to find the deepest existing
ancestor plus one batched insert would collapse it. Low value — NS-02 is a one-cell
pathological case — but cheap, and it is the worst ratio in the matrix, which invites
misreading.

---

## Tried and rejected — do not pay for these twice

- **Batching search hit resolution through `json_each`** (previous pass): worse. The
  table-valued scan costs more than the few queries it saves when a query matches few files.
- **Moving `ORDER BY rank` out of SQL into Rust** (previous pass): worse.
- **Folding `read_version`'s two lookups into one statement**: ten times slower.
  `{p}node_path` is a partial index over live rows only, and a historical read has to find
  tombstoned files too, so expressing "live row first, else the most recently deleted" as an
  `ORDER BY` loses the index. Over 300 files: two statements 3.6 us, correlated subquery
  17.4 us (scans `node`), join 39.4 us (scans `commit`, whose primary key starts at `file_id`
  and cannot serve a filter on `version` alone).
- **Sharing tree nodes behind an `Arc` instead of cloning them.** `Storage::get_node` returns
  `Node` by value and `tree::Cursor` holds nodes by value, so cloning a cursor deep-copies its
  path — `apply_one` does that once per suffix leaf in the re-chunk window. Measured, there is
  nothing there: a document has far more leaves than internal nodes (4698 against 154 at
  8 MiB), and the edit path is flat in document size — one 10-byte edit costs 21 us at 1 MiB,
  25 us at 8 MiB and 30 us at 32 MiB. Cost that does not grow with the file is not cost spent
  walking the file's tree. The `tree` probe keeps the measurement.
