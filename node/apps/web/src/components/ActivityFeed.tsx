import { useEffect, useReducer, useState } from "react";
import type { ChangeEvent, ChangeOp } from "../api";
import { authorStyle } from "../live/color";
import { baseName, parentOf } from "../live/paths";
import { relativeTime } from "../live/time";
import type { OwnWrites } from "../state/ownWrites";
import { effectiveAuthor } from "../state/useAuthor";
import { useNow } from "../state/useNow";

interface Props {
  events: ChangeEvent[];
  author: string;
  own: OwnWrites;
  open: boolean;
  onToggle: () => void;
  onOpen: (path: string) => void;
}

const OP: Record<ChangeOp, { glyph: string; label: string }> = {
  create: { glyph: "+", label: "created" },
  commit: { glyph: "~", label: "edited" },
  mkdir: { glyph: "▣", label: "folder created" },
  move: { glyph: "→", label: "moved" },
  delete: { glyph: "−", label: "deleted" },
};

export function ActivityFeed({ events, author, own, open, onToggle, onOpen }: Props) {
  const [othersOnly, setOthersOnly] = useState(false);
  const [, rerender] = useReducer((n: number) => n + 1, 0);
  useEffect(() => own.changed.on(rerender), [own]);
  const now = useNow(10_000);
  const me = effectiveAuthor(author);

  const isOwn = (e: ChangeEvent) => own.has(e.path, e.version) || e.author === me;
  const list = othersOnly ? events.filter((e) => !isOwn(e)) : events;

  if (!open) {
    return (
      <div className="activity collapsed">
        <button type="button" className="rail-button" onClick={onToggle} aria-expanded="false" aria-label="Show activity feed">
          <span className="rail-label">Activity</span>
          {events.length > 0 && <span className="rail-count">{Math.min(events.length, 99)}</span>}
        </button>
      </div>
    );
  }

  return (
    <div className="activity">
      <div className="activity-bar">
        <strong>Activity</strong>
        <label className="toggle">
          <input type="checkbox" checked={othersOnly} onChange={(e) => setOthersOnly(e.target.checked)} />
          Others only
        </label>
        <button type="button" className="btn btn-ghost btn-small" onClick={onToggle} aria-expanded="true" aria-label="Hide activity feed">
          ⟩
        </button>
      </div>
      {list.length === 0 ? (
        <div className="empty small">Waiting for changes…</div>
      ) : (
        <ol className="activity-list" aria-label="Recent changes" aria-live="off">
          {list.map((e) => {
            const op = OP[e.op] ?? { glyph: "·", label: e.op };
            const openable = e.node_kind === "file" && e.op !== "delete";
            const who = e.author ?? "unknown";
            return (
              <li key={e.seq}>
                <button
                  type="button"
                  className={`activity-row${isOwn(e) ? " own" : ""}`}
                  onClick={() => openable && onOpen(e.path)}
                  disabled={!openable}
                  title={e.message ? `${e.path}\n${e.message}` : e.path}
                >
                  <span className={`op op-${e.op}`} aria-label={op.label} role="img">
                    {op.glyph}
                  </span>
                  <span className="activity-main">
                    <span className="activity-path">
                      <span className="activity-dir">{parentOf(e.path) === "/" ? "" : parentOf(e.path) + "/"}</span>
                      <span className="activity-name">{baseName(e.path)}</span>
                    </span>
                    <span className="activity-meta">
                      <span className="chip" style={authorStyle(who)} aria-hidden="true" />
                      <span className="author-name" style={authorStyle(who)}>
                        {who}
                      </span>
                      {e.version !== null && <span className="mono">v{e.version}</span>}
                      {e.commit_kind && e.op === "commit" && <span className={`badge kind-${e.commit_kind}`}>{e.commit_kind}</span>}
                      {e.op === "move" && e.old_path && <span className="muted">from {baseName(e.old_path)}</span>}
                      <span className="activity-time" title={e.ts}>
                        {relativeTime(e.ts, now)}
                      </span>
                    </span>
                  </span>
                </button>
              </li>
            );
          })}
        </ol>
      )}
    </div>
  );
}
