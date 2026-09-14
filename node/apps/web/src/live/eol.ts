import { diffLines } from "./diff";
import { splitLines } from "./lines";

/** How a text ends its lines. */
export type LineEndings = "lf" | "crlf" | "mixed" | "none";

export function lineEndings(text: string): LineEndings {
  let crlf = 0;
  let lf = 0;
  for (let i = text.indexOf("\n"); i >= 0; i = text.indexOf("\n", i + 1)) {
    if (i > 0 && text[i - 1] === "\r") crlf++;
    else lf++;
  }
  if (crlf === 0 && lf === 0) return "none";
  if (crlf === 0) return "lf";
  return lf === 0 ? "crlf" : "mixed";
}

export function normalizeLineEndings(text: string): string {
  return text.replace(/\r\n/g, "\n");
}

export interface VersionComparison {
  /** The texts to show, with CRLF turned into LF. */
  a: string;
  b: string;
  hunks: number;
  added: number;
  removed: number;
  /** Lines the diff does not show as changed because only their line ending differs. */
  lineEndingOnly: number;
  endingsA: LineEndings;
  endingsB: LineEndings;
}

/**
 * Two versions as a diff view shows them. The store keeps bytes, so a file edited with LF lines
 * inside CRLF text (or converted back) differs on lines that look identical; those are counted,
 * not shown as changes. A line whose text changed is a change whatever its ending.
 */
export function compareVersions(a: string, b: string): VersionComparison {
  const na = normalizeLineEndings(a);
  const nb = normalizeLineEndings(b);
  const hunks = diffLines(na, nb);
  // Normalising keeps every "\n", so line k of a text is line k of its normalised form.
  const rawA = splitLines(a);
  const rawB = splitLines(b);
  let i = 0;
  let j = 0;
  let lineEndingOnly = 0;
  const unchangedUntil = (x: number) => {
    for (; i < x; i++, j++) if (rawA[i] !== rawB[j]) lineEndingOnly++;
  };
  for (const h of hunks) {
    unchangedUntil(h.old_from - 1);
    i = h.old_from - 1 + h.old_count;
    j = h.new_from - 1 + h.new_count;
  }
  unchangedUntil(rawA.length);
  return {
    a: na,
    b: nb,
    hunks: hunks.length,
    added: hunks.reduce((n, h) => n + h.new_count, 0),
    removed: hunks.reduce((n, h) => n + h.old_count, 0),
    lineEndingOnly,
    endingsA: lineEndings(a),
    endingsB: lineEndings(b),
  };
}

const NAMES: Record<LineEndings, string> = { lf: "LF", crlf: "CRLF", mixed: "mixed", none: "none" };

/** `"3 lines differ only in line endings (mixed → CRLF)"`, or `null` when none do. */
export function describeLineEndings(c: VersionComparison): string | null {
  if (c.lineEndingOnly === 0) return null;
  const lines = c.lineEndingOnly === 1 ? "1 line differs" : `${c.lineEndingOnly} lines differ`;
  const how = c.endingsA === c.endingsB ? "" : ` (${NAMES[c.endingsA]} → ${NAMES[c.endingsB]})`;
  return `${lines} only in line endings${how}`;
}
