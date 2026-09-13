import { useEffect, useId, useRef, useState, type FormEvent, type ReactNode } from "react";
import { api, type Stat } from "../api";
import { baseName, isWithin, parentOf } from "../live/paths";
import { effectiveAuthor } from "../state/useAuthor";
import { checkMove, count, describeStat, nameRange, type PathAction } from "../tree/actions";

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
          It disappears for everyone; its history stays in the store, but this app cannot bring it back.
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
