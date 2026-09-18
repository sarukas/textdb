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


_ENTRY_KEYS = (
    "path", "name", "kind", "version", "nbytes", "nlines", "updated_at", "updated_by", "id", "dir", "depth", "ext",
    "title", "nwords", "nsections", "nprops", "nlinks", "nlinks_broken", "versions", "created_at", "files", "folders",
    "nauthors", "authors",
)


def _utc(col):
    """A timestamp as ISO-8601 UTC, rendered in SQL.

    Left to the driver it would come back a ``datetime`` in the session's time zone: a
    different type *and* a different spelling of the same field from the SQLite backend's.
    """
    return "to_char(" + col + " AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')"


#: The canonical ``Entry`` columns of ``kb.entry``, in order.
_ENTRY_COLS = ", ".join(
    _utc("e." + k) if k in ("updated_at", "created_at") else ("e.authors::text" if k == "authors" else "e." + k)
    for k in _ENTRY_KEYS
)


def _entry(row):
    return dict(zip(_ENTRY_KEYS, row))


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
    def ls(self, path: str, recursive: bool = False):
        rows = self._rows(f"SELECT {_ENTRY_COLS} FROM kb.ls(%s, %s) e", (path, recursive))
        return [_entry(r) for r in rows]

    @_wrap
    def entry(self, path: str):
        rows = self._rows(f"SELECT {_ENTRY_COLS} FROM kb.entry e WHERE e.path = %s", (path,))
        return [_entry(r) for r in rows]

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
        rows = self._rows(
            f"SELECT version, author, {_utc('ts')}, message, kind, base_version, nbytes, nlines, nwords "
            "FROM kb.history(%s)",
            (path,),
        )
        return [dict(version=r[0], author=r[1], ts=r[2], message=r[3], kind=r[4], base_version=r[5], nbytes=r[6], nlines=r[7], nwords=r[8]) for r in rows]

    @_wrap
    def links(self, path: str, status: str, limit: int, incoming: bool):
        fn = "kb.backlinks" if incoming else "kb.links"
        rows = self._rows(f"SELECT path, version, line, kind, target, anchor, alias, status, resolved, asset FROM {fn}(%s, %s, %s)", (path, status, limit))
        return [dict(path=r[0], version=r[1], line=r[2], kind=r[3], target=r[4], anchor=r[5], alias=r[6], status=r[7], resolved=r[8], asset=bool(r[9])) for r in rows]

    @_wrap
    def diff(self, path: str, v1: int, v2: int) -> str:
        return self._one("SELECT kb.diff(%s, %s, %s)", (path, v1, v2)) or ""

    @_wrap
    def search(self, query: str, prefix: str, limit: int, per_file: int):
        rows = self._rows(
            "SELECT path, version, line, text, section, score, more FROM kb.search(%s, %s, %s, %s)",
            (query, prefix, limit, per_file),
        )
        return [dict(path=r[0], version=r[1], line=r[2], text=r[3], section=r[4], score=r[5], more=r[6]) for r in rows]

    @_wrap
    def property_keys(self, prefix: str, limit: int):
        rows = self._rows("SELECT key, docs, values_n, kind FROM kb.prop_keys(%s, %s)", (prefix, limit))
        return [dict(key=r[0], docs=r[1], values_n=r[2], kind=r[3]) for r in rows]

    @_wrap
    def property_values(self, key: str, prefix: str, limit: int):
        rows = self._rows("SELECT value, docs FROM kb.prop_values(%s, %s, %s)", (key, prefix, limit))
        return [dict(value=r[0], docs=r[1]) for r in rows]

    @_wrap
    def outline(self, prefix: str, heading, mode: str, max_level, limit: int):
        # `updated_at` as text so both engines return the same string; Postgres would
        # otherwise hand back a datetime and the SQLite backend an ISO-8601 string.
        rows = self._rows(
            "SELECT path, heading, heading_path, level, line_from, line_to, nwords, nwords_total, "
            "nbytes, nlines, file_nwords, version, "
            "to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), updated_by "
            "FROM kb.outline(%s, %s, %s, %s, %s)",
            (prefix, heading, mode, max_level, limit),
        )
        return [dict(path=r[0], heading=r[1], heading_path=r[2], level=r[3], line_from=r[4], line_to=r[5],
                     nwords=r[6], nwords_total=r[7], nbytes=r[8], nlines=r[9], file_nwords=r[10],
                     version=r[11], updated_at=r[12], updated_by=r[13]) for r in rows]

    @_wrap
    def heading_names(self, prefix: str, starts: str, limit: int):
        rows = self._rows("SELECT heading, sections, docs FROM kb.headings(%s, %s, %s)", (prefix, starts, limit))
        return [dict(heading=r[0], sections=r[1], docs=r[2]) for r in rows]

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
