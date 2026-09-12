#!/usr/bin/env bash
# Import a directory tree of markdown files into a textdb SQLite store and export it back,
# then compare (RT-06 / RT-07 on a real corpus). Usage: corpora/import.sh <dir> [db]
set -euo pipefail
DIR="$1"; DB="${2:-corpus.db}"
BIN="$(dirname "$0")/../target/release/textdb-corpus"
if [ ! -x "$BIN" ]; then echo "build first: cargo build --release -p textdb-bench (provides textdb-corpus)"; exit 1; fi
"$BIN" import "$DIR" "$DB"
OUT="$(mktemp -d)"
"$BIN" export "$DB" "$OUT"
diff -r "$DIR" "$OUT" && echo "round-trip: identical"
"$BIN" import "$DIR" "$DB" && "$BIN" versions "$DB"
