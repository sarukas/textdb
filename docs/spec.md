# textdb — Proof-of-Concept Specification

Version 0.1 · Draft for review · 2026-09-12 · Source: [issue #2](https://github.com/sarukas/textdb/issues/2)

> This file mirrors the specification in issue #2. Implementation notes and deviations
> decided during the POC are recorded in [`decisions/`](decisions/) and in
> [`../bench/RESULTS.md`](../bench/RESULTS.md).

---

## 1. Purpose

Build a storage and query layer that lets many humans and AI agents read, search, and edit a knowledge base of 10k–50k markdown documents concurrently, inside an SQL database, with full version history and no git push/pull. The POC must prove or disprove four claims:

1. Content-defined chunking plus a Merkle tree gives O(edit)-cost writes and near-free versioning for text documents.
2. Compare-and-swap on document content, with automatic rebase at chunk granularity, yields a conflict rate for concurrent agent edits that is an order of magnitude lower than whole-file optimistic locking.
3. An append-only chunk store makes full-text indexing insert-only, removing index churn on edits.
4. The whole surface — folders, files, content, history, search, edits — is expressible as ordinary SQL against views and functions.

## 2. Scope

**In scope for the POC**

- Engine-agnostic core library (`textdb-core`): chunker, tree, hashing, materialize, locate, edit, rebase, diff.
- SQLite binding as a virtual table module (`textdb-sqlite`) — algorithm validation.
- Postgres binding as a pgrx extension (`textdb-pg`) — concurrency validation and the SQL/OO surface.
- Dolt harness (`bench/dolt`) — independent check of the merge thesis.
- Markdown structure extractor (headings, wikilinks, frontmatter) as a plugin to core.
- Concurrency and performance harness with two baselines.

**Out of scope**

- Character-level CRDTs / live co-editing.
- Vector search or embeddings.
- Branching UI or pull-request workflow (branches are structurally supported; no tooling).
- Access control beyond a note on Postgres RLS.
- MCP server (thin wrapper, trivial once the SQL surface exists).
- Managed-Postgres (PL/pgSQL-only) port. Decision deferred to after POC results.

## 3. Assumptions

| # | Assumption | If wrong |
|---|---|---|
| A1 | Edits are batchy: an agent or human commits a coherent change, not keystrokes | Add a CRDT layer on top; core design unchanged |
| A2 | Documents are 1 KB – 1 MB; corpus 10k–50k files, ≤ 5 GB raw | Above 5M nodes, revisit path materialization |
| A3 | Content is UTF-8 text with `\n` line endings; core treats content as bytes and never normalizes | `\r\n` files round-trip exactly but line counts follow `\n` |
| A4 | Byte-exact round-trip to files is mandatory | — |
| A5 | Every version is retained; GC is by reference count and out of POC scope beyond a stub | — |
| A6 | Self-hosted Postgres for the POC (extension loading allowed) | PL/pgSQL port of core hot paths |

## 4. Architecture

```
┌──────────────────────────────────────────────────────────────┐
│  SQL surface: views, INSTEAD OF triggers, functions          │  textdb-pg / textdb-sqlite
├──────────────────────────────────────────────────────────────┤
│  Engine binding: maps Storage trait to tables; FTS glue      │
├──────────────────────────────────────────────────────────────┤
│  textdb-core (Rust, no engine deps, C ABI exported)          │
│   chunker · tree · hash · materialize · locate · edit ·      │
│   rebase · diff · structure-extractor plugin trait           │
└──────────────────────────────────────────────────────────────┘
```

Core owns every algorithm and is compiled once. Bindings own persistence and SQL grammar and contain no algorithmic logic. Core exposes a `Storage` trait that bindings implement:

```rust
pub trait Storage {
    fn get_chunk(&self, h: &Hash) -> Result<Option<Bytes>>;
    fn put_chunk(&mut self, h: &Hash, b: &[u8]) -> Result<()>;   // idempotent
    fn get_node(&self, h: &Hash) -> Result<Option<Node>>;
    fn put_node(&mut self, h: &Hash, n: &Node) -> Result<()>;    // idempotent
    fn get_root(&self, file_id: u64) -> Result<Option<(Hash, u64 /*version*/)>>;
    fn cas_root(&mut self, file_id: u64, expect: Option<&Hash>, new: &Hash) -> Result<bool>;
}
```

All chunk and node writes are append-only and idempotent; the only mutation in the system is `cas_root`.

## 5. Data model

### 5.1 Content layer (engine-agnostic)

| Object | Fields | Identity |
|---|---|---|
| Chunk | `bytes`, `nlines` | BLAKE3-256 of bytes |
| Node | `children: [(hash, nbytes, nlines, is_leaf)]` | BLAKE3-256 of canonical child encoding |
| Root | a Node hash | — |

Hash: BLAKE3, 32 bytes. Chosen over SHA-256 for throughput (≈ 5–10× on modern CPUs); tamper-evidence is not a POC goal, so cryptographic strength is not the selection criterion. Swappable behind a type alias.

### 5.2 Namespace layer (per engine, same schema)

```
node (
  id          bigint  PK,
  parent_id   bigint  NULL REFERENCES node(id),
  name        text    NOT NULL,             -- one path segment, any UTF-8 except '/'
  kind        smallint NOT NULL,            -- 0 folder, 1 file
  path        text    NOT NULL UNIQUE,      -- materialized '/a/b/c.md'
  root        bytea   NULL,                 -- Merkle root (files)
  version     bigint  NOT NULL DEFAULT 0,   -- CAS token, monotonic per file
  nbytes      bigint, nlines bigint,
  created_at, updated_at, updated_by,
  deleted_at  timestamptz NULL,             -- tombstone
  UNIQUE (parent_id, name)
)
commit (
  file_id bigint, version bigint, root bytea, parent_root bytea,
  author text, ts timestamptz, message text,
  PRIMARY KEY (file_id, version)
)
chunk (hash bytea PK, bytes bytea, nlines int)         -- + FTS index, engine-specific
tree_node (hash bytea PK, children bytea)              -- canonical encoding
chunk_ref (hash bytea, file_id bigint, version bigint) -- reverse index for search results
section (file_id, version, heading_path text, level int, line_from int, line_to int)
link    (file_id, version, target_path text, line int)
frontmatter (file_id, version, data jsonb)
```

Path index: btree with `text_pattern_ops` (Postgres) / plain btree (SQLite) supporting `LIKE '/prefix/%'`. `ltree` is explicitly rejected (label alphabet excludes dots and spaces).

## 6. Algorithms

### 6.1 Chunker

FastCDC with gear hash. Parameters (POC defaults, all tunable):

| Parameter | Value | Rationale |
|---|---|---|
| min | 512 B | Bounds tree size |
| avg | 1 024 B | Concurrency granularity ≈ paragraph–section |
| max | 4 096 B | Bounds re-chunk region on edit |
| boundary snap | forward to next `\n` within 256 B, else cut at CDC boundary | Chunks align to lines so line counts are exact per chunk and snippets are readable |

Normalization: none. Chunk boundaries are a function of bytes only, so identical content always yields identical chunks (determinism requirement D1).

### 6.2 Tree (prolly tree)

- Level 0: sequence of chunk hashes.
- Level k+1: boundaries over level-k entries determined by a rolling hash of the entry hashes (probability 1/32 → mean fanout 32). Deterministic: identical child sequence → identical parent split (D2).
- Each entry carries cumulative `nbytes` and `nlines` so descent by byte or line is O(log n) without visiting leaves.
- Consequence of D1 + D2: two documents with identical content have identical roots regardless of edit history. Structural sharing across versions and across documents is automatic.

### 6.3 Materialize

`materialize(root) -> Bytes`: in-order leaf traversal, concatenation. O(n). Must be byte-identical to the original input (property test P1).

### 6.4 Locate

`locate_byte(root, off) -> (leaf_hash, off_in_leaf)`, `locate_line(root, line) -> byte_off`. Descent using cumulative counts. O(log n).

### 6.5 Edit

Input: root R, edit set E = [(byte_from, byte_to, replacement_bytes)], non-overlapping, ascending.

1. For each edit, take the leaf containing `byte_from` and the leaf containing `byte_to`; the re-chunk window starts at the first leaf's start.
2. Stream bytes: window prefix + replacement + suffix from `byte_to` onward, running the chunker. Stop when a produced boundary coincides with a pre-existing boundary at the same suffix offset (resynchronization; guaranteed within `max` bytes after the last affected old leaf in the common case; hard cap: end of document).
3. Replace the affected leaf run with the new chunk hashes; rebuild ancestors under D2 to the root.

Output: new root R′, the set of new chunk hashes (for FTS insertion), and the changed byte range in R′ coordinates.

### 6.6 Commit with rebase

Caller holds base root R₀, submits edit E producing R₁.

```
loop:
  (Rcur, v) = get_root(file)
  if Rcur == R₀:
      if cas_root(file, R₀, R₁): return Ok(v+1)
      else continue                          # lost race, re-read
  D = chunk_diff(R₀, Rcur)                   # changed byte ranges in R₀ coords
  if disjoint(E.ranges ⊕ chunk_padding, D):
      E' = shift(E, D)                       # translate offsets past D's deltas
      R₁' = edit(Rcur, E')
      if cas_root(file, Rcur, R₁'): return Ok(v+1)
      else continue
  else:
      region = union(overlapping ranges), expanded to line boundaries
      m = diff3(base=slice(R₀,region), ours=slice(R₁,region), theirs=slice(Rcur,region))
      if m.clean: apply m to Rcur → R₁''; cas; return Ok
      else: return Conflict { path, region_lines, base, theirs, ours }
```

Bounded retries (default 8) before surfacing `Contention` distinct from `Conflict`. The conflict payload contains the *current* text of the region so an agent can retry without another read.

### 6.7 Diff

`diff(R_a, R_b)`: walk both trees, skip identical subtree hashes, emit changed leaf runs; convert to unified diff at line granularity using the leaf line counts. O(changes · log n).

### 6.8 Structure extractor (plugin)

Trait `StructureExtractor { fn extract(&self, bytes) -> Structure }` producing sections (heading path, level, line span), links, frontmatter. Markdown implementation uses `pulldown-cmark` with byte-offset tracking. Runs on the full materialized document after each commit (O(n); not worth making incremental). Core is format-agnostic; only this plugin knows markdown.

## 7. Engine bindings

### 7.1 SQLite (`textdb-sqlite`) — Stage 1

- Loadable extension via `rusqlite` `vtab` module.
- `CREATE VIRTUAL TABLE kb USING textdb(store='kb_')` creates shadow tables `kb_node`, `kb_chunk`, `kb_tree_node`, `kb_commit`, `kb_section`, `kb_link`, and `kb_fts` (FTS5, external-content on `kb_chunk`).
- Virtual table columns: `id, path, name, parent_path, kind, content, version, nbytes, nlines, updated_at`.
- `xUpdate` implements INSERT (create, `mkdir -p`), UPDATE of `content` (diff OLD/NEW → edit set → commit with rebase), UPDATE of `path` (move), DELETE (tombstone).
- Table-valued functions: `textdb_ls(path)`, `textdb_search(query, prefix)`, `textdb_history(path)`, `textdb_lines(path, from, to)`, `textdb_section(path, heading)`, `textdb_diff(path, v1, v2)`, `textdb_content(path, version)`.
- Single writer per connection is accepted; this stage validates algorithms, not concurrency.

### 7.2 Postgres (`textdb-pg`) — Stage 3

pgrx extension. Schema `kb`.

Views (updatable via `INSTEAD OF` triggers):

```
kb.folder       (id, path, name, parent_path, n_children, nbytes_total, updated_at)
kb.file         (id, path, name, parent_path, content, version, nbytes, nlines,
                 frontmatter, updated_at, updated_by)
kb.file_version (id, path, version, content, parent_version, author, ts, message)
```

Functions (attribute notation where single-arg):

```
content(kb.file) → text                 -- f.content (view column, materialize)
lines(kb.file, int, int) → text
section(kb.file, text) → text
edit(kb.file, old text, new text) → bigint   -- strict: old must be unique; raises on conflict
append(kb.file, text) → bigint
diff(kb.file, bigint, bigint) → text
kb.ls(path) → TABLE(name, kind, nbytes, nlines, updated_at)
kb.search(tsquery text, prefix text) → TABLE(path, line, snippet, rank)
kb.history(path) → TABLE(version, author, ts, message)
kb.content(path, version) → text
kb.export(prefix) → TABLE(path, content)
```

DML semantics:

| Statement | Behaviour |
|---|---|
| `INSERT INTO kb.file(path, content)` | Create parents, chunk, commit v1 |
| `UPDATE kb.file SET content = …` | Trigger computes byte-level diff OLD→NEW (Myers on lines, then bytes within changed lines) → edit set → §6.6 commit with rebase |
| `UPDATE kb.file SET path = …` | Rename/move; subtree path rewrite in one transaction; `id` stable |
| `UPDATE kb.folder SET path = …` | Same, recursive |
| `DELETE FROM kb.file / kb.folder` | Tombstone; content retained |
| `INSERT … ON CONFLICT (path) DO UPDATE SET content = EXCLUDED.content` | Re-import; identical content → no new version (root unchanged) |

FTS: `chunk.tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', convert_from(bytes,'UTF8'))) STORED`, GIN index. Insert-only by construction. Search maps chunk hits → `chunk_ref` → current versions → line offset via `locate`.

Errors: `Conflict` raised as SQLSTATE `40001`-class custom code `TX001` with `DETAIL` carrying the JSON conflict payload; `Contention` as `TX002`.

Isolation: READ COMMITTED is sufficient; correctness rests on `cas_root` (`UPDATE node SET root=$2, version=version+1 WHERE id=$1 AND root=$3`), not on snapshot isolation.

### 7.3 Dolt harness (`bench/dolt`) — Stage 2

Independent of core except the chunker. Schema `chunks(file_id, ordinal_key varchar, hash, text)` with fractional ordinal keys. Each simulated agent works on its own branch and merges to main. Measures conflicts detected by Dolt's cell-level merge for the same workload used in §9.3. Purpose: external evidence for claim 2 before the Postgres binding exists.

## 8. Concurrency model (normative)

1. The only mutable state per file is `(root, version)`.
2. A write succeeds iff its CAS succeeds; every success appends exactly one `commit` row.
3. Two writes on the same file whose re-chunk windows are disjoint in base coordinates both succeed without caller involvement.
4. Two writes whose windows overlap succeed iff line-level diff3 is clean; otherwise the later one receives `Conflict` with the current region text.
5. Folder move and file content edit on a file under that folder never conflict (path and root are independent columns).
6. Cross-file atomicity is the enclosing SQL transaction; a `kb.checkpoint(name)` function recording all current roots under a prefix is provided for named restore points.

## 9. Test and benchmark plan

### 9.1 Property tests (core)

| ID | Property |
|---|---|
| P1 | `materialize(build(bytes)) == bytes` for arbitrary bytes incl. empty, no trailing newline, `\r\n`, invalid UTF-8 |
| P2 | `build(bytes)` root is independent of construction path (build vs. any sequence of edits producing the same bytes) |
| P3 | `edit` changes ≤ ⌈len(replacement)/min⌉ + 3 leaves for a single contiguous edit |
| P4 | `locate_line(root, k)` equals the offset of the k-th `\n` + 1 |
| P5 | `diff(a,b)` applied to `materialize(a)` yields `materialize(b)` |
| P6 | Rebase of disjoint edits is commutative: `commit(E1) then commit(E2)` ≡ `commit(E2) then commit(E1)` |

### 9.2 Performance targets (Postgres, 50k files, 2 GB raw, commodity 8-core)

| Metric | Target | Baseline A (whole-file TEXT + OCC) | Baseline B (files + git) |
|---|---|---|---|
| Edit commit, 100 KB file, 3-line change, p99 | ≤ 5 ms | measure | measure (`sed -i` + `git commit`) |
| `content` read, 100 KB, p99 | ≤ 1 ms | measure | measure |
| `search`, p95, 2-term AND | ≤ 50 ms | measure | `git grep` |
| Storage after 100 edits/file | ≤ 1.3 × raw (excl. FTS) | measure | measure (`.git` size) |
| FTS index write per edit | O(new chunks) | full re-index of row | n/a |
| Bulk import 50k files | ≤ 10 min | measure | measure |

### 9.3 Concurrency harness

Workload generator: N ∈ {1, 5, 20, 50} simulated agents; each loop: pick a file (Zipf over corpus, s = 1.0 so hot files exist), pick a section, read it, apply a 1–10 line edit, commit; think time 0–200 ms. Run 10 minutes per N.

Report per N: throughput, p99 latency, fraction of commits that (a) succeeded direct, (b) succeeded via rebase, (c) returned `Conflict`, (d) returned `Contention`. Same workload against Baseline A (conflict = version mismatch) and the Dolt harness.

Success criterion for claim 2: at N = 20 on the hot file, `Conflict` rate ≤ 1/10 of Baseline A's version-mismatch rate.

### 9.4 Round-trip

Import a real repository of ≥ 10k markdown files, export, `diff -r` must be empty. Import again; `SELECT count(*) FROM kb.commit WHERE version > 1` must be 0.

## 10. Milestones

| Stage | Deliverable | Acceptance | Effort estimate |
|---|---|---|---|
| 0 | Repo scaffold, CI, core crate skeleton, `Storage` in-memory impl | Builds on Linux/macOS | 2 days |
| 1a | Chunker, tree, materialize, locate, edit, diff | P1–P5 pass on 10k random cases | 1 week |
| 1b | Rebase/commit, markdown extractor | P6 pass; extractor matches reference parser on test corpus | 1 week |
| 1c | SQLite virtual table + FTS5 | §9.4 round-trip on real corpus via SQLite | 1 week |
| 2 | Dolt harness + §9.3 workload generator | Conflict numbers for Dolt and Baseline A | 3 days |
| 3a | pgrx schema, views, triggers, functions | All §7.2 statements work; §9.4 via Postgres | 2 weeks |
| 3b | Benchmarks §9.2, §9.3 against both baselines | Report | 1 week |
| — | Decision review | Go / no-go on production build; managed-PG question | — |

## 11. Repository layout

```
textdb/
  crates/
    textdb-core/        # algorithms, Storage trait
    textdb-md/          # markdown StructureExtractor
    textdb-sqlite/      # rusqlite vtab extension
    textdb-pg/          # pgrx extension
  bench/
    harness/            # workload generator, metrics, all backends (incl. baselines)
    scripts/            # Postgres cluster start script
    dolt/               # Dolt schema + runner (N/A in this environment, see ../bench/RESULTS.md)
  corpora/              # import scripts for test repositories
  docs/
    spec.md             # this file
    test-suite.md       # companion test specification (issue #1)
    ../bench/RESULTS.md # measured results
    decisions/          # ADRs
```

## 12. Open decisions (to be closed during Stage 1, by measurement)

| # | Question | Default | Decide by |
|---|---|---|---|
| O1 | Chunk `avg` 1 KB vs 2 KB | 1 KB | Conflict rate vs. tree depth and `chunk_ref` size at 50k files |
| O2 | Boundary snap: `\n` vs blank line (`\n\n`) | `\n` | Snippet readability vs. chunk size variance |
| O3 | `UPDATE … SET content` diff algorithm: line Myers only vs. line+byte refinement | line+byte | Edit-set size on typical agent edits |
| O4 | FTS dictionary: `simple` vs. language stemming | `simple` | Recall on test queries; multilingual corpus |
| O5 | Store `frontmatter` per version or only HEAD | per version | Storage cost |
| O6 | GC trigger policy | none in POC | — |
| O7 | Managed-Postgres route: PL/pgSQL port vs. application service | deferred | Stage 3 results and deployment target |

## 13. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| CDC boundaries drift on small edits more than expected (P3 fails) | Write amplification, more conflicts | Tune `min`/`max`; fall back to fixed line-based snapping |
| Byte-level diff in trigger dominates edit latency | Miss 5 ms target | Restrict to line-level diff; agents use `edit(f, old, new)` |
| pgrx build/version friction | Schedule | Pin pgrx and PG16; CI matrix |
| Postgres GIN pending-list growth under insert load | Search latency spikes | `fastupdate = off` or periodic `gin_clean_pending_list` |
| Hot-file contention at N = 50 exceeds retry budget | `Contention` errors | Advisory lock fallback per file after k failed CAS |
