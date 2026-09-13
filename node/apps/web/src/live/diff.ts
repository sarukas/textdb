import type { Hunk } from "./hunks";
import { splitLines } from "./lines";

/** Beyond this many edits the diff gives up and reports one replacement of the middle. */
const MAX_EDITS = 2000;

/**
 * Line diff (Myers) between two texts, as store-style hunks (1-based, texts with newlines).
 * Used where the client must reconcile a text it holds with one the server returned.
 */
export function diffLines(a: string, b: string): Hunk[] {
  const al = splitLines(a);
  const bl = splitLines(b);
  let pre = 0;
  while (pre < al.length && pre < bl.length && al[pre] === bl[pre]) pre++;
  let suf = 0;
  while (suf < al.length - pre && suf < bl.length - pre && al[al.length - 1 - suf] === bl[bl.length - 1 - suf]) suf++;
  const A = al.slice(pre, al.length - suf);
  const B = bl.slice(pre, bl.length - suf);
  if (A.length === 0 && B.length === 0) return [];

  const ids = new Map<string, number>();
  const intern = (l: string) => {
    let id = ids.get(l);
    if (id === undefined) ids.set(l, (id = ids.size));
    return id;
  };
  const matches = myers(A.map(intern), B.map(intern)) ?? [];

  const hunks: Hunk[] = [];
  let i = 0;
  let j = 0;
  const flush = (x: number, y: number) => {
    if (x > i || y > j) {
      hunks.push({
        old_from: pre + i + 1,
        old_count: x - i,
        new_from: pre + j + 1,
        new_count: y - j,
        old_text: A.slice(i, x).join(""),
        new_text: B.slice(j, y).join(""),
      });
    }
  };
  for (const [x, y] of matches) {
    flush(x, y);
    i = x + 1;
    j = y + 1;
  }
  flush(A.length, B.length);
  return hunks;
}

/** Matching index pairs, ascending; `null` when the edit distance exceeds the cap. */
function myers(a: number[], b: number[]): Array<[number, number]> | null {
  const n = a.length;
  const m = b.length;
  const max = n + m;
  const off = max + 1;
  const v = new Int32Array(2 * max + 3);
  // trace[d] holds v[-d-1 ..= d+1] as it was before step d.
  const trace: Int32Array[] = [];
  for (let d = 0; d <= Math.min(max, MAX_EDITS); d++) {
    trace.push(v.slice(off - d - 1, off + d + 2));
    for (let k = -d; k <= d; k += 2) {
      let x = k === -d || (k !== d && v[off + k - 1]! < v[off + k + 1]!) ? v[off + k + 1]! : v[off + k - 1]! + 1;
      let y = x - k;
      while (x < n && y < m && a[x] === b[y]) {
        x++;
        y++;
      }
      v[off + k] = x;
      if (x >= n && y >= m) return backtrack(trace, n, m);
    }
  }
  return null;
}

function backtrack(trace: Int32Array[], n: number, m: number): Array<[number, number]> {
  const out: Array<[number, number]> = [];
  let x = n;
  let y = m;
  for (let d = trace.length - 1; d >= 0; d--) {
    const v = trace[d]!;
    const at = (k: number) => v[k + d + 1]!;
    const k = x - y;
    const prevK = k === -d || (k !== d && at(k - 1) < at(k + 1)) ? k + 1 : k - 1;
    const prevX = d === 0 ? 0 : at(prevK);
    const prevY = d === 0 ? 0 : prevX - prevK;
    while (x > prevX && y > prevY) {
      x--;
      y--;
      out.push([x, y]);
    }
    x = prevX;
    y = prevY;
  }
  return out.reverse();
}
