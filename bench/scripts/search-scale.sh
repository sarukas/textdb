#!/bin/bash
# How search latency moves with the size of the corpus, on Postgres.
#
# The matrix measures search on one corpus. This asks the other question: what happens as the
# index grows. It matters because the two bindings retrieve differently — SQLite takes the top
# chunks from the FTS index and *then* applies the folder and visibility filters, while Postgres
# filters first and ranks everything that survives — so their curves need not have the same shape.
#
# Four queries, chosen to separate the effects:
#   common   a term in every document        — the worst case for "rank everything that matches"
#   rare     a term in a handful             — what a selective index should make flat
#   and      one common term and one rare    — the top-k-per-term intersection
#   prefixed the rare term inside one folder — retrieval versus filtering
#
# Usage: bench/scripts/search-scale.sh [URL] [SIZES...]
set -euo pipefail

U="${1:-postgres://postgres@localhost:54329/postgres}"
shift || true
SIZES=("${@:-2000 10000 40000}")
read -r -a SIZES <<<"${SIZES[*]}"

P=(psql "$U" -q -v ON_ERROR_STOP=1)
psql "$U" -tAq -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                    WHERE datname = current_database() AND pid <> pg_backend_pid()" >/dev/null

"${P[@]}" -c "DROP EXTENSION IF EXISTS textdb_pg CASCADE; DROP SCHEMA IF EXISTS kb CASCADE; CREATE EXTENSION textdb_pg;" 2>/dev/null

printf '%8s %9s %10s %10s %10s %10s %10s\n' docs chunks common_ms rare_ms and_ms prefixed_ms hits_common

timed() { # sql
  { echo "\\o /dev/null"; echo "\\timing on"
    for _ in 1 2 3 4 5; do echo "$1"; done
  } | psql "$U" -q 2>&1 | grep '^Time:' | awk '{print $2}' | sort -g |
    awk '{a[NR]=$1} END {printf "%.1f", a[int(NR/2)+1]}'
}

prev=0
for n in "${SIZES[@]}"; do
  # Every document holds the common term; one in 500 holds the rare one. Bodies are a few
  # kilobytes so each document is two or three chunks, which is what the index actually holds.
  "${P[@]}" -c "INSERT INTO kb.file (path, content)
                  SELECT '/c/'||(i/1000)||'/'||i||'.md',
                         '# Doc '||i||E'\n\n'||repeat('common filler text about systems and data. ', 40)||
                         CASE WHEN i % 500 = 0 THEN E'\n\nzarquon appears here.\n' ELSE E'\n\nnothing special.\n' END
                    FROM generate_series($((prev + 1)),$n) i" >/dev/null
  "${P[@]}" -c "SELECT kb.analyze_store()" >/dev/null
  prev=$n
  chunks=$(psql "$U" -tAq -c "SELECT count(*) FROM kb.chunk")
  hits=$(psql "$U" -tAq -c "SELECT count(*) FROM kb.search('common', '/', 100)")
  c=$(timed "SELECT count(*) FROM kb.search('common', '/', 100);")
  r=$(timed "SELECT count(*) FROM kb.search('zarquon', '/', 100);")
  a=$(timed "SELECT count(*) FROM kb.search('common zarquon', '/', 100);")
  p=$(timed "SELECT count(*) FROM kb.search('zarquon', '/c/1', 100);")
  printf '%8d %9d %10s %10s %10s %10s %10s\n' "$n" "$chunks" "$c" "$r" "$a" "$p" "$hits"
done
