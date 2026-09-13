import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { useEffect, useRef, useState } from "react";
import { getVersionText } from "../doc/versions";
import { baseExtensions, readOnlyExtensions } from "../editor/setup";

interface Props {
  path: string;
  version: number;
  isHead: boolean;
  onDiffPrevious?: (() => void) | undefined;
}

export function VersionView({ path, version, isHead, onDiffPrevious }: Props) {
  const host = useRef<HTMLDivElement>(null);
  const [text, setText] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setText(null);
    setError(null);
    getVersionText(path, version).then(
      (t) => !cancelled && setText(t),
      (err: unknown) => !cancelled && setError(err instanceof Error ? err.message : String(err)),
    );
    return () => {
      cancelled = true;
    };
  }, [path, version]);

  useEffect(() => {
    if (text === null || !host.current) return;
    const view = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: text,
        extensions: [
          ...baseExtensions(),
          ...readOnlyExtensions(),
          EditorView.contentAttributes.of({ "aria-label": `${path} at version ${version}, read-only` }),
        ],
      }),
    });
    return () => view.destroy();
  }, [text, path, version]);

  return (
    <div className="viewer">
      <div className="viewer-bar">
        <span>
          <strong className="mono">v{version}</strong> {isHead ? "· head" : ""} <span className="badge">read-only</span>
        </span>
        {onDiffPrevious && (
          <button type="button" className="btn btn-small" onClick={onDiffPrevious}>
            Diff with v{version - 1}
          </button>
        )}
      </div>
      {error && <div className="error-text pad">{error}</div>}
      {text === null && !error && <div className="muted pad">Loading…</div>}
      <div ref={host} className="viewer-code" />
    </div>
  );
}
