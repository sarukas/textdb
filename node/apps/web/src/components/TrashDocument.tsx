import { useEffect, useMemo, useState } from "react";
import { api, ApiError, type HistoryEntry, type PathEvent, type TrashEntry, type TrashFile } from "../api";
import { md } from "../doc/markdown";
import { downloadText } from "../doc/transfer";
import { formatBytes } from "../import/select";
import { authorStyle } from "../live/color";
import { relativeTime } from "../live/time";
import { describePathEvent, timeline } from "../live/timeline";
import type { FeedHub } from "../state/hub";
import { useNow } from "../state/useNow";

type View = "preview" | "source" | "history";

const VIEWS: Array<{ id: View; label: string }> = [
  { id: "preview", label: "Preview" },
  { id: "source", label: "Source" },
  { id: "history", label: "History" },
];

const MARKDOWN = /\.(md|markdown|mdx)$/i;

interface Props {
  id: number;
  hub: FeedHub;
  onPurge: (entry: TrashEntry) => void;
  onClose: () => void;
}

/** A deleted file, read-only, at any of its versions, until it is permanently removed. */
export function TrashDocument({ id, hub, onPurge, onClose }: Props) {
  /** null: the version it was deleted with. */
  const [version, setVersion] = useState<number | null>(null);
  const [file, setFile] = useState<TrashFile | null>(null);
  const [history, setHistory] = useState<HistoryEntry[]>([]);
  const [events, setEvents] = useState<PathEvent[]>([]);
  const [failure, setFailure] = useState<{ gone: boolean; message: string } | null>(null);
  const [view, setView] = useState<View>("preview");
  const [rev, setRev] = useState(0);
  const now = useNow(15_000);

  useEffect(() => {
    let live = true;
    Promise.all([api.trashFile(id, version ?? undefined), api.trashHistory(id), api.pathHistory({ id }).catch(() => [])]).then(
      ([f, h, moves]) => {
        if (!live) return;
        setFile(f);
        setHistory(h);
        setEvents(moves);
        setFailure(null);
      },
      (e: unknown) => {
        if (!live) return;
        setFailure({ gone: e instanceof ApiError && e.status === 404, message: e instanceof Error ? e.message : String(e) });
      },
    );
    return () => {
      live = false;
    };
  }, [id, version, rev]);

  // A purge made elsewhere may take this file, or the folder it was deleted with.
  useEffect(
    () =>
      hub.events.on((e) => {
        if (e.op === "purge") setRev((r) => r + 1);
      }),
    [hub],
  );

  const html = useMemo(() => (file && MARKDOWN.test(file.entry.name) ? md.render(file.content) : null), [file]);

  if (failure) {
    return (
      <div className="doc-status">
        {failure.gone ? (
          <div>This file was permanently removed from the trash.</div>
        ) : (
          <>
            <div className="error-text">Could not open trash entry {id}</div>
            <div className="muted">{failure.message}</div>
          </>
        )}
        <p>
          <button type="button" className="btn" onClick={onClose}>
            Close
          </button>
        </p>
      </div>
    );
  }
  if (!file) return <div className="doc-status muted">Loading…</div>;

  const { entry } = file;
  const parts = entry.path.split("/").filter(Boolean);
  const source = <pre className="trash-source">{file.content}</pre>;

  return (
    <div className="doc">
      <div className="doc-bar">
        <div className="doc-title">
          <span className="doc-path" title={entry.path}>
            {parts.map((part, i) => (
              <span key={i} className={i === parts.length - 1 ? "crumb last" : "crumb"}>
                {part}
              </span>
            ))}
          </span>
          <span className="doc-meta">
            <span className="badge trash-badge">In trash</span>
            <span className="mono" title="Version you are looking at">
              v{file.version}
            </span>
            {file.version !== entry.version && <span className="badge warn">deleted at v{entry.version}</span>}
            <span className="doc-updated" title={entry.deleted_at}>
              deleted
              {entry.deleted_by && (
                <>
                  {" "}
                  by <span className="chip" style={authorStyle(entry.deleted_by)} aria-hidden="true" />
                  {entry.deleted_by}
                </>
              )}
              {` · ${relativeTime(entry.deleted_at, now)}`}
            </span>
          </span>
        </div>
        <div className="doc-actions">
          <button
            type="button"
            className="btn btn-ghost btn-small"
            onClick={() => downloadText(entry.name, file.content)}
            title={`Download v${file.version}`}
          >
            Download
          </button>
          <button type="button" className="btn btn-ghost btn-small danger" onClick={() => onPurge(entry)}>
            Permanently remove…
          </button>
          <div className="segmented" role="tablist" aria-label="View">
            {VIEWS.map((v) => (
              <button key={v.id} type="button" role="tab" aria-selected={view === v.id} onClick={() => setView(v.id)}>
                {v.label}
              </button>
            ))}
          </div>
          <button type="button" className="btn btn-ghost btn-small" onClick={onClose} aria-label="Close">
            ✕
          </button>
        </div>
      </div>

      <div className="notice notice-trash" role="note">
        <span>Read-only. A deleted file keeps every version here until it is permanently removed.</span>
      </div>

      <div className="doc-body">
        {view === "preview" &&
          (html === null ? (
            source
          ) : (
            <div className="preview-scroll">
              <article className="preview" aria-label={`Preview of ${entry.path}`}>
                <div className="markdown md-html" dangerouslySetInnerHTML={{ __html: html }} />
              </article>
            </div>
          ))}
        {view === "source" && source}
        {view === "history" && (
          <ol className="trash-history" aria-label="Versions">
            {timeline(history, events)
              .reverse()
              .map((item) => {
                if (item.kind === "path") {
                  const e = item.event;
                  return (
                    <li
                      key={`path-${e.id}`}
                      className={`path-row path-${e.op}`}
                      title={e.new_path ? `${e.old_path} → ${e.new_path}` : e.old_path}
                    >
                      <span className="path-glyph" aria-hidden="true">
                        {e.op === "delete" ? "−" : "→"}
                      </span>
                      <span className="path-what">{describePathEvent(e)}</span>
                      <span className="chip" style={authorStyle(e.author ?? "unknown")} aria-hidden="true" />
                      <span>{e.author ?? "unknown"}</span>
                      <span className="muted trash-when" title={e.ts}>
                        {relativeTime(e.ts, now)}
                      </span>
                    </li>
                  );
                }
                const h = item.entry;
                return (
                  <li key={h.version}>
                    <button
                      type="button"
                      className={`trash-version${h.version === file.version ? " current" : ""}`}
                      onClick={() => {
                        setVersion(h.version === entry.version ? null : h.version);
                        setView("preview");
                      }}
                    >
                      <span className="mono">v{h.version}</span>
                      <span className="chip" style={authorStyle(h.author ?? "unknown")} aria-hidden="true" />
                      <span>{h.author ?? "unknown"}</span>
                      {h.kind && h.kind !== "direct" && <span className={`badge kind-${h.kind}`}>{h.kind}</span>}
                      {h.message && <span className="muted">{h.message}</span>}
                      <span className="muted trash-when" title={h.ts}>
                        {h.nbytes !== null ? `${formatBytes(h.nbytes)} · ` : ""}
                        {relativeTime(h.ts, now)}
                      </span>
                    </button>
                  </li>
                );
              })}
          </ol>
        )}
      </div>
    </div>
  );
}
