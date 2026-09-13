/** Renaming, moving and deleting files and folders from the UI. */
import type { Stat } from "../api";
import { formatBytes } from "../import/select";
import { isWithin } from "../live/paths";

export interface PathAction {
  op: "move" | "delete";
  path: string;
  kind: "file" | "folder";
}

export function actionFor(op: PathAction["op"], entry: { path: string; kind: string }): PathAction {
  return { op, path: entry.path, kind: entry.kind === "folder" ? "folder" : "file" };
}

export type MoveCheck = { ok: true; to: string } | { ok: false; reason: string | null };

/**
 * Whether `input` is somewhere `from` can move to, normalised (`" a//b/ "` → `/a/b`). An
 * unchanged path is not an error, only nothing to do: its `reason` is null.
 */
export function checkMove(from: string, input: string): MoveCheck {
  const trimmed = input.trim();
  if (!trimmed) return { ok: false, reason: "Enter the new path." };
  const segs = trimmed.split("/").filter(Boolean);
  if (segs.length === 0) return { ok: false, reason: "Nothing can move to the root itself." };
  if (segs.some((s) => s === "." || s === "..")) return { ok: false, reason: "A path cannot contain “.” or “..” segments." };
  const to = "/" + segs.join("/");
  if (to === from) return { ok: false, reason: null };
  if (isWithin(from, to)) return { ok: false, reason: "A folder cannot move inside itself." };
  return { ok: true, to };
}

/** The part of `path` a rename most likely changes: the name, without a file's extension. */
export function nameRange(path: string, kind: PathAction["kind"]): [number, number] {
  const start = path.lastIndexOf("/") + 1;
  const dot = kind === "file" ? path.lastIndexOf(".") : -1;
  return [start, dot > start ? dot : path.length];
}

export function count(n: number, one: string, many = `${one}s`): string {
  return `${n.toLocaleString()} ${n === 1 ? one : many}`;
}

/** "1 file, 2.1 KB" or "120 files, 4 subfolders, 1.3 MB". */
export function describeStat(s: Stat): string {
  const parts = [count(s.files, "file")];
  if (s.kind === "folder" && s.folders > 0) parts.push(count(s.folders, "subfolder"));
  parts.push(formatBytes(s.nbytes));
  return parts.join(", ");
}
