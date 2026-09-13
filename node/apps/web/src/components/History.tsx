import { useEffect, useState } from "react";
import { api, type HistoryEntry, type PathEvent } from "../api";
import type { DocState } from "../doc/controller";
import { authorStyle } from "../live/color";
import { relativeTime } from "../live/time";
import { describePathEvent, timeline } from "../live/timeline";
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
  const [events, setEvents] = useState<PathEvent[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const [compare, setCompare] = useState<number[]>([]);
  const [layout, setLayout] = useState<DiffLayout>("unified");
  const now = useNow(15_000);

  useEffect(() => {
    let cancelled = false;
    const t = setTimeout(
      () =>
        // A server older than path history has no endpoint for it; versions still show.
        Promise.all([api.history(path), api.pathHistory({ path }).catch(() => [])]).then(
          ([list, moves]) => {
            if (cancelled) return;
            setEntries(list);
            setEvents(moves);
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

  const newestFirst = entries ? timeline(entries, events).reverse() : [];
  const head = entries?.at(-1)?.version ?? null;
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
        {newestFirst.map((item) => {
          if (item.kind === "path") {
            const e = item.event;
            const author = e.author ?? "unknown";
            return (
              <div key={`path-${e.id}`} role="listitem" className={`path-row path-${e.op}`} title={e.new_path ? `${e.old_path} → ${e.new_path}` : e.old_path}>
                <span className="path-glyph" aria-hidden="true">
                  {e.op === "delete" ? "−" : "→"}
                </span>
                <span className="path-what">{describePathEvent(e)}</span>
                <span className="chip" style={authorStyle(author)} aria-hidden="true" />
                <span className="version-author">{author}</span>
                <span className="version-time" title={e.ts}>
                  {relativeTime(e.ts, now)}
                </span>
              </div>
            );
          }
          const h = item.entry;
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
