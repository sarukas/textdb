"""Runs against every backend that is available:
  - SQLite when the loadable extension can be found (TEXTDB_SQLITE_EXT or target/release)
  - Postgres when TEXTDB_TEST_PG (a URL to a database with `CREATE EXTENSION textdb_pg`) is set
"""
import os
import uuid

import pytest

from textdb import Conflict, Corpus, InvalidEdit, NotFound
from textdb.backends.sqlite import find_extension

URLS = []
try:
    find_extension()
    URLS.append("sqlite")
except Exception:  # pragma: no cover
    pass
if os.environ.get("TEXTDB_TEST_PG"):
    URLS.append("postgres")


@pytest.fixture(params=URLS or ["none"])
def kb(request, tmp_path):
    if request.param == "none":
        pytest.skip("no backend available: build crates/textdb-sqlite-ext or set TEXTDB_TEST_PG")
    if request.param == "sqlite":
        c = Corpus.open(f"sqlite:///{tmp_path}/t.db", author="test")
        yield c
        c.close()
    else:
        c = Corpus.open(os.environ["TEXTDB_TEST_PG"], author="test")
        root = f"/t-{uuid.uuid4().hex[:8]}"
        c._root = root
        yield c
        try:
            c.delete(root)
        except Exception:
            pass
        c.close()


def P(kb, p):
    return getattr(kb, "_root", "") + p


def test_write_read_edit_history(kb):
    p = P(kb, "/notes/a.md")
    assert kb.write(p, "# A\n\nalpha\nbeta\ngamma\n") == 1
    assert kb.read(p) == "# A\n\nalpha\nbeta\ngamma\n"
    assert kb.write(p, "# A\n\nalpha\nbeta\ngamma\n") == 1          # identical → no version
    assert kb.edit(p, "beta", "BETA") == 2
    assert kb.lines(p, 3, 4) == "alpha\nBETA\n"
    assert kb.section(p, "A").startswith("# A")
    assert kb.append(p, "tail\n") == 3
    assert kb.read_version(p, 1).count("beta") == 1
    assert [c.version for c in kb.history(p)] == [1, 2, 3]
    assert "-beta\n+BETA\n" in kb.diff(p, 1, 2)
    assert kb.version(p) == 3
    with pytest.raises(InvalidEdit):
        kb.edit(p, "nope", "x")
    with pytest.raises(NotFound):
        kb.read(P(kb, "/missing.md"))


def test_base_version_rebase_and_conflict(kb):
    p = P(kb, "/f.md")
    kb.write(p, "line one\nline two\nline three\nline four\n")
    text, v = kb.read_versioned(p)
    assert v == 1
    kb.update(p, text.replace("line one", "LINE ONE"), base_version=1)
    kb.update(p, text.replace("line four", "LINE FOUR"), base_version=1)       # disjoint → rebased
    assert kb.read(p) == "LINE ONE\nline two\nline three\nLINE FOUR\n"
    with pytest.raises(Conflict) as ei:
        kb.update(p, text.replace("line one", "Line 1"), base_version=1)
    c = ei.value
    assert c.theirs == "LINE ONE\n" and c.base == "line one\n" and c.ours == "Line 1\n"
    assert c.current_version == 3
    # resolve from the payload
    kb.update(p, kb.read(p).replace("LINE ONE", "Line 1"), base_version=c.current_version)
    assert kb.read(p).startswith("Line 1\n")
    # identical concurrent change is absorbed (no new version)
    v = kb.version(p)
    assert kb.update(p, kb.read(p), base_version=v) == v


def test_search_move_delete_export(kb, tmp_path):
    root = P(kb, "/kb")
    for i in range(20):
        kb.write(f"{root}/d{i % 3}/doc{i}.md", f"# Doc {i}\n\nThe quick brown fox {i} jumps.\n")
    hits = kb.search("quick fox", f"{root}/d1")
    assert hits and all(h.path.startswith(f"{root}/d1/") and h.line == 3 for h in hits)
    assert [h.path for h in kb.search('"fox 7"', root)] == [f"{root}/d1/doc7.md"]
    names = sorted(e.name for e in kb.ls(root))
    assert names == ["d0", "d1", "d2"]
    kb.move(f"{root}/d1", f"{root}/moved")
    assert kb.read(f"{root}/moved/doc7.md").startswith("# Doc 7")
    assert [c.version for c in kb.history(f"{root}/moved/doc7.md")] == [1]
    n = kb.export_folder(root, tmp_path / "out")
    assert n == 20 and (tmp_path / "out" / "moved" / "doc7.md").read_text().startswith("# Doc 7")
    kb.delete(f"{root}/moved")
    assert kb.search('"fox 7"', root) == []
    assert kb.read_version(f"{root}/moved/doc7.md", 1).startswith("# Doc 7")     # history survives delete


def test_load_file_and_folder(kb, tmp_path):
    src = tmp_path / "src"
    (src / "a").mkdir(parents=True)
    (src / "index.md").write_text("# Index\n\nsee [[a/one]]\n")
    (src / "a" / "one.md").write_text("# One\n\nfirst\n")
    (src / "a" / "bin.dat").write_bytes(b"\0\1\2")
    (src / "a" / "notes.txt").write_text("plain\n")
    root = P(kb, "/imp")
    stats = kb.load_folder(src, root)
    assert (stats.created, stats.updated, stats.unchanged) == (3, 0, 0)
    assert stats.skipped >= 1                                     # binary skipped
    stats = kb.load_folder(src, root)                             # idempotent
    assert (stats.created, stats.updated, stats.unchanged) == (0, 0, 3)
    (src / "a" / "one.md").write_text("# One\n\nfirst\nsecond\n")
    stats = kb.load_folder(src, root)
    assert (stats.updated, stats.unchanged) == (1, 2)
    assert kb.version(f"{root}/a/one.md") == 2
    assert kb.load_file(src / "index.md", f"{root}/copy.md") == 1
    assert kb.read(f"{root}/copy.md").startswith("# Index")
    assert sorted(kb.list(root)) == [f"{root}/a/notes.txt", f"{root}/a/one.md", f"{root}/copy.md", f"{root}/index.md"]


def test_edit_with_retry(kb):
    p = P(kb, "/r.md")
    kb.write(p, "count: 0\n")
    for _ in range(5):
        kb.edit_with_retry(p, lambda t: t.replace(f"count: {int(t.split(':')[1])}", f"count: {int(t.split(':')[1]) + 1}"))
    assert kb.read(p) == "count: 5\n"
