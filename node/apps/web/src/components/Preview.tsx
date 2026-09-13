import type { ChangeSet, Text } from "@codemirror/state";
import { memo, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { DocController, DocState, FlashInfo } from "../doc/controller";
import { md } from "../doc/markdown";
import { blockAtLine, blocksTouching, renderBlocks, type Block } from "../live/blocks";

interface Props {
  controller: DocController;
  state: DocState;
  chunksOn: boolean;
  focus: { line: number; nonce: number } | null;
}

interface Rendered {
  doc: Text;
  blocks: Block[];
}

interface Anchor {
  key: string;
  offset: number;
  /** Start of the anchor block in the previous document, with the change that followed. */
  pos: number;
  changes: ChangeSet | null;
}

interface Band {
  top: number;
  height: number;
  ord: number;
  hash: string;
  fresh: boolean;
}

const FLASH_MS = 4200;

const BlockView = memo(function BlockView({ block, flash }: { block: Block; flash: FlashInfo | undefined }) {
  return (
    <div className="md-block" data-key={block.key} data-line={block.start + 1}>
      <div className="md-html" dangerouslySetInnerHTML={{ __html: block.html }} />
      {flash && (
        <div key={flash.id} className="md-flash" style={{ "--h": String(flash.hue) } as React.CSSProperties} aria-hidden="true">
          <span className="md-flash-label">{flash.label}</span>
        </div>
      )}
    </div>
  );
});

function render(doc: Text): Rendered {
  return { doc, blocks: renderBlocks(md, doc.toString()) };
}

export function Preview({ controller, state, chunksOn, focus }: Props) {
  const scroller = useRef<HTMLDivElement>(null);
  const content = useRef<HTMLDivElement>(null);
  const [view, setView] = useState<Rendered>(() => render(controller.tracker!.current));
  const viewRef = useRef(view);
  viewRef.current = view;
  const [flashes, setFlashes] = useState<ReadonlyMap<string, FlashInfo>>(() => new Map());
  const anchor = useRef<Anchor | null>(null);
  const [bands, setBands] = useState<Band[]>([]);
  const [width, setWidth] = useState(0);

  useEffect(() => {
    const capture = (changes: ChangeSet | null) => {
      const sc = scroller.current;
      const ct = content.current;
      anchor.current = null;
      if (!sc || !ct || sc.scrollTop <= 0) return;
      const top = sc.getBoundingClientRect().top;
      const kids = ct.children;
      let lo = 0;
      let hi = kids.length - 1;
      let found = -1;
      while (lo <= hi) {
        const mid = (lo + hi) >> 1;
        const r = (kids[mid] as HTMLElement).getBoundingClientRect();
        if (r.bottom > top) {
          found = mid;
          hi = mid - 1;
        } else {
          lo = mid + 1;
        }
      }
      if (found < 0) return;
      const el = kids[found] as HTMLElement;
      const block = viewRef.current.blocks[found];
      if (!block || el.dataset.key !== block.key) return;
      const doc = viewRef.current.doc;
      anchor.current = {
        key: block.key,
        offset: el.getBoundingClientRect().top - top,
        pos: doc.line(Math.min(block.start + 1, doc.lines)).from,
        changes,
      };
    };

    const timers = new Set<ReturnType<typeof setTimeout>>();
    const offRemote = controller.remote.on(({ applied, flash }) => {
      capture(applied.changes);
      const next = render(applied.doc);
      setView(next);
      if (!flash) return;
      const keys = blocksTouching(next.blocks, applied.spans).map((i) => next.blocks[i]!.key);
      if (!keys.length) return;
      setFlashes((prev) => {
        const m = new Map(prev);
        for (const k of keys) m.set(k, flash);
        return m;
      });
      const t = setTimeout(() => {
        timers.delete(t);
        setFlashes((prev) => new Map([...prev].filter(([, f]) => f.id !== flash.id)));
      }, FLASH_MS);
      timers.add(t);
    });
    const offReset = controller.reset.on((doc) => {
      capture(null);
      setView(render(doc));
    });
    return () => {
      offRemote();
      offReset();
      timers.forEach(clearTimeout);
    };
  }, [controller]);

  // Keep the first visible block where it was.
  useLayoutEffect(() => {
    const a = anchor.current;
    const sc = scroller.current;
    const ct = content.current;
    anchor.current = null;
    if (!a || !sc || !ct) return;
    let el = ct.querySelector<HTMLElement>(`[data-key="${a.key}"]`);
    if (!el && a.changes) {
      const pos = a.changes.mapPos(a.pos, 1);
      const line = view.doc.lineAt(Math.min(pos, view.doc.length)).number;
      el = (ct.children[blockAtLine(view.blocks, line)] as HTMLElement | undefined) ?? null;
    }
    if (!el) return;
    const delta = el.getBoundingClientRect().top - sc.getBoundingClientRect().top - a.offset;
    if (Math.abs(delta) >= 1) sc.scrollTop += delta;
  }, [view]);

  // Jump to a line (search hit).
  useEffect(() => {
    if (!focus) return;
    const ct = content.current;
    const el = ct?.children[blockAtLine(viewRef.current.blocks, focus.line)] as HTMLElement | undefined;
    if (!el) return;
    el.scrollIntoView({ block: "center" });
    el.classList.remove("md-target");
    void el.offsetWidth;
    el.classList.add("md-target");
  }, [focus]);

  useEffect(() => {
    const ct = content.current;
    if (!ct) return;
    const ro = new ResizeObserver(() => setWidth(ct.clientWidth));
    ro.observe(ct);
    return () => ro.disconnect();
  }, []);

  // Chunk bands, positioned by interpolating chunk lines within the rendered blocks.
  useLayoutEffect(() => {
    const ct = content.current;
    const marks = chunksOn ? controller.chunkMarks() : null;
    if (!chunksOn) {
      setBands([]);
      return;
    }
    if (!ct || !marks) return;
    const { blocks, doc } = view;
    const kids = ct.children;
    const yOf = (line0: number): number => {
      if (!blocks.length) return 0;
      const idx = blockAtLine(blocks, line0 + 1);
      const b = blocks[idx]!;
      const el = kids[idx] as HTMLElement | undefined;
      if (!el) return 0;
      const top = el.offsetTop;
      const h = el.offsetHeight;
      if (line0 < b.start) return top;
      if (line0 < b.end) return top + (h * (line0 - b.start)) / Math.max(1, b.end - b.start);
      const next = blocks[idx + 1];
      const nextEl = kids[idx + 1] as HTMLElement | undefined;
      const nextTop = nextEl ? nextEl.offsetTop : ct.scrollHeight;
      const gapLines = (next ? next.start : doc.lines) - b.end;
      return top + h + ((nextTop - top - h) * (line0 - b.end)) / Math.max(1, gapLines);
    };
    const out: Band[] = marks.map((m, i) => {
      const from = doc.lineAt(Math.min(m.pos, doc.length)).number - 1;
      const nextMark = marks[i + 1];
      const to = nextMark ? doc.lineAt(Math.min(nextMark.pos, doc.length)).number - 1 : doc.lines;
      const top = yOf(from);
      const bottom = nextMark ? yOf(to) : ct.scrollHeight;
      return { top, height: Math.max(2, bottom - top), ord: m.ord, hash: m.hash, fresh: m.fresh };
    });
    setBands(out);
  }, [controller, view, chunksOn, state.chunks, state.chunkVersion, state.freshChunks, width]);

  return (
    <div ref={scroller} className={`preview-scroll${chunksOn ? " with-chunks" : ""}`}>
      <article className="preview" aria-label={`Preview of ${state.path}`}>
        {chunksOn && (
          <div className="chunk-bands" aria-hidden="true">
            {bands.map((b) => (
              <div
                key={`${b.ord}:${b.hash}`}
                className={`chunk-band ${b.ord % 2 ? "odd" : "even"}${b.fresh ? " fresh" : ""}`}
                style={{ top: b.top, height: b.height }}
                title={`chunk ${b.ord} · ${b.hash.slice(0, 12)}`}
              />
            ))}
          </div>
        )}
        <div ref={content} className="markdown">
          {view.blocks.map((b) => (
            <BlockView key={b.key} block={b} flash={flashes.get(b.key)} />
          ))}
        </div>
        {view.blocks.length === 0 && <p className="muted">This document is empty.</p>}
      </article>
    </div>
  );
}
