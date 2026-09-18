import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, type PropertyHit, type PropertyKey, type PropertyValue } from "../api";
import { isRunnable, quoteIfNeeded } from "../meta/query";
import { MetaQueryBar } from "./MetaQueryBar";

/** One row of the visual builder: a clause the user assembled rather than typed. */
interface Clause {
  id: number;
  negated: boolean;
  key: string;
  op: string;
  value: string;
}

let clauseSeq = 0;

/** A blank clause, defaulting to the vault's commonest property. */
function freshClause(keys: PropertyKey[]): Clause {
  return { id: ++clauseSeq, negated: false, key: keys[0]?.key ?? "", op: ":", value: "" };
}

/** Operators offered in the builder. `*` is existence, which takes no value. */
const OPS = [
  { op: ":", label: "is" },
  { op: ":!=", label: "is not" },
  { op: ":~", label: "contains" },
  { op: ":", label: "starts with", suffix: "*" },
  { op: ":>", label: ">", numeric: true },
  { op: ":>=", label: "≥", numeric: true },
  { op: ":<", label: "<", numeric: true },
  { op: ":<=", label: "≤", numeric: true },
  { op: ":*", label: "exists", noValue: true },
] as const;

/**
 * Read the query text back into clauses, or `null` when the builder cannot represent it.
 *
 * The builder only expresses a flat AND of simple terms. A query with `OR`, parentheses or a
 * connective it did not write is left alone — switching to the builder must never quietly
 * rewrite what someone typed, so the builder says it cannot show that query instead.
 */
function queryToClauses(text: string): Clause[] | null {
  const t = text.trim();
  if (!t) return [];
  if (/[()]/.test(t) || /(^|\s)(OR|AND|NOT)(\s|$)/i.test(t)) return null;
  const out: Clause[] = [];
  for (const raw of t.match(/(?:[^\s"]|"[^"]*")+/g) ?? []) {
    const negated = raw.startsWith("-");
    const body = negated ? raw.slice(1) : raw;
    const colon = body.indexOf(":");
    if (colon <= 0) return null;
    const key = body.slice(0, colon);
    if (key.toLowerCase() === "has") {
      out.push({ id: ++clauseSeq, negated, key: body.slice(colon + 1), op: ":*", value: "" });
      continue;
    }
    const rest = body.slice(colon + 1);
    const m = /^(>=|<=|!=|[><~])?(.*)$/s.exec(rest);
    const operator = m?.[1] ?? "";
    let value = m?.[2] ?? "";
    if (value === "*") {
      out.push({ id: ++clauseSeq, negated, key, op: ":*", value: "" });
      continue;
    }
    let op = `:${operator}`;
    if (!operator && value.endsWith("*")) {
      op = ":";
      value = value.slice(0, -1);
      out.push({ id: ++clauseSeq, negated, key, op: ":", value });
      continue;
    }
    out.push({ id: ++clauseSeq, negated, key, op, value: value.replace(/^"|"$/g, "") });
  }
  return out;
}

/** Render the builder's clauses as the query text, so the two views never disagree. */
function clausesToQuery(clauses: Clause[]): string {
  return clauses
    .filter((c) => c.key && (c.value || c.op === ":*"))
    .map((c) => {
      const body = c.op === ":*" ? `${c.key}:*` : `${c.key}${c.op}${quoteIfNeeded(c.value)}`;
      return c.negated ? `-${body}` : body;
    })
    .join(" ");
}

/** A property value for a table cell: a list joins with commas rather than printing as JSON. */
function cell(v: unknown): string {
  if (v === null || v === undefined) return "";
  if (Array.isArray(v)) return v.map((x) => cell(x)).join(", ");
  if (typeof v === "object") return JSON.stringify(v);
  return String(v);
}

interface Props {
  /** Open a document from a result row. */
  onOpen: (path: string) => void;
  /** Restrict to a folder; `/` for the whole store. */
  folder?: string;
}

/**
 * Attribute search and explorer.
 *
 * Two ways in, on the same query: a text box with autosuggest for people who know what they
 * want, and a clause builder for people who do not. The builder writes the query text rather
 * than keeping its own state, so switching between them never loses or contradicts anything —
 * and what the builder produces is exactly what could have been typed.
 *
 * The left rail is the vault's own schema: every property in use, with how many documents
 * carry it, and its values on demand. That is what makes the thing explorable rather than
 * just searchable — you can find out what there is to filter on without knowing in advance.
 */
export function MetaExplorer({ onOpen, folder = "/" }: Props) {
  const [query, setQuery] = useState("");
  const [mode, setMode] = useState<"text" | "builder">("text");
  const [clauses, setClauses] = useState<Clause[]>([]);
  /** The typed query uses OR or brackets, which the clause rows cannot show. */
  const [unrepresentable, setUnrepresentable] = useState(false);
  const [hits, setHits] = useState<PropertyHit[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [keys, setKeys] = useState<PropertyKey[]>([]);
  const [openKey, setOpenKey] = useState<string | null>(null);
  const [openValues, setOpenValues] = useState<PropertyValue[]>([]);
  const [columns, setColumns] = useState<string[]>([]);

  // The vault's schema, for the left rail and the builder's property menus.
  useEffect(() => {
    const ctrl = new AbortController();
    api
      .metaKeys({ limit: 200, signal: ctrl.signal })
      .then(setKeys)
      .catch(() => {});
    return () => ctrl.abort();
  }, []);

  // Run the query as it is typed, but only when it is complete enough to mean something:
  // `status:` is a half-typed term, and complaining about it on every keystroke is noise.
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => {
    if (!isRunnable(query)) return;
    const ctrl = new AbortController();
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      setBusy(true);
      api
        .metaFind(query, { folder, limit: 500, signal: ctrl.signal })
        .then((rows) => {
          setHits(rows);
          setError(null);
        })
        .catch((e: unknown) => {
          if (ctrl.signal.aborted) return;
          setHits(null);
          setError(e instanceof Error ? e.message : String(e));
        })
        .finally(() => !ctrl.signal.aborted && setBusy(false));
    }, 120);
    return () => {
      ctrl.abort();
      window.clearTimeout(timer.current);
    };
  }, [query, folder]);

  const showKey = useCallback((key: string) => {
    setOpenKey((cur) => (cur === key ? null : key));
    api
      .metaValues(key, { limit: 50 })
      .then(setOpenValues)
      .catch(() => setOpenValues([]));
  }, []);

  /** Add a term to whatever the query already says, rather than replacing it. */
  const addTerm = useCallback((term: string) => {
    setQuery((q) => (q.trim() ? `${q.trim()} ${term}` : term));
  }, []);

  const setClause = (id: number, patch: Partial<Clause>) =>
    setClauses((cs) => {
      const next = cs.map((c) => (c.id === id ? { ...c, ...patch } : c));
      setQuery(clausesToQuery(next));
      return next;
    });

  const addClause = () => setClauses((cs) => [...cs, freshClause(keys)]);

  const removeClause = (id: number) =>
    setClauses((cs) => {
      const next = cs.filter((c) => c.id !== id);
      setQuery(clausesToQuery(next));
      return next;
    });

  // Columns default to the properties the query mentions, which is almost always what you
  // want to see: you filtered on them, so you want to check them.
  const shown = useMemo(() => {
    if (columns.length) return columns;
    const mentioned = Array.from(query.matchAll(/(?:^|[\s(-])([\w.]+):/g)).map((m) => m[1]!);
    return Array.from(new Set(mentioned.filter((k) => k.toLowerCase() !== "has"))).slice(0, 4);
  }, [columns, query]);

  const toggleColumn = (key: string) =>
    setColumns((cs) => (cs.includes(key) ? cs.filter((c) => c !== key) : [...cs, key]));

  return (
    <section className="meta">
      <aside className="meta-rail">
        <h3>Properties</h3>
        {keys.length === 0 && <p className="meta-empty">No front matter in this store yet.</p>}
        <ul className="meta-keys">
          {keys.map((k) => (
            <li key={k.key}>
              <button type="button" className={`meta-key${openKey === k.key ? " on" : ""}`} onClick={() => showKey(k.key)}>
                <span className="meta-key-name">{k.key}</span>
                {k.kind !== "text" && <span className="metaq-kind">{k.kind}</span>}
                <span className="metaq-count">{k.docs}</span>
              </button>
              {openKey === k.key && (
                <ul className="meta-values">
                  {openValues.map((v) => (
                    <li key={v.value ?? " "}>
                      <button
                        type="button"
                        className="meta-value"
                        title={`Add ${k.key}:${v.value ?? ""} to the query`}
                        onClick={() => addTerm(`${k.key}:${quoteIfNeeded(v.value ?? "")}`)}
                      >
                        <span>{v.value ?? "(empty)"}</span>
                        <span className="metaq-count">{v.docs}</span>
                      </button>
                    </li>
                  ))}
                  <li>
                    <button type="button" className="meta-value" onClick={() => addTerm(`has:${k.key}`)}>
                      <span className="meta-any">has any value</span>
                    </button>
                  </li>
                </ul>
              )}
            </li>
          ))}
        </ul>
      </aside>

      <div className="meta-main">
        <div className="meta-tabs">
          <button type="button" className={mode === "text" ? "on" : ""} onClick={() => setMode("text")}>
            Query
          </button>
          <button
            type="button"
            className={mode === "builder" ? "on" : ""}
            onClick={() => {
              const parsed = queryToClauses(query);
              setUnrepresentable(parsed === null);
              // Only adopt the text when the builder can express it; otherwise it shows
              // nothing and says why, rather than overwriting the query on the first edit.
              if (parsed) setClauses(parsed.length ? parsed : [freshClause(keys)]);
              setMode("builder");
            }}
          >
            Builder
          </button>
        </div>

        {mode === "text" ? (
          <MetaQueryBar
            value={query}
            onChange={setQuery}
            autoFocus
            error={error}
            status={hits ? `${hits.length}${hits.length === 500 ? "+" : ""} documents${busy ? " …" : ""}` : undefined}
          />
        ) : (
          <div className="meta-builder">
            {unrepresentable && (
              <p className="meta-asText">
                This query uses <code>OR</code> or brackets, which the builder cannot show. Editing here would replace
                it — switch back to <strong>Query</strong> to keep it.
              </p>
            )}
            {clauses.map((c) => {
              const prop = keys.find((k) => k.key === c.key);
              const spec = OPS.find((o) => `${o.op}${"suffix" in o ? o.suffix : ""}` === c.op) ?? OPS[0];
              return (
                <div key={c.id} className="meta-clause">
                  <button
                    type="button"
                    className={`meta-not${c.negated ? " on" : ""}`}
                    title="Exclude documents matching this"
                    onClick={() => setClause(c.id, { negated: !c.negated })}
                  >
                    not
                  </button>
                  <select value={c.key} onChange={(e) => setClause(c.id, { key: e.target.value })} aria-label="Property">
                    {keys.map((k) => (
                      <option key={k.key} value={k.key}>
                        {k.key}
                      </option>
                    ))}
                  </select>
                  <select value={c.op} onChange={(e) => setClause(c.id, { op: e.target.value })} aria-label="Comparison">
                    {OPS.filter((o) => !("numeric" in o && o.numeric) || prop?.kind !== "text").map((o) => (
                      <option key={o.label} value={`${o.op}${"suffix" in o ? o.suffix : ""}`}>
                        {o.label}
                      </option>
                    ))}
                  </select>
                  {!("noValue" in spec && spec.noValue) && (
                    <input
                      value={c.value}
                      placeholder="value"
                      aria-label="Value"
                      list={`vals-${c.id}`}
                      onFocus={() => showKeyInto(c.key, c.id)}
                      onChange={(e) => setClause(c.id, { value: e.target.value })}
                    />
                  )}
                  <datalist id={`vals-${c.id}`}>
                    {(valueCache[c.key] ?? []).map((v) => (
                      <option key={v.value ?? ""} value={v.value ?? ""} />
                    ))}
                  </datalist>
                  <button type="button" className="meta-drop" onClick={() => removeClause(c.id)} aria-label="Remove">
                    ×
                  </button>
                </div>
              );
            })}
            <button type="button" className="meta-add" onClick={addClause}>
              + and another
            </button>
            <p className="meta-asText">
              {/* The builder only ever produces a query that could have been typed, so showing
                  it is both a check and a way to learn the syntax. */}
              <code>{query || "(everything)"}</code>
            </p>
            {error && <p className="metaq-error">{error}</p>}
          </div>
        )}

        {shown.length > 0 && (
          <div className="meta-cols">
            <span>Columns:</span>
            {keys.slice(0, 12).map((k) => (
              <button
                key={k.key}
                type="button"
                className={`meta-col${shown.includes(k.key) ? " on" : ""}`}
                onClick={() => toggleColumn(k.key)}
              >
                {k.key}
              </button>
            ))}
          </div>
        )}

        <div className="meta-results">
          {hits === null && !error && <p className="meta-empty">Type a query, or pick a property on the left.</p>}
          {hits?.length === 0 && <p className="meta-empty">Nothing matches.</p>}
          {hits && hits.length > 0 && (
            <table className="meta-table">
              <thead>
                <tr>
                  <th>Document</th>
                  {shown.map((c) => (
                    <th key={c}>{c}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {hits.map((h) => (
                  <tr key={h.path}>
                    <td>
                      <button type="button" className="meta-path" onClick={() => onOpen(h.path)}>
                        {h.path}
                      </button>
                    </td>
                    {shown.map((c) => (
                      <td key={c}>{cell(h.frontmatter?.[c])}</td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>
    </section>
  );
}

/**
 * Values already fetched for the builder's `datalist`s, keyed by property.
 *
 * Module-level rather than state: a `datalist` is read by the browser when the input is
 * focused, and re-rendering the whole explorer to fill one would flicker the results table.
 */
const valueCache: Record<string, PropertyValue[]> = {};
const pending = new Set<string>();

function showKeyInto(key: string, _clauseId: number): void {
  if (!key || valueCache[key] || pending.has(key)) return;
  pending.add(key);
  api
    .metaValues(key, { limit: 50 })
    .then((vs) => {
      valueCache[key] = vs;
    })
    .catch(() => {})
    .finally(() => pending.delete(key));
}
