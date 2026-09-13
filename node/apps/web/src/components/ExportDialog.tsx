import { useEffect, useMemo, useRef, useState } from "react";
import { api, type ExportFileInfo } from "../api";
import { planFolderExport, writeFolderExport, type DirHandle, type ExportPlan, type PlanProgress, type WriteResult } from "../export/folder";
import { checkNames, detectPlatform, PLATFORM_LABEL, type NameProblem } from "../export/names";
import { formatBytes } from "../import/select";
import { errorText } from "../import/source";

interface Props {
  /** The store folder to export. */
  path: string;
  onClose: () => void;
}

type Step =
  | { kind: "start" }
  | { kind: "planning"; target: string; progress: PlanProgress | null }
  | { kind: "review"; root: DirHandle; plan: ExportPlan }
  | { kind: "zip-review"; files: ExportFileInfo[]; problems: NameProblem[] }
  | { kind: "writing"; root: DirHandle; plan: ExportPlan; progress: WriteResult | null; stopping: boolean }
  | { kind: "done"; root: DirHandle; plan: ExportPlan; result: WriteResult };

type DirectoryPicker = (options: { mode: "readwrite"; id?: string }) => Promise<unknown>;

function folderPicker(): DirectoryPicker | null {
  const w = window as unknown as { showDirectoryPicker?: DirectoryPicker };
  return typeof w.showDirectoryPicker === "function" ? w.showDirectoryPicker.bind(window) : null;
}

const plural = (n: number, one: string, many = `${one}s`) => `${n.toLocaleString()} ${n === 1 ? one : many}`;

export function ExportDialog({ path, onClose }: Props) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const abortRef = useRef<AbortController | null>(null);
  const stopRef = useRef(false);
  const [step, setStep] = useState<Step>({ kind: "start" });
  const [error, setError] = useState<string | null>(null);
  const platform = useMemo(() => detectPlatform(), []);
  const picker = useMemo(folderPicker, []);
  const base = path === "/" ? "" : path;
  const label = path === "/" ? "the whole store" : path;

  useEffect(() => {
    dialogRef.current?.showModal();
    return () => abortRef.current?.abort();
  }, []);

  const busy = step.kind === "planning" || step.kind === "writing";

  const chooseFolder = async () => {
    if (!picker) return;
    setError(null);
    let root: DirHandle;
    try {
      root = (await picker({ mode: "readwrite", id: "textdb-export" })) as DirHandle;
    } catch (e) {
      if (!(e instanceof DOMException && e.name === "AbortError")) setError(errorText(e));
      return;
    }
    const ctl = new AbortController();
    abortRef.current = ctl;
    setStep({ kind: "planning", target: root.name, progress: null });
    try {
      const { files } = await api.exportFiles(path, ctl.signal);
      const plan = await planFolderExport({
        root,
        files,
        platform,
        storeHashes: async (rels) => {
          const { hashes } = await api.exportHashes(
            rels.map((r) => `${base}/${r}`),
            ctl.signal,
          );
          return new Map(hashes.map((h) => [h.path.slice(base.length + 1), h.sha256]));
        },
        onProgress: (progress) => setStep({ kind: "planning", target: root.name, progress }),
        signal: ctl.signal,
      });
      setStep({ kind: "review", root, plan });
    } catch (e) {
      if (!ctl.signal.aborted) setError(errorText(e));
      setStep({ kind: "start" });
    }
  };

  const checkZip = async () => {
    setError(null);
    const ctl = new AbortController();
    abortRef.current = ctl;
    setStep({ kind: "planning", target: "a zip file", progress: null });
    try {
      const { files } = await api.exportFiles(path, ctl.signal);
      setStep({ kind: "zip-review", files, problems: checkNames(files.map((f) => f.rel), platform) });
    } catch (e) {
      if (!ctl.signal.aborted) setError(errorText(e));
      setStep({ kind: "start" });
    }
  };

  const write = async (root: DirHandle, plan: ExportPlan) => {
    stopRef.current = false;
    setStep({ kind: "writing", root, plan, progress: null, stopping: false });
    const result = await writeFolderExport({
      root,
      files: plan.files.filter((f) => f.change !== "unchanged"),
      fetchBytes: (rel) => api.exportBytes(`${base}/${rel}`),
      onProgress: (p) =>
        setStep((s) => (s.kind === "writing" ? { ...s, progress: { ...p, failures: [...p.failures], stopped: false } } : s)),
      shouldStop: () => stopRef.current,
    });
    setStep({ kind: "done", root, plan, result });
  };

  const cancelPlanning = () => {
    abortRef.current?.abort();
    setStep({ kind: "start" });
  };

  return (
    <dialog
      ref={dialogRef}
      className="dialog export-dialog"
      aria-labelledby="export-title"
      onCancel={(e) => {
        e.preventDefault();
        if (!busy) onClose();
      }}
    >
      <div className="dialog-head">
        <h2 id="export-title">Export {path === "/" ? "the store" : <span className="mono">{path}</span>}</h2>
        <button type="button" className="btn btn-ghost btn-small" onClick={onClose} disabled={busy} aria-label="Close">
          ✕
        </button>
      </div>

      {step.kind === "start" && (
        <>
          <div className="dialog-body">
            <p>
              Writes the files in {label} to a folder on this computer, byte for byte as they are stored: line endings and
              encodings are kept.
            </p>
            <ul className="export-points">
              <li>Files that are already identical on disk are not touched, so a git checkout shows only real changes.</li>
              <li>Nothing on disk is deleted: files removed or renamed in textdb stay where they are.</li>
              <li>
                Names that cannot coexist on {PLATFORM_LABEL[platform]} — differing only in letter case, reserved or with
                forbidden characters — stop the export and are listed, so you can rename them first.
              </li>
            </ul>
            {!picker && (
              <p className="muted">
                This browser cannot write into a folder (Chrome and Edge can); the files come as a zip to unpack instead.
              </p>
            )}
            {error && <p className="error-text">{error}</p>}
          </div>
          <div className="dialog-foot">
            <button type="button" className={`btn${picker ? " spacer" : ""}`} onClick={() => void checkZip()}>
              Download as zip…
            </button>
            {picker && (
              <button type="button" className="btn" onClick={onClose}>
                Cancel
              </button>
            )}
            {picker && (
              <button type="button" className="btn btn-primary" onClick={() => void chooseFolder()} autoFocus>
                Choose folder…
              </button>
            )}
          </div>
        </>
      )}

      {step.kind === "planning" && (
        <>
          <div className="dialog-body">
            <p role="status" aria-live="polite">
              {!step.progress
                ? "Listing the store…"
                : step.progress.phase === "listing"
                  ? `Reading ${step.target}: ${plural(step.progress.done, "folder")} listed`
                  : `Comparing files of the same size: ${step.progress.done.toLocaleString()} / ${step.progress.total.toLocaleString()}`}
            </p>
            {step.progress?.phase === "comparing" ? (
              <progress className="progress" value={step.progress.done} max={Math.max(step.progress.total, 1)} aria-label="Comparing" />
            ) : (
              <progress className="progress" aria-label="Planning the export" />
            )}
          </div>
          <div className="dialog-foot">
            <button type="button" className="btn" onClick={cancelPlanning}>
              Cancel
            </button>
          </div>
        </>
      )}

      {step.kind === "review" && (
        <PlanReview
          plan={step.plan}
          target={step.root.name}
          platform={PLATFORM_LABEL[platform]}
          onAnother={() => void chooseFolder()}
          onClose={onClose}
          onWrite={() => void write(step.root, step.plan)}
        />
      )}

      {step.kind === "zip-review" && (
        <>
          <div className="dialog-body">
            <p>
              <strong>{plural(step.files.length, "file")}</strong>,{" "}
              <strong>{formatBytes(step.files.reduce((n, f) => n + f.nbytes, 0))}</strong> before compression. Unpacking
              over a checkout replaces every file, but git still shows only the files whose content changed.
            </p>
            <Problems problems={step.problems} platform={PLATFORM_LABEL[platform]} />
          </div>
          <div className="dialog-foot">
            <button type="button" className="btn" onClick={onClose}>
              Cancel
            </button>
            <a
              className={`btn btn-primary${step.problems.some((p) => p.blocking) || step.files.length === 0 ? " disabled" : ""}`}
              href={api.exportZipUrl(path)}
              download
              aria-disabled={step.problems.some((p) => p.blocking) || step.files.length === 0}
              onClick={(e) => {
                if (e.currentTarget.getAttribute("aria-disabled") === "true") e.preventDefault();
                else setTimeout(onClose, 0);
              }}
            >
              Download zip
            </a>
          </div>
        </>
      )}

      {(step.kind === "writing" || step.kind === "done") && (
        <WriteView
          step={step}
          onStop={() => {
            stopRef.current = true;
            setStep((s) => (s.kind === "writing" ? { ...s, stopping: true } : s));
          }}
          onClose={onClose}
        />
      )}
    </dialog>
  );
}

function Problems({ problems, platform }: { problems: NameProblem[]; platform: string }) {
  const blocking = problems.filter((p) => p.blocking);
  const warnings = problems.filter((p) => !p.blocking);
  return (
    <>
      {blocking.length > 0 && (
        <div className="export-problems" role="alert">
          <p className="error-text">
            {plural(blocking.length, "name")} cannot be written on {platform}. Rename {blocking.length === 1 ? "it" : "them"} in
            the store, or remove what is in the way on disk, then export again.
          </p>
          <ul>
            {blocking.slice(0, 500).map((p) => (
              <li key={`${p.kind}:${p.path}`} title={`${p.path}: ${p.detail}`}>
                <span className="mono">{p.path}</span> <span className="muted">— {p.detail}</span>
              </li>
            ))}
          </ul>
        </div>
      )}
      {warnings.length > 0 && (
        <details className="failures">
          <summary>
            {plural(warnings.length, "name")} would be a problem on other systems, not on {platform}
          </summary>
          <ul>
            {warnings.slice(0, 500).map((p) => (
              <li key={`${p.kind}:${p.path}`} title={`${p.path}: ${p.detail}`}>
                <span className="mono">{p.path}</span>{" "}
                <span className="muted">
                  — {p.detail} ({p.platforms.map((x) => (x === "macos" ? "macOS" : x[0]!.toUpperCase() + x.slice(1))).join(", ")})
                </span>
              </li>
            ))}
          </ul>
        </details>
      )}
    </>
  );
}

interface ReviewProps {
  plan: ExportPlan;
  target: string;
  platform: string;
  onAnother: () => void;
  onClose: () => void;
  onWrite: () => void;
}

function PlanReview({ plan, target, platform, onAnother, onClose, onWrite }: ReviewProps) {
  const toWrite = plan.counts.new + plan.counts.changed;
  const blocked = plan.problems.some((p) => p.blocking);
  const listed = plan.files.filter((f) => f.change !== "unchanged");
  return (
    <>
      <div className="dialog-body">
        <p>
          Into <strong className="mono">{target}</strong>:{" "}
          {toWrite === 0 ? (
            "every file is already identical on disk."
          ) : (
            <>
              <strong>{plural(toWrite, "file")}</strong> to write, {formatBytes(plan.bytesToWrite)}.
            </>
          )}{" "}
          Nothing on disk is deleted.
        </p>
        <div className="stats">
          {(["new", "changed", "unchanged"] as const).map((k) => (
            <div key={k} className="stat">
              <b>{plan.counts[k].toLocaleString()}</b>
              <span>{k}</span>
            </div>
          ))}
        </div>
        <Problems problems={plan.problems} platform={platform} />
        {listed.length > 0 && (
          <details className="failures">
            <summary>Files to write</summary>
            <ul>
              {listed.slice(0, 1000).map((f) => (
                <li key={f.rel}>
                  <span className={`export-change export-${f.change}`}>{f.change}</span> <span className="mono">{f.rel}</span>
                </li>
              ))}
              {listed.length > 1000 && <li className="muted">and {(listed.length - 1000).toLocaleString()} more</li>}
            </ul>
          </details>
        )}
      </div>
      <div className="dialog-foot">
        <button type="button" className="btn spacer" onClick={onAnother}>
          Choose another…
        </button>
        <button type="button" className="btn" onClick={onClose}>
          {toWrite === 0 && !blocked ? "Close" : "Cancel"}
        </button>
        {toWrite > 0 && (
          <button type="button" className="btn btn-primary" onClick={onWrite} disabled={blocked} autoFocus={!blocked}>
            Write {plural(toWrite, "file")}
          </button>
        )}
      </div>
    </>
  );
}

interface WriteProps {
  step: Extract<Step, { kind: "writing" | "done" }>;
  onStop: () => void;
  onClose: () => void;
}

function WriteView({ step, onStop, onClose }: WriteProps) {
  const total = step.plan.counts.new + step.plan.counts.changed;
  const p = step.kind === "done" ? step.result : step.progress;
  const done = p?.done ?? 0;
  const failures = p?.failures ?? [];
  const headline =
    step.kind === "writing"
      ? step.stopping
        ? "Stopping after the files being written…"
        : `Writing into ${step.root.name}`
      : step.result.stopped
        ? `Stopped: ${plural(done - failures.length, "file")} of ${total.toLocaleString()} written into ${step.root.name}`
        : `Exported into ${step.root.name}: ${plural(done - failures.length, "file")} written, ${step.plan.counts.unchanged.toLocaleString()} already identical`;
  return (
    <>
      <div className="dialog-body">
        <p role="status" aria-live="polite">
          {headline}
        </p>
        <progress className="progress" value={done} max={Math.max(total, 1)} aria-label="Export progress" />
        <div className="progress-line">
          <span>
            {done.toLocaleString()} / {total.toLocaleString()} files · {formatBytes(p?.bytes ?? 0)} written
          </span>
        </div>
        {failures.length > 0 && (
          <details className="failures" open={step.kind === "done"}>
            <summary className="error-text">{plural(failures.length, "file")} could not be written</summary>
            <ul>
              {failures.map((f) => (
                <li key={f.rel} title={`${f.rel}: ${f.reason}`}>
                  <span className="mono">{f.rel}</span> <span className="muted">— {f.reason}</span>
                </li>
              ))}
            </ul>
          </details>
        )}
      </div>
      <div className="dialog-foot">
        {step.kind === "done" ? (
          <button type="button" className="btn btn-primary" onClick={onClose} autoFocus>
            Close
          </button>
        ) : (
          <button type="button" className="btn btn-danger" onClick={onStop} disabled={step.stopping}>
            Stop
          </button>
        )}
      </div>
    </>
  );
}
