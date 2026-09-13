import { lineStarts } from "./lines";

/**
 * A line hunk as the store reports it: lines `[old_from, old_from + old_count)` of the old
 * text (1-based) became lines `[new_from, new_from + new_count)` of the new text. A zero count
 * is an insertion or deletion in front of that line. The texts include their newlines.
 */
export interface Hunk {
  old_from: number;
  old_count: number;
  new_from: number;
  new_count: number;
  old_text: string;
  new_text: string;
}

/** Hunks do not fit the text they are applied to: the caller must resync from the store. */
export class HunkMismatchError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "HunkMismatchError";
  }
}

function sorted(hunks: readonly Hunk[]): Hunk[] {
  return [...hunks].sort((a, b) => a.old_from - b.old_from || a.old_count - b.old_count);
}

/**
 * Byte (UTF-16 unit) range each hunk replaces in `text`, validated against `old_text`.
 * Throws {@link HunkMismatchError} when a hunk does not match.
 */
export function hunkRanges(text: string, hunks: readonly Hunk[]): Array<{ from: number; to: number; insert: string }> {
  const starts = lineStarts(text);
  const count = starts.length - 1;
  const out: Array<{ from: number; to: number; insert: string }> = [];
  let floor = 0;
  for (const h of sorted(hunks)) {
    const first = h.old_from - 1;
    if (first < 0 || h.old_count < 0 || first + h.old_count > count) {
      throw new HunkMismatchError(`hunk @${h.old_from}+${h.old_count} is outside a ${count}-line text`);
    }
    const from = starts[first]!;
    const to = starts[first + h.old_count]!;
    if (from < floor) throw new HunkMismatchError(`hunk @${h.old_from} overlaps the previous one`);
    if (text.slice(from, to) !== h.old_text) {
      throw new HunkMismatchError(`hunk @${h.old_from}+${h.old_count}: old text does not match`);
    }
    out.push({ from, to, insert: h.new_text });
    floor = to;
  }
  return out;
}

/** Apply hunks computed against `text`; returns the new text. */
export function applyHunks(text: string, hunks: readonly Hunk[]): string {
  let out = "";
  let pos = 0;
  for (const r of hunkRanges(text, hunks)) {
    out += text.slice(pos, r.from) + r.insert;
    pos = r.to;
  }
  return out + text.slice(pos);
}

/** Where old line `line` (1-based) sits after the hunks; lines inside a replaced region go to its start. */
export function mapLine(line: number, hunks: readonly Hunk[]): number {
  let delta = 0;
  for (const h of sorted(hunks)) {
    if (line < h.old_from) break;
    if (line < h.old_from + h.old_count) return h.new_from;
    delta = h.new_from + h.new_count - (h.old_from + h.old_count);
  }
  return line + delta;
}

/** Sum of the hunks' text lengths, to decide whether a patch is worth applying. */
export function hunkTextSize(hunks: readonly Hunk[]): number {
  return hunks.reduce((n, h) => n + h.old_text.length + h.new_text.length, 0);
}
