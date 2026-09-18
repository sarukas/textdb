import { useEffect, useState } from "react";
import { api, type Link, type OutlineEntry } from "../api";
import { byDocument, linkStatusLabel, linkText, NEEDS_ATTENTION, outlineTree, statusCounts, type OutlineNode } from "../doc/structure";
import { baseName } from "../live/paths";

interface Props {
  path: string;
  /** The version on screen: the rows are re-read when it moves, since a commit changes both. */
  version: number;
  onOpen: (path: string, line?: number) => void;
  onClose: () => void;
}

type Tab = "outline" | "links";

/**
 * What the store knows about this markdown document besides its text: its headings, and its links
 * both ways.
 *
 * Neither is parsed here. The store writes a row per heading and per link at commit and keeps the
 * links resolved as files are created, moved and deleted (`docs/outlines.md`, `docs/shapes.md`),
 * so this rail asks for rows and shows them. Which is why backlinks can be shown at all: the
 * document itself says nothing about who points at it.
 *
 * Every row is a place in a document, so every row opens it there.
 */
export function MarkdownRail({ path, version, onOpen, onClose }: Props) {
  const [tab, setTab] = useState<Tab>("outline");
  const [outline, setOutline] = useState<OutlineEntry[] | null>(null);
  const [out, setOut] = useState<Link[] | null>(null);
  const [back, setBack] = useState<Link[] | null>(null);
  const [problem, setProblem] = useState<string | null>(null);

  useEffect(() => {
    const ctl = new AbortController();
    setProblem(null);
    // What was shown of the last document, or of the version before this commit, is not shown as
    // this one's: the three lists go back to "reading" rather than standing as somebody else's.
    setOutline(null);
    setOut(null);
    setBack(null);
    const said = (error: unknown) => {
      if (!ctl.signal.aborted) setProblem(error instanceof Error ? error.message : String(error));
    };
    // Three asks, not one: a document whose backlinks fail still has an outline worth showing, and
    // one rejection of a `Promise.all` would have left all three reading forever.
    api.outline(path, { limit: 2000, signal: ctl.signal }).then(setOutline, said);
    api.links(path, { signal: ctl.signal }).then(setOut, said);
    api.backlinks(path, { signal: ctl.signal }).then(setBack, said);
    return () => ctl.abort();
  }, [path, version]);

  const counts = (rows: Link[] | null) => (rows === null ? "" : ` ${rows.length}`);
  const needs = (out ?? []).filter((l) => NEEDS_ATTENTION.includes(l.status!)).length;

  return (
    <aside className="doc-rail" aria-label="Headings and links">
      <div className="rail-head">
        <div className="segmented" role="tablist" aria-label="Rail">
          <button type="button" role="tab" aria-selected={tab === "outline"} onClick={() => setTab("outline")}>
            Outline
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === "links"}
            onClick={() => setTab("links")}
            // Read out as one sentence: the two numbers next to each other are "42" to a screen
            // reader, which is a number this document has nothing to do with.
            aria-label={out === null ? "Links" : `Links: ${out.length}, ${needs} needing attention`}
          >
            Links{counts(out)}
            {needs > 0 && (
              <span className="rail-needs" aria-hidden="true" title={`${needs} of them need attention`}>
                {needs}
              </span>
            )}
          </button>
        </div>
        <button type="button" className="btn btn-ghost btn-small" onClick={onClose} aria-label="Hide the rail">
          ✕
        </button>
      </div>

      {problem && <p className="rail-problem">{problem}</p>}

      {tab === "outline" && (
        <div className="rail-body">
          {outline === null ? (
            <p className="muted rail-note">Reading the headings…</p>
          ) : outline.length === 0 ? (
            <p className="muted rail-note">No headings in this document.</p>
          ) : (
            <Headings nodes={outlineTree(outline)} onOpen={(line) => onOpen(path, line)} />
          )}
        </div>
      )}

      {tab === "links" && (
        <div className="rail-body">
          <h3 className="rail-section">Links out</h3>
          {out !== null && out.length > 0 && (
            <p className="rail-counts">
              {statusCounts(out).map(({ status, n }) => {
                const label = linkStatusLabel(status);
                return (
                  <span key={status ?? "unresolved"} className={`rail-status link-${label.tone}`} title={label.hint}>
                    {n} {label.label.toLowerCase()}
                  </span>
                );
              })}
            </p>
          )}
          {out === null ? (
            <p className="muted rail-note">Reading the links…</p>
          ) : out.length === 0 ? (
            <p className="muted rail-note">This document links to nothing.</p>
          ) : (
            <ul className="rail-links">
              {out.map((l, i) => (
                <li key={`${l.line}-${i}`}>
                  <button type="button" className="rail-link" onClick={() => onOpen(path, l.line)} title={`line ${l.line}`}>
                    <code>{linkText(l)}</code>
                  </button>
                  <Status link={l} />
                  {l.resolved && (
                    <button
                      type="button"
                      className="rail-target"
                      onClick={() => onOpen(l.resolved!)}
                      title={`Open ${l.resolved}${l.asset ? " (an asset)" : ""}`}
                    >
                      → {baseName(l.resolved)}
                      {l.asset && <span className="rail-asset">asset</span>}
                    </button>
                  )}
                </li>
              ))}
            </ul>
          )}

          <h3 className="rail-section">Links in{counts(back)}</h3>
          {back === null ? (
            <p className="muted rail-note">Reading the backlinks…</p>
          ) : back.length === 0 ? (
            <p className="muted rail-note">Nothing in the store points here.</p>
          ) : (
            byDocument(back).map((doc) => (
              <div key={doc.path} className="rail-backlink">
                <button type="button" className="rail-doc" onClick={() => onOpen(doc.path)} title={doc.path}>
                  {baseName(doc.path)}
                </button>
                <ul className="rail-links">
                  {doc.rows.map((l, i) => (
                    <li key={`${l.line}-${i}`}>
                      <button type="button" className="rail-link" onClick={() => onOpen(doc.path, l.line)} title={`${doc.path}:${l.line}`}>
                        <code>{linkText(l)}</code>
                      </button>
                      <Status link={l} />
                    </li>
                  ))}
                </ul>
              </div>
            ))
          )}
        </div>
      )}
    </aside>
  );
}

function Status({ link }: { link: Link }) {
  const label = linkStatusLabel(link.status);
  // `ok` is the ordinary case and says nothing worth the room; the others are the point.
  if (label.tone === "ok" && link.status === "ok") return null;
  return (
    <span className={`rail-status link-${label.tone}`} title={label.hint}>
      {label.label}
    </span>
  );
}

function Headings({ nodes, onOpen }: { nodes: OutlineNode[]; onOpen: (line: number) => void }) {
  return (
    <ul className="rail-outline">
      {nodes.map((node) => (
        <li key={`${node.entry.lineFrom}-${node.entry.headingPath}`}>
          <button
            type="button"
            className={`rail-heading rail-h${Math.min(node.entry.level, 6)}`}
            onClick={() => onOpen(node.entry.lineFrom)}
            title={`${node.entry.headingPath} — line ${node.entry.lineFrom}${
              node.entry.nwordsTotal === null ? "" : `, ${node.entry.nwordsTotal} words with everything under it`
            }`}
          >
            <span className="rail-heading-text">{node.entry.heading}</span>
            {node.entry.nwords !== null && (
              // Own words and the total under it: a heading with a large total and a small own
              // count has its content further down, and one with both small is a stub.
              <span className="rail-words">
                {node.entry.nwords}
                {node.entry.nwordsTotal !== null && node.entry.nwordsTotal !== node.entry.nwords && <> / {node.entry.nwordsTotal}</>}
              </span>
            )}
          </button>
          {node.children.length > 0 && <Headings nodes={node.children} onOpen={onOpen} />}
        </li>
      ))}
    </ul>
  );
}
