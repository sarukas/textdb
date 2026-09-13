import { useEffect, useMemo, useRef, useState } from "react";
import { runImport, type ImportProgress } from "../import/runner";
import {
  DEFAULT_EXTENSIONS,
  destPath,
  formatBytes,
  formatDuration,
  includeFile,
  normalizePrefix,
  parseExtensions,
} from "../import/select";
import {
  canPickDirectory,
  errorText,
  folderFromFileList,
  pickFolder,
  type PickedFile,
  type PickedFolder,
} from "../import/source";
import { effectiveAuthor } from "../state/useAuthor";

interface Props {
  author: string;
  onClose: () => void;
  onOpen: (path: string) => void;
}

type Step =
  | { kind: "pick" }
  | { kind: "scanning"; found: number }
  | { kind: "review"; folder: PickedFolder }
  | { kind: "importing"; prefix: string; files: readonly PickedFile[]; progress: ImportProgress };

export function ImportDialog({ author, onClose, onOpen }: Props) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const stopRef = useRef(false);
  const scanRef = useRef<AbortController | null>(null);
  const [step, setStep] = useState<Step>({ kind: "pick" });
  const [error, setError] = useState<string | null>(null);
  const [extInput, setExtInput] = useState(DEFAULT_EXTENSIONS.join(", "));
  const [prefixInput, setPrefixInput] = useState("/");

  useEffect(() => {
    dialogRef.current?.showModal();
    return () => scanRef.current?.abort();
  }, []);

  const running = step.kind === "importing" && (step.progress.state === "running" || step.progress.state === "cancelling");
  const busy = step.kind === "scanning" || running;

  const extensions = useMemo(() => parseExtensions(extInput), [extInput]);
  const selection = useMemo(
    () => (step.kind === "review" ? step.folder.files.filter((f) => includeFile(f.rel, extensions)) : []),
    [step, extensions],
  );
  // Sizes are known up front only when the browser handed over the files themselves.
  const selectionBytes = selection.every((f) => f.size !== null)
    ? selection.reduce((n, f) => n + (f.size ?? 0), 0)
    : null;
  const prefix = normalizePrefix(prefixInput);

  const chosen = (folder: PickedFolder | null) => {
    if (!folder) {
      setStep({ kind: "pick" });
      return;
    }
    setPrefixInput(`/${folder.name}`);
    setStep({ kind: "review", folder });
  };

  const pick = async () => {
    setError(null);
    if (!canPickDirectory()) {
      inputRef.current?.click();
      return;
    }
    const ctl = new AbortController();
    scanRef.current = ctl;
    try {
      chosen(await pickFolder((found) => setStep({ kind: "scanning", found }), ctl.signal));
    } catch (e) {
      setError(errorText(e));
      setStep({ kind: "pick" });
    }
  };

  const start = async () => {
    if (!prefix || selection.length === 0) return;
    stopRef.current = false;
    const files = selection;
    const target = prefix;
    await runImport({
      files,
      prefix: target,
      author: effectiveAuthor(author),
      onProgress: (progress) => setStep({ kind: "importing", prefix: target, files, progress }),
      shouldStop: () => stopRef.current,
    });
  };

  const cancel = () => {
    stopRef.current = true;
    setStep((s) => (s.kind === "importing" && s.progress.state === "running" ? { ...s, progress: { ...s.progress, state: "cancelling" } } : s));
  };

  return (
    <dialog
      ref={dialogRef}
      className="dialog"
      aria-labelledby="import-title"
      onCancel={(e) => {
        // A file input fires a bubbling `cancel` when its picker is dismissed; only the
        // dialog's own (Escape) closes it.
        if (e.target !== e.currentTarget) return;
        e.preventDefault();
        if (!busy) onClose();
      }}
    >
      <div className="dialog-head">
        <h2 id="import-title">Import a folder</h2>
        <button type="button" className="btn btn-ghost btn-small" onClick={onClose} disabled={busy} aria-label="Close">
          ✕
        </button>
      </div>

      <input
        ref={inputRef}
        type="file"
        multiple
        hidden
        {...{ webkitdirectory: "" }}
        onChange={(e) => {
          const list = e.currentTarget.files;
          if (list && list.length > 0) chosen(folderFromFileList(list));
          e.currentTarget.value = "";
        }}
      />

      {step.kind === "pick" && (
        <>
          <div className="dialog-body">
            <p>
              Pick a folder on this computer. Its documents are sent to the store a few megabytes at a time; files already
              in the store at the same path are updated, and unchanged ones make no new version.
            </p>
            {error && <p className="error-text">{error}</p>}
          </div>
          <div className="dialog-foot">
            <button type="button" className="btn" onClick={onClose}>
              Cancel
            </button>
            <button type="button" className="btn btn-primary" onClick={() => void pick()} autoFocus>
              Choose folder…
            </button>
          </div>
        </>
      )}

      {step.kind === "scanning" && (
        <>
          <div className="dialog-body">
            <p role="status" aria-live="polite">
              Listing the folder… <strong className="mono">{step.found.toLocaleString()}</strong> files found
            </p>
            <progress className="progress" aria-label="Listing the folder" />
          </div>
          <div className="dialog-foot">
            <button
              type="button"
              className="btn"
              onClick={() => {
                scanRef.current?.abort();
                setStep({ kind: "pick" });
              }}
            >
              Cancel
            </button>
          </div>
        </>
      )}

      {step.kind === "review" && (
        <>
          <div className="dialog-body">
            <p className="import-summary">
              <strong>{step.folder.name}</strong> — <strong>{selection.length.toLocaleString()}</strong> of{" "}
              {step.folder.files.length.toLocaleString()} files
              {selectionBytes !== null && (
                <>
                  , <strong>{formatBytes(selectionBytes)}</strong>
                </>
              )}
              . Hidden folders and <span className="mono">node_modules</span> are not read.
            </p>
            {step.folder.unreadable.length > 0 && (
              <details className="failures">
                <summary className="error-text">
                  {step.folder.unreadable.length.toLocaleString()}{" "}
                  {step.folder.unreadable.length === 1 ? "entry" : "entries"} could not be read and will be left out
                </summary>
                <ul>
                  {step.folder.unreadable.slice(0, 200).map((u) => (
                    <li key={u.rel} title={`${u.rel || step.folder.name}: ${u.reason}`}>
                      <span className="mono">{u.rel || step.folder.name}</span> <span className="muted">— {u.reason}</span>
                    </li>
                  ))}
                </ul>
              </details>
            )}
            <label className="field">
              <span>Into folder</span>
              <input
                value={prefixInput}
                onChange={(e) => setPrefixInput(e.target.value)}
                aria-invalid={prefix === null}
                spellCheck={false}
              />
              {prefix === null ? (
                <small className="error-text">A folder path cannot contain “.” or “..” segments.</small>
              ) : (
                selection[0] && <small>For example {destPath(prefix, selection[0].rel)}</small>
              )}
            </label>
            <label className="field">
              <span>File types</span>
              <input value={extInput} onChange={(e) => setExtInput(e.target.value)} spellCheck={false} />
              <small>Extensions, separated by commas; * for every file. Binary and non-UTF-8 files are skipped.</small>
            </label>
          </div>
          <div className="dialog-foot">
            <button type="button" className="btn spacer" onClick={() => void pick()}>
              Choose another…
            </button>
            <button type="button" className="btn" onClick={onClose}>
              Cancel
            </button>
            <button
              type="button"
              className="btn btn-primary"
              onClick={() => void start()}
              disabled={prefix === null || selection.length === 0}
            >
              Import {selection.length.toLocaleString()} files
            </button>
          </div>
        </>
      )}

      {step.kind === "importing" && (
        <ImportProgressView
          step={step}
          onCancel={cancel}
          onClose={onClose}
          onAgain={() => setStep({ kind: "pick" })}
          onOpenFirst={() => {
            const first = step.files[0];
            if (first) onOpen(destPath(step.prefix, first.rel));
            onClose();
          }}
        />
      )}
    </dialog>
  );
}

interface ProgressProps {
  step: Extract<Step, { kind: "importing" }>;
  onCancel: () => void;
  onClose: () => void;
  onAgain: () => void;
  onOpenFirst: () => void;
}

function ImportProgressView({ step, onCancel, onClose, onAgain, onOpenFirst }: ProgressProps) {
  const p = step.progress;
  const finished = p.finishedAt !== null;
  const seconds = ((p.finishedAt ?? Date.now()) - p.startedAt) / 1000;
  const rate = seconds > 0 ? p.doneFiles / seconds : 0;
  const remaining = rate > 0 ? (p.totalFiles - p.doneFiles) / rate : null;
  const headline: Record<typeof p.state, string> = {
    running: `Importing into ${step.prefix}`,
    cancelling: "Stopping after the current batch…",
    cancelled: `Stopped: ${p.doneFiles.toLocaleString()} of ${p.totalFiles.toLocaleString()} files were imported into ${step.prefix}`,
    done: `Imported into ${step.prefix}`,
    error: "The import stopped with an error",
  };
  const stats: [string, number, boolean][] = [
    ["created", p.created, false],
    ["updated", p.updated, false],
    ["unchanged", p.unchanged, false],
    ["failed", p.failed, p.failed > 0],
    ["skipped", p.skipped, false],
  ];

  return (
    <>
      <div className="dialog-body">
        <p role="status" aria-live="polite">
          {headline[p.state]}
        </p>
        <progress
          className="progress"
          value={p.doneFiles}
          max={Math.max(p.totalFiles, 1)}
          aria-label="Import progress"
        />
        <div className="progress-line">
          <span>
            {p.doneFiles.toLocaleString()} / {p.totalFiles.toLocaleString()} files · {formatBytes(p.sentBytes)} sent
          </span>
          <span>
            {Math.round(rate).toLocaleString()} files/s ·{" "}
            {finished ? `took ${formatDuration(seconds)}` : remaining === null ? "estimating…" : `${formatDuration(remaining)} left`}
          </span>
        </div>
        <div className="stats">
          {stats.map(([label, value, bad]) => (
            <div key={label} className={`stat${bad ? " bad" : ""}`}>
              <b>{value.toLocaleString()}</b>
              <span>{label}</span>
            </div>
          ))}
        </div>
        {p.error && <p className="error-text">{p.error}</p>}
        {p.failures.length > 0 && (
          <details className="failures">
            <summary>
              {(p.failed + p.skipped).toLocaleString()} {p.failed + p.skipped === 1 ? "file was" : "files were"} not
              imported
            </summary>
            <ul>
              {p.failures.map((f) => (
                <li key={f.path} title={`${f.path}: ${f.reason}`}>
                  <span className="mono">{f.path}</span> <span className="muted">— {f.reason}</span>
                </li>
              ))}
            </ul>
          </details>
        )}
      </div>
      <div className="dialog-foot">
        {finished ? (
          <>
            <button type="button" className="btn spacer" onClick={onAgain}>
              Import another…
            </button>
            {step.files.length > 0 && p.doneFiles > 0 && (
              <button type="button" className="btn" onClick={onOpenFirst}>
                Open first file
              </button>
            )}
            <button type="button" className="btn btn-primary" onClick={onClose} autoFocus>
              Close
            </button>
          </>
        ) : (
          <button type="button" className="btn btn-danger" onClick={onCancel} disabled={p.state === "cancelling"}>
            Cancel
          </button>
        )}
      </div>
    </>
  );
}
