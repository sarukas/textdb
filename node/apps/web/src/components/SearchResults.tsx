import type { KeyboardEvent, ReactNode } from "react";
import type { SearchHit } from "../api";

interface Props {
  query: string;
  hits: SearchHit[] | null;
  loading: boolean;
  error: string | null;
  onOpen: (path: string, line?: number) => void;
  onBack: () => void;
}

function highlight(snippet: string, query: string): ReactNode[] {
  const terms = query
    .split(/\s+/)
    .map((t) => t.replace(/[^\p{L}\p{N}_-]/gu, ""))
    .filter((t) => t.length > 1);
  if (!terms.length) return [snippet];
  const re = new RegExp(`(${terms.map((t) => t.replace(/[-]/g, "\\-")).join("|")})`, "giu");
  return snippet.split(re).map((part, i) => (i % 2 === 1 ? <mark key={i}>{part}</mark> : part));
}

export function SearchResults({ query, hits, loading, error, onOpen, onBack }: Props) {
  const onKeyDown = (e: KeyboardEvent<HTMLUListElement>) => {
    const items = Array.from(e.currentTarget.querySelectorAll<HTMLElement>(".hit"));
    const i = items.indexOf(document.activeElement as HTMLElement);
    if (e.key === "ArrowDown" && i < items.length - 1) {
      e.preventDefault();
      items[i + 1]?.focus();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      if (i <= 0) onBack();
      else items[i - 1]?.focus();
    } else if (e.key === "Escape") {
      onBack();
    }
  };
  return (
    <div className="search-results" aria-busy={loading}>
      <div className="search-status" role="status">
        {error ? (
          <span className="error-text">{error}</span>
        ) : hits === null ? (
          "Searching…"
        ) : (
          <>
            {hits.length === 0 ? "No matches" : `${hits.length}${hits.length >= 100 ? "+" : ""} matches`}
            {loading && " · updating…"}
          </>
        )}
      </div>
      {hits && hits.length > 0 && (
        <ul className="hits" aria-label={`Search results for ${query}`} onKeyDown={onKeyDown}>
          {hits.map((h, i) => (
            <li key={`${h.path}:${h.line}:${i}`}>
              <button type="button" className="hit" onClick={() => onOpen(h.path, h.line)}>
                <span className="hit-loc">
                  <span className="hit-path">{h.path}</span>
                  <span className="hit-line">:{h.line}</span>
                  {/* The heading the line sits under: what tells two hits in one document apart. */}
                  {h.section && <span className="hit-section">{h.section}</span>}
                </span>
                <span className="hit-snippet">{highlight(h.text, query)}</span>
                {h.more > 0 && <span className="hit-more">{h.more} more in this file</span>}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
