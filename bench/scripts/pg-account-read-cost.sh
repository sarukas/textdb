#!/bin/bash
# What one `SELECT content FROM kb.file WHERE path = $1` costs the owner and a delegated
# account, against the number of nodes in the store.
#
# The matrix answers "how much slower is the account" for a whole workload; this answers
# "why", in the smallest form that shows it. It exists because the first delegated run
# (`textdb-bench run --as-account`) came back 2.3x on Postgres against 0.96x on SQLite,
# which is not a difference two engines should have for the same algorithm.
#
# The shape to look for is in the last column. A constant ratio is a fixed cost per call.
# A ratio that grows with the node count is a scan, and the plan says which:
#
#   psql "$URL" -c "EXPLAIN (ANALYZE) EXECUTE q('/probe.md')"
#
# Usage: bench/scripts/pg-account-read-cost.sh [URL]
# Default URL is the throwaway cluster from bench/scripts/pg-start.sh.
set -euo pipefail

U="${1:-postgres://postgres@localhost:54329/postgres}"
SHARE=/bench

q() { psql "$U" -tAq -c "$1"; }

q "DROP EXTENSION IF EXISTS textdb_pg CASCADE; DROP SCHEMA IF EXISTS kb CASCADE; CREATE EXTENSION textdb_pg;" >/dev/null
q "INSERT INTO kb.folder (path) VALUES ('$SHARE')" >/dev/null
q "SELECT kb.account_create('probe','agent','$SHARE')" >/dev/null
q "INSERT INTO kb.file (path, content) VALUES ('$SHARE/probe.md', repeat('x ', 512))" >/dev/null
BEARER=$(q "SELECT bearer FROM kb.token_create('probe','probe')")

# The document read is the same one at every size, so the only thing changing is how many
# other nodes the store holds. `force_generic_plan` keeps re-planning out of the numbers.
timed() { # who auth path reps
  local auth="$2" path="$3" reps="$4"
  {
    echo "SET plan_cache_mode = force_generic_plan;"
    [ -n "$auth" ] && echo "$auth"
    echo "PREPARE q(text) AS SELECT length(content), version FROM kb.file WHERE path = \$1;"
    echo "EXECUTE q('$path');"   # once untimed, so nothing first-call lands in the median
    echo "\\o /dev/null"
    echo "\\timing on"
    for _ in $(seq 1 "$reps"); do echo "EXECUTE q('$path');"; done
  } | psql "$U" -q 2>&1 | grep '^Time:' | awk '{print $2}' | sort -g |
    awk '{a[NR]=$1} END {print a[int(NR/2)+1]}'
}

printf '%9s %12s %14s %10s\n' nodes owner_ms account_ms ratio
prev=0
for n in 10 100 1000 5000 20000; do
  psql "$U" -q -c "INSERT INTO kb.file (path, content)
                   SELECT '$SHARE/pad/'||i||'.md','pad' FROM generate_series($((prev + 1)),$n) i" >/dev/null
  psql "$U" -q -c "SELECT kb.analyze_store()" >/dev/null
  prev=$n
  owner=$(timed owner "" "$SHARE/probe.md" 20)
  # Reps drop for the account because a scan at 20k nodes is seconds per call, which is the
  # result rather than a reason to wait for twenty of them.
  account=$(timed account "SELECT kb.auth('$BEARER');" "/probe.md" 3)
  printf '%9d %12.3f %14.3f %9.0fx\n' "$n" "$owner" "$account" "$(echo "$account/$owner" | bc -l)"
done
