import type { AssetItem, AssetState, SyncLink } from "../api";

/** The suffix of an asset's pointer document, kept next to where the real file belongs. */
export const POINTER_SUFFIX = ".tdbasset";

export function isPointer(path: string): boolean {
  return path.length > POINTER_SUFFIX.length && path.endsWith(POINTER_SUFFIX);
}

/** The asset a pointer stands for: `/img/a.png.tdbasset` → `/img/a.png`; other paths unchanged. */
export function assetPath(path: string): string {
  return isPointer(path) ? path.slice(0, -POINTER_SUFFIX.length) : path;
}

export function pointerPath(asset: string): string {
  return `${asset}${POINTER_SUFFIX}`;
}

/** The synced folder `path` is in (the innermost, when folders nest); null when none. */
export function syncLinkFor<L extends Pick<SyncLink, "prefix">>(path: string, links: readonly L[]): L | null {
  let best: L | null = null;
  for (const link of links) {
    const inside = link.prefix === "/" || path === link.prefix || path.startsWith(`${link.prefix}/`);
    if (inside && (!best || link.prefix.length > best.prefix.length)) best = link;
  }
  return best;
}

export interface StateLabel {
  label: string;
  /** How it reads at a glance: fine, work to do here, or a problem to look at. */
  tone: "ok" | "action" | "problem";
  /** What it means, and what to do about it. */
  hint: string;
}

const LABELS: Record<AssetState, StateLabel> = {
  ok: { label: "OK", tone: "ok", hint: "The file here is the asset's bytes." },
  new: { label: "New", tone: "action", hint: "A file with no pointer yet: push it to keep it in the asset store." },
  modified: { label: "Changed", tone: "action", hint: "Changed here since it was pulled or pushed: push it to publish the change." },
  outdated: { label: "Outdated", tone: "action", hint: "The store has newer bytes: pull them." },
  conflict: { label: "Conflict", tone: "problem", hint: "Changed both here and in the store: move the file aside and pull to compare." },
  "not-pulled": { label: "Not pulled", tone: "action", hint: "Its file is not on the server's disk: pull it to see it." },
  "conflict-copy": { label: "Conflict copy", tone: "problem", hint: "Kept from a conflict: compare it with the asset, then delete or rename it." },
  orphan: { label: "Orphan", tone: "problem", hint: "Its pointer was moved or deleted in the store: the next sync moves or trashes the file." },
  "invalid-pointer": { label: "Invalid pointer", tone: "problem", hint: "The pointer document cannot be read." },
  "invalid-path": { label: "Invalid name", tone: "problem", hint: "The name cannot be a file on every system: rename it." },
};

export function stateLabel(state: string): StateLabel {
  return LABELS[state as AssetState] ?? { label: state, tone: "problem", hint: state };
}

/** The types the server sends inline; keep in step with INLINE in node/apps/server/src/assets.ts. */
const PREVIEWS: Record<string, "image" | "pdf" | "audio" | "video"> = {
  "image/png": "image",
  "image/jpeg": "image",
  "image/gif": "image",
  "image/webp": "image",
  "image/avif": "image",
  "image/bmp": "image",
  "application/pdf": "pdf",
  "audio/mpeg": "audio",
  "audio/ogg": "audio",
  "audio/wav": "audio",
  "audio/flac": "audio",
  "video/mp4": "video",
  "video/webm": "video",
};

/** What a browser can show of an asset's type: an image, a PDF, audio, video, or only a download. */
export function previewKind(type: string): "image" | "pdf" | "audio" | "video" | "download" {
  return PREVIEWS[type] ?? "download";
}

/** `path` is `folder` or below it. */
export function inFolder(folder: string, path: string): boolean {
  return folder === "/" || path === folder || path.startsWith(`${folder}/`);
}

/**
 * Where `path` is after the change `e`: where it moved (itself or a folder above it), `null` when it
 * was deleted, `undefined` when the change left it where it was.
 */
export function afterChange(path: string, e: { op: string; path: string; old_path: string | null }): string | null | undefined {
  if (e.op === "move" && e.old_path && e.old_path !== "/" && inFolder(e.old_path, path)) return e.path + path.slice(e.old_path.length);
  if (e.op === "delete" && e.path !== "/" && inFolder(e.path, path)) return null;
  return undefined;
}

/** The pull or push an asset in `state` calls for, if any. */
export function actionFor(item: Pick<AssetItem, "state">): "pull" | "push" | null {
  switch (item.state) {
    case "not-pulled":
    case "outdated":
      return "pull";
    case "new":
    case "modified":
      return "push";
    default:
      return null;
  }
}
