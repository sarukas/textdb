"""SQLite backend: Python's stdlib `sqlite3` plus the loadable extension
`libtextdb_sqlite_ext.so` (built from crates/textdb-sqlite-ext)."""

import os
import sqlite3
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

from .base import Backend, Bytes, to_bytes, to_text
from ..errors import TextdbError, from_message

_CANDIDATES = [
    "libtextdb_sqlite_ext.so", "libtextdb_sqlite_ext.dylib", "textdb_sqlite_ext.dll",
]


def find_extension() -> str:
    """Locate the loadable extension: $TEXTDB_SQLITE_EXT, next to the package (textdb/lib/),
    the repository's target/release, or the usual library directories."""
    env = os.environ.get("TEXTDB_SQLITE_EXT")
    if env:
        return env
    here = Path(__file__).resolve().parent.parent
    roots = [here / "lib", here.parent.parent / "target" / "release", here.parent.parent / "crates" / "textdb-sqlite-ext" / "target" / "release",
             Path("/usr/local/lib"), Path("/usr/lib")]
    for root in roots:
        for name in _CANDIDATES:
            p = root / name
            if p.exists():
                return str(p)
    raise TextdbError(
        "textdb SQLite extension not found. Build it with "
        "`cd crates/textdb-sqlite-ext && cargo build --release` and set TEXTDB_SQLITE_EXT to the .so path.")


def _wrap(fn):
    def inner(*a, **kw):
        try:
            return fn(*a, **kw)
        except sqlite3.Error as e:
            err = from_message(str(e))
            if err:
                raise err from None
            raise TextdbError(str(e)) from None
    return inner


class SqliteBackend(Backend):
    name = "sqlite"

    def __init__(self, url: str, *, store: str = "kb_", extension: Optional[str] = None, **_):
        path = url.split("://", 1)[1] if "://" in url else url
        if path.startswith("/") and not os.path.exists(os.path.dirname(path) or "/"):
            pass
        if path in ("", ":memory:"):
            path = ":memory:"
        elif path.startswith("/") and url.startswith("sqlite:///") and not path.startswith("//"):
            # sqlite:///relative.db  → "relative.db"; sqlite:////abs.db → "/abs.db"
            path = path[1:] if not url.startswith("sqlite:////") else path
        self.path = path
        self.store = store
        self.conn = sqlite3.connect(path, isolation_level=None, check_same_thread=False)
        self.conn.enable_load_extension(True)
        self.conn.load_extension(extension or find_extension())
        self.conn.enable_load_extension(False)
        self.conn.execute("PRAGMA journal_mode = WAL")
        self.conn.execute("PRAGMA synchronous = NORMAL")
        self.conn.execute("PRAGMA busy_timeout = 30000")
        self.conn.execute(f"CREATE VIRTUAL TABLE IF NOT EXISTS kb USING textdb(store='{store}')")

    def close(self) -> None:
        self.conn.close()

    def _one(self, sql: str, params=()):
        cur = self.conn.execute(sql, params)
        row = cur.fetchone()
        return row[0] if row else None

    # namespace ---------------------------------------------------------------
    @_wrap
    def ls(self, path: str):
        rows = self.conn.execute("SELECT name, kind, nbytes, nlines, updated_at, path FROM textdb_ls(?)", (path,)).fetchall()
        return [dict(name=r[0], kind=r[1], nbytes=r[2], nlines=r[3], updated_at=r[4], path=r[5]) for r in rows]

    @_wrap
    def list_files(self, prefix: str):
        rows = self.conn.execute(
            "SELECT path, nbytes, nlines, version, updated_at FROM kb WHERE kind = 'file' AND (? = '/' OR substr(path, 1, length(?) + 1) = ? || '/') ORDER BY path",
            (prefix, prefix, prefix)).fetchall()
        return [dict(path=r[0], nbytes=r[1], nlines=r[2], version=r[3], updated_at=r[4]) for r in rows]

    @_wrap
    def mkdir(self, path: str) -> None:
        self.conn.execute("INSERT INTO kb(path, kind) VALUES (?, 'folder')", (path,))

    @_wrap
    def move(self, src: str, dst: str) -> None:
        if self.conn.execute("UPDATE kb SET path = ? WHERE path = ?", (dst, src)).rowcount == 0:
            raise TextdbError(f"not found: {src}", "TX003")

    @_wrap
    def delete(self, path: str) -> None:
        if self.conn.execute("DELETE FROM kb WHERE path = ?", (path,)).rowcount == 0:
            raise TextdbError(f"not found: {path}", "TX003")

    # read ----------------------------------------------------------------------
    @_wrap
    def read(self, path: str):
        row = self.conn.execute("SELECT content, version FROM kb WHERE path = ? AND kind = 'file'", (path,)).fetchone()
        if row is None:
            raise TextdbError(f"not found: {path}", "TX003")
        return to_bytes(row[0] if row[0] is not None else b""), int(row[1])

    @_wrap
    def read_version(self, path: str, version: int) -> bytes:
        return to_bytes(self._one("SELECT textdb_content(?, ?)", (path, version)) or b"")

    @_wrap
    def lines(self, path: str, first: int, last: int) -> bytes:
        return to_bytes(self._one("SELECT textdb_lines(?, ?, ?)", (path, first, last)) or b"")

    @_wrap
    def section(self, path: str, heading: str):
        v = self._one("SELECT textdb_section(?, ?)", (path, heading))
        return None if v is None else to_bytes(v)

    # write -----------------------------------------------------------------------
    @_wrap
    def exists(self, path: str) -> bool:
        return self._one("SELECT count(*) FROM kb WHERE path = ?", (path,)) > 0

    @_wrap
    def create(self, path: str, content: Bytes, author):
        self.conn.execute("INSERT INTO kb(path, content, author) VALUES (?, ?, ?)", (path, _param(content), author))
        return int(self._one("SELECT version FROM kb WHERE path = ?", (path,)))

    @_wrap
    def update(self, path: str, content: Bytes, base_version, author):
        n = self.conn.execute("UPDATE kb SET content = ?, base_version = ?, author = ? WHERE path = ?",
                              (_param(content), base_version, author, path)).rowcount
        if n == 0:
            raise TextdbError(f"not found: {path}", "TX003")
        return int(self._one("SELECT version FROM kb WHERE path = ?", (path,)))

    @_wrap
    def edit(self, path: str, old: Bytes, new: Bytes, author):
        return int(self._one("SELECT textdb_edit(?, ?, ?, ?)", (path, _param(old), _param(new), author or "")))

    @_wrap
    def append(self, path: str, tail: Bytes, author):
        return int(self._one("SELECT textdb_append(?, ?, ?)", (path, _param(tail), author or "")))

    # history / search ------------------------------------------------------------
    @_wrap
    def history(self, path: str):
        rows = self.conn.execute("SELECT version, author, ts, message, nbytes FROM textdb_history(?)", (path,)).fetchall()
        return [dict(version=r[0], author=r[1], ts=r[2], message=r[3], nbytes=r[4]) for r in rows]

    @_wrap
    def diff(self, path: str, v1: int, v2: int) -> str:
        return self._one("SELECT textdb_diff(?, ?, ?)", (path, v1, v2)) or ""

    @_wrap
    def search(self, query: str, prefix: str, limit: int):
        rows = self.conn.execute("SELECT path, line, snippet, rank FROM textdb_search(?, ?, ?)", (query, prefix, limit)).fetchall()
        return [dict(path=r[0], line=r[1], snippet=r[2], rank=r[3]) for r in rows]

    @_wrap
    def checkpoint(self, name: str) -> int:
        return int(self._one("SELECT textdb_checkpoint(?)", (name,)))

    @_wrap
    def export(self, prefix: str):
        rows = self.conn.execute("SELECT path, content FROM textdb_export(?)", (prefix,)).fetchall()
        return [(r[0], to_bytes(r[1] if r[1] is not None else b"")) for r in rows]


def _param(v: Bytes):
    """Text stays text (so the store keeps it as TEXT); bytes that are valid UTF-8 become text
    too; anything else is stored as a BLOB byte-for-byte."""
    if isinstance(v, str):
        return v
    try:
        return bytes(v).decode("utf-8")
    except UnicodeDecodeError:
        return bytes(v)


__all__ = ["SqliteBackend", "find_extension", "to_text"]
