"""textdb — versioned text corpora in SQL with swappable backends.

    from textdb import Corpus
    kb = Corpus.open("sqlite:///kb.db")                      # or "postgresql://user@host/db"
    kb.load_folder("./notes", "/notes")
    print(kb.read("/notes/index.md"))
    kb.edit("/notes/index.md", "TODO", "DONE")
    for hit in kb.search("renewal pricing", "/notes"):
        print(hit.path, hit.line, hit.snippet)
"""

from .corpus import Corpus, Entry, Hit, Commit
from .errors import TextdbError, Conflict, Contention, NotFound, InvalidEdit

__all__ = [
    "Corpus", "Entry", "Hit", "Commit",
    "TextdbError", "Conflict", "Contention", "NotFound", "InvalidEdit",
]
__version__ = "0.1.0"
