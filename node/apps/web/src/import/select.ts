/** Which files a folder import picks up and where they land. */

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

/** The selection entry for files without an extension (`Makefile`, `LICENSE`). */
export const NO_EXTENSION = "(none)";

/** Extensions whose files are almost always binary, which an import skips anyway. */
const LIKELY_BINARY = new Set(
  "png jpg jpeg gif webp bmp ico tif tiff heic svgz pdf zip gz tgz bz2 xz 7z rar jar exe dll so dylib bin dat db sqlite woff woff2 ttf otf eot mp3 mp4 m4a mov avi mkv wav flac ogg webm psd ai doc docx xls xlsx ppt pptx odt ods key pages numbers class o a pyc".split(
    " ",
  ),
);

export function isLikelyBinary(ext: string): boolean {
  return LIKELY_BINARY.has(ext);
}

/**
 * The extension of the file at `rel` (a /-separated path inside the picked folder), lower case,
 * `NO_EXTENSION` when it has none, or `null` when the file is never read: hidden, or inside a
 * hidden folder or `node_modules`.
 */
export function extensionOf(rel: string): string | null {
  const parts = rel.split("/");
  if (parts.slice(0, -1).some(skipDirectory)) return null;
  const name = parts[parts.length - 1] ?? "";
  if (name.startsWith(".")) return null;
  const dot = name.lastIndexOf(".");
  return dot > 0 && dot < name.length - 1 ? name.slice(dot + 1).toLowerCase() : NO_EXTENSION;
}

/** Whether the file at `rel` is imported. */
export function includeFile(rel: string, extensions: readonly string[]): boolean {
  const ext = extensionOf(rel);
  return ext !== null && (extensions.includes("*") || extensions.includes(ext));
}

export interface ExtensionCount {
  ext: string;
  files: number;
  /** Total size, when the browser gave the sizes up front. */
  bytes: number | null;
}

/** The extensions in a picked folder, most files first, counting only files an import could read. */
export function countExtensions(files: readonly { rel: string; size: number | null }[]): ExtensionCount[] {
  const counts = new Map<string, ExtensionCount>();
  for (const f of files) {
    const ext = extensionOf(f.rel);
    if (ext === null) continue;
    const c = counts.get(ext) ?? { ext, files: 0, bytes: 0 };
    c.files += 1;
    c.bytes = c.bytes === null || f.size === null ? null : c.bytes + f.size;
    counts.set(ext, c);
  }
  return [...counts.values()].sort(
    (a, b) =>
      b.files - a.files ||
      Number(a.ext === NO_EXTENSION) - Number(b.ext === NO_EXTENSION) ||
      a.ext.localeCompare(b.ext),
  );
}

/**
 * Tick or untick one extension. From `*` (everything), unticking one keeps every other
 * extension found in the folder.
 */
export function toggleExtension(selected: readonly string[], ext: string, on: boolean, found: readonly string[]): string[] {
  const base = selected.includes("*") ? [...found] : [...selected];
  return on ? [...new Set([...base, ext])] : base.filter((e) => e !== ext);
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
