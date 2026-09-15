import { useCallback, useEffect, useState } from "react";
import { api, assetFileUrl, type AssetItem, type HistoryEntry, type SyncLink } from "../api";
import { actionFor, assetPath, isPointer, previewKind, stateLabel } from "../assets/model";
import { formatBytes } from "../import/select";
import { authorStyle } from "../live/color";
import { relativeTime } from "../live/time";
import type { FeedHub } from "../state/hub";
import { effectiveAuthor } from "../state/useAuthor";
import { useNow } from "../state/useNow";
import type { PathAction } from "../tree/actions";
import { useToast } from "./Toasts";

interface Props {
  /** The pointer document, `NAME.tdbasset`. */
  pointer: string;
  /** The synced folder the asset is in: its file is on the server's disk there, or can be pulled there. */
  link: SyncLink;
  hub: FeedHub;
  author: string;
  onAction: (action: PathAction) => void;
  onOpenFolder: (path: string) => void;
}

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

/**
 * An asset, opened through its pointer: what state its file on the server's disk is in, a preview
 * of it where a browser can show one, pull and push, and the versions of its pointer.
 */
export function AssetPane({ pointer, link, hub, author, onAction, onOpenFolder }: Props) {
  const toast = useToast();
  const now = useNow(30_000);
  const asset = assetPath(pointer);
  const name = asset.slice(asset.lastIndexOf("/") + 1);
  const [item, setItem] = useState<AssetItem | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [history, setHistory] = useState<HistoryEntry[]>([]);
  const [busy, setBusy] = useState<"pull" | "push" | null>(null);
  // Bumped after a pull, a push or a change of the pointer: state, preview and versions load again.
  const [rev, setRev] = useState(0);

  const load = useCallback(() => {
    const ctl = new AbortController();
    api.assets(link.prefix, asset, ctl.signal).then(
      (s) => {
        setItem(s.assets.find((a) => a.path === asset) ?? null);
        setError(null);
      },
      (e: unknown) => {
        if (!ctl.signal.aborted) setError(message(e));
      },
    );
    api.history(pointer).then(
      (h) => setHistory([...h].reverse()),
      () => setHistory([]),
    );
    return () => ctl.abort();
  }, [link.prefix, asset, pointer]);
  useEffect(() => load(), [load, rev]);

  useEffect(
    () =>
      hub.events.on((e) => {
        if (e.path === pointer || e.old_path === pointer) setRev((n) => n + 1);
      }),
    [hub, pointer],
  );

  const run = async (op: "pull" | "push") => {
    setBusy(op);
    try {
      const who = effectiveAuthor(author);
      if (op === "pull") {
        const r = await api.pullAssets({ prefix: link.prefix, paths: [asset], author: who });
        const problems = [...r.failed, ...r.kept];
        if (problems.length) toast(problems.join("; "), "error");
        else toast(r.pulled.length ? `pulled ${name}` : `${name}: nothing to pull`, "ok");
      } else {
        const r = await api.pushAssets({ prefix: link.prefix, paths: [asset], author: who });
        const problems = [...r.failed, ...r.conflicts];
        if (problems.length) toast(problems.join("; "), "error");
        else toast(r.pushed.length ? `pushed ${name}` : `${name}: nothing to push`, "ok");
      }
    } catch (e) {
      toast(message(e), "error");
    } finally {
      setBusy(null);
      setRev((n) => n + 1);
    }
  };

  const label = item ? stateLabel(item.state) : null;
  const action = item ? actionFor(item) : null;
  const kind = item ? previewKind(item.type) : "download";
  const src = `${assetFileUrl(link.prefix, asset)}&rev=${rev}`;

  const preview = () => {
    if (error) {
      return (
        <div className="empty">
          <div className="error-text">Could not look at {name}</div>
          <div className="muted">{error}</div>
        </div>
      );
    }
    if (!item) return <div className="empty muted">Loading {name}…</div>;
    if (!item.file) {
      return (
        <div className="empty">
          <p>{name} is not on the server's disk.</p>
          {action === "pull" && (
            <p>
              <button type="button" className="btn btn-primary" onClick={() => void run("pull")} disabled={busy !== null}>
                {busy === "pull" ? "Pulling…" : "Pull it"}
              </button>
            </p>
          )}
        </div>
      );
    }
    switch (kind) {
      case "image":
        return <img className="asset-image" src={src} alt={name} />;
      case "pdf":
        return <iframe className="asset-pdf" src={src} title={name} />;
      case "audio":
        return <audio className="asset-audio" controls src={src} />;
      case "video":
        return <video className="asset-video" controls src={src} />;
      default:
        return (
          <div className="empty">
            <p>No preview for {item.type}.</p>
            <p>
              <a className="btn" href={assetFileUrl(link.prefix, asset, true)} download={name}>
                Download
              </a>
            </p>
          </div>
        );
    }
  };

  return (
    <div className="doc asset-pane">
      <div className="doc-bar">
        <div className="doc-title">
          <nav className="doc-path" title={asset} aria-label="Path">
            <button type="button" className="crumb" onClick={() => onOpenFolder("/")} title="All files">
              All files
            </button>
            {asset.split("/").filter(Boolean).map((part, i, all) =>
              i === all.length - 1 ? (
                <span key={i} className="crumb last" aria-current="page">
                  {part}
                </span>
              ) : (
                <button
                  key={i}
                  type="button"
                  className="crumb"
                  onClick={() => onOpenFolder(`/${all.slice(0, i + 1).join("/")}`)}
                  title={`Open /${all.slice(0, i + 1).join("/")}`}
                >
                  {part}
                </button>
              ),
            )}
          </nav>
          <span className="doc-meta">
            {label && (
              <span className={`badge asset-${label.tone}`} title={item?.note ?? label.hint}>
                {label.label}
              </span>
            )}
            {item && <span className="mono">{item.type}</span>}
            {item?.size !== undefined && <span>{formatBytes(item.size)}</span>}
            {item?.store && <span className="muted" title="The asset store its bytes are kept in">{item.store}</span>}
          </span>
        </div>
        <div className="doc-actions">
          <span className="doc-file-actions">
            {item?.file && (
              <a className="btn btn-ghost btn-small" href={assetFileUrl(link.prefix, asset, true)} download={name} title="Download the file">
                Download
              </a>
            )}
            <button
              type="button"
              className="btn btn-ghost btn-small"
              onClick={() => onAction({ op: "move", path: pointer, kind: "file" })}
              title="Rename or move the asset (its pointer; the next sync moves the file)"
            >
              Rename…
            </button>
            <button
              type="button"
              className="btn btn-ghost btn-small danger"
              onClick={() => onAction({ op: "delete", path: pointer, kind: "file" })}
              title="Delete the asset's pointer (the next sync moves the file to the trash)"
            >
              Delete…
            </button>
            <button type="button" className="btn btn-ghost btn-small" onClick={() => setRev((n) => n + 1)} title="Look at the file again">
              Refresh
            </button>
          </span>
          {action && (
            <button
              type="button"
              className="btn btn-primary"
              onClick={() => void run(action)}
              disabled={busy !== null}
              title={label?.hint}
            >
              {busy === action ? (action === "pull" ? "Pulling…" : "Pushing…") : action === "pull" ? "Pull" : "Push"}
            </button>
          )}
        </div>
      </div>

      {item?.note && (
        <div className="notice" role="status">
          <span>{item.note}</span>
        </div>
      )}

      <div className="doc-body asset-body">
        <div className="asset-view">{preview()}</div>
        {history.length > 0 && isPointer(pointer) && (
          <ol className="asset-history" aria-label="Versions">
            {history.map((h) => (
              <li key={h.version}>
                <span className="mono">v{h.version}</span>
                {h.author && (
                  <span className="asset-history-author">
                    <span className="chip" style={authorStyle(h.author)} aria-hidden="true" />
                    {h.author}
                  </span>
                )}
                <span className="muted" title={h.ts}>
                  {relativeTime(h.ts, now)}
                </span>
                {h.message && <span className="asset-history-message">{h.message}</span>}
              </li>
            ))}
          </ol>
        )}
      </div>
    </div>
  );
}
