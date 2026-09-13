import { useEffect, useRef, useState } from "react";
import { api, ApiError, type SyncLink, type SyncReport } from "../api";
import { effectiveAuthor } from "../state/useAuthor";

interface Props {
  link: SyncLink;
  author: string;
  onClose: () => void;
  onOpenFile: (path: string) => void;
}

type Step =
  | { kind: "planning" }
  | { kind: "review"; plan: SyncReport }
  | { kind: "applying" }
  | { kind: "done"; result: SyncReport }
  | { kind: "error"; message: string };

const plural = (n: number, one: string, many = `${one}s`) => `${n.toLocaleString()} ${n === 1 ? one : many}`;
const short = (commit: string | null | undefined) => (commit ? commit.slice(0, 7) : "");
const errorText = (e: unknown) => (e instanceof ApiError || e instanceof Error ? e.message : String(e));

/** Files that would change, on either side. */
function changeCount(r: SyncReport): number {
  const side = (s: SyncReport["to_disk"]) => s.new.length + s.changed.length + s.deleted.length;
  return side(r.to_disk) + side(r.to_textdb) + r.moved.length + r.merged.length + r.conflicts.length;
}

/** Whether the sync changes files on disk, i.e. has something a git commit could hold. */
function writesToDisk(r: SyncReport): boolean {
  return r.to_disk.new.length + r.to_disk.changed.length + r.to_disk.deleted.length + r.merged.length > 0;
}

export function SyncDialog({ link, author, onClose, onOpenFile }: Props) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const [step, setStep] = useState<Step>({ kind: "planning" });
  const [commit, setCommit] = useState(false);
  const [base, setBase] = useState("");
  const [resolving, setResolving] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const who = effectiveAuthor(author);

  const plan = async (withBase: string) => {
    setError(null);
    setStep({ kind: "planning" });
    try {
      const report = await api.sync({ prefix: link.prefix, dry_run: true, base: withBase.trim() || undefined, author: who });
      setStep({ kind: "review", plan: report });
    } catch (e) {
      setStep({ kind: "error", message: errorText(e) });
    }
  };

  useEffect(() => {
    dialogRef.current?.showModal();
    void plan("");
    // Planned once on opening; "Plan again" plans later ones.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const apply = async (planned: SyncReport) => {
    setStep({ kind: "applying" });
    try {
      const result = await api.sync({
        prefix: link.prefix,
        commit: commit && planned.git !== null,
        base: planned.base_commit,
        author: who,
      });
      setStep({ kind: "done", result });
    } catch (e) {
      setStep({ kind: "error", message: errorText(e) });
    }
  };

  const resolve = async (rel: string, keep: "textdb" | "disk") => {
    setResolving(rel);
    setError(null);
    try {
      setStep({ kind: "done", result: await api.syncResolve({ prefix: link.prefix, rel, keep, author: who }) });
    } catch (e) {
      setError(errorText(e));
    } finally {
      setResolving(null);
    }
  };

  const busy = step.kind === "planning" || step.kind === "applying" || resolving !== null;
  const openInStore = (rel: string) => {
    onOpenFile(link.prefix === "/" ? `/${rel}` : `${link.prefix}/${rel}`);
    onClose();
  };

  return (
    <dialog
      ref={dialogRef}
      className="dialog export-dialog sync-dialog"
      aria-labelledby="sync-title"
      onCancel={(e) => {
        e.preventDefault();
        if (!busy) onClose();
      }}
    >
      <div className="dialog-head">
        <h2 id="sync-title">
          Sync <span className="mono">{link.prefix}</span>
        </h2>
        <button type="button" className="btn btn-ghost btn-small" onClick={onClose} disabled={busy} aria-label="Close">
          ✕
        </button>
      </div>

      {(step.kind === "planning" || step.kind === "applying") && (
        <div className="dialog-body">
          <p role="status" aria-live="polite">
            {step.kind === "planning" ? "Comparing" : "Syncing"} {link.prefix} with <span className="mono">{link.dir}</span>…
          </p>
          <progress className="progress" aria-label={step.kind === "planning" ? "Planning the sync" : "Syncing"} />
        </div>
      )}

      {step.kind === "error" && (
        <>
          <div className="dialog-body">
            <p className="error-text" role="alert">
              {step.message}
            </p>
          </div>
          <div className="dialog-foot">
            <button type="button" className="btn" onClick={onClose}>
              Close
            </button>
            <button type="button" className="btn btn-primary" onClick={() => void plan(base)}>
              Try again
            </button>
          </div>
        </>
      )}

      {step.kind === "review" &&
        (() => {
          const r = step.plan;
          const n = changeCount(r);
          return (
            <>
              <div className="dialog-body">
                <Where report={r} />
                {r.first_sync && (
                  <p className="muted">
                    First sync: nothing is recorded for this folder yet, so a file that differs between the store and the
                    directory is a conflict.
                  </p>
                )}
                <Summary report={r} />
                <Details report={r} onOpen={openInStore} />
                <Conflicts
                  prefix={link.prefix}
                  rels={r.unresolved}
                  heading={`${plural(r.unresolved.length, "file")} still ${r.unresolved.length === 1 ? "has" : "have"} conflict markers from an earlier sync.`}
                  resolving={resolving}
                  onResolve={(rel, keep) => void resolve(rel, keep)}
                />
                {r.first_sync && r.git && r.conflicts.length > 0 && (
                  <div className="field">
                    <span>The commit the store's content came from (optional)</span>
                    <div className="sync-base">
                      <input
                        value={base}
                        onChange={(e) => setBase(e.target.value)}
                        placeholder="for example 3f9c2e1 or HEAD~3"
                        spellCheck={false}
                        aria-label="Base commit"
                      />
                      <button type="button" className="btn btn-small" onClick={() => void plan(base)} disabled={!base.trim()}>
                        Plan with it
                      </button>
                    </div>
                    <small>With it, changes made since that commit on either side are merged instead of conflicting.</small>
                  </div>
                )}
                {r.git && writesToDisk(r) && !r.stopped && (
                  <label className="toggle sync-commit">
                    <input type="checkbox" checked={commit} onChange={(e) => setCommit(e.target.checked)} />
                    Commit the files written to disk to git, with Textdb-* trailers
                  </label>
                )}
                {error && <p className="error-text">{error}</p>}
              </div>
              <div className="dialog-foot">
                <button type="button" className="btn spacer" onClick={() => void plan(base)}>
                  Plan again
                </button>
                <button type="button" className="btn" onClick={onClose}>
                  {n === 0 ? "Close" : "Cancel"}
                </button>
                {n > 0 && (
                  <button type="button" className="btn btn-primary" onClick={() => void apply(r)} disabled={r.stopped} autoFocus>
                    Sync {plural(n, "file")}
                  </button>
                )}
              </div>
            </>
          );
        })()}

      {step.kind === "done" &&
        (() => {
          const r = step.result;
          const marked = [...r.conflicts, ...r.unresolved];
          return (
            <>
              <div className="dialog-body">
                <p role="status" aria-live="polite">
                  {r.stopped ? "Nothing was synced: the names below cannot be written on this computer." : `Synced ${r.prefix}.`}
                  {r.git?.committed && (
                    <>
                      {" "}
                      Committed <span className="mono">{short(r.git.committed)}</span>
                      {r.git.branch ? ` on ${r.git.branch}` : ""}.
                    </>
                  )}
                </p>
                {r.git?.commit_error && <p className="error-text">The git commit failed: {r.git.commit_error}</p>}
                <Summary report={r} />
                <Details report={r} onOpen={openInStore} />
                <Conflicts
                  prefix={link.prefix}
                  rels={marked}
                  heading={`${plural(marked.length, "file")} ${marked.length === 1 ? "has" : "have"} conflict markers on disk. Keep one side here, or edit the file and sync again.`}
                  resolving={resolving}
                  onResolve={(rel, keep) => void resolve(rel, keep)}
                />
                {error && <p className="error-text">{error}</p>}
              </div>
              <div className="dialog-foot">
                <button type="button" className="btn spacer" onClick={() => void plan("")} disabled={busy}>
                  Sync again…
                </button>
                <button type="button" className="btn btn-primary" onClick={onClose} disabled={busy} autoFocus>
                  Close
                </button>
              </div>
            </>
          );
        })()}
    </dialog>
  );
}

function Where({ report: r }: { report: SyncReport }) {
  return (
    <p>
      With <span className="mono">{r.dir}</span>
      {r.git && (
        <>
          {" "}
          · git <span className="mono">{short(r.git.commit) || "(no commits)"}</span> on {r.git.branch ?? "a detached HEAD"}
          {r.git.clean ? "" : ", with uncommitted changes"}
        </>
      )}
      .
    </p>
  );
}

function Summary({ report: r }: { report: SyncReport }) {
  const disk = r.to_disk.new.length + r.to_disk.changed.length + r.to_disk.deleted.length;
  const store = r.to_textdb.new.length + r.to_textdb.changed.length + r.to_textdb.deleted.length + r.moved.length;
  const stats: [string, number, boolean][] = [
    ["to disk", disk, false],
    ["to textdb", store, false],
    ["merged", r.merged.length, false],
    ["conflicts", r.conflicts.length, r.conflicts.length > 0],
    ["unchanged", r.unchanged, false],
  ];
  return (
    <div className="stats">
      {stats.map(([label, value, bad]) => (
        <div key={label} className={`stat${bad ? " bad" : ""}`}>
          <b>{value.toLocaleString()}</b>
          <span>{label}</span>
        </div>
      ))}
    </div>
  );
}

type Tagged = [tag: string, path: string];

function ChangeList({ title, items, onOpen }: { title: string; items: Tagged[]; onOpen?: (rel: string) => void }) {
  if (items.length === 0) return null;
  return (
    <details className="failures">
      <summary>
        {title} ({items.length.toLocaleString()})
      </summary>
      <ul>
        {items.slice(0, 1000).map(([tag, rel]) => (
          <li key={`${tag}:${rel}`}>
            <span className={`export-change export-${tag}`}>{tag}</span>{" "}
            {onOpen && tag !== "deleted" ? (
              <button type="button" className="link-button mono" onClick={() => onOpen(rel.split(" → ").pop() ?? rel)}>
                {rel}
              </button>
            ) : (
              <span className="mono">{rel}</span>
            )}
          </li>
        ))}
        {items.length > 1000 && <li className="muted">and {(items.length - 1000).toLocaleString()} more</li>}
      </ul>
    </details>
  );
}

function Details({ report: r, onOpen }: { report: SyncReport; onOpen: (rel: string) => void }) {
  const tagged = (tag: string, list: string[]): Tagged[] => list.map((p) => [tag, p]);
  const blocking = r.problems.filter((p) => p.blocking);
  const notes = [
    ...r.kept.map((n) => ["kept", n] as const),
    ...r.skipped.map((n) => ["skipped", n] as const),
    ...r.failed.map((n) => ["failed", n] as const),
  ];
  return (
    <>
      {blocking.length > 0 && (
        <div className="export-problems" role="alert">
          <p className="error-text">
            {plural(blocking.length, "name")} cannot be written on this computer. Rename {blocking.length === 1 ? "it" : "them"} in
            the store, then sync again.
          </p>
          <ul>
            {blocking.map((p) => (
              <li key={`${p.kind}:${p.path}`} title={p.detail}>
                <span className="mono">{p.path}</span> <span className="muted">— {p.detail}</span>
              </li>
            ))}
          </ul>
        </div>
      )}
      <ChangeList
        title="To disk"
        items={[...tagged("new", r.to_disk.new), ...tagged("changed", r.to_disk.changed), ...tagged("deleted", r.to_disk.deleted)]}
      />
      <ChangeList
        title="To textdb"
        items={[
          ...tagged("new", r.to_textdb.new),
          ...tagged("changed", r.to_textdb.changed),
          ...tagged("deleted", r.to_textdb.deleted),
          ...r.moved.map((m): Tagged => ["moved", `${m.from} → ${m.to}`]),
        ]}
        onOpen={onOpen}
      />
      <ChangeList title="Merged on both sides" items={tagged("merged", r.merged)} onOpen={onOpen} />
      {r.dry_run && (
        <ChangeList title="Conflicts: markers will be written to these files on disk" items={tagged("conflict", r.conflicts)} />
      )}
      {notes.length > 0 && (
        <details className="failures" open={r.failed.length > 0}>
          <summary className={r.failed.length > 0 ? "error-text" : undefined}>Notes ({notes.length})</summary>
          <ul>
            {notes.map(([tag, n]) => (
              <li key={`${tag}:${n.path}`} title={`${n.path}: ${n.reason}`}>
                <span className={`export-change export-${tag}`}>{tag}</span> <span className="mono">{n.path}</span>{" "}
                <span className="muted">— {n.reason}</span>
              </li>
            ))}
          </ul>
        </details>
      )}
    </>
  );
}

const isMarker = (line: string) => line.startsWith("<<<<<<< ") || line.startsWith(">>>>>>> ") || /^=======\r?\n?$/.test(line);

interface ConflictsProps {
  prefix: string;
  rels: string[];
  heading: string;
  resolving: string | null;
  onResolve: (rel: string, keep: "textdb" | "disk") => void;
}

function Conflicts({ prefix, rels, heading, resolving, onResolve }: ConflictsProps) {
  const [shown, setShown] = useState<string | null>(null);
  const [text, setText] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  if (rels.length === 0) return null;

  const view = async (rel: string) => {
    if (shown === rel) {
      setShown(null);
      return;
    }
    setShown(rel);
    setText(null);
    setError(null);
    try {
      setText((await api.syncConflict(prefix, rel)).text);
    } catch (e) {
      setError(errorText(e));
    }
  };

  return (
    <div className="sync-conflicts" role="region" aria-label="Conflicts">
      <p className="error-text">{heading}</p>
      <ul>
        {rels.map((rel) => (
          <li key={rel}>
            <div className="sync-conflict-row">
              <span className="mono" title={rel}>
                {rel}
              </span>
              <span className="sync-conflict-actions">
                <button type="button" className="btn btn-ghost btn-small" onClick={() => void view(rel)} aria-expanded={shown === rel}>
                  {shown === rel ? "Hide" : "View"}
                </button>
                <button
                  type="button"
                  className="btn btn-small"
                  disabled={resolving !== null}
                  onClick={() => onResolve(rel, "textdb")}
                  title="Keep the textdb side of every conflict in this file, then sync"
                >
                  {resolving === rel ? "Resolving…" : "Keep textdb"}
                </button>
                <button
                  type="button"
                  className="btn btn-small"
                  disabled={resolving !== null}
                  onClick={() => onResolve(rel, "disk")}
                  title="Keep the disk side of every conflict in this file, then sync"
                >
                  Keep disk
                </button>
              </span>
            </div>
            {shown === rel && (
              <pre className="sync-conflict-text" aria-label={`${rel} on disk`}>
                {text === null
                  ? (error ?? "Loading…")
                  : text.split(/(?<=\n)/).map((line, i) => (
                      <span key={i} className={isMarker(line) ? "marker" : undefined}>
                        {line}
                      </span>
                    ))}
              </pre>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}
