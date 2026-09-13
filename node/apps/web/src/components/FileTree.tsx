import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent } from "react";
import { api, ApiError, type ChangeEvent, type LsEntry, type TrashEntry } from "../api";
import { authorHue } from "../live/color";
import { ancestorsOf, isWithin, parentOf } from "../live/paths";
import { relativeTime } from "../live/time";
import type { FeedHub } from "../state/hub";
import { actionFor, type PathAction, type TrashAction } from "../tree/actions";
import { ContextMenu, type MenuItem, type MenuState } from "./ContextMenu";
import { ensureRowVisible, VirtualList } from "./VirtualList";

const ROW = 24;
const MARK_MS = 5000;
/** The Trash's key in the tree; a trashed folder is `trash:<id>`. Live paths start with "/". */
const TRASH = "trash:";
const trashKey = (id: number) => `${TRASH}${id}`;
const isTrashKey = (key: string) => key.startsWith(TRASH);

interface Props {
  hub: FeedHub;
  openPath: string | null;
  /** The folder shown in the central listing, when one is. */
  openFolder: string | null;
  openTrashId: number | null;
  onOpen: (path: string, line?: number) => void;
  onOpenFolder: (path: string) => void;
  onOpenTrash: (id: number) => void;
  onAction: (action: PathAction) => void;
  onTrashAction: (action: TrashAction) => void;
}

type Row =
  | { type: "entry"; key: string; entry: LsEntry; depth: number; open: boolean }
  | { type: "trash-root"; key: string; depth: number; open: boolean; count: number | null }
  | { type: "trash"; key: string; entry: TrashEntry; depth: number; open: boolean }
  | { type: "status"; key: string; list: string; depth: number; text: string; error: boolean };

interface Lists<T> {
  children: Map<string, T[]>;
  loading: Set<string>;
  errors: Map<string, string>;
}

interface Marker {
  hue: number;
  until: number;
  seq: number;
  op: string;
}

/**
 * Lists fetched on demand, one per key. A forced load refreshes a list already shown. With
 * `onGone`, a list whose owner no longer exists (404) is dropped and reported instead of
 * shown as an error.
 */
function useLists<T>(fetchList: (key: string) => Promise<T[]>, onGone?: (key: string) => void) {
  const [data, setData] = useState<Lists<T>>(() => ({ children: new Map(), loading: new Set(), errors: new Map() }));
  const dataRef = useRef(data);
  dataRef.current = data;
  const inflight = useRef(new Map<string, Promise<void>>());
  const goneRef = useRef(onGone);
  goneRef.current = onGone;

  const load = useCallback(
    (key: string, force = false): Promise<void> => {
      if (!force && dataRef.current.children.has(key)) return Promise.resolve();
      const running = inflight.current.get(key);
      if (running && !force) return running;
      setData((d) => ({ ...d, loading: new Set(d.loading).add(key) }));
      const settle = (update: (d: Lists<T>) => Lists<T>) =>
        setData((d) => {
          const next = update(d);
          const loading = new Set(next.loading);
          loading.delete(key);
          return { ...next, loading };
        });
      const p = fetchList(key).then(
        (list) =>
          settle((d) => {
            const errors = new Map(d.errors);
            errors.delete(key);
            return { ...d, children: new Map(d.children).set(key, list), errors };
          }),
        (err: unknown) => {
          if (goneRef.current && err instanceof ApiError && err.status === 404) {
            settle((d) => ({ ...d, children: new Map([...d.children].filter(([k]) => k !== key)) }));
            goneRef.current(key);
            return;
          }
          settle((d) => ({ ...d, errors: new Map(d.errors).set(key, err instanceof Error ? err.message : String(err)) }));
        },
      );
      const tracked = p.finally(() => {
        if (inflight.current.get(key) === tracked) inflight.current.delete(key);
      });
      inflight.current.set(key, tracked);
      return tracked;
    },
    [fetchList],
  );

  const drop = useCallback(
    (match: (key: string) => boolean) =>
      setData((d) => {
        if (![...d.children.keys()].some(match)) return d;
        return { ...d, children: new Map([...d.children].filter(([k]) => !match(k))) };
      }),
    [],
  );

  return { data, dataRef, load, drop };
}

export function FileTree({
  hub,
  openPath,
  openFolder,
  openTrashId,
  onOpen,
  onOpenFolder,
  onOpenTrash,
  onAction,
  onTrashAction,
}: Props) {
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());
  const collapse = useCallback(
    (key: string) =>
      setExpanded((prev) => {
        if (!prev.has(key)) return prev;
        const next = new Set(prev);
        next.delete(key);
        return next;
      }),
    [],
  );
  const listDir = useCallback((dir: string) => api.ls(dir), []);
  const listTrash = useCallback((key: string) => api.trash(key === TRASH ? undefined : Number(key.slice(TRASH.length))), []);
  const live = useLists<LsEntry>(listDir);
  const trash = useLists<TrashEntry>(listTrash, collapse);
  const { data, dataRef, load, drop: dropLive } = live;
  const { data: trashData, dataRef: trashRef, load: loadTrash } = trash;

  const [active, setActive] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);
  const pendingReveal = useRef<string | null>(null);
  const [menu, setMenu] = useState<MenuState | null>(null);
  const closeMenu = useCallback((refocus: boolean) => {
    setMenu(null);
    if (refocus) listRef.current?.focus();
  }, []);

  useEffect(() => {
    void load("/");
    void loadTrash(TRASH);
  }, [load, loadTrash]);

  // The server may not be up yet when the page loads: keep retrying the root listing.
  const rootError = data.errors.get("/");
  useEffect(() => {
    if (!rootError) return;
    const t = setTimeout(() => {
      void load("/", true);
      void loadTrash(TRASH, true);
    }, 3000);
    return () => clearTimeout(t);
  }, [rootError, load, loadTrash]);

  const toggle = useCallback(
    (key: string) => {
      setExpanded((prev) => {
        const next = new Set(prev);
        if (next.has(key)) next.delete(key);
        else {
          next.add(key);
          void (isTrashKey(key) ? loadTrash(key) : load(key));
        }
        return next;
      });
    },
    [load, loadTrash],
  );

  const rows = useMemo(() => {
    const out: Row[] = [];
    const status = (list: string, depth: number, error: string | undefined) =>
      out.push({ type: "status", key: `status:${list}`, list, depth, text: error ? `${error} · click to retry` : "Loading…", error: !!error });
    const walk = (dir: string, depth: number) => {
      const kids = data.children.get(dir);
      if (!kids) return status(dir, depth, data.errors.get(dir));
      if (kids.length === 0 && dir !== "/") {
        out.push({ type: "status", key: `status:${dir}`, list: dir, depth, text: "Empty folder", error: false });
      }
      for (const entry of kids) {
        const open = entry.kind === "folder" && expanded.has(entry.path);
        out.push({ type: "entry", key: entry.path, entry, depth, open });
        if (open) walk(entry.path, depth + 1);
      }
    };
    const walkTrash = (key: string, depth: number) => {
      const kids = trashData.children.get(key);
      if (!kids) return status(key, depth, trashData.errors.get(key));
      if (kids.length === 0) {
        const text = key === TRASH ? "Trash is empty" : "Empty folder";
        out.push({ type: "status", key: `status:${key}`, list: key, depth, text, error: false });
      }
      for (const entry of kids) {
        const k = trashKey(entry.id);
        const open = entry.kind === "folder" && expanded.has(k);
        out.push({ type: "trash", key: k, entry, depth, open });
        if (open) walkTrash(k, depth + 1);
      }
    };
    walk("/", 0);
    const trashOpen = expanded.has(TRASH);
    out.push({ type: "trash-root", key: TRASH, depth: 0, open: trashOpen, count: trashData.children.get(TRASH)?.length ?? null });
    if (trashOpen) walkTrash(TRASH, 1);
    return out;
  }, [data, trashData, expanded]);

  // ---- live markers and refreshes ----------------------------------------------------------

  const markers = useRef(new Map<string, Marker>());
  const [, setMarkRev] = useState(0);
  const bumpTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const refreshTimers = useRef(new Map<string, ReturnType<typeof setTimeout>>());

  useEffect(() => {
    const scheduleRefresh = (dir: string) => {
      if (!dataRef.current.children.has(dir)) return;
      const t = refreshTimers.current.get(dir);
      if (t) clearTimeout(t);
      refreshTimers.current.set(
        dir,
        setTimeout(() => {
          refreshTimers.current.delete(dir);
          void load(dir, true);
        }, 300),
      );
    };
    // A delete adds to the trash and a purge may take a trashed folder with it, so every
    // trash list on screen is reloaded; one that is gone collapses.
    let trashTimer: ReturnType<typeof setTimeout> | null = null;
    const refreshTrash = () => {
      if (trashTimer) clearTimeout(trashTimer);
      trashTimer = setTimeout(() => {
        trashTimer = null;
        const keys = new Set([TRASH, ...trashRef.current.children.keys()]);
        for (const key of keys) void loadTrash(key, true);
      }, 300);
    };
    const forget = (path: string) => {
      dropLive((k) => k === path || isWithin(path, k));
      setExpanded((prev) => new Set([...prev].filter((k) => k !== path && !isWithin(path, k))));
    };
    const onEvent = (e: ChangeEvent) => {
      if (e.op === "delete" || e.op === "purge") refreshTrash();
      if (e.op === "purge") return;
      const mark: Marker = { hue: authorHue(e.author ?? ""), until: Date.now() + MARK_MS, seq: e.seq, op: e.op };
      for (const p of [e.path, ...ancestorsOf(e.path)]) markers.current.set(p, mark);
      if (!bumpTimer.current) {
        bumpTimer.current = setTimeout(() => {
          bumpTimer.current = null;
          setMarkRev((r) => r + 1);
        }, 50);
      }
      if (e.op === "commit") return;
      if (e.op === "delete") forget(e.path);
      if (e.op === "move" && e.old_path) {
        forget(e.old_path);
        scheduleRefresh(parentOf(e.old_path));
      }
      scheduleRefresh(parentOf(e.path));
    };
    const off = hub.events.on(onEvent);
    const sweep = setInterval(() => {
      const now = Date.now();
      let removed = false;
      for (const [k, m] of markers.current) {
        if (m.until < now) {
          markers.current.delete(k);
          removed = true;
        }
      }
      if (removed) setMarkRev((r) => r + 1);
    }, 1000);
    const timers = refreshTimers.current;
    return () => {
      off();
      clearInterval(sweep);
      timers.forEach(clearTimeout);
      if (trashTimer) clearTimeout(trashTimer);
      if (bumpTimer.current) clearTimeout(bumpTimer.current);
    };
  }, [hub, load, loadTrash, dropLive, dataRef, trashRef]);

  // ---- reveal the open file ------------------------------------------------------------------

  // The open file, or else the folder in the central listing.
  const revealPath = openPath ?? (openFolder && openFolder !== "/" ? openFolder : null);
  useEffect(() => {
    if (!revealPath) return;
    let cancelled = false;
    const dirs = ancestorsOf(revealPath);
    void (async () => {
      await load("/");
      for (const dir of dirs) {
        if (cancelled) return;
        await load(dir);
      }
      if (cancelled) return;
      pendingReveal.current = revealPath;
      setExpanded((prev) => (dirs.every((d) => prev.has(d)) ? new Set(prev) : new Set([...prev, ...dirs])));
    })();
    return () => {
      cancelled = true;
    };
  }, [revealPath, load]);

  useEffect(() => {
    const target = pendingReveal.current;
    if (!target) return;
    const idx = rows.findIndex((r) => r.type === "entry" && r.entry.path === target);
    if (idx < 0) return;
    pendingReveal.current = null;
    setActive(idx);
    const el = listRef.current;
    if (el && (idx * ROW < el.scrollTop || idx * ROW > el.scrollTop + el.clientHeight - ROW)) {
      el.scrollTop = Math.max(0, idx * ROW - el.clientHeight / 2);
    }
  }, [rows]);

  // ---- actions -----------------------------------------------------------------------------------

  const activeIdx = Math.min(active, Math.max(0, rows.length - 1));

  /** The key a row expands under, or null for a row that does not expand. */
  const expandKey = (row: Row | undefined): string | null => {
    if (row?.type === "entry") return row.entry.kind === "folder" ? row.entry.path : null;
    if (row?.type === "trash") return row.entry.kind === "folder" ? row.key : null;
    return row?.type === "trash-root" ? TRASH : null;
  };

  /** A folder opens in the central listing and expands; its twisty (or ←/→) only expands or collapses. */
  const activate = (row: Row | undefined) => {
    if (!row || row.type === "status") return;
    if (row.type === "entry" && row.entry.kind === "folder") {
      onOpenFolder(row.entry.path);
      if (!row.open) toggle(row.entry.path);
      return;
    }
    const key = expandKey(row);
    if (key) toggle(key);
    else if (row.type === "entry") onOpen(row.entry.path);
    else if (row.type === "trash") onOpenTrash(row.entry.id);
  };

  const remove = (row: Row | undefined) => {
    if (row?.type === "entry") onAction(actionFor("delete", row.entry));
    else if (row?.type === "trash") onTrashAction({ op: "purge", entry: row.entry });
    else if (row?.type === "trash-root" && row.count) onTrashAction({ op: "empty" });
  };

  const menuFor = (row: Row): Omit<MenuState, "x" | "y"> | null => {
    const expandOrOpen = (folder: boolean): MenuItem => ({
      label: folder ? "Expand or collapse" : "Open",
      hint: "Enter",
      run: () => activate(row),
    });
    switch (row.type) {
      case "entry": {
        const folder = row.entry.kind === "folder";
        return {
          key: row.key,
          label: row.entry.name,
          items: [
            ...(folder
              ? [
                  { label: "Open folder", hint: "Enter", run: () => onOpenFolder(row.entry.path) },
                  { label: row.open ? "Collapse" : "Expand", hint: row.open ? "←" : "→", run: () => toggle(row.entry.path) },
                ]
              : [expandOrOpen(false)]),
            ...(folder
              ? []
              : [
                  { label: "Download", run: () => onAction(actionFor("download", row.entry)) },
                  { label: "Replace with a file…", run: () => onAction(actionFor("replace", row.entry)) },
                ]),
            { label: "Rename or move…", hint: "F2", separated: !folder, run: () => onAction(actionFor("move", row.entry)) },
            { label: folder ? "Delete folder…" : "Delete file…", hint: "Del", danger: true, separated: true, run: () => remove(row) },
          ],
        };
      }
      case "trash":
        return {
          key: row.key,
          label: row.entry.name,
          items: [
            expandOrOpen(row.entry.kind === "folder"),
            { label: "Permanently remove…", hint: "Del", danger: true, separated: true, run: () => remove(row) },
          ],
        };
      case "trash-root":
        return {
          key: row.key,
          label: "Trash",
          items: [
            expandOrOpen(true),
            { label: "Permanently clean trash…", hint: "Del", danger: true, separated: true, disabled: !row.count, run: () => remove(row) },
          ],
        };
      default:
        return null;
    }
  };

  const openMenu = (i: number, row: Row, x: number, y: number) => {
    const m = menuFor(row);
    if (!m) return;
    setActive(i);
    setMenu({ ...m, x, y });
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (!rows.length) return;
    const row = rows[activeIdx];
    const key = expandKey(row);
    const open = row !== undefined && row.type !== "status" && row.open;
    const page = Math.max(1, Math.floor((listRef.current?.clientHeight ?? 400) / ROW) - 1);
    let next = activeIdx;
    switch (e.key) {
      case "ArrowDown":
        next = Math.min(rows.length - 1, activeIdx + 1);
        break;
      case "ArrowUp":
        next = Math.max(0, activeIdx - 1);
        break;
      case "PageDown":
        next = Math.min(rows.length - 1, activeIdx + page);
        break;
      case "PageUp":
        next = Math.max(0, activeIdx - page);
        break;
      case "Home":
        next = 0;
        break;
      case "End":
        next = rows.length - 1;
        break;
      case "ArrowRight":
        if (key) {
          if (!open) toggle(key);
          else next = Math.min(rows.length - 1, activeIdx + 1);
        }
        break;
      case "ArrowLeft":
        if (key && open) {
          toggle(key);
        } else if (row) {
          for (let i = activeIdx - 1; i >= 0; i--) {
            if (rows[i]!.depth < row.depth) {
              next = i;
              break;
            }
          }
        }
        break;
      case "Enter":
      case " ":
        activate(row);
        break;
      case "F2":
        if (row?.type === "entry") onAction(actionFor("move", row.entry));
        break;
      case "Delete":
        remove(row);
        break;
      case "ContextMenu": {
        const rect = document.getElementById(`tree-row-${activeIdx}`)?.getBoundingClientRect();
        if (row && rect) openMenu(activeIdx, row, rect.left + 24, rect.bottom);
        break;
      }
      default:
        return;
    }
    e.preventDefault();
    setActive(next);
    ensureRowVisible(listRef.current, next, ROW);
  };

  const now = Date.now();
  const activeRow = rows[activeIdx];

  const rowEvents = (i: number, row: Row) => ({
    onClick: () => {
      setActive(i);
      activate(row);
    },
    onContextMenu: (e: MouseEvent) => {
      e.preventDefault();
      openMenu(i, row, e.clientX, e.clientY);
    },
  });

  const more = (i: number, row: Row, name: string) => (
    <button
      type="button"
      className="tree-more"
      tabIndex={-1}
      aria-label={`Actions for ${name}`}
      aria-haspopup="menu"
      aria-expanded={menu?.key === row.key}
      title="Actions"
      onClick={(e) => {
        e.stopPropagation();
        const r = e.currentTarget.getBoundingClientRect();
        openMenu(i, row, r.left, r.bottom + 2);
      }}
    >
      ⋯
    </button>
  );

  return (
    <>
      <VirtualList
        outerRef={listRef}
        className="tree"
        role="tree"
        aria-label="Files"
        tabIndex={0}
        aria-activedescendant={activeRow ? `tree-row-${activeIdx}` : undefined}
        onKeyDown={onKeyDown}
        count={rows.length}
        rowHeight={ROW}
        renderRow={(i, style) => {
          const row = rows[i]!;
          const pad = 8 + row.depth * 14;
          const activeClass = i === activeIdx ? " active" : "";
          if (row.type === "status") {
            return (
              <div
                key={row.key}
                id={`tree-row-${i}`}
                role="none"
                className={`tree-status${row.error ? " error-text retry" : ""}`}
                style={{ ...style, paddingLeft: pad + 18 }}
                onClick={row.error ? () => void (isTrashKey(row.list) ? loadTrash(row.list, true) : load(row.list, true)) : undefined}
              >
                {row.text}
              </div>
            );
          }
          if (row.type === "trash-root") {
            return (
              <div
                key={row.key}
                id={`tree-row-${i}`}
                role="treeitem"
                aria-level={1}
                aria-expanded={row.open}
                className={`tree-row tree-trash-root${activeClass}`}
                style={{ ...style, paddingLeft: pad }}
                title="Deleted files and folders stay here until they are permanently removed"
                {...rowEvents(i, row)}
              >
                <span className={`twisty${row.open ? " open" : ""}`} aria-hidden="true" />
                <span className="icon icon-trash" aria-hidden="true" />
                <span className="tree-name">Trash</span>
                {row.count ? <span className="tree-count">{row.count.toLocaleString()}</span> : null}
                {more(i, row, "Trash")}
              </div>
            );
          }
          if (row.type === "trash") {
            const { entry } = row;
            const folder = entry.kind === "folder";
            const selected = !folder && entry.id === openTrashId;
            const when = relativeTime(entry.deleted_at, now);
            return (
              <div
                key={row.key}
                id={`tree-row-${i}`}
                role="treeitem"
                aria-level={row.depth + 1}
                aria-expanded={folder ? row.open : undefined}
                aria-selected={selected}
                className={`tree-row trashed${activeClass}${selected ? " selected" : ""}`}
                style={{ ...style, paddingLeft: pad }}
                title={`${entry.path}\ndeleted${entry.deleted_by ? ` by ${entry.deleted_by}` : ""} ${when}`}
                {...rowEvents(i, row)}
              >
                <span className={`twisty${folder ? (row.open ? " open" : "") : " none"}`} aria-hidden="true" />
                <span className={folder ? "icon icon-folder" : "icon icon-file"} aria-hidden="true" />
                <span className="tree-name">{entry.name}</span>
                {row.depth === 1 && <span className="tree-meta">{when}</span>}
                {more(i, row, entry.name)}
              </div>
            );
          }
          const { entry } = row;
          const folder = entry.kind === "folder";
          const mark = markers.current.get(entry.path);
          const markLive = mark && mark.until > now;
          const isSelected = folder ? openPath === null && entry.path === openFolder : entry.path === openPath;
          return (
            <div
              key={row.key}
              id={`tree-row-${i}`}
              role="treeitem"
              aria-level={row.depth + 1}
              aria-expanded={folder ? row.open : undefined}
              aria-selected={isSelected}
              className={`tree-row${activeClass}${isSelected ? " selected" : ""}`}
              style={{ ...style, paddingLeft: pad }}
              title={entry.path}
              {...rowEvents(i, row)}
            >
              <span
                className={`twisty${folder ? (row.open ? " open" : "") : " none"}`}
                aria-hidden="true"
                onClick={
                  folder
                    ? (e) => {
                        e.stopPropagation();
                        setActive(i);
                        toggle(entry.path);
                      }
                    : undefined
                }
              />
              <span className={folder ? "icon icon-folder" : "icon icon-file"} aria-hidden="true" />
              <span className="tree-name">{entry.name}</span>
              {markLive && (
                <span key={mark.seq} className={`tree-marker op-${mark.op}`} style={{ "--h": String(mark.hue) } as React.CSSProperties} aria-label="changed just now" />
              )}
              {more(i, row, entry.name)}
            </div>
          );
        }}
      />
      {menu && <ContextMenu {...menu} onClose={closeMenu} />}
    </>
  );
}
