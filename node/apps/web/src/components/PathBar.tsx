import { useEffect, useId, useRef, useState, type KeyboardEvent } from "react";
import { api, ApiError, type LsEntry } from "../api";
import { folderPath } from "../nav/hash";

interface Props {
  path: string;
  /** Bumped by a keyboard shortcut (F4, Alt+D) to start typing a path. */
  editSignal: number;
  onOpenFolder: (path: string) => void;
  onOpenFile: (path: string) => void;
  /** Text that is not a path: filter this folder by it. */
  onSearch: (text: string) => void;
}

type Suggestion = { kind: "entry"; entry: LsEntry } | { kind: "search"; text: string };

const SUGGESTIONS = 12;

/**
 * The folder's path as breadcrumbs. Clicking beside them (or F4) turns it into a text field:
 * a path starting with `/` opens that folder or file, completing names as you type (Tab takes
 * the highlighted one); anything else filters the folder, as Explorer's address bar searches.
 */
export function PathBar({ path, editSignal, onOpenFolder, onOpenFile, onSearch }: Props) {
  const [editing, setEditing] = useState(false);
  const [value, setValue] = useState("");
  const [suggestions, setSuggestions] = useState<Suggestion[]>([]);
  const [pick, setPick] = useState(-1);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const listId = useId();

  const edit = () => {
    setValue(path === "/" ? "/" : `${path}/`);
    setError(null);
    setPick(-1);
    setEditing(true);
  };
  const editRef = useRef(edit);
  editRef.current = edit;

  useEffect(() => {
    if (editSignal > 0) editRef.current();
  }, [editSignal]);

  useEffect(() => {
    if (editing) inputRef.current?.focus();
  }, [editing]);

  useEffect(() => {
    if (!editing) return;
    const text = value.trim();
    if (!text.startsWith("/")) {
      setSuggestions(text ? [{ kind: "search", text }] : []);
      setPick(text ? 0 : -1);
      return;
    }
    const cut = text.lastIndexOf("/");
    const dir = folderPath(text.slice(0, cut));
    const prefix = text.slice(cut + 1);
    const ctl = new AbortController();
    const timer = setTimeout(() => {
      api
        .list(dir, { sort: "name", order: "asc", offset: 0, limit: SUGGESTIONS, ...(prefix ? { name: `${prefix}*` } : {}) }, ctl.signal)
        .then(
          (page) => {
            setSuggestions(page.entries.map((entry) => ({ kind: "entry", entry })));
            setPick(-1);
          },
          () => {
            if (!ctl.signal.aborted) setSuggestions([]);
          },
        );
    }, 120);
    return () => {
      clearTimeout(timer);
      ctl.abort();
    };
  }, [editing, value]);

  const choose = (s: Suggestion) => {
    setEditing(false);
    if (s.kind === "search") onSearch(s.text);
    else if (s.entry.kind === "folder") onOpenFolder(s.entry.path);
    else onOpenFile(s.entry.path);
  };

  const go = async () => {
    const text = value.trim();
    if (!text.startsWith("/")) {
      setEditing(false);
      onSearch(text);
      return;
    }
    const target = folderPath(text);
    try {
      const entry = await api.entry(target);
      setEditing(false);
      if (entry.kind === "folder") onOpenFolder(entry.path);
      else onOpenFile(entry.path);
    } catch (err) {
      setError(err instanceof ApiError && err.status === 404 ? `Nothing at ${target}` : err instanceof Error ? err.message : String(err));
    }
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    switch (e.key) {
      case "ArrowDown":
        setPick((p) => Math.min(suggestions.length - 1, p + 1));
        break;
      case "ArrowUp":
        setPick((p) => Math.max(-1, p - 1));
        break;
      case "Tab": {
        const s = suggestions[Math.max(0, pick)];
        if (e.shiftKey || s?.kind !== "entry") return;
        setValue(s.entry.kind === "folder" ? `${s.entry.path}/` : s.entry.path);
        setPick(-1);
        break;
      }
      case "Enter": {
        const s = suggestions[pick];
        if (s) choose(s);
        else void go();
        break;
      }
      case "Escape":
        setEditing(false);
        break;
      default:
        return;
    }
    e.preventDefault();
    e.stopPropagation();
  };

  if (!editing) {
    const parts = path.split("/").filter(Boolean);
    return (
      <nav
        className="pathbar"
        aria-label="Folder path"
        title="Click beside the path to type one (F4)"
        onClick={(e) => {
          if (e.target === e.currentTarget) edit();
        }}
      >
        {parts.length === 0 ? (
          <span className="crumb last" aria-current="page">
            All files
          </span>
        ) : (
          <button type="button" className="crumb" onClick={() => onOpenFolder("/")}>
            All files
          </button>
        )}
        {parts.map((part, i) => {
          const to = `/${parts.slice(0, i + 1).join("/")}`;
          return i === parts.length - 1 ? (
            <span key={to} className="crumb last" aria-current="page">
              {part}
            </span>
          ) : (
            <button key={to} type="button" className="crumb" onClick={() => onOpenFolder(to)}>
              {part}
            </button>
          );
        })}
        <button type="button" className="pathbar-edit" onClick={edit} aria-label="Type a path or search this folder">
          Edit path
        </button>
      </nav>
    );
  }

  const open = suggestions.length > 0 || error !== null;
  return (
    <div className="pathbar editing">
      <input
        ref={inputRef}
        className="pathbar-input"
        value={value}
        onChange={(e) => {
          setValue(e.target.value);
          setError(null);
        }}
        onKeyDown={onKeyDown}
        onBlur={() => setEditing(false)}
        role="combobox"
        aria-expanded={open}
        aria-controls={listId}
        aria-autocomplete="list"
        aria-activedescendant={pick >= 0 ? `${listId}-${pick}` : undefined}
        aria-label="Path to open, or text to filter this folder"
        aria-invalid={error !== null}
        spellCheck={false}
      />
      {open && (
        <ul id={listId} className="suggest" role="listbox">
          {error && (
            <li className="error-text" role="presentation">
              {error}
            </li>
          )}
          {suggestions.map((s, i) => (
            <li
              key={s.kind === "entry" ? s.entry.path : `search:${s.text}`}
              id={`${listId}-${i}`}
              role="option"
              aria-selected={i === pick}
              className={i === pick ? "active" : undefined}
              onMouseDown={(e) => {
                e.preventDefault();
                choose(s);
              }}
            >
              {s.kind === "entry" ? (
                <>
                  <span className={s.entry.kind === "folder" ? "icon icon-folder" : "icon icon-file"} aria-hidden="true" />
                  <span className="mono">{s.entry.kind === "folder" ? `${s.entry.path}/` : s.entry.path}</span>
                </>
              ) : (
                <>Filter this folder for “{s.text}”</>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
