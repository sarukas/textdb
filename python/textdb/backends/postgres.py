"""Postgres backend over psycopg2 (or psycopg 3) against the `textdb_pg` extension (schema kb)."""

from typing import Any, Dict, List, Optional

from .base import Backend, Bytes, to_text
from ..errors import NotFound, TextdbError, from_code

try:  # psycopg 3
    import psycopg as _pg  # type: ignore
    _PG3 = True
except ImportError:  # psycopg2
    import psycopg2 as _pg  # type: ignore
    _PG3 = False


def _wrap(fn):
    def inner(*a, **kw):
        try:
            return fn(*a, **kw)
        except _pg.Error as e:  # type: ignore[attr-defined]
            code = getattr(e, "sqlstate", None) or getattr(e, "pgcode", None)
            diag = getattr(e, "diag", None)
            detail = getattr(diag, "message_detail", None) if diag else None
            msg = getattr(diag, "message_primary", None) if diag else None
            if code and code.startswith("TX"):
                raise from_code(code, msg or str(e), detail) from None
            raise TextdbError(msg or str(e), code or "TX000") from None
    return inner


class PostgresBackend(Backend):
    name = "postgres"

    def __init__(self, url: str, *, autocommit: bool = True, **_):
        url = url.replace("pg://", "postgresql://", 1)
        self.conn = _pg.connect(url)
        self.conn.autocommit = autocommit

    def close(self) -> None:
        self.conn.close()

    def _rows(self, sql: str, params=()):
        with self.conn.cursor() as cur:
            cur.execute(sql, params)
            return cur.fetchall()

    def _one(self, sql: str, params=()):
        rows = self._rows(sql, params)
        return rows[0][0] if rows else None

    def _exec(self, sql: str, params=()) -> int:
        with self.conn.cursor() as cur:
            cur.execute(sql, params)
            return cur.rowcount

    # namespace ---------------------------------------------------------------
    @_wrap
    def ls(self, path: str):
        rows = self._rows("SELECT name, kind, nbytes, nlines, updated_at FROM kb.ls(%s)", (path,))
        p = path.rstrip("/")
        return [dict(name=r[0], kind=r[1], nbytes=r[2], nlines=r[3], updated_at=r[4], path=f"{p}/{r[0]}") for r in rows]

    @_wrap
    def list_files(self, prefix: str):
        rows = self._rows(
            "SELECT path, nbytes, nlines, version, updated_at FROM kb.file WHERE %s = '/' OR path LIKE %s || '/%%' ORDER BY path",
            (prefix, prefix))
        return [dict(path=r[0], nbytes=r[1], nlines=r[2], version=r[3], updated_at=r[4]) for r in rows]

    @_wrap
    def mkdir(self, path: str) -> None:
        self._exec("INSERT INTO kb.folder(path) VALUES (%s)", (path,))

    @_wrap
    def move(self, src: str, dst: str) -> None:
        if self._exec("UPDATE kb.file SET path = %s WHERE path = %s", (dst, src)) == 0:
            if self._exec("UPDATE kb.folder SET path = %s WHERE path = %s", (dst, src)) == 0:
                raise NotFound(f"not found: {src}")

    @_wrap
    def delete(self, path: str) -> None:
        if self._exec("DELETE FROM kb.file WHERE path = %s", (path,)) == 0:
            if self._exec("DELETE FROM kb.folder WHERE path = %s", (path,)) == 0:
                raise NotFound(f"not found: {path}")

    # read ----------------------------------------------------------------------
    @_wrap
    def read(self, path: str):
        rows = self._rows("SELECT content, version FROM kb.file WHERE path = %s", (path,))
        if not rows:
            raise NotFound(f"not found: {path}")
        return (rows[0][0] or "").encode("utf-8"), int(rows[0][1])

    @_wrap
    def read_version(self, path: str, version: int) -> bytes:
        return (self._one("SELECT kb.content(%s, %s)", (path, version)) or "").encode("utf-8")

    @_wrap
    def lines(self, path: str, first: int, last: int) -> bytes:
        return (self._one("SELECT kb.lines(%s, %s, %s)", (path, first, last)) or "").encode("utf-8")

    @_wrap
    def section(self, path: str, heading: str):
        v = self._one("SELECT kb.section(%s, %s)", (path, heading))
        return None if v is None else v.encode("utf-8")

    # write -----------------------------------------------------------------------
    @_wrap
    def exists(self, path: str) -> bool:
        return bool(self._one("SELECT count(*) FROM kb.file WHERE path = %s", (path,)))

    @_wrap
    def create(self, path: str, content: Bytes, author):
        self._exec("INSERT INTO kb.file(path, content, updated_by) VALUES (%s, %s, %s)", (path, to_text(content), author))
        return int(self._one("SELECT version FROM kb.file WHERE path = %s", (path,)))

    @_wrap
    def update(self, path: str, content: Bytes, base_version, author):
        n = self._exec("UPDATE kb.file SET content = %s, base_version = %s, updated_by = %s WHERE path = %s",
                       (to_text(content), base_version, author, path))
        if n == 0:
            raise NotFound(f"not found: {path}")
        return int(self._one("SELECT version FROM kb.file WHERE path = %s", (path,)))

    @_wrap
    def edit(self, path: str, old: Bytes, new: Bytes, author):
        return int(self._one("SELECT kb.edit(%s, %s, %s, %s)", (path, to_text(old), to_text(new), author)))

    @_wrap
    def append(self, path: str, tail: Bytes, author):
        return int(self._one("SELECT kb.append(%s, %s, %s)", (path, to_text(tail), author)))

    # history / search ------------------------------------------------------------
    @_wrap
    def history(self, path: str):
        rows = self._rows("SELECT version, author, ts, message FROM kb.history(%s)", (path,))
        return [dict(version=r[0], author=r[1], ts=r[2], message=r[3], nbytes=None) for r in rows]

    @_wrap
    def diff(self, path: str, v1: int, v2: int) -> str:
        return self._one("SELECT kb.diff(%s, %s, %s)", (path, v1, v2)) or ""

    @_wrap
    def search(self, query: str, prefix: str, limit: int):
        rows = self._rows("SELECT path, line, snippet, rank FROM kb.search(%s, %s, %s)", (query, prefix, limit))
        return [dict(path=r[0], line=r[1], snippet=r[2], rank=r[3]) for r in rows]

    @_wrap
    def property_keys(self, prefix: str, limit: int):
        rows = self._rows("SELECT key, docs, values_n, kind FROM kb.prop_keys(%s, %s)", (prefix, limit))
        return [dict(key=r[0], docs=r[1], values=r[2], kind=r[3]) for r in rows]

    @_wrap
    def property_values(self, key: str, prefix: str, limit: int):
        rows = self._rows("SELECT value, docs FROM kb.prop_values(%s, %s, %s)", (key, prefix, limit))
        return [dict(value=r[0], docs=r[1]) for r in rows]

    @_wrap
    def property_find(self, query: str, folder: str, limit: int):
        rows = self._rows("SELECT path, nbytes, updated_at, frontmatter FROM kb.prop_find(%s, %s, %s)", (query, folder, limit))
        return [dict(path=r[0], nbytes=r[1], updated_at=r[2], frontmatter=r[3]) for r in rows]

    @_wrap
    def checkpoint(self, name: str) -> int:
        return int(self._one("SELECT kb.checkpoint(%s)", (name,)))

    @_wrap
    def export(self, prefix: str):
        return [(r[0], (r[1] or "").encode("utf-8")) for r in self._rows("SELECT path, content FROM kb.export(%s)", (prefix,))]
