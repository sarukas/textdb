import { useEffect, useRef, useState } from "react";
import { api, type SearchHit } from "../api";
import type { FeedHub } from "../state/hub";
import { FileTree } from "./FileTree";
import { SearchResults } from "./SearchResults";

interface Props {
  hub: FeedHub;
  openPath: string | null;
  onOpen: (path: string, line?: number) => void;
}

interface SearchState {
  q: string;
  hits: SearchHit[] | null;
  loading: boolean;
  error: string | null;
}

export function Sidebar({ hub, openPath, onOpen }: Props) {
  const [query, setQuery] = useState("");
  const [search, setSearch] = useState<SearchState | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const q = query.trim();
    if (q.length < 2) {
      setSearch(null);
      return;
    }
    const ctl = new AbortController();
    const timer = setTimeout(() => {
      setSearch((s) => ({ q, hits: s?.hits ?? null, loading: true, error: null }));
      api.search(q, { limit: 100, signal: ctl.signal }).then(
        (hits) => setSearch({ q, hits, loading: false, error: null }),
        (err: unknown) => {
          if (ctl.signal.aborted) return;
          setSearch({ q, hits: [], loading: false, error: err instanceof Error ? err.message : String(err) });
        },
      );
    }, 250);
    return () => {
      clearTimeout(timer);
      ctl.abort();
    };
  }, [query]);

  // "/" focuses the filter from anywhere outside a text field.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "/" || e.ctrlKey || e.metaKey || e.altKey) return;
      const t = e.target as HTMLElement | null;
      if (t && (t.isContentEditable || t.tagName === "INPUT" || t.tagName === "TEXTAREA")) return;
      e.preventDefault();
      inputRef.current?.focus();
      inputRef.current?.select();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const searching = query.trim().length >= 2;
  return (
    <div className="sidebar-inner">
      <div className="filter">
        <input
          ref={inputRef}
          type="search"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") setQuery("");
            if (e.key === "ArrowDown") {
              e.preventDefault();
              document.querySelector<HTMLElement>(".search-results .hit")?.focus();
            }
          }}
          placeholder="Search corpus…"
          aria-label="Search the corpus"
          spellCheck={false}
        />
        <kbd aria-hidden="true">/</kbd>
      </div>
      {searching && (
        <SearchResults
          query={search?.q ?? query.trim()}
          hits={search?.hits ?? null}
          loading={search?.loading ?? true}
          error={search?.error ?? null}
          onOpen={onOpen}
          onBack={() => inputRef.current?.focus()}
        />
      )}
      <div className="tree-wrap" hidden={searching}>
        <FileTree hub={hub} openPath={openPath} onOpen={onOpen} />
      </div>
    </div>
  );
}
