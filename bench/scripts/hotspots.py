#!/usr/bin/env python3
"""Rank optimisation candidates for one backend against its baselines.

Usage: bench/scripts/hotspots.py bench/out/results.jsonl [target-backend]

`verdict.py` answers whether a claim holds and `key-metrics.py` prints the headline
tables. Neither says where work would pay off, which is a different question: an operation
can be 5x slower than the baseline and still be irrelevant because it costs 20us and runs
twice, while a 1.3x gap on the operation in every hot path is worth real effort.

So candidates are ranked by *time recoverable*, not by ratio: how long the target spends
on an operation, minus what the best baseline spends on the same work. Ratios are shown
alongside, because a large recoverable time at a ratio near 1.0 usually means the workload
is inherently expensive rather than the implementation being poor.

Cells whose timings were voided by a failed accuracy check contribute nothing, by
construction: the harness withholds their latency rows, so a broken cell cannot look fast
here either.
"""
import json
import statistics
import sys
from collections import defaultdict

BASELINES = ["fs", "sql-text-sqlite", "sql-text-pg", "fs-git"]

# Below this, a recorded operation is an immediate "not supported" return rather than
# work performed. Real I/O against SQLite or the filesystem does not complete this fast.
UNSUPPORTED_US = 5.0


def load(path):
    return [json.loads(l) for l in open(path, encoding="utf-8") if l.strip()]


def median_by(rows, pred):
    """(test, case, metric, backend) -> median value, ignoring FAIL rows."""
    acc = defaultdict(list)
    for r in rows:
        if r["value"] is None or r["note"].startswith("FAIL") or not pred(r):
            continue
        acc[(r["test"], r["case"], r["metric"], r["backend"])].append(r["value"])
    return {k: statistics.median(v) for k, v in acc.items()}


def fmt_us(x):
    if x >= 1e6:
        return f"{x / 1e6:.2f} s"
    if x >= 1e3:
        return f"{x / 1e3:.2f} ms"
    return f"{x:.0f} us"


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "bench/out/results.jsonl"
    target = sys.argv[2] if len(sys.argv) > 2 else "textdb-sqlite"
    rows = load(path)
    present = {r["backend"] for r in rows}
    baselines = [b for b in BASELINES if b in present]
    if target not in present:
        sys.exit(f"{target} not in {sorted(present)}")

    med = median_by(rows, lambda r: True)

    # ---- 1. Per-operation totals, from the op_* rows Ctx::op emits. -------------------
    # Pair each test's p50 with that test's call count before combining: summing p50s
    # across tests is meaningless, and a plain mean would weight a 3-call test the same
    # as a 200000-call one. Estimated time is the sum of p50 x calls per test, and the
    # quoted p50 is that total divided by the calls, i.e. call-weighted.
    def op_totals(backend):
        per_test = defaultdict(dict)  # (op, test) -> {"p50":…, "n":…}
        for (test, case, metric, b), val in med.items():
            if b != backend or case != "ops" or not metric.startswith("op_"):
                continue
            if metric.endswith("_p50_us"):
                per_test[(metric[len("op_"):-len("_p50_us")], test)]["p50"] = val
            elif metric.endswith("_n"):
                per_test[(metric[len("op_"):-len("_n")], test)]["n"] = val
        out = {}
        for (name, _test), v in per_test.items():
            if "p50" not in v or "n" not in v:
                continue
            acc = out.setdefault(name, {"est": 0.0, "n": 0.0})
            acc["est"] += v["p50"] * v["n"]
            acc["n"] += v["n"]
        for name, acc in out.items():
            acc["p50"] = acc["est"] / acc["n"] if acc["n"] else 0.0
        # A backend that does not implement an operation returns immediately, and an
        # instant refusal is not a baseline worth dividing by — it would make the backend
        # that cannot do the work at all look infinitely fast at it. Newer runs exclude
        # these at the source; the floor keeps older results.jsonl honest too.
        return {k: v for k, v in out.items() if v["n"] > 0 and v["p50"] >= UNSUPPORTED_US}

    tgt_ops = op_totals(target)
    base_ops = {b: op_totals(b) for b in baselines}
    print(f"# Optimisation candidates — {target}\n")
    print(f"Source: `{path}` · baselines: {', '.join(baselines) or 'none'}\n")

    print("## By operation\n")
    print("Time is `p50 x calls`, summed over the tests that used the operation — an")
    print("estimate of where the run's time went, not a measured total.\n")
    print("| operation | calls | " + target + " p50 | est. time | best baseline | ratio |")
    print("|---|---|---|---|---|---|")
    rank = []
    for name, v in tgt_ops.items():
        calls, p50, est = v["n"], v["p50"], v["est"]
        best_name, best_p50 = None, None
        for b in baselines:
            bv = base_ops[b].get(name)
            if bv and (best_p50 is None or bv["p50"] < best_p50):
                best_name, best_p50 = b, bv["p50"]
        ratio = (p50 / best_p50) if best_p50 else None
        recoverable = (p50 - best_p50) * calls if best_p50 and p50 > best_p50 else 0.0
        rank.append((recoverable, name, calls, p50, est, best_name, best_p50, ratio))
    for _, name, calls, p50, est, bn, bp, ratio in sorted(rank, reverse=True):
        r = f"{ratio:.2f}x" if ratio else "–"
        b = f"{bn} {fmt_us(bp)}" if bn else "–"
        print(f"| {name} | {calls:.0f} | {fmt_us(p50)} | {fmt_us(est)} | {b} | {r} |")

    # ---- 2. Individual cells with the most recoverable time. -------------------------
    print("\n## Slowest cells relative to baseline\n")
    print("Per test/case p99, where the target is slower than the best baseline. Ranked by")
    print("the gap, so the top rows are where a fix changes the most.\n")
    print("| test | case | metric | " + target + " | best baseline | ratio |")
    print("|---|---|---|---|---|---|")
    cells = []
    for (test, case, metric, b), val in med.items():
        if b != target or not metric.endswith("_p99_us") or case == "ops":
            continue
        best_name, best = None, None
        for ob in baselines:
            v = med.get((test, case, metric, ob))
            if v is not None and (best is None or v < best):
                best_name, best = ob, v
        if best is None or best <= 0 or val <= best:
            continue
        cells.append((val - best, test, case, metric, val, best_name, best))
    for _, test, case, metric, val, bn, best in sorted(cells, reverse=True)[:25]:
        print(f"| {test} | {case or '–'} | {metric[:-7]} | {fmt_us(val)} | {bn} {fmt_us(best)} | {val / best:.2f}x |")
    if not cells:
        print("| – | – | – | – | – | – |")

    # ---- 3. Footprint, the claim chunk sharing exists to serve. ----------------------
    print("\n## Footprint\n")
    print("| test | case | metric | " + " | ".join([target] + baselines) + " |")
    print("|---|---|---|" + "---|" * (1 + len(baselines)))
    any_fp = False
    for (test, case, metric, b), _ in sorted(med.items()):
        if b != target or "footprint" not in metric:
            continue
        any_fp = True
        vals = [med.get((test, case, metric, x)) for x in [target] + baselines]
        out = []
        for v in vals:
            if v is None:
                out.append("–")
            elif "over_raw" in metric:
                out.append(f"{v:.2f}x")
            else:
                out.append(f"{v:,.0f} B")
        print(f"| {test} | {case or '–'} | {metric} | " + " | ".join(out) + " |")
    if not any_fp:
        print("| – | – | – |" + " – |" * (1 + len(baselines)))

    voided = [r for r in rows if r["metric"] == "timings_voided"]
    if voided:
        print(f"\n> {len(voided)} cell(s) published no timings because an accuracy check failed:")
        for r in voided:
            print(f"> - {r['test']} / {r['backend']}")


if __name__ == "__main__":
    main()
