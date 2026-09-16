# Outlines: markdown headings across a vault

Every markdown document's heading tree is parsed at commit and stored as rows, one per
heading, so "which notes have a *Next steps* section" is an index seek rather than a scan of
every document's text.

```markdown
# Quarterly Review
...
## Goals
...
### Detail
...
## Next Steps
```

becomes four rows. Each carries the **heading path** (`Quarterly Review / Goals / Detail`),
the heading's own last component, its **level**, its **line span**, and two word counts.

## The two word counts

`nwords` is the section's own lines. `nwords_total` is those plus everything nested under it.
A word never spans a line and sections partition a document by line, so both are exact and
they compose: a parent's total is its own words plus every descendant's own. That is what
makes "which sections of this plan are actually written" answerable — a heading with a large
total and a small own count is a stub with content underneath it, and one with both small is
a stub.

They are kept current on the same write that parses the headings. Because word counts move on
nearly every body edit while the heading tree stays put, a commit that leaves the tree alone
refreshes only the counts, in one statement, instead of rebuilding the rows.

## Asking

**CLI**

```sh
textdb outline /notes/plan.md               # one document's table of contents, indented
textdb outline /notes                       # everything under a folder, grouped by document
textdb outline                              # the whole store
textdb outline --heading "Next steps"       # who has this heading, ignoring case
textdb outline --heading next --match prefix
textdb outline --heading step --match contains
textdb outline --level 1                    # just the top of each document
textdb outline --names                      # the distinct headings in use, with counts
```

`exact` and `prefix` seek the folded index; `contains` is the one shape that has to scan.

**SQL** — `textdb_outline(path, heading, match, level, limit)` and `textdb_headings(path,
starts, limit)` on SQLite, `kb.outline(...)` and `kb.headings(...)` on Postgres, plus the
`sections` view:

```sql
SELECT path, heading, line_from FROM textdb_outline('/', 'Next steps');
SELECT heading, nwords_total FROM textdb_outline('/plan.md') ORDER BY nwords_total DESC;
SELECT * FROM textdb_headings('/', 'ne', 20);       -- autosuggest
```

**Python / Node**

```python
for h in corpus.outline("/notes", heading="next", match="prefix"):
    print(h.path, h.heading, h.nwords, h.nwords_total)
corpus.heading_names("/", "ne")
```

```ts
corpus.outline('/notes', { heading: 'next', match: 'prefix' });
corpus.headingNames('/', { starts: 'ne' });
```

**HTTP** — `/api/outline` and `/api/outline/names`.

Every row carries its document's `nbytes`, `nlines`, word count, version, and last change, so
a table can show them without a query per row.

## What it costs

Measured on a generated 2,000-note vault, headings distributed the way a vault's are:

| | SQLite | Postgres |
|---|---|---|
| one document's outline | 0.22 ms | 0.45 ms |
| a heading across the vault (`exact`) | 4.2 ms | 7.7 ms |
| the same as `prefix` (twice the rows) | 9.6 ms | 15.7 ms |
| `contains` (the shape that scans) | 9.0 ms | 15.8 ms |
| the whole vault, ~10,000 headings | 20.8 ms | 36.3 ms |
| autosuggest, 2-character prefix | 1.3 ms | 3.9 ms |

Two honest limits. **Postgres needs its statistics.** A store built in one burst keeps
whatever autovacuum worked out while it was nearly empty, and the same heading query then
plans as a nested loop and takes 74 ms instead of 7.7 ms — run `SELECT kb.analyze_store()`
after a bulk import. And **there is no notion of a title**: an H1 is a level-1 heading and a
front-matter `title` is a property, and nothing reconciles them, so "every document's title"
still means choosing a convention.
