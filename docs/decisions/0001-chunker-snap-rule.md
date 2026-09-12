# ADR 0001 — Newline snap must not look past the chunk it decides

Status: accepted (Stage 1a)

## Context

Spec §6.1 snaps a CDC boundary forward to the next `\n` within 256 bytes. That makes the
cut of a chunk depend on up to 256 bytes *after* its boundary. The edit algorithm (§6.5)
re-chunks from "the leaf containing `byte_from`"; with the naive snap rule a leaf ending
just before an edit could have been cut based on bytes the edit changed, so P2 (history
independence) failed on random inputs.

## Decision

1. A boundary that already follows a `\n` is never snapped. Consequence: a chunk ending in
   `\n` was cut using only its own bytes; a chunk ending in any other byte was cut knowing
   that the following 256 bytes contain no `\n`.
2. The edit window starts at the leaf containing `byte_from`, and additionally includes the
   previous leaf when (a) the edit begins within 256 bytes of that leaf boundary and (b) the
   previous leaf does not end in `\n`.

## Consequences

- P2 holds on 3 000+ random cases including binary content.
- Typical text edits rewrite one chunk (≈ 1 KB) plus at most one resynchronisation chunk.
- Mask bits were tuned (`bits`, `bits-2`) so the mean chunk on text is ≈ 1.1 KB, matching
  the nominal `avg = 1024`.
