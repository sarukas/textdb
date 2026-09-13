import { useEffect, useMemo, useRef, useState } from "react";
import { DocController } from "../doc/controller";
import { useDocState } from "../doc/useDocState";
import { authorStyle } from "../live/color";
import { relativeTime } from "../live/time";
import type { FeedHub } from "../state/hub";
import type { OwnWrites } from "../state/ownWrites";
import { effectiveAuthor } from "../state/useAuthor";
import type { PathAction } from "../tree/actions";
import { useNow } from "../state/useNow";
import { ConflictPanel } from "./ConflictPanel";
import { Editor } from "./Editor";
import { History } from "./History";
import { Preview } from "./Preview";
import { useToast } from "./Toasts";

export type Mode = "preview" | "edit" | "history";

export interface OpenDoc {
  id: number;
  path: string;
  line: number | null;
  nonce: number;
}

interface Props {
  open: OpenDoc;
  mode: Mode;
  onMode: (m: Mode) => void;
  hub: FeedHub;
  own: OwnWrites;
  author: string;
  onPathChange: (path: string) => void;
  onAction: (action: PathAction) => void;
  onOpenFolder: (path: string) => void;
}

const MODES: Array<{ id: Mode; label: string }> = [
  { id: "preview", label: "Preview" },
  { id: "edit", label: "Edit" },
  { id: "history", label: "History" },
];

/** Mounted with `key={open.id}`: one controller per opened document. */
export function DocumentPane({ open, mode, onMode, hub, own, author, onPathChange, onAction, onOpenFolder }: Props) {
  const toast = useToast();
  const authorRef = useRef(author);
  authorRef.current = author;
  const pathChangeRef = useRef(onPathChange);
  pathChangeRef.current = onPathChange;

  const [controller, setController] = useState<DocController | null>(null);
  useEffect(() => {
    const c = new DocController(open.path, {
      hub,
      own,
      toast,
      getAuthor: () => effectiveAuthor(authorRef.current),
      onPathChange: (p) => pathChangeRef.current(p),
    });
    setController(c);
    return () => c.dispose();
    // The pane is keyed by the open id, so this runs once per document.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const state = useDocState(controller);
  const now = useNow(15_000);

  const [chunksOn, setChunksOn] = useState(false);
  useEffect(() => {
    controller?.setChunksEnabled(chunksOn && mode !== "history");
  }, [controller, chunksOn, mode, state?.status]);

  const [editorMounted, setEditorMounted] = useState(false);
  useEffect(() => {
    if (mode === "edit") setEditorMounted(true);
  }, [mode]);

  // Ctrl/Cmd+S saves from anywhere in the page.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || e.key.toLowerCase() !== "s") return;
      e.preventDefault();
      if (controller && controller.state.status === "ready") void controller.save();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [controller]);

  const focus = useMemo(() => (open.line ? { line: open.line, nonce: open.nonce } : null), [open.line, open.nonce]);

  if (!controller || !state || state.status === "loading") {
    return <div className="doc-status muted">Loading {open.path}…</div>;
  }
  if (state.status === "error") {
    return (
      <div className="doc-status">
        <div className="error-text">Could not open {state.path}</div>
        <div className="muted">{state.error}</div>
      </div>
    );
  }

  const copyMine = () => {
    const text = controller.tracker?.current.toString() ?? state.conflict?.mine ?? "";
    navigator.clipboard.writeText(text).then(
      () => toast("copied your version to the clipboard", "ok"),
      () => toast("clipboard unavailable", "error"),
    );
  };

  const behind = state.headVersion > state.version;

  return (
    <div className="doc">
      <div className="doc-bar">
        <div className="doc-title">
          <nav className="doc-path" title={state.path} aria-label="Path">
            <button type="button" className="crumb" onClick={() => onOpenFolder("/")} title="All files">
              All files
            </button>
            {state.path.split("/").filter(Boolean).map((part, i, all) =>
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
            <span className="mono" title="Version of the text you are looking at">
              v{state.version}
            </span>
            {behind && <span className="badge warn">head v{state.headVersion}</span>}
            {state.updatedBy && (
              <span className="doc-updated" title={state.updatedAt ?? ""}>
                <span className="chip" style={authorStyle(state.updatedBy)} aria-hidden="true" />
                {state.updatedBy}
                {state.updatedAt ? ` · ${relativeTime(state.updatedAt, now)}` : ""}
              </span>
            )}
            {state.dirty && (
              <span className="dirty" role="status">
                <span className="dirty-dot" aria-hidden="true" />
                unsaved
              </span>
            )}
          </span>
        </div>
        <div className="doc-actions">
          <span className="doc-file-actions">
            <button
              type="button"
              className="btn btn-ghost btn-small"
              onClick={() => onAction({ op: "download", path: state.path, kind: "file", version: state.version })}
              title={`Download v${state.version} as saved${state.dirty ? " (without your unsaved changes)" : ""}`}
            >
              Download
            </button>
            <button
              type="button"
              className="btn btn-ghost btn-small"
              onClick={() => onAction({ op: "replace", path: state.path, kind: "file" })}
              title="Upload a file from this computer as the next version"
            >
              Replace…
            </button>
            <button
              type="button"
              className="btn btn-ghost btn-small"
              onClick={() => onAction({ op: "move", path: state.path, kind: "file" })}
              title="Rename or move this file"
            >
              Rename…
            </button>
            <button
              type="button"
              className="btn btn-ghost btn-small danger"
              onClick={() => onAction({ op: "delete", path: state.path, kind: "file" })}
              title="Delete this file"
            >
              Delete…
            </button>
          </span>
          {mode !== "history" && (
            <label className="toggle" title="Show textdb's content-defined chunk boundaries">
              <input type="checkbox" checked={chunksOn} onChange={(e) => setChunksOn(e.target.checked)} />
              Chunks
            </label>
          )}
          <div className="segmented" role="tablist" aria-label="View mode">
            {MODES.map((m) => (
              <button
                key={m.id}
                type="button"
                role="tab"
                aria-selected={mode === m.id}
                onClick={() => onMode(m.id)}
              >
                {m.label}
              </button>
            ))}
          </div>
          {(mode === "edit" || state.dirty) && (
            <button
              type="button"
              className="btn btn-primary"
              onClick={() => void controller.save()}
              disabled={state.saving || (!state.dirty && state.notice?.kind !== "deleted")}
              title="Save (Ctrl/Cmd+S)"
            >
              {state.saving ? "Saving…" : "Save"}
            </button>
          )}
        </div>
      </div>

      {state.notice && (
        <div className={`notice notice-${state.notice.kind}`} role="status">
          <span>{state.notice.text}</span>
          <button type="button" className="btn btn-ghost btn-small" onClick={() => controller.dismissNotice()} aria-label="Dismiss">
            ✕
          </button>
        </div>
      )}

      {state.conflict && (
        <ConflictPanel
          conflict={state.conflict}
          busy={state.saving}
          onReloadTheirs={() => void controller.reloadTheirs()}
          onOverwrite={() => void controller.save({ overwrite: true })}
          onCopyMine={copyMine}
          onDismiss={() => controller.dismissConflict()}
        />
      )}

      <div className="doc-body">
        {mode === "preview" && <Preview controller={controller} state={state} chunksOn={chunksOn} focus={focus} />}
        {editorMounted && (
          <Editor controller={controller} state={state} visible={mode === "edit"} chunksOn={chunksOn} focus={mode === "edit" ? focus : null} />
        )}
        {mode === "history" && <History state={state} />}
      </div>
    </div>
  );
}
