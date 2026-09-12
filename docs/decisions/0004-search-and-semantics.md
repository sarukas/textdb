# ADR 0004 — Search: document-level AND over a chunk index

Status: accepted (Stage 1c / 3a)

## Context

The FTS index is over chunks (claim 3). A naive `a AND b` query against it only finds
chunks containing both terms, while the baselines (`rg`, whole-document tsvector, FTS5 on
`body`) match documents containing both terms anywhere. Recall would be incomparable.

## Decision

Multi-term queries are evaluated per term against the chunk index; chunk hits are mapped
to live files through `chunk_ref`, the per-term file sets are intersected, and the hit line
is located from the first term's chunk via the tree. Phrases are single FTS terms
(`"a b"` → FTS5 phrase / `'a' <-> 'b'`), prefixes use `term*` / `'term':*`.

`chunk_ref` is append-only; a chunk that has left a file is detected by scanning HEAD's
leaves (O(n / 1 KB)) and the hit falls back to a HEAD scan. A future GC step would prune
`chunk_ref`.

## Consequences

- Same query semantics on every backend, so recall/precision are comparable.
- Cost grows with the number of chunk hits per term, not with document size.
