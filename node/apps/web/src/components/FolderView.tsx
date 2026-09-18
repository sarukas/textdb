import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent,
  type MouseEvent,
} from "react";
import { api, ApiError, type AssetItem, type AuthorCount, type LsEntry, type SearchHit, type SyncLink } from "../api";
import { assetPath, inFolder, isPointer, stateLabel } from "../assets/model";
import {
  COLUMNS,
  PAGE,
  authorsText,
  extensionOf,
  isFiltered,
  nextSort,
  pagesIn,
  orderIsStable,
  parseFilter,
  planChange,
  readColumns,
  type Column,
  type ColumnId,
  type Sort,
} from "../folder/model";
import { formatBytes } from "../import/select";
import { authorHue, authorStyle } from "../live/color";
import { baseName, isWithin, parentOf } from "../live/paths";
import { relativeTime } from "../live/time";
import { hashFor } from "../nav/hash";
import type { FeedHub } from "../state/hub";
import { useNow } from "../state/useNow";
import { actionFor, count, type BulkAction, type PathAction } from "../tree/actions";
import { ContextMenu, type MenuItem, type MenuState } from "./ContextMenu";
import { PathBar } from "./PathBar";
import { MetaExplorer } from "./MetaExplorer";
import { SearchResults } from "./SearchResults";
import { ensureRowVisible, VirtualList } from "./VirtualList";

const ROW = 34;
const HEAD = 34;
const CHECK = 36;
const FLASH_MS = 4000;
/** In-place refreshes per burst of changes; beyond this the listing waits for a reload. */
const REFRESH_LIMIT = 50;

interface Props {
  path: string;
  hub: FeedHub;
  onOpenFolder: (path: string) => void;
  onOpenFile: (path: string, line?: number) => void;
  onAction: (action: PathAction) => void;
  onBulk: (action: BulkAction) => void;
  /** The server syncs this folder with a directory. */
  syncLink?: SyncLink | null;
  /** The synced folder this folder is in (itself or one above it): the state of its assets is shown. */
  assetLink?: SyncLink | null;
  onSync?: (prefix: string) => void;
}

/** The rows fetched so far for one query, page by page. */
interface Listing {
  key: string;
  total: number | null;
  pages: ReadonlyMap<number, LsEntry[]>;
  error: string | null;
}

interface ContentSearch {
  q: string;
  hits: SearchHit[] | null;
  loading: boolean;
  error: string | null;
}

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));
const columnOf = (id: ColumnId): Column => COLUMNS.find((c) => c.id === id) ?? COLUMNS[0]!;
const trackMin = (width: string) => Number(/(\d+)px/.exec(width)?.[1] ?? 80);
const num = (n: number | null) => (n === null ? "" : n.toLocaleString());

function stored<T>(key: string, parse: (raw: string | null) => T): T {
  try {
    return parse(localStorage.getItem(key));
  } catch {
    return parse(null);
  }
}

function save(key: string, value: unknown): void {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Not persisted; the choice still applies to this page.
  }
}

function readSort(raw: string | null): Sort {
  try {
    const s = JSON.parse(raw ?? "") as Partial<Sort>;
    if (COLUMNS.some((c) => c.id === s.key) && (s.order === "asc" || s.order === "desc")) return s as Sort;
  } catch {
    // default below
  }
  return { key: "name", order: "asc" };
}

/**
 * The central listing of a folder, GitHub style: a path bar, a filter, and a table of what is in
 * the folder (or everything below it) sortable by any column, fetched a page at a time as it
 * scrolls. Changes from anyone update rows in place; rows that appear or would reorder wait
 * behind a "Refresh" so the list does not jump under the pointer.
 */
export function FolderView({ path, hub, onOpenFolder, onOpenFile, onAction, onBulk, syncLink, assetLink, onSync }: Props) {
  const now = useNow(30_000);
  const label = path === "/" ? "all files" : baseName(path);
  const [sort, setSort] = useState<Sort>(() => stored("textdb.folderSort", readSort));
  const [columns, setColumns] = useState<ColumnId[]>(() => stored("textdb.folderColumns", readColumns));
  const [filterText, setFilterText] = useState("");
  const [filter, setFilter] = useState(() => parseFilter(""));
  const [contents, setContents] = useState(false);
  // Search by front-matter property, scoped to this folder.
  const [props, setProps] = useState(false);
  const [recursive, setRecursive] = useState(false);
  const [nonce, setNonce] = useState(0);
  const [summary, setSummary] = useState<LsEntry | null>(null);
  const [gone, setGone] = useState(false);
  const [removed, setRemoved] = useState<ReadonlySet<string>>(() => new Set());
  const [pending, setPending] = useState(0);
  const [active, setActive] = useState(0);
  const [selected, setSelected] = useState<ReadonlyMap<string, LsEntry>>(() => new Map());
  const anchor = useRef(0);
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [editSignal, setEditSignal] = useState(0);
  const [search, setSearch] = useState<ContentSearch | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const filterRef = useRef<HTMLInputElement>(null);
  const flashes = useRef(new Map<string, { hue: number; until: number }>());
  const [, setFlashRev] = useState(0);

  useEffect(() => {
    const t = setTimeout(() => setFilter(parseFilter(filterText)), 200);
    return () => clearTimeout(t);
  }, [filterText]);

  // ---- the listing ----------------------------------------------------------------------------

  const query = useMemo(
    () => ({ sort: sort.key, order: sort.order, recursive, ...(contents ? {} : filter) }),
    [sort, recursive, contents, filter],
  );
  const baseKey = JSON.stringify(query);
  const key = `${baseKey}#${nonce}`;
  const [listing, setListing] = useState<Listing>({ key, total: null, pages: new Map(), error: null });
  const stale = listing.key !== key;
  const inflight = useRef(new Map<string, AbortController>());

  const loadPage = useCallback(
    (page: number) => {
      const id = `${key}@${page}`;
      if (inflight.current.has(id)) return;
      const ctl = new AbortController();
      inflight.current.set(id, ctl);
      api
        .list(path, { ...query, offset: page * PAGE, limit: PAGE }, ctl.signal)
        .then(
          (res) =>
            setListing((l) => {
              // A new query replaces the rows on screen only once its first page is in.
              if (l.key !== key) return page === 0 ? { key, total: res.total, pages: new Map([[0, res.entries]]), error: null } : l;
              return { ...l, total: res.total, pages: new Map(l.pages).set(page, res.entries), error: null };
            }),
          (err: unknown) => {
            if (ctl.signal.aborted) return;
            if (err instanceof ApiError && err.status === 404) setGone(true);
            setListing((l) => (l.key === key ? { ...l, error: message(err) } : { key, total: null, pages: new Map(), error: message(err) }));
          },
        )
        .finally(() => {
          if (inflight.current.get(id) === ctl) inflight.current.delete(id);
        });
    },
    [key, path, query],
  );

  useEffect(() => {
    for (const [id, ctl] of inflight.current) {
      if (!id.startsWith(`${key}@`)) {
        ctl.abort();
        inflight.current.delete(id);
      }
    }
    setRemoved(new Set());
    setPending(0);
    loadPage(0);
  }, [key, loadPage]);

  useEffect(() => {
    if (listRef.current) listRef.current.scrollTop = 0;
    setActive(0);
  }, [baseKey]);

  useEffect(() => {
    const ctls = inflight.current;
    return () => ctls.forEach((c) => c.abort());
  }, []);

  // The pages of the rows on screen; a stale or failed listing waits for its first page or a retry.
  const onRange = (start: number, end: number) => {
    if (stale || listing.error) return;
    for (const p of pagesIn(start, end)) if (!listing.pages.has(p)) loadPage(p);
  };

  const total = listing.total ?? 0;
  const entryAt = (i: number): LsEntry | undefined => listing.pages.get(Math.floor(i / PAGE))?.[i % PAGE];
  const isRemoved = (p: string) => removed.has(p) || [...removed].some((r) => isWithin(r, p));

  const loadSummary = useCallback(() => {
    api.entry(path).then(setSummary, (err: unknown) => {
      if (err instanceof ApiError && err.status === 404) setGone(true);
    });
  }, [path]);
  useEffect(loadSummary, [loadSummary]);

  // The state of the assets below this folder, by asset path, when it is in a synced folder.
  const assetPrefix = assetLink?.prefix ?? null;
  // A sync of the folder (the first one included) changes what its assets' states can be.
  const assetSyncSeq = assetLink?.last?.seq ?? null;
  const [assetRev, setAssetRev] = useState(0);
  const [assets, setAssets] = useState<ReadonlyMap<string, AssetItem>>(() => new Map());
  useEffect(() => {
    if (assetPrefix === null) {
      setAssets(new Map());
      return;
    }
    const ctl = new AbortController();
    api.assets(assetPrefix, path, ctl.signal).then(
      (s) => setAssets(new Map(s.assets.map((a) => [a.path, a]))),
      () => {
        if (!ctl.signal.aborted) setAssets(new Map());
      },
    );
    return () => ctl.abort();
  }, [assetPrefix, assetSyncSeq, path, nonce, assetRev]);
  const assetOf = (e: LsEntry): AssetItem | undefined => (e.kind === "file" && isPointer(e.path) ? assets.get(assetPath(e.path)) : undefined);

  // ---- live changes ---------------------------------------------------------------------------

  const refreshRows = useCallback((paths: string[]) => {
    for (const p of paths.slice(0, REFRESH_LIMIT)) {
      api.entry(p).then(
        (fresh) =>
          setListing((l) => {
            let pages: Map<number, LsEntry[]> | null = null;
            for (const [n, rows] of l.pages) {
              const i = rows.findIndex((r) => r.path === p);
              if (i < 0) continue;
              pages ??= new Map(l.pages);
              const copy = rows.slice();
              copy[i] = fresh;
              pages.set(n, copy);
            }
            return pages ? { ...l, pages } : l;
          }),
        (err: unknown) => {
          if (err instanceof ApiError && err.status === 404) setRemoved((r) => new Set(r).add(p));
        },
      );
    }
  }, []);

  const liveRef = useRef({ recursive, sortKey: sort.key, onOpenFolder });
  liveRef.current = { recursive, sortKey: sort.key, onOpenFolder };

  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | null = null;
    const toRefresh = new Set<string>();
    // A pointer changed: the assets' state is looked at again, once per burst.
    let pointers = false;
    const flush = () => {
      timer = null;
      if (pointers) {
        pointers = false;
        setAssetRev((r) => r + 1);
      }
      loadSummary();
      refreshRows([...toRefresh]);
      toRefresh.clear();
      setFlashRev((r) => r + 1);
    };
    const off = hub.events.on((e) => {
      // A pointer here changed, or something here moved or went: the assets' states load again.
      const here = inFolder(path, e.path) || (e.old_path !== null && inFolder(path, e.old_path));
      if (here && (isPointer(e.path) || isPointer(e.old_path ?? "") || e.op === "move" || e.op === "delete")) {
        pointers = true;
        timer ??= setTimeout(flush, 300);
      }
      const { recursive: rec, sortKey, onOpenFolder: follow } = liveRef.current;
      const plan = planChange(path, rec, e);
      if (plan.folder) {
        if (plan.folder.to) follow(plan.folder.to);
        else setGone(true);
        return;
      }
      if (!plan.refresh.length && !plan.removed.length && !plan.added) return;
      plan.refresh.forEach((p) => toRefresh.add(p));
      const mark = { hue: authorHue(e.author ?? ""), until: Date.now() + FLASH_MS };
      [...plan.refresh, ...plan.removed].forEach((p) => flashes.current.set(p, mark));
      if (plan.removed.length) setRemoved((r) => new Set([...r, ...plan.removed]));
      const reorders = plan.added + plan.removed.length + (orderIsStable(sortKey) ? 0 : plan.refresh.length);
      if (reorders) setPending((n) => n + reorders);
      timer ??= setTimeout(flush, 300);
    });
    const offReconnect = hub.reconnected.on(() => {
      loadSummary();
      setNonce((n) => n + 1);
    });
    const sweep = setInterval(() => {
      const t = Date.now();
      let expired = false;
      for (const [k, m] of flashes.current) {
        if (m.until < t) {
          flashes.current.delete(k);
          expired = true;
        }
      }
      if (expired) setFlashRev((r) => r + 1);
    }, 1000);
    return () => {
      off();
      offReconnect();
      clearInterval(sweep);
      if (timer) clearTimeout(timer);
    };
  }, [hub, path, loadSummary, refreshRows]);

  // Selected rows that went away are no longer selected.
  useEffect(() => {
    if (!removed.size) return;
    setSelected((s) => {
      const kept = new Map([...s].filter(([p]) => !removed.has(p) && ![...removed].some((r) => isWithin(r, p))));
      return kept.size === s.size ? s : kept;
    });
  }, [removed]);

  // ---- content search -------------------------------------------------------------------------

  useEffect(() => {
    const q = filterText.trim();
    if (!contents || q.length < 2) {
      setSearch(null);
      return;
    }
    const ctl = new AbortController();
    const t = setTimeout(() => {
      setSearch((s) => ({ q, hits: s?.hits ?? null, loading: true, error: null }));
      api.search(q, { prefix: path, limit: 200, signal: ctl.signal }).then(
        (hits) => setSearch({ q, hits, loading: false, error: null }),
        (err: unknown) => {
          if (!ctl.signal.aborted) setSearch({ q, hits: [], loading: false, error: message(err) });
        },
      );
    }, 250);
    return () => {
      clearTimeout(t);
      ctl.abort();
    };
  }, [contents, filterText, path]);

  // ---- selection and actions ------------------------------------------------------------------

  const open = (e: LsEntry) => (e.kind === "folder" ? onOpenFolder(e.path) : onOpenFile(e.path));

  const toggleSelect = (e: LsEntry) =>
    setSelected((s) => {
      const next = new Map(s);
      if (next.has(e.path)) next.delete(e.path);
      else next.set(e.path, e);
      return next;
    });

  const selectRange = (from: number, to: number) => {
    const [a, b] = from < to ? [from, to] : [to, from];
    setSelected((s) => {
      const next = new Map(s);
      for (let i = a; i <= b; i++) {
        const e = entryAt(i);
        if (e && !isRemoved(e.path)) next.set(e.path, e);
      }
      return next;
    });
  };

  const bulk = (op: BulkAction["op"], entries: LsEntry[]) =>
    onBulk({
      op,
      items: entries.map((e) => ({ path: e.path, kind: e.kind, files: e.kind === "folder" ? (e.files ?? 0) : 1 })),
    });

  const menuFor = (e: LsEntry): Omit<MenuState, "x" | "y"> => {
    const many = selected.size > 1 && selected.has(e.path) ? [...selected.values()] : null;
    if (many) {
      return {
        key: e.path,
        label: count(many.length, "item"),
        items: [
          { label: `Move ${count(many.length, "item")}…`, run: () => bulk("move", many) },
          { label: `Delete ${count(many.length, "item")}…`, hint: "Del", danger: true, separated: true, run: () => bulk("delete", many) },
        ],
      };
    }
    const folder = e.kind === "folder";
    const items: MenuItem[] = [
      { label: folder ? "Open folder" : "Open", hint: "Enter", run: () => open(e) },
      ...(folder
        ? [{ label: "Export to disk…", run: () => onAction(actionFor("export", e)) }]
        : [
            { label: "Download", run: () => onAction(actionFor("download", e)) },
            { label: "Replace with a file…", run: () => onAction(actionFor("replace", e)) },
          ]),
      { label: "Rename or move…", hint: "F2", separated: true, run: () => onAction(actionFor("move", e)) },
      { label: folder ? "Delete folder…" : "Delete file…", hint: "Del", danger: true, separated: true, run: () => onAction(actionFor("delete", e)) },
    ];
    return { key: e.path, label: e.name, items };
  };

  const closeMenu = useCallback((refocus: boolean) => {
    setMenu(null);
    if (refocus) listRef.current?.focus();
  }, []);

  const changeSort = (id: ColumnId) => {
    const next = nextSort(sort, id);
    setSort(next);
    save("textdb.folderSort", next);
  };

  const changeColumns = (ids: ColumnId[]) => {
    setColumns(ids);
    save("textdb.folderColumns", ids);
  };

  const goUp = () => {
    if (path !== "/") onOpenFolder(parentOf(path));
  };

  const onGridKey = (ev: KeyboardEvent<HTMLDivElement>) => {
    if (ev.target !== ev.currentTarget) return;
    const last = total - 1;
    const cur = Math.min(active, Math.max(0, last));
    const entry = entryAt(cur);
    const usable = entry && !isRemoved(entry.path) ? entry : undefined;
    const page = Math.max(1, Math.floor(((listRef.current?.clientHeight ?? 400) - HEAD) / ROW) - 1);
    let next: number | null = null;
    switch (ev.key) {
      case "ArrowDown":
        next = Math.min(last, cur + 1);
        break;
      case "ArrowUp":
        if (ev.altKey) goUp();
        else next = Math.max(0, cur - 1);
        break;
      case "PageDown":
        next = Math.min(last, cur + page);
        break;
      case "PageUp":
        next = Math.max(0, cur - page);
        break;
      case "Home":
        next = 0;
        break;
      case "End":
        next = Math.max(0, last);
        break;
      case "Enter":
        if (usable) open(usable);
        break;
      case "Backspace":
        goUp();
        break;
      case " ":
        if (usable) {
          toggleSelect(usable);
          anchor.current = cur;
        }
        break;
      case "a":
      case "A":
        if (!(ev.ctrlKey || ev.metaKey)) return;
        selectRange(0, last);
        break;
      case "Escape":
        if (!selected.size) return;
        setSelected(new Map());
        break;
      case "Delete":
        if (selected.size > 1) bulk("delete", [...selected.values()]);
        else if (selected.size === 1) onAction(actionFor("delete", [...selected.values()][0]!));
        else if (usable) onAction(actionFor("delete", usable));
        break;
      case "F2":
        if (usable) onAction(actionFor("move", usable));
        break;
      case "F10":
      case "ContextMenu": {
        if (ev.key === "F10" && !ev.shiftKey) return;
        const r = document.getElementById(`frow-${cur}`)?.getBoundingClientRect();
        if (usable && r) setMenu({ ...menuFor(usable), x: r.left + CHECK, y: r.bottom });
        break;
      }
      default:
        return;
    }
    ev.preventDefault();
    if (next === null) return;
    if (ev.shiftKey) selectRange(anchor.current, next);
    else anchor.current = next;
    setActive(next);
    ensureRowVisible(listRef.current, next, ROW, HEAD);
  };

  const onViewKey = (ev: KeyboardEvent<HTMLDivElement>) => {
    if (ev.key === "F4" || (ev.altKey && ev.key.toLowerCase() === "d")) {
      ev.preventDefault();
      setEditSignal((n) => n + 1);
    } else if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "f") {
      ev.preventDefault();
      filterRef.current?.focus();
      filterRef.current?.select();
    }
  };

  const onRowClick = (ev: MouseEvent, i: number, e: LsEntry) => {
    listRef.current?.focus({ preventScroll: true });
    if (ev.shiftKey) selectRange(anchor.current, i);
    else if (ev.ctrlKey || ev.metaKey) {
      toggleSelect(e);
      anchor.current = i;
    } else anchor.current = i;
    setActive(i);
  };

  // ---- rendering ------------------------------------------------------------------------------

  const grid = `${CHECK}px ${columns.map((id) => columnOf(id).width).join(" ")}`;
  const minWidth = CHECK + columns.reduce((n, id) => n + trackMin(columnOf(id).width) + 12, 0);
  const loaded = [...listing.pages.values()].flat().filter((e) => !isRemoved(e.path));
  const allSelected = loaded.length > 0 && loaded.every((e) => selected.has(e.path));

  const cell = (id: ColumnId, e: LsEntry) => {
    switch (id) {
      case "name": {
        const within = recursive ? parentOf(e.path).slice(path === "/" ? 1 : path.length + 1) : "";
        const asset = assetOf(e);
        const label = asset ? stateLabel(asset.state) : null;
        return (
          <>
            <span className={e.kind === "folder" ? "icon icon-folder" : "icon icon-file"} aria-hidden="true" />
            <a
              className="fname"
              tabIndex={-1}
              href={hashFor(e.kind === "folder" ? { type: "folder", path: e.path } : { type: "file", path: e.path, line: null })}
              onClick={(ev) => {
                if (ev.ctrlKey || ev.metaKey || ev.button !== 0) return ev.stopPropagation();
                ev.preventDefault();
                if (ev.shiftKey) return;
                ev.stopPropagation();
                if (!isRemoved(e.path)) open(e);
              }}
            >
              {e.kind === "file" && isPointer(e.name) ? assetPath(e.name) : e.name}
            </a>
            {label && (
              <span className={`badge asset-${label.tone} fasset`} title={asset?.note ?? label.hint}>
                {label.label}
              </span>
            )}
            {/* Held read-only: the row says so where it is, rather than leaving a save to find
                out. The owner's rows carry no rights at all, so nothing is shown for them. */}
            {e.rights === "ro" && (
              <span className="rights rights-ro fasset" title={`Read-only: ${e.share ? `the share /${e.share}` : "this share"} is held ro`}>
                ro
              </span>
            )}
            {within && <span className="fwithin">{within}</span>}
          </>
        );
      }
      case "type":
        return e.kind === "folder" ? "Folder" : extensionOf(isPointer(e.name) ? assetPath(e.name) : e.name, e.kind).toUpperCase() || "File";
      case "size":
        // An asset's own size, where its state is known; its pointer's otherwise.
        return formatBytes(assetOf(e)?.size ?? e.nbytes ?? 0);
      case "lines":
        return num(e.nlines);
      case "words":
        return num(e.nwords);
      case "versions":
        return num(e.versions);
      case "created":
        return (
          <time dateTime={e.created_at} title={e.created_at}>
            {relativeTime(e.created_at, now)}
          </time>
        );
      case "updated":
        return e.updated_at ? (
          <time dateTime={e.updated_at} title={`${e.updated_at}${e.updated_by ? ` · ${e.updated_by}` : ""}`}>
            {relativeTime(e.updated_at, now)}
          </time>
        ) : null;
      case "authors":
        return e.kind === "folder" ? (
          <span className="muted">
            {count(e.files ?? 0, "file")}
            {e.folders ? ` · ${count(e.folders, "folder")}` : ""}
          </span>
        ) : (
          <Authors authors={e.authors} />
        );
    }
  };

  const header = (
    <div className="frow fhead" role="row" aria-rowindex={1} style={{ gridTemplateColumns: grid, height: HEAD }}>
      <span className="fcell check" role="columnheader">
        <input
          type="checkbox"
          aria-label="Select all loaded rows"
          checked={allSelected}
          ref={(el) => {
            if (el) el.indeterminate = selected.size > 0 && !allSelected;
          }}
          onChange={() => (allSelected ? setSelected(new Map()) : selectRange(0, total - 1))}
        />
      </span>
      {columns.map((id) => {
        const col = columnOf(id);
        const on = sort.key === id;
        return (
          <span
            key={id}
            role="columnheader"
            aria-sort={on ? (sort.order === "asc" ? "ascending" : "descending") : "none"}
            className={`fcell c-${id}${col.numeric ? " num" : ""}`}
          >
            <button type="button" className={`sort${on ? " on" : ""}`} onClick={() => changeSort(id)}>
              {col.label}
              <span className="sort-arrow" aria-hidden="true">
                {on ? (sort.order === "asc" ? "↑" : "↓") : ""}
              </span>
            </button>
          </span>
        );
      })}
    </div>
  );

  const renderRow = (i: number, style: CSSProperties) => {
    const e = entryAt(i);
    const rowStyle: CSSProperties = { ...style, gridTemplateColumns: grid };
    if (!e) {
      return (
        <div key={`row-${i}`} className="frow placeholder" role="row" aria-rowindex={i + 2} style={rowStyle}>
          <span />
          <span className="skeleton" />
        </div>
      );
    }
    const isGone = isRemoved(e.path);
    const isSelected = selected.has(e.path);
    const flash = flashes.current.get(e.path);
    const flashing = flash !== undefined && flash.until > Date.now();
    const classes = ["frow", i === active && "active", isSelected && "selected", isGone && "removed", flashing && "flash"];
    return (
      <div
        key={e.path}
        id={`frow-${i}`}
        role="row"
        aria-rowindex={i + 2}
        aria-selected={isSelected}
        className={classes.filter(Boolean).join(" ")}
        style={flashing ? ({ ...rowStyle, "--h": String(flash.hue) } as CSSProperties) : rowStyle}
        title={isGone ? "No longer here · Refresh to update the list" : undefined}
        onClick={(ev) => onRowClick(ev, i, e)}
        onDoubleClick={() => !isGone && open(e)}
        onContextMenu={(ev) => {
          ev.preventDefault();
          if (isGone) return;
          setActive(i);
          setMenu({ ...menuFor(e), x: ev.clientX, y: ev.clientY });
        }}
      >
        <span className="fcell check" role="gridcell">
          <input
            type="checkbox"
            tabIndex={-1}
            checked={isSelected}
            disabled={isGone}
            aria-label={`Select ${e.name}`}
            onClick={(ev) => ev.stopPropagation()}
            onChange={() => {
              toggleSelect(e);
              anchor.current = i;
            }}
          />
        </span>
        {columns.map((id) => (
          <span key={id} role="gridcell" className={`fcell c-${id}${columnOf(id).numeric ? " num" : ""}`}>
            {cell(id, e)}
          </span>
        ))}
      </div>
    );
  };

  const filtered = !contents && (isFiltered(filter) || recursive);

  return (
    <div className="folder" onKeyDown={onViewKey}>
      <div className="folder-bar">
        <PathBar
          path={path}
          editSignal={editSignal}
          onOpenFolder={onOpenFolder}
          onOpenFile={onOpenFile}
          onSearch={(text) => {
            setContents(false);
            setFilterText(text);
            filterRef.current?.focus();
          }}
        />
        <div className="folder-tools">
          <input
            ref={filterRef}
            className="folder-filter"
            type="search"
            value={filterText}
            onChange={(e) => setFilterText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape" && filterText) {
                e.stopPropagation();
                setFilterText("");
              } else if (e.key === "ArrowDown" && !contents) {
                e.preventDefault();
                listRef.current?.focus();
              }
            }}
            disabled={props}
            placeholder={
              props ? "Use the query box below" : contents ? `Search the text of ${label}` : `Filter ${label}: name, author:…, type:…`
            }
            aria-label={contents ? `Search the text of files in ${label}` : `Filter ${label}`}
            spellCheck={false}
          />
          <div className="segmented" role="group" aria-label="Search in">
            <button
              type="button"
              aria-pressed={!contents && !props}
              onClick={() => {
                setContents(false);
                setProps(false);
              }}
              title="Filter by name, author and type"
            >
              Names
            </button>
            <button
              type="button"
              aria-pressed={contents}
              onClick={() => {
                setContents(true);
                setProps(false);
              }}
              title="Full-text search inside this folder"
            >
              Contents
            </button>
            <button
              type="button"
              aria-pressed={props}
              onClick={() => {
                setProps(true);
                setContents(false);
              }}
              title="Search by front-matter property, and browse what properties this vault uses"
            >
              Properties
            </button>
          </div>
          <label className="toggle" title="List everything below this folder, not only what is directly in it">
            <input type="checkbox" checked={recursive} disabled={contents || props} onChange={(e) => setRecursive(e.target.checked)} />
            Subfolders
          </label>
          <ColumnChooser columns={columns} onChange={changeColumns} />
          <button
            type="button"
            className="btn btn-small"
            onClick={() => onAction({ op: "export", path, kind: "folder" })}
            title={`Write the files in ${label} to a folder on this computer`}
          >
            Export…
          </button>
          {syncLink && onSync && (
            <button
              type="button"
              className="btn btn-small"
              onClick={() => onSync(syncLink.prefix)}
              disabled={syncLink.running}
              title={`Sync ${label} with ${syncLink.dir}`}
            >
              {syncLink.running ? "Syncing…" : "Sync…"}
            </button>
          )}
        </div>
      </div>

      <div className="folder-status">
        <span className="folder-summary" role="status">
          {summary && (
            <>
              {summary.folders ? `${count(summary.folders, "folder")} · ` : ""}
              {count(summary.files ?? 0, "file")} · {formatBytes(summary.nbytes ?? 0)} · {count(summary.nwords ?? 0, "word")} ·{" "}
              {count(summary.versions, "version")}
              {summary.updated_at && summary.files ? ` · last change ${relativeTime(summary.updated_at, now)}` : ""}
            </>
          )}
          {filtered && listing.total !== null && !stale && <strong> · {count(listing.total, "match", "matches")}</strong>}
        </span>
        {syncLink && (
          <span className="sync-badge" title={syncLink.dir}>
            {syncLink.last ? (
              <>
                synced
                {syncLink.last.git?.commit && (
                  <>
                    {" "}
                    at <span className="mono">{syncLink.last.git.commit.slice(0, 7)}</span>
                    {syncLink.last.git.branch ? ` (${syncLink.last.git.branch})` : ""}
                  </>
                )}{" "}
                {relativeTime(syncLink.last.synced_at, now)}
                {syncLink.last.changed > 0 && ` · ${count(syncLink.last.changed, "file")} changed here since`}
                {syncLink.last.conflicts.length > 0 && (
                  <strong className="error-text"> · {count(syncLink.last.conflicts.length, "conflict")}</strong>
                )}
              </>
            ) : (
              "not synced yet"
            )}
          </span>
        )}
        {pending > 0 && !contents && (
          <button type="button" className="pending-pill" onClick={() => setNonce((n) => n + 1)} title="Reload the list in the chosen order">
            {count(pending, "change")} · Refresh
          </button>
        )}
      </div>

      {gone && syncLink && !syncLink.last ? (
        <div className="notice" role="status">
          <span>
            This folder is set up to sync with <span className="mono">{syncLink.dir}</span>; the first sync brings its files in.
          </span>
          {onSync && (
            <button type="button" className="btn btn-small" onClick={() => onSync(syncLink.prefix)}>
              Sync…
            </button>
          )}
        </div>
      ) : gone && (
        <div className="notice notice-deleted" role="alert">
          <span>This folder no longer exists; it was deleted or moved away.</span>
          <button type="button" className="btn btn-small" onClick={goUp}>
            Open the folder above
          </button>
        </div>
      )}

      {selected.size > 0 && !contents && (
        <div className="bulk-bar" role="toolbar" aria-label="Selected items">
          <strong>{count(selected.size, "item")} selected</strong>
          <button type="button" className="btn btn-small" onClick={() => bulk("move", [...selected.values()])}>
            Move…
          </button>
          <button type="button" className="btn btn-small btn-ghost danger" onClick={() => bulk("delete", [...selected.values()])}>
            Delete…
          </button>
          <button type="button" className="btn btn-small btn-ghost" onClick={() => setSelected(new Map())}>
            Clear
          </button>
        </div>
      )}

      {props ? (
        <MetaExplorer onOpen={onOpenFile} folder={path} />
      ) : contents ? (
        search ? (
          <div className="folder-hits">
            <SearchResults
              query={search.q}
              hits={search.hits}
              loading={search.loading}
              error={search.error}
              onOpen={onOpenFile}
              onBack={() => filterRef.current?.focus()}
            />
          </div>
        ) : (
          <div className="empty small">Type at least two characters to search the text of the files in {label}.</div>
        )
      ) : (
        <div className="fgrid-wrap">
          <VirtualList
            outerRef={listRef}
            className={`fgrid${stale ? " stale" : ""}`}
            role="grid"
            aria-label={`Contents of ${label}`}
            aria-rowcount={total + 1}
            aria-multiselectable
            aria-busy={listing.total === null || stale}
            aria-activedescendant={entryAt(active) ? `frow-${active}` : undefined}
            tabIndex={0}
            onKeyDown={onGridKey}
            count={total}
            rowHeight={ROW}
            header={header}
            headerHeight={HEAD}
            innerStyle={{ minWidth }}
            renderRow={renderRow}
            onRange={onRange}
          />
          {(listing.total === null || total === 0 || (listing.error && !stale)) && (
            <div className="fgrid-empty">
              {listing.error && gone && syncLink && !syncLink.last ? (
                "Nothing here yet."
              ) : listing.error ? (
                <>
                  <span className="error-text">{listing.error}</span>{" "}
                  <button type="button" className="btn btn-small" onClick={() => setNonce((n) => n + 1)}>
                    Retry
                  </button>
                </>
              ) : listing.total === null ? (
                "Loading…"
              ) : isFiltered(filter) ? (
                "Nothing here matches the filter."
              ) : (
                "This folder is empty."
              )}
            </div>
          )}
        </div>
      )}
      {menu && <ContextMenu {...menu} onClose={closeMenu} />}
    </div>
  );
}

function Authors({ authors }: { authors: AuthorCount[] }) {
  const shown = authors.slice(0, 2);
  return (
    <span className="authors" title={authorsText(authors)}>
      {shown.map((a) => (
        <span key={a.author ?? ""} className="author" style={authorStyle(a.author)}>
          <span className="chip" aria-hidden="true" />
          <span className="author-label">{a.author ?? "unknown"}</span>
          <span className="author-commits">{a.commits}</span>
        </span>
      ))}
      {authors.length > shown.length && <span className="author-more">+{authors.length - shown.length}</span>}
    </span>
  );
}

function ColumnChooser({ columns, onChange }: { columns: ColumnId[]; onChange: (ids: ColumnId[]) => void }) {
  const ref = useRef<HTMLDetailsElement>(null);
  useEffect(() => {
    const away = (e: Event) => {
      if (ref.current?.open && !ref.current.contains(e.target as Node)) ref.current.open = false;
    };
    window.addEventListener("mousedown", away, true);
    return () => window.removeEventListener("mousedown", away, true);
  }, []);
  return (
    <details ref={ref} className="columns-chooser">
      <summary className="btn btn-ghost btn-small">Columns</summary>
      <div className="columns-pop" role="group" aria-label="Columns shown">
        {COLUMNS.filter((c) => c.id !== "name").map((c) => (
          <label key={c.id}>
            <input
              type="checkbox"
              checked={columns.includes(c.id)}
              onChange={(e) =>
                onChange(COLUMNS.map((x) => x.id).filter((id) => id === "name" || (id === c.id ? e.target.checked : columns.includes(id))))
              }
            />
            {c.label}
          </label>
        ))}
      </div>
    </details>
  );
}
