import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, type PropertyKey, type PropertyValue } from "../api";
import { applySuggestion, connectiveSuggestions, slotAt, type Slot } from "../meta/query";

/** One row of the dropdown. */
interface Suggestion {
  /** What gets inserted. */
  insert: string;
  /** What the row shows, when it differs (an empty value is named rather than blank). */
  label: string;
  /** Documents using it, when known. */
  docs?: number;
  /** `number` / `text` / `mixed` for a property; absent for values and connectives. */
  kind?: string;
  /** A property name gets a colon appended so the value list opens straight away. */
  addColon?: boolean;
  /** Grouping label shown once above the first row of each run. */
  group: "Property" | "Value" | "Connective";
}

interface Props {
  value: string;
  onChange: (next: string) => void;
  /** Called when the user commits with Enter, for a caller that wants to record history. */
  onSubmit?: (query: string) => void;
  /** Shown under the box: the server's complaint, or a count. */
  status?: string;
  error?: string | null;
  autoFocus?: boolean;
}

/**
 * The query box, with the suggestions Obsidian's property search has: names while you type a
 * property, that property's own values after the colon, and the connectives in between.
 *
 * Both lists come from the indexed property rows rather than from anything held in the page,
 * so a vault with thousands of values suggests from all of them — and the request carries an
 * abort signal, because a slower earlier answer arriving late would otherwise replace a newer
 * one and make the dropdown flicker between two states.
 */
export function MetaQueryBar({ value, onChange, onSubmit, status, error, autoFocus }: Props) {
  const input = useRef<HTMLInputElement>(null);
  const [caret, setCaret] = useState(0);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const [keys, setKeys] = useState<PropertyKey[]>([]);
  const [values, setValues] = useState<PropertyValue[]>([]);

  const slot: Slot = useMemo(() => slotAt(value, caret), [value, caret]);

  // Property names for the current token. Refetched per keystroke; the index range makes that
  // cheap, and it is the only way a name typed since the page loaded can be suggested.
  useEffect(() => {
    if (slot.kind === "value") return;
    const ctrl = new AbortController();
    api
      .metaKeys({ prefix: slot.token, limit: 12, signal: ctrl.signal })
      .then(setKeys)
      .catch(() => {
        /* aborted, or the store went away: the dropdown simply shows nothing */
      });
    return () => ctrl.abort();
  }, [slot.kind, slot.token]);

  useEffect(() => {
    if (slot.kind !== "value" || !slot.key) return;
    const ctrl = new AbortController();
    api
      .metaValues(slot.key, { prefix: slot.token, limit: 12, signal: ctrl.signal })
      .then(setValues)
      .catch(() => {});
    return () => ctrl.abort();
  }, [slot.kind, slot.key, slot.token]);

  const suggestions: Suggestion[] = useMemo(() => {
    if (slot.kind === "value") {
      return values.map((v) => ({
        insert: v.value ?? "",
        // A property that is present but empty is a real state; naming it beats a blank row.
        label: v.value ?? "(empty)",
        docs: v.docs,
        group: "Value" as const,
      }));
    }
    const names: Suggestion[] = keys.map((k) => ({
      insert: k.key,
      label: k.key,
      docs: k.docs,
      kind: k.kind,
      addColon: true,
      group: "Property" as const,
    }));
    // Connectives only where one could go: after a finished term, or when the typed word
    // could still become one. Offering `AND` while a property name is half-typed is noise.
    const connectives = connectiveSuggestions(slot.token).map((c) => ({
      insert: c,
      label: c,
      group: "Connective" as const,
    }));
    return slot.kind === "connective" ? [...connectives, ...names] : [...names, ...connectives];
  }, [slot, keys, values]);

  useEffect(() => setActive(0), [suggestions.length, slot.from, slot.kind]);

  const accept = useCallback(
    (s: Suggestion) => {
      const next = applySuggestion(value, slot, s.insert, { addColon: s.addColon });
      onChange(next.text);
      setOpen(s.group === "Property");
      // Put the caret where the suggestion left it, after React has written the value.
      requestAnimationFrame(() => {
        input.current?.setSelectionRange(next.caret, next.caret);
        setCaret(next.caret);
      });
    },
    [onChange, slot, value],
  );

  const onKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Escape") {
      setOpen(false);
      return;
    }
    if (!open || !suggestions.length) {
      if (e.key === "Enter") onSubmit?.(value);
      if (e.key === "ArrowDown") setOpen(true);
      return;
    }
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setActive((i) => (i + 1) % suggestions.length);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((i) => (i - 1 + suggestions.length) % suggestions.length);
    } else if (e.key === "Enter" || e.key === "Tab") {
      // Tab always completes; Enter completes only while the list is open, so a query typed
      // straight through and submitted does not get a suggestion forced into it.
      e.preventDefault();
      const s = suggestions[active];
      if (s) accept(s);
    }
  };

  const sync = (e: React.SyntheticEvent<HTMLInputElement>) => setCaret(e.currentTarget.selectionStart ?? 0);

  let lastGroup = "";
  return (
    <div className="metaq">
      <div className="metaq-box">
        <input
          ref={input}
          className="metaq-input"
          value={value}
          autoFocus={autoFocus}
          spellCheck={false}
          placeholder="status:draft tags:telco -priority:>3"
          aria-label="Property query"
          aria-expanded={open}
          onChange={(e) => {
            onChange(e.target.value);
            setCaret(e.target.selectionStart ?? 0);
            setOpen(true);
          }}
          onKeyDown={onKeyDown}
          onKeyUp={sync}
          onClick={sync}
          onFocus={() => setOpen(true)}
          // A click on a suggestion blurs the input first, so closing is deferred past it.
          onBlur={() => setTimeout(() => setOpen(false), 120)}
        />
        {value && (
          <button className="metaq-clear" type="button" onClick={() => onChange("")} aria-label="Clear the query">
            ×
          </button>
        )}
      </div>
      {open && suggestions.length > 0 && (
        <ul className="metaq-list" role="listbox">
          {suggestions.map((s, i) => {
            const header = s.group !== lastGroup ? ((lastGroup = s.group), s.group) : null;
            return (
              <li key={`${s.group}:${s.insert}:${i}`}>
                {header && <div className="metaq-group">{header}</div>}
                <button
                  type="button"
                  role="option"
                  aria-selected={i === active}
                  className={`metaq-item${i === active ? " on" : ""}`}
                  onMouseEnter={() => setActive(i)}
                  onMouseDown={(e) => e.preventDefault()}
                  onClick={() => accept(s)}
                >
                  <span className="metaq-label">{s.label}</span>
                  {s.kind && s.kind !== "text" && <span className="metaq-kind">{s.kind}</span>}
                  {s.docs !== undefined && <span className="metaq-count">{s.docs}</span>}
                </button>
              </li>
            );
          })}
        </ul>
      )}
      {error ? <p className="metaq-error">{error}</p> : status ? <p className="metaq-status">{status}</p> : null}
    </div>
  );
}
