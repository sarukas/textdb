import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { api, type ChangeEvent, type LsEntry } from "../api";
import { authorHue } from "../live/color";
import { ancestorsOf, isWithin, parentOf } from "../live/paths";
import type { FeedHub } from "../state/hub";
import { actionFor, type PathAction } from "../tree/actions";
import { ensureRowVisible, VirtualList } from "./VirtualList";

const ROW = 24;
const MARK_MS = 5000;

interface Props {
  hub: FeedHub;
  openPath: string | null;
  onOpen: (path: string, line?: number) => void;
  onAction: (action: PathAction) => void;
}

type Row =
  | { type: "entry"; key: string; entry: LsEntry; depth: number; open: boolean }
  | { type: "status"; key: string; dir: string; depth: number; text: string; error: boolean };

interface TreeData {
  children: Map<string, LsEntry[]>;
  loading: Set<string>;
  errors: Map<string, string>;
}

interface Marker {
  hue: number;
  until: number;
  seq: number;
  op: string;
}

export function FileTree({ hub, openPath, onOpen, onAction }: Props) {
  const [data, setData] = useState<TreeData>(() => ({ children: new Map(), loading: new Set(), errors: new Map() }));
  const dataRef = useRef(data);
  dataRef.current = data;
  const inflight = useRef(new Map<string, Promise<void>>());
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());
  const [active, setActive] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);
  const pendingReveal = useRef<string | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; entry: LsEntry } | null>(null);
  const closeMenu = useCallback((refocus: boolean) => {
    setMenu(null);
    if (refocus) listRef.current?.focus();
  }, []);

  const load = useCallback((dir: string, force = false): Promise<void> => {
    if (!force && dataRef.current.children.has(dir)) return Promise.resolve();
    const running = inflight.current.get(dir);
    if (running && !force) return running;
    setData((d) => ({ ...d, loading: new Set(d.loading).add(dir) }));
    const p = api.ls(dir).then(
      (list) =>
        setData((d) => {
          const loading = new Set(d.loading);
          loading.delete(dir);
          const errors = new Map(d.errors);
          errors.delete(dir);
          return { children: new Map(d.children).set(dir, list), loading, errors };
        }),
      (err: unknown) =>
        setData((d) => {
          const loading = new Set(d.loading);
          loading.delete(dir);
          return { ...d, loading, errors: new Map(d.errors).set(dir, err instanceof Error ? err.message : String(err)) };
        }),
    );
    const tracked = p.finally(() => {
      if (inflight.current.get(dir) === tracked) inflight.current.delete(dir);
    });
    inflight.current.set(dir, tracked);
    return tracked;
  }, []);

  useEffect(() => {
    void load("/");
  }, [load]);

  // The server may not be up yet when the page loads: keep retrying the root listing.
  const rootError = data.errors.get("/");
  useEffect(() => {
    if (!rootError) return;
    const t = setTimeout(() => void load("/", true), 3000);
    return () => clearTimeout(t);
  }, [rootError, load]);

  const toggle = useCallback(
    (path: string) => {
      setExpanded((prev) => {
        const next = new Set(prev);
        if (next.has(path)) next.delete(path);
        else {
          next.add(path);
          void load(path);
        }
        return next;
      });
    },
    [load],
  );

  const rows = useMemo(() => {
    const out: Row[] = [];
    const walk = (dir: string, depth: number) => {
      const kids = data.children.get(dir);
      if (!kids) {
        const err = data.errors.get(dir);
        out.push({ type: "status", key: `status:${dir}`, dir, depth, text: err ? `${err} · click to retry` : "Loading…", error: !!err });
        return;
      }
      if (kids.length === 0 && dir !== "/") {
        out.push({ type: "status", key: `status:${dir}`, dir, depth, text: "Empty folder", error: false });
      }
      for (const entry of kids) {
        const open = entry.kind === "folder" && expanded.has(entry.path);
        out.push({ type: "entry", key: entry.path, entry, depth, open });
        if (open) walk(entry.path, depth + 1);
      }
    };
    walk("/", 0);
    return out;
  }, [data, expanded]);

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
    const forget = (path: string) => {
      setData((d) => {
        if (![...d.children.keys()].some((k) => k === path || isWithin(path, k))) return d;
        return { ...d, children: new Map([...d.children].filter(([k]) => k !== path && !isWithin(path, k))) };
      });
      setExpanded((prev) => new Set([...prev].filter((k) => k !== path && !isWithin(path, k))));
    };
    const onEvent = (e: ChangeEvent) => {
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
      if (bumpTimer.current) clearTimeout(bumpTimer.current);
    };
  }, [hub, load]);

  // ---- reveal the open file ------------------------------------------------------------------

  useEffect(() => {
    if (!openPath) return;
    let cancelled = false;
    const dirs = ancestorsOf(openPath);
    void (async () => {
      await load("/");
      for (const dir of dirs) {
        if (cancelled) return;
        await load(dir);
      }
      if (cancelled) return;
      pendingReveal.current = openPath;
      setExpanded((prev) => (dirs.every((d) => prev.has(d)) ? new Set(prev) : new Set([...prev, ...dirs])));
    })();
    return () => {
      cancelled = true;
    };
  }, [openPath, load]);

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

  // ---- keyboard ----------------------------------------------------------------------------------

  const activeIdx = Math.min(active, Math.max(0, rows.length - 1));

  const activate = (row: Row | undefined) => {
    if (!row || row.type !== "entry") return;
    if (row.entry.kind === "folder") toggle(row.entry.path);
    else onOpen(row.entry.path);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (!rows.length) return;
    const row = rows[activeIdx];
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
        if (row?.type === "entry" && row.entry.kind === "folder") {
          if (!row.open) toggle(row.entry.path);
          else next = Math.min(rows.length - 1, activeIdx + 1);
        }
        break;
      case "ArrowLeft":
        if (row?.type === "entry" && row.entry.kind === "folder" && row.open) {
          toggle(row.entry.path);
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
        if (row?.type === "entry") onAction(actionFor("delete", row.entry));
        break;
      case "ContextMenu":
        if (row?.type === "entry") {
          const el = document.getElementById(`tree-row-${activeIdx}`)?.getBoundingClientRect();
          if (el) setMenu({ x: el.left + 24, y: el.bottom, entry: row.entry });
        }
        break;
      default:
        return;
    }
    e.preventDefault();
    setActive(next);
    ensureRowVisible(listRef.current, next, ROW);
  };

  const now = Date.now();
  const activeRow = rows[activeIdx];

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
        if (row.type === "status") {
          return (
            <div
              key={row.key}
              id={`tree-row-${i}`}
              role="none"
              className={`tree-status${row.error ? " error-text retry" : ""}`}
              style={{ ...style, paddingLeft: pad + 18 }}
              onClick={row.error ? () => void load(row.dir, true) : undefined}
            >
              {row.text}
            </div>
          );
        }
        const { entry } = row;
        const folder = entry.kind === "folder";
        const mark = markers.current.get(entry.path);
        const live = mark && mark.until > now;
        return (
          <div
            key={row.key}
            id={`tree-row-${i}`}
            role="treeitem"
            aria-level={row.depth + 1}
            aria-expanded={folder ? row.open : undefined}
            aria-selected={entry.path === openPath}
            className={`tree-row${i === activeIdx ? " active" : ""}${entry.path === openPath ? " selected" : ""}`}
            style={{ ...style, paddingLeft: pad }}
            title={entry.path}
            onClick={() => {
              setActive(i);
              activate(row);
            }}
            onContextMenu={(e) => {
              e.preventDefault();
              setActive(i);
              setMenu({ x: e.clientX, y: e.clientY, entry });
            }}
          >
            <span className={`twisty${folder ? (row.open ? " open" : "") : " none"}`} aria-hidden="true" />
            <span className={folder ? "icon icon-folder" : "icon icon-file"} aria-hidden="true" />
            <span className="tree-name">{entry.name}</span>
            {live && (
              <span key={mark.seq} className={`tree-marker op-${mark.op}`} style={{ "--h": String(mark.hue) } as React.CSSProperties} aria-label="changed just now" />
            )}
            <button
              type="button"
              className="tree-more"
              tabIndex={-1}
              aria-label={`Actions for ${entry.name}`}
              aria-haspopup="menu"
              aria-expanded={menu?.entry.path === entry.path}
              title="Rename, move or delete"
              onClick={(e) => {
                e.stopPropagation();
                const r = e.currentTarget.getBoundingClientRect();
                setActive(i);
                setMenu({ x: r.left, y: r.bottom + 2, entry });
              }}
            >
              ⋯
            </button>
          </div>
        );
      }}
    />
    {menu && (
      <TreeMenu
        {...menu}
        onClose={closeMenu}
        onOpen={() => (menu.entry.kind === "folder" ? toggle(menu.entry.path) : onOpen(menu.entry.path))}
        onAction={onAction}
      />
    )}
    </>
  );
}

interface MenuProps {
  x: number;
  y: number;
  entry: LsEntry;
  onClose: (refocus: boolean) => void;
  onOpen: () => void;
  onAction: (action: PathAction) => void;
}

/** A row's context menu: kept inside the window, closed by Escape, a click elsewhere or a choice. */
function TreeMenu({ x, y, entry, onClose, onOpen, onAction }: MenuProps) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ left: x, top: y });

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    setPos({
      left: Math.max(4, Math.min(x, window.innerWidth - r.width - 4)),
      top: Math.max(4, Math.min(y, window.innerHeight - r.height - 4)),
    });
    el.querySelector<HTMLElement>("[role=menuitem]")?.focus();
  }, [x, y]);

  useEffect(() => {
    const away = (e: Event) => {
      if (!ref.current?.contains(e.target as Node)) onClose(false);
    };
    const leave = () => onClose(false);
    window.addEventListener("mousedown", away, true);
    window.addEventListener("resize", leave);
    window.addEventListener("blur", leave);
    return () => {
      window.removeEventListener("mousedown", away, true);
      window.removeEventListener("resize", leave);
      window.removeEventListener("blur", leave);
    };
  }, [onClose]);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const items = Array.from(ref.current?.querySelectorAll<HTMLElement>("[role=menuitem]") ?? []);
    const at = items.indexOf(document.activeElement as HTMLElement);
    if (e.key === "Escape" || e.key === "Tab") onClose(true);
    else if (e.key === "ArrowDown") items[(at + 1) % items.length]?.focus();
    else if (e.key === "ArrowUp") items[(at - 1 + items.length) % items.length]?.focus();
    else return;
    e.preventDefault();
    e.stopPropagation();
  };

  const choose = (fn: () => void) => () => {
    onClose(false);
    fn();
  };
  const folder = entry.kind === "folder";

  return (
    <div
      ref={ref}
      className="menu"
      role="menu"
      aria-label={`Actions for ${entry.name}`}
      style={pos}
      onKeyDown={onKeyDown}
      onContextMenu={(e) => e.preventDefault()}
    >
      <button type="button" role="menuitem" onClick={choose(onOpen)}>
        {folder ? "Expand or collapse" : "Open"}
        <kbd>Enter</kbd>
      </button>
      <button type="button" role="menuitem" onClick={choose(() => onAction(actionFor("move", entry)))}>
        Rename or move…
        <kbd>F2</kbd>
      </button>
      <div role="separator" />
      <button type="button" role="menuitem" className="danger" onClick={choose(() => onAction(actionFor("delete", entry)))}>
        {folder ? "Delete folder…" : "Delete file…"}
        <kbd>Del</kbd>
      </button>
    </div>
  );
}
