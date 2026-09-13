/** The folder view's pure parts: columns, the filter syntax, paging, and what a change does to a listing. */
import type { AuthorCount, ChangeEvent, LsEntry, NodeKind, SortKey } from "../api";
import { ancestorsOf, isWithin, parentOf } from "../live/paths";

export type ColumnId = SortKey;

export interface Column {
  id: ColumnId;
  label: string;
  /** A CSS grid track. */
  width: string;
  numeric?: boolean;
  /** The first click sorts descending: biggest, newest, most first. */
  desc?: boolean;
}

export const COLUMNS: readonly Column[] = [
  { id: "name", label: "Name", width: "minmax(220px, 3fr)" },
  { id: "type", label: "Type", width: "minmax(96px, 0.9fr)" },
  { id: "size", label: "Size", width: "88px", numeric: true, desc: true },
  { id: "lines", label: "Lines", width: "80px", numeric: true, desc: true },
  { id: "words", label: "Words", width: "88px", numeric: true, desc: true },
  { id: "versions", label: "Versions", width: "80px", numeric: true, desc: true },
  { id: "created", label: "Created", width: "112px", desc: true },
  { id: "updated", label: "Updated", width: "112px", desc: true },
  { id: "authors", label: "Authors", width: "minmax(150px, 1.3fr)", desc: true },
];

export const DEFAULT_COLUMNS: readonly ColumnId[] = ["name", "type", "size", "words", "versions", "updated", "authors"];

/** Saved column choice, in column order, always with the name; the default when unreadable. */
export function readColumns(raw: string | null): ColumnId[] {
  try {
    const ids = JSON.parse(raw ?? "") as unknown;
    if (Array.isArray(ids)) {
      const chosen = COLUMNS.filter((c) => c.id === "name" || ids.includes(c.id)).map((c) => c.id);
      if (chosen.length > 1) return chosen;
    }
  } catch {
    // fall through
  }
  return [...DEFAULT_COLUMNS];
}

export interface Sort {
  key: SortKey;
  order: "asc" | "desc";
}

/** Clicking a column header: the same column flips the order, another starts at its natural one. */
export function nextSort(current: Sort, key: SortKey): Sort {
  if (current.key === key) return { key, order: current.order === "asc" ? "desc" : "asc" };
  return { key, order: COLUMNS.find((c) => c.id === key)?.desc ? "desc" : "asc" };
}

/** A file's extension as the store sorts it: lower case, without the dot; "" for folders and names without one. */
export function extensionOf(name: string, kind: NodeKind): string {
  if (kind === "folder") return "";
  const dot = name.lastIndexOf(".");
  return dot < 0 ? "" : name.slice(dot + 1).toLowerCase();
}

export interface Filter {
  name?: string;
  author?: string;
  type?: string;
  kind?: NodeKind;
}

/**
 * The filter box: `guide author:ann type:md is:file`. `author:` (or `by:`), `type:` (or `ext:`)
 * and `is:file|folder` narrow the listing; the rest matches names, as a glob when it has `*`
 * or `?`. Values with spaces are quoted: `author:"Ann Lee"`.
 */
export function parseFilter(text: string): Filter {
  const out: Filter = {};
  const rest: string[] = [];
  for (const m of text.matchAll(/(\w+):(?:"([^"]*)"|(\S*))|"([^"]*)"|(\S+)/g)) {
    const key = m[1]?.toLowerCase();
    const value = m[2] ?? m[3] ?? "";
    if (key === "author" || key === "by") out.author = value;
    else if (key === "type" || key === "ext") out.type = value.replace(/^\./, "");
    else if ((key === "is" || key === "kind") && (value === "file" || value === "folder")) out.kind = value;
    else rest.push(m[4] ?? m[0]);
  }
  const name = rest.join(" ").trim();
  if (name) out.name = name;
  if (out.type === "") delete out.type;
  return out;
}

export function isFiltered(f: Filter): boolean {
  return f.name !== undefined || f.author !== undefined || f.type !== undefined || f.kind !== undefined;
}

export const PAGE = 200;

/** The pages holding rows `[start, end)`. */
export function pagesIn(start: number, end: number, page = PAGE): number[] {
  const out: number[] = [];
  for (let p = Math.floor(Math.max(0, start) / page); p * page < end; p++) out.push(p);
  return out;
}

/** Paths without those inside another listed folder, which go along with it. */
export function topLevel(paths: Iterable<string>): string[] {
  const all = [...new Set(paths)].sort();
  return all.filter((p) => !all.some((q) => q !== p && isWithin(q, p)));
}

/** "ann (3), bo (1)" */
export function authorsText(authors: readonly AuthorCount[]): string {
  return authors.map((a) => `${a.author ?? "unknown"} (${a.commits})`).join(", ");
}

/** What one change means for a listing on screen. */
export interface ChangePlan {
  /** Rows whose figures changed: fetch them again. */
  refresh: string[];
  /** Rows that are gone from here, with everything inside them. */
  removed: string[];
  /** Rows that appeared; placing them needs a reload. */
  added: number;
  /** The folder itself moved (`to` its new path) or was deleted (`to` null). */
  folder: { to: string | null } | null;
}

/**
 * Plan the effect of `e` on the listing of `folder` (its own entries, or everything below it
 * when `recursive`). A change deep inside a subfolder changes that subfolder's totals, so the
 * row to refresh is the listed one that contains it.
 */
export function planChange(folder: string, recursive: boolean, e: ChangeEvent): ChangePlan {
  const plan: ChangePlan = { refresh: [], removed: [], added: 0, folder: null };
  if (e.op === "purge") return plan;
  const covers = (p: string) => p === folder || isWithin(p, folder);
  if (e.op === "delete" && covers(e.path)) {
    plan.folder = { to: null };
    return plan;
  }
  if (e.op === "move" && e.old_path && covers(e.old_path)) {
    plan.folder = { to: e.path + folder.slice(e.old_path.length) };
    return plan;
  }
  const refresh = new Set<string>();
  const inside = (p: string) => isWithin(folder, p);
  const listed = (p: string) => inside(p) && (recursive || parentOf(p) === folder);
  // The listed rows whose totals include `p`, not counting `p` itself.
  const containers = (p: string): string[] => {
    if (!inside(p)) return [];
    if (recursive) return ancestorsOf(p).filter(inside);
    const slash = p.indexOf("/", folder === "/" ? 1 : folder.length + 1);
    return slash < 0 ? [] : [p.slice(0, slash)];
  };
  const gone = (p: string) => {
    if (!inside(p)) return;
    if (listed(p)) plan.removed.push(p);
    containers(p).forEach((c) => refresh.add(c));
  };
  const came = (p: string) => {
    if (!inside(p)) return;
    if (listed(p)) plan.added += 1;
    containers(p).forEach((c) => refresh.add(c));
  };
  switch (e.op) {
    case "commit":
      if (listed(e.path)) refresh.add(e.path);
      containers(e.path).forEach((c) => refresh.add(c));
      break;
    case "create":
    case "mkdir":
      came(e.path);
      break;
    case "delete":
      gone(e.path);
      break;
    case "move":
      if (e.old_path) gone(e.old_path);
      came(e.path);
      break;
  }
  plan.refresh = [...refresh];
  return plan;
}

/** Whether the rows of `entry` are in order for `sort` whatever the change: name, type and creation never change in place. */
export function orderIsStable(key: SortKey): boolean {
  return key === "name" || key === "type" || key === "created";
}

export function entryLabel(e: LsEntry): string {
  return e.kind === "folder" ? `${e.name}/` : e.name;
}
