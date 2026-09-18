# What a delegated account costs

Two runs of the same matrix, same host, same seed, both at size `s`, the second after a round of
fixes the first one paid for. Each run compares an account against **its own owner in the same
invocation**, which is the comparison that matters: both columns are measured back to back on one
host, so the drift below never enters it.

| run | commit | what it is |
|---|---|---|
| A | `94fbeed`~1 | the first whole-matrix delegated run |
| B | `a98e4ec` | after the property, link and feed surfaces were fixed |

**The drift between the two runs**, from the backends nothing here touches: `fs` 0.896× geomean,
`sql-text-sqlite` 0.968×. Read a cross-run move under about 10% as the host.

## The account against its owner

| | cells | median | geomean | cells under 2 ms | over 2 ms |
|---|---|---|---|---|---|
| `textdb-sqlite@account`, run B | 281 | **1.043×** | **1.161×** | 1.20× | 1.08× |
| `textdb-pg@account`, run A | 316 | 1.654× | 2.162× | | |
| `textdb-pg@account`, run B | 316 | 1.660× | **2.025×** | 2.49× | 1.90× |

The whole-matrix geomean barely moved, and that is not the fixes failing — it is what a geomean
over 316 cells does when the wins are concentrated in a dozen of them. Per cell it is not subtle:

| test | what it measures | run A | run B |
|---|---|---|---|
| `MD-06` `feed_full` | a watcher catching up from zero | **172.2×** | **1.7×** |
| `MD-07` `prop_keys` | the property names in use | 23.0× | **1.0×** |
| `MD-07` `prop_values` | the values one property takes | 15.4× | **1.1×** |
| `MD-07` `prop_keys_prefix` | the autosuggest call, per keystroke | 11.4× | **1.1×** |
| `MD-06` `feed_tail` | polling after each write | 8.1× | 4.5× |
| `MD-07` `find` | a property query | 20–36× | 10–13× |

All of it was one mistake in five places: `kb.visible(p)` and `kb.to_view(p)` are function calls,
each queries the grant table, and the grant table query reaches the token table — so a surface
that asked per row asked three questions per row. The feed asked five. They join `kb.my_node`
now, which is the same restriction evaluated once.

## What is left, and why it is a different problem

Two things remain above 8×, and neither is a per-row function call.

### 1. A view's output column cannot be an index key — `CR-02`, `RT-03`

| test | case | owner | account | |
|---|---|---|---|---|
| `CR-02` | 100 readers, one file | 31.2 ms | 576.0 ms | 18.4× |
| `CR-02` | 10 readers | 3.5 ms | 127.5 ms | 36.3× |
| `RT-03` | 1 MiB read | 7.7 ms | 129.6 ms | 16.8× |

`kb.file`'s account branch is bounded by the share as an index **range** — which is what took a
delegated read from 6.3 s to 12.7 ms at 20,000 nodes — but `WHERE path = $1` on it is still a
filter over that range and not a lookup. Nothing a view can do changes that: the caller's
predicate binds to the view's output column, and there is no way to apply `kb.to_store` to it on
the way in. So an account's point read is O(share), and `CR-02` is a hundred readers paying that
at once.

This is the third option `#14` item 1 already named: a set-returning function, with the branch
and the translation chosen once in Rust rather than by the planner per call. `kb.content(path)`
is already that shape and is indexed; what is missing is the same for the row.

### 2. Projection crosses the SQL boundary twice — the large-document reads

| test | case | owner | account | |
|---|---|---|---|---|
| `XL-01..06` | 10 MiB `read_version` | 17.9 ms | 160.5 ms | 8.9× (sqlite) |
| `LL-04` | `read_version` | 1.1 ms | 14.6 ms | 13.4× (sqlite) |
| `ME-05` | `read_version` | 2.8 ms | 37.7 ms | 13.4× (pg) |

An account's content goes through `kb._project_text(id, path, kb._materialize(root))` where the
owner's is a bare `kb._materialize(root)`. For a document with no links the projection itself is
now skipped — Postgres asks the node row above 8 KiB rather than parsing a megabyte of
non-markdown as markdown to find out — but the **text still makes two extra copies** crossing the
function boundary. The SQLite binding has no equivalent short-circuit at all, which is why its
large-document `read_version` is the worst cell it has.

## The shape of the answer

`textdb-sqlite@account` at 1.16× geomean is inside what this host moves on its own. Delegation is
free there, and the 1.20× on sub-2 ms cells is the same fixed cost the owner's numbers have always
carried on small operations.

`textdb-pg@account` at 2.03× is not drift, and the remaining two items above say what it is made
of. Both are structural rather than accidental, and both have a named next step.
