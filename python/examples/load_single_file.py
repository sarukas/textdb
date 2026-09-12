"""Load one local file into textdb, then read it back and show its history.

    python examples/load_single_file.py sqlite:///kb.db ./README.md /docs/README.md
    python examples/load_single_file.py postgresql://postgres@localhost:54329/postgres ./README.md
"""
import sys

from textdb import Corpus

url, local = sys.argv[1], sys.argv[2]
dest = sys.argv[3] if len(sys.argv) > 3 else None          # defaults to "/<file name>"

with Corpus.open(url, author="loader") as kb:
    version = kb.load_file(local, dest)                    # idempotent: same bytes → same version
    path = dest or "/" + local.rsplit("/", 1)[-1]
    print(f"{path} is at version {version}, {len(kb.read_bytes(path))} bytes")
    print("first 5 lines:\n" + kb.lines(path, 1, 5))
    for c in kb.history(path):
        print(f"  v{c.version} {c.ts} by {c.author}")
