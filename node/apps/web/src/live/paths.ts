/** Store paths are absolute, `/`-separated, with no trailing slash (except the root). */

export function parentOf(path: string): string {
  const i = path.lastIndexOf("/");
  return i <= 0 ? "/" : path.slice(0, i);
}

export function baseName(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1) || "/";
}

/** True when `path` lies strictly inside folder `dir`. */
export function isWithin(dir: string, path: string): boolean {
  if (dir === "/") return path !== "/" && path.startsWith("/");
  return path.startsWith(dir + "/");
}

/** The folders above `path`, outermost first, without the root: `/a/b/c.md` → `/a`, `/a/b`. */
export function ancestorsOf(path: string): string[] {
  const parts = path.split("/").filter(Boolean);
  const out: string[] = [];
  let cur = "";
  for (let i = 0; i < parts.length - 1; i++) {
    cur += "/" + parts[i];
    out.push(cur);
  }
  return out;
}
