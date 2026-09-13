/**
 * The store's line model: a line is everything up to and including a `\n`; a final line
 * without one is still a line. The empty document has zero lines.
 */

/** Split text into lines, each keeping its trailing `\n` if present. */
export function splitLines(text: string): string[] {
  const out: string[] = [];
  let start = 0;
  for (;;) {
    const nl = text.indexOf("\n", start);
    if (nl < 0) break;
    out.push(text.slice(start, nl + 1));
    start = nl + 1;
  }
  if (start < text.length) out.push(text.slice(start));
  return out;
}

/**
 * Offsets of every line start, followed by `text.length`: line `k` (1-based) spans
 * `[starts[k-1], starts[k])`, and `starts.length - 1` is the line count.
 */
export function lineStarts(text: string): number[] {
  const starts = [0];
  let pos = 0;
  for (;;) {
    const nl = text.indexOf("\n", pos);
    if (nl < 0 || nl === text.length - 1) break;
    starts.push(nl + 1);
    pos = nl + 1;
  }
  if (text.length === 0) return [0];
  starts.push(text.length);
  return starts;
}

export function countLines(text: string): number {
  return lineStarts(text).length - 1;
}
