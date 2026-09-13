/** Which files a folder import picks up, where they land, and how they are sent. */

export const DEFAULT_EXTENSIONS = ["md", "markdown", "mdx", "txt"];

/** Folders an import never reads: version control, dependencies, anything hidden. */
export function skipDirectory(name: string): boolean {
  return name.startsWith(".") || name === "node_modules";
}

/** "md, .markdown *.txt" → ["md", "markdown", "txt"]; "*" means every file. */
export function parseExtensions(input: string): string[] {
  const exts = input
    .split(/[\s,;]+/)
    .map((e) => e.trim().replace(/^\*?\./, "").toLowerCase())
    .filter(Boolean);
  return [...new Set(exts)];
}

/** Whether the file at `rel` (a /-separated path inside the picked folder) is imported. */
export function includeFile(rel: string, extensions: readonly string[]): boolean {
  const parts = rel.split("/");
  if (parts.slice(0, -1).some(skipDirectory)) return false;
  const name = parts[parts.length - 1] ?? "";
  if (name.startsWith(".")) return false;
  if (extensions.includes("*")) return true;
  const dot = name.lastIndexOf(".");
  return dot > 0 && extensions.includes(name.slice(dot + 1).toLowerCase());
}

/** " docs/guides/ " → "/docs/guides", "" → "/". `null` for a folder the store would refuse. */
export function normalizePrefix(input: string): string | null {
  const segs = input.split(/[\\/]+/).map((s) => s.trim()).filter(Boolean);
  if (segs.some((s) => s === "." || s === "..")) return null;
  return "/" + segs.join("/");
}

export function destPath(prefix: string, rel: string): string {
  return prefix === "/" ? `/${rel}` : `${prefix}/${rel}`;
}

/**
 * Group files into requests of at most `maxBytes` of content or `maxFiles` files, keeping
 * their order. A file larger than `maxBytes` travels alone.
 */
export function planBatches<T extends { size: number }>(files: readonly T[], maxBytes: number, maxFiles: number): T[][] {
  const batches: T[][] = [];
  let current: T[] = [];
  let bytes = 0;
  for (const file of files) {
    if (current.length > 0 && (bytes + file.size > maxBytes || current.length >= maxFiles)) {
      batches.push(current);
      current = [];
      bytes = 0;
    }
    current.push(file);
    bytes += file.size;
  }
  if (current.length > 0) batches.push(current);
  return batches;
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v >= 100 ? v.toFixed(0) : v.toFixed(1)} ${units[i]}`;
}

export function formatDuration(seconds: number): string {
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.floor(s / 60)} min ${s % 60} s`;
  return `${Math.floor(s / 3600)} h ${Math.floor((s % 3600) / 60)} min`;
}
