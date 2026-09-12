# textdb (Python)

A small library for manipulating versioned text corpora stored in **textdb**, with a
swappable backend chosen from the URL: `sqlite:///file.db` (embedded, needs the loadable
extension) or `postgresql://…` (needs the `textdb_pg` extension in that database).

```sh
pip install -e python/                 # from the repository root
pip install -e 'python/[postgres]'     # adds psycopg2-binary
```

For SQLite build the extension once and point the library at it:

```sh
(cd crates/textdb-sqlite-ext && cargo build --release)
export TEXTDB_SQLITE_EXT=$PWD/crates/textdb-sqlite-ext/target/release/libtextdb_sqlite_ext.so   # optional; auto-found under target/release
```

```python
from textdb import Corpus, Conflict

with Corpus.open("sqlite:///kb.db", author="me") as kb:      # or "postgresql://user@host/db"
    kb.load_folder("./notes", "/notes")                      # idempotent incremental import
    kb.load_file("./README.md", "/docs/README.md")
    text, version = kb.read_versioned("/notes/index.md")
    kb.edit("/notes/index.md", "TODO", "DONE")               # anchored, rebased over concurrent writes
    try:
        kb.update("/notes/index.md", text.replace("x", "y"), base_version=version)
    except Conflict as c:
        print(c.theirs, c.current_version)                   # current text of the conflicting lines
    kb.append("/notes/journal.md", "- done\n")               # never conflicts
    for hit in kb.search("renewal pricing", "/notes"):
        print(hit.path, hit.line, hit.snippet)
    print(kb.history("/notes/index.md"), kb.diff("/notes/index.md", 1, 2))
    kb.export_folder("/notes", "./notes-export")
```

Command line: `python -m textdb sqlite:///kb.db load ./notes /notes`, `… ls /notes`,
`… cat /notes/index.md --version 2`, `… search "renewal pricing" /notes`,
`… edit /p "old" "new"`, `… append /p "text"`, `… history /p`, `… diff /p 1 2`,
`… mv /a /b`, `… rm /p`, `… export /notes ./out`.

Examples: [`examples/load_single_file.py`](examples/load_single_file.py),
[`examples/load_folder.py`](examples/load_folder.py),
[`examples/agent_edit_loop.py`](examples/agent_edit_loop.py),
[`examples/switch_backends.py`](examples/switch_backends.py).

Tests: `pytest python/tests` (SQLite runs when the extension is found; set
`TEXTDB_TEST_PG=postgresql://…` to include Postgres).

## API

| Method | Notes |
|---|---|
| `Corpus.open(url, author=None)` | backend by scheme; context manager |
| `ls(path)`, `list(prefix)`, `files(prefix)`, `exists(path)` | namespace |
| `read(path)`, `read_bytes`, `read_versioned(path) → (text, version)`, `version(path)` | |
| `lines(path, first, last)`, `section(path, heading)` | fragments (O(fragment)) |
| `write(path, content)` | create or replace (diffed); identical content → no new version |
| `update(path, content, base_version=…)` | rebase over concurrent commits; raises `Conflict` |
| `edit(path, old, new)` | unique anchored replace; `InvalidEdit` if not unique |
| `append(path, tail)` | never conflicts |
| `edit_with_retry(path, fn)` | read → `fn(text)` → write with base; retries on `Conflict` |
| `history(path)`, `read_version(path, v)`, `diff(path, v1, v2)`, `checkpoint(name)` | |
| `search(query, prefix, limit)` | AND per document, `"phrase"`, `prefix*` |
| `mkdir`, `move(src, dst)`, `delete(path)` | files and folders (subtrees) |
| `load_file(local, dest)`, `load_folder(dir, prefix, include=…, exclude=…)`, `export_folder(prefix, dir)` | loaders |

Errors: `TextdbError` (`.code`), `Conflict` (`.theirs`, `.base`, `.ours`, `.region`, `.current_version`),
`Contention`, `NotFound`, `InvalidEdit`.
