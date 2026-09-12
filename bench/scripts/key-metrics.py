#!/usr/bin/env python3
"""Print compact markdown tables of the headline metrics per claim from results.jsonl.

Usage: bench/scripts/key-metrics.py bench/out/results.jsonl
"""
import json, sys, statistics
from collections import defaultdict

rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
backends = ["fs", "fs-git", "sql-text-sqlite", "sql-text-pg", "textdb-sqlite", "textdb-pg"]
backends = [b for b in backends if any(r["backend"] == b for r in rows)]
vals = defaultdict(list); notes = defaultdict(list)
for r in rows:
    k = (r["test"], r["backend"], r["case"], r["metric"])
    if r["value"] is not None and not r["note"].startswith("FAIL"):
        vals[k].append(r["value"])
    if r["note"] not in ("ok", ""):
        notes[k].append(r["note"])

def v(test, b, case, metric):
    xs = vals.get((test, b, case, metric)); return statistics.median(xs) if xs else None

def cell(test, b, case, metric, kind):
    x = v(test, b, case, metric)
    k = (test, b, case, metric)
    if x is None:
        n = notes.get(k) or notes.get((test, b, "", "na")) or notes.get((test, b, case, "na"))
        if n:
            n0 = n[0]
            return "**FAIL**" if n0.startswith("FAIL") else "N/A"
        return "–"
    if kind == "us":
        return f"{x/1000:.2f} ms" if x >= 1000 else f"{x:.0f} µs"
    if kind == "bytes":
        return f"{x/1e6:.1f} MB" if x >= 1e6 else (f"{x/1e3:.1f} KB" if x >= 1e3 else f"{x:.0f} B")
    if kind == "int":
        return f"{x:,.0f}"
    if kind == "x":
        return f"{x:,.1f}×"
    if kind == "pct":
        return f"{100*x:.1f} %"
    return f"{x:.3g}"

def table(title, spec):
    print(f"\n### {title}\n")
    print("| cell | " + " | ".join(backends) + " |")
    print("|---|" + "---|" * len(backends))
    for label, test, case, metric, kind in spec:
        print(f"| {label} | " + " | ".join(cell(test, b, case, metric, kind) for b in backends) + " |")

xl_cases = sorted({r["case"].split("@")[0] for r in rows if r["test"].startswith("XL") and r["case"]})
table("Claim 1 — O(edit) writes: write amplification (bytes written / bytes changed) and latency of a 3-line replace", [
    *[(f"XL-04 replace @50 % of {c}: write amplification", "XL-01..06", f"{c}@50%", "replace_write_amplification", "x") for c in xl_cases],
    *[(f"XL-04 replace @50 % of {c}: p50", "XL-01..06", f"{c}@50%", "replace_p50_us", "us") for c in xl_cases],
    *[(f"XL-05 footprint growth per edit ({c})", "XL-01..06", c, "footprint_growth_per_edit", "bytes") for c in xl_cases],
    ("LL-02 replace 10 B in a 1 MiB single line: write amplification", "LL-01..03", "1MiB", "replace_write_amplification", "x"),
    ("LL-02 textdb leaves changed (1 MiB line)", "LL-01..03", "1MiB", "leaves_changed", "int"),
    ("LL-04 JSON 10 MiB: leaves unchanged per edit (min fraction)", "LL-04", "", "leaves_unchanged_frac_min", "pct"),
    ("ME-04 alternate ends: leaves changed per edit (max)", "ME-04", "", "leaves_changed_max", "int"),
    ("ME-01 counter ×2000 on 100 KiB: replace p50", "ME-01/02", "", "replace_p50_us", "us"),
    ("ME-01 write amplification", "ME-01/02", "", "write_amplification", "x"),
    ("ME-01 footprint / raw after 2000 edits", "ME-01/02", "", "footprint_over_raw", "x"),
    ("ME-01 textdb chunk bytes (content layer only)", "ME-01/02", "", "chunk_bytes", "bytes"),
])

table("Claim 2 — conflict rate and lost updates under concurrent writers (base = last seen version)", [
    *[(f"{t} N={n}: {m}", t, f"N={n}", m, "int")
      for t in ("CW-01", "CW-02", "CW-03", "ME-06") for n in ((20,) if t == "ME-06" else (5, 20, 50)) for m in ("committed_direct", "committed_rebased", "absorbed_identical", "conflict", "contention", "error", "lost_updates")],
    *[(f"{t} N=20: write p99", t, "N=20", "write_p99_us", "us") for t in ("CW-01", "CW-02", "CW-03", "ME-06")],
    *[(f"{t} N=20: throughput ops/s", t, "N=20", "throughput_ops_s", "int") for t in ("CW-01", "CW-02", "CW-03", "ME-06")],
    ("CW-04 Zipf files N=20: conflict", "CW-04", "N=20", "conflict", "int"),
    ("CW-04 Zipf files N=20: committed_rebased", "CW-04", "N=20", "committed_rebased", "int"),
    ("CW-05 append N=20: lost updates", "CW-05", "N=20", "lost_updates", "int"),
    ("CW-05 append N=20: append p50", "CW-05", "N=20", "write_p50_us", "us"),
    ("CW-06 rename race N=20: error (write failed by rename)", "CW-06", "N=20", "error", "int"),
    ("CW-06 rename race N=20: lost updates", "CW-06", "N=20", "lost_updates", "int"),
    ("CW-07 fs-git 20 s: contention (index.lock)", "CW-07", "N=20", "contention", "int"),
])

table("Claim 3 — insert-only index: footprint growth per edit with search index, maintenance", [
    ("SR-04 footprint growth per edit (2000 edits, 2000 × 4 KiB docs)", "SR-04", "", "footprint_growth_per_edit", "bytes"),
    ("SR-04 bytes written for the edits", "SR-04", "", "bytes_written_edits", "bytes"),
    ("SR-04 maintenance time", "SR-04", "", "maintenance_s", "s"),
    ("SR-01 import 3000 × 4 KiB", "SR-01/02", "", "import_s", "s"),
    ("SR-01 footprint / raw after import", "SR-01/02", "", "footprint_over_raw", "x"),
    ("FP-01 footprint / raw after 4000 Zipf edits on 2000 files", "FP-01/02", "after_4000", "footprint_over_raw", "x"),
    ("FP-02 footprint / raw after maintenance", "FP-01/02", "", "footprint_after_maintenance_over_raw", "x"),
    ("FP-02 maintenance time", "FP-01/02", "", "maintenance_s", "s"),
])

table("Search — latency and recall/precision vs the reference tokenizer (3000 docs)", [
    *[(f"SR-02 {q}: p50", "SR-01/02", q, "search_p50_us", "us") for q in ("single", "and2", "phrase", "prefix")],
    *[(f"SR-02 {q}: recall", "SR-01/02", q, "recall", "pct") for q in ("single", "and2", "phrase", "prefix")],
    *[(f"SR-02 {q}: precision", "SR-01/02", q, "precision", "pct") for q in ("single", "and2", "phrase", "prefix")],
    ("SR-05 prefix-restricted AND: p50", "SR-05", "and2", "search_p50_us", "us"),
    ("CR-05 20 concurrent searchers: throughput", "CR-05", "N=20", "throughput_ops_s", "int"),
])

table("Reads — full and fragment reads, history", [
    *[(f"XL-02 full read {c}: p50", "XL-01..06", c, "read_p50_us", "us") for c in xl_cases],
    *[(f"XL-02 full read {c}: MB/s", "XL-01..06", c, "read_MBps", "int") for c in xl_cases],
    *[(f"XL-03 read_lines 50 @50 % of {c}: p50", "XL-01..06", f"{c}@50%", "read_lines_p50_us", "us") for c in xl_cases],
    *[(f"XL-06 read_version(v1) after edits ({c})", "XL-01..06", c, "read_version_v1_p50_us", "us") for c in xl_cases],
    ("CR-01 N=1 read 100 KiB: p50", "CR-01", "N=1", "read_p50_us", "us"),
    ("CR-01 N=100 read 100 KiB: throughput", "CR-01", "N=100", "throughput_ops_s", "int"),
    ("CR-01 N=100 read 100 KiB: p99", "CR-01", "N=100", "read_p99_us", "us"),
    ("CR-03 N=100 readers + writer: reader p99", "CR-03", "N=100", "read_p99_us", "us"),
    ("CR-03 N=100 torn reads", "CR-03", "N=100", "torn_reads", "int"),
    ("CR-04 N=100 read_lines on 32 MiB: throughput", "CR-04", "N=100", "throughput_ops_s", "int"),
    ("CR-04 N=100 read_lines on 32 MiB: p50", "CR-04", "N=100", "read_lines_p50_us", "us"),
    ("CR-06 N=50 read_version (200 versions): p50", "CR-06", "N=50", "read_version_p50_us", "us"),
    ("ME-02 read_version p50 after 2000 versions", "ME-01/02", "", "read_version_p50_us", "us"),
    ("ME-05 history p50 after 3000 versions", "ME-05", "", "history_p50_us", "us"),
])

table("Namespace and round trip", [
    ("RT-01 all sizes identical (1 = pass)", "RT-01", "0B/lf/mixed", "identical", "int"),
    ("RT-03 random bytes 1 MiB identical", "RT-03", "1MiB/lf/random_bytes", "identical", "int"),
    ("RT-04 300 random edits: versions identical", "RT-04", "", "versions_identical", "int"),
    ("RT-05 all historical versions identical", "RT-05", "", "versions_identical", "int"),
    ("NS-01 create p99 among 5000 files in one folder", "NS-01", "", "create_last100_p99_us", "us"),
    ("NS-01 list 5000 entries", "NS-01", "", "list_p50_us", "us"),
    ("NS-02 depth 1000 create", "NS-02", "", "create_p50_us", "us"),
    ("NS-03 rename folder with 5000 descendants", "NS-03", "5000", "rename_p50_us", "us"),
    ("NS-03 versions preserved", "NS-03", "5000", "versions_preserved", "int"),
    ("NS-04 read_version after folder delete", "NS-04", "", "read_version_after_delete", "int"),
    ("NS-05 255-byte name identical", "NS-05", "255byte_name", "identical", "int"),
    ("NS-05 4 KiB path identical", "NS-05", "4KiB_path", "identical", "int"),
    ("LL-06 UTF-8 intact after multibyte replace", "LL-06", "8MiB", "utf8_intact", "int"),
])
