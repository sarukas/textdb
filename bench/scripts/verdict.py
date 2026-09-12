#!/usr/bin/env python3
"""Compute the claim verdicts (test spec §10) from results.jsonl and print a markdown table.

Usage: bench/scripts/verdict.py bench/out/results.jsonl
"""
import json, sys, statistics
from collections import defaultdict

rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
# (test, backend, case, metric) -> median value / notes
vals = defaultdict(list); notes = defaultdict(set)
for r in rows:
    k = (r["test"], r["backend"], r["case"], r["metric"])
    if r["value"] is not None and not r["note"].startswith("FAIL"):
        vals[k].append(r["value"])
    if r["note"] and r["note"] != "ok":
        notes[k].add(r["note"])

def v(test, backend, case, metric):
    xs = vals.get((test, backend, case, metric))
    return statistics.median(xs) if xs else None

def fmt(x):
    if x is None: return "–"
    if abs(x) >= 100: return f"{x:,.0f}"
    if abs(x) >= 1: return f"{x:.2f}"
    return f"{x:.3f}"

backends = sorted({r["backend"] for r in rows})
tdb = [b for b in backends if b.startswith("textdb")]
out = []
out.append("| Claim | Deciding cells | textdb result | baseline result | Verdict |")
out.append("|---|---|---|---|---|")

# Claim 1: write amplification flat across sizes (XL-04 replace at 50 %), LL-02, ME-04 leaves changed
def wa(test, backend, case):
    return v(test, backend, case, "replace_write_amplification")
xl_cases = sorted({r["case"] for r in rows if r["test"].startswith("XL") and r["case"].endswith("@50%")})
c1_t = "; ".join(f"{b}: " + ", ".join(f"{c.split('@')[0]}={fmt(wa('XL-01..06', b, c))}" for c in xl_cases) for b in tdb)
c1_b = "; ".join(f"{b}: " + ", ".join(f"{c.split('@')[0]}={fmt(wa('XL-01..06', b, c))}" for c in xl_cases) for b in backends if b not in tdb)
me04 = "; ".join(f"{b}: leaves_changed_max={fmt(v('ME-04', b, '', 'leaves_changed_max'))}" for b in tdb)
ok1 = all((wa('XL-01..06', b, c) or 1e9) <= 10 for b in tdb for c in xl_cases) and all((v('ME-04', b, '', 'leaves_changed_max') or 99) <= 4 for b in tdb)
out.append(f"| 1 — O(edit) writes | XL-04 (write amplification at 50 %), ME-04 (leaves changed) | {c1_t}; {me04} | {c1_b} | {'PASS' if ok1 else 'FAIL'} |")

# Claim 2: conflict rate at N=20 on CW-01 / CW-02 vs sql-text; zero lost updates
def conf(test, b): return v(test, b, "N=20", "conflict_rate")
def lost(test, b): return v(test, b, "N=20", "lost_updates")
c2_t = "; ".join(f"{b}: CW-01 conflict={fmt(conf('CW-01', b))} lost={fmt(lost('CW-01', b))}, CW-02 conflict={fmt(conf('CW-02', b))} lost={fmt(lost('CW-02', b))}, CW-03 conflict={fmt(conf('CW-03', b))}" for b in tdb)
sq = [b for b in backends if b.startswith("sql-text")]
c2_b = "; ".join(f"{b}: CW-01 conflict={fmt(conf('CW-01', b))}, CW-02 conflict={fmt(conf('CW-02', b))}, CW-03 conflict={fmt(conf('CW-03', b))}" for b in sq)
ok2 = True
for b in tdb:
    base = [b2 for b2 in sq if b2.endswith(b.split('-')[1])]
    for t in ("CW-01", "CW-02"):
        ct, cb = conf(t, b), (conf(t, base[0]) if base else None)
        if ct is None or cb is None: ok2 = False; continue
        if ct > 0.1 * cb + 1e-9: ok2 = False
        if (lost(t, b) or 0) > 0: ok2 = False
out.append(f"| 2 — conflict rate | CW-01, CW-02 at N=20 (conflict ≤ 0.1 × sql-text, zero lost updates) | {c2_t} | {c2_b} | {'PASS' if ok2 else 'FAIL'} |")

# Claim 3: index growth per edit (SR-04)
c3_t = "; ".join(f"{b}: growth/edit={fmt(v('SR-04', b, '', 'footprint_growth_per_edit'))} B" for b in tdb)
c3_b = "; ".join(f"{b}: growth/edit={fmt(v('SR-04', b, '', 'footprint_growth_per_edit'))} B" for b in sq)
ok3 = all((v('SR-04', b, '', 'footprint_growth_per_edit') or 1e12) < 0.5 * min([v('SR-04', s, '', 'footprint_growth_per_edit') or 1e12 for s in sq] or [1e12]) for b in tdb)
out.append(f"| 3 — insert-only index | SR-04 footprint growth per edit (textdb ≈ chunk, sql-text ≈ document) | {c3_t} | {c3_b} | {'PASS' if ok3 else 'FAIL'} |")

# Claim 4: RT-06, NS-03, NS-04 on textdb-pg with no FAIL
def fails(test, b): return [r for r in rows if r["test"] == test and r["backend"] == b and r["note"].startswith("FAIL")]
c4 = "; ".join(f"{t}: {'no failures' if not fails(t, 'textdb-pg') else str(len(fails(t, 'textdb-pg'))) + ' FAIL'}" for t in ("RT-06", "NS-03", "NS-04"))
ok4 = "textdb-pg" in backends and not any(fails(t, "textdb-pg") for t in ("RT-06", "NS-03", "NS-04"))
out.append(f"| 4 — SQL surface | RT-06, NS-03, NS-04 through kb.file / kb.folder views on textdb-pg | {c4} | – | {'PASS' if ok4 else 'FAIL / not run'} |")

# Not worse: XL-02 read p50 ≤ 2 × fs; CR-01 N=1 throughput; CR-04 read_lines p50 ≤ fs
def xl_read(b, c): return v('XL-01..06', b, c, 'read_p50_us')
xl_sizes = sorted({r["case"] for r in rows if r["test"].startswith("XL") and "@" not in r["case"] and r["case"]})
nw_t = "; ".join(f"{b}: " + ", ".join(f"{c} read={fmt((xl_read(b, c) or 0)/max(xl_read('fs', c) or 1, 1))}×fs" for c in xl_sizes) + f", CR-01 N=1 {fmt((v('CR-01', b, 'N=1', 'read_p50_us') or 0)/max(v('CR-01', 'fs', 'N=1', 'read_p50_us') or 1, 1))}×fs, CR-04 N=10 read_lines {fmt((v('CR-04', b, 'N=10', 'read_lines_p50_us') or 0)/max(v('CR-04', 'fs', 'N=10', 'read_lines_p50_us') or 1, 1))}×fs" for b in tdb)
ok5 = all(((xl_read(b, c) or 1e12) <= 2 * (xl_read('fs', c) or 0)) for b in tdb for c in xl_sizes) and all((v('CR-04', b, 'N=10', 'read_lines_p50_us') or 1e12) <= (v('CR-04', 'fs', 'N=10', 'read_lines_p50_us') or 0) for b in tdb)
out.append(f"| Not worse where it shouldn't be | XL-02 full read ≤ 2 × fs, CR-04 fragment read ≤ 1 × fs | {nw_t} | fs = 1.0 | {'PASS' if ok5 else 'FAIL'} |")
print("\n".join(out))
