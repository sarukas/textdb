# ADR 0007 — Section / link / frontmatter rows are kept for HEAD only

Status: accepted (Stage 3b, closes open decision O5 by measurement)

## Context

Spec §6.8 runs the markdown extractor after each commit and §5.2 stores `section`, `link`
and `frontmatter` rows per `(file_id, version)`. Measured on the POC run (100 KiB markdown,
2 000 single-line edits, SQLite): footprint 233 × raw, and ~11 KB written per edit against
~1 KB of new chunk data — the ~50 section rows per version outweighed the content layer.
On a 100 MiB document with ~50 k headings each commit rewrote ~44 MB of structure rows.

## Decision

Structure rows describe HEAD only: on commit the file's previous rows are deleted and the
new ones inserted. Historical structure is recomputable from `kb.content(path, version)`.
The `version` column is retained so a per-version mode can be re-enabled later.

## Consequences

- Storage growth per edit returns to O(chunks touched) + one commit row.
- `section(path, heading)` always reflects HEAD, which is what the SQL surface exposes.
- XL/LL benchmark documents use `.txt` paths so they measure the content layer alone;
  RT/ME/CW/SR/NS/FP use `.md` and include extraction.
