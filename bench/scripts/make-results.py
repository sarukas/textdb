#!/usr/bin/env python3
"""Assemble bench/RESULTS.md from results.jsonl + manifest.json + the narrative file.

Usage: bench/scripts/make-results.py bench/out bench/RESULTS.md bench/scripts/results-narrative.md
"""
import json, subprocess, sys, pathlib

out_dir, target, narrative = map(pathlib.Path, sys.argv[1:4])
here = pathlib.Path(__file__).parent
res = out_dir / "results.jsonl"
manifest = json.loads((out_dir / "manifest.json").read_text())
rows = [json.loads(l) for l in res.open() if l.strip()]
verdict = subprocess.run([sys.executable, here / "verdict.py", res], capture_output=True, text=True, check=True).stdout
metrics = subprocess.run([sys.executable, here / "key-metrics.py", res], capture_output=True, text=True, check=True).stdout
fails = [r for r in rows if r["note"].startswith("FAIL")]
nas = sorted({(r["test"], r["backend"], r["note"][5:80]) for r in rows if r["note"].startswith("N/A")})
tests = sorted({r["test"] for r in rows})
backends = ["fs", "fs-git", "sql-text-sqlite", "sql-text-pg", "textdb-sqlite", "textdb-pg"]

doc = []
doc.append("# textdb — benchmark results (POC run)\n")
doc.append(narrative.read_text())
doc.append("\n## Claim verdicts (test spec §10)\n")
doc.append(verdict)
doc.append("\n## Key metrics\n")
doc.append("Medians over repetitions; latencies per operation; `N/A` = not available on that backend; **FAIL** = oracle violated (see the list below).\n")
doc.append(metrics)
doc.append("\n## Oracle failures\n")
if fails:
    doc.append("| test | backend | case | metric | detail |\n|---|---|---|---|---|")
    seen = set()
    for f in fails:
        k = (f["test"], f["backend"], f["case"], f["metric"])
        if k in seen: continue
        seen.add(k)
        doc.append(f"| {f['test']} | {f['backend']} | {f['case']} | {f['metric']} | {f['note'][6:120].replace('|','/')} |")
else:
    doc.append("None.")
doc.append("\n## Cells recorded as N/A\n")
doc.append("| test | backend | reason |\n|---|---|---|")
for t, b, n in nas:
    doc.append(f"| {t} | {b} | {n.replace('|','/')} |")
doc.append("\n## Run manifest\n")
doc.append("```json\n" + json.dumps(manifest, indent=2) + "\n```\n")
doc.append(f"\nTests run: {len(tests)} · rows: {len(rows)} · raw data: [`results/results.jsonl`](results/results.jsonl), full generated matrix: [`results/report.md`](results/report.md).\n")
target.write_text("\n".join(doc))
print("wrote", target, len(rows), "rows,", len(fails), "fail rows")
