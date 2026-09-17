#!/bin/bash
# Per-operation cost on Postgres, for the owner and for a delegated account, on one store.
#
# The matrix says which operations are worth attention; this says whether a change to one of
# them worked, in a minute rather than the better part of an hour. Every operation runs the
# same number of times against the same corpus, owner and account back to back, so the two
# columns are comparable and so are two runs of it across a code change.
#
# Usage: bench/scripts/pg-op-cost.sh [URL] [CORPUS] [REPS]
set -euo pipefail

U="${1:-postgres://postgres@localhost:54329/postgres}"
CORPUS="${2:-2000}"
REPS="${3:-40}"
SHARE=/bench

P=(psql "$U" -q -v ON_ERROR_STOP=1)

# Backends left behind by an interrupted benchmark hold locks and burn CPU, and they do not
# announce themselves: one run of this script against 51 of them reported kb.ls at 271 ms that
# measured 1.2 ms once they were gone. Clear them before measuring anything.
psql "$U" -tAq -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                    WHERE datname = current_database() AND pid <> pg_backend_pid()" >/dev/null

"${P[@]}" -c "DROP EXTENSION IF EXISTS textdb_pg CASCADE; DROP SCHEMA IF EXISTS kb CASCADE; CREATE EXTENSION textdb_pg;" 2>/dev/null
"${P[@]}" -c "INSERT INTO kb.folder (path) VALUES ('$SHARE')" >/dev/null
"${P[@]}" -c "SELECT kb.account_create('probe','agent','$SHARE')" >/dev/null
# A markdown corpus, so the structure sidecar is on the write path the way it is in real use.
"${P[@]}" -c "INSERT INTO kb.file (path, content)
                SELECT '$SHARE/c/'||i||'.md',
                       '# Doc '||i||E'\n\nbody line one\n\nsee [other](/c/'||((i%$CORPUS)+1)||E'.md)\n\n## Notes\n\nowner-mark here\naccount-mark here\n'
                  FROM generate_series(1,$CORPUS) i" >/dev/null
"${P[@]}" -c "SELECT kb.analyze_store()" >/dev/null
BEARER=$(psql "$U" -tAq -c "SELECT bearer FROM kb.token_create('probe','probe')")

printf '%-14s %-8s %10s %10s %10s  %s\n' operation vault p50_ms mean_ms p90_ms note

# `tpl` is a printf template taking (prefix, n) three times over. The prefix is the share root
# for the owner and empty for the account, whose root *is* that folder — so both spell the same
# document and only the namespace differs.
run_op() { # label who tpl
  local label="$1" who="$2" tpl="$3" auth="" pre="$SHARE" out errs
  if [ "$who" = account ]; then auth="SELECT kb.auth('$BEARER');"; pre=""; fi
  out=$(
    {
      echo "SET plan_cache_mode = force_generic_plan;"
      [ -n "$auth" ] && echo "$auth"
      echo "\\o /dev/null"
      echo "\\timing on"
        # Two conversions in the template, two arguments: printf repeats its format when it is
      # handed more, which quietly tripled every operation's sample count.
      for i in $(seq 1 "$REPS"); do printf "$tpl\n" "$pre" "$i"; done
    } | psql "$U" -q 2>&1
  )
  # psql prints a `Time:` line for a statement that *failed* as readily as for one that
  # succeeded, so a refused operation reads as a very fast one. An account passing an author is
  # refused (TX005: a token session writes as its own account) and that is exactly how this
  # first showed up: account writes at 0.14 ms, six times quicker than the owner's.
  errs=$(grep -c '^ERROR:' <<<"$out" || true)
  if [ "$errs" -gt 0 ]; then
    printf '%-14s %-8s %10s %10s %10s  %d FAILED: %s\n' "$label" "$who" - - - "$errs" \
      "$(grep -m1 '^ERROR:' <<<"$out" | cut -c1-70)"
    return
  fi
  grep '^Time:' <<<"$out" | awk '{print $2}' | sort -g |
    awk -v l="$label" -v w="$who" '
      {a[NR] = $1; s += $1}
      END {
        # p50 and the mean, because they disagree and the disagreement is the point: a first
        # call that plans and warms the cache lands in the mean and not in the median.
        printf "%-14s %-8s %10.3f %10.3f %10.3f  n=%d\n", l, w, a[int(NR/2)+1], s/NR, a[int(NR*0.9)+1], NR
      }'
}

for who in owner account; do
  # A token session writes as its own account and refuses to be told otherwise, so the author
  # argument is the owner's alone. Same function either way; one fewer argument.
  if [ "$who" = account ]; then AP=""; else AP=", 'probe'"; fi
  run_op create       "$who" "INSERT INTO kb.file (path, content) VALUES ('%s/new-$who-%d.md', E'# New\\\\n\\\\nbody\\\\n');"
  run_op append       "$who" "SELECT kb.append('%s/c/%d.md', E'\\\\nappended line\\\\n'$AP);"
  # Each vault replaces its own marker: the owner going first would otherwise consume the
  # string the account then fails to find, and a failed edit is not a measurement.
  run_op replace      "$who" "SELECT kb.replace('%s/c/%d.md', '$who-mark here', '$who-done here', NULL::bigint$AP);"
  run_op read         "$who" "SELECT length(content) FROM kb.file WHERE path = '%s/c/%d.md';"
  run_op read_version "$who" "SELECT length(kb.content('%s/c/%d.md', 1));"
  run_op ls           "$who" "SELECT count(*) FROM kb.ls('%s/c') WHERE %d > 0;"
  run_op search       "$who" "SELECT count(*) FROM kb.search('body', '%s/c', 100) WHERE %d > 0;"
  echo
done
