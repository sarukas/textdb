import { MergeView, unifiedMergeView } from "@codemirror/merge";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { useEffect, useMemo, useRef, useState } from "react";
import { getVersionText } from "../doc/versions";
import { baseExtensions, readOnlyExtensions } from "../editor/setup";
import type { DiffLayout } from "../live/diffLayout";
import { compareVersions, describeLineEndings } from "../live/eol";

interface Props {
  path: string;
  from: number;
  to: number;
  layout: DiffLayout;
  onLayout: (l: DiffLayout) => void;
  /** Show the whole of version `to` instead of its changes. */
  onShowContent?: (() => void) | undefined;
}

export function DiffView({ path, from, to, layout, onLayout, onShowContent }: Props) {
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

  // The editor keeps "\r" as text (its offsets match the store's bytes), so a CRLF line next to
  // the same line with LF would show as changed with nothing visible: compare normalised texts
  // and say how many lines differ only in their endings.
  const comparison = useMemo(() => (texts ? compareVersions(texts[0], texts[1]) : null), [texts]);
  const endings = comparison ? describeLineEndings(comparison) : null;

  useEffect(() => {
    const el = host.current;
    if (!comparison || !el) return;
    const { a, b } = comparison;
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
  }, [comparison, layout, from, to]);

  return (
    <div className="viewer">
      <div className="viewer-bar">
        <span>
          <strong className="mono">v{from}</strong> → <strong className="mono">v{to}</strong>
          {comparison && (
            <span className="diff-stats">
              {" "}
              · <span className="added">+{comparison.added}</span> <span className="removed">−{comparison.removed}</span> lines in{" "}
              {comparison.hunks} {comparison.hunks === 1 ? "hunk" : "hunks"}
            </span>
          )}
          {endings && <span className="diff-eol"> · {endings}</span>}
        </span>
        <div className="viewer-actions">
          {onShowContent && (
            <button type="button" className="btn btn-small" onClick={onShowContent}>
              Show v{to}
            </button>
          )}
          <div className="segmented" role="group" aria-label="Diff layout">
            <button type="button" aria-pressed={layout === "split"} onClick={() => onLayout("split")}>
              Side by side
            </button>
            <button type="button" aria-pressed={layout === "unified"} onClick={() => onLayout("unified")}>
              Unified
            </button>
          </div>
        </div>
      </div>
      {error && <div className="error-text pad">{error}</div>}
      {!texts && !error && <div className="muted pad">Loading…</div>}
      {comparison && comparison.hunks === 0 && (
        <div className="muted pad">{endings ? "No changes to the text: only line endings differ." : "No differences."}</div>
      )}
      <div ref={host} className={`viewer-code diff-${layout}`} key={`${layout}:${from}:${to}`} />
    </div>
  );
}
