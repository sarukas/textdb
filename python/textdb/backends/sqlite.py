"""SQLite backend: Python's stdlib `sqlite3` plus the loadable extension
`libtextdb_sqlite_ext.so` (built from crates/textdb-sqlite-ext)."""

import os
import sqlite3
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

from .base import Backend, Bytes, to_bytes, to_text
from ..errors import NotFound, TextdbError, from_message

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
        # A store written by an older build lacks the change feed and the commit columns
        # this one records; bring it up to date before the first write needs them.
        self.conn.execute("SELECT textdb_migrate()")

    def close(self) -> None:
        self.conn.close()

    def _one(self, sql: str, params=()):
        cur = self.conn.execute(sql, params)
        row = cur.fetchone()
        return row[0] if row else None

    # namespace ---------------------------------------------------------------
    @_wrap
    def ls(self, path: str, recursive: bool = False):
        rows = self.conn.execute("SELECT path, name, kind, version, nbytes, nlines, updated_at, updated_by, id, dir, depth, ext, title, nwords, nsections, nprops, nlinks, nlinks_broken, versions, created_at, files, folders, nauthors, authors FROM textdb_ls(?, ?)", (path, 1 if recursive else 0)).fetchall()
        return [dict(path=r[0], name=r[1], kind=r[2], version=r[3], nbytes=r[4], nlines=r[5], updated_at=r[6], updated_by=r[7], id=r[8], dir=r[9], depth=r[10], ext=r[11], title=r[12], nwords=r[13], nsections=r[14], nprops=r[15], nlinks=r[16], nlinks_broken=r[17], versions=r[18], created_at=r[19], files=r[20], folders=r[21], nauthors=r[22], authors=r[23]) for r in rows]

    @_wrap
    def entry(self, path: str):
        rows = self.conn.execute("SELECT path, name, kind, version, nbytes, nlines, updated_at, updated_by, id, dir, depth, ext, title, nwords, nsections, nprops, nlinks, nlinks_broken, versions, created_at, files, folders, nauthors, authors FROM textdb_entry(?)", (path,)).fetchall()
        return [dict(path=r[0], name=r[1], kind=r[2], version=r[3], nbytes=r[4], nlines=r[5], updated_at=r[6], updated_by=r[7], id=r[8], dir=r[9], depth=r[10], ext=r[11], title=r[12], nwords=r[13], nsections=r[14], nprops=r[15], nlinks=r[16], nlinks_broken=r[17], versions=r[18], created_at=r[19], files=r[20], folders=r[21], nauthors=r[22], authors=r[23]) for r in rows]

    @_wrap
    def list_files(self, prefix: str):
        # A range on `path` rather than `substr(path, 1, length(?) + 1) = ? || '/'`: the
        # virtual table turns a bound into an index seek on the shadow table, where the
        # substr form is a function of the column and forces a full scan. "0" (0x30) is the
        # byte after "/" (0x2F), so `prefix || '0'` is the exclusive end of the subtree.
        cols = "SELECT path, nbytes, nlines, version, updated_at FROM kb WHERE kind = 'file'"
        if prefix == "/":
            rows = self.conn.execute(f"{cols} ORDER BY path").fetchall()
        else:
            rows = self.conn.execute(
                f"{cols} AND path >= ? AND path < ? ORDER BY path", (prefix + "/", prefix + "0")
            ).fetchall()
        return [dict(path=r[0], nbytes=r[1], nlines=r[2], version=r[3], updated_at=r[4]) for r in rows]

    @_wrap
    def mkdir(self, path: str) -> None:
        self.conn.execute("INSERT INTO kb(path, kind) VALUES (?, 'folder')", (path,))

    @_wrap
    def move(self, src: str, dst: str) -> None:
        if self.conn.execute("UPDATE kb SET path = ? WHERE path = ?", (dst, src)).rowcount == 0:
            raise NotFound(f"not found: {src}")

    @_wrap
    def delete(self, path: str) -> None:
        if self.conn.execute("DELETE FROM kb WHERE path = ?", (path,)).rowcount == 0:
            raise NotFound(f"not found: {path}")

    # read ----------------------------------------------------------------------
    @_wrap
    def read(self, path: str):
        row = self.conn.execute("SELECT content, version FROM kb WHERE path = ? AND kind = 'file'", (path,)).fetchone()
        if row is None:
            raise NotFound(f"not found: {path}")
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
            raise NotFound(f"not found: {path}")
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
        rows = self.conn.execute(
            "SELECT version, author, ts, message, nbytes, kind, base_version, nlines, nwords FROM textdb_history(?)",
            (path,),
        ).fetchall()
        return [dict(version=r[0], author=r[1], ts=r[2], message=r[3], nbytes=r[4], kind=r[5], base_version=r[6], nlines=r[7], nwords=r[8]) for r in rows]

    @_wrap
    def diff(self, path: str, v1: int, v2: int) -> str:
        return self._one("SELECT textdb_diff(?, ?, ?)", (path, v1, v2)) or ""

    @_wrap
    def search(self, query: str, prefix: str, limit: int, per_file: int):
        rows = self.conn.execute(
            "SELECT path, version, line, text, section, score, more FROM textdb_search(?, ?, ?, ?)",
            (query, prefix, limit, per_file),
        ).fetchall()
        return [dict(path=r[0], version=r[1], line=r[2], text=r[3], section=r[4], score=r[5], more=r[6]) for r in rows]

    @_wrap
    def property_keys(self, prefix: str, limit: int):
        rows = self.conn.execute("SELECT key, docs, values_n, kind FROM textdb_prop_keys(?, ?)", (prefix, limit)).fetchall()
        return [dict(key=r[0], docs=r[1], values_n=r[2], kind=r[3]) for r in rows]

    @_wrap
    def property_values(self, key: str, prefix: str, limit: int):
        rows = self.conn.execute("SELECT value, docs FROM textdb_prop_values(?, ?, ?)", (key, prefix, limit)).fetchall()
        return [dict(value=r[0], docs=r[1]) for r in rows]

    @_wrap
    def outline(self, prefix: str, heading, mode: str, max_level, limit: int):
        rows = self.conn.execute(
            "SELECT path, heading, heading_path, level, line_from, line_to, nwords, nwords_total, nbytes, nlines, file_nwords, version, updated_at, updated_by FROM textdb_outline(?, ?, ?, ?, ?)", (prefix, heading, mode, max_level, limit)
        ).fetchall()
        return [dict(path=r[0], heading=r[1], heading_path=r[2], level=r[3], line_from=r[4], line_to=r[5],
                     nwords=r[6], nwords_total=r[7], nbytes=r[8], nlines=r[9], file_nwords=r[10],
                     version=r[11], updated_at=r[12], updated_by=r[13]) for r in rows]

    @_wrap
    def heading_names(self, prefix: str, starts: str, limit: int):
        rows = self.conn.execute("SELECT heading, sections, docs FROM textdb_headings(?, ?, ?)", (prefix, starts, limit)).fetchall()
        return [dict(heading=r[0], sections=r[1], docs=r[2]) for r in rows]

    @_wrap
    def property_find(self, query: str, folder: str, limit: int):
        rows = self.conn.execute(
            "SELECT path, nbytes, updated_at, frontmatter FROM textdb_prop_find(?, ?, ?)", (query, folder, limit)
        ).fetchall()
        return [dict(path=r[0], nbytes=r[1], updated_at=r[2], frontmatter=r[3]) for r in rows]

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
