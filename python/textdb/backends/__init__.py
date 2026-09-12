"""Backends implement the same small protocol; `Corpus` picks one from the URL scheme."""

from .base import Backend  # noqa: F401


def open_backend(url: str, **kwargs) -> "Backend":
    scheme = url.split(":", 1)[0].lower()
    if scheme in ("sqlite", "sqlite3", "file"):
        from .sqlite import SqliteBackend

        return SqliteBackend(url, **kwargs)
    if scheme in ("postgres", "postgresql", "pg"):
        from .postgres import PostgresBackend

        return PostgresBackend(url, **kwargs)
    if url.endswith(".db") or url.endswith(".sqlite"):
        from .sqlite import SqliteBackend

        return SqliteBackend("sqlite:///" + url, **kwargs)
    raise ValueError(f"unsupported textdb URL: {url!r} (use sqlite:///file.db or postgresql://…)")
