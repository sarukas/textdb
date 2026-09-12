# ADR 0002 — Changed runs are refined to line level before rebase

Status: accepted (Stage 1b)

## Context

`chunk_diff(R₀, Rcur)` is chunk-granular. Two agents editing different lines of the same
≈1 KB chunk would be treated as overlapping and go through diff3; with a coarse region the
conflict payload also carried the whole chunk.

## Decision

After the tree walk, each changed leaf run is refined with the same line Myers + byte trim
used for `UPDATE … SET content`, producing line-precise runs. Rebase therefore shifts edits
that are disjoint at line level and only calls diff3 when lines genuinely overlap.

## Consequences

- Concurrent edits inside one chunk rebase (`CommitKind::Rebased`) instead of merging.
- Conflict payloads carry only the overlapping lines.
- Cost is proportional to the changed chunks, not the document.
