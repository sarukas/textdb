import { useCallback, useEffect, useRef, useState } from "react";
import { api, type Link, type OutlineEntry } from "../api";
import { byDocument, linkStatusLabel, NEEDS_ATTENTION, statusCounts } from "../doc/structure";
import { baseName } from "../live/paths";
import type { FeedHub } from "../state/hub";
import { LinkRow, OutlineTree } from "./OutlineTree";

interface Props {
  /** The document on screen. */
  path: string;
  hub: FeedHub;
  onOpen: (path: string, line?: number) => void;
}

type Zone = "outline" | "links";

/**
 * What the store knows about the open document besides its text: its headings, and its links both
 * ways -- beside the activity feed, where someone reading a document is already looking.
 *
 * It was a rail behind a checkbox in the document's own bar, which is where it could not be found.
 * The right-hand column is the one place in this app that answers "what else is there about what I
 * am looking at", so these sit above the feed as zones of it, open unless somebody closes them.
 *
 * Nothing here is parsed: the store writes a row per heading and per link at commit and keeps the
 * links resolved as files are created, moved and deleted, which is why backlinks can be shown at
 * all -- a document says nothing about who points at it.
 */
export function DocumentAside({ path, hub, onOpen }: Props) {
  const [outline, setOutline] = useState<OutlineEntry[] | null>(null);
  const [out, setOut] = useState<Link[] | null>(null);
  const [back, setBack] = useState<Link[] | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [shut, setShut] = useState<Zone[]>(readShut);
  // Bumped when this document changes in the store: its headings and its links change with it.
  const [rev, setRev] = useState(0);
  const pathRef = useRef(path);
  pathRef.current = path;

  // A change to this document changes its headings and its links at once, so that is read again
  // straight away. A change *elsewhere* can matter too -- a link into this document is written in
  // another one, and creating or deleting a file settles or breaks what points at it -- but a vault
  // with an agent writing in it would then ask three times a second, so those are gathered up and
  // asked once, a few seconds later.
  const laterRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    const off = hub.events.on((e) => {
      if (e.path === pathRef.current || e.old_path === pathRef.current) {
        if (laterRef.current) clearTimeout(laterRef.current);
        laterRef.current = null;
        setRev((n) => n + 1);
      } else if (laterRef.current === null) {
        laterRef.current = setTimeout(() => {
          laterRef.current = null;
          setRev((n) => n + 1);
        }, 4000);
      }
    });
    return () => {
      off();
      if (laterRef.current) clearTimeout(laterRef.current);
    };
  }, [hub]);

  useEffect(() => {
    const ctl = new AbortController();
    setProblem(null);
    const said = (error: unknown) => {
      if (!ctl.signal.aborted) setProblem(error instanceof Error ? error.message : String(error));
    };
    // Three asks, not one: a document whose backlinks fail still has an outline worth showing.
    api.outline(path, { limit: 2000, signal: ctl.signal }).then(setOutline, said);
    api.links(path, { signal: ctl.signal }).then(setOut, said);
    api.backlinks(path, { signal: ctl.signal }).then(setBack, said);
    return () => ctl.abort();
  }, [path, rev]);

  // What was shown of the last document is not shown as this one's.
  useEffect(() => {
    setOutline(null);
    setOut(null);
    setBack(null);
  }, [path]);

  const toggle = useCallback((zone: Zone) => {
    setShut((had) => {
      const next = had.includes(zone) ? had.filter((z) => z !== zone) : [...had, zone];
      try {
        localStorage.setItem("textdb.asideShut", JSON.stringify(next));
      } catch {
        // A browser that keeps no site data still gets the zones; it just forgets them.
      }
      return next;
    });
  }, []);

  const needs = (out ?? []).filter((l) => NEEDS_ATTENTION.includes(l.status!)).length;
  const open = (zone: Zone) => !shut.includes(zone);

  return (
    <div className="aside-zones">
      {problem && <p className="rail-problem">{problem}</p>}

      <section className={`aside-zone${open("outline") ? "" : " is-shut"}`}>
        <h3>
          <button type="button" onClick={() => toggle("outline")} aria-expanded={open("outline")}>
            <span className="aside-caret" aria-hidden="true">
              {open("outline") ? "▾" : "▸"}
            </span>
            Outline
            {outline !== null && outline.length > 0 && <span className="aside-count">{outline.length}</span>}
          </button>
        </h3>
        {open("outline") && (
          <div className="aside-body">
            {outline === null ? (
              <p className="muted rail-note">Reading the headings…</p>
            ) : outline.length === 0 ? (
              <p className="muted rail-note">No headings in this document.</p>
            ) : (
              <OutlineTree entries={outline} onOpen={(line) => onOpen(path, line)} />
            )}
          </div>
        )}
      </section>

      <section className={`aside-zone${open("links") ? "" : " is-shut"}`}>
        <h3>
          <button type="button" onClick={() => toggle("links")} aria-expanded={open("links")}>
            <span className="aside-caret" aria-hidden="true">
              {open("links") ? "▾" : "▸"}
            </span>
            Links
            {out !== null && <span className="aside-count">{out.length}</span>}
            {needs > 0 && (
              <span className="rail-needs" title={`${needs} need attention`}>
                {needs}
              </span>
            )}
          </button>
        </h3>
        {open("links") && (
          <div className="aside-body">
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
                    <LinkRow
                      link={l}
                      onOpen={() => onOpen(path, l.line)}
                      onOpenTarget={l.resolved ? () => onOpen(l.resolved!) : undefined}
                      target={l.resolved ? baseName(l.resolved) : undefined}
                    />
                  </li>
                ))}
              </ul>
            )}

            <h4 className="rail-section">Links in{back === null ? "" : ` ${back.length}`}</h4>
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
                        <LinkRow link={l} onOpen={() => onOpen(doc.path, l.line)} />
                      </li>
                    ))}
                  </ul>
                </div>
              ))
            )}
          </div>
        )}
      </section>
    </div>
  );
}

/** The zones someone closed, remembered: they stay closed on the next document and the next visit. */
function readShut(): Zone[] {
  try {
    const kept: unknown = JSON.parse(localStorage.getItem("textdb.asideShut") ?? "[]");
    return Array.isArray(kept) ? (kept.filter((z) => z === "outline" || z === "links") as Zone[]) : [];
  } catch {
    return [];
  }
}
