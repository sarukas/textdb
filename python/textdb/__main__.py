"""Command line: python -m textdb <url> <command> …

    textdb sqlite:///kb.db load ./notes /notes
    textdb sqlite:///kb.db load-file ./README.md /README.md
    textdb sqlite:///kb.db ls /notes
    textdb sqlite:///kb.db cat /notes/index.md [--version 3]
    textdb sqlite:///kb.db search "renewal pricing" [/notes]
    textdb sqlite:///kb.db edit /notes/index.md "old" "new"
    textdb sqlite:///kb.db append /notes/journal.md "- done"
    textdb sqlite:///kb.db history /notes/index.md
    textdb sqlite:///kb.db diff /notes/index.md 1 2
    textdb sqlite:///kb.db export /notes ./out
"""

import argparse
import sys

from . import Corpus, TextdbError


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="textdb", description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("url")
    ap.add_argument("--author", default=None)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("load"); p.add_argument("src"); p.add_argument("dest", nargs="?", default="/")
    p = sub.add_parser("load-file"); p.add_argument("src"); p.add_argument("dest", nargs="?")
    p = sub.add_parser("ls"); p.add_argument("path", nargs="?", default="/")
    p = sub.add_parser("cat"); p.add_argument("path"); p.add_argument("--version", type=int)
    p = sub.add_parser("lines"); p.add_argument("path"); p.add_argument("first", type=int); p.add_argument("last", type=int)
    p = sub.add_parser("search"); p.add_argument("query"); p.add_argument("prefix", nargs="?", default="/")
    p = sub.add_parser("edit"); p.add_argument("path"); p.add_argument("old"); p.add_argument("new")
    p = sub.add_parser("append"); p.add_argument("path"); p.add_argument("text")
    p = sub.add_parser("history"); p.add_argument("path")
    p = sub.add_parser("diff"); p.add_argument("path"); p.add_argument("v1", type=int); p.add_argument("v2", type=int)
    p = sub.add_parser("mv"); p.add_argument("src"); p.add_argument("dst")
    p = sub.add_parser("rm"); p.add_argument("path")
    p = sub.add_parser("export"); p.add_argument("prefix"); p.add_argument("dir")
    a = ap.parse_args(argv)
    try:
        with Corpus.open(a.url, author=a.author) as kb:
            if a.cmd == "load":
                print(kb.load_folder(a.src, a.dest, on_file=lambda rel, dest: print(f"  {rel} -> {dest}")))
            elif a.cmd == "load-file":
                print("version", kb.load_file(a.src, a.dest))
            elif a.cmd == "ls":
                for e in kb.ls(a.path):
                    print(f"{e.kind:6} {e.nbytes or '':>9} {e.path}")
            elif a.cmd == "cat":
                sys.stdout.write(kb.read_version(a.path, a.version) if a.version else kb.read(a.path))
            elif a.cmd == "lines":
                sys.stdout.write(kb.lines(a.path, a.first, a.last))
            elif a.cmd == "search":
                for h in kb.search(a.query, a.prefix):
                    print(f"{h.path}:{h.line}: {h.snippet}")
            elif a.cmd == "edit":
                print("version", kb.edit(a.path, a.old, a.new))
            elif a.cmd == "append":
                print("version", kb.append(a.path, a.text + ("\n" if not a.text.endswith("\n") else "")))
            elif a.cmd == "history":
                for c in kb.history(a.path):
                    print(f"v{c.version:<5} {c.ts} {c.author or ''} {c.message or ''}")
            elif a.cmd == "diff":
                sys.stdout.write(kb.diff(a.path, a.v1, a.v2))
            elif a.cmd == "mv":
                kb.move(a.src, a.dst)
            elif a.cmd == "rm":
                kb.delete(a.path)
            elif a.cmd == "export":
                print("exported", kb.export_folder(a.prefix, a.dir), "files")
    except TextdbError as e:
        print(f"{e.code}: {e}", file=sys.stderr)
        if e.payload:
            print(e.payload, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
