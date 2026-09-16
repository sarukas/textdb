# Front-matter properties: asking across a vault

Every markdown document's YAML front matter is parsed at commit and indexed as rows, one per
(document, property path, value). That is what makes "which notes have `status: draft`" an
index seek rather than a scan of every document's JSON, and what lets a UI suggest property
names and values while you type.

```yaml
---
title: Quarterly Review
status: draft
tags: [cvm, telco, q3]
priority: 2
project:
  name: atlas
  phase: pilot
---
```

becomes eight rows. A **list is one row per element**, so "does `tags` contain `telco`" is an
ordinary equality rather than a search inside an array — which is why it is the *fastest*
shape to query rather than the slowest. A **nested object flattens to a dotted path**
(`project.name`), which is what people already type. A **number keeps a numeric form** as
well as its text, so `priority:2` and `priority:>1` both work and 10 sorts above 9. A
property that is present but **null is still recorded**, so `has:due` finds it: empty is a
real state in a vault and hiding it would be wrong.

## The query language

One grammar, parsed in `textdb-md`, compiled to SQL separately by each binding. A query means
the same thing against SQLite and against Postgres.

| Written | Means |
|---|---|
| `status:draft` | the property equals the value, ignoring case |
| `tags:telco` | a list contains it — lists are rows, so this is the same thing |
| `project.name:atlas` | nested properties are dotted |
| `title:"quarterly review"` | quote a value with spaces; a quoted value is never read as an operator |
| `priority:>3` `due:<=2026-10-01` | compare; numerically when both sides are numbers, as text otherwise (so ISO dates work) |
| `has:budget` or `budget:*` | the property exists at all, whatever it holds |
| `name:atl*` | starts with |
| `note:~telco` | contains |
| `status:!=draft` | **has** the property, but not with that value |
| `status:draft tags:telco` | a space means AND |
| `a:1 OR b:2` | explicit OR, which binds looser than AND |
| `-status:archived`, `NOT status:archived` | either spelling of NOT |
| `(a OR b) AND c` | parentheses regroup |
| *(empty)* | every document that has front matter |

`!=` is worth stating plainly: it means *has the property but not as that value*. A note with
no `status` at all is not a note whose status is not draft, and treating it as one would make
`status:draft` and `status:!=draft` overlap or leave gaps. They partition the corpus exactly.

## Asking

**CLI**

```sh
textdb meta keys                  # every property in use, most-used first
textdb meta keys pro              # names starting with "pro"
textdb meta values status         # what that property holds, and how often
textdb meta find "status:draft tags:telco" --show status,tags,priority
textdb meta find "priority:>3" --folder /notes --limit 50
```

`meta keys` returns `key, docs, values_n, kind` — how many documents carry each property, how many distinct values it takes,
and whether those values are numeric — the last so a reader knows whether `>` will mean
anything on it.

**SQL** — the `properties` view (path, key, value, number, ord) and the functions underneath. The view is a
`TEMP VIEW` the CLI creates on its own connection, so it exists inside `textdb sql` and not in the store
itself; a client connecting directly calls the functions:

```sql
SELECT * FROM textdb_prop_keys('', 20);              -- SQLite
SELECT * FROM textdb_prop_values('status', '', 50);
SELECT * FROM textdb_prop_find('status:draft', '/', 500);

SELECT * FROM kb.prop_keys('', 20);                  -- Postgres
SELECT * FROM kb.prop_values('status', '', 50);
SELECT * FROM kb.prop_find('status:draft', '/', 500);
```

**Python**

```python
c = Corpus.open("kb.db")
[k.key for k in c.property_keys("pro")]          # ['project.name', 'project.phase']
[v.value for v in c.property_values("status")]   # ['draft', 'review', ...]
for hit in c.property_find("status:draft tags:telco"):
    print(hit.path, hit.frontmatter["priority"])
```

**Node**

```ts
const corpus = openCorpus({ db: "kb.db" });
corpus.propertyKeys({ prefix: "pro" });
corpus.propertyValues("status");
corpus.propertyFind("status:draft tags:telco").map((h) => h.path);
```

Every one of these returns the document's whole front matter alongside the path, so a table
view can show any column without a query per cell.

**HTTP** — `/api/meta/keys`, `/api/meta/values?key=…`, `/api/meta/find?q=…`, each taking a
`prefix` or `folder` and a `limit`.

## In the web app

The folder view's **Properties** tab. The left rail is the vault's own schema — every property
with a document count, expandable to its values with counts, each clickable to add a term.
That is what makes it explorable rather than only searchable: you can find out what there is
to filter on without knowing in advance.

Two ways to ask, on one query:

- **Query** is a text box with Obsidian-style autosuggest. Type `sta` and it offers property
  names; `Tab` completes it and adds the colon, and the value list for *that* property opens
  straight away. Connectives are offered in the gaps between terms. Both lists come from the
  index, so a vault with thousands of values suggests from all of them.
- **Builder** is clause rows — not / property / comparison / value — for people who would
  rather not learn the syntax. It writes the query text, and shows it, so the builder is also
  how you learn what to type.

Switching between them adopts the query where it can. A query using `OR` or brackets is more
than clause rows can express, so the builder **says so and leaves it alone** rather than
quietly rewriting it on the first edit.

Results are a table whose columns default to the properties the query mentions — you filtered
on them, so you want to see them — and any other property can be toggled on.

## What it costs

Measured on a generated 5,000-note vault, and again at 50,000 by growing the table:

| | before (scan) | after (indexed) |
|---|---|---|
| `status:draft` | 37 ms | 2 ms |
| `tags` contains `telco` | 60 ms | 1 ms |
| every property name (autosuggest) | 67 ms | 0.11 ms |
| values of one property | 37 ms | 0.05 ms |
| name-prefix autosuggest | — | 0.04 ms |

The rows cost about 860 bytes per note with all three indexes — roughly 20 % against realistic
4 KB notes, more in a vault of stubs. They are maintained on the same write that parses the
front matter, and an existing store backfills them the first time it is opened by a build that
has the table, so a vault answers immediately rather than after a reindex.

Two honest limits. **Multi-property AND is the least improved**: two unselective properties
intersect in about 20 ms at 50,000 notes against 60 ms scanning — better, but not the order of
magnitude the single-property cases give. And the index answers questions about *values*, not
about text: full-text search is still `textdb search`.
