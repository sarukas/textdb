import { useEffect, useId, useRef, useState, type FormEvent, type ReactNode } from "react";
import {
  api,
  ApiError,
  type BulkResult,
  type FileDoc,
  type PurgeStats,
  type Stat,
  type TrashEntry,
  type WriteResult,
} from "../api";
import { folderPath } from "../nav/hash";
import { lineChange, type LineChange } from "../doc/transfer";
import { formatBytes } from "../import/select";
import { readText } from "../import/source";
import { baseName, isWithin, parentOf } from "../live/paths";
import { relativeTime } from "../live/time";
import { effectiveAuthor } from "../state/useAuthor";
import {
  checkMove,
  count,
  describeStat,
  nameRange,
  type BulkAction,
  type BulkItem,
  type PathAction,
  type TrashAction,
} from "../tree/actions";

/** A folder this large asks for its name to be typed before it is deleted. */
const TYPE_TO_CONFIRM_FILES = 50;

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

function useStat(path: string) {
  const [state, setState] = useState<{ stat: Stat | null; error: string | null }>({ stat: null, error: null });
  useEffect(() => {
    let live = true;
    api.stat(path).then(
      (stat) => live && setState({ stat, error: null }),
      (e: unknown) => live && setState({ stat: null, error: message(e) }),
    );
    return () => {
      live = false;
    };
  }, [path]);
  return state;
}

interface ShellProps {
  title: string;
  busy: boolean;
  onClose: () => void;
  onSubmit: (e: FormEvent) => void;
  /** Runs once the dialog is open, to place focus. */
  onShown: () => void;
  children: ReactNode;
  foot: ReactNode;
}

function PathDialog({ title, busy, onClose, onSubmit, onShown, children, foot }: ShellProps) {
  const ref = useRef<HTMLDialogElement>(null);
  const id = useId();
  const shown = useRef(onShown);
  useEffect(() => {
    ref.current?.showModal();
    shown.current();
  }, []);
  return (
    <dialog
      ref={ref}
      className="dialog dialog-narrow"
      aria-labelledby={id}
      onCancel={(e) => {
        if (e.target !== e.currentTarget) return;
        e.preventDefault();
        if (!busy) onClose();
      }}
    >
      <form onSubmit={onSubmit}>
        <div className="dialog-head">
          <h2 id={id}>{title}</h2>
          <button type="button" className="btn btn-ghost btn-small" onClick={onClose} disabled={busy} aria-label="Close">
            ✕
          </button>
        </div>
        <div className="dialog-body">{children}</div>
        <div className="dialog-foot">{foot}</div>
      </form>
    </dialog>
  );
}

interface MoveProps {
  target: PathAction;
  author: string;
  onClose: () => void;
  onMoved: (from: string, to: string) => void;
}

export function MoveDialog({ target, author, onClose, onMoved }: MoveProps) {
  const folder = target.kind === "folder";
  const inputRef = useRef<HTMLInputElement>(null);
  const [value, setValue] = useState(target.path);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { stat, error: statError } = useStat(target.path);
  const check = checkMove(target.path, value);
  const rename = check.ok && parentOf(check.to) === parentOf(target.path);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!check.ok || busy) return;
    setBusy(true);
    setError(null);
    try {
      await api.move(target.path, check.to, effectiveAuthor(author));
      onMoved(target.path, check.to);
    } catch (err) {
      setError(message(err));
      setBusy(false);
      inputRef.current?.focus();
    }
  };

  return (
    <PathDialog
      title={folder ? "Rename or move folder" : "Rename or move file"}
      busy={busy}
      onClose={onClose}
      onSubmit={(e) => void submit(e)}
      onShown={() => {
        const el = inputRef.current;
        if (!el) return;
        el.focus();
        el.setSelectionRange(...nameRange(target.path, target.kind));
      }}
      foot={
        <>
          <button type="button" className="btn" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button type="submit" className="btn btn-primary" disabled={!check.ok || busy}>
            {busy ? "Moving…" : rename ? "Rename" : "Move"}
          </button>
        </>
      }
    >
      <label className="field">
        <span>New path</span>
        <input
          ref={inputRef}
          value={value}
          onChange={(e) => {
            setValue(e.target.value);
            setError(null);
          }}
          aria-invalid={!check.ok && check.reason !== null}
          readOnly={busy}
          spellCheck={false}
        />
        {!check.ok && check.reason ? (
          <small className="error-text">{check.reason}</small>
        ) : (
          <small>Missing folders are created. History moves along with {folder ? "every file" : "the file"}.</small>
        )}
      </label>
      {folder && (
        <p>
          {stat
            ? stat.files + stat.folders === 0
              ? "The folder is empty."
              : `Everything inside moves with it: ${describeStat(stat)}.`
            : (statError ?? "Counting what is inside…")}
        </p>
      )}
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
    </PathDialog>
  );
}

interface DeleteProps {
  target: PathAction;
  author: string;
  openPath: string | null;
  onClose: () => void;
  onDeleted: (path: string) => void;
}

export function DeleteDialog({ target, author, openPath, onClose, onDeleted }: DeleteProps) {
  const folder = target.kind === "folder";
  const name = baseName(target.path);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { stat, error: statError } = useStat(target.path);
  const mustType = folder && (stat?.files ?? 0) >= TYPE_TO_CONFIRM_FILES;
  const ready = stat !== null && (!mustType || typed.trim() === name);
  const closesOpen = openPath !== null && (openPath === target.path || isWithin(target.path, openPath));

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!ready || busy) return;
    setBusy(true);
    setError(null);
    try {
      await api.remove(target.path, effectiveAuthor(author));
      onDeleted(target.path);
    } catch (err) {
      setError(message(err));
      setBusy(false);
    }
  };

  const label = busy ? "Deleting…" : !folder ? "Delete file" : stat ? `Delete ${count(stat.files, "file")}` : "Delete";

  return (
    <PathDialog
      title={folder ? "Delete folder" : "Delete file"}
      busy={busy}
      onClose={onClose}
      onSubmit={(e) => void submit(e)}
      onShown={() => cancelRef.current?.focus()}
      foot={
        <>
          <button ref={cancelRef} type="button" className="btn" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button type="submit" className="btn btn-danger-solid" disabled={!ready || busy}>
            {label}
          </button>
        </>
      }
    >
      <p className="confirm-path">
        <strong>{target.path}</strong>
      </p>
      {statError ? (
        <p className="error-text">{statError}</p>
      ) : (
        <p>
          {!stat
            ? "Counting what is inside…"
            : folder
              ? stat.files + stat.folders === 0
                ? "The folder is empty."
                : `The folder and everything inside it are deleted: ${describeStat(stat)}.`
              : `The file is deleted (${describeStat(stat)}).`}{" "}
          It moves to the trash with its history, where it can still be read until it is permanently removed.
        </p>
      )}
      {closesOpen && <p>The open document {openPath === target.path ? "is" : "is inside it and is"} closed, unsaved changes included.</p>}
      {mustType && (
        <label className="field">
          <span>
            Type <span className="mono">{name}</span> to confirm
          </span>
          <input value={typed} onChange={(e) => setTyped(e.target.value)} readOnly={busy} spellCheck={false} autoFocus />
        </label>
      )}
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
    </PathDialog>
  );
}

interface BulkProps {
  action: BulkAction;
  author: string;
  openPath: string | null;
  onClose: () => void;
  onDone: (result: BulkResult) => void;
}

/** The first few names of a selection, and how many more. */
function BulkList({ items }: { items: BulkItem[] }) {
  const shown = items.slice(0, 6);
  return (
    <ul className="bulk-list">
      {shown.map((i) => (
        <li key={i.path}>
          <span className={i.kind === "folder" ? "icon icon-folder" : "icon icon-file"} aria-hidden="true" />
          <span className="mono">{i.path}</span>
        </li>
      ))}
      {items.length > shown.length && <li className="muted">and {count(items.length - shown.length, "more item")}</li>}
    </ul>
  );
}

export function BulkMoveDialog({ action, author, onClose, onDone }: BulkProps) {
  const { items } = action;
  const inputRef = useRef<HTMLInputElement>(null);
  const [value, setValue] = useState(() => parentOf(items[0]?.path ?? "/"));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const to = folderPath(value);
  const into = items.find((i) => i.kind === "folder" && (i.path === to || isWithin(i.path, to)));
  const reason = !value.trim()
    ? "Enter the destination folder."
    : value.split("/").some((s) => s === "." || s === "..")
      ? "A path cannot contain “.” or “..” segments."
      : into
        ? `${into.path} cannot move inside itself.`
        : null;
  const already = items.every((i) => parentOf(i.path) === to);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (reason || already || busy) return;
    setBusy(true);
    setError(null);
    try {
      onDone(await api.bulk({ op: "move", paths: items.map((i) => i.path), to, author: effectiveAuthor(author) }));
    } catch (err) {
      setError(message(err));
      setBusy(false);
      inputRef.current?.focus();
    }
  };

  return (
    <PathDialog
      title={`Move ${count(items.length, "item")}`}
      busy={busy}
      onClose={onClose}
      onSubmit={(e) => void submit(e)}
      onShown={() => inputRef.current?.select()}
      foot={
        <>
          <button type="button" className="btn" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button type="submit" className="btn btn-primary" disabled={!!reason || already || busy}>
            {busy ? "Moving…" : "Move"}
          </button>
        </>
      }
    >
      <label className="field">
        <span>Destination folder</span>
        <input
          ref={inputRef}
          value={value}
          onChange={(e) => {
            setValue(e.target.value);
            setError(null);
          }}
          aria-invalid={!!reason}
          readOnly={busy}
          spellCheck={false}
        />
        {reason ? (
          <small className="error-text">{reason}</small>
        ) : already ? (
          <small>Everything selected is already there.</small>
        ) : (
          <small>Missing folders are created. Names stay the same, and history moves along. All move, or none do.</small>
        )}
      </label>
      <BulkList items={items} />
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
    </PathDialog>
  );
}

export function BulkDeleteDialog({ action, author, openPath, onClose, onDone }: BulkProps) {
  const { items } = action;
  const cancelRef = useRef<HTMLButtonElement>(null);
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const files = items.reduce((n, i) => n + i.files, 0);
  const folders = items.filter((i) => i.kind === "folder").length;
  const mustType = files >= TYPE_TO_CONFIRM_FILES;
  const ready = !mustType || typed.trim() === "delete";
  const closesOpen = openPath !== null && items.some((i) => i.path === openPath || isWithin(i.path, openPath));

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!ready || busy) return;
    setBusy(true);
    setError(null);
    try {
      onDone(await api.bulk({ op: "delete", paths: items.map((i) => i.path), author: effectiveAuthor(author) }));
    } catch (err) {
      setError(message(err));
      setBusy(false);
    }
  };

  return (
    <PathDialog
      title={`Delete ${count(items.length, "item")}`}
      busy={busy}
      onClose={onClose}
      onSubmit={(e) => void submit(e)}
      onShown={() => cancelRef.current?.focus()}
      foot={
        <>
          <button ref={cancelRef} type="button" className="btn" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button type="submit" className="btn btn-danger-solid" disabled={!ready || busy}>
            {busy ? "Deleting…" : `Delete ${count(files, "file")}`}
          </button>
        </>
      }
    >
      <BulkList items={items} />
      <p>
        {folders > 0
          ? `The ${count(folders, "folder")} go with everything inside them: ${count(files, "file")} in all.`
          : `${count(files, "file")} ${files === 1 ? "is" : "are"} deleted.`}{" "}
        Everything moves to the trash with its history, readable until it is permanently removed.
      </p>
      {closesOpen && <p>The open document is among them and is closed, unsaved changes included.</p>}
      {mustType && (
        <label className="field">
          <span>
            Type <span className="mono">delete</span> to confirm
          </span>
          <input value={typed} onChange={(e) => setTyped(e.target.value)} readOnly={busy} spellCheck={false} autoFocus />
        </label>
      )}
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
    </PathDialog>
  );
}

interface PurgeProps {
  action: TrashAction;
  author: string;
  onClose: () => void;
  onDone: (stats: PurgeStats) => void;
}

export function PurgeDialog({ action, author, onClose, onDone }: PurgeProps) {
  const all = action.op === "empty";
  const cancelRef = useRef<HTMLButtonElement>(null);
  const [items, setItems] = useState<TrashEntry[] | null>(action.op === "purge" ? [action.entry] : null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!all) return;
    let live = true;
    api.trash().then(
      (list) => live && setItems(list),
      (e: unknown) => live && setLoadError(message(e)),
    );
    return () => {
      live = false;
    };
  }, [all]);

  const files = items?.reduce((n, e) => n + e.files, 0) ?? 0;
  const bytes = items?.reduce((n, e) => n + e.nbytes, 0) ?? 0;
  const word = action.op === "purge" ? action.entry.name : "trash";
  const mustType = files >= TYPE_TO_CONFIRM_FILES;
  const ready = items !== null && items.length > 0 && (!mustType || typed.trim() === word);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!ready || busy) return;
    setBusy(true);
    setError(null);
    try {
      const who = effectiveAuthor(author);
      onDone(action.op === "purge" ? await api.purge(action.entry.id, who) : await api.emptyTrash(who));
    } catch (err) {
      setError(message(err));
      setBusy(false);
    }
  };

  return (
    <PathDialog
      title={all ? "Permanently clean trash" : "Permanently remove"}
      busy={busy}
      onClose={onClose}
      onSubmit={(e) => void submit(e)}
      onShown={() => cancelRef.current?.focus()}
      foot={
        <>
          <button ref={cancelRef} type="button" className="btn" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button type="submit" className="btn btn-danger-solid" disabled={!ready || busy}>
            {busy ? "Removing…" : all ? "Clean trash" : "Remove permanently"}
          </button>
        </>
      }
    >
      {action.op === "purge" ? (
        <>
          <p className="confirm-path">
            <strong>{action.entry.path}</strong>
          </p>
          <p>
            {action.entry.kind === "folder"
              ? `The folder and everything deleted with it (${count(files, "file")}, ${formatBytes(bytes)})`
              : `The file (${formatBytes(bytes)}) and its ${count(action.entry.version, "version")}`}{" "}
            leave the store for good. It was deleted
            {action.entry.deleted_by ? ` by ${action.entry.deleted_by}` : ""} {relativeTime(action.entry.deleted_at, Date.now())}.
          </p>
        </>
      ) : loadError ? (
        <p className="error-text">{loadError}</p>
      ) : (
        <p>
          {items === null
            ? "Counting what is in the trash…"
            : items.length === 0
              ? "The trash is empty."
              : `Everything in the trash leaves the store for good: ${count(items.length, "item")}, ${count(files, "file")}, ${formatBytes(bytes)}, with every version.`}
        </p>
      )}
      <p>This cannot be undone. Content that other files or versions also contain stays in the store.</p>
      {mustType && (
        <label className="field">
          <span>
            Type <span className="mono">{word}</span> to confirm
          </span>
          <input value={typed} onChange={(e) => setTyped(e.target.value)} readOnly={busy} spellCheck={false} autoFocus />
        </label>
      )}
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
    </PathDialog>
  );
}

interface ReplaceProps {
  path: string;
  file: File;
  author: string;
  onClose: () => void;
  onReplaced: (path: string, result: WriteResult) => void;
}

type Compared = { current: FileDoc; text: string; change: LineChange } | { error: string };

/** Upload a file as the next version of `path`, after showing how much it changes. */
export function ReplaceDialog({ path, file, author, onClose, onReplaced }: ReplaceProps) {
  const cancelRef = useRef<HTMLButtonElement>(null);
  const [compared, setCompared] = useState<Compared | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<{ text: string; stale: boolean } | null>(null);
  const [rev, setRev] = useState(0);

  useEffect(() => {
    let live = true;
    Promise.all([api.file(path), readText(file)]).then(
      ([current, read]) => {
        if (!live) return;
        if ("skip" in read) {
          setCompared({ error: `${file.name} cannot replace a document: ${read.skip}. Only UTF-8 text can.` });
        } else {
          setCompared({ current, text: read.text, change: lineChange(current.content, read.text) });
        }
      },
      (e: unknown) => live && setCompared({ error: message(e) }),
    );
    return () => {
      live = false;
    };
  }, [path, file, rev]);

  const ok = compared !== null && "current" in compared ? compared : null;
  const same = ok !== null && ok.text === ok.current.content;
  const ready = ok !== null && !same;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!ok || same || busy) return;
    setBusy(true);
    setError(null);
    try {
      const result = await api.write({
        path,
        content: ok.text,
        base_version: ok.current.head_version,
        author: effectiveAuthor(author),
        message: `replaced with ${file.name}`,
      });
      onReplaced(path, result);
    } catch (err) {
      const stale = err instanceof ApiError && err.status === 409;
      setError({
        text: stale ? "Someone changed the same lines while you were choosing the file. Compare again with the latest version." : message(err),
        stale,
      });
      setBusy(false);
    }
  };

  return (
    <PathDialog
      title="Replace with a file"
      busy={busy}
      onClose={onClose}
      onSubmit={(e) => void submit(e)}
      onShown={() => cancelRef.current?.focus()}
      foot={
        <>
          {error?.stale && (
            <button
              type="button"
              className="btn spacer"
              onClick={() => {
                setError(null);
                setCompared(null);
                setRev((r) => r + 1);
              }}
            >
              Compare again
            </button>
          )}
          <button ref={cancelRef} type="button" className="btn" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button type="submit" className="btn btn-primary" disabled={!ready || busy}>
            {busy ? "Replacing…" : "Replace"}
          </button>
        </>
      }
    >
      <p className="confirm-path">
        <strong>{path}</strong>
      </p>
      {compared === null ? (
        <p>Reading {file.name}…</p>
      ) : "error" in compared ? (
        <p className="error-text">{compared.error}</p>
      ) : same ? (
        <p>
          <strong>{file.name}</strong> is the same as the current version, v{compared.current.head_version}. There is nothing to
          replace.
        </p>
      ) : (
        <>
          <p>
            <strong>{file.name}</strong> ({formatBytes(file.size)}) becomes the next version after v{compared.current.head_version}:{" "}
            <span className="line-add">+{compared.change.added.toLocaleString()}</span>{" "}
            <span className="line-del">−{compared.change.removed.toLocaleString()}</span> lines. Earlier versions stay in the
            history.
          </p>
          <p>If this document is open with unsaved edits, they are kept on top of the new version.</p>
        </>
      )}
      {error && (
        <p className="error-text" role="alert">
          {error.text}
        </p>
      )}
    </PathDialog>
  );
}
