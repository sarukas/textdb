import type { Link, OutlineEntry } from "../api";
import { linkStatusLabel, linkText, outlineTree, type OutlineNode } from "../doc/structure";

/**
 * The two things a markdown document is made of besides its text, drawn once.
 *
 * A heading tree and a link row look the same wherever they are shown -- beside the open document,
 * or in the folder's own browser -- so they are drawn here and hosted twice, rather than written
 * twice and drifting.
 */

interface TreeProps {
  entries: readonly OutlineEntry[];
  /** Open the document at a heading's first line. */
  onOpen: (line: number) => void;
  /** The line currently in view, when the host knows one: that heading is marked. */
  at?: number | null;
}

export function OutlineTree({ entries, onOpen, at }: TreeProps) {
  return <Nodes nodes={outlineTree(entries)} onOpen={onOpen} at={at ?? null} />;
}

function Nodes({ nodes, onOpen, at }: { nodes: OutlineNode[]; onOpen: (line: number) => void; at: number | null }) {
  return (
    <ul className="outline-tree">
      {nodes.map((node) => {
        const e = node.entry;
        const here = at !== null && at >= e.lineFrom && at <= e.lineTo;
        return (
          <li key={`${e.lineFrom}-${e.headingPath}`}>
            <button
              type="button"
              className={`outline-heading outline-h${Math.min(e.level, 6)}${here ? " is-here" : ""}`}
              onClick={() => onOpen(e.lineFrom)}
              title={`${e.headingPath} — line ${e.lineFrom}${e.nwordsTotal === null ? "" : `, ${e.nwordsTotal} words with everything under it`}`}
            >
              <span className="outline-heading-text">{e.heading}</span>
              {/* Its own words and the total under it: a heading with a large total and a small own
                  count has its content further down, and one with both small is a stub. */}
              {e.nwords !== null && (
                <span className="outline-words">
                  {e.nwords}
                  {e.nwordsTotal !== null && e.nwordsTotal !== e.nwords && <> / {e.nwordsTotal}</>}
                </span>
              )}
            </button>
            {node.children.length > 0 && <Nodes nodes={node.children} onOpen={onOpen} at={at} />}
          </li>
        );
      })}
    </ul>
  );
}

/** A link's status, shown when it is anything other than the ordinary `ok`. */
export function LinkStatus({ status }: { status: Link["status"] }) {
  const label = linkStatusLabel(status);
  if (status === "ok") return null;
  return (
    <span className={`rail-status link-${label.tone}`} title={label.hint}>
      {label.label}
    </span>
  );
}

interface RowProps {
  link: Link;
  /** Open the document the link is written in, at its line. */
  onOpen: () => void;
  /** Open what it resolves to, when it resolves to something. */
  onOpenTarget?: (() => void) | undefined;
  /** What to show for the target: its name beside the document, its whole path in a list. */
  target?: string;
}

export function LinkRow({ link, onOpen, onOpenTarget, target }: RowProps) {
  return (
    <>
      <button type="button" className="rail-link" onClick={onOpen} title={`${link.path}:${link.line}`}>
        <code>{linkText(link)}</code>
      </button>
      <LinkStatus status={link.status} />
      {link.resolved && target && (
        <button
          type="button"
          className="rail-target"
          onClick={onOpenTarget}
          disabled={!onOpenTarget}
          title={`Open ${link.resolved}${link.asset ? " (an asset)" : ""}`}
        >
          → {target}
          {link.asset && <span className="rail-asset">asset</span>}
        </button>
      )}
    </>
  );
}
