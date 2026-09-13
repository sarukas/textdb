/**
 * What the location hash opens, so a view can be linked and survives a reload:
 * `#/guides/intro.md` a file (`:42` scrolls to line 42), `#/guides/` a folder (`#/` the root),
 * `#trash:8144` a trashed file.
 */
export type Target =
  | { type: "file"; path: string; line: number | null }
  | { type: "folder"; path: string }
  | { type: "trash"; id: number };

/** `/a//b/` → `/a/b`; the root is `/`. */
export function folderPath(input: string): string {
  return `/${input.split("/").filter(Boolean).join("/")}`;
}

export function parseHash(hash: string): Target | null {
  const trash = /^#trash:(\d+)$/.exec(hash);
  if (trash) return { type: "trash", id: Number(trash[1]) };
  let raw: string;
  try {
    raw = decodeURIComponent(hash.replace(/^#/, ""));
  } catch {
    return null;
  }
  if (!raw.startsWith("/")) return null;
  if (raw.endsWith("/")) return { type: "folder", path: folderPath(raw) };
  const m = /^(\/.*?)(?::(\d+))?$/.exec(raw);
  if (!m?.[1]) return null;
  return { type: "file", path: m[1], line: m[2] ? Number(m[2]) : null };
}

export function hashFor(target: Target): string {
  switch (target.type) {
    case "trash":
      return `#trash:${target.id}`;
    case "folder":
      return `#${encodeURI(target.path === "/" ? "/" : `${target.path}/`)}`;
    case "file":
      return `#${encodeURI(target.path)}${target.line ? `:${target.line}` : ""}`;
  }
}
