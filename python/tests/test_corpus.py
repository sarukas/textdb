"""Runs against every backend that is available:
  - SQLite when the loadable extension can be found (TEXTDB_SQLITE_EXT or target/release)
  - Postgres when TEXTDB_TEST_PG (a URL to a database with `CREATE EXTENSION textdb_pg`) is set
"""
import os
import uuid

import pytest

from textdb import Conflict, Corpus, InvalidEdit, NotFound, TextdbError
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


def _note(title, status, tags, priority, extra=""):
    return (
        f"---\ntitle: {title}\nstatus: {status}\ntags: [{', '.join(tags)}]\n"
        f"priority: {priority}\nproject:\n  name: atlas\n---\n{extra}\n# {title}\n\nbody\n"
    )


@pytest.fixture
def vault(kb):
    """Four documents under this test's own root.

    The Postgres fixture shares one database between tests, so everything here is scoped to
    `P(kb, ...)` and every query names that folder — otherwise these assertions would count
    whatever else happens to be in the database.
    """
    kb.write(P(kb, "/a.md"), _note("A", "draft", ["cvm", "telco"], 5))
    kb.write(P(kb, "/b.md"), _note("B", "review", ["telco"], 2))
    kb.write(P(kb, "/c.md"), _note("C", "draft", ["cvm"], 1))
    kb.write(P(kb, "/plain.md"), "# Plain\n\nno front matter\n")
    return kb


def _paths(kb, q):
    """Matching paths, with this test's root stripped so the assertions read plainly."""
    root = getattr(kb, "_root", "")
    return [h.path[len(root):] for h in kb.property_find(q, folder=root or "/")]


def test_property_keys_count_documents_not_rows(vault):
    by_key = {k.key: k for k in vault.property_keys()}
    # Three documents carry `tags`; five rows of them, since a list is one row per element.
    # On Postgres the store is shared, so this counts at least three rather than exactly.
    assert by_key["tags"].docs >= 3
    # A UI offers `>` only where it means something.
    assert by_key["priority"].kind == "number"
    assert by_key["status"].kind == "text"
    assert "project.name" in by_key, "nested keys are dotted"


def test_property_keys_and_values_narrow_by_prefix(vault):
    # Containment, not equality: the Postgres fixture shares one database, so other tests'
    # documents can contribute keys under the same prefix.
    assert "project.name" in [k.key for k in vault.property_keys("project.")]
    values = {v.value for v in vault.property_values("status")}
    assert {"draft", "review"} <= values
    assert [v.value for v in vault.property_values("tags", "tel")] == ["telco"]


def test_property_find_handles_the_whole_grammar(vault):
    paths = lambda q: _paths(vault, q)  # noqa: E731
    assert paths("status:draft") == ["/a.md", "/c.md"]
    # A list containing a value is an ordinary equality, because lists are rows.
    assert paths("tags:telco") == ["/a.md", "/b.md"]
    assert paths("status:draft tags:telco") == ["/a.md"]
    assert len(paths("status:draft OR status:review")) == 3
    assert paths("-status:draft") == ["/b.md"]
    assert paths("NOT status:draft") == ["/b.md"]
    assert len(paths("project.name:atlas")) == 3
    # Numbers compare numerically: lexically "5" would sort below "2".
    assert paths("priority:>3") == ["/a.md"]
    assert paths("tags:c*") == ["/a.md", "/c.md"]
    assert paths("title:~B") == ["/b.md"]
    assert paths("(status:draft OR status:review) has:priority") == ["/a.md", "/b.md", "/c.md"]


def test_not_equal_means_has_it_but_not_as_that(vault):
    paths = lambda q: _paths(vault, q)  # noqa: E731
    # /plain.md has no status at all, so it is not a document whose status is not draft.
    assert paths("status:!=draft") == ["/b.md"]
    assert len(paths("status:draft")) + len(paths("status:!=draft")) == 3


def test_a_property_search_is_over_documents_that_have_properties(vault):
    hits = _paths(vault, "")
    assert len(hits) == 3
    assert "/plain.md" not in hits


def test_hits_carry_the_whole_front_matter(vault):
    root = getattr(vault, "_root", "")
    hit = vault.property_find("status:draft tags:telco", folder=root or "/")[0]
    assert hit.frontmatter["status"] == "draft"
    assert hit.frontmatter["tags"] == ["cvm", "telco"]
    assert hit.frontmatter["priority"] == 5
    assert hit.nbytes > 0


def test_the_index_follows_edits_and_deletes(vault):
    paths = lambda q: _paths(vault, q)  # noqa: E731
    vault.write(P(vault, "/c.md"), _note("C", "published", ["cvm"], 1))
    assert paths("status:draft") == ["/a.md"]
    assert paths("status:published") == ["/c.md"]
    vault.delete(P(vault, "/b.md"))
    assert paths("tags:telco") == ["/a.md"]


def test_a_malformed_query_raises_naming_where(vault):
    # The message names the offset on both backends. The *class* differs: Postgres loses a
    # custom SQLSTATE raised from a set-returning function (see `fail` in textdb-pg), so it
    # arrives as a plain TextdbError rather than InvalidEdit — a pre-existing gap that
    # kb.leaf_hashes has too, not something this query path introduced.
    with pytest.raises(TextdbError, match="ends early"):
        vault.property_find("status:draft AND")
