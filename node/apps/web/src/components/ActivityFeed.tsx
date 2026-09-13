import { useEffect, useReducer, useState } from "react";
import type { ChangeEvent, ChangeOp } from "../api";
import type { FeedItem, ImportGroup } from "../live/activity";
import { authorStyle } from "../live/color";
import { baseName, parentOf } from "../live/paths";
import { relativeTime } from "../live/time";
import type { OwnWrites } from "../state/ownWrites";
import { effectiveAuthor } from "../state/useAuthor";
import { useNow } from "../state/useNow";

interface Props {
  items: FeedItem[];
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
  delete: { glyph: "−", label: "moved to trash" },
  purge: { glyph: "×", label: "permanently removed" },
};

export function ActivityFeed({ items, author, own, open, onToggle, onOpen }: Props) {
  const [othersOnly, setOthersOnly] = useState(false);
  const [, rerender] = useReducer((n: number) => n + 1, 0);
  useEffect(() => own.changed.on(rerender), [own]);
  const now = useNow(10_000);
  const me = effectiveAuthor(author);

  const isOwn = (item: FeedItem) =>
    item.kind === "import" ? item.author === me : own.has(item.event.path, item.event.version) || item.event.author === me;
  const list = othersOnly ? items.filter((i) => !isOwn(i)) : items;

  if (!open) {
    return (
      <div className="activity collapsed">
        <button type="button" className="rail-button" onClick={onToggle} aria-expanded="false" aria-label="Show activity feed">
          <span className="rail-label">Activity</span>
          {items.length > 0 && <span className="rail-count">{Math.min(items.length, 99)}</span>}
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
          {list.map((item) => (
            <li key={item.key}>
              {item.kind === "import" && (item.files > 1 || item.folders > 0) ? (
                <ImportRow group={item} own={isOwn(item)} now={now} onOpen={onOpen} />
              ) : (
                <EventRow event={item.kind === "import" ? item.first : item.event} own={isOwn(item)} now={now} onOpen={onOpen} />
              )}
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}

interface RowProps {
  own: boolean;
  now: number;
  onOpen: (path: string) => void;
}

function Who({ name }: { name: string | null }) {
  const who = name ?? "unknown";
  return (
    <>
      <span className="chip" style={authorStyle(who)} aria-hidden="true" />
      <span className="author-name" style={authorStyle(who)}>
        {who}
      </span>
    </>
  );
}

function EventRow({ event: e, own, now, onOpen }: RowProps & { event: ChangeEvent }) {
  const op = OP[e.op] ?? { glyph: "·", label: e.op };
  const openable = e.node_kind === "file" && e.op !== "delete" && e.op !== "purge";
  return (
    <button
      type="button"
      className={`activity-row${own ? " own" : ""}`}
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
          <Who name={e.author} />
          {e.version !== null && <span className="mono">v{e.version}</span>}
          {e.commit_kind && e.op === "commit" && <span className={`badge kind-${e.commit_kind}`}>{e.commit_kind}</span>}
          {e.op === "move" && e.old_path && <span className="muted">from {baseName(e.old_path)}</span>}
          <span className="activity-time" title={e.ts}>
            {relativeTime(e.ts, now)}
          </span>
        </span>
      </span>
    </button>
  );
}

function ImportRow({ group, own, now, onOpen }: RowProps & { group: ImportGroup }) {
  const files = `${group.files.toLocaleString()} ${group.files === 1 ? "file" : "files"}`;
  return (
    <button
      type="button"
      className={`activity-row${own ? " own" : ""}`}
      onClick={() => onOpen(group.first.path)}
      title={`Imported ${files} into ${group.prefix} (changes #${group.firstSeq}–#${group.lastSeq})`}
    >
      <span className="op op-import" aria-label="imported" role="img">
        ⇣
      </span>
      <span className="activity-main">
        <span className="activity-path">
          <span className="activity-name">Imported {files}</span> <span className="activity-dir">into {group.prefix}</span>
        </span>
        <span className="activity-meta">
          <Who name={group.author} />
          {group.folders > 0 && (
            <span className="muted">
              {group.folders.toLocaleString()} {group.folders === 1 ? "folder" : "folders"}
            </span>
          )}
          <span className="activity-time" title={group.ts}>
            {relativeTime(group.ts, now)}
          </span>
        </span>
      </span>
    </button>
  );
}
