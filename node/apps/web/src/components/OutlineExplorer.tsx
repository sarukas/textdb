import { useEffect, useState } from "react";
import { ApiError, api, LINK_STATUSES, type HeadingName, type Link, type LinkStatus, type OutlineEntry } from "../api";
import { byDocument, linkStatusLabel, NEEDS_ATTENTION } from "../doc/structure";
import { baseName } from "../live/paths";
import { LinkRow, OutlineTree } from "./OutlineTree";

interface Props {
  /** The folder to read, `/` for the whole store. */
  folder: string;
  onOpen: (path: string, line?: number) => void;
}

type Tab = "headings" | "links";
type Match = "exact" | "prefix" | "contains";

/**
 * A folder's markdown structure, browsed the way its files are.
 *
 * Every document's heading tree, one after another, is the view this app was missing: a vault reads
 * as a table of names and sizes until you can see what is actually written in it. Above it, the
 * question only an index can answer -- which headings are in use, and who else has this one -- and
 * beside that the links of the folder that reach nothing, grouped by the document to fix them in.
 *
 * A mode of the folder listing rather than a dialog, because it is a way of looking at the folder,
 * and because a dialog over the list is a thing you find once and never again.
 */
export function OutlineExplorer({ folder, onOpen }: Props) {
  const [tab, setTab] = useState<Tab>("headings");
  const [problem, setProblem] = useState<string | null>(null);
  const said = (error: unknown | null) =>
    setProblem(error === null ? null : error instanceof ApiError ? `${error.code}: ${error.message}` : String(error));

  return (
    <div className="outline-explorer">
      <div className="outline-bar">
        <div className="segmented" role="group" aria-label="What to show">
          <button type="button" aria-pressed={tab === "headings"} onClick={() => setTab("headings")} title="Every document's headings, and who has a heading">
            Headings
          </button>
          <button type="button" aria-pressed={tab === "links"} onClick={() => setTab("links")} title="The links of this folder that reach nothing">
            Links
          </button>
        </div>
        <span className="muted outline-scope" title={folder}>
          in {folder}
        </span>
      </div>

      {problem && <p className="access-problem">{problem}</p>}

      {tab === "headings" ? <Headings folder={folder} onOpen={onOpen} onProblem={said} /> : <Links folder={folder} onOpen={onOpen} onProblem={said} />}
    </div>
  );
}

/**
 * The folder's outline, and the index behind it.
 *
 * With nothing typed this is every document's heading tree in path order -- the browsable view. A
 * heading picked from the list on the left narrows it to the documents that have that one, which is
 * an index seek rather than a scan: `headingNames` answers from the index range, so the suggestions
 * can be asked for on every keystroke, and `contains` is the one shape that has to scan and says so.
 */
function Headings({ folder, onOpen, onProblem }: { folder: string; onOpen: Props["onOpen"]; onProblem: (e: unknown | null) => void }) {
  const [starts, setStarts] = useState("");
  const [names, setNames] = useState<HeadingName[] | null>(null);
  const [chosen, setChosen] = useState<string | null>(null);
  const [match, setMatch] = useState<Match>("exact");
  const [sections, setSections] = useState<OutlineEntry[] | null>(null);

  // The names are the index: asked for on every keystroke, from the folder that is open.
  useEffect(() => {
    const ctl = new AbortController();
    const timer = setTimeout(() => {
      onProblem(null);
      api.headingNames(folder, { starts, limit: 200, signal: ctl.signal }).then(setNames, (e: unknown) => {
        if (!ctl.signal.aborted) onProblem(e);
      });
    }, 150);
    return () => {
      clearTimeout(timer);
      ctl.abort();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [folder, starts]);

  // The sections themselves: everything below the folder, or the ones matching the chosen heading.
  useEffect(() => {
    const ctl = new AbortController();
    setSections(null);
    onProblem(null);
    const options = chosen === null ? { limit: 4000 } : { heading: chosen, match, limit: 1000 };
    api.outline(folder, { ...options, signal: ctl.signal }).then(setSections, (e: unknown) => {
      if (!ctl.signal.aborted) onProblem(e);
    });
    return () => ctl.abort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [folder, chosen, match]);

  const docs = byDocument(sections ?? []);
  return (
    <div className="outline-split">
      <div className="outline-names">
        <input
          value={starts}
          onChange={(e) => setStarts(e.target.value)}
          placeholder="headings starting with…"
          aria-label="Heading prefix"
          spellCheck={false}
        />
        <ul>
          <li>
            <button type="button" className={`outline-name ${chosen === null ? "is-chosen" : ""}`} onClick={() => setChosen(null)}>
              <span className="outline-heading-text">Every document</span>
            </button>
          </li>
          {(names ?? []).map((n) => (
            <li key={n.heading}>
              <button type="button" className={`outline-name ${chosen === n.heading ? "is-chosen" : ""}`} onClick={() => setChosen(n.heading)}>
                <span className="outline-heading-text">{n.heading}</span>
                <span className="muted" title={`${n.sections} sections in ${n.docs} documents`}>
                  {n.docs}
                </span>
              </button>
            </li>
          ))}
          {names?.length === 0 && <li className="muted outline-empty">No headings here.</li>}
        </ul>
        {chosen !== null && (
          <label className="outline-match">
            match
            <select value={match} onChange={(e) => setMatch(e.target.value as Match)} aria-label="How the chosen heading is matched">
              <option value="exact">exact</option>
              <option value="prefix">starts with</option>
              <option value="contains">contains (scans)</option>
            </select>
          </label>
        )}
      </div>

      <div className="outline-sections">
        {sections === null ? (
          <p className="muted">Reading the headings…</p>
        ) : docs.length === 0 ? (
          <p className="muted">{chosen === null ? "No markdown headings below this folder." : `No section matches “${chosen}” here.`}</p>
        ) : (
          <>
            <p className="muted outline-count">
              {sections.length} {sections.length === 1 ? "heading" : "headings"} in {docs.length} {docs.length === 1 ? "document" : "documents"}
            </p>
            {docs.map((doc) => (
              <section key={doc.path} className="outline-doc">
                <h3>
                  <button type="button" onClick={() => onOpen(doc.path)} title={doc.path}>
                    {baseName(doc.path)}
                    <span className="muted">{doc.path}</span>
                  </button>
                </h3>
                {/* The document's own tree when this is the whole folder; the matching sections,
                    with their full breadcrumb, when a heading was chosen -- those are not a tree,
                    they are answers from different depths of different documents. */}
                {chosen === null ? (
                  <OutlineTree entries={doc.rows} onOpen={(line) => onOpen(doc.path, line)} />
                ) : (
                  <ul className="outline-matches">
                    {doc.rows.map((s) => (
                      <li key={`${s.lineFrom}-${s.headingPath}`}>
                        <button type="button" className="outline-match-row" onClick={() => onOpen(doc.path, s.lineFrom)}>
                          <span className="outline-heading-text">{s.headingPath}</span>
                          <span className="muted mono">line {s.lineFrom}</span>
                          {s.nwordsTotal !== null && <span className="muted">{s.nwordsTotal} words</span>}
                        </button>
                      </li>
                    ))}
                  </ul>
                )}
              </section>
            ))}
          </>
        )}
      </div>
    </div>
  );
}

/** The folder's links by status, grouped by the document they are written in. */
function Links({ folder, onOpen, onProblem }: { folder: string; onOpen: Props["onOpen"]; onProblem: (e: unknown | null) => void }) {
  /** `null` is every status worth attention, asked for one at a time and shown together. */
  const [status, setStatus] = useState<LinkStatus | null>(null);
  const [links, setLinks] = useState<Link[] | null>(null);

  useEffect(() => {
    const ctl = new AbortController();
    setLinks(null);
    onProblem(null);
    const wanted = status === null ? NEEDS_ATTENTION : [status];
    Promise.all(wanted.map((s) => api.links(folder, { status: s, limit: 5000, signal: ctl.signal })))
      .then((answers) => setLinks(answers.flat()))
      .catch((e: unknown) => {
        if (!ctl.signal.aborted) onProblem(e);
      });
    return () => ctl.abort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [folder, status]);

  const docs = byDocument(links ?? []);
  return (
    <div className="outline-links">
      <div className="outline-statuses">
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
        <p className="muted">{status === null ? "No link in this folder needs attention." : `No link here is ${status}.`}</p>
      ) : (
        <>
          <p className="muted outline-count">
            {links.length} {links.length === 1 ? "link" : "links"} in {docs.length} {docs.length === 1 ? "document" : "documents"}
          </p>
          {docs.map((doc) => (
            <section key={doc.path} className="outline-doc">
              <h3>
                <button type="button" onClick={() => onOpen(doc.path)} title={doc.path}>
                  {baseName(doc.path)}
                  <span className="muted">{doc.path}</span>
                </button>
              </h3>
              <ul className="rail-links">
                {doc.rows.map((l, i) => (
                  <li key={`${l.line}-${i}`}>
                    <span className="muted mono">line {l.line}</span>
                    <LinkRow
                      link={l}
                      onOpen={() => onOpen(doc.path, l.line)}
                      onOpenTarget={l.resolved ? () => onOpen(l.resolved!) : undefined}
                      target={l.resolved ?? undefined}
                    />
                  </li>
                ))}
              </ul>
            </section>
          ))}
        </>
      )}
    </div>
  );
}
