"""Load a folder tree of markdown/text files into textdb under a prefix, re-run to sync.

    python examples/load_folder.py sqlite:///kb.db ./notes /notes
    python examples/load_folder.py postgresql://postgres@localhost:54329/postgres ./docs /docs --include '*.md'

Only changed files create new versions, so this doubles as an incremental sync.
"""
import argparse

from textdb import Corpus

ap = argparse.ArgumentParser()
ap.add_argument("url")
ap.add_argument("folder")
ap.add_argument("prefix", nargs="?", default="/")
ap.add_argument("--include", action="append", help="glob(s), default: common text formats")
ap.add_argument("--author", default="loader")
a = ap.parse_args()

with Corpus.open(a.url, author=a.author) as kb:
    stats = kb.load_folder(
        a.folder, a.prefix,
        include=a.include or ("*.md", "*.markdown", "*.txt", "*.rst", "*.csv", "*.json", "*.yaml", "*.yml", "*.toml"),
        on_file=lambda rel, dest: print(f"  {rel} -> {dest}"),
    )
    print(stats)
    print("now stored under", a.prefix + ":")
    for e in kb.ls(a.prefix):
        print(f"  {e.kind:6} {e.nbytes or '':>8}  {e.path}")
    hits = kb.search("the", a.prefix, limit=3)
    print("sample search hits:", [(h.path, h.line) for h in hits])
