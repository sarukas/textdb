import { ChangeSet, type Text } from "@codemirror/state";
import { HunkMismatchError, type Hunk } from "./hunks";

/** A span of lines in a document, 1-based and half-open; `from === to` marks a deletion in front of `from`. */
export interface LineSpan {
  from: number;
  to: number;
}

/** Number of lines in the store's sense (a trailing newline does not open a line). */
export function storeLineCount(doc: Text): number {
  if (doc.length === 0) return 0;
  return doc.sliceString(doc.length - 1) === "\n" ? doc.lines - 1 : doc.lines;
}

/** Offset where store line `line` (1-based) starts; one past the last line is the document end. */
export function offsetOfLine(doc: Text, line: number): number {
  return line <= doc.lines ? doc.line(line).from : doc.length;
}

/**
 * Convert hunks computed on `doc` (the base text) into a ChangeSet over it.
 * The document must use `\n` as its only line separator.
 */
export function hunksToChangeSet(doc: Text, hunks: readonly Hunk[]): ChangeSet {
  const count = storeLineCount(doc);
  const specs: Array<{ from: number; to: number; insert: string }> = [];
  let floor = 0;
  const ordered = [...hunks].sort((a, b) => a.old_from - b.old_from);
  for (const h of ordered) {
    const first = h.old_from;
    if (first < 1 || h.old_count < 0 || first - 1 + h.old_count > count) {
      throw new HunkMismatchError(`hunk @${h.old_from}+${h.old_count} is outside a ${count}-line document`);
    }
    const from = offsetOfLine(doc, first);
    const to = offsetOfLine(doc, first + h.old_count);
    if (from < floor) throw new HunkMismatchError(`hunk @${h.old_from} overlaps the previous one`);
    if (to - from !== h.old_text.length || doc.sliceString(from, to) !== h.old_text) {
      throw new HunkMismatchError(`hunk @${h.old_from}+${h.old_count}: old text does not match`);
    }
    specs.push({ from, to, insert: h.new_text });
    floor = to;
  }
  return ChangeSet.of(specs, doc.length);
}

export interface Rebased {
  /** The remote change, applicable to the current document (base + local). */
  remote: ChangeSet;
  /** The local changes, now relative to the new base (base + remote). */
  local: ChangeSet;
}

/**
 * Rebase a remote change over unsaved local changes. Both start from the same base; local
 * insertions at the same position as remote ones stay in front of them.
 */
export function rebase(local: ChangeSet, remote: ChangeSet): Rebased {
  return { remote: remote.map(local), local: local.map(remote, true) };
}

/** The lines of `doc` (the document after `changes`) that `changes` inserted into or modified. */
export function changedLineSpans(changes: ChangeSet, doc: Text): LineSpan[] {
  const spans: LineSpan[] = [];
  changes.iterChangedRanges((_fromA, _toA, fromB, toB) => {
    const first = doc.lineAt(fromB);
    let span: LineSpan;
    if (fromB === toB) {
      span = fromB === first.from ? { from: first.number, to: first.number } : { from: first.number, to: first.number + 1 };
    } else {
      const last = doc.lineAt(toB);
      const to = toB === last.from ? last.number : last.number + 1;
      span = { from: first.number, to: Math.max(to, first.number + 1) };
    }
    const prev = spans[spans.length - 1];
    if (prev && span.from <= prev.to && prev.to > prev.from && span.to > span.from) {
      prev.to = Math.max(prev.to, span.to);
    } else {
      spans.push(span);
    }
  }, true);
  return spans;
}
