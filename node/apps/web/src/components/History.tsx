import { useEffect, useState } from "react";
import { api, type HistoryEntry } from "../api";
import type { DocState } from "../doc/controller";
import { authorStyle } from "../live/color";
import { relativeTime } from "../live/time";
import { useNow } from "../state/useNow";
import { DiffView } from "./DiffView";
import { VersionView } from "./VersionView";

interface Props {
  state: DocState;
}

export type DiffLayout = "unified" | "split";

export function History({ state }: Props) {
  const { path, feedRev, version } = state;
  const [entries, setEntries] = useState<HistoryEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const [compare, setCompare] = useState<number[]>([]);
  const [layout, setLayout] = useState<DiffLayout>("unified");
  const now = useNow(15_000);

  useEffect(() => {
    let cancelled = false;
    const t = setTimeout(
      () =>
        api.history(path).then(
          (list) => {
            if (cancelled) return;
            setEntries(list);
            setError(null);
          },
          (err: unknown) => !cancelled && setError(err instanceof Error ? err.message : String(err)),
        ),
      entries ? 200 : 0,
    );
    return () => {
      cancelled = true;
      clearTimeout(t);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path, feedRev, version]);

  const newestFirst = entries ? [...entries].reverse() : [];
  const head = newestFirst[0]?.version ?? null;
  const shown = selected ?? head;

  const toggleCompare = (v: number) =>
    setCompare((prev) => (prev.includes(v) ? prev.filter((x) => x !== v) : [...prev.slice(-1), v]));

  const pair = compare.length === 2 ? ([Math.min(...compare), Math.max(...compare)] as const) : null;

  return (
    <div className="history">
      <div className="history-list" role="list" aria-label="Versions">
        <div className="history-hint">
          {compare.length === 1 ? "Pick one more version to compare" : "Click to view · tick two to diff"}
        </div>
        {error && <div className="error-text pad">{error}</div>}
        {!entries && !error && <div className="muted pad">Loading history…</div>}
        {newestFirst.map((h) => {
          const author = h.author ?? "unknown";
          const isShown = !pair && h.version === shown;
          const inCompare = compare.includes(h.version);
          return (
            <div key={h.version} role="listitem" className={`version-row${isShown ? " shown" : ""}${inCompare ? " compared" : ""}`}>
              <input
                type="checkbox"
                className="version-check"
                checked={inCompare}
                onChange={() => toggleCompare(h.version)}
                aria-label={`Compare version ${h.version}`}
              />
              <button
                type="button"
                className="version-main"
                onClick={() => {
                  setSelected(h.version);
                  setCompare([]);
                }}
                aria-current={isShown ? "true" : undefined}
              >
                <span className="version-top">
                  <span className="version-num mono">v{h.version}</span>
                  <span className="chip" style={authorStyle(author)} aria-hidden="true" />
                  <span className="version-author">{author}</span>
                  {h.kind && <span className={`badge kind-${h.kind}`}>{h.kind}</span>}
                  <span className="version-time" title={h.ts}>
                    {relativeTime(h.ts, now)}
                  </span>
                </span>
                <span className="version-sub">
                  {h.base_version !== null && h.base_version !== h.version - 1 && (
                    <span className="mono" title="Base version the writer started from">
                      base v{h.base_version} ·{" "}
                    </span>
                  )}
                  {h.base_version !== null && h.base_version === h.version - 1 && <span className="mono">base v{h.base_version} · </span>}
                  <span className="version-msg">{h.message || "—"}</span>
                </span>
              </button>
            </div>
          );
        })}
      </div>
      <div className="history-view">
        {pair ? (
          <DiffView path={path} from={pair[0]} to={pair[1]} layout={layout} onLayout={setLayout} />
        ) : shown !== null ? (
          <VersionView
            path={path}
            version={shown}
            isHead={shown === head}
            onDiffPrevious={shown > 1 ? () => setCompare([shown - 1, shown]) : undefined}
          />
        ) : (
          <div className="empty">No versions yet.</div>
        )}
      </div>
    </div>
  );
}
