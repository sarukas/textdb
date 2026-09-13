#!/usr/bin/env python3
"""Load `libtextdb_sqlite_ext` into a stock `sqlite3` and exercise the SQL surface.

Run after `cd crates/textdb-sqlite-ext && cargo build --release`:

    python3 python/tests/load_extension_smoke.py

Not a pytest test: the rest of the Python suite goes through `textdb.Corpus`, which means
a break in the extension's *entry point* — the thing that differs between rusqlite
versions and is invisible to `cargo build` — shows up there as "extension not found" and
gets skipped. This script fails loudly instead, so CI catches it.
"""

import os
import sqlite3
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "python"))

from textdb.backends.sqlite import find_extension  # noqa: E402


def main() -> int:
    ext = find_extension()
    print("extension:", ext)
    with tempfile.TemporaryDirectory() as d:
        conn = sqlite3.connect(os.path.join(d, "kb.db"))
        conn.enable_load_extension(True)
        conn.load_extension(ext)
        conn.enable_load_extension(False)
        conn.execute("CREATE VIRTUAL TABLE kb USING textdb(store='kb_')")

        conn.execute(
            "INSERT INTO kb(path, content, author) VALUES ('/notes/a.md', ?, 'alice')",
            ("# A\nalpha beta\n",),
        )
        conn.execute("UPDATE kb SET content = replace(content, 'alpha', 'ALPHA') WHERE path = '/notes/a.md'")
        conn.execute("SELECT textdb_edit('/notes/a.md', 'beta', 'BETA')").fetchone()

        def one(sql, *args):
            return conn.execute(sql, args).fetchone()

        checks = [
            ("content", one("SELECT content FROM kb WHERE path = '/notes/a.md'")[0], "# A\nALPHA BETA\n"),
            ("version", one("SELECT version FROM kb WHERE path = '/notes/a.md'")[0], 3),
            ("version 1", one("SELECT textdb_content('/notes/a.md', 1)")[0], "# A\nalpha beta\n"),
            ("lines 2..2", one("SELECT textdb_lines('/notes/a.md', 2, 2)")[0], "ALPHA BETA\n"),
            ("search path", one("SELECT path FROM textdb_search('BETA', '/notes')")[0], "/notes/a.md"),
            ("history rows", one("SELECT count(*) FROM textdb_history('/notes/a.md')")[0], 3),
            ("ls", one("SELECT name FROM textdb_ls('/notes')")[0], "a.md"),
        ]
        diff = one("SELECT textdb_diff('/notes/a.md', 1, 3)")[0]
        conn.close()

    failed = False
    for name, got, want in checks:
        ok = got == want
        failed = failed or not ok
        print(f"  {'ok  ' if ok else 'FAIL'} {name}: {got!r}" + ("" if ok else f" (want {want!r})"))
    if "-alpha beta" not in diff or "+ALPHA BETA" not in diff:
        failed = True
        print(f"  FAIL diff: {diff!r}")
    else:
        print("  ok   diff")
    print("FAILED" if failed else "OK")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
