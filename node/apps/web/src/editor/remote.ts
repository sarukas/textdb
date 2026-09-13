import { Annotation, RangeSet, StateEffect, StateField, type EditorState, type Range } from "@codemirror/state";
import { Decoration, EditorView, gutter, GutterMarker, WidgetType, type DecorationSet } from "@codemirror/view";
import type { LineSpan } from "../live/changeset";

/** Marks transactions that carry a remote commit (or a reload), so they are not recorded as local edits. */
export const remoteAnnotation = Annotation.define<boolean>();

export interface FlashSpec {
  id: number;
  spans: LineSpan[];
  hue: number;
  label: string;
}

interface Flash {
  id: number;
  hue: number;
  label: string;
  /** Position ranges in the document: start of the first changed line, end of the last. */
  ranges: Array<{ from: number; to: number; deletion: boolean }>;
}

export const addFlash = StateEffect.define<FlashSpec>();
export const expireFlash = StateEffect.define<number>();

class LabelWidget extends WidgetType {
  constructor(
    readonly id: number,
    readonly label: string,
    readonly hue: number,
  ) {
    super();
  }
  override eq(other: LabelWidget): boolean {
    return other.id === this.id;
  }
  toDOM(): HTMLElement {
    const el = document.createElement("span");
    el.className = "cm-remote-label";
    el.style.setProperty("--h", String(this.hue));
    el.textContent = this.label;
    el.setAttribute("aria-hidden", "true");
    return el;
  }
  override ignoreEvent(): boolean {
    return true;
  }
}

class RemoteGutterMarker extends GutterMarker {
  constructor(
    readonly hue: number,
    readonly deletion: boolean,
  ) {
    super();
  }
  override eq(other: RemoteGutterMarker): boolean {
    return other.hue === this.hue && other.deletion === this.deletion;
  }
  override toDOM(): Node {
    const el = document.createElement("div");
    el.className = this.deletion ? "cm-remote-mark cm-remote-mark-del" : "cm-remote-mark";
    el.style.setProperty("--h", String(this.hue));
    return el;
  }
}

interface FlashState {
  flashes: Flash[];
  decorations: DecorationSet;
  markers: RangeSet<GutterMarker>;
}

function toFlash(state: EditorState, spec: FlashSpec): Flash {
  const doc = state.doc;
  const ranges = spec.spans.map((s) => {
    const first = Math.min(Math.max(s.from, 1), doc.lines);
    if (s.from === s.to) {
      const line = doc.line(first);
      return { from: line.from, to: line.from, deletion: true };
    }
    const last = Math.min(Math.max(s.to - 1, first), doc.lines);
    return { from: doc.line(first).from, to: doc.line(last).to, deletion: false };
  });
  return { id: spec.id, hue: spec.hue, label: spec.label, ranges };
}

function build(state: EditorState, flashes: Flash[]): FlashState {
  const decos: Range<Decoration>[] = [];
  const marks: Range<GutterMarker>[] = [];
  const doc = state.doc;
  for (const f of flashes) {
    const style = `--h:${f.hue}`;
    f.ranges.forEach((r, i) => {
      const firstLine = doc.lineAt(r.from);
      if (r.deletion) {
        decos.push(Decoration.line({ class: "cm-remote-deleted", attributes: { style } }).range(firstLine.from));
        marks.push(new RemoteGutterMarker(f.hue, true).range(firstLine.from));
      } else {
        const lastLine = doc.lineAt(Math.max(r.to, r.from)).number;
        for (let n = firstLine.number; n <= lastLine; n++) {
          const line = doc.line(n);
          decos.push(Decoration.line({ class: "cm-remote-line", attributes: { style } }).range(line.from));
          marks.push(new RemoteGutterMarker(f.hue, false).range(line.from));
        }
      }
      if (i === 0) {
        decos.push(Decoration.widget({ widget: new LabelWidget(f.id, f.label, f.hue), side: 1 }).range(firstLine.to));
      }
    });
  }
  return {
    flashes,
    decorations: Decoration.set(decos, true),
    markers: RangeSet.of(marks, true),
  };
}

const flashField = StateField.define<FlashState>({
  create: () => ({ flashes: [], decorations: Decoration.none, markers: RangeSet.empty }),
  update(value, tr) {
    let flashes = value.flashes;
    let changed = false;
    if (tr.docChanged && flashes.length) {
      flashes = flashes.map((f) => ({
        ...f,
        ranges: f.ranges.map((r) => ({
          from: tr.changes.mapPos(r.from, -1),
          to: tr.changes.mapPos(r.to, 1),
          deletion: r.deletion,
        })),
      }));
      changed = true;
    }
    for (const e of tr.effects) {
      if (e.is(addFlash)) {
        flashes = [...flashes, toFlash(tr.state, e.value)];
        changed = true;
      } else if (e.is(expireFlash)) {
        flashes = flashes.filter((f) => f.id !== e.value);
        changed = true;
      }
    }
    return changed ? build(tr.state, flashes) : value;
  },
  provide: (f) => EditorView.decorations.from(f, (s) => s.decorations),
});

/** Decorations for remote changes: author-coloured lines, a gutter marker and a fading label. */
export function remoteHighlights() {
  return [
    flashField,
    gutter({
      class: "cm-remote-gutter",
      markers: (view) => view.state.field(flashField).markers,
    }),
  ];
}
