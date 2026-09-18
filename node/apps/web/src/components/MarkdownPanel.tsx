import { useEffect, useRef, useState } from "react";
import { ApiError, api, LINK_STATUSES, type HeadingName, type Link, type LinkStatus, type OutlineEntry } from "../api";
import { byDocument, linkStatusLabel, linkText, NEEDS_ATTENTION } from "../doc/structure";
import { baseName } from "../live/paths";

interface Props {
  /** The folder to ask about, `/` for the whole store. */
  scope: string;
  onOpen: (path: string, line?: number) => void;
  onClose: () => void;
}

type Tab = "headings" | "links";
type Match = "exact" | "prefix" | "contains";

/**
 * Headings and links across a folder, rather than within one document.
 *
 * Both questions are ones only the store can answer -- "who else has a *Next steps* section" and
 * "which links reach nothing" are index seeks over rows written at commit, not a scan of every
 * document -- and neither had anywhere to be asked in the app.
 *
 * The links half is a triage view: the statuses that need attention first (`broken`,
 * `anchor-missing`, `ambiguous` -- the three `nlinks_broken` counts), grouped by the document the
 * link is written in, because that is where someone goes to fix it.
 */
export function MarkdownPanel({ scope, onOpen, onClose }: Props) {
  const [tab, setTab] = useState<Tab>("headings");
  const [where, setWhere] = useState(scope);
  const [problem, setProblem] = useState<string | null>(null);
  const dialogRef = useRef<HTMLDialogElement | null>(null);

  useEffect(() => {
    dialogRef.current?.showModal();
  }, []);

  const said = (error: unknown) => setProblem(error instanceof ApiError ? `${error.code}: ${error.message}` : String(error));

  return (
    <dialog
      ref={dialogRef}
      className="dialog markdown-dialog"
      aria-labelledby="markdown-title"
      onCancel={(e) => {
        e.preventDefault();
        onClose();
      }}
    >
      <div className="dialog-head">
        <h2 id="markdown-title">Headings and links</h2>
        <button type="button" className="btn btn-ghost btn-small" onClick={onClose} aria-label="Close">
          ✕
        </button>
      </div>

      <nav className="access-tabs">
        {(["headings", "links"] as Tab[]).map((t) => (
          <button key={t} type="button" className={`btn btn-small ${tab === t ? "btn-on" : ""}`} onClick={() => setTab(t)}>
            {t}
          </button>
        ))}
        <label className="markdown-scope">
          in
          <input
            value={where}
            onChange={(e) => setWhere(e.target.value)}
            spellCheck={false}
            aria-label="Folder to look in"
            placeholder="/"
          />
        </label>
      </nav>

      {problem && <p className="access-problem">{problem}</p>}

      {tab === "headings" ? (
        <Headings scope={where || "/"} onOpen={onOpen} onProblem={said} />
      ) : (
        <Links scope={where || "/"} onOpen={onOpen} onProblem={said} />
      )}
    </dialog>
  );
}

/**
 * Which headings are in use, and which documents have the one that was asked for.
 *
 * `headingNames` answers from an index range, so the suggestions can be asked for on every
 * keystroke; `outline` then finds the sections themselves. `contains` is the one shape that has
 * to scan, and it says so where it is chosen.
 */
function Headings({ scope, onOpen, onProblem }: { scope: string; onOpen: Props["onOpen"]; onProblem: (e: unknown) => void }) {
  const [starts, setStarts] = useState("");
  const [names, setNames] = useState<HeadingName[] | null>(null);
  const [chosen, setChosen] = useState<string | null>(null);
  const [match, setMatch] = useState<Match>("exact");
  const [sections, setSections] = useState<OutlineEntry[] | null>(null);

  useEffect(() => {
    const ctl = new AbortController();
    const timer = setTimeout(() => {
      api.headingNames(scope, { starts, limit: 200, signal: ctl.signal }).then(setNames, (e: unknown) => {
        if (!ctl.signal.aborted) onProblem(e);
      });
    }, 150);
    return () => {
      clearTimeout(timer);
      ctl.abort();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [scope, starts]);

  useEffect(() => {
    if (chosen === null) {
      setSections(null);
      return;
    }
    const ctl = new AbortController();
    api.outline(scope, { heading: chosen, match, limit: 500, signal: ctl.signal }).then(setSections, (e: unknown) => {
      if (!ctl.signal.aborted) onProblem(e);
    });
    return () => ctl.abort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [scope, chosen, match]);

  return (
    <section className="access-section markdown-section">
      <div className="markdown-bar">
        <input
          value={starts}
          onChange={(e) => setStarts(e.target.value)}
          placeholder="headings starting with…"
          aria-label="Heading prefix"
          spellCheck={false}
        />
        <select value={match} onChange={(e) => setMatch(e.target.value as Match)} aria-label="How the chosen heading is matched">
          <option value="exact">exact</option>
          <option value="prefix">starts with</option>
          <option value="contains">contains (scans)</option>
        </select>
      </div>
      <div className="markdown-split">
        <ul className="markdown-names">
          {(names ?? []).map((n) => (
            <li key={n.heading}>
              <button type="button" className={`markdown-name ${chosen === n.heading ? "is-chosen" : ""}`} onClick={() => setChosen(n.heading)}>
                <span className="markdown-heading-text">{n.heading}</span>
                <span className="muted" title={`${n.sections} sections in ${n.docs} documents`}>
                  {n.docs}
                </span>
              </button>
            </li>
          ))}
          {names?.length === 0 && <li className="muted markdown-empty">No headings here.</li>}
        </ul>
        <div className="markdown-sections">
          {chosen === null ? (
            <p className="muted">Pick a heading to see who has it.</p>
          ) : sections === null ? (
            <p className="muted">Looking for “{chosen}”…</p>
          ) : sections.length === 0 ? (
            <p className="muted">No section matches “{chosen}” here.</p>
          ) : (
            byDocument(sections).map((doc) => (
              <div key={doc.path} className="markdown-doc">
                <button type="button" className="markdown-doc-name" onClick={() => onOpen(doc.path)} title={doc.path}>
                  {baseName(doc.path)}
                  <span className="muted">{doc.path}</span>
                </button>
                <ul>
                  {doc.rows.map((s) => (
                    <li key={`${s.lineFrom}-${s.headingPath}`}>
                      <button type="button" className="markdown-section-row" onClick={() => onOpen(doc.path, s.lineFrom)}>
                        <span className="markdown-heading-text">{s.headingPath}</span>
                        <span className="muted mono">line {s.lineFrom}</span>
                        {s.nwordsTotal !== null && <span className="muted">{s.nwordsTotal} words</span>}
                      </button>
                    </li>
                  ))}
                </ul>
              </div>
            ))
          )}
        </div>
      </div>
    </section>
  );
}

/** The links of a folder, by status, grouped by the document they are written in. */
function Links({ scope, onOpen, onProblem }: { scope: string; onOpen: Props["onOpen"]; onProblem: (e: unknown) => void }) {
  /** `null` is every status worth attention, asked for one at a time and shown together. */
  const [status, setStatus] = useState<LinkStatus | null>(null);
  const [links, setLinks] = useState<Link[] | null>(null);

  useEffect(() => {
    const ctl = new AbortController();
    setLinks(null);
    const wanted = status === null ? NEEDS_ATTENTION : [status];
    Promise.all(wanted.map((s) => api.links(scope, { status: s, limit: 5000, signal: ctl.signal })))
      .then((answers) => setLinks(answers.flat()))
      .catch((e: unknown) => {
        if (!ctl.signal.aborted) onProblem(e);
      });
    return () => ctl.abort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [scope, status]);

  const docs = byDocument(links ?? []);
  return (
    <section className="access-section markdown-section">
      <div className="markdown-bar">
        <button type="button" className={`btn btn-small ${status === null ? "btn-on" : ""}`} onClick={() => setStatus(null)}>
          needs attention
        </button>
        {LINK_STATUSES.map((s) => (
          <button
            key={s}
            type="button"
            className={`btn btn-small ${status === s ? "btn-on" : ""}`}
            title={linkStatusLabel(s).hint}
            onClick={() => setStatus(s)}
          >
            {s}
          </button>
        ))}
      </div>
      {links === null ? (
        <p className="muted">Reading the links…</p>
      ) : links.length === 0 ? (
        <p className="muted">
          {status === null ? "No link in this folder needs attention." : `No link here is ${status}.`}
        </p>
      ) : (
        <>
          <p className="muted markdown-count">
            {links.length} {links.length === 1 ? "link" : "links"} in {docs.length} {docs.length === 1 ? "document" : "documents"}
          </p>
          {docs.map((doc) => (
            <div key={doc.path} className="markdown-doc">
              <button type="button" className="markdown-doc-name" onClick={() => onOpen(doc.path)} title={doc.path}>
                {baseName(doc.path)}
                <span className="muted">{doc.path}</span>
              </button>
              <ul>
                {doc.rows.map((l, i) => {
                  const label = linkStatusLabel(l.status);
                  return (
                    <li key={`${l.line}-${i}`}>
                      <button type="button" className="markdown-link-row" onClick={() => onOpen(doc.path, l.line)} title={label.hint}>
                        <span className="muted mono">line {l.line}</span>
                        <code>{linkText(l)}</code>
                        <span className={`rail-status link-${label.tone}`}>{label.label}</span>
                        {l.resolved && <span className="muted">→ {l.resolved}</span>}
                      </button>
                    </li>
                  );
                })}
              </ul>
            </div>
          ))}
        </>
      )}
    </section>
  );
}
