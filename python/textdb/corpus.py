"""`Corpus`: the user-facing API over a backend, plus file/folder loaders."""

import fnmatch
import json
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Dict, Iterable, List, Optional, Sequence, Tuple, Union

from .backends import open_backend
from .backends.base import Bytes, to_text
from .errors import Conflict, TextdbError

DEFAULT_INCLUDE: Tuple[str, ...] = ("*.md", "*.markdown", "*.txt", "*.rst", "*.adoc", "*.org", "*.csv", "*.json", "*.yaml", "*.yml", "*.toml")


@dataclass
class Entry:
    path: str
    name: str
    kind: str
    nbytes: Optional[int]
    nlines: Optional[int]
    updated_at: Any


@dataclass
class Hit:
    path: str
    line: int
    snippet: str
    rank: float


@dataclass
class PropertyKey:
    """A front-matter property name in use across the store."""
    key: str
    #: Documents carrying it — a note with three tags counts once.
    docs: int
    #: Distinct values it takes.
    values: int
    #: ``number``, ``text`` or ``mixed``; a UI offers ``>`` only where it means something.
    kind: str


@dataclass
class PropertyValue:
    """One value a property takes, and how many documents use it."""
    value: Optional[str]
    docs: int


@dataclass
class OutlineEntry:
    """One markdown heading, with its document's own figures alongside."""
    path: str
    #: The last component of the heading path, as written.
    heading: str
    #: The breadcrumb, ``Parent / Child``.
    heading_path: str
    #: 1 for ``#``, 2 for ``##``, and so on.
    level: int
    line_from: int
    line_to: int
    #: Words in the section's own lines, and in it plus everything nested under it. ``None``
    #: on a store whose rows predate the counts and has not been rewritten since.
    nwords: Optional[int]
    nwords_total: Optional[int]
    #: The document's own figures, repeated on each of its rows.
    nbytes: Optional[int]
    nlines: Optional[int]
    file_nwords: Optional[int]
    version: int
    updated_at: str
    updated_by: Optional[str]


@dataclass
class HeadingName:
    """A distinct heading in use across the scope asked about."""
    heading: str
    #: Sections carrying it, and documents they are spread over.
    sections: int
    docs: int


@dataclass
class PropertyHit:
    """A document matched by a property query."""
    path: str
    nbytes: int
    updated_at: str
    #: The whole front matter, so a table view needs no query per cell.
    frontmatter: Optional[Dict[str, Any]]


@dataclass
class Commit:
    version: int
    author: Optional[str]
    ts: Any
    message: Optional[str]


@dataclass
class LoadStats:
    created: int = 0
    updated: int = 0
    unchanged: int = 0
    skipped: int = 0
    errors: int = 0
    files: int = 0

    def __str__(self) -> str:
        return (f"{self.files} files: {self.created} created, {self.updated} updated, "
                f"{self.unchanged} unchanged, {self.skipped} skipped, {self.errors} errors")


def normalize(path: str) -> str:
    segs = [s for s in path.replace("\\", "/").split("/") if s not in ("", ".")]
    if any(s == ".." for s in segs):
        raise TextdbError(f"invalid path: {path}", "TX004")
    return "/" + "/".join(segs)


class Corpus:
    """A versioned text corpus. Open with a URL; the backend is chosen by scheme:

        Corpus.open("sqlite:///kb.db")                  # embedded (needs libtextdb_sqlite_ext)
        Corpus.open("postgresql://user:pw@host/db")     # textdb_pg extension installed there
    """

    def __init__(self, backend, author: Optional[str] = None):
        self.backend = backend
        self.author = author

    @classmethod
    def open(cls, url: str, *, author: Optional[str] = None, **backend_kwargs) -> "Corpus":
        return cls(open_backend(url, **backend_kwargs), author=author)

    def close(self) -> None:
        self.backend.close()

    def __enter__(self) -> "Corpus":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    # ------------------------------------------------------------------ namespace
    def ls(self, path: str = "/") -> List[Entry]:
        return [Entry(r["path"], r["name"], r["kind"], r["nbytes"], r["nlines"], r["updated_at"]) for r in self.backend.ls(normalize(path))]

    def list(self, prefix: str = "/") -> List[str]:
        return [r["path"] for r in self.backend.list_files(normalize(prefix))]

    def files(self, prefix: str = "/") -> List[Dict[str, Any]]:
        return self.backend.list_files(normalize(prefix))

    def exists(self, path: str) -> bool:
        return self.backend.exists(normalize(path))

    def mkdir(self, path: str) -> None:
        self.backend.mkdir(normalize(path))

    def move(self, src: str, dst: str) -> None:
        self.backend.move(normalize(src), normalize(dst))

    def delete(self, path: str) -> None:
        self.backend.delete(normalize(path))

    # ----------------------------------------------------------------------- read
    def read(self, path: str) -> str:
        return to_text(self.backend.read(normalize(path))[0])

    def read_bytes(self, path: str) -> bytes:
        return self.backend.read(normalize(path))[0]

    def read_versioned(self, path: str) -> Tuple[str, int]:
        b, v = self.backend.read(normalize(path))
        return to_text(b), v

    def version(self, path: str) -> int:
        return self.backend.read(normalize(path))[1]

    def read_version(self, path: str, version: int) -> str:
        return to_text(self.backend.read_version(normalize(path), version))

    def lines(self, path: str, first: int, last: int) -> str:
        """Lines first..last, 1-based inclusive."""
        return to_text(self.backend.lines(normalize(path), first, last))

    def section(self, path: str, heading: str) -> Optional[str]:
        """Text of a markdown section by heading (exact 'A / B' path or last component)."""
        v = self.backend.section(normalize(path), heading)
        return None if v is None else to_text(v)

    # ---------------------------------------------------------------------- write
    def write(self, path: str, content: Bytes, *, author: Optional[str] = None) -> int:
        """Create the file or replace its content (diffed against the current version).
        Identical content creates no new version. Returns the version."""
        path = normalize(path)
        a = author or self.author
        if self.backend.exists(path):
            return self.backend.update(path, content, None, a)
        return self.backend.create(path, content, a)

    create = write

    def update(self, path: str, content: Bytes, *, base_version: Optional[int] = None, author: Optional[str] = None) -> int:
        """Replace content that was derived from `base_version`; textdb rebases over concurrent
        commits and raises `Conflict` (with the current text) when the same lines changed."""
        return self.backend.update(normalize(path), content, base_version, author or self.author)

    def edit(self, path: str, old: Bytes, new: Bytes, *, author: Optional[str] = None) -> int:
        """Replace the unique occurrence of `old` with `new`. Raises InvalidEdit if `old` is
        missing or ambiguous, Conflict if the lines changed concurrently."""
        return self.backend.edit(normalize(path), old, new, author or self.author)

    def append(self, path: str, tail: Bytes, *, author: Optional[str] = None) -> int:
        """Append at the end; never conflicts."""
        return self.backend.append(normalize(path), tail, author or self.author)

    def edit_with_retry(self, path: str, fn: Callable[[str], str], *, attempts: int = 5, author: Optional[str] = None) -> int:
        """Read → fn(current_text) → write with base_version; on Conflict re-read and retry.
        `fn` must be a pure function of the text it is given."""
        last: Optional[TextdbError] = None
        for _ in range(attempts):
            text, version = self.read_versioned(path)
            new = fn(text)
            if new == text:
                return version
            try:
                return self.update(path, new, base_version=version, author=author)
            except Conflict as e:
                last = e
        raise last or TextdbError("edit_with_retry: no attempts")

    # -------------------------------------------------------------- history/search
    def history(self, path: str) -> List[Commit]:
        return [Commit(r["version"], r["author"], r["ts"], r["message"]) for r in self.backend.history(normalize(path))]

    def diff(self, path: str, v1: int, v2: int) -> str:
        return self.backend.diff(normalize(path), v1, v2)

    def search(self, query: str, prefix: str = "/", *, limit: int = 100) -> List[Hit]:
        """Terms are ANDed per document; "quoted phrase"; prefix*. Hits carry the first matching line."""
        return [Hit(r["path"], r["line"], r["snippet"], r["rank"]) for r in self.backend.search(query, normalize(prefix), limit)]

    def property_keys(self, prefix: str = "", *, limit: int = 200) -> List[PropertyKey]:
        """Property names in use, most-used first.

        ``prefix`` is what the user has typed: this is the autosuggest call, so it reads an
        index range rather than scanning.
        """
        return [PropertyKey(r["key"], r["docs"], r["values"], r["kind"]) for r in self.backend.property_keys(prefix, limit)]

    def outline(
        self,
        path: str = "/",
        *,
        heading: Optional[str] = None,
        match: str = "exact",
        level: Optional[int] = None,
        limit: int = 1000,
    ) -> List[OutlineEntry]:
        """Markdown headings under ``path``: one document's outline, a folder's, or the store's.

        Each entry carries its document's size, counts and last change, so a table needs no
        second query per row. ``heading`` narrows to one heading, matched ignoring case, with
        ``match`` one of ``exact``, ``prefix`` or ``contains``; ``level`` caps the depth.
        """
        rows = self.backend.outline(normalize(path), heading, match, level, limit)
        return [OutlineEntry(**r) for r in rows]

    def heading_names(self, path: str = "/", starts: str = "", *, limit: int = 100) -> List[HeadingName]:
        """Distinct headings in use, most-used first — the autosuggest call for outlines."""
        return [HeadingName(**r) for r in self.backend.heading_names(normalize(path), starts, limit)]

    def property_values(self, key: str, prefix: str = "", *, limit: int = 200) -> List[PropertyValue]:
        """The values one property takes, most-used first; ``prefix`` narrows them as above."""
        return [PropertyValue(r["value"], r["docs"]) for r in self.backend.property_values(key, prefix, limit)]

    def property_find(self, query: str = "", folder: str = "/", *, limit: int = 500) -> List[PropertyHit]:
        """Documents matching a property query: ``status:draft tags:telco -priority:>3``.

        ``key:value`` equals, ``has:key`` exists, ``key:>3`` compares, ``key:val*`` starts
        with, ``key:~val`` contains, ``key:!=val`` has it but not as that. A space means AND;
        ``OR``, ``NOT`` (or a leading ``-``) and parentheses work as written. An empty query
        lists every document that has front matter.
        """
        out = []
        for r in self.backend.property_find(query, normalize(folder), limit):
            data = r["frontmatter"]
            if isinstance(data, str):
                # A row whose JSON will not parse is reported as having no front matter
                # rather than failing the whole search.
                try:
                    data = json.loads(data)
                except ValueError:
                    data = None
            out.append(PropertyHit(r["path"], r["nbytes"], r["updated_at"], data if isinstance(data, dict) else None))
        return out

    def checkpoint(self, name: str) -> int:
        return self.backend.checkpoint(name)

    # -------------------------------------------------------------------- loaders
    def load_file(self, local_path: Union[str, os.PathLike], dest: Optional[str] = None, *, author: Optional[str] = None) -> int:
        """Load one local file. `dest` defaults to '/<file name>'. Unchanged content makes no
        new version, so re-loading is idempotent. Returns the version."""
        p = Path(local_path)
        dest = normalize(dest or f"/{p.name}")
        data = p.read_bytes()
        return self.write(dest, data, author=author or self.author or "load")

    def load_folder(self, local_dir: Union[str, os.PathLike], dest_prefix: str = "/", *,
                    include: Sequence[str] = DEFAULT_INCLUDE, exclude: Sequence[str] = (".git/*", ".*", "node_modules/*"),
                    skip_binary: bool = True, author: Optional[str] = None,
                    on_file: Optional[Callable[[str, str], None]] = None) -> LoadStats:
        """Load every matching file under `local_dir` to `dest_prefix/<relative path>`.
        Idempotent: re-running loads only changed files (unchanged ones make no version).
        `include`/`exclude` are glob patterns matched against the relative POSIX path."""
        root = Path(local_dir)
        dest_prefix = normalize(dest_prefix)
        stats = LoadStats()
        a = author or self.author or "load"
        for path in sorted(root.rglob("*")):
            if not path.is_file():
                continue
            rel = path.relative_to(root).as_posix()
            if any(fnmatch.fnmatch(rel, pat) or fnmatch.fnmatch(path.name, pat) for pat in exclude):
                continue
            if include and not any(fnmatch.fnmatch(rel, pat) or fnmatch.fnmatch(path.name, pat) for pat in include):
                stats.skipped += 1
                continue
            data = path.read_bytes()
            if skip_binary and (b"\0" in data[:8192]):
                stats.skipped += 1
                continue
            dest = normalize(f"{dest_prefix}/{rel}")
            stats.files += 1
            try:
                if self.backend.exists(dest):
                    before = self.backend.read(dest)[1]
                    v = self.backend.update(dest, data, None, a)
                    if v == before:
                        stats.unchanged += 1
                    else:
                        stats.updated += 1
                else:
                    self.backend.create(dest, data, a)
                    stats.created += 1
                if on_file:
                    on_file(rel, dest)
            except TextdbError as e:
                stats.errors += 1
                if on_file:
                    on_file(rel, f"ERROR {e.code}: {e}")
        return stats

    def export_folder(self, prefix: str, local_dir: Union[str, os.PathLike]) -> int:
        """Write every file under `prefix` to `local_dir/<relative path>`. Returns the count."""
        root = Path(local_dir)
        prefix = normalize(prefix)
        n = 0
        for path, content in self.backend.export(prefix):
            rel = path[len(prefix):].lstrip("/") if prefix != "/" else path.lstrip("/")
            target = root / rel
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(content)
            n += 1
        return n
