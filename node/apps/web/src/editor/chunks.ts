import { StateEffect, StateField } from "@codemirror/state";
import { gutter, GutterMarker } from "@codemirror/view";

/** A chunk boundary placed in the current document. */
export interface ChunkMark {
  pos: number;
  ord: number;
  hash: string;
  fresh: boolean;
}

export const setChunks = StateEffect.define<ChunkMark[] | null>();

export const chunkField = StateField.define<ChunkMark[] | null>({
  create: () => null,
  update(value, tr) {
    for (const e of tr.effects) if (e.is(setChunks)) return e.value;
    if (value && tr.docChanged) {
      return value.map((m) => ({ ...m, pos: tr.changes.mapPos(m.pos, 1) }));
    }
    return value;
  },
});

class BandMarker extends GutterMarker {
  constructor(
    readonly ord: number,
    readonly start: boolean,
    readonly fresh: boolean,
    readonly title: string,
  ) {
    super();
  }
  override eq(o: BandMarker): boolean {
    return o.ord === this.ord && o.start === this.start && o.fresh === this.fresh;
  }
  override toDOM(): Node {
    const el = document.createElement("div");
    el.className = `cm-chunk-band ${this.ord % 2 ? "odd" : "even"}${this.start ? " start" : ""}${this.fresh ? " fresh" : ""}`;
    if (this.title) el.title = this.title;
    return el;
  }
}

const spacer = new BandMarker(0, false, false, "");

function markAt(marks: ChunkMark[], pos: number): number {
  let lo = 0;
  let hi = marks.length - 1;
  let best = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (marks[mid]!.pos <= pos) {
      best = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return best;
}

/** A narrow gutter showing content-defined chunk boundaries as alternating bands. */
export const chunkGutter = gutter({
  class: "cm-chunk-gutter",
  lineMarker(view, line) {
    const marks = view.state.field(chunkField, false);
    if (!marks || marks.length === 0) return null;
    const idx = markAt(marks, line.to);
    if (idx < 0) return null;
    const m = marks[idx]!;
    const start = m.pos >= line.from && m.pos <= line.to;
    return new BandMarker(m.ord, start, m.fresh, start ? `chunk ${m.ord} · ${m.hash.slice(0, 12)}` : "");
  },
  lineMarkerChange: (u) => u.docChanged || u.startState.field(chunkField, false) !== u.state.field(chunkField, false),
  initialSpacer: () => spacer,
});
