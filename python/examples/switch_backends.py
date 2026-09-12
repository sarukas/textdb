"""Same code, two backends. Set TEXTDB_URL to switch:

    TEXTDB_URL=sqlite:///kb.db python examples/switch_backends.py
    TEXTDB_URL=postgresql://postgres@localhost:54329/postgres python examples/switch_backends.py
"""
import os

from textdb import Corpus

url = os.environ.get("TEXTDB_URL", "sqlite:///kb.db")
with Corpus.open(url) as kb:
    print("backend:", kb.backend.name)
    kb.write("/demo/hello.md", "# Hello\n\nfrom " + kb.backend.name + "\n")
    print(kb.search("hello", "/demo"))
    print(kb.history("/demo/hello.md"))
