"""textdb — versioned text corpora in SQL with swappable backends.

    from textdb import Corpus
    kb = Corpus.open("sqlite:///kb.db")                      # or "postgresql://user@host/db"
    kb.load_folder("./notes", "/notes")
    print(kb.read("/notes/index.md"))
    kb.edit("/notes/index.md", "TODO", "DONE")
    for hit in kb.search("renewal pricing", "/notes"):
        print(hit.path, hit.line, hit.snippet)
"""

from .corpus import SORT_KEYS, HeadingName, OutlineEntry, PropertyHit, PropertyKey, PropertyValue, Corpus, Entry, Hit, Commit, Link
from .errors import TextdbError, Conflict, Contention, NotFound, InvalidEdit

__all__ = [
    "SORT_KEYS",
    "PropertyKey",
    "PropertyValue",
    "HeadingName",
    "OutlineEntry",
    "PropertyHit",
    "Corpus", "Entry", "Hit", "Commit", "Link",
    "TextdbError", "Conflict", "Contention", "NotFound", "InvalidEdit",
]
__version__ = "0.1.0"
