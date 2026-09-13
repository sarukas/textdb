import { MergeView, unifiedMergeView } from "@codemirror/merge";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { useEffect, useMemo, useRef, useState } from "react";
import { getVersionText } from "../doc/versions";
import { baseExtensions, readOnlyExtensions } from "../editor/setup";
import { diffLines } from "../live/diff";
import type { DiffLayout } from "./History";

interface Props {
  path: string;
  from: number;
  to: number;
  layout: DiffLayout;
  onLayout: (l: DiffLayout) => void;
}

export function DiffView({ path, from, to, layout, onLayout }: Props) {
  const host = useRef<HTMLDivElement>(null);
  const [texts, setTexts] = useState<[string, string] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setTexts(null);
    setError(null);
    Promise.all([getVersionText(path, from), getVersionText(path, to)]).then(
      ([a, b]) => !cancelled && setTexts([a, b]),
      (err: unknown) => !cancelled && setError(err instanceof Error ? err.message : String(err)),
    );
    return () => {
      cancelled = true;
    };
  }, [path, from, to]);

  const stats = useMemo(() => {
    if (!texts) return null;
    const hunks = diffLines(texts[0], texts[1]);
    return {
      hunks: hunks.length,
      added: hunks.reduce((n, h) => n + h.new_count, 0),
      removed: hunks.reduce((n, h) => n + h.old_count, 0),
    };
  }, [texts]);

  useEffect(() => {
    const el = host.current;
    if (!texts || !el) return;
    const [a, b] = texts;
    const shared = [...baseExtensions(), ...readOnlyExtensions()];
    const collapse = { margin: 3, minSize: 8 };
    if (layout === "unified") {
      const view = new EditorView({
        parent: el,
        state: EditorState.create({
          doc: b,
          extensions: [
            ...shared,
            unifiedMergeView({ original: a, mergeControls: false, highlightChanges: true, gutter: true, collapseUnchanged: collapse }),
            EditorView.contentAttributes.of({ "aria-label": `Changes from v${from} to v${to}` }),
          ],
        }),
      });
      return () => view.destroy();
    }
    const mv = new MergeView({
      a: { doc: a, extensions: [...shared, EditorView.contentAttributes.of({ "aria-label": `v${from}` })] },
      b: { doc: b, extensions: [...shared, EditorView.contentAttributes.of({ "aria-label": `v${to}` })] },
      parent: el,
      highlightChanges: true,
      gutter: true,
      collapseUnchanged: collapse,
    });
    return () => mv.destroy();
  }, [texts, layout, from, to]);

  return (
    <div className="viewer">
      <div className="viewer-bar">
        <span>
          <strong className="mono">v{from}</strong> → <strong className="mono">v{to}</strong>
          {stats && (
            <span className="diff-stats">
              {" "}
              · <span className="added">+{stats.added}</span> <span className="removed">−{stats.removed}</span> lines in {stats.hunks}{" "}
              {stats.hunks === 1 ? "hunk" : "hunks"}
            </span>
          )}
        </span>
        <div className="segmented" role="group" aria-label="Diff layout">
          <button type="button" aria-pressed={layout === "unified"} onClick={() => onLayout("unified")}>
            Unified
          </button>
          <button type="button" aria-pressed={layout === "split"} onClick={() => onLayout("split")}>
            Side by side
          </button>
        </div>
      </div>
      {error && <div className="error-text pad">{error}</div>}
      {!texts && !error && <div className="muted pad">Loading…</div>}
      {stats && stats.hunks === 0 && <div className="muted pad">No differences.</div>}
      <div ref={host} className={`viewer-code diff-${layout}`} key={`${layout}:${from}:${to}`} />
    </div>
  );
}
